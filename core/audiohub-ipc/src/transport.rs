//! 传输档位：延迟目标与质量档的**档位表**、语义、与「谁真的读它」。
//!
//! # 这个模块存在的理由
//!
//! `DaemonSettings.latency` / `.quality` 在本模块出现之前是两个 `String`：
//! `settings.set` 收下、写盘、原样回显，**然后没有任何一行代码读它们**。
//! 不是「改了要重启才生效」——重启也不生效，媒体面根本不认识这两个字段。
//! `spec-ui.md` 第 101 行与 `Settings.tsx` 的「已保存 · 暂未生效」角标是对这件事
//! 的诚实披露，本模块是把披露换成实现。
//!
//! # 两个档位的语义**不对称**，这是关键
//!
//! - **质量档**是**手段**：它直接指定阶梯上的一格（采样率），发送侧照做。
//!   选中什么就是什么，没有伺服，也不需要。
//! - **延迟档是目标，不是手段。** 用户裁定的原话是「区间内固定延迟需要考虑
//!   实际与对方连接的延迟包含进去等于这个数」——即它是**端到端总延迟**
//!   （`PipelineLatency.sum_ms`，含 `net_ms` 与对端分项）的目标值，
//!   **不是某一级缓冲的大小**。选 200 就该稳定在 ~200 ms。
//!
//!   于是延迟档必须走闭环：被控量是 `sum_ms`，操纵量是 jitter buffer 深度，
//!   其余各级（采集环、声卡、网络、播放环）是我们动不了的**地板**。
//!   伺服在 `audiohubd::servo`。
//!
//! # 「达不到」必须说出来，不许假装达到
//!
//! 目标低于物理下限时（地板 90 ms 而用户选 0），系统停在地板上并**如实上报
//! 实际达到的值**加一个 `at_floor` 标记，UI 显示真实数字 + 「已达物理下限」。
//! 把目标值回显成「当前值」是本项目反复栽的那个形态。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 延迟

/// AUTO 的线上拼写。固定档拼作十进制毫秒（`"0"`、`"200"`、`"1000"`）。
pub const LATENCY_AUTO: &str = "auto";

/// 本模块出现之前 `settings.json` 里那个值。语义是「固定最小缓冲」，
/// 与新档位的 `0`（「尽你所能地低」）**是同一件事**，所以这是一次
/// 忠实翻译而不是猜测——因此允许静默迁移。
pub const LATENCY_LEGACY_MIN: &str = "min";

/// 滑条的固定档，单位毫秒，**升序**。
///
/// # 取值理由（逐档，不是凑的）
///
/// | 档 | 依据 |
/// |---|---|
/// | `0` | 「尽你所能地低」。不是承诺 0 ms，是「不要为了平滑多留任何一帧」。实际落点由物理地板决定并如实上报。 |
/// | `10` / `20` / `30` | 一 / 二 / 三个帧时（`FRAME_MS = 10`）。**30 ms 是 AES67 强制的接收缓冲下限**（3 × packet time），`JbTuning` 的文档里已经按这条推过一次。 |
/// | `50` | 当前默认整定实测的 JB 深度（`min_target = 4`）。选它 = 「就按现在这样」。 |
/// | `75` / `100` | ITU-T G.114：单向 < 150 ms 无可察觉损伤。100 是这条带里的整数锚点，75 补中间一步。 |
/// | `150` | G.114 的那条线本身。 |
/// | `200` / `300` | G.114「150–400 ms 可接受」区间。300 往上交互性明显退化。 |
/// | `500` | 已经不适合对话，留给单向监听与劣质链路。 |
/// | `750` / `1000` | 用户点名的上限。极差链路 / 故意深缓冲的单向场景。 |
///
/// 相邻档比值 1.3–2×：低端细（那里 10 ms 是可感知的一大步），高端粗
/// （那里 50 ms 谁也听不出差别）。等差会在高端浪费一半档位，等比会在低端
/// 给不出 10 ms 这种必须存在的整数档。
pub const LATENCY_STOPS_MS: [u16; 13] =
    [0, 10, 20, 30, 50, 75, 100, 150, 200, 300, 500, 750, 1000];

/// 用户对**端到端总延迟**的要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyTarget {
    /// plan §5：按网络质量自适应（沿用既有的抖动驱动 `update_target`）。
    Auto,
    /// 端到端总延迟的目标毫秒数。`0` = 尽可能低。
    ///
    /// **不是缓冲深度。** 伺服要把 `sum_ms` 拉到这个数，网络那一段算在里面。
    TotalMs(u16),
}

