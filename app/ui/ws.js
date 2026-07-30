// daemon IPC WebSocket 客户端。契约：core/audiohub-ipc/src/lib.rs
//   首帧 {"auth":token} → {"ok":true,"daemon":DaemonInfo}
//   请求 {"id":n,"method":..,"params":..} → {"id":n,"ok":..,"result"/"error":..}
//   事件 {"event":"stats","data":Vec<SessionInfo>}

// 必须与 audiohub_ipc::IPC_VERSION 一致。
export const IPC_VERSION = 1;

const DEFAULT_TIMEOUT_MS = 10000;

// 认证握手超时：socket 已连上但回帧不来时必须主动失败，否则 UI 永远停在「连接中」。
const AUTH_TIMEOUT_MS = 5000;

// 方法级超时必须**严格大于** daemon 侧的最坏耗时，否则会话其实已经建立却报错，
// 开关随之与真实状态脱节。core/audiohubd/src/conn.rs 的常量：
//   peers.connect = CONNECT_TIMEOUT 5s + HANDSHAKE_TIMEOUT 10s            = 15s
//   session.open  = 上面 15s + OPEN_TIMEOUT 10s + SOURCE_ACK_TIMEOUT 5s   = 30s
// 改动那几个常量时，这里要跟着抬。
// daemon.request_permission 的「最坏耗时」是**人**：TCC 弹窗停在屏幕上多久，
// 这个请求就挂多久。按默认 10s 去卡它，等于用户还在读弹窗时界面就报了超时。
const METHOD_TIMEOUT_MS = {
  'peers.connect': 30000,
  'session.open': 45000,
  'daemon.request_permission': 180000,
};

export function methodTimeout(method) {
  return METHOD_TIMEOUT_MS[method] ?? DEFAULT_TIMEOUT_MS;
}

export class VersionMismatchError extends Error {
  constructor(actual) {
    super(`daemon 协议版本不匹配（期望 ${IPC_VERSION}，实际 ${actual}）`);
    this.name = 'VersionMismatchError';
    this.expected = IPC_VERSION;
    this.actual = actual;
  }
}

export class IpcClient {
  constructor() {
    this.ws = null;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.authed = false;
  }

  on(name, fn) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(fn);
    return () => this.listeners.get(name)?.delete(fn);
  }

  _emit(name, data) {
    for (const fn of [...(this.listeners.get(name) || [])]) {
      try { fn(data); } catch (e) { console.error(e); }
    }
  }

  get connected() {
    return this.authed && !!this.ws && this.ws.readyState === WebSocket.OPEN;
  }

  connect(port, token) {
    this.close();
    return new Promise((resolve, reject) => {
      let settled = false;
      let authTimer = null;
      let ws;

      const finish = (fn, arg) => {
        clearTimeout(authTimer);
        authTimer = null;
        if (settled) return;
        settled = true;
        fn(arg);
      };
      // 握手失败/超时一律关掉 socket：close 事件会把 conn 推向 offline 并触发重试。
      const fail = (err) => {
        finish(reject, err);
        try { ws.close(); } catch (_) { /* ignore */ }
      };

      try {
        ws = new WebSocket(`ws://127.0.0.1:${port}`);
      } catch (e) {
        reject(e);
        return;
      }
      this.ws = ws;

      authTimer = setTimeout(() => fail(new Error('daemon 认证握手超时')), AUTH_TIMEOUT_MS);

      ws.addEventListener('open', () => {
        try { ws.send(JSON.stringify({ auth: token })); } catch (_) { /* close 事件兜底 */ }
      });

      ws.addEventListener('message', (ev) => {
        if (ws.__abandoned) return;
        let msg;
        try { msg = JSON.parse(ev.data); } catch (_) { return; }
        if (!this.authed) {
          if (msg && msg.ok === true && msg.daemon) {
            const v = Number(msg.daemon.ipc_version);
            if (v !== IPC_VERSION) {
              fail(new VersionMismatchError(Number.isFinite(v) ? v : '未知'));
              return;
            }
            this.authed = true;
            finish(resolve, msg.daemon);
          } else {
            fail(new Error((msg && msg.error) || '认证失败'));
          }
          return;
        }
        if (msg && msg.id != null && this.pending.has(msg.id)) {
          const p = this.pending.get(msg.id);
          this.pending.delete(msg.id);
          clearTimeout(p.timer);
          if (msg.ok) p.resolve(msg.result);
          else p.reject(new Error(msg.error || '请求失败'));
          return;
        }
        if (msg && msg.event) this._emit('event:' + msg.event, msg.data);
      });

      ws.addEventListener('close', () => {
        if (ws.__abandoned) return;
        const wasAuthed = this.authed;
        this.authed = false;
        this._failAll('连接已断开');
        finish(reject, new Error('无法连接 daemon'));
        if (this.ws === ws) this.ws = null;
        this._emit('close', wasAuthed);
      });

      ws.addEventListener('error', () => { /* close 事件兜底 */ });
    });
  }

  request(method, params = {}, timeoutMs) {
    if (!this.connected) return Promise.reject(new Error('IPC 未连接'));
    const ms = Number.isFinite(timeoutMs) && timeoutMs > 0 ? timeoutMs : methodTimeout(method);
    const id = this.nextId++;
    const frame = JSON.stringify({ id, method, params });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`请求超时：${method}`));
      }, ms);
      this.pending.set(id, { resolve, reject, timer });
      try {
        this.ws.send(frame);
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e);
      }
    });
  }

  _failAll(reason) {
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pending.clear();
  }

  close() {
    const ws = this.ws;
    this.ws = null;
    this.authed = false;
    this._failAll('连接已关闭');
    if (ws) {
      ws.__abandoned = true;
      try { ws.close(); } catch (_) { /* ignore */ }
    }
  }
}
