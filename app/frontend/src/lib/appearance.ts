// The rules behind the two appearance preferences in the top strip -- colour
// scheme and language -- with no DOM and no storage in sight. The half that
// touches `document` and `localStorage` is `appearanceHost.ts`, exactly the
// split `shortcuts.ts` / `shortcutHost.ts` already uses: rules stay pure so
// they can be tested under vitest's default `node` environment, without
// dragging jsdom into the dependency tree.
//
// Both preferences are tri-state in the same shape -- "follow the system", or
// one specific value -- so they share `parsePref`. That is the whole reason
// they live in one module rather than two: the plumbing is identical and would
// otherwise be written twice and drift once.

import type { IconName } from '../components/Icon';
import type { Locale } from '../i18n';
import { LOCALES, DEFAULT_LOCALE } from '../i18n';

// ------------------------------------------------------------------ storage keys

/**
 * Why `localStorage` and not the daemon's settings.
 *
 * These two are **viewer** preferences, not device configuration. In
 * web-access mode (`docs/plan.md` §7.5) one daemon can be open in several
 * browsers at once; a theme parked in the daemon would mean the person on the
 * laptop flips to light and the person on the tablet watches their own window
 * change under them. Per-origin storage gets that right for free.
 *
 * The second reason is the first frame. Anything held by the daemon is only
 * knowable after `boot()` has connected, so the app would have to paint *some*
 * theme before it learns which one -- a guaranteed flash of the wrong colours
 * on every launch. Reading `localStorage` synchronously before React mounts
 * has no such window.
 *
 * The cost avoided is real too: a settings-backed theme is a change to the IPC
 * contract (new fields, version bump, both ends redeployed) for something the
 * daemon has no use for.
 */
export const THEME_STORAGE_KEY = 'audiohub.theme';
export const LOCALE_STORAGE_KEY = 'audiohub.locale';

// ------------------------------------------------------------------ theme

/** What the user asked for. `system` defers to `prefers-color-scheme`. */
export type ThemePref = 'system' | 'light' | 'dark';
/** What actually gets painted. There is no `system` here -- it always resolves. */
export type Theme = 'light' | 'dark';

/** The three states, in the order they are *listed*. Not the cycle order. */
export const THEME_PREFS = ['system', 'light', 'dark'] as const;

/**
 * Next in the cycle -- which depends on what the OS is set to.
 *
 * # Why a cycle at all
 *
 * The language control next to this one is a menu, and this one is not. The
 * difference is that the theme has exactly three states, forever, and every one
 * of them is visible the instant it is selected. Three states you can see is
 * the case a cycle is *for*; there is nothing to choose between, only something
 * to step through. A menu here would mean two clicks and a popover covering the
 * thing you are trying to look at.
 *
 * # Why the order is not the fixed system → light → dark
 *
 * Because that order makes the first press do nothing you can see, on the state
 * every user starts in. Measured in the built app on a light-mode Mac: from
 * `system` (resolving to light), one press pins `light` -- identical pixels,
 * and the tooltip reads "click to switch to light" while already looking light.
 *
 * So the cycle visits the **contrasting** pinned value first:
 *
 *     light-mode Mac:  system → dark → light → system
 *     dark-mode Mac:   system → light → dark → system
 *
 * Same three states, same three presses to return, but the first two presses
 * always repaint the window. Only the last step -- back to `system` -- can be
 * visually inert, which is right: that step is "stop overriding", and it is the
 * one the icon reports most clearly.
 */
export function nextThemePref(cur: ThemePref, systemPrefersDark: boolean): ThemePref {
  const contrasting: Theme = systemPrefersDark ? 'light' : 'dark';
  const matching: Theme = systemPrefersDark ? 'dark' : 'light';
  if (cur === contrasting) return matching;
  if (cur === matching) return 'system';
  return contrasting;
}

