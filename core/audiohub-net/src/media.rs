//! Media plane building blocks (spec-m4a §3): per-stream AEAD, jitter buffer
//! with PLC, frame sources, deterministic loss injection, AUTO quality ladder.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use audiohub_core::audio::{AudioRx, LiveCapture};
use audiohub_core::dsp::LinearResampler;
use audiohub_core::sysaudio::{self, BackendInfo, SysAudioCapture};

use crate::packet::{Header, HEADER_LEN};

pub const AEAD_TAG_LEN: usize = 16;

/// HKDF info prefix for per-stream media keys; the stream_id is appended LE.
const STREAM_KEY_INFO: &[u8] = b"audiohub-stream-v1";

/// Per-stream media AEAD. nonce = 4B stream_id LE ‖ 4B seq LE ‖ 4B zero,
/// AAD = the 40-byte wire header. `codec` in the header still names the
/// plaintext encoding; the wire payload is ciphertext.
pub struct MediaCrypto {
    cipher: ChaCha20Poly1305,
}

fn media_nonce(stream_id: u32, seq: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&stream_id.to_le_bytes());
    n[4..8].copy_from_slice(&seq.to_le_bytes());
    n
}

impl MediaCrypto {
    pub fn new(key: &[u8; 32]) -> Self {
        MediaCrypto {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// Per-stream key: HKDF-SHA256(ikm = media_key, salt, info =
    /// "audiohub-stream-v1" ‖ stream_id LE). Real streams MUST use this
    /// instead of `new`: the nonce is (peer-chosen stream_id ‖ seq), so under
    /// one connection-wide key a peer that closes and reopens the same
    /// stream_id restarts seq at 0 and repeats the exact keystream. Binding
    /// the key to the opener's fresh per-stream salt keeps the keystream
    /// distinct even when stream_id and seq both repeat.
    pub fn new_for_stream(media_key: &[u8; 32], stream_id: u32, salt: &[u8]) -> MediaCrypto {
        let mut info = Vec::with_capacity(STREAM_KEY_INFO.len() + 4);
        info.extend_from_slice(STREAM_KEY_INFO);
        info.extend_from_slice(&stream_id.to_le_bytes());
        let hk = Hkdf::<Sha256>::new(Some(salt), media_key);
        let mut stream_key = [0u8; 32];
        hk.expand(&info, &mut stream_key).expect("hkdf expand 32B");
        let mc = MediaCrypto {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&stream_key)),
        };
        stream_key.zeroize();
        mc
    }

    /// Builds the full datagram: 40B header (payload_len set to ciphertext
    /// length) followed by the ciphertext.
    pub fn seal(&self, header: &Header, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut h = header.clone();
        h.payload_len = (plaintext.len() + AEAD_TAG_LEN) as u32;
        let mut datagram = h.encode(&[]);
        let nonce = media_nonce(h.stream_id, h.seq);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: plaintext, aad: &datagram },
            )
            .map_err(|_| anyhow!("media encrypt failed"))?;
        datagram.extend_from_slice(&ct);
        Ok(datagram)
    }

    /// Parse + authenticate + decrypt one datagram into (header, plaintext).
    pub fn open(&self, datagram: &[u8]) -> Result<(Header, Vec<u8>)> {
        let (h, ct) = Header::parse(datagram).map_err(|e| anyhow!("bad media packet: {e}"))?;
        let nonce = media_nonce(h.stream_id, h.seq);
        let pt = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: ct, aad: &datagram[..HEADER_LEN] },
            )
            .map_err(|_| anyhow!("media decrypt failed (tampered or wrong key)"))?;
        Ok((h, pt))
    }
}

/// Per-receive-stream jitter buffer holding 48k mono f32 frames (one frame =
/// one 10ms tick). pop() once per tick after warm-up; missing frames get PLC
/// (repeat last frame decayed 30% per repeat), silence after 5 consecutive
/// misses. Depth beyond target+6 drops oldest frames to catch up on latency.
/// An underrun re-enters pre-buffering until depth is back at target, so the
/// buffer rebuilds its cushion instead of running at depth 0 forever.
pub struct JitterBuffer {
    frames: BTreeMap<u32, Vec<f32>>,
    next_seq: Option<u32>,
    /// Holding output to rebuild depth: true before the first frame ever plays
    /// and again after every underrun.
    prebuffering: bool,
    target: u32,
    frame_len: usize,
    last_frame: Vec<f32>,
    plc_run: u32,
    pub popped: u64,
    pub plc_count: u64,
    pub silence_count: u64,
    pub dropped: u64, // late arrivals + catch-up drops
    pub underruns: u64,
}

