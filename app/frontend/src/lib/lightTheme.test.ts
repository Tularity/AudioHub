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

  // -------------------------------------------------------------------------
  // Class C: the right ink, on the wrong kind of object.
  //
  // A and B were about strokes. This one is about which TIER a colour comes
  // from. The four semantic tokens are tuned for body text, which in the light
  // theme forces them to L* 41-44. They were also painting things that are not
  // text at all -- 7px status dots, the quality pips, the segments of the
  // latency band, the 3px rule down the side of the tier callout. Those are
  // graphical objects: SC 1.4.11 asks 3:1 of them, not 4.5:1, and holding them
  // to the text bar cost 11-14 L* of brightness each. On a light canvas that is
  // the difference between "a status colour" and "a black speck".
  //
  // The proof that it mattered is the latency band, whose four segments came
  // from four different places: three fill-tuned --hue-* tokens and one text-
  // tuned --accent. In the dark theme all four happen to sit at L* 66-73 and
  // nothing shows. In the light theme they measured 48.3 / 40.9 / 62.4 / 43.8.
  //
  // So: --*-mark is the graphics tier, and these assertions keep marks off the
  // text tier. Use `ratio(x, white)` as a lightness proxy throughout -- a lower
  // ratio against white means a lighter colour.
  // -------------------------------------------------------------------------
  const darkBodyC = ALL.find((r) => r.selector === ':root')?.body ?? '';
  const MARKS = ['--ok-mark', '--accent-mark', '--warn-mark', '--danger-mark'] as const;

  it('defines the dark mark tier as the semantic token itself', () => {
    // This is what makes test/ui2/darkdiff.mjs green BY CONSTRUCTION rather
    // than by care: in the dark theme every mark resolves to the exact literal
    // it replaced, so swapping a call site over cannot move a dark pixel. Give
    // dark its own mark value and that guarantee is gone -- which is fine, but
    // it has to be a decision, not an accident.
    for (const m of MARKS) {
      const semantic = m.replace('-mark', '');
      expect(decl(darkBodyC, m)?.trim(), `dark ${m}`).toBe(`var(${semantic})`);
    }
    expect(decl(darkBodyC, '--hue-capture')?.trim()).toBe('var(--accent)');
  });

  it('keeps the light mark tier above the 3:1 non-text bar, and lighter than its text twin', () => {
    for (const m of MARKS) {
      expect(ratio(token(m), card), `${m} on card`).toBeGreaterThanOrEqual(3);
      expect(ratio(token(m), inked), `${m} on watermark`).toBeGreaterThanOrEqual(3);
      // Lighter than the text tier, or the split bought nothing at all.
      expect(ratio(token(m), card), `${m} vs its text twin`)
        .toBeLessThan(ratio(token(m.replace('-mark', '')), card));
    }
  });

  it('gives --accent-mark and --ring the same ink', () => {
    // Same question -- "the accent, drawn as a graphic, at 3:1 on a light
    // surface" -- so it must not have two answers that drift apart.
    expect(decl(lightBody, '--accent-mark')?.trim()).toBe(decl(lightBody, '--ring')?.trim());
  });

  it('holds the four latency-band fills in one lightness band', () => {
    const band = ['--hue-net', '--hue-capture', '--hue-buffer', '--hue-play'].map((n) => ratio(token(n), card));
    for (const r of band) expect(r, 'band fill on card').toBeGreaterThanOrEqual(3);
    // Before this round the spread was 2.93 .. 6.24 -- one segment read as a
    // black bar between three coloured ones. Contrast-against-white is a
    // monotone proxy for lightness, so bounding its spread bounds theirs.
    expect(Math.max(...band) / Math.min(...band), 'band lightness spread').toBeLessThan(1.35);
  });

  it('keeps text legible on a wash of its own colour', () => {
    // The trap this round very nearly shipped. A callout is usually "coloured
    // text on a faint wash of the same colour" -- .metric-tier, .stage-chip.warn,
    // every .tag. Brightening the text token brightens the WASH by exactly the
    // same amount, so the pair can move DOWN while every measurement against
    // the page background moves up. Lifting --warn from #7d5a0e to #955802 took
    // .metric-tier from 4.87 to 4.44 and .stage-chip.warn from 4.77 to 4.36 --
    // both out of AA -- while --warn on the canvas got better. Nothing that
    // measures a token against the page can see this; it has to be measured
    // pair by pair, at the call site.
    //
    // The fix is to draw the wash with the MARK tier: a lighter ink makes a
    // lighter wash under the same dark text.
    const SEM = ['ok', 'accent', 'warn', 'danger'];
    // Conservative surface: the darker of the two a callout can sit on.
    const surface = token('--bg-2');

    // A base rule's background is not necessarily what the LIGHT theme paints.
    // `.seg.on` is the case in point: the base sheet gives it an 18% -> 8.6%
    // accent gradient, and the light block replaces the whole thing with a flat
    // 10% tint. Reading the base rule there would charge the light theme for a
    // wash it never draws (18% would read 4.42; the 10% it actually uses is
    // 4.67). So index the light overrides first and prefer them.
    const lightBg = new Map<string, string>();
    for (const r of ALL) {
      if (!r.selector.includes(LIGHT)) continue;
      const bg = decl(r.body, 'background') ?? decl(r.body, 'background-color');
      if (!bg) continue;
      for (const part of r.selector.split(',')) {
        const sel = part.trim().replace(`${LIGHT} `, '').trim();
        if (sel && sel !== LIGHT) lightBg.set(sel, bg);
      }
    }

    /** Two idioms in this file: rgba(var(--X-rgb), A), color-mix(--X P%). */
    function wash(bgDecl: string | null): { tok: string; alpha: number } | null {
      if (!bgDecl) return null;
      const m = bgDecl.match(/rgba\(\s*var\(--(\w+)-rgb\)\s*,\s*([\d.]+)\s*\)/)
        ?? bgDecl.match(/color-mix\(in srgb,\s*var\(--([\w-]+)\)\s*([\d.]+)%/);
      if (!m) return null;
      const n = Number(m[2]);
      return { tok: m[1], alpha: n > 1 ? n / 100 : n };
    }
    const bgOf = (r: Rule) => lightBg.get(r.selector.replace(/\s+/g, ' ').trim())
      ?? decl(r.body, 'background') ?? decl(r.body, 'background-color');
    const base = ALL.filter((r) => !r.selector.includes(LIGHT) && !r.selector.startsWith('@'));

    // The wash and the text are not always in the same rule. `.metric-tier`
    // carries the 12% background while `.metric-tier-label` carries the colour,
    // and it was precisely that pair that fell out of AA. There is no
    // structural relation a stylesheet parser can see between those two
    // selectors -- only the naming convention, which this file follows
    // throughout: a block is `.x`, its parts are `.x-*`. So pair them by that.
    const pairs: { label: string; fg: string; tok: string; alpha: number }[] = [];
    for (const r of base) {
      const w = wash(bgOf(r) ?? null);
      if (!w || !SEM.includes(w.tok.replace(/-mark$/, ''))) continue;
      const sel = r.selector.replace(/\s+/g, ' ').trim();
      // (a) same rule
      const own = decl(r.body, 'color')?.match(/var\(--(ok|accent|warn|danger)\)/);
      if (own) pairs.push({ label: sel, fg: own[1], ...w });
      // (b) `.x-*` parts of the block `.x`
      if (!/^\.[\w-]+$/.test(sel)) continue;
      for (const c of base) {
        const csel = c.selector.replace(/\s+/g, ' ').trim();
        if (!csel.startsWith(`${sel}-`)) continue;
        const cfg = decl(c.body, 'color')?.match(/var\(--(ok|accent|warn|danger)\)/);
        if (cfg) pairs.push({ label: `${csel} on ${sel}`, fg: cfg[1], ...w });
      }
    }
    let checked = 0;
    for (const p of pairs) {
      if (p.tok.replace(/-mark$/, '') !== p.fg) continue;      // same-hue washes only
      const r = ratio(token(`--${p.fg}`), over(token(`--${p.tok}`), surface, p.alpha));
      expect(r, `${p.label}: text on its own ${(p.alpha * 100).toFixed(1)}% wash`)
        .toBeGreaterThanOrEqual(4.5);
      checked++;
    }
    // Guard the guard: if the parse stops matching, this must not read green.
    expect(checked, 'same-hue text-on-wash pairs found').toBeGreaterThanOrEqual(8);
  });

  it('paints no small mark with a text-tier colour', () => {
    // The rule a future contributor will break. Each of these draws a solid
    // shape a few pixels across; none has text on it.
    const MARK_RULES = [
      '.dot.online', '.dot.connecting', '.peer-inbound .dot.live',
      '.qdot.on.tone-ok', '.qdot.on.tone-accent', '.qdot.on.tone-warn', '.qdot.on.tone-danger',
      '.pair-steps li.done .step-dot', '.pair-steps li.doing .step-dot', '.pair-steps li.failed .step-dot',
      '.band-capture', '.wf-capture', '.stop-tick.on', '.switch.pending .knob',
    ];
    const seen = new Set<string>();
    for (const r of ALL) {
      const sel = r.selector.replace(/\s+/g, ' ').trim();
      if (!MARK_RULES.includes(sel)) continue;
      seen.add(sel);
      // `--accent-mark` contains `--accent`, so match the closing paren too.
      const bare = r.body.match(/var\(--(ok|accent|warn|danger)\)/g);
      expect(bare, `${sel} still paints itself with a text-tier token`).toBeNull();
    }
    // Guard the guard: a renamed selector must not silently drop out.
    expect([...seen].sort(), 'mark rules found in the stylesheet').toEqual([...MARK_RULES].sort());
  });
});

