// 音量同步控件（spec-m4b.md §A3）：滑块 + 静音按钮，当前值来自 stats.volume。
//
// 契约（core/audiohub-ipc/src/lib.rs，唯一事实来源）：
//   session.set_volume {id, scalar, muted?} -> {}
//     —— **省略 muted 即保持对端当前静音态**，所以拖动滑块只发 scalar；
//        否则一次拖动就会把对端悄悄解除静音。只有静音按钮显式带 muted。
//   stats.volume: Option<VolumeState{scalar,muted,adjustable}>
//     —— spk 会话两侧都有，null = 该会话不同步音量（或首帧还没到）。
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
const HOLD_MS = 2000;      // 本地意图压住回声的上限；stats 1s 一帧，给足两帧
const EPS = 0.02;          // 收敛容差：设备量化后回报值与请求值有细微差异
const WAIT_MS = 5000;      // 超过它还没等到 volume，判定该会话没开音量同步
const ERR_MS = 4000;       // 失败提示的停留时长

function pctOf(scalar: number | undefined): number {
  const v = typeof scalar === 'number' && isFinite(scalar) ? scalar : 0;
  return Math.round(Math.max(0, Math.min(1, v)) * 100);
}

interface Intent { scalar: number; muted: boolean; at: number }

export function VolumeControl({
  volumeTestid, muteTestid, label, sess, onSet,
}: {
  volumeTestid: string;
  muteTestid: string;
  label?: string;
  /** 本机驱动的那条 spk 会话，无则 null。 */
  sess: SessionInfo | null;
  onSet: (id: number, params: { scalar: number; muted?: boolean }) => Promise<unknown>;
}) {
  const [, force] = useReducer((x: number) => x + 1, 0);
  const sliderLabel = label ?? t('volume.label');

  const idRef = useRef<number | null>(null);
  const intent = useRef<Intent | null>(null);
  const seenAt = useRef(0);
  const errAt = useRef(0);
  const held = useRef(false);          // 指针正按在滑块上：这期间绝不回写 value
  const lastSendAt = useRef(-Infinity);
  const flushTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const queued = useRef<{ scalar: number; muted: boolean | null } | null>(null);

  const id = sess && sess.id != null ? sess.id : null;
  const reported: VolumeState | null = (id != null && sess?.stats?.volume) || null;

  const reset = useCallback(() => {
    intent.current = null;
    queued.current = null;
    errAt.current = 0;
    held.current = false;
    if (flushTimer.current) clearTimeout(flushTimer.current);
    flushTimer.current = null;
  }, []);

  // 换了一条会话（或会话没了）：旧的意图/节流状态一律作废，否则尾发会打到一个
  // 已经不存在的 id 上。用 ref 比较而不是 useEffect —— 必须在本次渲染就生效。
  if (idRef.current !== id) {
    reset();
    idRef.current = id;
    seenAt.current = id == null ? 0 : Date.now();
  }

  // 意图的生命周期：收敛或超时即交还给真实值。
  if (intent.current) {
    const it = intent.current;
    if (reported && Math.abs(reported.scalar - it.scalar) <= EPS && reported.muted === it.muted) {
      intent.current = null;
    } else if (Date.now() - it.at > HOLD_MS) {
      intent.current = null;
    }
  }

  useEffect(() => () => {
    if (flushTimer.current) clearTimeout(flushTimer.current);
  }, []);

  const fire = useCallback((args: { scalar: number; muted: boolean | null }) => {
    lastSendAt.current = performance.now();
    queued.current = null;
    const sid = idRef.current;
    if (sid == null) return;
    const params: { scalar: number; muted?: boolean } = { scalar: args.scalar };
    if (args.muted != null) params.muted = args.muted;
    Promise.resolve(onSet(sid, params)).catch(() => {
      // 失败就立刻放弃本地意图，让下一帧真实值接管——绝不留一个骗人的滑块位置。
      if (idRef.current !== sid) return;
      intent.current = null;
      errAt.current = Date.now();
      force();
    });
  }, [onSet]);

  // 节流：首发即时、尾发必达，两次 RPC 之间至少 THROTTLE_MS。
  const schedule = useCallback((scalar: number, muted: boolean | null) => {
    const base = intent.current || reported;
    intent.current = {
      scalar,
      muted: muted == null ? !!(base && base.muted) : muted,
      at: Date.now(),
    };
    force();
    if (idRef.current == null) return;
    queued.current = { scalar, muted };
    const wait = THROTTLE_MS - (performance.now() - lastSendAt.current);
    if (wait <= 0 && flushTimer.current == null) {
      fire(queued.current);
      return;
    }
    if (flushTimer.current == null) {
      flushTimer.current = setTimeout(() => {
        flushTimer.current = null;
        if (queued.current) fire(queued.current);
      }, Math.max(0, wait));
    }
  }, [fire, reported]);

  const release = useCallback(() => {
    if (!held.current) return;
    held.current = false;
    force();
  }, []);

  useEffect(() => {
    window.addEventListener('pointerup', release);
    window.addEventListener('pointercancel', release);
    return () => {
      window.removeEventListener('pointerup', release);
      window.removeEventListener('pointercancel', release);
    };
  }, [release]);

  const cur = intent.current || reported;
  const adjustable = reported ? reported.adjustable !== false : false;
  const usable = !!reported && adjustable;
  const pct = cur ? pctOf(cur.scalar) : 0;
  const muted = !!(cur && cur.muted);

  // 滑块是**非受控**的，值由这个 effect 有条件地写回：拖动中（指针按住，或本地意图
  // 仍在压制回声）绝不覆盖，否则每秒一帧的 stats 会把滑块从手指下抢走。受控写法
  // （value={pct}）做不到「有时候不跟随」——那正是这个控件的全部难点。
  const sliderRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const node = sliderRef.current;
    if (!node) return;
    if (!held.current && !intent.current) node.value = String(pct);
    node.style.setProperty('--vol', `${pct}%`);
  });

  let note = '';
  if (errAt.current && Date.now() - errAt.current < ERR_MS) note = t('volume.failed');
  else if (reported && !adjustable) note = t('volume.unadjustable');
  else if (!reported && id != null) {
    note = Date.now() - seenAt.current < WAIT_MS ? t('volume.reading') : t('volume.noSync');
  }

  const mLabel = muted ? t('volume.unmute') : t('volume.mute');

  return (
    <div
      className={`volume-box${reported && !adjustable ? ' unadjustable' : ''}`}
      data-testid={`${volumeTestid}-box`}
      hidden={id == null}
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
          disabled={!usable}
          onClick={(e) => {
            e.stopPropagation();
            if (!cur || id == null) return;
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
