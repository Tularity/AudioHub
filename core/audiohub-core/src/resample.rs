//! Band-limited clock matching shared by capture, ordinary and spatial output.
//!
//! A fractional delay must not turn clock correction into a moving low-pass
//! filter. Coefficients are prepared at stream creation, shared by all lanes,
//! and linearly interpolated between 1024 phases. Processing retains history
//! and does not build coefficient tables or allocate internal scratch buffers.
//! The caller-owned output vector can grow if its initial capacity is too small.

const PHASES: usize = 1024;
const BASE_TAPS: usize = 128;
const MAX_TAPS: usize = 1024;

pub struct ClockResampler {
    channels: usize,
    nominal: f64,
    step: f64,
    phase: f64,
    half: usize,
    unity_kernel: bool,
    coefficients: Vec<f32>,
    history: Vec<f32>,
}

impl ClockResampler {
    pub fn new(src: u32, dst: u32, channels: u8) -> Self {
        assert!(src > 0 && dst > 0, "resampling rates must be nonzero");
        let channels = channels.clamp(1, 12) as usize;
        let nominal = f64::from(src) / f64::from(dst);
        let taps = (BASE_TAPS * nominal.ceil().max(1.0) as usize).min(MAX_TAPS);
        let half = taps / 2;
        let cutoff = 1.0 / nominal.max(1.0);
        let mut coefficients = vec![0.0; (PHASES + 1) * taps];
        for (phase, row) in coefficients.chunks_exact_mut(taps).enumerate() {
            let fraction = phase as f64 / PHASES as f64;
            let mut sum = 0.0;
            for (tap, coefficient) in row.iter_mut().enumerate() {
                let distance = tap as f64 - (half - 1) as f64 - fraction;
                let x = std::f64::consts::PI * distance;
                let sinc = if x.abs() < 1e-12 {
                    cutoff
                } else {
                    (cutoff * x).sin() / x
                };
                let window =
                    0.42 + 0.5 * (x / half as f64).cos() + 0.08 * (2.0 * x / half as f64).cos();
                let value = sinc * window;
                *coefficient = value as f32;
                sum += value;
            }
            for coefficient in row {
                *coefficient /= sum as f32;
            }
        }
        Self {
            channels,
            nominal,
            step: nominal,
            phase: (half - 1) as f64,
            half,
            unity_kernel: cutoff == 1.0,
            coefficients,
            history: vec![0.0; (taps - 1) * channels],
        }
    }

    pub fn channels(&self) -> u8 {
        self.channels as u8
    }

    /// Input frames consumed per output frame. Corrections multiply this
    /// ratio; output's inverse convention is adapted by its existing wrapper.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// A clock servo, not a playback-speed control. Invalid updates preserve
    /// the last valid ratio and can never create a zero-progress output loop.
    pub fn set_correction(&mut self, correction: f64) {
        if correction.is_finite() && (0.99..=1.01).contains(&correction) {
            self.step = self.nominal * correction;
        }
    }

    /// Fixed group delay in input frames (64 at equal nominal sample rates).
    pub fn delay_frames(&self) -> usize {
        self.half
    }

