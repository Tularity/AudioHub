use crate::bus::PcmLanes;
use crate::protocol::media::engine::PcmOutput;
use crate::protocol::server::{
    self, EventCommandSendError, EventCommandSender, PcmOutputFactory, ProbeEvent,
    RemoteControlMap as Ap2RemoteControlMap, RemoteControlState as Ap2RemoteControlState,
    ServerConfig, VolumeMap as Ap2VolumeMap, VolumeUpdate as Ap2VolumeUpdate,
};
use crate::{BusConfig, PcmBus, StreamingStereoPcmConverter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

const START_TIMEOUT: Duration = Duration::from_secs(3);
const DNS_LABEL_MAX_BYTES: usize = 63;
// Sized for several concurrent senders (design §2.2).
const UPSTREAM_EVENT_QUEUE_CAPACITY: usize = 64;
/// Per-subscriber event backlog. Runtime status is the source of truth, so a
/// consumer that cannot keep up drops intermediate notifications instead of
/// being allowed to retain an unbounded amount of artwork/metadata in memory.
const EVENT_QUEUE_CAPACITY: usize = 32;
/// Ended stream ids remembered so a late watch value cannot revive them.
const RETIRED_STREAM_LIMIT: usize = 64;
/// Network-runtime worker threads: control connections plus up to four
/// decoding media streams.
const RUNTIME_WORKER_THREADS: usize = 4;
/// Structured marker consumed by the daemon when deciding whether a persisted
/// control port may be replaced. Callers must not infer bind phase from an I/O
/// kind shared by later identity, PTP, or startup operations.
#[derive(Debug)]
struct ControlPortBindError(io::Error);

impl fmt::Display for ControlPortBindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cannot reserve AirPlay control port: {}", self.0)
    }
}

impl std::error::Error for ControlPortBindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// True only for an error produced by the control-listener bind operation.
pub fn is_control_port_bind_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<ControlPortBindError>())
}

/// AirPlay protocol identifier attached to public events and sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Encrypted classic RAOP audio on the shared control listener.
    AirPlay1,
    /// AudioHub-owned AirPlay 2 control and media protocol.
    AirPlay2,
}

/// Receiver-owned volume state exposed to an AirPlay sender.
///
/// AirPlay 2 represents the visible slider and mute state independently. The
/// slider always remains in the ordinary `-30..=0 dB` range; `-144 dB` is a
/// legacy text-parameter sentinel and must not replace the slider while muted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ReceiverVolumeSnapshot {
    /// Receiver slider position in AirPlay's linear `-30..=0 dB` domain.
    pub(crate) slider_db: f32,
    /// Whether the receiver output is muted independently of its slider.
    pub(crate) is_muted: bool,
}

impl ReceiverVolumeSnapshot {
    /// Validate and construct one receiver volume snapshot.
    pub(crate) fn new(slider_db: f32, is_muted: bool) -> io::Result<Self> {
        if !slider_db.is_finite() || !(-30.0..=0.0).contains(&slider_db) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay receiver slider volume must be within -30..=0 dB",
            ));
        }
        Ok(Self {
            slider_db,
            is_muted,
        })
    }

    /// Convert the legacy text-parameter representation into AP2's split
    /// slider/mute state. A mute sentinel has no slider payload, so the
    /// conservative zero-percent position is retained for compatibility.
    fn from_legacy_db(db: f32) -> io::Result<Self> {
        if db == -144.0 {
            Self::new(-30.0, true)
        } else {
            Self::new(db, false)
        }
    }

    /// Value used by legacy `GET_PARAMETER volume` responses.
    pub(crate) fn legacy_text_db(self) -> f32 {
        if self.is_muted {
            -144.0
        } else {
            self.slider_db
        }
    }
}

type ReceiverVolumeReader = dyn Fn() -> io::Result<ReceiverVolumeSnapshot> + Send + Sync + 'static;

/// Thread-safe live receiver-volume reader shared by protocol connections.
///
/// The callback is invoked for every protocol query, rather than being sampled
/// only when the receiver starts. This keeps `/info`, event `updateInfo`, and
/// text volume queries aligned with the actual system output state.
#[derive(Clone)]
pub(crate) struct ReceiverVolumeProvider {
    reader: Arc<ReceiverVolumeReader>,
    last_good: Arc<Mutex<Option<ReceiverVolumeSnapshot>>>,
}

impl ReceiverVolumeProvider {
    /// Wrap a thread-safe live reader.
    fn new(
        reader: impl Fn() -> io::Result<ReceiverVolumeSnapshot> + Send + Sync + 'static,
    ) -> Self {
        Self {
            reader: Arc::new(reader),
            last_good: Arc::new(Mutex::new(None)),
        }
    }

    /// Construct a provider for a fixed compatibility value.
    pub(crate) fn fixed(snapshot: ReceiverVolumeSnapshot) -> Self {
        let provider = Self::new(move || Ok(snapshot));
        *provider
            .last_good
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(snapshot);
        provider
    }

    /// Read and validate the current receiver state.
    pub(crate) fn current(&self) -> io::Result<ReceiverVolumeSnapshot> {
        let fresh = (self.reader)().and_then(|snapshot| {
            ReceiverVolumeSnapshot::new(snapshot.slider_db, snapshot.is_muted)
        });
        match fresh {
            Ok(snapshot) => {
                *self
                    .last_good
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(snapshot);
                Ok(snapshot)
            }
            Err(error) => self
                .last_good
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .copied()
                .ok_or(error),
        }
    }
}

impl fmt::Debug for ReceiverVolumeProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReceiverVolumeProvider")
            .finish_non_exhaustive()
    }
}

/// Configuration for one receiver runtime.
#[derive(Clone)]
pub struct AirPlayConfig {
    /// Name shown in the system AirPlay picker.
    pub name: String,
    /// Stable receiver MAC. `None` derives one from the AirPlay 2 identity.
    pub mac: Option<[u8; 6]>,
    /// Optional receiver password. It is passed directly to protocol auth and
    /// is never exposed through status, events, Debug, or mDNS records.
    pub password: Option<String>,
    /// AudioHub-owned AirPlay 2 listener.
    pub enable_airplay2: bool,
    /// Requested AP2 control port. Zero selects a currently free ephemeral
    /// port and is the default.
    pub airplay2_port: u16,
    /// Stable AirPlay 2 Ed25519 identity. Required when AP2 is enabled.
    pub airplay2_identity_path: Option<PathBuf>,
    /// Legacy fixed fallback for callers without a live receiver-volume
    /// provider. `-144 dB` is interpreted as muted with a zero-percent AP2
    /// slider; new callers should use [`Self::set_receiver_volume_provider`].
    pub initial_volume_db: Option<f32>,
    /// Live default-output volume queried by each AirPlay protocol request.
    /// When present, this takes precedence over `initial_volume_db` and keeps
    /// the ordinary slider value independent from the mute flag.
    receiver_volume_provider: Option<ReceiverVolumeProvider>,
    /// Bounded PCM bus and clock-servo configuration.
    pub bus: BusConfig,
    // In-process daemon tests need independent UDP sockets because Cargo
    // compiles this crate as a normal dependency of `audiohubd` tests. Real
    // receiver construction always leaves this false and binds 319/320.
    ptp_test_ephemeral: bool,
}

impl AirPlayConfig {
    /// Production defaults: native AirPlay 2 on, no password, automatic
    /// control port, and no in-crate mDNS advertisement.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mac: None,
            password: None,
            enable_airplay2: true,
            airplay2_port: 0,
            airplay2_identity_path: None,
            initial_volume_db: None,
            receiver_volume_provider: None,
            bus: BusConfig::default(),
            ptp_test_ephemeral: false,
        }
    }

    /// Isolate PTP sockets for a test process that starts several receivers.
    /// This must never be used by an advertised receiver because native
    /// senders transmit PTP to the well-known UDP ports 319 and 320.
    #[doc(hidden)]
    pub fn use_ephemeral_ptp_ports_for_tests(&mut self) {
        self.ptp_test_ephemeral = true;
    }

    /// Install the live system-volume reader used by AirPlay protocol
    /// responses. The callback may run concurrently on several connection
    /// tasks and must return `(slider_db, is_muted)`, where `slider_db` is in
    /// the ordinary `-30..=0 dB` range even while muted.
    pub fn set_receiver_volume_provider(
        &mut self,
        reader: impl Fn() -> io::Result<(f32, bool)> + Send + Sync + 'static,
    ) {
        self.receiver_volume_provider = Some(ReceiverVolumeProvider::new(move || {
            let (slider_db, is_muted) = reader()?;
            ReceiverVolumeSnapshot::new(slider_db, is_muted)
        }));
    }

    fn validate(mut self) -> io::Result<Self> {
        if self.name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay receiver name is empty",
            ));
        }
        if !self.enable_airplay2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay 2 must be enabled for an AirPlay runtime",
            ));
        }
        if self.enable_airplay2 && self.airplay2_identity_path.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay 2 requires a stable identity path",
            ));
        }
        if self
            .initial_volume_db
            .is_some_and(|db| !db.is_finite() || (db != -144.0 && !(-30.0..=0.0).contains(&db)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay initial volume must be -144 dB or within -30..=0 dB",
            ));
        }
        if self.password.as_deref() == Some("") {
            self.password = None;
        }
        self.bus = self.bus.validate()?;
        Ok(self)
    }
}

impl fmt::Debug for AirPlayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AirPlayConfig")
            .field("name", &self.name)
            .field("mac", &self.mac)
            .field("password_set", &self.password.is_some())
            .field("enable_airplay2", &self.enable_airplay2)
            .field("airplay2_port", &self.airplay2_port)
            .field("airplay2_identity_path", &self.airplay2_identity_path)
            .field("initial_volume_db", &self.initial_volume_db)
            .field(
                "receiver_volume_provider_set",
                &self.receiver_volume_provider.is_some(),
            )
            .field("bus", &self.bus)
            .finish()
    }
}

/// Complete data for a daemon-owned DNS-SD registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdnsService {
    /// Protocol represented by this service.
    pub protocol: Protocol,
    /// Fully-qualified `_airplay._tcp.local.` service type.
    pub service_type: String,
    /// DNS-SD instance label.
    pub instance_name: String,
    /// Concrete listener port selected during startup.
    pub port: u16,
    /// Upstream-generated protocol TXT records.
    pub txt_records: Vec<String>,
}

/// Timing protocol of a session and, for PTP, the backend mapping it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTiming {
    /// NTP-style timing (AirPlay 2 realtime over NTP, classic RAOP).
    Ntp,
    /// macOS per-sender TimeSync gPTP instance.
    PtpInstance,
    /// macOS shared CoreMedia system clock (one PTP sender at a time).
    PtpSystem,
    /// Portable UDP observer and mapper (Windows).
    PtpPortable,
    #[default]
    Unknown,
}

/// One active incoming sender session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Runtime-unique ID; changes on every new stream and is also the PCM
    /// lane's session identity.
    pub id: u64,
    /// Negotiated protocol.
    pub protocol: Protocol,
    /// Sender address, once the protocol event has supplied it.
    pub peer: Option<IpAddr>,
    /// Input sample rate reported by the protocol engine.
    pub sample_rate: u32,
    /// Input channel count reported by the protocol engine.
    pub channels: u8,
    /// Whether transport is paused (AP2 only reports this explicitly).
    pub paused: bool,
    /// Latest sender volume in AirPlay dB. It is observational only; PCM is
    /// always full-scale and the daemon must drive system output volume.
    pub volume_db: Option<f32>,
    /// Current metadata, if supplied.
    pub title: Option<String>,
    /// Current metadata, if supplied.
    pub artist: Option<String>,
    /// Current metadata, if supplied.
    pub album: Option<String>,
    /// Monotonic revision for the current artwork state. A revision with no
    /// content type means the sender explicitly cleared the previous image.
    pub artwork_revision: Option<u64>,
    pub artwork_content_type: Option<String>,
    /// Last reported playback position.
    pub elapsed_ms: Option<u64>,
    /// Last reported track duration.
    pub duration_ms: Option<u64>,
    /// Unix timestamp when this runtime first observed the stream.
    pub started_unix_ms: u64,
    /// True when this session holds a PCM lane (its audio is being mixed).
    pub selected_for_output: bool,
    /// PCM lane index held by this session.
    #[serde(default)]
    pub lane: Option<u8>,
    /// Timing negotiated for this session.
    #[serde(default)]
    pub timing: SessionTiming,
}

