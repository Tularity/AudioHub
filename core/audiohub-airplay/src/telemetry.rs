use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use uuid::Uuid;

const TELEMETRY_SCHEMA_VERSION: u32 = 3;
const RECENT_EVENT_LIMIT: usize = 128;
const CONTROL_SESSION_HISTORY_LIMIT: usize = 128;

/// Cumulative packet-stage counters for one negotiated AirPlay media profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTelemetryCounters {
    pub packets_received: u64,
    pub packets_decrypted: u64,
    pub decrypt_failures: u64,
    pub packets_admitted: u64,
    pub admission_rejections_total: u64,
    pub admission_rejections_replay: u64,
    pub admission_rejections_limit: u64,
    pub admission_rejections_parse: u64,
    pub admission_rejections_profile: u64,
    pub admission_rejections_sequence: u64,
    pub admission_rejections_timestamp: u64,
    pub admission_rejections_timeline: u64,
    pub admission_rejections_source_ip: u64,
    pub admission_rejections_source_endpoint: u64,
    pub admission_rejections_packet_too_large: u64,
    pub admission_rejections_other: u64,
    pub jitter_rejections_total: u64,
    pub jitter_rejections_duplicate: u64,
    pub jitter_rejections_too_old: u64,
    pub jitter_rejections_outside_window: u64,
    pub jitter_rejections_full: u64,
    pub jitter_rejections_sequence_exhausted: u64,
    pub jitter_rejections_invalid_limits: u64,
    pub jitter_rejections_empty_payload: u64,
    pub jitter_rejections_payload_too_large: u64,
    pub jitter_rejections_invalid_advance: u64,
    pub jitter_rejections_other: u64,
    pub jitter_evictions_far_future: u64,
    pub packets_decoded: u64,
    pub decode_failures: u64,
    pub output_writes: u64,
    pub output_samples: u64,
    pub future_safety_waits: u64,
    pub scheduler_late_drops: u64,
    pub predecode_late_drops: u64,
    pub deadline_rejections_total: u64,
    /// Rejections produced by the platform clock path. This independently
    /// sampled monotonic counter preserves source attribution; readers must not
    /// require an atomic equality with `deadline_rejections_total` while media
    /// is active.
    pub deadline_rejections_platform_total: u64,
    /// Rejections produced by the portable PTP mapper path.
    pub deadline_rejections_portable_mapper_total: u64,
    pub deadline_rejections_generation_mismatch: u64,
    pub deadline_rejections_source_mismatch: u64,
    pub deadline_rejections_non_monotonic_local_time: u64,
    pub deadline_rejections_offset_jump: u64,
    pub deadline_rejections_not_ready: u64,
    pub deadline_rejections_stale: u64,
    pub deadline_rejections_invalid_sample_rate: u64,
    pub deadline_rejections_ambiguous_rtp_delta: u64,
    pub deadline_rejections_mapped_time_out_of_range: u64,
    pub deadline_rejections_platform_clock_unavailable: u64,
    pub deadline_rejections_other: u64,
    /// Non-failing control/scheduler dispositions, derived from the reason counters below.
    pub expected_dispositions_total: u64,
    /// First bounded `NotReady` wait for a frame, derived from the two source
    /// counters below. Repeated polls for that frame do not increment it.
    pub deadline_warmup_waits_total: u64,
    pub deadline_warmup_waits_platform: u64,
    pub deadline_warmup_waits_portable_mapper: u64,
    pub ptp_broadcast_lag_events: u64,
    pub ptp_broadcast_lag_samples: u64,
    pub ptp_cross_generation_discard_events: u64,
    pub ptp_cross_generation_discard_samples: u64,
    pub expected_control_flushes: u64,
    pub expected_control_flush_drops: u64,
    pub expected_deferred_flushes: u64,
    pub expected_deferred_flush_drops: u64,
    pub scheduler_waits_missing_anchor: u64,
    pub scheduler_waits_missing_mapper: u64,
    /// Rejected control operations, derived from the reason counters below.
    pub control_failures_total: u64,
    pub control_failures_invalid_anchor: u64,
    pub control_failures_invalid_flush: u64,
    pub control_failures_deferred_limit: u64,
}

/// Receiver diagnostics since the latest explicit reset. Numeric fields are
/// cumulative unless their name ends in `_gauge`; gauges are point-in-time
/// derived values and may decrease without a reset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AirPlayTelemetryCounters {
    pub rtsp_phase2_requests: u64,
    pub rtsp_phase2_prepared: u64,
    pub rtsp_phase2_committed_accepted: u64,
    /// Point-in-time prepared requests without a terminal accept/failure.
    /// Gauges may decrease and must never be interpreted as reset deltas.
    pub rtsp_phase2_prepared_inflight_gauge: u64,
    pub rtsp_phase2_accepted: u64,
    /// Terminal socket/commit failures across the phase-two response path.
    /// Use the prepared-socket subset plus commit failures to close the
    /// status-200 prepared gauge.
    pub rtsp_phase2_terminal_failures_total: u64,
    pub rtsp_phase2_socket_write_failures_total: u64,
    /// Subset of socket-write failures whose prepared RTSP status was 200.
    pub rtsp_phase2_prepared_socket_write_failures_total: u64,
    pub rtsp_phase2_commit_failures_total: u64,
    pub rtsp_phase2_failures_total: u64,
    pub rtsp_phase2_failures_400: u64,
    pub rtsp_phase2_failures_453: u64,
    pub rtsp_phase2_failures_455: u64,
    pub rtsp_phase2_failures_500: u64,
    pub rtsp_phase2_failures_501: u64,
    pub rtsp_phase2_failures_other_status: u64,
    pub rtsp_phase2_active_media_rejected_453: u64,
    pub timing_peer_info_observed: u64,
    pub peer_list_update_requests: u64,
    pub peer_list_update_setpeers_requests: u64,
    pub peer_list_update_setpeersx_requests: u64,
    pub peer_list_update_parsed: u64,
    pub peer_list_update_parse_rejected_total: u64,
    pub peer_list_update_parse_rejected_content_type: u64,
    pub peer_list_update_parse_rejected_empty_body: u64,
    pub peer_list_update_parse_rejected_plist: u64,
    pub peer_list_update_parse_rejected_shape: u64,
    pub peer_list_update_response_committed_200: u64,
    pub peer_list_update_socket_write_failures_total: u64,
    pub peer_list_update_commit_failures_total: u64,
    pub peer_list_update_applied_to_session_clock: u64,
    pub peer_list_update_acknowledged_but_ignored: u64,
    /// Point-in-time prepared requests without a committed response or terminal
    /// socket/commit failure. This is a gauge, not a cumulative counter.
    pub peer_list_update_prepared_inflight_gauge: u64,
    pub ptp_datagrams_received: u64,
    pub ptp_datagrams_accepted: u64,
    pub ptp_datagrams_rejected_total: u64,
    pub ptp_datagrams_rejected_source_port: u64,
    pub ptp_datagrams_rejected_parse: u64,
    pub ptp_datagrams_rejected_message_port: u64,
    pub ptp_datagrams_rejected_other: u64,
    pub ptp_sync_rejected_missing_announce: u64,
    pub ptp_samples_emitted: u64,
    pub ptp_mapper_selected: u64,
    pub ptp_mapper_samples_accepted: u64,
    pub ptp_mapper_samples_rejected: u64,
    pub ptp_mapper_ready: u64,
    pub ptp_failures_total: u64,
    pub ptp_pending_sync_evictions: u64,
    pub ptp_pending_follow_up_evictions: u64,
    pub ptp_pending_sync_expirations: u64,
    pub ptp_pending_follow_up_expirations: u64,
    pub ptp_pair_expirations: u64,
    pub ptp_invalid_time_conversions: u64,
    pub ptp_samples_without_subscriber: u64,
    pub ptp_mapper_pending_sample_evictions: u64,
    pub ptp_mapper_pending_sample_discards: u64,
    pub ptp_announcement_evictions: u64,
    pub ptp_expected_dispositions_total: u64,
    pub ptp_completed_pair_evictions: u64,
    pub ptp_completed_pair_expirations: u64,
    pub ptp_announcement_expirations: u64,
    pub realtime: MediaTelemetryCounters,
    pub buffered: MediaTelemetryCounters,
    pub pcm_bus_pushes_accepted: u64,
    pub pcm_bus_pushes_owner_mismatch: u64,
    pub pcm_bus_samples_accepted: u64,
    pub pcm_bus_failures_total: u64,
    pub pcm_bus_lock_contention_reads: u64,
    pub pcm_bus_lock_contention_samples: u64,
    pub pcm_bus_underrun_reads: u64,
    pub pcm_bus_underrun_samples: u64,
    pub pcm_bus_overflow_events: u64,
    pub pcm_bus_overflow_samples: u64,
    pub pcm_bus_reader_dropped_samples: u64,
    pub pcm_bus_reader_drop_events: u64,
    pub pcm_bus_read_calls: u64,
    pub pcm_bus_output_samples: u64,
    pub pcm_bus_silence_samples: u64,
}

/// Bounded, deduplicated observations that retain addresses and rejection reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AirPlayTelemetryEvent {
    RtspPhase2 {
        sequence: u64,
        peer: String,
        connection_id: String,
        stream_type: Option<u64>,
        outcome: String,
        status: u16,
        reason: String,
    },
    TimingPeerSelection {
        sequence: u64,
        control_peer: String,
        connection_id: String,
        timing_peer_id: String,
        advertised_addresses: Vec<String>,
        receive_match_clock_identity: Option<String>,
        receive_match_port: Option<u16>,
        mapper_peer: String,
        mapper_peer_in_advertised_addresses: bool,
        timing_peer_list_ids: Vec<String>,
        timing_peer_info_in_list: bool,
    },
    PeerListUpdate {
        sequence: u64,
        control_peer: String,
        connection_id: String,
        method: String,
        content_type: Option<String>,
        response_status: u16,
        outcome: String,
        reason: String,
        peer_count: u64,
        address_count: u64,
        peer_ids: Vec<String>,
        addresses: Vec<String>,
        clock_identity_count: u64,
        clock_identities: Vec<String>,
        clock_port_count: u64,
        local_clock_port_matches: u64,
        context_truncated: bool,
    },
    PtpDatagram {
        sequence: u64,
        peer: String,
        source_port: u16,
        local_port: u16,
        message: Option<String>,
        outcome: String,
        reason: String,
    },
    PtpMapper {
        sequence: u64,
        selected_peer: String,
        sample_peer: Option<String>,
        grandmaster: String,
        outcome: String,
        reason: String,
    },
    PtpDeadlineRejected {
        sequence: u64,
        media_profile: String,
        clock_source: String,
        reason: String,
    },
    MediaFailure {
        sequence: u64,
        media_profile: String,
        stage: String,
        outcome: String,
        reason: String,
        packets: u64,
    },
    PcmBusOwnerMismatch {
        sequence: u64,
        attempted_session: u64,
        active_session: Option<u64>,
        samples: u64,
    },
    PcmBusFailure {
        sequence: u64,
        operation: String,
        reason: String,
        samples: u64,
    },
}

/// Public schema-v3 identity for one AirPlay control connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AirPlayControlSessionIdentity {
    pub peer_ip: String,
    /// Opaque process-local identity for one accepted RTSP connection. It does
    /// not contain pairing, DACP, Active-Remote, or any other credential.
    pub connection_id: String,
}

/// Versioned diagnostics returned through the daemon's local authenticated IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AirPlayTelemetrySnapshot {
    pub schema_version: u32,
    pub reset_generation: u64,
    /// Opaque process-generated identity for this exact reset generation.
    pub generation_marker: String,
    /// Monotonic nanoseconds since this process's telemetry origin at reset.
    pub reset_monotonic_ns: u64,
    pub counters: AirPlayTelemetryCounters,
    /// Exact control connections that produced phase-two, timing-selection, or
    /// peer-list activity retained in this generation. `(peer_ip,
    /// connection_id)` is unique even for reconnects and concurrent clients
    /// behind one address.
    pub control_sessions: Vec<AirPlayControlSessionIdentity>,
    /// True once inactive history exceeded its bounded retention. Every active
    /// connection remains present in `active_control_sessions`; this flag says
    /// that `control_sessions` is no longer a complete generation history.
    pub control_sessions_truncated: bool,
    /// Control connections currently registered in this generation. A newly
    /// accepted connection may appear here before it produces any activity and
    /// therefore before it appears in `control_sessions`.
    pub active_control_sessions: Vec<AirPlayControlSessionIdentity>,
    pub recent_events: Vec<AirPlayTelemetryEvent>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MediaKind {
    Realtime,
    Buffered,
}

