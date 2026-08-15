// The motion system's three non-negotiable rules, as assertions.
//
// spec-motion froze a duration scale, two new easing curves and -- the part that
// actually answers the original complaint -- an amplitude floor. The floor is
// the one that keeps getting lost, because breaking it produces no error, no
// visual glitch and no failing test: the animation still plays, it is just
// invisible. `viewIn` scaled from .994 for months. `.btn:active` deformed by 4%,
// which after the spring's ~5.7% overshoot is 0.23px of movement.
//
// The mechanism, from the perception literature: people cannot sense
// acceleration directly. A curve is inferred from the difference between the
// speed at the start and the speed at the end, so it is only legible if the
// motion covers enough ground to produce two distinguishable speed samples.
// Below the floor, ease-out, ease-in-out and linear all collapse into the same
// event -- which is precisely what "the animation is too fast to see" describes.
// So the fix for an invisible animation is more travel, not more time.
//
// `sheet.test.ts:146` already guards one keyframe this way. These generalise it:
//
//   1. every entrance keyframe clears the floor (>= 8px translate or >= 8%
//      scale, or a rotation large enough to read),
//   2. no duration literal survives outside the token scale,
//   3. `--ease-in` stays out of anything that can be re-opened.
//
// Reading the stylesheet is the same trick `lightTheme.test.ts`,
// `sheet.test.ts` and `peerMetricsAlignment.test.ts` use; vitest runs with
// `environment: 'node'` on purpose, so there is no DOM to measure instead.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';
import { SHEET_EXIT_MS } from './sheet';

const CSS = readFileSync(fileURLToPath(new URL('../styles.css', import.meta.url)), 'utf8');

/**
 * Comments blanked out, **not deleted**.
 *
 * Every failure message in this file quotes `styles.css:N`, computed by counting
 * newlines up to the match. Deleting a comment deletes its newlines with it, so
 * the count drifts by the size of everything commented above — and this file is
 * roughly two thirds comment. Measured 2026-08-14: a rule on real line 1074 was
 * being reported as line 475. A guard that points at the wrong line costs the
 * next reader more than no guard at all.
 *
 * Replacing each non-newline character with a space keeps both the line count
 * and the column offsets identical to the source.
 */
const BARE = CSS.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '));

// ---- amplitude floor, from spec-motion §2.3 ----
const MIN_PX = 8;
const MIN_SCALE = 0.08; // 8% of travel
const MIN_DEG = 15;
/**
 * Floating-point slack for the floor comparisons.
 *
 * CSS carries decimal literals and the checks are subtractions, so a value that
 * sits exactly on the floor lands just under it: `1 - 0.92` is
 * 0.07999999999999996 in IEEE754, and `scale(.92)` -- Material's own
 * fade-through value, chosen precisely because it equals the floor -- failed a
 * `>= 0.08` test. Rejecting a value for being 4e-17 short is a bug in the
 * guard, not a finding about the animation.
 */
const EPS = 1e-9;

/** `:root` value of a custom property, e.g. `--enter-shift` -> `12px`. */
function token(name: string): string {
  const m = BARE.match(new RegExp(`(?:^|;|\\{)\\s*${name}\\s*:([^;}]*)`));
  expect(m, `${name} must be defined in styles.css`).toBeTruthy();
  return m![1].trim();
}

/** Resolve one level of `var(--x)`; the motion tokens are all literals. */
function resolve(value: string): string {
  return value.replace(/var\(\s*(--[\w-]+)\s*(?:,[^)]*)?\)/g, (_, n: string) => token(n));
}

interface Frame { name: string; body: string }

/** Every `@keyframes` block, with its full body (all steps). */
function keyframes(): Frame[] {
  const out: Frame[] = [];
  const re = /@keyframes\s+([\w-]+)\s*\{/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(BARE))) {
    let depth = 1;
    let i = re.lastIndex;
    for (; i < BARE.length && depth > 0; i++) {
      if (BARE[i] === '{') depth++;
      else if (BARE[i] === '}') depth--;
    }
    out.push({ name: m[1], body: BARE.slice(re.lastIndex, i - 1) });
  }
  return out;
}

