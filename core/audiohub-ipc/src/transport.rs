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
//! - **质量档**是**手段**：它直接指定阶梯上的一格 —— 一个 **(采样率, 线上位深)
//!   二元组** —— 发送侧照做。选中什么就是什么，没有伺服，也不需要。
//!   （位深进阶梯之前这里只有采样率，而「线上恒为 16 位」这件事没有任何一处
//!   代码说得出来。）
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

// NOTE (deleted compatibility layer): `QUALITY_LEGACY_PCM` and
// `QUALITY_LEGACY_IDS` used to translate the pre-bit-depth spellings
// (`pcm`, `pcm48k`, `pcm32k`, `pcm24k`, `pcm16k`) onto the new rungs. The
// project has no released users, and the layer manufactured a real regression
// of its own: the frontend had to mirror the same table, one of the three read
// paths (`PeerTransport.transportCells`) forgot to apply it, and the same
// stored value rendered as `pcm32k` on one page and "PCM 32 kHz - 16 bit" on
// another with nothing to flag the divergence. The identifiers are now the six
// ladder ids plus `auto`, and an unrecognised string is **rejected** by
// `QualityTarget::parse` so the caller can reset to the default and say so —
// see `PeerTransport::sanitize`. Silent translation is what allowed a stored
// value to keep meaning something the UI never showed.

/// 一格的线上 id：`pcm<kHz>k<位深>`，位深 32 浮点写 `32f`。
///
/// 形如 `pcm48k24` / `pcm48k32f`，可正则解析（`^pcm(\d+)k(\d+)(f?)$`）。
/// **不写成 `pcm48kf32`**：那样「先采样率后位深」的顺序会在浮点档上被打断。
pub fn quality_stop_id(f: audiohub_net::media::WireFormat) -> String {
    use audiohub_core::dsp::WireDepth;
    let tag = match f.depth {
        WireDepth::S16 => "16",
        WireDepth::S24 => "24",
        WireDepth::F32 => "32f",
    };
    format!("pcm{}k{}", f.rate_hz / 1000, tag)
}

/// 一个质量档，连同**它是否真的存在**。
///
/// `available == false` 的档照样出现在契约里：滑条要把它画成灰色刻度，
/// 用户才看得见阶梯的完整形状（plan §5 承诺过 Opus 三档）。
/// 但 [`QualityTarget::parse`] 拒绝它——**看得见 ≠ 选得中**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityStop {
    /// 线上 id，同时是前端查文案的键。
    pub id: String,
    /// **音频**码率（kbps）= `采样率 × 位深`，单声道。`None` = AUTO。
    ///
    /// ⚠ 不含协议开销。深档按 5 ms 分包，每 10 ms 付两份包头 + 两份 AEAD 标签，
    /// 实测带宽会高于这个数——那不是 bug。
    pub kbps: Option<u32>,
    /// 采样率（Hz）。`None` = AUTO 或非 PCM 档。
    pub rate: Option<u32>,
    /// 线上位深：`"s16" | "s24" | "f32"`。`None` = AUTO 或非 PCM 档。
    ///
    /// **前端不许从 `id` 里解析它**。id 的拼法是给人看的，位深是数据；
    /// 让前端解析 id 就是在前端复刻一份格式表，两处一漂没有任何地方会报错。
    /// **刻意不报数字 `32`**：`32` 在整数与浮点之间是歧义的。
    #[serde(default)]
    pub depth: Option<String>,
    /// 本 build 能不能真的用这一档。
    pub available: bool,
    /// 不可用的原因**标识符**（不是文案——文案在前端 i18n）。
    /// 目前只有 `"opus"`：`Cargo.toml` 没有 libopus，`Codec::Opus` 是死枚举。
    pub blocked_by: Option<String>,
}

