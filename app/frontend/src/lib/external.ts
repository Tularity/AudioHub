// 外链：把地址交给**系统浏览器**。
//
// 在 Tauri 里绝不能让 <a href> 走默认行为：webview 会把**应用界面本身**导航到外站，
// 而这个窗口没有后退按钮，用户就回不来了。
//
// ⚠ 2026-08-10 之前这条路是断的：下面先试的是 `opener` / `shell` 两个插件，而
// app/src-tauri/Cargo.toml 里**一个都没有**——于是每一条链接都掉到 `window.open`
// （webview 拒绝），再掉到「已复制链接」。一条会变成剪贴板写入的链接比没有链接更糟，
// 而这一版界面已经把所有解释都交给了这些链接。现在第一顺位是 `open_external_url`
// 这个**应用命令**（app/src-tauri/src/main.rs），它不受插件 ACL 约束，因此不需要
// capabilities 文件；插件路径保留在后面，装了插件的构建仍然走得通。

import { toast } from '../components/Toasts';
import { t } from '../i18n';
import { tauriInvoke, isTauri } from '../ipc/endpoint';

// 公开 wiki 的深链。**URL 不进语料**：它们不是文案，翻译一门语言不该有机会改
// 掉一个地址。语料里只有那句可点的话（`wiki.*`，现在多半只作为 `?` 的无障碍
// 名称），指向哪里由这里决定。
//
// ⚠ 每一条都指向**具体章节**。界面上已经不再解释任何功能（用户 2026-08-10 裁定
// 「为了简化而简化」，见 docs/plan.md §3.1），所以一个落在页顶、要用户自己往下
// 找的链接等于没有把话说完。锚点由 GitHub 从标题生成，改标题就要改这里。
const WIKI_BASE = 'https://github.com/Tularity/AudioHub/wiki';

export const WIKI = {
  home: WIKI_BASE,

  // —— 运行模式
  modes: `${WIKI_BASE}/Operating-Modes`,
  modeB: `${WIKI_BASE}/Operating-Modes#consumer-mode-b--virtual-devices`,
  bridge: `${WIKI_BASE}/Operating-Modes#bridging-to-a-third-party-virtual-cable`,
  captureSource: `${WIKI_BASE}/Operating-Modes#consumer-mode-a--driverless`,

  // —— 连通方式
  transport: `${WIKI_BASE}/Transport-Tiers`,
  tunnel: `${WIKI_BASE}/Transport-Tiers#the-tunnel-address`,
  degraded: `${WIKI_BASE}/Transport-Tiers#what-degradation-costs`,

  // —— 音质与延迟
  qualityLadder: `${WIKI_BASE}/Audio-Quality#the-ladder`,
  latencyTarget: `${WIKI_BASE}/Latency#the-latency-setting-is-a-target-not-a-limit`,

  // —— 音量
  volumeModes: `${WIKI_BASE}/Volume#volume-in-each-mode`,

  // —— 平台
  permissions: `${WIKI_BASE}/Platform-Notes#permissions`,
  deviceNaming: `${WIKI_BASE}/Platform-Notes#device-naming`,

  // —— 发现与配对
  discovery: `${WIKI_BASE}/Discovery-and-Pairing`,
  announce: `${WIKI_BASE}/Discovery-and-Pairing#what-the-broadcast-contains`,
  fingerprint: `${WIKI_BASE}/Discovery-and-Pairing#fingerprints`,
  ports: `${WIKI_BASE}/Discovery-and-Pairing#ports`,
  unpair: `${WIKI_BASE}/Discovery-and-Pairing#unpairing`,

  // —— 共享协议（外部来源接入；仅共享模式）
  shareProtocols: `${WIKI_BASE}/Share-Protocols`,
  airplay: `${WIKI_BASE}/Share-Protocols#airplay`,

  // —— 网页访问
  web: `${WIKI_BASE}/Web-Access`,
  webLocalOnly: `${WIKI_BASE}/Web-Access#why-local-only-is-locked`,

  // —— 其余开关
  startup: `${WIKI_BASE}/Settings-Reference#startup-at-login`,
  shortcuts: `${WIKI_BASE}/Settings-Reference#keyboard-shortcuts`,
  deviceOptions: `${WIKI_BASE}/Settings-Reference#virtual-device-options`,
  paths: `${WIKI_BASE}/Settings-Reference#paths`,
} as const;

export async function openExternal(url: string): Promise<boolean> {
  // 应用命令优先。它只放行 http/https（见 main.rs），所以这里不必再筛一遍，
  // 但也因此**不能**用它来开 `x-apple.systempreferences:` 这类系统设置深链——
  // 那条路走下面的插件/兜底，与本函数的其它调用点共用同一套失败提示。
  if (isTauri() && /^https?:\/\//i.test(url)) {
    try {
      await tauriInvoke('open_external_url', { url });
      return true;
    } catch { /* 换下一种 */ }
  }
  const tauri = window.__TAURI__ || {};
  for (const mod of [tauri.opener, tauri.shell]) {
    const fn = mod && ((mod.openUrl || mod.open) as ((u: string) => Promise<void>) | undefined);
    if (typeof fn !== 'function') continue;
    try {
      await fn.call(mod, url);
      return true;
    } catch { /* 换下一种 */ }
  }
  try {
    if (window.open(url, '_blank', 'noopener,noreferrer')) return true;
  } catch { /* 继续兜底 */ }
  try {
    await navigator.clipboard.writeText(url);
    toast(t('link.copied'), 'info');
  } catch {
    toast(t('link.openManually', { url }), 'warn');
  }
  return false;
}
