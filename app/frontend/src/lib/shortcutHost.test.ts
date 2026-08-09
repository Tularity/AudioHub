// One invariant, and it is not a performance one.
//
// `currentBindings` is read through `useSyncExternalStore`, which decides
// "did this change?" by object identity. `resolveBindings` builds a fresh
// object every call, so wiring it up directly makes every render report a new
// snapshot -- and React answers that with an unbounded re-render loop. The
// symptom is not a warning or a slow frame: the entire app renders as an empty
// window, in the built bundle, with nothing in the logs. That happened once
// here, which is why the memoisation has a test rather than a comment.
//
// The second half is the flip side: the snapshot must actually change when the
// overrides do, or the settings page edits a binding and nothing anywhere
// notices.

import { describe, it, expect, beforeEach } from 'vitest';
import {
  beginRecordingCapture, currentBindings, endRecordingCapture, getOverrides, isRecordingCapture,
  isTextEntry, setOverrides, subscribeShortcuts,
} from './shortcutHost';
import { PLATFORM, defaultBindings } from './shortcuts';

describe('binding snapshot', () => {
  beforeEach(() => { setOverrides({}); });

  it('is stable by identity between changes', () => {
    const a = currentBindings();
    const b = currentBindings();
    expect(b).toBe(a);
  });

  it('is a new object after an override changes', () => {
    const before = currentBindings();
    setOverrides({ 'view.pair': null });
    const after = currentBindings();
    expect(after).not.toBe(before);
    expect(after['view.pair']).toBeNull();
  });

  it('starts from the platform defaults', () => {
    expect(currentBindings()['view.peers']).toBe(defaultBindings(PLATFORM)['view.peers']);
  });

  it('notifies subscribers exactly once per change', () => {
    let n = 0;
    const off = subscribeShortcuts(() => { n += 1; });
    setOverrides({ 'view.stats': 'Meta+7' });
    expect(n).toBe(1);
    off();
    setOverrides({});
    expect(n).toBe(1);
  });

  it('survives a storage backend that throws', () => {
    // Safari private mode and blocked third-party storage both make
    // `localStorage` throw outright. A keyboard preference is not worth taking
    // the app down for; the fallback is "defaults, not persisted".
    expect(() => setOverrides({ 'nav.back': null })).not.toThrow();
    expect(getOverrides()['nav.back']).toBeNull();
  });
});

describe('recording capture', () => {
  it('is claimed and released by the same token', () => {
    const row = {};
    expect(isRecordingCapture()).toBe(false);
    beginRecordingCapture(row);
    expect(isRecordingCapture()).toBe(true);
    endRecordingCapture(row);
    expect(isRecordingCapture()).toBe(false);
  });

  it('cannot be released by a row that does not hold it', () => {
    // The reason this is a token and not a boolean (or, as it first was, a
    // `data-` attribute on <body>): SettingsView subscribes to the whole store,
    // so every status poll re-runs all six rows' effects. A sibling that
    // cleared shared state on the way past revoked the recording row's claim,
    // and the next keypress navigated the app instead of being recorded.
    const recorder = {};
    const sibling = {};
    beginRecordingCapture(recorder);
    endRecordingCapture(sibling);
    expect(isRecordingCapture()).toBe(true);
    endRecordingCapture(recorder);
    expect(isRecordingCapture()).toBe(false);
  });

  it('hands over cleanly when a second row starts recording', () => {
    // Clicking another capsule blurs the first, so its cleanup lands *after*
    // the new owner has claimed. That late release must not disarm the new one.
    const first = {};
    const second = {};
    beginRecordingCapture(first);
    beginRecordingCapture(second);
    endRecordingCapture(first);
    expect(isRecordingCapture()).toBe(true);
    endRecordingCapture(second);
    expect(isRecordingCapture()).toBe(false);
  });
});

describe('isTextEntry', () => {
  it('claims the keyboard for anything with a caret', () => {
    // Typing must win over navigating: someone entering a port number who
    // types `4` should get a `4`, not the settings page.
    for (const tag of ['INPUT', 'TEXTAREA', 'SELECT']) {
      expect(isTextEntry({ tagName: tag } as Element), tag).toBe(true);
    }
    expect(isTextEntry({ tagName: 'BUTTON' } as Element)).toBe(false);
    expect(isTextEntry(null)).toBe(false);
  });
});
