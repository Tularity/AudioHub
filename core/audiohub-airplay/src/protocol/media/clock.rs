//! Pure packet and estimation primitives for AirPlay's UDP timing clock.

use std::collections::VecDeque;

pub(crate) const TIMING_REQUEST_LEN: usize = 32;
pub(crate) const TIMING_RESPONSE_LEN: usize = 32;
pub(crate) const SYNC_ANCHOR_LEN: usize = 20;

const RTP_V2_FIXED: u8 = 0x80;
const TIMING_REQUEST_TYPE: u8 = 0xd2;
const TIMING_RESPONSE_TYPE: u8 = 0xd3;
const SYNC_ANCHOR_TYPE: u8 = 0xd4;
const MAX_TIMING_WINDOW_CAPACITY: usize = 64;
const NTP_UNIX_EPOCH_OFFSET_SECONDS: u64 = 2_208_988_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockError {
    WrongLength { expected: usize, actual: usize },
    InvalidHeader,
    UnexpectedSequence { expected: u16, actual: u16 },
    OriginMismatch,
    ResponseExpired,
    MissingTimestamp,
    RemoteClockWentBackwards,
    LocalClockWentBackwards,
    RemoteProcessingExceedsLocalRoundTrip,
    InvalidLimits,
    RttTooLarge { actual_ns: u64, maximum_ns: u64 },
    NonMonotonicObservation,
}

/// The 32-bit-seconds / 32-bit-fraction timestamp carried by AirPlay timing
/// packets. This is deliberately epoch-agnostic: the clock estimator only
/// needs all remote timestamps to share one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NtpTimestamp(u64);

impl NtpTimestamp {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) fn from_parts(seconds: u32, fraction: u32) -> Self {
        Self(((seconds as u64) << 32) | fraction as u64)
    }

    pub(crate) fn from_nanos(nanos: u64) -> Option<Self> {
        let seconds = nanos / 1_000_000_000;
        let fraction_nanos = nanos % 1_000_000_000;
        if seconds > u32::MAX as u64 {
            return None;
        }
        let fraction = ((fraction_nanos as u128) << 32) / 1_000_000_000u128;
        Some(Self::from_parts(seconds as u32, fraction as u32))
    }

    /// Convert a Unix-epoch timestamp to the NTP epoch used on the wire.
    /// AirPlay's 32-bit seconds field remains unambiguous for the product's
    /// current supported time range (before the 2036 NTP era rollover).
    pub(crate) fn from_unix_nanos(nanos: u64) -> Option<Self> {
        let offset = NTP_UNIX_EPOCH_OFFSET_SECONDS.checked_mul(1_000_000_000)?;
        Self::from_nanos(nanos.checked_add(offset)?)
    }

    pub(crate) fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub(crate) fn fraction(self) -> u32 {
        self.0 as u32
    }

    pub(crate) fn to_nanos(self) -> u64 {
        self.seconds() as u64 * 1_000_000_000
            + (((self.fraction() as u128) * 1_000_000_000u128) >> 32) as u64
    }

    fn decode(bytes: &[u8]) -> Self {
        Self(u64::from_be_bytes(
            bytes.try_into().expect("eight-byte timestamp"),
        ))
    }
}

/// Correlation fields returned in the next `d2` after one successful timing
/// response. Apple senders use them to track the receiver's prior exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimingFeedback {
    pub(crate) remote_reference: NtpTimestamp,
    pub(crate) local_receive: NtpTimestamp,
}

