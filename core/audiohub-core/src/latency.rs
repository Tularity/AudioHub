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

use std::collections::VecDeque;
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

impl LatSource {
    /// 这个读数够不够格把总和从「≥ N ms」升级成一个**精确**的端到端物理量？
    ///
    /// **只有 `Api`。** 另外三个都不行，而且理由各不相同：
    ///
    /// | 取值 | 为什么仍是下限 |
    /// |---|---|
    /// | `Assumed` | 按模型算的，不是读来的 |
    /// | `Unreliable` | API 答了，但已知差着一个数量级。**Windows 上实测低报 4.2 倍**：`GetDevicePeriod` 报 10.00 ms，同一端点写到播 41.92 ms（`docs/spec-playdev-measurement.md` §3） |
    /// | `Unavailable` | 根本没有数，`ms()` 已经是 `None` |
    ///
    /// 存在的理由是它**将来会被误用**。`audiohubd` 那边接设备固有延迟的说明里
    /// 曾写着「设备项齐全后它不再是下限」——照字面做就会把 Windows 那 10 ms
    /// 当真值加进去，报出「121 ms」而真值 153 ms，且不带「≥」。
    /// 那正是 `plan.md` §7.6 第 6 条（不许拿不完整的量冒充端到端物理量）
    /// 要杀死的形态。把判据写成一个有名字的谓词，比写在注释里更难绕过。
    ///
    /// ⚠ 它是**必要条件不是充分条件**：即使两侧都 `Api`，还得先让求和真的把
    /// 设备项加进去（今天的 `compose_sum_ms` 没加）。少做那一步就升级，
    /// 是同一个谎的另一种拼法。
    pub fn is_exact(self) -> bool {
        matches!(self, LatSource::Api)
    }
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

/// 置信区间取几倍标准误。3σ ≈ 正态下 99.7%。
///
/// 取 3 而不是常见的 2，是因为这个字段的读者是**诊断判据**而不是统计报告：
/// 一个假阳性会把「稳态但永远迟到」误判成「正在走向饱和」，而这两种病理的
/// 修法完全不同（规格 §3.3）。
pub const DRIFT_T: f64 = 3.0;

/// 这个窗口要算「分辨得出漂移」，3σ 半宽必须紧到这个值以内（样本/秒）。
///
/// ## 这个数是怎么定的（2026-08-02 重新标定）
///
/// 它不是「小到不用管」的经验值，是**这一级真实病灶的大小**：实测 `hal_spk`
/// 9 小时的平均涨速是 **+0.34 样本/秒**（≈7 ppm @48k），9 小时积出 253 ms。
/// 「测到了，就是不漂」这句话若要成立，就必须**排除得掉 0.34 样本/秒**——
/// 半宽比它还宽时，「不漂」与「正在走向饱和」在这份数据里根本不可分，
/// 唯一诚实的输出是 `None`。所以门槛必须 ≤ 0.34，取 **0.3**（≈6.3 ppm）。
///
/// ## 为什么此前是 1.0，现在能收紧
///
/// 1.0 是照着**未剔除量化噪声**的读数定的：那时 `hal_spk` 的 3σ 半宽是
/// 6.7 样本/秒，门槛定在 0.3 会让**所有**级都报 `None`，等于关掉这个指标。
/// `DepthInterp` 把写块量化（±384 样本）从读数里减掉之后，噪声底降了一到两个
/// 数量级，0.3 这个门槛才有东西够得着——**先降噪声底，再收门槛**，顺序反了
/// 就只是把指标关掉。
///
/// ⚠ 收紧对**没有**插值的级同样生效，这是有意的：一条 3σ 半宽 0.5 样本/秒的
/// 序列在旧门槛下会报「≈0」，而它其实排除不掉 +0.34 的真实病灶——那句「不漂」
/// 是假保证。宁可报 `None`。
pub const DRIFT_RESOLUTION_SPS: f64 = 0.3;

/// 修正量的绝对上限（样本）。100 ms @48k。
///
/// 快照坏掉（时间戳陈旧、速率字段没填、时钟回跳）时，`since_* × rate` 会算出
/// 一个天文数字，而它会被当成真读数直接进回归窗口——**一个错的修正比不修正更坏**。
/// PipeWire 在同一位置也钳：`alsa-pcm.c` 用 `snd_pcm_htimestamp` 精修 delay 时，
/// 只在 `SPA_ABS(diff) < threshold*3` 时采纳，且修正量钳在 ±threshold。
///
/// 取 100 ms 是因为这个修正项的物理含义是「一个整块里还没写出来的那一截」，
/// 而没有任何音频设备的一次 IO 块有 100 ms 那么长（典型 5–21 ms）。
const INTERP_CLAMP_SAMPLES: f64 = 4_800.0;

/// 一次深度读数的**量化修正项**：把两侧的整块阶梯插值回连续位置。
///
/// # 这不是滤波，是把测量自身的量化减掉
///
/// 一级缓冲的深度 = 生产侧累计写入 − 消费侧累计读出，而**两侧都是阶梯**：
/// 驱动一次 `DoIOOperation` 写整块 B 个样本，`tx_loop` 一次读整块 480 个样本。
/// 于是任意时刻读到的深度是
///
/// ```text
/// D_obs(t) = D_连续(t) − (生产侧本块还没写出来的那一截)
///                      + (消费侧本块还没读走的那一截)
/// ```
///
/// 那两截各自在 `[0, 一个块)` 里随时间锯齿状滑动。实测 `hal_spk` 上它就是
/// ±384 样本（`docs/investigate-hal-residency.md` §1.2 的 86 个小步，σ≈111），
/// 而要探测的真实漂移只有 +0.34 样本/秒——**噪声底比效应大 20 倍**。
///
/// 此前把这归因为「窗口太短」，提议把 30 s 拉到 5–10 分钟。**那个归因是错的**：
/// 拉长窗口只能按 √N 压它（要压 20 倍得 400 倍的点，即 3 小时以上），而它根本
/// 不是随机噪声，是**确定性的量化**——知道「上一次整块发生在多久以前」就能
/// 直接把它减掉，一步到位、不需要更长的窗口。
///
/// 三个独立实现都这么做（`docs/research-latency-prior-art.md` 附录 C-DLL §C.4.3、
/// 附录 D §D.2-③c）：
/// - zita-ajbridge：`err = k + (_k_a1 - _k_a0) * d1 / d2 + ...`
///   —— 用两个带时间戳的计数快照对当前周期做线性插值；
/// - PipeWire `alsa-pcm.c`：用 `snd_pcm_htimestamp` 精修 delay；
/// - PipeWire `node-driver.c:414-419`，注释原文：
///   *"time_since_nsec estimates the delay, and subtracts that estimation, …
///   which increases the control loop stability."*
///
/// # ⚠ 符号推导（写反了噪声会翻倍，不是减半）
///
/// 记生产侧连续位置 `W_c(t) = r_w · t`，阶梯位置 `W(t) = B·floor(t·r_w/B)`。
/// 最近一次写块发生在 `t_w ≤ t`，于是
/// `W_c(t) − W(t) = r_w · (t − t_w)` —— **阶梯永远落后于连续线**。
/// 深度 = W − R，所以
///
/// ```text
/// D_连续 = D_obs + r_w·(t − t_w) − r_r·(t − t_r)
///                 ^^^^^^^^^^^^^^   ^^^^^^^^^^^^^^
///                 生产侧：加       消费侧：减
/// ```
///
/// 记住方向就一句话：**「生产侧欠着的补上去，消费侧欠着的扣回来」**。
/// 两项都写成加，噪声不但不消，还会变成原来的两倍——
/// `interpolating_with_the_wrong_sign_makes_it_worse` 这条测试就是钉这个的。
///
/// # 只填得出一侧怎么办
///
/// 填一侧、另一侧留 0 即可（`producer()` / `consumer()`），少减掉一截量化总比
/// 减错方向好。字段非有限值 / 负值 / 速率为 0 一律当 0 处理——**缺项不许用
/// 猜出来的数填补**，与文件头约束 1 同一条纪律。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DepthInterp {
    /// 读数时刻 − 生产侧最近一次整块写入的时刻（秒，≥0）。
    pub since_write_s: f32,
    /// 生产侧实测写入速率（样本/秒）。零 / 非有限 ⇒ 本项不参与修正。
    pub writer_sps: f32,
    /// 读数时刻 − 消费侧最近一次整块读出的时刻（秒，≥0）。
    pub since_read_s: f32,
    /// 消费侧实测读出速率（样本/秒）。零 / 非有限 ⇒ 本项不参与修正。
    pub reader_sps: f32,
}