// ---------------------------------------------------------------------------
// Class B: the inverted glow.
//
// Everything above guards class A -- SHAPE errors, where a stroke simulates a
// curved surface that is not there. It did not catch the bug the user reported
// next: "the glow around the switch looks blackish in light mode". That is a
// different failure. The stroke is fine; its INK is wrong.
//
//     on a dark surface, "brighter than the background" reads as light;
//     on a light surface, "darker than the background" reads as dirt.
//
// `box-shadow: 0 0 0 2px rgba(var(--accent-rgb), .2)` is a glow in the dark
// theme and a smudge in the light one, from the same declaration -- because the
// light theme's --accent is DARKENED on purpose, to clear body-text contrast.
// A colour tuned to be legible as text is, by construction, wrong as light.
//
// Hence the split: --ring / --halo / --halo-soft are the ring inks, free to be
// brighter than --accent because nothing is ever printed on them, and
// --shadow-knob joins the --shadow* family so no call site invents its own
// depth. docs/design-ui-chrome.md §9.3 is the full checklist.
// ---------------------------------------------------------------------------
describe('light theme does not paint glows with text-contrast colours', () => {
  const lightBody = ALL.find((r) => r.selector === LIGHT)?.body ?? '';
  const darkBody = ALL.find((r) => r.selector === ':root')?.body ?? '';
  const bare = stripComments(CSS);

  /** Non-inset ring/glow layers: `0 0 0 Npx <ink>` and friends, no blur offset. */
  const OUTER_GLOW = /box-shadow:[^;]*?(?<!inset\s)\b0\s+0\s+0\s+\d/;
  /** `@keyframes` stops (`0%, 100%`, `from`, `to`) are not selectors. */
  const isKeyframeStop = (sel: string) => /^(\d|from\b|to\b)/.test(sel.trim());

  it('routes every outer accent ring through --ring / --halo, not --accent', () => {
    // The three call sites that used to spell `rgba(var(--accent-rgb), .2)` out
    // by hand (.switch.pending, the volume thumb, the dragged transport thumb)
    // plus the two focus rings. Any NEW one will show up here.
    //
    // Keyframe stops are exempt HERE and only here: they are dark-only, and the
    // 're-points every glow keyframe' case below is what holds them to account.
    // Skipping them in both places would be the hole, so it does not.
    const offenders = ALL.filter((r) => {
      if (isKeyframeStop(r.selector)) return false;
      const sh = decl(r.body, 'box-shadow');
      if (!sh || !OUTER_GLOW.test(`box-shadow:${sh}`)) return false;
      return /--accent(-rgb)?\)/.test(sh);
    }).map((r) => r.selector);
    expect(offenders).toEqual([]);
  });

  it('never draws an outline in a colour picked for body text', () => {
    // `outline: … var(--accent)` is the single worst case measured this round
    // (the focused control's ring sat 168/255 darker than the white card).
    const outlines = ALL
      .map((r) => [r.selector, decl(r.body, 'outline')] as const)
      .filter(([, v]) => v && /--accent\b/.test(v));
    expect(outlines).toEqual([]);
  });

  it('gives --ring an ink of its own in light, and reuses --accent in dark', () => {
    // Dark must keep resolving to exactly what it resolved to before the split,
    // or this refactor silently restyled the theme the user said was perfect.
    expect(decl(darkBody, '--ring')).toBe('var(--accent)');
    expect(decl(darkBody, '--ring-rgb')).toBe('var(--accent-rgb)');
    // Light must NOT: pointing --ring back at --accent would undo the whole fix.
    expect(decl(lightBody, '--ring')).toMatch(/^#[0-9a-f]{6}$/i);
    expect(decl(lightBody, '--ring')).not.toContain('var(--accent');
  });

  it('keeps --ring above the 3:1 non-text bar on every surface it lands on', () => {
    // WCAG 2.2 SC 1.4.11 / 2.4.13. The ceiling this sets is the reason --ring is
    // not brighter: the watermarked canvas is the tightest of the four.
    const chan = (v: number) => { const s = v / 255; return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4; };
    const lum = ([r, g, b]: number[]) => 0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b);
    const ratio = (a: number[], b: number[]) => {
      const [hi, lo] = [lum(a), lum(b)].sort((p, q) => q - p);
      return (hi + 0.05) / (lo + 0.05);
    };
    const over = (src: number[], dst: number[], a: number) => src.map((c, i) => Math.round(c * a + dst[i] * (1 - a)));
    const hex = (v: string | null) => {
      const m = v?.match(/#([0-9a-f]{6})/i);
      if (!m) throw new Error(`no hex in ${v}`);
      return [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16));
    };
    const ring = hex(decl(lightBody, '--ring'));
    const bg = hex(decl(lightBody, '--bg'));
    const inkM = (decl(lightBody, '--watermark-ink') ?? '').match(/rgba\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*(\.?\d*\.?\d+)\s*\)/)!;
    const inked = over([+inkM[1], +inkM[2], +inkM[3]], bg, +inkM[4]);
    for (const [name, surface] of [['--bg-1', hex(decl(lightBody, '--bg-1'))], ['--bg-2', hex(decl(lightBody, '--bg-2'))],
      ['--bg', bg], ['canvas+watermark', inked]] as const) {
      expect(ratio(ring, surface), `--ring on ${name}`).toBeGreaterThanOrEqual(3);
    }
  });

  it('gives every hand-rolled outer --sh-rgb shadow a light override', () => {
    // A call site writing `0 10px 26px rgba(var(--sh-rgb), .3)` invents a depth
    // step outside the --shadow* family: invisible in dark, and in light it
    // measured 2.5x the weight of a plain card. Two rules did (.disc-item:hover,
    // .sheet-card) and two more duplicated a knob shadow (now --shadow-knob).
    //
    // Scope: OUTER layers only. An `inset` --sh-rgb stroke is class A -- the
    // groove -- and the directional-inset case at the top of this file owns it.
    // The bar here is "has a light override", not "must use a token": the dark
    // theme is frozen, so the base rule keeps its literal on purpose.
    const outerSh = (sh: string) => sh.split(/,(?![^(]*\))/)
      .some((layer) => !layer.includes('inset') && layer.includes('--sh-rgb'));
    const overridden = new Set(ALL
      .filter((r) => r.selector.includes(LIGHT) && decl(r.body, 'box-shadow') !== null)
      .flatMap((r) => parts(r.selector))
      .map((s) => s.replace(LIGHT, '').trim()));
    const leaks = ALL.flatMap((r) => {
      if (r.selector === ':root' || r.selector === LIGHT || r.selector.includes('[data-theme=')) return [];
      const sh = decl(r.body, 'box-shadow');
      if (!sh || !outerSh(sh)) return [];
      return parts(r.selector).filter((s) => !overridden.has(s)).map((s) => `${s}  {${sh.trim()}}`);
    });
    expect(leaks).toEqual([]);
  });

  it('re-points every glow keyframe away from box-shadow in light mode', () => {
    // @keyframes cannot be reached by a `:root[data-theme=light] .x` override --
    // it belongs to no selector. `animation-name` CAN be, so each consumer of a
    // shadow-based breathe is redirected to a colourless one.
    const glowFrames = [...bare.matchAll(/@keyframes\s+([\w-]+)\s*\{([^}]*\{[^}]*\}\s*)*\}/g)]
      .filter((m) => m[0].includes('box-shadow'))
      .map((m) => m[1]);
    expect(glowFrames.length).toBeGreaterThanOrEqual(3);

    for (const frame of glowFrames) {
      // Everything that plays this animation…
      const consumers = ALL.filter((r) => {
        const a = decl(r.body, 'animation') ?? decl(r.body, 'animation-name');
        return !!a && new RegExp(`(^|[\\s:])${frame}([\\s,;]|$)`).test(a);
      }).flatMap((r) => parts(r.selector)).filter((s) => !s.includes('[data-theme='));
      expect(consumers.length, `nothing plays @${frame}`).toBeGreaterThan(0);

      // …must have a light override renaming the animation.
      const redirected = new Set(ALL
        .filter((r) => r.selector.includes(LIGHT) && decl(r.body, 'animation-name'))
        .flatMap((r) => parts(r.selector))
        .map((s) => s.replace(LIGHT, '').trim()));
      for (const c of consumers) {
        expect(redirected.has(c), `${c} plays @${frame} with no light override`).toBe(true);
      }
    }
  });

  it('has no 180deg accent gradient left un-flattened in light mode', () => {
    // Class A rule 3 ("no vertical gradient on a solid control") had exactly one
    // survivor, .mode-bar, because its alpha was 3.9% and nobody looked twice.
    const gradients = ALL.filter((r) => {
      if (r.selector.includes('[data-theme=')) return false;
      const bgv = decl(r.body, 'background') ?? '';
      return /linear-gradient\(\s*180deg/.test(bgv) && /--accent/.test(bgv);
    }).flatMap((r) => parts(r.selector));
    const flattened = new Set(ALL
      .filter((r) => r.selector.includes(LIGHT) && decl(r.body, 'background'))
      .flatMap((r) => parts(r.selector))
      .map((s) => s.replace(LIGHT, '').trim()));
    expect(gradients.filter((s) => !flattened.has(s))).toEqual([]);
  });

  it('draws all four modal scrims with the same ink', () => {
    // Three wash the page lighter (--bg-rgb), one washed it darker (--sh-rgb).
    // Either is defensible; having both means "a layer opened" has two opposite
    // readings inside one app.
    const scrims = ['.confirm-mask', '#gate', '#overlay', '.sheet-scrim'];
    const inkOf = (sel: string) => {
      const hits = ALL.filter((r) => parts(r.selector).some((p) => p === sel || p.endsWith(` ${sel}`)))
        .map((r) => decl(r.body, 'background') ?? decl(r.body, 'background-color'))
        .filter((v): v is string => !!v && v.includes('-rgb'));
      return hits[hits.length - 1]?.match(/var\((--[\w-]+-rgb)\)/)?.[1] ?? null;
    };
    const light = new Set(scrims.map((s) => {
      const over = ALL.filter((r) => r.selector.includes(LIGHT) && parts(r.selector).some((p) => p.replace(LIGHT, '').trim() === s))
        .map((r) => decl(r.body, 'background'))
        .filter((v): v is string => !!v);
      return (over[0] ?? '').match(/var\((--[\w-]+-rgb)\)/)?.[1] ?? inkOf(s);
    }));
    expect([...light]).toEqual(['--bg-rgb']);
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