impl LatencyTarget {
    /// 线上拼写。固定档就是十进制毫秒，没有单位后缀——加后缀就得有人去解析它，
    /// 而这个值同时要经过 JSON、`settings.json` 与前端三道手。
    pub fn as_wire(self) -> String {
        match self {
            LatencyTarget::Auto => LATENCY_AUTO.to_string(),
            LatencyTarget::TotalMs(ms) => ms.to_string(),
        }
    }

    /// `None` = 这个字符串不是本 build 认识的档。调用方决定怎么办；
    /// 这里**不猜**，因为「猜错的那一档」和「用户选的那一档」在延迟上可以差
    /// 一个数量级，而错了不会有任何声音上的报错。
    ///
    /// 只接受档位表里的精确取值：`"137"` 是拒绝，不是「就近取 150」。
    /// 允许任意毫秒数会让 UI 与 daemon 对「有哪些档」产生分歧，
    /// 而滑条的全部意义就是这个集合是有限且双方一致的。
    pub fn parse(s: &str) -> Option<LatencyTarget> {
        if s == LATENCY_AUTO {
            return Some(LatencyTarget::Auto);
        }
        // 旧文件里的 "min"。语义等价于 0，翻译是忠实的。
        if s == LATENCY_LEGACY_MIN {
            return Some(LatencyTarget::TotalMs(0));
        }
        let ms: u16 = s.parse().ok()?;
        LATENCY_STOPS_MS
            .contains(&ms)
            .then_some(LatencyTarget::TotalMs(ms))
    }

    /// 滑条位置（`0` = AUTO，其后依 [`LATENCY_STOPS_MS`] 顺序）。
    pub fn slider_index(self) -> usize {
        match self {
            LatencyTarget::Auto => 0,
            LatencyTarget::TotalMs(ms) => LATENCY_STOPS_MS
                .iter()
                .position(|&m| m == ms)
                .map(|i| i + 1)
                .unwrap_or(0),
        }
    }
}

// ---------------------------------------------------------------- 质量

pub const QUALITY_AUTO: &str = "auto";

/// 本模块出现之前 `settings.json` 里的「PCM」。等价于满速率那一档。
pub const QUALITY_LEGACY_PCM: &str = "pcm";

/// 一个质量档，连同**它是否真的存在**。
///
/// `available == false` 的档照样出现在契约里：滑条要把它画成灰色刻度，
/// 用户才看得见阶梯的完整形状（plan §5 承诺过 Opus 三档）。
/// 但 [`QualityTarget::parse`] 拒绝它——**看得见 ≠ 选得中**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityStop {
    /// 线上 id，同时是前端查文案的键。
    pub id: String,
    /// 线速率（kbps）。`None` = AUTO（速率随链路变）。
    pub kbps: Option<u32>,
    /// 采样率（Hz）。`None` = AUTO 或非 PCM 档。
    pub rate: Option<u32>,
    /// 本 build 能不能真的用这一档。
    pub available: bool,
    /// 不可用的原因**标识符**（不是文案——文案在前端 i18n）。
    /// 目前只有 `"opus"`：`Cargo.toml` 没有 libopus，`Codec::Opus` 是死枚举。
    pub blocked_by: Option<String>,
}

/// 阶梯的**唯一**真值。前端从 `settings.get` 拿它，不自己写一份。
///
/// # 排序：按线速率升序，AUTO 在最左
///
/// 用户要的是「AUTO … 常见档 … PCM」。这里的「常见档」取的是**音频带宽**那条
/// 众所周知的阶梯（宽带 / 超宽带 / 准全带 / 全带），因为本 build 真正能调的
/// 就是采样率——`AUTO_RATES = [48000, 32000, 24000, 16000]`，s16 单声道。
///
/// Opus 三档来自 plan §5，**一行编解码器都没有**（`docs/audit-open-items.md`
/// 第 67 行：`Cargo.toml` 无 opus 依赖，发送恒为 `Codec::PcmS16le`）。
/// 它们在表里但 `available = false`，位置按各自码率排在 PCM 档之下——
/// 有损压缩在同码率下承载的信息不多于原始 PCM，这是可辩护的排序依据，
/// 而「Opus 256k 听起来比 16 kHz PCM 好」是个我拿不出测量的主观断言。
pub fn quality_stops() -> Vec<QualityStop> {
    fn pcm(id: &str, rate: u32) -> QualityStop {
        QualityStop {
            id: id.to_string(),
            // s16 单声道：rate × 16 bit。这是**线上真实字节数**，
            // 不是标称值——载荷就是 `dsp::f32_to_s16le` 的输出。
            kbps: Some(rate * 16 / 1000),
            rate: Some(rate),
            available: true,
            blocked_by: None,
        }
    }
    fn opus(id: &str, kbps: u32) -> QualityStop {
        QualityStop {
            id: id.to_string(),
            kbps: Some(kbps),
            rate: None,
            available: false,
            blocked_by: Some("opus".to_string()),
        }
    }
    vec![
        QualityStop {
            id: QUALITY_AUTO.to_string(),
            kbps: None,
            rate: None,
            available: true,
            blocked_by: None,
        },
        opus("opus64", 64),
        opus("opus128", 128),
        opus("opus256", 256),
        pcm("pcm16k", 16_000),
        pcm("pcm24k", 24_000),
        pcm("pcm32k", 32_000),
        pcm("pcm48k", 48_000),
    ]
}

