// 网页访问（plan §7.5）的读写面。**这是 App 自己的设置，不是 daemon 的**：
// 走 Tauri 命令读写 `<config>/webui.json`，不经过 IPC 的 settings.get/set——
// daemon 是音频与网络引擎，不该为「App 要不要开个网页端口」长一个字段。
//
// 因此在浏览器态（本身就是被这个服务服出来的那个页面）里这三个选项**改不了**：
// 没有 Tauri 桥就没有调用面。这不是缺陷而是唯一正确的形状——否则局域网上任何人
// 都能顺手把 local_only 关掉，或者把你自己正在用的入口关掉。

import { isTauri, tauriInvoke } from '../ipc/endpoint';

export interface WebUiStatus {
  enabled: boolean;
  port: number;
  /** **生效值**（服务端口径），不是配置文件里存的值。 */
  local_only: boolean;
  /**
   * 「仅允许本机」当前被锁死。判据由服务端下发（src-tauri/src/webui.rs 的
   * `FORCE_LOCAL_ONLY`），前端不自己写死——解锁那天只需要改一处。
   */
  local_only_locked: boolean;
  /** 端口真的在监听。`enabled && !running` 时 error 里是原因。 */
  running: boolean;
  url: string | null;
  lan_url: string | null;
  source: 'disk' | 'embedded' | null;
  root: string | null;
  error: string | null;
}

export type WebUiPatch = Partial<Pick<WebUiStatus, 'enabled' | 'port' | 'local_only'>>;

/** 低于 1024 需要 root，绑定必然失败；与 src-tauri/src/webui.rs 的 MIN_PORT 一致。 */
export const WEB_PORT_MIN = 1024;
export const WEB_PORT_MAX = 65535;

export function webPortValid(n: number): boolean {
  return Number.isInteger(n) && n >= WEB_PORT_MIN && n <= WEB_PORT_MAX;
}

export function getWebUiStatus(): Promise<WebUiStatus> {
  return tauriInvoke<WebUiStatus>('get_webui_status');
}

export function setWebUiSettings(patch: WebUiPatch): Promise<WebUiStatus> {
  // 单个对象参数：字段名按 serde 的 snake_case 原样过去，绕开 Tauri 对具名参数的
  // camelCase 约定，省得 `local_only` / `localOnly` 两头对不上还查不出来。
  return tauriInvoke<WebUiStatus>('set_webui_settings', { settings: patch });
}

const LOOPBACK = /^(?:127\.\d{1,3}\.\d{1,3}\.\d{1,3}|localhost|\[?::1\]?)$/i;

/** 当前页面是不是从回环地址打开的。 */
export function onLoopback(): boolean {
  return LOOPBACK.test(location.hostname);
}

/**
 * 浏览器态下的**推断**状态：拿不到真值，但有两件事由「你正看着这个页面」直接成立
 * ——服务开着，端口就是当前这个。而地址不是回环，就证明 local_only 已经关了。
 * 全部只读：推断值绝不回写。
 */
export function inferredStatus(): WebUiStatus {
  const port = Number(location.port) || (location.protocol === 'https:' ? 443 : 80);
  const loopback = onLoopback();
  return {
    enabled: true,
    port,
    local_only: loopback,
    // 浏览器态本来就一律只读，这里给 true 只是让「锁死」的呈现两态一致。
    local_only_locked: true,
    running: true,
    url: `${location.origin}/`,
    // 不是回环，那么"局域网地址"就是你此刻用的这一个——这里没有可猜的余地，
    // 也不该去问一个问不到的后端。
    lan_url: loopback ? null : `${location.origin}/`,
    source: null,
    root: null,
    error: null,
  };
}

export function webUiSupported(): boolean {
  return isTauri();
}
