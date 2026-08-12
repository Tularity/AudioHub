// The appearance preferences, tested where the decisions actually are.
//
// Three of these guard against mistakes this codebase has a documented history
// of: an unlabelled control whose states share a glyph (nothing tells you where
// you are), a `null` folded into a value (`?? 0` on a port, twice), and a
// stored preference trusted without validation.

import { describe, it, expect } from 'vitest';
import {
  LOCALE_PREFS, THEME_PREFS,
  localeIcon, nextThemePref, parseLocalePref, parsePref, parseThemePref,
  resolveLocale, resolveTheme, themeIcon,
} from './appearance';
import type { ThemePref } from './appearance';
import { DEFAULT_LOCALE, LOCALES } from '../i18n';

describe('theme preference cycle', () => {
  /** Walk the cycle from `system` and record where each press lands. */
  function walk(systemDark: boolean, steps = THEME_PREFS.length): ThemePref[] {
    const out: ThemePref[] = [];
    let cur: ThemePref = 'system';
    for (let i = 0; i < steps; i += 1) {
      cur = nextThemePref(cur, systemDark);
      out.push(cur);
    }
    return out;
  }

  it('goes to the contrasting theme first on a dark-mode system', () => {
    expect(walk(true)).toEqual(['light', 'dark', 'system']);
  });

  it('goes to the contrasting theme first on a light-mode system', () => {
    expect(walk(false)).toEqual(['dark', 'light', 'system']);
  });

  it('repaints the window on both of the first two presses', () => {
    // The defect this replaced: with a fixed system -> light -> dark order, the
    // first press on a light-mode Mac pins `light` and nothing on screen moves.
    // Found in the built app, not in a test, which is why it has one now.
    for (const systemDark of [true, false]) {
      const [first, second] = walk(systemDark);
      const before = resolveTheme('system', systemDark);
      expect(resolveTheme(first!, systemDark)).not.toBe(before);
      expect(resolveTheme(second!, systemDark)).not.toBe(resolveTheme(first!, systemDark));
    }
  });

  it('visits every state and returns to the start', () => {
    for (const systemDark of [true, false]) {
      const seen = walk(systemDark);
      expect(new Set(seen).size).toBe(THEME_PREFS.length);
      expect(seen[seen.length - 1]).toBe('system');
    }
  });

  it('recovers to a real state when handed something that is not one', () => {
    // A stored value can be anything; the cycle must not dead-end on it.
    expect(THEME_PREFS).toContain(nextThemePref('sepia' as ThemePref, true));
    expect(THEME_PREFS).toContain(nextThemePref('sepia' as ThemePref, false));
  });
});

describe('theme resolution', () => {
  it('lets a pinned preference override the system setting', () => {
    expect(resolveTheme('light', true)).toBe('light');
    expect(resolveTheme('dark', false)).toBe('dark');
  });

  it('follows the system only when asked to', () => {
    expect(resolveTheme('system', true)).toBe('dark');
    expect(resolveTheme('system', false)).toBe('light');
  });
});

describe('state icons', () => {
  // The buttons carry no text. If two states shared a glyph the control would
  // be lying about where in the cycle it is -- which is the single failure mode
  // that makes a cycling toggle unusable.
  it('gives every theme state its own glyph', () => {
    const icons = THEME_PREFS.map(themeIcon);
    expect(new Set(icons).size).toBe(THEME_PREFS.length);
  });

  it('tells follow-system apart from a pinned language', () => {
    expect(localeIcon('system')).not.toBe(localeIcon(DEFAULT_LOCALE));
  });
});

describe('locale resolution', () => {
  it('takes an exact tag match first', () => {
    expect(resolveLocale('system', ['zh-CN', 'en-US'])).toBe('zh-CN');
    expect(resolveLocale('system', ['en-US', 'zh-CN'])).toBe('en-US');
  });

  it('matches tags case-insensitively', () => {
    // Real navigators report `zh-cn` in some configurations.
    expect(resolveLocale('system', ['zh-cn'])).toBe('zh-CN');
    expect(resolveLocale('system', ['EN-us'])).toBe('en-US');
  });

  it('falls back to the primary subtag', () => {
    expect(resolveLocale('system', ['zh'])).toBe('zh-CN');
    expect(resolveLocale('system', ['en-GB'])).toBe('en-US');
  });

  it('prefers an exact match further down the list over a near match higher up', () => {
    // Only meaningful once there are two catalogues; asserted now so the day a
    // second one lands the ordering is already pinned down.
    const langs = ['zh-Hant', 'zh-CN'];
    expect(resolveLocale('system', langs)).toBe('zh-CN');
  });

  it('falls back to the default when nothing matches', () => {
    expect(resolveLocale('system', ['fr-FR', 'de'])).toBe(DEFAULT_LOCALE);
    expect(resolveLocale('system', [])).toBe(DEFAULT_LOCALE);
  });

  it('survives a navigator list with empty entries', () => {
    expect(resolveLocale('system', ['', 'zh-CN'])).toBe('zh-CN');
  });

  it('rejects a pinned locale that has no catalogue', () => {
    expect(resolveLocale('xx-YY' as never, ['zh-CN'])).toBe(DEFAULT_LOCALE);
  });

  it('keeps an explicitly pinned English catalogue independent of the system', () => {
    expect(resolveLocale('en-US', ['zh-CN'])).toBe('en-US');
  });

  it('offers follow-system plus every published catalogue, in that order', () => {
    expect(LOCALE_PREFS[0]).toBe('system');
    expect(LOCALE_PREFS.slice(1)).toEqual([...LOCALES]);
    expect(LOCALE_PREFS).toEqual(['system', 'zh-CN', 'en-US']);
  });
});

describe('stored preference parsing', () => {
  it('treats a missing key as follow-system', () => {
    // `null` means "never chosen". It must reach the fallback as `null`, not be
    // folded into '' or 0 on the way -- this project has shipped that bug.
    expect(parseThemePref(null)).toBe('system');
    expect(parseLocalePref(null)).toBe('system');
  });

  it('refuses a value that is not one of the states', () => {
    // Storage is user-writable: devtools, a synced profile, a hand-edited file.
    expect(parseThemePref('solarized')).toBe('system');
    expect(parseLocalePref('klingon')).toBe('system');
    expect(parseThemePref('')).toBe('system');
  });

  it('accepts every value it is capable of writing', () => {
    for (const p of THEME_PREFS) expect(parseThemePref(p)).toBe(p);
    for (const p of LOCALE_PREFS) expect(parseLocalePref(p)).toBe(p);
  });

  it('keeps the fallback distinct from the allow-list check', () => {
    expect(parsePref(null, ['a', 'b'] as const, 'b')).toBe('b');
    expect(parsePref('a', ['a', 'b'] as const, 'b')).toBe('a');
    expect(parsePref('c', ['a', 'b'] as const, 'b')).toBe('b');
  });

  it('does not turn a missing key into the empty string on the way in', () => {
    // The obvious "simplification" of `parsePref` is to drop the null guard and
    // write `allowed.includes(String(raw ?? ''))`. Against the real allow-lists
    // that behaves identically -- '' is not a theme -- so nothing above catches
    // it. Here '' *is* on the list, which is the only shape that can tell the
    // two implementations apart: `null` must reach the fallback, not be coerced
    // into a member. This project has shipped the `?? 0` version of this bug on
    // a port number; the guard is not decoration.
    expect(parsePref(null, ['', 'b'] as const, 'b')).toBe('b');
    expect(parsePref('', ['', 'b'] as const, 'b')).toBe('');
  });
});
