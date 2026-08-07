//! 音质三分量的采集与定级（规格 spec-telemetry-ia.md §4）。
//!
//! **音质 = 保真度**：最终送进扬声器的样本流，相对于对端采集到的原始波形被
//! 损坏了多少。测点在 **JitterBuffer pop 之后、送进播放环之前**——那是「用户
//! 实际会听到的样本」最后一次可被观测的地方，也正是本次之前遥测的黑洞段。
//!
//! ## 明确拒绝：用丢包率当音质
//!
//! 与「RTT 冒充延迟」同构的错误。丢包 2% 在 PLC 修得住时几乎不可闻；丢包 0%
//! 时两路重复流相加照样把声音削烂。丢包率是**网口上的量**，音质是**扬声器上
//! 的量**，中间差一整条流水线。
//!
//! ## 三个分量物理上互不换算，所以取 min 而不是加权平均
//!
//! 48 kHz 全频带的破音不会因为频带宽而好听；无削顶的断续也不会因为不破音而
//! 连续。加权平均会把「两路重复流把声音削烂」（Q2=差、Q1=优、Q3=优）稀释成
//! 「良」，**恰好掩盖用户要抓的那个 bug**。min 不会。
//!
//! ## 窗口化：为什么不能直接用 JitterBuffer 的 lifetime 计数
//!
//! `media.rs` 的五个计数器是 lifetime 累计。用 lifetime 算隐藏率会让一次早期
//! 抖动**永远**压着等级——这与 `take_interval` 已经吸取过的教训是同一条
//! （那里的注释原话：「one early dropout must not pin the sender to the lowest
//! rung forever」）。也不能复用 `take_interval` 本身：它是**消费型**的，
//! ticker 每秒调一次，而 stats 事件是另一条路径且周期由订阅方指定
//! （`ipcserv.rs`），两条路径不能共用消费型接口。
//!
//! 所以差分放在 daemon 侧的非消费型时间戳环里，`JitterBuffer` 保持纯累计——
//! `audiohub-net` 不需要知道窗口的存在。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 统计窗口。10 s @ 100 帧/秒 = 1000 个 tick，足够让 0.1% 量级的隐藏率有意义。
pub(crate) const WINDOW: Duration = Duration::from_secs(10);

/// 窗口至少要有这么长才给结论。短于此的窗口分母太小，一次 prebuffering 就能
/// 把隐藏率顶到 20%——那是**噪声不是信息**，宁可报 `None` 让 UI 显示「—」。
const MIN_SPAN_S: f64 = 1.0;

/// `soft_clip` 的拐点（engine.rs 的频结曲线：≤0.8 线性，之上 tanh 压缩）。
/// 削顶率就是越过这条线的采样占比。
pub(crate) const CLIP_THRESHOLD: f32 = 0.8;

// ---------------------------------------------------------------- Q1 隐藏率

/// `JitterBuffer` 五个 lifetime 计数器的一次快照。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct JbCounts {
    pub popped: u64,
    pub plc: u64,
    pub silence: u64,
    pub underruns: u64,
    pub dropped: u64,
    /// 深档（5 ms 分包）里按**半帧隐藏**交付的帧数。
    ///
    /// 不来自 `JitterBuffer`——它在重组环节，见 `JbState::half_conceal`。放进
    /// 这个快照是为了让它跟着同一个 10 s 窗口差分，并计入 [`conceal_ratio`]。
    pub half_conceal: u64,
}

impl JbCounts {
    /// 两次快照之差。用 `saturating_sub` 是因为 JB 会被**整体重建**
    /// （engine.rs 的 starvation self-heal 把 `st.jb` 换成新的），届时
    /// lifetime 计数归零，naive 相减会下溢成天文数字。
    fn delta(newer: JbCounts, older: JbCounts) -> JbCounts {
        JbCounts {
            popped: newer.popped.saturating_sub(older.popped),
            plc: newer.plc.saturating_sub(older.plc),
            silence: newer.silence.saturating_sub(older.silence),
            underruns: newer.underruns.saturating_sub(older.underruns),
            dropped: newer.dropped.saturating_sub(older.dropped),
            half_conceal: newer.half_conceal.saturating_sub(older.half_conceal),
        }
    }
}

/// 非消费型的 10 s 滚动窗口：只记时间戳 + 累计值，谁读都不改变它。
///
/// 约 100 项/流（每 10 次 push 采一点，外加 ticker 每秒兜一点），可忽略。
pub(crate) struct ConcealWindow {
    pts: VecDeque<(Instant, JbCounts)>,
}

impl ConcealWindow {
    pub(crate) fn new() -> ConcealWindow {
        ConcealWindow { pts: VecDeque::new() }
    }

    /// 追加一个采样点并丢弃窗口外的。
    ///
    /// **两个调用方**：接收线程每 10 次 push 一次（音频在流时给出细分辨率），
    /// 以及 ticker 每秒一次。后者不是冗余——**断流时接收线程根本不跑**，
    /// 而断流恰恰是 JB 在疯狂 underrun / 静音、Q1 最该报警的时候。只挂在
    /// push 上会让窗口在黑屏期间冻结，报出黑屏**之前**那 10 秒的漂亮数字。
    pub(crate) fn sample(&mut self, now: Instant, c: JbCounts) {
        self.pts.push_back((now, c));
        // `Instant` 在两个平台上都是「自开机以来」，所以开机后头 10 秒内
        // `now - 10s` 是真的算不出来。算不出来就**不修剪**——让窗口暂时长一点，
        // 远好过 `unwrap_or(now)` 那种写法：那会把 cutoff 定成「现在」，一次
        // 把窗口削到只剩一个点，于是接下来 10 秒 `window()` 全部返回 None。
        let Some(cutoff) = now.checked_sub(WINDOW) else { return };
        // 队首始终保留「不晚于 cutoff 的最新一点」作为基线：直接按 cutoff 硬删
        // 会在稀疏采样时把整个窗口删空。
        while self.pts.len() >= 2 && self.pts[1].0 <= cutoff {
            self.pts.pop_front();
        }
    }

