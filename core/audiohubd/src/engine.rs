//! Media plane wiring: single shared UDP socket, 10ms send scheduler with
//! fan-out + AUTO resample-before-encode, receive/decrypt into jitter buffers,
//! 10ms mixer with soft clip and a 2s post-mix ring for mix_verdicts.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audiohub_core::audio::{self, AudioTx, LiveCapture, LivePlayback};
use audiohub_core::dsp::{self, LinearResampler, ToneVerdict};
use audiohub_core::latency::{DropMode, SourceDepths, StageDepth, StageId, StageSlot, NO_DEPTHS};
use audiohub_core::sysaudio::{self, SysAudioCapture};
use audiohub_net::media::{FrameSource, LossInjector, MediaCrypto, MicSource, ToneSource};
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
#[cfg(target_os = "macos")]
pub(crate) fn raise_audio_thread_qos(what: &str) {
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
pub(crate) fn raise_audio_thread_qos(_what: &str) {
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
const SEND_SLOTS: usize = 128;

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
    raise_audio_thread_qos("udp_send_loop");
    let _ = inner.media_send.thread.set(std::thread::current());
    loop {
        while inner.media_send.q.consume(|slot| {
            let owner = slot.owner.take(); // 在**本线程**析构
            if inner.udp.send_to(&slot.buf, slot.dest).is_ok() {
                if let Some(o) = owner {
                    o.sent_packets.fetch_add(1, Ordering::Relaxed);
                    o.sent_bytes.fetch_add(slot.buf.len() as u64, Ordering::Relaxed);
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
        /// Where this stream's media goes: a UDP destination (tier 0) or this
        /// peer's media TCP link (tier 1). Read from `ConnShared` once, when
        /// the stream is created — see `ConnShared::current_media_path`.
        path: MediaPath,
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
    /// This stream's media path, taken at creation and never changed
    /// afterwards. On [`MediaPath::Udp`] `dest_override` may still move the
    /// port (see `refresh_dest`); on [`MediaPath::Tcp`] there is no address to
    /// move, which is the point of the enum.
    path: MediaPath,
    spec: SourceSpec,
    loss: LossInjector,
    seq: u32,
    rung: u32,
    rs: Option<LinearResampler>, // 48k -> rung rate, recreated on rung switch
    rs_last: f32,                // last source sample; seeds the next resampler
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
    shared: Arc<TxShared>,
}

struct SourceEnt {
    src: Src,
    refs: usize,
    /// 造出这个源的那次建源请求的代号。收尸时要原样带回去，好让建源线程
    /// 精确丢掉**配套的**那个 `LiveCapture`（设备变更重建期间，同一个 spec
    /// 会短暂存在新旧两份）。
    gen: u64,
    frame: Vec<f32>, // one 48k frame per tick, broadcast to all attached streams
    /// 本 tick 读到的各级深度，随 `frame` 一起广播给挂在这个源上的每条流。
    /// 读一次、发 N 份：物理队列只有一份（规格 §7.2 R8）。
    depths: SourceDepths,
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
pub(crate) struct SysAudioFrames {
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
    Retire { spec: SourceSpec, gen: u64, src: Src },
}

/// 建源线程的产出。
pub(crate) struct BuildDone {
    pub spec: SourceSpec,
    pub gen: u64,
    pub result: std::result::Result<Src, String>,
}

/// 建源 / 收尸线程。**这是进程里唯一允许开关音频设备的地方**（混音线程的
/// `apply_mixcmd` 是另一处，理由同样是 cpal 流不能跨线程）。
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
            let (src, cap) = MicSource::open(FRAME_MS as u32).context("start microphone capture")?;
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
            Src::Frame(Box::new(crate::halbridge::HalSpeakerSource::new(&hal, *slot)))
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
fn resampler_for(rung: u32, last: f32) -> Option<LinearResampler> {
    let f = audiohub_net::media::rung_format(rung);
    (f.rate_hz != MicSource::OUT_RATE)
        .then(|| seeded_resampler(MicSource::OUT_RATE, f.rate_hz, last))
}

fn seeded_resampler(src_rate: u32, dst_rate: u32, last: f32) -> LinearResampler {
    let mut rs = LinearResampler::new(src_rate, dst_rate);
    let mut discard = Vec::new();
    rs.process(&[last], &mut discard); // primes `last`; output is not audio
    rs
}

/// 把 JB 的目标深度**精确**设到 `want` 帧，不重建、不分配。
///
/// # 为什么是 `update_target` 的逆运算而不是一个 setter
///
/// `JitterBuffer` 没有 `set_target`。它只有
/// `update_target(jitter_p95_ms, frame_ms)`，公式是
/// `target = clamp(ceil(p95 / frame) + 1, min, max)`。
/// 代入 `p95 = (want − 1) × frame` 得 `ceil(want − 1) + 1 = want`——
/// **精确相等，不是近似**（`want ≥ 1` 时 `(want−1)×frame` 恰是 frame 的整数倍，
/// `ceil` 是恒等）。
///
/// 这条路是刻意选的：`core/audiohub-net/src/media.rs` 归另一条线在改，
/// 本轮不动它。合并时若那边加了 `set_target_frames(u32)`，这里换过去即可，
/// 语义完全一致——见文件末 `TODO(merge)`。
///
/// 越出包络时由 `update_target` 自己夹住，与伺服侧的夹逻辑同一个 `[min, max]`，
/// 所以两边不会打架。
fn steer_jitter_target(jb: &mut audiohub_net::media::JitterBuffer, want: u32) {
    let frame_ms = FRAME_MS as f64;
    let synthetic_p95 = want.max(1).saturating_sub(1) as f64 * frame_ms;
    jb.update_target(synthetic_p95, frame_ms);
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
        crate::tcpmedia::MediaPath::Tcp(_) => audiohub_net::media::JbTuning::degraded_from_env(),
    }
}

fn reshape_jitter_envelope(
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
    st.jb = JitterBuffer::with_tuning(seed.clamp(cfg.min_target, cfg.max_target), cfg);
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
        self.pending.insert(spec.clone(), PendingBuild { gen, waiters: Vec::new() });
        let _ = self.builder.send(BuildReq::Build { spec, gen });
        gen
    }

    fn install_stream(&mut self, spec: &SourceSpec, add: PendingAdd) {
        // 钳位与 `tx_loop` 那处同一条理由：加档而不改钳位 = 新档静默不可达。
        let start_rung = add
            .shared
            .rung
            .load(Ordering::Relaxed)
            .min(audiohub_net::media::LADDER.len() as u32 - 1);
        self.streams.insert(
            add.stream_id,
            TxStream {
                id: add.stream_id,
                // real streams are always keyed per stream, never with
                // the bare connection media key
                crypto: MediaCrypto::new_for_stream(&add.key, add.stream_id, &add.salt),
                path: add.path,
                spec: spec.clone(),
                loss: LossInjector::new(add.stream_id, add.loss_pct),
                seq: 0,
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
                rs: resampler_for(start_rung, 0.0),
                rs_last: 0.0,
                // 一帧最深档 = 480 × 4 B（f32）。容量随格号变（换档同时改帧长度
                // 与每样本字节数），按最深档预留就不会在音频线程上扩容。
                pay: Vec::with_capacity(F48 * 4),
                // 0 = 「还没看过」。`dest_override` 在这条流建起来**之前**就被
                // 学到过的情形因此不会漏：那时代号已经 ≥1，第一 tick 就会去读。
                dest_epoch_seen: 0,
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
        // ① 源已经在表里 ⇒ 这是一次设备变更重建。**换芯**：新的先造好、这一刻
        // 才丢老的，与搬家前 `rebuild_mic_source` 的顺序保证逐字相同。
        if let Some(ent) = self.sources.get_mut(&d.spec) {
            let old_src = std::mem::replace(&mut ent.src, src);
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
                refs: waiters.len(),
                gen: d.gen,
                frame: Vec::new(),
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
        TxCmd::Add { stream_id, key, salt, path, spec, loss_pct, shared, ack } => {
            let add = PendingAdd { stream_id, key, salt, path, loss_pct, shared, ack };
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
            st.pending.insert(spec.clone(), PendingBuild { gen, waiters: vec![add] });
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
    let MediaPath::Udp(dest) = &mut tx.path else { return };
    let epoch = tx.shared.dest_epoch.load(Ordering::Acquire);
    if epoch == tx.dest_epoch_seen {
        return;
    }
    tx.dest_epoch_seen = epoch;
    if let Some(a) = *lk(&tx.shared.dest_override) {
        if a != *dest {
            dlog!("[audiohubd] stream {} dest {} -> {} (keepalive)", tx.id, dest, a);
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
    raise_audio_thread_qos("tx_loop");
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
        let TxState { streams, sources, .. } = &mut st;
        let mut queued_any = false;
        // 见下面用到它的那处注释：循环级的重采样暂存。
        staged.clear();
        for tx in streams.values_mut() {
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
                tx.rs = resampler_for(want, last);
                // **把 seq 对齐到新的分包数。** 接收侧用 `seq / parts` 还原帧
                // 序号，并靠 `seq % 2` 分前后半；换到深档时若 seq 是奇数，
                // 整条流的前后半会**永久错位**（每一帧都配不上对），表现是
                // 持续的半帧隐藏 —— 有声音，但一半是编的。
                //
                // 代价：至多跳过一个 seq（接收侧记 1 个丢包）。只在
                // 「不分包 → 分包」这一个方向上、且 seq 为奇数时才发生。
                let parts = f.wire_packets_per_frame() as u32;
                let rem = tx.seq % parts;
                if rem != 0 {
                    tx.seq = tx.seq.wrapping_add(parts - rem);
                }
            }
            tx.rs_last = ent.frame.last().copied().unwrap_or(tx.rs_last);
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
                    rs.process(&ent.frame, &mut staged);
                    &staged
                }
                None => &ent.frame,
            };
            // 线上一帧拆成几个数据报。深档（48k/24、48k/32f）的整帧明文超过
            // 一个以太网数据报装得下的量，按 **5 ms** 切成两个包发。
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
            let parts = fmt.wire_packets_per_frame();
            let dropped = tx.loss.should_drop(); // advance LCG every frame
            if dropped {
                // 丢的是**整帧**：`seq` 照样按实际会发的包数推进，否则接收侧的
                // 期望序号会与发送侧错位，丢包率算出来是假的。
                tx.seq = tx.seq.wrapping_add(parts as u32);
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
                let hi = if p + 1 == parts { samples.len() } else { lo + chunk };
                // 载荷写进本流长期复用的缓冲，不再每 tick 造一个 `Vec`。
                dsp::encode_pcm_into(&samples[lo..hi], fmt.depth, &mut tx.pay);
                let header = Header {
                    kind: Kind::Media,
                    codec: Codec::for_depth(fmt.depth),
                    channels: 1,
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
                    timestamp_us: split_timestamp_us(ts_us, p, parts),
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
                        inner.media_send.enqueue(*dest, &tx.shared, tx.pay.len(), seal)
                    }
                    // `tick_at` and not a fresh `Instant::now()` per packet: the
                    // stale gate measures how long a frame waited in OUR queue,
                    // and both halves of a split frame waited the same amount.
                    MediaPath::Tcp(link) => {
                        tcp_link = Some(link);
                        link.enqueue(tick_at, &tx.shared, tx.pay.len(), seal)
                    }
                };
                if queued {
                    queued_any = true;
                }
            }
            if let Some(l) = tcp_link {
                l.wake();
            }
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

pub(crate) fn rx_loop(inner: Arc<DaemonInner>) {
    const _: () = assert!(
        RECV_BUF_BYTES >= DEEPEST_SEALED_FRAME_BYTES,
        "收流缓冲装不下最深档的整帧：mac 上表现为 tampered，Windows 上表现为收流每包睡 100 ms"
    );
    let mut buf = [0u8; RECV_BUF_BYTES];
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

/// 一帧被切成 `parts` 个包时，第 `p` 个包该带的时间戳。
///
/// ⚠ **后半包必须是 `ts + 5000 µs`，不能与前半包共用同一个时间戳。**
/// 抖动是 `|transit − prev_transit|`；两个包若共用同一个 `timestamp_us`，
/// 后半包的 transit 差会退化成「两包间的发送间隔」（微秒级），于是**一半的
/// 抖动样本近似 0**，p95 被系统性拉低 ⇒ AUTO 的降档判据（抖动 > 15 ms）变迟钝，
/// 链路已经很糟了它还不降档。
///
/// 单独提成函数是为了让守门测试**调用它**而不是把同一行算术抄一遍——
/// 抄一遍的测试对生产代码的改动完全免疫（本项目栽过的「测试是戏剧」）。
pub(crate) fn split_timestamp_us(frame_ts_us: u64, part: usize, parts: usize) -> u64 {
    frame_ts_us + (part as u64) * (FRAME_MS * 1000 / parts.max(1) as u64)
}

/// 深档丢了半帧时，把在手的那一半补成整帧。
///
/// `held_is_second` = 在手的是**后**半。补法是**上一段真实音频的衰减重复**——
/// 与 `JitterBuffer::conceal` 同一条原语，只是作用在 240 个样本上而不是 480 个。
///
/// # 为什么不干脆丢掉整帧走 JB 的 PLC
///
/// 半帧隐藏正是「5 ms 分包」相对「让 IP 去分片」的核心收益：分片下任一片丢失
/// 整帧作废，而这里**一半的真实音频保住了**，期望隐藏音频减半。
/// 代价是 JB 看到的是一个「完整」帧、不会记 PLC ⇒ 调用方**必须**同时递增
/// `JbState::half_conceal`，否则这条降级在 Q1 上完全不可见。那个计数器经
/// `JbState::counts()` 以 **0.5 帧**的权重进 `quality::conceal_ratio`
/// （一次半帧隐藏正好伪造 10 ms 里的 5 ms），并单独上报为
/// `SessionStats::jb_half_conceal`。
///
/// # 为什么不等搭档
///
/// 等待要么设定时器（凭空多出一级缓冲），要么把这一拍拖到下一拍
/// （直接顶穿延迟目标）。这条管线的纪律是不许为了平滑多留任何一帧：
/// 搭档没来就是没来，下一帧的包一到，上一帧的残片立刻作废。
fn conceal_missing_half(held: &[f32], held_is_second: bool, full: usize) -> Vec<f32> {
    let missing = full.saturating_sub(held.len());
    let mut out = Vec::with_capacity(full);
    // 衰减系数与 `JitterBuffer` 的 PLC 同量级：一段 5 ms 的重复，线性淡出到 0。
    let fade = |i: usize| 1.0 - (i as f32 + 1.0) / (missing.max(1) as f32 + 1.0);
    if held_is_second {
        // 缺的是**前**半：用后半的内容反向淡入，让它接到缺口上。
        // （拿不到「上一帧的尾巴」——那住在 JB 里，这条路径上没有它。）
        for i in 0..missing {
            let s = held.get(i % held.len().max(1)).copied().unwrap_or(0.0);
            out.push(s * (1.0 - fade(i)));
        }
        out.extend_from_slice(held);
    } else {
        out.extend_from_slice(held);
        for i in 0..missing {
            let s = held.get(i % held.len().max(1)).copied().unwrap_or(0.0);
            out.push(s * fade(i));
        }
    }
    out.truncate(full);
    while out.len() < full {
        out.push(0.0);
    }
    out
}

pub(crate) fn handle_datagram(inner: &DaemonInner, dg: &[u8], from: SocketAddr) {
    let Ok((h, _payload)) = Header::parse(dg) else { return };
    match h.kind {
        Kind::Media => {
            let rx = rd(&inner.rx_table).get(&h.stream_id).cloned();
            let Some(rx) = rx else { return };
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
                dlog!("[audiohubd] stream {} 收到非 PCM codec {:?}，丢弃", h.stream_id, h.codec);
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
            // ---- 深档的 5 ms 分包：把两个半帧拼回一帧 ------------------------
            //
            // 判据用**实到样本数**而不是格式表：一个不分包的对端发来整帧时
            // 照样能认出来，而按表推会把它当半帧去等一个永远不来的搭档。
            let full = (h.sample_rate as usize / 100).max(1); // 10 ms @ 线上速率
            let parts = if !decoded.is_empty() && decoded.len() * 2 == full { 2 } else { 1 };

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
            if decoded.len() * parts != full {
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
                     codec {:?} @ {} Hz，载荷 {} B 解出 {} 样本，本帧应为 {} 样本；丢弃",
                    h.stream_id,
                    h.codec,
                    h.sample_rate,
                    plain.len(),
                    decoded.len(),
                    full,
                );
                return;
            }

            let mut st = lk(&rx.jbs);

            // `frame_seq` 是**帧**序号（JB 的 seq 必须逐帧 +1，它靠这个判洞）；
            // `h.seq` 是**包**序号。分包数一变，换算基准就变了 ⇒ 帧序号会跳，
            // 而 JB 的 `next_seq` 会永久停在那个跳过去的洞上（要靠 late_streak
            // 熬 50 个包 ≈ 500 ms 静音才自愈）。所以这里**主动干净重建**。
            //
            // ⚠ 这一步的代价（重新预缓冲，约一个 JB 深度）只落在**跨越分包边界**
            // 的换档上，也就是 rung 2 ↔ rung 1。而 AUTO 的天花板正是 rung 2
            // ⇒ **AUTO 自己永远不会触发它**；只有用户手动把滑条拖过
            // 「48 kHz·16 bit ↔ 48 kHz·24 bit」这条线时才会听到一次，
            // 那时用户正在主动改音质。AUTO 内部的升降（rung 2..5）一如既往无缝。
            if st.wire_parts != parts {
                let was = st.wire_parts;
                st.wire_parts = parts;
                st.half = None;
                if was != 0 {
                    let target = st.jb.target();
                    let tuning = st.jb.tuning();
                    st.jb = audiohub_net::media::JitterBuffer::with_tuning(target, tuning);
                    st.last_dropped = 0;
                    st.late_streak = 0;
                    // 五个 lifetime 计数器随新 JB 归零，这是一次真实的不连续。
                    st.conceal.reset();
                    dlog!(
                        "[audiohubd] stream {} 线上分包 {was} -> {parts}，JB 重建",
                        h.stream_id
                    );
                }
            }
            let frame_seq = h.seq / parts as u32;
            let raw: Vec<f32> = if parts == 1 {
                decoded
            } else {
                let second = h.seq % 2 == 1;
                match st.half.take() {
                    // 搭档到了：拼成整帧。乱序到达（后半先来）也能拼，
                    // 因为配对判据是**帧序号**，不是到达顺序。
                    Some((pseq, psecond, mut held)) if pseq == frame_seq && psecond != second => {
                        if second {
                            held.extend_from_slice(&decoded);
                            held
                        } else {
                            let mut out = decoded;
                            out.extend_from_slice(&held);
                            out
                        }
                    }
                    // 搭档没来（或来的是**上一帧**的残片）：立刻按半帧隐藏交付
                    // 那一帧，**不等**。等待会把延迟目标顶穿。
                    other => {
                        if let Some((pseq, psecond, held)) = other {
                            let filled = conceal_missing_half(&held, psecond, full);
                            st.half_conceal += 1;
                            st.jb.push(pseq, filled);
                        }
                        st.half = Some((frame_seq, second, decoded));
                        // 这一半先攒着，本包到此为止（下面的抖动/目标维护照旧走）。
                        Vec::new()
                    }
                }
            };
            let last_sample = raw.last().copied();
            let frame = if raw.is_empty() {
                // 只到了半帧，本拍没有可交付的整帧。
                Vec::new()
            } else if h.sample_rate == 48000 {
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
            // 空帧不入 JB：`push` 会把 `frame_len` 当成 0 之外还占一个 seq，
            // 于是那一帧对 JB 来说「到了、但是空的」——比没到还坏。
            if !frame.is_empty() {
                st.jb.push(frame_seq, frame.clone());
            }
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
                st.jb = audiohub_net::media::JitterBuffer::with_tuning(target, st.jb.tuning());
                if !frame.is_empty() {
                    st.jb.push(frame_seq, frame);
                }
                st.half = None;
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
            // ---- 谁来决定 JB 的目标深度：伺服，还是抖动公式 ----
            //
            // 固定延迟档下**必须**是伺服，而且抖动公式必须彻底闭嘴。两个都写，
            // 就是两条回路抢同一个水位：用户选的 200 ms 会在每一次 p95 更新时
            // 被改回抖动算出来的那个数，而界面照旧显示 200——「设置生效了」
            // 的错觉，正是本项目栽过五次的形态。
            let servo_want = rx.transport.servo_frames();
            if st.pushes % 100 == 0 {
                // 包络（min/max_target）只能在构造时给定。用户把目标从 100 ms
                // 拖到 1000 ms 时，默认包络 4..12 帧 = 40..120 ms 根本够不着，
                // 于是必须重建。每秒问一次、已经对了就立刻返回。
                let target = rx.transport.latency_target();
                let reseeded =
                    reshape_jitter_envelope(&mut st, target, jb_tuning_for(&rx.ka_path), h.stream_id);
                if reseeded {
                    // **把旧的伺服输出一并作废。**
                    //
                    // 下面那条 `Some(_) if reseeded => {}` 的本意是「让伺服下一拍
                    // 重新算」，但它只跳过**这一拍**——而这一拍与下一拍相隔 100 个
                    // 包（≈1 s），伺服未必在这中间跑过。伺服没跑过时下一拍读到的
                    // 还是那个**在旧包络下算出来的**旧值，于是刚落好的预置
                    // （300 ms ⇒ 30 帧）会被一个 2、3 帧的旧值立刻推翻，
                    // 之后只能靠伺服每拍 +1 帧地爬回去（30 帧要爬近 30 秒）。
                    //
                    // 这是一个**相位**决定输赢的竞态：伺服那一拍恰好落在重建之前
                    // 还是之后，结论完全相反，而两条路径都不报错。清掉旧值把它
                    // 从「看运气」变成「看得见的空缺」。
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
                            v.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
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
    let Some(dest) = rx.ka_path.udp_dest() else { return };
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
    let _ = inner.udp.send_to(&h.encode(&[]), dest);
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
    // 与 `tx_loop` 同一条理由：这也是一条 10 ms 截止期线程，`play_ring` 的
    // 5 ms `margin` 买的正是它的唤醒过冲，而一次阻塞 `write` 就能吃光它。
    rtlog::arm("mixer_loop");
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
        // 唤醒过冲直方图（见 [`LateCell`] 与 [`sleep_until`]）。**测点必须在等待
        // 之后**：`margin` 买的是「醒来晚了多少」，不是「整 tick 丢没丢」。
        MIX_LATE.record(sleep_until(deadline));
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
                let Some(depth) = h.mic_depth(slot as u8) else { continue };
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
pub(crate) mod tests {
    use super::*;

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
        assert_eq!(tail(LATE_BUCKETS - 1), 1, ">100 ms 的桶应当只有 250 ms 那一发");
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

    pub(super) fn tx_stream_for(shared: &Arc<TxShared>) -> TxStream {
        TxStream {
            id: 7,
            crypto: MediaCrypto::new_for_stream(&[0u8; 32], 7, &[0u8; 16]),
            path: MediaPath::Udp("127.0.0.1:1".parse().unwrap()),
            spec: SourceSpec::Mic,
            loss: LossInjector::new(7, 0.0),
            seq: 0,
            rung: audiohub_net::media::AUTO_TOP_RUNG,
            rs: None,
            rs_last: 0.0,
            pay: Vec::with_capacity(F48 * 4),
            dest_epoch_seen: 0,
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
            body.contains("JitterBuffer::with_tuning(target, st.jb.tuning())"),
            "the resync no longer rebuilds through the buffer's own tuning, so a tier 1 stream \
             loses DEGRADED (and its learned depth) on every self-heal"
        );
        assert!(
            !body.contains("JitterBuffer::new("),
            "a JitterBuffer is being built from the cached DEFAULT tuning inside handle_datagram; \
             on a degraded link that is the wrong profile"
        );
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
        assert!(pull < feed, "在源被取之前就取观测 —— 拿到的是上一 tick 的读数");
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
        let branch = skip_branch(fn_body("pub(crate) fn mixer_loop("));
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
        let body = fn_body("pub(crate) fn tx_loop(");
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
        assert_eq!(m.len(), 2, "two slots must be two sources, not one shared ring");
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
        ("write(", "write 进内核网络栈；TCP 媒体（tier 1）就是靠它发的"),
        ("write_all(", "同上，而且它会一直重试到写完，上界更差"),
        ("flush(", "flush 会把攒着的字节推进内核，与 write 同级"),
        ("write_frame(", "控制帧写在截止期线程上：JSON 序列化 + 阻塞 write"),
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
        assert!(s.contains("a();") && s.contains("b();"), "块注释剥过头了：{s:?}");
        // 反向：字符串字面量里的同名字符**不许**被当成注释。
        let s = strip_comments(r#"let u = "https://x/y"; let c = "// 不是注释";"#);
        assert!(s.contains("https://x/y"), "把 URL 里的 // 当成注释了：{s:?}");
        assert!(s.contains("// 不是注释"), "把字符串里的 // 当成注释了：{s:?}");
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
        for f in ["pub(crate) fn tx_loop(", "fn apply_txcmd(", "fn refresh_dest("] {
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
        assert!(
            body.contains("inner.media_send.enqueue("),
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
                BANNED_ON_THE_DEADLINE_THREAD.iter().any(|(n, _)| text.contains(n)),
                "禁表里没有任何一条认领得了这份回归样本，于是它可以原样落进 tx_loop：\n{text}"
            );
        }
        // 而真正的 tx_loop 体是非空的（切范围没切歪）。
        let body = fn_body("pub(crate) fn tx_loop(");
        assert!(body.len() > 2000, "tx_loop 的函数体只有 {} 字节，切范围歪了", body.len());
        assert!(body.contains("tx.crypto.seal_into("), "切出来的不是 tx_loop 的正文");
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
            assert!(s.q.consume(|slot| seen.push((slot.buf.as_ptr() as usize, slot.buf.capacity()))));
        }
        for i in SEND_SLOTS..seen.len() {
            assert_eq!(
                seen[i], seen[i - SEND_SLOTS],
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
        assert_eq!(
            src.matches("media_send.enqueue(").count(),
            1,
            "发送队列有了第二个生产者 —— SpscRing 的 unsafe 前提当场作废"
        );
        assert_eq!(
            src.matches("media_send.q.consume(").count(),
            1,
            "发送队列有了第二个消费者 —— 同上"
        );
        assert!(fn_body("pub(crate) fn tx_loop(").contains("media_send.enqueue("));
        assert!(fn_body("pub(crate) fn udp_send_loop(").contains("media_send.q.consume("));
        let ka = fn_body("pub(crate) fn send_pullreq(");
        assert!(ka.contains("inner.udp.send_to("), "keepalive 不再直接发了");
        assert!(!ka.contains("media_send"), "keepalive 走进了发送队列 = 第二个生产者");
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
        assert_eq!(st.dest_epoch_seen, shared.dest_epoch.load(Ordering::Acquire));
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
        assert!(st.path.udp_dest().is_none(), "tier 1 的路径上长出了一个 UDP 目的地");

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

    /// `rx_loop` 学到新地址之后必须**在写完值之后**推代号。
    /// 反过来写就是「代号说变了、锁里还是旧值」——而代号不会再动第二次。
    #[test]
    fn the_receiver_bumps_the_epoch_after_it_writes_the_address() {
        let body = fn_body("fn handle_datagram(");
        let arm = body.split("let learned = SocketAddr::new(").nth(1).expect("keepalive 分支");
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
        for f in ["pub(crate) fn tx_loop(", "fn apply_txcmd(", "fn reap_dead_sources("] {
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
        Src::Frame(Box::new(ToneSource::new(440.0, TONE_AMP, 48000, FRAME_MS as u32)))
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
        assert!(br.try_recv().is_err(), "同一个源发了两次 Build = 开两次设备");
        // 谁都还没被 ack（设备还没开出来）。
        assert!(ack1.try_recv().is_err() && ack2.try_recv().is_err());
        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen, result: Ok(tone()) });
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
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else { panic!() };
        st.on_build_done(BuildDone {
            spec: SourceSpec::Mic,
            gen,
            result: Err("no default input device".into()),
        });
        assert!(st.streams.is_empty(), "失败了还把流装上了");
        assert!(st.sources.is_empty());
        assert_eq!(ack1.try_recv().unwrap(), Err("no default input device".into()));
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
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else { panic!() };
        apply_txcmd(&mut st, TxCmd::Remove { stream_id: 1 });
        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen, result: Ok(tone()) });
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
        let BuildReq::Build { gen: g0, .. } = br.try_recv().unwrap() else { panic!() };
        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen: g0, result: Ok(tone()) });
        assert_eq!(st.sources[&SourceSpec::Mic].gen, g0);

        request_mic_rebuild(&mut st);
        let BuildReq::Build { gen: g1, .. } = br.try_recv().unwrap() else { panic!() };
        assert_ne!(g0, g1);
        // 重建在途期间，老的源一个字节都没动 —— 「新设备打不开就保留旧采集」
        // 这条保证就是靠它兑现的。
        assert_eq!(st.sources[&SourceSpec::Mic].gen, g0, "重建还没回来就把老的换掉了");
        assert!(br.try_recv().is_err(), "重建在途还发了第二条请求");

        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen: g1, result: Ok(tone()) });
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
        let BuildReq::Build { gen: g0, .. } = br.try_recv().unwrap() else { panic!() };
        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen: g0, result: Ok(tone()) });
        request_mic_rebuild(&mut st);
        let BuildReq::Build { gen: g1, .. } = br.try_recv().unwrap() else { panic!() };
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
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else { panic!() };
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
        let BuildReq::Build { gen, .. } = br.try_recv().unwrap() else { panic!() };
        st.on_build_done(BuildDone { spec: SourceSpec::Mic, gen, result: Ok(tone()) });
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
                resampler_for(rung, 0.0).is_some(),
                rate != MicSource::OUT_RATE,
                "rung {rung} is {rate} Hz: a stream installed here would put {} samples on the \
                 wire under a header declaring {rate} Hz",
                if rate == MicSource::OUT_RATE { "the right number of" } else { "48 kHz" }
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
        let at = src.find("fn install_stream(").expect("install_stream is gone");
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
            assert_eq!(samples_per_frame % parts, 0, "rung {i} 的帧切不成等长的两半");
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
            let parts = f.wire_packets_per_frame();
            // **调生产代码的那个函数**，不是把同一行算术抄一遍。
            // 抄一遍的版本对 `tx_loop` 的改动完全免疫（实测：把生产代码里的
            // `+p*5000` 删掉，抄一遍的版本照样绿）。
            let stamps: Vec<u64> = (0..parts).map(|p| split_timestamp_us(ts_us, p, parts)).collect();
            assert_eq!(stamps[0], ts_us, "前半包的时间戳被动过了");
            if parts == 2 {
                assert_eq!(stamps[1], ts_us + 5_000, "后半包没有 +5 ms：抖动样本会有一半近似 0");
            } else {
                assert_eq!(stamps.len(), 1);
            }
        }
        // 直接钉住这条不变量，免得将来 `LADDER` 里恰好没有分包档时这条测试
        // 退化成「什么都没测」。
        assert_eq!(split_timestamp_us(7_000, 0, 2), 7_000);
        assert_eq!(split_timestamp_us(7_000, 1, 2), 12_000, "后半包必须 +5 ms");
        assert_eq!(split_timestamp_us(7_000, 0, 1), 7_000, "不分包时时间戳不许被动");
    }

    /// 接收侧从**实到样本数**认出半帧，而不是从格式表推。
    ///
    /// 一个不分包的对端发来整帧时照样能认出来；按表推会把它当半帧、
    /// 去等一个永远不来的搭档，表现是**每一帧都走半帧隐藏**（有声音，一半是编的）。
    #[test]
    fn a_half_frame_is_recognised_by_its_sample_count_not_by_the_ladder() {
        for f in LADDER.iter() {
            let full = f.rate_hz as usize / 100;
            let parts = f.wire_packets_per_frame();
            let arrived = full / parts;
            let detected = if arrived * 2 == full { 2 } else { 1 };
            assert_eq!(detected, parts, "{f:?} 的分包数认错了");
            // 同一档、对端不分包地发整帧：必须被认成整帧。
            assert_eq!(if full * 2 == full { 2 } else { 1 }, 1, "整帧被当成了半帧");
        }
    }

    /// **搭档没来时补出来的那一帧长度必须正好是一整帧**，且真实的那一半
    /// 逐样本原样保留。
    ///
    /// 长度错了会让 JB 的 `frame_len` 跟着变，混音那一拍就少（或多）一段音频，
    /// 而没有任何一处会报错。
    ///
    /// 注入对照：把 `conceal_missing_half` 的 `out.truncate(full)` 删掉并让
    /// 淡出循环跑 `missing + 1` 次，这条红在长度。
    #[test]
    fn a_missing_partner_is_concealed_into_exactly_one_full_frame() {
        let full = 480usize;
        let held: Vec<f32> = (0..240).map(|i| (i as f32 / 240.0) - 0.5).collect();

        // 缺后半：前半原样在前，补出来的在后。
        let a = conceal_missing_half(&held, false, full);
        assert_eq!(a.len(), full, "补出来的帧不是一整帧");
        assert_eq!(&a[..240], &held[..], "在手的那一半被改动了");
        assert!(a.iter().all(|v| v.is_finite()), "隐藏出来的样本里有非有限值");

        // 缺前半：补出来的在前，后半原样在后。
        let b = conceal_missing_half(&held, true, full);
        assert_eq!(b.len(), full);
        assert_eq!(&b[240..], &held[..], "在手的那一半被改动了");

        // 隐藏段必须**衰减**（最后一个样本比第一个更接近 0），否则它就不是
        // 「上一段真实音频的衰减重复」，而是一段会被听成回声的原样复读。
        let tail_first = a[240].abs();
        let tail_last = a[full - 1].abs();
        assert!(tail_last <= tail_first, "隐藏段没有衰减：{tail_first} -> {tail_last}");
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
            let samples: Vec<f32> =
                (0..n).map(|i| ((i as f32 / n as f32) * 2.0 - 1.0) * 0.9).collect();
            let whole = dsp::decode_pcm(&dsp::encode_pcm(&samples, f.depth), f.depth);

            let parts = f.wire_packets_per_frame();
            let chunk = samples.len() / parts;
            let mut rebuilt: Vec<f32> = Vec::with_capacity(n);
            for p in 0..parts {
                let lo = p * chunk;
                let hi = if p + 1 == parts { samples.len() } else { lo + chunk };
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
                assert_eq!(a.to_bits(), b.to_bits(), "{f:?} 第 {i} 个样本分包前后不一致");
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
        let at48: Vec<u8> = seen.iter().filter(|(r, _)| *r == 48_000).map(|(_, c)| *c).collect();
        assert_eq!(at48.len(), 3, "48 kHz 应当有三档");
        let mut uniq = at48.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 3, "三个 48 kHz 档的 codec 撞在了一起：收方分不出位深");
        assert_eq!(
            rung_format(2),
            WireFormat { rate_hz: 48_000, depth: WireDepth::S16 },
            "AUTO 天花板那一档的格式变了：所有 AUTO 用户的线上格式会跟着变"
        );
    }
}
