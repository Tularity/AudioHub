//! `hal_mic` 生产侧水位闸门 —— 虚拟麦克风环的唯一一处水位治理。
//!
//! # 病灶（`docs/spec-hal-mic-latency.md` 实测复现）
//!
//! `hal_mic` 是全链路上**唯一一级我们是生产者、驱动是消费者**的环。
//! `hal_spk` 那套治法（跳 tick 排空、周期 trim）全部动 `read_idx`——
//! 而麦克风环的 `read_idx` 属于驱动，SPSC 不变量不许我们碰
//! （`halbridge.rs` 顶部：exactly ONE may call `write_mic_mono`）。
//!
//! 于是这一级此前**一个控制器都没有**，水位是一个取值域 `[0, 500 ms]`、
//! 没有任何回复力的自由参数：
//!
//! ```text
//! App StopIO ──→ 驱动 IOProc 立刻停止读环
//!                 │  ← 这段窗口里 mixer 仍按 480 帧/10 ms 往环里灌，无人取走
//!                 │     窗口 = 驱动 bridge 线程一趟（≤200 ms）
//!                 │           + daemon coordinator 一拍（≤200 ms）
//!                 ▼
//! daemon 收到 IoState(running=false) ──→ hal_mic_io[slot]=false，停写
//! ```
//!
//! 灌进去多少就**永久**留多少（两侧同锚 `mach_absolute_time`，实测
//! |drift| < 5.2 ppm，没有任何机制排它）。实测棘轮：
//! 160→209→87→313→179→477→**500 ms 饱和**。用户看到的 132–141 ms 是这条
//! 区间上的一个点，`hal_spk` 同期只有 24–33 ms。
//!
//! 后果不只是延迟。`AudioHubRing_Read` 从**最旧**的样本开始读，所以一个 App
//! 打开虚拟麦克风时，头 132–500 ms 录到的是**上一次会话的音频**——
//! 这是正确性问题，不是性能问题。
//!
//! # 手段：不能 trim，但可以少写
//!
//! 生产者手里有一个完全合法的操纵量：**少写**。
//! `mic_depth()` 每拍都在读（写之前读，语义正确），水位是可观测的；
//! 水位高于天花板就停写，直到落回目标。丢弃发生在**我们能数到**的一侧，
//! 比 `hal_spk` 的处境还好一点（那一级的丢弃计数在驱动那里）。
//!
//! # 为什么只有一个机制
//!
//! 调查报告提了三个修法（停读检出 / 水位 trim / 驱动侧 StartIO 冲洗）。
//! 前两个在**效果上是同一件事**：
//!
//! - 「消费者停了就别写」把停写检出从 ≤400 ms 降到 ≤20 ms，于是单次注入
//!   ≤20 ms；
//! - 「水位天花板」把**任何**来源的积压都截在 `D_CEIL`。
//!
//! 而消费者停着时水位**只增不减**（`occupied` 只在驱动读走时才降），
//! 所以停读那一路最终也是被天花板截停的，残留恰好等于 `D_CEIL` = 30 ms。
//! 停读检出（连续 N 拍零消费才敢判定，N 至少要 5 拍才不会把驱动 10.67 ms 的
//! 读周期误判成停读）能把残留压到「停读那一刻的水位 + 50 ms」= 0.7…12.7 + 50
//! = **50.7…62.7 ms —— 比天花板还高**。也就是说在天花板存在的前提下，
//! 停读检出**一毫秒都削不下来**，只会多一个会误判、会造成静音的状态机。
//! 所以本模块只实现天花板，一个机制、一条不变式。
//!
//! （驱动侧 StartIO 冲洗是另一回事：它治的是**残留本身**而不是积累速度，
//! 与本模块正交，见 `drivers/macos-hal/src/AudioHubDriver.c` 的 StartIO。）
//!
//! # 唯一的不变式（欠载的构造性论证）
//!
//! **`I_FLOOR`：水位低于 `D_FLOOR` 时，本闸门绝不少写一个样本。**
//!
//! 论证是一个**偏序关系**，不是一组保守参数：
//!
//! - 无闸门时这一级在一条实测 **32–608 帧**（`D_BAND_TOP`）的带里自由滑动，
//!   带的下沿每个周期都被访问一次——即无闸门系统自己**每个周期都会走到 32 帧**；
//! - 闸门只在水位 ≥ `D_FLOOR = 480` 帧时才少写，**从不把水位守到 480 帧以下**。
//!
//! ⇒ 闸门守的水位**严格高于**无闸门系统自己访问的最低点
//! ⇒ **闸门不可能让欠载比无闸门时更容易发生。**
//!
//! 这句话可以逐项检查，`t3_the_gate_never_starves_the_driver_under_adversarial_depths`
//! 用 20 万个随机水位（含跳变、含贴地、含饱和）把它钉死；把地板判据挪到迟滞
//! 判据后面（= 排空一路排穿）会让它当场变红。
//!
//! 判据的**顺序**是论证的载体：地板判据写在 `decide()` 的第一位并直接
//! `return`，所以「绕过地板」在结构上不可表达，不需要调用方守规矩。
//!
//! # 可削到多少（2026-08-04 实测）
//!
//! | | `hal_mic` |
//! |---|---|
//! | 治理前（用户现场） | 132–141 ms，上界 500 ms 饱和 |
//! | 治理后（同机同对端，60 s 会话） | **0–14.7 ms，p50 8.7 ms** |
//! | 闸门的硬上界 | `D_CEIL` = 30 ms |
//!
//! 稳态下闸门**一次都不动手**（`drain_events == 0`）：自由带 0.7–12.7 ms
//! 整个落在天花板 30 ms 之下。它的价值是**上界**——把一个取值域
//! `[0, 500 ms]`、没有回复力的自由参数，变成一个有界受控量。

