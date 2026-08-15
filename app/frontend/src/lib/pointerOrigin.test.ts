import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  clearPointerOrigin, installPointerOrigin, layoutRect, notePointerOrigin,
  originPercent, recentPointerOrigin, ORIGIN_MAX_AGE_MS,
} from './pointerOrigin';

beforeEach(() => clearPointerOrigin());

describe('the recorded press expires', () => {
  it('is available immediately after the press', () => {
    notePointerOrigin({ x: 120, y: 40 }, 1000);
    expect(recentPointerOrigin(1000)).toEqual({ x: 120, y: 40 });
    expect(recentPointerOrigin(1000 + ORIGIN_MAX_AGE_MS)).toEqual({ x: 120, y: 40 });
  });

  // A stale coordinate is worse than none: open a panel from the keyboard ten
  // seconds after clicking a corner and it would fly out of that corner for no
  // reason the user can connect to anything.
  it('is gone once it is older than the window', () => {
    notePointerOrigin({ x: 120, y: 40 }, 1000);
    expect(recentPointerOrigin(1001 + ORIGIN_MAX_AGE_MS)).toBeNull();
  });

  it('reports nothing before any press', () => {
    expect(recentPointerOrigin(5000)).toBeNull();
  });
});

describe('originPercent maps a viewport point into the panel box', () => {
  const rect = { left: 100, top: 100, width: 200, height: 100 };

  it('puts a press at the centre at 50/50', () => {
    expect(originPercent(rect, { x: 200, y: 150 })).toEqual({ ox: 50, oy: 50 });
  });

  it('puts a press at a corner at that corner', () => {
    expect(originPercent(rect, { x: 100, y: 100 })).toEqual({ ox: 0, oy: 0 });
    expect(originPercent(rect, { x: 300, y: 200 })).toEqual({ ox: 100, oy: 100 });
  });

  it('goes negative for a press above and left of the panel', () => {
    expect(originPercent(rect, { x: 0, y: 50 })).toEqual({ ox: -50, oy: -50 });
  });

  // Scaling about a point hundreds of percent away is not an expansion any
  // more, it is the panel flying in from off-screen.
  it('clamps an origin that is absurdly far away', () => {
    const far = originPercent(rect, { x: -100000, y: 100000 });
    expect(far.ox).toBe(-150);
    expect(far.oy).toBe(250);
  });

  it('falls back to the centre for a zero-sized box', () => {
    expect(originPercent({ left: 0, top: 0, width: 0, height: 0 }, { x: 9, y: 9 }))
      .toEqual({ ox: 50, oy: 50 });
  });
});

