// Guards for the stop tables and for what happens to a rung nobody recognises.
//
// Two distinct hazards live in this module:
//
//  1. **Table drift.** The fallback tables here are a hand-copied mirror of the
//     daemon's `LATENCY_STOPS_MS` and `LADDER`. Nothing links the two, so a rung
//     added or reordered on the Rust side leaves this file quietly stale, and
//     users on an older daemon get a slider whose stops the daemon never named.
//     The bit-depth round is exactly this: the ladder grew `s24`/`f32` and the
//     frontend's copy still described everything as 16-bit.
//
//  2. **Silent translation.** The deleted `QUALITY_LEGACY` compatibility layer
//     mapped unknown ids onto real rungs, and one of its three read paths missed
//     the normalisation -- so the same stored value rendered as
//     "PCM 32 kHz - 16 bit" on one page and as raw `pcm32k` on another, with
//     nothing anywhere raising an error. The rule that replaced it: the frontend
//     translates nothing. An id it cannot name is shown verbatim so it is
//     visibly foreign, and the daemon is the one that resets it and reports
//     `*_reset_from` for the UI to explain.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, it, expect } from 'vitest';
import type { DaemonSettings, QualityStop } from '../ipc/types';
import { latencyStops, qualityStops, stopLabel, normLatency } from './transportStops';

// app/frontend/src/lib -> repo root
const ROOT = fileURLToPath(new URL('../../../../', import.meta.url));

function rust(rel: string): string {
  return readFileSync(ROOT + rel, 'utf8');
}

/** Values of the daemon's `LATENCY_STOPS_MS` const, read from the Rust source. */
function backendLatencyStops(): number[] {
  const src = rust('core/audiohub-ipc/src/transport.rs');
  const m = src.match(/pub const LATENCY_STOPS_MS:\s*\[u16;\s*\d+\]\s*=\s*\[([^\]]*)\]/);
  // Throwing beats returning [] -- a cross-language check that silently matches
  // nothing is worse than no check, because it reads green forever.
  if (!m) throw new Error('could not locate LATENCY_STOPS_MS in transport.rs');
  return m[1].split(',').map((s) => Number(s.trim())).filter((n) => Number.isFinite(n));
}

const DEPTH_TAG: Record<string, string> = { S16: '16', S24: '24', F32: '32f' };
const DEPTH_WIRE: Record<string, string> = { S16: 's16', S24: 's24', F32: 'f32' };
const DEPTH_BITS: Record<string, number> = { S16: 16, S24: 24, F32: 32 };

/**
 * The daemon's `LADDER`, projected exactly the way `quality_stops()` projects it:
 * reversed (LADDER is best-first, the slider is worst-first), with the id built
 * by `quality_stop_id()` and kbps by `WireFormat::kbps()`.
 */
function backendQualityRungs(): { id: string; kbps: number; rate: number; depth: string }[] {
  const src = rust('core/audiohub-net/src/media.rs');
  const m = src.match(/pub const LADDER:\s*\[WireFormat;\s*\d+\]\s*=\s*\[([\s\S]*?)\n\];/);
  if (!m) throw new Error('could not locate LADDER in media.rs');
  const rows = [...m[1].matchAll(/rate_hz:\s*(\d+),\s*depth:\s*WireDepth::(\w+)/g)];
  if (!rows.length) throw new Error('LADDER matched but held no WireFormat rows');
  return rows.map((r) => {
    const rate = Number(r[1]);
    const d = r[2];
    return {
      id: `pcm${rate / 1000}k${DEPTH_TAG[d]}`,
      kbps: (rate / 1000) * DEPTH_BITS[d],
      rate,
      depth: DEPTH_WIRE[d],
    };
  }).reverse();
}