/// 消费量子：驱动每次 IOProc 从环里取走的帧数。
///
/// **实测钉死**，不是猜的：四次会话里所有 `occupied` 读数 mod 32 == 0，
/// 且 `512 − 480 = 32`（`docs/spec-hal-mic-latency.md` §2）。
pub const Q_C: u32 = 512;

/// 生产量子：mixer 每 10 ms tick 写进环里的帧数（48 kHz）。
pub const Q_P: u32 = 480;

/// 无闸门系统**实测**的自由带上沿，帧。
///
/// ⚠ 这个数是量出来的，不是推出来的，而且它**推翻了调查报告的推导**。
/// 报告按 `D_min = Q_C + 48·L`（`L` = `MIX_LATE` p99.93 = 10 ms）得出 992 帧，
/// 并据此建议目标 25–30 ms。2026-08-04 部署后在同一台机器、同一对端上实测
/// 一条**干净**的 60 s 会话，水位取值集合是：
///
/// ```text
/// 32 64 96 160 192 224 256 288 320 352 384 480 512 544 576 608   （步长 32 帧）
/// ```
///
/// 即整条带是 **32–608 帧 = 0.67–12.7 ms**，**整个落在推导出来的 `D_min`
/// 之下**。会话上报的 `hal_mic` 同期 0–14.7 ms（p50 8.7），而这一分钟的
/// `probe capture` 收到 2 879 488 个样本 = 59.99 s @ 48 kHz，**连续无洞**。
///
/// 所以真实的驱动在 0.67 ms 水位上照样读得满：报告那条推导假设「生产者迟到
/// `L` 时水位要能撑住」，而实际上我们的观测点在**写之前**、正好是驱动刚读完
/// 的波谷，观测到的最低点与驱动读取那一刻面对的水位不是同一个数。
///
/// 按推导值定门限的后果很实在：`D_FLOOR = 1536` 帧会把排空**停在 32 ms**，
/// 而自由带只有 0.7–12.7 ms —— 闸门会把这一级永久钉在比它自己该有的位置
/// **高 20 ms** 的地方（两侧同锚，没有任何机制让它再降下来）。
pub const D_BAND_TOP: u32 = 608; // 12.7 ms，实测

/// 少写的**停止**线，也是绝不越过的地板：480 帧 = 10 ms = 一个生产量子。
///
/// 它落在实测自由带 32–608 帧的**上半段**，于是：
///
/// - 闸门永远不会把水位守得比 `D_FLOOR` 低；
/// - 而无闸门系统自己每个周期都要走到 32 帧。
///
/// ⇒ **闸门守的水位严格高于无闸门系统自己访问的最低点**，这就是不变式
/// `I_FLOOR` 的全部内容（见模块文档）。取一个生产量子而不是更小的数，
/// 是为了让排空的落点仍在带内而不是贴着带底。
pub const D_FLOOR: u32 = Q_P; // 480 帧 = 10 ms