const FRAMES = keyframes();

/** The `from` / `0%` step of a keyframe, or null when it has none. */
function startStep(body: string): string | null {
  const m = body.match(/(?:^|\})\s*(?:from|0%)[^{]*\{([^}]*)\}/);
  return m ? m[1] : null;
}

/**
 * An entrance: something that arrives from nothing. `opacity: 0` at the start
 * plus a transform is what separates these from exits (`fadeOut`,
 * `sheetShrink`) and from the ambient loops, which never start transparent.
 */
const ENTRANCES = FRAMES.filter((f) => {
  const from = startStep(f.body);
  return !!from && /opacity\s*:\s*0(?![.\d])/.test(from) && /transform\s*:/.test(from);
});

describe('entrance keyframes clear the amplitude floor', () => {
  it('finds the entrance keyframes at all', () => {
    // Guards the detector itself: if a refactor renames or restructures the
    // keyframes so this list empties out, every assertion below would pass
    // vacuously and the floor would be unguarded again.
    expect(ENTRANCES.length).toBeGreaterThanOrEqual(4);
    expect(ENTRANCES.map((f) => f.name)).toContain('viewIn');
  });

  for (const frame of ENTRANCES) {
    it(`@${frame.name} travels far enough for its curve to be read`, () => {
      // Scan the whole resolved `transform` value rather than each function's
      // arguments: the arguments may be `calc(var(--enter-shift) * -1)`, and for
      // an amplitude question the direction is irrelevant anyway.
      const from = resolve(startStep(frame.body)!.match(/transform\s*:([^;]*)/)![1]);

      const px = [...from.matchAll(/(-?[\d.]+)px/g)].map((m) => Math.abs(Number(m[1])));
      const scale = [...from.matchAll(/scale[XYZ]?\(([^)]*)\)/g)]
        .map((m) => Math.abs(1 - Number(m[1].match(/-?[\d.]+/)![0])));
      const deg = [...from.matchAll(/(-?[\d.]+)deg/g)].map((m) => Math.abs(Number(m[1])));

      expect(
        px.length + scale.length + deg.length,
        `@${frame.name} declares a transform with no readable translate/scale/rotate`,
      ).toBeGreaterThan(0);

      const ok = px.some((v) => v >= MIN_PX - EPS)
        || scale.some((v) => v >= MIN_SCALE - EPS)
        || deg.some((v) => v >= MIN_DEG - EPS);

      expect(
        ok,
        `@${frame.name} moves ${px.join('/') || '-'}px, scales ${scale.map((v) => `${(v * 100).toFixed(1)}%`).join('/') || '-'}: `
        + `below the floor (>= ${MIN_PX}px or >= ${MIN_SCALE * 100}%). Either enlarge it or drop the geometry `
        + 'and transition opacity alone -- do NOT lengthen the duration, that yields a slow invisible animation.',
      ).toBe(true);
    });
  }
});

// ---- every transition/animation declaration in the sheet ----
interface Decl { prop: 'transition' | 'animation'; body: string; line: number }

const DECLS: Decl[] = (() => {
  const out: Decl[] = [];
  const re = /(?:^|[;{\s])(transition|animation)\s*:\s*([^;}]*)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(BARE))) {
    out.push({
      prop: m[1] as Decl['prop'],
      body: m[2].replace(/\s+/g, ' ').trim(),
      line: BARE.slice(0, m.index + 1).split('\n').length,
    });
  }
  return out;
})();

/** Time literals in a declaration, in seconds. */
function times(body: string): number[] {
  return [...body.matchAll(/(?<![\w-])(\.?\d+(?:\.\d+)?)(m?s)(?![\w-])/g)]
    .map((m) => (m[2] === 'ms' ? Number(m[1]) / 1000 : Number(m[1])));
}

// The exit pair `sheet.test.ts` pins. That test parses the duration straight out
// of this stylesheet and compares it to SHEET_EXIT_MS, so a var() reference --
// which carries no digits -- would make it fail. The literal is the interface to
// that test, and these two are the only places allowed to carry one.
const SHEET_EXIT = /\b(fadeOut|sheetShrink)\b/;

