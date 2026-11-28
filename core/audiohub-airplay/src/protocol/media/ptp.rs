//! Minimal IEEE 802.1AS clock observer for AirPlay 2.
//!
//! AirPlay senders act as the timing grandmaster. AudioHub listens for the
//! sender's two-step Sync / Follow_Up exchange (or a one-step Sync) and maps
//! that remote timeline onto its local monotonic clock. It does not
//! implement multiroom topology; it follows the grandmaster the sender's
//! group elects, including changes announced through SETPEERS
//! (`ptp_session`).
//!
//! The observer transmits exactly one kind of PTP message: the wake-up
//! Announce pair for a "silent" sender clock (see [`WakeRegistry`]). Some
//! Apple senders (an iPad, 2026-11-28 capture) only Announce and Signal
//! until a peer takes part at the PTP layer; shairport-sync's nqptp wakes
//! them by briefly announcing a better clock and at once a worse one. It
//! never sends Sync or Follow_Up, so it can never actually serve as anyone's
//! grandmaster.

use super::buffered_clock::{rtp_delta, PlaybackAnchor};
#[cfg(target_os = "macos")]
use super::ptp_macos::{MacPtpClock, MacPtpError, MacPtpPort};
use super::ptp_session::{PeerSetChange, SessionShared};
#[cfg(target_os = "macos")]
use super::ptp_timesync;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
const MESSAGE_SIGNALING: u8 = 0xc;
/// targetPortIdentity, the fixed part of a Signaling body.
const PTP_SIGNALING_BYTES: usize = PTP_HEADER_BYTES + 10;
/// unicastFlag | ptpTimescale, as on Apple's and nqptp's Announce.
const WAKE_ANNOUNCE_FLAGS: u16 = 0x0408;
/// sourcePortIdentity.portNumber of the wake-up Announce (nqptp: 32776).
const WAKE_PORT_NUMBER: u16 = 0x8008;
/// clockClass 248, clockAccuracy unknown, offsetScaledLogVariance 0x436a
/// (nqptp `send_awakening_announcement_sequence`).
const WAKE_CLOCK_QUALITY: u32 = 0xf8fe_436a;
/// timeSource INTERNAL_OSCILLATOR.
const WAKE_TIME_SOURCE: u8 = 0xa0;
const WAKE_UTC_OFFSET: u16 = 37;
/// logMessageInterval -2 (four Announces a second, as the senders use).
const WAKE_LOG_INTERVAL: u8 = 0xfe;
/// controlField "all others".
const WAKE_CONTROL_FIELD: u8 = 0x05;
/// Announces from a sender clock without any Sync/Follow_Up before it is
/// woken (nqptp `broadcasting_task`: `announcements_without_followups == 3`).
const WAKE_AFTER_ANNOUNCES: u64 = 3;
/// The worse Announce follows the better one after this (nqptp: 150 ms).
const WAKE_INFERIOR_DELAY: Duration = Duration::from_millis(150);
/// A still-silent clock is woken again after this, at most
/// [`WAKE_MAX_ATTEMPTS`] times per session (UDP may lose the pair).
const WAKE_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const WAKE_MAX_ATTEMPTS: u32 = 3;
/// A failed send of the retracting (worse) Announce is retried this often.
const WAKE_RESEND_DELAY: Duration = Duration::from_millis(50);
const WAKE_MAX_RESENDS: u8 = 3;
const PAIR_LIFETIME: Duration = Duration::from_secs(2);
const ANNOUNCE_LIFETIME: Duration = Duration::from_secs(5);
const MAX_PENDING_SYNCS: usize = 128;
const MAX_CLOCK_SOURCES: usize = 64;
// Room for several senders' interleaved Sync/Follow_Up pairs (design §2.5).
const SAMPLE_CHANNEL_CAPACITY: usize = 512;
const MAPPER_WINDOW: Duration = Duration::from_secs(2);
/// A frame may wait this long for the mapper's initial three-sample window
/// before `NotReady` becomes a telemetry failure. Audio behavior remains a
/// hold in both cases.
pub(crate) const DEADLINE_NOT_READY_WARMUP: Duration = MAPPER_WINDOW;
const MAPPER_MAX_SAMPLES: usize = 32;
const MAPPER_MIN_SAMPLES: usize = 3;
const MAPPER_MAX_OFFSET_STEP_NS: i128 = 250_000_000;
/// Samples within this distance of an estimate support it for a handoff.
const HANDOFF_CONSISTENCY_NS: i128 = 20_000_000;
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
    Announce {
        header: Header,
        grandmaster: u64,
        priority1: u8,
        priority2: u8,
    },
    /// `one_step_origin_ns` is the origin timestamp of a one-step Sync (the
    /// two-step flag is clear); `None` for a two-step Sync, whose time comes
    /// in its Follow_Up.
    Sync {
        header: Header,
        one_step_origin_ns: Option<u64>,
    },
    FollowUp { header: Header, origin_ns: u64 },
    /// Counted for diagnostics only (Apple sends an organization-extension
    /// TLV with every Announce).
    Signaling { header: Header },
}

#[derive(Debug, Clone, Copy)]
struct Announcement {
    grandmaster: u64,
    received_at: Instant,
    telemetry_token: crate::telemetry::OperationToken,
}

#[derive(Debug, Clone, Copy)]
struct PendingSync {
    received_at: Instant,
    correction_scaled_ns: i64,
    grandmaster: u64,
    telemetry_token: crate::telemetry::OperationToken,
}

#[derive(Debug, Clone, Copy)]
struct PendingFollowUp {
    received_at: Instant,
    correction_scaled_ns: i64,
    origin_ns: u64,
    telemetry_token: crate::telemetry::OperationToken,
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
    /// Reset generation captured when Sync entered the observer. A queued
    /// pre-reset sample must stay inert after a telemetry reset.
    pub(crate) telemetry_token: crate::telemetry::OperationToken,
}

/// Authenticated IEEE 1588 identity advertised by the AirPlay sender for
/// receive-side port matching. The port is a logical sourcePortIdentity value,
/// never UDP 319/320 or an ephemeral socket port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtpRemoteMatch {
    pub(crate) clock_identity: u64,
    pub(crate) port_number: u16,
}

/// A platform clock followed a new grandmaster: add `delta_ns` to a time on
/// the old grandmaster's timeline to get the same instant on the new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformHandoff {
    pub(crate) grandmaster: u64,
    pub(crate) delta_ns: i128,
}

/// Extra unicast timing ports a SETPEERS update opened or closed on a
/// platform clock (macOS). Portable sessions need no ports.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PeerPortUpdate {
    pub(crate) opened: Vec<Ipv4Addr>,
    pub(crate) closed: Vec<Ipv4Addr>,
    pub(crate) failed: Vec<(Ipv4Addr, &'static str)>,
}

/// Outcome of applying one peer list to a session's clock source.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PeerListApplied {
    pub(crate) change: PeerSetChange,
    pub(crate) ports: Option<PeerPortUpdate>,
}

/// Whether `address` can be one of a session's timing peers: a unicast
/// remote address other than the control peer and this host.
pub(crate) fn usable_timing_peer(
    address: IpAddr,
    control_peer: IpAddr,
    local: IpAddr,
    interfaces: &[IpAddr],
) -> bool {
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address == control_peer
        || address == local
        || interfaces.contains(&address))
}

/// Parse one advertised timing address. Senders list IPv4 and IPv6 strings;
/// an IPv6 zone suffix (`%en0`) is dropped.
pub(crate) fn parse_timing_address(text: &str) -> Option<IpAddr> {
    let text = text.trim();
    let text = text.split('%').next().unwrap_or(text);
    text.parse().ok()
}

/// Latest grandmaster each PTP peer announced to the portable observer.
#[derive(Debug, Clone, Copy)]
struct ObservedGrandmaster {
    peer: IpAddr,
    grandmaster: u64,
    seen_at: Instant,
}

static OBSERVED_GRANDMASTERS: Mutex<Vec<ObservedGrandmaster>> = Mutex::new(Vec::new());

fn note_observed_grandmaster(peer: IpAddr, grandmaster: u64, seen_at: Instant) {
    let mut observed = OBSERVED_GRANDMASTERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(entry) = observed.iter_mut().find(|entry| entry.peer == peer) {
        entry.grandmaster = grandmaster;
        entry.seen_at = seen_at;
        return;
    }
    if observed.len() >= MAX_CLOCK_SOURCES {
        if let Some(oldest) = observed
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.seen_at)
            .map(|(index, _)| index)
        {
            observed.swap_remove(oldest);
        }
    }
    observed.push(ObservedGrandmaster {
        peer,
        grandmaster,
        seen_at,
    });
}

/// Grandmasters announced by `peers` to the portable observer, with their
/// age, for diagnostics (`peer:gm@age_ms`).
pub(crate) fn describe_observed_grandmasters(peers: &[IpAddr], now: Instant) -> String {
    let observed = OBSERVED_GRANDMASTERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut text = String::new();
    for entry in observed.iter().filter(|entry| peers.contains(&entry.peer)) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&format!(
            "{}:{:016x}@{}ms",
            entry.peer,
            entry.grandmaster,
            now.saturating_duration_since(entry.seen_at).as_millis()
        ));
    }
    text
}

/// PTP clockIdentity of this receiver: the EUI-48 AirPlay device id
/// widened to EUI-64 by inserting FF-FE (IEEE 1588 7.5.2.2.2, as nqptp's
/// `get_self_clock_id` does with an interface MAC).
pub(crate) fn clock_identity_from_mac(mac: [u8; 6]) -> u64 {
    u64::from_be_bytes([mac[0], mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]])
}

/// What one session's sender (its control peer) sent to the portable
/// observer since the session registered. Read for the silent-clock warning
/// and the never-ready diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SenderClockStatus {
    pub(crate) first_announce_at: Option<Instant>,
    pub(crate) last_announce_at: Option<Instant>,
    /// Grandmaster named by the latest Announce.
    pub(crate) grandmaster: Option<u64>,
    pub(crate) announces: u64,
    pub(crate) sync_two_step: u64,
    pub(crate) sync_one_step: u64,
    pub(crate) follow_ups: u64,
    pub(crate) signaling: u64,
    pub(crate) wake_sequences: u32,
}

impl SenderClockStatus {
    /// Sync and Follow_Up messages: any of them means the clock is not
    /// silent.
    pub(crate) fn timing_messages(&self) -> u64 {
        self.sync_two_step
            .saturating_add(self.sync_one_step)
            .saturating_add(self.follow_ups)
    }
}

impl fmt::Display for SenderClockStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "announce={} signaling={} sync_two_step={} sync_one_step={} follow_up={} wake_sequences={}",
            self.announces,
            self.signaling,
            self.sync_two_step,
            self.sync_one_step,
            self.follow_ups,
            self.wake_sequences
        )
    }
}

/// The sender's own clock as its latest Announce described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnnouncedClock {
    /// Where the Announce came from; the wake-up goes back there.
    from: SocketAddr,
    clock: u64,
    grandmaster: u64,
    priority1: u8,
    priority2: u8,
}

#[derive(Debug)]
struct WakeTarget {
    id: u64,
    peer: IpAddr,
    status: SenderClockStatus,
    announced: Option<AnnouncedClock>,
    last_attempt: Option<Instant>,
}

/// Sender clocks of the live portable sessions, shared by the sessions
/// (register on SETUP, deregister on drop) and the observer task that counts
/// their datagrams and wakes a silent one.
///
/// Wake-up rule, after nqptp (`nqptp.c` `broadcasting_task` and
/// `send_awakening_announcement_sequence`): when the session's control peer
/// has sent [`WAKE_AFTER_ANNOUNCES`] Announces as its own grandmaster and no
/// Sync or Follow_Up at all, unicast two Announces back to the address they
/// came from: first one priority1 better than the sender's, then
/// [`WAKE_INFERIOR_DELAY`] later one priority1 worse. Deviations from nqptp:
/// nqptp also sends the pair on the first Announce of every clock, including
/// ones already sending Sync; here only a clock with no Sync/Follow_Up is
/// touched, never one the session already times from. The pair is repeated
/// at most [`WAKE_MAX_ATTEMPTS`] times, [`WAKE_RETRY_INTERVAL`] apart, while
/// the clock stays silent (nqptp parks after one), and the two Announces
/// carry distinct sequence numbers so a receiver cannot discard the
/// retraction as a duplicate.
#[derive(Debug, Default)]
pub(crate) struct WakeRegistry {
    targets: Mutex<Vec<WakeTarget>>,
    next_id: AtomicU64,
}

impl WakeRegistry {
    fn register(self: &Arc<Self>, peer: IpAddr) -> SenderClockWatch {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(WakeTarget {
                id,
                peer,
                status: SenderClockStatus::default(),
                announced: None,
                last_attempt: None,
            });
        SenderClockWatch {
            registry: Arc::clone(self),
            id,
        }
    }