/// Authoritative, versioned cover image for one concrete runtime session.
/// Artwork bytes are intentionally absent from [`RuntimeStatus`] and must be
/// fetched only when the small session view reports a new revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkSnapshot {
    pub session_id: u64,
    pub revision: u64,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// DACP bearer capability for one AirPlay sender session.
///
/// This is intentionally separate from [`RuntimeStatus`]: `active_remote` is
/// a secret request token and therefore must never enter IPC/status snapshots.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteControlInfo {
    pub session_id: u64,
    /// RTSP peer address. Its original TCP port is informational; the IPv6
    /// scope ID is retained so a link-local DACP service is connectable.
    pub peer: SocketAddr,
    pub dacp_id: String,
    pub active_remote: String,
}

impl fmt::Debug for RemoteControlInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteControlInfo")
            .field("session_id", &self.session_id)
            .field("peer", &self.peer)
            .field("dacp_id", &self.dacp_id)
            .field("active_remote", &"<redacted>")
            .finish()
    }
}

/// Secret-bearing DACP snapshot paired with a revocable authority lease.
///
/// A daemon may queue work with this value, but must check [`Self::is_current`]
/// immediately before using the bearer token. A credential refresh or the end
/// of this session revokes older snapshots without copying or hashing the
/// token. Leases are per session: another sender's refresh never revokes it.
#[derive(Clone)]
pub struct RemoteControlSnapshot {
    info: RemoteControlInfo,
    revision: u64,
    current_revision: Arc<AtomicU64>,
}

impl RemoteControlSnapshot {
    /// Secret-bearing capability protected by this lease.
    pub fn info(&self) -> &RemoteControlInfo {
        &self.info
    }

    /// Non-secret monotonically changing authority revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether the capability is still the runtime's current authority.
    pub fn is_current(&self) -> bool {
        self.current_revision.load(Ordering::Acquire) == self.revision
    }
}

impl fmt::Debug for RemoteControlSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteControlSnapshot")
            .field("info", &self.info)
            .field("revision", &self.revision)
            .field("current", &self.is_current())
            .finish()
    }
}

/// Lossless authoritative sender-volume state for one concrete PCM session.
///
/// `revision` advances for every accepted sender command of this stream,
/// even when `db` is identical to the previous value. Pollers can therefore
/// distinguish a new same-value command from an old status snapshot after a
/// reverse-volume write. The tuple `(session_id, revision)` is the command
/// identity; `order` ranks commands across all sessions (last one wins).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SenderVolumeSnapshot {
    /// Runtime-local concrete PCM session identity.
    pub session_id: u64,
    /// Latest sender-authored AirPlay volume in dB.
    pub db: f32,
    /// Per-stream command revision: 1, 2, 3, ... with no gaps from the wire.
    pub revision: u64,
    /// Receiver-wide command order, monotonic across sessions.
    pub order: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverVolumeControlInfo {
    session_id: u64,
    sender: EventCommandSender,
}

/// Revocable AP2 event-channel capability for receiver-originated volume.
///
/// This is intentionally absent from serialized runtime status. It owns no
/// socket or cryptographic state; the protocol actor remains their sole owner.
#[derive(Clone)]
pub struct ReceiverVolumeControlSnapshot {
    info: ReceiverVolumeControlInfo,
    revision: u64,
    current_revision: Arc<AtomicU64>,
}

impl ReceiverVolumeControlSnapshot {
    pub fn session_id(&self) -> u64 {
        self.info.session_id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_current(&self) -> bool {
        self.current_revision.load(Ordering::Acquire) == self.revision
            && self.info.sender.is_active()
    }

    pub fn send_device_volume(
        &self,
        volume_scalar: f32,
        is_muted: bool,
    ) -> Result<(), EventCommandSendError> {
        if !self.is_current() {
            return Err(EventCommandSendError::revoked(
                io::ErrorKind::NotConnected,
                "AirPlay event volume authority was revoked",
            ));
        }
        self.info.sender.send_device_volume(volume_scalar, is_muted)
    }
}

impl fmt::Debug for ReceiverVolumeControlSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReceiverVolumeControlSnapshot")
            .field("session_id", &self.session_id())
            .field("revision", &self.revision)
            .field("current", &self.is_current())
            .finish()
    }
}

/// Normalized events emitted by the native AirPlay 2 engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AirPlayEvent {
    /// A sender completed stream setup.
    SessionStarted { session: SessionInfo },
    /// Sender volume in AirPlay dB. AudioHub must apply this to the receiver
    /// machine's system output; this crate never scales PCM.
    Volume {
        protocol: Protocol,
        session_id: Option<u64>,
        db: f32,
    },
    /// Complete textual now-playing metadata.
    Metadata {
        protocol: Protocol,
        session_id: Option<u64>,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
    },
    /// Sender-provided cover revision. Bytes stay in the bounded runtime
    /// artwork store and are fetched explicitly by session/revision.
    Artwork {
        protocol: Protocol,
        session_id: Option<u64>,
        content_type: Option<String>,
        revision: u64,
    },
    /// Playback progress derived by the protocol engine.
    Progress {
        protocol: Protocol,
        session_id: Option<u64>,
        elapsed_ms: u64,
        duration_ms: u64,
    },
    /// AP2 pause/resume state.
    Paused {
        protocol: Protocol,
        session_id: Option<u64>,
        paused: bool,
    },
    /// Sender flushed/seeked. PCM reader generations have already reset.
    Flushed {
        protocol: Protocol,
        session_id: Option<u64>,
    },
    /// Sender ended or disconnected.
    SessionEnded {
        protocol: Protocol,
        session_id: Option<u64>,
    },
    /// A listener failed after startup.
    RuntimeFailed { message: String },
}

/// Lifecycle state of the receiver thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Starting,
    Listening,
    Stopped,
    Failed,
}

/// Snapshot safe to expose over AudioHub IPC. It contains no password or
/// AirPlay 2 private identity material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub phase: RuntimePhase,
    pub airplay2_port: Option<u16>,
    pub sessions: Vec<SessionInfo>,
    pub last_error: Option<String>,
    /// PTP backend a new sender would get: `instance`, `system`, `portable`
    /// or `unavailable`; `None` until determined (macOS probes lazily).
    #[serde(default)]
    pub ptp_backend: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SenderVolumeState {
    db: f32,
    revision: u64,
    order: u64,
}

/// Everything the runtime knows about one upstream stream. Keyed by the
/// server's stream id, which is unique across protocols and connections.
#[derive(Debug)]
struct SessionRecord {
    info: SessionInfo,
    remote: Option<RemoteControlInfo>,
    /// Per-session DACP lease; bumped on refresh and when the session ends.
    remote_revision: Arc<AtomicU64>,
    event_volume: Option<EventCommandSender>,
    /// Per-session AP2 event-volume lease.
    event_revision: Arc<AtomicU64>,
    sender_volume: Option<SenderVolumeState>,
    artwork: Option<ArtworkSnapshot>,
    /// The protocol published the stream's end; the sink may still be
    /// draining. Descriptive updates are no longer accepted.
    upstream_ended: bool,
}

impl SessionRecord {
    fn new(info: SessionInfo) -> Self {
        Self {
            info,
            remote: None,
            remote_revision: Arc::new(AtomicU64::new(1)),
            event_volume: None,
            event_revision: Arc::new(AtomicU64::new(1)),
            sender_volume: None,
            artwork: None,
            upstream_ended: false,
        }
    }

    fn holds_lane(&self) -> bool {
        self.info.lane.is_some()
    }

    fn revoke_leases(&self) {
        bump(&self.remote_revision);
        bump(&self.event_revision);
    }

    fn remote_snapshot(&self) -> Option<RemoteControlSnapshot> {
        if !self.holds_lane() {
            return None;
        }
        Some(RemoteControlSnapshot {
            info: self.remote.clone()?,
            revision: self.remote_revision.load(Ordering::Acquire),
            current_revision: Arc::clone(&self.remote_revision),
        })
    }

    fn event_volume_snapshot(&self) -> Option<ReceiverVolumeControlSnapshot> {
        if !self.holds_lane() {
            return None;
        }
        let sender = self.event_volume.as_ref()?;
        if !sender.is_active() {
            return None;
        }
        Some(ReceiverVolumeControlSnapshot {
            info: ReceiverVolumeControlInfo {
                session_id: self.info.id,
                sender: sender.clone(),
            },
            revision: self.event_revision.load(Ordering::Acquire),
            current_revision: Arc::clone(&self.event_revision),
        })
    }

    fn sender_volume_snapshot(&self) -> Option<SenderVolumeSnapshot> {
        if !self.holds_lane() {
            return None;
        }
        let volume = self.sender_volume?;
        volume.db.is_finite().then_some(SenderVolumeSnapshot {
            session_id: self.info.id,
            db: volume.db,
            revision: volume.revision,
            order: volume.order,
        })
    }
}

fn bump(revision: &AtomicU64) -> u64 {
    revision.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug)]
struct StatusState {
    phase: RuntimePhase,
    sessions: BTreeMap<u64, SessionRecord>,
    /// Recently ended stream ids, oldest first.
    retired: VecDeque<u64>,
    /// Every id at or below this that is not live has ended (ids evicted
    /// from `retired`; stream ids only grow).
    retired_floor: u64,
    last_error: Option<String>,
}

impl StatusState {
    fn is_retired(&self, stream_id: u64) -> bool {
        !self.sessions.contains_key(&stream_id)
            && (stream_id <= self.retired_floor || self.retired.contains(&stream_id))
    }

    fn retire(&mut self, stream_id: u64) {
        if self.retired.contains(&stream_id) {
            return;
        }
        self.retired.push_back(stream_id);
        while self.retired.len() > RETIRED_STREAM_LIMIT {
            if let Some(evicted) = self.retired.pop_front() {
                self.retired_floor = self.retired_floor.max(evicted);
            }
        }
    }


    fn by_session_id(&self, session_id: u64) -> Option<&SessionRecord> {
        self.sessions
            .values()
            .find(|record| record.info.id == session_id)
    }

    /// A live, not yet ended record for descriptive updates.
    fn current_mut(&mut self, stream_id: u64) -> Option<&mut SessionRecord> {
        self.sessions
            .get_mut(&stream_id)
            .filter(|record| !record.upstream_ended)
    }
}

#[derive(Debug, Default)]
struct EventHub {
    subscribers: Mutex<Vec<mpsc::SyncSender<AirPlayEvent>>>,
}

