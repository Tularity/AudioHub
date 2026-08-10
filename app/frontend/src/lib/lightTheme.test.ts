// The light theme's material rules, as assertions.
//
// The stylesheet's dark side expresses depth with light: a specular line along
// the top edge for things that stand up, an inset shade for things that sink
// in. On a dark surface those read as material. On a white card the same two
// strokes read as a bevel and a groove -- which is the definition of a
// skeuomorphic control, and is exactly what the light theme was reported as
// looking like.
//
// A comment saying "remember to neutralise highlights in light mode" cannot
// detect the next control that forgets to. These can:
//
//   1. every DIRECTIONAL inset shadow in the base sheet is cancelled by a light
//      override (uniform rings are exempt -- they are borders, not lighting),
//   2. the light glass is frosted rather than layered with a highlight,
//   3. the palette still clears WCAG on the surfaces text actually lands on,
//      including the watermark-tinted canvas.
//
// Assertion 1 is the load-bearing one: it is the rule a future contributor will
// break without noticing.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';

const ROOT = fileURLToPath(new URL('../../../../', import.meta.url));
const CSS = readFileSync(ROOT + 'app/frontend/src/styles.css', 'utf8');
const LIGHT = ':root[data-theme="light"]';

/** Strip comments; they contain the word `inset` in prose. */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '');
}

interface Rule { selector: string; body: string }

/**
 * Leaf rules only, with nesting tracked by hand. A regex cannot do this: an
 * `@media` wrapper would be swallowed into the selector of the rule inside it.
 */
function rules(src: string): Rule[] {
  const out: Rule[] = [];
  const stack: string[] = [];
  let buf = '';
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (ch === '{') { stack.push(buf.trim()); buf = ''; } else if (ch === '}') {
      const sel = stack.pop() ?? '';
      // A body containing `{` never reaches here as text, so anything with
      // declarations in `buf` is a leaf.
      if (buf.includes(':')) out.push({ selector: sel, body: buf });
      buf = '';
    } else buf += ch;
  }
  return out;
}

/** The value of one declaration, or null. */
function decl(body: string, prop: string): string | null {
  const m = body.match(new RegExp(`(?:^|;)\\s*${prop}\\s*:([^;]*)`));
  return m ? m[1].trim() : null;
}

/**
 * An inset shadow with a non-zero vertical offset simulates a light source
 * above (or below) the surface. `inset 0 0 0 1px` is a ring: it sits equally on
 * all four sides, reads as a border, and is fine in either theme.
 */
function directionalInsets(shadow: string): string[] {
  const found: string[] = [];
  const re = /inset\s+(-?[\d.]+)(?:px)?\s+(-?[\d.]+)px/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(shadow))) if (Number(m[2]) !== 0) found.push(m[0]);
  return found;
}

const ALL = rules(stripComments(CSS));
const parts = (sel: string) => sel.split(',').map((s) => s.trim()).filter(Boolean);

describe('light theme cancels the dark theme lighting model', () => {
  // Selectors that carry a directional inset when no theme attribute is set.
  const base = new Map<string, string>();
  for (const r of ALL) {
    if (r.selector.includes('[data-theme=')) continue;
    const sh = decl(r.body, 'box-shadow');
    if (!sh) continue;
    for (const hit of directionalInsets(sh)) {
      for (const p of parts(r.selector)) base.set(p, hit);
    }
  }

  // Selectors the light theme re-declares `box-shadow` for WITHOUT bringing a
  // directional inset back. Merely re-declaring is not enough: an override that
  // restates the bevel is the bug wearing the fix's clothes, and an earlier
  // draft of this test accepted exactly that.
  const overridden = new Set<string>();
  for (const r of ALL) {
    if (!r.selector.includes(LIGHT)) continue;
    const sh = decl(r.body, 'box-shadow');
    if (sh === null || directionalInsets(sh).length > 0) continue;
    for (const p of parts(r.selector)) overridden.add(p.replace(LIGHT, '').trim());
  }

  it('found the rules it is supposed to be guarding', () => {
    // A guard that matches nothing passes forever. These are the controls the
    // user pointed at, so if the sheet stops containing them the test is wrong,
    // not the sheet.
    expect([...base.keys()]).toEqual(expect.arrayContaining(['.btn', '.btn.primary', '.input']));
    expect(base.size).toBeGreaterThanOrEqual(9);
  });

  it('leaves no directional inset unneutralised in light mode', () => {
    const leaked = [...base.entries()]
      .filter(([sel]) => !overridden.has(sel))
      .map(([sel, hit]) => `${sel}  (${hit})`);
    expect(leaked).toEqual([]);
  });

  it('treats a uniform ring as a border, not as lighting', () => {
    // .qdot draws its empty state with `inset 0 0 0 1px`; that is the ring, and
    // requiring a light override for it would be noise.
    expect(directionalInsets('inset 0 0 0 1px var(--ring-empty)')).toEqual([]);
    expect(directionalInsets('inset 0 1px 0 rgba(255,255,255,.3)')).toHaveLength(1);
    expect(directionalInsets('inset 0 -14px 22px rgba(0,0,0,.28)')).toHaveLength(1);
  });
});

