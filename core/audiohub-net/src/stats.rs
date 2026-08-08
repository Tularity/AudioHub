use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A sliding-window byte-rate meter.
///
/// # Why this type exists at all: a lifetime average has no inverse
///
/// `RxSummary::bitrate_kbps` used to be `total_bytes / time_since_first_packet`.
/// On a session that has been up for 40 minutes, a bit-depth change moves the
/// next 20 seconds of traffic — 0.8% of the denominator — so the reported
/// number does not budge. Measured on a real mac -> win session, the three
/// 48 kHz rungs (whose true payload rates are 768 / 1152 / 1536 kbps) all read
/// back as 1469.2 / 1464.3 / 1467.0 kbps: a 0.34% spread that looks like noise
/// and is in fact the metric being blind to a 2x change on the wire.
///
/// Trying to recover the instantaneous rate algebraically from several lifetime
/// samples does not work either: a least-squares fit over three readings
/// recovered 840 B/datagram where the truth was 1016 B (-17%). The average is
/// not merely imprecise, it is not invertible.
///
/// So the rate has to be measured over a bounded, recent window. This type owns
/// nothing but `(Instant, cumulative_bytes)` points and is **non-consuming**:
/// reading it never mutates history, so the 1 Hz ticker and any number of IPC
/// readers can share one window (the same discipline `quality::ConcealWindow`
/// arrived at, and for the same reason).
pub struct RateWindow {
    pts: VecDeque<(Instant, u64)>,
    window: Duration,
    min_span: Duration,
}

impl RateWindow {
    /// `window` = how far back the rate is measured. `min_span` = the shortest
    /// span that still yields a number; below it [`RateWindow::kbps`] reports
    /// `None` rather than a figure computed from a denominator near zero.
    pub fn new(window: Duration, min_span: Duration) -> RateWindow {
        RateWindow { pts: VecDeque::new(), window, min_span }
    }

    /// Append a reading of the cumulative byte counter and drop points that
    /// have aged out.
    pub fn sample(&mut self, now: Instant, total_bytes: u64) {
        self.pts.push_back((now, total_bytes));
        // `Instant` is "since boot" on both platforms, so within the first
        // `window` after boot this subtraction genuinely has no answer. When it
        // has none, do not trim — a `unwrap_or(now)` here would set the cutoff
        // to "now", collapse the window to a single point, and make every
        // subsequent read return `None` until the window refilled.
        let Some(cutoff) = now.checked_sub(self.window) else { return };
        // Keep the newest point at or before the cutoff as the baseline;
        // deleting strictly by cutoff empties the window under sparse sampling.
        while self.pts.len() >= 2 && self.pts[1].0 <= cutoff {
            self.pts.pop_front();
        }
    }

    /// Bytes/second over the window, in kbps. `None` = not enough span yet.
    ///
    /// `None` is a distinct answer from `0.0`: a stream that just opened has no
    /// measured rate, whereas `0.0` asserts that nothing is flowing.
    pub fn kbps(&self) -> Option<f64> {
        if self.pts.len() < 2 {
            return None;
        }
        let (t0, b0) = self.pts[0];
        let (t1, b1) = self.pts[self.pts.len() - 1];
        let span = t1.duration_since(t0).as_secs_f64();
        if t1.duration_since(t0) < self.min_span {
            return None;
        }
        // `saturating_sub`: the counter is reset when a stream is rebuilt, and
        // a naive subtraction would underflow into an astronomical rate.
        Some(b1.saturating_sub(b0) as f64 * 8.0 / span / 1000.0)
    }

    /// Drop all history. Call this when the underlying counter restarts;
    /// otherwise the first post-reset sample differences against a stale, much
    /// larger baseline and saturates to zero.
    pub fn reset(&mut self) {
        self.pts.clear();
    }
}

