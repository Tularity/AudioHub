// Keyboard shortcuts: the accelerator model, the default table, conflict
// classification, and persistence.
//
// Everything in here is a pure function over plain data -- no DOM at module
// scope -- so the rules can be tested without a browser. The wiring (one
// capture-phase `keydown` on `document`) lives in `shortcutHost.ts`.
//
// Three decisions worth stating up front, because each of them is the kind of
// thing a later edit would quietly undo:
//
//  1. **Chords are identified by `event.code`, not `event.key`.** `key` is the
//     *produced character*, which on macOS changes under Option (⌥1 produces
//     "¡") and on any non-US layout changes under nothing at all. `code` is the
//     physical key, so "the 1 key" stays the 1 key. The cost is that a French
//     AZERTY user sees "1" on a key that prints "&" -- the same trade every
//     editor makes, and the opposite trade (bindings that silently move when
//     you switch layout) is worse.
//
//  2. **Unbound is `null`, missing is `undefined`, and they are not the same.**
//     A user who clears a shortcut must not get the default back on the next
//     launch. `resolveBindings` therefore distinguishes "key absent from the
//     override map" (use the default) from "key present with value `null`"
//     (deliberately unbound). Folding one into the other is the single most
//     likely regression in this file.
//
//  3. **Reserved combinations are rejected, not silently shadowed.** The
//     tables below are what the OS and the webview take before our listener
//     ever runs; binding to them would produce a shortcut that does nothing,
//     which reads as a broken app rather than a blocked key.

import type { MsgKey } from '../i18n';
import { IS_MAC } from './fmt';

/**
 * The set of things a shortcut can do. Stable ids: they are persisted.
 *
 * `view.pair` is gone (2026-08-11): the pairing wizard is no longer a view, it
 * is a sheet on the main panel. A stale `view.pair` entry left in a user's
 * localStorage is dropped by `decodeOverrides`, which only keeps known ids.
 *
 * The "share protocols" page deliberately gets **no** shortcut. It exists only
 * in share mode, so a binding for it would be a listed key that does nothing
 * half the time -- worse than not offering one.
 */
export type ShortcutActionId =
  | 'view.peers'
  | 'view.stats'
  | 'view.settings'
  | 'nav.back'
  | 'help.shortcuts';

export const SHORTCUT_ACTIONS: readonly ShortcutActionId[] = [
  'view.peers', 'view.stats', 'view.settings', 'nav.back', 'help.shortcuts',
] as const;

export const ACTION_LABEL: Record<ShortcutActionId, MsgKey> = {
  'view.peers': 'shortcuts.action.peers',
  'view.stats': 'shortcuts.action.stats',
  'view.settings': 'shortcuts.action.settings',
  'nav.back': 'shortcuts.action.back',
  'help.shortcuts': 'shortcuts.action.help',
};

/** Which platform's rules to apply. Passed explicitly so tests can check both. */
export type ShortcutPlatform = 'mac' | 'win';

export const PLATFORM: ShortcutPlatform = IS_MAC ? 'mac' : 'win';

export interface Chord {
  meta: boolean;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  /** Normalized physical key id: `1`, `A`, `,`, `[`, `ArrowLeft`, `F5`, `Space`. */
  key: string;
}

