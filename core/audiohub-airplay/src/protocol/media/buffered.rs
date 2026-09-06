//! AirPlay 2 buffered-audio transport (stream type 103).
//!
//! This module owns the complete data path after RTSP negotiation: it validates
//! the one format AudioHub currently advertises, binds a peer-restricted TCP
//! listener, frames and authenticates incoming blocks, decodes raw AAC-LC, and
//! delivers paced stereo PCM.  The implementation intentionally accepts no
//! codec or channel-layout fallbacks; expanding the advertised profile must be
//! accompanied by an explicit decoder path here.

use super::buffered_clock::{network_time_nanos, PlaybackAnchor};
use super::engine::PcmOutput;
use super::ptp::{PtpClockError, PtpClockMapper, PtpClockSample, PtpClockSource};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use plist::{Dictionary, Value};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io::{self, Cursor};
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::Duration;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia::core::formats::Packet;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant, MissedTickBehavior};
use zeroize::Zeroize;

pub(crate) const STREAM_TYPE_BUFFERED_AUDIO: u64 = 103;
pub(crate) const AUDIO_FORMAT_AAC_LC_44K1_STEREO: u64 = 0x0040_0000;
// AirPlay's SETUP compression identifiers are bit values rather than the
// sequential codec IDs used by some legacy receivers: 4 is AAC, while 8
// is AAC-ELD.  Stock macOS Music sends `ct=4` with the 0x400000 AAC-LC profile.
pub(crate) const COMPRESSION_TYPE_AAC_LC: u64 = 4;
pub(crate) const SAMPLES_PER_AAC_FRAME: u64 = 1_024;
pub(crate) const BUFFERED_SAMPLE_RATE: u64 = 44_100;
pub(crate) const BUFFERED_CHANNELS: usize = 2;
pub(crate) const BUFFERED_SSRC_AAC_44K1_STEREO: u32 = 0x1600_0000;
pub(crate) const ADVERTISED_AUDIO_BUFFER_BYTES: u64 = 8 * 1024 * 1024;

const SHARED_KEY_BYTES: usize = 32;
const MAX_SETUP_BODY_BYTES: usize = 64 * 1024;
const HEADER_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const NONCE_SUFFIX_BYTES: usize = 8;
// Stock Music emits authenticated zero-payload blocks at stream boundaries.
// They advance the transport sequence but contain no AAC access unit.
const MIN_PACKET_BYTES: usize = HEADER_BYTES + TAG_BYTES + NONCE_SUFFIX_BYTES;
const LENGTH_PREFIX_BYTES: usize = 2;
const MIN_WIRE_BLOCK_BYTES: usize = LENGTH_PREFIX_BYTES + MIN_PACKET_BYTES;
const TCP_READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_PENDING_TCP_BYTES: usize = ADVERTISED_AUDIO_BUFFER_BYTES as usize;
const COMMAND_CAPACITY: usize = 8;
// A type-103 setup gets a fresh AEAD key and its transmitted 8-byte nonce is a
// little-endian monotonic counter. Refuse a second traversal of the 24-bit
// transport sequence space under one key. This bounds the session lifetime
// without retaining one heap allocation per authenticated block.
const MAX_AUTHENTICATED_BLOCKS: usize = SEQUENCE_MODULUS as usize;
const MAX_BLOCKS_PER_PUMP: usize = 64;
const MAX_PENDING_PTP_SAMPLES: usize = 32;
const MAX_CLOCKED_FUTURE: Duration = Duration::from_secs(30);
const MAX_CLOCKED_LATENESS: Duration = Duration::from_millis(500);
// The default PCM bus primes five 10 ms frames. Feed it this far ahead of the
// network presentation deadline so the platform output starts on the anchor.
const PCM_OUTPUT_LEAD: Duration = Duration::from_millis(50);
/// How long a **starved** receiver waits for a sender that stopped mid-frame
/// before giving up on the data channel.
///
/// ⚠ This used to be 5 s and the only other condition was `read_limit > 0`,
/// which killed healthy sessions after a few minutes of normal playback
/// (observed on 30-win, 2026-08-16: two iOS sessions ended at 425 s and 410 s
/// with `buffered data channel stalled inside a framed block`, while the RTSP
/// control channel kept answering every request with 200 — the client went on
/// showing "connected and playing" with no audio and no working volume).
///
/// The reasoning that produced 5 s does not survive contact with buffered mode.
/// A buffered sender ships far ahead and then goes quiet — that is the whole
/// point of announcing an 8 MiB buffer — and a TCP burst has no reason to end on
/// a frame boundary, so "a partial frame is pending and nothing has arrived for
/// five seconds" is the *normal* idle shape, not a fault. The `read_limit > 0`
/// guard was supposed to cover it but almost never fires: `read_limit` is
/// `8 MiB - retained`, and retention sits far below 8 MiB for all of a healthy
/// session.
///
/// What actually distinguishes a dead sender is that we have run *out* — no
/// queued blocks, no ready frame, still playing, and still nothing arriving. See
/// `starved_stall`.
const STARVED_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const DATA_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(250);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const SEQUENCE_MASK: u32 = 0x00ff_ffff;
const SEQUENCE_MODULUS: u32 = 0x0100_0000;
const SEQUENCE_HALF_RANGE: u32 = SEQUENCE_MODULUS / 2;
const MAX_DEFERRED_FLUSHES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferedRateAnchor {
    pub(crate) playing: bool,
    pub(crate) rtp_time: Option<u32>,
    pub(crate) network_time_secs: Option<u64>,
    pub(crate) network_time_frac: Option<u64>,
    pub(crate) timeline_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WirePoint {
    pub(crate) sequence: u32,
    /// Sender control-timeline coordinate for this flush boundary.  It is not
    /// necessarily the RTP timestamp carried by the type-103 block with the
    /// same sequence number (native senders can change RTP epochs at a seek).
    pub(crate) timestamp: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferedFlush {
    All,
    Immediate { until: WirePoint },
    Deferred { from: WirePoint, until: WirePoint },
}

/// Keeps extracted plist key material out of ordinary error/debug values and
/// erases it even when validation of a later field fails.
struct SecretBytes(Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        SECRET_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
static SECRET_DROP_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Exact negotiated parameters for AudioHub's first buffered-audio profile.
/// The shared key cannot be cloned and is redacted from `Debug` output.
pub(crate) struct Type103Setup {
    pub(crate) remote_control_port: Option<u16>,
    shared_key: [u8; SHARED_KEY_BYTES],
}

impl Type103Setup {
    pub(crate) fn shared_key(&self) -> &[u8; SHARED_KEY_BYTES] {
        &self.shared_key
    }
}

impl fmt::Debug for Type103Setup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Type103Setup")
            .field("stream_type", &STREAM_TYPE_BUFFERED_AUDIO)
            .field("audio_format", &AUDIO_FORMAT_AAC_LC_44K1_STEREO)
            .field("compression_type", &COMPRESSION_TYPE_AAC_LC)
            .field("samples_per_frame", &SAMPLES_PER_AAC_FRAME)
            .field("sample_rate", &BUFFERED_SAMPLE_RATE)
            .field("channels", &BUFFERED_CHANNELS)
            .field("remote_control_port", &self.remote_control_port)
            .field("shared_key", &"<redacted>")
            .finish()
    }
}

impl Drop for Type103Setup {
    fn drop(&mut self) {
        self.shared_key.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BufferedSetupError {
    EmptyBody,
    BodyTooLarge { actual: usize, maximum: usize },
    NotBinaryPlist,
    MalformedPlist,
    RootNotDictionary,
    MissingField(&'static str),
    WrongFieldType(&'static str),
    StreamCount { actual: usize },
    UnsupportedValue { field: &'static str, actual: u64 },
    InvalidSharedKeyLength { actual: usize },
}

impl fmt::Display for BufferedSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBody => f.write_str("buffered SETUP body is empty"),
            Self::BodyTooLarge { actual, maximum } => write!(
                f,
                "buffered SETUP body is {actual} bytes; limit is {maximum}"
            ),
            Self::NotBinaryPlist => {
                f.write_str("buffered SETUP body is not a binary property list")
            }
            Self::MalformedPlist => f.write_str("buffered SETUP property list is malformed"),
            Self::RootNotDictionary => {
                f.write_str("buffered SETUP property-list root is not a dictionary")
            }
            Self::MissingField(field) => write!(f, "buffered SETUP is missing {field}"),
            Self::WrongFieldType(field) => {
                write!(f, "buffered SETUP field {field} has the wrong type")
            }
            Self::StreamCount { actual } => write!(
                f,
                "buffered SETUP has {actual} streams; exactly one is accepted"
            ),
            Self::UnsupportedValue { field, actual } => write!(
                f,
                "buffered SETUP field {field} has unsupported value {actual}"
            ),
            Self::InvalidSharedKeyLength { actual } => write!(
                f,
                "buffered SETUP shared key has {actual} bytes; expected {SHARED_KEY_BYTES}"
            ),
        }
    }
}

impl Error for BufferedSetupError {}

/// Validate a phase-two SETUP body for type 103 AAC-LC.
///
/// `ct` and `sr` are optional in observed sender plists, but when supplied they
/// must agree with the selected `audioFormat`.  `type`, `audioFormat`, `spf`,
/// and the 32-byte session key are mandatory.
pub(crate) fn parse_phase2(body: &[u8]) -> Result<Type103Setup, BufferedSetupError> {
    if body.is_empty() {
        return Err(BufferedSetupError::EmptyBody);
    }
    if body.len() > MAX_SETUP_BODY_BYTES {
        return Err(BufferedSetupError::BodyTooLarge {
            actual: body.len(),
            maximum: MAX_SETUP_BODY_BYTES,
        });
    }
    if !body.starts_with(b"bplist00") {
        return Err(BufferedSetupError::NotBinaryPlist);
    }

    let mut root_value =
        Value::from_reader(Cursor::new(body)).map_err(|_| BufferedSetupError::MalformedPlist)?;
    let root = root_value
        .as_dictionary_mut()
        .ok_or(BufferedSetupError::RootNotDictionary)?;
    let streams = root
        .get_mut("streams")
        .ok_or(BufferedSetupError::MissingField("streams"))?
        .as_array_mut()
        .ok_or(BufferedSetupError::WrongFieldType("streams"))?;
    if streams.len() != 1 {
        return Err(BufferedSetupError::StreamCount {
            actual: streams.len(),
        });
    }
    let stream = streams[0]
        .as_dictionary_mut()
        .ok_or(BufferedSetupError::WrongFieldType("streams[0]"))?;

    let key_value = stream
        .remove("shk")
        .ok_or(BufferedSetupError::MissingField("shk"))?;
    let Value::Data(key_data) = key_value else {
        return Err(BufferedSetupError::WrongFieldType("shk"));
    };
    let key_data = SecretBytes(key_data);

    require_exact_integer(stream, "type", STREAM_TYPE_BUFFERED_AUDIO)?;
    require_exact_integer(stream, "audioFormat", AUDIO_FORMAT_AAC_LC_44K1_STEREO)?;
    require_exact_integer(stream, "spf", SAMPLES_PER_AAC_FRAME)?;
    optional_exact_integer(stream, "ct", COMPRESSION_TYPE_AAC_LC)?;
    optional_exact_integer(stream, "sr", BUFFERED_SAMPLE_RATE)?;
    let remote_control_port = optional_port(stream, "controlPort")?;

    if key_data.0.len() != SHARED_KEY_BYTES {
        return Err(BufferedSetupError::InvalidSharedKeyLength {
            actual: key_data.0.len(),
        });
    }
    let mut shared_key = [0u8; SHARED_KEY_BYTES];
    shared_key.copy_from_slice(&key_data.0);

    Ok(Type103Setup {
        remote_control_port,
        shared_key,
    })
}

fn integer_field(dictionary: &Dictionary, field: &'static str) -> Result<u64, BufferedSetupError> {
    dictionary
        .get(field)
        .ok_or(BufferedSetupError::MissingField(field))?
        .as_unsigned_integer()
        .ok_or(BufferedSetupError::WrongFieldType(field))
}

fn require_exact_integer(
    dictionary: &Dictionary,
    field: &'static str,
    expected: u64,
) -> Result<(), BufferedSetupError> {
    let actual = integer_field(dictionary, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(BufferedSetupError::UnsupportedValue { field, actual })
    }
}

fn optional_exact_integer(
    dictionary: &Dictionary,
    field: &'static str,
    expected: u64,
) -> Result<(), BufferedSetupError> {
    if !dictionary.contains_key(field) {
        return Ok(());
    }
    require_exact_integer(dictionary, field, expected)
}

fn optional_port(
    dictionary: &Dictionary,
    field: &'static str,
) -> Result<Option<u16>, BufferedSetupError> {
    let Some(value) = dictionary.get(field) else {
        return Ok(None);
    };
    let actual = value
        .as_unsigned_integer()
        .ok_or(BufferedSetupError::WrongFieldType(field))?;
    let port = u16::try_from(actual)
        .ok()
        .filter(|port| *port != 0)
        .ok_or(BufferedSetupError::UnsupportedValue { field, actual })?;
    Ok(Some(port))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Type103Ports {
    pub(crate) data_port: u16,
    pub(crate) control_port: u16,
    pub(crate) audio_buffer_size: u64,
}

/// Type-103 resources are bound and the decoder is constructed before SETUP is
/// acknowledged.  Starting the stream therefore cannot fail later because a
/// port or codec was unavailable.
pub(crate) struct PreparedType103 {
    listener: TcpListener,
    control_socket: UdpSocket,
    peer_ip: IpAddr,
    decryptor: BufferedDecryptor,
    decoder: AacLcDecoder,
    ptp_source: PtpClockSource,
    ptp_samples: broadcast::Receiver<PtpClockSample>,
    ptp_epoch: std::time::Instant,
    ports: Type103Ports,
}

impl PreparedType103 {
    pub(crate) async fn prepare(
        local: SocketAddr,
        peer: SocketAddr,
        setup: Type103Setup,
        ptp_source: PtpClockSource,
    ) -> io::Result<Self> {
        validate_media_endpoints(local, peer)?;
        let listener = TcpListener::bind(with_port(local, 0)).await?;
        let control_socket = UdpSocket::bind(with_port(local, 0)).await?;
        let ports = Type103Ports {
            data_port: listener.local_addr()?.port(),
            control_port: control_socket.local_addr()?.port(),
            audio_buffer_size: ADVERTISED_AUDIO_BUFFER_BYTES,
        };
        let decryptor = BufferedDecryptor::new(*setup.shared_key());
        let decoder = AacLcDecoder::new().map_err(|error| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("AAC-LC decoder unavailable: {error}"),
            )
        })?;

        Ok(Self {
            listener,
            control_socket,
            peer_ip: peer.ip(),
            decryptor,
            decoder,
            ptp_samples: ptp_source.subscribe(),
            ptp_source,
            ptp_epoch: std::time::Instant::now(),
            ports,
        })
    }

    pub(crate) fn ports(&self) -> Type103Ports {
        self.ports
    }

    /// The stream begins rate-gated.  A valid `SETRATEANCHORTIME` with rate 1
    /// should install a valid playing anchor before PCM is released.
    pub(crate) fn start(self, output: Box<dyn PcmOutput>) -> Type103MediaHandle {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let task = tokio::spawn(async move {
            let mut engine = Type103Engine::new(self, output, command_rx);
            match engine.run().await {
                RunExit::Shutdown(reply) => {
                    let _ = reply.send(());
                }
                RunExit::PeerClosed => {}
                RunExit::Io(error) => {
                    log::warn!("AirPlay 2 buffered media task stopped: {error}")
                }
            }
        });
        Type103MediaHandle { commands, task }
    }
}

pub(crate) struct Type103MediaHandle {
    commands: mpsc::Sender<BufferedCommand>,
    task: JoinHandle<()>,
}

impl Type103MediaHandle {
    pub(crate) async fn set_rate(&self, anchor: BufferedRateAnchor) -> io::Result<()> {
        let telemetry_token = crate::telemetry::operation_token();
        let (reply, done) = oneshot::channel();
        self.send_command(BufferedCommand::SetRate {
            anchor,
            telemetry_token,
            reply,
        })
        .await?;
        wait_for_reply(done).await
    }

    /// Apply an authenticated buffered-stream flush request.
    pub(crate) async fn flush(&self, request: BufferedFlush) -> io::Result<()> {
        let telemetry_token = crate::telemetry::operation_token();
        let invalid_sequence = match request {
            BufferedFlush::All => false,
            BufferedFlush::Immediate { until } => until.sequence > SEQUENCE_MASK,
            BufferedFlush::Deferred { from, until } => {
                from.sequence > SEQUENCE_MASK || until.sequence > SEQUENCE_MASK
            }
        };
        if invalid_sequence {
            crate::telemetry::record_control_failure(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "invalid_flush",
            );
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffered flush boundary exceeds the 24-bit sequence range",
            ));
        }
        let (reply, done) = oneshot::channel();
        self.send_command(BufferedCommand::Flush {
            request,
            telemetry_token,
            reply,
        })
        .await?;
        wait_for_reply(done).await
    }

    async fn send_command(&self, command: BufferedCommand) -> io::Result<()> {
        self.commands.send(command).await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "buffered media task has stopped")
        })
    }

    pub(crate) async fn abort(&self) {
        if self.task.is_finished() {
            return;
        }
        let (reply, done) = oneshot::channel();
        let sent = time::timeout(
            SHUTDOWN_TIMEOUT,
            self.commands.send(BufferedCommand::Shutdown { reply }),
        )
        .await;
        if matches!(sent, Ok(Ok(()))) && time::timeout(SHUTDOWN_TIMEOUT, done).await.is_ok() {
            return;
        }
        self.task.abort();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for Type103MediaHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_for_reply(done: oneshot::Receiver<io::Result<()>>) -> io::Result<()> {
    done.await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "buffered media task has stopped"))?
}