/// One-way delay **spread**: the p95 of `transit` minus the minimum `transit`
/// over the same rolling window.
///
/// # Why this exists, and why `jitter_ms` cannot do its job on a stream
///
/// `RxStats` reports RFC 3550 jitter — an EWMA of `|transit − prev_transit|`,
/// a *first difference*. `media.rs`'s `update_target` already carries a note
/// admitting the wanted quantity is something else: the dispersion relative to
/// the earliest arrival, at a high quantile, which is what WebRTC's NetEq calls
/// the relative delay histogram.
///
/// On UDP the difference is a matter of accuracy. On a TCP transport it is the
/// difference between a signal and a constant: TCP's failure shape is "stall,
/// then deliver **in a burst**". Inside the burst consecutive differences are
/// ≈0 and only the first packet after the stall carries the whole delay, so a
/// 256-sample p95 of first differences **systematically under-reads** exactly
/// when the link is worst. The same shape burned this repo once already, in
/// `engine::split_timestamp_us` ("两个半包共用时间戳把一半抖动样本压成 0").
///
/// The subtraction is what makes the number usable across machines: `transit`
/// is `arrival − timestamp_us` across two unsynchronised clocks, so it contains
/// an arbitrary constant offset. p95 **minus the window minimum** cancels it
/// exactly, which is why the minimum is part of the definition rather than a
/// baseline someone has to calibrate.
///
/// Non-consuming, like [`RateWindow`] and `quality::ConcealWindow`: reading it
/// never mutates history, so the 1 Hz ticker and any number of IPC readers can
/// share one window.
pub struct SpreadWindow {
    /// Raw `transit` samples in arrival order, newest at the back.
    samples: VecDeque<i64>,
    cap: usize,
}

/// Window length. 256 is the same window `engine.rs` keeps for the jitter p95
/// that drives the jitter buffer target, and at 100 packets/s it is ~2.5 s —
/// long enough to contain a TCP retransmission's whole stall-and-burst, short
/// enough that a recovered link stops being punished for it within seconds.
pub const SPREAD_WINDOW: usize = 256;

/// Below this many samples [`SpreadWindow::spread_ms`] answers `None`.
///
/// A p95 taken over five points is the maximum of five points wearing a
/// quantile's name. `None` is a different claim from `0.0` and the callers
/// treat it as one.
pub const SPREAD_MIN_SAMPLES: usize = 32;

impl SpreadWindow {
    pub fn new() -> SpreadWindow {
        SpreadWindow { samples: VecDeque::with_capacity(SPREAD_WINDOW), cap: SPREAD_WINDOW }
    }

