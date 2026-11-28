//! Minimal IEEE 802.1AS clock observer for AirPlay 2.
//!
//! AirPlay senders act as the timing grandmaster. AudioHub listens for the
//! sender's two-step Sync / Follow_Up exchange (or a one-step Sync) and maps
//! that remote timeline onto its local monotonic clock. It does not
//! implement multiroom topology; it follows the grandmaster the sender's
//! group elects, including changes announced through SETPEERS
//! (`ptp_session`).
//!
//! As a slave the observer transmits one kind of PTP message: the IEEE
//! 802.1AS message interval request (Signaling) that a slave port sends its
//! master (see [`SenderClockRegistry`]).
//!
//! Master mode: an iPad sender announces itself as grandmaster but never
//! sends Sync, neither to AudioHub nor to macOS's own AirPlay receiver. There
//! the receiver announces a better dataset at once, the iPad yields after a
//! few Announces and the receiver is the PTP master: Announce + Apple
//! Signaling 4/s, two-step Sync/Follow_Up 8/s, unicast to the iPad
//! (`airplay-ipad-applercv-20261128T075756Z`). When a session's control peer
//! announces itself as grandmaster and has sent no Sync/Follow_Up for
//! [`MASTER_TRIGGER_AFTER`], the portable observer does the same for that
//! peer only, on its own timescale ([`OwnTimescale`]), and publishes its own
//! Syncs as clock samples so the session maps an anchor on this receiver's
//! timeline exactly. Senders that send Sync (Mac, iPhone) never see it; a
//! sender that starts Sync, or announces a better grandmaster, makes it
//! yield for good (see [`MasterPort`]).

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
const MESSAGE_DELAY_REQ: u8 = 0x1;
const MESSAGE_DELAY_RESP: u8 = 0x9;
/// Delay_Resp: header, receiveTimestamp, requestingPortIdentity.
const PTP_DELAY_RESP_BYTES: usize = PTP_SYNC_BYTES + 10;
/// controlField of Delay_Resp (IEEE 1588-2008 table 23).
const CONTROL_DELAY_RESP: u8 = 0x03;
/// targetPortIdentity, the fixed part of a Signaling body.
const PTP_SIGNALING_BYTES: usize = PTP_HEADER_BYTES + 10;
/// A Signaling message carrying one 802.1AS message interval request TLV.
const PTP_INTERVAL_REQUEST_BYTES: usize = 60;
/// unicastFlag | ptpTimescale, as on every Apple PTP message.
const APPLE_UNICAST_FLAGS: u16 = 0x0408;
/// sourcePortIdentity.portNumber of everything this receiver sends
/// (interval requests and master mode). Any value but 0 and 0xffff is valid;
/// Apple numbers its unicast ports 0x80nn (the iPad used 0x8011, 0x8014,
/// 0x8015; the Mac 0x8015..0x8017). One fixed value keeps the identity
/// stable across sessions and restarts.
const OWN_PTP_PORT: u16 = 0x8001;
/// controlField "all others" and logMessageInterval 0x80, as the iPad sends.
const INTERVAL_REQUEST_CONTROL_FIELD: u8 = 0x05;
const INTERVAL_REQUEST_LOG_INTERVAL: u8 = 0x80;
/// ORGANIZATION_EXTENSION TLV, IEEE 802.1 OUI, subtype 2: message interval
/// request (802.1AS-2020 10.6.4.3).
const INTERVAL_REQUEST_TLV: [u8; 10] = [0x00, 0x03, 0x00, 0x0c, 0x00, 0x80, 0xc2, 0x00, 0x00, 0x02];
/// computeNeighborRateRatio | computeNeighborPropDelay, as the iPad sends.
const INTERVAL_REQUEST_TLV_FLAGS: u8 = 0x03;
/// linkDelayInterval, timeSyncInterval, announceInterval of the first
/// request: 126 = "set to the initial value" for all three.
const INTERVAL_REQUEST_INITIAL: [u8; 3] = [0x7e, 0x7e, 0x7e];
/// Later requests: -128 "no change", timeSyncInterval -3 (8 Sync a second),
/// -128 "no change".
const INTERVAL_REQUEST_SYNC_8HZ: [u8; 3] = [0x80, 0xfd, 0x80];
/// The iPad as slave sent the second request about 30 ms after the first
/// (2026-11-28 capture: 36 ms and 30 ms), then about once a second.
const INTERVAL_REQUEST_FOLLOW_DELAY: Duration = Duration::from_millis(30);
const INTERVAL_REQUEST_PERIOD: Duration = Duration::from_secs(1);
/// Bound on one send (interval request or master message) so a socket that
/// is not writable cannot stall the receive path.
const PTP_SEND_TIMEOUT: Duration = Duration::from_millis(50);
/// A session's control peer that announces itself as grandmaster and has
/// sent no Sync or Follow_Up this long after its first Announce gets this
/// receiver as its PTP master. Senders that are clock sources send Sync
/// within one Sync interval of announcing (macOS: 73 ms; Apple's rate is
/// 8/s), so at 500 ms a working sender has sent about four. It must also be
/// short: the iPad's first SETRATEANCHORTIME came 1.7-2.3 s after its first
/// Announce on 30-win (2026-11-28 10:23Z), and it needs about two of our
/// Announces (0.3 s at Apple's receiver) to yield and some Syncs to lock
/// before anchoring on our timeline.
const MASTER_TRIGGER_AFTER: Duration = Duration::from_millis(500);
/// logAnnounceInterval -2 and logSyncInterval -3, as Apple's receiver sends.
const MASTER_ANNOUNCE_INTERVAL: Duration = Duration::from_millis(250);
const MASTER_SYNC_INTERVAL: Duration = Duration::from_millis(125);
const MASTER_ANNOUNCE_LOG_INTERVAL: u8 = 0xfe;
const MASTER_SYNC_LOG_INTERVAL: u8 = 0xfd;
/// controlField values (IEEE 1588-2008 table 23): Sync 0, Follow_Up 2,
/// all others 5.
const CONTROL_SYNC: u8 = 0x00;
const CONTROL_FOLLOW_UP: u8 = 0x02;
const CONTROL_OTHER: u8 = 0x05;
/// unicastFlag | twoStepFlag | ptpTimescale (Apple's Sync).
const MASTER_SYNC_FLAGS: u16 = APPLE_UNICAST_FLAGS | PTP_TWO_STEP_FLAG;
/// Announce with the path-trace TLV naming this receiver (Apple's layout).
const PTP_MASTER_ANNOUNCE_BYTES: usize = 76;
/// Follow_Up with the 802.1AS information TLV and Apple's subtype-4 TLV.
const PTP_MASTER_FOLLOW_UP_BYTES: usize = 96;
/// Signaling with Apple's two organization-extension TLVs.
const PTP_MASTER_SIGNALING_BYTES: usize = PTP_SIGNALING_BYTES + APPLE_SIGNALING_TLVS.len();
const CURRENT_UTC_OFFSET: u16 = 37;
/// timeSource INTERNAL_OSCILLATOR, as Apple's receiver announces.
const TIME_SOURCE_INTERNAL_OSCILLATOR: u8 = 0xa0;
/// 802.1AS Follow_Up information TLV (ORGANIZATION_EXTENSION, OUI 00-80-C2,
/// subtype 1): cumulativeScaledRateOffset, gmTimeBaseIndicator,
/// lastGmPhaseChange and scaledLastGmFreqChange all zero, i.e. a grandmaster
/// whose time base never changed. Apple's receiver sends its own non-zero
/// lastGmPhaseChange/scaledLastGmFreqChange history there; they describe
/// that Mac's past grandmaster changes, not ours.
const FOLLOW_UP_INFORMATION_TLV: [u8; 32] = [
    0x00, 0x03, 0x00, 0x1c, 0x00, 0x80, 0xc2, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
/// Apple organization extension (OUI 00-0D-93) subtype 4 on Follow_Up:
/// followed by the sender's clockIdentity and two zero bytes.
const APPLE_FOLLOW_UP_TLV_HEAD: [u8; 10] = [0x00, 0x03, 0x00, 0x10, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x04];
/// Test hook variant V1: median of the trailing three bytes Apple's
/// receiver sent in both TLVs over 8 Signaling messages (0x34bbdf..0x396763;
/// sorted median 0x35def8). Probably a scaled link delay: as 2^-16 ns
/// (802.1AS UScaledNs) it is about 54 ns, a wired neighbour's delay; the
/// byte before it is always 0. A macOS sender as grandmaster sends flags 0
/// and zero here, so the value is not required by macOS.
const APPLE_SIGNALING_TRAILING_MEDIAN: [u8; 3] = [0x35, 0xde, 0xf8];
/// Test hook variant V3: Apple's receiver's lastGmPhaseChange (12 bytes) and
/// scaledLastGmFreqChange (4 bytes) from its Follow_Up 802.1AS TLV.
const APPLE_FOLLOW_UP_GM_CHANGE_FIELDS: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x06, 0x9c, 0xd4, 0xf1, 0xbd, 0x68, 0xb5, 0xfe, 0xc7, 0x2a, 0xf7,
];
const APPLE_LIKE_FOLLOW_UP_CORRECTION_NS: u64 = 80_000;
/// The two Apple organization-extension TLVs (subtypes 1 and 5) Apple's
/// receiver sends with every Announce while it is grandmaster
/// (`airplay-ipad-applercv-20261128T075756Z`). Meaning unknown; copied byte
/// for byte except the trailing three bytes of each, which vary over time
/// on the Mac (0x34bbdf..0x396763) and are zero on the iPad; zero here.
const APPLE_SIGNALING_TLVS: [u8; 62] = [
    // Subtype 1, 22 bytes.
    0x00, 0x03, 0x00, 0x16, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00,
    0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x00, 0x00,
    // Subtype 5, 32 bytes.
    0x00, 0x03, 0x00, 0x20, 0x00, 0x0d, 0x93, 0x00, 0x00, 0x05, 0x00, 0x1f, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10, 0x00, 0x00, 0x27, 0x10,
    0x00, 0x00, 0x00, 0x00,
];
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
        quality: ClockQuality,
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
    /// End-to-end delay request of a slave (macOS TimeSync as our slave
    /// sends one per Sync on 319 and does not lock without the answers).
    DelayReq { header: Header },
}

/// grandmasterClockQuality of an Announce (IEEE 1588-2008 5.3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockQuality {
    class: u8,
    accuracy: u8,
    variance: u16,
}

/// The grandmaster an Announce advertises, as the best master clock
/// algorithm compares it (IEEE 1588-2008 9.3.4, figure 28: priority1,
/// clockClass, clockAccuracy, offsetScaledLogVariance, priority2, identity;
/// lower wins). Only different grandmasters are ever compared here, so the
/// stepsRemoved/topology branch does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrandmasterDataset {
    priority1: u8,
    quality: ClockQuality,
    priority2: u8,
    identity: u64,
}

impl GrandmasterDataset {
    fn rank(self) -> (u8, u8, u8, u16, u8, u64) {
        (
            self.priority1,
            self.quality.class,
            self.quality.accuracy,
            self.quality.variance,
            self.priority2,
            self.identity,
        )
    }

    fn better_than(self, other: Self) -> bool {
        self.rank() < other.rank()
    }
}

/// The dataset this receiver announces as master of a sender that
/// announced `sender`: the sender's own priority1 and clock quality with
/// priority2 one lower, which wins on priority2 alone (Apple's receiver beat
/// the iPad the same way: 250/248/0x21/0x436a with priority2 237 against
/// 246). With priority2 already 0, priority1 one lower instead. `None` when
/// both are 0: nothing but a better clock quality could win, and claiming
/// one would misdescribe this receiver's clock.
fn master_dataset(sender: GrandmasterDataset, own_identity: u64) -> Option<GrandmasterDataset> {
    let (priority1, priority2) = match (sender.priority1, sender.priority2) {
        (priority1, priority2) if priority2 > 0 => (priority1, priority2 - 1),
        (priority1, priority2) if priority1 > 0 => (priority1 - 1, priority2),
        _ => return None,
    };
    let ours = GrandmasterDataset {
        priority1,
        quality: sender.quality,
        priority2,
        identity: own_identity,
    };
    ours.better_than(sender).then_some(ours)
}

