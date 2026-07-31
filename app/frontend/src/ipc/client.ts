// daemon IPC WebSocket 客户端。契约：core/audiohub-ipc/src/lib.rs
//   首帧 {"auth":token} → {"ok":true,"daemon":DaemonInfo}
//   请求 {"id":n,"method":..,"params":..} → {"id":n,"ok":..,"result"/"error":..}
//   事件 {"event":"stats","data":Vec<SessionInfo>}
//
// 迁移说明：这一层与 React 无关，逐字照搬自旧 app/ui/ws.js（仅补类型）。
// 它不该知道有没有框架——换 UI 框架时唯一不必重写的就是它。

import type { DaemonInfo } from './types';

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
const METHOD_TIMEOUT_MS: Record<string, number> = {
  'peers.connect': 30000,
  'session.open': 45000,
  'daemon.request_permission': 180000,
};

export function methodTimeout(method: string): number {
  return METHOD_TIMEOUT_MS[method] ?? DEFAULT_TIMEOUT_MS;
}

export class VersionMismatchError extends Error {
  expected: number;
  actual: number | string;

  constructor(actual: number | string) {
    super(`daemon 协议版本不匹配（期望 ${IPC_VERSION}，实际 ${actual}）`);
    this.name = 'VersionMismatchError';
    this.expected = IPC_VERSION;
    this.actual = actual;
  }
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

type Listener = (data: unknown) => void;

export class IpcClient {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private listeners = new Map<string, Set<Listener>>();
  private authed = false;

  on(name: string, fn: Listener): () => void {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name)!.add(fn);
    return () => { this.listeners.get(name)?.delete(fn); };
  }

  private emit(name: string, data: unknown): void {
    for (const fn of [...(this.listeners.get(name) || [])]) {
      try { fn(data); } catch (e) { console.error(e); }
    }
  }

  get connected(): boolean {
    return this.authed && !!this.ws && this.ws.readyState === WebSocket.OPEN;
  }

  connect(port: number, token: string): Promise<DaemonInfo> {
    this.close();
    return new Promise<DaemonInfo>((resolve, reject) => {
      let settled = false;
      let authTimer: ReturnType<typeof setTimeout> | null = null;
      let ws: WebSocket & { __abandoned?: boolean };

      const finish = <T>(fn: (arg: T) => void, arg: T) => {
        if (authTimer) clearTimeout(authTimer);
        authTimer = null;
        if (settled) return;
        settled = true;
        fn(arg);
      };
      // 握手失败/超时一律关掉 socket：close 事件会把 conn 推向 offline 并触发重试。
      const fail = (err: Error) => {
        finish(reject, err);
        try { ws.close(); } catch { /* ignore */ }
      };

      try {
        ws = new WebSocket(`ws://127.0.0.1:${port}`);
      } catch (e) {
        reject(e as Error);
        return;
      }
      this.ws = ws;

      authTimer = setTimeout(() => fail(new Error('daemon 认证握手超时')), AUTH_TIMEOUT_MS);

      ws.addEventListener('open', () => {
        try { ws.send(JSON.stringify({ auth: token })); } catch { /* close 事件兜底 */ }
      });

      ws.addEventListener('message', (ev: MessageEvent) => {
        if (ws.__abandoned) return;
        let msg: Record<string, unknown>;
        try { msg = JSON.parse(String(ev.data)); } catch { return; }
        if (!this.authed) {
          const daemon = msg && (msg.daemon as DaemonInfo | undefined);
          if (msg && msg.ok === true && daemon) {
            const v = Number(daemon.ipc_version);
            if (v !== IPC_VERSION) {
              fail(new VersionMismatchError(Number.isFinite(v) ? v : '未知'));
              return;
            }
            this.authed = true;
            finish(resolve, daemon);
          } else {
            fail(new Error(String((msg && msg.error) || '认证失败')));
          }
          return;
        }
        const id = msg && (msg.id as number | undefined);
        if (id != null && this.pending.has(id)) {
          const p = this.pending.get(id)!;
          this.pending.delete(id);
          clearTimeout(p.timer);
          if (msg.ok) p.resolve(msg.result);
          else p.reject(new Error(String(msg.error || '请求失败')));
          return;
        }
        if (msg && msg.event) this.emit('event:' + String(msg.event), msg.data);
      });

      ws.addEventListener('close', () => {
        if (ws.__abandoned) return;
        const wasAuthed = this.authed;
        this.authed = false;
        this.failAll('连接已断开');
        finish(reject, new Error('无法连接 daemon'));
        if (this.ws === ws) this.ws = null;
        this.emit('close', wasAuthed);
      });

      ws.addEventListener('error', () => { /* close 事件兜底 */ });
    });
  }

  request<T = unknown>(method: string, params: unknown = {}, timeoutMs?: number): Promise<T> {
    if (!this.connected) return Promise.reject(new Error('IPC 未连接'));
    const ms = Number.isFinite(timeoutMs) && (timeoutMs as number) > 0
      ? (timeoutMs as number)
      : methodTimeout(method);
    const id = this.nextId++;
    const frame = JSON.stringify({ id, method, params });
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`请求超时：${method}`));
      }, ms);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      try {
        this.ws!.send(frame);
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e as Error);
      }
    });
  }

  private failAll(reason: string): void {
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new Error(reason));
    }
    this.pending.clear();
  }

  close(): void {
    const ws = this.ws as (WebSocket & { __abandoned?: boolean }) | null;
    this.ws = null;
    this.authed = false;
    this.failAll('连接已关闭');
    if (ws) {
      ws.__abandoned = true;
      try { ws.close(); } catch { /* ignore */ }
    }
  }
}
