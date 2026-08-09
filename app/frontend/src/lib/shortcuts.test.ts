// Guards for the shortcut rules. Everything under test here is a pure function
// over plain data, which is why `shortcuts.ts` keeps the DOM out.
//
// Four hazards, each of which has a named test below:
//
//  1. **`null` folded into "missing".** Unbound and never-set are different
//     states: a user who clears a shortcut must not get the default back on the
//     next launch. `resolveBindings` is the only thing standing between those
//     two, and the bug is invisible until a restart -- exactly the class of
//     regression that reads fine in review.
//
//  2. **Silent overwrite.** Assigning a chord that another action already owns
//     has to hand back *whose* it was, so the UI can name it before taking it.
//     A `verdictFor` that returned plain `ok` would let a user lose a binding
//     without ever learning that they did.
//
//  3. **Binding to a chord that can never fire.** ⌘Q and friends are matched by
//     the macOS menu ahead of the webview; a shortcut bound there does nothing,
//     which reads as a broken app rather than as a blocked key.
//
//  4. **Accelerator strings that do not round-trip.** Conflict detection is
//     string equality, so two spellings of the same chord would defeat it
//     entirely.

import { describe, it, expect } from 'vitest';
import {
  SHORTCUT_ACTIONS,
  aliasFor, applyBinding, chordFromEvent, clearOverride, decodeOverrides, defaultBindings,
  encodeOverrides, formatAccelerator, isBindableChord, isCustomized, keyIdFromCode,
  isEscape, lookupAction, parseAccelerator, reservedKind, resolveBindings, verdictFor,
} from './shortcuts';
import type { Chord, KeyEventLike, ShortcutOverrides } from './shortcuts';

function ev(code: string, mods: Partial<KeyEventLike> = {}): KeyEventLike {
  return { code, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...mods };
}

describe('key identity', () => {
  it('reads the physical key, not the produced character', () => {
    // ⌥1 produces "¡" on a US layout and something else again on others; the
    // binding has to stay on "the 1 key" either way.
    expect(chordFromEvent(ev('Digit1', { altKey: true, metaKey: true })))
      .toEqual({ meta: true, ctrl: false, alt: true, shift: false, key: '1' });
    expect(keyIdFromCode('KeyA')).toBe('A');
    expect(keyIdFromCode('Comma')).toBe(',');
    expect(keyIdFromCode('F5')).toBe('F5');
    expect(keyIdFromCode('ArrowLeft')).toBe('ArrowLeft');
  });

  it('refuses keys that cannot carry a binding', () => {
    // A modifier on its own is not a chord, and `null` here means "unusable
    // key" -- never "no key was pressed".
    expect(keyIdFromCode('MetaLeft')).toBeNull();
    expect(keyIdFromCode('ShiftRight')).toBeNull();
    expect(keyIdFromCode('Numpad1')).toBeNull();
    expect(chordFromEvent(ev('ControlLeft', { ctrlKey: true }))).toBeNull();
  });

  it('requires a modifier except on the function row', () => {
    const bare = (key: string): Chord => ({ meta: false, ctrl: false, alt: false, shift: false, key });
    // A bare letter would fight every text field on the page.
    expect(isBindableChord(bare('A'))).toBe(false);
    expect(isBindableChord(bare('F5'))).toBe(true);
    expect(isBindableChord({ ...bare('A'), meta: true })).toBe(true);
  });
});

describe('isEscape', () => {
  it('trusts the physical key over the produced value', () => {
    // A synthesised press (osascript `key code 53`, and any CGEvent with no
    // unicode payload) reaches WebKit with the right `code` and a `key` that is
    // not 'Escape'. Measured in the built app: the cheat sheet would not close
    // while the press was demonstrably reaching `document`.
    expect(isEscape({ code: 'Escape', key: 'Unidentified' })).toBe(true);
    expect(isEscape({ code: 'Escape' })).toBe(true);
    // The other direction still counts -- some hosts fill in only `key`.
    expect(isEscape({ key: 'Escape' })).toBe(true);
    expect(isEscape({ code: 'KeyE', key: 'e' })).toBe(false);
    expect(isEscape({})).toBe(false);
  });
});