enum BufferedCommand {
    SetRate {
        anchor: BufferedRateAnchor,
        telemetry_token: crate::telemetry::OperationToken,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Flush {
        request: BufferedFlush,
        telemetry_token: crate::telemetry::OperationToken,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

enum CommandResult {
    Continue,
    Shutdown(oneshot::Sender<()>),
}

enum RunExit {
    Shutdown(oneshot::Sender<()>),
    PeerClosed,
    Io(io::Error),
}

struct Type103Engine {
    listener: TcpListener,
    // Keep the advertised UDP endpoint bound for the lifetime of the stream.
    // AudioHub does not currently implement control datagrams for type 103;
    // polling and discarding an untrusted socket here would let a LAN sender
    // starve PTP, TCP media, and shutdown processing.
    _control_socket: UdpSocket,
    peer_ip: IpAddr,
    decryptor: BufferedDecryptor,
    decoder: AacLcDecoder,
    output: Box<dyn PcmOutput>,
    commands: mpsc::Receiver<BufferedCommand>,
    playing: bool,
    flush_boundary: Option<WirePoint>,
    deferred_flushes: Vec<DeferredFlush>,
    last_sequence: Option<u32>,
    nonce_history: SessionNonceCounter,
    framer: TcpBlockFramer,
    /// Number of already-received TCP bytes that belonged to the stream before
    /// a boundary-less FLUSH. If the marker ends inside a block, that complete
    /// block is discarded once framing can finish; clearing the raw buffer
    /// would instead desynchronize every following length prefix.
    flush_pending_wire_bytes: usize,
    queued_blocks: VecDeque<AuthenticatedBlock>,
    queued_wire_bytes: usize,
    ready_frame: Option<ReadyFrame>,
    playback_anchor: Option<PlaybackAnchor>,
    ptp_source: PtpClockSource,
    ptp_samples: broadcast::Receiver<PtpClockSample>,
    pending_ptp_samples: VecDeque<PtpClockSample>,
    ptp_epoch: std::time::Instant,
    ptp_clock: Option<PtpClockMapper>,
    ptp_clock_logged: bool,
    authenticated_block_seen: bool,
    decoded_block_seen: bool,
    /// Blocks discarded since the last frame actually delivered.
    ///
    /// A discard used to cost a decode, a `decoder.reset()` and an
    /// `output.flush()` **each**, and the flush empties the whole PCM ring and
    /// un-primes every reader (`bus.rs` `reset_locked`). A multi-second outage
    /// therefore ran that ~86 times, which is what destroyed the cushion the
    /// buffer exists to hold — the audible "the 2 s delay is gone" the user
    /// reported on 2026-08-16. Counting the run and settling up once, when the
    /// first frame is delivered again, keeps the discard decision (all three
    /// reference receivers discard late audio) without the collateral damage.
    late_run_blocks: u64,
    late_run_first: Option<(u32, u32)>,
    /// Edge detection for the media clock. The `Err` arm of `ready_schedule`
    /// used to log at debug, so an INFO capture of a real failure showed
    /// nothing at all during the outage and then a burst of drop warnings —
    /// leaving no way to tell a network gap from a local clock stall.
    clock_unavailable_since: Option<std::time::Instant>,
    last_clock_outage_ms: u64,
    housekeeping_ticks: u32,
}

struct AuthenticatedBlock {
    header: BufferedHeader,
    payload: Vec<u8>,
    wire_bytes: usize,
    telemetry_token: crate::telemetry::OperationToken,
}

struct ReadyFrame {
    sequence: u32,
    timestamp: u32,
    pcm: Vec<i16>,
    wire_bytes: usize,
    telemetry_token: crate::telemetry::OperationToken,
    deadline_not_ready_since: Option<std::time::Instant>,
    deadline_warmup_reported: bool,
    deadline_failure_reasons: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadySchedule {
    Wait,
    DeliverAt(Instant),
    DropLate,
}

#[derive(Debug, Clone, Copy)]
struct DeferredFlush {
    from: WirePoint,
    until: WirePoint,
    activated: bool,
}

impl Type103Engine {
    fn new(
        prepared: PreparedType103,
        output: Box<dyn PcmOutput>,
        commands: mpsc::Receiver<BufferedCommand>,
    ) -> Self {
        Self {
            listener: prepared.listener,
            _control_socket: prepared.control_socket,
            peer_ip: prepared.peer_ip,
            decryptor: prepared.decryptor,
            decoder: prepared.decoder,
            output,
            commands,
            playing: false,
            flush_boundary: None,
            deferred_flushes: Vec::with_capacity(MAX_DEFERRED_FLUSHES),
            last_sequence: None,
            nonce_history: SessionNonceCounter::new(MAX_AUTHENTICATED_BLOCKS),
            framer: TcpBlockFramer::new(),
            flush_pending_wire_bytes: 0,
            queued_blocks: VecDeque::new(),
            queued_wire_bytes: 0,
            ready_frame: None,
            playback_anchor: None,
            ptp_source: prepared.ptp_source,
            ptp_samples: prepared.ptp_samples,
            pending_ptp_samples: VecDeque::with_capacity(MAX_PENDING_PTP_SAMPLES),
            ptp_epoch: prepared.ptp_epoch,
            ptp_clock: None,
            ptp_clock_logged: false,
            authenticated_block_seen: false,
            decoded_block_seen: false,
            late_run_blocks: 0,
            late_run_first: None,
            clock_unavailable_since: None,
            last_clock_outage_ms: 0,
            housekeeping_ticks: 0,
        }
    }

    async fn run(&mut self) -> RunExit {
        loop {
            let result = match self.accept_expected_peer().await {
                Ok(AcceptResult::Connected(stream)) => self.run_stream(stream).await,
                Ok(AcceptResult::Shutdown(reply)) => RunExit::Shutdown(reply),
                Err(error) => RunExit::Io(error),
            };
            match result {
                RunExit::PeerClosed => {
                    // A buffered sender may replace its TCP data connection
                    // without rebuilding the authenticated RTSP session. Drop
                    // all connection-local framing/codec state, but keep the
                    // negotiated key and the session-wide nonce replay set.
                    self.reset_for_data_reconnect();
                }
                result => {
                    self.output.flush();
                    return result;
                }
            }
        }
    }

    fn reset_for_data_reconnect(&mut self) {
        self.framer = TcpBlockFramer::new();
        self.flush_pending_wire_bytes = 0;
        self.queued_blocks.clear();
        self.queued_wire_bytes = 0;
        self.ready_frame = None;
        self.flush_boundary = None;
        self.deferred_flushes.clear();
        self.last_sequence = None;
        self.reset_decoded_output();
    }

    async fn accept_expected_peer(&mut self) -> io::Result<AcceptResult> {
        let accept_deadline = time::sleep(DATA_ACCEPT_TIMEOUT);
        tokio::pin!(accept_deadline);
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        Some(command) => match self.apply_command(command) {
                            CommandResult::Continue => {}
                            CommandResult::Shutdown(reply) => {
                                return Ok(AcceptResult::Shutdown(reply));
                            }
                        },
                        None => return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "buffered media controller was dropped",
                        )),
                    }
                }
                accepted = self.listener.accept() => {
                    let (stream, peer) = accepted?;
                    if peer.ip() != self.peer_ip {
                        log::debug!(
                            "AirPlay 2 buffered data connection rejected from unexpected peer {}",
                            peer.ip()
                        );
                        drop(stream);
                        continue;
                    }
                    stream.set_nodelay(true)?;
                    log::info!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 buffered data connection accepted peer={peer}"
                    );
                    return Ok(AcceptResult::Connected(stream));
                }
                sample = self.ptp_samples.recv() => {
                    self.observe_ptp_sample(sample);
                }
                _ = &mut accept_deadline => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "buffered data peer did not connect within 30 seconds",
                    ));
                }
            }
        }
    }

    async fn run_stream(&mut self, mut stream: TcpStream) -> RunExit {
        let mut read_buffer = vec![0u8; TCP_READ_CHUNK_BYTES];
        let mut drops_this_pass = 0usize;
        let mut housekeeping = time::interval(HOUSEKEEPING_INTERVAL);
        housekeeping.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut stream_eof = false;

        loop {
            if let Err(error) = self.pump_authenticated_blocks() {
                return RunExit::Io(error);
            }
            if let Err(error) = self.prepare_ready_frame() {
                return RunExit::Io(error);
            }

            let schedule = self.ready_schedule();
            match schedule {
                ReadySchedule::DeliverAt(deadline)
                    if self.playing_is_current() && deadline <= Instant::now() =>
                {
                    self.deliver_ready_frame();
                    drops_this_pass = 0;
                    continue;
                }
                ReadySchedule::DropLate if self.playing_is_current() => {
                    self.drop_late_ready_frame();
                    drops_this_pass += 1;
                    if drops_this_pass >= MAX_BLOCKS_PER_PUMP {
                        drops_this_pass = 0;
                        // Catching up is a synchronous loop bounded only by the
                        // queue, which can hold minutes of audio. Without this
                        // the command channel, the PTP samples and the TCP read
                        // are all starved for its duration, so an RTSP flush or
                        // set_rate arriving mid-catch-up waits it out.
                        tokio::task::yield_now().await;
                    }
                    continue;
                }
                _ => {}
            }

            if stream_eof
                && self.framer.is_empty()
                && self.queued_blocks.is_empty()
                && self.ready_frame.is_none()
            {
                return RunExit::PeerClosed;
            }

            let retained = self.retained_wire_bytes();
            if retained > ADVERTISED_AUDIO_BUFFER_BYTES as usize {
                return RunExit::Io(io::Error::other(
                    "buffered media retention exceeded the advertised audio buffer",
                ));
            }
            let read_limit =
                (ADVERTISED_AUDIO_BUFFER_BYTES as usize - retained).min(TCP_READ_CHUNK_BYTES);
            let deadline = match schedule {
                ReadySchedule::DeliverAt(deadline) => Some(deadline),
                ReadySchedule::Wait | ReadySchedule::DropLate => None,
            };
            let has_deadline = self.playing_is_current() && deadline.is_some();
            let sleep_deadline = deadline.unwrap_or_else(|| {
                Instant::now()
                    .checked_add(Duration::from_secs(24 * 60 * 60))
                    .expect("one day fits tokio Instant")
            });

            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    match command {
                        Some(command) => match self.apply_command(command) {
                            CommandResult::Continue => {}
                            CommandResult::Shutdown(reply) => {
                                return RunExit::Shutdown(reply);
                            }
                        },
                        None => return RunExit::PeerClosed,
                    }
                }
                _ = time::sleep_until(sleep_deadline), if has_deadline => {
                    self.deliver_ready_frame();
                }
                sample = self.ptp_samples.recv() => {
                    self.observe_ptp_sample(sample);
                }
                read = stream.read(&mut read_buffer[..read_limit]), if !stream_eof && read_limit > 0 => {
                    match read {
                        Ok(0) if self.framer.is_empty() => stream_eof = true,
                        Ok(0) => return RunExit::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "buffered data channel closed during a framed block",
                        )),
                        Ok(length) => {
                            if let Err(error) = self.framer.push(&read_buffer[..length]) {
                                return RunExit::Io(error.into());
                            }
                        }
                        Err(error) => return RunExit::Io(error),
                    }
                }
                _ = housekeeping.tick() => {
                    self.housekeeping_ticks = self.housekeeping_ticks.wrapping_add(1);
                    // Every 5 s. `lead_ms` is how far ahead of the speaker the
                    // next frame is — the one number that answers "did the
                    // cushion come back" without relying on how it sounded.
                    if self.housekeeping_ticks % 20 == 0 && self.playing_is_current() {
                        if let Some((timestamp, telemetry_token)) = self
                            .ready_frame
                            .as_ref()
                            .map(|ready| (ready.timestamp, ready.telemetry_token))
                        {
                            let now = std::time::Instant::now();
                            let lead_ms = self
                                .deadline_for(timestamp, telemetry_token)
                                .map(|d| d.saturating_duration_since(now).as_millis() as i64)
                                .unwrap_or(-1);
                            log::info!(
                                target: "audiohub_airplay::airplay2",
                                "AirPlay 2 buffered lead_ms={} queued_blocks={} retained_bytes={}",
                                lead_ms,
                                self.queued_blocks.len(),
                                self.retained_wire_bytes()
                            );
                        }
                    }
                    if starved_stall(
                        self.playing_is_current(),
                        self.queued_blocks.is_empty(),
                        self.ready_frame.is_none(),
                        read_limit,
                        self.framer.partial_frame_timed_out(Instant::now()),
                    ) {
                        return RunExit::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "buffered data channel stalled inside a framed block \
                             while the queue was empty",
                        ));
                    }
                }
            }
        }
    }

    fn apply_command(&mut self, command: BufferedCommand) -> CommandResult {
        match command {
            BufferedCommand::SetRate {
                anchor,
                telemetry_token,
                reply,
            } => {
                let playing = anchor.playing;
                let state_changed = self.playing != playing;
                let result = if playing {
                    self.install_playback_anchor(anchor, telemetry_token)
                } else {
                    self.playback_anchor = None;
                    Ok(())
                };
                if result.is_ok() {
                    if state_changed {
                        self.playing = playing;
                        if !playing {
                            self.output.flush();
                        }
                    }
                }
                if result.is_err() {
                    crate::telemetry::record_control_failure(
                        telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                        "invalid_anchor",
                    );
                }
                let _ = reply.send(result);
                CommandResult::Continue
            }
            BufferedCommand::Flush {
                request,
                telemetry_token,
                reply,
            } => {
                let result = match request {
                    BufferedFlush::All => {
                        let dropped =
                            self.queued_blocks.len() + usize::from(self.ready_frame.is_some());
                        crate::telemetry::record_expected_control_disposition(
                            telemetry_token,
                            crate::telemetry::MediaKind::Buffered,
                            "control_flush",
                            1,
                        );
                        crate::telemetry::record_expected_control_disposition(
                            telemetry_token,
                            crate::telemetry::MediaKind::Buffered,
                            "control_flush_drop",
                            dropped,
                        );
                        self.playing = false;
                        self.playback_anchor = None;
                        self.flush_boundary = None;
                        self.deferred_flushes.clear();
                        self.flush_pending_wire_bytes = self.framer.pending_len();
                        self.queued_blocks.clear();
                        self.queued_wire_bytes = 0;
                        self.ready_frame = None;
                        self.last_sequence = None;
                        self.reset_decoded_output();
                        Ok(())
                    }
                    BufferedFlush::Immediate { until } => {
                        let (boundary_reached, dropped) = self.discard_before(until);
                        crate::telemetry::record_expected_control_disposition(
                            telemetry_token,
                            crate::telemetry::MediaKind::Buffered,
                            "control_flush",
                            1,
                        );
                        crate::telemetry::record_expected_control_disposition(
                            telemetry_token,
                            crate::telemetry::MediaKind::Buffered,
                            "control_flush_drop",
                            dropped,
                        );
                        self.playing = false;
                        self.playback_anchor = None;
                        self.flush_boundary = (!boundary_reached).then_some(until);
                        self.deferred_flushes.clear();
                        self.reset_decoded_output();
                        Ok(())
                    }
                    BufferedFlush::Deferred { from, until } => {
                        let distance = sequence_distance(from.sequence, until.sequence);
                        if distance >= SEQUENCE_HALF_RANGE {
                            crate::telemetry::record_control_failure(
                                telemetry_token,
                                crate::telemetry::MediaKind::Buffered,
                                "invalid_flush",
                            );
                            Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "deferred buffered flush spans an ambiguous sequence range",
                            ))
                        } else if distance == 0 {
                            crate::telemetry::record_expected_control_disposition(
                                telemetry_token,
                                crate::telemetry::MediaKind::Buffered,
                                "deferred_flush",
                                1,
                            );
                            Ok(())
                        } else if self.deferred_flushes.len() >= MAX_DEFERRED_FLUSHES {
                            crate::telemetry::record_control_failure(
                                telemetry_token,
                                crate::telemetry::MediaKind::Buffered,
                                "deferred_limit",
                            );
                            Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "too many pending deferred buffered flushes",
                            ))
                        } else {
                            crate::telemetry::record_expected_control_disposition(
                                telemetry_token,
                                crate::telemetry::MediaKind::Buffered,
                                "deferred_flush",
                                1,
                            );
                            self.deferred_flushes.push(DeferredFlush {
                                from,
                                until,
                                activated: false,
                            });
                            Ok(())
                        }
                    }
                };
                let _ = reply.send(result);
                CommandResult::Continue
            }
            BufferedCommand::Shutdown { reply } => CommandResult::Shutdown(reply),
        }
    }

    fn reset_decoded_output(&mut self) {
        self.decoder.reset();
        self.output.flush();
    }

    fn install_playback_anchor(
        &mut self,
        anchor: BufferedRateAnchor,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> io::Result<()> {
        let (Some(rtp_time), Some(seconds), Some(fraction), Some(timeline_id)) = (
            anchor.rtp_time,
            anchor.network_time_secs,
            anchor.network_time_frac,
            anchor.timeline_id,
        ) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "playing rate anchor is incomplete",
            ));
        };
        let remote_time_ns = network_time_nanos(seconds, fraction).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "rate anchor network time exceeds the supported timeline",
            )
        })?;
        self.playback_anchor = Some(PlaybackAnchor {
            timeline_id,
            remote_time_ns,
            rtp_time,
            telemetry_token,
        });
        let reset_clock = self
            .ptp_clock
            .as_ref()
            .is_none_or(|clock| clock.grandmaster() != timeline_id);
        if reset_clock {
            self.ptp_clock = Some(PtpClockMapper::new(
                self.peer_ip,
                timeline_id,
                self.ptp_epoch,
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
            ));
            self.ptp_clock_logged = false;
        }

        let pending = std::mem::take(&mut self.pending_ptp_samples);
        for sample in pending {
            if sample.peer == self.peer_ip && sample.grandmaster == timeline_id {
                self.observe_ptp_sample_value(sample);
            } else {
                crate::telemetry::record_ptp_pending_disposition(
                    sample.telemetry_token,
                    "mapper_pending_sample_discarded",
                    1,
                );
            }
        }
        Ok(())
    }

    fn observe_ptp_sample(&mut self, sample: Result<PtpClockSample, broadcast::error::RecvError>) {
        let sample = match sample {
            Ok(sample) => sample,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                crate::telemetry::record_ptp_broadcast_lag(
                    crate::telemetry::operation_token(),
                    crate::telemetry::MediaKind::Buffered,
                    skipped,
                );
                return;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if sample.peer != self.peer_ip {
            return;
        }
        if self.ptp_clock.is_none() {
            if self.pending_ptp_samples.len() == MAX_PENDING_PTP_SAMPLES {
                if let Some(evicted) = self.pending_ptp_samples.pop_front() {
                    crate::telemetry::record_ptp_pending_disposition(
                        evicted.telemetry_token,
                        "mapper_pending_sample_evicted",
                        1,
                    );
                }
            }
            self.pending_ptp_samples.push_back(sample);
            return;
        }
        self.observe_ptp_sample_value(sample);
    }

    fn observe_ptp_sample_value(&mut self, sample: PtpClockSample) {
        let Some(clock) = self.ptp_clock.as_mut() else {
            return;
        };
        match clock.observe(sample) {
            Ok(Some(estimate)) => {
                if !self.ptp_clock_logged {
                    self.ptp_clock_logged = true;
                    log::info!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 PTP clock ready samples={} timeline={:016x}",
                        estimate.retained_samples,
                        sample.grandmaster
                    );
                }
            }
            Ok(None) => {}
            Err(error) => log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP clock sample rejected: {error}"
            ),
        }
    }

    fn pump_authenticated_blocks(&mut self) -> io::Result<()> {
        for _ in 0..MAX_BLOCKS_PER_PUMP {
            let Some(packet) = self.framer.next_packet().map_err(io::Error::from)? else {
                break;
            };
            let wire_bytes = packet.len() + LENGTH_PREFIX_BYTES;
            let telemetry_token = crate::telemetry::operation_token();
            crate::telemetry::record_media_received(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
            );
            if self.flush_pending_wire_bytes > 0 {
                self.flush_pending_wire_bytes =
                    self.flush_pending_wire_bytes.saturating_sub(wire_bytes);
                crate::telemetry::record_expected_control_disposition(
                    telemetry_token,
                    crate::telemetry::MediaKind::Buffered,
                    "control_flush_drop",
                    1,
                );
                continue;
            }
            let block = self.authenticate_block(&packet, wire_bytes, telemetry_token)?;
            if let Some(boundary) = self.flush_boundary {
                if sequence_precedes(block.header.sequence, boundary.sequence) {
                    crate::telemetry::record_expected_control_disposition(
                        block.telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                        "control_flush_drop",
                        1,
                    );
                    continue;
                }
                self.flush_boundary = None;
            }
            self.queued_wire_bytes = self.queued_wire_bytes.saturating_add(block.wire_bytes);
            crate::telemetry::record_media_admitted(
                block.telemetry_token,
                crate::telemetry::MediaKind::Buffered,
            );
            self.queued_blocks.push_back(block);
        }
        Ok(())
    }

    fn authenticate_block(
        &mut self,
        packet: &[u8],
        wire_bytes: usize,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> io::Result<AuthenticatedBlock> {
        let header = match parse_buffered_header(packet) {
            Ok(header) => header,
            Err(error) => {
                crate::telemetry::record_media_admission_rejection(
                    telemetry_token,
                    crate::telemetry::MediaKind::Buffered,
                    "header_parse",
                );
                return Err(io::Error::from(error));
            }
        };
        let decrypted = match self.decryptor.decrypt(packet, &header) {
            Ok(decrypted) => {
                crate::telemetry::record_media_decrypted(
                    telemetry_token,
                    crate::telemetry::MediaKind::Buffered,
                );
                decrypted
            }
            Err(error) => {
                crate::telemetry::record_media_decrypt_failure(
                    telemetry_token,
                    crate::telemetry::MediaKind::Buffered,
                );
                return Err(io::Error::from(error));
            }
        };
        // Only authenticated wire data is allowed to advance or terminate the
        // nonce state. An untrusted process sharing the sender IP must not be
        // able to kill playback merely by guessing an old counter value.
        if let Err(error) = self.nonce_history.commit(header.nonce_suffix) {
            let nonce_rejection_reason =
                if self.nonce_history.accepted >= self.nonce_history.capacity {
                    "session_limit"
                } else {
                    "nonce_replay"
                };
            crate::telemetry::record_media_admission_rejection(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                nonce_rejection_reason,
            );
            return Err(error);
        }

        if header.ssrc != BUFFERED_SSRC_AAC_44K1_STEREO
            && !(header.ssrc == 0 && decrypted.is_empty())
        {
            crate::telemetry::record_media_admission_rejection(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "ssrc",
            );
            return Err(PacketError::UnsupportedSsrc {
                actual: header.ssrc,
            }
            .into());
        }
        if let Some(previous) = self.last_sequence {
            let step = header.sequence.wrapping_sub(previous) & SEQUENCE_MASK;
            if step == 0 || step >= SEQUENCE_HALF_RANGE {
                // The transport sequence is a flush coordinate, not a replay
                // counter. Native senders can start a new sequence epoch after
                // a seek while keeping the authenticated TCP connection and
                // AEAD key alive. The strictly increasing nonce below remains
                // the security boundary; treating a sequence discontinuity as
                // fatal would reject that legitimate re-send epoch.
                log::debug!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 buffered sequence discontinuity previous={} current={}",
                    previous,
                    header.sequence
                );
            }
        }
        self.last_sequence = Some(header.sequence);
        if !self.authenticated_block_seen {
            self.authenticated_block_seen = true;
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 buffered media authentication confirmed"
            );
        }
        Ok(AuthenticatedBlock {
            header,
            payload: decrypted,
            wire_bytes,
            telemetry_token,
        })
    }

    fn prepare_ready_frame(&mut self) -> io::Result<()> {
        if !self.playing_is_current() {
            return Ok(());
        }

        loop {
            if let Some(ready) = self.ready_frame.as_ref() {
                let point = WirePoint {
                    sequence: ready.sequence,
                    timestamp: ready.timestamp,
                };
                let telemetry_token = ready.telemetry_token;
                if self.apply_deferred_flushes(point, telemetry_token) {
                    self.ready_frame = None;
                    continue;
                }
                break;
            }

            let Some(block) = self.queued_blocks.pop_front() else {
                break;
            };
            self.queued_wire_bytes = self.queued_wire_bytes.saturating_sub(block.wire_bytes);
            let point = WirePoint {
                sequence: block.header.sequence,
                timestamp: block.header.timestamp,
            };
            if self.apply_deferred_flushes(point, block.telemetry_token) {
                continue;
            }
            if block.payload.is_empty() {
                continue;
            }
            if self.timestamp_is_late(block.header.timestamp, block.telemetry_token) {
                crate::telemetry::record_predecode_late_drop(
                    block.telemetry_token,
                    crate::telemetry::MediaKind::Buffered,
                );
                if self.late_run_blocks == 0 {
                    self.late_run_first = Some((block.header.sequence, block.header.timestamp));
                }
                self.late_run_blocks = self.late_run_blocks.saturating_add(1);
                continue;
            }
            let pcm = match self.decoder.decode(block.header.timestamp, &block.payload) {
                Ok(pcm) => {
                    crate::telemetry::record_media_decoded(
                        block.telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                    );
                    pcm
                }
                Err(error) => {
                    crate::telemetry::record_media_decode_failure(
                        block.telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid buffered AAC-LC frame: {error}"),
                    ));
                }
            };
            let sequence = block.header.sequence;
            let timestamp = block.header.timestamp;
            let wire_bytes = block.wire_bytes;
            let telemetry_token = block.telemetry_token;
            self.ready_frame = Some(ReadyFrame {
                sequence,
                timestamp,
                pcm,
                wire_bytes,
                telemetry_token,
                deadline_not_ready_since: None,
                deadline_warmup_reported: false,
                deadline_failure_reasons: 0,
            });
        }
        let Some(ready) = self.ready_frame.as_ref() else {
            return Ok(());
        };
        if !self.decoded_block_seen {
            self.decoded_block_seen = true;
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 buffered AAC decode confirmed samples_per_channel={}",
                ready.pcm.len() / BUFFERED_CHANNELS
            );
        }
        Ok(())
    }

    /// Wall-clock deadline for an RTP timestamp, or `None` when the clock
    /// cannot answer right now.
    ///
    /// `None` and "late" are deliberately different answers: an unusable clock
    /// is a reason to hold a frame, never a reason to throw it away. The
    /// portable mapper reports `Stale` once its samples are older than
    /// `MAPPER_WINDOW` (2 s), and everything it was holding is data we already
    /// have in hand.
    fn deadline_for(
        &mut self,
        timestamp: u32,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> Option<std::time::Instant> {
        let Some(anchor) = self.playback_anchor else {
            crate::telemetry::record_scheduler_wait(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "missing_anchor",
            );
            return None;
        };
        let Some(clock) = self.ptp_clock.as_ref() else {
            crate::telemetry::record_scheduler_wait(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "missing_mapper",
            );
            return None;
        };
        let now = std::time::Instant::now();
        let deadline = match self.ptp_source.platform_deadline_for_rtp(
            anchor,
            timestamp,
            BUFFERED_SAMPLE_RATE as u32,
            telemetry_token,
        ) {
            Some(result) => result,
            None => clock.deadline_for_rtp(
                now,
                anchor,
                timestamp,
                BUFFERED_SAMPLE_RATE as u32,
                telemetry_token,
            ),
        };
        match deadline {
            Ok(deadline) => Some(deadline.checked_sub(PCM_OUTPUT_LEAD).unwrap_or(now)),
            Err(error) => {
                // `ready_schedule` owns per-frame deadline telemetry. This
                // helper is also used by predecode and periodic lead probes;
                // recording here would multiply one disposition by poll rate.
                if error != PtpClockError::NotReady {
                    self.note_clock_unavailable(&error);
                }
                None
            }
        }
    }

    /// Whether this block is already past the drop tolerance, decided **before**
    /// it is decoded.
    ///
    /// Discarding in timestamp space turns a multi-second catch-up from one AAC
    /// decode per 23 ms of skipped audio into pointer arithmetic. The block has
    /// already passed `apply_deferred_flushes` and the nonce ledger by the time
    /// this runs, so nothing security-relevant is skipped with it.
    fn timestamp_is_late(
        &mut self,
        timestamp: u32,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> bool {
        let now = std::time::Instant::now();
        match self.deadline_for(timestamp, telemetry_token) {
            Some(deadline) => now.saturating_duration_since(deadline) > MAX_CLOCKED_LATENESS,
            None => false,
        }
    }

    /// One edge per outage, at INFO, so a capture can tell a local clock stall
    /// from a real network gap without guessing from the drop count.
    fn note_clock_unavailable(&mut self, error: &PtpClockError) {
        if self.clock_unavailable_since.is_none() {
            self.clock_unavailable_since = Some(std::time::Instant::now());
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 media clock unavailable: {error}"
            );
        }
    }

    fn note_clock_available(&mut self) {
        let Some(since) = self.clock_unavailable_since.take() else {
            return;
        };
        self.last_clock_outage_ms = since.elapsed().as_millis() as u64;
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 media clock recovered outage_ms={} queued_blocks={} retained_bytes={}",
            self.last_clock_outage_ms,
            self.queued_blocks.len(),
            self.retained_wire_bytes()
        );
    }

    fn ready_schedule(&mut self) -> ReadySchedule {
        let Some((ready_timestamp, telemetry_token)) = self
            .ready_frame
            .as_ref()
            .map(|ready| (ready.timestamp, ready.telemetry_token))
        else {
            return ReadySchedule::Wait;
        };
        let Some(anchor) = self.playback_anchor else {
            crate::telemetry::record_scheduler_wait(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "missing_anchor",
            );
            return ReadySchedule::Wait;
        };
        let Some(clock) = self.ptp_clock.as_ref() else {
            crate::telemetry::record_scheduler_wait(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "missing_mapper",
            );
            return ReadySchedule::Wait;
        };
        let now = std::time::Instant::now();
        let (deadline_source, deadline) = match self.ptp_source.platform_deadline_for_rtp(
            anchor,
            ready_timestamp,
            BUFFERED_SAMPLE_RATE as u32,
            telemetry_token,
        ) {
            Some(result) => (crate::telemetry::DeadlineSource::Platform, result),
            None => (
                crate::telemetry::DeadlineSource::PortableMapper,
                clock.deadline_for_rtp(
                    now,
                    anchor,
                    ready_timestamp,
                    BUFFERED_SAMPLE_RATE as u32,
                    telemetry_token,
                ),
            ),
        };
        match deadline {
            Ok(deadline) => {
                self.note_clock_available();
                let deadline = deadline.checked_sub(PCM_OUTPUT_LEAD).unwrap_or(now);
                if deadline > now + MAX_CLOCKED_FUTURE {
                    crate::telemetry::record_future_safety_wait(
                        telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                    );
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 PTP media deadline exceeded the 30 second safety limit"
                    );
                    ReadySchedule::Wait
                } else if now.saturating_duration_since(deadline) <= MAX_CLOCKED_LATENESS {
                    ReadySchedule::DeliverAt(Instant::from_std(deadline))
                } else {
                    ReadySchedule::DropLate
                }
            }
            Err(error) => {
                if error == PtpClockError::NotReady {
                    let ready = self
                        .ready_frame
                        .as_mut()
                        .expect("ready frame remains installed while scheduling");
                    let since = *ready.deadline_not_ready_since.get_or_insert(now);
                    if !ready.deadline_warmup_reported {
                        ready.deadline_warmup_reported = true;
                        crate::telemetry::record_deadline_warmup_wait(
                            telemetry_token,
                            crate::telemetry::MediaKind::Buffered,
                            deadline_source,
                        );
                    }
                    if now.saturating_duration_since(since) < super::ptp::DEADLINE_NOT_READY_WARMUP
                    {
                        return ReadySchedule::Wait;
                    }
                }
                let bit = error.telemetry_bit();
                let first_for_frame = {
                    let ready = self
                        .ready_frame
                        .as_mut()
                        .expect("ready frame remains installed while scheduling");
                    let first = ready.deadline_failure_reasons & bit == 0;
                    ready.deadline_failure_reasons |= bit;
                    first
                };
                if first_for_frame {
                    crate::telemetry::record_deadline_rejection(
                        telemetry_token,
                        crate::telemetry::MediaKind::Buffered,
                        deadline_source,
                        error.telemetry_reason(),
                    );
                }
                self.note_clock_unavailable(&error);
                ReadySchedule::Wait
            }
        }
    }

    fn drop_late_ready_frame(&mut self) {
        let Some(ready) = self.ready_frame.take() else {
            return;
        };
        crate::telemetry::record_scheduler_late_drop(
            ready.telemetry_token,
            crate::telemetry::MediaKind::Buffered,
        );
        if self.late_run_blocks == 0 {
            self.late_run_first = Some((ready.sequence, ready.timestamp));
        }
        self.late_run_blocks = self.late_run_blocks.saturating_add(1);
        // Deliberately no `reset_decoded_output()` here. The discontinuity is
        // real and the decoder must not bridge across it, but that is one reset
        // for the whole run — settled in `deliver_ready_frame` — not one per
        // discarded frame. Per-frame it also flushed the PCM bus, and that
        // throws away audio already staged for the speaker and re-arms the
        // reader prebuffer, so the repair cost far more than the audio it
        // dropped.
    }

    fn deliver_ready_frame(&mut self) {
        if !self.playing_is_current() {
            return;
        }
        if let Some(ready) = self.ready_frame.take() {
            if self.late_run_blocks > 0 {
                let dropped = std::mem::take(&mut self.late_run_blocks);
                let (from_sequence, from_timestamp) = self
                    .late_run_first
                    .take()
                    .unwrap_or((ready.sequence, ready.timestamp));
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 discarded a late buffered run blocks={} approx_ms={}                      from_sequence={} from_timestamp={} resume_sequence={}                      resume_timestamp={} clock_outage_ms={}",
                    dropped,
                    dropped.saturating_mul(1_000 * SAMPLES_PER_AAC_FRAME) / BUFFERED_SAMPLE_RATE,
                    from_sequence,
                    from_timestamp,
                    ready.sequence,
                    ready.timestamp,
                    self.last_clock_outage_ms
                );
                // One discontinuity, one decoder reset. The output bus keeps
                // what it already holds: flushing it here is what turned a skip
                // into a collapsed buffer.
                self.decoder.reset();
            }
            crate::telemetry::record_media_output(
                ready.telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                ready.pcm.len(),
            );
            self.output.write(&ready.pcm, ready.telemetry_token);
        }
    }

    fn apply_deferred_flushes(
        &mut self,
        point: WirePoint,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> bool {
        let mut drop_point = false;
        let mut reset_output = false;
        let mut index = 0;

        while index < self.deferred_flushes.len() {
            let request = self.deferred_flushes[index];
            let span = sequence_distance(request.from.sequence, request.until.sequence);
            debug_assert!(span > 0 && span < SEQUENCE_HALF_RANGE);
            let from_to_point = sequence_distance(request.from.sequence, point.sequence);
            if from_to_point < span {
                drop_point = true;
                if !request.activated {
                    self.deferred_flushes[index].activated = true;
                    reset_output = true;
                }
                index += 1;
            } else if from_to_point < SEQUENCE_HALF_RANGE {
                // The stream reached (or passed) the exclusive end of this
                // range. Reset even when no packet landed inside the range so
                // decoder history cannot bridge across a sender-side seek.
                reset_output |= !request.activated;
                self.deferred_flushes.remove(index);
            } else {
                // This point still precedes the deferred range in 24-bit wire
                // order; keep the request for a later block.
                index += 1;
            }
        }

        if reset_output {
            self.reset_decoded_output();
        }
        if drop_point {
            crate::telemetry::record_expected_control_disposition(
                telemetry_token,
                crate::telemetry::MediaKind::Buffered,
                "deferred_flush_drop",
                1,
            );
        }
        drop_point
    }

    fn discard_before(&mut self, boundary: WirePoint) -> (bool, usize) {
        let queued_before = self.queued_blocks.len();
        let ready_before = usize::from(self.ready_frame.is_some());
        if self
            .ready_frame
            .as_ref()
            .is_some_and(|ready| sequence_precedes(ready.sequence, boundary.sequence))
        {
            self.ready_frame = None;
        }
        self.queued_blocks
            .retain(|block| !sequence_precedes(block.header.sequence, boundary.sequence));
        self.queued_wire_bytes = self
            .queued_blocks
            .iter()
            .map(|block| block.wire_bytes)
            .sum();
        let boundary_reached =
            self.ready_frame
                .as_ref()
                .is_some_and(|ready| !sequence_precedes(ready.sequence, boundary.sequence))
                || self.queued_blocks.front().is_some_and(|block| {
                    !sequence_precedes(block.header.sequence, boundary.sequence)
                });
        let dropped = queued_before
            .saturating_sub(self.queued_blocks.len())
            .saturating_add(ready_before.saturating_sub(usize::from(self.ready_frame.is_some())));
        (boundary_reached, dropped)
    }

    fn retained_wire_bytes(&self) -> usize {
        self.framer
            .pending_len()
            .saturating_add(self.queued_wire_bytes)
            .saturating_add(
                self.ready_frame
                    .as_ref()
                    .map_or(0, |ready| ready.wire_bytes),
            )
    }

    fn playing_is_current(&self) -> bool {
        self.playing
    }
}