/// Builds the fixed 32-byte `d2` timing probe used by the UDP clock exchange.
/// The current local NTP timestamp is carried in the transmit field and is
/// echoed into the response origin field. After a successful exchange, the
/// next request also carries the previous remote transmit timestamp and local
/// receive timestamp.
pub(crate) fn build_timing_request(
    sequence: u16,
    transmit: NtpTimestamp,
    feedback: Option<TimingFeedback>,
) -> [u8; TIMING_REQUEST_LEN] {
    let mut packet = [0u8; TIMING_REQUEST_LEN];
    packet[0] = RTP_V2_FIXED;
    packet[1] = TIMING_REQUEST_TYPE;
    packet[2..4].copy_from_slice(&sequence.to_be_bytes());
    if let Some(feedback) = feedback {
        packet[8..16].copy_from_slice(&feedback.remote_reference.0.to_be_bytes());
        packet[16..24].copy_from_slice(&feedback.local_receive.0.to_be_bytes());
    }
    packet[24..32].copy_from_slice(&transmit.0.to_be_bytes());
    packet
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimingResponse {
    pub(crate) sequence: u16,
    pub(crate) origin: NtpTimestamp,
    pub(crate) remote_receive: NtpTimestamp,
    pub(crate) remote_transmit: NtpTimestamp,
}

/// Correlation data retained beside an outstanding `d2` probe. Wall-clock NTP
/// nanoseconds are used only in the four-timestamp calculation. A separate
/// monotonic deadline prevents a system-clock adjustment from extending the
/// response lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingTimingRequest {
    pub(crate) sequence: u16,
    pub(crate) origin: NtpTimestamp,
    pub(crate) local_departure_ns: u64,
    pub(crate) expires_at_monotonic_ns: u64,
}

impl PendingTimingRequest {
    pub(crate) fn validate_response(
        self,
        response: TimingResponse,
        local_arrival_ns: u64,
        local_arrival_monotonic_ns: u64,
    ) -> Result<TimingSample, ClockError> {
        if response.sequence != self.sequence {
            return Err(ClockError::UnexpectedSequence {
                expected: self.sequence,
                actual: response.sequence,
            });
        }
        if local_arrival_monotonic_ns > self.expires_at_monotonic_ns {
            return Err(ClockError::ResponseExpired);
        }
        if response.origin != self.origin {
            return Err(ClockError::OriginMismatch);
        }
        TimingSample::from_exchange(self.local_departure_ns, response, local_arrival_ns)
    }
}

impl TimingResponse {
    pub(crate) fn parse(packet: &[u8], expected_sequence: Option<u16>) -> Result<Self, ClockError> {
        if packet.len() != TIMING_RESPONSE_LEN {
            return Err(ClockError::WrongLength {
                expected: TIMING_RESPONSE_LEN,
                actual: packet.len(),
            });
        }
        if packet[0] != RTP_V2_FIXED
            || packet[1] != TIMING_RESPONSE_TYPE
            || packet[4..8] != [0, 0, 0, 0]
        {
            return Err(ClockError::InvalidHeader);
        }

        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        if let Some(expected) = expected_sequence {
            if sequence != expected {
                return Err(ClockError::UnexpectedSequence {
                    expected,
                    actual: sequence,
                });
            }
        }

        let origin = NtpTimestamp::decode(&packet[8..16]);
        let remote_receive = NtpTimestamp::decode(&packet[16..24]);
        let remote_transmit = NtpTimestamp::decode(&packet[24..32]);
        if remote_receive == NtpTimestamp::ZERO || remote_transmit == NtpTimestamp::ZERO {
            return Err(ClockError::MissingTimestamp);
        }
        if remote_transmit < remote_receive {
            return Err(ClockError::RemoteClockWentBackwards);
        }

        Ok(Self {
            sequence,
            origin,
            remote_receive,
            remote_transmit,
        })
    }
}

/// One four-timestamp clock measurement. `offset_ns` is remote minus local;
/// positive means the sender's clock is ahead of AudioHub's local clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimingSample {
    pub(crate) offset_ns: i128,
    pub(crate) rtt_ns: u64,
}

