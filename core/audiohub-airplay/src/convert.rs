use crate::AUDIOHUB_RATE;
use audiohub_dsp::ClockResampler;
use std::io;
use std::time::Duration;

/// Stateful interleaved-i16 to native 48 kHz stereo-frame converter.
///
/// Chunk boundaries do not reset channel grouping or interpolation phase. Mono
/// input is duplicated to left and right. Stereo input keeps independent lane
/// history while sharing one interpolation phase. A channel count above two is
/// rejected because AirPlay supplies no channel layout with which to map it.
#[derive(Debug, Clone)]
pub struct StreamingStereoPcmConverter {
    core: StreamingFrameConverter,
}

impl StreamingStereoPcmConverter {
    pub fn new(input_rate: u32, channels: u8) -> io::Result<Self> {
        if channels > 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay stereo conversion requires a channel layout above two channels",
            ));
        }
        Ok(Self {
            core: StreamingFrameConverter::new(input_rate, channels)?,
        })
    }

    /// Update receiver-clock correction without changing volume or lane balance.
    pub fn set_correction_ppm(&mut self, correction_ppm: f64) {
        self.core.set_correction_ppm(correction_ppm);
    }

    /// Append native 48 kHz stereo frames to `out`.
    pub fn process_i16(&mut self, input: &[i16], out: &mut Vec<[f32; 2]>) {
        self.core.process_i16(input, out);
    }

    /// Fixed content delay of the reconstruction filter, including at 48 kHz.
    /// A clock correction never switches between different content timelines.
    pub fn delay(&self) -> Duration {
        Duration::from_secs_f64(self.core.resampler.delay_frames() as f64 / self.core.input_rate as f64)
    }

    /// Drop partial input/interpolation state after a sender seek or flush.
    pub fn flush(&mut self) {
        self.core.flush();
    }
}

/// Compatibility adapter for the historical 48 kHz mono-f32 converter API.
///
/// New production paths should use [`StreamingStereoPcmConverter`]. This type
/// deliberately retains its mono output shape and legacy multichannel downmix.
#[derive(Debug, Clone)]
pub struct StreamingPcmConverter {
    core: StreamingFrameConverter,
    staged: Vec<[f32; 2]>,
}

impl StreamingPcmConverter {
    /// Construct the exported legacy mono converter.
    pub fn new(input_rate: u32, channels: u8) -> io::Result<Self> {
        Ok(Self {
            core: StreamingFrameConverter::new(input_rate, channels)?,
            staged: Vec::new(),
        })
    }

    pub fn set_correction_ppm(&mut self, correction_ppm: f64) {
        self.core.set_correction_ppm(correction_ppm);
    }

    /// Convert a chunk, appending explicit L/R downmix samples to `out`.
    pub fn process_i16(&mut self, input: &[i16], out: &mut Vec<f32>) {
        self.staged.clear();
        self.core.process_i16(input, &mut self.staged);
        out.extend(
            self.staged
                .iter()
                .map(|frame| (frame[0] + frame[1]) * 0.5),
        );
    }

    pub fn flush(&mut self) {
        self.core.flush();
        self.staged.clear();
    }
}

#[derive(Debug, Clone)]
struct StreamingFrameConverter {
    input_rate: u32,
    channels: u8,
    partial_frame: [f32; 2],
    partial_sum: f32,
    partial_channels: u8,
    resampler: ClockResampler,
    seed_pending: bool,
    input_frames: Vec<f32>,
    output_frames: Vec<f32>,
    correction_ppm: f64,
}

