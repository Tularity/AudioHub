//! Per-session PTP timeline: accepted timing peers and grandmaster handoff.
//!
//! An AirPlay 2 stream maps its sender's rate anchor (RTP time <-> network
//! time on the anchor's `timeline_id`, the grandmaster identity) onto the
//! local monotonic clock. When the sender groups another device, the group's
//! grandmaster can move. Evidence from 2026-11-20 (30-win, Mac Music adding
//! and removing a HomePod, docs/airplay-gm-handoff-20261121.md):
//!
//! - the sender first sends SETPEERS with the new peer address list, then its
//!   TimeSync clock follows the new grandmaster about 0.1 s later;
//! - the sender keeps relaying Sync/Follow_Up to the receiver as a gPTP
//!   boundary clock, now carrying the new grandmaster's identity and time;
//! - Apple Music does not resend SETRATEANCHORTIME (its own CoreMedia clock
//!   resynthesizes a continuous timeline across the change), while an iPhone
//!   resent an anchor on the new timeline about 2 s later.
//!
//! This module therefore:
//! - keeps the session's accepted timing peers (control peer, SETUP
//!   timing-peer addresses, the latest SETPEERS/SETPEERSX list);
//! - keeps warm mappers for every other accepted (peer, grandmaster) source;
//! - follows a new anchor's timeline directly (promoting a warm mapper);
//! - when the source's grandmaster changes without a new anchor, carries the
//!   current anchor across through local time: the old and the new mapping
//!   are both measured against the same local clock, so
//!   `new = old + (remote_new(t) - remote_old(t))` keeps playback continuous,
//!   which is what CoreMedia's `figTimeSyncClock` resync does on Apple
//!   receivers;
//! - fails the stream explicitly when neither is possible, instead of
//!   holding audio forever.
//!
//! macOS backends map inside TimeSync/CoreMedia; there the handoff delta
//! comes from [`PtpClockSource::platform_handoff`].

use super::buffered_clock::PlaybackAnchor;
use super::ptp::{PlatformHandoff, PtpClockError, PtpClockMapper, PtpClockSample, PtpClockSource};
use crate::telemetry::{DeadlineSource, MediaKind};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A source silent this long may be replaced by another accepted source
/// (AirPlay senders send Sync every 125 ms).
pub(crate) const SOURCE_SILENCE: Duration = Duration::from_secs(1);
/// The old grandmaster's last valid mapping may carry an anchor across a
/// handoff for this long.
pub(crate) const HANDOFF_MAX_OLD_AGE: Duration = Duration::from_secs(5);
/// A detected grandmaster change that neither a handoff nor a new anchor
/// resolved within this time ends the stream. An iPhone resent its anchor
/// about 2 s after SETPEERS (30-win 2026-11-20 05:27:57Z -> 05:27:59Z).
pub(crate) const HANDOFF_FAIL_GRACE: Duration = Duration::from_secs(4);
/// A per-peer grandmaster observation older than this is not current.
const OBSERVED_FRESH: Duration = Duration::from_secs(2);
const MAX_CANDIDATES: usize = 4;
const MAX_PENDING_SAMPLES: usize = 32;
const MAX_RETIRED_TIMELINES: usize = 4;
const MAX_OBSERVED_SOURCES: usize = 8;
/// Upper bound of accepted timing peers per session (SETPEERS lists hold at
/// most 64 entries).
pub(crate) const MAX_SESSION_PEERS: usize = 64;
/// CoreMedia conversions closer than this to the target host time end the
/// handoff search.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const CONVERGED_NS: i128 = 1_000_000;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MAX_CONVERGENCE_STEPS: usize = 8;
/// Offsets 2^30 ns (~1 s) .. 2^62 ns (~146 years) tried around an
/// unconvertible old-timeline time.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SEARCH_EXPONENTS: std::ops::RangeInclusive<u32> = 30..=62;

// ---------------------------------------------------------------- shared

/// Session state shared by the RTSP control connection (SETPEERS) and the
/// media engine (sample filter, handoff, diagnostics).
#[derive(Debug)]
pub(crate) struct SessionShared {
    control_peer: IpAddr,
    generation: AtomicU64,
    peers: Mutex<Vec<IpAddr>>,
    /// Grandmaster the media engine currently maps through; 0 = none.
    grandmaster: AtomicU64,
}

/// Result of replacing a session's accepted timing peers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PeerSetChange {
    pub(crate) peers: Vec<IpAddr>,
    pub(crate) added: Vec<IpAddr>,
    pub(crate) removed: Vec<IpAddr>,
}

impl SessionShared {
    pub(crate) fn new(control_peer: IpAddr) -> Self {
        Self {
            control_peer,
            generation: AtomicU64::new(0),
            peers: Mutex::new(Vec::new()),
            grandmaster: AtomicU64::new(0),
        }
    }

    pub(crate) fn control_peer(&self) -> IpAddr {
        self.control_peer
    }

