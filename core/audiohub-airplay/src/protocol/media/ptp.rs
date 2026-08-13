//! Minimal, receive-only IEEE 802.1AS clock observer for AirPlay 2.
//!
//! AirPlay senders act as the timing grandmaster. AudioHub listens for the
//! sender's two-step Sync / Follow_Up exchange and maps that remote timeline
//! onto its local monotonic clock. It does not announce itself as a master,
//! participate in grouping, or implement multiroom topology.

use super::buffered_clock::{rtp_delta, PlaybackAnchor};
#[cfg(target_os = "macos")]
use super::ptp_macos::{MacPtpClock, MacPtpError, MacPtpPort};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(target_os = "macos")]
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
#[cfg(target_os = "macos")]
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_HEADER_BYTES: usize = 34;
const PTP_TIMESTAMP_BYTES: usize = 10;
const PTP_SYNC_BYTES: usize = PTP_HEADER_BYTES + PTP_TIMESTAMP_BYTES;
const PTP_ANNOUNCE_BYTES: usize = 64;
const PTP_MAJOR_SDO_ID: u8 = 1;
const PTP_VERSION: u8 = 2;
const PTP_DOMAIN: u8 = 0;
const PTP_TWO_STEP_FLAG: u16 = 0x0200;
const MESSAGE_SYNC: u8 = 0x0;
const MESSAGE_FOLLOW_UP: u8 = 0x8;
const MESSAGE_ANNOUNCE: u8 = 0xb;
const PAIR_LIFETIME: Duration = Duration::from_secs(2);
const ANNOUNCE_LIFETIME: Duration = Duration::from_secs(5);
const MAX_PENDING_SYNCS: usize = 128;
const MAX_CLOCK_SOURCES: usize = 64;
const SAMPLE_CHANNEL_CAPACITY: usize = 128;
const MAPPER_WINDOW: Duration = Duration::from_secs(2);
const MAPPER_MAX_SAMPLES: usize = 32;
const MAPPER_MIN_SAMPLES: usize = 3;
const MAPPER_MAX_OFFSET_STEP_NS: i128 = 250_000_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PortIdentity {
    clock: u64,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    message_type: u8,
    flags: u16,
    correction_scaled_ns: i64,
    source: PortIdentity,
    sequence: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    Announce { header: Header, grandmaster: u64 },
    Sync { header: Header },
    FollowUp { header: Header, origin_ns: u64 },
}

#[derive(Debug, Clone, Copy)]
struct Announcement {
    grandmaster: u64,
    received_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct PendingSync {
    received_at: Instant,
    correction_scaled_ns: i64,
    grandmaster: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingFollowUp {
    received_at: Instant,
    correction_scaled_ns: i64,
    origin_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SyncKey {
    peer: IpAddr,
    source: PortIdentity,
    sequence: u16,
}

/// A correlated two-step PTP observation. The local instant is the receive
/// time of Sync; the remote time is the corrected grandmaster egress time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PtpClockSample {
    pub(crate) peer: IpAddr,
    pub(crate) grandmaster: u64,
    pub(crate) remote_time_ns: u64,
    pub(crate) received_at: Instant,
}

/// Authenticated IEEE 1588 identity advertised by the AirPlay sender for
/// receive-side port matching. The port is a logical sourcePortIdentity value,
/// never UDP 319/320 or an ephemeral socket port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtpRemoteMatch {
    pub(crate) clock_identity: u64,
    pub(crate) port_number: u16,
}

#[derive(Clone)]
pub(crate) struct PtpClockSource {
    samples: broadcast::Sender<PtpClockSample>,
    #[cfg(target_os = "macos")]
    mac_clock: Option<MacPtpClock>,
    #[cfg(target_os = "macos")]
    mac_port: Option<Arc<MacPtpPort>>,
    #[cfg(target_os = "macos")]
    mac_session_gate: Option<Arc<Semaphore>>,
    #[cfg(target_os = "macos")]
    _mac_session_permit: Option<Arc<OwnedSemaphorePermit>>,
}

impl PtpClockSource {
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<PtpClockSample> {
        self.samples.subscribe()
    }

