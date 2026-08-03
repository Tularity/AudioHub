// 运行模式（plan §7.1 + §13，冻结）——**全局**设置，不是每个对端各自的开关。
//
// 这个模块是整套 UI 里关于「现在是哪种模式」「模式 B 能不能用」的唯一判据来源：
// 三个视图各自判一次的话，迟早会出现主面板按 A 渲染、设置页按 B 渲染的分裂。
//
// 迁移后这条规矩比以前更硬：这里全是**纯函数选择器**，组件只能经
// `useStore(selectXxx)` 读，不许在组件内就地重算。谁想改判据，只能改这个文件。
//
// **三模式互斥（plan §13）**：共享模式能被别人用、不能用别人；模式 A / B 反过来。
// 于是「本机是共享端」这件事从一个恒真的背景条件，变成了一个和 A/B 并列的选项，
// 而界面上每一处「取对方麦克风 / 送对方扬声器」的控件都只在 A/B 下才有意义。
//
// 另两条冻结语义仍然决定各视图的形状：
//   · 模式 B 下「用哪个对端」由用户在系统声音设置里选设备决定，**UI 不提供对端
//     选择器**——这正是模式 B 存在的理由（以系统最原生的体验为核心目标）；
//   · 只有模式 A 才在 UI 里决定音频送往哪个对端。

import type { DaemonInfo, HalDeviceInfo, PeerState } from '../ipc/types';
import { t } from '../i18n';
import type { AppState } from './store';

export const MODE_SHARE = 'share';
export const MODE_A = 'a';
export const MODE_B = 'b';
export type AppMode = 'share' | 'a' | 'b';

/** daemon 给的模式字符串 → 本模块的类型。认不出就 null，**绝不猜**。 */
export function parseMode(v: unknown): AppMode | null {
  return v === MODE_SHARE || v === MODE_A || v === MODE_B ? v : null;
}

export type HalKind = 'unknown' | 'absent' | 'mismatch' | 'detached' | 'ready';

export interface HalState {
  kind: HalKind;
  available: boolean;
  tone: 'dim' | 'warn' | 'danger' | 'ok';
  text: string;
  why?: string;
}

/**
 * 模式 B 的可用性 + 该说什么。
 *
 * 判据是「这台 daemon 有没有 HAL 桥」（`hal != null` / `registered`），**不是**
 * 「此刻有没有驱动连着」。理由不是偏好而是一致性：daemon 的 `effective_mode`
 * 就是这么算的（haldev.rs `wants_mode_b() && inner.hal().is_some()`）。
 * coreaudiod 会重启，桥几秒内自己接回来、绑定全程保留（plan §7.3）；若 UI 改用
 * `driver_connected` 置灰，重启那几秒里 daemon 明明还在模式 B（设备还在系统里、
 * 会话照跑），界面却把 B 变灰——用户看到的是一个「已选中但点不动」的选项，
 * 而且此时切走会真的把设备全删一遍。所以：
 *   · 桥不存在   → 真禁用（daemon 也会把模式钳回 A，界面与它一致）
 *   · 桥在、驱动没连上 → **仍可选**，但要说清楚是哪一种没连上
 */
export function halState(daemon: DaemonInfo | null | undefined): HalState {
  const hal = daemon && daemon.hal ? daemon.hal : null;
  if (!daemon) {
    return { kind: 'unknown', available: false, tone: 'dim', text: t('hal.unknown') };
  }
  if (!hal || !hal.registered) {
    return {
      kind: 'absent',
      available: false,
      tone: 'warn',
      text: t('hal.absent'),
      why: t('hal.absent.why'),
    };
  }
  if (hal.status_reason === 'driver_protocol_mismatch') {
    // 唯一一种「重启也没用」的驱动故障：装着的驱动和这一版服务说的不是同一套协议。
    // 表现是系统里一台 AudioHub 设备都没有，而 daemon 一切正常——不点破就是无解。
    const mine = hal.protocol_version;
    const theirs = hal.driver_protocol_version;
    const versions = (mine != null && theirs != null)
      ? t('hal.mismatch.versions', { mine, theirs })
      : '';
    return {
      kind: 'mismatch',
      available: true,
      tone: 'danger',
      text: t('hal.mismatch', { versions }),
    };
  }
  if (!hal.driver_connected) {
    return {
      kind: 'detached',
      available: true,
      tone: 'warn',
      text: t('hal.detached'),
    };
  }
  return { kind: 'ready', available: true, tone: 'ok', text: t('hal.ready') };
}

