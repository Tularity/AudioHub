// 窗口拖拽。**不走 `-webkit-app-region: drag`**，显式调 startDragging。
//
// 为什么换掉：报过两次的现象是「刚启动能拖，点过任何别的元素、或拖过一次文字之后，
// 整个会话里就再也拖不动了」。曾经按「文字选中把拖拽区弄坏了」去修——给拖拽区加
// `-webkit-user-select: none`、给子元素加 `no-drag`——那套东西一直在 styles.css 里，
// 迁移到 React 时也原样保住了，**而现象照旧**。所以那个诊断是错的，或者至少不够：
// 问题出在 WKWebView 自己对 app-region 的处理上，不在我们的 CSS 上。
//
// 这里改成 Tauri 官方 `data-tauri-drag-region` 内部用的同一条路径：mousedown 时直接
// 让窗口进入拖拽循环。区别只是判据由我们自己写，因而能精确排除控件。
//
// 双击放大为什么仍然成立：macOS 的 `performWindowDragWithEvent:`（start_dragging 的
// 底层）在指针没有位移时不吞掉后续点击，第二次 mousedown 照常送达且 `detail === 2`。
// Tauri 自己的注入脚本正是靠这一点分流的，这里照抄，不自作聪明。

import { isTauri, tauriInvoke } from '../ipc/endpoint';
import { toast } from '../components/Toasts';
import { t } from '../i18n';

/**
 * 命中其中任何一个（或它们的后代）就不拖窗口。少一条，用户点那个控件时窗口会跟着
 * 手跑；多一条也只是少一块拖拽面积，代价不对称，所以宁可写全。
 */
const INTERACTIVE = [
  'button', 'a', 'input', 'select', 'textarea', 'label', 'code',
  '[role="button"]', '[role="slider"]', '[contenteditable]', '[data-no-drag]',
].join(',');

// 命令不通时只吼一次：拖不动的时候用户会反复试，每次弹一条 toast 只会更吵。
let reported = false;

async function call(cmd: 'start_window_drag' | 'toggle_window_zoom'): Promise<void> {
  try {
    await tauriInvoke(cmd);
  } catch (err) {
    // 静默失败在这里是最坏的结果——窗口拖不动而界面什么都不说，正是这个 bug
    // 之前难以定位的原因。宁可弹一条，把「壳没接上」变成看得见的事实。
    if (reported) return;
    reported = true;
    toast(t('chrome.dragFailed', { message: err instanceof Error ? err.message : String(err) }), 'error');
  }
}

/**
 * 挂在浮动头部（以及授权门自己那条把手）上的 mousedown。浏览器态直接返回，
 * 连模块导入的副作用都不会碰到 Tauri。
 */
export function chromeMouseDown(e: { button: number; detail: number; target: EventTarget | null }): void {
  if (!isTauri() || e.button !== 0) return;
  const el = e.target as Element | null;
  if (el && typeof el.closest === 'function' && el.closest(INTERACTIVE)) return;
  void call(e.detail >= 2 ? 'toggle_window_zoom' : 'start_window_drag');
}

// 关于「滚轮划过浮动头部那 64px 时不滚动」——这是**有意为之**，不是漏掉的。
//
// 试过把滚轮转发给 #view-root（root.scrollTop += e.deltaY），实测不成立：同一个
// wheel 事件里 e.deltaY 是 400，而浏览器自己原生滚动只走 200（devicePixelRatio
// 确认为 1，所以不是缩放问题——那是引擎内部的 wheel→scroll 系数）。照 deltaY 转发
// 出来的速度正好是原生的两倍，而这个系数既不公开也不保证跨引擎一致，Tauri 实际
// 跑的 WKWebView 很可能又是另一个值。一个滚太快的转发比没有转发更糟。
//
// 而且原生行为本来就站得住脚：Finder / Safari / 邮件的工具栏区域同样不把滚轮
// 透传给内容。顶部这 64px 还压着 scroll edge effect（内容在那里已经溶进背景），
// 本来就不是拿来读的地方。
