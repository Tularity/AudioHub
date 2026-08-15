import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';
import { sheetEscapeCloses, trapIndex, FOCUSABLE_SELECTOR, SHEET_EXIT_MS } from './sheet';

describe('escape belongs to the innermost thing that is open', () => {
  it('closes the sheet when nothing inner claims the key', () => {
    expect(sheetEscapeCloses({ recording: false, confirmOpen: false })).toBe(true);
  });

  it('yields to a shortcut row that is recording', () => {
    // The row registers its capture listener after the sheet did, so listener
    // order alone would let the sheet close first and take the recorder with it.
    expect(sheetEscapeCloses({ recording: true, confirmOpen: false })).toBe(false);
  });

  it('yields to a confirm dialog opened on top of it', () => {
    // Reset-fingerprint and unpair both put a confirm above a sheet. Escape has
    // to cancel the dangerous action only, not navigate the user off the panel.
    expect(sheetEscapeCloses({ recording: false, confirmOpen: true })).toBe(false);
  });

  it('yields when both inner claims are live', () => {
    expect(sheetEscapeCloses({ recording: true, confirmOpen: true })).toBe(false);
  });
});

describe('the focus trap wraps at both ends', () => {
  it('walks forward and wraps past the last element', () => {
    expect(trapIndex(3, 0, false)).toBe(1);
    expect(trapIndex(3, 1, false)).toBe(2);
    expect(trapIndex(3, 2, false)).toBe(0);
  });

  it('walks backward and wraps past the first element', () => {
    expect(trapIndex(3, 2, true)).toBe(1);
    expect(trapIndex(3, 1, true)).toBe(0);
    expect(trapIndex(3, 0, true)).toBe(2);
  });

  it('pulls focus back inside when it is not in the trap', () => {
    // -1 is "focus is on <body> or on something the scrim covers". Staying put
    // would be a leak: the next Tab reaches a control behind the scrim.
    expect(trapIndex(3, -1, false)).toBe(0);
    expect(trapIndex(3, -1, true)).toBe(2);
    expect(trapIndex(3, 9, false)).toBe(0);
  });

  it('reports no target when the card holds nothing focusable', () => {
    expect(trapIndex(0, -1, false)).toBe(-1);
    expect(trapIndex(0, 0, true)).toBe(-1);
  });

  it('has a single focusable element trap on itself', () => {
    expect(trapIndex(1, 0, false)).toBe(0);
    expect(trapIndex(1, 0, true)).toBe(0);
  });
});

describe('the focusable selector', () => {
  it('excludes hidden rows in every clause', () => {
    // Whole rows are collapsed with `hidden` all over this app rather than being
    // left out of the tree. A trap that counts them stops on nothing visible.
    const clauses = FOCUSABLE_SELECTOR.split(', ');
    expect(clauses.length).toBeGreaterThan(4);
    for (const c of clauses) expect(c.endsWith(':not([hidden])')).toBe(true);
  });

  it('excludes disabled controls and tabindex -1', () => {
    expect(FOCUSABLE_SELECTOR).toContain('button:not([disabled])');
    expect(FOCUSABLE_SELECTOR).toContain('[tabindex]:not([tabindex="-1"])');
  });
});

// The mount effect's dependency array, as an assertion.
//
// `Sheet` opens by moving focus into the card and closes by handing focus back
// to whatever opened it. Both live in one `useEffect`. If that effect ever
// lists `onClose` as a dependency, a caller passing an inline arrow -- three of
// the seven call sites end up with one, and it is the obvious way to write it --
// makes the effect tear down and re-run on **every render**: focus goes back to
// the opener, then straight into the card's first focusable. `BeDiscoveredSheet`
// re-renders at 1 Hz while the pairing countdown runs, so it yanks focus to the
// `?` beside its title once a second and the 停止配对 button cannot be reached
// with the keyboard at all.
//
// This is a wiring bug, not a decision, so there is no pure function to pin it
// to; and vitest runs with `environment: 'node'` on purpose, so it cannot be
// caught by mounting the component. Reading the source is what is left. The
// same trick guards the stylesheet in `lightTheme.test.ts`.
describe('the sheet does not re-run its focus effect on every render', () => {
  const SRC = readFileSync(
    fileURLToPath(new URL('../components/Sheet.tsx', import.meta.url)),
    'utf8',
  );

  it('closes over onClose through a ref instead of reading it in the effect', () => {
    expect(SRC).toContain('closeRef.current()');
    // The prop itself must not be called from inside the effect -- that is what
    // would force it into the dependency array.
    expect(SRC).not.toMatch(/\n\s+onClose\(\);/);
  });

  it('mounts its key handler and focus trap exactly once', () => {
    // The effect that adds the keydown listener must end with an empty array.
    const effect = SRC.slice(SRC.indexOf('const opener'));
    const deps = effect.slice(effect.indexOf('}, ['), effect.indexOf('}, [') + 7);
    expect(deps).toBe('}, []);');
  });
});