/// 阶梯的**唯一**真值。前端从 `settings.get` 拿它，不自己写一份。
///
/// # 排序：按**音频码率**升序，AUTO 在最左
///
/// 用户要的是「AUTO … 常见档 … PCM」。位深进阶梯之后，这条滑条上的一格是一个
/// **(采样率, 位深) 二元组**，而不再只是一个采样率——两个维度合并成一条按码率
/// 排序的阶梯（用户裁定：「只是会因此多出更多档位而已」）。
///
/// 表本身是 `audiohub_net::media::LADDER` 的**投影**，不是第二份手写表：
/// 那里的注释写了排序准则（先把采样率买满 48 kHz，再买位深）与它的依据。
/// `LADDER` 是 rung 0 最好，这里要滑条从左到右越来越好，所以**倒着投影**。
///
/// Opus 三档来自 plan §5，**一行编解码器都没有**（`docs/audit-open-items.md`
/// 第 67 行：`Cargo.toml` 无 opus 依赖）。它们在表里但 `available = false`，
/// 位置按各自码率排在 PCM 档之下——有损压缩在同码率下承载的信息不多于原始 PCM，
/// 这是可辩护的排序依据，而「Opus 256k 听起来比 16 kHz PCM 好」是个我拿不出
/// 测量的主观断言。
pub fn quality_stops() -> Vec<QualityStop> {
    fn opus(id: &str, kbps: u32) -> QualityStop {
        QualityStop {
            id: id.to_string(),
            kbps: Some(kbps),
            rate: None,
            depth: None,
            available: false,
            blocked_by: Some("opus".to_string()),
        }
    }
    let mut out = vec![
        QualityStop {
            id: QUALITY_AUTO.to_string(),
            kbps: None,
            rate: None,
            depth: None,
            available: true,
            blocked_by: None,
        },
        opus("opus64", 64),
        opus("opus128", 128),
        opus("opus256", 256),
    ];
    // `LADDER` 是 rung 0 最好 ⇒ 倒序投影出「从左到右越来越好」。
    for f in audiohub_net::media::LADDER.iter().rev() {
        out.push(QualityStop {
            id: quality_stop_id(*f),
            kbps: Some(f.kbps()),
            rate: Some(f.rate_hz),
            depth: Some(f.depth.as_str().to_string()),
            available: true,
            blocked_by: None,
        });
    }
    out
}

/// 用户选的质量档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityTarget {
    /// plan §5：按丢包/抖动在阶梯上升降（既有的 `AutoLadder`）。
    Auto,
    /// 钉死在 `LADDER` 的这一格上，AUTO 阶梯停摆。
    ///
    /// # 为什么是**格号**而不是 `(采样率, 位深)` 两个值
    ///
    /// 音频线程每拍读它。两个独立的值意味着两次读，中间会**撕裂**：
    /// 瞬间读出一个阶梯上根本不存在的组合（新的 48 kHz 配上还没更新的 16 bit）。
    /// 存一个格号从根上消灭这个态。
    Fixed(u32),
}

impl QualityTarget {
    pub fn as_wire(self) -> String {
        match self {
            QualityTarget::Auto => QUALITY_AUTO.to_string(),
            QualityTarget::Fixed(r) => quality_stop_id(audiohub_net::media::rung_format(r)),
        }
    }

    /// 这一档的线上格式。`None` = AUTO（格式随链路变）。
    pub fn format(self) -> Option<audiohub_net::media::WireFormat> {
        match self {
            QualityTarget::Auto => None,
            QualityTarget::Fixed(r) => Some(audiohub_net::media::rung_format(r)),
        }
    }

    /// `None` = 本 build 给不了这一档。**Opus 三档走的就是这条**——
    /// 它们在 [`quality_stops`] 里可见、可解释，但选不中。
    /// 一个能被 `settings.set` 收下的 `"opus128"` 会让用户以为改成了 Opus，
    /// 而线上照旧是 PCM——正是「一切都报成功，什么都没发生」。
    ///
    /// 认不出来的串一律 `None`，**不翻译、不就近吸附**。盘上留着的旧拼写
    /// （`pcm48k` 那一族）现在走这条：调用方据此重置到默认并让 UI 说明，
    /// 而不是静默落到某一档——静默翻译正是被删掉的那层兼容代码干的事。
    pub fn parse(s: &str) -> Option<QualityTarget> {
        if s == QUALITY_AUTO {
            return Some(QualityTarget::Auto);
        }
        let stop = quality_stops().into_iter().find(|q| q.id == s)?;
        if !stop.available {
            return None;
        }
        let (rate, depth) = (stop.rate?, audiohub_core::dsp::WireDepth::parse(&stop.depth?)?);
        audiohub_net::media::rung_of(rate, depth).map(QualityTarget::Fixed)
    }