/// Reset generation captured at the entrance to one media or PCM operation.
///
/// The token deliberately does not pin its slot. Each later stage briefly
/// validates and pins the captured generation while updating atomics. A reset
/// therefore makes an old operation inert without waiting for a queued packet
/// or audio callback to finish, and a second reset can immediately reuse the
/// old slot once any in-flight recorder call has returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperationToken {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ConnectionId(Uuid);

impl ConnectionId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    fn text(self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadlineSource {
    Platform,
    PortableMapper,
}

#[derive(Default)]
struct MediaCounters {
    packets_received: AtomicU64,
    packets_decrypted: AtomicU64,
    decrypt_failures: AtomicU64,
    packets_admitted: AtomicU64,
    admission_rejections_total: AtomicU64,
    admission_rejections_replay: AtomicU64,
    admission_rejections_limit: AtomicU64,
    admission_rejections_parse: AtomicU64,
    admission_rejections_profile: AtomicU64,
    admission_rejections_sequence: AtomicU64,
    admission_rejections_timestamp: AtomicU64,
    admission_rejections_timeline: AtomicU64,
    admission_rejections_source_ip: AtomicU64,
    admission_rejections_source_endpoint: AtomicU64,
    admission_rejections_packet_too_large: AtomicU64,
    admission_rejections_other: AtomicU64,
    jitter_rejections_total: AtomicU64,
    jitter_rejections_duplicate: AtomicU64,
    jitter_rejections_too_old: AtomicU64,
    jitter_rejections_outside_window: AtomicU64,
    jitter_rejections_full: AtomicU64,
    jitter_rejections_sequence_exhausted: AtomicU64,
    jitter_rejections_invalid_limits: AtomicU64,
    jitter_rejections_empty_payload: AtomicU64,
    jitter_rejections_payload_too_large: AtomicU64,
    jitter_rejections_invalid_advance: AtomicU64,
    jitter_rejections_other: AtomicU64,
    jitter_evictions_far_future: AtomicU64,
    packets_decoded: AtomicU64,
    decode_failures: AtomicU64,
    output_writes: AtomicU64,
    output_samples: AtomicU64,
    future_safety_waits: AtomicU64,
    scheduler_late_drops: AtomicU64,
    predecode_late_drops: AtomicU64,
    deadline_rejections_total: AtomicU64,
    deadline_rejections_platform_total: AtomicU64,
    deadline_rejections_portable_mapper_total: AtomicU64,
    deadline_rejections_generation_mismatch: AtomicU64,
    deadline_rejections_source_mismatch: AtomicU64,
    deadline_rejections_non_monotonic_local_time: AtomicU64,
    deadline_rejections_offset_jump: AtomicU64,
    deadline_rejections_not_ready: AtomicU64,
    deadline_rejections_stale: AtomicU64,
    deadline_rejections_invalid_sample_rate: AtomicU64,
    deadline_rejections_ambiguous_rtp_delta: AtomicU64,
    deadline_rejections_mapped_time_out_of_range: AtomicU64,
    deadline_rejections_platform_clock_unavailable: AtomicU64,
    deadline_rejections_other: AtomicU64,
    deadline_warmup_waits_platform: AtomicU64,
    deadline_warmup_waits_portable_mapper: AtomicU64,
    ptp_broadcast_lag_events: AtomicU64,
    ptp_broadcast_lag_samples: AtomicU64,
    ptp_cross_generation_discard_events: AtomicU64,
    ptp_cross_generation_discard_samples: AtomicU64,
    expected_control_flushes: AtomicU64,
    expected_control_flush_drops: AtomicU64,
    expected_deferred_flushes: AtomicU64,
    expected_deferred_flush_drops: AtomicU64,
    scheduler_waits_missing_anchor: AtomicU64,
    scheduler_waits_missing_mapper: AtomicU64,
    control_failures_invalid_anchor: AtomicU64,
    control_failures_invalid_flush: AtomicU64,
    control_failures_deferred_limit: AtomicU64,
}

impl MediaCounters {
    fn snapshot(&self) -> MediaTelemetryCounters {
        let admission_rejections_replay = load(&self.admission_rejections_replay);
        let admission_rejections_limit = load(&self.admission_rejections_limit);
        let admission_rejections_parse = load(&self.admission_rejections_parse);
        let admission_rejections_profile = load(&self.admission_rejections_profile);
        let admission_rejections_sequence = load(&self.admission_rejections_sequence);
        let admission_rejections_timestamp = load(&self.admission_rejections_timestamp);
        let admission_rejections_timeline = load(&self.admission_rejections_timeline);
        let admission_rejections_source_ip = load(&self.admission_rejections_source_ip);
        let admission_rejections_source_endpoint = load(&self.admission_rejections_source_endpoint);
        let admission_rejections_packet_too_large =
            load(&self.admission_rejections_packet_too_large);
        let admission_rejections_other = load(&self.admission_rejections_other);
        let admission_rejections_total = admission_rejections_replay
            .saturating_add(admission_rejections_limit)
            .saturating_add(admission_rejections_parse)
            .saturating_add(admission_rejections_profile)
            .saturating_add(admission_rejections_sequence)
            .saturating_add(admission_rejections_timestamp)
            .saturating_add(admission_rejections_timeline)
            .saturating_add(admission_rejections_source_ip)
            .saturating_add(admission_rejections_source_endpoint)
            .saturating_add(admission_rejections_packet_too_large)
            .saturating_add(admission_rejections_other);
        let jitter_rejections_duplicate = load(&self.jitter_rejections_duplicate);
        let jitter_rejections_too_old = load(&self.jitter_rejections_too_old);
        let jitter_rejections_outside_window = load(&self.jitter_rejections_outside_window);
        let jitter_rejections_full = load(&self.jitter_rejections_full);
        let jitter_rejections_sequence_exhausted = load(&self.jitter_rejections_sequence_exhausted);
        let jitter_rejections_invalid_limits = load(&self.jitter_rejections_invalid_limits);
        let jitter_rejections_empty_payload = load(&self.jitter_rejections_empty_payload);
        let jitter_rejections_payload_too_large = load(&self.jitter_rejections_payload_too_large);
        let jitter_rejections_invalid_advance = load(&self.jitter_rejections_invalid_advance);
        let jitter_rejections_other = load(&self.jitter_rejections_other);
        let jitter_evictions_far_future = load(&self.jitter_evictions_far_future);
        let jitter_rejections_total = jitter_rejections_duplicate
            .saturating_add(jitter_rejections_too_old)
            .saturating_add(jitter_rejections_outside_window)
            .saturating_add(jitter_rejections_full)
            .saturating_add(jitter_rejections_sequence_exhausted)
            .saturating_add(jitter_rejections_invalid_limits)
            .saturating_add(jitter_rejections_empty_payload)
            .saturating_add(jitter_rejections_payload_too_large)
            .saturating_add(jitter_rejections_invalid_advance)
            .saturating_add(jitter_rejections_other)
            .saturating_add(jitter_evictions_far_future);
        let deadline_rejections_generation_mismatch =
            load(&self.deadline_rejections_generation_mismatch);
        let deadline_rejections_source_mismatch = load(&self.deadline_rejections_source_mismatch);
        let deadline_rejections_non_monotonic_local_time =
            load(&self.deadline_rejections_non_monotonic_local_time);
        let deadline_rejections_offset_jump = load(&self.deadline_rejections_offset_jump);
        let deadline_rejections_not_ready = load(&self.deadline_rejections_not_ready);
        let deadline_rejections_stale = load(&self.deadline_rejections_stale);
        let deadline_rejections_invalid_sample_rate =
            load(&self.deadline_rejections_invalid_sample_rate);
        let deadline_rejections_ambiguous_rtp_delta =
            load(&self.deadline_rejections_ambiguous_rtp_delta);
        let deadline_rejections_mapped_time_out_of_range =
            load(&self.deadline_rejections_mapped_time_out_of_range);
        let deadline_rejections_platform_clock_unavailable =
            load(&self.deadline_rejections_platform_clock_unavailable);
        let deadline_rejections_other = load(&self.deadline_rejections_other);
        let deadline_rejections_platform_total = load(&self.deadline_rejections_platform_total);
        let deadline_rejections_portable_mapper_total =
            load(&self.deadline_rejections_portable_mapper_total);
        let deadline_rejections_total = deadline_rejections_generation_mismatch
            .saturating_add(deadline_rejections_source_mismatch)
            .saturating_add(deadline_rejections_non_monotonic_local_time)
            .saturating_add(deadline_rejections_offset_jump)
            .saturating_add(deadline_rejections_not_ready)
            .saturating_add(deadline_rejections_stale)
            .saturating_add(deadline_rejections_invalid_sample_rate)
            .saturating_add(deadline_rejections_ambiguous_rtp_delta)
            .saturating_add(deadline_rejections_mapped_time_out_of_range)
            .saturating_add(deadline_rejections_platform_clock_unavailable)
            .saturating_add(deadline_rejections_other);
        let expected_control_flushes = load(&self.expected_control_flushes);
        let expected_control_flush_drops = load(&self.expected_control_flush_drops);
        let expected_deferred_flushes = load(&self.expected_deferred_flushes);
        let expected_deferred_flush_drops = load(&self.expected_deferred_flush_drops);
        let scheduler_waits_missing_anchor = load(&self.scheduler_waits_missing_anchor);
        let scheduler_waits_missing_mapper = load(&self.scheduler_waits_missing_mapper);
        let deadline_warmup_waits_platform = load(&self.deadline_warmup_waits_platform);
        let deadline_warmup_waits_portable_mapper =
            load(&self.deadline_warmup_waits_portable_mapper);
        let deadline_warmup_waits_total =
            deadline_warmup_waits_platform.saturating_add(deadline_warmup_waits_portable_mapper);
        let expected_dispositions_total = expected_control_flushes
            .saturating_add(expected_control_flush_drops)
            .saturating_add(expected_deferred_flushes)
            .saturating_add(expected_deferred_flush_drops)
            .saturating_add(scheduler_waits_missing_anchor)
            .saturating_add(scheduler_waits_missing_mapper)
            .saturating_add(deadline_warmup_waits_total);
        let control_failures_invalid_anchor = load(&self.control_failures_invalid_anchor);
        let control_failures_invalid_flush = load(&self.control_failures_invalid_flush);
        let control_failures_deferred_limit = load(&self.control_failures_deferred_limit);
        let control_failures_total = control_failures_invalid_anchor
            .saturating_add(control_failures_invalid_flush)
            .saturating_add(control_failures_deferred_limit);
        MediaTelemetryCounters {
            packets_received: load(&self.packets_received),
            packets_decrypted: load(&self.packets_decrypted),
            decrypt_failures: load(&self.decrypt_failures),
            packets_admitted: load(&self.packets_admitted),
            admission_rejections_total,
            admission_rejections_replay,
            admission_rejections_limit,
            admission_rejections_parse,
            admission_rejections_profile,
            admission_rejections_sequence,
            admission_rejections_timestamp,
            admission_rejections_timeline,
            admission_rejections_source_ip,
            admission_rejections_source_endpoint,
            admission_rejections_packet_too_large,
            admission_rejections_other,
            jitter_rejections_total,
            jitter_rejections_duplicate,
            jitter_rejections_too_old,
            jitter_rejections_outside_window,
            jitter_rejections_full,
            jitter_rejections_sequence_exhausted,
            jitter_rejections_invalid_limits,
            jitter_rejections_empty_payload,
            jitter_rejections_payload_too_large,
            jitter_rejections_invalid_advance,
            jitter_rejections_other,
            jitter_evictions_far_future,
            packets_decoded: load(&self.packets_decoded),
            decode_failures: load(&self.decode_failures),
            output_writes: load(&self.output_writes),
            output_samples: load(&self.output_samples),
            future_safety_waits: load(&self.future_safety_waits),
            scheduler_late_drops: load(&self.scheduler_late_drops),
            predecode_late_drops: load(&self.predecode_late_drops),
            deadline_rejections_total,
            deadline_rejections_platform_total,
            deadline_rejections_portable_mapper_total,
            deadline_rejections_generation_mismatch,
            deadline_rejections_source_mismatch,
            deadline_rejections_non_monotonic_local_time,
            deadline_rejections_offset_jump,
            deadline_rejections_not_ready,
            deadline_rejections_stale,
            deadline_rejections_invalid_sample_rate,
            deadline_rejections_ambiguous_rtp_delta,
            deadline_rejections_mapped_time_out_of_range,
            deadline_rejections_platform_clock_unavailable,
            deadline_rejections_other,
            expected_dispositions_total,
            deadline_warmup_waits_total,
            deadline_warmup_waits_platform,
            deadline_warmup_waits_portable_mapper,
            ptp_broadcast_lag_events: load(&self.ptp_broadcast_lag_events),
            ptp_broadcast_lag_samples: load(&self.ptp_broadcast_lag_samples),
            ptp_cross_generation_discard_events: load(&self.ptp_cross_generation_discard_events),
            ptp_cross_generation_discard_samples: load(&self.ptp_cross_generation_discard_samples),
            expected_control_flushes,
            expected_control_flush_drops,
            expected_deferred_flushes,
            expected_deferred_flush_drops,
            scheduler_waits_missing_anchor,
            scheduler_waits_missing_mapper,
            control_failures_total,
            control_failures_invalid_anchor,
            control_failures_invalid_flush,
            control_failures_deferred_limit,
        }
    }

    fn reset(&self) {
        for counter in [
            &self.packets_received,
            &self.packets_decrypted,
            &self.decrypt_failures,
            &self.packets_admitted,
            &self.admission_rejections_total,
            &self.admission_rejections_replay,
            &self.admission_rejections_limit,
            &self.admission_rejections_parse,
            &self.admission_rejections_profile,
            &self.admission_rejections_sequence,
            &self.admission_rejections_timestamp,
            &self.admission_rejections_timeline,
            &self.admission_rejections_source_ip,
            &self.admission_rejections_source_endpoint,
            &self.admission_rejections_packet_too_large,
            &self.admission_rejections_other,
            &self.jitter_rejections_total,
            &self.jitter_rejections_duplicate,
            &self.jitter_rejections_too_old,
            &self.jitter_rejections_outside_window,
            &self.jitter_rejections_full,
            &self.jitter_rejections_sequence_exhausted,
            &self.jitter_rejections_invalid_limits,
            &self.jitter_rejections_empty_payload,
            &self.jitter_rejections_payload_too_large,
            &self.jitter_rejections_invalid_advance,
            &self.jitter_rejections_other,
            &self.jitter_evictions_far_future,
            &self.packets_decoded,
            &self.decode_failures,
            &self.output_writes,
            &self.output_samples,
            &self.future_safety_waits,
            &self.scheduler_late_drops,
            &self.predecode_late_drops,
            &self.deadline_rejections_total,
            &self.deadline_rejections_platform_total,
            &self.deadline_rejections_portable_mapper_total,
            &self.deadline_rejections_generation_mismatch,
            &self.deadline_rejections_source_mismatch,
            &self.deadline_rejections_non_monotonic_local_time,
            &self.deadline_rejections_offset_jump,
            &self.deadline_rejections_not_ready,
            &self.deadline_rejections_stale,
            &self.deadline_rejections_invalid_sample_rate,
            &self.deadline_rejections_ambiguous_rtp_delta,
            &self.deadline_rejections_mapped_time_out_of_range,
            &self.deadline_rejections_platform_clock_unavailable,
            &self.deadline_rejections_other,
            &self.deadline_warmup_waits_platform,
            &self.deadline_warmup_waits_portable_mapper,
            &self.ptp_broadcast_lag_events,
            &self.ptp_broadcast_lag_samples,
            &self.ptp_cross_generation_discard_events,
            &self.ptp_cross_generation_discard_samples,
            &self.expected_control_flushes,
            &self.expected_control_flush_drops,
            &self.expected_deferred_flushes,
            &self.expected_deferred_flush_drops,
            &self.scheduler_waits_missing_anchor,
            &self.scheduler_waits_missing_mapper,
            &self.control_failures_invalid_anchor,
            &self.control_failures_invalid_flush,
            &self.control_failures_deferred_limit,
        ] {
            counter.store(0, Ordering::Release);
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum EventCategory {
    RtspAccepted,
    RtspRejected400,
    RtspRejected453,
    RtspRejected455,
    RtspRejected500,
    RtspRejected501,
    RtspRejectedOtherStatus,
    TimingPeerSelection,
    PeerListSetPeersParsed,
    PeerListSetPeersRejectedContentType,
    PeerListSetPeersRejectedEmptyBody,
    PeerListSetPeersRejectedPlist,
    PeerListSetPeersRejectedShape,
    PeerListSetPeersXParsed,
    PeerListSetPeersXRejectedContentType,
    PeerListSetPeersXRejectedEmptyBody,
    PeerListSetPeersXRejectedPlist,
    PeerListSetPeersXRejectedShape,
    PtpSourcePortMismatch,
    PtpParseWrongLength,
    PtpParseUnsupportedProfile,
    PtpParseUnsupportedMessage,
    PtpParseInvalidTimestamp,
    PtpMessagePortMismatch,
    PtpMissingAnnouncement,
    PtpDuplicateCompletedPair,
    PtpAnnouncementObserved,
    PtpSyncObserved,
    PtpFollowUpObserved,
    PtpOther,
    PtpMapperSelected,
    PtpMapperAccepted,
    PtpMapperGenerationMismatch,
    PtpMapperSourceMismatch,
    PtpMapperNonMonotonicLocalTime,
    PtpMapperOffsetJump,
    PtpMapperNotReady,
    PtpMapperStale,
    PtpMapperInvalidSampleRate,
    PtpMapperAmbiguousRtpDelta,
    PtpMapperMappedTimeOutOfRange,
    PtpMapperPlatformClockUnavailable,
    PtpMapperOther,
}

#[derive(Default)]
struct TelemetrySlot {
    users: AtomicU64,
    next_event_sequence: AtomicU64,
    event_categories_low: AtomicU64,
    event_categories_high: AtomicU64,
    event_categories_third: AtomicU64,
    rtsp_phase2_prepared: AtomicU64,
    rtsp_phase2_accepted: AtomicU64,
    rtsp_phase2_socket_write_failures: AtomicU64,
    rtsp_phase2_prepared_socket_write_failures: AtomicU64,
    rtsp_phase2_commit_failures: AtomicU64,
    rtsp_phase2_failures_total: AtomicU64,
    rtsp_phase2_failures_400: AtomicU64,
    rtsp_phase2_failures_453: AtomicU64,
    rtsp_phase2_failures_455: AtomicU64,
    rtsp_phase2_failures_500: AtomicU64,
    rtsp_phase2_failures_501: AtomicU64,
    rtsp_phase2_failures_other_status: AtomicU64,
    rtsp_phase2_active_media_rejected_453: AtomicU64,
    timing_peer_info_observed: AtomicU64,
    peer_list_update_setpeers_requests: AtomicU64,
    peer_list_update_setpeersx_requests: AtomicU64,
    peer_list_update_parsed: AtomicU64,
    peer_list_update_parse_rejected_content_type: AtomicU64,
    peer_list_update_parse_rejected_empty_body: AtomicU64,
    peer_list_update_parse_rejected_plist: AtomicU64,
    peer_list_update_parse_rejected_shape: AtomicU64,
    peer_list_update_response_committed_200: AtomicU64,
    peer_list_update_socket_write_failures: AtomicU64,
    peer_list_update_commit_failures: AtomicU64,
    peer_list_update_applied_to_session_clock: AtomicU64,
    peer_list_update_acknowledged_but_ignored: AtomicU64,
    ptp_datagrams_received: AtomicU64,
    ptp_datagrams_accepted: AtomicU64,
    ptp_datagrams_rejected_source_port: AtomicU64,
    ptp_datagrams_rejected_parse: AtomicU64,
    ptp_datagrams_rejected_message_port: AtomicU64,
    ptp_datagrams_rejected_other: AtomicU64,
    ptp_sync_rejected_missing_announce: AtomicU64,
    ptp_samples_emitted: AtomicU64,
    ptp_mapper_selected: AtomicU64,
    ptp_mapper_samples_accepted: AtomicU64,
    ptp_mapper_samples_rejected: AtomicU64,
    ptp_mapper_ready: AtomicU64,
    ptp_pending_sync_evictions: AtomicU64,
    ptp_pending_follow_up_evictions: AtomicU64,
    ptp_pending_sync_expirations: AtomicU64,
    ptp_pending_follow_up_expirations: AtomicU64,
    ptp_pair_expirations: AtomicU64,
    ptp_invalid_time_conversions: AtomicU64,
    ptp_samples_without_subscriber: AtomicU64,
    ptp_mapper_pending_sample_evictions: AtomicU64,
    ptp_mapper_pending_sample_discards: AtomicU64,
    ptp_announcement_evictions: AtomicU64,
    ptp_completed_pair_evictions: AtomicU64,
    ptp_completed_pair_expirations: AtomicU64,
    ptp_announcement_expirations: AtomicU64,
    realtime: MediaCounters,
    buffered: MediaCounters,
    pcm_bus_pushes_accepted: AtomicU64,
    pcm_bus_pushes_owner_mismatch: AtomicU64,
    pcm_bus_samples_accepted: AtomicU64,
    pcm_bus_failures_total: AtomicU64,
    pcm_bus_lock_contention_reads: AtomicU64,
    pcm_bus_lock_contention_samples: AtomicU64,
    pcm_bus_underrun_reads: AtomicU64,
    pcm_bus_underrun_samples: AtomicU64,
    pcm_bus_overflow_events: AtomicU64,
    pcm_bus_overflow_samples: AtomicU64,
    pcm_bus_reader_dropped_samples: AtomicU64,
    pcm_bus_reader_drop_events: AtomicU64,
    pcm_bus_read_calls: AtomicU64,
    pcm_bus_output_samples: AtomicU64,
    pcm_bus_silence_samples: AtomicU64,
    // SETPEERS/SETPEERSX are control-plane requests. Their prepare and commit
    // transactions span several reason counters, so snapshot takes this same
    // narrow lock to expose an exact classification rather than a torn sum.
    peer_list_update_lock: Mutex<()>,
    observed_control_sessions: Mutex<BTreeSet<AirPlayControlSessionIdentity>>,
    control_sessions_truncated: AtomicBool,
    active_control_sessions: Mutex<BTreeSet<AirPlayControlSessionIdentity>>,
    events: Mutex<VecDeque<AirPlayTelemetryEvent>>,
}

impl TelemetrySlot {
    fn snapshot(&self, generation: u64, boundary: &GenerationBoundary) -> AirPlayTelemetrySnapshot {
        // Clone events first. Every event is queued after its counter update,
        // so an included event always has a corresponding visible counter;
        // counters may legitimately advance after this bounded clone.
        let mut recent_events: Vec<AirPlayTelemetryEvent> = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect();
        // Phase-two commit is strictly after preparation. Keep this pair in
        // one sequentially consistent order and read commit first so a
        // concurrent snapshot can never expose committed > prepared.
        let phase2_accepted = self.rtsp_phase2_accepted.load(Ordering::SeqCst);
        let phase2_socket_write_failures = self
            .rtsp_phase2_socket_write_failures
            .load(Ordering::SeqCst);
        let phase2_prepared_socket_write_failures = self
            .rtsp_phase2_prepared_socket_write_failures
            .load(Ordering::SeqCst);
        let phase2_commit_failures = self.rtsp_phase2_commit_failures.load(Ordering::SeqCst);
        let phase2_terminal_failures =
            phase2_socket_write_failures.saturating_add(phase2_commit_failures);
        let phase2_prepared = self.rtsp_phase2_prepared.load(Ordering::SeqCst);
        let phase2_failures_400 = load(&self.rtsp_phase2_failures_400);
        let phase2_failures_453 = load(&self.rtsp_phase2_failures_453);
        let phase2_failures_455 = load(&self.rtsp_phase2_failures_455);
        let phase2_failures_500 = load(&self.rtsp_phase2_failures_500);
        let phase2_failures_501 = load(&self.rtsp_phase2_failures_501);
        let phase2_failures_other_status = load(&self.rtsp_phase2_failures_other_status);
        let phase2_failures = phase2_failures_400
            .saturating_add(phase2_failures_453)
            .saturating_add(phase2_failures_455)
            .saturating_add(phase2_failures_500)
            .saturating_add(phase2_failures_501)
            .saturating_add(phase2_failures_other_status);
        // A committed 200 is recorded only after the response write. Read the
        // commit-side counters first; their sequentially consistent writes are
        // after every parse/method counter for the same request.
        let peer_list_update_guard = self
            .peer_list_update_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let peer_list_update_applied_to_session_clock = self
            .peer_list_update_applied_to_session_clock
            .load(Ordering::SeqCst);
        let peer_list_update_acknowledged_but_ignored = self
            .peer_list_update_acknowledged_but_ignored
            .load(Ordering::SeqCst);
        let peer_list_update_response_committed_200 = self
            .peer_list_update_response_committed_200
            .load(Ordering::SeqCst);
        let peer_list_update_socket_write_failures = self
            .peer_list_update_socket_write_failures
            .load(Ordering::SeqCst);
        let peer_list_update_commit_failures =
            self.peer_list_update_commit_failures.load(Ordering::SeqCst);
        let peer_list_update_parsed = self.peer_list_update_parsed.load(Ordering::SeqCst);
        let peer_list_update_setpeers_requests = self
            .peer_list_update_setpeers_requests
            .load(Ordering::SeqCst);
        let peer_list_update_setpeersx_requests = self
            .peer_list_update_setpeersx_requests
            .load(Ordering::SeqCst);
        let peer_list_update_requests =
            peer_list_update_setpeers_requests.saturating_add(peer_list_update_setpeersx_requests);
        let peer_list_update_parse_rejected_content_type = self
            .peer_list_update_parse_rejected_content_type
            .load(Ordering::SeqCst);
        let peer_list_update_parse_rejected_empty_body = self
            .peer_list_update_parse_rejected_empty_body
            .load(Ordering::SeqCst);
        let peer_list_update_parse_rejected_plist = self
            .peer_list_update_parse_rejected_plist
            .load(Ordering::SeqCst);
        let peer_list_update_parse_rejected_shape = self
            .peer_list_update_parse_rejected_shape
            .load(Ordering::SeqCst);
        let peer_list_update_parse_rejected_total = peer_list_update_parse_rejected_content_type
            .saturating_add(peer_list_update_parse_rejected_empty_body)
            .saturating_add(peer_list_update_parse_rejected_plist)
            .saturating_add(peer_list_update_parse_rejected_shape);
        let peer_list_update_prepared_inflight_gauge = peer_list_update_requests
            .saturating_sub(peer_list_update_response_committed_200)
            .saturating_sub(peer_list_update_socket_write_failures)
            .saturating_sub(peer_list_update_commit_failures);
        drop(peer_list_update_guard);
        let ptp_datagrams_accepted = load(&self.ptp_datagrams_accepted);
        let ptp_datagrams_rejected_source_port = load(&self.ptp_datagrams_rejected_source_port);
        let ptp_datagrams_rejected_parse = load(&self.ptp_datagrams_rejected_parse);
        let ptp_datagrams_rejected_message_port = load(&self.ptp_datagrams_rejected_message_port);
        let ptp_datagrams_rejected_other = load(&self.ptp_datagrams_rejected_other);
        let ptp_sync_rejected_missing_announce = load(&self.ptp_sync_rejected_missing_announce);
        let ptp_datagrams_rejected_total = ptp_datagrams_rejected_source_port
            .saturating_add(ptp_datagrams_rejected_parse)
            .saturating_add(ptp_datagrams_rejected_message_port)
            .saturating_add(ptp_datagrams_rejected_other)
            .saturating_add(ptp_sync_rejected_missing_announce);
        let ptp_pending_sync_evictions = load(&self.ptp_pending_sync_evictions);
        let ptp_pending_follow_up_evictions = load(&self.ptp_pending_follow_up_evictions);
        let ptp_pending_sync_expirations = load(&self.ptp_pending_sync_expirations);
        let ptp_pending_follow_up_expirations = load(&self.ptp_pending_follow_up_expirations);
        let ptp_pair_expirations = load(&self.ptp_pair_expirations);
        let ptp_invalid_time_conversions = load(&self.ptp_invalid_time_conversions);
        let ptp_samples_without_subscriber = load(&self.ptp_samples_without_subscriber);
        let ptp_mapper_pending_sample_evictions = load(&self.ptp_mapper_pending_sample_evictions);
        let ptp_mapper_pending_sample_discards = load(&self.ptp_mapper_pending_sample_discards);
        let ptp_announcement_evictions = load(&self.ptp_announcement_evictions);
        let ptp_mapper_samples_rejected = load(&self.ptp_mapper_samples_rejected);
        let ptp_failures_total = ptp_datagrams_rejected_total
            .saturating_add(ptp_pending_sync_evictions)
            .saturating_add(ptp_pending_follow_up_evictions)
            .saturating_add(ptp_pending_sync_expirations)
            .saturating_add(ptp_pending_follow_up_expirations)
            .saturating_add(ptp_pair_expirations)
            .saturating_add(ptp_invalid_time_conversions)
            .saturating_add(ptp_mapper_pending_sample_evictions)
            .saturating_add(ptp_mapper_pending_sample_discards)
            .saturating_add(ptp_announcement_evictions)
            .saturating_add(ptp_mapper_samples_rejected);
        let ptp_completed_pair_expirations = load(&self.ptp_completed_pair_expirations);
        let ptp_completed_pair_evictions = load(&self.ptp_completed_pair_evictions);
        let ptp_announcement_expirations = load(&self.ptp_announcement_expirations);
        let ptp_expected_dispositions_total = ptp_completed_pair_expirations
            .saturating_add(ptp_completed_pair_evictions)
            .saturating_add(ptp_announcement_expirations)
            .saturating_add(ptp_samples_without_subscriber);
        let pcm_bus_pushes_owner_mismatch = load(&self.pcm_bus_pushes_owner_mismatch);
        let pcm_bus_lock_contention_reads = load(&self.pcm_bus_lock_contention_reads);
        let pcm_bus_underrun_reads = load(&self.pcm_bus_underrun_reads);
        let pcm_bus_overflow_events = load(&self.pcm_bus_overflow_events);
        let pcm_bus_reader_drop_events = load(&self.pcm_bus_reader_drop_events);
        let pcm_bus_failures_total = pcm_bus_pushes_owner_mismatch
            .saturating_add(pcm_bus_lock_contention_reads)
            .saturating_add(pcm_bus_underrun_reads)
            .saturating_add(pcm_bus_overflow_events)
            .saturating_add(pcm_bus_reader_drop_events);
        let realtime = self.realtime.snapshot();
        let buffered = self.buffered.snapshot();
        let counters = AirPlayTelemetryCounters {
            // A request is public only after preparation or terminal
            // rejection. A prepared success becomes accepted only after
            // the response write and media-state commit both complete.
            rtsp_phase2_requests: phase2_prepared.saturating_add(phase2_failures),
            rtsp_phase2_prepared: phase2_prepared,
            rtsp_phase2_committed_accepted: phase2_accepted,
            rtsp_phase2_prepared_inflight_gauge: phase2_prepared
                .saturating_sub(phase2_accepted)
                .saturating_sub(phase2_prepared_socket_write_failures)
                .saturating_sub(phase2_commit_failures),
            rtsp_phase2_accepted: phase2_accepted,
            rtsp_phase2_terminal_failures_total: phase2_terminal_failures,
            rtsp_phase2_socket_write_failures_total: phase2_socket_write_failures,
            rtsp_phase2_prepared_socket_write_failures_total: phase2_prepared_socket_write_failures,
            rtsp_phase2_commit_failures_total: phase2_commit_failures,
            rtsp_phase2_failures_total: phase2_failures,
            rtsp_phase2_failures_400: phase2_failures_400,
            rtsp_phase2_failures_453: phase2_failures_453,
            rtsp_phase2_failures_455: phase2_failures_455,
            rtsp_phase2_failures_500: phase2_failures_500,
            rtsp_phase2_failures_501: phase2_failures_501,
            rtsp_phase2_failures_other_status: phase2_failures_other_status,
            rtsp_phase2_active_media_rejected_453: load(
                &self.rtsp_phase2_active_media_rejected_453,
            ),
            timing_peer_info_observed: load(&self.timing_peer_info_observed),
            peer_list_update_requests,
            peer_list_update_setpeers_requests,
            peer_list_update_setpeersx_requests,
            peer_list_update_parsed,
            peer_list_update_parse_rejected_total,
            peer_list_update_parse_rejected_content_type,
            peer_list_update_parse_rejected_empty_body,
            peer_list_update_parse_rejected_plist,
            peer_list_update_parse_rejected_shape,
            peer_list_update_response_committed_200,
            peer_list_update_socket_write_failures_total: peer_list_update_socket_write_failures,
            peer_list_update_commit_failures_total: peer_list_update_commit_failures,
            peer_list_update_applied_to_session_clock,
            peer_list_update_acknowledged_but_ignored,
            peer_list_update_prepared_inflight_gauge,
            ptp_datagrams_received: ptp_datagrams_accepted
                .saturating_add(ptp_datagrams_rejected_total),
            ptp_datagrams_accepted,
            ptp_datagrams_rejected_total,
            ptp_datagrams_rejected_source_port,
            ptp_datagrams_rejected_parse,
            ptp_datagrams_rejected_message_port,
            ptp_datagrams_rejected_other,
            ptp_sync_rejected_missing_announce,
            ptp_samples_emitted: load(&self.ptp_samples_emitted),
            ptp_mapper_selected: load(&self.ptp_mapper_selected),
            ptp_mapper_samples_accepted: load(&self.ptp_mapper_samples_accepted),
            ptp_mapper_samples_rejected,
            ptp_mapper_ready: load(&self.ptp_mapper_ready),
            ptp_failures_total,
            ptp_pending_sync_evictions,
            ptp_pending_follow_up_evictions,
            ptp_pending_sync_expirations,
            ptp_pending_follow_up_expirations,
            ptp_pair_expirations,
            ptp_invalid_time_conversions,
            ptp_samples_without_subscriber,
            ptp_mapper_pending_sample_evictions,
            ptp_mapper_pending_sample_discards,
            ptp_announcement_evictions,
            ptp_expected_dispositions_total,
            ptp_completed_pair_evictions,
            ptp_completed_pair_expirations,
            ptp_announcement_expirations,
            realtime,
            buffered,
            pcm_bus_pushes_accepted: load(&self.pcm_bus_pushes_accepted),
            pcm_bus_pushes_owner_mismatch,
            pcm_bus_samples_accepted: load(&self.pcm_bus_samples_accepted),
            pcm_bus_failures_total,
            pcm_bus_lock_contention_reads,
            pcm_bus_lock_contention_samples: load(&self.pcm_bus_lock_contention_samples),
            pcm_bus_underrun_reads,
            pcm_bus_underrun_samples: load(&self.pcm_bus_underrun_samples),
            pcm_bus_overflow_events,
            pcm_bus_overflow_samples: load(&self.pcm_bus_overflow_samples),
            pcm_bus_reader_dropped_samples: load(&self.pcm_bus_reader_dropped_samples),
            pcm_bus_reader_drop_events,
            pcm_bus_read_calls: load(&self.pcm_bus_read_calls),
            pcm_bus_output_samples: load(&self.pcm_bus_output_samples),
            pcm_bus_silence_samples: load(&self.pcm_bus_silence_samples),
        };
        let control_sessions = self
            .observed_control_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect();
        let control_sessions_truncated = self.control_sessions_truncated.load(Ordering::Acquire);
        let active_control_sessions = self
            .active_control_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect();
        append_deferred_realtime_events(self, &mut recent_events, &counters);
        AirPlayTelemetrySnapshot {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            reset_generation: generation,
            generation_marker: boundary.marker.clone(),
            reset_monotonic_ns: boundary.reset_monotonic_ns,
            counters,
            control_sessions,
            control_sessions_truncated,
            active_control_sessions,
            recent_events,
        }
    }

    fn reset_contents(&self) {
        for counter in [
            &self.next_event_sequence,
            &self.event_categories_low,
            &self.event_categories_high,
            &self.event_categories_third,
            &self.rtsp_phase2_prepared,
            &self.rtsp_phase2_accepted,
            &self.rtsp_phase2_socket_write_failures,
            &self.rtsp_phase2_prepared_socket_write_failures,
            &self.rtsp_phase2_commit_failures,
            &self.rtsp_phase2_failures_total,
            &self.rtsp_phase2_failures_400,
            &self.rtsp_phase2_failures_453,
            &self.rtsp_phase2_failures_455,
            &self.rtsp_phase2_failures_500,
            &self.rtsp_phase2_failures_501,
            &self.rtsp_phase2_failures_other_status,
            &self.rtsp_phase2_active_media_rejected_453,
            &self.timing_peer_info_observed,
            &self.peer_list_update_setpeers_requests,
            &self.peer_list_update_setpeersx_requests,
            &self.peer_list_update_parsed,
            &self.peer_list_update_parse_rejected_content_type,
            &self.peer_list_update_parse_rejected_empty_body,
            &self.peer_list_update_parse_rejected_plist,
            &self.peer_list_update_parse_rejected_shape,
            &self.peer_list_update_response_committed_200,
            &self.peer_list_update_socket_write_failures,
            &self.peer_list_update_commit_failures,
            &self.peer_list_update_applied_to_session_clock,
            &self.peer_list_update_acknowledged_but_ignored,
            &self.ptp_datagrams_received,
            &self.ptp_datagrams_accepted,
            &self.ptp_datagrams_rejected_source_port,
            &self.ptp_datagrams_rejected_parse,
            &self.ptp_datagrams_rejected_message_port,
            &self.ptp_datagrams_rejected_other,
            &self.ptp_sync_rejected_missing_announce,
            &self.ptp_samples_emitted,
            &self.ptp_mapper_selected,
            &self.ptp_mapper_samples_accepted,
            &self.ptp_mapper_samples_rejected,
            &self.ptp_mapper_ready,
            &self.ptp_pending_sync_evictions,
            &self.ptp_pending_follow_up_evictions,
            &self.ptp_pending_sync_expirations,
            &self.ptp_pending_follow_up_expirations,
            &self.ptp_pair_expirations,
            &self.ptp_invalid_time_conversions,
            &self.ptp_samples_without_subscriber,
            &self.ptp_mapper_pending_sample_evictions,
            &self.ptp_mapper_pending_sample_discards,
            &self.ptp_announcement_evictions,
            &self.ptp_completed_pair_evictions,
            &self.ptp_completed_pair_expirations,
            &self.ptp_announcement_expirations,
            &self.pcm_bus_pushes_accepted,
            &self.pcm_bus_pushes_owner_mismatch,
            &self.pcm_bus_samples_accepted,
            &self.pcm_bus_failures_total,
            &self.pcm_bus_lock_contention_reads,
            &self.pcm_bus_lock_contention_samples,
            &self.pcm_bus_underrun_reads,
            &self.pcm_bus_underrun_samples,
            &self.pcm_bus_overflow_events,
            &self.pcm_bus_overflow_samples,
            &self.pcm_bus_reader_dropped_samples,
            &self.pcm_bus_reader_drop_events,
            &self.pcm_bus_read_calls,
            &self.pcm_bus_output_samples,
            &self.pcm_bus_silence_samples,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        self.realtime.reset();
        self.buffered.reset();
        self.observed_control_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.control_sessions_truncated
            .store(false, Ordering::Release);
        self.active_control_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn media(&self, kind: MediaKind) -> &MediaCounters {
        match kind {
            MediaKind::Realtime => &self.realtime,
            MediaKind::Buffered => &self.buffered,
        }
    }

    fn claim_event(&self, category: EventCategory) -> bool {
        self.claim_event_index(category as u32)
    }

    fn claim_event_index(&self, index: u32) -> bool {
        // Only the first occurrence of a reason/state in this generation pays
        // for String allocation and the bounded event-queue mutex.
        let (categories, bit) = if index < 64 {
            (&self.event_categories_low, 1u64 << index)
        } else if index < 128 {
            (&self.event_categories_high, 1u64 << (index - 64))
        } else if index < 192 {
            (&self.event_categories_third, 1u64 << (index - 128))
        } else {
            debug_assert!(false, "telemetry event category exceeds its bitset");
            return false;
        };
        if categories.load(Ordering::Relaxed) & bit != 0 {
            return false;
        }
        categories.fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    fn next_sequence(&self) -> u64 {
        self.next_event_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    fn push_event(&self, event: AirPlayTelemetryEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if events.len() == RECENT_EVENT_LIMIT {
            events.pop_front();
        }
        events.push_back(event);
    }
}

fn append_deferred_media_events(
    events: &mut Vec<AirPlayTelemetryEvent>,
    profile: &'static str,
    counters: &MediaTelemetryCounters,
    next_sequence: &mut u64,
) {
    let mut push =
        |stage: &'static str, outcome: &'static str, reason: &'static str, packets: u64| {
            if packets == 0 || events.len() >= RECENT_EVENT_LIMIT {
                return;
            }
            *next_sequence = (*next_sequence).wrapping_add(1);
            events.push(AirPlayTelemetryEvent::MediaFailure {
                sequence: *next_sequence,
                media_profile: profile.to_owned(),
                stage: stage.to_owned(),
                outcome: outcome.to_owned(),
                reason: reason.to_owned(),
                packets,
            });
        };
    push(
        "admission",
        "rejected",
        "reason_counters_nonzero",
        counters.admission_rejections_total,
    );
    push(
        "jitter",
        "rejected",
        "reason_counters_nonzero",
        counters.jitter_rejections_total,
    );
    push(
        "scheduler",
        "wait",
        "future_safety_limit",
        counters.future_safety_waits,
    );
    push(
        "scheduler",
        "dropped",
        "late",
        counters.scheduler_late_drops,
    );
    push(
        "predecode",
        "dropped",
        "late",
        counters.predecode_late_drops,
    );
    push(
        "deadline",
        "rejected",
        "reason_counters_nonzero",
        counters.deadline_rejections_total,
    );
    push(
        "control",
        "rejected",
        "reason_counters_nonzero",
        counters.control_failures_total,
    );
    push(
        "control",
        "expected_disposition",
        "disposition_counters_nonzero",
        counters.expected_dispositions_total,
    );
}

/// Build explanatory events away from packet, scheduler, and PCM callback
/// paths. Those paths update only fixed atomics; allocation and the event mutex
/// are paid by the diagnostics reader that explicitly requested a snapshot.
fn append_deferred_realtime_events(
    slot: &TelemetrySlot,
    events: &mut Vec<AirPlayTelemetryEvent>,
    counters: &AirPlayTelemetryCounters,
) {
    // Derived summaries belong only to this returned value. Reserving sequence
    // numbers in the live slot would make a read-only snapshot mutate future
    // event identity and make two snapshots of quiescent state differ.
    let mut next_sequence = slot.next_event_sequence.load(Ordering::Acquire);
    append_deferred_media_events(events, "realtime", &counters.realtime, &mut next_sequence);
    append_deferred_media_events(events, "buffered", &counters.buffered, &mut next_sequence);
    // Never evict endpoint-bearing RTSP/PTP/peer-list observations merely to
    // add a counter-derived summary. The counters remain authoritative when
    // the bounded event list is already full.
    if counters.pcm_bus_failures_total > 0 && events.len() < RECENT_EVENT_LIMIT {
        next_sequence = next_sequence.wrapping_add(1);
        events.push(AirPlayTelemetryEvent::PcmBusFailure {
            sequence: next_sequence,
            operation: "read_or_write".to_owned(),
            reason: "reason_counters_nonzero".to_owned(),
            samples: counters
                .pcm_bus_overflow_samples
                .saturating_add(counters.pcm_bus_reader_dropped_samples)
                .saturating_add(counters.pcm_bus_lock_contention_samples)
                .saturating_add(counters.pcm_bus_underrun_samples),
        });
    }
}

#[derive(Debug, Clone)]
struct GenerationBoundary {
    marker: String,
    reset_monotonic_ns: u64,
}

impl GenerationBoundary {
    fn new(reset_monotonic_ns: u64) -> Self {
        Self {
            marker: Uuid::new_v4().to_string(),
            reset_monotonic_ns,
        }
    }
}

struct Telemetry {
    // Recorders pin one of two generation slots with atomics. Reset clears the
    // inactive slot before publishing it, so a late writer can only finish in
    // its old slot and can never leak into the new generation.
    active_generation: AtomicU64,
    slots: [TelemetrySlot; 2],
    boundaries: [Mutex<GenerationBoundary>; 2],
    monotonic_origin: Instant,
    reset_lock: Mutex<()>,
}

impl Default for Telemetry {
    fn default() -> Self {
        let monotonic_origin = Instant::now();
        Self {
            active_generation: AtomicU64::new(0),
            slots: [TelemetrySlot::default(), TelemetrySlot::default()],
            boundaries: [
                Mutex::new(GenerationBoundary::new(0)),
                Mutex::new(GenerationBoundary::new(0)),
            ],
            monotonic_origin,
            reset_lock: Mutex::new(()),
        }
    }
}

struct SlotGuard<'a> {
    generation: u64,
    slot: &'a TelemetrySlot,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.slot.users.fetch_sub(1, Ordering::Release);
    }
}

impl Telemetry {
    fn operation_token(&self) -> OperationToken {
        OperationToken {
            generation: self.active_generation.load(Ordering::Acquire),
        }
    }

    fn enter(&self) -> SlotGuard<'_> {
        loop {
            let generation = self.active_generation.load(Ordering::Acquire);
            let slot = &self.slots[(generation & 1) as usize];
            slot.users.fetch_add(1, Ordering::AcqRel);
            if self.active_generation.load(Ordering::Acquire) == generation {
                return SlotGuard { generation, slot };
            }
            slot.users.fetch_sub(1, Ordering::Release);
        }
    }

    fn enter_token(&self, token: OperationToken) -> Option<SlotGuard<'_>> {
        if self.active_generation.load(Ordering::Acquire) != token.generation {
            return None;
        }
        let slot = &self.slots[(token.generation & 1) as usize];
        slot.users.fetch_add(1, Ordering::AcqRel);
        if self.active_generation.load(Ordering::Acquire) == token.generation {
            Some(SlotGuard {
                generation: token.generation,
                slot,
            })
        } else {
            slot.users.fetch_sub(1, Ordering::Release);
            None
        }
    }

    fn snapshot(&self) -> AirPlayTelemetrySnapshot {
        loop {
            let guard = self.enter();
            let boundary = self.boundaries[(guard.generation & 1) as usize]
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let snapshot = guard.slot.snapshot(guard.generation, &boundary);

            // Reading the slot can overlap a reset that publishes the other
            // slot. Treat the final generation read as the linearization
            // point: an old snapshot is discarded if that publication became
            // visible while it was being assembled. Keeping the slot guard
            // pinned through this check also prevents a second reset from
            // clearing this slot while its counters and events are cloned.
            if self.active_generation.load(Ordering::Acquire) == guard.generation {
                return snapshot;
            }
        }
    }

    fn reset(&self) -> AirPlayTelemetrySnapshot {
        let _guard = self
            .reset_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = self.active_generation.load(Ordering::Acquire);
        let next = current
            .checked_add(1)
            .expect("AirPlay telemetry generation exhausted");
        let target = &self.slots[(next & 1) as usize];
        // This wait is reset-only. Packet/audio paths never take reset_lock and
        // never wait; they perform only the generation/user atomic handshake.
        let mut spins = 0usize;
        while target.users.load(Ordering::Acquire) != 0 {
            if spins < 64 {
                std::hint::spin_loop();
                spins += 1;
            } else {
                std::thread::yield_now();
            }
        }
        target.reset_contents();
        let boundary = GenerationBoundary::new(monotonic_ns_since(&self.monotonic_origin));
        *self.boundaries[(next & 1) as usize]
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = boundary.clone();
        self.active_generation.store(next, Ordering::Release);
        AirPlayTelemetrySnapshot {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            reset_generation: next,
            generation_marker: boundary.marker,
            reset_monotonic_ns: boundary.reset_monotonic_ns,
            counters: AirPlayTelemetryCounters::default(),
            control_sessions: Vec::new(),
            control_sessions_truncated: false,
            active_control_sessions: Vec::new(),
            recent_events: Vec::new(),
        }
    }
}

fn telemetry() -> &'static Telemetry {
    static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
    TELEMETRY.get_or_init(Telemetry::default)
}

/// Initialize UUID/reset metadata on a controller/startup thread before any
/// network packet or realtime PCM callback can enter telemetry.
pub(crate) fn prewarm() {
    let _ = telemetry();
}

pub(crate) fn operation_token() -> OperationToken {
    telemetry().operation_token()
}

pub(crate) fn token_is_current(token: OperationToken) -> bool {
    telemetry().active_generation.load(Ordering::Acquire) == token.generation
}

fn monotonic_ns_since(origin: &Instant) -> u64 {
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn load(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Acquire)
}

fn increment(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn add(counter: &AtomicU64, value: usize) {
    counter.fetch_add(value as u64, Ordering::Relaxed);
}

/// Read the current process-wide receiver diagnostics without changing them.
pub fn snapshot() -> AirPlayTelemetrySnapshot {
    telemetry().snapshot()
}

/// Reset counters and events, then return the new generation's initial snapshot.
pub fn reset() -> AirPlayTelemetrySnapshot {
    telemetry().reset()
}

fn phase2_event_category(status: u16) -> EventCategory {
    match status {
        200 => EventCategory::RtspAccepted,
        400 => EventCategory::RtspRejected400,
        453 => EventCategory::RtspRejected453,
        455 => EventCategory::RtspRejected455,
        500 => EventCategory::RtspRejected500,
        501 => EventCategory::RtspRejected501,
        _ => EventCategory::RtspRejectedOtherStatus,
    }
}

fn peer_list_event_category(method: &str, parsed: bool, reason: &str) -> EventCategory {
    match (method, parsed, reason) {
        ("SETPEERS", true, _) => EventCategory::PeerListSetPeersParsed,
        ("SETPEERS", false, "content_type") => EventCategory::PeerListSetPeersRejectedContentType,
        ("SETPEERS", false, "empty_body") => EventCategory::PeerListSetPeersRejectedEmptyBody,
        ("SETPEERS", false, "plist") => EventCategory::PeerListSetPeersRejectedPlist,
        ("SETPEERS", false, _) => EventCategory::PeerListSetPeersRejectedShape,
        ("SETPEERSX", true, _) => EventCategory::PeerListSetPeersXParsed,
        ("SETPEERSX", false, "content_type") => EventCategory::PeerListSetPeersXRejectedContentType,
        ("SETPEERSX", false, "empty_body") => EventCategory::PeerListSetPeersXRejectedEmptyBody,
        ("SETPEERSX", false, "plist") => EventCategory::PeerListSetPeersXRejectedPlist,
        ("SETPEERSX", false, _) => EventCategory::PeerListSetPeersXRejectedShape,
        _ => EventCategory::PeerListSetPeersRejectedShape,
    }
}

fn ptp_datagram_event_category(reason: &str) -> EventCategory {
    match reason {
        "source_port_mismatch" => EventCategory::PtpSourcePortMismatch,
        "parse_wrong_length" => EventCategory::PtpParseWrongLength,
        "parse_unsupported_profile" => EventCategory::PtpParseUnsupportedProfile,
        "parse_unsupported_message" => EventCategory::PtpParseUnsupportedMessage,
        "parse_invalid_timestamp" => EventCategory::PtpParseInvalidTimestamp,
        "message_port_mismatch" => EventCategory::PtpMessagePortMismatch,
        "missing_announcement" => EventCategory::PtpMissingAnnouncement,
        "duplicate_completed_pair" => EventCategory::PtpDuplicateCompletedPair,
        "announcement_observed" => EventCategory::PtpAnnouncementObserved,
        "sync_observed" => EventCategory::PtpSyncObserved,
        "follow_up_observed" => EventCategory::PtpFollowUpObserved,
        _ => EventCategory::PtpOther,
    }
}

fn ptp_mapper_event_category(accepted: bool, reason: &str) -> EventCategory {
    if accepted {
        return EventCategory::PtpMapperAccepted;
    }
    match reason {
        "generation_mismatch" => EventCategory::PtpMapperGenerationMismatch,
        "source_mismatch" => EventCategory::PtpMapperSourceMismatch,
        "non_monotonic_local_time" => EventCategory::PtpMapperNonMonotonicLocalTime,
        "offset_jump" => EventCategory::PtpMapperOffsetJump,
        "not_ready" => EventCategory::PtpMapperNotReady,
        "stale" => EventCategory::PtpMapperStale,
        "invalid_sample_rate" => EventCategory::PtpMapperInvalidSampleRate,
        "ambiguous_rtp_delta" => EventCategory::PtpMapperAmbiguousRtpDelta,
        "mapped_time_out_of_range" => EventCategory::PtpMapperMappedTimeOutOfRange,
        "platform_clock_unavailable" => EventCategory::PtpMapperPlatformClockUnavailable,
        _ => EventCategory::PtpMapperOther,
    }
}

fn control_session_identity(
    peer: IpAddr,
    connection_id: ConnectionId,
) -> AirPlayControlSessionIdentity {
    AirPlayControlSessionIdentity {
        peer_ip: peer.to_string(),
        connection_id: connection_id.text(),
    }
}

fn note_control_session(slot: &TelemetrySlot, peer: IpAddr, connection_id: ConnectionId) {
    let identity = control_session_identity(peer, connection_id);
    let mut active = slot
        .active_control_sessions
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    active.insert(identity.clone());
    let mut observed = slot
        .observed_control_sessions
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if observed.contains(&identity) {
        return;
    }
    if observed.len() >= CONTROL_SESSION_HISTORY_LIMIT {
        // Publish truncation before changing the retained set so a concurrent
        // snapshot can only be conservative about completeness.
        slot.control_sessions_truncated
            .store(true, Ordering::Release);
        let evicted = observed
            .iter()
            .find(|candidate| !active.contains(*candidate))
            .cloned();
        let Some(evicted) = evicted else {
            // The server caps active control connections far below the history
            // limit. Keep this fail-closed branch for direct/internal callers:
            // active identities are never sacrificed for historical detail.
            return;
        };
        observed.remove(&evicted);
    }
    observed.insert(identity);
}

pub(crate) fn register_control_session(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    guard
        .slot
        .active_control_sessions
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(control_session_identity(peer, connection_id));
}

pub(crate) fn unregister_control_session(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    guard
        .slot
        .active_control_sessions
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&control_session_identity(peer, connection_id));
}

pub(crate) fn record_phase2_prepared(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
    stream_type: Option<u64>,
    status: u16,
    reason: &str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, peer, connection_id);
    if status == 200 {
        guard
            .slot
            .rtsp_phase2_prepared
            .fetch_add(1, Ordering::SeqCst);
    } else {
        increment(&guard.slot.rtsp_phase2_failures_total);
        match status {
            400 => increment(&guard.slot.rtsp_phase2_failures_400),
            453 => increment(&guard.slot.rtsp_phase2_failures_453),
            455 => increment(&guard.slot.rtsp_phase2_failures_455),
            500 => increment(&guard.slot.rtsp_phase2_failures_500),
            501 => increment(&guard.slot.rtsp_phase2_failures_501),
            _ => increment(&guard.slot.rtsp_phase2_failures_other_status),
        }
        if status == 453 && reason == "active_media_permit_unavailable" {
            increment(&guard.slot.rtsp_phase2_active_media_rejected_453);
        }
    }
    if !guard.slot.claim_event(phase2_event_category(status)) {
        return;
    }
    guard.slot.push_event(AirPlayTelemetryEvent::RtspPhase2 {
        sequence: guard.slot.next_sequence(),
        peer: peer.to_string(),
        connection_id: connection_id.text(),
        stream_type,
        outcome: (if status == 200 {
            "prepared"
        } else {
            "rejected"
        })
        .to_owned(),
        status,
        reason: reason.to_owned(),
    });
}

pub(crate) fn record_phase2_committed(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, peer, connection_id);
    guard
        .slot
        .rtsp_phase2_accepted
        .fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_phase2_socket_write_failure(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
    prepared_status: u16,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, peer, connection_id);
    guard
        .slot
        .rtsp_phase2_socket_write_failures
        .fetch_add(1, Ordering::SeqCst);
    if prepared_status == 200 {
        guard
            .slot
            .rtsp_phase2_prepared_socket_write_failures
            .fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) fn record_phase2_commit_failure(
    token: OperationToken,
    peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, peer, connection_id);
    guard
        .slot
        .rtsp_phase2_commit_failures
        .fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_timing_peer_selection(
    token: OperationToken,
    control_peer: IpAddr,
    connection_id: ConnectionId,
    timing_peer_id: &str,
    advertised_addresses: &[String],
    receive_match: Option<(u64, u16)>,
    mapper_peer: IpAddr,
    timing_peer_list_ids: &[String],
    timing_peer_info_in_list: bool,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, control_peer, connection_id);
    increment(&guard.slot.timing_peer_info_observed);
    if !guard.slot.claim_event(EventCategory::TimingPeerSelection) {
        return;
    }
    let mapper_peer = mapper_peer.to_string();
    let mapper_peer_in_advertised_addresses = advertised_addresses
        .iter()
        .any(|address| address == &mapper_peer);
    guard
        .slot
        .push_event(AirPlayTelemetryEvent::TimingPeerSelection {
            sequence: guard.slot.next_sequence(),
            control_peer: control_peer.to_string(),
            connection_id: connection_id.text(),
            timing_peer_id: timing_peer_id.to_owned(),
            advertised_addresses: advertised_addresses.to_vec(),
            receive_match_clock_identity: receive_match.map(|(clock, _)| format!("{clock:016x}")),
            receive_match_port: receive_match.map(|(_, port)| port),
            mapper_peer,
            mapper_peer_in_advertised_addresses,
            timing_peer_list_ids: timing_peer_list_ids.to_vec(),
            timing_peer_info_in_list,
        });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_peer_list_update_prepared(
    token: OperationToken,
    control_peer: IpAddr,
    connection_id: ConnectionId,
    method: &'static str,
    content_type: Option<&str>,
    response_status: u16,
    parsed: bool,
    reason: &'static str,
    peer_count: usize,
    address_count: usize,
    peer_ids: &[String],
    addresses: &[String],
    clock_identity_count: usize,
    clock_identities: &[u64],
    clock_port_count: usize,
    local_clock_port_matches: usize,
    context_truncated: bool,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, control_peer, connection_id);
    {
        let _transaction = guard
            .slot
            .peer_list_update_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let method_counter = match method {
            "SETPEERS" => &guard.slot.peer_list_update_setpeers_requests,
            "SETPEERSX" => &guard.slot.peer_list_update_setpeersx_requests,
            _ => return,
        };
        if parsed {
            guard
                .slot
                .peer_list_update_parsed
                .fetch_add(1, Ordering::SeqCst);
        } else {
            let counter = match reason {
                "content_type" => &guard.slot.peer_list_update_parse_rejected_content_type,
                "empty_body" => &guard.slot.peer_list_update_parse_rejected_empty_body,
                "plist" => &guard.slot.peer_list_update_parse_rejected_plist,
                _ => &guard.slot.peer_list_update_parse_rejected_shape,
            };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        method_counter.fetch_add(1, Ordering::SeqCst);
    }
    if !guard
        .slot
        .claim_event(peer_list_event_category(method, parsed, reason))
    {
        return;
    }
    guard
        .slot
        .push_event(AirPlayTelemetryEvent::PeerListUpdate {
            sequence: guard.slot.next_sequence(),
            control_peer: control_peer.to_string(),
            connection_id: connection_id.text(),
            method: method.to_owned(),
            content_type: content_type.map(str::to_owned),
            response_status,
            outcome: (if response_status != 200 {
                "response_overridden"
            } else if parsed {
                "parsed_prepared_200"
            } else {
                "parse_rejected_prepared_200"
            })
            .to_owned(),
            reason: reason.to_owned(),
            peer_count: u64::try_from(peer_count).unwrap_or(u64::MAX),
            address_count: u64::try_from(address_count).unwrap_or(u64::MAX),
            peer_ids: peer_ids.to_vec(),
            addresses: addresses.to_vec(),
            clock_identity_count: u64::try_from(clock_identity_count).unwrap_or(u64::MAX),
            clock_identities: clock_identities
                .iter()
                .map(|clock| format!("{clock:016x}"))
                .collect(),
            clock_port_count: u64::try_from(clock_port_count).unwrap_or(u64::MAX),
            local_clock_port_matches: u64::try_from(local_clock_port_matches).unwrap_or(u64::MAX),
            context_truncated,
        });
}

pub(crate) fn record_peer_list_update_committed(
    token: OperationToken,
    control_peer: IpAddr,
    connection_id: ConnectionId,
    parsed: bool,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, control_peer, connection_id);
    let _transaction = guard
        .slot
        .peer_list_update_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard
        .slot
        .peer_list_update_response_committed_200
        .fetch_add(1, Ordering::SeqCst);
    if parsed {
        guard
            .slot
            .peer_list_update_acknowledged_but_ignored
            .fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) fn record_peer_list_update_socket_write_failure(
    token: OperationToken,
    control_peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, control_peer, connection_id);
    let _transaction = guard
        .slot
        .peer_list_update_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard
        .slot
        .peer_list_update_socket_write_failures
        .fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_peer_list_update_commit_failure(
    token: OperationToken,
    control_peer: IpAddr,
    connection_id: ConnectionId,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    note_control_session(guard.slot, control_peer, connection_id);
    let _transaction = guard
        .slot
        .peer_list_update_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard
        .slot
        .peer_list_update_commit_failures
        .fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_ptp_datagram(
    token: OperationToken,
    peer: IpAddr,
    source_port: u16,
    local_port: u16,
    message: Option<&str>,
    accepted: bool,
    reason: &str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.ptp_datagrams_received);
    if accepted {
        increment(&guard.slot.ptp_datagrams_accepted);
    } else {
        match reason {
            "source_port_mismatch" => {
                increment(&guard.slot.ptp_datagrams_rejected_source_port);
            }
            "message_port_mismatch" => {
                increment(&guard.slot.ptp_datagrams_rejected_message_port);
            }
            "missing_announcement" => {
                increment(&guard.slot.ptp_sync_rejected_missing_announce);
            }
            _ if reason.starts_with("parse_") => {
                increment(&guard.slot.ptp_datagrams_rejected_parse);
            }
            _ => increment(&guard.slot.ptp_datagrams_rejected_other),
        }
    }
    // This runs on the PTP UDP observer task, not a media scheduler or PCM
    // callback. Preserve the first endpoint/reason observation per generation
    // so a failed two-target run can be attributed without putting allocation
    // or the event mutex on an audio hot path.
    if !guard.slot.claim_event(ptp_datagram_event_category(reason)) {
        return;
    }
    guard.slot.push_event(AirPlayTelemetryEvent::PtpDatagram {
        sequence: guard.slot.next_sequence(),
        peer: peer.to_string(),
        source_port,
        local_port,
        message: message.map(str::to_owned),
        outcome: (if accepted { "accepted" } else { "rejected" }).to_owned(),
        reason: reason.to_owned(),
    });
}

pub(crate) fn record_ptp_sample_emitted(token: OperationToken, delivered: bool) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    if delivered {
        increment(&guard.slot.ptp_samples_emitted);
    } else {
        increment(&guard.slot.ptp_samples_without_subscriber);
    }
}

