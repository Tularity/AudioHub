import { describe, it, expect } from 'vitest';
import { pendingSignature, shouldAutoOpenPermissions } from './permIntro';

const MAC_FIRST_RUN = [
  { id: 'microphone', status: 'undetermined' },
  { id: 'local_network', status: 'unknown' },
  { id: 'system_audio', status: 'undetermined' },
];

const ALL_GRANTED = [
  { id: 'microphone', status: 'granted' },
  { id: 'local_network', status: 'granted' },
  { id: 'system_audio', status: 'granted' },
];

/** The steady state on macOS: the two unqueryable permissions never go green. */
const NEVER_KNOWABLE = [
  { id: 'microphone', status: 'granted' },
  { id: 'local_network', status: 'unknown' },
  { id: 'system_audio', status: 'undetermined' },
];

const ONLINE = { online: true, probed: true, gateVisible: false, tauri: true };

describe('the pending-permission signature', () => {
  it('lists only the permissions that are not granted', () => {
    expect(pendingSignature(NEVER_KNOWABLE)).toBe('local_network|system_audio');
  });

  it('is empty when everything is granted', () => {
    expect(pendingSignature(ALL_GRANTED)).toBe('');
  });

  it('is empty for an empty list', () => {
    expect(pendingSignature([])).toBe('');
  });

  it('does not depend on the order the daemon replied in', () => {
    // The reply order is not contractual. An unsorted signature would differ
    // between two launches with identical permissions, so the panel would open
    // every single time.
    const a = pendingSignature([
      { id: 'system_audio', status: 'undetermined' },
      { id: 'local_network', status: 'unknown' },
    ]);
    const b = pendingSignature([
      { id: 'local_network', status: 'unknown' },
      { id: 'system_audio', status: 'undetermined' },
    ]);
    expect(a).toBe(b);
  });

  it('counts denied and restricted as pending', () => {
    expect(pendingSignature([
      { id: 'microphone', status: 'denied' },
      { id: 'local_network', status: 'restricted' },
    ])).toBe('local_network|microphone');
  });
});

describe('when the permission panel opens by itself', () => {
  it('opens on a first run with permissions outstanding', () => {
    expect(shouldAutoOpenPermissions({
      ...ONLINE, signature: pendingSignature(MAC_FIRST_RUN), seen: null,
    })).toBe(true);
  });

  it('never opens once every permission is granted, with no stored marker', () => {
    // This is the clause that makes the marker optional rather than load
    // bearing: a fully authorised machine is quiet because it has nothing
    // pending, not because something was written to disk.
    expect(shouldAutoOpenPermissions({
      ...ONLINE, signature: pendingSignature(ALL_GRANTED), seen: null,
    })).toBe(false);
  });

  it('does not open again after the user dismissed the same set', () => {
    // The single failure mode of this feature: on macOS `local_network` is
    // never queryable, so "something is pending" stays true forever and a
    // missing clause 4 would reopen the panel on every launch.
    const sig = pendingSignature(NEVER_KNOWABLE);
    expect(shouldAutoOpenPermissions({ ...ONLINE, signature: sig, seen: sig })).toBe(false);
  });

  it('opens again when the pending set grows', () => {
    // The user revoked the microphone in System Settings after dismissing the
    // panel. The stored signature no longer describes reality, so it lapses.
    const seen = pendingSignature(NEVER_KNOWABLE);
    const now = pendingSignature([
      { id: 'microphone', status: 'denied' },
      { id: 'local_network', status: 'unknown' },
      { id: 'system_audio', status: 'undetermined' },
    ]);
    expect(now).not.toBe(seen);
    expect(shouldAutoOpenPermissions({ ...ONLINE, signature: now, seen })).toBe(true);
  });

  it('opens again when the pending set shrinks to a different set', () => {
    const seen = pendingSignature(NEVER_KNOWABLE);
    const now = pendingSignature([
      { id: 'microphone', status: 'granted' },
      { id: 'local_network', status: 'unknown' },
      { id: 'system_audio', status: 'granted' },
    ]);
    expect(shouldAutoOpenPermissions({ ...ONLINE, signature: now, seen })).toBe(true);
  });

  it('waits until the probe has produced a result', () => {
    expect(shouldAutoOpenPermissions({
      ...ONLINE, probed: false, signature: 'microphone', seen: null,
    })).toBe(false);
  });

  it('waits until the daemon is connected', () => {
    expect(shouldAutoOpenPermissions({
      ...ONLINE, online: false, signature: 'microphone', seen: null,
    })).toBe(false);
  });

  it('stays out of the way while the onboarding gate is up', () => {
    // The sheet sits at z-index 60 and the gate at 40, so it would float on top
    // of a screen that is already asking for the same permissions.
    expect(shouldAutoOpenPermissions({
      ...ONLINE, gateVisible: true, signature: 'microphone', seen: null,
    })).toBe(false);
  });

  it('does not open in the browser build', () => {
    // The grant buttons drive the machine running the daemon, which is not the
    // machine whose browser this is.
    expect(shouldAutoOpenPermissions({
      ...ONLINE, tauri: false, signature: 'microphone', seen: null,
    })).toBe(false);
  });

  it('treats an unrelated stored signature as not seen', () => {
    expect(shouldAutoOpenPermissions({
      ...ONLINE, signature: 'local_network', seen: 'system_audio',
    })).toBe(true);
  });
});