describe('durations come from the token scale', () => {
  it('leaves no bare sub-500ms literal in a transition or animation', () => {
    const leaks = DECLS.filter((d) => {
      if (d.body.includes('linear')) return false;      // see the next test
      if (SHEET_EXIT.test(d.body)) return false;         // pinned by sheet.test.ts
      return times(d.body).some((s) => s > 0 && s < 0.5);
    }).map((d) => `styles.css:${d.line}  ${d.prop}: ${d.body}`);

    // Ambient loops (breathe 2.4s, spin, waveBar 1.1s...) are periods, not
    // perceptual durations, and are deliberately left as literals -- hence the
    // 500ms cut-off rather than a blanket ban.
    expect(leaks).toEqual([]);
  });

  it('keeps exactly the three deliberate `linear` declarations', () => {
    // All three are correct and spec-motion explicitly preserves them: the
    // visibility delay trick, the countdown ring (easing would make elapsed time
    // look uneven) and the spinner (easing would show a hitch once per turn).
    // Pinned by count as well as by shape, so a new one cannot hide behind the
    // exemption above -- `linear` is otherwise a free pass out of the previous
    // assertion.
    const KNOWN = [
      /visibility 0s linear/,          // delay the hit-area removal until the fade ends
      /stroke-dashoffset [\d.]+s linear/, // pairing countdown ring
      /spin [\d.]+s linear infinite/,  // spinner
      // The sheet's exit fade. A keyframe segment inherits the animation's own
      // timing function, and every entrance/exit curve is front-loaded by
      // construction -- so a nominally 120ms fade collapsed into 25ms of real
      // change (measured 2026-08-14: opacity 0.83 -> 0.24 between t=176ms and
      // t=201ms). That collapse is what "the sheet just vanishes" was. A steady
      // fade is the point here, so this segment overrides to linear.
      /^linear$/,
    ];
    // `animation-timing-function` is collected separately: the DECLS regex
    // requires `animation` to be followed by a colon, so a per-keyframe
    // override slips straight past it. Leaving that hole open would let any
    // future `linear` hide inside a @keyframes block, which is exactly where
    // this one had to go.
    const perKeyframe = [...BARE.matchAll(/animation-timing-function\s*:\s*([^;}]*)/g)]
      .map((m) => ({ body: m[1].trim(), line: BARE.slice(0, m.index).split('\n').length }));
    const linear = [...DECLS, ...perKeyframe].filter((d) => d.body.includes('linear'));
    expect(linear).toHaveLength(4);
    for (const d of linear) {
      expect(
        KNOWN.some((k) => k.test(d.body)),
        `styles.css:${d.line} uses linear but is not one of the sanctioned cases: ${d.body}`,
      ).toBe(true);
    }
  });

  it('keeps the sheet exit literal in exactly the two places sheet.test.ts reads', () => {
    const pinned = DECLS.filter((d) => SHEET_EXIT.test(d.body));
    expect(pinned).toHaveLength(2);
    // Derived, not hard-coded: this pair exists precisely so the stylesheet and
    // SHEET_EXIT_MS stay equal, so pinning a third copy of the number here
    // would just add another thing to forget.
    //
    // Sum, not first-value: the scrim starts late on purpose (`--sheet-trail`)
    // so the card has a still backdrop to leave against. What has to equal
    // SHEET_EXIT_MS is when the animation ENDS, since that is when React pulls
    // the node. `sheet.test.ts` checks the same invariant from the other side.
    // The two do NOT end together, and that is the point: the card finishes its
    // travel first, then the scrim lifts. Reversing them costs the card its dark
    // backdrop mid-flight, which is what "it disappears halfway" was.
    //
    // So the invariant is a pair: nothing may run past the unmount, and
    // something must reach it -- otherwise the last frames are either cut off
    // or the screen sits dark with nothing in it.
    const ends = pinned.map((d) => {
      const resolved = d.body.replace(/var\((--[\w-]+)\)/g, (_, n) => token(n));
      return { line: d.line, end: +times(resolved).reduce((a, b) => a + b, 0).toFixed(3) };
    });
    for (const e of ends) {
      expect(e.end, `styles.css:${e.line} runs past the unmount (${e.end}s > SHEET_EXIT_MS)`)
        .toBeLessThanOrEqual(SHEET_EXIT_MS / 1000);
    }
    expect(Math.max(...ends.map((e) => e.end)), 'nothing reaches SHEET_EXIT_MS')
      .toBe(SHEET_EXIT_MS / 1000);
  });
});

