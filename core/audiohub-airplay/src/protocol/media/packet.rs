//! Strict parsing and wrap-aware numbering for the UDP media path.

use std::collections::VecDeque;
use std::fmt;

const RTP_HEADER_LEN: usize = 12;
const RECENT_OBSERVATIONS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketError {
    TooShort,
    UnsupportedVersion(u8),
    PaddingUnsupported,
    ExtensionUnsupported,
    CsrcUnsupported(u8),
    EmptyPayload,
}

/// An RTP packet with the fixed, 12-byte header used by AirPlay audio.
#[derive(PartialEq, Eq)]
pub(crate) struct RtpPacket<'a> {
    pub(crate) marker: bool,
    pub(crate) payload_type: u8,
    pub(crate) sequence: u16,
    pub(crate) timestamp: u32,
    pub(crate) ssrc: u32,
    pub(crate) payload: &'a [u8],
}

impl fmt::Debug for RtpPacket<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtpPacket")
            .field("marker", &self.marker)
            .field("payload_type", &self.payload_type)
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("ssrc", &self.ssrc)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl<'a> RtpPacket<'a> {
    pub(crate) fn parse(datagram: &'a [u8]) -> Result<Self, PacketError> {
        if datagram.len() < RTP_HEADER_LEN {
            return Err(PacketError::TooShort);
        }

        let first = datagram[0];
        let version = first >> 6;
        if version != 2 {
            return Err(PacketError::UnsupportedVersion(version));
        }
        if first & 0x20 != 0 {
            return Err(PacketError::PaddingUnsupported);
        }
        if first & 0x10 != 0 {
            return Err(PacketError::ExtensionUnsupported);
        }
        let csrc_count = first & 0x0f;
        if csrc_count != 0 {
            return Err(PacketError::CsrcUnsupported(csrc_count));
        }

        let payload = &datagram[RTP_HEADER_LEN..];
        if payload.is_empty() {
            return Err(PacketError::EmptyPayload);
        }

        Ok(Self {
            marker: datagram[1] & 0x80 != 0,
            payload_type: datagram[1] & 0x7f,
            sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
            timestamp: u32::from_be_bytes(datagram[4..8].try_into().expect("fixed slice")),
            ssrc: u32::from_be_bytes(datagram[8..12].try_into().expect("fixed slice")),
            payload,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingDisposition {
    First,
    /// A new high-water mark. `by` is the distance from the previous maximum.
    Advanced {
        by: u64,
    },
    /// A previously unseen value behind the high-water mark.
    Reordered {
        behind: u64,
    },
    /// A value already seen within the bounded duplicate window.
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExtendedRingValue {
    pub(crate) value: u64,
    pub(crate) disposition: RingDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingError {
    /// The nearest interpretation belongs to the epoch before zero.
    BeforeStart,
    /// The logical counter has exhausted the `u64` representation.
    Exhausted,
    /// A preview no longer matches the state at commit time.
    StalePreview,
}

#[derive(Debug)]
struct RingExtender {
    modulus: u64,
    half_range: u64,
    highest: Option<u64>,
    recent: VecDeque<u64>,
}

impl RingExtender {
    fn new(bits: u32) -> Self {
        debug_assert!(matches!(bits, 16 | 32));
        let modulus = 1u64 << bits;
        Self {
            modulus,
            half_range: modulus / 2,
            highest: None,
            recent: VecDeque::with_capacity(RECENT_OBSERVATIONS),
        }
    }

    #[cfg(test)]
    fn highest(&self) -> Option<u64> {
        self.highest
    }

    fn peek(&self, raw: u64) -> Result<ExtendedRingValue, RingError> {
        let Some(highest) = self.highest else {
            // Reserve one epoch below the first observation. UDP may deliver
            // wire sequence 0 before the preceding 65535 packet; an unsigned
            // logical counter otherwise could not represent that predecessor.
            let value = self.modulus.checked_add(raw).ok_or(RingError::Exhausted)?;
            return Ok(ExtendedRingValue {
                value,
                disposition: RingDisposition::First,
            });
        };

        let epoch = highest / self.modulus;
        let mut candidate = (epoch as u128) * (self.modulus as u128) + raw as u128;
        let highest_wide = highest as u128;
        let half_wide = self.half_range as u128;
        let modulus_wide = self.modulus as u128;

        // Pick the epoch whose value is nearest to the current high-water mark.
        // At the exact half-range ambiguity we choose the future interpretation.
        if candidate + half_wide <= highest_wide {
            candidate += modulus_wide;
        } else if candidate > highest_wide + half_wide {
            if candidate < modulus_wide {
                return Err(RingError::BeforeStart);
            }
            candidate -= modulus_wide;
        }

        let candidate = u64::try_from(candidate).map_err(|_| RingError::Exhausted)?;
        let disposition = if candidate > highest {
            RingDisposition::Advanced {
                by: candidate - highest,
            }
        } else if self.recent.contains(&candidate) {
            RingDisposition::Duplicate
        } else {
            RingDisposition::Reordered {
                behind: highest - candidate,
            }
        };

        Ok(ExtendedRingValue {
            value: candidate,
            disposition,
        })
    }

    fn commit(
        &mut self,
        raw: u64,
        preview: ExtendedRingValue,
    ) -> Result<ExtendedRingValue, RingError> {
        let current = self.peek(raw)?;
        if current != preview {
            return Err(RingError::StalePreview);
        }

        match current.disposition {
            RingDisposition::First | RingDisposition::Advanced { .. } => {
                self.highest = Some(current.value);
                self.remember(current.value);
            }
            RingDisposition::Reordered { .. } => self.remember(current.value),
            RingDisposition::Duplicate => {}
        }
        Ok(current)
    }

    #[cfg(test)]
    fn observe(&mut self, raw: u64) -> Result<ExtendedRingValue, RingError> {
        let preview = self.peek(raw)?;
        self.commit(raw, preview)
    }

    fn remember(&mut self, value: u64) {
        if self.recent.len() == RECENT_OBSERVATIONS {
            self.recent.pop_front();
        }
        self.recent.push_back(value);
    }
}

/// Extends a wrapping RTP sequence number while preserving a monotonic
/// high-water mark.
#[derive(Debug)]
pub(crate) struct SequenceExtender16(RingExtender);

impl SequenceExtender16 {
    pub(crate) fn new() -> Self {
        Self(RingExtender::new(16))
    }

    /// Computes an extended value without changing duplicate history or the
    /// high-water mark. Commit it only after authentication and bounded jitter
    /// insertion have both accepted the packet.
    pub(crate) fn peek(&self, value: u16) -> Result<ExtendedRingValue, RingError> {
        self.0.peek(value as u64)
    }

    pub(crate) fn commit(
        &mut self,
        value: u16,
        preview: ExtendedRingValue,
    ) -> Result<ExtendedRingValue, RingError> {
        self.0.commit(value as u64, preview)
    }

    #[cfg(test)]
    pub(crate) fn observe(&mut self, value: u16) -> Result<ExtendedRingValue, RingError> {
        self.0.observe(value as u64)
    }

    #[cfg(test)]
    pub(crate) fn highest(&self) -> Option<u64> {
        self.0.highest()
    }
}

impl Default for SequenceExtender16 {
    fn default() -> Self {
        Self::new()
    }
}

/// Extends a wrapping RTP timestamp while preserving a monotonic high-water
/// mark.
#[derive(Debug)]
pub(crate) struct TimestampExtender32(RingExtender);

impl TimestampExtender32 {
    pub(crate) fn new() -> Self {
        Self(RingExtender::new(32))
    }

    /// Computes an extended value without changing the timestamp high-water
    /// mark. See [`SequenceExtender16::peek`] for the media admission order.
    pub(crate) fn peek(&self, value: u32) -> Result<ExtendedRingValue, RingError> {
        self.0.peek(value as u64)
    }

    pub(crate) fn commit(
        &mut self,
        value: u32,
        preview: ExtendedRingValue,
    ) -> Result<ExtendedRingValue, RingError> {
        self.0.commit(value as u64, preview)
    }

    #[cfg(test)]
    pub(crate) fn observe(&mut self, value: u32) -> Result<ExtendedRingValue, RingError> {
        self.0.observe(value as u64)
    }

    #[cfg(test)]
    pub(crate) fn highest(&self) -> Option<u64> {
        self.0.highest()
    }
}

impl Default for TimestampExtender32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u16, timestamp: u32, ssrc: u32) -> Vec<u8> {
        let mut bytes = vec![0x80, 0xe0];
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        bytes.extend_from_slice(&[0xaa, 0xbb]);
        bytes
    }

    #[test]
    fn parses_fixed_rtp_header_fields() {
        let bytes = packet(0x1234, 0x5678_9abc, 0xdef0_1234);
        let parsed = RtpPacket::parse(&bytes).unwrap();

        assert!(parsed.marker);
        assert_eq!(parsed.payload_type, 96);
        assert_eq!(parsed.sequence, 0x1234);
        assert_eq!(parsed.timestamp, 0x5678_9abc);
        assert_eq!(parsed.ssrc, 0xdef0_1234);
        assert_eq!(parsed.payload, [0xaa, 0xbb]);

        let debug = format!("{parsed:?}");
        assert!(debug.contains("payload_len: 2"));
        assert!(!debug.contains("170"));
        assert!(!debug.contains("187"));
    }

    #[test]
    fn rejects_non_fixed_rtp_headers() {
        assert_eq!(RtpPacket::parse(&[0u8; 11]), Err(PacketError::TooShort));

        let mut bytes = packet(1, 2, 3);
        bytes[0] = 0x40;
        assert_eq!(
            RtpPacket::parse(&bytes),
            Err(PacketError::UnsupportedVersion(1))
        );
        bytes[0] = 0xa0;
        assert_eq!(
            RtpPacket::parse(&bytes),
            Err(PacketError::PaddingUnsupported)
        );
        bytes[0] = 0x90;
        assert_eq!(
            RtpPacket::parse(&bytes),
            Err(PacketError::ExtensionUnsupported)
        );
        bytes[0] = 0x81;
        assert_eq!(
            RtpPacket::parse(&bytes),
            Err(PacketError::CsrcUnsupported(1))
        );
        bytes[0] = 0x80;
        bytes.truncate(RTP_HEADER_LEN);
        assert_eq!(RtpPacket::parse(&bytes), Err(PacketError::EmptyPayload));
    }

    #[test]
    fn sequence_extension_crosses_wrap_and_keeps_high_water_monotonic() {
        let mut extender = SequenceExtender16::new();
        assert_eq!(extender.observe(65_534).unwrap().value, 131_070);
        assert_eq!(extender.observe(65_535).unwrap().value, 131_071);
        let wrapped = extender.observe(0).unwrap();
        assert_eq!(wrapped.value, 131_072);
        assert_eq!(wrapped.disposition, RingDisposition::Advanced { by: 1 });
        assert_eq!(extender.highest(), Some(131_072));
    }

    #[test]
    fn sequence_extension_distinguishes_reordering_and_duplicates() {
        let mut extender = SequenceExtender16::new();
        extender.observe(65_534).unwrap();
        extender.observe(65_535).unwrap();
        extender.observe(0).unwrap();

        let old = extender.observe(65_533).unwrap();
        assert_eq!(old.value, 131_069);
        assert_eq!(old.disposition, RingDisposition::Reordered { behind: 3 });
        assert_eq!(extender.highest(), Some(131_072));
        assert_eq!(
            extender.observe(65_533).unwrap().disposition,
            RingDisposition::Duplicate
        );
        assert_eq!(
            extender.observe(0).unwrap().disposition,
            RingDisposition::Duplicate
        );
    }

    #[test]
    fn first_zero_reserves_an_epoch_for_reordered_65535() {
        let mut extender = SequenceExtender16::new();
        assert_eq!(extender.observe(0).unwrap().value, 65_536);
        let predecessor = extender.observe(u16::MAX).unwrap();
        assert_eq!(predecessor.value, 65_535);
        assert_eq!(
            predecessor.disposition,
            RingDisposition::Reordered { behind: 1 }
        );
        assert_eq!(extender.highest(), Some(65_536));
    }

    #[test]
    fn discarded_preview_does_not_advance_high_water_or_duplicate_history() {
        let mut extender = SequenceExtender16::new();
        extender.observe(100).unwrap();
        let rejected = extender.peek(500).unwrap();
        assert_eq!(rejected.disposition, RingDisposition::Advanced { by: 400 });
        assert_eq!(extender.highest(), Some(65_636));

        let accepted = extender.peek(101).unwrap();
        extender.commit(101, accepted).unwrap();
        assert_eq!(extender.highest(), Some(65_637));
        assert_eq!(
            extender.peek(500).unwrap().disposition,
            RingDisposition::Advanced { by: 399 }
        );
    }

    #[test]
    fn commit_rejects_a_preview_made_against_stale_state() {
        let mut extender = SequenceExtender16::new();
        let stale = extender.peek(101).unwrap();
        let first = extender.peek(100).unwrap();
        extender.commit(100, first).unwrap();
        assert_eq!(extender.commit(101, stale), Err(RingError::StalePreview));
        assert_eq!(extender.highest(), Some(65_636));
    }

    #[test]
    fn timestamp_extension_crosses_u32_wrap() {
        let mut extender = TimestampExtender32::new();
        assert_eq!(extender.observe(u32::MAX - 1).unwrap().value, 8_589_934_590);
        assert_eq!(extender.observe(u32::MAX).unwrap().value, 8_589_934_591);
        let wrapped = extender.observe(0).unwrap();
        assert_eq!(wrapped.value, 8_589_934_592);
        assert_eq!(wrapped.disposition, RingDisposition::Advanced { by: 1 });

        let reordered = extender.observe(u32::MAX - 2).unwrap();
        assert_eq!(reordered.value, 8_589_934_589);
        assert_eq!(
            reordered.disposition,
            RingDisposition::Reordered { behind: 3 }
        );
        assert_eq!(extender.highest(), Some(8_589_934_592));
    }

    #[test]
    fn first_timestamp_zero_reserves_an_epoch_for_reordered_maximum() {
        let mut extender = TimestampExtender32::new();
        assert_eq!(extender.observe(0).unwrap().value, 4_294_967_296);
        let predecessor = extender.observe(u32::MAX).unwrap();
        assert_eq!(predecessor.value, 4_294_967_295);
        assert_eq!(
            predecessor.disposition,
            RingDisposition::Reordered { behind: 1 }
        );
        assert_eq!(extender.highest(), Some(4_294_967_296));
    }

    #[test]
    fn exact_half_range_ambiguity_is_resolved_forward() {
        let mut sequences = SequenceExtender16::new();
        sequences.observe(0).unwrap();
        let sequence = sequences.observe(1 << 15).unwrap();
        assert_eq!(sequence.value, 65_536 + (1 << 15));
        assert_eq!(
            sequence.disposition,
            RingDisposition::Advanced { by: 1 << 15 }
        );

        let mut timestamps = TimestampExtender32::new();
        timestamps.observe(0).unwrap();
        let timestamp = timestamps.observe(1 << 31).unwrap();
        assert_eq!(timestamp.value, 4_294_967_296 + (1 << 31));
        assert_eq!(
            timestamp.disposition,
            RingDisposition::Advanced { by: 1 << 31 }
        );
    }
}