/** Minimal shape of a `KeyboardEvent`, so the recorder logic is testable in node. */
export interface KeyEventLike {
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

// ---------------------------------------------------------------- key ids

const PUNCTUATION: Record<string, string> = {
  Comma: ',', Period: '.', Slash: '/', Semicolon: ';', Quote: "'",
  BracketLeft: '[', BracketRight: ']', Backslash: '\\',
  Minus: '-', Equal: '=', Backquote: '`',
};

const NAMED = new Set([
  'Space', 'Enter', 'Tab', 'Backspace', 'Delete', 'Escape',
  'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight',
  'Home', 'End', 'PageUp', 'PageDown',
]);

/**
 * Physical key code -> the id we store and display. Returns `null` for keys we
 * refuse to bind (modifiers on their own, numpad, anything unrecognized) --
 * `null` here means "not a usable key", never "no key pressed".
 */
export function keyIdFromCode(code: string): string | null {
  const digit = /^Digit([0-9])$/.exec(code);
  if (digit) return digit[1]!;
  const letter = /^Key([A-Z])$/.exec(code);
  if (letter) return letter[1]!;
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  if (PUNCTUATION[code]) return PUNCTUATION[code]!;
  if (NAMED.has(code)) return code;
  return null;
}

// ---------------------------------------------------------------- accelerators

/**
 * Canonical string form. Fixed modifier order so two chords that mean the same
 * thing always serialize identically -- conflict detection is string equality.
 */
export function formatAccelerator(c: Chord): string {
  const parts: string[] = [];
  if (c.meta) parts.push('Meta');
  if (c.ctrl) parts.push('Ctrl');
  if (c.alt) parts.push('Alt');
  if (c.shift) parts.push('Shift');
  parts.push(c.key);
  return parts.join('+');
}

/** Inverse of `formatAccelerator`. Returns `null` for anything malformed. */
export function parseAccelerator(s: string): Chord | null {
  if (!s) return null;
  const parts = s.split('+');
  const key = parts.pop();
  if (!key) return null;
  const c: Chord = { meta: false, ctrl: false, alt: false, shift: false, key };
  for (const p of parts) {
    if (p === 'Meta') c.meta = true;
    else if (p === 'Ctrl') c.ctrl = true;
    else if (p === 'Alt') c.alt = true;
    else if (p === 'Shift') c.shift = true;
    else return null;
  }
  // Round-trip guard: rejects `Ctrl+Meta+1`, `Meta+Meta+1` and friends, so a
  // hand-edited localStorage entry cannot smuggle in a form that never matches.
  return formatAccelerator(c) === s ? c : null;
}

/**
 * "Is this the Escape key?", asked of `code` rather than `key`.
 *
 * `KeyboardEvent.key` is the *produced value*, and it is not reliable for named
 * keys from every event source: a synthesised press (`osascript`'s `key code`,
 * and any other CGEvent that carries no unicode payload) arrives at WebKit with
 * `code === 'Escape'` and a `key` that is not `'Escape'` — measured in the built
 * app, where the cheat sheet refused to close and the same press was visibly
 * reaching `document`. `code` is the physical key and is right in both cases,
 * which is also the reason chords are identified by `code` (see the header).
 */
export function isEscape(e: { key?: string; code?: string }): boolean {
  return e.code === 'Escape' || e.key === 'Escape';
}

export function chordFromEvent(e: KeyEventLike): Chord | null {
  const key = keyIdFromCode(e.code);
  if (key === null) return null;
  return { meta: e.metaKey, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, key };
}

export function hasModifier(c: Chord): boolean {
  return c.meta || c.ctrl || c.alt || c.shift;
}

const FUNCTION_KEY = /^F([1-9]|1[0-9]|2[0-4])$/;

/**
 * A bare letter would fight every text field on the page, so a modifier is
 * required -- except for the function row, which no field claims.
 */
export function isBindableChord(c: Chord): boolean {
  return hasModifier(c) || FUNCTION_KEY.test(c.key);
}

// ---------------------------------------------------------------- display

const MAC_GLYPH: Record<string, string> = { meta: '⌘', ctrl: '⌃', alt: '⌥', shift: '⇧' };
const WIN_WORD: Record<string, MsgKey> = {
  ctrl: 'shortcuts.mod.ctrl', alt: 'shortcuts.mod.alt',
  shift: 'shortcuts.mod.shift', meta: 'shortcuts.mod.win',
};

const KEY_GLYPH: Record<string, string> = {
  ArrowLeft: '←', ArrowRight: '→', ArrowUp: '↑', ArrowDown: '↓',
};

/**
 * One capsule per token, never one string.
 *
 * `translate` is injected rather than imported so this stays a pure function:
 * the Windows modifier names are user-visible text and must come from the
 * catalogue, but a test should not have to boot i18n to check ordering.
 * macOS renders glyphs (⌘⌥⌃⇧, its own convention); Windows renders words.
 */
export function acceleratorTokens(
  c: Chord,
  platform: ShortcutPlatform,
  translate: (k: MsgKey) => string,
): string[] {
  const out: string[] = [];
  if (platform === 'mac') {
    // Apple's fixed order on the key cap: ⌃ ⌥ ⇧ ⌘, command adjacent to the key.
    if (c.ctrl) out.push(MAC_GLYPH.ctrl!);
    if (c.alt) out.push(MAC_GLYPH.alt!);
    if (c.shift) out.push(MAC_GLYPH.shift!);
    if (c.meta) out.push(MAC_GLYPH.meta!);
  } else {
    if (c.ctrl) out.push(translate(WIN_WORD.ctrl!));
    if (c.meta) out.push(translate(WIN_WORD.meta!));
    if (c.alt) out.push(translate(WIN_WORD.alt!));
    if (c.shift) out.push(translate(WIN_WORD.shift!));
  }
  out.push(KEY_GLYPH[c.key] ?? c.key);
  return out;
}

// ---------------------------------------------------------------- defaults

const DEFAULTS: Record<ShortcutPlatform, Record<ShortcutActionId, string>> = {
  mac: {
    'view.peers': 'Meta+1',
    'view.stats': 'Meta+2',
    // ⌘, is an Apple platform convention, not a preference -- it is the
    // default here rather than ⌘3 for that reason. ⌘3 still works, as a
    // built-in alias below, so the number row stays complete.
    'view.settings': 'Meta+,',
    'nav.back': 'Meta+[',
    'help.shortcuts': 'Meta+/',
  },
  win: {
    'view.peers': 'Ctrl+1',
    'view.stats': 'Ctrl+2',
    'view.settings': 'Ctrl+,',
    'nav.back': 'Alt+ArrowLeft',
    'help.shortcuts': 'Ctrl+/',
  },
};

/**
 * Fixed extra accelerators that are not user-editable and not listed as rows.
 *
 * They exist so that ⌘1–⌘3 reads as one complete family even though settings'
 * primary binding is the platform-conventional ⌘,. An alias only fires when no
 * user binding claims the same chord, so rebinding ⌘3 elsewhere wins.
 */
const ALIASES: Record<ShortcutPlatform, Partial<Record<ShortcutActionId, string>>> = {
  mac: { 'view.settings': 'Meta+3' },
  win: { 'view.settings': 'Ctrl+3' },
};

export function defaultBindings(platform: ShortcutPlatform): Record<ShortcutActionId, string> {
  return { ...DEFAULTS[platform] };
}

export function aliasFor(action: ShortcutActionId, platform: ShortcutPlatform): string | null {
  return ALIASES[platform][action] ?? null;
}

// ---------------------------------------------------------------- reserved

/** Chords the OS eats before the webview sees them. Binding to these is refused. */
const SYSTEM_RESERVED: Record<ShortcutPlatform, readonly string[]> = {
  mac: [
    // Tauri installs `Menu::default()` when no menu is set (tauri 2.11.5
    // app.rs:2245); a menu key equivalent is matched ahead of the webview, so
    // these never arrive as keydown at all.
    'Meta+Q', 'Meta+W', 'Meta+M', 'Meta+H', 'Meta+Alt+H',
    'Meta+X', 'Meta+C', 'Meta+V', 'Meta+A', 'Meta+Z', 'Meta+Shift+Z',
    // System-wide.
    'Meta+Space', 'Ctrl+Space', 'Meta+Tab', 'Meta+Alt+Escape', 'Meta+`',
    'Meta+Shift+3', 'Meta+Shift+4', 'Meta+Shift+5', 'Ctrl+Meta+F',
  ],
  win: [
    'Alt+F4', 'Alt+Tab', 'Alt+Space', 'Ctrl+Shift+Escape', 'Ctrl+Alt+Delete',
  ],
};

/** Chords the webview claims. Bindable, but they may not survive -- so we warn. */
const WEBVIEW_RESERVED: Record<ShortcutPlatform, readonly string[]> = {
  mac: [
    'Meta+R', 'Meta+Shift+R', 'Meta+P', 'Meta+F', 'Meta+G',
    'Meta+-', 'Meta+=', 'Meta+0', 'Meta+Alt+I',
  ],
  win: [
    'Ctrl+R', 'Ctrl+Shift+R', 'Ctrl+P', 'Ctrl+F', 'F5', 'F12',
    'Ctrl+-', 'Ctrl+=', 'Ctrl+0', 'Ctrl+Shift+I',
  ],
};

export type ReservedKind = 'system' | 'webview';

export function reservedKind(accel: string, platform: ShortcutPlatform): ReservedKind | null {
  if (SYSTEM_RESERVED[platform].includes(accel)) return 'system';
  if (WEBVIEW_RESERVED[platform].includes(accel)) return 'webview';
  return null;
}

// ---------------------------------------------------------------- persistence

export const SHORTCUTS_STORAGE_KEY = 'audiohub.shortcuts.v1';

/**
 * The persisted shape.
 *
 * A key that is **absent** means "no opinion, use the default". A key mapped to
 * **`null`** means "the user cleared this on purpose". Those are two different
 * states and the whole point of storing overrides rather than resolved values.
 */
export type ShortcutOverrides = Partial<Record<ShortcutActionId, string | null>>;

function isActionId(k: string): k is ShortcutActionId {
  return (SHORTCUT_ACTIONS as readonly string[]).includes(k);
}

/** Tolerant of anything: a corrupt entry drops that one action, not the file. */
export function decodeOverrides(raw: string | null): ShortcutOverrides {
  if (!raw) return {};
  let parsed: unknown;
  try { parsed = JSON.parse(raw); } catch { return {}; }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
  const out: ShortcutOverrides = {};
  for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
    if (!isActionId(k)) continue;
    // `null` survives verbatim -- that is the "cleared" state. Anything that is
    // neither null nor a parseable accelerator is dropped, so a stale format
    // degrades to the default instead of to an unreachable binding.
    if (v === null) { out[k] = null; continue; }
    if (typeof v === 'string' && parseAccelerator(v)) out[k] = v;
  }
  return out;
}

