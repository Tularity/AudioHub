// Where the user's last press landed, so panels can grow out of it.
//
// # Why a global listener and not a prop
//
// A Sheet is opened from seven different call sites, three of which pass an
// inline arrow and none of which has the event object in hand by the time the
// panel mounts — the state flip and the mount are separate turns. Threading a
// coordinate through every one of them is seven chances to forget, and the one
// that forgets does not break, it just animates from the middle of the screen
// like it always did. A single capture-phase listener on `document` is one
// place to get right.
//
// # Why `pointerdown` and why it expires
//
// `pointerdown` fires before the state flip, so by mount time the coordinate is
// already recorded. It expires because a stale one is worse than none: open a
// panel from the keyboard ten seconds after clicking something in a corner and
// the panel would fly out of that corner for no reason the user can see.

export interface Point { x: number; y: number }

export interface Rect { left: number; top: number; width: number; height: number }

/** How long a press stays eligible to be an animation origin. */
export const ORIGIN_MAX_AGE_MS = 800;

/**
 * How far outside the panel the origin may sit, as a fraction of the panel's
 * own size.
 *
 * A press near the panel's eventual position gives a natural "it grew from
 * there". A press in the far corner of a 1440px window would put the origin at
 * something like -400%, and scaling about a point that distant is not an
 * expansion any more — it is the panel flying in from off-screen. Clamping
 * keeps the gesture readable while still pointing at the right side of the
 * screen.
 */
export const ORIGIN_CLAMP = 1.5;

let last: Point | null = null;
let lastAt = -Infinity;

/** Record a press. Exported for tests; production goes through the installer. */
export function notePointerOrigin(p: Point, now: number): void {
  last = p;
  lastAt = now;
}

/** Forget any recorded press. Tests use it; so does a locale/theme swap. */
export function clearPointerOrigin(): void {
  last = null;
  lastAt = -Infinity;
}

/** The last press, if it is recent enough to still explain what is opening. */
export function recentPointerOrigin(now: number, maxAgeMs = ORIGIN_MAX_AGE_MS): Point | null {
  if (!last) return null;
  return now - lastAt <= maxAgeMs ? last : null;
}

/**
 * Express `p` as a `transform-origin` for a box at `rect`, in percent.
 *
 * Percentages rather than pixels so the value survives the panel being resized
 * or re-laid-out mid-animation (`max-height` plus `overflow-y:auto` means the
 * card's height depends on its content, which can settle a frame late).
 *
 * A zero-sized rect yields the centre: dividing by it would produce Infinity,
 * and an element with no box has no meaningful interior point anyway.
 */
export function originPercent(rect: Rect, p: Point): { ox: number; oy: number } {
  if (!(rect.width > 0) || !(rect.height > 0)) return { ox: 50, oy: 50 };
  const clamp = (v: number): number => Math.max(-ORIGIN_CLAMP * 100, Math.min((1 + ORIGIN_CLAMP) * 100, v));
  return {
    ox: clamp(((p.x - rect.left) / rect.width) * 100),
    oy: clamp(((p.y - rect.top) / rect.height) * 100),
  };
}

/** The bits of `HTMLElement` {@link layoutRect} needs, so it can be tested. */
export interface LayoutBox {
  offsetLeft: number;
  offsetTop: number;
  offsetWidth: number;
  offsetHeight: number;
  offsetParent: { getBoundingClientRect(): Rect; clientLeft: number; clientTop: number } | null;
}

/**
 * The element's LAYOUT box in viewport coordinates — what it would occupy with
 * no transform on it.
 *
 * ⚠ Do NOT use `getBoundingClientRect()` for this. It reports the *transformed*
 * box, and `.sheet-card` carries `animation: sheetGrow … backwards` whose `from`
 * keyframe is `scale(.72)`. `backwards` puts that scale in force during the
 * animation's delay — from the moment the element exists — and both engines
 * have resolved it by the first forced layout. Measured in Chromium on
 * 2026-08-15: the layout effect saw a 403.2px-wide box for a card whose layout
 * width is 560, i.e. exactly .72.
 *
 * Feeding that box to {@link originPercent} yields `(ox_true - 14) / .72`. The
 * centre maps to itself — which is why a dialog opened from mid-screen looked
 * right and hid this for months — but every other point is pushed about 1.39×
 * further out. For the「Add peer」button in a 1000px window the origin landed at
 * 1078px: 162px past the button and outside the window. On close the card then
 * shrank toward a point it could never reach and was still half size as it swept
 * over the button, which is the symptom the user reported (2026-08-15).
 *
 * The `offset*` family is used because it is the only geometry the layout
 * engine exposes that transforms cannot touch, and reading it mutates nothing.
 * The obvious alternative — blank `animation-name`, measure, put it back — also
 * works, but only before the first paint: restoring the property restarts the
 * animation, so it cannot be used to re-measure a card that is already on
 * screen, which is exactly what closing has to do.
 *
 * `offsetLeft` is measured from the offset parent's PADDING edge while
 * `getBoundingClientRect()` returns its BORDER box, so the parent's border
 * (`clientLeft`/`clientTop`) closes the gap. It is zero for today's
 * `.sheet-scrim`; it is here so a border added later cannot silently skew the
 * origin by its width.
 */
export function layoutRect(el: LayoutBox): Rect {
  const parent = el.offsetParent;
  const base = parent
    ? { left: parent.getBoundingClientRect().left + parent.clientLeft,
        top: parent.getBoundingClientRect().top + parent.clientTop }
    : { left: 0, top: 0 };
  return {
    left: base.left + el.offsetLeft,
    top: base.top + el.offsetTop,
    width: el.offsetWidth,
    height: el.offsetHeight,
  };
}

interface OriginTarget {
  addEventListener(type: string, handler: (e: PointerEvent) => void, options?: unknown): void;
  removeEventListener(type: string, handler: (e: PointerEvent) => void, options?: unknown): void;
}

/**
 * Start recording presses. Returns the uninstaller.
 *
 * Capture phase: a handler that stops propagation somewhere in the tree must
 * not be able to hide the press from us, and we never act on the event.
 */
export function installPointerOrigin(
  target: OriginTarget | null | undefined,
  now: () => number = () => Date.now(),
): () => void {
  if (!target) return () => {};
  const handler = (e: PointerEvent) => {
    // Synthetic clicks (keyboard Space/Enter on a button) arrive with no real
    // coordinates. Recording 0,0 would animate every keyboard-opened panel out
    // of the top-left corner.
    if (!Number.isFinite(e.clientX) || !Number.isFinite(e.clientY)) return;
    if (e.clientX === 0 && e.clientY === 0) return;
    notePointerOrigin({ x: e.clientX, y: e.clientY }, now());
  };
  target.addEventListener('pointerdown', handler, true);
  return () => target.removeEventListener('pointerdown', handler, true);
}
