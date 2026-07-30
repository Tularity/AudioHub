pub fn gen_sine(freq_hz: f32, sample_rate: u32, num_samples: usize, amp: f32) -> Vec<f32> {
    // f64 phase: f32 accumulation audibly degrades SNR on multi-second tones
    let step = 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64;
    (0..num_samples)
        .map(|n| (amp as f64 * (step * n as f64).sin()) as f32)
        .collect()
}

pub fn goertzel_power(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len() as f32;
    let omega = 2.0 * std::f64::consts::PI * (freq_hz as f64) / (sample_rate as f64);
    let coeff = 2.0 * omega.cos();
    let (mut s_prev, mut s_prev2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = x as f64 + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let power = s_prev * s_prev + s_prev2 * s_prev2 - coeff * s_prev * s_prev2;
    // Normalize so pure tone of amp A yields ~A^2/4 regardless of window length.
    (power / (n as f64 * n as f64)) as f32
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToneVerdict {
    pub freq_hz: f32,
    pub snr_db: f32,
    pub detected: bool,
    pub samples_analyzed: usize,
}

pub fn verify_tone(samples: &[f32], sample_rate: u32, freq_hz: f32) -> ToneVerdict {
    let sr = sample_rate as usize;
    let skip = sr / 5; // 200ms
    let win = sr / 10; // 100ms
    if samples.len() < sr * 3 / 10 || win == 0 {
        return ToneVerdict {
            freq_hz,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    let usable = &samples[skip..];
    let mut snrs: Vec<f32> = Vec::new();
    let mut analyzed = 0usize;
    for chunk in usable.chunks(win) {
        if chunk.len() < win {
            break;
        }
        analyzed += chunk.len();
        let target = goertzel_power(chunk, sample_rate, freq_hz) as f64;
        // Total normalized power on same scale as goertzel_power: mean(x^2)/2.
        let total: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
            / (chunk.len() as f64)
            / 2.0;
        let eps = 1e-12f64;
        let noise = (total - target).max(0.0) + eps;
        snrs.push((10.0 * (target.max(eps) / noise).log10()) as f32);
    }
    if snrs.is_empty() {
        return ToneVerdict {
            freq_hz,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    snrs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let snr_db = snrs[snrs.len() / 2];
    ToneVerdict {
        freq_hz,
        snr_db,
        detected: snr_db > 20.0,
        samples_analyzed: analyzed,
    }
}

pub fn f32_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn s16le_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Stateful linear resampler: carries phase + last sample across calls, so
/// block-split processing equals whole-block processing (within tolerance).
pub struct LinearResampler {
    step: f64, // input samples per output sample
    phase: f64,
    last: f32,
    passthrough: bool,
}

impl LinearResampler {
    pub fn new(src: u32, dst: u32) -> Self {
        LinearResampler {
            step: src as f64 / dst.max(1) as f64,
            phase: 0.0,
            last: 0.0,
            passthrough: src == dst,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let len = input.len() as f64;
        let mut p = self.phase;
        // position p: 0.0 == previous chunk's last sample, 1.0 == input[0]
        while p < len {
            let i = p.floor() as usize;
            let frac = (p - i as f64) as f32;
            let s0 = if i == 0 { self.last } else { input[i - 1] };
            let s1 = input[i];
            out.push(s0 + (s1 - s0) * frac);
            p += self.step;
        }
        self.phase = p - len;
        self.last = *input.last().unwrap();
    }
}