/// 用户选的质量档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityTarget {
    /// plan §5：按丢包/抖动在阶梯上升降（既有的 `AutoLadder`）。
    Auto,
    /// 钉死在这个采样率上，AUTO 阶梯停摆。
    Rate(u32),
}

impl QualityTarget {
    pub fn as_wire(self) -> String {
        match self {
            QualityTarget::Auto => QUALITY_AUTO.to_string(),
            QualityTarget::Rate(r) => format!("pcm{}k", r / 1000),
        }
    }

    /// `None` = 本 build 给不了这一档。**Opus 三档走的就是这条**——
    /// 它们在 [`quality_stops`] 里可见、可解释，但选不中。
    /// 一个能被 `settings.set` 收下的 `"opus128"` 会让用户以为改成了 Opus，
    /// 而线上照旧是 PCM——正是「一切都报成功，什么都没发生」。
    pub fn parse(s: &str) -> Option<QualityTarget> {
        if s == QUALITY_AUTO {
            return Some(QualityTarget::Auto);
        }
        if s == QUALITY_LEGACY_PCM {
            return Some(QualityTarget::Rate(48_000));
        }
        let stop = quality_stops().into_iter().find(|q| q.id == s)?;
        if !stop.available {
            return None;
        }
        stop.rate.map(QualityTarget::Rate)
    }

    pub fn slider_index(self) -> usize {
        let id = self.as_wire();
        quality_stops()
            .iter()
            .position(|q| q.id == id)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 档位表本身的形状：升序、无重复、两端就是用户点名的那两个数。
    #[test]
    fn the_latency_stops_are_an_ascending_ladder_from_zero_to_one_second() {
        assert_eq!(*LATENCY_STOPS_MS.first().unwrap(), 0, "最左固定档必须是 0（最低）");
        assert_eq!(*LATENCY_STOPS_MS.last().unwrap(), 1000, "上限由用户点名");
        for w in LATENCY_STOPS_MS.windows(2) {
            assert!(w[0] < w[1], "档位必须严格升序：{} !< {}", w[0], w[1]);
        }
    }

    /// 每一档都能原样转一圈回来。滑条的全部前提是两端对「有哪些档」一致。
    #[test]
    fn every_latency_stop_round_trips_through_the_wire_spelling() {
        assert_eq!(LatencyTarget::parse(LATENCY_AUTO), Some(LatencyTarget::Auto));
        assert_eq!(LatencyTarget::Auto.as_wire(), LATENCY_AUTO);
        for &ms in &LATENCY_STOPS_MS {
            let t = LatencyTarget::TotalMs(ms);
            assert_eq!(LatencyTarget::parse(&t.as_wire()), Some(t), "{ms} ms 没转回来");
        }
    }

    /// 档位表以外的毫秒数**被拒绝**，不是被就近吸附。
    ///
    /// 吸附会让 daemon 悄悄执行一个用户没选过的档，而 UI 显示的是它请求的那个——
    /// 两端各说各话，且没有任何一处会报错。
    #[test]
    fn a_millisecond_value_that_is_not_a_stop_is_refused_not_snapped() {
        for bad in ["137", "1", "999", "1001", "-1", "", "abc", "200ms", "AUTO"] {
            assert_eq!(LatencyTarget::parse(bad), None, "`{bad}` 不该被接受");
        }
    }

    /// 旧文件里的 `"min"` 落到 0 —— 语义相同，是翻译不是猜测。
    #[test]
    fn the_legacy_min_spelling_lands_on_zero() {
        assert_eq!(
            LatencyTarget::parse(LATENCY_LEGACY_MIN),
            Some(LatencyTarget::TotalMs(0)),
            "旧的「最低」档与新的 0 档是同一件事"
        );
    }

    /// 滑条位置：AUTO 在 0，固定档依次在其后，且**互不重叠**。
    #[test]
    fn slider_indices_are_a_bijection_onto_the_stops() {
        let mut seen = vec![LatencyTarget::Auto.slider_index()];
        for &ms in &LATENCY_STOPS_MS {
            seen.push(LatencyTarget::TotalMs(ms).slider_index());
        }
        assert_eq!(seen[0], 0, "AUTO 必须在最左");
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "两个档撞到了同一个滑条位置");
        assert_eq!(sorted, (0..=LATENCY_STOPS_MS.len()).collect::<Vec<_>>());
    }