enum AcceptResult {
    Connected(TcpStream),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FrameError {
    DeclaredLengthTooSmall { actual: usize, minimum: usize },
    PendingBufferTooLarge { actual: usize, maximum: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeclaredLengthTooSmall { actual, minimum } => write!(
                f,
                "buffered block declares {actual} bytes; minimum is {minimum}"
            ),
            Self::PendingBufferTooLarge { actual, maximum } => write!(
                f,
                "buffered TCP parser retained {actual} bytes; limit is {maximum}"
            ),
        }
    }
}

impl Error for FrameError {}

impl From<FrameError> for io::Error {
    fn from(error: FrameError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// Whether a half-read frame means the sender is gone rather than merely idle.
///
/// Every term is load-bearing, and the version of this check that shipped had
/// only the last two:
///
/// * `playing` — a paused session receives nothing by design. Without this a
///   long pause is indistinguishable from a dead peer.
/// * `queue_empty` / `ready_empty` — the difference between "idle" and "dead".
///   A buffered sender that has filled us up and gone quiet is doing its job;
///   only once we have played everything and still have nothing is its silence
///   a failure. This is the term whose absence cost two live sessions.
/// * `read_limit` — a full 8 MiB queue deliberately stops TCP reads, and that
///   local backpressure must not read as a sender timeout. Kept, though with
///   `queue_empty` it can no longer be the only guard: they cannot both be true
///   unless something is very wrong.
/// * `framer_timed_out` — nothing has arrived for `STARVED_STALL_TIMEOUT` and
///   what we do hold is less than one whole frame.
fn starved_stall(
    playing: bool,
    queue_empty: bool,
    ready_empty: bool,
    read_limit: usize,
    framer_timed_out: bool,
) -> bool {
    playing && queue_empty && ready_empty && read_limit > 0 && framer_timed_out
}

struct TcpBlockFramer {
    pending: Vec<u8>,
    last_progress: Option<Instant>,
}

impl TcpBlockFramer {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(TCP_READ_CHUNK_BYTES * 2),
            last_progress: None,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        let new_length = self.pending.len().saturating_add(bytes.len());
        if new_length > MAX_PENDING_TCP_BYTES {
            return Err(FrameError::PendingBufferTooLarge {
                actual: new_length,
                maximum: MAX_PENDING_TCP_BYTES,
            });
        }
        self.pending.extend_from_slice(bytes);
        self.last_progress = Some(Instant::now());
        Ok(())
    }

    fn next_packet(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.pending.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }
        let declared = u16::from_be_bytes([self.pending[0], self.pending[1]]) as usize;
        if declared < MIN_WIRE_BLOCK_BYTES {
            return Err(FrameError::DeclaredLengthTooSmall {
                actual: declared,
                minimum: MIN_WIRE_BLOCK_BYTES,
            });
        }
        if self.pending.len() < declared {
            return Ok(None);
        }
        let packet = self.pending[LENGTH_PREFIX_BYTES..declared].to_vec();
        self.pending.drain(..declared);
        if self.pending.is_empty() {
            self.last_progress = None;
        }
        Ok(Some(packet))
    }