impl JitterBuffer {
    pub const MIN_TARGET: u32 = 2;
    pub const MAX_TARGET: u32 = 12;
    const DEFAULT_FRAME_LEN: usize = 480; // 48k @ 10ms

    pub fn new(target: u32) -> Self {
        JitterBuffer {
            frames: BTreeMap::new(),
            next_seq: None,
            prebuffering: true,
            target: target.clamp(1, Self::MAX_TARGET),
            frame_len: Self::DEFAULT_FRAME_LEN,
            last_frame: Vec::new(),
            plc_run: 0,
            popped: 0,
            plc_count: 0,
            silence_count: 0,
            dropped: 0,
            underruns: 0,
        }
    }

    pub fn push(&mut self, seq: u32, frame: Vec<f32>) {
        if let Some(next) = self.next_seq {
            if seq < next {
                self.dropped += 1; // too late, already played/PLC'd
                return;
            }
        }
        if !frame.is_empty() {
            self.frame_len = frame.len();
        }
        self.frames.insert(seq, frame);
    }

    /// One 10ms tick. None before the very first frame plays (initial
    /// pre-buffering). Once started it always yields a frame: real when the
    /// next seq is buffered, otherwise PLC/silence — including while
    /// re-buffering after an underrun, so the output cadence never stalls.
    pub fn pop(&mut self) -> Option<Vec<f32>> {
        if self.prebuffering {
            if self.depth() < self.target {
                // Nothing has played yet => no cadence to keep and nothing to
                // conceal with; once started, hold the tick with PLC/silence.
                return match self.next_seq {
                    None => None,
                    Some(_) => Some(self.conceal()),
                };
            }
            self.prebuffering = false;
            self.next_seq = self.frames.keys().next().copied();
        }
        // catch up: drop oldest when depth runs away
        while self.frames.len() as u32 > self.target + 6 {
            if let Some((&oldest, _)) = self.frames.iter().next() {
                self.frames.remove(&oldest);
                self.dropped += 1;
            }
            self.next_seq = self.frames.keys().next().copied();
        }
        let seq = self.next_seq.expect("started");
        match self.frames.remove(&seq) {
            Some(frame) => {
                self.next_seq = Some(seq.wrapping_add(1));
                self.plc_run = 0;
                self.last_frame = frame.clone();
                self.popped += 1;
                Some(frame)
            }
            None => {
                // Underrun. Rebuild depth before playing again and leave
                // next_seq parked on the missing seq: marching it on every
                // starved tick is what made later arrivals look "late" and got
                // them dropped, pinning the buffer at depth 0 forever.
                self.underruns += 1;
                self.prebuffering = true;
                Some(self.conceal())
            }
        }
    }

    /// PLC (decayed repeat of the last real frame) for up to 5 consecutive
    /// starved ticks, silence afterwards.
    fn conceal(&mut self) -> Vec<f32> {
        self.plc_run += 1;
        if self.plc_run <= 5 && !self.last_frame.is_empty() {
            for s in self.last_frame.iter_mut() {
                *s *= 0.7;
            }
            self.plc_count += 1;
            self.last_frame.clone()
        } else {
            self.silence_count += 1;
            vec![0.0; self.frame_len]
        }
    }

    pub fn depth(&self) -> u32 {
        self.frames.len() as u32
    }

