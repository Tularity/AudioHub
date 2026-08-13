//! Realtime type-96 UDP transport owned by AudioHub.
//!
//! The engine deliberately stops at ordered PCM delivery. The platform audio
//! sink remains responsible for consuming PCM at the device clock; adding a
//! separate sender-side pacer here would create a second clock and eventually
//! drift.

use super::audio_crypto::{AudioDecryptor, MAX_ENCRYPTED_AUDIO_PACKET_BYTES};
use super::clock::{
    build_timing_request, NtpTimestamp, PendingTimingRequest, SyncAnchor, TimingFeedback,
    TimingResponse, TimingWindow,
};
use super::decode::{Type96AlacDecoder, CHANNELS, FRAMES_PER_PACKET};
use super::jitter::JitterBuffer;
use super::packet::{RingDisposition, RtpPacket, SequenceExtender16, TimestampExtender32};
use super::setup::Type96Setup;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant, MissedTickBehavior};

const CONTROL_WRAPPER_BYTES: usize = 4;
const RETRANSMIT_REQUEST_BYTES: usize = 8;
const MAX_RETRANSMIT_PACKETS: u16 = 32;
const RETRANSMIT_BACKOFF: Duration = Duration::from_millis(50);
const RETRANSMIT_WINDOW: Duration = Duration::from_millis(300);
const GAP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const COMMAND_CAPACITY: usize = 4;
const TIMING_RESPONSE_LIFETIME: Duration = Duration::from_secs(1);
const TIMING_WINDOW_AGE_NS: u64 = 15_000_000_000;
const TIMING_MAX_RTT_NS: u64 = 500_000_000;
const STARTUP_PRIME: Duration = Duration::from_millis(24);
const MIN_SYNC_INTERVAL: Duration = Duration::from_millis(10);
const MAX_RETRANSMIT_ATTEMPTS: u8 = 6;
const NONCE_HISTORY: usize = 2_048;

#[cfg(not(test))]
const TIMING_PROBE_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(test)]
const TIMING_PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// PCM consumer supplied by the receiver runtime. Calls are intentionally
/// synchronous and short: the platform bus copies into its own bounded queue.
pub(crate) trait PcmOutput: Send {
    fn write(&mut self, samples: &[i16]);
    fn flush(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Type96Ports {
    pub(crate) data_port: u16,
    pub(crate) control_port: u16,
}

/// The shared timing/control UDP socket is bound during SETUP phase one so the
/// advertised timing port already exists. Phase two consumes this value and
/// keeps using the same socket for d2/d3 and d4/d5/d6.
pub(crate) struct PreparedTiming {
    socket: UdpSocket,
    local_ip: IpAddr,
    peer_ip: IpAddr,
    remote_timing: SocketAddr,
    port: u16,
    initial_probe: Option<InitialTimingProbe>,
}

#[derive(Debug, Clone, Copy)]
struct InitialTimingProbe {
    sequence: u16,
    origin: NtpTimestamp,
    local_departure_ns: u64,
    sent_at: Instant,
}

impl PreparedTiming {
    pub(crate) async fn bind(
        local: SocketAddr,
        peer: SocketAddr,
        remote_timing_port: u16,
    ) -> io::Result<Self> {
        validate_endpoints(local, peer, remote_timing_port)?;
        let socket = UdpSocket::bind(with_port(local, 0)).await?;
        let local_address = socket.local_addr()?;
        let initial_probe = if let Some(transmit) = system_ntp_now() {
            let sequence = 1;
            let packet = build_timing_request(sequence, transmit, None);
            let sent_at = Instant::now();
            socket
                .send_to(&packet, with_port(peer, remote_timing_port))
                .await?;
            Some(InitialTimingProbe {
                sequence,
                origin: transmit,
                local_departure_ns: transmit.to_nanos(),
                sent_at,
            })
        } else {
            None
        };
        Ok(Self {
            socket,
            local_ip: local_address.ip(),
            peer_ip: peer.ip(),
            remote_timing: with_port(peer, remote_timing_port),
            port: local_address.port(),
            initial_probe,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

/// A fully validated and pre-bound type-96 stream. No UDP bind, decoder
/// construction, or clock-window allocation is deferred until `start`.
pub(crate) struct PreparedType96 {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    peer_ip: IpAddr,
    remote_timing: SocketAddr,
    advertised_control_target: Option<SocketAddr>,
    initial_timing_probe: Option<InitialTimingProbe>,
    ports: Type96Ports,
    decryptor: AudioDecryptor,
    decoder: Type96AlacDecoder,
    jitter: JitterBuffer,
    timing_window: TimingWindow,
}

impl PreparedType96 {
    pub(crate) async fn prepare(
        local: SocketAddr,
        peer: SocketAddr,
        remote_timing_port: u16,
        setup: Type96Setup,
    ) -> io::Result<Self> {
        let timing = PreparedTiming::bind(local, peer, remote_timing_port).await?;
        Self::prepare_with_timing(local, peer, timing, setup).await
    }

    pub(crate) async fn prepare_with_timing(
        local: SocketAddr,
        peer: SocketAddr,
        timing: PreparedTiming,
        setup: Type96Setup,
    ) -> io::Result<Self> {
        validate_endpoints(local, peer, timing.remote_timing.port())?;
        if timing.peer_ip != peer.ip() || timing.local_ip != local.ip() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "phase-one timing socket does not belong to this media connection",
            ));
        }

        let bind_address = with_port(local, 0);
        let data_socket = UdpSocket::bind(bind_address).await?;
        let data_port = data_socket.local_addr()?.port();
        let control_port = timing.port;

        // Build every state object that can fail before SETUP is acknowledged.
        let decoder = Type96AlacDecoder::new()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let timing_window =
            TimingWindow::new(8, TIMING_WINDOW_AGE_NS, TIMING_MAX_RTT_NS).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid type-96 clock limits: {error:?}"),
                )
            })?;
        let decryptor = AudioDecryptor::new(*setup.shared_key());
        let advertised_control_target = setup.remote_control_port.map(|port| with_port(peer, port));

        Ok(Self {
            data_socket,
            control_socket: timing.socket,
            peer_ip: peer.ip(),
            remote_timing: timing.remote_timing,
            advertised_control_target,
            initial_timing_probe: timing.initial_probe,
            ports: Type96Ports {
                data_port,
                control_port,
            },
            decryptor,
            decoder,
            jitter: JitterBuffer::type96(),
            timing_window,
        })
    }

