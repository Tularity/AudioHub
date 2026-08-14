//! Bounded AirPlay 2 control-session probe owned by AudioHub.

use super::airport_express::BundledAirPortExpressProvider;
use super::apple_challenge::{self, AppleChallengeError};
use super::crypto::{derive_control_keys, derive_event_keys, ChannelKeys, EncryptedChannel};
use super::event::encode_device_volume_command;
use super::fairplay::{self, BundledFairPlayProvider, FairPlayError};
use super::identity::ReceiverIdentity;
use super::info::{InfoProfile, FEATURES};
use super::media::buffered::{
    self, BufferedFlush, BufferedRateAnchor, PreparedType103, Type103MediaHandle, Type103Ports,
    WirePoint, STREAM_TYPE_BUFFERED_AUDIO,
};
use super::media::engine::{
    PcmOutput, PreparedTiming, PreparedType96, Type96MediaHandle, Type96Ports,
};
use super::media::ptp::{PtpClockSource, PtpObserver, PtpRemoteMatch};
use super::media::setup::{self, STREAM_TYPE_REALTIME_AUDIO};
use super::metadata::{self, Artwork, MergePolicy, NowPlayingUpdate, PatchField, TrackMetadata};
use super::pairing::PairSetupServer;
use super::rtsp::{Header, Request, RequestDecoder, RequestLimits, Response};
use super::tlv8::{self, Tlv8Field, Tlv8Limits};
use crate::runtime::{ReceiverVolumeProvider, ReceiverVolumeSnapshot};
use plist::{Dictionary, Value};
use rand::{rngs::OsRng, RngCore};
use std::collections::HashMap;
use std::io::{self, Cursor};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zeroize::Zeroizing;

const MAX_CONNECTIONS: usize = 16;
// AudioHub owns one local output mix. A second AirPlay sender must not reserve
// another decoder, network buffer, and device writer behind a separate control
// connection while the first stream is active.
const MAX_ACTIVE_MEDIA_STREAMS: usize = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENT_RESPONSE_BYTES: usize = 16 * 1024;
const EVENT_COMMAND_QUEUE_CAPACITY: usize = 1;
const ACTIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_EVENT_ACCEPT_ATTEMPTS: usize = 4;
const READ_CHUNK_BYTES: usize = 4 * 1024;
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;
const MAX_METADATA_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARTWORK_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_NOW_PLAYING_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_PAIRING_BODY_BYTES: usize = 4 * 1024;
const PAIRING_TLV8_CONTENT_TYPE: &str = "application/pairing+tlv8";
const BINARY_PLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
const OCTET_STREAM_CONTENT_TYPE: &str = "application/octet-stream";
const TEXT_PARAMETERS_CONTENT_TYPE: &str = "text/parameters";
const AIRPLAY_VOLUME_MIN_DB: f32 = -30.0;
const AIRPLAY_VOLUME_MAX_DB: f32 = 0.0;
const AIRPLAY_VOLUME_MUTE_DB: f32 = -144.0;
const MAX_DACP_ID_BYTES: usize = 128;
const MAX_ACTIVE_REMOTE_BYTES: usize = 128;
const MAX_BUFFERED_SEQUENCE: u64 = 0x00ff_ffff;
const MAX_CONCURRENT_APPLE_RESPONSES: usize = 2;
const APPLE_CHALLENGE_BURST: f64 = 4.0;
const APPLE_CHALLENGE_REFILL_PER_SECOND: f64 = 1.0;
const APPLE_CHALLENGE_RETRY_AFTER_SECONDS: u64 = 1;

const TLV_METHOD: u8 = 0x00;
const TLV_SALT: u8 = 0x02;
const TLV_PUBLIC_KEY: u8 = 0x03;
const TLV_PROOF: u8 = 0x04;
const TLV_STATE: u8 = 0x06;
const TLV_FLAGS: u8 = 0x13;
const PAIR_SETUP_METHOD: u8 = 0x00;
const TRANSIENT_PAIRING_FLAG: u8 = 0x10;
const DEFAULT_TRANSIENT_PIN: &str = "3939";

pub(crate) struct ServerConfig {
    name: String,
    mac: [u8; 6],
    password: Option<String>,
    identity: ReceiverIdentity,
    apple_response: Arc<dyn super::apple_challenge::AppleResponseProvider>,
    fairplay: BundledFairPlayProvider,
    media_permits: Arc<Semaphore>,
    ptp_clock: OnceLock<PtpClockSource>,
    ptp_test_ephemeral: bool,
    features: u64,
    receiver_volume: Option<ReceiverVolumeProvider>,
}

impl ServerConfig {
    pub(crate) fn load(
        name: String,
        mac: Option<[u8; 6]>,
        password: Option<String>,
        identity_path: PathBuf,
        ptp_test_ephemeral: bool,
        receiver_volume: Option<ReceiverVolumeProvider>,
    ) -> io::Result<Self> {
        let identity = ReceiverIdentity::load_or_create(&identity_path)?;
        let mac = mac.unwrap_or_else(|| stable_mac(&identity));
        Ok(Self {
            name,
            mac,
            password: password.filter(|value| !value.is_empty()),
            identity,
            apple_response: Arc::new(BundledAirPortExpressProvider),
            fairplay: BundledFairPlayProvider,
            media_permits: Arc::new(Semaphore::new(MAX_ACTIVE_MEDIA_STREAMS)),
            ptp_clock: OnceLock::new(),
            ptp_test_ephemeral,
            features: FEATURES,
            receiver_volume,
        })
    }

    pub(crate) fn txt_records(&self) -> Vec<String> {
        self.profile(None).txt_records()
    }

    pub(crate) fn info_body(&self) -> io::Result<Vec<u8>> {
        let volume = self.current_receiver_volume()?;
        self.profile(volume).binary_plist()
    }

    fn current_receiver_volume(&self) -> io::Result<Option<ReceiverVolumeSnapshot>> {
        self.receiver_volume
            .as_ref()
            .map(ReceiverVolumeProvider::current)
            .transpose()
    }

    fn profile(&self, volume: Option<ReceiverVolumeSnapshot>) -> InfoProfile<'_> {
        InfoProfile {
            name: &self.name,
            mac: self.mac,
            password_required: self.password.is_some(),
            identity: &self.identity,
            features: self.features,
            initial_volume_db: volume.map(|volume| volume.slider_db),
            is_muted: volume.map(|volume| volume.is_muted),
        }
    }

    fn event_update_info_body(&self) -> io::Result<Vec<u8>> {
        let volume = self.current_receiver_volume()?;
        let mut command = Dictionary::new();
        command.insert("type".into(), Value::String("updateInfo".into()));
        command.insert(
            "value".into(),
            Value::Dictionary(self.profile(volume).dictionary()?),
        );
        let mut body = Vec::new();
        Value::Dictionary(command)
            .to_writer_binary(&mut body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(body)
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("name", &self.name)
            .field("mac", &self.mac)
            .field("password_set", &self.password.is_some())
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupPhase1Fields {
    pub(crate) timing_protocol: Option<String>,
    pub(crate) remote_control_only: bool,
    pub(crate) has_timing_port: bool,
    pub(crate) supports_event_volume: bool,
}

/// Synchronous factory invoked only after the successful phase-two response
/// has reached the encrypted control socket. It cannot make SETUP falsely
/// succeed: every fallible network/decoder resource is prepared beforehand.
pub(crate) type PcmOutputFactory = Arc<dyn Fn(u64, SocketAddr) -> Box<dyn PcmOutput> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VolumeUpdate {
    pub(crate) stream_id: u64,
    pub(crate) revision: u64,
    pub(crate) db: f32,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteControlCredentials {
    pub(crate) dacp_id: String,
    pub(crate) active_remote: String,
}

impl std::fmt::Debug for RemoteControlCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteControlCredentials")
            .field("dacp_id", &self.dacp_id)
            .field("active_remote", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteControlState {
    Active {
        stream_id: u64,
        peer: SocketAddr,
        credentials: Option<RemoteControlCredentials>,
        event_commands: Option<EventCommandSender>,
    },
    Ended {
        stream_id: u64,
    },
}

/// Synchronous, bounded handle to the encrypted AP2 event-channel actor.
///
/// The daemon calls this only from its dedicated reverse-volume worker. The
/// Tokio task remains the sole owner of socket encryption counters and returns
/// only after the sender has accepted the command with a matching RTSP 2xx.
#[derive(Clone)]
pub(crate) struct EventCommandSender {
    commands: mpsc::Sender<OutboundEventCommand>,
    ready: Arc<AtomicBool>,
    record_started: EventStartSignal,
    supports_device_volume: bool,
}

impl EventCommandSender {
    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.commands.same_channel(&other.commands)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub(crate) fn supports_device_volume(&self) -> bool {
        self.supports_device_volume
    }

    /// Release the initial event-channel `updateInfo` only after RECORD has
    /// been acknowledged on the control connection. Apple receivers establish
    /// the event TCP socket during phase-one SETUP, but their protocol state
    /// does not accept the receiver's first command until the media session has
    /// started.
    fn activate_after_record(&self) {
        self.record_started.activate();
    }

    pub(crate) fn send_device_volume(
        &self,
        volume_scalar: f32,
        is_muted: bool,
    ) -> Result<(), EventCommandSendError> {
        if !self.supports_device_volume() {
            return Err(EventCommandSendError::before_write(
                io::ErrorKind::Unsupported,
                "AirPlay sender version does not support event-channel volume",
            ));
        }
        if !self.is_active() {
            return Err(EventCommandSendError::before_write(
                io::ErrorKind::NotConnected,
                "AirPlay event channel is not ready",
            ));
        }
        let body = encode_device_volume_command(volume_scalar, is_muted).map_err(|_| {
            EventCommandSendError::before_write(
                io::ErrorKind::InvalidInput,
                "invalid AirPlay event volume command",
            )
        })?;
        let (completion_tx, completion_rx) = std_mpsc::sync_channel(1);
        let attempt = Arc::new(EventCommandAttempt::new());
        self.commands
            .try_send(OutboundEventCommand {
                body,
                completion: completion_tx,
                attempt: Arc::clone(&attempt),
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => EventCommandSendError::before_write(
                    io::ErrorKind::WouldBlock,
                    "AirPlay event command queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => EventCommandSendError::before_write(
                    io::ErrorKind::NotConnected,
                    "AirPlay event channel is closed",
                ),
            })?;
        match completion_rx.recv_timeout(EVENT_COMMAND_TIMEOUT + Duration::from_secs(1)) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                let may_have_written = attempt.cancel();
                Err(EventCommandSendError::new(
                    io::ErrorKind::TimedOut,
                    "AirPlay event command response timed out",
                    may_have_written,
                    Some(ConnectionError::TimedOut),
                ))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let may_have_written = attempt.cancel();
                Err(EventCommandSendError::new(
                    io::ErrorKind::BrokenPipe,
                    "AirPlay event command task stopped",
                    may_have_written,
                    Some(ConnectionError::Io),
                ))
            }
        }
    }

    fn revoke(&self) {
        self.ready.store(false, Ordering::Release);
    }
}

impl std::fmt::Debug for EventCommandSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventCommandSender")
            .field("active", &self.is_active())
            .field("supports_device_volume", &self.supports_device_volume)
            .finish_non_exhaustive()
    }
}

impl PartialEq for EventCommandSender {
    fn eq(&self, other: &Self) -> bool {
        self.supports_device_volume == other.supports_device_volume && self.same_channel(other)
    }
}

impl Eq for EventCommandSender {}

#[derive(Clone)]
struct EventStartSignal {
    started: Arc<AtomicBool>,
    changed: Arc<Notify>,
}

impl EventStartSignal {
    fn new() -> Self {
        Self {
            started: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(Notify::new()),
        }
    }

    fn activate(&self) {
        if !self.started.swap(true, Ordering::AcqRel) {
            // Exactly one event actor owns this signal. `notify_one` retains
            // a permit if activation lands after `notified()` is created but
            // before that future is first polled; `notify_waiters` would lose
            // that wake and could strand the actor at the RECORD boundary.
            self.changed.notify_one();
        }
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            if self.started.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

struct OutboundEventCommand {
    body: Vec<u8>,
    completion: std_mpsc::SyncSender<Result<(), EventCommandSendError>>,
    attempt: Arc<EventCommandAttempt>,
}

const EVENT_COMMAND_PENDING: u8 = 0;
const EVENT_COMMAND_CANCELLED: u8 = 1;
const EVENT_COMMAND_WRITING: u8 = 2;

struct EventCommandAttempt {
    phase: AtomicU8,
}

impl EventCommandAttempt {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(EVENT_COMMAND_PENDING),
        }
    }

    fn begin_write(&self) -> bool {
        self.phase
            .compare_exchange(
                EVENT_COMMAND_PENDING,
                EVENT_COMMAND_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Cancel a command that has not begun writing. Returns whether bytes may
    /// already have reached the peer.
    fn cancel(&self) -> bool {
        match self.phase.compare_exchange(
            EVENT_COMMAND_PENDING,
            EVENT_COMMAND_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => false,
            Err(phase) => phase == EVENT_COMMAND_WRITING,
        }
    }
}

/// Failure to send a receiver-originated command over the AP2 event channel.
///
/// `may_have_written` is deliberately conservative: once a socket write has
/// begun, TCP cannot prove how many bytes reached the sender. Callers must treat
/// such failures as a possibly visible stale command and reassert current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventCommandSendError {
    kind: io::ErrorKind,
    label: &'static str,
    may_have_written: bool,
    connection_error: Option<ConnectionError>,
}

impl EventCommandSendError {
    fn new(
        kind: io::ErrorKind,
        label: &'static str,
        may_have_written: bool,
        connection_error: Option<ConnectionError>,
    ) -> Self {
        Self {
            kind,
            label,
            may_have_written,
            connection_error,
        }
    }

    fn before_write(kind: io::ErrorKind, label: &'static str) -> Self {
        Self::new(kind, label, false, None)
    }

    pub(crate) fn revoked(kind: io::ErrorKind, label: &'static str) -> Self {
        Self::before_write(kind, label)
    }

    fn during_transaction(error: ConnectionError) -> Self {
        Self::new(error.io_kind(), error.safe_label(), true, Some(error))
    }

    pub fn io_kind(&self) -> io::ErrorKind {
        self.kind
    }

    pub fn may_have_written(&self) -> bool {
        self.may_have_written
    }

    fn as_connection_error(&self) -> ConnectionError {
        self.connection_error.unwrap_or(ConnectionError::Io)
    }
}

impl std::fmt::Display for EventCommandSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label)
    }
}

impl std::error::Error for EventCommandSendError {}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProbeEvent {
    PairingComplete {
        peer: IpAddr,
    },
    FairPlayComplete {
        peer: IpAddr,
        phase: u8,
    },
    SetupPhase1 {
        peer: IpAddr,
        fields: SetupPhase1Fields,
    },
    SetupPhase2 {
        peer: IpAddr,
        stream_type: Option<u64>,
    },
    SessionStarted {
        peer: IpAddr,
        stream_id: u64,
    },
    Volume {
        peer: IpAddr,
        stream_id: u64,
        db: f32,
    },
    Flushed {
        peer: IpAddr,
        stream_id: u64,
    },
    Metadata {
        peer: IpAddr,
        stream_id: u64,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
    },
    Artwork {
        peer: IpAddr,
        stream_id: u64,
        content_type: String,
        data: Vec<u8>,
    },
    Progress {
        peer: IpAddr,
        stream_id: u64,
        elapsed_ms: Option<u64>,
        duration_ms: Option<u64>,
    },
    Paused {
        peer: IpAddr,
        stream_id: u64,
        paused: bool,
    },
}

/// Run the control accept loop on a listener already reserved by the runtime.
pub(crate) async fn serve(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    event_tx: mpsc::Sender<ProbeEvent>,
    latest_volume: watch::Sender<Option<VolumeUpdate>>,
    latest_remote_control: watch::Sender<Option<RemoteControlState>>,
    output_factory: PcmOutputFactory,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let ptp_binding = if config.ptp_test_ephemeral {
        PtpObserver::bind_ephemeral().await
    } else {
        PtpObserver::bind_ipv4().await
    };
    let (ptp_observer, ptp_clock) = ptp_binding.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot bind AirPlay 2 PTP sockets: {error}"),
        )
    })?;
    config.ptp_clock.set(ptp_clock).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "PTP clock source already installed",
        )
    })?;
    let ptp_task = ptp_observer.run();
    tokio::pin!(ptp_task);
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let pair_setup_limiter = Arc::new(PairSetupLimiter::new());
    let apple_challenges = Arc::new(AppleChallengeExecutor::new());
    let next_stream_id = Arc::new(AtomicU64::new(1));
    let volume_revision = Arc::new(AtomicU64::new(0));

    loop {
        let (socket, peer) = tokio::select! {
            result = &mut ptp_task => return result,
            accepted = listener.accept() => accepted?,
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 control connection refused: connection limit reached"
            );
            drop(socket);
            continue;
        };

        let config = Arc::clone(&config);
        let event_tx = event_tx.clone();
        let latest_volume = latest_volume.clone();
        let latest_remote_control = latest_remote_control.clone();
        let output_factory = Arc::clone(&output_factory);
        let pair_setup_limiter = Arc::clone(&pair_setup_limiter);
        let apple_challenges = Arc::clone(&apple_challenges);
        let next_stream_id = Arc::clone(&next_stream_id);
        let volume_revision = Arc::clone(&volume_revision);
        tokio::spawn(async move {
            let _permit = permit;
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 control connection accepted peer={peer}"
            );
            let result = run_connection(
                socket,
                peer,
                config,
                event_tx,
                latest_volume,
                latest_remote_control,
                output_factory,
                pair_setup_limiter,
                apple_challenges,
                next_stream_id,
                volume_revision,
            )
            .await;
            match result {
                Ok(()) => log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 control connection ended peer={peer}"
                ),
                Err(error) => log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 control connection failed peer={peer} reason={}",
                    error.safe_label()
                ),
            }
        });
    }
}

enum NowPlayingWireUpdate {
    Metadata(TrackMetadata),
    Artwork(Option<Artwork>),
    Progress { elapsed_ms: u64, duration_ms: u64 },
    Rich(NowPlayingUpdate),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeardownScope {
    /// A valid plist with a `streams` array ends only the current media stream.
    Stream,
    /// A valid plist without `streams` ends the complete control session.
    Session,
    /// Missing, malformed, or mistyped decorative input is acknowledged but
    /// cannot be allowed to tear down an authenticated connection.
    Ignore,
}

#[derive(Default)]
struct ConnectionNowPlaying {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    metadata_known: bool,
    artwork: Option<Artwork>,
    artwork_known: bool,
    elapsed_ms: Option<u64>,
    duration_ms: Option<u64>,
    progress_known: bool,
    paused: Option<bool>,
}

#[derive(Default)]
struct NowPlayingChanges {
    metadata: bool,
    artwork: bool,
    progress: bool,
    paused: bool,
}

impl ConnectionNowPlaying {
    fn apply(&mut self, update: NowPlayingWireUpdate) -> NowPlayingChanges {
        let mut changed = NowPlayingChanges::default();
        match update {
            NowPlayingWireUpdate::Metadata(metadata) => {
                self.title = metadata.title;
                self.artist = metadata.artist;
                self.album = metadata.album;
                self.metadata_known = true;
                changed.metadata = true;
            }
            NowPlayingWireUpdate::Artwork(artwork) => {
                self.artwork = artwork;
                self.artwork_known = true;
                changed.artwork = true;
            }
            NowPlayingWireUpdate::Progress {
                elapsed_ms,
                duration_ms,
            } => {
                self.elapsed_ms = Some(elapsed_ms);
                self.duration_ms = Some(duration_ms);
                self.progress_known = true;
                changed.progress = true;
            }
            NowPlayingWireUpdate::Rich(update) => {
                let replace = update.merge_policy == MergePolicy::Replace;
                changed.metadata = replace
                    || !matches!(update.title, PatchField::Missing)
                    || !matches!(update.artist, PatchField::Missing)
                    || !matches!(update.album, PatchField::Missing);
                if changed.metadata {
                    apply_patch_field(&mut self.title, update.title, replace);
                    apply_patch_field(&mut self.artist, update.artist, replace);
                    apply_patch_field(&mut self.album, update.album, replace);
                    self.metadata_known = true;
                }

                changed.artwork = replace || !matches!(update.artwork, PatchField::Missing);
                if changed.artwork {
                    apply_patch_field(&mut self.artwork, update.artwork, replace);
                    self.artwork_known = true;
                }

                changed.progress = replace
                    || !matches!(update.elapsed_time, PatchField::Missing)
                    || !matches!(update.duration, PatchField::Missing);
                if changed.progress {
                    apply_seconds_patch(&mut self.elapsed_ms, update.elapsed_time, replace);
                    apply_seconds_patch(&mut self.duration_ms, update.duration, replace);
                    self.progress_known = true;
                }

                changed.paused = replace || !matches!(update.playback_rate, PatchField::Missing);
                if changed.paused {
                    let mut rate = self.paused.map(|paused| if paused { 0.0 } else { 1.0 });
                    apply_patch_field(&mut rate, update.playback_rate, replace);
                    self.paused = Some(rate.is_some_and(|rate| rate == 0.0));
                }
            }
        }
        changed
    }

    fn events(&self, peer: IpAddr, stream_id: u64, changed: &NowPlayingChanges) -> Vec<ProbeEvent> {
        let mut events = Vec::with_capacity(4);
        if changed.metadata && self.metadata_known {
            events.push(ProbeEvent::Metadata {
                peer,
                stream_id,
                title: self.title.clone(),
                artist: self.artist.clone(),
                album: self.album.clone(),
            });
        }
        if changed.artwork && self.artwork_known {
            let (content_type, data) = self
                .artwork
                .as_ref()
                .map(|artwork| (artwork.content_type.clone(), artwork.data.clone()))
                .unwrap_or_else(|| ("image/none".to_string(), Vec::new()));
            events.push(ProbeEvent::Artwork {
                peer,
                stream_id,
                content_type,
                data,
            });
        }
        if changed.progress && self.progress_known {
            events.push(ProbeEvent::Progress {
                peer,
                stream_id,
                elapsed_ms: self.elapsed_ms,
                duration_ms: self.duration_ms,
            });
        }
        if changed.paused {
            if let Some(paused) = self.paused {
                events.push(ProbeEvent::Paused {
                    peer,
                    stream_id,
                    paused,
                });
            }
        }
        events
    }