    fn partial_frame_timed_out(&self, now: Instant) -> bool {
        if !self.has_partial_frame() {
            return false;
        }
        self.last_progress
            .is_some_and(|last| now.saturating_duration_since(last) >= STARVED_STALL_TIMEOUT)
    }

    fn has_partial_frame(&self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        if self.pending.len() < LENGTH_PREFIX_BYTES {
            return true;
        }
        let declared = u16::from_be_bytes([self.pending[0], self.pending[1]]) as usize;
        self.pending.len() < declared
    }

    fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferedHeader {
    sequence: u32,
    timestamp: u32,
    ssrc: u32,
    nonce_suffix: [u8; NONCE_SUFFIX_BYTES],
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PacketError {
    TooShort { actual: usize, minimum: usize },
    UnsupportedSsrc { actual: u32 },
    AuthenticationFailed,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { actual, minimum } => {
                write!(f, "buffered packet is {actual} bytes; minimum is {minimum}")
            }
            Self::UnsupportedSsrc { actual } => {
                write!(f, "buffered packet has unsupported SSRC 0x{actual:08x}")
            }
            Self::AuthenticationFailed => f.write_str("buffered packet authentication failed"),
        }
    }
}

impl Error for PacketError {}

impl From<PacketError> for io::Error {
    fn from(error: PacketError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

fn parse_buffered_header(packet: &[u8]) -> Result<BufferedHeader, PacketError> {
    if packet.len() < MIN_PACKET_BYTES {
        return Err(PacketError::TooShort {
            actual: packet.len(),
            minimum: MIN_PACKET_BYTES,
        });
    }
    let sequence_word = u32::from_be_bytes(packet[0..4].try_into().expect("length checked"));
    let timestamp = u32::from_be_bytes(packet[4..8].try_into().expect("length checked"));
    let ssrc = u32::from_be_bytes(packet[8..12].try_into().expect("length checked"));
    let mut nonce_suffix = [0u8; NONCE_SUFFIX_BYTES];
    nonce_suffix.copy_from_slice(&packet[packet.len() - NONCE_SUFFIX_BYTES..]);
    Ok(BufferedHeader {
        sequence: sequence_word & SEQUENCE_MASK,
        timestamp,
        ssrc,
        nonce_suffix,
    })
}

struct BufferedDecryptor {
    key: [u8; SHARED_KEY_BYTES],
}

impl BufferedDecryptor {
    fn new(key: [u8; SHARED_KEY_BYTES]) -> Self {
        Self { key }
    }

    fn decrypt(&self, packet: &[u8], header: &BufferedHeader) -> Result<Vec<u8>, PacketError> {
        let nonce_start = packet.len() - NONCE_SUFFIX_BYTES;
        let encrypted = &packet[HEADER_BYTES..nonce_start];
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&header.nonce_suffix);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: encrypted,
                    aad: &packet[4..HEADER_BYTES],
                },
            )
            .map_err(|_| PacketError::AuthenticationFailed)
    }
}