    fn status(&self, id: u64) -> Option<SenderClockStatus> {
        self.targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|target| target.id == id)
            .map(|target| target.status)
    }

    /// Apply `update` to every live session whose control peer is `peer`.
    fn observe(&self, peer: IpAddr, mut update: impl FnMut(&mut WakeTarget)) {
        let mut targets = self.targets.lock().unwrap_or_else(PoisonError::into_inner);
        for target in targets.iter_mut().filter(|target| target.peer == peer) {
            update(target);
        }
    }

    /// The clock to wake for `peer` now, if any (see the type docs). Marks
    /// the attempt on every session of that peer, so one pair serves all of
    /// them.
    fn wake_due(&self, peer: IpAddr, own_identity: u64, now: Instant) -> Option<(AnnouncedClock, u32)> {
        let mut targets = self.targets.lock().unwrap_or_else(PoisonError::into_inner);
        let recent_attempt = targets.iter().any(|target| {
            target.peer == peer
                && target
                    .last_attempt
                    .is_some_and(|at| now.saturating_duration_since(at) < WAKE_RETRY_INTERVAL)
        });
        if recent_attempt {
            return None;
        }
        let (clock, attempt) = targets
            .iter()
            .filter(|target| target.peer == peer)
            .find_map(|target| {
                let clock = target.announced?;
                let status = target.status;
                let wanted = status.timing_messages() == 0
                    && status.announces >= WAKE_AFTER_ANNOUNCES
                    && status.wake_sequences < WAKE_MAX_ATTEMPTS
                    && status
                        .last_announce_at
                        .is_some_and(|at| now.saturating_duration_since(at) <= ANNOUNCE_LIFETIME)
                    // Only a sender that is its own grandmaster: never ask a
                    // clock that follows another one (a grouped HomePod's)
                    // to renegotiate.
                    && clock.clock == clock.grandmaster
                    && clock.grandmaster != own_identity
                    && wake_priorities(clock.priority1).is_some();
                wanted.then_some((clock, status.wake_sequences + 1))
            })?;
        for target in targets.iter_mut().filter(|target| target.peer == peer) {
            target.last_attempt = Some(now);
            target.status.wake_sequences = target.status.wake_sequences.max(attempt);
        }
        Some((clock, attempt))
    }

    /// Record an Announce from `peer` naming `grandmaster`, as the observer
    /// does for a received datagram.
    #[cfg(test)]
    pub(crate) fn test_note_announce(&self, peer: IpAddr, grandmaster: u64, at: Instant) {
        self.observe(peer, |target| {
            target.status.announces += 1;
            target.status.first_announce_at.get_or_insert(at);
            target.status.last_announce_at = Some(at);
            target.status.grandmaster = Some(grandmaster);
        });
    }

    fn deregister(&self, id: u64) {
        self.targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|target| target.id != id);
    }
}

/// A session's registration in the [`WakeRegistry`]; dropping it (the
/// session ending) stops any further wake-up for that session.
#[derive(Debug)]
pub(crate) struct SenderClockWatch {
    registry: Arc<WakeRegistry>,
    id: u64,
}

impl SenderClockWatch {
    pub(crate) fn status(&self) -> Option<SenderClockStatus> {
        self.registry.status(self.id)
    }
}

impl Drop for SenderClockWatch {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

/// priority1 values of the better and the worse wake-up Announce, or `None`
/// when the sender's priority1 leaves no strictly better and strictly worse
/// value (nqptp falls back to fixed values there; this never risks
/// announcing a lasting better clock instead).
fn wake_priorities(sender_priority1: u8) -> Option<(u8, u8)> {
    Some((
        sender_priority1.checked_sub(1)?,
        sender_priority1.checked_add(1)?,
    ))
}

/// One wake-up Announce (64 bytes, no TLV), field for field as nqptp's
/// `send_awakening_announcement_sequence` builds it.
fn wake_announce(own_identity: u64, sequence: u16, priority1: u8, priority2: u8) -> [u8; PTP_ANNOUNCE_BYTES] {
    let mut packet = [0u8; PTP_ANNOUNCE_BYTES];
    packet[0] = (PTP_MAJOR_SDO_ID << 4) | MESSAGE_ANNOUNCE;
    packet[1] = PTP_VERSION;
    packet[2..4].copy_from_slice(&(PTP_ANNOUNCE_BYTES as u16).to_be_bytes());
    packet[4] = PTP_DOMAIN;
    packet[6..8].copy_from_slice(&WAKE_ANNOUNCE_FLAGS.to_be_bytes());
    packet[20..28].copy_from_slice(&own_identity.to_be_bytes());
    packet[28..30].copy_from_slice(&WAKE_PORT_NUMBER.to_be_bytes());
    packet[30..32].copy_from_slice(&sequence.to_be_bytes());
    packet[32] = WAKE_CONTROL_FIELD;
    packet[33] = WAKE_LOG_INTERVAL;
    // 34..44 originTimestamp stays zero.
    packet[44..46].copy_from_slice(&WAKE_UTC_OFFSET.to_be_bytes());
    packet[47] = priority1;
    packet[48..52].copy_from_slice(&WAKE_CLOCK_QUALITY.to_be_bytes());
    packet[52] = priority2;
    packet[53..61].copy_from_slice(&own_identity.to_be_bytes());
    // 61..63 stepsRemoved stays zero.
    packet[63] = WAKE_TIME_SOURCE;
    packet
}

/// A wake-up Announce waiting for its send time.
#[derive(Debug, Clone, Copy)]
struct WakeDatagram {
    due: Instant,
    to: SocketAddr,
    packet: [u8; PTP_ANNOUNCE_BYTES],
    /// The worse Announce that retracts the better one; it ends the pair.
    retraction: bool,
    resends: u8,
    /// Whether every earlier datagram of this pair was sent.
    pair_ok: bool,
    telemetry_token: crate::telemetry::OperationToken,
}

/// Timing backend bound to one PTP control session.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtpBackendKind {
    /// Process-wide UDP 319/320 observer plus a per-session mapper (Windows,
    /// and isolated test receivers).
    Portable,
    /// macOS: a per-sender TimeSync gPTP instance.
    MacInstance,
    /// macOS: the shared CoreMedia system clock behind the one-sender gate.
    MacSystem,
}

impl PtpBackendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::MacInstance => "instance",
            Self::MacSystem => "system",
        }
    }
}

/// Process-wide view of the PTP timing backends (design §3 step 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtpBackendStatus {
    /// Backend a new PTP sender would get: `instance`, `system`, `portable`
    /// or `unavailable`; `None` while not yet determined (macOS probes
    /// TimeSync lazily at the first PTP SETUP).
    pub kind_available: Option<&'static str>,
    pub tripped: bool,
    pub instance_sessions: u64,
    pub system_sessions: u64,
    pub instance_create_failures: u64,
    pub lock_timeouts: u64,
    pub gm_lost: u64,
}

const PLATFORM_UNBOUND: u8 = 0;
const PLATFORM_PORTABLE: u8 = 1;
const PLATFORM_MAC_WITH_SYSTEM_CLOCK: u8 = 2;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const PLATFORM_MAC_WITHOUT_SYSTEM_CLOCK: u8 = 3;
static PLATFORM: AtomicU8 = AtomicU8::new(PLATFORM_UNBOUND);
static INSTANCE_SESSIONS: AtomicU64 = AtomicU64::new(0);
static SYSTEM_SESSIONS: AtomicU64 = AtomicU64::new(0);
static INSTANCE_CREATE_FAILURES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn ptp_backend_status() -> PtpBackendStatus {
    #[cfg(target_os = "macos")]
    let (lock_timeouts, gm_lost, tripped) = (
        ptp_timesync::LOCK_TIMEOUTS.load(Ordering::Relaxed),
        ptp_timesync::GM_LOST.load(Ordering::Relaxed),
        ptp_timesync::BREAKER.is_tripped(),
    );
    #[cfg(not(target_os = "macos"))]
    let (lock_timeouts, gm_lost, tripped) = (0, 0, false);
    PtpBackendStatus {
        kind_available: backend_available(tripped),
        tripped,
        instance_sessions: INSTANCE_SESSIONS.load(Ordering::Relaxed),
        system_sessions: SYSTEM_SESSIONS.load(Ordering::Relaxed),
        instance_create_failures: INSTANCE_CREATE_FAILURES.load(Ordering::Relaxed),
        lock_timeouts,
        gm_lost,
    }
}

fn backend_available(tripped: bool) -> Option<&'static str> {
    let platform = PLATFORM.load(Ordering::Relaxed);
    let system = match platform {
        PLATFORM_UNBOUND => return None,
        PLATFORM_PORTABLE => return Some("portable"),
        PLATFORM_MAC_WITH_SYSTEM_CLOCK => true,
        _ => false,
    };
    let instance = if super::ptp_timesync::test_hooks::forced_system() || tripped {
        Some(false)
    } else {
        #[cfg(target_os = "macos")]
        {
            ptp_timesync::probed_availability().map(ptp_timesync::Availability::is_available)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Some(false)
        }
    };
    match instance {
        Some(true) => Some("instance"),
        Some(false) if system => Some("system"),
        Some(false) => Some("unavailable"),
        // Not probed yet: a system clock alone would be the answer only if
        // the probe fails, so report "not determined".
        None => None,
    }
}

/// Whether `peer` is this host: loopback, the connection's local address or
/// any address of a local interface (`interfaces`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn is_local_peer(peer: Ipv4Addr, local: Ipv4Addr, interfaces: &[Ipv4Addr]) -> bool {
    peer.is_loopback() || peer == local || interfaces.contains(&peer)
}

/// IPv4 addresses of this host's interfaces. An enumeration failure is
/// logged and yields none; loopback and the connection's local address are
/// still recognized by [`is_local_peer`].
#[cfg(target_os = "macos")]
fn local_interface_ipv4() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter_map(|interface| match interface.ip() {
                IpAddr::V4(address) => Some(address),
                IpAddr::V6(_) => None,
            })
            .collect(),
        Err(error) => {
            log::warn!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP could not enumerate local interfaces: {error}"
            );
            Vec::new()
        }
    }
}

/// Every address of this host's interfaces (macOS); elsewhere only the
/// connection's local address is known and checked by the caller.
fn local_interface_addresses() -> Vec<IpAddr> {
    #[cfg(target_os = "macos")]
    {
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => interfaces
                .into_iter()
                .map(|interface| interface.ip())
                .collect(),
            Err(error) => {
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP could not enumerate local interfaces: {error}"
                );
                Vec::new()
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Reason a session must skip the per-sender instance backend before any
/// instance is created, or `None` when the instance may be tried.
///
/// A sender on this very host never locks a TimeSync instance (its PTP
/// packets do not cross an interface), so it goes straight to the system
/// clock and never counts toward the breaker.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn instance_precheck(
    breaker: &super::ptp_timesync::Breaker,
    peer: Ipv4Addr,
    peer_is_local: bool,
) -> Option<&'static str> {
    if peer_is_local {
        Some("local_peer")
    } else if breaker.is_tripped() {
        Some("tripped")
    } else if breaker.peer_failed(peer) {
        Some("peer_lock_failed")
    } else {
        None
    }
}

/// Outcome of the backend fallback chain for one session.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug)]
pub(crate) enum BackendChoice<I, S> {
    Instance(I),
    System(S),
}

/// Design §2.4 fallback chain, independent of the concrete backends so the
/// state machine is testable with fakes:
///
/// 1. `forced_system` (test builds only) skips the instance step;
/// 2. `instance` is `Err(reason)` when unavailable or tripped, otherwise a
///    creation future whose failure reason is kept as the fallback reason;
/// 3. `system` is `None` without a CoreMedia clock, otherwise it may fail
///    with `WouldBlock` when the one-sender gate is taken (RTSP 453);
/// 4. neither available is `Unsupported` (RTSP 500).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) async fn select_backend<I, S, F>(
    forced_system: bool,
    instance: Result<F, &'static str>,
    system: Option<impl FnOnce() -> io::Result<S>>,
) -> io::Result<(BackendChoice<I, S>, Option<&'static str>)>
where
    F: Future<Output = Result<I, &'static str>>,
{
    let fallback_reason = if forced_system {
        Some("forced_system")
    } else {
        match instance {
            Ok(create) => match create.await {
                Ok(port) => return Ok((BackendChoice::Instance(port), None)),
                Err(reason) => Some(reason),
            },
            Err(reason) => Some(reason),
        }
    };
    match system {
        Some(open) => open().map(|port| (BackendChoice::System(port), fallback_reason)),
        None => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "no PTP timing backend: {}",
                fallback_reason.unwrap_or("system clock unavailable")
            ),
        )),
    }
}

#[cfg(target_os = "macos")]
struct MacRoot {
    clock: Option<MacPtpClock>,
    gate: Arc<Semaphore>,
}

#[cfg(target_os = "macos")]
enum MacSession {
    System {
        port: MacPtpPort,
        _permit: OwnedSemaphorePermit,
        /// Extra ports for the session's SETPEERS peers. Only one session
        /// holds the system clock (the gate), so no cross-session counts.
        peer_ports: Mutex<HashMap<Ipv4Addr, MacPtpPort>>,
    },
    Instance(ptp_timesync::TimeSyncPort),
}

#[derive(Clone)]
pub(crate) struct PtpClockSource {
    samples: broadcast::Sender<PtpClockSample>,
    /// Sender clocks of the portable sessions, shared with the observer.
    wake: Arc<WakeRegistry>,
    backend: PtpBackendKind,
    fallback_reason: Option<&'static str>,
    /// Per-session timing peers and handoff state; `None` on the
    /// process-wide root source.
    session: Option<Arc<SessionShared>>,
    #[cfg(target_os = "macos")]
    mac_root: Option<Arc<MacRoot>>,
    #[cfg(target_os = "macos")]
    mac_session: Option<Arc<MacSession>>,
    #[cfg(test)]
    fake_platform: Option<Arc<FakePlatform>>,
}

