//! Media plane wiring: single shared UDP socket, 10ms send scheduler with
//! fan-out + AUTO resample-before-encode, receive/decrypt into jitter buffers,
//! 10ms mixer with soft clip and a 2s post-mix ring for mix_verdicts.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audiohub_core::audio::{self, AudioTx, LivePlayback};
use audiohub_core::dsp::{self, LinearResampler, ToneVerdict};
use audiohub_core::latency::{DropMode, SourceDepths, StageDepth, StageId, StageSlot, NO_DEPTHS};
use audiohub_core::sysaudio::{self, SysAudioCapture};
use audiohub_net::media::{rung_rate, FrameSource, LossInjector, MediaCrypto, MicSource, ToneSource};
use audiohub_net::packet::{Codec, Header, Kind};

use crate::{dlog, lk, rd, DaemonInner, RxStream, TxShared};

const FRAME_MS: u64 = 10;
const F48: usize = 480; // 48k @ 10ms
const RING_CAP: usize = 96000; // 2s @ 48k
const TONE_AMP: f32 = 0.5;

/// `TxShared::stages` 的最后一槽，专给级 4 `send_pace`。
///
/// 前两槽由 `SourceDepths` 广播（源自己能观测到的排队），第三槽是**调度器自己**
/// 那一级：`tx_loop` 每 10 ms 一次性取走 480 个样本，而生产者跑在设备时钟上，
/// 把连续到达量化到打包边界的那半个 tick 是这个循环造成的，不是任何一个源造成的
/// ——所以它由这里发射，不由 `depths()` 发射。
const SEND_PACE_SLOT: usize = 2;

/// 清空一条发送流的全部分项槽。
///
/// **不是「顺手清一下」**：`TxShared` 的生命周期比 `tx_loop` 里的 `TxStream`
/// 长（会话表还持有它，报告线程还在读），源被收尸之后若不清，UI 会继续显示一段
/// 早已不存在的排队，而且**没有任何字段说它是陈的**。
fn clear_send_stages(st: &TxStream) {
    for slot in st.shared.stages.iter() {
        slot.store(None);
    }
}

/// 本源这一 tick 该不该报级 4 `send_pace`（常数 5 ms）。
///
/// 判据：**这个源有没有真实排队**。这 5 ms 是把连续到达量化到 10 ms 打包边界的
/// 期望等待，成立的前提是到达相位相对 tick 均匀分布——那要求生产者跑在**另一个
/// 时钟**上（设备回调 / 驱动 IOProc），而「有队列」正是这件事的同义词。
/// `ToneSource` 是在 tick 里现合成的，样本诞生的时刻就是被取走的时刻，等待恒为
/// 0；给它记 5 ms 是凭空捏造。驱动没附着时 `HalSpeakerSource` 报 `NO_DEPTHS`，
/// 那一级连同节拍一起不存在。
fn send_pace_for(depths: &SourceDepths) -> Option<StageDepth> {
    depths
        .iter()
        .any(|d| d.is_some())
        .then(StageDepth::send_pace)
}

/// 把一个源本 tick 的各级深度发布到一条发送流的槽里（含级 4 `send_pace`）。
///
/// 与 `publish_play_ring` 同一条理由拆出来：这三行是**接线**——哪一级进哪个槽、
/// 空槽清不清、节拍这一级由谁发射。`tx_loop` 里要一个真实设备、一条 UDP socket
/// 和一整张源表才走得到它，于是接线本身没法被断言，而漏掉的从来是接线不是逻辑
/// （`send_pace` 就曾经在枚举里声明、在规格里编号、**全仓库零发布点**）。
///
/// 每 tick 都写，包括 `None`：源换过之后（默认输入设备变化触发 `MicSource`
/// 重建）若不清槽，报告线程会一直读到已经不存在的那一级。
pub(crate) fn publish_send_stages(stages: &[StageSlot; 3], depths: &SourceDepths) {
    for (slot, d) in stages.iter().zip(depths.iter()) {
        slot.store(*d);
    }
    stages[SEND_PACE_SLOT].store(send_pace_for(depths));
}

fn poll_tick(kind: ErrorKind) -> bool {
    // see audiohub-net session.rs: Windows latches ICMP unreachable as
    // ConnectionReset on unconnected UDP sockets
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
    )
}

// ---------------------------------------------------------------- tx engine

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum SourceSpec {
    Tone { freq_bits: u32 },
    Mic,
    /// What this machine is playing (spec-m4b §B2). The backend id is part of
    /// the dedup key: two streams naming different backends are two captures.
    SysAudio { backend: String },
    /// What an application played into ONE peer's virtual speaker (spec-m5b
    /// §5.4). The slot is part of the dedup key, so each speaker ring gets
    /// exactly one consumer entry — which is what keeps the halbridge SPSC rule
    /// (exactly one reader per ring) literally true with sixteen of them.
    ///
    /// Collapsing this back to a slot-less variant is the single most dangerous
    /// simplification available here: every peer's audio would come out of one
    /// ring, every positive test would still pass, and the only symptom would
    /// be one peer hearing another's audio.
    HalSpeaker { slot: u8 },
}

impl SourceSpec {
    pub(crate) fn tone(freq: f32) -> SourceSpec {
        SourceSpec::Tone { freq_bits: freq.to_bits() }
    }

    fn label(&self) -> String {
        match self {
            SourceSpec::Tone { freq_bits } => format!("tone {}Hz", f32::from_bits(*freq_bits)),
            SourceSpec::Mic => "mic".to_string(),
            SourceSpec::SysAudio { backend } => format!("sysaudio '{backend}'"),
            SourceSpec::HalSpeaker { slot } => format!("hal speaker slot {slot}"),
        }
    }
}

pub(crate) enum TxCmd {
    Add {
        stream_id: u32,
        key: [u8; 32],
        /// Per-stream media salt from the stream opener (frozen API).
        salt: Vec<u8>,
        dest: SocketAddr,
        spec: SourceSpec,
        loss_pct: f32,
        shared: Arc<TxShared>,
        /// Reports whether the source actually started, so the control-plane
        /// handler can answer AcceptStream/RejectStream truthfully.
        ack: Option<mpsc::Sender<std::result::Result<(), String>>>,
    },
    Remove {
        stream_id: u32,
    },
}

/// Mixer-thread commands. A cpal stream is not `Send` on every platform, so a
/// bridge device can only be opened (and dropped) on the thread that renders
/// into it — the ack carries the real open error back to `session.open`.
pub(crate) enum MixCmd {
    OpenBridge {
        device: String,
        /// Single-winner commit flag between the opener and the mixer. The
        /// mixer does the slow part (cpal) first and only KEEPS the device if
        /// it wins this flag; an opener whose ack deadline expired claims it on
        /// the way out. `true` after the mixer's swap therefore means exactly
        /// "a refcount is held for this open" — nobody has to guess.
        claim: Arc<AtomicBool>,
        ack: mpsc::Sender<std::result::Result<(), String>>,
    },
    ReleaseBridge {
        device: String,
    },
}