    pub fn slider_index(self) -> usize {
        let id = self.as_wire();
        quality_stops().iter().position(|q| q.id == id).unwrap_or(0)
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

    /// **本 build 能给的质量档，恰好是 `LADDER` 那六个 (采样率, 位深)。**
    ///
    /// 这条把契约钉在能力上而不是钉在一张手写表上：有人往 `quality_stops()`
    /// 里加一档 `pcm96k16`，而 `LADDER` 里没有它，这条就红。
    /// 没有它，多出来的那一档会一路走到 `settings.set` 被收下、被写盘、
    /// 然后 `rung_of` 找不到它 —— 而那一步是**静默**回落。
    ///
    /// ⚠ 位深进阶梯之后**只比采样率是不够的**：48000 在阶梯上出现三次，
    /// 只比采样率的版本会把「多了一档 48 kHz/20 bit」放过去。
    #[test]
    fn the_available_stops_are_exactly_the_formats_the_media_plane_has() {
        use audiohub_net::media::LADDER;
        let mut have: Vec<(u32, String)> = quality_stops()
            .iter()
            .filter(|q| q.available)
            .filter_map(|q| Some((q.rate?, q.depth.clone()?)))
            .collect();
        have.sort();
        let mut want: Vec<(u32, String)> =
            LADDER.iter().map(|f| (f.rate_hz, f.depth.as_str().to_string())).collect();
        want.sort();
        assert_eq!(
            have, want,
            "可选质量档与媒体面的格式阶梯对不上；多出来的那档会被静默忽略"
        );
        // 每一档都必须同时报出采样率**与**位深——少一个，前端就得去解析 id。
        for q in quality_stops().iter().filter(|q| q.available && q.id != QUALITY_AUTO) {
            assert!(q.rate.is_some(), "{} 没报采样率", q.id);
            assert!(q.depth.is_some(), "{} 没报位深", q.id);
            assert!(q.kbps.is_some(), "{} 没报码率", q.id);
        }
    }

    /// **每个 id 都把两个维度写全**，且**位深绝不写成裸的 `32`**。
    ///
    /// 注入对照：把 `quality_stop_id` 里 `F32 => "32f"` 改成 `"32"`，这条变红。
    /// 那次改动在界面上的表现是 `pcm48k32`——与「32 位整数」无法区分，
    /// 而两端根本没有 32 位整数这一档。
    #[test]
    fn every_pcm_stop_id_spells_out_both_dimensions() {
        let re_ok = |id: &str| {
            let rest = id.strip_prefix("pcm")?;
            let (khz, depth) = rest.split_once('k')?;
            khz.parse::<u32>().ok()?;
            matches!(depth, "16" | "24" | "32f").then_some(())
        };
        for q in quality_stops().iter().filter(|q| q.rate.is_some()) {
            assert!(
                re_ok(&q.id).is_some(),
                "{} 不符合 `pcm<kHz>k<16|24|32f>`：两个维度必须都写在 id 里",
                q.id
            );
            assert_ne!(q.id, "pcm48k32", "裸的 32 与 32 位整数无法区分");
        }
    }

    /// The pre-bit-depth spellings are gone from the table **and** rejected by
    /// `parse` — no silent translation survives anywhere.
    ///
    /// Injection check: restore the old
    /// `if s == "pcm48k" { s = "pcm48k16" }` translation in `parse` and this
    /// goes red on "was silently translated". That translation is what let a
    /// stored `pcm32k` execute as one rung while a read-only overview drew the
    /// raw string, with nothing able to notice.
    #[test]
    fn the_pre_bit_depth_quality_ids_are_rejected_not_translated() {
        for old in ["pcm", "pcm48k", "pcm32k", "pcm24k", "pcm16k"] {
            assert!(
                !quality_stops().iter().any(|q| q.id == old),
                "stale id {old} is still in the stop table: the ambiguity moved into the code"
            );
            assert_eq!(
                QualityTarget::parse(old),
                None,
                "{old} was silently translated; an unknown stop must be refused so the \
                 caller can reset to the default and say so in the UI"
            );
        }
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
            "pcm48k32f",
            "最右必须是阶梯顶端（满速率 + 最深位深）"
        );
    }

    /// 滑条位置：AUTO 在 0，其余各占一格且**互不重叠**；
    /// 每一档的 `slider_index` 与它在表里的位置一致。
    ///
    /// 这条挡的是「`as_wire()` 拼出来的 id 与 `quality_stops()` 里的 id 不一致」
    /// ——那种情况下 `position()` 找不到，`unwrap_or(0)` 会把**所有**固定档都
    /// 画在 AUTO 那一格上，而没有任何一处会报错。
    #[test]
    fn quality_slider_indices_match_the_table_positions() {
        let stops = quality_stops();
        assert_eq!(QualityTarget::Auto.slider_index(), 0, "AUTO 必须在最左");
        let mut seen = vec![0usize];
        for (i, q) in stops.iter().enumerate() {
            let Some(t) = QualityTarget::parse(&q.id) else { continue };
            assert_eq!(t.slider_index(), i, "{} 的滑条位置与它在表里的位置对不上", q.id);
            if i > 0 {
                seen.push(i);
            }
        }
        // 六个 PCM 档 + AUTO 各占一格，没有两个撞在一起。
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "两个质量档撞到了同一个滑条位置");
        assert_eq!(n, 1 + audiohub_net::media::LADDER.len(), "可选档数与阶梯长度对不上");
    }
}