describe('--ease-in is reserved for permanent departures', () => {
  // M3's semantics: an accelerate curve ends at peak speed, which is what makes
  // a component read as gone rather than merely hidden. Applying it to something
  // the user can bring straight back -- a sheet, a popover, a menu -- states the
  // opposite of what is true, so those keep --ease-out.
  const REOPENABLE = /sheet|pop|menu|onboarding|overlay-card|confirm-card|gate/i;

  it('never lands on a surface that can be re-opened', () => {
    const rules = [...BARE.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
      .filter((m) => m[2].includes('var(--ease-in)'))
      .map((m) => m[1].trim().split('\n').pop()!.trim());

    expect(rules.length, '--ease-in has no consumers; it should be used or removed').toBeGreaterThan(0);
    expect(rules.filter((s) => REOPENABLE.test(s))).toEqual([]);
  });

  it('never drives an entrance keyframe', () => {
    const names = new Set(ENTRANCES.map((f) => f.name));
    const misuse = DECLS.filter((d) => d.prop === 'animation'
      && d.body.includes('var(--ease-in)')
      && [...names].some((n) => new RegExp(`(^|[\\s,])${n}([\\s,]|$)`).test(d.body)));
    expect(misuse.map((d) => `styles.css:${d.line}  ${d.body}`)).toEqual([]);
  });
});

// The top strip mirrors its popovers on Windows, because Windows draws its
// caption buttons at the trailing edge and the icon cluster moves to the other
// side with them. A card dropdown is not in the top strip and must not follow.
//
// This is guarded rather than merely written down because the branch is
// unreachable on the development machine: `data-window-controls` is `left` on
// macOS, so the mirror rule never applies and neither a screenshot nor a human
// can see the difference. It was already wrong once -- 2026-08-14, the first
// attempt raised `.sel-pop` to `body .sel-pop` (0,1,1) believing that beat the
// mirror rule at (0,2,1); it does not, and the dropdown opened against the
// wrong edge with its scale origin in the wrong corner on Windows only.
describe('the Windows popover mirror stays out of the card dropdowns', () => {
  // Every mirrored selector, not just the first: the strip mirrors several
  // things (`#chrome-controls`, the popovers, the caption buttons), and only
  // the ones reaching `.chrome-pop` are dangerous.
  const MIRRORED = [...BARE.matchAll(/body\[data-window-controls="right"\]([^{]+)\{/g)]
    .map((m) => m[1].trim());

  it('mirrors something at all', () => {
    // Guards the detector: a rename would empty this list and every assertion
    // below would pass vacuously.
    expect(MIRRORED.length).toBeGreaterThan(0);
  });

  it('never mirrors a selector that a card dropdown also matches', () => {
    const dangerous = MIRRORED.filter(
      (s) => s.includes('.chrome-pop') && !s.includes(':not(.sel-pop)'),
    );
    expect(
      dangerous,
      'these mirrored selectors also match a card dropdown, which carries '
      + '.chrome-pop for the glass styling and would flip to the wrong edge on '
      + "Windows. Raising the dropdown's own specificity does NOT work: "
      + 'body .sel-pop is (0,1,1) and the mirror rule is (0,2,1). Exclude the '
      + 'class instead.',
    ).toEqual([]);
  });

  it('keeps the dropdown pinned to the trailing edge in its own rule', () => {
    const rule = BARE.match(/body\s+\.sel-pop\s*\{([^}]*)\}/);
    expect(rule, 'body .sel-pop must exist').toBeTruthy();
    expect(rule![1]).toMatch(/right:\s*0/);
    expect(rule![1]).toMatch(/transform-origin:\s*top right/);
  });
});
