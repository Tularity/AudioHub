//! 逐级缓冲深度会计的公共类型（规格 spec-telemetry-ia.md §3）。
//!
//! 这里放的是**被测量的物理描述**，不是 IPC 报文：`audiohub-net`、`audiohubd`、
//! `audiohub-ipc` 三个 crate 都要用它，而 core 是它们共同的底座。IPC 侧的
//! `PipelineStage` / `PipelineLatency` 由 `audiohub-ipc` 再包一层（带 `String`
//! id 与 `drift_sps`），**因为那两样都不能在 10ms 节拍上产生**。
//!
//! ## 三条贯穿的硬约束（规格附录）
//!
//! 1. **绝不用 0 填补缺失分项**。`rate == 0` 即判该级读数无效，`ms()` 返回
//!    `None`，上层据此把总和也判为 `None`。用 0 填补会让蓝牙耳机（真实
//!    +150~250 ms）看起来和模拟输出一样好。
//! 2. RTT 只能当一段，永远不能当总数。
//! 3. **10ms 节拍线程上只允许常数次原子 load/store 与 `len()`/`occupied_len()`。**
//!    所以本文件里凡是节拍上要用的东西（`StageDepth`、`StageSlot`）全部
//!    `Copy` + 无分配；ms 换算、线性回归、字符串化统统留给报告线程。
//!
//! ## 为什么 `drop_mode` 是必填字段
//!
//! 规格 §0.2 已经证明：全链路四个 1 秒 FIFO 的**丢弃方向并不相同**。
//! 三个源侧 FIFO 是 `while len > cap { pop_front() }`（丢最旧），播放环与采集环
//! 是 `push_slice` 短写（丢最新）。两者**饱和时的深度读数一模一样**——都恰好等于
//! cap/rate——但听感天差地别：丢最旧是「恒定迟到但连续」，丢最新是「迟到并伴随
//! 断续」。标量读数在这里完全简并，只能靠 `drop_mode` + `dropped` 的增长区分。
//! 少了这个标签，遥测就只能告诉你「有一秒卡在某处」，没法告诉你那一秒是怎么卡的。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// 缓冲满了以后丢哪一头。见本文件头部「为什么 `drop_mode` 是必填字段」。
///
/// 序列化取值与前端 `app/frontend/src/lib/metrics.ts` 的
/// `StageReading.dropMode: 'oldest' | 'newest' | 'none'` **逐字一致**，
/// 中间不留映射表——映射表漏一条就是那一级静默显示错误语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DropMode {
    /// 满时丢最早的。听感：恒定迟到但连续。三个源侧 FIFO + PostMix 属此。
    Oldest,
    /// 满时丢最新的。听感：迟到 + 周期性断续。播放环 / 采集环属此
    /// （`push_slice` 写不下就短写，新采样根本没进去）。
    Newest,
    /// 不会丢（有界但从不饱和，或根本没有队列）。
    None,
}

/// 设备固有延迟这个数**是怎么来的**。
///
/// 它不是修饰，是判据：`Unavailable` 必须让总和变成 `None`，`Unreliable`
/// 必须让 UI 永远带「≥」。蓝牙 A2DP 真实延迟 150–250 ms 而系统 API 常只报
/// 20–30 ms，若把它当 `Api` 采信，读数会漂亮且完全错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatSource {
    /// 平台 API 给出的真值。
    Api,
    /// 库/平台的硬编码假设（如 cpal CoreAudio 的双缓冲假设）。
    Assumed,
    /// API 有值但已知少报（蓝牙 / HDMI / AirPlay）。
    Unreliable,
    /// 读不到。**绝不用 0 冒充 Api。**
    Unavailable,
}

/// 声卡自身的固有延迟。P0 阶段恒为 `Unavailable`（平台查询是 P1 的活），
/// 保留类型是为了让「缺项 ⇒ 总和 None ⇒ UI 带『≥』」这条链路现在就成立，
/// 而不是等 P1 再回来补一遍判空。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DevLatency {
    pub frames: u32,
    pub rate: u32,
    pub source: LatSource,
}

impl DevLatency {
    /// P0 的唯一取值：读不到。
    pub fn unavailable() -> DevLatency {
        DevLatency { frames: 0, rate: 0, source: LatSource::Unavailable }
    }

    /// `None` 时**不可**当 0 用（见文件头约束 1）。
    pub fn ms(&self) -> Option<f64> {
        match self.source {
            LatSource::Unavailable => None,
            _ if self.rate == 0 => None,
            _ => Some(self.frames as f64 * 1000.0 / self.rate as f64),
        }
    }
}

