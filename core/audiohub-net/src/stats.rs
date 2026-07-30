pub struct RxStats {
    received: u64,
    bytes: u64,
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
            min_seq: None,
            max_seq: None,
            reordered: 0,
            jitter_us: 0.0,
            jitter_max_us: 0.0,
            prev_transit: None,
        }
    }

    pub fn on_packet(&mut self, seq: u32, timestamp_us: u64, arrival_us: u64, payload_bytes: usize) {
        self.received += 1;
        self.bytes += payload_bytes as u64;
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
        let bitrate_kbps = if duration_s > 0.0 {
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
            bitrate_kbps,
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
    pub bytes: u64,
    pub bitrate_kbps: f64,
}
