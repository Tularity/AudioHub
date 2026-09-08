// 连接编排：连接/重连、轮询、权限探测、settings 读写的**唯一出口**。
//
// 这一层刻意不是 React：它的生命周期是「整个应用进程」，不是某个组件的挂载周期。
// 放进 useEffect 会在 StrictMode 的双次挂载、路由切换时被反复起停，而每一次重连
// 都要重新握手 + 重新订阅 stats。组件只通过导出的这些函数触发动作，状态一律经
// store 回流。

import { IpcClient, VersionMismatchError, IPC_VERSION } from '../ipc/client';
import { resolveEndpoint, isTauri, tauriInvoke } from '../ipc/endpoint';
import type {
  AirPlaySessionInfo, DaemonInfo, DaemonSettings, DaemonSettingsPatch,
  DaemonServiceStatus, DriverInstallResult, IpcEndpoint, PeerState, SessionInfo,
} from '../ipc/types';
import { actions, getState, setState } from './store';
import type { ConnError } from './store';
import { effectiveMode } from './mode';
import { normalizeList, normalizeOne, gateNeeded } from './permissions';
import { trayVolumeOf } from '../lib/trayVolume';
import { applyChromeDirection } from '../lib/platform';
import { activeTheme } from '../lib/appearanceHost';
import { iconStateFrom } from '../lib/trayIcon';
import { isUnknownMethod, sanitizeDaemonSettings } from '../lib/airplay';
import { nativeLocaleNeedsSync } from '../lib/nativeLocale';
import { nativeServiceGate } from '../lib/daemonService';
import type { NativeServiceGate } from '../lib/daemonService';
import { autoRetryDelay } from '../lib/connectionRetry';
import { toast } from '../components/Toasts';
import { getLocale, t } from '../i18n';

export { IPC_VERSION };

export const client = new IpcClient();

let statusTimer: ReturnType<typeof setInterval> | null = null;
let peersTimer: ReturnType<typeof setInterval> | null = null;
let airplayTimer: ReturnType<typeof setInterval> | null = null;
let retryTimer: ReturnType<typeof setTimeout> | null = null;
let retryAttempt = 0;
let nativeLifecycleOperation = false;
let booted = false;

/**
 * Identity of one connectDaemon transaction.
 *
 * Promise.race cannot cancel the losing promise.  A timed-out native status
 * probe can therefore resume much later, and without this fence it would be
 * able to open a socket over the current attempt, close that socket from its
 * catch block, and publish stale state. `live` stops work inside the attempt;
 * the monotonic generation separately tells its outer controller whether no
 * newer attempt has taken over the visible state slot.
 */
interface ConnectAttempt {
  generation: number;
  live: boolean;
}

let nextConnectGeneration = 0;
let activeConnectAttempt: ConnectAttempt | null = null;

class StaleConnectAttempt extends Error {
  constructor() {
    super('stale AudioHub connection attempt');
    this.name = 'StaleConnectAttempt';
  }
}

function attemptOwnsSlot(attempt: ConnectAttempt): boolean {
  return activeConnectAttempt?.generation === attempt.generation;
}

function assertCurrentAttempt(attempt: ConnectAttempt): void {
  if (!attempt.live || !attemptOwnsSlot(attempt)) throw new StaleConnectAttempt();
}

async function awaitCurrentAttempt<T>(attempt: ConnectAttempt, promise: Promise<T>): Promise<T> {
  const value = await promise;
  assertCurrentAttempt(attempt);
  return value;
}

/** Invalidate the old generation before a retry or native lifecycle action. */
function invalidateActiveConnectAttempt(): boolean {
  const previous = activeConnectAttempt;
  if (!previous) return false;
  previous.live = false;
  activeConnectAttempt = null;
  return true;
}

function beginConnectAttempt(): ConnectAttempt {
  const attempt = { generation: ++nextConnectGeneration, live: true };
  activeConnectAttempt = attempt;
  return attempt;
}

