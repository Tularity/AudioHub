// Volume synchronization: mode A targets a session, while mode B targets a
// real peer endpoint that remains addressable while the device is idle.
//
// Wire contracts (core/audiohub-ipc/src/lib.rs is the source of truth):
//   session.set_volume {id, scalar, muted?} -> {}
//     Omitting muted preserves the peer's current mute state. Slider writes
//     therefore send scalar only; only the mute button includes muted.
//   stats.volume: Option<VolumeState{scalar,muted,adjustable}>
//     Speaker sessions report this on both sides. Null means volume sync is
//     unavailable for that session or the first stats frame has not arrived.
//   peer.set_device_volume {peer, endpoint, scalar, muted?} -> {}
//     endpoint is default_output/default_input. It never uses a fabricated
//     session id; state comes from PeerHalDevice out_volume/in_volume.
//
// 为什么不能把 stats 直接绑到滑块上：daemon 每秒回报提供端真实设备的状态，拖动时
// 回声必然滞后于手指，朴素绑定会让滑块来回跳。这里用「本地意图压住回声」：用户一动
// 就记下意图并立刻渲染，直到 (a) 回报值收敛到意图（容差 EPS），或 (b) 超过 HOLD_MS
// 仍未收敛——那说明设备没照做，此时如实显示真值而不是继续骗人。
//
// 迁移要点：意图/节流全部放在 **ref** 里而不是 React state。它们每 100ms 变一次，
// 变成 state 就是每 100ms 重渲一次对端卡片；而滑块的 value 由这些 ref 直接决定，
// 拖动中一旦被 props 重渲覆盖，滑块就会从手指下跳走。

import { useCallback, useEffect, useReducer, useRef } from 'react';
import { Icon } from './Icon';
import type { SessionInfo, VolumeState } from '../ipc/types';
import { t } from '../i18n';

const THROTTLE_MS = 100;   // §A3：拖动最多 100ms 发一次 RPC（首发 + 尾发）
export const VOLUME_INTENT_HOLD_MS = 2000;
const HOLD_MS = VOLUME_INTENT_HOLD_MS; // Local intent holds through two 1 Hz stats frames.
const EPS = 0.02;          // 收敛容差：设备量化后回报值与请求值有细微差异
const WAIT_MS = 5000;      // 超过它还没等到 volume，判定该会话没开音量同步
const ERR_MS = 4000;       // 失败提示的停留时长

function pctOf(scalar: number | undefined): number {
  const v = typeof scalar === 'number' && isFinite(scalar) ? scalar : 0;
  return Math.round(Math.max(0, Math.min(1, v)) * 100);
}

export interface VolumeIntentPin<T> { value: T; at: number }
export interface VolumeIntentPins {
  scalar: VolumeIntentPin<number> | null;
  muted: VolumeIntentPin<boolean> | null;
}

export interface QueuedVolumeWrite {
  scalar: number;
  muted: boolean | null;
  scalarOwner: VolumeIntentPin<number> | null;
  muteOwner: VolumeIntentPin<boolean> | null;
}

export interface EnqueuedVolumeIntent {
  pins: VolumeIntentPins;
  queued: QueuedVolumeWrite;
}

export interface ConsumedVolumeWrite {
  write: QueuedVolumeWrite | null;
  remaining: null;
}

export interface VolumeWriteFailureOwnershipResult {
  pins: VolumeIntentPins;
  applied: boolean;
  scalarCleared: boolean;
  muteCleared: boolean;
  clearGlobalRefresh: boolean;
}

export interface VolumeWriteFailureContext {
  currentGeneration: number;
  consumedGeneration: number;
  currentSequence: number;
  consumedSequence: number;
  targetMatches: boolean;
}

export function updateVolumeIntentPins(
  current: VolumeIntentPins,
  scalar: number,
  muted: boolean | null,
  at: number,
): VolumeIntentPins {
  if (muted == null) {
    return {
      scalar: { value: scalar, at },
      muted: current.muted,
    };
  }
  return {
    scalar: current.scalar,
    muted: { value: muted, at },
  };
}

