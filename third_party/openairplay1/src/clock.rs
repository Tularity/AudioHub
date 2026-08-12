//! Clock synchronisation model for latency-correct playback.
//!
//! Two independent measurements combine to tell us *when*, on our local
//! monotonic clock, a given audio frame should reach the DAC:
//!
//! 1. The **NTP timing exchange** gives the offset between the client's clock
//!    and ours. We send a request, the client stamps its receive/transmit
//!    times, and from the four timestamps we recover the round-trip time and
//!    the clock offset (keeping the estimate from the lowest-RTT exchange).
//! 2. The **sync packet** anchors an RTP timestamp to a client-clock instant.
//!
//! Together: `play_time(rtp)` = local instant that frame should play.

use std::sync::OnceLock;
use std::time::Instant;

/// Local monotonic time in nanoseconds since the first call, shared by every
/// thread. The timing exchange, sync anchor, and player all measure against
/// this one clock so `play_time` results are directly comparable to `now_ns`.
pub fn now_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Convert a 32.32 fixed-point NTP timestamp to nanoseconds.
pub fn ntp_to_ns(seconds: u32, fraction: u32) -> u64 {
    seconds as u64 * 1_000_000_000 + ((fraction as u64 * 1_000_000_000) >> 32)
}

/// Convert nanoseconds to a 32.32 NTP `(seconds, fraction)` pair. Inverse of
/// [`ntp_to_ns`] (used by tests / a synthetic client).
pub fn ns_to_ntp(ns: u64) -> (u32, u32) {
    let seconds = (ns / 1_000_000_000) as u32;
    let frac_ns = ns % 1_000_000_000;
    let fraction = ((frac_ns << 32) / 1_000_000_000) as u32;
    (seconds, fraction)
}

/// How many recent timing samples to keep for lowest-RTT selection.
const WINDOW: usize = 8;

#[derive(Clone, Copy)]
struct Sample {
    rtt: u64,
    /// remote_clock − local_clock, in nanoseconds.
    offset: i128,
}

pub struct ClockModel {
    sample_rate: u32,
    samples: Vec<Sample>,
    /// Best (lowest-RTT) offset, remote − local in ns.
    offset: Option<i128>,
    /// Sync anchor: (remote-clock instant in ns, RTP timestamp at the DAC then).
    anchor: Option<(u64, u32)>,
}

impl ClockModel {
    pub fn new(sample_rate: u32) -> ClockModel {
        ClockModel {
            sample_rate,
            samples: Vec::with_capacity(WINDOW),
            offset: None,
            anchor: None,
        }
    }

    /// Feed one completed timing exchange. `t1`/`t4` are our local send/arrival
    /// times (ns, monotonic); `t2`/`t3` are the client's receive/transmit times
    /// (ns, its clock). Ill-formed exchanges (negative durations) are ignored.
    pub fn add_timing(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) {
        let (Some(round_trip), Some(remote_processing)) = (t4.checked_sub(t1), t3.checked_sub(t2))
        else {
            return;
        };
        let Some(rtt) = round_trip.checked_sub(remote_processing) else {
            return;
        };
        // Estimate the remote clock at our arrival instant t4: the client's
        // transmit time plus half the network round-trip.
        let remote_at_t4 = t3 as i128 + (rtt / 2) as i128;
        let offset = remote_at_t4 - t4 as i128;

        if self.samples.len() == WINDOW {
            self.samples.remove(0);
        }
        self.samples.push(Sample { rtt, offset });
        // Lowest RTT is the least-dispersed, most trustworthy measurement.
        self.offset = self.samples.iter().min_by_key(|s| s.rtt).map(|s| s.offset);
    }

    /// Record a sync anchor: at client-clock instant `remote_time_ns`, the
    /// frame with RTP timestamp `rtp_at_dac` is at the DAC.
    pub fn set_anchor(&mut self, remote_time_ns: u64, rtp_at_dac: u32) {
        self.anchor = Some((remote_time_ns, rtp_at_dac));
    }

    /// Both an offset and an anchor are needed before playback can be timed.
    pub fn is_ready(&self) -> bool {
        self.offset.is_some() && self.anchor.is_some()
    }

    /// The local monotonic instant (ns) at which frame `rtp` should reach the
    /// DAC, or `None` until the model is ready.
    pub fn play_time(&self, rtp: u32) -> Option<u64> {
        let offset = self.offset?;
        let (remote_sync_ns, rtp_anchor) = self.anchor?;
        // Convert the anchor's client-clock instant to our local clock.
        let local_anchor = remote_sync_ns as i128 - offset;
        // Signed frame distance, handling u32 RTP wraparound.
        let delta_frames = rtp.wrapping_sub(rtp_anchor) as i32 as i128;
        let delta_ns = delta_frames * 1_000_000_000 / self.sample_rate as i128;
        let play = local_anchor + delta_ns;
        (play >= 0).then_some(play as u64)
    }