    pub(crate) fn ports(&self) -> Type96Ports {
        self.ports
    }

    pub(crate) fn start(self, output: Box<dyn PcmOutput>) -> Type96MediaHandle {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let task = tokio::spawn(async move {
            let engine = Type96Engine::new(self, output, command_rx);
            match engine.run().await {
                RunExit::Shutdown(reply) => {
                    // `engine` (including its decryptor and sink) was dropped
                    // on return from `run` before this acknowledgement.
                    let _ = reply.send(());
                }
                RunExit::Io(error) => log::warn!("AirPlay 2 media task stopped: {error}"),
            }
        });
        Type96MediaHandle { commands, task }
    }
}

pub(crate) struct Type96MediaHandle {
    commands: mpsc::Sender<MediaCommand>,
    task: JoinHandle<()>,
}

impl Type96MediaHandle {
    pub(crate) async fn flush(&self) -> io::Result<()> {
        let (reply, done) = oneshot::channel();
        self.commands
            .send(MediaCommand::Flush(reply))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "media task has stopped"))?;
        done.await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "media task has stopped"))
    }

    pub(crate) async fn abort(&self) {
        if self.task.is_finished() {
            return;
        }
        let (reply, done) = oneshot::channel();
        if self
            .commands
            .send(MediaCommand::Shutdown(reply))
            .await
            .is_ok()
            && time::timeout(Duration::from_secs(1), done).await.is_ok()
        {
            return;
        }
        self.task.abort();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for Type96MediaHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum MediaCommand {
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

enum RunExit {
    Shutdown(oneshot::Sender<()>),
    Io(io::Error),
}

#[derive(Debug)]
struct GapWait {
    first_missing: u64,
    started: Instant,
    last_request: Option<Instant>,
    request_count: u8,
}

struct Type96Engine {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    peer_ip: IpAddr,
    advertised_control_target: Option<SocketAddr>,
    observed_control_source: Option<SocketAddr>,
    remote_timing: SocketAddr,
    data_source: Option<SocketAddr>,
    stream_ssrc: Option<u32>,
    stream_origin: Option<(u64, u64)>,
    decryptor: AudioDecryptor,
    decoder: Type96AlacDecoder,
    jitter: JitterBuffer,
    sequence: SequenceExtender16,
    timestamp: TimestampExtender32,
    timing_window: TimingWindow,
    pending_timing: Option<PendingTimingRequest>,
    timing_feedback: Option<TimingFeedback>,
    timing_sequence: u16,
    last_anchor: Option<SyncAnchor>,
    last_local_anchor_ns: Option<u64>,
    last_sync: Option<Instant>,
    recent_nonces: VecDeque<[u8; 8]>,
    nonce_index: HashSet<[u8; 8]>,
    startup_deadline: Option<Instant>,
    delivery_started: bool,
    gap: Option<GapWait>,
    retransmit_sequence: u16,
    monotonic_origin: Instant,
    output: Box<dyn PcmOutput>,
    commands: mpsc::Receiver<MediaCommand>,
}

impl Type96Engine {
    fn new(
        prepared: PreparedType96,
        output: Box<dyn PcmOutput>,
        commands: mpsc::Receiver<MediaCommand>,
    ) -> Self {
        let monotonic_origin = Instant::now();
        let initial_timing_probe = prepared.initial_timing_probe;
        let pending_timing = initial_timing_probe.and_then(|probe| {
            let expires_at = probe.sent_at.checked_add(TIMING_RESPONSE_LIFETIME)?;
            let remaining = expires_at.saturating_duration_since(monotonic_origin);
            if remaining.is_zero() {
                return None;
            }
            Some(PendingTimingRequest {
                sequence: probe.sequence,
                origin: probe.origin,
                local_departure_ns: probe.local_departure_ns,
                expires_at_monotonic_ns: duration_nanos(remaining),
            })
        });
        Self {
            data_socket: prepared.data_socket,
            control_socket: prepared.control_socket,
            peer_ip: prepared.peer_ip,
            advertised_control_target: prepared.advertised_control_target,
            // The SETUP controlPort is a d5 destination, not proof of which
            // source port the sender will use for d4/d6. Observe that source
            // independently from the first valid control datagram.
            observed_control_source: None,
            remote_timing: prepared.remote_timing,
            data_source: None,
            stream_ssrc: None,
            stream_origin: None,
            decryptor: prepared.decryptor,
            decoder: prepared.decoder,
            jitter: prepared.jitter,
            sequence: SequenceExtender16::new(),
            timestamp: TimestampExtender32::new(),
            timing_window: prepared.timing_window,
            pending_timing,
            timing_feedback: None,
            timing_sequence: initial_timing_probe.map_or(1, |probe| probe.sequence.wrapping_add(1)),
            last_anchor: None,
            last_local_anchor_ns: None,
            last_sync: None,
            recent_nonces: VecDeque::with_capacity(NONCE_HISTORY),
            nonce_index: HashSet::with_capacity(NONCE_HISTORY),
            startup_deadline: None,
            delivery_started: false,
            gap: None,
            retransmit_sequence: 1,
            monotonic_origin,
            output,
            commands,
        }
    }

    async fn run(mut self) -> RunExit {
        let mut data = [0u8; MAX_ENCRYPTED_AUDIO_PACKET_BYTES + 1];
        let mut control = [0u8; CONTROL_WRAPPER_BYTES + MAX_ENCRYPTED_AUDIO_PACKET_BYTES + 1];
        let mut gap_tick = time::interval(GAP_POLL_INTERVAL);
        gap_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut timing_tick = time::interval_at(Instant::now(), TIMING_PROBE_INTERVAL);
        timing_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                received = self.data_socket.recv_from(&mut data) => {
                    let (length, source) = match received {
                        Ok(value) => value,
                        Err(error) => return RunExit::Io(error),
                    };
                    if source.ip() == self.peer_ip
                        && length <= MAX_ENCRYPTED_AUDIO_PACKET_BYTES
                        && self.data_source.is_none_or(|pinned| pinned == source)
                    {
                        self.accept_encrypted_audio(&data[..length], Some(source));
                    }
                }
                received = self.control_socket.recv_from(&mut control) => {
                    let (length, source) = match received {
                        Ok(value) => value,
                        Err(error) => return RunExit::Io(error),
                    };
                    if source.ip() == self.peer_ip {
                        self.accept_control(&control[..length], source);
                    }
                }
                _ = gap_tick.tick() => {
                    self.service_media_tick().await;
                }
                _ = timing_tick.tick() => {
                    self.send_timing_probe().await;
                }
                command = self.commands.recv() => {
                    match command {
                        Some(MediaCommand::Flush(reply)) => {
                            self.flush_stream();
                            let _ = reply.send(());
                        }
                        Some(MediaCommand::Shutdown(reply)) => {
                            self.output.flush();
                            return RunExit::Shutdown(reply);
                        }
                        None => {
                            // All handles disappeared. Returning drops the key
                            // and the sink even if the JoinHandle was detached.
                            self.output.flush();
                            return RunExit::Io(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "media control channel closed",
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Returns true once the datagram has authenticated and has a valid RTP
    /// shape. A duplicate or late packet is still suitable for endpoint
    /// pinning because its AEAD authentication binds it to this stream.
    fn accept_encrypted_audio(&mut self, datagram: &[u8], data_source: Option<SocketAddr>) -> bool {
        let clear = match self.decryptor.decrypt(datagram) {
            Ok(clear) => clear,
            Err(_) => return false,
        };
        let nonce: [u8; 8] = datagram[datagram.len() - 8..]
            .try_into()
            .expect("the decryptor accepted a transmitted nonce");
        if self.nonce_index.contains(&nonce) {
            return false;
        }
        let mut packet_bytes = Vec::with_capacity(clear.rtp_header.len() + clear.payload.len());
        packet_bytes.extend_from_slice(&clear.rtp_header);
        packet_bytes.extend_from_slice(&clear.payload);
        let parsed = match RtpPacket::parse(&packet_bytes) {
            Ok(packet) if packet.payload_type == 96 => packet,
            _ => return false,
        };

        if self.stream_ssrc.is_some_and(|ssrc| ssrc != parsed.ssrc) {
            return false;
        }

        let sequence_preview = match self.sequence.peek(parsed.sequence) {
            Ok(preview) if preview.disposition != RingDisposition::Duplicate => preview,
            _ => return false,
        };
        match sequence_preview.disposition {
            RingDisposition::Advanced { by } if by > super::jitter::DEFAULT_REORDER_WINDOW => {
                return false
            }
            RingDisposition::Reordered { behind }
                if behind > super::jitter::DEFAULT_REORDER_WINDOW.saturating_mul(2) =>
            {
                return false
            }
            _ => {}
        }
        let timestamp_preview = match self.timestamp.peek(parsed.timestamp) {
            Ok(preview) if preview.disposition != RingDisposition::Duplicate => preview,
            _ => return false,
        };
        let timestamp_limit = super::jitter::DEFAULT_REORDER_WINDOW
            .saturating_mul(FRAMES_PER_PACKET as u64)
            .saturating_mul(2);
        match timestamp_preview.disposition {
            RingDisposition::Advanced { by } | RingDisposition::Reordered { behind: by }
                if by > timestamp_limit =>
            {
                return false
            }
            _ => {}
        }
        let sequence_candidate = sequence_preview.value;
        let timestamp_candidate = timestamp_preview.value;
        if let Some((origin_sequence, origin_timestamp)) = self.stream_origin {
            let sequence_delta = sequence_candidate as i128 - origin_sequence as i128;
            let timestamp_delta = timestamp_candidate as i128 - origin_timestamp as i128;
            if timestamp_delta != sequence_delta * FRAMES_PER_PACKET as i128 {
                return false;
            }
        }

        let payload = parsed.payload.to_vec();
        if self
            .jitter
            .insert(sequence_candidate, timestamp_candidate, payload)
            .is_err()
        {
            return false;
        }
        // The engine is single-threaded. Nothing between preview and commit
        // mutates either extender; jitter insertion is the last fallible
        // admission step for untrusted packet numbering.
        self.sequence
            .commit(parsed.sequence, sequence_preview)
            .expect("sequence preview remains current after jitter insertion");
        self.timestamp
            .commit(parsed.timestamp, timestamp_preview)
            .expect("timestamp preview remains current after jitter insertion");
        // A packet rejected by the bounded jitter policy may later be the
        // legitimate body of a d6 retransmission. Mark the nonce only once all
        // packet-number state is committed; admitted replays are rejected.
        self.remember_nonce(nonce);
        self.stream_ssrc.get_or_insert(parsed.ssrc);
        self.stream_origin
            .get_or_insert((sequence_candidate, timestamp_candidate));
        if let Some(source) = data_source {
            self.data_source.get_or_insert(source);
        }
        if self.startup_deadline.is_none() {
            self.startup_deadline = Some(Instant::now() + STARTUP_PRIME);
        }
        if self.delivery_started {
            self.drain_ready();
        }
        true
    }

    fn accept_control(&mut self, datagram: &[u8], source: SocketAddr) {
        if datagram.len() < 2 {
            return;
        }
        match datagram[1] {
            0xd3 if source.port() == self.remote_timing.port() => {
                self.accept_timing_response(datagram)
            }
            0xd4 if self.control_source_allowed(source)
                && self
                    .last_sync
                    .is_none_or(|last| last.elapsed() >= MIN_SYNC_INTERVAL) =>
            {
                if let Ok(anchor) = SyncAnchor::parse(datagram) {
                    self.observed_control_source.get_or_insert(source);
                    self.last_sync = Some(Instant::now());
                    self.last_anchor = Some(anchor);
                    self.refresh_local_anchor();
                }
            }
            0xd6 if self.control_source_allowed(source)
                && datagram.len() > CONTROL_WRAPPER_BYTES
                && datagram[0] == 0x80
                && self.accept_encrypted_audio(&datagram[CONTROL_WRAPPER_BYTES..], None) =>
            {
                // The d6 prefix is exactly four bytes: RTP-like type and the
                // retransmission request sequence, followed by the original
                // encrypted RTP datagram without any extra header.
                self.observed_control_source.get_or_insert(source);
            }
            _ => {}
        }
    }

    fn control_source_allowed(&self, source: SocketAddr) -> bool {
        self.observed_control_source
            .is_none_or(|pinned| pinned == source)
    }

    fn remember_nonce(&mut self, nonce: [u8; 8]) {
        if self.recent_nonces.len() == NONCE_HISTORY {
            if let Some(oldest) = self.recent_nonces.pop_front() {
                self.nonce_index.remove(&oldest);
            }
        }
        self.recent_nonces.push_back(nonce);
        self.nonce_index.insert(nonce);
    }

    fn accept_timing_response(&mut self, datagram: &[u8]) {
        let Some(pending) = self.pending_timing else {
            return;
        };
        let response = match TimingResponse::parse(datagram, None) {
            Ok(response) => response,
            Err(_) => return,
        };
        let Some(arrival) = system_ntp_now() else {
            return;
        };
        let monotonic = self.monotonic_ns();
        // Implementations disagree about whether d3 echoes the d2 header
        // sequence (some reply with zero). The exact, unpredictable NTP origin
        // timestamp is the correlation token; endpoint and monotonic lifetime
        // were already checked by the receive path.
        if response.origin != pending.origin || monotonic > pending.expires_at_monotonic_ns {
            return;
        }
        let sample = match super::clock::TimingSample::from_exchange(
            pending.local_departure_ns,
            response,
            arrival.to_nanos(),
        ) {
            Ok(sample) => sample,
            Err(_) => return,
        };
        if self.timing_window.insert(sample, monotonic).is_err() {
            return;
        }
        self.pending_timing = None;
        self.timing_feedback = Some(TimingFeedback {
            remote_reference: response.remote_transmit,
            local_receive: arrival,
        });
        self.refresh_local_anchor();
    }

    fn refresh_local_anchor(&mut self) {
        let Some(anchor) = self.last_anchor else {
            return;
        };
        let Some(sample) = self.timing_window.best(self.monotonic_ns()) else {
            return;
        };
        self.last_local_anchor_ns = anchor.local_anchor_ns(sample);
    }

    fn drain_ready(&mut self) {
        while let Some(packet) = self.jitter.pop_ready() {
            match self.decoder.decode_packet(packet.payload()) {
                Ok(pcm) => self.output.write(pcm.samples()),
                Err(_) => self.write_silence(1),
            }
        }
        if self.jitter.missing_ranges().is_empty() {
            self.gap = None;
        }
    }

    async fn service_media_tick(&mut self) {
        if !self.delivery_started
            && self
                .startup_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.delivery_started = true;
            self.jitter.finish_priming();
            self.drain_ready();
        }
        if self.delivery_started {
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
            self.gap = Some(GapWait {
                first_missing: missing.first,
                started: now,
                last_request: None,
                request_count: 0,
            });
        } else if let Some(gap) = &mut self.gap {
            if gap.first_missing != missing.first {
                // Filling part of a range must not restart the overall 300 ms
                // loss window, but the next d5 should describe the new head.
                gap.first_missing = missing.first;
                gap.last_request = None;
                gap.request_count = 0;
            }
        }

        let expired = self
            .gap
            .as_ref()
            .is_some_and(|gap| now.duration_since(gap.started) >= RETRANSMIT_WINDOW);
        if expired {
            let skipped = missing.packet_count();
            let new_next = missing.last.saturating_add(1);
            if self.jitter.advance_to(new_next).is_ok() {
                self.write_silence(skipped);
                self.gap = None;
                self.drain_ready();
            }
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
        let Some(remote_control) = self
            .observed_control_source
            .or(self.advertised_control_target)
        else {
            return;
        };
        let request =
            build_retransmit_request(self.retransmit_sequence, missing.first as u16, count);
        if self
            .control_socket
            .send_to(&request, remote_control)
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

    async fn send_timing_probe(&mut self) {
        let monotonic = self.monotonic_ns();
        if self
            .pending_timing
            .is_some_and(|pending| monotonic <= pending.expires_at_monotonic_ns)
        {
            // Keep one origin token outstanding at a time. Replacing it before
            // d3 arrives would make the valid response impossible to correlate
            // and would also emit a request without the previous exchange's
            // feedback timestamps.
            return;
        }
        self.pending_timing = None;

        let Some(transmit) = system_ntp_now() else {
            return;
        };
        let packet = build_timing_request(self.timing_sequence, transmit, self.timing_feedback);
        if self
            .control_socket
            .send_to(&packet, self.remote_timing)
            .await
            .is_err()
        {
            return;
        }
        self.pending_timing = Some(PendingTimingRequest {
            sequence: self.timing_sequence,
            origin: transmit,
            local_departure_ns: transmit.to_nanos(),
            expires_at_monotonic_ns: monotonic
                .saturating_add(duration_nanos(TIMING_RESPONSE_LIFETIME)),
        });
        self.timing_sequence = self.timing_sequence.wrapping_add(1);
    }

    fn write_silence(&mut self, packet_count: u64) {
        let samples_per_packet = FRAMES_PER_PACKET.saturating_mul(CHANNELS);
        let maximum_packets = super::jitter::DEFAULT_REORDER_WINDOW.saturating_add(1);
        let packet_count = packet_count.min(maximum_packets);
        let sample_count = usize::try_from(packet_count)
            .ok()
            .and_then(|count| count.checked_mul(samples_per_packet))
            .unwrap_or(samples_per_packet);
        self.output.write(&vec![0i16; sample_count]);
    }

    fn flush_stream(&mut self) {
        self.jitter = JitterBuffer::type96();
        self.sequence = SequenceExtender16::new();
        self.timestamp = TimestampExtender32::new();
        self.stream_ssrc = None;
        self.stream_origin = None;
        self.gap = None;
        self.startup_deadline = None;
        self.delivery_started = false;
        self.last_anchor = None;
        self.last_local_anchor_ns = None;
        self.output.flush();
    }

    fn monotonic_ns(&self) -> u64 {
        duration_nanos(self.monotonic_origin.elapsed())
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn validate_endpoints(
    local: SocketAddr,
    peer: SocketAddr,
    remote_timing_port: u16,
) -> io::Result<()> {
    if local.is_ipv4() != peer.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local and peer media addresses use different IP families",
        ));
    }
    if remote_timing_port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the sender timing port must be non-zero",
        ));
    }
    Ok(())
}

fn system_ntp_now() -> Option<NtpTimestamp> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    NtpTimestamp::from_unix_nanos(u64::try_from(nanos).ok()?)
}

fn build_retransmit_request(sequence: u16, first_missing: u16, count: u16) -> [u8; 8] {
    debug_assert!(count > 0 && count <= MAX_RETRANSMIT_PACKETS);
    let mut packet = [0u8; RETRANSMIT_REQUEST_BYTES];
    packet[0] = 0x80;
    packet[1] = 0xd5;
    packet[2..4].copy_from_slice(&sequence.to_be_bytes());
    packet[4..6].copy_from_slice(&first_missing.to_be_bytes());
    packet[6..8].copy_from_slice(&count.to_be_bytes());
    packet
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
    use chacha20poly1305::aead::{Aead, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
    use plist::{Dictionary, Value};
    use std::sync::{Arc, Mutex};

    const KEY: [u8; 32] = [0x42; 32];
    const SSRC: u32 = 0x1020_3040;
    const SILENCE_352: &str = concat!(
        "200010000002c000000f0801000000000000000f080100000000000000",
        "ff80afbfe02bfc"
    );

    #[derive(Default)]
    struct OutputState {
        samples: Vec<i16>,
        writes: usize,
        flushes: usize,
        dropped: bool,
    }

    struct TestOutput(Arc<Mutex<OutputState>>);

    impl PcmOutput for TestOutput {
        fn write(&mut self, samples: &[i16]) {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            state.samples.extend_from_slice(samples);
        }

        fn flush(&mut self) {
            self.0.lock().unwrap().flushes += 1;
        }
    }

    impl Drop for TestOutput {
        fn drop(&mut self) {
            self.0.lock().unwrap().dropped = true;
        }
    }

    struct Harness {
        data: UdpSocket,
        control: UdpSocket,
        timing: UdpSocket,
        ports: Type96Ports,
        state: Arc<Mutex<OutputState>>,
        handle: Type96MediaHandle,
    }

    async fn harness() -> Harness {
        let data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let timing = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = timing.local_addr().unwrap();
        let prepared = PreparedType96::prepare(
            "127.0.0.1:0".parse().unwrap(),
            peer,
            peer.port(),
            type96_setup(),
        )
        .await
        .unwrap();
        let ports = prepared.ports();
        let state = Arc::new(Mutex::new(OutputState::default()));
        let handle = prepared.start(Box::new(TestOutput(state.clone())));
        Harness {
            data,
            control,
            timing,
            ports,
            state,
            handle,
        }
    }

    fn type96_setup() -> Type96Setup {
        type96_setup_with_control(None)
    }

    fn type96_setup_with_control(control_port: Option<u16>) -> Type96Setup {
        let mut stream: Dictionary = [
            ("type", Value::from(96u64)),
            ("audioFormat", Value::from(0x0004_0000u64)),
            ("ct", Value::from(2u64)),
            ("spf", Value::from(352u64)),
            ("sr", Value::from(44_100u64)),
            ("shk", Value::Data(KEY.to_vec())),
        ]
        .into_iter()
        .collect();
        if let Some(port) = control_port {
            stream.insert("controlPort".into(), Value::from(port as u64));
        }
        let root: Dictionary = [("streams", Value::Array(vec![Value::Dictionary(stream)]))]
            .into_iter()
            .collect();
        let mut body = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut body).unwrap();
        super::super::setup::parse_phase2(&body).unwrap()
    }

    fn encrypted_packet(sequence: u16, timestamp: u32, ssrc: u32, nonce_suffix: u64) -> Vec<u8> {
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = 0xe0;
        header[2..4].copy_from_slice(&sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&ssrc.to_be_bytes());
        let suffix = nonce_suffix.to_be_bytes();
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&suffix);
        let encrypted = ChaCha20Poly1305::new((&KEY).into())
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &hex::decode(SILENCE_352).unwrap(),
                    aad: &header[4..],
                },
            )
            .unwrap();
        let mut packet = header.to_vec();
        packet.extend_from_slice(&encrypted);
        packet.extend_from_slice(&suffix);
        packet
    }

    fn target(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) {
        time::timeout(Duration::from_secs(2), async {
            while !condition() {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition was not reached before timeout");
    }

    fn valid_sync() -> [u8; 20] {
        let mut packet = [0u8; 20];
        packet[0] = 0x90;
        packet[1] = 0xd4;
        packet[4..8].copy_from_slice(&100u32.to_be_bytes());
        packet[8..16].copy_from_slice(&(1u64 << 32).to_be_bytes());
        packet[16..20].copy_from_slice(&452u32.to_be_bytes());
        packet
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filters_sources_primes_reordering_and_pins_stream_identity() {
        let harness = harness().await;
        assert_ne!(harness.ports.data_port, 0);
        assert_ne!(harness.ports.control_port, 0);
        assert_ne!(harness.ports.data_port, harness.ports.control_port);

        // N+1 may arrive before N. Startup priming gives the bounded jitter
        // buffer a chance to move its not-yet-consumed baseline backwards.
        harness
            .data
            .send_to(
                &encrypted_packet(101, 352, SSRC, 2),
                target(harness.ports.data_port),
            )
            .await
            .unwrap();
        harness
            .data
            .send_to(
                &encrypted_packet(100, 0, SSRC, 3),
                target(harness.ports.data_port),
            )
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 2).await;

        // After the first authenticated packet, a second source port cannot
        // inject even a correctly encrypted stream packet.
        let other_port = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sequence_102 = encrypted_packet(102, 704, SSRC, 4);
        other_port
            .send_to(&sequence_102, target(harness.ports.data_port))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(25)).await;
        assert_eq!(harness.state.lock().unwrap().writes, 2);
        harness
            .data
            .send_to(&sequence_102, target(harness.ports.data_port))
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 3).await;

        // Replaying the nonce, changing SSRC, and breaking the exact 352-frame
        // timestamp progression are all rejected before extender state moves.
        harness
            .data
            .send_to(&sequence_102, target(harness.ports.data_port))
            .await
            .unwrap();
        harness
            .data
            .send_to(
                &encrypted_packet(103, 1_056, SSRC ^ 1, 5),
                target(harness.ports.data_port),
            )
            .await
            .unwrap();
        harness
            .data
            .send_to(
                &encrypted_packet(103, 999_999, SSRC, 6),
                target(harness.ports.data_port),
            )
            .await
            .unwrap();
        time::sleep(Duration::from_millis(25)).await;
        assert_eq!(harness.state.lock().unwrap().writes, 3);
        harness
            .data
            .send_to(
                &encrypted_packet(103, 1_056, SSRC, 7),
                target(harness.ports.data_port),
            )
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 4).await;
        assert!(harness
            .state
            .lock()
            .unwrap()
            .samples
            .iter()
            .all(|sample| *sample == 0));
        harness.handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_real_udp_from_an_unexpected_peer_ip() {
        // The engine expects 127.0.0.2 but the actual sender is 127.0.0.1.
        // Nothing needs to bind the alias, keeping this portable to macOS while
        // still exercising recv_from's real source address and the filter.
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer: SocketAddr = "127.0.0.2:7001".parse().unwrap();
        let prepared = PreparedType96::prepare(
            "127.0.0.1:0".parse().unwrap(),
            peer,
            peer.port(),
            type96_setup(),
        )
        .await
        .unwrap();
        let data_port = prepared.ports().data_port;
        let state = Arc::new(Mutex::new(OutputState::default()));
        let handle = prepared.start(Box::new(TestOutput(state.clone())));
        sender
            .send_to(&encrypted_packet(1, 0, SSRC, 1), target(data_port))
            .await
            .unwrap();
        time::sleep(STARTUP_PRIME + Duration::from_millis(20)).await;
        assert_eq!(state.lock().unwrap().writes, 0);
        handle.abort().await;
        wait_for(|| state.lock().unwrap().dropped).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepts_exact_d6_and_bounds_d5_retransmission_requests() {
        let harness = harness().await;
        let data_target = target(harness.ports.data_port);
        let control_target = target(harness.ports.control_port);

        // d4 is unencrypted, so it may establish the same-IP control endpoint
        // but all later control datagrams must use this exact source address.
        harness
            .control
            .send_to(&valid_sync(), control_target)
            .await
            .unwrap();
        // UDP send completion does not mean the receiver task has consumed
        // the d4. Wait for an observable d5 after creating a first gap before
        // racing the d6 source-pinning assertions below.
        harness
            .data
            .send_to(&encrypted_packet(20, 0, SSRC, 20), data_target)
            .await
            .unwrap();
        harness
            .data
            .send_to(&encrypted_packet(22, 704, SSRC, 22), data_target)
            .await
            .unwrap();

        let mut initial_request = [0u8; 8];
        let (initial_length, _) = time::timeout(
            Duration::from_millis(150),
            harness.control.recv_from(&mut initial_request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(initial_length, 8);
        assert_eq!(&initial_request[..2], [0x80, 0xd5]);
        assert_eq!(
            u16::from_be_bytes([initial_request[4], initial_request[5]]),
            21
        );

        let retransmitted = encrypted_packet(21, 352, SSRC, 21);
        let mut wrong_wrapper = vec![0x80, 0xd6, 0, 1, 0, 0];
        wrong_wrapper.extend_from_slice(&retransmitted);
        harness
            .control
            .send_to(&wrong_wrapper, control_target)
            .await
            .unwrap();
        let other_control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut exact = vec![0x80, 0xd6, 0, 1];
        exact.extend_from_slice(&retransmitted);
        other_control.send_to(&exact, control_target).await.unwrap();
        time::sleep(Duration::from_millis(15)).await;
        // Packet 20 may already have crossed the 24 ms startup prime while we
        // waited for d5. The wrong wrapper and wrong source must not fill 21,
        // so packet 22 remains blocked and at most one write is observable.
        assert!(harness.state.lock().unwrap().writes <= 1);
        harness
            .control
            .send_to(&exact, control_target)
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 3).await;

        // A d5 generated just before the accepted d6 can already be queued in
        // the sender socket even though the gap is now filled. Drain only
        // those stale requests before opening the next, independently
        // asserted loss window.
        let mut stale = [0u8; RETRANSMIT_REQUEST_BYTES];
        while let Ok(Ok((length, _))) = time::timeout(
            Duration::from_millis(10),
            harness.control.recv_from(&mut stale),
        )
        .await
        {
            assert_eq!(length, RETRANSMIT_REQUEST_BYTES);
            assert_eq!(&stale[..2], [0x80, 0xd5]);
            assert_eq!(u16::from_be_bytes([stale[4], stale[5]]), 21);
        }

        // Leave packet 23 missing. Requests are exactly eight bytes, cover at
        // most 32 packets, and are separated by the 50 ms loss backoff.
        harness
            .data
            .send_to(&encrypted_packet(24, 1_408, SSRC, 24), data_target)
            .await
            .unwrap();
        let collection_deadline = Instant::now() + Duration::from_millis(180);
        let mut request_times = Vec::new();
        let mut buffer = [0u8; 64];
        while Instant::now() < collection_deadline {
            let remaining = collection_deadline.saturating_duration_since(Instant::now());
            let received = time::timeout(remaining, harness.control.recv_from(&mut buffer)).await;
            let Ok(Ok((length, _))) = received else {
                break;
            };
            assert_eq!(length, RETRANSMIT_REQUEST_BYTES);
            assert_eq!(&buffer[..2], [0x80, 0xd5]);
            assert_eq!(u16::from_be_bytes([buffer[4], buffer[5]]), 23);
            assert!(
                (1..=MAX_RETRANSMIT_PACKETS).contains(&u16::from_be_bytes([buffer[6], buffer[7]]))
            );
            request_times.push(Instant::now());
        }
        assert!((2..=4).contains(&request_times.len()), "{request_times:?}");
        for pair in request_times.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= Duration::from_millis(45));
        }

        // The stream cannot stall forever: after the bounded retransmission
        // window one packet of stereo silence is inserted, then 24 is decoded.
        wait_for(|| harness.state.lock().unwrap().writes == 5).await;
        assert_eq!(
            harness.state.lock().unwrap().samples.len(),
            5 * FRAMES_PER_PACKET * CHANNELS
        );
        harness.handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn advertised_d5_target_is_distinct_from_observed_control_source() {
        let data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let timing = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = timing.local_addr().unwrap();
        let prepared = PreparedType96::prepare(
            "127.0.0.1:0".parse().unwrap(),
            peer,
            peer.port(),
            type96_setup_with_control(Some(control.local_addr().unwrap().port())),
        )
        .await
        .unwrap();
        let ports = prepared.ports();
        let state = Arc::new(Mutex::new(OutputState::default()));
        let handle = prepared.start(Box::new(TestOutput(state.clone())));

        data.send_to(&encrypted_packet(30, 0, SSRC, 30), target(ports.data_port))
            .await
            .unwrap();
        data.send_to(
            &encrypted_packet(32, 704, SSRC, 32),
            target(ports.data_port),
        )
        .await
        .unwrap();

        let mut request = [0u8; 8];
        let (length, _) =
            time::timeout(Duration::from_millis(150), control.recv_from(&mut request))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(length, 8);
        assert_eq!(&request[..2], [0x80, 0xd5]);
        assert_eq!(u16::from_be_bytes([request[4], request[5]]), 31);

        // SETUP controlPort says where d5 goes; it does not promise that d4/d6
        // originate there. A different same-peer-IP port can be observed and
        // pinned after a structurally valid d4.
        let actual_control_source = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        actual_control_source
            .send_to(&valid_sync(), target(ports.control_port))
            .await
            .unwrap();
        let mut d6 = vec![0x80, 0xd6, 0, 1];
        d6.extend_from_slice(&encrypted_packet(31, 352, SSRC, 31));
        actual_control_source
            .send_to(&d6, target(ports.control_port))
            .await
            .unwrap();
        wait_for(|| state.lock().unwrap().writes == 3).await;

        // Once observed, another port (including the advertised d5 target)
        // cannot inject a subsequent control datagram.
        let mut next = vec![0x80, 0xd6, 0, 2];
        next.extend_from_slice(&encrypted_packet(33, 1_056, SSRC, 33));
        control
            .send_to(&next, target(ports.control_port))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(25)).await;
        assert_eq!(state.lock().unwrap().writes, 3);
        handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn phase_one_bind_sends_d2_from_the_advertised_timing_port() {
        let timing_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = timing_server.local_addr().unwrap();
        let timing = PreparedTiming::bind("127.0.0.1:0".parse().unwrap(), peer, peer.port())
            .await
            .unwrap();
        let advertised = timing.port();
        let mut packet = [0u8; 32];
        let (length, source) = time::timeout(
            Duration::from_millis(150),
            timing_server.recv_from(&mut packet),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, 32);
        assert_eq!(&packet[..2], [0x80, 0xd2]);
        assert_eq!(source.port(), advertised);
        assert!(packet[24..32].iter().any(|byte| *byte != 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn d2_uses_ntp_transmit_and_returns_previous_exchange_feedback() {
        let harness = harness().await;
        let mut request = [0u8; 64];
        let (length, receiver) = time::timeout(
            Duration::from_secs(1),
            harness.timing.recv_from(&mut request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, 32);
        assert_eq!(&request[..2], [0x80, 0xd2]);
        assert!(request[8..24].iter().all(|byte| *byte == 0));
        assert!(request[24..32].iter().any(|byte| *byte != 0));

        let first_transmit: [u8; 8] = request[24..32].try_into().unwrap();
        // The engine tick fires every 100 ms in tests. Crossing one complete
        // tick here proves it does not replace an unexpired outstanding origin
        // with a second request before the matching d3 can arrive.
        assert!(time::timeout(
            Duration::from_millis(125),
            harness.timing.recv_from(&mut request),
        )
        .await
        .is_err());

        let mut response = [0u8; 32];
        response[0] = 0x80;
        response[1] = 0xd3;
        response[2..4].copy_from_slice(&request[2..4]);
        response[8..16].copy_from_slice(&first_transmit);
        response[16..24].copy_from_slice(&first_transmit);
        response[24..32].copy_from_slice(&first_transmit);
        harness.timing.send_to(&response, receiver).await.unwrap();

        let (length, _) = time::timeout(
            Duration::from_secs(1),
            harness.timing.recv_from(&mut request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(length, 32);
        assert_eq!(&request[8..16], &first_transmit);
        assert!(request[16..24].iter().any(|byte| *byte != 0));
        assert_ne!(&request[24..32], &first_transmit);
        harness.handle.abort().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flush_resets_stream_order_and_abort_drops_the_sink() {
        let harness = harness().await;
        let target = target(harness.ports.data_port);
        harness
            .data
            .send_to(&encrypted_packet(500, 10_000, SSRC, 500), target)
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 1).await;

        harness.handle.flush().await.unwrap();
        assert_eq!(harness.state.lock().unwrap().flushes, 1);
        harness
            .data
            .send_to(&encrypted_packet(1, 0, SSRC ^ 1, 501), target)
            .await
            .unwrap();
        wait_for(|| harness.state.lock().unwrap().writes == 2).await;
        harness.handle.abort().await;
        wait_for(|| harness.state.lock().unwrap().dropped).await;
        wait_for(|| harness.handle.is_finished()).await;
        assert_eq!(harness.state.lock().unwrap().flushes, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_handle_cancels_task_and_drops_output() {
        let harness = harness().await;
        let state = harness.state.clone();
        drop(harness.handle);
        wait_for(|| state.lock().unwrap().dropped).await;
    }

    #[test]
    fn retransmit_packet_and_wrap_helpers_are_strict() {
        assert_eq!(
            build_retransmit_request(0x1234, 0xfffe, 32),
            [0x80, 0xd5, 0x12, 0x34, 0xff, 0xfe, 0x00, 0x20]
        );
    }
}