impl PtpClockSource {
    fn portable(samples: broadcast::Sender<PtpClockSample>, wake: Arc<WakeRegistry>) -> Self {
        Self {
            samples,
            wake,
            backend: PtpBackendKind::Portable,
            fallback_reason: None,
            session: None,
            #[cfg(target_os = "macos")]
            mac_root: None,
            #[cfg(target_os = "macos")]
            mac_session: None,
            #[cfg(test)]
            fake_platform: None,
        }
    }

    /// Session state shared with the media engine.
    pub(crate) fn session(&self) -> Option<&Arc<SessionShared>> {
        self.session.as_ref()
    }

    /// Whether deadlines come from a platform clock (macOS TimeSync or
    /// CoreMedia) rather than the portable observer's samples.
    pub(crate) fn maps_on_platform(&self) -> bool {
        #[cfg(test)]
        if self.fake_platform.is_some() {
            return true;
        }
        #[cfg(target_os = "macos")]
        if self.mac_session.is_some() {
            return true;
        }
        false
    }

    /// Grandmaster the platform clock currently follows, for diagnostics.
    pub(crate) fn platform_grandmaster(&self) -> Option<u64> {
        #[cfg(test)]
        if let Some(fake) = self.fake_platform.as_ref() {
            return fake.grandmaster();
        }
        #[cfg(target_os = "macos")]
        if let Some(session) = self.mac_session.as_deref() {
            return match session {
                MacSession::System { port, .. } => port.last_grandmaster(),
                MacSession::Instance(port) => port.grandmaster(),
            };
        }
        None
    }

