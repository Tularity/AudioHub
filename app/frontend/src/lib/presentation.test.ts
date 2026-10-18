import { describe, expect, it } from 'vitest';
import { deviceKindText, presentationLabel, reasonBucket } from './presentation';
import { enUS } from '../i18n/en-US';
import { zhCN } from '../i18n/zh-CN';
import type { PeerState } from '../ipc/types';

describe('presentation indicator', () => {
  it('says nothing when the daemon reported nothing', () => {
    expect(presentationLabel(undefined)).toBeNull();
    expect(presentationLabel(null)).toBeNull();
  });

  it('describes a device in force as physical with its kind and transport', () => {
    const l = presentationLabel({
      class: 'physical', name: 'Jabra (Peer)', transport: 'usb', form_factor: 'headset',
      reason: 'physical', pending_republish: false,
    });
    expect(l).toMatchObject({ state: 'physical', reason: null });
    expect(l?.text).toContain('耳麦');
    expect(l?.text).toContain('USB');
  });

  it('drops an unknown form factor instead of printing the raw code', () => {
    expect(deviceKindText({ form_factor: 'unknown', transport: 'bluetooth' })).toBe('蓝牙');
    expect(deviceKindText({ form_factor: 'future_thing', transport: 'future_bus' })).toBe('');
    const l = presentationLabel({ class: 'physical', form_factor: 'weird', reason: 'physical' });
    expect(l?.text).not.toContain('weird');
  });

  it('gives every virtual device its reason, mapped by bucket', () => {
    const cases: Array<[string, string]> = [
      ['consumer_disabled', 'consumerOff'],
      ['provider_opt_out', 'peerOptOut'],
      ['peer_unsupported', 'peerOld'],
      ['driver_unsupported', 'driver'],
      ['driver_caller_check', 'driver'],
      ['no_facts', 'pending'],
      ['audiohub_own', 'peerVirtual'],
      ['transport_virtual', 'peerVirtual'],
      ['owner_unreadable', 'unverified'],
    ];
    for (const [code, bucket] of cases) {
      const l = presentationLabel({ class: 'virtual', reason: code });
      expect(l?.state, code).toBe('virtual');
      expect(l?.reason, code).toBe(bucket);
    }
  });

  it('renders an unknown reason code generically, never raw', () => {
    expect(reasonBucket('brand_new_code')).toBe('other');
    expect(reasonBucket(undefined)).toBe('other');
    const l = presentationLabel({ class: 'virtual', reason: 'brand_new_code' });
    expect(l?.text).not.toContain('brand_new_code');
    expect(l?.text).toBe('虚拟呈现 · 暂不满足物理呈现条件');
  });

  it('shows the two transitional states only when the daemon marks them pending', () => {
    expect(presentationLabel({ class: 'virtual', reason: 'physical', pending_republish: true }))
      .toMatchObject({ state: 'to-physical', reason: null });
    expect(presentationLabel({
      class: 'physical', reason: 'consumer_disabled', pending_republish: true,
    })).toMatchObject({ state: 'to-virtual', reason: 'consumerOff' });
    // A virtual device whose target is also virtual is plainly virtual even if
    // the pending flag is set for a different reason.
    expect(presentationLabel({ class: 'virtual', reason: 'peer_unsupported', pending_republish: true }))
      .toMatchObject({ state: 'virtual' });
  });

  it('parses the documented wire shape (out / in) off PeerState', () => {
    const peer: PeerState = JSON.parse(`{
      "fingerprint": "fp",
      "presentation": {
        "out": {"class": "physical", "name": "Speakers (Peer)", "transport": "built_in",
                "form_factor": "speakers", "reason": "physical", "pending_republish": false},
        "in": {"class": "virtual", "reason": "provider_opt_out", "pending_republish": false}
      }
    }`);
    expect(presentationLabel(peer.presentation?.out)?.state).toBe('physical');
    expect(presentationLabel(peer.presentation?.in)?.reason).toBe('peerOptOut');
  });

  it('keeps both catalogues complete for every code it can emit', () => {
    for (const k of Object.keys(zhCN).filter((x) => x.startsWith('presentation.'))) {
      expect(enUS[k as keyof typeof enUS], k).toBeTruthy();
    }
  });
});
