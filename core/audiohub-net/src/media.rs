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
use audiohub_core::latency::{DropMode, SourceDepths, StageDepth, StageId, NO_DEPTHS};
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

    /// 从 `next_seq` 起**连续**的帧数——真正的排队深度（规格 §7.2 R10）。
    ///
    /// `depth()` 返回的是 `BTreeMap` 的条目数，乱序到达时它**不等于**「队首样本
    /// 前面还排着多少样本」：若 `next_seq` 缺失而更远期的 seq 已经入表，
    /// `len()` 把「洞之后的帧」也算进去，深度被高估。举例：next_seq=10 缺失，
    /// 表里有 {11,12,13} ⇒ `depth()=3`（谎报 30 ms 排队），而 `contiguous()=0`
    /// ——下一个 tick 一定 underrun，排队深度实际是 0。
    ///
    /// 两个都要上报：`depth()` 是「占了多少内存/多少帧在手上」，`contiguous()`
    /// 是「按现在的节拍还能连续放多久」。**延迟分项用 contiguous**，因为它才是
    /// 那条「以已知速率排空 ⇒ N/rate 就是确切驻留时间」的定理成立的前提；
    /// 有洞的部分并不会以 100 帧/秒的速度被放出来。
    pub fn contiguous(&self) -> u32 {
        let Some(&first) = self.frames.keys().next() else {
            return 0;
        };
        let start = match self.next_seq {
            // 队首还没到：一个样本都排不上队，哪怕表里有一堆更晚的帧。
            Some(n) => {
                if !self.frames.contains_key(&n) {
                    return 0;
                }
                n
            }
            // 还没起播（初始预缓冲）：从表里最小的 seq 起算，那正是起播后的队首。
            None => first,
        };
        let mut n = 0u32;
        let mut want = start;
        // 表长受 `target + 6` 约束（≤18 项），所以这个遍历是常数级的。
        // seq 回绕在 100 帧/秒下要 497 天，`push` 的 `seq < next` 比较同样没做
        // 回绕处理——两处口径一致，不在这里单独发明一套。
        for (&seq, _) in self.frames.range(start..) {
            if seq != want {
                break;
            }
            n += 1;
            want = want.wrapping_add(1);
        }
        n
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

    /// 本源在「交给发送调度器之前」还压着多少音频（规格 §3.2 的级 1 / 3 / 3′）。
    ///
    /// 默认 `NO_DEPTHS` = 这个源没有任何可观测的排队。`ToneSource` 就属此类：
    /// 它是即时合成的，不存在队列——**这不是「测不到」，是「确实没有」**，
    /// 所以给空数组而不是给 0 样本的假读数。
    ///
    /// 返回定长数组而非 `Vec`：本方法在 10ms 节拍上被调用，那里不允许分配
    /// （规格附录约束 3）。
    fn depths(&self) -> SourceDepths {
        NO_DEPTHS
    }
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
    /// 只为让 cpal 流活着而持有。`Option` 是给测试留的口子：`LiveCapture` 里
    /// 是一个 `cpal::Stream`，没有真设备就造不出来，而 `depths()` 的接线
    /// （哪个读数进哪个字段、丢弃方向标成什么）恰恰是必须被真调用一次才作数的
    /// 那部分。`None` 只在单元测试里出现，运行时永远是 `Some`。
    _cap: Option<LiveCapture>,
    rx: AudioRx,
    resampler: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    frame_samples: usize,
    /// FIFO 满时丢掉的样本数（累计）。方向是 **`DropMode::Oldest`**
    /// （`while len > CAP { pop_front() }`）：饱和时驻留恰好 = CAP/48000 = 1 秒，
    /// 音频连续，听感是「恒定迟到但不断」。
    dropped: u64,
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
            _cap: Some(cap),
            rx,
            resampler,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: (Self::OUT_RATE as u64 * frame_ms as u64 / 1000) as usize,
            dropped: 0,
        })
    }

    pub fn fifo_len(&self) -> u32 {
        self.fifo.len() as u32
    }

    pub fn fifo_cap(&self) -> u32 {
        Self::FIFO_CAP as u32
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
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
            self.dropped += 1; // 丢弃行为未改，只是现在数得出来
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

    /// 麦克风源横跨两级：声卡采集环（2 s，丢最新）→ 重采样 → 发送 FIFO
    /// （1 s，丢最旧）。两级的**速率不同**（采集环走设备速率，FIFO 已经是
    /// 48k），所以必须分开上报，不能相加成一个数。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::CapRing,
                samples: self.rx.queued(),
                capacity: self.rx.capacity(),
                rate: self.rx.rate(),
                dropped: Some(self.rx.dropped()),
                drop_mode: DropMode::Newest,
            }),
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo_len(),
                capacity: self.fifo_cap(),
                rate: Self::OUT_RATE,
                dropped: Some(self.dropped),
                drop_mode: DropMode::Oldest,
            }),
        ]
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
    /// 同 `MicSource::dropped`：方向是 `DropMode::Oldest`。
    dropped: u64,
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
            dropped: 0,
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

    pub fn fifo_len(&self) -> u32 {
        self.fifo.len() as u32
    }

    pub fn fifo_cap(&self) -> u32 {
        Self::FIFO_CAP as u32
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
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
            self.dropped += 1;
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

    /// 只有发送 FIFO 一级：系统音频后端自己的内部缓冲不经过 `AudioRx`，
    /// 从这里读不到——**所以不报**，而不是报 0（规格 §7.2 R11 记着这条口径缺口：
    /// Windows loopback 交付的样本尚未经过本机 DAC，那部分是 P1 的活）。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo_len(),
                capacity: self.fifo_cap(),
                rate: Self::OUT_RATE,
                dropped: Some(self.dropped),
                drop_mode: DropMode::Oldest,
            }),
            None,
        ]
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

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    fn frame() -> Vec<f32> {
        vec![0.1; 480]
    }

    /// 规格 §7.2 R10：有洞时 `depth()` 高估，`contiguous()` 才是真排队深度。
    /// 这是把「延迟分项用哪个数」这条决定钉死的断言。
    #[test]
    fn depth_overcounts_across_a_hole_but_contiguous_does_not() {
        let mut jb = JitterBuffer::new(2);
        // 先起播，让 next_seq 落在 10
        jb.push(10, frame());
        jb.push(11, frame());
        assert!(jb.pop().is_some());
        assert_eq!(jb.next_seq, Some(11));

        // 11 已在表里，12 缺失，13/14 提前到达
        jb.push(13, frame());
        jb.push(14, frame());
        assert_eq!(jb.depth(), 3, "len() 把洞之后的两帧也算进去了");
        assert_eq!(jb.contiguous(), 1, "从 next_seq=11 起只有一帧是连续的");
    }

    /// 队首本身缺失 = 一个样本都排不上队，哪怕表里堆了一串更晚的帧。
    /// 此时 `depth()` 谎报 30 ms 排队，而下一个 tick 一定 underrun。
    #[test]
    fn a_missing_head_means_zero_queue_however_full_the_map_is() {
        let mut jb = JitterBuffer::new(2);
        jb.push(10, frame());
        jb.push(11, frame());
        assert!(jb.pop().is_some()); // 放掉 10，next_seq = 11
        jb.frames.remove(&11); // 制造队首空洞
        jb.push(12, frame());
        jb.push(13, frame());
        jb.push(14, frame());
        assert_eq!(jb.depth(), 3);
        assert_eq!(jb.contiguous(), 0, "队首没到就是 0，不是 3");
    }

    /// 还没起播时，队首就是表里最小的 seq。
    #[test]
    fn before_playback_starts_contiguous_counts_from_the_lowest_seq() {
        let mut jb = JitterBuffer::new(4);
        jb.push(100, frame());
        jb.push(101, frame());
        jb.push(103, frame());
        assert_eq!(jb.next_seq, None, "还没起播");
        assert_eq!(jb.depth(), 3);
        assert_eq!(jb.contiguous(), 2, "100,101 连续，103 之前有洞");
    }

    #[test]
    fn an_empty_buffer_is_zero_on_both_readings() {
        let jb = JitterBuffer::new(2);
        assert_eq!(jb.depth(), 0);
        assert_eq!(jb.contiguous(), 0);
    }

    /// 无洞时两者必须一致——否则 UI 会看到两个互相矛盾的深度。
    #[test]
    fn contiguous_equals_depth_when_nothing_is_missing() {
        let mut jb = JitterBuffer::new(2);
        for seq in 0..6 {
            jb.push(seq, frame());
        }
        assert!(jb.pop().is_some());
        assert_eq!(jb.depth(), jb.contiguous());
    }

    // ------------------------------------------------ 源侧 depths() 的接线
    //
    // 下面几条**必须真的调用 `FrameSource::depths()`** 并断言它返回的东西。
    // 上一版这里写的是「构造一个 StageDepth 字面量，再断言它等于自己刚写下的
    // 那个 DropMode」——生产代码把 `src_fifo` 标成 `Newest` 它一声不吭，而
    // 那个标签正是「恒定迟到但连续」与「迟到 + 周期性断续」唯一的区分
    // （规格 §0.2：两者深度读数完全简并）。

    /// 站在 cpal 采集回调的位置上写环，站在系统音频后端的位置上交样本。
    /// 环、FIFO、重采样器全是**真的**，只有设备是假的。
    struct FakeSys {
        rate: u32,
        /// 每次 `read` 交出多少样本。
        chunk: usize,
        /// 单调递增的样本值：靠它能分辨 FIFO 里剩下的到底是**早**的还是**晚**
        /// 的那一批，也就是丢弃方向。全填同一个常数就永远看不出来。
        n: u32,
    }

    impl SysAudioCapture for FakeSys {
        fn read(&mut self, out: &mut Vec<f32>) -> usize {
            for _ in 0..self.chunk {
                self.n += 1;
                out.push(self.n as f32);
            }
            self.chunk
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
    }

    fn fake_backend() -> BackendInfo {
        BackendInfo {
            id: "fake".to_string(),
            name: "fake".to_string(),
            available: true,
            excludes_self: true,
            note: String::new(),
        }
    }

    fn sys_source(rate: u32, chunk: usize) -> SysAudioSource {
        SysAudioSource {
            cap: Box::new(FakeSys { rate, chunk, n: 0 }),
            info: fake_backend(),
            resampler: (rate != SysAudioSource::OUT_RATE)
                .then(|| LinearResampler::new(rate, SysAudioSource::OUT_RATE)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        }
    }

    /// `MicSource::depths()` 的两级接线：采集环（**2 秒**、设备速率、丢**最新**）
    /// 与发送 FIFO（1 秒、48k、丢**最旧**）。两级速率不同，绝不能合并成一个数。
    ///
    /// 环与 FIFO 都是真的：环由 `AudioRx::detached_for_test` 造出（与
    /// `LiveCapture::on_device` 同构），FIFO 由真正跑一遍 `next_frame()` 填出。
    #[test]
    fn mic_source_reports_a_2s_capture_ring_and_a_1s_send_fifo() {
        // 设备速率故意取 44100：采集环那一级若被硬写成 48000，ms 会偏 −8.8%。
        let (rx, mut feed) = AudioRx::detached_for_test(44_100);
        let mut mic = MicSource {
            _cap: None,
            rx,
            resampler: Some(LinearResampler::new(44_100, MicSource::OUT_RATE)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };

        // 声卡交来 4410 个样本（100 ms @44.1k），还没被取走。
        assert_eq!(feed.write(&vec![0.5; 4_410]), 4_410);
        let [cap, fifo] = mic.depths();
        let cap = cap.expect("麦克风源必须报采集环这一级");
        assert_eq!(cap.id, StageId::CapRing);
        assert_eq!(cap.samples, 4_410, "环里此刻积着的就是刚写进去的那些");
        assert_eq!(cap.capacity, 88_200, "2 秒 @44.1k —— 不是 1 秒（规格 §0.4）");
        assert_eq!(cap.rate, 44_100, "采集环走**设备**速率，不是 48000");
        assert_eq!(cap.ms(), Some(100.0), "4410 / 44100 = 100 ms");
        assert_eq!(
            cap.drop_mode,
            DropMode::Newest,
            "采集环是 push_slice 短写：丢的是新样本，听感是断续"
        );
        assert_eq!(cap.dropped, Some(0), "还没溢出过 —— 0 是真读数，不是『观测不到』");
        // FIFO 那一级此刻是空的，但它必须**存在**（0 样本 ≠ 这一级不存在）。
        let fifo = fifo.expect("麦克风源必须报发送 FIFO 这一级");
        assert_eq!(fifo.id, StageId::SrcFifo);
        assert_eq!(fifo.samples, 0);
        assert_eq!(fifo.rate, MicSource::OUT_RATE, "FIFO 已经是 48k 了");
        assert_eq!(fifo.capacity, 48_000, "1 秒 @48k");
        assert_eq!(
            fifo.drop_mode,
            DropMode::Oldest,
            "`while len > CAP {{ pop_front() }}` 丢的是最旧的：恒定迟到但连续"
        );

        // 跑一个 tick：环被 pop 排空、样本经重采样进 FIFO、取走一帧 480。
        let mut out = Vec::new();
        mic.next_frame(&mut out);
        assert_eq!(out.len(), 480);
        let [cap, fifo] = mic.depths();
        assert_eq!(cap.unwrap().samples, 0, "AudioRx::pop 全量排空（规格 §0.4）");
        let fifo = fifo.unwrap();
        // 4410 @44.1k -> 48k 约 4800 个样本，取走 480 后剩下的都还压在 FIFO 里。
        assert!(
            (4_000..=4_400).contains(&fifo.samples),
            "重采样后 4800 减去取走的 480，got {}",
            fifo.samples
        );
        assert_eq!(fifo.ms(), Some(fifo.samples as f64 * 1000.0 / 48_000.0));
    }

    /// 采集环溢出丢的是**新**样本，且计数穿过 `depths()` 原样上报。
    /// 这一级溢出丢的是真实音频——它是**音质**指标的输入，不是延迟嫌疑。
    #[test]
    fn mic_source_capture_ring_overflow_is_counted_as_newest_dropped() {
        let (rx, mut feed) = AudioRx::detached_for_test(48_000);
        let mic = MicSource {
            _cap: None,
            rx,
            resampler: None,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };
        // 环 = 2 秒 = 96000。灌 100000 进去，最后 4000 个写不下。
        let wrote = feed.write(&vec![0.5; 100_000]);
        let cap = mic.depths()[0].expect("采集环这一级");
        assert_eq!(cap.samples as usize, wrote, "写进去多少就积着多少");
        assert_eq!(
            cap.dropped,
            Some(100_000 - wrote as u64),
            "短写的部分必须数得出来 —— 这里以前是全链路的遥测黑洞"
        );
        assert!(cap.dropped.unwrap() > 0, "确实溢出了");
        assert!(cap.saturated());
    }

    /// **任务点名：把 1 秒 FIFO 与 2 秒采集环分别灌满，看遥测报出多少。**
    ///
    /// 这两级都在提供方（mac→win 那条链路的发送端），是「秒级延迟」最可能的
    /// 藏身处之二。下面把真的 `VecDeque` 与真的 `HeapRb` 灌到**恰好容量**，
    /// 断言 `depths()` 报出 1000.0 / 2000.0 ms 而不是沉默。
    ///
    /// 为什么要单独有这一条：稳态下跑 `next_frame()` 观测到的上限是 47_520
    /// （990 ms）——修剪到 48000 之后本 tick 又被取走一帧。那 10 ms 是**相位
    /// 约定**（读数取自「排在这一帧前面的样本数」），不是误差。这条测试把
    /// 「FIFO 真的满着的时候读数是多少」单独钉死，免得 990 这个数字被后人
    /// 当成 1 秒 FIFO 的物理上限。
    #[test]
    fn a_brimming_send_fifo_reads_exactly_one_second_and_the_capture_ring_two() {
        let (rx, mut feed) = AudioRx::detached_for_test(48_000);
        let mut mic = MicSource {
            _cap: None,
            rx,
            resampler: None,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };

        // 1 秒发送 FIFO 装到**恰好**容量。样本是真的、队列是真的。
        mic.fifo.extend(std::iter::repeat(0.3f32).take(MicSource::FIFO_CAP));
        // 2 秒采集环同样装到恰好容量（96000 @48k）。
        assert_eq!(feed.write(&vec![0.3f32; 96_000]), 96_000);

        let [cap, fifo] = mic.depths();
        let cap = cap.expect("采集环这一级");
        let fifo = fifo.expect("发送 FIFO 这一级");
        assert_eq!(fifo.samples, 48_000);
        assert_eq!(
            fifo.ms(),
            Some(1000.0),
            "满载的 1 秒发送 FIFO = 1000 ms —— 这一级要是不报，用户的那一秒就没人说"
        );
        assert!(fifo.saturated());
        assert_eq!(fifo.drop_mode, DropMode::Oldest, "丢最旧 ⇒ 恒定迟到但连续");
        assert_eq!(cap.samples, 96_000);
        assert_eq!(
            cap.ms(),
            Some(2000.0),
            "采集环是 **2 秒**（规格 §0.4 的修正三），满载就是 2000 ms"
        );
        assert!(cap.saturated());
        assert_eq!(cap.drop_mode, DropMode::Newest, "push_slice 短写 ⇒ 迟到 + 断续");

        // 稳态跑一 tick 之后回落到 990 ms —— 相位约定，不是读数变坏。
        let mut out = Vec::new();
        mic.next_frame(&mut out);
        assert_eq!(mic.depths()[1].unwrap().ms(), Some(990.0));
    }

    /// 发送 FIFO 溢出丢的是**最旧**的，饱和时驻留恰好 1.000 秒。
    /// 真的跑 `next_frame()` 把 FIFO 灌爆，而不是手填一个 `samples: 48_000`。
    #[test]
    fn source_fifo_drops_oldest_and_saturates_at_exactly_one_second() {
        // 每 tick 交 5000 个样本、只被取走 480 —— 十几个 tick 就撑爆 1 秒上限。
        let mut src = sys_source(48_000, 5_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            src.next_frame(&mut out);
        }
        let [fifo, second] = src.depths();
        assert!(second.is_none(), "系统音频源只有 FIFO 一级，后端内部缓冲读不到");
        let fifo = fifo.expect("发送 FIFO 这一级");
        assert_eq!(fifo.id, StageId::SrcFifo);
        // 修剪到 CAP=48000，随即本 tick 的 480 被取走 ⇒ 47520 = 990 ms。
        assert_eq!(fifo.samples, 47_520, "贴着 1 秒上限（刚被取走一帧）");
        assert_eq!(fifo.capacity, 48_000);
        assert_eq!(fifo.rate, 48_000);
        assert!(fifo.saturated(), "≥95% 容量");
        assert_eq!(fifo.ms(), Some(990.0), "1 秒 FIFO 被灌满 = 将近 1000 ms 驻留");
        assert_eq!(
            fifo.drop_mode,
            DropMode::Oldest,
            "pop_front 丢最旧：听感是恒定迟到但连续，与播放环的丢最新完全不同"
        );
        // 20 tick 收 100000、放 9600、还剩 47520，其余被丢。
        let dropped = fifo.dropped.expect("源侧的丢弃本进程数得出来");
        assert_eq!(dropped, 100_000 - 9_600 - 47_520);
        assert!(dropped > 0, "没溢出就谈不上方向");

        // **丢的确实是最旧的**：源交出的是 1,2,3,… 的递增序列，取出的这一帧
        // 必须落在序列的**尾部**。若真丢了最新的（`DropMode::Newest`），这里
        // 拿到的会是最开头那 480 个样本。
        src.next_frame(&mut out);
        assert!(
            out[0] > 50_000.0,
            "FIFO 里留下的必须是晚到的样本，got {} —— 丢弃方向反了",
            out[0]
        );
    }

    /// 不溢出时不能凭空记丢弃——`dropped` 的斜率是「稳态产销失配」与「曾被一次
    /// 卡顿灌满」两种病理的唯一区分（规格 §3.3），虚报它就把诊断毁了。
    #[test]
    fn a_source_fifo_within_budget_drops_nothing() {
        let mut src = sys_source(48_000, 480); // 收 480 放 480，收支刚好平衡
        let mut out = Vec::new();
        for _ in 0..50 {
            src.next_frame(&mut out);
        }
        let fifo = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(fifo.dropped, Some(0));
        assert!(!fifo.saturated());
        assert_eq!(fifo.samples, 0, "来多少走多少");
    }

    /// 后端速率不是 48k 时，FIFO 那一级仍然按 **48000** 换算——它在重采样
    /// **之后**。这条与采集环那条（走设备速率）是一对，方向相反，写反了
    /// 任何一个都会静默偏 ±8.8%。
    #[test]
    fn the_send_fifo_converts_at_48k_even_when_the_backend_runs_at_44k1() {
        let mut src = sys_source(44_100, 4_410); // 100 ms @44.1k / tick
        let mut out = Vec::new();
        src.next_frame(&mut out);
        let fifo = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(fifo.rate, 48_000, "FIFO 在重采样之后，是 48k");
        assert_eq!(fifo.ms(), Some(fifo.samples as f64 * 1000.0 / 48_000.0));
        // 100 ms 进来、10 ms 被取走 ⇒ 剩约 90 ms，而不是按 44.1k 算的 98 ms。
        let ms = fifo.ms().unwrap();
        assert!((ms - 90.0).abs() < 2.0, "约 90 ms，got {ms:.2}");
    }

    /// 即时合成的源没有队列——给空数组，不是给 0 样本的假读数。
    #[test]
    fn a_synthesised_source_reports_no_stages_at_all() {
        let t = ToneSource::new(1000.0, 0.5, 48_000, 10);
        assert!(t.depths().iter().all(|s| s.is_none()));
    }
}