/// How long `session.open` waits for the mixer to actually open the bridge
/// device before it reports the session as failed.
const BRIDGE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves a bridge selector to the device name the mixer will actually open.
/// Bridges are refcounted by this name, so it MUST be the resolved one: keyed
/// by the raw selector, "BlackHole" and "BlackHole 2ch" are two entries for one
/// card — opened twice, and neither release frees the other.
///
/// audiohub-core resolves privately inside `LivePlayback::start_on` and exposes
/// only the listing, so the rule is mirrored here: exact match first, then a
/// unique case-insensitive prefix.
pub fn resolve_bridge_device(names: &[String], query: &str) -> Result<String> {
    let q = query.trim();
    if q.is_empty() {
        return Err(anyhow!("empty bridge device name"));
    }
    if let Some(n) = names.iter().find(|n| n.as_str() == q) {
        return Ok(n.clone());
    }
    let ql = q.to_lowercase();
    let hits: Vec<&String> = names
        .iter()
        .filter(|n| n.to_lowercase().starts_with(&ql))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => Err(anyhow!(
            "no output device matches {q:?}; available: [{}]",
            names.join(", ")
        )),
        _ => Err(anyhow!(
            "output device name {q:?} is ambiguous; candidates: [{}]",
            hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Opens (or ref-counts) the named output device on the mixer thread and hands
/// back the RESOLVED device name, which is the refcount key the caller must
/// release with. spec-m4c §B: a failure here fails the session open with the
/// device's real reason — there is no fallback to the default output.
pub(crate) fn open_bridge(inner: &DaemonInner, device: &str) -> Result<String> {
    let resolved = resolve_bridge_device(&audio::list_output_devices(), device)
        .map_err(|e| anyhow!("open bridge device '{device}': {e:#}"))?;
    let claim = Arc::new(AtomicBool::new(false));
    let (ack_tx, ack_rx) = mpsc::channel();
    lk(&inner.mix_cmds)
        .send(MixCmd::OpenBridge {
            device: resolved.clone(),
            claim: claim.clone(),
            ack: ack_tx,
        })
        .map_err(|_| anyhow!("mixer unavailable"))?;
    match ack_rx.recv_timeout(BRIDGE_ACK_TIMEOUT) {
        Ok(Ok(())) => Ok(resolved),
        Ok(Err(e)) => Err(anyhow!("{e}")),
        Err(_) => {
            // Nothing will ever release what the mixer may still be about to
            // take: this open has no session behind it any more. Winning the
            // claim tells a late mixer to keep nothing; losing it means the
            // refcount is already real, so balance it here.
            if claim.swap(true, Ordering::SeqCst) {
                release_bridge(inner, &resolved);
            }
            Err(anyhow!(
                "bridge device '{resolved}' did not open within {BRIDGE_ACK_TIMEOUT:?}"
            ))
        }
    }
}

pub(crate) fn release_bridge(inner: &DaemonInner, device: &str) {
    let _ = lk(&inner.mix_cmds).send(MixCmd::ReleaseBridge { device: device.to_string() });
}

struct TxStream {
    id: u32,
    crypto: MediaCrypto,
    dest: SocketAddr,
    spec: SourceSpec,
    loss: LossInjector,
    seq: u32,
    rung: u32,
    rs: Option<LinearResampler>, // 48k -> rung rate, recreated on rung switch
    rs_last: f32,                // last source sample; seeds the next resampler
    staged: Vec<f32>,
    shared: Arc<TxShared>,
}

struct SourceEnt {
    src: Src,
    refs: usize,
    frame: Vec<f32>, // one 48k frame per tick, broadcast to all attached streams
    /// 本 tick 读到的各级深度，随 `frame` 一起广播给挂在这个源上的每条流。
    /// 读一次、发 N 份：物理队列只有一份（规格 §7.2 R8）。
    depths: SourceDepths,
}

/// A media source plus the one thing `FrameSource` cannot express: a system
/// capture that has died for good (group C's frozen `SysAudioCapture::failed`).
enum Src {
    Frame(Box<dyn FrameSource>),
    Sys(SysAudioFrames),
}

impl Src {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        match self {
            Src::Frame(f) => f.next_frame(out),
            Src::Sys(s) => s.next_frame(out),
        }
    }

    /// 本源在交给发送调度器之前压着的各级排队（规格 §3.2 的级 1 / 3 / 3′）。
    /// 无分配、常数次 `len()`，可以在 10 ms 节拍上调用。
    fn depths(&self) -> SourceDepths {
        match self {
            Src::Frame(f) => f.depths(),
            Src::Sys(s) => s.depths(),
        }
    }

    /// `Some(reason)` once the source can never produce audio again.
    fn failed(&self) -> Option<String> {
        match self {
            Src::Frame(_) => None,
            Src::Sys(s) => s.cap.failed(),
        }
    }
}

/// Bridges `SysAudioCapture` into the 10ms send scheduler: the capture appends
/// mono f32 at its own rate in irregular WASAPI-sized chunks, the scheduler
/// wants exactly one 48k frame per tick. Underruns emit silence rather than
/// stalling the cadence — a loopback capture is silent whenever nothing plays.
struct SysAudioFrames {
    cap: Box<dyn SysAudioCapture>,
    backend: String,
    excludes_self: bool,
    rs: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    /// FIFO 满时丢掉的样本数。方向是 `DropMode::Oldest`（`pop_front`）。
    dropped: u64,
}

impl SysAudioFrames {
    /// 1s: a reader that fell behind must drop old audio, never grow unbounded.
    const FIFO_CAP: usize = 48000;

    fn new(cap: Box<dyn SysAudioCapture>, backend: String, excludes_self: bool) -> SysAudioFrames {
        let rate = cap.sample_rate();
        SysAudioFrames {
            cap,
            backend,
            excludes_self,
            rs: (rate != 48000).then(|| LinearResampler::new(rate, 48000)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            dropped: 0,
        }
    }

    /// 只有发送 FIFO 一级：后端自己的内部缓冲从这里读不到，**所以不报**，
    /// 而不是报 0（规格 §7.2 R11 记着这条口径缺口）。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo.len() as u32,
                capacity: Self::FIFO_CAP as u32,
                rate: 48_000,
                dropped: Some(self.dropped),
                drop_mode: DropMode::Oldest,
            }),
            None,
        ]
    }

    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.cap.read(&mut self.raw);
        match self.rs.as_mut() {
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
        if self.fifo.len() >= F48 {
            out.extend(self.fifo.drain(..F48));
        } else {
            out.resize(F48, 0.0);
        }
        true
    }
}

/// plan §5 hard requirement, fired at most once per process: a backend that
/// cannot keep our own playback out of its capture, running while we are also
/// playing a peer's audio, closes an audio loop (peer mic -> our speakers ->
/// our capture -> back to the peer). Warn the operator; do not spam them.
static SELF_CAPTURE_WARNED: AtomicBool = AtomicBool::new(false);

/// True while some received stream is routed to this machine's real output.
fn playing_remote_audio(inner: &DaemonInner) -> bool {
    rd(&inner.rx_table).values().any(|r| r.is_spk || r.monitor)
}

fn warn_feedback_risk(inner: &DaemonInner, backend: &str) {
    // cheap guard first: this runs once a second while such a capture is live
    if SELF_CAPTURE_WARNED.load(Ordering::Relaxed) || !playing_remote_audio(inner) {
        return;
    }
    if SELF_CAPTURE_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    dlog!(
        "[audiohubd] WARNING: sysaudio backend '{backend}' does not exclude this process, and \
         this machine is also playing a peer's audio — the capture will contain that audio and \
         feed it back (plan §5). Use a backend with excludes_self, or stop playing remote audio \
         while mirroring."
    );
}

fn build_source(inner: &DaemonInner, spec: &SourceSpec) -> Result<Src> {
    Ok(match spec {
        SourceSpec::Tone { freq_bits } => Src::Frame(Box::new(ToneSource::new(
            f32::from_bits(*freq_bits),
            TONE_AMP,
            48000,
            FRAME_MS as u32,
        ))),
        SourceSpec::Mic => Src::Frame(Box::new(
            MicSource::new(FRAME_MS as u32).context("start microphone capture")?,
        )),
        SourceSpec::SysAudio { backend } => {
            // resolve first: start_backend would re-resolve "auto" and we need
            // the concrete id + excludes_self for the feedback check anyway
            let info = sysaudio::resolve_backend(backend)?;
            let cap = sysaudio::start_backend(&info.id)
                .with_context(|| format!("start sysaudio backend '{}'", info.id))?;
            if !info.excludes_self {
                warn_feedback_risk(inner, &info.id);
            }
            Src::Sys(SysAudioFrames::new(cap, info.id, info.excludes_self))
        }
        // No bridge = no ring to read, so this would be an accepted stream that
        // is silent forever with nothing saying why. Fail the open instead, the
        // same way an unresolvable sysaudio backend does. A bridge that IS
        // there but has no driver attached is a different thing entirely and
        // succeeds: halbridge answers silence, one full frame per tick.
        SourceSpec::HalSpeaker { slot } => {
            let hal = inner.hal().ok_or_else(|| {
                anyhow!(
                    "the macOS HAL bridge is not available (no LaunchDaemon holding \
                     '{}', or AUDIOHUB_HAL_BRIDGE=off)",
                    crate::halbridge::HAL_SERVICE_NAME
                )
            })?;
            // An app may have been playing into the virtual speaker long before
            // anyone opened a session for it, and only the ring's CONSUMER may
            // move read_idx — so a backlog we do not drop here is not a
            // one-off: producer and consumer then run at the same 480/10ms and
            // the peer hears everything a fixed half second late, forever.
            // Same reasoning (and the same 500ms) as the driver's own flush of
            // mic_ring at handshake.
            let mut stale = Vec::with_capacity(crate::halbridge::HAL_RING_FRAMES as usize);
            let dropped =
                hal.read_spk_mono(*slot, &mut stale, crate::halbridge::HAL_RING_FRAMES as usize);
            if dropped > 0 {
                dlog!(
                    "[audiohubd] hal speaker slot {slot}: dropped {}ms of audio played before \
                     this stream opened",
                    dropped / (crate::halbridge::HAL_SAMPLE_RATE as usize / 1000)
                );
            }
            Src::Frame(Box::new(crate::halbridge::HalSpeakerSource::new(&hal, *slot)))
        }
    })
}

/// Creates a resampler for the new rate that continues from `last`, so a rung
/// switch mid-stream cannot inject the zero sample (audible click) a fresh
/// resampler would interpolate from.
fn seeded_resampler(src_rate: u32, dst_rate: u32, last: f32) -> LinearResampler {
    let mut rs = LinearResampler::new(src_rate, dst_rate);
    let mut discard = Vec::new();
    rs.process(&[last], &mut discard); // primes `last`; output is not audio
    rs
}

fn apply_txcmd(
    inner: &DaemonInner,
    cmd: TxCmd,
    streams: &mut HashMap<u32, TxStream>,
    sources: &mut HashMap<SourceSpec, SourceEnt>,
) {
    match cmd {
        TxCmd::Add { stream_id, key, salt, dest, spec, loss_pct, shared, ack } => {
            let started = match sources.entry(spec.clone()) {
                Entry::Occupied(mut o) => {
                    o.get_mut().refs += 1;
                    Ok(())
                }
                Entry::Vacant(v) => match build_source(inner, &spec) {
                    Ok(src) => {
                        v.insert(SourceEnt {
                            src,
                            refs: 1,
                            frame: Vec::new(),
                            depths: NO_DEPTHS,
                        });
                        Ok(())
                    }
                    Err(e) => {
                        dlog!("[audiohubd] source for stream {stream_id}: {e:#}");
                        Err(format!("{e:#}"))
                    }
                },
            };
            if started.is_ok() {
                streams.insert(
                    stream_id,
                    TxStream {
                        id: stream_id,
                        // real streams are always keyed per stream, never with
                        // the bare connection media key
                        crypto: MediaCrypto::new_for_stream(&key, stream_id, &salt),
                        dest,
                        spec,
                        loss: LossInjector::new(stream_id, loss_pct),
                        seq: 0,
                        rung: 0,
                        rs: None,
                        rs_last: 0.0,
                        staged: Vec::new(),
                        shared,
                    },
                );
            }
            if let Some(a) = ack {
                let _ = a.send(started);
            }
        }
        TxCmd::Remove { stream_id } => {
            if let Some(st) = streams.remove(&stream_id) {
                // 这条流从此不再被 tick 到，槽再也不会被覆盖 —— 但 `TxShared`
                // 还活着且还在被报告线程读。不清就是把最后一次读数永久钉住。
                clear_send_stages(&st);
                if let Some(ent) = sources.get_mut(&st.spec) {
                    ent.refs = ent.refs.saturating_sub(1);
                    if ent.refs == 0 {
                        sources.remove(&st.spec);
                    }
                }
            }
        }
    }
}