/// 九级测点的稳定 id（规格 §3.2 的表）。
///
/// `as_str()` 的取值与前端 `metrics.ts` 的 `LATENCY_STAGES[].id` **逐字一致**
/// （snake_case，不做大小写转写）。用枚举而不是 `&str` 承载，是因为节拍线程
/// 要把它塞进原子槽——那里只能放整数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StageId {
    /// 1. 声卡采集环（提供方，2 s 环，丢最新）
    CapRing = 1,
    /// 2. 采集设备固有（提供方，平台属性，P1）
    CapDev = 2,
    /// 3. 发送 FIFO（提供方，1 s，丢最旧）
    SrcFifo = 3,
    /// 3′. 虚拟扬声器环（提供方，500 ms）
    HalSpk = 4,
    /// 4. 组帧节拍（提供方，常数 5 ms）
    SendPace = 5,
    /// 5. 网络单程（两端，min-RTT/2）
    Network = 6,
    /// 6. 抖动缓冲（使用方）
    JitterBuf = 7,
    /// 7. 混音对齐缓冲（使用方，100 ms，丢最旧）
    PostMix = 8,
    /// 8. 播放环（使用方，1 s，丢最新）
    PlayRing = 9,
    /// 9. 播放设备固有（使用方，平台属性，P1）
    PlayDev = 10,
    /// 残差：实测总延迟 − Σ 各级。是**检验量**，不是分段（P1）。
    Residual = 11,
    /// 8′. 桥接虚拟声卡的播放环（使用方，1 s，丢最新）。
    ///
    /// 规格 §3.2 的表只列了 `play_ring` 一条尾级，因为它是照着「送本机默认输出」
    /// 那一条路径写的。但桥接流走的是**另一个** `AudioTx`（`engine.rs` 的
    /// `BridgeOut::tx`，每个桥一个，容量同样 = 设备速率 = 1 秒），它同样在送音频
    /// 的路上。不给它一级，桥接流的 `local_ms` 就只有 jitter_buf + post_mix，
    /// **静默漏掉整整一秒**——而「静默漏掉」正是这套遥测存在的理由。
    BridgeRing = 12,
    /// 8″. 虚拟麦克风环（使用方，500 ms，丢最新）。
    ///
    /// 模式 B 的接收流写进 HAL mic ring（`halbridge.rs` 的 `HAL_RING_FRAMES`
    /// = 24000 帧 = 500 ms），由驱动的 IOProc 交给选了这个虚拟麦克风的 App。
    /// 与 `hal_spk` 严格对称，只是方向相反：那一级是**我们读**驱动写的环，
    /// 这一级是**我们写**驱动读的环。丢弃在我们这一侧（`write` 满了短写），
    /// 所以与 `hal_spk` 不同，这一级的 `dropped` 是**可观测**的。
    HalMic = 13,
}

impl StageId {
    pub fn as_str(self) -> &'static str {
        match self {
            StageId::CapRing => "cap_ring",
            StageId::CapDev => "cap_dev",
            StageId::SrcFifo => "src_fifo",
            StageId::HalSpk => "hal_spk",
            StageId::SendPace => "send_pace",
            StageId::Network => "network",
            StageId::JitterBuf => "jitter_buf",
            StageId::PostMix => "post_mix",
            StageId::PlayRing => "play_ring",
            StageId::PlayDev => "play_dev",
            StageId::Residual => "residual",
            StageId::BridgeRing => "bridge_ring",
            StageId::HalMic => "hal_mic",
        }
    }

    pub fn from_code(code: u8) -> Option<StageId> {
        Some(match code {
            1 => StageId::CapRing,
            2 => StageId::CapDev,
            3 => StageId::SrcFifo,
            4 => StageId::HalSpk,
            5 => StageId::SendPace,
            6 => StageId::Network,
            7 => StageId::JitterBuf,
            8 => StageId::PostMix,
            9 => StageId::PlayRing,
            10 => StageId::PlayDev,
            11 => StageId::Residual,
            12 => StageId::BridgeRing,
            13 => StageId::HalMic,
            _ => return None,
        })
    }

    /// `as_str()` 的逆。上层拿到的是 IPC 那一层的 `String` id，要把并行尾级
    /// （见 `is_output_tail`）从串联链里认出来就得能反解析。
    /// **一张手写映射表就够漏一条**，所以这里直接遍历判别码，与 `as_str()`
    /// 共用同一份真值。
    pub fn from_id_str(s: &str) -> Option<StageId> {
        (1..=Self::MAX_CODE)
            .filter_map(StageId::from_code)
            .find(|id| id.as_str() == s)
    }

    /// 这一级是**并行尾级**吗？
    ///
    /// 一帧解码结果会被**同时**送进多个目的地（`engine.rs` 的 mixer：真实输出 /
    /// 桥接虚拟声卡 / 虚拟麦克风，三者互相独立而非互斥）。它们在时间上是**并联**
    /// 的支路，不是串联的两段——把两条尾级相加会凭空报出双倍延迟。
    /// 所以 Σ 的规则是：串联各级求和，**并行尾级取 max**。
    pub fn is_output_tail(self) -> bool {
        matches!(self, StageId::PlayRing | StageId::BridgeRing | StageId::HalMic)
    }

    /// 最大合法判别码。加新级时**必须**同步改这里，否则 `from_id_str` 会漏掉它。
    pub const MAX_CODE: u8 = 13;

    /// 漂移窗口等固定数组的下标空间（判别码直接当下标，故为 `MAX_CODE + 1`）。
    pub const COUNT: usize = Self::MAX_CODE as usize + 1;

    pub fn index(self) -> usize {
        self as usize
    }
}