    /// Install the authenticated AirPlay peer into the platform timing
    /// backend. On Windows the process-wide UDP observer is already ready and
    /// this is a cheap clone. macOS must register the peer with CoreMedia's
    /// TimeSync service because the kernel owns UDP 319/320 and does not fan
    /// those packets out to ordinary application sockets.
    pub(crate) fn for_session(
        &self,
        local_ip: IpAddr,
        peer_ip: IpAddr,
        remote_match: Option<PtpRemoteMatch>,
    ) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        if let Some(clock) = self.mac_clock.as_ref() {
            let (IpAddr::V4(local_ip), IpAddr::V4(peer_ip)) = (local_ip, peer_ip) else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the macOS AirPlay 2 PTP backend currently requires IPv4",
                ));
            };
            let permit = self
                .mac_session_gate
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "the macOS PTP session gate is unavailable",
                    )
                })?
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "macOS currently supports one active AirPlay 2 PTP sender",
                    )
                })?;
            let port = clock
                .add_ipv4_port(local_ip, peer_ip)
                .map_err(mac_ptp_io_error)?;
            // Current Apple senders commonly omit ClockID/ClockPorts from
            // timingPeerInfo. Keep CoreMedia's default receive matching in
            // that case; Apple's AirPlaySupport likewise applies an override
            // only when the sender provides a complete sourcePortIdentity
            // tuple. A complete tuple lets us narrow matching explicitly.
            if let Some(remote_match) = remote_match {
                port.override_receive_matching(
                    remote_match.clock_identity,
                    remote_match.port_number,
                )
                .map_err(mac_ptp_io_error)?;
            }
            return Ok(Self {
                samples: self.samples.clone(),
                mac_clock: Some(clock.clone()),
                mac_port: Some(Arc::new(port)),
                mac_session_gate: self.mac_session_gate.clone(),
                _mac_session_permit: Some(Arc::new(permit)),
            });
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (local_ip, peer_ip, remote_match);

        Ok(self.clone())
    }

    /// Map a buffered RTP timestamp directly through the macOS system PTP
    /// clock. `None` means this source uses the portable UDP observer/mapper.
    pub(crate) fn platform_deadline_for_rtp(
        &self,
        anchor: PlaybackAnchor,
        packet_rtp: u32,
        sample_rate: u32,
    ) -> Option<Result<Instant, PtpClockError>> {
        #[cfg(target_os = "macos")]
        if let Some(port) = self.mac_port.as_ref() {
            let remote_time_ns = match remote_time_for_rtp(anchor, packet_rtp, sample_rate) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            return Some(
                port.map_remote_time(remote_time_ns, anchor.timeline_id)
                    .map_err(|error| match error {
                        MacPtpError::NotLocked => PtpClockError::NotReady,
                        MacPtpError::GrandmasterMismatch { .. } => PtpClockError::SourceMismatch,
                        _ => PtpClockError::PlatformClockUnavailable,
                    }),
            );
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (anchor, packet_rtp, sample_rate);

        None
    }

    pub(crate) fn local_port_identity(&self) -> Option<(u64, u16)> {
        #[cfg(target_os = "macos")]
        {
            self.mac_port
                .as_ref()
                .map(|port| (port.clock_identity(), port.port_number()))
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    #[cfg(test)]
    pub(crate) fn test_source() -> Self {
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        Self {
            samples,
            #[cfg(target_os = "macos")]
            mac_clock: None,
            #[cfg(target_os = "macos")]
            mac_port: None,
            #[cfg(target_os = "macos")]
            mac_session_gate: None,
            #[cfg(target_os = "macos")]
            _mac_session_permit: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn publish_test_sample(&self, sample: PtpClockSample) {
        let _ = self.samples.send(sample);
    }
}

pub(crate) struct PtpObserver {
    event_socket: Option<UdpSocket>,
    general_socket: Option<UdpSocket>,
    samples: broadcast::Sender<PtpClockSample>,
    announcements: HashMap<(IpAddr, PortIdentity), Announcement>,
    pending_syncs: HashMap<SyncKey, PendingSync>,
    pending_follow_ups: HashMap<SyncKey, PendingFollowUp>,
    completed_pairs: HashMap<SyncKey, Instant>,
}

impl PtpObserver {
    /// Bind the standard AirPlay PTP ports before advertising the receiver.
    /// Failure is fatal: continuing would claim a timing capability that this
    /// process cannot actually observe.
    pub(crate) async fn bind_ipv4() -> io::Result<(Self, PtpClockSource)> {
        #[cfg(target_os = "macos")]
        {
            Self::bind_macos_system_clock()
        }
        #[cfg(not(target_os = "macos"))]
        Self::bind(
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, PTP_EVENT_PORT)),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, PTP_GENERAL_PORT)),
        )
        .await
    }

    /// Isolated binding for in-process daemon tests. Real receivers must use
    /// the well-known ports; this exists because Cargo builds a dependency
    /// without `cfg(test)` even when its caller's tests run in parallel.
    pub(crate) async fn bind_ephemeral() -> io::Result<(Self, PtpClockSource)> {
        Self::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        )
        .await
    }

    async fn bind(
        event_address: SocketAddr,
        general_address: SocketAddr,
    ) -> io::Result<(Self, PtpClockSource)> {
        let event_socket = UdpSocket::bind(event_address).await?;
        let general_socket = UdpSocket::bind(general_address).await?;
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let source = PtpClockSource {
            samples: samples.clone(),
            #[cfg(target_os = "macos")]
            mac_clock: None,
            #[cfg(target_os = "macos")]
            mac_port: None,
            #[cfg(target_os = "macos")]
            mac_session_gate: None,
            #[cfg(target_os = "macos")]
            _mac_session_permit: None,
        };
        Ok((
            Self {
                event_socket: Some(event_socket),
                general_socket: Some(general_socket),
                samples,
                announcements: HashMap::with_capacity(8),
                pending_syncs: HashMap::with_capacity(16),
                pending_follow_ups: HashMap::with_capacity(16),
                completed_pairs: HashMap::with_capacity(16),
            },
            source,
        ))
    }

    #[cfg(target_os = "macos")]
    fn bind_macos_system_clock() -> io::Result<(Self, PtpClockSource)> {
        let clock = MacPtpClock::new().map_err(mac_ptp_io_error)?;
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let source = PtpClockSource {
            samples: samples.clone(),
            mac_clock: Some(clock),
            mac_port: None,
            mac_session_gate: Some(Arc::new(Semaphore::new(1))),
            _mac_session_permit: None,
        };
        Ok((
            Self {
                event_socket: None,
                general_socket: None,
                samples,
                announcements: HashMap::new(),
                pending_syncs: HashMap::new(),
                pending_follow_ups: HashMap::new(),
                completed_pairs: HashMap::new(),
            },
            source,
        ))
    }

    pub(crate) async fn run(mut self) -> io::Result<()> {
        let (Some(event_socket), Some(general_socket)) =
            (self.event_socket.take(), self.general_socket.take())
        else {
            // The macOS backend is driven by the system TimeSync service. Keep
            // this task alive so the server's select has one uniform lifetime
            // contract without pretending an ordinary UDP bind can work.
            std::future::pending::<()>().await;
            unreachable!("the macOS PTP observer future is intentionally pending")
        };
        let mut event = [0u8; 256];
        let mut general = [0u8; 512];
        loop {
            tokio::select! {
                received = event_socket.recv_from(&mut event) => {
                    let (length, peer) = received?;
                    self.receive(&event[..length], peer, PTP_EVENT_PORT, Instant::now());
                }
                received = general_socket.recv_from(&mut general) => {
                    let (length, peer) = received?;
                    self.receive(&general[..length], peer, PTP_GENERAL_PORT, Instant::now());
                }
            }
        }
    }

    fn receive(&mut self, packet: &[u8], peer: SocketAddr, local_port: u16, received_at: Instant) {
        // AirPlay uses symmetric well-known ports. Requiring this also keeps a
        // random LAN datagram from becoming a clock observation accidentally.
        if peer.port() != local_port {
            return;
        }
        let Ok(message) = parse_message(packet) else {
            return;
        };
        let expected_port = match message {
            Message::Sync { .. } => PTP_EVENT_PORT,
            Message::Announce { .. } | Message::FollowUp { .. } => PTP_GENERAL_PORT,
        };
        if local_port != expected_port {
            return;
        }

        self.prune(received_at);
        match message {
            Message::Announce {
                header,
                grandmaster,
            } => {
                if self.announcements.len() >= MAX_CLOCK_SOURCES
                    && !self.announcements.contains_key(&(peer.ip(), header.source))
                {
                    if let Some(oldest) = self
                        .announcements
                        .iter()
                        .min_by_key(|(_, value)| value.received_at)
                        .map(|(key, _)| *key)
                    {
                        self.announcements.remove(&oldest);
                    }
                }
                self.announcements.insert(
                    (peer.ip(), header.source),
                    Announcement {
                        grandmaster,
                        received_at,
                    },
                );
            }
            Message::Sync { header } => {
                let key = SyncKey {
                    peer: peer.ip(),
                    source: header.source,
                    sequence: header.sequence,
                };
                if self.completed_pairs.contains_key(&key) {
                    return;
                }
                let Some(announcement) = self.announcements.get(&(peer.ip(), header.source)) else {
                    return;
                };
                if self.pending_syncs.len() >= MAX_PENDING_SYNCS {
                    if let Some(oldest) = self
                        .pending_syncs
                        .iter()
                        .min_by_key(|(_, value)| value.received_at)
                        .map(|(key, _)| *key)
                    {
                        self.pending_syncs.remove(&oldest);
                    }
                }
                self.pending_syncs.entry(key).or_insert(PendingSync {
                    received_at,
                    correction_scaled_ns: header.correction_scaled_ns,
                    // Bind the pair to the grandmaster that was current
                    // when Sync arrived. A subsequent Announce must not
                    // relabel a partly received two-step exchange.
                    grandmaster: announcement.grandmaster,
                });
                self.emit_completed_pair(key, received_at);
            }
            Message::FollowUp { header, origin_ns } => {
                let key = SyncKey {
                    peer: peer.ip(),
                    source: header.source,
                    sequence: header.sequence,
                };
                if self.completed_pairs.contains_key(&key) {
                    return;
                }
                if self.pending_follow_ups.len() >= MAX_PENDING_SYNCS {
                    if let Some(oldest) = self
                        .pending_follow_ups
                        .iter()
                        .min_by_key(|(_, value)| value.received_at)
                        .map(|(key, _)| *key)
                    {
                        self.pending_follow_ups.remove(&oldest);
                    }
                }
                self.pending_follow_ups
                    .entry(key)
                    .or_insert(PendingFollowUp {
                        received_at,
                        correction_scaled_ns: header.correction_scaled_ns,
                        origin_ns,
                    });
                self.emit_completed_pair(key, received_at);
            }
        }
    }

    fn emit_completed_pair(&mut self, key: SyncKey, now: Instant) {
        let (Some(sync), Some(follow_up)) = (
            self.pending_syncs.get(&key).copied(),
            self.pending_follow_ups.get(&key).copied(),
        ) else {
            return;
        };
        let separation = if sync.received_at >= follow_up.received_at {
            sync.received_at.duration_since(follow_up.received_at)
        } else {
            follow_up.received_at.duration_since(sync.received_at)
        };
        self.pending_syncs.remove(&key);
        self.pending_follow_ups.remove(&key);
        if separation > PAIR_LIFETIME {
            return;
        }

        let correction =
            i128::from(sync.correction_scaled_ns) + i128::from(follow_up.correction_scaled_ns);
        let corrected_scaled = i128::from(follow_up.origin_ns) * 65_536 + correction;
        let Ok(remote_time_ns) = u64::try_from(corrected_scaled.div_euclid(65_536)) else {
            return;
        };
        if self.completed_pairs.len() >= MAX_PENDING_SYNCS {
            if let Some(oldest) = self
                .completed_pairs
                .iter()
                .min_by_key(|(_, received_at)| *received_at)
                .map(|(key, _)| *key)
            {
                self.completed_pairs.remove(&oldest);
            }
        }
        self.completed_pairs.insert(key, now);
        let _ = self.samples.send(PtpClockSample {
            peer: key.peer,
            grandmaster: sync.grandmaster,
            remote_time_ns,
            received_at: sync.received_at,
        });
    }

    fn prune(&mut self, now: Instant) {
        self.pending_syncs
            .retain(|_, sync| now.saturating_duration_since(sync.received_at) <= PAIR_LIFETIME);
        self.pending_follow_ups.retain(|_, follow_up| {
            now.saturating_duration_since(follow_up.received_at) <= PAIR_LIFETIME
        });
        self.completed_pairs.retain(|_, completed_at| {
            now.saturating_duration_since(*completed_at) <= PAIR_LIFETIME
        });
        self.announcements.retain(|_, announce| {
            now.saturating_duration_since(announce.received_at) <= ANNOUNCE_LIFETIME
        });
    }
}