    /// Current best offset (remote − local, ns), for diagnostics.
    pub fn offset_ns(&self) -> Option<i128> {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_ns_round_trips() {
        for ns in [
            0u64,
            1,
            1_000_000_000,
            1_234_567_891,
            3_912_345_678_000_000_000,
        ] {
            let (s, f) = ns_to_ntp(ns);
            let back = ntp_to_ns(s, f);
            // Fixed-point fraction loses sub-nanosecond precision; within 1 ns.
            assert!(back.abs_diff(ns) <= 1, "ns={ns} back={back}");
        }
    }

    #[test]
    fn ntp_half_second_is_max_fraction() {
        assert_eq!(ntp_to_ns(0, 0x8000_0000), 500_000_000);
        assert_eq!(ntp_to_ns(5, 0), 5_000_000_000);
    }

    #[test]
    fn offset_from_symmetric_exchange() {
        // Remote clock is exactly 1_000_000_000 ns ahead of local. Symmetric
        // 10 ms each way, remote takes 2 ms to turn around.
        let offset = 1_000_000_000i128;
        let t1 = 100_000_000u64; // local send
        let t2 = (t1 as i128 + offset + 10_000_000) as u64; // remote receive
        let t3 = t2 + 2_000_000; // remote transmit
        let t4 = t1 + 10_000_000 + 2_000_000 + 10_000_000; // local arrival
        let mut m = ClockModel::new(44100);
        m.add_timing(t1, t2, t3, t4);
        // rtt = 22ms - 2ms = 20ms; remote_at_t4 = t3 + 10ms; offset ≈ 1e9.
        let est = m.offset_ns().unwrap();
        assert!((est - offset).abs() < 1_000, "offset estimate {est}");
    }

    #[test]
    fn picks_lowest_rtt_sample() {
        let mut m = ClockModel::new(44100);
        // A high-RTT (noisy) exchange with a skewed offset...
        m.add_timing(
            0,
            1_000_000_000 + 40_000_000,
            1_000_000_000 + 41_000_000,
            100_000_000,
        );
        let noisy = m.offset_ns().unwrap();
        // ...then a clean low-RTT exchange with offset ~1e9.
        m.add_timing(
            200_000_000,
            200_000_000 + 1_000_000_000 + 1_000_000,
            200_000_000 + 1_000_000_000 + 1_100_000,
            200_000_000 + 2_100_000,
        );
        let best = m.offset_ns().unwrap();
        assert_ne!(best, noisy);
        assert!((best - 1_000_000_000).abs() < 200_000, "best {best}");
    }

    #[test]
    fn rejects_malformed_exchange() {
        let mut m = ClockModel::new(44100);
        m.add_timing(100, 50, 40, 200); // t3 < t2
        assert!(m.offset_ns().is_none());
    }

    #[test]
    fn not_ready_without_both_offset_and_anchor() {
        let mut m = ClockModel::new(44100);
        assert!(!m.is_ready());
        m.add_timing(0, 1_000_000_000, 1_000_100_000, 200_000);
        assert!(!m.is_ready(), "offset alone is not enough");
        m.set_anchor(2_000_000_000, 1000);
        assert!(m.is_ready());
    }

    #[test]
    fn play_time_advances_with_rtp() {
        let mut m = ClockModel::new(44100);
        // Zero offset for simple arithmetic.
        m.add_timing(0, 1_000_000, 1_000_000, 2_000_000); // rtt tiny, offset≈ -1ms? keep simple
        m.set_anchor(10_000_000_000, 1000);
        let base = m.play_time(1000).unwrap();
        // One frame later = 1/44100 s ≈ 22676 ns later.
        let next = m.play_time(1001).unwrap();
        assert_eq!(next - base, 1_000_000_000 / 44100);
        // 44100 frames later = exactly one second later.
        let one_sec = m.play_time(1000 + 44100).unwrap();
        assert_eq!(one_sec - base, 1_000_000_000);
    }

    #[test]
    fn play_time_handles_rtp_wrap() {
        let mut m = ClockModel::new(44100);
        m.set_anchor(10_000_000_000, u32::MAX - 1); // anchor near wrap
        m.add_timing(0, 0, 0, 0); // zero offset
        let before = m.play_time(u32::MAX - 1).unwrap();
        // Two frames past the anchor wraps u32 to 0.
        let after = m.play_time(0).unwrap();
        // Single-expression division (as the implementation does it).
        assert_eq!(after - before, 2 * 1_000_000_000 / 44100);
    }

    #[test]
    fn play_time_none_until_ready() {
        let mut m = ClockModel::new(44100);
        assert_eq!(m.play_time(1000), None);
        m.set_anchor(1_000_000_000, 0);
        assert_eq!(m.play_time(1000), None, "still needs an offset");
    }
}