impl DepthInterp {
    /// 拿不到任何时间快照：退回旧行为（读数原样进窗口）。
    pub const NONE: DepthInterp =
        DepthInterp { since_write_s: 0.0, writer_sps: 0.0, since_read_s: 0.0, reader_sps: 0.0 };

    /// 只知道生产侧的写块时刻（`hal_spk` / `play_ring` 这类**我们读、别人写**的级）。
    pub fn producer(since_write_s: f32, writer_sps: f32) -> DepthInterp {
        DepthInterp { since_write_s, writer_sps, ..DepthInterp::NONE }
    }

    /// 只知道消费侧的读块时刻（`hal_mic` / `bridge_ring` 这类**我们写、别人读**的级）。
    pub fn consumer(since_read_s: f32, reader_sps: f32) -> DepthInterp {
        DepthInterp { since_read_s, reader_sps, ..DepthInterp::NONE }
    }

    /// 补上另一侧。两侧都填才可能把量化减到 0。
    pub fn with_consumer(mut self, since_read_s: f32, reader_sps: f32) -> DepthInterp {
        self.since_read_s = since_read_s;
        self.reader_sps = reader_sps;
        self
    }

    /// 这份快照一个可用项都没有吗？用来把「插值过的读数」与「原始读数」分开
    /// （见 `DriftFit::interpolated`）。
    pub fn is_none(&self) -> bool {
        self.term(self.since_write_s, self.writer_sps) == 0.0
            && self.term(self.since_read_s, self.reader_sps) == 0.0
    }

    /// 加到读数上的修正量（样本，可正可负）。符号推导见结构体文档。
    pub fn correction_samples(&self) -> f64 {
        self.term(self.since_write_s, self.writer_sps)
            - self.term(self.since_read_s, self.reader_sps)
    }

    /// 单侧的「欠着没走完的那一截」。非法输入一律 0，且钳在
    /// `INTERP_CLAMP_SAMPLES` 以内。
    fn term(&self, dt_s: f32, rate_sps: f32) -> f64 {
        if !dt_s.is_finite() || !rate_sps.is_finite() || dt_s <= 0.0 || rate_sps <= 0.0 {
            return 0.0;
        }
        (dt_s as f64 * rate_sps as f64).min(INTERP_CLAMP_SAMPLES)
    }
}

/// 一次回归的完整结果。`slope_sps` 单独拿出去是危险的——必须与
/// `stderr_sps` 一起看才知道那个数字是不是噪声。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftFit {
    /// 最小二乘斜率，样本/秒。
    pub slope_sps: f64,
    /// 斜率的标准误，样本/秒。`0.0` = 完美拟合（残差为 0）。
    pub stderr_sps: f64,
    /// 参与拟合的点数。
    pub n: usize,
    /// 首末点的时间跨度（秒）。
    pub span_s: f32,
    /// 窗口里的点**是否全部**做过量化修正（见 `DepthInterp`）。
    ///
    /// 它不参与任何判据，只回答「这条读数的噪声底是被剔除过的，还是原始的」。
    /// 混着来（换源、快照中途失效）时为 `false`——半段修正过、半段没修过的
    /// 序列会在接缝处凭空多出一个台阶，把它当「插值过」汇报是误导。
    pub interpolated: bool,
}

impl DriftFit {
    /// 这个窗口对漂移的分辨力：3σ 置信半宽（样本/秒）。越小越有话语权。
    pub fn resolution_sps(&self) -> f64 {
        DRIFT_T * self.stderr_sps
    }

    /// 这个读数说得出话吗？三分，不是两分——**「测了，不漂」与「没测出来」
    /// 是两个不同的结论**，合并它们正是这次要修的那个错。
    ///
    /// 1. 斜率越过自己的 3σ 界 ⇒ 测到了漂移，报这个数。
    /// 2. 没越界，但**界本身很紧**（≤ `DRIFT_RESOLUTION_SPS`）⇒ 测到了
    ///    「不漂」，报 ≈0。一条只抖 ±1 个样本的稳态队列属此。
    /// 3. 没越界且**界很松** ⇒ 这个窗口分辨不出来，`slope()` 报 `None`。
    ///
    /// 情形 3 的实例（实测，`docs/investigate-hal-residency.md` §2.3）：mac 的
    /// `hal_spk` 在 30 s 窗口 + 1 Hz 采样下 N≈31，叠着 ±192 samples 的写块相位
    /// 噪声（σ≈111）⇒ `stderr ≈ 111/49.8 ≈ 2.2`，3σ 半宽 ≈ **6.7 样本/秒**；
    /// 而要探测的真实涨速只有 **+0.34 样本/秒**。噪声底比效应大 20 倍，
    /// 此时报出去的 −2.07…+3.10 连符号都是随机的。
    ///
    /// ⚠ 情形 3 是**可以被治好的**，而不是这一级的宿命：那 ±192 是写块量化，
    /// 喂点时带上 `DepthInterp` 就能直接减掉，同一条序列会从情形 3 落回情形 1/2。
    /// 拉长窗口只能按 √N 压它，是当初那条错误归因的产物。
    pub fn resolved(&self) -> bool {
        self.slope_sps.abs() >= self.resolution_sps()
            || self.resolution_sps() <= DRIFT_RESOLUTION_SPS
    }
}

// ------------------------------------------------- 阶跃累积检测（跳变才是病因）

/// 鲁棒噪声尺度用最近多少个 `|Δ|` 估计。
///
/// 用**中位数**而不是均值。被判为阶跃的那些 `Δ` 本来就不进这个窗口，所以要防的
/// 不是它们，而是**大而没越线**的那一批：现实里就是接近但未越过 100 ms 那条线的
/// 卡顿（`engine.rs` 的门限），以及设备重配一类的中等位移。它们合法地进了窗口，
/// 均值会被少数几个这样的值拽高一大截，门限跟着抬起来，于是**下一次真正越线的
/// 跳变反而被自己的噪声底盖住**。中位数对少于一半的污染免疫，均值不免疫。
const SCALE_WIN: usize = 64;

/// 预热：尺度窗口攒够这么多个 `|Δ|` 之前**一律不判阶跃**。
///
/// 这不是保守，是必需的——少了它检测器会自锁：第一个 `Δ` 因为「尺度还是 0」
/// 被判成阶跃，而阶跃不进尺度窗口，于是尺度永远是 0，于是**每一个** `Δ` 都是
/// 阶跃。一条等速上涨 600 样本/秒的曲线会被报成「每秒跳变一次、净注入 59400」，
/// 而它恰恰是斜率该管、这里不该管的那一种。
const SCALE_MIN: usize = 8;

