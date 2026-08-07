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
    /// **This is a mean, and it is only meaningful when the caller owns the
    /// duration** — i.e. a probe that captures one fixed configuration over a
    /// bounded window. On a long-lived session it is worse than useless: see
    /// the [`RateWindow`] doc comment for the measurements. Live sessions must
    /// read a [`RateWindow`] instead, and the name says `mean_` so that the two
    /// can never again be confused at a call site.
    pub mean_payload_kbps: f64,
}