pub(crate) fn record_ptp_mapper_selected(token: OperationToken, peer: IpAddr, grandmaster: u64) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.ptp_mapper_selected);
    if !guard.slot.claim_event(EventCategory::PtpMapperSelected) {
        return;
    }
    guard.slot.push_event(AirPlayTelemetryEvent::PtpMapper {
        sequence: guard.slot.next_sequence(),
        selected_peer: peer.to_string(),
        sample_peer: None,
        grandmaster: format!("{grandmaster:016x}"),
        outcome: "selected".to_owned(),
        reason: "timing_peer_selected".to_owned(),
    });
}

pub(crate) fn record_ptp_mapper_sample(
    token: OperationToken,
    selected_peer: IpAddr,
    sample_peer: IpAddr,
    grandmaster: u64,
    accepted: bool,
    ready: bool,
    reason: &str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    if accepted {
        increment(&guard.slot.ptp_mapper_samples_accepted);
    } else {
        increment(&guard.slot.ptp_mapper_samples_rejected);
    }
    if ready {
        increment(&guard.slot.ptp_mapper_ready);
    }
    if !guard
        .slot
        .claim_event(ptp_mapper_event_category(accepted, reason))
    {
        return;
    }
    guard.slot.push_event(AirPlayTelemetryEvent::PtpMapper {
        sequence: guard.slot.next_sequence(),
        selected_peer: selected_peer.to_string(),
        sample_peer: Some(sample_peer.to_string()),
        grandmaster: format!("{grandmaster:016x}"),
        outcome: (if !accepted {
            "rejected"
        } else if ready {
            "ready"
        } else {
            "accepted_pending"
        })
        .to_owned(),
        reason: reason.to_owned(),
    });
}