    /// JB 被整体重建时调用：计数器归零是一次真实的不连续，旧点不能再参与差分。
    pub(crate) fn reset(&mut self) {
        self.pts.clear();
    }

    /// `(窗口秒数, 窗口内增量)`。点数不足或跨度太短 ⇒ `None`——**不是 0%**。
    pub(crate) fn window(&self) -> Option<(f64, JbCounts)> {
        if self.pts.len() < 2 {
            return None;
        }
        let (t0, c0) = self.pts[0];
        let (t1, c1) = self.pts[self.pts.len() - 1];
        let span = t1.duration_since(t0).as_secs_f64();
        if span < MIN_SPAN_S {
            return None;
        }
        Some((span, JbCounts::delta(c1, c0)))
    }
}

/// Q1 加权隐藏率：`(plc + 3*silence + 0.5*half_conceal) / (popped + plc + silence)`。
///
/// **silence 权重 3 的依据**：PLC 在 `media.rs` 是「上一帧 ×0.7 重复」，仍有
/// 能量、仍连续；silence 是彻底的真空。ITU-T G.113 附录 I 对帧擦除给出有隐藏 /
/// 无隐藏两条 Ie 曲线，同一丢失率下无隐藏的损伤值约为有隐藏的 2.5~3 倍。取 3
/// 是这条经验的整数化，且与「PLC 连续 5 帧后转静音」自洽。
///
/// # `half_conceal` 权重 0.5 的依据（位深进阶梯新增）
///
/// 深档按 5 ms 分包，搭档半帧没来时 `conceal_missing_half` 会把到手的那半帧
/// 淡出、补齐成一个**长度完整**的帧交付。于是：
///
/// - 它**已经在分母里**：那一帧照常进 JB、照常被 pop，`popped` 算过它。
/// - 它伪造的正好是 **10 ms 里的 5 ms** —— 半个 PLC 帧的隐藏量，且伪造方式
///   与 PLC 同族（衰减延续），所以权重取 PLC 的一半，不是 1、也不是 3。
///
/// 不计它的后果是这条降级**在 Q1 上完全不可见**：JB 看到的是完整长度的帧，
/// 不记 PLC、不记 underrun，`popped` 照常增长 ⇒ 深档丢掉一半的包，等级仍报「优」。
///
/// 分母为 0（窗口内一个 tick 都没输出）⇒ `None`：没有输出就没有音质可言。
pub(crate) fn conceal_ratio(c: &JbCounts) -> Option<f64> {
    let total = c.popped + c.plc + c.silence;
    if total == 0 {
        return None;
    }
    Some((c.plc as f64 + 3.0 * c.silence as f64 + 0.5 * c.half_conceal as f64) / total as f64)
}

// ---------------------------------------------------------------- Q2 电平

/// 一页完成的削顶统计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClipWindow {
    pub span_s: f64,
    pub samples: u64,
    pub over: u64,
    /// 削顶**之前**的峰值。
    pub peak: f32,
}

impl ClipWindow {
    pub(crate) fn ratio(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.over as f64 / self.samples as f64
        }
    }

    /// `20*log10(peak / 0.8)`。负值 = 根本没碰到削顶阈值。
    /// 全静音时下钳到 −120 dB，避免 `-inf` 变成 JSON 的 `null`。
    pub(crate) fn excess_db(&self) -> f64 {
        let p = (self.peak as f64).max(1e-6);
        (20.0 * (p / CLIP_THRESHOLD as f64).log10()).max(-120.0)
    }
}

/// 削顶计量表，**双缓冲**：读取方永远拿到一个**完整**的窗口，不会读到半页。
///
/// 写入方唯一，就是混音线程；读取方是报告线程。翻页用 seqlock 保护，读到
/// 撕裂就重试——不上互斥锁是因为写入方在 10 ms 节拍上（规格附录约束 3：那里
/// 只允许常数次原子操作，任何 `Mutex` 获取都会把节拍污染进被测对象）。
pub(crate) struct ClipMeter {
    /// 偶数 = 稳定，奇数 = 正在翻页。
    epoch: AtomicU64,
    cur_samples: AtomicU64,
    cur_over: AtomicU64,
    cur_peak_q16: AtomicU32,
    cur_start_ms: AtomicU64,
    prev_samples: AtomicU64,
    prev_over: AtomicU64,
    prev_peak_q16: AtomicU32,
    prev_span_ms: AtomicU64,
}

/// 峰值以 Q16 定点存放，好让「取最大」是一条 `fetch_max` 而不是 CAS 循环。
/// 分辨率 1/65536 ≈ −96 dB，量程 65535.0（求和后的峰值远够用）。
fn to_q16(v: f32) -> u32 {
    (v.max(0.0) * 65536.0).min(u32::MAX as f32) as u32
}

fn from_q16(q: u32) -> f32 {
    q as f32 / 65536.0
}

impl Default for ClipMeter {
    fn default() -> Self {
        ClipMeter::new()
    }
}

