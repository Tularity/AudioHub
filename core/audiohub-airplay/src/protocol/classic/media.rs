//! Bounded classic RAOP UDP media actor.

use super::audio_crypto::{decrypt_audio_payload, MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES};
use crate::protocol::media;
use media::clock::{
    build_timing_request, NtpTimestamp, PendingTimingRequest, SyncAnchor, TimingFeedback,
    TimingResponse, TimingWindow,
};
use media::decode::{Type96AlacDecoder, CHANNELS, FRAMES_PER_PACKET, SAMPLE_RATE};
use media::jitter::JitterBuffer;
use media::packet::{RingDisposition, RtpPacket, SequenceExtender16, TimestampExtender32};
use rand::random;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant, MissedTickBehavior};
use zeroize::Zeroizing;

const RTP_HEADER_BYTES: usize = 12;
const MAX_AUDIO_DATAGRAM_BYTES: usize = RTP_HEADER_BYTES + MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES;
const CONTROL_WRAPPER_BYTES: usize = 4;
const COMMAND_CAPACITY: usize = 4;
const GAP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const STARTUP_PRIME: Duration = Duration::from_millis(24);
const TIMING_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const CLASSIC_TIMING_SEQUENCE: u16 = 7;
const TIMING_RESPONSE_LIFETIME: Duration = Duration::from_secs(1);
const TIMING_WINDOW_AGE_NS: u64 = 15_000_000_000;
const TIMING_MAX_RTT_NS: u64 = 500_000_000;
const MIN_SYNC_INTERVAL: Duration = Duration::from_millis(10);
const RETRANSMIT_BACKOFF: Duration = Duration::from_millis(50);
const RETRANSMIT_WINDOW: Duration = Duration::from_millis(300);
const MAX_RETRANSMIT_ATTEMPTS: u8 = 6;
const MAX_RETRANSMIT_PACKETS: u16 = 32;
const MAX_CONCEALMENT_PACKETS: usize = 128;
// Retain the full valid two-second NTP lead and leave room for arrival jitter
// beyond the largest latency this actor advertises as retainable.
const CLASSIC_PACKET_CAPACITY: usize = 1_024;
const CLASSIC_REORDER_WINDOW: u64 = 1_024;
const LATENCY_SAFETY_PACKETS: usize = 24;
const MAX_BUFFERED_LATENCY_PACKETS: usize = CLASSIC_PACKET_CAPACITY - LATENCY_SAFETY_PACKETS;
const MAX_CLOCKED_FUTURE: Duration = Duration::from_secs(30);
const MAX_CLOCKED_LATENESS: Duration = Duration::from_millis(500);
const MAX_PACED_LATENESS: Duration = Duration::from_millis(24);
const RESYNC_ANCHOR_MAX_AGE: Duration = Duration::from_secs(1);
const RESYNC_SEQUENCE_THRESHOLD: u64 = 512;
const MAX_RESYNC_SEQUENCE_JUMP: u64 = (1u64 << 15) - 1;
// The platform PCM bus primes five 10 ms blocks. Write 50 ms before the
// mapped RTP-at-DAC instant and let the bounded platform sink own the final
// device-clock edge.
const PCM_OUTPUT_LEAD: Duration = Duration::from_millis(50);
const MAX_LATENCY_FRAMES: u32 = (MAX_BUFFERED_LATENCY_PACKETS * FRAMES_PER_PACKET) as u32;

fn classic_jitter() -> JitterBuffer {
    JitterBuffer::with_limits(CLASSIC_PACKET_CAPACITY, CLASSIC_REORDER_WINDOW)
        .expect("fixed classic jitter limits are valid")
}

pub(crate) struct ClassicAudioConfig {
    pub(crate) key: Zeroizing<[u8; 16]>,
    pub(crate) iv: Zeroizing<[u8; 16]>,
    pub(crate) min_latency: Option<u32>,
    pub(crate) max_latency: Option<u32>,
}

impl fmt::Debug for ClassicAudioConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassicAudioConfig")
            .field("key_material", &"<redacted>")
            .field("min_latency", &self.min_latency)
            .field("max_latency", &self.max_latency)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClassicPorts {
    pub(crate) data_port: u16,
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
}

pub(crate) struct PreparedClassic {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    timing_socket: UdpSocket,
    peer_ip: IpAddr,
    remote_control: SocketAddr,
    remote_timing: SocketAddr,
    ports: ClassicPorts,
    audio: ClassicAudioConfig,
    decoder: Type96AlacDecoder,
    validator: Type96AlacDecoder,
    jitter: JitterBuffer,
    timing_window: TimingWindow,
}

impl PreparedClassic {
    pub(crate) async fn prepare(
        local: SocketAddr,
        peer: SocketAddr,
        transport: super::transport::Transport,
        audio: ClassicAudioConfig,
    ) -> io::Result<Self> {
        validate_prepare_inputs(local, peer, transport, &audio)?;

        let data_socket = UdpSocket::bind(with_port(local, 0)).await?;
        let control_socket = UdpSocket::bind(with_port(local, 0)).await?;
        let timing_socket = UdpSocket::bind(with_port(local, 0)).await?;
        let ports = ClassicPorts {
            data_port: data_socket.local_addr()?.port(),
            control_port: control_socket.local_addr()?.port(),
            timing_port: timing_socket.local_addr()?.port(),
        };

        let decoder = Type96AlacDecoder::new()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // Admission validates ALAC before an untrusted UDP source port can be
        // pinned. Playback uses a separate decoder so validation cannot carry
        // malformed decoder state into audible delivery.
        let validator = Type96AlacDecoder::new()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let timing_window =
            TimingWindow::new(8, TIMING_WINDOW_AGE_NS, TIMING_MAX_RTT_NS).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid classic timing limits: {error:?}"),
                )
            })?;

        Ok(Self {
            data_socket,
            control_socket,
            timing_socket,
            peer_ip: normalize_ip(peer.ip()),
            remote_control: with_port(peer, transport.control_port),
            remote_timing: with_port(peer, transport.timing_port),
            ports,
            audio,
            decoder,
            validator,
            jitter: classic_jitter(),
            timing_window,
        })
    }

    pub(crate) fn ports(&self) -> ClassicPorts {
        self.ports
    }

    pub(crate) fn start(self, output: Box<dyn media::engine::PcmOutput>) -> ClassicMediaHandle {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        // Construct the output guard before spawning. If the handle is dropped
        // before Tokio first polls the task, cancelling the future still drops
        // the guard and flushes the platform sink.
        let engine = ClassicEngine::new(self, output, command_rx);
        let task = tokio::spawn(async move { engine.run().await });
        ClassicMediaHandle {
            commands,
            task: Some(task),
        }
    }
}

pub(crate) struct ClassicMediaHandle {
    commands: mpsc::Sender<MediaCommand>,
    task: Option<JoinHandle<()>>,
}

impl ClassicMediaHandle {
    pub(crate) fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn prepare_record(
        &self,
        sequence: Option<u16>,
        timestamp: Option<u32>,
    ) -> io::Result<ClassicRecordCommit> {
        if self.is_finished() || self.commands.is_closed() {
            return Err(media_stopped());
        }
        reserve_record(self.commands.clone(), sequence, timestamp)
    }

    pub(crate) async fn flush(
        &self,
        sequence: Option<u16>,
        timestamp: Option<u32>,
    ) -> io::Result<()> {
        let (reply, done) = oneshot::channel();
        self.commands
            .send(MediaCommand::Flush {
                floor: RecordFloor::new(sequence, timestamp),
                telemetry_token: crate::telemetry::operation_token(),
                reply,
            })
            .await
            .map_err(|_| media_stopped())?;
        done.await.map_err(|_| media_stopped())
    }

