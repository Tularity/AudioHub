// 应用内的二级菜单层。用户 2026-08-10 的 22 条指令里七次要求「二级菜单」，
// 这里是它们**唯一**的落点。
//
// 为什么不是第六个视图：详情页本身已经是二级了（对端详情上的别名/危险操作都开在它
// 之上），再加一层路由就是三级；而路由层的返回语义只有 `nav.back → peers` 一条，
// 加两级会让它含义漂移。
//
// 这一层收敛了原先各写一遍的两套模态（ConfirmDialog / ShortcutSheet）在 Esc、遮罩、
// 焦点这三件事上的做法，并补上它们都没做的两件：
//
//   · **关闭时把焦点还给触发它的按钮**。原先两套都不还，键盘用户关掉面板后焦点落在
//     `<body>`，下一次 Tab 从页首重新开始。
//   · **Tab 焦点陷阱**。没有陷阱时 Tab 会走到被遮罩盖住的背景控件上——那些控件此刻
//     既看不见又点得到。
//
// ⚠ 开合**只由 React state 决定**。`styles.css` 里记着一次真机事故：曾用
// `:focus-within` 让浮层跟着焦点开合，结果 Esc 关掉的那一瞬间焦点回到触发按钮，
// `:focus-within` 立刻又把它打开，表现为「Esc 按不掉」。这里不出现任何 `:focus-*`
// 驱动的开合。

import { useEffect, useId, useRef } from 'react';
import type { ReactNode } from 'react';
import { t } from '../i18n';
import { isEscape } from '../lib/shortcuts';
import { isRecordingCapture } from '../lib/shortcutHost';
import { isConfirmOpen } from './ConfirmDialog';
import { sheetEscapeCloses, trapIndex, FOCUSABLE_SELECTOR } from '../lib/sheet';

