import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  clearPointerOrigin, installPointerOrigin, notePointerOrigin,
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