export function reconcileVolumeIntentPins(
  current: VolumeIntentPins,
  reported: VolumeState | null | undefined,
  now: number,
): VolumeIntentPins {
  const scalar = current.scalar
    && ((reported && Math.abs(reported.scalar - current.scalar.value) <= EPS)
      || now - current.scalar.at >= HOLD_MS)
    ? null
    : current.scalar;
  const muted = current.muted
    && ((reported && reported.muted === current.muted.value)
      || now - current.muted.at >= HOLD_MS)
    ? null
    : current.muted;
  return scalar === current.scalar && muted === current.muted
    ? current
    : { scalar, muted };
}

export function volumeWithIntentPins(
  reported: VolumeState | null | undefined,
  pins: VolumeIntentPins,
): VolumeState | null {
  const scalar = pins.scalar?.value ?? reported?.scalar;
  if (scalar == null) return null;
  return {
    scalar,
    muted: pins.muted?.value ?? reported?.muted ?? false,
    adjustable: reported?.adjustable ?? false,
    mute_adjustable: reported?.mute_adjustable,
  };
}

/** Merge field values and owners only within the batch which has not reached fire(). */
export function coalesceQueuedVolumeWrite(
  current: QueuedVolumeWrite | null,
  update: QueuedVolumeWrite,
): QueuedVolumeWrite {
  return {
    scalar: update.scalar,
    muted: update.muted ?? current?.muted ?? null,
    scalarOwner: update.scalarOwner ?? current?.scalarOwner ?? null,
    muteOwner: update.muteOwner ?? current?.muteOwner ?? null,
  };
}

/** Apply one user action to the exact pin and unsent-batch ownership state. */
export function enqueueVolumeIntent(
  currentPins: VolumeIntentPins,
  currentQueue: QueuedVolumeWrite | null,
  scalar: number,
  muted: boolean | null,
  at: number,
): EnqueuedVolumeIntent {
  const pins = updateVolumeIntentPins(currentPins, scalar, muted, at);
  return {
    pins,
    queued: coalesceQueuedVolumeWrite(currentQueue, {
      scalar,
      muted,
      scalarOwner: muted == null ? pins.scalar : null,
      muteOwner: muted == null ? null : pins.muted,
    }),
  };
}

/** Atomically remove the batch from the queue before its RPC starts. */
export function consumeQueuedVolumeWrite(
  current: QueuedVolumeWrite | null,
): ConsumedVolumeWrite {
  return { write: current, remaining: null };
}

/** Resolve a failed RPC only against fields that the consumed batch owned. */
export function applyVolumeWriteFailureOwnership(
  current: VolumeIntentPins,
  consumed: QueuedVolumeWrite,
  context: VolumeWriteFailureContext,
): VolumeWriteFailureOwnershipResult {
  if (!context.targetMatches
    || context.currentGeneration !== context.consumedGeneration) {
    return {
      pins: current,
      applied: false,
      scalarCleared: false,
      muteCleared: false,
      clearGlobalRefresh: false,
    };
  }
  const scalarCleared = consumed.scalarOwner != null
    && current.scalar === consumed.scalarOwner;
  const muteCleared = consumed.muteOwner != null
    && current.muted === consumed.muteOwner;
  return {
    pins: scalarCleared || muteCleared
      ? {
        scalar: scalarCleared ? null : current.scalar,
        muted: muteCleared ? null : current.muted,
      }
      : current,
    applied: true,
    scalarCleared,
    muteCleared,
    // Sequence is deliberately global only for refresh bookkeeping. An older
    // cross-field failure still owns its unchanged pin, but it must not cancel
    // refreshes which belong to a later batch.
    clearGlobalRefresh: context.currentSequence === context.consumedSequence,
  };
}

/**
 * Arm one field's device-only authority hand-back deadline.
 *
 * Session controls receive a stats render every second. Device controls do
 * not, so merely checking `pin.at` during render can leave a refused or
 * quantized write visible until the next 10-second peer poll. Keeping this
 * timer helper explicit also makes the no-late-local-intent contract testable
 * without requiring a browser DOM in the unit-test project.
 */
