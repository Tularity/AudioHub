//! Local IPC contract between audiohubd and thin clients (CLI `ctl`, later the
//! Tauri UI). Transport: WebSocket on 127.0.0.1 (random port), text frames,
//! one JSON object per frame. Endpoint + token live in `<config_dir>/ipc.json`.
//!
//! Frame flow:
//!   client -> {"auth":"<token>"}            first frame, mandatory
//!   server -> {"ok":true,"daemon":DaemonInfo}
//!   client -> {"id":1,"method":"...","params":{...}}
//!   server -> {"id":1,"ok":true,"result":...} | {"id":1,"ok":false,"error":"..."}
//!   server -> {"event":"stats","data":...}   unsolicited after stats.subscribe

use serde::{Deserialize, Serialize};

pub const IPC_VERSION: u32 = 1;

pub use audiohub_core::audio::DevicesReport;
pub use audiohub_core::dsp::ToneVerdict;
pub use audiohub_core::permissions::{
    PermissionKind, PermissionState, KIND_LOCAL_NETWORK, KIND_MICROPHONE, KIND_SYSTEM_AUDIO,
};
pub use audiohub_core::sysaudio::VirtualCard;
pub use audiohub_core::volume::VolumeState;
pub use audiohub_net::identity::PairedPeer;

/// Where a page served by the daemon's own web UI asks for the endpoint below.
///
/// The daemon serves the UI over HTTP on its CONTROL port, loopback only
/// (`audiohubd::webui`). A page loaded from there has no `?port&token` in its
/// URL and no Tauri bridge to ask, so it `fetch`es this path on its own origin
/// and gets `{"ipc_version","port","token"}` — the same three values
/// `ipc.json` carries, minus `pid`, which is a liveness detail for whoever owns
/// the file and means nothing to a client that just reached the owner.
pub const IPC_ENDPOINT_PATH: &str = "/ipc-endpoint";

/// Written to `<config_dir>/ipc.json` (0600) by the daemon on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEndpoint {
    pub ipc_version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub ipc_version: u32,
    pub name: String,
    pub fingerprint: String,
    pub control_port: u16,
    pub uptime_s: f64,
    /// Named output devices the UI can offer as a bridge target (spec-m4c §B).
    #[serde(default)]
    pub output_devices: Vec<String>,
    /// Third-party virtual sound cards, `present` telling the UI whether the
    /// bridge selector is selectable or greyed out (spec-m4b §C / m4c §B).
    #[serde(default)]
    pub virtual_cards: Vec<VirtualCard>,
}

/// `kind` from the CALLER's perspective:
/// - "mic": consume the peer's microphone (media flows peer -> me)
/// - "spk": send audio to the peer's default output (media flows me -> peer)
pub const KIND_MIC: &str = "mic";
pub const KIND_SPK: &str = "spk";

/// Audio source for locally-originated streams.
/// "tone" is the probe source (deviceless); "mic" needs capture permission;
/// "sysaudio" mirrors what this machine is playing (spec-m4b §B2, `backend`).
/// "halspk" is whatever an application played into the addressed peer's own
/// virtual speaker on macOS (spec-m5b §5.4) — one device per paired peer, named
/// after that peer. It needs the HAL bridge to be registered, and yields
/// silence — never a stall — while no driver is attached.
pub const SOURCE_TONE: &str = "tone";
pub const SOURCE_MIC: &str = "mic";
pub const SOURCE_SYSAUDIO: &str = "sysaudio";
pub const SOURCE_HAL_SPEAKER: &str = "halspk";

