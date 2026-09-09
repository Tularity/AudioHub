// 二级菜单（Sheet）的两条纯判定：**Esc 归谁**，**Tab 往哪跳**。
//
// 两条都从组件里抽出来，是因为它们各自对应一次真机事故，而事故的形状是「谁先注册
// 谁先吃到键」这种不可能在组件测试里稳定复现的东西——抽成纯函数才能钉住。

/** Hidden ancestors must not leave unreachable controls inside a modal's tab ring. */
export function isModalFocusable(element: HTMLElement): boolean {
  return !element.closest('[hidden], [inert]') && element.getClientRects().length > 0
    && getComputedStyle(element).visibility !== 'hidden';
}

/**
 * 这一层 Sheet 该不该吃掉这次 Escape。
 *
 * capture 阶段同一个节点上的监听器**按注册顺序**触发，而 Sheet 总是比它内部后开的
 * 东西先注册。于是不加判断的话：
 *
 *   1. **快捷键录制态**（`ShortcutRow` 在 `beginRecordingCapture` 之后才挂监听）——
 *      按 Esc 的本意是「取消这次录制」，实际会把整个快捷键面板一起关掉。
 *   2. **开在 Sheet 之上的确认框**（`ConfirmHost` 在 `current` 变真时才挂监听）——
 *      按 Esc 的本意是「取消这个危险动作」，实际会连确认框带 Sheet 一起关掉，
 *      而用户下一眼看到的是自己已经离开了那一页。
 *
 * 两种情况下 Esc 都**不属于 Sheet**：让位给里层，里层自己的监听器随后照常处理。
 */
export function sheetEscapeCloses(
  { recording, confirmOpen }: { recording: boolean; confirmOpen: boolean },
): boolean {
  return !recording && !confirmOpen;
}

/**
 * 焦点陷阱：在 `count` 个可聚焦元素里，从 `current` 出发按一次 Tab（`backwards`
 * 为 Shift+Tab）应当落到哪个下标。
 *
 * `current < 0` 表示焦点此刻不在陷阱内（点了遮罩、或被别处抢走）：这时 Tab 回到
 * 首项、Shift+Tab 回到末项，而不是原地不动——原地不动等于陷阱漏了，Tab 会走到被
 * 遮罩盖住的背景控件上去。
 */
export function trapIndex(count: number, current: number, backwards: boolean): number {
  if (count <= 0) return -1;
  if (current < 0 || current >= count) return backwards ? count - 1 : 0;
  return backwards
    ? (current - 1 + count) % count
    : (current + 1) % count;
}

/**
 * 陷阱里认哪些元素。
 *
 * `[hidden]` 与 `disabled` 必须排除：本项目大量用 `hidden` 收起整行（而不是不渲染），
 * 把它们算进来会让 Tab 停在一个看不见的地方，表现为「按 Tab 没反应」。
 */
export const FOCUSABLE_SELECTOR = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].map((s) => `${s}:not([hidden])`).join(', ');
