//! `probe loopback` 的延迟**估算**（plan §9 M1 验收：「打印估算延迟」）。
//!
//! # 这个数是什么，以及**不是**什么
//!
//! 它是四个分项相加：麦克风设备固有延迟 + 采集环驻留 + 播放环驻留 + 输出设备
//! 固有延迟。前后两项是**设备自报**的属性读数（`devlat`），中间两项是本进程
//! 观测到的整数样本计数。
//!
//! **它没有做过环回实测标定。** 没有人往麦克风送一个已知信号再量它什么时候从
//! 喇叭出来——`probe loopback` 全程不知道自己的端到端延迟到底是多少。所以：
//!
//! - 输出里 `calibrated` 恒为 `false`，且 `note` 用大白话把这句话再说一遍；
//! - 定性走 `LatSource`，四项取**最不可信**的那个（`devlat` 的 `worse()` 同序）。
//!   Windows 上 `play_dev` 是 `Unreliable`（`GetDevicePeriod` 实测低报 4.2 倍），
//!   于是整个估算也只能是 `Unreliable`——这正是它该有的样子。
//!
//! 为什么要这么小心：本项目已经因为「日志给出一个自信的数字，读的人当成实测」
//! 吃过亏（§二.7）。一个标着「估算、未标定、下限」的数字有用；同一个数字不带
//! 这些标签则是有害的，因为它会把所有人劝离真正的排查方向。
//!
//! # 分项为什么不能合并成一个数
//!
//! 与 Windows 驱动那条「两级缓冲驻留深度必须**分别**报出」是同一条纪律：
//! 27 ms 里有 20 ms 是设备固有、还是有 20 ms 堵在播放环里，是两个完全不同的
//! 结论，指向两套完全不同的动作。合计数字回答不了任何一个问题。
//!
//! # 纯函数
//!
//! 本模块**不碰设备**：输入是已经读好的 `DevLatencyParts` 与观测好的环深度。
//! 这样这套换算与判空逻辑可以在任何主机上单测，而不需要真的开一次麦克风。

use audiohub_core::devlat::DevLatencyParts;
use audiohub_core::latency::LatSource;

/// 环深度的采样口径。**必须随数字一起发出去**：环驻留是瞬时量，
/// 「结束时刻的快照」与「全程均值」是两个不同的量，差一个数量级都不奇怪。
pub const RING_SAMPLING_NOTE: &str = concat!(
    "ring depth is sampled once per poll of the copy loop: the capture ring ",
    "BEFORE its drain (that backlog is what a sample actually waited through) ",
    "and the playback ring AFTER the push (that is what sits ahead of the ",
    "sample just written). Reported figure is the arithmetic mean over the run; ",
    "`samples_max` is the worst single observation."
);

/// 这个估算的方法与它的边界，一字不改地进 JSON。
pub const METHOD_NOTE: &str = concat!(
    "ESTIMATE, NOT A MEASUREMENT. Sum of the two devices' self-reported ",
    "latency (read from platform properties) and the two ring residencies ",
    "observed in this process. No loopback calibration was performed: nothing ",
    "here timed a known signal from microphone to speaker, so the true ",
    "mic-to-speaker latency may be larger. Treat the total as a floor."
);

/// 一个环在整轮运行中的观测。
///
/// `polls == 0` ⇒ 这一级**没有观测**（循环一次都没跑），与「观测到 0 深度」是
/// 两回事——后者是真读数。前者必须报 `None`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingObservation {
    /// 各次采样深度之和（样本）。用累加和而不是均值入参，是为了让调用方在
    /// 10 ms 节拍上只做一次整数加法。
    pub samples_sum: u64,
    pub samples_max: u32,
    /// 采样次数。
    pub polls: u64,
    /// 这一级**消费者**的标称速率(Hz)。0 = 未知 ⇒ 换算不出毫秒。
    /// 与 `StageDepth.rate` 同一条规矩：播放环走设备速率，混用 48000 会引入
    /// 系统性偏差。
    pub rate: u32,
}

