//! Media plane wiring: single shared UDP socket, 10ms send scheduler with
//! fan-out + AUTO resample-before-encode, receive/decrypt into jitter buffers,
//! 10ms mixer with soft clip and a 2s post-mix ring for mix_verdicts.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audiohub_core::audio::{
    self, AudioTx, LiveCapture, LivePlayback, PlaybackConfigSnapshot, PlaybackMonitor,
};
use audiohub_core::dsp::{self, InterleavedLinearResampler, ToneVerdict};
use audiohub_core::latency::{DropMode, SourceDepths, StageDepth, StageId, StageSlot, NO_DEPTHS};
use audiohub_core::sysaudio::{self, SysAudioCapture};
use audiohub_net::media::{
    CaptureFifo, FrameSource, LossInjector, MediaCrypto, MicSource, ToneSource,
};
use audiohub_net::packet::{Codec, Header, Kind};

use crate::rtsafe::SpscRing;
use crate::tcpmedia::MediaPath;
use crate::{dlog, lk, rd, rtlog, DaemonInner, RxStream, TxShared};

/// 一帧的毫秒数。`pub(crate)` 是为了让 `servo.rs` 能断言它与伺服里那份常量
/// 相等——同一个物理量在两处各写一份，漂了之后伺服每一步的换算都会偏，
/// 而不会有任何一处报错。
pub(crate) const FRAME_MS: u64 = 10;

const _: () = assert!(
    FRAME_MS == audiohub_net::media::FRAME_MS,
    "the frame length disagrees with the one audiohub-net derives its jitter-buffer thresholds \
     from; every ms<->frame conversion on one side of that boundary would be off and nothing \
     would report it"
);

const F48: usize = 480; // 48k @ 10ms
const F48_STEREO: usize = F48 * 2;
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

// -------------------------------------------------- 调度迟到直方图（100 ms 以下）
//
// **这是全链路上唯一决定 `jitter_buf` 深度的量，而它此前一个数字都没有。**
//
// `SkipCell` 的判据是 `behind > tick + 10`，即**只有超过 100 ms 的迟到才留痕**；
// 以下的迟到走「背靠背补跑」路径，不写日志、不计数、任何遥测字段里都没有它。
// 而 JB 的整定表（`media.rs` 的 `JbTuning::DEFAULT` 文档）说的正是 20–50 ms
// 这一段：
//
// ```text
// JB 深度 20 ms ⇒ 欠载 3.75 次/分     ⇒ 「>20 ms 的发送端停顿」≈ 1/16 s
// JB 深度 50 ms ⇒ 欠载 0.18 次/分     ⇒ 「>50 ms 的发送端停顿」≈ 1/333 s
// >100 ms 的停顿（SkipCell 实测）      ≈ 1/241 s
// ```
//
// 三点连起来是一条单调下降的尾，但**中间两点是从欠载率反推的，不是测出来的**。
// 反推依赖「一次停顿恰好换一次欠载」这个未经验证的假设。本直方图直接测那条尾。
//
// # 为什么必须先有它，才谈得上削 `jitter_buf`
//
// 发送端停顿 Δ 毫秒 ⇒ 接收端 JB 在 Δ 毫秒里净排空 Δ/10 帧（接收端 `mixer_loop`
// 无论如何每 10 ms `pop()` 一次）。**不欠载的充要条件是 `JB 深度 ≥ Δ`**，
// 单位是毫秒。所以 `min_target` 该取多少，等价于问「停顿尾的 p99.9 是多少」。
// 网络抖动统计量（RFC 3550 一阶差分 EWMA，实测 p95 = 0.18 ms）**看不见这件事**：
// EWMA 把 1600 个包里的一个尖峰平均掉了。判据和被判的量不是一回事。
//
// # 两条循环量的**不是同一个东西**（2026-08-04 实测后修正）
//
// 同一个 `LateCell` 类型，两个测点，两套语义。混为一谈会读出相反的结论：
//
// | | `TX_LATE`（`tx_loop`） | `MIX_LATE`（`mixer_loop`） |
// |---|---|---|
// | 测点 | 循环顶部，**等待之前** | `sleep` **之后**（`sleep_until`） |
// | 量的是 | 「这一 tick 相对计划整体推迟了多久」= 上一 tick 的活 + 抢占 − 一个 tick | 「醒来时刻比计划晚了多久」= 纯唤醒过冲 |
// | 服务于 | `jitter_buf` 深度（对端 JB 净排空 Δ/10 帧，语义就是整 tick） | `play_ring` 目标水位里那 5 ms `margin` |
// | 死区 | **一整个 tick（10 ms）** —— 这正是它要的 | **无** |
// | 典型量级 | mac 现场 6.3 h：max 126.7 ms、均值 4.7 ms/tick | 30-win 探针：p50 0.45 / p99.9 1.21 / max 1.67 ms |
//
// ## `MIX_LATE` 此前的测点错在哪：**一个 10 ms 的死区，恰好盖住了要量的东西**
//
// 它曾经也在等待之前测。`tick` 在循环末尾 +1，所以那个位置量到的是
//
// ```text
// max(0, 上一 tick 的唤醒过冲 + 上一 tick 的活 − 10 ms)
// ```
//
// —— **一个带 10 ms 死区的「超支」指标**，只在上一 tick 的活撑破了整个 tick 时才动。
//
// ⚠ **不要把它说成「恒等于 0」**：mac 现场跑 6.3 h 的真实读数是
// `mixer.max_us = 64737`（64.7 ms），它记的是真实发生过的**卡顿**。
// 30-win 探针那 27000 个 tick 之所以**全是 0**，是因为探针的 tick 里**没有活**
// （`docs/spec-playdev-measurement.md` §4.4）。
//
// 真正的缺陷是**量程错位**：`margin` 关心的是唤醒过冲，实测量级 0.02–1.67 ms，
// **整个落在那 10 ms 死区里面**，所以旧测点在原理上就看不见它。
// 判据是「回调那一刻环里够不够 `block`」——回调迟到多少就吃掉多少余量，
// 与「整 tick 有没有超支」无关。
//
// 后果是**双向**的误读，两边都错：
// - 读到 `max_us = 0`（空闲机）⇒「margin 白留了」——实测需要 2×1.665 = 3.33 ms，现有 5 ms 是对的；
// - 读到 `max_us = 64737`（mac 现场）⇒「margin 差着 13 倍，赶紧加」——那 64.7 ms 是一次卡顿，
//   卡顿由 `MIX_SKIP` 和 JB 自愈接管，不是 `margin` 该覆盖的量。
//
// 新测点在唤醒之后，**死区没了**，两种量都还在：卡顿时 `sleep_until` 立刻返回全额迟到，
// 准时时返回亚毫秒过冲。⚠ 因此 `late_us_sum / ticks` 的含义变了
// （旧：平均超支，稳态≈0；新：平均唤醒过冲，≈0.45 ms）——**新旧读数不可并列比较**。
//
// `TX_LATE` 的测点**保持不变**：那一处的 10 ms 死区正是对端 JB 要的语义
// （停顿 Δ ⇒ JB 净排空 Δ/10 帧），见其代码处注释。
//
// # 为什么它不违反「测量不许改变被测对象」
//
// 每 tick 的成本是：一次 `saturating_duration_since`、一次至多 11 步的常量数组比较、
// 两次 relaxed 原子加、一次 `fetch_max`。零分配、零锁、零系统调用。
// `tx_loop` 复用循环本来就要取的那个 `Instant::now()`，不新增取时钟；
// `mixer_loop` 在**准时**的那条路径上多取一次时钟（睡醒后必须重新读，否则测的还是睡前），
// 迟到那条路径不多取。`Instant::now()` 是 `mach_absolute_time` / `QueryPerformanceCounter`，
// 数十纳秒，占 10 ms 节拍的 1e-5 —— 如实写在这里，不假装是零。

/// 直方图的桶上界，**毫秒**。边界不是随手取的：`10/20/30/40/50` 正好是
/// JB 深度 1/2/3/4/5 帧所能扛住的停顿长度，所以「累计尾 ≥ 40 ms 的比例」
/// 可以直接读成「`min_target = 4` 时的欠载率上界」。
///
/// ⚠ **这套边界是照 `TX_LATE` 的量程定的。** `MIX_LATE` 量的是唤醒过冲，
/// 30-win 实测全分布落在 0.02–1.67 ms —— 也就是几乎全部挤在第 0 桶（`<1 ms`）。
/// 读 `mixer` 那一条时**桶没有分辨率，要看 `max_us` 与 `late_us_sum / ticks`**
/// （微秒精度，`play_ring` 的 `margin` 判据要的正是 `max_us`）。
/// 不为它单独加一套更细的边界：那要么给 `LateCounters` 加一个变体、要么让两条
/// 循环的 `edges_ms` 不同——前者是类型分叉，后者会让并排读数的人误以为同刻度。
const LATE_EDGES_MS: [u64; 11] = [1, 2, 5, 10, 15, 20, 30, 40, 50, 70, 100];
/// 桶数 = 边界数 + 1（最后一个是 `>100 ms`，与 `SkipCell` 的判据接壤）。
const LATE_BUCKETS: usize = LATE_EDGES_MS.len() + 1;

/// 一条循环的调度迟到分布。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct LateCounters {
    /// 观测到的 tick 总数（分母）。
    pub ticks: u64,
    /// 迟到总量，微秒。`late_us_sum / ticks` = 平均迟到。
    pub late_us_sum: u64,
    /// 迄今最大单次迟到，微秒。
    pub max_us: u64,
    /// 每桶的 tick 数，上界见 [`LATE_EDGES_MS`]，最后一个桶是 `>100 ms`。
    pub buckets: [u64; LATE_BUCKETS],
    /// 桶上界的副本，让读的人不必去翻源码对齐语义。
    pub edges_ms: [u64; 11],
}

#[derive(Debug)]
struct LateCell {
    ticks: AtomicU64,
    late_us_sum: AtomicU64,
    max_us: AtomicU64,
    buckets: [AtomicU64; LATE_BUCKETS],
}

impl LateCell {
    const fn new() -> LateCell {
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        LateCell {
            ticks: AtomicU64::new(0),
            late_us_sum: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            buckets: [Z; LATE_BUCKETS],
        }
    }

    /// 记一个 tick。`late` = 实际唤醒时刻 − 计划时刻（早到记 0）。
    ///
    /// 在 10 ms 音频线程上调用，**必须**保持零分配、零锁、零系统调用。
    #[inline]
    fn record(&self, late: Duration) {
        let us = late.as_micros() as u64;
        self.ticks.fetch_add(1, Ordering::Relaxed);
        if us == 0 {
            // 绝大多数 tick 走这条：早到或准时，一次原子加就够了。
            self.buckets[0].fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.late_us_sum.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
        let ms = us / 1000;
        let mut i = 0;
        while i < LATE_EDGES_MS.len() && ms >= LATE_EDGES_MS[i] {
            i += 1;
        }
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> LateCounters {
        let mut buckets = [0u64; LATE_BUCKETS];
        for (dst, src) in buckets.iter_mut().zip(self.buckets.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        LateCounters {
            ticks: self.ticks.load(Ordering::Relaxed),
            late_us_sum: self.late_us_sum.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
            buckets,
            edges_ms: LATE_EDGES_MS,
        }
    }
}

static TX_LATE: LateCell = LateCell::new();
static MIX_LATE: LateCell = LateCell::new();

/// `tx_loop` 的调度迟到分布（IPC / probe 用）。见 [`LateCell`] 的模块说明。
pub fn tx_late_counters() -> LateCounters {
    TX_LATE.snapshot()
}

/// `mixer_loop` 的**唤醒过冲**分布（IPC / probe 用）。
///
/// 这一条在 **Windows** 上尤其要紧：`play_ring` 的目标水位 `dac + block + margin`
/// 里那 5 ms 的 `margin` 就是留给这条循环迟到的。
///
/// # 怎么读它（2026-08-04 起）
///
/// 看 **`max_us`**，不要看桶——见 [`LATE_EDGES_MS`]。判据是 2× 超订：
/// `需要的 margin = 2 × max`。30-win 独立探针（9000 tick × 3 臂交错）实测
/// max = 1.665 ms ⇒ 需要 3.33 ms，**现有 5 ms 是对的，可削空间只有 1.7 ms**
/// （占 `sum_ms` 的 1.5%），`docs/spec-latency-floor.md` §2.5.3 已据此判定不削。
///
/// # 它此前带一个 10 ms 死区，恰好盖住了要量的东西
///
/// 测点在 `sleep` **之前**，量到的是「上一 tick 的活 + 过冲 − 一个 tick」并钳零
/// —— 一个**带 10 ms 死区的超支指标**。而唤醒过冲实测 0.02–1.67 ms，
/// **整个落在死区里**，所以它在原理上量不到 `margin` 关心的东西。
///
/// 现测点在 [`sleep_until`] 内、唤醒之后，死区没了。⚠ **新旧读数不可并列比较**：
/// `late_us_sum / ticks` 的含义从「平均超支（稳态≈0）」变成「平均唤醒过冲（≈0.45 ms）」。
/// 判定与证据见 `docs/spec-playdev-measurement.md` §4.4 与 [`LateCell`] 上方的对照。
pub fn mixer_late_counters() -> LateCounters {
    MIX_LATE.snapshot()
}

/// 睡到 `deadline`，返回**唤醒之后实测**的迟到量（早到不可能，见下）。
///
/// 存在的唯一理由是把「等待」和「量迟到」绑成一个不可拆开的动作。
/// 拆开写过一次，测点落在了 `sleep` 之前 —— 那等于给指标加了一个 10 ms 死区，
/// 而要量的唤醒过冲（0.02–1.67 ms）整个在死区里面
/// （`docs/spec-playdev-measurement.md` §4.4，以及 [`LateCell`] 上方的对照）。
///
/// 两条路径的取时钟次数不同，这是刻意的：
/// - **已经迟到**（`now >= deadline`）：不睡，`now − deadline` 就是答案，不再取第二次；
/// - **准时**：睡到 `deadline`，**再取一次**。`std::thread::sleep` 的契约是「至少睡
///   这么久」，所以醒来时刻严格 ≥ `deadline`，返回值恒 > 0（实测 0.02–1.67 ms）。
///   这一次多出来的时钟读数是这个测点的全部成本，数十纳秒 / 10 ms 节拍。
///
/// `saturating_duration_since` 只是防御性写法：按上面两条，两个分支都不可能为负。
fn sleep_until(deadline: Instant) -> Duration {
    let now = Instant::now();
    if now >= deadline {
        return now.saturating_duration_since(deadline);
    }
    std::thread::sleep(deadline - now);
    Instant::now().saturating_duration_since(deadline)
}

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
#[cfg(target_os = "windows")]
pub(crate) type AudioThreadQosGuard = audio::ProAudioThreadGuard;

#[cfg(not(target_os = "windows"))]
pub(crate) struct AudioThreadQosGuard;

#[cfg(target_os = "macos")]
pub(crate) fn raise_audio_thread_qos(what: &'static str) -> AudioThreadQosGuard {
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
    AudioThreadQosGuard
}

#[cfg(target_os = "windows")]
pub(crate) fn raise_audio_thread_qos(what: &'static str) -> AudioThreadQosGuard {
    let guard = audio::promote_current_thread_to_pro_audio(what);
    if guard.is_active() {
        dlog!("[audiohubd] {what}: MMCSS Pro Audio active");
    } else {
        dlog!("[audiohubd] {what}: MMCSS Pro Audio unavailable; using default priority");
    }
    guard
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn raise_audio_thread_qos(_what: &'static str) -> AudioThreadQosGuard {
    AudioThreadQosGuard
}

#[cfg(target_os = "macos")]
pub(crate) fn raise_media_send_thread_qos(what: &'static str) -> AudioThreadQosGuard {
    raise_audio_thread_qos(what)
}

#[cfg(target_os = "windows")]
pub(crate) fn raise_media_send_thread_qos(_what: &'static str) -> AudioThreadQosGuard {
    AudioThreadQosGuard::inactive()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn raise_media_send_thread_qos(_what: &'static str) -> AudioThreadQosGuard {
    AudioThreadQosGuard
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

// ------------------------------------------- UDP 发送：搬出截止期线程（J1-1）
//
// `sendto()` 进内核网络栈：路由查找、邻居解析、qdisc 排队、socket 发送缓冲
// 满时的等待 —— **单次调用的耗时上界不可预知**。`engine.rs` 里那段拒绝申报
// `THREAD_TIME_CONSTRAINT_POLICY` 的论证（见 `raise_audio_thread_qos`）说的
// 正是这件事：给不出能诚实填进 `computation` 的上界，是因为 `sendto` 在这条
// 线程上。把它搬走，拒绝的理由自己就消失了。
//
// # 队列的三个选项，以及为什么选了第三个
//
// | 选项 | 为什么不行 / 行 |
// |---|---|
// | **有界 + 满了阻塞** | 生产者又会被同一条尾巴按住。**白搬。** |
// | **无界** | ① 内存无上界；② 更要命的是**语义**：发送线程卡 10 秒之后会把 1000 个陈包**成串**灌给对端，而对端 JB 早已把那些 seq 判成迟到并推进过去（`media.rs` 的 `pop()` 每 10 ms 推一格）。那正是治法 A 存在的理由——一次卡顿不该变成一串永久的陈音频。 |
// | **有界 + 满了丢最新并计数**（本实现） | 见下 |
//
// # 丢弃为什么是**对**的，而不是「只好丢」
//
// 队列只有在发送线程卡住时才非空。卡住 Δ 毫秒 ⇒ 对端 JB 在 Δ 里净排空 Δ/10
// 帧；`SEND_SLOTS = 128` 个槽在单流下是 1.28 s、16 条流下是 80 ms，
// **两个数都远超今天 50 ms 的 JB 深度**（`docs/spec-latency-floor.md` §9.1）。
// 也就是说：**能溢出的时候，对端早就欠载了**，队列里那些包送不送到已经不影响
// 「听不听得见断」，只影响「断完之后是不是还要再听一段陈的」。
//
// 而且这不是新行为：今天 `inner.udp.send_to(..).is_ok()` 里那个 `Err` 分支
// **本来就是静默丢弃**（socket 缓冲满、EHOSTUNREACH 都走它，一个计数器都没有）。
// 本实现把同一类事件搬到用户态并**数出来** —— 观测性是净增的。
//
// # 丢最新还是丢最旧
//
// 丢**最新**。`SpscRing` 的不变量是「只有消费者动 `read`」，丢最旧要生产者去
// 推 `read`，那等于把 SPSC 契约作废换取几十毫秒的新鲜度——而上一段刚证明这
// 几十毫秒落在「已经欠载」的区间里，不值得拿契约去换。

/// 一个待发数据报。`buf` 在建队时一次性分配，之后只被就地改写。
struct SendSlot {
    buf: Vec<u8>,
    dest: SocketAddr,
    /// Plaintext payload length of this datagram, i.e. `buf.len()` minus header
    /// and AEAD tag.
    ///
    /// It has to ride along in the slot because the only honest place to count
    /// it is where `send_to` returned `Ok` — the same criterion `sent_bytes`
    /// already uses. Counting it at encode time would silently redefine the
    /// number from "went out" to "was queued".
    payload_len: usize,
    /// 发成功之后要记账的那条流。**由消费者 `take()` 走**，于是这个 `Arc` 的
    /// 引用计数递减（以及可能触发的 `TxShared` 析构）落在发送线程上，
    /// 不在截止期线程上。
    owner: Option<Arc<TxShared>>,
}

/// 队列深度（数据报）。必须是 2 的幂（[`SpscRing`] 的硬约束）。
const SEND_SLOTS: usize = 128 * audiohub_net::media::MAX_WIRE_PARTS;

/// 每个槽预留的字节数，**按阶梯最深档推导**：
/// 40 B 头 + 480 样本 × 4 B（`WireDepth::F32`）+ 16 B AEAD 标签 = 1976，
/// 取整到 2048 留一点富余。全部 128 × 2048 = 256 KB，进程启动时一次性分配。
///
/// # 为什么必须按**不分包**的帧长留（而不是按 5 ms 分包后的 1016 B）
///
/// 深档在线上按 5 ms 分包，每个数据报只有 1016 B —— 但**分包是可回退的**
/// （`docs/design-bitdepth-ladder.md` §12「回退面」）。按分包后的尺寸掐紧，
/// 回退那天就会重演下面这个失效。
///
/// # 这行数字曾经写死在 s16 上（第三颗地雷）
///
/// 槽的 `buf` 是 `Vec::with_capacity(SEND_SLOT_BYTES)`，`seal_into` 会
/// `reserve` 并**扩容**，所以不会截断——但扩容是一次 `malloc`，而那个闭包跑在
/// **`tx_loop` 这条 10 ms 截止期线程上**，正是 J1 零分配纪律要消灭的东西。
/// 失效形态：切到深档的**那一瞬间**，环里 128 个槽各扩容一次 = 128 次 malloc
/// 撒在截止期线程上，可能撞上分配器的 magazine refill 锁。
///
/// 守门测试 `the_send_slots_stop_allocating_after_the_first_lap` 的 payload
/// 因此必须是**最深档的实际帧长**，不是一个随手写的 1000。
const SEND_SLOT_BYTES: usize = 2048;

/// 阶梯最深档一帧的**密文**长度（含包头与 AEAD 标签），[`SEND_SLOT_BYTES`] 的下界。
///
/// 单独提出来是给守门测试用的：让它拿真实的最深帧长去跑，而不是一个字面量。
pub(crate) const DEEPEST_SEALED_FRAME_BYTES: usize = audiohub_net::packet::HEADER_LEN
    + audiohub_net::media::LADDER[0].frame_bytes()
    + audiohub_net::media::AEAD_TAG_LEN;

// **编译期**钉住这条不变量。
//
// 只靠运行期守门测试不够：那条测试比的是「第二圈与第一圈用的是同一块内存」，
// 而容量不够时**第一圈就已经全部扩容完了**，第二圈起自然稳定 —— 于是它对
// 「容量比最深档小」这件事完全免疫（实测：把 SEND_SLOT_BYTES 改回 1152，
// 那条测试照样绿）。真正的判据是「初始容量就够」，那是一个常量关系，
// 应当在编译期回答。
const _: () = assert!(
    SEND_SLOT_BYTES >= DEEPEST_SEALED_FRAME_BYTES,
    "发送槽的初始容量装不下最深档：切档瞬间 128 个槽各 malloc 一次，全撒在 10 ms 截止期线程上"
);

/// 发送线程空转时的兜底超时。
///
/// 稳态下用不到：生产者每 tick 入队完毕会 `unpark` 一次。它挡的是「唤醒丢了」
/// 这一种理论情形，以及关机时的响应延迟。**不能把它当成轮询周期**——轮询周期
/// 会原样变成 `network` 一级的抖动，而那正是 JB 深度要覆盖的东西。
const SEND_IDLE_BACKSTOP: Duration = Duration::from_millis(20);

/// 媒体发送队列。**恰好一个生产者（`tx_loop`）、恰好一个消费者
/// （[`udp_send_loop`]）** —— 这是 [`SpscRing`] 的安全前提，不是风格问题。
///
/// `send_pullreq`（ticker 线程）仍然直接 `inner.udp.send_to`：它 1 Hz、
/// 不在任何截止期线程上，而 UDP socket 本身是多线程安全的。**它不走这个队列**
/// ——走了就会变成第二个生产者，直接违反 SPSC 契约。
pub(crate) struct UdpSender {
    q: SpscRing<SendSlot>,
    revoked_dropped: AtomicU64,
    /// 发送线程句柄。`OnceLock` 而不是 `Mutex`：生产者每 tick 要读它一次，
    /// 那里不许有锁。
    thread: std::sync::OnceLock<std::thread::Thread>,
    /// 消费者是不是正打算/正在 park。生产者据此决定要不要付那次 `unpark`。
    parked: AtomicBool,
}

impl UdpSender {
    pub(crate) fn new() -> UdpSender {
        UdpSender {
            q: SpscRing::new(SEND_SLOTS, |_| SendSlot {
                buf: Vec::with_capacity(SEND_SLOT_BYTES),
                dest: SocketAddr::from(([0, 0, 0, 0], 0)),
                payload_len: 0,
                owner: None,
            }),
            revoked_dropped: AtomicU64::new(0),
            thread: std::sync::OnceLock::new(),
            parked: AtomicBool::new(false),
        }
    }

    /// 把一个数据报排队。`fill` 就地把字节写进槽的缓冲；返回 `false` 表示封包
    /// 失败，这一条作废（消费者永远看不到它）。
    ///
    /// **零分配、零锁、零系统调用。** 返回 `false` 也可能是队列满（已计数）。
    fn enqueue(
        &self,
        dest: SocketAddr,
        owner: &Arc<TxShared>,
        payload_len: usize,
        fill: impl FnOnce(&mut Vec<u8>) -> bool,
    ) -> bool {
        if !owner.may_transmit() { return false; }
        self.q.produce(|slot| {
            if !fill(&mut slot.buf) {
                return false;
            }
            slot.dest = dest;
            slot.payload_len = payload_len;
            slot.owner = Some(owner.clone());
            true
        })
    }

    /// 叫醒发送线程。**每 tick 至多一次**（把这一 tick 全部流的包排完之后），
    /// 不是每个包一次。
    ///
    /// 代价如实写在这里：Darwin 上 `Thread::unpark` 在对方确实 park 了的时候是
    /// 一次 `pthread_mutex_lock` + `__psynch_cvsignal`，微秒级、**有界**、
    /// 不进网络栈、不做 I/O。这是本轮唯一保留在截止期线程上的系统调用，
    /// 因为替代方案（发送线程轮询）会把轮询周期原样变成媒体路径的抖动。
    fn wake(&self) {
        // `produce` 里发布槽用的是 Release store，而这里要读的是另一个原子。
        // 没有这道 SeqCst 栅栏，「发布 → 读 parked」与消费者的「置 parked →
        // 复查队列」之间就不构成全序，两边可能各自看到旧值 ⇒ 丢一次唤醒。
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) {
            if let Some(t) = self.thread.get() {
                t.unpark();
            }
        }
    }

    /// 此刻排着的数据报数（诊断用）。
    pub(crate) fn queued(&self) -> usize {
        self.q.len()
    }

    /// 累计因队列满而丢掉的数据报数（诊断用）。
    ///
    /// 这里的「拒收」就是「丢弃」：`enqueue` **不重试**（重试就等于阻塞，
    /// 而阻塞正是本次搬家要消灭的东西）。
    pub(crate) fn dropped(&self) -> u64 {
        self.q.rejected()
    }

    /// 队列容量（诊断用，让读数的人不必翻源码）。
    pub(crate) fn capacity(&self) -> usize {
        self.q.capacity()
    }

    pub(crate) fn revoked_dropped(&self) -> u64 {
        self.revoked_dropped.load(Ordering::Relaxed)
    }
}

fn transmit_if_current(owner: Option<&Arc<TxShared>>, send: impl FnOnce() -> bool) -> Option<bool> {
    if owner.is_some_and(|owner| !owner.may_transmit()) { return None; }
    Some(send())
}

/// 媒体发送线程：把 `sendto` 从 10 ms 截止期线程上接过来。
///
/// 计数器（`sent_packets` / `sent_bytes`）也在这里加，与搬家之前**逐字相同**
/// 的判据：只有 `send_to` 返回 `Ok` 才算。在入队处加会把语义从「内核收下了」
/// 悄悄改成「排上队了」。
///
/// `sent_payload_bytes` follows the same criterion. It is a second counter and
/// not a division of `sent_bytes` by some assumed framing size: the header is
/// variable-length, so any such division would be a guess that no display could
/// detect being wrong.
pub(crate) fn udp_send_loop(inner: Arc<DaemonInner>) {
    // 与 tx/mixer 同一档 QoS：这条线程现在在媒体路径上，被降档就等于把刚搬走
    // 的延迟原样搬回来。它每 tick 只做一次 `sendto`，抢不走什么。
    let _qos_guard = raise_media_send_thread_qos("udp_send_loop");
    let _ = inner.media_send.thread.set(std::thread::current());
    // Read once, outside the loop, like `rx_loop`'s half of the same hook.
    let block_out = inner.udp_block.out;
    loop {
        while inner.media_send.q.consume(|slot| {
            let owner = slot.owner.take(); // 在**本线程**析构
                                           // The counters still advance when the hook is swallowing the
                                           // datagram, and that is the whole point: a firewall drops packets
                                           // the kernel already accepted, so `sent_packets` climbs on a
                                           // blocked link exactly as it does on a working one. Skipping the
                                           // accounting too would make the simulated link differ from the real
                                           // one in precisely the variable the keepalive signal reads
                                           // (`autotier`'s `sent_packets > 0` guard), and that signal would
                                           // then be untestable through this hook.
            let Some(accepted) = transmit_if_current(owner.as_ref(), || block_out || inner.udp.send_to(&slot.buf, slot.dest).is_ok()) else {
                inner.media_send.revoked_dropped.fetch_add(1, Ordering::Relaxed);
                return;
            };
            if accepted {
                if let Some(o) = owner {
                    o.sent_packets.fetch_add(1, Ordering::Relaxed);
                    o.sent_bytes
                        .fetch_add(slot.buf.len() as u64, Ordering::Relaxed);
                    o.sent_payload_bytes
                        .fetch_add(slot.payload_len as u64, Ordering::Relaxed);
                }
            }
        }) {}
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // 先置标志再复查队列，与 `UdpSender::wake` 的「先发布再读标志」配对：
        // 两边都用 SeqCst，于是「生产者没看到 parked」蕴含「消费者会看到那一条」。
        // 少了这一次复查就是经典的丢唤醒——表现是偶尔一个包晚 20 ms 到，
        // 而那 20 ms 会以抖动的形式落在对端 JB 上。
        inner.media_send.parked.store(true, Ordering::SeqCst);
        if inner.media_send.q.len() == 0 {
            std::thread::park_timeout(SEND_IDLE_BACKSTOP);
        }
        inner.media_send.parked.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- tx engine

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SourceSpec {
    Tone {
        freq_bits: u32,
    },
    Mic,
    /// What this machine is playing (spec-m4b §B2). The backend id is part of
    /// the dedup key: two streams naming different backends are two captures.
    SysAudio {
        backend: String,
    },
    /// What an application played into ONE peer's virtual speaker (spec-m5b
    /// §5.4). The slot is part of the dedup key, so each speaker ring gets
    /// exactly one consumer entry — which is what keeps the halbridge SPSC rule
    /// (exactly one reader per ring) literally true with sixteen of them.
    ///
    /// Collapsing this back to a slot-less variant is the single most dangerous
    /// simplification available here: every peer's audio would come out of one
    /// ring, every positive test would still pass, and the only symptom would
    /// be one peer hearing another's audio.
    HalSpeaker {
        slot: u8,
    },
}

impl SourceSpec {
    pub(crate) fn tone(freq: f32) -> SourceSpec {
        SourceSpec::Tone {
            freq_bits: freq.to_bits(),
        }
    }

    fn label(&self) -> String {
        match self {
            SourceSpec::Tone { freq_bits } => format!("tone {}Hz", f32::from_bits(*freq_bits)),
            SourceSpec::Mic => "mic".to_string(),
            SourceSpec::SysAudio { backend } => format!("sysaudio '{backend}'"),
            SourceSpec::HalSpeaker { slot } => format!("hal speaker slot {slot}"),
        }
    }

    pub(crate) fn preferred_channels(&self) -> u8 {
        match self {
            SourceSpec::Tone { .. } | SourceSpec::Mic => 1,
            SourceSpec::SysAudio { .. } | SourceSpec::HalSpeaker { .. } => 2,
        }
    }
}

pub(crate) enum TxCmd {
    Add {
        stream_id: u32,
        key: [u8; 32],
        /// Per-stream media salt from the stream opener (frozen API).
        salt: Vec<u8>,
        /// Where this stream's media goes: a UDP destination (tier 0) or this
        /// peer's media TCP link (tier 1). Read from `ConnShared` once, when
        /// the stream is created — see `ConnShared::current_media_path`.
        path: MediaPath,
        spec: SourceSpec,
        /// Negotiated interleaved media channels (1 with a 1.0.0 peer).
        channels: u8,
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
            hits.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
    let _ = lk(&inner.mix_cmds).send(MixCmd::ReleaseBridge {
        device: device.to_string(),
    });
}

struct TxStream {
    id: u32,
    crypto: MediaCrypto,
    /// This stream's media path, taken at creation and never changed
    /// afterwards. On [`MediaPath::Udp`] `dest_override` may still move the
    /// port (see `refresh_dest`); on [`MediaPath::Tcp`] there is no address to
    /// move, which is the point of the enum.
    path: MediaPath,
    spec: SourceSpec,
    loss: LossInjector,
    seq: u32,
    /// Logical media frames for this stream, independent of scheduler ticks.
    /// Advances exactly when `seq` consumes one frame's packet positions;
    /// format-alignment padding and tx-loop catch-up do not advance it.
    media_frame_seq: u64,
    rung: u32,
    channels: u8,
    rs: Option<InterleavedLinearResampler>, // 48k -> rung rate
    rs_last: [f32; 2],                      // last source frame; seeds the next resampler
    /// 这一帧（或半帧）的线上载荷。**长期复用**：`dsp::encode_pcm` 每 tick 每流
    /// 分配一个 `Vec` 是本轮第 3 项要消灭的东西。
    ///
    /// ⚠ 容量随**格号**变，不是「第一帧之后不再变」：换档同时改帧长度与每样本
    /// 字节数（48 kHz/16 bit 是 960 B，48 kHz/32f 是 1920 B）。`encode_pcm_into`
    /// 用 `reserve` 而不是断言容量，正是为了这一步。
    pay: Vec<u8>,
    /// 上一次读到的 `TxShared::dest_epoch`。
    ///
    /// 存在的理由见 `TxShared::dest_epoch` 的文档：稳态下把「每 tick 一次
    /// `Mutex`」换成「每 tick 一次 relaxed load」。0 = 还没看过，所以在这条流
    /// 建起来**之前**就学到的地址不会漏掉。
    dest_epoch_seen: u64,
    /// plan §7.2 发送侧软件增益的**斜坡与 dither 状态**。
    ///
    /// **在这里，不在 `SourceEnt` 上。** 源是扇出的（一份 `frame` 发给 N 条流），
    /// 增益是对端属性（N 个对端可以有 N 个音量）。挂到源上不是「串台一次」，
    /// 而是**逐帧复利** —— 0.5 的增益在第 k 帧上累成 0.5ᵏ，而包数、丢包率、
    /// 音调探针全绿。目标值来自 `shared.send_gain`，这里只有执行状态。
    gain: dsp::SendGain,
    shared: Arc<TxShared>,
}

struct SourceEnt {
    src: Src,
    channels: u8,
    refs: usize,
    /// 造出这个源的那次建源请求的代号。收尸时要原样带回去，好让建源线程
    /// 精确丢掉**配套的**那个 `LiveCapture`（设备变更重建期间，同一个 spec
    /// 会短暂存在新旧两份）。
    gen: u64,
    frame: Vec<f32>, // one 48k frame per tick, broadcast to all attached streams
    frame_valid: bool,
    /// 本 tick 读到的各级深度，随 `frame` 一起广播给挂在这个源上的每条流。
    /// 读一次、发 N 份：物理队列只有一份（规格 §7.2 R8）。
    depths: SourceDepths,
}

fn validate_transmit_layout(
    shared: &TxShared,
    requested_channels: u8,
    source_channels: u8,
    source_layout: Option<audiohub_core::spatial_output::SpeakerLayout>,
) -> Result<()> {
    if let Some(contract) = &shared.spatial {
        contract.validate()?;
        if requested_channels != contract.channels() || source_channels != contract.channels()
            || source_layout != Some(contract.layout)
        {
            return Err(anyhow!("source layout does not match the accepted spatial PCM contract"));
        }
    } else if source_channels > 2 || source_layout.is_some() {
        return Err(anyhow!("wide source requires an explicit spatial PCM contract"));
    }
    Ok(())
}

fn send_spatial_frame(
    udp: &UdpSender,
    tx: &mut TxStream,
    source: &SourceEnt,
    tick_at: Instant,
    timestamp_us: u64,
    gained: &mut Vec<f32>,
) -> bool {
    use audiohub_net::spatial_media::{encode_fragment, fragment_count};
    if !tx.shared.may_transmit() {
        return false;
    }
    let contract = tx.shared.spatial.as_ref().expect("spatial transmit branch");
    let layout = contract.layout;
    let channels = contract.channels();
    if let Err(error) = validate_transmit_layout(&tx.shared, tx.channels, source.channels, source.src.speaker_layout()) {
        tx.shared.fail_media(error.to_string());
        return false;
    }
    if source.src.sample_rate() != 48_000 || !source.frame_valid || source.frame.len() != F48 * channels as usize
        || source.frame.iter().any(|value| !value.is_finite())
    {
        tx.shared.fail_media("spatial source did not provide one complete finite 48 kHz frame".into());
        return false;
    }
    if tx.shared.transport.quality_rung().is_some_and(|rung| rung != 0) {
        tx.shared.fail_media("selected quality is incompatible with the fixed 48 kHz/F32 spatial contract".into());
        return false;
    }
    let parts = fragment_count(layout);
    let Some(next_seq) = tx.seq.checked_add(parts as u32) else {
        tx.shared.fail_media("spatial packet nonce space requires a fresh stream".into());
        return false;
    };
    let Some(next_frame) = tx.media_frame_seq.checked_add(1) else {
        tx.shared.fail_media("spatial frame sequence requires a fresh stream".into());
        return false;
    };
    if tx.loss.should_drop() {
        tx.seq = next_seq;
        tx.media_frame_seq = next_frame;
        return false;
    }
    refresh_dest(tx);
    tx.gain.set_target(TxShared::gain_of(tx.shared.send_gain.load(Ordering::Relaxed)));
    let samples = tx.gain.apply_interleaved(&source.frame, 48_000, dsp::WireDepth::F32, channels, gained);
    let mut queued_any = false;
    for part in 0..parts {
        if !tx.shared.may_transmit() { break; }
        if let Err(error) = encode_fragment(layout, tx.media_frame_seq, part, samples, &mut tx.pay) {
            tx.shared.fail_media(format!("encode spatial PCM: {error:#}"));
            return queued_any;
        }
        let header = Header {
            kind: Kind::Media, codec: Codec::PcmF32le, channels, sample_rate: 48_000,
            session_id: tx.id as u64, stream_id: tx.id, seq: tx.seq,
            timestamp_us, payload_len: 0,
        };
        tx.seq += 1;
        let seal = |buffer: &mut Vec<u8>| match tx.crypto.seal_into(&header, &tx.pay, buffer) {
            Ok(()) => true,
            Err(error) => { tx.shared.fail_media(format!("seal spatial media: {error:#}")); false }
        };
        queued_any |= match &tx.path {
            MediaPath::Udp(destination) => udp.enqueue(*destination, &tx.shared, tx.pay.len(), seal),
            MediaPath::Tcp(_) | MediaPath::Framed(_) => tx.path.media_link()
                .is_some_and(|link| link.enqueue(tick_at, &tx.shared, tx.pay.len(), seal)),
        };
    }
    if let Some(link) = tx.path.media_link() {
        link.wake();
    }
    tx.media_frame_seq = next_frame;
    queued_any
}

#[cfg(test)]
mod spatial_transmit_tests {
    use super::*;
    use audiohub_core::spatial_output::SpeakerLayout;
    use audiohub_net::spatial_media::{fragment_count, SpatialMediaContract, SpatialReassembler};

    struct LayoutSource { layout: SpeakerLayout }
    impl FrameSource for LayoutSource {
        fn sample_rate(&self) -> u32 { 48_000 }
        fn channels(&self) -> u8 { self.layout.channels() as u8 }
        fn speaker_layout(&self) -> Option<SpeakerLayout> { Some(self.layout) }
        fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
            out.clear();
            out.resize(F48 * self.layout.channels(), 0.0);
            true
        }
    }

    fn fixture(layout: SpeakerLayout) -> (Arc<TxShared>, TxStream, SourceEnt) {
        let shared = Arc::new(TxShared::new_on(0).with_spatial_contract(SpatialMediaContract {
            version: 1, provider_revision: 1, endpoint_id: "provider-output".into(),
            active_format: "native-format".into(), layout,
        }).unwrap());
        let mut tx = super::tests::tx_stream_for(&shared);
        tx.channels = layout.channels() as u8;
        tx.rung = 0;
        let source = SourceEnt {
            src: Src::Frame(Box::new(LayoutSource { layout })), channels: tx.channels,
            refs: 1, gen: 1, frame: (0..F48 * layout.channels()).map(|n| n as f32 / 10_000.0).collect(),
            frame_valid: true, depths: NO_DEPTHS,
        };
        (shared, tx, source)
    }

    fn drain(sender: &UdpSender, tx: &TxStream, layout: SpeakerLayout) -> Vec<f32> {
        let mut assembly = SpatialReassembler::new(layout);
        let mut frames = Vec::new();
        let mut count = 0;
        while sender.q.consume(|slot| {
            let (header, payload) = tx.crypto.open(&slot.buf).unwrap();
            assert_eq!(header.channels, layout.channels() as u8);
            assert_eq!(header.codec, Codec::PcmF32le);
            assert_eq!(header.sample_rate, 48_000);
            assert_eq!(header.timestamp_us, 1_234_567);
            assert_eq!(header.seq, count);
            assert!(payload.len() <= 1200);
            assert!(Arc::ptr_eq(slot.owner.as_ref().unwrap(), &tx.shared));
            frames.extend(assembly.push(&payload).unwrap());
            count += 1;
        }) {}
        assert_eq!(count as usize, fragment_count(layout));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_seq, 0);
        frames.remove(0).samples
    }

    #[test]
    fn spatial_transmit_queues_authenticated_complete_frames_for_every_layout() {
        for layout in [SpeakerLayout::Surround51, SpeakerLayout::Surround71, SpeakerLayout::Immersive714] {
            let (_shared, mut tx, source) = fixture(layout);
            let sender = UdpSender::new();
            assert!(send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
            assert_eq!(drain(&sender, &tx, layout), source.frame);
            assert_eq!(tx.seq as usize, fragment_count(layout));
            assert_eq!(tx.media_frame_seq, 1);
        }
    }

    #[test]
    fn spatial_transmit_never_sends_before_acceptance_or_after_failure() {
        let (shared, mut tx, source) = fixture(SpeakerLayout::Surround51);
        let sender = UdpSender::new();
        shared.armed.store(false, Ordering::Release);
        assert!(!send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        assert_eq!(tx.seq, 0);
        shared.fail_media("provider contract changed".into());
        shared.armed.store(true, Ordering::Release);
        assert!(!send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        assert!(!sender.q.consume(|_| panic!("unexpected packet")));
    }

    #[test]
    fn spatial_transmit_gain_uses_one_gain_for_all_channels_in_each_frame() {
        let (shared, mut tx, mut source) = fixture(SpeakerLayout::Immersive714);
        source.frame.fill(0.5);
        shared.send_gain.store(TxShared::gain_bits(0.0), Ordering::Relaxed);
        let sender = UdpSender::new();
        assert!(send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        let samples = drain(&sender, &tx, SpeakerLayout::Immersive714);
        for frame in samples.chunks_exact(12) { assert!(frame.iter().all(|sample| *sample == frame[0])); }
        assert!(samples[0] > *samples.last().unwrap());
    }

    #[test]
    fn spatial_transmit_tracks_dropped_frame_identity_and_stops_before_nonce_wrap() {
        let (shared, mut tx, source) = fixture(SpeakerLayout::Surround71);
        let sender = UdpSender::new();
        tx.loss = LossInjector::new(7, 100.0);
        assert!(!send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        assert_eq!(tx.seq as usize, fragment_count(SpeakerLayout::Surround71));
        assert_eq!(tx.media_frame_seq, 1);
        tx.seq = u32::MAX - 1;
        assert!(!send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        assert!(shared.media_failed.load(Ordering::Acquire));
        assert!(!sender.q.consume(|_| panic!("unexpected packet")));
    }

    #[test]
    fn spatial_transmit_refuses_unlabelled_or_changed_source_geometry() {
        let (shared, mut tx, mut source) = fixture(SpeakerLayout::Immersive714);
        assert!(validate_transmit_layout(&shared, 12, 12, None).is_err());
        assert!(validate_transmit_layout(&shared, 2, 12, Some(SpeakerLayout::Immersive714)).is_err());
        source.frame.pop(); source.frame_valid = false;
        let sender = UdpSender::new();
        assert!(!send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        assert!(shared.media_failed.load(Ordering::Acquire));
        assert!(!sender.q.consume(|_| panic!("unexpected packet")));
    }

    #[test]
    fn spatial_source_admission_failure_releases_its_source_reference() {
        let (builder, retired) = mpsc::channel();
        let mut state = TxState::new(builder);
        let spec = SourceSpec::HalSpeaker { slot: 0 };
        let (shared, _tx, mut source) = fixture(SpeakerLayout::Immersive714);
        source.src = Src::Frame(Box::new(ToneSource::new(440.0, 0.1, 48_000, 10)));
        source.channels = 1;
        source.refs = 0;
        state.sources.insert(spec.clone(), source);
        let (ack, answer) = mpsc::channel();
        apply_txcmd(&mut state, TxCmd::Add {
            stream_id: 7, key: [0; 32], salt: vec![0; 16],
            path: MediaPath::Udp("127.0.0.1:1".parse().unwrap()), spec,
            channels: 12, loss_pct: 0.0, shared, ack: Some(ack),
        });
        assert!(answer.recv().unwrap().is_err());
        assert!(state.streams.is_empty() && state.sources.is_empty());
        assert!(matches!(retired.recv().unwrap(), BuildReq::Retire { .. }));
    }

    #[test]
    fn spatial_queued_udp_packets_are_revoked_when_the_source_is_removed() {
        let (shared, mut tx, source) = fixture(SpeakerLayout::Immersive714);
        let sender = UdpSender::new();
        assert!(send_spatial_frame(&sender, &mut tx, &source, Instant::now(), 1_234_567, &mut Vec::new()));
        let (builder, _retired) = mpsc::channel();
        let mut state = TxState::new(builder);
        state.sources.insert(tx.spec.clone(), source);
        state.streams.insert(tx.id, tx);
        state.remove_stream(7);
        assert!(!shared.may_transmit());
        let mut dropped = 0;
        while sender.q.consume(|slot| {
            let owner = slot.owner.take();
            assert_eq!(transmit_if_current(owner.as_ref(), || panic!("revoked audio reached the socket")), None);
            dropped += 1;
        }) {}
        assert_eq!(dropped, 20);
        assert_eq!(shared.sent_packets.load(Ordering::Relaxed), 0);
        assert!(!sender.enqueue("127.0.0.1:1".parse().unwrap(), &shared, 1, |_| panic!("revoked producer encoded audio")));
        assert!(super::tests::fn_body("pub(crate) fn udp_send_loop(").contains("transmit_if_current("));
    }

    #[test]
    fn spatial_revocation_during_enqueue_is_caught_before_socket_io() {
        let (owner, _tx, _source) = fixture(SpeakerLayout::Surround51);
        let sender = UdpSender::new();
        assert!(sender.enqueue("127.0.0.1:1".parse().unwrap(), &owner, 1, |buffer| {
            buffer.clear(); buffer.push(1);
            owner.revoke_media();
            true
        }));
        assert!(sender.q.consume(|slot| {
            let owner = slot.owner.take();
            assert_eq!(transmit_if_current(owner.as_ref(), || panic!("revoked packet reached socket IO")), None);
        }));
        assert!(!sender.enqueue("127.0.0.1:1".parse().unwrap(), &owner, 1, |_| panic!("a later fragment was encoded")));
    }
}

/// A media source plus the one thing `FrameSource` cannot express: a system
/// capture that has died for good (group C's frozen `SysAudioCapture::failed`).
///
/// `+ Send` 是**承重的**：它就是「设备在别的线程上开、音频线程只拿环」这条
/// 纪律的编译期形态。`MicSource` 之所以 `Send`，正是因为
/// `audiohub_net::media::MicSource` 不再持有 `!Send` 的 cpal 流
/// —— 那个 `LiveCapture` 留在 [`source_builder_loop`] 手里。
/// 把这个 `+ Send` 去掉，本轮的第 5 项就会在下一个人手上悄悄退回去。
pub(crate) enum Src {
    Frame(Box<dyn FrameSource + Send>),
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

    fn channels(&self) -> u8 {
        match self {
            Src::Frame(f) => f.channels().clamp(1, 12),
            Src::Sys(s) => s.channels(),
        }
    }

    fn speaker_layout(&self) -> Option<audiohub_core::spatial_output::SpeakerLayout> {
        match self {
            Src::Frame(source) => source.speaker_layout(),
            Src::Sys(_) => None,
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            Src::Frame(source) => source.sample_rate(),
            Src::Sys(_) => 48_000,
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
/// interleaved f32 at its own rate in irregular backend-sized chunks, while the
/// scheduler wants exactly one 48k frame per tick. Underruns emit silence rather than
/// stalling the cadence — a loopback capture is silent whenever nothing plays.
pub(crate) struct SysAudioFrames {
    cap: Box<dyn SysAudioCapture>,
    backend: String,
    excludes_self: bool,
    raw: Vec<f32>,
    /// 发送 FIFO 与它的速率伺服。
    ///
    /// ⚠ 这里曾是**第三份**逐字相同的裸 `VecDeque` + 定比重采样器
    /// （另两份在 `audiohub-net` 的 `MicSource` / `SysAudioSource`）。
    /// 2026-08-15 给那两份加了速率伺服，**唯独漏了这一份**——而 daemon 的模式 A
    /// 走的正是这一份，`SysAudioSource` 只被 CLI 探针用。结果是单元测试全绿、
    /// 探针实测正常，而 9.6 小时真机 soak 里深度仍以 +9.6 ms/小时 爬升。
    /// 三份合一之后这种「修了但没修到生产路径」不可能再发生。
    fifo: CaptureFifo,
    channels: u8,
}

impl SysAudioFrames {
    fn new(cap: Box<dyn SysAudioCapture>, backend: String, excludes_self: bool) -> SysAudioFrames {
        let rate = cap.sample_rate();
        let channels = cap.channels().clamp(1, 2);
        SysAudioFrames {
            cap,
            backend,
            excludes_self,
            raw: Vec::new(),
            fifo: CaptureFifo::new_channels(rate, FRAME_MS as u32, channels),
            channels,
        }
    }

    /// 只有发送 FIFO 一级：后端自己的内部缓冲从这里读不到，**所以不报**，
    /// 而不是报 0（规格 §7.2 R11 记着这条口径缺口）。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo.len(),
                capacity: self.fifo.capacity(),
                rate: 48_000,
                dropped: Some(self.fifo.dropped()),
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
        self.fifo.drain_skipped(samples)
    }

    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.cap.read(&mut self.raw);
        self.fifo.tick(&self.raw, out);
        true
    }

    fn channels(&self) -> u8 {
        self.channels
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
        || inner.airplay.has_active_session()
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

// ------------------------------------- 建源 / 收尸：搬出截止期线程（J1-5）
//
// **这一条是最可疑的一项。** `apply_txcmd` 跑在 `tx_loop` 的等待循环里，而它
// 会一路调到 `MicSource::open`（开一次 CoreAudio 输入设备）、
// `sysaudio::start_backend`（开一次系统捕获）。那两件事的量级与实测停顿直方图
// 的 110–600 ms **正好对得上**（`docs/spec-latency-floor.md` §1.4）。
// 对称地，`sources.remove()` 让 cpal 流在**同一条线程**上析构——关设备和开设备
// 一样会进 CoreAudio 的服务端往返。
//
// 搬完之后，截止期线程上剩下的只有一次原子交接：请求出去、成品回来、装上。
//
// # 为什么不是「在别的线程上造好整个源再搬过来」这么简单
//
// `cpal::Stream` 在 macOS 上 **`!Send`**。一个持有它的 `MicSource` 根本没法
// 跨线程移动——这正是 `MixCmd::OpenBridge` 当初存在的理由。所以拆的是所有权
// 而不是位置：**开设备的线程留住 `LiveCapture`，音频线程只拿 `AudioRx`**
// （无锁环的消费端，`Send`）。见 `audiohub_net::media::MicSource` 的类型文档。

/// 建源线程的入口消息。
pub(crate) enum BuildReq {
    /// 造一个源。`gen` 由 `tx_loop` 单调发放，用来把成品与请求配对。
    Build { spec: SourceSpec, gen: u64 },
    /// 把一个源的**尸体**交过来析构。带 `gen` 是为了丢掉配套的那个
    /// `LiveCapture`：设备变更重建期间同一个 spec 会短暂有新旧两份，
    /// 按 spec 删就会误杀刚开好的那一个。
    Retire {
        spec: SourceSpec,
        gen: u64,
        src: Src,
    },
}

/// 建源线程的产出。
pub(crate) struct BuildDone {
    pub spec: SourceSpec,
    pub gen: u64,
    pub result: std::result::Result<Src, String>,
}

/// Opens and retires capture sources on their owner thread. Default-output
/// playback follows the same ownership split in [`site_playback_builder_loop`];
/// named bridge outputs remain the separate `apply_mixcmd` exception.
pub(crate) fn source_builder_loop(
    inner: Arc<DaemonInner>,
    reqs: mpsc::Receiver<BuildReq>,
    done: mpsc::Sender<BuildDone>,
) {
    // 开着的 cpal 采集流，按「哪一次请求造的」存。**只有这条线程碰它**。
    let mut caps: HashMap<(SourceSpec, u64), LiveCapture> = HashMap::new();
    loop {
        // 用超时而不是阻塞 `recv`：关机时 `tx_loop` 先退出、通道那头还活着
        // （`DaemonInner` 还被别人持有），死等就永远退不出来。
        let req = match reqs.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if inner.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        match req {
            BuildReq::Build { spec, gen } => {
                let result = build_source(&inner, &spec, gen, &mut caps).map_err(|e| {
                    dlog!("[audiohubd] 建源失败 {}: {e:#}", spec.label());
                    format!("{e:#}")
                });
                if done.send(BuildDone { spec, gen, result }).is_err() {
                    return; // tx_loop 走了
                }
            }
            BuildReq::Retire { spec, gen, src } => {
                // 顺序刻意：先丢源（`SysAudioFrames` 里的捕获在这里析构），
                // 再丢采集流。反过来会让 `MicSource` 有一瞬间读一个已死的环，
                // 虽然无害，但没有理由制造它。
                drop(src);
                caps.remove(&(spec, gen));
            }
        }
    }
}

fn build_source(
    inner: &DaemonInner,
    spec: &SourceSpec,
    gen: u64,
    caps: &mut HashMap<(SourceSpec, u64), LiveCapture>,
) -> Result<Src> {
    Ok(match spec {
        SourceSpec::Tone { freq_bits } => Src::Frame(Box::new(ToneSource::new(
            f32::from_bits(*freq_bits),
            TONE_AMP,
            48000,
            FRAME_MS as u32,
        ))),
        SourceSpec::Mic => {
            // cpal 流留在**这条**线程上；跨线程走的只有 `AudioRx`。
            let (src, cap) =
                MicSource::open(FRAME_MS as u32).context("start microphone capture")?;
            caps.insert((spec.clone(), gen), cap);
            Src::Frame(Box::new(src))
        }
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
            Src::Frame(Box::new(crate::halbridge::HalSpeakerSource::new(
                &hal, *slot,
            )))
        }
    })
}

/// Creates a resampler for the new rate that continues from `last`, so a rung
/// switch mid-stream cannot inject the zero sample (audible click) a fresh
/// resampler would interpolate from.
/// The resampler a send stream needs on `rung`, or `None` when it needs none.
///
/// **The criterion is the sample RATE, not the rung number**: after the
/// bit-depth ladder, rungs 0/1/2 are all 48 kHz, so `rung != 0` would build a
/// pointless 48→48 resampler for two of them. Extracted so that the install
/// path and the rung-switch path cannot disagree about it — they used to be one
/// site because a stream always started on a 48 kHz rung, which stopped being
/// true when AUTO's ceiling became per-transport.
fn resampler_for(rung: u32, channels: u8, last: [f32; 2]) -> Option<InterleavedLinearResampler> {
    let f = audiohub_net::media::rung_format(rung);
    (f.rate_hz != MicSource::OUT_RATE)
        .then(|| seeded_interleaved_resampler(MicSource::OUT_RATE, f.rate_hz, channels, last))
}

fn seeded_interleaved_resampler(
    src_rate: u32,
    dst_rate: u32,
    channels: u8,
    last: [f32; 2],
) -> InterleavedLinearResampler {
    let mut rs = InterleavedLinearResampler::new(src_rate, dst_rate, channels);
    let mut discard = Vec::new();
    rs.process(&last[..channels.clamp(1, 2) as usize], &mut discard);
    rs
}

/// Convert one decoded wire frame to the mixer's 48 kHz timeline.
///
/// A 48 kHz frame bypasses the resampler, but it must also invalidate any
/// converter left by an earlier lower-rate rung. That converter did not consume
/// the bypassed frames; reusing it when the wire later returns to the same lower
/// rate would interpolate from an old phase and an old sample instead of from
/// the immediately preceding 48 kHz frame.
fn resample_received_frame(
    rs: &mut Option<InterleavedLinearResampler>,
    rs_rate: &mut u32,
    rs_last: [f32; 2],
    frame_rate: u32,
    frame_channels: u8,
    raw: Vec<f32>,
) -> Vec<f32> {
    if frame_rate == MicSource::OUT_RATE {
        *rs = None;
        *rs_rate = MicSource::OUT_RATE;
        return raw;
    }
    if *rs_rate != frame_rate || rs.is_none() {
        // Continue from the last decoded frame: a mid-stream rate change must
        // not interpolate up from zero.
        *rs = Some(seeded_interleaved_resampler(
            frame_rate,
            MicSource::OUT_RATE,
            frame_channels,
            rs_last,
        ));
        *rs_rate = frame_rate;
    }
    let mut out = Vec::with_capacity((F48 + 8) * frame_channels as usize);
    rs.as_mut()
        .expect("non-48 kHz has a resampler")
        .process(&raw, &mut out);
    out
}

/// Convert one interleaved source frame to the negotiated wire width. This is
/// the sole compatibility downmix: a new peer keeps L/R, while a 1.0.0 peer
/// receives the historical mono signal. Mono sources can also feed a stereo
/// stream without changing balance by duplicating into both lanes.
fn convert_channels(input: &[f32], src_channels: u8, dst_channels: u8, out: &mut Vec<f32>) {
    let src = src_channels.clamp(1, 2) as usize;
    let dst = dst_channels.clamp(1, 2) as usize;
    out.clear();
    if src == dst {
        out.extend_from_slice(input);
        return;
    }
    let frames = input.len() / src;
    out.reserve(frames * dst);
    match (src, dst) {
        (2, 1) => {
            for frame in input[..frames * 2].chunks_exact(2) {
                out.push((frame[0] + frame[1]) * 0.5);
            }
        }
        (1, 2) => {
            for &sample in &input[..frames] {
                out.extend_from_slice(&[sample, sample]);
            }
        }
        _ => unreachable!(),
    }
}

/// 把 JB 的**有效**目标深度设到 `want` 帧，不重建、不分配。
///
/// 伺服读的 `JitterBuffer::target()` 含欠载惩罚，所以它的输出也是
/// 有效深度。换算回内部基线必须由 JB 自己做：它才拥有惩罚项，直接
/// 绕经 `update_target` 会把惩罚再加一次，使“要从 10 降到 9”反而不降。
pub(crate) fn steer_jitter_target(jb: &mut audiohub_net::media::JitterBuffer, want: u32) {
    jb.set_target_effective(want);
}

/// 重建 JB 以换一个**包络**（`min_target` / `max_target`）。
///
/// # 为什么非重建不可
///
/// 包络是 `JbTuning` 的字段，而 `JbTuning` 只在 `JitterBuffer::with_tuning` 的
/// 构造点被读一次。默认包络是 4..12 帧（40..120 ms，`JbTuning::DEFAULT` 的实测
/// 整定）。用户选 1000 ms 时，`target_effective()` 的 `clamp` 会把伺服的
/// 102 帧直接砍成 12 —— 滑条右半边全部失效，而 UI 会一路显示「已达物理上限」，
/// 那是**我们自己造的**上限，不是物理的。
///
/// # 代价与触发频率
///
/// 重建丢掉队列里的帧并重新预缓冲（几十毫秒，由 PLC 遮掉）。它**只在换代号
/// 变化时**发生，也就是用户动了滑条那一下。稳态一次都不会发生。
///
/// AUTO 时恢复调用方给的 `base` 整定——固定档期间放开的下限
/// （`min_target = 1`）不许留给 AUTO，那会悄悄改掉 plan §5 里 AUTO 的整定。
///
/// ⚠ `base` **不一定是 `JbTuning::DEFAULT`**。生产走 [`jb_tuning_for`]：
/// tier 0 是 `DEFAULT`，tier 1/2 是 `DEGRADED`。这里早先写死「恢复
/// `JbTuning::cached()` 那个实测默认」，在降级链路上是一句假话。
/// 目标 -> JB 应有的包络。**纯函数**，于是「哪个目标该配哪个包络」这条规则
/// 可以被直接测，不必起一台 daemon。
///
/// `base` 由调用方给（生产是 `JbTuning::from_env()`，测试给一个已知的），
/// 于是这条规则不依赖环境变量。
pub(crate) fn envelope_for(
    target: audiohub_ipc::LatencyTarget,
    base: audiohub_net::media::JbTuning,
) -> audiohub_net::media::JbTuning {
    use audiohub_ipc::LatencyTarget;
    match target {
        // AUTO：一个字段都不改，回到**这条链路的** base 整定
        // （tier 0 = `DEFAULT`，tier 1/2 = `DEGRADED`；见 `jb_tuning_for`）。
        // 固定档期间放开的下限**不许**留给 AUTO——那会悄悄改掉 plan §5 里
        // AUTO 的整定，而 AUTO 是默认档。
        LatencyTarget::Auto => base,
        LatencyTarget::TotalMs(ms) => {
            // 目标全部交给 JB 时需要的帧数，是 JB 深度的**上界**
            // （JB 驻留是端到端总延迟的真子集）。再给欠载惩罚留 2 帧余量。
            let need = (ms as u32).div_ceil(FRAME_MS as u32);
            let hi = need.saturating_add(2).max(base.max_target);
            audiohub_net::media::JbTuning {
                // 下界放开到 1：用户选 0 ms 就是「尽你所能地低」，
                // 拿实测默认的 4 帧去挡他，等于替他否决了他的选择。
                // 放开只是**允许**浅，不是强制——深度由伺服给。
                min_target: 1,
                max_target: hi,
                // 内存上界必须跟着抬，否则 `pop()` 的修剪线会落在目标之下，
                // 每一拍都在删刚到的真音频。`hard_slack` 是那条线与目标的距离。
                max_frames: hi.saturating_add(base.hard_slack).saturating_add(6),
                // `..base` 而不是逐字段列全：`JbTuning` 归另一条线在改，
                // 加字段时这里不该编译不过，也不该悄悄用上一个过时的默认值。
                ..base
            }
        }
    }
}

/// 按当前**目标**重建 JB 的包络（`min_target` / `max_target` / `max_frames`）。
///
/// # 为什么按目标而不是按伺服的输出
///
/// 第一版按伺服输出算，于是有一个先有鸡还是先有蛋：伺服的输出被旧包络夹在
/// 4 帧以上 ⇒ 永远算不出 1 ⇒ 包络永远不放开 ⇒ 伺服永远够不到低档。
/// 实测下来那一版的效果是滑条左半边完全无效，而日志里一行异常都没有。
/// 包络是**目标**的函数，与伺服此刻走到哪里无关。
///
/// # 为什么每拍都调而不是只在换档时调
///
/// 只在换代号变化时调，就得保证那一拍恰好在流已建好之后——第一版就是在流的
/// 第一个包上锁死了包络。这里改成每秒调一次、**包络已经对了就立刻返回**，
/// 于是「什么时候调」不再是正确性的一部分。
///
/// # 代价
///
/// 真正重建时丢掉队列里的帧并重新预缓冲（几十毫秒，由 PLC 遮掉），
/// 同时把 JB 预置到目标对应的深度——用户刚动过滑条，这一跳是他要的那一跳。
/// 稳态一次都不会发生。
/// 返回 `true` = 这一拍真的重建了。
///
/// 调用方必须据此**跳过本拍的伺服执行**：伺服的输出是上一拍算的，那时包络
/// 还是旧的（比如 4..12 帧），于是它被夹在 12 上。刚把 JB 预置到 50 帧，
/// 转手就按 12 去执行，等于预置从未发生——实测下来的表现是深度先跳到 50、
/// 同一拍掉回 12，然后以每秒一帧的限速慢慢爬 37 秒。
/// The jitter-buffer profile a stream on this media path runs.
///
/// Tier 1/2 get `JbTuning::DEGRADED` (`docs/design-m8-fallback.md` §3.3): a TCP
/// retransmission is ≥200 ms and `DEFAULT`'s deepest target is 120 ms, so under
/// `DEFAULT` **every** retransmission drains the buffer. Measured on the real
/// link, tier 1 ran ~1.6 underruns/min against a flat tier 0 baseline.
///
/// Keyed off the path rather than off the stored tier: the path is what the
/// stream is actually using, and the stored tier can be edited while a stream
/// is live (`peers.set_tier`) without the stream moving — media never changes
/// transport inside a live stream (design §5.1).
pub(crate) fn jb_tuning_for(path: &crate::tcpmedia::MediaPath) -> audiohub_net::media::JbTuning {
    match path {
        crate::tcpmedia::MediaPath::Udp(_) => audiohub_net::media::JbTuning::from_env(),
        // Tier 1 and tier 2 share the degraded profile because they share the
        // failure they exist for: TCP's minimum retransmission timeout is
        // 200–300 ms against a default `max_target` of 120 ms, so **any**
        // retransmission punches through the buffer. Tier 2 is if anything
        // worse (both directions on one congestion window), so the deeper
        // profile is if anything more right there.
        crate::tcpmedia::MediaPath::Tcp(_) | crate::tcpmedia::MediaPath::Framed(_) => {
            audiohub_net::media::JbTuning::degraded_from_env()
        }
    }
}

pub(crate) fn reshape_jitter_envelope(
    st: &mut crate::JbState,
    target: audiohub_ipc::LatencyTarget,
    base: audiohub_net::media::JbTuning,
    stream_id: u32,
) -> bool {
    use audiohub_ipc::LatencyTarget;
    use audiohub_net::media::JitterBuffer;
    let cfg = envelope_for(target, base);
    let cur_cfg = st.jb.tuning();
    if cfg.min_target == cur_cfg.min_target
        && cfg.max_target == cur_cfg.max_target
        && cfg.max_frames == cur_cfg.max_frames
    {
        return false; // 包络已经对了，不值得付一次重新预缓冲
    }
    // 预置深度：固定档直接落到「JB 独自承担全部目标」那个上界，闭环再往下收敛。
    // 走一帧一拍地爬过去要几十秒，而用户刚刚才动过滑条。
    let seed = match target {
        LatencyTarget::Auto => st.jb.target(),
        LatencyTarget::TotalMs(ms) => (ms as u32).div_ceil(FRAME_MS as u32).max(1),
    };
    st.jb = JitterBuffer::with_tuning_channels(
        seed.clamp(cfg.min_target, cfg.max_target),
        cfg,
        st.channels,
    );
    // 五个 lifetime 计数器随新 JB 归零 —— 与 `jb resync` 那条路同一个理由：
    // 旧采样点不能再参与差分，否则窗口值会被 saturating_sub 压成 0，
    // 让一次重建看起来像「这 10 秒完美无瑕」。
    st.conceal.reset();
    dlog!(
        "[audiohubd] stream {stream_id}: jitter envelope -> {}..{} frames, seeded at {seed}",
        cfg.min_target,
        cfg.max_target
    );
    true
}

/// 一条还在等源造好的 `TxCmd::Add`。
struct PendingAdd {
    stream_id: u32,
    key: [u8; 32],
    salt: Vec<u8>,
    path: MediaPath,
    channels: u8,
    loss_pct: f32,
    shared: Arc<TxShared>,
    ack: Option<mpsc::Sender<std::result::Result<(), String>>>,
}

/// 一次在途的建源请求。
struct PendingBuild {
    gen: u64,
    /// 等这个源的 Add。**可以为空**，那有两种含义，处理方式相同（成品回来时
    /// 由 `on_build_done` 判断）：一次设备变更重建（源已在表里，不需要 waiter），
    /// 或者等的人都撤了（成品直接收尸）。
    waiters: Vec<PendingAdd>,
}

/// `tx_loop` 的全部可变状态。
///
/// 打成一个结构体只为一件事：Add / Remove / 建源回来 / 收尸这四条路要同时改到
/// 四张表，逐个传 `&mut` 会让「哪条路忘了更新哪张表」退化成一个隐形的接线错误
/// —— 而本文件的历史上，出事的从来是接线不是逻辑。
struct TxState {
    streams: HashMap<u32, TxStream>,
    sources: HashMap<SourceSpec, SourceEnt>,
    /// 已发出、还没回来的建源请求。**`tx_loop` 每 tick 会把这里的 HalSpeaker
    /// 槽也算进 `busy`**：否则 `drain_idle_speakers` 会和建源线程里那次开流
    /// 排空同时动一个环的 `read_idx`，两个消费者，SPSC 契约当场作废。
    pending: HashMap<SourceSpec, PendingBuild>,
    next_gen: u64,
    builder: mpsc::Sender<BuildReq>,
}

impl TxState {
    fn new(builder: mpsc::Sender<BuildReq>) -> TxState {
        TxState {
            streams: HashMap::new(),
            sources: HashMap::new(),
            pending: HashMap::new(),
            next_gen: 1,
            builder,
        }
    }

    fn new_gen(&mut self) -> u64 {
        self.next_gen += 1;
        self.next_gen
    }

    /// 把一个源交给建源线程析构。**截止期线程上不许 `drop` 一个设备**：
    /// 关一条 cpal 流会进 CoreAudio 的服务端往返，和开它一样慢。
    fn retire(&self, spec: SourceSpec, gen: u64, src: Src) {
        // 送不出去（建源线程已经退了）就地丢掉：关机路径，不再有截止期可言。
        let _ = self.builder.send(BuildReq::Retire { spec, gen, src });
    }

    fn request_build(&mut self, spec: SourceSpec) -> u64 {
        let gen = self.new_gen();
        self.pending.insert(
            spec.clone(),
            PendingBuild {
                gen,
                waiters: Vec::new(),
            },
        );
        let _ = self.builder.send(BuildReq::Build { spec, gen });
        gen
    }

    fn install_stream(&mut self, spec: &SourceSpec, add: PendingAdd) {
        let source = self.sources.get(spec).expect("source exists before stream installation");
        let validation = validate_transmit_layout(&add.shared, add.channels, source.channels, source.src.speaker_layout())
            .and_then(|_| if add.shared.spatial.is_some() && source.src.sample_rate() != 48_000 {
                Err(anyhow!("spatial source must provide 48 kHz PCM"))
            } else { Ok(()) });
        if let Err(error) = validation {
            if let Some(ack) = add.ack {
                let _ = ack.send(Err(error.to_string()));
            }
            self.release_source(spec);
            return;
        }
        let channels = add.shared.spatial.as_ref().map_or(add.channels.clamp(1, 2), |contract| contract.channels());
        // 钳位与 `tx_loop` 那处同一条理由：加档而不改钳位 = 新档静默不可达。
        let start_rung = if add.shared.spatial.is_some() { 0 } else { add
            .shared
            .rung
            .load(Ordering::Relaxed)
            .min(audiohub_net::media::LADDER.len() as u32 - 1) };
        add.shared
            .media_channels
            .store(channels as u32, Ordering::Relaxed);
        self.streams.insert(
            add.stream_id,
            TxStream {
                id: add.stream_id,
                // real streams are always keyed per stream, never with
                // the bare connection media key
                crypto: MediaCrypto::new_for_stream(&add.key, add.stream_id, &add.salt),
                path: add.path,
                spec: spec.clone(),
                channels,
                loss: LossInjector::new(add.stream_id, add.loss_pct),
                seq: 0,
                media_frame_seq: 0,
                // 与 `TxShared` 的起步格一致。两处若分了岔，第一 tick 就会
                // 看到 `want != tx.rung`、白重建一次重采样器并跳一个 seq。
                //
                // **读它、不是再算一遍**：起步格现在按传输取值
                // （`MediaPath::auto_top_rung`），第二次推导就是第二个真值源。
                //
                // ⚠ 起步格与 `rs` **必须一起定**。这两行分开写的时候（起步格
                // 取自 shared、`rs: None` 照旧），tier 1 上的失效形态是：包头
                // 声明 32 kHz 而载荷仍是 48 kHz 的 960 B，接收侧
                // `format_mismatch` 每帧递增、整条流一个字都听不见。此前
                // 之所以没暴露，只是因为起步格恒为 48 kHz 的那一格。
                rung: start_rung,
                rs: if add.shared.spatial.is_some() { None } else { resampler_for(start_rung, channels, [0.0; 2]) },
                rs_last: [0.0; 2],
                // 一帧最深档 = 480 × 4 B（f32）。容量随格号变（换档同时改帧长度
                // 与每样本字节数），按最深档预留就不会在音频线程上扩容。
                pay: Vec::with_capacity(F48_STEREO * 4),
                // 0 = 「还没看过」。`dest_override` 在这条流建起来**之前**就被
                // 学到过的情形因此不会漏：那时代号已经 ≥1，第一 tick 就会去读。
                dest_epoch_seen: 0,
                // 透明起步。`shared.send_gain` 的默认也是 `SEND_GAIN_OFF`，
                // 所以一条流在对端明说「我的设备没有可写音量」之前满幅传输。
                gain: dsp::SendGain::new(),
                shared: add.shared,
            },
        );
        if let Some(a) = add.ack {
            let _ = a.send(Ok(()));
        }
    }

    fn release_source(&mut self, spec: &SourceSpec) {
        let gone = match self.sources.get_mut(spec) {
            Some(ent) => {
                ent.refs = ent.refs.saturating_sub(1);
                ent.refs == 0
            }
            None => false,
        };
        if gone {
            if let Some(ent) = self.sources.remove(spec) {
                self.retire(spec.clone(), ent.gen, ent.src);
            }
        }
    }

    /// 哪些虚拟扬声器槽此刻**有主**（位掩码，第 N 位 = 槽 N）。
    ///
    /// **在建的也算。** 建源线程在 `build_source` 的 HAL 分支里会把开流之前的
    /// 积压从那个环里排掉（`read_spk_mono`），而 `drain_idle_speakers` 动的是
    /// 同一个 `read_idx` —— 两个消费者同时推一个 SPSC 环的读下标，环里的数据
    /// 会被撕成谁也说不清的两半，而且**两边都不会报错**。
    ///
    /// 搬家之前这两件事在同一条线程、同一 tick 内先后发生，所以撞不上；
    /// 搬走之后它们真的并发了。这一行 `.chain(self.pending.keys())`
    /// 就是那次并发的全部对策。
    fn busy_speakers(&self) -> u16 {
        let mut busy = 0u16;
        for spec in self.sources.keys().chain(self.pending.keys()) {
            if let SourceSpec::HalSpeaker { slot } = spec {
                busy |= 1u16 << (*slot).min(15);
            }
        }
        busy
    }

    fn remove_stream(&mut self, stream_id: u32) {
        if let Some(s) = self.streams.remove(&stream_id) {
            s.shared.revoke_media();
            // 这条流从此不再被 tick 到，槽再也不会被覆盖 —— 但 `TxShared`
            // 还活着且还在被报告线程读。不清就是把最后一次读数永久钉住。
            clear_send_stages(&s);
            self.release_source(&s.spec);
            return;
        }
        // 还在等源造好的那一条：把 waiter 撤掉。**不撤销建源请求** —— 它已经
        // 在别的线程上跑了，撤不回来；成品回来时 `on_build_done` 会发现没人等
        // 并直接收尸。
        for p in self.pending.values_mut() {
            p.waiters.retain(|w| w.stream_id != stream_id);
        }
    }

    /// 建源线程交回一个成品（或一个失败）。
    fn on_build_done(&mut self, d: BuildDone) {
        let waiters = self
            .pending
            .remove(&d.spec)
            // 代号对不上 = 这是一次已经被更新的请求的迟到回音（同一个 spec 又
            // 发过一次 Build）。那条 pending 属于**新**的请求，不能被这次删掉。
            .filter(|p| p.gen == d.gen)
            .map(|p| p.waiters)
            .unwrap_or_default();
        let src = match d.result {
            Ok(s) => s,
            Err(why) => {
                if self.sources.contains_key(&d.spec) {
                    dlog!(
                        "[audiohubd] {} 重建失败（{why}）；保留原来的采集",
                        d.spec.label()
                    );
                }
                for w in waiters {
                    if let Some(a) = w.ack {
                        let _ = a.send(Err(why.clone()));
                    }
                }
                return;
            }
        };
        let channels = src.channels();
        // ① 源已经在表里 ⇒ 这是一次设备变更重建。**换芯**：新的先造好、这一刻
        // 才丢老的，与搬家前 `rebuild_mic_source` 的顺序保证逐字相同。
        if let Some(ent) = self.sources.get_mut(&d.spec) {
            let old_src = std::mem::replace(&mut ent.src, src);
            ent.channels = channels;
            let old_gen = std::mem::replace(&mut ent.gen, d.gen);
            // 换过源，上一 tick 的深度读数描述的是另一个队列了。
            ent.depths = NO_DEPTHS;
            ent.refs += waiters.len();
            self.retire(d.spec.clone(), old_gen, old_src);
            for w in waiters {
                self.install_stream(&d.spec.clone(), w);
            }
            dlog!("[audiohubd] {} 已重建（默认设备变化）", d.spec.label());
            return;
        }
        // ② 没人等 ⇒ 等的人在建源期间全撤了。直接收尸，别留一个没人读的设备。
        if waiters.is_empty() {
            self.retire(d.spec.clone(), d.gen, src);
            return;
        }
        // ③ 正常开流：装上，引用数 = 等的人数。
        self.sources.insert(
            d.spec.clone(),
            SourceEnt {
                src,
                channels,
                refs: waiters.len(),
                gen: d.gen,
                frame: Vec::with_capacity(F48 * channels as usize),
                frame_valid: true,
                depths: NO_DEPTHS,
            },
        );
        for w in waiters {
            self.install_stream(&d.spec.clone(), w);
        }
    }
}

/// **这条函数跑在 `tx_loop` 的截止期上，所以它一件设备都不许开。**
///
/// 搬家前它会一路调到 `build_source` → `MicSource::open` / `start_backend`
/// （110–600 ms 量级，`docs/spec-latency-floor.md` §1.4 的停顿直方图）。
/// 现在它只做三件常数时间的事：查表、推一条请求进通道、（源已在时）装流。
fn apply_txcmd(st: &mut TxState, cmd: TxCmd) {
    match cmd {
        TxCmd::Add {
            stream_id,
            key,
            salt,
            path,
            spec,
            channels,
            loss_pct,
            shared,
            ack,
        } => {
            let add = PendingAdd {
                stream_id,
                key,
                salt,
                path,
                channels,
                loss_pct,
                shared,
                ack,
            };
            // 源已经在跑：扇出一份就行，和搬家前一样是**同步**完成的。
            if let Some(ent) = st.sources.get_mut(&spec) {
                ent.refs += 1;
                st.install_stream(&spec, add);
                return;
            }
            // 已经有人在等同一个源：搭车，不再开第二次设备。
            if let Some(p) = st.pending.get_mut(&spec) {
                p.waiters.push(add);
                return;
            }
            let gen = st.new_gen();
            st.pending.insert(
                spec.clone(),
                PendingBuild {
                    gen,
                    waiters: vec![add],
                },
            );
            let _ = st.builder.send(BuildReq::Build { spec, gen });
        }
        TxCmd::Remove { stream_id } => st.remove_stream(stream_id),
    }
}

/// Closes every stream fed by a source that reported itself dead (the frozen
/// `SysAudioCapture::failed` seam). Without this the capture keeps returning 0
/// samples and the peer receives digital silence forever, with nothing on
/// either side saying why — the reason is logged and the peer gets CloseStream.
fn reap_dead_sources(inner: &DaemonInner, st: &mut TxState) {
    let dead: Vec<(SourceSpec, String)> = st
        .sources
        .iter()
        .filter_map(|(spec, ent)| ent.src.failed().map(|why| (spec.clone(), why)))
        .collect();
    for (spec, why) in dead {
        let ids: Vec<u32> = st
            .streams
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
        st.streams.retain(|_, s| {
            let keep = s.spec != spec;
            if !keep {
                // 同 TxCmd::Remove：走了就得清槽，否则一段死掉的排队会永远
                // 留在 UI 上，且不带任何「这是陈的」标记。
                clear_send_stages(s);
            }
            keep
        });
        // **不在这里 `drop`**：那具尸体里可能是一条 WASAPI / CoreAudio 捕获，
        // 关它和开它一样会进服务端往返。交给建源线程。
        if let Some(ent) = st.sources.remove(&spec) {
            st.retire(spec.clone(), ent.gen, ent.src);
        }
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
/// to the wrong device.
///
/// 「先造好新的、再丢老的」这条保证**没有变**，只是换了地方兑现：老的
/// `SourceEnt` 原封不动留在表里，直到 `TxState::on_build_done` 拿到新的那一刻
/// 才换芯。新设备打不开时（`Err` 分支）表里那一个一个字节都没动过。
///
/// 变的只有一件事：**开设备这件事不再发生在这条线程上**。
fn request_mic_rebuild(st: &mut TxState) {
    if !st.sources.contains_key(&SourceSpec::Mic) {
        dlog!("[audiohubd] default input changed; no microphone source to rebuild");
        return;
    }
    if st.pending.contains_key(&SourceSpec::Mic) {
        // 上一次重建还没回来。再发一次只会白开一次设备，并让两个成品互相覆盖。
        dlog!("[audiohubd] default input changed; 上一次麦克风重建还在进行中，本次略过");
        return;
    }
    st.request_build(SourceSpec::Mic);
    dlog!("[audiohubd] default input changed; 已请求重建麦克风源（设备在建源线程上开）");
}

/// 把 keepalive 学到的对端端口取过来（规格 spec-m4a §3：只学端口，不学 IP）。
///
/// # 为什么不是直接 `lk(&shared.dest_override)`（J1-4）
///
/// 搬家之前这里每 tick 每流拿一次 `Mutex<Option<SocketAddr>>`，而那把锁的另一头
/// 是 `rx_loop` —— **一条普通优先级线程**。它在临界区里被抢占，10 ms 音频线程
/// 就要陪等一个调度量子。100 次/秒/流的暴露面，换来的信息量是「一个几乎从不
/// 变化的地址」。
///
/// 代号（`TxShared::dest_epoch`）只在地址**真的变了**时才动（`rx_loop` 自己也
/// 只在 `*d != Some(learned)` 时写），所以稳态下这里只剩一次 Acquire 原子读。
///
/// **残留，如实写明**：地址真变的那一 tick 仍会取一次锁。按实测语义那是每条流
/// 一生一次（第一个 keepalive 教会我们对端端口那一次），不是稳态项。
/// [`crate::rtlog`] 那种「彻底搬走」在这里做不到而且不值得：要做到就得给
/// `SocketAddr` 手写一个 seqlock 编码（v4/v6/scope_id/flowinfo），
/// 而编码写错的表现是**媒体流被静默发去错误的地址**——比它治的病更坏。
///
/// # Tier 1 (M8)
///
/// Does nothing on [`MediaPath::Tcp`]. There is no destination to learn: the
/// media connection *is* the destination, and tier 1 sends no `PullReq` in the
/// first place — a keepalive exists to hold NAT/firewall state open for a UDP
/// flow. The `match` is what makes that skip structural rather than a rule
/// somebody has to remember (design §4.2 item 3).
fn refresh_dest(tx: &mut TxStream) {
    let MediaPath::Udp(dest) = &mut tx.path else {
        return;
    };
    let epoch = tx.shared.dest_epoch.load(Ordering::Acquire);
    if epoch == tx.dest_epoch_seen {
        return;
    }
    tx.dest_epoch_seen = epoch;
    if let Some(a) = *lk(&tx.shared.dest_override) {
        if a != *dest {
            dlog!(
                "[audiohubd] stream {} dest {} -> {} (keepalive)",
                tx.id,
                dest,
                a
            );
            *dest = a;
        }
    }
}

pub(crate) fn tx_loop(
    inner: Arc<DaemonInner>,
    cmds: mpsc::Receiver<TxCmd>,
    builder: mpsc::Sender<BuildReq>,
    built: mpsc::Receiver<BuildDone>,
) {
    let mut st = TxState::new(builder);
    // Lifted out of the daemon mutex once, here, so the tick itself never
    // touches that lock; the bridge is installed before any thread starts and
    // is never replaced.
    let hal = inner.hal();
    let mut dev_epoch = inner.dev_in_epoch.load(Ordering::Relaxed);
    let _qos_guard = raise_audio_thread_qos("tx_loop");
    // 本线程的 `dlog!` 从此走入队 + 独立线程落盘。**必须在进循环之前**：
    // 之后这条线程上任何一处 `dlog!`（包括 `halbridge` 里那两条欠载段首/段尾）
    // 都不再做阻塞 `write` 也不再抢 `Stderr` 的全局锁。见 `rtlog` 模块文档。
    rtlog::arm("tx_loop");
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
    // 重采样暂存，进程内只分配这一次。48k→其它档只会变短，`F48 * 2` 够用；
    // 真不够 `rs.process` 会自己扩一次，此后不再扩。
    let mut staged: Vec<f32> = Vec::with_capacity(F48 * 2);
    let mut converted: Vec<f32> = Vec::with_capacity(F48 * 2);
    // plan §7.2 软件增益的输出暂存。与 `staged` 同样是**循环级**的纯 scratch：
    // 写进去、同一次迭代里编完就不再被看，N 条流共一块。
    //
    // 它必须是一块**独立**的缓冲，不能就地写回 `staged` 或 `ent.frame`：
    // 后者是扇出的共享帧（改了它，同一个源上的其余流全被带偏，而且逐帧复利）。
    let mut gained: Vec<f32> = Vec::with_capacity(F48 * 12);
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        {
            let e = inner.dev_in_epoch.load(Ordering::Relaxed);
            if e != dev_epoch {
                dev_epoch = e;
                request_mic_rebuild(&mut st);
            }
        }
        // 建源线程交回来的成品。**放在最前面**：这一 tick 的其余部分（扇出、
        // 空闲排空、发包）都该看到刚装上的源，而不是等下一 tick。
        while let Ok(d) = built.try_recv() {
            st.on_build_done(d);
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
        let late = Instant::now().saturating_duration_since(next_time);
        // 调度迟到直方图。**位置就是这里，且与 `MIX_LATE` 刻意不同**：
        // `next_time` 是本 tick 的计划时刻，差值即「上一 tick 的活 + 任何抢占」
        // 把我们推迟了多久。这一条服务于对端 `jitter_buf` —— 发送端停顿 Δ ⇒
        // 对端 JB 在 Δ 里净排空 Δ/10 帧，**整 tick 正是要的语义**。
        //
        // 挪到下面等待循环之后，量到的就变成纯唤醒过冲（亚毫秒），
        // 那是 `play_ring` 的 `margin` 关心的量、不是这一条关心的量。
        // `mixer_loop` 需要的恰好是后者，所以它走 `sleep_until`；
        // 两个测点量的不是同一个东西，见 [`LateCell`] 上方的对照表。
        TX_LATE.record(late);
        let late_ms = late.as_millis() as u64;
        let behind = tick + late_ms / FRAME_MS;
        // 本 tick 准不准时。落后 ≤100 ms 时循环用背靠背的 tick 追平（自愈），
        // 那期间队列深度是**假高**——高是因为我们暂时没读，不是因为积压。水位
        // 控制器必须知道这件事，否则它会把马上就要用到的音频削掉（不变量 I6）。
        let punctual = behind <= tick;
        if behind > tick + 10 {
            // 治法 A：被跳过的那些帧从队列里读走丢掉，而不是留在里面。
            // 这条路径此前无日志、无计数，是它能潜伏 9 小时的直接原因。
            let skipped = behind - tick;
            let drained = drain_skipped_ticks(hal.as_deref(), &mut st.sources, skipped);
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
                Ok(cmd) => apply_txcmd(&mut st, cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_txcmd(&mut st, cmd);
        }
        // spec-m5b §5.4: a PUBLISHED speaker ring with no session behind it
        // still receives whatever the app that selected it played. Nobody would
        // ever move its read_idx, the ring fills, and the driver's census
        // starts logging "audiohubd has stopped draining it" at error level.
        // Only a ring's consumer may move read_idx, and on this side that is
        // THIS thread — so the drain belongs here, above the idle short-circuit
        // below, because "no streams at all" is exactly the case it exists for.
        if let Some(h) = hal.as_ref() {
            h.drain_idle_speakers(st.busy_speakers());
        }
        if st.streams.is_empty() {
            // 空闲短路的等待时长。**有建源在途时必须短**：这条路径只等
            // `cmds`，等不到 `built`，而第一条流的 `BuildDone` 恰恰是在
            // `streams` 还空着的时候回来的。等满 200 ms 就是给每一次开流
            // 平白加上最多 200 ms —— 搬家搬出一个新的延迟来，白搬。
            let idle = if st.pending.is_empty() { 200 } else { 2 };
            match cmds.recv_timeout(Duration::from_millis(idle)) {
                Ok(cmd) => apply_txcmd(&mut st, cmd),
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
        for ent in st.sources.values_mut() {
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
            let expected = F48 * ent.channels as usize;
            ent.frame_valid = ent.frame.len() == expected;
            if ent.frame.len() != expected {
                // An OVER-long frame means the source appended instead of
                // replacing, and the resize below then re-sends whatever its
                // very first call produced, forever, while the packet counts,
                // the loss rate and the tone probe all stay green. That cost a
                // full debugging session once; it must never be silent again.
                debug_assert!(
                    ent.src.speaker_layout().is_some() || ent.frame.len() <= expected,
                    "FrameSource yielded {} samples (> {expected}): it appended instead of replacing",
                    ent.frame.len(),
                );
                if ent.frame.len() > expected && slow_tick && ent.src.speaker_layout().is_none() {
                    dlog!(
                        "[audiohubd] BUG: source yielded {} samples, expected {expected} — \
                         the stream is repeating its first frame",
                        ent.frame.len()
                    );
                }
                ent.frame.resize(expected, 0.0);
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
        reap_dead_sources(&inner, &mut st);
        // One clock read, shared by two consumers that must agree: the wire
        // timestamp (`tx_loop`'s own epoch, read by the peer's jitter buffer)
        // and the tier 1 queue stamp (an absolute `Instant`, read by the write
        // thread's stale gate). Deriving `ts_us` from `tick_at` is not a
        // micro-optimisation — it is what stops the two from being two
        // separate samples of the clock that can straddle a scheduling gap.
        let tick_at = Instant::now();
        let ts_us = tick_at.duration_since(start).as_micros() as u64;
        // 拆开借用：这一趟要同时按流迭代（`&mut`）和按 spec 查源（`&`）。
        let TxState {
            streams, sources, ..
        } = &mut st;
        let mut queued_any = false;
        // 见下面用到它的那处注释：循环级的重采样暂存。
        staged.clear();
        for tx in streams.values_mut() {
            // Source built, peer has not accepted yet (`TxShared::armed`). There
            // is nowhere for a datagram to go — the peer has no receiving stream
            // — and one sent anyway lands in our payload counter and in nobody
            // else's. Stages are cleared for the same reason the missing-source
            // arm below clears them: a stale reading left in the slot is a
            // number the UI keeps showing about a stream that is not running.
            // Acquire pairs with open_session's publication store: all initial
            // transport and send-gain state must be visible before the first
            // media frame is allowed to leave.
            if !tx.shared.armed.load(Ordering::Acquire) {
                clear_send_stages(tx);
                continue;
            }
            let Some(ent) = sources.get(&tx.spec) else {
                // 源已经不在表里了（`reap_dead_sources` 收了尸，或 Remove 把
                // refs 减到 0），而这条流的 `TxShared` 还活着并且仍在被报告线程
                // 读。**这里必须清槽再走**：早先的 `continue` 会把最后一次读数
                // 留在槽里，于是 UI 继续显示一段早已不存在的排队——这正是下面
                // 那句注释要消灭的「静默缺项」，而缺项本身就是从这条捷径漏出去的。
                clear_send_stages(tx);
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
            publish_send_stages(&tx.shared.stages, &ent.depths);
            if tx.shared.spatial.is_some() {
                queued_any |= send_spatial_frame(&inner.media_send, tx, ent, tick_at, ts_us, &mut gained);
                continue;
            }
            if ent.channels > 2 {
                tx.shared.fail_media("wide source cannot enter the legacy stereo transmit path".into());
                continue;
            }
            // 钳位用 `LADDER.len()`，**不是字面量**：位深进阶梯之前这里写的是
            // `.min(3)`，加档而不改它 = 新档**静默不可达**（rung 4/5 被钳成 3，
            // 用户选了 16 kHz 却在发 24 kHz，而没有任何一处会报错）。
            let want = tx
                .shared
                .rung
                .load(Ordering::Relaxed)
                .min(audiohub_net::media::LADDER.len() as u32 - 1);
            if want != tx.rung {
                tx.rung = want;
                let last = tx.rs_last;
                // 只有 48 kHz 的那几格不需要重采样。判据是**采样率**，不是格号：
                // 位深进阶梯之后 rung 0/1/2 全是 48 kHz，写 `want != 0` 会给
                // 48 kHz/24 bit 和 48 kHz/16 bit 白建一个 48→48 的重采样器。
                let f = audiohub_net::media::rung_format(want);
                tx.rs = resampler_for(want, tx.channels, last);
                // Align the first packet of a format epoch to its part count.
                // The receiver uses this boundary to freeze an epoch-relative
                // frame base and then derives each part from the raw sequence.
                // Without alignment, observing part 1 before part 0 would make
                // the boundary ambiguous and could permanently pair chunks from
                // adjacent frames. At most `parts - 1` raw sequence values are
                // skipped; packet statistics deliberately retain those holes.
                let parts = f.wire_packets_per_frame_for(tx.channels) as u32;
                tx.seq = align_wire_seq(tx.seq, parts);
            }
            let wire_input: &[f32] = if ent.channels == tx.channels {
                &ent.frame
            } else {
                convert_channels(&ent.frame, ent.channels, tx.channels, &mut converted);
                &converted
            };
            if let Some(last) = wire_input.chunks_exact(tx.channels as usize).last() {
                tx.rs_last[..tx.channels as usize].copy_from_slice(last);
            }
            let fmt = audiohub_net::media::rung_format(tx.rung);
            let rate = fmt.rate_hz;
            // 重采样输出写进**循环级**的暂存，不再是每条流一个字段。
            //
            // 它是纯 scratch：写进去、同一次迭代里读完就不再被看。挪出来有两个
            // 好处，第二个是承重的：
            //   1. N 条流共一块，省 N−1 份；
            //   2. `samples` 从此借的是这块暂存而不是 `tx` —— 于是
            //      `refresh_dest(&mut tx)` 能留在它**原来的位置**（丢包判据
            //      之后）。放到别处会让「被 LossInjector 丢掉的那一 tick 也刷
            //      地址」，虽然无害，但没有理由为了让借用检查器过关而动语义。
            // ⚠ `rs.process` 是**追加**语义，所以每次必须先 `clear()`。
            let samples: &[f32] = match tx.rs.as_mut() {
                Some(rs) => {
                    staged.clear();
                    rs.process(wire_input, &mut staged);
                    &staged
                }
                None => wire_input,
            };
            // ------------------------------------ plan §7.2 发送侧软件增益
            //
            // 对端真实设备没有我们驱得动的音量时（macOS 聚合设备），使用端虚拟
            // 设备自管音量，增益就落在这里。**默认路径一次乘法都不做**：
            // `SendGain::apply` 在透明态把 `samples` 原样还回去（同一个指针），
            // 不复制、不分配。而这条线程是 10 ms 截止期线程。
            //
            // # 位置：重采样之后，编码之前
            //
            // - 在**编码之前**是这条支路存在的全部意义：写到编码之后，线上仍是
            //   满幅，兜底等于没接。
            // - 在**重采样之后**是为了 dither：dither 必须紧贴量化器
            //   （`encode_pcm_into` 的 round），先 dither 再重采样会被线性插值
            //   低通掉、相邻样本变成相关的，TPDF 那点性质就没了。斜坡因此按
            //   **线上**采样率算（`rate`），于是任何格号上都是同样的毫秒数。
            //
            // # 增益属于流，不属于源
            //
            // `ent.frame` 是**扇出**的（一份，发给挂在这个源上的每条流），而
            // `tx.gain` 每条流一份。`apply` 的入参是 `&[f32]`，所以「就地把共享
            // 帧乘掉」在这里连写都写不出来 —— 那个 bug 的形状是逐帧复利
            //（0.5ᵏ）而不是一次串台，且全线指标不动。
            tx.gain.set_target(TxShared::gain_of(
                tx.shared.send_gain.load(Ordering::Relaxed),
            ));
            let samples: &[f32] =
                tx.gain
                    .apply_interleaved(samples, rate, fmt.depth, tx.channels, &mut gained);
            // Split one 10 ms interleaved frame into the minimum number of
            // equal MTU-safe datagrams. The widest stereo rung needs four.
            //
            // # 为什么是应用层 5 ms 分包，而不是让 IP 去分片
            //
            //   1. **丢包代价减半**：IP 分片下任一片丢失整帧作废（≈2q 概率 ×
            //      10 ms 音频）；两个独立包各自 q × 5 ms ⇒ 期望隐藏音频减半，
            //      而且 10 ms 的洞换成 5 ms 的洞（PLC 是上一真实帧的衰减重复，
            //      洞越短复原越像，这不是线性关系）。
            //   2. **保住「每个数据报独立鉴权」**：IP 分片之后鉴权单位是重组后
            //      的整体，内核要维持重组队列，一片丢失就要等超时。
            //   3. 带宽代价含以太网帧开销只有 +3.8 %——分片的第二片只有 16 B
            //      IP 载荷却要占一整个最小帧，所谓「省下的那份包头」根本没省到。
            //
            // ⚠ **`FRAME_MS` 一个字不改。** 这里动的是**线上包时长**，
            // 与调度节拍是两件事（AES67 没有我们这种抖动缓冲，所以它把两者当成
            // 一件事；我们不必）。JB / 伺服 / DLL / 延迟档 / 音质分级全不受影响。
            let parts = fmt.wire_packets_per_frame_for(tx.channels);
            let dropped = tx.loss.should_drop(); // advance LCG every frame
            if dropped {
                // 丢的是**整帧**：`seq` 照样按实际会发的包数推进，否则接收侧的
                // 期望序号会与发送侧错位，丢包率算出来是假的。
                tx.seq = tx.seq.wrapping_add(parts as u32);
                tx.media_frame_seq = tx.media_frame_seq.wrapping_add(1);
                continue;
            }
            refresh_dest(tx);
            // The tier 1 link this stream queued into, if any, so it can be
            // woken **once per stream** rather than once per packet.
            //
            // UDP wakes once per tick because there is one socket and therefore
            // one send thread. Tier 1 has one thread PER PEER, so a single wake
            // cannot cover them; per stream is the next bound down. A redundant
            // wake to an already-running writer costs a fence and a load.
            let mut tcp_link: Option<&Arc<crate::tcpmedia::TcpMediaLink>> = None;
            let chunk = samples.len() / parts;
            for p in 0..parts {
                let seq = tx.seq;
                tx.seq = tx.seq.wrapping_add(1);
                let lo = p * chunk;
                let hi = if p + 1 == parts {
                    samples.len()
                } else {
                    lo + chunk
                };
                // 载荷写进本流长期复用的缓冲，不再每 tick 造一个 `Vec`。
                dsp::encode_pcm_into(&samples[lo..hi], fmt.depth, &mut tx.pay);
                let header = Header {
                    kind: Kind::Media,
                    codec: Codec::for_depth(fmt.depth),
                    channels: tx.channels,
                    sample_rate: rate,
                    session_id: tx.id as u64,
                    stream_id: tx.id,
                    seq,
                    // ⚠ **后半包必须 +5000 µs，不能与前半包共用同一个时间戳。**
                    // 抖动是 `|transit − prev_transit|`；两个包若共用同一个
                    // `timestamp_us`，后半包的 transit 差会退化成「两包间的发送
                    // 间隔」（微秒级）⇒ **一半的抖动样本近似 0**，p95 被系统性
                    // 拉低 ⇒ AUTO 的降档判据（抖动 > 15 ms）变迟钝。
                    // 加上偏移之后两个样本各自诚实。
                    timestamp_us: media_timestamp_with_frame_tag(
                        split_timestamp_us(ts_us, p, parts),
                        tx.media_frame_seq,
                        p,
                    ),
                    payload_len: 0, // seal_into() sets ciphertext length
                };
                // **`sendto` 不在这条线程上了**（J1-1）：就地把数据报封进发送
                // 队列的槽里，由 `udp_send_loop` 去进内核。计数器也跟着搬过去，
                // 判据不变（只有 `send_to` 返回 `Ok` 才算）。
                //
                // Tier 1 (M8) is the same shape with a different queue: a
                // `write` into the kernel has no more of a predictable upper
                // bound than a `sendto` does, so it lives on
                // `tcpmedia::write_loop`'s thread for exactly the same reason.
                // The frame bytes are identical either way — a sealed media
                // datagram already *is* a mux frame (design decision B).
                let seal = |buf: &mut Vec<u8>| match tx.crypto.seal_into(&header, &tx.pay, buf) {
                    Ok(()) => true,
                    Err(e) => {
                        dlog!("[audiohubd] media seal stream {}: {e}", tx.id);
                        false
                    }
                };
                let queued = match &tx.path {
                    MediaPath::Udp(dest) => {
                        inner
                            .media_send
                            .enqueue(*dest, &tx.shared, tx.pay.len(), seal)
                    }
                    // `tick_at` and not a fresh `Instant::now()` per packet: the
                    // stale gate measures how long a frame waited in OUR queue,
                    // and both halves of a split frame waited the same amount.
                    //
                    // Tier 1 and tier 2 are one arm on purpose. The frame is the
                    // same sealed datagram, the queue is the same type and the
                    // stale gate is the same gate; what differs is only who
                    // drains it (`tcpmedia::write_loop` against
                    // `mux::write_loop`), and that difference has no business
                    // being re-decided on the 10 ms deadline thread.
                    // Listed rather than caught by `_`, so a fourth transport
                    // is a compile error here instead of media silently
                    // vanishing down `media_link() == None`. That is the same
                    // bargain `ControlTransport` makes, and it should not be
                    // made in one place and declined in the other.
                    MediaPath::Tcp(_) | MediaPath::Framed(_) => match tx.path.media_link() {
                        Some(link) => {
                            tcp_link = Some(link);
                            link.enqueue(tick_at, &tx.shared, tx.pay.len(), seal)
                        }
                        None => false,
                    },
                };
                if queued {
                    queued_any = true;
                }
            }
            if let Some(l) = tcp_link {
                l.wake();
            }
            tx.media_frame_seq = tx.media_frame_seq.wrapping_add(1);
        }
        // 每 tick 至多一次唤醒，在**全部**流入队之后。见 `UdpSender::wake`。
        if queued_any {
            inner.media_send.wake();
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

/// 收流缓冲。**必须 ≥ 最深档不分包时的整帧密文**（1976 B），不是「够用就行」。
///
/// # 缓冲太小的失效形态在两个平台上不一样，Windows 那个严重一个量级
///
/// - **macOS / BSD**：`recvfrom` 把超长数据报**截断**并丢弃余部，返回截断长度
///   ⇒ 密文过不了 AEAD ⇒ 走 `handle_datagram` 里那句
///   `let Ok((h, plain)) = rx.crypto.open(dg) else { return }`，
///   而那条路径的注释写着 `// tampered/foreign`——**没有任何一处会说「包太大」**。
/// - **Windows**：`recvfrom` 直接返回 `WSAEMSGSIZE`（10040），**不是截断**。
///   Rust 把它映射成一个**不在 [`poll_tick`] 白名单里**的 `ErrorKind`
///   ⇒ 落进下面那条 `sleep(100ms)` 分支。
///   ⇒ **每一个超长数据报让收流线程睡 100 ms。** 100 pkt/s 全超长 = 收流彻底
///   停摆，而日志里只有一行看不出所以然的 `udp recv:`。
///
/// ⇒ 留 4096：最深档 1976 B 的两倍有余。将来任何一次「加个声道」或「加个
/// 96 kHz 档」都不会变成「Windows 上全静音、日志里只有 tampered」。
const RECV_BUF_BYTES: usize = 4096;

/// Which directions the `AUDIOHUB_TEST_BLOCK_UDP` hook is swallowing.
///
/// # What this is for, and the honesty it costs
///
/// The automatic tier 0 → 1 downgrade cannot be tested without a link where
/// UDP does not get through, and every other way of producing one needs
/// administrative privileges on a machine that is also carrying the user's real
/// audio (`pfctl`, `New-NetFirewallRule`). This hook needs none: it drops
/// datagrams at the point **closest to the socket** on each side, so almost the
/// entire path under test is the real one.
///
/// What it is not: a firewall. It is our own simulator being fed to our own
/// detector, and a real-firewall confirmation is a separate, one-off exercise
/// (design §7 Level 2b). That limitation is why the hook sits at the socket
/// rather than somewhere convenient higher up — the smaller the faked segment,
/// the less the two can disagree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UdpBlock {
    pub out: bool,
    pub inbound: bool,
}

impl UdpBlock {
    pub(crate) const OFF: UdpBlock = UdpBlock {
        out: false,
        inbound: false,
    };

    /// Parse the hook's value. **An unrecognised value blocks both
    /// directions.**
    ///
    /// Deliberately the harsh reading. Treating a typo as "off" would let a
    /// harness believe it had built a blocked link, watch the downgrade not
    /// happen, and record a pass — the exact shape of a test that is theatre.
    /// Blocking too much can only make a test fail loudly, which is a fault
    /// somebody investigates.
    pub(crate) fn parse(v: &str) -> UdpBlock {
        match v {
            "out" => UdpBlock {
                out: true,
                inbound: false,
            },
            "in" => UdpBlock {
                out: false,
                inbound: true,
            },
            _ => UdpBlock {
                out: true,
                inbound: true,
            },
        }
    }

    fn describe(self) -> &'static str {
        match (self.out, self.inbound) {
            (true, true) => "in both directions",
            (true, false) => "on the way out",
            (false, true) => "on the way in",
            (false, false) => "nowhere",
        }
    }

    /// Resolve the hook for one daemon: the explicit setting if there is one,
    /// otherwise `AUDIOHUB_TEST_BLOCK_UDP`.
    ///
    /// Both, and in that order, for the reason
    /// [`crate::DaemonCfg::tx_throttle_kbps`] gives at length: **the
    /// environment is process-global and the tests share a process.** An
    /// in-process pair of daemons cannot express "block this one's UDP and not
    /// that one's" through the environment at all — and a one-directional block
    /// is precisely what the keepalive signal exists to catch, so without the
    /// explicit form half the detector would be untestable.
    ///
    /// The environment form stays because it is the only one available to a
    /// separate-process harness (`regress/`), where the daemon is a binary and
    /// there is nobody to pass a struct to.
    pub(crate) fn resolve(explicit: Option<&str>) -> UdpBlock {
        let b = match explicit {
            Some(v) => UdpBlock::parse(v),
            None => match std::env::var("AUDIOHUB_TEST_BLOCK_UDP") {
                Ok(v) => UdpBlock::parse(&v),
                Err(_) => return UdpBlock::OFF,
            },
        };
        // Said out loud, because a stray setting produces total silence with
        // every counter and every screen looking healthy — this line is the
        // only thing that would tell it apart from a network fault.
        dlog!(
            "[audiohubd] ⚠ UDP test block active: dropping UDP datagrams {}. \
             This is a TEST HOOK; tier 0 audio will not work.",
            b.describe()
        );
        b
    }
}

pub(crate) fn rx_loop(inner: Arc<DaemonInner>) {
    const _: () = assert!(
        RECV_BUF_BYTES >= DEEPEST_SEALED_FRAME_BYTES,
        "收流缓冲装不下最深档的整帧：mac 上表现为 tampered，Windows 上表现为收流每包睡 100 ms"
    );
    let mut buf = [0u8; RECV_BUF_BYTES];
    // Read once, outside the loop: the loop runs at the packet rate.
    let block_in = inner.udp_block.inbound;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match inner.udp.recv_from(&mut buf) {
            // Dropped after `recv_from` and before anything else looks at it —
            // as close to the socket as this side gets, which is what keeps the
            // simulated segment down to a single branch.
            Ok(_) if block_in => {}
            Ok((n, from)) => handle_datagram(&inner, &buf[..n], from),
            Err(e) if poll_tick(e.kind()) => {}
            Err(e) => {
                dlog!("[audiohubd] udp recv: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Timestamp packet `part` at its position inside the 10 ms audio frame.
///
/// Reusing the frame timestamp for every packet would inject near-zero transit
/// deltas and bias p95 jitter downward. Keep this as a callable production
/// helper so the guard test cannot pass by duplicating the arithmetic.
pub(crate) fn split_timestamp_us(frame_ts_us: u64, part: usize, parts: usize) -> u64 {
    frame_ts_us + (part as u64) * (FRAME_MS * 1000 / parts.max(1) as u64)
}

/// Return the first wrapping sequence at or after `seq` divisible by `parts`.
///
/// `seq + (parts - seq % parts)` is wrong when the addition crosses u32 wrap
/// and `parts` does not divide 2^32 (notably the three-packet stereo rung).
/// Trying the at-most-four legal paddings preserves the receiver's invariant
/// that every new epoch begins at a numeric part-zero boundary.
fn align_wire_seq(seq: u32, parts: u32) -> u32 {
    let parts = parts.clamp(1, audiohub_net::media::MAX_WIRE_PARTS as u32);
    (0..parts)
        .map(|padding| seq.wrapping_add(padding))
        .find(|candidate| candidate % parts == 0)
        .expect("one of consecutive wrapping sequence values is aligned")
}

const MEDIA_FRAME_COUNTER_BITS: u32 = 6;
const MEDIA_FRAME_COUNTER_MASK: u64 = (1 << MEDIA_FRAME_COUNTER_BITS) - 1;
const MEDIA_PACKET_PART_BITS: u32 = 2;
const MEDIA_PACKET_PART_MASK: u64 = (1 << MEDIA_PACKET_PART_BITS) - 1;
const MEDIA_FRAME_TAG_BITS: u32 = MEDIA_FRAME_COUNTER_BITS + MEDIA_PACKET_PART_BITS;
const MEDIA_FRAME_TAG_MODULUS: u64 = 1 << MEDIA_FRAME_TAG_BITS;
const MEDIA_FRAME_TAG_MASK: u64 = MEDIA_FRAME_TAG_MODULUS - 1;
/// Beyond 64 frames (640 ms) even the degraded jitter buffer's 48-frame memory
/// envelope cannot retain the missing audio. Exact PLC placement has no value
/// there, so the receiver explicitly collapses and re-anchors adjacent.
const MAX_TAGGED_TRANSITION_FRAMES: u32 = 64;

/// Encode a bounded logical frame tag without expanding the frozen header.
///
/// `timestamp_us` is authenticated header data and its microsecond precision is
/// far finer than any timing decision in the receiver. The nearest timestamp
/// whose low eight bits encode `(media_frame_seq mod 64, packet part)` differs
/// by at most 128 us; two adjacent transit samples can therefore gain at most
/// 256 us of error, below two percent of AUTO's 15 ms jitter threshold. Six
/// counter bits make advances 1..=64 unique, while two part bits keep a format
/// boundary self-describing even when u32 wrap changes numeric divisibility.
fn media_timestamp_with_frame_tag(timestamp_us: u64, media_frame_seq: u64, part: usize) -> u64 {
    let tag = ((media_frame_seq & MEDIA_FRAME_COUNTER_MASK) << MEDIA_PACKET_PART_BITS)
        | (part as u64 & MEDIA_PACKET_PART_MASK);
    let residue = timestamp_us & MEDIA_FRAME_TAG_MASK;
    let forward = tag.wrapping_sub(residue) & MEDIA_FRAME_TAG_MASK;
    let backward = MEDIA_FRAME_TAG_MODULUS - forward;
    if forward <= backward {
        timestamp_us
            .checked_add(forward)
            .or_else(|| timestamp_us.checked_sub(backward))
            .unwrap_or(timestamp_us)
    } else {
        timestamp_us
            .checked_sub(backward)
            .or_else(|| timestamp_us.checked_add(forward))
            .unwrap_or(timestamp_us)
    }
}

#[derive(Clone, Copy)]
struct MediaFrameTag {
    frame: u8,
    part: usize,
}

fn media_frame_tag(timestamp_us: u64) -> MediaFrameTag {
    let tag = timestamp_us & MEDIA_FRAME_TAG_MASK;
    MediaFrameTag {
        frame: (tag >> MEDIA_PACKET_PART_BITS) as u8,
        part: (tag & MEDIA_PACKET_PART_MASK) as usize,
    }
}

struct ReassembledWireFrame {
    frame_seq: u32,
    sample_rate: u32,
    channels: u8,
    samples: Vec<f32>,
    partial_conceal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WireFormatEpoch {
    format: audiohub_net::media::WireFormat,
    parts: usize,
    /// Raw packet sequence at part zero of `frame_base`.
    wire_base: u32,
    /// Receiver-local frame sequence corresponding to `wire_base`.
    frame_base: u32,
    /// Greatest raw packet sequence admitted in this epoch.
    high_wire_seq: u32,
    /// Logical sender frame modulo 64 for `high_wire_seq`'s frame.
    high_frame_tag: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WireFormatTransition {
    from: audiohub_net::media::WireFormat,
    from_parts: usize,
    to: audiohub_net::media::WireFormat,
    to_parts: usize,
    /// A tagged transition exceeded the exact 64-frame window or carried an
    /// inconsistent tag, so it was conservatively re-anchored adjacent.
    frame_tag_fallback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WirePacketPosition {
    frame_seq: u32,
    part: usize,
    transition: Option<WireFormatTransition>,
}

/// Maps the sender's packet sequence onto a stable receiver-local frame clock.
///
/// The raw sequence advances once per datagram, so dividing it by the current
/// packet count cannot be a frame clock when that count changes. A format epoch
/// freezes the divisor and carries the local frame sequence across the boundary.
/// A new format may commit only beyond the current raw-sequence high-water mark;
/// packets from an older epoch therefore cannot roll the format back after
/// reordering or retransmission.
#[derive(Default)]
pub(crate) struct WireFormatTimeline {
    current: Option<WireFormatEpoch>,
    /// Zero keeps the 1.0.0 ambiguity-preserving fallback; one decodes the
    /// authenticated frame-counter/packet-part tag advertised by the sender.
    frame_tag_version: u8,
}

impl WireFormatTimeline {
    pub(crate) fn with_frame_tag_version(frame_tag_version: u8) -> Self {
        Self {
            current: None,
            frame_tag_version: frame_tag_version.min(1),
        }
    }

    fn rebase_behind_high_water(epoch: &mut WireFormatEpoch) {
        const REBASE_AFTER_PACKETS: u32 = 1 << 30;
        const RETAIN_FRAMES: u32 = 4;

        let high_offset = epoch.high_wire_seq.wrapping_sub(epoch.wire_base);
        if high_offset < REBASE_AFTER_PACKETS {
            return;
        }
        let high_frame = high_offset / epoch.parts as u32;
        let shift_frames = high_frame.saturating_sub(RETAIN_FRAMES);
        epoch.wire_base = epoch
            .wire_base
            .wrapping_add(shift_frames.wrapping_mul(epoch.parts as u32));
        epoch.frame_base = epoch.frame_base.wrapping_add(shift_frames);
    }

    fn locate(
        &mut self,
        wire_seq: u32,
        timestamp_us: u64,
        format: audiohub_net::media::WireFormat,
        parts: usize,
    ) -> Option<WirePacketPosition> {
        if !(1..=audiohub_net::media::MAX_WIRE_PARTS).contains(&parts) {
            return None;
        }

        let tag = media_frame_tag(timestamp_us);
        let tag_part_valid = self.frame_tag_version < 1 || tag.part < parts;
        let part = if self.frame_tag_version >= 1 && tag_part_valid {
            tag.part
        } else {
            wire_seq as usize % parts
        };
        let wire_base = wire_seq.wrapping_sub(part as u32);
        let Some(mut epoch) = self.current else {
            let frame_base = wire_base / parts as u32;
            let epoch = WireFormatEpoch {
                format,
                parts,
                wire_base,
                frame_base,
                high_wire_seq: wire_seq,
                high_frame_tag: tag.frame,
            };
            self.current = Some(epoch);
            return Some(WirePacketPosition {
                frame_seq: frame_base,
                part,
                transition: None,
            });
        };

        if epoch.format != format || epoch.parts != parts {
            // The sender changes formats only between complete 10 ms frames and
            // aligns its raw sequence to the new part count. The aligned base of
            // a real successor epoch is therefore strictly newer than every raw
            // sequence admitted under the old format. A delayed old packet fails
            // this test even after A -> B -> A, because the current epoch's base
            // remains newer than that packet.
            if !wire_seq_after(wire_base, epoch.high_wire_seq) {
                return None;
            }
            let high_offset = epoch.high_wire_seq.wrapping_sub(epoch.wire_base);
            if high_offset >= (1 << 31) {
                return None;
            }
            // Decompose the raw gap into an old-format tail, alignment padding,
            // and zero or more wholly lost new-format frames. The per-stream
            // authenticated frame tag fixes the total logical advance. If the
            // transition exceeds its exact 640 ms window (or is inconsistent),
            // commit it adjacent instead of rejecting every packet forever or
            // manufacturing an unproven gap. Legacy senders use that same safe
            // adjacent mapping because their timestamp low bits carry no tag.
            let exact_advance = if self.frame_tag_version >= 1 && tag_part_valid {
                transition_frame_advance(&epoch, wire_base, parts, tag.frame)
            } else {
                None
            };
            let frame_tag_fallback = self.frame_tag_version >= 1 && exact_advance.is_none();
            let frame_advance = exact_advance.unwrap_or(1);
            let previous = WireFormatTransition {
                from: epoch.format,
                from_parts: epoch.parts,
                to: format,
                to_parts: parts,
                frame_tag_fallback,
            };
            let frame_base = epoch
                .frame_base
                .wrapping_add(high_offset / epoch.parts as u32)
                .wrapping_add(frame_advance);
            epoch = WireFormatEpoch {
                format,
                parts,
                wire_base,
                frame_base,
                high_wire_seq: wire_seq,
                high_frame_tag: tag.frame,
            };
            self.current = Some(epoch);
            return Some(WirePacketPosition {
                frame_seq: frame_base,
                part,
                transition: Some(previous),
            });
        }

        let mut offset = wire_seq.wrapping_sub(epoch.wire_base);
        if offset >= (1 << 31) {
            // This is either from an older same-format epoch or older than the
            // unambiguous half of the wrapping sequence space. Neither can be
            // allowed to mutate the current assembly.
            return None;
        }
        if self.frame_tag_version >= 1 && tag_part_valid && tag.part != offset as usize % parts {
            return None;
        }
        if wire_seq_after(wire_seq, epoch.high_wire_seq) {
            epoch.high_wire_seq = wire_seq;
            epoch.high_frame_tag = tag.frame;
            // Keep every comparison inside the unambiguous half of wrapping
            // sequence space for indefinitely running fixed-quality streams.
            // Four retained frames exceed the reassembler's two-frame window.
            Self::rebase_behind_high_water(&mut epoch);
            offset = wire_seq.wrapping_sub(epoch.wire_base);
        }
        let position = WirePacketPosition {
            frame_seq: epoch.frame_base.wrapping_add(offset / parts as u32),
            part: offset as usize % parts,
            transition: None,
        };
        self.current = Some(epoch);
        Some(position)
    }
}

/// Resolve loss and alignment around a new format epoch.
///
/// Let `x` be wholly lost old-format frames after the old frame containing the
/// high packet, and `y` wholly lost new-format frames before the first observed
/// new packet. For old part `h`, old width `a`, new width `b`, and alignment
/// padding `p`, the raw distance is `(a - h) + x*a + p + y*b`, while the logical
/// frame advance is `1 + x + y`. The authenticated per-stream counter supplies
/// that advance modulo 64 and the same tag supplies the observed packet part.
/// Restricting the transition to 1..=64 makes the answer unique;
/// callers explicitly re-anchor adjacent outside that bounded exact window.
fn transition_frame_advance(
    epoch: &WireFormatEpoch,
    new_wire_base: u32,
    new_parts: usize,
    new_frame_tag: u8,
) -> Option<u32> {
    let old_offset = epoch.high_wire_seq.wrapping_sub(epoch.wire_base);
    if old_offset >= (1 << 31) {
        return None;
    }
    let old_parts = epoch.parts as u32;
    let new_parts = new_parts as u32;
    let old_part = old_offset % old_parts;
    let raw_span = new_wire_base.wrapping_sub(epoch.high_wire_seq);
    if raw_span == 0 || raw_span >= (1 << 31) {
        return None;
    }
    let completion_packets = old_parts - old_part;
    if raw_span < completion_packets {
        return None;
    }
    // Every logical frame beyond completion consumes at least min(a, b) raw
    // positions; alignment consumes additional positions and can only lower
    // the possible advance. This upper bound prevents a real K=128/192 gap
    // from aliasing to the six-bit K=64 tag value.
    let max_possible_advance = 1 + (raw_span - completion_packets) / old_parts.min(new_parts);
    if max_possible_advance > MAX_TAGGED_TRANSITION_FRAMES {
        return None;
    }
    let tag_delta =
        (new_frame_tag.wrapping_sub(epoch.high_frame_tag) as u32) & MEDIA_FRAME_COUNTER_MASK as u32;
    let frame_advance = if tag_delta == 0 {
        MAX_TAGGED_TRANSITION_FRAMES
    } else {
        tag_delta
    };
    if !(1..=MAX_TAGGED_TRANSITION_FRAMES).contains(&frame_advance) {
        return None;
    }

    // `frame_advance = 1 + old_whole_lost + new_whole_lost`.
    for old_whole_lost in 0..frame_advance {
        let new_whole_lost = frame_advance - 1 - old_whole_lost;
        let next_old_seq = epoch
            .high_wire_seq
            .wrapping_add(old_parts - old_part)
            .wrapping_add(old_whole_lost.wrapping_mul(old_parts));
        let expected_observed_base = align_wire_seq(next_old_seq, new_parts)
            .wrapping_add(new_whole_lost.wrapping_mul(new_parts));
        if expected_observed_base == new_wire_base {
            return Some(frame_advance);
        }
    }
    None
}

/// The two adjacent wire frames that may legitimately be in flight together.
///
/// Packet reordering across a frame boundary is normal: part 0 of frame N+1
/// can arrive before the last parts of frame N.  Keeping only one partial frame
/// turned that reorder into artificial concealment.  These slots stay ordered
/// (`older`, then `newer`) and `retired_through` prevents a late packet from
/// resurrecting a frame that has already been delivered or declared missing.
#[derive(Default)]
pub(crate) struct WireFrameAssembly {
    older: Option<crate::PartialWireFrame>,
    newer: Option<crate::PartialWireFrame>,
    retired_through: Option<u32>,
}

impl WireFrameAssembly {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One datagram can unblock both adjacent frames, so reassembly may return two
/// frames.  A fixed batch avoids another allocation on the receive path.
struct ReassembledWireBatch {
    frames: [Option<ReassembledWireFrame>; 2],
    len: usize,
}

impl ReassembledWireBatch {
    fn new() -> Self {
        Self {
            frames: [None, None],
            len: 0,
        }
    }

    fn push(&mut self, frame: ReassembledWireFrame) {
        debug_assert!(self.len < self.frames.len());
        self.frames[self.len] = Some(frame);
        self.len += 1;
    }
}

/// Finalize an incomplete packet set without adding another buffering tier.
///
/// At least half of the chunks must be real. Otherwise returning `None` leaves
/// the frame sequence absent and lets the jitter buffer apply its whole-frame
/// PLC. When enough chunks survived, every real chunk remains bit-identical in
/// its original position. Each missing run is filled by fading from the nearest
/// real chunk before it toward the nearest real chunk after it; a prefix fades
/// in from zero and a suffix fades out to zero. Gains advance per audio frame,
/// not per scalar, so stereo channels cannot receive different envelopes.
fn finalize_partial_wire_frame(pending: crate::PartialWireFrame) -> Option<ReassembledWireFrame> {
    let parts = pending.parts;
    if !(2..=audiohub_net::media::MAX_WIRE_PARTS).contains(&parts) {
        return None;
    }
    let present = pending.chunks[..parts]
        .iter()
        .filter(|chunk| chunk.is_some())
        .count();
    if present * 2 < parts {
        return None;
    }
    let chunk_len = pending.chunks[..parts]
        .iter()
        .find_map(|chunk| chunk.as_ref().map(Vec::len))?;
    let channels = pending.channels.clamp(1, 2) as usize;
    if chunk_len == 0
        || chunk_len % channels != 0
        || pending.chunks[..parts]
            .iter()
            .flatten()
            .any(|chunk| chunk.len() != chunk_len)
    {
        return None;
    }

    let partial_conceal = present != parts;
    let chunk_frames = chunk_len / channels;
    let mut samples = Vec::with_capacity(chunk_len * parts);
    for part in 0..parts {
        if let Some(chunk) = pending.chunks[part].as_ref() {
            samples.extend_from_slice(chunk);
            continue;
        }

        let mut run_start = part;
        while run_start > 0 && pending.chunks[run_start - 1].is_none() {
            run_start -= 1;
        }
        let mut run_end = part + 1;
        while run_end < parts && pending.chunks[run_end].is_none() {
            run_end += 1;
        }
        let left = run_start
            .checked_sub(1)
            .and_then(|index| pending.chunks[index].as_deref());
        let right = (run_end < parts)
            .then(|| pending.chunks[run_end].as_deref())
            .flatten();
        let hidden_frames = (run_end - run_start) * chunk_frames;
        let hidden_base = (part - run_start) * chunk_frames;

        for frame in 0..chunk_frames {
            let u = (hidden_base + frame + 1) as f32 / (hidden_frames + 1) as f32;
            for channel in 0..channels {
                let scalar = frame * channels + channel;
                let value = match (left, right) {
                    (Some(before), Some(after)) => before[scalar] * (1.0 - u) + after[scalar] * u,
                    (Some(before), None) => before[scalar] * (1.0 - u),
                    (None, Some(after)) => after[scalar] * u,
                    (None, None) => 0.0,
                };
                samples.push(value);
            }
        }
    }

    Some(ReassembledWireFrame {
        frame_seq: pending.frame_seq,
        sample_rate: pending.sample_rate,
        channels: pending.channels,
        samples,
        partial_conceal,
    })
}

fn partial_wire_frame_with_chunk(
    frame_seq: u32,
    part: usize,
    parts: usize,
    sample_rate: u32,
    channels: u8,
    decoded: Vec<f32>,
) -> crate::PartialWireFrame {
    let mut current = empty_partial_wire_frame(frame_seq, parts, sample_rate, channels);
    current.chunks[part] = Some(decoded);
    current
}

fn empty_partial_wire_frame(
    frame_seq: u32,
    parts: usize,
    sample_rate: u32,
    channels: u8,
) -> crate::PartialWireFrame {
    crate::PartialWireFrame {
        frame_seq,
        parts,
        sample_rate,
        channels,
        chunks: std::array::from_fn(|_| None),
    }
}

fn wire_frame_complete(frame: &crate::PartialWireFrame) -> bool {
    frame.chunks[..frame.parts].iter().all(Option::is_some)
}

/// Sequence comparison for a wrapping u32 media timeline.
fn wire_seq_after(candidate: u32, anchor: u32) -> bool {
    let distance = candidate.wrapping_sub(anchor);
    distance != 0 && distance < (1 << 31)
}

fn retire_oldest_wire_frame(assembly: &mut WireFrameAssembly, ready: &mut ReassembledWireBatch) {
    let Some(oldest) = assembly.older.take() else {
        return;
    };
    assembly.retired_through = Some(oldest.frame_seq);
    if let Some(frame) = finalize_partial_wire_frame(oldest) {
        ready.push(frame);
    }
    assembly.older = assembly.newer.take();
}

fn deliver_complete_wire_frames(
    assembly: &mut WireFrameAssembly,
    ready: &mut ReassembledWireBatch,
) {
    while assembly.older.as_ref().is_some_and(wire_frame_complete) {
        retire_oldest_wire_frame(assembly, ready);
    }
}

fn insert_wire_chunk(
    frame: &mut crate::PartialWireFrame,
    frame_seq: u32,
    part: usize,
    parts: usize,
    sample_rate: u32,
    channels: u8,
    decoded: Vec<f32>,
) {
    if frame.frame_seq == frame_seq
        && frame.parts == parts
        && frame.sample_rate == sample_rate
        && frame.channels == channels
    {
        frame.chunks[part] = Some(decoded);
    } else {
        // A single logical frame cannot safely combine chunks carrying
        // different format metadata. Replace it and leave the discarded
        // instance as a sequence hole instead of publishing mixed-format data.
        *frame =
            partial_wire_frame_with_chunk(frame_seq, part, parts, sample_rate, channels, decoded);
    }
}

/// Insert one decoded wire chunk and return completed frames in strict order.
///
/// Frames N and N+1 may coexist. N is finalized only when it becomes complete,
/// or when N+2 establishes that its reorder window has expired. A complete N+1
/// waits behind incomplete N; when a late chunk completes N, both frames can be
/// returned by the same call. Fewer than half of an expired frame's chunks still
/// produce no frame, preserving the jitter buffer's whole-frame PLC semantics.
fn collect_wire_chunk(
    assembly: &mut WireFrameAssembly,
    frame_seq: u32,
    part: usize,
    parts: usize,
    sample_rate: u32,
    channels: u8,
    decoded: Vec<f32>,
) -> ReassembledWireBatch {
    let mut ready = ReassembledWireBatch::new();
    if parts == 1 {
        assembly.clear();
        ready.push(ReassembledWireFrame {
            frame_seq,
            sample_rate,
            channels,
            samples: decoded,
            partial_conceal: false,
        });
        return ready;
    }
    if parts > audiohub_net::media::MAX_WIRE_PARTS || part >= parts {
        return ready;
    }

    if let Some(retired) = assembly.retired_through {
        if !wire_seq_after(frame_seq, retired) {
            return ready;
        }
    }

    let mut decoded = Some(decoded);
    loop {
        let Some(older_seq) = assembly.older.as_ref().map(|frame| frame.frame_seq) else {
            let incoming = partial_wire_frame_with_chunk(
                frame_seq,
                part,
                parts,
                sample_rate,
                channels,
                decoded.take().expect("wire chunk inserted once"),
            );
            if assembly
                .retired_through
                .is_some_and(|retired| frame_seq.wrapping_sub(retired) == 2)
            {
                // Preserve the one missing adjacent frame as an empty slot. A
                // late N+1 can still fill it; N+2 expires it normally.
                assembly.older = Some(empty_partial_wire_frame(
                    frame_seq.wrapping_sub(1),
                    parts,
                    sample_rate,
                    channels,
                ));
                assembly.newer = Some(incoming);
            } else {
                assembly.older = Some(incoming);
            }
            break;
        };

        if frame_seq == older_seq {
            insert_wire_chunk(
                assembly.older.as_mut().expect("older slot exists"),
                frame_seq,
                part,
                parts,
                sample_rate,
                channels,
                decoded.take().expect("wire chunk inserted once"),
            );
            break;
        }

        if assembly
            .newer
            .as_ref()
            .is_some_and(|frame| frame.frame_seq == frame_seq)
        {
            insert_wire_chunk(
                assembly.newer.as_mut().expect("newer slot exists"),
                frame_seq,
                part,
                parts,
                sample_rate,
                channels,
                decoded.take().expect("wire chunk inserted once"),
            );
            break;
        }

        if assembly.newer.is_none() && older_seq.wrapping_sub(frame_seq) == 1 {
            // The first packet we saw belonged to N+1. Admit the immediately
            // preceding N only if it has not already crossed the retired floor.
            assembly.newer = assembly.older.take();
            assembly.older = Some(partial_wire_frame_with_chunk(
                frame_seq,
                part,
                parts,
                sample_rate,
                channels,
                decoded.take().expect("wire chunk inserted once"),
            ));
            break;
        }

        let forward = frame_seq.wrapping_sub(older_seq);
        if forward == 1 {
            debug_assert!(assembly.newer.is_none());
            assembly.newer = Some(partial_wire_frame_with_chunk(
                frame_seq,
                part,
                parts,
                sample_rate,
                channels,
                decoded.take().expect("wire chunk inserted once"),
            ));
            break;
        }
        if forward < (1 << 31) {
            // N+2 (or a larger forward jump) is the explicit expiry boundary.
            // Retire in order until the incoming frame fits the two-slot window.
            retire_oldest_wire_frame(assembly, &mut ready);
            deliver_complete_wire_frames(assembly, &mut ready);
            continue;
        }

        // Older than both live slots: it is late and must not evict either.
        return ready;
    }

    deliver_complete_wire_frames(assembly, &mut ready);
    ready
}

pub(crate) fn handle_datagram(inner: &DaemonInner, dg: &[u8], from: SocketAddr) {
    let Ok((h, _payload)) = Header::parse(dg) else {
        return;
    };
    match h.kind {
        Kind::Media => {
            let rx = rd(&inner.rx_table).get(&h.stream_id).cloned();
            let Some(rx) = rx else { return };
            // Before the AEAD, before the codec check, before anything that can
            // reject this datagram: the tier watchdog's question is whether the
            // *network* delivered anything, and every early return below is a
            // fault this daemon can diagnose on its own. See `RxStream::media_seen`.
            rx.media_seen.fetch_add(1, Ordering::Relaxed);
            // tampered/foreign. Counted, because this was the one drop on the
            // media path with no number anywhere behind it — and on tier 1 it
            // is the only thing that can explain a `frames_read` that climbs
            // while `received` does not (`frames_read` counts every Kind::Media
            // frame off the socket, authenticated or not).
            let Ok((h, plain)) = rx.crypto.open(dg) else {
                lk(&rx.stats).auth_failed += 1;
                return;
            };
            // 位深由包头的 `codec` 决定，**不是由我们的假设决定**。认不出的
            // codec（Opus / Passthrough / 将来的新值）直接丢包：按 s16 硬解一个
            // 24 位载荷会得到一段**有声音、但全是垃圾**的波形，没有任何一处会报错。
            let Some(depth) = h.codec.wire_depth() else {
                dlog!(
                    "[audiohubd] stream {} 收到非 PCM codec {:?}，丢弃",
                    h.stream_id,
                    h.codec
                );
                return;
            };
            let arrival = inner.start.elapsed().as_micros() as u64;
            let mut jit_ms = 0.0f32;
            {
                let mut c = lk(&rx.stats);
                if c.first.is_none() {
                    c.first = Some(Instant::now());
                }
                // 喂给 RTP 式统计的是**线上序号**，不是下面算出来的帧序号：
                // 深档每帧两个包，两个包各自是一次真实的到达/丢失。
                // `plain.len()` and `dg.len()` are two different quantities and
                // both get counted: the payload is what the bit-depth rung
                // changes, `dg.len()` is what the link actually carries.
                c.rx.on_packet(h.seq, h.timestamp_us, arrival, plain.len(), dg.len());
                c.last_rate = h.sample_rate;
                c.last_depth = Some(depth);
                let transit = arrival as i64 - h.timestamp_us as i64;
                if let Some(p) = c.prev_transit {
                    jit_ms = (transit - p).unsigned_abs() as f32 / 1000.0;
                    c.note_jitter(jit_ms); // feeds the per-interval Stats window
                }
                c.prev_transit = Some(transit);
                // The rolling window `spread_ms` is read off. Fed the raw
                // `transit`, not the first difference: the window's own minimum
                // is the reference point, which is what cancels the offset
                // between two unsynchronised clocks. **Both quantities are kept**
                // — `jitter_ms` still drives tier 0 (design §3.4 scope rule:
                // replacing it there would change every existing user's AUTO and
                // jitter-buffer depth, and this round has no controlled data for
                // that), `spread_ms` drives tier 1/2 and is reported everywhere.
                c.spread.push(transit);
            }
            let mut decoded = Vec::new();
            if rx.spatial.is_some() {
                crate::spatial::receive(&rx, &h, &plain, jit_ms);
                return;
            }
            let dec = dsp::decode_pcm_into(&plain, depth, &mut decoded);
            if dec.nonfinite > 0 || dec.ragged > 0 {
                // f32 档独有的故障面：一个 NaN 经 `mixer_loop` 的求和会扩散成
                // 整段静音或爆音。已经被 `decode_pcm_into` 置 0，这里只负责
                // **让它说出来**——静默消毒与静默错解一样坏。
                dlog!(
                    "[audiohubd] stream {} 解码消毒：非有限 {} 个、残字节 {}（codec {:?}）",
                    h.stream_id,
                    dec.nonfinite,
                    dec.ragged,
                    h.codec
                );
            }
            // The stream width is frozen by OpenStream. A header that changes
            // it mid-stream would reinterpret every scalar after this point.
            let wire_channels = h.channels.clamp(1, 2);
            if audiohub_net::media::rung_of(h.sample_rate, depth).is_none() {
                let mut c = lk(&rx.stats);
                c.format_mismatch += 1;
                dlog!(
                    "[audiohubd] stream {} received format outside the ladder {:?} @ {} Hz; dropping",
                    h.stream_id,
                    depth,
                    h.sample_rate
                );
                return;
            }
            let fmt = audiohub_net::media::WireFormat {
                rate_hz: h.sample_rate,
                depth,
            };
            let parts = fmt.wire_packets_per_frame_for(wire_channels);
            let full = (h.sample_rate as usize / 100).max(1) * wire_channels as usize;

            // ---- 包头声明的格式必须与载荷长度**一一对应** ---------------------
            //
            // # 为什么 `DecodeStats.ragged` 抓不到这件事（一个都抓不到）
            //
            // `ragged` 是「载荷字节数不是位深宽度的整数倍」。而 48 kHz 下一帧的
            // 三个合法字节数是 960 / 1440 / 1920（= 480 × {2,3,4}），半帧是它们
            // 的一半 —— **这六个数被 2、3、4 除的余数全部为 0**。「声明 A、实为
            // B」的 12 种组合枚举下来，`ragged` **全部为 0**。
            //
            // 下游同样一言不发：`JitterBuffer::push` 无条件 `frame_len =
            // frame.len()`，短帧照收；`PostMix::advance` 对不足的部分零填充且
            // 不计数。稳态表现是每 10 ms 有一段静音洞（例如声明 s24 实为 s16
            // 时是 3.3 ms ⇒ 100 Hz 蜂鸣），而 JB 的 popped / plc / underruns /
            // dropped **全部一片正常**。这正是本项目反复栽的那个形态。
            //
            // 判据写成 `decoded.len() * parts != full` 可同时覆盖两支：
            // `parts == 2` 时由构造即成立（等式就是它的判据），
            // `parts == 1` 时退化成 `decoded.len() != full` —— 也就是原先
            // **完全没有被检查过**的那一支。
            //
            // ⚠ 这一段必须留在 `lk(&rx.jbs)` **之前**：它要取 `rx.stats`，而这条
            // 函数里既有的锁序是 stats → jbs（上面那个作用域先取 stats 再放）。
            // 在持有 jbs 时反向去取 stats 会引入一条相反的锁序。
            if h.channels != rx.channels || decoded.len() * parts != full {
                let n = {
                    let mut c = lk(&rx.stats);
                    c.format_mismatch += 1;
                    c.format_mismatch
                };
                // 五个数一个都不能少：少任何一个都定位不到是哪一端把格式写错了。
                // codec + sample_rate 是**对端声明**的，plain.len() 是它**实际
                // 发**的字节数，decoded.len() / full 是按声明解出来 vs 应有的
                // 样本数。三者一对照，错在哪一维一眼可见。
                dlog!(
                    "[audiohubd] stream {} 包头格式与载荷长度对不上（第 {n} 次）：\
                     codec {:?} @ {} Hz x {}ch (negotiated {}ch), payload {} B decoded to {} \
                     scalars, expected {} scalars / {} parts; dropping",
                    h.stream_id,
                    h.codec,
                    h.sample_rate,
                    h.channels,
                    rx.channels,
                    plain.len(),
                    decoded.len(),
                    full,
                    parts,
                );
                return;
            }

            let mut st = lk(&rx.jbs);
            let Some(position) = st.wire_timeline.locate(h.seq, h.timestamp_us, fmt, parts) else {
                // Authentication succeeded, but the packet belongs to an epoch
                // older than the committed format boundary. It has already fed
                // packet-level transport statistics; it must not touch format,
                // reassembly, resampling, or the jitter buffer.
                return;
            };
            if let Some(change) = position.transition {
                // Chunks from two epochs can never form one audio frame. Drop at
                // most the two-frame reorder window, but keep the jitter buffer,
                // its learned target, and its concealment history. The stable
                // local frame clock makes a rebuild both unnecessary and harmful.
                st.partial.clear();
                if change.frame_tag_fallback {
                    dlog!(
                        "[audiohubd] stream {} media frame tag could not prove a format-boundary \
                         gap at packet {}; re-anchoring the new epoch adjacent",
                        h.stream_id,
                        h.seq,
                    );
                }
                dlog!(
                    "[audiohubd] stream {} wire format {:?}/{} parts -> {:?}/{} parts at packet {}",
                    h.stream_id,
                    change.from,
                    change.from_parts,
                    change.to,
                    change.to_parts,
                    h.seq,
                );
            }
            let ready = collect_wire_chunk(
                &mut st.partial,
                position.frame_seq,
                position.part,
                parts,
                h.sample_rate,
                wire_channels,
                decoded,
            );
            st.jit_win.push(jit_ms);
            if st.jit_win.len() > 256 {
                st.jit_win.remove(0);
            }

            let pushes_before = st.pushes;
            for ready_frame in ready.frames.into_iter().flatten() {
                let ReassembledWireFrame {
                    frame_seq,
                    sample_rate: frame_rate,
                    channels: frame_channels,
                    samples: raw,
                    partial_conceal,
                } = ready_frame;
                if partial_conceal {
                    // The jitter buffer sees a complete-length frame and therefore
                    // cannot account for this upstream concealment itself.
                    st.half_conceal = st.half_conceal.saturating_add(1);
                }
                let last_frame = raw
                    .chunks_exact(frame_channels as usize)
                    .last()
                    .map(|frame| {
                        let mut last = [0.0; 2];
                        last[..frame_channels as usize].copy_from_slice(frame);
                        last
                    });
                let rs_last = st.rs_last;
                let crate::JbState { rs, rs_rate, .. } = &mut *st;
                let frame =
                    resample_received_frame(rs, rs_rate, rs_last, frame_rate, frame_channels, raw);
                if let Some(last) = last_frame {
                    st.rs_last = last;
                }
                // Empty frames never enter the JB: `push` would consume a
                // sequence number while carrying no audio, which is worse than
                // leaving the sequence absent for ordinary PLC.
                if frame.is_empty() {
                    continue;
                }
                st.jb.push(frame_seq, frame.clone());

                // Starvation self-heal runs once per completed FRAME, not once
                // per wire part. A single datagram may now release two ordered
                // frames, so both must advance these counters independently.
                if st.jb.dropped > st.last_dropped && st.jb.depth() <= 1 {
                    st.late_streak += 1;
                } else {
                    st.late_streak = 0;
                }
                st.last_dropped = st.jb.dropped;
                if st.late_streak >= 50 {
                    let target = st.jb.target();
                    // Restart this buffer, do **not** re-tune it. `JitterBuffer::new`
                    // would reach for `JbTuning::cached()` — i.e. `DEFAULT` — and a
                    // resync would silently swap a tier 1 stream's `DEGRADED`
                    // profile for the tier 0 one, on top of `with_tuning`'s
                    // `clamp(1, max_target)` chopping a learned depth of up to 40
                    // frames down to 12. The envelope comes back on the next
                    // `reshape_jitter_envelope` pass (<=1s), but its seed is
                    // `st.jb.target()` — already clamped — so the depth does not:
                    // it can only be re-earned one frame per underrun.
                    //
                    // The trigger is `late_streak >= 50`, i.e. arrivals judged late
                    // while the buffer sits near empty. That is precisely TCP's
                    // stall-then-burst shape, so the site fires *more* readily on
                    // the very link `DEGRADED` exists for. Same class of mistake as
                    // the stale-gate subject drift, one site over.
                    st.jb = audiohub_net::media::JitterBuffer::with_tuning_channels(
                        target,
                        st.jb.tuning(),
                        st.channels,
                    );
                    st.jb.push(frame_seq, frame);
                    st.partial.clear();
                    st.last_dropped = 0;
                    st.late_streak = 0;
                    // Reset all five lifetime counters with the new JB. This
                    // is a real discontinuity: old samples must not enter a
                    // delta and make the next 10-second window look perfect.
                    st.conceal.reset();
                    dlog!("[audiohubd] jb resync on stream {}", h.stream_id);
                }

                st.pushes += 1;
                // Fine-grained Q1 window sample (spec §4.6: every 10 frames,
                // about 100 ms). The ticker also adds one each second; that is
                // the only path still running during a blackout, when this
                // receive path does not execute and Q1 matters most.
                if st.pushes % 10 == 0 {
                    st.sample_conceal();
                }
            }
            let frame_ready = st.pushes != pushes_before;
            // ---- 谁来决定 JB 的目标深度：伺服，还是抖动公式 ----
            //
            // 固定延迟档下**必须**是伺服，而且抖动公式必须彻底闭嘴。两个都写，
            // 就是两条回路抢同一个水位：用户选的 200 ms 会在每一次 p95 更新时
            // 被改回抖动算出来的那个数，而界面照旧显示 200——「设置生效了」
            // 的错觉，正是本项目栽过五次的形态。
            let servo_want = rx.transport.servo_frames();
            if frame_ready && st.pushes / 100 != pushes_before / 100 {
                // 包络（min/max_target）只能在构造时给定。用户把目标从 100 ms
                // 拖到 1000 ms 时，默认包络 4..12 帧 = 40..120 ms 根本够不着，
                // 于是必须重建。每秒问一次、已经对了就立刻返回。
                let target = rx.transport.latency_target();
                let reseeded = reshape_jitter_envelope(
                    &mut st,
                    target,
                    jb_tuning_for(&rx.ka_path),
                    h.stream_id,
                );
                if reseeded {
                    // Invalidate the old servo output as part of the rebuild.
                    //
                    // The `Some(_) if reseeded` arm below skips only this
                    // pass; the next pass is 100 audio frames (about one
                    // second) later, and the servo may not have run between
                    // them. Reusing an output computed under the old envelope
                    // would immediately replace a fresh 30-frame preset with
                    // an old two- or three-frame target, then take almost 30
                    // seconds to climb back one frame at a time.
                    //
                    // Whether the servo happens to run just before or just
                    // after the rebuild must not decide the result. Clearing
                    // the old value turns that timing race into an explicit
                    // missing value that the next servo pass can replace.
                    rx.transport.set_servo_frames(None);
                }
                let servo_want = if reseeded { None } else { servo_want };
                match servo_want {
                    // 刚重建过：`servo_want` 是上一拍在**旧包络**下算的，
                    // 拿它执行会把刚落好的预置立刻推翻。让伺服下一拍重新算。
                    Some(_) if reseeded => {}
                    // 固定档：伺服说了算。
                    Some(want) => steer_jitter_target(&mut st.jb, want),
                    // 固定档 + 伺服还没有输出（刚重建，或刚换档）：**什么都不做**。
                    //
                    // 绝不能掉进下面那条抖动公式：这个模块开头那段注释写着
                    // 「固定延迟档下抖动公式必须彻底闭嘴」，而抖动公式会把
                    // 刚落好的 30 帧预置改回它自己算出来的 2 帧，界面照旧显示
                    // 300 ms —— 正是那段注释要消灭的形态。
                    None if !matches!(target, audiohub_ipc::LatencyTarget::Auto) => {}
                    // AUTO（plan §5）：抖动 p95 驱动，与改动前逐字相同。
                    None => {
                        if !st.jit_win.is_empty() {
                            let mut v = st.jit_win.clone();
                            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                            let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)] as f64;
                            st.jb.update_target(p95, FRAME_MS as f64);
                        }
                    }
                }
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
                    .and_then(|e| e.tx.clone().map(|t| (t, e.conn.peer_ip)))
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
                // 代号最后动，且用 Release：`tx_loop` 那边 Acquire 读到新代号时
                // 必须已经能看到新地址。反过来写就是「代号说变了、锁里还是旧值」，
                // 那条流会一直发到旧端口，而且下一次代号不会再动。
                t.dest_epoch.fetch_add(1, Ordering::Release);
            }
        }
        _ => {}
    }
}

/// Receiver-side keepalive (spec §3): one unencrypted PullReq per stream per
/// second toward the sender to hold NAT/firewall state.
///
/// # Tier 1 (M8): nothing to send, and nothing to send it to
///
/// A keepalive holds a UDP flow's NAT/firewall state open and teaches the
/// sender which port to answer on. A tier 1 stream has no UDP flow, and its
/// connection keeps its own state open by being a connection. Skipping is not
/// an optimisation: [`MediaPath::Tcp`] has no address, so there is literally
/// nowhere to address the datagram — which is why the early return reads it out
/// of the path rather than off a tier flag somebody has to keep in sync.
pub(crate) fn send_pullreq(inner: &DaemonInner, rx: &RxStream) {
    let Some(dest) = rx.ka_path.udp_dest() else {
        return;
    };
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
    // Same hook as the media path. A keepalive is a UDP datagram on the same
    // socket, so a simulated block that let it through would be simulating a
    // firewall nobody has — and it would defeat the very signal this keepalive
    // feeds (`autotier`'s second signal is "no keepalive ever arrived").
    if !inner.udp_block.out {
        let _ = inner.udp.send_to(&h.encode(&[]), dest);
    }
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

/// A single AirPlay source must remain amplitude-transparent: its sender dB is
/// represented by the receiver's native output control, not by a second
/// sample-domain gain or limiter. Multiple local contributors still use the
/// site's frozen soft-knee protection. Clamp is only a final malformed-sample
/// guard; decoded network-audio i16 and the linear resampler are already in this range.
fn protect_output_sample(sample: f32, sole_airplay: bool) -> f32 {
    if sole_airplay {
        sample.clamp(-1.0, 1.0)
    } else {
        soft_clip(sample)
    }
}

fn add_airplay_to_local_mix(
    airplay_frame: &[[f32; 2]; F48],
    mix: &mut [f32; F48_STEREO],
    stereo_scratch: &mut [f32; F48_STEREO],
    corr_a: &mut [f32; F48_STEREO],
    contrib: &mut u32,
    corr: &mut Option<f64>,
) {
    for (dst, frame) in stereo_scratch.chunks_exact_mut(2).zip(airplay_frame) {
        dst.copy_from_slice(frame);
    }
    *contrib += 1;
    if *contrib == 1 {
        corr_a.copy_from_slice(stereo_scratch);
    } else if *contrib == 2 {
        *corr = crate::quality::correlation(corr_a, stereo_scratch);
    }
    for (dst, sample) in mix.iter_mut().zip(stereo_scratch.iter()) {
        *dst += *sample;
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

const SITE_CALLBACK_STALL_THRESHOLD: Duration = Duration::from_millis(250);
const UNKNOWN_CALLBACK_AGE_US: u64 = u64::MAX;
const SITE_PLAYBACK_RETRY_MIN: Duration = Duration::from_millis(250);
const SITE_PLAYBACK_RETRY_MAX: Duration = Duration::from_secs(2);
const SITE_PLAYBACK_STABLE_RUNNING: Duration = Duration::from_secs(1);
const SITE_PLAYBACK_OPEN_WATCHDOG: Duration = Duration::from_secs(5);
const SITE_PLAYBACK_OWNER_POLL: Duration = Duration::from_millis(25);
const SITE_PLAYBACK_MAX_ABANDONED_OWNERS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SitePlaybackOpenTicket {
    request_id: u64,
    device_epoch: u64,
    reason: &'static str,
    requested_at: Instant,
}

#[derive(Default)]
struct SitePlaybackBuildState {
    next_request_id: u64,
    pending: Option<SitePlaybackOpenTicket>,
}

impl SitePlaybackBuildState {
    fn begin(
        &mut self,
        device_epoch: u64,
        reason: &'static str,
        now: Instant,
    ) -> Option<SitePlaybackOpenTicket> {
        if self.pending.is_some() {
            return None;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let ticket = SitePlaybackOpenTicket {
            request_id: self.next_request_id,
            device_epoch,
            reason,
            requested_at: now,
        };
        self.pending = Some(ticket);
        Some(ticket)
    }

    fn invalidate(&mut self) -> Option<SitePlaybackOpenTicket> {
        self.pending.take()
    }

    fn complete_if_current(
        &mut self,
        request_id: u64,
        request_epoch: u64,
        current_epoch: u64,
    ) -> Option<SitePlaybackOpenTicket> {
        let current = self.pending.is_some_and(|pending| {
            pending.request_id == request_id
                && pending.device_epoch == request_epoch
                && request_epoch == current_epoch
        });
        current.then(|| self.pending.take().expect("current ticket exists"))
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// Requests coordinated away from the mixer. Each native default-output stream
/// remains on its per-generation owner thread; only its sendable monitor and
/// ring producer cross back to the mixer.
pub(crate) enum SitePlaybackBuildReq {
    Open {
        request_id: u64,
        device_epoch: u64,
        reason: &'static str,
    },
    Retire {
        request_id: u64,
    },
}

pub(crate) struct SitePlaybackBuildDone {
    request_id: u64,
    device_epoch: u64,
    reason: &'static str,
    open_elapsed: Duration,
    result: std::result::Result<(PlaybackMonitor, AudioTx), String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SitePlaybackOwnerPhase {
    Opening = 0,
    Ready = 1,
    Expired = 2,
    Finished = 3,
}

impl SitePlaybackOwnerPhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Ready,
            2 => Self::Expired,
            3 => Self::Finished,
            _ => Self::Opening,
        }
    }
}

struct SitePlaybackOwnerProgress {
    phase: AtomicU8,
}

impl SitePlaybackOwnerProgress {
    fn new() -> Self {
        Self {
            phase: AtomicU8::new(SitePlaybackOwnerPhase::Opening as u8),
        }
    }

    fn phase(&self) -> SitePlaybackOwnerPhase {
        SitePlaybackOwnerPhase::from_raw(self.phase.load(Ordering::Acquire))
    }

    /// Only one side may win the open/watchdog race. A late native open cannot
    /// publish Ready after the coordinator has expired its generation.
    fn opened(&self) -> bool {
        self.phase
            .compare_exchange(
                SitePlaybackOwnerPhase::Opening as u8,
                SitePlaybackOwnerPhase::Ready as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn open_failed(&self) -> bool {
        self.phase
            .compare_exchange(
                SitePlaybackOwnerPhase::Opening as u8,
                SitePlaybackOwnerPhase::Finished as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn expire_open(&self) -> bool {
        self.phase
            .compare_exchange(
                SitePlaybackOwnerPhase::Opening as u8,
                SitePlaybackOwnerPhase::Expired as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel(&self) {
        let mut phase = self.phase.load(Ordering::Acquire);
        while phase != SitePlaybackOwnerPhase::Expired as u8
            && phase != SitePlaybackOwnerPhase::Finished as u8
        {
            match self.phase.compare_exchange_weak(
                phase,
                SitePlaybackOwnerPhase::Expired as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => phase = observed,
            }
        }
    }

    fn finish(&self) {
        self.phase
            .store(SitePlaybackOwnerPhase::Finished as u8, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy)]
enum SitePlaybackOwnerCmd {
    Retire,
}

struct SitePlaybackOwner {
    control: mpsc::Sender<SitePlaybackOwnerCmd>,
    progress: Arc<SitePlaybackOwnerProgress>,
    requested_at: Instant,
    device_epoch: u64,
    reason: &'static str,
}

fn site_playback_owner_budget_allows(abandoned: usize) -> bool {
    abandoned < SITE_PLAYBACK_MAX_ABANDONED_OWNERS
}

fn site_playback_owner_watchdog_due(requested_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(requested_at) >= SITE_PLAYBACK_OPEN_WATCHDOG
}

fn site_playback_owner_loop(
    request_id: u64,
    device_epoch: u64,
    reason: &'static str,
    control: mpsc::Receiver<SitePlaybackOwnerCmd>,
    progress: Arc<SitePlaybackOwnerProgress>,
    done: mpsc::Sender<SitePlaybackBuildDone>,
) {
    let cancelled = || !matches!(control.try_recv(), Err(mpsc::TryRecvError::Empty));
    if cancelled() {
        progress.finish();
        return;
    }

    let started = Instant::now();
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        LivePlayback::start_channels(48_000, 2)
    }));
    let open_elapsed = started.elapsed();
    match opened {
        Ok(Ok((playback, tx))) => {
            // Expiry/cancellation wins by changing Opening first. The native
            // handle is still local here, so every losing path drops it on the
            // exact thread which created it.
            if cancelled() || !progress.opened() {
                drop(playback);
                progress.finish();
                return;
            }
            if cancelled() {
                drop(playback);
                progress.finish();
                return;
            }
            let monitor = playback.monitor();
            if done
                .send(SitePlaybackBuildDone {
                    request_id,
                    device_epoch,
                    reason,
                    open_elapsed,
                    result: Ok((monitor, tx)),
                })
                .is_err()
            {
                drop(playback);
                progress.finish();
                return;
            }

            let _ = control.recv();
            drop(playback);
            progress.finish();
        }
        Ok(Err(error)) => {
            if progress.open_failed() {
                let _ = done.send(SitePlaybackBuildDone {
                    request_id,
                    device_epoch,
                    reason,
                    open_elapsed,
                    result: Err(format!("{error:#}")),
                });
            }
            progress.finish();
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic payload");
            if progress.open_failed() {
                let _ = done.send(SitePlaybackBuildDone {
                    request_id,
                    device_epoch,
                    reason,
                    open_elapsed,
                    result: Err(format!("default-output playback open panicked: {message}")),
                });
            }
            progress.finish();
        }
    }
}

fn stop_site_playback_owners(owners: &mut HashMap<u64, SitePlaybackOwner>) {
    for (_, owner) in owners.drain() {
        owner.progress.cancel();
        let _ = owner.control.send(SitePlaybackOwnerCmd::Retire);
    }
}

/// Coordinates default-output owners away from the 10 ms mixer.
///
/// Each native open runs on its own detached owner thread. A successful owner
/// retains and drops its `LivePlayback` on that same thread. The tracked
/// coordinator never waits for an uninterruptible host call, so its shutdown
/// and the daemon's join remain bounded even if one native open never returns.
/// Owners deliberately receive no `DaemonInner`: after shutdown, a stuck host
/// call can retain only its small control/progress channels, not daemon sockets,
/// HAL state, peers, or other restart-sensitive resources.
pub(crate) fn site_playback_builder_loop(
    inner: Arc<DaemonInner>,
    reqs: mpsc::Receiver<SitePlaybackBuildReq>,
    done: mpsc::Sender<SitePlaybackBuildDone>,
) {
    let mut owners: HashMap<u64, SitePlaybackOwner> = HashMap::new();
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            stop_site_playback_owners(&mut owners);
            return;
        }

        match reqs.recv_timeout(SITE_PLAYBACK_OWNER_POLL) {
            Ok(SitePlaybackBuildReq::Open {
                request_id,
                device_epoch,
                reason,
            }) => {
                let abandoned = owners
                    .values()
                    .filter(|owner| owner.progress.phase() == SitePlaybackOwnerPhase::Expired)
                    .count();
                if !site_playback_owner_budget_allows(abandoned) {
                    let error = format!(
                        "default-output playback owner budget exhausted: {abandoned} native \
                         owners remain blocked after cancellation or watchdog expiry"
                    );
                    dlog!(
                        "[audiohubd] site playback owner rejected request={} epoch={} reason={} \
                         abandoned={} limit={}",
                        request_id,
                        device_epoch,
                        reason,
                        abandoned,
                        SITE_PLAYBACK_MAX_ABANDONED_OWNERS,
                    );
                    if done
                        .send(SitePlaybackBuildDone {
                            request_id,
                            device_epoch,
                            reason,
                            open_elapsed: Duration::ZERO,
                            result: Err(error),
                        })
                        .is_err()
                    {
                        stop_site_playback_owners(&mut owners);
                        return;
                    }
                    continue;
                }

                if let Some(previous) = owners.remove(&request_id) {
                    previous.progress.cancel();
                    let _ = previous.control.send(SitePlaybackOwnerCmd::Retire);
                }

                let requested_at = Instant::now();
                let progress = Arc::new(SitePlaybackOwnerProgress::new());
                let (control_send, control_recv) = mpsc::channel();
                let owner_progress = Arc::clone(&progress);
                let owner_done = done.clone();
                let owner_thread = std::thread::Builder::new()
                    .name(format!("ahb-playback-owner-{request_id}"))
                    .spawn(move || {
                        site_playback_owner_loop(
                            request_id,
                            device_epoch,
                            reason,
                            control_recv,
                            owner_progress,
                            owner_done,
                        )
                    });
                match owner_thread {
                    Ok(owner_thread) => {
                        // Dropping a JoinHandle detaches it. This is deliberate:
                        // an OS call with no cancellation API must not make the
                        // daemon's tracked-thread join unbounded.
                        drop(owner_thread);
                        owners.insert(
                            request_id,
                            SitePlaybackOwner {
                                control: control_send,
                                progress,
                                requested_at,
                                device_epoch,
                                reason,
                            },
                        );
                    }
                    Err(error) => {
                        if done
                            .send(SitePlaybackBuildDone {
                                request_id,
                                device_epoch,
                                reason,
                                open_elapsed: requested_at.elapsed(),
                                result: Err(format!(
                                    "spawn default-output playback owner: {error}"
                                )),
                            })
                            .is_err()
                        {
                            stop_site_playback_owners(&mut owners);
                            return;
                        }
                    }
                }
            }
            Ok(SitePlaybackBuildReq::Retire { request_id }) => {
                if let Some(owner) = owners.get(&request_id) {
                    owner.progress.cancel();
                    let _ = owner.control.send(SitePlaybackOwnerCmd::Retire);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop_site_playback_owners(&mut owners);
                return;
            }
        }

        owners.retain(|_, owner| owner.progress.phase() != SitePlaybackOwnerPhase::Finished);
        let now = Instant::now();
        let mut expired_results = Vec::new();
        for (&request_id, owner) in &owners {
            let elapsed = now.saturating_duration_since(owner.requested_at);
            if !site_playback_owner_watchdog_due(owner.requested_at, now)
                || !owner.progress.expire_open()
            {
                continue;
            }
            let _ = owner.control.send(SitePlaybackOwnerCmd::Retire);
            dlog!(
                "[audiohubd] site playback owner expired request={} epoch={} reason={} \
                 open_elapsed_ms={} watchdog_ms={}",
                request_id,
                owner.device_epoch,
                owner.reason,
                elapsed.as_millis(),
                SITE_PLAYBACK_OPEN_WATCHDOG.as_millis(),
            );
            expired_results.push(SitePlaybackBuildDone {
                request_id,
                device_epoch: owner.device_epoch,
                reason: owner.reason,
                open_elapsed: elapsed,
                result: Err(format!(
                    "default-output playback open exceeded the {} ms watchdog",
                    SITE_PLAYBACK_OPEN_WATCHDOG.as_millis()
                )),
            });
        }
        for result in expired_results {
            if done.send(result).is_err() {
                stop_site_playback_owners(&mut owners);
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SitePlaybackOpenCause {
    Initial,
    DefaultOutputChanged,
    StreamError,
    CallbackStall,
}

/// Retry state for the daemon's real default-output stream.
///
/// A fatal callback error or a callback stall retires the old stream, then the
/// mixer waits only a short bounded interval before reopening. A default-device
/// epoch change is authoritative and bypasses any retry that was queued for the
/// previous device.
struct SitePlaybackRetry {
    cause: SitePlaybackOpenCause,
    retrying: bool,
    not_before: Option<Instant>,
    next_delay: Duration,
    running_since: Option<Instant>,
    stable: bool,
}

impl SitePlaybackRetry {
    fn new() -> Self {
        Self {
            cause: SitePlaybackOpenCause::Initial,
            retrying: false,
            not_before: None,
            next_delay: SITE_PLAYBACK_RETRY_MIN,
            running_since: None,
            stable: false,
        }
    }

    fn open_reason(&self) -> &'static str {
        match (self.cause, self.retrying) {
            (SitePlaybackOpenCause::Initial, false) => "initial",
            (SitePlaybackOpenCause::Initial, true) => "retry_after_open_failure",
            (SitePlaybackOpenCause::DefaultOutputChanged, false) => "default_output_changed",
            (SitePlaybackOpenCause::DefaultOutputChanged, true) => {
                "retry_after_default_output_change"
            }
            (SitePlaybackOpenCause::StreamError, false) => "stream_error_recovery",
            (SitePlaybackOpenCause::StreamError, true) => "retry_after_stream_error",
            (SitePlaybackOpenCause::CallbackStall, false) => "callback_stall_recovery",
            (SitePlaybackOpenCause::CallbackStall, true) => "retry_after_callback_stall",
        }
    }

    fn ready(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|deadline| now >= deadline)
    }

    fn schedule(&mut self, now: Instant) -> Duration {
        let delay = self.next_delay;
        self.not_before = Some(now + delay);
        self.next_delay = std::cmp::min(delay + delay, SITE_PLAYBACK_RETRY_MAX);
        delay
    }

    fn after_stream_error(&mut self, now: Instant) -> Duration {
        self.cause = SitePlaybackOpenCause::StreamError;
        self.retrying = false;
        self.running_since = None;
        self.stable = false;
        self.schedule(now)
    }

    fn after_callback_stall(&mut self, now: Instant) -> Duration {
        self.cause = SitePlaybackOpenCause::CallbackStall;
        self.retrying = false;
        self.running_since = None;
        self.stable = false;
        self.schedule(now)
    }

    fn after_open_failure(&mut self, now: Instant) -> Duration {
        self.retrying = true;
        self.running_since = None;
        self.stable = false;
        self.schedule(now)
    }

    fn default_output_changed(&mut self) {
        self.cause = SitePlaybackOpenCause::DefaultOutputChanged;
        self.retrying = false;
        self.not_before = None;
        self.next_delay = SITE_PLAYBACK_RETRY_MIN;
        self.running_since = None;
        self.stable = false;
    }

    fn open_started(&mut self) {
        self.retrying = false;
        self.not_before = None;
        self.running_since = None;
        self.stable = false;
    }

    /// Clears accumulated backoff only after the replacement has delivered a
    /// continuous Running window. `start_channels()` returning `Ok` is not
    /// enough: WASAPI can report a fatal asynchronous error immediately after
    /// that return, and treating it as success creates a permanent 250 ms loop.
    fn observe_phase(&mut self, phase: SitePlaybackPhase, now: Instant) -> bool {
        if self.stable {
            return false;
        }
        if phase != SitePlaybackPhase::Running {
            self.running_since = None;
            return false;
        }
        let since = *self.running_since.get_or_insert(now);
        if now.saturating_duration_since(since) < SITE_PLAYBACK_STABLE_RUNNING {
            return false;
        }
        self.next_delay = SITE_PLAYBACK_RETRY_MIN;
        self.running_since = None;
        self.stable = true;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SitePlaybackPhase {
    Closed = 0,
    Starting = 1,
    Running = 2,
    Stalled = 3,
    Dead = 4,
}

impl SitePlaybackPhase {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Starting,
            2 => Self::Running,
            3 => Self::Stalled,
            4 => Self::Dead,
            _ => Self::Closed,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stalled => "stalled",
            Self::Dead => "dead",
        }
    }
}

fn classify_site_playback_phase(
    alive: bool,
    callback_count: u64,
    callback_age: Option<Duration>,
    age_since_open: Option<Duration>,
) -> SitePlaybackPhase {
    if !alive {
        return SitePlaybackPhase::Dead;
    }
    let stale = match callback_age {
        Some(age) => age > SITE_CALLBACK_STALL_THRESHOLD,
        None if callback_count == 0 => {
            age_since_open.is_some_and(|age| age > SITE_CALLBACK_STALL_THRESHOLD)
        }
        // The callback timestamp and count are deliberately relaxed atomics.
        // If a reader catches the count first, wait for a coherent snapshot on
        // the next tick instead of mistaking the stream's total age for a stall.
        None => false,
    };
    if stale {
        SitePlaybackPhase::Stalled
    } else if callback_count == 0 {
        SitePlaybackPhase::Starting
    } else {
        SitePlaybackPhase::Running
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SitePlaybackRetireCause {
    StreamError,
    CallbackStall,
}

fn site_playback_retire_cause(phase: SitePlaybackPhase) -> Option<SitePlaybackRetireCause> {
    match phase {
        SitePlaybackPhase::Dead => Some(SitePlaybackRetireCause::StreamError),
        SitePlaybackPhase::Stalled => Some(SitePlaybackRetireCause::CallbackStall),
        SitePlaybackPhase::Closed | SitePlaybackPhase::Starting | SitePlaybackPhase::Running => {
            None
        }
    }
}

struct InstalledSitePlayback {
    request_id: u64,
    monitor: PlaybackMonitor,
    tx: AudioTx,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SitePlaybackError {
    generation: u64,
    message: String,
}

#[derive(Debug, Default)]
struct SitePlaybackMetadata {
    open_reason: Option<String>,
    pending_open_reason: Option<String>,
    open_config: Option<PlaybackConfigSnapshot>,
    last_stream_error: Option<SitePlaybackError>,
}

/// Lock-free hot-path snapshot for the daemon's one real default-output stream.
/// Bridge outputs never publish here. The mutex contains transition-only text
/// and configuration; the 10 ms mixer path writes atomics only.
pub(crate) struct SitePlaybackProbe {
    generation: AtomicU64,
    phase: AtomicU8,
    callback_count: AtomicU64,
    callback_age_us: AtomicU64,
    ring_queued: AtomicU64,
    ring_capacity: AtomicU64,
    ring_dropped: AtomicU64,
    metadata: Mutex<SitePlaybackMetadata>,
}

impl SitePlaybackProbe {
    pub(crate) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            phase: AtomicU8::new(SitePlaybackPhase::Closed as u8),
            callback_count: AtomicU64::new(0),
            callback_age_us: AtomicU64::new(UNKNOWN_CALLBACK_AGE_US),
            ring_queued: AtomicU64::new(0),
            ring_capacity: AtomicU64::new(0),
            ring_dropped: AtomicU64::new(0),
            metadata: Mutex::new(SitePlaybackMetadata::default()),
        }
    }

    fn clear_current(&self) {
        self.callback_count.store(0, Ordering::Relaxed);
        self.callback_age_us
            .store(UNKNOWN_CALLBACK_AGE_US, Ordering::Relaxed);
        self.ring_queued.store(0, Ordering::Relaxed);
        self.ring_capacity.store(0, Ordering::Relaxed);
        self.ring_dropped.store(0, Ordering::Relaxed);
    }

    fn reopened(&self, config: &PlaybackConfigSnapshot, reason: &str) -> u64 {
        let generation = self.generation.load(Ordering::Relaxed).wrapping_add(1);
        {
            let mut metadata = lk(&self.metadata);
            metadata.open_reason = Some(reason.to_owned());
            metadata.pending_open_reason = None;
            metadata.open_config = Some(config.clone());
        }
        self.clear_current();
        self.generation.store(generation, Ordering::Release);
        self.phase
            .store(SitePlaybackPhase::Starting as u8, Ordering::Release);
        generation
    }

    fn reopen_requested(&self, reason: &str) {
        {
            let mut metadata = lk(&self.metadata);
            metadata.open_reason = None;
            metadata.pending_open_reason = Some(reason.to_owned());
            metadata.open_config = None;
        }
        self.clear_current();
        self.phase
            .store(SitePlaybackPhase::Closed as u8, Ordering::Release);
    }

    fn open_failed(&self, retry_reason: &str) {
        {
            let mut metadata = lk(&self.metadata);
            metadata.open_reason = None;
            metadata.pending_open_reason = Some(retry_reason.to_owned());
            metadata.open_config = None;
        }
        self.clear_current();
        self.phase
            .store(SitePlaybackPhase::Closed as u8, Ordering::Release);
    }

    fn publish(
        &self,
        phase: SitePlaybackPhase,
        callback_count: u64,
        callback_age: Option<Duration>,
        tx: &AudioTx,
    ) {
        self.callback_count.store(callback_count, Ordering::Relaxed);
        self.callback_age_us.store(
            callback_age
                .map(|age| age.as_micros().min(u64::MAX as u128) as u64)
                .unwrap_or(UNKNOWN_CALLBACK_AGE_US),
            Ordering::Relaxed,
        );
        self.ring_queued
            .store(tx.queued() as u64, Ordering::Relaxed);
        self.ring_capacity
            .store(tx.capacity() as u64, Ordering::Relaxed);
        self.ring_dropped.store(tx.dropped(), Ordering::Relaxed);
        self.phase.store(phase as u8, Ordering::Release);
    }

    fn record_stream_error(&self, generation: u64, message: String) {
        lk(&self.metadata).last_stream_error = Some(SitePlaybackError {
            generation,
            message,
        });
    }

    fn status(&self) -> serde_json::Value {
        let phase = SitePlaybackPhase::from_raw(self.phase.load(Ordering::Acquire));
        let present = phase != SitePlaybackPhase::Closed;
        let callback_age_us = self.callback_age_us.load(Ordering::Relaxed);
        let metadata = lk(&self.metadata);
        serde_json::json!({
            "generation": self.generation.load(Ordering::Acquire),
            "state": phase.as_str(),
            "alive": present.then_some(phase != SitePlaybackPhase::Dead),
            "open_reason": metadata.open_reason.as_deref(),
            "pending_open_reason": metadata.pending_open_reason.as_deref(),
            "open_config": metadata.open_config.as_ref(),
            "callback_count": present.then(|| self.callback_count.load(Ordering::Relaxed)),
            "last_callback_age_ms": (present && callback_age_us != UNKNOWN_CALLBACK_AGE_US)
                .then_some(callback_age_us / 1_000),
            "ring": present.then(|| serde_json::json!({
                "queued": self.ring_queued.load(Ordering::Relaxed),
                "capacity": self.ring_capacity.load(Ordering::Relaxed),
                "dropped": self.ring_dropped.load(Ordering::Relaxed),
            })),
            "last_stream_error": metadata.last_stream_error.as_ref(),
        })
    }
}

pub(crate) fn site_playback_status(probe: &SitePlaybackProbe) -> serde_json::Value {
    probe.status()
}

struct SitePlaybackTransitions {
    generation: u64,
    phase: SitePlaybackPhase,
    opened_at: Option<Instant>,
}

impl SitePlaybackTransitions {
    fn new() -> Self {
        Self {
            generation: 0,
            phase: SitePlaybackPhase::Closed,
            opened_at: None,
        }
    }

    fn opened(
        &mut self,
        probe: &SitePlaybackProbe,
        playback: &PlaybackMonitor,
        reason: &str,
        now: Instant,
    ) {
        let config = playback.config_snapshot();
        self.generation = probe.reopened(config, reason);
        self.phase = SitePlaybackPhase::Starting;
        self.opened_at = Some(now);
        dlog!(
            "[audiohubd] site playback opened generation={} reason={} source={}Hz/{}ch \
             device={}Hz/{}ch/{} buffer={}",
            self.generation,
            reason,
            config.source_rate_hz,
            config.source_channels,
            config.device_rate_hz,
            config.device_channels,
            config.sample_format,
            config.buffer_size,
        );
    }

    fn closed(&mut self) {
        self.phase = SitePlaybackPhase::Closed;
        self.opened_at = None;
    }

    fn observe(
        &mut self,
        probe: &SitePlaybackProbe,
        playback: &PlaybackMonitor,
        tx: &AudioTx,
        now: Instant,
    ) -> SitePlaybackPhase {
        let callback_count = tx.output_callback_count();
        let callback_age = tx.last_output_callback_age();
        let no_callback_age = self
            .opened_at
            .map(|opened| now.saturating_duration_since(opened));
        let phase = classify_site_playback_phase(
            playback.is_alive(),
            callback_count,
            callback_age,
            no_callback_age,
        );

        // StreamHealth publishes its error text before its fatal flag. Mirror
        // that ordering here: a status reader that acquires `Dead` must already
        // be able to read the matching generation and error message.
        let stream_error = if phase == SitePlaybackPhase::Dead {
            playback.error_snapshot()
        } else {
            None
        };
        if let Some(message) = stream_error.clone() {
            probe.record_stream_error(self.generation, message);
        }
        probe.publish(phase, callback_count, callback_age, tx);

        if phase == self.phase {
            return phase;
        }
        match phase {
            SitePlaybackPhase::Stalled => {
                let observed_age = callback_age.or(no_callback_age).unwrap_or_default();
                dlog!(
                    "[audiohubd] site playback stalled generation={} callback_count={} \
                     callback_age_ms={} ring={}/{} dropped={}",
                    self.generation,
                    callback_count,
                    observed_age.as_millis(),
                    tx.queued(),
                    tx.capacity(),
                    tx.dropped(),
                );
            }
            SitePlaybackPhase::Dead => {
                dlog!(
                    "[audiohubd] site playback dead generation={} callback_count={} \
                     callback_age_ms={} ring={}/{} dropped={} error={}",
                    self.generation,
                    callback_count,
                    callback_age.map_or(0, |age| age.as_millis()),
                    tx.queued(),
                    tx.capacity(),
                    tx.dropped(),
                    stream_error.as_deref().unwrap_or("unavailable"),
                );
            }
            _ => {}
        }
        self.phase = phase;
        phase
    }
}

/// Retires the site stream when the default-output epoch advances.
///
/// This also cancels a delayed retry for the previous device. Calling it once
/// near the start of a tick and once immediately before output protects the
/// open/push window: a stream opened while the epoch changes is discarded
/// rather than receiving a frame for the now-stale default device.
fn apply_site_default_output_epoch(
    probe: &SitePlaybackProbe,
    observed_epoch: u64,
    current_epoch: &mut u64,
    playback: &mut Option<InstalledSitePlayback>,
    build_state: &mut SitePlaybackBuildState,
    builder: &mpsc::Sender<SitePlaybackBuildReq>,
    transitions: &mut SitePlaybackTransitions,
    retry: &mut SitePlaybackRetry,
) -> bool {
    if observed_epoch == *current_epoch {
        return false;
    }

    *current_epoch = observed_epoch;
    let generation = transitions.generation;
    let retired = playback.take();
    let had_stream = retired.is_some();
    if let Some(playback) = retired.as_ref() {
        let _ = builder.send(SitePlaybackBuildReq::Retire {
            request_id: playback.request_id,
        });
    }
    let invalidated = build_state.invalidate();
    let had_open_request = invalidated.is_some();
    if let Some(ticket) = invalidated {
        // This command follows its Open on the same FIFO. If the native open is
        // already blocking, the builder drops the result before servicing the
        // replacement request; the mixer also rejects its stale completion.
        let _ = builder.send(SitePlaybackBuildReq::Retire {
            request_id: ticket.request_id,
        });
    }
    retry.default_output_changed();
    let reason = retry.open_reason();
    probe.reopen_requested(reason);
    transitions.closed();
    drop(retired);
    dlog!(
        "[audiohubd] site playback reopen requested generation={} reason={} had_stream={} \
         had_open_request={}",
        generation,
        reason,
        had_stream,
        had_open_request,
    );
    true
}

/// Publishes stream health and retires a playback after a fatal callback error
/// or a callback stall. A stalled producer must not keep filling its one-second
/// ring: if callbacks resume at the same rate, that backlog never drains and
/// becomes permanent added latency.
/// Returning `true` means the paired `AudioTx` was removed and must not receive
/// this tick's output frame.
fn observe_and_retire_unhealthy_site_playback(
    probe: &SitePlaybackProbe,
    playback: &mut Option<InstalledSitePlayback>,
    builder: &mpsc::Sender<SitePlaybackBuildReq>,
    transitions: &mut SitePlaybackTransitions,
    retry: &mut SitePlaybackRetry,
    now: Instant,
) -> bool {
    let phase = match playback.as_ref() {
        Some(playback) => transitions.observe(probe, &playback.monitor, &playback.tx, now),
        None => return false,
    };
    if retry.observe_phase(phase, now) {
        dlog!(
            "[audiohubd] site playback stable generation={} running_ms={} retry_ms_reset={}",
            transitions.generation,
            SITE_PLAYBACK_STABLE_RUNNING.as_millis(),
            SITE_PLAYBACK_RETRY_MIN.as_millis(),
        );
    }
    let Some(cause) = site_playback_retire_cause(phase) else {
        return false;
    };

    let generation = transitions.generation;
    let error = playback
        .as_ref()
        .and_then(|playback| playback.monitor.error_snapshot());
    let delay = match cause {
        SitePlaybackRetireCause::StreamError => retry.after_stream_error(now),
        SitePlaybackRetireCause::CallbackStall => retry.after_callback_stall(now),
    };
    let next_reason = retry.open_reason();
    let retired = playback.take();
    if let Some(playback) = retired.as_ref() {
        let _ = builder.send(SitePlaybackBuildReq::Retire {
            request_id: playback.request_id,
        });
    }
    probe.reopen_requested(next_reason);
    transitions.closed();
    drop(retired);
    dlog!(
        "[audiohubd] site playback retired generation={} reason={} \
         next_open_reason={} retry_ms={} error={}",
        generation,
        match cause {
            SitePlaybackRetireCause::StreamError => "stream_error",
            SitePlaybackRetireCause::CallbackStall => "callback_stall",
        },
        next_reason,
        delay.as_millis(),
        error.as_deref().unwrap_or("unavailable"),
    );
    true
}

fn request_site_playback_open(
    probe: &SitePlaybackProbe,
    build_state: &mut SitePlaybackBuildState,
    builder: &mpsc::Sender<SitePlaybackBuildReq>,
    retry: &mut SitePlaybackRetry,
    device_epoch: u64,
    now: Instant,
) -> bool {
    if build_state.is_pending() || !retry.ready(now) {
        return false;
    }
    let reason = retry.open_reason();
    let Some(ticket) = build_state.begin(device_epoch, reason, now) else {
        return false;
    };
    probe.reopen_requested(reason);
    dlog!(
        "[audiohubd] site playback open requested request={} epoch={} reason={}",
        ticket.request_id,
        ticket.device_epoch,
        ticket.reason,
    );
    if builder
        .send(SitePlaybackBuildReq::Open {
            request_id: ticket.request_id,
            device_epoch: ticket.device_epoch,
            reason: ticket.reason,
        })
        .is_ok()
    {
        return true;
    }

    let _ = build_state.complete_if_current(
        ticket.request_id,
        ticket.device_epoch,
        ticket.device_epoch,
    );
    let delay = retry.after_open_failure(now);
    let retry_reason = retry.open_reason();
    probe.open_failed(retry_reason);
    dlog!(
        "[audiohubd] site playback open finished request={} epoch={} reason={} result=error \
         open_elapsed_ms=0 total_elapsed_ms=0 next_open_reason={} retry_ms={} \
         error=playback builder unavailable",
        ticket.request_id,
        ticket.device_epoch,
        ticket.reason,
        retry_reason,
        delay.as_millis(),
    );
    false
}

#[allow(clippy::too_many_arguments)]
fn apply_site_playback_build_results(
    probe: &SitePlaybackProbe,
    playback: &mut Option<InstalledSitePlayback>,
    build_state: &mut SitePlaybackBuildState,
    builder: &mpsc::Sender<SitePlaybackBuildReq>,
    built: &mpsc::Receiver<SitePlaybackBuildDone>,
    transitions: &mut SitePlaybackTransitions,
    retry: &mut SitePlaybackRetry,
    current_epoch: u64,
    now: Instant,
) {
    while let Ok(done) = built.try_recv() {
        let ticket =
            build_state.complete_if_current(done.request_id, done.device_epoch, current_epoch);
        let total_elapsed = ticket
            .map(|ticket| now.saturating_duration_since(ticket.requested_at))
            .unwrap_or_default();
        let Some(ticket) = ticket else {
            let result = if done.result.is_ok() {
                let _ = builder.send(SitePlaybackBuildReq::Retire {
                    request_id: done.request_id,
                });
                "stale_success"
            } else {
                "stale_error"
            };
            dlog!(
                "[audiohubd] site playback open finished request={} epoch={} reason={} \
                 result={} open_elapsed_ms={} current_epoch={}",
                done.request_id,
                done.device_epoch,
                done.reason,
                result,
                done.open_elapsed.as_millis(),
                current_epoch,
            );
            continue;
        };

        match done.result {
            Ok((monitor, tx)) => {
                dlog!(
                    "[audiohubd] site playback open finished request={} epoch={} reason={} \
                     result=ok open_elapsed_ms={} total_elapsed_ms={}",
                    ticket.request_id,
                    ticket.device_epoch,
                    ticket.reason,
                    done.open_elapsed.as_millis(),
                    total_elapsed.as_millis(),
                );
                transitions.opened(probe, &monitor, ticket.reason, now);
                retry.open_started();
                let replaced = playback.replace(InstalledSitePlayback {
                    request_id: ticket.request_id,
                    monitor,
                    tx,
                });
                if let Some(replaced) = replaced {
                    let _ = builder.send(SitePlaybackBuildReq::Retire {
                        request_id: replaced.request_id,
                    });
                }
            }
            Err(error) => {
                let delay = retry.after_open_failure(now);
                let retry_reason = retry.open_reason();
                probe.open_failed(retry_reason);
                dlog!(
                    "[audiohubd] site playback open finished request={} epoch={} reason={} \
                     result=error open_elapsed_ms={} total_elapsed_ms={} next_open_reason={} \
                     retry_ms={} error={}",
                    ticket.request_id,
                    ticket.device_epoch,
                    ticket.reason,
                    done.open_elapsed.as_millis(),
                    total_elapsed.as_millis(),
                    retry_reason,
                    delay.as_millis(),
                    error,
                );
            }
        }
    }
}

/// Current depth of an `AudioTx` playback ring (stage 8 `play_ring` or
/// stage 8-prime `bridge_ring`).
///
/// ## Sampling phase: call before `push()`
///
/// The measurement is how long a frame entering this stage must wait. Before
/// `push`, `queued()` is exactly the number of samples ahead of that frame.
/// Reading after `push` would add the frame's own 480 samples and overstate
/// residence by a constant 10 ms.
///
/// This matches the source-side phase: its three stages are read immediately
/// after `next_frame()`, so both sides report samples ahead of the incoming
/// frame instead of mixing a trough on one side with a peak on the other.
///
/// Rate and capacity come from `AudioTx`'s device rate, not a hard-coded
/// 48 kHz. The ring holds exactly one second at `dev_rate`; dividing a 44.1 kHz
/// device count by 48 kHz would silently under-report it by 8.8 percent.
///
/// The drop mode is `Newest`: a full `push_slice` short-writes and the incoming
/// samples never enter the ring. Depth alone cannot distinguish that from the
/// source FIFOs dropping their oldest samples, so the explicit label matters.
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

/// Publish stage 8 `play_ring` depth.
///
/// Taking `&StageSlot` instead of `&DaemonInner` keeps every wiring decision in
/// this small function and lets tests exercise it without constructing sockets,
/// thread channels, or a real device. Production passes `&inner.play_ring`.
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
    buf: [f32; F48_STEREO],
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
                LivePlayback::start_on_channels(&device, 48000, 2)
                    .map(|(pb, tx)| {
                        Some(BridgeOut {
                            _pb: pb,
                            tx,
                            refs: 0,
                            buf: [0.0; F48_STEREO],
                            depth: None,
                        })
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

pub(crate) fn mixer_loop(
    inner: Arc<DaemonInner>,
    cmds: mpsc::Receiver<MixCmd>,
    site_builder: mpsc::Sender<SitePlaybackBuildReq>,
    site_built: mpsc::Receiver<SitePlaybackBuildDone>,
) {
    let _qos_guard = raise_audio_thread_qos("mixer_loop");
    // 与 `tx_loop` 同一条理由：这也是一条 10 ms 截止期线程，`play_ring` 的
    // 5 ms `margin` 买的正是它的唤醒过冲，而一次阻塞 `write` 就能吃光它。
    rtlog::arm("mixer_loop");
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut playback: Option<InstalledSitePlayback> = None;
    let mut playback_build = SitePlaybackBuildState::default();
    let mut playback_transitions = SitePlaybackTransitions::new();
    let mut playback_retry = SitePlaybackRetry::new();
    let mut bridges: HashMap<String, BridgeOut> = HashMap::new();
    let mut dev_epoch = inner.dev_out_epoch.load(Ordering::Relaxed);
    let mut mix = [0.0f32; F48_STEREO];
    let mut mon = [0.0f32; F48_STEREO];
    let mut frame = [0.0f32; F48_STEREO];
    let mut stereo = [0.0f32; F48_STEREO];
    let mut airplay_frame = [[0.0f32; 2]; F48];
    let mut airplay = inner.airplay.local_reader();
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
    // `hal_mic` 水位闸门，每槽一份（见 `micgate` 的模块文档）。
    //
    // 状态放在**循环局部**而不是 `Shared` 里：它是「这条 mixer 循环对这个槽的
    // 处置」，只有这一个线程读写，和 `SpkPhaseWindow` 同一条理由——把它放进
    // 共享状态会让测试和 probe 与音频线程争同一个迟滞位。
    let mut mic_gates = [crate::micgate::MicGate::new(); crate::haldev::HAL_MAX_SLOTS];
    // 「这个槽真的有音频在送进虚拟麦克风」的粘滞位，见下面 `mic_live |= hal_dirty`
    // 处的长注释：单靠 `hal_mic_io` 判不出来，它的初值是 true。
    let mut mic_live: u16 = 0;
    // 重复流判据（规格 §4.6）：把**第一个**送进本机输出的 frame 拷进暂存，
    // 与**第二个**做零延迟归一化互相关。零延迟即可——重复流是同一份解码结果
    // 分两条会话进来，样本级已经对齐。480 点点积 ≈ 1.4k flops / 10 ms。
    let mut corr_a = [0.0f32; F48_STEREO];
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_mixcmd(cmd, &mut bridges);
        }
        // The default output moved, so this stream now targets the old device.
        // Named bridge outputs are independent and remain open.
        apply_site_default_output_epoch(
            &inner.site_playback,
            inner.dev_out_epoch.load(Ordering::Relaxed),
            &mut dev_epoch,
            &mut playback,
            &mut playback_build,
            &site_builder,
            &mut playback_transitions,
            &mut playback_retry,
        );
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
        // 唤醒过冲直方图（见 [`LateCell`] 与 [`sleep_until`]）。**测点必须在等待
        // 之后**：`margin` 买的是「醒来晚了多少」，不是「整 tick 丢没丢」。
        MIX_LATE.record(sleep_until(deadline));
        let streams: Vec<Arc<RxStream>> = rd(&inner.rx_table).values().cloned().collect();
        let airplay_read = airplay.read_into(&mut airplay_frame);
        let airplay_local = airplay_read.session_id.is_some();
        apply_site_playback_build_results(
            &inner.site_playback,
            &mut playback,
            &mut playback_build,
            &site_builder,
            &site_built,
            &mut playback_transitions,
            &mut playback_retry,
            dev_epoch,
            Instant::now(),
        );
        observe_and_retire_unhealthy_site_playback(
            &inner.site_playback,
            &mut playback,
            &site_builder,
            &mut playback_transitions,
            &mut playback_retry,
            Instant::now(),
        );
        let site_output_requested =
            airplay_local || streams.iter().any(|stream| stream.is_spk || stream.monitor);
        if site_output_requested && playback.is_none() {
            request_site_playback_open(
                &inner.site_playback,
                &mut playback_build,
                &site_builder,
                &mut playback_retry,
                dev_epoch,
                Instant::now(),
            );
        }
        if streams.is_empty() && !airplay_local {
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
            if s.spatial.is_some() {
                crate::spatial::render(&inner, s, now_ms);
                continue;
            }
            let popped = lk(&s.jbs).jb.pop();
            let frame_len = F48 * s.channels as usize;
            lk(&s.post).advance(popped, &mut frame[..frame_len]);
            to_stereo(&frame[..frame_len], s.channels, &mut stereo);
            // Q2 的可归属那一半（规格 §4.6）：测点在 advance 之后、加进任何
            // 目的地之前。这回答的是「我这一路送进来多响」，是**求和前**的量，
            // 与站点级的求和后削顶是两个不同的问题。
            s.clip.feed(now_ms, &stereo);
            if let Some(ring) = s.ring.as_ref() {
                let mut r = lk(ring);
                r.extend(stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5));
                if r.len() > RING_CAP {
                    let d = r.len() - RING_CAP;
                    r.drain(..d);
                }
            }
            // the bridge is a third destination, not an alternative to monitor:
            // one decoded frame may feed the virtual card AND the local output
            if let Some(name) = s.bridge.as_ref() {
                if let Some(b) = bridges.get_mut(name) {
                    for i in 0..F48_STEREO {
                        b.buf[i] += stereo[i];
                    }
                }
            }
            // ...and the virtual microphone is a fourth one: monitor, bridge
            // and hal are independent destinations for the SAME decode
            // (spec-m5b §5.4). The bucket is chosen by the PEER's slot, so two
            // peers' audio can never meet.
            add_to_hal_bucket(s.hal_slot, &stereo, &mut hal_bufs, &mut hal_dirty);
            if s.is_spk || s.monitor {
                // 送本机真实输出的那一集合：`out = soft_clip(mix + mon)`。
                // 站点级削顶正是在这里发生的，所以重复流判据也只看这一集合。
                contrib += 1;
                if contrib == 1 {
                    corr_a.copy_from_slice(&stereo);
                } else if contrib == 2 {
                    corr = crate::quality::correlation(&corr_a, &stereo);
                }
            }
            if s.is_spk {
                any_spk = true;
                for i in 0..F48_STEREO {
                    mix[i] += stereo[i];
                }
            } else if s.monitor {
                any_mon = true;
                for i in 0..F48_STEREO {
                    mon[i] += stereo[i];
                }
            }
        }
        if airplay_local {
            // AirPlay samples stay full-scale. The sender's dB event is
            // applied to the system output control in `airplay.rs`, never
            // multiplied into this frame.
            add_airplay_to_local_mix(
                &airplay_frame,
                &mut mix,
                &mut stereo,
                &mut corr_a,
                &mut contrib,
                &mut corr,
            );
            any_spk = true;
        }
        inner.mix_meter.feed(now_ms, contrib, corr);
        let sole_airplay = airplay_local && contrib == 1;
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
        // 本 tick 之后仍然「有人在用」的槽。
        //
        // ⚠ 判据**不能**只用 `hal_mic_io`：它的初值是 `true`（`lib.rs` 的
        // `from_fn(|_| AtomicBool::new(true))`，注释写明理由——「还没被告知」
        // 时按乐观处理，否则新对端的虚拟麦克风在第一条 IoState 到达前是静音的）。
        // 于是 16 个槽**默认全是 true**，只按它判会让闸门每 tick 观测 16 条
        // 根本不存在的通路。第一版就是这么写的，实测 60 s 里把
        // `starved_ticks` 刷到 95 730（= 6000 拍 × 15.95 个空槽），
        // 而真正在用的那一条完全健康。
        //
        // 正确判据是「这个槽**真的有音频要送**」：`hal_dirty` 置位过，且
        // `hal_mic_io` 还没被驱动告知已停。`hal_dirty` 是逐 tick 的，所以用
        // 一个粘滞位记住——排空段里我们故意不写，那些 tick 上 `hal_dirty`
        // 仍然是 1（音频照常到达），但即使不是，也必须继续观测水位。
        mic_live |= hal_dirty;
        if let Some(h) = hal.as_ref() {
            let mut out = [0.0f32; F48];
            for slot in 0..crate::haldev::HAL_MAX_SLOTS {
                // 没有应用在读这只虚拟麦克风、或这个槽压根没有音频要送 ⇒ 不写，
                // 并把闸门复位：上一条会话若结束在排空段中间，保留那个位会让
                // 下一条会话的第一拍无条件少写（= 开头静音）。
                if !inner.hal_mic_io[slot].load(Ordering::Relaxed) {
                    mic_gates[slot].reset();
                    mic_live &= !(1u16 << slot);
                    continue;
                }
                if mic_live & (1 << slot) == 0 {
                    continue;
                }
                // 级 8″：模式 B 虚拟麦克风环（500 ms）。**写之前**读——
                // 读到的是「驱动还没取走的积压」，正是这一帧要等的排队量。
                //
                // ⚠ 这一读**移出了 `hal_dirty` 分支**。此前只有「本 tick 真有
                // 音频要写」时才读，于是排空段（我们故意不写的那些 tick）里
                // 这一级在遥测上**整个消失**——正好是最需要看见它的时候。
                let Some(depth) = h.mic_depth(slot as u8) else {
                    continue;
                };
                hal_mic_depth[slot] = Some(depth);
                let plan = mic_gates[slot].decide(depth.samples, F48 as u32);
                h.record_mic_gate(slot as u8, &plan, depth.samples);
                if plan.drain_started {
                    dlog!(
                        "[audiohubd] hal_mic slot {slot}: 水位 {:.0} ms 越过天花板 {:.0} ms，\
                         开始排空到 {:.0} ms（一次连续空洞代替永久延迟）",
                        crate::micgate::frames_to_ms(depth.samples),
                        crate::micgate::frames_to_ms(crate::micgate::D_CEIL),
                        crate::micgate::frames_to_ms(crate::micgate::D_FLOOR),
                    );
                }
                if hal_dirty & (1 << slot) == 0 || plan.allow == 0 {
                    continue;
                }
                // 站点级削顶计入点 2/3：写进某个对端的虚拟麦克风。
                inner.mix_clip.feed(now_ms, &hal_bufs[slot]);
                for i in 0..F48 {
                    out[i] = soft_clip(hal_bufs[slot][i]);
                }
                h.write_mic_mono(slot as u8, &out[..plan.allow as usize]);
            }
        }
        if any_spk {
            // ⚠ 这个 soft_clip **不计入**站点级削顶统计（规格 §0.6）：
            // `mix_ring` 是 probe 的旁路 tap，不在送扬声器的路径上。把它算进去
            // 会让每一路 spk 流的削顶被重复计数一次，凭空虚增一倍。
            let clipped: Vec<f32> = mix
                .iter()
                .map(|&sample| protect_output_sample(sample, sole_airplay))
                .collect();
            let probe: Vec<f32> = clipped
                .chunks_exact(2)
                .map(|f| (f[0] + f[1]) * 0.5)
                .collect();
            push_mix(inner.as_ref(), &probe);
        } else {
            clear_mix(inner.as_ref());
        }
        // 本 tick 到底有没有一个活的播放环。没有就得清槽（设备打不开、或压根
        // 没有流送本机输出），不能留着上一次的读数。
        let mut have_play_ring = false;
        if any_spk || any_mon {
            // Recheck immediately before accepting an asynchronous open or
            // writing. The device epoch can advance while this tick is mixed.
            apply_site_default_output_epoch(
                &inner.site_playback,
                inner.dev_out_epoch.load(Ordering::Relaxed),
                &mut dev_epoch,
                &mut playback,
                &mut playback_build,
                &site_builder,
                &mut playback_transitions,
                &mut playback_retry,
            );
            apply_site_playback_build_results(
                &inner.site_playback,
                &mut playback,
                &mut playback_build,
                &site_builder,
                &site_built,
                &mut playback_transitions,
                &mut playback_retry,
                dev_epoch,
                Instant::now(),
            );
            // A completion can be current when dequeued and stale one instant
            // later. Recheck the epoch after committing it and before health or
            // ring access; a stale native stream is retired on its owner thread.
            apply_site_default_output_epoch(
                &inner.site_playback,
                inner.dev_out_epoch.load(Ordering::Relaxed),
                &mut dev_epoch,
                &mut playback,
                &mut playback_build,
                &site_builder,
                &mut playback_transitions,
                &mut playback_retry,
            );
            observe_and_retire_unhealthy_site_playback(
                &inner.site_playback,
                &mut playback,
                &site_builder,
                &mut playback_transitions,
                &mut playback_retry,
                Instant::now(),
            );
            if playback.is_none() {
                request_site_playback_open(
                    &inner.site_playback,
                    &mut playback_build,
                    &site_builder,
                    &mut playback_retry,
                    dev_epoch,
                    Instant::now(),
                );
            }
            if let Some(playback) = playback.as_mut() {
                let tx = &mut playback.tx;
                let mut out = [0.0f32; F48_STEREO];
                for i in 0..F48_STEREO {
                    out[i] = mix[i] + mon[i];
                }
                // 站点级削顶计入点 3/3：真实默认输出。这是最重要的一个——
                // 「两路重复流相加」的破音就出现在这里。同样喂削顶**之前**的和。
                inner.mix_clip.feed(now_ms, &out);
                for sample in &mut out {
                    *sample = protect_output_sample(*sample, sole_airplay);
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
            s.hal_mic.store(
                s.hal_slot
                    .and_then(|slot| hal_mic_depth.get(slot as usize).copied().flatten()),
            );
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
fn to_stereo(input: &[f32], channels: u8, out: &mut [f32; F48_STEREO]) {
    match channels.clamp(1, 2) {
        1 => {
            for (i, &sample) in input.iter().take(F48).enumerate() {
                out[i * 2] = sample;
                out[i * 2 + 1] = sample;
            }
        }
        2 => out.copy_from_slice(&input[..F48_STEREO]),
        _ => unreachable!(),
    }
}

fn add_to_hal_bucket(
    hal_slot: Option<u8>,
    frame: &[f32; F48_STEREO],
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
        bufs[slot][i] += (frame[i * 2] + frame[i * 2 + 1]) * 0.5;
    }
}

/// Presence verdict for one frequency on the summed mixer output. Plain
/// verify_tone can't apply here: concurrent probe tones are signal, not
/// noise, so detection keys on absolute Goertzel power (median of 100ms
/// windows); snr_db is still reported for diagnostics.
/// `mix_tone_verdict` 的存在判据：中位单格功率的下限。
///
/// amp-0.5 的音调落在 ~0.0625，PLC 衰减与削顶都不会把它拉到这个量级；
/// 静音与噪声则远在其下。
///
/// **两decade 的余量是承重的，不是宽松。** 播放伺服的合法弯速率按 `sinc²(δ)`
/// 削这一格的功率（终审 §二.14 的根因），2 kHz 上顶到 500 ppm 钳位削掉 3.2 %。
/// 正是这两个数量级的差距，让本判据**没有**掉进 `verify_tone` 当年那条抛硬币的
/// 噪声带里。谁想动这个数，先看
/// `tests::the_mix_verdict_is_not_in_the_same_noise_band_as_the_old_verify_tone`。
const MIX_TONE_POWER_FLOOR: f32 = 1e-4;

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
        let total: f64 =
            chunk.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / chunk.len() as f64 / 2.0;
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
        detected: p_med > MIX_TONE_POWER_FLOOR,
        samples_analyzed: analyzed,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn stereo_is_only_downmixed_at_the_legacy_peer_boundary() {
        let stereo = [1.0, 0.0, -0.25, 0.75, 0.4, -0.4];
        let mut out = Vec::new();
        convert_channels(&stereo, 2, 2, &mut out);
        assert_eq!(out, stereo, "new-to-new transport changed L/R");

        convert_channels(&stereo, 2, 1, &mut out);
        assert_eq!(out, [0.5, 0.25, 0.0], "legacy mono compatibility changed");

        let mut restored = [0.0; F48_STEREO];
        let mono = vec![0.5; F48];
        to_stereo(&mono, 1, &mut restored);
        assert!(restored.chunks_exact(2).all(|f| f == [0.5, 0.5]));
    }

    #[test]
    fn rx_48k_bypass_invalidates_a_parked_resampler_before_returning_to_the_same_rate() {
        const LOW_RATE: u32 = 32_000;
        const CHANNELS: u8 = 2;

        let mut rs = None;
        let mut rs_rate = MicSource::OUT_RATE;
        let mut first_low = Vec::with_capacity(320 * CHANNELS as usize);
        for _ in 0..320 {
            first_low.extend_from_slice(&[0.9, -0.9]);
        }
        let _ = resample_received_frame(
            &mut rs,
            &mut rs_rate,
            [0.0; 2],
            LOW_RATE,
            CHANNELS,
            first_low,
        );
        assert!(
            rs.is_some(),
            "the first lower-rate frame did not build a converter"
        );
        assert_eq!(rs_rate, LOW_RATE);

        let bypass_last = [0.25, -0.75];
        let mut bypass = vec![0.0; F48_STEREO];
        bypass[F48_STEREO - 2..].copy_from_slice(&bypass_last);
        let bypass_expected = bypass.clone();
        let bypassed = resample_received_frame(
            &mut rs,
            &mut rs_rate,
            [0.9, -0.9],
            MicSource::OUT_RATE,
            CHANNELS,
            bypass,
        );
        assert_eq!(
            bypassed, bypass_expected,
            "48 kHz stopped being a bit-exact bypass"
        );
        assert!(
            rs.is_none(),
            "a converter which skipped the 48 kHz frame remained reusable"
        );
        assert_eq!(
            rs_rate,
            MicSource::OUT_RATE,
            "the converter's rate identity did not cross the bypass boundary"
        );

        let mut next_low = Vec::with_capacity(320 * CHANNELS as usize);
        for _ in 0..320 {
            next_low.extend_from_slice(&[0.1, 0.2]);
        }
        let got = resample_received_frame(
            &mut rs,
            &mut rs_rate,
            bypass_last,
            LOW_RATE,
            CHANNELS,
            next_low.clone(),
        );
        let mut fresh =
            seeded_interleaved_resampler(LOW_RATE, MicSource::OUT_RATE, CHANNELS, bypass_last);
        let mut expected = Vec::new();
        fresh.process(&next_low, &mut expected);
        assert_eq!(
            got, expected,
            "returning to the same lower rate reused phase/history from before the 48 kHz span"
        );
    }

    #[test]
    fn sole_airplay_is_amplitude_transparent_but_mixed_output_stays_protected() {
        for sample in [-1.0f32, -0.95, 0.0, 0.95, 1.0] {
            assert_eq!(protect_output_sample(sample, true), sample);
        }
        assert_eq!(protect_output_sample(1.2, true), 1.0);
        assert_eq!(protect_output_sample(-1.2, true), -1.0);

        let protected = protect_output_sample(1.0, false);
        assert_eq!(protected, soft_clip(1.0));
        assert!(protected < 1.0);
        assert_ne!(protect_output_sample(0.95, false), 0.95);
    }

    #[test]
    fn airplay_stereo_lanes_reach_local_mix_and_correlation() {
        let mut airplay = [[0.0; 2]; F48];
        for (index, frame) in airplay.iter_mut().enumerate() {
            let left = index as f32 / F48 as f32;
            *frame = [left, -left];
        }
        let expected: Vec<f32> = airplay.iter().flat_map(|frame| *frame).collect();

        let mut mix = [0.25; F48_STEREO];
        let mut stereo = [0.0; F48_STEREO];
        let mut corr_a = [0.0; F48_STEREO];
        let mut contrib = 0;
        let mut corr = None;
        add_airplay_to_local_mix(
            &airplay,
            &mut mix,
            &mut stereo,
            &mut corr_a,
            &mut contrib,
            &mut corr,
        );

        assert_eq!(contrib, 1);
        assert_eq!(corr, None);
        assert_eq!(stereo.as_slice(), expected.as_slice());
        assert_eq!(corr_a.as_slice(), expected.as_slice());
        for (mixed, source) in mix.iter().zip(expected.iter()) {
            assert_eq!(*mixed, 0.25 + source);
        }

        let mut second_mix = [0.0; F48_STEREO];
        let mut second_scratch = [0.0; F48_STEREO];
        let mut second_contrib = 1;
        let mut second_corr = None;
        add_airplay_to_local_mix(
            &airplay,
            &mut second_mix,
            &mut second_scratch,
            &mut corr_a,
            &mut second_contrib,
            &mut second_corr,
        );
        assert_eq!(second_contrib, 2);
        assert_eq!(second_corr, Some(1.0));
        assert_eq!(second_mix.as_slice(), expected.as_slice());
    }

    #[test]
    fn fatal_site_playback_retries_quickly_with_a_two_second_cap() {
        let start = Instant::now();
        let mut retry = SitePlaybackRetry::new();

        let first = retry.after_stream_error(start);
        assert_eq!(first, Duration::from_millis(250));
        assert_eq!(retry.open_reason(), "stream_error_recovery");
        assert!(!retry.ready(start + Duration::from_millis(249)));
        assert!(retry.ready(start + Duration::from_millis(250)));

        let mut now = start + first;
        for expected in [500, 1_000, 2_000, 2_000, 2_000] {
            let delay = retry.after_open_failure(now);
            assert_eq!(delay, Duration::from_millis(expected));
            assert_eq!(retry.open_reason(), "retry_after_stream_error");
            assert!(!retry.ready(now + delay - Duration::from_millis(1)));
            assert!(retry.ready(now + delay));
            now += delay;
        }
    }

    #[test]
    fn callback_stall_uses_bounded_recovery_instead_of_preserving_the_ring() {
        let start = Instant::now();
        let mut retry = SitePlaybackRetry::new();

        let first = retry.after_callback_stall(start);
        assert_eq!(first, SITE_PLAYBACK_RETRY_MIN);
        assert_eq!(retry.open_reason(), "callback_stall_recovery");
        assert!(!retry.ready(start + first - Duration::from_millis(1)));
        assert!(retry.ready(start + first));

        let second = retry.after_open_failure(start + first);
        assert_eq!(second, Duration::from_millis(500));
        assert_eq!(retry.open_reason(), "retry_after_callback_stall");
    }

    #[test]
    fn callback_age_classification_makes_stall_a_retirement_condition() {
        let threshold = SITE_CALLBACK_STALL_THRESHOLD;
        assert_eq!(
            classify_site_playback_phase(true, 1, Some(threshold), None),
            SitePlaybackPhase::Running,
            "the exact threshold must not flap a healthy callback"
        );
        assert_eq!(
            classify_site_playback_phase(true, 1, Some(threshold + Duration::from_millis(1)), None,),
            SitePlaybackPhase::Stalled
        );
        assert_eq!(
            classify_site_playback_phase(true, 0, None, Some(threshold + Duration::from_millis(1)),),
            SitePlaybackPhase::Stalled,
            "a stream which never produced its first callback must recover too"
        );
        assert_eq!(
            classify_site_playback_phase(true, 1, None, Some(threshold + Duration::from_secs(1)),),
            SitePlaybackPhase::Running,
            "a relaxed count/timestamp snapshot must not use total stream age as callback age"
        );
        assert_eq!(
            classify_site_playback_phase(false, 1, Some(Duration::ZERO), None),
            SitePlaybackPhase::Dead
        );

        assert_eq!(
            site_playback_retire_cause(SitePlaybackPhase::Stalled),
            Some(SitePlaybackRetireCause::CallbackStall)
        );
        assert_eq!(
            site_playback_retire_cause(SitePlaybackPhase::Dead),
            Some(SitePlaybackRetireCause::StreamError)
        );
        assert_eq!(site_playback_retire_cause(SitePlaybackPhase::Running), None);
    }

    #[test]
    fn playback_open_generation_rejects_stale_builder_results() {
        let now = Instant::now();
        let mut state = SitePlaybackBuildState::default();
        let first = state.begin(7, "initial", now).expect("first request");
        assert!(state.begin(7, "initial", now).is_none());
        assert_eq!(state.invalidate(), Some(first));

        let second = state
            .begin(8, "default_output_changed", now)
            .expect("replacement request");
        assert!(
            state
                .complete_if_current(first.request_id, first.device_epoch, 8)
                .is_none(),
            "an old result replaced the new in-flight request"
        );
        assert!(state.is_pending());
        assert!(
            state
                .complete_if_current(second.request_id, second.device_epoch, 9)
                .is_none(),
            "a result for an already superseded device epoch was accepted"
        );
        assert_eq!(
            state.complete_if_current(second.request_id, second.device_epoch, 8),
            Some(second)
        );
        assert!(!state.is_pending());
    }

    #[test]
    fn expired_playback_owner_cannot_publish_a_late_native_open() {
        let progress = SitePlaybackOwnerProgress::new();
        assert_eq!(progress.phase(), SitePlaybackOwnerPhase::Opening);
        assert!(progress.expire_open());
        assert_eq!(progress.phase(), SitePlaybackOwnerPhase::Expired);
        assert!(
            !progress.opened(),
            "a watchdog-expired native open became installable"
        );
        assert!(
            !progress.open_failed(),
            "an expired owner emitted a second completion"
        );
    }

    #[test]
    fn playback_owner_watchdog_expires_at_its_declared_boundary() {
        let requested_at = Instant::now();
        assert!(!site_playback_owner_watchdog_due(
            requested_at,
            requested_at + SITE_PLAYBACK_OPEN_WATCHDOG - Duration::from_millis(1),
        ));
        assert!(site_playback_owner_watchdog_due(
            requested_at,
            requested_at + SITE_PLAYBACK_OPEN_WATCHDOG,
        ));
    }

    #[test]
    fn completed_playback_open_wins_before_its_watchdog() {
        let progress = SitePlaybackOwnerProgress::new();
        assert!(progress.opened());
        assert_eq!(progress.phase(), SitePlaybackOwnerPhase::Ready);
        assert!(
            !progress.expire_open(),
            "the watchdog expired an owner which had already published Ready"
        );
    }

    #[test]
    fn one_abandoned_native_open_allows_rotation_but_the_leak_is_bounded() {
        assert!(site_playback_owner_budget_allows(0));
        assert!(
            site_playback_owner_budget_allows(1),
            "one blocked generation prevented its replacement from opening"
        );
        assert!(!site_playback_owner_budget_allows(
            SITE_PLAYBACK_MAX_ABANDONED_OWNERS
        ));
    }

    #[test]
    fn retired_owner_stays_counted_until_its_creation_thread_finishes() {
        let progress = SitePlaybackOwnerProgress::new();
        assert!(progress.opened());
        progress.cancel();
        assert_eq!(progress.phase(), SitePlaybackOwnerPhase::Expired);
        progress.finish();
        assert_eq!(progress.phase(), SitePlaybackOwnerPhase::Finished);
    }

    #[test]
    fn repeated_ok_then_early_fatal_streams_keep_escalating_backoff() {
        let start = Instant::now();
        let mut retry = SitePlaybackRetry::new();
        let mut now = start;

        for expected in [250, 500, 1_000, 2_000, 2_000] {
            retry.open_started();
            assert!(!retry.observe_phase(SitePlaybackPhase::Running, now));
            now += Duration::from_millis(10);
            let delay = retry.after_stream_error(now);
            assert_eq!(
                delay,
                Duration::from_millis(expected),
                "an Ok open followed by an early async fatal reset the backoff"
            );
            now += delay;
        }
    }

    #[test]
    fn a_stably_running_replacement_eventually_resets_the_backoff() {
        let start = Instant::now();
        let mut retry = SitePlaybackRetry::new();
        assert_eq!(retry.after_stream_error(start), Duration::from_millis(250));
        assert_eq!(
            retry.after_open_failure(start + Duration::from_millis(250)),
            Duration::from_millis(500)
        );

        let reopened = start + Duration::from_millis(750);
        retry.open_started();
        assert!(!retry.observe_phase(SitePlaybackPhase::Starting, reopened));
        assert!(!retry.observe_phase(SitePlaybackPhase::Running, reopened));
        assert!(!retry.observe_phase(
            SitePlaybackPhase::Running,
            reopened + Duration::from_millis(999)
        ));
        assert!(retry.observe_phase(
            SitePlaybackPhase::Running,
            reopened + SITE_PLAYBACK_STABLE_RUNNING
        ));

        let delay = retry.after_stream_error(reopened + Duration::from_secs(2));
        assert_eq!(delay, Duration::from_millis(250));
        assert_eq!(retry.open_reason(), "stream_error_recovery");
    }

    #[test]
    fn a_default_output_epoch_overrides_a_stale_stream_error_retry() {
        let start = Instant::now();
        let mut retry = SitePlaybackRetry::new();
        retry.after_stream_error(start);
        assert!(!retry.ready(start));

        retry.default_output_changed();
        assert!(retry.ready(start));
        assert_eq!(retry.open_reason(), "default_output_changed");

        let delay = retry.after_open_failure(start);
        assert_eq!(
            delay,
            Duration::from_millis(250),
            "the new default device inherited the previous device's backoff"
        );
        assert_eq!(retry.open_reason(), "retry_after_default_output_change");
    }

    #[test]
    fn site_playback_status_carries_recovery_generation_and_reason() {
        let probe = SitePlaybackProbe::new();
        let config = PlaybackConfigSnapshot {
            source_rate_hz: 48_000,
            source_channels: 2,
            device_rate_hz: 48_000,
            device_channels: 2,
            sample_format: "F32".to_owned(),
            buffer_size: "Default".to_owned(),
        };
        assert_eq!(probe.reopened(&config, "initial"), 1);
        probe.record_stream_error(1, "device unavailable".to_owned());
        probe.reopen_requested("stream_error_recovery");

        let pending = probe.status();
        assert_eq!(pending["generation"], 1);
        assert_eq!(pending["state"], "closed");
        assert_eq!(pending["pending_open_reason"], "stream_error_recovery");
        assert_eq!(pending["last_stream_error"]["generation"], 1);

        assert_eq!(probe.reopened(&config, "stream_error_recovery"), 2);
        let reopened = probe.status();
        assert_eq!(reopened["generation"], 2);
        assert_eq!(reopened["state"], "starting");
        assert_eq!(reopened["open_reason"], "stream_error_recovery");
        assert!(reopened["pending_open_reason"].is_null());
        assert_eq!(reopened["last_stream_error"]["generation"], 1);
    }

    #[test]
    fn asynchronous_site_result_is_epoch_checked_before_the_real_output_push() {
        let body = fn_body("pub(crate) fn mixer_loop(");
        let output = body
            .split("let mut have_play_ring = false;")
            .nth(1)
            .expect("real-output lifecycle branch is missing");
        let first_epoch = output
            .find("apply_site_default_output_epoch(")
            .expect("pre-completion epoch check is missing");
        let completion = output
            .find("apply_site_playback_build_results(")
            .expect("asynchronous open results are not consumed");
        let after_completion = &output[completion + 1..];
        let second_epoch = completion
            + 1
            + after_completion
                .find("apply_site_default_output_epoch(")
                .expect("post-completion epoch check is missing");
        let health = output
            .find("observe_and_retire_unhealthy_site_playback(")
            .expect("post-completion health check is missing");
        let installed_tx = output
            .find("playback.as_mut()")
            .expect("real-output playback access is missing");
        let push = output
            .find("tx.push(&out)")
            .expect("real-output push is missing");
        assert!(
            first_epoch < completion
                && completion < second_epoch
                && second_epoch < health
                && health < installed_tx
                && installed_tx < push,
            "real-output lifecycle order must be epoch < completion < epoch < health < \
             AudioTx access < push"
        );
    }

    #[test]
    fn idle_playback_health_is_checked_before_the_idle_short_circuit() {
        let mixer = fn_body("pub(crate) fn mixer_loop(");
        let idle = mixer
            .find("if streams.is_empty() && !airplay_local")
            .expect("idle short-circuit is missing");
        let health = mixer[..idle]
            .rfind("observe_and_retire_unhealthy_site_playback(")
            .expect("idle path skips site playback health");
        assert!(
            health < idle,
            "the idle short-circuit leaves a dead site playback installed"
        );
    }

    #[test]
    fn a_dead_status_never_precedes_its_error_metadata() {
        let observe = fn_body("fn observe(");
        let record = observe
            .find("probe.record_stream_error(")
            .expect("fatal error metadata is not recorded");
        let publish = observe
            .find("probe.publish(phase")
            .expect("playback phase is not published");
        assert!(
            record < publish,
            "Dead can become visible before the matching error metadata"
        );
    }

    #[test]
    fn native_site_playback_open_is_confined_to_its_detached_owner_thread() {
        let mixer = fn_body("pub(crate) fn mixer_loop(");
        assert!(
            !mixer.contains("LivePlayback::start_channels("),
            "the 10 ms mixer still performs a potentially blocking native open"
        );
        let coordinator = fn_body("pub(crate) fn site_playback_builder_loop(");
        assert!(
            !coordinator.contains("LivePlayback::start_channels("),
            "one blocked native open still blocks every later generation"
        );
        assert!(
            coordinator.contains("drop(owner_thread)")
                && coordinator.contains("recv_timeout(SITE_PLAYBACK_OWNER_POLL)")
                && !coordinator.contains(".join("),
            "the coordinator can still wait forever for an uninterruptible owner"
        );
        let owner = fn_body("fn site_playback_owner_loop(");
        assert!(
            owner.contains("LivePlayback::start_channels(48_000, 2)"),
            "the generation owner does not perform the native open"
        );
        assert!(
            owner.contains("drop(playback)") && owner.contains("control.recv()"),
            "the native stream is not retained and destroyed on its creation thread"
        );
        let completion = fn_body("fn apply_site_playback_build_results(");
        assert!(
            completion.contains("result=ok open_elapsed_ms={}")
                && completion.contains("result=error open_elapsed_ms={}"),
            "asynchronous open completion lost success/failure duration diagnostics"
        );
    }

    #[test]
    fn playback_coordinator_shutdown_never_joins_an_uninterruptible_owner() {
        let coordinator = fn_body("pub(crate) fn site_playback_builder_loop(");
        assert!(
            coordinator.contains("if inner.shutdown.load(Ordering::SeqCst)")
                && coordinator.contains("reqs.recv_timeout(SITE_PLAYBACK_OWNER_POLL)")
                && coordinator.contains("stop_site_playback_owners(&mut owners)")
                && coordinator.contains("drop(owner_thread)")
                && !coordinator.contains(".join("),
            "daemon shutdown can still block on a native playback owner"
        );
        let owner = fn_body("fn site_playback_owner_loop(");
        let source = code();
        let signature_at = source
            .find("fn site_playback_owner_loop(")
            .expect("playback owner signature is missing");
        let signature_end = signature_at
            + source[signature_at..]
                .find(") {")
                .expect("playback owner signature is incomplete")
            + 1;
        let signature = &source[signature_at..signature_end];
        assert!(
            !signature.contains("DaemonInner")
                && !signature.contains("Arc<DaemonInner>")
                && !owner.contains("inner")
                && owner.contains("control.recv()"),
            "a detached owner can retain daemon resources after bounded shutdown"
        );
    }

    /// 终审 §二.14 的**推广检查**：`mix_tone_verdict` 也判音，它是否也坐在
    /// 量具的噪声带里？
    ///
    /// 答案是**否**，而这条测试就是那个「否」的证据，不是一句转述。
    /// 理由与 `verify_tone` 不同：这里的 `detected` 用的是**绝对功率**
    /// （`p_med > 1e-4`），而不是单格相干比。播放伺服的弯速率按 `sinc²(δ)`
    /// 削功率，500 ppm 钳位在 2 kHz 上 δ = 0.1 格 ⇒ 只削掉 3.2 %，
    /// 而 amp-0.5 的音调落在 0.0625，是判据的 625 倍。
    ///
    /// ⚠ 所以这条测试**不是**在测「弯了还检得到」这件容易的事，它钉的是
    /// **余量本身**：谁把 `1e-4` 抬到 0.05、或者把 `detected` 改回相干比，
    /// 这条就变红。§二.7 缺的就是这一步（只改了一条断言，没有回头看兄弟量具）。
    #[test]
    fn the_mix_verdict_is_not_in_the_same_noise_band_as_the_old_verify_tone() {
        const SR: u32 = 48_000;
        // 伺服钳位的两端 + 中间，覆盖它能造成的全部弯量。
        for ppm in [0.0f32, 250.0, 500.0, -500.0] {
            for freq in [1000.0f32, 2000.0] {
                let bent = freq * (1.0 + ppm * 1e-6);
                let x = dsp::gen_sine(bent, SR, SR as usize * 3, 0.5);
                let v = mix_tone_verdict(&x, SR, freq);
                assert!(
                    v.detected,
                    "{freq} Hz bent {ppm} ppm went undetected in the mixer tap: {v:?}"
                );
            }
        }
        // 余量：弯到钳位时，中位功率必须仍然远高于 1e-4 的判据。
        let bent = 2000.0f32 * (1.0 - 500.0 * 1e-6);
        let x = dsp::gen_sine(bent, SR, SR as usize * 3, 0.5);
        let p = mix_median_power(&x, SR, 2000.0);
        let decades = (p / MIX_TONE_POWER_FLOOR as f64).log10();
        assert!(
            decades >= 2.0,
            "a clamp-bent 2 kHz tone leaves {p:.6} of median bin power, only \
             {decades:.2} decades above the {MIX_TONE_POWER_FLOOR:e} detection \
             floor. Below two decades this instrument is drifting toward the \
             §二.14 failure mode — a legal servo bend would start deciding the \
             verdict. Fix the instrument, not the floor."
        );
        // 反向：静音必须远在判据之下（否则上面的余量断言什么都没证明）。
        let quiet = vec![0.0f32; SR as usize * 3];
        assert!(
            !mix_tone_verdict(&quiet, SR, 2000.0).detected,
            "silence read as a tone"
        );
    }

    /// 上一条要用的中位单格功率。与 `mix_tone_verdict` 内部同一口径。
    fn mix_median_power(samples: &[f32], rate: u32, freq: f32) -> f64 {
        let win = (rate / 10) as usize;
        let skip = (rate / 5) as usize;
        let mut ps: Vec<f64> = samples[skip..]
            .chunks(win)
            .filter(|c| c.len() == win)
            .map(|c| dsp::goertzel_power(c, rate, freq) as f64)
            .collect();
        ps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ps[ps.len() / 2]
    }

    /// **调度迟到直方图的分桶必须与 `LATE_EDGES_MS` 声明的语义一致。**
    ///
    /// 这条守 [`LateCell::record`]。分桶写错的后果特别隐蔽：直方图照样有数、
    /// 照样单调、照样看起来合理，只是「P(迟到 > 40 ms)」答的是另一个问题——
    /// 而那个数是 `min_target` 该取 3 还是 4 的**唯一**判据。一个差一错位
    /// 就能让下一轮把 JB 削到听得见咔哒的深度，并且事后无从追查。
    ///
    /// 边界取**闭下开上**（`[lo, hi)`）：恰好 10.000 ms 的迟到算进 `10-15` 桶，
    /// 因为 JB 深度 1 帧（10 ms）扛得住的是**严格小于** 10 ms 的停顿。
    #[test]
    fn the_lateness_histogram_buckets_match_their_declared_edges() {
        let c = LateCell::new();
        // 每个边界上下各打一发，外加 0 和一个远超上界的值
        let probes_us: [u64; 16] = [
            0, 999, 1_000, 1_999, 2_000, 4_999, 5_000, 9_999, 10_000, 14_999, 20_000, 29_999,
            40_000, 50_000, 99_999, 250_000,
        ];
        for us in probes_us {
            c.record(Duration::from_micros(us));
        }
        let s = c.snapshot();
        assert_eq!(s.ticks, probes_us.len() as u64, "tick 总数（分母）不对");
        assert_eq!(s.max_us, 250_000, "最大值没记对");
        // 逐个探针独立复算它该落哪个桶，与实现的累计结果对账。
        let mut want = [0u64; LATE_BUCKETS];
        for us in probes_us {
            let ms = us / 1000;
            let mut i = 0;
            while i < LATE_EDGES_MS.len() && ms >= LATE_EDGES_MS[i] {
                i += 1;
            }
            want[i] += 1;
        }
        assert_eq!(s.buckets, want, "分桶与边界语义不一致");
        // 关键的三条读法：把桶从尾部累加得到 P(迟到 ≥ 边界)。
        let tail = |from: usize| -> u64 { s.buckets[from..].iter().sum() };
        assert_eq!(
            tail(LATE_BUCKETS - 1),
            1,
            ">100 ms 的桶应当只有 250 ms 那一发"
        );
        // `edges_ms[3] = 10`，所以第 4 个桶起就是「≥10 ms」。
        assert_eq!(s.edges_ms[3], 10);
        assert_eq!(tail(4), 8, "P(迟到 ≥ 10 ms) 的分子算错");
        // 0 的那一发既不进 max 也不进 sum，但必须进分母和 0 号桶。
        assert_eq!(s.buckets[0], 2, "0 和 999 µs 都该落在 0-1ms 桶");
        assert_eq!(
            s.late_us_sum,
            probes_us.iter().sum::<u64>(),
            "迟到总量漏掉了某些样本"
        );
    }

    /// **`sleep_until` 必须在唤醒之后量，不能在睡之前量。**
    ///
    /// 这条守的是 `docs/spec-playdev-measurement.md` §4.4 记下的那个缺陷：
    /// 测点落在 `sleep` 之前，量到的是 `max(0, 上一 tick 的活 + 过冲 − 一个 tick)`
    /// —— **一个带 10 ms 死区的超支指标**。而 `play_ring` 的 `margin` 关心的
    /// 唤醒过冲实测 0.02–1.67 ms，**整个落在死区里面**，原理上就量不到。
    /// 30-win 探针（tick 内无活）27000 次全记 0，正是这个死区的极端形态。
    ///
    /// 注入对照：把 `sleep_until` 改回「先量后睡」（即在 `now < deadline` 时
    /// 返回 `now.saturating_duration_since(deadline)` == 0），本条**必红**。
    #[test]
    fn sleeping_until_a_deadline_measures_the_overshoot_after_waking() {
        for i in 0..5 {
            let deadline = Instant::now() + Duration::from_millis(2);
            let late = sleep_until(deadline);
            let after = Instant::now();
            // 关键断言：睡前量的版本在这条路径上恒等于 0。
            // `std::thread::sleep` 的契约是「至少睡这么久」⇒ 醒来严格晚于
            // deadline ⇒ 过冲严格 > 0。实测量级 0.02–1.67 ms，不是 1 ns 级的擦边。
            assert!(
                late > Duration::ZERO,
                "第 {i} 次：唤醒过冲记成了 0 —— 测点回到了 sleep 之前"
            );
            // 返回值必须是「唤醒时刻 − deadline」，那一刻不晚于现在。
            assert!(
                late <= after.saturating_duration_since(deadline),
                "第 {i} 次：返回值 {late:?} 超过了到现在为止的全部经过时间"
            );
            // 而且真的等到了 deadline（没有提前返回）。
            assert!(after >= deadline, "第 {i} 次：还没到 deadline 就返回了");
        }
    }

    /// **已经错过的 deadline：如实报出全部迟到量，且不再睡。**
    ///
    /// 注入对照：
    /// - 无条件 `sleep(deadline − now)` 之类的写法在这里会下溢 / panic 或睡满，
    ///   `elapsed < 5 ms` 那一条会红；
    /// - 把返回值钳成 0（「迟到当没发生」）会让第一条红。
    ///
    /// 这一支才是 `LateCell` 尾部桶（≥10 ms）的来源；准时那一支只喂第 0 桶。
    #[test]
    fn a_deadline_already_missed_reports_the_whole_lateness_without_sleeping() {
        let deadline = Instant::now() - Duration::from_millis(30);
        let t0 = Instant::now();
        let late = sleep_until(deadline);
        let elapsed = t0.elapsed();
        assert!(
            late >= Duration::from_millis(29),
            "迟到 30 ms 却只报了 {late:?}"
        );
        assert!(
            late < Duration::from_millis(60),
            "迟到量 {late:?} 远超实际，基准点取错了"
        );
        assert!(
            elapsed < Duration::from_millis(5),
            "已经迟到还睡了 {elapsed:?} —— 迟到会被自己放大"
        );
    }

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

        add_to_hal_bucket(Some(0), &[0.25; F48_STEREO], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(3), &[0.75; F48_STEREO], &mut bufs, &mut dirty);

        assert_eq!(dirty, 0b1001, "exactly the two slots written are dirty");
        assert!(
            bufs[0].iter().all(|&v| v == 0.25),
            "slot 0 must carry only its own peer"
        );
        assert!(
            bufs[3].iter().all(|&v| v == 0.75),
            "slot 3 must carry only its own peer"
        );
        for (i, b) in bufs.iter().enumerate() {
            if i != 0 && i != 3 {
                assert!(
                    b.iter().all(|&v| v == 0.0),
                    "slot {i} was written to by nobody"
                );
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
        add_to_hal_bucket(Some(1), &[0.25; F48_STEREO], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(1), &[0.25; F48_STEREO], &mut bufs, &mut dirty);
        assert!(bufs[1].iter().all(|&v| v == 0.5));
        assert_eq!(dirty, 0b10);
    }

    #[test]
    fn a_stream_bound_to_no_device_touches_nothing() {
        let mut bufs = vec![[0.0f32; F48]; 4];
        let mut dirty = 0u16;
        add_to_hal_bucket(None, &[1.0; F48_STEREO], &mut bufs, &mut dirty);
        // ...and neither does one naming a slot this driver does not have.
        add_to_hal_bucket(Some(200), &[1.0; F48_STEREO], &mut bufs, &mut dirty);
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

    /// 过量生产由安全网排回目标水位，**不再顶到 1 秒**；丢弃方向仍是最旧。
    ///
    /// 原名 `..._saturates_at_one_second_...`，断言 47_520 样本（990 ms）——
    /// 那是缺陷成立时的读数。速率伺服上线后饱和本身成了 bug，断言随之反过来。
    #[test]
    fn an_overproducing_sysaudio_source_is_resynced_instead_of_saturating() {
        let mut src = sys_frames(48_000, 5_000); // 每 tick 收 5000、放 480
        let mut out = Vec::new();
        for _ in 0..20 {
            src.next_frame(&mut out);
        }
        let d = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(d.samples, 1_440, "安全网排回目标水位（3 帧 = 30 ms）");
        assert!(
            !d.saturated(),
            "过量生产必须被排掉而不是顶到容量上限：饱和就是那个 1 秒延迟缺陷"
        );
        assert_eq!(d.ms(), Some(30.0));
        assert_eq!(d.drop_mode, DropMode::Oldest);
        // 收支守恒，容差留给伺服正在弯的重采样比。
        let accounted = 20 * 480 + 1_440 + d.dropped.unwrap() as i64;
        assert!(
            (accounted - 20 * 5_000).abs() < 200,
            "收支对不上：吐出 {accounted} vs 吃进 100000"
        );
        // 丢的确实是最旧的：源交的是 1,2,3,…，留在 FIFO 里的必须是尾部。
        src.next_frame(&mut out);
        assert!(
            out[0] > 95_000.0,
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
    fn injection_b_a_rate_mismatch_beyond_the_servo_is_shed_at_a_bounded_depth() {
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
        assert!(!mid.saturated(), "永远不许贴顶, got {} 样本", mid.samples);
        // 与旧断言相反：安全网从第一秒就在排，所以丢弃**立刻**开始涨。这不是
        // 退步——排掉的是伺服权限之外的那 9 500 ppm，代价明码记在 `dropped` 上，
        // 而深度被钉在有界水位而不是一路爬到用户听得见的一秒。
        assert!(
            mid.dropped.unwrap() > 0,
            "超出伺服权限的失配必须由安全网排掉并计数"
        );
        assert!(
            mid.ms().unwrap() <= 130.0,
            "深度必须被钉在安全网门限附近，而不是继续爬, got {:?}",
            mid.ms()
        );

        // ---- 阶段二：长期稳态——深度有界、丢弃匀速 ----
        //
        // 旧版这里等的是「跑到饱和」。有了安全网就永远等不到：深度被钉住，
        // 而失配的代价改为以恒定速率记在 `dropped` 上。
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
        assert!(
            !d.saturated(),
            "跑再久也不许贴顶——那正是被治好的缺陷, got {} 样本",
            d.samples
        );
        assert!(
            d.ms().unwrap() <= 130.0,
            "150 秒之后深度仍须有界, got {:?}（旧代码此刻是 990 ms）",
            d.ms()
        );
        assert_eq!(
            d.drop_mode,
            DropMode::Oldest,
            "丢最旧 ⇒ 恒定迟到但**连续**，不断续"
        );
        assert!(dropped_seen.len() >= 10, "采到了足够多的点");
        // **单调不减**，而不是逐秒严格增长：安全网是锯齿式的。净失配把深度从目标
        // 推到门槛要十秒左右才排一次，所以相邻两秒常常相等——旧断言要求每秒都涨，
        // 那描述的是「每 tick 都在溢出」的旧行为。
        assert!(
            dropped_seen.windows(2).all(|w| w[1] >= w[0]),
            "`dropped` 只能涨，不能倒退"
        );
        assert!(
            dropped_seen.last().unwrap() > dropped_seen.first().unwrap(),
            "**丢弃必须一直在涨** —— 这是「稳态速率失配」区别于「被一次卡顿灌满」的唯一判据（规格 §3.3）"
        );
        // 每秒排掉的是失配量**减去伺服自己吃下的那一份**：注入 500 样本/秒，
        // 伺服在 ±500 ppm 上限能吸收 0.0005 × 48000 = 24 样本/秒，余下约 476
        // 归安全网。这个差值本身就是伺服确实在出力的证据。
        let per_sec = (dropped_seen.last().unwrap() - dropped_seen.first().unwrap()) as f64
            / (dropped_seen.len() - 1) as f64;
        assert!(
            (per_sec - 476.0).abs() < 30.0,
            "稳态每秒排掉的应是 500 − 伺服吃下的 24 ≈ 476, got {per_sec}"
        );
        // 深度被钉住 ⇒ 长期斜率≈0。**只看斜率会以为一切正常**，必须与 `dropped`
        // 一起读才能得出「正在持续丢」的结论——这一条在有安全网之后反而更要紧：
        // 从前是「饱和了所以不动」，现在是「被治住了所以不动」，两者深度读数相似
        // 而含义相反，唯一能分开它们的仍然是 `dropped` 的斜率。
        // `None` 在这里是**合法结论**，不是失败：安全网的锯齿本身就是噪声源，
        // `DriftFit::resolved()` 的情形 3（斜率没越过自己的 3σ 界、而界很松）
        // 正是为这种序列准备的。要求它一定报得出数字，等于要求遥测在分辨不出时
        // 也硬给一个——那恰好是 `drift_sps` 用 `Option` 而不是 0.0 的理由。
        match drift.slope(StageId::SrcFifo) {
            Some(late) => assert!(
                late.abs() < 30.0,
                "深度被钉住后长期斜率应接近 0, got {late}"
            ),
            None => { /* 锯齿噪声盖过了效应；`dropped` 的斜率才是此处的判据 */ }
        }
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
        assert!(
            (ms - 90.0).abs() < 2.0,
            "100 ms 进、10 ms 出 ⇒ 约 90 ms，got {ms:.2}"
        );
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
        assert_eq!(
            send_pace_for(&[Some(cap), Some(fifo)]),
            Some(StageDepth::send_pace())
        );

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
            .split("if let Some(playback) = playback.as_mut() {")
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

    pub(super) fn tx_stream_for(shared: &Arc<TxShared>) -> TxStream {
        TxStream {
            id: 7,
            crypto: MediaCrypto::new_for_stream(&[0u8; 32], 7, &[0u8; 16]),
            path: MediaPath::Udp("127.0.0.1:1".parse().unwrap()),
            spec: SourceSpec::Mic,
            channels: 1,
            loss: LossInjector::new(7, 0.0),
            seq: 0,
            media_frame_seq: 0,
            rung: audiohub_net::media::AUTO_TOP_RUNG,
            rs: None,
            rs_last: [0.0; 2],
            pay: Vec::with_capacity(F48_STEREO * 4),
            dest_epoch_seen: 0,
            gain: dsp::SendGain::new(),
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

    // ---------------------------------------------------- 源码守卫的公共设施
    //
    // ⚠ **本项目的源码守卫栽过一次「对注释免疫」**：判据写成
    // `branch.contains("dll.resync()")`，于是把那一行**注释掉**——功能没了、
    // 子串还在——守卫照样绿。所以下面所有源码扫描都走 [`code`]，它先把注释
    // 剥掉再交出来；[`stripping_comments_really_removes_them`] 守这件事本身。
    //
    // 第二个坑同样踩过：`include_str!("engine.rs")` 把**测试模块自己**也包进来，
    // 而测试里满是把被守卫的代码片段当字符串字面量写下的断言。于是
    // `!contains("开环那一行")` 会被自己的断言文本证伪，`fn_body` 也会在测试
    // 模块里的签名字面量上开始切。[`code`] 因此先把测试模块砍掉。

    /// 剥掉 `//` 行注释与 `/* */` 块注释；字符串字面量里的同名字符保持原样。
    ///
    /// 扫描器只跟踪三件事：字符串（含 `\` 转义、含跨行的续行字符串）、行注释、
    /// 块注释。**字符字面量刻意不跟踪** —— Rust 的生命周期 `'a` 和字符字面量
    /// `'x'` 用同一个引号，分不干净；而本文件里没有任何一个字符字面量含 `"`
    /// （唯三是 `'{}'`、`'{'`），所以不跟踪它不会污染字符串状态。
    /// 真有人写了 `'"'`，[`stripping_comments_really_removes_them`] 覆盖不到，
    /// 但那一刻别的断言会因为剥错而**变红**，不会静默放行。
    pub(crate) fn strip_comments(src: &str) -> String {
        let b = src.as_bytes();
        // **按字节扫，不按 `char`**：本文件里全是中文注释，而
        // `b[i] as char` 会把每一个 UTF-8 续字节当成一个 Latin-1 字符推出去，
        // 剥完的东西不再是原文（第一版就是这么错的，测试当场抓到）。
        // 按字节是安全的：`/ " \ \n` 全是 ASCII，UTF-8 的续字节恒 ≥ 0x80，
        // 不可能与它们相等。
        let mut out: Vec<u8> = Vec::with_capacity(b.len());
        let (mut i, mut in_str, mut in_line, mut in_block) = (0usize, false, false, false);
        while i < b.len() {
            let c = b[i];
            let d = if i + 1 < b.len() { b[i + 1] } else { 0 };
            if in_line {
                if c == b'\n' {
                    in_line = false;
                    out.push(b'\n');
                }
                i += 1;
                continue;
            }
            if in_block {
                if c == b'*' && d == b'/' {
                    in_block = false;
                    i += 2;
                    continue;
                }
                // 换行留着：行号与「分支体到哪结束」的缩进判据都靠它。
                if c == b'\n' {
                    out.push(b'\n');
                }
                i += 1;
                continue;
            }
            if in_str {
                out.push(c);
                if c == b'\\' {
                    if i + 1 < b.len() {
                        out.push(d);
                    }
                    i += 2;
                    continue;
                }
                if c == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if c == b'/' && d == b'/' {
                in_line = true;
                i += 2;
                continue;
            }
            if c == b'/' && d == b'*' {
                in_block = true;
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = true;
            }
            out.push(c);
            i += 1;
        }
        String::from_utf8(out).expect("剥注释只删整段 ASCII，不该破坏 UTF-8")
    }

    /// 本文件的**代码正文**：注释已剥掉、测试模块已砍掉。所有源码守卫都用它。
    pub(super) fn code() -> &'static str {
        static C: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        C.get_or_init(|| {
            let src = include_str!("engine.rs");
            let cut = src
                .find("\n#[cfg(test)]\npub(crate) mod tests {")
                .expect("找不到测试模块的起点 —— code() 会把测试自己的字面量也扫进去");
            strip_comments(&src[..cut])
        })
    }

    /// 取一个顶层函数的函数体。顶层 `}` 只在函数结束时顶格出现。
    ///
    /// `name` 给到左括号为止（如 `"pub(crate) fn tx_loop("`），这样多行签名
    /// 也认得出来 —— 上一版把整条单行签名写死，签名一加参数守卫就整批 panic。
    pub(super) fn fn_body(name: &str) -> &'static str {
        let src = code();
        let at = src.find(name).unwrap_or_else(|| panic!("找不到 {name}"));
        let open = at + src[at..].find(" {\n").expect("函数签名没有收尾") + 3;
        let end = open + src[open..].find("\n}\n").expect("函数没有结束");
        &src[open..end]
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
        let branch = skip_branch(fn_body("pub(crate) fn tx_loop("));
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
    /// **The starvation self-heal restarts the buffer; it does not re-tune it.**
    ///
    /// `JitterBuffer::new` reads `JbTuning::cached()` = `DEFAULT`, so writing
    /// the resync that way makes every self-heal on a tier 1 stream silently
    /// swap the `DEGRADED` profile for the tier 0 one and — through
    /// `with_tuning`'s `clamp(1, max_target)` — cut a learned depth of up to 40
    /// frames to 12. Nothing reports it: the envelope comes back on the next
    /// `reshape_jitter_envelope` pass, so the only visible trace is a buffer
    /// that has to re-earn its depth one frame per underrun.
    ///
    /// It is guarded in source because the trigger (`late_streak >= 50`) needs a
    /// stalled mixer or real cross-machine clock drift to reach, and the damage
    /// is invisible for the second it takes the envelope to return. The
    /// mechanism itself is tested in
    /// `media::ladder_tests`, test
    /// `rebuilding_a_buffer_through_its_own_tuning_keeps_the_depth_it_learned`.
    #[test]
    fn the_jb_resync_keeps_the_profile_the_stream_was_running() {
        let body = fn_body("pub(crate) fn handle_datagram(");
        assert!(
            body.contains("JitterBuffer::with_tuning_channels(")
                && body.contains("st.jb.tuning()")
                && body.contains("st.channels"),
            "the resync no longer rebuilds through the buffer's own tuning, so a tier 1 stream \
             loses DEGRADED (and its learned depth) on every self-heal"
        );
        assert!(
            !body.contains("JitterBuffer::new("),
            "a JitterBuffer is being built from the cached DEFAULT tuning inside handle_datagram; \
             on a degraded link that is the wrong profile"
        );
    }

    /// **plan §7.2 软件增益的接线守卫。**
    ///
    /// 增益本身在 `dsp::SendGain` 里有单测（斜坡、dither、只衰减一次）。这里守的
    /// 是**接线**，而接线恰恰是单测够不着、真机又要一台没有主音量的聚合设备才
    /// 触发得了的那一层：
    ///
    /// 1. 增益经 **`tx`** 施加（每条流一份）。挂到 `ent`（共享的源）上不是「串台
    ///    一次」而是**逐帧复利** —— 0.5 的增益在第 k 帧上累成 0.5ᵏ，而包数、
    ///    丢包率、音调探针全绿。
    /// 2. 在**编码之前**。写到 `encode_pcm_into` 之后，线上仍是满幅，兜底等于没接。
    /// 3. 在**重采样之后**：dither 必须紧贴量化器，先 dither 再线性插值会把它
    ///    低通掉。
    /// 4. 目标值从**每条流自己的** `TxShared` 上读。
    /// 5. 流循环不许拿到源的可变引用——那是第 1 条唯一可能的落地形态。
    #[test]
    fn the_send_gain_is_per_stream_and_lands_between_the_resampler_and_the_encoder() {
        let body = fn_body("pub(crate) fn tx_loop(");
        let at = body.find(".apply_interleaved(").unwrap_or_else(|| {
            panic!(
                "发送侧软件增益没有按流施加（plan §7.2）：要么根本没接，\
                 要么挂到了共享的源上\n{body}"
            )
        });
        let enc = body.find("encode_pcm_into(").expect("编码不见了");
        assert!(at < enc, "增益施加在编码之后 —— 线上仍是满幅，兜底等于没接");
        let rs = body.find("rs.process(").expect("重采样不见了");
        assert!(
            rs < at,
            "增益施加在重采样之前 —— dither 会被线性插值低通掉，TPDF 的性质没了"
        );
        assert!(
            body.contains("tx.shared.send_gain.load("),
            "增益目标不是从每条流自己的 TxShared 上读的"
        );
        assert!(
            !body.contains("sources.get_mut("),
            "流循环拿到了源的可变引用：增益一旦就地写进那份扇出的共享帧，\
             同一个源上的其余流全被带偏，而且逐帧复利"
        );
    }

    /// [`crate::SEND_GAIN_OFF`] 与任何合法增益都不可能撞上——`send_gain` 用
    /// 一个原子量同时表达「启不启用」和「多大」，这条编码是它成立的前提。
    ///
    /// 注入对照：把 `SEND_GAIN_OFF` 改成 `0`（= 增益 0.0 的位型），这条立刻变红，
    /// 而线上的表现会是「一开兜底就全程静音」。
    #[test]
    fn the_send_gain_off_sentinel_is_not_a_gain() {
        assert_eq!(
            TxShared::gain_of(crate::SEND_GAIN_OFF),
            1.0,
            "哨兵必须读作透明"
        );
        for i in 0..=1000u32 {
            let g = i as f32 / 1000.0;
            let bits = TxShared::gain_bits(g);
            assert_ne!(bits, crate::SEND_GAIN_OFF, "合法增益 {g} 撞上了哨兵");
            assert_eq!(TxShared::gain_of(bits), g, "增益 {g} 转一圈变了");
        }
        // 非有限值在编码这一步就被挡住，永远到不了 10 ms 线程。
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(TxShared::gain_of(TxShared::gain_bits(bad)), 1.0, "{bad}");
        }
        // 越界值被钳住而不是被当成非有限值。
        assert_eq!(TxShared::gain_of(TxShared::gain_bits(-1.0)), 0.0);
        assert_eq!(TxShared::gain_of(TxShared::gain_bits(9.0)), 1.0);
    }

    #[test]
    fn the_tx_deadline_is_driven_by_the_dll_not_by_open_loop_accumulation() {
        let body = fn_body("pub(crate) fn tx_loop(");
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
        let branch = skip_branch(fn_body("pub(crate) fn tx_loop("));
        for (needle, why) in [
            (
                "next_time = Instant::now()",
                "计划时刻没有重锚，循环会空转到追平",
            ),
            (
                "dll.resync()",
                "积分器没复位，跳变之后会按旧误差继续修正 ⇒ 过冲",
            ),
            ("dll_win.invalidate()", "排空当拍的水位仍会被当成有效观测"),
        ] {
            assert!(
                branch.contains(needle),
                "跳 tick 之后少了 `{needle}`：{why}\n{branch}"
            );
        }
    }

    /// 追平期一个观测都不喂（不变量 I6 的 DLL 侧）。
    ///
    /// 源侧已经挡了一道（不推进发布代次），这里是第二道。两道都要，因为写反的
    /// 表现是「偶尔有点断续」，靠听抓不住，而两处只要有一处漏了就复发。
    #[test]
    fn the_dll_is_only_fed_on_punctual_ticks() {
        let body = fn_body("pub(crate) fn tx_loop(");
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
        assert!(
            pull < feed,
            "在源被取之前就取观测 —— 拿到的是上一 tick 的读数"
        );
    }

    /// 空闲路径同样要重锚 + 复位，否则恢复的第一 tick 会被误判成一次 200 ms 卡顿。
    #[test]
    fn the_idle_path_re_anchors_the_schedule() {
        let body = fn_body("pub(crate) fn tx_loop(");
        let arm = body
            .split("if st.streams.is_empty() {")
            .nth(1)
            .expect("空闲短路分支");
        let arm = arm.split("\n        }").next().unwrap();
        assert!(
            arm.contains("next_time = Instant::now()"),
            "空闲之后没有重锚计划时刻 —— 恢复的第一 tick 会看到 200 ms 的落后，\
             直接触发治法 A 的丢弃\n{arm}"
        );
        assert!(arm.contains("dll.resync()"), "空闲之后没有复位环路\n{arm}");
        assert!(
            arm.contains("dll_win.invalidate()"),
            "空闲之后没有作废观测基准\n{arm}"
        );
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
            .split("pub(crate) fn raise_audio_thread_qos(")
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
        let branch = skip_branch(fn_body("pub(crate) fn mixer_loop("));
        assert!(
            branch.contains("MIX_SKIP.record("),
            "mixer 跳 tick 没有计数\n{branch}"
        );
        assert!(
            branch.contains("dlog!("),
            "mixer 跳 tick 没有日志\n{branch}"
        );
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
        let body = fn_body("pub(crate) fn tx_loop(");
        let punctual = body
            .find("let punctual = behind <= tick;")
            .expect("准时判据");
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
        // 积压必须**一次性**造出来：稳态喂再多也没用，安全网每 tick 都会把它排回
        // 目标水位。一次 120 ms 的到货正是消费侧卡顿之后的真实形态，而且落在安全网
        // 门限（target + 100 ms）之下，所以它留得住、可被 `drain_skipped` 排。
        let mut src = sys_frames(48_000, 48_000 * 130 / 1000);
        let mut out = Vec::new();
        src.next_frame(&mut out);
        let before = src.depths()[0].unwrap().samples;
        let dropped_before = src.depths()[0].unwrap().dropped;
        assert!(
            before as usize > 11 * F48,
            "precondition: the FIFO must contain a backlog, got {before}"
        );

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
        assert_eq!(
            src.drain_skipped(4_800),
            0,
            "已经只剩储备了，一个样本都不许再排"
        );
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
            let _qos_guard = raise_audio_thread_qos("test");
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

    #[cfg(target_os = "windows")]
    #[test]
    fn deadline_audio_threads_enter_windows_mmcss_pro_audio() {
        let active = std::thread::spawn(|| {
            let qos_guard = raise_audio_thread_qos("mmcss-unit-test");
            qos_guard.is_active()
        })
        .join()
        .expect("MMCSS test thread panicked");
        assert!(
            active,
            "the deadline thread remained at ordinary Windows scheduling priority"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn blocking_media_writers_never_enter_mmcss_pro_audio() {
        let engine = include_str!("engine.rs");
        let udp = fn_body("pub(crate) fn udp_send_loop(");
        assert!(udp.contains("raise_media_send_thread_qos("));
        assert!(!udp.contains("raise_audio_thread_qos("));
        for (name, source) in [
            ("mux", include_str!("mux.rs")),
            ("tcpmedia", include_str!("tcpmedia.rs")),
        ] {
            assert!(
                source.contains("raise_media_send_thread_qos("),
                "{name} writer lost its non-deadline scheduling wrapper"
            );
            assert!(
                !source.contains("raise_audio_thread_qos("),
                "{name} writer can perform blocking I/O at Pro Audio priority"
            );
        }
        assert!(
            engine.matches("raise_audio_thread_qos(\"").count() >= 2,
            "tx_loop and mixer_loop must still register the deadline helper"
        );
    }

    /// 两条音频循环都要提，而且要在循环开始**之前**提。
    #[test]
    fn both_audio_loops_raise_their_qos_before_the_loop() {
        for sig in ["pub(crate) fn tx_loop(", "pub(crate) fn mixer_loop("] {
            let body = fn_body(sig);
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
        assert_eq!(
            m.len(),
            2,
            "two slots must be two sources, not one shared ring"
        );
        // ...and the same slot twice is one source with two references, which
        // is what keeps the ring to a single consumer (halbridge SPSC rule).
        m.insert(SourceSpec::HalSpeaker { slot: 0 }, 12);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&SourceSpec::HalSpeaker { slot: 0 }], 12);
    }
}

// =========================================================================
//                       J1 守卫：截止期线程上不许有阻塞调用
// =========================================================================
//
// 本组守的是 `docs/spec-latency-floor.md` §9.3 手段 J1 的**全部收益**。
// 它们守的东西有一个共同的坏性质：**退回去之后一切照样跑**。
// 包数、丢包率、音调探针、听感——全都不动，只有 `tx_loop` 的停顿尾会重新变肥，
// 而那条尾只有跑上几小时、再和对端的 `jb_underruns` 对起来才看得见。
// 所以它们必须守在源码与运行时上，不能指望别的测试顺带抓到。
#[cfg(test)]
pub(crate) mod deadline_thread_guards {
    use super::tests::{code, fn_body, strip_comments};
    use super::*;
    use std::sync::mpsc;

    /// 截止期线程（`tx_loop` 及它同步调用的那几个）里**一个都不许出现**的调用，
    /// 以及各自一旦出现会发生什么。
    ///
    /// # 为什么是一张表，而不是一个名字
    ///
    /// 扩容前这里只禁 `.send_to(`。M8 的 tier 1（`tcpmedia.rs`）把媒体搬到一条
    /// TCP 上之后，「把系统调用放回截止期线程」的写法不再叫 `send_to`，叫
    /// `write` / `write_all` —— **守卫还在、被守的东西没了**。所有测试仍然全绿，
    /// 而 `write` 进内核网络栈的耗时上界与 `sendto` 一样不可预知。
    /// 加一条新传输就要往这张表里加一行。
    ///
    /// # 子串**不带前导的点**
    ///
    /// 带点只认得 `s.write_all(..)`；`Write::write_all(&mut s, ..)` 是同一件事的
    /// 另一种拼法，一个字都匹配不上。这不是推测——2026-08-07 的注入对照里，
    /// 带点的判据对着一行真的 `std::io::Write::write_all(..)` 报了绿。
    ///
    /// # 它必须跟着调用跨文件走
    ///
    /// 下面这张表只扫 `engine.rs` 里那三个函数体。`tx_loop` 今天还**同步调进**
    /// `tcpmedia::TcpMediaLink::enqueue` / `wake`（tier 1 的投递口），而它们在
    /// 另一个文件里 —— 禁表跟不过去。所以 `tcpmedia.rs` 的 guard 段引用的是
    /// **这一张**表，不是它自己抄的一份：抄一份的那一版会在这里加一行、那边
    /// 忘一行的时候静默失去覆盖，而那正是本条注释在讲的病。
    pub(crate) const BANNED_ON_THE_DEADLINE_THREAD: &[(&str, &str)] = &[
        ("send_to(", "sendto 进内核网络栈，单次耗时上界不可预知"),
        (
            "write(",
            "write 进内核网络栈；TCP 媒体（tier 1）就是靠它发的",
        ),
        ("write_all(", "同上，而且它会一直重试到写完，上界更差"),
        ("flush(", "flush 会把攒着的字节推进内核，与 write 同级"),
        (
            "write_frame(",
            "控制帧写在截止期线程上：JSON 序列化 + 阻塞 write",
        ),
    ];

    // ------------------------------------------------- 0. 守卫自己的守卫

    /// **剥注释必须真的把注释剥掉。**
    ///
    /// 这一条守的是本项目**已经栽过的**那个形态：判据写成
    /// `branch.contains("dll.resync()")`，于是把那一行注释掉——功能没了、
    /// 子串还在——守卫照样绿。下面每一条 `!contains(...)` 的可信度都压在这里。
    ///
    /// 注入对照：把 [`strip_comments`] 改成 `src.to_string()`（= 不剥），
    /// 本条的前三个断言立刻变红。
    #[test]
    fn stripping_comments_really_removes_them() {
        let s = strip_comments("let x = 1; // dll.resync()\n");
        assert!(!s.contains("dll.resync()"), "行注释没被剥掉：{s:?}");
        let s = strip_comments("a();\n/* udp.send_to(x);\n   还有一行 */\nb();\n");
        assert!(!s.contains("udp.send_to("), "块注释没被剥掉：{s:?}");
        assert!(
            s.contains("a();") && s.contains("b();"),
            "块注释剥过头了：{s:?}"
        );
        // 反向：字符串字面量里的同名字符**不许**被当成注释。
        let s = strip_comments(r#"let u = "https://x/y"; let c = "// 不是注释";"#);
        assert!(
            s.contains("https://x/y"),
            "把 URL 里的 // 当成注释了：{s:?}"
        );
        assert!(
            s.contains("// 不是注释"),
            "把字符串里的 // 当成注释了：{s:?}"
        );
        // 转义引号不许让扫描器提前出串。
        let s = strip_comments(r#"let e = "a\"// b"; c();"#);
        assert!(s.contains(r#"a\"// b"#), "转义引号处理错了：{s:?}");
        assert!(s.contains("c();"), "转义引号之后的代码丢了：{s:?}");
        // 而且 `code()` 真的不含注释里的中文标记（自证它走了这条路）。
        assert!(
            !code().contains("这一条守的是"),
            "code() 里还留着文档注释 —— 所有 !contains 守卫都不作数了"
        );
        assert!(code().contains("fn tx_loop("), "code() 把代码也剥掉了");
    }

    // ------------------------------------------------- 1. sendto

    /// **`tx_loop` 的 tick 里不许有 `send_to`。**
    ///
    /// `sendto` 进内核网络栈，单次调用的耗时上界不可预知；它是
    /// `raise_audio_thread_qos` 那段「给不出诚实的 computation 上界」论证的
    /// 唯一根据。搬回去之后一切照跑，只有停顿尾变肥。
    ///
    /// 注入对照：把 `inner.media_send.enqueue(...)` 换回
    /// `inner.udp.send_to(&dg, tx.dest)`，本条变红。
    ///
    /// # ⚠ M8 扩容：只禁 `.send_to(` 的那一版**已经不再保护任何东西**
    ///
    /// Tier 1（`tcpmedia.rs`）把媒体搬到一条 TCP 上之后，把系统调用放回截止期
    /// 线程的写法不再叫 `send_to`，叫 `write` / `write_all`。守卫还在、
    /// 被守的东西没了 —— **这是本阶段最隐蔽的一种退化**：所有测试仍然全绿，
    /// 而 `write` 进内核网络栈的耗时上界与 `sendto` 一样不可预知。
    ///
    /// 所以禁的是一张**表**，不是一个名字；加一条新传输就要往表里加一行。
    /// 判据全部跑在 [`code`]（已剥注释、已砍测试模块）上：本仓的 grep 守卫
    /// 对注释免疫过一次，那次的表现是「把功能注释掉，守卫照样绿」。
    #[test]
    fn the_send_tick_never_touches_the_socket_itself() {
        // 每一项：(被禁的子串, 它一旦出现在截止期线程上会发生什么)
        for f in [
            "pub(crate) fn tx_loop(",
            "fn apply_txcmd(",
            "fn refresh_dest(",
        ] {
            let body = fn_body(f);
            for (needle, why) in BANNED_ON_THE_DEADLINE_THREAD {
                assert!(
                    !body.contains(needle),
                    "{f} 里出现了 `{needle}` —— {why}。\n\
                     媒体必须**入队**（`media_send.enqueue` / `TcpMediaLink::enqueue`），\
                     由 `udp_send_loop` / `tcpmedia::write_loop` 去进内核。"
                );
            }
        }
        let body = fn_body("pub(crate) fn tx_loop(");
        let compact: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            compact.contains("inner.media_send.enqueue("),
            "tx_loop 不再往 UDP 发送队列里投递了 —— 那它是怎么把音频发出去的？"
        );
        assert!(
            body.contains("link.enqueue("),
            "tx_loop 不再往 tier 1 队列里投递了 —— 要么 tier 1 的发送路径没了，\
             要么它改成在这条线程上直接写 socket 了"
        );
        // 而 `sendto` 必须还在，只是在发送线程上。
        assert!(
            fn_body("pub(crate) fn udp_send_loop(").contains("inner.udp.send_to("),
            "发送线程不发包了"
        );
    }

    /// **上面那张禁表必须真的能抓到每一种写法。**
    ///
    /// 守卫扩容之后最容易出的事有两种，这条都守：
    ///   1. 表里漏了一种拼法（`sk.write_all(..)` 与
    ///      `Write::write_all(&mut sk, ..)` 是同一件事的两种写法）；
    ///   2. 表写对了，但判据跑在一份看不见它的文本上——例如 [`fn_body`] 因为
    ///      签名改动切错了范围，于是每一条 `!contains` 都在空串上成立。
    ///
    /// ⚠ **这条测试本身第一次写错过，值得记下来**：它原本比的是一个硬写的
    /// 子串字面量（`fake.contains("write_all(")`），而不是
    /// [`BANNED_ON_THE_DEADLINE_THREAD`]。于是把 `write_all(` 从表里删掉之后
    /// 它照样绿——**它测的是它自己的字面量，不是那张表**。这正是本仓「测试是
    /// 戏剧」的标准形态，而且是在注入对照里当场抓到的。
    ///
    /// 注入对照（2026-08-07 实跑）：从表里删掉任意一行，对应的样本没人认领，
    /// 本条变红并指名道姓说出是哪一份样本漏网。
    #[test]
    fn the_banned_call_list_actually_matches_a_write() {
        // 每一份都是「有人把 socket 写搬回了截止期线程」的一种真实拼法。
        const REGRESSIONS: &[&str] = &[
            "fn t() {\n    sk.write_all(&dg);\n}\n",
            "fn t() {\n    std::io::Write::write_all(&mut sk, &dg);\n}\n",
            "fn t() {\n    let n = sk.write(&dg)?;\n}\n",
            "fn t() {\n    sk.flush()?;\n}\n",
            "fn t() {\n    inner.udp.send_to(&dg, dest);\n}\n",
            "fn t() {\n    write_frame(&mut s, &msg)?;\n}\n",
        ];
        for sample in REGRESSIONS {
            let text = strip_comments(sample);
            assert!(
                BANNED_ON_THE_DEADLINE_THREAD
                    .iter()
                    .any(|(n, _)| text.contains(n)),
                "禁表里没有任何一条认领得了这份回归样本，于是它可以原样落进 tx_loop：\n{text}"
            );
        }
        // 而真正的 tx_loop 体是非空的（切范围没切歪）。
        let body = fn_body("pub(crate) fn tx_loop(");
        assert!(
            body.len() > 2000,
            "tx_loop 的函数体只有 {} 字节，切范围歪了",
            body.len()
        );
        assert!(
            body.contains("tx.crypto.seal_into("),
            "切出来的不是 tx_loop 的正文"
        );
    }

    /// **队列满了要丢，不许阻塞、不许无界。**
    ///
    /// 阻塞 = 白搬；无界 = 卡顿之后把一串陈包灌给对端（治法 A 反过来做一遍）。
    #[test]
    fn the_send_queue_is_bounded_and_drops_rather_than_waits() {
        let s = UdpSender::new();
        assert_eq!(s.capacity(), SEND_SLOTS);
        let shared = Arc::new(TxShared::new());
        let dest: SocketAddr = "127.0.0.1:1".parse().unwrap();
        // 一条都不消费，把它灌满再多灌 5 条。
        for i in 0..SEND_SLOTS + 5 {
            let ok = s.enqueue(dest, &shared, 0, |b: &mut Vec<u8>| {
                b.clear();
                b.extend_from_slice(&(i as u32).to_le_bytes());
                true
            });
            assert_eq!(ok, i < SEND_SLOTS, "第 {i} 条的收/拒判断不对");
        }
        assert_eq!(s.queued(), SEND_SLOTS, "队列长过了容量 —— 它不是有界的");
        assert_eq!(s.dropped(), 5, "满了之后丢的那几条没有被数出来");
    }

    /// **封包失败的那一条不许被发出去。**（`seal_into` 出错 ⇒ 槽里是半截字节）
    #[test]
    fn a_failed_seal_never_reaches_the_wire() {
        let s = UdpSender::new();
        let shared = Arc::new(TxShared::new());
        let dest: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(!s.enqueue(dest, &shared, 0, |b: &mut Vec<u8>| {
            b.clear();
            b.extend_from_slice(b"half-written");
            false // 封包失败
        }));
        assert_eq!(s.queued(), 0, "作废的数据报还是排进去了");
    }

    /// **发送槽的缓冲复用之后不再分配。**
    ///
    /// 走满四整圈，每个槽被复用四次；同一个槽每次的首地址与容量都必须一样。
    /// 注入对照：把 `SendSlot.buf` 的填充改成 `*b = Vec::from(...)`（= 每次
    /// 换一块新内存），第二圈起地址就对不上，本条变红。
    ///
    /// ⚠ **payload 必须是阶梯最深档的真实帧长**，不是一个随手写的 1000。
    /// 这条测试此前用 `[7u8; 1000]`，而 `SEND_SLOT_BYTES` 那时是 1152 ——
    /// 于是「切到深档时 128 个槽各扩容一次（= 128 次 malloc 撒在 10 ms 截止期
    /// 线程上）」这颗地雷从它下面整个走过去了。用真实帧长之后，任何一次让
    /// `SEND_SLOT_BYTES` 跟不上阶梯的改动都会在这里现形。
    ///
    /// 注入对照 2：把 `SEND_SLOT_BYTES` 改回 1152，这条立刻红在「换了内存」。
    #[test]
    fn the_send_slots_stop_allocating_after_the_first_lap() {
        let s = UdpSender::new();
        let shared = Arc::new(TxShared::new());
        let dest: SocketAddr = "127.0.0.1:1".parse().unwrap();
        // 最深档的**明文**帧长；`enqueue` 的闭包里再套上头与标签就是
        // `DEEPEST_SEALED_FRAME_BYTES`。这里直接按密文长度填，等价且更严。
        let payload = vec![7u8; DEEPEST_SEALED_FRAME_BYTES];
        assert!(
            payload.len() > 1152,
            "最深档的帧居然没超过旧的 SEND_SLOT_BYTES —— 这条测试就白写了"
        );
        let mut seen: Vec<(usize, usize)> = Vec::new();
        for _ in 0..SEND_SLOTS * 4 {
            assert!(s.enqueue(dest, &shared, 0, |b: &mut Vec<u8>| {
                b.clear();
                b.extend_from_slice(&payload);
                true
            }));
            assert!(s
                .q
                .consume(|slot| seen.push((slot.buf.as_ptr() as usize, slot.buf.capacity()))));
        }
        for i in SEND_SLOTS..seen.len() {
            assert_eq!(
                seen[i],
                seen[i - SEND_SLOTS],
                "第 {i} 次用到的槽换了内存 —— 发送路径上又开始 malloc 了"
            );
        }
    }

    /// 发送线程把 `Arc<TxShared>` **拿走**再析构：截止期线程上不许掉引用计数
    /// 到 0（那会在音频线程上跑 `TxShared` 的析构，里面有三把 `Mutex`）。
    #[test]
    fn the_consumer_takes_the_owner_out_of_the_slot() {
        let s = UdpSender::new();
        let shared = Arc::new(TxShared::new());
        let dest: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(s.enqueue(dest, &shared, 0, |b: &mut Vec<u8>| {
            b.clear();
            true
        }));
        assert_eq!(Arc::strong_count(&shared), 2, "槽里应当持着一份引用");
        let mut took = false;
        assert!(s.q.consume(|slot| took = slot.owner.take().is_some()));
        assert!(took, "消费者没有把 owner 取走");
        assert_eq!(Arc::strong_count(&shared), 1, "引用没被消费者放掉");
        // 源码侧：`take()` 必须在**发送线程**的函数体里。
        assert!(
            fn_body("pub(crate) fn udp_send_loop(").contains("slot.owner.take()"),
            "owner 不再由发送线程取走 —— TxShared 的析构会落回音频线程"
        );
    }

    /// **发送队列恰好一个生产者、恰好一个消费者。**
    ///
    /// 这不是风格问题：它是 [`crate::rtsafe::SpscRing`] 里那几处 `unsafe` 的
    /// **全部**依据。多一个生产者，两条线程就会同时对同一个槽持可变引用 ——
    /// UB，而且表现多半是「偶尔一个数据报的字节是两个包拼起来的」，
    /// 对端 AEAD 校验失败静默丢弃，只剩丢包率里的一点点异常。
    ///
    /// `send_pullreq`（ticker，1 Hz）**故意**不走队列：它一走就是第二个生产者。
    /// 它直接 `send_to` 是安全的（UDP socket 本身多线程安全），而且它不在任何
    /// 截止期线程上。
    #[test]
    fn the_send_queue_has_exactly_one_producer_and_one_consumer() {
        let src = code();
        let compact: String = src.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert_eq!(
            compact.matches("media_send.enqueue(").count(),
            1,
            "发送队列有了第二个生产者 —— SpscRing 的 unsafe 前提当场作废"
        );
        assert_eq!(
            compact.matches("media_send.q.consume(").count(),
            1,
            "发送队列有了第二个消费者 —— 同上"
        );
        let tx: String = fn_body("pub(crate) fn tx_loop(")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        let udp: String = fn_body("pub(crate) fn udp_send_loop(")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(tx.contains("media_send.enqueue("));
        assert!(udp.contains("media_send.q.consume("));
        let ka = fn_body("pub(crate) fn send_pullreq(");
        assert!(ka.contains("inner.udp.send_to("), "keepalive 不再直接发了");
        assert!(
            !ka.contains("media_send"),
            "keepalive 走进了发送队列 = 第二个生产者"
        );
    }

    // ------------------------------------------------- 2. 日志

    /// **两条截止期循环都必须在进 `loop` 之前把日志切成延迟落盘。**
    ///
    /// 行为侧的断言在 `crate::rtlog::tests`（真的入队、真的不写 stderr）；
    /// 这里守的是**接线**——少一行 `arm`，那条线程的每一次 `dlog!` 就重新变成
    /// 一次阻塞 `write` 加一次 `Stderr` 全局锁，而日志内容一个字都不会变。
    #[test]
    fn both_deadline_loops_defer_their_logging_before_entering_the_loop() {
        for f in ["pub(crate) fn tx_loop(", "pub(crate) fn mixer_loop("] {
            let body = fn_body(f);
            let arm = body
                .find("rtlog::arm(")
                .unwrap_or_else(|| panic!("{f} 没有把日志切到延迟落盘"));
            let lp = body.find("\n    loop {").expect("循环");
            assert!(arm < lp, "{f} 的 rtlog::arm 写在了循环里面");
        }
    }

    /// 上一轮刚加的**欠载段首/段尾**日志必须还在（`halbridge` 侧）。
    ///
    /// 「把 `dlog!` 搬出音频线程」有一个偷懒的做法是**直接删掉它们**。那会把
    /// 上一轮花力气建的观测性一起删掉，而且同样不会有任何测试变红。
    #[test]
    fn the_underrun_segment_logs_are_still_there() {
        let hal = include_str!("halbridge.rs");
        for needle in ["欠载开始 slot", "欠载结束 slot"] {
            assert!(
                hal.contains(needle),
                "`{needle}` 的埋点没了 —— 延迟落盘是为了留住它们，不是为了删掉它们"
            );
        }
    }

    // ------------------------------------------------- 4. dest_override

    /// **稳态下这条 tick 不许碰 `dest_override` 那把锁。**
    ///
    /// 这是一条**运行时**判据，不是 grep：测试线程把锁攥在手里，然后让
    /// `refresh_dest` 在另一条线程上跑。没变代号 ⇒ 它必须立刻返回；
    /// 变了代号 ⇒ 它必须被锁挡住（证明「它确实会去拿锁」，也就证明了前一半
    /// 不是因为代码根本没有这条路径才通过的）。
    ///
    /// 注入对照：把 `refresh_dest` 开头的 `if epoch == tx.dest_epoch_seen { return; }`
    /// 删掉（= 回到每 tick 加锁），第一段的 `join` 会超时，本条变红。
    #[test]
    fn the_steady_tick_does_not_take_the_destination_lock() {
        use std::sync::mpsc::RecvTimeoutError;

        let shared = Arc::new(TxShared::new());
        let run = |shared: &Arc<TxShared>, seen: u64| {
            let (tx_done, rx_done) = mpsc::channel::<u64>();
            let sh = shared.clone();
            std::thread::spawn(move || {
                let mut st = super::tests::tx_stream_for(&sh);
                st.dest_epoch_seen = seen;
                refresh_dest(&mut st);
                let _ = tx_done.send(st.dest_epoch_seen);
            });
            rx_done.recv_timeout(Duration::from_millis(500))
        };

        // ① 代号没变（都是 0）：锁被我们攥着，它也必须立刻回来。
        let held = lk(&shared.dest_override);
        assert!(
            run(&shared, 0).is_ok(),
            "代号没变却去拿了锁 —— 每 tick 一次的锁竞争回来了"
        );
        drop(held);

        // ② 代号变了：它**必须**去拿锁，所以攥着锁时它回不来。
        shared.dest_epoch.fetch_add(1, Ordering::Release);
        let held = lk(&shared.dest_override);
        assert!(
            matches!(run(&shared, 0), Err(RecvTimeoutError::Timeout)),
            "代号变了却没去读地址 —— 那 keepalive 学到的端口就永远用不上了"
        );
        drop(held);

        // ③ 锁放开之后，新地址真的被取走了。
        let learned: SocketAddr = "127.0.0.1:65000".parse().unwrap();
        *lk(&shared.dest_override) = Some(learned);
        let mut st = super::tests::tx_stream_for(&shared);
        st.dest_epoch_seen = 0;
        refresh_dest(&mut st);
        assert_eq!(st.path.udp_dest(), Some(learned), "代号动了但地址没被采纳");
        assert_eq!(
            st.dest_epoch_seen,
            shared.dest_epoch.load(Ordering::Acquire)
        );
    }

    /// **Tier 1 上 `refresh_dest` 与 `send_pullreq` 都不执行**（M8 设计 §4.2 第 3 条）。
    ///
    /// keepalive 存在的理由是给一条 UDP 流撑开 NAT/防火墙状态、并教发送侧对端
    /// 用的哪个端口。TCP 媒体链路两件都不需要，而且**根本没有地址可以发**。
    ///
    /// 判据写成「它连锁都不去拿」而不是「地址没变」：后者在
    /// [`MediaPath::Tcp`] 上恒成立（没有地址可变），于是一条把 `let MediaPath::Udp
    /// (..) else { return }` 删掉的改动照样绿——而那条改动会让一条 tier 1 流
    /// 每次代号变动都去抢一次 `rx_loop` 持着的锁。
    ///
    /// 注入对照：把 `refresh_dest` 开头那行 `let MediaPath::Udp(dest) = ... else
    /// { return }` 换成 `if let MediaPath::Udp(..) = tx.path {}`（= 不再早退），
    /// 第一条断言变红（`dest_epoch_seen` 被推进了）。
    #[test]
    fn a_tier_one_stream_neither_learns_a_destination_nor_sends_a_keepalive() {
        let shared = Arc::new(TxShared::new());
        *lk(&shared.dest_override) = Some("127.0.0.1:65000".parse().unwrap());
        shared.dest_epoch.fetch_add(1, Ordering::Release);

        let mut st = super::tests::tx_stream_for(&shared);
        st.path = MediaPath::Tcp(Arc::new(crate::tcpmedia::TcpMediaLink::new_for_test(
            "fp".into(),
            "127.0.0.1:1".parse().unwrap(),
        )));
        st.dest_epoch_seen = 0;
        refresh_dest(&mut st);
        assert_eq!(
            st.dest_epoch_seen, 0,
            "refresh_dest 在 tier 1 流上仍然读了代号 —— 早退没了，锁竞争也就回来了"
        );
        assert!(
            st.path.udp_dest().is_none(),
            "tier 1 的路径上长出了一个 UDP 目的地"
        );

        // keepalive：判据是「没有目的地」，而 `send_pullreq` 的第一行正是据此早退。
        let body = fn_body("pub(crate) fn send_pullreq(");
        assert!(
            body.contains("ka_path.udp_dest()"),
            "send_pullreq 不再按路径判断了 —— tier 1 上它会往一个编出来的地址发"
        );
        assert!(
            body.find("ka_path.udp_dest()") < body.find("Header {"),
            "早退不在最前面：keepalive 的包头已经造好了才发现没地方发"
        );
    }

    /// **Acceptance 4 (design §6, P5), the `refresh_dest` half: on
    /// `MediaPath::Framed` it does the work **zero** times.**
    ///
    /// Counted, not grepped. The count is how many times `refresh_dest`
    /// advanced `dest_epoch_seen` — i.e. how many times it went and read the
    /// address — over N epoch bumps. Its sibling above uses the same
    /// observable for tier 1; the difference here is the positive control,
    /// which is what stops "zero" from being true because nothing runs.
    ///
    /// On tier 2 the stakes are higher than on tier 1. `conn.peer_ip` is the
    /// **tunnel's** address and `peer.port` a number about a listener the tunnel
    /// does not expose, so a `refresh_dest` that ran here would not merely waste
    /// a lock — it would install a well-formed address belonging to somebody
    /// else and send media there.
    #[test]
    fn a_tier_two_stream_never_learns_a_udp_destination() {
        const BUMPS: u64 = 6;

        let count_updates = |path: crate::tcpmedia::MediaPath| -> u64 {
            let shared = Arc::new(TxShared::new());
            *lk(&shared.dest_override) = Some("127.0.0.1:65000".parse().unwrap());
            let mut st = super::tests::tx_stream_for(&shared);
            st.path = path;
            st.dest_epoch_seen = 0;
            let mut updates = 0;
            for _ in 0..BUMPS {
                shared.dest_epoch.fetch_add(1, Ordering::Release);
                let before = st.dest_epoch_seen;
                refresh_dest(&mut st);
                if st.dest_epoch_seen != before {
                    updates += 1;
                }
            }
            updates
        };

        // Positive control: the same loop, the same call, a UDP path.
        assert_eq!(
            count_updates(MediaPath::Udp("127.0.0.1:1".parse().unwrap())),
            BUMPS,
            "the tier 0 path did not learn its address either, so the zero below proves nothing"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let peer = listener.local_addr().expect("addr");
        let link = Arc::new(crate::tcpmedia::TcpMediaLink::new_for_test(
            "fp".into(),
            peer,
        ));
        assert_eq!(
            count_updates(MediaPath::Framed(crate::mux::MuxLink::new_for_test(
                link, peer
            ))),
            0,
            "a tier 2 stream went and read a UDP destination; on this transport the address it \
             would have found belongs to the tunnel, not to the peer"
        );
    }

    /// `rx_loop` 学到新地址之后必须**在写完值之后**推代号。
    /// 反过来写就是「代号说变了、锁里还是旧值」——而代号不会再动第二次。
    #[test]
    fn the_receiver_bumps_the_epoch_after_it_writes_the_address() {
        let body = fn_body("fn handle_datagram(");
        let arm = body
            .split("let learned = SocketAddr::new(")
            .nth(1)
            .expect("keepalive 分支");
        let write = arm.find("*d = Some(learned);").expect("写地址");
        let bump = arm.find("dest_epoch.fetch_add(").expect("推代号");
        assert!(write < bump, "代号推在了写地址之前");
    }

    // ------------------------------------------------- 5. 建源 / 收尸

    /// **开设备只能发生在建源线程上。**
    ///
    /// `build_source` 会一路调到 `MicSource::open` / `sysaudio::start_backend`
    /// ——110–600 ms 量级。它的调用点全集必须落在 `source_builder_loop` 的
    /// 函数体里。
    ///
    /// 注入对照：在 `apply_txcmd` 里加回一句 `build_source(...)`，本条变红。
    #[test]
    fn opening_a_device_only_ever_happens_on_the_builder_thread() {
        let src = code();
        let builder = fn_body("pub(crate) fn source_builder_loop(");
        assert!(builder.contains("build_source("), "建源线程自己不建源了？");
        // 全文件的调用点：定义那一处 + 建源线程里的那些，别无他处。
        let def = src.find("fn build_source(").expect("build_source 的定义");
        let b_at = src.find(builder).expect("建源线程的函数体");
        let b_end = b_at + builder.len();
        let mut from = 0;
        while let Some(i) = src[from..].find("build_source(") {
            let at = from + i;
            from = at + 1;
            if at == def || (at > def - 4 && at <= def + 3) {
                continue; // `fn build_source(` 本身
            }
            assert!(
                at >= b_at && at < b_end,
                "字节 {at} 处有一个 build_source 调用点在建源线程之外 —— \
                 开一次 CoreAudio 输入设备会把 10 ms 截止期直接打穿：\n{}",
                &src[at.saturating_sub(200)..(at + 60).min(src.len())]
            );
        }
        // 截止期线程上也不许**析构**一个源：关设备和开设备一样慢。
        for f in [
            "pub(crate) fn tx_loop(",
            "fn apply_txcmd(",
            "fn reap_dead_sources(",
        ] {
            assert!(
                !fn_body(f).contains("drop(src)"),
                "{f} 里就地丢了一个源 —— 关设备也要走建源线程（BuildReq::Retire）"
            );
        }
        assert!(
            fn_body("fn reap_dead_sources(").contains("st.retire("),
            "收尸没有交给建源线程"
        );
    }

    fn tone() -> Src {
        Src::Frame(Box::new(ToneSource::new(
            440.0,
            TONE_AMP,
            48000,
            FRAME_MS as u32,
        )))
    }

    fn add_cmd(id: u32, spec: SourceSpec) -> (TxCmd, mpsc::Receiver<Result<(), String>>) {
        let (a, r) = mpsc::channel();
        (
            TxCmd::Add {
                stream_id: id,
                key: [0u8; 32],
                salt: vec![0u8; 16],
                path: MediaPath::Udp("127.0.0.1:1".parse().unwrap()),
                spec,
                channels: 1,
                loss_pct: 0.0,
                shared: Arc::new(TxShared::new()),
                ack: Some(a),
            },
            r,
        )
    }

    /// 两条流要同一个源 ⇒ **只开一次设备**，两条一起装上，引用数为 2。
    #[test]
    fn two_streams_wanting_one_source_share_a_single_build() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, ack1) = add_cmd(1, SourceSpec::Mic);
        let (c2, ack2) = add_cmd(2, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        apply_txcmd(&mut st, c2);
        // 只发了一条建源请求。
        let gen = match br.try_recv().expect("没发建源请求") {
            BuildReq::Build { spec, gen } => {
                assert_eq!(spec, SourceSpec::Mic);
                gen
            }
            _ => panic!("第一条不是 Build"),
        };
        assert!(
            br.try_recv().is_err(),
            "同一个源发了两次 Build = 开两次设备"
        );
        // 谁都还没被 ack（设备还没开出来）。
        assert!(ack1.try_recv().is_err() && ack2.try_recv().is_err());
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen,
            result: Ok(tone()),
        });
        assert_eq!(st.streams.len(), 2);
        assert_eq!(st.sources[&SourceSpec::Mic].refs, 2, "引用数不等于等的人数");
        assert_eq!(ack1.try_recv().unwrap(), Ok(()));
        assert_eq!(ack2.try_recv().unwrap(), Ok(()));
    }

    /// 建源失败 ⇒ 每个等的人都拿到**真实理由**，一条流都不装。
    #[test]
    fn a_failed_build_answers_every_waiter_with_the_real_reason() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, ack1) = add_cmd(1, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen,
            result: Err("no default input device".into()),
        });
        assert!(st.streams.is_empty(), "失败了还把流装上了");
        assert!(st.sources.is_empty());
        assert_eq!(
            ack1.try_recv().unwrap(),
            Err("no default input device".into())
        );
    }

    /// 等的人在设备开出来**之前**就撤了 ⇒ 成品直接收尸，不许留一个没人读的设备。
    ///
    /// 这条路是真会走到的：`conn.rs` 的 `SOURCE_ACK_TIMEOUT` 到点就发 Remove。
    #[test]
    fn a_source_nobody_waits_for_any_more_is_retired_not_installed() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, _ack1) = add_cmd(1, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        apply_txcmd(&mut st, TxCmd::Remove { stream_id: 1 });
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen,
            result: Ok(tone()),
        });
        assert!(st.sources.is_empty(), "没人要的源被装上了");
        assert!(st.streams.is_empty());
        assert!(
            matches!(br.try_recv(), Ok(BuildReq::Retire { gen: g, .. }) if g == gen),
            "没人要的源没有被交回去收尸 —— 设备就一直开着"
        );
    }

    /// 设备变更重建：**先造好新的、这一刻才丢老的**，而且交回去收尸的是**旧**代号。
    ///
    /// 代号写错（比如带上新代号）的后果是建源线程把**刚开好的**那个采集流关掉，
    /// 于是麦克风从此静音，而所有计数器一切正常。
    #[test]
    fn a_rebuild_swaps_in_place_and_retires_exactly_the_old_generation() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, _a) = add_cmd(1, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        let BuildReq::Build { gen: g0, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen: g0,
            result: Ok(tone()),
        });
        assert_eq!(st.sources[&SourceSpec::Mic].gen, g0);

        request_mic_rebuild(&mut st);
        let BuildReq::Build { gen: g1, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        assert_ne!(g0, g1);
        // 重建在途期间，老的源一个字节都没动 —— 「新设备打不开就保留旧采集」
        // 这条保证就是靠它兑现的。
        assert_eq!(
            st.sources[&SourceSpec::Mic].gen,
            g0,
            "重建还没回来就把老的换掉了"
        );
        assert!(br.try_recv().is_err(), "重建在途还发了第二条请求");

        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen: g1,
            result: Ok(tone()),
        });
        assert_eq!(st.sources[&SourceSpec::Mic].gen, g1, "没换成新的");
        assert_eq!(st.sources[&SourceSpec::Mic].refs, 1, "换芯把引用数弄丢了");
        assert!(
            matches!(br.try_recv(), Ok(BuildReq::Retire { gen: g, .. }) if g == g0),
            "收尸带的不是旧代号 —— 建源线程会去关掉刚开好的那条采集流"
        );
    }

    /// 重建失败 ⇒ 老的原样留着（spec-m4c §D 的「保留原来的采集」）。
    #[test]
    fn a_failed_rebuild_keeps_the_previous_capture() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, _a) = add_cmd(1, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        let BuildReq::Build { gen: g0, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen: g0,
            result: Ok(tone()),
        });
        request_mic_rebuild(&mut st);
        let BuildReq::Build { gen: g1, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen: g1,
            result: Err("device busy".into()),
        });
        assert_eq!(st.sources[&SourceSpec::Mic].gen, g0, "重建失败却把老的丢了");
        assert!(br.try_recv().is_err(), "重建失败却收了尸");
    }

    /// **在建的虚拟扬声器槽必须算作 busy。**
    ///
    /// 不算的话，`drain_idle_speakers` 会和建源线程里那次开流排空同时推同一个
    /// SPSC 环的 `read_idx`。两个消费者，数据被撕开，两边都不报错。
    ///
    /// 注入对照：把 `busy_speakers` 里的 `.chain(self.pending.keys())` 去掉，
    /// 本条变红。
    #[test]
    fn a_speaker_slot_being_opened_already_counts_as_busy() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        assert_eq!(st.busy_speakers(), 0);
        let (c, _a) = add_cmd(1, SourceSpec::HalSpeaker { slot: 3 });
        apply_txcmd(&mut st, c);
        assert_eq!(
            st.busy_speakers(),
            1 << 3,
            "槽 3 还在建源线程手里，却被当成空闲去排空了 —— 一个环两个消费者"
        );
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::HalSpeaker { slot: 3 },
            gen,
            result: Ok(tone()),
        });
        assert_eq!(st.busy_speakers(), 1 << 3, "装上之后反而不 busy 了");
        apply_txcmd(&mut st, TxCmd::Remove { stream_id: 1 });
        assert_eq!(st.busy_speakers(), 0, "流没了槽还占着");
    }

    /// 源的引用降到 0 ⇒ 交回去收尸（而不是在这条线程上 `drop`）。
    #[test]
    fn the_last_stream_leaving_retires_the_source_off_thread() {
        let (bs, br) = mpsc::channel();
        let mut st = TxState::new(bs);
        let (c1, _a) = add_cmd(1, SourceSpec::Mic);
        let (c2, _b) = add_cmd(2, SourceSpec::Mic);
        apply_txcmd(&mut st, c1);
        apply_txcmd(&mut st, c2);
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else {
            panic!()
        };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen,
            result: Ok(tone()),
        });
        apply_txcmd(&mut st, TxCmd::Remove { stream_id: 1 });
        assert!(br.try_recv().is_err(), "还有一条流在用，不该收尸");
        assert_eq!(st.sources[&SourceSpec::Mic].refs, 1);
        apply_txcmd(&mut st, TxCmd::Remove { stream_id: 2 });
        assert!(
            matches!(br.try_recv(), Ok(BuildReq::Retire { gen: g, .. }) if g == gen),
            "最后一条流走了却没把源交出去 —— 设备在音频线程上析构"
        );
        assert!(st.sources.is_empty());
    }
}

// ---------------------------------------------------------------- 5 ms 分包
//
// 深档（48 kHz/24 bit、48 kHz/32f）的整帧明文装不进一个以太网数据报，线上按
// 5 ms 切成两个包发。这一组守的是那条路径上四件**都不会报错**的事：
// 分包判据、seq 与时间戳的构造、接收侧的配对、以及搭档缺席时的出路。
#[cfg(test)]
mod start_rung_tests {
    use super::*;
    use audiohub_net::media::{rung_format, LADDER};

    /// A stream's starting rung and its resampler **must be decided together**.
    ///
    /// # The bug this exists for, in full
    ///
    /// A send stream used to start on `AUTO_TOP_RUNG` = rung 2, which is
    /// 48 kHz, which needs no resampler — so `rs: None` at install time was
    /// correct *by coincidence*. Making the starting rung per-transport
    /// (rung 3 = 32 kHz on tier 1) broke that coincidence and produced a stream
    /// whose header declared 32 kHz while its payload was still 48 kHz worth of
    /// samples. Measured 2026-08-08: `format_mismatch` climbed once per frame,
    /// 2002 frames in 20 s, the receiver discarded every one, and the only
    /// symptom above the log line was a tone verdict that never appeared.
    ///
    /// So this asserts the rule at every rung, not just the two that happen to
    /// be reachable today.
    #[test]
    fn every_rung_that_needs_a_resampler_gets_one_at_install_time() {
        for rung in 0..LADDER.len() as u32 {
            let rate = rung_format(rung).rate_hz;
            assert_eq!(
                resampler_for(rung, 2, [0.0; 2]).is_some(),
                rate != MicSource::OUT_RATE,
                "rung {rung} is {rate} Hz: a stream installed here would put {} samples on the \
                 wire under a header declaring {rate} Hz",
                if rate == MicSource::OUT_RATE {
                    "the right number of"
                } else {
                    "48 kHz"
                }
            );
        }
    }

    /// The install path may not hand-roll the decision above.
    ///
    /// Source text rather than behaviour because `install_stream` needs the
    /// whole `TxState` machinery plus a real device; what actually failed was
    /// a literal `rs: None` sitting next to a rung that was no longer always
    /// 48 kHz, and that is exactly what this reads.
    #[test]
    fn the_install_path_derives_the_resampler_from_the_starting_rung() {
        // Not `tests::fn_body`: that one keys off a top-level `\n}` and this is
        // a method inside an `impl`, so it would run to the end of the block.
        let src = tests::code();
        let at = src
            .find("fn install_stream(")
            .expect("install_stream is gone");
        let body = &src[at..at + src[at..].find("\n    }\n").expect("no end of method")];
        let body = tests::strip_comments(body);
        assert!(
            body.contains("rs: resampler_for(start_rung"),
            "install_stream no longer derives the resampler from the rung it installs"
        );
        assert!(
            !body.contains("rs: None"),
            "install_stream installs a stream with no resampler; that is only correct while the \
             starting rung is a 48 kHz one, and it is not on tier 1"
        );
    }
}

#[cfg(test)]
mod wire_split_tests {
    use super::*;
    use audiohub_core::dsp::WireDepth;
    use audiohub_net::media::{rung_format, WireFormat, LADDER};

    /// 分包只发生在装不下的那两档，且**判据只由帧长决定**。
    ///
    /// 注入对照：把 `SINGLE_PACKET_PAYLOAD_MAX` 抬到 2000（= 所有档都不分包），
    /// 这条红在「分包的格变了」；同时 `media.rs` 的 MTU 断言也会红——
    /// 两条从相反方向钉住同一件事。
    #[test]
    fn only_the_two_deep_rungs_split_and_the_halves_are_equal_length() {
        for (i, f) in LADDER.iter().enumerate() {
            let parts = f.wire_packets_per_frame();
            assert_eq!(parts, if i < 2 { 2 } else { 1 }, "rung {i} 的分包数不对");
            // 每个包装 5 ms 或 10 ms 的整数个样本；切不整齐的档不许存在。
            let samples_per_frame = f.rate_hz as usize / 100;
            assert_eq!(
                samples_per_frame % parts,
                0,
                "rung {i} 的帧切不成等长的两半"
            );
        }
    }

    /// **后半包的时间戳必须比前半包大 5000 µs。**
    ///
    /// 这一条守的是一个不会有任何报错的失效：两个包若共用同一个 `timestamp_us`，
    /// 后半包的 `transit` 差会退化成「两包间的发送间隔」（微秒级），于是**一半
    /// 的抖动样本近似 0**，p95 被系统性拉低 ⇒ AUTO 的降档判据（抖动 > 15 ms）
    /// 变迟钝，链路已经很糟了它还不降档。
    ///
    /// 注入对照：把 `tx_loop` 里的 `ts_us + (p as u64) * (FRAME_MS * 1000 / parts)`
    /// 改回 `ts_us`，这条立刻变红。
    #[test]
    fn the_second_half_carries_a_timestamp_five_milliseconds_later() {
        let ts_us = 1_000_000u64;
        for f in LADDER.iter() {
            let parts = f.wire_packets_per_frame_for(2);
            // **调生产代码的那个函数**，不是把同一行算术抄一遍。
            // 抄一遍的版本对 `tx_loop` 的改动完全免疫（实测：把生产代码里的
            // `+p*5000` 删掉，抄一遍的版本照样绿）。
            let stamps: Vec<u64> = (0..parts)
                .map(|p| split_timestamp_us(ts_us, p, parts))
                .collect();
            assert_eq!(stamps[0], ts_us, "前半包的时间戳被动过了");
            for p in 0..parts {
                assert_eq!(
                    stamps[p],
                    ts_us + p as u64 * 10_000 / parts as u64,
                    "stereo part {p}/{parts} timestamp is not frame-relative"
                );
            }
        }
        // 直接钉住这条不变量，免得将来 `LADDER` 里恰好没有分包档时这条测试
        // 退化成「什么都没测」。
        assert_eq!(split_timestamp_us(7_000, 0, 2), 7_000);
        assert_eq!(split_timestamp_us(7_000, 1, 2), 12_000, "后半包必须 +5 ms");
        assert_eq!(
            split_timestamp_us(7_000, 0, 1),
            7_000,
            "不分包时时间戳不许被动"
        );
    }

    fn timestamp_for_test_frame(media_frame_seq: u64) -> u64 {
        timestamp_for_test_packet(media_frame_seq, 0)
    }

    fn timestamp_for_test_packet(media_frame_seq: u64, part: usize) -> u64 {
        media_timestamp_with_frame_tag(
            1_000_000 + media_frame_seq * FRAME_MS * 1000,
            media_frame_seq,
            part,
        )
    }

    #[test]
    fn media_frame_tag_has_bounded_timing_error_and_uses_the_per_stream_counter() {
        for media_frame_seq in 0..512u64 {
            for parts in 1..=audiohub_net::media::MAX_WIRE_PARTS {
                for part in 0..parts {
                    let plain =
                        split_timestamp_us(1_000_003 + media_frame_seq * 10_000, part, parts);
                    let tagged = media_timestamp_with_frame_tag(plain, media_frame_seq, part);
                    assert!(plain.abs_diff(tagged) <= 128);
                    let decoded = media_frame_tag(tagged);
                    assert_eq!(
                        decoded.frame,
                        (media_frame_seq & MEDIA_FRAME_COUNTER_MASK) as u8
                    );
                    assert_eq!(decoded.part, part);
                }
            }
        }
        let tx = tests::fn_body("pub(crate) fn tx_loop(");
        assert!(
            tx.contains("timestamp_us: media_timestamp_with_frame_tag(")
                && tx.contains("split_timestamp_us(ts_us, p, parts)")
                && tx.contains("tx.media_frame_seq,")
                && tx.contains("p,")
                && tx
                    .matches("tx.media_frame_seq = tx.media_frame_seq.wrapping_add(1);")
                    .count()
                    == 2,
            "the media sender's tag is not tied one-for-one to per-stream raw frame advances"
        );
    }

    #[test]
    fn scheduler_tick_jump_without_raw_advance_does_not_change_the_timeline() {
        let old_format = rung_format(4);
        let new_format = rung_format(0);
        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let old = timeline
            .locate(
                100,
                media_timestamp_with_frame_tag(1_000_000, 17, 0),
                old_format,
                1,
            )
            .unwrap();

        // The wall-clock timestamp jumps by eight seconds, modelling tx_loop's
        // catch-up assignment to its global scheduler tick. This stream did not
        // consume raw sequence positions during the jump, so its own next media
        // frame is still exactly counter 18.
        let new = timeline
            .locate(
                107,
                media_timestamp_with_frame_tag(9_000_000, 18, 3),
                new_format,
                4,
            )
            .unwrap();
        assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(1));
        assert_eq!(new.part, 3);
        assert!(!new.transition.unwrap().frame_tag_fallback);
    }

    /// 接收侧从**实到样本数**认出半帧，而不是从格式表推。
    ///
    /// 一个不分包的对端发来整帧时照样能认出来；按表推会把它当半帧、
    /// 去等一个永远不来的搭档，表现是**每一帧都走半帧隐藏**（有声音，一半是编的）。
    #[test]
    fn packet_count_is_derived_from_format_and_channel_width() {
        for f in LADDER.iter() {
            for channels in [1, 2] {
                let full = f.rate_hz as usize / 100 * channels as usize;
                let parts = f.wire_packets_per_frame_for(channels);
                assert_eq!(
                    full % parts,
                    0,
                    "{f:?} x {channels}ch split is not frame/channel aligned"
                );
                assert!(parts <= audiohub_net::media::MAX_WIRE_PARTS);
            }
        }
    }

    #[test]
    fn fixed_format_timeline_matches_the_legacy_packet_division() {
        for format in LADDER {
            for channels in [1, 2] {
                let parts = format.wire_packets_per_frame_for(channels);
                let mut timeline = WireFormatTimeline::default();
                for wire_seq in 0..(parts as u32 * 8) {
                    let position = timeline
                        .locate(
                            wire_seq,
                            timestamp_for_test_frame(wire_seq as u64 / parts as u64),
                            format,
                            parts,
                        )
                        .expect("a current fixed-format packet was rejected");
                    assert_eq!(position.frame_seq, wire_seq / parts as u32);
                    assert_eq!(position.part, wire_seq as usize % parts);
                    assert!(position.transition.is_none());
                }
            }
        }
    }

    #[test]
    fn stereo_auto_crosses_a_real_two_part_to_one_part_boundary() {
        let rung_2 = rung_format(2);
        let rung_3 = rung_format(3);
        let rung_4 = rung_format(4);
        assert_eq!(rung_2.wire_packets_per_frame_for(2), 2);
        assert_eq!(rung_3.wire_packets_per_frame_for(2), 2);
        assert_eq!(rung_4.wire_packets_per_frame_for(2), 1);

        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let first = timeline
            .locate(100, timestamp_for_test_frame(50), rung_3, 2)
            .unwrap();
        let tail = timeline
            .locate(101, timestamp_for_test_packet(50, 1), rung_3, 2)
            .unwrap();
        assert_eq!((first.frame_seq, first.part), (50, 0));
        assert_eq!((tail.frame_seq, tail.part), (50, 1));

        let successor = timeline
            .locate(102, timestamp_for_test_frame(51), rung_4, 1)
            .unwrap();
        assert_eq!((successor.frame_seq, successor.part), (51, 0));
        assert_eq!(
            successor.transition,
            Some(WireFormatTransition {
                from: rung_3,
                from_parts: 2,
                to: rung_4,
                to_parts: 1,
                frame_tag_fallback: false,
            })
        );
        assert_eq!(
            timeline.locate(101, timestamp_for_test_packet(50, 1), rung_3, 2),
            None
        );
        assert_eq!(timeline.current.unwrap().format, rung_4);

        let following = timeline
            .locate(103, timestamp_for_test_frame(52), rung_4, 1)
            .unwrap();
        assert_eq!((following.frame_seq, following.part), (52, 0));
        assert!(following.transition.is_none());
    }

    #[test]
    fn an_old_same_format_epoch_cannot_reenter_after_a_round_trip() {
        let two_parts = rung_format(3);
        let one_part = rung_format(4);
        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);

        timeline
            .locate(200, timestamp_for_test_frame(100), two_parts, 2)
            .unwrap();
        timeline
            .locate(201, timestamp_for_test_packet(100, 1), two_parts, 2)
            .unwrap();
        assert!(timeline
            .locate(202, timestamp_for_test_frame(101), one_part, 1)
            .unwrap()
            .transition
            .is_some());
        // The sender aligns 203 up to 204 before returning to a two-part
        // format. The skipped raw sequence never represented an audio frame.
        let newest = timeline
            .locate(204, timestamp_for_test_frame(102), two_parts, 2)
            .unwrap();
        assert_eq!((newest.frame_seq, newest.part), (102, 0));
        assert!(newest.transition.is_some());

        // This packet has the same format metadata as the current epoch, but
        // its raw sequence is below the committed boundary and must stay dead.
        assert_eq!(
            timeline.locate(201, timestamp_for_test_packet(100, 1), two_parts, 2),
            None
        );
        assert_eq!(timeline.current.unwrap().wire_base, 204);
        let tail = timeline
            .locate(205, timestamp_for_test_packet(102, 1), two_parts, 2)
            .unwrap();
        assert_eq!((tail.frame_seq, tail.part), (102, 1));
        assert!(tail.transition.is_none());
    }

    #[test]
    fn alignment_padding_never_turns_into_audio_frame_loss() {
        let one_part = rung_format(4);
        for (new_format, new_parts) in [
            (rung_format(5), 1usize),
            (rung_format(3), 2usize),
            (rung_format(1), 3usize),
            (rung_format(0), 4usize),
        ] {
            assert_eq!(new_format.wire_packets_per_frame_for(2), new_parts);
            let unaligned_next = 101u32;
            let boundary = align_wire_seq(unaligned_next, new_parts as u32);
            for new_whole_lost in 0..=2u32 {
                let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
                let old = timeline
                    .locate(100, timestamp_for_test_frame(100), one_part, 1)
                    .unwrap();
                assert_eq!(old.frame_seq, 100);

                // Observe the last part of the first surviving new-format
                // frame before its head, after zero, one, or two whole new
                // frames were lost.
                let observed_base = boundary.wrapping_add(new_whole_lost * new_parts as u32);
                let first_seen = observed_base.wrapping_add(new_parts as u32 - 1);
                let observed_frame = 101 + new_whole_lost as u64;
                let new = timeline
                    .locate(
                        first_seen,
                        timestamp_for_test_packet(observed_frame, new_parts - 1),
                        new_format,
                        new_parts,
                    )
                    .unwrap();
                assert_eq!(
                    new.frame_seq,
                    old.frame_seq.wrapping_add(1 + new_whole_lost)
                );
                assert_eq!(new.part, new_parts - 1);
                assert!(new.transition.is_some());
                assert!(!new.transition.unwrap().frame_tag_fallback);

                let head = timeline
                    .locate(
                        observed_base,
                        timestamp_for_test_frame(observed_frame),
                        new_format,
                        new_parts,
                    )
                    .unwrap();
                assert_eq!(head.frame_seq, new.frame_seq);
                assert_eq!(head.part, 0);
                assert_eq!(
                    timeline.locate(100, timestamp_for_test_frame(100), one_part, 1),
                    None
                );
            }
        }
    }

    #[test]
    fn reverse_part_count_transitions_are_contiguous() {
        let one_part = rung_format(4);
        for (old_format, old_parts) in [
            (rung_format(3), 2usize),
            (rung_format(1), 3usize),
            (rung_format(0), 4usize),
        ] {
            let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
            let boundary = 120u32;
            let mut old = None;
            for part in 0..old_parts {
                old = timeline.locate(
                    boundary + part as u32,
                    timestamp_for_test_packet(60, part),
                    old_format,
                    old_parts,
                );
            }
            let old = old.unwrap();
            let new = timeline
                .locate(
                    boundary + old_parts as u32,
                    timestamp_for_test_frame(61),
                    one_part,
                    1,
                )
                .unwrap();
            assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(1));
            assert_eq!(new.part, 0);
            assert!(new.transition.is_some());
        }
    }

    #[test]
    fn tagged_boundary_preserves_a_missing_tail_and_a_whole_old_frame() {
        let old_format = rung_format(3);
        let new_format = rung_format(0);
        assert_eq!(old_format.wire_packets_per_frame_for(2), 2);
        assert_eq!(new_format.wire_packets_per_frame_for(2), 4);

        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let last_seen = timeline
            .locate(100, timestamp_for_test_frame(20), old_format, 2)
            .unwrap();
        assert_eq!((last_seen.frame_seq, last_seen.part), (50, 0));

        // Packet 101 (the tail of frame 50) and packets 102..=103 (all of
        // frame 51) are lost. Sequence 104 is already aligned for four parts;
        // observing part 3 first must still place the new frame at 52.
        let new_tail = timeline
            .locate(107, timestamp_for_test_packet(22, 3), new_format, 4)
            .unwrap();
        assert_eq!((new_tail.frame_seq, new_tail.part), (52, 3));
        assert!(new_tail.transition.is_some());
        let new_head = timeline
            .locate(104, timestamp_for_test_frame(22), new_format, 4)
            .unwrap();
        assert_eq!((new_head.frame_seq, new_head.part), (52, 0));

        // Reordered old packets cannot roll the committed epoch back.
        assert_eq!(
            timeline.locate(103, timestamp_for_test_packet(21, 1), old_format, 2),
            None
        );
    }

    #[test]
    fn tagged_tail_loss_is_exact_for_every_new_part_count() {
        let old_formats = [
            (rung_format(5), 1usize),
            (rung_format(3), 2usize),
            (rung_format(1), 3usize),
            (rung_format(0), 4usize),
        ];
        let new_formats = [
            (rung_format(4), 1usize),
            (rung_format(2), 2usize),
            (rung_format(1), 3usize),
            (rung_format(0), 4usize),
        ];
        for (old_format, old_parts) in old_formats {
            assert_eq!(old_format.wire_packets_per_frame_for(2), old_parts);
            for (new_format, new_parts) in new_formats {
                assert_eq!(new_format.wire_packets_per_frame_for(2), new_parts);
                if old_format == new_format {
                    continue;
                }
                for old_part in 0..old_parts as u32 {
                    for new_whole_lost in 0..=2u32 {
                        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
                        let old_wire_base = 120u32;
                        let old = timeline
                            .locate(
                                old_wire_base + old_part,
                                timestamp_for_test_packet(30, old_part as usize),
                                old_format,
                                old_parts,
                            )
                            .unwrap();

                        // Lose the rest of the observed old frame, one entire
                        // old-format frame, and zero to two complete new-format
                        // frames before observing the new epoch's last part.
                        let unaligned_new_base = old_wire_base + 2 * old_parts as u32;
                        let first_new_base = align_wire_seq(unaligned_new_base, new_parts as u32);
                        let new_wire_base =
                            first_new_base.wrapping_add(new_whole_lost * new_parts as u32);
                        let first_seen = new_wire_base.wrapping_add(new_parts as u32 - 1);
                        let logical_advance = 2 + new_whole_lost;
                        let new = timeline
                            .locate(
                                first_seen,
                                timestamp_for_test_packet(
                                    30 + logical_advance as u64,
                                    new_parts - 1,
                                ),
                                new_format,
                                new_parts,
                            )
                            .unwrap();
                        assert_eq!(
                            new.frame_seq,
                            old.frame_seq.wrapping_add(logical_advance),
                            "old parts {old_parts}, old part {old_part}, new parts {new_parts}, \
                             new whole lost {new_whole_lost}"
                        );
                        assert_eq!(new.part, new_parts - 1);
                        assert!(new.transition.is_some());
                        assert!(!new.transition.unwrap().frame_tag_fallback);
                    }
                }
            }
        }
    }

    #[test]
    fn a_legacy_sender_keeps_the_safe_ambiguous_tail_fallback() {
        let old_format = rung_format(3);
        let new_format = rung_format(0);
        let mut timeline = WireFormatTimeline::default();
        let old = timeline
            .locate(100, timestamp_for_test_frame(20), old_format, 2)
            .unwrap();
        let new = timeline
            .locate(107, timestamp_for_test_frame(22), new_format, 4)
            .unwrap();
        assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(1));
        assert_eq!(new.part, 3);
    }

    #[test]
    fn a_gap_beyond_the_exact_horizon_reanchors_adjacent_and_keeps_running() {
        let old_format = rung_format(4);
        let new_format = rung_format(0);
        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let old = timeline
            .locate(100, timestamp_for_test_frame(0), old_format, 1)
            .unwrap();

        // First new frame starts at 104, but 64 complete new-format frames are
        // lost. Logical advance 65 aliases to one in the six-bit counter; the
        // raw-span horizon gate must refuse that interpretation and collapse
        // the unretainable gap instead of rejecting the epoch forever.
        let observed_base = 104u32 + 64 * 4;
        let new = timeline
            .locate(
                observed_base + 3,
                timestamp_for_test_packet(65, 3),
                new_format,
                4,
            )
            .unwrap();
        assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(1));
        assert!(new.transition.unwrap().frame_tag_fallback);

        let following = timeline
            .locate(
                observed_base + 4,
                timestamp_for_test_frame(66),
                new_format,
                4,
            )
            .unwrap();
        assert_eq!(following.frame_seq, new.frame_seq.wrapping_add(1));
        assert!(following.transition.is_none());
    }

    #[test]
    fn the_sixty_four_frame_tag_value_is_exact_only_inside_the_raw_horizon() {
        let old_format = rung_format(4);
        let new_format = rung_format(5);
        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let old = timeline
            .locate(100, timestamp_for_test_frame(0), old_format, 1)
            .unwrap();
        let new = timeline
            .locate(164, timestamp_for_test_frame(64), new_format, 1)
            .unwrap();
        assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(64));
        assert!(!new.transition.unwrap().frame_tag_fallback);
    }

    #[test]
    fn tagged_alignment_transition_is_safe_across_raw_sequence_wrap() {
        let old_format = rung_format(4);
        let new_format = rung_format(0);
        let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
        let old = timeline
            .locate(u32::MAX - 2, timestamp_for_test_frame(40), old_format, 1)
            .unwrap();
        // The sender burns MAX-1 and MAX solely to align the four-part epoch
        // at zero. Part 3 may arrive first after the wrap.
        let new = timeline
            .locate(3, timestamp_for_test_packet(41, 3), new_format, 4)
            .unwrap();
        assert_eq!(new.frame_seq, old.frame_seq.wrapping_add(1));
        assert_eq!(new.part, 3);
        assert!(new.transition.is_some());
    }

    #[test]
    fn three_part_epoch_keeps_its_explicit_part_across_raw_sequence_wrap() {
        let old_format = rung_format(4);
        let new_format = rung_format(1);
        assert_eq!(new_format.wire_packets_per_frame_for(2), 3);
        for new_whole_lost in 0..=2u32 {
            let mut timeline = WireFormatTimeline::with_frame_tag_version(1);
            let old = timeline
                .locate(u32::MAX - 1, timestamp_for_test_frame(40), old_format, 1)
                .unwrap();
            let first_new_base = u32::MAX;
            let observed_base = first_new_base.wrapping_add(new_whole_lost * 3);
            let observed_tail = observed_base.wrapping_add(2);
            let new = timeline
                .locate(
                    observed_tail,
                    timestamp_for_test_packet(41 + new_whole_lost as u64, 2),
                    new_format,
                    3,
                )
                .unwrap();
            assert_eq!(
                new.frame_seq,
                old.frame_seq.wrapping_add(1 + new_whole_lost)
            );
            assert_eq!(new.part, 2);
            assert!(!new.transition.unwrap().frame_tag_fallback);
        }
    }

    #[test]
    fn real_loss_inside_an_epoch_keeps_its_frame_hole() {
        let format = rung_format(3);
        let mut timeline = WireFormatTimeline::default();
        let frame_0 = timeline
            .locate(0, timestamp_for_test_frame(0), format, 2)
            .unwrap();
        // Both packets of frame 1 are absent. The first packet of frame 2 must
        // remain frame 2 rather than being compressed next to frame 0.
        let frame_2 = timeline
            .locate(4, timestamp_for_test_frame(2), format, 2)
            .unwrap();
        assert_eq!(frame_0.frame_seq, 0);
        assert_eq!(frame_2.frame_seq, 2);
    }

    #[test]
    fn timeline_survives_raw_sequence_wrap_and_long_fixed_streams() {
        let format = rung_format(3);
        let mut wrapping = WireFormatTimeline::default();
        let before = wrapping
            .locate(u32::MAX - 1, timestamp_for_test_frame(0), format, 2)
            .unwrap();
        let tail = wrapping
            .locate(u32::MAX, timestamp_for_test_frame(0), format, 2)
            .unwrap();
        let after = wrapping
            .locate(0, timestamp_for_test_frame(1), format, 2)
            .unwrap();
        assert_eq!(before.frame_seq, u32::MAX / 2);
        assert_eq!(tail.frame_seq, before.frame_seq);
        assert_eq!(after.frame_seq, before.frame_seq.wrapping_add(1));

        let mut long = WireFormatTimeline::default();
        long.locate(0, timestamp_for_test_frame(0), format, 2)
            .unwrap();
        let far = long
            .locate(
                1 << 30,
                timestamp_for_test_frame((1u64 << 30) / 2),
                format,
                2,
            )
            .unwrap();
        assert_eq!(far.frame_seq, (1 << 30) / 2);
        assert!(long.current.unwrap().wire_base > 0);
    }

    #[test]
    fn a_format_boundary_does_not_rebuild_the_jitter_buffer() {
        let body = tests::fn_body("pub(crate) fn handle_datagram(");
        let boundary = body
            .find("st.wire_timeline.locate(")
            .expect("receive path does not consult the format timeline");
        let reassembly = body[boundary..]
            .find("collect_wire_chunk(")
            .map(|offset| boundary + offset)
            .expect("format timeline is not connected to reassembly");
        let commit = &body[boundary..reassembly];
        assert!(
            !commit.contains("st.jb =") && !commit.contains("JitterBuffer::"),
            "a wire-format boundary rebuilds the jitter buffer and reintroduces a playout blackout"
        );
        assert!(
            commit.contains("st.partial.clear();"),
            "mixed-format partial chunks survive an epoch boundary"
        );
    }

    fn partial_for_mask(frame_seq: u32, parts: usize, mask: usize) -> crate::PartialWireFrame {
        const CHANNELS: usize = 2;
        const FRAMES_PER_CHUNK: usize = 8;
        let mut chunks: [Option<Vec<f32>>; 4] = std::array::from_fn(|_| None);
        for (part, chunk) in chunks.iter_mut().enumerate().take(parts) {
            if mask & (1 << part) == 0 {
                continue;
            }
            *chunk = Some(
                (0..FRAMES_PER_CHUNK * CHANNELS)
                    .map(|scalar| frame_seq as f32 + part as f32 / 10.0 + scalar as f32 / 1000.0)
                    .collect(),
            );
        }
        crate::PartialWireFrame {
            frame_seq,
            parts,
            sample_rate: 48_000,
            channels: CHANNELS as u8,
            chunks,
        }
    }

    /// Every supported packet count and loss layout must make the same safety
    /// decision: preserve every received chunk in place when at least half the
    /// frame is real, otherwise leave a sequence hole for whole-frame PLC.
    #[test]
    fn partial_conceal_generalizes_to_every_two_three_and_four_part_layout() {
        for parts in 2usize..=audiohub_net::media::MAX_WIRE_PARTS {
            for mask in 0usize..(1 << parts) {
                let pending = partial_for_mask(17, parts, mask);
                let expected = pending.chunks.clone();
                let present = mask.count_ones() as usize;
                let finished = finalize_partial_wire_frame(pending);
                if present * 2 < parts {
                    assert!(
                        finished.is_none(),
                        "{present}/{parts} real chunks fabricated a complete frame"
                    );
                    continue;
                }

                let finished = finished.unwrap_or_else(|| {
                    panic!("{present}/{parts} real chunks should be safely concealable")
                });
                let chunk_len = expected.iter().flatten().next().unwrap().len();
                assert_eq!(finished.samples.len(), chunk_len * parts);
                assert_eq!(finished.partial_conceal, present != parts);
                assert!(finished.samples.iter().all(|sample| sample.is_finite()));
                for (part, expected) in expected.iter().enumerate().take(parts) {
                    if let Some(expected) = expected {
                        let actual = &finished.samples[part * chunk_len..(part + 1) * chunk_len];
                        assert_eq!(
                            actual, expected,
                            "received chunk {part}/{parts} was modified during concealment"
                        );
                    }
                }
            }
        }
    }

    fn test_chunk(frame_seq: u32, part: usize) -> Vec<f32> {
        (0..16)
            .map(|scalar| frame_seq as f32 + part as f32 / 10.0 + scalar as f32 / 1000.0)
            .collect()
    }

    fn collect_test_chunk(
        assembly: &mut WireFrameAssembly,
        frame_seq: u32,
        part: usize,
        parts: usize,
    ) -> ReassembledWireBatch {
        collect_wire_chunk(
            assembly,
            frame_seq,
            part,
            parts,
            48_000,
            2,
            test_chunk(frame_seq, part),
        )
    }

    fn take_batch(batch: ReassembledWireBatch) -> Vec<ReassembledWireFrame> {
        batch.frames.into_iter().flatten().collect()
    }

    /// N+1 part 0 is ordinary cross-frame reorder, not proof that N is lost.
    /// N must remain live and complete without concealment when its tail arrives.
    #[test]
    fn next_frame_head_does_not_conceal_a_reordered_current_frame_tail() {
        let mut assembly = WireFrameAssembly::default();
        for part in [0usize, 1] {
            assert_eq!(collect_test_chunk(&mut assembly, 10, part, 4).len, 0);
        }
        assert_eq!(collect_test_chunk(&mut assembly, 11, 0, 4).len, 0);
        assert_eq!(collect_test_chunk(&mut assembly, 10, 2, 4).len, 0);

        let frames = take_batch(collect_test_chunk(&mut assembly, 10, 3, 4));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_seq, 10);
        assert!(!frames[0].partial_conceal);
        for part in 0..4 {
            assert_eq!(
                &frames[0].samples[part * 16..(part + 1) * 16],
                test_chunk(10, part).as_slice()
            );
        }
        assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 11);
        assert!(assembly.newer.is_none());

        // Once N has been delivered, a delayed duplicate cannot resurrect it
        // ahead of the still-live N+1 slot.
        assert_eq!(collect_test_chunk(&mut assembly, 10, 2, 4).len, 0);
        assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 11);
        assert_eq!(assembly.retired_through, Some(10));
    }

    /// N is not concealed merely because N+1 exists. N+2 is the explicit
    /// boundary that retires N when its missing tail never arrives.
    #[test]
    fn n_plus_two_expires_an_incomplete_n_and_preserves_both_newer_slots() {
        let mut assembly = WireFrameAssembly::default();
        for part in [0usize, 1] {
            assert_eq!(collect_test_chunk(&mut assembly, 20, part, 4).len, 0);
        }
        assert_eq!(collect_test_chunk(&mut assembly, 21, 0, 4).len, 0);

        let frames = take_batch(collect_test_chunk(&mut assembly, 22, 0, 4));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_seq, 20);
        assert!(frames[0].partial_conceal);
        assert_eq!(&frames[0].samples[..16], test_chunk(20, 0).as_slice());
        assert_eq!(&frames[0].samples[16..32], test_chunk(20, 1).as_slice());
        assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 21);
        assert_eq!(assembly.newer.as_ref().unwrap().frame_seq, 22);

        assert_eq!(collect_test_chunk(&mut assembly, 20, 3, 4).len, 0);
        assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 21);
        assert_eq!(assembly.newer.as_ref().unwrap().frame_seq, 22);
    }

    /// A complete N+1 must wait behind incomplete N. The chunk that finally
    /// completes N releases both frames in sequence order, without concealment.
    #[test]
    fn two_complete_adjacent_frames_release_in_order_after_reverse_completion() {
        let mut assembly = WireFrameAssembly::default();
        assert_eq!(collect_test_chunk(&mut assembly, 30, 0, 4).len, 0);
        for part in 0..4 {
            assert_eq!(collect_test_chunk(&mut assembly, 31, part, 4).len, 0);
        }
        for part in [1usize, 2] {
            assert_eq!(collect_test_chunk(&mut assembly, 30, part, 4).len, 0);
        }

        let frames = take_batch(collect_test_chunk(&mut assembly, 30, 3, 4));
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.frame_seq)
                .collect::<Vec<_>>(),
            vec![30, 31]
        );
        assert!(frames.iter().all(|frame| !frame.partial_conceal));
        assert!(assembly.older.is_none());
        assert!(assembly.newer.is_none());
        assert_eq!(assembly.retired_through, Some(31));
    }

    /// On an established stream, every packet of N+1 may beat every packet of
    /// N. The retired N-1 anchor creates an empty N slot, so even this fully
    /// reversed pair is held and released in timeline order.
    #[test]
    fn two_whole_frames_arriving_in_reverse_order_use_the_retired_anchor() {
        let mut assembly = WireFrameAssembly {
            older: None,
            newer: None,
            retired_through: Some(49),
        };
        for part in 0..4 {
            assert_eq!(collect_test_chunk(&mut assembly, 51, part, 4).len, 0);
        }
        assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 50);
        assert_eq!(assembly.newer.as_ref().unwrap().frame_seq, 51);
        assert!(wire_frame_complete(assembly.newer.as_ref().unwrap()));

        for part in 0..3 {
            assert_eq!(collect_test_chunk(&mut assembly, 50, part, 4).len, 0);
        }
        let frames = take_batch(collect_test_chunk(&mut assembly, 50, 3, 4));
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.frame_seq)
                .collect::<Vec<_>>(),
            vec![50, 51]
        );
        assert!(frames.iter().all(|frame| !frame.partial_conceal));
        assert_eq!(assembly.retired_through, Some(51));
    }

    /// The N+2 boundary uses the same at-least-half rule for every supported
    /// packet count and preserves every real chunk in its original position.
    #[test]
    fn assembly_expiry_obeys_every_two_three_and_four_part_mask() {
        for parts in 2usize..=audiohub_net::media::MAX_WIRE_PARTS {
            for mask in 0usize..((1 << parts) - 1) {
                let mut assembly = WireFrameAssembly {
                    older: Some(partial_for_mask(40, parts, mask)),
                    newer: None,
                    retired_through: Some(39),
                };
                assert_eq!(collect_test_chunk(&mut assembly, 41, 0, parts).len, 0);
                let frames = take_batch(collect_test_chunk(&mut assembly, 42, 0, parts));
                let present = mask.count_ones() as usize;
                if present * 2 < parts {
                    assert!(
                        frames.is_empty(),
                        "{present}/{parts} real chunks fabricated an expired frame"
                    );
                } else {
                    assert_eq!(frames.len(), 1, "mask {mask:#b} parts {parts}");
                    let frame = &frames[0];
                    assert_eq!(frame.frame_seq, 40);
                    assert!(frame.partial_conceal);
                    for part in 0..parts {
                        if mask & (1 << part) != 0 {
                            assert_eq!(
                                &frame.samples[part * 16..(part + 1) * 16],
                                test_chunk(40, part).as_slice(),
                                "real chunk {part}/{parts} moved for mask {mask:#b}"
                            );
                        }
                    }
                }
                assert_eq!(assembly.older.as_ref().unwrap().frame_seq, 41);
                assert_eq!(assembly.newer.as_ref().unwrap().frame_seq, 42);
            }
        }
    }

    /// The public lifetime counter is the only way the quality pipeline can
    /// see concealment that produces a complete-length frame above the JB.
    #[test]
    fn production_accounts_for_every_delivered_partial_frame() {
        let body = tests::fn_body("pub(crate) fn handle_datagram(");
        let collect = body
            .find("collect_wire_chunk(")
            .expect("the production reassembler is not called");
        let account = body
            .find("st.half_conceal = st.half_conceal.saturating_add(1);")
            .expect("partial concealment is invisible to lifetime telemetry");
        assert!(
            collect < account,
            "telemetry runs before reassembly has a result"
        );
    }

    /// 一整趟：**编码 → 切两半 → 各自解码 → 拼回来**，必须与不分包的整帧一致。
    ///
    /// 这条把 `dsp` 的编解码与 `tx_loop` 的切分对起来。注入对照：把切分改成
    /// 按**字节**对半切（而不是按样本），s24 档立刻红——3 字节/样本时字节数
    /// 是奇数个样本宽，切在样本中间会把整条流错位一个字节。
    #[test]
    fn splitting_and_reassembling_a_frame_is_bit_identical_to_not_splitting() {
        for f in LADDER.iter() {
            let n = f.rate_hz as usize / 100;
            let samples: Vec<f32> = (0..n)
                .map(|i| ((i as f32 / n as f32) * 2.0 - 1.0) * 0.9)
                .collect();
            let whole = dsp::decode_pcm(&dsp::encode_pcm(&samples, f.depth), f.depth);

            let parts = f.wire_packets_per_frame();
            let chunk = samples.len() / parts;
            let mut rebuilt: Vec<f32> = Vec::with_capacity(n);
            for p in 0..parts {
                let lo = p * chunk;
                let hi = if p + 1 == parts {
                    samples.len()
                } else {
                    lo + chunk
                };
                let bytes = dsp::encode_pcm(&samples[lo..hi], f.depth);
                assert_eq!(
                    bytes.len(),
                    (hi - lo) * f.depth.bytes_per_sample(),
                    "{f:?} 的半包字节数不是整数个样本"
                );
                rebuilt.extend(dsp::decode_pcm(&bytes, f.depth));
            }
            assert_eq!(rebuilt.len(), whole.len(), "{f:?} 拼回来的样本数变了");
            for (i, (a, b)) in whole.iter().zip(rebuilt.iter()).enumerate() {
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "{f:?} 第 {i} 个样本分包前后不一致"
                );
            }
        }
    }

    /// **每一格的 `codec` 与 `sample_rate` 都由格号唯一决定**，且两个 48 kHz 的
    /// 深档在包头上只靠 `codec` 区分。
    ///
    /// 注入对照：把 `tx_loop` 的 `Codec::for_depth(fmt.depth)` 改回
    /// `Codec::PcmS16le`，这条红在「rung 0/1 的 codec 变成了 s16」——而在
    /// 生产上那次改动的表现是：选了 24 bit，线上发的是把 24 位字节当 16 位解的
    /// 垃圾，包头写着 s16，遥测据此报 s16，**处处自洽，全都是错的**。
    #[test]
    fn the_header_fields_are_a_function_of_the_rung_alone() {
        let mut seen = Vec::new();
        for i in 0..LADDER.len() as u32 {
            let f = rung_format(i);
            let codec = Codec::for_depth(f.depth);
            seen.push((f.rate_hz, codec as u8));
            assert_eq!(codec.wire_depth(), Some(f.depth));
        }
        // 三个 48 kHz 的档采样率相同，必须靠 codec 分开。
        let at48: Vec<u8> = seen
            .iter()
            .filter(|(r, _)| *r == 48_000)
            .map(|(_, c)| *c)
            .collect();
        assert_eq!(at48.len(), 3, "48 kHz 应当有三档");
        let mut uniq = at48.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            3,
            "三个 48 kHz 档的 codec 撞在了一起：收方分不出位深"
        );
        assert_eq!(
            rung_format(2),
            WireFormat {
                rate_hz: 48_000,
                depth: WireDepth::S16
            },
            "AUTO 天花板那一档的格式变了：所有 AUTO 用户的线上格式会跟着变"
        );
    }
}
