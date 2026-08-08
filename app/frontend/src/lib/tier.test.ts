// Guards for `effectiveTier()`'s three-state verdict.
//
// The state this file protects is the one plan §16.4 rule 5 calls out by name:
// "already judged to be direct" and "no idea" are two different things and must
// never render the same. In this module that is `'tier0'` vs `null`. A single
// `??`, a flipped lookup order, or an early `return 'tier0'` collapses them,
// and nothing else in the tree would notice -- the UI would just quietly start
// telling users "direct connection" about links it has not judged at all.

import { describe, it, expect } from 'vitest';
import type { DaemonInfo, PeerState } from '../ipc/types';
import { effectiveTier, tierUnknownWhy, isDegradedTier } from './tier';

const FP = 'aa:bb:cc';

function peer(over: Partial<PeerState> = {}): PeerState {
  return { fingerprint: FP, online: true, ...over };
}

function daemon(guard?: DaemonInfo['latency_guard']): DaemonInfo {
  return { fingerprint: 'self', latency_guard: guard };
}

describe('effectiveTier: the three states stay three states', () => {
  it('reports tier0 for an online peer absent from both degraded tables', () => {
    expect(effectiveTier(daemon({ tcp_media: [] }), peer())).toBe('tier0');
  });

  it('reports tier1 when the peer appears in tcp_media', () => {
    const d = daemon({ tcp_media: [{ fingerprint: FP }], mux: [] });
    expect(effectiveTier(d, peer())).toBe('tier1');
  });

  it('reports tier2 when the peer appears in mux', () => {
    const d = daemon({ tcp_media: [{ fingerprint: FP }], mux: [{ fingerprint: FP }] });
    expect(effectiveTier(d, peer())).toBe('tier2');
  });

  // The ordering guard. A Tier 2 link is present in BOTH tables, so testing mux
  // in isolation proves nothing: swap the two `if`s and every Tier 2 peer
  // silently renders as Tier 1 while this same fixture still has a mux entry.
  it('prefers tier2 over tier1 when the peer is in both tables at once', () => {
    const d = daemon({
      tcp_media: [{ fingerprint: FP, alive: true }],
      mux: [{ fingerprint: FP, alive: true }],
    });
    expect(effectiveTier(d, peer())).toBe('tier2');
    expect(effectiveTier(d, peer())).not.toBe('tier1');
  });

  it('does not let another peer\'s degraded link leak onto this peer', () => {
    const d = daemon({ tcp_media: [{ fingerprint: 'someone-else' }], mux: [] });
    expect(effectiveTier(d, peer())).toBe('tier0');
  });
});

describe('effectiveTier: undetermined is null, never tier0', () => {
  it('returns null when latency_guard is absent (older daemon)', () => {
    expect(effectiveTier(daemon(undefined), peer())).toBeNull();
  });

  it('returns null when tcp_media is not an array', () => {
    const d = { fingerprint: 'self', latency_guard: { tcp_media: undefined } } as DaemonInfo;
    expect(effectiveTier(d, peer())).toBeNull();
  });

  // Offline peers have no MediaPath at all, so there is nothing to have judged.
  // Writing "direct" on an offline card asserts something not true right now.
  it('returns null for an offline peer rather than falling through to tier0', () => {
    const d = daemon({ tcp_media: [] });
    expect(effectiveTier(d, peer({ online: false }))).toBeNull();
    expect(effectiveTier(d, peer({ online: false }))).not.toBe('tier0');
  });

  it('returns null when online is absent entirely (unknown, not assumed up)', () => {
    expect(effectiveTier(daemon({ tcp_media: [] }), peer({ online: undefined }))).toBeNull();
  });

  it('returns null for a missing peer or missing fingerprint', () => {
    expect(effectiveTier(daemon({ tcp_media: [] }), null)).toBeNull();
    const noFp = { fingerprint: '', online: true } as PeerState;
    expect(effectiveTier(daemon({ tcp_media: [] }), noFp)).toBeNull();
  });

  // Version inference from the module header: `mux_status()` and
  // `MediaPath::Framed` shipped in the same commit, so a daemon that reports
  // tcp_media but no mux cannot be running Tier 2 -- that row is Tier 1, not
  // "unknown". Degrading it to null would hide a real degradation banner.
  it('still reports tier1 when mux is absent but tcp_media lists the peer', () => {
    expect(effectiveTier(daemon({ tcp_media: [{ fingerprint: FP }] }), peer())).toBe('tier1');
  });
});

describe('tierUnknownWhy: the two causes need opposite next steps', () => {
  it('says unsupported when the daemon does not report the table', () => {
    expect(tierUnknownWhy(daemon(undefined))).toBe('unsupported');
    expect(tierUnknownWhy({ fingerprint: 'self' })).toBe('unsupported');
  });

  it('says offline once the table exists but holds no connection', () => {
    expect(tierUnknownWhy(daemon({ tcp_media: [] }))).toBe('offline');
  });
});

describe('isDegradedTier', () => {
  it('counts only tier1 and tier2 as degraded', () => {
    expect(isDegradedTier('tier1')).toBe(true);
    expect(isDegradedTier('tier2')).toBe(true);
  });

  // tier0 and null are both "not degraded" but for opposite reasons; neither
  // may raise the degradation banner.
  it('treats tier0 and the undetermined state as not degraded', () => {
    expect(isDegradedTier('tier0')).toBe(false);
    expect(isDegradedTier(null)).toBe(false);
  });
});