    /// Replace the accepted timing peers. The control peer is always
    /// accepted and is never part of the list.
    pub(crate) fn replace_peers(&self, mut peers: Vec<IpAddr>) -> PeerSetChange {
        peers.retain(|peer| *peer != self.control_peer);
        peers.sort_unstable();
        peers.dedup();
        peers.truncate(MAX_SESSION_PEERS);
        let mut current = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        let added = peers
            .iter()
            .filter(|peer| !current.contains(peer))
            .copied()
            .collect();
        let removed = current
            .iter()
            .filter(|peer| !peers.contains(peer))
            .copied()
            .collect();
        *current = peers.clone();
        self.generation.fetch_add(1, Ordering::AcqRel);
        PeerSetChange {
            peers,
            added,
            removed,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn peers(&self) -> (u64, Vec<IpAddr>) {
        let peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        (self.generation.load(Ordering::Acquire), peers.clone())
    }

    pub(crate) fn publish_grandmaster(&self, grandmaster: u64) {
        self.grandmaster.store(grandmaster, Ordering::Relaxed);
    }

    pub(crate) fn grandmaster(&self) -> Option<u64> {
        let value = self.grandmaster.load(Ordering::Relaxed);
        (value != 0).then_some(value)
    }
}

// ---------------------------------------------------------------- media side

#[derive(Debug, Clone, Copy)]
struct ObservedSource {
    peer: IpAddr,
    grandmaster: u64,
    at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct PendingChange {
    since: Instant,
    from: u64,
    to: Option<u64>,
    failure: Option<&'static str>,
}

/// The session has no mapper yet (no playing anchor was installed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MissingMapper;

/// One media stream's PTP clock: source selection, mapping and handoff.
pub(crate) struct SessionPtpClock {
    source: PtpClockSource,
    control_peer: IpAddr,
    epoch: Instant,
    kind: MediaKind,
    peers_generation: Option<u64>,
    peers: Vec<IpAddr>,
    /// Mapper for the anchor's timeline. On macOS backends it only records
    /// the timeline; mapping happens in TimeSync/CoreMedia.
    current: Option<PtpClockMapper>,
    current_since: Instant,
    /// Last valid `local_minus_remote_ns` of `current` and when it was read.
    last_good: Option<(Instant, i128)>,
    candidates: Vec<PtpClockMapper>,
    pending: VecDeque<PtpClockSample>,
    observed: Vec<ObservedSource>,
    /// Timelines handed off from, with the delta that converts their time
    /// to the current timeline.
    retired: Vec<(u64, i128)>,
    change: Option<PendingChange>,
    /// Last `(retired, current)` translation reported at INFO.
    translation_logged: Option<(u64, u64)>,
    mapped_on_anchor: bool,
    fatal_reported: bool,
    ready_logged: bool,
}

impl SessionPtpClock {
    pub(crate) fn new(
        source: PtpClockSource,
        control_peer: IpAddr,
        epoch: Instant,
        kind: MediaKind,
    ) -> Self {
        Self {
            source,
            control_peer,
            epoch,
            kind,
            peers_generation: None,
            peers: Vec::new(),
            current: None,
            current_since: epoch,
            last_good: None,
            candidates: Vec::with_capacity(MAX_CANDIDATES),
            pending: VecDeque::with_capacity(MAX_PENDING_SAMPLES),
            observed: Vec::with_capacity(MAX_OBSERVED_SOURCES),
            retired: Vec::new(),
            change: None,
            translation_logged: None,
            mapped_on_anchor: false,
            fatal_reported: false,
            ready_logged: false,
        }
    }

    /// Grandmaster of the current mapping, if any.
    pub(crate) fn grandmaster(&self) -> Option<u64> {
        self.current.as_ref().map(PtpClockMapper::grandmaster)
    }

    fn refresh_peers(&mut self) {
        let Some(shared) = self.source.session() else {
            return;
        };
        if self.peers_generation == Some(shared.generation()) {
            return;
        }
        let (generation, peers) = shared.peers();
        self.peers = peers;
        self.peers_generation = Some(generation);
        let control = self.control_peer;
        let accepted = self.peers.clone();
        let accepts = |peer: IpAddr| peer == control || accepted.contains(&peer);
        self.candidates.retain(|mapper| accepts(mapper.peer()));
        self.observed.retain(|source| accepts(source.peer));
        self.pending.retain(|sample| accepts(sample.peer));
    }

    fn accepts(&self, peer: IpAddr) -> bool {
        peer == self.control_peer || self.peers.contains(&peer)
    }

    fn note_observed(&mut self, peer: IpAddr, grandmaster: u64, at: Instant) {
        if let Some(entry) = self.observed.iter_mut().find(|entry| entry.peer == peer) {
            if at >= entry.at {
                entry.grandmaster = grandmaster;
                entry.at = at;
            }
            return;
        }
        if self.observed.len() == MAX_OBSERVED_SOURCES {
            if let Some(oldest) = self
                .observed
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(index, _)| index)
            {
                self.observed.swap_remove(oldest);
            }
        }
        self.observed.push(ObservedSource {
            peer,
            grandmaster,
            at,
        });
    }

    fn observed_grandmaster(&self, peer: IpAddr, now: Instant) -> Option<u64> {
        self.observed
            .iter()
            .find(|entry| entry.peer == peer)
            .filter(|entry| now.saturating_duration_since(entry.at) <= OBSERVED_FRESH)
            .map(|entry| entry.grandmaster)
    }

    /// Feed one observer sample. `anchor` is the stream's playing anchor, if
    /// any; a handoff rebases it in place.
    pub(crate) fn observe_sample(
        &mut self,
        sample: PtpClockSample,
        now: Instant,
        anchor: Option<&mut PlaybackAnchor>,
    ) {
        self.refresh_peers();
        if !self.accepts(sample.peer) {
            return;
        }
        let now = now.max(sample.received_at);
        self.note_observed(sample.peer, sample.grandmaster, sample.received_at);
        let Some((current_peer, current_gm)) = self
            .current
            .as_ref()
            .map(|mapper| (mapper.peer(), mapper.grandmaster()))
        else {
            if self.pending.len() == MAX_PENDING_SAMPLES {
                if let Some(evicted) = self.pending.pop_front() {
                    crate::telemetry::record_ptp_pending_disposition(
                        evicted.telemetry_token,
                        "mapper_pending_sample_evicted",
                        1,
                    );
                }
            }
            self.pending.push_back(sample);
            return;
        };
        if current_peer == sample.peer && current_gm == sample.grandmaster {
            self.observe_current(sample);
        } else {
            self.observe_candidate(sample);
        }
        self.maintain(now, anchor);
    }

    fn observe_current(&mut self, sample: PtpClockSample) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        match current.observe(sample) {
            Ok(Some(estimate)) => {
                if !self.ready_logged {
                    self.ready_logged = true;
                    let label = match self.kind {
                        MediaKind::Buffered => "AirPlay 2 PTP clock ready",
                        MediaKind::Realtime => "AirPlay 2 realtime PTP clock ready",
                    };
                    log::info!(
                        target: "audiohub_airplay::airplay2",
                        "{label} samples={} timeline={:016x} peer={}",
                        estimate.retained_samples,
                        sample.grandmaster,
                        sample.peer
                    );
                }
            }
            Ok(None) => {}
            Err(error) => log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP sample rejected: {error}"
            ),
        }
    }