impl Drop for BufferedDecryptor {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

/// Heap-free replay protection for the counter nonce used by buffered audio.
///
/// Apple senders serialize a monotonically increasing `u64` counter in little
/// endian form. TCP preserves block order, so retaining the largest accepted
/// counter rejects every replay (including a replay with its unauthenticated
/// 24-bit sequence word altered) without a session-sized set.
struct SessionNonceCounter {
    capacity: usize,
    accepted: usize,
    highest: Option<u64>,
}

impl SessionNonceCounter {
    fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            capacity,
            accepted: 0,
            highest: None,
        }
    }

    fn validate(&self, nonce: [u8; NONCE_SUFFIX_BYTES]) -> io::Result<()> {
        if self.accepted >= self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "buffered session authenticated-block limit reached",
            ));
        }
        let counter = u64::from_le_bytes(nonce);
        if self.highest.is_some_and(|highest| counter <= highest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "buffered packet nonce repeated or moved backwards",
            ));
        }
        Ok(())
    }

    fn commit(&mut self, nonce: [u8; NONCE_SUFFIX_BYTES]) -> io::Result<()> {
        self.validate(nonce)?;
        self.highest = Some(u64::from_le_bytes(nonce));
        self.accepted += 1;
        Ok(())
    }
}

#[derive(Debug)]
enum AacDecodeError {
    Decoder(String),
    WrongSampleRate { actual: u32 },
    WrongChannelCount { actual: usize },
    WrongFrameCount { actual: usize },
}

impl fmt::Display for AacDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decoder(error) => write!(f, "AAC decoder error: {error}"),
            Self::WrongSampleRate { actual } => {
                write!(f, "AAC decoder returned {actual} Hz")
            }
            Self::WrongChannelCount { actual } => {
                write!(f, "AAC decoder returned {actual} channels")
            }
            Self::WrongFrameCount { actual } => {
                write!(f, "AAC decoder returned {actual} frames")
            }
        }
    }
}

impl Error for AacDecodeError {}

struct AacLcDecoder {
    decoder: Box<dyn Decoder>,
}