/// This receiver's PTP time while it is a sender's master: the monotonic
/// clock the media path uses for host time ([`Instant`]) plus a fixed
/// offset, so the mapping from this timeline to host time is the identity
/// and never drifts. The offset is the TAI time (UTC + 37 s) when the
/// process first needed it, so the values look like PTP time; it is fixed
/// from then on and wall-clock changes do not affect it. Apple's receiver
/// uses an uptime-like timescale (about 2.6e6 s), so a sender does not need
/// real TAI.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnTimescale {
    epoch: Instant,
    base_ns: u64,
}

impl OwnTimescale {
    fn process() -> Self {
        static PROCESS: std::sync::OnceLock<OwnTimescale> = std::sync::OnceLock::new();
        *PROCESS.get_or_init(|| {
            let epoch = Instant::now();
            let unix_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(0));
            let tai_offset_ns = u64::from(CURRENT_UTC_OFFSET) * NANOS_PER_SECOND as u64;
            Self {
                epoch,
                base_ns: unix_ns.saturating_add(tai_offset_ns),
            }
        })
    }

    /// Test hook (variant V3): an uptime-like timescale, as Apple's
    /// receiver uses (about 2.6e6 s there): seconds since this host booted
    /// when the process first needed it, then the monotonic clock.
    fn uptime() -> Self {
        let epoch = Instant::now();
        #[cfg(windows)]
        let base_ns = {
            extern "system" {
                fn GetTickCount64() -> u64;
            }
            // SAFETY: GetTickCount64 has no preconditions.
            unsafe { GetTickCount64() }.saturating_mul(1_000_000)
        };
        #[cfg(not(windows))]
        let base_ns = 2_600_000 * NANOS_PER_SECOND as u64;
        Self { epoch, base_ns }
    }

    pub(crate) fn time_ns(self, at: Instant) -> u64 {
        let elapsed = u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
            .unwrap_or(u64::MAX);
        self.base_ns.saturating_add(elapsed)
    }
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
    /// Grandmaster named by the latest Announce.
    pub(crate) grandmaster: Option<u64>,
    pub(crate) announces: u64,
    pub(crate) sync_two_step: u64,
    pub(crate) sync_one_step: u64,
    pub(crate) follow_ups: u64,
    pub(crate) signaling: u64,
    /// Interval requests sent to it (shared schedule of its sessions).
    pub(crate) interval_requests: u64,
    /// Delay_Req messages from it (a slave of ours measuring path delay).
    pub(crate) delay_requests: u64,
    /// This receiver as the peer's PTP master, if it ever was.
    pub(crate) master: Option<MasterSummary>,
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
            "announce={} signaling={} sync_two_step={} sync_one_step={} follow_up={} interval_requests={}",
            self.announces,
            self.signaling,
            self.sync_two_step,
            self.sync_one_step,
            self.follow_ups,
            self.interval_requests
        )?;
        if self.delay_requests > 0 {
            write!(f, " delay_req={}", self.delay_requests)?;
        }
        if let Some(master) = self.master {
            write!(f, " master={master}")?;
        }
        Ok(())
    }
}

/// State of this receiver as one control peer's PTP master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MasterMode {
    /// It is the peer's master now.
    Active,
    /// It was, until the given reason; not re-entered while the peer has
    /// sessions.
    Yielded(&'static str),
    /// The peer's dataset cannot be outranked (see [`master_dataset`]).
    Refused,
}

/// Master mode of a session's control peer, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MasterSummary {
    pub(crate) mode: MasterMode,
    pub(crate) announces: u64,
    pub(crate) syncs: u64,
}

impl fmt::Display for MasterSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mode {
            MasterMode::Active => f.write_str("active")?,
            MasterMode::Yielded(reason) => write!(f, "yielded:{reason}")?,
            MasterMode::Refused => return f.write_str("refused"),
        }
        write!(f, "(announces={} syncs={})", self.announces, self.syncs)
    }
}

/// The sender's own clock as its latest Announce described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnnouncedClock {
    /// Where the Announce came from; the requests go back there.
    from: SocketAddr,
    clock: u64,
    grandmaster: u64,
}

/// A session's control peer's latest Announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LastAnnounce {
    clock: AnnouncedClock,
    dataset: GrandmasterDataset,
    at: Instant,
}

impl LastAnnounce {
    /// The announcing clock names itself, not this receiver, as grandmaster.
    fn names_itself(&self, own_identity: u64) -> bool {
        self.clock.clock == self.clock.grandmaster && self.clock.grandmaster != own_identity
    }
}

/// When a session's next interval request is due, and which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestSchedule {
    to: SocketAddr,
    due: Instant,
    /// Requests sent so far; the first one asks for the initial intervals.
    sent: u64,
}

#[derive(Debug)]
struct SenderClockTarget {
    id: u64,
    peer: IpAddr,
    status: SenderClockStatus,
    requests: Option<RequestSchedule>,
    last_announce: Option<LastAnnounce>,
}

/// One interval request due now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DueRequest {
    to: SocketAddr,
    intervals: [u8; 3],
}

/// This receiver acting as one control peer's PTP master, as Apple's own
/// receiver is for an iPad (see the module documentation).
///
/// Entered once per peer, by [`SenderClockRegistry::evaluate_masters`], when
/// one of the peer's sessions saw it announce itself as grandmaster (not
/// this receiver) at least [`MASTER_TRIGGER_AFTER`] before, its latest
/// Announce still does so and is fresh, and none of its sessions received a
/// Sync or Follow_Up from it. Sends go to that peer only, from the observer's
/// 319/320 sockets: Announce + Apple Signaling to its 320 every
/// [`MASTER_ANNOUNCE_INTERVAL`], two-step Sync to its 319 and Follow_Up to its
/// 320 every [`MASTER_SYNC_INTERVAL`], both starting at once.
///
/// Yields for good (until the peer's last session ends) when the peer sends
/// any Sync or Follow_Up, i.e. it is a clock source after all, or announces a
/// grandmaster that beats ours by BMCA (another master appeared). Re-entering
/// against a better silent grandmaster would only chase it. The interval
/// request schedule to the peer pauses while active: a master does not ask
/// its slave for Sync.
#[derive(Debug)]
struct MasterPort {
    peer: IpAddr,
    /// The peer's 319 (Sync) and 320 (the rest): the latest Announce's
    /// source address and its event port.
    event_to: SocketAddr,
    general_to: SocketAddr,
    /// Our dataset; `None` when refused.
    ours: Option<GrandmasterDataset>,
    mode: MasterMode,
    entered_at: Instant,
    next_announce: Instant,
    next_sync: Instant,
    announce_sequence: u16,
    sync_sequence: u16,
    announces: u64,
    syncs: u64,
}

impl MasterPort {
    fn summary(&self) -> MasterSummary {
        MasterSummary {
            mode: self.mode,
            announces: self.announces,
            syncs: self.syncs,
        }
    }
}

/// A master-mode transition, for the observer to log and count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterChange {
    Entered {
        peer: IpAddr,
        sender: GrandmasterDataset,
        ours: GrandmasterDataset,
        silent_for: Duration,
    },
    Refused {
        peer: IpAddr,
        sender: GrandmasterDataset,
    },
    Yielded {
        peer: IpAddr,
        reason: &'static str,
        after: Duration,
        summary: MasterSummary,
    },
}

/// One master-mode message due now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterSend {
    /// Announce, then the Apple Signaling.
    Announce {
        peer: IpAddr,
        to: SocketAddr,
        dataset: GrandmasterDataset,
        sequence: u16,
    },
    /// Sync, then its Follow_Up.
    Sync {
        peer: IpAddr,
        event_to: SocketAddr,
        general_to: SocketAddr,
        sequence: u16,
    },
}

/// The next due time of a periodic message sent at `due`: one interval
/// later, or one interval from `now` after a stall (no burst to catch up).
fn next_period(due: Instant, interval: Duration, now: Instant) -> Instant {
    let next = due + interval;
    if next <= now {
        now + interval
    } else {
        next
    }
}

#[derive(Debug, Default)]
struct RegistryState {
    targets: Vec<SenderClockTarget>,
    masters: Vec<MasterPort>,
}

impl RegistryState {
    fn master_active(&self, peer: IpAddr) -> bool {
        self.masters
            .iter()
            .any(|master| master.peer == peer && master.mode == MasterMode::Active)
    }
}

/// Sender clocks of the live portable sessions, shared by the sessions
/// (register on SETUP, deregister on drop) and the observer task that counts
/// their datagrams, asks a self-announcing one for Sync and, if it stays
/// silent, becomes its master ([`MasterPort`]).
///
/// Request rule, copied from what the iPad does as a slave of macOS's
/// AirPlay receiver (`airplay-ipad-applercv-20261128T075756Z`): once the
/// session's control peer has announced itself as grandmaster, send it one
/// message interval request with "initial" intervals, a second one asking for
/// 8 Sync a second [`INTERVAL_REQUEST_FOLLOW_DELAY`] later, and that one again
/// every [`INTERVAL_REQUEST_PERIOD`] for as long as the session lives, Sync
/// or not (the real slave keeps sending), except while this receiver is the
/// peer's master. Unicast to the Announce's source address. A later Announce
/// for another grandmaster (a sender relaying a grouped HomePod) does not
/// stop the schedule; the master port is still the sender's. Sessions
/// sharing a control peer share one schedule (the oldest registration's).
#[derive(Debug, Default)]
pub(crate) struct SenderClockRegistry {
    state: Mutex<RegistryState>,
    next_id: AtomicU64,
    /// This receiver's clockIdentity; 0 where it never sends (macOS, tests).
    own_identity: u64,
    /// Test hook only ([`force_master_test_hook`]): enter master mode even
    /// toward a sender that sends Sync, and do not yield to its Sync.
    force_master: bool,
    /// Test hook only ([`master_variant_test_hook`]); all off by default.
    variant: MasterVariant,
}

