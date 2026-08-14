// Guards for `lib/autostart.ts` -- the開機自启 switch's tri-state.
//
// The regression this file exists for is one specific cell of the matrix.
// `supported` and `enabled` are orthogonal: a login item outlives the bundle
// that wrote it, so `supported=false && enabled=true` is reachable by an
// ordinary route (turn autostart on inside the installed .app, then run the
// daemon from a dev build, or move the bundle). The UI used to grey the switch
// out on `!supported` alone, which on that cell produces a switch the user
// cannot turn off while the system keeps launching the app at every login --
// exactly the "shown as off while actually on" shape plan §16.4 rule 5 bans.
//
// The daemon half of the same fix is `autostart::plan_set` in
// `core/audiohubd/src/autostart.rs` ("registering needs the layout, revoking
// does not"). These two must agree; if the daemon ever refuses the off
// direction again, this file keeps offering a live switch that errors.

import { describe, it, expect } from 'vitest';
import { t } from '../i18n';
import { autostartNote, autostartToggleable, autostartView } from './autostart';
import type { AutostartFields } from './autostart';

/** Every combination the daemon can report, plus the "old daemon" case. */
const CELLS: Array<{ ds: AutostartFields | null; label: string }> = [
  { ds: null, label: 'no settings yet' },
  { ds: {}, label: 'old daemon (fields absent)' },
  { ds: { autostart: false, autostart_supported: false }, label: 'cannot, is not' },
  { ds: { autostart: true, autostart_supported: false }, label: 'cannot, but IS' },
  { ds: { autostart: false, autostart_supported: true }, label: 'can, is not' },
  { ds: { autostart: true, autostart_supported: true }, label: 'can, is' },
];

describe('autostartToggleable: an already-registered login item is always switchable', () => {
  // The load-bearing cell. Written on its own so a failure names it.
  it('stays live when the layout cannot register but something IS registered', () => {
    expect(autostartToggleable(false, true)).toBe(true);
  });

  it('is dead only when the layout cannot register AND nothing is', () => {
    expect(autostartToggleable(false, false)).toBe(false);
  });

  it('is live whenever the layout can register', () => {
    expect(autostartToggleable(true, false)).toBe(true);
    expect(autostartToggleable(true, true)).toBe(true);
  });

  // Guards against the switch being wired to `supported` again: that predicate
  // agrees with this one on three cells out of four, so a test that only
  // checked the common cells would not notice.
  it('is not just a rename of `supported`', () => {
    expect(autostartToggleable(false, true)).not.toBe(false);
  });
});

describe('autostartNote: every state says something, and says the right thing', () => {
  it('distinguishes an old daemon from a machine that cannot', () => {
    // Different next step for the user: upgrade vs. install differently.
    expect(autostartNote(false, false, false)).toBe('unknown');
    expect(autostartNote(true, false, false)).toBe('unsupported');
  });

  it('calls out the orphaned login item instead of reusing the greyed-out line', () => {
    expect(autostartNote(true, false, true)).toBe('orphaned');
  });

  it('explains itself when off, and stays quiet when plainly on', () => {
    expect(autostartNote(true, true, false)).toBe('off');
    expect(autostartNote(true, true, true)).toBe('on');
  });
});

describe('autostartView', () => {
  it('reads the daemon fields straight through', () => {
    const v = autostartView({
      autostart: true,
      autostart_supported: false,
      autostart_target: '/old/AudioHub.app',
      autostart_reason: 'not a bundle',
    });
    expect(v.on).toBe(true);
    expect(v.supported).toBe(false);
    expect(v.target).toBe('/old/AudioHub.app');
    expect(v.reason).toBe('not a bundle');
    expect(v.enabled).toBe(true);
    expect(v.note).toBe('orphaned');
  });

  // `supported: false` is a meaningful answer ("this machine cannot"), not a
  // missing one. Folding it into `known` would leave the reason with nowhere
  // to be shown.
  it('separates "the daemon did not say" from "the daemon said no"', () => {
    expect(autostartView({}).known).toBe(false);
    expect(autostartView({ autostart_supported: false }).known).toBe(true);
    expect(autostartView(null).known).toBe(false);
    expect(autostartView(undefined).known).toBe(false);
  });

  it('never throws and never yields undefined strings on any reachable state', () => {
    for (const { ds, label } of CELLS) {
      const v = autostartView(ds);
      expect(typeof v.target, label).toBe('string');
      expect(typeof v.reason, label).toBe('string');
      expect(typeof v.enabled, label).toBe('boolean');
    }
  });

  // A null target must not render the literal string "null" in the code block.
  it('turns a null target into an empty string, not "null"', () => {
    expect(autostartView({ autostart_target: null }).target).toBe('');
  });
});

describe('the notes the view can ask for all exist in the catalogue', () => {
  // `t()` returns the key itself when a string is missing, so comparing against
  // the key catches a note branch whose message was never written -- the
  // failure mode is a machine token rendered at the user.
  // `off` and `on` deliberately render nothing: the switch already says which
  // one you are in, and the paragraph that used to sit under `off` was a
  // description of the feature -- it lives in the wiki now (docs/plan.md §3.1,
  // user ruling 2026-08-10). The three below are the ones that carry a fact the
  // switch cannot show by itself.
  for (const key of [
    'settings.startup.unknown',
    'settings.startup.unsupported',
    'settings.startup.orphaned',
  ] as const) {
    it(`has a string for ${key}`, () => {
      const s = t(key);
      expect(s).not.toBe(key);
      expect(s.length).toBeGreaterThan(0);
    });
  }

  // The orphaned line has one job the others do not: telling the user the item
  // is still in force *and* still removable. The daemon's natural-language
  // reason is intentionally not interpolated because it may use another UI
  // locale; the detailed diagnostic remains in the service log.
  it('tells the orphaned user they can still switch it off', () => {
    const s = t('settings.startup.orphaned');
    expect(s).toContain('关闭');
    expect(s).not.toContain('not a bundle');
  });
});