    pub(crate) async fn abort(&mut self) {
        let commands = self.commands.clone();
        let Some(task) = self.task.as_mut() else {
            return;
        };
        if !task.is_finished() {
            let (reply, done) = oneshot::channel();
            let shutdown = async {
                commands
                    .send(MediaCommand::Shutdown(reply))
                    .await
                    .map_err(|_| ())?;
                done.await.map_err(|_| ())
            };
            if !matches!(
                time::timeout(Duration::from_secs(1), shutdown).await,
                Ok(Ok(()))
            ) {
                task.abort();
            }
        }
        let _ = (&mut *task).await;
        self.task = None;
    }
}

impl Drop for ClassicMediaHandle {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[must_use = "dropping an uncommitted RECORD reservation keeps PCM gated"]
pub(crate) struct ClassicRecordCommit {
    permit: Option<mpsc::OwnedPermit<MediaCommand>>,
    floor: RecordFloor,
    telemetry_token: crate::telemetry::OperationToken,
}

impl ClassicRecordCommit {
    pub(crate) fn commit(mut self) {
        if let Some(permit) = self.permit.take() {
            permit.send(MediaCommand::Record {
                floor: self.floor,
                telemetry_token: self.telemetry_token,
            });
        }
    }
}

fn reserve_record(
    commands: mpsc::Sender<MediaCommand>,
    sequence: Option<u16>,
    timestamp: Option<u32>,
) -> io::Result<ClassicRecordCommit> {
    let permit = commands.try_reserve_owned().map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => io::Error::new(
            io::ErrorKind::WouldBlock,
            "classic media command queue is busy",
        ),
        mpsc::error::TrySendError::Closed(_) => media_stopped(),
    })?;
    Ok(ClassicRecordCommit {
        permit: Some(permit),
        floor: RecordFloor::new(sequence, timestamp),
        telemetry_token: crate::telemetry::operation_token(),
    })
}