impl SenderClockRegistry {
    pub(crate) fn new(own_identity: u64) -> Self {
        Self {
            own_identity,
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, peer: IpAddr) -> SenderClockWatch {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().targets.push(SenderClockTarget {
            id,
            peer,
            status: SenderClockStatus::default(),
            requests: None,
            last_announce: None,
        });
        SenderClockWatch {
            registry: Arc::clone(self),
            id,
        }
    }

    fn status(&self, id: u64) -> Option<SenderClockStatus> {
        let state = self.lock();
        let target = state.targets.iter().find(|target| target.id == id)?;
        let mut status = target.status;
        status.master = state
            .masters
            .iter()
            .find(|master| master.peer == target.peer)
            .map(MasterPort::summary);
        Some(status)
    }

    /// Apply `update` to every live session whose control peer is `peer`.
    fn observe(&self, peer: IpAddr, mut update: impl FnMut(&mut SenderClockTarget)) {
        let mut state = self.lock();
        for target in state.targets.iter_mut().filter(|target| target.peer == peer) {
            update(target);
        }
    }

    /// Start the request schedule of every session of `peer` that has none,
    /// when `clock` announces itself as grandmaster (and is not this
    /// receiver). Returns whether a schedule started.
    fn start_requests(&self, peer: IpAddr, clock: AnnouncedClock, own_identity: u64, now: Instant) -> bool {
        if clock.clock != clock.grandmaster || clock.grandmaster == own_identity {
            return false;
        }
        let mut started = false;
        let mut state = self.lock();
        for target in state
            .targets
            .iter_mut()
            .filter(|target| target.peer == peer && target.requests.is_none())
        {
            target.requests = Some(RequestSchedule {
                to: clock.from,
                due: now,
                sent: 0,
            });
            started = true;
        }
        started
    }

    /// The earliest due time of any request schedule (paused ones aside) or
    /// active master's next message.
    fn next_due(&self) -> Option<Instant> {
        let state = self.lock();
        let requests = state
            .targets
            .iter()
            .filter(|target| !state.master_active(target.peer))
            .filter_map(|target| target.requests.map(|schedule| schedule.due));
        let masters = state
            .masters
            .iter()
            .filter(|master| master.mode == MasterMode::Active)
            .map(|master| master.next_announce.min(master.next_sync));
        requests.chain(masters).min()
    }

    /// Take every request due at `now` and advance its schedule; one per
    /// control peer (the oldest session's schedule serves the peer). Paused
    /// while this receiver is the peer's master.
    fn take_due(&self, now: Instant) -> Vec<DueRequest> {
        let mut guard = self.lock();
        let state = &mut *guard;
        let mut served: Vec<IpAddr> = Vec::new();
        let mut sent: Vec<(IpAddr, u64)> = Vec::new();
        let mut due = Vec::new();
        let paused: Vec<IpAddr> = state
            .masters
            .iter()
            .filter(|master| master.mode == MasterMode::Active)
            .map(|master| master.peer)
            .collect();
        for target in state.targets.iter_mut() {
            let Some(schedule) = target.requests.as_mut() else {
                continue;
            };
            if served.contains(&target.peer) || paused.contains(&target.peer) {
                continue;
            }
            served.push(target.peer);
            if schedule.due > now {
                continue;
            }
            due.push(DueRequest {
                to: schedule.to,
                intervals: if schedule.sent == 0 {
                    INTERVAL_REQUEST_INITIAL
                } else {
                    INTERVAL_REQUEST_SYNC_8HZ
                },
            });
            schedule.due = now
                + if schedule.sent == 0 {
                    INTERVAL_REQUEST_FOLLOW_DELAY
                } else {
                    INTERVAL_REQUEST_PERIOD
                };
            schedule.sent += 1;
            sent.push((target.peer, schedule.sent));
        }
        for (peer, count) in sent {
            for target in state.targets.iter_mut().filter(|target| target.peer == peer) {
                target.status.interval_requests = count;
            }
        }
        due
    }

    /// Enter master mode for every control peer that qualifies now (see
    /// [`MasterPort`]).
    fn evaluate_masters(&self, own_identity: u64, now: Instant) -> Vec<MasterChange> {
        let mut guard = self.lock();
        let state = &mut *guard;
        let mut changes = Vec::new();
        let mut peers: Vec<IpAddr> = Vec::new();
        for target in &state.targets {
            if !peers.contains(&target.peer)
                && !state.masters.iter().any(|master| master.peer == target.peer)
            {
                peers.push(target.peer);
            }
        }
        for peer in peers {
            let mut sessions = state.targets.iter().filter(|target| target.peer == peer);
            if !self.force_master && sessions.clone().any(|target| target.status.timing_messages() > 0) {
                continue;
            }
            let Some((announce, first)) = sessions.find_map(|target| {
                let announce = target.last_announce?;
                let first = target.status.first_announce_at?;
                (announce.names_itself(own_identity)
                    && now.saturating_duration_since(announce.at) <= ANNOUNCE_LIFETIME
                    && now.saturating_duration_since(first) >= MASTER_TRIGGER_AFTER)
                    .then_some((announce, first))
            }) else {
                continue;
            };
            let sender = announce.dataset;
            let ours = master_dataset(sender, own_identity);
            state.masters.push(MasterPort {
                peer,
                event_to: SocketAddr::new(announce.clock.from.ip(), PTP_EVENT_PORT),
                general_to: announce.clock.from,
                ours,
                mode: if ours.is_some() {
                    MasterMode::Active
                } else {
                    MasterMode::Refused
                },
                entered_at: now,
                next_announce: now,
                next_sync: now,
                // Apple's receiver numbers both from 1.
                announce_sequence: 1,
                sync_sequence: 1,
                announces: 0,
                syncs: 0,
            });
            changes.push(match ours {
                Some(ours) => MasterChange::Entered {
                    peer,
                    sender,
                    ours,
                    silent_for: now.saturating_duration_since(first),
                },
                None => MasterChange::Refused { peer, sender },
            });
        }
        changes
    }

    /// Leave master mode for `peer` for good, if active.
    fn yield_master(&self, peer: IpAddr, reason: &'static str, now: Instant) -> Option<MasterChange> {
        let mut state = self.lock();
        let master = state
            .masters
            .iter_mut()
            .find(|master| master.peer == peer && master.mode == MasterMode::Active)?;
        master.mode = MasterMode::Yielded(reason);
        Some(MasterChange::Yielded {
            peer,
            reason,
            after: now.saturating_duration_since(master.entered_at),
            summary: master.summary(),
        })
    }

    /// An Announce from `peer` naming `dataset`'s grandmaster: yield when
    /// that grandmaster beats ours.
    fn master_sees_announce(
        &self,
        peer: IpAddr,
        dataset: GrandmasterDataset,
        now: Instant,
    ) -> Option<MasterChange> {
        let beaten = self.lock().masters.iter().any(|master| {
            master.peer == peer
                && master.mode == MasterMode::Active
                && master.ours.is_some_and(|ours| dataset.better_than(ours))
        });
        if beaten {
            self.yield_master(peer, "better_grandmaster", now)
        } else {
            None
        }
    }

    /// Where an active master for `peer` sends general messages.
    fn master_general_to(&self, peer: IpAddr) -> Option<SocketAddr> {
        self.lock()
            .masters
            .iter()
            .find(|master| master.peer == peer && master.mode == MasterMode::Active)
            .map(|master| master.general_to)
    }

    /// Every master message due at `now`, advancing the schedules.
    fn take_master_due(&self, now: Instant) -> Vec<MasterSend> {
        let mut state = self.lock();
        let mut due = Vec::new();
        for master in state
            .masters
            .iter_mut()
            .filter(|master| master.mode == MasterMode::Active)
        {
            let Some(dataset) = master.ours else {
                continue;
            };
            if master.next_announce <= now {
                due.push(MasterSend::Announce {
                    peer: master.peer,
                    to: master.general_to,
                    dataset,
                    sequence: master.announce_sequence,
                });
                master.announce_sequence = master.announce_sequence.wrapping_add(1);
                master.announces += 1;
                master.next_announce = next_period(master.next_announce, MASTER_ANNOUNCE_INTERVAL, now);
            }
            if master.next_sync <= now {
                due.push(MasterSend::Sync {
                    peer: master.peer,
                    event_to: master.event_to,
                    general_to: master.general_to,
                    sequence: master.sync_sequence,
                });
                master.sync_sequence = master.sync_sequence.wrapping_add(1);
                master.syncs += 1;
                master.next_sync = next_period(master.next_sync, MASTER_SYNC_INTERVAL, now);
            }
        }
        due
    }

    /// Send `peer`'s master messages to these addresses instead (tests bind
    /// ephemeral ports, not 319/320).
    #[cfg(test)]
    fn test_redirect_master(&self, peer: IpAddr, event_to: SocketAddr, general_to: SocketAddr) {
        for master in self.lock().masters.iter_mut().filter(|master| master.peer == peer) {
            master.event_to = event_to;
            master.general_to = general_to;
        }
    }

    /// Record an Announce from `peer` naming `grandmaster`, as the observer
    /// does for a received datagram.
    #[cfg(test)]
    pub(crate) fn test_note_announce(&self, peer: IpAddr, grandmaster: u64, at: Instant) {
        self.observe(peer, |target| {
            target.status.announces += 1;
            target.status.first_announce_at.get_or_insert(at);
            target.status.grandmaster = Some(grandmaster);
        });
    }

    fn deregister(&self, id: u64) {
        let mut guard = self.lock();
        let state = &mut *guard;
        state.targets.retain(|target| target.id != id);
        let targets = &state.targets;
        state.masters.retain(|master| {
            let keep = targets.iter().any(|target| target.peer == master.peer);
            if !keep && master.mode == MasterMode::Active {
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master mode for {} ended with its last session after {} ms ({})",
                    master.peer,
                    master.entered_at.elapsed().as_millis(),
                    master.summary()
                );
            }
            keep
        });
    }
}