impl EventHub {
    fn subscribe(&self) -> mpsc::Receiver<AirPlayEvent> {
        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    fn send(&self, event: AirPlayEvent) {
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|tx| match tx.try_send(event.clone()) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
    }
}

#[derive(Debug)]
struct Shared {
    lanes: PcmLanes,
    ports: BTreeMap<Protocol, u16>,
    /// The PTP sockets are the portable observer (test receivers).
    ptp_portable: bool,
    next_session: AtomicU64,
    next_artwork_revision: AtomicU64,
    status: Mutex<StatusState>,
    events: EventHub,
}

impl Shared {
    fn new(lanes: PcmLanes, ports: BTreeMap<Protocol, u16>, ptp_portable: bool) -> Self {
        Self {
            lanes,
            ports,
            ptp_portable,
            next_session: AtomicU64::new(1),
            next_artwork_revision: AtomicU64::new(1),
            status: Mutex::new(StatusState {
                phase: RuntimePhase::Starting,
                sessions: BTreeMap::new(),
                retired: VecDeque::new(),
                retired_floor: 0,
                last_error: None,
            }),
            events: EventHub::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StatusState> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_phase(&self, phase: RuntimePhase, error: Option<String>) {
        let mut status = self.lock();
        status.phase = phase;
        status.last_error = error;
        if matches!(phase, RuntimePhase::Stopped | RuntimePhase::Failed) {
            let ended: Vec<u64> = status.sessions.keys().copied().collect();
            for record in status.sessions.values() {
                record.revoke_leases();
            }
            status.sessions.clear();
            for stream_id in ended {
                status.retire(stream_id);
            }
        }
    }

    /// Create (provisionally) or refresh the record for a stream reported by
    /// the protocol's remote-control watch. `remote_control_update`: `None`
    /// preserves credentials, `Some(None)` clears and `Some(Some(_))`
    /// replaces them. Ended streams are never revived.
    fn ensure_session_versioned(
        &self,
        protocol: Protocol,
        stream_id: u64,
        rate: u32,
        channels: u8,
        peer: Option<IpAddr>,
        remote_control_update: Option<Option<RemoteControlInfo>>,
    ) -> Option<SessionInfo> {
        let mut status = self.lock();
        if status.is_retired(stream_id) {
            return None;
        }
        if status
            .sessions
            .get(&stream_id)
            .is_some_and(|record| record.upstream_ended)
        {
            return None;
        }
        let record = status.sessions.entry(stream_id).or_insert_with(|| {
            SessionRecord::new(SessionInfo {
                id: self.next_session.fetch_add(1, Ordering::Relaxed),
                protocol,
                peer,
                sample_rate: rate,
                channels,
                paused: false,
                volume_db: None,
                title: None,
                artist: None,
                album: None,
                artwork_revision: None,
                artwork_content_type: None,
                elapsed_ms: None,
                duration_ms: None,
                started_unix_ms: unix_now_ms(),
                // Provisional: the sink callback binds PCM ownership.
                selected_for_output: false,
                lane: None,
                timing: SessionTiming::Unknown,
            })
        });
        record.info.sample_rate = rate;
        record.info.channels = channels;
        if peer.is_some() {
            record.info.peer = peer;
        }
        if let Some(remote_control) = remote_control_update {
            let session_id = record.info.id;
            let remote_control = remote_control.map(|mut remote| {
                remote.session_id = session_id;
                remote
            });
            if record.remote != remote_control {
                bump(&record.remote_revision);
                record.remote = remote_control;
            }
        }
        Some(record.info.clone())
    }

    #[cfg(test)]
    fn session_id(&self, stream_id: u64) -> Option<u64> {
        self.lock().sessions.get(&stream_id).map(|record| record.info.id)
    }

    fn session_id_versioned(&self, stream_id: u64) -> Option<(Protocol, u64)> {
        let status = self.lock();
        let record = status.sessions.get(&stream_id)?;
        (!record.upstream_ended).then_some((record.info.protocol, record.info.id))
    }

    /// Authoritative binding of a concrete sink: mint the session id and
    /// assign the lowest free PCM lane. Never preempts another session.
    fn activate_sink(
        &self,
        protocol: Protocol,
        stream_id: u64,
        rate: u32,
        channels: u8,
        peer: Option<IpAddr>,
        timing: SessionTiming,
    ) -> (u64, Option<PcmBus>) {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut status = self.lock();
        let used: Vec<u8> = status
            .sessions
            .iter()
            .filter(|(key, _)| **key != stream_id)
            .filter_map(|(_, record)| record.info.lane)
            .collect();
        let lane = (0..crate::bus::MAX_AIRPLAY_LANES as u8).find(|index| !used.contains(index));
        let previous = status.sessions.remove(&stream_id);
        if let Some(previous) = previous.as_ref() {
            // The provisional id is replaced; outstanding leases name it.
            previous.revoke_leases();
        }
        let mut record = SessionRecord::new(SessionInfo {
            id,
            protocol,
            peer: peer.or_else(|| previous.as_ref().and_then(|item| item.info.peer)),
            sample_rate: rate,
            channels,
            paused: false,
            volume_db: previous.as_ref().and_then(|item| item.info.volume_db),
            title: previous.as_ref().and_then(|item| item.info.title.clone()),
            artist: previous.as_ref().and_then(|item| item.info.artist.clone()),
            album: previous.as_ref().and_then(|item| item.info.album.clone()),
            artwork_revision: previous.as_ref().and_then(|item| item.info.artwork_revision),
            artwork_content_type: previous
                .as_ref()
                .and_then(|item| item.info.artwork_content_type.clone()),
            elapsed_ms: previous.as_ref().and_then(|item| item.info.elapsed_ms),
            duration_ms: previous.as_ref().and_then(|item| item.info.duration_ms),
            started_unix_ms: unix_now_ms(),
            selected_for_output: lane.is_some(),
            lane,
            timing,
        });
        if let Some(previous) = previous {
            record.remote = previous.remote.map(|mut remote| {
                remote.session_id = id;
                remote
            });
            record.event_volume = previous.event_volume;
            record.sender_volume = previous.sender_volume;
            record.artwork = previous.artwork.map(|mut artwork| {
                artwork.session_id = id;
                artwork
            });
        }
        status.sessions.insert(stream_id, record);
        let bus = match lane {
            Some(index) => {
                let bus = self.lanes.ensure_lane(usize::from(index)).clone();
                bus.activate(id);
                Some(bus)
            }
            None => {
                // Unreachable while lanes == media permits; never preempt.
                crate::telemetry::record_pcm_lane_exhausted(crate::telemetry::operation_token());
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay sink for stream {stream_id} found no free PCM lane; its audio is discarded"
                );
                None
            }
        };
        (id, bus)
    }

    fn session(&self, stream_id: u64, id: u64) -> Option<SessionInfo> {
        self.lock()
            .sessions
            .get(&stream_id)
            .filter(|record| record.info.id == id)
            .map(|record| record.info.clone())
    }

    #[cfg(test)]
    fn update_volume(&self, stream_id: u64, db: f32) -> Option<u64> {
        let (revision, order) = {
            let status = self.lock();
            let record = status.sessions.get(&stream_id)?;
            let previous = record.sender_volume.map(|volume| (volume.revision, volume.order));
            let (revision, order) = previous.unwrap_or((0, 0));
            (revision + 1, order + 1)
        };
        self.update_volume_versioned(stream_id, revision, order, db)
    }

    /// Accept one sender volume command. Only a session holding a lane is a
    /// volume source; per-stream revisions must strictly increase.
    fn update_volume_versioned(
        &self,
        stream_id: u64,
        revision: u64,
        order: u64,
        db: f32,
    ) -> Option<u64> {
        if !db.is_finite() || revision == 0 {
            return None;
        }
        let mut status = self.lock();
        let record = status.current_mut(stream_id)?;
        if !record.holds_lane() {
            return None;
        }
        if record
            .sender_volume
            .is_some_and(|previous| revision <= previous.revision)
        {
            return None;
        }
        record.info.volume_db = Some(db);
        record.sender_volume = Some(SenderVolumeState {
            db,
            revision,
            order,
        });
        Some(record.info.id)
    }

    fn update_metadata_versioned(
        &self,
        stream_id: u64,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
    ) -> Option<(Protocol, u64)> {
        let mut status = self.lock();
        let record = status.current_mut(stream_id)?;
        record.info.title = title;
        record.info.artist = artist;
        record.info.album = album;
        Some((record.info.protocol, record.info.id))
    }

    fn update_progress_versioned(
        &self,
        stream_id: u64,
        elapsed_ms: Option<u64>,
        duration_ms: Option<u64>,
    ) -> Option<(Protocol, u64)> {
        let mut status = self.lock();
        let record = status.current_mut(stream_id)?;
        record.info.elapsed_ms = elapsed_ms;
        record.info.duration_ms = duration_ms;
        Some((record.info.protocol, record.info.id))
    }

    fn update_paused_versioned(&self, stream_id: u64, paused: bool) -> Option<(Protocol, u64)> {
        let mut status = self.lock();
        let record = status.current_mut(stream_id)?;
        record.info.paused = paused;
        Some((record.info.protocol, record.info.id))
    }

    /// One artwork per session (each at most the 4 MB protocol body limit).
    fn update_artwork_versioned(
        &self,
        stream_id: u64,
        content_type: String,
        data: Vec<u8>,
    ) -> Option<(Protocol, u64, u64)> {
        let revision = self.next_artwork_revision.fetch_add(1, Ordering::AcqRel);
        let mut status = self.lock();
        let record = status.current_mut(stream_id)?;
        let session_id = record.info.id;
        let normalized_type = content_type.to_ascii_lowercase();
        let content_type = (!data.is_empty()
            && matches!(normalized_type.as_str(), "image/jpeg" | "image/png"))
        .then_some(normalized_type);
        record.info.artwork_revision = Some(revision);
        record.info.artwork_content_type = content_type.clone();
        record.artwork = content_type.map(|content_type| ArtworkSnapshot {
            session_id,
            revision,
            content_type,
            data,
        });
        Some((record.info.protocol, session_id, revision))
    }

    fn artwork(&self, session_id: u64, revision: u64) -> Option<ArtworkSnapshot> {
        self.lock()
            .by_session_id(session_id)?
            .artwork
            .as_ref()
            .filter(|artwork| artwork.revision == revision)
            .cloned()
    }

    /// The concrete sink is gone: revoke its leases, release its lane and
    /// retire the stream id.
    fn end_sink(&self, stream_id: u64, id: u64, lane: Option<&PcmBus>) {
        let removed = {
            let mut status = self.lock();
            let matches = status
                .sessions
                .get(&stream_id)
                .is_some_and(|record| record.info.id == id);
            let removed = if matches {
                status.sessions.remove(&stream_id)
            } else {
                None
            };
            if let Some(record) = removed.as_ref() {
                // Revoke credentials before releasing PCM ownership so no
                // queued callback can acquire a fresh lease meanwhile.
                record.revoke_leases();
                status.retire(stream_id);
            }
            if let Some(lane) = lane {
                lane.end(id);
            }
            removed
        };
        if let Some(record) = removed {
            self.events.send(AirPlayEvent::SessionEnded {
                protocol: record.info.protocol,
                session_id: Some(id),
            });
        }
    }

    /// The protocol published the stream's end. Credentials die now; the
    /// record stays until its sink drains so the lane is released exactly
    /// once. A record that never got a sink is removed at once.
    fn end_upstream_session(&self, stream_id: u64) {
        let mut status = self.lock();
        let Some(record) = status.sessions.get_mut(&stream_id) else {
            status.retire(stream_id);
            return;
        };
        record.upstream_ended = true;
        record.remote = None;
        record.event_volume = None;
        record.revoke_leases();
        if !record.holds_lane() {
            status.sessions.remove(&stream_id);
            status.retire(stream_id);
        }
    }

    fn set_receiver_volume_control(&self, stream_id: u64, sender: Option<EventCommandSender>) {
        let mut status = self.lock();
        let Some(record) = status.current_mut(stream_id) else {
            return;
        };
        if record.event_volume != sender {
            record.event_volume = sender;
            bump(&record.event_revision);
        }
    }

    fn remote_control_snapshot(&self, session_id: u64) -> Option<RemoteControlSnapshot> {
        self.lock().by_session_id(session_id)?.remote_snapshot()
    }

    fn receiver_volume_control_snapshot(
        &self,
        session_id: u64,
    ) -> Option<ReceiverVolumeControlSnapshot> {
        self.lock().by_session_id(session_id)?.event_volume_snapshot()
    }

    fn sender_volume_snapshots(&self) -> Vec<SenderVolumeSnapshot> {
        let status = self.lock();
        let mut records: Vec<&SessionRecord> = status
            .sessions
            .values()
            .filter(|record| record.holds_lane())
            .collect();
        records.sort_by_key(|record| record.info.lane);
        records
            .into_iter()
            .filter_map(SessionRecord::sender_volume_snapshot)
            .collect()
    }

    fn live_session_ids(&self) -> Vec<u64> {
        let status = self.lock();
        let mut live: Vec<(u8, u64)> = status
            .sessions
            .values()
            .filter_map(|record| Some((record.info.lane?, record.info.id)))
            .collect();
        live.sort_unstable();
        live.into_iter().map(|(_, id)| id).collect()
    }

    fn snapshot(&self) -> RuntimeStatus {
        let status = self.lock();
        let sessions = status
            .sessions
            .values()
            .map(|record| record.info.clone())
            .collect();
        let ptp_backend = if self.ptp_portable {
            Some("portable")
        } else {
            crate::protocol::media::ptp::ptp_backend_status().kind_available
        };
        RuntimeStatus {
            phase: status.phase,
            airplay2_port: self.ports.get(&Protocol::AirPlay2).copied(),
            sessions,
            last_error: status.last_error.clone(),
            ptp_backend: ptp_backend.map(str::to_owned),
        }
    }
}

struct PortReservation {
    listener: TcpListener,
    port: u16,
}

struct NativeAp2Receiver {
    listener: TcpListener,
    config: Arc<ServerConfig>,
}

impl PortReservation {
    fn acquire(requested: u16) -> io::Result<Self> {
        // The current AP2 control, PTP and media implementation is one IPv4
        // capability. Binding only IPv4 is intentional; the daemon mirrors
        // this at the DNS-SD boundary instead of advertising unusable AAAA
        // routes merely because the host has an IPv6 address.
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, requested)).map_err(|e| {
            io::Error::new(
                e.kind(),
                ControlPortBindError(io::Error::new(e.kind(), format!("port {requested}: {e}"))),
            )
        })?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn airplay2_instance_name(full_name: &str) -> String {
    utf8_prefix(full_name, DNS_LABEL_MAX_BYTES).to_string()
}

/// Running AirPlay 2 receiver. Dropping it synchronously stops the listener
/// future and its private Tokio runtime.
pub struct AirPlayRuntime {
    shared: Arc<Shared>,
    services: Vec<MdnsService>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl fmt::Debug for AirPlayRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AirPlayRuntime")
            .field("status", &self.status())
            .field("services", &self.services)
            .finish_non_exhaustive()
    }
}

impl AirPlayRuntime {
    /// Build and start the native AirPlay 2 listener. The method returns only
    /// after its concrete port accepts a loopback connection, or returns the
    /// bind/start failure. Advertisement remains daemon-owned.
    pub fn start(config: AirPlayConfig) -> io::Result<Self> {
        // This method runs on the daemon/controller thread, before the network
        // runtime or PCM callback can start. UUID and OnceLock initialization
        // therefore never occurs on either audio path.
        crate::telemetry::prewarm();
        let config = config.validate()?;
        let mut ports = BTreeMap::new();
        let identity_path = config
            .airplay2_identity_path
            .clone()
            .expect("validated AirPlay 2 identity path");
        let receiver_volume_provider = match config.receiver_volume_provider.clone() {
            Some(provider) => Some(provider),
            None => config
                .initial_volume_db
                .map(ReceiverVolumeSnapshot::from_legacy_db)
                .transpose()?
                .map(ReceiverVolumeProvider::fixed),
        };
        let server_config = Arc::new(ServerConfig::load(
            config.name.clone(),
            config.mac,
            config.password.clone(),
            identity_path,
            config.ptp_test_ephemeral,
            receiver_volume_provider,
        )?);
        // Reserve the requested control port only after all fallible receiver
        // configuration has loaded. The daemon may safely classify a marked
        // error from this one operation as a persisted-control-port conflict;
        // identity and other startup failures can never inherit that marker.
        let reservation = PortReservation::acquire(config.airplay2_port)?;
        ports.insert(Protocol::AirPlay2, reservation.port);
        let services = vec![
            MdnsService {
                protocol: Protocol::AirPlay2,
                service_type: "_airplay._tcp.local.".to_string(),
                instance_name: airplay2_instance_name(&config.name),
                port: reservation.port,
                txt_records: server_config.txt_records(),
            },
            MdnsService {
                protocol: Protocol::AirPlay1,
                service_type: "_raop._tcp.local.".to_string(),
                instance_name: airplay2_instance_name(&server_config.classic_instance_name()),
                port: reservation.port,
                txt_records: server_config.classic_txt_records(),
            },
        ];
        reservation.listener.set_nonblocking(true)?;
        let ap2 = NativeAp2Receiver {
            listener: reservation.listener,
            config: server_config,
        };

        let lanes = PcmLanes::new(config.bus);
        let shared = Arc::new(Shared::new(lanes, ports, config.ptp_test_ephemeral));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("audiohub-airplay".to_string())
            .spawn(move || run_thread(thread_shared, ap2, shutdown_rx, startup_tx))?;

        match startup_rx.recv_timeout(START_TIMEOUT + Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(Self {
                shared,
                services,
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }),
            Ok(Err((kind, message))) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                Err(io::Error::new(kind, message))
            }
            Err(e) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("AirPlay runtime did not become ready: {e}"),
                ))
            }
        }
    }

    /// The per-sender PCM lanes. Each concurrently playing sender owns one
    /// lane; the daemon mixes every active lane.
    pub fn pcm_lanes(&self) -> PcmLanes {
        self.shared.lanes.clone()
    }

    /// Subscribe to normalized protocol events. Status remains the source of
    /// truth for a subscriber joining after a session began.
    pub fn subscribe_events(&self) -> mpsc::Receiver<AirPlayEvent> {
        self.shared.events.subscribe()
    }

    /// mDNS records for the daemon to register independently.
    pub fn mdns_services(&self) -> &[MdnsService] {
        &self.services
    }

    /// Current runtime/session snapshot with no secret fields.
    pub fn status(&self) -> RuntimeStatus {
        self.shared.snapshot()
    }

    /// Fetch one exact artwork revision reported by [`Self::status`]. Stale
    /// revisions and ended sessions return `None`, so callers cannot attach a
    /// delayed image to a replacement sender.
    pub fn artwork(&self, session_id: u64, revision: u64) -> Option<ArtworkSnapshot> {
        self.shared.artwork(session_id, revision)
    }

    /// Session ids currently holding a PCM lane, in lane order.
    pub fn live_session_ids(&self) -> Vec<u64> {
        self.shared.live_session_ids()
    }

    /// Latest sender-authored volume of every lane-holding session, in lane
    /// order. Unlike the bounded event stream, these snapshots cannot lose a
    /// final command; a new identical dB value is observable through its
    /// higher revision, and `order` ranks commands across sessions.
    pub fn sender_volume_snapshots(&self) -> Vec<SenderVolumeSnapshot> {
        self.shared.sender_volume_snapshots()
    }

    /// Secret-bearing DACP snapshot of one session together with its
    /// revocable lease. Daemon-internal: never serialize or expose it through
    /// IPC, and re-check the lease immediately before an authenticated
    /// request.
    pub fn remote_control_snapshot(&self, session_id: u64) -> Option<RemoteControlSnapshot> {
        self.shared.remote_control_snapshot(session_id)
    }

    /// AP2 event-channel volume capability of one session: the preferred
    /// reverse path; DACP remains the fallback for traditional credentials.
    pub fn receiver_volume_control_snapshot(
        &self,
        session_id: u64,
    ) -> Option<ReceiverVolumeControlSnapshot> {
        self.shared.receiver_volume_control_snapshot(session_id)
    }

    /// Stop listeners and wait for the private runtime thread to exit.
    pub fn stop(mut self) -> io::Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let result = match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| io::Error::other("AirPlay runtime thread panicked"))?,
            None => Ok(()),
        };
        if result.is_ok() {
            self.shared.set_phase(RuntimePhase::Stopped, None);
        }
        result
    }
}