impl TimingSample {
    pub(crate) fn from_exchange(
        local_departure_ns: u64,
        response: TimingResponse,
        local_arrival_ns: u64,
    ) -> Result<Self, ClockError> {
        let local_elapsed = local_arrival_ns
            .checked_sub(local_departure_ns)
            .ok_or(ClockError::LocalClockWentBackwards)?;
        let remote_receive_ns = response.remote_receive.to_nanos();
        let remote_transmit_ns = response.remote_transmit.to_nanos();
        let remote_processing = remote_transmit_ns
            .checked_sub(remote_receive_ns)
            .ok_or(ClockError::RemoteClockWentBackwards)?;
        let rtt_ns = local_elapsed
            .checked_sub(remote_processing)
            .ok_or(ClockError::RemoteProcessingExceedsLocalRoundTrip)?;

        let t1 = local_departure_ns as i128;
        let t2 = remote_receive_ns as i128;
        let t3 = remote_transmit_ns as i128;
        let t4 = local_arrival_ns as i128;
        let offset_ns = ((t2 - t1) + (t3 - t4)) / 2;

        Ok(Self { offset_ns, rtt_ns })
    }
}

#[derive(Debug, Clone, Copy)]
struct StampedSample {
    sample: TimingSample,
    observed_at_ns: u64,
}

/// A bounded collection that exposes the unexpired, lowest-RTT clock sample.
/// The limit values are injected, keeping tests and future virtual-clock use
/// independent from wall time.
#[derive(Debug)]
pub(crate) struct TimingWindow {
    capacity: usize,
    max_age_ns: u64,
    max_rtt_ns: u64,
    last_observation_ns: Option<u64>,
    samples: VecDeque<StampedSample>,
}

impl TimingWindow {
    pub(crate) fn new(
        capacity: usize,
        max_age_ns: u64,
        max_rtt_ns: u64,
    ) -> Result<Self, ClockError> {
        if capacity == 0
            || capacity > MAX_TIMING_WINDOW_CAPACITY
            || max_age_ns == 0
            || max_rtt_ns == 0
        {
            return Err(ClockError::InvalidLimits);
        }
        Ok(Self {
            capacity,
            max_age_ns,
            max_rtt_ns,
            last_observation_ns: None,
            samples: VecDeque::with_capacity(capacity),
        })
    }

    pub(crate) fn insert(
        &mut self,
        sample: TimingSample,
        observed_at_ns: u64,
    ) -> Result<(), ClockError> {
        if sample.rtt_ns > self.max_rtt_ns {
            return Err(ClockError::RttTooLarge {
                actual_ns: sample.rtt_ns,
                maximum_ns: self.max_rtt_ns,
            });
        }
        if self
            .last_observation_ns
            .is_some_and(|last| observed_at_ns < last)
        {
            return Err(ClockError::NonMonotonicObservation);
        }

        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(StampedSample {
            sample,
            observed_at_ns,
        });
        self.last_observation_ns = Some(observed_at_ns);
        Ok(())
    }

    pub(crate) fn best(&self, now_ns: u64) -> Option<TimingSample> {
        self.samples
            .iter()
            .filter(|entry| {
                now_ns
                    .checked_sub(entry.observed_at_ns)
                    .is_some_and(|age| age <= self.max_age_ns)
            })
            .min_by_key(|entry| entry.sample.rtt_ns)
            .map(|entry| entry.sample)
    }
}

/// The 20-byte `d4` control packet that anchors an RTP time to the sender's
/// NTP-like clock. The layout is verified against an independent fixed wire
/// vector, but this parser is independently implemented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyncAnchor {
    pub(crate) initial: bool,
    pub(crate) flags_or_sequence: u16,
    pub(crate) rtp_at_dac: u32,
    pub(crate) remote_time: NtpTimestamp,
    pub(crate) rtp_current: u32,
}

impl SyncAnchor {
    pub(crate) fn parse(packet: &[u8]) -> Result<Self, ClockError> {
        if packet.len() != SYNC_ANCHOR_LEN {
            return Err(ClockError::WrongLength {
                expected: SYNC_ANCHOR_LEN,
                actual: packet.len(),
            });
        }
        let initial = match packet[0] {
            0x80 => false,
            0x90 => true,
            _ => return Err(ClockError::InvalidHeader),
        };
        if packet[1] != SYNC_ANCHOR_TYPE {
            return Err(ClockError::InvalidHeader);
        }

        let remote_time = NtpTimestamp::decode(&packet[8..16]);
        if remote_time == NtpTimestamp::ZERO {
            return Err(ClockError::MissingTimestamp);
        }

        Ok(Self {
            initial,
            flags_or_sequence: u16::from_be_bytes([packet[2], packet[3]]),
            rtp_at_dac: u32::from_be_bytes(packet[4..8].try_into().expect("fixed slice")),
            remote_time,
            rtp_current: u32::from_be_bytes(packet[16..20].try_into().expect("fixed slice")),
        })
    }