enum MediaCommand {
    Record {
        floor: RecordFloor,
        telemetry_token: crate::telemetry::OperationToken,
    },
    Flush {
        floor: RecordFloor,
        telemetry_token: crate::telemetry::OperationToken,
        reply: oneshot::Sender<()>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RecordFloor {
    sequence: Option<u16>,
    timestamp: Option<u32>,
}

impl RecordFloor {
    const fn new(sequence: Option<u16>, timestamp: Option<u32>) -> Self {
        Self {
            sequence,
            timestamp,
        }
    }

    fn with_fallback(self, sequence: Option<u16>, timestamp: Option<u32>) -> Self {
        Self {
            sequence: self.sequence.or(sequence),
            timestamp: self.timestamp.or(timestamp),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ControlFloor {
    wire: RecordFloor,
    sequence: Option<u64>,
    timestamp: Option<u64>,
}

impl ControlFloor {
    fn record(wire: RecordFloor) -> Self {
        Self {
            wire,
            sequence: None,
            timestamp: None,
        }
    }

    fn allows(
        self,
        wire_sequence: u16,
        wire_timestamp: u32,
        sequence: u64,
        timestamp: u64,
    ) -> bool {
        self.sequence.map_or_else(
            || raw_at_or_after_u16(wire_sequence, self.wire.sequence),
            |floor| sequence >= floor,
        ) && self.timestamp.map_or_else(
            || raw_at_or_after_u32(wire_timestamp, self.wire.timestamp),
            |floor| timestamp >= floor,
        )
    }

    fn bind_to_accepted(&mut self, sequence: u64, timestamp: u64) {
        if self.sequence.is_none() {
            self.sequence = self
                .wire
                .sequence
                .map(|raw| floor_at_or_before(sequence, raw as u64, 1u64 << 16));
        }
        if self.timestamp.is_none() {
            self.timestamp = self
                .wire
                .timestamp
                .map(|raw| floor_at_or_before(timestamp, raw as u64, 1u64 << 32));
        }
    }

    fn advance(
        self,
        requested: RecordFloor,
        highest_sequence: Option<u64>,
        highest_timestamp: Option<u64>,
    ) -> Option<Self> {
        let sequence = advance_floor_dimension(
            self.wire.sequence,
            self.sequence,
            requested.sequence,
            highest_sequence,
            1u64 << 16,
        )?;
        let timestamp = advance_floor_dimension(
            self.wire.timestamp,
            self.timestamp,
            requested.timestamp,
            highest_timestamp,
            1u64 << 32,
        )?;
        Some(Self {
            wire: requested,
            sequence,
            timestamp,
        })
    }
}

fn raw_at_or_after_u16(value: u16, floor: Option<u16>) -> bool {
    floor.is_none_or(|floor| value.wrapping_sub(floor) < 0x8000)
}

fn raw_at_or_after_u32(value: u32, floor: Option<u32>) -> bool {
    floor.is_none_or(|floor| value.wrapping_sub(floor) < 0x8000_0000)
}

fn floor_at_or_before(value: u64, raw: u64, modulus: u64) -> u64 {
    let candidate = value / modulus * modulus + raw;
    if candidate > value {
        // The first accepted packet may be just after a wrap while its
        // declared floor belongs to the unrepresentable preceding epoch.
        candidate.saturating_sub(modulus)
    } else {
        candidate
    }
}

fn extend_nearest(reference: u64, raw: u64, modulus: u64) -> Option<u64> {
    let half = modulus / 2;
    let mut candidate = (reference / modulus) as u128 * modulus as u128 + raw as u128;
    let reference = reference as u128;
    if candidate + half as u128 <= reference {
        candidate += modulus as u128;
    } else if candidate > reference + half as u128 {
        candidate = candidate.checked_sub(modulus as u128)?;
    }
    u64::try_from(candidate).ok()
}

fn advance_floor_dimension<T>(
    previous_wire: Option<T>,
    previous: Option<u64>,
    requested_wire: Option<T>,
    high_water: Option<u64>,
    modulus: u64,
) -> Option<Option<u64>>
where
    T: Copy + Into<u64>,
{
    let Some(requested_wire) = requested_wire else {
        return Some(previous);
    };
    let requested_raw = requested_wire.into();
    // Repeated FLUSH is idempotent even after the packet high-water mark has
    // crossed a wire-number wrap and could otherwise re-epoch the same value.
    if previous_wire.is_some_and(|wire| wire.into() == requested_raw) {
        return Some(previous);
    }
    if let Some(previous) = previous {
        let reference = high_water.unwrap_or(previous).max(previous);
        let requested = extend_nearest(reference, requested_raw, modulus)?;
        return (requested >= previous).then_some(Some(requested));
    }
    if let Some(previous_wire) = previous_wire {
        let previous_raw = previous_wire.into();
        let delta = (requested_raw + modulus - previous_raw) % modulus;
        if delta >= modulus / 2 {
            return None;
        }
    }
    match high_water {
        Some(reference) => Some(Some(extend_nearest(reference, requested_raw, modulus)?)),
        None => Some(None),
    }
}

struct OutputGuard {
    inner: Box<dyn media::engine::PcmOutput>,
    flushed: bool,
}

impl OutputGuard {
    fn new(inner: Box<dyn media::engine::PcmOutput>) -> Self {
        Self {
            inner,
            // Cancellation may drop the task before it handles a command or
            // writes PCM. The sink must still receive one terminal flush.
            flushed: false,
        }
    }

    fn write(&mut self, samples: &[i16], token: crate::telemetry::OperationToken) {
        self.inner.write(samples, token);
        self.flushed = false;
    }

    fn flush(&mut self) {
        self.inner.flush();
        self.flushed = true;
    }
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        if !self.flushed {
            self.inner.flush();
        }
    }
}

struct ClassicClock {
    window: TimingWindow,
    pending: Option<PendingTimingRequest>,
    feedback: Option<TimingFeedback>,
    anchor: Option<SyncAnchor>,
    last_sync: Option<Instant>,
}

struct ScheduledFrame {
    timestamp: u32,
    pcm: Zeroizing<Vec<i16>>,
    telemetry_token: crate::telemetry::OperationToken,
}

#[derive(Debug, Clone, Copy)]
struct ConcealmentFrame {
    timestamp: u32,
    telemetry_token: crate::telemetry::OperationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schedule {
    Wait,
    DeliverAt(Instant),
    CatchUp,
    DropLate,
}

#[derive(Debug)]
struct GapWait {
    first_missing: u64,
    last_missing: u64,
    started: Instant,
    last_request: Option<Instant>,
    request_count: u8,
}

impl GapWait {
    fn new(first_missing: u64, last_missing: u64, started: Instant) -> Self {
        Self {
            first_missing,
            last_missing,
            started,
            last_request: None,
            request_count: 0,
        }
    }

    fn observe(&mut self, first_missing: u64, last_missing: u64, now: Instant) {
        let same_contiguous_gap =
            first_missing <= self.last_missing && last_missing >= self.first_missing;
        if same_contiguous_gap {
            self.first_missing = first_missing;
            self.last_missing = last_missing;
        } else if first_missing != self.first_missing || last_missing != self.last_missing {
            *self = Self::new(first_missing, last_missing, now);
        }
    }
}

struct ClassicEngine {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    timing_socket: UdpSocket,
    peer_ip: IpAddr,
    remote_control: SocketAddr,
    remote_timing: SocketAddr,
    data_source: Option<SocketAddr>,
    audio: ClassicAudioConfig,
    decoder: Type96AlacDecoder,
    validator: Type96AlacDecoder,
    jitter: JitterBuffer,
    sequence: SequenceExtender16,
    timestamp: TimestampExtender32,
    stream_ssrc: Option<u32>,
    stream_origin: Option<(u64, u64)>,
    highest_sequence: Option<u64>,
    highest_timestamp: Option<u64>,
    floor: ControlFloor,
    active: bool,
    delivery_started: bool,
    startup_deadline: Option<Instant>,
    scheduled: Option<ScheduledFrame>,
    concealment: VecDeque<ConcealmentFrame>,
    gap: Option<GapWait>,
    catching_up: bool,
    retransmit_sequence: u16,
    clock: ClassicClock,
    monotonic_origin: Instant,
    output: OutputGuard,
    commands: mpsc::Receiver<MediaCommand>,
}

impl ClassicEngine {
    fn new(
        prepared: PreparedClassic,
        output: Box<dyn media::engine::PcmOutput>,
        commands: mpsc::Receiver<MediaCommand>,
    ) -> Self {
        Self {
            data_socket: prepared.data_socket,
            control_socket: prepared.control_socket,
            timing_socket: prepared.timing_socket,
            peer_ip: prepared.peer_ip,
            remote_control: prepared.remote_control,
            remote_timing: prepared.remote_timing,
            data_source: None,
            audio: prepared.audio,
            decoder: prepared.decoder,
            validator: prepared.validator,
            jitter: prepared.jitter,
            sequence: SequenceExtender16::new(),
            timestamp: TimestampExtender32::new(),
            stream_ssrc: None,
            stream_origin: None,
            highest_sequence: None,
            highest_timestamp: None,
            floor: ControlFloor::default(),
            active: false,
            delivery_started: false,
            startup_deadline: None,
            scheduled: None,
            concealment: VecDeque::with_capacity(MAX_CONCEALMENT_PACKETS),
            gap: None,
            catching_up: false,
            retransmit_sequence: random(),
            clock: ClassicClock {
                window: prepared.timing_window,
                pending: None,
                feedback: None,
                anchor: None,
                last_sync: None,
            },
            monotonic_origin: Instant::now(),
            output: OutputGuard::new(output),
            commands,
        }
    }

    async fn run(mut self) {
        let mut data = [0u8; MAX_AUDIO_DATAGRAM_BYTES + 1];
        let mut control = [0u8; CONTROL_WRAPPER_BYTES + MAX_AUDIO_DATAGRAM_BYTES + 1];
        let mut timing = [0u8; media::clock::TIMING_RESPONSE_LEN + 1];
        let mut gap_tick = time::interval(GAP_POLL_INTERVAL);
        gap_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut timing_tick = time::interval_at(Instant::now(), TIMING_PROBE_INTERVAL);
        timing_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            self.prepare_scheduled();
            let schedule = self.schedule();
            let media_turn_complete = match schedule {
                Schedule::DeliverAt(deadline) if deadline <= Instant::now() => {
                    self.catching_up = false;
                    self.deliver_scheduled();
                    true
                }
                Schedule::CatchUp | Schedule::DropLate => {
                    if !self.catching_up {
                        self.output.flush();
                        self.catching_up = true;
                    }
                    self.scheduled = None;
                    true
                }
                Schedule::Wait | Schedule::DeliverAt(_) => false,
            };
            let deadline = match (schedule, media_turn_complete) {
                (_, true) => None,
                (Schedule::DeliverAt(deadline), false) => Some(deadline),
                (Schedule::Wait | Schedule::CatchUp | Schedule::DropLate, false) => None,
            };
            let sleep_until =
                deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));

            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        Some(MediaCommand::Record { floor, telemetry_token }) => {
                            self.activate_record(floor, telemetry_token);
                        }
                        Some(MediaCommand::Flush { floor, telemetry_token, reply }) => {
                            self.flush_stream(floor, telemetry_token);
                            let _ = reply.send(());
                        }
                        Some(MediaCommand::Shutdown(reply)) => {
                            self.output.flush();
                            let _ = reply.send(());
                            return;
                        }
                        None => break,
                    }
                }
                received = self.control_socket.recv_from(&mut control) => {
                    let Ok((length, source)) = received else { break };
                    if endpoint_eq(source, self.remote_control) {
                        self.accept_control(&control[..length], source);
                    }
                }
                received = self.timing_socket.recv_from(&mut timing) => {
                    let Ok((length, source)) = received else { break };
                    if endpoint_eq(source, self.remote_timing) {
                        self.accept_timing_response(&timing[..length]);
                    }
                }
                received = self.data_socket.recv_from(&mut data) => {
                    let Ok((length, source)) = received else { break };
                    if length <= MAX_AUDIO_DATAGRAM_BYTES
                        && normalize_ip(source.ip()) == self.peer_ip
                        && self.data_source.is_none_or(|pinned| endpoint_eq(pinned, source))
                    {
                        let token = crate::telemetry::operation_token();
                        self.accept_audio(&data[..length], Some(source), token);
                    }
                }
                _ = gap_tick.tick() => self.service_media_tick().await,
                _ = timing_tick.tick() => self.send_timing_probe().await,
                _ = time::sleep_until(sleep_until), if deadline.is_some() => {
                    self.catching_up = false;
                    self.deliver_scheduled();
                }
                _ = std::future::ready(()), if media_turn_complete => {
                    tokio::task::yield_now().await;
                }
            }
        }
        self.output.flush();
    }

    fn accept_audio(
        &mut self,
        datagram: &[u8],
        source: Option<SocketAddr>,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> bool {
        let packet = match RtpPacket::parse(datagram) {
            Ok(packet) if packet.payload_type == 96 => packet,
            _ => return false,
        };
        let clear = match decrypt_audio_payload(packet.payload, &self.audio.key, &self.audio.iv) {
            Ok(clear) => clear,
            Err(_) => return false,
        };
        match self.validator.decode_packet(&clear) {
            Ok(decoded) if decoded.frames() == FRAMES_PER_PACKET => {}
            _ => return false,
        }

        if !self.active {
            if let Some(source) = source {
                self.data_source.get_or_insert(source);
            }
            return true;
        }
        let sequence_preview = match self.sequence.peek(packet.sequence) {
            Ok(preview) if preview.disposition != RingDisposition::Duplicate => preview,
            _ => return false,
        };
        match sequence_preview.disposition {
            RingDisposition::Reordered { behind }
                if behind > CLASSIC_REORDER_WINDOW.saturating_mul(2) =>
            {
                return false
            }
            _ => {}
        }
        let timestamp_preview = match self.timestamp.peek(packet.timestamp) {
            Ok(preview) if preview.disposition != RingDisposition::Duplicate => preview,
            _ => return false,
        };
        let timestamp_limit = CLASSIC_REORDER_WINDOW
            .saturating_mul(FRAMES_PER_PACKET as u64)
            .saturating_mul(2);
        match timestamp_preview.disposition {
            RingDisposition::Reordered { behind } if behind > timestamp_limit => return false,
            _ => {}
        }
        if self.stream_ssrc.is_some_and(|ssrc| ssrc != packet.ssrc) {
            return false;
        }
        if let Some((origin_sequence, origin_timestamp)) = self.stream_origin {
            let sequence_delta = sequence_preview.value as i128 - origin_sequence as i128;
            let timestamp_delta = timestamp_preview.value as i128 - origin_timestamp as i128;
            if timestamp_delta != sequence_delta * FRAMES_PER_PACKET as i128 {
                return false;
            }
        }
        if !self.floor.allows(
            packet.sequence,
            packet.timestamp,
            sequence_preview.value,
            timestamp_preview.value,
        ) {
            return false;
        }

        let discontinuity = matches!(
            sequence_preview.disposition,
            RingDisposition::Advanced { by } if by > RESYNC_SEQUENCE_THRESHOLD
        ) || matches!(
            timestamp_preview.disposition,
            RingDisposition::Advanced { by }
                if by > RESYNC_SEQUENCE_THRESHOLD.saturating_mul(FRAMES_PER_PACKET as u64)
        );
        if discontinuity {
            if !self.clock_validates_resync(
                packet.timestamp,
                sequence_preview.disposition,
                timestamp_preview.disposition,
            ) {
                return false;
            }
            self.reset_for_resync(sequence_preview.value, timestamp_preview.value, packet.ssrc);
        }

        if self
            .jitter
            .insert_with_token(
                sequence_preview.value,
                timestamp_preview.value,
                packet.payload.to_vec(),
                telemetry_token,
            )
            .is_err()
        {
            return false;
        }
        self.sequence
            .commit(packet.sequence, sequence_preview)
            .expect("single-threaded sequence preview remains current");
        self.timestamp
            .commit(packet.timestamp, timestamp_preview)
            .expect("single-threaded timestamp preview remains current");
        self.floor
            .bind_to_accepted(sequence_preview.value, timestamp_preview.value);
        self.stream_ssrc.get_or_insert(packet.ssrc);
        self.stream_origin
            .get_or_insert((sequence_preview.value, timestamp_preview.value));
        self.highest_sequence = Some(
            self.highest_sequence
                .map_or(sequence_preview.value, |highest| {
                    highest.max(sequence_preview.value)
                }),
        );
        self.highest_timestamp = Some(
            self.highest_timestamp
                .map_or(timestamp_preview.value, |highest| {
                    highest.max(timestamp_preview.value)
                }),
        );
        if let Some(source) = source {
            self.data_source.get_or_insert(source);
        }
        if self.startup_deadline.is_none() {
            self.startup_deadline = Some(Instant::now() + STARTUP_PRIME);
        }
        true
    }

    fn clock_validates_resync(
        &self,
        packet_timestamp: u32,
        sequence: RingDisposition,
        timestamp: RingDisposition,
    ) -> bool {
        if !validated_discontinuity(sequence, timestamp) {
            return false;
        }
        let (Some(anchor), Some(last_sync)) = (self.clock.anchor, self.clock.last_sync) else {
            return false;
        };
        if last_sync.elapsed() > RESYNC_ANCHOR_MAX_AGE
            || !latency_allowed(&self.audio, anchor.latency_frames())
        {
            return false;
        }
        let anchor_distance = packet_timestamp.wrapping_sub(anchor.rtp_current) as i32;
        let maximum_anchor_distance = MAX_LATENCY_FRAMES as u64
            + CLASSIC_REORDER_WINDOW.saturating_mul(FRAMES_PER_PACKET as u64);
        if u64::from(anchor_distance.unsigned_abs()) > maximum_anchor_distance {
            return false;
        }
        let Some(sample) = self.clock.window.best(self.monotonic_ns()) else {
            return false;
        };
        let Some(local_anchor_ns) = anchor.local_anchor_ns(sample) else {
            return false;
        };
        let Some(presentation_ns) =
            rtp_presentation_ns(local_anchor_ns, anchor.rtp_at_dac, packet_timestamp)
        else {
            return false;
        };
        let Some(now_ns) = system_ntp_now().map(NtpTimestamp::to_nanos) else {
            return false;
        };
        if presentation_ns >= now_ns {
            presentation_ns - now_ns <= duration_nanos(MAX_CLOCKED_FUTURE)
        } else {
            now_ns - presentation_ns <= duration_nanos(MAX_CLOCKED_LATENESS)
        }
    }

    fn reset_for_resync(&mut self, sequence: u64, timestamp: u64, ssrc: u32) {
        self.jitter = classic_jitter();
        self.decoder.reset();
        self.validator.reset();
        self.stream_ssrc = Some(ssrc);
        self.stream_origin = Some((sequence, timestamp));
        self.delivery_started = false;
        self.startup_deadline = None;
        self.scheduled = None;
        self.concealment.clear();
        self.gap = None;
        self.catching_up = false;
        self.output.flush();
    }

    fn accept_control(&mut self, datagram: &[u8], _source: SocketAddr) {
        if datagram.len() < 2 {
            return;
        }
        match datagram[1] {
            0xd4 if self
                .clock
                .last_sync
                .is_none_or(|last| last.elapsed() >= MIN_SYNC_INTERVAL) =>
            {
                if let Ok(anchor) = SyncAnchor::parse(datagram) {
                    let latency = anchor.latency_frames();
                    if latency_allowed(&self.audio, latency) {
                        self.clock.anchor = Some(anchor);
                        self.clock.last_sync = Some(Instant::now());
                    }
                }
            }
            0xd6 if datagram[0] == 0x80 && datagram.len() > CONTROL_WRAPPER_BYTES => {
                let token = crate::telemetry::operation_token();
                self.accept_audio(&datagram[CONTROL_WRAPPER_BYTES..], None, token);
            }
            _ => {}
        }
    }

    fn accept_timing_response(&mut self, datagram: &[u8]) {
        let Some(pending) = self.clock.pending else {
            return;
        };
        let response = match TimingResponse::parse(datagram, Some(pending.sequence)) {
            Ok(response) => response,
            Err(_) => return,
        };
        let Some(arrival) = system_ntp_now() else {
            return;
        };
        let monotonic = self.monotonic_ns();
        let sample = match pending.validate_response(response, arrival.to_nanos(), monotonic) {
            Ok(sample) => sample,
            Err(_) => return,
        };
        if self.clock.window.insert(sample, monotonic).is_err() {
            return;
        }
        self.clock.pending = None;
        self.clock.feedback = Some(TimingFeedback {
            remote_reference: response.remote_transmit,
            local_receive: arrival,
        });
    }

    async fn send_timing_probe(&mut self) {
        let monotonic = self.monotonic_ns();
        if self
            .clock
            .pending
            .is_some_and(|pending| monotonic <= pending.expires_at_monotonic_ns)
        {
            return;
        }
        self.clock.pending = None;
        let Some(transmit) = system_ntp_now() else {
            return;
        };
        // Classic senders require the fixed wire sequence. The nonzero
        // transmit timestamp still correlates each response independently.
        let sequence = CLASSIC_TIMING_SEQUENCE;
        let request = build_timing_request(sequence, transmit, self.clock.feedback);
        if self
            .timing_socket
            .send_to(&request, self.remote_timing)
            .await
            .is_err()
        {
            return;
        }
        self.clock.pending = Some(PendingTimingRequest {
            sequence,
            origin: transmit,
            local_departure_ns: transmit.to_nanos(),
            expires_at_monotonic_ns: monotonic
                .saturating_add(duration_nanos(TIMING_RESPONSE_LIFETIME)),
        });
    }

    fn prepare_scheduled(&mut self) {
        if !self.active || !self.delivery_started || self.scheduled.is_some() {
            return;
        }
        if let Some(frame) = self.concealment.pop_front() {
            self.scheduled = Some(ScheduledFrame {
                timestamp: frame.timestamp,
                pcm: Zeroizing::new(vec![0; FRAMES_PER_PACKET * CHANNELS]),
                telemetry_token: frame.telemetry_token,
            });
            return;
        }
        let Some(packet) = self.jitter.pop_ready() else {
            return;
        };
        let pcm = Zeroizing::new(
            decrypt_audio_payload(packet.payload(), &self.audio.key, &self.audio.iv)
                .ok()
                .and_then(|clear| self.decoder.decode_packet(&clear).ok())
                .filter(|decoded| decoded.frames() == FRAMES_PER_PACKET)
                .map_or_else(
                    || vec![0; FRAMES_PER_PACKET * CHANNELS],
                    |decoded| decoded.samples().to_vec(),
                ),
        );
        self.scheduled = Some(ScheduledFrame {
            timestamp: packet.timestamp as u32,
            pcm,
            telemetry_token: packet.telemetry_token,
        });
        if self.jitter.missing_ranges().is_empty() {
            self.gap = None;
        }
    }

    fn schedule(&self) -> Schedule {
        let Some(frame) = self.scheduled.as_ref() else {
            return Schedule::Wait;
        };
        let Some(anchor) = self.clock.anchor else {
            return Schedule::Wait;
        };
        let Some(sample) = self.clock.window.best(self.monotonic_ns()) else {
            return Schedule::Wait;
        };
        let Some(local_anchor_ns) = anchor.local_anchor_ns(sample) else {
            return Schedule::Wait;
        };
        let Some(presentation_ns) =
            rtp_presentation_ns(local_anchor_ns, anchor.rtp_at_dac, frame.timestamp)
        else {
            return Schedule::DropLate;
        };
        let Some(output_ns) = presentation_ns.checked_sub(duration_nanos(PCM_OUTPUT_LEAD)) else {
            return Schedule::DropLate;
        };
        let Some(now_ntp) = system_ntp_now().map(NtpTimestamp::to_nanos) else {
            return Schedule::Wait;
        };
        classify_output_deadline(now_ntp, output_ns, Instant::now())
    }

    fn deliver_scheduled(&mut self) {
        if let Some(frame) = self.scheduled.take() {
            self.output.write(&frame.pcm, frame.telemetry_token);
        }
    }

    async fn service_media_tick(&mut self) {
        if self.active
            && !self.delivery_started
            && self
                .startup_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.delivery_started = true;
            self.jitter.finish_priming();
        }
        if self.active && self.delivery_started {
            self.service_gap().await;
        }
    }

    async fn service_gap(&mut self) {
        let Some(missing) = self.jitter.missing_ranges().first().copied() else {
            self.gap = None;
            return;
        };
        let now = Instant::now();
        if self.gap.is_none() {
            self.gap = Some(GapWait::new(missing.first, missing.last, now));
        } else if let Some(gap) = &mut self.gap {
            gap.observe(missing.first, missing.last, now);
        }

        if self
            .gap
            .as_ref()
            .is_some_and(|gap| now.duration_since(gap.started) >= RETRANSMIT_WINDOW)
        {
            let next = missing.last.saturating_add(1);
            if self.jitter.advance_to(next).is_ok() {
                self.enqueue_concealment(missing.first, missing.last);
            }
            self.gap = None;
            return;
        }

        let Some(gap) = self.gap.as_ref() else {
            return;
        };
        if gap.request_count >= MAX_RETRANSMIT_ATTEMPTS
            || gap
                .last_request
                .is_some_and(|last| now.duration_since(last) < RETRANSMIT_BACKOFF)
        {
            return;
        }
        let count = missing.packet_count().min(MAX_RETRANSMIT_PACKETS as u64) as u16;
        let request =
            build_retransmit_request(self.retransmit_sequence, missing.first as u16, count);
        if self
            .control_socket
            .send_to(&request, self.remote_control)
            .await
            .is_ok()
        {
            self.retransmit_sequence = self.retransmit_sequence.wrapping_add(1);
            if let Some(gap) = &mut self.gap {
                gap.last_request = Some(now);
                gap.request_count = gap.request_count.saturating_add(1);
            }
        }
    }

    fn enqueue_concealment(&mut self, first: u64, last: u64) {
        let Some((origin_sequence, origin_timestamp)) = self.stream_origin else {
            return;
        };
        let token = crate::telemetry::operation_token();
        let mut sequence = first;
        while sequence <= last && self.concealment.len() < MAX_CONCEALMENT_PACKETS {
            let delta = sequence as i128 - origin_sequence as i128;
            let timestamp = origin_timestamp as i128 + delta * FRAMES_PER_PACKET as i128;
            if let Ok(timestamp) = u64::try_from(timestamp) {
                self.concealment.push_back(ConcealmentFrame {
                    timestamp: timestamp as u32,
                    telemetry_token: token,
                });
            }
            let Some(next) = sequence.checked_add(1) else {
                break;
            };
            sequence = next;
        }
    }

    fn activate_record(
        &mut self,
        floor: RecordFloor,
        _telemetry_token: crate::telemetry::OperationToken,
    ) {
        self.reset_media(floor);
        self.active = true;
    }

    fn flush_stream(
        &mut self,
        requested: RecordFloor,
        _telemetry_token: crate::telemetry::OperationToken,
    ) {
        let fallback_sequence = self
            .highest_sequence
            .map(|value| (value as u16).wrapping_add(1));
        let fallback_timestamp = self
            .highest_timestamp
            .map(|value| (value as u32).wrapping_add(FRAMES_PER_PACKET as u32));
        let requested = requested
            .with_fallback(fallback_sequence, fallback_timestamp)
            .with_fallback(self.floor.wire.sequence, self.floor.wire.timestamp);
        let Some(floor) =
            self.floor
                .advance(requested, self.highest_sequence, self.highest_timestamp)
        else {
            // A delayed decreasing FLUSH must not reopen an older admission
            // boundary or discard media accepted under the current epoch.
            return;
        };
        self.jitter = classic_jitter();
        self.decoder.reset();
        self.validator.reset();
        self.floor = floor;
        self.delivery_started = false;
        self.startup_deadline = None;
        self.scheduled = None;
        self.concealment.clear();
        self.gap = None;
        self.catching_up = false;
        self.clock.anchor = None;
        self.clock.last_sync = None;
        self.output.flush();
    }

    fn reset_media(&mut self, floor: RecordFloor) {
        self.jitter = classic_jitter();
        self.sequence = SequenceExtender16::new();
        self.timestamp = TimestampExtender32::new();
        self.decoder.reset();
        self.validator.reset();
        self.stream_ssrc = None;
        self.stream_origin = None;
        self.highest_sequence = None;
        self.highest_timestamp = None;
        self.floor = ControlFloor::record(floor);
        self.delivery_started = false;
        self.startup_deadline = None;
        self.scheduled = None;
        self.concealment.clear();
        self.gap = None;
        self.catching_up = false;
    }

    fn monotonic_ns(&self) -> u64 {
        duration_nanos(self.monotonic_origin.elapsed())
    }
}