/// Closes every stream fed by a source that reported itself dead (the frozen
/// `SysAudioCapture::failed` seam). Without this the capture keeps returning 0
/// samples and the peer receives digital silence forever, with nothing on
/// either side saying why — the reason is logged and the peer gets CloseStream.
fn reap_dead_sources(
    inner: &DaemonInner,
    streams: &mut HashMap<u32, TxStream>,
    sources: &mut HashMap<SourceSpec, SourceEnt>,
) {
    let dead: Vec<(SourceSpec, String)> = sources
        .iter()
        .filter_map(|(spec, ent)| ent.src.failed().map(|why| (spec.clone(), why)))
        .collect();
    for (spec, why) in dead {
        let ids: Vec<u32> = streams
            .values()
            .filter(|s| s.spec == spec)
            .map(|s| s.id)
            .collect();
        for id in ids {
            dlog!(
                "[audiohubd] stream {id}: media source ({}) died: {why}; closing the stream",
                spec.label()
            );
            // queues a TxCmd::Remove we will drain next tick, and tells the peer
            crate::conn::teardown_stream(inner, id, true);
        }
        // drop the corpse now: the queued Remove would only reach it next tick
        streams.retain(|_, s| {
            let keep = s.spec != spec;
            if !keep {
                // 同 TxCmd::Remove：走了就得清槽，否则一段死掉的排队会永远
                // 留在 UI 上，且不带任何「这是陈的」标记。
                clear_send_stages(s);
            }
            keep
        });
        sources.remove(&spec);
    }
}

/// spec-m4c §D: the default input changed, so a live `MicSource` is now bound
/// to the wrong device. Build the replacement BEFORE dropping the old one: if
/// the new default cannot be opened the session keeps its (silent) capture
/// rather than dying, and the reason is on stderr.
fn rebuild_mic_source(inner: &DaemonInner, sources: &mut HashMap<SourceSpec, SourceEnt>) {
    let Some(ent) = sources.get_mut(&SourceSpec::Mic) else {
        dlog!("[audiohubd] default input changed; no microphone source to rebuild");
        return;
    };
    match build_source(inner, &SourceSpec::Mic) {
        Ok(src) => {
            ent.src = src; // old capture dropped here, after the new one exists
            dlog!("[audiohubd] default input changed; microphone source rebuilt");
        }
        Err(e) => dlog!(
            "[audiohubd] default input changed but the new device failed to open ({e:#}); \
             keeping the previous capture"
        ),
    }
}

pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {
    let mut streams: HashMap<u32, TxStream> = HashMap::new();
    let mut sources: HashMap<SourceSpec, SourceEnt> = HashMap::new();
    // Lifted out of the daemon mutex once, here, so the tick itself never
    // touches that lock; the bridge is installed before any thread starts and
    // is never replaced.
    let hal = inner.hal();
    let mut dev_epoch = inner.dev_in_epoch.load(Ordering::Relaxed);
    let start = Instant::now();
    let mut tick: u64 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        {
            let e = inner.dev_in_epoch.load(Ordering::Relaxed);
            if e != dev_epoch {
                dev_epoch = e;
                rebuild_mic_source(&inner, &mut sources);
            }
        }
        // if a stall (device open, scheduler) put us far behind, skip the
        // missed frames instead of bursting them — receiver JBs trim bursts
        // by advancing, which starves them against the steady arrival rate
        let behind = start.elapsed().as_millis() as u64 / FRAME_MS;
        if behind > tick + 10 {
            tick = behind;
        }
        let deadline = start + Duration::from_millis(tick * FRAME_MS);
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match cmds.recv_timeout(deadline - now) {
                Ok(cmd) => apply_txcmd(&inner, cmd, &mut streams, &mut sources),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_txcmd(&inner, cmd, &mut streams, &mut sources);
        }
        // spec-m5b §5.4: a PUBLISHED speaker ring with no session behind it
        // still receives whatever the app that selected it played. Nobody would
        // ever move its read_idx, the ring fills, and the driver's census
        // starts logging "audiohubd has stopped draining it" at error level.
        // Only a ring's consumer may move read_idx, and on this side that is
        // THIS thread — so the drain belongs here, above the idle short-circuit
        // below, because "no streams at all" is exactly the case it exists for.
        if let Some(h) = hal.as_ref() {
            let mut busy = 0u16;
            for spec in sources.keys() {
                if let SourceSpec::HalSpeaker { slot } = spec {
                    busy |= 1u16 << (*slot).min(15);
                }
            }
            h.drain_idle_speakers(busy);
        }
        if streams.is_empty() {
            match cmds.recv_timeout(Duration::from_millis(200)) {
                Ok(cmd) => apply_txcmd(&inner, cmd, &mut streams, &mut sources),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            tick = start.elapsed().as_millis() as u64 / FRAME_MS + 1;
            continue;
        }

        let slow_tick = tick % 100 == 0; // ~1s
        for ent in sources.values_mut() {
            if ent.refs == 0 {
                // 没被取过音频的源，它的深度读数这一 tick 就不成立（`depths()`
                // 的语义是「刚被取走一帧之后还剩多少」）。清掉而不是留着上一轮
                // 的值——留着就是把陈旧读数交给下一条挂上来的流。
                ent.depths = NO_DEPTHS;
                continue;
            }
            if !ent.src.next_frame(&mut ent.frame) {
                ent.frame.clear();
            }
            // 取完这一 tick 的音频之后立刻读深度：这才是「刚被取走 480 个样本
            // 之后还剩多少」的稳态读数，也就是「此刻进来的样本前面排着几个」。
            // 放在 next_frame 之前读会系统性地多出一帧（10 ms）。
            //
            // 接收侧的播放环必须取**同一个相位**：那边是在 `push` 之**前**读
            // （见 `ring_depth_before_push`）。一边谷值一边峰值，差的那一帧会
            // 恒定挂在总数上，而且看起来完全像一个真实缓冲。
            ent.depths = ent.src.depths();
            if ent.frame.len() != F48 {
                // An OVER-long frame means the source appended instead of
                // replacing, and the resize below then re-sends whatever its
                // very first call produced, forever, while the packet counts,
                // the loss rate and the tone probe all stay green. That cost a
                // full debugging session once; it must never be silent again.
                debug_assert!(
                    ent.frame.len() <= F48,
                    "FrameSource yielded {} samples (> {F48}): it appended instead of replacing",
                    ent.frame.len()
                );
                if ent.frame.len() > F48 && slow_tick {
                    dlog!(
                        "[audiohubd] BUG: source yielded {} samples, expected {F48} — \
                         the stream is repeating its first frame",
                        ent.frame.len()
                    );
                }
                ent.frame.resize(F48, 0.0);
            }
            // playback can start long after the capture did, so the plan §5
            // condition is re-evaluated while such a capture is alive
            if slow_tick {
                if let Src::Sys(s) = &ent.src {
                    if !s.excludes_self {
                        warn_feedback_risk(&inner, &s.backend);
                    }
                }
            }
        }
        reap_dead_sources(&inner, &mut streams, &mut sources);
        let ts_us = start.elapsed().as_micros() as u64;
        for st in streams.values_mut() {
            let Some(ent) = sources.get(&st.spec) else {
                // 源已经不在表里了（`reap_dead_sources` 收了尸，或 Remove 把
                // refs 减到 0），而这条流的 `TxShared` 还活着并且仍在被报告线程
                // 读。**这里必须清槽再走**：早先的 `continue` 会把最后一次读数
                // 留在槽里，于是 UI 继续显示一段早已不存在的排队——这正是下面
                // 那句注释要消灭的「静默缺项」，而缺项本身就是从这条捷径漏出去的。
                clear_send_stages(st);
                continue;
            };
            // 发布本流的发送侧分项。只有原子 store，没有除法、没有锁、没有
            // 分配（规格附录约束 3：否则测量会改变被测对象）。
            //
            // 每 tick 都写，包括 `None`：源换过之后（如默认输入设备变化触发的
            // MicSource 重建）若不清槽，报告线程会一直读到已经不存在的那一级。
            // 级 4 `send_pace`（规格 §3.2）：常数 5 ms，由 `publish_send_stages`
            // 一并发射。判据见 `send_pace_for`。这一级此前**在枚举里声明了、在
            // 规格里编了号，却一个发布点都没有** ⇒ 发送侧的 local_ms 系统性短
            // 5 ms，而且没有任何字段标出它缺席。
            publish_send_stages(&st.shared.stages, &ent.depths);
            let want = st.shared.rung.load(Ordering::Relaxed).min(3);
            if want != st.rung {
                st.rung = want;
                let last = st.rs_last;
                st.rs = (want != 0).then(|| seeded_resampler(48000, rung_rate(want), last));
            }
            st.rs_last = ent.frame.last().copied().unwrap_or(st.rs_last);
            let rate = rung_rate(st.rung);
            let samples: &[f32] = match st.rs.as_mut() {
                Some(rs) => {
                    st.staged.clear();
                    rs.process(&ent.frame, &mut st.staged);
                    &st.staged
                }
                None => &ent.frame,
            };
            let seq = st.seq;
            st.seq = st.seq.wrapping_add(1);
            let dropped = st.loss.should_drop(); // advance LCG every frame
            if dropped {
                continue;
            }
            if let Some(a) = *lk(&st.shared.dest_override) {
                if a != st.dest {
                    dlog!("[audiohubd] stream {} dest {} -> {} (keepalive)", st.id, st.dest, a);
                    st.dest = a;
                }
            }
            let payload = dsp::f32_to_s16le(samples);
            let header = Header {
                kind: Kind::Media,
                codec: Codec::PcmS16le,
                channels: 1,
                sample_rate: rate,
                session_id: st.id as u64,
                stream_id: st.id,
                seq,
                timestamp_us: ts_us,
                payload_len: 0, // seal() sets ciphertext length
            };
            match st.crypto.seal(&header, &payload) {
                Ok(dg) => {
                    if inner.udp.send_to(&dg, st.dest).is_ok() {
                        st.shared.sent_packets.fetch_add(1, Ordering::Relaxed);
                        st.shared.sent_bytes.fetch_add(dg.len() as u64, Ordering::Relaxed);
                    }
                }
                Err(e) => dlog!("[audiohubd] media seal stream {}: {e}", st.id),
            }
        }
        tick += 1;
    }
}