describe('layoutRect reports the box a transform cannot touch', () => {
  /** A stand-in for the card. `offset*` is layout geometry, so a scaled card
   *  reports the same numbers a settled one does — that is the whole point. */
  function card(over: Partial<{
    offsetLeft: number; offsetTop: number; offsetWidth: number; offsetHeight: number;
    parent: { left: number; top: number; borderLeft: number; borderTop: number } | null;
  }> = {}) {
    const parent = over.parent === undefined
      ? { left: 0, top: 0, borderLeft: 0, borderTop: 0 }
      : over.parent;
    return {
      offsetLeft: over.offsetLeft ?? 220,
      offsetTop: over.offsetTop ?? 57,
      offsetWidth: over.offsetWidth ?? 560,
      offsetHeight: over.offsetHeight ?? 586,
      offsetParent: parent && {
        getBoundingClientRect: () => ({ left: parent.left, top: parent.top, width: 0, height: 0 }),
        clientLeft: parent.borderLeft,
        clientTop: parent.borderTop,
      },
    };
  }

  it('places the box relative to the offset parent', () => {
    expect(layoutRect(card())).toEqual({ left: 220, top: 57, width: 560, height: 586 });
  });

  it('follows an offset parent that is not at the viewport origin', () => {
    const r = layoutRect(card({ parent: { left: 30, top: 40, borderLeft: 0, borderTop: 0 } }));
    expect(r.left).toBe(250);
    expect(r.top).toBe(97);
  });

  // `offsetLeft` counts from the parent's padding edge, `getBoundingClientRect`
  // from its border box. Today `.sheet-scrim` has no border; this keeps a
  // border added later from silently skewing the origin by its width.
  it('closes the gap left by a border on the offset parent', () => {
    const r = layoutRect(card({ parent: { left: 0, top: 0, borderLeft: 4, borderTop: 6 } }));
    expect(r.left).toBe(224);
    expect(r.top).toBe(63);
  });

  it('treats a detached element as sitting at the viewport origin', () => {
    expect(layoutRect(card({ parent: null }))).toEqual({ left: 220, top: 57, width: 560, height: 586 });
  });

  // The bug this exists to stop, in the numbers it was measured with (Chromium,
  // 2026-08-15): the「Add peer」button centred at x=916.37 in a 1000px window,
  // a 560px card whose `backwards` fill had already applied scale(.72) by the
  // time the layout effect ran, so getBoundingClientRect reported 403.2 wide.
  it('keeps the origin on the button a scaled measurement overshoots', () => {
    const press = { x: 916.37, y: 102.25 };
    const rendered = { left: 298.4, top: 139.05, width: 403.2, height: 421.91 };
    const layout = layoutRect(card());
    // 153.27% of 560 past the card's left edge is x=1078 — 162px beyond the
    // button and outside the window entirely.
    expect(originPercent(rendered, press).ox).toBeCloseTo(153.27, 1);
    expect(originPercent(layout, press).ox).toBeCloseTo(124.35, 1);
    expect(layout.left + (originPercent(layout, press).ox / 100) * layout.width)
      .toBeCloseTo(press.x, 1);
  });

  // The centre is the one point the scaling error leaves alone, which is why a
  // dialog opened from mid-screen looked right and hid this for so long.
  it('agrees with the broken measurement only at the centre', () => {
    const centre = { x: 500, y: 350 };
    const rendered = { left: 298.4, top: 139.05, width: 403.2, height: 421.91 };
    expect(originPercent(rendered, centre).ox).toBeCloseTo(50, 0);
    expect(originPercent(layoutRect(card()), centre).ox).toBeCloseTo(50, 0);
  });

  // Why the exit re-computes instead of reusing the mount-time percentage: the
  // card grows as late content lands, and because it is flex-centred that moves
  // its top edge too. Measured: 484px tall at mount, 586px settled.
  it('gives a different percentage once the card has settled taller', () => {
    const press = { x: 916.37, y: 102.25 };
    const atMount = layoutRect(card({ offsetHeight: 484, offsetTop: (700 - 484) / 2 }));
    const settled = layoutRect(card({ offsetHeight: 586, offsetTop: (700 - 586) / 2 }));
    expect(originPercent(atMount, press).oy).toBeCloseTo(-1.19, 1);
    expect(originPercent(settled, press).oy).toBeCloseTo(7.72, 1);
    // Reusing the mount-time percentage against the settled box lands 52px high.
    expect(settled.top + (originPercent(atMount, press).oy / 100) * settled.height)
      .toBeCloseTo(50.0, 0);
    expect(settled.top + (originPercent(settled, press).oy / 100) * settled.height)
      .toBeCloseTo(press.y, 0);
  });
});

describe('the installer', () => {
  function fakeTarget() {
    const handlers: ((e: unknown) => void)[] = [];
    return {
      handlers,
      addEventListener: vi.fn((_t: string, h: (e: unknown) => void) => { handlers.push(h); }),
      removeEventListener: vi.fn(),
    };
  }

  it('listens in the capture phase so a stopPropagation cannot hide the press', () => {
    const target = fakeTarget();
    installPointerOrigin(target as never);
    expect(target.addEventListener).toHaveBeenCalledWith('pointerdown', expect.any(Function), true);
  });

  it('records a real press and releases the listener', () => {
    const target = fakeTarget();
    const stop = installPointerOrigin(target as never, () => 500);
    target.handlers[0]({ clientX: 7, clientY: 9 });
    expect(recentPointerOrigin(500)).toEqual({ x: 7, y: 9 });
    stop();
    expect(target.removeEventListener).toHaveBeenCalled();
  });

  // Space/Enter on a focused button produces a click with no real coordinates.
  // Recording 0,0 would animate every keyboard-opened panel out of the very
  // top-left corner of the window.
  it('ignores a synthetic press with no coordinates', () => {
    const target = fakeTarget();
    installPointerOrigin(target as never, () => 500);
    target.handlers[0]({ clientX: 0, clientY: 0 });
    expect(recentPointerOrigin(500)).toBeNull();
    target.handlers[0]({ clientX: NaN, clientY: 3 });
    expect(recentPointerOrigin(500)).toBeNull();
  });

  it('is a no-op with no target', () => {
    expect(() => installPointerOrigin(null)()).not.toThrow();
  });
});
