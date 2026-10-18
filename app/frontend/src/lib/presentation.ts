// Physical device presentation: what the per-peer device indicator says.
//
// The daemon (core/audiohubd/src/presentation.rs) decides whether each
// direction of a peer's virtual device is shown to the system as the peer's
// real, physical device or as an AudioHub virtual one, and exposes the verdict
// as `PeerState.presentation` (design §5.5). This file only turns that verdict
// into a short label. It never decides anything itself: the verdict, the
// reason code and the pending flag all come from the daemon, so the UI cannot
// disagree with what the operating system is actually showing.
//
// Reason codes are a growing protocol vocabulary (`[a-z0-9_]`), shared by the
// provider's classifier and the consumer's policy. An unknown code must render
// a generic line rather than vanish, hence the `other` bucket.

import { t } from '../i18n';
import type { MsgKey } from '../i18n';
import type { DirectionPresentation } from '../ipc/types';

export type PresentationState =
  | 'physical'         // in force: shown as the peer's real device
  | 'virtual'          // in force: shown as an AudioHub virtual device
  | 'to-physical'      // virtual now, switching to physical
  | 'to-virtual';      // physical now, being withdrawn

export interface PresentationLabel {
  state: PresentationState;
  /** One short line, ready to render. */
  text: string;
  /** The reason bucket when the device is virtual; null otherwise. */
  reason: ReasonBucket | null;
}

export type ReasonBucket =
  | 'consumerOff'   // switched off on this machine
  | 'peerOptOut'    // the peer does not share device info
  | 'peerOld'       // the peer's version predates the feature
  | 'driver'        // the local driver cannot present it
  | 'pending'       // waiting for the peer's description
  | 'peerVirtual'   // the peer's default device is itself virtual
  | 'unverified'    // the peer's device type could not be confirmed
  | 'other';

const REASON_BUCKET: Record<string, ReasonBucket> = {
  consumer_disabled: 'consumerOff',
  provider_opt_out: 'peerOptOut',
  peer_unsupported: 'peerOld',
  driver_unsupported: 'driver',
  driver_caller_check: 'driver',
  no_facts: 'pending',
  not_evaluated: 'pending',
  facts_pending: 'pending',
  peer_capabilities_unheard: 'pending',
  probe_pending: 'pending',
  probe_timeout: 'pending',
  probe_worker_failed: 'pending',
  probe_failed: 'pending',
  not_probed: 'pending',
  no_default_device: 'peerVirtual',
  audiohub_own: 'peerVirtual',
  unsupported_platform: 'peerVirtual',
  fake_audio_backend: 'peerVirtual',
  injected_refused: 'peerVirtual',
  virtual_ancestor: 'peerVirtual',
  adapter_enumerator_virtual: 'peerVirtual',
  owner_third_party: 'peerVirtual',
  transport_virtual: 'peerVirtual',
  transport_airplay: 'peerVirtual',
  transport_continuity: 'peerVirtual',
  capture_form_factor: 'peerVirtual',
  remote_network_form_factor: 'peerVirtual',
  aggregate_empty: 'peerVirtual',
  aggregate_has_tap: 'peerVirtual',
  aggregate_member_not_physical: 'peerVirtual',
  name_unreadable: 'unverified',
  identity_unreadable: 'unverified',
  invalid_facts: 'unverified',
  adapter_unresolved: 'unverified',
  adapter_enumerator_unreadable: 'unverified',
  adapter_enumerator_unrecognized: 'unverified',
  ancestor_unreadable: 'unverified',
  ancestor_enumerator_unrecognized: 'unverified',
  ancestor_chain_incomplete: 'unverified',
  owner_unreadable: 'unverified',
  transport_unreadable: 'unverified',
  transport_unrecognized: 'unverified',
  aggregate_incomplete: 'unverified',
  aggregate_too_deep: 'unverified',
};

const REASON_KEY: Record<ReasonBucket, MsgKey> = {
  consumerOff: 'presentation.why.consumerOff',
  peerOptOut: 'presentation.why.peerOptOut',
  peerOld: 'presentation.why.peerOld',
  driver: 'presentation.why.driver',
  pending: 'presentation.why.pending',
  peerVirtual: 'presentation.why.peerVirtual',
  unverified: 'presentation.why.unverified',
  other: 'presentation.why.other',
};

const FORM_KEY: Record<string, MsgKey> = {
  speakers: 'presentation.form.speakers',
  headphones: 'presentation.form.headphones',
  headset: 'presentation.form.headset',
  microphone: 'presentation.form.microphone',
  handset: 'presentation.form.handset',
  line_level: 'presentation.form.lineLevel',
  spdif: 'presentation.form.spdif',
  digital_display: 'presentation.form.display',
};

const TRANSPORT_KEY: Record<string, MsgKey> = {
  built_in: 'presentation.transport.builtIn',
  usb: 'presentation.transport.usb',
  bluetooth: 'presentation.transport.bluetooth',
  hdmi: 'presentation.transport.hdmi',
  display_port: 'presentation.transport.displayPort',
  thunderbolt: 'presentation.transport.thunderbolt',
  pci: 'presentation.transport.pci',
  fire_wire: 'presentation.transport.fireWire',
};

export function reasonBucket(reason: string | null | undefined): ReasonBucket {
  return (reason && REASON_BUCKET[reason]) || 'other';
}

/** "Headphones · USB"; either half is dropped when the daemon did not say. */
export function deviceKindText(p: Pick<DirectionPresentation, 'form_factor' | 'transport'>): string {
  const parts: string[] = [];
  const form = p.form_factor ? FORM_KEY[p.form_factor] : undefined;
  if (form) parts.push(t(form));
  const transport = p.transport ? TRANSPORT_KEY[p.transport] : undefined;
  if (transport) parts.push(t(transport));
  return parts.join(' · ');
}

/**
 * The indicator for one direction. `null` when the daemon reported nothing
 * (older daemon, or no virtual device yet): "unknown" renders as nothing at
 * all, never as "virtual".
 */
export function presentationLabel(
  p: DirectionPresentation | null | undefined,
): PresentationLabel | null {
  if (!p || typeof p.class !== 'string') return null;
  const physical = p.class === 'physical';
  if (physical) {
    if (p.pending_republish) {
      return { state: 'to-virtual', text: t('presentation.toVirtual'), reason: reasonBucket(p.reason) };
    }
    const kind = deviceKindText(p);
    return {
      state: 'physical',
      text: kind ? t('presentation.physicalKind', { kind }) : t('presentation.physical'),
      reason: null,
    };
  }
  if (p.pending_republish && p.reason === 'physical') {
    return { state: 'to-physical', text: t('presentation.toPhysical'), reason: null };
  }
  const bucket = reasonBucket(p.reason);
  return {
    state: 'virtual',
    text: t('presentation.virtualWhy', { reason: t(REASON_KEY[bucket]) }),
    reason: bucket,
  };
}
