// 连接参数从哪来。三种形态都必须支持，且**互不依赖**：
//
//   1) Tauri 壳内   → invoke("get_ipc_endpoint") 读 <config>/ipc.json
//   2) URL 查询参数 → ?port=N&token=T（回归脚本与 Tauri 壳都用它）
//   3) 同源引导     → fetch('/ipc-endpoint')，daemon 自己在控制端口上服这份 UI，
//                     浏览器直接打开 http://127.0.0.1:47810/ 即可，URL 里不带令牌
//
// 顺序是 2 → 1 → 3，不是任务描述里的 1 → 2 → 3：显式给了 ?port&token 就该以它为准
// （回归脚本正是靠这条把浏览器指向一个**指定的** daemon；若 Tauri 优先，那条断言会
// 悄悄连到另一个实例上去）。三者都不成立时才报「缺少连接参数」。

import type { IpcEndpoint } from './types';

export type EndpointSource = 'tauri' | 'query' | 'origin';

export interface ResolvedEndpoint extends IpcEndpoint {
  source: EndpointSource;
}

type TauriGlobal = {
  core?: { invoke?: (cmd: string, args?: unknown) => Promise<unknown> };
  invoke?: (cmd: string, args?: unknown) => Promise<unknown>;
  opener?: Record<string, unknown>;
  shell?: Record<string, unknown>;
};

declare global {
  interface Window { __TAURI__?: TauriGlobal }
}

export function isTauri(): boolean {
  return !!window.__TAURI__;
}

export function tauriInvoke<T = unknown>(cmd: string, args?: unknown): Promise<T> {
  const t = window.__TAURI__ || {};
  const inv = (t.core && t.core.invoke) || t.invoke;
  if (!inv) return Promise.reject(new Error('非 Tauri 环境'));
  return inv(cmd, args) as Promise<T>;
}

function validPort(v: unknown): number | null {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 && n < 65536 ? n : null;
}

/**
 * 同源引导：daemon 在控制端口上把这份 UI 连同 `/ipc-endpoint` 一起服出去
 * （仅回环）。回包形状 `{"ipc_version":N,"port":P,"token":"..."}`。
 *
 * 拿不到（404 / 网络错误 / 形状不对）一律返回 null，让调用方回落到「缺少连接
 * 参数」那条既有错误路径——绝不抛出，也绝不半信半疑地拿一个残缺对象去连。
 */
async function fetchOriginEndpoint(): Promise<IpcEndpoint | null> {
  // file:// 下没有可用的同源 HTTP 根，fetch 只会抛一个没信息量的错。
  if (!/^https?:$/.test(location.protocol)) return null;
  try {
    const res = await fetch('/ipc-endpoint', {
      method: 'GET',
      headers: { accept: 'application/json' },
      cache: 'no-store',
    });
    if (!res.ok) return null;
    const body = (await res.json()) as Record<string, unknown> | null;
    if (!body || typeof body !== 'object') return null;
    const port = validPort(body.port);
    const token = typeof body.token === 'string' ? body.token : '';
    if (!port || !token) return null;
    return { port, token };
  } catch {
    return null;
  }
}

export async function resolveEndpoint(): Promise<ResolvedEndpoint | null> {
  const q = new URLSearchParams(location.search);
  const qPort = validPort(q.get('port'));
  const qToken = q.get('token');
  if (qPort && qToken) return { port: qPort, token: qToken, source: 'query' };

  if (isTauri()) {
    const ep = await tauriInvoke<IpcEndpoint | null>('get_ipc_endpoint').catch(() => null);
    const port = ep ? validPort(ep.port) : null;
    if (ep && port && ep.token) return { port, token: ep.token, source: 'tauri' };
  }

  const origin = await fetchOriginEndpoint();
  if (origin) return { ...origin, source: 'origin' };

  return null;
}