    pub(crate) fn latency_frames(self) -> u32 {
        self.rtp_current.wrapping_sub(self.rtp_at_dac)
    }

    pub(crate) fn local_anchor_ns(self, sample: TimingSample) -> Option<u64> {
        let local = self.remote_time.to_nanos() as i128 - sample.offset_ns;
        u64::try_from(local).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(
        sequence: u16,
        origin: NtpTimestamp,
        receive: NtpTimestamp,
        transmit: NtpTimestamp,
    ) -> [u8; TIMING_RESPONSE_LEN] {
        let mut packet = [0u8; TIMING_RESPONSE_LEN];
        packet[0] = 0x80;
        packet[1] = 0xd3;
        packet[2..4].copy_from_slice(&sequence.to_be_bytes());
        packet[8..16].copy_from_slice(&origin.0.to_be_bytes());
        packet[16..24].copy_from_slice(&receive.0.to_be_bytes());
        packet[24..32].copy_from_slice(&transmit.0.to_be_bytes());
        packet
    }

    fn ntp(nanos: u64) -> NtpTimestamp {
        NtpTimestamp::from_nanos(nanos).unwrap()
    }

    #[test]
    fn builds_fixed_d2_request_with_correlation_timestamps() {
        let transmit = ntp(4_000_000_000);
        let packet = build_timing_request(0x1234, transmit, None);
        assert_eq!(&packet[..4], [0x80, 0xd2, 0x12, 0x34]);
        assert!(packet[4..24].iter().all(|byte| *byte == 0));
        assert_eq!(&packet[24..32], &transmit.0.to_be_bytes());

        let feedback = TimingFeedback {
            remote_reference: ntp(5_000_000_000),
            local_receive: ntp(4_025_000_000),
        };
        let packet = build_timing_request(7, transmit, Some(feedback));
        assert_eq!(&packet[8..16], &feedback.remote_reference.0.to_be_bytes());
        assert_eq!(&packet[16..24], &feedback.local_receive.0.to_be_bytes());
        assert_eq!(&packet[24..32], &transmit.0.to_be_bytes());
    }

    #[test]
    fn ntp_fixed_point_round_trip_is_within_one_nanosecond() {
        for value in [1, 500_000_000, 1_234_567_890, 4_000_000_000_000_000_000] {
            let decoded = ntp(value).to_nanos();
            assert!(decoded.abs_diff(value) <= 1, "{value} -> {decoded}");
        }
    }

    #[test]
    fn unix_epoch_conversion_adds_ntp_epoch_offset() {
        let timestamp = NtpTimestamp::from_unix_nanos(1_500_000_000).unwrap();
        assert_eq!(timestamp.seconds(), 2_208_988_801);
        assert!(timestamp.to_nanos().abs_diff(2_208_988_801_500_000_000) <= 1);
    }

    #[test]
    fn strictly_parses_d3_response() {
        let packet = response(
            7,
            NtpTimestamp::ZERO,
            ntp(3_010_000_000),
            ntp(3_012_000_000),
        );
        let parsed = TimingResponse::parse(&packet, Some(7)).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.origin, NtpTimestamp::ZERO);
        assert_eq!(parsed.remote_receive.to_nanos(), 3_009_999_999);

        assert_eq!(
            TimingResponse::parse(&packet[..31], Some(7)),
            Err(ClockError::WrongLength {
                expected: 32,
                actual: 31
            })
        );
        assert_eq!(
            TimingResponse::parse(&packet, Some(8)),
            Err(ClockError::UnexpectedSequence {
                expected: 8,
                actual: 7
            })
        );

        let mut malformed = packet;
        malformed[4] = 1;
        assert_eq!(
            TimingResponse::parse(&malformed, None),
            Err(ClockError::InvalidHeader)
        );
        malformed = packet;
        malformed[16..24].fill(0);
        assert_eq!(
            TimingResponse::parse(&malformed, None),
            Err(ClockError::MissingTimestamp)
        );
    }