    /// `transit_us` = `arrival_us − timestamp_us`, the same quantity
    /// `RxStats::on_packet` differences for jitter. Feed **every** packet.
    pub fn push(&mut self, transit_us: i64) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(transit_us);
    }

    /// p95 − min over the window, in milliseconds. `None` = not enough samples.
    ///
    /// Never negative by construction (the p95 is at or above the minimum), so
    /// a negative reading would mean the window was mutated concurrently — it
    /// cannot be, this takes `&self`.
    pub fn spread_ms(&self) -> Option<f64> {
        if self.samples.len() < SPREAD_MIN_SAMPLES {
            return None;
        }
        let mut v: Vec<i64> = self.samples.iter().copied().collect();
        v.sort_unstable();
        let min = v[0];
        // Nearest-rank p95, clamped to the last index: with n = 256 this is
        // v[243]. Integer arithmetic on purpose — a float index would put the
        // boundary sample on the wrong side of the rank on some window lengths
        // and nothing would ever notice.
        let idx = ((v.len() * 95) / 100).min(v.len() - 1);
        Some((v[idx] - min) as f64 / 1000.0)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Drop all history. Call this when the stream is rebuilt: `transit`
    /// carries a per-connection clock offset, and mixing two of them produces a
    /// spread that is the offset difference rather than anything about delay.
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

impl Default for SpreadWindow {
    fn default() -> Self {
        SpreadWindow::new()
    }
}

pub struct RxStats {
    received: u64,
    bytes: u64,
    datagram_bytes: u64,
    min_seq: Option<u32>,
    max_seq: Option<u32>,
    reordered: u64,
    jitter_us: f64,
    jitter_max_us: f64,
    prev_transit: Option<i64>, // arrival_us - timestamp_us
}

impl RxStats {
    pub fn new() -> Self {
        RxStats {
            received: 0,
            bytes: 0,
            datagram_bytes: 0,
            min_seq: None,
            max_seq: None,
            reordered: 0,
            jitter_us: 0.0,
            jitter_max_us: 0.0,
            prev_transit: None,
        }
    }

    /// `payload_bytes` = decrypted plaintext length. `datagram_bytes` = what
    /// actually arrived on the socket (payload + header + AEAD tag).
    ///
    /// **They are counted separately on purpose.** The send side used to report
    /// the datagram figure under the same name the receive side used for the
    /// payload figure, so one stream read 1525 kbps on one machine and 1458 on
    /// the other — the 56 B/packet of framing — and no display could notice.
    pub fn on_packet(
        &mut self,
        seq: u32,
        timestamp_us: u64,
        arrival_us: u64,
        payload_bytes: usize,
        datagram_bytes: usize,
    ) {
        self.received += 1;
        self.bytes += payload_bytes as u64;
        self.datagram_bytes += datagram_bytes as u64;
        if let Some(max) = self.max_seq {
            if seq < max {
                self.reordered += 1;
            }
        }
        self.min_seq = Some(self.min_seq.map_or(seq, |m| m.min(seq)));
        self.max_seq = Some(self.max_seq.map_or(seq, |m| m.max(seq)));

        let transit = arrival_us as i64 - timestamp_us as i64;
        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).abs() as f64;
            self.jitter_us += (d - self.jitter_us) / 16.0;
            if self.jitter_us > self.jitter_max_us {
                self.jitter_max_us = self.jitter_us;
            }
        }
        self.prev_transit = Some(transit);
    }

    pub fn summary(&self, duration_s: f64) -> RxSummary {
        let expected = match (self.min_seq, self.max_seq) {
            (Some(min), Some(max)) => (max - min) as u64 + 1,
            _ => 0,
        };
        let lost = expected.saturating_sub(self.received);
        let loss_pct = if expected > 0 {
            lost as f64 * 100.0 / expected as f64
        } else {
            0.0
        };
        let mean_payload_kbps = if duration_s > 0.0 {
            self.bytes as f64 * 8.0 / duration_s / 1000.0
        } else {
            0.0
        };
        RxSummary {
            received: self.received,
            expected,
            lost,
            loss_pct,
            reordered: self.reordered,
            jitter_ms: self.jitter_us / 1000.0,
            jitter_max_ms: self.jitter_max_us / 1000.0,
            bytes: self.bytes,
            datagram_bytes: self.datagram_bytes,
            mean_payload_kbps,
        }
    }
}

impl Default for RxStats {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RxSummary {
    pub received: u64,
    pub expected: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub reordered: u64,
    pub jitter_ms: f64,
    pub jitter_max_ms: f64,
    /// Decrypted plaintext payload, cumulative.
    pub bytes: u64,
    /// Whole datagrams as they arrived (payload + header + AEAD tag), cumulative.
    pub datagram_bytes: u64,
    /// Payload bytes divided by the `duration_s` handed to [`RxStats::summary`].
    ///
    /// (see [`SpreadWindow`] for the statistic this one cannot replace)
    ///
    /// **This is a mean, and it is only meaningful when the caller owns the
    /// duration** — i.e. a probe that captures one fixed configuration over a
    /// bounded window. On a long-lived session it is worse than useless: see
    /// the [`RateWindow`] doc comment for the measurements. Live sessions must
    /// read a [`RateWindow`] instead, and the name says `mean_` so that the two
    /// can never again be confused at a call site.
    pub mean_payload_kbps: f64,
}

#[cfg(test)]
mod spread_tests {
    use super::*;

    /// The clock offset between two machines cancels out.
    ///
    /// `transit` is `arrival − timestamp_us` across two unsynchronised clocks,
    /// so it carries an arbitrary constant. If that constant survived into the
    /// answer, this statistic would be a property of the two machines' boot
    /// times, and the threshold it feeds would have to be calibrated per pair.
    #[test]
    fn the_clock_offset_between_two_machines_cancels() {
        let pattern: Vec<i64> = (0..SPREAD_WINDOW as i64).map(|i| (i % 7) * 1_000).collect();
        let mut a = SpreadWindow::new();
        let mut b = SpreadWindow::new();
        for t in &pattern {
            a.push(*t);
            b.push(*t + 3_600_000_000); // one hour of offset
        }
        assert_eq!(a.spread_ms(), b.spread_ms(), "the spread moved with the clock offset");
    }