export interface RpcOpts { silent?: boolean; timeoutMs?: number }

export async function rpc<T = unknown>(method: string, params: unknown = {}, opts: RpcOpts = {}): Promise<T> {
  try {
    return await client.request<T>(method, params, opts.timeoutMs);
  } catch (e) {
    // IPC errors are daemon diagnostics and may have been authored in another
    // locale.  Keep the raw value in the developer console; visible copy must
    // come from the active UI catalogue until the wire carries structured
    // error codes.
    console.error(`[audiohub] ${method} failed`, e);
    if (!opts.silent) toast(t('error.requestFailed'), 'error');
    throw e;
  }
}

export function ensureDaemon(): Promise<IpcEndpoint> {
  return tauriInvoke<IpcEndpoint>('ensure_daemon');
}

export function daemonServiceStatus(): Promise<DaemonServiceStatus> {
  return tauriInvoke<DaemonServiceStatus>('daemon_service_status');
}

/**
 * Complete the explicit native recovery action selected on the service gate.
 * Registration and process start are deliberately separate: a stopped,
 * already-installed service must not silently rewrite the user's login item.
 */
export async function recoverDaemon(action: 'install' | 'start'): Promise<void> {
  if (!isTauri() || nativeLifecycleOperation) return;
  nativeLifecycleOperation = true;
  // A delayed status/handshake from the service gate must not resume inside the
  // install/start transaction. If it already owns a socket, abandon that exact
  // pre-transaction socket before invoking native lifecycle code.
  if (invalidateActiveConnectAttempt()) client.close();
  cancelRetry(true);
  setState({ conn: 'starting', connError: null });
  try {
    await tauriInvoke<IpcEndpoint>(
      action === 'install' ? 'install_daemon_service' : 'start_daemon_service',
    );
  } catch (err) {
    setState({ conn: 'offline', connError: connError(startFailure(err)) });
    nativeLifecycleOperation = false;
    return;
  }
  nativeLifecycleOperation = false;
  await connectDaemon();
}

/**
 * Restart the native daemon as one UI transaction. While it runs, the socket
 * close handler suppresses its ordinary retry so it cannot race the native
 * stop/start window. The command only returns once the replacement endpoint is
 * healthy; reconnect immediately instead of leaving the user behind a
 * five-second offline retry gate.
 */
export async function restartDaemonService(): Promise<void> {
  await runNativeLifecycleCommand<IpcEndpoint>('restart_daemon_service');
  if (getState().conn !== 'online') {
    throw new Error(t('settings.driver.serviceRestartFailed'));
  }
}

export async function installDriverAndReconnect(): Promise<DriverInstallResult> {
  return runNativeLifecycleCommand<DriverInstallResult>('install_driver');
}

async function runNativeLifecycleCommand<T>(command: string): Promise<T> {
  if (!isTauri() || nativeLifecycleOperation) {
    throw new Error(t('settings.driver.serviceRestartFailed'));
  }
  nativeLifecycleOperation = true;
  if (invalidateActiveConnectAttempt()) client.close();
  cancelRetry(true);
  let result: T;
  try {
    result = await tauriInvoke<T>(command);
  } catch (error) {
    nativeLifecycleOperation = false;
    // The operation may have failed before touching the daemon (for example a
    // cancelled authorization) or after its socket closed. In the latter
    // case, expose one stable, actionable failure. Starting a reconnect timer
    // here races the lifecycle command's own stop/start transaction and used
    // to multiply blocked auth attempts when the replacement daemon was
    // present but unhealthy.
    if (!client.connected) {
      setState({ conn: 'offline', connError: connError(startFailure(error)) });
    }
    throw error;
  }
  // A successful driver install may or may not have restarted the daemon
  // (Windows can require a reboot), while Restart always did. Re-authenticate
  // either way so the UI cannot retain a socket and status snapshot belonging
  // to the previous process generation.
  setState({ conn: 'starting', connError: null });
  client.close();
  nativeLifecycleOperation = false;
  await connectDaemon();
  return result;
}

