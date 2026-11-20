//! Per-sender gPTP instances through macOS TimeSync.framework
//! (docs/airplay-multi-sender-design-20261120.md §2.4).
//!
//! CoreMedia's `CM8021ASClock` is one system gPTP domain with one
//! grandmaster, so it can follow only one AirPlay sender at a time. The
//! private `TSgPTPManager -addPTPInstance:error:` creates an independent
//! instance with its own clock identity and BMCA; one instance per sender IP
//! lets several senders lock at once (experiment ptp-mgm-20261119T151245Z, V3).
//!
//! Layout:
//! - Platform-independent pieces (type-encoding skeletons, mach timebase,
//!   the published mapping line and its seqlock, the circuit breaker and the
//!   lock watchdog rule) compile on every host so their unit tests run on the
//!   Windows VM as well.
//! - The Objective-C service is macOS-only. Every ObjC object lives on one
//!   dedicated thread, `ahb-airplay-timesync`; each command and refresh runs
//!   inside an autorelease pool. Media and Tokio threads never send an ObjC
//!   message: they read the line through a seqlock.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use super::ptp::{PlatformHandoff, PtpClockError};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Refresh period of every instance's mapping line.
pub(crate) const LINE_REFRESH: Duration = Duration::from_millis(100);
/// A line older than this is not used for mapping.
pub(crate) const LINE_MAX_AGE: Duration = Duration::from_millis(500);
/// Media demand without any valid line for this long fails the port.
pub(crate) const LOCK_WATCHDOG: Duration = Duration::from_secs(10);
/// Demand counts as continuous while map requests are closer than this.
pub(crate) const DEMAND_IDLE: Duration = Duration::from_secs(2);
/// While no instance has ever mapped, lock failures from this many distinct
/// peer addresses trip the breaker.
pub(crate) const BREAKER_DISTINCT_PEERS: usize = 2;
/// Bound on the remembered failing peer addresses.
const MAX_FAILED_PEERS: usize = 64;
/// Upper bound of the availability probe, including manager start-up.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// TimeSync `lockState` value meaning "locked".
pub(crate) const LOCK_STATE_LOCKED: i32 = 2;
/// TimeSync `portRole` value meaning "slave" (the instance follows the peer).
pub(crate) const PORT_ROLE_SLAVE: i32 = 3;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

// ---------------------------------------------------------------- encodings

/// One private selector the backend calls, with the type-encoding skeleton
/// (offsets removed) taken from the macOS 26.6 dumps `objc.txt` /
/// `objc-port.txt`. `c` stands for BOOL and also accepts arm64's `B`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequiredSelector {
    pub(crate) class: &'static str,
    pub(crate) selector: &'static str,
    pub(crate) skeleton: &'static str,
    pub(crate) class_method: bool,
}

const fn instance(class: &'static str, selector: &'static str, skeleton: &'static str) -> RequiredSelector {
    RequiredSelector {
        class,
        selector,
        skeleton,
        class_method: false,
    }
}

pub(crate) const REQUIRED_CLASSES: [&str; 4] = [
    "TSgPTPManager",
    "TSClockManager",
    "TSgPTPClock",
    "TSgPTPNetworkPort",
];

pub(crate) const REQUIRED_SELECTORS: [RequiredSelector; 18] = [
    RequiredSelector {
        class: "TSgPTPManager",
        selector: "sharedgPTPManager",
        skeleton: "@@:",
        class_method: true,
    },
    RequiredSelector {
        class: "TSClockManager",
        selector: "sharedClockManager",
        skeleton: "@@:",
        class_method: true,
    },
    instance("TSgPTPManager", "addPTPInstance:error:", "c@:^Q^@"),
    instance("TSgPTPManager", "removePTPInstanceWithIdentifier:error:", "c@:Q^@"),
    instance("TSClockManager", "clockWithClockIdentifier:", "@@:Q"),
    instance(
        "TSgPTPClock",
        "addUnicastUDPv4EtEPortOnInterfaceNamed:withDestinationAddress:allocatedPortNumber:error:",
        "c@:@I^S^@",
    ),
    instance(
        "TSgPTPClock",
        "removeUnicastUDPv4EtEPortFromInterfaceNamed:withDestinationAddress:error:",
        "c@:@I^@",
    ),
    instance(
        "TSgPTPClock",
        "convertFromDomainToMachAbsoluteTime:grandmasterUsed:portNumber:",
        "Q@:Q^Q^S",
    ),
    instance("TSgPTPClock", "lockState", "i@:"),
    instance("TSgPTPClock", "grandmasterIdentity", "Q@:"),
    instance("TSgPTPClock", "clockIdentity", "Q@:"),
    instance("TSgPTPClock", "ports", "@@:"),
    instance("TSgPTPNetworkPort", "portNumber", "S@:"),
    instance("TSgPTPNetworkPort", "portRole", "i@:"),
    instance("TSgPTPNetworkPort", "enabled", "c@:"),
    instance("TSgPTPNetworkPort", "enablePortError:", "c@:^@"),
    instance(
        "TSgPTPNetworkPort",
        "overrideReceiveMatchingWithRemoteClockIdentity:remotePortNumber:error:",
        "c@:QS^@",
    ),
    // Not strictly required: without it the line is anchored at the last
    // remote time requested by media instead of at "now".
    instance(
        "TSgPTPClock",
        "convertFromMachAbsoluteToDomainTime:grandmasterUsed:portNumber:",
        "Q@:Q^Q^S",
    ),
];

/// Index of the optional reverse conversion in [`REQUIRED_SELECTORS`].
pub(crate) const OPTIONAL_REVERSE_CONVERT: usize = 17;

/// Remove the frame offsets from an ObjC method type encoding.
pub(crate) fn encoding_skeleton(encoding: &str) -> String {
    encoding.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// Compare a runtime encoding with an expected skeleton. A `c` (BOOL on
/// x86_64) in the expectation also accepts `B` (BOOL on arm64).
pub(crate) fn encoding_matches(actual: &str, expected_skeleton: &str) -> bool {
    let actual = encoding_skeleton(actual);
    actual.len() == expected_skeleton.len()
        && actual
            .bytes()
            .zip(expected_skeleton.bytes())
            .all(|(a, e)| a == e || (e == b'c' && a == b'B'))
}

// ---------------------------------------------------------------- timebase

/// `mach_timebase_info`: nanoseconds = ticks * numer / denom. x86_64 is 1/1,
/// Apple silicon is 125/3 (24 MHz ticks); forgetting the conversion there is
/// a 41.7x timing error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MachTimebase {
    numer: u32,
    denom: u32,
}

impl MachTimebase {
    pub(crate) fn new(numer: u32, denom: u32) -> Option<Self> {
        (numer != 0 && denom != 0).then_some(Self { numer, denom })
    }

    pub(crate) fn ticks_to_ns(self, ticks: i128) -> i128 {
        div_round(ticks * i128::from(self.numer), i128::from(self.denom))
    }

    /// Nominal mach ticks per second.
    pub(crate) fn ticks_per_second(self) -> i128 {
        div_round(NANOS_PER_SECOND * i128::from(self.denom), i128::from(self.numer))
    }
}

fn div_round(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    }
}

// ---------------------------------------------------------------- line

/// One published mapping from an instance's gPTP domain to mach ticks plus
/// the health fields read in the same refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TsLine {
    /// Domain (grandmaster) time of the anchor, ns.
    pub(crate) domain_anchor_ns: u64,
    /// Mach ticks of `domain_anchor_ns`.
    pub(crate) mach_anchor: u64,
    /// Mach ticks per second of domain time; zero when no line exists.
    pub(crate) mach_per_domain_second: u64,
    pub(crate) lock_state: i32,
    /// Grandmaster used by the conversion.
    pub(crate) grandmaster: u64,
    /// The instance's own clock identity.
    pub(crate) self_identity: u64,
    pub(crate) port_role: i32,
    /// Process-monotonic ns at which the fields were sampled.
    pub(crate) sampled_at_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineRejection {
    Stale,
    NotLocked,
    /// The instance elected itself (or the port became master): the sender's
    /// grandmaster is gone.
    GrandmasterLost,
    NoMapping,
    SourceMismatch,
    OutOfRange,
}

impl From<LineRejection> for PtpClockError {
    fn from(value: LineRejection) -> Self {
        match value {
            LineRejection::Stale => PtpClockError::Stale,
            LineRejection::NotLocked
            | LineRejection::GrandmasterLost
            | LineRejection::NoMapping => PtpClockError::NotReady,
            LineRejection::SourceMismatch => PtpClockError::SourceMismatch,
            LineRejection::OutOfRange => PtpClockError::MappedTimeOutOfRange,
        }
    }
}

impl TsLine {
    /// Whether the instance currently follows a remote grandmaster.
    /// lockState alone stays 2 after the grandmaster disappears (experiment
    /// §4.7); the self-identity and port-role checks catch that case.
    pub(crate) fn health(&self) -> Result<(), LineRejection> {
        if self.lock_state != LOCK_STATE_LOCKED {
            return Err(LineRejection::NotLocked);
        }
        if self.grandmaster == 0
            || self.grandmaster == self.self_identity
            || self.port_role != PORT_ROLE_SLAVE
        {
            return Err(LineRejection::GrandmasterLost);
        }
        if self.mach_per_domain_second == 0 {
            return Err(LineRejection::NoMapping);
        }
        Ok(())
    }

    /// Extrapolate `remote_ns` (domain time of `expected_grandmaster`) to mach
    /// ticks. `now_ns` is on the same process-monotonic scale as
    /// `sampled_at_ns`.
    pub(crate) fn map_to_mach(
        &self,
        now_ns: u64,
        remote_ns: u64,
        expected_grandmaster: u64,
    ) -> Result<u64, LineRejection> {
        if now_ns.saturating_sub(self.sampled_at_ns) > LINE_MAX_AGE.as_nanos() as u64 {
            return Err(LineRejection::Stale);
        }
        self.health()?;
        if self.grandmaster != expected_grandmaster {
            return Err(LineRejection::SourceMismatch);
        }
        let delta = i128::from(remote_ns) - i128::from(self.domain_anchor_ns);
        let ticks = div_round(
            delta * i128::from(self.mach_per_domain_second),
            NANOS_PER_SECOND,
        );
        u64::try_from(i128::from(self.mach_anchor) + ticks).map_err(|_| LineRejection::OutOfRange)
    }

    /// Domain (grandmaster) time at `mach` by linear extrapolation.
    pub(crate) fn domain_at_mach(&self, mach: u64) -> Option<i128> {
        if self.mach_per_domain_second == 0 {
            return None;
        }
        let ticks = i128::from(mach) - i128::from(self.mach_anchor);
        Some(
            i128::from(self.domain_anchor_ns)
                + div_round(
                    ticks * NANOS_PER_SECOND,
                    i128::from(self.mach_per_domain_second),
                ),
        )
    }
}