export function scheduleDeviceIntentExpiry({
  isCurrent,
  clearPin,
  rerender,
  refresh,
  delayMs = VOLUME_INTENT_HOLD_MS,
}: {
  isCurrent: () => boolean;
  clearPin: () => void;
  rerender: () => void;
  refresh: () => void;
  delayMs?: number;
}): ReturnType<typeof setTimeout> {
  return setTimeout(() => {
    if (!isCurrent()) return;
    clearPin();
    rerender();
    refresh();
  }, Math.max(0, delayMs));
}

export interface VolumeWriteParams { scalar: number; muted?: boolean }
export type DeviceVolumeEndpoint = 'default_output' | 'default_input';
export type DeviceVolumeStatus = 'legacy' | 'waiting' | 'pending' | 'fixed' | 'ready';
export interface DeviceVolumeWriteRequest extends VolumeWriteParams {
  peer: string;
  endpoint: DeviceVolumeEndpoint;
}

export function deviceVolumeEndpointFor(dir: 'out' | 'in'): DeviceVolumeEndpoint {
  return dir === 'out' ? 'default_output' : 'default_input';
}

/** Build the device RPC payload without introducing a synthetic session id. */
export function deviceVolumeRequest(
  peer: string,
  endpoint: DeviceVolumeEndpoint,
  params: VolumeWriteParams,
): DeviceVolumeWriteRequest {
  const request: DeviceVolumeWriteRequest = { peer, endpoint, scalar: params.scalar };
  if (params.muted !== undefined) request.muted = params.muted;
  return request;
}

/** Pure presentation state for an idle-capable mode-B endpoint control. */
export function deviceVolumeStatus(
  version: number | null | undefined,
  reported: VolumeState | null | undefined,
  pending: boolean | null | undefined,
  online = false,
): DeviceVolumeStatus {
  // A queued write is stronger evidence than a now-disconnected capability
  // cell. Do not relabel a genuine retained intent as "legacy" merely because
  // connection-scoped capabilities disappear while the peer is offline.
  if (pending && !online) return 'pending';
  if (!Number.isFinite(version) || Number(version) < 1) return 'legacy';
  if (pending) return 'pending';
  if (!reported) return 'waiting';
  if (reported.adjustable === false) return 'fixed';
  return 'ready';
}

export function volumeUsability(
  reported: VolumeState | null | undefined,
  softwareGain: boolean,
  available: boolean,
): { scalar: boolean; mute: boolean } {
  if (!available || !reported) return { scalar: false, mute: false };
  const scalarAdjustable = reported.adjustable !== false;
  // Old IPC omitted the independent bit and historically tied mute to scalar.
  // An explicit false from a current daemon must win over that compatibility.
  const muteAdjustable = reported.mute_adjustable ?? scalarAdjustable;
  return {
    scalar: scalarAdjustable || softwareGain,
    mute: muteAdjustable || softwareGain,
  };
}

type VolumeTarget = number | string;

interface VolumeSurfaceProps {
  volumeTestid: string;
  muteTestid: string;
  label?: string;
  target: VolumeTarget | null;
  reported: VolumeState | null;
  softwareGain?: boolean;
  available?: boolean;
  pending?: boolean;
  compact?: boolean;
  unavailableNote?: string;
  waitingNote?: string;
  pendingNote?: string;
  onSet: (target: VolumeTarget, params: VolumeWriteParams) => Promise<unknown>;
  /** Mode-B peer state has no stats push; refresh twice after the debounced tail. */
  onRefresh?: () => void;
}