/// 一级缓冲的瞬时存量。**可在 10ms 节拍上无分配产生**（全 `Copy`，id 是枚举）。
///
/// 基本定理（逐级会计严谨性的来源）：一级缓冲以已知速率 `rate` 排空时，此刻积压
/// `samples` 个样本 ⇒ 此刻进入该级的样本要等 `samples / rate` 秒才出得来。这是
/// 该级的**确切驻留时间**，不是估计——回调何时来、一次取多少，全部约掉。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageDepth {
    pub id: StageId,
    pub samples: u32,
    /// 该级容量；0 = 无界 / 不适用。
    pub capacity: u32,
    /// 该级**消费者**的标称速率(Hz)。**必填**：播放环走设备速率（可能 44.1k），
    /// 混用 48000 会引入 −8.8% 的系统性偏差。`rate == 0` 即判该级读数无效。
    pub rate: u32,
    /// 会话累计丢弃样本数（丢头 / 丢尾都算）。
    ///
    /// **`None` = 本进程观测不到这一级的丢弃，不是「没丢过」。** 典型是
    /// `hal_spk`：环满时写不进去的是**驱动**（coreaudiod 沙箱里的 IOProc），
    /// 丢弃计数在它那一侧，我们只有 `read_idx`/`write_idx` 两个下标。
    /// 报 0 会给出「这一级很健康」的假保证，而这恰恰是规格附录约束 1
    /// （绝不用 0 填补缺失分项）要消灭的失败形态——只不过这次缺的是丢弃数
    /// 而不是延迟数。
    pub dropped: Option<u64>,
    pub drop_mode: DropMode,
}

/// 组帧节拍（规格 §3.2 的级 4）的常数驻留：**半个 tick 的期望值 = 5 ms**。
///
/// ## 它为什么不是与 `src_fifo` 的重复计数
///
/// `src_fifo` 的深度读数回答的是「此刻进来的样本前面排着几个」，而 `tx_loop`
/// **不是连续排空**这个 FIFO 的——它每 10 ms 一次性取走整整 480 个样本。所以一个
/// 样本真正等到的是「下一次取走它的那个 tick」，比 `N/rate` 多出把连续到达量化到
/// 10 ms 边界的那一截。到达相位相对 tick 均匀分布 ⇒ 期望 5 ms。
///
/// ## 为什么它必须真的被发射出去
///
/// 这一级在枚举里声明了、在规格里编了号，却一个发布点都没有 ⇒ `local_ms`
/// **系统性短 5 ms**，而且没有任何字段标出它缺席。规格附录约束 1 反过来同样成立：
/// **不许静默缺席**。所以构造子放在这里（与枚举同文件、带断言测试），发布点在
/// `engine.rs` 的 `tx_loop`。
pub const SEND_PACE_MS: f64 = 5.0;

/// 48 kHz 下 5 ms = 240 个样本。换算走的是与其它级完全相同的
/// `samples / rate`——常数级不走另一条路径，是为了让「这一级也会被 `rate == 0`
/// 判无效」这类不变量对它同样成立。
const SEND_PACE_SAMPLES: u32 = 240;

impl StageDepth {
    /// 丢弃数可观测且当前为 0 的构造（测试与常量级用）。
    pub fn new(id: StageId, samples: u32, capacity: u32, rate: u32, drop_mode: DropMode) -> StageDepth {
        StageDepth { id, samples, capacity, rate, dropped: Some(0), drop_mode }
    }

    /// 级 4 `send_pace`：常数 5 ms（见 `SEND_PACE_MS` 上的论证）。
    ///
    /// - `capacity = 0`：这不是队列，没有「满」可言，所以 `saturated()` 恒 false。
    /// - `dropped = Some(0)`：节拍不丢样本。这是**真读数 0**，不是「观测不到」。
    pub fn send_pace() -> StageDepth {
        StageDepth {
            id: StageId::SendPace,
            samples: SEND_PACE_SAMPLES,
            capacity: 0,
            rate: 48_000,
            dropped: Some(0),
            drop_mode: DropMode::None,
        }
    }

    /// 驻留时长。`rate == 0` ⇒ `None`——**调用方不得当 0 用**（文件头约束 1）。
    pub fn ms(&self) -> Option<f64> {
        if self.rate == 0 {
            return None;
        }
        Some(self.samples as f64 * 1000.0 / self.rate as f64)
    }

    /// 深度是否贴着容量上限（≥95%）。饱和 + `dropped` 冻结 = 曾被一次卡顿灌满、
    /// 之后收支平衡但永远迟到；饱和 + `dropped` 持续增长 = 稳态产销速率失配。
    /// 这两种病理修法完全不同，靠单一延迟数字区分不了（规格 §3.3）。
    pub fn saturated(&self) -> bool {
        self.capacity > 0 && self.samples as u64 * 100 >= self.capacity as u64 * 95
    }
}

/// 节拍线程 → 报告线程的单级发布槽。
///
/// 只有原子 load/store，没有锁、没有分配、没有除法：写入方是 10ms 节拍
/// （文件头约束 3），读取方是每秒一次的报告/ticker 线程。
///
/// 六个字段不是原子快照——报告线程可能读到「新 samples + 旧 dropped」这种跨了
/// 一个节拍的组合。这被有意接受：两者相差至多 10 ms，而这些读数本来就要被 UI
/// 的 5 点中位数平滑，为它上一把 seqlock 只会把节拍污染进被测对象。
pub struct StageSlot {
    /// 0 = 本槽为空（该源没有这一级）。非 0 即 `StageId` 的判别码。
    id: AtomicU8,
    samples: AtomicU32,
    capacity: AtomicU32,
    rate: AtomicU32,
    dropped: AtomicU64,
    /// 上面那个 `dropped` 是不是真读数。false = 本级的丢弃本进程观测不到，
    /// 读取方必须给出 `None` 而不是 0（见 `StageDepth::dropped`）。
    dropped_known: AtomicBool,
    drop_mode: AtomicU8,
}

