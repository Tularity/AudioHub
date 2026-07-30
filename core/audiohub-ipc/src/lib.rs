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
/// "halspk" is whatever an application played into the macOS virtual device
/// "AudioHub Speaker" (spec-round2 §B2); it needs the HAL bridge to be
/// registered, and yields silence — never a stall — while no driver is attached.
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
    /// mic: ALSO write the decoded peer audio into the macOS HAL bridge's mic
    /// ring, so anything on this Mac that selects "AudioHub Microphone" hears
    /// the peer (spec-round2 §B2). Independent of `monitor` and `bridge` — one
    /// decode feeds all three. Explicit by design: it defaults to false so a
    /// session never quietly takes over the virtual microphone.
    #[serde(default)]
    pub hal: bool,
}

/// Health of the macOS HAL bridge, reported by `daemon.status` as `hal`.
/// Absent/null everywhere the bridge does not exist (any non-macOS host, or a
/// macOS host without the LaunchDaemon), which is the normal case.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HalStatus {
    /// launchd handed us the mach name: the driver can find us.
    pub registered: bool,
    /// A HAL plug-in completed the handshake and holds live rings.
    pub driver_connected: bool,
    /// Speaker-direction frames handed to the media engine.
    pub spk_frames: u64,
    /// Microphone-direction frames accepted by the ring.
    pub mic_frames: u64,
    /// Microphone frames the ring had no room for (driver not draining).
    pub mic_dropped: u64,
    /// Seconds since the last message from the driver, `None` if it never spoke.
    #[serde(default)]
    pub last_driver_msg_secs: Option<f64>,
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
}