pub(crate) fn record_ptp_pending_disposition(
    token: OperationToken,
    reason: &'static str,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counter = match reason {
        "pending_sync_evicted" => &guard.slot.ptp_pending_sync_evictions,
        "pending_follow_up_evicted" => &guard.slot.ptp_pending_follow_up_evictions,
        "pending_sync_expired" => &guard.slot.ptp_pending_sync_expirations,
        "pending_follow_up_expired" => &guard.slot.ptp_pending_follow_up_expirations,
        "pair_expired" => &guard.slot.ptp_pair_expirations,
        "invalid_time_conversion" => &guard.slot.ptp_invalid_time_conversions,
        "mapper_pending_sample_evicted" => &guard.slot.ptp_mapper_pending_sample_evictions,
        "mapper_pending_sample_discarded" => &guard.slot.ptp_mapper_pending_sample_discards,
        "announcement_evicted" => &guard.slot.ptp_announcement_evictions,
        "completed_pair_evicted" => &guard.slot.ptp_completed_pair_evictions,
        "completed_pair_expired" => &guard.slot.ptp_completed_pair_expirations,
        "announcement_expired" => &guard.slot.ptp_announcement_expirations,
        _ => return,
    };
    add(counter, count);
}

pub(crate) fn record_media_received(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).packets_received);
}

