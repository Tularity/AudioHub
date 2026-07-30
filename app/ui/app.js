// 入口：连接（Tauri 内自动拉起服务 / 浏览器测试挂钩 ?port&token）、路由、顶栏徽标与覆盖层。

import { store } from './store.js';
import { IpcClient, VersionMismatchError, IPC_VERSION } from './ws.js';
import { el, toast } from './ui.js';
import { normalizeList, normalizeOne, gateNeeded } from './permissions.js';
import * as onboardingView from './views/onboarding.js';
import * as peersView from './views/peers.js';
import * as detailView from './views/detail.js';
import * as pairView from './views/pair.js';
import * as settingsView from './views/settings.js';
import * as statsView from './views/stats.js';

const VIEWS = { peers: peersView, detail: detailView, pair: pairView, settings: settingsView, stats: statsView };
const TITLES = { peers: '主面板', detail: '对端详情', pair: '配对向导', settings: '设置', stats: '统计诊断' };
const NAV_OF = { detail: 'peers' }; // 详情高亮主面板

const client = new IpcClient();
let statusTimer = null;
let peersTimer = null;
let retryTimer = null;
let connecting = false;

function isTauri() {
  return !!window.__TAURI__;
}

function tauriInvoke(cmd, args) {
  const t = window.__TAURI__ || {};
  const inv = (t.core && t.core.invoke) || t.invoke;
  if (!inv) return Promise.reject(new Error('非 Tauri 环境'));
  return inv(cmd, args);
}

async function rpc(method, params = {}, opts = {}) {
  try {
    return await client.request(method, params, opts.timeoutMs);
  } catch (e) {
    if (!opts.silent) toast(String((e && e.message) || e), 'error');
    throw e;
  }
}

function ensureDaemon() {
  return tauriInvoke('ensure_daemon');
}

// ---- 连接 ----

async function resolveEndpoint() {
  // ?port&token 只是自动化测试挂钩（Playwright），不是用户路径。
  const q = new URLSearchParams(location.search);
  const qPort = Number(q.get('port'));
  const qToken = q.get('token');
  if (qPort && qToken) return { port: qPort, token: qToken };
  if (isTauri()) {
    const ep = await tauriInvoke('get_ipc_endpoint').catch(() => null);
    if (ep && ep.port && ep.token) return { port: ep.port, token: ep.token };
  }
  return null;
}

function scheduleRetry() {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(connectDaemon, 5000);
}

// 整轮连接（含自动拉起）的兜底上限：任何一步挂死都不能让 conn 永久停在
// 'connecting'/'starting'——那样覆盖层没有重试、界面再也不会恢复。
// Rust 侧 ensure_daemon 自带 8s 就绪窗口，这里必须比它 + 认证握手宽裕。
const CONNECT_ATTEMPT_TIMEOUT_MS = 30000;

function withTimeout(promise, ms, msg) {
  let timer = null;
  return Promise.race([
    promise.finally(() => clearTimeout(timer)),
    new Promise((_, rej) => { timer = setTimeout(() => rej(new Error(msg)), ms); }),
  ]);
}

// Rust 的 DaemonError（{kind,message,detail}）越过 invoke 后是普通对象，不是 Error。
function startFailure(err) {
  const e = new Error(String((err && err.message) || err || '启动服务失败'));
  e.__kind = (err && err.kind) || 'start-failed';
  e.__detail = (err && err.detail) || null;
  return e;
}

function connError(e) {
  if (e instanceof VersionMismatchError) {
    return { kind: 'version', message: e.message, actual: e.actual, detail: null };
  }
  if (e && e.__kind) {
    return { kind: e.__kind, message: e.message, detail: e.__detail || null };
  }
  return { kind: 'other', message: String((e && e.message) || e), detail: null };
}

