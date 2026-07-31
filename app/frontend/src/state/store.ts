// 全局状态：Zustand。
//
// 迁移前这里是一个手写的 pub/sub（`store.subscribe(fn)` + 每个视图自己 diff DOM）。
// 换掉它的理由不是「框架更时髦」，而是那套写法已经稳定地生产出一类 bug：
// **派生值在多处各算一遍**。设置页存了一个偏好、主面板按另一处判据渲染，界面显示
// 的东西和 daemon 实际执行的东西可以长期不一致而没人发现（真实案例：一个存了却
// 什么也没驱动的设置开关）。
//
// 换成 Zustand 后的规矩，只有两条，但必须守住：
//   1. **原始状态只存在这一个 store 里**（daemon 回包 + 少量本地偏好）。
//   2. **任何派生值都写成纯函数选择器**（见 state/mode.ts），组件通过
//      `useStore(选择器)` 读，绝不在组件里就地再算一遍。
//   于是「渲染用的判据」和「写回 daemon 用的判据」在物理上是同一行代码。
//
// React 的协调取代了逐视图手写 diff：不再有 `if (key !== gridKey) 重建整块` 这种
// 缓存键，也不再有「忘了同步某个字段」的可能。

import { create } from 'zustand';
import { useStoreWithEqualityFn } from 'zustand/traditional';
import type {
  DaemonInfo, DaemonSettings, DiscoverResult, PeerState, SessionInfo,
} from '../ipc/types';
import type { EndpointSource } from '../ipc/endpoint';
import type { PermissionState } from './permissions';

const SETTINGS_KEY = 'audiohub.ui.settings';

export type ConnState = 'connecting' | 'starting' | 'online' | 'offline';
export type RunMode = 'tauri' | 'browser';
export type ViewName = 'peers' | 'detail' | 'pair' | 'settings' | 'stats';

export interface ConnError {
  kind: string;
  message: string;
  detail?: string | null;
  actual?: number | string;
}

export interface LocalSettings {
  latency: string;
  quality: string;
  removeVirtual: boolean;
  consumerMode: 'a' | 'b';
}

export interface MetricHistory {
  loss: number[];
  jitter: number[];
  bitrate: number[];
  rung: number[];
}

export interface AddrSeen { addr: string; seenAt: number }

export interface PairingState { pin: string; ttlS: number; expiresAt: number }

export interface PermissionsSlice {
  probed: boolean;
  supported: boolean | null;
  list: PermissionState[];
  error: string | null;
  dismissed: boolean;
  skipped: boolean;
  busy: string | null;
}

export interface AppState {
  conn: ConnState;
  mode: RunMode;
  endpoint: { port: number; token: string } | null;
  endpointSource: EndpointSource | null;
  connError: ConnError | null;
  daemon: DaemonInfo | null;
  lastStatusAt: number;
  ipcRttMs: number | null;
  peers: PeerState[];
  sessions: SessionInfo[];
  history: Record<string, MetricHistory>;
  addrHistory: Record<string, AddrSeen[]>;
  pairing: PairingState | null;
  discover: { running: boolean; results: DiscoverResult[] };
  monitorPref: Record<string, boolean>;
  bridgePref: Record<string, string>;
  permissions: PermissionsSlice;
  settings: LocalSettings;
  daemonSettings: DaemonSettings | null;
  settingsSupported: boolean | null;
  route: { view: ViewName; peerFp: string | null };
}

function loadSettings(): LocalSettings {
  // **这份只是缓存**（spec-ui §2）：设置的真身在 daemon 里（settings.get/set，
  // 落盘 <config>/settings.json）。留着它只为两件事：首帧还没拿到回包时不闪，
  // 以及 daemon 版本较旧、没有 settings.* 时仍有个本地值可显示。
  // 任何时候 daemon 的值都覆盖它——见 state/mode.ts 的 requestedMode/effectiveMode。
  const dft: LocalSettings = { latency: 'min', quality: 'auto', removeVirtual: false, consumerMode: 'a' };
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (raw) return { ...dft, ...(JSON.parse(raw) as Partial<LocalSettings>) };
  } catch { /* ignore */ }
  return dft;
}

const initial: AppState = {
  conn: 'connecting',
  mode: 'browser',
  endpoint: null,
  endpointSource: null,
  connError: null,
  daemon: null,
  lastStatusAt: 0,
  ipcRttMs: null,
  peers: [],
  sessions: [],
  history: {},
  addrHistory: {},
  pairing: null,
  discover: { running: false, results: [] },
  monitorPref: {},
  bridgePref: {},
  // 系统权限。**全部只活在内存里**：授权门要在每次启动时重新探测，落盘任何
  // 「已看过」标记都会在用户撤销授权后把门永久藏起来。
  permissions: {
    probed: false, supported: null, list: [], error: null,
    dismissed: false, skipped: false, busy: null,
  },
  settings: loadSettings(),
  daemonSettings: null,
  settingsSupported: null,
  route: { view: 'peers', peerFp: null },
};

