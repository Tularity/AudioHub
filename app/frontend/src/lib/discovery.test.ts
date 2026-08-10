// Guards for the discovery cache's notion of time.
//
// Two hazards, both of which this project has already paid for once elsewhere:
//
//  1. **A record with no timestamp rendering as a fresh one.** Before the two
//     tier ageing landed, a host that had been powered off for half an hour sat
//     in the list looking exactly like one that answered two seconds ago, and
//     clicking it bought a pairing timeout. "Cannot read it" must not render as
//     "read it" -- so an undated record is stale, never fresh.
//
//  2. **Truncation eating the newest find.** `mergeDiscover` used to do
//     `results.length = 50` on an *insertion ordered* array, so once the cache
//     filled, every host discovered afterwards was dropped on the floor. The
//     ordering is now part of the cap, and this file is what keeps it there.

import { describe, it, expect } from 'vitest';
import type { DiscoverResult } from '../ipc/types';
import {
  MAX_RESULTS, MAX_SCAN_FAILURES, RESULT_EXPIRE_MS, RESULT_STALE_MS,
  SCAN_COOLDOWN_MS, SCAN_WINDOW_MS,
  capResults, discoverKey, isExpired, isStale, remainingMs, remainingSecs, resultAge,
  scanVerdict, shouldAutoScan, shouldClearUserStop, shouldStopAfterFailure, visibleResults,
} from './discovery';

const NOW = 1_700_000_000_000;

function seen(agoMs: number, extra: Partial<DiscoverResult> = {}): DiscoverResult {
  return { fingerprint: `fp-${agoMs}`, port: 47810, lastSeen: NOW - agoMs, ...extra };
}

describe('result ageing', () => {
  it('reports age in milliseconds', () => {
    expect(resultAge(seen(1234), NOW)).toBe(1234);
  });

  it('reports an unknown age as null rather than zero', () => {
    // Zero is a concrete and excellent age. Folding "no reading" onto it is the
    // same class of lie the metrics layer forbids.
    expect(resultAge({ port: 1 }, NOW)).toBeNull();
    expect(resultAge({ port: 1, lastSeen: 0 }, NOW)).toBeNull();
    expect(resultAge({ port: 1, lastSeen: NaN }, NOW)).toBeNull();
  });

  it('calls a record fresh only below the stale threshold', () => {
    expect(isStale(seen(RESULT_STALE_MS - 1), NOW)).toBe(false);
    expect(isStale(seen(RESULT_STALE_MS), NOW)).toBe(true);
  });

  it('calls an undated record stale but never expired', () => {
    const undated: DiscoverResult = { instance: 'ghost', port: 47810 };
    expect(isStale(undated, NOW)).toBe(true);
    expect(isExpired(undated, NOW)).toBe(false);
  });

  it('expires a record only at the expiry threshold', () => {
    expect(isExpired(seen(RESULT_EXPIRE_MS - 1), NOW)).toBe(false);
    expect(isExpired(seen(RESULT_EXPIRE_MS), NOW)).toBe(true);
  });

  it('keeps stale records visible so the list does not jump under a finger', () => {
    const stale = seen(RESULT_STALE_MS + 1000);
    expect(isStale(stale, NOW)).toBe(true);
    expect(visibleResults([stale], NOW)).toHaveLength(1);
  });
});

describe('visible results', () => {
  it('drops expired records and sorts the rest newest first', () => {
    const list = [seen(30_000), seen(RESULT_EXPIRE_MS + 1), seen(1_000), seen(50_000)];
    const out = visibleResults(list, NOW);
    expect(out.map((d) => NOW - (d.lastSeen || 0))).toEqual([1_000, 30_000, 50_000]);
  });

  it('does not mutate the input array', () => {
    const list = [seen(50_000), seen(1_000)];
    const before = list.slice();
    visibleResults(list, NOW);
    expect(list).toEqual(before);
  });
});

describe('cap', () => {
  it('keeps the newest entries when the cache overflows', () => {
    // Insertion order is deliberately the *reverse* of recency here: the old
    // `results.length = 50` would have kept exactly the wrong half.
    const list: DiscoverResult[] = [];
    for (let i = 0; i < MAX_RESULTS + 10; i++) {
      list.push({ fingerprint: `fp${i}`, port: 47810, lastSeen: NOW - (MAX_RESULTS + 10 - i) * 100 });
    }
    const out = capResults(list, NOW);
    expect(out).toHaveLength(MAX_RESULTS);
    expect(out[0].fingerprint).toBe(`fp${MAX_RESULTS + 9}`);
    // The very first insertions are the oldest, and they are the ones dropped.
    expect(out.some((d) => d.fingerprint === 'fp0')).toBe(false);
  });

  it('leaves a short list alone apart from ordering', () => {
    const out = capResults([seen(9_000), seen(1_000)], NOW);
    expect(out.map((d) => d.lastSeen)).toEqual([NOW - 1_000, NOW - 9_000]);
  });
});