/// 少写的**启动**线：1440 帧 = 30 ms。超过它就一路排到 `D_FLOOR`。
///
/// 余量 = `D_CEIL − D_BAND_TOP` = 832 帧 ≈ 17 ms ≈ **1.6 个自由带宽**。
/// 留这么宽是因为带宽依赖 `Q_C`（App 的 IOProc 缓冲尺寸），换一个 App 就可能
/// 变——而闸门在稳态下误动手的后果是**持续断续**，正是麦克风方向最难发现的
/// 那种故障。`drain_events` 是否长期为 0 就是判断这个余量够不够的运行时读数。
pub const D_CEIL: u32 = 1440; // 30 ms

/// 本拍的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicPlan {
    /// 本拍允许写进环里的帧数（`0..=want`）。
    pub allow: u32,
    /// 本拍**丢掉**的帧数 = `want − allow`。丢弃是可数的，这是它的计数点。
    pub withheld: u32,
    /// 本拍是否处在排空段（`trim.events` 只在**进入**这一段时 +1）。
    pub draining: bool,
    /// 本拍是不是排空段的第一拍（用于 `events` 计数与打日志）。
    pub drain_started: bool,
    /// 环被读**空**了（`occupied == 0`）。
    ///
    /// ## 这个判据是什么，不是什么
    ///
    /// 麦克风方向的欠载发生在**驱动进程里**：`AudioHubBridge_ReadRing` 取不满
    /// 就补静音，不给我们任何回执。所以 daemon 侧无法直接观测它，只能观测
    /// 「有没有可能」。
    ///
    /// - 短读 ⇒ 那一刻环是空的 ⇒ **必要条件**；
    /// - 环空过 ⇏ 短读：我们的观测点在**写之前**、正好是驱动刚读完的波谷，
    ///   而下一次读之前我们已经补了 480 帧。
    ///
    /// 所以它是短读次数的**上界**，只能这么用：**恒为 0 ⇒ 一次短读都不可能
    /// 发生**（这是它唯一的强结论）；非 0 ⇒ 值得去看 `probe capture` 的波形，
    /// 不等于已经出事。
    ///
    /// ⚠ 判据**不是** `occupied < Q_C`。第一版这么写过，部署后实测 60 s 里
    /// 报了 95 730 次「欠载」，而同一分钟的 `probe capture` 是 59.99 s 连续
    /// 无洞的完美录音——因为真实自由带是 32–608 帧，**整条带都在一个消费量子
    /// 之下**。一个在系统健康时尖叫的指标比没有指标更坏：它会让下一个排障的人
    /// 认定这里有病然后去修一个不存在的问题（与 `assemble_pipelines` 那条
    /// 「绝不用 0 填补」同源）。权威计数只能加在驱动侧。
    pub starved: bool,
}

/// 一个槽的闸门状态。**纯状态机**，不碰环、不碰原子量，可以被对抗性测试穷举。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicGate {
    /// 是否处在排空段。迟滞的载体：进入靠 `D_CEIL`，退出靠 `D_FLOOR`。
    draining: bool,
}

impl MicGate {
    pub fn new() -> MicGate {
        MicGate { draining: false }
    }

    /// 一次会话结束 / 槽解绑时复位。
    ///
    /// 排空段跨会话保留没有意义：下一条会话的水位要重新观测。而**保留**它
    /// 反而危险——上一条会话结束在排空段中间时，新会话的第一拍会无条件少写。
    pub fn reset(&mut self) {
        self.draining = false;
    }