fn parse_message(packet: &[u8]) -> Result<Message, PtpParseError> {
    if packet.len() < PTP_HEADER_BYTES {
        return Err(PtpParseError::WrongLength);
    }
    if packet[0] >> 4 != PTP_MAJOR_SDO_ID
        || packet[1] & 0x0f != PTP_VERSION
        || packet[4] != PTP_DOMAIN
        || packet[5] != 0
    {
        return Err(PtpParseError::UnsupportedProfile);
    }
    let declared = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if declared != packet.len() {
        return Err(PtpParseError::WrongLength);
    }
    let header = Header {
        message_type: packet[0] & 0x0f,
        flags: u16::from_be_bytes([packet[6], packet[7]]),
        correction_scaled_ns: i64::from_be_bytes(
            packet[8..16].try_into().expect("PTP header length checked"),
        ),
        source: PortIdentity {
            clock: u64::from_be_bytes(
                packet[20..28]
                    .try_into()
                    .expect("PTP header length checked"),
            ),
            port: u16::from_be_bytes(
                packet[28..30]
                    .try_into()
                    .expect("PTP header length checked"),
            ),
        },
        sequence: u16::from_be_bytes(
            packet[30..32]
                .try_into()
                .expect("PTP header length checked"),
        ),
    };
    match header.message_type {
        MESSAGE_SYNC if packet.len() >= PTP_SYNC_BYTES && header.flags & PTP_TWO_STEP_FLAG != 0 => {
            Ok(Message::Sync { header })
        }
        MESSAGE_FOLLOW_UP if packet.len() >= PTP_SYNC_BYTES => Ok(Message::FollowUp {
            header,
            origin_ns: parse_timestamp(&packet[PTP_HEADER_BYTES..PTP_SYNC_BYTES])?,
        }),
        MESSAGE_ANNOUNCE if packet.len() >= PTP_ANNOUNCE_BYTES => Ok(Message::Announce {
            header,
            grandmaster: u64::from_be_bytes(
                packet[53..61]
                    .try_into()
                    .expect("announce minimum length checked"),
            ),
        }),
        _ => Err(PtpParseError::UnsupportedMessage),
    }
}