export function encodeOverrides(o: ShortcutOverrides): string {
  return JSON.stringify(o);
}

/**
 * Overrides layered on defaults. The result maps every action to an
 * accelerator **or to `null`**, and `null` is reachable only through an
 * explicit clear.
 */
export function resolveBindings(
  overrides: ShortcutOverrides,
  platform: ShortcutPlatform,
): Record<ShortcutActionId, string | null> {
  const defaults = DEFAULTS[platform];
  const out = {} as Record<ShortcutActionId, string | null>;
  for (const id of SHORTCUT_ACTIONS) {
    // `in` rather than a truthiness check: `overrides[id] === null` is a real
    // answer and must not fall through to the default.
    out[id] = Object.prototype.hasOwnProperty.call(overrides, id)
      ? (overrides[id] ?? null)
      : defaults[id];
  }
  return out;
}

/** Which action a chord fires, honouring user bindings first and aliases second. */
export function lookupAction(
  accel: string,
  bindings: Record<ShortcutActionId, string | null>,
  platform: ShortcutPlatform,
): ShortcutActionId | null {
  for (const id of SHORTCUT_ACTIONS) {
    if (bindings[id] === accel) return id;
  }
  for (const id of SHORTCUT_ACTIONS) {
    // An alias is shadowed by any real binding on the same chord, and by the
    // action's own binding having been cleared -- a cleared action stays silent.
    if (bindings[id] !== null && ALIASES[platform][id] === accel) return id;
  }
  return null;
}

