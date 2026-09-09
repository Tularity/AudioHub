// Guard shared timing tokens and platform anchor placement. Perceptual scale
// thresholds are design choices, not correctness rules; verify motion in the lab.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';

const CSS = readFileSync(fileURLToPath(new URL('../styles.css', import.meta.url)), 'utf8');
const BARE = CSS.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '));

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

describe('durations come from the token scale', () => {
  it('leaves no bare sub-500ms literal in a transition or animation', () => {
    const leaks = DECLS.filter((d) => {
      if (d.body.includes('linear')) return false; // progress clocks and spinners
      return times(d.body).some((s) => s > 0 && s < 0.5);
    }).map((d) => `styles.css:${d.line}  ${d.prop}: ${d.body}`);

    // Ambient loops (breathe 2.4s, spin, waveBar 1.1s...) are periods, not
    // perceptual durations, and are deliberately left as literals -- hence the
    // 500ms cut-off rather than a blanket ban.
    expect(leaks).toEqual([]);
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