fn parse_timestamp(bytes: &[u8]) -> Result<u64, PtpParseError> {
    if bytes.len() != PTP_TIMESTAMP_BYTES {
        return Err(PtpParseError::WrongLength);
    }
    let seconds = (u64::from(u16::from_be_bytes([bytes[0], bytes[1]])) << 32)
        | u64::from(u32::from_be_bytes(
            bytes[2..6].try_into().expect("timestamp length checked"),
        ));
    let nanos = u64::from(u32::from_be_bytes(
        bytes[6..10].try_into().expect("timestamp length checked"),
    ));
    if nanos >= NANOS_PER_SECOND as u64 {
        return Err(PtpParseError::InvalidTimestamp);
    }
    let total = u128::from(seconds) * NANOS_PER_SECOND + u128::from(nanos);
    u64::try_from(total).map_err(|_| PtpParseError::InvalidTimestamp)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtpParseError {
    WrongLength,
    UnsupportedProfile,
    UnsupportedMessage,
    InvalidTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtpClockEstimate {
    pub(crate) local_minus_remote_ns: i128,
    pub(crate) retained_samples: usize,
}

#[derive(Debug, Clone, Copy)]
struct MapperSample {
    received_at: Instant,
    local_minus_remote_ns: i128,
}

/// Bounded mapping from one authenticated session's selected PTP grandmaster
/// to the receiver's monotonic timeline.
pub(crate) struct PtpClockMapper {
    epoch: Instant,
    peer: IpAddr,
    grandmaster: u64,
    samples: VecDeque<MapperSample>,
}

impl PtpClockMapper {
    pub(crate) fn new(peer: IpAddr, grandmaster: u64, epoch: Instant) -> Self {
        Self {
            epoch,
            peer,
            grandmaster,
            samples: VecDeque::with_capacity(MAPPER_MAX_SAMPLES),
        }
    }

    pub(crate) fn grandmaster(&self) -> u64 {
        self.grandmaster
    }

    pub(crate) fn observe(
        &mut self,
        sample: PtpClockSample,
    ) -> Result<Option<PtpClockEstimate>, PtpClockError> {
        if sample.peer != self.peer || sample.grandmaster != self.grandmaster {
            return Err(PtpClockError::SourceMismatch);
        }
        if sample.received_at < self.epoch
            || self
                .samples
                .back()
                .is_some_and(|previous| sample.received_at < previous.received_at)
        {
            return Err(PtpClockError::NonMonotonicLocalTime);
        }
        self.prune(sample.received_at);
        let local_ns = sample.received_at.duration_since(self.epoch).as_nanos();
        let offset = i128::try_from(local_ns).map_err(|_| PtpClockError::MappedTimeOutOfRange)?
            - i128::from(sample.remote_time_ns);
        if let Ok(current) = self.estimate_at(sample.received_at) {
            let step = offset - current.local_minus_remote_ns;
            if step.abs() > MAPPER_MAX_OFFSET_STEP_NS {
                return Err(PtpClockError::OffsetJump);
            }
        }
        self.samples.push_back(MapperSample {
            received_at: sample.received_at,
            local_minus_remote_ns: offset,
        });
        while self.samples.len() > MAPPER_MAX_SAMPLES {
            self.samples.pop_front();
        }
        match self.estimate_at(sample.received_at) {
            Ok(value) => Ok(Some(value)),
            Err(PtpClockError::NotReady) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn deadline_for_rtp(
        &self,
        now: Instant,
        anchor: PlaybackAnchor,
        packet_rtp: u32,
        sample_rate: u32,
    ) -> Result<Instant, PtpClockError> {
        if anchor.timeline_id != self.grandmaster {
            return Err(PtpClockError::SourceMismatch);
        }
        let estimate = self.estimate_at(now)?;
        let remote_ns = i128::from(remote_time_for_rtp(anchor, packet_rtp, sample_rate)?);
        let local_ns = remote_ns + estimate.local_minus_remote_ns;
        let local_ns = u64::try_from(local_ns).map_err(|_| PtpClockError::MappedTimeOutOfRange)?;
        self.epoch
            .checked_add(Duration::from_nanos(local_ns))
            .ok_or(PtpClockError::MappedTimeOutOfRange)
    }

    fn estimate_at(&self, now: Instant) -> Result<PtpClockEstimate, PtpClockError> {
        let Some(latest) = self.samples.back() else {
            return Err(PtpClockError::NotReady);
        };
        if now < latest.received_at {
            return Err(PtpClockError::NonMonotonicLocalTime);
        }
        if now.duration_since(latest.received_at) > MAPPER_WINDOW {
            return Err(PtpClockError::Stale);
        }
        let mut offsets = self
            .samples
            .iter()
            .filter(|sample| now.duration_since(sample.received_at) <= MAPPER_WINDOW)
            .map(|sample| sample.local_minus_remote_ns)
            .collect::<Vec<_>>();
        if offsets.len() < MAPPER_MIN_SAMPLES {
            return Err(PtpClockError::NotReady);
        }
        offsets.sort_unstable();
        Ok(PtpClockEstimate {
            // The second-smallest value rejects one implausibly low software
            // timestamp while still preferring the least-queued PTP path.
            local_minus_remote_ns: offsets[1],
            retained_samples: offsets.len(),
        })
    }

    fn prune(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.received_at) > MAPPER_WINDOW)
        {
            self.samples.pop_front();
        }
    }
}

fn remote_time_for_rtp(
    anchor: PlaybackAnchor,
    packet_rtp: u32,
    sample_rate: u32,
) -> Result<u64, PtpClockError> {
    if sample_rate == 0 {
        return Err(PtpClockError::InvalidSampleRate);
    }
    let frames = i128::from(
        rtp_delta(packet_rtp, anchor.rtp_time).map_err(|_| PtpClockError::AmbiguousRtpDelta)?,
    );
    let delta_ns = div_round_nearest(frames * NANOS_PER_SECOND as i128, i128::from(sample_rate));
    let remote_ns = i128::from(anchor.remote_time_ns) + delta_ns;
    u64::try_from(remote_ns).map_err(|_| PtpClockError::MappedTimeOutOfRange)
}

fn div_round_nearest(numerator: i128, denominator: i128) -> i128 {
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtpClockError {
    SourceMismatch,
    NonMonotonicLocalTime,
    OffsetJump,
    NotReady,
    Stale,
    InvalidSampleRate,
    AmbiguousRtpDelta,
    MappedTimeOutOfRange,
    PlatformClockUnavailable,
}

impl fmt::Display for PtpClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SourceMismatch => "PTP source does not match the authenticated media session",
            Self::NonMonotonicLocalTime => "PTP receive time moved backwards",
            Self::OffsetJump => "PTP offset jumped outside the safety window",
            Self::NotReady => "PTP clock does not yet have three fresh samples",
            Self::Stale => "PTP clock samples are stale",
            Self::InvalidSampleRate => "RTP sample rate must be nonzero",
            Self::AmbiguousRtpDelta => "RTP timestamp is ambiguously half a wrap from its anchor",
            Self::MappedTimeOutOfRange => "PTP-mapped time is outside the local timeline",
            Self::PlatformClockUnavailable => "the platform PTP clock could not map this time",
        })
    }
}