/// `kind` of `daemon.simulate_device_change`.
pub const DEVICE_INPUT: &str = "input";
pub const DEVICE_OUTPUT: &str = "output";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSessionParams {
    pub peer: String,               // fingerprint (prefix allowed, unique)
    pub kind: String,               // KIND_MIC | KIND_SPK
    #[serde(default)]
    pub source: Option<String>,     // for spk / provider tone probes
    #[serde(default)]
    pub freq: Option<f32>,          // tone source frequency
    #[serde(default)]
    pub backend: Option<String>,    // sysaudio source: backend id, None = "auto"
    #[serde(default)]
    pub monitor: bool,              // mic: play received audio locally
    #[serde(default)]
    pub verify_freq: Option<f32>,   // receiver computes ToneVerdict (probe)
    #[serde(default)]
    pub simulate_loss_pct: Option<f32>, // sender-side loss injection (probe)
    #[serde(default)]
    pub volume_sync: bool,          // spk: drive the peer's output volume
    /// mic: ALSO render the decoded peer audio into this NAMED output device
    /// (a third-party virtual card, spec-m4c §B). Independent of `monitor`:
    /// one decode can feed both. A device that cannot be opened fails the
    /// session open — it never falls back to the default output.
    #[serde(default)]
    pub bridge: Option<String>,
    /// mic: ALSO write the decoded peer audio into the virtual microphone this
    /// peer owns, so anything on this Mac that selects "AudioHub – <peer>
    /// 麦克风" hears them (spec-m5b §5.4). Independent of `monitor` and
    /// `bridge` — one decode feeds all three. Explicit by design: it defaults
    /// to false so a session never quietly takes over a virtual microphone.
    ///
    /// WHICH virtual microphone is not expressible here and never will be: the
    /// device belongs to `peer`, and slots are a daemon-internal index that no
    /// IPC client may name (spec-m5b §5.6).
    #[serde(default)]
    pub hal: bool,
    /// Open this session even though mode B owns the session lifecycle
    /// (spec-m5b §6.1). CLI/probe only: in mode B the daemon refuses a plain
    /// `session.open`, because a UI that could open its own sessions would have
    /// turned mode B back into mode A with different labels.
    #[serde(default, rename = "override")]
    pub override_mode: bool,
}

/// Global consumer mode (plan §7.1, frozen): it is a property of THIS machine,
/// not of a peer, so it lives in the daemon and the UI's copy is a cache.
pub const MODE_A: &str = "a";
pub const MODE_B: &str = "b";

/// Daemon-owned settings, `settings.get` / `settings.set` (spec-m5b §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonSettings {
    /// What the user asked for: `MODE_A` | `MODE_B`.
    pub consumer_mode: String,
    /// What is actually in force. `MODE_B` only when the driver is usable, so
    /// the two ends can no longer disagree for long about which mode is live.
    pub effective_mode: String,
    /// plan §7.3: remove a peer's virtual devices while it is disconnected.
    pub remove_virtual_on_disconnect: bool,
    /// Append `（离线）` to a disconnected peer's device names, so "no sound"
    /// is visible in the system's own device list (spec-m5b OPEN QUESTION 1).
    pub mark_offline_devices: bool,
    /// Persisted for the UI; not yet wired to the media plane (the AUTO ladder
    /// still decides both). Kept here so the UI has one home for its settings
    /// instead of localStorage.
    pub latency: String,
    pub quality: String,
    /// Virtual-device slots the attached driver offers, and how many are bound.
    pub hal_capacity: u8,
    pub hal_used: u8,
}

/// One published (or intended) virtual device pair, `hal.devices`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HalDeviceInfo {
    /// Diagnostics only. Never an input anywhere in this contract.
    pub slot: u8,
    pub fingerprint: String,
    pub out_uid: String,
    pub in_uid: String,
    pub out_name: String,
    pub in_name: String,
    pub generation: u32,
    /// "free" | "bound" | "delisted" | "pending" (sent, not yet answered).
    pub state: String,
    /// The system's own device list really contains both UIDs. This is the
    /// closed-loop half: `state == "bound"` alone only says the driver
    /// acknowledged us (spec-m5b §5.2).
    pub observed: bool,
    pub peer_connected: bool,
    pub io_out: bool,
    pub io_in: bool,
    pub spk_frames: u64,
    pub mic_frames: u64,
    pub mic_dropped: u64,
}

/// A peer's virtual devices, as `PeerState.hal_device`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerHalDevice {
    pub out_name: String,
    pub in_name: String,
    pub out_uid: String,
    pub in_uid: String,
    pub state: String,
    pub observed: bool,
}

/// Who opened a session, reported as `SessionInfo.origin`.
pub const ORIGIN_USER: &str = "user";
pub const ORIGIN_HAL: &str = "hal";
pub const ORIGIN_PEER: &str = "peer";

