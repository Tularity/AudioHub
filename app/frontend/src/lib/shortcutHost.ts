// The DOM half of the shortcut system: persistence, subscription, and the one
// keydown listener. All the *rules* live in `shortcuts.ts`, which stays pure.
//
// Why a module-local store rather than the zustand app store: shortcuts are a
// pure UI preference with no daemon counterpart, and `AppState` is the shape
// the IPC layer fills in. Parking a browser-only preference in there would
// invite it into a settings payload later.

import {
  PLATFORM, SHORTCUTS_STORAGE_KEY,
  chordFromEvent, decodeOverrides, encodeOverrides, formatAccelerator,
  lookupAction, resolveBindings,
} from './shortcuts';
import type { ShortcutActionId, ShortcutOverrides } from './shortcuts';

// ---------------------------------------------------------------- store

let overrides: ShortcutOverrides = {};
let loaded = false;
const listeners = new Set<() => void>();

/**
 * Memoised result of `resolveBindings`, recomputed only when the overrides
 * change.
 *
 * This is load-bearing, not an optimisation. `currentBindings` is read through
 * `useSyncExternalStore`, which compares snapshots by identity: a function that
 * builds a fresh object on every call reports "changed" on every render, and
 * React answers with an infinite re-render loop that renders the whole app as a
 * blank window. Measured, in the built app, exactly once.
 */
let resolved: Record<ShortcutActionId, string | null> | null = null;

/**
 * `localStorage` throws outright in a few real configurations (Safari private
 * mode, third-party-cookie blocking on an embedded origin). A shortcut
 * preference is not worth taking the app down for, so every access is guarded
 * and failure degrades to "defaults, not persisted".
 */
function readStorage(): string | null {
  try { return window.localStorage.getItem(SHORTCUTS_STORAGE_KEY); } catch { return null; }
}

function writeStorage(value: string): void {
  try { window.localStorage.setItem(SHORTCUTS_STORAGE_KEY, value); } catch { /* preference-only */ }
}

export function getOverrides(): ShortcutOverrides {
  if (!loaded) {
    overrides = decodeOverrides(readStorage());
    loaded = true;
  }
  return overrides;
}

export function setOverrides(next: ShortcutOverrides): void {
  overrides = next;
  loaded = true;
  resolved = null;
  writeStorage(encodeOverrides(next));
  for (const fn of listeners) fn();
}

export function subscribeShortcuts(fn: () => void): () => void {
  listeners.add(fn);
  return () => { listeners.delete(fn); };
}

/**
 * The bindings in force. **Stable by identity** until `setOverrides` runs — see
 * the note on `resolved`; this is a `useSyncExternalStore` snapshot.
 */
export function currentBindings(): Record<ShortcutActionId, string | null> {
  if (!resolved) resolved = resolveBindings(getOverrides(), PLATFORM);
  return resolved;
}

// ---------------------------------------------------------------- capture

/**
 * Who, if anyone, is currently recording a shortcut and therefore owns the
 * keyboard.
 *
 * This used to be a `data-` attribute on `<body>`, set by the recording row and
 * cleared by any row that was not recording. That is broken in a way that only
 * shows up in the built app: `SettingsView` subscribes to the whole store, so
 * every status poll re-renders all six rows, and each non-recording row's
 * effect then cleared the flag its recording sibling had just set. Measured
 * symptom: with a row listening for a new chord, pressing ⌘1 navigated the app
 * instead of being captured.
 *
 * A token makes ownership explicit -- a row can only release a claim it still
 * holds, so neither a sibling's re-render nor the previous owner's late cleanup
 * can revoke the current one.
 */
let recordingOwner: object | null = null;

export function beginRecordingCapture(token: object): void {
  recordingOwner = token;
}

export function endRecordingCapture(token: object): void {
  if (recordingOwner === token) recordingOwner = null;
}

export function isRecordingCapture(): boolean {
  return recordingOwner !== null;
}

// ---------------------------------------------------------------- dispatch

/**
 * Typing must win over navigating. A user entering a port number who types `4`
 * should get a `4`, not the settings page -- so anything with a text caret
 * swallows the whole table. `Escape` is not routed through here at all (the
 * dialogs own it), so nothing is lost by the blanket rule.
 */
export function isTextEntry(el: Element | null): boolean {
  if (!el) return false;
  const tag = el.tagName;
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
  // Read the property rather than testing `instanceof HTMLElement`: the
  // constructor does not exist outside a DOM, so the `instanceof` form throws a
  // ReferenceError under the node-environment tests (`vitest.config.ts` keeps
  // jsdom out of the tree on purpose) and would do the same in any other
  // non-browser host. `isContentEditable` already accounts for inheritance.
  return (el as Partial<HTMLElement>).isContentEditable === true;
}

export type ShortcutDispatch = (action: ShortcutActionId) => void;

/**
 * Capture phase, so a shortcut still fires when focus sits on a button that
 * would otherwise stop propagation. `preventDefault` only runs for chords we
 * actually claim -- otherwise ⌘C in the fingerprint field would stop copying.
 */
export function installShortcuts(dispatch: ShortcutDispatch): () => void {
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.repeat) return;
    if (isTextEntry(document.activeElement)) return;
    // A recorder in listening state takes the whole keyboard. Asked through the
    // ownership token above rather than relying on listener order, which React
    // does not guarantee.
    if (isRecordingCapture()) return;
    const chord = chordFromEvent(e);
    if (!chord) return;
    const action = lookupAction(formatAccelerator(chord), currentBindings(), PLATFORM);
    if (!action) return;
    e.preventDefault();
    e.stopPropagation();
    dispatch(action);
  };
  document.addEventListener('keydown', onKeyDown, true);
  return () => document.removeEventListener('keydown', onKeyDown, true);
}
