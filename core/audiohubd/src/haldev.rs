//! haldev — one pair of virtual devices per paired peer (spec-m5b §5).
//!
//! # What this module is
//!
//! The driver owns sixteen slots, each a permanent pair of rings. It publishes
//! NOTHING until a `Bind` names a slot; "adding a device" is binding metadata
//! to a slot, and "removing" one is retiring that binding. This module decides
//! which peer owns which slot, what the devices are called, when a session has
//! to exist behind them, and it is the only thing in the daemon that sends a
//! `Bind`.
//!
//! # Two invariants everything here is shaped around
//!
//! **A daemon restart must not churn devices.** macOS remembers the user's
//! default output by device UID. If a restart re-assigned slots, or cleared a
//! slot before re-setting it, the chosen default output would be destroyed and
//! silently replaced by the built-in speakers — once per restart, with every
//! functional test still green. So the slot table is PERSISTED
//! (`<config>/hal_slots.json`) and the reconcile emits an idempotent `Set`,
//! never `Clear`-then-`Set`. The other half of that bargain: the driver replays
//! a slot's IO state and volume only when an idempotent `Set` lands on it, so
//! after a restart we must re-`Set` every slot we still intend — otherwise an
//! application that was mid-recording keeps recording, and records silence.
//!
//! **Publication is closed-loop.** "I sent a Bind and mach returned OK" is not
//! evidence that a device exists. Every pass enumerates what the system
//! actually publishes (`kAudioHardwarePropertyDevices`, filtered by our UID
//! prefix) and diffs against the intended set. A dropped notification, an
//! `Initialize` race, a coreaudiod restart and a slot desync all become the
//! same self-healing diff.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use audiohub_core::volume;
use audiohub_ipc::{
    HalDeviceInfo, Mode, OpenSessionParams, PeerHalDevice, VolumeState, KIND_MIC, KIND_SPK,
    SOURCE_HAL_SPEAKER,
};
use audiohub_net::identity::{PairedPeer, PeerStore};
use audiohub_net::secure::SessionMsg;

use crate::halbridge::{
    self, HalBindRequest, HalControlEvent, HalEndpoint, HalSlotState, HAL_PUBLISH_BOTH,
    HAL_PUBLISH_IN, HAL_PUBLISH_OUT,
};
use crate::{conn, dlog, lk, DaemonInner, DaemonState, SessionOrigin};

pub const HAL_MAX_SLOTS: usize = halbridge::HAL_MAX_SLOTS;

/// Every virtual device's UID starts with this. It is what the closed-loop
/// enumeration matches on, what regression scripts discover devices by, and —
/// because it embeds the FINGERPRINT rather than the host name — what survives
/// the peer renaming its computer.
pub const UID_PREFIX: &str = "AudioHub:";

/// U+2013 EN DASH, as spec-m5b §3.5 spells it. Not a hyphen.
const NAME_PREFIX: &str = "AudioHub – ";
const NAME_OFFLINE_ZH_CN: &str = "（离线）";
const NAME_OFFLINE_EN_US: &str = " (Offline)";

/// `char[128]` on the wire, and the driver rejects a name that does not fit.
const MAX_NAME_BYTES: usize = 127;

pub fn uid_out(fingerprint: &str) -> String {
    format!("{UID_PREFIX}{fingerprint}:out")
}

pub fn uid_in(fingerprint: &str) -> String {
    format!("{UID_PREFIX}{fingerprint}:in")
}

/// The part BOTH of a peer's device names share: "AudioHub – WIN-30", i.e. the
/// disambiguated display name with the frozen plan §7.1 prefix and no
/// direction suffix.
///
/// (Not to be confused with [`base_name`] below, which is the peer's own label
/// — alias or host name — before any of this is put around it.)
///
/// The single spelling of the complete visible name, and both platforms go
/// through it: output and input deliberately share this label. The operating
/// system's device class and icon already distinguish their direction.
///
/// It is a function rather than a `pub const` so the two callers cannot end up
/// composing it two slightly different ways — which is exactly what happened:
/// the Windows path sent the bare peer name and every endpoint came out
/// labelled `WIN-IR01HVEFU7G`, with no AudioHub anywhere in it.
pub fn device_name_stem(display: &str) -> String {
    format!("{NAME_PREFIX}{display}")
}

/// The two device names for one peer.  They intentionally have the same visible
/// text: input/output is already a structural property in both operating
/// systems and the UI uses distinct icons.  Identity and routing remain the
/// direction-specific UIDs below, never the display string.
fn offline_mark(locale: &str) -> &'static str {
    if locale == "en-US" {
        NAME_OFFLINE_EN_US
    } else {
        NAME_OFFLINE_ZH_CN
    }
}

/// The peer label carried across the Windows bind wire. It is deliberately
/// prefix-free because the encoder adds `AudioHub – `, but it already contains
/// the localized offline suffix so macOS and Windows receive the same name.
pub fn device_display_name(display: &str, offline: bool, locale: &str) -> String {
    let mark = if offline { offline_mark(locale) } else { "" };
    let available = MAX_NAME_BYTES
        .saturating_sub(NAME_PREFIX.len())
        .saturating_sub(mark.len());
    format!("{}{mark}", clamp_utf8(display, available))
}

pub fn device_names(display: &str, offline: bool, locale: &str) -> (String, String) {
    let shown = device_display_name(display, offline, locale);
    let name = device_name_stem(&shown);
    (name.clone(), name)
}

/// Truncates on a CHARACTER boundary. A name cut mid-codepoint is invalid UTF-8
/// and the driver refuses the whole `Bind` over it, which would take out the
/// device rather than shorten its name.
fn clamp_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------- naming

/// One peer's contribution to the naming pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameInput {
    pub fingerprint: String,
    /// Alias if the user set one, otherwise the peer's own computer name.
    pub base: String,
    pub added_unix: u64,
}