async function attemptConnect() {
  const ep = await resolveEndpoint();
  store.update((s) => { s.endpoint = ep; });
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
    const e = new Error('未提供连接参数');
    e.__kind = 'no-endpoint';
    throw e;
  }

  // 独立 App：启动即可用。用户不需要点任何东西，这里自动把服务拉起来。
  // Rust 侧 ensure_daemon 幂等（先探测 ipc.json + 连接），健康 daemon 绝不重复拉起。
  store.update((s) => { s.conn = 'starting'; s.connError = null; });
  let started;
  try {
    started = await ensureDaemon();
  } catch (err) {
    throw startFailure(err);
  }
  store.update((s) => {
    s.endpoint = { port: started.port, token: started.token };
    s.conn = 'connecting';
  });
  return client.connect(started.port, started.token);
}

async function connectDaemon() {
  if (connecting) return;
  connecting = true;
  clearTimeout(retryTimer);
  store.update((s) => { s.conn = 'connecting'; s.connError = null; });
  try {
    const daemon = await withTimeout(attemptConnect(), CONNECT_ATTEMPT_TIMEOUT_MS, '连接服务超时');
    store.update((s) => {
      s.conn = 'online';
      s.daemon = daemon;
      s.connError = null;
      s.lastStatusAt = Date.now();
    });
    afterConnect();
  } catch (e) {
    client.close(); // 放弃可能仍在挂起的 socket
    store.update((s) => { s.conn = 'offline'; s.connError = connError(e); });
    scheduleRetry();
  } finally {
    connecting = false;
  }
}

function afterConnect() {
  refreshStatus();
  refreshPeers();
  refreshSessions();
  refreshPermissions({ force: true });
  rpc('stats.subscribe', { interval_ms: 1000 }, { silent: true }).catch(() => {});
  clearInterval(statusTimer);
  statusTimer = setInterval(refreshStatus, 5000);
  clearInterval(peersTimer);
  peersTimer = setInterval(refreshPeers, 10000);
}

async function refreshStatus() {
  if (!client.connected) return;
  const t0 = performance.now();
  try {
    const info = await client.request('daemon.status', {});
    const rtt = performance.now() - t0;
    store.update((s) => {
      s.daemon = info;
      s.ipcRttMs = rtt;
      s.lastStatusAt = Date.now();
    });
  } catch (_) { /* 断线由 close 处理 */ }
}

async function refreshPeers() {
  if (!client.connected) return;
  try { store.setPeers(await client.request('peers.list', {})); } catch (_) { /* ignore */ }
}

async function refreshSessions() {
  if (!client.connected) return;
  try { store.pushStats(await client.request('session.list', {})); } catch (_) { /* ignore */ }
}

// ---- 系统权限探测 ----

// 每次启动都重新探测，**不落任何「已看过」标记**：一旦落盘，用户在系统设置里
// 撤销授权后这道门就再也不出现了，功能会莫名其妙地坏掉而界面一声不吭。
const PERM_MIN_INTERVAL_MS = 800;
let permAt = -Infinity;
let permInflight = null;

function mergeOne(list, one) {
  const out = list.slice();
  const i = out.findIndex((p) => p.id === one.id);
  if (i >= 0) out[i] = one;
  else out.push(one);
  return out;
}

async function refreshPermissions(opts = {}) {
  // request_permission 的回包先落地：权威复查还在路上时，界面已经能翻牌。
  if (opts.seed) {
    const one = normalizeOne(opts.seed, null);
    if (one.id) store.setPermissions(mergeOne(store.state.permissions.list, one));
  }
  if (!client.connected) return;
  // 这一版 daemon 根本没有权限方法：别在每次窗口聚焦时都去撞一次墙。
  if (store.state.permissions.supported === false && !opts.force) return;
  const now = performance.now();
  if (!opts.force && now - permAt < PERM_MIN_INTERVAL_MS) return;
  if (permInflight) return permInflight;
  permAt = now;
  permInflight = (async () => {
    try {
      store.setPermissions(normalizeList(await client.request('daemon.permissions', {})));
    } catch (e) {
      const msg = String((e && e.message) || e);
      // ipcserv.rs 的兜底文案是 unknown method '<name>'：这不是故障，只是这一版
      // 服务不上报权限。查不到就当没有门——「不知道」绝不能被当成「没授权」。
      store.setPermissionsError(msg, /unknown method/i.test(msg) ? false : null);
    } finally {
      permAt = performance.now();
      permInflight = null;
    }
  })();
  return permInflight;
}