    fn observe_candidate(&mut self, sample: PtpClockSample) {
        let index = match self.candidates.iter().position(|mapper| {
            mapper.peer() == sample.peer && mapper.grandmaster() == sample.grandmaster
        }) {
            Some(index) => index,
            None => {
                self.push_candidate(PtpClockMapper::new(
                    sample.peer,
                    sample.grandmaster,
                    self.epoch,
                    sample.telemetry_token,
                    self.kind,
                ));
                self.candidates.len() - 1
            }
        };
        if let Err(error) = self.candidates[index].observe(sample) {
            log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP candidate sample rejected peer={} gm={:016x}: {error}",
                sample.peer,
                sample.grandmaster
            );
        }
    }

    fn push_candidate(&mut self, mapper: PtpClockMapper) {
        self.candidates.retain(|existing| {
            existing.peer() != mapper.peer() || existing.grandmaster() != mapper.grandmaster()
        });
        if self.candidates.len() >= MAX_CANDIDATES {
            if let Some(oldest) = self
                .candidates
                .iter()
                .enumerate()
                .min_by_key(|(_, mapper)| mapper.last_sample_at())
                .map(|(index, _)| index)
            {
                self.candidates.remove(oldest);
            }
        }
        self.candidates.push(mapper);
    }

    /// Install a playing anchor. A retired timeline is translated to the
    /// current one; a new timeline promotes a warm mapper when one exists.
    pub(crate) fn install_anchor(&mut self, anchor: &mut PlaybackAnchor, now: Instant) {
        self.refresh_peers();
        self.translate_retired(anchor);
        let previous = self
            .current
            .as_ref()
            .map(|mapper| (mapper.peer(), mapper.grandmaster()));
        if previous.map(|(_, grandmaster)| grandmaster) != Some(anchor.timeline_id) {
            // A new timeline restarts handoff bookkeeping. A refresh on the
            // same timeline (realtime d7 every second, a resume) keeps any
            // pending change so an unfollowable one still fails.
            self.mapped_on_anchor = false;
            self.change = None;
            self.fatal_reported = false;
            let warm = self.take_candidate_for(anchor.timeline_id, now);
            let promoted = warm.is_some();
            let replacement = warm.unwrap_or_else(|| {
                PtpClockMapper::new(
                    self.control_peer,
                    anchor.timeline_id,
                    self.epoch,
                    anchor.telemetry_token,
                    self.kind,
                )
            });
            let source_peer = replacement.peer();
            if let Some(old) = self.current.replace(replacement) {
                self.push_candidate(old);
            }
            self.current_since = now;
            self.last_good = None;
            self.ready_logged = false;
            if let Some((_, old)) = previous {
                // The sender moved its anchor to another timeline itself.
                self.retired
                    .retain(|(grandmaster, _)| *grandmaster != anchor.timeline_id);
                crate::telemetry::record_ptp_anchor_timeline_switch(anchor.telemetry_token);
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP anchor timeline switched old={old:016x} new={:016x} source_peer={source_peer} warm_mapper={promoted} {}",
                    anchor.timeline_id,
                    self.describe(now)
                );
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for sample in pending {
            let current = self.current.as_ref().expect("installed above");
            if current.peer() == sample.peer && current.grandmaster() == sample.grandmaster {
                self.observe_current(sample);
            } else if self.accepts(sample.peer) {
                self.observe_candidate(sample);
            } else {
                crate::telemetry::record_ptp_pending_disposition(
                    sample.telemetry_token,
                    "mapper_pending_sample_discarded",
                    1,
                );
            }
        }
        self.publish();
    }

    fn take_candidate_for(&mut self, grandmaster: u64, now: Instant) -> Option<PtpClockMapper> {
        let control = self.control_peer;
        let index = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, mapper)| mapper.grandmaster() == grandmaster)
            .max_by_key(|(_, mapper)| {
                (
                    mapper.handoff_ready(now).is_some(),
                    mapper.peer() == control,
                    mapper.last_sample_at(),
                )
            })
            .map(|(index, _)| index)?;
        Some(self.candidates.remove(index))
    }

    fn translate_retired(&mut self, anchor: &mut PlaybackAnchor) {
        let Some(current) = self.grandmaster() else {
            return;
        };
        if current == anchor.timeline_id {
            return;
        }
        let Some(&(_, delta)) = self
            .retired
            .iter()
            .find(|(grandmaster, _)| *grandmaster == anchor.timeline_id)
        else {
            return;
        };
        let Some(translated) = shift_remote(anchor.remote_time_ns, delta) else {
            return;
        };
        // Realtime senders repeat their anchor every second: report the
        // first translation per retired timeline, then only at debug.
        let key = (anchor.timeline_id, current);
        if self.translation_logged != Some(key) {
            self.translation_logged = Some(key);
            log::info!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP anchor on retired timeline {:016x} translated to {current:016x} delta_ns={delta}",
                anchor.timeline_id
            );
        } else {
            log::debug!(
                target: "audiohub_airplay::airplay2",
                "AirPlay 2 PTP anchor on retired timeline {:016x} translated to {current:016x}",
                anchor.timeline_id
            );
        }
        anchor.remote_time_ns = translated;
        anchor.timeline_id = current;
    }

    /// Record a handoff from `from` to `to` for later anchors still labelled
    /// with an older timeline.
    fn retire(&mut self, from: u64, to: u64, delta: i128) {
        if from == to {
            return;
        }
        for (_, value) in &mut self.retired {
            *value += delta;
        }
        self.retired
            .retain(|(grandmaster, _)| *grandmaster != to && *grandmaster != from);
        self.retired.insert(0, (from, delta));
        self.retired.truncate(MAX_RETIRED_TIMELINES);
    }

    fn publish(&self) {
        if let (Some(shared), Some(grandmaster)) = (self.source.session(), self.grandmaster()) {
            shared.publish_grandmaster(grandmaster);
        }
    }

    /// Portable grandmaster/source handoff. Called on every accepted sample
    /// and before every portable deadline.
    fn maintain(&mut self, now: Instant, anchor: Option<&mut PlaybackAnchor>) {
        if self.source.maps_on_platform() {
            return;
        }
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let (current_peer, current_gm) = (current.peer(), current.grandmaster());
        if let Ok(estimate) = current.estimate(now) {
            self.last_good = Some((now, estimate.local_minus_remote_ns));
        }
        let last_sample = current.last_sample_at().unwrap_or(self.current_since);
        let silent = now.saturating_duration_since(last_sample) >= SOURCE_SILENCE;
        let source_gm = self.observed_grandmaster(current_peer, now);
        let (wanted, target) = match source_gm {
            Some(grandmaster) if grandmaster != current_gm => {
                // The source now relays another grandmaster.
                let target = self.candidates.iter().position(|mapper| {
                    mapper.peer() == current_peer
                        && mapper.grandmaster() == grandmaster
                        && mapper.handoff_ready(now).is_some()
                });
                (Some(grandmaster), target)
            }
            _ if silent => match self.best_ready_candidate(now, current_gm, source_gm) {
                Some(index) => (Some(self.candidates[index].grandmaster()), Some(index)),
                // Plain silence is held like before: it may be a network gap.
                None => return,
            },
            _ => {
                if self.change.take().is_some() {
                    log::info!(
                        target: "audiohub_airplay::airplay2",
                        "AirPlay 2 PTP source back on grandmaster {current_gm:016x} peer={current_peer}"
                    );
                }
                return;
            }
        };
        self.note_change(now, current_gm, wanted);
        if let Some(index) = target {
            self.handoff_to(index, now, anchor);
        }
    }

    fn best_ready_candidate(
        &self,
        now: Instant,
        current_gm: u64,
        source_gm: Option<u64>,
    ) -> Option<usize> {
        let control = self.control_peer;
        self.candidates
            .iter()
            .enumerate()
            .filter(|(_, mapper)| {
                mapper.handoff_ready(now).is_some()
                    && mapper
                        .last_sample_at()
                        .is_some_and(|at| now.saturating_duration_since(at) < SOURCE_SILENCE)
            })
            .max_by_key(|(_, mapper)| {
                (
                    mapper.grandmaster() == current_gm,
                    Some(mapper.grandmaster()) == source_gm,
                    mapper.peer() == control,
                    mapper.last_sample_at(),
                )
            })
            .map(|(index, _)| index)
    }

    fn note_change(&mut self, now: Instant, from: u64, to: Option<u64>) {
        match self.change.as_mut() {
            Some(change) if change.to == to => return,
            Some(change) => {
                change.to = to;
                change.failure = None;
            }
            None => {
                self.change = Some(PendingChange {
                    since: now,
                    from,
                    to,
                    failure: None,
                });
            }
        }
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP grandmaster change detected from_gm={from:016x} new_gm={} {}",
            format_grandmaster(to),
            self.describe(now)
        );
    }

    fn fail_change(&mut self, now: Instant, reason: &'static str) {
        let Some(change) = self.change.as_mut() else {
            return;
        };
        if change.failure == Some(reason) {
            return;
        }
        change.failure = Some(reason);
        let (from, to) = (change.from, change.to);
        crate::telemetry::record_ptp_gm_handoff_failure(
            crate::telemetry::operation_token(),
            self.control_peer,
            from,
            to,
            reason,
        );
        log::warn!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP grandmaster handoff failed reason={reason} from_gm={from:016x} new_gm={} {}",
            format_grandmaster(to),
            self.describe(now)
        );
    }

    fn handoff_to(&mut self, index: usize, now: Instant, anchor: Option<&mut PlaybackAnchor>) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let (old_peer, old_gm) = (current.peer(), current.grandmaster());
        let target = &self.candidates[index];
        let (new_peer, new_gm) = (target.peer(), target.grandmaster());
        let Some(new_estimate) = target.handoff_ready(now) else {
            return;
        };
        let delta = if new_gm == old_gm {
            // Same timeline from another peer: nothing to rebase.
            0
        } else {
            let old_offset = current
                .estimate(now)
                .ok()
                .map(|estimate| estimate.local_minus_remote_ns)
                .or_else(|| {
                    self.last_good
                        .filter(|(at, _)| now.saturating_duration_since(*at) <= HANDOFF_MAX_OLD_AGE)
                        .map(|(_, offset)| offset)
                });
            let Some(old_offset) = old_offset else {
                self.fail_change(now, "old_mapping_unavailable");
                return;
            };
            // remote_new(t) - remote_old(t) for one local instant t.
            old_offset - new_estimate.local_minus_remote_ns
        };
        let mut rebased = false;
        if let Some(anchor) = anchor {
            if anchor.timeline_id == old_gm {
                let Some(remote) = shift_remote(anchor.remote_time_ns, delta) else {
                    self.fail_change(now, "rebase_out_of_range");
                    return;
                };
                anchor.remote_time_ns = remote;
                anchor.timeline_id = new_gm;
                rebased = true;
            }
        }
        self.retire(old_gm, new_gm, delta);
        let target = self.candidates.remove(index);
        if let Some(old) = self.current.replace(target) {
            self.push_candidate(old);
        }
        self.current_since = now;
        self.last_good = Some((now, new_estimate.local_minus_remote_ns));
        self.ready_logged = true;
        self.change = None;
        let kind = if new_gm == old_gm {
            "same_grandmaster"
        } else {
            "rebased"
        };
        crate::telemetry::record_ptp_gm_handoff(
            crate::telemetry::operation_token(),
            self.control_peer,
            old_gm,
            new_gm,
            kind,
            delta,
        );
        log::info!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP grandmaster handoff old_gm={old_gm:016x} new_gm={new_gm:016x} old_peer={old_peer} new_peer={new_peer} kind={kind} delta_ns={delta} anchor_rebased={rebased} {}",
            self.describe(now)
        );
        self.publish();
    }

    fn platform_handoff(&mut self, now: Instant, anchor: &mut PlaybackAnchor) -> bool {
        let from = anchor.timeline_id;
        let result = match self.source.platform_handoff(from) {
            None => return false,
            Some(result) => result,
        };
        match result {
            Ok(PlatformHandoff {
                grandmaster,
                delta_ns,
            }) => {
                let Some(remote) = shift_remote(anchor.remote_time_ns, delta_ns) else {
                    self.note_change(now, from, Some(grandmaster));
                    self.fail_change(now, "rebase_out_of_range");
                    return false;
                };
                anchor.remote_time_ns = remote;
                anchor.timeline_id = grandmaster;
                self.retire(from, grandmaster, delta_ns);
                // Keep the recorded timeline aligned with the rebased anchor.
                self.current = Some(PtpClockMapper::new(
                    self.control_peer,
                    grandmaster,
                    self.epoch,
                    anchor.telemetry_token,
                    self.kind,
                ));
                self.change = None;
                crate::telemetry::record_ptp_gm_handoff(
                    crate::telemetry::operation_token(),
                    self.control_peer,
                    from,
                    grandmaster,
                    "platform",
                    delta_ns,
                );
                log::info!(
                    target: "audiohub_airplay::airplay2",
                    "AirPlay 2 PTP grandmaster handoff old_gm={from:016x} new_gm={grandmaster:016x} backend={} kind=platform delta_ns={delta_ns} anchor_rebased=true",
                    self.source.backend().as_str()
                );
                self.publish();
                true
            }
            Err(reason) => {
                // Before this anchor ever mapped, a mismatch is start-up
                // convergence of the platform clock; keep holding as before.
                if self.mapped_on_anchor {
                    self.note_change(now, from, self.source.platform_grandmaster());
                    self.fail_change(now, reason);
                }
                false
            }
        }
    }

    /// Deadline for `rtp` under the playing `anchor`, which a handoff may
    /// rebase in place.
    pub(crate) fn deadline(
        &mut self,
        now: Instant,
        anchor: &mut PlaybackAnchor,
        rtp: u32,
        sample_rate: u32,
        telemetry_token: crate::telemetry::OperationToken,
    ) -> Result<(DeadlineSource, Result<Instant, PtpClockError>), MissingMapper> {
        if self.current.is_none() {
            return Err(MissingMapper);
        }
        if let Some(mut result) =
            self.source
                .platform_deadline_for_rtp(*anchor, rtp, sample_rate, telemetry_token)
        {
            if matches!(result, Err(PtpClockError::SourceMismatch))
                && self.platform_handoff(now, anchor)
            {
                if let Some(retry) = self.source.platform_deadline_for_rtp(
                    *anchor,
                    rtp,
                    sample_rate,
                    telemetry_token,
                ) {
                    result = retry;
                }
            }
            if result.is_ok() {
                self.mapped_on_anchor = true;
            }
            return Ok((DeadlineSource::Platform, result));
        }
        self.maintain(now, Some(anchor));
        let mapper = self.current.as_ref().ok_or(MissingMapper)?;
        let result = mapper.deadline_for_rtp(now, *anchor, rtp, sample_rate, telemetry_token);
        if result.is_ok() {
            self.mapped_on_anchor = true;
        }
        Ok((DeadlineSource::PortableMapper, result))
    }

    /// A detected grandmaster change that could not be followed for
    /// [`HANDOFF_FAIL_GRACE`] while an anchor that already mapped is
    /// playing. Returned once; the caller ends the stream so the sender sees
    /// the failure instead of silence.
    pub(crate) fn take_fatal(&mut self, now: Instant, playing: bool) -> Option<io::Error> {
        if self.fatal_reported || !playing || !self.mapped_on_anchor {
            return None;
        }
        let change = self.change?;
        if now.saturating_duration_since(change.since) < HANDOFF_FAIL_GRACE {
            return None;
        }
        self.fatal_reported = true;
        let reason = change.failure.unwrap_or("new_grandmaster_not_mappable");
        crate::telemetry::record_ptp_gm_handoff_failure(
            crate::telemetry::operation_token(),
            self.control_peer,
            change.from,
            change.to,
            "grace_expired",
        );
        log::warn!(
            target: "audiohub_airplay::airplay2",
            "AirPlay 2 PTP grandmaster changed from {:016x} to {} and the session clock could not follow it within {:?} (reason={reason}); ending the stream so the sender can rebuild it {}",
            change.from,
            format_grandmaster(change.to),
            HANDOFF_FAIL_GRACE,
            self.describe(now)
        );
        Some(io::Error::new(
            io::ErrorKind::TimedOut,
            "PTP grandmaster changed and the session clock could not follow it",
        ))
    }

    /// One-line diagnostics: the session's grandmaster against what the
    /// accepted peers currently relay.
    pub(crate) fn describe(&self, now: Instant) -> String {
        let mut text = String::with_capacity(160);
        let _ = write!(
            text,
            "session_gm={} backend={} control_peer={}",
            format_grandmaster(self.grandmaster()),
            self.source.backend().as_str(),
            self.control_peer
        );
        if let Some(current) = self.current.as_ref() {
            let _ = write!(text, " source_peer={}", current.peer());
        }
        if let Some(platform) = self.source.platform_grandmaster() {
            let _ = write!(text, " platform_gm={platform:016x}");
        }
        text.push_str(" observed=[");
        for (index, source) in self.observed.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            let _ = write!(
                text,
                "{}:{:016x}@{}ms",
                source.peer,
                source.grandmaster,
                now.saturating_duration_since(source.at).as_millis()
            );
        }
        text.push_str("] peers=[");
        for (index, peer) in self.peers.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            let _ = write!(text, "{peer}");
        }
        text.push(']');
        text
    }
}

