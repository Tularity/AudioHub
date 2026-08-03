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

use audiohub_ipc::{
    HalDeviceInfo, Mode, OpenSessionParams, PeerHalDevice, KIND_MIC, KIND_SPK, SOURCE_HAL_SPEAKER,
};
use audiohub_net::identity::{PairedPeer, PeerStore};

use crate::halbridge::{self, HalBindRequest, HalControlEvent, HalEndpoint, HalSlotState};
use crate::{conn, dlog, lk, DaemonInner, SessionOrigin};

pub const HAL_MAX_SLOTS: usize = halbridge::HAL_MAX_SLOTS;

/// Every virtual device's UID starts with this. It is what the closed-loop
/// enumeration matches on, what regression scripts discover devices by, and —
/// because it embeds the FINGERPRINT rather than the host name — what survives
/// the peer renaming its computer.
pub const UID_PREFIX: &str = "AudioHub:";

/// U+2013 EN DASH, as spec-m5b §3.5 spells it. Not a hyphen.
const NAME_PREFIX: &str = "AudioHub – ";
const NAME_OUT: &str = " 扬声器";
const NAME_IN: &str = " 麦克风";
const NAME_OFFLINE: &str = "（离线）";

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
/// The single spelling of the prefix, and both platforms go through it: macOS
/// via [`device_names`], which appends " 扬声器" / " 麦克风" here, and Windows
/// via `halbridge_win::wire::encode_bind_request`, which sends this and lets
/// the driver append the direction word it read from the INF.
///
/// It is a function rather than a `pub const` so the two callers cannot end up
/// composing it two slightly different ways — which is exactly what happened:
/// the Windows path sent the bare peer name and every endpoint came out
/// labelled `WIN-IR01HVEFU7G`, with no AudioHub anywhere in it.
pub fn device_name_stem(display: &str) -> String {
    format!("{NAME_PREFIX}{display}")
}

/// The two device names for one peer. The daemon builds the whole string
/// because the driver runs inside coreaudiod's sandbox: it can read neither the
/// computer name nor any localisation, and putting half the naming logic there
/// would mean two places to disambiguate (spec-m5b §3.5).
pub fn device_names(display: &str, offline: bool) -> (String, String) {
    let mark = if offline { NAME_OFFLINE } else { "" };
    let stem = device_name_stem(display);
    (
        clamp_utf8(&format!("{stem}{NAME_OUT}{mark}"), MAX_NAME_BYTES),
        clamp_utf8(&format!("{stem}{NAME_IN}{mark}"), MAX_NAME_BYTES),
    )
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
}