    #[test]
    fn computes_positive_and_negative_four_timestamp_offsets() {
        // +2 s remote offset, 10 ms each direction, 2 ms remote processing.
        let positive_packet = response(
            1,
            NtpTimestamp::ZERO,
            ntp(3_010_000_000),
            ntp(3_012_000_000),
        );
        let positive = TimingSample::from_exchange(
            1_000_000_000,
            TimingResponse::parse(&positive_packet, Some(1)).unwrap(),
            1_022_000_000,
        )
        .unwrap();
        assert!((positive.offset_ns - 2_000_000_000).abs() <= 1);
        assert!(positive.rtt_ns.abs_diff(20_000_000) <= 1);

        // -500 ms remote offset, 5 ms each direction, 1 ms processing.
        let negative_packet = response(
            2,
            NtpTimestamp::ZERO,
            ntp(1_505_000_000),
            ntp(1_506_000_000),
        );
        let negative = TimingSample::from_exchange(
            2_000_000_000,
            TimingResponse::parse(&negative_packet, Some(2)).unwrap(),
            2_011_000_000,
        )
        .unwrap();
        assert!((negative.offset_ns + 500_000_000).abs() <= 1);
        assert!(negative.rtt_ns.abs_diff(10_000_000) <= 1);
    }

    #[test]
    fn pending_request_binds_sequence_origin_and_lifetime() {
        let expected_origin = ntp(1_000_000_000);
        let packet = response(9, expected_origin, ntp(3_010_000_000), ntp(3_012_000_000));
        let parsed = TimingResponse::parse(&packet, None).unwrap();
        let pending = PendingTimingRequest {
            sequence: 9,
            origin: expected_origin,
            local_departure_ns: 1_000_000_000,
            expires_at_monotonic_ns: 500_000_000,
        };
        assert!(pending
            .validate_response(parsed, 1_022_000_000, 420_000_000)
            .is_ok());

        assert_eq!(
            PendingTimingRequest {
                sequence: 10,
                ..pending
            }
            .validate_response(parsed, 1_022_000_000, 420_000_000),
            Err(ClockError::UnexpectedSequence {
                expected: 10,
                actual: 9
            })
        );
        assert_eq!(
            PendingTimingRequest {
                origin: ntp(2_000_000_000),
                ..pending
            }
            .validate_response(parsed, 1_022_000_000, 420_000_000),
            Err(ClockError::OriginMismatch)
        );
        assert_eq!(
            pending.validate_response(parsed, 1_022_000_000, 500_000_001),
            Err(ClockError::ResponseExpired)
        );

        // Even a deliberately zero origin is matched exactly; callers cannot
        // disable origin correlation by choosing a sentinel value.
        let zero_origin_packet = response(
            9,
            NtpTimestamp::ZERO,
            ntp(3_010_000_000),
            ntp(3_012_000_000),
        );
        assert!(PendingTimingRequest {
            origin: NtpTimestamp::ZERO,
            ..pending
        }
        .validate_response(
            TimingResponse::parse(&zero_origin_packet, None).unwrap(),
            1_022_000_000,
            420_000_000,
        )
        .is_ok());
    }

    #[test]
    fn rejects_impossible_exchange() {
        let packet = response(
            3,
            NtpTimestamp::ZERO,
            ntp(2_000_000_000),
            ntp(2_020_000_000),
        );
        let response = TimingResponse::parse(&packet, None).unwrap();
        assert_eq!(
            TimingSample::from_exchange(1_000_000_000, response, 1_010_000_000),
            Err(ClockError::RemoteProcessingExceedsLocalRoundTrip)
        );
        assert_eq!(
            TimingSample::from_exchange(20, response, 10),
            Err(ClockError::LocalClockWentBackwards)
        );
    }

