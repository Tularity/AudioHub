// Page-to-page transitions, through the View Transitions API.
//
// # Why the API rather than a CSS animation on the incoming view
//
// `App.tsx` renders the active view with `key={view}`, so React unmounts the old
// tree **synchronously** the moment the route changes. Measured 2026-08-15: at
// t=11ms the outgoing view was already absent from the DOM. There is nothing
// left to animate out, which is why a page switch read as a hard cut no matter
// what curve the incoming `viewIn` used -- the exit did not exist.
//
// Keeping the old tree mounted to animate it is the alternative, and it is a bad
// one here: the views hold live subscriptions (`Peers` renders 1 Hz metrics,
// `Stats` holds sparkline buffers), so a lingering copy keeps re-rendering
// against the same store and doubles the work at exactly the moment the app is
// busiest. `document.startViewTransition()` avoids that entirely -- the
// compositor keeps a *snapshot* of the old frame, not a live tree.
//
// The app already depends on this API for the theme reveal (`lib/reveal.ts`),
// so this adds no new platform requirement: Chrome/Edge 111+, WebKit 18+, which
// covers both shells this ships in.
//
// # The `data-vt` marker
//
// Both features animate the same two pseudo-elements, and they want opposite
// things: the theme reveal turns the default cross-fade OFF (it draws its own
// expanding circle over an otherwise untouched frame), while navigation wants a
// cross-fade and nothing else. A marker on `<html>` lets the stylesheet tell
// them apart; without it the reveal's `animation: none` would silently kill the
// navigation animation, which fails as "the page still cuts".

/** Value of `<html data-vt>` while a navigation transition is running. */
export const VT_NAV = 'nav';
/** Value of `<html data-vt>` while the theme reveal is running. */
export const VT_THEME = 'theme';

/**
 * Which way the content slides, derived from where the views sit **on the nav
 * pill**. The pill is a horizontal row — 主面板 | 共享协议 | 统计诊断 | 设置 —
 * so clicking a tab to the right means the content comes in from the right and
 * the old content yields to the left, the same direction the pill's own marker
 * slides. Direction here is spatial fact, not an invented hierarchy: it must
 * match `navEntries()` in `components/Chrome.tsx`, which is why the order is
 * written out rather than imported (importing would drag React into this
 * DOM-free module; the test pins the two against each other instead).
 *
 * `detail` is not on the pill — it is the peers view's child. Entering it is a
 * drill-in (always forward), leaving it is a step back (always backward),
 * regardless of which view comes next.
 */
const NAV_ORDER: readonly string[] = ['peers', 'share', 'stats', 'settings'];

/**
 * Unit direction of the incoming content, one axis at a time.
 * `dx: 1` = from the right, `dx: -1` = from the left;
 * `dy: 1` = from the bottom, `dy: -1` = from the top.
 */
export interface NavMotion { dx: 1 | -1 | 0; dy: 1 | -1 | 0 }

export function navMotion(from: string, to: string): NavMotion {
  // Drilling into a card is a different gesture from moving along the pill,
  // and it gets a different axis: the detail rises from the bottom like a
  // layer being lifted onto the peers view, and sinks back down on exit while
  // the main panel returns from above. Horizontal would claim the detail sits
  // *beside* peers on the pill, which it does not (user ruling, 2026-08-15).
  if (to === 'detail') return { dx: 0, dy: 1 };
  if (from === 'detail') return { dx: 0, dy: -1 };
  const a = NAV_ORDER.indexOf(from);
  const b = NAV_ORDER.indexOf(to);
  // An unknown view has no position to reason from; forward is the neutral
  // reading ("something new arrived") and never produces a wrong-way slide,
  // only an unexciting one.
  if (a < 0 || b < 0) return { dx: 1, dy: 0 };
  return { dx: b >= a ? 1 : -1, dy: 0 };
}

interface ViewTransitionCapable {
  startViewTransition?: (cb: () => void) => { ready: Promise<void>; finished: Promise<void> };
}

export function supportsViewTransitions(doc: Document): boolean {
  return typeof (doc as Document & ViewTransitionCapable).startViewTransition === 'function';
}

export function prefersReducedMotion(win: Window): boolean {
  try {
    return !!win.matchMedia && win.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

/**
 * Whether a route change should be animated.
 *
 * Split out as a pure function of the three facts that decide it so the policy
 * is testable. `busy` is the one that is easy to miss: a second
 * `startViewTransition` while one is in flight skips the first, and the theme
 * reveal is a transition too. Overlapping them produced a half-drawn circle
 * over a half-swapped page, so navigation simply declines while one is running.
 */
export function shouldAnimateNav(
  supported: boolean,
  reducedMotion: boolean,
  busy: boolean,
): boolean {
  return supported && !reducedMotion && !busy;
}

/**
 * Run `apply` -- which must synchronously mutate the route -- and slide the old
 * frame out and the new one in where possible.
 *
 * `motion` is the result of {@link navMotion}. Its two unit signs are written
 * to `--nav-dx` / `--nav-dy` on `<html>` **before** `startViewTransition`, so
 * the keyframes (which multiply their travel by them) already see the right
 * direction when the snapshots are taken.
 *
 * `apply` runs exactly once on every path, including all the failure ones, so
 * callers never need their own fallback.
 */
export function navigateWithTransition(apply: () => void, motion: NavMotion = { dx: 1, dy: 0 }): void {
  const doc = typeof document === 'undefined' ? null : document;
  const win = typeof window === 'undefined' ? null : window;
  const busy = !!doc && !!doc.documentElement.dataset.vt;
  if (!doc || !win
    || !shouldAnimateNav(supportsViewTransitions(doc), prefersReducedMotion(win), busy)) {
    apply();
    return;
  }

  const start = (doc as Document & ViewTransitionCapable).startViewTransition;
  doc.documentElement.dataset.vt = VT_NAV;
  doc.documentElement.style.setProperty('--nav-dx', String(motion.dx));
  doc.documentElement.style.setProperty('--nav-dy', String(motion.dy));
  // `start` is a method on the document; calling it detached loses `this`.
  const transition = start!.call(doc, apply);
  const clear = () => {
    // Only clear our own marker: a theme reveal that began after us owns it now.
    if (doc.documentElement.dataset.vt === VT_NAV) delete doc.documentElement.dataset.vt;
    doc.documentElement.style.removeProperty('--nav-dx');
    doc.documentElement.style.removeProperty('--nav-dy');
  };
  transition.finished.then(clear, clear);
}