fn validate_prepare_inputs(
    local: SocketAddr,
    peer: SocketAddr,
    transport: super::transport::Transport,
    audio: &ClassicAudioConfig,
) -> io::Result<()> {
    if local.is_ipv4() != peer.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local and peer classic media addresses use different IP families",
        ));
    }
    let peer_ip = normalize_ip(peer.ip());
    if peer_ip.is_unspecified()
        || peer_ip.is_multicast()
        || peer_ip == IpAddr::V4(Ipv4Addr::BROADCAST)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic media peer address is not unicast",
        ));
    }
    if transport.control_port == 0 || transport.timing_port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic control and timing ports must be non-zero",
        ));
    }
    if audio
        .min_latency
        .is_some_and(|value| value > MAX_LATENCY_FRAMES)
        || matches!((audio.min_latency, audio.max_latency), (Some(min), Some(max)) if min > max)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic latency bounds are invalid",
        ));
    }
    Ok(())
}

fn latency_allowed(audio: &ClassicAudioConfig, latency: u32) -> bool {
    latency <= MAX_LATENCY_FRAMES
        && audio.min_latency.is_none_or(|minimum| latency >= minimum)
        && audio.max_latency.is_none_or(|maximum| latency <= maximum)
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        IpAddr::V4(ip) => IpAddr::V4(ip),
    }
}

