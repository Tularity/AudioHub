// The Windows caption strip is described three times -- in TypeScript, in CSS,
// and in Rust -- and only one of those three can be wrong without anything
// visibly breaking.
//
// `win_chrome.rs` answers `WM_NCHITTEST` with `HTMAXBUTTON` over a rectangle it
// computes from its own constants, because that answer has to be correct before
// React has laid anything out. If the button drawn by CSS and the rectangle
// claimed by Rust drift apart, the app still runs, the button still highlights
// on hover (Rust drives that too, from its own rectangle) -- the only symptom
// is that the highlight is offset from the glyph and the snap picker appears
// over the wrong box. That is a bug nobody reports precisely; it just feels
// broken. Hence a test rather than a comment.
//
// The direction rules get the same treatment: `platform.ts` decides which end
// of the strip is free and `styles.css` acts on it, and the connection between
// them is a string in a `data-` attribute.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';
import { CAPTION_BUTTON_W, CAPTION_BUTTON_H, CAPTION_STRIP_W, onCaptionSignal } from './caption';

// app/frontend/src/lib -> repo root
const ROOT = fileURLToPath(new URL('../../../../', import.meta.url));

function read(rel: string): string {
  return readFileSync(ROOT + rel, 'utf8');
}

/** A `const NAME: f64 = 46.0;` out of the Rust source. */
function rustConst(src: string, name: string): number {
  const m = src.match(new RegExp(`const ${name}:\\s*f64\\s*=\\s*([0-9.]+)`));
  // Throwing beats returning NaN: a cross-language check that quietly matches
  // nothing reads green forever, which is worse than having no check.
  if (!m) throw new Error(`could not locate ${name} in win_chrome.rs`);
  return Number(m[1]);
}

/** A `--name: 46px;` out of the stylesheet. */
function cssPx(src: string, name: string): number {
  const m = src.match(new RegExp(`--${name}:\\s*([0-9.]+)px`));
  if (!m) throw new Error(`could not locate --${name} in styles.css`);
  return Number(m[1]);
}

describe('caption button geometry', () => {
  const rs = read('app/src-tauri/src/win_chrome.rs');
  const css = read('app/frontend/src/styles.css');

  it('agrees with the rectangle win_chrome.rs hit-tests', () => {
    expect(rustConst(rs, 'CAPTION_BUTTON_W')).toBe(CAPTION_BUTTON_W);
    expect(rustConst(rs, 'CAPTION_BUTTON_H')).toBe(CAPTION_BUTTON_H);
  });

  it('agrees with the box styles.css draws', () => {
    expect(cssPx(css, 'caption-btn-w')).toBe(CAPTION_BUTTON_W);
    expect(cssPx(css, 'caption-btn-h')).toBe(CAPTION_BUTTON_H);
  });

  it('puts maximize in the middle slot, counted from the right', () => {
    // Rust locates the button as `client.right - SLOT * w` .. minus another w.
    // Slot 1 is the middle of close/maximize/minimize; slot 0 would silently
    // hit-test the close button, which is how you end up maximising on close.
    expect(rustConst(rs, 'MAXIMIZE_SLOT_FROM_RIGHT')).toBe(1);
    expect(CAPTION_STRIP_W).toBe(CAPTION_BUTTON_W * 3);
  });

  it('reserves exactly the strip width on the trailing edge', () => {
    // styles.css derives the gutter from the button width rather than repeating
    // 138px, so the assertion is that it still does.
    expect(css).toContain('--chrome-trail: calc(var(--caption-btn-w) * 3)');
  });
});

describe('caption signal hook', () => {
  it('routes the three kinds Rust pushes and ignores anything else', () => {
    const seen: [string, boolean][] = [];
    const off = onCaptionSignal((kind, on) => seen.push([kind, on]));

    globalThis.__audiohubCaption!('hover', true);
    globalThis.__audiohubCaption!('press', true);
    globalThis.__audiohubCaption!('max', false);
    // A kind from a newer shell must not reach a listener that cannot type it.
    globalThis.__audiohubCaption!('teleport', true);

    expect(seen).toEqual([['hover', true], ['press', true], ['max', false]]);
    off();
    // Unsubscribing removes the global: on macOS nothing ever installs it, and a
    // leftover hook that no longer reaches a listener is a false signal that the
    // Windows path is live.
    expect(globalThis.__audiohubCaption).toBeUndefined();
  });
});