describe('accelerator strings', () => {
  it('serialises one chord to exactly one string', () => {
    // Conflict detection is string equality, so a second spelling of the same
    // chord would silently defeat it. Modifier order is fixed for that reason.
    const a = formatAccelerator({ meta: true, ctrl: false, alt: true, shift: true, key: '1' });
    const b = formatAccelerator({ shift: true, alt: true, ctrl: false, meta: true, key: '1' });
    expect(a).toBe('Meta+Alt+Shift+1');
    expect(b).toBe(a);
  });

  it('round-trips, and rejects anything that does not', () => {
    for (const s of ['Meta+1', 'Ctrl+Shift+F5', 'Alt+ArrowLeft', 'Meta+,']) {
      expect(formatAccelerator(parseAccelerator(s)!)).toBe(s);
    }
    // Wrong order, duplicated modifier, unknown modifier, empty: all refused,
    // so a hand-edited localStorage entry cannot smuggle in a form that never
    // matches a real keypress.
    expect(parseAccelerator('Ctrl+Meta+1')).toBeNull();
    expect(parseAccelerator('Meta+Meta+1')).toBeNull();
    expect(parseAccelerator('Hyper+1')).toBeNull();
    expect(parseAccelerator('')).toBeNull();
  });
});

describe('defaults', () => {
  it('gives every action a binding on both platforms', () => {
    for (const platform of ['mac', 'win'] as const) {
      const d = defaultBindings(platform);
      for (const a of SHORTCUT_ACTIONS) {
        expect(d[a], `${platform}/${a}`).toBeTruthy();
        expect(parseAccelerator(d[a]), `${platform}/${a} parses`).not.toBeNull();
      }
    }
  });

  it('never ships a default that the platform reserves', () => {
    // The whole point of the reserved tables: a default we cannot receive is a
    // menu item that looks broken out of the box.
    for (const platform of ['mac', 'win'] as const) {
      const d = defaultBindings(platform);
      for (const a of SHORTCUT_ACTIONS) {
        expect(reservedKind(d[a], platform), `${platform}/${a}`).not.toBe('system');
      }
    }
  });

  it('has no two actions on the same chord', () => {
    for (const platform of ['mac', 'win'] as const) {
      const used = Object.values(defaultBindings(platform));
      expect(new Set(used).size).toBe(used.length);
    }
  });

  it('opens settings with the platform-conventional chord', () => {
    // Apple's convention, not a preference. The number-row ⌘4 survives as an
    // alias so that family stays complete.
    expect(defaultBindings('mac')['view.settings']).toBe('Meta+,');
    expect(defaultBindings('win')['view.settings']).toBe('Ctrl+,');
    expect(aliasFor('view.settings', 'mac')).toBe('Meta+4');
    expect(aliasFor('view.peers', 'mac')).toBeNull();
  });
});

describe('resolveBindings', () => {
  it('treats "cleared" and "never set" as different states', () => {
    // The single most likely regression in the module: fold `null` into
    // "absent" and a cleared shortcut silently comes back on next launch.
    const cleared = resolveBindings({ 'view.pair': null }, 'mac');
    expect(cleared['view.pair']).toBeNull();
    expect(cleared['view.peers']).toBe('Meta+1');

    const absent = resolveBindings({}, 'mac');
    expect(absent['view.pair']).toBe('Meta+2');
  });

  it('survives a corrupt or hand-edited store one entry at a time', () => {
    const raw = JSON.stringify({
      'view.peers': 'Meta+9',     // valid override
      'view.pair': null,          // deliberately cleared
      'view.stats': 'Nonsense++', // unparseable -> dropped, falls back to default
      'not.an.action': 'Meta+8',  // unknown id -> dropped
    });
    const o = decodeOverrides(raw);
    expect(o).toEqual({ 'view.peers': 'Meta+9', 'view.pair': null });
    const b = resolveBindings(o, 'mac');
    expect(b['view.peers']).toBe('Meta+9');
    expect(b['view.pair']).toBeNull();
    expect(b['view.stats']).toBe('Meta+3');
  });

  it('decodes nothing at all rather than throwing', () => {
    expect(decodeOverrides(null)).toEqual({});
    expect(decodeOverrides('not json')).toEqual({});
    expect(decodeOverrides('[1,2,3]')).toEqual({});
    expect(decodeOverrides('"a string"')).toEqual({});
  });

  it('round-trips through storage', () => {
    const o: ShortcutOverrides = { 'view.peers': 'Meta+9', 'nav.back': null };
    expect(decodeOverrides(encodeOverrides(o))).toEqual(o);
  });
});