impl AacLcDecoder {
    fn new() -> Result<Self, AacDecodeError> {
        // MPEG-4 AudioSpecificConfig: AAC-LC (AOT 2), sampling-frequency
        // index 4 (44.1 kHz), channel configuration 2 (stereo).
        let asc = vec![0x12, 0x10].into_boxed_slice();
        let channels = Channels::FRONT_LEFT | Channels::FRONT_RIGHT;
        let mut parameters = CodecParameters::new();
        parameters
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(BUFFERED_SAMPLE_RATE as u32)
            .with_channels(channels)
            .with_max_frames_per_packet(SAMPLES_PER_AAC_FRAME)
            .with_extra_data(asc);
        let decoder = symphonia::default::get_codecs()
            .make(&parameters, &DecoderOptions::default())
            .map_err(|error| AacDecodeError::Decoder(error.to_string()))?;
        Ok(Self { decoder })
    }

    fn decode(&mut self, timestamp: u32, frame: &[u8]) -> Result<Vec<i16>, AacDecodeError> {
        let packet = Packet::new_from_slice(0, u64::from(timestamp), SAMPLES_PER_AAC_FRAME, frame);
        let decoded = self
            .decoder
            .decode(&packet)
            .map_err(|error| AacDecodeError::Decoder(error.to_string()))?;
        let spec = *decoded.spec();
        if spec.rate != BUFFERED_SAMPLE_RATE as u32 {
            return Err(AacDecodeError::WrongSampleRate { actual: spec.rate });
        }
        if spec.channels.count() != BUFFERED_CHANNELS {
            return Err(AacDecodeError::WrongChannelCount {
                actual: spec.channels.count(),
            });
        }
        if decoded.frames() != SAMPLES_PER_AAC_FRAME as usize {
            return Err(AacDecodeError::WrongFrameCount {
                actual: decoded.frames(),
            });
        }
        let mut pcm = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
        pcm.copy_interleaved_ref(decoded);
        Ok(pcm.samples().to_vec())
    }

    fn reset(&mut self) {
        self.decoder.reset();
    }
}

fn sequence_precedes(sequence: u32, boundary: u32) -> bool {
    let backwards = sequence.wrapping_sub(boundary) & SEQUENCE_MASK;
    backwards != 0 && backwards >= SEQUENCE_HALF_RANGE
}

fn sequence_distance(from: u32, to: u32) -> u32 {
    to.wrapping_sub(from) & SEQUENCE_MASK
}

fn validate_media_endpoints(local: SocketAddr, peer: SocketAddr) -> io::Result<()> {
    if local.is_ipv4() != peer.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local and peer buffered-media addresses use different IP families",
        ));
    }
    if local.ip().is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffered-media listener requires the control connection's concrete local address",
        ));
    }
    if peer.ip().is_unspecified() || peer.ip().is_multicast() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffered-media peer must be a concrete unicast address",
        ));
    }
    Ok(())
}