describe('light glass is frosted rather than lit', () => {
  const lightRoot = ALL.find((r) => r.selector === LIGHT)?.body ?? '';
  const darkRoot = ALL.find((r) => r.selector === ':root')?.body ?? '';

  it('carries no gradient layer', () => {
    // The dark fill layers a radial specular over the tint. On a near-white
    // pill that layer is not "light", it is "a bulge".
    expect(decl(darkRoot, '--glass-fill')).toContain('gradient(');
    expect(decl(lightRoot, '--glass-fill')).not.toContain('gradient(');
  });

  it('is more opaque than the dark tint, not less', () => {
    // Pin the tint layer by name. A generic `rgba(...)` match walks into the
    // gradient stops, and `[^)]*` stops early on the `)` inside `var(...)`.
    const alpha = (v: string | null) => Number(v?.match(/rgba\(var\(--glass-rgb\),\s*(\.?\d*\.?\d+)\s*\)/)?.[1] ?? NaN);
    const dark = alpha(decl(darkRoot, '--glass-fill'));
    const light = alpha(decl(lightRoot, '--glass-fill'));
    expect(dark).toBeGreaterThan(0);
    expect(light).toBeGreaterThan(dark);
  });

  it('edges the pill with a ring instead of a top highlight', () => {
    expect(directionalInsets(decl(lightRoot, '--glass-edge') ?? '')).toEqual([]);
  });

  it('drops the backdrop filter when the system asks for less transparency', () => {
    const media = CSS.match(/@media \(prefers-reduced-transparency: reduce\)[\s\S]*?\n}/);
    expect(media).not.toBeNull();
    expect(media![0]).toContain('--glass-blur: none');
    // Both selectors, or the light theme keeps its own (higher-specificity) value.
    expect(media![0]).toContain(LIGHT);
  });
});

describe('light palette clears WCAG on the surfaces text lands on', () => {
  // sRGB relative luminance, per WCAG 2.1.
  const chan = (v: number) => { const s = v / 255; return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4; };
  const lum = ([r, g, b]: number[]) => 0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b);
  const ratio = (a: number[], b: number[]) => {
    const [hi, lo] = [lum(a), lum(b)].sort((p, q) => q - p);
    return (hi + 0.05) / (lo + 0.05);
  };
  const over = (src: number[], dst: number[], a: number) => src.map((c, i) => Math.round(c * a + dst[i] * (1 - a)));

  /** Read a hex token straight out of the light block, so the test cannot drift. */
  const lightBody = ALL.find((r) => r.selector === LIGHT)?.body ?? '';
  function token(name: string): number[] {
    const v = decl(lightBody, name);
    const m = v?.match(/#([0-9a-f]{6})/i);
    if (!m) throw new Error(`no hex for ${name} in the light block (got ${v})`);
    return [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16));
  }

  const bg = token('--bg');
  const card = token('--bg-1');

  /** The watermark is authored as `rgba(r, g, b, a)`, not as a hex. */
  const ink = (() => {
    const v = decl(lightBody, '--watermark-ink') ?? '';
    const m = v.match(/rgba\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\.?\d*\.?\d+)\s*\)/);
    if (!m) throw new Error(`could not parse --watermark-ink (got ${v})`);
    return { rgb: [Number(m[1]), Number(m[2]), Number(m[3])], a: Number(m[4]) };
  })();
  // The canvas as text meets it: footnotes sit outside the cards, on watermark.
  const inked = over(ink.rgb, bg, ink.a);

  it('keeps --text-dim at AAA even over the watermark', () => {
    // docs/design-ui-chrome.md §5.2.1: compositing the watermark must not drop
    // a grade. A previous pass shipped 7.22 -> 6.77 and fell out of AAA.
    expect(ratio(token('--text-dim'), bg)).toBeGreaterThanOrEqual(7);
    expect(ratio(token('--text-dim'), inked)).toBeGreaterThanOrEqual(7);
  });

  it('keeps the primary button legible now that its fill is flat', () => {
    // The gradient is gone, so the text sits on --accent itself, not on the
    // lighter --accent-lift end that used to be under the first line of glyphs.
    expect(ratio(token('--on-accent'), token('--accent'))).toBeGreaterThanOrEqual(4.5);
  });

  it('keeps placeholder text above AA on the input fill', () => {
    // Placeholders here say what to type, so they are content, not decoration.
    expect(ratio(token('--placeholder'), bg)).toBeGreaterThanOrEqual(4.5);
    // ...and still clearly a step below body text, or it stops reading as a hint.
    expect(ratio(token('--placeholder'), bg)).toBeLessThan(ratio(token('--text-dim'), bg));
  });

  it('keeps every semantic colour at AA on card and on watermark', () => {
    for (const name of ['--accent', '--ok', '--danger', '--warn']) {
      expect(ratio(token(name), card), `${name} on card`).toBeGreaterThanOrEqual(4.5);
      expect(ratio(token(name), inked), `${name} on watermark`).toBeGreaterThanOrEqual(4.5);
    }
  });
});

describe('dead variables stay dead', () => {
  it('has no --chrome-lead left to mislead the next reader', () => {
    // It was declared 0px, re-declared 100px for macOS, and read by nothing:
    // the brand block it reserved space for became the background watermark.
    // Comments are stripped first -- the header explains why it went away, and
    // that sentence is the point of the removal, not a relapse.
    expect(stripComments(CSS)).not.toContain('--chrome-lead');
  });
});