    /// 本拍写多少。`occupied` 是**写之前**读到的环内积压（`mic_depth`），
    /// `want` 是本拍本来要写的帧数（恒为 `Q_P`，参数化只为测试）。
    ///
    /// 判据顺序是要害，不可交换：
    ///
    /// 1. **地板优先**（不变式 `I_FLOOR`）：水位低于 `D_FLOOR` 就无条件全写，
    ///    连排空段也就地结束。任何后续判据都不可能推翻它——这一行是欠载
    ///    构造性论证的全部依据，把它放在第一位是为了让「绕过地板」在结构上
    ///    不可表达，而不是靠调用方守规矩。
    /// 2. 已在排空段 ⇒ 继续少写（迟滞，避免在天花板上抖动）。
    /// 3. 水位越过 `D_CEIL` ⇒ 进入排空段。
    pub fn decide(&mut self, occupied: u32, want: u32) -> MicPlan {
        let starved = occupied == 0;

        // 1. 地板：结构上不可绕过。
        if occupied < D_FLOOR {
            self.draining = false;
            return MicPlan {
                allow: want,
                withheld: 0,
                draining: false,
                drain_started: false,
                starved,
            };
        }

        // 2/3. 迟滞：`D_CEIL` 进、`D_FLOOR` 出（出口已在第 1 步处理掉）。
        let drain_started = !self.draining && occupied >= D_CEIL;
        if drain_started {
            self.draining = true;
        }

        if self.draining {
            // 一次**连续**的空洞代替**永久**的延迟——与 `hal_spk` 治法 A
            // 同一条教义。分散成每拍丢一点会把一个可听的断点摊成一段听起来
            // 像故障的持续毛刺。
            MicPlan {
                allow: 0,
                withheld: want,
                draining: true,
                drain_started,
                starved,
            }
        } else {
            MicPlan { allow: want, withheld: 0, draining: false, drain_started: false, starved }
        }
    }

    #[cfg(test)]
    pub fn is_draining(&self) -> bool {
        self.draining
    }
}