    /// Carry an anchor on `from_grandmaster` over to the grandmaster the
    /// platform clock now follows. `None` for the portable backend, which
    /// hands off in `ptp_session` from its own samples.
    pub(crate) fn platform_handoff(
        &self,
        from_grandmaster: u64,
    ) -> Option<Result<PlatformHandoff, &'static str>> {
        #[cfg(test)]
        if let Some(fake) = self.fake_platform.as_ref() {
            return Some(fake.handoff(from_grandmaster));
        }
        #[cfg(target_os = "macos")]
        if let Some(session) = self.mac_session.as_deref() {
            return Some(match session {
                MacSession::System { port, .. } => port.handoff(from_grandmaster),
                MacSession::Instance(port) => port.handoff(from_grandmaster),
            });
        }
        let _ = from_grandmaster;
        None
    }

    /// Apply an authenticated peer list (SETUP timing peers, SETPEERS,
    /// SETPEERSX) to this session: samples from every listed peer become
    /// acceptable, and on macOS the listed IPv4 peers get unicast timing
    /// ports on the session's instance or system clock. `None` when this is
    /// not a session source.
    pub(crate) async fn apply_peer_list(
        &self,
        local_ip: IpAddr,
        addresses: &[IpAddr],
    ) -> Option<PeerListApplied> {
        let session = self.session.as_ref()?;
        let control = session.control_peer();
        let interfaces = local_interface_addresses();
        let peers: Vec<IpAddr> = addresses
            .iter()
            .copied()
            .filter(|address| usable_timing_peer(*address, control, local_ip, &interfaces))
            .collect();
        let change = session.replace_peers(peers);
        let ports = self.apply_platform_ports(&change.peers).await;
        Some(PeerListApplied { change, ports })
    }

    #[cfg(target_os = "macos")]
    async fn apply_platform_ports(&self, peers: &[IpAddr]) -> Option<PeerPortUpdate> {
        let session = self.mac_session.as_deref()?;
        let wanted: Vec<Ipv4Addr> = peers
            .iter()
            .filter_map(|peer| match peer {
                IpAddr::V4(address) => Some(*address),
                IpAddr::V6(_) => None,
            })
            .collect();
        Some(match session {
            MacSession::Instance(port) => port.set_peers(wanted).await,
            MacSession::System {
                port, peer_ports, ..
            } => {
                let mut update = PeerPortUpdate::default();
                let mut ports = peer_ports.lock().unwrap_or_else(PoisonError::into_inner);
                let stale: Vec<Ipv4Addr> = ports
                    .keys()
                    .filter(|address| !wanted.contains(address))
                    .copied()
                    .collect();
                for address in stale {
                    // Dropping the port removes it from the CoreMedia clock.
                    ports.remove(&address);
                    update.closed.push(address);
                }
                for address in wanted {
                    if ports.contains_key(&address) || address == port.destination_peer() {
                        continue;
                    }
                    match port.sibling(address) {
                        Ok(sibling) => {
                            ports.insert(address, sibling);
                            update.opened.push(address);
                        }
                        Err(error) => {
                            log::warn!(
                                target: "audiohub_airplay::airplay2",
                                "AirPlay 2 PTP system clock could not add timing peer {address}: {error}"
                            );
                            update.failed.push((address, "port_failed"));
                        }
                    }
                }
                update
            }
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn apply_platform_ports(&self, _peers: &[IpAddr]) -> Option<PeerPortUpdate> {
        None
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<PtpClockSample> {
        self.samples.subscribe()
    }

    /// Backend this session maps through.
    pub(crate) fn backend(&self) -> PtpBackendKind {
        self.backend
    }

    /// Why a preferred backend was not used for this session, if any.
    pub(crate) fn fallback_reason(&self) -> Option<&'static str> {
        self.fallback_reason
    }

    /// Becomes true when the session's instance port failed its lock
    /// watchdog; the control connection should then close. `None` for
    /// backends without such a signal.
    pub(crate) fn failed(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        #[cfg(target_os = "macos")]
        if let Some(MacSession::Instance(port)) = self.mac_session.as_deref() {
            return Some(port.failed());
        }
        None
    }

    /// Install the authenticated AirPlay peer into the platform timing
    /// backend. On Windows the process-wide UDP observer is already ready and
    /// this is a cheap clone. On macOS the kernel owns UDP 319/320, so the
    /// peer is registered with TimeSync: preferably as its own gPTP instance
    /// (several senders at once), otherwise on CoreMedia's shared system
    /// clock, which can follow only one sender (`WouldBlock` -> RTSP 453).
    pub(crate) async fn for_session(
        &self,
        local_ip: IpAddr,
        peer_ip: IpAddr,
        remote_match: Option<PtpRemoteMatch>,
    ) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        if let Some(root) = self.mac_root.as_ref() {
            let (IpAddr::V4(local_ip), IpAddr::V4(peer_ip)) = (local_ip, peer_ip) else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the macOS AirPlay 2 PTP backend currently requires IPv4",
                ));
            };
            return self
                .mac_session_source(root, local_ip, peer_ip, remote_match)
                .await;
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (local_ip, remote_match);

        let mut source = self.clone();
        source.session = Some(Arc::new(SessionShared::with_clock_watch(
            peer_ip,
            self.wake.register(peer_ip),
        )));
        Ok(source)
    }

    #[cfg(target_os = "macos")]
    async fn mac_session_source(
        &self,
        root: &Arc<MacRoot>,
        local_ip: Ipv4Addr,
        peer_ip: Ipv4Addr,
        remote_match: Option<PtpRemoteMatch>,
    ) -> io::Result<Self> {
        let forced_system = ptp_timesync::test_hooks::forced_system();
        let availability = if forced_system {
            None
        } else {
            Some(
                tokio::task::spawn_blocking(ptp_timesync::availability)
                    .await
                    .unwrap_or(ptp_timesync::Availability::Unavailable("probe_panicked")),
            )
        };
        let peer_is_local = is_local_peer(peer_ip, local_ip, &local_interface_ipv4());
        let precheck = instance_precheck(&ptp_timesync::BREAKER, peer_ip, peer_is_local);
        let instance = match (availability, precheck) {
            (None, _) => Err("forced_system"),
            (_, Some(reason)) => Err(reason),
            (Some(ptp_timesync::Availability::Available { .. }), None) => {
                Ok(ptp_timesync::acquire(local_ip, peer_ip, remote_match))
            }
            (Some(ptp_timesync::Availability::Unavailable(reason)), None) => Err(reason),
        };
        let attempted = instance.is_ok();
        let system = root.clock.as_ref().map(|clock| {
            let gate = Arc::clone(&root.gate);
            move || -> io::Result<MacSession> {
                let permit = gate.try_acquire_owned().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "macOS system PTP clock already follows another AirPlay 2 sender",
                    )
                })?;
                let port = clock
                    .add_ipv4_port(local_ip, peer_ip)
                    .map_err(mac_ptp_io_error)?;
                // Current Apple senders commonly omit ClockID/ClockPorts from
                // timingPeerInfo. Keep CoreMedia's default receive matching in
                // that case; Apple's AirPlaySupport likewise applies an
                // override only when the sender provides a complete
                // sourcePortIdentity tuple.
                if let Some(remote_match) = remote_match {
                    port.override_receive_matching(
                        remote_match.clock_identity,
                        remote_match.port_number,
                    )
                    .map_err(mac_ptp_io_error)?;
                }
                Ok(MacSession::System {
                    port,
                    _permit: permit,
                    peer_ports: Mutex::new(HashMap::new()),
                })
            }
        });
        let (choice, fallback_reason) = select_backend(forced_system, instance, system).await?;
        if attempted && fallback_reason.is_some() {
            INSTANCE_CREATE_FAILURES.fetch_add(1, Ordering::Relaxed);
            crate::telemetry::record_ptp_instance_create_failure(
                crate::telemetry::operation_token(),
            );
        }
        let (backend, session) = match choice {
            BackendChoice::Instance(port) => {
                INSTANCE_SESSIONS.fetch_add(1, Ordering::Relaxed);
                (PtpBackendKind::MacInstance, MacSession::Instance(port))
            }
            BackendChoice::System(session) => {
                SYSTEM_SESSIONS.fetch_add(1, Ordering::Relaxed);
                (PtpBackendKind::MacSystem, session)
            }
        };
        Ok(Self {
            samples: self.samples.clone(),
            wake: Arc::clone(&self.wake),
            backend,
            fallback_reason,
            session: Some(Arc::new(SessionShared::new(IpAddr::V4(peer_ip)))),
            mac_root: Some(Arc::clone(root)),
            mac_session: Some(Arc::new(session)),
            #[cfg(test)]
            fake_platform: None,
        })
    }

    /// Map a buffered RTP timestamp directly through the macOS PTP backend.
    /// `None` means this source uses the portable UDP observer/mapper.
    pub(crate) fn platform_deadline_for_rtp(
        &self,
        anchor: PlaybackAnchor,
        packet_rtp: u32,
        sample_rate: u32,
        _telemetry_token: crate::telemetry::OperationToken,
    ) -> Option<Result<Instant, PtpClockError>> {
        #[cfg(test)]
        if let Some(fake) = self.fake_platform.as_ref() {
            return Some(fake.deadline(anchor, packet_rtp, sample_rate));
        }
        #[cfg(target_os = "macos")]
        if let Some(session) = self.mac_session.as_deref() {
            let remote_time_ns = match remote_time_for_rtp(anchor, packet_rtp, sample_rate) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            return Some(match session {
                MacSession::System { port, .. } => port
                    .map_remote_time(remote_time_ns, anchor.timeline_id)
                    .map_err(|error| match error {
                        MacPtpError::NotLocked => PtpClockError::NotReady,
                        MacPtpError::GrandmasterMismatch { .. } => PtpClockError::SourceMismatch,
                        _ => PtpClockError::PlatformClockUnavailable,
                    }),
                MacSession::Instance(port) => {
                    port.map_remote_time(remote_time_ns, anchor.timeline_id)
                }
            });
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (anchor, packet_rtp, sample_rate);

        None
    }

    pub(crate) fn local_port_identity(&self) -> Option<(u64, u16)> {
        #[cfg(target_os = "macos")]
        {
            self.mac_session.as_deref().map(|session| match session {
                MacSession::System { port, .. } => (port.clock_identity(), port.port_number()),
                MacSession::Instance(port) => (port.clock_identity(), port.port_number()),
            })
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    #[cfg(test)]
    pub(crate) fn test_source() -> Self {
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        Self::portable(samples, Arc::default())
    }

    /// Portable session source registered with `wake` like `for_session`
    /// registers with its observer's registry.
    #[cfg(test)]
    pub(crate) fn test_watched_session_source(control_peer: IpAddr, wake: &Arc<WakeRegistry>) -> Self {
        let mut source = Self::test_source();
        source.wake = Arc::clone(wake);
        source.session = Some(Arc::new(SessionShared::with_clock_watch(
            control_peer,
            wake.register(control_peer),
        )));
        source
    }

    /// Portable session source for `control_peer` (what `for_session`
    /// returns off macOS).
    #[cfg(test)]
    pub(crate) fn test_session_source(control_peer: IpAddr) -> Self {
        let mut source = Self::test_source();
        source.session = Some(Arc::new(SessionShared::new(control_peer)));
        source
    }

    /// Session source whose deadlines and handoffs come from a fake
    /// platform clock (the macOS backends' contract).
    #[cfg(test)]
    pub(crate) fn test_platform_source(control_peer: IpAddr, fake: Arc<FakePlatform>) -> Self {
        let mut source = Self::test_session_source(control_peer);
        source.backend = PtpBackendKind::MacInstance;
        source.fake_platform = Some(fake);
        source
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
    completed_pairs: HashMap<SyncKey, (Instant, crate::telemetry::OperationToken)>,
    wake: Arc<WakeRegistry>,
    /// This receiver's clockIdentity (wake-up Announces; an Announce naming
    /// it as grandmaster is ignored).
    own_identity: u64,
    wake_sequence: u16,
    /// Wake-up Announces waiting for their send time, in due order.
    outgoing: VecDeque<WakeDatagram>,
}

impl PtpObserver {
    fn new(
        event_socket: Option<UdpSocket>,
        general_socket: Option<UdpSocket>,
        samples: broadcast::Sender<PtpClockSample>,
        wake: Arc<WakeRegistry>,
        own_identity: u64,
    ) -> Self {
        Self {
            event_socket,
            general_socket,
            samples,
            announcements: HashMap::with_capacity(8),
            pending_syncs: HashMap::with_capacity(16),
            pending_follow_ups: HashMap::with_capacity(16),
            completed_pairs: HashMap::with_capacity(16),
            wake,
            own_identity,
            wake_sequence: 0,
            outgoing: VecDeque::new(),
        }
    }

    fn completed_pair_is_duplicate(&self, key: SyncKey) -> bool {
        self.completed_pairs.contains_key(&key)
    }

    /// Bind the standard AirPlay PTP ports before advertising the receiver.
    /// Failure is fatal: continuing would claim a timing capability that this
    /// process cannot actually observe. On macOS that means neither the
    /// CoreMedia system clock nor TimeSync instances are usable.
    pub(crate) async fn bind_ipv4(own_identity: u64) -> io::Result<(Self, PtpClockSource)> {
        #[cfg(target_os = "macos")]
        {
            let _ = own_identity;
            Self::bind_macos().await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let bound = Self::bind(
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, PTP_EVENT_PORT)),
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, PTP_GENERAL_PORT)),
                own_identity,
            )
            .await?;
            PLATFORM.store(PLATFORM_PORTABLE, Ordering::Relaxed);
            Ok(bound)
        }
    }

    /// Isolated binding for in-process daemon tests. Real receivers must use
    /// the well-known ports; this exists because Cargo builds a dependency
    /// without `cfg(test)` even when its caller's tests run in parallel.
    pub(crate) async fn bind_ephemeral(own_identity: u64) -> io::Result<(Self, PtpClockSource)> {
        Self::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            own_identity,
        )
        .await
    }

    async fn bind(
        event_address: SocketAddr,
        general_address: SocketAddr,
        own_identity: u64,
    ) -> io::Result<(Self, PtpClockSource)> {
        let event_socket = UdpSocket::bind(event_address).await?;
        let general_socket = UdpSocket::bind(general_address).await?;
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let wake = Arc::new(WakeRegistry::default());
        let source = PtpClockSource::portable(samples.clone(), Arc::clone(&wake));
        Ok((
            Self::new(
                Some(event_socket),
                Some(general_socket),
                samples,
                wake,
                own_identity,
            ),
            source,
        ))
    }

    #[cfg(target_os = "macos")]
    async fn bind_macos() -> io::Result<(Self, PtpClockSource)> {
        let clock = match MacPtpClock::new() {
            Ok(clock) => Some(clock),
            Err(error) => {
                // The per-sender backend alone is enough to receive PTP
                // senders; only both missing is fatal.
                let availability = tokio::task::spawn_blocking(ptp_timesync::availability)
                    .await
                    .unwrap_or(ptp_timesync::Availability::Unavailable("probe_panicked"));
                if !availability.is_available() {
                    return Err(mac_ptp_io_error(error));
                }
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "CoreMedia PTP clock unavailable ({error}); using TimeSync instances only"
                );
                None
            }
        };
        PLATFORM.store(
            if clock.is_some() {
                PLATFORM_MAC_WITH_SYSTEM_CLOCK
            } else {
                PLATFORM_MAC_WITHOUT_SYSTEM_CLOCK
            },
            Ordering::Relaxed,
        );
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        // TimeSync owns UDP 319/320 and takes part in PTP itself: nothing
        // for this process to count or wake.
        let wake = Arc::new(WakeRegistry::default());
        let mut source = PtpClockSource::portable(samples.clone(), Arc::clone(&wake));
        source.backend = PtpBackendKind::MacSystem;
        source.mac_root = Some(Arc::new(MacRoot {
            clock,
            gate: Arc::new(Semaphore::new(1)),
        }));
        Ok((Self::new(None, None, samples, wake, 0), source))
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
        let mut prune_tick = tokio::time::interval(Duration::from_millis(250));
        prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let next_wake = self.outgoing.front().map(|datagram| datagram.due);
            tokio::select! {
                received = event_socket.recv_from(&mut event) => {
                    if let Some((length, peer)) = tolerate_reset(received)? {
                        self.receive(&event[..length], peer, PTP_EVENT_PORT, Instant::now());
                    }
                }
                received = general_socket.recv_from(&mut general) => {
                    if let Some((length, peer)) = tolerate_reset(received)? {
                        self.receive(&general[..length], peer, PTP_GENERAL_PORT, Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(next_wake.unwrap_or_else(Instant::now).into()), if next_wake.is_some() => {
                    self.send_due_wake(&general_socket, Instant::now());
                }
                _ = prune_tick.tick() => {
                    self.prune(Instant::now());
                }
            }
        }
    }

    /// Send every wake-up Announce that is due. A refused retraction is
    /// retried: it must not stay unsent after the better Announce went out.
    fn send_due_wake(&mut self, socket: &UdpSocket, now: Instant) {
        while self.outgoing.front().is_some_and(|datagram| datagram.due <= now) {
            let Some(mut datagram) = self.outgoing.pop_front() else {
                break;
            };
            let sent = match socket.try_send_to(&datagram.packet, datagram.to) {
                Ok(_) => true,
                Err(error) => {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 PTP wake-up Announce to {} not sent (retraction={}): {error}",
                        datagram.to,
                        datagram.retraction
                    );
                    false
                }
            };
            if datagram.retraction {
                if !sent && datagram.resends < WAKE_MAX_RESENDS {
                    datagram.resends += 1;
                    datagram.due = now + WAKE_RESEND_DELAY;
                    datagram.pair_ok = false;
                    self.queue_wake(datagram);
                    continue;
                }
                crate::telemetry::record_ptp_wake_sequence(
                    datagram.telemetry_token,
                    datagram.to.ip(),
                    sent && datagram.pair_ok,
                );
            } else if !sent {
                if let Some(retraction) = self
                    .outgoing
                    .iter_mut()
                    .find(|next| next.retraction && next.to == datagram.to)
                {
                    retraction.pair_ok = false;
                }
            }
        }
    }

    fn queue_wake(&mut self, datagram: WakeDatagram) {
        let index = self
            .outgoing
            .iter()
            .position(|queued| queued.due > datagram.due)
            .unwrap_or(self.outgoing.len());
        self.outgoing.insert(index, datagram);
    }

    /// After an Announce from `peer`: wake its clock if a session waits on
    /// it in vain (see [`WakeRegistry`]).
    fn maybe_wake(&mut self, peer: IpAddr, now: Instant) {
        let Some((clock, attempt)) = self.wake.wake_due(peer, self.own_identity, now) else {
            return;
        };
        let Some((better, worse)) = wake_priorities(clock.priority1) else {
            return;
        };
        let telemetry_token = crate::telemetry::operation_token();
        let first = self.wake_sequence;
        self.wake_sequence = self.wake_sequence.wrapping_add(2);
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP sender {} clock {:016x} announces itself as grandmaster (priority1 {} priority2 {}) but sends no Sync/Follow_Up; sending wake-up Announce pair {attempt}/{WAKE_MAX_ATTEMPTS} to {} (priority1 {better} then {worse}, own clock {:016x})",
            peer,
            clock.clock,
            clock.priority1,
            clock.priority2,
            clock.from,
            self.own_identity
        );
        for (offset, priority1, retraction) in [
            (Duration::ZERO, better, false),
            (WAKE_INFERIOR_DELAY, worse, true),
        ] {
            let sequence = first.wrapping_add(u16::from(retraction));
            self.queue_wake(WakeDatagram {
                due: now + offset,
                to: clock.from,
                packet: wake_announce(self.own_identity, sequence, priority1, clock.priority2),
                retraction,
                resends: 0,
                pair_ok: true,
                telemetry_token,
            });
        }
    }

    /// Count one parsed datagram from `peer` for the sessions it serves.
    fn note_sender_message(&self, peer: SocketAddr, message: Message, received_at: Instant) {
        self.wake.observe(peer.ip(), |target| {
            let status = &mut target.status;
            match message {
                Message::Announce {
                    header,
                    grandmaster,
                    priority1,
                    priority2,
                } => {
                    status.announces += 1;
                    status.first_announce_at.get_or_insert(received_at);
                    status.last_announce_at = Some(received_at);
                    status.grandmaster = Some(grandmaster);
                    target.announced = Some(AnnouncedClock {
                        from: peer,
                        clock: header.source.clock,
                        grandmaster,
                        priority1,
                        priority2,
                    });
                }
                Message::Sync {
                    one_step_origin_ns: None,
                    ..
                } => status.sync_two_step += 1,
                Message::Sync {
                    one_step_origin_ns: Some(_),
                    ..
                } => status.sync_one_step += 1,
                Message::FollowUp { .. } => status.follow_ups += 1,
                Message::Signaling { .. } => status.signaling += 1,
            }
        });
    }

    fn receive(&mut self, packet: &[u8], peer: SocketAddr, local_port: u16, received_at: Instant) {
        let telemetry_token = crate::telemetry::operation_token();
        // AirPlay uses symmetric well-known ports. Requiring this also keeps a
        // random LAN datagram from becoming a clock observation accidentally.
        if peer.port() != local_port {
            crate::telemetry::record_ptp_datagram(
                telemetry_token,
                peer.ip(),
                peer.port(),
                local_port,
                None,
                false,
                "source_port_mismatch",
            );
            return;
        }
        let message = match parse_message(packet) {
            Ok(message) => message,
            Err(error) => {
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    None,
                    false,
                    error.telemetry_reason(),
                );
                return;
            }
        };
        let message_name = message.telemetry_name();
        let expected_port = match message {
            Message::Sync { .. } => PTP_EVENT_PORT,
            Message::Announce { .. } | Message::FollowUp { .. } | Message::Signaling { .. } => {
                PTP_GENERAL_PORT
            }
        };
        if local_port != expected_port {
            crate::telemetry::record_ptp_datagram(
                telemetry_token,
                peer.ip(),
                peer.port(),
                local_port,
                Some(message_name),
                false,
                "message_port_mismatch",
            );
            return;
        }

        if let Message::Announce { grandmaster, .. } = message {
            if grandmaster == self.own_identity {
                // Only a sender reacting to our wake-up Announce could name
                // this receiver; it never sends Sync, so never follow that.
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    false,
                    "self_grandmaster",
                );
                return;
            }
        }

        self.prune(received_at);
        self.note_sender_message(peer, message, received_at);
        match message {
            Message::Signaling { .. } => {
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    true,
                    "signaling_observed",
                );
            }
            Message::Announce {
                header,
                grandmaster,
                ..
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
                        if let Some(announcement) = self.announcements.remove(&oldest) {
                            crate::telemetry::record_ptp_pending_disposition(
                                announcement.telemetry_token,
                                "announcement_evicted",
                                1,
                            );
                        }
                    }
                }
                let previous = self
                    .announcements
                    .insert(
                        (peer.ip(), header.source),
                        Announcement {
                            grandmaster,
                            received_at,
                            telemetry_token,
                        },
                    )
                    .map(|announcement| announcement.grandmaster);
                if previous.is_some_and(|previous| previous != grandmaster) {
                    log::info!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 PTP peer {} now announces grandmaster {grandmaster:016x} (was {:016x})",
                        peer.ip(),
                        previous.unwrap_or_default()
                    );
                }
                note_observed_grandmaster(peer.ip(), grandmaster, received_at);
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    true,
                    "announcement_observed",
                );
                self.maybe_wake(peer.ip(), received_at);
            }
            Message::Sync {
                header,
                one_step_origin_ns: Some(origin_ns),
            } => {
                let key = SyncKey {
                    peer: peer.ip(),
                    source: header.source,
                    sequence: header.sequence,
                };
                if self.completed_pair_is_duplicate(key) {
                    crate::telemetry::record_ptp_datagram(
                        telemetry_token,
                        peer.ip(),
                        peer.port(),
                        local_port,
                        Some(message_name),
                        false,
                        "duplicate_completed_pair",
                    );
                    return;
                }
                let Some(announcement) = self.announcements.get(&(peer.ip(), header.source)) else {
                    crate::telemetry::record_ptp_datagram(
                        telemetry_token,
                        peer.ip(),
                        peer.port(),
                        local_port,
                        Some(message_name),
                        false,
                        "missing_announcement",
                    );
                    return;
                };
                let grandmaster = announcement.grandmaster;
                // A one-step Sync is a complete sample: its own origin
                // timestamp plus its correctionField.
                self.emit_sample(
                    key,
                    grandmaster,
                    origin_ns,
                    i128::from(header.correction_scaled_ns),
                    received_at,
                    received_at,
                    telemetry_token,
                );
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    true,
                    "sync_one_step_observed",
                );
            }
            Message::Sync {
                header,
                one_step_origin_ns: None,
            } => {
                let key = SyncKey {
                    peer: peer.ip(),
                    source: header.source,
                    sequence: header.sequence,
                };
                if self.completed_pair_is_duplicate(key) {
                    crate::telemetry::record_ptp_datagram(
                        telemetry_token,
                        peer.ip(),
                        peer.port(),
                        local_port,
                        Some(message_name),
                        false,
                        "duplicate_completed_pair",
                    );
                    return;
                }
                let Some(announcement) = self.announcements.get(&(peer.ip(), header.source)) else {
                    crate::telemetry::record_ptp_datagram(
                        telemetry_token,
                        peer.ip(),
                        peer.port(),
                        local_port,
                        Some(message_name),
                        false,
                        "missing_announcement",
                    );
                    return;
                };
                if self.pending_syncs.len() >= MAX_PENDING_SYNCS {
                    if let Some(oldest) = self
                        .pending_syncs
                        .iter()
                        .min_by_key(|(_, value)| value.received_at)
                        .map(|(key, _)| *key)
                    {
                        if let Some(pending) = self.pending_syncs.remove(&oldest) {
                            crate::telemetry::record_ptp_pending_disposition(
                                pending.telemetry_token,
                                "pending_sync_evicted",
                                1,
                            );
                        }
                    }
                }
                self.pending_syncs.entry(key).or_insert(PendingSync {
                    received_at,
                    correction_scaled_ns: header.correction_scaled_ns,
                    // Bind the pair to the grandmaster that was current
                    // when Sync arrived. A subsequent Announce must not
                    // relabel a partly received two-step exchange.
                    grandmaster: announcement.grandmaster,
                    telemetry_token,
                });
                self.emit_completed_pair(key, received_at);
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    true,
                    "sync_observed",
                );
            }
            Message::FollowUp { header, origin_ns } => {
                let key = SyncKey {
                    peer: peer.ip(),
                    source: header.source,
                    sequence: header.sequence,
                };
                if self.completed_pair_is_duplicate(key) {
                    crate::telemetry::record_ptp_datagram(
                        telemetry_token,
                        peer.ip(),
                        peer.port(),
                        local_port,
                        Some(message_name),
                        false,
                        "duplicate_completed_pair",
                    );
                    return;
                }
                if self.pending_follow_ups.len() >= MAX_PENDING_SYNCS {
                    if let Some(oldest) = self
                        .pending_follow_ups
                        .iter()
                        .min_by_key(|(_, value)| value.received_at)
                        .map(|(key, _)| *key)
                    {
                        if let Some(pending) = self.pending_follow_ups.remove(&oldest) {
                            crate::telemetry::record_ptp_pending_disposition(
                                pending.telemetry_token,
                                "pending_follow_up_evicted",
                                1,
                            );
                        }
                    }
                }
                self.pending_follow_ups
                    .entry(key)
                    .or_insert(PendingFollowUp {
                        received_at,
                        correction_scaled_ns: header.correction_scaled_ns,
                        origin_ns,
                        telemetry_token,
                    });
                self.emit_completed_pair(key, received_at);
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    true,
                    "follow_up_observed",
                );
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
        let sample_token = sync.telemetry_token;
        let separation = if sync.received_at >= follow_up.received_at {
            sync.received_at.duration_since(follow_up.received_at)
        } else {
            follow_up.received_at.duration_since(sync.received_at)
        };
        self.pending_syncs.remove(&key);
        self.pending_follow_ups.remove(&key);
        if separation > PAIR_LIFETIME {
            crate::telemetry::record_ptp_pending_disposition(sample_token, "pair_expired", 1);
            return;
        }

        let correction =
            i128::from(sync.correction_scaled_ns) + i128::from(follow_up.correction_scaled_ns);
        self.emit_sample(
            key,
            sync.grandmaster,
            follow_up.origin_ns,
            correction,
            sync.received_at,
            now,
            sample_token,
        );
    }

    /// Publish one sample: `origin_ns` corrected by `correction_scaled_ns`
    /// (2^-16 ns), received at `received_at` (the Sync's arrival).
    #[allow(clippy::too_many_arguments)]
    fn emit_sample(
        &mut self,
        key: SyncKey,
        grandmaster: u64,
        origin_ns: u64,
        correction_scaled_ns: i128,
        received_at: Instant,
        now: Instant,
        sample_token: crate::telemetry::OperationToken,
    ) {
        let corrected_scaled = i128::from(origin_ns) * 65_536 + correction_scaled_ns;
        let Ok(remote_time_ns) = u64::try_from(corrected_scaled.div_euclid(65_536)) else {
            crate::telemetry::record_ptp_pending_disposition(
                sample_token,
                "invalid_time_conversion",
                1,
            );
            return;
        };
        if self.completed_pairs.len() >= MAX_PENDING_SYNCS {
            if let Some(oldest) = self
                .completed_pairs
                .iter()
                .min_by_key(|(_, (received_at, _))| *received_at)
                .map(|(key, _)| *key)
            {
                if let Some((_, token)) = self.completed_pairs.remove(&oldest) {
                    crate::telemetry::record_ptp_pending_disposition(
                        token,
                        "completed_pair_evicted",
                        1,
                    );
                }
            }
        }
        self.completed_pairs.insert(key, (now, sample_token));
        let delivered = self
            .samples
            .send(PtpClockSample {
                peer: key.peer,
                grandmaster,
                remote_time_ns,
                received_at,
                telemetry_token: sample_token,
            })
            .is_ok();
        crate::telemetry::record_ptp_sample_emitted(sample_token, delivered);
    }

    fn prune(&mut self, now: Instant) {
        self.pending_syncs.retain(|_, sync| {
            let keep = now.saturating_duration_since(sync.received_at) <= PAIR_LIFETIME;
            if !keep {
                crate::telemetry::record_ptp_pending_disposition(
                    sync.telemetry_token,
                    "pending_sync_expired",
                    1,
                );
            }
            keep
        });
        self.pending_follow_ups.retain(|_, follow_up| {
            let keep = now.saturating_duration_since(follow_up.received_at) <= PAIR_LIFETIME;
            if !keep {
                crate::telemetry::record_ptp_pending_disposition(
                    follow_up.telemetry_token,
                    "pending_follow_up_expired",
                    1,
                );
            }
            keep
        });
        self.completed_pairs.retain(|_, (completed_at, token)| {
            let keep = now.saturating_duration_since(*completed_at) <= PAIR_LIFETIME;
            if !keep {
                crate::telemetry::record_ptp_pending_disposition(
                    *token,
                    "completed_pair_expired",
                    1,
                );
            }
            keep
        });
        self.announcements.retain(|_, announce| {
            let keep = now.saturating_duration_since(announce.received_at) <= ANNOUNCE_LIFETIME;
            if !keep {
                crate::telemetry::record_ptp_pending_disposition(
                    announce.telemetry_token,
                    "announcement_expired",
                    1,
                );
            }
            keep
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
        MESSAGE_SYNC if packet.len() >= PTP_SYNC_BYTES => Ok(Message::Sync {
            header,
            one_step_origin_ns: if header.flags & PTP_TWO_STEP_FLAG != 0 {
                None
            } else {
                Some(parse_timestamp(&packet[PTP_HEADER_BYTES..PTP_SYNC_BYTES])?)
            },
        }),
        MESSAGE_FOLLOW_UP if packet.len() >= PTP_SYNC_BYTES => Ok(Message::FollowUp {
            header,
            origin_ns: parse_timestamp(&packet[PTP_HEADER_BYTES..PTP_SYNC_BYTES])?,
        }),
        MESSAGE_ANNOUNCE if packet.len() >= PTP_ANNOUNCE_BYTES => Ok(Message::Announce {
            header,
            priority1: packet[47],
            priority2: packet[52],
            grandmaster: u64::from_be_bytes(
                packet[53..61]
                    .try_into()
                    .expect("announce minimum length checked"),
            ),
        }),
        MESSAGE_SIGNALING if packet.len() >= PTP_SIGNALING_BYTES => {
            Ok(Message::Signaling { header })
        }
        _ => Err(PtpParseError::UnsupportedMessage),
    }
}

/// A receive error on an unconnected UDP socket that only reports an ICMP
/// port-unreachable for an earlier send (Windows `WSAECONNRESET`, e.g.
/// after a wake-up Announce to a sender that has gone) must not end the
/// observer.
fn tolerate_reset<T>(received: io::Result<T>) -> io::Result<Option<T>> {
    match received {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
            log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP socket reported an unreachable peer: {error}"
            );
            Ok(None)
        }
        Err(error) => Err(error),
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

impl Message {
    fn telemetry_name(self) -> &'static str {
        match self {
            Self::Announce { .. } => "announce",
            Self::Sync {
                one_step_origin_ns: None,
                ..
            } => "sync",
            Self::Sync {
                one_step_origin_ns: Some(_),
                ..
            } => "sync_one_step",
            Self::FollowUp { .. } => "follow_up",
            Self::Signaling { .. } => "signaling",
        }
    }
}

