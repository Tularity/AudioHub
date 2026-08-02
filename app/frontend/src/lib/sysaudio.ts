// 模式 A「送对方扬声器」的共享来源与系统音频捕获后端（plan §6 / §7.1）的纯数据部分。
//
// plan §7.1 对模式 A 扬声器方向的定义只有一句：**捕获本机系统默认音频送对方默认输出
// 播放**，效果是本机与对端同时发声。麦克风是共享端（Provider）那条路的语义，不是这里
// 的默认值——把 spk 流的 source 写成 'mic' 就等于产品说的和做的是两件事。
//
// 契约（core/audiohub-ipc/src/lib.rs）：
//   session.open { peer, kind:'spk', source:'sysaudio'|'mic', backend?:string }
//   backend 缺席 = 'auto'，daemon 按优先级取第一个可用后端（sysaudio.rs resolve_backend）。
//
// **缺口（daemon 侧，本轮不动 core）**：`sysaudio::list_backends()` 只经 CLI
// `probe sysaudio --list --json` 露出来，`DaemonInfo` 里没有对应字段。所以 UI 拿不到
// 「本机哪个后端可用、为什么不可用」。这里的处理是**承认不知道**，而不是猜：
//   - daemon 上报了 sysaudio_backends → 照它说的画（含 available / note）；
//   - 没上报 → 只按平台列出后端 id，available 记 null（未知），并在文案里明说
//     「是否可用要到真正开启时才知道，不可用会明确报错」。
// 绝不本地推断 OS 版本去伪造 available：那正是这个项目反复吃亏的「界面自说自话」。
//
// 体验红线（plan §6）：这里列出的后端全部是**旁路读取本机正在播放的内容**，用户的
// 输出设备保持原样。任何文案都不得出现「把系统输出切到某个虚拟设备」这类要求。

import type { DaemonInfo, SysAudioBackend } from '../ipc/types';
import { IS_MAC } from './fmt';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';

/** session.open 的 source 取值，与 core 的 SOURCE_* 常量一一对应。 */
export const SOURCE_SYSAUDIO = 'sysaudio';
export const SOURCE_MIC = 'mic';

/** backend 的「自动」哨兵。发出去时**不带 backend 字段**，让 daemon 自己选。 */
export const BACKEND_AUTO = 'auto';

/** plan §7.1：模式 A 的 spk 方向就是系统音频，麦克风只是可选的另一种来源。 */
export const DEFAULT_SPK_SOURCE = SOURCE_SYSAUDIO;

export function isShareSource(v: unknown): v is typeof SOURCE_SYSAUDIO | typeof SOURCE_MIC {
  return v === SOURCE_SYSAUDIO || v === SOURCE_MIC;
}

/** 落到 daemon 契约上的取值：认不出来的一律回落到默认来源。 */
export function normalizeSource(v: unknown): string {
  return isShareSource(v) ? v : DEFAULT_SPK_SOURCE;
}

// ---------------------------------------------------------------- 后端目录

interface BackendMeta {
  id: string;
  /** 这个后端属于哪个平台。跨平台的没有，所以布尔就够。 */
  mac: boolean;
  labelKey: MsgKey;
  noteKey: MsgKey;
}

// id 必须与 core/audiohub-core/src/sysaudio.rs 的 BACKEND_* 常量逐字相同：
// daemon 收到认不出的 id 会直接开会话失败（resolve_backend 报 unknown backend），
// 这是好事——但前提是我们别先把 id 写错。顺序 = core 的优先级顺序。
const BACKENDS: BackendMeta[] = [
  {
    id: 'win-proc-exclude',
    mac: false,
    labelKey: 'sysaudio.backend.winProcExclude.label',
    noteKey: 'sysaudio.backend.winProcExclude.note',
  },
  {
    id: 'win-device-loopback',
    mac: false,
    labelKey: 'sysaudio.backend.winDeviceLoopback.label',
    noteKey: 'sysaudio.backend.winDeviceLoopback.note',
  },
  {
    id: 'mac-catap',
    mac: true,
    labelKey: 'sysaudio.backend.macCatap.label',
    noteKey: 'sysaudio.backend.macCatap.note',
  },
  {
    id: 'mac-sck',
    mac: true,
    labelKey: 'sysaudio.backend.macSck.label',
    noteKey: 'sysaudio.backend.macSck.note',
  },
];