export const useStore = create<AppState>()(() => initial);

/** 命令式代码（连接层、事件回调）读当前状态用；组件一律用 useStore(选择器)。 */
export const getState = useStore.getState;
export const setState = useStore.setState;

// ---------------------------------------------------------------- 动作

function push60(arr: number[], v: number): number[] {
  const out = arr.length >= 60 ? arr.slice(arr.length - 59) : arr.slice();
  out.push(v);
  return out;
}

function num(v: unknown): number {
  return typeof v === 'number' && isFinite(v) ? v : 0;
}

function persist(settings: LocalSettings): void {
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch { /* ignore */ }
}

export const actions = {
  setPeers(list: PeerState[] | unknown): void {
    const authoritative = Array.isArray(list);
    const peers = (authoritative ? list : []) as PeerState[];
    setState((s) => {
      // 按指纹存的旁路状态跟着对端一起消失。不清就是无限增长，而且同一指纹重新
      // 配对时，上一次的桥接目标/监听开关会悄悄套回去。只在拿到权威列表（真的是
      // 一个数组）时修剪：peers.list 回来一坨垃圾不该顺手把用户偏好抹掉。
      let { monitorPref, bridgePref, addrHistory } = s;
      if (authoritative) {
        const alive = new Set(peers.map((p) => p && p.fingerprint).filter(Boolean));
        const prune = <T,>(map: Record<string, T>): Record<string, T> => {
          const keys = Object.keys(map);
          if (keys.every((k) => alive.has(k))) return map;
          const out: Record<string, T> = {};
          for (const k of keys) if (alive.has(k)) out[k] = map[k];
          return out;
        };
        monitorPref = prune(monitorPref);
        bridgePref = prune(bridgePref);
        addrHistory = prune(addrHistory);
      }
      const nextAddr: Record<string, AddrSeen[]> = { ...addrHistory };
      for (const p of peers) {
        if (!p || !p.fingerprint || !p.last_addr) continue;
        const arr = nextAddr[p.fingerprint] ? nextAddr[p.fingerprint].slice() : [];
        const i = arr.findIndex((e) => e.addr === p.last_addr);
        if (i >= 0) arr[i] = { ...arr[i], seenAt: Date.now() };
        else arr.push({ addr: p.last_addr, seenAt: Date.now() });
        nextAddr[p.fingerprint] = arr;
      }
      return { peers, monitorPref, bridgePref, addrHistory: nextAddr };
    });
  },

  upsertSession(info: SessionInfo | null | undefined): void {
    if (!info || info.id == null) return;
    setState((s) => {
      const i = s.sessions.findIndex((x) => x.id === info.id);
      const sessions = s.sessions.slice();
      if (i >= 0) sessions[i] = info;
      else sessions.push(info);
      return { sessions };
    });
  },

  removeSession(id: number): void {
    setState((s) => {
      const history = { ...s.history };
      delete history[String(id)];
      return { sessions: s.sessions.filter((x) => x.id !== id), history };
    });
  },

  /** "stats" 事件（Vec<SessionInfo>，1s 一帧）→ 会话全量替换 + 各指标推 1 点。 */
  pushStats(list: SessionInfo[] | unknown): void {
    setState((s) => {
      const sessions = (Array.isArray(list) ? list : []) as SessionInfo[];
      const history: Record<string, MetricHistory> = {};
      for (const info of sessions) {
        const key = String(info.id);
        const prev = s.history[key] || { loss: [], jitter: [], bitrate: [], rung: [] };
        const st = info.stats || {};
        history[key] = {
          loss: push60(prev.loss, num(st.loss_pct)),
          jitter: push60(prev.jitter, num(st.jitter_ms)),
          bitrate: push60(prev.bitrate, num(st.bitrate_kbps)),
          rung: push60(prev.rung, num(st.rung)),
        };
      }
      return { sessions, history };
    });
  },

  setPermissions(list: PermissionState[]): void {
    setState((s) => ({
      permissions: {
        ...s.permissions,
        list: Array.isArray(list) ? list : [],
        probed: true,
        supported: true,
        error: null,
      },
    }));
  },

  // 查不到 ≠ 没授权。探测失败只记下来，列表保持原样（可能是上一轮拿到的权威结果），
  // 由 gateNeeded 决定是否还要挡人——绝不能因为一次 RPC 超时就把用户关在门外。
  setPermissionsError(message: string | null, supported: boolean | null): void {
    setState((s) => ({
      permissions: {
        ...s.permissions,
        error: message ? String(message) : null,
        supported: supported === false ? false : s.permissions.supported,
        list: supported === false ? [] : s.permissions.list,
        probed: true,
      },
    }));
  },

  setPermissionBusy(id: string | null): void {
    setState((s) => ({ permissions: { ...s.permissions, busy: id || null } }));
  },

  dismissGate(skipped: boolean): void {
    setState((s) => ({ permissions: { ...s.permissions, dismissed: true, skipped: !!skipped } }));
  },

  // settings.get / settings.set 的回包。本地缓存跟着写一份，纯粹为了下次启动的首帧
  // ——绝不反过来覆盖 daemon。
  setDaemonSettings(d: DaemonSettings | null | undefined): void {
    if (!d || typeof d !== 'object') return;
    let saved: LocalSettings | null = null;
    setState((s) => {
      const settings = { ...s.settings };
      if (d.consumer_mode === 'a' || d.consumer_mode === 'b') settings.consumerMode = d.consumer_mode;
      if (typeof d.remove_virtual_on_disconnect === 'boolean') settings.removeVirtual = d.remove_virtual_on_disconnect;
      if (typeof d.latency === 'string') settings.latency = d.latency;
      if (typeof d.quality === 'string') settings.quality = d.quality;
      saved = settings;
      return { daemonSettings: d, settingsSupported: true, settings };
    });
    if (saved) persist(saved);
  },

  // 「查不到」≠「是 A」：只记下这版服务没有 settings.*，模式判定回落到本地缓存 +
  // 驱动状态（state/mode.ts），而不是把界面钉死在某个模式上。
  setSettingsUnsupported(): void {
    setState({ settingsSupported: false, daemonSettings: null });
  },

  navigate(view: ViewName, peerFp: string | null = null): void {
    setState({ route: { view, peerFp } });
  },

  setMonitorPref(fp: string, want: boolean): void {
    setState((s) => ({ monitorPref: { ...s.monitorPref, [fp]: want } }));
  },

  setBridgePref(fp: string, value: string): void {
    setState((s) => ({ bridgePref: { ...s.bridgePref, [fp]: value } }));
  },

  setPairing(p: PairingState | null): void {
    setState({ pairing: p });
  },

  setDiscoverRunning(running: boolean): void {
    setState((s) => ({ discover: { ...s.discover, running } }));
  },

  mergeDiscover(list: DiscoverResult[] | unknown): void {
    if (!Array.isArray(list)) return;
    setState((s) => {
      const key = (d: DiscoverResult) => d.fingerprint || `${d.instance || 'unknown'}-${d.port}`;
      const results = s.discover.results.slice();
      for (const d of list as DiscoverResult[]) {
        const k = key(d);
        const i = results.findIndex((x) => key(x) === k);
        const entry = { ...d, lastSeen: Date.now() };
        if (i >= 0) results[i] = entry;
        else results.push(entry);
      }
      if (results.length > 50) results.length = 50;
      return { discover: { ...s.discover, results } };
    });
  },
};