export interface AssignVerdict {
  /** `'ok'` and `'warn'` are savable; `'reject'` is not. */
  status: 'ok' | 'warn' | 'reject';
  reason: 'system' | 'webview' | 'no-modifier' | 'taken' | null;
  /** Set only when `reason === 'taken'`: the action that currently owns it. */
  conflictsWith: ShortcutActionId | null;
}

/**
 * The one place that decides whether a recorded chord may be stored.
 *
 * Three outcomes on purpose (design §6.3): reject what can never fire, warn
 * about what may not fire, and hand back the displaced action so the UI can ask
 * before taking a binding away from it. Silently overwriting is the failure
 * mode that loses a user's binding without telling them.
 */
export function verdictFor(
  action: ShortcutActionId,
  accel: string,
  bindings: Record<ShortcutActionId, string | null>,
  platform: ShortcutPlatform,
): AssignVerdict {
  const chord = parseAccelerator(accel);
  if (!chord || !isBindableChord(chord)) {
    return { status: 'reject', reason: 'no-modifier', conflictsWith: null };
  }
  const reserved = reservedKind(accel, platform);
  if (reserved === 'system') {
    return { status: 'reject', reason: 'system', conflictsWith: null };
  }
  const taken = SHORTCUT_ACTIONS.find((id) => id !== action && bindings[id] === accel) ?? null;
  if (taken) return { status: 'warn', reason: 'taken', conflictsWith: taken };
  if (reserved === 'webview') return { status: 'warn', reason: 'webview', conflictsWith: null };
  return { status: 'ok', reason: null, conflictsWith: null };
}