function VolumeSurface({
  volumeTestid,
  muteTestid,
  label,
  target,
  reported,
  softwareGain = false,
  available = true,
  pending = false,
  compact = false,
  unavailableNote,
  waitingNote,
  pendingNote,
  onSet,
  onRefresh,
}: VolumeSurfaceProps) {
  const [, force] = useReducer((x: number) => x + 1, 0);
  const sliderLabel = label ?? t('volume.label');

  const targetRef = useRef<VolumeTarget | null>(null);
  const generation = useRef(0);
  const scalarPin = useRef<VolumeIntentPin<number> | null>(null);
  const mutePin = useRef<VolumeIntentPin<boolean> | null>(null);
  const seenAt = useRef(0);
  const errAt = useRef(0);
  const held = useRef(false);          // 指针正按在滑块上：这期间绝不回写 value
  const lastSendAt = useRef(-Infinity);
  const flushTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const queued = useRef<QueuedVolumeWrite | null>(null);
  const scalarExpiryTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const muteExpiryTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refreshSoonTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refreshConvergeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refreshWanted = useRef(false);
  const latestSent = useRef(0);
  const latestSucceeded = useRef(0);
  const onSetRef = useRef(onSet);
  const onRefreshRef = useRef(onRefresh);
  onSetRef.current = onSet;
  onRefreshRef.current = onRefresh;

  const clearRefreshTimers = useCallback(() => {
    if (refreshSoonTimer.current) clearTimeout(refreshSoonTimer.current);
    if (refreshConvergeTimer.current) clearTimeout(refreshConvergeTimer.current);
    refreshSoonTimer.current = null;
    refreshConvergeTimer.current = null;
  }, []);

  const clearIntentExpiryTimers = useCallback(() => {
    if (scalarExpiryTimer.current) clearTimeout(scalarExpiryTimer.current);
    if (muteExpiryTimer.current) clearTimeout(muteExpiryTimer.current);
    scalarExpiryTimer.current = null;
    muteExpiryTimer.current = null;
  }, []);

  const reset = useCallback(() => {
    generation.current += 1;
    scalarPin.current = null;
    mutePin.current = null;
    queued.current = null;
    errAt.current = 0;
    held.current = false;
    refreshWanted.current = false;
    latestSent.current = 0;
    latestSucceeded.current = 0;
    if (flushTimer.current) clearTimeout(flushTimer.current);
    flushTimer.current = null;
    clearIntentExpiryTimers();
    clearRefreshTimers();
  }, [clearIntentExpiryTimers, clearRefreshTimers]);

  // 换了一条会话（或会话没了）：旧的意图/节流状态一律作废，否则尾发会打到一个
  // 已经不存在的 id 上。用 ref 比较而不是 useEffect —— 必须在本次渲染就生效。
  if (targetRef.current !== target) {
    reset();
    targetRef.current = target;
    seenAt.current = target == null ? 0 : Date.now();
  }

  // Scalar and mute return authority independently. A scalar drag must not
  // hide a newer remote mute, nor may it extend an older explicit mute intent.
  const reconciledPins = reconcileVolumeIntentPins(
    { scalar: scalarPin.current, muted: mutePin.current },
    reported,
    Date.now(),
  );
  scalarPin.current = reconciledPins.scalar;
  mutePin.current = reconciledPins.muted;

  useEffect(() => () => {
    if (flushTimer.current) clearTimeout(flushTimer.current);
    clearIntentExpiryTimers();
    clearRefreshTimers();
    generation.current += 1;
  }, [clearIntentExpiryTimers, clearRefreshTimers]);

  const armScalarExpiry = useCallback((pin: VolumeIntentPin<number>) => {
    if (!onRefreshRef.current) return;
    if (scalarExpiryTimer.current) clearTimeout(scalarExpiryTimer.current);
    const currentGeneration = generation.current;
    scalarExpiryTimer.current = scheduleDeviceIntentExpiry({
      isCurrent: () => generation.current === currentGeneration && scalarPin.current === pin,
      clearPin: () => {
        scalarPin.current = null;
        scalarExpiryTimer.current = null;
      },
      rerender: force,
      refresh: () => { onRefreshRef.current?.(); },
      delayMs: pin.at + HOLD_MS - Date.now(),
    });
  }, []);

  const armMuteExpiry = useCallback((pin: VolumeIntentPin<boolean>) => {
    if (!onRefreshRef.current) return;
    if (muteExpiryTimer.current) clearTimeout(muteExpiryTimer.current);
    const currentGeneration = generation.current;
    muteExpiryTimer.current = scheduleDeviceIntentExpiry({
      isCurrent: () => generation.current === currentGeneration && mutePin.current === pin,
      clearPin: () => {
        mutePin.current = null;
        muteExpiryTimer.current = null;
      },
      rerender: force,
      refresh: () => { onRefreshRef.current?.(); },
      delayMs: pin.at + HOLD_MS - Date.now(),
    });
  }, []);

  // Peer/device state is polled rather than pushed with session stats. Refresh
  // once for the normal acknowledgement and once for eventual device readback.
  // A later successful write resets both timers, so a drag refreshes only after
  // its 100 ms-throttled tail succeeds.
  const queueRefresh = useCallback(() => {
    if (!onRefreshRef.current || held.current || queued.current || flushTimer.current) return;
    if (!refreshWanted.current || latestSucceeded.current !== latestSent.current) return;
    refreshWanted.current = false;
    clearRefreshTimers();
    refreshSoonTimer.current = setTimeout(() => {
      refreshSoonTimer.current = null;
      onRefreshRef.current?.();
    }, 250);
    refreshConvergeTimer.current = setTimeout(() => {
      refreshConvergeTimer.current = null;
      onRefreshRef.current?.();
    }, 1250);
  }, [clearRefreshTimers]);

  const fire = useCallback(() => {
    const consumed = consumeQueuedVolumeWrite(queued.current);
    queued.current = consumed.remaining;
    const args = consumed.write;
    if (!args) return;
    lastSendAt.current = performance.now();
    const currentTarget = targetRef.current;
    if (currentTarget == null) return;
    const currentGeneration = generation.current;
    const sequence = latestSent.current + 1;
    latestSent.current = sequence;
    const params: VolumeWriteParams = { scalar: args.scalar };
    if (args.muted != null) params.muted = args.muted;
    Promise.resolve()
      .then(() => {
        if (generation.current !== currentGeneration || targetRef.current !== currentTarget) return;
        return onSetRef.current(currentTarget, params);
      })
      .then(() => {
        if (generation.current !== currentGeneration || targetRef.current !== currentTarget) return;
        latestSucceeded.current = Math.max(latestSucceeded.current, sequence);
        refreshWanted.current = true;
        if (sequence === latestSent.current) queueRefresh();
      })
      .catch(() => {
        const failure = applyVolumeWriteFailureOwnership(
          { scalar: scalarPin.current, muted: mutePin.current },
          args,
          {
            currentGeneration: generation.current,
            consumedGeneration: currentGeneration,
            currentSequence: latestSent.current,
            consumedSequence: sequence,
            targetMatches: targetRef.current === currentTarget,
          },
        );
        // Target/generation staleness rejects the whole completion. Sequence
        // does not: per-field owner identity safely decides what may be cleared.
        if (!failure.applied) return;
        scalarPin.current = failure.pins.scalar;
        mutePin.current = failure.pins.muted;
        if (failure.scalarCleared) {
          if (scalarExpiryTimer.current) clearTimeout(scalarExpiryTimer.current);
          scalarExpiryTimer.current = null;
        }
        if (failure.muteCleared) {
          if (muteExpiryTimer.current) clearTimeout(muteExpiryTimer.current);
          muteExpiryTimer.current = null;
        }
        if (failure.clearGlobalRefresh) {
          refreshWanted.current = false;
          clearRefreshTimers();
        }
        // A failed physical payload may carry a field it did not semantically
        // own (mute writes must also carry scalar). Report an error only when
        // this consumed batch actually handed one of its current pins back.
        if (!failure.scalarCleared && !failure.muteCleared) return;
        errAt.current = Date.now();
        force();
      });
  }, [clearRefreshTimers, queueRefresh]);

  // 节流：首发即时、尾发必达，两次 RPC 之间至少 THROTTLE_MS。
  const schedule = useCallback((scalar: number, muted: boolean | null) => {
    const next = enqueueVolumeIntent(
      { scalar: scalarPin.current, muted: mutePin.current },
      queued.current,
      scalar,
      muted,
      Date.now(),
    );
    scalarPin.current = next.pins.scalar;
    mutePin.current = next.pins.muted;
    force();
    if (targetRef.current == null) return;
    // Only device-backed controls supply onRefresh. Session-backed controls
    // keep their established 1 Hz stats-driven hand-back behavior unchanged.
    if (muted == null && next.pins.scalar) armScalarExpiry(next.pins.scalar);
    if (muted != null && next.pins.muted) armMuteExpiry(next.pins.muted);
    refreshWanted.current = false;
    clearRefreshTimers();
    // A scalar tail may replace the scalar of an unsent mute click, but it must
    // not erase that explicit mute before fire() has put it on the RPC path.
    queued.current = next.queued;
    const wait = THROTTLE_MS - (performance.now() - lastSendAt.current);
    if (wait <= 0 && flushTimer.current == null) {
      fire();
      return;
    }
    if (flushTimer.current == null) {
      flushTimer.current = setTimeout(() => {
        flushTimer.current = null;
        if (queued.current) fire();
      }, Math.max(0, wait));
    }
  }, [armMuteExpiry, armScalarExpiry, clearRefreshTimers, fire]);

  const release = useCallback(() => {
    if (!held.current) return;
    held.current = false;
    force();
    queueRefresh();
  }, [queueRefresh]);

  useEffect(() => {
    window.addEventListener('pointerup', release);
    window.addEventListener('pointercancel', release);
    return () => {
      window.removeEventListener('pointerup', release);
      window.removeEventListener('pointercancel', release);
    };
  }, [release]);

  const cur = volumeWithIntentPins(reported, {
    scalar: scalarPin.current,
    muted: mutePin.current,
  });
  const adjustable = reported ? reported.adjustable !== false : false;
  // plan §7.2：对端设备没有可写音量时，daemon 把音量接管到本机发送侧的软件增益上。
  // 此时 `adjustable` 仍然是 false（那是关于**对端设备**的事实，没有变），但滑块
  // 是真的 —— 它动的是本机的增益。只看 `adjustable` 就会把一个管用的控件置灰。
  const { scalar: usable, mute: muteUsable } = volumeUsability(
    reported,
    softwareGain,
    available,
  );
  const pct = cur ? pctOf(cur.scalar) : 0;
  const muted = !!(cur && cur.muted);
  const pendingVisible = !!pendingNote
    && (pending || !!scalarPin.current || !!mutePin.current);

  // 滑块是**非受控**的，值由这个 effect 有条件地写回：拖动中（指针按住，或本地意图
  // 仍在压制回声）绝不覆盖，否则每秒一帧的 stats 会把滑块从手指下抢走。受控写法
  // （value={pct}）做不到「有时候不跟随」——那正是这个控件的全部难点。
  const sliderRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const node = sliderRef.current;
    if (!node) return;
    if (!held.current && !scalarPin.current) node.value = String(pct);
    node.style.setProperty('--vol', `${pct}%`);
  });

  let note = '';
  if (errAt.current && Date.now() - errAt.current < ERR_MS) note = t('volume.failed');
  else if (pendingVisible && pendingNote) note = pendingNote;
  else if (!available) note = unavailableNote || t('volume.noSync');
  // 兜底生效时也要说一句，但说的是**另一件事**：不是「调不了」，而是「对端调不了，
  // 所以由本机接管」——用户据此才明白线上此刻带着音量。
  else if (reported && !adjustable) {
    note = t(softwareGain
      ? 'volume.softwareGain'
      : reported.mute_adjustable === true
        ? 'volume.scalarFixedMuteAvailable'
        : 'volume.unadjustable');
  }
  else if (!reported && target != null) {
    note = waitingNote || (Date.now() - seenAt.current < WAIT_MS ? t('volume.reading') : t('volume.noSync'));
  }

  const mLabel = muted ? t('volume.unmute') : t('volume.mute');

  return (
    <div
      className={`volume-box${compact ? ' device-volume' : ''}${!available ? ' unavailable' : ''}${reported && !usable ? ' unadjustable' : ''}${pendingVisible ? ' pending' : ''}`}
      data-testid={`${volumeTestid}-box`}
      hidden={target == null}
      // 对端卡片整体可点击（进入详情）：控件里的点击绝不能冒泡上去。
      onClick={(e) => e.stopPropagation()}
    >
      <div className="volume-row">
        <button
          type="button"
          className={`icon-btn vol-mute${muted ? ' on' : ''}`}
          data-testid={muteTestid}
          aria-pressed={muted}
          aria-label={mLabel}
          title={mLabel}
          disabled={!muteUsable}
          onClick={(e) => {
            e.stopPropagation();
            if (!cur || target == null) return;
            schedule(cur.scalar, !cur.muted);
          }}
        >
          <Icon name={muted ? 'mute' : 'spk'} />
        </button>
        <input
          ref={sliderRef}
          type="range"
          min={0}
          max={100}
          step={1}
          defaultValue={0}
          className={`vol-slider${muted ? ' muted' : ''}`}
          data-testid={volumeTestid}
          aria-label={sliderLabel}
          disabled={!usable}
          onPointerDown={() => { held.current = true; }}
          onBlur={release}
          // muted 传 null = 不下发 muted 字段：拖动音量不该改变对端静音态。
          onChange={(e) => schedule(e.currentTarget.valueAsNumber / 100, null)}
        />
        <span className="vol-val" data-testid={`${volumeTestid}-value`}>
          {cur ? (muted ? t('volume.muted') : t('volume.pct', { n: pct })) : t('common.dash')}
        </span>
      </div>
      <p className="vol-note" data-testid={`${volumeTestid}-note`} hidden={!note}>{note}</p>
    </div>
  );
}

