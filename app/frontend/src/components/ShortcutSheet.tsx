// The `⌘/` cheat sheet.
//
// It exists because this app's shortcuts are not discoverable anywhere else.
// The obvious home for "open settings, ⌘," would be a native menu item — Apple
// puts Settings in the application menu and macOS renders its key equivalent
// for free. The reason that is *not* what we do: a menu key equivalent is
// matched ahead of the webview, so the moment ⌘, belongs to a menu item the
// page can never see it, the recorder can never capture it, and the binding
// stops being one the user can change. Given a choice between discoverable and
// rebindable, this sheet buys back the first without giving up the second.
//
// It is rendered from the same table the dispatcher reads, so a shortcut cannot
// be listed here and be wrong.

import { useEffect } from 'react';
import { t } from '../i18n';
import {
  ACTION_LABEL, PLATFORM, SHORTCUT_ACTIONS,
  acceleratorTokens, aliasFor, isEscape, parseAccelerator,
} from '../lib/shortcuts';
import type { ShortcutActionId } from '../lib/shortcuts';

function Row({ action, accel }: { action: ShortcutActionId; accel: string | null }) {
  const chord = accel ? parseAccelerator(accel) : null;
  const alias = aliasFor(action, PLATFORM);
  const aliasChord = alias ? parseAccelerator(alias) : null;
  return (
    <div className="sheet-row" data-testid={`sheet-row-${action}`}>
      <span className="sheet-label">{t(ACTION_LABEL[action])}</span>
      <span className="sheet-keys">
        {chord
          ? acceleratorTokens(chord, PLATFORM, t).map((tok, i) => <kbd key={i} className="kbd">{tok}</kbd>)
          : <span className="sheet-unset">{t('shortcuts.sheet.unset')}</span>}
        {chord && aliasChord ? (
          <span className="sheet-alias">
            {acceleratorTokens(aliasChord, PLATFORM, t).map((tok, i) => <kbd key={i} className="kbd">{tok}</kbd>)}
          </span>
        ) : null}
      </span>
    </div>
  );
}

export function ShortcutSheet({
  bindings, onClose,
}: {
  bindings: Record<ShortcutActionId, string | null>;
  onClose: () => void;
}) {
  useEffect(() => {
    // Capture phase and its own listener rather than relying on the global
    // dispatcher: Escape is deliberately not in the shortcut table (dialogs own
    // it), so nothing else would close this.
    const onKey = (e: KeyboardEvent) => {
      if (isEscape(e)) { e.preventDefault(); e.stopPropagation(); onClose(); }
    };
    document.addEventListener('keydown', onKey, true);
    return () => document.removeEventListener('keydown', onKey, true);
  }, [onClose]);

  return (
    <div className="sheet-scrim" data-testid="shortcut-sheet" onClick={onClose}>
      <div
        className="sheet-card"
        role="dialog"
        aria-modal="true"
        aria-label={t('shortcuts.sheet.title')}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="sheet-title">{t('shortcuts.sheet.title')}</h2>
        {SHORTCUT_ACTIONS.map((a) => <Row key={a} action={a} accel={bindings[a]} />)}
        {/* Escape is real and always works, but it is not in the editable table
            (nothing may rebind it), so it is spelled out rather than derived. */}
        <div className="sheet-row">
          <span className="sheet-label">{t('shortcuts.sheet.esc')}</span>
          <span className="sheet-keys"><kbd className="kbd">Esc</kbd></span>
        </div>
        <p className="sheet-foot">{t('shortcuts.sheet.customize')}</p>
      </div>
    </div>
  );
}
