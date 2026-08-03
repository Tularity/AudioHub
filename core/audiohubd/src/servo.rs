//! 延迟目标的伺服：把**端到端总延迟**拉到用户选的那个数。
//!
//! # 被控量是 `sum_ms`，不是缓冲深度
//!
//! 用户的原话是「区间内固定延迟需要考虑实际与对方连接的延迟包含进去等于这个数」。
//! 于是这条回路的被控量只能是 `PipelineLatency.sum_ms`（本侧 Σ + 网络段 +
//! 对端 Σ），而**唯一的操纵量是 jitter buffer 深度**——采集环、声卡、网络、
//! 播放环全都不由这里决定。
//!
//! ```text
//!   floor_ms = sum_ms − jb_ms        ← 动不了的那部分（每拍重新测，不是常数）
//!   need_ms  = target_ms − floor_ms  ← JB 必须贡献多少
//!   want     = clamp(round(need_ms / 10), envelope)
//! ```
//!
//! `floor_ms` 每拍从实测里反解，而不是标定一次存起来：默认设备换了、链路变了、
//! 对端换了声卡，地板就变了，一个缓存下来的地板会让伺服朝着一个不存在的目标走。
//!
//! # 够不到就说够不到
//!
//! 地板 90 ms 而用户选 0，回路停在下限并置 `at_floor`；UI 显示**实测值**加
//! 「已达物理下限」。把目标值当成当前值回显，是本项目栽过五次的那个形态
//! （「一切都报成功，什么都没发生」）。`at_ceiling` 同理，方向相反。
//!
//! # 为什么要死区和限速
//!
//! JB 内部还有一条自己的回路（欠载惩罚 `extra`，见 `media::JbTuning`）。两条
//! 回路盯着同一个水位，没有死区就会互相追：伺服削一帧、欠载加一帧、伺服再削。
//! 死区取半帧（5 ms），限速取每拍 1 帧——1 s 一帧即 ρ ≈ 1 %，与 JB 自己的
//! `accel_interval_ticks` 同一个量级，不会比它更急。

use audiohub_ipc::LatencyTarget;

/// 一帧的毫秒数。与 `engine::FRAME_MS` 同值——**同值不同源是隐患**，
/// 由 `the_frame_length_agrees_with_the_engine` 钉住。
pub(crate) const FRAME_MS: f64 = 10.0;

/// 死区（毫秒）。误差落在 ±半帧内就不动——反正一帧是最小可执行步长，
/// 半帧以内的误差**无论如何都消不掉**，动了只会来回摆。
const DEADBAND_MS: f64 = FRAME_MS / 2.0;

/// 每拍最多移动几帧。
const MAX_STEP_FRAMES: u32 = 1;

/// 伺服一拍的输入。**全部是实测量**，没有一个是配置常数——这是刻意的：
/// 这个函数不许知道任何整定表，于是它不可能「按标称值算得很对而现场是错的」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ServoIn {
    /// 用户选的端到端总延迟目标。
    pub target: LatencyTarget,
    /// 实测端到端总延迟（ms）。`None` = 还没测出来（对端分项未到 / RTT 窗口
    /// 未攒够）——**此时不动**，不拿一个猜的数去伺服。
    pub sum_ms: Option<f64>,
    /// 当前 JB 有效目标深度（帧）。就是 `JitterBuffer::target()`。
    pub jb_frames: u32,
    /// JB 深度的物理下界（帧）。
    pub lo_frames: u32,
    /// JB 深度的物理上界（帧）。
    pub hi_frames: u32,
}

/// 伺服一拍的输出。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ServoOut {
    /// 这一拍希望 JB 走到的深度（帧）。AUTO 或无读数时等于 `jb_frames`（不动）。
    pub want_frames: u32,
    /// 目标够不到、已经贴在下限上。UI 据此显示「已达物理下限」。
    pub at_floor: bool,
    /// 目标够不到、已经贴在上限上。
    pub at_ceiling: bool,
}

