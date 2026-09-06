use crate::AUDIOHUB_RATE;
use std::io;

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
    source_index: u64,
    next_output_position: f64,
    previous: Option<[f32; 2]>,
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
            source_index: 0,
            next_output_position: 0.0,
            previous: None,
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
        for &sample in input {
            let sample = sample as f32 / 32_768.0;
            match self.channels {
                1 => self.push_frame([sample, sample], out),
                2 => {
                    self.partial_frame[self.partial_channels as usize] = sample;
                    self.partial_channels += 1;
                    if self.partial_channels == 2 {
                        let frame = self.partial_frame;
                        self.partial_channels = 0;
                        self.push_frame(frame, out);
                    }
                }
                _ => {
                    self.partial_sum += sample;
                    self.partial_channels += 1;
                    if self.partial_channels == self.channels {
                        let mono = self.partial_sum / self.channels as f32;
                        self.partial_sum = 0.0;
                        self.partial_channels = 0;
                        self.push_frame([mono, mono], out);
                    }
                }
            }
        }
    }

    fn flush(&mut self) {
        self.partial_frame = [0.0; 2];
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

    fn push_frame(&mut self, frame: [f32; 2], out: &mut Vec<[f32; 2]>) {
        let current_position = self.source_index as f64;
        self.source_index = self.source_index.saturating_add(1);

        let Some(previous) = self.previous.replace(frame) else {
            // Preserve the first real frame. Interpolating against invented
            // silence would create a small click/fade at every FLUSH.
            out.push(frame);
            self.next_output_position = self.step();
            return;
        };

        let previous_position = current_position - 1.0;
        while self.next_output_position <= current_position {
            let fraction = (self.next_output_position - previous_position) as f32;
            let fraction = fraction.clamp(0.0, 1.0);
            out.push([
                previous[0] + (frame[0] - previous[0]) * fraction,
                previous[1] + (frame[1] - previous[1]) * fraction,
            ]);
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
        assert_eq!(out, [[0.5, 0.5], [-0.25, -0.25]]);
    }

    #[test]
    fn stereo_converter_rejects_multichannel_without_layout() {
        let error = StreamingStereoPcmConverter::new(48_000, 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
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