/** Session-backed mode-A wrapper. Its public behavior and RPC contract stay unchanged. */
export function VolumeControl({
  volumeTestid, muteTestid, label, sess, onSet,
}: {
  volumeTestid: string;
  muteTestid: string;
  label?: string;
  /** The locally initiated speaker session, or null when no such session exists. */
  sess: SessionInfo | null;
  onSet: (id: number, params: VolumeWriteParams) => Promise<unknown>;
}) {
  const id = sess && sess.id != null ? sess.id : null;
  const reported: VolumeState | null = (id != null && sess?.stats?.volume) || null;
  return (
    <VolumeSurface
      volumeTestid={volumeTestid}
      muteTestid={muteTestid}
      label={label}
      target={id}
      reported={reported}
      softwareGain={!!sess?.stats?.volume_software_gain}
      onSet={(target, params) => onSet(Number(target), params)}
    />
  );
}

/** Idle-capable mode-B control for one concrete peer endpoint. */
export function DeviceVolumeControl({
  peer,
  endpoint,
  volumeTestid,
  muteTestid,
  label,
  reported,
  pending,
  version,
  online,
  active,
  softwareGain,
  inactiveNote,
  onSet,
  onRefresh,
}: {
  peer: string;
  endpoint: DeviceVolumeEndpoint;
  volumeTestid: string;
  muteTestid: string;
  label?: string;
  reported: VolumeState | null | undefined;
  pending: boolean | null | undefined;
  version: number | null | undefined;
  online: boolean;
  active: boolean;
  softwareGain?: boolean;
  inactiveNote?: string;
  onSet: (endpoint: DeviceVolumeEndpoint, params: VolumeWriteParams) => Promise<unknown>;
  onRefresh: () => void;
}) {
  // A retained pending action may explain an offline row, but it cannot grant
  // protocol authority to a live cap0/Unheard connection.
  const status = deviceVolumeStatus(version, reported, pending, online);
  return (
    <VolumeSurface
      volumeTestid={volumeTestid}
      muteTestid={muteTestid}
      label={label}
      target={`${peer}\u0000${endpoint}`}
      reported={reported || null}
      softwareGain={!!softwareGain}
      available={active && status !== 'legacy'}
      pending={status === 'pending'}
      compact
      unavailableNote={active
        ? t('volume.deviceProtocolUnavailable')
        : inactiveNote || t('volume.deviceModeUnavailable')}
      waitingNote={t('volume.deviceWaiting')}
      pendingNote={t(active ? 'volume.devicePending' : 'volume.deviceQueued')}
      onSet={(_target, params) => onSet(endpoint, params)}
      onRefresh={onRefresh}
    />
  );
}

/** 只读音量回显（详情/统计页用）：不发 RPC，只把 VolumeState 翻成中文。 */
export function volumeText(v: VolumeState | null | undefined) {
  if (!v) return null;
  const n = pctOf(v.scalar);
  return {
    pct: t('volume.pct', { n }),
    muted: !!v.muted,
    adjustable: v.adjustable !== false,
    text: v.muted ? t('volume.mutedPct', { n }) : t('volume.pct', { n }),
    scalarPct: n,
  };
}