/// A session's registration in the [`SenderClockRegistry`]; dropping it
/// (the session ending) stops its interval requests, and master mode for its
/// control peer once no other session of that peer is left.
#[derive(Debug)]
pub(crate) struct SenderClockWatch {
    registry: Arc<SenderClockRegistry>,
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

/// One message interval request, byte for byte as the iPad sends it as a
/// slave (2026-11-28), with this receiver's sourcePortIdentity and sequenceId;
/// targetPortIdentity stays all zero as in the capture.
fn interval_request(own_identity: u64, sequence: u16, intervals: [u8; 3]) -> [u8; PTP_INTERVAL_REQUEST_BYTES] {
    let mut packet = [0u8; PTP_INTERVAL_REQUEST_BYTES];
    packet[0] = (PTP_MAJOR_SDO_ID << 4) | MESSAGE_SIGNALING;
    packet[1] = PTP_VERSION;
    packet[2..4].copy_from_slice(&(PTP_INTERVAL_REQUEST_BYTES as u16).to_be_bytes());
    packet[4] = PTP_DOMAIN;
    packet[6..8].copy_from_slice(&APPLE_UNICAST_FLAGS.to_be_bytes());
    packet[20..28].copy_from_slice(&own_identity.to_be_bytes());
    packet[28..30].copy_from_slice(&OWN_PTP_PORT.to_be_bytes());
    packet[30..32].copy_from_slice(&sequence.to_be_bytes());
    packet[32] = INTERVAL_REQUEST_CONTROL_FIELD;
    packet[33] = INTERVAL_REQUEST_LOG_INTERVAL;
    // 34..44 targetPortIdentity stays zero.
    packet[44..54].copy_from_slice(&INTERVAL_REQUEST_TLV);
    packet[54..57].copy_from_slice(&intervals);
    packet[57] = INTERVAL_REQUEST_TLV_FLAGS;
    // 58..60 reserved.
    packet
}

/// The PTP header of a message this receiver sends as master: version 2,
/// domain 0, correctionField 0, sourcePortIdentity `identity`:[`OWN_PTP_PORT`].
fn write_master_header(
    packet: &mut [u8],
    message_type: u8,
    flags: u16,
    identity: u64,
    sequence: u16,
    (control, log_interval): (u8, u8),
) {
    let length = u16::try_from(packet.len()).expect("PTP message length fits 16 bits");
    packet[0] = (PTP_MAJOR_SDO_ID << 4) | message_type;
    packet[1] = PTP_VERSION;
    packet[2..4].copy_from_slice(&length.to_be_bytes());
    packet[4] = PTP_DOMAIN;
    packet[6..8].copy_from_slice(&flags.to_be_bytes());
    // 8..16 correctionField and 16..20 messageTypeSpecific stay zero.
    packet[20..28].copy_from_slice(&identity.to_be_bytes());
    packet[28..30].copy_from_slice(&OWN_PTP_PORT.to_be_bytes());
    packet[30..32].copy_from_slice(&sequence.to_be_bytes());
    packet[32] = control;
    packet[33] = log_interval;
}

/// A PTP Timestamp (48-bit seconds, 32-bit nanoseconds) of `ns`.
fn write_timestamp(bytes: &mut [u8], ns: u64) {
    let per_second = NANOS_PER_SECOND as u64;
    let (seconds, nanos) = (ns / per_second, (ns % per_second) as u32);
    bytes[0..2].copy_from_slice(&((seconds >> 32) as u16).to_be_bytes());
    bytes[2..6].copy_from_slice(&(seconds as u32).to_be_bytes());
    bytes[6..10].copy_from_slice(&nanos.to_be_bytes());
}

/// Master Announce, byte for byte as Apple's receiver sends it
/// (`airplay-ipad-applercv-20261128T075756Z`) but for `dataset`: zero
/// originTimestamp, currentUtcOffset 37, stepsRemoved 0, timeSource
/// internal oscillator and a PATH_TRACE TLV naming this clock.
fn master_announce(dataset: GrandmasterDataset, sequence: u16) -> [u8; PTP_MASTER_ANNOUNCE_BYTES] {
    let mut packet = [0u8; PTP_MASTER_ANNOUNCE_BYTES];
    write_master_header(
        &mut packet,
        MESSAGE_ANNOUNCE,
        APPLE_UNICAST_FLAGS,
        dataset.identity,
        sequence,
        (CONTROL_OTHER, MASTER_ANNOUNCE_LOG_INTERVAL),
    );
    packet[44..46].copy_from_slice(&CURRENT_UTC_OFFSET.to_be_bytes());
    packet[47] = dataset.priority1;
    packet[48] = dataset.quality.class;
    packet[49] = dataset.quality.accuracy;
    packet[50..52].copy_from_slice(&dataset.quality.variance.to_be_bytes());
    packet[52] = dataset.priority2;
    packet[53..61].copy_from_slice(&dataset.identity.to_be_bytes());
    packet[63] = TIME_SOURCE_INTERNAL_OSCILLATOR;
    packet[64..68].copy_from_slice(&[0x00, 0x08, 0x00, 0x08]);
    packet[68..76].copy_from_slice(&dataset.identity.to_be_bytes());
    packet
}

/// Master Signaling with Apple's two organization-extension TLVs, sent
/// after every Announce as Apple's receiver does; targetPortIdentity zero.
fn master_signaling(identity: u64, sequence: u16) -> [u8; PTP_MASTER_SIGNALING_BYTES] {
    let mut packet = [0u8; PTP_MASTER_SIGNALING_BYTES];
    write_master_header(
        &mut packet,
        MESSAGE_SIGNALING,
        APPLE_UNICAST_FLAGS,
        identity,
        sequence,
        (CONTROL_OTHER, INTERVAL_REQUEST_LOG_INTERVAL),
    );
    packet[PTP_SIGNALING_BYTES..].copy_from_slice(&APPLE_SIGNALING_TLVS);
    packet
}

/// Delay_Resp to a slave's Delay_Req (IEEE 1588-2008 11.3, 13.8): its
/// sequenceId and correctionField, our receive time of the request
/// (software, on [`OwnTimescale`]) and its sourcePortIdentity as
/// requestingPortIdentity; logMessageInterval -3 (logMinDelayReqInterval:
/// one request per Sync is fine).
fn delay_resp(identity: u64, request: Header, receive_ns: u64) -> [u8; PTP_DELAY_RESP_BYTES] {
    let mut packet = [0u8; PTP_DELAY_RESP_BYTES];
    write_master_header(
        &mut packet,
        MESSAGE_DELAY_RESP,
        APPLE_UNICAST_FLAGS,
        identity,
        request.sequence,
        (CONTROL_DELAY_RESP, MASTER_SYNC_LOG_INTERVAL),
    );
    packet[8..16].copy_from_slice(&request.correction_scaled_ns.to_be_bytes());
    write_timestamp(&mut packet[PTP_HEADER_BYTES..PTP_SYNC_BYTES], receive_ns);
    packet[44..52].copy_from_slice(&request.source.clock.to_be_bytes());
    packet[52..54].copy_from_slice(&request.source.port.to_be_bytes());
    packet
}

/// [`master_signaling`] with `trailing` as the last three bytes of both
/// Apple TLVs (test hook variant V1).
fn master_signaling_with(identity: u64, sequence: u16, trailing: [u8; 3]) -> [u8; PTP_MASTER_SIGNALING_BYTES] {
    let mut packet = master_signaling(identity, sequence);
    for end in [PTP_SIGNALING_BYTES + 26, PTP_MASTER_SIGNALING_BYTES] {
        packet[end - 3..end].copy_from_slice(&trailing);
    }
    packet
}

/// Master Follow_Up shaped like Apple's receiver's (test hook variant V3):
/// a non-zero correctionField (Apple's 64-123 us egress corrections; 80 us
/// here) with the origin moved back by the same amount, so origin +
/// correction is still `corrected_ns`, and Apple's lastGmPhaseChange and
/// scaledLastGmFreqChange bytes.
fn master_follow_up_apple_like(identity: u64, sequence: u16, corrected_ns: u64) -> [u8; PTP_MASTER_FOLLOW_UP_BYTES] {
    let correction_ns = APPLE_LIKE_FOLLOW_UP_CORRECTION_NS.min(corrected_ns);
    let mut packet = master_follow_up(identity, sequence, corrected_ns - correction_ns);
    packet[8..16].copy_from_slice(&((correction_ns as i64) << 16).to_be_bytes());
    packet[60..76].copy_from_slice(&APPLE_FOLLOW_UP_GM_CHANGE_FIELDS);
    packet
}

/// Master two-step Sync: zero originTimestamp, flags 0x0608.
fn master_sync(identity: u64, sequence: u16) -> [u8; PTP_SYNC_BYTES] {
    let mut packet = [0u8; PTP_SYNC_BYTES];
    write_master_header(
        &mut packet,
        MESSAGE_SYNC,
        MASTER_SYNC_FLAGS,
        identity,
        sequence,
        (CONTROL_SYNC, MASTER_SYNC_LOG_INTERVAL),
    );
    packet
}

/// Master Follow_Up for the Sync with `sequence`: preciseOriginTimestamp
/// `origin_ns` on this receiver's timescale, correctionField 0, the 802.1AS
/// information TLV and Apple's subtype-4 TLV naming this clock.
fn master_follow_up(identity: u64, sequence: u16, origin_ns: u64) -> [u8; PTP_MASTER_FOLLOW_UP_BYTES] {
    let mut packet = [0u8; PTP_MASTER_FOLLOW_UP_BYTES];
    write_master_header(
        &mut packet,
        MESSAGE_FOLLOW_UP,
        APPLE_UNICAST_FLAGS,
        identity,
        sequence,
        (CONTROL_FOLLOW_UP, MASTER_SYNC_LOG_INTERVAL),
    );
    write_timestamp(&mut packet[PTP_HEADER_BYTES..PTP_SYNC_BYTES], origin_ns);
    packet[44..76].copy_from_slice(&FOLLOW_UP_INFORMATION_TLV);
    packet[76..86].copy_from_slice(&APPLE_FOLLOW_UP_TLV_HEAD);
    packet[86..94].copy_from_slice(&identity.to_be_bytes());
    packet
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
    clocks: Arc<SenderClockRegistry>,
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
    fn portable(samples: broadcast::Sender<PtpClockSample>, clocks: Arc<SenderClockRegistry>) -> Self {
        Self {
            samples,
            clocks,
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

    /// The clockIdentity this receiver uses as a sender's PTP master (its
    /// samples carry it as grandmaster); `None` where it never is (macOS).
    pub(crate) fn master_identity(&self) -> Option<u64> {
        let identity = self.clocks.own_identity;
        (identity != 0).then_some(identity)
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
            self.clocks.register(peer_ip),
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
            clocks: Arc::clone(&self.clocks),
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
        {
            // Test hook (variant V2): advertise this receiver's PTP port in
            // SETUP timingPeerInfo like the macOS platform backend does.
            (self.clocks.variant.clock_id && self.clocks.own_identity != 0)
                .then_some((self.clocks.own_identity, OWN_PTP_PORT))
        }
    }

    #[cfg(test)]
    pub(crate) fn test_source() -> Self {
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        Self::portable(samples, Arc::default())
    }

    /// Portable session source registered with `clocks` like `for_session`
    /// registers with its observer's registry.
    #[cfg(test)]
    pub(crate) fn test_watched_session_source(control_peer: IpAddr, clocks: &Arc<SenderClockRegistry>) -> Self {
        let mut source = Self::test_source();
        source.clocks = Arc::clone(clocks);
        source.session = Some(Arc::new(SessionShared::with_clock_watch(
            control_peer,
            clocks.register(control_peer),
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
    clocks: Arc<SenderClockRegistry>,
    /// This receiver's clockIdentity (interval requests and master mode; an
    /// Announce naming it as grandmaster is ignored).
    own_identity: u64,
    /// Time of this receiver's Syncs in master mode.
    timescale: OwnTimescale,
    /// sequenceId of every Signaling this receiver sends (interval requests
    /// and master Signaling share one port, so one sequence space).
    request_sequence: u16,
    /// A refused request is logged once per observer, then at debug.
    request_failure_logged: bool,
    /// A refused master message is logged once per master entry.
    master_failure_logged: bool,
    /// Delay_Resp messages built on receipt, sent right after (peer, to,
    /// message).
    delay_responses: Vec<(IpAddr, SocketAddr, [u8; PTP_DELAY_RESP_BYTES])>,
}

impl PtpObserver {
    fn new(
        event_socket: Option<UdpSocket>,
        general_socket: Option<UdpSocket>,
        samples: broadcast::Sender<PtpClockSample>,
        clocks: Arc<SenderClockRegistry>,
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
            clocks,
            own_identity,
            timescale: OwnTimescale::process(),
            request_sequence: 0,
            request_failure_logged: false,
            master_failure_logged: false,
            delay_responses: Vec::new(),
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
        let mut registry = SenderClockRegistry::new(own_identity);
        registry.force_master = force_master_test_hook();
        registry.variant = master_variant_test_hook();
        if registry.variant != MasterVariant::default() {
            log::warn!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP TEST HOOK active (AUDIOHUB_TEST_AIRPLAY_PTP_MASTER_VARIANT): {:?}",
                registry.variant
            );
        }
        let outcome = [("event", &event_socket), ("general", &general_socket)]
            .map(|(name, socket)| format!("{name}:{}", apple_ip_header(socket)))
            .join(" ");
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP socket options (Apple's PTP IP header: TTL 255, DF, UDP checksum 0): {outcome}"
        );
        if registry.force_master {
            log::warn!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP TEST HOOK active (AUDIOHUB_TEST_AIRPLAY_PTP_FORCE_MASTER=1): master mode is forced toward every self-announcing sender, Sync or not"
            );
        }
        let uptime_timescale = registry.variant.apple_follow_up;
        let clocks = Arc::new(registry);
        let source = PtpClockSource::portable(samples.clone(), Arc::clone(&clocks));
        let mut observer = Self::new(
            Some(event_socket),
            Some(general_socket),
            samples,
            clocks,
            own_identity,
        );
        if uptime_timescale {
            observer.timescale = OwnTimescale::uptime();
        }
        Ok((observer, source))
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
        // for this process to count or request.
        let clocks = Arc::new(SenderClockRegistry::default());
        let mut source = PtpClockSource::portable(samples.clone(), Arc::clone(&clocks));
        source.backend = PtpBackendKind::MacSystem;
        source.mac_root = Some(Arc::new(MacRoot {
            clock,
            gate: Arc::new(Semaphore::new(1)),
        }));
        Ok((Self::new(None, None, samples, clocks, 0), source))
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
            let next_request = self.clocks.next_due();
            tokio::select! {
                received = event_socket.recv_from(&mut event) => {
                    if let Some((length, peer)) = tolerate_reset(received)? {
                        self.receive(&event[..length], peer, PTP_EVENT_PORT, Instant::now());
                        self.send_delay_responses(&general_socket).await;
                    }
                }
                received = general_socket.recv_from(&mut general) => {
                    if let Some((length, peer)) = tolerate_reset(received)? {
                        self.receive(&general[..length], peer, PTP_GENERAL_PORT, Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(next_request.unwrap_or_else(Instant::now).into()), if next_request.is_some() => {
                    let now = Instant::now();
                    self.send_due_requests(&general_socket, now).await;
                    self.send_master_due(&event_socket, &general_socket, now).await;
                }
                _ = prune_tick.tick() => {
                    let now = Instant::now();
                    self.prune(now);
                    self.evaluate_masters(now);
                }
            }
        }
    }

    /// Send every interval request that is due. The send waits for the
    /// socket to be writable (a `try_send_to` on a socket that has only been
    /// read can report `WouldBlock`), bounded so a stuck socket cannot stall
    /// the receive path.
    async fn send_due_requests(&mut self, socket: &UdpSocket, now: Instant) {
        for request in self.clocks.take_due(now) {
            let packet = interval_request(self.own_identity, self.request_sequence, request.intervals);
            self.request_sequence = self.request_sequence.wrapping_add(1);
            let result = send_datagram(socket, &packet, request.to).await;
            let sent = match result {
                Ok(_) => true,
                Err(error) => {
                    if self.request_failure_logged {
                        log::debug!(
                            target: "audiohub_airplay::airplay2",
                            "AirPlay 2 PTP interval request to {} not sent: {error}",
                            request.to
                        );
                    } else {
                        self.request_failure_logged = true;
                        log::warn!(
                            target: "audiohub_airplay::airplay2",
                            "AirPlay 2 PTP interval request to {} not sent: {error}",
                            request.to
                        );
                    }
                    false
                }
            };
            crate::telemetry::record_ptp_interval_request(
                crate::telemetry::operation_token(),
                request.to.ip(),
                sent,
            );
        }
    }

    /// Send every master-mode message that is due (see [`MasterPort`]).
    /// The Follow_Up's preciseOriginTimestamp is the software time halfway
    /// through the Sync's send call, and that same instant and time become a
    /// clock sample of this receiver's own timeline for the peer's sessions:
    /// the anchor the peer sends on our timeline then maps exactly.
    async fn send_master_due(&mut self, event_socket: &UdpSocket, general_socket: &UdpSocket, now: Instant) {
        for send in self.clocks.take_master_due(now) {
            match send {
                MasterSend::Announce {
                    peer,
                    to,
                    dataset,
                    sequence,
                } => {
                    let mut result = send_datagram(general_socket, &master_announce(dataset, sequence), to).await;
                    if result.is_ok() {
                        let signaling = master_signaling_with(
                            self.own_identity,
                            self.request_sequence,
                            if self.clocks.variant.apple_tlv_value {
                                APPLE_SIGNALING_TRAILING_MEDIAN
                            } else {
                                [0; 3]
                            },
                        );
                        self.request_sequence = self.request_sequence.wrapping_add(1);
                        result = send_datagram(general_socket, &signaling, to).await;
                    }
                    self.master_sent(peer, crate::telemetry::PtpMasterMessage::Announce, result);
                }
                MasterSend::Sync {
                    peer,
                    event_to,
                    general_to,
                    sequence,
                } => {
                    let before = Instant::now();
                    let result = send_datagram(event_socket, &master_sync(self.own_identity, sequence), event_to).await;
                    let sent_at = before + before.elapsed() / 2;
                    let origin_ns = self.timescale.time_ns(sent_at);
                    let result = match result {
                        Ok(()) => {
                            send_datagram(
                                general_socket,
                                &if self.clocks.variant.apple_follow_up {
                                    master_follow_up_apple_like(self.own_identity, sequence, origin_ns)
                                } else {
                                    master_follow_up(self.own_identity, sequence, origin_ns)
                                },
                                general_to,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    if result.is_ok() {
                        self.emit_own_sample(peer, origin_ns, sent_at);
                    }
                    self.master_sent(peer, crate::telemetry::PtpMasterMessage::Sync, result);
                }
            }
        }
    }

    /// Publish one of this receiver's own Syncs as a sample of its own
    /// timeline for `peer`'s sessions: remote time `origin_ns` at local
    /// `sent_at`, so their mapper's offset is exact and constant.
    fn emit_own_sample(&self, peer: IpAddr, origin_ns: u64, sent_at: Instant) {
        let telemetry_token = crate::telemetry::operation_token();
        let delivered = self
            .samples
            .send(PtpClockSample {
                peer,
                grandmaster: self.own_identity,
                remote_time_ns: origin_ns,
                received_at: sent_at,
                telemetry_token,
            })
            .is_ok();
        crate::telemetry::record_ptp_sample_emitted(telemetry_token, delivered);
    }

    /// Send the Delay_Resp messages queued by `receive`.
    async fn send_delay_responses(&mut self, general_socket: &UdpSocket) {
        for (peer, to, packet) in std::mem::take(&mut self.delay_responses) {
            let result = send_datagram(general_socket, &packet, to).await;
            self.master_sent(peer, crate::telemetry::PtpMasterMessage::DelayResp, result);
        }
    }

    fn master_sent(&mut self, peer: IpAddr, message: crate::telemetry::PtpMasterMessage, result: io::Result<()>) {
        if let Err(error) = &result {
            if self.master_failure_logged {
                log::debug!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master {message:?} to {peer} not sent: {error}"
                );
            } else {
                self.master_failure_logged = true;
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master {message:?} to {peer} not sent: {error}"
                );
            }
        }
        crate::telemetry::record_ptp_master_send(
            crate::telemetry::operation_token(),
            peer,
            message,
            result.is_ok(),
        );
    }

    /// Enter master mode where a session's control peer qualifies.
    fn evaluate_masters(&mut self, now: Instant) {
        for change in self.clocks.evaluate_masters(self.own_identity, now) {
            self.report_master(change);
        }
    }

    fn report_master(&mut self, change: MasterChange) {
        let token = crate::telemetry::operation_token();
        match change {
            MasterChange::Entered {
                peer,
                sender,
                ours,
                silent_for,
            } => {
                self.master_failure_logged = false;
                crate::telemetry::record_ptp_master_mode(
                    token,
                    peer,
                    crate::telemetry::PtpMasterTransition::Entered,
                );
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master mode entered for {peer}: it announces itself as grandmaster {:016x} (priority1 {} clockClass {} accuracy 0x{:02x} variance 0x{:04x} priority2 {}) but sent no Sync/Follow_Up in {} ms; announcing {:016x}:{OWN_PTP_PORT:04x} with priority1 {} priority2 {}, Announce 4/s and two-step Sync 8/s unicast to {peer}:319/320",
                    sender.identity,
                    sender.priority1,
                    sender.quality.class,
                    sender.quality.accuracy,
                    sender.quality.variance,
                    sender.priority2,
                    silent_for.as_millis(),
                    ours.identity,
                    ours.priority1,
                    ours.priority2
                );
            }
            MasterChange::Refused { peer, sender } => {
                crate::telemetry::record_ptp_master_mode(
                    token,
                    peer,
                    crate::telemetry::PtpMasterTransition::Refused,
                );
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master mode refused for {peer}: its grandmaster {:016x} announces priority1 {} priority2 {}, which no honest dataset of this receiver outranks; staying a slave",
                    sender.identity,
                    sender.priority1,
                    sender.priority2
                );
            }
            MasterChange::Yielded {
                peer,
                reason,
                after,
                summary,
            } => {
                crate::telemetry::record_ptp_master_mode(
                    token,
                    peer,
                    crate::telemetry::PtpMasterTransition::Yielded,
                );
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP master mode yielded for {peer} after {} ms (reason={reason}, {summary}); following the sender's clock again",
                    after.as_millis()
                );
            }
        }
    }

    /// After an Announce from `peer`: start asking it for Sync if it
    /// announces itself as grandmaster to a session (see
    /// [`SenderClockRegistry`]).
    fn maybe_request_sync(
        &self,
        peer: SocketAddr,
        clock: u64,
        grandmaster: u64,
        (priority1, priority2): (u8, u8),
        now: Instant,
    ) {
        let announced = AnnouncedClock {
            from: peer,
            clock,
            grandmaster,
        };
        if self.clocks.start_requests(peer.ip(), announced, self.own_identity, now) {
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP sender {} clock {clock:016x} announces itself as grandmaster (priority1 {priority1} priority2 {priority2}); sending 802.1AS message interval requests (8 Sync/s) to {peer} as port {:016x}:{OWN_PTP_PORT:04x}",
                peer.ip(),
                self.own_identity
            );
        }
    }

    /// Master-mode input from one parsed datagram of `peer`: Sync or
    /// Follow_Up means it is a clock source (yield); an Announce may name a
    /// better grandmaster (yield) or complete the trigger (enter).
    fn update_master(&mut self, peer: IpAddr, message: Message, received_at: Instant) {
        let change = match message {
            Message::Sync { .. } | Message::FollowUp { .. } if !self.clocks.force_master => {
                self.clocks.yield_master(peer, "sender_sends_sync", received_at)
            }
            Message::Sync { .. } | Message::FollowUp { .. } => None,
            Message::Announce {
                grandmaster,
                priority1,
                priority2,
                quality,
                ..
            } => self.clocks.master_sees_announce(
                peer,
                GrandmasterDataset {
                    priority1,
                    quality,
                    priority2,
                    identity: grandmaster,
                },
                received_at,
            ),
            Message::Signaling { .. } | Message::DelayReq { .. } => None,
        };
        if let Some(change) = change {
            self.report_master(change);
        }
        if matches!(message, Message::Announce { .. }) {
            self.evaluate_masters(received_at);
        }
    }

    /// Count one parsed datagram from `peer` for the sessions it serves.
    fn note_sender_message(&self, peer: SocketAddr, message: Message, received_at: Instant) {
        self.clocks.observe(peer.ip(), |target| {
            let status = &mut target.status;
            match message {
                Message::Announce {
                    header,
                    grandmaster,
                    priority1,
                    priority2,
                    quality,
                } => {
                    status.announces += 1;
                    status.first_announce_at.get_or_insert(received_at);
                    status.grandmaster = Some(grandmaster);
                    target.last_announce = Some(LastAnnounce {
                        clock: AnnouncedClock {
                            from: peer,
                            clock: header.source.clock,
                            grandmaster,
                        },
                        dataset: GrandmasterDataset {
                            priority1,
                            quality,
                            priority2,
                            identity: grandmaster,
                        },
                        at: received_at,
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
                Message::DelayReq { .. } => status.delay_requests += 1,
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
            Message::Sync { .. } | Message::DelayReq { .. } => PTP_EVENT_PORT,
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
                // This receiver never announces; a sender naming it as
                // grandmaster is confused, and it never sends Sync, so never
                // follow that.
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
        self.update_master(peer.ip(), message, received_at);
        match message {
            Message::DelayReq { header } => {
                // Answered only while this receiver is the peer's master.
                let to = self.clocks.master_general_to(peer.ip());
                if let Some(to) = to {
                    let receive_ns = self.timescale.time_ns(received_at);
                    self.delay_responses.push((
                        peer.ip(),
                        to,
                        delay_resp(self.own_identity, header, receive_ns),
                    ));
                }
                crate::telemetry::record_ptp_datagram(
                    telemetry_token,
                    peer.ip(),
                    peer.port(),
                    local_port,
                    Some(message_name),
                    to.is_some(),
                    if to.is_some() {
                        "delay_req_answered"
                    } else {
                        "delay_req_not_master"
                    },
                );
            }
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
                priority1,
                priority2,
                ..
            } => {
                let priorities = (priority1, priority2);
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
                self.maybe_request_sync(peer, header.source.clock, grandmaster, priorities, received_at);
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
            quality: ClockQuality {
                class: packet[48],
                accuracy: packet[49],
                variance: u16::from_be_bytes([packet[50], packet[51]]),
            },
            grandmaster: u64::from_be_bytes(
                packet[53..61]
                    .try_into()
                    .expect("announce minimum length checked"),
            ),
        }),
        MESSAGE_DELAY_REQ if packet.len() >= PTP_SYNC_BYTES => Ok(Message::DelayReq { header }),
        MESSAGE_SIGNALING if packet.len() >= PTP_SIGNALING_BYTES => {
            Ok(Message::Signaling { header })
        }
        _ => Err(PtpParseError::UnsupportedMessage),
    }
}

/// Test-only (`airplay-test-hooks` Cargo feature, refused by build.rs in a
/// release build): `AUDIOHUB_TEST_AIRPLAY_PTP_FORCE_MASTER=1` makes this
/// receiver the master of every self-announcing sender after
/// [`MASTER_TRIGGER_AFTER`], even one that sends Sync, so an Apple sender
/// (macOS TimeSync) can be made a slave of it the way the iPad is.
#[cfg(feature = "airplay-test-hooks")]
fn force_master_test_hook() -> bool {
    std::env::var("AUDIOHUB_TEST_AIRPLAY_PTP_FORCE_MASTER").as_deref() == Ok("1")
}

#[cfg(not(feature = "airplay-test-hooks"))]
fn force_master_test_hook() -> bool {
    false
}

/// Master-mode variants for live experiments with a real iPad (test hooks
/// only): V1 Apple TLV trailing value, V2 ClockID/ClockPorts in SETUP
/// timingPeerInfo, V3 Apple-like Follow_Up and uptime-like timescale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct MasterVariant {
    apple_tlv_value: bool,
    clock_id: bool,
    apple_follow_up: bool,
}

impl MasterVariant {
    fn parse(text: &str) -> Self {
        let (v1, v2, v3) = match text.trim().to_ascii_lowercase().as_str() {
            "v1" => (true, false, false),
            "v2" => (false, true, false),
            "v3" => (false, false, true),
            "v4" => (true, true, true),
            _ => (false, false, false),
        };
        Self {
            apple_tlv_value: v1,
            clock_id: v2,
            apple_follow_up: v3,
        }
    }
}

/// Send PTP the way Apple's devices do (every Apple PTP datagram in the
/// 2026-11-28 captures: IPv4 TTL 255, Don't Fragment, UDP checksum 0). An
/// iPad followed Apple's receiver as master over IPv4 with such packets but
/// not ours (TTL 128, DF clear, checksum set). Sets IP_TTL 255 (and the
/// multicast TTL), and on Windows IP_DONTFRAGMENT and UDP_NOCHECKSUM. A
/// refused option is reported, never fatal: the socket still works as
/// before. Returns what each option did, for the log line.
fn apple_ip_header(socket: &UdpSocket) -> String {
    let mut done = vec![
        format!("ttl={}", socket.set_ttl(255).map_or_else(|error| error.to_string(), |()| "ok".to_owned())),
        format!(
            "mcast_ttl={}",
            socket
                .set_multicast_ttl_v4(255)
                .map_or_else(|error| error.to_string(), |()| "ok".to_owned())
        ),
    ];
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        extern "system" {
            fn setsockopt(socket: usize, level: i32, name: i32, value: *const u8, length: i32) -> i32;
        }
        const IPPROTO_IP: i32 = 0;
        const IP_DONTFRAGMENT: i32 = 14;
        const IPPROTO_UDP: i32 = 17;
        const UDP_NOCHECKSUM: i32 = 1;
        let set = |level: i32, name: i32| -> String {
            let enabled: u32 = 1;
            // SAFETY: a valid socket handle owned by `socket`, a 4-byte
            // DWORD option value and its length.
            let result = unsafe {
                setsockopt(
                    socket.as_raw_socket() as usize,
                    level,
                    name,
                    (&enabled as *const u32).cast(),
                    4,
                )
            };
            if result == 0 {
                "ok".to_owned()
            } else {
                io::Error::last_os_error().to_string()
            }
        };
        done.push(format!("df={}", set(IPPROTO_IP, IP_DONTFRAGMENT)));
        done.push(format!("udp_nochecksum={}", set(IPPROTO_UDP, UDP_NOCHECKSUM)));
    }
    // The portable backend runs on Windows; elsewhere only TTL is set.
    #[cfg(not(windows))]
    done.push("df=not_set,udp_nochecksum=not_set".to_owned());
    done.join(",")
}

/// `AUDIOHUB_TEST_AIRPLAY_PTP_MASTER_VARIANT=v0|v1|v2|v3|v4`, read at start.
#[cfg(feature = "airplay-test-hooks")]
fn master_variant_test_hook() -> MasterVariant {
    std::env::var("AUDIOHUB_TEST_AIRPLAY_PTP_MASTER_VARIANT")
        .map(|value| MasterVariant::parse(&value))
        .unwrap_or_default()
}

#[cfg(not(feature = "airplay-test-hooks"))]
fn master_variant_test_hook() -> MasterVariant {
    MasterVariant::default()
}

/// Send one datagram, waiting for the socket to be writable (a `try_send_to`
/// on a socket that has only been read can report `WouldBlock`) at most
/// [`PTP_SEND_TIMEOUT`].
async fn send_datagram(socket: &UdpSocket, packet: &[u8], to: SocketAddr) -> io::Result<()> {
    match tokio::time::timeout(PTP_SEND_TIMEOUT, socket.send_to(packet, to)).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "socket not writable")),
    }
}

/// A receive error on an unconnected UDP socket that only reports an ICMP
/// port-unreachable for an earlier send (Windows `WSAECONNRESET`, e.g.
/// after an interval request to a sender that has gone) must not end the
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
            Self::DelayReq { .. } => "delay_req",
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

        // Delay_Req is parsed (master mode answers it); other types are not.
        let delay_request = header(MESSAGE_DELAY_REQ, PTP_SYNC_BYTES, 9, 0);
        assert!(matches!(
            parse_message(&delay_request),
            Ok(Message::DelayReq { header: Header { sequence: 9, .. } })
        ));
        let pdelay_request = header(0x2, PTP_SYNC_BYTES, 9, 0);
        assert_eq!(
            parse_message(&pdelay_request),
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
                ..
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

    // ------------------------------------------------- self-announcing sender clock

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
    /// The iPad's two message interval requests as slave of macOS's AirPlay
    /// receiver (airplay-ipad-applercv-20261128T075756Z, sequenceId 2 and 3,
    /// sourcePortIdentity 342b6edcbdd20008:8014).
    const IPAD_REQUEST_INITIAL: &str = concat!(
        "1c02003c00000408000000000000000000000000342b6edcbdd20008801400020580",
        "000000000000000000000003000c0080c20000027e7e7e030000"
    );
    const IPAD_REQUEST_8HZ: &str = concat!(
        "1c02003c00000408000000000000000000000000342b6edcbdd20008801400030580",
        "000000000000000000000003000c0080c200000280fd80030000"
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

    fn watched_observer(peer: IpAddr) -> (PtpObserver, Arc<SenderClockRegistry>, SenderClockWatch) {
        let (samples, _) = broadcast::channel(SAMPLE_CHANNEL_CAPACITY);
        let clocks = Arc::new(SenderClockRegistry::default());
        let watch = clocks.register(peer);
        (
            PtpObserver::new(None, None, samples, Arc::clone(&clocks), OWN_CLOCK),
            clocks,
            watch,
        )
    }

    /// The template with this receiver's identity, port and `sequence`.
    fn expected_request(template: &str, sequence: u16) -> Vec<u8> {
        let mut packet = hex::decode(template).unwrap();
        packet[20..28].copy_from_slice(&OWN_CLOCK.to_be_bytes());
        packet[28..30].copy_from_slice(&OWN_PTP_PORT.to_be_bytes());
        packet[30..32].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    #[test]
    fn clock_identity_widens_the_device_mac_to_eui64() {
        assert_eq!(
            clock_identity_from_mac([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]),
            0x02aa_bbff_fecc_ddee
        );
    }

    #[test]
    fn interval_requests_match_the_ipad_slave_bytes_but_for_identity_and_sequence() {
        let initial = interval_request(OWN_CLOCK, 9, INTERVAL_REQUEST_INITIAL);
        assert_eq!(initial.to_vec(), expected_request(IPAD_REQUEST_INITIAL, 9));
        let sync = interval_request(OWN_CLOCK, 0xabcd, INTERVAL_REQUEST_SYNC_8HZ);
        assert_eq!(sync.to_vec(), expected_request(IPAD_REQUEST_8HZ, 0xabcd));
        // The captured templates differ from each other only there too.
        let (a, b) = (
            hex::decode(IPAD_REQUEST_INITIAL).unwrap(),
            hex::decode(IPAD_REQUEST_8HZ).unwrap(),
        );
        assert_eq!(a.len(), PTP_INTERVAL_REQUEST_BYTES);
        let differing: Vec<usize> = (0..a.len()).filter(|&index| a[index] != b[index]).collect();
        assert_eq!(differing, vec![31, 54, 55, 56]);
        // Our own parser sees a Signaling message, never an Announce.
        assert!(matches!(
            parse_message(&sync),
            Ok(Message::Signaling {
                header: Header {
                    sequence: 0xabcd,
                    source: PortIdentity { clock: OWN_CLOCK, port: OWN_PTP_PORT },
                    ..
                }
            })
        ));
    }

    #[test]
    fn captured_ipad_messages_parse() {
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
                quality: ClockQuality { class: 248, accuracy: 0x21, variance: 0x436a },
            })
        ));
        for signaling in [IPAD_SIGNALING, IPAD_REQUEST_INITIAL, IPAD_REQUEST_8HZ] {
            assert!(matches!(
                parse_message(&hex::decode(signaling).unwrap()),
                Ok(Message::Signaling { .. })
            ));
        }
    }

    #[test]
    fn a_self_announcing_sender_gets_initial_then_8hz_requests_for_the_whole_session() {
        let peer: IpAddr = "192.0.2.40".parse().unwrap();
        let (mut observer, clocks, watch) = watched_observer(peer);
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        assert_eq!(clocks.next_due(), None);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, at(0));
        // First Announce: the first request is due at once.
        assert_eq!(clocks.next_due(), Some(at(0)));
        assert_eq!(
            clocks.take_due(at(0)),
            vec![DueRequest { to: general(peer), intervals: INTERVAL_REQUEST_INITIAL }]
        );
        assert_eq!(clocks.next_due(), Some(at(30)));
        assert!(clocks.take_due(at(29)).is_empty());
        assert_eq!(
            clocks.take_due(at(30)),
            vec![DueRequest { to: general(peer), intervals: INTERVAL_REQUEST_SYNC_8HZ }]
        );
        assert_eq!(clocks.next_due(), Some(at(1_030)));
        // More Announces do not restart or double the schedule.
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, at(250));
        // Sync starting (before the master-mode trigger) does not stop it:
        // the real slave keeps asking.
        observer.receive(&ipad_follow_up(1, 10), general(peer), PTP_GENERAL_PORT, at(400));
        observer.receive(&ipad_announce(3), general(peer), PTP_GENERAL_PORT, at(500));
        assert!(clocks.take_due(at(1_000)).is_empty());
        assert_eq!(clocks.take_due(at(1_030)).len(), 1);
        assert_eq!(watch.status().unwrap().master, None, "a clock source never gets a master");
        let due = clocks.take_due(at(2_030));
        assert_eq!(
            due,
            vec![DueRequest { to: general(peer), intervals: INTERVAL_REQUEST_SYNC_8HZ }]
        );
        let status = watch.status().unwrap();
        assert_eq!((status.announces, status.follow_ups, status.interval_requests), (3, 1, 4));
        // The session ends: no more requests.
        drop(watch);
        assert_eq!(clocks.next_due(), None);
        assert!(clocks.take_due(at(10_000)).is_empty());
    }