/// Resolves display names for a whole set of peers at once, appending ` (2)`,
/// ` (3)` … to duplicates.
///
/// The order is `(added_unix, fingerprint)` and the FIRST peer keeps its name
/// untouched. Renaming the incumbent would be the worse behaviour by far: the
/// device an application already selected would change its label because
/// somebody else paired a second laptop with the same name, and the person
/// looking at it has no way to connect those two events. Hex suffixes are
/// deliberately not used either — ` (2)` is what every OS does here, and a
/// fingerprint fragment in a device name is unreadable.
pub fn display_names(peers: &[NameInput]) -> HashMap<String, String> {
    let mut by_base: HashMap<&str, Vec<&NameInput>> = HashMap::new();
    for p in peers {
        by_base.entry(p.base.as_str()).or_default().push(p);
    }
    let mut out = HashMap::new();
    for (base, mut group) in by_base {
        group.sort_by(|a, b| {
            a.added_unix
                .cmp(&b.added_unix)
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
        for (i, p) in group.iter().enumerate() {
            let name = if i == 0 {
                base.to_string()
            } else {
                format!("{base} ({})", i + 1)
            };
            out.insert(p.fingerprint.clone(), name);
        }
    }
    out
}

/// The name a peer's devices carry before disambiguation: the user's alias if
/// there is one, otherwise whatever the peer called itself at the last
/// connection.
pub fn base_name(peer: &PairedPeer) -> String {
    let alias = peer.alias.as_deref().map(str::trim).unwrap_or("");
    if !alias.is_empty() {
        return alias.to_string();
    }
    let name = peer.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    peer.fingerprint.clone()
}

// ---------------------------------------------------------------- slot table

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SlotEntry {
    slot: u8,
    fingerprint: String,
    /// Last direction mask advertised by this peer. `None` is deliberately
    /// distinct from `Some(0)`: the former has not answered yet, while the
    /// latter explicitly has neither endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directions: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlotFile {
    version: u32,
    slots: Vec<SlotEntry>,
}

/// Which peer owns which slot, persisted across restarts.
///
/// Not persisting this is the single most expensive mistake available in this
/// design: slots would be re-assigned on every start, the `Bind`s would stop
/// being idempotent, every device would be unpublished and republished, and the
/// user's chosen default output would be quietly thrown away each time.
#[derive(Debug, Clone, Default)]
pub struct SlotTable {
    /// slot -> fingerprint.
    assign: Vec<Option<String>>,
    /// Last known publication mask for the same slot. Unknown must survive a
    /// restart as `None`; collapsing it into zero would permanently turn a
    /// peer that disconnected before its first capability advert into an
    /// explicit "has neither endpoint" peer.
    directions: Vec<Option<u8>>,
}

impl SlotTable {
    pub fn new() -> SlotTable {
        SlotTable {
            assign: vec![None; HAL_MAX_SLOTS],
            directions: vec![None; HAL_MAX_SLOTS],
        }
    }

    fn path(dir: &Path) -> PathBuf {
        dir.join("hal_slots.json")
    }

    /// A missing or unreadable file is an EMPTY table, not an error: the worst
    /// case is one round of re-assignment, and refusing to start would be worse.
    pub fn load(dir: &Path) -> SlotTable {
        let mut t = SlotTable::new();
        let Ok(bytes) = std::fs::read(Self::path(dir)) else {
            return t;
        };
        let Ok(file) = serde_json::from_slice::<SlotFile>(&bytes) else {
            dlog!("[audiohubd] hal_slots.json is unreadable; slots will be re-assigned");
            return t;
        };
        let file_version = file.version;
        for e in file.slots {
            let s = e.slot as usize;
            if s < HAL_MAX_SLOTS && !e.fingerprint.is_empty() && t.assign[s].is_none() {
                t.assign[s] = Some(e.fingerprint);
                t.directions[s] = match e.directions {
                    Some(mask) => Some(mask & HAL_PUBLISH_BOTH),
                    // v1 never stored capabilities. Keep its incumbent pair
                    // visible in both directions until it reconnects rather
                    // than making an upgrade remove a selected system device.
                    None if file_version <= 1 => Some(HAL_PUBLISH_BOTH),
                    None => None,
                };
            }
        }
        t
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let file = SlotFile {
            version: 3,
            slots: self
                .assign
                .iter()
                .enumerate()
                .filter_map(|(s, fp)| {
                    fp.as_ref().map(|fp| SlotEntry {
                        slot: s as u8,
                        fingerprint: fp.clone(),
                        directions: self.directions[s].map(|mask| mask & HAL_PUBLISH_BOTH),
                    })
                })
                .collect(),
        };
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = Self::path(dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&file)?.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    pub fn slot_of(&self, fingerprint: &str) -> Option<u8> {
        self.assign
            .iter()
            .position(|f| f.as_deref() == Some(fingerprint))
            .map(|s| s as u8)
    }

    /// The peer's existing slot, or the lowest free one within `capacity`.
    /// `None` = the pool is full, which is a visible `hal_reason: "capacity"`
    /// on that one peer and changes nothing for the sixteen already bound.
    pub fn assign(&mut self, fingerprint: &str, capacity: usize) -> Option<u8> {
        if let Some(s) = self.slot_of(fingerprint) {
            return (usize::from(s) < capacity).then_some(s);
        }
        let free = self.assign[..capacity.min(HAL_MAX_SLOTS)]
            .iter()
            .position(Option::is_none)?;
        self.assign[free] = Some(fingerprint.to_string());
        self.directions[free] = None;
        Some(free as u8)
    }

    pub fn directions_of(&self, fingerprint: &str) -> Option<u8> {
        self.slot_of(fingerprint)
            .and_then(|s| self.directions[s as usize].map(|mask| mask & HAL_PUBLISH_BOTH))
    }

    pub fn set_directions(&mut self, fingerprint: &str, directions: u8) -> bool {
        let Some(slot) = self.slot_of(fingerprint) else {
            return false;
        };
        let directions = directions & HAL_PUBLISH_BOTH;
        let cell = &mut self.directions[slot as usize];
        let changed = *cell != Some(directions);
        *cell = Some(directions);
        changed
    }

    pub fn release(&mut self, fingerprint: &str) -> Option<u8> {
        let s = self.slot_of(fingerprint)?;
        self.assign[s as usize] = None;
        self.directions[s as usize] = None;
        Some(s)
    }

    /// Drops assignments for peers that are no longer paired at all. Called on
    /// every pass, so a peer unpaired by another process (the CLI writes
    /// `paired_peers.json` directly) still frees its slot.
    pub fn retain(&mut self, paired: &HashSet<String>) -> bool {
        let mut changed = false;
        for (index, slot) in self.assign.iter_mut().enumerate() {
            if let Some(fp) = slot.as_ref() {
                if !paired.contains(fp.as_str()) {
                    *slot = None;
                    self.directions[index] = None;
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn used(&self) -> usize {
        self.assign.iter().filter(|s| s.is_some()).count()
    }

    /// Cheap equality for "did this pass change the assignment?". The answer
    /// decides whether the table is written back, and a table that is not
    /// written back is one that comes back different after a restart — which is
    /// the single failure this whole persistence exists to prevent.
    fn same_as(&self, other: &SlotTable) -> bool {
        self.assign == other.assign && self.directions == other.directions
    }
}

// ---------------------------------------------------------------- planning

/// One device pair the daemon intends to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredDevice {
    pub slot: u8,
    pub fingerprint: String,
    pub out_uid: String,
    pub in_uid: String,
    pub out_name: String,
    pub in_name: String,
    /// The disambiguated peer label WITHOUT the shared `AudioHub – ` prefix or
    /// a direction suffix. It may already include the localized offline marker.
    /// macOS ignores it; the Windows wire encoder adds the shared prefix before
    /// handing the complete endpoint label to the driver.
    ///
    /// Carried as a plain extra field rather than behind a platform
    /// conditional: this file has none at all, and keeping it that way is worth
    /// more than saving one `String` per slot. `halwire_win.rs` asserts the
    /// count stays zero.
    pub display: String,
    /// Virtual directions this peer can actually serve.
    pub directions: u8,
    pub online: bool,
}

impl DesiredDevice {
    fn to_bind(&self) -> HalBindRequest {
        HalBindRequest {
            slot: self.slot,
            peer_key: self.fingerprint.clone(),
            out_uid: self.out_uid.clone(),
            in_uid: self.in_uid.clone(),
            out_name: self.out_name.clone(),
            in_name: self.in_name.clone(),
            display: self.display.clone(),
            directions: self.directions,
            online: self.online,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindAction {
    Set(HalBindRequest),
    Clear { slot: u8, generation: u32 },
}

/// One unacknowledged user intent for a peer's real default endpoint.
///
/// The request is connection-independent: moving a virtual control while the
/// peer is offline must survive until the next authenticated control channel.
/// `sent_connection` only suppresses duplicate writes on one live channel; a
/// replacement connection gets a different token and therefore retransmits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PendingDeviceVolume {
    pub(crate) request_id: u64,
    pub(crate) scalar: f32,
    pub(crate) muted: Option<bool>,
    sent_connection: Option<u64>,
    sent_at: Option<Instant>,
}

/// Per-slot, per-direction device-volume state for mode B.
///
/// This deliberately keeps three facts separate. `pending` is local user
/// intent and `remote` is the peer's last real readback.  Driver delivery keeps
/// two more facts separate: `last_delivered` suppresses a repeat of the peer
/// request, while `driver_echo` is the value the platform actually read back
/// and will report as the one-shot control event.  Windows may quantize a
/// scalar on its private audio-taper curve, so those two values are not safely
/// interchangeable.
#[derive(Debug, Clone, Copy)]
struct DeviceVolumeNotifyRetry {
    value: (f32, bool),
    not_before: Instant,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DeviceVolumeRelay {
    pub(crate) remote: Option<VolumeState>,
    pub(crate) pending: Option<PendingDeviceVolume>,
    /// Local send-gain authority when the peer's real output is fixed. This is
    /// deliberately separate from `remote`: the latter remains the physical
    /// fact (`adjustable=false`), while this state is the working virtual knob.
    software_gain: Option<VolumeState>,
    remote_connection: Option<u64>,
    remote_revision: u64,
    last_delivered: Option<(f32, bool)>,
    driver_echo: Option<(f32, bool)>,
    /// Last scalar/mute read from the local virtual endpoint. A Windows master
    /// update may raise separate channel/node events; after the first event
    /// reads the final COM state, later duplicates must not become fresh peer
    /// writes merely because the one-shot echo token was already consumed.
    local_observed: Option<(f32, bool)>,
    notify_retry: Option<DeviceVolumeNotifyRetry>,
    next_request_id: u64,
}

impl DeviceVolumeRelay {
    fn clear_protocol_state(&mut self) {
        self.remote = None;
        self.pending = None;
        self.software_gain = None;
        self.remote_connection = None;
        self.remote_revision = 0;
        self.invalidate_notification();
    }

    /// Forget only daemon -> HAL delivery state. Peer readback, local pending
    /// intent and fixed-device software gain all survive a driver reconnect.
    fn invalidate_notification(&mut self) {
        self.last_delivered = None;
        self.driver_echo = None;
        self.local_observed = None;
        self.notify_retry = None;
    }

    /// Consume exactly one daemon -> driver reflection.
    ///
    /// A mismatching event clears the guard and is a genuine user action. This
    /// is important when the driver suppresses an equal notification: leaving
    /// the guard armed forever would swallow a later user move back to the same
    /// scalar.
    fn consume_driver_echo(&mut self, now: (f32, bool)) -> bool {
        self.driver_echo
            .take()
            .is_some_and(|sent| vol_same(sent, now))
    }

    /// Record/coalesce local intent. Returns false for invalid input or the
    /// one-shot echo of a state this daemon just notified into the endpoint.
    fn queue_local(&mut self, scalar: f32, muted: Option<bool>) -> bool {
        if !scalar.is_finite() {
            return false;
        }
        let scalar = scalar.clamp(0.0, 1.0);
        if let Some(muted) = muted {
            let value = (scalar, muted);
            if self.consume_driver_echo(value) {
                self.local_observed = Some(value);
                return false;
            }
            if self
                .local_observed
                .is_some_and(|observed| vol_same(observed, value))
            {
                return false;
            }
            self.local_observed = Some(value);
        }
        self.queue_intent(scalar, muted)
    }

    /// Queue an explicit app/IPC action. It cannot be a HAL reflection, so it
    /// must not consume `driver_echo` merely because the requested value is
    /// equal to the latest peer state.
    fn queue_intent(&mut self, scalar: f32, muted: Option<bool>) -> bool {
        if !scalar.is_finite() {
            return false;
        }
        let scalar = scalar.clamp(0.0, 1.0);
        // A scalar-only tail must not erase an explicit mute write which is
        // still awaiting acknowledgement. `None` means "leave mute alone",
        // not "forget the value this same coalesced request already promised".
        let muted = muted.or_else(|| self.pending.and_then(|pending| pending.muted));
        if self
            .pending
            .is_some_and(|p| (p.scalar - scalar).abs() < HAL_VOL_EPS && p.muted == muted)
        {
            return true;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.pending = Some(PendingDeviceVolume {
            request_id: self.next_request_id,
            scalar,
            muted,
            sent_connection: None,
            sent_at: None,
        });
        true
    }

    fn uses_software_gain(&self) -> bool {
        self.software_gain.is_some() && self.remote.is_some_and(|remote| !remote.adjustable)
    }

    fn displayed_state(&self) -> Option<VolumeState> {
        if self.uses_software_gain() {
            self.software_gain
        } else {
            self.remote
        }
    }

    /// Update the local fallback knob. `from_driver` arms the current-value
    /// guard because no daemon -> HAL reflection is needed for a value already
    /// read from that HAL control.
    fn queue_software_gain(&mut self, scalar: f32, muted: Option<bool>, from_driver: bool) -> bool {
        if !scalar.is_finite() || !self.uses_software_gain() {
            return false;
        }
        let scalar = scalar.clamp(0.0, 1.0);
        let current = self.software_gain.unwrap_or(VolumeState {
            scalar: 1.0,
            muted: false,
            adjustable: false,
            mute_adjustable: false,
        });
        let requested_mute = muted;
        let muted = requested_mute.unwrap_or(current.muted);
        if from_driver {
            let value = (scalar, muted);
            if self.consume_driver_echo(value) {
                self.local_observed = Some(value);
                return false;
            }
            if self
                .local_observed
                .is_some_and(|observed| vol_same(observed, value))
            {
                return false;
            }
            self.local_observed = Some(value);
        }
        self.software_gain = Some(VolumeState {
            scalar,
            muted,
            adjustable: false,
            mute_adjustable: current.mute_adjustable,
        });
        let carries_remote_mute = current.mute_adjustable
            && (requested_mute.is_some()
                || self.pending.is_some_and(|pending| pending.muted.is_some()));
        if carries_remote_mute {
            // The scalar remains local gain, but this endpoint advertised an
            // independent writable mute. Carry the explicit mute to the peer;
            // its readback decides whether the write actually took.
            self.queue_intent(scalar, requested_mute);
        } else {
            self.pending = None;
        }
        if from_driver {
            self.last_delivered = Some((scalar, muted));
            self.driver_echo = None;
            self.local_observed = Some((scalar, muted));
            self.notify_retry = None;
        }
        true
    }

    /// The pending request this concrete control connection must carry now.
    ///
    /// A request is sent immediately on a replacement connection and retried
    /// on the same connection until its matching readback acknowledges it.
    fn due_for(&self, connection: u64, now: Instant) -> Option<PendingDeviceVolume> {
        self.pending.filter(|pending| {
            pending.sent_connection != Some(connection)
                || pending.sent_at.is_none_or(|sent_at| {
                    now.saturating_duration_since(sent_at) >= DEVICE_VOLUME_RETRY
                })
        })
    }

    fn mark_sent(&mut self, request_id: u64, connection: u64, now: Instant) {
        if let Some(pending) = self.pending.as_mut().filter(|p| p.request_id == request_id) {
            pending.sent_connection = Some(connection);
            pending.sent_at = Some(now);
        }
    }

    /// Store peer readback and report whether it is now authoritative for the
    /// virtual endpoint. An initial/periodic snapshot cannot overwrite a local
    /// pending request; only the matching request acknowledgement can.
    fn accept_remote(
        &mut self,
        connection: u64,
        revision: u64,
        request_id: Option<u64>,
        state: VolumeState,
        software_gain_capable: bool,
    ) -> bool {
        if self.remote_connection == Some(connection) && revision < self.remote_revision {
            // Network sends happen after the provider releases its endpoint
            // revision lock. A later periodic snapshot can therefore reach us
            // before the earlier write acknowledgement. The newer snapshot
            // remains authoritative, but a matching ACK must still retire the
            // request; otherwise a snapped/refused device value retries
            // forever simply because the two valid messages were reordered.
            let Some(pending) = self
                .pending
                .filter(|pending| request_id == Some(pending.request_id))
            else {
                return false;
            };
            if software_gain_capable {
                if let Some(remote) = self.remote.filter(|remote| !remote.adjustable) {
                    let mut local = self.software_gain.unwrap_or(VolumeState {
                        scalar: pending.scalar,
                        muted: remote.muted,
                        adjustable: false,
                        mute_adjustable: remote.mute_adjustable,
                    });
                    local.scalar = pending.scalar;
                    if remote.mute_adjustable {
                        local.muted = remote.muted;
                    }
                    local.mute_adjustable = remote.mute_adjustable;
                    self.software_gain = Some(local);
                }
            }
            self.pending = None;
            return true;
        }
        self.remote_connection = Some(connection);
        self.remote_revision = revision;
        self.remote = Some(state);
        if software_gain_capable && !state.adjustable {
            let mut local = self.software_gain.unwrap_or(VolumeState {
                scalar: 1.0,
                muted: state.muted,
                adjustable: false,
                mute_adjustable: state.mute_adjustable,
            });
            local.mute_adjustable = state.mute_adjustable;
            let mut clear_pending = true;
            // A write which discovered the fixed endpoint becomes local gain
            // intent instead of being discarded with the failed platform set.
            if let Some(pending) = self.pending {
                local.scalar = pending.scalar;
                if let Some(wanted_mute) = pending.muted {
                    if state.mute_adjustable {
                        let completed = request_id == Some(pending.request_id)
                            || (request_id.is_none() && state.muted == wanted_mute);
                        if completed {
                            // Matching ACK wins even when the device refused
                            // and read back a different state.
                            local.muted = state.muted;
                        } else {
                            // An older periodic snapshot cannot cancel a mute
                            // write which still needs the peer.
                            local.muted = wanted_mute;
                            clear_pending = false;
                        }
                    } else {
                        local.muted = wanted_mute;
                    }
                } else if state.mute_adjustable {
                    local.muted = state.muted;
                }
            } else if state.mute_adjustable {
                local.muted = state.muted;
            }
            self.software_gain = Some(local);
            if clear_pending {
                self.pending = None;
            }
            return true;
        }
        if state.adjustable {
            self.software_gain = None;
        }
        let Some(pending) = self.pending else {
            return true;
        };
        let observed_intent = (pending.scalar - state.scalar).abs() < HAL_VOL_EPS
            && pending.muted.is_none_or(|muted| muted == state.muted);
        if request_id != Some(pending.request_id) && !(request_id.is_none() && observed_intent) {
            return false;
        }
        self.pending = None;
        true
    }

    /// Return a new authoritative peer state for HAL notification, suppressing
    /// repeats, every state shadowed by local pending intent, and a failed
    /// attempt until its bounded retry deadline. Planning is read-only: only a
    /// platform-confirmed delivery may advance `last_delivered`.
    fn plan_notify(&self, at: Instant) -> Option<(f32, bool)> {
        if self.pending.is_some() {
            return None;
        }
        let remote = self.displayed_state()?;
        if !remote.scalar.is_finite() {
            return None;
        }
        let value = (remote.scalar.clamp(0.0, 1.0), remote.muted);
        if self
            .last_delivered
            .is_some_and(|notified| vol_same(notified, value))
        {
            return None;
        }
        if self
            .notify_retry
            .is_some_and(|retry| vol_same(retry.value, value) && at < retry.not_before)
        {
            return None;
        }
        Some(value)
    }

    /// Complete the two-phase daemon -> HAL delivery. A failed send retains
    /// no false acknowledgement and becomes due again after one second. The
    /// attempted value is recorded rather than the current peer value because
    /// network readback may legitimately change while the platform call runs.
    fn record_notify_result(
        &mut self,
        requested: (f32, bool),
        driver_echo: Option<(f32, bool)>,
        delivered: bool,
        now: Instant,
    ) {
        if delivered {
            self.last_delivered = Some(requested);
            let echo = driver_echo.unwrap_or(requested);
            self.driver_echo = Some(echo);
            self.local_observed = Some(echo);
            self.notify_retry = None;
        } else {
            self.notify_retry = Some(DeviceVolumeNotifyRetry {
                value: requested,
                not_before: now + DEVICE_VOLUME_RETRY,
            });
        }
    }
}

/// Peer-owned real endpoint named on the device-volume wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DeviceVolumeEndpoint {
    DefaultOutput,
    DefaultInput,
}

impl DeviceVolumeEndpoint {
    pub(crate) const fn as_wire(self) -> &'static str {
        match self {
            Self::DefaultOutput => "default_output",
            Self::DefaultInput => "default_input",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "default_output" => Some(Self::DefaultOutput),
            "default_input" => Some(Self::DefaultInput),
            _ => None,
        }
    }

    fn relay(self, rec: &SlotRec) -> &DeviceVolumeRelay {
        match self {
            Self::DefaultOutput => &rec.out_volume,
            Self::DefaultInput => &rec.in_volume,
        }
    }

    fn relay_mut(self, rec: &mut SlotRec) -> &mut DeviceVolumeRelay {
        match self {
            Self::DefaultOutput => &mut rec.out_volume,
            Self::DefaultInput => &mut rec.in_volume,
        }
    }

    fn hal_endpoint(self, slot: u8) -> HalEndpoint {
        match self {
            Self::DefaultOutput => HalEndpoint::out(slot),
            Self::DefaultInput => HalEndpoint::mic(slot),
        }
    }

    const fn required_direction(self) -> u8 {
        match self {
            Self::DefaultOutput => HAL_PUBLISH_OUT,
            Self::DefaultInput => HAL_PUBLISH_IN,
        }
    }
}

/// The connection-scoped contract which may carry one slot's endpoint
/// control.  This is deliberately resolved from `DaemonState::conns` while the
/// caller holds the slot's HAL lock: a cached Arc/version from before a
/// reconnect can otherwise resurrect v1 pending state after a replacement
/// connection has explicitly downgraded and cleared it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerDeviceVolumeProtocol {
    Offline,
    PeerNotSharing,
    Legacy,
    Modern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentPeerDeviceVolume {
    connection_id: Option<u64>,
    version: u8,
    protocol: PeerDeviceVolumeProtocol,
}

fn classify_peer_device_volume_protocol(
    live: bool,
    peer_mode: Option<Mode>,
    version: u8,
) -> PeerDeviceVolumeProtocol {
    if !live {
        PeerDeviceVolumeProtocol::Offline
    } else if peer_mode != Some(Mode::Share) {
        PeerDeviceVolumeProtocol::PeerNotSharing
    } else if version < 1 {
        PeerDeviceVolumeProtocol::Legacy
    } else {
        PeerDeviceVolumeProtocol::Modern
    }
}

/// Resolve only the connection currently registered for `fingerprint`.
///
/// Callers intentionally invoke this while holding `inner.haldev`: a cap1 ->
/// cap0 capability handler publishes its new connection cell before waiting
/// for the same HAL lock to clear relay state. Consequently either this read
/// observes the downgrade, or the later clear removes everything queued under
/// the old contract; there is no interleaving which clears first and then lets
/// a stale cap1 hint write pending state back.
fn with_current_peer_device_volume_protocol<T>(
    state: &DaemonState,
    fingerprint: &str,
    apply: impl FnOnce(CurrentPeerDeviceVolume) -> T,
) -> T {
    let Some(conn) = state
        .conns
        .get(fingerprint)
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
    else {
        return apply(CurrentPeerDeviceVolume {
            connection_id: None,
            version: 0,
            protocol: PeerDeviceVolumeProtocol::Offline,
        });
    };
    // Keep both connection-scoped cells locked while `apply` mutates the HAL
    // relay. A downgrade therefore either happens before this snapshot, or
    // waits and clears the completed old-contract mutation afterwards.
    let peer_mode = lk(&conn.peer_mode);
    let capabilities = lk(&conn.peer_audio_capabilities);
    let version = capabilities.device_volume_version();
    apply(CurrentPeerDeviceVolume {
        connection_id: Some(conn.connection_id),
        version,
        protocol: classify_peer_device_volume_protocol(true, peer_mode.mode(), version),
    })
}

/// Everything the daemon knows about one slot.
#[derive(Debug, Clone, Default)]
pub struct SlotRec {
    /// Empty = this slot is not assigned to anyone.
    pub fingerprint: String,
    pub out_uid: String,
    pub in_uid: String,
    /// The strings the last `Bind Set` actually carried. A difference from the
    /// desired names is a RENAME, which is an in-place update at the same UID —
    /// no new AudioObjectID, no device list change, so an application's
    /// remembered selection is untouched.
    pub sent_out_name: String,
    pub sent_in_name: String,
    /// Direction mask carried by the last successful send attempt.
    pub sent_directions: u8,
    pub sent_online: bool,
    /// A `Set` with those strings has been put on the wire.
    pub sent: bool,
    /// A `Clear` is in flight and the slot has not reported Free yet.
    pub clearing: bool,
    /// The slot's stamp, from `BindState`. Every other control message about
    /// this slot is filtered against it.
    pub generation: u32,
    /// What the driver last reported. `None` = it has never said anything about
    /// this slot, which after a restart is the normal starting point.
    pub state: Option<HalSlotState>,
    /// A `BindState` has arrived for the binding we last sent.
    pub acked: bool,
    /// What the driver last acknowledged as actually published.
    pub published_directions: u8,
    /// Directions the operating system's own device list currently contains.
    pub observed_directions: u8,
    /// Every direction requested in `sent_directions` is both acknowledged and
    /// visible. Kept as the legacy pair-level IPC summary; new callers use the
    /// masks above.
    pub observed: bool,
    pub peer_connected: bool,
    pub io_out: bool,
    pub io_in: bool,
    pub io_out_off_since: Option<Instant>,
    pub io_in_off_since: Option<Instant>,
    pub sess_out: Option<u32>,
    pub sess_in: Option<u32>,
    /// Per-slot echo suppression for the volume relay. Replaces a single global
    /// cell, which could only ever be right for one peer at a time.
    pub vol_echo: Option<(f32, bool)>,
    /// Peer value most recently delivered to the legacy virtual speaker.
    /// Separate from `vol_echo`, which is the platform's applied readback and
    /// may differ after Windows quantizes its audio-tapered control.
    legacy_last_delivered: Option<(f32, bool)>,
    /// Retry throttle for the legacy active-speaker reverse relay. Kept
    /// separate from `vol_echo`: a failed HAL send is not an echo guard.
    legacy_notify_retry: Option<DeviceVolumeNotifyRetry>,
    /// Peer-level controls are independent of media-session lifetime and are
    /// independent in the two endpoint directions.
    pub(crate) out_volume: DeviceVolumeRelay,
    pub(crate) in_volume: DeviceVolumeRelay,
    /// What this slot's virtual SPEAKER declares to the OS as its latency, and
    /// what the driver says it actually declares. See `crate::devdecl`.
    ///
    /// Speaker only. The microphone direction is deliberately never declared:
    /// its tail stage `hal_mic` is recorded in docs/spec-hal-mic-latency.md as a
    /// [0, 500 ms] free parameter with no restoring force, and broadcasting a
    /// random number to the system is worse than declaring nothing.
    pub decl_out: crate::devdecl::DeclState,
}

impl SlotRec {
    fn invalidate_volume_notifications(&mut self) {
        self.vol_echo = None;
        self.legacy_last_delivered = None;
        self.legacy_notify_retry = None;
        self.out_volume.invalidate_notification();
        self.in_volume.invalidate_notification();
    }

    fn claimed(&self) -> bool {
        !self.fingerprint.is_empty() || self.sent
    }

    fn state_label(&self) -> &'static str {
        match self.state {
            Some(s) => s.label(),
            None if self.sent => "pending",
            None => "free",
        }
    }
}

/// Queue one mode-B endpoint action by peer identity. This is used by the IPC
/// UI path; driver events use [`queue_slot_device_volume`] after the generation
/// gate has already resolved a slot. Network delivery is intentionally left to
/// the coordinator tick so no caller performs encrypted I/O while holding the
/// HAL state lock.
pub(crate) fn queue_peer_device_volume(
    inner: &Arc<DaemonInner>,
    fingerprint: &str,
    endpoint: DeviceVolumeEndpoint,
    scalar: f32,
    muted: Option<bool>,
) -> Result<()> {
    if effective_mode(inner) != Mode::B {
        anyhow::bail!("peer device volume is available only while mode B is in force");
    }
    if !scalar.is_finite() {
        anyhow::bail!("volume scalar must be finite");
    }
    let mut st = lk(&inner.haldev);
    // The first mode check gives a fast error. This one is the authority check
    // at the mutation point, after waiting for the slot lock.
    if effective_mode(inner) != Mode::B {
        anyhow::bail!("peer device volume is available only while mode B is in force");
    }
    let slot = st
        .table
        .slot_of(fingerprint)
        .ok_or_else(|| anyhow::anyhow!("peer {fingerprint} has no virtual device slot"))?
        as usize;
    let rec = st
        .slots
        .get_mut(slot)
        .filter(|rec| {
            rec.fingerprint == fingerprint
                && rec.sent
                && rec.sent_directions & endpoint.required_direction() != 0
        })
        .ok_or_else(|| {
            anyhow::anyhow!("peer {fingerprint} has no active virtual device binding")
        })?;
    // Keep the connection-map guard through the relay mutation. Registration
    // also needs this lock, so the connection whose id/capabilities we sample
    // cannot be replaced between the authority decision and the slot write.
    let state = lk(&inner.state);
    let (software_gain, queued, desired) =
        with_current_peer_device_volume_protocol(&state, fingerprint, |current| -> Result<_> {
            let modern = match current.protocol {
                PeerDeviceVolumeProtocol::Offline => false,
                PeerDeviceVolumeProtocol::PeerNotSharing => {
                    anyhow::bail!("peer is not currently sharing its audio endpoints")
                }
                PeerDeviceVolumeProtocol::Legacy => {
                    anyhow::bail!("peer does not support idle device-volume synchronization")
                }
                PeerDeviceVolumeProtocol::Modern => true,
            };
            let relay = endpoint.relay_mut(rec);
            let software_gain = modern
                && endpoint == DeviceVolumeEndpoint::DefaultOutput
                && relay.uses_software_gain();
            let queued = if software_gain {
                relay.queue_software_gain(scalar, muted, false)
            } else {
                relay.queue_intent(scalar, muted)
            };
            let desired = software_gain.then(|| relay.displayed_state()).flatten();
            Ok((software_gain, queued, desired))
        })?;
    drop(state);
    drop(st);
    if !queued {
        anyhow::bail!("volume request was invalid");
    }
    if software_gain {
        conn::sync_peer_software_gain(inner, fingerprint, desired);
    }
    Ok(())
}

/// Queue a genuine OS control change from one exact virtual endpoint.
fn queue_slot_device_volume(
    inner: &Arc<DaemonInner>,
    slot: u8,
    generation: u32,
    endpoint: DeviceVolumeEndpoint,
    scalar: f32,
    muted: bool,
) -> Option<bool> {
    if effective_mode(inner) != Mode::B || !scalar.is_finite() {
        return None;
    }
    let mut st = lk(&inner.haldev);
    if effective_mode(inner) != Mode::B {
        return None;
    }
    let Some(rec) = st.slots.get_mut(slot as usize) else {
        return None;
    };
    if rec.fingerprint.is_empty()
        || !rec.sent
        || rec.generation != generation
        || rec.sent_directions & endpoint.required_direction() == 0
    {
        return None;
    }
    let fingerprint = rec.fingerprint.clone();
    // Do not trust a protocol hint sampled before this HAL critical section:
    // connection replacement/capability downgrade can clear the relay while
    // this event is waiting for the lock.
    let state = lk(&inner.state);
    let (modern, software_gain, queued, desired) =
        with_current_peer_device_volume_protocol(&state, &fingerprint, |current| {
            let modern = current.protocol == PeerDeviceVolumeProtocol::Modern;
            let relay = endpoint.relay_mut(rec);
            let software_gain = modern
                && endpoint == DeviceVolumeEndpoint::DefaultOutput
                && relay.uses_software_gain();
            let queued = if software_gain {
                relay.queue_software_gain(scalar, Some(muted), true)
            } else if modern {
                relay.queue_local(scalar, Some(muted))
            } else {
                // A live v0/Unheard connection must not consume a stale v1 HAL echo
                // guard and thereby lose a genuine legacy session-bound write.
                relay.queue_intent(scalar, Some(muted))
            };
            let desired = software_gain.then(|| relay.displayed_state()).flatten();
            (modern, software_gain, queued, desired)
        });
    drop(state);
    drop(st);
    if queued && software_gain {
        conn::sync_peer_software_gain(inner, &fingerprint, desired);
    }
    queued.then_some(modern)
}

fn clear_matching_device_volume_pending(
    inner: &DaemonInner,
    slot: u8,
    endpoint: DeviceVolumeEndpoint,
    scalar: f32,
    muted: Option<bool>,
) {
    let mut hal = lk(&inner.haldev);
    let Some(rec) = hal.slots.get_mut(slot as usize) else {
        return;
    };
    let relay = endpoint.relay_mut(rec);
    if relay.pending.is_some_and(|pending| {
        (pending.scalar - scalar).abs() < HAL_VOL_EPS && pending.muted == muted
    }) {
        relay.pending = None;
    }
}

/// Accept an authenticated peer readback for its own real endpoint. Returns
/// false for an unknown slot or malformed scalar. A non-matching request id is
/// still retained as the latest observation, but it remains shadowed by local
/// pending intent until the matching acknowledgement arrives.
pub(crate) fn accept_peer_device_volume(
    inner: &Arc<DaemonInner>,
    fingerprint: &str,
    endpoint: DeviceVolumeEndpoint,
    connection: u64,
    revision: u64,
    request_id: Option<u64>,
    mut state: VolumeState,
) -> bool {
    if effective_mode(inner) != Mode::B || !state.scalar.is_finite() {
        return false;
    }
    state.scalar = state.scalar.clamp(0.0, 1.0);
    let mut hal = lk(&inner.haldev);
    if effective_mode(inner) != Mode::B {
        return false;
    }
    let daemon_state = lk(&inner.state);
    let Some(slot) = hal.table.slot_of(fingerprint) else {
        return false;
    };
    let Some(rec) = hal.slots.get_mut(slot as usize) else {
        return false;
    };
    if rec.fingerprint != fingerprint
        || !rec.sent
        || rec.sent_directions & endpoint.required_direction() == 0
    {
        return false;
    }
    let (accepted, software_gain) =
        with_current_peer_device_volume_protocol(&daemon_state, fingerprint, |current| {
            if current.connection_id != Some(connection)
                || current.protocol != PeerDeviceVolumeProtocol::Modern
            {
                return (false, None);
            }
            let relay = endpoint.relay_mut(rec);
            let accepted = relay.accept_remote(
                connection,
                revision,
                request_id,
                state,
                endpoint == DeviceVolumeEndpoint::DefaultOutput,
            );
            let software_gain = accepted
                .then(|| {
                    (endpoint == DeviceVolumeEndpoint::DefaultOutput)
                        .then(|| relay.software_gain)
                        .flatten()
                })
                .flatten();
            (accepted, software_gain)
        });
    if !accepted {
        return false;
    }
    drop(daemon_state);
    drop(hal);
    if endpoint == DeviceVolumeEndpoint::DefaultOutput {
        conn::sync_peer_software_gain(inner, fingerprint, software_gain);
    }
    true
}

/// Working local fallback state for a peer whose output has no writable
/// hardware scalar. Used when a newly opened B session receives its first
/// per-session fixed-device report.
pub(crate) fn peer_software_gain_authority(
    inner: &DaemonInner,
    fingerprint: &str,
) -> Option<Option<VolumeState>> {
    let hal = lk(&inner.haldev);
    let slot = hal.table.slot_of(fingerprint)? as usize;
    let rec = hal.slots.get(slot)?;
    if rec.fingerprint != fingerprint {
        return None;
    }
    rec.out_volume.remote?;
    Some(rec.out_volume.software_gain)
}

/// A live capability advertisement explicitly downgraded this peer to the
/// legacy session-bound contract. Offline/Unheard never calls this, so genuine
/// queued intent survives ordinary disconnects; an explicit v0 peer cannot
/// leave stale v1 gain and network volume active at the same time.
pub(crate) fn clear_peer_device_volume_protocol(
    inner: &Arc<DaemonInner>,
    fingerprint: &str,
    connection: u64,
) {
    let cleared = {
        let mut hal = lk(&inner.haldev);
        let state = lk(&inner.state);
        let Some(slot) = hal.table.slot_of(fingerprint) else {
            return;
        };
        let Some(rec) = hal.slots.get_mut(slot as usize) else {
            return;
        };
        if rec.fingerprint != fingerprint {
            return;
        }
        let cleared = with_current_peer_device_volume_protocol(&state, fingerprint, |current| {
            if current.connection_id != Some(connection) || current.version >= 1 {
                return false;
            }
            rec.out_volume.clear_protocol_state();
            rec.in_volume.clear_protocol_state();
            true
        });
        drop(state);
        cleared
    };
    if !cleared {
        return;
    }
    conn::sync_peer_software_gain(inner, fingerprint, None);
}

/// Apply authoritative peer readbacks to both virtual directions. The HAL send
/// remains outside the state lock because the bridge has a bounded but real
/// control timeout.
fn push_peer_device_volumes(inner: &DaemonInner, hal: &halbridge::HalBridge) {
    let mut pending = Vec::new();
    let planned_at = Instant::now();
    {
        let mut st = lk(&inner.haldev);
        for (slot, rec) in st.slots.iter_mut().enumerate() {
            if rec.fingerprint.is_empty() || rec.state != Some(HalSlotState::Bound) {
                continue;
            }
            for endpoint in [
                DeviceVolumeEndpoint::DefaultOutput,
                DeviceVolumeEndpoint::DefaultInput,
            ] {
                if rec.sent_directions & endpoint.required_direction() == 0 {
                    continue;
                }
                if let Some((scalar, muted)) = endpoint.relay(rec).plan_notify(planned_at) {
                    pending.push((
                        slot as u8,
                        rec.fingerprint.clone(),
                        endpoint,
                        endpoint.hal_endpoint(slot as u8),
                        rec.generation,
                        scalar,
                        muted,
                    ));
                }
            }
        }
    }
    for (slot, fingerprint, endpoint, at, generation, scalar, muted) in pending {
        #[cfg(windows)]
        let _ = (at, generation);
        #[cfg(windows)]
        let (delivered, driver_echo) = match volume::set_audiohub_peer_endpoint_volume(
            &fingerprint,
            endpoint == DeviceVolumeEndpoint::DefaultInput,
            scalar,
            muted,
        ) {
            Ok(state) => (true, Some((state.scalar, state.muted))),
            Err(err) => {
                dlog!(
                    "[audiohubd] hal: cannot apply peer {} {} scalar/mute through its exact \
                         Windows endpoint: {err:#}",
                    fingerprint,
                    endpoint.as_wire()
                );
                (false, None)
            }
        };
        #[cfg(not(windows))]
        let (delivered, driver_echo) = (
            hal.notify_volume(at, generation, scalar, muted),
            Some((scalar, muted)),
        );
        let mut st = lk(&inner.haldev);
        let Some(rec) = st.slots.get_mut(slot as usize) else {
            continue;
        };
        if rec.fingerprint != fingerprint
            || rec.generation != generation
            || rec.state != Some(HalSlotState::Bound)
            || rec.sent_directions & endpoint.required_direction() == 0
        {
            continue;
        }
        endpoint.relay_mut(rec).record_notify_result(
            (scalar, muted),
            driver_echo,
            delivered,
            Instant::now(),
        );
    }
}

/// Deliver all due mode-B endpoint writes without holding daemon state locks
/// across encrypted control I/O.
///
/// The connection capability and advertised mode are presentation gates here;
/// the receiving peer independently enforces its local Share-mode authority.
/// Keeping both gates avoids sending a control an honest peer must reject while
/// still treating the receiver as the security boundary.
fn flush_pending_device_volumes(inner: &DaemonInner) {
    if effective_mode(inner) != Mode::B {
        return;
    }

    let conns: Vec<Arc<crate::ConnShared>> = lk(&inner.state)
        .conns
        .values()
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    let eligible: HashMap<String, Arc<crate::ConnShared>> = conns
        .into_iter()
        .filter(|conn| lk(&conn.peer_mode).mode() == Some(Mode::Share))
        .filter(|conn| lk(&conn.peer_audio_capabilities).device_volume_version() >= 1)
        .map(|conn| (conn.fp.clone(), conn))
        .collect();

    let now = Instant::now();
    let mut due = Vec::new();
    {
        let st = lk(&inner.haldev);
        for (slot, rec) in st.slots.iter().enumerate() {
            if rec.fingerprint.is_empty() || !rec.sent || rec.state != Some(HalSlotState::Bound) {
                continue;
            }
            let Some(conn) = eligible.get(&rec.fingerprint) else {
                continue;
            };
            let connection = conn.connection_id;
            for endpoint in [
                DeviceVolumeEndpoint::DefaultOutput,
                DeviceVolumeEndpoint::DefaultInput,
            ] {
                if rec.sent_directions & endpoint.required_direction() == 0 {
                    continue;
                }
                if let Some(pending) = endpoint.relay(rec).due_for(connection, now) {
                    due.push((
                        slot as u8,
                        rec.fingerprint.clone(),
                        endpoint,
                        connection,
                        pending,
                        conn.clone(),
                    ));
                }
            }
        }
    }

    for (slot, fingerprint, endpoint, connection, pending, conn) in due {
        let sent = conn
            .send_msg(&SessionMsg::DeviceVolumeSet {
                endpoint: endpoint.as_wire().to_string(),
                request_id: pending.request_id,
                scalar: pending.scalar,
                muted: pending.muted,
            })
            .is_ok();
        if !sent {
            continue;
        }
        let sent_at = Instant::now();
        let mut st = lk(&inner.haldev);
        let Some(rec) = st.slots.get_mut(slot as usize) else {
            continue;
        };
        if rec.fingerprint == fingerprint {
            endpoint
                .relay_mut(rec)
                .mark_sent(pending.request_id, connection, sent_at);
        }
    }
}

/// The whole reconcile, as a pure function.
///
/// `observed` is `None` when the device list could not be read at all (no
/// macOS, or an enumeration that returned nothing — a real Mac always has at
/// least one device). Treating "I cannot see" as "nothing is published" would
/// re-`Set` all sixteen slots every second forever.
pub fn plan_binds(
    desired: &[DesiredDevice],
    slots: &[SlotRec],
    observed: Option<&HashSet<String>>,
) -> Vec<BindAction> {
    let mut actions = Vec::new();
    let mut wanted = vec![false; slots.len()];

    for d in desired {
        let s = d.slot as usize;
        if s >= slots.len() {
            continue;
        }
        wanted[s] = true;
        let rec = &slots[s];
        let identity =
            rec.fingerprint == d.fingerprint && rec.out_uid == d.out_uid && rec.in_uid == d.in_uid;
        let observed_directions = observed.map(|o| {
            (if o.contains(&d.out_uid) {
                HAL_PUBLISH_OUT
            } else {
                0
            }) | (if o.contains(&d.in_uid) {
                HAL_PUBLISH_IN
            } else {
                0
            })
        });
        // "Published" needs every REQUESTED half and no stale extra half: the
        // driver acknowledged the exact mask AND the system really lists it.
        // Either one alone has a failure mode
        // that is completely silent — an ack without publication is the
        // Initialize race, publication without an ack is a slot we would
        // happily hand to somebody else.
        let published = identity
            && rec.sent
            && rec.acked
            && rec.state == Some(HalSlotState::Bound)
            && rec.published_directions == d.directions
            && observed_directions.map_or(true, |m| m == d.directions);
        if !published {
            // The idempotent upsert. This is also the ONLY thing sent after a
            // daemon restart — never a Clear first, which would take the user's
            // default output with it (spec-m5b §1).
            actions.push(BindAction::Set(d.to_bind()));
            continue;
        }
        if rec.sent_out_name != d.out_name
            || rec.sent_in_name != d.in_name
            || rec.sent_directions != d.directions
            || rec.sent_online != d.online
        {
            // Same UID, new name: an in-place rename on the driver's side.
            actions.push(BindAction::Set(d.to_bind()));
        }
    }

    // A device the system publishes that nobody intends. It can only come from
    // a slot this daemon has lost track of (its table was deleted, or another
    // daemon bound it), and the only way to find out which slot it is, is to
    // ask: a Clear with the wrong generation is ignored, but the driver answers
    // with a BindState carrying the real one, so the next pass gets it right.
    let orphan = observed.map_or(false, |o| {
        // "Accounted for" includes the slots we still TRACK, not just the ones
        // we still want: a device on its way out through a Clear is explained
        // by the record that Clear is aimed at, and treating it as an orphan
        // would make every retirement sweep all sixteen slots.
        let mine: HashSet<&str> = desired
            .iter()
            .flat_map(|d| [d.out_uid.as_str(), d.in_uid.as_str()])
            .chain(
                slots
                    .iter()
                    .filter(|r| !r.fingerprint.is_empty())
                    .flat_map(|r| [r.out_uid.as_str(), r.in_uid.as_str()]),
            )
            .collect();
        o.iter()
            .any(|u| u.starts_with(UID_PREFIX) && !mine.contains(u.as_str()))
    });

    for (s, rec) in slots.iter().enumerate() {
        if wanted[s] || rec.clearing || rec.state == Some(HalSlotState::Free) {
            continue;
        }
        if rec.claimed() || (orphan && rec.state.is_none()) {
            actions.push(BindAction::Clear {
                slot: s as u8,
                generation: rec.generation,
            });
        }
    }
    actions
}

// ---------------------------------------------------------------- runtime

/// How long a stopped output device keeps its session. Safari opens and closes
/// the device on every play/pause; tearing the network stream down and
/// rebuilding it (handshake plus a jitter-buffer refill) each time is audible.
const LINGER_OUT: Duration = Duration::from_secs(3);
/// The input direction lingers far less on purpose: this is somebody else's
/// microphone, and one second past the last user is the privacy trade-off.
const LINGER_IN: Duration = Duration::from_secs(1);
/// A slot whose Clear went unanswered this long is reused anyway. Correctness
/// does not depend on it — the generation check does — so this only stops a
/// wedged slot from costing capacity forever.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(10);
/// Do not re-Set the same slot faster than this while it stays unpublished. A
/// driver that ignores us must not be flooded, and every Set costs the driver a
/// device-list announcement.
const SET_COOLDOWN: Duration = Duration::from_millis(1000);
/// A peer that is offline is dialled at most this often by the session worker;
/// the reconnect supervisor's own ladder does the rest.
const OPEN_COOLDOWN: Duration = Duration::from_secs(2);
/// The device list is enumerated at most this often. Worst-case self-healing
/// latency, and the reason this is a poll rather than a listener (spec-m5b §8).
const OBSERVE_EVERY: Duration = Duration::from_secs(1);

pub(crate) struct HalDevState {
    pub slots: Vec<SlotRec>,
    pub table: SlotTable,
    /// Slots the ATTACHED driver offers. 0 while detached, which is why a
    /// detached bridge cannot silently look like a full pool.
    pub capacity: usize,
    pub attach_epoch: u64,
    /// fingerprint -> display name, recomputed every pass.
    pub display: HashMap<String, String>,
    /// fingerprint -> why it has no devices.
    pub reasons: HashMap<String, String>,
    last_set: Vec<Option<Instant>>,
    clear_at: Vec<Option<Instant>>,
    open_at: Vec<Option<Instant>>,
    opening_out: Vec<bool>,
    opening_in: Vec<bool>,
}

impl HalDevState {
    pub(crate) fn new(table: SlotTable) -> HalDevState {
        HalDevState {
            slots: vec![SlotRec::default(); HAL_MAX_SLOTS],
            table,
            capacity: 0,
            attach_epoch: 0,
            display: HashMap::new(),
            reasons: HashMap::new(),
            last_set: vec![None; HAL_MAX_SLOTS],
            clear_at: vec![None; HAL_MAX_SLOTS],
            open_at: vec![None; HAL_MAX_SLOTS],
            opening_out: vec![false; HAL_MAX_SLOTS],
            opening_in: vec![false; HAL_MAX_SLOTS],
        }
    }

    pub(crate) fn slot_of(&self, fingerprint: &str) -> Option<u8> {
        self.table.slot_of(fingerprint)
    }

    /// Device records that still occupy the attached driver's live slot set.
    ///
    /// The persistent assignment table deliberately survives a switch out of
    /// mode B so switching back can reuse the same endpoint identity. It is
    /// therefore not a count of currently bound or clearing devices.
    pub(crate) fn device_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|record| !record.fingerprint.is_empty())
            .count()
    }

    /// The published bitmask the tx loop drains idle speakers from.
    fn published_mask(&self) -> u16 {
        let mut m = 0u16;
        for (s, rec) in self.slots.iter().enumerate() {
            if rec.state == Some(HalSlotState::Bound)
                && rec.published_directions & HAL_PUBLISH_OUT != 0
                && !rec.fingerprint.is_empty()
            {
                m |= 1 << s;
            }
        }
        m
    }

    /// `hal.devices` for `daemon.status`.
    pub(crate) fn device_infos(
        &self,
        counters: &[halbridge::HalSlotCounters],
    ) -> Vec<HalDeviceInfo> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.fingerprint.is_empty())
            .map(|(s, r)| {
                let c = counters.get(s).copied().unwrap_or_default();
                HalDeviceInfo {
                    slot: s as u8,
                    fingerprint: r.fingerprint.clone(),
                    out_uid: r.out_uid.clone(),
                    in_uid: r.in_uid.clone(),
                    out_name: r.sent_out_name.clone(),
                    in_name: r.sent_in_name.clone(),
                    generation: r.generation,
                    state: r.state_label().to_string(),
                    observed: r.observed,
                    requested_directions: Some(r.sent_directions),
                    published_directions: Some(r.published_directions),
                    observed_directions: Some(r.observed_directions),
                    peer_connected: r.peer_connected,
                    io_out: r.io_out,
                    io_in: r.io_in,
                    spk_frames: c.spk_frames,
                    mic_frames: c.mic_frames,
                    mic_dropped: c.mic_dropped,
                }
            })
            .collect()
    }

    pub(crate) fn peer_device(&self, fingerprint: &str) -> Option<PeerHalDevice> {
        let s = self.table.slot_of(fingerprint)? as usize;
        let r = self.slots.get(s)?;
        if r.fingerprint != fingerprint || !r.sent {
            return None;
        }
        Some(PeerHalDevice {
            out_name: r.sent_out_name.clone(),
            in_name: r.sent_in_name.clone(),
            out_uid: r.out_uid.clone(),
            in_uid: r.in_uid.clone(),
            state: r.state_label().to_string(),
            observed: r.observed,
            requested_directions: Some(r.sent_directions),
            published_directions: Some(r.published_directions),
            observed_directions: Some(r.observed_directions),
            out_volume: r.out_volume.displayed_state(),
            in_volume: r.in_volume.remote,
            out_volume_pending: r.out_volume.pending.is_some(),
            in_volume_pending: r.in_volume.pending.is_some(),
            out_volume_software_gain: r.out_volume.uses_software_gain(),
            // Connection-scoped capability is filled by ipcserv::peer_states;
            // HalDev owns slot state, not claims made by a possibly-dead peer.
            device_volume_version: 0,
        })
    }
}

/// Work the coordinator must not do on its own tick: `open_session` connects
/// synchronously (TCP + verify + secure handshake), and an offline peer would
/// otherwise stall the device reconcile and the volume relay behind it.
pub(crate) enum SessCmd {
    Open {
        slot: u8,
        fingerprint: String,
        kind: &'static str,
    },
    Close {
        slot: u8,
        out: bool,
        id: u32,
    },
}

// ---------------------------------------------------------------- mode

/// What mode is actually in force.
///
/// The only mode that can be requested but not delivered is `B`, and it falls
/// back to `A` rather than to `Share`: the user asked to *use* other machines,
/// and only the driver-specific half of that is unavailable. Falling back to
/// `Share` would flip which side of the §13 exclusion the machine is on — a far
/// larger change than the one the missing driver actually forces, and one that
/// would silently stop the machine consuming while the UI still showed a
/// consumer mode selected.
///
/// The `B` availability test is deliberately keyed on "this daemon HAS a HAL
/// bridge", not on "a driver is attached this instant". coreaudiod restarts;
/// the bridge reconnects within seconds and the bindings survive it (plan §7.3
/// keeps the devices through an outage). A mode that flapped on every such blip
/// would tear down every virtual device and hand the UI's session controls back
/// for a few seconds, which is precisely the churn this design exists to avoid.
pub(crate) fn effective_mode(inner: &DaemonInner) -> Mode {
    let want = lk(&inner.settings).mode;
    match want {
        Mode::Share => Mode::Share,
        Mode::A => Mode::A,
        Mode::B if inner.hal().is_some() => Mode::B,
        Mode::B => Mode::A,
    }
}

/// Whether a locally-originated `session.open` is allowed right now — i.e.
/// whether this machine may reach out and use another one.
///
/// Two modes refuse, for unrelated reasons that must not be collapsed into one
/// message, because the user's next action differs:
///
///   - `Share` (plan §13): this machine is the one being used. Nothing about
///     the peer or the device selection will change that; the fix is to pick a
///     consumer mode, which is a decision, not a retry.
///   - `B` (spec-m5b §6.1): the system's device selection is the session
///     control. An app selects the output-class device "AudioHub – X" and the daemon opens the
///     stream behind it. A UI that could also open sessions by peer would be
///     mode A wearing mode B's labels, and every mode-B property (one device =
///     one peer, the selection living in the system) would quietly stop
///     holding. The fix is to go and select the device.
///
/// `override` exists for the CLI and the probes, which have to drive their own
/// daemon directly. It cannot defeat the peer's half of the exclusion — see
/// `refuse_being_used`, which runs on the other machine.
pub(crate) fn refuse_using_others(mode: Mode, override_mode: bool) -> Option<String> {
    if override_mode {
        return None;
    }
    match mode {
        Mode::A => None,
        Mode::Share => Some(
            "share mode: this machine shares its own audio devices and does not use other \
             machines' (plan §13 — the three modes are mutually exclusive). Switch to mode A or \
             mode B to use a peer, or pass override:true to force one anyway"
                .to_string(),
        ),
        Mode::B => Some(
            "mode B: sessions are driven by the system device selection — select \
             the peer's AudioHub device in 系统设置 › 声音 or in the application \
             (pass override:true to force one anyway)"
                .to_string(),
        ),
    }
}

/// Whether a session opened by `origin` may still exist once `mode` is in force.
///
/// This is the **mirror of the two `refuse_*` gates above**, and it has to be:
/// the gates decide what may be OPENED in a mode, and this decides what may
/// SURVIVE a switch into that mode. If the two ever disagree, the daemon ends
/// up holding a session it would refuse to create — which is exactly the bug
/// this function was extracted to kill (2026-08-15, reproduced 3/3 on the live
/// daemon: every A→B switch stranded the mode-A `sysaudio` sender, and three
/// cycles left five senders streaming into one peer at 200 packets/s each).
///
/// The predicate it replaces asked only "does the new mode still allow the SIDE
/// that opened this?" — `theirs { !serves_peers() } else { !consumes_peers() }`.
/// Both A and B consume, so nothing locally-opened was ever doomed by a switch
/// between them. The side is not enough information: A and B are two different
/// MECHANISMS of consuming, and a session belongs to exactly one of them.
///
/// `override_mode` deliberately has no counterpart here. It exists so probes can
/// drive their own daemon past the open gate; it is not a licence for a session
/// to outlive the mode whose machinery feeds it. A forced `halspk` session in
/// mode A has no virtual device behind it, and a forced `sysaudio` session in
/// mode B is the leak above wearing a flag.
pub(crate) fn mode_permits_origin(mode: Mode, origin: SessionOrigin) -> bool {
    match origin {
        // The peer is the opener; we are the provider. Only share mode serves.
        SessionOrigin::Peer => mode.serves_peers(),
        // IPC (`session.open`) — mode A's way of consuming a peer. Mode B
        // refuses these at the gate because there the SYSTEM's device
        // selection is what opens sessions.
        SessionOrigin::User => matches!(mode, Mode::A),
        // A virtual device started doing IO, which can only happen in mode B
        // because that is the only mode in which virtual devices exist.
        SessionOrigin::Hal { .. } => matches!(mode, Mode::B),
    }
}

/// Whether a PEER may open a stream on us — the enforcement half of plan §13.
///
/// This is the guard that actually prevents the relay: X sharing its "default
/// microphone" while X is a mode-B consumer would hand out **Z's** microphone,
/// and if Z is using X the graph closes into a cycle whose latency grows
/// without bound. Refusing here is what makes that unreachable, and it is
/// checked against OUR OWN mode only — never against anything the peer claimed,
/// which is why a peer that lies or never advertises changes nothing.
///
/// There is deliberately **no override**. The probe flag on `session.open` is
/// about driving one's own daemon; nothing should be able to talk another
/// machine out of this.
/// Why a peer has no virtual devices, when the reason is the mode itself.
/// `None` in mode B — that is the mode where devices are supposed to exist.
///
/// plan §13 推论 3 rides on this being a *refusal to desire* rather than a
/// "keep but silence": leaving share mode's peers out of the desired set makes
/// the ordinary reconcile diff remove their devices unconditionally, exactly as
/// unpairing does. "Kept but silent" is only the rule for a peer going offline
/// (§7.3), and applying it here would leave a machine that is no longer a
/// consumer still publishing consumer devices.
///
/// Share and A are separate strings because the user's next step differs, and
/// because a shared "not mode B" reason would label every card on a
/// share-mode machine with mode A's explanation. The frontend switches on these
/// exact values — see `reasons_the_frontend_can_explain`, which reads the
/// frontend and fails if either side drifts.
pub(crate) fn no_device_reason(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::B => None,
        Mode::Share => Some("mode_share"),
        Mode::A => Some("mode_a"),
    }
}

pub(crate) fn refuse_being_used(mode: Mode) -> Option<String> {
    if mode.serves_peers() {
        return None;
    }
    Some(format!(
        "this machine is in mode {mode} and cannot be used as an audio device right now \
         (plan §13: the machine that shares must not also consume, or it becomes an \
         unwitting relay). It has to be switched to share mode to serve you"
    ))
}

// ---------------------------------------------------------------- reconcile

/// UIDs the SYSTEM currently publishes for us. `None` when the device list
/// could not be read at all.
fn observe() -> Option<HashSet<String>> {
    let all = audiohub_core::audio::list_devices_detailed();
    if all.is_empty() {
        // Not "nothing of ours is published" — "I cannot see". A real Mac
        // always has at least one device, so an empty listing means the
        // enumeration failed or this is not macOS.
        return None;
    }
    Some(
        all.into_iter()
            .filter_map(|d| d.uid)
            .filter(|u| u.starts_with(UID_PREFIX))
            .collect(),
    )
}

pub(crate) struct PassInputs {
    pub(crate) desired: Vec<DesiredDevice>,
    display: HashMap<String, String>,
    pub(crate) reasons: HashMap<String, String>,
    paired: HashSet<String>,
}

/// Everything the reconcile needs from the peer store and the settings, with
/// the naming pass already applied.
///
/// `pub(crate)` for the §13 tests in `mode_tests`: `capacity` is a parameter, so
/// they can ask "what WOULD the coordinator want, if a driver with N slots were
/// attached" on a host that has no driver — which is every CI host and every
/// machine running the suite. Testing the mode branch through the real function
/// is the point; a test that re-implemented the branch would pass while
/// `compute_desired` ignored the mode entirely.
pub(crate) fn compute_desired(
    inner: &DaemonInner,
    capacity: usize,
    table: &mut SlotTable,
) -> PassInputs {
    let mode = effective_mode(inner);
    let (remove_offline, mark_offline, native_locale) = {
        let s = lk(&inner.settings);
        (
            s.remove_virtual_on_disconnect,
            s.mark_offline_devices,
            s.native_locale.clone(),
        )
    };
    let peers = PeerStore::load_at(Some(&inner.cfg_dir))
        .map(|s| s.list().to_vec())
        .unwrap_or_default();
    let paired: HashSet<String> = peers.iter().map(|p| p.fingerprint.clone()).collect();
    let connected: HashSet<String> = {
        let st = lk(&inner.state);
        st.conns
            .iter()
            .filter(|(_, c)| c.alive.load(Ordering::SeqCst))
            .map(|(fp, _)| fp.clone())
            .collect()
    };
    // A live capability advertisement is the newest fact.  When the channel
    // is gone, the persisted slot mask is deliberately retained: offline
    // devices keep the user's selection, and "disconnected" is not evidence
    // that a microphone or speaker ceased to exist.
    let live_capabilities: HashMap<String, u8> = {
        let st = lk(&inner.state);
        st.conns
            .iter()
            .filter(|(_, c)| c.alive.load(Ordering::SeqCst))
            .filter_map(|(fp, c)| {
                crate::lk(&c.peer_audio_capabilities)
                    .known()
                    .map(|(input, output)| {
                        let mask = (if output { HAL_PUBLISH_OUT } else { 0 })
                            | (if input { HAL_PUBLISH_IN } else { 0 });
                        (fp.clone(), mask)
                    })
            })
            .collect()
    };
    // Names are resolved over EVERY paired peer, not just the ones that get a
    // device: the disambiguation order must not change when a peer drops out of
    // the desired set, or the survivor would be renamed by somebody else's
    // disconnect.
    let display = display_names(
        &peers
            .iter()
            .map(|p| NameInput {
                fingerprint: p.fingerprint.clone(),
                base: base_name(p),
                added_unix: p.added_unix,
            })
            .collect::<Vec<_>>(),
    );

    let mut desired = Vec::new();
    let mut reasons = HashMap::new();
    for p in &peers {
        let fp = &p.fingerprint;
        let online = connected.contains(fp);
        if let Some(why) = no_device_reason(mode) {
            reasons.insert(fp.clone(), why.to_string());
            continue;
        }
        if capacity == 0 {
            reasons.insert(fp.clone(), "no_driver".to_string());
            continue;
        }
        if !online && remove_offline {
            reasons.insert(fp.clone(), "removed_while_offline".to_string());
            continue;
        }
        let Some(slot) = table.assign(fp, capacity) else {
            reasons.insert(fp.clone(), "capacity".to_string());
            continue;
        };
        if let Some(&mask) = live_capabilities.get(fp) {
            table.set_directions(fp, mask);
        }
        // An old/not-yet-advertised peer stays backward compatible until its
        // first exact answer. An explicit Some(0) still publishes neither.
        let directions = table.directions_of(fp).unwrap_or(HAL_PUBLISH_BOTH);
        let name = display.get(fp).cloned().unwrap_or_else(|| fp.clone());
        let marked_offline = mark_offline && !online;
        let (out_name, in_name) = device_names(&name, marked_offline, &native_locale);
        desired.push(DesiredDevice {
            slot,
            fingerprint: fp.clone(),
            out_uid: uid_out(fp),
            in_uid: uid_in(fp),
            out_name,
            in_name,
            display: device_display_name(&name, marked_offline, &native_locale),
            directions,
            online,
        });
    }
    PassInputs {
        desired,
        display,
        reasons,
        paired,
    }
}

/// One reconcile pass. `observed` is passed in so the enumeration (a few dozen
/// CoreAudio calls) can run on its own slower cadence.
fn reconcile(inner: &DaemonInner, hal: &halbridge::HalBridge, observed: Option<&HashSet<String>>) {
    let capacity = hal.slot_count();
    let mut st = lk(&inner.haldev);
    st.capacity = capacity;

    // A fresh handshake means the driver has kept its bindings but nothing of
    // ours is acknowledged any more, and — the part that is easy to miss — the
    // driver only replays a slot's IO state and volume when an idempotent Set
    // lands on it. Dropping the acks is what makes the pass below re-Set every
    // slot we still intend, which is what gets that replay.
    let epoch = hal.attach_epoch();
    if epoch != st.attach_epoch {
        st.attach_epoch = epoch;
        for rec in st.slots.iter_mut() {
            rec.acked = false;
            rec.state = None;
            rec.clearing = false;
        }
        st.last_set.iter_mut().for_each(|t| *t = None);
        st.clear_at.iter_mut().for_each(|t| *t = None);
        dlog!("[audiohubd] hal: driver re-attached ({capacity} slots); re-stating every binding");
    }

    // Snapshotted BEFORE the pass: `compute_desired` assigns slots to peers
    // that do not have one yet, and an assignment that is not written back is
    // an assignment that comes back different after a restart.
    let table_before = st.table.clone();
    let PassInputs {
        desired,
        display,
        reasons,
        paired,
    } = compute_desired(inner, capacity, &mut st.table);
    st.table.retain(&paired);
    let table_changed = !st.table.same_as(&table_before);
    st.display = display;
    st.reasons = reasons;

    // The slot table is the assignment; the records carry the identity we last
    // put on the wire. Keep the two in step BEFORE planning, so a slot whose
    // peer was unpaired stops being "wanted" immediately.
    for d in &desired {
        let rec = &mut st.slots[d.slot as usize];
        if rec.fingerprint != d.fingerprint {
            *rec = SlotRec {
                fingerprint: d.fingerprint.clone(),
                out_uid: d.out_uid.clone(),
                in_uid: d.in_uid.clone(),
                ..SlotRec::default()
            };
        }
        rec.peer_connected = d.online;
        if let Some(o) = observed {
            rec.observed_directions = (if o.contains(&d.out_uid) {
                HAL_PUBLISH_OUT
            } else {
                0
            }) | (if o.contains(&d.in_uid) {
                HAL_PUBLISH_IN
            } else {
                0
            });
            rec.observed = rec.acked
                && rec.state == Some(HalSlotState::Bound)
                && rec.published_directions == d.directions
                && rec.observed_directions == d.directions;
        }
    }

    let now = Instant::now();
    for (i, at) in st.clear_at.clone().iter().enumerate() {
        if let Some(t) = at {
            if now.duration_since(*t) > CLEAR_TIMEOUT {
                // Never answered. The generation check is what actually keeps
                // reuse safe, so this only stops one wedged slot from costing
                // capacity forever.
                st.slots[i].clearing = false;
                st.clear_at[i] = None;
            }
        }
    }

    // Planned under the lock, SENT outside it. A mach send waits up to its
    // 500ms timeout when the driver's queue is full, and sixteen of them with
    // this lock held would block `daemon.status` and `peers.list` for seconds.
    // Safe because only THIS thread mutates the fields below (the session
    // worker touches `sess_*` / `opening_*` and nothing else).
    let pending: Vec<BindAction> = plan_binds(&desired, &st.slots, observed)
        .into_iter()
        .filter(|a| match a {
            BindAction::Set(req) => {
                let s = req.slot as usize;
                let Some(t) = st.last_set[s] else { return true };
                // A rename is a user-visible action and goes out at once; a
                // repeat of an unanswered Set is a retry and waits, so a driver
                // that ignores us is not flooded (each Set costs it a
                // device-list announcement).
                let renaming = st.slots[s].sent_out_name != req.out_name
                    || st.slots[s].sent_in_name != req.in_name
                    || st.slots[s].sent_directions != req.directions;
                renaming || now.duration_since(t) >= SET_COOLDOWN
            }
            BindAction::Clear { .. } => true,
        })
        .collect();
    let mask = st.published_mask();
    drop(st);
    hal.set_published(mask);

    for a in pending {
        match a {
            BindAction::Set(req) => {
                if !hal.bind_set(&req) {
                    continue; // no driver, or the queue is full: retried next pass
                }
                let mut st = lk(&inner.haldev);
                let rec = &mut st.slots[req.slot as usize];
                rec.fingerprint = req.peer_key.clone();
                rec.out_uid = req.out_uid.clone();
                rec.in_uid = req.in_uid.clone();
                rec.sent_out_name = req.out_name.clone();
                rec.sent_in_name = req.in_name.clone();
                rec.sent_directions = req.directions;
                rec.sent_online = req.online;
                rec.sent = true;
                rec.acked = false;
                st.last_set[req.slot as usize] = Some(now);
            }
            BindAction::Clear { slot, generation } => {
                if !hal.bind_clear(slot, generation) {
                    continue;
                }
                let mut st = lk(&inner.haldev);
                st.slots[slot as usize].clearing = true;
                st.clear_at[slot as usize] = Some(now);
                dlog!("[audiohubd] hal: retiring slot {slot} (generation {generation})");
            }
        }
    }
    if table_changed {
        save_table(inner);
    }
}

pub(crate) fn save_table(inner: &DaemonInner) {
    let (table, dir) = (lk(&inner.haldev).table.clone(), inner.cfg_dir.clone());
    if let Err(e) = table.save(&dir) {
        dlog!("[audiohubd] hal: could not persist the slot table ({e:#}); a restart may re-assign slots and lose the user's default output");
    }
}

// ---------------------------------------------------------------- events

fn apply_events(inner: &Arc<DaemonInner>, hal: &halbridge::HalBridge) {
    let events = hal.drain_events();
    if events.is_empty() {
        return;
    }
    // A slider drag posts a burst; only where it ENDED is worth a round trip,
    // and it is per SLOT — the peer that owns slot 3 must not be moved by a
    // drag on slot 5's device, which is exactly what a single global "latest"
    // did (lib.rs's un-filtered fan-out).
    let mut latest: HashMap<(u8, DeviceVolumeEndpoint), (u32, f32, bool)> = HashMap::new();
    for ev in events {
        match ev {
            HalControlEvent::Attached {
                session_id,
                slot_count,
            } => {
                dlog!("[audiohubd] hal: attached, session {session_id}, {slot_count} slots");
                // A new driver session may have rebuilt every endpoint volume
                // node from defaults. Peer/local intent survives, but every
                // prior delivery acknowledgement belongs to the old session.
                let mut st = lk(&inner.haldev);
                for rec in st.slots.iter_mut() {
                    rec.invalidate_volume_notifications();
                }
            }
            HalControlEvent::Detached => {
                let mut st = lk(&inner.haldev);
                for rec in st.slots.iter_mut() {
                    rec.invalidate_volume_notifications();
                    rec.acked = false;
                    rec.state = None;
                    rec.published_directions = 0;
                    rec.observed_directions = 0;
                    rec.observed = false;
                    rec.io_out = false;
                    rec.io_in = false;
                    // We no longer KNOW what the driver's latency property
                    // says, so stop claiming we do. Not zeroed — cleared: the
                    // difference is that `want` survives (it is our own
                    // measurement, still valid) while `acked` goes back to
                    // "never answered" so the next attach re-establishes it.
                    //
                    // Without this, a driver that came back with a property of
                    // 0 (coreaudiod restarted; that value lives in the driver's
                    // device record, not ours) would face a daemon whose
                    // `acked` still equalled `want`, so it would never re-send
                    // and the device would silently go back to declaring zero.
                    rec.decl_out.acked = None;
                    rec.decl_out.pending = false;
                    rec.decl_out.tries = 0;
                }
                for s in 0..HAL_MAX_SLOTS {
                    inner.hal_mic_io[s].store(true, Ordering::Relaxed);
                }
            }
            HalControlEvent::BindState {
                slot,
                generation,
                state,
                published,
            } => {
                let mut st = lk(&inner.haldev);
                let Some(rec) = st.slots.get_mut(slot as usize) else {
                    continue;
                };
                let changed_directions = rec.published_directions ^ published;
                if rec.generation != generation
                    || (state == HalSlotState::Bound && rec.state != Some(HalSlotState::Bound))
                    || changed_directions != 0
                {
                    rec.invalidate_volume_notifications();
                }
                let mic_withdrawn = apply_published_directions(rec, published);
                rec.generation = generation;
                rec.state = Some(state);
                if mic_withdrawn {
                    // This endpoint was explicitly withdrawn, not handed to a
                    // new tenant. Keep the data plane shut until a restored
                    // microphone sends a fresh StartIO event.
                    inner.hal_mic_io[slot as usize].store(false, Ordering::Relaxed);
                }
                match state {
                    HalSlotState::Bound => {
                        rec.acked = true;
                        rec.observed = rec.observed_directions == rec.sent_directions
                            && published == rec.sent_directions;
                    }
                    HalSlotState::Free => {
                        // The slot is genuinely retired now, so it may be
                        // handed to another peer. Everything about the previous
                        // tenant goes with it — a stale vol_echo would suppress
                        // the first volume the NEXT peer should have received.
                        *rec = SlotRec {
                            generation,
                            state: Some(state),
                            ..SlotRec::default()
                        };
                        st.clear_at[slot as usize] = None;
                        st.last_set[slot as usize] = None;
                        // Back to the "not told yet" default. Leaving it false
                        // would make the next peer's virtual microphone silent
                        // until its first IoState arrived.
                        inner.hal_mic_io[slot as usize].store(true, Ordering::Relaxed);
                    }
                    HalSlotState::Delisted => rec.acked = false,
                }
            }
            HalControlEvent::IoState {
                at,
                generation,
                running,
            } => {
                let mut st = lk(&inner.haldev);
                let Some(rec) = st.slots.get_mut(at.slot as usize) else {
                    continue;
                };
                if rec.generation != generation {
                    continue;
                }
                let now = Instant::now();
                if at.input {
                    rec.io_in = running;
                    rec.io_in_off_since = (!running).then_some(now);
                    inner.hal_mic_io[at.slot as usize].store(running, Ordering::Relaxed);
                } else {
                    rec.io_out = running;
                    rec.io_out_off_since = (!running).then_some(now);
                }
                let fp = rec.fingerprint.clone();
                dlog!(
                    "[audiohubd] hal: slot {} {} io {} (peer {})",
                    at.slot,
                    if at.input { "microphone" } else { "speaker" },
                    if running { "started" } else { "stopped" },
                    if fp.is_empty() { "-" } else { &fp }
                );
            }
            HalControlEvent::LatencyState {
                at,
                generation,
                frames,
                pending,
            } => {
                // The driver's account of what its latency property NOW says.
                // Recorded, never compared against what we asked for here: the
                // decision to re-send belongs to the one place that also knows
                // what the current measurement is (`devdecl::should_send`).
                //
                // The INPUT direction can only appear if some future change
                // starts declaring the microphone; storing it in `decl_out`
                // would then quietly make the speaker's status page describe the
                // microphone. Dropped with a log instead.
                if at.input {
                    dlog!(
                        "[audiohubd] hal: slot {} reported a MICROPHONE latency ({frames}f); \
                         nothing declares that direction, so this is a bug on one side or the other",
                        at.slot
                    );
                    continue;
                }
                let mut st = lk(&inner.haldev);
                let Some(rec) = st.slots.get_mut(at.slot as usize) else {
                    continue;
                };
                if rec.generation != generation {
                    continue;
                }
                rec.decl_out.acked = Some(frames);
                rec.decl_out.pending = pending;
            }
            HalControlEvent::Volume {
                at,
                generation,
                scalar,
                muted,
            } => {
                let endpoint = if at.input {
                    DeviceVolumeEndpoint::DefaultInput
                } else {
                    DeviceVolumeEndpoint::DefaultOutput
                };
                latest.insert((at.slot, endpoint), (generation, scalar, muted));
            }
        }
    }
    for ((slot, endpoint), (generation, event_scalar, event_muted)) in latest {
        #[cfg(windows)]
        let (scalar, muted) = {
            let fingerprint = {
                let st = lk(&inner.haldev);
                let Some(rec) = st.slots.get(slot as usize) else {
                    continue;
                };
                if rec.generation != generation || rec.fingerprint.is_empty() {
                    continue;
                }
                rec.fingerprint.clone()
            };
            // KSPROPERTY_AUDIO_VOLUMELEVEL is dB, while the public endpoint
            // scalar follows an undocumented Windows audio-taper curve.  The
            // driver event is therefore a change SIGNAL only; reading the
            // exact MMDevice is the only authoritative scalar/mute value.
            match volume::get_audiohub_peer_endpoint_volume(
                &fingerprint,
                endpoint == DeviceVolumeEndpoint::DefaultInput,
            ) {
                Ok(state) => (state.scalar, state.muted),
                Err(err) => {
                    dlog!(
                        "[audiohubd] hal: cannot read peer {} {} after a Windows volume event: \
                         {err:#}",
                        fingerprint,
                        endpoint.as_wire()
                    );
                    continue;
                }
            }
        };
        #[cfg(not(windows))]
        let (scalar, muted) = (event_scalar, event_muted);
        #[cfg(windows)]
        let _ = (event_scalar, event_muted);
        let Some(modern) =
            queue_slot_device_volume(inner, slot, generation, endpoint, scalar, muted)
        else {
            continue;
        };
        // Mixed-version compatibility: a legacy peer can still carry its
        // historical active-speaker control on a media session. Idle output
        // and every microphone case remain pending until a v1 control channel
        // exists; they are never silently claimed as synchronized.
        if endpoint == DeviceVolumeEndpoint::DefaultOutput
            && !modern
            && relay_volume_to_peer(inner, slot, scalar, muted)
        {
            clear_matching_device_volume_pending(inner, slot, endpoint, scalar, Some(muted));
        }
    }
}

/// Apply the driver's exact publication mask and forget IO intent for any
/// endpoint it actually withdrew. A later capability restore is a new device
/// availability event and must wait for a fresh StartIO rather than inheriting
/// the selection/linger state of an endpoint that ceased to exist.
///
/// Returns whether the microphone direction was withdrawn, so the caller can
/// reset the lock-free mic-IO mirror alongside the slot record.
fn apply_published_directions(rec: &mut SlotRec, published: u8) -> bool {
    let withdrawn = rec.published_directions & !published;
    rec.published_directions = published;
    if withdrawn & HAL_PUBLISH_OUT != 0 {
        rec.io_out = false;
        rec.io_out_off_since = None;
    }
    let mic_withdrawn = withdrawn & HAL_PUBLISH_IN != 0;
    if mic_withdrawn {
        rec.io_in = false;
        rec.io_in_off_since = None;
    }
    mic_withdrawn
}

/// Volume values this close are the same value: the driver stores a float the
/// user dragged, the peer's device snaps to its own step grid, and neither is
/// allowed to look like a change and start another round trip.
pub(crate) const HAL_VOL_EPS: f32 = 1.0 / 512.0;

/// Re-send an unacknowledged endpoint write on a healthy control channel.
/// This is deliberately longer than the 200 ms coordinator tick so a normal
/// round trip remains one frame while a lost response cannot strand the UI.
const DEVICE_VOLUME_RETRY: Duration = Duration::from_secs(1);

fn vol_same(a: (f32, bool), b: (f32, bool)) -> bool {
    (a.0 - b.0).abs() < HAL_VOL_EPS && a.1 == b.1
}

/// Forward direction (spec-m5b §5.5): the local user moved slot N's virtual
/// speaker, so the peer that owns slot N — and NOBODY else — must follow.
fn relay_volume_to_peer(inner: &Arc<DaemonInner>, slot: u8, scalar: f32, muted: bool) -> bool {
    let fp = {
        let mut st = lk(&inner.haldev);
        let Some(rec) = st.slots.get_mut(slot as usize) else {
            return false;
        };
        if rec.fingerprint.is_empty() {
            return false;
        }
        // Our own notify_volume coming back around: applying it would send the
        // peer what the peer just told us.
        if rec.vol_echo.map_or(false, |l| vol_same(l, (scalar, muted))) {
            return false;
        }
        rec.vol_echo = Some((scalar, muted));
        rec.legacy_last_delivered = None;
        rec.fingerprint.clone()
    };
    // ONLY this peer's sessions.
    let targets: Vec<u32> = lk(&inner.state)
        .sessions
        .values()
        .filter(|e| carries_volume_for(&e.conn.fp, &e.kind, &e.dir, e.volume.enabled, &fp))
        .map(|e| e.id)
        .collect();
    if targets.is_empty() {
        dlog!(
            "[audiohubd] hal: slot {slot} speaker volume {scalar:.3} muted={muted} held for \
             {fp}: no volume_sync'd spk session to carry it yet"
        );
        return false;
    }
    let mut sent = false;
    for id in targets {
        match conn::set_session_volume(inner, id, scalar, Some(muted)) {
            Ok(()) => sent = true,
            Err(e) => dlog!("[audiohubd] hal: volume {scalar:.3} -> session {id}: {e:#}"),
        }
    }
    sent
}

/// Can this session carry a volume change for the peer that owns a slot?
///
/// Extracted so the peer filter can be tested on its own, because it is exactly
/// the clause the previous implementation did not have: it selected every
/// volume_sync'd spk session this side drove, full stop. With one fixed device
/// pair that was right by accident; with one pair per peer it means dragging
/// peer A's virtual speaker also moves peer B's real machine — at 2am, from a
/// slider labelled with somebody else's computer name.
fn carries_volume_for(
    session_fp: &str,
    kind: &str,
    dir: &str,
    volume_enabled: bool,
    want_fp: &str,
) -> bool {
    session_fp == want_fp && kind == KIND_SPK && dir == crate::DIR_SEND && volume_enabled
}

/// Reverse direction (spec-m5b §5.5): each peer's real output reported a new
/// state, so THAT peer's virtual speaker control must show it.
fn push_peer_volumes(inner: &Arc<DaemonInner>, hal: &halbridge::HalBridge) {
    // Snapshot first: every other reader of a session's volume cell takes it
    // with the state lock already released, and this one must not be the
    // exception that introduces a lock order.
    let sessions = crate::snapshot_sessions(inner);
    let conns: Vec<Arc<crate::ConnShared>> = lk(&inner.state)
        .conns
        .values()
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    let modern: HashSet<String> = conns
        .into_iter()
        .filter(|conn| lk(&conn.peer_audio_capabilities).device_volume_version() >= 1)
        .map(|conn| conn.fp.clone())
        .collect();
    let planned_at = Instant::now();
    let mut pending: Vec<(u8, String, HalEndpoint, u32, f32, bool)> = Vec::new();
    {
        let mut st = lk(&inner.haldev);
        for slot in 0..HAL_MAX_SLOTS {
            let (fp, generation) = {
                let rec = &st.slots[slot];
                if rec.fingerprint.is_empty() || rec.state != Some(HalSlotState::Bound) {
                    continue;
                }
                (rec.fingerprint.clone(), rec.generation)
            };
            if modern.contains(&fp) {
                continue;
            }
            let state = sessions
                .iter()
                .find(|e| carries_volume_for(&e.conn.fp, &e.kind, &e.dir, e.volume.enabled, &fp))
                .and_then(|e| *lk(&e.volume.state));
            let Some(v) = state else { continue };
            if !v.scalar.is_finite() {
                continue;
            }
            let now = (v.scalar.clamp(0.0, 1.0), v.muted);
            let rec = &mut st.slots[slot];
            if rec
                .legacy_last_delivered
                .is_some_and(|delivered| vol_same(delivered, now))
            {
                continue;
            }
            if rec
                .legacy_notify_retry
                .is_some_and(|retry| vol_same(retry.value, now) && planned_at < retry.not_before)
            {
                continue;
            }
            pending.push((
                slot as u8,
                fp,
                HalEndpoint::out(slot as u8),
                generation,
                now.0,
                now.1,
            ));
        }
    }
    // Outside the lock: a mach send can sit for its full 500ms timeout.
    for (slot, fingerprint, at, generation, scalar, muted) in pending {
        #[cfg(windows)]
        let _ = (at, generation);
        #[cfg(windows)]
        let (delivered, driver_echo) =
            match volume::set_audiohub_peer_endpoint_volume(&fingerprint, false, scalar, muted) {
                Ok(state) => (true, Some((state.scalar, state.muted))),
                Err(err) => {
                    dlog!(
                        "[audiohubd] hal: cannot apply legacy peer {} output scalar/mute through \
                     its exact Windows endpoint: {err:#}",
                        fingerprint
                    );
                    (false, None)
                }
            };
        #[cfg(not(windows))]
        let (delivered, driver_echo) = (
            hal.notify_volume(at, generation, scalar, muted),
            Some((scalar, muted)),
        );
        let mut st = lk(&inner.haldev);
        let Some(rec) = st.slots.get_mut(slot as usize) else {
            continue;
        };
        if rec.fingerprint != fingerprint
            || rec.generation != generation
            || rec.state != Some(HalSlotState::Bound)
        {
            continue;
        }
        if delivered {
            rec.legacy_last_delivered = Some((scalar, muted));
            rec.vol_echo = driver_echo;
            rec.legacy_notify_retry = None;
        } else {
            rec.legacy_notify_retry = Some(DeviceVolumeNotifyRetry {
                value: (scalar, muted),
                not_before: Instant::now() + DEVICE_VOLUME_RETRY,
            });
        }
    }
}

// ---------------------------------------------------------------- sessions

/// Declarative session coordination (spec-m5b §5.6). The ONLY switch that opens
/// or closes a mode-B session: `CTL_IO_STATE` says an application started using
/// a virtual device, and a session appears behind it.
fn coordinate_sessions(inner: &Arc<DaemonInner>, tx: &mpsc::Sender<SessCmd>) {
    let mode_b = effective_mode(inner) == Mode::B;
    let (live, connection_contracts): (HashSet<u32>, HashMap<String, bool>) = {
        let state = lk(&inner.state);
        let live = state.sessions.keys().copied().collect();
        let contracts = state
            .conns
            .values()
            .filter(|conn| conn.alive.load(Ordering::Acquire))
            .map(|conn| {
                let ready = conn.registration_ready.load(Ordering::Acquire)
                    && lk(&conn.peer_mode).mode() == Some(Mode::Share)
                    && !matches!(
                        *lk(&conn.peer_audio_capabilities),
                        crate::PeerAudioCapabilitiesCell::Unheard
                    );
                (conn.fp.clone(), ready)
            })
            .collect();
        (live, contracts)
    };
    let now = Instant::now();
    let mut cmds = Vec::new();
    let mut st = lk(&inner.haldev);
    for slot in 0..HAL_MAX_SLOTS {
        // A session that went away with its connection is not ours to remember.
        // This is what makes an intent survive a peer restart: the record drops
        // the dead id, `want` is still true, and the next pass re-opens it.
        for id in [st.slots[slot].sess_out, st.slots[slot].sess_in] {
            if let Some(id) = id {
                if !live.contains(&id) {
                    let rec = &mut st.slots[slot];
                    if rec.sess_out == Some(id) {
                        rec.sess_out = None;
                    }
                    if rec.sess_in == Some(id) {
                        rec.sess_in = None;
                    }
                }
            }
        }
        let rec = &st.slots[slot];
        if rec.fingerprint.is_empty() {
            continue;
        }
        let lingering =
            |off: Option<Instant>, d: Duration| off.map_or(false, |t| now.duration_since(t) < d);
        // A retired or delisted slot does not linger: the device is on its way
        // out of the system and holding somebody's microphone open for it would
        // be exactly backwards.
        let alive = rec.state == Some(HalSlotState::Bound);
        let format_ready = inner.hal().is_none_or(|hal| !hal.supports_output_formats() || hal.output_format(slot as u8).is_some());
        let want_out = alive && format_ready
            && rec.published_directions & HAL_PUBLISH_OUT != 0
            && (rec.io_out || lingering(rec.io_out_off_since, LINGER_OUT));
        let want_in = alive
            && rec.published_directions & HAL_PUBLISH_IN != 0
            && (rec.io_in || lingering(rec.io_in_off_since, LINGER_IN));
        let fp = rec.fingerprint.clone();

        for (want, have, opening, kind, out) in [
            (want_out, rec.sess_out, st.opening_out[slot], KIND_SPK, true),
            (want_in, rec.sess_in, st.opening_in[slot], KIND_MIC, false),
        ] {
            if want && have.is_none() && !opening {
                if !mode_b {
                    // Mode A must never be hijacked by a stray device
                    // selection: in mode A these devices should not exist at
                    // all, and if one lingers, it stays silent.
                    continue;
                }
                // No connection means the worker should dial it. An already
                // published but half-registered connection must instead wait:
                // opening now would freeze its provisional UDP path/mono
                // capability before tier negotiation and advertisements land.
                if connection_contracts.get(&fp) == Some(&false) {
                    continue;
                }
                if st.open_at[slot].map_or(false, |t| now.duration_since(t) < OPEN_COOLDOWN) {
                    continue;
                }
                st.open_at[slot] = Some(now);
                if out {
                    st.opening_out[slot] = true;
                } else {
                    st.opening_in[slot] = true;
                }
                cmds.push(SessCmd::Open {
                    slot: slot as u8,
                    fingerprint: fp.clone(),
                    kind,
                });
            } else if !want {
                if let Some(id) = have {
                    cmds.push(SessCmd::Close {
                        slot: slot as u8,
                        out,
                        id,
                    });
                }
            }
        }
    }
    drop(st);
    for c in cmds {
        let _ = tx.send(c);
    }
}

/// Runs the blocking half of session coordination. `open_session` dials the
/// peer synchronously; on the coordinator's tick one unreachable peer would
/// stall the device reconcile and the volume relay for the whole connect
/// timeout.
pub(crate) fn session_worker(inner: Arc<DaemonInner>, rx: mpsc::Receiver<SessCmd>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // recv_timeout, not recv: a clone of the sender lives in
        // `inner.hal_sess` for the life of the daemon, so the channel never
        // closes and a blocking recv would hold this thread — and every
        // `DaemonHandle::wait()` behind it — open forever.
        let cmd = match rx.recv_timeout(TICK) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match cmd {
            SessCmd::Open {
                slot,
                fingerprint,
                kind,
            } => {
                let out = kind == KIND_SPK;
                let params = OpenSessionParams {
                    peer: fingerprint.clone(),
                    kind: kind.to_string(),
                    // The speaker direction sends what an app played INTO this
                    // slot's virtual speaker; the microphone direction receives
                    // the peer's capture and writes it into this slot's mic ring.
                    source: out.then(|| SOURCE_HAL_SPEAKER.to_string()),
                    freq: None,
                    backend: None,
                    monitor: false,
                    verify_freq: None,
                    simulate_loss_pct: None,
                    volume_sync: out,
                    bridge: None,
                    hal: !out,
                    override_mode: true, // this IS the mode-B path
                };
                let res = conn::open_session_from(
                    &inner,
                    &params,
                    SessionOrigin::Hal { slot },
                    conn::OpenCause::Fresh,
                    if out { inner.hal().and_then(|hal| hal.output_format(slot)).and_then(|(layout, _)| crate::halformat::spatial_layout(layout)) } else { None },
                );
                let mut st = lk(&inner.haldev);
                if out {
                    st.opening_out[slot as usize] = false;
                } else {
                    st.opening_in[slot as usize] = false;
                }
                match res {
                    Ok(info) => {
                        // The slot may have been retired or re-bound while the
                        // connect was in flight; if so this session belongs to
                        // nobody and has to go.
                        let stale = st.slots[slot as usize].fingerprint != fingerprint;
                        if stale {
                            drop(st);
                            let _ = conn::close_session(&inner, info.id);
                            continue;
                        }
                        let rec = &mut st.slots[slot as usize];
                        if out {
                            rec.sess_out = Some(info.id);
                        } else {
                            rec.sess_in = Some(info.id);
                        }
                        dlog!(
                            "[audiohubd] hal: slot {slot} {} session {} opened for {fingerprint}",
                            if out { "speaker" } else { "microphone" },
                            info.id
                        );
                    }
                    Err(e) => {
                        // Not a failure to report anywhere: an offline peer is
                        // the ordinary case. `open_session` has armed the
                        // reconnect on its way through `connect_peer`, the
                        // intent stays pending in `io_out`/`io_in`, and the
                        // next pass past the cooldown tries again — which is
                        // what makes "play into a sleeping peer's speaker, then
                        // wake it up" work with no IPC call at all.
                        dlog!(
                            "[audiohubd] hal: slot {slot} {} session for {fingerprint} not open \
                             yet ({e:#}); intent held",
                            if out { "speaker" } else { "microphone" }
                        );
                    }
                }
            }
            SessCmd::Close { slot, out, id } => {
                {
                    let mut st = lk(&inner.haldev);
                    let rec = &mut st.slots[slot as usize];
                    if out {
                        rec.sess_out = None;
                    } else {
                        rec.sess_in = None;
                    }
                }
                if let Err(e) = conn::close_session(&inner, id) {
                    dlog!("[audiohubd] hal: closing session {id}: {e:#}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------- loop

fn coordinate_output_formats(inner: &Arc<DaemonInner>, hal: &halbridge::HalBridge) {
    if !hal.supports_output_formats() { return; }
    let records: Vec<_> = {
        let state = lk(&inner.haldev);
        state.slots.iter().enumerate().filter(|(_, rec)| rec.state == Some(HalSlotState::Bound) && rec.published_directions & HAL_PUBLISH_OUT != 0)
            .map(|(slot, rec)| (slot as u8, rec.generation, rec.fingerprint.clone())).collect()
    };
    for (slot, generation, fingerprint) in records {
        let conn = lk(&inner.state).conns.get(&fingerprint).cloned();
        let mut mask = crate::halformat::STEREO_MASK;
        let quality = lk(&inner.peer_transport).get(&fingerprint).send.quality_target();
        if !matches!(quality, audiohub_ipc::QualityTarget::Fixed(rung) if rung != 0) {
            if let Some(conn) = conn.filter(|conn| conn.alive.load(Ordering::Acquire)) {
                if let Some(offer) = lk(&conn.peer_spatial_output).as_ref() {
                    for contract in &offer.contracts { mask |= 1 << crate::halformat::layout_id(contract.layout); }
                }
            }
        }
        let desired = hal.output_format(slot).map(|(layout, _)| layout).filter(|layout| mask & (1 << layout) != 0).unwrap_or(0);
        hal.offer_output_format(slot, generation, mask, desired);
        let Some(lease) = hal.take_format_prepare(slot) else { continue };
        let worker_lease = lease.clone();
        let owner = Arc::clone(inner);
        let spawned = std::thread::Builder::new().name("ahb-hal-format".into()).spawn(move || {
            let mut acknowledged = false;
            if worker_lease.is_current() {
                for entry in crate::snapshot_sessions(&owner) {
                    if entry.conn.fp == fingerprint && entry.replay.as_ref().is_some_and(|params| params.source.as_deref() == Some(SOURCE_HAL_SPEAKER)) {
                        if let Some(tx) = &entry.tx { tx.fail_media("virtual speaker format is changing".into()); }
                    }
                }
                let (reply, done) = mpsc::channel();
                if lk(&owner.tx_cmds).send(crate::engine::TxCmd::QuiesceHal { lease: worker_lease.clone(), ack: reply }).is_ok()
                    && done.recv_timeout(Duration::from_secs(1)) == Ok(true)
                {
                    for _ in 0..3 {
                        if owner.shutdown.load(Ordering::Acquire) || !worker_lease.is_current() { break; }
                        if owner.hal().is_some_and(|hal| hal.format_quiesced(worker_lease.message)) { acknowledged = true; break; }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            if !acknowledged { if let Some(hal) = owner.hal() { hal.format_failed(&worker_lease); } }
            worker_lease.finish();
        });
        if spawned.is_err() { lease.finish(); }
    }
}

/// 200ms: fast enough that a device selection feels immediate, slow enough that
/// the CoreAudio enumeration behind it (once a second) is free.
///
/// The peer store IS re-read on every one of these, deliberately: it is one
/// small file read, and it is what makes a pairing done by another process —
/// the CLI writes `paired_peers.json` directly — turn into a pair of devices
/// within 200ms instead of within the spec's 1s fallback. The expensive half
/// (enumerating every CoreAudio device) is the part gated to 1Hz.
const TICK: Duration = Duration::from_millis(200);

pub(crate) fn coordinator_loop(inner: Arc<DaemonInner>, tx: mpsc::Sender<SessCmd>) {
    let mut next_observe = Instant::now();
    let mut observed: Option<HashSet<String>> = None;
    while !inner.shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(TICK);
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let Some(hal) = inner.hal() else { continue };
        apply_events(&inner, &hal);
        let now = Instant::now();
        if now >= next_observe {
            next_observe = now + OBSERVE_EVERY;
            observed = observe();
        }
        reconcile(&inner, &hal, observed.as_ref());
        coordinate_output_formats(&inner, &hal);
        // Order matters, and it is the same one the old single-pair tick used:
        // the driver's own change is dispatched (and recorded as "the control
        // already reads this") BEFORE the peer's state is pushed back, so a
        // slider move never bounces off its own round trip.
        flush_pending_device_volumes(&inner);
        push_peer_device_volumes(&inner, &hal);
        push_peer_volumes(&inner, &hal);
        coordinate_sessions(&inner, &tx);
    }
}

/// Everything a peer's devices need after it stops being a peer: close its
/// sessions now (no linger — the device is leaving), retire the slot, and drop
/// the assignment. The ORDER is not interchangeable: releasing the slot first
/// would let a new pairing take it while an old session was still writing into
/// that slot's ring.
pub(crate) fn release_peer(inner: &Arc<DaemonInner>, fingerprint: &str) {
    let ids: Vec<u32> = {
        let mut st = lk(&inner.haldev);
        let Some(slot) = st.table.slot_of(fingerprint) else {
            return;
        };
        let rec = &mut st.slots[slot as usize];
        let ids: Vec<u32> = [rec.sess_out.take(), rec.sess_in.take()]
            .into_iter()
            .flatten()
            .collect();
        rec.io_out = false;
        rec.io_in = false;
        ids
    };
    for id in ids {
        let _ = conn::close_session(inner, id);
    }
    // The reconcile does the Clear: `fingerprint` is gone from the store, so
    // the slot is no longer wanted and the very next pass retires it with the
    // right generation. Dropping the assignment here as well would lose the
    // record the Clear is aimed at.
    lk(&inner.haldev).table.release(fingerprint);
    save_table(inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(fp: &str, name: &str, added: u64) -> NameInput {
        NameInput {
            fingerprint: fp.to_string(),
            base: name.to_string(),
            added_unix: added,
        }
    }

    fn want(slot: u8, fp: &str, name: &str) -> DesiredDevice {
        let (out_name, in_name) = device_names(name, false, "zh-CN");
        DesiredDevice {
            slot,
            fingerprint: fp.to_string(),
            out_uid: uid_out(fp),
            in_uid: uid_in(fp),
            out_name,
            in_name,
            display: name.to_string(),
            directions: HAL_PUBLISH_BOTH,
            online: true,
        }
    }

    /// A slot as it looks once its device is genuinely published.
    fn published(d: &DesiredDevice, generation: u32) -> SlotRec {
        SlotRec {
            fingerprint: d.fingerprint.clone(),
            out_uid: d.out_uid.clone(),
            in_uid: d.in_uid.clone(),
            sent_out_name: d.out_name.clone(),
            sent_in_name: d.in_name.clone(),
            sent_directions: d.directions,
            sent_online: d.online,
            sent: true,
            acked: true,
            published_directions: d.directions,
            generation,
            state: Some(HalSlotState::Bound),
            observed_directions: d.directions,
            observed: true,
            ..SlotRec::default()
        }
    }

    fn seen(ds: &[&DesiredDevice]) -> HashSet<String> {
        let mut s: HashSet<String> = ds
            .iter()
            .flat_map(|d| {
                [
                    (d.directions & HAL_PUBLISH_OUT != 0).then(|| d.out_uid.clone()),
                    (d.directions & HAL_PUBLISH_IN != 0).then(|| d.in_uid.clone()),
                ]
                .into_iter()
                .flatten()
            })
            .collect();
        // a real Mac always has some device of its own
        s.insert("BuiltInSpeakerDevice".to_string());
        s
    }

    fn empty_slots() -> Vec<SlotRec> {
        vec![SlotRec::default(); HAL_MAX_SLOTS]
    }

    // ------------------------------------------------------------- naming

    #[test]
    fn a_duplicate_name_renames_the_later_peer_not_the_incumbent() {
        let names = display_names(&[
            peer("bbbb", "MacBook Pro", 100),
            peer("aaaa", "MacBook Pro", 50),
            peer("cccc", "Mac mini", 10),
        ]);
        // The incumbent keeps its name. Renaming it because somebody else
        // paired a second identical laptop would relabel a device an app has
        // already selected, for a reason invisible to the person looking at it.
        assert_eq!(names["aaaa"], "MacBook Pro");
        assert_eq!(names["bbbb"], "MacBook Pro (2)");
        assert_eq!(names["cccc"], "Mac mini");
    }

    #[test]
    fn ties_on_added_unix_break_on_fingerprint_so_both_ends_agree() {
        let a = display_names(&[peer("ffff", "Mac", 7), peer("0000", "Mac", 7)]);
        let b = display_names(&[peer("0000", "Mac", 7), peer("ffff", "Mac", 7)]);
        assert_eq!(a, b, "the order of the input must not decide the names");
        assert_eq!(a["0000"], "Mac");
        assert_eq!(a["ffff"], "Mac (2)");
    }

    #[test]
    fn device_names_are_the_frozen_shape() {
        let (out, mic) = device_names("客厅 Mac", false, "zh-CN");
        assert_eq!(out, "AudioHub – 客厅 Mac");
        assert_eq!(mic, "AudioHub – 客厅 Mac");
        let (out, _) = device_names("客厅 Mac", true, "zh-CN");
        assert_eq!(out, "AudioHub – 客厅 Mac（离线）");
        let (out, _) = device_names("Living Room Mac", true, "en-US");
        assert_eq!(out, "AudioHub – Living Room Mac (Offline)");
        // ...and a name that would not fit the driver's char[128] is cut on a
        // character boundary, because invalid UTF-8 makes the driver reject the
        // whole Bind — losing the device, not just the tail of its name.
        let (out, _) = device_names(&"漢".repeat(200), false, "zh-CN");
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        let (out, _) = device_names(&"漢".repeat(200), true, "en-US");
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(
            out.ends_with(" (Offline)"),
            "offline marker was truncated: {out}"
        );
    }

    #[test]
    fn an_alias_replaces_the_computer_name_and_still_disambiguates() {
        let mut p = PairedPeer {
            name: "MacBook Pro".into(),
            fingerprint: "aaaa".into(),
            public_key_b64: String::new(),
            last_addr: None,
            port: 1,
            added_unix: 1,
            alias: None,
        };
        assert_eq!(base_name(&p), "MacBook Pro");
        p.alias = Some("  书房  ".into());
        assert_eq!(
            base_name(&p),
            "书房",
            "an alias is trimmed, not taken literally"
        );
        p.alias = Some("   ".into());
        assert_eq!(
            base_name(&p),
            "MacBook Pro",
            "a blank alias is not an alias"
        );
    }

    // ------------------------------------------------------- slot table

    #[test]
    fn the_slot_table_round_trips_and_keeps_assignments_stable() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ahb-slots-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut t = SlotTable::load(&dir);
        assert_eq!(t.assign("aaaa", 16), Some(0));
        assert_eq!(t.assign("bbbb", 16), Some(1));
        assert_eq!(t.assign("aaaa", 16), Some(0), "assignment is idempotent");
        assert!(t.set_directions("aaaa", HAL_PUBLISH_OUT));
        assert!(t.set_directions("bbbb", HAL_PUBLISH_IN));
        t.save(&dir).expect("save");

        // What a restart sees. If this ever came back different, every device
        // would be republished on every restart and the user's default output
        // would be thrown away with it.
        let t2 = SlotTable::load(&dir);
        assert_eq!(t2.slot_of("aaaa"), Some(0));
        assert_eq!(t2.slot_of("bbbb"), Some(1));
        assert_eq!(t2.directions_of("aaaa"), Some(HAL_PUBLISH_OUT));
        assert_eq!(t2.directions_of("bbbb"), Some(HAL_PUBLISH_IN));
        assert_eq!(t2.used(), 2);

        // A released slot is the LOWEST free one again, not the next one up.
        let mut t3 = t2.clone();
        assert_eq!(t3.release("aaaa"), Some(0));
        assert_eq!(t3.assign("cccc", 16), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hal_used_counts_live_device_records_not_persisted_assignments() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign("peer-a", 16), Some(0));
        let mut state = HalDevState::new(table);

        assert_eq!(state.table.used(), 1, "the stable assignment is retained");
        assert_eq!(
            state.device_count(),
            0,
            "a retained assignment without a live record is not a bound device"
        );

        state.slots[0].fingerprint = "peer-a".to_string();
        assert_eq!(state.device_count(), 1);

        state.slots[0] = SlotRec::default();
        assert_eq!(state.table.used(), 1, "retirement keeps endpoint identity");
        assert_eq!(state.device_count(), 0, "retirement frees live capacity");
        assert_eq!(
            state.table.assign("peer-a", 16),
            Some(0),
            "switching back to mode B must reuse the original slot"
        );
    }

    #[test]
    fn a_v1_slot_file_migrates_to_both_directions_until_the_peer_reconnects() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ahb-slots-v1-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            SlotTable::path(&dir),
            br#"{"version":1,"slots":[{"slot":4,"fingerprint":"old-peer"}]}"#,
        )
        .expect("write legacy file");

        let table = SlotTable::load(&dir);
        assert_eq!(table.slot_of("old-peer"), Some(4));
        assert_eq!(
            table.directions_of("old-peer"),
            Some(HAL_PUBLISH_BOTH),
            "an upgrade must not make an offline incumbent's selected device disappear"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_and_explicitly_empty_capabilities_remain_distinct_on_disk() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ahb-slots-v3-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut table = SlotTable::new();
        assert_eq!(table.assign("unknown-peer", 16), Some(0));
        assert_eq!(table.assign("empty-peer", 16), Some(1));
        assert!(table.set_directions("empty-peer", 0));
        table.save(&dir).expect("save");

        let json = std::fs::read_to_string(SlotTable::path(&dir)).expect("read");
        assert!(json.contains("\"version\": 3"));
        let restored = SlotTable::load(&dir);
        assert_eq!(restored.slot_of("unknown-peer"), Some(0));
        assert_eq!(restored.directions_of("unknown-peer"), None);
        assert_eq!(restored.directions_of("empty-peer"), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The persistence trigger. A pass that hands a peer a slot MUST write the
    /// table back; the version of this that only wrote on removals looked
    /// perfectly healthy until the first restart, at which point every device
    /// was rebuilt and the user's chosen default output was gone.
    #[test]
    fn a_new_assignment_is_detected_as_a_change_to_persist() {
        let before = SlotTable::new();
        let mut after = before.clone();
        assert!(before.same_as(&after), "an untouched pass writes nothing");
        after.assign("aaaa", 16);
        assert!(
            !before.same_as(&after),
            "a new assignment has to reach the disk, or the next start re-assigns \
             it, the Bind stops being idempotent, and the default output is lost"
        );
        // ...and a pass that only re-reads an existing assignment does not.
        let mut again = after.clone();
        assert_eq!(again.assign("aaaa", 16), Some(0));
        assert!(after.same_as(&again));
    }

    #[test]
    fn capacity_is_a_named_refusal_not_a_reassignment() {
        let mut t = SlotTable::new();
        assert_eq!(t.assign("aaaa", 2), Some(0));
        assert_eq!(t.assign("bbbb", 2), Some(1));
        assert_eq!(
            t.assign("cccc", 2),
            None,
            "the third peer gets no slot at all"
        );
        // ...and the two that fit are untouched by the refusal.
        assert_eq!(t.slot_of("aaaa"), Some(0));
        assert_eq!(t.slot_of("bbbb"), Some(1));
    }

    #[test]
    fn a_peer_that_stopped_being_paired_frees_its_slot() {
        let mut t = SlotTable::new();
        t.assign("aaaa", 16);
        t.assign("bbbb", 16);
        assert!(t.retain(&HashSet::from(["aaaa".to_string()])));
        assert_eq!(t.slot_of("bbbb"), None);
        assert_eq!(t.slot_of("aaaa"), Some(0));
    }

    // --------------------------------------------------------- planning

    #[test]
    fn a_new_pairing_binds_its_slot_once() {
        let d = want(0, "aaaa", "Mac mini");
        let acts = plan_binds(&[d.clone()], &empty_slots(), Some(&seen(&[])));
        assert_eq!(acts, vec![BindAction::Set(d.to_bind())]);
    }

    /// THE restart invariant. A daemon that comes back to a driver still
    /// holding its bindings must send ONE idempotent Set per slot — never a
    /// Clear first, and never nothing at all.
    #[test]
    fn a_restart_re_states_bindings_and_never_clears_them() {
        let d = want(3, "aaaa", "Mac mini");
        // Exactly the state a fresh process is in: the slot table came back
        // from disk, so the ASSIGNMENT is known, but nothing has been
        // acknowledged and the devices are still published by the driver.
        let mut slots = empty_slots();
        slots[3] = SlotRec {
            fingerprint: d.fingerprint.clone(),
            out_uid: d.out_uid.clone(),
            in_uid: d.in_uid.clone(),
            ..SlotRec::default()
        };
        let acts = plan_binds(&[d.clone()], &slots, Some(&seen(&[&d])));

        assert!(
            !acts.iter().any(|a| matches!(a, BindAction::Clear { .. })),
            "a restart must not Clear anything — the devices are already \
             published and Clear-then-Set destroys the user's default output \
             selection, silently, once per restart: {acts:?}"
        );
        assert_eq!(
            acts,
            vec![BindAction::Set(d.to_bind())],
            "and it must re-Set every slot it still intends: the driver replays \
             a slot's IO state and volume ONLY on an idempotent Set, so skipping \
             it leaves an app that was mid-recording recording silence"
        );
    }

    #[test]
    fn a_published_and_acknowledged_slot_is_left_alone() {
        let d = want(0, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[0] = published(&d, 7);
        assert!(
            plan_binds(&[d.clone()], &slots, Some(&seen(&[&d]))).is_empty(),
            "a steady state must produce no wire traffic at all"
        );
    }

    #[test]
    fn one_direction_is_a_complete_binding_and_an_extra_endpoint_is_repaired() {
        let mut output_only = want(0, "aaaa", "Output-only host");
        output_only.directions = HAL_PUBLISH_OUT;
        let mut slots = empty_slots();
        slots[0] = published(&output_only, 7);

        assert!(
            plan_binds(&[output_only.clone()], &slots, Some(&seen(&[&output_only]))).is_empty(),
            "the absent microphone is the intended steady state, not half a failed pair"
        );

        let mut stale_extra = seen(&[&output_only]);
        stale_extra.insert(output_only.in_uid.clone());
        assert_eq!(
            plan_binds(&[output_only.clone()], &slots, Some(&stale_extra)),
            vec![BindAction::Set(output_only.to_bind())],
            "a driver that left the unrequested microphone visible must be corrected"
        );
    }

    #[test]
    fn changing_capability_is_an_in_place_set_not_a_clear() {
        let both = want(2, "aaaa", "Laptop");
        let mut output_only = both.clone();
        output_only.directions = HAL_PUBLISH_OUT;
        let mut slots = empty_slots();
        slots[2] = published(&both, 11);

        let acts = plan_binds(&[output_only.clone()], &slots, Some(&seen(&[&both])));
        assert_eq!(acts, vec![BindAction::Set(output_only.to_bind())]);
        assert!(
            !acts.iter().any(|a| matches!(a, BindAction::Clear { .. })),
            "removing one direction must preserve the surviving endpoint identity"
        );
    }

    #[test]
    fn a_peer_with_no_endpoints_has_a_valid_invisible_binding() {
        let mut none = want(5, "aaaa", "Headless host");
        none.directions = 0;
        let mut slots = empty_slots();
        slots[5] = published(&none, 3);
        let observed = seen(&[&none]);

        assert!(plan_binds(&[none.clone()], &slots, Some(&observed)).is_empty());
        let mut state = HalDevState::new(SlotTable::new());
        state.slots = slots;
        // `peer_device` is keyed through the persistent slot table, so install
        // the same stable assignment before asking for its IPC projection.
        state.table.assign[5] = Some("aaaa".to_string());
        let device = state
            .peer_device("aaaa")
            .expect("the slot itself stays assigned");
        assert_eq!(device.requested_directions, Some(0));
        assert_eq!(device.published_directions, Some(0));
    }

    #[test]
    fn withdrawing_then_restoring_a_direction_does_not_reuse_old_io_intent() {
        let mut rec = SlotRec {
            published_directions: HAL_PUBLISH_BOTH,
            io_out: true,
            io_in: true,
            io_out_off_since: Some(Instant::now()),
            io_in_off_since: Some(Instant::now()),
            ..SlotRec::default()
        };
        assert!(apply_published_directions(&mut rec, HAL_PUBLISH_OUT));
        assert!(
            rec.io_out,
            "the surviving endpoint keeps its current IO state"
        );
        assert!(!rec.io_in, "the withdrawn endpoint loses its old selection");
        assert!(
            rec.io_in_off_since.is_none(),
            "its linger must be cancelled too"
        );

        assert!(!apply_published_directions(&mut rec, HAL_PUBLISH_BOTH));
        assert!(
            !rec.io_in && rec.io_in_off_since.is_none(),
            "republishing is not a fresh StartIO and must not reopen a session"
        );
    }

    /// The closed loop. An acknowledged binding whose device the system does
    /// NOT list is a lost notification, an Initialize race or a coreaudiod
    /// restart — all of which look identical from here, and all of which are
    /// repaired by re-stating the binding.
    #[test]
    fn an_acknowledged_binding_the_system_does_not_publish_is_re_sent() {
        let d = want(0, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[0] = published(&d, 7);
        let acts = plan_binds(&[d.clone()], &slots, Some(&seen(&[])));
        assert_eq!(acts, vec![BindAction::Set(d.to_bind())]);
    }

    #[test]
    fn an_unreadable_device_list_does_not_trigger_a_re_set_storm() {
        let d = want(0, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[0] = published(&d, 7);
        assert!(
            plan_binds(&[d], &slots, None).is_empty(),
            "'I cannot see the device list' must not read as 'nothing is published'"
        );
    }

    #[test]
    fn a_rename_is_a_set_on_the_same_slot_and_the_same_uids() {
        let old = want(0, "aaaa", "Mac mini");
        let new = want(0, "aaaa", "书房");
        let mut slots = empty_slots();
        slots[0] = published(&old, 7);
        let acts = plan_binds(&[new.clone()], &slots, Some(&seen(&[&old])));
        assert_eq!(acts, vec![BindAction::Set(new.to_bind())]);
        let BindAction::Set(req) = &acts[0] else {
            panic!()
        };
        assert_eq!(req.out_uid, old.out_uid, "a rename must not move the UID");
        assert!(!acts.iter().any(|a| matches!(a, BindAction::Clear { .. })));
    }

    #[test]
    fn going_offline_restates_the_binding_rather_than_removing_it() {
        let online = want(0, "aaaa", "Mac mini");
        let mut offline = online.clone();
        offline.online = false;
        let (o, i) = device_names("Mac mini", true, "zh-CN");
        offline.out_name = o;
        offline.in_name = i;
        let mut slots = empty_slots();
        slots[0] = published(&online, 7);
        let acts = plan_binds(&[offline.clone()], &slots, Some(&seen(&[&online])));
        assert_eq!(acts, vec![BindAction::Set(offline.to_bind())]);
    }

    #[test]
    fn changing_native_locale_is_an_in_place_rename_and_reconnect_removes_the_mark() {
        let online = want(0, "aaaa", "Mac mini");
        let mut zh = online.clone();
        zh.online = false;
        (zh.out_name, zh.in_name) = device_names("Mac mini", true, "zh-CN");
        zh.display = device_display_name("Mac mini", true, "zh-CN");
        let mut slots = empty_slots();
        slots[0] = published(&zh, 7);

        let mut en = zh.clone();
        (en.out_name, en.in_name) = device_names("Mac mini", true, "en-US");
        en.display = device_display_name("Mac mini", true, "en-US");
        let acts = plan_binds(&[en.clone()], &slots, Some(&seen(&[&zh])));
        assert_eq!(acts, vec![BindAction::Set(en.to_bind())]);
        let BindAction::Set(req) = &acts[0] else {
            panic!()
        };
        assert_eq!(
            req.out_uid, zh.out_uid,
            "language changes must preserve the device UID"
        );
        assert_eq!(
            req.display, "Mac mini (Offline)",
            "Windows receives the localized suffix"
        );

        slots[0] = published(&en, 7);
        let acts = plan_binds(&[online.clone()], &slots, Some(&seen(&[&en])));
        assert_eq!(acts, vec![BindAction::Set(online.to_bind())]);
        assert_eq!(
            online.out_uid, en.out_uid,
            "reconnect must restore the same selected device"
        );
    }

    #[test]
    fn unpairing_retires_the_slot_at_its_current_generation() {
        let d = want(2, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[2] = published(&d, 9);
        let acts = plan_binds(&[], &slots, Some(&seen(&[&d])));
        assert_eq!(
            acts,
            vec![BindAction::Clear {
                slot: 2,
                generation: 9
            }]
        );
    }

    #[test]
    fn a_clear_already_in_flight_is_not_repeated() {
        let d = want(2, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[2] = SlotRec {
            clearing: true,
            ..published(&d, 9)
        };
        assert!(plan_binds(&[], &slots, Some(&seen(&[&d]))).is_empty());
        // ...and once the driver says Free, there is nothing left to do either.
        slots[2] = SlotRec {
            state: Some(HalSlotState::Free),
            ..SlotRec::default()
        };
        assert!(plan_binds(&[], &slots, Some(&seen(&[]))).is_empty());
    }

    /// Two peers, and only the unpaired one goes. The regression that matters
    /// here is R7: unpairing P1 must not disturb P2's devices.
    #[test]
    fn retiring_one_peer_leaves_the_others_bound() {
        let a = want(0, "aaaa", "Mac mini");
        let b = want(1, "bbbb", "MacBook");
        let mut slots = empty_slots();
        slots[0] = published(&a, 3);
        slots[1] = published(&b, 4);
        let acts = plan_binds(&[b.clone()], &slots, Some(&seen(&[&a, &b])));
        assert_eq!(
            acts,
            vec![BindAction::Clear {
                slot: 0,
                generation: 3
            }]
        );
    }

    /// A device we did not ask for, published under our own UID prefix: the
    /// slot table was lost, or another daemon bound it. We cannot know which
    /// slot it is, so every untracked slot is asked — a Clear with the wrong
    /// generation is ignored, and the BindState it provokes carries the truth.
    #[test]
    fn an_orphan_device_makes_the_daemon_ask_the_slots_it_does_not_know() {
        let ghost = want(0, "dead", "Ghost");
        let acts = plan_binds(&[], &empty_slots(), Some(&seen(&[&ghost])));
        assert_eq!(acts.len(), HAL_MAX_SLOTS);
        assert!(acts
            .iter()
            .all(|a| matches!(a, BindAction::Clear { generation: 0, .. })));
        // ...and with no orphan, an untouched pool produces no traffic.
        assert!(plan_binds(&[], &empty_slots(), Some(&seen(&[]))).is_empty());
    }

    // ------------------------------------------------------------- mode

    /// A volume change belongs to ONE peer: the one whose virtual speaker was
    /// dragged. This is regression N5 as a unit test.
    #[test]
    fn a_volume_change_reaches_only_the_peer_whose_device_moved() {
        let a = "aaaa";
        let b = "bbbb";
        assert!(
            carries_volume_for(a, KIND_SPK, crate::DIR_SEND, true, a),
            "the owner's own volume_sync'd spk session must carry it"
        );
        assert!(
            !carries_volume_for(b, KIND_SPK, crate::DIR_SEND, true, a),
            "another peer's session must NOT: with one device pair per peer, a \
             fan-out with no peer filter moves a second machine's real volume \
             from a slider bearing the first one's name"
        );
        // ...and the pre-existing gates are still gates.
        assert!(!carries_volume_for(a, KIND_MIC, crate::DIR_SEND, true, a));
        assert!(!carries_volume_for(a, KIND_SPK, crate::DIR_RECV, true, a));
        assert!(!carries_volume_for(a, KIND_SPK, crate::DIR_SEND, false, a));
    }

    #[test]
    fn replacement_connection_contract_overrides_a_stale_cap1_hint() {
        assert_eq!(
            classify_peer_device_volume_protocol(true, Some(Mode::Share), 1),
            PeerDeviceVolumeProtocol::Modern
        );
        // This is the connection cell after replacement, and is the value the
        // slot-locked runtime lookup must use. A cap1 value cached before the
        // replacement is deliberately not an input to either queue function.
        assert_eq!(
            classify_peer_device_volume_protocol(true, Some(Mode::Share), 0),
            PeerDeviceVolumeProtocol::Legacy
        );
        assert_eq!(
            classify_peer_device_volume_protocol(true, None, 1),
            PeerDeviceVolumeProtocol::PeerNotSharing
        );
        assert_eq!(
            classify_peer_device_volume_protocol(false, Some(Mode::Share), 1),
            PeerDeviceVolumeProtocol::Offline
        );

        // The classifier is only safe if every mutation path samples it after
        // acquiring the slot lock. Keep that ordering structural: accepting a
        // pre-lock `modern` argument is exactly the cap1 -> cap0 resurrection
        // window this regression covers.
        let source = include_str!("haldev.rs");
        for (start, end) in [
            (
                "pub(crate) fn queue_peer_device_volume(",
                "fn queue_slot_device_volume(",
            ),
            (
                "fn queue_slot_device_volume(",
                "fn clear_matching_device_volume_pending(",
            ),
            (
                "pub(crate) fn accept_peer_device_volume(",
                "pub(crate) fn peer_software_gain_authority(",
            ),
            (
                "pub(crate) fn clear_peer_device_volume_protocol(",
                "fn push_peer_device_volumes(",
            ),
        ] {
            let body = source
                .split_once(start)
                .and_then(|(_, rest)| rest.split_once(end).map(|(body, _)| body))
                .expect("volume queue function body");
            let locked = body.find("lk(&inner.haldev);").expect("slot HAL lock");
            let resolved = body
                .find("with_current_peer_device_volume_protocol(")
                .expect("current connection protocol lookup");
            assert!(
                locked < resolved,
                "{start} sampled the connection contract before taking the slot lock"
            );
            assert!(
                !body[..locked].contains("modern: bool"),
                "{start} accepted a stale pre-lock protocol hint"
            );
        }
    }

    #[test]
    fn idle_device_volume_intent_survives_until_matching_readback() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        assert!(relay.queue_local(0.5, Some(false)));
        let pending = relay.due_for(11, t0).expect("idle intent must be retained");
        assert_eq!(pending.request_id, 1);
        assert_eq!(pending.scalar, 0.5);
        assert_eq!(pending.muted, Some(false));

        relay.mark_sent(pending.request_id, 11, t0);
        assert!(
            relay.due_for(11, t0 + DEVICE_VOLUME_RETRY / 2).is_none(),
            "one live channel must not receive a frame every coordinator tick"
        );
        assert_eq!(
            relay
                .due_for(11, t0 + DEVICE_VOLUME_RETRY)
                .map(|p| p.request_id),
            Some(pending.request_id),
            "a lost acknowledgement must cause a bounded retry"
        );
        assert_eq!(
            relay.due_for(12, t0).map(|p| p.request_id),
            Some(pending.request_id),
            "a replacement connection must replay unacknowledged intent"
        );

        let stale = VolumeState {
            scalar: 0.9,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(!relay.accept_remote(11, 1, None, stale, false));
        assert!(
            relay.plan_notify(t0).is_none(),
            "snapshot must not beat pending intent"
        );
        assert!(!relay.accept_remote(11, 2, Some(99), stale, false));
        assert!(
            relay.pending.is_some(),
            "a stale ack must not clear the request"
        );

        let readback = VolumeState {
            scalar: 0.48,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(11, 3, Some(pending.request_id), readback, false,));
        assert!(relay.pending.is_none());
        assert_eq!(relay.plan_notify(t0), Some((0.48, false)));
        relay.record_notify_result((0.48, false), Some((0.48, false)), true, t0);
        assert!(
            relay.plan_notify(t0).is_none(),
            "equal readback is notified once"
        );

        assert!(
            !relay.queue_local(0.48, Some(false)),
            "the daemon's own HAL notification must not become a network write"
        );
        assert!(relay.queue_local(0.6, Some(false)));
        assert_eq!(relay.pending.map(|p| p.request_id), Some(2));
    }

    #[test]
    fn failed_hal_notification_retries_without_claiming_delivery() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let state = VolumeState {
            scalar: 0.42,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, state, false));
        let value = relay.plan_notify(t0).expect("first delivery is due");
        relay.record_notify_result(value, None, false, t0);
        assert!(relay.last_delivered.is_none());
        assert!(relay.driver_echo.is_none());
        assert!(
            relay.plan_notify(t0 + DEVICE_VOLUME_RETRY / 2).is_none(),
            "a missing driver must not receive five attempts per second"
        );
        assert_eq!(
            relay.plan_notify(t0 + DEVICE_VOLUME_RETRY),
            Some(value),
            "the same authoritative state must become due again"
        );
        relay.record_notify_result(value, Some(value), true, t0 + DEVICE_VOLUME_RETRY);
        assert!(relay.plan_notify(t0 + DEVICE_VOLUME_RETRY * 2).is_none());
    }

    #[test]
    fn a_new_authoritative_value_bypasses_an_older_failed_retry() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let first = VolumeState {
            scalar: 0.2,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, first, false));
        let attempted = relay.plan_notify(t0).unwrap();
        relay.record_notify_result(attempted, None, false, t0);

        let second = VolumeState {
            scalar: 0.8,
            muted: true,
            ..first
        };
        assert!(relay.accept_remote(7, 2, None, second, false));
        assert_eq!(
            relay.plan_notify(t0 + Duration::from_millis(1)),
            Some((0.8, true)),
            "backoff belongs to the failed value, not the endpoint"
        );
    }

    #[test]
    fn delivery_commit_does_not_hide_a_readback_that_changed_during_the_send() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let first = VolumeState {
            scalar: 0.3,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, first, false));
        let attempted = relay.plan_notify(t0).unwrap();
        let second = VolumeState {
            scalar: 0.7,
            muted: true,
            ..first
        };
        assert!(relay.accept_remote(7, 2, None, second, false));
        relay.record_notify_result(attempted, Some(attempted), true, t0);
        assert_eq!(relay.plan_notify(t0), Some((0.7, true)));
    }

    #[test]
    fn a_quantized_windows_readback_suppresses_its_echo_without_rearming_delivery() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let remote = VolumeState {
            scalar: 0.5,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, remote, false));
        let requested = relay.plan_notify(t0).unwrap();

        // Windows may expose a nearby scalar after its private audio-taper
        // curve and the driver's dB step are applied.  Delivery belongs to the
        // peer request; the one-shot event guard belongs to the readback.
        let applied = (0.497, true);
        relay.record_notify_result(requested, Some(applied), true, t0);
        assert!(relay.plan_notify(t0).is_none());
        assert!(!relay.queue_local(applied.0, Some(applied.1)));
        assert!(
            !relay.queue_local(applied.0, Some(applied.1)),
            "a second channel/node event reports the same final COM state"
        );
        assert!(relay.pending.is_none());

        assert!(relay.queue_local(0.65, Some(true)));
        assert!(
            relay.pending.is_some(),
            "a later real user move must survive"
        );
    }

    #[test]
    fn duplicate_fixed_output_events_do_not_turn_a_reflected_mute_into_peer_intent() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let fixed = VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            // This is the dangerous case: a duplicate endpoint event would
            // otherwise queue the explicit mute back to the peer even though
            // the fixed scalar itself is local software-gain authority.
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, fixed, true));
        let reflected = relay.plan_notify(t0).expect("initial virtual state");
        relay.record_notify_result(reflected, Some(reflected), true, t0);

        assert!(!relay.queue_software_gain(reflected.0, Some(reflected.1), true));
        assert!(!relay.queue_software_gain(reflected.0, Some(reflected.1), true));
        assert!(
            relay.pending.is_none(),
            "duplicate channel/node events must not become a peer mute write"
        );
    }

    #[test]
    fn driver_reconnect_replays_readback_without_losing_pending_intent() {
        let mut relay = DeviceVolumeRelay::default();
        let t0 = Instant::now();
        let state = VolumeState {
            scalar: 0.55,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, state, false));
        let value = relay.plan_notify(t0).unwrap();
        relay.record_notify_result(value, Some(value), true, t0);
        relay.invalidate_notification();
        assert_eq!(relay.plan_notify(t0), Some(value));

        assert!(relay.queue_intent(0.9, Some(true)));
        let request = relay.pending.unwrap().request_id;
        relay.invalidate_notification();
        assert_eq!(relay.pending.unwrap().request_id, request);
        assert!(relay.plan_notify(t0).is_none());
    }

    #[test]
    fn microphone_and_speaker_device_volume_state_never_share_an_echo_or_ack() {
        let mut rec = SlotRec::default();
        assert!(rec.out_volume.queue_local(0.25, Some(false)));
        assert!(rec.in_volume.queue_local(0.75, Some(true)));
        let out_request = rec.out_volume.pending.unwrap().request_id;
        let in_request = rec.in_volume.pending.unwrap().request_id;

        let input_readback = VolumeState {
            scalar: 0.75,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(rec
            .in_volume
            .accept_remote(11, 1, Some(in_request), input_readback, false));
        assert!(rec.in_volume.pending.is_none());
        assert_eq!(
            rec.out_volume.pending.map(|p| p.request_id),
            Some(out_request),
            "input acknowledgement must not touch output pending state"
        );
        let now = Instant::now();
        assert_eq!(rec.in_volume.plan_notify(now), Some((0.75, true)));
        assert!(rec.out_volume.plan_notify(now).is_none());
    }

    #[test]
    fn scalar_only_tail_preserves_an_unacknowledged_explicit_mute() {
        let mut relay = DeviceVolumeRelay::default();
        assert!(relay.queue_intent(0.4, Some(true)));
        assert!(relay.queue_intent(0.6, None));
        let pending = relay.pending.expect("coalesced intent must remain queued");
        assert_eq!(pending.scalar, 0.6);
        assert_eq!(pending.muted, Some(true));
    }

    #[test]
    fn endpoint_revisions_reject_reordered_snapshots_but_reset_on_reconnect() {
        let mut relay = DeviceVolumeRelay::default();
        let newer = VolumeState {
            scalar: 0.7,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        let older = VolumeState {
            scalar: 0.2,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(41, 2, None, newer, false));
        assert!(!relay.accept_remote(41, 1, None, older, false));
        assert_eq!(relay.remote, Some(newer));
        assert!(relay.accept_remote(42, 1, None, older, false));
        assert_eq!(relay.remote, Some(older));
    }

    #[test]
    fn reordered_matching_ack_retires_intent_without_overwriting_newer_state() {
        let mut relay = DeviceVolumeRelay::default();
        assert!(relay.queue_intent(0.6, Some(true)));
        let request = relay.pending.unwrap().request_id;
        let newer = VolumeState {
            scalar: 0.7,
            muted: false,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(!relay.accept_remote(41, 2, None, newer, false));
        assert!(relay.pending.is_some());

        let older_ack = VolumeState {
            scalar: 0.58,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(41, 1, Some(request), older_ack, false));
        assert!(relay.pending.is_none());
        assert_eq!(relay.remote, Some(newer));
        assert_eq!(relay.remote_revision, 2);
        assert_eq!(relay.plan_notify(Instant::now()), Some((0.7, false)));
    }

    #[test]
    fn a_fixed_output_converts_pending_device_intent_to_persistent_gain() {
        let mut relay = DeviceVolumeRelay::default();
        assert!(relay.queue_intent(0.3, Some(true)));
        let fixed = VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            mute_adjustable: false,
        };
        assert!(relay.accept_remote(7, 1, None, fixed, true));
        assert!(relay.pending.is_none());
        assert_eq!(
            relay.displayed_state(),
            Some(VolumeState {
                scalar: 0.3,
                muted: true,
                adjustable: false,
                mute_adjustable: false,
            })
        );
        assert!(relay.uses_software_gain());
    }

    #[test]
    fn fixed_scalar_keeps_independently_adjustable_mute_on_the_peer() {
        let mut relay = DeviceVolumeRelay::default();
        let fixed = VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, fixed, true));
        assert!(relay.queue_software_gain(0.4, Some(true), false));
        assert_eq!(relay.pending.and_then(|pending| pending.muted), Some(true));
        assert!(relay.queue_software_gain(0.6, None, false));
        let pending = relay.pending.expect("slider tail must retain peer mute");
        assert_eq!(pending.scalar, 0.6);
        assert_eq!(pending.muted, Some(true));

        assert!(relay.accept_remote(7, 2, None, fixed, true));
        assert_eq!(
            relay.pending.map(|pending| pending.request_id),
            Some(pending.request_id)
        );
        assert_eq!(relay.displayed_state().unwrap().muted, true);
        assert_eq!(relay.displayed_state().unwrap().scalar, 0.6);

        let readback = fixed; // matching ACK may truthfully report refusal
        assert!(relay.accept_remote(7, 3, Some(pending.request_id), readback, true,));
        assert!(relay.pending.is_none());
        assert_eq!(relay.displayed_state().unwrap().muted, false);
        assert_eq!(relay.displayed_state().unwrap().scalar, 0.6);
    }

    #[test]
    fn reordered_fixed_output_ack_follows_the_newer_physical_mute() {
        let mut relay = DeviceVolumeRelay::default();
        let fixed = VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            mute_adjustable: true,
        };
        assert!(relay.accept_remote(7, 1, None, fixed, true));
        assert!(relay.queue_software_gain(0.6, Some(true), false));
        let request = relay.pending.unwrap().request_id;

        assert!(relay.accept_remote(7, 3, None, fixed, true));
        assert!(relay.pending.is_some());
        assert_eq!(relay.displayed_state().unwrap().muted, true);

        let older_ack = VolumeState {
            muted: true,
            ..fixed
        };
        assert!(relay.accept_remote(7, 2, Some(request), older_ack, true));
        assert!(relay.pending.is_none());
        assert_eq!(relay.remote, Some(fixed));
        assert_eq!(relay.remote_revision, 3);
        assert_eq!(relay.displayed_state().unwrap().scalar, 0.6);
        assert_eq!(relay.displayed_state().unwrap().muted, false);

        // When the fixed endpoint cannot change mute independently, mute is
        // part of the local software gain and periodic physical readback must
        // not erase it.
        let fixed_local_mute = VolumeState {
            mute_adjustable: false,
            ..fixed
        };
        let mut local = DeviceVolumeRelay::default();
        assert!(local.accept_remote(9, 1, None, fixed_local_mute, true));
        assert!(local.queue_software_gain(0.4, Some(true), false));
        assert!(local.pending.is_none());
        assert!(local.accept_remote(9, 2, None, fixed_local_mute, true));
        assert_eq!(local.displayed_state().unwrap().scalar, 0.4);
        assert!(local.displayed_state().unwrap().muted);
    }

    #[test]
    fn mode_b_refuses_a_bare_ui_session_open() {
        // The structural guarantee that mode B has not become mode A with new
        // labels: in mode B the SYSTEM's device selection opens sessions, so a
        // UI that could open one by peer would have reintroduced exactly the
        // peer picker mode B exists to remove (plan §7.1).
        let refusal = refuse_using_others(Mode::B, false).expect("mode B must refuse");
        assert!(refusal.contains("mode B"), "{refusal}");
        // CLI and probes must still be able to drive the daemon directly.
        assert!(refuse_using_others(Mode::B, true).is_none());
        // ...and mode A is untouched: every existing session flow keeps working.
        assert!(refuse_using_others(Mode::A, false).is_none());
    }

    /// plan §13: share mode does not use other machines, so the outbound half
    /// is refused too — and with a DIFFERENT message from mode B's, because the
    /// user's next action differs (pick another mode vs. go select a device).
    #[test]
    fn share_mode_refuses_to_use_other_machines() {
        let refusal = refuse_using_others(Mode::Share, false).expect("share mode must refuse");
        assert!(refusal.contains("share mode"), "{refusal}");
        assert_ne!(
            refusal,
            refuse_using_others(Mode::B, false).unwrap(),
            "share and mode B refuse for unrelated reasons; one message for both would send \
             half the users to look for a device selection that does not apply to them"
        );
        assert!(
            refuse_using_others(Mode::Share, true).is_none(),
            "probes still drive us"
        );
    }

    /// The enforcement half of plan §13, and the one that actually prevents the
    /// relay. Exactly one mode may be used by others.
    #[test]
    fn only_share_mode_lets_a_peer_open_a_stream_on_us() {
        assert!(
            refuse_being_used(Mode::Share).is_none(),
            "share mode is the whole point: it must serve"
        );
        for m in [Mode::A, Mode::B] {
            let why = refuse_being_used(m)
                .unwrap_or_else(|| panic!("{m} is a consumer mode and must refuse to be used"));
            assert!(
                why.contains(m.as_str()),
                "the refusal must name the mode: {why}"
            );
        }
    }

    /// There is no override on the inbound guard, and there must not be: the
    /// override flag is about driving one's OWN daemon. This is a signature
    /// assertion — it goes red if somebody gives `refuse_being_used` an escape
    /// hatch, which is the shape the relay would come back in.
    #[test]
    fn the_inbound_guard_takes_nothing_but_our_own_mode() {
        // Deliberately written as a call with exactly one argument. If a second
        // parameter is added this stops compiling, which is the point.
        let f: fn(Mode) -> Option<String> = refuse_being_used;
        assert!(f(Mode::Share).is_none());
    }

    /// plan §13 推论 3: virtual devices are desired ONLY in mode B.
    ///
    /// This is the branch `compute_desired` takes for every paired peer, so a
    /// `Some` here is what makes the reconcile diff delete that peer's devices.
    /// If share mode ever returned `None`, a machine that had just stopped
    /// being a consumer would keep publishing consumer devices.
    #[test]
    fn only_mode_b_desires_virtual_devices() {
        assert_eq!(
            no_device_reason(Mode::B),
            None,
            "mode B is where devices live"
        );
        let share = no_device_reason(Mode::Share).expect("share mode must not desire devices");
        let a = no_device_reason(Mode::A).expect("mode A must not desire devices");
        assert_ne!(
            share, a,
            "a shared 'not mode B' reason would compile, reconcile correctly, and then label \
             every card on a share-mode machine with mode A's explanation"
        );
    }

    /// Every reason this daemon can emit has to be a reason the frontend can
    /// put into words, and the two live in different languages with no compiler
    /// between them.
    ///
    /// Read out of the frontend source, in the style of `audiohub-ipc`'s
    /// `the_three_ipc_version_declarations_agree` — and for the same reason:
    /// `cargo test`, `tsc --noEmit` and `npm run build` are all perfectly green
    /// while a `hal_reason` the UI has never heard of falls through to a
    /// generic "暂无虚拟设备（mode_share）". Missing the file is a panic, never
    /// a skip: a guard that goes quiet when its subject is renamed is not a
    /// guard.
    #[test]
    fn reasons_the_frontend_can_explain() {
        const TS: &str = "app/frontend/src/state/mode.ts";
        let path = format!("{}/../../{TS}", env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "读不到 {TS}（{e}）。文件被改名/挪走了就把这条测试一起更新，\
                 不要让它退化成一条恒真断言"
            )
        });
        // Every mode-derived reason, plus the ones the reconcile emits for
        // non-mode causes. All of them reach `halReasonText`.
        let emitted = [
            no_device_reason(Mode::Share).unwrap(),
            no_device_reason(Mode::A).unwrap(),
            "no_driver",
            "removed_while_offline",
            "capacity",
        ];
        for reason in emitted {
            assert!(
                src.contains(&format!("case '{reason}':")),
                "{TS} has no branch for hal_reason '{reason}': the card would fall through to \
                 the generic text and show the user a machine-readable token"
            );
        }
    }
}