pub(crate) fn record_media_decrypted(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).packets_decrypted);
}

pub(crate) fn record_media_decrypt_failure(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).decrypt_failures);
}

pub(crate) fn record_media_admitted(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).packets_admitted);
}

pub(crate) fn record_media_admission_rejection(
    token: OperationToken,
    kind: MediaKind,
    reason: &'static str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.admission_rejections_total);
    match reason {
        "nonce_replay" => {
            increment(&counters.admission_rejections_replay);
        }
        "session_limit" => increment(&counters.admission_rejections_limit),
        "rtp_parse" | "header_parse" => increment(&counters.admission_rejections_parse),
        "payload_type" | "ssrc" => increment(&counters.admission_rejections_profile),
        "sequence_duplicate" | "sequence_invalid" | "sequence_out_of_window" => {
            increment(&counters.admission_rejections_sequence);
        }
        "timestamp_duplicate" | "timestamp_invalid" | "timestamp_out_of_window" => {
            increment(&counters.admission_rejections_timestamp);
        }
        "timeline_discontinuity" => increment(&counters.admission_rejections_timeline),
        "source_ip" => increment(&counters.admission_rejections_source_ip),
        "source_endpoint" => increment(&counters.admission_rejections_source_endpoint),
        "packet_too_large" => increment(&counters.admission_rejections_packet_too_large),
        _ => increment(&counters.admission_rejections_other),
    }
}