// ---------------------------------------------------------------- rx engine

pub(crate) fn rx_loop(inner: Arc<DaemonInner>) {
    let mut buf = [0u8; 2048];
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match inner.udp.recv_from(&mut buf) {
            Ok((n, from)) => handle_datagram(&inner, &buf[..n], from),
            Err(e) if poll_tick(e.kind()) => {}
            Err(e) => {
                dlog!("[audiohubd] udp recv: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_datagram(inner: &DaemonInner, dg: &[u8], from: SocketAddr) {
    let Ok((h, _payload)) = Header::parse(dg) else { return };
    match h.kind {
        Kind::Media => {
            let rx = rd(&inner.rx_table).get(&h.stream_id).cloned();
            let Some(rx) = rx else { return };
            let Ok((h, plain)) = rx.crypto.open(dg) else { return }; // tampered/foreign
            let arrival = inner.start.elapsed().as_micros() as u64;
            let mut jit_ms = 0.0f32;
            {
                let mut c = lk(&rx.stats);
                if c.first.is_none() {
                    c.first = Some(Instant::now());
                }
                c.rx.on_packet(h.seq, h.timestamp_us, arrival, plain.len());
                c.last_rate = h.sample_rate;
                let transit = arrival as i64 - h.timestamp_us as i64;
                if let Some(p) = c.prev_transit {
                    jit_ms = (transit - p).unsigned_abs() as f32 / 1000.0;
                    c.note_jitter(jit_ms); // feeds the per-interval Stats window
                }
                c.prev_transit = Some(transit);
            }
            let raw = dsp::s16le_to_f32(&plain);
            let last_sample = raw.last().copied();
            let mut st = lk(&rx.jbs);
            let frame = if h.sample_rate == 48000 {
                raw
            } else {
                if st.rs_rate != h.sample_rate || st.rs.is_none() {
                    // continue from the last decoded sample: a mid-stream rate
                    // change must not interpolate up from zero
                    st.rs = Some(seeded_resampler(h.sample_rate, 48000, st.rs_last));
                    st.rs_rate = h.sample_rate;
                }
                let mut out = Vec::with_capacity(F48 + 8);
                st.rs.as_mut().unwrap().process(&raw, &mut out);
                out
            };
            if let Some(l) = last_sample {
                st.rs_last = l;
            }
            st.jb.push(h.seq, frame.clone());
            // starvation self-heal: if the JB keeps rejecting arrivals as
            // late while nearly empty (expected seq raced ahead — mixer
            // stall or cross-machine clock drift), restart it cleanly
            if st.jb.dropped > st.last_dropped && st.jb.depth() <= 1 {
                st.late_streak += 1;
            } else {
                st.late_streak = 0;
            }
            st.last_dropped = st.jb.dropped;
            if st.late_streak >= 50 {
                let target = st.jb.target();
                st.jb = audiohub_net::media::JitterBuffer::new(target);
                st.jb.push(h.seq, frame);
                st.last_dropped = 0;
                st.late_streak = 0;
                // 五个 lifetime 计数器随新 JB 归零，这是一次真实的不连续：
                // 旧采样点不能再参与差分，否则窗口值会被 saturating_sub 压成 0，
                // 让一次 resync 看起来像「这 10 秒完美无瑕」。
                st.conceal.reset();
                dlog!("[audiohubd] jb resync on stream {}", h.stream_id);
            }
            st.jit_win.push(jit_ms);
            if st.jit_win.len() > 256 {
                st.jit_win.remove(0);
            }
            st.pushes += 1;
            // Q1 窗口的细分辨率采样点（规格 §4.6：每 10 次 push 一点，≈100 ms）。
            // ticker 每秒还会补一点——那一路才是断流时唯一还在走的，因为**断流
            // 时这里根本不执行**，而断流正是 Q1 最该报警的时候。
            if st.pushes % 10 == 0 {
                st.sample_conceal();
            }
            if st.pushes % 100 == 0 && !st.jit_win.is_empty() {
                let mut v = st.jit_win.clone();
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)] as f64;
                st.jb.update_target(p95, FRAME_MS as f64);
            }
        }
        Kind::PullReq => {
            // Receiver keepalive. spec-m4a §3 freezes the media destination as
            // "control-TCP peer IP + peer daemon port" and keepalives as
            // count-only, so this arm may learn the PORT (the peer store's port
            // goes stale when a daemon moves) but never the IP: a keepalive is
            // an unencrypted 40-byte header carrying a cleartext stream_id, so
            // trusting its source IP lets any host on the path redirect the
            // live media stream to itself. Tier-0 single-side reachability is
            // docs/plan.md §4.3, not spec-m4a §4.3.
            let found = {
                let st = lk(&inner.state);
                st.sessions
                    .get(&h.stream_id)
                    .and_then(|e| e.tx.clone().map(|t| (t, e.conn.media_dest.ip())))
            };
            let Some((t, peer_ip)) = found else { return };
            t.ka_count.fetch_add(1, Ordering::Relaxed);
            if from.ip() != peer_ip {
                t.ka_rejected.fetch_add(1, Ordering::Relaxed);
                if t.first_ka_warning() {
                    dlog!(
                        "[audiohubd] stream {}: keepalive from {} ignored (control peer is {})",
                        h.stream_id,
                        from,
                        peer_ip
                    );
                }
                return;
            }
            let learned = SocketAddr::new(peer_ip, from.port());
            let mut d = lk(&t.dest_override);
            if *d != Some(learned) {
                *d = Some(learned);
            }
        }
        _ => {}
    }
}

/// Receiver-side keepalive (spec §3): one unencrypted PullReq per stream per
/// second toward the sender to hold NAT/firewall state.
pub(crate) fn send_pullreq(inner: &DaemonInner, rx: &RxStream) {
    let h = Header {
        kind: Kind::PullReq,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: 48000,
        session_id: rx.stream_id as u64,
        stream_id: rx.stream_id,
        seq: rx.ka_seq.fetch_add(1, Ordering::Relaxed),
        timestamp_us: inner.start.elapsed().as_micros() as u64,
        payload_len: 0,
    };
    let _ = inner.udp.send_to(&h.encode(&[]), rx.ka_dest);
}

// ---------------------------------------------------------------- mixer

/// Frozen clip curve: linear to 0.8, tanh-compressed knee above.
fn soft_clip(s: f32) -> f32 {
    let a = s.abs();
    if a <= 0.8 {
        s
    } else {
        (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()).copysign(s)
    }
}

/// Appends post-clip mixer output to the 2s ring used by mix_verdicts.
fn push_mix(inner: &DaemonInner, samples: &[f32]) {
    let mut r = lk(&inner.mix_ring);
    r.extend(samples.iter().copied());
    if r.len() > RING_CAP {
        let d = r.len() - RING_CAP;
        r.drain(..d);
    }
}

/// 一个 `AudioTx` 播放环此刻的深度（级 8 `play_ring` / 级 8′ `bridge_ring`）。
///
/// ## ⚠ 采样相位：必须在 `push()` **之前**调用
///
/// 被测量是「此刻交进这一级的样本还要排多久」。`push` 之前的 `queued()` 恰好是
/// **排在这一帧前面**的样本数，也就是这一帧的驻留时间。`push` 之后读到的是它
/// **+ 480**，恒定多算一整帧 ≈ 10 ms —— 刚推进去的 480 个样本不用等自己。
///
/// 这也是与源侧的相位对齐：源侧三级都在 `next_frame()` **之后**读，读到的同样是
/// 「此刻进来的样本前面排着几个」。一边取谷值、一边取峰值，差的那 10 ms 会一直
/// 挂在总数上，而且因为它恒定，看起来完全像一个真实的缓冲。
///
/// 速率与容量都取自 `AudioTx` 自己的**设备**速率，不是 48000：环容量恰好等于
/// `dev_rate`（1.000 秒），拿 48000 去除一个 44.1k 设备的读数会静默偏 −8.8%。
///
/// 丢弃方向是 `Newest`——`push_slice` 满了就短写，新采样根本没进环。这与三个
/// 源侧 FIFO 的「丢最旧」在深度上完全简并，只有这个标签能把它们分开
/// （规格 §0.2）。
pub(crate) fn ring_depth_before_push(id: StageId, tx: &AudioTx) -> StageDepth {
    StageDepth {
        id,
        samples: tx.queued(),
        capacity: tx.capacity(),
        rate: tx.dev_rate(),
        dropped: Some(tx.dropped()),
        drop_mode: DropMode::Newest,
    }
}

/// 发布播放环深度（规格 §3.2 的级 8 `play_ring`）。
///
/// 取 `&StageSlot` 而不是 `&DaemonInner`：这一级的全部接线决策（哪个 getter
/// 进哪个字段、丢弃方向标什么）都在这几行里，而 `DaemonInner` 要一个 UDP
/// socket、一堆线程通道和一个真实设备才造得出来——那会把它们永久挡在测试
/// 之外。调用方传 `&inner.play_ring`。
pub(crate) fn publish_play_ring(slot: &StageSlot, tx: &AudioTx) {
    slot.store(Some(ring_depth_before_push(StageId::PlayRing, tx)));
}

/// Drops the mix history when nothing feeds the mixer. The ring is a rolling
/// window read by mix_verdicts, and the idle path advances it far slower than
/// real time, so rolling silence through it would keep a stopped tone testing
/// as present for seconds. No spk stream means no mix output at all.
fn clear_mix(inner: &DaemonInner) {
    let mut r = lk(&inner.mix_ring);
    if !r.is_empty() {
        r.clear();
    }
}

/// One bridge target: a NAMED output device fed by every mic-recv stream that
/// asked for it (spec-m4c §B). Ref-counted so two sessions bridging to the same
/// card share one device stream.
struct BridgeOut {
    _pb: LivePlayback,
    tx: AudioTx,
    refs: usize,
    buf: [f32; F48],
    /// 本 tick **推之前**读到的环深度（级 8′ `bridge_ring`）。
    ///
    /// 存在这里而不是当场发布，是因为发布要按**流**做（一个桥可被多条流引用），
    /// 而深度是按**桥**读的一份。先在推的循环里读好、再在第二趟里广播给引用它
    /// 的每条流——顺序反过来就只能在推之后读，那恒定多算一整帧（见
    /// `ring_depth_before_push`）。
    depth: Option<StageDepth>,
}

fn apply_mixcmd(cmd: MixCmd, bridges: &mut HashMap<String, BridgeOut>) {
    match cmd {
        MixCmd::OpenBridge { device, claim, ack } => {
            // Open first, commit second: cpal can sit here for seconds, which
            // is exactly when the opener's ack deadline expires. Whatever is
            // built before the claim is lost costs nothing to drop.
            let opened = if bridges.contains_key(&device) {
                Ok(None) // already open: this is only a new reference
            } else {
                LivePlayback::start_on(&device, 48000)
                    .map(|(pb, tx)| {
                        Some(BridgeOut { _pb: pb, tx, refs: 0, buf: [0.0; F48], depth: None })
                    })
                    .map_err(|e| format!("open bridge device '{device}': {e:#}"))
            };
            let r = match opened {
                // a failed open holds nothing, so it never claims: the opener
                // must stay free to give up without releasing someone else's
                Err(e) => Err(e),
                Ok(fresh) => {
                    if claim.swap(true, Ordering::SeqCst) {
                        return; // opener gave up: hold nothing, `fresh` drops here
                    }
                    if let Some(b) = fresh {
                        dlog!("[audiohubd] bridge output '{device}' opened");
                        bridges.insert(device.clone(), b);
                    }
                    if let Some(b) = bridges.get_mut(&device) {
                        b.refs += 1;
                    }
                    Ok(())
                }
            };
            let _ = ack.send(r);
        }
        MixCmd::ReleaseBridge { device } => {
            if let Some(b) = bridges.get_mut(&device) {
                b.refs = b.refs.saturating_sub(1);
                if b.refs == 0 {
                    bridges.remove(&device);
                    dlog!("[audiohubd] bridge output '{device}' closed");
                }
            }
        }
    }
}

pub(crate) fn mixer_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<MixCmd>) {
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut playback: Option<(LivePlayback, AudioTx)> = None;
    let mut pb_fail_at: Option<Instant> = None;
    let mut bridges: HashMap<String, BridgeOut> = HashMap::new();
    let mut dev_epoch = inner.dev_out_epoch.load(Ordering::Relaxed);
    let mut mix = [0.0f32; F48];
    let mut mon = [0.0f32; F48];
    let mut frame = [0.0f32; F48];
    // spec-m5b §5.4 microphone direction. Lifted out of the daemon mutex once,
    // here, so the tick itself never touches that lock; the bridge is installed
    // before any thread starts and is never replaced.
    let hal = inner.hal();
    // ONE BUCKET PER SLOT, not one shared buffer.
    //
    // The version this replaces summed every `hal` stream into a single `hal_buf`
    // and wrote it into the one mic ring. With two peers bound that is a mixer,
    // not a router: whoever recorded peer A's virtual microphone got peer B's
    // audio too — and every positive test still passed, because A's audio was
    // in there as well. `dirty` keeps the clearing cost proportional to the
    // buckets actually used rather than to 16 * 480 floats per 10ms tick.
    let mut hal_bufs = vec![[0.0f32; F48]; crate::haldev::HAL_MAX_SLOTS];
    let mut hal_dirty: u16 = 0;
    // 重复流判据（规格 §4.6）：把**第一个**送进本机输出的 frame 拷进暂存，
    // 与**第二个**做零延迟归一化互相关。零延迟即可——重复流是同一份解码结果
    // 分两条会话进来，样本级已经对齐。480 点点积 ≈ 1.4k flops / 10 ms。
    let mut corr_a = [0.0f32; F48];
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_mixcmd(cmd, &mut bridges);
        }
        {
            // spec-m4c §D: the default output moved, so this stream now plays
            // into the old device. Drop it and let the code below re-open on
            // the new default; one frame of silence, no session teardown.
            // Bridges name their device explicitly and are left alone.
            let e = inner.dev_out_epoch.load(Ordering::Relaxed);
            if e != dev_epoch {
                dev_epoch = e;
                dlog!("[audiohubd] default output changed; rebuilding the playback stream");
                playback = None;
                pb_fail_at = None; // retry now, not after the 10s backoff
            }
        }
        // never replay missed ticks (see tx_loop): each replayed tick is an
        // extra pop that races the JB expected-seq ahead of real arrivals
        let behind = start.elapsed().as_millis() as u64 / FRAME_MS;
        if behind > tick + 10 {
            tick = behind;
        }
        let deadline = start + Duration::from_millis(tick * FRAME_MS);
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
        let streams: Vec<Arc<RxStream>> = rd(&inner.rx_table).values().cloned().collect();
        if streams.is_empty() {
            // an open bridge keeps being written to even before its stream's
            // first frame arrives: a virtual card that is never written to may
            // not spin up its IO cycle at all, and the first real audio would
            // then be swallowed by the warm-up
            for b in bridges.values_mut() {
                b.buf.fill(0.0);
                let silence = b.buf;
                b.depth = Some(ring_depth_before_push(StageId::BridgeRing, &b.tx));
                b.tx.push(&silence);
            }
            clear_mix(inner.as_ref()); // never serve stale mix audio
            // 没有任何流 = 没有这一级。清槽，否则报告线程会一直读到最后一次的
            // 陈旧深度——那是「静默缺项」的另一种形态。
            inner.play_ring.store(None);
            std::thread::sleep(Duration::from_millis(20));
            tick = start.elapsed().as_millis() as u64 / FRAME_MS + 1;
            continue;
        }
        mix.fill(0.0);
        mon.fill(0.0);
        for slot in 0..crate::haldev::HAL_MAX_SLOTS {
            if hal_dirty & (1 << slot) != 0 {
                hal_bufs[slot].fill(0.0);
            }
        }
        hal_dirty = 0;
        for b in bridges.values_mut() {
            b.buf.fill(0.0);
        }
        let mut any_spk = false;
        let mut any_mon = false;
        // 本 tick 有多少路真的落到本机输出上，以及前两路的相关度。
        let now_ms = inner.start.elapsed().as_millis() as u64;
        let mut contrib: u32 = 0;
        let mut corr: Option<f64> = None;
        for s in &streams {
            let popped = lk(&s.jbs).jb.pop();
            lk(&s.post).advance(popped, &mut frame);
            // Q2 的可归属那一半（规格 §4.6）：测点在 advance 之后、加进任何
            // 目的地之前。这回答的是「我这一路送进来多响」，是**求和前**的量，
            // 与站点级的求和后削顶是两个不同的问题。
            s.clip.feed(now_ms, &frame);
            if let Some(ring) = s.ring.as_ref() {
                let mut r = lk(ring);
                r.extend(frame.iter().copied());
                if r.len() > RING_CAP {
                    let d = r.len() - RING_CAP;
                    r.drain(..d);
                }
            }
            // the bridge is a third destination, not an alternative to monitor:
            // one decoded frame may feed the virtual card AND the local output
            if let Some(name) = s.bridge.as_ref() {
                if let Some(b) = bridges.get_mut(name) {
                    for i in 0..F48 {
                        b.buf[i] += frame[i];
                    }
                }
            }
            // ...and the virtual microphone is a fourth one: monitor, bridge
            // and hal are independent destinations for the SAME decode
            // (spec-m5b §5.4). The bucket is chosen by the PEER's slot, so two
            // peers' audio can never meet.
            add_to_hal_bucket(s.hal_slot, &frame, &mut hal_bufs, &mut hal_dirty);
            if s.is_spk || s.monitor {
                // 送本机真实输出的那一集合：`out = soft_clip(mix + mon)`。
                // 站点级削顶正是在这里发生的，所以重复流判据也只看这一集合。
                contrib += 1;
                if contrib == 1 {
                    corr_a.copy_from_slice(&frame);
                } else if contrib == 2 {
                    corr = crate::quality::correlation(&corr_a, &frame);
                }
            }
            if s.is_spk {
                any_spk = true;
                for i in 0..F48 {
                    mix[i] += frame[i];
                }
            } else if s.monitor {
                any_mon = true;
                for i in 0..F48 {
                    mon[i] += frame[i];
                }
            }
        }
        inner.mix_meter.feed(now_ms, contrib, corr);
        for b in bridges.values_mut() {
            // 站点级削顶计入点 1/3：桥接到第三方虚拟声卡（规格 §4.6）。
            // 喂的是**削顶之前**的 buf——削顶之后再量就永远量不到越界。
            inner.mix_clip.feed(now_ms, &b.buf);
            let out: Vec<f32> = b.buf.iter().map(|&v| soft_clip(v)).collect();
            // 级 8′ `bridge_ring`：桥接流的尾级。**推之前**读（见
            // `ring_depth_before_push`）。这一整秒的环此前完全没有建模——桥接流
            // 的 `local_ms` 只有 jitter_buf + post_mix，静默漏掉它。
            b.depth = Some(ring_depth_before_push(StageId::BridgeRing, &b.tx));
            b.tx.push(&out);
        }
        // Exactly 480 mono samples per 10ms tick per slot = each ring's 48k
        // rate. Only into slots a session asked for AND an application is
        // actually reading: writing into a ring nobody drains would do nothing
        // but run that slot's mic_dropped up. The write is a lock-free SPSC
        // index bump, safe to do on this loop.
        // 级 8″ `hal_mic` 的本 tick 读数，按槽存一份（一个槽可被多条流写，
        // 深度只有一份 —— 与 `bridge_ring` 同理）。全 `None` 起手：没写的槽
        // 这一 tick 就没有这一级。
        let mut hal_mic_depth: [Option<StageDepth>; crate::haldev::HAL_MAX_SLOTS] =
            [None; crate::haldev::HAL_MAX_SLOTS];
        if hal_dirty != 0 {
            if let Some(h) = hal.as_ref() {
                let mut out = [0.0f32; F48];
                for slot in 0..crate::haldev::HAL_MAX_SLOTS {
                    if hal_dirty & (1 << slot) == 0
                        || !inner.hal_mic_io[slot].load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    // 站点级削顶计入点 2/3：写进某个对端的虚拟麦克风。
                    inner.mix_clip.feed(now_ms, &hal_bufs[slot]);
                    for i in 0..F48 {
                        out[i] = soft_clip(hal_bufs[slot][i]);
                    }
                    // 级 8″：模式 B 虚拟麦克风环（500 ms）。同样**写之前**读——
                    // 读到的是「驱动还没取走的积压」，正是这一帧要等的排队量。
                    // 这一级此前也完全没有建模：模式 B 的接收流上报的
                    // `local_ms` 只有 jitter_buf + post_mix。
                    hal_mic_depth[slot] = h.mic_depth(slot as u8);
                    h.write_mic_mono(slot as u8, &out);
                }
            }
        }
        if any_spk {
            // ⚠ 这个 soft_clip **不计入**站点级削顶统计（规格 §0.6）：
            // `mix_ring` 是 probe 的旁路 tap，不在送扬声器的路径上。把它算进去
            // 会让每一路 spk 流的削顶被重复计数一次，凭空虚增一倍。
            let clipped: Vec<f32> = mix.iter().map(|&v| soft_clip(v)).collect();
            push_mix(inner.as_ref(), &clipped);
        } else {
            clear_mix(inner.as_ref());
        }
        // 本 tick 到底有没有一个活的播放环。没有就得清槽（设备打不开、或压根
        // 没有流送本机输出），不能留着上一次的读数。
        let mut have_play_ring = false;
        if any_spk || any_mon {
            if playback.is_none()
                && pb_fail_at.map_or(true, |t| t.elapsed() > Duration::from_secs(10))
            {
                match LivePlayback::start(48000) {
                    Ok(p) => playback = Some(p),
                    Err(e) => {
                        dlog!("[audiohubd] playback unavailable: {e:#}");
                        pb_fail_at = Some(Instant::now());
                    }
                }
            }
            if let Some((_, tx)) = playback.as_mut() {
                let mut out = [0.0f32; F48];
                for i in 0..F48 {
                    out[i] = mix[i] + mon[i];
                }
                // 站点级削顶计入点 3/3：真实默认输出。这是最重要的一个——
                // 「两路重复流相加」的破音就出现在这里。同样喂削顶**之前**的和。
                inner.mix_clip.feed(now_ms, &out);
                for o in out.iter_mut() {
                    *o = soft_clip(*o);
                }
                // 播放环深度（规格 §3.2 的级 8）。**`push` 之前**读：读到的是
                // 排在这一帧前面的样本数，也就是这一帧的驻留时间。之前这里是
                // push 之后读，恒定多算一整帧 ≈ 10 ms（刚推进去的 480 个样本
                // 不用等自己），而且因为恒定，看起来完全像一个真实的缓冲。
                publish_play_ring(&inner.play_ring, tx);
                tx.push(&out);
                have_play_ring = true;
            }
        }
        if !have_play_ring {
            inner.play_ring.store(None);
        }
        // 每条流的两条**并行**尾级（桥接虚拟声卡 / 虚拟麦克风）。每 tick 都写，
        // 包括 `None`：桥关掉、槽解绑之后若不清槽，报告线程会一直读到最后一次的
        // 陈旧深度 —— 与发送侧同一条纪律。
        //
        // 并行而非串联：一帧解码结果会被**同时**送进真实输出 / 桥 / 虚拟麦克风，
        // 求和会报出双倍延迟，所以 `sum_stage_ms` 对尾级取 max（见
        // `StageId::is_output_tail`）。
        for s in &streams {
            s.bridge_ring.store(
                s.bridge
                    .as_ref()
                    .and_then(|n| bridges.get(n))
                    .and_then(|b| b.depth),
            );
            s.hal_mic
                .store(s.hal_slot.and_then(|slot| hal_mic_depth.get(slot as usize).copied().flatten()));
        }
        tick += 1;
    }
}