    fn all_events(&self, peer: IpAddr, stream_id: u64) -> Vec<ProbeEvent> {
        self.events(
            peer,
            stream_id,
            &NowPlayingChanges {
                metadata: self.metadata_known,
                artwork: self.artwork_known,
                progress: self.progress_known,
                paused: self.paused.is_some(),
            },
        )
    }
}

fn apply_patch_field<T>(slot: &mut Option<T>, patch: PatchField<T>, replace: bool) {
    match patch {
        PatchField::Missing if replace => *slot = None,
        PatchField::Missing => {}
        PatchField::Clear => *slot = None,
        PatchField::Value(value) => *slot = Some(value),
    }
}

fn apply_seconds_patch(slot: &mut Option<u64>, patch: PatchField<f64>, replace: bool) {
    let patch = match patch {
        PatchField::Value(seconds) => {
            PatchField::Value((seconds * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64)
        }
        PatchField::Missing => PatchField::Missing,
        PatchField::Clear => PatchField::Clear,
    };
    apply_patch_field(slot, patch, replace);
}

struct ConnectionState {
    requests: RequestDecoder,
    pairing: Option<PairSetupServer>,
    control: Option<EncryptedChannel>,
    event_keys: Option<ChannelKeys>,
    event_task: Option<JoinHandle<()>>,
    event_commands: Option<EventCommandSender>,
    phase_one: Option<PhaseOneState>,
    media: Option<ActiveMedia>,
    /// One global receiver slot owned by this control connection while it has
    /// an active or prepared media stream. Keeping the permit outside the
    /// concrete stream lets the same connection replace a stream atomically.
    media_permit: Option<OwnedSemaphorePermit>,
    remote_control: RemoteControlAccumulator,
    pending_volume_db: Option<f32>,
    now_playing: ConnectionNowPlaying,
    plaintext_apple_response_sent: bool,
    latest_remote_control: watch::Sender<Option<RemoteControlState>>,
}

impl ConnectionState {
    fn new(
        latest_remote_control: watch::Sender<Option<RemoteControlState>>,
    ) -> Result<Self, ConnectionError> {
        // Plaintext pairing/control stays at the small default body budget.
        // `install_control_after_m4` raises it only after authentication.
        let limits = RequestLimits::default();
        Ok(Self {
            requests: RequestDecoder::new(limits).map_err(|_| ConnectionError::Framing)?,
            pairing: None,
            control: None,
            event_keys: None,
            event_task: None,
            event_commands: None,
            phase_one: None,
            media: None,
            media_permit: None,
            remote_control: RemoteControlAccumulator::default(),
            pending_volume_db: None,
            now_playing: ConnectionNowPlaying::default(),
            plaintext_apple_response_sent: false,
            latest_remote_control,
        })
    }

    fn active_stream_id(&self) -> Option<u64> {
        self.media.as_ref().map(ActiveMedia::stream_id)
    }

    fn active_media_kind(&self) -> Option<MediaKind> {
        self.media.as_ref().map(ActiveMedia::kind)
    }

    fn publish_remote_control(&self, peer: SocketAddr) {
        let Some(stream_id) = self.active_stream_id() else {
            return;
        };
        let _ = self
            .latest_remote_control
            .send(Some(RemoteControlState::Active {
                stream_id,
                peer,
                credentials: self.remote_control.complete(),
                event_commands: publishable_event_commands(self.event_commands.as_ref()),
            }));
    }
}

fn publishable_event_commands(commands: Option<&EventCommandSender>) -> Option<EventCommandSender> {
    // Publish the capability before the asynchronous event handshake becomes
    // ready. Runtime snapshots consult `is_active`, so this same handle becomes
    // usable after the initial updateInfo 2xx without another control request.
    commands.and_then(|commands| commands.supports_device_volume().then(|| commands.clone()))
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        if let Some(commands) = self.event_commands.take() {
            commands.revoke();
        }
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
        if let Some(media) = self.media.take() {
            let _ = self
                .latest_remote_control
                .send(Some(RemoteControlState::Ended {
                    stream_id: media.stream_id(),
                }));
            drop(media);
        }
    }
}

enum PhaseOneState {
    Ntp(PreparedTiming),
    Ptp(PtpClockSource),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Realtime,
    Buffered,
}

enum PreparedMedia {
    Type96 {
        stream_id: u64,
        media: PreparedType96,
    },
    Type103 {
        stream_id: u64,
        media: PreparedType103,
    },
}

impl PreparedMedia {
    fn stream_id(&self) -> u64 {
        match self {
            Self::Type96 { stream_id, .. } | Self::Type103 { stream_id, .. } => *stream_id,
        }
    }

    fn start(self, output: Box<dyn PcmOutput>) -> ActiveMedia {
        match self {
            Self::Type96 { stream_id, media } => ActiveMedia::Type96 {
                stream_id,
                handle: media.start(output),
            },
            Self::Type103 { stream_id, media } => ActiveMedia::Type103 {
                stream_id,
                handle: media.start(output),
            },
        }
    }
}

enum ActiveMedia {
    Type96 {
        stream_id: u64,
        handle: Type96MediaHandle,
    },
    Type103 {
        stream_id: u64,
        handle: Type103MediaHandle,
    },
}

impl ActiveMedia {
    fn stream_id(&self) -> u64 {
        match self {
            Self::Type96 { stream_id, .. } | Self::Type103 { stream_id, .. } => *stream_id,
        }
    }

    fn kind(&self) -> MediaKind {
        match self {
            Self::Type96 { .. } => MediaKind::Realtime,
            Self::Type103 { .. } => MediaKind::Buffered,
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Type96 { handle, .. } => handle.is_finished(),
            Self::Type103 { handle, .. } => handle.is_finished(),
        }
    }

    async fn set_rate(&self, anchor: BufferedRateAnchor) -> io::Result<()> {
        match self {
            Self::Type103 { handle, .. } => handle.set_rate(anchor).await,
            Self::Type96 { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rate control is only valid for buffered media",
            )),
        }
    }

    async fn flush(&self, request: FlushRequest) -> io::Result<()> {
        match (self, request) {
            (Self::Type96 { handle, .. }, FlushRequest::CurrentStream) => handle.flush().await,
            (Self::Type103 { handle, .. }, FlushRequest::CurrentStream) => {
                handle.flush(BufferedFlush::All).await
            }
            (Self::Type103 { handle, .. }, FlushRequest::Buffered { request }) => {
                handle.flush(request).await
            }
            (Self::Type96 { .. }, FlushRequest::Buffered { .. }) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffered flush is only valid for buffered media",
            )),
        }
    }

    async fn abort(&self) {
        match self {
            Self::Type96 { handle, .. } => handle.abort().await,
            Self::Type103 { handle, .. } => handle.abort().await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushRequest {
    CurrentStream,
    Buffered { request: BufferedFlush },
}

#[derive(Default)]
struct RemoteControlAccumulator {
    dacp_id: Option<String>,
    active_remote: Option<String>,
}

impl RemoteControlAccumulator {
    fn observe(&mut self, request: &Request) {
        if let Some(value) = unique_ascii_header(request, "DACP-ID", MAX_DACP_ID_BYTES) {
            self.dacp_id = Some(value);
        }
        if let Some(value) = unique_ascii_header(request, "Active-Remote", MAX_ACTIVE_REMOTE_BYTES)
        {
            self.active_remote = Some(value);
        }
    }

    fn complete(&self) -> Option<RemoteControlCredentials> {
        Some(RemoteControlCredentials {
            dacp_id: self.dacp_id.clone()?,
            active_remote: self.active_remote.clone()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionProgress {
    Continue,
    Close,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionError {
    Io,
    TimedOut,
    Framing,
    Encryption,
    ResponseEncoding,
}

impl ConnectionError {
    fn safe_label(self) -> &'static str {
        match self {
            Self::Io => "I/O error",
            Self::TimedOut => "request timeout",
            Self::Framing => "invalid request framing",
            Self::Encryption => "encrypted transport error",
            Self::ResponseEncoding => "response encoding error",
        }
    }

    fn io_kind(self) -> io::ErrorKind {
        match self {
            Self::Io => io::ErrorKind::BrokenPipe,
            Self::TimedOut => io::ErrorKind::TimedOut,
            Self::Framing | Self::ResponseEncoding => io::ErrorKind::InvalidData,
            Self::Encryption => io::ErrorKind::PermissionDenied,
        }
    }
}

async fn run_connection(
    mut socket: TcpStream,
    peer: SocketAddr,
    config: Arc<ServerConfig>,
    event_tx: mpsc::Sender<ProbeEvent>,
    latest_volume: watch::Sender<Option<VolumeUpdate>>,
    latest_remote_control: watch::Sender<Option<RemoteControlState>>,
    output_factory: PcmOutputFactory,
    pair_setup_limiter: Arc<PairSetupLimiter>,
    apple_challenges: Arc<AppleChallengeExecutor>,
    next_stream_id: Arc<AtomicU64>,
    volume_revision: Arc<AtomicU64>,
) -> Result<(), ConnectionError> {
    let mut state = ConnectionState::new(latest_remote_control)?;
    loop {
        if state.media.as_ref().is_some_and(ActiveMedia::is_finished) {
            if let Some(media) = state.media.take() {
                let stream_id = media.stream_id();
                let _ = state
                    .latest_remote_control
                    .send(Some(RemoteControlState::Ended { stream_id }));
                drop(media);
            }
            state.media_permit = None;
            state.now_playing = ConnectionNowPlaying::default();
        }
        let request_timeout = if state.media.is_some() {
            ACTIVE_REQUEST_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let progress = timeout(
            request_timeout,
            process_one_request(
                &mut socket,
                peer,
                &config,
                &event_tx,
                &latest_volume,
                &output_factory,
                &next_stream_id,
                &volume_revision,
                &pair_setup_limiter,
                &apple_challenges,
                &mut state,
            ),
        )
        .await
        .map_err(|_| ConnectionError::TimedOut)??;
        match progress {
            ConnectionProgress::Continue => {}
            ConnectionProgress::Close | ConnectionProgress::Eof => return Ok(()),
        }
    }
}

async fn process_one_request(
    socket: &mut TcpStream,
    peer: SocketAddr,
    config: &ServerConfig,
    event_tx: &mpsc::Sender<ProbeEvent>,
    latest_volume: &watch::Sender<Option<VolumeUpdate>>,
    output_factory: &PcmOutputFactory,
    next_stream_id: &AtomicU64,
    volume_revision: &AtomicU64,
    pair_setup_limiter: &PairSetupLimiter,
    apple_challenges: &AppleChallengeExecutor,
    state: &mut ConnectionState,
) -> Result<ConnectionProgress, ConnectionError> {
    let Some(mut request) = read_next_request(socket, state).await? else {
        return Ok(ConnectionProgress::Eof);
    };

    let local_addr = socket.local_addr().map_err(|_| ConnectionError::Io)?;
    let encrypted = state.control.is_some();
    let challenge_present = request
        .headers()
        .iter()
        .any(|header| header.name().eq_ignore_ascii_case("Apple-Challenge"));
    let apple_response = match unique_apple_challenge(&request) {
        Err(error) => Err(AppleResponseAttemptError::Challenge(error)),
        Ok(Some(_)) if encrypted || state.plaintext_apple_response_sent => {
            Err(AppleResponseAttemptError::Repeated)
        }
        Ok(challenge) => match apple_challenge::prepare(challenge, local_addr.ip(), config.mac) {
            Err(error) => Err(AppleResponseAttemptError::Challenge(error)),
            Ok(None) => Ok(None),
            Ok(Some(prepared)) => {
                let result = apple_challenges
                    .respond(peer.ip(), Arc::clone(&config.apple_response), prepared)
                    .await;
                if result.is_ok() {
                    state.plaintext_apple_response_sent = true;
                }
                result.map(Some)
            }
        },
    };

    if apple_response.is_ok() {
        state.remote_control.observe(&request);
        state.publish_remote_control(peer);
    }

    let active_stream_id = state.active_stream_id();
    let active_media_kind = state.active_media_kind();
    let mut outcome = match &apple_response {
        Ok(_) => {
            dispatch_request(
                &request,
                encrypted,
                peer,
                local_addr,
                config,
                pair_setup_limiter,
                &mut state.pairing,
                &mut state.event_keys,
                state.event_task.is_some(),
                &mut state.phase_one,
                state.media_permit.is_some(),
                active_stream_id,
                active_media_kind,
                next_stream_id,
            )
            .await
        }
        Err(AppleResponseAttemptError::Challenge(AppleChallengeError::MalformedChallenge))
        | Err(AppleResponseAttemptError::Repeated) => {
            DispatchOutcome::close(response_for(&request, 400, "Bad Request"))
        }
        Err(AppleResponseAttemptError::Challenge(
            AppleChallengeError::ProviderUnavailable | AppleChallengeError::InvalidProviderResponse,
        ))
        | Err(AppleResponseAttemptError::WorkerFailed) => {
            DispatchOutcome::close(response_for(&request, 500, "Internal Server Error"))
        }
        Err(AppleResponseAttemptError::RateLimited) => {
            let mut outcome =
                DispatchOutcome::close(response_for(&request, 429, "Too Many Requests"));
            if let Ok(header) = Header::new(
                "Retry-After",
                APPLE_CHALLENGE_RETRY_AFTER_SECONDS.to_string(),
            ) {
                outcome.response.headers.push(header);
            }
            outcome
        }
        Err(AppleResponseAttemptError::Busy) => {
            DispatchOutcome::close(response_for(&request, 503, "Service Unavailable"))
        }
    };
    // A control request may only receive 200 after the media engine has
    // accepted it. This keeps RTSP state and audio state atomic from the
    // sender's perspective, especially when the bounded deferred-flush queue
    // is full or a boundary conflicts with already authenticated media.
    let mut flushed_stream_id = None;
    if let Some(flush) = outcome.flush_media {
        let result = match state.media.as_ref() {
            Some(media) => media.flush(flush).await.map(|()| media.stream_id()),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "media stream is no longer active",
            )),
        };
        match result {
            Ok(stream_id) => flushed_stream_id = Some(stream_id),
            Err(error) => {
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 media flush rejected kind={:?}",
                    error.kind()
                );
                outcome.response = media_command_error_response(&request, &error);
                outcome.flush_media = None;
            }
        }
    }
    if let Some(anchor) = outcome.set_media_rate {
        let result = match state.media.as_ref() {
            Some(media) => media.set_rate(anchor).await,
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "media stream is no longer active",
            )),
        };
        if let Err(error) = result {
            log::warn!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 rate anchor rejected kind={:?}",
                error.kind()
            );
            outcome.response = media_command_error_response(&request, &error);
            outcome.set_media_rate = None;
        }
    }

    if let Ok(Some(value)) = &apple_response {
        match Header::new("Apple-Response", value.as_bytes().to_vec()) {
            Ok(header) => outcome.response.headers.push(header),
            Err(_) => {
                outcome =
                    DispatchOutcome::close(response_for(&request, 500, "Internal Server Error"));
            }
        }
    }

    if request.method() == "POST" && request.target() == "/feedback" {
        log::debug!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 feedback peer={peer} encrypted={encrypted} challenge_present={challenge_present} status={}",
            outcome.response.status,
        );
    } else {
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 RTSP peer={peer} encrypted={encrypted} challenge_present={challenge_present} method={} target={} status={}",
            request.method(),
            request.target(),
            outcome.response.status,
        );
    }

    let activate_event_after_response =
        request.method() == "RECORD" && (200..300).contains(&outcome.response.status);
    request.wipe_body();
    let encoded = outcome
        .response
        .encode()
        .map_err(|_| ConnectionError::ResponseEncoding)?;

    // M4 is itself plaintext. Install the control keys only after every byte
    // of that response has been accepted by the socket write path.
    write_response(socket, state.control.as_mut(), &encoded).await?;
    if activate_event_after_response {
        if let Some(commands) = state.event_commands.as_ref() {
            commands.activate_after_record();
        }
    }
    if let Some(keys) = outcome.install_control {
        install_control_after_m4(state, keys)?;
        state.event_keys = outcome.install_event_keys;
        let _ = event_tx.try_send(ProbeEvent::PairingComplete { peer: peer.ip() });
    }
    if let Some(endpoint) = outcome.install_event_endpoint {
        if let Some(previous) = state.event_commands.replace(endpoint.sender.clone()) {
            previous.revoke();
        }
        if let Some(previous) = state.event_task.replace(spawn_event_endpoint(endpoint)) {
            previous.abort();
        }
    }
    if let Some(phase_one) = outcome.install_phase_one {
        state.phase_one = Some(phase_one);
    }
    if let Some(permit) = outcome.install_media_permit {
        state.media_permit = Some(permit);
    }
    if let Some(prepared) = outcome.start_media {
        if let Some(previous) = state.media.take() {
            previous.abort().await;
        }
        let stream_id = prepared.stream_id();
        let output = output_factory(stream_id, peer);
        state.media = Some(prepared.start(output));
        match state.remote_control.complete() {
            Some(credentials) => log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 remote control ready peer={} stream_id={} dacp_id={}",
                peer.ip(),
                stream_id,
                credentials.dacp_id,
            ),
            None => log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 remote control unavailable peer={} stream_id={} (DACP-ID or Active-Remote missing)",
                peer.ip(),
                stream_id,
            ),
        }
        state.publish_remote_control(peer);
        if let Some(db) = state.pending_volume_db {
            publish_volume(latest_volume, volume_revision, stream_id, db);
        }
        let _ = event_tx.try_send(ProbeEvent::SessionStarted {
            peer: peer.ip(),
            stream_id,
        });
        for event in state.now_playing.all_events(peer.ip(), stream_id) {
            // Descriptive state is authoritative UI data and may carry the
            // only copy of a cover. Backpressure this connection briefly
            // rather than dropping it through the milestone queue.
            let _ = event_tx.send(event).await;
        }
    }
    if let Some(db) = outcome.volume_db {
        state.pending_volume_db = Some(db);
        if let Some(stream_id) = state.active_stream_id() {
            publish_volume(latest_volume, volume_revision, stream_id, db);
            let _ = event_tx.try_send(ProbeEvent::Volume {
                peer: peer.ip(),
                stream_id,
                db,
            });
        }
    }
    if let Some(update) = outcome.now_playing {
        let changed = state.now_playing.apply(update);
        if let Some(stream_id) = state.active_stream_id() {
            for event in state.now_playing.events(peer.ip(), stream_id, &changed) {
                let _ = event_tx.send(event).await;
            }
        }
    }
    if let Some(stream_id) = flushed_stream_id {
        let _ = event_tx.try_send(ProbeEvent::Flushed {
            peer: peer.ip(),
            stream_id,
        });
    }
    if let Some(event) = outcome.probe_event {
        let _ = event_tx.try_send(event);
    }

    if outcome.teardown_media {
        if let Some(media) = state.media.take() {
            let _ = state
                .latest_remote_control
                .send(Some(RemoteControlState::Ended {
                    stream_id: media.stream_id(),
                }));
            media.abort().await;
        }
        state.media_permit = None;
        state.pending_volume_db = None;
        state.now_playing = ConnectionNowPlaying::default();
    }

    Ok(if outcome.close_after {
        ConnectionProgress::Close
    } else {
        ConnectionProgress::Continue
    })
}

fn media_command_error_response(request: &Request, error: &io::Error) -> Response {
    match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            response_for(request, 400, "Bad Request")
        }
        io::ErrorKind::WouldBlock => response_for(request, 453, "Not Enough Bandwidth"),
        io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected => {
            response_for(request, 455, "Method Not Valid in This State")
        }
        _ => response_for(request, 500, "Internal Server Error"),
    }
}

fn publish_volume(
    latest_volume: &watch::Sender<Option<VolumeUpdate>>,
    volume_revision: &AtomicU64,
    stream_id: u64,
    db: f32,
) {
    let revision = volume_revision
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let _ = latest_volume.send(Some(VolumeUpdate {
        stream_id,
        revision,
        db,
    }));
}

fn install_control_after_m4(
    state: &mut ConnectionState,
    keys: ChannelKeys,
) -> Result<(), ConnectionError> {
    let mut control = keys.into_codec();
    // A sender may pipeline its first encrypted frame in the same TCP packet
    // as plaintext M3. The RTSP decoder has already retained those trailing
    // bytes, so move them across the exact plaintext/encrypted boundary
    // instead of ever parsing them as a plaintext request.
    let trailing = state.requests.take_buffered();
    state
        .requests
        .set_default_max_body_bytes(MAX_NOW_PLAYING_BODY_BYTES)
        .map_err(|_| ConnectionError::Framing)?;
    if !trailing.is_empty() {
        let plaintext = control
            .inbound
            .feed(&trailing)
            .map_err(|_| ConnectionError::Encryption)?;
        if !plaintext.is_empty() {
            state
                .requests
                .feed(&plaintext)
                .map_err(|_| ConnectionError::Framing)?;
        }
    }
    state.control = Some(control);
    Ok(())
}

async fn read_next_request(
    socket: &mut TcpStream,
    state: &mut ConnectionState,
) -> Result<Option<Request>, ConnectionError> {
    loop {
        let authenticated = state.control.is_some();
        if let Some(request) = state
            .requests
            .next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, authenticated)
            })
            .map_err(|_| ConnectionError::Framing)?
        {
            return Ok(Some(request));
        }

        let mut wire = [0u8; READ_CHUNK_BYTES];
        let read = socket
            .read(&mut wire)
            .await
            .map_err(|_| ConnectionError::Io)?;
        if read == 0 {
            return Ok(None);
        }
        if let Some(control) = state.control.as_mut() {
            let plaintext = control
                .inbound
                .feed(&wire[..read])
                .map_err(|_| ConnectionError::Encryption)?;
            if !plaintext.is_empty() {
                state
                    .requests
                    .feed(&plaintext)
                    .map_err(|_| ConnectionError::Framing)?;
            }
        } else {
            state
                .requests
                .feed(&wire[..read])
                .map_err(|_| ConnectionError::Framing)?;
        }
    }
}

fn max_body_for_request(
    method: &str,
    target: &str,
    headers: &[Header],
    authenticated: bool,
) -> Option<usize> {
    if authenticated && method == "SET_PARAMETER" {
        let content_type = unique_content_type_from_headers(headers);
        return Some(match content_type.as_deref() {
            Some("application/x-dmap-tagged") => MAX_METADATA_BODY_BYTES,
            Some("image/jpeg" | "image/png") => MAX_ARTWORK_BODY_BYTES,
            _ => MAX_CONTROL_BODY_BYTES,
        });
    }
    if authenticated && method == "POST" && target == "/command" {
        return Some(MAX_NOW_PLAYING_BODY_BYTES);
    }
    Some(match method {
        // Stock senders attach a small binary-plist qualifier to GET /info.
        // Keep GET bounded, but do not reject that request before dispatch.
        "GET" => MAX_CONTROL_BODY_BYTES,
        "OPTIONS" | "RECORD" | "FLUSH" => 0,
        "TEARDOWN" => MAX_CONTROL_BODY_BYTES,
        "POST" | "SETUP" | "SET_PARAMETER" | "GET_PARAMETER" => MAX_CONTROL_BODY_BYTES,
        _ => MAX_CONTROL_BODY_BYTES,
    })
}

fn unique_content_type_from_headers(headers: &[Header]) -> Option<String> {
    let mut values = headers
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("Content-Type"));
    let value = values.next()?.value();
    if values.next().is_some() {
        return None;
    }
    let value = std::str::from_utf8(value).ok()?;
    Some(value.split(';').next()?.trim().to_ascii_lowercase())
}

async fn write_response(
    socket: &mut TcpStream,
    control: Option<&mut EncryptedChannel>,
    plaintext: &[u8],
) -> Result<(), ConnectionError> {
    let wire = match control {
        Some(control) => control
            .outbound
            .encode(plaintext)
            .map_err(|_| ConnectionError::Encryption)?,
        None => plaintext.to_vec(),
    };
    socket
        .write_all(&wire)
        .await
        .map_err(|_| ConnectionError::Io)
}

struct DispatchOutcome {
    response: Response,
    install_control: Option<ChannelKeys>,
    install_event_keys: Option<ChannelKeys>,
    install_event_endpoint: Option<PreparedEventEndpoint>,
    install_phase_one: Option<PhaseOneState>,
    install_media_permit: Option<OwnedSemaphorePermit>,
    start_media: Option<PreparedMedia>,
    volume_db: Option<f32>,
    now_playing: Option<NowPlayingWireUpdate>,
    flush_media: Option<FlushRequest>,
    set_media_rate: Option<BufferedRateAnchor>,
    teardown_media: bool,
    probe_event: Option<ProbeEvent>,
    close_after: bool,
}

impl DispatchOutcome {
    fn reply(response: Response) -> Self {
        Self {
            response,
            install_control: None,
            install_event_keys: None,
            install_event_endpoint: None,
            install_phase_one: None,
            install_media_permit: None,
            start_media: None,
            volume_db: None,
            now_playing: None,
            flush_media: None,
            set_media_rate: None,
            teardown_media: false,
            probe_event: None,
            close_after: false,
        }
    }

    fn close(response: Response) -> Self {
        Self {
            response,
            install_control: None,
            install_event_keys: None,
            install_event_endpoint: None,
            install_phase_one: None,
            install_media_permit: None,
            start_media: None,
            volume_db: None,
            now_playing: None,
            flush_media: None,
            set_media_rate: None,
            teardown_media: false,
            probe_event: None,
            close_after: true,
        }
    }
}

