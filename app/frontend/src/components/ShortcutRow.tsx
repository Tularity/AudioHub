// One editable shortcut. The capsule is the control: click it, press the
// combination, done.
//
// Shape of the interaction follows VS Code's "Define Keybinding" widget (it
// listens for the press rather than asking you to type a name) with two
// deliberate departures:
//
//  * **No confirming keystroke.** VS Code needs Enter because its widget sits
//    inside a text editor. This is a standalone control, so the first
//    non-modifier key commits.
//  * **The displaced action is named before it is displaced.** Taking a chord
//    from another row asks first and says whose it was. Silently reassigning is
//    how a user loses a binding without ever learning that they did.
//
// The failure mode that gets explicit handling is the one with no feedback at
// all: ⌘Q/⌘W/⌘M/⌘H are matched by the macOS menu ahead of the webview, so no
// keydown arrives and the capsule would just sit there looking broken. A short
// silence timer turns that into a sentence.

import { useEffect, useRef, useState } from 'react';
import { RawIcon } from './Icon';
import { t } from '../i18n';
import { beginRecordingCapture, endRecordingCapture } from '../lib/shortcutHost';
import {
  ACTION_LABEL, PLATFORM,
  acceleratorTokens, aliasFor, chordFromEvent, formatAccelerator, isEscape, parseAccelerator,
  verdictFor,
} from '../lib/shortcuts';
import type { AssignVerdict, Chord, ShortcutActionId } from '../lib/shortcuts';

/** How long a recording capsule may stay silent before it explains itself. */
const SILENCE_HINT_MS = 1200;

type Pending = { accel: string; verdict: AssignVerdict };

function Keys({ chord, partial }: { chord: Chord; partial?: boolean }) {
  const tokens = acceleratorTokens(chord, PLATFORM, t);
  // While only modifiers are held there is no key yet; the trailing ellipsis
  // stands in for it so the row does not jump width on commit.
  const shown = partial ? tokens.slice(0, -1) : tokens;
  return (
    <>
      {shown.map((tok, i) => <kbd key={`${tok}-${i}`} className="kbd">{tok}</kbd>)}
      {partial ? <kbd className="kbd kbd-pending">{t('shortcuts.record.more')}</kbd> : null}
    </>
  );
}