/// 判为阶跃需要超过鲁棒尺度的倍数。
///
/// 高斯噪声下 `median|Δ| ≈ 0.95σ`，5 倍即 ≈4.8σ ⇒ 单点假阳率 ~2e-6。
/// 同时它天然把**连续漂移**排除在外：等速上涨时每个 `Δ` 都相等，
/// 尺度就等于 `Δ` 本身，`|Δ| > 5·Δ` 永假。这正是要的判别——
/// 连续漂移归斜率管，离散跳变归这里管。
const STEP_K: f64 = 5.0;

/// 阶跃的绝对下限（样本）：一个 10 ms 帧 @48k。
///
/// 没有它，一条纹丝不动的序列（尺度 = 0）会把任何 ±1 样本的抖动都算成阶跃。
/// 取一帧是因为生产侧/消费侧的漏拍都以帧为单位，小于一帧的位移不是「卡顿」。
const STEP_FLOOR: f64 = 480.0;

/// 「正在累积」的读数：**离散事件的积分**，不是连续斜率。
///
/// # 为什么这一级需要它
///
/// `hal_spk` 的病理（`docs/investigate-hal-residency.md` §2.1）是：
/// 驻留量在两次卡顿之间**纹丝不动**，只在 `tx_loop` 落后 >100 ms 时
/// 一次性阶跃注入。规格 §3.3 的三态判据（drift / saturated / dropped）
/// 在这一级上三个输入**全部失效**——drift 是噪声（见 `DriftFit::significant`）、
/// `saturated` 只在 100% 才为真、`dropped` 因丢弃发生在驱动侧而结构性为 `None`。
///
/// 于是唯一还看得见这个病的量是**跳变本身**：检测阶跃，把它们的净和攒起来。
/// 一次没有被吐回来的上跳就是一次永久注入，这是判据的全部内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepAccum {
    /// 上跳次数（消费侧卡顿 ⇒ 积压被永久注入）。
    pub steps_up: u32,
    /// 下跳次数（生产侧漏写 ⇒ 积压被永久排出）。
    pub steps_down: u32,
    /// 阶跃净和（样本）。正 = 这一级净积累了多少积压。
    ///
    /// **不含**小步噪声：只有被判为阶跃的那几个 `Δ` 计入，所以它是
    /// 「历史上被灌进去、至今没出来」的量，而不是当前水位。
    ///
    /// ⚠ 它仍然带着**跳变发生那一刻的相位噪声**：测到的 `Δ` 是
    /// 「真实阶跃 + (本次噪声 − 上次噪声)」，两次跳变就带两份。所以它是
    /// 「≈4736，误差一个噪声带宽」而不是「精确 4736」，判据必须留出这个余量
    /// （见 `is_accumulating`）。
    pub net_samples: i64,
}

impl StepAccum {
    /// 这一级在累积吗？
    ///
    /// 判据 = 至少发生过一次上跳，**且**净注入超过一个 10 ms 帧。
    ///
    /// - 「一次就算」是刻意的：这一级的病理正是**一次卡顿永久注入**
    ///   （`engine.rs` 的 `tick = behind` 把跳过的帧留在环里），没有任何机制
    ///   把它拿回来。等到「多次」才报警，等的是第二次事故。
    /// - 「超过一帧」也是刻意的：`net_samples` 带着跳变时刻的相位噪声，
    ///   一次上跳 + 一次等量下跳的净和是 0 ± 一个噪声带宽。拿 `> 0` 当判据，
    ///   那种「灌进去又吐回来」的健康情形会因为几百个样本的噪声残渣被报成
    ///   「正在累积」。低于一帧的净位移不是延迟注入。
    pub fn is_accumulating(&self) -> bool {
        self.steps_up > 0 && self.net_samples > STEP_FLOOR as i64
    }
}

/// 一级的阶跃检测状态。跟着 `DriftTracker` 走，但**不受 30 s 窗口约束**：
/// 累积量是会话生命周期的积分，窗口只服务于斜率。
#[derive(Default)]
struct StepState {
    last: Option<f32>,
    /// 最近 `SCALE_WIN` 个 `|Δ|`，用来出鲁棒尺度（中位数）。
    scale_win: VecDeque<f64>,
    acc: StepAccum,
}

impl StepState {
    fn push(&mut self, v: f32) {
        let Some(prev) = self.last.replace(v) else { return };
        let d = (v - prev) as f64;
        let mag = d.abs();
        // 尺度先用**旧**窗口判，再把本次 |Δ| 收进去：否则一次大跳变会先把
        // 自己的门限抬起来，然后自己判自己不显著。预热期（见 `SCALE_MIN`）
        // 只喂窗口、不判阶跃。
        let thr = (STEP_K * self.scale()).max(STEP_FLOOR);
        if self.scale_win.len() >= SCALE_MIN && mag > thr {
            if d > 0.0 {
                self.acc.steps_up += 1;
            } else {
                self.acc.steps_down += 1;
            }
            self.acc.net_samples += d as i64;
            // 阶跃**不进**尺度窗口：它不是噪声的样本，收进去只会污染门限。
            return;
        }
        if self.scale_win.len() == SCALE_WIN {
            self.scale_win.pop_front();
        }
        self.scale_win.push_back(mag);
    }

    /// `|Δ|` 的中位数。窗口空时返回 0 ⇒ 门限退到 `STEP_FLOOR`
    /// （但预热期本来就不判阶跃，见 `SCALE_MIN`）。
    fn scale(&self) -> f64 {
        if self.scale_win.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f64> = self.scale_win.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    }
}

/// 30 s 窗口的深度斜率（样本/秒）+ 生命周期的阶跃累积。
///
/// 用途（规格 §3.3，三种病理靠它区分）：
/// - `drift ≈ 0` + 饱和 + `dropped` 冻结 ⇒ 曾被一次卡顿灌满，之后收支平衡但**永远迟到**。
/// - `drift ≈ 0` + 饱和 + `dropped` 持续增长 ⇒ **稳态产销速率失配**（`Instant` 节拍 vs 设备时钟）。
/// - `drift` 持续同号且未饱和 ⇒ 正在走向饱和，尚未到达。
///
/// ⚠ **上面这套判据在 `hal_spk` 上三个输入全部失效**（实测见
/// `docs/investigate-hal-residency.md` §2.3）。所以本结构同时维护
/// `StepAccum`：那一级的「正在累积」只能靠**检测跳变**看见，拟合斜率看不见。
///
/// **不在节拍上跑**：由 1 s 的 ticker 喂点，报告线程读斜率。
/// 窗口里的一个采样点。
///
/// `interp` 跟着**点**走而不是跟着级走，是为了免掉一处状态同步：级一级的
/// 「插值过没有」若单独存一份，就得在 `clear` / `retain_only` / 窗口滑出
/// 三个地方各清一次，漏一处就是一条陈旧的 `interpolated=true`。挂在点上，
/// 它的生命周期与那个点完全一致，三处全部自动正确。
#[derive(Debug, Clone, Copy)]
struct Pt {
    t_s: f32,
    /// **修正后**的深度（样本）。带小数：修正量是连续量，取整会把刚减掉的
    /// 量化噪声又量化回去一部分。
    v: f32,
    /// 这个点带过 `DepthInterp` 吗。
    interp: bool,
}