impl ClipMeter {
    pub(crate) fn new() -> ClipMeter {
        ClipMeter {
            epoch: AtomicU64::new(0),
            cur_samples: AtomicU64::new(0),
            cur_over: AtomicU64::new(0),
            cur_peak_q16: AtomicU32::new(0),
            cur_start_ms: AtomicU64::new(0),
            prev_samples: AtomicU64::new(0),
            prev_over: AtomicU64::new(0),
            prev_peak_q16: AtomicU32::new(0),
            prev_span_ms: AtomicU64::new(0),
        }
    }

    /// 混音线程：吃一帧（削顶**之前**的样本），必要时在 tick 边界翻页。
    ///
    /// `now_ms` 是 daemon 单调时基下的毫秒。遍历 480 个样本的代价与混音循环
    /// 本身同阶（那里已经在做 480 次加法），可以接受；除法与格式化全部留给
    /// 报告线程。
    pub(crate) fn feed(&self, now_ms: u64, frame: &[f32]) {
        let mut over = 0u64;
        let mut peak = 0.0f32;
        for &v in frame {
            let a = v.abs();
            if a > CLIP_THRESHOLD {
                over += 1;
            }
            if a > peak {
                peak = a;
            }
        }
        self.cur_samples
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        if over > 0 {
            self.cur_over.fetch_add(over, Ordering::Relaxed);
        }
        self.cur_peak_q16.fetch_max(to_q16(peak), Ordering::Relaxed);

        let start = self.cur_start_ms.load(Ordering::Relaxed);
        if start == 0 {
            self.cur_start_ms.store(now_ms, Ordering::Relaxed);
            return;
        }
        if now_ms.saturating_sub(start) >= WINDOW.as_millis() as u64 {
            self.flip(now_ms, start);
        }
    }