/** 用户请求的模式：daemon 说了算，没拿到回包前用本地缓存兜住首帧。 */
export function requestedMode(s: AppState): AppMode {
  return parseMode(s.daemonSettings?.mode) ?? parseMode(s.settings.mode) ?? MODE_SHARE;
}

/**
 * 真正生效的模式。daemon 的 `effective_mode` 优先——它才知道自己有没有 HAL 桥。
 * 拿不到时（还没回包）自己按 halState 回落一次，绝不把存过的 'b' 当成
 * 「现在能用 B」。
 *
 * 回落方向与 daemon 的 `haldev::effective_mode` 逐条一致：**只有 B 会落空，
 * 而且落到 A**。落到共享模式会把本机换到互斥关系的另一侧——一个比缺个驱动
 * 大得多的变化，而且界面上还选中着一个使用端模式。
 */
export function effectiveMode(s: AppState): AppMode {
  const d = parseMode(s.daemonSettings?.effective_mode);
  if (d) return d;
  const want = requestedMode(s);
  return want === MODE_B && !halState(s.daemon).available ? MODE_A : want;
}

export function isModeB(s: AppState): boolean {
  return effectiveMode(s) === MODE_B;
}

/** 共享模式：本机被别人使用，卡片上那排「使用对端」的控件全部无意义。 */
export function isShareMode(s: AppState): boolean {
  return effectiveMode(s) === MODE_SHARE;
}

/** 用户选了 B、daemon 却只能给 A：必须说出来，否则界面在无声地降级。 */
export function modeDowngraded(s: AppState): boolean {
  return requestedMode(s) === MODE_B && effectiveMode(s) === MODE_A;
}

/**
 * 这台对端此刻能不能被本机使用（plan §13 推论 1）。
 *
 * 三态，**不可压成布尔**：
 *   · 'yes'     —— 它在共享模式，可用；
 *   · 'no'      —— 它明确说了自己是使用端，或报了个本版本不认识的模式；
 *   · 'unknown' —— 离线，或刚连上还没收到通告。什么都别说。
 *
 * 判据取 daemon 给的 `peer_unusable`，而不是自己从 `peer_mode` 推：`peer_mode`
 * 为空有两种成因（没上报 / 认不出），只有 daemon 分得清，见 types.ts 的说明。
 */
export type PeerUsable = 'yes' | 'no' | 'unknown';

export function peerUsable(p: PeerState | null | undefined): PeerUsable {
  if (!p || !p.online) return 'unknown';
  if (p.peer_unusable) return 'no';
  return parseMode(p.peer_mode) ? 'yes' : 'unknown';
}

/** 对端不可用时该显示的那句话。'yes' / 'unknown' 一律空串（什么都不说）。 */
export function peerUnusableText(p: PeerState | null | undefined): string {
  if (peerUsable(p) !== 'no') return '';
  const m = parseMode(p?.peer_mode);
  if (m === MODE_A) return t('peers.unusable.modeA');
  if (m === MODE_B) return t('peers.unusable.modeB');
  // 认不出的模式（协议版本相等比较之下本不该出现）：说得含糊但不说错。
  return t('peers.unusable.unknownMode');
}

/** 组件用的选择器（引用稳定的对象请勿在此处新建——halState 每次返回新对象，
 *  所以取用它的组件应当只订阅需要的字段，或接受一次浅比较）。 */