    /// True while holding output to (re)build depth to target.
    pub fn prebuffering(&self) -> bool {
        self.prebuffering
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    /// AUTO profile: retarget from observed jitter p95, clamped to [2, 12].
    pub fn update_target(&mut self, jitter_p95_ms: f64, frame_ms: f64) {
        if frame_ms <= 0.0 {
            return;
        }
        let t = (jitter_p95_ms / frame_ms).ceil() as i64 + 1;
        self.target = (t.max(0) as u32).clamp(Self::MIN_TARGET, Self::MAX_TARGET);
    }
}

/// 10ms-frame audio source for the send scheduler.
pub trait FrameSource {
    /// REPLACES the contents of `out` with exactly one frame — implementations
    /// must clear it first. Appending instead is silently destructive: the
    /// engine truncates an over-long frame back to one frame's worth, so the
    /// stream keeps re-sending whatever the FIRST call produced while every
    /// counter and probe still looks healthy.
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool;
    fn sample_rate(&self) -> u32;
}

/// Phase-continuous sine source.
pub struct ToneSource {
    rate: u32,
    frame_samples: usize,
    amp: f64,
    step: f64,
    phase: f64,
}

impl ToneSource {
    pub fn new(freq_hz: f32, amp: f32, sample_rate: u32, frame_ms: u32) -> Self {
        ToneSource {
            rate: sample_rate,
            frame_samples: (sample_rate as u64 * frame_ms as u64 / 1000) as usize,
            amp: amp as f64,
            step: 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64,
            phase: 0.0,
        }
    }
}

impl FrameSource for ToneSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        out.clear();
        for _ in 0..self.frame_samples {
            out.push((self.amp * self.phase.sin()) as f32);
            self.phase += self.step;
        }
        // rem_euclid keeps continuity exactly (sin is 2π-periodic) while
        // bounding f64 magnitude over long runs
        self.phase = self.phase.rem_euclid(2.0 * std::f64::consts::PI);
        true
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }
}

/// Default-microphone source, resampled to 48k. Underruns emit silence so the
/// send cadence never stalls. Create on the sending thread (cpal streams are
/// not Send on all platforms).
pub struct MicSource {
    _cap: LiveCapture,
    rx: AudioRx,
    resampler: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    frame_samples: usize,
}

impl MicSource {
    pub const OUT_RATE: u32 = 48000;
    const FIFO_CAP: usize = 48000; // 1s: bound added latency

    pub fn new(frame_ms: u32) -> Result<Self> {
        let (cap, rx, rate) = LiveCapture::start()?;
        let resampler = if rate == Self::OUT_RATE {
            None
        } else {
            Some(LinearResampler::new(rate, Self::OUT_RATE))
        };
        Ok(MicSource {
            _cap: cap,
            rx,
            resampler,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: (Self::OUT_RATE as u64 * frame_ms as u64 / 1000) as usize,
        })
    }
}

impl FrameSource for MicSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.rx.pop(&mut self.raw);
        match self.resampler.as_mut() {
            None => self.fifo.extend(self.raw.iter().copied()),
            Some(rs) => {
                self.staged.clear();
                rs.process(&self.raw, &mut self.staged);
                self.fifo.extend(self.staged.iter().copied());
            }
        }
        while self.fifo.len() > Self::FIFO_CAP {
            self.fifo.pop_front();
        }
        out.clear();
        if self.fifo.len() >= self.frame_samples {
            out.extend(self.fifo.drain(..self.frame_samples));
        } else {
            out.resize(self.frame_samples, 0.0);
        }
        true
    }

    fn sample_rate(&self) -> u32 {
        Self::OUT_RATE
    }
}

/// System-audio source (spec-m4b §B): whatever this machine is playing,
/// resampled to 48k mono. Same shape as MicSource — underruns emit silence so
/// the send cadence never stalls, and the FIFO is bounded so a stalled reader
/// costs audio, not latency. `excludes_self()` reports whether the chosen
/// backend keeps our own playback out of the capture; a false there while we
/// are also playing the peer's audio is the feedback loop of plan §5.
pub struct SysAudioSource {
    cap: Box<dyn SysAudioCapture>,
    info: BackendInfo,
    resampler: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    frame_samples: usize,
}

impl SysAudioSource {
    pub const OUT_RATE: u32 = 48000;
    const FIFO_CAP: usize = 48000; // 1s