// 用户很可能刚在系统设置里点完授权切回来：那边的改动不会通知我们，只能自己复查。
function reprobeOnReturn() {
  if (document.hidden) return;
  refreshPermissions();
}

client.on('close', () => {
  clearInterval(statusTimer);
  clearInterval(peersTimer);
  store.update((s) => { s.conn = 'offline'; });
  scheduleRetry();
});

client.on('event:stats', (data) => store.pushStats(data));

// ---- 路由 ----

const ctx = { rpc, navigate, isTauri, ensureDaemon, refreshPeers, refreshSessions, refreshPermissions };
let currentCleanup = null;

function navigate(view, peerFp = null) {
  if (!VIEWS[view]) view = 'peers';
  store.update((s) => { s.route = { view, peerFp }; });
  mountView();
}

function mountView() {
  const { view } = store.state.route;
  const root = document.getElementById('view-root');
  if (currentCleanup) {
    try { currentCleanup(); } catch (e) { console.error(e); }
    currentCleanup = null;
  }
  root.innerHTML = '';
  const wrap = el('section', { class: 'view', 'data-testid': `view-${view}` });
  root.append(wrap);
  currentCleanup = VIEWS[view].mount(wrap, ctx) || null;
  document.getElementById('view-title').textContent = TITLES[view];
  const active = NAV_OF[view] || view;
  document.querySelectorAll('#nav .nav-item').forEach((b) => {
    b.classList.toggle('active', b.dataset.view === active);
  });
}

// ---- 首启授权门 ----

// 门是**算出来**的，不是记出来的：只要还有「必需 + 状态可知 + 未授权」的项就挡人
// （permissions.js isBlocking）。dismissed 只活在内存里，重启即失效。
const gateCtx = {
  rpc,
  refresh: (o) => refreshPermissions(o || {}),
  dismiss: (skipped) => store.dismissGate(skipped),
};

let gateCleanup = null;
let gateTimer = null;
let gateShown = false;
let gateArmed = false;

function syncGate(s) {
  // 门一旦挡上，就只能由用户自己按「进入主界面」或「跳过」让开：最后一项权限刚授权
  // 完就把整页抽走，用户会以为自己点错了什么，而且再没机会看一眼可选项。
  gateArmed = !s.permissions.dismissed && (gateArmed || gateNeeded(s.permissions.list));
  // 服务没连上时权限也查不出来，且 #overlay 正盖在最上面——此刻挂门只会两层叠着。
  // 但 armed 保留着：连回来还得继续挡。
  const want = gateArmed && s.conn === 'online';
  if (want === gateShown) return;
  gateShown = want;
  const host = document.getElementById('gate');
  if (gateCleanup) {
    try { gateCleanup(); } catch (e) { console.error(e); }
    gateCleanup = null;
  }
  clearInterval(gateTimer);
  gateTimer = null;
  host.innerHTML = '';
  host.hidden = !want;
  if (!want) return;
  gateCleanup = onboardingView.mount(host, gateCtx) || null;
  // 系统设置里的改动没有任何通知；focus 事件在某些窗口状态下也不来。
  // 门开着的时候多这一路慢轮询，用户回来就能看见状态自己翻过来。
  gateTimer = setInterval(() => refreshPermissions(), 5000);
}

// ---- 覆盖层文案 ----

