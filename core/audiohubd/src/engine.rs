//! Media plane wiring: single shared UDP socket, 10ms send scheduler with
//! fan-out + AUTO resample-before-encode, receive/decrypt into jitter buffers,
//! 10ms mixer with soft clip and a 2s post-mix ring for mix_verdicts.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

// ------------------------------------------------------------ 跳 tick 的埋点
//
// `tick = behind` 这条路径此前**无日志、无计数**，而它是全链路唯一的永久性延迟
// 注入点：找出它花了整整一轮调查（9 小时里水位从 ≈0 涨到 434 ms，环只有 500 ms，
// 期间遥测除了水位读数本身没有一个数字会动）。**不能指望第二次也能找出来。**

/// 一条循环的跳 tick 统计（规格 §10.1 的 `skip`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SkipCounters {
    /// 跳 tick 事件数（一次卡顿一次）。
    pub events: u64,
    /// 累计被跳过的 tick 数。
    pub ticks: u64,
    /// 累计被跳过的时长。
    pub ms: u64,
    /// 治法 A 因此从各级队列里排掉的帧/样本数。mixer 侧恒为 0（见 §8.2a）。
    pub drained_frames: u64,
}

#[derive(Debug)]
struct SkipCell {
    events: AtomicU64,
    ticks: AtomicU64,
    drained: AtomicU64,
}

impl SkipCell {
    const fn new() -> SkipCell {
        SkipCell {
            events: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            drained: AtomicU64::new(0),
        }
    }
    fn record(&self, ticks: u64, drained: u64) {
        self.events.fetch_add(1, Ordering::Relaxed);
        self.ticks.fetch_add(ticks, Ordering::Relaxed);
        self.drained.fetch_add(drained, Ordering::Relaxed);
    }
    fn snapshot(&self) -> SkipCounters {
        let ticks = self.ticks.load(Ordering::Relaxed);
        SkipCounters {
            events: self.events.load(Ordering::Relaxed),
            ticks,
            ms: ticks * FRAME_MS,
            drained_frames: self.drained.load(Ordering::Relaxed),
        }
    }
}

/// 进程级而不是 `DaemonInner` 上的字段：这两条循环各只有一个实例，而放进
/// `DaemonInner` 就要改 `lib.rs`（本次改动的边界之外）。
static TX_SKIP: SkipCell = SkipCell::new();
static MIX_SKIP: SkipCell = SkipCell::new();

/// 发送调度器跳过了多少 tick（IPC / probe 用）。
///
/// `allow(dead_code)`：`lib.rs` 的 `daemon.status` 组装点在本次改动的文件边界
/// 之外（并行 agent 在改那个文件），接线是一行
/// `obj.insert("skip", json!({"tx": engine::tx_skip_counters(), ...}))`。
#[allow(dead_code)]
pub fn tx_skip_counters() -> SkipCounters {
    TX_SKIP.snapshot()
}

/// 混音器跳过了多少 tick（IPC / probe 用）。见 [`tx_skip_counters`]。
#[allow(dead_code)]
pub fn mixer_skip_counters() -> SkipCounters {
    MIX_SKIP.snapshot()
}

/// `tx_loop` 的 DLL 现场读数（IPC / probe 用）。
///
/// **必须导出**，理由和跳 tick 埋点完全一样：一个伺服环出问题时，除了水位本身
/// 没有任何一个数字会动。三个数各自能单独定位一类故障：
/// - `corr_ppm` 长期贴着 +500 或 −500 ⇒ 要么真有一大笔存量在被斜坡排空，
///   要么**误差符号写反了**（发散时它会永久贴在一侧）；
/// - `clamped` 在稳态还在涨 ⇒ 观测噪声超出了环路的线性区，该查写块量化；
/// - `resyncs` 涨得快 ⇒ 跳 tick / 驱动重附着在反复发生，病在别处。
///
/// `allow(dead_code)`：`lib.rs` 的 `latency_guard_status` 在本次改动的文件边界
/// 之外，接线是一行 `"dll": engine::tx_dll_counters(),`。
#[allow(dead_code)]
pub fn tx_dll_counters() -> crate::halbridge::dll::DllCounters {
    TX_DLL.snapshot()
}

/// [`tx_dll_counters`] 的存储。`Dll` 本身活在 tx 线程的栈上（单所有者、无原子），
/// 所以每 tick 把读数抄一份到这里 —— 与 `SkipCell` 同一套理由：放进 `DaemonInner`
/// 就要改 `lib.rs`。
#[derive(Debug)]
struct DllCell {
    updates: AtomicU64,
    clamped: AtomicU64,
    resyncs: AtomicU64,
    /// f32 的位模式。
    corr_ppm: std::sync::atomic::AtomicU32,
    bw_hz: std::sync::atomic::AtomicU32,
}