/**
 * Applying a binding also **unbinds whoever held it**, recorded as an explicit
 * `null` rather than by deleting the key -- deleting would resurrect that
 * action's default on the next launch, which is exactly the shortcut the user
 * just took away.
 */
export function applyBinding(
  overrides: ShortcutOverrides,
  action: ShortcutActionId,
  accel: string | null,
  bindings: Record<ShortcutActionId, string | null>,
): ShortcutOverrides {
  const next: ShortcutOverrides = { ...overrides };
  if (accel !== null) {
    for (const id of SHORTCUT_ACTIONS) {
      if (id !== action && bindings[id] === accel) next[id] = null;
    }
  }
  next[action] = accel;
  return next;
}

/** Restoring a default is *removing* the override, not writing the default in. */
export function clearOverride(overrides: ShortcutOverrides, action: ShortcutActionId): ShortcutOverrides {
  const next: ShortcutOverrides = { ...overrides };
  delete next[action];
  return next;
}

export function isCustomized(overrides: ShortcutOverrides, action: ShortcutActionId): boolean {
  return Object.prototype.hasOwnProperty.call(overrides, action);
}

/**
 * 有几个动作的键位**与出厂不同**（设置 › 杂项那一行的值）。
 *
 * 判据取「解析结果 ≠ 该平台默认值」，而不是「overrides 里有没有这个键」。两者会
 * 分岔，而分岔的方向恰好是骗人的那一边：
 *
 *   · 用户把某个动作改回它原本的键，`ShortcutRow` 照样会写一条 override —— 按
 *     键数算就成了「1 个已自定义」，而屏幕上四个键位与出厂一模一样；
 *   · 用户**清掉**一个键（`null`，与默认不同）是真的改过，必须算进去。
 *
 * 这一行是给用户估「我动过多少」的，不是给存储层做统计的。
 */
export function customizedCount(
  overrides: ShortcutOverrides,
  platform: ShortcutPlatform,
): number {
  const defaults = DEFAULTS[platform];
  const resolved = resolveBindings(overrides, platform);
  return SHORTCUT_ACTIONS.filter((id) => resolved[id] !== defaults[id]).length;
}