pub(crate) fn record_jitter_rejection(
    token: OperationToken,
    kind: MediaKind,
    reason: &'static str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.jitter_rejections_total);
    match reason {
        "duplicate" => increment(&counters.jitter_rejections_duplicate),
        "too_old" => increment(&counters.jitter_rejections_too_old),
        "outside_window" => increment(&counters.jitter_rejections_outside_window),
        "full" => increment(&counters.jitter_rejections_full),
        "sequence_exhausted" => increment(&counters.jitter_rejections_sequence_exhausted),
        "invalid_limits" => increment(&counters.jitter_rejections_invalid_limits),
        "empty_payload" => increment(&counters.jitter_rejections_empty_payload),
        "payload_too_large" => increment(&counters.jitter_rejections_payload_too_large),
        "invalid_advance" => increment(&counters.jitter_rejections_invalid_advance),
        _ => increment(&counters.jitter_rejections_other),
    }
}

pub(crate) fn record_jitter_far_future_eviction(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.jitter_rejections_total);
    increment(&counters.jitter_evictions_far_future);
}

pub(crate) fn record_media_decoded(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).packets_decoded);
}

pub(crate) fn record_media_decode_failure(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).decode_failures);
}

pub(crate) fn record_media_output(token: OperationToken, kind: MediaKind, samples: usize) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.output_writes);
    add(&counters.output_samples, samples);
}