describe('lookupAction', () => {
  const mac = resolveBindings({}, 'mac');

  it('fires the action a chord is bound to', () => {
    expect(lookupAction('Meta+1', mac, 'mac')).toBe('view.peers');
    expect(lookupAction('Meta+,', mac, 'mac')).toBe('view.settings');
    expect(lookupAction('Meta+9', mac, 'mac')).toBeNull();
  });

  it('honours the alias, but lets a real binding shadow it', () => {
    expect(lookupAction('Meta+4', mac, 'mac')).toBe('view.settings');
    const stolen = resolveBindings({ 'view.stats': 'Meta+4' }, 'mac');
    expect(lookupAction('Meta+4', stolen, 'mac')).toBe('view.stats');
  });

  it('stays silent for an action whose binding was cleared', () => {
    // Clearing settings must also silence its alias -- otherwise ⌘4 keeps
    // opening the page the user just unbound.
    const cleared = resolveBindings({ 'view.settings': null }, 'mac');
    expect(lookupAction('Meta+,', cleared, 'mac')).toBeNull();
    expect(lookupAction('Meta+4', cleared, 'mac')).toBeNull();
  });
});

describe('verdictFor', () => {
  const mac = resolveBindings({}, 'mac');

  it('refuses a chord the OS takes before we see it', () => {
    // ⌘Q reaches Tauri's default menu, never the webview. Accepting it would
    // store a shortcut that provably cannot fire.
    const v = verdictFor('view.stats', 'Meta+Q', mac, 'mac');
    expect(v).toEqual({ status: 'reject', reason: 'system', conflictsWith: null });
  });

  it('refuses a bare key', () => {
    expect(verdictFor('view.stats', 'A', mac, 'mac').reason).toBe('no-modifier');
    expect(verdictFor('view.stats', 'F7', mac, 'mac').status).toBe('ok');
  });

  it('names the action it would displace instead of taking it silently', () => {
    const v = verdictFor('view.stats', 'Meta+1', mac, 'mac');
    expect(v.status).toBe('warn');
    expect(v.reason).toBe('taken');
    expect(v.conflictsWith).toBe('view.peers');
  });

  it('does not call an action a conflict with itself', () => {
    expect(verdictFor('view.peers', 'Meta+1', mac, 'mac').status).toBe('ok');
  });

  it('warns but allows a chord the webview may intercept', () => {
    const v = verdictFor('view.stats', 'Meta+F', mac, 'mac');
    expect(v.status).toBe('warn');
    expect(v.reason).toBe('webview');
  });

  it('applies the platform it is given, not the one it is running on', () => {
    // Alt+F4 is fatal on Windows and unremarkable on macOS; the tables are per
    // platform so a Windows build cannot be checked against Apple's list.
    expect(verdictFor('view.stats', 'Alt+F4', resolveBindings({}, 'win'), 'win').reason).toBe('system');
    expect(verdictFor('view.stats', 'Meta+Q', resolveBindings({}, 'win'), 'win').status).toBe('ok');
  });
});

describe('applyBinding', () => {
  const mac = resolveBindings({}, 'mac');

  it('unbinds the displaced action explicitly, not by deletion', () => {
    // Deleting the key would resurrect that action's *default* on the next
    // launch -- which is exactly the shortcut the user just took away.
    const next = applyBinding({}, 'view.stats', 'Meta+1', mac);
    expect(next['view.stats']).toBe('Meta+1');
    expect(Object.prototype.hasOwnProperty.call(next, 'view.peers')).toBe(true);
    expect(next['view.peers']).toBeNull();
    expect(resolveBindings(next, 'mac')['view.peers']).toBeNull();
  });

  it('clearing a binding leaves the others alone', () => {
    const next = applyBinding({}, 'view.pair', null, mac);
    expect(next).toEqual({ 'view.pair': null });
  });

  it('restoring a default removes the override rather than writing it in', () => {
    // Writing the default value in would pin today's default forever; a later
    // change to the table would never reach a user who once pressed "reset".
    const custom = applyBinding({}, 'view.pair', 'Meta+8', mac);
    expect(isCustomized(custom, 'view.pair')).toBe(true);
    const back = clearOverride(custom, 'view.pair');
    expect(isCustomized(back, 'view.pair')).toBe(false);
    expect(back).toEqual({});
    expect(resolveBindings(back, 'mac')['view.pair']).toBe('Meta+2');
  });

  it('counts a cleared action as customised', () => {
    // Otherwise the row offers no way back to its default.
    expect(isCustomized(applyBinding({}, 'view.pair', null, mac), 'view.pair')).toBe(true);
  });
});