export const selectEffectiveMode = (s: AppState): AppMode => effectiveMode(s);
export const selectIsModeB = (s: AppState): boolean => isModeB(s);
export const selectIsShareMode = (s: AppState): boolean => isShareMode(s);
export const selectModeDowngraded = (s: AppState): boolean => modeDowngraded(s);
export const selectHalKind = (s: AppState): HalKind => halState(s.daemon).kind;
export const selectHalAvailable = (s: AppState): boolean => halState(s.daemon).available;

// ---------------------------------------------------------------- 设备

const DEVICE_STATE_KEY = {
  bound: 'device.state.bound',
  pending: 'device.state.pending',
  delisted: 'device.state.delisted',
  free: 'device.state.free',
} as const;

/** 驱动侧的机器可读状态 → 本地化文案。认不出的状态原样回显（诊断值，不进语料）。 */
export function deviceStateLabel(state: string | null | undefined): string {
  const k = DEVICE_STATE_KEY[state as keyof typeof DEVICE_STATE_KEY];
  return k ? t(k) : String(state ?? '');
}

/** `hal.devices` 里属于某个对端的那一条（诊断字段齐全，PeerState 上的那份没有）。 */
export function halDeviceOf(daemon: DaemonInfo | null | undefined, fp: string): HalDeviceInfo | null {
  const list = daemon && daemon.hal && Array.isArray(daemon.hal.devices) ? daemon.hal.devices : [];
  return list.find((d) => d && d.fingerprint === fp) || null;
}

export interface DeviceRow {
  dir: 'out' | 'in';
  icon: 'spk' | 'mic';
  name: string;
  uid: string;
  role: string;
  io: boolean;
  frames: number | null;
  dropped: number | null;
}

/**
 * 一台对端的两台设备，按「输出 / 输入」摊平成行，供卡片与设置页共用。
 * `peer.hal_device` 是权威（模式 A 下它就是 null），`hal.devices` 只补诊断字段。
 */
export function peerDeviceRows(
  peer: PeerState | null | undefined,
  daemon: DaemonInfo | null | undefined,
): DeviceRow[] {
  const d = peer && peer.hal_device;
  if (!d || !peer) return [];
  const info = halDeviceOf(daemon, peer.fingerprint);
  return [
    {
      dir: 'out',
      icon: 'spk',
      name: d.out_name || '',
      uid: d.out_uid || '',
      role: t('device.speaker'),
      io: !!(info && info.io_out),
      frames: info ? (info.spk_frames ?? null) : null,
      dropped: null,
    },
    {
      dir: 'in',
      icon: 'mic',
      name: d.in_name || '',
      uid: d.in_uid || '',
      role: t('device.microphone'),
      io: !!(info && info.io_in),
      frames: info ? (info.mic_frames ?? null) : null,
      dropped: info ? (info.mic_dropped ?? null) : null,
    },
  ];
}

/**
 * 某台对端「为什么没有虚拟设备」。hal_reason 是 daemon 给的机器可读原因，
 * 每一种下一步动作都不同，不能合并成一句「暂不可用」。
 */
export function halReasonText(reason: string | null | undefined): string {
  switch (reason) {
    case 'capacity': return t('halReason.capacity');
    case 'no_driver': return t('halReason.noDriver');
    case 'removed_while_offline': return t('halReason.removedWhileOffline');
    case 'mode_a': return t('halReason.modeA');
    // plan §13 推论 3：切到共享模式即无条件移除，与模式 A 不是同一件事，
    // 用户的下一步也不同。daemon 侧的 `haldev::no_device_reason` 有一条测试
    // 会读这个文件，确认每个它能发出的 reason 在这里都有分支。
    case 'mode_share': return t('halReason.modeShare');
    default: return reason ? t('halReason.other', { reason }) : t('halReason.none');
  }
}
