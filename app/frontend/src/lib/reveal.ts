// Circular reveal: the new theme spreads out of the button that was pressed
// until it covers the window.
//
// # Why the View Transitions API and not a hand-rolled overlay
//
// The obvious DIY version -- screenshot the old look into an absolutely
// positioned layer, swap the theme underneath, animate a `clip-path` on the
// layer -- cannot actually screenshot anything from script. What people build
// instead is a solid-colour disc that grows over the page, which reads as "a
// coloured circle wiped across the screen", not as "the interface changed".
// `document.startViewTransition()` hands the compositor a real snapshot of the
// old frame and a real snapshot of the new one, so what expands is the *new
// interface*, edge for edge with the old one behind it. That is the effect
// being asked for, and it is the only way to get it.
//
// Recipe follows the Chrome team's write-up (developer.chrome.com,
// "View Transitions API - same-document"): suppress the default cross-fade on
// both root snapshots, then animate `clip-path` on `::view-transition-new(root)`
// through the Web Animations API, because the circle's centre is only known at
// click time and CSS cannot read it.
//
// # Support, checked rather than assumed
//
// Single-document view transitions: Chrome/Edge 111+, Safari/WebKit 18.0+
// (caniuse "view-transitions"). Both of the engines this app actually ships on
// clear that bar -- WKWebView on macOS 26 and a current WebView2 -- but the
// feature detect below is what decides at runtime, and the fallback is a plain
// unanimated swap rather than a broken frame.
//
// # Everything here degrades to "the theme just changes"
//
// Three separate ways out, all landing on the same behaviour: no API, reduced
// motion, or the transition being skipped/rejected mid-flight. The DOM update
// runs in every one of them. A theme toggle that leaves the app mid-animation
// because a promise rejected would be far worse than one that never animates.

/** Where the circle starts, in viewport coordinates. */
export interface RevealOrigin {
  x: number;
  y: number;
}

/** How long the circle takes to cross the window, and its curve. */
export const REVEAL_DURATION_MS = 480;
/**
 * `ease-out`, not the spring used for panels appearing.
 *
 * A wipe that overshoots would have to come back, and there is nothing for the
 * edge of the window to bounce against. Fast at the start, settling at the end,
 * is what reads as "the new surface swept in" -- and 480ms is long enough to be
 * legible on a 1400px-wide window without being something you wait for.
 * `docs/plan.md` §3.1 asks for measured animation, not a demo reel.
 */
export const REVEAL_EASING = 'cubic-bezier(.22, 1, .36, 1)';

/**
 * Radius the circle must reach to cover the viewport: the distance from the
 * origin to whichever corner is furthest away.
 *
 * `Math.max(x, w - x)` picks the further horizontal edge and the same
 * vertically; the hypotenuse of those two legs is the far corner. Taking the
 * nearest corner instead is the classic bug -- it leaves the opposite corner
 * un-revealed and the old theme snaps in at the end.
 */
export function revealRadius(origin: RevealOrigin, w: number, h: number): number {
  const dx = Math.max(origin.x, w - origin.x);
  const dy = Math.max(origin.y, h - origin.y);
  return Math.hypot(dx, dy);
}

/**
 * Whether to animate at all.
 *
 * Split out as a pure function of the two facts that decide it so the policy
 * is testable; `revealSwap` supplies them from the live document.
 *
 * `reducedMotion` wins over capability, and it means *no* animation here
 * rather than a shorter one. The reduced-motion request is usually about
 * vestibular discomfort, and a 1400px disc sweeping across the whole window is
 * precisely the large-area motion it is asking not to see. There is nothing to
 * substitute: the destination state is the entire point, and it arrives
 * instantly.
 */
export function shouldAnimateReveal(supported: boolean, reducedMotion: boolean): boolean {
  return supported && !reducedMotion;
}

/** The element's centre in viewport coordinates -- where the circle starts. */
export function originOfElement(el: Element | null): RevealOrigin | null {
  if (!el || typeof el.getBoundingClientRect !== 'function') return null;
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
}

/** Narrow view of the bits of `Document` this module uses. Keeps the cast local. */
interface ViewTransitionCapable {
  startViewTransition?: (cb: () => void) => { ready: Promise<void>; finished: Promise<void> };
}

function supportsViewTransitions(doc: Document): boolean {
  return typeof (doc as Document & ViewTransitionCapable).startViewTransition === 'function';
}

function prefersReducedMotion(win: Window): boolean {
  try {
    return !!win.matchMedia && win.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

/**
 * Run `apply` -- which must synchronously mutate the DOM into its new state --
 * and, where possible, reveal the result as a circle growing from `origin`.
 *
 * `apply` is called exactly once on every path, including all the failure
 * ones. Callers can rely on that and do not need their own fallback.
 */
export function revealSwap(origin: RevealOrigin | null, apply: () => void): void {
  const doc = typeof document === 'undefined' ? null : document;
  const win = typeof window === 'undefined' ? null : window;
  if (!doc || !win || !origin
    || !shouldAnimateReveal(supportsViewTransitions(doc), prefersReducedMotion(win))) {
    apply();
    return;
  }

  const start = (doc as Document & ViewTransitionCapable).startViewTransition;
  // `start` is a method on the document; calling it detached loses `this`.
  const transition = start!.call(doc, apply);

  const radius = revealRadius(origin, win.innerWidth, win.innerHeight);
  transition.ready.then(() => {
    doc.documentElement.animate(
      {
        clipPath: [
          `circle(0px at ${origin.x}px ${origin.y}px)`,
          `circle(${radius}px at ${origin.x}px ${origin.y}px)`,
        ],
      },
      {
        duration: REVEAL_DURATION_MS,
        easing: REVEAL_EASING,
        pseudoElement: '::view-transition-new(root)',
      },
    );
  }).catch(() => {
    // `ready` rejects when the transition is skipped -- another one started, the
    // tab was hidden mid-flight, the callback threw. The DOM update has already
    // happened (or has failed on its own terms); there is nothing to undo here,
    // and an unhandled rejection in a theme toggle is not worth surfacing.
  });
}