fn format_grandmaster(grandmaster: Option<u64>) -> String {
    grandmaster.map_or_else(|| "none".to_owned(), |value| format!("{value:016x}"))
}

/// `remote + delta`, or `None` outside the unsigned nanosecond timeline.
pub(crate) fn shift_remote(remote: u64, delta: i128) -> Option<u64> {
    u64::try_from(i128::from(remote).checked_add(delta)?).ok()
}

/// Last successful clock-time -> host-time conversion on one grandmaster.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LastMapping {
    pub(crate) grandmaster: u64,
    pub(crate) remote_ns: i128,
    pub(crate) host_ns: i128,
}

/// Handoff delta for a clock that only converts grandmaster time to host
/// time (CoreMedia's CM8021AS clock). The old grandmaster's time at
/// `host_now` is extrapolated from its last mapping; the new grandmaster's
/// time at `host_now` is found by repeatedly correcting a guess with the
/// clock's own conversion. `convert` returns `(host_ns, grandmaster_used)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn converge_clock_handoff(
    from: u64,
    own_identity: u64,
    last: Option<LastMapping>,
    host_now: i128,
    mut convert: impl FnMut(i128) -> Result<(i128, u64), &'static str>,
) -> Result<PlatformHandoff, &'static str> {
    let last = last
        .filter(|last| last.grandmaster == from)
        .ok_or("old_mapping_unavailable")?;
    let age = host_now - last.host_ns;
    if age < 0 || age > HANDOFF_MAX_OLD_AGE.as_nanos() as i128 {
        return Err("old_mapping_too_old");
    }
    let old_now = last.remote_ns + age;
    // A time on the old timeline can be unconvertible on the new one (it
    // may lie before boot there). Search outward for a convertible start,
    // then correct the guess with the clock's own conversion.
    let in_range = |value: i128| (0..=i128::from(i64::MAX)).contains(&value);
    let mut start = convert(old_now).ok().map(|result| (old_now, result));
    if start.is_none() {
        'search: for exponent in SEARCH_EXPONENTS {
            for direction in [1i128, -1] {
                let candidate = old_now + direction * (1i128 << exponent);
                if !in_range(candidate) {
                    continue;
                }
                if let Ok(result) = convert(candidate) {
                    start = Some((candidate, result));
                    break 'search;
                }
            }
        }
    }
    let (mut guess, mut result) = start.ok_or("no_convertible_time")?;
    for _ in 0..MAX_CONVERGENCE_STEPS {
        let (host, grandmaster) = result;
        if grandmaster == 0 || grandmaster == from {
            return Err("no_grandmaster_change");
        }
        if grandmaster == own_identity {
            return Err("self_grandmaster");
        }
        let error = host - host_now;
        if error.abs() <= CONVERGED_NS {
            return Ok(PlatformHandoff {
                grandmaster,
                delta_ns: (guess - error) - old_now,
            });
        }
        guess -= error;
        if !in_range(guess) {
            return Err("rebase_out_of_range");
        }
        result = convert(guess)?;
    }
    Err("did_not_converge")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::media::ptp::{FakePlatform, PtpClockSource};
    use std::sync::Arc;

    const MAC_GM: u64 = 0x5273_7efc_a4c2_0008;
    const HOMEPOD_GM: u64 = 0xf4cb_e772_eb37_0008;
    /// Mac and HomePod grandmaster times differ by ~1.86e6 s (05:34Z log).
    const MAC_BASE_NS: u64 = 1_910_362_000_000_000;
    const HOMEPOD_BASE_NS: u64 = 49_072_000_000_000;
    const SYNC: Duration = Duration::from_millis(125);
    const RATE: u32 = 44_100;

    fn mac() -> IpAddr {
        "10.130.32.13".parse().unwrap()
    }

    fn homepod() -> IpAddr {
        "10.130.32.243".parse().unwrap()
    }

    /// A grandmaster timeline: remote = base + (local - epoch) * (1 + ppm).
    #[derive(Clone, Copy)]
    struct Timeline {
        grandmaster: u64,
        base_ns: u64,
        ppm: i64,
    }

    impl Timeline {
        fn remote_at(self, epoch: Instant, local: Instant) -> u64 {
            let elapsed = local.duration_since(epoch).as_nanos() as i128;
            (i128::from(self.base_ns) + elapsed + elapsed * i128::from(self.ppm) / 1_000_000) as u64
        }
    }

    fn sample(peer: IpAddr, timeline: Timeline, epoch: Instant, at: Instant) -> PtpClockSample {
        PtpClockSample {
            peer,
            grandmaster: timeline.grandmaster,
            remote_time_ns: timeline.remote_at(epoch, at),
            received_at: at,
            telemetry_token: crate::telemetry::operation_token(),
        }
    }

    fn session(source: PtpClockSource, epoch: Instant) -> SessionPtpClock {
        SessionPtpClock::new(source, mac(), epoch, MediaKind::Buffered)
    }

    fn anchor_at(timeline: Timeline, epoch: Instant, local: Instant, rtp: u32) -> PlaybackAnchor {
        PlaybackAnchor {
            timeline_id: timeline.grandmaster,
            remote_time_ns: timeline.remote_at(epoch, local),
            rtp_time: rtp,
            telemetry_token: crate::telemetry::operation_token(),
        }
    }

    /// The local instant at which `rtp` should play: anchor plus elapsed
    /// frames, i.e. the continuous playback timeline.
    fn expected_play(anchor_local: Instant, frames: u32) -> Instant {
        anchor_local + Duration::from_nanos(u64::from(frames) * 1_000_000_000 / u64::from(RATE))
    }

    fn assert_close(actual: Instant, expected: Instant, tolerance: Duration) {
        let diff = if actual > expected {
            actual - expected
        } else {
            expected - actual
        };
        assert!(diff <= tolerance, "deadline off by {diff:?}");
    }

    struct Run {
        clock: SessionPtpClock,
        anchor: PlaybackAnchor,
        anchor_local: Instant,
        epoch: Instant,
        now: Instant,
    }

    impl Run {
        /// Session on the Mac's timeline with a playing anchor and a ready
        /// mapper (both sources share the session's local epoch).
        fn start(source: PtpClockSource, timeline: Timeline) -> Self {
            let epoch = Instant::now();
            let mut clock = session(source, epoch);
            let mut now = epoch + Duration::from_millis(10);
            for _ in 0..4 {
                clock.observe_sample(sample(mac(), timeline, epoch, now), now, None);
                now += SYNC;
            }
            let anchor_local = now + Duration::from_millis(500);
            let mut anchor = anchor_at(timeline, epoch, anchor_local, 1_000);
            clock.install_anchor(&mut anchor, now);
            Run {
                clock,
                anchor,
                anchor_local,
                epoch,
                now,
            }
        }

        /// Feed Syncs from `peers` for `count` intervals.
        fn feed(&mut self, peers: &[(IpAddr, Timeline)], count: usize) {
            for _ in 0..count {
                for (peer, timeline) in peers {
                    let sample = sample(*peer, *timeline, self.epoch, self.now);
                    self.clock
                        .observe_sample(sample, self.now, Some(&mut self.anchor));
                }
                self.now += SYNC;
            }
        }

        fn deadline(&mut self, frames: u32) -> Result<Instant, PtpClockError> {
            let rtp = self.anchor.rtp_time.wrapping_add(frames);
            self.clock
                .deadline(
                    self.now,
                    &mut self.anchor,
                    rtp,
                    RATE,
                    crate::telemetry::operation_token(),
                )
                .expect("mapper installed")
                .1
        }

        /// Frames elapsed between the original anchor and now (+ lead).
        fn frames_at(&self, lead: Duration) -> u32 {
            let elapsed = (self.now + lead).duration_since(self.anchor_local);
            (elapsed.as_nanos() * u128::from(RATE) / 1_000_000_000) as u32
        }
    }

    fn mac_timeline() -> Timeline {
        Timeline {
            grandmaster: MAC_GM,
            base_ns: MAC_BASE_NS,
            ppm: 0,
        }
    }

    fn homepod_timeline() -> Timeline {
        Timeline {
            grandmaster: HOMEPOD_GM,
            base_ns: HOMEPOD_BASE_NS,
            ppm: 8,
        }
    }

    #[test]
    fn mac_to_homepod_without_a_new_anchor_rebases_through_local_time() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source.clone(), mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 8);
        let frames = run.frames_at(Duration::from_millis(100));
        let before = run.deadline(frames).unwrap();
        assert_close(
            before,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(50),
        );

        // SETPEERS adds the HomePod; the Mac then relays the HomePod's time.
        source.session().unwrap().replace_peers(vec![homepod()]);
        run.feed(&[(mac(), homepod_timeline())], 4);
        assert_eq!(
            run.anchor.timeline_id, HOMEPOD_GM,
            "anchor follows the new grandmaster"
        );
        assert_eq!(run.clock.grandmaster(), Some(HOMEPOD_GM));
        // Still continuous: the same RTP plays at the same local time.
        let frames = run.frames_at(Duration::from_millis(100));
        let after = run.deadline(frames).unwrap();
        assert_close(
            after,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(200),
        );
        // Long after the old mapper would have gone stale.
        run.feed(&[(mac(), homepod_timeline())], 40);
        let frames = run.frames_at(Duration::from_millis(100));
        let late = run.deadline(frames).unwrap();
        // The HomePod runs 8 ppm fast; over ~6 s that is <= ~50 us.
        assert_close(
            late,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(300),
        );
        assert!(run.clock.take_fatal(run.now, true).is_none());
    }

    #[test]
    fn homepod_back_to_mac_without_a_new_anchor_also_rebases() {
        let source = PtpClockSource::test_session_source(mac());
        source.session().unwrap().replace_peers(vec![homepod()]);
        let mut run = Run::start(source.clone(), homepod_timeline());
        run.feed(&[(mac(), homepod_timeline())], 6);
        // Group back to the Mac alone (SETPEERS with the Mac's addresses).
        source.session().unwrap().replace_peers(vec![]);
        run.feed(&[(mac(), mac_timeline())], 5);
        assert_eq!(run.anchor.timeline_id, MAC_GM);
        let frames = run.frames_at(Duration::from_millis(100));
        let after = run.deadline(frames).unwrap();
        assert_close(
            after,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(200),
        );
    }

    #[test]
    fn round_trip_keeps_one_continuous_timeline() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source.clone(), mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        source.session().unwrap().replace_peers(vec![homepod()]);
        run.feed(&[(mac(), homepod_timeline())], 8);
        source.session().unwrap().replace_peers(vec![]);
        run.feed(&[(mac(), mac_timeline())], 8);
        assert_eq!(run.anchor.timeline_id, MAC_GM);
        let frames = run.frames_at(Duration::from_millis(100));
        let deadline = run.deadline(frames).unwrap();
        assert_close(
            deadline,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(300),
        );
    }

    #[test]
    fn new_anchor_on_the_new_timeline_promotes_the_warm_mapper() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source.clone(), mac_timeline());
        source.session().unwrap().replace_peers(vec![homepod()]);
        // The HomePod sends to us directly while the Mac still relays the
        // old grandmaster: a warm candidate builds up without a handoff.
        run.feed(
            &[(mac(), mac_timeline()), (homepod(), homepod_timeline())],
            4,
        );
        assert_eq!(run.clock.grandmaster(), Some(MAC_GM));
        // iPhone-style: a fresh anchor on the HomePod timeline.
        let anchor_local = run.now + Duration::from_millis(400);
        let mut anchor = anchor_at(homepod_timeline(), run.epoch, anchor_local, 77);
        run.clock.install_anchor(&mut anchor, run.now);
        assert_eq!(anchor.timeline_id, HOMEPOD_GM);
        run.anchor = anchor;
        run.anchor_local = anchor_local;
        // Ready immediately: no three-sample warm-up after the switch.
        let deadline = run.deadline(441).unwrap();
        assert_close(
            deadline,
            expected_play(anchor_local, 441),
            Duration::from_micros(200),
        );
    }

    #[test]
    fn new_anchor_after_a_rebase_wins_and_old_labels_are_translated() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source.clone(), mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        source.session().unwrap().replace_peers(vec![homepod()]);
        run.feed(&[(mac(), homepod_timeline())], 4);
        assert_eq!(run.anchor.timeline_id, HOMEPOD_GM);
        // A late anchor still labelled with the Mac timeline is translated.
        let anchor_local = run.now + Duration::from_millis(300);
        let mut stale_label = anchor_at(mac_timeline(), run.epoch, anchor_local, 5);
        run.clock.install_anchor(&mut stale_label, run.now);
        assert_eq!(stale_label.timeline_id, HOMEPOD_GM);
        run.anchor = stale_label;
        run.anchor_local = anchor_local;
        let deadline = run.deadline(0).unwrap();
        assert_close(deadline, anchor_local, Duration::from_micros(200));
    }

    #[test]
    fn samples_from_unlisted_peers_are_ignored_until_setpeers_lists_them() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source.clone(), mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        // The Mac stops relaying; an unlisted HomePod sends its time.
        run.feed(&[(homepod(), homepod_timeline())], 20);
        assert_eq!(
            run.clock.grandmaster(),
            Some(MAC_GM),
            "unlisted peer never maps"
        );
        assert!(run.deadline(0).is_err());
        // Once listed, the silent source is replaced by the HomePod.
        source.session().unwrap().replace_peers(vec![homepod()]);
        run.feed(&[(homepod(), homepod_timeline())], 4);
        assert_eq!(run.clock.grandmaster(), Some(HOMEPOD_GM));
        let frames = run.frames_at(Duration::from_millis(100));
        // The old mapping was last valid ~3 s ago (<= 5 s): continuous.
        let deadline = run.deadline(frames).unwrap();
        assert_close(
            deadline,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(300),
        );
    }

    #[test]
    fn same_grandmaster_from_another_listed_peer_switches_without_rebase() {
        let source = PtpClockSource::test_session_source(mac());
        source.session().unwrap().replace_peers(vec![homepod()]);
        let mut run = Run::start(source.clone(), homepod_timeline());
        run.feed(
            &[(mac(), homepod_timeline()), (homepod(), homepod_timeline())],
            4,
        );
        let remote_before = run.anchor.remote_time_ns;
        // The relay stops; the HomePod's direct path continues.
        run.feed(&[(homepod(), homepod_timeline())], 10);
        assert_eq!(run.anchor.remote_time_ns, remote_before);
        assert_eq!(run.anchor.timeline_id, HOMEPOD_GM);
        let frames = run.frames_at(Duration::from_millis(100));
        let deadline = run.deadline(frames).unwrap();
        assert_close(
            deadline,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(200),
        );
    }

    #[test]
    fn plain_silence_is_held_and_never_fatal() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source, mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        assert!(run.deadline(0).is_ok());
        run.now += Duration::from_secs(20);
        assert_eq!(run.deadline(0), Err(PtpClockError::Stale));
        assert!(run.clock.take_fatal(run.now, true).is_none());
    }

    #[test]
    fn unfollowable_change_fails_fast_after_the_grace() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source, mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        assert!(run.deadline(0).is_ok());
        // The Mac announces a new grandmaster, relays two Syncs and then
        // nothing usable: the new mapping never becomes ready.
        run.feed(&[(mac(), homepod_timeline())], 2);
        run.now += Duration::from_secs(2);
        assert!(run.deadline(0).is_err());
        assert!(
            run.clock.take_fatal(run.now, true).is_none(),
            "inside the grace"
        );
        run.now += Duration::from_secs(3);
        let fatal = run.clock.take_fatal(run.now, true).expect("grace expired");
        assert_eq!(fatal.kind(), io::ErrorKind::TimedOut);
        assert!(
            run.clock.take_fatal(run.now, true).is_none(),
            "reported once"
        );
    }

    #[test]
    fn inconsistent_first_samples_on_the_new_grandmaster_delay_but_do_not_corrupt_the_handoff() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source, mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        // Two Syncs already labelled with the new grandmaster still carry the
        // old timeline (Announce and Sync switch at slightly different times).
        for _ in 0..2 {
            let mut early = sample(mac(), homepod_timeline(), run.epoch, run.now);
            early.remote_time_ns = mac_timeline().remote_at(run.epoch, run.now);
            run.clock
                .observe_sample(early, run.now, Some(&mut run.anchor));
            run.now += SYNC;
        }
        run.feed(&[(mac(), homepod_timeline())], 30);
        assert_eq!(run.anchor.timeline_id, HOMEPOD_GM);
        let frames = run.frames_at(Duration::from_millis(100));
        let deadline = run.deadline(frames).unwrap();
        assert_close(
            deadline,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(300),
        );
    }

    #[test]
    fn change_while_paused_is_not_fatal() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source, mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        assert!(run.deadline(0).is_ok());
        run.feed(&[(mac(), homepod_timeline())], 1);
        run.now += Duration::from_secs(10);
        assert!(run.clock.take_fatal(run.now, false).is_none());
    }

    #[test]
    fn transient_foreign_grandmaster_from_the_source_does_not_hand_off() {
        let source = PtpClockSource::test_session_source(mac());
        let mut run = Run::start(source, mac_timeline());
        run.feed(&[(mac(), mac_timeline())], 4);
        run.feed(&[(mac(), homepod_timeline())], 1);
        run.feed(&[(mac(), mac_timeline())], 4);
        assert_eq!(run.anchor.timeline_id, MAC_GM);
        assert_eq!(run.clock.grandmaster(), Some(MAC_GM));
        assert!(run.deadline(0).is_ok());
        run.now += HANDOFF_FAIL_GRACE * 2;
        run.feed(&[(mac(), mac_timeline())], 4);
        assert!(run.clock.take_fatal(run.now, true).is_none());
    }

    #[test]
    fn multiple_listed_peers_prefer_the_sources_new_grandmaster() {
        let other: IpAddr = "10.130.32.107".parse().unwrap();
        let other_timeline = Timeline {
            grandmaster: 0x141b_a086_932e_0008,
            base_ns: 7_000_000_000_000,
            ppm: -3,
        };
        let source = PtpClockSource::test_session_source(mac());
        source
            .session()
            .unwrap()
            .replace_peers(vec![homepod(), other, mac()]);
        let mut run = Run::start(source.clone(), mac_timeline());
        run.feed(
            &[
                (mac(), mac_timeline()),
                (other, other_timeline),
                (homepod(), homepod_timeline()),
            ],
            4,
        );
        assert_eq!(
            run.clock.grandmaster(),
            Some(MAC_GM),
            "healthy source is kept"
        );
        run.feed(
            &[
                (mac(), homepod_timeline()),
                (other, other_timeline),
                (homepod(), homepod_timeline()),
            ],
            4,
        );
        assert_eq!(run.clock.grandmaster(), Some(HOMEPOD_GM));
        let frames = run.frames_at(Duration::from_millis(100));
        let deadline = run.deadline(frames).unwrap();
        assert_close(
            deadline,
            expected_play(run.anchor_local, frames),
            Duration::from_micros(300),
        );
        // The control peer is accepted but never part of the shared list.
        assert!(!source.session().unwrap().peers().1.contains(&mac()));
    }

    // -------------------------------------------- macOS backends (fake)

    #[test]
    fn platform_handoff_rebases_both_directions() {
        let epoch = Instant::now();
        let fake = Arc::new(FakePlatform::new(epoch, MAC_GM, 0x00aa_bbcc_ddee_0001));
        fake.lock(MAC_GM, -(MAC_BASE_NS as i128));
        let source = PtpClockSource::test_platform_source(mac(), Arc::clone(&fake));
        let mut clock = session(source, epoch);
        let anchor_local = epoch + Duration::from_secs(1);
        let mut anchor = anchor_at(mac_timeline(), epoch, anchor_local, 0);
        clock.install_anchor(&mut anchor, epoch);
        let token = crate::telemetry::operation_token();
        let now = epoch + Duration::from_millis(500);
        let (source_kind, result) = clock
            .deadline(now, &mut anchor, 44_100, RATE, token)
            .unwrap();
        assert_eq!(source_kind, DeadlineSource::Platform);
        assert_close(
            result.unwrap(),
            anchor_local + Duration::from_secs(1),
            Duration::from_micros(1),
        );

        // Mac -> HomePod: the instance follows the new grandmaster.
        fake.lock(HOMEPOD_GM, -(HOMEPOD_BASE_NS as i128));
        let (_, result) = clock
            .deadline(now, &mut anchor, 44_100, RATE, token)
            .unwrap();
        assert_eq!(anchor.timeline_id, HOMEPOD_GM);
        assert_close(
            result.unwrap(),
            anchor_local + Duration::from_secs(1),
            Duration::from_micros(1),
        );

        // HomePod -> Mac.
        fake.lock(MAC_GM, -(MAC_BASE_NS as i128));
        let (_, result) = clock
            .deadline(now, &mut anchor, 88_200, RATE, token)
            .unwrap();
        assert_eq!(anchor.timeline_id, MAC_GM);
        assert_close(
            result.unwrap(),
            anchor_local + Duration::from_secs(2),
            Duration::from_micros(1),
        );
    }

    #[test]
    fn platform_self_grandmaster_is_never_mapped_and_fails_after_grace() {
        let epoch = Instant::now();
        let own = 0x00aa_bbcc_ddee_0001;
        let fake = Arc::new(FakePlatform::new(epoch, MAC_GM, own));
        fake.lock(MAC_GM, -(MAC_BASE_NS as i128));
        let source = PtpClockSource::test_platform_source(mac(), Arc::clone(&fake));
        let mut clock = session(source, epoch);
        let mut anchor = anchor_at(mac_timeline(), epoch, epoch + Duration::from_secs(1), 0);
        clock.install_anchor(&mut anchor, epoch);
        let token = crate::telemetry::operation_token();
        let now = epoch + Duration::from_millis(100);
        assert!(clock
            .deadline(now, &mut anchor, 0, RATE, token)
            .unwrap()
            .1
            .is_ok());
        // The grandmaster disappears: the instance elects itself.
        fake.lock(own, 0);
        let (_, result) = clock.deadline(now, &mut anchor, 0, RATE, token).unwrap();
        assert_eq!(result, Err(PtpClockError::NotReady));
        assert_eq!(
            anchor.timeline_id, MAC_GM,
            "never rebased onto the own clock"
        );
        // A foreign GM that has no retired mapping for ours cannot be followed.
        fake.forget_retired();
        fake.lock(HOMEPOD_GM, -(HOMEPOD_BASE_NS as i128));
        fake.forget_retired();
        let (_, result) = clock.deadline(now, &mut anchor, 0, RATE, token).unwrap();
        assert_eq!(result, Err(PtpClockError::SourceMismatch));
        assert!(clock
            .take_fatal(now + Duration::from_secs(1), true)
            .is_none());
        assert!(clock
            .take_fatal(now + HANDOFF_FAIL_GRACE + Duration::from_millis(1), true)
            .is_some());
    }

    #[test]
    fn platform_mismatch_before_the_first_mapping_is_held_without_failure() {
        let epoch = Instant::now();
        let fake = Arc::new(FakePlatform::new(epoch, HOMEPOD_GM, 1));
        fake.lock(HOMEPOD_GM, -(HOMEPOD_BASE_NS as i128));
        fake.forget_retired();
        let source = PtpClockSource::test_platform_source(mac(), Arc::clone(&fake));
        let mut clock = session(source, epoch);
        let mut anchor = anchor_at(mac_timeline(), epoch, epoch + Duration::from_secs(1), 0);
        clock.install_anchor(&mut anchor, epoch);
        let token = crate::telemetry::operation_token();
        let (_, result) = clock.deadline(epoch, &mut anchor, 0, RATE, token).unwrap();
        assert_eq!(result, Err(PtpClockError::SourceMismatch));
        assert!(clock
            .take_fatal(epoch + Duration::from_secs(60), true)
            .is_none());
    }

    #[test]
    fn clock_handoff_converges_from_far_timelines_in_both_directions() {
        // Fake CoreMedia: host = (remote - base_new) * (1 + ppm) + host0.
        for (old_base, new_base, ppm) in [
            (MAC_BASE_NS as i128, HOMEPOD_BASE_NS as i128, 8i128),
            (HOMEPOD_BASE_NS as i128, MAC_BASE_NS as i128, -8i128),
        ] {
            let host_now: i128 = 5_000_000_000_000;
            let last = LastMapping {
                grandmaster: MAC_GM,
                remote_ns: old_base,
                host_ns: host_now - 1_000_000_000,
            };
            let new_at_host_now = new_base + 123_456_789;
            let convert = |remote: i128| -> Result<(i128, u64), &'static str> {
                let delta = remote - new_at_host_now;
                Ok((host_now + delta + delta * ppm / 1_000_000, HOMEPOD_GM))
            };
            let handoff = converge_clock_handoff(MAC_GM, 7, Some(last), host_now, convert).unwrap();
            assert_eq!(handoff.grandmaster, HOMEPOD_GM);
            let old_now = old_base + 1_000_000_000;
            let error = (old_now + handoff.delta_ns) - new_at_host_now;
            assert!(error.abs() <= 1_000_000, "error {error} ns");
        }
    }

    #[test]
    fn clock_handoff_searches_when_the_old_time_is_before_boot_on_the_new_timeline() {
        // HomePod -> Mac on CoreMedia: the HomePod time (~49e12 ns) is 1.86e6 s
        // before "now" on the Mac's timeline, i.e. before this host booted.
        let host_now: i128 = 37_000_000_000_000; // ~10 h of uptime
        let last = LastMapping {
            grandmaster: HOMEPOD_GM,
            remote_ns: HOMEPOD_BASE_NS as i128,
            host_ns: host_now - 500_000_000,
        };
        let mac_now = MAC_BASE_NS as i128 + 777;
        let mut calls = 0;
        let convert = |remote: i128| -> Result<(i128, u64), &'static str> {
            calls += 1;
            let host = host_now + (remote - mac_now) + (remote - mac_now) * 3 / 1_000_000;
            if host < 0 {
                return Err("conversion_failed");
            }
            Ok((host, MAC_GM))
        };
        let handoff = converge_clock_handoff(HOMEPOD_GM, 7, Some(last), host_now, convert).unwrap();
        assert_eq!(handoff.grandmaster, MAC_GM);
        let old_now = HOMEPOD_BASE_NS as i128 + 500_000_000;
        assert!(((old_now + handoff.delta_ns) - mac_now).abs() <= 1_000_000);
        assert!(calls < 80, "{calls} conversions");
        // Nothing convertible at all: an explicit failure, not a hang.
        let never = |_: i128| -> Result<(i128, u64), &'static str> { Err("conversion_failed") };
        assert_eq!(
            converge_clock_handoff(HOMEPOD_GM, 7, Some(last), host_now, never),
            Err("no_convertible_time")
        );
    }

    #[test]
    fn clock_handoff_refuses_self_unchanged_and_stale_mappings() {
        let host_now: i128 = 10_000_000_000;
        let last = Some(LastMapping {
            grandmaster: MAC_GM,
            remote_ns: 1_000,
            host_ns: host_now,
        });
        let same = |_: i128| Ok((host_now, MAC_GM));
        assert_eq!(
            converge_clock_handoff(MAC_GM, 7, last, host_now, same),
            Err("no_grandmaster_change")
        );
        let own = |_: i128| Ok((host_now, 7));
        assert_eq!(
            converge_clock_handoff(MAC_GM, 7, last, host_now, own),
            Err("self_grandmaster")
        );
        let any = |_: i128| Ok((host_now, HOMEPOD_GM));
        assert_eq!(
            converge_clock_handoff(MAC_GM, 7, None, host_now, any),
            Err("old_mapping_unavailable")
        );
        assert_eq!(
            converge_clock_handoff(MAC_GM, 7, last, host_now + 6_000_000_000, any),
            Err("old_mapping_too_old")
        );
    }

    #[test]
    fn shared_peer_set_reports_changes_and_never_lists_the_control_peer() {
        let shared = SessionShared::new(mac());
        let first = shared.replace_peers(vec![homepod(), mac(), homepod()]);
        assert_eq!(first.peers, vec![homepod()]);
        assert_eq!(first.added, vec![homepod()]);
        let other: IpAddr = "fe80::1".parse().unwrap();
        let second = shared.replace_peers(vec![other]);
        assert_eq!(second.added, vec![other]);
        assert_eq!(second.removed, vec![homepod()]);
        assert_eq!(shared.peers().0, 2);
        assert_eq!(shift_remote(10, -11), None);
        assert_eq!(shift_remote(10, -10), Some(0));
    }
}
