import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';
import { sheetEscapeCloses, trapIndex, FOCUSABLE_SELECTOR } from './sheet';

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
// lists `onClose` as a dependency, a caller passing an inline arrow -- two of
// the six call sites do, and it is the obvious way to write it -- makes the
// effect tear down and re-run on **every render**: focus goes back to the
// opener, then straight into the card's first focusable. `PairView` re-renders
// at 1 Hz while the pairing countdown runs, so the pairing sheet yanks focus to
// the `?` beside its title once a second and the 停止配对 button cannot be
// reached with the keyboard at all.
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