/// Handoff from the instance's retired line (last healthy line on `from`)
/// to its current line, evaluated at one host instant `mach_now`. A planned
/// grandmaster change keeps the current line healthy, so it never counts as
/// a lock failure; this only computes how to carry the anchor across.
pub(crate) fn handoff_between_lines(
    retired: Option<TsLine>,
    current: Option<TsLine>,
    from: u64,
    now_ns: u64,
    mach_now: u64,
) -> Result<PlatformHandoff, &'static str> {
    let current = current.ok_or("new_grandmaster_not_locked")?;
    if now_ns.saturating_sub(current.sampled_at_ns) > LINE_MAX_AGE.as_nanos() as u64 {
        return Err("new_line_stale");
    }
    // Never map through the instance's own clock or an unlocked line.
    current.health().map_err(|_| "new_grandmaster_not_locked")?;
    if current.grandmaster == from {
        return Err("no_grandmaster_change");
    }
    let retired = retired
        .filter(|line| line.grandmaster == from)
        .ok_or("old_mapping_unavailable")?;
    let max_age = super::ptp_session::HANDOFF_MAX_OLD_AGE.as_nanos() as u64;
    if now_ns.saturating_sub(retired.sampled_at_ns) > max_age {
        return Err("old_mapping_too_old");
    }
    let old = retired
        .domain_at_mach(mach_now)
        .ok_or("old_mapping_unavailable")?;
    let new = current
        .domain_at_mach(mach_now)
        .ok_or("new_mapping_unavailable")?;
    Ok(PlatformHandoff {
        grandmaster: current.grandmaster,
        delta_ns: new - old,
    })
}

// ---------------------------------------------------------------- peer ports

/// Reference counts of the extra unicast ports one instance opened for
/// SETPEERS peers, shared by every session lease on that instance.
#[derive(Debug, Default)]
pub(crate) struct ExtraPortLeases {
    refs: HashMap<Ipv4Addr, usize>,
    leases: HashMap<u64, Vec<Ipv4Addr>>,
}

/// Ports to open (first reference) and close (last reference gone).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ExtraPortPlan {
    pub(crate) open: Vec<Ipv4Addr>,
    pub(crate) close: Vec<Ipv4Addr>,
}

impl ExtraPortLeases {
    /// Replace one lease's peer set.
    pub(crate) fn replace(&mut self, lease: u64, peers: &[Ipv4Addr]) -> ExtraPortPlan {
        let mut wanted = peers.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let previous = self.leases.remove(&lease).unwrap_or_default();
        let mut plan = ExtraPortPlan::default();
        for peer in previous.iter().filter(|peer| !wanted.contains(peer)) {
            if self.decrement(*peer) {
                plan.close.push(*peer);
            }
        }
        for peer in wanted.iter().filter(|peer| !previous.contains(peer)) {
            let count = self.refs.entry(*peer).or_insert(0);
            *count += 1;
            if *count == 1 {
                plan.open.push(*peer);
            }
        }
        if !wanted.is_empty() {
            self.leases.insert(lease, wanted);
        }
        plan
    }

    /// The lease ended: ports nobody else references.
    pub(crate) fn release(&mut self, lease: u64) -> Vec<Ipv4Addr> {
        let peers = self.leases.remove(&lease).unwrap_or_default();
        peers
            .into_iter()
            .filter(|peer| self.decrement(*peer))
            .collect()
    }

    /// Opening `peer` failed: forget the lease's reference so a later
    /// update (from any lease) retries it.
    pub(crate) fn open_failed(&mut self, lease: u64, peer: Ipv4Addr) {
        if let Some(peers) = self.leases.get_mut(&lease) {
            peers.retain(|existing| *existing != peer);
        }
        // Every lease that referenced it in the same step expected it open.
        self.refs.remove(&peer);
        for peers in self.leases.values_mut() {
            peers.retain(|existing| *existing != peer);
        }
    }

    pub(crate) fn open_ports(&self) -> Vec<Ipv4Addr> {
        let mut ports: Vec<Ipv4Addr> = self.refs.keys().copied().collect();
        ports.sort_unstable();
        ports
    }

    fn decrement(&mut self, peer: Ipv4Addr) -> bool {
        let Some(count) = self.refs.get_mut(&peer) else {
            return false;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.refs.remove(&peer);
            true
        } else {
            false
        }
    }
}

/// Single-writer seqlock holding the latest [`TsLine`].
#[derive(Debug, Default)]
pub(crate) struct LineCell {
    sequence: AtomicU64,
    fields: [AtomicU64; 8],
}

impl LineCell {
    /// Only the TimeSync thread writes.
    pub(crate) fn publish(&self, line: &TsLine) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence.store(sequence.wrapping_add(1), Ordering::SeqCst);
        let values = [
            line.domain_anchor_ns,
            line.mach_anchor,
            line.mach_per_domain_second,
            u64::from(line.lock_state as u32),
            line.grandmaster,
            line.self_identity,
            u64::from(line.port_role as u32),
            line.sampled_at_ns,
        ];
        for (field, value) in self.fields.iter().zip(values) {
            field.store(value, Ordering::SeqCst);
        }
        self.sequence.store(sequence.wrapping_add(2), Ordering::SeqCst);
    }

    /// `None` before the first publication or after three torn reads.
    pub(crate) fn read(&self) -> Option<TsLine> {
        for _ in 0..3 {
            let before = self.sequence.load(Ordering::SeqCst);
            if before == 0 || before & 1 != 0 {
                if before == 0 {
                    return None;
                }
                std::hint::spin_loop();
                continue;
            }
            let values: [u64; 8] = std::array::from_fn(|i| self.fields[i].load(Ordering::SeqCst));
            if self.sequence.load(Ordering::SeqCst) == before {
                return Some(TsLine {
                    domain_anchor_ns: values[0],
                    mach_anchor: values[1],
                    mach_per_domain_second: values[2],
                    lock_state: values[3] as u32 as i32,
                    grandmaster: values[4],
                    self_identity: values[5],
                    port_role: values[6] as u32 as i32,
                    sampled_at_ns: values[7],
                });
            }
        }
        None
    }
}

// ---------------------------------------------------------------- breaker

/// Process-wide circuit breaker.
///
/// One sender whose instance never locks says more about that sender (for
/// example an unusual network path) than about TimeSync, so its failure only
/// fails its own sessions; while no instance has ever mapped, later sessions
/// from that peer use the system clock instead of failing again. Lock
/// failures from [`BREAKER_DISTINCT_PEERS`] distinct peer addresses while no
/// instance session has ever mapped a time mean the backend is broken on
/// this system: the breaker trips and every later session uses the system
/// clock until the runtime restarts. After one success, failures are only
/// counted.
#[derive(Debug, Default)]
pub(crate) struct Breaker {
    ever_mapped: AtomicBool,
    tripped: AtomicBool,
    failed_peers: Mutex<Vec<Ipv4Addr>>,
}

impl Breaker {
    pub(crate) const fn new() -> Self {
        Self {
            ever_mapped: AtomicBool::new(false),
            tripped: AtomicBool::new(false),
            failed_peers: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn note_mapped(&self) {
        if !self.ever_mapped.load(Ordering::Relaxed) {
            self.ever_mapped.store(true, Ordering::Relaxed);
        }
    }

    /// Record one instance lock-watchdog failure for `peer`. Returns true
    /// when this failure trips the breaker.
    pub(crate) fn note_watchdog_failure(&self, peer: Ipv4Addr) -> bool {
        let distinct = {
            let mut peers = self
                .failed_peers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !peers.contains(&peer) && peers.len() < MAX_FAILED_PEERS {
                peers.push(peer);
            }
            peers.len()
        };
        distinct >= BREAKER_DISTINCT_PEERS
            && !self.ever_mapped.load(Ordering::Relaxed)
            && !self.tripped.swap(true, Ordering::AcqRel)
    }

    /// Whether a new session from `peer` should skip the instance backend
    /// because that peer's instance already failed to lock and no instance
    /// has ever mapped in this process.
    pub(crate) fn peer_failed(&self, peer: Ipv4Addr) -> bool {
        !self.ever_mapped.load(Ordering::Relaxed)
            && self
                .failed_peers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&peer)
    }

    pub(crate) fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }
}

pub(crate) static BREAKER: Breaker = Breaker::new();
pub(crate) static LOCK_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static GM_LOST: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------- watchdog

/// Media demand bookkeeping shared by every session using one instance.
#[derive(Debug, Default)]
pub(crate) struct Demand {
    last_ns: AtomicU64,
    since_ns: AtomicU64,
    last_remote_ns: AtomicU64,
}

impl Demand {
    /// Called on every map request. `now_ns` must be nonzero.
    pub(crate) fn note(&self, now_ns: u64, remote_ns: u64) {
        let previous = self.last_ns.swap(now_ns, Ordering::Relaxed);
        if previous == 0 || now_ns.saturating_sub(previous) > DEMAND_IDLE.as_nanos() as u64 {
            self.since_ns.store(now_ns, Ordering::Relaxed);
        }
        self.last_remote_ns.store(remote_ns, Ordering::Relaxed);
    }

    pub(crate) fn last_remote_ns(&self) -> Option<u64> {
        let value = self.last_remote_ns.load(Ordering::Relaxed);
        (value != 0).then_some(value)
    }

    /// The lock watchdog rule: media has been asking continuously and no
    /// valid line has existed for [`LOCK_WATCHDOG`] since it started asking.
    pub(crate) fn watchdog_expired(&self, now_ns: u64, last_valid_ns: Option<u64>) -> bool {
        let last = self.last_ns.load(Ordering::Relaxed);
        if last == 0 || now_ns.saturating_sub(last) > DEMAND_IDLE.as_nanos() as u64 {
            return false;
        }
        let since = self.since_ns.load(Ordering::Relaxed);
        let reference = since.max(last_valid_ns.unwrap_or(0));
        now_ns.saturating_sub(reference) > LOCK_WATCHDOG.as_nanos() as u64
    }
}

// ---------------------------------------------------------------- test hooks

/// Fault injection for the regression suite. Present only with the
/// test-only `airplay-test-hooks` Cargo feature; release builds carry
/// neither the code nor the environment variable names.
#[allow(dead_code)] // Constructed only with the `airplay-test-hooks` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    MissingClass,
    BadEncoding,
    CreateFail,
    PortFail,
    NeverLock,
}

#[cfg(feature = "airplay-test-hooks")]
pub(crate) mod test_hooks {
    use super::Fault;
    use std::sync::OnceLock;

    pub(crate) fn forced_system() -> bool {
        static VALUE: OnceLock<bool> = OnceLock::new();
        *VALUE.get_or_init(|| {
            std::env::var("AUDIOHUB_TEST_AIRPLAY_PTP_BACKEND").as_deref() == Ok("system")
        })
    }

    pub(crate) fn fault() -> Option<Fault> {
        static VALUE: OnceLock<Option<Fault>> = OnceLock::new();
        *VALUE.get_or_init(|| {
            match std::env::var("AUDIOHUB_TEST_TIMESYNC_FAULT").as_deref() {
                Ok("missing_class") => Some(Fault::MissingClass),
                Ok("bad_encoding") => Some(Fault::BadEncoding),
                Ok("create_fail") => Some(Fault::CreateFail),
                Ok("port_fail") => Some(Fault::PortFail),
                Ok("never_lock") => Some(Fault::NeverLock),
                _ => None,
            }
        })
    }
}

