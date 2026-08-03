//! 传输档位的**活体状态**：用户选了什么，媒体面此刻真的在做什么。
//!
//! # 这个模块是「修改即生效」的那条路
//!
//! `settings.set` 写完盘之后调 [`TransportControl::publish`]，把两个档位放进一组
//! 原子量。音频线程（10 ms 的 `tx_loop` / `rx` 路径）每拍 `load(Relaxed)` 一次——
//! **不加锁、不分配、不阻塞**，与既有的 `TxShared.rung` 完全同一个形状。
//! 没有重启，没有重连，没有「下次连接时生效」。
//!
//! # 为什么是原子量而不是「读 settings 锁」
//!
//! `inner.settings` 是 `Mutex<StoredSettings>`。协调线程（200 ms）与 IPC 线程
//! 拿它没问题，**10 ms 的音频线程不行**：那条路径上只允许常数时间的原子操作
//! （`quality.rs` 文件头、遥测规格附录约束 3 都写着同一条）。于是档位在这里
//! 被解析一次、拍扁成整数，音频线程只读整数。
//!
//! # 编码：0 是「AUTO」，不是「第 0 档」
//!
//! `quality_rate` / `latency_ms` 都用 0 表示 AUTO。质量档的 0 不会与真档位相撞
//! （采样率不可能是 0）；延迟档的 0 会——用户选的 0 ms 是一个合法档位。
//! 所以延迟走**两个**原子量：`latency_fixed`（是不是固定档）与 `latency_ms`。
//! 一个 `u32` 塞两件事，就是下一个人读错的地方。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use audiohub_ipc::{LatencyTarget, QualityTarget};

/// 媒体面**真的**在做什么。全部由伺服/阶梯写，`settings.get` 读。
///
/// 它与用户选的档位是两个不同的东西，UI 必须显示这一个。
/// 把目标值当成当前值回显，是本项目栽过五次的那个形态。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct TransportLive {
    /// 实测端到端总延迟（ms）。`None` = 还没测出来。
    pub achieved_ms: Option<f64>,
    pub at_floor: bool,
    pub at_ceiling: bool,
    /// 当前实际线上采样率（Hz）。
    pub rate: Option<u32>,
    /// 正在被伺服的接收流数。0 = 没有会话，档位暂时没有作用对象——
    /// UI 要说这句，否则用户会以为设置没生效。
    pub streams: u32,
}

/// 音频线程能无锁读到的档位，加上伺服的输出。
#[derive(Debug)]
pub(crate) struct TransportControl {
    /// 固定质量档的采样率（Hz）。`0` = AUTO（阶梯当家）。
    quality_rate: AtomicU32,
    /// 延迟档是不是固定档。
    latency_fixed: AtomicBool,
    /// 固定档的目标总延迟（ms）。`latency_fixed == false` 时无意义。
    latency_ms: AtomicU32,
    /// 档位换过几次。**不是控制量**，只是 `publish` 用来判断「这次真的变了吗」
    /// 的内部计数：变了才作废旧的伺服输出与读数。
    ///
    /// 曾经它还兼任「rx 路径判断包络要不要重建」的信号，那一版有个先有鸡还是
    /// 先有蛋：第一个媒体包到达时伺服还没跑过，包络就按空值锁死了，此后代号
    /// 再不变化 ⇒ 永远不重建 ⇒ 滑条左半边完全无效。现在包络是**目标**的函数，
    /// 每秒重算、已经对了就早退（见 `engine::reshape_jitter_envelope`）。
    generation: AtomicU32,

    /// 伺服希望 JB 走到的深度（帧）。`0` = AUTO，rx 路径照旧用抖动公式。
    servo_frames: AtomicU32,
    /// 以下三个是给 UI 的读数，×100 存以避开浮点原子。
    live_ms_x100: AtomicU32,
    live_ms_valid: AtomicBool,
    live_at_floor: AtomicBool,
    live_at_ceiling: AtomicBool,
    live_rate: AtomicU32,
    live_streams: AtomicU32,
}

impl Default for TransportControl {
    fn default() -> Self {
        TransportControl {
            quality_rate: AtomicU32::new(0),
            latency_fixed: AtomicBool::new(false),
            latency_ms: AtomicU32::new(0),
            generation: AtomicU32::new(0),
            servo_frames: AtomicU32::new(0),
            live_ms_x100: AtomicU32::new(0),
            live_ms_valid: AtomicBool::new(false),
            live_at_floor: AtomicBool::new(false),
            live_at_ceiling: AtomicBool::new(false),
            live_rate: AtomicU32::new(0),
            live_streams: AtomicU32::new(0),
        }
    }
}