impl Drop for AirPlayRuntime {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

fn run_thread(
    shared: Arc<Shared>,
    ap2: NativeAp2Receiver,
    shutdown: oneshot::Receiver<()>,
    startup: mpsc::SyncSender<Result<(), (io::ErrorKind, String)>>,
) -> io::Result<()> {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKER_THREADS)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let message = format!("cannot build AirPlay async runtime: {e}");
            shared.set_phase(RuntimePhase::Failed, Some(message.clone()));
            let _ = startup.send(Err((e.kind(), message.clone())));
            return Err(io::Error::new(e.kind(), message));
        }
    };

    runtime.block_on(async move {
        let (failure_tx, mut failure_rx) = tokio::sync::mpsc::channel(1);
        let mut listener_tasks = Vec::new();
        let mut event_tasks = Vec::new();

        let (event_tx, event_rx) = tokio::sync::mpsc::channel(UPSTREAM_EVENT_QUEUE_CAPACITY);
        let (volume_tx, volume_rx) = tokio::sync::watch::channel(Ap2VolumeMap::new());
        let (remote_control_tx, remote_control_rx) =
            tokio::sync::watch::channel(Ap2RemoteControlMap::new());
        let sink_shared = Arc::clone(&shared);
        let output_factory: PcmOutputFactory =
            Arc::new(move |stream_id, peer, protocol, timing| {
            let ingress =
                PcmIngress::new(Arc::clone(&sink_shared), stream_id, peer, protocol, timing);
            if let Some(session) = sink_shared.session(stream_id, ingress.session_id) {
                sink_shared
                    .events
                    .send(AirPlayEvent::SessionStarted { session });
            }
            Box::new(Ap2Sink(ingress))
        });
        let failures = failure_tx.clone();
        listener_tasks.push(tokio::spawn(async move {
            let result = server::serve(
                ap2.listener,
                ap2.config,
                event_tx,
                volume_tx,
                remote_control_tx,
                output_factory,
            )
            .await;
            let _ = failures.send((Protocol::AirPlay2, result)).await;
        }));
        let event_shared = Arc::clone(&shared);
        event_tasks.push(tokio::spawn(async move {
            forward_ap2(event_rx, volume_rx, remote_control_rx, event_shared).await;
        }));
        drop(failure_tx);

        let startup_result = wait_until_listening(&shared, &mut failure_rx).await;
        if let Err(e) = startup_result {
            let kind = e.kind();
            let message = e.to_string();
            shared.set_phase(RuntimePhase::Failed, Some(message.clone()));
            let _ = startup.send(Err((kind, message.clone())));
            for task in listener_tasks.iter().chain(event_tasks.iter()) {
                task.abort();
            }
            return Err(io::Error::new(kind, message));
        }

        shared.set_phase(RuntimePhase::Listening, None);
        let _ = startup.send(Ok(()));
        let outcome = tokio::select! {
            _ = shutdown => Ok(()),
            failure = failure_rx.recv() => {
                match failure {
                    Some((protocol, Ok(()))) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{protocol:?} listener stopped unexpectedly"),
                    )),
                    Some((protocol, Err(e))) => Err(io::Error::new(
                        e.kind(),
                        format!("{protocol:?} listener failed: {e}"),
                    )),
                    None => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "all AirPlay listeners stopped unexpectedly",
                    )),
                }
            }
        };

        for task in listener_tasks.iter().chain(event_tasks.iter()) {
            task.abort();
        }
        if let Err(e) = &outcome {
            let message = e.to_string();
            shared.set_phase(RuntimePhase::Failed, Some(message.clone()));
            shared.events.send(AirPlayEvent::RuntimeFailed { message });
        } else {
            shared.set_phase(RuntimePhase::Stopped, None);
        }
        outcome
    })
}

async fn wait_until_listening(
    shared: &Shared,
    failures: &mut tokio::sync::mpsc::Receiver<(Protocol, io::Result<()>)>,
) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut pending: BTreeMap<Protocol, u16> = shared.ports.clone();
    while !pending.is_empty() {
        let protocols: Vec<(Protocol, u16)> = pending.iter().map(|(p, v)| (*p, *v)).collect();
        for (protocol, port) in protocols {
            if probe_listener(port).await {
                pending.remove(&protocol);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        tokio::select! {
            failure = failures.recv() => {
                return match failure {
                    Some((protocol, Ok(()))) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{protocol:?} listener stopped during startup"),
                    )),
                    Some((protocol, Err(e))) => Err(io::Error::new(
                        e.kind(),
                        format!("{protocol:?} listener failed during startup: {e}"),
                    )),
                    None => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "AirPlay listeners stopped during startup",
                    )),
                };
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let names = pending
                .keys()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("AirPlay listeners did not accept connections: {names}"),
            ));
        }
    }
    Ok(())
}

