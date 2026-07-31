// 在 Tauri 里绝不能让 <a href> 走默认行为：webview 会把**应用界面本身**导航到外站，
// 而这个窗口没有后退按钮，用户就回不来了。依次尝试 opener / shell 插件与
// window.open；一个都不可用时把地址复制到剪贴板并说明，绝不静默失败。

import { toast } from '../components/Toasts';
import { t } from '../i18n';

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
