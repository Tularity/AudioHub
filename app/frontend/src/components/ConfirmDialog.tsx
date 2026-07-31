// 应用内确认框。刻意不用 window.confirm：Tauri 的 webview 对它的支持随平台变化，
// 而且原生弹窗没法带 data-testid、没法排版长文案——而这里最需要说清楚的恰恰是
// 「按下去之后系统里会发生什么」。
//
// 命令式 API（`await confirmDialog({...})`）保持不变：调用点在事件处理器里，
// 改成声明式会把「问一句再做」拆成两段状态机，得不偿失。

import { useEffect, useRef, useSyncExternalStore } from 'react';
import { t } from '../i18n';

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
  const okRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!live) return;
    okRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') { e.preventDefault(); done(false); }
    };
    document.addEventListener('keydown', onKey, true);
    return () => document.removeEventListener('keydown', onKey, true);
  }, [live]);

  if (!live) return null;
  const testid = live.testid || 'confirm';
  const lines = (Array.isArray(live.body) ? live.body : [live.body]).filter(Boolean) as string[];

  return (
    <div
      className="confirm-mask"
      data-testid={testid}
      // 点遮罩 = 取消。点卡片内部不能穿透过去（危险操作误关掉是小事，误确认才是大事）。
      onClick={(e) => { if (e.target === e.currentTarget) done(false); }}
    >
      <div className="confirm-card" role="alertdialog" aria-modal="true">
        <h2 className="confirm-title">{live.title}</h2>
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