impl TransportControl {
    /// 把用户选的两个档位推给音频线程。**`settings.set` 落盘之后立刻调**，
    /// 于是「改了要重启」在这条路上不成立。
    ///
    /// 换代号只在**真的变了**时才 +1：每次 `settings.set`（哪怕只是改了别的
    /// 开关）都 +1 会让 rx 路径白重建一次 JB，每次重建都是一次重新预缓冲。
    pub(crate) fn publish(&self, lat: LatencyTarget, qual: QualityTarget) {
        let rate = match qual {
            QualityTarget::Auto => 0,
            QualityTarget::Rate(r) => r,
        };
        let (fixed, ms) = match lat {
            LatencyTarget::Auto => (false, 0),
            LatencyTarget::TotalMs(m) => (true, m as u32),
        };
        // 三个 swap **全部要执行**，所以先各自求值再合并：写成
        // `a() || b() || c()` 会短路，第一个就变了的话后两个原子量根本不会被更新。
        let q_moved = self.quality_rate.swap(rate, Ordering::Relaxed) != rate;
        let f_moved = self.latency_fixed.swap(fixed, Ordering::Relaxed) != fixed;
        let m_moved = self.latency_ms.swap(ms, Ordering::Relaxed) != ms;
        // `latency_ms` 在 AUTO 下无意义，它单独变了不算变（AUTO -> AUTO 时
        // `ms` 恒为 0，不会误判；固定档之间换值 `f_moved` 为假而 `m_moved` 为真，
        // 那才是真的变了）。
        let changed = q_moved || f_moved || (fixed && m_moved);
        if changed {
            self.generation.fetch_add(1, Ordering::Release);
            // 档位换了，旧的伺服输出立刻作废：留着它会让 rx 路径在新目标下
            // 继续执行上一个目标算出来的深度，直到伺服下一拍（最多 1 s）。
            self.servo_frames.store(0, Ordering::Relaxed);
            self.live_ms_valid.store(false, Ordering::Relaxed);
            self.live_at_floor.store(false, Ordering::Relaxed);
            self.live_at_ceiling.store(false, Ordering::Relaxed);
        }
    }

    /// 固定质量档的采样率，`None` = AUTO。音频线程每拍读它。
    pub(crate) fn quality_rate(&self) -> Option<u32> {
        match self.quality_rate.load(Ordering::Relaxed) {
            0 => None,
            r => Some(r),
        }
    }

    pub(crate) fn latency_target(&self) -> LatencyTarget {
        if self.latency_fixed.load(Ordering::Relaxed) {
            LatencyTarget::TotalMs(self.latency_ms.load(Ordering::Relaxed) as u16)
        } else {
            LatencyTarget::Auto
        }
    }