    /// Nominal group delay in destination-rate frames, rounded up for latency
    /// accounting. The servo's tiny deviation does not change this fixed term.
    pub fn output_delay_frames(&self) -> u32 {
        (self.half as f64 / self.nominal).ceil() as u32
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / self.channels;
        if frames == 0 {
            return;
        }
        let taps = self.half * 2;
        let history_frames = taps - 1;
        let at = |frame: usize, channel: usize| {
            if frame < history_frames {
                self.history[frame * self.channels + channel]
            } else {
                input[(frame - history_frames) * self.channels + channel]
            }
        };
        let limit = (frames + self.half - 1) as f64;
        let mut p = self.phase;
        out.reserve(((limit - p).max(0.0) / self.step).ceil() as usize * self.channels);
        while p < limit {
            let index = p.floor() as usize;
            let fraction = p - index as f64;
            if self.unity_kernel && fraction == 0.0 {
                // An integer fractional delay is exact, even after a previous
                // correction. This is not a nominal-rate passthrough shortcut.
                for channel in 0..self.channels {
                    out.push(at(index, channel));
                }
            } else {
                let phase = fraction * PHASES as f64;
                let row = (phase.floor() as usize).min(PHASES - 1);
                let mix = (phase - row as f64) as f32;
                let a = &self.coefficients[row * taps..(row + 1) * taps];
                let b = &self.coefficients[(row + 1) * taps..(row + 2) * taps];
                for channel in 0..self.channels {
                    let mut value = 0.0f64;
                    for tap in 0..taps {
                        let coefficient = a[tap] + (b[tap] - a[tap]) * mix;
                        value += f64::from(at(index + tap - (self.half - 1), channel))
                            * f64::from(coefficient);
                    }
                    out.push(value as f32);
                }
            }
            p += self.step;
        }
        self.phase = p - frames as f64;
        let keep = frames.min(history_frames) * self.channels;
        if frames < history_frames {
            self.history.copy_within(keep.., 0);
        }
        let end = frames * self.channels;
        let offset = self.history.len() - keep;
        self.history[offset..].copy_from_slice(&input[end - keep..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| (n as f64 * hz * std::f64::consts::TAU / 48000.0).sin() as f32)
            .collect()
    }

    #[test]
    fn clock_correction_keeps_the_audio_band_flat() {
        for corr in [0.9995, 1.0005] {
            for hz in [997.0, 5000.0, 10000.0, 15000.0, 20000.0] {
                let input = tone(hz, 48000);
                let mut rs = ClockResampler::new(48000, 48000, 1);
                rs.set_correction(corr);
                let mut output = Vec::new();
                for block in input.chunks(480) {
                    rs.process(block, &mut output);
                }
                let x = &output[4800..43200];
                let power = x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64;
                let db = 10.0 * (2.0 * power).log10();
                assert!(
                    db.abs() < 0.01,
                    "frequency={hz} correction={corr} gain_db={db}"
                );
            }
        }
    }

    #[test]
    fn unity_is_sample_exact_after_the_declared_delay() {
        let input = tone(1499.0, 4096);
        let mut rs = ClockResampler::new(48000, 48000, 1);
        let mut output = Vec::new();
        rs.process(&input, &mut output);
        assert_eq!(input.len(), output.len());
        assert_eq!(
            input[..input.len() - rs.delay_frames()],
            output[rs.delay_frames()..]
        );
    }

    #[test]
    fn tiny_blocks_and_rate_changes_preserve_history() {
        let input = tone(17000.0, 8192);
        for (src, dst) in [
            (48000, 48000),
            (48000, 44100),
            (44100, 48000),
            (96000, 48000),
        ] {
            let (mut a, mut b) = (
                ClockResampler::new(src, dst, 1),
                ClockResampler::new(src, dst, 1),
            );
            let (mut x, mut y) = (Vec::new(), Vec::new());
            for (i, block) in input.chunks(2048).enumerate() {
                let correction = if i % 2 == 0 { 0.9995 } else { 1.0005 };
                a.set_correction(correction);
                b.set_correction(correction);
                a.process(block, &mut x);
                for sample in block.chunks(1) {
                    b.process(sample, &mut y);
                }
            }
            assert_eq!(x.len(), y.len());
            assert!(x.iter().zip(y).all(|(a, b)| (a - b).abs() < 1e-5));
        }
    }

    #[test]
    fn spatial_lanes_do_not_bleed_or_shift_relative_to_each_other() {
        let source = tone(11000.0, 4096);
        let mut input = vec![0.0; source.len() * 12];
        for (i, sample) in source.iter().enumerate() {
            input[i * 12] = *sample;
            input[i * 12 + 11] = -*sample;
        }
        let mut rs = ClockResampler::new(48000, 48000, 12);
        rs.set_correction(1.0002);
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(rs.channels(), 12);
        for frame in out.chunks_exact(12) {
            assert_eq!(frame[0], -frame[11]);
            assert!(frame[1..11].iter().all(|v| *v == 0.0));
        }
    }

    #[test]
    fn invalid_corrections_do_not_poison_state() {
        let mut rs = ClockResampler::new(48000, 48000, 1);
        rs.set_correction(1.0005);
        let step = rs.step();
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e-300, 1e300] {
            rs.set_correction(bad);
            assert_eq!(rs.step(), step);
        }
        let mut out = Vec::new();
        rs.process(&[0.25; 480], &mut out);
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