export function ShortcutRow({
  action, accel, customized, bindings, onCommit, onReset,
}: {
  action: ShortcutActionId;
  accel: string | null;
  customized: boolean;
  bindings: Record<ShortcutActionId, string | null>;
  onCommit: (accel: string | null) => void;
  onReset: () => void;
}) {
  const [recording, setRecording] = useState(false);
  const [live, setLive] = useState<Chord | null>(null);
  const [error, setError] = useState<string>('');
  const [silent, setSilent] = useState(false);
  const [pending, setPending] = useState<Pending | null>(null);
  const btnRef = useRef<HTMLButtonElement | null>(null);
  // Identity for the keyboard claim: one per row instance, stable for its life.
  const token = useRef({}).current;

  const label = t(ACTION_LABEL[action]);
  const chord = accel ? parseAccelerator(accel) : null;
  const alias = aliasFor(action, PLATFORM);
  const aliasChord = alias ? parseAccelerator(alias) : null;

  function stop() {
    setRecording(false);
    setLive(null);
    setSilent(false);
  }

  useEffect(() => {
    // Nothing to do when idle -- in particular, **do not** release the keyboard
    // claim here. `SettingsView` subscribes to the whole store, so a status poll
    // re-renders all six rows; a row that reset shared state just because it is
    // not the recording one would revoke its sibling's claim on every poll.
    // (Measured: with a row listening, ⌘1 navigated instead of being recorded.)
    if (!recording) return;
    beginRecordingCapture(token);
    const timer = window.setTimeout(() => setSilent(true), SILENCE_HINT_MS);

    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      window.clearTimeout(timer);
      setSilent(false);
      // `code`, not `key`: see `isEscape` -- a synthesised press can arrive with
      // the right physical key and the wrong produced value.
      if (isEscape(e)) { stop(); return; }
      if (e.code === 'Backspace' || e.code === 'Delete') { stop(); onCommit(null); return; }

      const c = chordFromEvent(e);
      if (!c) {
        // A modifier on its own: echo what is held so the control visibly listens.
        setLive({ meta: e.metaKey, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, key: '' });
        return;
      }
      const next = formatAccelerator(c);
      const verdict = verdictFor(action, next, bindings, PLATFORM);
      if (verdict.status === 'reject') {
        setLive(c);
        setError(verdict.reason === 'system'
          ? t('shortcuts.err.system', { accel: acceleratorTokens(c, PLATFORM, t).join(' ') })
          : t('shortcuts.err.noModifier'));
        return;
      }
      setError('');
      if (verdict.reason === 'taken') { stop(); setPending({ accel: next, verdict }); return; }
      stop();
      onCommit(next);
      if (verdict.reason === 'webview') {
        setError(t('shortcuts.warn.webview', { accel: acceleratorTokens(c, PLATFORM, t).join(' ') }));
      }
    };
    const onKeyUp = (e: KeyboardEvent) => {
      // Releasing a modifier while nothing else is down rewinds the echo.
      if (!e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) setLive(null);
    };
    const onPointerDown = (e: MouseEvent) => {
      if (btnRef.current && e.target instanceof Node && btnRef.current.contains(e.target)) return;
      stop();
    };

    document.addEventListener('keydown', onKeyDown, true);
    document.addEventListener('keyup', onKeyUp, true);
    document.addEventListener('mousedown', onPointerDown, true);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener('keydown', onKeyDown, true);
      document.removeEventListener('keyup', onKeyUp, true);
      document.removeEventListener('mousedown', onPointerDown, true);
      // Releasing our own claim only; a sibling that started recording in the
      // meantime keeps the keyboard. Doing it in the cleanup (rather than in
      // `stop`) also covers unmounting mid-record.
      endRecordingCapture(token);
    };
    // `bindings` is read inside the handler; re-arming on change keeps conflict
    // detection honest when another row is edited while this one is open.
  }, [recording, action, bindings, onCommit]);

  const ariaLabel = accel && chord
    ? t('shortcuts.edit.aria', { action: label, accel: acceleratorTokens(chord, PLATFORM, t).join(' ') })
    : t('shortcuts.edit.ariaUnset', { action: label });

  return (
    <div className="sc-row" data-testid={`shortcut-row-${action}`}>
      <span className="sc-label">{label}</span>

      <div className="sc-slot">
        <button
          ref={btnRef}
          type="button"
          className={`sc-capsule${recording ? ' recording' : ''}${accel ? '' : ' unset'}`}
          data-testid={`shortcut-capsule-${action}`}
          data-recording={recording ? 'on' : undefined}
          aria-label={ariaLabel}
          onClick={() => { setError(''); setPending(null); setRecording((v) => !v); }}
          onBlur={() => { if (recording) stop(); }}
        >
          {recording
            ? (live ? <Keys chord={live} partial={!live.key} /> : <span className="sc-prompt">{t('shortcuts.record.prompt')}</span>)
            : chord ? <Keys chord={chord} />
              : <span className="sc-unset">{t('shortcuts.unset')}</span>}
        </button>

        <button
          type="button"
          className="sc-mini"
          data-testid={`shortcut-reset-${action}`}
          hidden={!customized}
          title={t('shortcuts.reset')}
          aria-label={t('shortcuts.resetOne', { action: label })}
          onClick={() => { setError(''); setPending(null); onReset(); }}
        >
          <RawIcon name="refresh" />
        </button>
        <button
          type="button"
          className="sc-mini"
          data-testid={`shortcut-clear-${action}`}
          hidden={!accel}
          title={t('shortcuts.clear')}
          aria-label={t('shortcuts.clearOne', { action: label })}
          onClick={() => { setError(''); setPending(null); onCommit(null); }}
        >
          <RawIcon name="close" />
        </button>
      </div>

      {/* One live region per row: the recorder's verdicts are the only thing a
          screen reader would otherwise miss, since the capsule text changes
          without focus moving. */}
      <p className="sc-note" aria-live="polite" data-testid={`shortcut-note-${action}`}>
        {recording && silent && PLATFORM === 'mac' ? t('shortcuts.record.silent') : ''}
        {recording && !silent && !error ? t('shortcuts.record.hint') : ''}
        {!recording && error ? <span className="sc-err">{error}</span> : null}
        {!recording && !error && aliasChord && accel
          ? <span className="sc-alias">{t('shortcuts.alias', { accel: acceleratorTokens(aliasChord, PLATFORM, t).join(' ') })}</span>
          : null}
      </p>

      {pending ? (
        <div className="sc-conflict" data-testid={`shortcut-conflict-${action}`}>
          <span>
            {t('shortcuts.conflict.desc', {
              accel: acceleratorTokens(parseAccelerator(pending.accel)!, PLATFORM, t).join(' '),
              action: t(ACTION_LABEL[pending.verdict.conflictsWith!]),
            })}
          </span>
          <button
            type="button"
            className="btn small primary"
            data-testid={`shortcut-replace-${action}`}
            onClick={() => { const p = pending; setPending(null); onCommit(p.accel); }}
          >
            {t('shortcuts.conflict.replace')}
          </button>
          <button
            type="button"
            className="btn small"
            data-testid={`shortcut-conflict-cancel-${action}`}
            onClick={() => setPending(null)}
          >
            {t('common.cancel')}
          </button>
        </div>
      ) : null}
    </div>
  );
}
