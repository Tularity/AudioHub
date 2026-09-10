//! Device-independent band-limited clock matching for AudioHub audio paths.
//!
//! A fractional delay must not turn clock correction into a moving low-pass
//! filter. Coefficients are prepared at stream creation, shared by all lanes,
//! and linearly interpolated between 1024 phases. Processing retains history
//! and does not build coefficient tables or allocate internal scratch buffers.
//! The caller-owned output vector can grow if its initial capacity is too small.
//! Filter support scales with downsampling up to 8:1; larger ratios require
//! caller-side rejection or a separate staged conversion to retain the same
//! transition-band guarantee. Device-facing compatibility callers keep their
//! historical constructor contract, while AirPlay validates this bound.

const PHASES: usize = 1024;
const BASE_TAPS: usize = 128;
const MAX_TAPS: usize = 1024;

#[derive(Debug, Clone)]
pub struct ClockResampler {
    channels: usize,
    source_rate: u32,
    nominal: f64,
    step: f64,
    phase_index: usize,
    phase_fraction: f64,
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
            source_rate: src,
            nominal,
            step: nominal,
            phase_index: half - 1,
            phase_fraction: 0.0,
            half,
            unity_kernel: cutoff == 1.0,
            coefficients,
            history: vec![0.0; (taps - 1) * channels],
        }
    }

    pub fn channels(&self) -> u8 {
        self.channels as u8
    }

    /// Start a fixed-rate epoch without interpolating from invented silence.
    /// The previous endpoint value extends into the unavailable history; the
    /// FIR then blends in real samples causally. This preserves DC, not an
    /// assertion of phase-exact continuity across a changed bandwidth.
    pub fn new_seeded(src: u32, dst: u32, channels: u8, last: &[f32]) -> Self {
        let mut result = Self::new(src, dst, channels);
        result.reset_seeded(last);
        result
    }

    /// Reset stream history without rebuilding the immutable filter bank.
    /// The unavailable past extends the supplied frame; future real frames
    /// still pass through the declared fixed group delay.
    pub fn reset_seeded(&mut self, last: &[f32]) {
        assert_eq!(last.len(), self.channels);
        for frame in self.history.chunks_exact_mut(self.channels) {
            for (sample, value) in frame.iter_mut().zip(last) {
                *sample = if value.is_finite() { *value } else { 0.0 };
            }
        }
        self.phase_index = self.half - 1;
        self.phase_fraction = 0.0;
        self.step = self.nominal;
    }

    /// Fixed filter delay expressed in any caller's sample clock.
    pub fn delay_at_rate(&self, rate: u32) -> u32 {
        (self.half as u64 * u64::from(rate)).div_ceil(u64::from(self.source_rate)) as u32
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
        // A constant complete support is reconstructed exactly. This avoids
        // DC roundoff and unnecessary FIR work for silence and clock checks.
        // Bit comparison also preserves signed-zero unity semantics.
        let first = &input[..self.channels];
        let constant = self.history.chunks_exact(self.channels)
            .chain(input[..frames * self.channels].chunks_exact(self.channels))
            .all(|frame| frame.iter().zip(first).all(|(a, b)| a.to_bits() == b.to_bits()));
        let at = |frame: usize, channel: usize| {
            if frame < history_frames {
                self.history[frame * self.channels + channel]
            } else {
                input[(frame - history_frames) * self.channels + channel]
            }
        };
        let limit = frames + self.half - 1;
        let mut index = self.phase_index;
        let mut fraction = self.phase_fraction;
        out.reserve(((limit.saturating_sub(index) as f64 - fraction).max(0.0) / self.step).ceil() as usize * self.channels);
        while index < limit {
            if constant {
                out.extend_from_slice(first);
            } else if self.unity_kernel && fraction == 0.0 {
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
                    let start = index - (self.half - 1);
                    let first = at(start, channel);
                    // The support, not the caller's chunk, determines DC.
                    // A constant prefix followed by a transition must produce
                    // identical samples whether that transition is in this
                    // call or the next one.
                    if (1..taps).all(|tap| at(start + tap, channel).to_bits() == first.to_bits()) {
                        out.push(first);
                        continue;
                    }
                    let mut value = 0.0f64;
                    for tap in 0..taps {
                        let coefficient = a[tap] + (b[tap] - a[tap]) * mix;
                        value += f64::from(at(index + tap - (self.half - 1), channel))
                            * f64::from(coefficient);
                    }
                    out.push(value as f32);
                }
            }
            // Only integer source positions are rebased across input chunks.
            // Keep the fractional recurrence independent of block size, so
            // callers get identical FIR coefficients for identical streams.
            let next = fraction + self.step;
            let advance = next.floor() as usize;
            index += advance;
            fraction = next - advance as f64;
            // Rational audio ratios can land one rounding bit below an
            // integer (for example repeated 2/3). Do not turn that numerical
            // residue into an extra output frame at a 10 ms boundary.
            // This is in input-frame units, far below even f32 phase accuracy.
            if fraction < 1e-12 {
                fraction = 0.0;
            } else if 1.0 - fraction < 1e-12 {
                index += 1;
                fraction = 0.0;
            }
        }
        self.phase_index = index - frames;
        self.phase_fraction = fraction;
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

    #[test]
    fn reset_reuses_filter_storage_and_constant_waveforms_are_exact() {
        for rate in [16_000, 44_100, 48_000, 96_000] {
            let mut rs = ClockResampler::new(rate, 48_000, 2);
            let bank = rs.coefficients.as_ptr();
            let history = rs.history.as_ptr();
            let mut out = Vec::new();
            rs.process(&vec![0.9; 1000], &mut out);
            rs.reset_seeded(&[0.25, -0.5]);
            assert_eq!(rs.coefficients.as_ptr(), bank);
            assert_eq!(rs.history.as_ptr(), history);
            for correction in [0.998, 1.0, 1.002] {
                rs.set_correction(correction); out.clear();
                let samples: Vec<f32> = (0..500).flat_map(|_| [0.25, -0.5]).collect();
                rs.process(&samples, &mut out);
                assert!(out.chunks_exact(2).all(|frame| frame == [0.25, -0.5]));
            }
        }
    }

    #[test]
    fn constant_prefix_transition_is_sample_exact_across_every_chunk_boundary() {
        for rate in [44_100, 48_000] {
            let samples: Vec<f32> = (0..1000).flat_map(|n| {
                if n < 128 { [0.25, -0.5] } else { [(n as f32 * 0.17).sin(), (n as f32 * 0.31).cos()] }
            }).collect();
            let mut whole = ClockResampler::new_seeded(rate, 48_000, 2, &[0.25, -0.5]);
            let mut expected = Vec::new(); whole.process(&samples, &mut expected);
            for size in [1, 63, 64, 127, 128, 257] {
                let mut split = ClockResampler::new_seeded(rate, 48_000, 2, &[0.25, -0.5]);
                let mut actual = Vec::new();
                for chunk in samples.chunks(size * 2) { split.process(chunk, &mut actual); }
                assert_eq!(expected, actual, "rate={rate} chunk_frames={size}");
            }
        }
    }

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

    #[test]
    fn seeded_fixed_conversion_preserves_dc_and_reports_delay_in_each_clock() {
        for (src,dst) in [(48000,32000),(48000,24000),(48000,16000),(32000,48000)] {
            let mut converter=ClockResampler::new_seeded(src,dst,2,&[0.25,-0.5]);
            let source:Vec<f32>=(0..480).flat_map(|_|[0.25,-0.5]).collect();
            let mut output=Vec::new();converter.process(&source,&mut output);
            for frame in output.chunks_exact(2) {
                assert!((frame[0]-0.25).abs()<1e-6);
                assert!((frame[1]+0.5).abs()<1e-6);
            }
            assert_eq!(converter.delay_at_rate(src),converter.delay_frames() as u32);
            assert_eq!(converter.delay_at_rate(dst),converter.output_delay_frames());
            assert_eq!(converter.delay_at_rate(96000),converter.delay_at_rate(48000)*2);
        }
    }

    #[test]
    fn fixed_downsampling_rejects_content_above_the_new_nyquist() {
        let source=tone(20000.0,48000);
        for (dst,alias) in [(32000,12000.0),(24000,4000.0),(16000,4000.0)] {
            let mut converter=ClockResampler::new(48000,dst,1);
            let mut output=Vec::new();
            for chunk in source.chunks(480){converter.process(chunk,&mut output);}
            let middle=&output[(dst/4) as usize..(dst*3/4) as usize];
            let (mut real,mut imaginary)=(0.0,0.0);
            for (i,sample) in middle.iter().enumerate(){
                let phase=std::f64::consts::TAU*alias*i as f64/f64::from(dst);
                real+=f64::from(*sample)*phase.cos();imaginary+=f64::from(*sample)*phase.sin();
            }
            let amplitude=2.0*real.hypot(imaginary)/middle.len() as f64;
            assert!(amplitude < 1e-4,"destination={dst} alias_amplitude={amplitude}");
        }
    }

    #[test]
    fn fixed_wire_rates_keep_exact_ten_millisecond_frame_counts() {
        for channels in [1u8,2] {
            for rate in [32000u32,24000,16000] {
                let mut down=ClockResampler::new_seeded(48000,rate,channels,&vec![0.0;channels as usize]);
                let mut up=ClockResampler::new_seeded(rate,48000,channels,&vec![0.0;channels as usize]);
                for block in 0..100 {
                    let input:Vec<f32>=(0..480*usize::from(channels)).map(|i| ((i+block*480) as f32*0.017).sin()*0.2).collect();
                    let mut wire=Vec::new();down.process(&input,&mut wire);
                    assert_eq!(wire.len(),(rate/100) as usize*usize::from(channels));
                    let mut playback=Vec::new();up.process(&wire,&mut playback);
                    assert_eq!(playback.len(),480*usize::from(channels));
                    assert!(playback.iter().all(|v|v.is_finite()));
                }
            }
        }
    }
}