/** Fold a preference plus the current OS setting into the theme to paint. */
export function resolveTheme(pref: ThemePref, systemPrefersDark: boolean): Theme {
  if (pref === 'light' || pref === 'dark') return pref;
  return systemPrefersDark ? 'dark' : 'light';
}

/**
 * A distinct glyph per state, which is the whole point of a cycling button:
 * with no label, the icon is the only thing saying where in the cycle you are.
 * `themeAuto` is the half-filled disc macOS uses for "Auto" appearance.
 */
export function themeIcon(pref: ThemePref): IconName {
  if (pref === 'light') return 'themeLight';
  if (pref === 'dark') return 'themeDark';
  return 'themeAuto';
}

// ------------------------------------------------------------------ locale

/** `system` follows `navigator.languages`; anything else pins one catalogue. */
export type LocalePref = 'system' | Locale;

/** Menu order. `system` first, for the same reason as the theme. */
export const LOCALE_PREFS: readonly LocalePref[] = ['system', ...LOCALES];

/**
 * Which catalogue a preference lands on.
 *
 * The `system` branch is written out properly even though there is exactly one
 * catalogue today and it therefore always returns `zh-CN`. The matching order
 * -- exact tag first, then primary subtag -- is the part that would otherwise
 * be reinvented, badly, on the day a second language lands.
 *
 * Subtag comparison rather than `startsWith` because `startsWith` only works in
 * one direction: a `zh-CN` catalogue does start with a `zh` system tag, but an
 * `en` catalogue does *not* start with `en-GB`, so a bare catalogue tag would
 * silently never match a regional system tag. Splitting both sides makes the
 * two cases the same case.
 *
 * A consequence worth stating: `zh-Hant` matches `zh-CN` on the primary subtag,
 * i.e. Traditional falls back to Simplified. That is a poor match but a
 * deliberate one -- it is where the default lands anyway, and it stops being
 * reachable the moment a `zh-Hant` catalogue exists, because the exact pass
 * runs first.
 */
export function resolveLocale(pref: LocalePref, systemLangs: readonly string[]): Locale {
  if (pref !== 'system') return LOCALES.includes(pref) ? pref : DEFAULT_LOCALE;
  for (const raw of systemLangs) {
    const tag = String(raw || '');
    const exact = LOCALES.find((l) => l.toLowerCase() === tag.toLowerCase());
    if (exact) return exact;
  }
  for (const raw of systemLangs) {
    const primary = String(raw || '').split('-')[0]?.toLowerCase();
    if (!primary) continue;
    const near = LOCALES.find((l) => l.split('-')[0]?.toLowerCase() === primary);
    if (near) return near;
  }
  return DEFAULT_LOCALE;
}

/**
 * Globe for "whatever the system says", translate glyph for "a language was
 * chosen". Deliberately not a flag or a CJK character: the pinned state means
 * *some* explicit language, and the icon should not need redrawing the day a
 * second one is added.
 */
export function localeIcon(pref: LocalePref): IconName {
  return pref === 'system' ? 'langAuto' : 'langPinned';
}

// ------------------------------------------------------------------ parsing

/**
 * Read a stored preference back.
 *
 * Storage is user-writable (devtools, a synced profile, a hand-edited file),
 * so anything not on the allow-list falls back to `system` rather than being
 * trusted. Note `raw` is `string | null` and the null case returns the
 * fallback -- it is *not* folded into an empty string first, because "never
 * chosen" and "chosen and cleared" would then be indistinguishable if this
 * ever grows a third meaning.
 */
export function parsePref<T extends string>(
  raw: string | null,
  allowed: readonly T[],
  fallback: T,
): T {
  if (raw == null) return fallback;
  return allowed.includes(raw as T) ? (raw as T) : fallback;
}

export function parseThemePref(raw: string | null): ThemePref {
  return parsePref(raw, THEME_PREFS, 'system');
}

export function parseLocalePref(raw: string | null): LocalePref {
  return parsePref(raw, LOCALE_PREFS, 'system');
}
