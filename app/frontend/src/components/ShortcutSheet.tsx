// The shortcut panel: `⌘/` and 设置 › 杂项 › 快捷键 open **this same sheet**.
//
// It used to be two renderings of one table — a read-only cheat sheet here, an
// editor on the settings page — joined by a line of copy at the bottom of the
// cheat sheet pointing at the editor. The user's 2026-08-10 restructure merged
// them (instruction 7: shortcuts become a second-level menu). Merging retires
// that line of copy, one of the two testid sets, and the standing risk of the
// two views disagreeing about what a binding currently is.
//
// The cost of merging is that `⌘/` now opens something editable rather than
// something you glance at. It is bounded: `ShortcutRow` requires a **click into
// the capsule** before it records anything (focus alone does not arm it), and
// opening the sheet arms no row, so the bar for changing a binding by accident
// is exactly where it was.
//
// It exists at all because this app's shortcuts are not discoverable anywhere
// else. The obvious home for "open settings, ⌘," would be a native menu item —
// Apple puts Settings in the application menu and macOS renders its key
// equivalent for free. The reason that is *not* what we do: a menu key
// equivalent is matched ahead of the webview, so the moment ⌘, belongs to a
// menu item the page can never see it, the recorder can never capture it, and
// the binding stops being one the user can change. Given a choice between
// discoverable and rebindable, this sheet buys back the first without giving up
// the second.
//
// The list is rendered from the same table the dispatcher reads, so a shortcut
// cannot be listed here and be wrong.

import { useState, useSyncExternalStore } from 'react';
import { Help } from './Controls';
import { Sheet } from './Sheet';
import { ShortcutRow } from './ShortcutRow';
import { WIKI } from '../lib/external';
import { getOverrides, setOverrides, subscribeShortcuts } from '../lib/shortcutHost';
import {
  ACTION_LABEL, PLATFORM, SHORTCUT_ACTIONS,
  applyBinding, clearOverride, isCustomized, resolveBindings,
} from '../lib/shortcuts';
import type { ShortcutActionId, ShortcutOverrides } from '../lib/shortcuts';
import { t } from '../i18n';

// ---------------------------------------------------------------- 开合

// 模块级而不是 `App` 的 useState：两个入口（快捷键派发器、设置页杂项里那一行）都
// 要能开它，而它们不在同一棵子树上。`shortcutHost` 的 override 本来就住在模块级
// （纯 UI 偏好，不进 AppState），开合状态跟着它是同一条理由。
let open = false;
const listeners = new Set<() => void>();
const emit = () => { for (const fn of [...listeners]) fn(); };
const subscribe = (fn: () => void) => { listeners.add(fn); return () => listeners.delete(fn); };

export function openShortcutSheet(): void {
  if (open) return;
  open = true;
  emit();
}

export function closeShortcutSheet(): void {
  if (!open) return;
  open = false;
  emit();
}

/** `⌘/` 的语义是「开/合」，不是「开」。 */
export function toggleShortcutSheet(): void {
  open = !open;
  emit();
}

// ---------------------------------------------------------------- 宿主

export function ShortcutSheetHost() {
  const live = useSyncExternalStore(subscribe, () => open);
  return live ? <ShortcutSheet /> : null;
}

function ShortcutSheet() {
  // override 仍存在模块级 + localStorage，**不进 `AppState`**；Sheet 打开时只复制一份
  // 草稿。录制、逐项清除与「全部恢复」都先改草稿，底部保存才替换当前生效值。
  const committed = useSyncExternalStore(subscribeShortcuts, getOverrides);
  const [draft, setDraft] = useState<ShortcutOverrides>(() => ({ ...getOverrides() }));
  const bindings = resolveBindings(draft, PLATFORM);
  const has = (value: ShortcutOverrides, action: ShortcutActionId) => (
    Object.prototype.hasOwnProperty.call(value, action)
  );
  // 缺席 = 用默认，显式 null = 禁用；比较时必须连「有没有这个键」一起比较。
  const dirty = SHORTCUT_ACTIONS.some((action) => (
    has(committed, action) !== has(draft, action) || committed[action] !== draft[action]
  ));
  const hasDraftOverrides = SHORTCUT_ACTIONS.some((action) => has(draft, action));
  // 冲突提示要说出「被抢走绑定的是谁」，而那一行随即变成「未设置」——不高亮一下
  // 的话，用户看不到自己刚刚拿走了什么。
  const [freed, setFreed] = useState<ShortcutActionId | null>(null);

  function commit(action: ShortcutActionId, accel: string | null): void {
    const taken = accel
      ? SHORTCUT_ACTIONS.find((id) => id !== action && bindings[id] === accel) ?? null
      : null;
    setDraft(applyBinding(draft, action, accel, bindings));
    setFreed(taken);
  }

  return (
    <Sheet
      testid="shortcut-sheet"
      title={t('shortcuts.sheet.title')}
      help={<Help label={t('wiki.shortcuts')} url={WIKI.shortcuts} testid="settings-shortcuts-help" />}
      dismissLabel={t('common.cancel')}
      onClose={closeShortcutSheet}
      footer={(
        <button
          type="button"
          className="btn small"
          data-testid="shortcuts-reset-all"
          disabled={!hasDraftOverrides}
          onClick={() => {
            setDraft({});
            setFreed(null);
          }}
        >
          {t('settings.shortcuts.resetAll')}
        </button>
      )}
      primaryAction={(
        <button
          type="button"
          className="btn primary"
          data-testid="shortcuts-save"
          disabled={!dirty}
          onClick={() => {
            setOverrides(draft);
            closeShortcutSheet();
          }}
        >
          {t('common.save')}
        </button>
      )}
    >
      <div className="sc-list">
        {SHORTCUT_ACTIONS.map((action) => (
          <ShortcutRow
            key={action}
            action={action}
            accel={bindings[action]}
            customized={isCustomized(draft, action)}
            bindings={bindings}
            onCommit={(accel) => commit(action, accel)}
            onReset={() => { setDraft(clearOverride(draft, action)); setFreed(null); }}
          />
        ))}
      </div>

      {/* Escape is real and always works, but it is not in the editable table
          (nothing may rebind it), so it is spelled out rather than derived.
          It is also why this sheet must yield Escape to a row that is recording
          — see `sheetEscapeCloses` in lib/sheet.ts. */}
      <div className="sheet-row">
        <span className="sheet-label">{t('shortcuts.sheet.esc')}</span>
        <span className="sheet-keys"><kbd className="kbd">Esc</kbd></span>
      </div>

      <p className="sc-freed" aria-live="polite" data-testid="shortcuts-freed">
        {freed ? t('shortcuts.conflict.freed', { action: t(ACTION_LABEL[freed]) }) : ''}
      </p>
    </Sheet>
  );
}
