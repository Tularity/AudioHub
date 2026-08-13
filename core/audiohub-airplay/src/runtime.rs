use crate::protocol::media::engine::PcmOutput;
use crate::protocol::server::{
    self, EventCommandSendError, EventCommandSender, PcmOutputFactory, ProbeEvent,
    RemoteControlState as Ap2RemoteControlState, ServerConfig, VolumeUpdate as Ap2VolumeUpdate,
};
use crate::{BusConfig, PcmBus, StreamingPcmConverter};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
const UPSTREAM_EVENT_QUEUE_CAPACITY: usize = 16;
/// Per-subscriber event backlog. Runtime status is the source of truth, so a
/// consumer that cannot keep up drops intermediate notifications instead of
/// being allowed to retain an unbounded amount of artwork/metadata in memory.
const EVENT_QUEUE_CAPACITY: usize = 8;
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
    /// AudioHub-owned AirPlay 2 control and media protocol.
    AirPlay2,
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
    /// Default-output volume reported to a sender while the AP2 session is
    /// established. `None` omits the optional `initialVolume` field.
    pub initial_volume_db: Option<f32>,
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

/// One active incoming sender session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Runtime-unique ID; changes on every new stream and is also the PCM bus
    /// generation identity.
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
    /// Last reported playback position.
    pub elapsed_ms: Option<u64>,
    /// Last reported track duration.
    pub duration_ms: Option<u64>,
    /// Unix timestamp when this runtime first observed the stream.
    pub started_unix_ms: u64,
    /// True when this session currently owns the shared PCM bus.
    pub selected_for_output: bool,
}

/// DACP bearer capability for the active AirPlay 2 sender.
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
/// immediately before using the bearer token. Any credential refresh, stream
/// replacement, or teardown revokes older snapshots without copying or
/// hashing the token.
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