#[cfg(not(feature = "airplay-test-hooks"))]
pub(crate) mod test_hooks {
    use super::Fault;

    pub(crate) fn forced_system() -> bool {
        false
    }

    pub(crate) fn fault() -> Option<Fault> {
        None
    }
}

// ---------------------------------------------------------------- macOS service

#[cfg(target_os = "macos")]
pub(crate) use service::{acquire, availability, probed_availability, Availability, TimeSyncPort};

#[cfg(target_os = "macos")]
mod service {
    use super::*;
    use crate::protocol::media::ptp::{PeerPortUpdate, PtpRemoteMatch};
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{msg_send, sel};
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::net::Ipv4Addr;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc;
    use std::sync::{Arc, OnceLock};
    use std::time::Instant;
    use tokio::sync::{oneshot, watch};

    const TIMESYNC_PATH: &[u8] = b"/System/Library/PrivateFrameworks/TimeSync.framework/TimeSync\0";

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    /// One (mach, Instant) pair sampled at first use. Rust's `Instant` on
    /// macOS is the same uptime clock as mach_absolute_time, so the offset
    /// between the two scales is constant.
    struct ClockBase {
        instant: Instant,
        mach: u64,
        timebase: MachTimebase,
    }

    fn clock_base() -> &'static ClockBase {
        static BASE: OnceLock<ClockBase> = OnceLock::new();
        BASE.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            // SAFETY: plain out-parameter call into libSystem.
            let status = unsafe { mach_timebase_info(&mut info) };
            let timebase = (status == 0)
                .then(|| MachTimebase::new(info.numer, info.denom))
                .flatten()
                .unwrap_or(MachTimebase { numer: 1, denom: 1 });
            let before = Instant::now();
            // SAFETY: no arguments; reads the monotonic tick counter.
            let mach = unsafe { mach_absolute_time() };
            let after = Instant::now();
            ClockBase {
                instant: before + after.duration_since(before) / 2,
                mach,
                timebase,
            }
        })
    }

    impl ClockBase {
        /// Nonzero process-monotonic ns.
        fn now_ns(&self) -> u64 {
            (self.instant.elapsed().as_nanos() as u64).max(1)
        }

        fn instant_from_mach(&self, mach: u64) -> Option<Instant> {
            let delta_ns = self
                .timebase
                .ticks_to_ns(i128::from(mach) - i128::from(self.mach));
            let magnitude = Duration::from_nanos(u64::try_from(delta_ns.unsigned_abs()).ok()?);
            if delta_ns >= 0 {
                self.instant.checked_add(magnitude)
            } else {
                self.instant.checked_sub(magnitude)
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Availability {
        Available { reverse_convert: bool },
        Unavailable(&'static str),
    }

    impl Availability {
        pub(crate) fn is_available(self) -> bool {
            matches!(self, Self::Available { .. })
        }
    }

    /// State shared between the TimeSync thread (sole writer) and every
    /// session mapping through one peer instance.
    pub(crate) struct PortShared {
        peer: Ipv4Addr,
        entry_id: u64,
        clock_identity: u64,
        port_number: u16,
        line: LineCell,
        /// Last healthy line on the previous grandmaster, published when the
        /// instance follows a new one (planned GM change).
        retired: LineCell,
        demand: Demand,
        failed: watch::Sender<bool>,
        /// Receive-matching override: 0 not requested, 1 accepted, 2 refused.
        #[cfg_attr(not(test), allow(dead_code))]
        receive_matching: std::sync::atomic::AtomicU8,
    }

    /// One session's lease on a peer instance. Dropping it releases the
    /// lease; the instance is removed when the last lease for the peer ends.
    pub(crate) struct TimeSyncPort {
        shared: Arc<PortShared>,
        /// This session's lease on the instance (keys its SETPEERS ports).
        lease: u64,
        commands: mpsc::Sender<Command>,
    }

    impl TimeSyncPort {
        pub(crate) fn clock_identity(&self) -> u64 {
            self.shared.clock_identity
        }

        pub(crate) fn port_number(&self) -> u16 {
            self.shared.port_number
        }

        pub(crate) fn failed(&self) -> watch::Receiver<bool> {
            self.shared.failed.subscribe()
        }

        /// Lock-free media-path mapping; never sends an ObjC message.
        pub(crate) fn map_remote_time(
            &self,
            remote_ns: u64,
            expected_grandmaster: u64,
        ) -> Result<Instant, PtpClockError> {
            let base = clock_base();
            let now_ns = base.now_ns();
            self.shared.demand.note(now_ns, remote_ns);
            let line = self.shared.line.read().ok_or(PtpClockError::NotReady)?;
            let mach = line.map_to_mach(now_ns, remote_ns, expected_grandmaster)?;
            let instant = base
                .instant_from_mach(mach)
                .ok_or(PtpClockError::MappedTimeOutOfRange)?;
            BREAKER.note_mapped();
            Ok(instant)
        }

        /// Grandmaster of the instance's latest line, for diagnostics.
        pub(crate) fn grandmaster(&self) -> Option<u64> {
            self.shared
                .line
                .read()
                .map(|line| line.grandmaster)
                .filter(|grandmaster| *grandmaster != 0)
        }

        /// Carry an anchor on `from` to the grandmaster the instance follows
        /// now. Lock-free; never sends an ObjC message.
        pub(crate) fn handoff(&self, from: u64) -> Result<PlatformHandoff, &'static str> {
            let base = clock_base();
            let now_ns = base.now_ns();
            // SAFETY: no arguments; reads the tick counter.
            let mach_now = unsafe { mach_absolute_time() };
            handoff_between_lines(
                self.shared.retired.read(),
                self.shared.line.read(),
                from,
                now_ns,
                mach_now,
            )
        }

        /// Replace this session's SETPEERS timing ports on the instance.
        pub(crate) async fn set_peers(&self, peers: Vec<Ipv4Addr>) -> PeerPortUpdate {
            let failed = |peers: &[Ipv4Addr], reason| PeerPortUpdate {
                failed: peers.iter().map(|peer| (*peer, reason)).collect(),
                ..PeerPortUpdate::default()
            };
            let (reply, result) = oneshot::channel();
            if self
                .commands
                .send(Command::SetPeers {
                    peer: self.shared.peer,
                    entry_id: self.shared.entry_id,
                    lease: self.lease,
                    peers: peers.clone(),
                    reply,
                })
                .is_err()
            {
                return failed(&peers, "service_thread_failed");
            }
            match tokio::time::timeout(PROBE_TIMEOUT, result).await {
                Ok(Ok(update)) => update,
                Ok(Err(_)) => failed(&peers, "service_thread_failed"),
                Err(_) => failed(&peers, "service_timeout"),
            }
        }

        /// Whether the sender's sourcePortIdentity override was accepted;
        /// `None` when no override was requested.
        #[cfg(test)]
        pub(crate) fn receive_matching_overridden(&self) -> Option<bool> {
            match self.shared.receive_matching.load(Ordering::Relaxed) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            }
        }

        #[cfg(test)]
        pub(crate) fn line(&self) -> Option<TsLine> {
            self.shared.line.read()
        }

        /// Process-monotonic "now" on the scale used by [`TsLine::sampled_at_ns`].
        #[cfg(test)]
        pub(crate) fn now_ns() -> u64 {
            clock_base().now_ns()
        }

        /// Ask the TimeSync thread for a direct conversion (test-only, used to
        /// compare against line extrapolation). Returns `(mach, gm_used)`.
        #[cfg(test)]
        pub(crate) fn direct_convert(&self, remote_ns: u64) -> Option<(u64, u64)> {
            let (reply, result) = mpsc::sync_channel(1);
            self.commands
                .send(Command::DirectConvert {
                    peer: self.shared.peer,
                    remote_ns,
                    reply,
                })
                .ok()?;
            result.recv_timeout(Duration::from_secs(2)).ok().flatten()
        }

        #[cfg(test)]
        pub(crate) fn instant_from_mach(mach: u64) -> Option<Instant> {
            clock_base().instant_from_mach(mach)
        }
    }

    impl Drop for TimeSyncPort {
        fn drop(&mut self) {
            let _ = self.commands.send(Command::Release {
                peer: self.shared.peer,
                entry_id: self.shared.entry_id,
                lease: self.lease,
            });
        }
    }

    pub(crate) enum Command {
        Probe(mpsc::SyncSender<Availability>),
        Acquire {
            local: Ipv4Addr,
            peer: Ipv4Addr,
            remote: Option<PtpRemoteMatch>,
            reply: oneshot::Sender<Result<(Arc<PortShared>, u64), &'static str>>,
        },
        Release {
            peer: Ipv4Addr,
            entry_id: u64,
            lease: u64,
        },
        SetPeers {
            peer: Ipv4Addr,
            entry_id: u64,
            lease: u64,
            peers: Vec<Ipv4Addr>,
            reply: oneshot::Sender<PeerPortUpdate>,
        },
        #[cfg(test)]
        DirectConvert {
            peer: Ipv4Addr,
            remote_ns: u64,
            reply: mpsc::SyncSender<Option<(u64, u64)>>,
        },
    }

    fn commands() -> Option<&'static mpsc::Sender<Command>> {
        static SERVICE: OnceLock<Option<mpsc::Sender<Command>>> = OnceLock::new();
        SERVICE
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel();
                match std::thread::Builder::new()
                    .name("ahb-airplay-timesync".into())
                    .spawn(move || ServiceThread::new().run(rx))
                {
                    Ok(_) => Some(tx),
                    Err(error) => {
                        log::warn!("TimeSync service thread could not start: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }

    static AVAILABILITY: OnceLock<Availability> = OnceLock::new();

    /// Probe once (blocking, at most [`PROBE_TIMEOUT`] plus slack) and cache.
    /// Call from a blocking context.
    pub(crate) fn availability() -> Availability {
        *AVAILABILITY.get_or_init(|| {
            let Some(commands) = commands() else {
                return Availability::Unavailable("service_thread_failed");
            };
            let (reply, result) = mpsc::sync_channel(1);
            if commands.send(Command::Probe(reply)).is_err() {
                return Availability::Unavailable("service_thread_failed");
            }
            let availability = result
                .recv_timeout(PROBE_TIMEOUT + Duration::from_secs(1))
                .unwrap_or(Availability::Unavailable("probe_timeout"));
            match availability {
                Availability::Available { reverse_convert } => log::info!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync per-sender PTP backend available reverse_convert={reverse_convert}"
                ),
                Availability::Unavailable(reason) => log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync per-sender PTP backend unavailable reason={reason}"
                ),
            }
            availability
        })
    }

    /// The cached probe result, without probing.
    pub(crate) fn probed_availability() -> Option<Availability> {
        AVAILABILITY.get().copied()
    }

    /// Create or share the peer's instance. Requires a prior successful
    /// [`availability`].
    pub(crate) async fn acquire(
        local: Ipv4Addr,
        peer: Ipv4Addr,
        remote: Option<PtpRemoteMatch>,
    ) -> Result<TimeSyncPort, &'static str> {
        let commands = commands().ok_or("service_thread_failed")?;
        let (reply, result) = oneshot::channel();
        commands
            .send(Command::Acquire {
                local,
                peer,
                remote,
                reply,
            })
            .map_err(|_| "service_thread_failed")?;
        let (shared, lease) = tokio::time::timeout(PROBE_TIMEOUT, result)
            .await
            .map_err(|_| "acquire_timeout")?
            .map_err(|_| "service_thread_failed")??;
        Ok(TimeSyncPort {
            shared,
            lease,
            commands: commands.clone(),
        })
    }

    struct ObjcContext {
        manager: Retained<AnyObject>,
        clock_manager: Retained<AnyObject>,
        reverse_convert: bool,
    }

    struct PeerEntry {
        refs: usize,
        instance_id: u64,
        clock: Retained<AnyObject>,
        interface: CFString,
        destination: u32,
        shared: Arc<PortShared>,
        last_valid_ns: Option<u64>,
        healthy: bool,
        failed: bool,
        /// Last healthy line, to detect a grandmaster change.
        last_healthy: Option<TsLine>,
        /// SETPEERS ports opened on this instance, counted per lease.
        extra: ExtraPortLeases,
    }

    struct ServiceThread {
        objc: Option<ObjcContext>,
        probe: Option<Availability>,
        entries: HashMap<Ipv4Addr, PeerEntry>,
        next_entry_id: u64,
        next_lease: u64,
        poisoned: bool,
    }

    fn class_named(name: &str) -> Option<&'static AnyClass> {
        let name = CString::new(name).ok()?;
        AnyClass::get(&name)
    }

    fn selector(name: &str) -> Sel {
        let name = CString::new(name).expect("selector names contain no NUL");
        Sel::register(&name)
    }

    /// Short, secret-free description of an NSError out-parameter.
    fn error_code(error: *mut AnyObject) -> isize {
        if error.is_null() {
            return 0;
        }
        // SAFETY: a non-null NSError out-parameter is a valid autoreleased
        // object for the duration of the current pool.
        unsafe { msg_send![&*error, code] }
    }

    fn ns_string(value: &CFString) -> &AnyObject {
        // SAFETY: CFString is toll-free bridged with NSString and outlives the
        // returned borrow.
        unsafe { &*(value.as_concrete_TypeRef() as *const AnyObject) }
    }

    impl ServiceThread {
        fn new() -> Self {
            Self {
                objc: None,
                probe: None,
                entries: HashMap::new(),
                next_entry_id: 1,
                next_lease: 1,
                poisoned: false,
            }
        }

        fn run(mut self, commands: mpsc::Receiver<Command>) {
            let mut next_tick = Instant::now();
            loop {
                let command = if self.entries.is_empty() {
                    match commands.recv() {
                        Ok(command) => Some(command),
                        Err(_) => return,
                    }
                } else {
                    match commands.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                if let Some(command) = command {
                    let became_busy = self.entries.is_empty();
                    self.guarded(|this| this.handle(command));
                    if became_busy && !self.entries.is_empty() {
                        next_tick = Instant::now() + LINE_REFRESH;
                    }
                }
                if !self.entries.is_empty() && Instant::now() >= next_tick {
                    self.guarded(Self::tick);
                    next_tick = Instant::now() + LINE_REFRESH;
                }
            }
        }

        /// Run inside an autorelease pool. A Rust panic in ObjC glue must not
        /// silently stall every session: fail them so their connections close.
        fn guarded(&mut self, work: impl FnOnce(&mut Self)) {
            let result = catch_unwind(AssertUnwindSafe(|| autoreleasepool(|_| work(self))));
            if result.is_err() && !self.poisoned {
                self.poisoned = true;
                log::error!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync service panicked; failing every per-sender PTP port"
                );
                for entry in self.entries.values_mut() {
                    entry.failed = true;
                    let _ = entry.shared.failed.send(true);
                }
            }
        }

        fn handle(&mut self, command: Command) {
            match command {
                Command::Probe(reply) => {
                    let availability = match self.probe {
                        Some(value) => value,
                        None => {
                            let value = self.probe_once();
                            self.probe = Some(value);
                            value
                        }
                    };
                    let _ = reply.send(availability);
                }
                Command::Acquire {
                    local,
                    peer,
                    remote,
                    reply,
                } => {
                    let result = self.acquire(local, peer, remote).map(|shared| {
                        let lease = self.next_lease;
                        self.next_lease += 1;
                        (shared, lease)
                    });
                    let lease = result
                        .as_ref()
                        .ok()
                        .map(|(shared, lease)| (shared.entry_id, *lease));
                    if reply.send(result).is_err() {
                        // The caller timed out; return the lease it never got.
                        if let Some((entry_id, lease)) = lease {
                            self.release(peer, entry_id, lease);
                        }
                    }
                }
                Command::Release {
                    peer,
                    entry_id,
                    lease,
                } => self.release(peer, entry_id, lease),
                Command::SetPeers {
                    peer,
                    entry_id,
                    lease,
                    peers,
                    reply,
                } => {
                    let update = self.set_peers(peer, entry_id, lease, peers);
                    let _ = reply.send(update);
                }
                #[cfg(test)]
                Command::DirectConvert {
                    peer,
                    remote_ns,
                    reply,
                } => {
                    let value = self
                        .entries
                        .get(&peer)
                        .and_then(|entry| convert_to_mach(&entry.clock, remote_ns));
                    let _ = reply.send(value);
                }
            }
        }

        fn probe_once(&mut self) -> Availability {
            if self.poisoned {
                return Availability::Unavailable("service_panicked");
            }
            // SAFETY: static NUL-terminated path; the handle is intentionally
            // never closed because ObjC images cannot be unloaded.
            let handle = unsafe {
                libc::dlopen(
                    TIMESYNC_PATH.as_ptr().cast(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            if handle.is_null() {
                return Availability::Unavailable("dlopen_failed");
            }
            let fault = test_hooks::fault();
            if fault == Some(Fault::MissingClass) {
                return Availability::Unavailable("missing_class");
            }
            for class in REQUIRED_CLASSES {
                if class_named(class).is_none() {
                    return Availability::Unavailable("missing_class");
                }
            }
            let mut reverse_convert = true;
            for (index, required) in REQUIRED_SELECTORS.iter().enumerate() {
                let class = class_named(required.class).expect("class checked above");
                let sel = selector(required.selector);
                let method = if required.class_method {
                    class.class_method(sel)
                } else {
                    class.instance_method(sel)
                };
                let encoding = method.and_then(|method| method_encoding(method));
                let encoding = match (fault, index) {
                    (Some(Fault::BadEncoding), 2) => encoding.map(|_| "v32@0:8^Q16^@24".to_owned()),
                    _ => encoding,
                };
                let ok = encoding
                    .as_deref()
                    .is_some_and(|encoding| encoding_matches(encoding, required.skeleton));
                if !ok {
                    if index == OPTIONAL_REVERSE_CONVERT {
                        reverse_convert = false;
                        continue;
                    }
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync selector check failed class={} selector={} encoding={:?}",
                        required.class,
                        required.selector,
                        encoding
                    );
                    return Availability::Unavailable(if encoding.is_none() {
                        "missing_selector"
                    } else {
                        "bad_encoding"
                    });
                }
            }
            let manager_class = class_named("TSgPTPManager").expect("class checked above");
            let clock_manager_class =
                class_named("TSClockManager").expect("class checked above");
            let deadline = Instant::now() + PROBE_TIMEOUT;
            let manager = loop {
                // SAFETY: selector and +0 object return verified above.
                let manager: Option<Retained<AnyObject>> =
                    unsafe { msg_send![manager_class, sharedgPTPManager] };
                if let Some(manager) = manager {
                    break manager;
                }
                if Instant::now() >= deadline {
                    return Availability::Unavailable("manager_unavailable");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            // SAFETY: selector and +0 object return verified above.
            let clock_manager: Option<Retained<AnyObject>> =
                unsafe { msg_send![clock_manager_class, sharedClockManager] };
            let Some(clock_manager) = clock_manager else {
                return Availability::Unavailable("clock_manager_unavailable");
            };
            self.objc = Some(ObjcContext {
                manager,
                clock_manager,
                reverse_convert,
            });
            Availability::Available { reverse_convert }
        }

        fn acquire(
            &mut self,
            local: Ipv4Addr,
            peer: Ipv4Addr,
            remote: Option<PtpRemoteMatch>,
        ) -> Result<Arc<PortShared>, &'static str> {
            if self.poisoned {
                return Err("service_panicked");
            }
            let Some(objc) = self.objc.as_ref() else {
                return Err("unavailable");
            };
            if let Some(entry) = self.entries.get_mut(&peer) {
                if entry.failed {
                    return Err("peer_instance_failed");
                }
                entry.refs += 1;
                return Ok(Arc::clone(&entry.shared));
            }
            let fault = test_hooks::fault();
            if fault == Some(Fault::CreateFail) {
                return Err("create_failed");
            }
            let mut instance_id = 0u64;
            let mut error: *mut AnyObject = std::ptr::null_mut();
            // SAFETY: encoding `c@:^Q^@` verified by the probe; both out
            // pointers are valid for the call.
            let created: Bool = unsafe {
                msg_send![&*objc.manager, addPTPInstance: &mut instance_id as *mut u64, error: &mut error as *mut *mut AnyObject]
            };
            if !created.as_bool() || instance_id == 0 {
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync addPTPInstance failed peer={peer} code={}",
                    error_code(error)
                );
                return Err("create_failed");
            }
            let remove_instance = |objc: &ObjcContext| {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: encoding `c@:Q^@` verified by the probe.
                let removed: Bool = unsafe {
                    msg_send![&*objc.manager, removePTPInstanceWithIdentifier: instance_id, error: &mut error as *mut *mut AnyObject]
                };
                if !removed.as_bool() {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync removePTPInstance failed code={}",
                        error_code(error)
                    );
                }
            };
            // SAFETY: encoding `@@:Q` verified by the probe.
            let clock: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*objc.clock_manager, clockWithClockIdentifier: instance_id] };
            let Some(clock) = clock else {
                remove_instance(objc);
                return Err("clock_missing");
            };
            // SAFETY: encoding `Q@:` verified by the probe.
            let clock_identity: u64 = unsafe { msg_send![&*clock, clockIdentity] };
            let interface = match super::super::ptp_macos::interface_name_for(local) {
                Ok(name) => CFString::new(&name),
                Err(error) => {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync port interface lookup failed local={local}: {error}"
                    );
                    remove_instance(objc);
                    return Err("interface_missing");
                }
            };
            let destination = u32::from_be_bytes(peer.octets());
            let mut port_number = 0u16;
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let port_added = fault != Some(Fault::PortFail) && {
                // SAFETY: encoding `c@:@I^S^@` verified by the probe; the
                // interface string outlives the call.
                let added: Bool = unsafe {
                    msg_send![&*clock, addUnicastUDPv4EtEPortOnInterfaceNamed: ns_string(&interface), withDestinationAddress: destination, allocatedPortNumber: &mut port_number as *mut u16, error: &mut error as *mut *mut AnyObject]
                };
                added.as_bool()
            };
            if !port_added {
                log::warn!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync addUnicastUDPv4EtEPort failed peer={peer} code={}",
                    error_code(error)
                );
                remove_instance(objc);
                return Err("port_failed");
            }
            let remove_port = |clock: &AnyObject| {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: encoding `c@:@I^@` verified by the probe.
                let removed: Bool = unsafe {
                    msg_send![clock, removeUnicastUDPv4EtEPortFromInterfaceNamed: ns_string(&interface), withDestinationAddress: destination, error: &mut error as *mut *mut AnyObject]
                };
                if !removed.as_bool() {
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync removeUnicastUDPv4EtEPort failed code={}",
                        error_code(error)
                    );
                }
            };
            let Some(port) = find_port(&clock, port_number) else {
                remove_port(&clock);
                remove_instance(objc);
                return Err("port_missing");
            };
            // SAFETY: encoding `c@:` verified by the probe.
            let enabled: Bool = unsafe { msg_send![&*port, enabled] };
            if !enabled.as_bool() {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: encoding `c@:^@` verified by the probe. A NO result
                // is not fatal: the experiment saw NO with enabled == 1.
                let result: Bool =
                    unsafe { msg_send![&*port, enablePortError: &mut error as *mut *mut AnyObject] };
                // SAFETY: as above.
                let now_enabled: Bool = unsafe { msg_send![&*port, enabled] };
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync enablePort peer={peer} result={} enabled={} code={}",
                    result.as_bool(),
                    now_enabled.as_bool(),
                    error_code(error)
                );
            }
            let mut receive_matching = 0u8;
            if let Some(remote) = remote {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: encoding `c@:QS^@` verified by the probe.
                let overridden: Bool = unsafe {
                    msg_send![&*port, overrideReceiveMatchingWithRemoteClockIdentity: remote.clock_identity, remotePortNumber: remote.port_number, error: &mut error as *mut *mut AnyObject]
                };
                receive_matching = if overridden.as_bool() { 1 } else { 2 };
                if !overridden.as_bool() {
                    // Not fatal (design §2.4 step 4): default matching works.
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync receive-matching override failed peer={peer} code={}",
                        error_code(error)
                    );
                }
            }
            let entry_id = self.next_entry_id;
            self.next_entry_id += 1;
            let shared = Arc::new(PortShared {
                peer,
                entry_id,
                clock_identity,
                port_number,
                line: LineCell::default(),
                retired: LineCell::default(),
                demand: Demand::default(),
                failed: watch::channel(false).0,
                receive_matching: std::sync::atomic::AtomicU8::new(receive_matching),
            });
            let mut entry = PeerEntry {
                refs: 1,
                instance_id,
                clock,
                interface,
                destination,
                shared: Arc::clone(&shared),
                last_valid_ns: None,
                healthy: false,
                failed: false,
                last_healthy: None,
                extra: ExtraPortLeases::default(),
            };
            let reverse = objc.reverse_convert;
            refresh_entry(&mut entry, reverse, clock_base().now_ns());
            log::info!(
                target: "audiohub_airplay::airplay2",
                "TimeSync instance ready peer={peer} instance={instance_id:016x} clock_identity={clock_identity:016x} port={port_number}"
            );
            self.entries.insert(peer, entry);
            Ok(shared)
        }

        fn release(&mut self, peer: Ipv4Addr, entry_id: u64, lease: u64) {
            let Some(entry) = self.entries.get_mut(&peer) else {
                return;
            };
            if entry.shared.entry_id != entry_id {
                return;
            }
            for address in entry.extra.release(lease) {
                remove_unicast_port(entry, address);
            }
            entry.refs = entry.refs.saturating_sub(1);
            if entry.refs > 0 {
                return;
            }
            let entry = self.entries.remove(&peer).expect("entry exists");
            let Some(objc) = self.objc.as_ref() else {
                return;
            };
            let mut error: *mut AnyObject = std::ptr::null_mut();
            // SAFETY: encoding `c@:@I^@` verified by the probe.
            let removed: Bool = unsafe {
                msg_send![&*entry.clock, removeUnicastUDPv4EtEPortFromInterfaceNamed: ns_string(&entry.interface), withDestinationAddress: entry.destination, error: &mut error as *mut *mut AnyObject]
            };
            let port_code = error_code(error);
            let mut error: *mut AnyObject = std::ptr::null_mut();
            // SAFETY: encoding `c@:Q^@` verified by the probe.
            let instance_removed: Bool = unsafe {
                msg_send![&*objc.manager, removePTPInstanceWithIdentifier: entry.instance_id, error: &mut error as *mut *mut AnyObject]
            };
            log::info!(
                target: "audiohub_airplay::airplay2",
                "TimeSync instance released peer={peer} port_removed={} (code {port_code}) instance_removed={} (code {})",
                removed.as_bool(),
                instance_removed.as_bool(),
                error_code(error)
            );
        }

        /// Replace one lease's SETPEERS ports on its peer's instance.
        fn set_peers(
            &mut self,
            peer: Ipv4Addr,
            entry_id: u64,
            lease: u64,
            peers: Vec<Ipv4Addr>,
        ) -> PeerPortUpdate {
            let mut update = PeerPortUpdate::default();
            let Some(entry) = self
                .entries
                .get_mut(&peer)
                .filter(|entry| entry.shared.entry_id == entry_id && !self.poisoned)
            else {
                update.failed = peers
                    .iter()
                    .map(|address| (*address, "instance_missing"))
                    .collect();
                return update;
            };
            // The sender itself already has the instance's main port.
            let wanted: Vec<Ipv4Addr> = peers
                .into_iter()
                .filter(|address| *address != peer)
                .collect();
            let plan = entry.extra.replace(lease, &wanted);
            for address in plan.close {
                remove_unicast_port(entry, address);
                update.closed.push(address);
            }
            for address in plan.open {
                match add_unicast_port(entry, address) {
                    Ok(port_number) => {
                        log::info!(
                            target: "audiohub_airplay::airplay2",
                            "TimeSync instance for {peer} added timing peer {address} port={port_number}"
                        );
                        update.opened.push(address);
                    }
                    Err(code) => {
                        log::warn!(
                            target: "audiohub_airplay::airplay2",
                            "TimeSync instance for {peer} could not add timing peer {address} code={code}"
                        );
                        entry.extra.open_failed(lease, address);
                        update.failed.push((address, "port_failed"));
                    }
                }
            }
            update
        }

        fn tick(&mut self) {
            let reverse = self.objc.as_ref().is_some_and(|objc| objc.reverse_convert);
            let now_ns = clock_base().now_ns();
            for entry in self.entries.values_mut() {
                refresh_entry(entry, reverse, now_ns);
                if entry.failed {
                    continue;
                }
                if entry
                    .shared
                    .demand
                    .watchdog_expired(now_ns, entry.last_valid_ns)
                {
                    entry.failed = true;
                    let _ = entry.shared.failed.send(true);
                    LOCK_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    crate::telemetry::record_ptp_instance_lock_timeout(
                        crate::telemetry::operation_token(),
                    );
                    log::warn!(
                        target: "audiohub_airplay::airplay2",
                        "TimeSync instance for {} produced no valid lock within {:?}; failing its sessions",
                        entry.shared.peer,
                        LOCK_WATCHDOG
                    );
                    if BREAKER.note_watchdog_failure(entry.shared.peer) {
                        crate::telemetry::record_ptp_backend_tripped();
                        log::warn!(
                            target: "audiohub_airplay::airplay2",
                            "TimeSync per-sender backend tripped: instances for {BREAKER_DISTINCT_PEERS} distinct senders failed to lock and none has ever mapped; later sessions use the system clock"
                        );
                    }
                }
            }
        }
    }

    /// Open one SETPEERS unicast port on the entry's instance; returns the
    /// allocated port number or the NSError code.
    fn add_unicast_port(entry: &PeerEntry, address: Ipv4Addr) -> Result<u16, isize> {
        let destination = u32::from_be_bytes(address.octets());
        let mut port_number = 0u16;
        let mut error: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: encoding `c@:@I^S^@` verified by the probe; the interface
        // string outlives the call.
        let added: Bool = unsafe {
            msg_send![&*entry.clock, addUnicastUDPv4EtEPortOnInterfaceNamed: ns_string(&entry.interface), withDestinationAddress: destination, allocatedPortNumber: &mut port_number as *mut u16, error: &mut error as *mut *mut AnyObject]
        };
        if !added.as_bool() {
            return Err(error_code(error));
        }
        if let Some(port) = find_port(&entry.clock, port_number) {
            // SAFETY: encoding `c@:` verified by the probe.
            let enabled: Bool = unsafe { msg_send![&*port, enabled] };
            if !enabled.as_bool() {
                let mut error: *mut AnyObject = std::ptr::null_mut();
                // SAFETY: encoding `c@:^@` verified by the probe; NO is not
                // fatal (see `acquire`).
                let _: Bool = unsafe {
                    msg_send![&*port, enablePortError: &mut error as *mut *mut AnyObject]
                };
            }
        }
        Ok(port_number)
    }

    fn remove_unicast_port(entry: &PeerEntry, address: Ipv4Addr) {
        let destination = u32::from_be_bytes(address.octets());
        let mut error: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: encoding `c@:@I^@` verified by the probe.
        let removed: Bool = unsafe {
            msg_send![&*entry.clock, removeUnicastUDPv4EtEPortFromInterfaceNamed: ns_string(&entry.interface), withDestinationAddress: destination, error: &mut error as *mut *mut AnyObject]
        };
        log::info!(
            target: "audiohub_airplay::airplay2",
            "TimeSync instance for {} removed timing peer {address} removed={} code={}",
            entry.shared.peer,
            removed.as_bool(),
            error_code(error)
        );
    }

    fn method_encoding(method: &objc2::runtime::Method) -> Option<String> {
        let mut encoding = method.return_type().to_str().ok()?.to_owned();
        for index in 0..method.arguments_count() {
            encoding.push_str(method.argument_type(index)?.to_str().ok()?);
        }
        Some(encoding)
    }

    fn find_port(clock: &AnyObject, port_number: u16) -> Option<Retained<AnyObject>> {
        // SAFETY: encoding `@@:` verified by the probe; NSArray accessors are
        // public Foundation API.
        let ports: Option<Retained<AnyObject>> = unsafe { msg_send![clock, ports] };
        let ports = ports?;
        // SAFETY: `ports` is an NSArray.
        let count: usize = unsafe { msg_send![&*ports, count] };
        let port_sel = sel!(portNumber);
        let role_sel = sel!(portRole);
        for index in 0..count {
            // SAFETY: index < count.
            let port: Option<Retained<AnyObject>> = unsafe { msg_send![&*ports, objectAtIndex: index] };
            let Some(port) = port else { continue };
            let class = port.class();
            if !class.responds_to(port_sel) || !class.responds_to(role_sel) {
                continue;
            }
            // SAFETY: responds to `portNumber` (encoding `S@:`).
            let number: u16 = unsafe { msg_send![&*port, portNumber] };
            if number == port_number {
                return Some(port);
            }
        }
        None
    }

    fn convert_to_mach(clock: &AnyObject, domain_ns: u64) -> Option<(u64, u64)> {
        let mut grandmaster = 0u64;
        let mut port = 0u16;
        // SAFETY: encoding `Q@:Q^Q^S` verified by the probe.
        let mach: u64 = unsafe {
            msg_send![clock, convertFromDomainToMachAbsoluteTime: domain_ns, grandmasterUsed: &mut grandmaster as *mut u64, portNumber: &mut port as *mut u16]
        };
        (mach != 0 && mach != u64::MAX).then_some((mach, grandmaster))
    }

    fn convert_to_domain(clock: &AnyObject, mach: u64) -> Option<u64> {
        let mut grandmaster = 0u64;
        let mut port = 0u16;
        // SAFETY: encoding `Q@:Q^Q^S` verified by the probe (optional selector,
        // only called when present).
        let domain: u64 = unsafe {
            msg_send![clock, convertFromMachAbsoluteToDomainTime: mach, grandmasterUsed: &mut grandmaster as *mut u64, portNumber: &mut port as *mut u16]
        };
        (domain != 0 && domain != u64::MAX).then_some(domain)
    }

    /// Role of the instance's ports: slave (3) when any port follows the
    /// grandmaster (the main sender port or a SETPEERS peer port, which a
    /// HomePod-style grandmaster may use directly), otherwise the main
    /// port's role.
    fn instance_port_role(clock: &AnyObject, main_port: u16) -> i32 {
        // SAFETY: encoding `@@:` verified by the probe; NSArray accessors are
        // public Foundation API.
        let ports: Option<Retained<AnyObject>> = unsafe { msg_send![clock, ports] };
        let Some(ports) = ports else {
            return 0;
        };
        // SAFETY: `ports` is an NSArray.
        let count: usize = unsafe { msg_send![&*ports, count] };
        let port_sel = sel!(portNumber);
        let role_sel = sel!(portRole);
        let mut main_role = 0;
        for index in 0..count {
            // SAFETY: index < count.
            let port: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*ports, objectAtIndex: index] };
            let Some(port) = port else { continue };
            let class = port.class();
            if !class.responds_to(port_sel) || !class.responds_to(role_sel) {
                continue;
            }
            // SAFETY: responds to `portRole` (encoding `i@:`).
            let role: i32 = unsafe { msg_send![&*port, portRole] };
            if role == PORT_ROLE_SLAVE {
                return role;
            }
            // SAFETY: responds to `portNumber` (encoding `S@:`).
            let number: u16 = unsafe { msg_send![&*port, portNumber] };
            if number == main_port {
                main_role = role;
            }
        }
        main_role
    }

    /// `(domain, mach, slope)` from two conversions one second apart.
    fn line_at(clock: &AnyObject, domain: u64) -> Option<(u64, u64, u64, u64)> {
        let (mach0, gm0) = convert_to_mach(clock, domain)?;
        let (mach1, gm1) = convert_to_mach(clock, domain.saturating_add(1_000_000_000))?;
        let nominal = clock_base().timebase.ticks_per_second();
        let slope = i128::from(mach1) - i128::from(mach0);
        // A real rate differs from nominal by ppm, never by 1 %.
        (gm0 == gm1 && slope > 0 && (slope - nominal).abs() * 100 < nominal).then_some((
            domain,
            mach0,
            slope as u64,
            gm0,
        ))
    }

    /// Read health fields and recompute the line; publish both.
    fn refresh_entry(entry: &mut PeerEntry, reverse_convert: bool, now_ns: u64) {
        let clock = &*entry.clock;
        // SAFETY: encodings `i@:`, `Q@:`, `Q@:` verified by the probe.
        let mut lock_state: i32 = unsafe { msg_send![clock, lockState] };
        let identity_gm: u64 = unsafe { msg_send![clock, grandmasterIdentity] };
        let self_identity: u64 = unsafe { msg_send![clock, clockIdentity] };
        let port_role = instance_port_role(clock, entry.shared.port_number);
        if test_hooks::fault() == Some(Fault::NeverLock) {
            lock_state = 0;
        }
        // SAFETY: no arguments; reads the tick counter.
        let mach_now = unsafe { mach_absolute_time() };
        let anchor_domain = if reverse_convert {
            convert_to_domain(clock, mach_now)
        } else {
            entry.shared.demand.last_remote_ns()
        };
        let mut line = TsLine {
            lock_state,
            grandmaster: identity_gm,
            self_identity,
            port_role,
            sampled_at_ns: now_ns,
            ..TsLine::default()
        };
        if let Some(domain) = anchor_domain {
            let mut fitted = line_at(clock, domain);
            // Without the reverse conversion the anchor is the last time
            // media asked for, which after a grandmaster change belongs to
            // the old timeline and can be far from now on the new one.
            // Re-anchor at the extrapolated "now" so the line stays exact
            // where it is used.
            if let Some((domain, mach, slope, _)) = fitted {
                let ticks_per_second = clock_base().timebase.ticks_per_second();
                if (i128::from(mach) - i128::from(mach_now)).abs() > ticks_per_second {
                    let probe = TsLine {
                        domain_anchor_ns: domain,
                        mach_anchor: mach,
                        mach_per_domain_second: slope,
                        ..TsLine::default()
                    };
                    if let Some(near) = probe
                        .domain_at_mach(mach_now)
                        .and_then(|value| u64::try_from(value).ok())
                    {
                        fitted = line_at(clock, near).or(fitted);
                    }
                }
            }
            if let Some((domain, mach, slope, gm)) = fitted {
                line.domain_anchor_ns = domain;
                line.mach_anchor = mach;
                line.mach_per_domain_second = slope;
                if gm != 0 {
                    line.grandmaster = gm;
                }
            }
        }
        let healthy = line.health().is_ok();
        if healthy {
            if let Some(previous) = entry
                .last_healthy
                .filter(|previous| previous.grandmaster != line.grandmaster)
            {
                // Planned grandmaster change (e.g. a HomePod joined the
                // sender's group): keep the old line for the handoff.
                entry.shared.retired.publish(&previous);
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "TimeSync instance for {} grandmaster changed old={:016x} new={:016x} ports={:?}",
                    entry.shared.peer,
                    previous.grandmaster,
                    line.grandmaster,
                    entry.extra.open_ports()
                );
            }
            entry.last_healthy = Some(line);
        }
        entry.shared.line.publish(&line);
        if healthy {
            entry.last_valid_ns = Some(now_ns);
        } else if entry.healthy
            && line.health() == Err(LineRejection::GrandmasterLost)
        {
            GM_LOST.fetch_add(1, Ordering::Relaxed);
            crate::telemetry::record_ptp_instance_gm_lost(crate::telemetry::operation_token());
            log::warn!(
                target: "audiohub_airplay::airplay2",
                "TimeSync instance for {} lost its grandmaster (gm={:016x} role={})",
                entry.shared.peer,
                line.grandmaster,
                line.port_role
            );
        }
        entry.healthy = healthy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_skeleton_accepts_both_bool_encodings_and_rejects_shape_changes() {
        assert_eq!(encoding_skeleton("c32@0:8^Q16^@24"), "c@:^Q^@");
        assert!(encoding_matches("c32@0:8^Q16^@24", "c@:^Q^@"));
        assert!(encoding_matches("B32@0:8^Q16^@24", "c@:^Q^@"), "arm64 BOOL");
        assert!(encoding_matches("c44@0:8@16I24^S28^@36", "c@:@I^S^@"));
        assert!(encoding_matches("Q40@0:8Q16^Q24^S32", "Q@:Q^Q^S"));
        assert!(!encoding_matches("v32@0:8^Q16^@24", "c@:^Q^@"), "return type changed");
        assert!(!encoding_matches("c32@0:8^Q16", "c@:^Q^@"), "argument removed");
        assert!(!encoding_matches("B40@0:8Q16^Q24^S32", "Q@:Q^Q^S"), "B only replaces c");
        assert!(!encoding_matches("c36@0:8Q16I24^@28", "c@:QS^@"), "port width changed");
    }

    #[test]
    fn required_selector_table_matches_the_recorded_macos_26_dump() {
        // Encodings copied from objc.txt / objc-port.txt (x86_64, 25G83).
        let dump = [
            ("addPTPInstance:error:", "c32@0:8^Q16^@24"),
            ("removePTPInstanceWithIdentifier:error:", "c32@0:8Q16^@24"),
            ("clockWithClockIdentifier:", "@24@0:8Q16"),
            (
                "addUnicastUDPv4EtEPortOnInterfaceNamed:withDestinationAddress:allocatedPortNumber:error:",
                "c44@0:8@16I24^S28^@36",
            ),
            (
                "removeUnicastUDPv4EtEPortFromInterfaceNamed:withDestinationAddress:error:",
                "c36@0:8@16I24^@28",
            ),
            (
                "convertFromDomainToMachAbsoluteTime:grandmasterUsed:portNumber:",
                "Q40@0:8Q16^Q24^S32",
            ),
            ("lockState", "i16@0:8"),
            ("grandmasterIdentity", "Q16@0:8"),
            ("clockIdentity", "Q16@0:8"),
            ("ports", "@16@0:8"),
            ("portNumber", "S16@0:8"),
            ("portRole", "i16@0:8"),
            ("enabled", "c16@0:8"),
            ("enablePortError:", "c24@0:8^@16"),
            (
                "overrideReceiveMatchingWithRemoteClockIdentity:remotePortNumber:error:",
                "c36@0:8Q16S24^@28",
            ),
            (
                "convertFromMachAbsoluteToDomainTime:grandmasterUsed:portNumber:",
                "Q40@0:8Q16^Q24^S32",
            ),
            ("sharedgPTPManager", "@16@0:8"),
            ("sharedClockManager", "@16@0:8"),
        ];
        for required in REQUIRED_SELECTORS {
            let (_, encoding) = dump
                .iter()
                .find(|(selector, _)| *selector == required.selector)
                .unwrap_or_else(|| panic!("{} missing from dump", required.selector));
            assert!(encoding_matches(encoding, required.skeleton), "{}", required.selector);
        }
        assert_eq!(
            REQUIRED_SELECTORS[OPTIONAL_REVERSE_CONVERT].selector,
            "convertFromMachAbsoluteToDomainTime:grandmasterUsed:portNumber:"
        );
    }

    #[test]
    fn mach_timebase_converts_x86_identity_and_apple_silicon_125_over_3() {
        let x86 = MachTimebase::new(1, 1).unwrap();
        assert_eq!(x86.ticks_to_ns(1_234_567), 1_234_567);
        assert_eq!(x86.ticks_per_second(), 1_000_000_000);
        let arm = MachTimebase::new(125, 3).unwrap();
        assert_eq!(arm.ticks_to_ns(24_000_000), 1_000_000_000);
        assert_eq!(arm.ticks_to_ns(-24), -1_000);
        assert_eq!(arm.ticks_to_ns(1), 42, "41.67 ns rounds to nearest");
        assert_eq!(arm.ticks_per_second(), 24_000_000);
        assert!(MachTimebase::new(0, 1).is_none());
    }

    fn healthy_line() -> TsLine {
        TsLine {
            domain_anchor_ns: 50_000_000_000,
            mach_anchor: 7_000_000_000,
            mach_per_domain_second: 24_000_012, // +0.5 ppm on a 24 MHz base
            lock_state: LOCK_STATE_LOCKED,
            grandmaster: 0xaa00_0000_0000_0a01,
            self_identity: 0x0115_5d23_31f6_0004,
            port_role: PORT_ROLE_SLAVE,
            sampled_at_ns: 1_000,
        }
    }

    #[test]
    fn handoff_between_lines_carries_the_old_timeline_to_the_new_grandmaster() {
        let now_ns = 10_000_000_000;
        let old = TsLine {
            domain_anchor_ns: 1_910_362_000_000_000,
            mach_anchor: 1_000_000_000,
            mach_per_domain_second: 1_000_000_000,
            lock_state: LOCK_STATE_LOCKED,
            grandmaster: 0x52737efca4c20008,
            self_identity: 0x11,
            port_role: PORT_ROLE_SLAVE,
            sampled_at_ns: now_ns - 300_000_000,
        };
        // The HomePod runs 8 ppm fast relative to mach.
        let new = TsLine {
            domain_anchor_ns: 49_072_000_000_000,
            mach_anchor: 1_200_000_000,
            mach_per_domain_second: 999_992_000,
            grandmaster: 0xf4cbe772eb370008,
            sampled_at_ns: now_ns - 50_000_000,
            ..old
        };
        let mach_now = 1_500_000_000;
        let handoff =
            handoff_between_lines(Some(old), Some(new), old.grandmaster, now_ns, mach_now).unwrap();
        assert_eq!(handoff.grandmaster, new.grandmaster);
        let old_now = old.domain_at_mach(mach_now).unwrap();
        let new_now = new.domain_at_mach(mach_now).unwrap();
        assert_eq!(old_now, 1_910_362_500_000_000);
        assert_eq!(old_now + handoff.delta_ns, new_now);
        // Mapping the rebased time through the new line lands on mach_now.
        let rebased = u64::try_from(old_now + handoff.delta_ns).unwrap();
        let mach = new
            .map_to_mach(new.sampled_at_ns, rebased, new.grandmaster)
            .unwrap();
        assert!((i128::from(mach) - i128::from(mach_now)).abs() <= 1);
        // And back again (HomePod -> Mac).
        let back = TsLine {
            sampled_at_ns: now_ns,
            ..old
        };
        let retired = TsLine {
            sampled_at_ns: now_ns - 100_000_000,
            ..new
        };
        let handoff =
            handoff_between_lines(Some(retired), Some(back), new.grandmaster, now_ns, mach_now)
                .unwrap();
        assert_eq!(handoff.grandmaster, old.grandmaster);
        assert_eq!(new_now + handoff.delta_ns, old_now);
    }

    #[test]
    fn handoff_between_lines_refuses_self_unlocked_stale_and_unknown_timelines() {
        let now_ns = 10_000_000_000;
        let line = TsLine {
            domain_anchor_ns: 1_000,
            mach_anchor: 1_000,
            mach_per_domain_second: 1_000_000_000,
            lock_state: LOCK_STATE_LOCKED,
            grandmaster: 0xbb,
            self_identity: 0x11,
            port_role: PORT_ROLE_SLAVE,
            sampled_at_ns: now_ns,
        };
        let retired = TsLine {
            grandmaster: 0xaa,
            ..line
        };
        let own = TsLine {
            grandmaster: 0x11,
            ..line
        };
        assert_eq!(
            handoff_between_lines(Some(retired), Some(own), 0xaa, now_ns, 2_000),
            Err("new_grandmaster_not_locked")
        );
        let unlocked = TsLine {
            lock_state: 1,
            ..line
        };
        assert_eq!(
            handoff_between_lines(Some(retired), Some(unlocked), 0xaa, now_ns, 2_000),
            Err("new_grandmaster_not_locked")
        );
        assert_eq!(
            handoff_between_lines(Some(retired), Some(line), 0xbb, now_ns, 2_000),
            Err("no_grandmaster_change")
        );
        assert_eq!(
            handoff_between_lines(None, Some(line), 0xaa, now_ns, 2_000),
            Err("old_mapping_unavailable")
        );
        let ancient = TsLine {
            sampled_at_ns: 1,
            ..retired
        };
        assert_eq!(
            handoff_between_lines(Some(ancient), Some(line), 0xaa, now_ns, 2_000),
            Err("old_mapping_too_old")
        );
        let stale = TsLine {
            sampled_at_ns: now_ns - 2_000_000_000,
            ..line
        };
        assert_eq!(
            handoff_between_lines(Some(retired), Some(stale), 0xaa, now_ns, 2_000),
            Err("new_line_stale")
        );
        assert_eq!(
            handoff_between_lines(Some(retired), None, 0xaa, now_ns, 2_000),
            Err("new_grandmaster_not_locked")
        );
    }

    #[test]
    fn extra_port_leases_count_references_across_sessions_sharing_an_instance() {
        let homepod: Ipv4Addr = "10.130.32.243".parse().unwrap();
        let other: Ipv4Addr = "10.130.32.107".parse().unwrap();
        let mut leases = ExtraPortLeases::default();
        let plan = leases.replace(1, &[homepod, homepod]);
        assert_eq!(plan.open, vec![homepod]);
        assert!(plan.close.is_empty());
        // A second session from the same sender shares the port.
        let plan = leases.replace(2, &[homepod, other]);
        assert_eq!(plan.open, vec![other]);
        // Session 1's group drops the HomePod: session 2 still needs it.
        let plan = leases.replace(1, &[]);
        assert!(plan.open.is_empty() && plan.close.is_empty());
        assert_eq!(leases.open_ports(), vec![other, homepod]);
        // Session 2 ends: both ports go.
        let mut closed = leases.release(2);
        closed.sort_unstable();
        assert_eq!(closed, vec![other, homepod]);
        assert!(leases.open_ports().is_empty());
        // A failed open is forgotten so the next update retries it.
        let plan = leases.replace(3, &[homepod]);
        assert_eq!(plan.open, vec![homepod]);
        leases.open_failed(3, homepod);
        assert!(leases.open_ports().is_empty());
        assert_eq!(leases.replace(3, &[homepod]).open, vec![homepod]);
        assert!(leases.release(99).is_empty());
    }

    #[test]
    fn line_extrapolates_linearly_in_both_directions() {
        let line = healthy_line();
        let gm = line.grandmaster;
        assert_eq!(line.map_to_mach(1_000, 50_000_000_000, gm), Ok(7_000_000_000));
        assert_eq!(
            line.map_to_mach(1_000, 52_000_000_000, gm),
            Ok(7_000_000_000 + 48_000_024)
        );
        assert_eq!(
            line.map_to_mach(1_000, 49_500_000_000, gm),
            Ok(7_000_000_000 - 12_000_006)
        );
        let early = TsLine {
            mach_anchor: 1_000,
            ..line
        };
        assert_eq!(
            early.map_to_mach(1_000, 0, gm),
            Err(LineRejection::OutOfRange),
            "a negative mach result is rejected"
        );
    }

    #[test]
    fn line_rejects_stale_unlocked_self_grandmaster_master_role_and_wrong_gm() {
        let line = healthy_line();
        let gm = line.grandmaster;
        let stale_at = 1_000 + LINE_MAX_AGE.as_nanos() as u64 + 1;
        assert_eq!(line.map_to_mach(stale_at, 1, gm), Err(LineRejection::Stale));
        let unlocked = TsLine { lock_state: 1, ..line };
        assert_eq!(unlocked.map_to_mach(1_000, 1, gm), Err(LineRejection::NotLocked));
        // v3iso.err:54/59: after GM-B died the instance elected itself and
        // its port turned master while lockState stayed 2.
        let self_gm = TsLine {
            grandmaster: line.self_identity,
            ..line
        };
        assert_eq!(
            self_gm.map_to_mach(1_000, 1, line.self_identity),
            Err(LineRejection::GrandmasterLost)
        );
        assert_eq!(
            PtpClockError::from(LineRejection::GrandmasterLost),
            PtpClockError::NotReady
        );
        let master = TsLine { port_role: 4, ..line };
        assert_eq!(master.map_to_mach(1_000, 1, gm), Err(LineRejection::GrandmasterLost));
        // A new grandmaster (GM-C) is healthy but does not match the old anchor.
        let other = TsLine {
            grandmaster: 0xcc00_0000_0000_0c03,
            ..line
        };
        assert_eq!(other.map_to_mach(1_000, 1, gm), Err(LineRejection::SourceMismatch));
        assert_eq!(
            PtpClockError::from(LineRejection::SourceMismatch),
            PtpClockError::SourceMismatch
        );
        let no_mapping = TsLine {
            mach_per_domain_second: 0,
            ..line
        };
        assert_eq!(no_mapping.map_to_mach(1_000, 1, gm), Err(LineRejection::NoMapping));
    }

    #[test]
    fn seqlock_round_trips_negative_fields_and_rejects_before_publication() {
        let cell = LineCell::default();
        assert_eq!(cell.read(), None);
        let line = TsLine {
            lock_state: -1,
            port_role: 4,
            ..healthy_line()
        };
        cell.publish(&line);
        assert_eq!(cell.read(), Some(line));
        let next = healthy_line();
        cell.publish(&next);
        assert_eq!(cell.read(), Some(next));
    }

    #[test]
    fn seqlock_readers_never_observe_a_torn_line() {
        let cell = std::sync::Arc::new(LineCell::default());
        cell.publish(&TsLine::default());
        let writer = {
            let cell = std::sync::Arc::clone(&cell);
            std::thread::spawn(move || {
                for value in 1..20_000u64 {
                    cell.publish(&TsLine {
                        domain_anchor_ns: value,
                        mach_anchor: value,
                        mach_per_domain_second: value,
                        grandmaster: value,
                        self_identity: value,
                        sampled_at_ns: value,
                        ..TsLine::default()
                    });
                }
            })
        };
        for _ in 0..20_000 {
            if let Some(line) = cell.read() {
                assert_eq!(line.domain_anchor_ns, line.mach_anchor);
                assert_eq!(line.mach_anchor, line.grandmaster);
                assert_eq!(line.grandmaster, line.sampled_at_ns);
            }
        }
        writer.join().unwrap();
    }

    #[test]
    fn breaker_trips_only_after_two_distinct_failing_peers_and_no_success() {
        let a = Ipv4Addr::new(10, 0, 0, 1);
        let b = Ipv4Addr::new(10, 0, 0, 2);
        let c = Ipv4Addr::new(10, 0, 0, 3);

        let fresh = Breaker::new();
        assert!(!fresh.peer_failed(a));
        assert!(
            !fresh.note_watchdog_failure(a),
            "one failing peer does not trip"
        );
        assert!(!fresh.is_tripped());
        assert!(fresh.peer_failed(a), "the failing peer is remembered");
        assert!(
            !fresh.peer_failed(b),
            "other peers keep the instance backend"
        );
        assert!(
            !fresh.note_watchdog_failure(a),
            "the same peer again is not distinct"
        );
        assert!(!fresh.is_tripped());
        assert!(
            fresh.note_watchdog_failure(b),
            "a second distinct peer trips"
        );
        assert!(fresh.is_tripped());
        assert!(!fresh.note_watchdog_failure(c), "trips once");
        assert!(fresh.is_tripped());

        let proven = Breaker::new();
        proven.note_mapped();
        assert!(!proven.note_watchdog_failure(a));
        assert!(!proven.note_watchdog_failure(b));
        assert!(
            !proven.is_tripped(),
            "after one success failures are only counted"
        );
        assert!(
            !proven.peer_failed(a),
            "after one success every peer retries the instance"
        );

        let late = Breaker::new();
        assert!(!late.note_watchdog_failure(a));
        late.note_mapped();
        assert!(
            !late.note_watchdog_failure(b),
            "a success before the second peer prevents the trip"
        );
        assert!(!late.is_tripped());
    }

    #[test]
    fn watchdog_fires_only_after_ten_seconds_of_continuous_unsatisfied_demand() {
        let second = 1_000_000_000u64;
        let demand = Demand::default();
        assert!(!demand.watchdog_expired(100 * second, None), "no demand, no watchdog");
        let start = 10 * second;
        demand.note(start, 5);
        for step in 1..=10u64 {
            demand.note(start + step * second, 5);
        }
        assert!(!demand.watchdog_expired(start + 10 * second, None));
        demand.note(start + 10 * second + 500_000_000, 5);
        assert!(demand.watchdog_expired(start + 10 * second + 500_000_000, None));
        // A valid line within the window keeps it alive.
        assert!(!demand.watchdog_expired(
            start + 10 * second + 500_000_000,
            Some(start + 9 * second)
        ));
        // Demand that stopped is not a failure.
        assert!(!demand.watchdog_expired(start + 20 * second, None));
        // Restarting demand after an idle gap restarts the window.
        demand.note(start + 30 * second, 5);
        assert!(!demand.watchdog_expired(start + 30 * second, None));
        assert_eq!(demand.last_remote_ns(), Some(5));
    }
}

