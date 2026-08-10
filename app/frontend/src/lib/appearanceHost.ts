// The DOM half of the appearance preferences: storage, the `prefers-*` media
// listeners, the `<html>` attributes, and one subscription each so React can
// read them through `useSyncExternalStore`.
//
// Rules live in `appearance.ts` and stay pure. This file is where `document`,
// `window` and `localStorage` are allowed to appear -- same arrangement as
// `shortcuts.ts` / `shortcutHost.ts`, and for the same reason: the interesting
// decisions stay testable without a DOM.

import {
  LOCALE_STORAGE_KEY, THEME_STORAGE_KEY,
  parseLocalePref, parseThemePref, resolveLocale, resolveTheme,
} from './appearance';
import type { LocalePref, Theme, ThemePref } from './appearance';
import { setLocale } from '../i18n';

// ---------------------------------------------------------------- storage

/**
 * `localStorage` throws outright in a few real configurations (Safari private
 * mode, blocked storage on an embedded origin), and under vitest's `node`
 * environment there is no `window` at all. Neither is worth taking the app
 * down for; both degrade to "the default, not persisted".
 */
function read(key: string): string | null {
  try { return window.localStorage.getItem(key); } catch { return null; }
}

function write(key: string, value: string): void {
  try { window.localStorage.setItem(key, value); } catch { /* preference-only */ }
}

// ---------------------------------------------------------------- theme

let themePref: ThemePref | null = null;
let systemDark = true;
const themeListeners = new Set<() => void>();

/**
 * Cached snapshot for `useSyncExternalStore`, which compares by identity.
 * A string is compared by value so this is not the re-render trap that
 * `shortcutHost` documents, but keeping the read cheap costs nothing.
 */
function notifyTheme(): void {
  for (const fn of [...themeListeners]) fn();
}

function readSystemDark(): boolean {
  try {
    // Default to dark when the query is unavailable: dark is this app's base
    // theme (`styles.css` `:root`), so an unanswerable question resolves to the
    // state the stylesheet already assumes rather than flipping the window.
    if (!window.matchMedia) return true;
    return !window.matchMedia('(prefers-color-scheme: light)').matches;
  } catch {
    return true;
  }
}

export function getThemePref(): ThemePref {
  if (themePref == null) themePref = parseThemePref(read(THEME_STORAGE_KEY));
  return themePref;
}

/**
 * What the OS is set to, regardless of what the user pinned here.
 *
 * Distinct from `activeTheme()`: when the preference is pinned the two differ,
 * and the toggle's cycle order is decided by the *OS* value -- it is what makes
 * one of the two pinned states the contrasting one (see `nextThemePref`).
 */
export function systemPrefersDark(): boolean {
  return systemDark;
}

/** The theme actually painted right now, preference plus OS setting folded. */
export function activeTheme(): Theme {
  return resolveTheme(getThemePref(), systemDark);
}

/**
 * Write the resolved theme onto `<html>`.
 *
 * `data-theme` is only set for light. Absent means dark, which is what
 * `styles.css` `:root` already is -- so the pre-React first paint (see
 * `main.tsx`) and any frame where script has not run yet land on the base
 * theme instead of an unstyled flash.
 */
function paintTheme(): void {
  const root = document.documentElement;
  if (activeTheme() === 'light') root.dataset.theme = 'light';
  else delete root.dataset.theme;
}

export function setThemePref(next: ThemePref): void {
  themePref = next;
  write(THEME_STORAGE_KEY, next);
  paintTheme();
  notifyTheme();
}

export function subscribeTheme(fn: () => void): () => void {
  themeListeners.add(fn);
  return () => { themeListeners.delete(fn); };
}

// ---------------------------------------------------------------- locale

let localePref: LocalePref | null = null;
const localeListeners = new Set<() => void>();

function systemLangs(): readonly string[] {
  try {
    const nav = navigator;
    // `languages` is the ordered list the user actually configured; `language`
    // is only the first of it. Prefer the list, fall back to the single value
    // on engines that do not expose it.
    if (Array.isArray(nav.languages) && nav.languages.length) return nav.languages;
    return nav.language ? [nav.language] : [];
  } catch {
    return [];
  }
}

export function getLocalePref(): LocalePref {
  if (localePref == null) localePref = parseLocalePref(read(LOCALE_STORAGE_KEY));
  return localePref;
}

export function setLocalePref(next: LocalePref): void {
  localePref = next;
  write(LOCALE_STORAGE_KEY, next);
  setLocale(resolveLocale(next, systemLangs()));
  for (const fn of [...localeListeners]) fn();
}

export function subscribeLocale(fn: () => void): () => void {
  localeListeners.add(fn);
  return () => { localeListeners.delete(fn); };
}

// ---------------------------------------------------------------- boot

/**
 * Apply both preferences and start following the OS.
 *
 * Called from `main.tsx` *before* `createRoot`, so the first painted frame is
 * already the right theme and `<html lang>` is already right for font fallback
 * and line breaking.
 *
 * The `prefers-color-scheme` listener is installed unconditionally rather than
 * only while the preference is `system`: a listener that gets added and removed
 * as the preference changes is one more piece of state to get wrong, and
 * `paintTheme()` is a no-op when the preference is pinned. `notifyTheme()` runs
 * with it so the button's icon stays honest -- in `system` the glyph shown is
 * the *system* glyph, but its tooltip names the resolved theme.
 */
export function initAppearance(): void {
  systemDark = readSystemDark();
  paintTheme();
  setLocale(resolveLocale(getLocalePref(), systemLangs()));

  try {
    const mq = window.matchMedia?.('(prefers-color-scheme: light)');
    if (!mq) return;
    const onChange = () => {
      systemDark = !mq.matches;
      paintTheme();
      notifyTheme();
    };
    // `addEventListener` on MediaQueryList is the modern form; the deprecated
    // `addListener` is kept as a fallback because it is one line and this runs
    // before anything else can report the failure.
    if (typeof mq.addEventListener === 'function') mq.addEventListener('change', onChange);
    else mq.addListener?.(onChange);
  } catch { /* no media queries: stay on whatever readSystemDark() decided */ }
}
