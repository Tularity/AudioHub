// 在 Tauri 里绝不能让 <a href> 走默认行为：webview 会把**应用界面本身**导航到外站，
// 而这个窗口没有后退按钮，用户就回不来了。依次尝试 opener / shell 插件与
// window.open；一个都不可用时把地址复制到剪贴板并说明，绝不静默失败。

import { toast } from '../components/Toasts';
import { t } from '../i18n';

// 公开 wiki 的深链。**URL 不进语料**：它们不是文案，翻译一门语言不该有机会改
// 掉一个地址。语料里只有那句可点的话（`wiki.*`），指向哪里由这里决定。
//
// wiki 是英文的，与界面语种无关——这是项目的既定语言约束，不是遗漏。
const WIKI_BASE = 'https://github.com/Tularity/AudioHub/wiki';

export const WIKI = {
  home: WIKI_BASE,
  modes: `${WIKI_BASE}/Operating-Modes`,
  transport: `${WIKI_BASE}/Transport-Tiers`,
  quality: `${WIKI_BASE}/Audio-Quality`,
  latency: `${WIKI_BASE}/Latency`,
  volume: `${WIKI_BASE}/Volume`,
} as const;

export async function openExternal(url: string): Promise<boolean> {
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
