import { describe, expect, it } from 'vitest';
import { trayVolumeOf } from './trayVolume';
import type { SessionInfo } from '../ipc/types';

function sess(over: Record<string, unknown> = {}): SessionInfo {
  return {
    id: 7,
    kind: 'spk',
    dir: 'send',
    peer_fingerprint: 'ff',
    stats: { volume: { scalar: 0.5, muted: false, adjustable: true } },
    ...over,
  } as unknown as SessionInfo;
}

describe('trayVolumeOf', () => {
  it('offers the peer speaker volume in mode A', () => {
    expect(trayVolumeOf('a', [sess()])).toEqual({ id: 7, scalar: 0.5, muted: false });
  });

  // The card itself passes `sess={null}` in mode B (views/Peers.tsx), and share
  // mode owns no outbound spk session. A menu-bar slider that appears in a mode
  // where the card refuses to show one is the exact inconsistency this guards.
  it('offers nothing outside mode A', () => {
    for (const mode of ['b', 'share', '', 'A']) {
      expect(trayVolumeOf(mode, [sess()])).toBeNull();
    }
  });

  it('ignores sessions that are not the outbound speaker one', () => {
    expect(trayVolumeOf('a', [sess({ kind: 'mic' })])).toBeNull();
    expect(trayVolumeOf('a', [sess({ dir: 'recv' })])).toBeNull();
  });

  // "Volume has not arrived yet" and "volume is 0" are different facts. Folding
  // the first into a slider at zero would show a muted-looking control for a
  // session whose level is simply unknown.
  it('waits for the daemon to report a volume', () => {
    expect(trayVolumeOf('a', [sess({ stats: {} })])).toBeNull();
    expect(trayVolumeOf('a', [sess({ stats: { volume: null } })])).toBeNull();
  });

  // plan §7.2: when the peer's device has no writable volume the daemon takes
  // it over as a send-side software gain. `adjustable` stays false because it
  // describes the *peer's device*, but the slider is real.
  it('accepts a software-gain session even though the peer device is not adjustable', () => {
    const s = sess({
      stats: {
        volume: { scalar: 0.25, muted: false, adjustable: false },
        volume_software_gain: true,
      },
    });
    expect(trayVolumeOf('a', [s])).toEqual({ id: 7, scalar: 0.25, muted: false });
  });

  it('rejects a session that is neither adjustable nor software-gain', () => {
    const s = sess({ stats: { volume: { scalar: 0.25, muted: false, adjustable: false } } });
    expect(trayVolumeOf('a', [s])).toBeNull();
  });

  it('carries mute through without zeroing the level', () => {
    const s = sess({ stats: { volume: { scalar: 0.8, muted: true, adjustable: true } } });
    expect(trayVolumeOf('a', [s])).toEqual({ id: 7, scalar: 0.8, muted: true });
  });

  it('clamps and rejects nonsense scalars', () => {
    expect(trayVolumeOf('a', [sess({ stats: { volume: { scalar: 4, adjustable: true } } })]))
      .toEqual({ id: 7, scalar: 1, muted: false });
    expect(trayVolumeOf('a', [sess({ stats: { volume: { scalar: -1, adjustable: true } } })]))
      .toEqual({ id: 7, scalar: 0, muted: false });
    expect(trayVolumeOf('a', [sess({ stats: { volume: { scalar: NaN, adjustable: true } } })]))
      .toBeNull();
  });

  it('survives an absent session list', () => {
    expect(trayVolumeOf('a', null)).toBeNull();
    expect(trayVolumeOf('a', [])).toBeNull();
  });
});
