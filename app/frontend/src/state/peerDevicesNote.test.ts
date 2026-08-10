// The one sentence under the per-peer virtual-device list.
//
// It moved hosts on 2026-08-10 (user instruction 18): the list left its own card
// on the detail page and became a sub-block under "connectivity" inside the
// transport card. The move also changed one of its four branches -- the "no
// devices" arm used to pick between `halReasonText()` and a mode-A sentence, and
// the mode-A sentence no longer exists because the whole block is now gated on
// the user having *asked* for mode B.
//
// Moving a branch tree and rewriting one of its arms in the same edit is the
// combination this repo keeps losing to, and the losses have all had the same
// shape: the pure half is well covered, the wiring is not, and the wrong arm
// renders forever without anything failing (`regress/transport-per-peer.mjs`
// opens with that story). Hence this file.

import { describe, it, expect } from 'vitest';
import { peerDevicesNote } from './mode';
import { t } from '../i18n';
import type { DaemonInfo, PeerHalDevice, PeerState } from '../ipc/types';

const FP = 'aa:bb:cc';

function peer(over: Partial<PeerState> = {}): PeerState {
  return { fingerprint: FP, name: 'thirty-win', ...over };
}

/** A peer that owns two devices. `peerDeviceRows` keys off `hal_device` alone. */
function withDevices(dev: Partial<PeerHalDevice>, over: Partial<PeerState> = {}): PeerState {
  return peer({
    hal_device: { out_name: 'AudioHub spk', in_name: 'AudioHub mic', ...dev },
    ...over,
  });
}

// No `hal.devices` entry for this peer: `peerDeviceRows` keys off the peer's own
// `hal_device`, and the daemon's table only supplies diagnostic extras. Leaving
// it empty proves the note does not secretly depend on those extras.
const daemon: DaemonInfo = { fingerprint: 'local-fp' };

describe('peerDevicesNote: the "why is there nothing here" arm', () => {
  // The block only renders when the user has asked for mode B, so this arm is
  // the answer to "I picked mode B, where are my devices?". Rendering nothing,
  // or an empty string, at exactly that moment is the failure worth guarding.
  it('never returns an empty string, whatever the daemon reports', () => {
    const reasons = [
      'capacity', 'no_driver', 'removed_while_offline', 'mode_a', 'mode_share',
      'some_future_reason', '', null, undefined,
    ];
    for (const reason of reasons) {
      const note = peerDevicesNote(peer({ hal_reason: reason }), daemon);
      expect(note.trim(), `empty note for hal_reason=${String(reason)}`).not.toBe('');
    }
  });

  // `t()` echoes the key back when the catalogue has no entry, so a missing
  // string shows up on screen as `halReason.capacity` rather than failing.
  it('resolves to real copy, not to a message key', () => {
    for (const reason of ['capacity', 'no_driver', 'mode_share', null]) {
      expect(peerDevicesNote(peer({ hal_reason: reason }), daemon)).not.toMatch(/^halReason\./);
    }
  });

  // The arm that was rewritten. `detail.devices.modeA` was deleted from the
  // catalogue with this move; if any future edit reinstates the old
  // `modeB ? ... : t('detail.devices.modeA')` shape, `t()` will echo the dead
  // key straight onto the screen.
  it('never reaches for the deleted mode-A sentence', () => {
    const note = peerDevicesNote(peer({ hal_reason: 'mode_a' }), daemon);
    expect(note).not.toContain('detail.devices.modeA');
    expect(note).toBe(t('halReason.modeA'));
  });

  // A peer with no `hal_device` has no rows no matter what else is set, so the
  // published/listed arms must not be reachable from here.
  it('answers with the reason even when the peer is online', () => {
    expect(peerDevicesNote(peer({ online: true, hal_reason: 'capacity' }), daemon))
      .toBe(t('halReason.capacity'));
  });
});

describe('peerDevicesNote: the three arms that have devices', () => {
  const bound = { state: 'bound', observed: true };

  it('says the devices are selectable when bound, observed and the peer is up', () => {
    expect(peerDevicesNote(withDevices(bound, { online: true }), daemon))
      .toBe(t('detail.devices.published'));
  });

  // Offline is not the same as absent: the devices stay in the system list and
  // stay selectable, they just carry no audio. Saying "published" there would
  // promise sound that will not arrive.
  it('warns instead when the peer is offline but the devices are still listed', () => {
    expect(peerDevicesNote(withDevices(bound, { online: false }), daemon))
      .toBe(t('detail.devices.offline'));
  });

  it('reports the driver state plus "listed" when observed but not bound', () => {
    const note = peerDevicesNote(withDevices({ state: 'pending', observed: true }), daemon);
    expect(note).toBe(t('detail.devices.stateListed', { state: t('device.state.pending') }));
    expect(note).not.toBe(t('detail.devices.stateUnlisted', { state: t('device.state.pending') }));
  });

  it('reports the driver state plus "not listed yet" when unobserved', () => {
    const note = peerDevicesNote(withDevices({ state: 'pending', observed: false }), daemon);
    expect(note).toBe(t('detail.devices.stateUnlisted', { state: t('device.state.pending') }));
  });

  // `bound` alone is not enough -- the daemon can hold a bound slot the system
  // has not surfaced yet. Treating that as published is the optimistic lie the
  // `observed` flag exists to prevent.
  it('does not call a bound-but-unobserved slot published', () => {
    const note = peerDevicesNote(withDevices({ state: 'bound', observed: false }, { online: true }), daemon);
    expect(note).not.toBe(t('detail.devices.published'));
  });

  // A daemon that reports no state at all must still produce a sentence rather
  // than a hole where the state word should be -- and specifically not the
  // literal "undefined", which is what an unguarded interpolation renders.
  it('falls back to a dash rather than an empty state word', () => {
    const note = peerDevicesNote(withDevices({ observed: false }), daemon);
    expect(note).toBe(t('detail.devices.stateUnlisted', { state: t('common.dash') }));
    expect(note).not.toContain('undefined');
  });

  // An unrecognised state string is passed through verbatim rather than
  // swallowed: a future daemon state the UI has no word for is still more
  // useful on screen than a dash.
  it('passes an unrecognised state string through', () => {
    const note = peerDevicesNote(withDevices({ state: 'quarantined', observed: true }), daemon);
    expect(note).toBe(t('detail.devices.stateListed', { state: 'quarantined' }));
  });
});