    /// **本 build 能给的质量档，恰好是 `AUTO_RATES` 那四个采样率。**
    ///
    /// 这条把契约钉在能力上而不是钉在一张手写表上：有人往 `quality_stops()`
    /// 里加一档 `pcm96k`，而 `AUTO_RATES` 里没有 96000，这条就红。
    /// 没有它，多出来的那一档会一路走到 `settings.set` 被收下、被写盘、
    /// 然后 `rung_of_rate` 找不到它 —— 而那一步是**静默**回落。
    #[test]
    fn the_available_stops_are_exactly_the_rates_the_media_plane_has() {
        use audiohub_net::media::AUTO_RATES;
        let mut have: Vec<u32> = quality_stops()
            .iter()
            .filter(|q| q.available)
            .filter_map(|q| q.rate)
            .collect();
        have.sort_unstable();
        let mut want = AUTO_RATES.to_vec();
        want.sort_unstable();
        assert_eq!(
            have, want,
            "可选质量档与媒体面的采样率阶梯对不上；多出来的那档会被静默忽略"
        );
    }

    /// **Opus 三档看得见但选不中。**
    ///
    /// 这是本次改动里最容易退化的一条：随手给 `parse` 加一句
    /// 「找到 id 就返回」，滑条立刻能选中 Opus，`settings.set` 收下，
    /// 界面显示「Opus 128k」，而线上一个字节都没变——正是本项目栽过五次的形态。
    #[test]
    fn the_opus_rungs_are_visible_but_unselectable() {
        let opus: Vec<QualityStop> = quality_stops()
            .into_iter()
            .filter(|q| q.blocked_by.as_deref() == Some("opus"))
            .collect();
        assert_eq!(opus.len(), 3, "plan §5 承诺的是 256k/128k/64k 三档");
        for q in &opus {
            assert!(!q.available, "{} 标了可用，但 Cargo.toml 里没有 libopus", q.id);
            assert_eq!(
                QualityTarget::parse(&q.id),
                None,
                "{} 被 parse 接受了：用户会以为切到了 Opus，而线上仍是 PCM",
                q.id
            );
            assert!(q.rate.is_none(), "Opus 档不是采样率档，不该带 rate");
        }
    }

    #[test]
    fn every_available_quality_stop_round_trips() {
        assert_eq!(QualityTarget::parse(QUALITY_AUTO), Some(QualityTarget::Auto));
        assert_eq!(QualityTarget::Auto.as_wire(), QUALITY_AUTO);
        for q in quality_stops().iter().filter(|q| q.available) {
            let Some(t) = QualityTarget::parse(&q.id) else { continue };
            assert_eq!(t.as_wire(), q.id, "{} 的 as_wire 与 id 不一致", q.id);
            assert_eq!(QualityTarget::parse(&t.as_wire()), Some(t));
        }
        assert_eq!(
            QualityTarget::parse(QUALITY_LEGACY_PCM),
            Some(QualityTarget::Rate(48_000)),
            "旧的 PCM 档就是满速率那一档"
        );
    }

    /// id 唯一、码率单调不减 —— 滑条从左到右必须越来越好，否则「往右拖 = 提质量」
    /// 这个唯一的操作直觉就是错的。
    #[test]
    fn the_quality_ladder_is_monotone_and_has_unique_ids() {
        let stops = quality_stops();
        let mut ids: Vec<&str> = stops.iter().map(|q| q.id.as_str()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "质量档 id 有重复");
        assert_eq!(stops[0].id, QUALITY_AUTO, "AUTO 必须在最左");
        let rates: Vec<u32> = stops[1..].iter().filter_map(|q| q.kbps).collect();
        assert_eq!(rates.len(), stops.len() - 1, "除 AUTO 外每档都要有码率");
        for w in rates.windows(2) {
            assert!(w[0] <= w[1], "码率不单调：{} > {}", w[0], w[1]);
        }
        assert_eq!(
            stops.last().unwrap().id,
            "pcm48k",
            "最右必须是满速率 PCM（用户点名 AUTO … PCM）"
        );
    }
}