impl Default for StageSlot {
    fn default() -> Self {
        StageSlot::new()
    }
}

impl StageSlot {
    pub fn new() -> StageSlot {
        StageSlot {
            id: AtomicU8::new(0),
            samples: AtomicU32::new(0),
            capacity: AtomicU32::new(0),
            rate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            dropped_known: AtomicBool::new(false),
            drop_mode: AtomicU8::new(0),
        }
    }

    /// 节拍侧：发布一次读数。`None` 清空本槽（源消失时必须清，否则报告线程会
    /// 一直读到最后一次的陈旧深度，那是「静默缺项」的另一种形态）。
    pub fn store(&self, d: Option<StageDepth>) {
        match d {
            None => self.id.store(0, Ordering::Relaxed),
            Some(d) => {
                self.samples.store(d.samples, Ordering::Relaxed);
                self.capacity.store(d.capacity, Ordering::Relaxed);
                self.rate.store(d.rate, Ordering::Relaxed);
                self.dropped.store(d.dropped.unwrap_or(0), Ordering::Relaxed);
                self.dropped_known
                    .store(d.dropped.is_some(), Ordering::Relaxed);
                self.drop_mode.store(
                    match d.drop_mode {
                        DropMode::Oldest => 1,
                        DropMode::Newest => 2,
                        DropMode::None => 3,
                    },
                    Ordering::Relaxed,
                );
                // id 最后写：它是本槽的有效性开关，先写它会让读取方看到
                // 「有效 id + 上一轮的 samples」。
                self.id.store(d.id as u8, Ordering::Release);
            }
        }
    }

    /// 报告侧：取一次读数。`None` = 本槽为空。
    pub fn load(&self) -> Option<StageDepth> {
        let id = StageId::from_code(self.id.load(Ordering::Acquire))?;
        Some(StageDepth {
            id,
            samples: self.samples.load(Ordering::Relaxed),
            capacity: self.capacity.load(Ordering::Relaxed),
            rate: self.rate.load(Ordering::Relaxed),
            dropped: self
                .dropped_known
                .load(Ordering::Relaxed)
                .then(|| self.dropped.load(Ordering::Relaxed)),
            drop_mode: match self.drop_mode.load(Ordering::Relaxed) {
                1 => DropMode::Oldest,
                2 => DropMode::Newest,
                _ => DropMode::None,
            },
        })
    }
}

/// 一个媒体源能观测到的各级深度。
///
/// 固定长度、`Copy`、无分配：它在 10ms 节拍上被产生。目前最多两级
/// （MicSource = 采集环 + 发送 FIFO），留 2 槽即可。
pub type SourceDepths = [Option<StageDepth>; 2];

/// 空读数：源没有可观测的排队（如 `ToneSource` 是即时合成）。
pub const NO_DEPTHS: SourceDepths = [None, None];

// ---------------------------------------------------------------- 漂移

/// 30 s 窗口的深度斜率（样本/秒），对 `samples` 做最小二乘线性回归。
///
/// 用途（规格 §3.3，三种病理靠它区分）：
/// - `drift ≈ 0` + 饱和 + `dropped` 冻结 ⇒ 曾被一次卡顿灌满，之后收支平衡但**永远迟到**。
/// - `drift ≈ 0` + 饱和 + `dropped` 持续增长 ⇒ **稳态产销速率失配**（`Instant` 节拍 vs 设备时钟）。
/// - `drift` 持续同号且未饱和 ⇒ 正在走向饱和，尚未到达。
///
/// **不在节拍上跑**：由 1 s 的 ticker 喂点，报告线程读斜率。
pub struct DriftTracker {
    /// 每级一个 (秒, 样本数) 序列，按 StageId 判别码索引。
    win: Vec<Vec<(f32, f32)>>,
}

impl Default for DriftTracker {
    fn default() -> Self {
        DriftTracker::new()
    }
}

impl DriftTracker {
    /// 30 s 窗口 @ 1 s 喂点 = 31 点。
    pub const WINDOW_S: f32 = 30.0;

    pub fn new() -> DriftTracker {
        DriftTracker { win: (0..StageId::COUNT).map(|_| Vec::new()).collect() }
    }

    /// 喂一个采样点。`now_s` 是任意单调时基下的秒数（只用差值，所以常偏无所谓）。
    pub fn push(&mut self, now_s: f32, id: StageId, samples: u32) {
        let w = &mut self.win[id.index()];
        w.push((now_s, samples as f32));
        // 窗口外的点直接丢：一次早期抖动不该永远压着斜率。
        let cutoff = now_s - Self::WINDOW_S;
        let keep = w.iter().position(|&(t, _)| t >= cutoff).unwrap_or(w.len());
        if keep > 0 {
            w.drain(..keep);
        }
    }

    /// 该级不再存在时清掉它的历史，避免下一条同名会话继承上一条的斜率。
    pub fn clear(&mut self, id: StageId) {
        self.win[id.index()].clear();
    }