// 每一种失败原因都要给出**不同的**下一步动作；kind 与 src-tauri/src/main.rs
// 的 DaemonError::kind 一一对应，那边加一种这里就要加一条。
const FAILURE_COPY = {
  'no-binary': {
    title: '找不到 AudioHub 服务程序',
    desc: '应用内缺少 audiohub 服务程序，无法启动音频服务。请重新安装 AudioHub；'
      + '若在开发环境运行，可设置环境变量 AUDIOHUB_BIN 指向已编译的 audiohub。',
    hint: '重装后再点「重试」。',
  },
  'spawn-failed': {
    title: '无法启动 AudioHub 服务',
    desc: '服务程序找到了，但拉起失败——通常是文件权限或系统隔离属性所致。'
      + '可尝试重新安装，或在终端手动运行一次 audiohub daemon 查看具体报错。',
    hint: '',
  },
  'port-busy': {
    title: 'AudioHub 服务端口被占用',
    desc: '所需端口已被其他程序（很可能是仍在运行的旧实例）占用。请先结束它，'
      + '或在终端执行 audiohub ctl shutdown，然后重试。',
    hint: '',
  },
  timeout: {
    title: 'AudioHub 服务启动超时',
    desc: '服务进程已拉起，但未在预期时间内就绪。请稍候重试；若持续失败，'
      + '在终端运行 audiohub daemon 观察启动日志。',
    hint: '',
  },
  'start-failed': {
    title: '无法启动 AudioHub 服务',
    desc: '启动服务时发生未预期的错误。请重试；若持续失败，在终端运行 audiohub daemon 查看报错。',
    hint: '',
  },
  internal: {
    title: '无法启动 AudioHub 服务',
    desc: '界面与本机服务管理器之间的调用失败。请重试，或重启 AudioHub。',
    hint: '',
  },
};

// ---- 顶栏徽标 / 覆盖层 / 侧栏脚注 ----

let chromeKey = null;
let trayKey = null;

function syncTray(s) {
  if (s.mode !== 'tauri') return;
  const online = s.conn === 'online';
  const port = s.endpoint ? s.endpoint.port : null;
  const key = `${online}|${port}`;
  if (key === trayKey) return;
  trayKey = key;
  tauriInvoke('set_tray_status', { online, port: online ? port : null }).catch(() => {});
}