pub(crate) fn record_future_safety_wait(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).future_safety_waits);
}

pub(crate) fn record_scheduler_late_drop(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).scheduler_late_drops);
}

pub(crate) fn record_predecode_late_drop(token: OperationToken, kind: MediaKind) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    increment(&guard.slot.media(kind).predecode_late_drops);
}

pub(crate) fn record_deadline_rejection(
    token: OperationToken,
    kind: MediaKind,
    source: DeadlineSource,
    reason: &'static str,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.deadline_rejections_total);
    match source {
        DeadlineSource::Platform => increment(&counters.deadline_rejections_platform_total),
        DeadlineSource::PortableMapper => {
            increment(&counters.deadline_rejections_portable_mapper_total);
        }
    }
    match reason {
        "generation_mismatch" => increment(&counters.deadline_rejections_generation_mismatch),
        "source_mismatch" => increment(&counters.deadline_rejections_source_mismatch),
        "non_monotonic_local_time" => {
            increment(&counters.deadline_rejections_non_monotonic_local_time);
        }
        "offset_jump" => increment(&counters.deadline_rejections_offset_jump),
        "not_ready" => increment(&counters.deadline_rejections_not_ready),
        "stale" => increment(&counters.deadline_rejections_stale),
        "invalid_sample_rate" => increment(&counters.deadline_rejections_invalid_sample_rate),
        "ambiguous_rtp_delta" => increment(&counters.deadline_rejections_ambiguous_rtp_delta),
        "mapped_time_out_of_range" => {
            increment(&counters.deadline_rejections_mapped_time_out_of_range);
        }
        "platform_clock_unavailable" => {
            increment(&counters.deadline_rejections_platform_clock_unavailable);
        }
        _ => increment(&counters.deadline_rejections_other),
    }
}

pub(crate) fn record_deadline_warmup_wait(
    token: OperationToken,
    kind: MediaKind,
    source: DeadlineSource,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    match source {
        DeadlineSource::Platform => increment(&counters.deadline_warmup_waits_platform),
        DeadlineSource::PortableMapper => {
            increment(&counters.deadline_warmup_waits_portable_mapper);
        }
    }
}

pub(crate) fn record_ptp_broadcast_lag(token: OperationToken, kind: MediaKind, skipped: u64) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.ptp_broadcast_lag_events);
    counters
        .ptp_broadcast_lag_samples
        .fetch_add(skipped.max(1), Ordering::Relaxed);
}

pub(crate) fn record_ptp_cross_generation_discard(
    token: OperationToken,
    kind: MediaKind,
    samples: usize,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    increment(&counters.ptp_cross_generation_discard_events);
    add(
        &counters.ptp_cross_generation_discard_samples,
        samples.max(1),
    );
}

pub(crate) fn record_scheduler_wait(token: OperationToken, kind: MediaKind, reason: &'static str) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    match reason {
        "missing_anchor" => increment(&counters.scheduler_waits_missing_anchor),
        "missing_mapper" => increment(&counters.scheduler_waits_missing_mapper),
        _ => {}
    }
}

pub(crate) fn record_expected_control_disposition(
    token: OperationToken,
    kind: MediaKind,
    reason: &'static str,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    let counter = match reason {
        "control_flush" => &counters.expected_control_flushes,
        "control_flush_drop" => &counters.expected_control_flush_drops,
        "deferred_flush" => &counters.expected_deferred_flushes,
        "deferred_flush_drop" => &counters.expected_deferred_flush_drops,
        _ => return,
    };
    add(counter, count);
}

pub(crate) fn record_control_failure(token: OperationToken, kind: MediaKind, reason: &'static str) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    let counters = guard.slot.media(kind);
    match reason {
        "invalid_anchor" => increment(&counters.control_failures_invalid_anchor),
        "invalid_flush" => increment(&counters.control_failures_invalid_flush),
        "deferred_limit" => increment(&counters.control_failures_deferred_limit),
        _ => {}
    }
}

pub(crate) fn record_pcm_bus_push(
    token: OperationToken,
    session_id: u64,
    active: Option<u64>,
    samples: usize,
    overflow: usize,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    if active == Some(session_id) {
        // Failure counters are updated before the accepted operation so a
        // concurrent snapshot cannot let success conceal a bounded-ring loss.
        if overflow > 0 {
            increment(&guard.slot.pcm_bus_failures_total);
            increment(&guard.slot.pcm_bus_overflow_events);
            add(&guard.slot.pcm_bus_overflow_samples, overflow);
        }
        increment(&guard.slot.pcm_bus_pushes_accepted);
        add(&guard.slot.pcm_bus_samples_accepted, samples);
        return;
    }
    increment(&guard.slot.pcm_bus_failures_total);
    increment(&guard.slot.pcm_bus_pushes_owner_mismatch);
    let _ = (session_id, active, samples);
}

