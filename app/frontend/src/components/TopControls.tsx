// The icon cluster in the top strip: connection status, interface language,
// colour scheme.
//
// # Why three icons and no labels
//
// The strip has exactly one job besides window dragging, and it is not to hold
// text. The status badge used to read "● 在线" and expand a fingerprint on
// hover; the word carried nothing the dot did not already carry by colour, and
// it cost the width that two more controls now occupy. So the dot lost its
// label, and the two new controls never had one.
//
// That trade has a price -- an unlabelled icon has to say what it is some other
// way -- and it is paid in three places: a distinct glyph per state
// (`appearance.ts`), `aria-label` on every button, and `title` naming both the
// current state and, for the cycling one, what the next press does.
//
// # Where the cluster sits
//
// At whichever end of the window the operating system did *not* claim, which is
// the rule the whole strip already follows (`lib/platform.ts`): macOS puts its
// traffic lights leading, so the cluster is trailing; Windows draws its caption
// buttons trailing, so the cluster is leading. Two CSS properties differ
// between the platforms -- which edge, and the flex direction that keeps the
// status dot on the outside of the group either way. No second layout branch.

import { useCallback, useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';
import { RawIcon } from './Icon';
import { useStore } from '../state/store';
import { LOCALE_ENDONYM, t } from '../i18n';
import type { Locale, MsgKey } from '../i18n';
import {
  LOCALE_PREFS,
  localeIcon, nextThemePref, resolveLocale, themeIcon,
} from '../lib/appearance';
import type { LocalePref, ThemePref } from '../lib/appearance';
import {
  activeTheme, getLocalePref, getThemePref, systemPrefersDark,
  setLocalePref, setThemePref, subscribeLocale, subscribeTheme,
} from '../lib/appearanceHost';
import { originOfElement, revealSwap } from '../lib/reveal';

// ---------------------------------------------------------------- shared

/**
 * Click-away and Escape for a popover that opens on click.
 *
 * `pointerdown` rather than `click`: a `click` listener fires after the button
 * has already re-rendered, and on the press that closes a menu the browser
 * delivers the close *and* the button's own toggle, so the menu reopens
 * immediately. Listening on the down edge and checking containment is the
 * version that does not fight itself.
 */
function useDismiss(open: boolean, close: () => void) {
  const host = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const el = e.target;
      if (el instanceof Node && host.current?.contains(el)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('pointerdown', onDown, true);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('pointerdown', onDown, true);
      document.removeEventListener('keydown', onKey);
    };
  }, [open, close]);
  return host;
}

// ---------------------------------------------------------------- status

const STATUS_LABEL: Record<string, MsgKey> = {
  online: 'badge.online',
  starting: 'badge.starting',
  connecting: 'badge.connecting',
  offline: 'badge.offline',
};

/**
 * Connection status, reduced to the dot.
 *
 * The detail (fingerprint, control port, host name) moved from an inline
 * expansion to a floating panel, which is what makes room for it to be *more*
 * than the two values it used to show.
 *
 * It opens on hover and on focus. `:focus-within` is the keyboard path, and it
 * is a real path now rather than the dead rule the old badge would have had --
 * this is an actual `<button>` because it actually opens something. Note it is
 * *not* a click path on macOS: WebKit does not focus a button on click unless
 * Full Keyboard Access is on. The permanent, always-reachable copy of all
 * three values is the Settings › 本机身份 block (`docs/plan.md` §7.6), which
 * is what the touch and screen-reader story rests on; the values here also
 * stay in the DOM either way, so automation reads them regardless.
 */