impl RingObservation {
    pub fn new(rate: u32) -> RingObservation {
        RingObservation { samples_sum: 0, samples_max: 0, polls: 0, rate }
    }

    /// 记一次观测。
    pub fn observe(&mut self, samples: u32) {
        self.samples_sum = self.samples_sum.saturating_add(samples as u64);
        self.samples_max = self.samples_max.max(samples);
        self.polls += 1;
    }

    /// 平均深度（样本）。没观测过 ⇒ `None`，**不得当 0 用**。
    pub fn samples_mean(&self) -> Option<f64> {
        (self.polls > 0).then(|| self.samples_sum as f64 / self.polls as f64)
    }

    /// 平均驻留时长。`rate == 0` 或没观测过 ⇒ `None`。
    pub fn ms(&self) -> Option<f64> {
        if self.rate == 0 {
            return None;
        }
        self.samples_mean().map(|s| s * 1000.0 / self.rate as f64)
    }

    /// 环深度是整数样本计数，单机单时钟内读取，**无任何估计成分**
    /// （`audio.rs` 的 `queued()` 原话）——所以定性是 `Api`。读不到才降级。
    pub fn source(&self) -> LatSource {
        match self.ms() {
            Some(_) => LatSource::Api,
            None => LatSource::Unavailable,
        }
    }
}

/// 四个分项的稳定 id。与 `latency.rs` 的九级测点命名同源（`cap_dev` / `play_dev`
/// 逐字相同），另外两项按本命令自己的环命名。
pub const STAGE_CAPTURE_DEV: &str = "capture_dev";
pub const STAGE_CAPTURE_RING: &str = "capture_ring";
pub const STAGE_PLAY_RING: &str = "play_ring";
pub const STAGE_PLAY_DEV: &str = "play_dev";

/// 分项在总和里的顺序 = 音频流经它们的顺序。
///
/// 进 JSON（`to_json` 的 `order`）而不是只留在 Rust 里：`parts` 是个对象，
/// **JSON 对象无序**，读的人没法从中恢复「哪一级在前」。而这个顺序恰恰是排障
/// 时的第一个问题——延迟堆在采集侧还是播放侧。
pub const STAGE_ORDER: [&str; 4] =
    [STAGE_CAPTURE_DEV, STAGE_CAPTURE_RING, STAGE_PLAY_RING, STAGE_PLAY_DEV];

