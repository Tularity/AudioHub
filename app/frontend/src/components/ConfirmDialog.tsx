// 应用内确认框。刻意不用 window.confirm：Tauri 的 webview 对它的支持随平台变化，
// 而且原生弹窗没法带 data-testid、没法排版长文案——而这里最需要说清楚的恰恰是
// 「按下去之后系统里会发生什么」。
//
// 命令式 API（`await confirmDialog({...})`）保持不变：调用点在事件处理器里，
// 改成声明式会把「问一句再做」拆成两段状态机，得不偿失。

import { useEffect, useId, useRef, useSyncExternalStore } from 'react';
import { t } from '../i18n';
import { isEscape } from '../lib/shortcuts';
import { FOCUSABLE_SELECTOR, trapIndex } from '../lib/sheet';
import { inertSiblings } from '../lib/modalInert';

export interface ConfirmOpts {
  title: string;
  body: string | (string | null | undefined)[];
  confirmText?: string;
  cancelText?: string;
  danger?: boolean;
  testid?: string;
}

interface Live extends ConfirmOpts { resolve: (v: boolean) => void }

let current: Live | null = null;
const listeners = new Set<() => void>();
const emit = () => { for (const fn of [...listeners]) fn(); };
const subscribe = (fn: () => void) => { listeners.add(fn); return () => listeners.delete(fn); };

/**
 * 此刻有没有确认框开着。
 *
 * 给二级菜单层（`components/Sheet.tsx`）问的：确认框可以开在 Sheet **之上**
 * （重置指纹、解除配对都是这个形状），而两者的 Esc 监听都在 capture 阶段、
 * Sheet 注册得更早。Sheet 不问这一句的话，一次 Esc 会把确认框和它底下那整面
 * 板子一起关掉，用户看到的是自己莫名其妙离开了刚才那一页。
 */
export function isConfirmOpen(): boolean {
  return current !== null;
}

export function confirmDialog(opts: ConfirmOpts): Promise<boolean> {
  if (current) return Promise.resolve(false); // 同时只允许一个，避免叠层
  return new Promise<boolean>((resolve) => {
    current = { ...opts, resolve };
    emit();
  });
}

function done(v: boolean): void {
  const c = current;
  if (!c) return;
  current = null;
  emit();
  c.resolve(v);
}

export function ConfirmHost() {
  const live = useSyncExternalStore(subscribe, () => current);
  const maskRef = useRef<HTMLDivElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const okRef = useRef<HTMLButtonElement>(null);
  const titleId = useId();

  useEffect(() => {
    if (!live) return;
    const mask = maskRef.current;
    const card = cardRef.current;
    const opener = document.activeElement as HTMLElement | null;
    // The recovery overlay is always mounted and has the highest modal
    // priority. Keep it out of this layer's background set so a daemon drop can
    // supersede the confirmation instead of revealing an inert recovery UI.
    const overlay = document.getElementById('overlay');
    const releaseBackground = mask
      ? inertSiblings(mask, overlay ? [overlay] : [])
      : () => undefined;
    const overlayVisible = () => overlay?.hidden === false;
    const focusables = (): HTMLElement[] => (card
      ? Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR))
      : []);
    const focusDialog = () => {
      if (overlayVisible()) return;
      (okRef.current ?? card)?.focus();
    };

    focusDialog();

    // A confirmation may be created by an async completion while the daemon
    // recovery overlay is already visible. Once recovery leaves, move focus
    // into the still-open confirmation; otherwise focus would return to an
    // inert background control. The timeout runs after Overlay's effect cleanup
    // has released its own inert lock.
    let focusTimer: number | undefined;
    const overlayObserver = overlay
      ? new MutationObserver(() => {
          if (overlayVisible()) return;
          window.clearTimeout(focusTimer);
          focusTimer = window.setTimeout(focusDialog, 0);
        })
      : null;
    overlayObserver?.observe(overlay!, { attributes: true, attributeFilter: ['hidden'] });

    const onKey = (e: KeyboardEvent) => {
      if (document.getElementById('overlay')?.hidden === false) return;
      if (isEscape(e)) {
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();
        done(false);
        return;
      }
      if (e.key !== 'Tab' || !card) return;
      const list = focusables();
      e.preventDefault();
      e.stopPropagation();
      e.stopImmediatePropagation();
      if (!list.length) {
        card.focus();
        return;
      }
      const idx = trapIndex(
        list.length,
        list.indexOf(document.activeElement as HTMLElement),
        e.shiftKey,
      );
      list[idx]!.focus();
    };
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      overlayObserver?.disconnect();
      window.clearTimeout(focusTimer);
      releaseBackground();
      // The trigger can disappear as a consequence of the confirmed action.
      // It can also still sit below the recovery overlay; focusing an inert
      // node would steal the screen reader cursor from that higher-priority UI.
      if (opener?.isConnected && !opener.closest('[inert]')) opener.focus();
    };
  }, [live]);

  if (!live) return null;
  const testid = live.testid || 'confirm';
  const lines = (Array.isArray(live.body) ? live.body : [live.body]).filter(Boolean) as string[];

  return (
    <div
      ref={maskRef}
      className="confirm-mask"
      data-testid={testid}
      // 点遮罩 = 取消。点卡片内部不能穿透过去（危险操作误关掉是小事，误确认才是大事）。
      onClick={(e) => { if (e.target === e.currentTarget) done(false); }}
    >
      <div
        ref={cardRef}
        className="confirm-card"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
      >
        <h2 className="confirm-title" id={titleId}>{live.title}</h2>
        {lines.map((line, i) => <p className="confirm-body" key={i}>{line}</p>)}
        <div className="confirm-actions">
          <button className="btn" type="button" data-testid={`${testid}-cancel`} onClick={() => done(false)}>
            {live.cancelText || t('common.cancel')}
          </button>
          <button
            ref={okRef}
            className={`btn ${live.danger ? 'danger' : 'primary'}`}
            type="button"
            data-testid={`${testid}-ok`}
            onClick={() => done(true)}
          >
            {live.confirmText || t('common.ok')}
          </button>
        </div>
      </div>
    </div>
  );
}