impl SlotTable {
    pub fn new() -> SlotTable {
        SlotTable { assign: vec![None; HAL_MAX_SLOTS] }
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
        for e in file.slots {
            let s = e.slot as usize;
            if s < HAL_MAX_SLOTS && !e.fingerprint.is_empty() && t.assign[s].is_none() {
                t.assign[s] = Some(e.fingerprint);
            }
        }
        t
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let file = SlotFile {
            version: 1,
            slots: self
                .assign
                .iter()
                .enumerate()
                .filter_map(|(s, fp)| {
                    fp.as_ref().map(|fp| SlotEntry { slot: s as u8, fingerprint: fp.clone() })
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
        Some(free as u8)
    }

    pub fn release(&mut self, fingerprint: &str) -> Option<u8> {
        let s = self.slot_of(fingerprint)?;
        self.assign[s as usize] = None;
        Some(s)
    }

    /// Drops assignments for peers that are no longer paired at all. Called on
    /// every pass, so a peer unpaired by another process (the CLI writes
    /// `paired_peers.json` directly) still frees its slot.
    pub fn retain(&mut self, paired: &HashSet<String>) -> bool {
        let mut changed = false;
        for slot in self.assign.iter_mut() {
            if let Some(fp) = slot.as_ref() {
                if !paired.contains(fp.as_str()) {
                    *slot = None;
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
        self.assign == other.assign
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
    /// The disambiguated display name WITHOUT a direction suffix — what
    /// `display_names` produced, before `device_names` appended " 扬声器" /
    /// " 麦克风". macOS ignores it; Windows uses only it, because there the
    /// system composes the endpoint name as "<pin name> (<this>)".
    ///
    /// Carried as a plain extra field rather than behind a platform
    /// conditional: this file has none at all, and keeping it that way is worth
    /// more than saving one `String` per slot. `halwire_win.rs` asserts the
    /// count stays zero.
    pub display: String,
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
            online: self.online,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindAction {
    Set(HalBindRequest),
    Clear { slot: u8, generation: u32 },
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
    /// The system's device list really contains both UIDs.
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
}

impl SlotRec {
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
        let identity = rec.fingerprint == d.fingerprint
            && rec.out_uid == d.out_uid
            && rec.in_uid == d.in_uid;
        // "Published" needs BOTH halves: the driver acknowledged the binding
        // AND the system really lists it. Either one alone has a failure mode
        // that is completely silent — an ack without publication is the
        // Initialize race, publication without an ack is a slot we would
        // happily hand to somebody else.
        let published = identity
            && rec.sent
            && rec.acked
            && rec.state == Some(HalSlotState::Bound)
            && observed.map_or(true, |o| o.contains(&d.out_uid) && o.contains(&d.in_uid));
        if !published {
            // The idempotent upsert. This is also the ONLY thing sent after a
            // daemon restart — never a Clear first, which would take the user's
            // default output with it (spec-m5b §1).
            actions.push(BindAction::Set(d.to_bind()));
            continue;
        }
        if rec.sent_out_name != d.out_name
            || rec.sent_in_name != d.in_name
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
            actions.push(BindAction::Clear { slot: s as u8, generation: rec.generation });
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

    /// The published bitmask the tx loop drains idle speakers from.
    fn published_mask(&self) -> u16 {
        let mut m = 0u16;
        for (s, rec) in self.slots.iter().enumerate() {
            if rec.state == Some(HalSlotState::Bound) && !rec.fingerprint.is_empty() {
                m |= 1 << s;
            }
        }
        m
    }

    /// `hal.devices` for `daemon.status`.
    pub(crate) fn device_infos(&self, counters: &[halbridge::HalSlotCounters]) -> Vec<HalDeviceInfo> {
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
        })
    }
}

/// Work the coordinator must not do on its own tick: `open_session` connects
/// synchronously (TCP + verify + secure handshake), and an offline peer would
/// otherwise stall the device reconcile and the volume relay behind it.
pub(crate) enum SessCmd {
    Open { slot: u8, fingerprint: String, kind: &'static str },
    Close { slot: u8, out: bool, id: u32 },
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
///     control. An app selects "AudioHub – X 扬声器" and the daemon opens the
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
    let (remove_offline, mark_offline) = {
        let s = lk(&inner.settings);
        (s.remove_virtual_on_disconnect, s.mark_offline_devices)
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
        let name = display.get(fp).cloned().unwrap_or_else(|| fp.clone());
        let (out_name, in_name) = device_names(&name, mark_offline && !online);
        desired.push(DesiredDevice {
            slot,
            fingerprint: fp.clone(),
            out_uid: uid_out(fp),
            in_uid: uid_in(fp),
            out_name,
            in_name,
            display: name,
            online,
        });
    }
    PassInputs { desired, display, reasons, paired }
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
    let PassInputs { desired, display, reasons, paired } =
        compute_desired(inner, capacity, &mut st.table);
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
        rec.observed = observed.map_or(rec.observed, |o| {
            o.contains(&d.out_uid) && o.contains(&d.in_uid)
        });
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
                    || st.slots[s].sent_in_name != req.in_name;
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
    let mut latest: HashMap<u8, (f32, bool)> = HashMap::new();
    for ev in events {
        match ev {
            HalControlEvent::Attached { session_id, slot_count } => {
                dlog!("[audiohubd] hal: attached, session {session_id}, {slot_count} slots");
            }
            HalControlEvent::Detached => {
                let mut st = lk(&inner.haldev);
                for rec in st.slots.iter_mut() {
                    rec.acked = false;
                    rec.state = None;
                    rec.observed = false;
                    rec.io_out = false;
                    rec.io_in = false;
                }
                for s in 0..HAL_MAX_SLOTS {
                    inner.hal_mic_io[s].store(true, Ordering::Relaxed);
                }
            }
            HalControlEvent::BindState { slot, generation, state } => {
                let mut st = lk(&inner.haldev);
                let Some(rec) = st.slots.get_mut(slot as usize) else { continue };
                rec.generation = generation;
                rec.state = Some(state);
                match state {
                    HalSlotState::Bound => rec.acked = true,
                    HalSlotState::Free => {
                        // The slot is genuinely retired now, so it may be
                        // handed to another peer. Everything about the previous
                        // tenant goes with it — a stale vol_echo would suppress
                        // the first volume the NEXT peer should have received.
                        *rec = SlotRec { generation, state: Some(state), ..SlotRec::default() };
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
            HalControlEvent::IoState { at, running, .. } => {
                let mut st = lk(&inner.haldev);
                let Some(rec) = st.slots.get_mut(at.slot as usize) else { continue };
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
            HalControlEvent::Volume { at, scalar, muted, .. } => {
                if at.input {
                    // The virtual microphone's own slider. The capture gain
                    // belongs to the peer (plan §7.2) and driving it from here
                    // would be reaching into somebody else's machine.
                    dlog!(
                        "[audiohubd] hal: slot {} microphone volume {scalar:.3} muted={muted} \
                         ignored (the peer owns its capture gain)",
                        at.slot
                    );
                    continue;
                }
                latest.insert(at.slot, (scalar, muted));
            }
        }
    }
    for (slot, (scalar, muted)) in latest {
        relay_volume_to_peer(inner, slot, scalar, muted);
    }
}

/// Volume values this close are the same value: the driver stores a float the
/// user dragged, the peer's device snaps to its own step grid, and neither is
/// allowed to look like a change and start another round trip.
pub(crate) const HAL_VOL_EPS: f32 = 1.0 / 512.0;

fn vol_same(a: (f32, bool), b: (f32, bool)) -> bool {
    (a.0 - b.0).abs() < HAL_VOL_EPS && a.1 == b.1
}

/// Forward direction (spec-m5b §5.5): the local user moved slot N's virtual
/// speaker, so the peer that owns slot N — and NOBODY else — must follow.
fn relay_volume_to_peer(inner: &Arc<DaemonInner>, slot: u8, scalar: f32, muted: bool) {
    let fp = {
        let mut st = lk(&inner.haldev);
        let Some(rec) = st.slots.get_mut(slot as usize) else { return };
        if rec.fingerprint.is_empty() {
            return;
        }
        // Our own notify_volume coming back around: applying it would send the
        // peer what the peer just told us.
        if rec.vol_echo.map_or(false, |l| vol_same(l, (scalar, muted))) {
            return;
        }
        rec.vol_echo = Some((scalar, muted));
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
        return;
    }
    for id in targets {
        if let Err(e) = conn::set_session_volume(inner, id, scalar, Some(muted)) {
            dlog!("[audiohubd] hal: volume {scalar:.3} -> session {id}: {e:#}");
        }
    }
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
    let mut pending: Vec<(HalEndpoint, u32, f32, bool)> = Vec::new();
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
            if rec.vol_echo.map_or(false, |l| vol_same(l, now)) {
                continue;
            }
            rec.vol_echo = Some(now);
            pending.push((HalEndpoint::out(slot as u8), generation, now.0, now.1));
        }
    }
    // Outside the lock: a mach send can sit for its full 500ms timeout.
    for (at, generation, scalar, muted) in pending {
        hal.notify_volume(at, generation, scalar, muted);
    }
}

// ---------------------------------------------------------------- sessions

/// Declarative session coordination (spec-m5b §5.6). The ONLY switch that opens
/// or closes a mode-B session: `CTL_IO_STATE` says an application started using
/// a virtual device, and a session appears behind it.
fn coordinate_sessions(inner: &Arc<DaemonInner>, tx: &mpsc::Sender<SessCmd>) {
    let mode_b = effective_mode(inner) == Mode::B;
    let live: HashSet<u32> = lk(&inner.state).sessions.keys().copied().collect();
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
        let lingering = |off: Option<Instant>, d: Duration| {
            off.map_or(false, |t| now.duration_since(t) < d)
        };
        // A retired or delisted slot does not linger: the device is on its way
        // out of the system and holding somebody's microphone open for it would
        // be exactly backwards.
        let alive = rec.state == Some(HalSlotState::Bound);
        let want_out = alive && (rec.io_out || lingering(rec.io_out_off_since, LINGER_OUT));
        let want_in = alive && (rec.io_in || lingering(rec.io_in_off_since, LINGER_IN));
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
                if st.open_at[slot].map_or(false, |t| now.duration_since(t) < OPEN_COOLDOWN) {
                    continue;
                }
                st.open_at[slot] = Some(now);
                if out {
                    st.opening_out[slot] = true;
                } else {
                    st.opening_in[slot] = true;
                }
                cmds.push(SessCmd::Open { slot: slot as u8, fingerprint: fp.clone(), kind });
            } else if !want {
                if let Some(id) = have {
                    cmds.push(SessCmd::Close { slot: slot as u8, out, id });
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
            SessCmd::Open { slot, fingerprint, kind } => {
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
                let res = conn::open_session_from(&inner, &params, SessionOrigin::Hal { slot });
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
        // Order matters, and it is the same one the old single-pair tick used:
        // the driver's own change is dispatched (and recorded as "the control
        // already reads this") BEFORE the peer's state is pushed back, so a
        // slider move never bounces off its own round trip.
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
        let Some(slot) = st.table.slot_of(fingerprint) else { return };
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
        NameInput { fingerprint: fp.to_string(), base: name.to_string(), added_unix: added }
    }

    fn want(slot: u8, fp: &str, name: &str) -> DesiredDevice {
        let (out_name, in_name) = device_names(name, false);
        DesiredDevice {
            slot,
            fingerprint: fp.to_string(),
            out_uid: uid_out(fp),
            in_uid: uid_in(fp),
            out_name,
            in_name,
            display: name.to_string(),
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
            sent_online: d.online,
            sent: true,
            acked: true,
            generation,
            state: Some(HalSlotState::Bound),
            observed: true,
            ..SlotRec::default()
        }
    }

    fn seen(ds: &[&DesiredDevice]) -> HashSet<String> {
        let mut s: HashSet<String> = ds
            .iter()
            .flat_map(|d| [d.out_uid.clone(), d.in_uid.clone()])
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
        let (out, mic) = device_names("客厅 Mac", false);
        assert_eq!(out, "AudioHub – 客厅 Mac 扬声器");
        assert_eq!(mic, "AudioHub – 客厅 Mac 麦克风");
        let (out, _) = device_names("客厅 Mac", true);
        assert_eq!(out, "AudioHub – 客厅 Mac 扬声器（离线）");
        // ...and a name that would not fit the driver's char[128] is cut on a
        // character boundary, because invalid UTF-8 makes the driver reject the
        // whole Bind — losing the device, not just the tail of its name.
        let (out, _) = device_names(&"漢".repeat(200), false);
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
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
        assert_eq!(base_name(&p), "书房", "an alias is trimmed, not taken literally");
        p.alias = Some("   ".into());
        assert_eq!(base_name(&p), "MacBook Pro", "a blank alias is not an alias");
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
        t.save(&dir).expect("save");

        // What a restart sees. If this ever came back different, every device
        // would be republished on every restart and the user's default output
        // would be thrown away with it.
        let t2 = SlotTable::load(&dir);
        assert_eq!(t2.slot_of("aaaa"), Some(0));
        assert_eq!(t2.slot_of("bbbb"), Some(1));
        assert_eq!(t2.used(), 2);

        // A released slot is the LOWEST free one again, not the next one up.
        let mut t3 = t2.clone();
        assert_eq!(t3.release("aaaa"), Some(0));
        assert_eq!(t3.assign("cccc", 16), Some(0));
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
        assert_eq!(t.assign("cccc", 2), None, "the third peer gets no slot at all");
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
        let BindAction::Set(req) = &acts[0] else { panic!() };
        assert_eq!(req.out_uid, old.out_uid, "a rename must not move the UID");
        assert!(!acts.iter().any(|a| matches!(a, BindAction::Clear { .. })));
    }

    #[test]
    fn going_offline_restates_the_binding_rather_than_removing_it() {
        let online = want(0, "aaaa", "Mac mini");
        let mut offline = online.clone();
        offline.online = false;
        let (o, i) = device_names("Mac mini", true);
        offline.out_name = o;
        offline.in_name = i;
        let mut slots = empty_slots();
        slots[0] = published(&online, 7);
        let acts = plan_binds(&[offline.clone()], &slots, Some(&seen(&[&online])));
        assert_eq!(acts, vec![BindAction::Set(offline.to_bind())]);
    }

    #[test]
    fn unpairing_retires_the_slot_at_its_current_generation() {
        let d = want(2, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[2] = published(&d, 9);
        let acts = plan_binds(&[], &slots, Some(&seen(&[&d])));
        assert_eq!(acts, vec![BindAction::Clear { slot: 2, generation: 9 }]);
    }

    #[test]
    fn a_clear_already_in_flight_is_not_repeated() {
        let d = want(2, "aaaa", "Mac mini");
        let mut slots = empty_slots();
        slots[2] = SlotRec { clearing: true, ..published(&d, 9) };
        assert!(plan_binds(&[], &slots, Some(&seen(&[&d]))).is_empty());
        // ...and once the driver says Free, there is nothing left to do either.
        slots[2] = SlotRec { state: Some(HalSlotState::Free), ..SlotRec::default() };
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
        assert_eq!(acts, vec![BindAction::Clear { slot: 0, generation: 3 }]);
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
        assert!(refuse_using_others(Mode::Share, true).is_none(), "probes still drive us");
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
            assert!(why.contains(m.as_str()), "the refusal must name the mode: {why}");
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
        assert_eq!(no_device_reason(Mode::B), None, "mode B is where devices live");
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