export function Sheet({
  testid, title, help, children, footer, primaryAction, dismissLabel,
  dismissDisabled = false, onClose, auto = false, wide = false,
}: {
  testid: string;
  title: string;
  /** 标题右边那枚 `?`。与页面上的区块标题同一个形状。 */
  help?: ReactNode;
  children: ReactNode;
  /** 底部左侧的辅助动作（例如恢复默认 / 重新检查），不承担提交。 */
  footer?: ReactNode;
  /** 底部最右的菜单级主动作。编辑型 Sheet 的「保存」只应出现在这里。 */
  primaryAction?: ReactNode;
  /** 有未提交草稿时用「取消」，只读 / 即时动作面板仍用缺省的「关闭」。 */
  dismissLabel?: ReactNode;
  /** 提交进行中时同时挡住按钮、Esc 与遮罩，避免已发出的写入看起来像被取消。 */
  dismissDisabled?: boolean;
  onClose: () => void;
  /** 由 effect 自动弹出（而非用户点开）。只做标记，供回归断言用。 */
  auto?: boolean;
  /** 行内还挂着动作按钮的面板（权限）要更宽，否则状态标签会被挤到换行。 */
  wide?: boolean;
}) {
  const cardRef = useRef<HTMLDivElement | null>(null);
  const titleId = useId();

  // `onClose` 走 ref，**下面那个 effect 的依赖数组必须留空**。
  //
  // 这不是一次微优化，是一条正确性要求。effect 一旦把 `onClose` 列进依赖，调用方
  // 传一个内联箭头（`onClose={() => setOpen(false)}`；Peers.tsx 那两扇二级菜单的
  // `onClose` 就由父组件内联生成、一路透传下来）会让它
  // **每次重渲都重跑一遍**：cleanup 先把焦点还给「触发者」，effect 再把焦点抢回
  // 卡片里的第一个可聚焦元素。而「让对方找到我」正开着倒计时，`BeDiscoveredSheet`
  // 是 1 Hz 重渲的——表现为用户每按一次 Tab，一秒之内就被弹回标题旁的 `?`，
  // 「停止配对」按钮用键盘根本走不到。
  //
  // 修在这里而不是让七个调用方各自 `useCallback`：与「焦点归还」放在组件里而不是
  // 靠调用方传是同一条理由——靠约定去守，迟早有一个忘了守，而这一条忘了不报错，
  // 只是键盘用户用不了。
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const dismissDisabledRef = useRef(dismissDisabled);
  dismissDisabledRef.current = dismissDisabled;

  useEffect(() => {
    const card = cardRef.current;
    // 触发它的那个按钮。存在这里而不是靠调用方传，是因为**每一个** Sheet 都要还，
    // 靠约定去还等于迟早有一个忘了还。
    const opener = document.activeElement as HTMLElement | null;

    const focusables = (): HTMLElement[] => (card
      ? Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR))
      : []);

    // 打开时把焦点移进卡片。没有任何可聚焦元素时退到卡片本身（它带 tabIndex=-1），
    // 否则读屏会停在面板外面读背景内容。
    const first = focusables()[0];
    if (first) first.focus();
    else card?.focus();

    const onKey = (e: KeyboardEvent) => {
      // The daemon recovery gate is the global top-level modal. A Sheet can
      // remain mounted underneath it while a connection drops, but must not
      // consume Esc/Tab or move focus until recovery completes.
      if (document.getElementById('overlay')?.hidden === false) return;
      // A confirmation is the modal directly above this Sheet. Its mask makes
      // the Sheet inert, and its own handler owns both Escape and Tab. Yielding
      // here avoids the older Sheet trap briefly pulling focus behind the
      // confirmation before the later-registered Confirm handler corrects it.
      if (isConfirmOpen()) return;
      if (isEscape(e)) {
        // Esc 不一定归这一层——录制态与开在上面的确认框都比它更内层。判据是纯函数，
        // 有单测；这里只负责问。
        if (!sheetEscapeCloses({ recording: isRecordingCapture(), confirmOpen: isConfirmOpen() })) {
          return;
        }
        if (dismissDisabledRef.current) return;
        e.preventDefault();
        e.stopPropagation();
        closeRef.current();
        return;
      }
      if (e.key !== 'Tab' || !card) return;
      const list = focusables();
      if (!list.length) { e.preventDefault(); return; }
      const idx = trapIndex(list.length, list.indexOf(document.activeElement as HTMLElement), e.shiftKey);
      if (idx < 0) return;
      e.preventDefault();
      list[idx].focus();
    };

    // capture 阶段 + 自己的监听器，不走快捷键派发器：Escape 刻意不在快捷键表里
    // （对话框拥有它），所以没有别的东西会关掉这一层。
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      // 焦点归还。`isConnected` 是必要的：触发按钮可能随这次操作一起消失
      // （例如「重置指纹」之后那一行整个换了内容）。
      if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 见上：依赖数组必须留空。
  }, []);

  return (
    <div
      className="sheet-scrim"
      data-testid={testid}
      data-auto={auto ? 'on' : undefined}
      // 点遮罩关闭，**仅当点的就是遮罩本身**。卡片内部的点击不许穿透过来——
      // 误关是小事，误确认才是大事（与 ConfirmDialog 同一条判据）。
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !dismissDisabled) onClose();
      }}
    >
      <div
        className={`sheet-card${wide ? ' wide' : ''}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        ref={cardRef}
      >
        <div className="sheet-head">
          <h2 className="sheet-title" id={titleId}>{title}</h2>
          {help}
        </div>
        <div className="sheet-body">{children}</div>
        <div className="sheet-actions">
          {footer ? <div className="sheet-actions-leading">{footer}</div> : null}
          <button
            className="btn" type="button" data-testid={`${testid}-close`}
            disabled={dismissDisabled}
            onClick={onClose}
          >
            {dismissLabel ?? t('common.close')}
          </button>
          {primaryAction}
        </div>
      </div>
    </div>
  );
}
