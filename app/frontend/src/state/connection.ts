// 连接编排：连接/重连、轮询、权限探测、settings 读写的**唯一出口**。
//
// 这一层刻意不是 React：它的生命周期是「整个应用进程」，不是某个组件的挂载周期。
// 放进 useEffect 会在 StrictMode 的双次挂载、路由切换时被反复起停，而每一次重连
// 都要重新握手 + 重新订阅 stats。组件只通过导出的这些函数触发动作，状态一律经
// store 回流。

import { IpcClient, VersionMismatchError, IPC_VERSION } from '../ipc/client';
import { resolveEndpoint, isTauri, tauriInvoke } from '../ipc/endpoint';
import type { DaemonInfo, DaemonSettings, IpcEndpoint, PeerState, SessionInfo } from '../ipc/types';
import { actions, getState, setState } from './store';
import type { ConnError } from './store';
import { normalizeList, normalizeOne, gateNeeded } from './permissions';
import { toast } from '../components/Toasts';
import { t } from '../i18n';

export { IPC_VERSION };

export const client = new IpcClient();

let statusTimer: ReturnType<typeof setInterval> | null = null;
let peersTimer: ReturnType<typeof setInterval> | null = null;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
let connecting = false;
let booted = false;

export interface RpcOpts { silent?: boolean; timeoutMs?: number }

export async function rpc<T = unknown>(method: string, params: unknown = {}, opts: RpcOpts = {}): Promise<T> {
  try {
    return await client.request<T>(method, params, opts.timeoutMs);
  } catch (e) {
    if (!opts.silent) toast(String((e as Error)?.message || e), 'error');
    throw e;
  }
}

export function ensureDaemon(): Promise<IpcEndpoint> {
  return tauriInvoke<IpcEndpoint>('ensure_daemon');
}

// ---- 连接 ----

function scheduleRetry(): void {
  if (retryTimer) clearTimeout(retryTimer);
  retryTimer = setTimeout(() => void connectDaemon(), 5000);
}

// 整轮连接（含自动拉起）的兜底上限：任何一步挂死都不能让 conn 永久停在
// 'connecting'/'starting'——那样覆盖层没有重试、界面再也不会恢复。
// Rust 侧 ensure_daemon 自带 8s 就绪窗口，这里必须比它 + 认证握手宽裕。
const CONNECT_ATTEMPT_TIMEOUT_MS = 30000;

function withTimeout<T>(promise: Promise<T>, ms: number, msg: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return Promise.race([
    promise.finally(() => { if (timer) clearTimeout(timer); }),
    new Promise<T>((_, rej) => { timer = setTimeout(() => rej(new Error(msg)), ms); }),
  ]);
}

// Rust 的 DaemonError（{kind,message,detail}）越过 invoke 后是普通对象，不是 Error。
function startFailure(err: unknown): Error & { __kind?: string; __detail?: string | null } {
  const raw = err as { message?: string; kind?: string; detail?: string } | undefined;
  const e = new Error(String(raw?.message || err || t('error.startFailed'))) as Error & {
    __kind?: string; __detail?: string | null;
  };
  e.__kind = raw?.kind || 'start-failed';
  e.__detail = raw?.detail || null;
  return e;
}

function connError(e: unknown): ConnError {
  if (e instanceof VersionMismatchError) {
    return { kind: 'version', message: e.message, actual: e.actual, detail: null };
  }
  const tagged = e as { __kind?: string; message?: string; __detail?: string | null };
  if (tagged && tagged.__kind) {
    return { kind: tagged.__kind, message: String(tagged.message), detail: tagged.__detail || null };
  }
  return { kind: 'other', message: String((e as Error)?.message || e), detail: null };
}

