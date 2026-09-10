//! Fixed-duration wire-rate conversion, independent of the device clock servo.
//!
//! Both directions retain a 4 ms content delay at every rate, including 48 kHz.
//! A downshift prefilters one packet before changing the wire sampling grid;
//! an upshift withholds the extra bandwidth until the old history has passed.
//! This preserves the content timeline instead of seeding a new FIR with DC.

use std::f64::consts::PI;
use std::sync::OnceLock;

pub const DELAY_FRAMES: usize = 192;
const FRAMES: usize = 480;
const TAPS: usize = 384;
const RATES: [u32; 4] = [16_000, 24_000, 32_000, 48_000];

fn rate_index(rate: u32) -> usize {
    RATES.iter().position(|r| *r == rate).expect("unsupported PCM wire rate")
}

fn blackman(distance: f64) -> f64 {
    0.42 + 0.5 * (PI * distance / 192.0).cos()
        + 0.08 * (2.0 * PI * distance / 192.0).cos()
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

struct Row {
    start: usize,
    weights: Box<[f32]>,
}

impl Row {
    fn sample(&self, data: &[f32], channels: usize, lane: usize) -> f32 {
        if self.weights.len() == 1 && self.weights[0] == 1.0 {
            return data[self.start * channels + lane];
        }
        self.weights.iter().enumerate().map(|(i, w)| {
            *w as f64 * data[(self.start + i) * channels + lane] as f64
        }).sum::<f64>() as f32
    }
}

struct Bank {
    // TX: cutoff index, destination rate index, fractional phase (0 or 1/2).
    tx: Vec<Vec<[Box<[f32]>; 2]>>,
    rx: Vec<Vec<Vec<Row>>>,
}

fn word(bytes: &mut &[u8]) -> usize {
    let (head, tail) = bytes.split_at(4);
    *bytes = tail;
    u32::from_le_bytes(head.try_into().unwrap()) as usize
}

fn uniform_row(before: u32, after: u32, output: usize) -> Row {
    let target = output as i32 * 2 - 384;
    let before_count = before as usize / 100;
    let count = before_count + after as usize / 100;
    let time = |i: usize| -> i32 {
        if i < before_count {
            (i as i32 - before_count as i32) * (96_000 / before) as i32
        } else {
            (i - before_count) as i32 * (96_000 / after) as i32
        }
    };
    if let Some(i) = (0..count).find(|i| time(*i) == target) {
        return Row { start: i, weights: vec![1.0].into_boxed_slice() };
    }
    let start = (0..count).find(|i| time(*i) >= target - 384).unwrap();
    let end = (start..count).find(|i| time(*i) > target + 384).unwrap_or(count);
    let rate = if time(end - 1) < 0 { before } else { after };
    let mut weights: Vec<f64> = (start..end).map(|i| {
        let distance = (time(i) - target) as f64;
        sinc(rate as f64 * distance / 96_000.0) * blackman(distance / 2.0)
    }).collect();
    let sum: f64 = weights.iter().sum();
    weights.iter_mut().for_each(|v| *v /= sum);
    Row { start, weights: weights.into_iter().map(|w| w as f32).collect() }
}

fn bank() -> &'static Bank {
    static BANK: OnceLock<Bank> = OnceLock::new();
    BANK.get_or_init(|| {
        let tx = RATES.iter().map(|cutoff| RATES.iter().map(|_| {
            std::array::from_fn(|phase| {
                let fraction = phase as f64 * 0.5;
                let weights: Vec<f64> = (0..TAPS).map(|i| {
                    let d = i as f64 - 191.0 - fraction;
                    sinc(*cutoff as f64 / 48_000.0 * d) * blackman(d)
                }).collect();
                let sum: f64 = weights.iter().sum();
                weights.into_iter().map(|w| (w / sum) as f32).collect()
            })
        }).collect()).collect();
        let mut rx: Vec<Vec<Vec<Row>>> = RATES.iter().map(|before| RATES.iter().map(|after| {
            (0..FRAMES).map(|i| uniform_row(*before, *after, i)).collect()
        }).collect()).collect();
        // Generated offline from a windowed 20 ms finite Fourier basis. There
        // is no matrix factorization or coefficient allocation during playback.
        let mut bytes = &include_bytes!("../assets/rate-boundary-v1.bin")[..];
        assert_eq!(&bytes[..8], b"AHRT0001");
        bytes = &bytes[8..];
        assert_eq!(word(&mut bytes), DELAY_FRAMES);
        assert_eq!(word(&mut bytes), 12);
        let mut seen = [[false; 4]; 4];
        for _ in 0..12 {
            let before = word(&mut bytes) as u32;
            let after = word(&mut bytes) as u32;
            let a = rate_index(before);
            let b = rate_index(after);
            assert!(a != b && !seen[a][b]);
            seen[a][b] = true;
            let rows = word(&mut bytes);
            assert_eq!(rows, 384);
            for i in 0..rows {
                let start = word(&mut bytes);
                let count = word(&mut bytes);
                assert!(count > 0 && start + count <= (before + after) as usize / 100);
                let weights: Box<[f32]> = (0..count).map(|_| f32::from_bits(word(&mut bytes) as u32)).collect();
                assert!(weights.iter().all(|w| w.is_finite()));
                assert!((weights.iter().map(|w| *w as f64).sum::<f64>() - 1.0).abs() < 2e-6);
                assert!(weights.iter().map(|w| (*w as f64).powi(2)).sum::<f64>() < 4.2 * 4.2);
                rx[a][b][i] = Row { start, weights };
            }
        }
        assert!(bytes.is_empty());
        Bank { tx, rx }
    })
}