async fn dispatch_request(
    request: &Request,
    encrypted: bool,
    peer: SocketAddr,
    local_addr: SocketAddr,
    config: &ServerConfig,
    pair_setup_limiter: &PairSetupLimiter,
    pairing: &mut Option<PairSetupServer>,
    event_keys: &mut Option<ChannelKeys>,
    event_endpoint_installed: bool,
    phase_one: &mut Option<PhaseOneState>,
    has_media_permit: bool,
    active_stream_id: Option<u64>,
    active_media_kind: Option<MediaKind>,
    next_stream_id: &AtomicU64,
) -> DispatchOutcome {
    if has_ambiguous_cseq(request) {
        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
    }

    if encrypted {
        dispatch_encrypted(
            request,
            peer,
            local_addr,
            config,
            event_keys,
            event_endpoint_installed,
            phase_one,
            has_media_permit,
            active_stream_id,
            active_media_kind,
            next_stream_id,
        )
        .await
    } else {
        dispatch_plaintext(request, peer.ip(), config, pair_setup_limiter, pairing).await
    }
}

async fn dispatch_plaintext(
    request: &Request,
    peer: IpAddr,
    config: &ServerConfig,
    pair_setup_limiter: &PairSetupLimiter,
    pairing: &mut Option<PairSetupServer>,
) -> DispatchOutcome {
    match (request.method(), request.target()) {
        ("OPTIONS", _) => options_response(request),
        ("GET", "/info") => match config.info_body() {
            Ok(body) => {
                let mut response = response_for(request, 200, "OK");
                if add_content_type(&mut response, "application/x-apple-binary-plist").is_err() {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
                response.body = body;
                DispatchOutcome::reply(response)
            }
            Err(_) => DispatchOutcome::close(response_for(request, 500, "Internal Server Error")),
        },
        ("POST", "/pair-pin-start") => {
            *pairing = None;
            DispatchOutcome::reply(response_for(request, 200, "OK"))
        }
        ("POST", "/pair-setup") => {
            handle_pair_setup(request, peer, config, pair_setup_limiter, pairing).await
        }
        (_, "/info" | "/pair-pin-start" | "/pair-setup") => {
            DispatchOutcome::reply(response_for(request, 405, "Method Not Allowed"))
        }
        _ => DispatchOutcome::reply(response_for(request, 501, "Not Implemented")),
    }
}

async fn dispatch_encrypted(
    request: &Request,
    peer: SocketAddr,
    local_addr: SocketAddr,
    config: &ServerConfig,
    event_keys: &mut Option<ChannelKeys>,
    event_endpoint_installed: bool,
    phase_one: &mut Option<PhaseOneState>,
    has_media_permit: bool,
    active_stream_id: Option<u64>,
    active_media_kind: Option<MediaKind>,
    next_stream_id: &AtomicU64,
) -> DispatchOutcome {
    match (request.method(), request.target()) {
        ("OPTIONS", _) => options_response(request),
        ("GET", "/info") => info_response(request, config),
        ("POST", "/fp-setup") => fairplay_response(request, peer.ip(), config),
        ("POST", "/command") => now_playing_command_response(request),
        ("POST", "/feedback" | "/audioMode") => {
            // Authenticated keep-alive/configuration messages outside the
            // audio-only now-playing profile are acknowledged and ignored.
            DispatchOutcome::reply(response_for(request, 200, "OK"))
        }
        ("SETUP", _) => {
            let local_timing_peer_id = config.identity.public_identifier();
            let mut outcome = setup_response(
                request,
                peer,
                local_addr,
                &local_timing_peer_id,
                config.ptp_clock.get(),
                &config.media_permits,
                event_keys,
                event_endpoint_installed,
                phase_one,
                has_media_permit,
                next_stream_id,
            )
            .await;
            if let Some(endpoint) = outcome.install_event_endpoint.as_mut() {
                endpoint.initial_update_info = match config.event_update_info_body() {
                    Ok(body) => Some(body),
                    Err(_) => {
                        return DispatchOutcome::close(response_for(
                            request,
                            500,
                            "Internal Server Error",
                        ));
                    }
                };
            }
            outcome
        }
        ("GET_PARAMETER", _) => match config.current_receiver_volume() {
            Ok(volume) => get_parameter_response(request, volume),
            Err(_) => DispatchOutcome::reply(response_for(request, 503, "Service Unavailable")),
        },
        ("SET_PARAMETER", _) => set_parameter_response(request),
        ("RECORD", _) => record_response(request),
        ("SETPEERS" | "SETPEERSX", _) => DispatchOutcome::reply(response_for(request, 200, "OK")),
        ("SETRATEANCHORTIME", _) => {
            if active_media_kind != Some(MediaKind::Buffered) {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            let anchor = match parse_rate_anchor(request) {
                Ok(anchor) => anchor,
                Err(()) => {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 rate anchor rejected shape={}",
                        safe_control_plist_shape(request)
                    );
                    return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
                }
            };
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            outcome.set_media_rate = Some(anchor);
            outcome
        }
        ("FLUSH", _) => {
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            outcome.flush_media = active_stream_id.map(|_| FlushRequest::CurrentStream);
            outcome
        }
        ("FLUSHBUFFERED", _) => {
            if active_media_kind != Some(MediaKind::Buffered) {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            let flush_request = match parse_buffered_flush(request) {
                Ok(request) => request,
                Err(()) => {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 buffered flush rejected shape={}",
                        safe_control_plist_shape(request)
                    );
                    return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
                }
            };
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            outcome.flush_media = Some(FlushRequest::Buffered {
                request: flush_request,
            });
            outcome
        }
        ("TEARDOWN", _) => {
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            match teardown_scope(request) {
                TeardownScope::Stream => {
                    outcome.teardown_media = true;
                }
                TeardownScope::Session => {
                    outcome.teardown_media = true;
                    outcome.close_after = true;
                    if let Ok(header) = Header::new("Connection", "close") {
                        outcome.response.headers.push(header);
                    }
                }
                TeardownScope::Ignore => {}
            }
            outcome
        }
        (_, "/info" | "/fp-setup") => {
            DispatchOutcome::reply(response_for(request, 405, "Method Not Allowed"))
        }
        (_, "/pair-pin-start" | "/pair-setup") => {
            DispatchOutcome::reply(response_for(request, 455, "Method Not Valid in This State"))
        }
        _ => DispatchOutcome::reply(response_for(request, 501, "Not Implemented")),
    }
}

fn info_response(request: &Request, config: &ServerConfig) -> DispatchOutcome {
    match config.info_body() {
        Ok(body) => {
            let mut response = response_for(request, 200, "OK");
            if add_content_type(&mut response, BINARY_PLIST_CONTENT_TYPE).is_err() {
                return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
            }
            response.body = body;
            DispatchOutcome::reply(response)
        }
        Err(_) => DispatchOutcome::close(response_for(request, 500, "Internal Server Error")),
    }
}

fn fairplay_response(request: &Request, peer: IpAddr, config: &ServerConfig) -> DispatchOutcome {
    if !has_content_type(request, OCTET_STREAM_CONTENT_TYPE) {
        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
    }
    let phase = request.body().get(6).copied();
    let body = match fairplay::respond(&config.fairplay, request.body()) {
        Ok(body) => body,
        Err(FairPlayError::MalformedRequest) => {
            return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
        }
        Err(FairPlayError::UnsupportedMode | FairPlayError::ProviderUnavailable) => {
            return DispatchOutcome::reply(response_for(request, 501, "Not Implemented"));
        }
        Err(FairPlayError::InvalidProviderResponse) => {
            return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
        }
    };

    let mut response = response_for(request, 200, "OK");
    if add_content_type(&mut response, OCTET_STREAM_CONTENT_TYPE).is_err() {
        return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
    }
    response.body = body;
    let mut outcome = DispatchOutcome::reply(response);
    if let Some(phase) = phase {
        outcome.probe_event = Some(ProbeEvent::FairPlayComplete { peer, phase });
    }
    outcome
}

async fn setup_response(
    request: &Request,
    peer: SocketAddr,
    local_addr: SocketAddr,
    local_timing_peer_id: &str,
    ptp_clock: Option<&PtpClockSource>,
    media_permits: &Arc<Semaphore>,
    event_keys: &mut Option<ChannelKeys>,
    event_endpoint_installed: bool,
    phase_one: &mut Option<PhaseOneState>,
    connection_has_media_permit: bool,
    next_stream_id: &AtomicU64,
) -> DispatchOutcome {
    if !has_content_type(request, BINARY_PLIST_CONTENT_TYPE) {
        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
    }
    let setup = match parse_setup(request.body()) {
        Ok(setup) => setup,
        Err(()) => return DispatchOutcome::close(response_for(request, 400, "Bad Request")),
    };

    match setup {
        ParsedSetup::Streams { stream_type } => {
            let Some(stream_type) = stream_type else {
                return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
            };
            if !matches!(
                stream_type,
                STREAM_TYPE_REALTIME_AUDIO | STREAM_TYPE_BUFFERED_AUDIO
            ) {
                let mut outcome =
                    DispatchOutcome::reply(response_for(request, 501, "Not Implemented"));
                outcome.probe_event = Some(ProbeEvent::SetupPhase2 {
                    peer: peer.ip(),
                    stream_type: Some(stream_type),
                });
                return outcome;
            }

            let timing_matches_stream = matches!(
                (stream_type, phase_one.as_ref()),
                (STREAM_TYPE_REALTIME_AUDIO, Some(PhaseOneState::Ntp(_)))
                    | (STREAM_TYPE_REALTIME_AUDIO, Some(PhaseOneState::Ptp(_)))
                    | (STREAM_TYPE_BUFFERED_AUDIO, Some(PhaseOneState::Ptp(_)))
            );
            if !timing_matches_stream {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }

            let new_media_permit = if connection_has_media_permit {
                None
            } else {
                let permit = match Arc::clone(media_permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        log::info!(
                            target: "audiohub_airplay::airplay2",
                            "AirPlay 2 media SETUP refused: active-stream limit reached"
                        );
                        return DispatchOutcome::reply(response_for(
                            request,
                            453,
                            "Not Enough Bandwidth",
                        ));
                    }
                };
                Some(permit)
            };

            let stream_id = next_stream_id.fetch_add(1, Ordering::Relaxed);
            let (prepared, body) = match stream_type {
                STREAM_TYPE_REALTIME_AUDIO => {
                    let setup = match setup::parse_phase2(request.body()) {
                        Ok(setup) => setup,
                        Err(_) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                400,
                                "Bad Request",
                            ));
                        }
                    };
                    let Some(timing) = phase_one.as_ref() else {
                        return DispatchOutcome::close(response_for(
                            request,
                            500,
                            "Internal Server Error",
                        ));
                    };
                    let media = match timing {
                        // An NTP timing socket is owned by one classic stream
                        // and cannot be duplicated safely. PTP, in contrast,
                        // is a connection-level cloneable clock source.
                        PhaseOneState::Ntp(_) => {
                            let Some(PhaseOneState::Ntp(timing)) = phase_one.take() else {
                                unreachable!("phase-one variant was just matched")
                            };
                            PreparedType96::prepare_with_timing(local_addr, peer, timing, setup)
                                .await
                        }
                        PhaseOneState::Ptp(ptp_clock) => {
                            PreparedType96::prepare_with_ptp(
                                local_addr,
                                peer,
                                ptp_clock.clone(),
                                setup,
                            )
                            .await
                        }
                    };
                    let media = match media {
                        Ok(media) => media,
                        Err(error) => {
                            log::warn!(
                                target: "audiohub_airplay::airplay2",
                                "AirPlay 2 realtime SETUP could not prepare local media resources: {error}"
                            );
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    let body = match setup_type96_phase_two_body(media.ports()) {
                        Ok(body) => body,
                        Err(_) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    (PreparedMedia::Type96 { stream_id, media }, body)
                }
                STREAM_TYPE_BUFFERED_AUDIO => {
                    let setup = match buffered::parse_phase2(request.body()) {
                        Ok(setup) => setup,
                        Err(error) => {
                            log::warn!(
                                "AirPlay 2 buffered SETUP rejected during strict validation: {error}"
                            );
                            return DispatchOutcome::close(response_for(
                                request,
                                400,
                                "Bad Request",
                            ));
                        }
                    };
                    let Some(PhaseOneState::Ptp(ptp_clock)) = phase_one.as_ref() else {
                        return DispatchOutcome::close(response_for(
                            request,
                            500,
                            "Internal Server Error",
                        ));
                    };
                    let media = match PreparedType103::prepare(
                        local_addr,
                        peer,
                        setup,
                        ptp_clock.clone(),
                    )
                    .await
                    {
                        Ok(media) => media,
                        Err(error) => {
                            log::warn!(
                                "AirPlay 2 buffered SETUP could not prepare local media resources: {error}"
                            );
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    let body = match setup_type103_phase_two_body(media.ports()) {
                        Ok(body) => body,
                        Err(_) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    (PreparedMedia::Type103 { stream_id, media }, body)
                }
                _ => unreachable!("stream type was validated above"),
            };
            let mut response = response_for(request, 200, "OK");
            if add_content_type(&mut response, BINARY_PLIST_CONTENT_TYPE).is_err() {
                return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
            }
            response.body = body;
            let mut outcome = DispatchOutcome::reply(response);
            outcome.start_media = Some(prepared);
            outcome.install_media_permit = new_media_permit;
            outcome.probe_event = Some(ProbeEvent::SetupPhase2 {
                peer: peer.ip(),
                stream_type: Some(stream_type),
            });
            outcome
        }
        ParsedSetup::Session {
            fields,
            remote_timing_port,
            timing_peer,
        } => {
            if event_endpoint_installed || phase_one.is_some() {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            }
            if fields.remote_control_only
                || !matches!(fields.timing_protocol.as_deref(), Some("NTP" | "PTP"))
            {
                return DispatchOutcome::reply(response_for(request, 501, "Not Implemented"));
            }
            let Some(keys) = event_keys.take() else {
                return DispatchOutcome::reply(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            };

            let mut bind_addr = local_addr;
            bind_addr.set_port(0);
            let listener = match TokioTcpListener::bind(bind_addr).await {
                Ok(listener) => listener,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let event_port = match listener.local_addr() {
                Ok(address) => address.port(),
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let (phase_one_state, body) = match fields.timing_protocol.as_deref() {
                Some("NTP") => {
                    let timing =
                        match PreparedTiming::bind(local_addr, peer, remote_timing_port).await {
                            Ok(timing) => timing,
                            Err(_) => {
                                return DispatchOutcome::close(response_for(
                                    request,
                                    500,
                                    "Internal Server Error",
                                ));
                            }
                        };
                    let body = match setup_ntp_phase_one_body(event_port, timing.port()) {
                        Ok(body) => body,
                        Err(_) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    (PhaseOneState::Ntp(timing), body)
                }
                Some("PTP") => {
                    let Some(remote_timing_peer) = timing_peer.as_ref() else {
                        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
                    };
                    let remote_match = match remote_timing_peer.match_for(local_timing_peer_id) {
                        Ok(remote_match) => remote_match,
                        Err(()) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                400,
                                "Bad Request",
                            ));
                        }
                    };
                    let Some(ptp_clock) = ptp_clock.cloned() else {
                        return DispatchOutcome::close(response_for(
                            request,
                            500,
                            "Internal Server Error",
                        ));
                    };
                    let ptp_clock =
                        match ptp_clock.for_session(local_addr.ip(), peer.ip(), remote_match) {
                            Ok(clock) => clock,
                            Err(error) => {
                                log::warn!(
                                    "AirPlay 2 PTP session could not register timing peer {}: {}",
                                    peer.ip(),
                                    error
                                );
                                let (status, reason) = if error.kind() == io::ErrorKind::WouldBlock
                                {
                                    (453, "Not Enough Bandwidth")
                                } else {
                                    (500, "Internal Server Error")
                                };
                                return DispatchOutcome::close(response_for(
                                    request, status, reason,
                                ));
                            }
                        };
                    let body = match setup_ptp_phase_one_body(
                        event_port,
                        local_addr.ip(),
                        local_timing_peer_id,
                        &remote_timing_peer.id,
                        ptp_clock.local_port_identity(),
                    ) {
                        Ok(body) => body,
                        Err(_) => {
                            return DispatchOutcome::close(response_for(
                                request,
                                500,
                                "Internal Server Error",
                            ));
                        }
                    };
                    (PhaseOneState::Ptp(ptp_clock), body)
                }
                _ => unreachable!("timing protocol was validated above"),
            };
            let mut response = response_for(request, 200, "OK");
            if add_content_type(&mut response, BINARY_PLIST_CONTENT_TYPE).is_err() {
                return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
            }
            response.body = body;
            let mut outcome = DispatchOutcome::reply(response);
            let (sender, commands) = event_command_channel(fields.supports_event_volume);
            outcome.install_event_endpoint = Some(PreparedEventEndpoint {
                listener,
                expected_peer: peer.ip(),
                keys,
                initial_update_info: None,
                sender,
                commands,
            });
            outcome.install_phase_one = Some(phase_one_state);
            outcome.probe_event = Some(ProbeEvent::SetupPhase1 {
                peer: peer.ip(),
                fields,
            });
            outcome
        }
    }
}

enum ParsedSetup {
    Session {
        fields: SetupPhase1Fields,
        remote_timing_port: u16,
        timing_peer: Option<ParsedTimingPeer>,
    },
    Streams {
        stream_type: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTimingPeer {
    id: String,
    _supports_receive_matching: bool,
    receive_matching: Option<ParsedReceiveMatching>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedReceiveMatching {
    clock_identity: u64,
    clock_ports: HashMap<String, u16>,
}

impl ParsedTimingPeer {
    fn match_for(&self, local_peer_id: &str) -> Result<Option<PtpRemoteMatch>, ()> {
        let Some(matching) = self.receive_matching.as_ref() else {
            return Ok(None);
        };
        let port_number = *matching.clock_ports.get(local_peer_id).ok_or(())?;
        Ok(Some(PtpRemoteMatch {
            clock_identity: matching.clock_identity,
            port_number,
        }))
    }
}

fn parse_setup(body: &[u8]) -> Result<ParsedSetup, ()> {
    if body.len() < 8 || !body.starts_with(b"bplist00") {
        return Err(());
    }
    let value = Value::from_reader(Cursor::new(body)).map_err(|_| ())?;
    let dictionary = value.as_dictionary().ok_or(())?;

    if let Some(streams) = dictionary.get("streams") {
        let streams = streams.as_array().ok_or(())?;
        if streams.is_empty() {
            return Err(());
        }
        let mut first_stream_type = None;
        for (index, stream) in streams.iter().enumerate() {
            let stream = stream.as_dictionary().ok_or(())?;
            let stream_type = plist_unsigned(stream, "type")?;
            if stream_type > u64::from(u32::MAX) {
                return Err(());
            }
            if index == 0 {
                first_stream_type = Some(stream_type);
            }
        }
        return Ok(ParsedSetup::Streams {
            stream_type: first_stream_type,
        });
    }

    let timing_protocol = dictionary
        .get("timingProtocol")
        .and_then(Value::as_string)
        .filter(|value| matches!(*value, "NTP" | "PTP" | "None"))
        .ok_or(())?
        .to_owned();
    let timing_port = match dictionary.get("timingPort") {
        Some(value) => Some(
            value
                .as_unsigned_integer()
                .filter(|port| *port <= u64::from(u16::MAX))
                .ok_or(())?,
        ),
        None => None,
    };
    let remote_control_only = match dictionary.get("isRemoteControlOnly") {
        Some(value) => value.as_boolean().ok_or(())?,
        None => false,
    };
    let supports_event_volume = dictionary
        .get("sourceVersion")
        .and_then(Value::as_string)
        .is_some_and(source_version_supports_event_volume);
    validate_ignored_session_encryption_fields(dictionary)?;

    let timing_peer = if timing_protocol == "PTP" {
        Some(parse_timing_peer_info(dictionary.get("timingPeerInfo"))?)
    } else {
        None
    };

    match timing_protocol.as_str() {
        "NTP" if timing_port.is_some_and(|port| port != 0) && !remote_control_only => {}
        "PTP"
            if timing_port.is_none()
                && !remote_control_only
                && parse_timing_peer_list(dictionary.get("timingPeerList")).is_ok() => {}
        "None" if timing_port.is_none() && remote_control_only => {}
        _ => return Err(()),
    }

    let remote_timing_port = timing_port
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(0);
    Ok(ParsedSetup::Session {
        fields: SetupPhase1Fields {
            timing_protocol: Some(timing_protocol),
            remote_control_only,
            has_timing_port: timing_port.is_some(),
            supports_event_volume,
        },
        remote_timing_port,
        timing_peer,
    })
}

fn source_version_supports_event_volume(value: &str) -> bool {
    const EVENT_VOLUME_MIN_EXCLUSIVE: [u32; 3] = [354, 54, 5];
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return false;
    }
    let mut parsed = [0u32; 3];
    let mut count = 0usize;
    for component in value.split('.') {
        if component.is_empty() || count == parsed.len() {
            return false;
        }
        let Ok(component) = component.parse::<u32>() else {
            return false;
        };
        if count != 0 && component > 99 {
            return false;
        }
        parsed[count] = component;
        count += 1;
    }
    count != 0 && parsed > EVENT_VOLUME_MIN_EXCLUSIVE
}

fn plist_unsigned(dictionary: &Dictionary, key: &str) -> Result<u64, ()> {
    dictionary
        .get(key)
        .and_then(Value::as_unsigned_integer)
        .ok_or(())
}

fn validate_ignored_session_encryption_fields(dictionary: &Dictionary) -> Result<(), ()> {
    let ekey = match dictionary.get("ekey") {
        Some(value) => Some(value.as_data().ok_or(())?),
        None => None,
    };
    let eiv = match dictionary.get("eiv") {
        Some(value) => Some(value.as_data().ok_or(())?),
        None => None,
    };
    if ekey.is_some() != eiv.is_some() {
        return Err(());
    }

    let Some(ekey) = ekey else {
        return if dictionary.contains_key("et") {
            Err(())
        } else {
            Ok(())
        };
    };
    if eiv.is_none_or(|value| value.len() != 16) {
        return Err(());
    }

    // These phase-one fields are not used by the type-96 media path (the
    // authenticated phase-two `shk` is), but stock senders use two legitimate
    // wire shapes: et=0 with a 16-byte key and et=32 with a 72-byte key. Keep
    // the ignored values strictly shaped so malformed opaque data cannot grow
    // unchecked, while accepting both deployed sender generations.
    match dictionary.get("et") {
        Some(value) => match value.as_unsigned_integer() {
            Some(0) if ekey.len() == 16 => Ok(()),
            Some(32) if ekey.len() == 72 => Ok(()),
            _ => Err(()),
        },
        None if matches!(ekey.len(), 16 | 72) => Ok(()),
        None => Err(()),
    }
}

fn parse_timing_peer_info(value: Option<&Value>) -> Result<ParsedTimingPeer, ()> {
    let dictionary = value.and_then(Value::as_dictionary).ok_or(())?;
    let id = dictionary
        .get("ID")
        .and_then(Value::as_string)
        .filter(|identifier| !identifier.is_empty() && identifier.len() <= 255)
        .ok_or(())?
        .to_owned();
    let addresses = dictionary
        .get("Addresses")
        .and_then(Value::as_array)
        .filter(|addresses| !addresses.is_empty() && addresses.len() <= 64)
        .ok_or(())?;
    if !addresses.iter().all(|address| {
        address
            .as_string()
            .is_some_and(|address| !address.is_empty() && address.len() <= 255)
    }) {
        return Err(());
    }

    let supports_override = match dictionary.get("SupportsClockPortMatchingOverride") {
        Some(value) => value.as_boolean().ok_or(())?,
        None => false,
    };
    let clock_identity = dictionary
        .get("ClockID")
        .map(plist_u64_bit_pattern)
        .transpose()?;
    let clock_ports = dictionary
        .get("ClockPorts")
        .map(parse_clock_ports)
        .transpose()?;
    // AirPlaySupport uses the complete ClockID/ClockPorts tuple when present;
    // the Supports... boolean is exchanged as capability metadata but is not
    // the runtime gate. Some deployed senders publish true before their tuple
    // is populated, while others publish false alongside a usable tuple.
    let receive_matching = match (clock_identity, clock_ports) {
        (None | Some(_), None) => None,
        (Some(clock_identity), Some(clock_ports)) if clock_identity != 0 => {
            Some(ParsedReceiveMatching {
                clock_identity,
                clock_ports,
            })
        }
        _ => return Err(()),
    };

    Ok(ParsedTimingPeer {
        id,
        _supports_receive_matching: supports_override,
        receive_matching,
    })
}

fn parse_timing_peer_list(value: Option<&Value>) -> Result<(), ()> {
    let peers = value
        .and_then(Value::as_array)
        .filter(|peers| !peers.is_empty() && peers.len() <= 64)
        .ok_or(())?;
    for peer in peers {
        parse_timing_peer_info(Some(peer))?;
    }
    Ok(())
}

fn parse_clock_ports(value: &Value) -> Result<HashMap<String, u16>, ()> {
    let dictionary = value.as_dictionary().ok_or(())?;
    if dictionary.is_empty() || dictionary.len() > 64 {
        return Err(());
    }
    dictionary
        .iter()
        .map(|(peer_id, value)| {
            if peer_id.is_empty() || peer_id.len() > 255 {
                return Err(());
            }
            Ok((peer_id.clone(), plist_u16_bit_pattern(value)?))
        })
        .collect()
}

fn plist_u64_bit_pattern(value: &Value) -> Result<u64, ()> {
    value
        .as_unsigned_integer()
        .or_else(|| value.as_signed_integer().map(|value| value as u64))
        .ok_or(())
}

fn plist_u16_bit_pattern(value: &Value) -> Result<u16, ()> {
    if let Some(value) = value.as_unsigned_integer() {
        return u16::try_from(value)
            .ok()
            .filter(|value| *value != 0)
            .ok_or(());
    }
    let value = value.as_signed_integer().ok_or(())?;
    let port = if (1..=i64::from(u16::MAX)).contains(&value) {
        value as u16
    } else if (i64::from(i16::MIN)..=-1).contains(&value) {
        value as i16 as u16
    } else {
        return Err(());
    };
    (port != 0).then_some(port).ok_or(())
}

fn setup_ntp_phase_one_body(event_port: u16, timing_port: u16) -> io::Result<Vec<u8>> {
    let mut dictionary = Dictionary::new();
    dictionary.insert(
        "eventPort".into(),
        Value::Integer(u64::from(event_port).into()),
    );
    dictionary.insert(
        "timingPort".into(),
        Value::Integer(u64::from(timing_port).into()),
    );
    let mut body = Vec::new();
    Value::Dictionary(dictionary)
        .to_writer_binary(&mut body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(body)
}

fn setup_ptp_phase_one_body(
    event_port: u16,
    local_ip: IpAddr,
    local_timing_peer_id: &str,
    remote_timing_peer_id: &str,
    local_port_identity: Option<(u64, u16)>,
) -> io::Result<Vec<u8>> {
    let mut peer_info = Dictionary::new();
    peer_info.insert(
        "Addresses".into(),
        Value::Array(vec![Value::String(local_ip.to_string())]),
    );
    peer_info.insert("ID".into(), Value::String(local_timing_peer_id.to_owned()));
    peer_info.insert("DeviceType".into(), Value::Integer(0i64.into()));
    if let Some((clock_identity, port_number)) = local_port_identity {
        // AirPlaySupport stores this as a signed CFNumber while treating the
        // payload as an opaque 64-bit IEEE 1588 clock identity.
        peer_info.insert(
            "ClockID".into(),
            Value::Integer((clock_identity as i64).into()),
        );
        peer_info.insert(
            "SupportsClockPortMatchingOverride".into(),
            Value::Boolean(true),
        );
        let mut clock_ports = Dictionary::new();
        clock_ports.insert(
            remote_timing_peer_id.to_owned(),
            Value::Integer(i64::from(port_number).into()),
        );
        peer_info.insert("ClockPorts".into(), Value::Dictionary(clock_ports));
    }

    let mut dictionary = Dictionary::new();
    dictionary.insert(
        "eventPort".into(),
        Value::Integer(u64::from(event_port).into()),
    );
    dictionary.insert("timingPort".into(), Value::Integer(0u64.into()));
    dictionary.insert("timingPeerInfo".into(), Value::Dictionary(peer_info));
    encode_binary_dictionary(dictionary)
}

fn setup_type96_phase_two_body(ports: Type96Ports) -> io::Result<Vec<u8>> {
    let mut stream = Dictionary::new();
    stream.insert(
        "type".into(),
        Value::Integer(STREAM_TYPE_REALTIME_AUDIO.into()),
    );
    stream.insert(
        "dataPort".into(),
        Value::Integer(u64::from(ports.data_port).into()),
    );
    stream.insert(
        "controlPort".into(),
        Value::Integer(u64::from(ports.control_port).into()),
    );
    let mut root = Dictionary::new();
    root.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(stream)]),
    );
    encode_binary_dictionary(root)
}

fn setup_type103_phase_two_body(ports: Type103Ports) -> io::Result<Vec<u8>> {
    let mut stream = Dictionary::new();
    stream.insert(
        "type".into(),
        Value::Integer(STREAM_TYPE_BUFFERED_AUDIO.into()),
    );
    stream.insert(
        "dataPort".into(),
        Value::Integer(u64::from(ports.data_port).into()),
    );
    stream.insert(
        "controlPort".into(),
        Value::Integer(u64::from(ports.control_port).into()),
    );
    stream.insert(
        "audioBufferSize".into(),
        Value::Integer(ports.audio_buffer_size.into()),
    );
    let mut root = Dictionary::new();
    root.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(stream)]),
    );
    encode_binary_dictionary(root)
}

fn encode_binary_dictionary(dictionary: Dictionary) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    Value::Dictionary(dictionary)
        .to_writer_binary(&mut body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(body)
}

fn record_response(request: &Request) -> DispatchOutcome {
    let mut response = response_for(request, 200, "OK");
    let Ok(header) = Header::new("Audio-Latency", b"0".to_vec()) else {
        return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
    };
    response.headers.push(header);
    DispatchOutcome::reply(response)
}

fn options_response(request: &Request) -> DispatchOutcome {
    let mut response = response_for(request, 200, "OK");
    let Ok(header) = Header::new(
        "Public",
        // The AP2 bootstrap immediately follows OPTIONS with GET /info and
        // pairing/FairPlay POSTs. Omitting those verbs makes a standards-style
        // sender correctly conclude that the advertised AP2 control flow is
        // unavailable. Keep this list restricted to handlers we own rather
        // than copying a receiver's broader (and partly unsupported) list.
        b"SETUP, RECORD, FLUSH, FLUSHBUFFERED, SETRATEANCHORTIME, TEARDOWN, OPTIONS, POST, GET, GET_PARAMETER, SET_PARAMETER, SETPEERS, SETPEERSX".to_vec(),
    ) else {
        return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
    };
    response.headers.push(header);
    DispatchOutcome::reply(response)
}

fn parse_rate_anchor(request: &Request) -> Result<BufferedRateAnchor, ()> {
    let dictionary = parse_control_binary_plist(request)?;
    let rate = dictionary
        .get("rate")
        .and_then(Value::as_unsigned_integer)
        .ok_or(())?;
    // RTP timestamps are modulo-2^32 wire values. Binary plists may encode
    // bit 31 as a negative i32, so preserve those bits just as FLUSHBUFFERED
    // boundaries do.
    let rtp_time = optional_u32_bits(&dictionary, "rtpTime")?;
    let network_time_secs = optional_u64(&dictionary, "networkTimeSecs")?;
    let network_time_frac = optional_u64_bits(&dictionary, "networkTimeFrac")?;
    let timeline_id =
        optional_u64_bits_alias(&dictionary, "networkTimeTimelineID", "networkTimeId")?;
    log::debug!(
        target: "audiohub_airplay::airplay2",
        "AirPlay 2 rate anchor shape rate={} rtp={} seconds={} fraction={} timeline={} keys={:?}",
        rate,
        rtp_time.is_some(),
        network_time_secs.is_some(),
        network_time_frac.is_some(),
        timeline_id.is_some(),
        dictionary.keys().collect::<Vec<_>>()
    );
    match rate {
        0 => Ok(BufferedRateAnchor {
            playing: false,
            rtp_time,
            network_time_secs,
            network_time_frac,
            timeline_id,
        }),
        1 if rtp_time.is_some()
            && network_time_secs.is_some()
            && network_time_frac.is_some()
            && timeline_id.is_some() =>
        {
            Ok(BufferedRateAnchor {
                playing: true,
                rtp_time,
                network_time_secs,
                network_time_frac,
                timeline_id,
            })
        }
        1 => Err(()),
        _ => Err(()),
    }
}

fn optional_u64(dictionary: &Dictionary, key: &str) -> Result<Option<u64>, ()> {
    dictionary
        .get(key)
        .map(|value| value.as_unsigned_integer().ok_or(()))
        .transpose()
}

// Binary plists can encode a protocol uint64 whose high bit is set as a
// signed i64. Preserve its wire bits instead of rejecting a legitimate PTP
// fraction or clock identity.
fn optional_u64_bits(dictionary: &Dictionary, key: &str) -> Result<Option<u64>, ()> {
    dictionary
        .get(key)
        .map(|value| {
            value
                .as_unsigned_integer()
                .or_else(|| value.as_signed_integer().map(|value| value as u64))
                .ok_or(())
        })
        .transpose()
}

fn optional_u64_bits_alias(
    dictionary: &Dictionary,
    preferred: &str,
    legacy: &str,
) -> Result<Option<u64>, ()> {
    let preferred_value = optional_u64_bits(dictionary, preferred)?;
    let legacy_value = optional_u64_bits(dictionary, legacy)?;
    match (preferred_value, legacy_value) {
        (Some(left), Some(right)) if left != right => Err(()),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn parse_buffered_flush(request: &Request) -> Result<BufferedFlush, ()> {
    let dictionary = parse_control_binary_plist(request)?;
    let from_sequence = optional_buffered_sequence(&dictionary, "flushFromSeq")?;
    let from_timestamp = optional_u32_bits(&dictionary, "flushFromTS")?;
    let until_sequence = optional_buffered_sequence(&dictionary, "flushUntilSeq")?;
    let until_timestamp = optional_u32_bits(&dictionary, "flushUntilTS")?;

    let from = paired_wire_point(from_sequence, from_timestamp)?;
    let until = paired_wire_point(until_sequence, until_timestamp)?;
    match (from, until) {
        // Native FLUSHBUFFERED always supplies the exclusive `until`
        // boundary. A body-less full flush is the separate FLUSH method;
        // treating an empty plist as the same operation would silently accept
        // a malformed request and discard authenticated media unexpectedly.
        (None, None) => Err(()),
        (None, Some(until)) => Ok(BufferedFlush::Immediate { until }),
        (Some(from), Some(until)) => {
            let distance =
                until.sequence.wrapping_sub(from.sequence) & (MAX_BUFFERED_SEQUENCE as u32);
            if distance >= ((MAX_BUFFERED_SEQUENCE as u32) + 1) / 2 {
                Err(())
            } else {
                Ok(BufferedFlush::Deferred { from, until })
            }
        }
        (Some(_), None) => Err(()),
    }
}

fn optional_buffered_sequence(dictionary: &Dictionary, key: &str) -> Result<Option<u32>, ()> {
    dictionary
        .get(key)
        .map(|value| {
            let value = value.as_unsigned_integer().ok_or(())?;
            if value > MAX_BUFFERED_SEQUENCE {
                return Err(());
            }
            u32::try_from(value).map_err(|_| ())
        })
        .transpose()
}

// Plist integers do not have an unsigned 32-bit type. Senders commonly encode
// timestamps with bit 31 set as a negative signed integer, so preserve the
// exact two's-complement wire value while rejecting values wider than 32 bits.
fn optional_u32_bits(dictionary: &Dictionary, key: &str) -> Result<Option<u32>, ()> {
    dictionary
        .get(key)
        .map(|value| {
            if let Some(value) = value.as_unsigned_integer() {
                return u32::try_from(value).map_err(|_| ());
            }
            value
                .as_signed_integer()
                .and_then(|value| i32::try_from(value).ok())
                .map(|value| value as u32)
                .ok_or(())
        })
        .transpose()
}

fn paired_wire_point(
    sequence: Option<u32>,
    timestamp: Option<u32>,
) -> Result<Option<WirePoint>, ()> {
    match (sequence, timestamp) {
        (None, None) => Ok(None),
        (Some(sequence), Some(timestamp)) => Ok(Some(WirePoint {
            sequence,
            timestamp,
        })),
        _ => Err(()),
    }
}

fn parse_control_binary_plist(request: &Request) -> Result<Dictionary, ()> {
    if !has_content_type(request, BINARY_PLIST_CONTENT_TYPE)
        || request.body().len() < 8
        || !request.body().starts_with(b"bplist00")
    {
        return Err(());
    }
    Value::from_reader(Cursor::new(request.body()))
        .map_err(|_| ())?
        .into_dictionary()
        .ok_or(())
}

fn teardown_scope(request: &Request) -> TeardownScope {
    let Ok(dictionary) = parse_control_binary_plist(request) else {
        return TeardownScope::Ignore;
    };
    match dictionary.get("streams") {
        Some(Value::Array(_)) => TeardownScope::Stream,
        Some(_) => TeardownScope::Ignore,
        None => TeardownScope::Session,
    }
}

fn safe_control_plist_shape(request: &Request) -> String {
    let Ok(dictionary) = parse_control_binary_plist(request) else {
        return "invalid-binary-plist".to_owned();
    };
    dictionary
        .iter()
        .map(|(key, value)| {
            let kind = match value {
                Value::Array(_) => "array",
                Value::Boolean(_) => "bool",
                Value::Data(_) => "data",
                Value::Date(_) => "date",
                Value::Dictionary(_) => "dict",
                Value::Integer(integer) if integer.as_unsigned().is_some() => "uint",
                Value::Integer(_) => "sint",
                Value::Real(_) => "real",
                Value::String(_) => "string",
                Value::Uid(_) => "uid",
                _ => "other",
            };
            format!("{key}:{kind}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn get_parameter_response(
    request: &Request,
    current_volume: Option<ReceiverVolumeSnapshot>,
) -> DispatchOutcome {
    if !has_content_type(request, TEXT_PARAMETERS_CONTENT_TYPE) {
        return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
    }
    let Ok(body) = std::str::from_utf8(request.body()) else {
        return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
    };
    if body.trim() != "volume" {
        return DispatchOutcome::reply(response_for(request, 200, "OK"));
    }
    let Some(current_volume) = current_volume else {
        return DispatchOutcome::reply(response_for(request, 503, "Service Unavailable"));
    };
    let mut response = response_for(request, 200, "OK");
    if add_content_type(&mut response, TEXT_PARAMETERS_CONTENT_TYPE).is_err() {
        return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
    }
    response.body = format!("volume: {:.6}\r\n", current_volume.legacy_text_db()).into_bytes();
    DispatchOutcome::reply(response)
}

fn now_playing_command_response(request: &Request) -> DispatchOutcome {
    let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
    if !has_content_type(request, BINARY_PLIST_CONTENT_TYPE) || request.body().is_empty() {
        return outcome;
    }
    match metadata::parse_now_playing_command(request.body()) {
        Ok(Some(update)) => outcome.now_playing = Some(NowPlayingWireUpdate::Rich(update)),
        Ok(None) => {}
        Err(error) => log::debug!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 ignored malformed now-playing command: {error}"
        ),
    }
    outcome
}

fn set_parameter_response(request: &Request) -> DispatchOutcome {
    if request.body().is_empty() {
        return DispatchOutcome::reply(response_for(request, 200, "OK"));
    }
    if has_content_type(request, "application/x-dmap-tagged") {
        let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
        match metadata::parse_dmap(request.body()) {
            Ok(Some(metadata)) => {
                outcome.now_playing = Some(NowPlayingWireUpdate::Metadata(metadata));
            }
            Ok(None) => {}
            Err(error) => log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 ignored malformed DMAP metadata: {error}"
            ),
        }
        return outcome;
    }
    if has_content_type(request, "image/none") {
        let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
        outcome.now_playing = Some(NowPlayingWireUpdate::Artwork(None));
        return outcome;
    }
    for content_type in ["image/jpeg", "image/png"] {
        if has_content_type(request, content_type) {
            let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
            match metadata::parse_artwork_bytes(content_type, request.body()) {
                Ok(artwork) => {
                    outcome.now_playing = Some(NowPlayingWireUpdate::Artwork(Some(artwork)));
                }
                Err(error) => {
                    log::debug!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 ignored malformed {content_type} artwork: {error}"
                    );
                }
            }
            return outcome;
        }
    }
    if !has_content_type(request, TEXT_PARAMETERS_CONTENT_TYPE) {
        // Decorative metadata is not allowed to tear down an otherwise valid
        // audio stream merely because a sender uses an extension we do not
        // understand.
        return DispatchOutcome::reply(response_for(request, 200, "OK"));
    }
    let Ok(body) = std::str::from_utf8(request.body()) else {
        return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
    };
    let mut volume = None;
    let mut progress = None;
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(value) = line.strip_prefix("volume:") {
            if volume.is_some() {
                return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
            }
            let Ok(db) = value.trim().parse::<f32>() else {
                return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
            };
            if !db.is_finite()
                || (db != AIRPLAY_VOLUME_MUTE_DB
                    && !(AIRPLAY_VOLUME_MIN_DB..=AIRPLAY_VOLUME_MAX_DB).contains(&db))
            {
                return DispatchOutcome::reply(response_for(request, 400, "Bad Request"));
            }
            volume = Some(db);
        } else if let Some(value) = line.strip_prefix("progress:") {
            if progress.is_none() {
                progress = parse_progress_parameter(value.trim());
            }
        }
    }
    let mut outcome = DispatchOutcome::reply(response_for(request, 200, "OK"));
    outcome.volume_db = volume;
    outcome.now_playing =
        progress.map(|(elapsed_ms, duration_ms)| NowPlayingWireUpdate::Progress {
            elapsed_ms,
            duration_ms,
        });
    outcome
}

fn parse_progress_parameter(value: &str) -> Option<(u64, u64)> {
    let mut fields = value.split('/');
    let start = fields.next()?.trim().parse::<u64>().ok()?;
    let current = fields.next()?.trim().parse::<u64>().ok()?;
    let end = fields.next()?.trim().parse::<u64>().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let elapsed_frames = current.saturating_sub(start);
    let duration_frames = end.saturating_sub(start);
    Some((
        elapsed_frames.saturating_mul(1000) / 44_100,
        duration_frames.saturating_mul(1000) / 44_100,
    ))
}

struct PreparedEventEndpoint {
    listener: TokioTcpListener,
    expected_peer: IpAddr,
    keys: ChannelKeys,
    initial_update_info: Option<Vec<u8>>,
    sender: EventCommandSender,
    commands: mpsc::Receiver<OutboundEventCommand>,
}

fn event_command_channel(
    supports_device_volume: bool,
) -> (EventCommandSender, mpsc::Receiver<OutboundEventCommand>) {
    let (commands, receiver) = mpsc::channel(EVENT_COMMAND_QUEUE_CAPACITY);
    let ready = Arc::new(AtomicBool::new(false));
    let record_started = EventStartSignal::new();
    (
        EventCommandSender {
            commands,
            ready,
            record_started,
            supports_device_volume,
        },
        receiver,
    )
}

struct EventReadyGuard {
    ready: Arc<AtomicBool>,
}

impl Drop for EventReadyGuard {
    fn drop(&mut self) {
        self.ready.store(false, Ordering::Release);
    }
}

fn spawn_event_endpoint(endpoint: PreparedEventEndpoint) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = run_event_endpoint(endpoint).await;
        if let Err(error) = result {
            // Losing the reverse event channel removes receiver-originated
            // volume synchronization and can make current Apple senders abort
            // setup. Keep the diagnostic bounded and redacted, but visible in
            // the daemon's normal operational log.
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 event channel closed: {}",
                error.safe_label(),
            );
        }
    })
}