fn with_port(address: SocketAddr, port: u16) -> SocketAddr {
    match address {
        SocketAddr::V4(address) => SocketAddr::V4(SocketAddrV4::new(*address.ip(), port)),
        SocketAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(
            *address.ip(),
            port,
            address.flowinfo(),
            address.scope_id(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::AeadInPlace;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    const TEST_KEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    // Created for AudioHub with ffmpeg 7.1.1 from a 997 Hz, 44.1 kHz stereo
    // sine.  This is one raw AAC-LC access unit after removing its ADTS header.
    const OWNED_AAC_FRAME_HEX: &str = concat!(
        "de02004c61766336312e31392e3130310042549ffffff80005133127a12368483a",
        "114aa6f7ceb5e6f4bb92e492488e691138033b3b3b3b3b3b3b3b3a1214fd9c0",
        "c4e828d9b366c181914a0c6cd830322448a5366e50606452831b372832296596",
        "5965965965965965965965965965965965965965965965965965965965965965",
        "9659658a28a28a28a28a28a28a28a28a28a28a28a28a28a28a28a28a28a2",
        "8a28a28a28a28a28a28a28a28a28a2a21cf9b43e6d0f87422954def9d6bcde",
        "97725c",
        "9007301c000000000000000e0"
    );

    fn valid_stream() -> Dictionary {
        [
            ("type", Value::from(STREAM_TYPE_BUFFERED_AUDIO)),
            ("audioFormat", Value::from(AUDIO_FORMAT_AAC_LC_44K1_STEREO)),
            ("ct", Value::from(COMPRESSION_TYPE_AAC_LC)),
            ("spf", Value::from(SAMPLES_PER_AAC_FRAME)),
            ("sr", Value::from(BUFFERED_SAMPLE_RATE)),
            ("shk", Value::Data(TEST_KEY.to_vec())),
        ]
        .into_iter()
        .collect()
    }

    fn setup_body(stream: Dictionary) -> Vec<u8> {
        let root: Dictionary = [("streams", Value::Array(vec![Value::Dictionary(stream)]))]
            .into_iter()
            .collect();
        let mut bytes = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut bytes)
            .unwrap();
        bytes
    }

    fn valid_setup() -> Type103Setup {
        parse_phase2(&setup_body(valid_stream())).unwrap()
    }

    #[test]
    fn parses_only_the_owned_buffered_profile_and_redacts_key() {
        let mut stream = valid_stream();
        stream.insert("controlPort".into(), Value::from(65_535u64));
        stream.insert("audioMode".into(), Value::String("default".into()));
        let setup = parse_phase2(&setup_body(stream)).unwrap();
        assert_eq!(setup.shared_key(), &TEST_KEY);
        assert_eq!(setup.remote_control_port, Some(65_535));
        let debug = format!("{setup:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("0, 1, 2"));
    }

    #[test]
    fn accepts_omitted_redundant_codec_fields() {
        let mut stream = valid_stream();
        stream.remove("ct");
        stream.remove("sr");
        assert!(parse_phase2(&setup_body(stream)).is_ok());
    }

    #[test]
    fn rejects_every_unsupported_codec_dimension() {
        for (field, actual) in [
            ("type", 96),
            ("audioFormat", 0x0004_0000),
            ("ct", 2),
            ("spf", 352),
            ("sr", 48_000),
        ] {
            let mut stream = valid_stream();
            stream.insert(field.into(), Value::from(actual));
            assert_eq!(
                parse_phase2(&setup_body(stream)).unwrap_err(),
                BufferedSetupError::UnsupportedValue { field, actual }
            );
        }
    }

    #[test]
    fn rejects_missing_mistyped_and_invalid_transport_fields() {
        for field in ["type", "audioFormat", "spf", "shk"] {
            let mut stream = valid_stream();
            stream.remove(field);
            assert_eq!(
                parse_phase2(&setup_body(stream)).unwrap_err(),
                BufferedSetupError::MissingField(field)
            );
        }

        let mut stream = valid_stream();
        stream.insert("spf".into(), Value::String("1024".into()));
        assert_eq!(
            parse_phase2(&setup_body(stream)).unwrap_err(),
            BufferedSetupError::WrongFieldType("spf")
        );

        for value in [0u64, 65_536] {
            let mut stream = valid_stream();
            stream.insert("controlPort".into(), Value::from(value));
            assert_eq!(
                parse_phase2(&setup_body(stream)).unwrap_err(),
                BufferedSetupError::UnsupportedValue {
                    field: "controlPort",
                    actual: value,
                }
            );
        }
    }

    #[test]
    fn setup_parser_bounds_and_shape_checks_untrusted_input() {
        assert_eq!(
            parse_phase2(&[]).unwrap_err(),
            BufferedSetupError::EmptyBody
        );
        assert_eq!(
            parse_phase2(b"not a plist").unwrap_err(),
            BufferedSetupError::NotBinaryPlist
        );
        let oversized = vec![0u8; MAX_SETUP_BODY_BYTES + 1];
        assert_eq!(
            parse_phase2(&oversized).unwrap_err(),
            BufferedSetupError::BodyTooLarge {
                actual: MAX_SETUP_BODY_BYTES + 1,
                maximum: MAX_SETUP_BODY_BYTES,
            }
        );

        let mut scalar = Vec::new();
        Value::String("x".into())
            .to_writer_binary(&mut scalar)
            .unwrap();
        assert_eq!(
            parse_phase2(&scalar).unwrap_err(),
            BufferedSetupError::RootNotDictionary
        );

        let root: Dictionary = [("streams", Value::Array(Vec::new()))]
            .into_iter()
            .collect();
        let mut no_streams = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut no_streams)
            .unwrap();
        assert_eq!(
            parse_phase2(&no_streams).unwrap_err(),
            BufferedSetupError::StreamCount { actual: 0 }
        );
    }

    #[test]
    fn extracted_key_guard_runs_on_success_and_late_validation_failure() {
        let before = SECRET_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
        drop(parse_phase2(&setup_body(valid_stream())).unwrap());

        let mut invalid = valid_stream();
        invalid.insert("spf".into(), Value::from(1u64));
        assert!(parse_phase2(&setup_body(invalid)).is_err());
        let after = SECRET_DROP_COUNT.load(std::sync::atomic::Ordering::SeqCst);
        // Other parser cases execute in parallel and may also advance this
        // diagnostic counter. These two paths must contribute at least two.
        assert!(after - before >= 2);
    }

    #[test]
    fn decrypts_independent_fixed_node_crypto_vector() {
        // Generated once with Node's OpenSSL-backed chacha20-poly1305 using
        // TEST_KEY, nonce 00000000a0..a7, and AAD 1020304016000000.
        let packet = hex::decode(concat!(
            "800123451020304016000000",
            "fe9a6a5f56cf6dd5fbf7d27ae82f1ad5d751ecb28b263271",
            "cea1a62cc69fcfbbf26bc64e5d1ccfd3",
            "a0a1a2a3a4a5a6a7"
        ))
        .unwrap();
        let header = parse_buffered_header(&packet).unwrap();
        assert_eq!(header.sequence, 0x012345);
        assert_eq!(header.timestamp, 0x1020_3040);
        assert_eq!(
            header.nonce_suffix,
            [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7]
        );
        let plaintext = BufferedDecryptor::new(TEST_KEY)
            .decrypt(&packet, &header)
            .unwrap();
        assert_eq!(plaintext, b"AudioHub-owned-AAC-frame");
    }

    #[test]
    fn authentication_covers_ciphertext_header_and_nonce() {
        let original = make_packet(7, 42_000, OWNED_AAC_FRAME_HEX, [9; 8]);
        for index in [4usize, HEADER_BYTES, original.len() - NONCE_SUFFIX_BYTES] {
            let mut tampered = original.clone();
            tampered[index] ^= 0x40;
            let header = parse_buffered_header(&tampered).unwrap();
            assert_eq!(
                BufferedDecryptor::new(TEST_KEY).decrypt(&tampered, &header),
                Err(PacketError::AuthenticationFailed)
            );
        }
        let header = parse_buffered_header(&original).unwrap();
        assert_eq!(
            BufferedDecryptor::new([0xff; 32]).decrypt(&original, &header),
            Err(PacketError::AuthenticationFailed)
        );
    }

    #[test]
    fn header_preserves_ssrc_and_masks_to_24_bit_sequence() {
        let packet = make_packet(0x00ff_ffff, 9, OWNED_AAC_FRAME_HEX, [3; 8]);
        assert_eq!(
            parse_buffered_header(&packet).unwrap().sequence,
            SEQUENCE_MASK
        );

        let mut wrong_ssrc = packet.clone();
        wrong_ssrc[8..12].copy_from_slice(&0x1700_0000u32.to_be_bytes());
        assert_eq!(
            parse_buffered_header(&wrong_ssrc).unwrap().ssrc,
            0x1700_0000
        );
        assert_eq!(
            parse_buffered_header(&[0u8; MIN_PACKET_BYTES - 1]).unwrap_err(),
            PacketError::TooShort {
                actual: MIN_PACKET_BYTES - 1,
                minimum: MIN_PACKET_BYTES,
            }
        );
    }

    #[test]
    fn header_preserves_observed_native_sender_sequence_above_23_bits() {
        // A native AirPlay sender can use the full 24-bit sequence space. This
        // value is representative of sequences observed on the wire and would
        // be corrupted by the former 23-bit mask.
        let packet = make_packet(0x00ea_6e5a, 9, OWNED_AAC_FRAME_HEX, [4; 8]);
        assert_eq!(
            parse_buffered_header(&packet).unwrap().sequence,
            0x00ea_6e5a
        );
    }

    #[test]
    fn tcp_framer_uses_big_endian_length_including_prefix() {
        let packet_a = make_packet(1, 1_024, OWNED_AAC_FRAME_HEX, [1; 8]);
        let packet_b = make_packet(2, 2_048, OWNED_AAC_FRAME_HEX, [2; 8]);
        let block_a = framed(&packet_a);
        let block_b = framed(&packet_b);
        let mut framer = TcpBlockFramer::new();
        framer.push(&block_a[..1]).unwrap();
        assert!(framer.next_packet().unwrap().is_none());
        framer.push(&block_a[1..17]).unwrap();
        assert!(framer.next_packet().unwrap().is_none());
        let mut tail = block_a[17..].to_vec();
        tail.extend_from_slice(&block_b);
        framer.push(&tail).unwrap();
        assert_eq!(framer.next_packet().unwrap(), Some(packet_a));
        assert_eq!(framer.next_packet().unwrap(), Some(packet_b));
        assert!(framer.next_packet().unwrap().is_none());
        assert!(framer.is_empty());
    }

    #[test]
    fn tcp_framer_rejects_tiny_declarations_and_unbounded_pending_data() {
        let mut framer = TcpBlockFramer::new();
        framer.push(&2u16.to_be_bytes()).unwrap();
        assert_eq!(
            framer.next_packet().unwrap_err(),
            FrameError::DeclaredLengthTooSmall {
                actual: 2,
                minimum: MIN_WIRE_BLOCK_BYTES,
            }
        );

        let mut framer = TcpBlockFramer::new();
        let error = framer
            .push(&vec![0u8; MAX_PENDING_TCP_BYTES + 1])
            .unwrap_err();
        assert_eq!(
            error,
            FrameError::PendingBufferTooLarge {
                actual: MAX_PENDING_TCP_BYTES + 1,
                maximum: MAX_PENDING_TCP_BYTES,
            }
        );
    }

    #[test]
    fn sequence_boundary_comparison_handles_24_bit_wrap() {
        assert!(sequence_precedes(99, 100));
        assert!(!sequence_precedes(100, 100));
        assert!(!sequence_precedes(101, 100));
        assert!(sequence_precedes(SEQUENCE_MASK - 1, 2));
        assert!(!sequence_precedes(2, SEQUENCE_MASK - 1));
        assert_eq!(sequence_distance(SEQUENCE_MASK - 1, 2), 4);
    }

    #[test]
    fn session_nonce_counter_is_heap_free_monotonic_and_fail_closed() {
        assert!(!std::mem::needs_drop::<SessionNonceCounter>());
        assert!(std::mem::size_of::<SessionNonceCounter>() <= 4 * std::mem::size_of::<usize>());

        let mut history = SessionNonceCounter::new(3);
        history.commit(7u64.to_le_bytes()).unwrap();
        history.commit(9u64.to_le_bytes()).unwrap();
        assert!(history.commit(9u64.to_le_bytes()).is_err());
        assert!(history.commit(8u64.to_le_bytes()).is_err());
        history.commit(10u64.to_le_bytes()).unwrap();
        assert!(history.commit(11u64.to_le_bytes()).is_err());
        assert_eq!(history.highest, Some(10));
        assert_eq!(history.accepted, 3);
    }

    #[test]
    fn owned_raw_aac_vector_decodes_to_exact_stereo_frame() {
        let frame = hex::decode(OWNED_AAC_FRAME_HEX).unwrap();
        let pcm = AacLcDecoder::new().unwrap().decode(0, &frame).unwrap();
        assert_eq!(
            pcm.len(),
            SAMPLES_PER_AAC_FRAME as usize * BUFFERED_CHANNELS
        );
        assert!(pcm.iter().any(|sample| sample.unsigned_abs() > 100));
    }

    #[test]
    fn endpoint_validation_requires_matching_concrete_addresses() {
        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 1);
        assert_eq!(
            validate_media_endpoints(v4, v6).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let unspecified = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1);
        assert_eq!(
            validate_media_endpoints(unspecified, v4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[derive(Default)]
    struct OutputState {
        writes: Vec<Vec<i16>>,
        flushes: usize,
    }

    struct TestOutput(Arc<Mutex<OutputState>>);

    impl PcmOutput for TestOutput {
        fn write(&mut self, samples: &[i16], _telemetry_token: crate::telemetry::OperationToken) {
            self.0.lock().unwrap().writes.push(samples.to_vec());
        }

        fn flush(&mut self) {
            self.0.lock().unwrap().flushes += 1;
        }
    }

    async fn controlled_harness() -> (
        Type103Ports,
        Type103MediaHandle,
        Arc<Mutex<OutputState>>,
        PtpClockSource,
        IpAddr,
        std::time::Instant,
    ) {
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9_000);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9_001);
        let ptp = PtpClockSource::test_source();
        let prepared = PreparedType103::prepare(local, peer, valid_setup(), ptp.clone())
            .await
            .unwrap();
        let ports = prepared.ports();
        assert_ne!(ports.data_port, 0);
        assert_ne!(ports.control_port, 0);
        assert_eq!(ports.audio_buffer_size, ADVERTISED_AUDIO_BUFFER_BYTES);
        let state = Arc::new(Mutex::new(OutputState::default()));
        let handle = prepared.start(Box::new(TestOutput(Arc::clone(&state))));
        let base = std::time::Instant::now();
        (ports, handle, state, ptp, peer.ip(), base)
    }

    async fn publish_ptp_burst(ptp: &PtpClockSource, peer: IpAddr, base: std::time::Instant) {
        for _ in 0..3 {
            let received_at = std::time::Instant::now();
            let elapsed = u64::try_from(received_at.duration_since(base).as_nanos()).unwrap();
            ptp.publish_test_sample(PtpClockSample {
                peer,
                grandmaster: 1,
                remote_time_ns: 1_000_000_000 + elapsed,
                received_at,
                telemetry_token: crate::telemetry::operation_token(),
            });
            time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn harness() -> (Type103Ports, Type103MediaHandle, Arc<Mutex<OutputState>>) {
        let (ports, handle, state, ptp, peer, base) = controlled_harness().await;
        tokio::spawn(async move {
            for _ in 0..128 {
                let received_at = std::time::Instant::now();
                let elapsed = u64::try_from(received_at.duration_since(base).as_nanos()).unwrap();
                ptp.publish_test_sample(PtpClockSample {
                    peer,
                    grandmaster: 1,
                    remote_time_ns: 1_000_000_000 + elapsed,
                    received_at,
                    telemetry_token: crate::telemetry::operation_token(),
                });
                time::sleep(Duration::from_millis(5)).await;
            }
        });
        (ports, handle, state)
    }

    fn test_rate(playing: bool) -> BufferedRateAnchor {
        BufferedRateAnchor {
            playing,
            rtp_time: playing.then_some(0),
            network_time_secs: playing.then_some(1),
            network_time_frac: playing.then_some(0),
            timeline_id: playing.then_some(1),
        }
    }

    async fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition timed out");
            time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_pipeline_is_rate_gated_decrypts_decodes_and_paces() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        let first = make_packet(10, 0, OWNED_AAC_FRAME_HEX, [10; 8]);
        sender.write_all(&framed(&first)).await.unwrap();
        time::sleep(Duration::from_millis(40)).await;
        assert!(state.lock().unwrap().writes.is_empty());

        handle.set_rate(test_rate(true)).await.unwrap();
        let second = make_packet(11, 1_024, OWNED_AAC_FRAME_HEX, [11; 8]);
        sender.write_all(&framed(&second)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        let guard = state.lock().unwrap();
        assert_eq!(guard.writes[0].len(), 2_048);
        assert_eq!(guard.writes[1].len(), 2_048);
        drop(guard);
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn telemetry_reset_preserves_partial_tcp_frame_anchor_and_decoder() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        sender
            .write_all(&framed(&make_packet(1, 0, OWNED_AAC_FRAME_HEX, [1; 8])))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;

        let second = framed(&make_packet(2, 1_024, OWNED_AAC_FRAME_HEX, [2; 8]));
        let split = 17;
        sender.write_all(&second[..split]).await.unwrap();
        time::sleep(Duration::from_millis(20)).await;

        crate::telemetry::reset();

        sender.write_all(&second[split..]).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        let guard = state.lock().unwrap();
        assert_eq!(guard.flushes, 0);
        assert_eq!(guard.writes[1].len(), 2_048);
        drop(guard);
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_clock_recovery_drops_late_head_and_resumes_current_audio() {
        let (ports, handle, state, ptp, peer, base) = controlled_harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        publish_ptp_burst(&ptp, peer, base).await;

        sender
            .write_all(&framed(&make_packet(1, 0, OWNED_AAC_FRAME_HEX, [101; 8])))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;

        // Let all mapper samples expire, then queue one frame whose deadline
        // is on the old side of the outage. It must wait while the clock is
        // stale, and must not permanently occupy the ready slot once PTP is
        // healthy again.
        time::sleep(Duration::from_millis(2_100)).await;
        sender
            .write_all(&framed(&make_packet(
                2,
                1_024,
                OWNED_AAC_FRAME_HEX,
                [102; 8],
            )))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(40)).await;
        assert_eq!(state.lock().unwrap().writes.len(), 1);

        let elapsed_ns = std::time::Instant::now().duration_since(base).as_nanos();
        let current_rtp = u32::try_from(
            ((elapsed_ns + PCM_OUTPUT_LEAD.as_nanos()) * u128::from(BUFFERED_SAMPLE_RATE))
                / 1_000_000_000,
        )
        .unwrap();
        sender
            .write_all(&framed(&make_packet(
                3,
                current_rtp,
                OWNED_AAC_FRAME_HEX,
                [103; 8],
            )))
            .await
            .unwrap();
        publish_ptp_burst(&ptp, peer, base).await;

        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        let guard = state.lock().unwrap();
        assert_eq!(guard.writes.len(), 2);
        // The stale frame is still discarded — that decision is unchanged, and
        // every reference receiver makes it. What changed on 2026-08-16 is its
        // price: this used to assert `flushes >= 1`, because each discarded
        // frame called `reset_decoded_output()`, and the flush inside it clears
        // the whole PCM ring and un-primes every reader (`bus.rs`
        // `reset_locked`). Over a multi-second outage that ran once per 23 ms of
        // skipped audio, so the repair destroyed far more audio than the gap
        // did — audible as "the AirPlay buffer is gone" after any hiccup.
        //
        // The discontinuity is still handled: one `decoder.reset()` when the
        // run ends. The output keeps what it already holds.
        assert_eq!(
            guard.flushes, 0,
            "a late discard must not flush the output bus"
        );
        drop(guard);
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authenticated_empty_boundary_block_advances_without_decoding() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        let empty = make_packet_with_ssrc(1, 0, 0, "", [1; 8]);
        let audio = make_packet(2, 1_024, OWNED_AAC_FRAME_HEX, [2; 8]);
        sender.write_all(&framed(&empty)).await.unwrap();
        sender.write_all(&framed(&audio)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_pause_applies_tcp_backpressure_until_resumed() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        sender
            .write_all(&framed(&make_packet(20, 0, OWNED_AAC_FRAME_HEX, [20; 8])))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;

        handle.set_rate(test_rate(false)).await.unwrap();
        sender
            .write_all(&framed(&make_packet(
                21,
                1_024,
                OWNED_AAC_FRAME_HEX,
                [21; 8],
            )))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(50)).await;
        assert_eq!(state.lock().unwrap().writes.len(), 1);
        handle.set_rate(test_rate(true)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_playing_anchor_is_rejected_without_releasing_audio() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        sender
            .write_all(&framed(&make_packet(25, 0, OWNED_AAC_FRAME_HEX, [25; 8])))
            .await
            .unwrap();

        let invalid = BufferedRateAnchor {
            playing: true,
            rtp_time: Some(0),
            network_time_secs: Some(u64::MAX),
            network_time_frac: Some(0),
            timeline_id: Some(1),
        };
        assert_eq!(
            handle.set_rate(invalid).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        time::sleep(Duration::from_millis(50)).await;
        assert!(state.lock().unwrap().writes.is_empty());

        handle.set_rate(test_rate(true)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn redundant_play_command_during_pacing_does_not_drop_decoded_audio() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        let first = framed(&make_packet(30, 0, OWNED_AAC_FRAME_HEX, [30; 8]));
        let second = framed(&make_packet(31, 1_024, OWNED_AAC_FRAME_HEX, [31; 8]));
        let mut both = first;
        both.extend_from_slice(&second);
        sender.write_all(&both).await.unwrap();
        wait_for(|| !state.lock().unwrap().writes.is_empty()).await;

        // The second frame may already be queued into the 50 ms PCM lead, or
        // it may still be awaiting its PTP deadline. Repeating rate=1 is
        // idempotent in either case and may not discard it.
        handle.set_rate(test_rate(true)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flush_boundary_drops_old_blocks_and_releases_at_target() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        handle
            .flush(BufferedFlush::Immediate {
                until: WirePoint {
                    sequence: 102,
                    timestamp: 2 * 1_024,
                },
            })
            .await
            .unwrap();
        for (sequence, nonce) in [(100u32, 1u8), (101, 2), (102, 3), (103, 4)] {
            sender
                .write_all(&framed(&make_packet(
                    sequence,
                    (sequence - 100) * 1_024,
                    OWNED_AAC_FRAME_HEX,
                    [nonce; 8],
                )))
                .await
                .unwrap();
        }
        time::sleep(Duration::from_millis(40)).await;
        assert!(state.lock().unwrap().writes.is_empty());
        handle.set_rate(test_rate(true)).await.unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        assert!(state.lock().unwrap().flushes >= 1);
        assert_eq!(
            handle
                .flush(BufferedFlush::Immediate {
                    until: WirePoint {
                        sequence: SEQUENCE_MASK + 1,
                        timestamp: 0,
                    },
                })
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn immediate_flush_uses_sequence_when_control_timestamp_is_a_new_epoch() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        handle
            .flush(BufferedFlush::Immediate {
                until: WirePoint {
                    sequence: 402,
                    // A native sender's seek boundary belongs to its control
                    // timeline and can differ from the packet timestamp.
                    timestamp: 0xf123_4567,
                },
            })
            .await
            .unwrap();
        for (sequence, timestamp, nonce) in [
            (400u32, 0u32, 61u8),
            (401, 1_024, 62),
            (402, 2_048, 63),
            (403, 3_072, 64),
        ] {
            sender
                .write_all(&framed(&make_packet(
                    sequence,
                    timestamp,
                    OWNED_AAC_FRAME_HEX,
                    [nonce; 8],
                )))
                .await
                .unwrap();
        }
        handle.set_rate(test_rate(true)).await.unwrap();

        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn immediate_flush_allows_a_lower_sequence_re_send_epoch() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        sender
            .write_all(&framed(&make_packet(900, 0, OWNED_AAC_FRAME_HEX, [91; 8])))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;

        handle
            .flush(BufferedFlush::Immediate {
                until: WirePoint {
                    sequence: 100,
                    timestamp: 0xabcd_0000,
                },
            })
            .await
            .unwrap();
        sender
            .write_all(&framed(&make_packet(100, 0, OWNED_AAC_FRAME_HEX, [92; 8])))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();

        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn immediate_flush_filters_queued_and_ready_frames_by_sequence_only() {
        let (ports, handle, state, ptp, peer, base) = controlled_harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        // With no PTP samples, the first decoded frame waits in `ready_frame`
        // while the rest remain authenticated in `queued_blocks`.
        handle.set_rate(test_rate(true)).await.unwrap();
        for (sequence, timestamp, nonce) in [
            (500u32, 0u32, 71u8),
            (501, 1_024, 72),
            (502, 2_048, 73),
            (503, 3_072, 74),
        ] {
            sender
                .write_all(&framed(&make_packet(
                    sequence,
                    timestamp,
                    OWNED_AAC_FRAME_HEX,
                    [nonce; 8],
                )))
                .await
                .unwrap();
        }
        time::sleep(Duration::from_millis(40)).await;
        handle
            .flush(BufferedFlush::Immediate {
                until: WirePoint {
                    sequence: 502,
                    timestamp: 0x1234_5678,
                },
            })
            .await
            .unwrap();
        publish_ptp_burst(&ptp, peer, base).await;
        handle.set_rate(test_rate(true)).await.unwrap();

        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deferred_flush_drops_only_the_announced_half_open_range() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        handle
            .flush(BufferedFlush::Deferred {
                from: WirePoint {
                    sequence: 202,
                    timestamp: 2 * 1_024,
                },
                until: WirePoint {
                    sequence: 204,
                    timestamp: 4 * 1_024,
                },
            })
            .await
            .unwrap();

        for (sequence, nonce) in [
            (200u32, 31u8),
            (201, 32),
            (202, 33),
            (203, 34),
            (204, 35),
            (205, 36),
        ] {
            sender
                .write_all(&framed(&make_packet(
                    sequence,
                    (sequence - 200) * 1_024,
                    OWNED_AAC_FRAME_HEX,
                    [nonce; 8],
                )))
                .await
                .unwrap();
        }

        wait_for(|| state.lock().unwrap().writes.len() == 4).await;
        assert!(state.lock().unwrap().flushes >= 1);
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deferred_flush_uses_wrapping_sequence_not_control_timestamps() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        handle
            .flush(BufferedFlush::Deferred {
                from: WirePoint {
                    sequence: SEQUENCE_MASK - 1,
                    timestamp: 0xefff_0000,
                },
                until: WirePoint {
                    sequence: 1,
                    timestamp: 0x0100_0000,
                },
            })
            .await
            .unwrap();

        for (sequence, timestamp, nonce) in [
            (SEQUENCE_MASK - 2, 0u32, 81u8),
            (SEQUENCE_MASK - 1, 1_024, 82),
            (SEQUENCE_MASK, 2_048, 83),
            (0, 3_072, 84),
            (1, 4_096, 85),
            (2, 5_120, 86),
        ] {
            sender
                .write_all(&framed(&make_packet(
                    sequence,
                    timestamp,
                    OWNED_AAC_FRAME_HEX,
                    [nonce; 8],
                )))
                .await
                .unwrap();
        }

        wait_for(|| state.lock().unwrap().writes.len() == 3).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_flush_finishes_and_discards_a_pre_flush_partial_tcp_block() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        let stale = framed(&make_packet(300, 0, OWNED_AAC_FRAME_HEX, [41; 8]));
        let split = 17;
        sender.write_all(&stale[..split]).await.unwrap();
        time::sleep(Duration::from_millis(50)).await;

        handle.flush(BufferedFlush::All).await.unwrap();
        sender.write_all(&stale[split..]).await.unwrap();
        sender
            .write_all(&framed(&make_packet(
                1,
                1_024,
                OWNED_AAC_FRAME_HEX,
                [42; 8],
            )))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();

        wait_for(|| state.lock().unwrap().writes.len() == 1).await;
        time::sleep(Duration::from_millis(40)).await;
        assert_eq!(state.lock().unwrap().writes.len(), 1);
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unexpected_source_ip_never_reaches_the_decoder() {
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9_000);
        let expected = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 9_001);
        let ptp = PtpClockSource::test_source();
        let prepared = PreparedType103::prepare(local, expected, valid_setup(), ptp.clone())
            .await
            .unwrap();
        let ports = prepared.ports();
        let state = Arc::new(Mutex::new(OutputState::default()));
        let handle = prepared.start(Box::new(TestOutput(Arc::clone(&state))));
        handle.set_rate(test_rate(true)).await.unwrap();
        let mut wrong_peer = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        let packet = make_packet(1, 0, OWNED_AAC_FRAME_HEX, [1; 8]);
        let _ = wrong_peer.write_all(&framed(&packet)).await;
        time::sleep(Duration::from_millis(50)).await;
        assert!(state.lock().unwrap().writes.is_empty());
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authenticated_nonce_replay_stops_the_stream() {
        let (ports, handle, state) = harness().await;
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        handle.set_rate(test_rate(true)).await.unwrap();
        let packet = make_packet(1, 0, OWNED_AAC_FRAME_HEX, [77; 8]);
        let mut wire = framed(&packet);
        wire.extend_from_slice(&framed(&packet));
        sender.write_all(&wire).await.unwrap();
        wait_for(|| handle.is_finished()).await;
        // The second block is authenticated before playback. Fail closed
        // without releasing either queued copy when replay is detected.
        assert_eq!(state.lock().unwrap().writes.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn same_session_accepts_a_replacement_data_connection() {
        let (ports, handle, state) = harness().await;
        handle.set_rate(test_rate(true)).await.unwrap();

        let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        first
            .write_all(&framed(&make_packet(70, 0, OWNED_AAC_FRAME_HEX, [70; 8])))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 1).await;
        drop(first);

        let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.data_port))
            .await
            .unwrap();
        second
            .write_all(&framed(&make_packet(
                900,
                SAMPLES_PER_AAC_FRAME as u32,
                OWNED_AAC_FRAME_HEX,
                [71; 8],
            )))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes.len() == 2).await;
        assert!(!handle.is_finished());
        handle.abort().await;
    }

    fn make_packet(
        sequence: u32,
        timestamp: u32,
        payload_hex: &str,
        nonce_suffix: [u8; 8],
    ) -> Vec<u8> {
        make_packet_with_ssrc(
            sequence,
            timestamp,
            BUFFERED_SSRC_AAC_44K1_STEREO,
            payload_hex,
            nonce_suffix,
        )
    }

    fn make_packet_with_ssrc(
        sequence: u32,
        timestamp: u32,
        ssrc: u32,
        payload_hex: &str,
        nonce_suffix: [u8; 8],
    ) -> Vec<u8> {
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.extend_from_slice(&(0x8000_0000 | (sequence & SEQUENCE_MASK)).to_be_bytes());
        header.extend_from_slice(&timestamp.to_be_bytes());
        header.extend_from_slice(&ssrc.to_be_bytes());
        let mut plaintext = hex::decode(payload_hex).unwrap();
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce_suffix);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&TEST_KEY));
        let tag = cipher
            .encrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                &header[4..HEADER_BYTES],
                &mut plaintext,
            )
            .unwrap();
        let mut packet = header;
        packet.extend_from_slice(&plaintext);
        packet.extend_from_slice(&tag);
        packet.extend_from_slice(&nonce_suffix);
        packet
    }

    // ---------------------------------------------------------------- stall
    //
    // The shipped check was `read_limit > 0 && framer.partial_frame_timed_out()`
    // with a 5 s timeout, and it ended two healthy iOS sessions on 30-win
    // (2026-08-16) at 425 s and 410 s of ordinary playback. Nothing here existed
    // then, which is the actual reason it shipped.

    #[test]
    fn an_idle_sender_with_audio_in_hand_is_not_a_stall() {
        // The regression, stated as a test: a buffered sender fills the queue and
        // goes quiet, the burst ended mid-frame, and `read_limit` is positive
        // because retention sits far below the advertised 8 MiB. Every input the
        // old check looked at is set exactly as it was in the failure.
        assert!(!starved_stall(
            true,
            false,
            true,
            TCP_READ_CHUNK_BYTES,
            true
        ));
        assert!(!starved_stall(
            true,
            false,
            false,
            TCP_READ_CHUNK_BYTES,
            true
        ));
    }

    #[test]
    fn a_paused_session_is_never_a_stall() {
        // A pause receives nothing by design; without the `playing` term a long
        // pause would look exactly like a dead peer.
        assert!(!starved_stall(
            false,
            true,
            true,
            TCP_READ_CHUNK_BYTES,
            true
        ));
    }

    #[test]
    fn backpressure_is_not_a_stall() {
        // A full queue stops our own reads. That is us, not the sender.
        assert!(!starved_stall(true, true, true, 0, true));
    }

    #[test]
    fn a_starved_playing_session_with_no_bytes_is_a_stall() {
        // Everything drained, still playing, still nothing arriving: the one
        // shape that means the sender is gone rather than merely ahead.
        assert!(starved_stall(true, true, true, TCP_READ_CHUNK_BYTES, true));
        // ...but not before the framer says the silence is long enough.
        assert!(!starved_stall(
            true,
            true,
            true,
            TCP_READ_CHUNK_BYTES,
            false
        ));
    }

    #[test]
    fn a_burst_ending_mid_frame_leaves_a_partial_frame() {
        // Why the old check fired at all: TCP has no reason to end a burst on a
        // frame boundary, so the idle state normally *does* hold a partial frame.
        let packet = vec![0u8; 40];
        let block = framed(&packet);
        let mut framer = TcpBlockFramer::new();
        framer.push(&block[..block.len() - 3]).unwrap();
        assert!(framer.has_partial_frame());
        assert!(framer.next_packet().unwrap().is_none());
        // Completing it clears the condition, and emptying clears the clock.
        framer.push(&block[block.len() - 3..]).unwrap();
        assert!(!framer.has_partial_frame());
        assert!(framer.next_packet().unwrap().is_some());
        assert!(framer.last_progress.is_none());
    }

    #[test]
    fn the_stall_clock_needs_the_full_timeout() {
        let block = framed(&vec![0u8; 40]);
        let mut framer = TcpBlockFramer::new();
        framer.push(&block[..8]).unwrap();
        let start = framer.last_progress.unwrap();
        assert!(!framer.partial_frame_timed_out(start));
        assert!(!framer
            .partial_frame_timed_out(start + STARVED_STALL_TIMEOUT - Duration::from_millis(1)));
        assert!(framer.partial_frame_timed_out(start + STARVED_STALL_TIMEOUT));
        // The value that shipped. Kept as a number so a future edit that lowers
        // the constant back into a sender's normal idle gap fails here first.
        assert!(!framer.partial_frame_timed_out(start + Duration::from_secs(5)));
    }

    fn framed(packet: &[u8]) -> Vec<u8> {
        let length = u16::try_from(packet.len() + LENGTH_PREFIX_BYTES).unwrap();
        let mut block = Vec::with_capacity(length as usize);
        block.extend_from_slice(&length.to_be_bytes());
        block.extend_from_slice(packet);
        block
    }
}
