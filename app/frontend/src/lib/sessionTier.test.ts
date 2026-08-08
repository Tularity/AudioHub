// Guards for `sessionTier()` -- the PER-SESSION connectivity readout the stats
// page prints (`SessionStats.transport`, daemon-side `SessionEntry::media_tier`).
//
// Two things can silently break here and nothing else in the tree would notice:
//
//   1. **The undetermined state collapsing into tier0.** A daemon that predates
//      the field sends nothing; a `?? 'tier0'` or a bare cast turns that silence
//      into "direct connection" and the stats page starts asserting a fact it
//      was never told. plan §16.4 rule 5 names exactly this: "already judged to
//      be direct" and "nothing has been judged" may not render the same way.
//   2. **An unknown string leaking through as a tier.** `Stats.tsx` indexes
//      `TIER_LABEL[tier]`, so a value this function lets past that the label
//      table has no row for renders as an empty badge -- visible to the user,
//      invisible to every test and every type check, because `as EffectiveTier`
//      would have satisfied the compiler.

import { describe, it, expect } from 'vitest';
import type { SessionInfo } from '../ipc/types';
import { isDegradedTier, sessionTier, TIER_LABEL } from './tier';

/** A session carrying `transport`, with the required `SessionInfo` fields. */
function session(transport?: string | null): SessionInfo {
  return {
    id: 1,
    peer_fingerprint: 'aa:bb:cc',
    kind: 'spk',
    dir: 'send',
    stats: { transport },
  };
}

describe('sessionTier: the three real tiers come through verbatim', () => {
  it('reads tier0, tier1 and tier2 off the session stats', () => {
    expect(sessionTier(session('tier0'))).toBe('tier0');
    expect(sessionTier(session('tier1'))).toBe('tier1');
    expect(sessionTier(session('tier2'))).toBe('tier2');
  });

  // Whatever this function admits is handed straight to `TIER_LABEL[...]` by
  // Stats.tsx. If the two ever disagree the badge renders blank, so pin the
  // agreement here rather than trusting two lists to stay in step.
  it('only ever returns values TIER_LABEL can render', () => {
    for (const tier of ['tier0', 'tier1', 'tier2'] as const) {
      const got = sessionTier(session(tier));
      expect(got).not.toBeNull();
      expect(TIER_LABEL[got!]).toBeTruthy();
    }
  });
});

describe('sessionTier: undetermined is null, never tier0', () => {
  // The older-daemon case, and the one that matters most: silence is not a
  // claim of directness.
  it('returns null when the daemon did not report the field at all', () => {
    expect(sessionTier(session(undefined))).toBeNull();
    expect(sessionTier(session(undefined))).not.toBe('tier0');
  });

  it('returns null for an explicit null', () => {
    expect(sessionTier(session(null))).toBeNull();
    expect(sessionTier(session(null))).not.toBe('tier0');
  });

  it('returns null when the session has no stats block, or no session at all', () => {
    expect(sessionTier({ id: 1, peer_fingerprint: 'x', kind: 'spk', dir: 'send' })).toBeNull();
    expect(sessionTier({ id: 1, peer_fingerprint: 'x', kind: 'spk', dir: 'send', stats: null }))
      .toBeNull();
    expect(sessionTier(null)).toBeNull();
    expect(sessionTier(undefined)).toBeNull();
  });

  // `'auto'` is the one wrong value most likely to arrive here by accident: it
  // is a legal value of the SETTING (`PeerState.transport.tier`), so any future
  // refactor that wires the wrong source in produces exactly this string. It is
  // never a live path -- "let the daemon decide" cannot describe where bytes
  // went -- so it has to fall out as undetermined rather than be forwarded.
  it('rejects "auto", which describes a preference and never a live path', () => {
    expect(sessionTier(session('auto'))).toBeNull();
  });

  it('rejects an unrecognised tier string instead of passing it through', () => {
    expect(sessionTier(session('tier3'))).toBeNull();
    expect(sessionTier(session(''))).toBeNull();
    expect(sessionTier(session('TIER1'))).toBeNull();
  });
});

describe('sessionTier feeds the badge rule Stats.tsx applies', () => {
  // Stats.tsx shows the warn badge on `isDegradedTier(sessionTier(info))`.
  // tier0 and undetermined must both stay off it, for opposite reasons: one is
  // "fine, and a badge here would train the eye to ignore this spot"
  // (§16.4 rule 3), the other is "we were not told".
  it('raises the degraded badge only for tier1 and tier2', () => {
    expect(isDegradedTier(sessionTier(session('tier1')))).toBe(true);
    expect(isDegradedTier(sessionTier(session('tier2')))).toBe(true);
    expect(isDegradedTier(sessionTier(session('tier0')))).toBe(false);
    expect(isDegradedTier(sessionTier(session(undefined)))).toBe(false);
  });
});
