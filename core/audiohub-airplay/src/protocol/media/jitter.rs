//! Bounded, ordering-only packet buffer for the AirPlay UDP media path.
//!
//! This module deliberately has no wall-clock waits and performs no PCM
//! pacing. Its caller decides when a missing range should become a `d5`
//! retransmission request and when decoded audio should be presented.

use super::decode::MAX_COMPRESSED_PACKET_BYTES;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub(crate) const DEFAULT_PACKET_CAPACITY: usize = 128;
pub(crate) const DEFAULT_REORDER_WINDOW: u64 = 512;
const MAX_PACKET_CAPACITY: usize = 1_024;
const MAX_REORDER_WINDOW: u64 = 65_535;

pub(crate) struct BufferedPacket {
    pub(crate) sequence: u64,
    pub(crate) timestamp: u64,
    payload: Vec<u8>,
}

impl BufferedPacket {
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl fmt::Debug for BufferedPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferedPacket")
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MissingRange {
    pub(crate) first: u64,
    pub(crate) last: u64,
}

impl MissingRange {
    pub(crate) fn packet_count(self) -> u64 {
        self.last - self.first + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    Inserted,
    /// The new packet was closer to the dequeue point, so the farthest packet
    /// was discarded. `missing_ranges` continues to report the evicted value.
    ReplacedFarFuture {
        evicted_sequence: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JitterError {
    InvalidLimits,
    EmptyPayload,
    PayloadTooLarge {
        actual: usize,
        maximum: usize,
    },
    Duplicate {
        sequence: u64,
    },
    TooOld {
        sequence: u64,
        expected: u64,
    },
    OutsideWindow {
        sequence: u64,
        maximum: u64,
    },
    InvalidAdvance {
        requested: u64,
        minimum: u64,
        maximum: u64,
    },
    Full {
        capacity: usize,
    },
    SequenceExhausted,
}

impl fmt::Display for JitterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("jitter-buffer limits are invalid"),
            Self::EmptyPayload => f.write_str("media packet payload is empty"),
            Self::PayloadTooLarge { actual, maximum } => {
                write!(
                    f,
                    "media packet payload is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::Duplicate { sequence } => {
                write!(f, "media packet sequence {sequence} is already buffered")
            }
            Self::TooOld { sequence, expected } => write!(
                f,
                "media packet sequence {sequence} is older than expected sequence {expected}"
            ),
            Self::OutsideWindow { sequence, maximum } => write!(
                f,
                "media packet sequence {sequence} exceeds reorder-window maximum {maximum}"
            ),
            Self::InvalidAdvance {
                requested,
                minimum,
                maximum,
            } => write!(
                f,
                "cannot advance jitter buffer to {requested}; valid range is {minimum} through {maximum}"
            ),
            Self::Full { capacity } => {
                write!(
                    f,
                    "jitter buffer has reached its {capacity}-packet capacity"
                )
            }
            Self::SequenceExhausted => f.write_str("extended media sequence is exhausted"),
        }
    }
}

impl Error for JitterError {}

/// Keeps compressed packets ordered by their already-extended RTP sequence.
/// Both capacity and distance from the next dequeue point are bounded.
pub(crate) struct JitterBuffer {
    capacity: usize,
    reorder_window: u64,
    next_sequence: Option<u64>,
    highest_observed: Option<u64>,
    priming: bool,
    exhausted: bool,
    packets: BTreeMap<u64, BufferedPacket>,
}

impl JitterBuffer {
    pub(crate) fn type96() -> Self {
        Self::with_limits(DEFAULT_PACKET_CAPACITY, DEFAULT_REORDER_WINDOW)
            .expect("fixed type-96 jitter limits are valid")
    }

    pub(crate) fn with_limits(capacity: usize, reorder_window: u64) -> Result<Self, JitterError> {
        if capacity == 0
            || capacity > MAX_PACKET_CAPACITY
            || reorder_window == 0
            || reorder_window > MAX_REORDER_WINDOW
        {
            return Err(JitterError::InvalidLimits);
        }
        Ok(Self {
            capacity,
            reorder_window,
            next_sequence: None,
            highest_observed: None,
            priming: true,
            exhausted: false,
            packets: BTreeMap::new(),
        })
    }