    #[test]
    fn requests_go_only_to_watched_peers_announcing_themselves_as_grandmaster() {
        let base = Instant::now();
        // Not a session's control peer.
        let stranger: IpAddr = "192.0.2.43".parse().unwrap();
        let (mut observer, clocks, _watch) = watched_observer("192.0.2.44".parse().unwrap());
        observer.receive(&ipad_announce(1), general(stranger), PTP_GENERAL_PORT, base);
        assert_eq!(clocks.next_due(), None);

        // A boundary clock relaying another grandmaster.
        let relay: IpAddr = "192.0.2.45".parse().unwrap();
        let (mut observer, clocks, _watch) = watched_observer(relay);
        let mut announce = ipad_announce(1);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(&announce, general(relay), PTP_GENERAL_PORT, base);
        assert_eq!(clocks.next_due(), None);
        // ...that later announces itself does get requests.
        observer.receive(&ipad_announce(2), general(relay), PTP_GENERAL_PORT, base);
        assert_eq!(clocks.next_due(), Some(base));

        // Registration gone before the first Announce.
        let peer: IpAddr = "192.0.2.46".parse().unwrap();
        let (mut observer, clocks, watch) = watched_observer(peer);
        drop(watch);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, base);
        assert_eq!(clocks.next_due(), None);