pub struct DriftTracker {
    /// 每级一个采样点序列，按 StageId 判别码索引。
    win: Vec<Vec<Pt>>,
    /// 每级一份阶跃累积，同样按判别码索引。与 `win` 同生共死
    /// （`clear` / `retain_only` 一并清）：一条新流继承上一条流的累积量，
    /// 与继承它的斜率是同一个错误。
    steps: Vec<StepState>,
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
        DriftTracker {
            win: (0..StageId::COUNT).map(|_| Vec::new()).collect(),
            steps: (0..StageId::COUNT).map(|_| StepState::default()).collect(),
        }
    }

    /// 喂一个采样点（**没有**时间快照，读数原样进窗口）。
    ///
    /// `now_s` 是任意单调时基下的秒数（只用差值，所以常偏无所谓）。
    ///
    /// ⚠ 这条路径上写块量化会原样进回归窗口。拿得到生产/消费侧的整块时刻时
    /// **一律走 `push_interp`**——那 ±384 样本是这个指标最大的噪声源，
    /// 而它是可以直接减掉的，不是必须忍受的本底。
    pub fn push(&mut self, now_s: f32, id: StageId, samples: u32) {
        self.push_interp(now_s, id, samples, DepthInterp::NONE);
    }

    /// 喂一个**做过量化修正**的采样点。见 `DepthInterp`（含符号推导）。
    ///
    /// 只修正**读数**，不动**时刻**：`now_s` 本来就是真实的读数时刻，最小二乘
    /// 对非均匀 x 是精确的。把 x 强行插到「统一时间栅格」上反而是往里注入误差
    /// ——那等于用一个估计出来的斜率去搬点，而斜率正是要估的东西。所以
    /// 「统一时间栅格」这件事在这里的正确落地是：**把两侧阶梯都插值到同一个
    /// 时刻（读数时刻）**，而不是把读数搬到整秒上。
    pub fn push_interp(&mut self, now_s: f32, id: StageId, samples: u32, interp: DepthInterp) {
        let v = (samples as f64 + interp.correction_samples()) as f32;
        // 阶跃检测吃的是**全部**采样点，不受 30 s 窗口约束：跳变的累积量是
        // 会话生命周期的积分，被窗口滑掉的那几次跳变照样还压在环里。
        // 它同样吃修正后的值——量化噪声被剔掉，鲁棒尺度收紧，检测器只会更灵。
        self.steps[id.index()].push(v);
        let w = &mut self.win[id.index()];
        w.push(Pt { t_s: now_s, v, interp: !interp.is_none() });
        // 窗口外的点直接丢：一次早期抖动不该永远压着斜率。
        let cutoff = now_s - Self::WINDOW_S;
        let keep = w.iter().position(|p| p.t_s >= cutoff).unwrap_or(w.len());
        if keep > 0 {
            w.drain(..keep);
        }
    }

    /// 该级不再存在时清掉它的历史，避免下一条同名会话继承上一条的斜率
    /// **与阶跃累积**。
    pub fn clear(&mut self, id: StageId) {
        self.win[id.index()].clear();
        self.steps[id.index()] = StepState::default();
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
        for i in 0..self.win.len() {
            if self.win[i].is_empty() && self.steps[i].last.is_none() {
                continue;
            }
            if !present.iter().flatten().any(|id| id.index() == i) {
                self.win[i].clear();
                self.steps[i] = StepState::default();
            }
        }
    }

    /// 最小二乘斜率，样本/秒。**分辨力不足时返回 `None`**。
    ///
    /// 三种情形都报 `None`，且三种都不是「没有漂移」：
    /// - 点数 < 3 或跨度 < 5 s：两点连线不是趋势。
    /// - 时间全同（分母退化）。
    /// - **斜率落在噪声底以内，且噪声底本身宽到没有话语权**（见
    ///   `DriftFit::resolved`）。
    ///
    /// 最后一条是这次补上的。此前这个函数把 `hal_spk` 上纯粹由写块相位噪声
    /// 产生的 −2.07…+3.10 样本/秒原样报了出去，而同一时段真实涨速是
    /// +0.34 样本/秒——报出去的数字连符号都是随机的，读者却会拿它去套规格
    /// §3.3 的三态判据。**报 `None`（「这个窗口分辨不出来」）是唯一诚实的输出**；
    /// 那一级真正的病要用 `steps()` 看。
    ///
    /// ⚠ 注意 `None` **没有**吞掉「测了，就是不漂」：一条低噪声的稳态队列
    /// 3σ 半宽很紧，照报 `Some(≈0)`（`DriftFit::resolved` 的情形 2）。
    pub fn slope(&self, id: StageId) -> Option<f64> {
        self.fit(id).filter(DriftFit::resolved).map(|f| f.slope_sps)
    }

    /// 回归本身：斜率 + 斜率标准误 + 点数 + 跨度。
    ///
    /// 拆出来是为了让「这个读数为什么被判成噪声」可查、可测：`slope()` 只给
    /// 一个 `None`，说不出它是点不够、跨度不够，还是信噪比不够。
    pub fn fit(&self, id: StageId) -> Option<DriftFit> {
        let w = &self.win[id.index()];
        if w.len() < 3 {
            return None;
        }
        let span_s = w[w.len() - 1].t_s - w[0].t_s;
        if span_s < 5.0 {
            return None;
        }
        let n = w.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for p in w {
            let (x, y) = (p.t_s as f64, p.v as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = n * sxx - sx * sx; // = n · Sxx（中心化后的平方和 × n）
        if denom.abs() < 1e-9 {
            return None;
        }
        let slope_sps = (n * sxy - sx * sy) / denom;
        let intercept = (sy - slope_sps * sx) / n;
        // 残差平方和 ⇒ 残差标准差 ⇒ 斜率标准误 = s / √Sxx。
        let sse: f64 = w
            .iter()
            .map(|p| {
                let r = p.v as f64 - (slope_sps * p.t_s as f64 + intercept);
                r * r
            })
            .sum();
        let resid_sd = (sse / (n - 2.0)).sqrt();
        let sxx_centered = denom / n;
        let stderr_sps = if sxx_centered <= 0.0 { 0.0 } else { resid_sd / sxx_centered.sqrt() };
        Some(DriftFit {
            slope_sps,
            stderr_sps,
            n: w.len(),
            span_s,
            interpolated: w.iter().all(|p| p.interp),
        })
    }

    /// 这一级到目前为止累积了多少**阶跃**注入。见 `StepAccum`。
    ///
    /// 与 `slope()` 的分工是硬的：斜率答「它在以什么速率连续变化」，
    /// 这里答「它被一次性灌进去过几次、净灌了多少」。`hal_spk` 只有后者说得清。
    pub fn steps(&self, id: StageId) -> StepAccum {
        self.steps[id.index()].acc
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

    // ------------------------------------------- 信噪比：噪声不许冒充漂移

    /// 确定性的伪随机噪声源（不引 rand 依赖，且每次跑出来的序列一模一样——
    /// 一条会随机翻绿翻红的统计断言比没有断言更糟）。
    ///
    /// 参数照抄实测：`hal_spk` 的写块相位噪声在 ±192 samples 之间滑动，
    /// σ ≈ 111（`docs/investigate-hal-residency.md` §1.2 的 86 个小步）。
    struct Lcg(u64);
    impl Lcg {
        fn new() -> Lcg {
            Lcg(0x2545_F491_4F6C_DD1D)
        }
        /// 均匀分布在 [-amp, amp] 的整数。
        fn noise(&mut self, amp: i64) -> i64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (((self.0 >> 33) as i64) % (2 * amp + 1)) - amp
        }
    }

    /// **噪声不许被报成漂移。**
    ///
    /// 实测（`docs/investigate-hal-residency.md` §2.3）：`hal_spk` 在 30 s 窗口 +
    /// 1 Hz 采样下 N≈31，噪声底让斜率标准误达 ≈2.3 样本/秒，而要探测的真实涨速
    /// 只有 +0.34 样本/秒——**噪声底比效应大 7 倍**。此前这个函数把 −2.07…+3.10
    /// 的噪声原样报出去，读者拿它去套规格 §3.3 的三态判据，得到的是随机结论。
    #[test]
    fn pure_phase_noise_reports_no_drift_at_all_instead_of_a_random_number() {
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        // 水位钉死在 8672（实测那一刻的值），只叠 ±192 的写块相位噪声。
        for i in 0..=30 {
            let v = (8_672 + rng.noise(192)) as u32;
            t.push(i as f32, StageId::HalSpk, v);
        }
        let f = t.fit(StageId::HalSpk).expect("点数与跨度都够");
        assert!(
            f.stderr_sps > 1.0,
            "前提：这段序列的噪声底就该是 2 样本/秒量级，实得 SE={}",
            f.stderr_sps
        );
        assert!(
            f.slope_sps.abs() < DRIFT_T * f.stderr_sps,
            "前提：拟合斜率落在噪声底以内，slope={} SE={}",
            f.slope_sps,
            f.stderr_sps
        );
        assert_eq!(
            t.slope(StageId::HalSpk),
            None,
            "信噪比不足 ⇒ 必须报 None（『这个窗口分辨不出来』），\
             不许把 {} 样本/秒这个方向都随机的数字当成漂移报出去",
            f.slope_sps
        );
    }

    /// **「测了，就是不漂」不许被这道门槛一起滤掉。**
    ///
    /// 上一条要的是「噪声 ⇒ None」。若判据只写成 `|slope| ≥ 3·SE`，那么**任何**
    /// 带一点噪声的稳态队列都会报 `None`——0 永远够不着 3σ 界。于是
    /// 「测到了，就是不漂」（规格 §3.3 三态判据的第一、二态都要它）就永久消失了，
    /// 而那与「没测出来」是两个不同的结论。
    ///
    /// 分界不是「有没有噪声」，是**噪声底宽不宽**（`DriftFit::resolved` 的
    /// 情形 2 vs 情形 3）：这里的两条序列同样稳态、同样有噪声，只差噪声幅度。
    #[test]
    fn a_steady_stage_with_tiny_noise_still_reports_measured_and_not_drifting() {
        // ±1 个样本的抖动：3σ 半宽 ≈0.16 样本/秒，紧得足以说「它没在漂」。
        let mut tight = DriftTracker::new();
        for i in 0..=30u32 {
            tight.push(i as f32, StageId::PostMix, 4_800 + (i % 2));
        }
        let f = tight.fit(StageId::PostMix).unwrap();
        assert!(f.stderr_sps > 0.0, "前提：确实有残差，不是完美拟合（SE={}）", f.stderr_sps);
        assert!(
            f.resolution_sps() <= DRIFT_RESOLUTION_SPS,
            "前提：噪声底很紧，实得 3σ 半宽 {}",
            f.resolution_sps()
        );
        let s = tight
            .slope(StageId::PostMix)
            .expect("低噪声稳态必须报『测到了，≈0』，不是 None");
        assert!(s.abs() < 0.1, "实得 {s}");

        // 对照：同样稳态，噪声抬到 ±192（hal_spk 的写块相位噪声）⇒ 半宽 ≈6.7，
        // 这个窗口就没有话语权了。
        let mut rng = Lcg::new();
        let mut loose = DriftTracker::new();
        for i in 0..=30 {
            loose.push(i as f32, StageId::PostMix, (4_800 + rng.noise(192)) as u32);
        }
        assert!(loose.fit(StageId::PostMix).unwrap().resolution_sps() > DRIFT_RESOLUTION_SPS);
        assert_eq!(
            loose.slope(StageId::PostMix),
            None,
            "同样是稳态，噪声底一宽就分辨不出来了 —— 这一条必须报 None"
        );
    }

    /// ……但**真的**漂移必须照报，门槛不能高到把病也滤掉。
    ///
    /// 1% 速率失配 = 480 样本/秒，即使叠上同一份 ±192 的噪声也必须显著。
    #[test]
    fn a_real_rate_mismatch_still_gets_through_the_significance_gate() {
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        for i in 0..=30 {
            let v = (1_000 + 480 * i + rng.noise(192)) as u32;
            t.push(i as f32, StageId::SrcFifo, v);
        }
        let s = t.slope(StageId::SrcFifo).expect("480 样本/秒远在噪声底之上");
        assert!((s - 480.0).abs() < 10.0, "斜率仍要准，实得 {s}");
    }

    // ------------------------------------ 时间插值：写块量化是可以直接减掉的

    /// 一段**物理上成立**的环深度轨迹发生器（照着 `hal_spk` 建模）。
    ///
    /// - 生产侧：驱动的 IOProc，一次 `DoIOOperation` 写整块 `block` 个样本，
    ///   速率 `writer_sps`；
    /// - 消费侧：`tx_loop`，每 10 ms 读整块 480 个，速率恰好 48000。
    ///
    /// 深度 = 两条阶梯之差。**真实漂移 = `writer_sps − 48000`，别的什么都没有**
    /// ——所以任何非零残差都是量化，不是物理。
    struct RingSim {
        block: f64,
        writer_sps: f64,
        read_chunk: f64,
        read_period_s: f64,
        base: f64,
    }

    impl RingSim {
        /// 实测那一级的参数：块 768（±384 的来源）、真实涨速 +0.34 样本/秒
        /// （9 小时 253 ms），起始水位 8672（`investigate-hal-residency.md` §0）。
        fn hal_spk() -> RingSim {
            RingSim {
                block: 768.0,
                writer_sps: 48_000.34,
                read_chunk: 480.0,
                read_period_s: 0.01,
                base: 8_672.0,
            }
        }

        /// `t` 时刻**能观测到**的深度：两条阶梯之差，带着全部量化。
        fn observed(&self, t: f64) -> u32 {
            let w = (t * self.writer_sps / self.block).floor() * self.block;
            let r = (t / self.read_period_s).floor() * self.read_chunk;
            (self.base + w - r) as u32
        }

        /// `t` 时刻生产侧最近一次写块距今多久（秒）。
        fn since_write_s(&self, t: f64) -> f64 {
            t - (t * self.writer_sps / self.block).floor() * self.block / self.writer_sps
        }

        /// `t` 时刻消费侧最近一次读块距今多久（秒）。
        fn since_read_s(&self, t: f64) -> f64 {
            t - (t / self.read_period_s).floor() * self.read_period_s
        }

        fn reader_sps(&self) -> f64 {
            self.read_chunk / self.read_period_s
        }

        /// 两侧都填的正确快照。
        fn interp(&self, t: f64) -> DepthInterp {
            DepthInterp::producer(self.since_write_s(t) as f32, self.writer_sps as f32)
                .with_consumer(self.since_read_s(t) as f32, self.reader_sps() as f32)
        }

        /// **写反了**的快照：生产侧的量填进消费侧的槽，反之亦然。
        /// 这正是符号搞反时会写出来的东西，修正量恰好取负。
        fn interp_swapped(&self, t: f64) -> DepthInterp {
            DepthInterp::producer(self.since_read_s(t) as f32, self.reader_sps() as f32)
                .with_consumer(self.since_write_s(t) as f32, self.writer_sps as f32)
        }

        /// 真实的连续深度（无量化）。
        fn truth(&self, t: f64) -> f64 {
            self.base + (self.writer_sps - self.reader_sps()) * t
        }
    }

    /// 1 Hz 心跳的真实开火时刻：整秒 ± 10 ms 的调度抖动。
    fn tick_times(rng: &mut Lcg) -> Vec<f64> {
        (0..=30).map(|k| k as f64 + rng.noise(10) as f64 / 1000.0).collect()
    }

    /// **喂入带量化噪声的理想斜率：插值前后的斜率标准误必须显著下降。**
    ///
    /// 这条是本轮的验收点。同一条物理轨迹喂两遍：
    /// - 原样喂 ⇒ ±384 的写块量化 + ±240 的读块量化原封不动进回归窗口，
    ///   标准误落在**样本/秒量级**，比要探测的 +0.34 大一个数量级 ⇒ 报 `None`；
    /// - 带 `DepthInterp` 喂 ⇒ 两条阶梯都被插值回连续位置，残差只剩 f32 的
    ///   舍入 ⇒ 标准误掉到 1e-5 量级，斜率精确落在 0.34。
    ///
    /// 实测（确定性，跑多少次都一样）：
    ///
    /// | | 斜率 | 标准误 | 3σ 半宽 | `slope()` |
    /// |---|---|---|---|---|
    /// | 原样喂 | **+3.79**（真值 0.34，连量级都不对） | 4.58 | 13.7 | `None` |
    /// | 带 `DepthInterp` 喂 | **+0.3400004** | 5.6e-6 | 1.7e-5 | `Some(0.34)` |
    ///
    /// 注意这**不是**把噪声按 √N 压下去（那要 400 倍的点、3 小时以上的窗口），
    /// 是把它整个减掉——窗口一秒都没变长。此前把 `drift_sps` 测不出漂移归因为
    /// 「窗口太短」是错的，归因错了就会去改窗口而不是改误差项。
    #[test]
    fn time_interpolation_removes_the_write_block_quantization() {
        let sim = RingSim::hal_spk();
        let mut rng = Lcg::new();
        let ts = tick_times(&mut rng);

        let mut raw = DriftTracker::new();
        let mut fixed = DriftTracker::new();
        for &t in &ts {
            let d = sim.observed(t);
            // 逐点先钉死机制本身：**读数 + 修正 = 无量化的真值**。
            // 这一条比下面的统计量强 —— 统计量只能说「噪声小了」，它说的是
            // 「量化被精确减掉了」。修正若只是碰巧朝对的方向偏一点，这里就红。
            let restored = d as f64 + sim.interp(t).correction_samples();
            assert!(
                (restored - sim.truth(t)).abs() < 0.05,
                "t={t}: 观测 {d} + 修正 = {restored}，真值 {}",
                sim.truth(t)
            );
            // 而未修正的读数与真值差着整整一个块的量级——那就是要减掉的东西。
            raw.push(t as f32, StageId::HalSpk, d);
            fixed.push_interp(t as f32, StageId::HalSpk, d, sim.interp(t));
        }
        assert!(
            ts.iter().any(|&t| (sim.observed(t) as f64 - sim.truth(t)).abs() > 100.0),
            "前提：未修正时至少有点偏出 100 样本，否则这条轨迹根本没有量化可减"
        );

        let fr = raw.fit(StageId::HalSpk).expect("点数与跨度都够");
        let ff = fixed.fit(StageId::HalSpk).expect("点数与跨度都够");

        // 前提：这条轨迹的原始噪声底确实压过了要探测的效应。
        assert!(
            fr.stderr_sps > 1.0,
            "前提：未插值时噪声底该是样本/秒量级，实得 SE={}",
            fr.stderr_sps
        );
        // 验收：标准误至少降两个数量级（实得 4.58 → 5.6e-6，降了 80 万倍）。
        assert!(
            ff.stderr_sps * 100.0 < fr.stderr_sps,
            "插值后标准误必须显著下降：raw SE={} → interp SE={}",
            fr.stderr_sps,
            ff.stderr_sps
        );
        // 再钉一条绝对上限：相对判据在「两边一起变差」时是绿的，绝对判据不是。
        // 1e-3 比实得的 5.6e-6 松 180 倍，只拦住量级级别的退化。
        assert!(ff.stderr_sps < 1e-3, "插值后的标准误不该有物理噪声，实得 {}", ff.stderr_sps);
        assert!(!fr.interpolated && ff.interpolated, "两条读数的来源必须自报家门");

        // 而且不只是「更平滑」——插值后的斜率要**对**。
        assert!(
            (ff.slope_sps - 0.34).abs() < 0.01,
            "插值后必须还原出真实涨速 +0.34 样本/秒，实得 {}",
            ff.slope_sps
        );
        assert_eq!(raw.slope(StageId::HalSpk), None, "原始读数分辨不出 0.34，只能报 None");
        let got = fixed.slope(StageId::HalSpk).expect("插值后这一级终于说得出话");
        assert!((got - 0.34).abs() < 0.01, "实得 {got}");
    }

    /// **符号写反了必须变红。**
    ///
    /// 修正项是 `+生产侧欠的 − 消费侧欠的`。把两侧对调（最容易写出来的那个错）
    /// 修正量恰好取负，于是 `观测 + (−修正) = 真值 − 2×量化`——噪声不但没消，
    /// **正好翻倍**。所以判据是硬的：写反时标准误必须比**不插值**还差，
    /// 而不只是「比正确插值差」。
    #[test]
    fn interpolating_with_the_wrong_sign_makes_it_worse_not_better() {
        let sim = RingSim::hal_spk();
        let mut rng = Lcg::new();
        let ts = tick_times(&mut rng);

        let mut raw = DriftTracker::new();
        let mut flipped = DriftTracker::new();
        for &t in &ts {
            let d = sim.observed(t);
            raw.push(t as f32, StageId::HalSpk, d);
            flipped.push_interp(t as f32, StageId::HalSpk, d, sim.interp_swapped(t));
        }
        let fr = raw.fit(StageId::HalSpk).unwrap().stderr_sps;
        let fl = flipped.fit(StageId::HalSpk).unwrap().stderr_sps;
        assert!(
            fl > 1.8 * fr,
            "符号反了残差就该精确翻倍（raw SE={fr} → flipped SE={fl}）。\
             这条断言若变绿说明 `DepthInterp::correction_samples` 的两项符号\
             不再是『生产侧加、消费侧减』"
        );
        assert_eq!(flipped.slope(StageId::HalSpk), None, "噪声翻倍后更不可能分辨出 0.34");
    }

    /// 修正量本身的符号与量纲，直接钉在 `correction_samples()` 上。
    ///
    /// 上一条测的是「反了会更差」，这一条测的是「正着是多少」——两条都在，
    /// 才能把「符号对」和「大小对」分开定位。
    #[test]
    fn the_correction_adds_what_the_producer_owes_and_subtracts_what_the_consumer_owes() {
        // 生产侧欠着 5 ms @48k = 240 个样本还没写出来 ⇒ 连续深度比读数**大** 240。
        // 容差 1e-3 而不是 1e-6：`0.005f32` 本来就不是精确的 5 ms（差 5e-9 s），
        // 乘 48000 就是 2.6e-4 个样本。写 1e-6 是在断言一件 f32 做不到的事。
        let p = DepthInterp::producer(0.005, 48_000.0);
        assert!((p.correction_samples() - 240.0).abs() < 1e-3, "{}", p.correction_samples());
        // 消费侧欠着 5 ms 没读走 ⇒ 连续深度比读数**小** 240。
        let c = DepthInterp::consumer(0.005, 48_000.0);
        assert!((c.correction_samples() + 240.0).abs() < 1e-3, "{}", c.correction_samples());
        // 两侧欠得一样多 ⇒ 互相抵消，读数本来就在连续位置上。
        assert!(p.with_consumer(0.005, 48_000.0).correction_samples().abs() < 1e-9);
        assert!(DepthInterp::NONE.is_none() && DepthInterp::default().is_none());
        assert!(!p.is_none());
    }

    /// **坏快照必须被忽略，不许被采信。**
    ///
    /// 时钟回跳、速率字段没填、时间戳陈旧——这些在真机上都会发生，而一个错的
    /// 修正比不修正更坏：它是直接加在读数上的，会被当成真深度进回归窗口。
    /// PipeWire 在同一位置也钳（`alsa-pcm.c` 只在 `|diff| < threshold*3` 时采纳
    /// 且修正量钳在 ±threshold）。
    #[test]
    fn a_broken_snapshot_is_ignored_rather_than_believed() {
        for bad in [
            DepthInterp::producer(f32::NAN, 48_000.0),
            DepthInterp::producer(0.005, f32::NAN),
            DepthInterp::producer(f32::INFINITY, 48_000.0),
            DepthInterp::producer(-0.005, 48_000.0), // 时钟回跳
            DepthInterp::producer(0.005, 0.0),       // 速率没填
            DepthInterp::producer(0.005, -48_000.0),
        ] {
            assert_eq!(bad.correction_samples(), 0.0, "坏快照必须退化成不修正：{bad:?}");
            assert!(bad.is_none(), "坏快照不算『插值过』：{bad:?}");
        }
        // 陈旧快照（一整秒没写块了）钳在 100 ms 等效量以内，而不是加进去 48000。
        let stale = DepthInterp::producer(1.0, 48_000.0);
        assert_eq!(stale.correction_samples(), INTERP_CLAMP_SAMPLES);

        // 钳住之后的读数仍然进得去窗口，只是不会把水位炸到天上。
        let mut t = DriftTracker::new();
        for i in 0..=10 {
            t.push_interp(i as f32, StageId::HalSpk, 8_672, stale);
        }
        let f = t.fit(StageId::HalSpk).unwrap();
        assert!(f.slope_sps.abs() < 1e-3, "恒定的修正量不产生斜率，实得 {}", f.slope_sps);
    }

    /// 半段插值过、半段没插过的窗口**不许**自称插值过：接缝处会凭空多出一个
    /// 台阶（一整个块），把它当「噪声已剔除」汇报是误导。
    #[test]
    fn a_window_that_only_partly_had_snapshots_does_not_claim_to_be_interpolated() {
        let mut t = DriftTracker::new();
        for i in 0..=5 {
            t.push(i as f32, StageId::HalSpk, 8_672);
        }
        for i in 6..=12 {
            t.push_interp(i as f32, StageId::HalSpk, 8_672, DepthInterp::producer(0.005, 48_000.0));
        }
        assert!(!t.fit(StageId::HalSpk).unwrap().interpolated, "混着来 ⇒ false");
        // 旧点滑出 30 s 窗口之后，剩下的全是插值点，这时才该为 true。
        for i in 40..=60 {
            t.push_interp(i as f32, StageId::HalSpk, 8_672, DepthInterp::producer(0.005, 48_000.0));
        }
        assert!(t.fit(StageId::HalSpk).unwrap().interpolated);
    }

    /// **门槛是照着真实病灶标定的，不是照着噪声底标定的。**
    ///
    /// 「测了，就是不漂」这句话若要成立，就必须排除得掉实测的 +0.34 样本/秒
    /// （9 小时 253 ms）。门槛一旦松过它，那句「不漂」就是假保证。
    /// 这条断言把这个推导钉死——将来谁为了「让 UI 少报 None」把它调松，这里会红。
    #[test]
    fn the_resolution_bar_is_tight_enough_to_exclude_the_measured_hal_spk_drift() {
        const MEASURED_HAL_SPK_DRIFT_SPS: f64 = 0.34;
        assert!(
            DRIFT_RESOLUTION_SPS < MEASURED_HAL_SPK_DRIFT_SPS,
            "门槛 {DRIFT_RESOLUTION_SPS} 必须紧过实测涨速 {MEASURED_HAL_SPK_DRIFT_SPS}，\
             否则『测到了，不漂』这个结论排除不掉那个正在把环推向饱和的真实漂移"
        );
        // 反过来也要有下限意识：门槛紧到 0 就等于永远不许说「不漂」。
        assert!(DRIFT_RESOLUTION_SPS > 0.0);
        // 一条被插值救活的序列必须真的够得着这个门槛（否则收紧就是关掉指标）。
        let sim = RingSim::hal_spk();
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        for tk in tick_times(&mut rng) {
            t.push_interp(tk as f32, StageId::HalSpk, sim.observed(tk), sim.interp(tk));
        }
        assert!(
            t.fit(StageId::HalSpk).unwrap().resolution_sps() < DRIFT_RESOLUTION_SPS,
            "插值后的 3σ 半宽必须落在门槛以内，否则这个门槛没有任何序列够得着"
        );
    }

    // ------------------------------------------- 阶跃：这一级真正看得见的量

    /// **斜率看不见的病，阶跃检测器看得见。**
    ///
    /// 复刻实测轨迹（`docs/investigate-hal-residency.md` §1.2）：水位在两次卡顿
    /// 之间纹丝不动（只有 ±192 的写块相位噪声），偶尔被一次 >100 ms 的消费侧
    /// 卡顿一次性抬高 2368 samples（`engine.rs` 的 `tick = behind` 把跳过的帧
    /// 永久留在环里）。9 小时涨 253 ms 就是这么来的。
    ///
    /// 两条断言合起来才是这条测试的全部内容：
    /// - 30 s 窗口的斜率**报不出来**（跳变早就滑出窗口，窗口里只剩噪声）；
    /// - 阶跃累积**报得出来**，而且净和精确等于两次跳变之和。
    #[test]
    fn a_staircase_is_invisible_to_the_slope_and_obvious_to_the_step_detector() {
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        let mut level: i64 = 8_672;
        let mut clock = 0.0f32;
        let quiet = |t: &mut DriftTracker, rng: &mut Lcg, clock: &mut f32, level: i64, n: u32| {
            for _ in 0..n {
                t.push(*clock, StageId::HalSpk, (level + rng.noise(192)) as u32);
                *clock += 1.0;
            }
        };
        quiet(&mut t, &mut rng, &mut clock, level, 40);
        for _ in 0..2 {
            level += 2_368; // 一次 >100 ms 的消费侧卡顿
            quiet(&mut t, &mut rng, &mut clock, level, 40);
        }

        assert_eq!(
            t.slope(StageId::HalSpk),
            None,
            "窗口里只剩最后 30 s 的噪声 —— 斜率对这个病是瞎的，这正是要点"
        );

        let acc = t.steps(StageId::HalSpk);
        assert_eq!(acc.steps_up, 2, "两次跳变都要被抓到，实得 {acc:?}");
        assert_eq!(acc.steps_down, 0);
        // 净注入 ≈ 2×2368。不写成精确相等是因为测到的 `Δ` 必然带着跳变那一刻的
        // 相位噪声（`真实阶跃 + 本次噪声 − 上次噪声`），两次跳变带两份，
        // 上界 2×2×192 = 768。写成 `assert_eq!(4736)` 是在断言一件物理上做不到
        // 的事，那种断言只会逼后来的人去调噪声幅度而不是去看检测器。
        assert!(
            (acc.net_samples - 4_736).abs() < 768,
            "净注入必须落在 2×2368 ± 一个噪声带宽内，实得 {acc:?}"
        );
        assert!(acc.is_accumulating(), "有净注入且从未吐回 ⇒ 正在累积");
        // 对照：驻留量本身（最后的水位 ≈ 8672+4736）远高于起点，
        // 而这一点**没有任何一个规格 §3.3 的输入看得见**——drift 是 None、
        // 环远未饱和、dropped 在驱动侧。只有上面这个 `acc` 说得出来。
    }

    /// **纯噪声不许产生任何阶跃。** 否则这个检测器只是换了个地方生成随机数。
    #[test]
    fn pure_noise_produces_no_steps() {
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        for i in 0..300 {
            t.push(i as f32, StageId::HalSpk, (8_672 + rng.noise(192)) as u32);
        }
        let acc = t.steps(StageId::HalSpk);
        assert_eq!(acc, StepAccum::default(), "300 个噪声点，一次阶跃都不许有：{acc:?}");
        assert!(!acc.is_accumulating());
    }

    /// **连续等速漂移不是阶跃。**
    ///
    /// 门限用 `|Δ|` 的中位数标定 ⇒ 等速上涨时门限恰好等于 5×Δ，永远够不着。
    /// 这条分工是硬的：连续漂移归 `slope()`，离散跳变归 `steps()`，
    /// 两个量不许互相冒充。
    #[test]
    fn a_smooth_ramp_is_drift_not_steps() {
        let mut t = DriftTracker::new();
        for i in 0..100u32 {
            t.push(i as f32, StageId::PlayRing, 1_000 + 600 * i); // 600 样本/秒
        }
        assert_eq!(t.steps(StageId::PlayRing), StepAccum::default(), "等速上涨没有阶跃");
        let s = t.slope(StageId::PlayRing).expect("这才是斜率该报的东西");
        assert!((s - 600.0).abs() < 1e-6, "实得 {s}");
    }

    /// 生产侧漏写把积压排出去 ⇒ 下跳，净和抵消。累积判据必须跟着变假。
    #[test]
    fn a_jump_that_drains_back_out_is_not_accumulation() {
        let mut rng = Lcg::new();
        let mut t = DriftTracker::new();
        let mut clock = 0.0f32;
        let mut level: i64 = 8_672;
        let quiet = |t: &mut DriftTracker, rng: &mut Lcg, clock: &mut f32, level: i64| {
            for _ in 0..40 {
                t.push(*clock, StageId::HalSpk, (level + rng.noise(192)) as u32);
                *clock += 1.0;
            }
        };
        quiet(&mut t, &mut rng, &mut clock, level);
        level += 2_368; // 消费侧卡顿
        quiet(&mut t, &mut rng, &mut clock, level);
        level -= 2_368; // 生产侧漏写，原样吐回来
        quiet(&mut t, &mut rng, &mut clock, level);
        let acc = t.steps(StageId::HalSpk);
        assert_eq!((acc.steps_up, acc.steps_down), (1, 1), "{acc:?}");
        assert!(
            acc.net_samples.abs() < 768,
            "灌进去多少吐回来多少 ⇒ 净和 ≈ 0（余下的是跳变时刻的相位噪声），实得 {acc:?}"
        );
        assert!(
            !acc.is_accumulating(),
            "净位移不足一帧就不是累积 —— 判据若写成 `net > 0`，这里的 {} 个噪声残渣\
             就会把一次健康的『灌进去又吐回来』报成正在累积",
            acc.net_samples
        );
    }

    /// **门限的尺度必须是中位数，不能是均值。**
    ///
    /// 少数几个「大而没越线」的 `Δ`（现实里就是接近但未越过 100 ms 那条线的
    /// 卡顿）会合法地进入尺度窗口。均值被它们拽高一大截，门限跟着抬起来，
    /// 于是**下一次真正越线的跳变被自己的噪声底盖住**——检测器越是刚经历过
    /// 一串抖动，就越是看不见随后那次真事故，与要它做的事正好相反。
    ///
    /// 构造：每 6 拍里 4 个 ±200、2 个 ±900 ⇒ 中位数 200（门限 1000）、
    /// 均值 ≈430（门限 ≈2150）。那次 1500 的真跳变恰好落在两者之间。
    #[test]
    fn the_step_threshold_uses_a_median_so_a_few_big_wiggles_cannot_hide_the_next_jump() {
        let mut t = DriftTracker::new();
        let mut clock = 0.0f32;
        let mut level: i64 = 8_000;
        for _ in 0..14 {
            for d in [200, -200, 200, -200, 900, -900] {
                level += d;
                t.push(clock, StageId::HalSpk, level as u32);
                clock += 1.0;
            }
        }
        assert_eq!(
            t.steps(StageId::HalSpk),
            StepAccum::default(),
            "前提：这些抖动一个都没越门限，它们只是**进了尺度窗口**"
        );

        level += 1_500; // 真跳变：> 5×中位数(1000)，< 5×均值(≈2150)
        t.push(clock, StageId::HalSpk, level as u32);
        let acc = t.steps(StageId::HalSpk);
        assert_eq!(
            acc.steps_up, 1,
            "中位数门限看得见这次跳变；把 `scale()` 换成均值就看不见了，实得 {acc:?}"
        );
        assert_eq!(acc.net_samples, 1_500);
    }

    /// 源被换掉之后，新源不许继承旧源的**阶跃累积**——与不许继承斜率同一条纪律。
    #[test]
    fn a_replaced_source_does_not_inherit_the_previous_step_accumulation() {
        let mut t = DriftTracker::new();
        for i in 0..10 {
            t.push(i as f32, StageId::HalSpk, 1_000);
        }
        for i in 10..20 {
            t.push(i as f32, StageId::HalSpk, 20_000); // 一次巨跳
        }
        assert!(t.steps(StageId::HalSpk).is_accumulating(), "前提：旧源确实累积过");
        t.retain_only(&[None, None, None]);
        assert_eq!(
            t.steps(StageId::HalSpk),
            StepAccum::default(),
            "换源必须断历史 —— 否则一条刚开的干净流一上来就报『正在累积』"
        );
        t.clear(StageId::HalSpk); // 幂等
        assert_eq!(t.steps(StageId::HalSpk), StepAccum::default());
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

    /// **「有值」和「是真值」是两件事，只有后者能清掉「≥」。**
    ///
    /// 这条守 [`LatSource::is_exact`]。它防的是一个具体的、已经写进文档过的错误
    /// 判据：「设备固有延迟项**齐全**后总和就不再是下限」。
    /// 30-win 实测把这个判据打穿了 —— Windows 的 `GetDevicePeriod` 报 10.00 ms
    /// （齐全、非 `Unavailable`、`ms()` 有值），而同一端点写到播实测 41.92 ms。
    /// 照「齐全就升级」做，用户会看到一个不带「≥」的 121 ms，真值 153 ms。
    ///
    /// 注入对照：
    /// - `is_exact` 放宽成 `!matches!(self, Unavailable)`（即「有值就算数」）⇒ 本条红；
    /// - 放宽成 `matches!(self, Api | Assumed)`（即「只排除已知少报的」）⇒ 本条红。
    #[test]
    fn having_a_number_is_not_the_same_as_having_a_true_number() {
        // Windows 形态：读到了 480 帧 = 10 ms，非 Unavailable，`ms()` 有值……
        let win = DevLatency { frames: 480, rate: 48_000, source: LatSource::Unreliable };
        assert_eq!(win.ms(), Some(10.0), "它确实有值 —— 所以「齐全了吗」这个判据答『是』");
        assert!(
            !win.source.is_exact(),
            "……但实测真值 41.92 ms，低报 4.2 倍：它永远只能当下限"
        );

        // 四个取值的完整真值表，一个都不许漏。
        assert!(LatSource::Api.is_exact());
        assert!(!LatSource::Assumed.is_exact());
        assert!(!LatSource::Unreliable.is_exact());
        assert!(!LatSource::Unavailable.is_exact());

        // `Unavailable` 那一格双重设防：既不 exact，也根本没有毫秒值。
        assert_eq!(DevLatency::unavailable().ms(), None);
        assert!(!DevLatency::unavailable().source.is_exact());
    }
}
