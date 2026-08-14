import { describe, expect, it } from 'vitest';
import {
  HAL_DIRECTION_OUT, peerAudioDirections, peerHasNoAudioDirections, transportCells,
} from './mode';
import type { DaemonSettings, PeerState } from '../ipc/types';

const peer = (over: Partial<PeerState>): PeerState => ({
  fingerprint: 'capability-peer',
  transport: {
    send: { latency: 'min', quality: 'auto' },
    recv: { latency: 'auto', quality: 'min' },
  },
  ...over,
});

const consumer = { mode: 'a', effective_mode: 'a' } as DaemonSettings;
const sharing = { mode: 'share', effective_mode: 'share' } as DaemonSettings;

describe('directional transport rows follow live peer capabilities', () => {
  it('keeps only TX for an output-only peer', () => {
    expect(transportCells(consumer, peer({
      peer_default_output: true,
      peer_default_input: false,
    })).map((row) => row.dir)).toEqual(['out']);
  });

  it('keeps only RX for an input-only peer', () => {
    expect(transportCells(consumer, peer({
      peer_default_output: false,
      peer_default_input: true,
    })).map((row) => row.dir)).toEqual(['in']);
  });

  it('returns no directional rows for an explicitly zero-capability peer', () => {
    expect(transportCells(consumer, peer({
      peer_default_output: false,
      peer_default_input: false,
    }))).toEqual([]);
  });

  it('keeps both rows for old or not-yet-advertised peers', () => {
    expect(transportCells(consumer, peer({})).map((row) => row.dir))
      .toEqual(['out', 'in']);
    expect(transportCells(consumer, peer({
      peer_default_output: null,
      peer_default_input: null,
    })).map((row) => row.dir)).toEqual(['out', 'in']);
  });

  it('does not apply consumer endpoint facts to Share-mode execution rows', () => {
    expect(transportCells(sharing, peer({
      peer_default_output: false,
      peer_default_input: false,
    })).map((row) => row.dir)).toEqual(['out', 'in']);
  });

  it('uses the last HAL mask while live capabilities are unknown or offline', () => {
    expect(peerAudioDirections(peer({
      peer_default_output: null,
      peer_default_input: null,
      hal_device: { requested_directions: HAL_DIRECTION_OUT },
    }))).toEqual(['out']);
    expect(peerHasNoAudioDirections(peer({
      peer_default_output: null,
      peer_default_input: null,
      hal_device: { requested_directions: 0 },
    }))).toBe(true);
  });

  it('accepts the canonical Share verdict when settings are not loaded yet', () => {
    const noSettings = null;
    const zero = peer({ peer_default_output: false, peer_default_input: false });
    expect(transportCells(noSettings, zero, true).map((row) => row.dir))
      .toEqual(['out', 'in']);
  });
});