async fn run_event_endpoint(endpoint: PreparedEventEndpoint) -> Result<(), ConnectionError> {
    let ready = Arc::clone(&endpoint.sender.ready);
    // This guard is held across every await in the endpoint. Normal EOF,
    // protocol failure, and Tokio cancellation all drop it, so no sender clone
    // can retain stale event-channel authority.
    let _ready_guard = EventReadyGuard {
        ready: Arc::clone(&ready),
    };
    let accepted = timeout(EVENT_ACCEPT_TIMEOUT, async {
        for _ in 0..MAX_EVENT_ACCEPT_ATTEMPTS {
            let (socket, peer) = endpoint
                .listener
                .accept()
                .await
                .map_err(|_| ConnectionError::Io)?;
            if peer.ip() == endpoint.expected_peer {
                return Ok(socket);
            }
            drop(socket);
        }
        Err(ConnectionError::Io)
    })
    .await
    .map_err(|_| ConnectionError::TimedOut)??;

    run_event_connection(
        accepted,
        endpoint.keys,
        endpoint.initial_update_info,
        endpoint.commands,
        ready,
        endpoint.sender.record_started,
    )
    .await
}

async fn run_event_connection(
    mut socket: TcpStream,
    keys: ChannelKeys,
    initial_update_info: Option<Vec<u8>>,
    mut commands: mpsc::Receiver<OutboundEventCommand>,
    ready: Arc<AtomicBool>,
    record_started: EventStartSignal,
) -> Result<(), ConnectionError> {
    let mut channel = keys.into_codec();
    let mut next_cseq = 0u32;
    if let Some(body) = initial_update_info {
        record_started.wait().await;
        send_event_command(&mut socket, &mut channel, next_cseq, &body)
            .await
            .map_err(|error| error.as_connection_error())?;
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 initial updateInfo accepted on event channel"
        );
        next_cseq = next_cseq.wrapping_add(1);
    }
    ready.store(true, Ordering::Release);
    let mut wire = [0u8; READ_CHUNK_BYTES];
    loop {
        enum EventAction {
            Command(Option<OutboundEventCommand>),
            Read(io::Result<usize>),
        }
        let action = tokio::select! {
            command = commands.recv() => EventAction::Command(command),
            read = socket.read(&mut wire) => EventAction::Read(read),
        };
        match action {
            EventAction::Command(Some(command)) => {
                if !command.attempt.begin_write() {
                    let _ = command
                        .completion
                        .send(Err(EventCommandSendError::before_write(
                            io::ErrorKind::TimedOut,
                            "AirPlay event command was cancelled before write",
                        )));
                    continue;
                }
                let cseq = next_cseq;
                next_cseq = next_cseq.wrapping_add(1);
                let result =
                    send_event_command(&mut socket, &mut channel, cseq, &command.body).await;
                let _ = command.completion.send(result);
                result.map_err(|error| error.as_connection_error())?;
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 MediaRemote command accepted on event channel cseq={cseq}"
                );
            }
            EventAction::Command(None) => return Ok(()),
            EventAction::Read(Ok(0)) => return Ok(()),
            EventAction::Read(Ok(read)) => {
                // The sender normally writes only RTSP responses after an
                // AudioHub command. Any independently received frame is still
                // authenticated and bounded, but no unsolicited semantic is
                // claimed by the audio-only receiver.
                channel
                    .inbound
                    .feed(&wire[..read])
                    .map_err(|_| ConnectionError::Encryption)?;
            }
            EventAction::Read(Err(_)) => return Err(ConnectionError::Io),
        }
    }
}