pub(crate) fn record_pcm_bus_read(
    token: OperationToken,
    copied: usize,
    silence: usize,
    dropped: u64,
    lock_contention: bool,
    underrun: bool,
) {
    let Some(guard) = telemetry().enter_token(token) else {
        return;
    };
    // Record failures before the aggregate read counters. This preserves the
    // failure-first diagnostic contract at every observable snapshot boundary.
    if lock_contention {
        increment(&guard.slot.pcm_bus_failures_total);
        increment(&guard.slot.pcm_bus_lock_contention_reads);
        add(&guard.slot.pcm_bus_lock_contention_samples, silence);
    }
    if underrun {
        increment(&guard.slot.pcm_bus_failures_total);
        increment(&guard.slot.pcm_bus_underrun_reads);
        add(&guard.slot.pcm_bus_underrun_samples, silence);
    }
    if dropped > 0 {
        increment(&guard.slot.pcm_bus_failures_total);
        increment(&guard.slot.pcm_bus_reader_drop_events);
        guard
            .slot
            .pcm_bus_reader_dropped_samples
            .fetch_add(dropped, Ordering::Relaxed);
    }
    increment(&guard.slot.pcm_bus_read_calls);
    add(&guard.slot.pcm_bus_output_samples, copied);
    add(&guard.slot.pcm_bus_silence_samples, silence);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_recorder_has_bounded_events_and_resettable_counters() {
        let recorder = Telemetry::default();
        {
            let guard = recorder.enter();
            increment(&guard.slot.rtsp_phase2_prepared);
            increment(&guard.slot.rtsp_phase2_accepted);
            for index in 0..(RECENT_EVENT_LIMIT + 3) {
                guard
                    .slot
                    .push_event(AirPlayTelemetryEvent::PcmBusOwnerMismatch {
                        sequence: guard.slot.next_sequence(),
                        attempted_session: index as u64,
                        active_session: None,
                        samples: 480,
                    });
            }
        }
        let before = recorder.snapshot();
        assert_eq!(before.schema_version, 3);
        assert!(Uuid::parse_str(&before.generation_marker).is_ok());
        assert_eq!(before.counters.rtsp_phase2_requests, 1);
        assert_eq!(before.recent_events.len(), RECENT_EVENT_LIMIT);

        let after = recorder.reset();
        assert_eq!(after.reset_generation, 1);
        assert_ne!(after.generation_marker, before.generation_marker);
        assert!(after.reset_monotonic_ns >= before.reset_monotonic_ns);
        assert_eq!(after.counters, AirPlayTelemetryCounters::default());
        assert!(after.recent_events.is_empty());

        let captured = recorder.snapshot();
        assert_eq!(captured.reset_generation, after.reset_generation);
        assert_eq!(captured.generation_marker, after.generation_marker);
        assert_eq!(captured.reset_monotonic_ns, after.reset_monotonic_ns);
    }

    #[test]
    fn public_snapshot_serializes_with_stable_top_level_fields() {
        let value = serde_json::to_value(Telemetry::default().snapshot()).unwrap();
        assert_eq!(value["schema_version"], 3);
        assert!(value["control_sessions"].is_array());
        assert!(value["control_sessions_truncated"].is_boolean());
        assert!(value["active_control_sessions"].is_array());
        assert!(value["reset_generation"].is_u64());
        assert!(value["generation_marker"].is_string());
        assert!(value["reset_monotonic_ns"].is_u64());
        assert!(value["counters"]["rtsp_phase2_prepared"].is_u64());
        assert!(value["counters"]["rtsp_phase2_committed_accepted"].is_u64());
        assert!(value["counters"]["peer_list_update_parsed"].is_u64());
        assert!(value["counters"]["peer_list_update_response_committed_200"].is_u64());
        assert!(value["counters"]["peer_list_update_applied_to_session_clock"].is_u64());
        assert!(value["counters"]["peer_list_update_acknowledged_but_ignored"].is_u64());
        assert!(value["counters"]["ptp_mapper_ready"].is_u64());
        assert!(value["counters"]["ptp_failures_total"].is_u64());
        assert!(value["counters"]["realtime"]["packets_received"].is_u64());
        assert!(value["counters"]["realtime"]["admission_rejections_total"].is_u64());
        assert!(value["counters"]["realtime"]["jitter_rejections_total"].is_u64());
        assert!(value["counters"]["buffered"]["predecode_late_drops"].is_u64());
        assert!(value["counters"]["pcm_bus_failures_total"].is_u64());
        assert!(value["counters"]["pcm_bus_lock_contention_reads"].is_u64());
        assert!(value["counters"]["pcm_bus_underrun_reads"].is_u64());
        assert!(value["counters"]["pcm_bus_overflow_events"].is_u64());
        assert!(value["recent_events"].is_array());
    }

    #[test]
    fn derived_snapshot_events_do_not_mutate_live_event_sequence() {
        let recorder = Telemetry::default();
        {
            let guard = recorder.enter();
            increment(&guard.slot.realtime.scheduler_late_drops);
            increment(&guard.slot.pcm_bus_underrun_reads);
        }
        let before = recorder.slots[0]
            .next_event_sequence
            .load(Ordering::Acquire);
        let first = recorder.snapshot();
        let second = recorder.snapshot();
        let after = recorder.slots[0]
            .next_event_sequence
            .load(Ordering::Acquire);

        assert_eq!(first.recent_events, second.recent_events);
        assert_eq!(before, after);
    }

    #[test]
    fn control_session_history_is_bounded_without_evicting_active_connections() {
        let recorder = Telemetry::default();
        let peer: IpAddr = "192.0.2.40".parse().unwrap();
        let pinned_connection = ConnectionId::new();
        {
            let guard = recorder.enter();
            note_control_session(guard.slot, peer, pinned_connection);
        }
        for _ in 0..CONTROL_SESSION_HISTORY_LIMIT {
            let connection_id = ConnectionId::new();
            let guard = recorder.enter();
            note_control_session(guard.slot, peer, connection_id);
            guard
                .slot
                .active_control_sessions
                .lock()
                .unwrap()
                .remove(&control_session_identity(peer, connection_id));
        }

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot.control_sessions.len(),
            CONTROL_SESSION_HISTORY_LIMIT
        );
        assert!(snapshot.control_sessions_truncated);
        let pinned_identity = control_session_identity(peer, pinned_connection);
        assert_eq!(
            snapshot.active_control_sessions,
            vec![pinned_identity.clone()]
        );
        assert!(snapshot.control_sessions.contains(&pinned_identity));

        let reset = recorder.reset();
        assert!(reset.control_sessions.is_empty());
        assert!(!reset.control_sessions_truncated);
        assert!(reset.active_control_sessions.is_empty());
    }

    #[test]
    fn phase2_request_snapshot_distinguishes_prepared_and_committed_success() {
        let recorder = Telemetry::default();
        {
            let guard = recorder.enter();
            increment(&guard.slot.rtsp_phase2_prepared);
            increment(&guard.slot.rtsp_phase2_accepted);
            increment(&guard.slot.rtsp_phase2_failures_453);
        }
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counters.rtsp_phase2_accepted, 1);
        assert_eq!(snapshot.counters.rtsp_phase2_failures_total, 1);
        assert_eq!(snapshot.counters.rtsp_phase2_requests, 2);
        assert_eq!(snapshot.counters.rtsp_phase2_prepared, 1);
        assert_eq!(snapshot.counters.rtsp_phase2_committed_accepted, 1);
        assert_eq!(snapshot.counters.rtsp_phase2_prepared_inflight_gauge, 0);
        assert_eq!(
            snapshot.counters.rtsp_phase2_requests,
            snapshot
                .counters
                .rtsp_phase2_prepared
                .saturating_add(snapshot.counters.rtsp_phase2_failures_total)
        );
    }

    #[test]
    fn failure_counters_are_explicit_and_reset_with_their_generation() {
        let recorder = Telemetry::default();
        {
            let guard = recorder.enter();
            increment(&guard.slot.realtime.admission_rejections_sequence);
            increment(&guard.slot.realtime.jitter_rejections_outside_window);
            increment(&guard.slot.buffered.predecode_late_drops);
            increment(&guard.slot.pcm_bus_lock_contention_reads);
            increment(&guard.slot.pcm_bus_underrun_reads);
            increment(&guard.slot.pcm_bus_overflow_events);
        }
        let before = recorder.snapshot();
        assert_eq!(before.counters.realtime.admission_rejections_total, 1);
        assert_eq!(before.counters.realtime.jitter_rejections_total, 1);
        assert_eq!(before.counters.buffered.predecode_late_drops, 1);
        assert_eq!(before.counters.pcm_bus_failures_total, 3);
        assert_eq!(before.counters.pcm_bus_lock_contention_reads, 1);
        assert_eq!(before.counters.pcm_bus_underrun_reads, 1);
        assert_eq!(before.counters.pcm_bus_overflow_events, 1);
        assert_eq!(
            recorder.reset().counters,
            AirPlayTelemetryCounters::default()
        );
    }

    #[test]
    fn failure_recorders_update_the_public_json_counters() {
        let before = snapshot().counters;
        let token = operation_token();
        record_media_admission_rejection(token, MediaKind::Realtime, "sequence_out_of_window");
        record_jitter_rejection(token, MediaKind::Realtime, "outside_window");
        record_future_safety_wait(token, MediaKind::Realtime);
        record_scheduler_late_drop(token, MediaKind::Realtime);
        record_predecode_late_drop(token, MediaKind::Buffered);
        record_pcm_bus_push(token, 9, Some(9), 960, 480);
        record_pcm_bus_read(token, 0, 480, 0, true, false);
        record_pcm_bus_read(token, 0, 480, 0, false, true);

        let after = snapshot().counters;
        assert!(
            after.realtime.admission_rejections_total
                >= before.realtime.admission_rejections_total.saturating_add(1)
        );
        assert!(
            after.realtime.jitter_rejections_total
                >= before.realtime.jitter_rejections_total.saturating_add(1)
        );
        assert!(
            after.realtime.future_safety_waits
                >= before.realtime.future_safety_waits.saturating_add(1)
        );
        assert!(
            after.realtime.scheduler_late_drops
                >= before.realtime.scheduler_late_drops.saturating_add(1)
        );
        assert!(
            after.buffered.predecode_late_drops
                >= before.buffered.predecode_late_drops.saturating_add(1)
        );
        assert!(after.pcm_bus_overflow_events >= before.pcm_bus_overflow_events.saturating_add(1));
        assert!(
            after.pcm_bus_lock_contention_reads
                >= before.pcm_bus_lock_contention_reads.saturating_add(1)
        );
        assert!(after.pcm_bus_underrun_reads >= before.pcm_bus_underrun_reads.saturating_add(1));
    }

    #[test]
    fn repeated_ptp_observations_increment_counters_without_flooding_events() {
        let recorder = Telemetry::default();
        for _ in 0..10 {
            let guard = recorder.enter();
            increment(&guard.slot.ptp_datagrams_received);
            increment(&guard.slot.ptp_datagrams_rejected_source_port);
            if guard.slot.claim_event(EventCategory::PtpSourcePortMismatch) {
                guard.slot.push_event(AirPlayTelemetryEvent::PtpDatagram {
                    sequence: guard.slot.next_sequence(),
                    peer: "192.0.2.20".to_owned(),
                    source_port: 49_152,
                    local_port: 319,
                    message: None,
                    outcome: "rejected".to_owned(),
                    reason: "source_port_mismatch".to_owned(),
                });
            }
        }
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counters.ptp_datagrams_received, 10);
        assert_eq!(snapshot.counters.ptp_datagrams_rejected_source_port, 10);
        assert_eq!(snapshot.recent_events.len(), 1);
    }

    #[test]
    fn stale_operation_token_cannot_pollute_or_pin_later_generations() {
        let recorder = Telemetry::default();
        let old_token = recorder.operation_token();
        {
            let old = recorder.enter_token(old_token).unwrap();
            increment(&old.slot.rtsp_phase2_accepted);
        }

        let reset = recorder.reset();
        assert_eq!(reset.reset_generation, 1);
        assert_eq!(reset.counters, AirPlayTelemetryCounters::default());
        assert!(reset.recent_events.is_empty());

        assert!(recorder.enter_token(old_token).is_none());
        let current = recorder.snapshot();
        assert_eq!(current.reset_generation, 1);
        assert_eq!(current.generation_marker, reset.generation_marker);
        assert_eq!(current.reset_monotonic_ns, reset.reset_monotonic_ns);
        assert_eq!(current.counters, AirPlayTelemetryCounters::default());
        assert!(current.recent_events.is_empty());

        // Capturing an operation does not hold a slot user. The immediately
        // following reset can reuse the old parity slot without waiting for the
        // stale operation itself to finish.
        let second_reset = recorder.reset();
        assert_eq!(second_reset.reset_generation, 2);
        assert_ne!(second_reset.generation_marker, reset.generation_marker);
        assert!(second_reset.reset_monotonic_ns >= reset.reset_monotonic_ns);
        assert_eq!(second_reset.counters, AirPlayTelemetryCounters::default());
        assert!(second_reset.recent_events.is_empty());
        assert!(recorder.enter_token(old_token).is_none());
    }
}