    /// `backend` is a backend id or `sysaudio::BACKEND_AUTO` ("auto").
    pub fn new(frame_ms: u32, backend: &str) -> Result<Self> {
        let info = sysaudio::resolve_backend(backend)?;
        let cap = sysaudio::start_backend(&info.id)?;
        let rate = cap.sample_rate();
        let resampler = if rate == Self::OUT_RATE {
            None
        } else {
            Some(LinearResampler::new(rate, Self::OUT_RATE))
        };
        Ok(SysAudioSource {
            cap,
            info,
            resampler,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: (Self::OUT_RATE as u64 * frame_ms as u64 / 1000) as usize,
        })
    }

    pub fn backend(&self) -> &BackendInfo {
        &self.info
    }

    pub fn excludes_self(&self) -> bool {
        self.info.excludes_self
    }

    /// Rate the backend actually captures at (before the 48k conversion).
    pub fn capture_rate(&self) -> u32 {
        self.cap.sample_rate()
    }
}

impl FrameSource for SysAudioSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.cap.read(&mut self.raw);
        match self.resampler.as_mut() {
            None => self.fifo.extend(self.raw.iter().copied()),
            Some(rs) => {
                self.staged.clear();
                rs.process(&self.raw, &mut self.staged);
                self.fifo.extend(self.staged.iter().copied());
            }
        }
        while self.fifo.len() > Self::FIFO_CAP {
            self.fifo.pop_front();
        }
        out.clear();
        if self.fifo.len() >= self.frame_samples {
            out.extend(self.fifo.drain(..self.frame_samples));
        } else {
            out.resize(self.frame_samples, 0.0);
        }
        true
    }

    fn sample_rate(&self) -> u32 {
        Self::OUT_RATE
    }
}

/// Deterministic sender-side loss injection (LCG seeded by stream_id).
pub struct LossInjector {
    state: u64,
    loss_pct: f64,
}

impl LossInjector {
    pub fn new(stream_id: u32, loss_pct: f32) -> Self {
        LossInjector {
            state: stream_id as u64,
            loss_pct: loss_pct.clamp(0.0, 100.0) as f64,
        }
    }

    /// Advance the LCG; true = drop this packet before sending.
    pub fn should_drop(&mut self) -> bool {
        // Knuth MMIX constants
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let u = (self.state >> 33) as f64 / (1u64 << 31) as f64; // [0,1)
        u * 100.0 < self.loss_pct
    }
}

/// AUTO quality ladder rungs 0..3, s16 mono at these rates.
pub const AUTO_RATES: [u32; 4] = [48000, 32000, 24000, 16000];

pub fn rung_rate(rung: u32) -> u32 {
    AUTO_RATES[rung.min(AUTO_RATES.len() as u32 - 1) as usize]
}

/// Pure sender-side ladder state machine, fed once per 1s stats period.
/// Demote fast (loss>5% or jitter>15ms), promote after 10 clean periods
/// (loss<0.5% and jitter<5ms); middling stats reset the clean streak.
pub struct AutoLadder {
    rung: u32,
    clean: u32,
    pub rung_changes: u32,
}

impl AutoLadder {
    pub fn new() -> Self {
        AutoLadder { rung: 0, clean: 0, rung_changes: 0 }
    }

    pub fn rung(&self) -> u32 {
        self.rung
    }

    pub fn sample_rate(&self) -> u32 {
        rung_rate(self.rung)
    }

    /// Some(new rung index) only when the rung actually changed.
    pub fn feed_stats(&mut self, loss_pct: f64, jitter_ms: f64) -> Option<u32> {
        if loss_pct > 5.0 || jitter_ms > 15.0 {
            self.clean = 0;
            if self.rung < AUTO_RATES.len() as u32 - 1 {
                self.rung += 1;
                self.rung_changes += 1;
                return Some(self.rung);
            }
            return None;
        }
        if loss_pct < 0.5 && jitter_ms < 5.0 {
            self.clean = self.clean.saturating_add(1);
            if self.clean >= 10 && self.rung > 0 {
                self.clean = 0;
                self.rung -= 1;
                self.rung_changes += 1;
                return Some(self.rung);
            }
            return None;
        }
        self.clean = 0;
        None
    }
}

impl Default for AutoLadder {
    fn default() -> Self {
        Self::new()
    }
}