// The exit animation's duration lives in two places that cannot import each
// other: a TS constant that delays the unmount, and a CSS animation. React
// unmounts synchronously, so the delay is the only thing keeping the card on
// screen long enough to be animated at all.
//
// Drift is silent and asymmetric. Too short and the panel is cut off
// mid-shrink; too long and it is invisible but still intercepting nothing for
// the remainder, which reads as a stuck frame. Reading the stylesheet is the
// same trick `lightTheme.test.ts` and `peerMetricsAlignment.test.ts` use.
describe('the sheet exit delay matches its stylesheet', () => {
  const CSS = readFileSync(
    fileURLToPath(new URL('../styles.css', import.meta.url)),
    'utf8',
  );

  /** `:root` value of a custom property, so a `var()` delay can be resolved. */
  function token(name: string): string {
    const m = CSS.replace(/\/\*[\s\S]*?\*\//g, '')
      .match(new RegExp(`(?:^|;|\\{)\\s*${name}\\s*:([^;}]*)`));
    expect(m, `${name} must be defined on :root`).toBeTruthy();
    return m![1].trim();
  }

  /**
   * Total wall time of the animation shorthand: **delay + duration**.
   *
   * Summing rather than reading the first number is the whole point. React
   * unmounts on a fixed `SHEET_EXIT_MS` timer, so what has to match is when the
   * animation *ends*, not how long it runs. The scrim deliberately starts late
   * (`--sheet-trail`) so the card has a still backdrop to leave against —
   * reading its duration alone would call that a mismatch when it is exactly
   * right, and, worse, would happily accept a delay that pushes the fade off
   * the end of the unmount, which is the bug this pair exists to prevent.
   */
  function endsAtMs(selector: string): number {
    const esc = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const rule = CSS.match(new RegExp(`${esc}\\s*\\{([^}]*)\\}`));
    expect(rule, `${selector} must exist in styles.css`).toBeTruthy();
    const anim = rule![1].match(/animation:([^;]*)/);
    expect(anim, `${selector} must declare an animation`).toBeTruthy();
    const resolved = anim![1].replace(/var\((--[\w-]+)\)/g, (_, n: string) => token(n));
    const times = [...resolved.matchAll(/(?<![\w-])(\.?\d+(?:\.\d+)?)(ms|s)(?![\w-])/g)]
      .map((m) => (m[2] === 'ms' ? Number(m[1]) : Number(m[1]) * 1000));
    expect(times.length, `${selector} must declare a duration`).toBeGreaterThan(0);
    return Math.round(times.reduce((a, b) => a + b, 0));
  }

  // The card leaves first, the scrim lifts after it. Both must fit inside the
  // unmount; the scrim is the one that reaches it.
  it('never lets the card run past the unmount', () => {
    expect(endsAtMs('.sheet-scrim.closing .sheet-card')).toBeLessThanOrEqual(SHEET_EXIT_MS);
  });

  it('finishes the scrim fade exactly as React unmounts', () => {
    expect(endsAtMs('.sheet-scrim.closing')).toBe(SHEET_EXIT_MS);
  });

  it('lets the card finish before the scrim starts to lift', () => {
    // The ordering IS the fix for "it disappears halfway": a dark card on a dark
    // app has no contrast, so the scrim has to outlast the card's travel.
    expect(endsAtMs('.sheet-scrim.closing .sheet-card'))
      .toBeLessThan(endsAtMs('.sheet-scrim.closing'));
  });

  // The entrance is what has to be *seen*; the previous `viewIn` scaled from
  // .994, a 0.6% move that no easing curve could make legible. Guarding the
  // magnitude keeps a later "tidy-up" from quietly flattening it again.
  it('enters with a scale change big enough for the curve to show', () => {
    const from = CSS.match(/@keyframes sheetGrow \{\s*from \{[^}]*scale\(([\d.]+)\)/);
    expect(from, 'sheetGrow must exist').toBeTruthy();
    expect(parseFloat(from![1])).toBeLessThanOrEqual(0.85);
  });

  it('grows from the pressed point rather than always from the centre', () => {
    expect(CSS).toContain('transform-origin: var(--sheet-ox, 50%) var(--sheet-oy, 50%)');
  });
});