    #[test]
    fn timing_window_selects_lowest_rtt_and_expires_it() {
        let mut window = TimingWindow::new(3, 100, 1_000).unwrap();
        window
            .insert(
                TimingSample {
                    offset_ns: 10,
                    rtt_ns: 80,
                },
                1_000,
            )
            .unwrap();
        window
            .insert(
                TimingSample {
                    offset_ns: 20,
                    rtt_ns: 20,
                },
                1_010,
            )
            .unwrap();
        window
            .insert(
                TimingSample {
                    offset_ns: 30,
                    rtt_ns: 40,
                },
                1_020,
            )
            .unwrap();

        assert_eq!(window.best(1_050).unwrap().offset_ns, 20);
        assert_eq!(window.best(1_111).unwrap().offset_ns, 30);
        assert_eq!(window.best(1_121), None);
    }

    #[test]
    fn timing_window_rejects_invalid_and_out_of_order_observations() {
        assert!(matches!(
            TimingWindow::new(0, 1, 1),
            Err(ClockError::InvalidLimits)
        ));
        assert!(matches!(
            TimingWindow::new(MAX_TIMING_WINDOW_CAPACITY + 1, 1, 1),
            Err(ClockError::InvalidLimits)
        ));
        let mut window = TimingWindow::new(2, 100, 50).unwrap();
        assert_eq!(
            window.insert(
                TimingSample {
                    offset_ns: 0,
                    rtt_ns: 51
                },
                10
            ),
            Err(ClockError::RttTooLarge {
                actual_ns: 51,
                maximum_ns: 50
            })
        );
        window
            .insert(
                TimingSample {
                    offset_ns: 0,
                    rtt_ns: 10,
                },
                20,
            )
            .unwrap();
        assert_eq!(
            window.insert(
                TimingSample {
                    offset_ns: 0,
                    rtt_ns: 10
                },
                19
            ),
            Err(ClockError::NonMonotonicObservation)
        );
        assert_eq!(window.best(19), None, "future-dated samples are not usable");
    }

    #[test]
    fn parses_fixed_d4_anchor_and_wraps_latency() {
        let remote_time = ntp(4_000_000_000);
        let mut packet = [0u8; SYNC_ANCHOR_LEN];
        packet[0] = 0x90;
        packet[1] = 0xd4;
        packet[2..4].copy_from_slice(&7u16.to_be_bytes());
        packet[4..8].copy_from_slice(&(u32::MAX - 9).to_be_bytes());
        packet[8..16].copy_from_slice(&remote_time.0.to_be_bytes());
        packet[16..20].copy_from_slice(&20u32.to_be_bytes());

        let anchor = SyncAnchor::parse(&packet).unwrap();
        assert!(anchor.initial);
        assert_eq!(anchor.flags_or_sequence, 7);
        assert_eq!(anchor.rtp_at_dac, u32::MAX - 9);
        assert_eq!(anchor.rtp_current, 20);
        assert_eq!(anchor.latency_frames(), 30);
        assert_eq!(
            anchor.local_anchor_ns(TimingSample {
                offset_ns: 1_000_000_000,
                rtt_ns: 1
            }),
            Some(3_000_000_000)
        );
    }

    #[test]
    fn rejects_invalid_d4_anchor() {
        let mut packet = [0u8; SYNC_ANCHOR_LEN];
        packet[0] = 0x80;
        packet[1] = 0xd4;
        assert_eq!(
            SyncAnchor::parse(&packet),
            Err(ClockError::MissingTimestamp)
        );

        packet[8..16].copy_from_slice(&ntp(1_000_000_000).0.to_be_bytes());
        packet[0] = 0xa0;
        assert_eq!(SyncAnchor::parse(&packet), Err(ClockError::InvalidHeader));
        assert_eq!(
            SyncAnchor::parse(&packet[..19]),
            Err(ClockError::WrongLength {
                expected: 20,
                actual: 19
            })
        );
    }
}