/// Live V3 reproductions (design §4.2 M1/M2/M3/M6). They need the macOS
/// lab VM and grandmaster emulators on other hosts, so they are ignored by
/// default and driven by environment variables:
/// `AUDIOHUB_TS_LOCAL_IPV4`, `AUDIOHUB_TS_PEERS` (comma-separated IPv4, one
/// TimeSync instance each), optional `AUDIOHUB_TS_REMOTE_IDS` (hex clock ids
/// in the same order; requests the receive-matching override with port 1),
/// optional `AUDIOHUB_TS_SYSTEM_PEER` (one more peer on the CoreMedia system
/// clock), `AUDIOHUB_TS_ECHO_PORT` (emulator time-echo UDP port),
/// `AUDIOHUB_TS_DURATION_S`, optional `AUDIOHUB_TS_PERIOD_MS` (default 1000)
/// and `AUDIOHUB_TS_CSV` (output path).
#[cfg(all(test, target_os = "macos"))]
mod live {
    use super::*;
    use crate::protocol::media::ptp::PtpRemoteMatch;
    use crate::protocol::media::ptp_macos::MacPtpClock;
    use std::io::Write;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::time::Instant;

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
    }

    /// Echo request: 8-byte nonce; reply: nonce + 8-byte GM time (ns).
    fn echo(socket: &UdpSocket, peer: Ipv4Addr, port: u16, nonce: u64) -> Option<(u64, Instant, Instant)> {
        socket.set_nonblocking(true).ok()?;
        while socket.recv(&mut [0u8; 64]).is_ok() {}
        socket.set_nonblocking(false).ok()?;
        let sent = Instant::now();
        socket.send_to(&nonce.to_be_bytes(), (peer, port)).ok()?;
        let mut buffer = [0u8; 64];
        loop {
            let length = socket.recv(&mut buffer).ok()?;
            let received = Instant::now();
            if length >= 16 && buffer[..8] == nonce.to_be_bytes() {
                let gm = u64::from_be_bytes(buffer[8..16].try_into().unwrap());
                return Some((gm, sent, received));
            }
            if received.duration_since(sent) > Duration::from_millis(300) {
                return None;
            }
        }
    }

    fn signed_ns(a: Instant, b: Instant) -> i128 {
        if a >= b {
            a.duration_since(b).as_nanos() as i128
        } else {
            -(b.duration_since(a).as_nanos() as i128)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the macOS lab VM and live gPTP grandmaster emulators"]
    async fn live_instances_follow_independent_grandmasters() {
        let local: Ipv4Addr = env("AUDIOHUB_TS_LOCAL_IPV4").parse().unwrap();
        let peers: Vec<Ipv4Addr> = env("AUDIOHUB_TS_PEERS")
            .split(',')
            .map(|peer| peer.trim().parse().unwrap())
            .collect();
        let remote_ids: Vec<Option<u64>> = std::env::var("AUDIOHUB_TS_REMOTE_IDS")
            .map(|ids| {
                ids.split(',')
                    .map(|id| u64::from_str_radix(id.trim().trim_start_matches("0x"), 16).ok())
                    .collect()
            })
            .unwrap_or_default();
        let system_peer: Option<Ipv4Addr> = std::env::var("AUDIOHUB_TS_SYSTEM_PEER")
            .ok()
            .map(|peer| peer.parse().unwrap());
        let echo_port: u16 = env("AUDIOHUB_TS_ECHO_PORT").parse().unwrap();
        let duration = Duration::from_secs(env("AUDIOHUB_TS_DURATION_S").parse().unwrap());
        let period = Duration::from_millis(
            std::env::var("AUDIOHUB_TS_PERIOD_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1000),
        );
        let mut csv = std::fs::File::create(env("AUDIOHUB_TS_CSV")).unwrap();
        let availability = tokio::task::spawn_blocking(availability).await.unwrap();
        writeln!(csv, "# availability={availability:?} pid={}", std::process::id()).unwrap();
        assert!(availability.is_available(), "{availability:?}");
        let mut ports = Vec::new();
        for (index, peer) in peers.iter().enumerate() {
            let remote = remote_ids.get(index).copied().flatten().map(|clock_identity| {
                PtpRemoteMatch {
                    clock_identity,
                    port_number: 1,
                }
            });
            let port = acquire(local, *peer, remote).await.expect("instance");
            writeln!(
                csv,
                "# peer={peer} clock_identity={:016x} port={} receive_matching_override={:?}",
                port.clock_identity(),
                port.port_number(),
                port.receive_matching_overridden()
            )
            .unwrap();
            ports.push(port);
        }
        let system = system_peer.map(|peer| {
            let clock = MacPtpClock::new().expect("CoreMedia clock");
            let port = clock.add_ipv4_port(local, peer).expect("system port");
            writeln!(
                csv,
                "# system peer={peer} clock_identity={:016x} port={}",
                port.clock_identity(),
                port.port_number()
            )
            .unwrap();
            (peer, clock, port)
        });
        csv.flush().unwrap();
        writeln!(csv, "t_s,peer,backend,lock,gm,self,role,err_ns,rtt_ns,line_vs_direct_ns,status").unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let started = Instant::now();
        let mut nonce = 1u64;
        let mut system_gm = 0u64;
        while started.elapsed() < duration {
            let tick = Instant::now();
            for (peer, port) in peers.iter().zip(&ports) {
                nonce += 1;
                let line = port.line().unwrap_or_default();
                let mut row = format!(
                    "{:.3},{peer},instance,{},{:016x},{:016x},{}",
                    started.elapsed().as_secs_f64(),
                    line.lock_state,
                    line.grandmaster,
                    line.self_identity,
                    line.port_role
                );
                match echo(&socket, *peer, echo_port, nonce) {
                    Some((gm_time, sent, received)) => {
                        let reference = sent + received.duration_since(sent) / 2;
                        let rtt = received.duration_since(sent).as_nanos();
                        let mapped = port.map_remote_time(gm_time, line.grandmaster);
                        let direct = tokio::task::block_in_place(|| port.direct_convert(gm_time));
                        let line_vs_direct = match (
                            line.map_to_mach(TimeSyncPort::now_ns(), gm_time, line.grandmaster),
                            direct,
                        ) {
                            (Ok(extrapolated), Some((direct, _))) => {
                                let a = TimeSyncPort::instant_from_mach(extrapolated).unwrap();
                                let b = TimeSyncPort::instant_from_mach(direct).unwrap();
                                signed_ns(a, b).to_string()
                            }
                            _ => String::new(),
                        };
                        match mapped {
                            Ok(at) => row.push_str(&format!(
                                ",{},{rtt},{line_vs_direct},ok",
                                signed_ns(at, reference)
                            )),
                            Err(error) => row.push_str(&format!(
                                ",,{rtt},{line_vs_direct},{}",
                                error.telemetry_reason()
                            )),
                        }
                    }
                    None => row.push_str(",,,,echo_timeout"),
                }
                writeln!(csv, "{row}").unwrap();
            }
            if let Some((peer, _clock, port)) = system.as_ref() {
                nonce += 1;
                let mut row = format!(
                    "{:.3},{peer},system,{},{system_gm:016x},{:016x},",
                    started.elapsed().as_secs_f64(),
                    u8::from(port.is_locked()),
                    port.clock_identity()
                );
                match echo(&socket, *peer, echo_port, nonce) {
                    Some((gm_time, sent, received)) => {
                        let reference = sent + received.duration_since(sent) / 2;
                        let rtt = received.duration_since(sent).as_nanos();
                        // Learn the system domain's GM from a mismatch once.
                        let mapped = port.map_remote_time(gm_time, system_gm);
                        let mapped = match mapped {
                            Err(crate::protocol::media::ptp_macos::MacPtpError::GrandmasterMismatch {
                                observed,
                                ..
                            }) => {
                                system_gm = observed;
                                port.map_remote_time(gm_time, system_gm)
                            }
                            other => other,
                        };
                        match mapped {
                            Ok(at) => row.push_str(&format!(",{},{rtt},,ok", signed_ns(at, reference))),
                            Err(error) => row.push_str(&format!(",,{rtt},,{error}")),
                        }
                    }
                    None => row.push_str(",,,,echo_timeout"),
                }
                writeln!(csv, "{row}").unwrap();
            }
            csv.flush().unwrap();
            let rest = period.saturating_sub(tick.elapsed());
            tokio::time::sleep(rest).await;
        }
        writeln!(
            csv,
            "# gm_lost_total={} lock_timeouts_total={} tripped={}",
            GM_LOST.load(Ordering::Relaxed),
            LOCK_TIMEOUTS.load(Ordering::Relaxed),
            BREAKER.is_tripped()
        )
        .unwrap();
        drop(system);
        drop(ports);
        // Give the service thread a moment to remove the instances.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