/// Health of the macOS HAL bridge, reported by `daemon.status` as `hal`.
/// Absent/null everywhere the bridge does not exist (any non-macOS host, or a
/// macOS host without the LaunchDaemon), which is the normal case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HalStatus {
    /// launchd handed us the mach name: the driver can find us.
    pub registered: bool,
    /// A HAL plug-in completed the handshake and holds live rings.
    pub driver_connected: bool,
    /// Speaker-direction frames handed to the media engine, over every slot.
    pub spk_frames: u64,
    /// Microphone-direction frames accepted by the rings, over every slot.
    pub mic_frames: u64,
    /// Microphone frames the rings had no room for (driver not draining).
    pub mic_dropped: u64,
    /// Seconds since the last message from the driver, `None` if it never spoke.
    #[serde(default)]
    pub last_driver_msg_secs: Option<f64>,
    /// What this daemon speaks, and what the driver said it speaks when it
    /// refused us. A mismatch is the one driver problem only a reinstall fixes.
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub driver_protocol_version: Option<u32>,
    /// Machine-readable reason there is no live bridge, e.g.
    /// `driver_protocol_mismatch`. `None` while connected.
    #[serde(default)]
    pub status_reason: Option<String>,
    /// Per-peer virtual devices. The three counters above are the sums of the
    /// per-slot ones here (spec-m5b §6.1).
    #[serde(default)]
    pub devices: Vec<HalDeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub received: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub bitrate_kbps: f64,
    pub jb_depth_frames: u32,       // current jitter buffer depth (recv side)
    pub sent_packets: u64,          // send side
    pub rung: u32,                  // current AUTO ladder rung (0 = best)
    pub rung_changes: u32,
    pub verdict: Option<ToneVerdict>,
    pub mix_verdicts: Option<Vec<ToneVerdict>>, // provider mixer taps (probe)
    /// spk sessions opened with `volume_sync`: the provider's real output
    /// device state. Present on BOTH sides — the provider fills it from its own
    /// device, the consumer from the provider's VolumeState reports. `None`
    /// means the session does not sync volume (or nothing arrived yet).
    #[serde(default)]
    pub volume: Option<VolumeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub peer_fingerprint: String,
    pub peer_name: String,
    pub kind: String,               // KIND_MIC | KIND_SPK
    pub dir: String,                // "send" | "recv"
    pub sample_rate: u32,
    pub channels: u8,
    pub stats: SessionStats,
    /// ORIGIN_USER | ORIGIN_HAL | ORIGIN_PEER. A `hal` session exists because
    /// an application selected a virtual device; closing it from the UI would
    /// leave that application's device selection pointing at silence, which is
    /// why the detail page hides its close button (spec-m5b §6.2).
    #[serde(default)]
    pub origin: String,
    /// Diagnostics only, and only on a `hal` session: which slot's rings this
    /// session is wired to. Never accepted as input.
    #[serde(default)]
    pub hal_slot: Option<u8>,
    /// The virtual device's display name, for the stats page.
    #[serde(default)]
    pub hal_device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerState {
    #[serde(flatten)]
    pub peer: PairedPeer,
    pub online: bool,               // live control channel right now
    /// A retry loop is armed for this peer (spec-m4c §C). Only ever true for a
    /// peer THIS daemon has connected to itself.
    #[serde(default)]
    pub reconnecting: bool,
    /// Seconds until the next retry, when `reconnecting`.
    #[serde(default)]
    pub retry_in_s: Option<f64>,
    /// The virtual devices this peer owns, `None` when it has none (mode A, no
    /// driver, or the slot pool is full — see `hal_reason`).
    #[serde(default)]
    pub hal_device: Option<PeerHalDevice>,
    /// Why this peer has no virtual devices: "mode_a" | "no_driver" |
    /// "capacity" | "removed_while_offline".
    #[serde(default)]
    pub hal_reason: Option<String>,
    /// The name the virtual devices carry: the alias if the user set one, the
    /// peer's own computer name otherwise, with ` (2)` appended when two peers
    /// would otherwise be indistinguishable (spec-m5b §5.3).
    #[serde(default)]
    pub display_name: String,
}

