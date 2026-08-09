// The Windows caption strip's geometry, and the channel Rust uses to tell the
// page about a button it can no longer see the mouse over.
//
// # Why the numbers live in two languages
//
// Answering `WM_NCHITTEST` with `HTMAXBUTTON` is what makes Windows 11 pop the
// snap-layout picker, and the answer has to be right from the first message --
// before React has laid anything out. So `app/src-tauri/src/win_chrome.rs`
// computes the maximize button's rectangle from constants of its own instead of
// waiting for the frontend to report one. That makes the box size a contract
// between the two sides, and `caption.test.ts` reads the Rust file to prove
// they still agree.
//
// 46x32 is not a taste: it is the size of Windows 11's own caption buttons, and
// matching it is most of what makes a hand-drawn strip read as native.
//
// # Why Rust pushes state in rather than emitting an event
//
// Over the maximize button the mouse is *non-client*, so the webview gets no
// `mousemove`/`mousedown` there: CSS `:hover` is dead and `onClick` never
// fires. Rust has to hand the hover/press state over. The usual channel is
// `emit`/`listen`, but `listen` is an ACL-governed plugin command and this app
// deliberately ships no capabilities file (see `start_window_drag` in
// `main.rs`). Rust-to-JS `eval` is not ACL-governed, so `win_chrome.rs` calls
// the hook installed below. On macOS nothing ever calls it.

/** Caption button box in CSS pixels. Mirrored in `win_chrome.rs`. */
export const CAPTION_BUTTON_W = 46;
export const CAPTION_BUTTON_H = 32;
/** close + maximize + minimize. The gutter the trailing edge must reserve. */
export const CAPTION_STRIP_W = CAPTION_BUTTON_W * 3;

/** The states Rust can push. Kept in sync with `win_chrome.rs`'s `push()`. */
export type CaptionSignal = 'hover' | 'press' | 'max';

type CaptionListener = (kind: CaptionSignal, on: boolean) => void;

// Declared on `globalThis`, not on `Window`. In a browser the two are the same
// object, so `win_chrome.rs` writing `window.__audiohubCaption(...)` still finds
// it -- and going through `globalThis` keeps this module free of DOM, which is
// what lets it be tested under vitest's default `environment: 'node'` (see
// `vitest.config.ts`: jsdom is deliberately not in the dependency tree).
declare global {
  // eslint-disable-next-line no-var
  var __audiohubCaption: ((kind: string, on: boolean) => void) | undefined;
}

const listeners = new Set<CaptionListener>();

/**
 * Subscribe to the pushed caption state.
 *
 * The global hook is installed on the first subscription rather than at module
 * load: on macOS the caption buttons never render, and leaving a global behind
 * that nothing reads is how a later reader concludes the platform branch is
 * live when it is not.
 */
export function onCaptionSignal(fn: CaptionListener): () => void {
  listeners.add(fn);
  globalThis.__audiohubCaption = (kind: string, on: boolean) => {
    if (kind !== 'hover' && kind !== 'press' && kind !== 'max') return;
    for (const l of listeners) l(kind, !!on);
  };
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0) delete globalThis.__audiohubCaption;
  };
}