impl PtpParseError {
    fn telemetry_reason(self) -> &'static str {
        match self {
            Self::WrongLength => "parse_wrong_length",
            Self::UnsupportedProfile => "parse_unsupported_profile",
            Self::UnsupportedMessage => "parse_unsupported_message",
            Self::InvalidTimestamp => "parse_invalid_timestamp",
        }
    }
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
    telemetry_token: crate::telemetry::OperationToken,
    samples: VecDeque<MapperSample>,
}

impl PtpClockMapper {
    pub(crate) fn new(
        peer: IpAddr,
        grandmaster: u64,
        epoch: Instant,
        telemetry_token: crate::telemetry::OperationToken,
        _media_kind: crate::telemetry::MediaKind,
    ) -> Self {
        crate::telemetry::record_ptp_mapper_selected(telemetry_token, peer, grandmaster);
        Self {
            epoch,
            peer,
            grandmaster,
            telemetry_token,
            samples: VecDeque::with_capacity(MAPPER_MAX_SAMPLES),
        }
    }

    pub(crate) fn grandmaster(&self) -> u64 {
        self.grandmaster
    }

    pub(crate) fn peer(&self) -> IpAddr {
        self.peer
    }

    pub(crate) fn last_sample_at(&self) -> Option<Instant> {
        self.samples.back().map(|sample| sample.received_at)
    }

    /// Current offset estimate (same rule as deadline mapping).
    pub(crate) fn estimate(&self, now: Instant) -> Result<PtpClockEstimate, PtpClockError> {
        self.estimate_at(now)
    }