/// Method names (params -> result):
/// - "daemon.status"     {}                    -> DaemonInfo + `hal`
///       `hal` is a `HalStatus` object (spec-round2 §B2) or null where no HAL
///       bridge exists. It is added to the DaemonInfo object by the daemon, not
///       carried as a DaemonInfo field, so a client that predates it sees
///       exactly what it saw before.
/// - "daemon.shutdown"   {}                    -> {}
/// - "daemon.simulate_device_change" {kind}    -> {kind, epoch}
///       kind = "input" | "output". Drives the same rebuild path a real
///       default-device change takes, without touching any system device.
/// - "daemon.permissions" {}                   -> Vec<PermissionState>
///       Status of every OS permission the app needs, for the first-run gate
///       page. NEVER prompts, so the UI may poll it freely. `granted: null`
///       means "unknown", which on macOS is the permanent steady state for
///       local network and system audio — neither has a query API. The gate
///       must therefore treat null as "尚未确认，让用户点一次授权", never as a
///       block, or it can never be passed. Only `granted: false` is a real
///       denial, and only System Settings (`settings_url`) can undo it.
/// - "daemon.request_permission" {kind}        -> PermissionState
///       kind = "microphone" | "local_network" | "system_audio" (also accepted
///       under the key "id", which is what the UI calls it). THE ONLY
///       PROMPTING METHOD: raises the OS consent dialog, so it must be driven
///       by a user click, never on load. Blocks while the dialog is up (the
///       microphone case waits ~20s for an answer, the system-audio case as
///       long as Core Audio takes) and answers with the post-attempt state.
///       A `granted` that is still null means the user had not answered yet —
///       keep polling "daemon.permissions". Errors are user-facing text
///       (already denied, no input device, tap refused).
/// - "peers.list"        {}                    -> Vec<PeerState>
/// - "peers.connect"     {peer, addr?}         -> PeerState        (verify by fingerprint)
/// - "peers.disconnect"  {peer}                -> {fingerprint}
///       Drops the control channel AND disarms the reconnect loop for that
///       peer: an explicit disconnect is not a failure to recover from.
/// - "pairing.enable"    {pin?, ttl_s?}        -> {pin}
/// - "pairing.disable"   {}                    -> {}
/// - "discover.run"      {secs?}               -> Vec<DiscoveredPeer-json>
/// - "session.open"      OpenSessionParams     -> SessionInfo
/// - "session.close"     {id}                  -> {}
/// - "session.list"      {}                    -> Vec<SessionInfo>
/// - "session.set_volume" {id, scalar, muted?} -> {}   (spk consumer side only;
///       the result shows up as `stats.volume` on the next session.list/event.
///       An omitted `muted` leaves the peer's mute control untouched — it is
///       never resolved to a default, which would unmute a muted machine)
/// - "stats.subscribe"   {interval_ms?}        -> {} (then "stats" events with Vec<SessionInfo>)
/// - "settings.get"      {}                    -> DaemonSettings
/// - "settings.set"      {consumer_mode?, remove_virtual_on_disconnect?,
///                        mark_offline_devices?, latency?, quality?}
///                                             -> DaemonSettings
///       The mode is DAEMON-owned global state (plan §7.1): switching to
///       mode A removes every virtual device, switching to B recreates them
///       under the same UIDs.
/// - "peers.pair"        {addr, pin}           -> PeerState
///       The initiator half of M3 pairing, moved out of the CLI so a pairing
///       done anywhere is visible to the device coordinator immediately.
/// - "peers.unpair"      {peer}                -> {fingerprint}
///       Removes the pairing, tells the PEER (so its copy of our devices goes
///       away too), closes sessions and removes the virtual devices.
/// - "peers.set_alias"   {peer, alias}         -> {fingerprint, display_name}
///       Renames the peer's virtual devices in place: same UID, same
///       AudioObjectID, so an application's device selection survives it.
pub mod methods {
    pub const DAEMON_STATUS: &str = "daemon.status";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    pub const DAEMON_SIMULATE_DEVICE_CHANGE: &str = "daemon.simulate_device_change";
    pub const DAEMON_PERMISSIONS: &str = "daemon.permissions";
    pub const DAEMON_REQUEST_PERMISSION: &str = "daemon.request_permission";
    pub const PEERS_LIST: &str = "peers.list";
    pub const PEERS_CONNECT: &str = "peers.connect";
    pub const PEERS_DISCONNECT: &str = "peers.disconnect";
    pub const PAIRING_ENABLE: &str = "pairing.enable";
    pub const PAIRING_DISABLE: &str = "pairing.disable";
    pub const DISCOVER_RUN: &str = "discover.run";
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_SET_VOLUME: &str = "session.set_volume";
    pub const STATS_SUBSCRIBE: &str = "stats.subscribe";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const PEERS_PAIR: &str = "peers.pair";
    pub const PEERS_UNPAIR: &str = "peers.unpair";
    pub const PEERS_SET_ALIAS: &str = "peers.set_alias";
}