impl StreamingFrameConverter {
    fn new(input_rate: u32, channels: u8) -> io::Result<Self> {
        if input_rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay stream reported a zero sample rate",
            ));
        }
        if input_rate > AUDIOHUB_RATE * 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay PCM conversion exceeds the supported 8:1 downsampling ratio",
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
            partial_frame: [0.0; 2],
            partial_sum: 0.0,
            partial_channels: 0,
            resampler: ClockResampler::new(input_rate, AUDIOHUB_RATE, 2),
            seed_pending: true,
            input_frames: Vec::with_capacity(4096),
            output_frames: Vec::with_capacity(8192),
            correction_ppm: 0.0,
        })
    }

    fn set_correction_ppm(&mut self, correction_ppm: f64) {
        self.correction_ppm = if correction_ppm.is_finite() {
            correction_ppm.clamp(-2_000.0, 2_000.0)
        } else {
            0.0
        };
    }

    fn process_i16(&mut self, input: &[i16], out: &mut Vec<[f32; 2]>) {
        self.input_frames.clear();
        for &sample in input {
            let sample = sample as f32 / 32_768.0;
            match self.channels {
                1 => self.input_frames.extend_from_slice(&[sample, sample]),
                2 => {
                    self.partial_frame[self.partial_channels as usize] = sample;
                    self.partial_channels += 1;
                    if self.partial_channels == 2 {
                        let frame = self.partial_frame;
                        self.partial_channels = 0;
                        self.input_frames.extend_from_slice(&frame);
                    }
                }
                _ => {
                    self.partial_sum += sample;
                    self.partial_channels += 1;
                    if self.partial_channels == self.channels {
                        let mono = self.partial_sum / self.channels as f32;
                        self.partial_sum = 0.0;
                        self.partial_channels = 0;
                        self.input_frames.extend_from_slice(&[mono, mono]);
                    }
                }
            }
        }
        if self.input_frames.is_empty() { return; }
        if self.seed_pending {
            // A seek has no trustworthy earlier waveform. Extend its first
            // actual frame into the missing past rather than inventing a zero
            // ramp. This preserves DC; real transitions retain the FIR delay.
            self.resampler.reset_seeded(&self.input_frames[..2]);
            self.seed_pending = false;
        }
        // Positive feedback correction requests more output frames. The
        // common kernel takes input-frames/output-frame, hence the reciprocal.
        self.resampler.set_correction(1.0 / (1.0 + self.correction_ppm * 1e-6));
        self.output_frames.clear();
        self.resampler.process(&self.input_frames, &mut self.output_frames);
        out.extend(self.output_frames.chunks_exact(2).map(|frame| [frame[0], frame[1]]));
    }

    fn flush(&mut self) {
        self.partial_frame = [0.0; 2];
        self.partial_sum = 0.0;
        self.partial_channels = 0;
        self.input_frames.clear();
        self.output_frames.clear();
        self.seed_pending = true;
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

    fn assert_constant_stereo(rate: u32, left: i16, right: i16) {
        let source_frames = if rate == 48_000 { 64 } else { 44_101 };
        let mut converter = StreamingStereoPcmConverter::new(rate, 2).unwrap();
        let mut out = Vec::new();
        for chunk in stereo(left, right, source_frames).chunks(733) {
            converter.process_i16(chunk, &mut out);
        }
        let expected_left = left as f32 / 32_768.0;
        let expected_right = right as f32 / 32_768.0;
        assert!(out.iter().all(|frame| {
            (frame[0] - expected_left).abs() < 1e-6
                && (frame[1] - expected_right).abs() < 1e-6
        }));
        if rate == 48_000 {
            assert_eq!(out.len(), source_frames);
        } else {
            assert!((out.len() as isize - 48_001).abs() <= 1, "{}", out.len());
        }
    }

    #[test]
    fn stereo_independence_and_antiphase_survive_at_48k() {
        assert_constant_stereo(48_000, 16_384, 0);
        assert_constant_stereo(48_000, 0, 16_384);
        assert_constant_stereo(48_000, 16_384, -16_384);
    }

    #[test]
    fn stereo_independence_and_antiphase_survive_44100_to_48000() {
        assert_constant_stereo(44_100, 16_384, 0);
        assert_constant_stereo(44_100, 0, 16_384);
        assert_constant_stereo(44_100, 16_384, -16_384);
    }

    #[test]
    fn arbitrary_odd_chunk_splits_preserve_lane_and_resampler_state() {
        let pcm: Vec<i16> = (0..20_003)
            .map(|n| ((n * 97) as i16).wrapping_sub(12_345))
            .collect();
        let mut whole = StreamingStereoPcmConverter::new(44_100, 2).unwrap();
        let mut whole_out = Vec::new();
        whole.process_i16(&pcm, &mut whole_out);

        let mut split = StreamingStereoPcmConverter::new(44_100, 2).unwrap();
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

    #[test]
    fn mono_duplicates_to_both_lanes() {
        let mut converter = StreamingStereoPcmConverter::new(48_000, 1).unwrap();
        let mut out = Vec::new();
        converter.process_i16(&[16_384, -8_192], &mut out);
        let delay = converter.core.resampler.delay_frames();
        converter.process_i16(&vec![0; delay], &mut out);
        assert_eq!(&out[delay..delay + 2], &[[0.5, 0.5], [-0.25, -0.25]]);
        assert!(out.iter().all(|frame| frame[0] == frame[1]));
    }

    #[test]
    fn stereo_converter_rejects_multichannel_without_layout() {
        let error = StreamingStereoPcmConverter::new(48_000, 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn conversion_rejects_rates_beyond_the_filter_support() {
        assert!(StreamingStereoPcmConverter::new(AUDIOHUB_RATE * 8 + 1, 2).is_err());
        assert!(StreamingPcmConverter::new(u32::MAX, 1).is_err());
    }

    #[test]
    fn treble_and_sampling_images_are_not_changed_by_linear_interpolation() {
        let mut converter = StreamingStereoPcmConverter::new(44_100, 2).unwrap();
        let input: Vec<i16> = (0..44_100).flat_map(|n| {
            let sample = (0.2 * (std::f64::consts::TAU * 20_000.0 * n as f64 / 44_100.0).sin() * 32767.0).round() as i16;
            [sample, 0]
        }).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(2048) { converter.process_i16(chunk, &mut out); }
        let window = &out[4800..43200];
        let amplitude = |hz: f64| {
            let (real, imag) = window.iter().enumerate().fold((0.0, 0.0), |(r, i), (n, sample)| {
                let phase = std::f64::consts::TAU * hz * n as f64 / 48_000.0;
                (r + sample[0] as f64 * phase.cos(), i + sample[0] as f64 * phase.sin())
            });
            2.0 * real.hypot(imag) / window.len() as f64
        };
        assert!((20.0 * (amplitude(20_000.0) / 0.2).log10()).abs() < 0.01);
        assert!(amplitude(23_900.0) < 0.00002, "the input sampling image entered the output band");
        assert!(out.iter().all(|frame| frame[1] == 0.0));
    }

    #[test]
    fn flush_discards_partial_channels_and_history_but_reuses_the_filter() {
        let mut converter = StreamingStereoPcmConverter::new(44_100, 2).unwrap();
        let mut out = Vec::new();
        converter.process_i16(&stereo(24_000, -24_000, 500), &mut out);
        converter.process_i16(&[20_000], &mut out);
        converter.flush();
        out.clear();
        converter.process_i16(&stereo(8192, -16384, 500), &mut out);
        assert!(out.iter().all(|frame| *frame == [0.25, -0.5]));
        assert_eq!(converter.delay(), Duration::from_secs_f64(64.0 / 44_100.0));
        let mut clone = converter.clone();
        let next = stereo(3000, -4000, 300);
        let mut expected = Vec::new(); let mut actual = Vec::new();
        converter.process_i16(&next, &mut expected); clone.process_i16(&next, &mut actual);
        assert_eq!(expected, actual);
    }

    #[test]
    fn legacy_mono_converter_remains_an_explicit_downmix_adapter() {
        let mut converter = StreamingPcmConverter::new(48_000, 2).unwrap();
        let mut out = Vec::new();
        converter.process_i16(&stereo(16_384, 0, 8), &mut out);
        assert_eq!(out, vec![0.25; 8]);

        let mut multichannel = StreamingPcmConverter::new(48_000, 3).unwrap();
        let mut legacy_out = Vec::new();
        multichannel.process_i16(&[3_000, 6_000, 9_000], &mut legacy_out);
        assert_eq!(legacy_out, vec![6_000.0 / 32_768.0]);
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
        let mut converter = StreamingStereoPcmConverter::new(44_100, 2).unwrap();
        let mut source_fraction = 0.0f64;
        let mut pcm = Vec::new();
        let mut converted = Vec::new();
        let mut frame = [[0.0f32; 2]; AUDIOHUB_FRAME_SAMPLES];

        // Ten simulated minutes, advanced in AudioHub's real 10 ms quanta.
        for _ in 0..60_000 {
            source_fraction += 441.0 * (1.0 + source_ppm * 1e-6);
            let source_frames = source_fraction.floor() as usize;
            source_fraction -= source_frames as f64;
            pcm.clear();
            for _ in 0..source_frames {
                pcm.extend_from_slice(&[8_192, -16_384]);
            }
            converted.clear();
            converter.set_correction_ppm(bus.correction_ppm());
            converter.process_i16(&pcm, &mut converted);
            bus.push(1, &converted);
            let read = reader.read_stereo_into(&mut frame);
            assert!(frame[..read.copied]
                .iter()
                .all(|sample| *sample == [0.25, -0.5]));
        }
        (bus.unread_for(reader.id()), bus.correction_ppm())
    }

    #[test]
    fn fifo_servo_bounds_plus_and_minus_200ppm_without_lane_skew() {
        let (fast_fill, fast_correction) = simulate_clock_error(200.0);
        let (slow_fill, slow_correction) = simulate_clock_error(-200.0);

        // Fixed-ratio drift would be 5,760 frames in ten minutes. The servo
        // holds both cases around a small bounded frame waterline.
        assert!((500..4_000).contains(&fast_fill), "fast fill={fast_fill}");
        assert!((500..4_000).contains(&slow_fill), "slow fill={slow_fill}");
        assert!(fast_correction < -150.0, "fast corr={fast_correction}");
        assert!(slow_correction > 150.0, "slow corr={slow_correction}");
    }
}