function StatusControl() {
  const conn = useStore((s) => s.conn);
  const name = useStore((s) => s.daemon?.name ?? null);
  const fp = useStore((s) => s.daemon?.fingerprint ?? null);
  // `?? null` and never `?? 0`: port 0 is a real value elsewhere in this app
  // (it means "pick one"), so folding an absent port into it would print a
  // plausible lie instead of an em dash.
  const ctlPort = useStore((s) => s.daemon?.control_port ?? null);
  const panelId = useId();

  const cls = conn === 'online' ? 'online'
    : (conn === 'connecting' || conn === 'starting') ? 'connecting' : 'offline';
  const label = t(STATUS_LABEL[conn] ?? 'badge.offline');

  // `daemon-badge` / `daemon-badge-ident` are kept as testids even though the
  // element behind each has changed shape (`docs/spec-ui.md` §169 lists both).
  // The thing they identify -- "the connection indicator", "the identity values
  // inside it" -- is the same thing it always was, and renaming a stable hook
  // because its markup moved is how automation quietly stops asserting.
  return (
    <div className="chrome-ctl hoverable" id="daemon-badge" data-testid="daemon-badge">
      <button
        type="button"
        className={`chrome-btn status ${cls}`}
        data-testid="chrome-status"
        aria-label={`${t('chrome.status.title')}: ${label}`}
        aria-describedby={panelId}
        title={label}
      >
        <span className={`dot ${cls}`} aria-hidden="true" />
      </button>
      <div
        className="chrome-pop status-pop"
        id={panelId}
        data-testid="chrome-status-pop"
        data-no-drag=""
      >
        <div className="pop-head">
          <span className={`dot ${cls}`} aria-hidden="true" />
          <span className={`pop-status ${cls}`} data-testid="chrome-status-text">{label}</span>
        </div>
        <dl className="pop-kv" data-testid="daemon-badge-ident">
          <dt>{t('chrome.status.name')}</dt>
          <dd data-testid="chrome-status-name">{name || t('common.dash')}</dd>
          <dt>{t('chrome.status.fingerprint')}</dt>
          <dd>
            <code data-testid="chrome-status-fp" title={fp || undefined}>
              {fp ? fp.slice(0, 16) : t('common.dash')}
            </code>
          </dd>
          <dt>{t('chrome.status.port')}</dt>
          <dd data-testid="chrome-status-port">{ctlPort ?? t('common.dash')}</dd>
        </dl>
        <p className="pop-note">{t('chrome.status.more')}</p>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- language

function localeName(loc: Locale): string {
  return LOCALE_ENDONYM[loc] ?? loc;
}

function localePrefLabel(pref: LocalePref, systemLocale: Locale): string {
  if (pref !== 'system') return localeName(pref as Locale);
  return t('chrome.locale.systemAs', { name: localeName(systemLocale) });
}

/**
 * Language, as a menu rather than a cycle.
 *
 * The theme button next to it cycles on click; this one does not, and the
 * difference is not an inconsistency. A cycle is legible over two or three
 * states you can name from the glyph. The language list is open-ended -- it
 * grows by one every time a catalogue is added -- and cycling through it would
 * mean pressing an unlabelled button an unknown number of times while the whole
 * interface changes language under you, with no way back except to keep going.
 */
function LocaleControl() {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const host = useDismiss(open, close);
  const pref = useSyncExternalStore(subscribeLocale, getLocalePref);
  const menuId = useId();

  // Resolved only to *name* the follow-system option; the actual switch happens
  // in `setLocalePref`, which resolves against the live navigator list.
  const systemLocale = resolveLocale('system', typeof navigator === 'undefined' ? [] : navigator.languages ?? []);

  return (
    <div className="chrome-ctl" ref={host} data-testid="chrome-locale-ctl">
      <button
        type="button"
        className={`chrome-btn${open ? ' open' : ''}`}
        data-testid="chrome-locale"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={menuId}
        aria-label={t('chrome.locale.current', { name: localePrefLabel(pref, systemLocale) })}
        title={t('chrome.locale.current', { name: localePrefLabel(pref, systemLocale) })}
        onClick={() => setOpen((v) => !v)}
      >
        <RawIcon name={localeIcon(pref)} />
      </button>
      <div
        className={`chrome-pop menu-pop${open ? ' open' : ''}`}
        id={menuId}
        role="menu"
        aria-label={t('chrome.locale.title')}
        data-testid="chrome-locale-menu"
        data-no-drag=""
      >
        <p className="pop-title">{t('chrome.locale.title')}</p>
        {LOCALE_PREFS.map((p) => (
          <button
            key={p}
            type="button"
            role="menuitemradio"
            aria-checked={p === pref}
            className={`pop-item${p === pref ? ' on' : ''}`}
            data-testid={`chrome-locale-${p}`}
            onClick={() => { setLocalePref(p); close(); }}
          >
            <span className="pop-item-label">
              {p === 'system' ? t('chrome.locale.system') : localeName(p as Locale)}
            </span>
            {p === 'system'
              ? <span className="pop-item-sub">{localeName(systemLocale)}</span>
              : null}
          </button>
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- theme

const THEME_LABEL: Record<ThemePref, MsgKey> = {
  system: 'chrome.theme.system',
  light: 'chrome.theme.light',
  dark: 'chrome.theme.dark',
};

/** "跟随系统（当前深色）" for system, plain "浅色"/"深色" otherwise. */
export function themeLabel(pref: ThemePref, resolved: 'light' | 'dark'): string {
  if (pref !== 'system') return t(THEME_LABEL[pref]);
  return t('chrome.theme.systemAs', { resolved: t(THEME_LABEL[resolved]) });
}

/**
 * Colour scheme: one press, one step through system → light → dark → system.
 *
 * The press is also the origin of the circular reveal (`lib/reveal.ts`): the
 * new theme grows out of this button until it covers the window. Taking the
 * origin from the button's own rectangle rather than from the pointer event is
 * what makes the keyboard path animate identically -- Space on a focused button
 * produces a click with no useful coordinates, and a reveal that starts at
 * (0, 0) for keyboard users would be a different, worse animation.
 */
function ThemeControl() {
  const btn = useRef<HTMLButtonElement>(null);
  const pref = useSyncExternalStore(subscribeTheme, getThemePref);
  const resolved = useSyncExternalStore(subscribeTheme, activeTheme);
  // The cycle needs the *OS* setting, not the resolved theme: when the
  // preference is pinned they differ, and it is the OS setting that decides
  // which pinned value is the contrasting one. See `nextThemePref`.
  const next = nextThemePref(pref, systemPrefersDark());

  const onClick = () => {
    revealSwap(originOfElement(btn.current), () => setThemePref(next));
  };

  const title = t('chrome.theme.current', {
    now: themeLabel(pref, resolved),
    next: t(THEME_LABEL[next]),
  });

  return (
    <button
      ref={btn}
      type="button"
      className="chrome-btn"
      data-testid="chrome-theme"
      data-theme-pref={pref}
      aria-label={title}
      title={title}
      onClick={onClick}
    >
      <RawIcon name={themeIcon(pref)} />
    </button>
  );
}

// ---------------------------------------------------------------- cluster

export function ChromeControls() {
  return (
    <div id="chrome-controls" data-testid="chrome-controls">
      <StatusControl />
      <LocaleControl />
      <ThemeControl />
    </div>
  );
}