fn endpoint_eq(left: SocketAddr, right: SocketAddr) -> bool {
    left.port() == right.port() && normalize_ip(left.ip()) == normalize_ip(right.ip())
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

fn build_retransmit_request(sequence: u16, first_missing: u16, count: u16) -> [u8; 8] {
    debug_assert!(count > 0 && count <= MAX_RETRANSMIT_PACKETS);
    let mut packet = [0u8; 8];
    packet[0] = 0x80;
    packet[1] = 0xd5;
    packet[2..4].copy_from_slice(&sequence.to_be_bytes());
    packet[4..6].copy_from_slice(&first_missing.to_be_bytes());
    packet[6..8].copy_from_slice(&count.to_be_bytes());
    packet
}

fn validated_discontinuity(sequence: RingDisposition, timestamp: RingDisposition) -> bool {
    let (
        RingDisposition::Advanced { by: sequence_jump },
        RingDisposition::Advanced { by: timestamp_jump },
    ) = (sequence, timestamp)
    else {
        return false;
    };
    sequence_jump > RESYNC_SEQUENCE_THRESHOLD
        && sequence_jump <= MAX_RESYNC_SEQUENCE_JUMP
        && sequence_jump
            .checked_mul(FRAMES_PER_PACKET as u64)
            .is_some_and(|expected| timestamp_jump == expected)
}

fn classify_output_deadline(now_ntp: u64, output_ns: u64, now: Instant) -> Schedule {
    if output_ns >= now_ntp {
        let future = Duration::from_nanos(output_ns - now_ntp);
        if future > MAX_CLOCKED_FUTURE {
            Schedule::Wait
        } else {
            Schedule::DeliverAt(now + future)
        }
    } else {
        let late = Duration::from_nanos(now_ntp - output_ns);
        if late <= MAX_PACED_LATENESS {
            Schedule::DeliverAt(now)
        } else if late <= MAX_CLOCKED_LATENESS {
            Schedule::CatchUp
        } else {
            Schedule::DropLate
        }
    }
}

fn rtp_presentation_ns(local_anchor_ns: u64, anchor_rtp: u32, rtp: u32) -> Option<u64> {
    let frame_delta = rtp.wrapping_sub(anchor_rtp) as i32 as i128;
    let time_delta = frame_delta
        .checked_mul(1_000_000_000)?
        .checked_div(SAMPLE_RATE as i128)?;
    u64::try_from(local_anchor_ns as i128 + time_delta).ok()
}

fn system_ntp_now() -> Option<NtpTimestamp> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    NtpTimestamp::from_unix_nanos(u64::try_from(nanos).ok()?)
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn media_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "classic media task has stopped")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{block_padding::NoPadding, BlockEncryptMut, KeyIvInit};
    use std::net::{Ipv6Addr, SocketAddrV6};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const NETWORK_KEY: [u8; 16] = [0x42; 16];
    const NETWORK_IV: [u8; 16] = [0x18; 16];

    fn stereo_packet(sequence: u16, timestamp: u32) -> Vec<u8> {
        // Uncompressed ALAC CPE: fixed 352-frame stereo, with a different
        // signed value per channel and packet so silence concealment fails.
        let mut bits = Vec::new();
        let mut push = |value: u32, width: usize| {
            bits.extend((0..width).rev().map(|bit| ((value >> bit) & 1) as u8));
        };
        push(1, 3);
        push(0, 4);
        push(0, 12);
        push(0, 1);
        push(0, 2);
        push(1, 1);
        for _ in 0..FRAMES_PER_PACKET {
            push(sequence as u32 + 1_000, 16);
            push((-(sequence as i16) - 2_000) as u16 as u32, 16);
        }
        push(7, 3);
        let mut payload: Vec<u8> = bits
            .chunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .enumerate()
                    .fold(0, |byte, (index, bit)| byte | (bit << (7 - index)))
            })
            .collect();
        let length = payload.len() / 16 * 16;
        cbc::Encryptor::<aes::Aes128>::new((&NETWORK_KEY).into(), (&NETWORK_IV).into())
            .encrypt_padded_mut::<NoPadding>(&mut payload[..length], length)
            .unwrap();
        let mut packet = vec![0x80, 0xe0];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&0x1020_3040u32.to_be_bytes());
        packet.extend_from_slice(&payload);
        packet
    }

    fn ntp_bytes(value: NtpTimestamp) -> [u8; 8] {
        (((value.seconds() as u64) << 32) | value.fraction() as u64).to_be_bytes()
    }

    fn sync_packet(timestamp: u32, latency: u32) -> [u8; 20] {
        let mut sync = [0; 20];
        sync[..2].copy_from_slice(&[0x90, 0xd4]);
        sync[4..8].copy_from_slice(&timestamp.wrapping_sub(latency).to_be_bytes());
        sync[8..16].copy_from_slice(&ntp_bytes(system_ntp_now().unwrap()));
        sync[16..20].copy_from_slice(&timestamp.to_be_bytes());
        sync
    }

    #[derive(Default)]
    struct NetworkOutputState {
        writes: Vec<(Instant, Vec<i16>)>,
        flushes: usize,
        dropped: bool,
    }

    struct NetworkOutput(Arc<Mutex<NetworkOutputState>>);

    impl media::engine::PcmOutput for NetworkOutput {
        fn write(&mut self, samples: &[i16], _: crate::telemetry::OperationToken) {
            self.0
                .lock()
                .unwrap()
                .writes
                .push((Instant::now(), samples.to_vec()));
        }

        fn flush(&mut self) {
            self.0.lock().unwrap().flushes += 1;
        }
    }

    impl Drop for NetworkOutput {
        fn drop(&mut self) {
            self.0.lock().unwrap().dropped = true;
        }
    }

    async fn wait_for_writes(state: &Arc<Mutex<NetworkOutputState>>, count: usize) {
        time::timeout(Duration::from_secs(2), async {
            while state.lock().unwrap().writes.len() < count {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("clocked PCM did not reach the test output");
    }

    #[test]
    fn network_fixture_preserves_distinct_signed_stereo_samples() {
        let packet = stereo_packet(10, 22_050);
        let clear = decrypt_audio_payload(&packet[12..], &NETWORK_KEY, &NETWORK_IV).unwrap();
        let mut decoder = Type96AlacDecoder::new().unwrap();
        let pcm = decoder.decode_packet(&clear).unwrap();
        assert_eq!(pcm.frames(), FRAMES_PER_PACKET);
        assert!(pcm
            .samples()
            .chunks_exact(2)
            .all(|frame| frame == [1_010, -2_010]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn classic_timing_keeps_sequence_seven_and_rotates_the_origin() {
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let timing = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let prepared = PreparedClassic::prepare(
            "127.0.0.1:0".parse().unwrap(),
            timing.local_addr().unwrap(),
            super::super::transport::Transport {
                control_port: control.local_addr().unwrap().port(),
                timing_port: timing.local_addr().unwrap().port(),
            },
            ClassicAudioConfig {
                key: Zeroizing::new(NETWORK_KEY),
                iv: Zeroizing::new(NETWORK_IV),
                min_latency: None,
                max_latency: None,
            },
        )
        .await
        .unwrap();
        let state = Arc::new(Mutex::new(NetworkOutputState::default()));
        let mut handle = prepared.start(Box::new(NetworkOutput(state)));
        let mut first = [0; 32];
        let (_, peer) = time::timeout(Duration::from_secs(1), timing.recv_from(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first[..4], &[0x80, 0xd2, 0, 7]);
        assert_ne!(&first[24..32], &[0; 8]);
        let mut response = first;
        response[1] = 0xd3;
        response[8..16].copy_from_slice(&first[24..32]);
        response[16..24].copy_from_slice(&first[24..32]);
        timing.send_to(&response, peer).await.unwrap();
        let mut second = [0; 32];
        time::timeout(Duration::from_secs(3), timing.recv_from(&mut second))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&second[..4], &[0x80, 0xd2, 0, 7]);
        assert_ne!(&first[24..32], &second[24..32]);
        assert_eq!(&second[8..16], &first[24..32]);
        assert_ne!(&second[16..24], &[0; 8]);
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn udp_record_clock_retransmit_flush_and_shutdown_preserve_pcm() {
        let data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let timing = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let prepared = PreparedClassic::prepare(
            "127.0.0.1:0".parse().unwrap(),
            data.local_addr().unwrap(),
            super::super::transport::Transport {
                control_port: control.local_addr().unwrap().port(),
                timing_port: timing.local_addr().unwrap().port(),
            },
            ClassicAudioConfig {
                key: Zeroizing::new(NETWORK_KEY),
                iv: Zeroizing::new(NETWORK_IV),
                min_latency: None,
                max_latency: None,
            },
        )
        .await
        .unwrap();
        let ports = prepared.ports();
        let target = |port| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        let state = Arc::new(Mutex::new(NetworkOutputState::default()));
        let mut handle = prepared.start(Box::new(NetworkOutput(state.clone())));
        let mut probe = [0; 32];
        let (length, source) = time::timeout(Duration::from_secs(1), timing.recv_from(&mut probe))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(length, 32);
        assert_eq!(source.port(), ports.timing_port);
        assert_eq!(&probe[..2], &[0x80, 0xd2]);

        // A malformed first sender must not claim the audio source port.
        other
            .send_to(&[0x80, 0xe0, 0, 0], target(ports.data_port))
            .await
            .unwrap();
        data.send_to(&stereo_packet(10, 22_050), target(ports.data_port))
            .await
            .unwrap();
        drop(handle.prepare_record(Some(10), Some(22_050)).unwrap());
        time::sleep(Duration::from_millis(30)).await;
        assert!(state.lock().unwrap().writes.is_empty());
        handle
            .prepare_record(Some(10), Some(22_050))
            .unwrap()
            .commit();
        // The acknowledged command fences the preceding RECORD on the actor.
        handle.flush(Some(10), Some(22_050)).await.unwrap();
        let anchor_sent = Instant::now();
        control
            .send_to(&sync_packet(22_050, 22_050), target(ports.control_port))
            .await
            .unwrap();
        data.send_to(&stereo_packet(10, 22_050), target(ports.data_port))
            .await
            .unwrap();
        data.send_to(&stereo_packet(12, 22_754), target(ports.data_port))
            .await
            .unwrap();
        let mut retransmit = [0; 8];
        let (length, source) = time::timeout(
            Duration::from_millis(250),
            control.recv_from(&mut retransmit),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, 8);
        assert_eq!(source.port(), ports.control_port);
        assert_eq!(&retransmit[..2], &[0x80, 0xd5]);
        assert_eq!(u16::from_be_bytes(retransmit[4..6].try_into().unwrap()), 11);
        assert_eq!(u16::from_be_bytes(retransmit[6..8].try_into().unwrap()), 1);
        let mut retransmitted = vec![0x80, 0xd6, 0, 1];
        retransmitted.extend_from_slice(&stereo_packet(11, 22_402));
        control
            .send_to(&retransmitted, target(ports.control_port))
            .await
            .unwrap();

        let mut response = [0; 32];
        response[..2].copy_from_slice(&[0x80, 0xd3]);
        response[2..4].copy_from_slice(&probe[2..4]);
        response[8..16].copy_from_slice(&probe[24..32]);
        let now = ntp_bytes(system_ntp_now().unwrap());
        response[16..24].copy_from_slice(&now);
        response[24..32].copy_from_slice(&now);
        other
            .send_to(&response, target(ports.timing_port))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(60)).await;
        assert!(
            state.lock().unwrap().writes.is_empty(),
            "untrusted timing must not release PCM"
        );
        let now = ntp_bytes(system_ntp_now().unwrap());
        response[16..24].copy_from_slice(&now);
        response[24..32].copy_from_slice(&now);
        timing
            .send_to(&response, target(ports.timing_port))
            .await
            .unwrap();
        wait_for_writes(&state, 3).await;
        {
            let state = state.lock().unwrap();
            assert_eq!(state.writes.len(), 3);
            assert!(state.writes[0].0.duration_since(anchor_sent) >= Duration::from_millis(350));
            for (index, (_, pcm)) in state.writes.iter().enumerate() {
                let sequence = 10 + index as i16;
                assert_eq!(pcm.len(), FRAMES_PER_PACKET * CHANNELS);
                assert!(pcm
                    .chunks_exact(2)
                    .all(|frame| frame == [1_000 + sequence, -2_000 - sequence]));
            }
        }

        handle.flush(Some(13), Some(23_106)).await.unwrap();
        data.send_to(&stereo_packet(12, 22_754), target(ports.data_port))
            .await
            .unwrap();
        data.send_to(&stereo_packet(13, 23_106), target(ports.data_port))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            state.lock().unwrap().writes.len(),
            3,
            "FLUSH requires a fresh playback anchor"
        );
        control
            .send_to(&sync_packet(23_106, 8_820), target(ports.control_port))
            .await
            .unwrap();
        wait_for_writes(&state, 4).await;
        handle.abort().await;
        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 4);
        assert!(state.writes[3]
            .1
            .chunks_exact(2)
            .all(|frame| frame == [1_013, -2_013]));
        assert!(state.dropped);
        assert!(state.flushes >= 3);
        assert!(handle.is_finished());
    }

    #[test]
    fn dropped_record_reservation_does_not_send_activation() {
        let (sender, mut receiver) = mpsc::channel(1);
        let reservation = reserve_record(sender.clone(), Some(7), Some(352)).unwrap();
        drop(reservation);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn committed_record_reservation_sends_exact_floor() {
        let (sender, mut receiver) = mpsc::channel(1);
        reserve_record(sender, Some(u16::MAX), Some(u32::MAX))
            .unwrap()
            .commit();
        assert!(matches!(
            receiver.try_recv(),
            Ok(MediaCommand::Record {
                floor: RecordFloor {
                    sequence: Some(u16::MAX),
                    timestamp: Some(u32::MAX),
                },
                ..
            })
        ));
    }

    #[test]
    fn logical_floor_survives_full_sequence_and_timestamp_wraps() {
        let first_sequence = u16::MAX - 5;
        let first_timestamp = u32::MAX - 703;
        let mut floor = ControlFloor::record(RecordFloor::new(
            Some(first_sequence),
            Some(first_timestamp),
        ));
        let mut sequences = SequenceExtender16::new();
        let mut timestamps = TimestampExtender32::new();

        for offset in 0..70_000u64 {
            let wire_sequence = first_sequence.wrapping_add(offset as u16);
            let wire_timestamp =
                first_timestamp.wrapping_add((offset * FRAMES_PER_PACKET as u64) as u32);
            let sequence = sequences.peek(wire_sequence).unwrap();
            let timestamp = timestamps.peek(wire_timestamp).unwrap();
            assert!(floor.allows(
                wire_sequence,
                wire_timestamp,
                sequence.value,
                timestamp.value,
            ));
            sequences.commit(wire_sequence, sequence).unwrap();
            timestamps.commit(wire_timestamp, timestamp).unwrap();
            floor.bind_to_accepted(sequence.value, timestamp.value);
        }

        assert!(sequences.highest().unwrap() >= 2 * (1u64 << 16));
        assert!(timestamps.highest().unwrap() >= (1u64 << 32));
    }

    #[test]
    fn floor_before_initial_epoch_is_bounded_and_unrepresentable_flush_is_rejected() {
        let mut floor = ControlFloor::record(RecordFloor::new(Some(u16::MAX), Some(u32::MAX)));
        assert!(floor.allows(0, 0, 0, 0));
        floor.bind_to_accepted(0, 0);
        assert_eq!(floor.sequence, Some(0));
        assert_eq!(floor.timestamp, Some(0));
        assert!(
            advance_floor_dimension::<u16>(None, None, Some(u16::MAX), Some(0), 1 << 16).is_none()
        );
    }

    #[test]
    fn repeated_floor_is_idempotent_and_decreasing_floor_is_rejected() {
        let mut floor = ControlFloor::record(RecordFloor::new(Some(100), Some(35_200)));
        floor.bind_to_accepted((1u64 << 16) + 100, (1u64 << 32) + 35_200);
        let high_sequence = Some((1u64 << 16) + 200);
        let high_timestamp = Some((1u64 << 32) + 70_400);

        let advanced = floor
            .advance(
                RecordFloor::new(Some(150), Some(52_800)),
                high_sequence,
                high_timestamp,
            )
            .unwrap();
        assert_eq!(
            advanced
                .advance(advanced.wire, high_sequence, high_timestamp)
                .unwrap(),
            advanced
        );
        assert!(advanced
            .advance(
                RecordFloor::new(Some(140), Some(49_280)),
                high_sequence,
                high_timestamp,
            )
            .is_none());
    }

    #[test]
    fn classic_jitter_retains_two_seconds_of_lookahead() {
        let mut jitter = classic_jitter();
        for offset in 0..251u64 {
            jitter
                .insert(
                    10_000 + offset,
                    20_000 + offset * FRAMES_PER_PACKET as u64,
                    vec![1],
                )
                .unwrap();
        }
        assert_eq!(jitter.len(), 251);

        let mut audio = ClassicAudioConfig {
            key: Zeroizing::new([0; 16]),
            iv: Zeroizing::new([0; 16]),
            min_latency: None,
            max_latency: Some(MAX_LATENCY_FRAMES + SAMPLE_RATE),
        };
        assert!(latency_allowed(&audio, SAMPLE_RATE * 2));
        assert!(latency_allowed(&audio, MAX_LATENCY_FRAMES));
        assert!(!latency_allowed(
            &audio,
            MAX_LATENCY_FRAMES + FRAMES_PER_PACKET as u32
        ));
        audio.min_latency = Some(MAX_LATENCY_FRAMES + 1);
        assert!(!latency_allowed(&audio, MAX_LATENCY_FRAMES));
    }

    #[test]
    fn substantial_lateness_enters_bounded_catch_up() {
        let now = Instant::now();
        assert_eq!(
            classify_output_deadline(1_000_000_000, 600_000_000, now),
            Schedule::CatchUp
        );
        assert_eq!(
            classify_output_deadline(1_000_000_000, 400_000_000, now),
            Schedule::DropLate
        );
        assert_eq!(
            classify_output_deadline(1_000_000_000, 990_000_000, now),
            Schedule::DeliverAt(now)
        );
    }

    #[test]
    fn disjoint_gap_gets_a_new_retry_window_but_advancing_gap_does_not() {
        let started = Instant::now();
        let mut gap = GapWait::new(100, 103, started);
        gap.observe(101, 103, started + Duration::from_millis(290));
        assert_eq!(gap.started, started);

        let second_started = started + Duration::from_millis(295);
        gap.observe(105, 106, second_started);
        assert_eq!(gap.started, second_started);
        assert_eq!((gap.first_missing, gap.last_missing), (105, 106));
    }

    #[test]
    fn validated_resync_requires_bounded_matching_sequence_and_timestamp_jumps() {
        assert!(validated_discontinuity(
            RingDisposition::Advanced { by: 600 },
            RingDisposition::Advanced {
                by: 600 * FRAMES_PER_PACKET as u64,
            },
        ));
        assert!(!validated_discontinuity(
            RingDisposition::Advanced { by: 600 },
            RingDisposition::Advanced { by: 600 },
        ));
        assert!(!validated_discontinuity(
            RingDisposition::Advanced {
                by: MAX_RESYNC_SEQUENCE_JUMP + 1,
            },
            RingDisposition::Advanced {
                by: (MAX_RESYNC_SEQUENCE_JUMP + 1) * FRAMES_PER_PACKET as u64,
            },
        ));
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_awaits_actor_task_termination() {
        let stopped = Arc::new(AtomicBool::new(false));
        let task_stopped = stopped.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = DropFlag(task_stopped);
            let _ = entered_tx.send(());
            std::thread::sleep(Duration::from_millis(1_200));
            std::future::pending::<()>().await;
        });
        entered_rx.await.unwrap();
        let (commands, _command_rx) = mpsc::channel(1);
        let mut handle = ClassicMediaHandle {
            commands,
            task: Some(task),
        };

        handle.abort().await;
        handle.abort().await;

        assert!(handle.is_finished());
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn endpoint_matching_normalizes_ipv4_mapped_ipv6_and_pins_ports() {
        let v4 = SocketAddr::from(([192, 0, 2, 10], 7000));
        let mapped = SocketAddr::V6(SocketAddrV6::new(
            Ipv4Addr::new(192, 0, 2, 10).to_ipv6_mapped(),
            7000,
            0,
            0,
        ));
        assert!(endpoint_eq(v4, mapped));
        assert!(!endpoint_eq(v4, SocketAddr::from(([192, 0, 2, 10], 7001))));
        assert_ne!(
            normalize_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            normalize_ip(v4.ip())
        );
    }

    #[test]
    fn retransmit_wrapper_and_request_bounds_are_strict() {
        assert_eq!(
            build_retransmit_request(0x1234, 0xfffe, 2),
            [0x80, 0xd5, 0x12, 0x34, 0xff, 0xfe, 0x00, 0x02]
        );
        assert!(RtpPacket::parse(&[0u8; RTP_HEADER_BYTES - 1]).is_err());
        assert_eq!(MAX_AUDIO_DATAGRAM_BYTES, 4108);
    }

    #[test]
    fn ntp_deadline_arithmetic_tracks_rtp_wrap_without_sleeping() {
        let anchor_ns = 10_000_000_000;
        let anchor_rtp = u32::MAX - 1;
        assert_eq!(
            rtp_presentation_ns(anchor_ns, anchor_rtp, anchor_rtp),
            Some(anchor_ns)
        );
        assert_eq!(
            rtp_presentation_ns(anchor_ns, anchor_rtp, 0),
            Some(anchor_ns + 2 * 1_000_000_000 / SAMPLE_RATE as u64)
        );
        assert_eq!(
            rtp_presentation_ns(anchor_ns, 1_000, 1_000 + SAMPLE_RATE),
            Some(anchor_ns + 1_000_000_000)
        );
    }
}