describe('scan window', () => {
  it('counts down and floors at zero', () => {
    const deadline = NOW + SCAN_WINDOW_MS;
    expect(remainingMs(deadline, NOW)).toBe(SCAN_WINDOW_MS);
    expect(remainingMs(deadline, NOW + SCAN_WINDOW_MS + 5_000)).toBe(0);
  });

  it('treats a zero deadline as "no window running"', () => {
    expect(remainingMs(0, NOW)).toBe(0);
  });

  it('rounds the displayed seconds up, so a spinning scan never reads zero', () => {
    expect(remainingSecs(NOW + 1, NOW)).toBe(1);
    expect(remainingSecs(NOW + 1_001, NOW)).toBe(2);
    expect(remainingSecs(NOW, NOW)).toBe(0);
  });
});

describe('auto scan on entering the pair view', () => {
  const base = { conn: 'online', running: false, userStopped: false, lastStopAt: 0, now: NOW };

  it('starts on a fresh entry with the daemon online', () => {
    expect(shouldAutoScan(base)).toBe(true);
  });

  it('does not stack a second loop onto a running one', () => {
    expect(shouldAutoScan({ ...base, running: true })).toBe(false);
  });

  it('stays quiet while the daemon is not online', () => {
    // Firing here only queues RPCs that are guaranteed to time out.
    for (const conn of ['connecting', 'starting', 'offline']) {
      expect(shouldAutoScan({ ...base, conn })).toBe(false);
    }
  });

  it('respects a manual stop', () => {
    expect(shouldAutoScan({ ...base, userStopped: true })).toBe(false);
  });

  it('holds off for the cooldown after any stop', () => {
    expect(shouldAutoScan({ ...base, lastStopAt: NOW - (SCAN_COOLDOWN_MS - 1) })).toBe(false);
    expect(shouldAutoScan({ ...base, lastStopAt: NOW - SCAN_COOLDOWN_MS })).toBe(true);
  });
});

describe('the manual-stop flag', () => {
  it('survives a quick trip to another page', () => {
    expect(shouldClearUserStop(NOW - (SCAN_COOLDOWN_MS - 1), NOW)).toBe(false);
  });

  it('lapses once the user has been away for a cooldown', () => {
    expect(shouldClearUserStop(NOW - SCAN_COOLDOWN_MS, NOW)).toBe(true);
  });

  it('never lapses while the user has not left at all', () => {
    expect(shouldClearUserStop(0, NOW)).toBe(false);
  });
});

describe('the scan loop stop table', () => {
  // The loop used to live in a component ref, where unmounting was the backstop
  // for every condition this table now has to state out loud. Moving it to a
  // module singleton turned "one extra loop" into "a loop that never ends", so
  // each row below is a way the loop is required to die.
  const live = { mine: 7, gen: 7, running: true, conn: 'online', deadlineAt: NOW + 10_000, now: NOW };

  it('keeps going while it is the live loop, inside the window, online', () => {
    expect(scanVerdict(live)).toBe('go');
  });

  it('retires a loop whose generation has been superseded', () => {
    // This is the whole anti-stacking guarantee: two discover.run calls queued
    // on the one shared IPC connection head-of-line block everything else.
    expect(scanVerdict({ ...live, gen: 8 })).toBe('superseded');
  });

  it('retires a loop once the flag says the scan is over', () => {
    expect(scanVerdict({ ...live, running: false })).toBe('superseded');
  });

  it('checks its own supersession before anything it would act on', () => {
    // A dead loop calling stopScan() would shoot down the live one that just
    // replaced it, so 'superseded' has to win over both other verdicts.
    expect(scanVerdict({ ...live, gen: 8, deadlineAt: NOW - 1, conn: 'offline' })).toBe('superseded');
  });

  it('stops at the window deadline even if the timer never fired', () => {
    // System sleep drifts setTimeout well past the window; the loop is what
    // actually has to notice.
    expect(scanVerdict({ ...live, deadlineAt: NOW })).toBe('deadline');
    expect(scanVerdict({ ...live, deadlineAt: NOW - 60_000 })).toBe('deadline');
  });

  it('treats a zero deadline as no window rather than an expired one', () => {
    expect(scanVerdict({ ...live, deadlineAt: 0 })).toBe('go');
  });

  it('stops when the daemon is no longer online', () => {
    expect(scanVerdict({ ...live, conn: 'offline' })).toBe('offline');
    expect(scanVerdict({ ...live, conn: 'connecting' })).toBe('offline');
  });

  it('gives up after a run of consecutive failures instead of retrying forever', () => {
    expect(shouldStopAfterFailure(MAX_SCAN_FAILURES - 1)).toBe(false);
    expect(shouldStopAfterFailure(MAX_SCAN_FAILURES)).toBe(true);
  });
});

describe('keys', () => {
  it('prefers the fingerprint', () => {
    expect(discoverKey({ fingerprint: 'abc', instance: 'x', port: 1 })).toBe('abc');
  });

  it('falls back to instance and port, and names the missing instance', () => {
    expect(discoverKey({ instance: 'x', port: 47810 })).toBe('x-47810');
    expect(discoverKey({ port: 47810 })).toBe('unknown-47810');
  });
});