async function attemptConnect(): Promise<DaemonInfo> {
  const ep = await resolveEndpoint();
  setState({
    endpoint: ep ? { port: ep.port, token: ep.token } : null,
    endpointSource: ep ? ep.source : null,
  });
  if (ep) {
    try {
      return await client.connect(ep.port, ep.token);
    } catch (e) {
      // 版本不兼容时端口已被占用，再拉一个 daemon 也解决不了。
      if (e instanceof VersionMismatchError) throw e;
      client.close();
      if (!isTauri()) throw e;
    }
  } else if (!isTauri()) {
    const e = new Error(t('error.noEndpoint')) as Error & { __kind?: string };
    e.__kind = 'no-endpoint';
    throw e;
  }

  // 独立 App：启动即可用。用户不需要点任何东西，这里自动把服务拉起来。
  // Rust 侧 ensure_daemon 幂等（先探测 ipc.json + 连接），健康 daemon 绝不重复拉起。
  setState({ conn: 'starting', connError: null });
  let started: IpcEndpoint;
  try {
    started = await ensureDaemon();
  } catch (err) {
    throw startFailure(err);
  }
  setState({
    endpoint: { port: started.port, token: started.token },
    endpointSource: 'tauri',
    conn: 'connecting',
  });
  return client.connect(started.port, started.token);
}

export async function connectDaemon(): Promise<void> {
  if (connecting) return;
  connecting = true;
  if (retryTimer) clearTimeout(retryTimer);
  setState({ conn: 'connecting', connError: null });
  try {
    const daemon = await withTimeout(attemptConnect(), CONNECT_ATTEMPT_TIMEOUT_MS, t('error.connectTimeout'));
    setState({ conn: 'online', daemon, connError: null, lastStatusAt: Date.now() });
    afterConnect();
  } catch (e) {
    client.close(); // 放弃可能仍在挂起的 socket
    setState({ conn: 'offline', connError: connError(e) });
    scheduleRetry();
  } finally {
    connecting = false;
  }
}

function afterConnect(): void {
  void refreshStatus();
  void refreshPeers();
  void refreshSessions();
  void refreshSettings();
  void refreshPermissions({ force: true });
  rpc('stats.subscribe', { interval_ms: 1000 }, { silent: true }).catch(() => {});
  if (statusTimer) clearInterval(statusTimer);
  statusTimer = setInterval(() => void refreshStatus(), 5000);
  if (peersTimer) clearInterval(peersTimer);
  peersTimer = setInterval(() => void refreshPeers(), 10000);
}

export async function refreshStatus(): Promise<void> {
  if (!client.connected) return;
  const t0 = performance.now();
  try {
    const info = await client.request<DaemonInfo>('daemon.status', {});
    setState({ daemon: info, ipcRttMs: performance.now() - t0, lastStatusAt: Date.now() });
  } catch { /* 断线由 close 处理 */ }
  // 模式是 daemon 拥有的全局状态，而 CLI（audiohub ctl settings --set）与另一个
  // UI 窗口都能改它。不跟着轮询，界面就会长期显示一个早已不成立的模式，而模式
  // 决定了整个主面板长什么样——这不是「稍后刷新」能糊过去的偏差。
  await refreshSettings();
}

// 这一版 daemon 可能根本没有 settings.*：那就退回本地缓存 + 驱动状态判定，
// 而不是把「查不到」当成「模式 A」。
export async function refreshSettings(): Promise<void> {
  if (!client.connected) return;
  try {
    actions.setDaemonSettings(await client.request<DaemonSettings>('settings.get', {}));
  } catch (e) {
    const msg = String((e as Error)?.message || e);
    if (/unknown method/i.test(msg)) actions.setSettingsUnsupported();
  }
}

/**
 * 写设置。回包就是新的权威值，直接落库——不做乐观翻转：模式切换在 daemon 侧要
 * 增删虚拟设备，失败时界面若已经翻过去，用户会以为设备该出现却没出现。
 */
export async function applySettings(patch: Partial<DaemonSettings>): Promise<DaemonSettings> {
  const res = await rpc<DaemonSettings>('settings.set', patch);
  actions.setDaemonSettings(res);
  void refreshPeers();   // 模式变了，每个对端的 hal_device 跟着变
  void refreshStatus();
  return res;
}

export async function refreshPeers(): Promise<void> {
  if (!client.connected) return;
  try { actions.setPeers(await client.request<PeerState[]>('peers.list', {})); } catch { /* ignore */ }
}