/// Routes ONE decoded frame into the bucket of the peer that owns it.
///
/// Extracted so the rule can be tested without a driver, because it is the rule
/// the previous implementation did not have: every `hal` stream was summed into
/// a single buffer and written to a single ring, so with two peers bound,
/// whoever recorded peer A's virtual microphone also got peer B. Every positive
/// test still passed — A's audio WAS in there.
fn add_to_hal_bucket(
    hal_slot: Option<u8>,
    frame: &[f32; F48],
    bufs: &mut [[f32; F48]],
    dirty: &mut u16,
) {
    let Some(slot) = hal_slot else { return };
    let slot = slot as usize;
    if slot >= bufs.len() {
        return;
    }
    *dirty |= 1 << slot;
    for i in 0..F48 {
        bufs[slot][i] += frame[i];
    }
}

/// Presence verdict for one frequency on the summed mixer output. Plain
/// verify_tone can't apply here: concurrent probe tones are signal, not
/// noise, so detection keys on absolute Goertzel power (median of 100ms
/// windows); snr_db is still reported for diagnostics.
pub(crate) fn mix_tone_verdict(samples: &[f32], rate: u32, freq: f32) -> ToneVerdict {
    let win = (rate / 10) as usize;
    let skip = (rate / 5) as usize;
    if win == 0 || samples.len() < skip + win {
        return ToneVerdict {
            freq_hz: freq,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    let mut powers: Vec<f32> = Vec::new();
    let mut snrs: Vec<f32> = Vec::new();
    let mut analyzed = 0usize;
    for chunk in samples[skip..].chunks(win) {
        if chunk.len() < win {
            break;
        }
        analyzed += chunk.len();
        let p = dsp::goertzel_power(chunk, rate, freq) as f64;
        let total: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
            / chunk.len() as f64
            / 2.0;
        let noise = (total - p).max(0.0) + 1e-12;
        powers.push(p as f32);
        snrs.push((10.0 * (p.max(1e-12) / noise).log10()) as f32);
    }
    if powers.is_empty() {
        return ToneVerdict {
            freq_hz: freq,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    powers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    snrs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p_med = powers[powers.len() / 2];
    ToneVerdict {
        freq_hz: freq,
        snr_db: snrs[snrs.len() / 2],
        // amp-0.5 tone lands at ~0.0625; PLC decay and clipping keep a live
        // tone well above this floor while silence/noise stays far below
        detected: p_med > 1e-4,
        samples_analyzed: analyzed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two peers' virtual microphones are two rings, and one decoded frame
    /// belongs to exactly one of them.
    ///
    /// This is regression N2 in miniature: with a single shared buffer (what
    /// this code did before spec-m5b §5.4), capturing peer A's virtual
    /// microphone yielded A's audio AND B's — inaudible as a bug in any test
    /// that only checks "did A arrive", and a privacy defect in the field.
    #[test]
    fn each_peers_audio_lands_only_in_its_own_bucket() {
        let n = crate::haldev::HAL_MAX_SLOTS;
        let mut bufs = vec![[0.0f32; F48]; n];
        let mut dirty = 0u16;

        add_to_hal_bucket(Some(0), &[0.25; F48], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(3), &[0.75; F48], &mut bufs, &mut dirty);

        assert_eq!(dirty, 0b1001, "exactly the two slots written are dirty");
        assert!(bufs[0].iter().all(|&v| v == 0.25), "slot 0 must carry only its own peer");
        assert!(bufs[3].iter().all(|&v| v == 0.75), "slot 3 must carry only its own peer");
        for (i, b) in bufs.iter().enumerate() {
            if i != 0 && i != 3 {
                assert!(b.iter().all(|&v| v == 0.0), "slot {i} was written to by nobody");
            }
        }
    }

    #[test]
    fn two_streams_on_the_same_slot_still_mix() {
        // The bucket is per DEVICE, not per stream: two sessions feeding one
        // peer's virtual microphone are a mix, which is the provider-side
        // fan-in plan §1 asks for.
        let mut bufs = vec![[0.0f32; F48]; 4];
        let mut dirty = 0u16;
        add_to_hal_bucket(Some(1), &[0.25; F48], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(1), &[0.25; F48], &mut bufs, &mut dirty);
        assert!(bufs[1].iter().all(|&v| v == 0.5));
        assert_eq!(dirty, 0b10);
    }

    #[test]
    fn a_stream_bound_to_no_device_touches_nothing() {
        let mut bufs = vec![[0.0f32; F48]; 4];
        let mut dirty = 0u16;
        add_to_hal_bucket(None, &[1.0; F48], &mut bufs, &mut dirty);
        // ...and neither does one naming a slot this driver does not have.
        add_to_hal_bucket(Some(200), &[1.0; F48], &mut bufs, &mut dirty);
        assert_eq!(dirty, 0);
        assert!(bufs.iter().all(|b| b.iter().all(|&v| v == 0.0)));
    }

    // ------------------------------------------- SysAudioFrames::depths()
    //
    // 这个源的 `depths()` 此前零覆盖。它是三个「1 秒源侧 FIFO」之一，而三个
    // FIFO 的丢弃方向（`Oldest`）与播放环的（`Newest`）在深度读数上完全简并
    // ——标错标签，遥测就只能说「有一秒卡在某处」，说不出那一秒是怎么卡的
    // （规格 §0.2）。所以下面真的跑 `next_frame()` 把 FIFO 灌到饱和，再断言
    // `depths()` 报出来的东西。

    /// 站在系统音频后端的位置上：按固定块交出**单调递增**的样本，好让「剩下的
    /// 是早的还是晚的」——也就是丢弃方向——看得出来。
    struct FakeSysCap {
        rate: u32,
        chunk: usize,
        n: u32,
    }

    impl SysAudioCapture for FakeSysCap {
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

    fn sys_frames(rate: u32, chunk: usize) -> SysAudioFrames {
        SysAudioFrames::new(
            Box::new(FakeSysCap { rate, chunk, n: 0 }),
            "fake".to_string(),
            true,
        )
    }

    /// 空 FIFO 也要报这一级：0 样本 ≠ 「这一级不存在」。后者是 `None`
    /// （`ToneSource` 那种即时合成的源），两者在 UI 上是两句不同的话。
    #[test]
    fn a_sysaudio_source_reports_one_send_fifo_stage_even_when_empty() {
        let src = sys_frames(48_000, 480);
        let [first, second] = src.depths();
        let d = first.expect("发送 FIFO 这一级必须存在");
        assert_eq!(d.id, StageId::SrcFifo);
        assert_eq!(d.samples, 0);
        assert_eq!(d.capacity, 48_000, "1 秒 @48k");
        assert_eq!(d.rate, 48_000, "FIFO 在重采样之后，恒为 48k");
        assert_eq!(d.dropped, Some(0), "本进程数得出来，0 是真读数");
        assert_eq!(d.drop_mode, DropMode::Oldest);
        assert_eq!(d.ms(), Some(0.0));
        assert!(
            second.is_none(),
            "后端自己的内部缓冲从这里读不到 —— 不报，而不是报 0（规格 §7.2 R11）"
        );
    }

    /// 灌爆 1 秒上限：深度贴顶、丢弃方向是**最旧**、计数对得上、ms 按 48k 换算。
    #[test]
    fn a_sysaudio_send_fifo_saturates_at_one_second_and_drops_the_oldest() {
        let mut src = sys_frames(48_000, 5_000); // 每 tick 收 5000、放 480
        let mut out = Vec::new();
        for _ in 0..20 {
            src.next_frame(&mut out);
        }
        let d = src.depths()[0].expect("发送 FIFO 这一级");
        // 修剪到 CAP=48000 后本 tick 又被取走 480。
        assert_eq!(d.samples, 47_520);
        assert!(d.saturated());
        assert_eq!(d.ms(), Some(990.0), "1 秒 FIFO 灌满 ≈ 990 ms 驻留");
        assert_eq!(d.drop_mode, DropMode::Oldest);
        assert_eq!(
            d.dropped,
            Some(20 * 5_000 - 20 * 480 - 47_520),
            "收进来的 − 放出去的 − 还压着的 = 丢掉的"
        );
        // 丢的确实是最旧的：源交的是 1,2,3,…，留在 FIFO 里的必须是尾部。
        src.next_frame(&mut out);
        assert!(
            out[0] > 50_000.0,
            "留下的必须是晚到的样本，got {} —— 丢弃方向反了",
            out[0]
        );
    }

    // ------------------------------------------------------------- 注入 B
    //
    // 规格 §6.3 注入 B：**稳态速率失配**（生产者比消费者快 1%）。
    //
    // 这是 §0.7 两种病理里的第二种：`tx_loop` 按 `Instant` 固定节拍每 tick 取走
    // 恰好 480 个样本，而生产者跑在**设备时钟**上。两个时钟只要有稳态速率差，
    // 这一级就**必然**单调涨到饱和，之后永远丢下去。它与「一次卡顿灌满」的深度
    // 读数完全相同（都贴着容量），修法却完全不同——所以必须靠 `drift_sps`
    // （饱和之前）与 `dropped` 是否还在增长（饱和之后）区分。
    //
    // 用真的 `SysAudioFrames`（真 FIFO、真重采样器、真 `next_frame`）跑完整整
    // 96 秒的模拟时间，喂真的 `DriftTracker`，不造任何字面量。
    #[test]
    fn injection_b_a_steady_rate_mismatch_climbs_then_keeps_dropping() {
        use audiohub_core::latency::DriftTracker;

        // 每 tick 交 485、取走 480 ⇒ +5 样本/tick = **+500 样本/秒**（约 1%）。
        let mut src = sys_frames(48_000, 485);
        let mut out = Vec::new();
        let mut drift = DriftTracker::new();

        // ---- 阶段一：还没饱和，斜率必须把「正在走向饱和」说出来 ----
        // 30 秒 = 3000 tick ⇒ 深度约 15000 样本（312 ms），离 48000 还远。
        for sec in 0..=30 {
            for _ in 0..100 {
                src.next_frame(&mut out);
            }
            let d = src.depths()[0].expect("这一级一直在");
            drift.push(sec as f32, d.id, d.samples);
        }
        let mid = src.depths()[0].unwrap();
        assert!(!mid.saturated(), "此刻还没饱和, got {} 样本", mid.samples);
        assert_eq!(mid.dropped, Some(0), "还没开始丢 —— 深度在涨，但一个样本都没丢");
        let slope = drift.slope(StageId::SrcFifo).expect("30 秒 31 个点，够算斜率");
        assert!(
            (slope - 500.0).abs() < 5.0,
            "1% 失配 = +500 样本/秒，遥测必须在**饱和之前**就说出来, got {slope}"
        );
        assert!(
            mid.ms().unwrap() > 250.0,
            "已经积到 250 ms 以上了, got {:?}",
            mid.ms()
        );

        // ---- 阶段二：跑到饱和之后，丢弃**持续增长** ----
        //
        // 深度 48000 / 500 每秒 ⇒ 第 96 秒才真正装满。注意 `saturated()` 的判据
        // 是 ≥95% 容量，也就是第 91 秒就为真，**而那时一个样本都还没丢**
        // ——「贴顶」与「开始丢」不是同一件事，差着 5 秒。所以取样窗口开在
        // 第 120 秒之后，那里已经是纯稳态。
        let mut dropped_seen = Vec::new();
        for sec in 31..=180 {
            for _ in 0..100 {
                src.next_frame(&mut out);
            }
            let d = src.depths()[0].unwrap();
            drift.push(sec as f32, d.id, d.samples);
            if sec >= 120 {
                dropped_seen.push(d.dropped.expect("源侧 FIFO 的丢弃是可观测的"));
            }
        }
        let d = src.depths()[0].unwrap();
        assert!(d.saturated(), "1% 失配跑够久必然贴顶, got {} 样本", d.samples);
        assert_eq!(d.samples, 47_520, "修剪到 48000 后本 tick 又被取走一帧");
        assert_eq!(d.ms(), Some(990.0), "这就是用户听到的那将近一秒");
        assert_eq!(d.drop_mode, DropMode::Oldest, "丢最旧 ⇒ 恒定迟到但**连续**，不断续");
        assert!(dropped_seen.len() >= 10, "饱和后采到了足够多的点");
        assert!(
            dropped_seen.windows(2).all(|w| w[1] > w[0]),
            "**丢弃必须一直在涨** —— 这是「稳态速率失配」区别于「被一次卡顿灌满」的唯一判据（规格 §3.3）"
        );
        // 每秒丢掉的正是那 1%：500 样本/秒。
        let per_sec = (dropped_seen.last().unwrap() - dropped_seen.first().unwrap()) as f64
            / (dropped_seen.len() - 1) as f64;
        assert!(
            (per_sec - 500.0).abs() < 5.0,
            "稳态每秒丢掉的样本数应等于失配量 500, got {per_sec}"
        );
        // 饱和之后深度不再动 ⇒ 斜率归零。**只看斜率会以为一切正常**，
        // 必须与 `dropped` 一起读才能得出「正在持续丢」的结论。
        let late = drift.slope(StageId::SrcFifo).expect("有斜率");
        assert!(
            late.abs() < 1.0,
            "饱和后深度封顶，斜率必然回到 0, got {late} —— 这正是 dropped 不可或缺的理由"
        );
    }

    /// 后端跑 44.1k 时这一级**仍然**按 48000 换算（它在重采样之后）。
    /// 与采集环那一级（走设备速率）恰好相反，写反任一个都静默偏 ±8.8%。
    #[test]
    fn a_sysaudio_send_fifo_converts_at_48k_whatever_the_backend_rate() {
        let mut src = sys_frames(44_100, 4_410); // 100 ms @44.1k / tick
        let mut out = Vec::new();
        src.next_frame(&mut out);
        let d = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(d.rate, 48_000);
        let ms = d.ms().expect("rate 非 0");
        assert!((ms - 90.0).abs() < 2.0, "100 ms 进、10 ms 出 ⇒ 约 90 ms，got {ms:.2}");
    }

    // ------------------------------------------------- 站点级削顶的计入点
    //
    // 三个计入点（bridge / 虚拟麦克风 / 真实输出）都在 `mixer_loop` 的 10 ms
    // 循环里，而那个循环要一个完整的 `DaemonInner`（UDP socket + 三条线程通道
    // + 真实设备）才跑得起来，单元测试构造不出来。所以这一条退到源码层面清点
    // 调用点——它仍然会在**多一个** feed 出现的那一刻变红，而那正是规格 §0.6
    // 唯一要防的事。

    /// probe 的 `mix_ring` tap **不计入**站点级削顶（规格 §0.6）。
    ///
    /// 它是旁路 tap，不在送扬声器的路径上；把它算进去会让每一路 spk 流的削顶被
    /// 重复计一次，`clip_ratio` 凭空翻倍，而「两路重复流把声音削烂」正是靠这个
    /// 比率抓的——虚增一倍就等于把判据本身毁掉。
    #[test]
    fn the_probe_tap_is_not_counted_in_site_clipping() {
        // 拆开写，免得这条断言自己被自己数进去。
        let needle = concat!("mix_clip", ".feed(");
        let src = include_str!("engine.rs");
        let n = src.matches(needle).count();
        assert_eq!(
            n, 3,
            "站点级削顶恰好三个计入点：bridge / 虚拟麦克风 / 真实输出。\
             多出来的那个八成是 push_mix 那条 probe 旁路（规格 §0.6 明确排除）"
        );
        // ...而且那三个都不在 `any_spk` 的 probe 分支里。
        let probe = src
            .split("if any_spk {")
            .nth(1)
            .expect("mixer_loop 里的 probe 分支");
        let probe = probe.split("clear_mix(").next().unwrap();
        assert!(
            probe.contains("push_mix("),
            "定位到的应该是 push_mix 那个分支"
        );
        assert!(
            !probe.contains(needle),
            "probe 旁路里出现了站点级削顶计入 —— 每一路 spk 流会被重复计一次"
        );
    }

    /// 上一条守的是「不能多喂一次」，这一条说明**为什么**：同一帧喂两次，
    /// 站点级窗口的分母和越界数一起翻倍，`clip_ratio` 却纹丝不动——所以光看
    /// 比率发现不了，只能靠计入点本身守住。而峰值与样本总数是会变的。
    #[test]
    fn feeding_one_frame_twice_doubles_the_site_window() {
        let once = crate::quality::ClipMeter::new();
        let twice = crate::quality::ClipMeter::new();
        let loud = [0.9f32; F48]; // 0.9 > 0.8 阈值 ⇒ 每个样本都算越界
        for t in 0..10u64 {
            let ms = 1_000 + t * 1_000;
            once.feed(ms, &loud);
            twice.feed(ms, &loud);
            twice.feed(ms, &loud); // 多喂的那一次
        }
        // 空帧只推时间、不加样本，用它干净地把两边各翻一页。
        once.feed(11_500, &[]);
        twice.feed(11_500, &[]);

        let a = once.window().expect("整页可读");
        let b = twice.window().expect("整页可读");
        assert_eq!(b.samples, a.samples * 2, "分母被凭空放大一倍");
        assert_eq!(b.over, a.over * 2);
        assert!(
            (b.ratio() - a.ratio()).abs() < 1e-12,
            "而**比率一模一样** —— 所以光盯着 clip_ratio 是发现不了重复计数的，\
             只能靠计入点本身守住"
        );
    }

    // ------------------------------------------------- 级 4 `send_pace`

    /// 有排队的源必须报节拍那一级；即时合成的源必须**不**报。
    ///
    /// 这一级过去在 `StageId` 里声明、在规格 §3.2 里编号，**全仓库零发布点**：
    /// 发送侧 `local_ms` 因此系统性短 5 ms，且没有任何字段说它缺席。
    #[test]
    fn send_pace_is_emitted_for_queued_sources_only() {
        let fifo = StageDepth::new(StageId::SrcFifo, 480, 48_000, 48_000, DropMode::Oldest);
        let p = send_pace_for(&[Some(fifo), None]).expect("有队列的源必须报节拍");
        assert_eq!(p.id, StageId::SendPace);
        assert_eq!(p.ms(), Some(5.0), "半个 tick 的期望值");

        // 采集环 + 发送 FIFO 两级齐全时也只加**一次** 5 ms：节拍是调度器的一级，
        // 不是每个队列各来一份。
        let cap = StageDepth::new(StageId::CapRing, 960, 96_000, 48_000, DropMode::Newest);
        assert_eq!(send_pace_for(&[Some(cap), Some(fifo)]), Some(StageDepth::send_pace()));

        // ToneSource / 驱动未附着的 HalSpeakerSource：样本在 tick 里现产现取，
        // 等待恒为 0，记 5 ms 是凭空捏造。
        assert_eq!(send_pace_for(&NO_DEPTHS), None);
    }

    /// **采样相位**：播放环深度必须在 `tx.push()` **之前**读。
    ///
    /// 推之后读到的是「这一帧 + 排在它前面的」，恒定多算一整帧 ≈ 10 ms——刚推
    /// 进去的 480 个样本不用等自己——而且因为它恒定，看起来完全像一个真实缓冲，
    /// 不会有人怀疑。源侧三级都在 `next_frame()` 之后读（同样是「新样本前面的
    /// 存量」），两边必须同相。
    ///
    /// `AudioTx` 要一台真设备才造得出来，所以这条守在源码顺序上——它会在有人
    /// 把两行调回去的那一刻变红，而那正是唯一要防的事。
    #[test]
    fn the_play_ring_is_sampled_before_the_push_not_after() {
        let src = include_str!("engine.rs");
        let body = src
            .split("if let Some((_, tx)) = playback.as_mut() {")
            .nth(1)
            .expect("mixer_loop 里的真实输出分支");
        let publish = body.find("publish_play_ring(").expect("发布点");
        let push = body.find("tx.push(").expect("推送点");
        assert!(
            publish < push,
            "publish_play_ring 必须在 tx.push 之前 —— 之后读恒定多算一整帧 10 ms"
        );
        // 桥接环同理：`ring_depth_before_push` 的名字本身就是契约。读数那一行
        // 之后的**三行以内**必须出现它守着的那次 push。
        // 拆开写，免得这条断言自己被自己匹配到（同 `the_probe_tap_...`）。
        let needle = concat!("ring_depth_before_push(StageId::", "BridgeRing");
        for b in src.split(needle).skip(1) {
            let seg: String = b.lines().take(3).collect::<Vec<_>>().join("\n");
            assert!(
                seg.contains("tx.push("),
                "桥接环的读数之后必须紧跟着那次 push，否则相位对不上；\n{seg}"
            );
        }
    }

    // ------------------------------------------- 源消失时必须清槽

    fn tx_stream_for(shared: &Arc<TxShared>) -> TxStream {
        TxStream {
            id: 7,
            crypto: MediaCrypto::new_for_stream(&[0u8; 32], 7, &[0u8; 16]),
            dest: "127.0.0.1:1".parse().unwrap(),
            spec: SourceSpec::Mic,
            loss: LossInjector::new(7, 0.0),
            seq: 0,
            rung: 0,
            rs: None,
            rs_last: 0.0,
            staged: Vec::new(),
            shared: shared.clone(),
        }
    }

    /// 源没了 ⇒ 槽必须清空，而不是把最后一次读数永久钉在那里。
    ///
    /// `TxShared` 的寿命比 `tx_loop` 里的 `TxStream` 长（会话表还持有它，报告
    /// 线程还在读）。`reap_dead_sources` 收尸、或 `TxCmd::Remove` 把 refs 减到
    /// 0 之后，tick 里的 `sources.get(&st.spec)` 拿不到东西——早先那条
    /// `else { continue }` 直接跳过了下面的发布，于是一段**早已不存在的排队**
    /// 会一直显示下去，而且不带任何「这是陈的」标记。
    #[test]
    fn a_vanished_source_clears_its_stage_slots() {
        let shared = Arc::new(TxShared::new());
        let st = tx_stream_for(&shared);
        // 上一 tick 报过的读数
        shared.stages[0].store(Some(StageDepth::new(
            StageId::SrcFifo,
            48_000,
            48_000,
            48_000,
            DropMode::Oldest,
        )));
        shared.stages[SEND_PACE_SLOT].store(Some(StageDepth::send_pace()));
        assert!(shared.stages[0].load().is_some());

        clear_send_stages(&st);
        for (i, slot) in shared.stages.iter().enumerate() {
            assert!(slot.load().is_none(), "槽 {i} 还留着一段死掉的排队");
        }
    }

    /// 三条清槽路径必须都在：tick 里源查不到、`TxCmd::Remove`、收尸。
    /// 少任何一条，那条流的槽就再也不会被覆盖。
    #[test]
    fn every_stream_teardown_path_clears_the_slots() {
        let needle = concat!("clear_send", "_stages(");
        let src = include_str!("engine.rs");
        // 定义 1 处 + 调用 3 处（tick / Remove / reap），测试里的 1 处另计
        let calls = src.matches(needle).count();
        assert!(
            calls >= 4,
            "清槽调用点少了：tick 里源查不到、TxCmd::Remove、reap_dead_sources 三条都要，got {calls}"
        );
    }

    /// The tx engine dedups sources by `SourceSpec`. If the slot were not part
    /// of the key, every peer's speaker session would share ONE entry — one
    /// ring, read once, fanned out to everybody — which is the same collapse
    /// from the other direction.
    #[test]
    fn each_slots_speaker_is_its_own_source_key() {
        let mut m: HashMap<SourceSpec, u32> = HashMap::new();
        m.insert(SourceSpec::HalSpeaker { slot: 0 }, 10);
        m.insert(SourceSpec::HalSpeaker { slot: 1 }, 11);
        assert_eq!(m.len(), 2, "two slots must be two sources, not one shared ring");
        // ...and the same slot twice is one source with two references, which
        // is what keeps the ring to a single consumer (halbridge SPSC rule).
        m.insert(SourceSpec::HalSpeaker { slot: 0 }, 12);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&SourceSpec::HalSpeaker { slot: 0 }], 12);
    }
}