async fn send_event_command<S>(
    socket: &mut S,
    channel: &mut EncryptedChannel,
    cseq: u32,
    body: &[u8],
) -> Result<(), EventCommandSendError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_event_command_with_timeout(socket, channel, cseq, body, EVENT_COMMAND_TIMEOUT).await
}

async fn send_event_command_with_timeout<S>(
    socket: &mut S,
    channel: &mut EncryptedChannel,
    cseq: u32,
    body: &[u8],
    command_timeout: Duration,
) -> Result<(), EventCommandSendError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let transaction = async {
        let mut plaintext = format!(
            "POST /command RTSP/1.0\r\nCSeq: {cseq}\r\nContent-Length: {}\r\nContent-Type: {BINARY_PLIST_CONTENT_TYPE}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        plaintext.extend_from_slice(body);
        let wire = channel
            .outbound
            .encode(&plaintext)
            .map_err(|_| ConnectionError::Encryption)?;
        socket
            .write_all(&wire)
            .await
            .map_err(|_| ConnectionError::Io)?;

        let mut response = Vec::new();
        let mut wire = [0u8; READ_CHUNK_BYTES];
        loop {
            let read = socket
                .read(&mut wire)
                .await
                .map_err(|_| ConnectionError::Io)?;
            if read == 0 {
                return Err(ConnectionError::Io);
            }
            let decoded = channel
                .inbound
                .feed(&wire[..read])
                .map_err(|_| ConnectionError::Encryption)?;
            if response.len().saturating_add(decoded.len()) > MAX_EVENT_RESPONSE_BYTES {
                return Err(ConnectionError::Framing);
            }
            response.extend_from_slice(&decoded);
            if event_response_is_complete(&response, cseq)? {
                return Ok(());
            }
        }
    };

    match timeout(command_timeout, transaction).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(EventCommandSendError::during_transaction(error)),
        Err(_) => Err(EventCommandSendError::during_transaction(
            ConnectionError::TimedOut,
        )),
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn event_response_is_complete(
    response: &[u8],
    expected_cseq: u32,
) -> Result<bool, ConnectionError> {
    if response.len() > MAX_EVENT_RESPONSE_BYTES {
        return Err(ConnectionError::Framing);
    }
    let Some(headers_end) = find_header_end(response) else {
        return if response.len() == MAX_EVENT_RESPONSE_BYTES {
            Err(ConnectionError::Framing)
        } else {
            Ok(false)
        };
    };
    let content_length = validate_event_response(&response[..headers_end], expected_cseq)?;
    let message_length = headers_end
        .checked_add(content_length)
        .filter(|length| *length <= MAX_EVENT_RESPONSE_BYTES)
        .ok_or(ConnectionError::Framing)?;
    match response.len().cmp(&message_length) {
        std::cmp::Ordering::Less => Ok(false),
        std::cmp::Ordering::Equal => Ok(true),
        // There is no safe request-boundary association for unsolicited bytes
        // coalesced after this transaction. Close instead of allowing them to
        // masquerade as the next command's response.
        std::cmp::Ordering::Greater => Err(ConnectionError::Framing),
    }
}

fn validate_event_response(headers: &[u8], expected_cseq: u32) -> Result<usize, ConnectionError> {
    let status_line_end = headers
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or(ConnectionError::Framing)?;
    let status_line =
        std::str::from_utf8(&headers[..status_line_end]).map_err(|_| ConnectionError::Framing)?;
    let mut fields = status_line.split_ascii_whitespace();
    if fields.next() != Some("RTSP/1.0") {
        return Err(ConnectionError::Framing);
    }
    let status = fields
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or(ConnectionError::Framing)?;
    if !(200..300).contains(&status) {
        return Err(ConnectionError::Framing);
    }
    let text = std::str::from_utf8(headers).map_err(|_| ConnectionError::Framing)?;
    let mut cseq = None;
    let mut content_length = None;
    for line in text.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(ConnectionError::Framing);
        };
        if name.eq_ignore_ascii_case("CSeq") {
            if cseq.is_some() {
                return Err(ConnectionError::Framing);
            }
            cseq = value.trim().parse::<u32>().ok();
            if cseq.is_none() {
                return Err(ConnectionError::Framing);
            }
        } else if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(ConnectionError::Framing);
            }
            content_length = value.trim().parse::<usize>().ok();
            if content_length.is_none() {
                return Err(ConnectionError::Framing);
            }
        }
    }
    // Current macOS senders omit CSeq on this authenticated event response,
    // while older AirTunes captures echo it but can omit Content-Length. This
    // actor permits exactly one in-flight command, so either observed shape is
    // unambiguous. A present CSeq must match exactly; an absent CSeq is accepted
    // only with an explicit empty body. Never accept both fields missing.
    match cseq {
        Some(cseq) if cseq != expected_cseq => return Err(ConnectionError::Framing),
        None if content_length != Some(0) => return Err(ConnectionError::Framing),
        _ => {}
    }
    // Apple's AirPlay 2 event peer omits Content-Length on an otherwise empty
    // successful response. RTSP responses with no declared length therefore
    // have a zero-length body; an explicitly supplied length is still parsed
    // strictly above so duplicate and malformed fields remain framing errors.
    Ok(content_length.unwrap_or(0))
}

async fn handle_pair_setup(
    request: &Request,
    peer: IpAddr,
    config: &ServerConfig,
    pair_setup_limiter: &PairSetupLimiter,
    pairing: &mut Option<PairSetupServer>,
) -> DispatchOutcome {
    if !has_pairing_content_type_or_none(request) || request.body().len() > MAX_PAIRING_BODY_BYTES {
        *pairing = None;
        return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
    }
    let limits = pairing_tlv_limits();
    let message = match tlv8::decode(request.body(), limits) {
        Ok(message) => message,
        Err(_) => {
            *pairing = None;
            return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
        }
    };
    let state = match message.state() {
        Ok(Some(state)) => state,
        _ => {
            *pairing = None;
            return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
        }
    };
    match state {
        1 => {
            if !pair_setup_limiter.allow(peer) {
                *pairing = None;
                let mut response = response_for(request, 429, "Too Many Requests");
                if let Ok(header) = Header::new("Retry-After", b"1".to_vec()) {
                    response.headers.push(header);
                }
                return DispatchOutcome::close(response);
            }
            if !valid_m1(&message) {
                *pairing = None;
                return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
            }

            let password = Zeroizing::new(
                config
                    .password
                    .as_deref()
                    .unwrap_or(DEFAULT_TRANSIENT_PIN)
                    .as_bytes()
                    .to_vec(),
            );
            let mut salt = [0u8; 16];
            let mut server_secret = [0u8; 32];
            let mut rng = OsRng;
            rng.fill_bytes(&mut salt);
            rng.fill_bytes(&mut server_secret);
            let server = match tokio::task::spawn_blocking(move || {
                PairSetupServer::new(password.as_slice(), salt, server_secret)
            })
            .await
            {
                Ok(Ok(server)) => server,
                _ => {
                    *pairing = None;
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let challenge = server.challenge();
            let body = match tlv8::encode(
                &[
                    Tlv8Field::new(TLV_STATE, vec![2]),
                    Tlv8Field::new(TLV_SALT, challenge.salt().to_vec()),
                    Tlv8Field::new(TLV_PUBLIC_KEY, challenge.server_public_key().to_vec()),
                ],
                limits,
            ) {
                Ok(body) => body,
                Err(_) => {
                    *pairing = None;
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            *pairing = Some(server);
            pairing_response(request, body, false)
        }
        3 => {
            let Some(server) = pairing.take() else {
                return DispatchOutcome::close(response_for(
                    request,
                    455,
                    "Method Not Valid in This State",
                ));
            };
            let client_public_key = match message.require_unique(TLV_PUBLIC_KEY) {
                Ok(value) => value.to_vec(),
                Err(_) => {
                    return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
                }
            };
            let client_proof = match message.require_unique(TLV_PROOF) {
                Ok(value) => value.to_vec(),
                Err(_) => {
                    return DispatchOutcome::close(response_for(request, 400, "Bad Request"));
                }
            };
            let verified = match tokio::task::spawn_blocking(move || {
                server.verify(&client_public_key, &client_proof)
            })
            .await
            {
                Ok(Ok(verified)) => verified,
                Ok(Err(_)) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        470,
                        "Connection Authorization Required",
                    ));
                }
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let control_keys = match derive_control_keys(verified.session_key()) {
                Ok(keys) => keys,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let event_keys = match derive_event_keys(verified.session_key()) {
                Ok(keys) => keys,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let body = match tlv8::encode(
                &[
                    Tlv8Field::new(TLV_STATE, vec![4]),
                    Tlv8Field::new(TLV_PROOF, verified.server_proof().to_vec()),
                ],
                limits,
            ) {
                Ok(body) => body,
                Err(_) => {
                    return DispatchOutcome::close(response_for(
                        request,
                        500,
                        "Internal Server Error",
                    ));
                }
            };
            let mut outcome = pairing_response(request, body, false);
            outcome.install_control = Some(control_keys);
            outcome.install_event_keys = Some(event_keys);
            outcome
        }
        _ => {
            *pairing = None;
            DispatchOutcome::close(response_for(request, 400, "Bad Request"))
        }
    }
}

fn valid_m1(message: &tlv8::Tlv8Message) -> bool {
    let method = matches!(
        message.get_unique(TLV_METHOD),
        Ok(Some([PAIR_SETUP_METHOD]))
    );
    let transient = match message.get_unique(TLV_FLAGS) {
        Ok(Some(flags)) if !flags.is_empty() && flags.len() <= 4 => {
            flags.iter().enumerate().fold(0u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (index * 8))
            }) & u32::from(TRANSIENT_PAIRING_FLAG)
                != 0
        }
        _ => false,
    };
    method && transient
}

fn pairing_response(request: &Request, body: Vec<u8>, close_after: bool) -> DispatchOutcome {
    let mut response = response_for(request, 200, "OK");
    // AirPlay's RTSP pairing wire uses octet-stream even though the body is
    // TLV8. Accept the HomeKit media type on input for compatibility, but
    // emit the stock AirPlay response type deterministically.
    if add_content_type(&mut response, OCTET_STREAM_CONTENT_TYPE).is_err() {
        return DispatchOutcome::close(response_for(request, 500, "Internal Server Error"));
    }
    response.body = body;
    DispatchOutcome {
        response,
        install_control: None,
        install_event_keys: None,
        install_event_endpoint: None,
        install_phase_one: None,
        install_media_permit: None,
        start_media: None,
        volume_db: None,
        now_playing: None,
        flush_media: None,
        set_media_rate: None,
        teardown_media: false,
        probe_event: None,
        close_after,
    }
}

fn response_for(request: &Request, status: u16, reason: &str) -> Response {
    let mut response = Response::new(status, reason);
    response.version = request.version().to_owned();
    if let Ok(header) = Header::new(
        "Server",
        format!("AirTunes/{}", super::info::SOURCE_VERSION),
    ) {
        response.headers.push(header);
    }
    if let Some(cseq) = request.header("CSeq") {
        if let Ok(header) = Header::new("CSeq", cseq.to_vec()) {
            response.headers.push(header);
        }
    }
    response
}

fn add_content_type(response: &mut Response, content_type: &str) -> Result<(), ()> {
    response
        .headers
        .push(Header::new("Content-Type", content_type.as_bytes().to_vec()).map_err(|_| ())?);
    Ok(())
}

fn unique_apple_challenge(request: &Request) -> Result<Option<&[u8]>, AppleChallengeError> {
    let mut values = request
        .headers()
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("Apple-Challenge"))
        .map(Header::value);
    let first = values.next();
    if values.next().is_some() {
        return Err(AppleChallengeError::MalformedChallenge);
    }
    Ok(first)
}

fn has_ambiguous_cseq(request: &Request) -> bool {
    request
        .headers()
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("CSeq"))
        .count()
        > 1
}

fn has_pairing_content_type(request: &Request) -> bool {
    has_content_type(request, OCTET_STREAM_CONTENT_TYPE)
        || has_content_type(request, PAIRING_TLV8_CONTENT_TYPE)
        // Music 1.6.5 on macOS 26.5.2 labels its nine-byte transient TLV8 M1
        // with the generic AirPlay plist type. The dedicated endpoint still
        // validates the body as bounded TLV8 before touching pairing state.
        || has_content_type(request, BINARY_PLIST_CONTENT_TYPE)
}

fn has_pairing_content_type_or_none(request: &Request) -> bool {
    let count = request
        .headers()
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("Content-Type"))
        .count();
    count == 0 || (count == 1 && has_pairing_content_type(request))
}

fn has_content_type(request: &Request, expected: &str) -> bool {
    let mut values = request
        .headers()
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("Content-Type"));
    let Some(value) = values.next().map(Header::value) else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = std::str::from_utf8(value) else {
        return false;
    };
    value
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(expected))
}

fn unique_ascii_header(request: &Request, name: &str, maximum: usize) -> Option<String> {
    let mut values = request
        .headers()
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case(name));
    let value = values.next()?.value();
    if values.next().is_some()
        || value.is_empty()
        || value.len() > maximum
        || !value.iter().all(u8::is_ascii_graphic)
    {
        return None;
    }
    Some(std::str::from_utf8(value).ok()?.to_owned())
}

fn pairing_tlv_limits() -> Tlv8Limits {
    Tlv8Limits {
        max_message_bytes: MAX_PAIRING_BODY_BYTES,
        max_fields: 16,
        max_value_bytes: 512,
    }
}

struct PairSetupLimiter {
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,
}

impl PairSetupLimiter {
    fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, peer: IpAddr) -> bool {
        const BURST: f64 = 3.0;
        const REFILL_PER_SECOND: f64 = 1.0;
        const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
        const MAX_TRACKED_IPS: usize = 4_096;

        let now = Instant::now();
        let Ok(mut buckets) = self.buckets.lock() else {
            return false;
        };
        if buckets.len() >= MAX_TRACKED_IPS && !buckets.contains_key(&peer) {
            buckets.retain(|_, bucket| now.duration_since(bucket.last_seen) < STALE_AFTER);
            if buckets.len() >= MAX_TRACKED_IPS {
                return false;
            }
        }
        let bucket = buckets.entry(peer).or_insert(TokenBucket {
            tokens: BURST,
            last_refill: now,
            last_seen: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * REFILL_PER_SECOND).min(BURST);
        bucket.last_refill = now;
        bucket.last_seen = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppleResponseAttemptError {
    Challenge(AppleChallengeError),
    Repeated,
    RateLimited,
    Busy,
    WorkerFailed,
}

struct AppleChallengeExecutor {
    limiter: TokenBucketLimiter,
    permits: Arc<Semaphore>,
}

struct AppleChallengePermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl AppleChallengeExecutor {
    fn new() -> Self {
        Self::with_limits(
            APPLE_CHALLENGE_BURST,
            APPLE_CHALLENGE_REFILL_PER_SECOND,
            MAX_CONCURRENT_APPLE_RESPONSES,
        )
    }

    fn with_limits(burst: f64, refill_per_second: f64, maximum_concurrent: usize) -> Self {
        Self {
            limiter: TokenBucketLimiter::new(burst, refill_per_second),
            permits: Arc::new(Semaphore::new(maximum_concurrent)),
        }
    }

    fn reserve(&self, peer: IpAddr) -> Result<AppleChallengePermit, AppleResponseAttemptError> {
        if !self.limiter.available(peer) {
            return Err(AppleResponseAttemptError::RateLimited);
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppleResponseAttemptError::Busy)?;
        if !self.limiter.allow(peer) {
            return Err(AppleResponseAttemptError::RateLimited);
        }
        Ok(AppleChallengePermit { _permit: permit })
    }

    async fn respond(
        &self,
        peer: IpAddr,
        provider: Arc<dyn super::apple_challenge::AppleResponseProvider>,
        prepared: apple_challenge::PreparedAppleResponse,
    ) -> Result<String, AppleResponseAttemptError> {
        let permit = self.reserve(peer)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            apple_challenge::respond(provider.as_ref(), prepared)
        })
        .await
        .map_err(|_| AppleResponseAttemptError::WorkerFailed)?
        .map_err(AppleResponseAttemptError::Challenge)
    }
}

struct TokenBucketLimiter {
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,
    burst: f64,
    refill_per_second: f64,
}

impl TokenBucketLimiter {
    fn new(burst: f64, refill_per_second: f64) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            burst,
            refill_per_second,
        }
    }

    fn allow(&self, peer: IpAddr) -> bool {
        const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
        const MAX_TRACKED_IPS: usize = 4_096;

        let now = Instant::now();
        let Ok(mut buckets) = self.buckets.lock() else {
            return false;
        };
        if buckets.len() >= MAX_TRACKED_IPS && !buckets.contains_key(&peer) {
            buckets.retain(|_, bucket| now.duration_since(bucket.last_seen) < STALE_AFTER);
            if buckets.len() >= MAX_TRACKED_IPS {
                return false;
            }
        }
        let bucket = buckets.entry(peer).or_insert(TokenBucket {
            tokens: self.burst,
            last_refill: now,
            last_seen: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_second).min(self.burst);
        bucket.last_refill = now;
        bucket.last_seen = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }

    fn available(&self, peer: IpAddr) -> bool {
        let now = Instant::now();
        let Ok(mut buckets) = self.buckets.lock() else {
            return false;
        };
        let Some(bucket) = buckets.get_mut(&peer) else {
            return self.burst >= 1.0;
        };
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_second).min(self.burst);
        bucket.last_refill = now;
        bucket.last_seen = now;
        bucket.tokens >= 1.0
    }
}

fn stable_mac(identity: &ReceiverIdentity) -> [u8; 6] {
    let public = identity.public_key();
    let mut mac: [u8; 6] = public[..6].try_into().expect("fixed public key length");
    mac[0] = (mac[0] | 0x02) & 0xfe;
    mac
}