    /// 伺服希望的 JB 深度（帧）。`None` = AUTO / 还没算出来 ⇒ rx 路径照旧
    /// 用抖动公式（plan §5 的 AUTO 语义）。
    pub(crate) fn servo_frames(&self) -> Option<u32> {
        match self.servo_frames.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    pub(crate) fn set_servo_frames(&self, frames: Option<u32>) {
        self.servo_frames
            .store(frames.unwrap_or(0), Ordering::Relaxed);
    }

    /// 伺服每拍把读数写进来。
    pub(crate) fn set_live(&self, live: TransportLive) {
        match live.achieved_ms {
            Some(ms) if ms.is_finite() && ms >= 0.0 => {
                self.live_ms_x100
                    .store((ms * 100.0).round().min(u32::MAX as f64) as u32, Ordering::Relaxed);
                self.live_ms_valid.store(true, Ordering::Relaxed);
            }
            // 测不到就说测不到。**绝不留上一次的值**——一条断了的流会显示成
            // 「还是那么快」，而那正是最该报警的时候。
            _ => self.live_ms_valid.store(false, Ordering::Relaxed),
        }
        self.live_at_floor.store(live.at_floor, Ordering::Relaxed);
        self.live_at_ceiling.store(live.at_ceiling, Ordering::Relaxed);
        self.live_rate.store(live.rate.unwrap_or(0), Ordering::Relaxed);
        self.live_streams.store(live.streams, Ordering::Relaxed);
    }

    pub(crate) fn live(&self) -> TransportLive {
        TransportLive {
            achieved_ms: self
                .live_ms_valid
                .load(Ordering::Relaxed)
                .then(|| self.live_ms_x100.load(Ordering::Relaxed) as f64 / 100.0),
            at_floor: self.live_at_floor.load(Ordering::Relaxed),
            at_ceiling: self.live_at_ceiling.load(Ordering::Relaxed),
            rate: match self.live_rate.load(Ordering::Relaxed) {
                0 => None,
                r => Some(r),
            },
            streams: self.live_streams.load(Ordering::Relaxed),
        }
    }
}

/// 采样率 -> 阶梯格号。`None` = 这个速率不在阶梯上。
///
/// **刻意不做就近吸附**：找不到就是找不到，调用方得决定怎么办。
/// 吸附会让一个不存在的档静默变成一个存在的档，而 UI 显示的还是原来那个。
pub(crate) fn rung_of_rate(rate: u32) -> Option<u32> {
    audiohub_net::media::AUTO_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发布之后**立刻**读得到——中间没有任何一步需要重启、重连或等下一拍。
    #[test]
    fn a_published_choice_is_readable_immediately() {
        let c = TransportControl::default();
        assert_eq!(c.quality_rate(), None, "默认是 AUTO");
        assert_eq!(c.latency_target(), LatencyTarget::Auto);

        c.publish(LatencyTarget::TotalMs(200), QualityTarget::Rate(24_000));
        assert_eq!(c.quality_rate(), Some(24_000));
        assert_eq!(c.latency_target(), LatencyTarget::TotalMs(200));

        c.publish(LatencyTarget::Auto, QualityTarget::Auto);
        assert_eq!(c.quality_rate(), None);
        assert_eq!(c.latency_target(), LatencyTarget::Auto);
    }

    /// 换档之后旧的伺服输出立刻作废；**重复发布同一个档位则什么都不动**。
    ///
    /// 后半句是承重的：`settings.set` 改任何一个别的开关都会走一遍 `publish`，
    /// 若每次都作废，伺服会被反复清零、永远收敛不到目标。
    #[test]
    fn changing_the_target_invalidates_the_previous_servo_output() {
        let c = TransportControl::default();
        c.publish(LatencyTarget::TotalMs(100), QualityTarget::Auto);
        c.set_servo_frames(Some(7));
        c.set_live(TransportLive { achieved_ms: Some(101.0), ..Default::default() });
        assert_eq!(c.servo_frames(), Some(7));

        // 同一个档位再发一次：不许动。
        c.publish(LatencyTarget::TotalMs(100), QualityTarget::Auto);
        assert_eq!(c.servo_frames(), Some(7), "重复发布同一档位却把伺服清零了");
        assert_eq!(c.live().achieved_ms, Some(101.0), "重复发布把读数也清了");

        c.publish(LatencyTarget::TotalMs(300), QualityTarget::Auto);
        assert_eq!(c.servo_frames(), None, "换档后旧深度必须作废");
        assert_eq!(c.live().achieved_ms, None, "换档后旧读数必须作废");

        // 质量档单独变化同样算「变了」。
        c.set_servo_frames(Some(3));
        c.publish(LatencyTarget::TotalMs(300), QualityTarget::Rate(48_000));
        assert_eq!(c.servo_frames(), None, "质量档变了也要作废旧输出");
    }

    /// 测不到就是 `None`，**绝不留上一次的值**。
    #[test]
    fn a_lost_measurement_clears_the_readout_rather_than_going_stale() {
        let c = TransportControl::default();
        c.set_live(TransportLive { achieved_ms: Some(123.45), streams: 2, ..Default::default() });
        assert_eq!(c.live().achieved_ms, Some(123.45));
        c.set_live(TransportLive { achieved_ms: None, streams: 2, ..Default::default() });
        assert_eq!(
            c.live().achieved_ms,
            None,
            "读数丢了却还显示上一次的值——断流时会显示成一切正常"
        );
        // NaN / 负数同样按「测不到」处理，不许写进读数
        c.set_live(TransportLive { achieved_ms: Some(f64::NAN), ..Default::default() });
        assert_eq!(c.live().achieved_ms, None);
        c.set_live(TransportLive { achieved_ms: Some(-1.0), ..Default::default() });
        assert_eq!(c.live().achieved_ms, None);
    }

    #[test]
    fn the_readout_round_trips_its_flags() {
        let c = TransportControl::default();
        let want = TransportLive {
            achieved_ms: Some(88.25),
            at_floor: true,
            at_ceiling: false,
            rate: Some(32_000),
            streams: 3,
        };
        c.set_live(want);
        assert_eq!(c.live(), want);
    }

    /// 阶梯外的速率**不被吸附**。吸附会让「用户选了一个不存在的档」静默变成
    /// 「daemon 执行了一个别的档」，而 UI 两边都不会报错。
    #[test]
    fn a_rate_outside_the_ladder_is_refused_not_snapped() {
        for (rate, want) in [(48_000u32, Some(0u32)), (32_000, Some(1)), (24_000, Some(2)), (16_000, Some(3))] {
            assert_eq!(rung_of_rate(rate), want, "{rate} 的格号不对");
        }
        for bad in [0, 1, 8_000, 44_100, 47_999, 96_000] {
            assert_eq!(rung_of_rate(bad), None, "{bad} 不在阶梯上，不该给出格号");
        }
    }

    /// 每一个**可选**的质量档都能落到一个真实格号上。
    ///
    /// 这条把 `transport.rs`（契约）与 `AUTO_RATES`（能力）扣在一起：
    /// 契约里多出一档而阶梯里没有，这条就红——而没有它，那一档会被
    /// `settings.set` 收下、写盘、然后在 `rung_of_rate` 处**静默**回落。
    #[test]
    fn every_selectable_quality_stop_maps_onto_a_real_rung() {
        for stop in audiohub_ipc::transport::quality_stops() {
            let Some(t) = QualityTarget::parse(&stop.id) else {
                assert!(!stop.available, "{} 可选却 parse 不了", stop.id);
                continue;
            };
            if let QualityTarget::Rate(r) = t {
                assert!(
                    rung_of_rate(r).is_some(),
                    "质量档 {} 的速率 {r} 不在 AUTO_RATES 上：选中它会被静默忽略",
                    stop.id
                );
            }
        }
    }
}