    pub(crate) fn insert(
        &mut self,
        sequence: u64,
        timestamp: u64,
        payload: Vec<u8>,
    ) -> Result<InsertOutcome, JitterError> {
        if self.exhausted {
            return Err(JitterError::SequenceExhausted);
        }
        if payload.is_empty() {
            return Err(JitterError::EmptyPayload);
        }
        if payload.len() > MAX_COMPRESSED_PACKET_BYTES {
            return Err(JitterError::PayloadTooLarge {
                actual: payload.len(),
                maximum: MAX_COMPRESSED_PACKET_BYTES,
            });
        }

        let mut expected = *self.next_sequence.get_or_insert(sequence);
        if sequence < expected {
            let behind = expected - sequence;
            if !self.priming || behind > self.reorder_window {
                return Err(JitterError::TooOld { sequence, expected });
            }
            // Until playback has started, the first authenticated arrival is
            // only a provisional baseline. This admits the common case where
            // N+1 wins the UDP race and N arrives immediately afterwards.
            self.next_sequence = Some(sequence);
            expected = sequence;
        }
        let distance = sequence - expected;
        if distance > self.reorder_window {
            return Err(JitterError::OutsideWindow {
                sequence,
                maximum: expected.saturating_add(self.reorder_window),
            });
        }
        if self.packets.contains_key(&sequence) {
            return Err(JitterError::Duplicate { sequence });
        }

        let packet = BufferedPacket {
            sequence,
            timestamp,
            payload,
        };
        let outcome = if self.packets.len() == self.capacity {
            let farthest = *self
                .packets
                .last_key_value()
                .expect("a full non-zero-capacity buffer has a last key")
                .0;
            if sequence >= farthest {
                return Err(JitterError::Full {
                    capacity: self.capacity,
                });
            }
            self.packets.remove(&farthest);
            self.packets.insert(sequence, packet);
            InsertOutcome::ReplacedFarFuture {
                evicted_sequence: farthest,
            }
        } else {
            self.packets.insert(sequence, packet);
            InsertOutcome::Inserted
        };
        self.highest_observed = Some(
            self.highest_observed
                .map_or(sequence, |highest| highest.max(sequence)),
        );
        Ok(outcome)
    }

    pub(crate) fn pop_ready(&mut self) -> Option<BufferedPacket> {
        // Keep a one-packet startup reserve. Normal type-96 traffic fills this
        // in roughly one packet duration, while allowing an immediately
        // reordered predecessor to move the provisional baseline backwards.
        if self.priming {
            let prime_target = self.capacity.min(2);
            if self.packets.len() < prime_target {
                return None;
            }
            self.priming = false;
        }
        let expected = self.next_sequence?;
        let packet = self.packets.remove(&expected)?;
        if expected == u64::MAX {
            self.next_sequence = None;
            self.exhausted = true;
        } else {
            self.next_sequence = Some(expected + 1);
        }
        Some(packet)
    }

    /// Advances the dequeue point after the caller has exhausted its bounded
    /// retransmission policy. Packets skipped by this operation are discarded;
    /// the caller should account for their duration as silence/discontinuity.
    ///
    /// `new_next` must move forward and may not pass one beyond the highest
    /// sequence ever accepted. This keeps a timeout path from jumping across
    /// an unobserved, attacker-selected sequence space.
    pub(crate) fn advance_to(&mut self, new_next: u64) -> Result<(), JitterError> {
        if self.exhausted {
            return Err(JitterError::SequenceExhausted);
        }
        let current = self.next_sequence.ok_or(JitterError::InvalidAdvance {
            requested: new_next,
            minimum: 0,
            maximum: 0,
        })?;
        let minimum = current
            .checked_add(1)
            .ok_or(JitterError::SequenceExhausted)?;
        let highest = self.highest_observed.unwrap_or(current);
        let observed_maximum = highest.saturating_add(1);
        let window_maximum = current.saturating_add(self.reorder_window);
        let maximum = observed_maximum.min(window_maximum);
        if new_next < minimum || new_next > maximum {
            return Err(JitterError::InvalidAdvance {
                requested: new_next,
                minimum,
                maximum,
            });
        }

        self.priming = false;
        self.packets = self.packets.split_off(&new_next);
        if new_next == u64::MAX && new_next > highest {
            self.next_sequence = None;
            self.exhausted = true;
        } else {
            self.next_sequence = Some(new_next);
        }
        Ok(())
    }

    /// Returns inclusive extended-sequence gaps between the dequeue point and
    /// the highest packet accepted so far. These ranges are the bounded input
    /// from which the transport layer can create `d5` retransmission requests.
    pub(crate) fn missing_ranges(&self) -> Vec<MissingRange> {
        let (Some(mut cursor), Some(highest)) = (self.next_sequence, self.highest_observed) else {
            return Vec::new();
        };
        if cursor > highest {
            return Vec::new();
        }

        let mut missing = Vec::new();
        for sequence in self.packets.range(cursor..=highest).map(|(key, _)| *key) {
            if sequence > cursor {
                missing.push(MissingRange {
                    first: cursor,
                    last: sequence - 1,
                });
            }
            if sequence == u64::MAX {
                return missing;
            }
            cursor = sequence + 1;
        }
        if cursor <= highest {
            missing.push(MissingRange {
                first: cursor,
                last: highest,
            });
        }
        missing
    }