impl DllCell {
    const fn new() -> DllCell {
        DllCell {
            updates: AtomicU64::new(0),
            clamped: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            corr_ppm: std::sync::atomic::AtomicU32::new(0),
            bw_hz: std::sync::atomic::AtomicU32::new(0),
        }
    }
    fn publish(&self, c: crate::halbridge::dll::DllCounters) {
        self.updates.store(c.updates, Ordering::Relaxed);
        self.clamped.store(c.clamped, Ordering::Relaxed);
        self.resyncs.store(c.resyncs, Ordering::Relaxed);
        self.corr_ppm.store(c.corr_ppm.to_bits(), Ordering::Relaxed);
        self.bw_hz.store(c.bw_hz.to_bits(), Ordering::Relaxed);
    }
    fn snapshot(&self) -> crate::halbridge::dll::DllCounters {
        crate::halbridge::dll::DllCounters {
            updates: self.updates.load(Ordering::Relaxed),
            clamped: self.clamped.load(Ordering::Relaxed),
            resyncs: self.resyncs.load(Ordering::Relaxed),
            corr_ppm: f32::from_bits(self.corr_ppm.load(Ordering::Relaxed)),
            bw_hz: f32::from_bits(self.bw_hz.load(Ordering::Relaxed)),
        }
    }
}

static TX_DLL: DllCell = DllCell::new();

/// 把本线程提到 `USER_INTERACTIVE` QoS（治法 D，macOS）。
///
/// ## 为什么做
///
/// `tx_loop` / `mixer_loop` 是**默认优先级的普通线程**，而它们配对的生产者是
/// coreaudiod 的**实时优先级** IOProc。普通线程被抢占超过 100 ms 的概率远高于
/// 实时线程漏一个周期的概率 ⇒ 上跳比下跳频繁 ⇒ 水位向 500 ms 上限单调爬升。
/// 这是那个上偏的物理来源。
///
/// ## 为什么是 QoS 而**不是** `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)`
///
/// QoS 是给调度器的**提示**：线程仍然在分时band里，只是排在前面。设错了最坏
/// 结果是没效果。时间约束策略是**真实时**：它向内核申请「每 period 保证 computation
/// 微秒」，预算写大了会直接和 coreaudiod 自己的 IOProc 抢 CPU，把用户的系统音频
/// 搞破音。
///
/// 拒绝它的理由**只有一条**：我们给不出一个能诚实填进 `computation` 的上界。
/// `sendto()` 会进内核网络栈，排队规则、路由查找、ARP 解析都可能让单次调用的
/// 耗时上界不可预知。时间约束策略要求申报一个诚实的预算，申报不出来就不该申报。
/// **风险不对称，明确不做。**
///
/// > 此处原先还把「这条线程不存在硬性截止期」列为拒绝理由之一。
/// > **那条论据是错的，已删。** `tx_loop` 的截止期硬得很：迟到 >100 ms 就触发
/// > 下面 `drain_skipped_ticks` 的丢弃，制造一次可闻空洞；治法 A 之前更是永久
/// > +100 ms 延迟。截止期正是这条线程整个故障模型的核心，说它不存在会在将来
/// > 误导人。**结论不变——错的是论据，不是结论。**
///
/// 另注：Apple 的 Energy Efficiency Guide 对 `user-interactive` 的定义是
/// 「operating on the main thread, refreshing the user interface, or performing
/// animations」，**只字未提音频、实时或核心选择**；它只承诺
/// 「The system uses QoS information to adjust priorities such as scheduling,
/// CPU and I/O throughput, and timer latency.」⇒ **不要声称 QoS 能保证 P-core
/// 落位**，没有一手依据。
///
/// 这只降低 >100 ms 卡顿的**频率**，不改变「一旦发生就永久」的性质，所以它
/// 不能替代治法 A / DLL 伺服，只能叠加。
#[cfg(target_os = "macos")]
fn raise_audio_thread_qos(what: &str) {
    // <pthread/qos.h>：qos_class_t 是 unsigned int，QOS_CLASS_USER_INTERACTIVE
    // = 0x21。relative_priority 传 0 = 该 band 的最高档。
    const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;
    extern "C" {
        fn pthread_set_qos_class_self_np(
            qos_class: libc::c_uint,
            relative_priority: libc::c_int,
        ) -> libc::c_int;
    }
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        // 失败不是错误：没提上去只是回到从前，治法 A/B 照常工作。
        dlog!("[audiohubd] {what}: 提升线程 QoS 失败 (rc={rc})，按默认优先级继续");
    }
}

