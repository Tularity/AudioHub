// Guards for the one rule metrics.ts repeats in every docblock: a missing
// reading is `undefined`, never `0`.
//
// This is the failure mode the project has already shipped once. `0` is a
// plausible-looking number, so a `?? 0` produces a reading that renders, sorts,
// and grades like a real measurement -- "0 ms network", "0% clipping",
// "excellent" -- while meaning "we never measured this". Every assertion below
// pins a specific field that would read as good news if it folded.

import { describe, it, expect } from 'vitest';
import type { PeerState, SessionInfo } from '../ipc/types';
import { readPeerNet, readQuality, medianOf5, qualityTone } from './metrics';

function sess(stats: SessionInfo['stats']): SessionInfo {
  return { id: 1, peer_fingerprint: 'aa:bb', kind: 'spk', dir: 'recv', stats };
}

describe('readPeerNet: "still measuring" is not "0 ms"', () => {
  // min-RTT has not filled its window yet. 0 ms would be the best possible
  // number on a metric where lower is better.
  it('keeps ms undefined when the daemon reports net_ms: null', () => {
    const r = readPeerNet({ fingerprint: 'aa', online: true, net_ms: null, rtt_ms: null });
    expect(r).toBeDefined();
    expect(r?.ms).toBeUndefined();
    expect(r?.ms).not.toBe(0);
  });

  it('passes a real reading through untouched', () => {
    const r = readPeerNet({ fingerprint: 'aa', online: true, net_ms: 0.58, rtt_ms: 1.16 });
    expect(r?.ms).toBe(0.58);
    expect(r?.rttMs).toBe(1.16);
  });

  // A remembered round-trip is a statement about the past; hanging it on an
  // offline card reads as "it is still this fast".
  it('renders nothing at all for an offline peer, even with values present', () => {
    const p: PeerState = { fingerprint: 'aa', online: false, net_ms: 0.58, rtt_ms: 1.16 };
    expect(readPeerNet(p)).toBeUndefined();
  });

  // Absent keys (old daemon) and null (new daemon, still measuring) must not
  // collapse: the first means "never shows up", the second means "wait a moment".
  it('distinguishes absent keys from null values', () => {
    expect(readPeerNet({ fingerprint: 'aa', online: true })).toBeUndefined();
    expect(readPeerNet({ fingerprint: 'aa', online: true, net_ms: null })).toBeDefined();
  });

  it('treats NaN as unmeasured rather than as a number', () => {
    const r = readPeerNet({ fingerprint: 'aa', online: true, net_ms: NaN });
    expect(r?.ms).toBeUndefined();
  });
});

describe('readQuality: absent components stay absent', () => {
  // `clip_ratio: null` is "the clipping window has not filled". `?? 0` would
  // state "measured, and nothing clipped" -- the exact sentence the docblock
  // forbids.
  it('keeps clipPct undefined when clip_ratio is null', () => {
    const q = readQuality(sess({ quality: { grade: 'good', clip_ratio: null } }));
    expect(q?.clipPct).toBeUndefined();
    expect(q?.clipPct).not.toBe(0);
  });

  it('converts a real ratio to a percentage', () => {
    const q = readQuality(sess({ quality: { grade: 'good', clip_ratio: 0.02 } }));
    expect(q?.clipPct).toBeCloseTo(2);
  });

  // With a component missing, the min over present components is only an upper
  // bound. Reporting the bound as the grade is how a stream that is actively
  // clipping gets labelled "good" for its first 10-20 seconds.
  it('drops an unrecognised grade instead of falling back to a real one', () => {
    const q = readQuality(sess({ quality: { grade: 'unknown', worst: 'none' } }));
    expect(q).toBeDefined();
    expect(q?.grade).toBeUndefined();
    expect(q?.worst).toBeUndefined();
  });

  it('drops `worst` whenever the grade itself did not survive', () => {
    const q = readQuality(sess({ quality: { grade: 'unknown', worst: 'level' } }));
    expect(q?.worst).toBeUndefined();
  });

  // 0 Hz means the rung never parsed, not "a stream with no bandwidth".
  it('treats 0 Hz bandwidth and 0 Hz wire rate as unread', () => {
    const q = readQuality(sess({ quality: { grade: 'good', bandwidth_hz: 0, wire_rate_hz: 0 } }));
    expect(q?.bandwidthKhz).toBeUndefined();
    expect(q?.wireRateKhz).toBeUndefined();
  });

  // Bit depth is read independently and must never be inferred from bitrate.
  it('leaves wireDepth undefined for an empty string rather than assuming s16', () => {
    const q = readQuality(sess({ quality: { grade: 'good', wire_depth: '' } }));
    expect(q?.wireDepth).toBeUndefined();
  });

  it('falls back to peer_quality and marks the reading as second-hand', () => {
    const q = readQuality(sess({ quality: null, peer_quality: { grade: 'fair' } }));
    expect(q?.grade).toBe('fair');
    expect(q?.fromPeer).toBe(true);
  });

  // The fallback order cannot invert: a local measurement is first-hand.
  it('prefers the local reading over the peer\'s when both exist', () => {
    const q = readQuality(sess({
      quality: { grade: 'poor' },
      peer_quality: { grade: 'excellent' },
    }));
    expect(q?.grade).toBe('poor');
    expect(q?.fromPeer).toBe(false);
  });

  it('returns undefined when neither side measured anything', () => {
    expect(readQuality(sess({ quality: null, peer_quality: null }))).toBeUndefined();
    expect(readQuality(null)).toBeUndefined();
  });
});

describe('medianOf5 / qualityTone: empty input yields no reading, not zero', () => {
  it('returns undefined for an empty sample list', () => {
    expect(medianOf5([])).toBeUndefined();
    expect(medianOf5([])).not.toBe(0);
  });

  it('ignores non-finite samples instead of counting them as 0', () => {
    expect(medianOf5([NaN, Infinity])).toBeUndefined();
    expect(medianOf5([10, NaN, 20])).toBe(15);
  });

  it('medians the last five samples only', () => {
    expect(medianOf5([999, 1, 2, 3, 4, 5])).toBe(3);
  });

  // The four quality dots were removed on 2026-08-11; the grade's only visual
  // encoding left on the card is the tone class on the kHz/bit readout. Which
  // makes this mapping load-bearing in a way it was not before: if two adjacent
  // grades ever resolve to the same tone, a stream that just dropped a rung
  // looks exactly like one that did not.
  it('gives every grade its own tone', () => {
    const tones = (['excellent', 'good', 'fair', 'poor'] as const).map(qualityTone);
    expect(tones).toEqual(['ok', 'accent', 'warn', 'danger']);
    expect(new Set(tones).size).toBe(4);
  });
});