    /// 只保留 `present` 这几级的历史，其余全清。
    ///
    /// 给「**级的集合本身会变**」的那一侧用。发送流的分项槽是匿名的
    /// （`StageSlot` 空的时候只剩一个 0，说不出它上一轮装的是哪一级），所以那里
    /// 没法像接收侧那样对着已知 id 逐条 `clear`。而它恰恰是最需要清的一侧：
    /// `TxShared` 的生命周期比源长，默认输入设备一变就重建 `MicSource`，新源会
    /// 直接接着读旧源留下的、最长 30 s 的斜率——而 `drift_sps` 的全部用途就是
    /// 判「这一级在不在漂」，继承来的斜率会把一个刚开的干净流报成正在走向饱和。
    ///
    /// 传本 tick 真实在场的那几级即可：不在其中的一律当作已经消失。这比「槽空
    /// 才清」更强一档——源换成了另一种源（`cap_ring`+`src_fifo` 换成 `hal_spk`）
    /// 时槽并不空，只是换了 id，同样必须断掉旧历史。
    ///
    /// 参数直接收**槽的形状**（`Option`，空槽即 `None`）而不是一串紧凑的 id：
    /// 调用方手上本来就是一排 `StageSlot::load()` 的结果，让它先过滤一遍只会多
    /// 一次分配、多一个写错的机会。
    pub fn retain_only(&mut self, present: &[Option<StageId>]) {
        for (i, w) in self.win.iter_mut().enumerate() {
            if w.is_empty() {
                continue;
            }
            if !present.iter().flatten().any(|id| id.index() == i) {
                w.clear();
            }
        }
    }