    fn flip(&self, now_ms: u64, start_ms: u64) {
        self.epoch.fetch_add(1, Ordering::AcqRel); // -> 奇数：读取方须重试
        self.prev_samples
            .store(self.cur_samples.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_over
            .store(self.cur_over.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_peak_q16
            .store(self.cur_peak_q16.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_span_ms
            .store(now_ms.saturating_sub(start_ms), Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::AcqRel); // -> 偶数：本页可读
        self.cur_samples.store(0, Ordering::Relaxed);
        self.cur_over.store(0, Ordering::Relaxed);
        self.cur_peak_q16.store(0, Ordering::Relaxed);
        self.cur_start_ms.store(now_ms, Ordering::Relaxed);
    }

    /// 报告线程：最近一个**完整**窗口。`None` = 还没攒够一整页，或读撕裂了。
    /// **绝不返回半页的比率**——那会在启动后头 10 秒给出随机的削顶率。
    pub(crate) fn window(&self) -> Option<ClipWindow> {
        for _ in 0..8 {
            let e1 = self.epoch.load(Ordering::Acquire);
            if e1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let samples = self.prev_samples.load(Ordering::Relaxed);
            let over = self.prev_over.load(Ordering::Relaxed);
            let peak = self.prev_peak_q16.load(Ordering::Relaxed);
            let span = self.prev_span_ms.load(Ordering::Relaxed);
            if self.epoch.load(Ordering::Acquire) != e1 {
                continue; // 翻页插进来了，重读
            }
            if samples == 0 {
                return None;
            }
            return Some(ClipWindow {
                span_s: span as f64 / 1000.0,
                samples,
                over,
                peak: from_q16(peak),
            });
        }
        None
    }
}

// ------------------------------------------------------- 重复流（站点级）

/// 两路参与求和的帧在零延迟上的归一化互相关。
///
/// 零延迟即可：重复流是**同一份解码结果分两条会话进来**，样本级已经对齐。
/// 480 点点积 ≈ 1.4k flops / 10 ms，可忽略。
///
/// **任一路能量为零 ⇒ `None`，不是 1.0**。这条至关重要：两路静音在数学上
/// 「完全相同」，若判成重复，任何空闲的双会话都会被永久诬告成叠加 bug，
/// 而真正的告警就淹没了。
pub(crate) fn correlation(a: &[f32], b: &[f32]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n == 0 {
        return None;
    }
    let (mut sab, mut saa, mut sbb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let (x, y) = (a[i] as f64, b[i] as f64);
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    let denom = (saa * sbb).sqrt();
    if denom < 1e-12 {
        return None;
    }
    Some(sab / denom)
}

/// 判为「同一份内容」的相关度门限。见 `MixHealth::duplicate_suspect` 上的
/// 「对阈值不敏感」论证：真重复给出 ~1.0，两路无关素材给出 <0.3，中间是真空。
const DUP_CORR: f64 = 0.98;
/// 窗口内多少比例的「双路 tick」相关度超标才算实锤。偶尔一帧巧合不算。
const DUP_SHARE: f64 = 0.9;

/// 一页完成的混音形态统计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MixWindow {
    pub span_s: f64,
    /// 单 tick 参与求和的最大流数。
    pub max_contrib: u32,
    /// 有过两路及以上、且两路都非静音的 tick 数。
    pub pair_ticks: u64,
    /// 其中相关度超标的 tick 数。
    pub dup_ticks: u64,
    /// 这些 tick 上相关度的最大值。`None` = 从没出现过可比较的两路。
    pub corr_peak: Option<f64>,
}

impl MixWindow {
    pub(crate) fn duplicate_suspect(&self) -> bool {
        self.pair_ticks > 0 && (self.dup_ticks as f64 / self.pair_ticks as f64) > DUP_SHARE
    }
}

/// 混音形态计量表。与 `ClipMeter` 同样的双缓冲 + seqlock。
pub(crate) struct MixMeter {
    epoch: AtomicU64,
    cur_max_contrib: AtomicU32,
    cur_pair: AtomicU64,
    cur_dup: AtomicU64,
    cur_corr_q16: AtomicU32,
    cur_has_corr: AtomicU64, // 0/1，避免再引一个 AtomicBool
    cur_start_ms: AtomicU64,
    prev_max_contrib: AtomicU32,
    prev_pair: AtomicU64,
    prev_dup: AtomicU64,
    prev_corr_q16: AtomicU32,
    prev_has_corr: AtomicU64,
    prev_span_ms: AtomicU64,
}

impl Default for MixMeter {
    fn default() -> Self {
        MixMeter::new()
    }
}

impl MixMeter {
    pub(crate) fn new() -> MixMeter {
        MixMeter {
            epoch: AtomicU64::new(0),
            cur_max_contrib: AtomicU32::new(0),
            cur_pair: AtomicU64::new(0),
            cur_dup: AtomicU64::new(0),
            cur_corr_q16: AtomicU32::new(0),
            cur_has_corr: AtomicU64::new(0),
            cur_start_ms: AtomicU64::new(0),
            prev_max_contrib: AtomicU32::new(0),
            prev_pair: AtomicU64::new(0),
            prev_dup: AtomicU64::new(0),
            prev_corr_q16: AtomicU32::new(0),
            prev_has_corr: AtomicU64::new(0),
            prev_span_ms: AtomicU64::new(0),
        }
    }

    /// 混音线程：一个 tick 的形态。`corr` 为 `None` 表示这一 tick 没有可比较的
    /// 两路（只有一路，或其中一路是静音）。
    pub(crate) fn feed(&self, now_ms: u64, contrib: u32, corr: Option<f64>) {
        self.cur_max_contrib.fetch_max(contrib, Ordering::Relaxed);
        if let Some(r) = corr {
            self.cur_pair.fetch_add(1, Ordering::Relaxed);
            if r > DUP_CORR {
                self.cur_dup.fetch_add(1, Ordering::Relaxed);
            }
            self.cur_corr_q16
                .fetch_max(to_q16(r.max(0.0) as f32), Ordering::Relaxed);
            self.cur_has_corr.store(1, Ordering::Relaxed);
        }
        let start = self.cur_start_ms.load(Ordering::Relaxed);
        if start == 0 {
            self.cur_start_ms.store(now_ms, Ordering::Relaxed);
            return;
        }
        if now_ms.saturating_sub(start) >= WINDOW.as_millis() as u64 {
            self.flip(now_ms, start);
        }
    }

    fn flip(&self, now_ms: u64, start_ms: u64) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.prev_max_contrib
            .store(self.cur_max_contrib.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_pair
            .store(self.cur_pair.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_dup
            .store(self.cur_dup.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_corr_q16
            .store(self.cur_corr_q16.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_has_corr
            .store(self.cur_has_corr.load(Ordering::Relaxed), Ordering::Relaxed);
        self.prev_span_ms
            .store(now_ms.saturating_sub(start_ms), Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cur_max_contrib.store(0, Ordering::Relaxed);
        self.cur_pair.store(0, Ordering::Relaxed);
        self.cur_dup.store(0, Ordering::Relaxed);
        self.cur_corr_q16.store(0, Ordering::Relaxed);
        self.cur_has_corr.store(0, Ordering::Relaxed);
        self.cur_start_ms.store(now_ms, Ordering::Relaxed);
    }

    pub(crate) fn window(&self) -> Option<MixWindow> {
        for _ in 0..8 {
            let e1 = self.epoch.load(Ordering::Acquire);
            if e1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let max_contrib = self.prev_max_contrib.load(Ordering::Relaxed);
            let pair = self.prev_pair.load(Ordering::Relaxed);
            let dup = self.prev_dup.load(Ordering::Relaxed);
            let corr = self.prev_corr_q16.load(Ordering::Relaxed);
            let has = self.prev_has_corr.load(Ordering::Relaxed);
            let span = self.prev_span_ms.load(Ordering::Relaxed);
            if self.epoch.load(Ordering::Acquire) != e1 {
                continue;
            }
            if span == 0 {
                return None;
            }
            return Some(MixWindow {
                span_s: span as f64 / 1000.0,
                max_contrib,
                pair_ticks: pair,
                dup_ticks: dup,
                corr_peak: (has == 1).then(|| from_q16(corr) as f64),
            });
        }
        None
    }
}

// ---------------------------------------------------------------- 定级

/// 四档。派生的 `Ord` 让 `min()` 直接就是「木桶」合成——差 < 一般 < 良好 < 优。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Grade {
    Poor = 0,
    Fair = 1,
    Good = 2,
    Excellent = 3,
}

impl Grade {
    /// 地板档。**缺席的分量再差也压不动它**——这条不等式就是 `compose` 判定
    /// 「等级已经确定」的全部依据，所以它必须是一个具名常量而不是散落的
    /// `== Grade::Poor`：将来若在下面再加一档，忘了改这里会让「测量中」提前
    /// 变回一个乐观的等级。
    pub(crate) const FLOOR: Grade = Grade::Poor;

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Grade::Excellent => "excellent",
            Grade::Good => "good",
            Grade::Fair => "fair",
            Grade::Poor => "poor",
        }
    }
}

/// Q1 隐藏率分档（规格 §4.3）。
///
/// 「优」线 0.2% 是**结构性噪声底**，不是审美：`MIN_TARGET = 2`，每次启动与
/// 每次 underrun 后都重进 prebuffering，都会产生 1~2 帧隐藏。10 秒 = 1000 帧，
/// 1~2 帧就是 0.1~0.2%。**低于这条线的差异是本系统自己的结构噪声，不是质量
/// 信息。** 1% / 3% 两线取自 G.107 E-model：PCM 语音丢失 <1% 时 R 值几乎不降
/// （业界「透明线」），3% 左右 R 值跌破 70（「用户开始不满意」档）。
pub(crate) fn grade_conceal(ratio: f64) -> Grade {
    if ratio < 0.002 {
        Grade::Excellent
    } else if ratio < 0.01 {
        Grade::Good
    } else if ratio < 0.03 {
        Grade::Fair
    } else {
        Grade::Poor
    }
}

/// Q2 削顶率分档（规格 §4.3）。
///
/// 这组阈值最强的一点是**对取值不敏感**：正常素材峰值 −3 dBFS 时，瞬时越过
/// 0.8（≈ −1.9 dBFS）的采样占比是 1e-4 量级；而**两路相同信号相加等于整段
/// 波形 ×2**，正常电平的音乐立刻有百分之几十的采样越界。两者之间隔着 3 个
/// 数量级的真空，阈值放在这个空隙里的任何位置结论都一样。一个对参数不敏感的
/// 判据，本身就是严谨性的证明。
pub(crate) fn grade_clip(ratio: f64) -> Grade {
    if ratio < 0.0001 {
        Grade::Excellent
    } else if ratio < 0.001 {
        Grade::Good
    } else if ratio < 0.01 {
        Grade::Fair
    } else {
        Grade::Poor
    }
}

/// Q3 有效带宽分档：直接映射 ITU-T 语音带宽分类（窄带 3.4k / 宽带 7k /
/// 超宽带 14k / 全频带 20k），不是发明分档。AUTO 阶梯
/// `[48000, 32000, 24000, 16000]` 的 Nyquist 恰好是 24k/16k/12k/8k。
pub(crate) fn grade_bandwidth(hz: u32) -> Grade {
    if hz >= 24_000 {
        Grade::Excellent
    } else if hz >= 16_000 {
        Grade::Good
    } else if hz >= 12_000 {
        Grade::Fair
    } else {
        Grade::Poor
    }
}

/// 三分量的**木桶**合成：`grade = min(...)`，`worst = argmin(...)`。
///
/// 平手时按 continuity → level → bandwidth 的顺序报第一个触底的：断续是三者中
/// 最刺耳、也最可能是链路真出了问题的一项。
/// 全部为「优」时 `worst = "none"`。
///
/// ## 缺席的分量不给等级，而不是「不参与 min」
///
/// `q2` 是 `Option`：削顶页还没攒满时它是 `None`（流开头约 10~20 秒）。
///
/// 这里有过两版都不对的写法，第二版尤其像已经修好了：
///
/// 1. 缺席即填 `grade_clip(0.0) = Excellent`。
/// 2. 缺席即「不进 min」，另外把 `partial = true` 报上去。
///
/// **两版在等级上逐值相同**——`Grade::Excellent` 是 `Ord` 的最大值，
/// `min(q1, Excellent, q3) ≡ min(q1, q3)`。第二版只是把缺席**标注**出来，
/// 而 `grade` 字段本身照旧宣称「良好」，用户看到的那个词一个字都没变。原缺陷
/// （一条正在爆音的流在开头 10~20 秒报「良好」）原样存活。
///
/// 真正的修法要从「缺席算什么等级」退回到「缺席时等级还成不成立」：
///
/// - 在场分量的 min 是真实等级的**上界**（缺的那块板只可能更短，不可能更长）。
/// - 上界之下、地板之上的整段区间都还是可能的 ⇒ 一个区间不是一个等级 ⇒
///   `None`，由 UI 说「测量中」。这与 Q1「窗口不够就整体不给结论」同一口径。
/// - **唯一的例外**：上界已经贴着地板（`Grade::FLOOR`）时，区间退化成一个点，
///   缺席分量再差也改不了结论 ⇒ 等级确定，照常报出来。已知是「差」的流没有
///   理由被藏进「测量中」，那反而是拿不确定性掩盖一个确定的坏消息。
///
/// 返回 `(等级, 拖后腿的分量, partial)`。`partial` 仍然如实上报——即便等级
/// 确定，「这一次的木桶少了一块板」也是调用方与 UI 该知道的事实。
pub(crate) fn compose(
    q1: Grade,
    q2: Option<Grade>,
    q3: Grade,
) -> (Option<Grade>, &'static str, bool) {
    let partial = q2.is_none();
    // 在场分量的 min：`partial` 为真时它是上界，为假时它就是结论本身。
    let bound = match q2 {
        Some(q2) => q1.min(q2).min(q3),
        None => q1.min(q3),
    };
    if partial && bound > Grade::FLOOR {
        // 真实等级落在 [FLOOR, bound] 里。**不报 bound**：那正是旧写法把
        // 「还没测」讲成「良好」的那一步。`worst` 同理没有意义——连等级都还
        // 没定，谈不上谁拖后腿。
        return (None, "none", true);
    }
    if bound == Grade::Excellent {
        return (Some(bound), "none", partial);
    }
    let worst = if q1 == bound {
        "continuity"
    } else if q2 == Some(bound) {
        // 缺席的 Q2 永远走不到这里：`Some(bound)` 对 `None` 不成立，所以
        // 「还没测的那一项」不会被说成拖后腿的那一项。
        "level"
    } else {
        "bandwidth"
    };
    (Some(bound), worst, partial)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Q1 窗口 ----

    fn c(popped: u64, plc: u64, silence: u64) -> JbCounts {
        JbCounts { popped, plc, silence, underruns: 0, dropped: 0, half_conceal: 0 }
    }

    /// 带半帧隐藏的那一版（深档专用）。
    fn ch(popped: u64, plc: u64, silence: u64, half: u64) -> JbCounts {
        JbCounts { half_conceal: half, ..c(popped, plc, silence) }
    }

    /// 规格 §6.2：构造 lifetime 计数序列，断言窗口值不受 10 s 之前的事件影响。
    /// 这条是「一次早期抖动永远压着等级」那个 bug 的直接防线。
    #[test]
    fn the_window_forgets_events_older_than_ten_seconds() {
        let mut w = ConcealWindow::new();
        let t0 = Instant::now();
        // 头两秒：一场灾难，300 帧里 200 帧是静音
        w.sample(t0, c(0, 0, 0));
        w.sample(t0 + Duration::from_secs(2), c(100, 0, 200));
        // 之后 20 秒：完全干净，每秒 100 帧真音频
        for i in 1..=20u64 {
            let popped = 100 + 100 * i;
            w.sample(t0 + Duration::from_secs(2 + i), c(popped, 0, 200));
        }
        let (span, d) = w.window().expect("窗口已成立");
        assert!(span <= 11.0, "窗口不该超出 10 秒太多, got {span}");
        assert_eq!(d.silence, 0, "早期那 200 帧静音必须已经滑出窗口");
        assert_eq!(conceal_ratio(&d), Some(0.0), "近 10 秒是干净的");
    }

    /// lifetime 累计值本身会把早期灾难永远算进去——这正是不能直接用它的原因。
    #[test]
    fn lifetime_counters_would_pin_the_grade_forever() {
        let lifetime = c(2100, 0, 200);
        let r = conceal_ratio(&lifetime).unwrap();
        assert!(r > 0.2, "lifetime 口径下隐藏率仍高达 {r:.3}");
        assert_eq!(grade_conceal(r), Grade::Poor, "20 秒前的一次抖动仍在定级");
    }

    /// JB 被整体重建后 lifetime 计数归零，naive 相减会下溢成天文数字。
    #[test]
    fn a_jitter_buffer_rebuild_cannot_underflow_the_delta() {
        let d = JbCounts::delta(c(5, 0, 0), c(9_000, 3, 7));
        assert_eq!(d.popped, 0);
        assert_eq!(d.plc, 0);
        assert_eq!(d.silence, 0);
    }

    #[test]
    fn a_reset_window_reports_nothing_rather_than_a_bogus_delta() {
        let mut w = ConcealWindow::new();
        let t0 = Instant::now();
        w.sample(t0, c(0, 0, 0));
        w.sample(t0 + Duration::from_secs(5), c(500, 0, 0));
        assert!(w.window().is_some());
        w.reset();
        assert_eq!(w.window(), None, "重建后必须报『没有数据』而不是一个假差分");
    }

    #[test]
    fn a_too_short_window_is_none_not_zero_percent() {
        let mut w = ConcealWindow::new();
        let t0 = Instant::now();
        w.sample(t0, c(0, 0, 0));
        w.sample(t0 + Duration::from_millis(200), c(20, 0, 0));
        assert_eq!(w.window(), None, "0.2 秒的分母太小，结论是噪声");
    }

    /// 规格 §6.2 点名：`plc=1 / silence=1` 应给出 `(1 + 3)/N`。
    #[test]
    fn silence_weighs_three_times_a_concealed_frame() {
        let r = conceal_ratio(&c(98, 1, 1)).unwrap();
        assert!((r - 4.0 / 100.0).abs() < 1e-12, "(1 + 3*1)/100, got {r}");
        // 同样 2 帧非原始采样，全是 PLC 时只有一半的损伤值
        let plc_only = conceal_ratio(&c(98, 2, 0)).unwrap();
        assert!((plc_only - 2.0 / 100.0).abs() < 1e-12);
        assert!(plc_only < r, "静音必须比 PLC 更重");
    }

    /// 窗口内一个 tick 都没输出 ⇒ 没有音质可言，不是「0% 隐藏率，完美」。
    #[test]
    fn no_output_at_all_is_none_not_a_perfect_score() {
        assert_eq!(conceal_ratio(&c(0, 0, 0)), None);
    }

    /// **深档的半帧隐藏必须动 Q1**，且权重恰是 PLC 的一半。
    ///
    /// 这条挡的是本项目反复栽的那个形态：计数器加了、注释写了「否则 Q1 上完全
    /// 不可见」、然后**没有任何一处读它**。半帧隐藏交付的是一个长度完整的帧，
    /// JB 不记 PLC、不记 underrun、`popped` 照常增长 ⇒ 不接进来的话，深档丢掉
    /// 一半的包，Q1 仍然报满分。
    ///
    /// 注入对照：把 `conceal_ratio` 里的 `0.5 * c.half_conceal` 删掉 ⇒ 红在
    /// 第一条断言（深档丢一半包与完全干净的链路读数相同）。
    #[test]
    fn a_half_frame_conceal_costs_half_a_plc_frame_in_q1() {
        let clean = conceal_ratio(&c(100, 0, 0)).unwrap();
        let halves = conceal_ratio(&ch(100, 0, 0, 20)).unwrap();
        assert!(
            halves > clean,
            "20 次半帧隐藏与完全干净的链路给出同一个 Q1（{halves} vs {clean}）：\
             深档丢一半包在等级上完全不可见"
        );
        // 权重 = PLC 的一半：一次半帧隐藏正好伪造 10 ms 里的 5 ms。
        assert!((halves - 10.0 / 100.0).abs() < 1e-12, "(0.5*20)/100, got {halves}");
        // 与整帧 PLC 比时**分母必须对齐**：PLC 帧是 JB 自己造出来的，它进分母
        // （`popped + plc + silence`）；半帧隐藏那一帧是真的被 pop 出去的，
        // `popped` 已经算过它。所以「同样 100 帧输出，其中 20 帧全隐藏」对的是
        // `c(80, 20, 0)`，不是 `c(100, 20, 0)`——后者是 120 帧输出。
        let full_plc = conceal_ratio(&c(80, 20, 0)).unwrap();
        assert!(
            (full_plc - 2.0 * halves).abs() < 1e-12,
            "同样 100 帧输出里 20 帧整帧 PLC，必须恰是 20 次半帧隐藏的两倍：\
             {full_plc} vs {halves}"
        );
        // 分母不变：那一帧照常进 JB、照常被 pop，`popped` 已经算过它。
        assert!(
            (conceal_ratio(&ch(100, 4, 2, 0)).unwrap() - conceal_ratio(&c(100, 4, 2)).unwrap())
                .abs()
                < 1e-12,
            "half_conceal = 0 时结论必须与改动前逐位相同"
        );
    }

    // ---- Q2 削顶 ----

    #[test]
    fn the_clip_meter_serves_only_complete_pages() {
        let m = ClipMeter::new();
        m.feed(1_000, &[0.5; 480]);
        assert_eq!(m.window(), None, "第一页还没满，绝不给半页的比率");
        m.feed(6_000, &[0.9; 480]);
        assert_eq!(m.window(), None);
        m.feed(11_001, &[0.5; 480]); // 越过 10s 边界 -> 翻页
        let w = m.window().expect("整页可读了");
        assert_eq!(w.samples, 3 * 480);
        assert_eq!(w.over, 480, "只有那一帧 0.9 越过了 0.8");
        assert!((w.peak - 0.9).abs() < 1e-3);
        assert!((w.ratio() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn excess_db_is_negative_below_the_knee_and_finite_in_silence() {
        let quiet = ClipWindow { span_s: 10.0, samples: 480, over: 0, peak: 0.4 };
        assert!(quiet.excess_db() < 0.0, "没碰到拐点就是负值");
        let silent = ClipWindow { span_s: 10.0, samples: 480, over: 0, peak: 0.0 };
        assert!(silent.excess_db().is_finite(), "全静音不能给出 -inf（JSON 会变 null）");
        // 两路 0.8 相加 = 1.6，正好 +6 dB
        let doubled = ClipWindow { span_s: 10.0, samples: 480, over: 480, peak: 1.6 };
        assert!((doubled.excess_db() - 6.0206).abs() < 1e-3);
    }

    // ---- 重复流 ----

    #[test]
    fn identical_streams_correlate_at_one() {
        let a: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin()).collect();
        let r = correlation(&a, &a).expect("有能量");
        assert!((r - 1.0).abs() < 1e-9);
    }

    /// **两路静音必须是 `None`，不是 1.0。** 否则任何空闲的双会话都会被永久
    /// 诬告成叠加 bug，真正的告警就淹没了。
    #[test]
    fn two_silent_streams_are_not_duplicates() {
        assert_eq!(correlation(&[0.0; 480], &[0.0; 480]), None);
        let a: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin()).collect();
        assert_eq!(correlation(&a, &[0.0; 480]), None, "一路静音也无从比较");
    }

    #[test]
    fn unrelated_content_correlates_far_below_the_threshold() {
        let a: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin()).collect();
        let b: Vec<f32> = (0..480).map(|i| (i as f32 * 0.79).cos()).collect();
        let r = correlation(&a, &b).unwrap();
        assert!(r.abs() < 0.3, "无关素材远在 0.98 之下, got {r}");
    }

    #[test]
    fn duplicate_needs_a_sustained_majority_not_one_lucky_frame() {
        let occasional = MixWindow {
            span_s: 10.0,
            max_contrib: 2,
            pair_ticks: 1000,
            dup_ticks: 50,
            corr_peak: Some(0.99),
        };
        assert!(!occasional.duplicate_suspect(), "5% 的巧合不是实锤");
        let sustained = MixWindow { dup_ticks: 990, ..occasional };
        assert!(sustained.duplicate_suspect());
        let never_paired = MixWindow { pair_ticks: 0, dup_ticks: 0, ..occasional };
        assert!(!never_paired.duplicate_suspect(), "从没两路同时求和过");
    }

    // ---- 合成 ----

    /// 规格 §6.2 点名的那条：Q1=优 / Q2=差 / Q3=优 必须给出「差」+
    /// `worst = "level"`。加权平均会把它稀释成「良」，**恰好掩盖用户要抓的
    /// 那个 bug**（两路重复流把声音削烂）。
    #[test]
    fn min_composition_does_not_dilute_a_single_bad_component() {
        let (g, worst, partial) = compose(Grade::Excellent, Some(Grade::Poor), Grade::Excellent);
        assert_eq!(g, Some(Grade::Poor));
        assert_eq!(worst, "level");
        assert!(!partial, "三分量齐全");
        // 对照：若用加权平均（这里手算一次）会得到「良」——这正是我们拒绝的做法
        let avg = (Grade::Excellent as u8 + Grade::Poor as u8 + Grade::Excellent as u8) as f32 / 3.0;
        assert!(avg > Grade::Good as u8 as f32 - 0.01, "平均值会谎报成 {avg:.2} ≈ 良");
    }

    #[test]
    fn min_composition_picks_each_limiting_component() {
        assert_eq!(compose(Grade::Fair, Some(Grade::Excellent), Grade::Good).1, "continuity");
        assert_eq!(compose(Grade::Good, Some(Grade::Fair), Grade::Excellent).1, "level");
        assert_eq!(compose(Grade::Good, Some(Grade::Good), Grade::Fair).1, "bandwidth");
        assert_eq!(
            compose(Grade::Excellent, Some(Grade::Excellent), Grade::Excellent),
            (Some(Grade::Excellent), "none", false)
        );
    }

    /// 平手时报最刺耳的那一项。
    #[test]
    fn ties_report_continuity_first() {
        assert_eq!(compose(Grade::Poor, Some(Grade::Poor), Grade::Poor).1, "continuity");
        assert_eq!(compose(Grade::Fair, Some(Grade::Fair), Grade::Excellent).1, "continuity");
    }

    /// **削顶还没测出来时，总等级不成立——不许拿在场分量的上界当结论。**
    ///
    /// 这条是本文件里最容易被「修好了」的假象骗过的一条，所以论证写全：
    ///
    /// 削顶页攒满要 10 s，流开头那一段 Q2 恒缺席。历史上有过两版写法——
    /// (a) 缺席填 `grade_clip(0.0) = Excellent`；(b) 缺席「不进 min」。
    /// `Grade::Excellent` 是 `Ord` 的最大值，于是
    /// `min(q1, Excellent, q3) ≡ min(q1, q3)`：**两版逐值相同**。(b) 只是额外把
    /// `partial` 标了出来，`grade` 那个字段一个字都没变，用户仍然在一条正在爆音
    /// 的流上读到「良好」。
    ///
    /// 所以这条测试断言的不是「等级等于某个值」，而是**等级不存在**。它同时对
    /// (a) 与 (b) 变红，因为两者都会在这里给出 `Some(_)`。
    #[test]
    fn an_unmeasured_component_leaves_the_grade_undecided() {
        // Q1 良好 / Q2 缺席 / Q3 优：真实等级只知道 ≤ 良好，可能是差。
        let (g, worst, partial) = compose(Grade::Good, None, Grade::Excellent);
        assert_eq!(g, None, "旧写法在这里给出 Some(Good) —— 那正是用户看到的『良好』");
        assert_eq!(worst, "none", "等级都还没定，谈不上谁拖后腿");
        assert!(partial, "少了一块板必须说出来");

        // 全优 + 缺一项同样不成立：「优」在这里只是上界，不是三分量的结论。
        assert_eq!(compose(Grade::Excellent, None, Grade::Excellent), (None, "none", true));
        // 一般也一样：区间 [差, 一般] 里没有一个等级可以拿出来说。
        assert_eq!(compose(Grade::Excellent, None, Grade::Fair), (None, "none", true));

        // 对照：同样的三个输入，只要 Q2 到场，等级立刻成立。缺席与在场的分界
        // 就是这一个 `Some`，不是任何阈值。
        assert_eq!(
            compose(Grade::Good, Some(Grade::Excellent), Grade::Excellent),
            (Some(Grade::Good), "continuity", false)
        );
    }

    /// **上界已经贴着地板时，缺席不再造成不确定 —— 等级照报。**
    ///
    /// 这是上一条的边界，也是它不至于矫枉过正的地方：真实等级落在
    /// `[FLOOR, bound]`，`bound == FLOOR` 时区间退化成一个点，缺的那块板再短也
    /// 改不了结论。把一个**已经确定是「差」**的流藏进「测量中」，等于拿不确定性
    /// 掩盖一个确定的坏消息——那和拿「良好」掩盖它是同一类错误，只是方向相反。
    #[test]
    fn a_grade_already_on_the_floor_survives_a_missing_component() {
        let (g, worst, partial) = compose(Grade::Poor, None, Grade::Excellent);
        assert_eq!(g, Some(Grade::Poor), "断续已经触底，削顶再差也压不下去");
        assert_eq!(worst, "continuity");
        assert!(partial, "等级确定，但木桶确实少了一块板 —— 两件事都要说");

        // 触底来自带宽时同理，且缺席的 Q2 依然不会被指认为拖后腿的那一项。
        let (g, worst, _) = compose(Grade::Excellent, None, Grade::Poor);
        assert_eq!((g, worst), (Some(Grade::Poor), "bandwidth"));
    }

    // ---- 阈值 ----

    #[test]
    fn conceal_thresholds_sit_where_the_spec_put_them() {
        assert_eq!(grade_conceal(0.0), Grade::Excellent);
        assert_eq!(grade_conceal(0.0019), Grade::Excellent); // 结构噪声底之下
        assert_eq!(grade_conceal(0.002), Grade::Good);
        assert_eq!(grade_conceal(0.009), Grade::Good); // G.107「透明线」之下
        assert_eq!(grade_conceal(0.01), Grade::Fair);
        assert_eq!(grade_conceal(0.03), Grade::Poor); // R 值跌破 70
    }

    #[test]
    fn clip_thresholds_sit_in_the_three_decade_vacuum() {
        // 正常素材：1e-4 量级
        assert_eq!(grade_clip(0.00005), Grade::Excellent);
        assert_eq!(grade_clip(0.0005), Grade::Good);
        assert_eq!(grade_clip(0.005), Grade::Fair);
        // 两路重复流相加：百分之几十
        assert_eq!(grade_clip(0.30), Grade::Poor);
    }

    #[test]
    fn bandwidth_maps_the_auto_ladder_rungs() {
        // `LADDER` 的四个采样率 48/32/24/16 kHz -> Nyquist 24k/16k/12k/8k。
        // ⚠ Q3 只看采样率，**位深不参与定级**：位深在本链路上从来不是限制项，
        // 给它编一条阈值等于制造一个永远是「优」的指标。
        assert_eq!(grade_bandwidth(24_000), Grade::Excellent);
        assert_eq!(grade_bandwidth(16_000), Grade::Good);
        assert_eq!(grade_bandwidth(12_000), Grade::Fair);
        assert_eq!(grade_bandwidth(8_000), Grade::Poor);
    }
}