#[cfg(test)]
mod tests {
    use super::super::crypto::{FrameDecoder, FrameEncoder};
    use super::*;
    use base64::Engine as _;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Condvar, Mutex as StdMutex};

    fn config() -> ServerConfig {
        ServerConfig {
            name: "AudioHub Test".into(),
            mac: [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee],
            password: None,
            identity: ReceiverIdentity::generate(),
            apple_response: Arc::new(BundledAirPortExpressProvider),
            fairplay: BundledFairPlayProvider,
            media_permits: Arc::new(Semaphore::new(MAX_ACTIVE_MEDIA_STREAMS)),
            ptp_clock: OnceLock::new(),
            ptp_test_ephemeral: true,
            features: FEATURES,
            receiver_volume: Some(ReceiverVolumeProvider::fixed(
                ReceiverVolumeSnapshot::new(-18.0, false).unwrap(),
            )),
        }
    }

    fn make_request(
        method: &str,
        target: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Request {
        let mut wire = format!("{method} {target} RTSP/1.0\r\nCSeq: 7\r\n").into_bytes();
        if let Some(content_type) = content_type {
            wire.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        wire.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        wire.extend_from_slice(body);
        parse_wire_request(&wire)
    }

    fn parse_wire_request(wire: &[u8]) -> Request {
        let mut decoder = RequestDecoder::new(RequestLimits::default()).unwrap();
        decoder.feed(wire).unwrap();
        decoder
            .next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, true)
            })
            .unwrap()
            .expect("complete test request")
    }

    fn binary_plist(dictionary: Dictionary) -> Vec<u8> {
        let mut body = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_binary(&mut body)
            .unwrap();
        body
    }

    fn test_state() -> ConnectionState {
        let (remote_tx, _remote_rx) = watch::channel(None);
        ConnectionState::new(remote_tx).unwrap()
    }

    fn media_permits() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(MAX_ACTIVE_MEDIA_STREAMS))
    }

    struct FailingAppleResponseProvider;

    impl super::super::apple_challenge::AppleResponseProvider for FailingAppleResponseProvider {
        fn private_pkcs1_v15(&self, _input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
            Err(AppleChallengeError::ProviderUnavailable)
        }
    }

    struct BlockingAppleResponseProvider {
        entered: AtomicUsize,
        gate: StdMutex<bool>,
        changed: Condvar,
    }

    impl BlockingAppleResponseProvider {
        fn new() -> Self {
            Self {
                entered: AtomicUsize::new(0),
                gate: StdMutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn wait_until_entered(&self, count: usize) {
            let mut open = self.gate.lock().unwrap();
            while self.entered.load(Ordering::Acquire) < count {
                open = self.changed.wait(open).unwrap();
            }
        }

        fn release(&self) {
            *self.gate.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    impl super::super::apple_challenge::AppleResponseProvider for BlockingAppleResponseProvider {
        fn private_pkcs1_v15(&self, _input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
            self.entered.fetch_add(1, Ordering::AcqRel);
            self.changed.notify_all();
            let mut open = self.gate.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
            Ok(vec![0x5a; 256])
        }
    }

    #[test]
    fn stock_info_qualifier_body_is_accepted_but_other_empty_methods_stay_strict() {
        let mut qualifier_dictionary = Dictionary::new();
        qualifier_dictionary.insert(
            "qualifier".into(),
            Value::Array(vec![Value::String("txtAirPlay".into())]),
        );
        let qualifier = binary_plist(qualifier_dictionary);
        let request = make_request("GET", "/info", Some(BINARY_PLIST_CONTENT_TYPE), &qualifier);
        assert_eq!(request.body(), qualifier);

        let mut decoder = RequestDecoder::new(RequestLimits::default()).unwrap();
        decoder
            .feed(b"OPTIONS * RTSP/1.0\r\nContent-Length: 1\r\n\r\nx")
            .unwrap();
        assert!(decoder
            .next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, true)
            })
            .is_err());
    }

    #[test]
    fn options_advertises_the_owned_ap2_bootstrap_verbs() {
        let request = make_request("OPTIONS", "*", None, b"");
        let outcome = options_response(&request);
        assert_eq!(outcome.response.status, 200);
        let public = outcome
            .response
            .headers
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case("Public"))
            .map(Header::value)
            .expect("OPTIONS must include Public");
        let public = std::str::from_utf8(public).unwrap();
        for method in [
            "OPTIONS",
            "GET",
            "POST",
            "SETUP",
            "RECORD",
            "FLUSH",
            "FLUSHBUFFERED",
            "SETRATEANCHORTIME",
            "TEARDOWN",
        ] {
            assert!(
                public.split(',').any(|entry| entry.trim() == method),
                "missing AP2 method {method} from {public}"
            );
        }
        for unsupported in ["ANNOUNCE", "PAUSE", "PUT"] {
            assert!(
                !public.split(',').any(|entry| entry.trim() == unsupported),
                "must not advertise unsupported method {unsupported}"
            );
        }
    }

    #[tokio::test]
    async fn teardown_distinguishes_stream_session_and_malformed_bodies() {
        let stream_body = binary_plist({
            let mut root = Dictionary::new();
            root.insert("streams".into(), Value::Array(Vec::new()));
            root
        });
        let session_body = binary_plist(Dictionary::new());
        let requests = [
            (
                make_request(
                    "TEARDOWN",
                    "/stream",
                    Some(BINARY_PLIST_CONTENT_TYPE),
                    &stream_body,
                ),
                true,
                false,
            ),
            (
                make_request(
                    "TEARDOWN",
                    "/session",
                    Some(BINARY_PLIST_CONTENT_TYPE),
                    &session_body,
                ),
                true,
                true,
            ),
            (
                make_request(
                    "TEARDOWN",
                    "/stream",
                    Some(BINARY_PLIST_CONTENT_TYPE),
                    b"malformed",
                ),
                false,
                false,
            ),
        ];

        for (request, teardown_media, close_after) in requests {
            let mut event_keys = None;
            let mut phase_one = None;
            let outcome = dispatch_encrypted(
                &request,
                "127.0.0.1:6000".parse().unwrap(),
                "127.0.0.1:7000".parse().unwrap(),
                &config(),
                &mut event_keys,
                false,
                &mut phase_one,
                true,
                Some(7),
                Some(MediaKind::Realtime),
                &AtomicU64::new(8),
            )
            .await;
            assert_eq!(outcome.response.status, 200);
            assert_eq!(outcome.teardown_media, teardown_media);
            assert_eq!(outcome.close_after, close_after);
            assert_eq!(
                outcome.response.headers.iter().any(|header| {
                    header.name().eq_ignore_ascii_case("Connection")
                        && header.value().eq_ignore_ascii_case(b"close")
                }),
                close_after
            );
        }
    }

    #[test]
    fn media_command_failures_become_rtsp_responses_instead_of_disconnects() {
        let request = make_request("FLUSHBUFFERED", "/stream", None, b"");
        for (kind, status) in [
            (io::ErrorKind::InvalidInput, 400),
            (io::ErrorKind::InvalidData, 400),
            (io::ErrorKind::WouldBlock, 453),
            (io::ErrorKind::BrokenPipe, 455),
            (io::ErrorKind::Other, 500),
        ] {
            let response = media_command_error_response(
                &request,
                &io::Error::new(kind, "redacted test failure"),
            );
            assert_eq!(response.status, status, "unexpected mapping for {kind:?}");
        }
    }

    fn null_output_factory() -> PcmOutputFactory {
        struct NullOutput;
        impl PcmOutput for NullOutput {
            fn write(&mut self, _samples: &[i16]) {}
            fn flush(&mut self) {}
        }
        Arc::new(|_, _| Box::new(NullOutput))
    }

    #[test]
    fn pair_setup_limiter_has_an_independent_three_request_burst_per_ip() {
        let limiter = PairSetupLimiter::new();
        let first = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let second = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(limiter.allow(first));
        assert!(limiter.allow(first));
        assert!(limiter.allow(first));
        assert!(!limiter.allow(first));
        assert!(limiter.allow(second));
    }

    #[test]
    fn pairing_limits_fit_srp_3072_public_values_but_remain_small() {
        let limits = pairing_tlv_limits();
        assert!(limits.max_value_bytes >= 384);
        assert!(limits.max_message_bytes <= 4 * 1024);
        assert!(limits.max_fields <= 16);
    }

    #[tokio::test]
    async fn encrypted_info_round_trip_preserves_framing_and_cseq() {
        let secret = [0x39; 64];
        let keys = derive_control_keys(&secret).unwrap();
        let sender_write_key = *keys.inbound();
        let sender_read_key = *keys.outbound();
        let mut receiver = keys.into_codec();
        let plaintext_request = b"GET /info RTSP/1.0\r\nCSeq: 41\r\nContent-Length: 0\r\n\r\n";
        let wire_request = FrameEncoder::new(sender_write_key)
            .encode(plaintext_request)
            .unwrap();
        let decrypted = receiver.inbound.feed(&wire_request).unwrap();
        let parsed = parse_wire_request(&decrypted);

        let mut pairing = None;
        let mut event_keys = Some(derive_event_keys(&secret).unwrap());
        let mut phase_one = None;
        let next_stream_id = AtomicU64::new(1);
        let outcome = dispatch_request(
            &parsed,
            true,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            &config(),
            &PairSetupLimiter::new(),
            &mut pairing,
            &mut event_keys,
            false,
            &mut phase_one,
            false,
            None,
            None,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 200);
        let encoded_response = outcome.response.encode().unwrap();
        let wire_response = receiver.outbound.encode(&encoded_response).unwrap();
        let decrypted_response = FrameDecoder::new(sender_read_key)
            .feed(&wire_response)
            .unwrap();
        assert!(decrypted_response.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert!(decrypted_response
            .windows(b"CSeq: 41\r\n".len())
            .any(|window| window == b"CSeq: 41\r\n"));
        assert!(decrypted_response
            .windows(b"Content-Type: application/x-apple-binary-plist\r\n".len())
            .any(|window| { window == b"Content-Type: application/x-apple-binary-plist\r\n" }));
        assert!(decrypted_response
            .windows(b"bplist00".len())
            .any(|window| window == b"bplist00"));
    }

    #[tokio::test]
    async fn encrypted_info_crosses_a_real_tcp_socket() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let secret = [0x49; 64];
        let keys = derive_control_keys(&secret).unwrap();
        let sender_write_key = *keys.inbound();
        let sender_read_key = *keys.outbound();
        let server = tokio::spawn(async move {
            let (mut socket, peer) = listener.accept().await.unwrap();
            let mut state = test_state();
            state.control = Some(keys.into_codec());
            let (event_tx, _event_rx) = mpsc::channel(4);
            let (volume_tx, _volume_rx) = watch::channel(None);
            let output_factory = null_output_factory();
            let next_stream_id = AtomicU64::new(1);
            let volume_revision = AtomicU64::new(0);
            process_one_request(
                &mut socket,
                peer,
                &config(),
                &event_tx,
                &volume_tx,
                &output_factory,
                &next_stream_id,
                &volume_revision,
                &PairSetupLimiter::new(),
                &AppleChallengeExecutor::new(),
                &mut state,
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        let request = b"GET /info RTSP/1.0\r\nCSeq: 51\r\nContent-Length: 0\r\n\r\n";
        let wire = FrameEncoder::new(sender_write_key).encode(request).unwrap();
        client.write_all(&wire).await.unwrap();
        let mut decoder = FrameDecoder::new(sender_read_key);
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), async {
            loop {
                let mut wire = [0u8; 257];
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0, "server closed before a complete response");
                response.extend(decoder.feed(&wire[..read]).unwrap());
                if response_is_complete(&response) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(response.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert!(response
            .windows(b"CSeq: 51\r\n".len())
            .any(|window| window == b"CSeq: 51\r\n"));
        assert_eq!(server.await.unwrap().unwrap(), ConnectionProgress::Continue);
    }

    #[tokio::test]
    async fn successful_record_response_activates_the_initial_event_command() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let secret = [0x4b; 64];
        let control_keys = derive_control_keys(&secret).unwrap();
        let sender_write_key = *control_keys.inbound();
        let sender_read_key = *control_keys.outbound();
        let (event_commands, _commands) = event_command_channel(true);
        let observer = event_commands.clone();
        assert!(!observer.record_started.started.load(Ordering::Acquire));

        let server = tokio::spawn(async move {
            let (mut socket, peer) = listener.accept().await.unwrap();
            let mut state = test_state();
            state.control = Some(control_keys.into_codec());
            state.event_commands = Some(event_commands);
            let (event_tx, _event_rx) = mpsc::channel(4);
            let (volume_tx, _volume_rx) = watch::channel(None);
            process_one_request(
                &mut socket,
                peer,
                &config(),
                &event_tx,
                &volume_tx,
                &null_output_factory(),
                &AtomicU64::new(1),
                &AtomicU64::new(0),
                &PairSetupLimiter::new(),
                &AppleChallengeExecutor::new(),
                &mut state,
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        let request =
            b"RECORD rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 52\r\nContent-Length: 0\r\n\r\n";
        let wire = FrameEncoder::new(sender_write_key).encode(request).unwrap();
        client.write_all(&wire).await.unwrap();
        let mut decoder = FrameDecoder::new(sender_read_key);
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), async {
            loop {
                let mut wire = [0u8; 257];
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0, "server closed before a complete response");
                response.extend(decoder.feed(&wire[..read]).unwrap());
                if response_is_complete(&response) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(response.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert_eq!(server.await.unwrap().unwrap(), ConnectionProgress::Continue);
        assert!(observer.record_started.started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn event_start_signal_retains_an_unpolled_notification() {
        let signal = EventStartSignal::new();
        // Constructing `notified()` without polling it reproduces the narrow
        // RECORD race that `notify_waiters` would lose. `notify_one` must keep
        // a permit so the waiter completes even after activation has passed.
        let notification = signal.changed.notified();
        signal.activate();
        timeout(Duration::from_millis(100), notification)
            .await
            .expect("RECORD activation permit was lost");
        timeout(Duration::from_millis(100), signal.wait())
            .await
            .expect("activated state was not retained");
    }

    async fn plaintext_round_trip_with(
        request: &[u8],
        config: ServerConfig,
        apple_challenges: Arc<AppleChallengeExecutor>,
    ) -> (Vec<u8>, ConnectionProgress) {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, peer) = listener.accept().await.unwrap();
            let mut state = test_state();
            let (event_tx, _event_rx) = mpsc::channel(4);
            let (volume_tx, _volume_rx) = watch::channel(None);
            process_one_request(
                &mut socket,
                peer,
                &config,
                &event_tx,
                &volume_tx,
                &null_output_factory(),
                &AtomicU64::new(1),
                &AtomicU64::new(0),
                &PairSetupLimiter::new(),
                &apple_challenges,
                &mut state,
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), async {
            loop {
                let mut wire = [0u8; 512];
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0, "server closed before a complete response");
                response.extend_from_slice(&wire[..read]);
                if response_is_complete(&response) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        (response, server.await.unwrap().unwrap())
    }

    async fn plaintext_round_trip(request: &[u8]) -> (Vec<u8>, ConnectionProgress) {
        plaintext_round_trip_with(request, config(), Arc::new(AppleChallengeExecutor::new())).await
    }

    #[tokio::test]
    async fn valid_apple_challenge_gets_unpadded_response_and_server_header() {
        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 71\r\nApple-Challenge: AAECAwQFBgcICQoLDA0ODw\r\nContent-Length: 0\r\n\r\n";
        let (response, progress) = plaintext_round_trip(request).await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(headers.contains("Server: AirTunes/366.0\r\n"));
        let apple_response = headers
            .lines()
            .find_map(|line| line.strip_prefix("Apple-Response: "))
            .expect("valid Apple-Challenge must produce Apple-Response");
        assert_eq!(apple_response.len(), 342);
        assert!(!apple_response.contains('='));
        assert_eq!(progress, ConnectionProgress::Continue);
    }

    #[tokio::test]
    async fn no_challenge_has_no_apple_response_but_keeps_server_header() {
        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 72\r\nContent-Length: 0\r\n\r\n";
        let (response, progress) = plaintext_round_trip(request).await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.contains("Server: AirTunes/366.0\r\n"));
        assert!(!headers.contains("Apple-Response:"));
        assert_eq!(progress, ConnectionProgress::Continue);
    }

    #[tokio::test]
    async fn canonical_padded_apple_challenge_is_accepted() {
        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 72\r\nApple-Challenge: AQIDBA==\r\nContent-Length: 0\r\n\r\n";
        let (response, progress) = plaintext_round_trip(request).await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(headers.contains("Apple-Response: "));
        assert_eq!(progress, ConnectionProgress::Continue);
    }

    #[tokio::test]
    async fn malformed_oversize_and_duplicate_challenges_return_400_and_close() {
        let oversized = base64::engine::general_purpose::STANDARD_NO_PAD.encode([0x55; 17]);
        let requests = [
            "OPTIONS * RTSP/1.0\r\nCSeq: 73\r\nApple-Challenge: AQIDBA=\r\nContent-Length: 0\r\n\r\n".to_owned(),
            format!("OPTIONS * RTSP/1.0\r\nCSeq: 74\r\nApple-Challenge: {oversized}\r\nContent-Length: 0\r\n\r\n"),
            "OPTIONS * RTSP/1.0\r\nCSeq: 75\r\nApple-Challenge: AQIDBA\r\nApple-Challenge: BQYHCA\r\nContent-Length: 0\r\n\r\n".to_owned(),
        ];
        for request in requests {
            let (response, progress) = plaintext_round_trip(request.as_bytes()).await;
            let headers = String::from_utf8(response).unwrap();
            assert!(headers.starts_with("RTSP/1.0 400 Bad Request\r\n"));
            assert!(headers.contains("Server: AirTunes/366.0\r\n"));
            assert!(!headers.contains("Apple-Response:"));
            assert_eq!(progress, ConnectionProgress::Close);
        }
    }

    #[tokio::test]
    async fn repeated_plaintext_challenge_on_one_connection_returns_400_and_closes() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, peer) = listener.accept().await.unwrap();
            let mut state = test_state();
            let (event_tx, _event_rx) = mpsc::channel(4);
            let (volume_tx, _volume_rx) = watch::channel(None);
            let executor = AppleChallengeExecutor::new();
            for expected in [ConnectionProgress::Continue, ConnectionProgress::Close] {
                assert_eq!(
                    process_one_request(
                        &mut socket,
                        peer,
                        &config(),
                        &event_tx,
                        &volume_tx,
                        &null_output_factory(),
                        &AtomicU64::new(1),
                        &AtomicU64::new(0),
                        &PairSetupLimiter::new(),
                        &executor,
                        &mut state,
                    )
                    .await
                    .unwrap(),
                    expected,
                );
            }
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        for cseq in [81, 82] {
            client
                .write_all(
                    format!(
                        "OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nApple-Challenge: AQIDBA\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            loop {
                let mut wire = [0u8; 512];
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0);
                response.extend_from_slice(&wire[..read]);
                if response_is_complete(&response) {
                    break;
                }
            }
            if cseq == 81 {
                assert!(response.starts_with(b"RTSP/1.0 200 OK\r\n"));
                assert!(response
                    .windows(16)
                    .any(|window| window == b"Apple-Response: "));
            } else {
                assert!(response.starts_with(b"RTSP/1.0 400 Bad Request\r\n"));
                assert!(!response
                    .windows(16)
                    .any(|window| window == b"Apple-Response: "));
            }
        }
        server.await.unwrap();
        assert_eq!(client.read(&mut [0u8; 1]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn apple_challenge_per_ip_limit_is_independent_and_refills() {
        let executor = AppleChallengeExecutor::with_limits(1.0, 20.0, 2);
        let first = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let second = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(executor.reserve(first).is_ok());
        assert_eq!(
            executor.reserve(first).err().unwrap(),
            AppleResponseAttemptError::RateLimited,
        );
        assert!(executor.reserve(second).is_ok());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(executor.reserve(first).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_gate_rejects_without_queueing_and_releases_after_completion() {
        let executor = Arc::new(AppleChallengeExecutor::with_limits(8.0, 1.0, 1));
        let provider = Arc::new(BlockingAppleResponseProvider::new());
        let prepared = |last: u8| {
            apple_challenge::prepare(
                Some(b"AQIDBA"),
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)),
                [0; 6],
            )
            .unwrap()
            .unwrap()
        };
        let task = {
            let executor = Arc::clone(&executor);
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                executor
                    .respond(
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        provider,
                        prepared(1),
                    )
                    .await
            })
        };
        tokio::task::block_in_place(|| provider.wait_until_entered(1));

        let ordinary = timeout(
            Duration::from_millis(250),
            plaintext_round_trip_with(
                b"OPTIONS * RTSP/1.0\r\nCSeq: 88\r\nContent-Length: 0\r\n\r\n",
                config(),
                Arc::clone(&executor),
            ),
        )
        .await
        .expect("ordinary request must remain schedulable while RSA is blocked");
        assert!(ordinary.0.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert_eq!(ordinary.1, ConnectionProgress::Continue);

        let timer_fired = Arc::new(AtomicBool::new(false));
        let timer = {
            let timer_fired = Arc::clone(&timer_fired);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                timer_fired.store(true, Ordering::Release);
            })
        };
        assert_eq!(
            executor
                .respond(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
                    Arc::clone(&provider)
                        as Arc<dyn super::super::apple_challenge::AppleResponseProvider>,
                    prepared(2),
                )
                .await
                .unwrap_err(),
            AppleResponseAttemptError::Busy,
        );
        timeout(Duration::from_millis(250), timer)
            .await
            .unwrap()
            .unwrap();
        assert!(timer_fired.load(Ordering::Acquire));

        provider.release();
        assert!(task.await.unwrap().is_ok());
        assert!(executor
            .respond(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)),
                provider,
                prepared(3),
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn provider_failure_returns_500_without_apple_response_and_closes() {
        let mut failing = config();
        failing.apple_response = Arc::new(FailingAppleResponseProvider);
        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 91\r\nApple-Challenge: AQIDBA\r\nContent-Length: 0\r\n\r\n";
        let (response, progress) =
            plaintext_round_trip_with(request, failing, Arc::new(AppleChallengeExecutor::new()))
                .await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.starts_with("RTSP/1.0 500 Internal Server Error\r\n"));
        assert!(!headers.contains("Apple-Response:"));
        assert_eq!(progress, ConnectionProgress::Close);
    }

    #[tokio::test]
    async fn per_ip_and_global_rejections_use_bounded_statuses_and_close() {
        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 92\r\nApple-Challenge: AQIDBA\r\nContent-Length: 0\r\n\r\n";

        let per_ip = Arc::new(AppleChallengeExecutor::with_limits(0.0, 0.0, 1));
        let (response, progress) = plaintext_round_trip_with(request, config(), per_ip).await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.starts_with("RTSP/1.0 429 Too Many Requests\r\n"));
        assert!(headers.contains("Retry-After: 1\r\n"));
        assert!(!headers.contains("Apple-Response:"));
        assert_eq!(progress, ConnectionProgress::Close);

        let busy = Arc::new(AppleChallengeExecutor::with_limits(1.0, 0.0, 0));
        let (response, progress) = plaintext_round_trip_with(request, config(), busy).await;
        let headers = String::from_utf8(response).unwrap();
        assert!(headers.starts_with("RTSP/1.0 503 Service Unavailable\r\n"));
        assert!(!headers.contains("Apple-Response:"));
        assert_eq!(progress, ConnectionProgress::Close);
    }

    fn response_is_complete(response: &[u8]) -> bool {
        let Some(header_start_end) = response.windows(4).position(|window| window == b"\r\n\r\n")
        else {
            return false;
        };
        let header_end = header_start_end + 4;
        let headers = String::from_utf8_lossy(&response[..header_end]);
        let Some(content_length) = headers.lines().find_map(|line| {
            line.strip_prefix("Content-Length: ")
                .and_then(|value| value.parse::<usize>().ok())
        }) else {
            return false;
        };
        response.len() >= header_end + content_length
    }

    #[test]
    fn m4_boundary_decrypts_a_pipelined_first_control_request() {
        let secret = [0x59; 64];
        let keys = derive_control_keys(&secret).unwrap();
        let sender_write_key = *keys.inbound();
        let first_request = b"GET /info RTSP/1.0\r\nCSeq: 61\r\nContent-Length: 0\r\n\r\n";
        let trailing = FrameEncoder::new(sender_write_key)
            .encode(first_request)
            .unwrap();
        let mut state = test_state();
        state.requests.feed(&trailing).unwrap();

        install_control_after_m4(&mut state, keys).unwrap();
        let parsed = state
            .requests
            .next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, true)
            })
            .unwrap()
            .unwrap();
        assert_eq!(parsed.method(), "GET");
        assert_eq!(parsed.target(), "/info");
        assert_eq!(parsed.header("CSeq"), Some(b"61".as_slice()));
    }

    #[tokio::test]
    async fn fairplay_both_phases_are_bounded_and_reported_after_dispatch() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut phase_one = [0u8; 16];
        phase_one[..4].copy_from_slice(b"FPLY");
        phase_one[4] = 3;
        phase_one[5] = 1;
        phase_one[6] = 1;
        phase_one[14] = 2;
        let request = make_request(
            "POST",
            "/fp-setup",
            Some(OCTET_STREAM_CONTENT_TYPE),
            &phase_one,
        );
        let outcome = fairplay_response(&request, peer, &config());
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.body.len(), 142);
        assert!(matches!(
            outcome.probe_event,
            Some(ProbeEvent::FairPlayComplete { phase: 1, .. })
        ));

        let mut phase_two = [0u8; 164];
        phase_two[..4].copy_from_slice(b"FPLY");
        phase_two[4] = 3;
        phase_two[5] = 1;
        phase_two[6] = 3;
        phase_two[144..].copy_from_slice(&core::array::from_fn::<_, 20, _>(|index| index as u8));
        let request = make_request(
            "POST",
            "/fp-setup",
            Some(OCTET_STREAM_CONTENT_TYPE),
            &phase_two,
        );
        let outcome = fairplay_response(&request, peer, &config());
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.body.len(), 32);
        assert_eq!(&outcome.response.body[12..], &phase_two[144..]);
        assert!(matches!(
            outcome.probe_event,
            Some(ProbeEvent::FairPlayComplete { phase: 3, .. })
        ));
    }

    #[tokio::test]
    async fn setup_phase_one_returns_a_live_encrypted_event_port() {
        let sender_timing = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut dictionary = Dictionary::new();
        dictionary.insert("timingProtocol".into(), Value::String("NTP".into()));
        dictionary.insert(
            "timingPort".into(),
            Value::Integer(u64::from(sender_timing.local_addr().unwrap().port()).into()),
        );
        dictionary.insert("isRemoteControlOnly".into(), Value::Boolean(false));
        dictionary.insert("et".into(), Value::Integer(32u64.into()));
        dictionary.insert("ekey".into(), Value::Data(vec![0x11; 72]));
        dictionary.insert("eiv".into(), Value::Data(vec![0x22; 16]));
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(dictionary),
        );
        let secret = [0x5a; 64];
        let keys = derive_event_keys(&secret).unwrap();
        let sender_write_key = *keys.inbound();
        let mut event_keys = Some(keys);
        let mut phase_one_state = None;
        let next_stream_id = AtomicU64::new(1);
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one_state,
            false,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 200);
        assert!(event_keys.is_none());
        assert!(matches!(
            outcome.probe_event,
            Some(ProbeEvent::SetupPhase1 {
                fields: SetupPhase1Fields {
                    timing_protocol: Some(ref protocol),
                    remote_control_only: false,
                    has_timing_port: true,
                    ..
                },
                ..
            }) if protocol == "NTP"
        ));
        let response = Value::from_reader(Cursor::new(&outcome.response.body)).unwrap();
        let response = response.as_dictionary().unwrap();
        let event_port = response
            .get("eventPort")
            .and_then(Value::as_unsigned_integer)
            .unwrap() as u16;
        assert_ne!(event_port, 0);
        let timing_port = response
            .get("timingPort")
            .and_then(Value::as_unsigned_integer)
            .unwrap() as u16;
        assert_ne!(timing_port, 0);
        assert_eq!(
            outcome
                .install_phase_one
                .as_ref()
                .and_then(|phase_one| match phase_one {
                    PhaseOneState::Ntp(timing) => Some(timing.port()),
                    PhaseOneState::Ptp(_) => None,
                }),
            Some(timing_port)
        );
        let mut timing_request = [0u8; 33];
        let (timing_length, timing_source) = timeout(
            Duration::from_secs(1),
            sender_timing.recv_from(&mut timing_request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(timing_length, 32);
        assert_eq!(&timing_request[..2], [0x80, 0xd2]);
        assert_eq!(timing_source.port(), timing_port);

        let mut endpoint = outcome
            .install_event_endpoint
            .expect("successful setup owns the bound listener");
        // This test exercises the encrypted endpoint/key handoff rather than
        // RECORD sequencing, which is covered by the event-command test.
        endpoint.initial_update_info = None;
        let server = tokio::spawn(run_event_endpoint(endpoint));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, event_port))
            .await
            .unwrap();
        let wire = FrameEncoder::new(sender_write_key)
            .encode(b"authenticated event probe")
            .unwrap();
        client.write_all(&wire).await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
    }

    #[tokio::test]
    async fn event_channel_posts_initial_update_info_and_requires_success_response() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let secret = [0x6b; 64];
        let keys = derive_event_keys(&secret).unwrap();
        let sender_write_key = *keys.inbound();
        let sender_read_key = *keys.outbound();
        let (sender, commands) = event_command_channel(true);
        let volume_sender = sender.clone();
        let lifecycle_sender = sender.clone();
        assert!(!lifecycle_sender.is_active());
        let error = lifecycle_sender.send_device_volume(0.5, false).unwrap_err();
        assert_eq!(error.io_kind(), io::ErrorKind::NotConnected);
        assert!(!error.may_have_written());
        let endpoint = PreparedEventEndpoint {
            listener,
            expected_peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            keys,
            initial_update_info: Some(config().event_update_info_body().unwrap()),
            sender,
            commands,
        };
        let server = tokio::spawn(run_event_endpoint(endpoint));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new(sender_read_key);
        let mut plaintext = Vec::new();
        let mut wire = [0u8; READ_CHUNK_BYTES];
        assert!(timeout(Duration::from_millis(30), client.read(&mut wire))
            .await
            .is_err());
        assert!(!lifecycle_sender.is_active());
        lifecycle_sender.activate_after_record();
        lifecycle_sender.activate_after_record();
        let body = timeout(Duration::from_secs(1), async {
            loop {
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0);
                plaintext.extend_from_slice(&decoder.feed(&wire[..read]).unwrap());
                let Some(headers_end) = find_header_end(&plaintext) else {
                    continue;
                };
                let headers = std::str::from_utf8(&plaintext[..headers_end]).unwrap();
                assert!(headers.starts_with("POST /command RTSP/1.0\r\n"));
                assert!(headers.contains("CSeq: 0\r\n"));
                assert!(headers.contains("Content-Type: application/x-apple-binary-plist\r\n"));
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if plaintext.len() >= headers_end + content_length {
                    break plaintext[headers_end..headers_end + content_length].to_vec();
                }
            }
        })
        .await
        .unwrap();
        let command = Value::from_reader(Cursor::new(body)).unwrap();
        let command = command.as_dictionary().unwrap();
        assert_eq!(
            command.get("type").and_then(Value::as_string),
            Some("updateInfo")
        );
        let value = command.get("value").and_then(Value::as_dictionary).unwrap();
        assert_eq!(
            value.get("initialVolume").and_then(Value::as_real),
            Some(-18.0)
        );
        assert!(!lifecycle_sender.is_active());

        let mut response_encoder = FrameEncoder::new(sender_write_key);
        // AirPlayXPCHelper sends this exact empty response without a
        // Content-Length header for the initial updateInfo command.
        let response = response_encoder
            .encode(b"RTSP/1.0 200 OK\r\nCSeq: 0\r\n\r\n")
            .unwrap();
        client.write_all(&response).await.unwrap();
        timeout(Duration::from_secs(1), async {
            while !lifecycle_sender.is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        plaintext.clear();
        let volume_task =
            tokio::task::spawn_blocking(move || volume_sender.send_device_volume(0.05, false));
        let body = timeout(Duration::from_secs(1), async {
            loop {
                let read = client.read(&mut wire).await.unwrap();
                assert_ne!(read, 0);
                plaintext.extend_from_slice(&decoder.feed(&wire[..read]).unwrap());
                let Some(headers_end) = find_header_end(&plaintext) else {
                    continue;
                };
                let headers = std::str::from_utf8(&plaintext[..headers_end]).unwrap();
                assert!(headers.starts_with("POST /command RTSP/1.0\r\n"));
                assert!(headers.contains("CSeq: 1\r\n"));
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if plaintext.len() >= headers_end + content_length {
                    break plaintext[headers_end..headers_end + content_length].to_vec();
                }
            }
        })
        .await
        .unwrap();
        let command = Value::from_reader(Cursor::new(body)).unwrap();
        let command = command.as_dictionary().unwrap();
        assert_eq!(
            command.get("type").and_then(Value::as_string),
            Some("sendMediaRemoteCommand")
        );
        assert_eq!(
            command.get("value").and_then(Value::as_string),
            Some("dvlc")
        );
        assert_eq!(
            command.get("volume").and_then(Value::as_real),
            Some(f64::from(0.05_f32))
        );
        assert_eq!(
            command.get("isMuted").and_then(Value::as_boolean),
            Some(false)
        );
        let response = response_encoder
            .encode(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        client.write_all(&response).await.unwrap();
        assert!(volume_task.await.unwrap().is_ok());
        client.shutdown().await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        assert!(!lifecycle_sender.is_active());
    }

    #[test]
    fn event_response_accepts_apple_empty_response_and_validates_framing() {
        let valid = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(validate_event_response(valid, 7), Ok(0));
        let apple_empty = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n";
        assert_eq!(validate_event_response(apple_empty, 7), Ok(0));
        assert_eq!(event_response_is_complete(apple_empty, 7), Ok(true));
        let modern_apple_empty =
            b"RTSP/1.0 200 OK\r\nContent-Length: 0\r\nX-Transport-Header: opaque\r\n\r\n";
        assert_eq!(validate_event_response(modern_apple_empty, 7), Ok(0));
        assert_eq!(event_response_is_complete(modern_apple_empty, 7), Ok(true));
        let ambiguous_empty = b"RTSP/1.0 200 OK\r\nServer: AirTunes/550.10\r\n\r\n";
        assert_eq!(
            validate_event_response(ambiguous_empty, 7),
            Err(ConnectionError::Framing)
        );
        let missing_cseq_with_body = b"RTSP/1.0 200 OK\r\nContent-Length: 1\r\n\r\n";
        assert_eq!(
            validate_event_response(missing_cseq_with_body, 7),
            Err(ConnectionError::Framing)
        );

        for invalid in [
            b"RTSP/1.0 200 OK\r\nCSeq: 8\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: nope\r\n\r\n".as_slice(),
            b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"
                .as_slice(),
        ] {
            assert_eq!(
                validate_event_response(invalid, 7),
                Err(ConnectionError::Framing)
            );
        }
    }

    #[test]
    fn event_response_waits_for_bounded_body_and_rejects_surplus() {
        let headers = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: 4\r\n\r\n";
        assert_eq!(event_response_is_complete(headers, 7), Ok(false));

        let mut complete = headers.to_vec();
        complete.extend_from_slice(b"pong");
        assert_eq!(event_response_is_complete(&complete, 7), Ok(true));

        complete.push(b'x');
        assert_eq!(
            event_response_is_complete(&complete, 7),
            Err(ConnectionError::Framing)
        );

        let mut apple_empty_with_surplus = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n".to_vec();
        apple_empty_with_surplus.push(b'x');
        assert_eq!(
            event_response_is_complete(&apple_empty_with_surplus, 7),
            Err(ConnectionError::Framing)
        );

        let oversized = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: {}\r\n\r\n",
            MAX_EVENT_RESPONSE_BYTES
        );
        assert_eq!(
            event_response_is_complete(oversized.as_bytes(), 7),
            Err(ConnectionError::Framing)
        );
    }

    #[tokio::test]
    async fn event_command_timeout_covers_socket_write() {
        let (mut socket, _blocked_peer) = tokio::io::duplex(1);
        let mut channel = derive_event_keys(&[0x45; 64]).unwrap().into_codec();
        let body = vec![0x5a; READ_CHUNK_BYTES];
        let error = send_event_command_with_timeout(
            &mut socket,
            &mut channel,
            1,
            &body,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(error.io_kind(), io::ErrorKind::TimedOut);
        assert!(error.may_have_written());
    }

    #[tokio::test]
    async fn aborting_event_actor_revokes_ready_sender_clones() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, commands) = event_command_channel(true);
        let observer = sender.clone();
        let endpoint = PreparedEventEndpoint {
            listener,
            expected_peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            keys: derive_event_keys(&[0x46; 64]).unwrap(),
            initial_update_info: None,
            sender,
            commands,
        };
        let actor = spawn_event_endpoint(endpoint);
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !observer.is_active() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        actor.abort();
        assert!(actor.await.unwrap_err().is_cancelled());
        assert!(!observer.is_active());
    }

    #[tokio::test]
    async fn aborting_event_actor_while_waiting_for_record_keeps_capability_inactive() {
        let listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, commands) = event_command_channel(true);
        let observer = sender.clone();
        let endpoint = PreparedEventEndpoint {
            listener,
            expected_peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            keys: derive_event_keys(&[0x47; 64]).unwrap(),
            initial_update_info: Some(config().event_update_info_body().unwrap()),
            sender,
            commands,
        };
        let actor = spawn_event_endpoint(endpoint);
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut wire = [0u8; 1];
        assert!(timeout(Duration::from_millis(30), client.read(&mut wire))
            .await
            .is_err());
        assert!(!observer.is_active());

        actor.abort();
        assert!(actor.await.unwrap_err().is_cancelled());
        assert!(!observer.is_active());
        assert_eq!(
            observer
                .send_device_volume(0.5, false)
                .unwrap_err()
                .io_kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn event_capability_is_publishable_before_handshake_is_ready() {
        let (supported, _commands) = event_command_channel(true);
        assert!(!supported.is_active());
        let published = publishable_event_commands(Some(&supported))
            .expect("supported event-volume capability is published while connecting");
        assert!(published.same_channel(&supported));
        assert!(!published.is_active());

        let (unsupported, _commands) = event_command_channel(false);
        assert!(publishable_event_commands(Some(&unsupported)).is_none());
    }

    #[test]
    fn event_attempt_cancellation_reports_possible_writes_conservatively() {
        let pending = EventCommandAttempt::new();
        assert!(!pending.cancel());
        assert!(!pending.begin_write());

        let writing = EventCommandAttempt::new();
        assert!(writing.begin_write());
        assert!(writing.cancel());
    }

    #[test]
    fn source_version_gate_matches_ap2_event_volume_compatibility_boundary() {
        for unsupported in [
            "",
            "354",
            "354.54",
            "354.54.5",
            "354.54.5.1",
            "354.100",
            "550.10.100",
            "550.100.1",
            "x.1",
        ] {
            assert!(!source_version_supports_event_volume(unsupported));
        }
        for supported in ["354.54.6", "354.55", "355", "550.10", "920.10.1"] {
            assert!(source_version_supports_event_volume(supported));
        }
    }

    #[tokio::test]
    async fn ptp_phase_one_returns_its_bound_event_endpoint_and_local_peer_identity() {
        let local: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let mut sender_peer = Dictionary::new();
        sender_peer.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String(peer.ip().to_string())]),
        );
        sender_peer.insert("ID".into(), Value::String("sender-clock".into()));

        let mut dictionary = Dictionary::new();
        dictionary.insert("timingProtocol".into(), Value::String("PTP".into()));
        dictionary.insert("isRemoteControlOnly".into(), Value::Boolean(false));
        dictionary.insert(
            "timingPeerInfo".into(),
            Value::Dictionary(sender_peer.clone()),
        );
        dictionary.insert(
            "timingPeerList".into(),
            Value::Array(vec![Value::Dictionary(sender_peer)]),
        );
        dictionary.insert("et".into(), Value::Integer(0u64.into()));
        dictionary.insert("ekey".into(), Value::Data(vec![0x31; 16]));
        dictionary.insert("eiv".into(), Value::Data(vec![0x32; 16]));
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(dictionary),
        );
        let mut event_keys = Some(derive_event_keys(&[0x91; 64]).unwrap());
        let mut phase_one = None;
        let (_ptp_observer, ptp_clock) = PtpObserver::bind_ephemeral().await.unwrap();
        let outcome = setup_response(
            &request,
            peer,
            local,
            "receiver-clock",
            Some(&ptp_clock),
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one,
            false,
            &AtomicU64::new(1),
        )
        .await;

        assert_eq!(outcome.response.status, 200);
        assert!(event_keys.is_none());
        assert!(matches!(
            outcome.install_phase_one,
            Some(PhaseOneState::Ptp(_))
        ));
        assert!(outcome.install_event_endpoint.is_some());
        assert!(matches!(
            outcome.probe_event,
            Some(ProbeEvent::SetupPhase1 {
                fields: SetupPhase1Fields {
                    timing_protocol: Some(ref protocol),
                    remote_control_only: false,
                    has_timing_port: false,
                    ..
                },
                ..
            }) if protocol == "PTP"
        ));

        let response = Value::from_reader(Cursor::new(&outcome.response.body)).unwrap();
        let response = response.as_dictionary().unwrap();
        assert_ne!(
            response
                .get("eventPort")
                .and_then(Value::as_unsigned_integer),
            Some(0)
        );
        assert_eq!(
            response
                .get("timingPort")
                .and_then(Value::as_unsigned_integer),
            Some(0)
        );
        let peer_info = response
            .get("timingPeerInfo")
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            peer_info.get("ID").and_then(Value::as_string),
            Some("receiver-clock")
        );
        assert_eq!(
            peer_info
                .get("DeviceType")
                .and_then(Value::as_signed_integer),
            Some(0)
        );
        assert_eq!(
            peer_info
                .get("Addresses")
                .and_then(Value::as_array)
                .and_then(|addresses| addresses.first())
                .and_then(Value::as_string),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn ptp_phase_one_encodes_the_platform_clock_port_identity_when_available() {
        let body = setup_ptp_phase_one_body(
            49_001,
            "192.0.2.20".parse().unwrap(),
            "receiver-clock",
            "sender-clock",
            Some((0xd273_7ec2_e765_0008, 0x800c)),
        )
        .unwrap();
        let response = Value::from_reader(Cursor::new(body)).unwrap();
        let peer_info = response
            .as_dictionary()
            .and_then(|root| root.get("timingPeerInfo"))
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            peer_info.get("ClockID").and_then(Value::as_signed_integer),
            Some(0xd273_7ec2_e765_0008u64 as i64)
        );
        assert_eq!(
            peer_info
                .get("SupportsClockPortMatchingOverride")
                .and_then(Value::as_boolean),
            Some(true)
        );
        assert_eq!(
            peer_info
                .get("ClockPorts")
                .and_then(Value::as_dictionary)
                .and_then(|ports| ports.get("sender-clock"))
                .and_then(Value::as_signed_integer),
            Some(0x800c)
        );
    }

    #[test]
    fn ptp_peer_matching_accepts_signed_wire_identities_and_requires_a_complete_tuple() {
        let mut clock_ports = Dictionary::new();
        clock_ports.insert("receiver-clock".into(), Value::Integer((-32_728i64).into()));
        let mut peer = Dictionary::new();
        peer.insert("ID".into(), Value::String("sender-clock".into()));
        peer.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String("192.0.2.10".into())]),
        );
        peer.insert(
            "ClockID".into(),
            Value::Integer((0xd273_7ec2_e765_0008u64 as i64).into()),
        );
        peer.insert(
            "SupportsClockPortMatchingOverride".into(),
            Value::Boolean(true),
        );
        peer.insert("ClockPorts".into(), Value::Dictionary(clock_ports));

        let parsed = parse_timing_peer_info(Some(&Value::Dictionary(peer.clone()))).unwrap();
        assert_eq!(
            parsed.match_for("receiver-clock"),
            Ok(Some(PtpRemoteMatch {
                clock_identity: 0xd273_7ec2_e765_0008,
                port_number: 32_808,
            }))
        );
        assert!(parsed.match_for("other-receiver").is_err());

        peer.remove("ClockPorts");
        let parsed = parse_timing_peer_info(Some(&Value::Dictionary(peer))).unwrap();
        assert_eq!(parsed.match_for("receiver-clock"), Ok(None));

        let mut incomplete_ports = Dictionary::new();
        incomplete_ports.insert("receiver-clock".into(), Value::Integer(0x800cu64.into()));
        let mut incomplete = Dictionary::new();
        incomplete.insert("ID".into(), Value::String("sender-clock".into()));
        incomplete.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String("192.0.2.10".into())]),
        );
        incomplete.insert("ClockPorts".into(), Value::Dictionary(incomplete_ports));
        assert!(parse_timing_peer_info(Some(&Value::Dictionary(incomplete))).is_err());
    }

    #[test]
    fn ptp_peer_without_override_uses_platform_default_receive_matching() {
        let mut peer = Dictionary::new();
        peer.insert("ID".into(), Value::String("sender-clock".into()));
        peer.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String("192.0.2.10".into())]),
        );
        peer.insert(
            "ClockID".into(),
            Value::Integer(0x5273_7ec2_e765_0008u64.into()),
        );
        peer.insert(
            "SupportsClockPortMatchingOverride".into(),
            Value::Boolean(false),
        );

        let parsed = parse_timing_peer_info(Some(&Value::Dictionary(peer))).unwrap();
        assert_eq!(parsed.match_for("receiver-clock"), Ok(None));
    }

    #[test]
    fn ptp_peer_uses_a_complete_tuple_independently_of_the_advisory_capability_flag() {
        for supports in [None, Some(false), Some(true)] {
            let mut ports = Dictionary::new();
            ports.insert("receiver-clock".into(), Value::Integer(0x800cu64.into()));
            let mut peer = Dictionary::new();
            peer.insert("ID".into(), Value::String("sender-clock".into()));
            peer.insert(
                "Addresses".into(),
                Value::Array(vec![Value::String("192.0.2.10".into())]),
            );
            peer.insert(
                "ClockID".into(),
                Value::Integer(0x5273_7ec2_e765_0008u64.into()),
            );
            peer.insert("ClockPorts".into(), Value::Dictionary(ports));
            if let Some(supports) = supports {
                peer.insert(
                    "SupportsClockPortMatchingOverride".into(),
                    Value::Boolean(supports),
                );
            }
            let parsed = parse_timing_peer_info(Some(&Value::Dictionary(peer))).unwrap();
            assert!(parsed.match_for("receiver-clock").unwrap().is_some());
        }

        let mut peer = Dictionary::new();
        peer.insert("ID".into(), Value::String("sender-clock".into()));
        peer.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String("192.0.2.10".into())]),
        );
        peer.insert(
            "SupportsClockPortMatchingOverride".into(),
            Value::Boolean(true),
        );
        let parsed = parse_timing_peer_info(Some(&Value::Dictionary(peer))).unwrap();
        assert_eq!(parsed.match_for("receiver-clock"), Ok(None));
    }

    #[tokio::test]
    async fn stream_setup_requires_phase_one_and_allocates_nothing_without_it() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(96u64.into()));
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(dictionary),
        );
        let mut event_keys = Some(derive_event_keys(&[0x71; 64]).unwrap());
        let mut phase_one_state = None;
        let next_stream_id = AtomicU64::new(1);
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one_state,
            false,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 455);
        assert!(outcome.response.body.is_empty());
        assert!(outcome.install_event_endpoint.is_none());
        assert!(event_keys.is_some());
        assert!(outcome.probe_event.is_none());
    }

    #[tokio::test]
    async fn stream_setup_prepares_live_type96_ports_before_success() {
        let timing_peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender_control = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let timing = PreparedTiming::bind(local, peer, timing_peer.local_addr().unwrap().port())
            .await
            .unwrap();
        let timing_port = timing.port();
        let mut phase_one = Some(PhaseOneState::Ntp(timing));

        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(96u64.into()));
        stream.insert("audioFormat".into(), Value::Integer(0x40000u64.into()));
        stream.insert("ct".into(), Value::Integer(2u64.into()));
        stream.insert("spf".into(), Value::Integer(352u64.into()));
        stream.insert("shk".into(), Value::Data(vec![0x39; 32]));
        stream.insert(
            "controlPort".into(),
            Value::Integer(u64::from(sender_control.local_addr().unwrap().port()).into()),
        );
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(root),
        );
        let mut event_keys = None;
        let outcome = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            true,
            &mut phase_one,
            false,
            &AtomicU64::new(41),
        )
        .await;
        assert_eq!(outcome.response.status, 200);
        assert!(phase_one.is_none());
        let prepared = outcome.start_media.expect("successful setup owns media");
        assert_eq!(prepared.stream_id(), 41);
        let PreparedMedia::Type96 { media, .. } = prepared else {
            panic!("type-96 SETUP must prepare type-96 media");
        };
        let ports = media.ports();
        assert_ne!(ports.data_port, 0);
        assert_eq!(ports.control_port, timing_port);

        let response = Value::from_reader(Cursor::new(&outcome.response.body)).unwrap();
        let response_stream = response
            .as_dictionary()
            .unwrap()
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.first())
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            response_stream
                .get("type")
                .and_then(Value::as_unsigned_integer),
            Some(96)
        );
        assert_eq!(
            response_stream
                .get("dataPort")
                .and_then(Value::as_unsigned_integer),
            Some(u64::from(ports.data_port))
        );
        assert_eq!(
            response_stream
                .get("controlPort")
                .and_then(Value::as_unsigned_integer),
            Some(u64::from(ports.control_port))
        );
        assert!(!response_stream.contains_key("streamID"));
    }

    #[tokio::test]
    async fn stream_setup_prepares_live_type103_ports_and_buffer_capacity() {
        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let (_ptp_observer, ptp_clock) = PtpObserver::bind_ephemeral().await.unwrap();
        let mut phase_one = Some(PhaseOneState::Ptp(ptp_clock));

        let mut stream = Dictionary::new();
        stream.insert(
            "type".into(),
            Value::Integer(STREAM_TYPE_BUFFERED_AUDIO.into()),
        );
        stream.insert("audioFormat".into(), Value::Integer(0x0040_0000u64.into()));
        stream.insert(
            "ct".into(),
            Value::Integer(buffered::COMPRESSION_TYPE_AAC_LC.into()),
        );
        stream.insert("spf".into(), Value::Integer(1_024u64.into()));
        stream.insert("sr".into(), Value::Integer(44_100u64.into()));
        stream.insert("shk".into(), Value::Data(vec![0x49; 32]));
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(root),
        );
        let mut event_keys = None;
        let outcome = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            true,
            &mut phase_one,
            false,
            &AtomicU64::new(51),
        )
        .await;
        assert_eq!(outcome.response.status, 200);
        assert!(matches!(phase_one, Some(PhaseOneState::Ptp(_))));
        let prepared = outcome.start_media.expect("successful setup owns media");
        assert_eq!(prepared.stream_id(), 51);
        let ports = match &prepared {
            PreparedMedia::Type103 { media, .. } => media.ports(),
            PreparedMedia::Type96 { .. } => {
                panic!("type-103 SETUP must prepare type-103 media")
            }
        };
        assert_ne!(ports.data_port, 0);
        assert_ne!(ports.control_port, 0);
        assert_eq!(ports.audio_buffer_size, 8 * 1024 * 1024);

        let active = prepared.start(null_output_factory()(51, peer));
        assert_eq!(active.kind(), MediaKind::Buffered);
        active
            .set_rate(BufferedRateAnchor {
                playing: true,
                rtp_time: Some(0),
                network_time_secs: Some(2_208_988_800),
                network_time_frac: Some(0),
                timeline_id: Some(1),
            })
            .await
            .unwrap();
        active
            .flush(FlushRequest::Buffered {
                request: BufferedFlush::Immediate {
                    until: WirePoint {
                        sequence: 17,
                        timestamp: 0,
                    },
                },
            })
            .await
            .unwrap();
        active.flush(FlushRequest::CurrentStream).await.unwrap();

        let connection = timeout(
            Duration::from_secs(1),
            TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port)),
        )
        .await
        .expect("buffered data port must already be listening")
        .expect("buffered data port must accept TCP");

        let response = Value::from_reader(Cursor::new(&outcome.response.body)).unwrap();
        let response_stream = response
            .as_dictionary()
            .unwrap()
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.first())
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            response_stream
                .get("type")
                .and_then(Value::as_unsigned_integer),
            Some(STREAM_TYPE_BUFFERED_AUDIO)
        );
        assert_eq!(
            response_stream
                .get("dataPort")
                .and_then(Value::as_unsigned_integer),
            Some(u64::from(ports.data_port))
        );
        assert_eq!(
            response_stream
                .get("controlPort")
                .and_then(Value::as_unsigned_integer),
            Some(u64::from(ports.control_port))
        );
        assert_eq!(
            response_stream
                .get("audioBufferSize")
                .and_then(Value::as_unsigned_integer),
            Some(ports.audio_buffer_size)
        );
        active.abort().await;
        drop(connection);
    }

    #[tokio::test]
    async fn media_gate_allows_same_connection_replacement_and_rejects_competitors() {
        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let permits = media_permits();
        let request = make_request(
            "SETUP",
            "/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist({
                let mut stream = Dictionary::new();
                stream.insert(
                    "type".into(),
                    Value::Integer(STREAM_TYPE_BUFFERED_AUDIO.into()),
                );
                stream.insert("audioFormat".into(), Value::Integer(0x0040_0000u64.into()));
                stream.insert(
                    "ct".into(),
                    Value::Integer(buffered::COMPRESSION_TYPE_AAC_LC.into()),
                );
                stream.insert("spf".into(), Value::Integer(1_024u64.into()));
                stream.insert("sr".into(), Value::Integer(44_100u64.into()));
                stream.insert("shk".into(), Value::Data(vec![0x59; 32]));
                let mut root = Dictionary::new();
                root.insert(
                    "streams".into(),
                    Value::Array(vec![Value::Dictionary(stream)]),
                );
                root
            }),
        );

        let (_observer, clock) = PtpObserver::bind_ephemeral().await.unwrap();
        let mut first_phase = Some(PhaseOneState::Ptp(clock));
        let mut event_keys = None;
        let mut first_outcome = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &permits,
            &mut event_keys,
            true,
            &mut first_phase,
            false,
            &AtomicU64::new(1),
        )
        .await;
        assert_eq!(first_outcome.response.status, 200);
        let first_permit = first_outcome
            .install_media_permit
            .take()
            .expect("first stream acquires the connection permit");
        let first = first_outcome
            .start_media
            .take()
            .unwrap()
            .start(null_output_factory()(1, peer));
        assert_eq!(permits.available_permits(), 0);

        // PTP timing and the connection-owned permit survive a stream-only
        // TEARDOWN, so the sender can immediately prepare a replacement media
        // stream on this same RTSP connection.
        assert!(matches!(first_phase, Some(PhaseOneState::Ptp(_))));
        let mut repeated = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &permits,
            &mut event_keys,
            true,
            &mut first_phase,
            true,
            &AtomicU64::new(2),
        )
        .await;
        assert_eq!(repeated.response.status, 200);
        assert!(repeated.install_media_permit.is_none());
        assert!(repeated.start_media.take().is_some());
        assert!(matches!(first_phase, Some(PhaseOneState::Ptp(_))));
        assert!(!first.is_finished());
        assert_eq!(permits.available_permits(), 0);

        // A different connection still cannot exceed the global media limit.
        let (_observer, clock) = PtpObserver::bind_ephemeral().await.unwrap();
        let mut second_phase = Some(PhaseOneState::Ptp(clock));
        let second = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &permits,
            &mut event_keys,
            true,
            &mut second_phase,
            false,
            &AtomicU64::new(2),
        )
        .await;
        assert_eq!(second.response.status, 453);
        assert!(second.start_media.is_none());
        assert!(matches!(second_phase, Some(PhaseOneState::Ptp(_))));

        first.abort().await;
        drop(first);
        assert_eq!(permits.available_permits(), 0);
        drop(first_permit);
        assert_eq!(permits.available_permits(), 1);

        let retry = setup_response(
            &request,
            peer,
            local,
            "receiver-peer",
            None,
            &permits,
            &mut event_keys,
            true,
            &mut second_phase,
            false,
            &AtomicU64::new(3),
        )
        .await;
        assert_eq!(retry.response.status, 200);
        assert!(retry.install_media_permit.is_some());
        drop(retry);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn ptp_phase_two_prepares_realtime_udp_ports_without_an_ntp_timing_port() {
        let mut stream = Dictionary::new();
        stream.insert(
            "type".into(),
            Value::Integer(STREAM_TYPE_REALTIME_AUDIO.into()),
        );
        stream.insert("audioFormat".into(), Value::Integer(0x0004_0000u64.into()));
        stream.insert("ct".into(), Value::Integer(2u64.into()));
        stream.insert("spf".into(), Value::Integer(352u64.into()));
        stream.insert("sr".into(), Value::Integer(44_100u64.into()));
        stream.insert("shk".into(), Value::Data(vec![0x59; 32]));
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let request = make_request(
            "SETUP",
            "rtsp://127.0.0.1/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(root),
        );
        let (_ptp_observer, ptp_clock) = PtpObserver::bind_ephemeral().await.unwrap();
        let mut phase_one = Some(PhaseOneState::Ptp(ptp_clock));
        let mut event_keys = None;
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            true,
            &mut phase_one,
            false,
            &AtomicU64::new(1),
        )
        .await;

        assert_eq!(outcome.response.status, 200);
        assert!(matches!(phase_one, Some(PhaseOneState::Ptp(_))));
        let prepared = outcome
            .start_media
            .expect("PTP realtime SETUP owns both UDP endpoints");
        let PreparedMedia::Type96 { media, .. } = prepared else {
            panic!("PTP realtime SETUP must prepare type-96 media");
        };
        let ports = media.ports();
        assert_ne!(ports.data_port, 0);
        assert_ne!(ports.control_port, 0);
        assert_ne!(ports.data_port, ports.control_port);

        let response = Value::from_reader(Cursor::new(&outcome.response.body)).unwrap();
        let response_stream = response
            .as_dictionary()
            .unwrap()
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.first())
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            response_stream
                .get("controlPort")
                .and_then(Value::as_unsigned_integer),
            Some(u64::from(ports.control_port))
        );
    }

    #[test]
    fn basic_apple_control_sequence_is_acknowledged_and_volume_is_strict() {
        let get = make_request(
            "GET_PARAMETER",
            "/stream",
            Some(TEXT_PARAMETERS_CONTENT_TYPE),
            b"volume\r\n",
        );
        let get = get_parameter_response(&get, None);
        assert_eq!(get.response.status, 503);
        assert!(get.response.body.is_empty());

        let get = make_request(
            "GET_PARAMETER",
            "/stream",
            Some(TEXT_PARAMETERS_CONTENT_TYPE),
            b"volume\r\n",
        );
        let get = get_parameter_response(
            &get,
            Some(ReceiverVolumeSnapshot::new(-17.25, false).unwrap()),
        );
        assert_eq!(get.response.body, b"volume: -17.250000\r\n");

        let muted = get_parameter_response(
            &make_request(
                "GET_PARAMETER",
                "/stream",
                Some(TEXT_PARAMETERS_CONTENT_TYPE),
                b"volume\r\n",
            ),
            Some(ReceiverVolumeSnapshot::new(-17.25, true).unwrap()),
        );
        assert_eq!(muted.response.body, b"volume: -144.000000\r\n");

        let record = make_request("RECORD", "/stream", None, b"");
        let record = record_response(&record);
        assert_eq!(record.response.status, 200);
        assert_eq!(
            record
                .response
                .headers
                .iter()
                .find(|header| header.name().eq_ignore_ascii_case("Audio-Latency"))
                .map(Header::value),
            Some(b"0".as_slice())
        );

        let volume = make_request(
            "SET_PARAMETER",
            "/stream",
            Some(TEXT_PARAMETERS_CONTENT_TYPE),
            b"volume: -30.000000\r\n",
        );
        let volume = set_parameter_response(&volume);
        assert_eq!(volume.response.status, 200);
        assert_eq!(volume.volume_db, Some(-30.0));

        let mute = make_request(
            "SET_PARAMETER",
            "/stream",
            Some(TEXT_PARAMETERS_CONTENT_TYPE),
            b"volume: -144.000000\r\n",
        );
        assert_eq!(set_parameter_response(&mute).volume_db, Some(-144.0));

        for invalid in [
            b"volume: -30.01\r\n".as_slice(),
            b"volume: 0\r\nvolume: -1\r\n",
        ] {
            let request = make_request(
                "SET_PARAMETER",
                "/stream",
                Some(TEXT_PARAMETERS_CONTENT_TYPE),
                invalid,
            );
            assert_eq!(set_parameter_response(&request).response.status, 400);
        }
    }

    #[test]
    fn authenticated_metadata_artwork_and_progress_are_accepted_and_latched() {
        fn dmap(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
            let mut encoded = Vec::with_capacity(8 + value.len());
            encoded.extend_from_slice(tag);
            encoded.extend_from_slice(&(value.len() as u32).to_be_bytes());
            encoded.extend_from_slice(value);
            encoded
        }

        let listing = [
            dmap(b"minm", b"Song"),
            dmap(b"asar", b"Artist"),
            dmap(b"asal", b"Album"),
        ]
        .concat();
        let metadata = dmap(b"mlit", &listing);
        let outcome = set_parameter_response(&make_request(
            "SET_PARAMETER",
            "/stream",
            Some("application/x-dmap-tagged; charset=binary"),
            &metadata,
        ));
        assert_eq!(outcome.response.status, 200);

        let mut latched = ConnectionNowPlaying::default();
        let changed = latched.apply(outcome.now_playing.unwrap());
        assert!(changed.metadata);
        assert!(latched
            .all_events(IpAddr::V4(Ipv4Addr::LOCALHOST), 9)
            .iter()
            .any(|event| matches!(
                event,
                ProbeEvent::Metadata {
                    stream_id: 9,
                    title: Some(title),
                    artist: Some(artist),
                    album: Some(album),
                    ..
                } if title == "Song" && artist == "Artist" && album == "Album"
            )));

        let progress = set_parameter_response(&make_request(
            "SET_PARAMETER",
            "/stream",
            Some(TEXT_PARAMETERS_CONTENT_TYPE),
            b"progress: 44100/88200/176400\r\n",
        ));
        let changed = latched.apply(progress.now_playing.unwrap());
        assert!(changed.progress);
        assert_eq!(latched.elapsed_ms, Some(1_000));
        assert_eq!(latched.duration_ms, Some(3_000));

        let clear = set_parameter_response(&make_request(
            "SET_PARAMETER",
            "/stream",
            Some("image/none"),
            b"none",
        ));
        let changed = latched.apply(clear.now_playing.unwrap());
        assert!(changed.artwork);
        assert!(latched.artwork.is_none());

        let malformed = set_parameter_response(&make_request(
            "SET_PARAMETER",
            "/stream",
            Some("application/x-dmap-tagged"),
            b"truncated",
        ));
        assert_eq!(malformed.response.status, 200);
        assert!(malformed.now_playing.is_none());
    }

    #[test]
    fn artwork_body_budget_is_large_only_for_authenticated_known_media_types() {
        let content_length = 2_393_210usize;
        let header = format!(
            "SET_PARAMETER /stream RTSP/1.0\r\nContent-Type: image/png\r\nContent-Length: {content_length}\r\n\r\n"
        );
        let limits = RequestLimits {
            default_max_body_bytes: MAX_NOW_PLAYING_BODY_BYTES,
            ..RequestLimits::default()
        };

        let mut authenticated = RequestDecoder::new(limits).unwrap();
        authenticated.feed(header.as_bytes()).unwrap();
        assert!(authenticated
            .next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, true)
            })
            .unwrap()
            .is_none());

        let mut plaintext = RequestDecoder::new(limits).unwrap();
        plaintext.feed(header.as_bytes()).unwrap();
        assert!(matches!(
            plaintext.next_request(|method, target, headers| {
                max_body_for_request(method, target, headers, false)
            }),
            Err(super::super::rtsp::RtspError::BodyTooLarge {
                actual: 2_393_210,
                max: MAX_CONTROL_BODY_BYTES,
            })
        ));
    }

    #[test]
    fn buffered_rate_and_flush_plists_are_binary_bounded_and_value_strict() {
        for (rate, expected) in [(0u64, false), (1, true)] {
            let mut dictionary = Dictionary::new();
            dictionary.insert("rate".into(), Value::Integer(rate.into()));
            dictionary.insert("rtpTime".into(), Value::Integer(123u64.into()));
            if expected {
                dictionary.insert(
                    "networkTimeSecs".into(),
                    Value::Integer(2_208_988_800u64.into()),
                );
                dictionary.insert("networkTimeFrac".into(), Value::Integer((-1i64).into()));
                dictionary.insert(
                    "networkTimeTimelineID".into(),
                    Value::Integer((-2i64).into()),
                );
            }
            let request = make_request(
                "SETRATEANCHORTIME",
                "/stream",
                Some(BINARY_PLIST_CONTENT_TYPE),
                &binary_plist(dictionary),
            );
            let parsed = parse_rate_anchor(&request).unwrap();
            assert_eq!(parsed.playing, expected);
            assert_eq!(parsed.rtp_time, Some(123));
            if expected {
                assert_eq!(parsed.network_time_frac, Some(u64::MAX));
                assert_eq!(parsed.timeline_id, Some(u64::MAX - 1));
            }
        }

        let mut signed_rtp = Dictionary::new();
        signed_rtp.insert("rate".into(), Value::Integer(1u64.into()));
        signed_rtp.insert("rtpTime".into(), Value::Integer((-1i64).into()));
        signed_rtp.insert(
            "networkTimeSecs".into(),
            Value::Integer(2_208_988_800u64.into()),
        );
        signed_rtp.insert("networkTimeFrac".into(), Value::Integer(0u64.into()));
        signed_rtp.insert("networkTimeTimelineID".into(), Value::Integer(1u64.into()));
        let request = make_request(
            "SETRATEANCHORTIME",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(signed_rtp),
        );
        assert_eq!(
            parse_rate_anchor(&request).unwrap().rtp_time,
            Some(u32::MAX)
        );

        let mut invalid_rate = Dictionary::new();
        invalid_rate.insert("rate".into(), Value::Integer(2u64.into()));
        let request = make_request(
            "SETRATEANCHORTIME",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(invalid_rate),
        );
        assert!(parse_rate_anchor(&request).is_err());

        let mut wrong_anchor_type = Dictionary::new();
        wrong_anchor_type.insert("rate".into(), Value::Integer(1u64.into()));
        wrong_anchor_type.insert("rtpTime".into(), Value::String("123".into()));
        let request = make_request(
            "SETRATEANCHORTIME",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(wrong_anchor_type),
        );
        assert!(parse_rate_anchor(&request).is_err());

        let empty_flush = make_request(
            "FLUSHBUFFERED",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(Dictionary::new()),
        );
        assert!(parse_buffered_flush(&empty_flush).is_err());

        let mut maximum_flush = Dictionary::new();
        maximum_flush.insert(
            "flushUntilSeq".into(),
            Value::Integer(MAX_BUFFERED_SEQUENCE.into()),
        );
        maximum_flush.insert("flushUntilTS".into(), Value::Integer((-1i64).into()));
        let request = make_request(
            "FLUSHBUFFERED",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(maximum_flush),
        );
        assert_eq!(
            parse_buffered_flush(&request),
            Ok(BufferedFlush::Immediate {
                until: WirePoint {
                    sequence: MAX_BUFFERED_SEQUENCE as u32,
                    timestamp: u32::MAX,
                }
            })
        );

        let mut overflow = Dictionary::new();
        overflow.insert(
            "flushUntilSeq".into(),
            Value::Integer((MAX_BUFFERED_SEQUENCE + 1).into()),
        );
        overflow.insert("flushUntilTS".into(), Value::Integer(0u64.into()));
        let request = make_request(
            "FLUSHBUFFERED",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(overflow),
        );
        assert!(parse_buffered_flush(&request).is_err());

        for incomplete in [
            [("flushUntilSeq", 4u64)],
            [("flushUntilTS", 4u64)],
            [("flushFromSeq", 4u64)],
            [("flushFromTS", 4u64)],
        ] {
            let mut dictionary = Dictionary::new();
            for (key, value) in incomplete {
                dictionary.insert(key.into(), Value::Integer(value.into()));
            }
            let request = make_request(
                "FLUSHBUFFERED",
                "/stream",
                Some(BINARY_PLIST_CONTENT_TYPE),
                &binary_plist(dictionary),
            );
            assert!(parse_buffered_flush(&request).is_err());
        }

        let mut wrapped = Dictionary::new();
        wrapped.insert(
            "flushFromSeq".into(),
            Value::Integer((MAX_BUFFERED_SEQUENCE - 1).into()),
        );
        wrapped.insert("flushFromTS".into(), Value::Integer((-2i64).into()));
        wrapped.insert("flushUntilSeq".into(), Value::Integer(1u64.into()));
        wrapped.insert("flushUntilTS".into(), Value::Integer(5u64.into()));
        let request = make_request(
            "FLUSHBUFFERED",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(wrapped),
        );
        assert_eq!(
            parse_buffered_flush(&request),
            Ok(BufferedFlush::Deferred {
                from: WirePoint {
                    sequence: MAX_BUFFERED_SEQUENCE as u32 - 1,
                    timestamp: u32::MAX - 1,
                },
                until: WirePoint {
                    sequence: 1,
                    timestamp: 5,
                },
            })
        );

        let mut ambiguous = Dictionary::new();
        ambiguous.insert("flushFromSeq".into(), Value::Integer(0u64.into()));
        ambiguous.insert("flushFromTS".into(), Value::Integer(0u64.into()));
        ambiguous.insert(
            "flushUntilSeq".into(),
            Value::Integer(0x0080_0000u64.into()),
        );
        ambiguous.insert("flushUntilTS".into(), Value::Integer(0u64.into()));
        let request = make_request(
            "FLUSHBUFFERED",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(ambiguous),
        );
        assert!(parse_buffered_flush(&request).is_err());

        let xml = br#"<?xml version="1.0"?><plist version="1.0"><dict><key>rate</key><integer>1</integer></dict></plist>"#;
        let request = make_request(
            "SETRATEANCHORTIME",
            "/stream",
            Some(BINARY_PLIST_CONTENT_TYPE),
            xml,
        );
        assert!(parse_rate_anchor(&request).is_err());

        let request = make_request(
            "SETRATEANCHORTIME",
            "/stream",
            Some(OCTET_STREAM_CONTENT_TYPE),
            &binary_plist({
                let mut dictionary = Dictionary::new();
                dictionary.insert("rate".into(), Value::Integer(1u64.into()));
                dictionary
            }),
        );
        assert!(parse_rate_anchor(&request).is_err());
    }

    #[tokio::test]
    async fn setup_rejects_non_binary_and_mistyped_plists() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><dict><key>timingProtocol</key><string>NTP</string></dict></plist>"#;
        let request = make_request("SETUP", "/session", Some(BINARY_PLIST_CONTENT_TYPE), xml);
        let mut event_keys = Some(derive_event_keys(&[0x81; 64]).unwrap());
        let mut phase_one_state = None;
        let next_stream_id = AtomicU64::new(1);
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one_state,
            false,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 400);
        assert!(event_keys.is_some());

        let mut dictionary = Dictionary::new();
        dictionary.insert("timingProtocol".into(), Value::Integer(1u64.into()));
        let request = make_request(
            "SETUP",
            "/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(dictionary),
        );
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one_state,
            false,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 400);
        assert!(event_keys.is_some());

        let mut dictionary = Dictionary::new();
        dictionary.insert("timingProtocol".into(), Value::String("NTP".into()));
        dictionary.insert("timingPort".into(), Value::Integer(7001u64.into()));
        dictionary.insert("ekey".into(), Value::Data(vec![0x11; 72]));
        let request = make_request(
            "SETUP",
            "/session",
            Some(BINARY_PLIST_CONTENT_TYPE),
            &binary_plist(dictionary),
        );
        let outcome = setup_response(
            &request,
            "127.0.0.1:6000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "receiver-peer",
            None,
            &media_permits(),
            &mut event_keys,
            false,
            &mut phase_one_state,
            false,
            &next_stream_id,
        )
        .await;
        assert_eq!(outcome.response.status, 400);
        assert!(event_keys.is_some());
    }

    #[test]
    fn phase_one_accepts_both_deployed_encryption_metadata_shapes() {
        for (et, key_len) in [(0u64, 16usize), (32, 72)] {
            let mut dictionary = Dictionary::new();
            dictionary.insert("timingProtocol".into(), Value::String("NTP".into()));
            dictionary.insert("timingPort".into(), Value::Integer(7001u64.into()));
            dictionary.insert("isRemoteControlOnly".into(), Value::Boolean(false));
            dictionary.insert("et".into(), Value::Integer(et.into()));
            dictionary.insert("ekey".into(), Value::Data(vec![0x11; key_len]));
            dictionary.insert("eiv".into(), Value::Data(vec![0x22; 16]));
            assert!(matches!(
                parse_setup(&binary_plist(dictionary)),
                Ok(ParsedSetup::Session {
                    remote_timing_port: 7001,
                    ..
                })
            ));
        }

        for (et, key_len) in [(0u64, 72usize), (32, 16), (7, 16)] {
            let mut dictionary = Dictionary::new();
            dictionary.insert("timingProtocol".into(), Value::String("NTP".into()));
            dictionary.insert("timingPort".into(), Value::Integer(7001u64.into()));
            dictionary.insert("et".into(), Value::Integer(et.into()));
            dictionary.insert("ekey".into(), Value::Data(vec![0x11; key_len]));
            dictionary.insert("eiv".into(), Value::Data(vec![0x22; 16]));
            assert!(parse_setup(&binary_plist(dictionary)).is_err());
        }
    }

    #[test]
    fn content_type_must_be_present_unique_and_exact() {
        let absent = make_request("POST", "/fp-setup", None, b"FPLY");
        assert!(!has_content_type(&absent, OCTET_STREAM_CONTENT_TYPE));

        let duplicate = parse_wire_request(
            b"POST /fp-setup RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/octet-stream\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\nFPLY",
        );
        assert!(!has_content_type(&duplicate, OCTET_STREAM_CONTENT_TYPE));

        let parameter = make_request(
            "POST",
            "/fp-setup",
            Some("application/octet-stream; charset=binary"),
            b"FPLY",
        );
        assert!(has_content_type(&parameter, OCTET_STREAM_CONTENT_TYPE));
    }

    #[test]
    fn pairing_accepts_airplay_octet_stream_and_emits_it_on_the_response() {
        for content_type in [
            OCTET_STREAM_CONTENT_TYPE,
            PAIRING_TLV8_CONTENT_TYPE,
            BINARY_PLIST_CONTENT_TYPE,
        ] {
            let request = make_request("POST", "/pair-setup", Some(content_type), &[6, 1, 1]);
            assert!(has_pairing_content_type(&request));
            let outcome = pairing_response(&request, vec![6, 1, 2], false);
            assert_eq!(outcome.response.status, 200);
            assert_eq!(
                outcome
                    .response
                    .headers
                    .iter()
                    .find(|header| header.name().eq_ignore_ascii_case("Content-Type"))
                    .map(Header::value),
                Some(OCTET_STREAM_CONTENT_TYPE.as_bytes())
            );
        }

        // Current macOS Music omits Content-Type on the nine-byte transient
        // M1 request. TLV8 is still unambiguous on this dedicated endpoint.
        let absent = make_request("POST", "/pair-setup", None, &[6, 1, 1, 0, 1, 0, 19, 1, 16]);
        assert!(has_pairing_content_type_or_none(&absent));

        let wrong = make_request("POST", "/pair-setup", Some("text/plain"), &[6, 1, 1]);
        assert!(!has_pairing_content_type_or_none(&wrong));

        let duplicate = parse_wire_request(
            b"POST /pair-setup RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/octet-stream\r\nContent-Type: application/octet-stream\r\nContent-Length: 3\r\n\r\n\x06\x01\x01",
        );
        assert!(!has_pairing_content_type_or_none(&duplicate));
    }

    #[tokio::test]
    async fn encrypted_command_and_audio_mode_keep_the_session_alive() {
        for target in ["/command", "/audioMode", "/feedback"] {
            let request = make_request(
                "POST",
                target,
                Some(BINARY_PLIST_CONTENT_TYPE),
                b"bounded opaque configuration",
            );
            let mut pairing = None;
            let mut event_keys = None;
            let mut phase_one = None;
            let outcome = dispatch_request(
                &request,
                true,
                "127.0.0.1:6000".parse().unwrap(),
                "127.0.0.1:7000".parse().unwrap(),
                &config(),
                &PairSetupLimiter::new(),
                &mut pairing,
                &mut event_keys,
                false,
                &mut phase_one,
                false,
                None,
                None,
                &AtomicU64::new(1),
            )
            .await;
            assert_eq!(outcome.response.status, 200, "{target}");
            assert!(!outcome.close_after, "{target}");
        }
    }
}