    /// The estimate when it is safe to carry an anchor onto this mapping: at
    /// least three fresh samples agree with it. Guards against the first
    /// Syncs after a grandmaster change, whose Announce label and timestamp
    /// can briefly disagree.
    pub(crate) fn handoff_ready(&self, now: Instant) -> Option<PtpClockEstimate> {
        let estimate = self.estimate_at(now).ok()?;
        let supporting = self
            .samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.received_at) <= MAPPER_WINDOW)
            .filter(|sample| {
                (sample.local_minus_remote_ns - estimate.local_minus_remote_ns).abs()
                    <= HANDOFF_CONSISTENCY_NS
            })
            .count();
        (supporting >= MAPPER_MIN_SAMPLES).then_some(estimate)
    }

    pub(crate) fn observe(
        &mut self,
        sample: PtpClockSample,
    ) -> Result<Option<PtpClockEstimate>, PtpClockError> {
        if self.telemetry_token != sample.telemetry_token {
            self.telemetry_token = sample.telemetry_token;
            crate::telemetry::record_ptp_mapper_selected(
                sample.telemetry_token,
                self.peer,
                self.grandmaster,
            );
        }
        let telemetry_token = sample.telemetry_token;
        let sample_peer = sample.peer;
        let sample_grandmaster = sample.grandmaster;
        let result = self.observe_inner(sample);
        crate::telemetry::record_ptp_mapper_sample(
            telemetry_token,
            self.peer,
            sample_peer,
            sample_grandmaster,
            result.is_ok(),
            matches!(&result, Ok(Some(_))),
            result
                .as_ref()
                .err()
                .map_or("accepted", PtpClockError::telemetry_reason),
        );
        result
    }

    fn observe_inner(
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
        _telemetry_token: crate::telemetry::OperationToken,
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

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtpClockError {
    GenerationMismatch,
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
            Self::GenerationMismatch => "PTP sample belongs to another telemetry generation",
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

impl PtpClockError {
    pub(crate) fn telemetry_bit(self) -> u16 {
        1u16 << self as u8
    }

    pub(crate) fn telemetry_reason(&self) -> &'static str {
        match self {
            Self::GenerationMismatch => "generation_mismatch",
            Self::SourceMismatch => "source_mismatch",
            Self::NonMonotonicLocalTime => "non_monotonic_local_time",
            Self::OffsetJump => "offset_jump",
            Self::NotReady => "not_ready",
            Self::Stale => "stale",
            Self::InvalidSampleRate => "invalid_sample_rate",
            Self::AmbiguousRtpDelta => "ambiguous_rtp_delta",
            Self::MappedTimeOutOfRange => "mapped_time_out_of_range",
            Self::PlatformClockUnavailable => "platform_clock_unavailable",
        }
    }
}

impl std::error::Error for PtpClockError {}

/// Platform clock fake with the macOS backends' contract: it follows one
/// grandmaster at a time, maps only anchors on that grandmaster, never maps
/// through its own identity, and can hand off from the grandmaster it
/// followed last (TimeSync's retired line / CoreMedia's last mapping).
#[cfg(test)]
pub(crate) struct FakePlatform {
    epoch: Instant,
    own_identity: u64,
    state: Mutex<FakePlatformState>,
}

#[cfg(test)]
struct FakePlatformState {
    grandmaster: u64,
    /// Local ns since `epoch` minus remote ns.
    offset_ns: i128,
    retired: Option<(u64, i128)>,
}

#[cfg(test)]
impl FakePlatform {
    pub(crate) fn new(epoch: Instant, grandmaster: u64, own_identity: u64) -> Self {
        Self {
            epoch,
            own_identity,
            state: Mutex::new(FakePlatformState {
                grandmaster,
                offset_ns: 0,
                retired: None,
            }),
        }
    }

    /// Follow `grandmaster` with `offset_ns` (local - remote).
    pub(crate) fn lock(&self, grandmaster: u64, offset_ns: i128) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.grandmaster != grandmaster && state.grandmaster != self.own_identity {
            state.retired = Some((state.grandmaster, state.offset_ns));
        }
        state.grandmaster = grandmaster;
        state.offset_ns = offset_ns;
    }

    pub(crate) fn forget_retired(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retired = None;
    }

    fn grandmaster(&self) -> Option<u64> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.grandmaster != self.own_identity).then_some(state.grandmaster)
    }

    fn deadline(
        &self,
        anchor: PlaybackAnchor,
        packet_rtp: u32,
        sample_rate: u32,
    ) -> Result<Instant, PtpClockError> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.grandmaster == self.own_identity {
            return Err(PtpClockError::NotReady);
        }
        if anchor.timeline_id != state.grandmaster {
            return Err(PtpClockError::SourceMismatch);
        }
        let remote = i128::from(remote_time_for_rtp(anchor, packet_rtp, sample_rate)?);
        let local = u64::try_from(remote + state.offset_ns)
            .map_err(|_| PtpClockError::MappedTimeOutOfRange)?;
        self.epoch
            .checked_add(Duration::from_nanos(local))
            .ok_or(PtpClockError::MappedTimeOutOfRange)
    }

    fn handoff(&self, from: u64) -> Result<PlatformHandoff, &'static str> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.grandmaster == self.own_identity {
            return Err("new_grandmaster_not_locked");
        }
        if state.grandmaster == from {
            return Err("no_grandmaster_change");
        }
        let (_, old_offset) = state
            .retired
            .filter(|(grandmaster, _)| *grandmaster == from)
            .ok_or("old_mapping_unavailable")?;
        Ok(PlatformHandoff {
            grandmaster: state.grandmaster,
            delta_ns: old_offset - state.offset_ns,
        })
    }
}

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
    const OWN_CLOCK: u64 = 0x02aa_bbff_fecc_ddee;

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
        timestamp(&mut one_step, 3, 7);
        assert!(matches!(
            parse_message(&one_step),
            Ok(Message::Sync {
                header: Header { sequence: 8, .. },
                one_step_origin_ns: Some(3_000_000_007),
            })
        ));
        one_step[40..44].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        assert_eq!(parse_message(&one_step), Err(PtpParseError::InvalidTimestamp));

        let delay_request = header(0x1, PTP_SYNC_BYTES, 9, 0);
        assert_eq!(
            parse_message(&delay_request),
            Err(PtpParseError::UnsupportedMessage)
        );
        let short_signaling = header(MESSAGE_SIGNALING, PTP_SIGNALING_BYTES - 1, 1, 0);
        assert_eq!(
            parse_message(&short_signaling),
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
                priority1: 250,
                priority2: 239,
            })
        ));
        assert!(matches!(
            parse_message(&sync),
            Ok(Message::Sync {
                header: Header {
                    sequence: 1,
                    flags: 0x0608,
                    ..
                },
                one_step_origin_ns: None,
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
        let mut observer = PtpObserver::new(Some(event_socket), Some(general_socket), samples, Arc::default(), OWN_CLOCK);
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
        let mut observer = PtpObserver::new(Some(event_socket), Some(general_socket), samples, Arc::default(), OWN_CLOCK);
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
    fn telemetry_reset_preserves_ptp_announce_and_pending_pair_state() {
        // Global recorder: serialized against concurrent `telemetry::reset`.
        let _telemetry = crate::telemetry::test_serial();
        let (samples, mut receiver) = broadcast::channel(8);
        let mut observer = PtpObserver::new(None, None, samples, Arc::default(), OWN_CLOCK);
        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        let base = Instant::now();
        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(
            &announce,
            SocketAddr::new(peer, PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            base,
        );

        let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 17, 0);
        observer.receive(
            &sync,
            SocketAddr::new(peer, PTP_EVENT_PORT),
            PTP_EVENT_PORT,
            base + Duration::from_millis(1),
        );
        crate::telemetry::reset();

        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, 17, 0);
        timestamp(&mut follow, 12, 34);
        observer.receive(
            &follow,
            SocketAddr::new(peer, PTP_GENERAL_PORT),
            PTP_GENERAL_PORT,
            base + Duration::from_millis(2),
        );

        let sample = receiver.try_recv().unwrap();
        assert_eq!(sample.peer, peer);
        assert_eq!(sample.grandmaster, GRANDMASTER);
        assert_eq!(sample.remote_time_ns, 12_000_000_034);
    }

    #[test]
    fn telemetry_reset_preserves_mapper_samples_and_existing_anchor() {
        // Global recorder: serialized against concurrent `telemetry::reset`.
        let _telemetry = crate::telemetry::test_serial();
        let epoch = Instant::now();
        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        let original_token = crate::telemetry::operation_token();
        let mut mapper = PtpClockMapper::new(
            peer,
            GRANDMASTER,
            epoch,
            original_token,
            crate::telemetry::MediaKind::Buffered,
        );
        for index in 1..=3u64 {
            mapper
                .observe(PtpClockSample {
                    peer,
                    grandmaster: GRANDMASTER,
                    remote_time_ns: index * 1_000,
                    received_at: epoch + Duration::from_nanos(index * 1_000 + 100),
                    telemetry_token: original_token,
                })
                .unwrap();
        }
        let anchor = PlaybackAnchor {
            timeline_id: GRANDMASTER,
            remote_time_ns: 10_000,
            rtp_time: 100,
            telemetry_token: original_token,
        };

        crate::telemetry::reset();
        let current_token = crate::telemetry::operation_token();
        assert_eq!(
            mapper
                .deadline_for_rtp(
                    epoch + Duration::from_nanos(3_100),
                    anchor,
                    100,
                    44_100,
                    current_token,
                )
                .unwrap(),
            epoch + Duration::from_nanos(10_100)
        );

        let estimate = mapper
            .observe(PtpClockSample {
                peer,
                grandmaster: GRANDMASTER,
                remote_time_ns: 4_000,
                received_at: epoch + Duration::from_nanos(4_100),
                telemetry_token: current_token,
            })
            .unwrap()
            .unwrap();
        assert_eq!(estimate.retained_samples, 4);
    }

    #[test]
    fn mapper_requires_matching_fresh_grandmaster_and_maps_wrapping_rtp() {
        let epoch = Instant::now();
        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        let telemetry_token = crate::telemetry::operation_token();
        let mut mapper = PtpClockMapper::new(
            peer,
            GRANDMASTER,
            epoch,
            telemetry_token,
            crate::telemetry::MediaKind::Realtime,
        );
        for index in 1..=3u64 {
            let remote = index * 1_000;
            mapper
                .observe(PtpClockSample {
                    peer,
                    grandmaster: GRANDMASTER,
                    remote_time_ns: remote,
                    received_at: epoch + Duration::from_nanos(remote + 100),
                    telemetry_token,
                })
                .unwrap();
        }
        let anchor = PlaybackAnchor {
            timeline_id: GRANDMASTER,
            remote_time_ns: 10_000,
            rtp_time: 0xffff_fff0,
            telemetry_token,
        };
        assert_eq!(
            mapper
                .deadline_for_rtp(
                    epoch + Duration::from_nanos(3_100),
                    anchor,
                    0x0000_03f0,
                    44_100,
                    telemetry_token,
                )
                .unwrap(),
            epoch + Duration::from_nanos(10_000 + 23_219_955 + 100)
        );
        assert_eq!(
            mapper.deadline_for_rtp(
                epoch + Duration::from_secs(3),
                anchor,
                anchor.rtp_time,
                44_100,
                telemetry_token,
            ),
            Err(PtpClockError::Stale)
        );
    }

    // ------------------------------------------------- backend fallback chain

    #[derive(Debug)]
    struct FakeInstance(u8);

    /// System backend fake with the real one-sender gate semantics.
    fn fake_system(
        gate: &std::sync::Arc<tokio::sync::Semaphore>,
    ) -> impl FnOnce() -> io::Result<tokio::sync::OwnedSemaphorePermit> {
        let gate = std::sync::Arc::clone(gate);
        move || {
            gate.try_acquire_owned()
                .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "gate taken"))
        }
    }

    #[tokio::test]
    async fn create_failure_falls_back_to_system_and_a_second_system_session_would_block() {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (first, reason) = select_backend(
            false,
            Ok(async { Err::<FakeInstance, _>("create_failed") }),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(first, BackendChoice::System(_)));
        assert_eq!(reason, Some("create_failed"));

        let second = select_backend(
            false,
            Ok(async { Err::<FakeInstance, _>("port_failed") }),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::WouldBlock, "RTSP 453");

        // Instance sessions are not constrained by the system gate.
        for index in 0..3u8 {
            let (choice, reason) = select_backend(
                false,
                Ok(async move { Ok::<_, &'static str>(FakeInstance(index)) }),
                Some(fake_system(&gate)),
            )
            .await
            .unwrap();
            assert!(matches!(choice, BackendChoice::Instance(FakeInstance(i)) if i == index));
            assert_eq!(reason, None);
        }
        drop(first);
        let (again, _) = select_backend(
            false,
            Err::<std::future::Ready<Result<FakeInstance, &'static str>>, _>("tripped"),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(again, BackendChoice::System(_)), "gate released with its session");
    }

    #[tokio::test]
    async fn forced_system_unavailable_and_missing_clock_follow_the_chain() {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (choice, reason) = select_backend(
            true,
            Ok(async { Ok::<_, &'static str>(FakeInstance(1)) }),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(choice, BackendChoice::System(_)), "forced system skips step 2");
        assert_eq!(reason, Some("forced_system"));
        drop(choice);

        let (choice, reason) = select_backend(
            false,
            Err::<std::future::Ready<Result<FakeInstance, &'static str>>, _>("missing_class"),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(choice, BackendChoice::System(_)));
        assert_eq!(reason, Some("missing_class"));

        let none = select_backend(
            false,
            Err::<std::future::Ready<Result<FakeInstance, &'static str>>, _>("bad_encoding"),
            None::<fn() -> io::Result<()>>,
        )
        .await
        .unwrap_err();
        assert_eq!(none.kind(), io::ErrorKind::Unsupported, "RTSP 500");

        let (choice, _) = select_backend(
            false,
            Ok(async { Ok::<_, &'static str>(FakeInstance(2)) }),
            None::<fn() -> io::Result<()>>,
        )
        .await
        .unwrap();
        assert!(matches!(choice, BackendChoice::Instance(_)), "instance alone suffices");
    }

    /// The instance argument of [`select_backend`] for a session whose
    /// precheck gave `precheck` and whose fake creation yields `created`.
    fn fake_instance(
        precheck: Option<&'static str>,
        created: Result<u8, &'static str>,
    ) -> Result<std::future::Ready<Result<FakeInstance, &'static str>>, &'static str> {
        match precheck {
            Some(reason) => Err(reason),
            None => Ok(std::future::ready(created.map(FakeInstance))),
        }
    }

    #[test]
    fn local_peer_recognizes_loopback_the_local_address_and_interface_addresses() {
        let local = Ipv4Addr::new(10, 130, 40, 39);
        let interfaces = [local, Ipv4Addr::new(192, 168, 64, 1)];
        assert!(is_local_peer(Ipv4Addr::LOCALHOST, local, &[]));
        assert!(is_local_peer(Ipv4Addr::new(127, 0, 0, 2), local, &[]));
        assert!(is_local_peer(local, local, &[]));
        assert!(is_local_peer(
            Ipv4Addr::new(192, 168, 64, 1),
            local,
            &interfaces
        ));
        assert!(!is_local_peer(
            Ipv4Addr::new(10, 130, 40, 41),
            local,
            &interfaces
        ));
    }

    #[tokio::test]
    async fn local_peer_uses_the_system_path_and_never_counts_toward_the_breaker() {
        let breaker = super::super::ptp_timesync::Breaker::new();
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let local = Ipv4Addr::new(10, 130, 40, 39);

        // The same-host sender would get an instance that never locks; it
        // must not even try.
        let precheck = instance_precheck(&breaker, local, is_local_peer(local, local, &[]));
        assert_eq!(precheck, Some("local_peer"));
        let (choice, reason) = select_backend(
            false,
            fake_instance(precheck, Ok(1)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(choice, BackendChoice::System(_)));
        assert_eq!(reason, Some("local_peer"));
        assert!(!breaker.is_tripped());
        assert!(!breaker.peer_failed(local));

        // A remote sender still gets its own instance while the local one
        // holds the system gate.
        let remote = Ipv4Addr::new(10, 130, 40, 41);
        let precheck = instance_precheck(&breaker, remote, is_local_peer(remote, local, &[local]));
        assert_eq!(precheck, None);
        let (remote_choice, reason) = select_backend(
            false,
            fake_instance(precheck, Ok(2)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(
            remote_choice,
            BackendChoice::Instance(FakeInstance(2))
        ));
        assert_eq!(reason, None);

        // The system clock still follows one sender: a second same-host
        // session while the first plays is RTSP 453, as in 1.0.2.
        let error = select_backend(
            false,
            fake_instance(instance_precheck(&breaker, local, true), Ok(3)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(choice);
    }

    #[tokio::test]
    async fn one_failing_peer_fails_only_itself_and_two_distinct_peers_trip_the_breaker() {
        let breaker = super::super::ptp_timesync::Breaker::new();
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let a = Ipv4Addr::new(10, 0, 0, 1);
        let b = Ipv4Addr::new(10, 0, 0, 2);
        let c = Ipv4Addr::new(10, 0, 0, 3);

        // Peer A gets an instance that then fails its lock watchdog.
        let (first_a, _) = select_backend(
            false,
            fake_instance(instance_precheck(&breaker, a, false), Ok(1)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(first_a, BackendChoice::Instance(_)));
        assert!(!breaker.note_watchdog_failure(a), "one peer does not trip");
        drop(first_a);

        // Peer B is unaffected and still gets an instance.
        assert_eq!(instance_precheck(&breaker, b, false), None);
        let (first_b, _) = select_backend(
            false,
            fake_instance(instance_precheck(&breaker, b, false), Ok(2)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(first_b, BackendChoice::Instance(FakeInstance(2))));

        // Peer A's next session goes to the system clock instead of failing
        // again after another ten seconds.
        let (again_a, reason) = select_backend(
            false,
            fake_instance(instance_precheck(&breaker, a, false), Ok(3)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(again_a, BackendChoice::System(_)));
        assert_eq!(reason, Some("peer_lock_failed"));

        // B fails too: two distinct peers and no success trip the breaker.
        assert!(breaker.note_watchdog_failure(b));
        assert!(breaker.is_tripped());
        drop(first_b);
        drop(again_a);
        let (third, reason) = select_backend(
            false,
            fake_instance(instance_precheck(&breaker, c, false), Ok(4)),
            Some(fake_system(&gate)),
        )
        .await
        .unwrap();
        assert!(matches!(third, BackendChoice::System(_)));
        assert_eq!(reason, Some("tripped"));
    }

    #[test]
    fn backend_kind_names_match_the_ipc_vocabulary() {
        assert_eq!(PtpBackendKind::Portable.as_str(), "portable");
        assert_eq!(PtpBackendKind::MacInstance.as_str(), "instance");
        assert_eq!(PtpBackendKind::MacSystem.as_str(), "system");
    }

    // ------------------------------------------------- Windows observer

    #[test]
    fn interleaved_samples_from_two_senders_reach_only_their_own_mapper() {
        let (samples, mut receiver) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let mut observer = PtpObserver::new(None, None, samples, Arc::default(), OWN_CLOCK);
        let senders: [(IpAddr, u64, u64); 2] = [
            ("192.0.2.10".parse().unwrap(), 0xaa00_0000_0000_0a01, 1_000),
            ("192.0.2.11".parse().unwrap(), 0xbb00_0000_0000_0b02, 49_000),
        ];
        let base = Instant::now();
        for (peer, grandmaster, _) in senders {
            let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
            announce[53..61].copy_from_slice(&grandmaster.to_be_bytes());
            observer.receive(
                &announce,
                SocketAddr::new(peer, PTP_GENERAL_PORT),
                PTP_GENERAL_PORT,
                base,
            );
        }
        // Sync from both senders first, then the Follow_Ups in reverse order,
        // all with the same sequence numbers (each sender counts on its own).
        for round in 0..4u16 {
            let at = base + Duration::from_millis(125 * u64::from(round) + 1);
            for (peer, _, _) in senders {
                let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, round, 0);
                observer.receive(&sync, SocketAddr::new(peer, PTP_EVENT_PORT), PTP_EVENT_PORT, at);
            }
            for (peer, _, seconds) in senders.iter().rev() {
                let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, round, 0);
                // Remote time advances with the 125 ms Sync interval.
                timestamp(&mut follow, *seconds, u32::from(round) * 125_000_000);
                observer.receive(
                    &follow,
                    SocketAddr::new(*peer, PTP_GENERAL_PORT),
                    PTP_GENERAL_PORT,
                    at + Duration::from_micros(200),
                );
            }
        }
        let token = crate::telemetry::operation_token();
        let mut mappers: Vec<PtpClockMapper> = senders
            .iter()
            .map(|(peer, grandmaster, _)| {
                PtpClockMapper::new(*peer, *grandmaster, base, token, crate::telemetry::MediaKind::Buffered)
            })
            .collect();
        let mut emitted = 0;
        while let Ok(sample) = receiver.try_recv() {
            emitted += 1;
            for (index, mapper) in mappers.iter_mut().enumerate() {
                let own = sample.peer == senders[index].0;
                let result = mapper.observe(sample);
                if own {
                    assert_eq!(sample.grandmaster, senders[index].1);
                    assert!(result.is_ok(), "own sample accepted");
                } else {
                    assert_eq!(result, Err(PtpClockError::SourceMismatch));
                }
            }
        }
        assert_eq!(emitted, 8, "every interleaved pair completed exactly once");
        for (index, mapper) in mappers.iter().enumerate() {
            let anchor = PlaybackAnchor {
                timeline_id: senders[index].1,
                remote_time_ns: senders[index].2 * 1_000_000_000,
                rtp_time: 0,
                telemetry_token: token,
            };
            let deadline = mapper
                .deadline_for_rtp(base + Duration::from_millis(400), anchor, 0, 44_100, token)
                .unwrap();
            // Round 0 Sync arrived at base+1 ms carrying the anchor time.
            assert_eq!(deadline, base + Duration::from_millis(1));
        }
    }

    // ------------------------------------------------- silent sender clock

    /// The iPad's Announce and Signaling exactly as captured on 2026-11-28
    /// (30-win pktmon, airplay-ipad-ptp-capture-20261128T055102Z).
    const IPAD_ANNOUNCE: &str = concat!(
        "1b02004c00000408000000000000000000000000",
        "342b6edcbdd200088011000105fe00000000000000000000002500faf821436af6",
        "342b6edcbdd200080000a000080008342b6edcbdd20008"
    );
    const IPAD_SIGNALING: &str = concat!(
        "1c02006a00000408000000000000000000000000",
        "342b6edcbdd200088011000005800000000000000000000000030016000d93000001",
        "0003030100002710000027100000000000030020000d93000005000f0301000000",
        "00271000002710000027100000271000000000"
    );
    const IPAD_CLOCK: u64 = 0x342b_6edc_bdd2_0008;

    fn ipad_announce(sequence: u16) -> Vec<u8> {
        let mut packet = hex::decode(IPAD_ANNOUNCE).unwrap();
        packet[30..32].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    fn ipad_follow_up(sequence: u16, seconds: u64) -> Vec<u8> {
        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, sequence, 0);
        follow[20..28].copy_from_slice(&IPAD_CLOCK.to_be_bytes());
        follow[28..30].copy_from_slice(&0x8011u16.to_be_bytes());
        timestamp(&mut follow, seconds, 0);
        follow
    }

    fn general(peer: IpAddr) -> SocketAddr {
        SocketAddr::new(peer, PTP_GENERAL_PORT)
    }

    fn watched_observer(peer: IpAddr) -> (PtpObserver, Arc<WakeRegistry>, SenderClockWatch) {
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let wake = Arc::new(WakeRegistry::default());
        let watch = wake.register(peer);
        (
            PtpObserver::new(None, None, samples, Arc::clone(&wake), OWN_CLOCK),
            wake,
            watch,
        )
    }

    fn queued(observer: &PtpObserver) -> Vec<(Instant, SocketAddr, u16, u8, bool)> {
        observer
            .outgoing
            .iter()
            .map(|datagram| {
                let packet = &datagram.packet;
                (
                    datagram.due,
                    datagram.to,
                    u16::from_be_bytes([packet[30], packet[31]]),
                    packet[47],
                    datagram.retraction,
                )
            })
            .collect()
    }

    #[test]
    fn clock_identity_widens_the_device_mac_to_eui64() {
        assert_eq!(
            clock_identity_from_mac([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]),
            0x02aa_bbff_fecc_ddee
        );
    }

    #[test]
    fn wake_announce_matches_the_nqptp_layout_and_parses_as_announce() {
        let packet = wake_announce(OWN_CLOCK, 7, 249, 246);
        assert_eq!(packet.len(), 64);
        assert_eq!(&packet[..4], &[0x1b, 0x02, 0x00, 0x40]);
        assert_eq!(&packet[6..8], &[0x04, 0x08]);
        assert_eq!(&packet[28..30], &0x8008u16.to_be_bytes());
        assert_eq!(&packet[32..34], &[0x05, 0xfe]);
        assert_eq!(&packet[44..46], &37u16.to_be_bytes());
        assert_eq!(&packet[48..52], &0xf8fe_436au32.to_be_bytes());
        assert_eq!(&packet[61..63], &[0, 0], "stepsRemoved");
        assert_eq!(packet[63], 0xa0);
        assert!(matches!(
            parse_message(&packet),
            Ok(Message::Announce {
                header: Header {
                    sequence: 7,
                    source: PortIdentity { clock: OWN_CLOCK, port: 0x8008 },
                    ..
                },
                grandmaster: OWN_CLOCK,
                priority1: 249,
                priority2: 246,
            })
        ));
        assert_eq!(wake_priorities(250), Some((249, 251)));
        assert_eq!(wake_priorities(0), None);
        assert_eq!(wake_priorities(255), None);
    }

    #[test]
    fn captured_ipad_announce_and_signaling_parse() {
        assert!(matches!(
            parse_message(&hex::decode(IPAD_ANNOUNCE).unwrap()),
            Ok(Message::Announce {
                header: Header {
                    sequence: 1,
                    source: PortIdentity { clock: IPAD_CLOCK, port: 0x8011 },
                    ..
                },
                grandmaster: IPAD_CLOCK,
                priority1: 250,
                priority2: 246,
            })
        ));
        assert!(matches!(
            parse_message(&hex::decode(IPAD_SIGNALING).unwrap()),
            Ok(Message::Signaling { header: Header { sequence: 0, .. } })
        ));
    }

    #[test]
    fn silent_sender_clock_gets_a_better_then_worse_announce_and_bounded_retries() {
        let peer: IpAddr = "192.0.2.40".parse().unwrap();
        let (mut observer, _wake, watch) = watched_observer(peer);
        let signaling = hex::decode(IPAD_SIGNALING).unwrap();
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        for index in 0..2u16 {
            observer.receive(&ipad_announce(index + 1), general(peer), PTP_GENERAL_PORT, at(250 * u64::from(index)));
            observer.receive(&signaling, general(peer), PTP_GENERAL_PORT, at(250 * u64::from(index)));
        }
        assert!(observer.outgoing.is_empty(), "two Announces are not enough");
        observer.receive(&ipad_announce(3), general(peer), PTP_GENERAL_PORT, at(500));
        assert_eq!(
            queued(&observer),
            vec![
                (at(500), general(peer), 0, 249, false),
                (at(650), general(peer), 1, 251, true),
            ]
        );
        let status = watch.status().unwrap();
        assert_eq!((status.announces, status.signaling, status.wake_sequences), (3, 2, 1));
        assert_eq!(status.grandmaster, Some(IPAD_CLOCK));

        // Announces inside the retry interval do not repeat the pair.
        observer.outgoing.clear();
        for ms in [750, 1_000, 2_250] {
            observer.receive(&ipad_announce(4), general(peer), PTP_GENERAL_PORT, at(ms));
        }
        assert!(observer.outgoing.is_empty());
        // Retries 2 s apart, three pairs in all.
        observer.receive(&ipad_announce(5), general(peer), PTP_GENERAL_PORT, at(2_500));
        assert_eq!(queued(&observer).len(), 2);
        assert_eq!(queued(&observer)[0].2, 2, "fresh sequence numbers");
        observer.outgoing.clear();
        observer.receive(&ipad_announce(6), general(peer), PTP_GENERAL_PORT, at(4_500));
        assert_eq!(queued(&observer).len(), 2);
        observer.outgoing.clear();
        observer.receive(&ipad_announce(7), general(peer), PTP_GENERAL_PORT, at(6_500));
        observer.receive(&ipad_announce(8), general(peer), PTP_GENERAL_PORT, at(9_000));
        assert!(observer.outgoing.is_empty(), "at most three pairs per session");
        assert_eq!(watch.status().unwrap().wake_sequences, WAKE_MAX_ATTEMPTS);
    }

    #[test]
    fn a_clock_that_sends_follow_ups_is_never_woken() {
        let peer: IpAddr = "192.0.2.41".parse().unwrap();
        let (mut observer, _wake, watch) = watched_observer(peer);
        let base = Instant::now();
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, base);
        observer.receive(&ipad_follow_up(1, 10), general(peer), PTP_GENERAL_PORT, base);
        for index in 2..20u16 {
            let at = base + Duration::from_millis(250 * u64::from(index));
            observer.receive(&ipad_announce(index), general(peer), PTP_GENERAL_PORT, at);
        }
        assert!(observer.outgoing.is_empty());
        assert_eq!(watch.status().unwrap().follow_ups, 1);

        // Woken once, then silent no more: no further pair.
        let peer: IpAddr = "192.0.2.42".parse().unwrap();
        let (mut observer, _wake, _watch) = watched_observer(peer);
        for index in 0..3u16 {
            let at = base + Duration::from_millis(250 * u64::from(index));
            observer.receive(&ipad_announce(index), general(peer), PTP_GENERAL_PORT, at);
        }
        assert_eq!(observer.outgoing.len(), 2);
        observer.outgoing.clear();
        observer.receive(&ipad_follow_up(1, 10), general(peer), PTP_GENERAL_PORT, base + Duration::from_secs(1));
        observer.receive(&ipad_announce(9), general(peer), PTP_GENERAL_PORT, base + Duration::from_secs(5));
        assert!(observer.outgoing.is_empty());
    }

    #[test]
    fn only_a_watched_sender_that_is_its_own_grandmaster_is_woken() {
        let base = Instant::now();
        // Not registered by any session.
        let stranger: IpAddr = "192.0.2.43".parse().unwrap();
        let (mut observer, _wake, _watch) = watched_observer("192.0.2.44".parse().unwrap());
        for index in 0..6u16 {
            let at = base + Duration::from_millis(250 * u64::from(index));
            observer.receive(&ipad_announce(index), general(stranger), PTP_GENERAL_PORT, at);
        }
        assert!(observer.outgoing.is_empty());

        // A boundary clock relaying another grandmaster.
        let relay: IpAddr = "192.0.2.45".parse().unwrap();
        let (mut observer, _wake, _watch) = watched_observer(relay);
        for index in 0..6u16 {
            let mut announce = ipad_announce(index);
            announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
            let at = base + Duration::from_millis(250 * u64::from(index));
            observer.receive(&announce, general(relay), PTP_GENERAL_PORT, at);
        }
        assert!(observer.outgoing.is_empty());

        // The session ended: its registration is gone.
        let peer: IpAddr = "192.0.2.46".parse().unwrap();
        let (mut observer, wake, watch) = watched_observer(peer);
        drop(watch);
        assert!(wake.targets.lock().unwrap().is_empty());
        for index in 0..6u16 {
            let at = base + Duration::from_millis(250 * u64::from(index));
            observer.receive(&ipad_announce(index), general(peer), PTP_GENERAL_PORT, at);
        }
        assert!(observer.outgoing.is_empty());
    }

    #[test]
    fn an_announce_naming_this_receiver_as_grandmaster_is_ignored() {
        let peer: IpAddr = "192.0.2.47".parse().unwrap();
        let (samples, mut receiver) = broadcast::channel(8);
        let mut observer = PtpObserver::new(None, None, samples, Arc::default(), OWN_CLOCK);
        let base = Instant::now();
        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[53..61].copy_from_slice(&OWN_CLOCK.to_be_bytes());
        observer.receive(&announce, general(peer), PTP_GENERAL_PORT, base);
        assert!(observer.announcements.is_empty());
        let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 3, 0);
        observer.receive(&sync, SocketAddr::new(peer, PTP_EVENT_PORT), PTP_EVENT_PORT, base);
        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, 3, 0);
        timestamp(&mut follow, 1, 0);
        observer.receive(&follow, general(peer), PTP_GENERAL_PORT, base);
        assert!(receiver.try_recv().is_err(), "no sample on our own identity");
    }

    #[test]
    fn one_step_sync_is_a_complete_sample_with_its_correction() {
        let peer: IpAddr = "192.0.2.48".parse().unwrap();
        let (samples, mut receiver) = broadcast::channel(8);
        let mut observer = PtpObserver::new(None, None, samples, Arc::default(), OWN_CLOCK);
        let base = Instant::now();
        let mut one_step = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 5, 3 * 65_536 + 32_768);
        one_step[6..8].copy_from_slice(&0x0008u16.to_be_bytes());
        timestamp(&mut one_step, 7, 11);
        let event = SocketAddr::new(peer, PTP_EVENT_PORT);
        // Without an Announce the grandmaster is unknown.
        observer.receive(&one_step, event, PTP_EVENT_PORT, base);
        assert!(receiver.try_recv().is_err());

        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(&announce, general(peer), PTP_GENERAL_PORT, base);
        let at = base + Duration::from_millis(3);
        observer.receive(&one_step, event, PTP_EVENT_PORT, at);
        let sample = receiver.try_recv().unwrap();
        assert_eq!(sample.peer, peer);
        assert_eq!(sample.grandmaster, GRANDMASTER);
        // 7 s + 11 ns + 3.5 ns correction, floored.
        assert_eq!(sample.remote_time_ns, 7_000_000_014);
        assert_eq!(sample.received_at, at);
        // A repeated datagram is not a second sample.
        observer.receive(&one_step, event, PTP_EVENT_PORT, at + Duration::from_millis(1));
        assert!(receiver.try_recv().is_err());
        // One-step on the general port is a port mismatch.
        observer.receive(&one_step, general(peer), PTP_GENERAL_PORT, at);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn due_wake_announces_go_out_in_order_from_the_general_socket() {
        let general_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let sender_address = sender.local_addr().unwrap();
        let (samples, _) = broadcast::channel(8);
        let wake = Arc::new(WakeRegistry::default());
        let _watch = wake.register(sender_address.ip());
        let mut observer = PtpObserver::new(None, None, samples, Arc::clone(&wake), OWN_CLOCK);
        let base = Instant::now();
        for index in 0..3u16 {
            observer.receive(&ipad_announce(index), general(sender_address.ip()), PTP_GENERAL_PORT, base);
        }
        assert_eq!(observer.outgoing.len(), 2);
        assert!(observer.outgoing.iter().all(|datagram| datagram.to == general(sender_address.ip())));
        // Deliver to the test socket instead of the real UDP 320.
        for datagram in &mut observer.outgoing {
            datagram.to = sender_address;
        }
        observer.send_due_wake(&general_socket, base);
        assert_eq!(observer.outgoing.len(), 1, "the retraction waits 150 ms");
        observer.send_due_wake(&general_socket, base + WAKE_INFERIOR_DELAY);
        assert!(observer.outgoing.is_empty());
        let mut buffer = [0u8; 128];
        let mut priorities = Vec::new();
        for _ in 0..2 {
            let (length, from) = tokio::time::timeout(Duration::from_secs(2), sender.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(from, general_socket.local_addr().unwrap());
            assert_eq!(length, PTP_ANNOUNCE_BYTES);
            priorities.push(buffer[47]);
        }
        assert_eq!(priorities, vec![249, 251]);
    }

    #[test]
    fn a_connection_reset_does_not_end_the_observer() {
        let reset: io::Result<u8> = Err(io::Error::from(io::ErrorKind::ConnectionReset));
        assert!(matches!(tolerate_reset(reset), Ok(None)));
        let other: io::Result<u8> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(tolerate_reset(other).is_err());
        assert!(matches!(tolerate_reset(Ok(3u8)), Ok(Some(3))));
    }
}

/// Live fallback-chain checks (design §4.2 M4) on the macOS lab VM. Run one
/// process per mode, built with `--features airplay-test-hooks`:
/// `AUDIOHUB_TS_EXPECT=system_gate` with `AUDIOHUB_TEST_AIRPLAY_PTP_BACKEND=system`;
/// `AUDIOHUB_TS_EXPECT=fallback` with `AUDIOHUB_TEST_TIMESYNC_FAULT=missing_class`
/// or `create_fail`; `AUDIOHUB_TS_EXPECT=never_lock` with
/// `AUDIOHUB_TEST_TIMESYNC_FAULT=never_lock` (one failing peer only fails
/// itself, a second distinct one trips the breaker); `AUDIOHUB_TS_EXPECT=local_peer`
/// without hooks (the host's own address uses the system clock, a remote
/// peer still gets an instance). Peers come from
/// `AUDIOHUB_TS_LOCAL_IPV4` / `AUDIOHUB_TS_PEERS` (two IPv4 addresses).
#[cfg(all(test, target_os = "macos"))]
mod mac_live {
    use super::*;

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the macOS lab VM, test hooks and live peers"]
    async fn live_backend_fallback_chain() {
        let local: IpAddr = env("AUDIOHUB_TS_LOCAL_IPV4").parse().unwrap();
        let peers: Vec<IpAddr> = env("AUDIOHUB_TS_PEERS")
            .split(',')
            .map(|peer| peer.trim().parse().unwrap())
            .collect();
        let expect = env("AUDIOHUB_TS_EXPECT");
        let (_observer, root) = PtpObserver::bind_ipv4(0).await.expect("macOS PTP root");
        let describe = |source: &io::Result<PtpClockSource>| match source {
            Ok(source) => format!(
                "backend={} reason={:?} identity={:?}",
                source.backend().as_str(),
                source.fallback_reason(),
                source.local_port_identity().map(|(id, port)| format!("{id:016x}/{port}"))
            ),
            Err(error) => format!("error kind={:?} {error}", error.kind()),
        };
        match expect.as_str() {
            "system_gate" | "fallback" => {
                let first = root.for_session(local, peers[0], None).await;
                println!("M4 {expect} first: {}", describe(&first));
                let second = root.for_session(local, peers[1], None).await;
                println!("M4 {expect} second: {}", describe(&second));
                let first = first.expect("first session uses the system clock");
                assert_eq!(first.backend(), PtpBackendKind::MacSystem);
                assert!(first.fallback_reason().is_some());
                assert_eq!(second.err().map(|error| error.kind()), Some(io::ErrorKind::WouldBlock));
                drop(first);
                let third = root.for_session(local, peers[1], None).await;
                println!("M4 {expect} after release: {}", describe(&third));
                assert_eq!(third.unwrap().backend(), PtpBackendKind::MacSystem);
                println!("M4 {expect} status: {:?}", ptp_backend_status());
            }
            "never_lock" => {
                let first = root.for_session(local, peers[0], None).await;
                println!("M4 never_lock first: {}", describe(&first));
                let first = first.expect("instance session");
                assert_eq!(first.backend(), PtpBackendKind::MacInstance);
                let failed = first.failed().expect("instance failure signal");
                let started = Instant::now();
                let Some(MacSession::Instance(port)) = first.mac_session.as_deref() else {
                    panic!("instance session expected");
                };
                let mut last_error = None;
                while !*failed.borrow() && started.elapsed() < Duration::from_secs(20) {
                    last_error = port.map_remote_time(1_000_000_000_000, 0xaa).err();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let elapsed = started.elapsed();
                println!(
                    "M4 never_lock failed={} after {:?} last_map_error={last_error:?} status={:?}",
                    *failed.borrow(),
                    elapsed,
                    ptp_backend_status()
                );
                assert!(*failed.borrow());
                assert!(elapsed >= Duration::from_millis(9_900) && elapsed < Duration::from_secs(12));
                assert!(
                    !ptp_backend_status().tripped,
                    "one failing peer does not trip"
                );
                drop(first);
                let retry = root.for_session(local, peers[0], None).await;
                println!("M4 never_lock same peer again: {}", describe(&retry));
                let retry = retry.unwrap();
                assert_eq!(retry.backend(), PtpBackendKind::MacSystem);
                assert_eq!(retry.fallback_reason(), Some("peer_lock_failed"));
                drop(retry);
                let second = root.for_session(local, peers[1], None).await;
                println!("M4 never_lock second peer: {}", describe(&second));
                let second = second.expect("instance session");
                assert_eq!(second.backend(), PtpBackendKind::MacInstance);
                let failed = second.failed().expect("instance failure signal");
                let Some(MacSession::Instance(port)) = second.mac_session.as_deref() else {
                    panic!("instance session expected");
                };
                let started = Instant::now();
                while !*failed.borrow() && started.elapsed() < Duration::from_secs(20) {
                    let _ = port.map_remote_time(1_000_000_000_000, 0xaa);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                println!(
                    "M4 never_lock second failed={} status={:?}",
                    *failed.borrow(),
                    ptp_backend_status()
                );
                assert!(*failed.borrow());
                assert!(
                    ptp_backend_status().tripped,
                    "two distinct failing peers trip"
                );
                drop(second);
                let third = root.for_session(local, peers[1], None).await;
                println!("M4 never_lock after trip: {}", describe(&third));
                let third = third.unwrap();
                assert_eq!(third.backend(), PtpBackendKind::MacSystem);
                assert_eq!(third.fallback_reason(), Some("tripped"));
            }
            "local_peer" => {
                let own = root.for_session(local, local, None).await;
                println!("M4 local_peer own address: {}", describe(&own));
                let own = own.expect("same-host session uses the system clock");
                assert_eq!(own.backend(), PtpBackendKind::MacSystem);
                assert_eq!(own.fallback_reason(), Some("local_peer"));
                let remote = root.for_session(local, peers[0], None).await;
                println!("M4 local_peer remote: {}", describe(&remote));
                assert_eq!(remote.unwrap().backend(), PtpBackendKind::MacInstance);
                println!("M4 local_peer status: {:?}", ptp_backend_status());
                assert!(!ptp_backend_status().tripped);
            }
            other => panic!("unknown AUDIOHUB_TS_EXPECT {other}"),
        }
    }
}
