// Which volume the macOS menu-bar slider is allowed to show, if any.
//
// # Why this is a function and not an inline `find`
//
// The slider lives in AppKit, on the other side of a Tauri command, and the
// only thing the Rust side is told is "here is a scalar" or "there is nothing
// to show". Every rule about *when* a slider may appear therefore has to be
// decided here — and it has to agree, value for value, with the slider already
// on the peer card (`components/VolumeControl.tsx`). Two places deciding
// "is this adjustable?" separately is how the menu bar ends up offering a
// control that the card greys out.
//
// # The three rules
//
//   1. Mode A only. Mode B routes through the virtual device, where the card
//      itself passes `sess={null}` (`views/Peers.tsx:498`); share mode is the
//      serving side and owns no outbound spk session of its own.
//   2. The session must be the outbound speaker one: `kind === 'spk'` and
//      `dir === 'send'`. That is literally what the menu-bar user means by
//      "volume" in mode A — the far end's speaker, the only thing making sound.
//   3. `stats.volume` must have arrived AND be actionable. Actionable is
//      `adjustable !== false` OR `volume_software_gain` — see plan §7.2: when
//      the peer's real device has no writable volume, the daemon takes the
//      volume over as a send-side software gain. `adjustable` stays false in
//      that case (it is a fact about the *peer's device*, and it has not
//      changed), but the slider is real. Testing `adjustable` alone would grey
//      out a control that works.
//
// Mode A allows exactly one peer at a time (plan §7.1), so at most one session
// can satisfy these. `find` rather than a uniqueness assertion: if the daemon
// ever reports two, showing the first is strictly better than showing none.

import type { SessionInfo } from '../ipc/types';

export interface TrayVolume {
  /** Session id, for `session.set_volume`. */
  id: number;
  /** 0..1, as last reported by the daemon. */
  scalar: number;
  muted: boolean;
}

export function trayVolumeOf(
  mode: string,
  sessions: readonly SessionInfo[] | null | undefined,
): TrayVolume | null {
  if (mode !== 'a' || !sessions) return null;
  for (const s of sessions) {
    if (s.kind !== 'spk' || s.dir !== 'send') continue;
    if (s.id == null) continue;
    const v = s.stats?.volume;
    if (!v) continue;
    const actionable = v.adjustable !== false || !!s.stats?.volume_software_gain;
    if (!actionable) continue;
    const scalar = Number(v.scalar);
    if (!Number.isFinite(scalar)) continue;
    return { id: s.id, scalar: Math.max(0, Math.min(1, scalar)), muted: !!v.muted };
  }
  return null;
}
