import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { navMotion, shouldAnimateNav } from './viewTransition';

describe('navDirection follows the nav pill left to right', () => {
  // The pill renders 主面板 | 共享协议 | 统计诊断 | 设置. Clicking a tab to the
  // right must bring content in from the right (+1); to the left, from the
  // left (-1). A wrong sign here is worse than no animation: content sliding
  // in from the side you did NOT move toward reads as the app disagreeing
  // with your own hand.
  it('slides horizontally, forward, when moving right along the pill', () => {
    expect(navMotion('peers', 'stats')).toEqual({ dx: 1, dy: 0 });
    expect(navMotion('peers', 'settings')).toEqual({ dx: 1, dy: 0 });
    expect(navMotion('share', 'stats')).toEqual({ dx: 1, dy: 0 });
    expect(navMotion('stats', 'settings')).toEqual({ dx: 1, dy: 0 });
  });

  it('slides horizontally, backward, when moving left along the pill', () => {
    expect(navMotion('settings', 'peers')).toEqual({ dx: -1, dy: 0 });
    expect(navMotion('stats', 'share')).toEqual({ dx: -1, dy: 0 });
    expect(navMotion('settings', 'stats')).toEqual({ dx: -1, dy: 0 });
  });

  // detail is not on the pill; it is the peers view's child, and drilling into
  // it is a different gesture from moving along the pill — so it gets a
  // different AXIS, not just a different sign (user ruling, 2026-08-15). The
  // detail rises from the bottom like a layer lifted onto the list, and sinks
  // back down on exit regardless of what comes next, because the user's mental
  // motion is "out of the detail", not "over to settings".
  it('lifts the detail view in from the bottom and drops it back out', () => {
    expect(navMotion('peers', 'detail')).toEqual({ dx: 0, dy: 1 });
    expect(navMotion('detail', 'peers')).toEqual({ dx: 0, dy: -1 });
    expect(navMotion('detail', 'settings')).toEqual({ dx: 0, dy: -1 });
  });

  it('defaults to horizontal-forward for anything it cannot place', () => {
    expect(navMotion('nonsense', 'peers')).toEqual({ dx: 1, dy: 0 });
    expect(navMotion('peers', 'nonsense')).toEqual({ dx: 1, dy: 0 });
  });

  // NAV_ORDER is written out inside viewTransition.ts rather than imported
  // from Chrome.tsx (importing would drag React into a DOM-free module), so
  // the two copies can drift. This pins them: the literals in Chrome.tsx's
  // navEntries must appear in exactly the order viewTransition assumes.
  it('matches the order navEntries() actually renders', () => {
    const chrome = readFileSync(
      fileURLToPath(new URL('../components/Chrome.tsx', import.meta.url)),
      'utf8',
    );
    const views = [...chrome.matchAll(/view:\s*'(\w+)'/g)].map((m) => m[1]);
    // NAV_BASE lists peers/stats/settings; NAV_SHARE ('share') is spliced in
    // at index 1 by navEntries(). Reconstruct the rendered order the same way.
    const base = views.filter((v) => v !== 'share');
    const rendered = [base[0], 'share', ...base.slice(1)];
    expect(rendered).toEqual(['peers', 'share', 'stats', 'settings']);
  });
});

describe('shouldAnimateNav', () => {
  it('declines while another transition is in flight', () => {
    // A second startViewTransition skips the first mid-frame; the theme reveal
    // is a transition too, and overlapping them produced a half-drawn circle
    // over a half-swapped page.
    expect(shouldAnimateNav(true, false, true)).toBe(false);
  });

  it('declines under reduced motion and without the API', () => {
    expect(shouldAnimateNav(true, true, false)).toBe(false);
    expect(shouldAnimateNav(false, false, false)).toBe(false);
  });

  it('animates when supported, motion is allowed and nothing is running', () => {
    expect(shouldAnimateNav(true, false, false)).toBe(true);
  });
});