export async function refreshSessions(): Promise<void> {
  if (!client.connected) return;
  try { actions.pushStats(await client.request<SessionInfo[]>('session.list', {})); } catch { /* ignore */ }
}

// ---- 系统权限探测 ----

// 每次启动都重新探测，**不落任何「已看过」标记**：一旦落盘，用户在系统设置里
// 撤销授权后这道门就再也不出现了，功能会莫名其妙地坏掉而界面一声不吭。
const PERM_MIN_INTERVAL_MS = 800;
let permAt = -Infinity;
let permInflight: Promise<void> | null = null;

export interface PermRefreshOpts { force?: boolean; seed?: unknown }

export async function refreshPermissions(opts: PermRefreshOpts = {}): Promise<void> {
  // request_permission 的回包先落地：权威复查还在路上时，界面已经能翻牌。
  if (opts.seed) {
    const one = normalizeOne(opts.seed, null);
    if (one.id) {
      const list = getState().permissions.list.slice();
      const i = list.findIndex((p) => p.id === one.id);
      if (i >= 0) list[i] = one; else list.push(one);
      actions.setPermissions(list);
    }
  }
  if (!client.connected) return;
  // 这一版 daemon 根本没有权限方法：别在每次窗口聚焦时都去撞一次墙。
  if (getState().permissions.supported === false && !opts.force) return;
  const now = performance.now();
  if (!opts.force && now - permAt < PERM_MIN_INTERVAL_MS) return;
  if (permInflight) return permInflight;
  permAt = now;
  permInflight = (async () => {
    try {
      actions.setPermissions(normalizeList(await client.request('daemon.permissions', {})));
    } catch (e) {
      const msg = String((e as Error)?.message || e);
      // ipcserv.rs 的兜底文案是 unknown method '<name>'：这不是故障，只是这一版
      // 服务不上报权限。查不到就当没有门——「不知道」绝不能被当成「没授权」。
      actions.setPermissionsError(msg, /unknown method/i.test(msg) ? false : null);
    } finally {
      permAt = performance.now();
      permInflight = null;
    }
  })();
  return permInflight;
}

// ---- 授权门是否该挡人 ----

// 门一旦挡上，就只能由用户自己按「进入主界面」或「跳过」让开：最后一项权限刚授权
// 完就把整页抽走，用户会以为自己点错了什么，而且再没机会看一眼可选项。
// armed 是**跨渲染的粘滞位**，所以放在模块里而不是组件 state。
let gateArmed = false;

export function gateVisible(): boolean {
  const s = getState();
  gateArmed = !s.permissions.dismissed && (gateArmed || gateNeeded(s.permissions.list));
  // 服务没连上时权限也查不出来，且覆盖层正盖在最上面——此刻挂门只会两层叠着。
  // 但 armed 保留着：连回来还得继续挡。
  return gateArmed && s.conn === 'online';
}

// ---- 托盘 ----

let trayKey: string | null = null;

export function syncTray(): void {
  const s = getState();
  if (s.mode !== 'tauri') return;
  const online = s.conn === 'online';
  const port = s.endpoint ? s.endpoint.port : null;
  const key = `${online}|${port}`;
  if (key === trayKey) return;
  trayKey = key;
  tauriInvoke('set_tray_status', { online, port: online ? port : null }).catch(() => {});
}

// ---- 启动 ----

client.on('close', () => {
  if (statusTimer) clearInterval(statusTimer);
  if (peersTimer) clearInterval(peersTimer);
  setState({ conn: 'offline' });
  scheduleRetry();
});

client.on('event:stats', (data) => actions.pushStats(data));

// 用户很可能刚在系统设置里点完授权切回来：那边的改动不会通知我们，只能自己复查。
function reprobeOnReturn(): void {
  if (document.hidden) return;
  void refreshPermissions();
}

export function boot(): void {
  if (booted) return; // React StrictMode 会把 effect 跑两遍
  booted = true;
  const tauri = isTauri();
  setState({ mode: tauri ? 'tauri' : 'browser' });
  document.body.classList.toggle('is-tauri', tauri);
  window.addEventListener('focus', reprobeOnReturn);
  document.addEventListener('visibilitychange', reprobeOnReturn);
  void connectDaemon();
}