/// Lossless authoritative sender-volume state for the concrete PCM owner.
///
/// `revision` advances for every accepted sender command, even when `db` is
/// identical to the previous value. Pollers can therefore distinguish a new
/// same-value command from an old status snapshot after a reverse-volume
/// write. The tuple `(session_id, revision)` is the command identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SenderVolumeSnapshot {
    /// Runtime-local concrete PCM session identity.
    pub session_id: u64,
    /// Latest sender-authored AirPlay volume in dB.
    pub db: f32,
    /// Monotonic sender-command revision for this runtime.
    pub revision: u64,
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
        volume: f32,
        is_muted: bool,
    ) -> Result<(), EventCommandSendError> {
        if !self.is_current() {
            return Err(EventCommandSendError::revoked(
                io::ErrorKind::NotConnected,
                "AirPlay event volume authority was revoked",
            ));
        }
        self.info.sender.send_device_volume(volume, is_muted)
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
    /// Sender-provided cover image. Empty data clears it.
    Artwork {
        protocol: Protocol,
        session_id: Option<u64>,
        content_type: String,
        data: Vec<u8>,
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
}

#[derive(Debug)]
struct StatusState {
    phase: RuntimePhase,
    sessions: BTreeMap<Protocol, SessionInfo>,
    remote_controls: BTreeMap<Protocol, RemoteControlInfo>,
    upstream_sessions: BTreeMap<Protocol, u64>,
    sender_volume_revisions: BTreeMap<Protocol, u64>,
    receiver_volume_control: Option<ReceiverVolumeControlInfo>,
    last_error: Option<String>,
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
    bus: PcmBus,
    ports: BTreeMap<Protocol, u16>,
    next_session: AtomicU64,
    status: Mutex<StatusState>,
    remote_control_revision: Arc<AtomicU64>,
    receiver_volume_control_revision: Arc<AtomicU64>,
    events: EventHub,
}

impl Shared {
    fn new(bus: PcmBus, ports: BTreeMap<Protocol, u16>) -> Self {
        Self {
            bus,
            ports,
            next_session: AtomicU64::new(1),
            status: Mutex::new(StatusState {
                phase: RuntimePhase::Starting,
                sessions: BTreeMap::new(),
                remote_controls: BTreeMap::new(),
                upstream_sessions: BTreeMap::new(),
                sender_volume_revisions: BTreeMap::new(),
                receiver_volume_control: None,
                last_error: None,
            }),
            remote_control_revision: Arc::new(AtomicU64::new(1)),
            receiver_volume_control_revision: Arc::new(AtomicU64::new(1)),
            events: EventHub::default(),
        }
    }

    fn set_phase(&self, phase: RuntimePhase, error: Option<String>) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.phase = phase;
        status.last_error = error;
        if matches!(phase, RuntimePhase::Stopped | RuntimePhase::Failed) {
            if !status.remote_controls.is_empty() {
                self.bump_remote_control_revision();
            }
            status.sessions.clear();
            status.remote_controls.clear();
            status.upstream_sessions.clear();
            status.sender_volume_revisions.clear();
            if status.receiver_volume_control.take().is_some() {
                self.bump_receiver_volume_control_revision();
            }
        }
    }

    fn bump_remote_control_revision(&self) -> u64 {
        self.remote_control_revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    fn bump_receiver_volume_control_revision(&self) -> u64 {
        self.receiver_volume_control_revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// `remote_control_update`: `None` preserves the snapshot, while
    /// `Some(None)` explicitly clears it and `Some(Some(_))` replaces it.
    fn ensure_session_versioned(
        &self,
        protocol: Protocol,
        upstream_session_id: Option<u64>,
        rate: u32,
        channels: u8,
        peer: Option<IpAddr>,
        remote_control_update: Option<Option<RemoteControlInfo>>,
    ) -> Option<SessionInfo> {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let active = self.bus.active_session();
        if let Some(upstream_session_id) = upstream_session_id {
            match status.upstream_sessions.get(&protocol).copied() {
                Some(current) if upstream_session_id < current => return None,
                Some(current) if upstream_session_id > current => {
                    if status
                        .sessions
                        .get(&protocol)
                        .is_some_and(|session| active == Some(session.id))
                    {
                        // The vendored single-session gate makes this
                        // impossible for a legitimate SETUP. Refuse to let a
                        // delayed/malformed transition replace live PCM.
                        return None;
                    }
                    status.sessions.remove(&protocol);
                    status.sender_volume_revisions.remove(&protocol);
                    if status.remote_controls.remove(&protocol).is_some() {
                        self.bump_remote_control_revision();
                    }
                    if status.receiver_volume_control.take().is_some() {
                        self.bump_receiver_volume_control_revision();
                    }
                    status
                        .upstream_sessions
                        .insert(protocol, upstream_session_id);
                }
                Some(_) => {}
                None => {
                    status
                        .upstream_sessions
                        .insert(protocol, upstream_session_id);
                }
            }
        }
        let session = {
            let session = status
                .sessions
                .entry(protocol)
                .or_insert_with(|| SessionInfo {
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
                    elapsed_ms: None,
                    duration_ms: None,
                    started_unix_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                    selected_for_output: false,
                });
            session.sample_rate = rate;
            session.channels = channels;
            if peer.is_some() {
                session.peer = peer;
            }
            // The protocol reports SessionStarted immediately before constructing
            // its sink. Treat an otherwise-idle bus as selected during that tiny
            // ordering window so the event does not falsely claim the stream is
            // inactive; activate_sink makes the ownership authoritative next.
            session.selected_for_output = active.is_none() || active == Some(session.id);
            session.clone()
        };
        if let Some(remote_control) = remote_control_update {
            let remote_control = remote_control.map(|mut remote| {
                remote.session_id = session.id;
                remote
            });
            if status.remote_controls.get(&protocol) != remote_control.as_ref() {
                self.bump_remote_control_revision();
                match remote_control {
                    Some(remote) => {
                        status.remote_controls.insert(protocol, remote);
                    }
                    None => {
                        status.remote_controls.remove(&protocol);
                    }
                }
            }
        }
        Some(session)
    }

    #[cfg(test)]
    fn session_id(&self, protocol: Protocol) -> Option<u64> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(&protocol)
            .map(|s| s.id)
    }

    fn session_id_versioned(&self, protocol: Protocol, upstream_session_id: u64) -> Option<u64> {
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.upstream_sessions.get(&protocol).copied() != Some(upstream_session_id) {
            return None;
        }
        status.sessions.get(&protocol).map(|session| session.id)
    }

    fn activate_sink(
        &self,
        protocol: Protocol,
        upstream_session_id: Option<u64>,
        rate: u32,
        channels: u8,
        peer: Option<IpAddr>,
    ) -> u64 {
        // The sink callback is the authoritative binding to the concrete
        // player, whereas tagged upstream milestones still travel through an
        // asynchronous queue. Always mint a new local id here so provisional
        // status can never be mistaken for PCM ownership.
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(upstream_session_id) = upstream_session_id {
            if status.upstream_sessions.get(&protocol).copied() != Some(upstream_session_id) {
                // The synchronous sink callback is the authoritative binding
                // between a vendor SETUP id and concrete PCM ownership. Drop
                // any provisional state created by a delayed older watch
                // value before installing this mapping.
                status.sessions.remove(&protocol);
                status.sender_volume_revisions.remove(&protocol);
                if status.remote_controls.remove(&protocol).is_some() {
                    self.bump_remote_control_revision();
                }
                if status.receiver_volume_control.take().is_some() {
                    self.bump_receiver_volume_control_revision();
                }
                status
                    .upstream_sessions
                    .insert(protocol, upstream_session_id);
            }
        }
        let previous = status.sessions.remove(&protocol);
        let session = SessionInfo {
            id,
            protocol,
            peer: peer.or_else(|| previous.as_ref().and_then(|item| item.peer)),
            sample_rate: rate,
            channels,
            paused: false,
            volume_db: previous.as_ref().and_then(|item| item.volume_db),
            title: previous.as_ref().and_then(|item| item.title.clone()),
            artist: previous.as_ref().and_then(|item| item.artist.clone()),
            album: previous.as_ref().and_then(|item| item.album.clone()),
            elapsed_ms: previous.as_ref().and_then(|item| item.elapsed_ms),
            duration_ms: previous.as_ref().and_then(|item| item.duration_ms),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            selected_for_output: true,
        };
        status.sessions.insert(protocol, session);
        if let Some(remote) = status.remote_controls.get_mut(&protocol) {
            remote.session_id = id;
            self.bump_remote_control_revision();
        }
        if let Some(control) = status.receiver_volume_control.as_mut() {
            control.session_id = id;
            self.bump_receiver_volume_control_revision();
        }
        for item in status.sessions.values_mut() {
            item.selected_for_output = item.id == id;
        }
        drop(status);
        self.bus.activate(id);
        id
    }

    fn session(&self, protocol: Protocol, id: u64) -> Option<SessionInfo> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(&protocol)
            .filter(|session| session.id == id)
            .cloned()
    }

    #[cfg(test)]
    fn update_volume(&self, protocol: Protocol, db: f32) -> Option<u64> {
        self.update_volume_versioned(protocol, None, None, db)
    }

    fn update_volume_versioned(
        &self,
        protocol: Protocol,
        upstream_session_id: Option<u64>,
        upstream_revision: Option<u64>,
        db: f32,
    ) -> Option<u64> {
        if !db.is_finite() {
            return None;
        }
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(upstream_session_id) = upstream_session_id {
            if status.upstream_sessions.get(&protocol).copied() != Some(upstream_session_id) {
                return None;
            }
        }
        let session_id = status.sessions.get(&protocol)?.id;
        if self.bus.active_session() != Some(session_id) {
            return None;
        }
        let previous_revision = status
            .sender_volume_revisions
            .get(&protocol)
            .copied()
            .unwrap_or(0);
        let revision = match upstream_revision {
            Some(0) => return None,
            Some(revision) if revision <= previous_revision => return None,
            Some(revision) => revision,
            None => previous_revision.saturating_add(1),
        };
        let session = status
            .sessions
            .get_mut(&protocol)
            .expect("session identity was checked while holding the lock");
        session.volume_db = Some(db);
        status.sender_volume_revisions.insert(protocol, revision);
        Some(session_id)
    }

    fn end_sink(&self, protocol: Protocol, id: u64) {
        let removed = {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if status
                .sessions
                .get(&protocol)
                .is_some_and(|item| item.id == id)
            {
                status.sessions.remove(&protocol);
                status.sender_volume_revisions.remove(&protocol);
                if status.remote_controls.remove(&protocol).is_some() {
                    self.bump_remote_control_revision();
                }
                if status.receiver_volume_control.take().is_some() {
                    self.bump_receiver_volume_control_revision();
                }
                status.upstream_sessions.remove(&protocol);
                true
            } else {
                false
            }
        };
        // Revoke credentials before releasing PCM ownership so no queued
        // callback can acquire a fresh lease in the teardown interval.
        self.bus.end(id);
        if removed {
            self.events.send(AirPlayEvent::SessionEnded {
                protocol,
                session_id: Some(id),
            });
        }
    }

    fn end_upstream_session(&self, protocol: Protocol, upstream_session_id: u64) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.upstream_sessions.get(&protocol).copied() != Some(upstream_session_id) {
            return;
        }
        status.upstream_sessions.remove(&protocol);
        status.sender_volume_revisions.remove(&protocol);
        if status.remote_controls.remove(&protocol).is_some() {
            self.bump_remote_control_revision();
        }
        if status.receiver_volume_control.take().is_some() {
            self.bump_receiver_volume_control_revision();
        }
    }

    fn active_remote_control(&self) -> Option<RemoteControlInfo> {
        let active = self.bus.active_session()?;
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remote_controls
            .values()
            .find(|remote| remote.session_id == active)
            .cloned()
    }

    fn active_remote_control_snapshot(&self) -> Option<RemoteControlSnapshot> {
        let active = self.bus.active_session()?;
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let info = status
            .remote_controls
            .values()
            .find(|remote| remote.session_id == active)?
            .clone();
        Some(RemoteControlSnapshot {
            info,
            revision: self.remote_control_revision.load(Ordering::Acquire),
            current_revision: Arc::clone(&self.remote_control_revision),
        })
    }

    fn set_receiver_volume_control(
        &self,
        protocol: Protocol,
        upstream_session_id: u64,
        sender: Option<EventCommandSender>,
    ) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.upstream_sessions.get(&protocol).copied() != Some(upstream_session_id) {
            return;
        }
        let Some(session_id) = status.sessions.get(&protocol).map(|session| session.id) else {
            return;
        };
        let replacement = sender.map(|sender| ReceiverVolumeControlInfo { session_id, sender });
        if status.receiver_volume_control != replacement {
            status.receiver_volume_control = replacement;
            self.bump_receiver_volume_control_revision();
        }
    }

    fn receiver_volume_control_snapshot(&self) -> Option<ReceiverVolumeControlSnapshot> {
        let active = self.bus.active_session()?;
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let info = status.receiver_volume_control.as_ref()?;
        if info.session_id != active || !info.sender.is_active() {
            return None;
        }
        Some(ReceiverVolumeControlSnapshot {
            info: info.clone(),
            revision: self
                .receiver_volume_control_revision
                .load(Ordering::Acquire),
            current_revision: Arc::clone(&self.receiver_volume_control_revision),
        })
    }

    fn sender_volume_snapshot(&self) -> Option<SenderVolumeSnapshot> {
        let active = self.bus.active_session()?;
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let (protocol, session) = status
            .sessions
            .iter()
            .find(|(_, session)| session.id == active)?;
        let db = session.volume_db.filter(|db| db.is_finite())?;
        let revision = *status.sender_volume_revisions.get(protocol)?;
        Some(SenderVolumeSnapshot {
            session_id: active,
            db,
            revision,
        })
    }

    fn snapshot(&self) -> RuntimeStatus {
        let active = self.bus.active_session();
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let sessions = status
            .sessions
            .values()
            .cloned()
            .map(|mut session| {
                session.selected_for_output = active == Some(session.id);
                session
            })
            .collect();
        RuntimeStatus {
            phase: status.phase,
            airplay2_port: self.ports.get(&Protocol::AirPlay2).copied(),
            sessions,
            last_error: status.last_error.clone(),
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
        let config = config.validate()?;
        let mut ports = BTreeMap::new();
        let identity_path = config
            .airplay2_identity_path
            .clone()
            .expect("validated AirPlay 2 identity path");
        let server_config = Arc::new(ServerConfig::load(
            config.name.clone(),
            config.mac,
            config.password.clone(),
            identity_path,
            config.ptp_test_ephemeral,
            config.initial_volume_db,
        )?);
        // Reserve the requested control port only after all fallible receiver
        // configuration has loaded. The daemon may safely classify a marked
        // error from this one operation as a persisted-control-port conflict;
        // identity and other startup failures can never inherit that marker.
        let reservation = PortReservation::acquire(config.airplay2_port)?;
        ports.insert(Protocol::AirPlay2, reservation.port);
        let services = vec![MdnsService {
            protocol: Protocol::AirPlay2,
            service_type: "_airplay._tcp.local.".to_string(),
            instance_name: airplay2_instance_name(&config.name),
            port: reservation.port,
            txt_records: server_config.txt_records(),
        }];
        reservation.listener.set_nonblocking(true)?;
        let ap2 = NativeAp2Receiver {
            listener: reservation.listener,
            config: server_config,
        };

        let bus = PcmBus::new(config.bus);
        let shared = Arc::new(Shared::new(bus, ports));
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

    /// Clone the shared multi-reader PCM bus.
    pub fn pcm_bus(&self) -> PcmBus {
        self.shared.bus.clone()
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

    /// Secret-bearing DACP capability for the concrete active PCM session.
    /// Unlike [`Self::status`], this value is daemon-internal and must not be
    /// serialized or exposed through IPC.
    pub fn active_remote_control(&self) -> Option<RemoteControlInfo> {
        self.shared.active_remote_control()
    }

    /// Return the active secret snapshot together with a revocable lease for
    /// queued daemon work. Callers must re-check the lease immediately before
    /// sending an authenticated DACP request.
    pub fn active_remote_control_snapshot(&self) -> Option<RemoteControlSnapshot> {
        self.shared.active_remote_control_snapshot()
    }

    /// Current modern AP2 event-channel volume capability for the concrete
    /// PCM owner. This is the preferred reverse path; DACP remains a fallback
    /// for old senders carrying traditional credentials.
    pub fn receiver_volume_control_snapshot(&self) -> Option<ReceiverVolumeControlSnapshot> {
        self.shared.receiver_volume_control_snapshot()
    }

    /// Latest sender-authored volume for the concrete PCM owner. Unlike the
    /// bounded event stream, this snapshot cannot lose a final command; a new
    /// identical dB value is observable through its higher revision.
    pub fn sender_volume_snapshot(&self) -> Option<SenderVolumeSnapshot> {
        self.shared.sender_volume_snapshot()
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
        .worker_threads(2)
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
        let (volume_tx, volume_rx) = tokio::sync::watch::channel(None);
        let (remote_control_tx, remote_control_rx) = tokio::sync::watch::channel(None);
        let sink_shared = Arc::clone(&shared);
        let output_factory: PcmOutputFactory = Arc::new(move |stream_id, peer| {
            let ingress = PcmIngress::new_ap2(Arc::clone(&sink_shared), stream_id, peer);
            if let Some(session) = sink_shared.session(Protocol::AirPlay2, ingress.session_id) {
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
    shared: Arc<Shared>,
    session_id: u64,
    converter: StreamingPcmConverter,
    staged: Vec<f32>,
}

impl PcmIngress {
    fn new_ap2(shared: Arc<Shared>, upstream_session_id: u64, peer: SocketAddr) -> Self {
        let rate = 44_100;
        let channels = 2;
        let session_id = shared.activate_sink(
            Protocol::AirPlay2,
            Some(upstream_session_id),
            rate,
            channels,
            Some(normalize_socket_addr(peer).ip()),
        );
        let converter = StreamingPcmConverter::new(rate, channels).unwrap_or_else(|_| {
            // The AP2 media engine guarantees nonzero 44.1k stereo.
            // A defensive fallback keeps their playback thread alive while
            // status/events still expose the bad negotiated format.
            StreamingPcmConverter::new(44_100, 2).expect("constant format is valid")
        });
        Self {
            shared,
            session_id,
            converter,
            staged: Vec::new(),
        }
    }

    fn write(&mut self, pcm: &[i16]) {
        self.staged.clear();
        self.converter
            .set_correction_ppm(self.shared.bus.correction_ppm());
        self.converter.process_i16(pcm, &mut self.staged);
        // Rejected writes belong to an older overlapping protocol session.
        self.shared.bus.push(self.session_id, &self.staged);
    }

    fn flush(&mut self) {
        self.converter.flush();
        self.shared.bus.flush(self.session_id);
    }
}

impl Drop for PcmIngress {
    fn drop(&mut self) {
        self.shared.end_sink(Protocol::AirPlay2, self.session_id);
    }
}

struct Ap2Sink(PcmIngress);

impl PcmOutput for Ap2Sink {
    fn write(&mut self, pcm: &[i16]) {
        self.0.write(pcm);
    }

    fn flush(&mut self) {
        self.0.flush();
    }
}

async fn forward_ap2(
    mut receiver: tokio::sync::mpsc::Receiver<ProbeEvent>,
    mut latest_volume: tokio::sync::watch::Receiver<Option<Ap2VolumeUpdate>>,
    mut latest_remote_control: tokio::sync::watch::Receiver<Option<Ap2RemoteControlState>>,
    shared: Arc<Shared>,
) {
    let mut events_open = true;
    let mut volume_open = true;
    let mut remote_control_open = true;
    while events_open || volume_open || remote_control_open {
        tokio::select! {
            biased;
            changed = latest_remote_control.changed(), if remote_control_open => {
                match changed {
                    Ok(()) => {
                        if let Some(state) = latest_remote_control.borrow_and_update().clone() {
                            forward_ap2_remote_control(&shared, state);
                        }
                    }
                    Err(_) => remote_control_open = false,
                }
            }
            changed = latest_volume.changed(), if volume_open => {
                match changed {
                    Ok(()) => {
                        if let Some(volume) = *latest_volume.borrow_and_update() {
                            if let Some(session_id) = shared.update_volume_versioned(
                                Protocol::AirPlay2,
                                Some(volume.stream_id),
                                Some(volume.revision),
                                volume.db,
                            ) {
                                shared.events.send(AirPlayEvent::Volume {
                                    protocol: Protocol::AirPlay2,
                                    session_id: Some(session_id),
                                    db: volume.db,
                                });
                            }
                        }
                    }
                    Err(_) => volume_open = false,
                }
            }
            event = receiver.recv(), if events_open => {
                match event {
                    Some(ProbeEvent::Flushed { stream_id, .. }) => {
                        if let Some(session_id) =
                            shared.session_id_versioned(Protocol::AirPlay2, stream_id)
                        {
                            shared.events.send(AirPlayEvent::Flushed {
                                protocol: Protocol::AirPlay2,
                                session_id: Some(session_id),
                            });
                        }
                    }
                    Some(event) => {
                        log::info!(
                            target: "audiohub_airplay::airplay2",
                            "AirPlay 2 protocol milestone: {event:?}"
                        );
                    }
                    None => events_open = false,
                }
            }
        }
    }
}

fn forward_ap2_remote_control(shared: &Shared, state: Ap2RemoteControlState) {
    match state {
        Ap2RemoteControlState::Active {
            stream_id,
            peer,
            credentials,
            event_commands,
        } => {
            let peer = normalize_socket_addr(peer);
            let remote_control = credentials.map(|credentials| RemoteControlInfo {
                session_id: 0,
                peer,
                dacp_id: credentials.dacp_id,
                active_remote: credentials.active_remote,
            });
            let session = shared.ensure_session_versioned(
                Protocol::AirPlay2,
                Some(stream_id),
                44_100,
                2,
                Some(peer.ip()),
                Some(remote_control),
            );
            if session.is_some() {
                shared.set_receiver_volume_control(Protocol::AirPlay2, stream_id, event_commands);
            }
        }
        Ap2RemoteControlState::Ended { stream_id } => {
            shared.end_upstream_session(Protocol::AirPlay2, stream_id);
        }
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
            PcmBus::new(BusConfig {
                capacity_samples: 960,
                prebuffer_samples: 0,
                max_correction_ppm: 500,
            }),
            BTreeMap::from([(Protocol::AirPlay2, 7000)]),
        ))
    }

    fn test_peer() -> SocketAddr {
        std::net::SocketAddrV6::new("fe80::1234".parse().unwrap(), 7000, 0, 7).into()
    }

    fn remote_state(stream_id: u64, dacp_id: &str, active_remote: &str) -> Ap2RemoteControlState {
        Ap2RemoteControlState::Active {
            stream_id,
            peer: test_peer(),
            credentials: Some(RemoteControlCredentials {
                dacp_id: dacp_id.into(),
                active_remote: active_remote.into(),
            }),
            event_commands: None,
        }
    }

    fn apply_volume(shared: &Shared, update: Ap2VolumeUpdate) -> Option<u64> {
        let session_id = shared.update_volume_versioned(
            Protocol::AirPlay2,
            Some(update.stream_id),
            Some(update.revision),
            update.db,
        );
        if session_id.is_some() {
            shared.events.send(AirPlayEvent::Volume {
                protocol: Protocol::AirPlay2,
                session_id,
                db: update.db,
            });
        }
        session_id
    }

    #[test]
    fn native_default_is_enabled_on_an_automatic_port() {
        let config = AirPlayConfig::new("Test Receiver");
        assert!(config.enable_airplay2);
        assert_eq!(config.airplay2_port, 0);
        assert_eq!(config.password, None);
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
        let session_id = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        let rx = shared.events.subscribe();
        for index in 0..EVENT_QUEUE_CAPACITY {
            shared.events.send(AirPlayEvent::RuntimeFailed {
                message: format!("filler-{index}"),
            });
        }

        let updated_session = shared.update_volume(Protocol::AirPlay2, -6.0);
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
            shared.sender_volume_snapshot(),
            Some(SenderVolumeSnapshot {
                session_id,
                db: -6.0,
                revision: 1,
            })
        );
    }

    #[tokio::test]
    async fn saturated_probe_queue_still_forwards_latest_volume() {
        let shared = test_shared();
        let session_id = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        let events = shared.events.subscribe();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(UPSTREAM_EVENT_QUEUE_CAPACITY);
        for _ in 0..UPSTREAM_EVENT_QUEUE_CAPACITY {
            event_tx
                .try_send(ProbeEvent::PairingComplete {
                    peer: Ipv4Addr::LOCALHOST.into(),
                })
                .unwrap();
        }
        let (volume_tx, volume_rx) = tokio::sync::watch::channel(None);
        for (revision, db) in [(1, -20.0), (2, -12.0), (3, -6.0)] {
            volume_tx
                .send(Some(Ap2VolumeUpdate {
                    stream_id: 1,
                    revision,
                    db,
                }))
                .unwrap();
        }
        let (remote_tx, remote_rx) =
            tokio::sync::watch::channel::<Option<Ap2RemoteControlState>>(None);
        drop(event_tx);
        drop(volume_tx);
        drop(remote_tx);

        tokio::time::timeout(
            Duration::from_secs(1),
            forward_ap2(event_rx, volume_rx, remote_rx, Arc::clone(&shared)),
        )
        .await
        .expect("forwarder did not close after its inputs closed");

        let status = shared.snapshot();
        assert_eq!(status.sessions[0].id, session_id);
        assert_eq!(status.sessions[0].volume_db, Some(-6.0));
        assert_eq!(shared.sender_volume_snapshot().unwrap().revision, 3);
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
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(UPSTREAM_EVENT_QUEUE_CAPACITY);
        for _ in 0..UPSTREAM_EVENT_QUEUE_CAPACITY {
            event_tx
                .try_send(ProbeEvent::PairingComplete {
                    peer: Ipv4Addr::LOCALHOST.into(),
                })
                .unwrap();
        }
        let (volume_tx, volume_rx) = tokio::sync::watch::channel::<Option<Ap2VolumeUpdate>>(None);
        let (remote_tx, remote_rx) = tokio::sync::watch::channel(None);
        remote_tx.send(Some(remote_state(1, "A1", "111"))).unwrap();
        remote_tx.send(Some(remote_state(1, "A1", "222"))).unwrap();
        drop(event_tx);
        drop(volume_tx);
        drop(remote_tx);

        tokio::time::timeout(
            Duration::from_secs(1),
            forward_ap2(event_rx, volume_rx, remote_rx, Arc::clone(&shared)),
        )
        .await
        .expect("forwarder did not drain saturated inputs");

        assert!(shared.active_remote_control_snapshot().is_none());
        let concrete = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        let snapshot = shared
            .active_remote_control_snapshot()
            .expect("credentials followed the concrete sink");
        assert_eq!(snapshot.info().session_id, concrete);
        assert_eq!(snapshot.info().active_remote, "222");
        assert!(snapshot.is_current());
    }

    #[test]
    fn credential_refresh_revokes_old_lease_without_crossing_streams() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        let concrete = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        let original = shared.active_remote_control_snapshot().unwrap();
        assert_eq!(original.info().session_id, concrete);

        forward_ap2_remote_control(&shared, remote_state(1, "A1", "222"));
        let token_refresh = shared.active_remote_control_snapshot().unwrap();
        assert!(!original.is_current());
        assert!(token_refresh.is_current());
        assert_eq!(token_refresh.info().peer, original.info().peer);
        assert_eq!(token_refresh.info().dacp_id, original.info().dacp_id);
        assert_eq!(token_refresh.info().active_remote, "222");

        forward_ap2_remote_control(&shared, remote_state(1, "B2", "333"));
        let id_refresh = shared.active_remote_control_snapshot().unwrap();
        assert!(!token_refresh.is_current());
        assert_eq!(id_refresh.info().dacp_id, "B2");
        assert_eq!(id_refresh.info().active_remote, "333");

        shared.end_sink(Protocol::AirPlay2, concrete);
        forward_ap2_remote_control(&shared, remote_state(2, "C3", "444"));
        let replacement = shared.activate_sink(
            Protocol::AirPlay2,
            Some(2),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        forward_ap2_remote_control(&shared, Ap2RemoteControlState::Ended { stream_id: 1 });
        let current = shared.active_remote_control_snapshot().unwrap();
        assert_eq!(current.info().session_id, replacement);
        assert_eq!(current.info().dacp_id, "C3");
        assert_eq!(current.info().active_remote, "444");
    }

    #[test]
    fn delayed_old_state_cannot_capture_replacement_sink() {
        let shared = test_shared();
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "111"));
        let old_sink = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        shared.end_sink(Protocol::AirPlay2, old_sink);

        let new_sink = shared.activate_sink(
            Protocol::AirPlay2,
            Some(2),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        forward_ap2_remote_control(&shared, remote_state(1, "A1", "333"));
        forward_ap2_remote_control(&shared, remote_state(2, "B2", "444"));

        let current = shared.active_remote_control_snapshot().unwrap();
        assert_eq!(current.info().session_id, new_sink);
        assert_eq!(current.info().dacp_id, "B2");
        assert_eq!(current.info().active_remote, "444");
    }

    #[test]
    fn sender_volume_revision_rejects_duplicates_and_old_streams() {
        let shared = test_shared();
        let first_session = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );

        assert_eq!(
            apply_volume(
                &shared,
                Ap2VolumeUpdate {
                    stream_id: 1,
                    revision: 10,
                    db: -6.0,
                },
            ),
            Some(first_session)
        );
        let first = shared.sender_volume_snapshot().unwrap();
        assert_eq!(first.revision, 10);

        apply_volume(
            &shared,
            Ap2VolumeUpdate {
                stream_id: 1,
                revision: 11,
                db: -6.0,
            },
        );
        let repeated = shared.sender_volume_snapshot().unwrap();
        assert_eq!(repeated.db, first.db);
        assert!(repeated.revision > first.revision);

        assert_eq!(
            apply_volume(
                &shared,
                Ap2VolumeUpdate {
                    stream_id: 1,
                    revision: 10,
                    db: -20.0,
                },
            ),
            None
        );
        assert_eq!(shared.sender_volume_snapshot(), Some(repeated));

        shared.end_sink(Protocol::AirPlay2, first_session);
        let replacement = shared.activate_sink(
            Protocol::AirPlay2,
            Some(2),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        assert_eq!(shared.sender_volume_snapshot(), None);
        assert_eq!(
            apply_volume(
                &shared,
                Ap2VolumeUpdate {
                    stream_id: 1,
                    revision: 12,
                    db: -3.0,
                },
            ),
            None
        );
        assert_eq!(
            apply_volume(
                &shared,
                Ap2VolumeUpdate {
                    stream_id: 2,
                    revision: 13,
                    db: -6.0,
                },
            ),
            Some(replacement)
        );
        assert_eq!(
            shared.sender_volume_snapshot(),
            Some(SenderVolumeSnapshot {
                session_id: replacement,
                db: -6.0,
                revision: 13,
            })
        );
    }

    #[tokio::test]
    async fn delayed_flush_probe_cannot_reset_replacement_bus() {
        let shared = test_shared();
        let old_sink = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        shared.end_sink(Protocol::AirPlay2, old_sink);
        let new_sink = shared.activate_sink(
            Protocol::AirPlay2,
            Some(2),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        let mut reader = shared.bus.subscribe();
        let events = shared.events.subscribe();
        let before = reader.read_into(&mut [0.0; 1]);
        assert_eq!(before.session_id, Some(new_sink));

        let (event_tx, event_rx) = tokio::sync::mpsc::channel(2);
        event_tx
            .send(ProbeEvent::Flushed {
                peer: test_peer().ip(),
                stream_id: 1,
            })
            .await
            .unwrap();
        event_tx
            .send(ProbeEvent::Flushed {
                peer: test_peer().ip(),
                stream_id: 2,
            })
            .await
            .unwrap();
        let (volume_tx, volume_rx) = tokio::sync::watch::channel::<Option<Ap2VolumeUpdate>>(None);
        let (remote_tx, remote_rx) =
            tokio::sync::watch::channel::<Option<Ap2RemoteControlState>>(None);
        drop(event_tx);
        drop(volume_tx);
        drop(remote_tx);
        forward_ap2(event_rx, volume_rx, remote_rx, Arc::clone(&shared)).await;

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
        let provisional = shared.session_id(Protocol::AirPlay2).unwrap();
        assert!(shared.active_remote_control().is_none());

        let active = shared.activate_sink(
            Protocol::AirPlay2,
            Some(1),
            44_100,
            2,
            Some(test_peer().ip()),
        );
        assert_ne!(active, provisional);
        let remote = shared
            .active_remote_control()
            .expect("active DACP capability");
        assert_eq!(remote.session_id, active);
        assert_eq!(remote.peer, test_peer());
        assert_eq!(remote.dacp_id, "0000A1B2C3D4E5F6");
        assert_eq!(remote.active_remote, "1986535575");
        let public = serde_json::to_string(&shared.snapshot()).unwrap();
        assert!(!public.contains("1986535575"));
        assert!(!public.contains("D4E5F6"));

        shared.end_sink(Protocol::AirPlay2, active);
        assert!(shared.active_remote_control().is_none());
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
    fn starts_native_receiver_and_exposes_only_its_service() {
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
        let port = status.airplay2_port.expect("control port");
        assert_ne!(port, 0);
        assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
        assert_eq!(runtime.mdns_services().len(), 1);
        let service = &runtime.mdns_services()[0];
        assert_eq!(service.protocol, Protocol::AirPlay2);
        assert_eq!(service.service_type, "_airplay._tcp.local.");
        assert!(service
            .txt_records
            .iter()
            .any(|record| record == "flags=0x84"));
        let public = format!("{:?}{service:?}", runtime.status());
        assert!(!public.contains("not-in-status-or-mdns"));
        runtime.stop().unwrap();
        let _ = std::fs::remove_dir_all(directory);
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