const META = new Map(BACKENDS.map((b) => [b.id, b]));

export interface BackendOption {
  id: string;
  label: string;
  note: string;
  /** true/false = daemon 说的；**null = 无从得知**（daemon 没上报这个字段）。 */
  available: boolean | null;
}

/** daemon 是否上报了后端清单。false 时一切 available 都是 null，界面必须如实说。 */
export function backendsReported(daemon: DaemonInfo | null | undefined): boolean {
  return Array.isArray(daemon && daemon.sysaudio_backends);
}

function fromDaemon(raw: SysAudioBackend): BackendOption | null {
  const id = typeof raw.id === 'string' ? raw.id.trim() : '';
  if (!id) return null;
  const meta = META.get(id);
  return {
    id,
    // daemon 的 name 是英文技术名（"macOS Core Audio process tap"）：它比我们的
    // 目录更权威，但不是给终端用户读的。目录里认得的一律用中文标签，认不出的新后端
    // 才退回 daemon 的原文——总比只显示一个 id 强。
    label: meta ? t(meta.labelKey) : (typeof raw.name === 'string' && raw.name.trim()) || id,
    // note 反过来：daemon 的 note 带着**本机实际情况**（版本号、上次被拒绝），
    // 比任何静态说明都值钱，优先用它。
    note: (typeof raw.note === 'string' && raw.note.trim())
      || (meta ? t(meta.noteKey) : ''),
    available: typeof raw.available === 'boolean' ? raw.available : null,
  };
}

/**
 * 可供选择的后端（不含「自动」，那一项由控件自己加）。
 *
 * daemon 上报了就照它说的（含不可用项，附原因）；没上报就只按平台给 id，
 * available 一律 null。
 */
export function backendOptions(daemon: DaemonInfo | null | undefined): BackendOption[] {
  if (backendsReported(daemon)) {
    const list = (daemon!.sysaudio_backends || [])
      .map(fromDaemon)
      .filter((x): x is BackendOption => !!x);
    if (list.length) return list;
  }
  // 回落路径按 UI 宿主平台过滤。这条假设成立的前提是 **daemon 一定在本机**
  // （IPC 走 127.0.0.1），今天确实如此；plan §7.5 的网页端一旦落地（浏览器可能在
  //另一台机器上），这里就必须改成读 daemon 上报的清单——也就是下面那条缺口。
  return BACKENDS
    .filter((b) => b.mac === IS_MAC)
    .map((b) => ({ id: b.id, label: t(b.labelKey), note: t(b.noteKey), available: null }));
}

/**
 * 本机是否**确定**没有任何可用后端。
 *
 * 只有 daemon 上报了清单、且每一项都明确 available=false 时才是 true。没上报时返回
 * false——「不知道」不能当成「不可用」，那会把功能在能用的机器上一并关掉。
 */
export function noBackendAvailable(daemon: DaemonInfo | null | undefined): boolean {
  if (!backendsReported(daemon)) return false;
  const list = backendOptions(daemon);
  return list.length > 0 && list.every((b) => b.available === false);
}

/** 选中的后端此刻还在不在清单里（daemon 换了平台/版本时会掉出去）。 */
export function backendKnown(daemon: DaemonInfo | null | undefined, id: string): boolean {
  return !id || id === BACKEND_AUTO || backendOptions(daemon).some((b) => b.id === id);
}

/** 落到 session.open 参数上的后端值：'auto'/空一律不带字段，交给 daemon 决定。 */
export function backendParam(id: string | null | undefined): string | null {
  const v = (id || '').trim();
  return !v || v === BACKEND_AUTO ? null : v;
}
