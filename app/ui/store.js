// 全局状态 + 极简 pub/sub；window.__AH_STATE__ 暴露只读快照（测试断言用，脱敏）。

const SETTINGS_KEY = 'audiohub.ui.settings';

function loadSettings() {
  // consumerMode 缺省是 'a'（plan §7.1）：模式 B 依赖驱动，而驱动可能被卸载、
  // 或换台机器就没有——存过的 'b' 因此不能被当作「现在能用 B」，settings 视图
  // 每次都按 daemon 上报的驱动状态重新判定可用性。
  const dft = { latency: 'lowest', quality: 'auto', removeVirtual: false, consumerMode: 'a' };
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (raw) return { ...dft, ...JSON.parse(raw) };
  } catch (_) { /* ignore */ }
  return dft;
}

const state = {
  conn: 'connecting',            // connecting | starting | online | offline
  mode: 'browser',               // tauri | browser
  endpoint: null,                // {port, token}
  connError: null,               // {kind, message, detail}；kind 见 app.js FAILURE_COPY
  daemon: null,                  // DaemonInfo
  lastStatusAt: 0,
  ipcRttMs: null,
  peers: [],                     // PeerState[]
  sessions: [],                  // SessionInfo[]
  history: {},                   // sessionId -> {loss[],jitter[],bitrate[],rung[]}，各 ≤60 点
  addrHistory: {},               // fingerprint -> [{addr, seenAt}]（本 UI 会话内累积）
  pairing: null,                 // {pin, ttlS, expiresAt}
  discover: { running: false, results: [] },
  monitorPref: {},               // fingerprint -> bool
  bridgePref: {},                // fingerprint -> 虚拟声卡设备名（'' = 不桥接）
  // 系统权限（permissions.js 归一化后的形状）。**全部只活在内存里**：授权门要在每次
  // 启动时重新探测，落盘任何「已看过」标记都会在用户撤销授权后把门永久藏起来。
  permissions: {
    probed: false,               // 拿到过一次权威回包（用来区分「还没问」和「问了没有」）
    supported: null,             // null=未知；false=daemon 没有这个方法（版本较旧）
    list: [],                    // PermissionState[]
    error: null,                 // 最近一次查询失败原因；仅供诊断，绝不据此挡人
    dismissed: false,            // 本次会话已进入主界面（含跳过）
    skipped: false,              // dismissed 的原因是「跳过」——权限并没齐
    busy: null,                  // 正在请求的权限 id（'*' = 全部授权进行中）
  },
  settings: loadSettings(),
  route: { view: 'peers', peerFp: null },
};

const listeners = new Set();
let scheduled = false;
let revision = 0;

function push60(arr, v) {
  arr.push(v);
  if (arr.length > 60) arr.splice(0, arr.length - 60);
}

function num(v) {
  return typeof v === 'number' && isFinite(v) ? v : 0;
}

export const store = {
  state,

  subscribe(fn) {
    listeners.add(fn);
    return () => listeners.delete(fn);
  },

  emit() {
    revision++;
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      for (const fn of [...listeners]) {
        try { fn(state); } catch (e) { console.error(e); }
      }
    });
  },

  update(fn) {
    fn(state);
    this.emit();
  },

  setPeers(list) {
    const authoritative = Array.isArray(list);
    this.update((s) => {
      s.peers = authoritative ? list : [];
      // 按指纹存的旁路状态跟着对端一起消失。不清就是无限增长，而且同一指纹重新
      // 配对时，上一次的桥接目标/监听开关会悄悄套回去。只在拿到权威列表（真的是
      // 一个数组）时修剪：peers.list 回来一坨垃圾不该顺手把用户偏好抹掉。
      if (authoritative) {
        const alive = new Set(s.peers.map((p) => p && p.fingerprint).filter(Boolean));
        for (const map of [s.monitorPref, s.bridgePref, s.addrHistory]) {
          for (const k of Object.keys(map)) {
            if (!alive.has(k)) delete map[k];
          }
        }
      }
      for (const p of s.peers) {
        if (!p || !p.fingerprint || !p.last_addr) continue;
        const arr = (s.addrHistory[p.fingerprint] ??= []);
        const hit = arr.find((e) => e.addr === p.last_addr);
        if (hit) hit.seenAt = Date.now();
        else arr.push({ addr: p.last_addr, seenAt: Date.now() });
      }
    });
  },

  upsertSession(info) {
    if (!info || info.id == null) return;
    this.update((s) => {
      const i = s.sessions.findIndex((x) => x.id === info.id);
      if (i >= 0) s.sessions[i] = info;
      else s.sessions.push(info);
    });
  },

  removeSession(id) {
    this.update((s) => {
      s.sessions = s.sessions.filter((x) => x.id !== id);
      delete s.history[id];
    });
  },

  // "stats" 事件（Vec<SessionInfo>，1s 一帧）→ 会话全量替换 + 各指标推 1 点
  pushStats(list) {
    this.update((s) => {
      s.sessions = Array.isArray(list) ? list : [];
      const alive = new Set();
      for (const info of s.sessions) {
        alive.add(String(info.id));
        const h = (s.history[info.id] ??= { loss: [], jitter: [], bitrate: [], rung: [] });
        const st = info.stats || {};
        push60(h.loss, num(st.loss_pct));
        push60(h.jitter, num(st.jitter_ms));
        push60(h.bitrate, num(st.bitrate_kbps));
        push60(h.rung, num(st.rung));
      }
      for (const id of Object.keys(s.history)) {
        if (!alive.has(id)) delete s.history[id];
      }
    });
  },

  setPermissions(list) {
    this.update((s) => {
      s.permissions.list = Array.isArray(list) ? list : [];
      s.permissions.probed = true;
      s.permissions.supported = true;
      s.permissions.error = null;
    });
  },

  // 查不到 ≠ 没授权。探测失败只记下来，列表保持原样（可能是上一轮拿到的权威结果），
  // 由 gateNeeded 决定是否还要挡人——绝不能因为一次 RPC 超时就把用户关在门外。
  setPermissionsError(message, supported) {
    this.update((s) => {
      s.permissions.error = message ? String(message) : null;
      if (supported === false) {
        s.permissions.supported = false;
        s.permissions.list = [];
      }
      s.permissions.probed = true;
    });
  },

  setPermissionBusy(id) {
    this.update((s) => { s.permissions.busy = id || null; });
  },

  dismissGate(skipped) {
    this.update((s) => {
      s.permissions.dismissed = true;
      s.permissions.skipped = !!skipped;
    });
  },

  saveSettings() {
    try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(state.settings)); } catch (_) { /* ignore */ }
  },
};

// 只读快照：IPC bearer token 绝不出现在快照里（任何页面脚本/扩展都能读到 window），
// 且按 revision 缓存——1s 一帧的 stats 下深拷贝整个 store 太贵。
let snapCache = { rev: -1, snap: null };

function snapshot() {
  if (snapCache.rev === revision) return snapCache.snap;
  const snap = JSON.parse(JSON.stringify(state));
  if (snap.endpoint && 'token' in snap.endpoint) snap.endpoint.token = '<redacted>';
  snapCache = { rev: revision, snap };
  return snap;
}

Object.defineProperty(window, '__AH_STATE__', {
  configurable: true,
  get: snapshot,
});