impl std::error::Error for PtpClockError {}

#[cfg(target_os = "macos")]
fn mac_ptp_io_error(error: MacPtpError) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("macOS CoreMedia PTP backend unavailable: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_CLOCK: u64 = 0x0102_0304_0506_0708;
    const GRANDMASTER: u64 = 0x1112_1314_1516_1718;

    fn header(message_type: u8, length: usize, sequence: u16, correction: i64) -> Vec<u8> {
        let mut packet = vec![0u8; length];
        packet[0] = (PTP_MAJOR_SDO_ID << 4) | message_type;
        packet[1] = PTP_VERSION;
        packet[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        packet[4] = PTP_DOMAIN;
        let flags = if message_type == MESSAGE_SYNC {
            0x0608u16
        } else {
            0x0408u16
        };
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        packet[8..16].copy_from_slice(&correction.to_be_bytes());
        packet[20..28].copy_from_slice(&SOURCE_CLOCK.to_be_bytes());
        packet[28..30].copy_from_slice(&1u16.to_be_bytes());
        packet[30..32].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    fn timestamp(packet: &mut [u8], seconds: u64, nanos: u32) {
        packet[34..36].copy_from_slice(&((seconds >> 32) as u16).to_be_bytes());
        packet[36..40].copy_from_slice(&(seconds as u32).to_be_bytes());
        packet[40..44].copy_from_slice(&nanos.to_be_bytes());
    }

    #[test]
    fn parser_accepts_owned_profile_and_rejects_length_profile_and_timestamp_errors() {
        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, 7, 65_536);
        timestamp(&mut follow, 5, 9);
        assert!(matches!(
            parse_message(&follow),
            Ok(Message::FollowUp {
                header: Header { sequence: 7, .. },
                origin_ns: 5_000_000_009,
            })
        ));

        let mut wrong = follow.clone();
        wrong[0] = MESSAGE_FOLLOW_UP;
        assert_eq!(
            parse_message(&wrong),
            Err(PtpParseError::UnsupportedProfile)
        );
        let mut wrong = follow.clone();
        wrong[5] = 1;
        assert_eq!(
            parse_message(&wrong),
            Err(PtpParseError::UnsupportedProfile)
        );
        let mut wrong = follow.clone();
        wrong[3] -= 1;
        assert_eq!(parse_message(&wrong), Err(PtpParseError::WrongLength));
        let mut wrong = follow;
        wrong[40..44].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        assert_eq!(parse_message(&wrong), Err(PtpParseError::InvalidTimestamp));

        let mut one_step = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 8, 0);
        one_step[6..8].copy_from_slice(&0x0408u16.to_be_bytes());
        assert_eq!(
            parse_message(&one_step),
            Err(PtpParseError::UnsupportedMessage)
        );
    }

    #[test]
    fn parser_accepts_native_music_ptp_vectors() {
        let announce = hex::decode(concat!(
            "1B02004C00000408000000000000000000000000",
            "52737EC2E7650008800C000105FE00000000000000000000002500FAF821436AEF",
            "52737EC2E76500080000A00008000852737EC2E7650008"
        ))
        .unwrap();
        let sync = hex::decode(concat!(
            "1002002C00000608000000000000000000000000",
            "52737EC2E7650008800C000100FD00000000000000000000"
        ))
        .unwrap();
        let follow_up = hex::decode(concat!(
            "180200600000040800000000AAE6000000000000",
            "52737EC2E7650008800C000102FD00000026E5973671DD6C",
            "0003001C0080C2000001000000000000FFFFFFFFFFFBEB9CEBA7AE54FFD913C8",
            "00030010000D9300000452737EC2E76500080000"
        ))
        .unwrap();

        assert!(matches!(
            parse_message(&announce),
            Ok(Message::Announce {
                header: Header {
                    source: PortIdentity {
                        clock: 0x5273_7ec2_e765_0008,
                        port: 0x800c,
                    },
                    ..
                },
                grandmaster: 0x5273_7ec2_e765_0008,
            })
        ));
        assert!(matches!(
            parse_message(&sync),
            Ok(Message::Sync {
                header: Header {
                    sequence: 1,
                    flags: 0x0608,
                    ..
                }
            })
        ));
        assert!(matches!(
            parse_message(&follow_up),
            Ok(Message::FollowUp {
                header: Header {
                    sequence: 1,
                    flags: 0x0408,
                    correction_scaled_ns: 0x0000_0000_aae6_0000,
                    ..
                },
                origin_ns: 2_549_143_913_431_916,
            })
        ));
    }

    #[tokio::test]
    async fn observer_pairs_sync_and_follow_up_with_announce_and_corrections() {
        let event_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let general_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (samples, mut receiver) = broadcast::channel(8);
        let mut observer = PtpObserver {
            event_socket: Some(event_socket),
            general_socket: Some(general_socket),
            samples,
            announcements: HashMap::new(),
            pending_syncs: HashMap::new(),
            pending_follow_ups: HashMap::new(),
            completed_pairs: HashMap::new(),
        };
        let peer: SocketAddr = "192.0.2.10:319".parse().unwrap();
        let received_at = Instant::now();

        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(
            &announce,
            SocketAddr::new(peer.ip(), PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            received_at,
        );
        let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 9, 65_536);
        observer.receive(&sync, peer, PTP_EVENT_PORT, received_at);
        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, 9, 65_536);
        timestamp(&mut follow, 2, 4);
        observer.receive(
            &follow,
            SocketAddr::new(peer.ip(), PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            received_at + Duration::from_millis(1),
        );

        let sample = receiver.try_recv().unwrap();
        assert_eq!(sample.peer, peer.ip());
        assert_eq!(sample.grandmaster, GRANDMASTER);
        assert_eq!(sample.remote_time_ns, 2_000_000_006);
        assert_eq!(sample.received_at, received_at);
    }

    #[tokio::test]
    async fn observer_pairs_follow_up_before_sync_and_ignores_duplicates() {
        let event_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let general_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (samples, mut receiver) = broadcast::channel(8);
        let mut observer = PtpObserver {
            event_socket: Some(event_socket),
            general_socket: Some(general_socket),
            samples,
            announcements: HashMap::new(),
            pending_syncs: HashMap::new(),
            pending_follow_ups: HashMap::new(),
            completed_pairs: HashMap::new(),
        };
        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        let base = Instant::now();
        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        let mut follow_up = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, u16::MAX, 32_768);
        timestamp(&mut follow_up, 9, 100);
        let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, u16::MAX, -65_536);

        observer.receive(
            &announce,
            SocketAddr::new(peer, PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            base,
        );
        observer.receive(
            &follow_up,
            SocketAddr::new(peer, PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            base + Duration::from_millis(2),
        );
        observer.receive(
            &sync,
            SocketAddr::new(peer, PTP_EVENT_PORT),
            PTP_EVENT_PORT,
            base + Duration::from_millis(1),
        );
        let sample = receiver.try_recv().unwrap();
        assert_eq!(sample.grandmaster, GRANDMASTER);
        // -1 ns plus 0.5 ns is floored using Euclidean division.
        assert_eq!(sample.remote_time_ns, 9_000_000_099);
        assert_eq!(sample.received_at, base + Duration::from_millis(1));

        observer.receive(
            &sync,
            SocketAddr::new(peer, PTP_EVENT_PORT),
            PTP_EVENT_PORT,
            base + Duration::from_millis(3),
        );
        observer.receive(
            &follow_up,
            SocketAddr::new(peer, PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            base + Duration::from_millis(4),
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn mapper_requires_matching_fresh_grandmaster_and_maps_wrapping_rtp() {
        let epoch = Instant::now();
        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        let mut mapper = PtpClockMapper::new(peer, GRANDMASTER, epoch);
        for index in 1..=3u64 {
            let remote = index * 1_000;
            mapper
                .observe(PtpClockSample {
                    peer,
                    grandmaster: GRANDMASTER,
                    remote_time_ns: remote,
                    received_at: epoch + Duration::from_nanos(remote + 100),
                })
                .unwrap();
        }
        let anchor = PlaybackAnchor {
            timeline_id: GRANDMASTER,
            remote_time_ns: 10_000,
            rtp_time: 0xffff_fff0,
        };
        assert_eq!(
            mapper
                .deadline_for_rtp(
                    epoch + Duration::from_nanos(3_100),
                    anchor,
                    0x0000_03f0,
                    44_100,
                )
                .unwrap(),
            epoch + Duration::from_nanos(10_000 + 23_219_955 + 100)
        );
        assert_eq!(
            mapper.deadline_for_rtp(
                epoch + Duration::from_secs(3),
                anchor,
                anchor.rtp_time,
                44_100
            ),
            Err(PtpClockError::Stale)
        );
    }
}