    /// 最小二乘斜率，样本/秒。点数 < 3 或时间跨度 < 5 s ⇒ `None`
    /// （两点连线在这里不是趋势，是噪声；**绝不用 0 冒充「没有漂移」**）。
    pub fn slope(&self, id: StageId) -> Option<f64> {
        let w = &self.win[id.index()];
        if w.len() < 3 {
            return None;
        }
        let span = w[w.len() - 1].0 - w[0].0;
        if span < 5.0 {
            return None;
        }
        let n = w.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for &(t, v) in w {
            let (x, y) = (t as f64, v as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = n * sxx - sx * sx;
        if denom.abs() < 1e-9 {
            return None;
        }
        Some((n * sxy - sx * sy) / denom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规格 §6.2 点名的第一条：rate=44100 与 48000 下的换算，防 −8.8% 偏差。
    /// 播放环的容量就是**设备速率**，用 48000 硬算会把 44.1k 设备的 1 秒环报成
    /// 918 ms，正好是那 8.8%。
    #[test]
    fn ms_uses_the_stages_own_rate() {
        let at48 = StageDepth::new(StageId::PlayRing, 48_000, 48_000, 48_000, DropMode::Newest);
        let at44 = StageDepth::new(StageId::PlayRing, 44_100, 44_100, 44_100, DropMode::Newest);
        assert!((at48.ms().unwrap() - 1000.0).abs() < 1e-9);
        assert!((at44.ms().unwrap() - 1000.0).abs() < 1e-9);
        // 同样 44100 个样本，若误按 48k 换算就只有 918.75 ms
        let wrong = StageDepth::new(StageId::PlayRing, 44_100, 48_000, 48_000, DropMode::Newest);
        assert!((wrong.ms().unwrap() - 918.75).abs() < 1e-9);
    }

    /// rate == 0 是「这一级读不到」，不是「这一级 0 ms」。
    #[test]
    fn a_rateless_stage_is_none_not_zero() {
        let s = StageDepth::new(StageId::PlayRing, 480, 48_000, 0, DropMode::Newest);
        assert_eq!(s.ms(), None);
    }

    /// 设备项读不到时同理——这条是「蓝牙耳机看起来和模拟输出一样好」的防线。
    #[test]
    fn unavailable_device_latency_is_none_not_zero() {
        assert_eq!(DevLatency::unavailable().ms(), None);
        let real = DevLatency { frames: 512, rate: 48_000, source: LatSource::Api };
        assert!((real.ms().unwrap() - 10.6666).abs() < 1e-3);
    }

    /// 规格 §0.2 的核心：两种丢弃方向在**深度上完全简并**，只有标签能区分。
    /// 这条测试存在的意义是把那句话钉死成断言——将来谁把 drop_mode 摘掉当
    /// 「冗余字段」，这里会红。
    #[test]
    fn drop_modes_are_indistinguishable_by_depth_alone() {
        let src_fifo = StageDepth::new(StageId::SrcFifo, 48_000, 48_000, 48_000, DropMode::Oldest);
        let play_ring = StageDepth::new(StageId::PlayRing, 48_000, 48_000, 48_000, DropMode::Newest);
        assert_eq!(src_fifo.ms(), play_ring.ms(), "深度读数一模一样");
        assert!(src_fifo.saturated() && play_ring.saturated(), "饱和判定也一模一样");
        assert_ne!(
            src_fifo.drop_mode, play_ring.drop_mode,
            "唯一的区别在 drop_mode：丢最旧=恒定迟到但连续，丢最新=迟到+断续"
        );
    }

    #[test]
    fn saturation_needs_a_capacity() {
        // 无界的级永远不算饱和，哪怕深度很大
        let unbounded = StageDepth::new(StageId::PostMix, 99_999, 0, 48_000, DropMode::None);
        assert!(!unbounded.saturated());
        let half = StageDepth::new(StageId::PostMix, 2_400, 4_800, 48_000, DropMode::Oldest);
        assert!(!half.saturated());
        let full = StageDepth::new(StageId::PostMix, 4_800, 4_800, 48_000, DropMode::Oldest);
        assert!(full.saturated());
    }

    /// 槽的 id 是有效性开关：清空后必须读不到，而不是读到上一轮的陈旧深度。
    #[test]
    fn a_cleared_slot_reads_empty_not_stale() {
        let slot = StageSlot::new();
        assert_eq!(slot.load(), None);
        let d = StageDepth {
            id: StageId::SrcFifo,
            samples: 4_800,
            capacity: 48_000,
            rate: 48_000,
            dropped: Some(7),
            drop_mode: DropMode::Oldest,
        };
        slot.store(Some(d));
        assert_eq!(slot.load(), Some(d));
        slot.store(None);
        assert_eq!(slot.load(), None, "源消失后必须报『没有这一级』，不是报旧值");
    }

    /// 「丢弃数观测不到」必须原样穿过原子槽 —— 若它退化成 0，UI 会给出
    /// 「这一级很健康」的假保证。hal_spk 正是这种级：环满时写不进去的是驱动，
    /// 计数在它那一侧。
    #[test]
    fn an_unobservable_drop_count_stays_none_through_the_slot() {
        let slot = StageSlot::new();
        slot.store(Some(StageDepth {
            id: StageId::HalSpk,
            samples: 19_200,
            capacity: 24_000,
            rate: 48_000,
            dropped: None,
            drop_mode: DropMode::Newest,
        }));
        let got = slot.load().expect("槽已填");
        assert_eq!(got.dropped, None, "观测不到就得是 None，不能变成 0");
        assert_eq!(got.samples, 19_200);
        // ...而 0 是一个真读数，必须与 None 区分得开
        slot.store(Some(StageDepth {
            id: StageId::HalSpk,
            samples: 0,
            capacity: 24_000,
            rate: 48_000,
            dropped: Some(0),
            drop_mode: DropMode::Newest,
        }));
        assert_eq!(slot.load().unwrap().dropped, Some(0), "真的 0 不能变成 None");
    }

    #[test]
    fn drift_needs_enough_points_and_span() {
        let mut t = DriftTracker::new();
        assert_eq!(t.slope(StageId::SrcFifo), None, "一个点都没有");
        t.push(0.0, StageId::SrcFifo, 0);
        t.push(1.0, StageId::SrcFifo, 480);
        assert_eq!(t.slope(StageId::SrcFifo), None, "两点连线是噪声，不是趋势");
        for i in 2..=10 {
            t.push(i as f32, StageId::SrcFifo, 480 * i);
        }
        let s = t.slope(StageId::SrcFifo).expect("10 秒跨度足够");
        assert!((s - 480.0).abs() < 1e-6, "每秒涨 480 样本 = 1% 速率失配, got {s}");
    }

    /// 稳态（深度不动）必须给出 0 斜率，而不是 None——「测到了，就是不漂」与
    /// 「没测到」是两个不同的结论。
    #[test]
    fn steady_depth_reports_zero_drift() {
        let mut t = DriftTracker::new();
        for i in 0..=10 {
            t.push(i as f32, StageId::PlayRing, 48_000);
        }
        assert!(t.slope(StageId::PlayRing).unwrap().abs() < 1e-6);
    }

    /// 30 s 之前的点必须落出窗口，否则一次早期尖峰会永远压着斜率。
    #[test]
    fn drift_window_forgets_old_points() {
        let mut t = DriftTracker::new();
        // 一段陡峭的早期上升
        for i in 0..10 {
            t.push(i as f32, StageId::JitterBuf, 1_000 * i);
        }
        // 然后 40 秒完全平稳
        for i in 0..=40 {
            t.push(40.0 + i as f32, StageId::JitterBuf, 9_000);
        }
        let s = t.slope(StageId::JitterBuf).unwrap();
        assert!(s.abs() < 1e-6, "早期上升已滑出 30s 窗口，斜率应归零, got {s}");
    }

    /// **源被换掉之后，新源不许继承旧源的斜率。**
    ///
    /// `TxShared` 的生命周期比源长，所以「槽空了」并不等于「这条流没了」——
    /// 默认输入设备一变就重建 `MicSource`，会话表里那个 `drift` 却原封不动。
    /// 少了这一步，一条刚开的干净流会带着上一条流最长 30 s 的斜率上报
    /// 「正在走向饱和」，而 `drift_sps` 的全部用途就是判这件事。
    #[test]
    fn a_replaced_source_does_not_inherit_the_previous_slope() {
        let mut t = DriftTracker::new();
        // 旧源：发送 FIFO 一路涨到饱和，30 s 里 +480 样本/秒
        for i in 0..=30 {
            t.push(i as f32, StageId::SrcFifo, 480 * i);
        }
        let before = t.slope(StageId::SrcFifo).expect("旧源确实在漂");
        assert!((before - 480.0).abs() < 1.0, "前提：旧源斜率 ≈ +480, got {before}");

        // 源没了：这一 tick 一个槽都没在场。
        t.retain_only(&[None, None, None]);
        assert_eq!(
            t.slope(StageId::SrcFifo),
            None,
            "历史必须断掉 —— 否则新源开头就背着旧源的斜率"
        );

        // 新源接上，稳态不漂：斜率只能反映新源自己的那几个点。
        for i in 31..=45 {
            t.push(i as f32, StageId::SrcFifo, 9_600);
        }
        let after = t.slope(StageId::SrcFifo).expect("新源攒够点了");
        assert!(after.abs() < 1e-6, "新源是平的，斜率必须 ≈ 0, got {after}");
    }

    /// 槽没空、只是**换了 id** 时同样要断历史：那也是换了一个源。
    #[test]
    fn switching_to_a_different_kind_of_source_also_breaks_the_history() {
        let mut t = DriftTracker::new();
        for i in 0..=30 {
            t.push(i as f32, StageId::SrcFifo, 480 * i);
            t.push(i as f32, StageId::CapRing, 100 * i);
        }
        assert!(t.slope(StageId::SrcFifo).is_some() && t.slope(StageId::CapRing).is_some());
        // 麦克风源换成了 HAL 扬声器源：两槽都还在，装的却是另一级。
        t.retain_only(&[Some(StageId::HalSpk), None, Some(StageId::SendPace)]);
        assert_eq!(t.slope(StageId::SrcFifo), None);
        assert_eq!(t.slope(StageId::CapRing), None);
    }

    /// 在场的级**不能**被顺手清掉——否则每秒清一次，`drift_sps` 永远是 `None`，
    /// 三种病理（灌满/失配/正在漂）就全都区分不出来了。
    #[test]
    fn retain_only_keeps_the_stages_that_are_still_there() {
        let mut t = DriftTracker::new();
        for i in 0..=30 {
            t.push(i as f32, StageId::SrcFifo, 480 * i);
            t.push(i as f32, StageId::HalSpk, 24_000);
        }
        t.retain_only(&[Some(StageId::SrcFifo), Some(StageId::HalSpk), None]);
        assert!(t.slope(StageId::SrcFifo).is_some(), "还在场的级必须留着历史");
        assert!(t.slope(StageId::HalSpk).is_some());
    }

    /// 从 `app/frontend/src/lib/metrics.ts` 里把 `LATENCY_STAGES` 的 id 抠出来。
    ///
    /// 读不到就 panic，不返回空表：**「文件不见了」必须与「表是空的」区分开**，
    /// 否则把 metrics.ts 改名或挪走会让下面那条断言退化成 `[] == []` 而全绿——
    /// 那正是它要防的那类静默失效，只不过换了个地方发生。
    fn frontend_stage_ids() -> Vec<String> {
        const REL: &str = "/../../app/frontend/src/lib/metrics.ts";
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/frontend/src/lib/metrics.ts");
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| {
            panic!("读不到前端的级表 {REL}（{e}）。文件被改名/挪走了就把这条测试一起更新，\
                   不要让它悄悄退化成一条恒真断言")
        });
        let start = src
            .find("export const LATENCY_STAGES")
            .expect("metrics.ts 里找不到 LATENCY_STAGES —— 契约的另一半没了");
        let body = &src[start..];
        let end = body.find("\n];").expect("LATENCY_STAGES 没有收尾的 `];`");
        let body = &body[..end];

        let mut ids = Vec::new();
        // 形如 `{ id: 'play_ring', ... }`。引号两种都认（Prettier 配置换过就会变）。
        for piece in body.split("id:").skip(1) {
            let piece = piece.trim_start();
            let quote = match piece.chars().next() {
                Some(c @ ('\'' | '"')) => c,
                _ => continue, // `id:` 出现在别处（类型声明里就有一个）
            };
            let rest = &piece[quote.len_utf8()..];
            let Some(close) = rest.find(quote) else { continue };
            ids.push(rest[..close].to_string());
        }
        ids
    }

    /// **前端的级表与 Rust 的 `StageId` 必须逐条对齐。**
    ///
    /// 这条测试此前的形态是把前端那张表**手抄一份进 Rust 的字面量数组**，再断言
    /// 「我们发射的每一级都在这份手抄件里」。那样的断言永远不可能为它命名的那个
    /// 漂移变红：抄件和被抄件是同一只手写的，前端删一级、加一级、改一个字，Rust
    /// 这边一无所知。事实上它就带着「前端缺三级」的注释一路全绿。
    ///
    /// 现在直接去读那个文件。两个方向都断言：
    /// - Rust 有、前端没有 ⇒ 那一级在明细里静默显示「未知」，永远不会有数据。
    /// - 前端有、Rust 没有 ⇒ 一行永远拿不到读数的死行（打错字最常见的形态）。
    #[test]
    fn the_frontend_stage_table_matches_the_rust_enum_exactly() {
        let mut theirs = frontend_stage_ids();
        let mut ours: Vec<String> = (1..=StageId::MAX_CODE)
            .filter_map(StageId::from_code)
            .map(|id| id.as_str().to_string())
            .collect();
        assert_eq!(ours.len(), StageId::MAX_CODE as usize, "枚举自己先要是完整的");

        theirs.sort();
        ours.sort();
        assert_eq!(
            theirs, ours,
            "metrics.ts 的 LATENCY_STAGES 与 StageId 对不上。\
             Rust 多出来的级在界面上永远显示「未知」；前端多出来的行永远拿不到读数。"
        );
    }

    /// id 的字符串取值是与前端 metrics.ts 的契约，改动即断线。
    #[test]
    fn stage_ids_match_the_frontend_contract() {
        assert_eq!(StageId::CapRing.as_str(), "cap_ring");
        assert_eq!(StageId::SrcFifo.as_str(), "src_fifo");
        assert_eq!(StageId::HalSpk.as_str(), "hal_spk");
        assert_eq!(StageId::SendPace.as_str(), "send_pace");
        assert_eq!(StageId::Network.as_str(), "network");
        assert_eq!(StageId::JitterBuf.as_str(), "jitter_buf");
        assert_eq!(StageId::PostMix.as_str(), "post_mix");
        assert_eq!(StageId::PlayRing.as_str(), "play_ring");
        assert_eq!(StageId::PlayDev.as_str(), "play_dev");
        assert_eq!(StageId::BridgeRing.as_str(), "bridge_ring");
        assert_eq!(StageId::HalMic.as_str(), "hal_mic");
        // 判别码 <-> 枚举的往返，StageSlot 的正确性押在它上面
        for code in 1..=StageId::MAX_CODE {
            let id = StageId::from_code(code).expect("1..=MAX_CODE 都是合法级");
            assert_eq!(id as u8, code);
            // 字符串往返：`from_id_str` 是靠遍历判别码实现的，漏一条这里就红
            assert_eq!(StageId::from_id_str(id.as_str()), Some(id));
            assert!(id.index() < StageId::COUNT, "判别码要能直接当 DriftTracker 的下标");
        }
        assert_eq!(StageId::from_code(0), None, "0 保留给『本槽为空』");
        assert_eq!(StageId::from_code(StageId::MAX_CODE + 1), None);
        assert_eq!(StageId::from_id_str("no_such_stage"), None);
    }

    /// 级 4 `send_pace` 是常数 5 ms。这条断言存在的意义：这一级曾经在枚举里
    /// 声明、在规格里编号，却**一个发布点都没有**，于是 `local_ms` 系统性短
    /// 5 ms 且没有任何字段标出它缺席。构造子与断言放在一起，是为了让「删了它」
    /// 和「忘了发它」都变成编译期/测试期可见的事件。
    #[test]
    fn send_pace_is_a_five_millisecond_constant() {
        let p = StageDepth::send_pace();
        assert_eq!(p.id, StageId::SendPace);
        assert_eq!(p.ms(), Some(SEND_PACE_MS));
        assert!(!p.saturated(), "它不是队列，永远不该被判饱和");
        assert_eq!(p.dropped, Some(0), "节拍不丢样本：这是真读数 0，不是观测不到");
        assert_eq!(p.drop_mode, DropMode::None);
    }

    /// 并行尾级不可相加：一帧解码结果**同时**进真实输出与桥接虚拟声卡，
    /// 两条 1 秒环相加会报出 2 秒的假延迟。
    #[test]
    fn the_three_output_tails_are_parallel_not_serial() {
        assert!(StageId::PlayRing.is_output_tail());
        assert!(StageId::BridgeRing.is_output_tail());
        assert!(StageId::HalMic.is_output_tail());
        for id in [
            StageId::CapRing,
            StageId::CapDev,
            StageId::SrcFifo,
            StageId::HalSpk,
            StageId::SendPace,
            StageId::Network,
            StageId::JitterBuf,
            StageId::PostMix,
            StageId::PlayDev,
            StageId::Residual,
        ] {
            assert!(!id.is_output_tail(), "{} 是串联级，必须参与求和", id.as_str());
        }
    }

    /// 两条新尾级必须能原样穿过原子槽（它们的 dropped 语义相反：桥接环与
    /// 虚拟麦克风环的丢弃都发生在**我们这一侧**，所以是可观测的 `Some`，
    /// 与 `hal_spk` 的 `None` 正好构成对照）。
    #[test]
    fn the_new_tail_stages_round_trip_through_the_slot() {
        let slot = StageSlot::new();
        for (id, cap) in [(StageId::BridgeRing, 48_000u32), (StageId::HalMic, 24_000)] {
            let d = StageDepth {
                id,
                samples: cap / 2,
                capacity: cap,
                rate: 48_000,
                dropped: Some(3),
                drop_mode: DropMode::Newest,
            };
            slot.store(Some(d));
            assert_eq!(slot.load(), Some(d), "{} 没能原样穿过槽", id.as_str());
        }
    }

    /// serde 取值必须与前端字面量联合类型逐字一致（不留映射表）。
    #[test]
    fn serde_values_match_the_frontend_literals() {
        let j = |v: &DropMode| serde_json::to_string(v).unwrap();
        assert_eq!(j(&DropMode::Oldest), "\"oldest\"");
        assert_eq!(j(&DropMode::Newest), "\"newest\"");
        assert_eq!(j(&DropMode::None), "\"none\"");
        assert_eq!(serde_json::to_string(&LatSource::Unreliable).unwrap(), "\"unreliable\"");
        assert_eq!(serde_json::to_string(&LatSource::Unavailable).unwrap(), "\"unavailable\"");
    }
}