async fn probe_listener(port: u16) -> bool {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    tokio::net::TcpStream::connect(v4).await.is_ok()
}

struct PcmIngress {
    stream_id: u64,
    shared: Arc<Shared>,
    session_id: u64,
    /// This session's own PCM lane; `None` only if no lane was free.
    lane: Option<PcmBus>,
    converter: StreamingStereoPcmConverter,
    staged: Vec<[f32; 2]>,
}

impl PcmIngress {
    #[cfg(test)]
    fn new_ap2(shared: Arc<Shared>, upstream_session_id: u64, peer: SocketAddr) -> Self {
        Self::new(
            shared,
            upstream_session_id,
            peer,
            Protocol::AirPlay2,
            SessionTiming::Unknown,
        )
    }

    fn new(
        shared: Arc<Shared>,
        stream_id: u64,
        peer: SocketAddr,
        protocol: Protocol,
        timing: SessionTiming,
    ) -> Self {
        let rate = 44_100;
        let channels = 2;
        let (session_id, lane) = shared.activate_sink(
            protocol,
            stream_id,
            rate,
            channels,
            Some(normalize_socket_addr(peer).ip()),
            timing,
        );
        let converter = StreamingStereoPcmConverter::new(rate, channels).unwrap_or_else(|_| {
            // The AP2 media engine guarantees nonzero 44.1k stereo.
            // A defensive fallback keeps their playback thread alive while
            // status/events still expose the bad negotiated format.
            StreamingStereoPcmConverter::new(44_100, 2).expect("constant format is valid")
        });
        Self {
            stream_id,
            shared,
            session_id,
            lane,
            converter,
            staged: Vec::new(),
        }
    }

    fn correction_ppm(&self) -> f64 {
        self.lane.as_ref().map_or(0.0, PcmBus::correction_ppm)
    }

    fn write(&mut self, pcm: &[i16], telemetry_token: crate::telemetry::OperationToken) {
        self.write_timed(pcm, telemetry_token, None);
    }

    fn write_timed(&mut self, pcm: &[i16], telemetry_token: crate::telemetry::OperationToken, timing: Option<crate::bus::PcmWriteTiming>) {
        self.staged.clear();
        self.converter.set_correction_ppm(self.correction_ppm());
        let converted = self.converter.process_i16_with_timing(pcm, &mut self.staged);
        let mapped = timing.zip(converted).and_then(|(timing, converted)| {
            // Each input block carries a fresh source-clock presentation.
            // Include the FIR history and fractional phase, not just its
            // nominal delay or the cumulative count of converted frames.
            let offset = Duration::from_secs_f64(converted.offset_seconds.abs());
            let presentation = if converted.offset_seconds < 0.0 {
                timing.presentation.checked_sub(offset)
            } else {
                timing.presentation.checked_add(offset)
            }?;
            Some((crate::bus::PcmWriteTiming { epoch: timing.epoch, presentation }, converted.frame_seconds))
        });
        let Some(lane) = self.lane.as_ref() else {
            return;
        };
        // The lane rejects writes once this session no longer owns it.
        lane.push_resampled_with_token(self.session_id, &self.staged, telemetry_token,
            mapped.map(|(timing, _)| timing),
            mapped.map_or(1.0 / crate::AUDIOHUB_RATE as f64, |(_, seconds)| seconds));
    }

    fn flush(&mut self) {
        self.converter.flush();
        if let Some(lane) = self.lane.as_ref() {
            lane.flush(self.session_id);
        }
    }
}

impl Drop for PcmIngress {
    fn drop(&mut self) {
        self.shared
            .end_sink(self.stream_id, self.session_id, self.lane.as_ref());
    }
}

struct Ap2Sink(PcmIngress);

impl PcmOutput for Ap2Sink {
    fn discard(&mut self, pcm: &[i16], _token: crate::telemetry::OperationToken) {
        self.0.staged.clear();
        let correction = self.0.correction_ppm();
        self.0.converter.set_correction_ppm(correction);
        self.0.converter.process_i16(pcm, &mut self.0.staged);
        self.0.staged.clear();
    }
    fn write(&mut self, pcm: &[i16], telemetry_token: crate::telemetry::OperationToken) {
        self.0.write(pcm, telemetry_token);
    }

    fn flush(&mut self) {
        self.0.flush();
    }

    fn write_timed(&mut self, pcm: &[i16], telemetry_token: crate::telemetry::OperationToken, timing: crate::bus::PcmWriteTiming) {
        self.0.write_timed(pcm, telemetry_token, Some(timing));
    }

    fn output_timing(&self) -> Option<crate::bus::OutputTiming> {
        let mut timing = self.0.lane.as_ref()?.output_timing()?;
        timing.lead = timing.lead.checked_add(self.0.converter.delay())?;
        Some(timing)
    }
}

/// Forward the protocol's per-stream watches and milestone queue into the
/// session model. Both watches are maps keyed by stream id; each change is
/// diffed against the previous value so every stream is forwarded
/// independently and a removed stream ends exactly that session.
async fn forward_ap2(
    mut receiver: tokio::sync::mpsc::Receiver<ProbeEvent>,
    mut latest_volume: tokio::sync::watch::Receiver<Ap2VolumeMap>,
    mut latest_remote_control: tokio::sync::watch::Receiver<Ap2RemoteControlMap>,
    shared: Arc<Shared>,
) {
    let mut events_open = true;
    let mut volume_open = true;
    let mut remote_control_open = true;
    let mut seen_remote = Ap2RemoteControlMap::new();
    let mut seen_volume = Ap2VolumeMap::new();
    while events_open || volume_open || remote_control_open {
        tokio::select! {
            biased;
            changed = latest_remote_control.changed(), if remote_control_open => {
                match changed {
                    Ok(()) => {
                        let current = latest_remote_control.borrow_and_update().clone();
                        forward_remote_control_diff(&shared, &seen_remote, &current);
                        seen_remote = current;
                    }
                    Err(_) => remote_control_open = false,
                }
            }
            changed = latest_volume.changed(), if volume_open => {
                match changed {
                    Ok(()) => {
                        let current = latest_volume.borrow_and_update().clone();
                        for (stream_id, volume) in &current {
                            if seen_volume.get(stream_id) != Some(volume) {
                                forward_volume(&shared, *volume);
                            }
                        }
                        seen_volume = current;
                    }
                    Err(_) => volume_open = false,
                }
            }
            event = receiver.recv(), if events_open => {
                match event {
                    Some(ProbeEvent::Flushed { stream_id, .. }) => {
                        if let Some((protocol, session_id)) = shared.session_id_versioned(stream_id) {
                            shared.events.send(AirPlayEvent::Flushed {
                                protocol,
                                session_id: Some(session_id),
                            });
                        }
                    }
                    Some(ProbeEvent::Metadata {
                        stream_id,
                        title,
                        artist,
                        album,
                        ..
                    }) => {
                        if let Some((protocol, session_id)) = shared.update_metadata_versioned(
                            stream_id,
                            title.clone(),
                            artist.clone(),
                            album.clone(),
                        ) {
                            shared.events.send(AirPlayEvent::Metadata {
                                protocol,
                                session_id: Some(session_id),
                                title,
                                artist,
                                album,
                            });
                        }
                    }
                    Some(ProbeEvent::Artwork {
                        stream_id,
                        content_type,
                        data,
                        ..
                    }) => {
                        if let Some((protocol, session_id, revision)) =
                            shared.update_artwork_versioned(stream_id, content_type.clone(), data)
                        {
                            let normalized_type = content_type.to_ascii_lowercase();
                            shared.events.send(AirPlayEvent::Artwork {
                                protocol,
                                session_id: Some(session_id),
                                content_type: matches!(
                                    normalized_type.as_str(),
                                    "image/jpeg" | "image/png"
                                )
                                .then_some(normalized_type),
                                revision,
                            });
                        }
                    }
                    Some(ProbeEvent::Progress {
                        stream_id,
                        elapsed_ms,
                        duration_ms,
                        ..
                    }) => {
                        if let Some((protocol, session_id)) =
                            shared.update_progress_versioned(stream_id, elapsed_ms, duration_ms)
                        {
                            if let (Some(elapsed_ms), Some(duration_ms)) =
                                (elapsed_ms, duration_ms)
                            {
                                shared.events.send(AirPlayEvent::Progress {
                                    protocol,
                                    session_id: Some(session_id),
                                    elapsed_ms,
                                    duration_ms,
                                });
                            }
                        }
                    }
                    Some(ProbeEvent::Paused { stream_id, paused, .. }) => {
                        if let Some((protocol, session_id)) =
                            shared.update_paused_versioned(stream_id, paused)
                        {
                            shared.events.send(AirPlayEvent::Paused {
                                protocol,
                                session_id: Some(session_id),
                                paused,
                            });
                        }
                    }
                    Some(event) => {
                        log::info!(
                            target: "audiohub_airplay::airplay2",
                            "AirPlay protocol milestone: {event:?}"
                        );
                    }
                    None => events_open = false,
                }
            }
        }
    }
}

fn forward_volume(shared: &Shared, volume: Ap2VolumeUpdate) {
    if let Some(session_id) =
        shared.update_volume_versioned(volume.stream_id, volume.revision, volume.order, volume.db)
    {
        shared.events.send(AirPlayEvent::Volume {
            protocol: volume.protocol,
            session_id: Some(session_id),
            db: volume.db,
        });
    }
}

fn forward_remote_control_diff(
    shared: &Shared,
    previous: &Ap2RemoteControlMap,
    current: &Ap2RemoteControlMap,
) {
    for stream_id in previous.keys() {
        if !current.contains_key(stream_id) {
            shared.end_upstream_session(*stream_id);
        }
    }
    for (stream_id, state) in current {
        if previous.get(stream_id) != Some(state) {
            forward_ap2_remote_control(shared, state.clone());
        }
    }
}

fn forward_ap2_remote_control(shared: &Shared, state: Ap2RemoteControlState) {
    let Ap2RemoteControlState {
        protocol,
        stream_id,
        peer,
        credentials,
        event_commands,
    } = state;
    let peer = normalize_socket_addr(peer);
    let remote_control = credentials.map(|credentials| RemoteControlInfo {
        session_id: 0,
        peer,
        dacp_id: credentials.dacp_id,
        active_remote: credentials.active_remote,
    });
    let session = shared.ensure_session_versioned(
        protocol,
        stream_id,
        44_100,
        2,
        Some(peer.ip()),
        Some(remote_control),
    );
    if session.is_some() {
        shared.set_receiver_volume_control(stream_id, event_commands);
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        other => other,
    }
}