/// 帧 → 毫秒（48 kHz）。发布读数用。
pub fn frames_to_ms(frames: u32) -> f32 {
    frames as f32 / 48.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条**没有闸门**的自由带，取值直接抄自 2026-08-04 的实测采样
    /// （60 s 干净会话，步长 32 帧）。用它做"闸门不该动手"的对照。
    ///
    /// 不用推导值：推导给的是 992–1984 帧，而实测整条带在 992 之下——
    /// 拿推导值写这条测试，它对真实系统一个字都没说。
    const MEASURED_BAND: [u32; 16] =
        [32, 64, 96, 160, 192, 224, 256, 288, 320, 352, 384, 480, 512, 544, 576, 608];

    #[test]
    fn t1_the_free_running_band_never_triggers_the_gate() {
        // 稳态下闸门必须**一次都不动手**。它动手了就说明 D_CEIL 定低了，
        // 而那会把一个健康的会话变成持续断续——麦克风方向最难发现的故障。
        let mut g = MicGate::new();
        for occ in MEASURED_BAND {
            let p = g.decide(occ, Q_P);
            assert_eq!(p.allow, Q_P, "自由带内水位 {occ} 帧被少写了");
            assert_eq!(p.withheld, 0);
            assert!(!p.draining);
        }
    }

    #[test]
    fn t2_a_five_hundred_millisecond_backlog_drains_to_the_floor_then_stops() {
        // 实测现场：E4 尾把环灌到 24000 帧 = 500 ms 饱和。
        let mut g = MicGate::new();
        let mut occ: u32 = 24_000;
        let mut ticks = 0;
        let mut withheld_total: u64 = 0;
        // 驱动照常按 Q_P/拍的平均速率取走（真实消费是 512/10.67 ms，
        // 长期均值等于 480/10 ms）。
        while occ >= D_FLOOR && ticks < 200 {
            let p = g.decide(occ, Q_P);
            withheld_total += p.withheld as u64;
            occ = occ + p.allow - Q_P.min(occ);
            ticks += 1;
        }
        assert!(ticks < 60, "排空用了 {ticks} 拍，超出一次可接受的空洞长度");
        assert!(occ < D_FLOOR, "排空没停在地板上：{occ} 帧");
        // 空洞长度 = 丢掉的帧数。它是**一次连续**的，不是摊开的毛刺。
        assert_eq!(withheld_total, ticks as u64 * Q_P as u64);
        // 排空之后闸门立刻松手。
        let p = g.decide(occ, Q_P);
        assert_eq!(p.allow, Q_P, "排空结束后仍在少写");
    }

    #[test]
    fn t3_the_gate_never_starves_the_driver_under_adversarial_depths() {
        // 不变式 I_FLOOR 的对抗性检验：**任意**水位序列（含跳变、含贴地、
        // 含饱和）下，闸门都不许在水位低于 D_FLOOR 时少写。
        //
        // 这条测试是欠载构造性论证的机械化版本。它变红的唯一方式是有人把
        // 地板判据挪到迟滞判据后面——而那正是"排空一路排穿"的形态。
        let mut g = MicGate::new();
        // 确定性伪随机：可复现，且比手写几个用例覆盖得宽。
        let mut x: u32 = 0x9E37_79B9;
        for _ in 0..200_000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let occ = x % 24_001; // [0, 500 ms]
            let p = g.decide(occ, Q_P);
            if occ < D_FLOOR {
                assert_eq!(p.allow, Q_P, "水位 {occ} 帧 < 地板，却少写了");
                assert_eq!(p.withheld, 0);
                assert!(!g.is_draining(), "地板之下排空段没有就地结束");
            }
            assert!(p.allow + p.withheld == Q_P, "allow + withheld 必须守恒");
        }
    }

    #[test]
    fn t4_hysteresis_means_touching_the_ceiling_once_does_not_latch_forever() {
        let mut g = MicGate::new();
        // 刚好越过天花板：进排空。
        let p = g.decide(D_CEIL, Q_P);
        assert!(p.drain_started && p.draining);
        // 天花板与地板之间：迟滞保持排空（否则会在天花板上每拍抖一次）。
        let p = g.decide((D_CEIL + D_FLOOR) / 2, Q_P);
        assert!(p.draining && !p.drain_started, "迟滞段应继续排空且不重复计事件");
        // 落到地板以下：松手。
        let p = g.decide(D_FLOOR - 1, Q_P);
        assert!(!p.draining);
        // 再回到迟滞段但没到天花板：**不得**重新排空。
        let p = g.decide(D_CEIL - 1, Q_P);
        assert!(!p.draining, "没越过天花板就不该排空");
    }

    #[test]
    fn t5_reset_clears_the_drain_state_across_sessions() {
        let mut g = MicGate::new();
        g.decide(24_000, Q_P);
        assert!(g.is_draining());
        g.reset();
        // 新会话第一拍水位正常 ⇒ 必须全写。保留排空段会让新会话开头静音。
        let p = g.decide(D_FLOOR + 1, Q_P);
        assert_eq!(p.allow, Q_P, "复位后仍在排空 —— 新会话开头会录到静音");
    }

    #[test]
    fn t6_starvation_means_the_ring_was_emptied_not_merely_shallow() {
        let mut g = MicGate::new();
        assert!(g.decide(0, Q_P).starved, "环被读空必须报出来");
        assert!(!g.decide(32, Q_P).starved, "32 帧是实测自由带的下沿，不是欠载");
        // ⚠ 这一条是回归防线。第一版判据是 `occupied < Q_C`，部署后 60 s 报了
        // 95 730 次「欠载」，而同一分钟的录音是 59.99 s 连续无洞——因为实测
        // 自由带 32–608 帧**整条都在一个消费量子之下**。在系统健康时尖叫的
        // 指标比没有指标更坏。
        for occ in MEASURED_BAND {
            assert!(!g.decide(occ, Q_P).starved, "实测自由带内的 {occ} 帧被报成欠载");
        }
        // 欠载判据与少写判据**互不影响**：环空时照样全写（地板优先）。
        assert_eq!(g.decide(0, Q_P).allow, Q_P);
    }

    #[test]
    fn t7_the_thresholds_bracket_the_measured_band() {
        // 不变式的两个前提，写成断言免得将来有人调参时把它们调穿。
        let top = *MEASURED_BAND.iter().max().unwrap();
        let bottom = *MEASURED_BAND.iter().min().unwrap();
        assert_eq!(top, D_BAND_TOP, "常量与实测采样对不上了");
        assert!(
            D_FLOOR > bottom,
            "地板 {D_FLOOR} 不高于无闸门系统自己访问的最低点 {bottom} —— 不变式不成立"
        );
        assert!(
            D_CEIL > top + (top - bottom),
            "天花板 {D_CEIL} 距自由带上沿 {top} 不足一个带宽，稳态会误伤"
        );
        assert!(D_FLOOR >= Q_P, "地板不足一个生产量子，排空的落点会贴在带底");
    }
}