/// Precompute shared kernels outside an audio deadline thread.
pub fn prepare() { let _ = bank(); }

#[derive(Clone, Copy)]
enum Ramp { None, Down, Up }

pub struct FixedRateTx {
    channels: usize,
    rate: u32,
    cutoff: u32,
    old_cutoff: u32,
    pending: Option<u32>,
    ramp: Ramp,
    history: Vec<f32>,
    data: Vec<f32>,
}

impl FixedRateTx {
    pub fn new(channels: u8, rate: u32) -> Self {
        assert!((1..=2).contains(&channels));
        rate_index(rate);
        prepare();
        let channels = channels as usize;
        Self { channels, rate, cutoff: rate, old_cutoff: rate, pending: None,
            ramp: Ramp::None, history: vec![0.0; (TAPS - 1) * channels],
            data: Vec::with_capacity((TAPS - 1 + FRAMES) * channels) }
    }

    /// Call exactly once for the next 480-frame input block. The returned
    /// rate, not `wanted`, determines the media header and packet splitting.
    pub fn select_rate(&mut self, wanted: u32) -> u32 {
        rate_index(wanted);
        self.old_cutoff = self.cutoff;
        self.ramp = Ramp::None;
        if self.pending.is_some_and(|p| p != wanted) { self.pending = None; }
        if self.pending == Some(wanted) {
            self.rate = wanted;
            self.pending = None;
        } else if wanted < self.rate {
            self.pending = Some(wanted);
            self.ramp = Ramp::Down;
        } else if wanted > self.rate {
            self.rate = wanted;
            self.ramp = Ramp::Up;
        } else if self.old_cutoff != wanted {
            self.ramp = Ramp::Up;
        }
        self.cutoff = wanted;
        self.rate
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        assert_eq!(input.len(), FRAMES * self.channels);
        self.data.clear();
        self.data.extend_from_slice(&self.history);
        self.data.extend_from_slice(input);
        out.clear();
        let count = self.rate as usize / 100;
        out.reserve(count * self.channels);
        for i in 0..count {
            let numerator = i * 48_000;
            let index = numerator / self.rate as usize;
            let phase = (numerator % self.rate as usize) * 2 / self.rate as usize;
            let sample = |cutoff: u32, lane: usize| {
                if cutoff == 48_000 && self.rate == 48_000 {
                    self.data[(index + 191) * self.channels + lane]
                } else {
                    bank().tx[rate_index(cutoff)][rate_index(self.rate)][phase].iter().enumerate()
                        .map(|(j, w)| *w as f64 * self.data[(index + j) * self.channels + lane] as f64)
                        .sum::<f64>() as f32
                }
            };
            let position_ms = i as f64 * 1000.0 / self.rate as f64;
            let blend = match self.ramp {
                Ramp::None => 1.0,
                Ramp::Down => 0.5 - 0.5 * (PI * (position_ms / 2.0).clamp(0.0, 1.0)).cos(),
                Ramp::Up => 0.5 - 0.5 * (PI * ((position_ms - 8.0) / 2.0).clamp(0.0, 1.0)).cos(),
            } as f32;
            for lane in 0..self.channels {
                let new = sample(self.cutoff, lane);
                out.push(if blend == 1.0 { new } else {
                    sample(self.old_cutoff, lane) * (1.0 - blend) + new * blend
                });
            }
        }
        self.history.copy_from_slice(&input[(FRAMES - (TAPS - 1)) * self.channels..]);
    }
}

pub struct FixedRateRx {
    channels: usize,
    rate: u32,
    history: Vec<f32>,
    data: Vec<f32>,
    discontinuous: bool,
}