fn normalize_socket_addr(addr: SocketAddr) -> SocketAddr {
    match normalize_ip(addr.ip()) {
        IpAddr::V4(ip) => SocketAddr::new(IpAddr::V4(ip), addr.port()),
        IpAddr::V6(_) => addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::server::RemoteControlCredentials;

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared::new(
            PcmLanes::new(BusConfig {
                capacity_samples: 960,
                prebuffer_samples: 0,
                max_correction_ppm: 500,
            }),
            BTreeMap::from([(Protocol::AirPlay2, 7000)]),
            true,
        ))
    }

    fn test_peer() -> SocketAddr {
        std::net::SocketAddrV6::new("fe80::1234".parse().unwrap(), 7000, 0, 7).into()
    }

    fn activate(shared: &Shared, protocol: Protocol, stream_id: u64) -> u64 {
        shared
            .activate_sink(
                protocol,
                stream_id,
                44_100,
                2,
                Some(test_peer().ip()),
                SessionTiming::Unknown,
            )
            .0
    }

    fn lane_of(shared: &Shared, stream_id: u64) -> Option<u8> {
        shared.lock().sessions.get(&stream_id)?.info.lane
    }

    fn lane_bus(shared: &Shared, stream_id: u64) -> PcmBus {
        let lane = lane_of(shared, stream_id).expect("session holds a lane");
        shared.lanes.lane(usize::from(lane)).unwrap().clone()
    }

    fn end(shared: &Shared, stream_id: u64, session_id: u64) {
        let lane = lane_of(shared, stream_id)
            .and_then(|lane| shared.lanes.lane(usize::from(lane)).cloned());
        shared.end_sink(stream_id, session_id, lane.as_ref());
    }

    #[test]
    fn timed_sink_subtracts_converter_delay_and_reanchors_after_flush() {
        let shared = test_shared();
        let mut sink = Ap2Sink(PcmIngress::new_ap2(Arc::clone(&shared), 93, test_peer()));
        let mut reader = lane_bus(&shared, 93).subscribe();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 9, None);
        let first_input = std::time::Instant::now();
        let pcm = [8192, -8192].repeat(128);
        for presentation in [first_input, first_input + Duration::from_millis(10)] {
            sink.flush();
            sink.write_timed(&pcm, crate::telemetry::operation_token(), crate::bus::PcmWriteTiming { epoch: 9, presentation });
            let next_output = presentation - sink.0.converter.delay() - Duration::from_nanos(62_500);
            let read = reader.read_stereo_timed(&mut out, 9, Some(next_output));
            assert_eq!((read.copied, read.silence, read.late), (5, 3, 0));
            assert_eq!(&out[..3], &[[0.0; 2]; 3]);
            assert_eq!(&out[3..], &[[0.25, -0.25]; 5]);
        }
    }

    #[test]
    fn telemetry_reset_preserves_stereo_ingress_state_and_old_token_pcm() {
        // Global recorder: serialized against concurrent `telemetry::reset`.
        let _telemetry = crate::telemetry::test_serial();
        let shared = test_shared();
        let mut ingress = PcmIngress::new_ap2(Arc::clone(&shared), 91, test_peer());
        let mut reader = lane_bus(&shared, 91).subscribe();
        let pre_reset_token = crate::telemetry::operation_token();

        ingress.write(&[16_384], pre_reset_token);
        crate::telemetry::reset();
        ingress.write(&[-16_384], pre_reset_token);

        let mut out = [[0.0; 2]; 1];
        let read = reader.read_stereo_into(&mut out);
        assert_eq!(read.copied, 1);
        assert_eq!(read.session_id, Some(ingress.session_id));
        assert_eq!(out, [[0.5, -0.5]]);
    }

    #[test]
    fn sink_timing_counts_its_filter_delay_once_before_and_after_flush() {
        let shared = test_shared();
        shared
            .lanes
            .ensure_lane(0)
            .publish_output_timing(7, Duration::from_millis(25), false);
        let mut sink = Ap2Sink(PcmIngress::new_ap2(Arc::clone(&shared), 92, test_peer()));
        let expected = Duration::from_millis(25) + Duration::from_secs_f64(64.0 / 44_100.0);
        let timing = sink.output_timing().unwrap();
        assert_eq!((timing.epoch, timing.lead), (7, expected));
        sink.flush();
        assert_eq!(sink.output_timing().unwrap().lead, expected);
    }

    fn remote_state(stream_id: u64, dacp_id: &str, active_remote: &str) -> Ap2RemoteControlState {
        Ap2RemoteControlState {
            protocol: Protocol::AirPlay2,
            stream_id,
            peer: test_peer(),
            credentials: Some(RemoteControlCredentials {
                dacp_id: dacp_id.into(),
                active_remote: active_remote.into(),
            }),
            event_commands: None,
        }
    }

    fn volume(stream_id: u64, revision: u64, order: u64, db: f32) -> Ap2VolumeUpdate {
        Ap2VolumeUpdate {
            protocol: Protocol::AirPlay2,
            stream_id,
            revision,
            order,
            db,
        }
    }

    fn apply_volume(shared: &Shared, update: Ap2VolumeUpdate) -> Option<u64> {
        let session_id = shared.update_volume_versioned(
            update.stream_id,
            update.revision,
            update.order,
            update.db,
        );
        if session_id.is_some() {
            shared.events.send(AirPlayEvent::Volume {
                protocol: update.protocol,
                session_id,
                db: update.db,
            });
        }
        session_id
    }

    async fn run_forwarder(
        shared: &Arc<Shared>,
        events: Vec<ProbeEvent>,
        volumes: Vec<Ap2VolumeMap>,
        remotes: Vec<Ap2RemoteControlMap>,
    ) {
        let (event_tx, event_rx) =
            tokio::sync::mpsc::channel(UPSTREAM_EVENT_QUEUE_CAPACITY.max(events.len()));
        for event in events {
            event_tx.try_send(event).unwrap();
        }
        let (volume_tx, volume_rx) = tokio::sync::watch::channel(Ap2VolumeMap::new());
        for value in volumes {
            volume_tx.send(value).unwrap();
        }
        let (remote_tx, remote_rx) = tokio::sync::watch::channel(Ap2RemoteControlMap::new());
        for value in remotes {
            remote_tx.send(value).unwrap();
        }
        drop(event_tx);
        drop(volume_tx);
        drop(remote_tx);
        tokio::time::timeout(
            Duration::from_secs(1),
            forward_ap2(event_rx, volume_rx, remote_rx, Arc::clone(shared)),
        )
        .await
        .expect("forwarder did not close after its inputs closed");
    }

    #[test]
    fn native_default_is_enabled_on_an_automatic_port() {
        let config = AirPlayConfig::new("Test Receiver");
        assert!(config.enable_airplay2);
        assert_eq!(config.airplay2_port, 0);
        assert_eq!(config.password, None);
    }

    #[test]
    fn receiver_volume_provider_is_live_validated_and_thread_safe() {
        let reads = Arc::new(AtomicU64::new(0));
        let provider_reads = Arc::clone(&reads);
        let provider = ReceiverVolumeProvider::new(move || {
            provider_reads.fetch_add(1, Ordering::SeqCst);
            ReceiverVolumeSnapshot::new(-12.5, true)
        });

        let workers = (0..4)
            .map(|_| {
                let provider = provider.clone();
                thread::spawn(move || provider.current().unwrap())
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert_eq!(
                worker.join().unwrap(),
                ReceiverVolumeSnapshot {
                    slider_db: -12.5,
                    is_muted: true,
                }
            );
        }
        assert_eq!(reads.load(Ordering::SeqCst), 4);

        let attempts = Arc::new(AtomicU64::new(0));
        let reader_attempts = Arc::clone(&attempts);
        let cached = ReceiverVolumeProvider::new(move || {
            if reader_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                ReceiverVolumeSnapshot::new(-9.0, false)
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "device changed"))
            }
        });
        assert_eq!(cached.current().unwrap().slider_db, -9.0);
        assert_eq!(cached.current().unwrap().slider_db, -9.0);

        let invalid = ReceiverVolumeProvider::new(|| {
            Ok(ReceiverVolumeSnapshot {
                slider_db: f32::NAN,
                is_muted: false,
            })
        });
        assert_eq!(
            invalid.current().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn legacy_mute_fallback_does_not_leak_into_airplay2_slider() {
        let snapshot = ReceiverVolumeSnapshot::from_legacy_db(-144.0).unwrap();
        assert_eq!(snapshot.slider_db, -30.0);
        assert!(snapshot.is_muted);
        assert_eq!(snapshot.legacy_text_db(), -144.0);
    }

    #[test]
    fn control_port_reservation_is_explicitly_ipv4() {
        let reservation = PortReservation::acquire(0).unwrap();
        assert!(reservation.listener.local_addr().unwrap().is_ipv4());
        assert_ne!(reservation.port, 0);
    }

    #[test]
    fn control_port_bind_failure_has_a_phase_marker() {
        let occupied = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        let error = PortReservation::acquire(port)
            .err()
            .expect("occupied control port must fail");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(is_control_port_bind_error(&error));
    }

    #[test]
    fn disabled_receiver_and_missing_identity_are_rejected() {
        let mut disabled = AirPlayConfig::new("Disabled");
        disabled.enable_airplay2 = false;
        let error = disabled.validate().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let error = AirPlayConfig::new("Missing Identity")
            .validate()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("stable identity"));
    }

    #[test]
    fn slow_event_subscriber_drops_excess_at_the_bounded_limit() {
        let hub = EventHub::default();
        let rx = hub.subscribe();
        for index in 0..=EVENT_QUEUE_CAPACITY {
            hub.send(AirPlayEvent::RuntimeFailed {
                message: format!("event-{index}"),
            });
        }

        assert_eq!(
            hub.subscribers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );
        assert_eq!(rx.try_iter().count(), EVENT_QUEUE_CAPACITY);
        hub.send(AirPlayEvent::RuntimeFailed {
            message: "after-drain".to_string(),
        });
        assert!(matches!(
            rx.try_recv(),
            Ok(AirPlayEvent::RuntimeFailed { message }) if message == "after-drain"
        ));
    }

    #[test]
    fn saturated_event_queue_preserves_authoritative_volume_status() {
        let shared = test_shared();
        let session_id = activate(&shared, Protocol::AirPlay2, 1);
        let rx = shared.events.subscribe();
        for index in 0..EVENT_QUEUE_CAPACITY {
            shared.events.send(AirPlayEvent::RuntimeFailed {
                message: format!("filler-{index}"),
            });
        }

        let updated_session = shared.update_volume(1, -6.0);
        assert_eq!(updated_session, Some(session_id));
        shared.events.send(AirPlayEvent::Volume {
            protocol: Protocol::AirPlay2,
            session_id: updated_session,
            db: -6.0,
        });

        assert_eq!(rx.try_iter().count(), EVENT_QUEUE_CAPACITY);
        let status = shared.snapshot();
        assert_eq!(status.sessions.len(), 1);
        assert_eq!(status.sessions[0].id, session_id);
        assert_eq!(status.sessions[0].volume_db, Some(-6.0));
        assert_eq!(
            shared.sender_volume_snapshots(),
            vec![SenderVolumeSnapshot {
                session_id,
                db: -6.0,
                revision: 1,
                order: 1,
            }]
        );
    }

    #[tokio::test]
    async fn saturated_probe_queue_still_forwards_latest_volume() {
        let shared = test_shared();
        let session_id = activate(&shared, Protocol::AirPlay2, 1);
        let events = shared.events.subscribe();
        let fillers = (0..UPSTREAM_EVENT_QUEUE_CAPACITY)
            .map(|_| ProbeEvent::PairingComplete {
                peer: Ipv4Addr::LOCALHOST.into(),
            })
            .collect();
        let volumes = [(1, 1, -20.0), (2, 2, -12.0), (3, 3, -6.0)]
            .into_iter()
            .map(|(revision, order, db)| Ap2VolumeMap::from([(1, volume(1, revision, order, db))]))
            .collect();
        run_forwarder(&shared, fillers, volumes, Vec::new()).await;

        let status = shared.snapshot();
        assert_eq!(status.sessions[0].id, session_id);
        assert_eq!(status.sessions[0].volume_db, Some(-6.0));
        assert_eq!(shared.sender_volume_snapshots()[0].revision, 3);
        let delivered: Vec<f32> = events
            .try_iter()
            .filter_map(|event| match event {
                AirPlayEvent::Volume { db, .. } => Some(db),
                _ => None,
            })
            .collect();
        assert_eq!(delivered, vec![-6.0]);
    }

    #[tokio::test]
    async fn saturated_probe_queue_preserves_latest_remote_credentials() {
        let shared = test_shared();
        let fillers = (0..UPSTREAM_EVENT_QUEUE_CAPACITY)
            .map(|_| ProbeEvent::PairingComplete {
                peer: Ipv4Addr::LOCALHOST.into(),
            })
            .collect();
        let remotes = vec![
            Ap2RemoteControlMap::from([(1, remote_state(1, "A1", "111"))]),
            Ap2RemoteControlMap::from([(1, remote_state(1, "A1", "222"))]),
        ];
        run_forwarder(&shared, fillers, Vec::new(), remotes).await;

        let provisional = shared.session_id(1).unwrap();
        assert!(
            shared.remote_control_snapshot(provisional).is_none(),
            "a provisional session without a sink exposes no credentials"
        );
        let concrete = activate(&shared, Protocol::AirPlay2, 1);
        let snapshot = shared
            .remote_control_snapshot(concrete)
            .expect("credentials followed the concrete sink");
        assert_eq!(snapshot.info().session_id, concrete);
        assert_eq!(snapshot.info().active_remote, "222");
        assert!(snapshot.is_current());
    }

    #[test]
    fn credential_refresh_revokes_old_lease_without_crossing_streams() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        let concrete = activate(&shared, Protocol::AirPlay2, 1);
        let original = shared.remote_control_snapshot(concrete).unwrap();
        assert_eq!(original.info().session_id, concrete);

        forward_ap2_remote_control(&shared, remote_state(1, "A1", "222"));
        let token_refresh = shared.remote_control_snapshot(concrete).unwrap();
        assert!(!original.is_current());
        assert!(token_refresh.is_current());
        assert_eq!(token_refresh.info().peer, original.info().peer);
        assert_eq!(token_refresh.info().dacp_id, original.info().dacp_id);
        assert_eq!(token_refresh.info().active_remote, "222");

        forward_ap2_remote_control(&shared, remote_state(1, "B2", "333"));
        let id_refresh = shared.remote_control_snapshot(concrete).unwrap();
        assert!(!token_refresh.is_current());
        assert_eq!(id_refresh.info().dacp_id, "B2");
        assert_eq!(id_refresh.info().active_remote, "333");

        end(&shared, 1, concrete);
        assert!(!id_refresh.is_current(), "ending the session revokes its lease");
        forward_ap2_remote_control(&shared, remote_state(2, "C3", "444"));
        let replacement = activate(&shared, Protocol::AirPlay2, 2);
        shared.end_upstream_session(1);
        let current = shared.remote_control_snapshot(replacement).unwrap();
        assert_eq!(current.info().session_id, replacement);
        assert_eq!(current.info().dacp_id, "C3");
        assert_eq!(current.info().active_remote, "444");
        assert!(current.is_current());
    }

    #[test]
    fn leases_are_per_session_and_a_refresh_never_revokes_another_sender() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        forward_ap2_remote_control(&shared, remote_state(2, "B2", "222"));
        let a = activate(&shared, Protocol::AirPlay2, 1);
        let b = activate(&shared, Protocol::AirPlay2, 2);
        shared.set_receiver_volume_control(1, Some(EventCommandSender::test_ready()));
        shared.set_receiver_volume_control(2, Some(EventCommandSender::test_ready()));
        let a_remote = shared.remote_control_snapshot(a).unwrap();
        let b_remote = shared.remote_control_snapshot(b).unwrap();
        let a_event = shared.receiver_volume_control_snapshot(a).unwrap();
        let b_event = shared.receiver_volume_control_snapshot(b).unwrap();
        assert_eq!((a_event.session_id(), b_event.session_id()), (a, b));

        forward_ap2_remote_control(&shared, remote_state(1, "A1", "999"));
        assert!(!a_remote.is_current());
        assert!(b_remote.is_current(), "B's DACP lease survives A's refresh");
        assert!(b_event.is_current());

        shared.end_upstream_session(1);
        end(&shared, 1, a);
        assert!(!a_event.is_current());
        assert!(b_remote.is_current() && b_event.is_current(), "B survives A's end");
        assert_eq!(shared.live_session_ids(), vec![b]);
    }

    #[test]
    fn classic_and_ap2_sinks_coexist_and_ending_one_keeps_the_other() {
        let shared = test_shared();
        let classic = activate(&shared, Protocol::AirPlay1, 1);
        let ap2 = activate(&shared, Protocol::AirPlay2, 2);
        assert_eq!((lane_of(&shared, 1), lane_of(&shared, 2)), (Some(0), Some(1)));
        shared.set_receiver_volume_control(2, Some(EventCommandSender::test_ready()));
        let current = shared.receiver_volume_control_snapshot(ap2).unwrap();
        assert_eq!(current.session_id(), ap2);
        shared.set_receiver_volume_control(1, None);
        assert!(current.is_current());
        shared.end_upstream_session(1);
        assert!(current.is_current());
        end(&shared, 1, classic);
        assert!(current.is_current());
        let status = shared.snapshot();
        assert_eq!(status.sessions.len(), 1);
        assert_eq!(status.sessions[0].id, ap2);
        assert!(status.sessions[0].selected_for_output);
        assert_eq!(status.sessions[0].lane, Some(1));
        end(&shared, 2, ap2);
        assert!(!current.is_current());
        assert!(shared.receiver_volume_control_snapshot(ap2).is_none());
    }

    #[test]
    fn two_sinks_write_their_own_lanes_concurrently() {
        let shared = test_shared();
        let mut first = PcmIngress::new_ap2(Arc::clone(&shared), 1, test_peer());
        let mut second = PcmIngress::new(
            Arc::clone(&shared),
            2,
            test_peer(),
            Protocol::AirPlay1,
            SessionTiming::Ntp,
        );
        let mut first_reader = lane_bus(&shared, 1).subscribe();
        let mut second_reader = lane_bus(&shared, 2).subscribe();
        let token = crate::telemetry::operation_token();
        first.write(&[16_384, 16_384], token);
        second.write(&[-8_192, -8_192], token);
        let mut out = [[0.0; 2]; 1];
        let read = first_reader.read_stereo_into(&mut out);
        assert_eq!(read.session_id, Some(first.session_id));
        assert_eq!(out, [[0.5, 0.5]]);
        let read = second_reader.read_stereo_into(&mut out);
        assert_eq!(read.session_id, Some(second.session_id));
        assert_eq!(out, [[-0.25, -0.25]]);
        assert!(shared.lanes.any_active());
        let timing = shared.snapshot().sessions;
        assert_eq!(timing[1].timing, SessionTiming::Ntp);

        drop(first);
        assert_eq!(
            lane_bus(&shared, 2).active_session(),
            Some(second.session_id),
            "ending one session leaves the other playing"
        );
        assert_eq!(shared.live_session_ids(), vec![second.session_id]);
        // A third sender reuses the freed lowest lane.
        let third = PcmIngress::new_ap2(Arc::clone(&shared), 3, test_peer());
        assert_eq!(lane_of(&shared, 3), Some(0));
        drop(third);
        drop(second);
        assert!(!shared.lanes.any_active());
    }

    #[test]
    fn lane_exhaustion_discards_the_extra_sink_without_preempting() {
        // Global recorder: serialized against concurrent `telemetry::reset`.
        let _telemetry = crate::telemetry::test_serial();
        let shared = test_shared();
        let before = crate::telemetry::snapshot().counters.pcm_lane_exhausted;
        let ids: Vec<u64> = (1..=4)
            .map(|stream| activate(&shared, Protocol::AirPlay2, stream))
            .collect();
        let (extra, lane) = shared.activate_sink(
            Protocol::AirPlay2,
            5,
            44_100,
            2,
            None,
            SessionTiming::Unknown,
        );
        assert!(lane.is_none());
        assert_eq!(shared.live_session_ids(), ids, "no running session lost its lane");
        let status = shared.snapshot();
        let extra_info = status.sessions.iter().find(|s| s.id == extra).unwrap();
        assert!(!extra_info.selected_for_output);
        assert_eq!(extra_info.lane, None);
        assert!(
            crate::telemetry::snapshot().counters.pcm_lane_exhausted >= before.saturating_add(1)
        );
    }

    #[tokio::test]
    async fn two_active_states_published_back_to_back_both_arrive() {
        let shared = test_shared();
        let a = activate(&shared, Protocol::AirPlay2, 1);
        let b = activate(&shared, Protocol::AirPlay2, 2);
        // The watch coalesces both publications into one map value.
        let both = Ap2RemoteControlMap::from([
            (1, remote_state(1, "A1", "111")),
            (2, remote_state(2, "B2", "222")),
        ]);
        run_forwarder(&shared, Vec::new(), Vec::new(), vec![both]).await;
        assert_eq!(shared.remote_control_snapshot(a).unwrap().info().dacp_id, "A1");
        assert_eq!(shared.remote_control_snapshot(b).unwrap().info().dacp_id, "B2");
    }

    #[tokio::test]
    async fn removing_one_stream_from_the_map_ends_only_that_session() {
        let shared = test_shared();
        let a = activate(&shared, Protocol::AirPlay2, 1);
        let b = activate(&shared, Protocol::AirPlay2, 2);
        let both = Ap2RemoteControlMap::from([
            (1, remote_state(1, "A1", "111")),
            (2, remote_state(2, "B2", "222")),
        ]);
        let only_b = Ap2RemoteControlMap::from([(2, remote_state(2, "B2", "222"))]);
        let (remote_tx, remote_rx) = tokio::sync::watch::channel(Ap2RemoteControlMap::new());
        let (_volume_tx, volume_rx) = tokio::sync::watch::channel(Ap2VolumeMap::new());
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        let forwarder = tokio::spawn(forward_ap2(event_rx, volume_rx, remote_rx, Arc::clone(&shared)));
        remote_tx.send(both).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let a_lease = shared.remote_control_snapshot(a).unwrap();
        remote_tx.send(only_b).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!a_lease.is_current());
        assert!(shared.remote_control_snapshot(a).is_none());
        assert!(shared.remote_control_snapshot(b).unwrap().is_current());
        drop((remote_tx, event_tx, _volume_tx));
        tokio::time::timeout(Duration::from_secs(1), forwarder)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn delayed_old_state_cannot_capture_replacement_sink() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        let old_sink = activate(&shared, Protocol::AirPlay2, 1);
        end(&shared, 1, old_sink);

        let new_sink = activate(&shared, Protocol::AirPlay2, 2);
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "333"));
        forward_ap2_remote_control(&shared, remote_state(2, "B2", "444"));

        assert!(shared.session_id(1).is_none(), "a retired stream is not revived");
        let current = shared.remote_control_snapshot(new_sink).unwrap();
        assert_eq!(current.info().session_id, new_sink);
        assert_eq!(current.info().dacp_id, "B2");
        assert_eq!(current.info().active_remote, "444");
    }

    #[test]
    fn retired_ids_stay_dead_beyond_the_bounded_history() {
        let shared = test_shared();
        let first = activate(&shared, Protocol::AirPlay2, 1);
        let survivor = activate(&shared, Protocol::AirPlay2, 2);
        end(&shared, 1, first);
        for stream in 3..(3 + RETIRED_STREAM_LIMIT as u64 + 8) {
            shared.end_upstream_session(stream);
        }
        assert!(!shared.lock().retired.contains(&1), "evicted from the bounded deque");
        assert!(shared
            .ensure_session_versioned(Protocol::AirPlay2, 1, 44_100, 2, None, None)
            .is_none());
        // A live session older than the floor still accepts updates.
        assert!(shared
            .ensure_session_versioned(Protocol::AirPlay2, 2, 44_100, 2, None, None)
            .is_some_and(|session| session.id == survivor));
        assert!(shared.update_metadata_versioned(2, Some("t".into()), None, None).is_some());
    }

    #[test]
    fn upstream_end_without_a_sink_removes_the_provisional_session() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(7, "A1", "111"));
        assert!(shared.session_id(7).is_some());
        shared.end_upstream_session(7);
        assert!(shared.session_id(7).is_none());
        assert!(shared.snapshot().sessions.is_empty());
        forward_ap2_remote_control(&shared, remote_state(7, "A1", "111"));
        assert!(shared.session_id(7).is_none());
    }

    #[test]
    fn sender_volume_revision_is_per_stream_and_order_is_global() {
        let shared = test_shared();
        let first = activate(&shared, Protocol::AirPlay2, 1);
        let second = activate(&shared, Protocol::AirPlay2, 2);

        assert_eq!(apply_volume(&shared, volume(1, 1, 1, -6.0)), Some(first));
        assert_eq!(apply_volume(&shared, volume(2, 1, 2, -12.0)), Some(second));
        assert_eq!(apply_volume(&shared, volume(1, 2, 3, -6.0)), Some(first));
        let snapshots = shared.sender_volume_snapshots();
        assert_eq!(
            snapshots,
            vec![
                SenderVolumeSnapshot {
                    session_id: first,
                    db: -6.0,
                    revision: 2,
                    order: 3,
                },
                SenderVolumeSnapshot {
                    session_id: second,
                    db: -12.0,
                    revision: 1,
                    order: 2,
                },
            ]
        );
        // Duplicates and older revisions are rejected per stream.
        assert_eq!(apply_volume(&shared, volume(1, 2, 4, -20.0)), None);
        assert_eq!(apply_volume(&shared, volume(1, 1, 5, -20.0)), None);
        assert_eq!(apply_volume(&shared, volume(1, 0, 6, -20.0)), None);
        // A same-value command is visible through its higher revision.
        assert_eq!(apply_volume(&shared, volume(2, 2, 7, -12.0)), Some(second));
        let latest = shared.sender_volume_snapshots();
        assert_eq!((latest[1].revision, latest[1].order), (2, 7));
        let max_order = latest.iter().map(|s| s.order).max().unwrap();
        assert_eq!(max_order, 7, "order identifies the last command across sessions");

        end(&shared, 1, first);
        let replacement = activate(&shared, Protocol::AirPlay2, 3);
        assert_eq!(apply_volume(&shared, volume(1, 3, 8, -3.0)), None, "old stream");
        assert_eq!(
            apply_volume(&shared, volume(3, 1, 9, -6.0)),
            Some(replacement),
            "a new stream restarts at revision 1"
        );
        let latest = shared.sender_volume_snapshots();
        assert!(latest.iter().any(|snapshot| snapshot.session_id == replacement));
        assert!(latest.iter().all(|snapshot| snapshot.session_id != first));
    }

    #[tokio::test]
    async fn delayed_flush_probe_cannot_reset_replacement_bus() {
        let shared = test_shared();
        let old_sink = activate(&shared, Protocol::AirPlay2, 1);
        end(&shared, 1, old_sink);
        let new_sink = activate(&shared, Protocol::AirPlay2, 2);
        let mut reader = lane_bus(&shared, 2).subscribe();
        let events = shared.events.subscribe();
        let before = reader.read_into(&mut [0.0; 1]);
        assert_eq!(before.session_id, Some(new_sink));

        run_forwarder(
            &shared,
            vec![
                ProbeEvent::Flushed {
                    peer: test_peer().ip(),
                    stream_id: 1,
                },
                ProbeEvent::Flushed {
                    peer: test_peer().ip(),
                    stream_id: 2,
                },
            ],
            Vec::new(),
            Vec::new(),
        )
        .await;

        let after = reader.read_into(&mut [0.0; 1]);
        assert_eq!(after.session_id, Some(new_sink));
        assert_eq!(after.epoch, before.epoch);
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![AirPlayEvent::Flushed {
                protocol: Protocol::AirPlay2,
                session_id: Some(new_sink),
            }]
        );
    }

    #[test]
    fn remote_control_follows_pcm_owner_but_never_public_status() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "0000A1B2C3D4E5F6", "1986535575"));
        let provisional = shared.session_id(1).unwrap();
        assert!(shared.remote_control_snapshot(provisional).is_none());

        let active = activate(&shared, Protocol::AirPlay2, 1);
        assert_ne!(active, provisional);
        let remote = shared
            .remote_control_snapshot(active)
            .expect("active DACP capability")
            .info()
            .clone();
        assert_eq!(remote.session_id, active);
        assert_eq!(remote.peer, test_peer());
        assert_eq!(remote.dacp_id, "0000A1B2C3D4E5F6");
        assert_eq!(remote.active_remote, "1986535575");
        let public = serde_json::to_string(&shared.snapshot()).unwrap();
        assert!(!public.contains("1986535575"));
        assert!(!public.contains("D4E5F6"));

        end(&shared, 1, active);
        assert!(shared.remote_control_snapshot(active).is_none());
    }

    #[test]
    fn per_session_controls_and_volumes_stay_separate() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        forward_ap2_remote_control(&shared, remote_state(2, "B2", "222"));
        let a = activate(&shared, Protocol::AirPlay2, 1);
        let b = activate(&shared, Protocol::AirPlay2, 2);
        shared.update_volume(2, -9.0);
        assert_eq!(shared.remote_control_snapshot(a).unwrap().info().session_id, a);
        assert_eq!(shared.remote_control_snapshot(b).unwrap().info().session_id, b);
        let volumes = shared.sender_volume_snapshots();
        assert_eq!(volumes.len(), 1, "only b has a sender volume");
        assert_eq!(volumes[0].session_id, b);
        end(&shared, 1, a);
        assert!(shared.remote_control_snapshot(a).is_none());
        assert_eq!(shared.remote_control_snapshot(b).unwrap().info().session_id, b);
        assert_eq!(shared.live_session_ids(), vec![b]);
    }

    #[test]
    fn descriptive_state_and_artwork_revision_follow_only_the_current_session() {
        let shared = test_shared();
        let session_id = activate(&shared, Protocol::AirPlay2, 7);
        let other = activate(&shared, Protocol::AirPlay2, 8);
        assert_eq!(
            shared.update_metadata_versioned(
                7,
                Some("Song".into()),
                Some("Artist".into()),
                Some("Album".into()),
            ),
            Some((Protocol::AirPlay2, session_id))
        );
        assert_eq!(
            shared.update_progress_versioned(7, Some(12_000), Some(180_000)),
            Some((Protocol::AirPlay2, session_id))
        );

        let (_, updated_session, revision) = shared
            .update_artwork_versioned(7, "image/png".into(), b"\x89PNG\r\n\x1a\ncover".to_vec())
            .unwrap();
        assert_eq!(updated_session, session_id);
        let (_, _, other_revision) = shared
            .update_artwork_versioned(8, "image/jpeg".into(), b"\xff\xd8other".to_vec())
            .unwrap();
        let artwork = shared.artwork(session_id, revision).unwrap();
        assert_eq!(artwork.content_type, "image/png");
        assert_eq!(artwork.data, b"\x89PNG\r\n\x1a\ncover");
        assert!(shared.artwork(session_id, revision + 1).is_none());
        assert_eq!(
            shared.artwork(other, other_revision).unwrap().data,
            b"\xff\xd8other",
            "artwork is stored per session"
        );
        assert!(shared.artwork(other, revision).is_none());

        let session = shared
            .snapshot()
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap();
        assert_eq!(session.title.as_deref(), Some("Song"));
        assert_eq!(session.artist.as_deref(), Some("Artist"));
        assert_eq!(session.album.as_deref(), Some("Album"));
        assert_eq!(session.elapsed_ms, Some(12_000));
        assert_eq!(session.duration_ms, Some(180_000));
        assert_eq!(session.artwork_revision, Some(revision));
        assert_eq!(session.artwork_content_type.as_deref(), Some("image/png"));

        let (_, _, cleared_revision) = shared
            .update_artwork_versioned(7, "image/none".into(), Vec::new())
            .unwrap();
        assert!(shared.artwork(session_id, cleared_revision).is_none());

        assert!(shared
            .update_artwork_versioned(6, "image/png".into(), vec![1])
            .is_none());
        end(&shared, 7, session_id);
        assert!(shared.artwork(session_id, revision).is_none());
        assert!(shared.artwork(other, other_revision).is_some());
    }

    #[test]
    fn debug_does_not_expose_password() {
        let mut config = AirPlayConfig::new("Test Receiver");
        config.password = Some("correct horse battery staple".to_string());
        let debug = format!("{config:?}");
        assert!(debug.contains("password_set: true"));
        assert!(!debug.contains("correct horse"));
    }

    #[test]
    fn starts_one_listener_and_exposes_matching_ap2_and_classic_services() {
        let directory = std::env::temp_dir().join(format!(
            "audiohub-native-receiver-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = AirPlayConfig::new("AudioHub Native Receiver");
        config.password = Some("not-in-status-or-mdns".to_string());
        config.airplay2_identity_path = Some(directory.join("identity"));
        config.use_ephemeral_ptp_ports_for_tests();
        let runtime = AirPlayRuntime::start(config).unwrap();
        let status = runtime.status();
        assert_eq!(status.phase, RuntimePhase::Listening);
        assert_eq!(status.ptp_backend.as_deref(), Some("portable"));
        let port = status.airplay2_port.expect("control port");
        assert_ne!(port, 0);
        assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
        assert_eq!(runtime.mdns_services().len(), 2);
        let service = &runtime.mdns_services()[0];
        assert_eq!(service.protocol, Protocol::AirPlay2);
        assert_eq!(service.service_type, "_airplay._tcp.local.");
        assert!(service
            .txt_records
            .iter()
            .any(|record| record == "flags=0x84"));
        let classic = &runtime.mdns_services()[1];
        assert_eq!(classic.protocol, Protocol::AirPlay1);
        assert_eq!(classic.service_type, "_raop._tcp.local.");
        assert_eq!(classic.port, port);
        for entry in ["pw=true", "cn=1", "et=1", "ch=2"] {
            assert!(classic.txt_records.iter().any(|value| value == entry));
        }
        let ap2_key = service
            .txt_records
            .iter()
            .find(|v| v.starts_with("pk="))
            .unwrap();
        assert!(classic.txt_records.contains(ap2_key));
        let public = format!("{:?}{service:?}", runtime.status());
        assert!(!public.contains("not-in-status-or-mdns"));
        let lanes = runtime.pcm_lanes();
        assert!(lanes.lane(0).is_none(), "lanes are allocated lazily");
        assert!(runtime.live_session_ids().is_empty());
        runtime.stop().unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn classic_sink_identity_and_stale_protocol_watch_coexist_with_ap2() {
        let shared = test_shared();
        let classic = PcmIngress::new(
            Arc::clone(&shared),
            10,
            test_peer(),
            Protocol::AirPlay1,
            SessionTiming::Ntp,
        );
        assert_eq!(
            shared.session(10, classic.session_id).unwrap().protocol,
            Protocol::AirPlay1
        );
        let ap2 = PcmIngress::new_ap2(Arc::clone(&shared), 11, test_peer());
        assert_eq!(shared.live_session_ids(), vec![classic.session_id, ap2.session_id]);
        let classic_id = classic.session_id;
        drop(classic);
        assert_eq!(lane_bus(&shared, 11).active_session(), Some(ap2.session_id));
        assert!(shared.session(10, classic_id).is_none());
        assert!(shared
            .ensure_session_versioned(
                Protocol::AirPlay1,
                10,
                44_100,
                2,
                Some(test_peer().ip()),
                None
            )
            .is_none());
        assert_eq!(
            shared.session_id_versioned(11),
            Some((Protocol::AirPlay2, ap2.session_id))
        );
    }

    #[test]
    fn session_timing_serializes_in_snake_case_and_defaults_to_unknown() {
        assert_eq!(
            serde_json::to_string(&SessionTiming::PtpInstance).unwrap(),
            "\"ptp_instance\""
        );
        assert_eq!(
            serde_json::to_string(&SessionTiming::PtpPortable).unwrap(),
            "\"ptp_portable\""
        );
        let shared = test_shared();
        activate(&shared, Protocol::AirPlay2, 1);
        let mut value = serde_json::to_value(&shared.snapshot().sessions[0]).unwrap();
        value.as_object_mut().unwrap().remove("lane");
        value.as_object_mut().unwrap().remove("timing");
        let decoded: SessionInfo = serde_json::from_value(value).unwrap();
        assert_eq!((decoded.lane, decoded.timing), (None, SessionTiming::Unknown));
    }

    #[test]
    fn mapped_ipv4_addresses_are_normalized_for_ui() {
        let mapped = "::ffff:192.0.2.9".parse::<IpAddr>().unwrap();
        assert_eq!(normalize_ip(mapped), "192.0.2.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn long_unicode_name_is_bounded_only_at_the_dns_boundary() {
        let full = "客".repeat(48);
        let instance = airplay2_instance_name(&full);
        assert!(instance.len() <= DNS_LABEL_MAX_BYTES);
        assert!(instance.is_char_boundary(instance.len()));
        assert_eq!(instance, "客".repeat(21));

        let config = AirPlayConfig::new(full.clone());
        assert_eq!(config.name, full);
    }
}
