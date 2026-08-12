use crate::AUDIOHUB_RATE;
use std::io;

/// Stateful interleaved-i16 to 48 kHz mono-f32 converter.
///
/// Chunk boundaries do not reset channel grouping or interpolation phase. The
/// correction input adjusts only the resampling ratio for clock recovery; it
/// is bounded by the caller and is unrelated to AirPlay volume. No volume gain
/// is accepted or applied anywhere in this type.
#[derive(Debug, Clone)]
pub struct StreamingPcmConverter {
    input_rate: u32,
    channels: u8,
    partial_sum: f32,
    partial_channels: u8,
    source_index: u64,
    next_output_position: f64,
    previous: Option<f32>,
    correction_ppm: f64,
}

impl StreamingPcmConverter {
    /// Construct a converter for the format negotiated by the AirPlay engine.
    pub fn new(input_rate: u32, channels: u8) -> io::Result<Self> {
        if input_rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay stream reported a zero sample rate",
            ));
        }
        if channels == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay stream reported zero channels",
            ));
        }
        Ok(Self {
            input_rate,
            channels,
            partial_sum: 0.0,
            partial_channels: 0,
            source_index: 0,
            next_output_position: 0.0,
            previous: None,
            correction_ppm: 0.0,
        })
    }

    /// Update the receiver-clock correction. The runtime supplies a bounded
    /// value from PCM bus water level; direct callers should use a similarly
    /// small bound (hundreds, not thousands, of ppm).
    pub fn set_correction_ppm(&mut self, correction_ppm: f64) {
        self.correction_ppm = if correction_ppm.is_finite() {
            correction_ppm.clamp(-2_000.0, 2_000.0)
        } else {
            0.0
        };
    }

    /// Convert a chunk, appending 48 kHz mono samples to `out`.
    pub fn process_i16(&mut self, input: &[i16], out: &mut Vec<f32>) {
        for &sample in input {
            self.partial_sum += sample as f32 / 32_768.0;
            self.partial_channels += 1;
            if self.partial_channels == self.channels {
                let mono = self.partial_sum / self.channels as f32;
                self.partial_sum = 0.0;
                self.partial_channels = 0;
                self.push_mono(mono, out);
            }
        }
    }

    /// Drop partial input/interpolation state after a sender seek or flush.
    pub fn flush(&mut self) {
        self.partial_sum = 0.0;
        self.partial_channels = 0;
        self.source_index = 0;
        self.next_output_position = 0.0;
        self.previous = None;
    }

    fn step(&self) -> f64 {
        let effective_output_rate = AUDIOHUB_RATE as f64 * (1.0 + self.correction_ppm * 1e-6);
        self.input_rate as f64 / effective_output_rate.max(1.0)
    }

    fn push_mono(&mut self, sample: f32, out: &mut Vec<f32>) {
        let current_position = self.source_index as f64;
        self.source_index = self.source_index.saturating_add(1);

        let Some(previous) = self.previous.replace(sample) else {
            // Preserve the first real sample. Interpolating it against an
            // invented zero would create a small click/fade at every FLUSH.
            out.push(sample);
            self.next_output_position = self.step();
            return;
        };

        let previous_position = current_position - 1.0;
        while self.next_output_position <= current_position {
            let fraction = (self.next_output_position - previous_position) as f32;
            out.push(previous + (sample - previous) * fraction.clamp(0.0, 1.0));
            self.next_output_position += self.step();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BusConfig, PcmBus, AUDIOHUB_FRAME_SAMPLES};

    fn stereo(left: i16, right: i16, frames: usize) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            pcm.extend_from_slice(&[left, right]);
        }
        pcm
    }

    #[test]
    fn downmixes_without_applying_airplay_volume() {
        let mut converter = StreamingPcmConverter::new(48_000, 2).unwrap();
        let mut out = Vec::new();
        converter.process_i16(&stereo(16_384, 0, 8), &mut out);
        assert_eq!(out.len(), 8);
        for value in out {
            assert!((value - 0.25).abs() < 1e-6);
        }
    }

    #[test]
    fn resamples_44100_stereo_to_48000_mono_streaming() {
        let frames = 44_101;
        let pcm = stereo(8_192, 8_192, frames);
        let mut converter = StreamingPcmConverter::new(44_100, 2).unwrap();
        let mut out = Vec::new();
        for chunk in pcm.chunks(733) {
            converter.process_i16(chunk, &mut out);
        }
        // The extra source endpoint lets interpolation produce the full first
        // second. Streaming output differs by at most one sample at a cut.
        assert!((out.len() as isize - 48_001).abs() <= 1, "{}", out.len());
        assert!(out.iter().all(|v| (*v - 0.25).abs() < 1e-6));
    }

    #[test]
    fn resampling_preserves_near_full_scale_amplitude() {
        let sample = (0.9 * i16::MAX as f32).round() as i16;
        let pcm = stereo(sample, sample, 44_101);
        let mut converter = StreamingPcmConverter::new(44_100, 2).unwrap();
        let mut out = Vec::new();
        for chunk in pcm.chunks(733) {
            converter.process_i16(chunk, &mut out);
        }
        let expected = sample as f32 / 32_768.0;
        assert!((out.len() as isize - 48_001).abs() <= 1, "{}", out.len());
        assert!(out.iter().all(|value| (*value - expected).abs() < 1e-6));
        assert!(expected > 0.89);
    }

    #[test]
    fn arbitrary_chunk_splits_preserve_channel_and_resampler_state() {
        let pcm: Vec<i16> = (0..20_003)
            .map(|n| ((n * 97) as i16).wrapping_sub(12_345))
            .collect();
        let mut whole = StreamingPcmConverter::new(44_100, 2).unwrap();
        let mut whole_out = Vec::new();
        whole.process_i16(&pcm, &mut whole_out);

        let mut split = StreamingPcmConverter::new(44_100, 2).unwrap();
        let mut split_out = Vec::new();
        let mut offset = 0;
        for size in [1, 7, 2, 511, 3, 1_024, 5, 89].into_iter().cycle() {
            if offset == pcm.len() {
                break;
            }
            let end = (offset + size).min(pcm.len());
            split.process_i16(&pcm[offset..end], &mut split_out);
            offset = end;
        }
        assert_eq!(whole_out, split_out);
    }

    fn simulate_clock_error(source_ppm: f64) -> (u64, f64) {
        let config = BusConfig {
            capacity_samples: 9_600,
            prebuffer_samples: 2_400,
            max_correction_ppm: 500,
        };
        let bus = PcmBus::new(config);
        let mut reader = bus.subscribe();
        bus.activate(1);
        let mut converter = StreamingPcmConverter::new(44_100, 2).unwrap();
        let mut source_fraction = 0.0f64;
        let mut pcm = Vec::new();
        let mut converted = Vec::new();
        let mut frame = [0.0f32; AUDIOHUB_FRAME_SAMPLES];

        // Ten simulated minutes, advanced in AudioHub's real 10 ms quanta.
        // No wall-clock sleeps are involved.
        for _ in 0..60_000 {
            source_fraction += 441.0 * (1.0 + source_ppm * 1e-6);
            let source_frames = source_fraction.floor() as usize;
            source_fraction -= source_frames as f64;
            pcm.clear();
            pcm.resize(source_frames * 2, 0);
            converted.clear();
            converter.set_correction_ppm(bus.correction_ppm());
            converter.process_i16(&pcm, &mut converted);
            bus.push(1, &converted);
            reader.read_into(&mut frame);
        }
        (bus.unread_for(reader.id()), bus.correction_ppm())
    }

    #[test]
    fn fifo_servo_bounds_plus_and_minus_200ppm_for_ten_minutes() {
        let (fast_fill, fast_correction) = simulate_clock_error(200.0);
        let (slow_fill, slow_correction) = simulate_clock_error(-200.0);

        // Fixed-ratio drift would be 5_760 samples in ten minutes. The servo
        // holds both cases around a small bounded waterline and converges in
        // the direction that cancels the source clock error.
        assert!((500..4_000).contains(&fast_fill), "fast fill={fast_fill}");
        assert!((500..4_000).contains(&slow_fill), "slow fill={slow_fill}");
        assert!(fast_correction < -150.0, "fast corr={fast_correction}");
        assert!(slow_correction > 150.0, "slow corr={slow_correction}");
    }
}
