// The two decisions in the circular reveal that can be wrong without looking
// wrong on the machine it was written on: how big the circle has to get, and
// whether to draw it at all.
//
// The radius bug is the classic one -- take the *nearest* corner and the window
// looks fully covered from a centred button, then leaves a wedge of the old
// theme in one corner the moment the button sits near an edge, which is exactly
// where this button sits.

import { describe, it, expect } from 'vitest';
import { originOfElement, revealRadius, shouldAnimateReveal } from './reveal';

const W = 1200;
const H = 800;

/** Every corner of the viewport, which is what the circle has to swallow. */
const CORNERS = [
  { x: 0, y: 0 }, { x: W, y: 0 }, { x: 0, y: H }, { x: W, y: H },
];

function covers(origin: { x: number; y: number }, r: number): boolean {
  return CORNERS.every((c) => Math.hypot(c.x - origin.x, c.y - origin.y) <= r + 1e-9);
}

describe('reveal radius', () => {
  it('reaches the far corner from the centre', () => {
    const r = revealRadius({ x: W / 2, y: H / 2 }, W, H);
    expect(r).toBeCloseTo(Math.hypot(W / 2, H / 2), 6);
  });

  it('spans the full diagonal from a corner', () => {
    expect(revealRadius({ x: 0, y: 0 }, W, H)).toBeCloseTo(Math.hypot(W, H), 6);
    expect(revealRadius({ x: W, y: H }, W, H)).toBeCloseTo(Math.hypot(W, H), 6);
  });

  it('covers all four corners wherever the button is', () => {
    // Including the two places it actually is: near the top-right (macOS) and
    // near the top-left (Windows).
    const origins = [
      { x: W - 34, y: 33 }, { x: 34, y: 33 },
      { x: W / 2, y: H / 2 }, { x: 1, y: H - 1 }, { x: 0, y: 0 },
    ];
    for (const o of origins) expect(covers(o, revealRadius(o, W, H))).toBe(true);
  });

  it('would not cover the window if it used the nearest corner instead', () => {
    // Guards the direction of the Math.max: this is the assertion that goes red
    // if `Math.max` is ever "simplified" to `Math.min`.
    const o = { x: W - 34, y: 33 };
    const nearest = Math.hypot(Math.min(o.x, W - o.x), Math.min(o.y, H - o.y));
    expect(covers(o, nearest)).toBe(false);
    expect(revealRadius(o, W, H)).toBeGreaterThan(nearest);
  });

  it('still returns something usable for a zero-sized window', () => {
    expect(revealRadius({ x: 0, y: 0 }, 0, 0)).toBe(0);
  });
});

describe('whether to animate', () => {
  it('needs the API', () => {
    expect(shouldAnimateReveal(false, false)).toBe(false);
  });

  it('lets reduced motion win over having the API', () => {
    // Not a shorter animation -- none. A disc sweeping the whole window is the
    // large-area motion the preference is asking not to see, and the theme
    // change itself is not motion.
    expect(shouldAnimateReveal(true, true)).toBe(false);
  });

  it('animates only when supported and motion is wanted', () => {
    expect(shouldAnimateReveal(true, false)).toBe(true);
  });
});

describe('origin of an element', () => {
  it('is the element centre in viewport coordinates', () => {
    const el = {
      getBoundingClientRect: () => ({ left: 100, top: 20, width: 28, height: 28 }),
    } as unknown as Element;
    expect(originOfElement(el)).toEqual({ x: 114, y: 34 });
  });

  it('is null when there is no element', () => {
    // The ref is null on the first render and after unmount; `revealSwap`
    // treats null as "no animation" rather than defaulting to (0, 0), which
    // would send the circle out of the corner instead of the button.
    expect(originOfElement(null)).toBeNull();
  });
});