    /// **The point of the whole statistic.** A stall-then-burst — TCP's failure
    /// shape — is invisible to RFC 3550 jitter and plain in the spread.
    ///
    /// Both windows see the same 256 packets: 240 arriving on time, then a
    /// 300 ms stall, then 16 delivered back to back (the retransmission
    /// completes and the receiver drains the kernel buffer at once). The EWMA
    /// of first differences sees one big step and fifteen ~zero ones, and its
    /// time constant of 16 dilutes even that; the spread sees the 300 ms
    /// directly, because its reference is the window's own minimum rather than
    /// the previous packet.
    #[test]
    fn a_stall_then_burst_is_visible_to_spread_and_nearly_invisible_to_rfc3550_jitter() {
        let mut spread = SpreadWindow::new();
        let mut rtp = RxStats::new();
        let mut seq = 0u32;
        // 240 on-time packets, 10 ms apart, ~0 transit.
        for i in 0..240u64 {
            let ts = i * 10_000;
            spread.push(0);
            rtp.on_packet(seq, ts, ts, 960, 1016);
            seq += 1;
        }
        // 16 packets that should have arrived over the next 160 ms all land at
        // once, 300 ms after the first of them was sent.
        let burst_at = 240 * 10_000 + 300_000;
        for i in 0..16u64 {
            let ts = (240 + i) * 10_000;
            spread.push(burst_at as i64 - ts as i64);
            rtp.on_packet(seq, ts, burst_at, 960, 1016);
            seq += 1;
        }
        let sm = rtp.summary(2.56);
        let sp = spread.spread_ms().expect("window is full");
        assert!(
            sm.jitter_ms < 60.0,
            "the RFC 3550 figure was supposed to under-read this stall; it read {:.1} ms, so \
             this test is no longer demonstrating the gap it exists to demonstrate",
            sm.jitter_ms
        );
        assert!(
            sp > 130.0,
            "the spread missed a 300 ms stall entirely (read {sp:.1} ms); it is the only signal \
             AUTO has on a transport where loss_pct is identically zero"
        );
        assert!(
            sp > sm.jitter_ms * 2.0,
            "spread {sp:.1} ms is not meaningfully larger than jitter {:.1} ms",
            sm.jitter_ms
        );
    }

    /// Too few samples answers `None`, not `0.0`.
    ///
    /// A p95 over a handful of points is the maximum of a handful of points
    /// wearing a quantile's name, and `0.0` would assert a measured absence of
    /// spread — which is exactly the "0 冒充未知" this telemetry forbids.
    #[test]
    fn a_short_window_has_no_answer_rather_than_a_confident_zero() {
        let mut w = SpreadWindow::new();
        for i in 0..(SPREAD_MIN_SAMPLES - 1) {
            w.push(i as i64 * 1_000);
            assert_eq!(w.spread_ms(), None, "answered with only {} samples", i + 1);
        }
        w.push(0);
        assert!(w.spread_ms().is_some(), "still no answer at the minimum sample count");
    }

    /// The window is bounded and forgets: a spike must not sit in the answer
    /// forever, or a link that recovered stays punished (the lifetime-average
    /// disease [`RateWindow`] was written to cure, one statistic over).
    #[test]
    fn an_old_spike_ages_out_of_the_window() {
        let mut w = SpreadWindow::new();
        w.push(500_000); // one 500 ms outlier
        for _ in 0..(SPREAD_WINDOW - 1) {
            w.push(0);
        }
        assert!(w.spread_ms().unwrap() < 1.0, "the p95 is holding a single outlier");
        assert_eq!(w.len(), SPREAD_WINDOW);
        for _ in 0..SPREAD_WINDOW {
            w.push(0);
        }
        assert_eq!(w.len(), SPREAD_WINDOW, "the window grew past its bound");
        assert_eq!(w.spread_ms(), Some(0.0));
    }

    /// Reading never mutates: the 1 Hz ticker and an IPC reader share one
    /// window, and a consuming read would hand the measurement to whichever
    /// one asked first.
    #[test]
    fn reading_the_window_is_not_a_consuming_operation() {
        let mut w = SpreadWindow::new();
        for i in 0..SPREAD_WINDOW {
            w.push((i as i64 % 11) * 1_000);
        }
        let first = w.spread_ms();
        assert_eq!(first, w.spread_ms());
        assert_eq!(first, w.spread_ms());
        assert_eq!(w.len(), SPREAD_WINDOW);
    }
}