    pub(crate) fn len(&self) -> usize {
        self.packets.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Ends startup priming when the transport's short prime deadline expires.
    /// This prevents a one-packet stream from waiting indefinitely.
    pub(crate) fn finish_priming(&mut self) {
        self.priming = false;
    }
}

impl fmt::Debug for JitterBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JitterBuffer")
            .field("capacity", &self.capacity)
            .field("reorder_window", &self.reorder_window)
            .field("next_sequence", &self.next_sequence)
            .field("highest_observed", &self.highest_observed)
            .field("priming", &self.priming)
            .field("buffered_packets", &self.packets.len())
            .field("exhausted", &self.exhausted)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(value: u8) -> Vec<u8> {
        vec![value]
    }

    #[test]
    fn dequeues_in_order_across_a_wire_sequence_wrap() {
        let mut jitter = JitterBuffer::with_limits(4, 8).unwrap();
        jitter.insert(65_534, 1_000, payload(1)).unwrap();
        assert!(jitter.pop_ready().is_none());

        jitter.insert(65_536, 1_704, payload(3)).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 65_534);
        assert_eq!(
            jitter.missing_ranges(),
            vec![MissingRange {
                first: 65_535,
                last: 65_535,
            }]
        );
        jitter.insert(65_535, 1_352, payload(2)).unwrap();

        let before_wrap = jitter.pop_ready().unwrap();
        let after_wrap = jitter.pop_ready().unwrap();
        assert_eq!(
            (before_wrap.sequence, before_wrap.timestamp),
            (65_535, 1_352)
        );
        assert_eq!((after_wrap.sequence, after_wrap.timestamp), (65_536, 1_704));
        assert!(jitter.missing_ranges().is_empty());
    }

    #[test]
    fn reports_disjoint_missing_ranges_for_retransmission() {
        let mut jitter = JitterBuffer::with_limits(8, 16).unwrap();
        for sequence in [100, 102, 105, 106] {
            jitter.insert(sequence, sequence * 352, payload(1)).unwrap();
        }
        assert_eq!(
            jitter.missing_ranges(),
            vec![
                MissingRange {
                    first: 101,
                    last: 101,
                },
                MissingRange {
                    first: 103,
                    last: 104,
                },
            ]
        );
        assert_eq!(jitter.missing_ranges()[1].packet_count(), 2);

        assert_eq!(jitter.pop_ready().unwrap().sequence, 100);
        jitter.insert(101, 101 * 352, payload(2)).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 101);
        assert_eq!(jitter.pop_ready().unwrap().sequence, 102);
    }