impl FixedRateRx {
    pub fn new(channels: u8, rate: u32) -> Self {
        assert!((1..=2).contains(&channels));
        rate_index(rate);
        prepare();
        let channels = channels as usize;
        let mut history = Vec::with_capacity(FRAMES * channels);
        history.resize(rate as usize / 100 * channels, 0.0);
        Self { channels, rate, history, data: Vec::with_capacity(FRAMES * 2 * channels), discontinuous: true }
    }

    /// Forget waveform history after a discontinuity, retaining the time grid.
    pub fn reset(&mut self) { self.discontinuous = true; }

    pub fn process(&mut self, input: &[f32], rate: u32, out: &mut Vec<f32>) {
        let current = rate_index(rate);
        assert_eq!(input.len(), rate as usize / 100 * self.channels);
        if self.discontinuous {
            // Missing or concealed history is not a bandlimited transition.
            // Restart on a uniform grid, not the higher-norm mixed-grid bank.
            self.rate = rate;
            self.history.resize(input.len(), 0.0);
            self.history.fill(0.0);
            self.discontinuous = false;
        }
        self.data.clear();
        self.data.extend_from_slice(&self.history);
        self.data.extend_from_slice(input);
        out.clear();
        out.reserve(FRAMES * self.channels);
        for row in &bank().rx[rate_index(self.rate)][current] {
            for lane in 0..self.channels { out.push(row.sample(&self.data, self.channels, lane)); }
        }
        self.history.clear();
        self.history.extend_from_slice(input);
        self.rate = rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_rate_preserves_samples_with_explicit_fixed_delay() {
        let mut tx = FixedRateTx::new(2, 48_000);
        let mut rx = FixedRateRx::new(2, 48_000);
        let source: Vec<f32> = (0..FRAMES * 8 * 2).map(|i| ((i * 73 % 2003) as f32 - 1001.0) / 1024.0).collect();
        let mut wire = Vec::new();
        let mut pcm = Vec::new();
        let mut actual = Vec::new();
        for frame in source.chunks_exact(FRAMES * 2) {
            tx.select_rate(48_000);
            tx.process(frame, &mut wire);
            rx.process(&wire, 48_000, &mut pcm);
            actual.extend_from_slice(&pcm);
        }
        let delay = 2 * DELAY_FRAMES * 2;
        assert!(actual[..delay].iter().all(|v| *v == 0.0));
        assert_eq!(actual[delay..], source[..source.len() - delay]);
    }

    #[test]
    fn downshift_prefilters_and_cancel_does_not_commit_stale_rate() {
        let mut tx = FixedRateTx::new(1, 48_000);
        let mut out = Vec::new();
        for (wanted, actual) in [(16_000, 48_000), (48_000, 48_000), (32_000, 48_000), (32_000, 32_000)] {
            assert_eq!(tx.select_rate(wanted), actual);
            tx.process(&vec![0.1; FRAMES], &mut out);
            assert_eq!(out.len(), actual as usize / 100);
        }
    }

    #[test]
    fn every_rate_transition_preserves_passband_time_and_channels() {
        for before in RATES { for after in RATES {
            let mut tx = FixedRateTx::new(2, before);
            let mut rx = FixedRateRx::new(2, before);
            let mut wire = Vec::new();
            let mut out = Vec::new();
            for block in 0..12 {
                let source: Vec<f32> = (0..FRAMES).flat_map(|i| {
                    let t = (block * FRAMES + i) as f64 / 48_000.0;
                    [0.15 * (2.0 * PI * 6007.0 * t).sin() as f32,
                     0.15 * (2.0 * PI * 7001.0 * t).sin() as f32]
                }).collect();
                let rate = tx.select_rate(if block < 6 { before } else { after });
                tx.process(&source, &mut wire);
                for v in &mut wire { *v = (*v * 32768.0).round() / 32768.0; }
                rx.process(&wire, rate, &mut out);
                assert_eq!(out.len(), FRAMES * 2);
                if block >= 3 { for (i, pair) in out.chunks_exact(2).enumerate() {
                    let t = (block * FRAMES + i - 2 * DELAY_FRAMES) as f64 / 48_000.0;
                    for (lane, frequency) in [6007.0, 7001.0].iter().enumerate() {
                        let expected = 0.15 * (2.0 * PI * frequency * t).sin() as f32;
                        assert!((pair[lane] - expected).abs() < 0.00012,
                            "{before}->{after} block={block} sample={i} lane={lane}: {} vs {expected}", pair[lane]);
                    }
                }}
            }
        }}
    }
}