#[cfg(not(target_os = "macos"))]
fn raise_audio_thread_qos(_what: &str) {
    // Windows 的对应物是 `AvSetMmThreadCharacteristicsW("Pro Audio")`（avrt.dll）。
    // 收益同样是「降低被抢占的概率」，但代价是给这个 crate 引入一条 Windows
    // 系统库依赖，而 Cargo.toml 明确记着「gated so the windows-gnu build keeps
    // its raw-dylib-free dep graph」。Windows 侧的病灶又在 `play_ring`（真跨
    // 时钟、另开规格），不在这条循环上 —— 先不动，等 win 侧的双向控制器一起做。
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

    /// 治法 A：一次 >100 ms 的消费侧卡顿之后，把被跳过的那些样本从 FIFO 里丢掉。
    ///
    /// 这一级与 `hal_spk` **完全同构**（消费者都是 `tx_loop`，缸还大一倍——1 秒
    /// vs 500 ms），同一次卡顿会**同时**在两处注入积压。不在这里一起排掉，
    /// 治好的只是其中一半。
    ///
    /// **不计进 `self.dropped`**：那个计数器的语义是「FIFO 饱和时丢最旧」，是
    /// 用来区分「稳态速率失配」与「被一次卡顿灌满」的判据；把主动排空混进去
    /// 会把那条诊断毁掉（规格 §10.1 同一条理由）。
    /// 与 HAL 环那一侧同一条纪律（`HalBridge::drain_spk`）：**留下一帧的工作
    /// 储备**。生产者在同一段时间里也停了的话，FIFO 里根本没那么多东西，
    /// 无脑排到底就是把一个延迟问题换成一个欠载问题。
    fn drain_skipped(&mut self, samples: usize) -> usize {
        let n = samples.min(self.fifo.len().saturating_sub(F48));
        self.fifo.drain(..n);
        n
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
            //
            // 排到 `D_target` 而**不是排到 0**（规格 §4.4）。排到 0 的代价是真实
            // 的：此后每一个 `W_n < F` 的 tick 都要短读补静音，水位靠**我们自己
            // 的短读**慢慢爬回写块抖动之上——那段爬升期是听得见的细碎断续。
            // 驱动声明的周期 512 帧 = 10.67 ms **比一个 tick 长**，所以 `W_n = 0`
            // 的 tick 是必然会出现的，不是偶发。
            //
            // 这里不需要淡化：开流时没有任何已交付的音频，不存在连续性可破坏。
            let keep = crate::halbridge::trim::D_TARGET_COLD;
            let backlog = hal.spk_depth(*slot).map(|(n, _)| n as usize).unwrap_or(0);
            let want = backlog.saturating_sub(keep);
            if want > 0 {
                let mut stale = Vec::with_capacity(want);
                let dropped = hal.read_spk_mono(*slot, &mut stale, want);
                let per_ms = crate::halbridge::HAL_SAMPLE_RATE as usize / 1000;
                dlog!(
                    "[audiohubd] hal speaker slot {slot}: dropped {}ms of audio played before \
                     this stream opened (留下 {}ms 作为起始水位)",
                    dropped / per_ms,
                    keep / per_ms,
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

/// **治法 A**：跳 tick 时把被跳过的那些帧从每一级消费侧队列里读走丢掉。
/// 返回丢掉的总量（帧/样本，用于埋点）。
///
/// 治的是这个：`tick = behind` 之后，被跳过的那段音频既没被读走也没被丢掉，
/// 它**永久**留在环里 —— 生产者与消费者锚在同一个 `mach_absolute_time` 上，
/// 长期速率误差为零，没有任何机制会把它排出去。实测一次 108 ms 的卡顿换来
/// 永久 +108 ms 的延迟，9 小时积到 434 ms（环容量 500 ms）。
///
/// 换来的是「一次 108 ms 空洞」代替「永久 +108 ms」。那个空洞**两种情况下都
/// 存在**（tx 线程停了，什么都没发出去，对端 JB 必然饿死）；做了 A 只是在已有
/// 空洞上额外丢掉 108 ms 内容，换掉永久延迟。
///
/// 只丢**已经积压**的部分（每一级都以自身 `len()` 封顶），所以不会欠载。
fn drain_skipped_ticks(
    hal: Option<&crate::halbridge::HalBridge>,
    sources: &mut HashMap<SourceSpec, SourceEnt>,
    skipped: u64,
) -> u64 {
    let frames = (skipped as usize).saturating_mul(F48);
    let mut total = 0u64;
    for (spec, ent) in sources.iter_mut() {
        if let SourceSpec::HalSpeaker { slot } = spec {
            if let Some(h) = hal {
                total += h.drain_spk(*slot, frames) as u64;
            }
        }
        // 三个 1 秒源侧 FIFO 里我们够得着的那一个。
        //
        // ## 遗留项：`MicSource` 的 1 秒 FIFO 仍然没人治，DLL **也没盖住它**
        //
        // 那个 FIFO 在 `audiohub-net` 的 `media.rs` 里，是私有的 `VecDeque`；
        // 读取面已经有了（`fifo_len()` / `fifo_cap()` / `depths()`），但**没有
        // 消费侧的排空接口**（只有 `next_frame` 每 tick 弹一帧）。所以治法 A
        // 够不着它：一次跳 tick 在这里注入的积压是永久的，和 `hal_spk` 病理相同。
        //
        // DLL 伺服覆盖了它的**执行器**，没覆盖它的**观测**：
        //
        // - 执行器**够得着**。唤醒周期变短，这一 tick 的 `next_frame` 就来得更
        //   早，mic FIFO 也跟着被弹得更快 —— 一个 `corr` 作用于全部源。
        // - 观测**够不着**。`spk_phase_error` 只归约 HAL 扬声器环。一个只有
        //   `Mic` 源的会话给不出任何观测 ⇒ `corr` 保持在 1.0 ⇒ 那条会话的
        //   调度**仍然是开环的**，跳 tick 注入的积压照样永久。
        // - 混合会话（同时挂 HAL 扬声器 + 麦克风）里，mic FIFO 会被一条按
        //   **别的时钟域**算出来的误差拖着走。`corr` 钳在 ±500 ppm ⇒ 拖动幅度
        //   ≤0.5 ms/s，且稳态下 `corr → 1`，所以不制造新的稳态偏置，但瞬态耦合
        //   是真的。
        //
        // **为什么这次不补**：补它要么给 `FrameSource` 加一个排空方法（改
        // `audiohub-net`，越界），要么让 DLL 同时伺服两个**独立晶振**的缓冲级
        // ——而一个唤醒周期只能服务一个时钟域边界（RFC 7273：共享参考时钟时应
        // avoiding rate conversion，反之则应当只有一个转换点）。麦克风是真实
        // 设备、有自己的晶振，属于**跨时钟**那一类，正确的归宿是 D.2-② 那套
        // 速率伺服，不是这条相位环。
        if let Src::Sys(s) = &mut ent.src {
            total += s.drain_skipped(frames) as u64;
        }
    }
    total
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
    raise_audio_thread_qos("tx_loop");
    let start = Instant::now();
    let mut tick: u64 = 0;
    // ---------------------------------------------------------------- DLL 伺服
    //
    // 这条循环的唤醒时刻**不再**是 `start + tick × 10 ms`（开环）。开环累加把
    // 每一次相位扰动永久积分：跳一次 tick 就永久 +100 ms，同时钟域里没有任何
    // 机制会把它排出去（实测 9 小时积到 434 ms）。
    //
    // 改成 `next_time += 10 ms / corr`，`corr` 由二阶 DLL 从缓冲深度误差算出。
    // 这是 PipeWire `alsa-pcm.c:3110` 在 driver+tsched 路径上的同一条式子——那条
    // 路径同样**不做重采样**（`matching = false` ⇒ `rate_match->rate` 硬置 1.0），
    // DLL 仍然照跑，`corr` 唯一的去处就是唤醒时刻。同时钟消除的是**速率**误差，
    // 不消除**相位**误差。
    //
    // 误差信号 `err = D_target − 读后残量` 由 `HalSpeakerSource` 在读的同一相位
    // 上发布（`halbridge::dll` 模块文档写明了符号推导：写成反的是正反馈）。
    let mut dll = crate::halbridge::dll::Dll::new(F48 as f64, 48_000.0);
    let mut dll_win = crate::halbridge::SpkPhaseWindow::new();
    let mut next_time = start;
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
        //
        // 「落后」的基准是 **DLL 伺服出来的计划时刻 `next_time`**，不再是
        // `start + tick × 10 ms`。这一改必须跟着 DLL 一起做：`corr ≠ 1` 期间
        // 计划时刻会相对标称慢慢漂开，拿标称当基准的话，一段持续 −500 ppm 的
        // 修正跑上 20 分钟就会被误判成一次 600 ms 的卡顿，凭空触发一次治法 A
        // 的丢弃。`corr ≡ 1` 时下式与旧的 `start.elapsed()/FRAME_MS` **逐位相等**
        //（`next_time = start + tick×10ms` ⇒ `tick + (elapsed − tick×10)/10`）。
        let late_ms = Instant::now().saturating_duration_since(next_time).as_millis() as u64;
        let behind = tick + late_ms / FRAME_MS;
        // 本 tick 准不准时。落后 ≤100 ms 时循环用背靠背的 tick 追平（自愈），
        // 那期间队列深度是**假高**——高是因为我们暂时没读，不是因为积压。水位
        // 控制器必须知道这件事，否则它会把马上就要用到的音频削掉（不变量 I6）。
        let punctual = behind <= tick;
        if behind > tick + 10 {
            // 治法 A：被跳过的那些帧从队列里读走丢掉，而不是留在里面。
            // 这条路径此前无日志、无计数，是它能潜伏 9 小时的直接原因。
            let skipped = behind - tick;
            let drained = drain_skipped_ticks(hal.as_deref(), &mut sources, skipped);
            TX_SKIP.record(skipped, drained);
            dlog!(
                "[audiohubd] tx_loop 落后 {}ms，跳过 {skipped} 个 tick 并从队列里排掉 \
                 {drained} 帧（累计：{} 次 / {}ms / {} 帧）",
                skipped * FRAME_MS,
                TX_SKIP.events.load(Ordering::Relaxed),
                TX_SKIP.ticks.load(Ordering::Relaxed) * FRAME_MS,
                TX_SKIP.drained.load(Ordering::Relaxed),
            );
            tick = behind;
            // **治法 A 与 DLL 的交接**（两者不冲突，但必须在这里握一次手）。
            //
            // A 治的是**离散注入**：跳 tick 期间那段音频既没被读走也没被丢掉，
            // 是一次阶跃。DLL 治的是**连续相位误差**，执行器是 500 ppm 的速率
            // 弯曲——它排一次 100 ms 的注入要三分多钟。让 DLL 去排阶跃，等于把
            // 一次可以立刻还清的债拖成几分钟的高水位；让 A 去修连续误差，它又
            // 完全没有触发条件（水位只在跳 tick 那一刻动）。**分工是互补的。**
            //
            // 但排空之后水位发生了阶跃，而 `z3`（唯一的积分器）里存的是阶跃
            // **之前**那段历史的积分。不复位它就会在跳变后继续输出为旧误差算出
            // 的修正 ⇒ 过冲 ⇒ 欠载。PipeWire 在 `node-driver.c:487–494` 做的是
            // 同一件事（重同步档强制 `BW_MAX` + `err = 0`，更大时再叠加
            // `spa_dll_init()`），PulseAudio 的 `fast_adjust` 之后也直接
            // `return`、跳过本轮速率更新。两个独立实现，同一条规矩。
            //
            // 三件事一起做，缺一不可：
            //   1. `next_time` 重新锚到现在——不然计划时刻停在几百毫秒前，
            //      循环会空转到追平为止；
            //   2. 整环复位并回到捕获带宽（`resync`）；
            //   3. 观测基准作废——排空当拍发布的水位描述的是「刚被削掉之前」，
            //      喂进环路就是一条纯噪声。
            next_time = Instant::now();
            dll.resync();
            dll_win.invalidate();
        }
        if let Some(h) = hal.as_ref() {
            h.set_tick_punctual(punctual);
        }
        let deadline = next_time;
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
            // 空闲这一段（最长 200 ms）没有任何源被取过，`drain_idle_speakers`
            // 反而把已发布的空闲环冲掉了 —— 是一次货真价实的不连续。与跳 tick
            // 分支同样的三件事：重锚计划时刻、整环复位、作废观测基准。
            // 不重锚的话，恢复的第一 tick 会看到 200 ms 的 `late_ms`，
            // 直接被误判成一次卡顿并触发治法 A。
            tick += 1;
            next_time = Instant::now() + Duration::from_millis(FRAME_MS);
            dll.resync();
            dll_win.invalidate();
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
        // ---- 闭环的那一步：喂误差、算下一次唤醒 -----------------------------
        //
        // 位置必须在**源已经被取过**之后：`HalSpeakerSource` 是在 `next_frame`
        // 里读环并发布「读后残量」的，放在取之前拿到的是上一 tick 的观测。
        //
        // 追平期（`!punctual`）一个观测都不喂：那期间循环在背靠背补跑，水位是
        // **假高**（高是因为我们暂时没读，不是积压）。喂进去环路会去排一段马上
        // 就要被自己读走的音频 —— 这是不变量 I6 在 DLL 侧的对应物。源侧也各自
        // 挡了一道（不推进发布代次），两道都在，因为写反的表现是「偶尔有点断续」，
        // 靠听抓不住。
        //
        // 没有新鲜观测的 tick（没挂 HAL 源、驱动没附着）**保持**上一次的 `corr`
        // 而不是回落到 1.0：回落等于每次观测中断都给环路注一次阶跃。
        if punctual {
            if let Some(p) = hal.as_ref().and_then(|h| h.spk_phase_error(&mut dll_win)) {
                dll.update(p.err_frames as f64);
            }
        }
        next_time += Duration::from_nanos(dll.period_nanos());
        if slow_tick {
            // 每秒抄一份给 IPC。10 ms 节拍上不做这件事：五个 store 换不来任何
            // 诊断价值，而这条路径上的规矩是「测量不许改变被测对象」。
            TX_DLL.publish(dll.counters());
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
    raise_audio_thread_qos("mixer_loop");
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
            // ⚠ **代码和 tx_loop 一样，后果不同：这里不排空任何东西。**
            //
            // mixer 跳 tick 时它少做了两件事：
            //  1) 少 pop 了 JB ⇒ JB 深度涨 —— 但 JB 自己的
            //     `while frames.len() > target + 6`（media.rs）在下一次 pop 时
            //     会全部修剪掉，封顶 180 ms 且**自愈**，不需要我们插手；
            //  2) 少 push 了 play_ring / bridge_ring / hal_mic ⇒ 那三级**变浅**，
            //     是欠载不是积压，**没有任何东西可以「排空」**。
            //
            // 照抄治法 A 给它加排空代码，就是主动制造欠载。这里唯一该补的是
            // 观测性——这条路径此前和 tx_loop 一样完全静默。
            let skipped = behind - tick;
            MIX_SKIP.record(skipped, 0);
            dlog!(
                "[audiohubd] mixer_loop 落后 {}ms，跳过 {skipped} 个 tick（累计 {} 次 / {}ms）\
                 ——不排空：JB 自带修剪，输出环那边是欠载不是积压",
                skipped * FRAME_MS,
                MIX_SKIP.events.load(Ordering::Relaxed),
                MIX_SKIP.ticks.load(Ordering::Relaxed) * FRAME_MS,
            );
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


    // ======================================== 跳 tick：治法 A 与它的观测缺口
    //
    // 这两条循环里的 `tick = behind` 是全链路**唯一**的永久性延迟注入点，而在
    // 本次改动之前它无日志、无计数 —— 一次 108 ms 的卡顿变成永久 +108 ms，
    // 9 小时积到 434 ms（环容量 500 ms），期间除了水位读数本身没有一个数字会动。
    // 找出它花了整整一轮调查，所以下面既测行为也测**观测**。

    /// 取一个顶层函数的函数体。顶层 `}` 只在函数结束时顶格出现。
    fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let s = src.split(sig).nth(1).unwrap_or_else(|| panic!("找不到 {sig}"));
        let end = s.find("\n}\n").expect("函数没有结束");
        &s[..end]
    }

    /// 取循环里 `if behind > tick + 10 { … }` 那个分支的分支体。
    fn skip_branch(body: &str) -> &str {
        let s = body
            .split("if behind > tick + 10 {")
            .nth(1)
            .expect("跳 tick 的分支不见了");
        let end = s.find("\n        }").expect("分支没有结束");
        &s[..end]
    }

    /// **治法 A 的接线守卫**：tx_loop 跳 tick 时必须排空、必须计数、必须落日志。
    ///
    /// 排空要一个真实设备 + 一条 UDP socket + 一整张源表才走得到，单测构造不出
    /// 来；而漏掉的从来是**接线**不是逻辑 —— 这个病本身就是「一行赋值没有配套
    /// 的排空」。所以这一条守在源码上：把 `drain_skipped_ticks` 或 `TX_SKIP.record`
    /// 从那个分支里拿掉（= 回到出病的行为），它立刻变红。
    #[test]
    fn the_tx_skip_branch_drains_counts_and_logs() {
        let src = include_str!("engine.rs");
        let branch = skip_branch(fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        ));
        assert!(
            branch.contains("drain_skipped_ticks("),
            "治法 A 没了：被跳过的帧会永久留在队列里，而这一次没有任何东西会告诉你\n{branch}"
        );
        assert!(
            branch.contains("TX_SKIP.record("),
            "跳 tick 没有计数 —— 这正是它能潜伏 9 小时的直接原因\n{branch}"
        );
        assert!(branch.contains("dlog!("), "跳 tick 没有日志\n{branch}");
        // 排空必须**在**把 tick 推过去之前发生：`tick = behind` 之后那些帧就
        // 再也没人认领了。
        let drain = branch.find("drain_skipped_ticks(").unwrap();
        let assign = branch.find("tick = behind").expect("跳 tick 本身");
        assert!(drain < assign, "排空写在了 `tick = behind` 之后");
    }

    /// **DLL 伺服的接线守卫**：`tx_loop` 的唤醒时刻必须由 `corr` 推出来。
    ///
    /// 这一条守的是本轮改动的全部收益。开环那行 `start + tick * FRAME_MS` 只要
    /// 回来一次，相位扰动就重新开始被永久积分，而**表面上什么都不会变**——
    /// 包数、丢包率、音调探针全绿，只有水位在几小时里慢慢爬。整轮调查就是这么
    /// 花掉的，所以它守在源码上。
    #[test]
    fn the_tx_deadline_is_driven_by_the_dll_not_by_open_loop_accumulation() {
        let src = include_str!("engine.rs");
        let body = fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        );
        assert!(
            body.contains("next_time += Duration::from_nanos(dll.period_nanos())"),
            "唤醒时刻不再由 DLL 推进 —— 每一次相位扰动会重新被永久积分"
        );
        assert!(
            body.contains("let deadline = next_time;"),
            "deadline 没接到伺服出来的计划时刻上"
        );
        assert!(
            !body.contains("start + Duration::from_millis(tick * FRAME_MS)"),
            "开环累加回来了"
        );
        // 落后判据也必须以 `next_time` 为基准：拿标称当基准的话，持续的
        // `corr ≠ 1` 修正会被误判成卡顿，凭空触发治法 A。
        assert!(
            body.contains("saturating_duration_since(next_time)"),
            "`behind` 还在拿 `start.elapsed()` 当基准 —— 一段持续 −500 ppm 的修正\
             跑 20 分钟就会被当成一次 600 ms 的卡顿"
        );
        assert!(
            !body.contains("start.elapsed().as_millis() as u64 / FRAME_MS"),
            "旧的标称基准还在"
        );
    }

    /// **治法 A 与 DLL 的交接**：跳 tick 之后三件事一件都不能少。
    ///
    /// 少了 `next_time` 重锚 ⇒ 计划时刻停在几百毫秒前，循环空转到追平；
    /// 少了 `dll.resync()` ⇒ 积分器里存着跳变**之前**的历史，排空之后继续按旧
    /// 误差修正 ⇒ 过冲 ⇒ 欠载（`dll::tests::skipping_resync_after_a_step_overshoots`
    /// 量化了这一条）；少了 `dll_win.invalidate()` ⇒ 排空当拍那个横跨跳变的水位
    /// 被喂进环路。
    #[test]
    fn the_skip_branch_hands_the_dll_over_properly() {
        let src = include_str!("engine.rs");
        let branch = skip_branch(fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        ));
        for (needle, why) in [
            ("next_time = Instant::now()", "计划时刻没有重锚，循环会空转到追平"),
            ("dll.resync()", "积分器没复位，跳变之后会按旧误差继续修正 ⇒ 过冲"),
            ("dll_win.invalidate()", "排空当拍的水位仍会被当成有效观测"),
        ] {
            assert!(branch.contains(needle), "跳 tick 之后少了 `{needle}`：{why}\n{branch}");
        }
    }

    /// 追平期一个观测都不喂（不变量 I6 的 DLL 侧）。
    ///
    /// 源侧已经挡了一道（不推进发布代次），这里是第二道。两道都要，因为写反的
    /// 表现是「偶尔有点断续」，靠听抓不住，而两处只要有一处漏了就复发。
    #[test]
    fn the_dll_is_only_fed_on_punctual_ticks() {
        let src = include_str!("engine.rs");
        let body = fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        );
        let feed = body
            .find("dll.update(")
            .expect("tx_loop 根本没有喂 DLL —— 环路是开环的");
        let guard = body[..feed]
            .rfind("if punctual {")
            .expect("`dll.update` 外面没有准时守卫");
        // 守卫和喂点之间不许隔着别的语句块（否则守的是别处）。
        let between = &body[guard..feed];
        assert!(
            between.matches('{').count() <= 3,
            "`if punctual` 与 `dll.update` 之间隔了太多层，守卫多半守错了地方：\n{between}"
        );
        // 取观测这一步必须排在源被取过之后：`HalSpeakerSource` 是在 `next_frame`
        // 里读环并发布读后残量的。
        let pull = body.find("ent.src.next_frame(").expect("取帧点");
        assert!(pull < feed, "在源被取之前就取观测 —— 拿到的是上一 tick 的读数");
    }

    /// 空闲路径同样要重锚 + 复位，否则恢复的第一 tick 会被误判成一次 200 ms 卡顿。
    #[test]
    fn the_idle_path_re_anchors_the_schedule() {
        let src = include_str!("engine.rs");
        let body = fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        );
        let arm = body
            .split("if streams.is_empty() {")
            .nth(1)
            .expect("空闲短路分支");
        let arm = arm.split("\n        }").next().unwrap();
        assert!(
            arm.contains("next_time = Instant::now()"),
            "空闲之后没有重锚计划时刻 —— 恢复的第一 tick 会看到 200 ms 的落后，\
             直接触发治法 A 的丢弃\n{arm}"
        );
        assert!(arm.contains("dll.resync()"), "空闲之后没有复位环路\n{arm}");
        assert!(arm.contains("dll_win.invalidate()"), "空闲之后没有作废观测基准\n{arm}");
    }

    /// QoS 的注释里不许再出现「没有硬截止期」这条**错误论据**。
    ///
    /// 结论（继续不用 `THREAD_TIME_CONSTRAINT_POLICY`）不变，变的是论据：
    /// `tx_loop` 有非常硬的截止期——迟到 >100 ms 就走 `drain_skipped_ticks`
    /// 的丢弃，制造一次可闻空洞。留着错的论据，将来会有人拿它去推翻结论。
    #[test]
    fn the_qos_rationale_no_longer_claims_there_is_no_deadline() {
        let src = include_str!("engine.rs");
        let doc = src
            .split("fn raise_audio_thread_qos(what: &str) {")
            .next()
            .unwrap();
        let doc = &doc[doc.rfind("/// 把本线程提到").expect("QoS 的文档注释")..];
        assert!(
            !doc.contains("没有硬截止期"),
            "错误论据回来了：`tx_loop` 迟到 >100 ms 就制造可闻空洞，那就是硬截止期"
        );
        assert!(
            doc.contains("computation"),
            "唯一站得住的那条理由（申报不出诚实的 computation 上界）被一起删掉了"
        );
    }

    /// **mixer_loop 恰恰相反：只计数、不排空**（规格 §8.2a）。
    ///
    /// 代码和 tx_loop 一模一样，后果完全不同：它少 pop 的是 JB（自带
    /// `while len > target + 6` 的硬修剪，封顶 180 ms 且自愈），少 push 的是三个
    /// 输出环（那是**欠载**不是积压，没有任何东西可以排空）。照抄治法 A 给它加
    /// 排空代码，就是主动制造欠载。
    #[test]
    fn the_mixer_skip_branch_counts_but_must_not_drain() {
        let src = include_str!("engine.rs");
        let branch = skip_branch(fn_body(
            src,
            "pub(crate) fn mixer_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<MixCmd>) {",
        ));
        assert!(branch.contains("MIX_SKIP.record("), "mixer 跳 tick 没有计数\n{branch}");
        assert!(branch.contains("dlog!("), "mixer 跳 tick 没有日志\n{branch}");
        assert!(
            !branch.contains("drain_skipped_ticks(") && !branch.contains("drain_spk("),
            "mixer 侧加了排空 —— JB 会自己修剪，输出环那边是欠载不是积压，\
             这行代码只会主动制造断续\n{branch}"
        );
    }

    /// 准时标志必须用**跳 tick 之前**的 `tick` 算，并且每 tick 都报给 HAL。
    ///
    /// 不变量 I6：追平期的水位是假高（我们暂时没读，不是积压），在那些 tick 上
    /// 削会把马上就要用到的音频削掉。写反的表现是「偶尔有点断续」，靠听抓不住。
    #[test]
    fn punctuality_is_measured_before_the_skip_and_reported_every_tick() {
        let src = include_str!("engine.rs");
        let body = fn_body(
            src,
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
        );
        let punctual = body.find("let punctual = behind <= tick;").expect("准时判据");
        let skip = body.find("if behind > tick + 10 {").expect("跳 tick 分支");
        assert!(
            punctual < skip,
            "`punctual` 在 `tick = behind` 之后才算 —— 那样刚跳完的那一 tick 会被\
             当成准时的，而它恰恰是最不该 trim 的一个"
        );
        assert!(
            body.contains("h.set_tick_punctual(punctual)"),
            "准时标志没报给 HAL：水位控制器于是永远以为自己是准时的"
        );
    }

    /// 计数器本身：一次事件 = 一次 `events`，tick 数与毫秒数要对得上。
    #[test]
    fn the_skip_counters_add_up() {
        let c = SkipCell::new();
        assert_eq!(c.snapshot(), SkipCounters::default());
        c.record(11, 5_280);
        c.record(23, 11_040);
        let s = c.snapshot();
        assert_eq!(s.events, 2, "两次卡顿是两个事件");
        assert_eq!(s.ticks, 34);
        assert_eq!(s.ms, 340, "34 个 tick × 10 ms");
        assert_eq!(s.drained_frames, 16_320);
        // 序列化成 IPC 要看的那几个键。
        let j = serde_json::to_value(s).unwrap();
        for k in ["events", "ticks", "ms", "drained_frames"] {
            assert!(j.get(k).is_some(), "IPC 少了 {k}");
        }
    }

    /// 源侧 FIFO 的排空：与 HAL 环完全同构的第二个病灶（缸还大一倍，1 秒 vs
    /// 500 ms，消费者是同一条 `tx_loop`）。同一次卡顿会**同时**在两处注入积压。
    #[test]
    fn a_skipped_tick_drains_the_source_fifo_too() {
        let mut src = sys_frames(48_000, 5_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            src.next_frame(&mut out); // 灌到 1 秒上限
        }
        let before = src.depths()[0].unwrap().samples;
        let dropped_before = src.depths()[0].unwrap().dropped;
        assert!(before > 40_000, "前提：FIFO 确实积着东西, got {before}");

        // 一次 108 ms 的卡顿 ⇒ 11 个 tick × 480。
        let n = src.drain_skipped(11 * F48);
        assert_eq!(n, 11 * F48);
        assert_eq!(src.depths()[0].unwrap().samples, before - 11 * F48 as u32);
        assert_eq!(
            src.depths()[0].unwrap().dropped,
            dropped_before,
            "主动排空绝不能计进 `dropped` —— 那个数是用来区分「稳态速率失配」\
             与「被一次卡顿灌满」的，混进去就把那条诊断毁了"
        );
        // 以 FIFO 现有长度封顶，而且**留下一帧的工作储备**：生产者在同一段时间
        // 里也停了的话，无脑排到底就是把一个延迟问题换成一个欠载问题。
        let rest = src.depths()[0].unwrap().samples as usize;
        assert_eq!(src.drain_skipped(10 * rest), rest - F48);
        assert_eq!(src.depths()[0].unwrap().samples as usize, F48);
        assert_eq!(src.drain_skipped(4_800), 0, "已经只剩储备了，一个样本都不许再排");
    }

    /// 治法 D：QoS 提升必须真的生效，而不是「调了一个签名写错的 C 函数」。
    ///
    /// `pthread_set_qos_class_self_np` 的返回值在参数写错时也可能是 0，所以这里
    /// 把提升后的 QoS **读回来**比对。这是唯一能区分「提上去了」和「以为提上去
    /// 了」的办法，而后者的表现就是本次要治的那个上偏一点没变。
    #[cfg(target_os = "macos")]
    #[test]
    fn the_audio_threads_really_get_a_higher_qos_class() {
        const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;
        const QOS_CLASS_DEFAULT: libc::c_uint = 0x15;
        extern "C" {
            fn pthread_self() -> *mut libc::c_void;
            fn pthread_get_qos_class_np(
                thread: *mut libc::c_void,
                qos_class: *mut libc::c_uint,
                relative_priority: *mut libc::c_int,
            ) -> libc::c_int;
        }
        // 在自己的线程上做，免得把测试线程池的 QoS 改掉。
        let got = std::thread::spawn(|| {
            let mut before = 0u32;
            let mut rel = 0i32;
            unsafe { pthread_get_qos_class_np(pthread_self(), &mut before, &mut rel) };
            raise_audio_thread_qos("test");
            let mut after = 0u32;
            unsafe { pthread_get_qos_class_np(pthread_self(), &mut after, &mut rel) };
            (before, after)
        })
        .join()
        .unwrap();
        assert_eq!(
            got.1, QOS_CLASS_USER_INTERACTIVE,
            "QoS 没提上去（{:#x} -> {:#x}）—— 对面是实时优先级的 coreaudiod IOProc，\
             这条线程按默认优先级跑正是水位上偏的物理来源",
            got.0, got.1
        );
        assert!(
            got.0 <= QOS_CLASS_DEFAULT,
            "前提：线程起手确实不是 USER_INTERACTIVE, got {:#x}",
            got.0
        );
    }

    /// 两条音频循环都要提，而且要在循环开始**之前**提。
    #[test]
    fn both_audio_loops_raise_their_qos_before_the_loop() {
        let src = include_str!("engine.rs");
        for sig in [
            "pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {",
            "pub(crate) fn mixer_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<MixCmd>) {",
        ] {
            let body = fn_body(src, sig);
            let raise = body
                .find("raise_audio_thread_qos(")
                .unwrap_or_else(|| panic!("{sig} 没有提 QoS"));
            let lp = body.find("\n    loop {").expect("循环");
            assert!(raise < lp, "{sig} 的 QoS 提升写在了循环里面");
        }
    }

    /// 开流那一次排空到 `D_target`，**不是**排空到 0（规格 §4.4）。
    ///
    /// 排到 0 的代价是真实的：驱动声明的周期 512 帧 = 10.67 ms 比一个 tick 长，
    /// `W_n = 0` 的 tick 必然周期性出现，此后每一个这样的 tick 都要短读补静音，
    /// 水位靠**我们自己的短读**慢慢爬回抖动之上 —— 那段爬升期是听得见的断续。
    #[test]
    fn the_open_stream_flush_leaves_a_starting_water_level() {
        let src = include_str!("engine.rs");
        let arm = src
            .split("SourceSpec::HalSpeaker { slot } => {")
            .nth(1)
            .expect("build_source 的 HAL 分支");
        let arm = arm.split("Src::Frame(").next().unwrap();
        assert!(
            arm.contains("trim::D_TARGET_COLD"),
            "开流排空又回到了「排到 0」\n{arm}"
        );
        assert!(
            arm.contains("saturating_sub(keep)"),
            "留下的水位不是从实际积压里减出来的\n{arm}"
        );
        assert!(
            !arm.contains("&mut stale, crate::halbridge::HAL_RING_FRAMES as usize"),
            "还在无条件读走整整一环\n{arm}"
        );
        // 起始水位就是冷启动目标：30 ms。
        assert_eq!(crate::halbridge::trim::D_TARGET_COLD, 30 * 48);
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