    #[test]
    fn rejects_duplicates_late_packets_and_packets_beyond_the_window() {
        let mut jitter = JitterBuffer::with_limits(4, 4).unwrap();
        jitter.insert(10, 20, payload(1)).unwrap();
        assert_eq!(
            jitter.insert(10, 20, payload(1)).unwrap_err(),
            JitterError::Duplicate { sequence: 10 }
        );
        jitter.finish_priming();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 10);
        assert_eq!(
            jitter.insert(10, 20, payload(1)).unwrap_err(),
            JitterError::TooOld {
                sequence: 10,
                expected: 11,
            }
        );
        assert_eq!(
            jitter.insert(16, 30, payload(1)).unwrap_err(),
            JitterError::OutsideWindow {
                sequence: 16,
                maximum: 15,
            }
        );
    }

    #[test]
    fn capacity_keeps_nearest_packets_and_reports_eviction_as_missing() {
        let mut jitter = JitterBuffer::with_limits(2, 8).unwrap();
        jitter.insert(10, 0, payload(1)).unwrap();
        jitter.finish_priming();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 10);
        jitter.insert(12, 704, payload(2)).unwrap();
        jitter.insert(13, 1_056, payload(3)).unwrap();

        assert_eq!(
            jitter.insert(11, 352, payload(1)).unwrap(),
            InsertOutcome::ReplacedFarFuture {
                evicted_sequence: 13,
            }
        );
        assert_eq!(jitter.pop_ready().unwrap().sequence, 11);
        assert_eq!(jitter.pop_ready().unwrap().sequence, 12);
        assert_eq!(
            jitter.missing_ranges(),
            vec![MissingRange {
                first: 13,
                last: 13,
            }]
        );
        jitter.advance_to(14).unwrap();
        assert!(jitter.is_empty());
    }

    #[test]
    fn retransmission_timeout_can_advance_past_a_gap_without_stalling() {
        let mut jitter = JitterBuffer::with_limits(4, 8).unwrap();
        jitter.insert(100, 0, payload(1)).unwrap();
        jitter.finish_priming();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 100);
        jitter.insert(103, 1_056, payload(3)).unwrap();
        jitter.insert(104, 1_408, payload(4)).unwrap();

        assert_eq!(
            jitter.missing_ranges(),
            vec![MissingRange {
                first: 101,
                last: 102,
            }]
        );
        jitter.advance_to(103).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 103);
        assert_eq!(jitter.pop_ready().unwrap().sequence, 104);
    }

    #[test]
    fn advance_is_forward_only_and_cannot_cross_unobserved_space() {
        let mut jitter = JitterBuffer::with_limits(4, 4).unwrap();
        jitter.insert(10, 0, payload(1)).unwrap();
        jitter.insert(12, 704, payload(2)).unwrap();

        for requested in [10, 14, 20] {
            assert!(matches!(
                jitter.advance_to(requested),
                Err(JitterError::InvalidAdvance {
                    requested: actual,
                    minimum: 11,
                    maximum: 13,
                }) if actual == requested
            ));
        }
        jitter.advance_to(11).unwrap();
        assert_eq!(
            jitter.missing_ranges(),
            vec![MissingRange {
                first: 11,
                last: 11,
            }]
        );
        jitter.advance_to(12).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 12);
    }

    #[test]
    fn rejects_limits_full_buffer_and_malicious_payload_sizes() {
        assert!(matches!(
            JitterBuffer::with_limits(0, 1),
            Err(JitterError::InvalidLimits)
        ));
        assert!(matches!(
            JitterBuffer::with_limits(MAX_PACKET_CAPACITY + 1, 1),
            Err(JitterError::InvalidLimits)
        ));

        let mut jitter = JitterBuffer::with_limits(1, 4).unwrap();
        assert_eq!(
            jitter.insert(1, 1, Vec::new()).unwrap_err(),
            JitterError::EmptyPayload
        );
        assert_eq!(
            jitter
                .insert(1, 1, vec![0xaa; MAX_COMPRESSED_PACKET_BYTES + 1])
                .unwrap_err(),
            JitterError::PayloadTooLarge {
                actual: MAX_COMPRESSED_PACKET_BYTES + 1,
                maximum: MAX_COMPRESSED_PACKET_BYTES,
            }
        );
        jitter.insert(1, 1, payload(1)).unwrap();
        assert_eq!(
            jitter.insert(2, 2, payload(2)).unwrap_err(),
            JitterError::Full { capacity: 1 }
        );
        assert_eq!(jitter.len(), 1);
    }

    #[test]
    fn debug_output_does_not_expose_compressed_payload() {
        let mut jitter = JitterBuffer::type96();
        let secret = vec![0xde, 0xad, 0xbe, 0xef];
        jitter.insert(1, 2, secret).unwrap();
        let packet_debug = format!("{:?}", jitter.packets.get(&1).unwrap());
        let jitter_debug = format!("{jitter:?}");
        assert!(packet_debug.contains("payload_bytes: 4"));
        assert!(!packet_debug.contains("222"));
        assert!(!packet_debug.contains("173"));
        assert!(!jitter_debug.contains("222"));
    }

    #[test]
    fn startup_prime_accepts_n_after_n_plus_one_and_plays_both_in_order() {
        let mut jitter = JitterBuffer::with_limits(4, 8).unwrap();
        jitter.insert(101, 352, payload(2)).unwrap();
        assert!(jitter.pop_ready().is_none());

        jitter.insert(100, 0, payload(1)).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 100);
        assert_eq!(jitter.pop_ready().unwrap().sequence, 101);
    }

    #[test]
    fn startup_prime_handles_zero_before_65535_across_wrap() {
        use super::super::packet::SequenceExtender16;

        let mut extender = SequenceExtender16::new();
        let zero = extender.observe(0).unwrap().value;
        let before_wrap = extender.observe(u16::MAX).unwrap().value;
        assert_eq!((before_wrap, zero), (65_535, 65_536));

        let mut jitter = JitterBuffer::with_limits(4, 8).unwrap();
        jitter.insert(zero, 352, payload(2)).unwrap();
        assert!(jitter.pop_ready().is_none());
        jitter.insert(before_wrap, 0, payload(1)).unwrap();
        assert_eq!(jitter.pop_ready().unwrap().sequence, before_wrap);
        assert_eq!(jitter.pop_ready().unwrap().sequence, zero);
    }

    #[test]
    fn prime_deadline_can_release_a_single_packet() {
        let mut jitter = JitterBuffer::with_limits(4, 8).unwrap();
        jitter.insert(100, 0, payload(1)).unwrap();
        assert!(jitter.pop_ready().is_none());
        jitter.finish_priming();
        assert_eq!(jitter.pop_ready().unwrap().sequence, 100);
    }
}