// ---- 连接 ----

function cancelRetry(resetBudget = false): void {
  if (retryTimer) clearTimeout(retryTimer);
  retryTimer = null;
  if (resetBudget) retryAttempt = 0;
}

function scheduleRetry(error: ConnError): void {
  // One owner, one timer. A WebSocket close emitted by the attempt itself is
  // suppressed below; this guard also protects against any future duplicate
  // notification path without shifting the promised retry deadline.
  if (retryTimer) return;
  const delay = autoRetryDelay(error.kind, retryAttempt);
  if (delay == null) return;
  retryAttempt += 1;
  retryTimer = setTimeout(() => {
    retryTimer = null;
    void connectDaemon({ background: true });
  }, delay);
}

// 整轮连接（含自动拉起）的兜底上限：任何一步挂死都不能让 conn 永久停在
// 'connecting'/'starting'——那样覆盖层没有重试、界面再也不会恢复。
// Rust 侧 ensure_daemon 自带 8s 就绪窗口，这里必须比它 + 认证握手宽裕。
const CONNECT_ATTEMPT_TIMEOUT_MS = 30000;

function withTimeout<T>(
  promise: Promise<T>,
  ms: number,
  msg: string,
  onTimeout: () => void,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return Promise.race([
    promise.finally(() => { if (timer) clearTimeout(timer); }),
    new Promise<T>((_, rej) => {
      timer = setTimeout(() => {
        onTimeout();
        rej(new Error(msg));
      }, ms);
    }),
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

function nativeGateFailure(gate: Exclude<NativeServiceGate, null>): Error {
  switch (gate) {
    case 'no-binary':
      return startFailure({ kind: gate, message: t('overlay.noBinary.title') });
    case 'not-installed':
      return startFailure({ kind: gate, message: t('overlay.notInstalled.title') });
    case 'stopped':
      return startFailure({ kind: gate, message: t('overlay.stopped.title') });
  }
}

async function attemptConnect(attempt: ConnectAttempt): Promise<DaemonInfo> {
  assertCurrentAttempt(attempt);
  // Native startup is gated by the installation transaction even when an old
  // or manually-started daemon happens to be reachable. Otherwise a failed
  // registration can disappear on the next poll: the socket connects, the UI
  // skips service status, and the user is told setup succeeded when no login
  // entry/marker exists.
  let nativeService: DaemonServiceStatus | null = null;
  if (isTauri()) {
    try {
      nativeService = await awaitCurrentAttempt(attempt, daemonServiceStatus());
    } catch (err) {
      if (err instanceof StaleConnectAttempt) throw err;
      throw startFailure(err);
    }
    const gate = nativeServiceGate(nativeService);
    // Do this before resolveEndpoint/client.connect. `running: false` means the
    // native shell did not bind ipc.json's PID to the selected daemon image; a
    // reachable old/foreign endpoint must not bypass that ownership boundary.
    if (gate) throw nativeGateFailure(gate);
  }

  const ep = await awaitCurrentAttempt(attempt, resolveEndpoint());
  setState({
    endpoint: ep ? { port: ep.port, token: ep.token } : null,
    endpointSource: ep ? ep.source : null,
  });
  if (ep) {
    try {
      return await awaitCurrentAttempt(attempt, client.connect(ep.port, ep.token));
    } catch (e) {
      if (e instanceof StaleConnectAttempt) throw e;
      // 版本不兼容时端口已被占用，再拉一个 daemon 也解决不了。
      if (e instanceof VersionMismatchError) throw e;
      assertCurrentAttempt(attempt);
      client.close();
      if (!isTauri()) throw e;
    }
  } else if (!isTauri()) {
    const e = new Error(t('error.noEndpoint')) as Error & { __kind?: string };
    e.__kind = 'no-endpoint';
    throw e;
  }

  // Native App owns the service lifecycle, but first installation and an
  // ordinary stopped service are different user decisions. Do not mutate a
  // login item or start a background process merely because a polling retry
  // happened. The overlay exposes the correct explicit action for each state.
  let service = nativeService;
  if (!service) {
    try {
      service = await awaitCurrentAttempt(attempt, daemonServiceStatus());
    } catch (err) {
      if (err instanceof StaleConnectAttempt) throw err;
      throw startFailure(err);
    }
  }
  const gate = nativeServiceGate(service);
  if (gate) throw nativeGateFailure(gate);
  if (service.running) {
    // The native probe says a daemon is accepting local connections, so a
    // failed WebSocket/auth handshake is not a stopped-service condition. Keep
    // the truthful generic reconnect action while the endpoint/token settles.
    throw startFailure({ kind: 'running-unreachable', message: t('overlay.disconnected.title') });
  }
  throw startFailure({ kind: 'stopped', message: t('overlay.stopped.title') });
}

interface ConnectDaemonOpts {
  /** Keep the current offline error visible while a timer-owned attempt runs. */
  background?: boolean;
}

export async function connectDaemon(opts: ConnectDaemonOpts = {}): Promise<void> {
  // A timer never supersedes an in-flight attempt. A user/lifecycle call does:
  // it creates a fresh generation even while the old Promise.race loser is
  // still pending, and abandons only that pre-existing attempt's socket.
  if (opts.background && activeConnectAttempt) return;
  const superseded = invalidateActiveConnectAttempt();
  if (superseded) client.close();
  // Direct calls are user/startup/lifecycle actions. Each gets a fresh finite
  // retry budget and supersedes any pending background attempt.
  if (!opts.background) cancelRetry(true);
  const attempt = beginConnectAttempt();
  if (!opts.background) setState({ conn: 'connecting', connError: null });
  try {
    const daemon = await withTimeout(
      attemptConnect(attempt),
      CONNECT_ATTEMPT_TIMEOUT_MS,
      t('error.connectTimeout'),
      // Keep the controller in its slot so it can publish this timeout, but
      // fence the losing promise before it can cross another await boundary.
      () => { if (attemptOwnsSlot(attempt)) attempt.live = false; },
    );
    assertCurrentAttempt(attempt);
    cancelRetry(true);
    setState({
      conn: 'online', daemon, connError: null, lastStatusAt: Date.now(),
      airplaySessions: [], airplaySessionsSupported: null,
    });
    afterConnect();
  } catch (e) {
    // A newer generation owns both the socket and the visible connection state.
    // The old controller must not close, overwrite, or schedule anything.
    if (!attemptOwnsSlot(attempt) || e instanceof StaleConnectAttempt) return;
    attempt.live = false;
    client.close(); // 放弃可能仍在挂起的 socket
    const error = connError(e);
    setState({ conn: 'offline', connError: error });
    scheduleRetry(error);
  } finally {
    if (attemptOwnsSlot(attempt)) activeConnectAttempt = null;
  }
}

function afterConnect(): void {
  void refreshStatus();
  void refreshPeers();
  void refreshSessions();
  void refreshAirPlaySessions();
  void refreshSettings();
  void refreshPermissions({ force: true });
  rpc('stats.subscribe', { interval_ms: 1000 }, { silent: true }).catch(() => {});
  if (statusTimer) clearInterval(statusTimer);
  statusTimer = setInterval(() => void refreshStatus(), 5000);
  if (peersTimer) clearInterval(peersTimer);
  peersTimer = setInterval(() => void refreshPeers(), 10000);
  if (airplayTimer) clearInterval(airplayTimer);
  airplayTimer = setInterval(() => void refreshAirPlaySessions(), 3000);
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
    // Only the installed native App owns native OS copy. A browser connected
    // to the same daemon may use another language, but must never rename this
    // machine's system devices underneath the interactive desktop user.
    syncNativeAppearance();
  } catch (e) {
    const msg = String((e as Error)?.message || e);
    if (/unknown method/i.test(msg)) actions.setSettingsUnsupported();
  }
}

/**
 * 写设置。回包就是新的权威值，直接落库——不做乐观翻转：模式切换在 daemon 侧要
 * 增删虚拟设备，失败时界面若已经翻过去，用户会以为设备该出现却没出现。
 */
export async function applySettings(patch: DaemonSettingsPatch): Promise<DaemonSettings> {
  const raw = await rpc<unknown>('settings.set', patch);
  const res = sanitizeDaemonSettings(raw);
  if (!res) {
    const error = new Error(t('error.requestFailed'));
    toast(error.message, 'error');
    throw error;
  }
  actions.setDaemonSettings(res);
  void refreshPeers();   // 模式变了，每个对端的 hal_device 跟着变
  void refreshStatus();
  void refreshAirPlaySessions();
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

/** 外部 AirPlay 来源不走 stats 订阅，连接后立即取一次，并以短轮询保持权威。 */
export async function refreshAirPlaySessions(): Promise<void> {
  if (!client.connected || getState().airplaySessionsSupported === false) return;
  try {
    const list = await client.request<AirPlaySessionInfo[]>('airplay.sessions.list', {});
    actions.setAirPlaySessions(list);
  } catch (error) {
    // 旧 daemon 没有这个方法不是运行故障；本次连接不再重试，重连时重新探测。
    if (isUnknownMethod(error)) actions.setAirPlaySessionsUnsupported();
  }
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
      actions.setPermissionsError(
        /unknown method/i.test(msg) ? null : t('settings.perm.error'),
        /unknown method/i.test(msg) ? false : null,
      );
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
let nativeLocaleSyncing = false;

/**
 * Push the resolved locale to native, machine-wide surfaces. This entry point
 * is subscribed to locale changes by App.tsx and is also run after every
 * settings refresh, covering both first connection and daemon restarts.
 *
 * `mode === 'tauri'` is the security/ownership boundary: browser Web UI locale
 * remains a viewer preference and cannot rename OS devices or the App tray.
 */
export function syncNativeAppearance(): void {
  syncTray();
  const s = getState();
  const want = getLocale();
  if (!client.connected || nativeLocaleSyncing || !nativeLocaleNeedsSync(
    s.mode, s.conn === 'online', s.daemonSettings?.native_locale, want,
  )) return;
  nativeLocaleSyncing = true;
  void client.request<unknown>('settings.set', { native_locale: want })
    .then((raw) => {
      const settings = sanitizeDaemonSettings(raw);
      if (settings) actions.setDaemonSettings(settings);
      void refreshPeers();
    })
    .catch(() => { /* next 5 s settings refresh retries; no locale-change toast */ })
    .finally(() => {
      nativeLocaleSyncing = false;
      // The user can choose another language while this write is in flight.
      // Re-read current state instead of letting the older reply win forever.
      if (getLocale() !== want) {
        syncNativeAppearance();
      }
    });
}

/**
 * 写回 macOS 菜单栏那条音量滑条拖出来的值。
 *
 * 会话 id **在事件到达的这一刻现取**，不由原生侧带过来：菜单可以一直开着，而
 * 期间对端可能断开、模式可能被 CLI 改掉。原生侧记住的 id 会指向一条已经不存在的
 * 会话，那次写入要么静默失败、要么打到别的会话上——两种都比「什么都不做」坏。
 *
 * silent：拖动会连发，失败不刷 toast（与对端卡片上那条滑条同一条纪律）。
 */
export function setTrayVolume(scalar: number): void {
  const s = getState();
  const vol = trayVolumeOf(effectiveMode(s), s.sessions);
  if (!vol) return;
  // 不带 muted：省略即保持对端当前静音态（IPC 契约，见 VolumeControl.tsx 顶部）。
  // 拖一下滑条就把对端悄悄解除静音，是这条契约要挡的那件事。
  void rpc('session.set_volume', { id: vol.id, scalar }, { silent: true }).catch(() => {});
}

export function syncTray(): void {
  const s = getState();
  if (s.mode !== 'tauri') return;
  const online = s.conn === 'online';
  const port = s.endpoint ? s.endpoint.port : null;
  const state = iconStateFrom({ conn: s.conn, sessionCount: s.sessions.length });
  // 传 activeTheme() 而不是系统深浅：用户把主题钉成浅色时，窗口是浅色的，
  // Dock 图标就该跟着窗口，而不是跟着系统。
  const theme = activeTheme();
  const locale = getLocale();
  // macOS 菜单栏那条音量滑条。`null` = 整行不出现（不是「出现但为 0」——0 是一个
  // 真实的音量值，把「没有可调对象」画成 0 就是在撒谎）。
  //
  // 量化到 1%：这个函数挂在 store 订阅上，每一帧 stats 都会走一遍，而菜单栏滑条
  // 的显示精度远低于 1%。不量化则去重键每帧都变，等于每秒往原生侧打一次无意义的
  // 调用；量化之后**发出去的值与去重键里的值是同一个数**，去重键仍然覆盖了全部
  // 进入参数表的量（这正是下面那条注释的要求）。
  const vol = trayVolumeOf(effectiveMode(s), s.sessions);
  const volume = vol ? Math.round(vol.scalar * 100) / 100 : null;
  const muted = vol ? vol.muted : null;
  // 去重键必须覆盖每一个进了参数表的量，否则新维度的变化会被这一行悄悄吃掉。
  const key = `${online}|${port}|${state}|${theme}|${locale}|${volume}|${muted}`;
  if (key === trayKey) return;
  trayKey = key;
  tauriInvoke('set_tray_status', {
    online,
    port: online ? port : null,
    state,
    theme,
    locale,
    volume,
    muted,
  }).catch(() => {});
}

// ---- 启动 ----

client.on('close', () => {
  if (statusTimer) clearInterval(statusTimer);
  if (peersTimer) clearInterval(peersTimer);
  if (airplayTimer) clearInterval(airplayTimer);
  setState({
    conn: nativeLifecycleOperation ? 'starting' : 'offline',
    connError: nativeLifecycleOperation ? null : getState().connError,
    airplaySessions: [], airplaySessionsSupported: null,
  });
  if (nativeLifecycleOperation) return;
  // A connection attempt owns its own failure classification and retry
  // decision. Its socket may emit close before connect() rejects; scheduling
  // here as well used to create a second visible attempt path.
  if (activeConnectAttempt) return;
  const error = getState().connError || {
    kind: 'running-unreachable', message: t('overlay.disconnected.title'), detail: null,
  };
  scheduleRetry(error);
});

client.on('event:stats', (data) => actions.pushStats(data));

// 用户很可能刚在系统设置里点完授权切回来：那边的改动不会通知我们，只能自己复查。
function reprobeOnReturn(): void {
  if (document.hidden) return;
  // Login startup can finish after the first read-only service probe. A
  // stopped gate has no timer retry; recheck when the user returns without
  // starting a service they deliberately stopped or racing a lifecycle action.
  const state = getState();
  if (isTauri() && !nativeLifecycleOperation && state.conn === 'offline' &&
      state.connError?.kind === 'stopped') {
    void connectDaemon({ background: true });
  }
  void refreshPermissions();
}

export function boot(): void {
  if (booted) return; // React StrictMode 会把 effect 跑两遍
  booted = true;
  const tauri = isTauri();
  setState({ mode: tauri ? 'tauri' : 'browser' });
  document.body.classList.toggle('is-tauri', tauri);
  // 顶栏让位方向。与上一行同为「一次性、由环境决定」的 body 标记，放在一起是为了
  // 让「外壳形态取决于什么」这件事只有一个落点。
  applyChromeDirection();
  window.addEventListener('focus', reprobeOnReturn);
  document.addEventListener('visibilitychange', reprobeOnReturn);
  void connectDaemon();
}