function renderChrome(s) {
  syncTray(s);

  const key = [
    s.conn, s.mode,
    s.daemon && s.daemon.fingerprint, s.daemon && s.daemon.control_port,
    s.endpoint && s.endpoint.port,
    s.connError && s.connError.kind, s.connError && s.connError.message,
  ].join('|');
  if (key === chromeKey) return;
  chromeKey = key;

  const badge = document.getElementById('daemon-badge');
  const stateCls = s.conn === 'online' ? 'online'
    : (s.conn === 'connecting' || s.conn === 'starting') ? 'connecting' : 'offline';
  badge.className = `daemon-badge ${stateCls}`;
  badge.innerHTML = '';
  badge.append(
    el('span', { class: `dot ${stateCls}` }),
    el('span', { class: 'badge-status' },
      s.conn === 'online' ? '在线' : s.conn === 'starting' ? '启动中' : s.conn === 'connecting' ? '连接中' : '离线'));
  if (s.daemon) {
    badge.append(
      el('span', { class: 'badge-sep' }, '·'),
      el('code', { class: 'badge-fp', title: s.daemon.fingerprint }, s.daemon.fingerprint.slice(0, 8)),
      el('span', { class: 'badge-port' }, `:${s.daemon.control_port}`));
  }

  // 「浏览器模式」是测试挂钩的自我说明，只在浏览器里出现，绝不进入 App 的文案。
  const foot = document.getElementById('conn-hint');
  if (s.mode === 'tauri') {
    foot.textContent = s.conn === 'online'
      ? `AudioHub · 已连接${s.endpoint ? ` · 端口 ${s.endpoint.port}` : ''}`
      : s.conn === 'starting' ? 'AudioHub · 正在启动服务…'
        : s.conn === 'connecting' ? 'AudioHub · 正在连接…' : 'AudioHub · 服务未连接';
  } else {
    foot.textContent = '浏览器模式' + (s.endpoint ? ` · IPC 端口 ${s.endpoint.port}` : '');
  }

  const ov = document.getElementById('overlay');
  const ico = document.getElementById('overlay-ico');
  const wave = document.getElementById('overlay-wave');
  const title = document.getElementById('overlay-title');
  const desc = document.getElementById('overlay-desc');
  const hint = document.getElementById('overlay-hint');
  const actions = document.getElementById('overlay-actions');
  const retryBtn = document.getElementById('overlay-retry');

  if (s.conn === 'online') {
    ov.hidden = true;
    return;
  }
  ov.hidden = false;

  // 启动/连接是**进行态**，不是错误：给动画与进度语，不给错误图标和按钮。
  const busy = s.conn === 'starting' || s.conn === 'connecting';
  ico.hidden = busy;
  wave.hidden = !busy;
  actions.hidden = busy;
  retryBtn.disabled = busy;
  hint.textContent = '';

  if (s.conn === 'starting') {
    title.textContent = '正在启动 AudioHub 服务…';
    desc.textContent = '首次启动需要几秒，完成后会自动进入主面板。';
    return;
  }
  if (s.conn === 'connecting') {
    title.textContent = '正在连接 AudioHub 服务…';
    desc.textContent = s.endpoint
      ? `正在连接本机端口 ${s.endpoint.port} …`
      : '正在获取本机服务连接信息…';
    return;
  }

  const err = s.connError || {};
  if (err.kind === 'version') {
    // 「服务没起来」与「服务版本不兼容」是两回事：后者重启界面也没用，
    // 必须换一个版本匹配的 daemon。
    title.textContent = 'AudioHub 服务版本不兼容';
    desc.textContent = `${err.message}。本界面只能与 IPC 协议 v${IPC_VERSION} 的服务通信，`
      + '请把服务与界面更新到同一次构建。';
    hint.textContent = '提示：确认 audiohub 与本界面来自同一次构建。';
    return;
  }
  if (err.kind === 'no-endpoint') {
    title.textContent = '缺少连接参数';
    desc.textContent = '请以 ?port=<端口>&token=<令牌> 打开本页面。';
    hint.textContent = '浏览器模式无法启动服务：请在终端运行 audiohub daemon 后等待自动重连。';
    return;
  }
  const copy = FAILURE_COPY[err.kind];
  if (copy) {
    title.textContent = copy.title;
    desc.textContent = copy.desc + (err.detail ? `\n\n详细信息：${String(err.detail).trim()}` : '');
    hint.textContent = copy.hint;
    return;
  }
  title.textContent = 'AudioHub 服务已断开';
  desc.textContent = s.mode === 'tauri'
    ? `与本机服务的连接已断开（${err.message || '原因未知'}），每 5 秒自动重连。`
    : '与 daemon 的连接已断开，每 5 秒自动重试。';
}

// ---- 启动 ----

function boot() {
  const tauri = isTauri();
  store.update((s) => { s.mode = tauri ? 'tauri' : 'browser'; });
  document.body.classList.toggle('is-tauri', tauri);

  document.querySelectorAll('#nav .nav-item').forEach((b) => {
    b.addEventListener('click', () => navigate(b.dataset.view));
  });

  document.getElementById('overlay-retry').addEventListener('click', () => connectDaemon());

  window.addEventListener('focus', reprobeOnReturn);
  document.addEventListener('visibilitychange', reprobeOnReturn);

  store.subscribe(renderChrome);
  store.subscribe(syncGate);
  renderChrome(store.state);
  syncGate(store.state);
  mountView();
  connectDaemon();
}

boot();
