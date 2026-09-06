//! Clock primitives for AirPlay 2 buffered audio.
//!
//! This module contains the exact timestamp arithmetic shared by the
//! authenticated rate-anchor parser and the passive PTP mapper. Socket and
//! session lifecycles stay in their owning modules.

use std::error::Error;
use std::fmt;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Convert a seconds plus unsigned 64-bit binary fraction timestamp to exact
/// integer nanoseconds. The intermediate multiplication is deliberately
/// `u128`, avoiding floating-point precision loss and `u64` overflow.
pub(crate) fn network_time_nanos(seconds: u64, fraction: u64) -> Result<u64, NetworkTimeError> {
    let fractional_ns = (u128::from(fraction) * NANOS_PER_SECOND) >> 64;
    let total = u128::from(seconds)
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|whole| whole.checked_add(fractional_ns))
        .ok_or(NetworkTimeError::Overflow)?;
    u64::try_from(total).map_err(|_| NetworkTimeError::Overflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkTimeError {
    Overflow,
}

impl fmt::Display for NetworkTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("network time does not fit in a u64 nanosecond timeline")
    }
}

impl Error for NetworkTimeError {}

/// Return the shortest signed distance between two wrapping RTP timestamps.
/// Exactly half the sequence space has no unique direction and is rejected.
pub(crate) fn rtp_delta(timestamp: u32, anchor: u32) -> Result<i64, RtpDeltaError> {
    let raw = timestamp.wrapping_sub(anchor);
    if raw == 0x8000_0000 {
        return Err(RtpDeltaError::AmbiguousHalfRange);
    }
    Ok(i64::from(raw as i32))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtpDeltaError {
    AmbiguousHalfRange,
}

impl fmt::Display for RtpDeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RTP timestamp is exactly half the wrapping range from its anchor")
    }
}

impl Error for RtpDeltaError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaybackAnchor {
    pub(crate) timeline_id: u64,
    pub(crate) remote_time_ns: u64,
    pub(crate) rtp_time: u32,
    /// Telemetry generation in which the authenticated control anchor was
    /// accepted. Scheduling must bind this to the frame and mapper tokens.
    pub(crate) telemetry_token: crate::telemetry::OperationToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_binary_fraction_to_nanoseconds_exactly_and_checks_overflow() {
        assert_eq!(network_time_nanos(0, 0), Ok(0));
        assert_eq!(
            network_time_nanos(0, 0x8000_0000_0000_0000),
            Ok(500_000_000)
        );
        assert_eq!(network_time_nanos(0, u64::MAX), Ok(999_999_999));
        assert_eq!(
            network_time_nanos(1_323_152, 6_275_326_383_463_858_176),
            Ok(1_323_152_340_186_124)
        );
        assert_eq!(
            network_time_nanos(u64::MAX, 0),
            Err(NetworkTimeError::Overflow)
        );
    }

    #[test]
    fn signed_rtp_delta_handles_both_wrap_directions() {
        assert_eq!(rtp_delta(0x0000_03f0, 0xffff_fff0), Ok(1_024));
        assert_eq!(rtp_delta(0xffff_fff0, 0x0000_03f0), Ok(-1_024));
        assert_eq!(rtp_delta(100, 100), Ok(0));
        assert_eq!(
            rtp_delta(0x8000_0000, 0),
            Err(RtpDeltaError::AmbiguousHalfRange)
        );
    }
}