#[derive(Debug, Clone, PartialEq)]
pub struct StageEstimate {
    pub id: &'static str,
    /// `None` = 这一级读不到。**调用方不得当 0 用**（latency.rs 文件头约束 1）。
    pub ms: Option<f64>,
    pub source: LatSource,
    /// 为什么是这个数 / 为什么读不到。人读的，进 JSON。
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopbackEstimate {
    pub stages: Vec<StageEstimate>,
    /// 四项之和。**任何一项缺失 ⇒ `None`**：少一项就少一段延迟，求和是全有
    /// 或全无（`DevLatencyParts::total()` 的第 2 条判据，同一条规矩）。
    pub total_ms: Option<f64>,
    /// 四项里**最不可信**的那个定性。`total_ms.is_some()` 时才有意义。
    pub source: LatSource,
    /// 缺了哪几项。`total_ms == None` 时这是唯一说得清「为什么没有总数」的地方。
    pub missing: Vec<&'static str>,
}

impl LoopbackEstimate {
    /// 这个总数够不够格被当成一个**精确**的端到端物理量？
    ///
    /// **永远是 false。** 不是保守，是事实：四项全 `Api` 也只说明每一项都是读来的，
    /// 而没有任何一项覆盖 ADC 之后到 DAC 之前那段以外的东西（驱动栈、混音器、
    /// 本命令 5 ms 的 sleep 节拍本身）。要把 `≥` 去掉，唯一的办法是真的做一次
    /// 环回实测标定——那是第二轮的活（需要声学或虚拟回环通路 + 麦克风授权）。
    ///
    /// 存在的理由与 `LatSource::is_exact` 一样：它**将来会被误用**。把判据写成
    /// 一个有名字的常量谓词，比写在注释里更难绕过。
    pub fn is_calibrated(&self) -> bool {
        false
    }
}

/// 从 `DevLatencyParts` 折出一个分项。
///
/// 直接复用 `parts.total()`——那里已经写好了四条判据（速率为 0 / 有缺项 /
/// 分项为空 / 后端自陈不可用），并且已经把传输方式的降级叠了进去（蓝牙、HDMI、
/// 聚合设备一律 `Unreliable`）。在这里另写一份判空只会让两处慢慢分叉。
fn device_stage(id: &'static str, parts: &DevLatencyParts) -> StageEstimate {
    let total = parts.total();
    let ms = total.ms();
    let detail = match ms {
        Some(_) => {
            let breakdown: Vec<String> =
                parts.parts.iter().map(|(n, f)| format!("{n}={f}f")).collect();
            format!(
                "{} frames @ {} Hz [{}] transport={:?} device={}",
                total.frames,
                total.rate,
                breakdown.join(" "),
                parts.transport,
                parts.device.as_deref().unwrap_or("?")
            )
        }
        // 读不到时**必须说清是哪一种读不到**：缺分项、缺速率、后端不支持，
        // 三者的下一步动作完全不同。只报一个 "unavailable" 等于什么都没说。
        None => {
            let mut why: Vec<String> = Vec::new();
            if let Some(e) = &parts.error {
                why.push(format!("error: {e}"));
            }
            if !parts.missing.is_empty() {
                why.push(format!("missing components: {:?}", parts.missing));
            }
            if parts.rate == 0 {
                why.push("device rate unreadable, frames cannot be converted to ms".into());
            }
            if parts.parts.is_empty() && parts.error.is_none() {
                why.push("platform returned no latency components".into());
            }
            if why.is_empty() {
                why.push("backend reports this reading as unavailable".into());
            }
            why.join("; ")
        }
    };
    StageEstimate { id, ms, source: total.source, detail }
}

fn ring_stage(id: &'static str, obs: &RingObservation) -> StageEstimate {
    let detail = match obs.samples_mean() {
        Some(mean) if obs.rate > 0 => format!(
            "mean {mean:.1} samples (max {}) over {} polls @ {} Hz",
            obs.samples_max, obs.polls, obs.rate
        ),
        Some(_) => format!(
            "{} polls observed but the ring rate is unknown, samples cannot be converted to ms",
            obs.polls
        ),
        None => "the copy loop never polled this ring".to_string(),
    };
    StageEstimate { id, ms: obs.ms(), source: obs.source(), detail }
}

/// 两个定性取**更不可信**的那个。与 `devlat::worse` 同序，刻意不共享：
/// 那个是私有的，而把它公开出去等于把一个内部排序变成契约。
fn worse(a: LatSource, b: LatSource) -> LatSource {
    fn rank(s: LatSource) -> u8 {
        match s {
            LatSource::Api => 0,
            LatSource::Assumed => 1,
            LatSource::Unreliable => 2,
            LatSource::Unavailable => 3,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// 把四个分项折成一份估算。**纯函数**，不碰设备。
pub fn estimate(
    capture_dev: &DevLatencyParts,
    capture_ring: &RingObservation,
    play_ring: &RingObservation,
    play_dev: &DevLatencyParts,
) -> LoopbackEstimate {
    let stages = vec![
        device_stage(STAGE_CAPTURE_DEV, capture_dev),
        ring_stage(STAGE_CAPTURE_RING, capture_ring),
        ring_stage(STAGE_PLAY_RING, play_ring),
        device_stage(STAGE_PLAY_DEV, play_dev),
    ];
    let missing: Vec<&'static str> =
        stages.iter().filter(|s| s.ms.is_none()).map(|s| s.id).collect();
    let total_ms = missing
        .is_empty()
        .then(|| stages.iter().filter_map(|s| s.ms).sum::<f64>());
    let source = stages.iter().fold(LatSource::Api, |acc, s| worse(acc, s.source));
    LoopbackEstimate { stages, total_ms, source, missing }
}

/// `probe loopback --json` 里 `est_latency` 那个对象。
pub fn to_json(est: &LoopbackEstimate) -> serde_json::Value {
    let stages: serde_json::Map<String, serde_json::Value> = est
        .stages
        .iter()
        .map(|s| {
            (
                s.id.to_string(),
                serde_json::json!({ "ms": s.ms, "source": s.source, "detail": s.detail }),
            )
        })
        .collect();
    serde_json::json!({
        "total_ms": est.total_ms,
        "source": est.source,
        "calibrated": est.is_calibrated(),
        "missing": est.missing,
        "order": STAGE_ORDER,
        "method": METHOD_NOTE,
        "ring_sampling": RING_SAMPLING_NOTE,
        "parts": stages,
    })
}

/// 人读的一行（非 JSON 模式）。缺项时**不给数字**，只说缺了什么。
pub fn to_line(est: &LoopbackEstimate) -> String {
    let head = match est.total_ms {
        // 前缀恒为 `>=`：见 `is_calibrated` —— 这个和永远是下限，即使四项全是
        // `Api`。定性只决定后面括号里那句，不决定要不要这个前缀。
        Some(ms) => format!("estimated mic->speaker latency >={ms:.2} ms ({:?}, uncalibrated)", est.source),
        None => format!("estimated mic->speaker latency unavailable (missing {:?})", est.missing),
    };
    let parts: Vec<String> = est
        .stages
        .iter()
        .map(|s| match s.ms {
            Some(ms) => format!("{}={ms:.2}ms", s.id),
            None => format!("{}=?", s.id),
        })
        .collect();
    format!("{head}  [{}]", parts.join(" + "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_core::devlat::Transport;

    fn parts(rate: u32, frames: u32, source: LatSource, transport: Transport) -> DevLatencyParts {
        DevLatencyParts {
            parts: vec![("device", frames)],
            missing: Vec::new(),
            rate,
            transport,
            transport_code: 0,
            device: Some("test device".into()),
            base_source: source,
            error: None,
        }
    }

    fn ok_parts(frames: u32) -> DevLatencyParts {
        parts(48_000, frames, LatSource::Api, Transport::BuiltIn)
    }

    fn ring(rate: u32, depths: &[u32]) -> RingObservation {
        let mut o = RingObservation::new(rate);
        for d in depths {
            o.observe(*d);
        }
        o
    }

    fn stage<'a>(est: &'a LoopbackEstimate, id: &str) -> &'a StageEstimate {
        est.stages.iter().find(|s| s.id == id).expect("stage present")
    }

    // ------------------------------------------------------------ the sum

    #[test]
    fn the_total_is_the_sum_of_the_four_parts_and_every_part_is_reported() {
        // 480 frames @48k = 10ms; 240 samples @48k = 5ms.
        let est = estimate(&ok_parts(480), &ring(48_000, &[240]), &ring(48_000, &[480]), &ok_parts(960));
        assert_eq!(est.missing, Vec::<&str>::new());
        let total = est.total_ms.expect("all four parts readable");
        assert!((total - 45.0).abs() < 1e-9, "10 + 5 + 10 + 20 = 45, got {total}");

        // 分项必须**各自**报出来 —— 合并成一个数就答不出「这 45 ms 堵在哪」。
        for (id, want) in [
            (STAGE_CAPTURE_DEV, 10.0),
            (STAGE_CAPTURE_RING, 5.0),
            (STAGE_PLAY_RING, 10.0),
            (STAGE_PLAY_DEV, 20.0),
        ] {
            let got = stage(&est, id).ms.unwrap_or_else(|| panic!("{id} has no ms"));
            assert!((got - want).abs() < 1e-9, "{id}: want {want} ms, got {got}");
        }
        assert_eq!(
            est.stages.iter().map(|s| s.id).collect::<Vec<_>>(),
            STAGE_ORDER.to_vec(),
            "parts are reported in signal-flow order"
        );
    }

    #[test]
    fn the_playback_ring_is_converted_at_the_device_rate_not_48k() {
        // 播放环走设备速率。混用 48000 会在 44.1k 设备上引入 -8.8% 的系统性偏差
        // （audio.rs `dev_rate()` 的原话）。
        let est = estimate(
            &ok_parts(0),
            &ring(48_000, &[0]),
            &ring(44_100, &[441]),
            &parts(44_100, 0, LatSource::Api, Transport::BuiltIn),
        );
        let ms = stage(&est, STAGE_PLAY_RING).ms.expect("readable");
        assert!((ms - 10.0).abs() < 1e-9, "441 samples @44.1k is 10 ms, got {ms}");
    }

    #[test]
    fn a_ring_mean_is_the_mean_of_every_poll_not_of_the_last_one() {
        let o = ring(48_000, &[0, 480, 0, 480]);
        assert_eq!(o.polls, 4);
        assert_eq!(o.samples_max, 480);
        assert!((o.samples_mean().expect("polled") - 240.0).abs() < 1e-9);
        assert!((o.ms().expect("polled") - 5.0).abs() < 1e-9);
    }

    // -------------------------------------------------- all-or-nothing rule

    #[test]
    fn one_unreadable_device_kills_the_total_instead_of_shrinking_it() {
        // 少一项就少一段延迟。把读到的三项加起来上报「看着更有用」，实际是把一个
        // 已知缺口伪装成一个完整读数 —— devlat 的判据 2，同一条规矩。
        let dead = DevLatencyParts::empty("no such device");
        let est = estimate(&ok_parts(480), &ring(48_000, &[240]), &ring(48_000, &[480]), &dead);
        assert_eq!(est.total_ms, None, "a partial sum must not be published as a total");
        assert_eq!(est.missing, vec![STAGE_PLAY_DEV]);
        assert_eq!(est.source, LatSource::Unavailable);
        // 其余三项照报：知道的部分不因为缺了一项而一起消失。
        assert!(stage(&est, STAGE_CAPTURE_DEV).ms.is_some());
        assert!(stage(&est, STAGE_PLAY_RING).ms.is_some());
    }

    #[test]
    fn a_device_that_answers_only_some_components_counts_as_unreadable() {
        let mut p = ok_parts(480);
        p.missing.push("stream");
        let est = estimate(&p, &ring(48_000, &[0]), &ring(48_000, &[0]), &ok_parts(480));
        assert_eq!(est.total_ms, None);
        assert_eq!(est.missing, vec![STAGE_CAPTURE_DEV]);
        assert!(
            stage(&est, STAGE_CAPTURE_DEV).detail.contains("missing components"),
            "the reason must name the missing components: {}",
            stage(&est, STAGE_CAPTURE_DEV).detail
        );
    }

    #[test]
    fn a_ring_that_was_never_polled_is_unknown_not_zero() {
        let est = estimate(
            &ok_parts(480),
            &RingObservation::new(48_000), // 一次都没轮询
            &ring(48_000, &[480]),
            &ok_parts(480),
        );
        assert_eq!(est.total_ms, None, "zero polls is 'unknown', and unknown kills the sum");
        assert_eq!(est.missing, vec![STAGE_CAPTURE_RING]);
    }

    #[test]
    fn a_ring_observed_at_depth_zero_is_a_real_reading() {
        // 与上一条的区别就是这套遥测的全部意义：观测到 0 是真读数，没观测过不是。
        let est = estimate(&ok_parts(480), &ring(48_000, &[0, 0]), &ring(48_000, &[0]), &ok_parts(480));
        assert_eq!(stage(&est, STAGE_CAPTURE_RING).ms, Some(0.0));
        assert!(est.total_ms.is_some());
    }

    #[test]
    fn a_ring_with_no_rate_cannot_be_converted() {
        let est = estimate(&ok_parts(480), &ring(0, &[240]), &ring(48_000, &[0]), &ok_parts(480));
        assert_eq!(est.missing, vec![STAGE_CAPTURE_RING]);
        assert!(stage(&est, STAGE_CAPTURE_RING).detail.contains("rate is unknown"));
    }

    // ------------------------------------------------------- the qualifier

    #[test]
    fn the_worst_part_decides_the_qualifier() {
        // Windows 的 play_dev 是 Unreliable（GetDevicePeriod 实测低报 4.2 倍）。
        // 三项 Api + 一项 Unreliable 的和只能是 Unreliable。
        let est = estimate(
            &ok_parts(480),
            &ring(48_000, &[0]),
            &ring(48_000, &[0]),
            &parts(48_000, 480, LatSource::Unreliable, Transport::BuiltIn),
        );
        assert_eq!(est.source, LatSource::Unreliable);
        assert!(est.total_ms.is_some(), "unreliable is still a number, just a worse one");
    }

    #[test]
    fn a_bluetooth_output_downgrades_the_whole_estimate() {
        // 传输方式的降级由 DevLatencyParts::total() 叠加，这里只确认它没被绕过。
        let est = estimate(
            &ok_parts(480),
            &ring(48_000, &[0]),
            &ring(48_000, &[0]),
            &parts(48_000, 960, LatSource::Api, Transport::Bluetooth),
        );
        assert_eq!(est.source, LatSource::Unreliable, "A2DP under-reports by an order of magnitude");
    }

    #[test]
    fn four_perfect_api_readings_are_still_not_a_calibrated_measurement() {
        // 这条测试守的是 plan §7.6 第 6 条：不许拿不完整的量冒充端到端物理量。
        // 谁把 is_calibrated 改成「四项全 Api 就算标定」，这里就红。
        let est = estimate(&ok_parts(480), &ring(48_000, &[0]), &ring(48_000, &[0]), &ok_parts(480));
        assert_eq!(est.source, LatSource::Api);
        assert!(!est.is_calibrated(), "nothing here timed a signal end to end");
        assert!(to_line(&est).contains(">="), "an uncalibrated total is a floor, and must read as one");
        assert!(to_line(&est).contains("uncalibrated"));
    }

    // --------------------------------------------------------------- shape

    #[test]
    fn the_json_says_what_the_number_is_and_is_not() {
        let est = estimate(&ok_parts(480), &ring(48_000, &[240]), &ring(48_000, &[480]), &ok_parts(960));
        let v = to_json(&est);
        assert_eq!(v["calibrated"], serde_json::json!(false));
        assert_eq!(v["source"], serde_json::json!("api"));
        // 口径必须自陈：读日志的人不该需要先读源码才知道这不是实测。
        let method = v["method"].as_str().expect("method note");
        assert!(method.contains("ESTIMATE, NOT A MEASUREMENT"));
        assert!(method.contains("No loopback calibration"));
        // 环驻留是瞬时量 —— 采样口径与数字必须同行。
        let sampling = v["ring_sampling"].as_str().expect("sampling note");
        assert!(sampling.contains("mean"), "the sampling rule must be stated: {sampling}");
        // `parts` is a JSON OBJECT, and JSON objects are unordered — without
        // this the reader cannot tell which stage comes first, which is the
        // first question anyone debugging a latency figure asks.
        assert_eq!(v["order"], serde_json::json!(STAGE_ORDER));
        for id in STAGE_ORDER {
            assert!(v["parts"][id]["ms"].is_number(), "part {id} missing from JSON: {v}");
            assert!(v["parts"][id]["source"].is_string());
            assert!(v["parts"][id]["detail"].is_string());
        }
    }

    #[test]
    fn the_json_reports_a_missing_total_as_null_not_as_zero() {
        let est = estimate(
            &DevLatencyParts::empty("no device"),
            &ring(48_000, &[0]),
            &ring(48_000, &[0]),
            &ok_parts(480),
        );
        let v = to_json(&est);
        assert!(v["total_ms"].is_null(), "0 would read as 'no latency': {v}");
        assert_eq!(v["missing"], serde_json::json!([STAGE_CAPTURE_DEV]));
        assert!(v["parts"][STAGE_CAPTURE_DEV]["ms"].is_null());
    }
}