describe('fallback tables still match the daemon', () => {
  it('latency stops mirror LATENCY_STOPS_MS, prefixed by auto', () => {
    const want = ['auto', ...backendLatencyStops().map(String)];
    expect(latencyStops(null).map((s) => s.value)).toEqual(want);
  });

  it('latency stops are the ladder the plan pins: 0 through 1000', () => {
    const values = latencyStops(null).map((s) => s.value);
    expect(values).toEqual(
      ['auto', '0', '10', '20', '30', '50', '75', '100', '150', '200', '300', '500', '750', '1000'],
    );
  });

  it('quality PCM rungs mirror LADDER in id, order, rate, depth and bitrate', () => {
    const rungs = backendQualityRungs();
    const pcm = qualityStops(null).map((s) => s.value).filter((v) => v.startsWith('pcm'));
    expect(pcm).toEqual(rungs.map((r) => r.id));

    // The fallback carries kbps/rate/depth too; assert the arithmetic that the
    // bit-depth round got wrong (kbps = kHz x bits, not kHz x 16).
    expect(rungs.map((r) => r.kbps)).toEqual([256, 384, 512, 768, 1152, 1536]);
    expect(rungs.map((r) => r.depth)).toEqual(['s16', 's16', 's16', 's16', 's24', 'f32']);
  });

  it('offers auto plus the three unavailable opus rungs ahead of PCM', () => {
    const stops = qualityStops(null);
    expect(stops.map((s) => s.value).slice(0, 4)).toEqual(['auto', 'opus64', 'opus128', 'opus256']);
    // Unknown-means-unavailable: a build with no opus must not offer a rung it
    // cannot send.
    for (const id of ['opus64', 'opus128', 'opus256']) {
      expect(stops.find((s) => s.value === id)?.available).toBe(false);
    }
    expect(stops.find((s) => s.value === 'auto')?.available).toBe(true);
  });
});

describe('daemon-supplied tables win over the fallback', () => {
  it('uses the daemon list when present', () => {
    const ds = { latency_stops_ms: [0, 42] } as DaemonSettings;
    expect(latencyStops(ds).map((s) => s.value)).toEqual(['auto', '0', '42']);
  });

  it('drops duplicate and non-finite latency stops', () => {
    const ds = { latency_stops_ms: [10, 10, NaN, 20] } as unknown as DaemonSettings;
    expect(latencyStops(ds).map((s) => s.value)).toEqual(['auto', '10', '20']);
  });

  it('falls back when the daemon sends an empty list', () => {
    const ds = { latency_stops_ms: [] } as unknown as DaemonSettings;
    expect(latencyStops(ds).map((s) => s.value).length).toBeGreaterThan(2);
  });
});

describe('unrecognised rungs are surfaced verbatim, never translated', () => {
  // A rung the daemon added but the catalogue has not caught up with must still
  // appear, labelled with its raw id. Skipping it makes a real, selectable stop
  // vanish from the UI with nobody notified.
  it('renders an unknown quality id using the id itself as the label', () => {
    const ds = {
      quality_stops: [{ id: 'pcm96k32f', kbps: 3072, available: true }] as QualityStop[],
    } as DaemonSettings;
    const stops = qualityStops(ds);
    expect(stops).toHaveLength(1);
    expect(stops[0].value).toBe('pcm96k32f');
    expect(stops[0].label).toBe('pcm96k32f');
    // No invented sublabel: a qualitative word made up here would be compared
    // against the real ones.
    expect(stops[0].sublabel).toBeUndefined();
  });

  // The legacy spelling the deleted compat layer used to rewrite. It must come
  // back out unchanged -- mapping it to `pcm32k16` here is precisely the silent
  // translation that made two pages disagree.
  it('does not map the legacy pcm32k id onto the pcm32k16 rung', () => {
    const stops = qualityStops(null);
    expect(stopLabel(stops, 'pcm32k')).toBe('pcm32k');
    expect(stopLabel(stops, 'pcm32k')).not.toBe(stopLabel(stops, 'pcm32k16'));
  });

  it('shows a known rung with its catalogue label, not its id', () => {
    const stops = qualityStops(null);
    const label = stopLabel(stops, 'pcm48k24');
    expect(label).not.toBe('pcm48k24');
    expect(label).toBeTruthy();
  });

  // "unset" and "set to auto" execute the same but must not render the same,
  // so the caller decides -- it does not receive a fabricated default.
  it('returns null for an unset value instead of defaulting to auto', () => {
    const stops = qualityStops(null);
    expect(stopLabel(stops, null)).toBeNull();
    expect(stopLabel(stops, undefined)).toBeNull();
    expect(stopLabel(stops, '')).toBeNull();
  });
});

describe('normLatency', () => {
  it('folds the legacy min spelling onto 0 and normalises numeric strings', () => {
    expect(normLatency('min')).toBe('0');
    expect(normLatency('200.0')).toBe('200');
    expect(normLatency('auto')).toBe('auto');
  });

  // Unrecognised input is returned untouched rather than snapped to a rung.
  it('returns an unparseable value unchanged', () => {
    expect(normLatency('quite fast')).toBe('quite fast');
  });
});