/// 一拍。**纯函数**：没有锁、没有时间、没有全局状态，于是每一条分支都能被直接测。
pub(crate) fn step(i: ServoIn) -> ServoOut {
    let hold = ServoOut {
        want_frames: i.jb_frames,
        at_floor: false,
        at_ceiling: false,
    };
    // AUTO：这条回路完全不参与，抖动驱动的 `update_target` 继续当家（plan §5）。
    let LatencyTarget::TotalMs(target_ms) = i.target else {
        return hold;
    };
    // 上下界颠倒（整定表被写坏）时不要放大成一个荒谬的深度。
    let (lo, hi) = (i.lo_frames.min(i.hi_frames), i.lo_frames.max(i.hi_frames));

    let jb_ms = i.jb_frames as f64 * FRAME_MS;
    // 反解地板：总延迟里减掉我们唯一能动的那一级。
    //
    // # `sum_ms` 还没测出来时怎么办（开环预置）
    //
    // `sum_ms` 要等 min-RTT 窗口攒够 8 个样本（≈8 s）**且**对端的分项上报到达。
    // 「测不到就完全不动」听上去最保守，实际是最坏的一种：用户拖了滑条，
    // 十几秒里什么都不发生；对端若是个不上报分项的旧版本，**永远**不发生，
    // 而且不会有任何一处报错——正是本项目要消灭的那个形态。
    //
    // 所以缺读数时按 `floor_ms = 0` 走一个**开环预置**。这不是「用 0 填补缺失
    // 分项」（那条红线管的是**上报**的数，`achieved_ms` 缺读数时照旧是 `None`，
    // 见 `TransportControl::set_live`），而是一个可证明不会过冲的初始条件：
    //
    //   JB 的驻留时间是端到端总延迟的**真子集** ⇒ `jb_ms ≤ sum_ms` 恒成立
    //   ⇒ 「JB 不许超过总目标」与任何测量无关地成立。
    //
    // `floor_ms = 0` 给出的正是这个上界。闭环一旦有读数就从这里往下收敛到真解。
    let floor_ms = match i.sum_ms {
        Some(sum) => (sum - jb_ms).max(0.0),
        None => 0.0,
    };
    let need_ms = target_ms as f64 - floor_ms;

    // 理想深度（帧），再夹进物理包络。
    let ideal = (need_ms / FRAME_MS).round();
    let ideal_frames = if ideal <= 0.0 { 0 } else { ideal as u32 };
    let want = ideal_frames.clamp(lo, hi);

    // 够不到的两侧：夹住之后**实际会落在哪**，与目标比。
    // 判据用 `want`（夹后）而不是 `ideal`（夹前）：贴边但恰好达标不算够不到。
    //
    // **只有闭环（真有读数）时才敢下这个结论。** 开环预置下 `floor_ms` 是假的 0，
    // 拿它去断言「已达物理下限」等于凭空宣布一个我们根本没测过的物理事实——
    // 而 UI 会把那句话原样显示给用户。
    let measured = i.sum_ms.is_some();
    let reachable_ms = floor_ms + want as f64 * FRAME_MS;
    let at_floor = measured && want == lo && reachable_ms > target_ms as f64 + DEADBAND_MS;
    let at_ceiling = measured && want == hi && reachable_ms < target_ms as f64 - DEADBAND_MS;

    // 死区：误差半帧以内不动。用**当前**深度算的误差，不是用 want 算的。
    let now_ms = floor_ms + jb_ms;
    if (now_ms - target_ms as f64).abs() <= DEADBAND_MS {
        return ServoOut { want_frames: i.jb_frames, at_floor, at_ceiling };
    }

    // 限速：一拍最多挪一帧。
    let step = MAX_STEP_FRAMES;
    let want_frames = if want > i.jb_frames {
        i.jb_frames.saturating_add(step).min(want)
    } else if want < i.jb_frames {
        i.jb_frames.saturating_sub(step).max(want)
    } else {
        i.jb_frames
    };
    ServoOut { want_frames, at_floor, at_ceiling }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ServoIn {
        ServoIn {
            target: LatencyTarget::TotalMs(100),
            sum_ms: Some(100.0),
            jb_frames: 4,
            lo_frames: 1,
            hi_frames: 40,
        }
    }

    /// 帧长这一个数在两个 crate 里各写了一份。漂了之后伺服会按 10 ms 一帧算，
    /// 而媒体面按别的走 —— 每一步都会算错，且没有任何一处会报错。
    #[test]
    fn the_frame_length_agrees_with_the_engine() {
        assert_eq!(
            FRAME_MS, crate::engine::FRAME_MS as f64,
            "servo 与 engine 的帧长不一致：伺服每一步的换算都会偏"
        );
    }

    /// AUTO 时这条回路**一步都不许走**——plan §5 把 AUTO 判给了抖动驱动的
    /// `update_target`，伺服插一脚就是两条回路抢同一个水位。
    #[test]
    fn auto_never_moves_the_buffer() {
        for sum in [None, Some(10.0), Some(1000.0)] {
            let out = step(ServoIn { target: LatencyTarget::Auto, sum_ms: sum, ..base() });
            assert_eq!(out.want_frames, 4, "AUTO 下伺服动了深度（sum={sum:?}）");
            assert!(!out.at_floor && !out.at_ceiling, "AUTO 不该报够不到");
        }
    }

    /// 没有读数时走**开环预置**：朝「JB 不许超过总目标」这个上界走。
    ///
    /// 「测不到就完全不动」才是坏的：对端若是个不上报分项的旧版本，
    /// `sum_ms` 永远是 `None`，滑条就永远没有作用，而且不会有任何一处报错。
    #[test]
    fn a_missing_measurement_still_bounds_the_buffer_by_the_target() {
        // 目标 0 ms ⇒ 上界 0 帧 ⇒ 往下走（限速一帧）
        let low = step(ServoIn { sum_ms: None, target: LatencyTarget::TotalMs(0), ..base() });
        assert!(low.want_frames < 4, "开环下也该朝目标走：{low:?}");
        // 目标 1000 ms ⇒ 上界 100 帧 ⇒ 往上走
        let high = step(ServoIn {
            sum_ms: None,
            target: LatencyTarget::TotalMs(1000),
            ..base()
        });
        assert!(high.want_frames > 4, "开环下高目标也该加深：{high:?}");
    }

    /// **开环时绝不宣布「已达物理下限」。**
    ///
    /// 那句话是一个关于物理的断言，而开环下我们把地板当成了 0——一个假设。
    /// 拿假设当测量结果显示给用户，正是这一轮要消灭的形态。
    #[test]
    fn without_a_measurement_the_servo_claims_nothing_about_the_physical_floor() {
        for target in [0u16, 10, 1000] {
            let out = step(ServoIn {
                sum_ms: None,
                target: LatencyTarget::TotalMs(target),
                jb_frames: 1,
                lo_frames: 1,
                hi_frames: 12,
            });
            assert!(
                !out.at_floor && !out.at_ceiling,
                "没有读数却宣布够不到（target={target}）：{out:?}"
            );
        }
        // 正向对照：一旦有读数，同样的边界立刻**敢**下结论——否则上面的
        // 「都是 false」只是因为这两个标志根本没被实现。
        let measured = step(ServoIn {
            sum_ms: Some(90.0 + FRAME_MS),
            target: LatencyTarget::TotalMs(0),
            jb_frames: 1,
            lo_frames: 1,
            hi_frames: 12,
        });
        assert!(measured.at_floor, "有读数时必须如实报「已达物理下限」");
    }

    /// **目标高于现状 ⇒ 加深；低于现状 ⇒ 削浅。** 方向搞反是最容易发生
    /// 又最难从「声音还行」里看出来的错误。
    #[test]
    fn the_servo_moves_toward_the_target_not_away_from_it() {
        // 现状 100 ms（地板 60 + JB 40），目标 200 ⇒ 必须加深
        let up = step(ServoIn { target: LatencyTarget::TotalMs(200), ..base() });
        assert!(up.want_frames > 4, "目标更高却没有加深：{up:?}");
        // 目标 50 ⇒ 必须削浅
        let down = step(ServoIn { target: LatencyTarget::TotalMs(50), ..base() });
        assert!(down.want_frames < 4, "目标更低却没有削浅：{down:?}");
    }

    /// 收敛：反复喂「地板不变、深度按上一拍输出走」，总延迟必须**停在目标上**。
    ///
    /// 这条才是真正的验收——单拍方向对不代表回路会停下来。
    #[test]
    fn the_loop_converges_onto_the_total_not_onto_a_buffer_size() {
        for target_ms in [50u16, 100, 200, 300] {
            let floor_ms = 42.0; // 采集 + 声卡 + 网络 + 播放，伺服动不了的那部分
            let mut frames = 4u32;
            for _ in 0..200 {
                let sum = floor_ms + frames as f64 * FRAME_MS;
                let out = step(ServoIn {
                    target: LatencyTarget::TotalMs(target_ms),
                    sum_ms: Some(sum),
                    jb_frames: frames,
                    ..base()
                });
                frames = out.want_frames;
            }
            let settled = floor_ms + frames as f64 * FRAME_MS;
            assert!(
                (settled - target_ms as f64).abs() <= FRAME_MS,
                "目标 {target_ms} ms 收敛到 {settled} ms（深度 {frames} 帧）—— \
                 差了不止一帧，回路没停在目标上"
            );
        }
    }

    /// **地板变了，回路要跟着重新收敛。** 这是「伺服总延迟」与「设了个缓冲深度」
    /// 的分水岭：把 `floor_ms` 缓存成常数的实现能过上一条测试，过不了这一条。
    #[test]
    fn a_moving_floor_is_re_solved_every_tick_not_cached() {
        let target = LatencyTarget::TotalMs(200);
        let mut frames = 4u32;
        // 先在 40 ms 地板上收敛
        for _ in 0..200 {
            let sum = 40.0 + frames as f64 * FRAME_MS;
            frames = step(ServoIn { target, sum_ms: Some(sum), jb_frames: frames, ..base() })
                .want_frames;
        }
        let first = frames;
        // 网络恶化：地板涨到 120 ms。总延迟目标不变 ⇒ JB 必须让出 80 ms。
        for _ in 0..200 {
            let sum = 120.0 + frames as f64 * FRAME_MS;
            frames = step(ServoIn { target, sum_ms: Some(sum), jb_frames: frames, ..base() })
                .want_frames;
        }
        assert!(
            frames < first,
            "地板从 40 涨到 120 ms，JB 却没有让出深度（{first} -> {frames} 帧）—— \
             说明实现把地板当成了常数，那就不是在伺服总延迟"
        );
        let settled = 120.0 + frames as f64 * FRAME_MS;
        assert!(
            (settled - 200.0).abs() <= FRAME_MS,
            "地板变化后没有重新收敛到 200 ms（现在 {settled} ms）"
        );
    }

    /// 够不到的时候**如实报**，并且停在边界上不再挣扎。
    #[test]
    fn an_unreachable_target_reports_the_floor_instead_of_pretending() {
        // 地板 90 ms，下限 2 帧 ⇒ 最好也就 110 ms，而用户选 0
        let out = step(ServoIn {
            target: LatencyTarget::TotalMs(0),
            sum_ms: Some(90.0 + 4.0 * FRAME_MS),
            jb_frames: 4,
            lo_frames: 2,
            hi_frames: 40,
        });
        assert!(out.at_floor, "够不到却没有置 at_floor：UI 会显示成达标了");
        assert!(!out.at_ceiling);
        assert!(out.want_frames < 4, "还没到下限就该继续往下走");
        // 已经贴在下限上：不动，且仍然如实报 at_floor
        let stuck = step(ServoIn {
            target: LatencyTarget::TotalMs(0),
            sum_ms: Some(90.0 + 2.0 * FRAME_MS),
            jb_frames: 2,
            lo_frames: 2,
            hi_frames: 40,
        });
        assert_eq!(stuck.want_frames, 2, "已在下限还要往下挤");
        assert!(stuck.at_floor, "贴在下限上时必须持续如实上报");
    }

    /// 目标高得连上限都够不到 ⇒ `at_ceiling`。
    #[test]
    fn a_target_above_the_envelope_reports_the_ceiling() {
        let out = step(ServoIn {
            target: LatencyTarget::TotalMs(1000),
            sum_ms: Some(40.0 + 12.0 * FRAME_MS),
            jb_frames: 12,
            lo_frames: 2,
            hi_frames: 12,
        });
        assert!(out.at_ceiling, "上限只能给到 160 ms，目标 1000 ms 却没报够不到");
        assert!(!out.at_floor);
        assert_eq!(out.want_frames, 12, "已在上限");
    }

    /// 达标时**两个标志都不许亮**——贴在边界上但恰好达标不算够不到。
    #[test]
    fn a_target_that_is_met_at_the_boundary_is_not_reported_as_unreachable() {
        // 地板 80，下限 2 帧 ⇒ 恰好 100 ms，目标正是 100
        let out = step(ServoIn {
            target: LatencyTarget::TotalMs(100),
            sum_ms: Some(80.0 + 2.0 * FRAME_MS),
            jb_frames: 2,
            lo_frames: 2,
            hi_frames: 12,
        });
        assert!(!out.at_floor && !out.at_ceiling, "恰好达标被报成了够不到：{out:?}");
    }

    /// 死区：误差半帧以内不动，否则会和 JB 自己的欠载回路互相追。
    #[test]
    fn errors_inside_the_deadband_do_not_move_the_buffer() {
        for err in [-DEADBAND_MS, -1.0, 0.0, 1.0, DEADBAND_MS] {
            let out = step(ServoIn {
                target: LatencyTarget::TotalMs(100),
                sum_ms: Some(100.0 + err),
                jb_frames: 4,
                ..base()
            });
            assert_eq!(out.want_frames, 4, "误差 {err} ms 在死区内却动了");
        }
        // 死区外一点点就必须动
        let out = step(ServoIn {
            target: LatencyTarget::TotalMs(100),
            sum_ms: Some(100.0 + DEADBAND_MS + 1.0),
            jb_frames: 4,
            ..base()
        });
        assert_ne!(out.want_frames, 4, "刚出死区就该开始收敛");
    }

    /// 限速：一拍最多一帧。没有它，一次网络抖动就能让水位跳 8 帧，
    /// 听上去是一声明显的断裂。
    #[test]
    fn the_servo_never_jumps_more_than_one_frame_per_tick() {
        for (sum, target) in [(1000.0, 0u16), (10.0, 1000u16)] {
            let out = step(ServoIn {
                target: LatencyTarget::TotalMs(target),
                sum_ms: Some(sum),
                jb_frames: 10,
                lo_frames: 1,
                hi_frames: 40,
            });
            let d = (out.want_frames as i64 - 10).abs();
            assert!(d <= MAX_STEP_FRAMES as i64, "一拍挪了 {d} 帧（sum={sum}, target={target}）");
        }
    }

    /// 上下界写反了也不能变成一个荒谬的深度。
    #[test]
    fn an_inverted_envelope_does_not_explode() {
        let out = step(ServoIn {
            target: LatencyTarget::TotalMs(1000),
            sum_ms: Some(50.0),
            jb_frames: 5,
            lo_frames: 12,
            hi_frames: 2,
        });
        assert!((2..=12).contains(&out.want_frames), "包络颠倒时越界了：{out:?}");
    }
}