        // Two sessions of one sender share one schedule.
        let peer: IpAddr = "192.0.2.47".parse().unwrap();
        let (mut observer, clocks, first) = watched_observer(peer);
        let second = clocks.register(peer);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, base);
        assert_eq!(clocks.take_due(base).len(), 1);
        assert_eq!(first.status().unwrap().interval_requests, 1);
        assert_eq!(second.status().unwrap().interval_requests, 1);
        drop(first);
        // The survivor's schedule takes over (it was started too).
        assert_eq!(clocks.take_due(base + Duration::from_millis(30)).len(), 1);
    }

    #[test]
    fn an_announce_naming_this_receiver_as_grandmaster_is_ignored() {
        let peer: IpAddr = "192.0.2.48".parse().unwrap();
        let (samples, mut receiver) = broadcast::channel(8);
        let clocks = Arc::new(SenderClockRegistry::default());
        let _watch = clocks.register(peer);
        let mut observer = PtpObserver::new(None, None, samples, Arc::clone(&clocks), OWN_CLOCK);
        let base = Instant::now();
        let mut announce = header(MESSAGE_ANNOUNCE, PTP_ANNOUNCE_BYTES, 1, 0);
        announce[20..28].copy_from_slice(&OWN_CLOCK.to_be_bytes());
        announce[53..61].copy_from_slice(&OWN_CLOCK.to_be_bytes());
        observer.receive(&announce, general(peer), PTP_GENERAL_PORT, base);
        assert!(observer.announcements.is_empty());
        assert_eq!(clocks.next_due(), None, "never request Sync from ourselves");
        let sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, 3, 0);
        observer.receive(&sync, SocketAddr::new(peer, PTP_EVENT_PORT), PTP_EVENT_PORT, base);
        let mut follow = header(MESSAGE_FOLLOW_UP, PTP_SYNC_BYTES, 3, 0);
        timestamp(&mut follow, 1, 0);
        observer.receive(&follow, general(peer), PTP_GENERAL_PORT, base);
        assert!(receiver.try_recv().is_err(), "no sample on our own identity");
    }

    #[test]
    fn one_step_sync_is_a_complete_sample_with_its_correction() {
        let peer: IpAddr = "192.0.2.49".parse().unwrap();
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
    async fn due_requests_go_out_from_the_general_socket_with_fresh_sequence_ids() {
        let general_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let sender_address = sender.local_addr().unwrap();
        let (samples, _) = broadcast::channel(8);
        let clocks = Arc::new(SenderClockRegistry::default());
        let _watch = clocks.register(sender_address.ip());
        let mut observer = PtpObserver::new(None, None, samples, Arc::clone(&clocks), OWN_CLOCK);
        let base = Instant::now();
        // The Announce "comes from" the test socket, so requests go there.
        observer.maybe_request_sync(sender_address, IPAD_CLOCK, IPAD_CLOCK, (250, 246), base);
        observer.send_due_requests(&general_socket, base).await;
        observer
            .send_due_requests(&general_socket, base + Duration::from_millis(10))
            .await;
        observer
            .send_due_requests(&general_socket, base + INTERVAL_REQUEST_FOLLOW_DELAY)
            .await;
        let mut buffer = [0u8; 128];
        let mut received = Vec::new();
        for _ in 0..2 {
            let (length, from) = tokio::time::timeout(Duration::from_secs(2), sender.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(from, general_socket.local_addr().unwrap());
            received.push(buffer[..length].to_vec());
        }
        assert_eq!(received[0], expected_request(IPAD_REQUEST_INITIAL, 0));
        assert_eq!(received[1], expected_request(IPAD_REQUEST_8HZ, 1));
    }

    #[test]
    fn a_connection_reset_does_not_end_the_observer() {
        let reset: io::Result<u8> = Err(io::Error::from(io::ErrorKind::ConnectionReset));
        assert!(matches!(tolerate_reset(reset), Ok(None)));
        let other: io::Result<u8> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(tolerate_reset(other).is_err());
        assert!(matches!(tolerate_reset(Ok(3u8)), Ok(Some(3))));
    }
    // ------------------------------------------------- receiver as PTP master

    /// Apple's own AirPlay receiver (macOS, 10.130.32.13) as the iPad's
    /// master (airplay-ipad-applercv-20261128T075756Z/ipad-ptp.pcap): its
    /// first Announce, Signaling, Sync and Follow_Up of the session on
    /// sourcePort 0x8016.
    const APPLE_RCV_CLOCK: u64 = 0x5273_7efc_a4c2_0008;
    const APPLE_RCV_ANNOUNCE: &str = concat!(
        "1b02004c0000040800000000000000000000000052737efca4c200088016000105fe",
        "00000000000000000000002500faf821436aed52737efca4c200080000a000080008",
        "52737efca4c20008"
    );
    const APPLE_RCV_SIGNALING: &str = concat!(
        "1c02006a0000040800000000000000000000000052737efca4c20008801600000580",
        "0000000000000000000000030016000d930000010007000000002710000027100039",
        "676300030020000d93000005001f00000000000027100000271000002710000027100039",
        "6763"
    );
    const APPLE_RCV_SYNC: &str = concat!(
        "1002002c0000060800000000000000000000000052737efca4c200088016000100fd",
        "00000000000000000000"
    );
    const APPLE_RCV_FOLLOW_UP: &str = concat!(
        "180200600000040800000000fafa00000000000052737efca4c200088016000102fd",
        "00000027d39a180318e60003001c0080c2000001000000000000ffffffff00069cd4",
        "f1bd68b5fec72af700030010000d9300000452737efca4c200080000"
    );

    /// A captured Apple receiver message with our port number.
    fn apple_rcv(text: &str) -> Vec<u8> {
        let mut packet = hex::decode(text).unwrap();
        packet[28..30].copy_from_slice(&OWN_PTP_PORT.to_be_bytes());
        packet
    }

    fn ipad_dataset() -> GrandmasterDataset {
        GrandmasterDataset {
            priority1: 250,
            quality: ClockQuality {
                class: 248,
                accuracy: 0x21,
                variance: 0x436a,
            },
            priority2: 246,
            identity: IPAD_CLOCK,
        }
    }

    #[test]
    fn master_messages_match_the_apple_receiver_bytes_but_for_documented_fields() {
        // Announce and Sync: identical, given the Apple receiver's dataset.
        let apple = GrandmasterDataset {
            priority2: 237,
            identity: APPLE_RCV_CLOCK,
            ..ipad_dataset()
        };
        assert_eq!(master_announce(apple, 1).to_vec(), apple_rcv(APPLE_RCV_ANNOUNCE));
        assert_eq!(master_sync(APPLE_RCV_CLOCK, 1).to_vec(), apple_rcv(APPLE_RCV_SYNC));
        // Signaling: identical but for the last three (time-varying) bytes
        // of each Apple TLV, which are zero.
        let mut expected = apple_rcv(APPLE_RCV_SIGNALING);
        assert_eq!(expected.len(), PTP_MASTER_SIGNALING_BYTES);
        for end in [PTP_SIGNALING_BYTES + 26, PTP_MASTER_SIGNALING_BYTES] {
            assert_ne!(expected[end - 3..end], [0, 0, 0]);
            expected[end - 3..end].fill(0);
        }
        assert_eq!(master_signaling(APPLE_RCV_CLOCK, 0).to_vec(), expected);
        // Follow_Up: same origin; correctionField (the Mac's egress
        // correction) and the 802.1AS TLV's grandmaster-change history zero.
        let mut expected = apple_rcv(APPLE_RCV_FOLLOW_UP);
        assert_eq!(expected.len(), PTP_MASTER_FOLLOW_UP_BYTES);
        let origin = parse_timestamp(&expected[PTP_HEADER_BYTES..PTP_SYNC_BYTES]).unwrap();
        expected[8..16].fill(0);
        expected[54..76].fill(0);
        assert_eq!(master_follow_up(APPLE_RCV_CLOCK, 1, origin).to_vec(), expected);

        // Our own parser reads them back.
        let ours = master_dataset(ipad_dataset(), OWN_CLOCK).unwrap();
        assert!(matches!(
            parse_message(&master_announce(ours, 7)),
            Ok(Message::Announce {
                header: Header { sequence: 7, source: PortIdentity { clock: OWN_CLOCK, port: OWN_PTP_PORT }, .. },
                grandmaster: OWN_CLOCK,
                priority1: 250,
                priority2: 245,
                quality: ClockQuality { class: 248, accuracy: 0x21, variance: 0x436a },
            })
        ));
        assert!(matches!(
            parse_message(&master_sync(OWN_CLOCK, 9)),
            Ok(Message::Sync { one_step_origin_ns: None, header: Header { sequence: 9, .. } })
        ));
        assert!(matches!(
            parse_message(&master_follow_up(OWN_CLOCK, 9, 1_795_000_000_123_456_789)),
            Ok(Message::FollowUp { origin_ns: 1_795_000_000_123_456_789, header: Header { sequence: 9, .. } })
        ));
        assert!(matches!(
            parse_message(&master_signaling(OWN_CLOCK, 3)),
            Ok(Message::Signaling { .. })
        ));
    }

    #[test]
    fn master_dataset_beats_the_sender_by_one_priority_step() {
        let ipad = ipad_dataset();
        let ours = master_dataset(ipad, OWN_CLOCK).unwrap();
        assert_eq!(
            (ours.priority1, ours.priority2, ours.quality, ours.identity),
            (250, 245, ipad.quality, OWN_CLOCK)
        );
        assert!(ours.better_than(ipad) && !ipad.better_than(ours));
        // Apple's receiver (priority2 237) beats us: we would yield to it.
        let apple = GrandmasterDataset {
            priority2: 237,
            identity: APPLE_RCV_CLOCK,
            ..ipad
        };
        assert!(apple.better_than(ours));
        // priority2 0: one priority1 step instead.
        let zero = GrandmasterDataset { priority2: 0, ..ipad };
        let ours = master_dataset(zero, OWN_CLOCK).unwrap();
        assert_eq!((ours.priority1, ours.priority2), (249, 0));
        assert!(ours.better_than(zero));
        // Nothing left to step: refuse rather than claim a better clock.
        let floor = GrandmasterDataset {
            priority1: 0,
            priority2: 0,
            ..ipad
        };
        assert_eq!(master_dataset(floor, OWN_CLOCK), None);
        // BMCA order: clock quality decides before priority2.
        let gps = GrandmasterDataset {
            quality: ClockQuality { class: 6, ..ipad.quality },
            priority2: 255,
            ..ipad
        };
        assert!(gps.better_than(master_dataset(ipad, OWN_CLOCK).unwrap()));
    }

    fn sync_from(clock: u64, sequence: u16) -> Vec<u8> {
        let mut sync = header(MESSAGE_SYNC, PTP_SYNC_BYTES, sequence, 0);
        sync[20..28].copy_from_slice(&clock.to_be_bytes());
        sync
    }

    fn event(peer: IpAddr) -> SocketAddr {
        SocketAddr::new(peer, PTP_EVENT_PORT)
    }

    fn master_mode(watch: &SenderClockWatch) -> Option<MasterMode> {
        watch.status().unwrap().master.map(|master| master.mode)
    }

    #[test]
    fn a_silent_self_announcing_sender_gets_this_receiver_as_master_after_the_trigger() {
        let peer: IpAddr = "192.0.2.60".parse().unwrap();
        let (mut observer, clocks, watch) = watched_observer(peer);
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, at(0));
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, at(250));
        observer.evaluate_masters(at(499));
        assert_eq!(master_mode(&watch), None, "not before the trigger");
        observer.receive(&ipad_announce(3), general(peer), PTP_GENERAL_PORT, at(500));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        // Announce (+ Signaling) and Sync (+ Follow_Up) are due at once, to the
        // peer's 320 and 319, with our dataset.
        assert_eq!(clocks.next_due(), Some(at(500)));
        let ours = master_dataset(ipad_dataset(), OWN_CLOCK).unwrap();
        assert_eq!(
            clocks.take_master_due(at(500)),
            vec![
                MasterSend::Announce { peer, to: general(peer), dataset: ours, sequence: 1 },
                MasterSend::Sync { peer, event_to: event(peer), general_to: general(peer), sequence: 1 },
            ]
        );
        // 8 Sync and 4 Announce a second.
        assert!(clocks.take_master_due(at(624)).is_empty());
        assert_eq!(
            clocks.take_master_due(at(625)),
            vec![MasterSend::Sync { peer, event_to: event(peer), general_to: general(peer), sequence: 2 }]
        );
        assert_eq!(clocks.take_master_due(at(750)).len(), 2);
        // Interval requests (started at the first Announce) pause meanwhile.
        assert!(clocks.take_due(at(5_000)).is_empty());
        // More silent Announces (the iPad before it yields) change nothing.
        observer.receive(&ipad_announce(4), general(peer), PTP_GENERAL_PORT, at(760));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        let status = watch.status().unwrap();
        assert_eq!(status.master.map(|master| (master.announces, master.syncs)), Some((2, 3)));
        assert!(status.to_string().contains(" master=active(announces=2 syncs=3)"));

        // The sender starts sending Sync after all: yield for good, requests resume.
        observer.receive(&sync_from(IPAD_CLOCK, 1), event(peer), PTP_EVENT_PORT, at(6_000));
        assert_eq!(master_mode(&watch), Some(MasterMode::Yielded("sender_sends_sync")));
        assert!(clocks.take_master_due(at(9_000)).is_empty());
        assert_eq!(clocks.take_due(at(6_000)).len(), 1);
        observer.receive(&ipad_announce(5), general(peer), PTP_GENERAL_PORT, at(6_100));
        observer.evaluate_masters(at(9_000));
        assert_eq!(master_mode(&watch), Some(MasterMode::Yielded("sender_sends_sync")), "not re-entered");
        // The last session ends: master state goes with it.
        drop(watch);
        assert_eq!(clocks.next_due(), None);
        assert!(clocks.take_master_due(at(10_000)).is_empty());
    }

    #[test]
    fn senders_that_send_sync_or_follow_others_never_get_a_master() {
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        // A Mac: announces itself and sends Sync/Follow_Up at once.
        let mac: IpAddr = "192.0.2.61".parse().unwrap();
        let (mut observer, clocks, watch) = watched_observer(mac);
        for (index, ms) in [0u64, 250, 500, 750, 1_000].into_iter().enumerate() {
            observer.receive(&ipad_announce(index as u16 + 1), general(mac), PTP_GENERAL_PORT, at(ms));
            observer.receive(&sync_from(IPAD_CLOCK, index as u16), event(mac), PTP_EVENT_PORT, at(ms + 70));
        }
        observer.evaluate_masters(at(5_000));
        assert_eq!(master_mode(&watch), None);
        assert!(clocks.take_master_due(at(5_000)).is_empty());

        // A sender relaying another grandmaster (boundary clock).
        let relay: IpAddr = "192.0.2.62".parse().unwrap();
        let (mut observer, _clocks, watch) = watched_observer(relay);
        let mut announce = ipad_announce(1);
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(&announce, general(relay), PTP_GENERAL_PORT, at(0));
        observer.receive(&announce, general(relay), PTP_GENERAL_PORT, at(600));
        assert_eq!(master_mode(&watch), None);

        // Not a session's control peer.
        let stranger: IpAddr = "192.0.2.63".parse().unwrap();
        let (mut observer, clocks, _watch) = watched_observer("192.0.2.64".parse().unwrap());
        observer.receive(&ipad_announce(1), general(stranger), PTP_GENERAL_PORT, at(0));
        observer.receive(&ipad_announce(2), general(stranger), PTP_GENERAL_PORT, at(600));
        assert!(clocks.take_master_due(at(600)).is_empty());

        // A stale Announce (the sender went quiet) does not start it.
        let quiet: IpAddr = "192.0.2.65".parse().unwrap();
        let (mut observer, _clocks, watch) = watched_observer(quiet);
        observer.receive(&ipad_announce(1), general(quiet), PTP_GENERAL_PORT, at(0));
        observer.evaluate_masters(at(0) + ANNOUNCE_LIFETIME + Duration::from_millis(1));
        assert_eq!(master_mode(&watch), None);
    }

    #[test]
    fn a_better_grandmaster_makes_master_mode_yield_and_an_unbeatable_one_refuses_it() {
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let peer: IpAddr = "192.0.2.66".parse().unwrap();
        let (mut observer, _clocks, watch) = watched_observer(peer);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, at(0));
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, at(500));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        // Its own worse dataset again: no change.
        observer.receive(&ipad_announce(3), general(peer), PTP_GENERAL_PORT, at(750));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        // It now relays a grandmaster with priority2 237 (better than our 245).
        let mut announce = ipad_announce(4);
        announce[52] = 237;
        announce[53..61].copy_from_slice(&GRANDMASTER.to_be_bytes());
        observer.receive(&announce, general(peer), PTP_GENERAL_PORT, at(1_000));
        assert_eq!(master_mode(&watch), Some(MasterMode::Yielded("better_grandmaster")));

        // priority1 and priority2 both 0: refused, nothing sent.
        let floor: IpAddr = "192.0.2.67".parse().unwrap();
        let (mut observer, clocks, watch) = watched_observer(floor);
        let mut announce = ipad_announce(1);
        announce[47] = 0;
        announce[52] = 0;
        observer.receive(&announce, general(floor), PTP_GENERAL_PORT, at(0));
        observer.receive(&announce, general(floor), PTP_GENERAL_PORT, at(500));
        assert_eq!(master_mode(&watch), Some(MasterMode::Refused));
        assert!(clocks.take_master_due(at(500)).is_empty());
        assert!(watch.status().unwrap().to_string().ends_with(" master=refused"));
    }

    /// macOS TimeSync's Delay_Req as our slave (mac-audio-lab, 2026-11-28,
    /// round2/runs/x1-forced-master-mac-music/ptp.pcap, sequenceId 1).
    const MAC_DELAY_REQ: &str = concat!(
        "1102002c0000040800000000000000000000000000155ddf2d1700088000000100fd",
        "00000000000000000000"
    );

    #[test]
    fn a_slave_delay_request_gets_a_delay_response_only_in_master_mode() {
        let peer: IpAddr = "192.0.2.69".parse().unwrap();
        let (mut observer, _clocks, watch) = watched_observer(peer);
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut request = hex::decode(MAC_DELAY_REQ).unwrap();
        request[8..16].copy_from_slice(&(5i64 * 65_536).to_be_bytes());
        // Not master yet: parsed, counted, not answered.
        observer.receive(&request, event(peer), PTP_EVENT_PORT, at(0));
        assert!(observer.delay_responses.is_empty());
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, at(0));
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, at(500));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        // Delay_Req on the general port is a port mismatch.
        observer.receive(&request, general(peer), PTP_GENERAL_PORT, at(600));
        assert!(observer.delay_responses.is_empty());
        observer.receive(&request, event(peer), PTP_EVENT_PORT, at(700));
        let (to_peer, to, response) = observer.delay_responses.pop().expect("answered");
        assert_eq!((to_peer, to), (peer, general(peer)));
        let receive_ns = observer.timescale.time_ns(at(700));
        let mut expected = vec![0u8; PTP_DELAY_RESP_BYTES];
        expected[..4].copy_from_slice(&[0x19, 0x02, 0x00, 0x36]);
        expected[6..8].copy_from_slice(&[0x04, 0x08]);
        expected[8..16].copy_from_slice(&(5i64 * 65_536).to_be_bytes());
        expected[20..28].copy_from_slice(&OWN_CLOCK.to_be_bytes());
        expected[28..30].copy_from_slice(&OWN_PTP_PORT.to_be_bytes());
        expected[30..32].copy_from_slice(&1u16.to_be_bytes());
        expected[32..34].copy_from_slice(&[0x03, 0xfd]);
        write_timestamp(&mut expected[34..44], receive_ns);
        expected[44..54].copy_from_slice(&hex::decode("00155ddf2d1700088000").unwrap());
        assert_eq!(response.to_vec(), expected);
        assert_eq!(watch.status().unwrap().delay_requests, 2);
        assert!(watch.status().unwrap().to_string().contains(" delay_req=2 master=active"));
    }

    #[test]
    fn master_variants_parse_and_shape_messages_like_apple() {
        assert_eq!(MasterVariant::parse("v0"), MasterVariant::default());
        assert_eq!(MasterVariant::parse("bogus"), MasterVariant::default());
        assert_eq!(
            MasterVariant::parse(" V4 "),
            MasterVariant { apple_tlv_value: true, clock_id: true, apple_follow_up: true }
        );
        assert!(MasterVariant::parse("v2").clock_id && !MasterVariant::parse("v2").apple_tlv_value);
        // V1: the Apple receiver's Signaling but for its median trailing value.
        let mut expected = apple_rcv(APPLE_RCV_SIGNALING);
        for end in [PTP_SIGNALING_BYTES + 26, PTP_MASTER_SIGNALING_BYTES] {
            expected[end - 3..end].copy_from_slice(&APPLE_SIGNALING_TRAILING_MEDIAN);
        }
        assert_eq!(
            master_signaling_with(APPLE_RCV_CLOCK, 0, APPLE_SIGNALING_TRAILING_MEDIAN).to_vec(),
            expected
        );
        // V3: Apple's TLV bytes; origin + correction is the corrected time.
        let fu = master_follow_up_apple_like(APPLE_RCV_CLOCK, 1, 5_000_000_000);
        let mut apple = apple_rcv(APPLE_RCV_FOLLOW_UP);
        assert_eq!(fu[44..96], apple[44..96]);
        let Ok(Message::FollowUp { origin_ns, header }) = parse_message(&fu) else {
            panic!("Follow_Up");
        };
        assert_eq!(origin_ns + (header.correction_scaled_ns >> 16) as u64, 5_000_000_000);
        apple[8..16].copy_from_slice(&fu[8..16]);
        apple[34..44].copy_from_slice(&fu[34..44]);
        assert_eq!(fu.to_vec(), apple);
        // V2: ClockID/port only with the variant and an identity.
        let mut source = PtpClockSource::test_source();
        assert_eq!(source.local_port_identity(), None);
        source.clocks = Arc::new(SenderClockRegistry {
            variant: MasterVariant::parse("v2"),
            ..SenderClockRegistry::new(OWN_CLOCK)
        });
        #[cfg(not(target_os = "macos"))]
        assert_eq!(source.local_port_identity(), Some((OWN_CLOCK, OWN_PTP_PORT)));
        // The uptime-like timescale is small (seconds since boot).
        assert!(OwnTimescale::uptime().time_ns(Instant::now()) < 400 * 86_400 * NANOS_PER_SECOND as u64);
    }

    #[tokio::test]
    async fn ptp_sockets_get_apple_ttl_255_and_the_windows_options() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let outcome = apple_ip_header(&socket);
        assert_eq!(socket.ttl().unwrap(), 255, "{outcome}");
        assert!(outcome.contains("ttl=ok"), "{outcome}");
        #[cfg(windows)]
        assert!(outcome.contains("df=ok") && outcome.contains("udp_nochecksum=ok"), "{outcome}");
    }

    #[test]
    fn the_force_master_test_hook_masters_a_sender_that_sends_sync_and_ignores_its_sync() {
        let peer: IpAddr = "192.0.2.68".parse().unwrap();
        let (samples, _) = broadcast::channel(8);
        let clocks = Arc::new(SenderClockRegistry {
            force_master: true,
            ..SenderClockRegistry::new(OWN_CLOCK)
        });
        let watch = clocks.register(peer);
        let mut observer = PtpObserver::new(None, None, samples, Arc::clone(&clocks), OWN_CLOCK);
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, at(0));
        observer.receive(&sync_from(IPAD_CLOCK, 1), event(peer), PTP_EVENT_PORT, at(70));
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, at(500));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active));
        observer.receive(&sync_from(IPAD_CLOCK, 2), event(peer), PTP_EVENT_PORT, at(570));
        assert_eq!(master_mode(&watch), Some(MasterMode::Active), "its Sync is ignored");
        // A better grandmaster still wins.
        let mut announce = ipad_announce(3);
        announce[47] = 1;
        observer.receive(&announce, general(peer), PTP_GENERAL_PORT, at(750));
        assert_eq!(master_mode(&watch), Some(MasterMode::Yielded("better_grandmaster")));
        // Without the hook (the release default) nothing changes.
        assert!(!force_master_test_hook() || cfg!(feature = "airplay-test-hooks"));
    }

    #[tokio::test]
    async fn master_messages_go_out_and_each_sync_is_an_exact_sample_of_the_own_timeline() {
        let event_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let general_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let peer_event = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let peer_general = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let peer = peer_general.local_addr().unwrap().ip();
        let (samples, mut receiver) = broadcast::channel(8);
        let clocks = Arc::new(SenderClockRegistry::new(OWN_CLOCK));
        let _watch = clocks.register(peer);
        let mut observer = PtpObserver::new(None, None, samples, Arc::clone(&clocks), OWN_CLOCK);
        let base = Instant::now();
        observer.receive(&ipad_announce(1), general(peer), PTP_GENERAL_PORT, base);
        let entered = base + MASTER_TRIGGER_AFTER;
        observer.receive(&ipad_announce(2), general(peer), PTP_GENERAL_PORT, entered);
        clocks.test_redirect_master(peer, peer_event.local_addr().unwrap(), peer_general.local_addr().unwrap());
        let before = Instant::now();
        observer.send_master_due(&event_socket, &general_socket, entered).await;

        let mut buffer = [0u8; 256];
        let mut general_messages = Vec::new();
        for _ in 0..3 {
            let (length, from) = tokio::time::timeout(Duration::from_secs(2), peer_general.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(from, general_socket.local_addr().unwrap());
            general_messages.push(buffer[..length].to_vec());
        }
        let (length, from) = tokio::time::timeout(Duration::from_secs(2), peer_event.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, event_socket.local_addr().unwrap());
        let ours = master_dataset(ipad_dataset(), OWN_CLOCK).unwrap();
        assert_eq!(buffer[..length].to_vec(), master_sync(OWN_CLOCK, 1).to_vec());
        assert_eq!(general_messages[0], master_announce(ours, 1).to_vec());
        assert_eq!(general_messages[1], master_signaling(OWN_CLOCK, 0).to_vec());
        let Ok(Message::FollowUp { origin_ns, header }) = parse_message(&general_messages[2]) else {
            panic!("third general message is the Follow_Up");
        };
        assert_eq!((header.sequence, header.source.clock), (1, OWN_CLOCK));

        let sample = receiver.try_recv().unwrap();
        assert_eq!((sample.peer, sample.grandmaster), (peer, OWN_CLOCK));
        assert_eq!(sample.remote_time_ns, origin_ns);
        assert!(sample.received_at >= before && sample.received_at <= Instant::now());
        assert_eq!(observer.timescale.time_ns(sample.received_at), origin_ns);
        // Our timescale is the local clock plus a constant.
        let later = sample.received_at + Duration::from_millis(1_234);
        assert_eq!(observer.timescale.time_ns(later) - origin_ns, 1_234_000_000);
        assert!(receiver.try_recv().is_err(), "one sample per Sync");
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