// ---------------------------------------------------------------- 测试快照

// 只读快照：IPC bearer token 绝不出现在快照里（任何页面脚本/扩展都能读到 window），
// 且按状态对象的引用缓存——1s 一帧的 stats 下深拷贝整个 store 太贵。
let snapCache: { src: AppState | null; snap: unknown } = { src: null, snap: null };

function snapshot(): unknown {
  const state = getState();
  if (snapCache.src === state) return snapCache.snap;
  const snap = JSON.parse(JSON.stringify(state)) as { endpoint?: { token?: string } };
  if (snap.endpoint && 'token' in snap.endpoint) snap.endpoint.token = '<redacted>';
  snapCache = { src: state, snap };
  return snap;
}

Object.defineProperty(window, '__AH_STATE__', {
  configurable: true,
  get: snapshot,
});

/**
 * 列表型选择器的浅比较版：`useStore(s => s.peers.map(...))` 每帧都会造出一个新数组，
 * 默认的 Object.is 比较会把它判成「变了」，于是 1Hz 的 stats 把整棵树重渲一遍。
 */
export function useShallow<T>(selector: (s: AppState) => T[]): T[] {
  return useStoreWithEqualityFn(useStore, selector, (a, b) =>
    a.length === b.length && a.every((x, i) => Object.is(x, b[i])));
}
