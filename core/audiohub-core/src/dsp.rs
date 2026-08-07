pub fn gen_sine(freq_hz: f32, sample_rate: u32, num_samples: usize, amp: f32) -> Vec<f32> {
    // f64 phase: f32 accumulation audibly degrades SNR on multi-second tones
    let step = 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64;
    (0..num_samples)
        .map(|n| (amp as f64 * (step * n as f64).sin()) as f32)
        .collect()
}

pub fn goertzel_power(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len() as f32;
    let omega = 2.0 * std::f64::consts::PI * (freq_hz as f64) / (sample_rate as f64);
    let coeff = 2.0 * omega.cos();
    let (mut s_prev, mut s_prev2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = x as f64 + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let power = s_prev * s_prev + s_prev2 * s_prev2 - coeff * s_prev * s_prev2;
    // Normalize so pure tone of amp A yields ~A^2/4 regardless of window length.
    (power / (n as f64 * n as f64)) as f32
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToneVerdict {
    pub freq_hz: f32,
    pub snr_db: f32,
    pub detected: bool,
    pub samples_analyzed: usize,
}

pub fn verify_tone(samples: &[f32], sample_rate: u32, freq_hz: f32) -> ToneVerdict {
    let sr = sample_rate as usize;
    let skip = sr / 5; // 200ms
    let win = sr / 10; // 100ms
    if samples.len() < sr * 3 / 10 || win == 0 {
        return ToneVerdict {
            freq_hz,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    let usable = &samples[skip..];
    let mut snrs: Vec<f32> = Vec::new();
    let mut analyzed = 0usize;
    for chunk in usable.chunks(win) {
        if chunk.len() < win {
            break;
        }
        analyzed += chunk.len();
        let target = goertzel_power(chunk, sample_rate, freq_hz) as f64;
        // Total normalized power on same scale as goertzel_power: mean(x^2)/2.
        let total: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
            / (chunk.len() as f64)
            / 2.0;
        let eps = 1e-12f64;
        let noise = (total - target).max(0.0) + eps;
        snrs.push((10.0 * (target.max(eps) / noise).log10()) as f32);
    }
    if snrs.is_empty() {
        return ToneVerdict {
            freq_hz,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    snrs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let snr_db = snrs[snrs.len() / 2];
    ToneVerdict {
        freq_hz,
        snr_db,
        detected: snr_db > 20.0,
        samples_analyzed: analyzed,
    }
}

// ---------------------------------------------------------------- 线上位深
//
// # 为什么位深是一个**显式参数**，而不是一个默认值
//
// 这个模块此前只有 `f32_to_s16le` / `s16le_to_f32` 一对函数，位深写死 16 位，
// 而**没有任何一处调用点说得出这件事**——`engine.rs` 的 tx_loop 直接调它，
// 阶梯只管采样率，包头里的 `codec` 恒为 `PcmS16le`。于是「线上是 16 位」这条
// 事实只活在函数名里，`transport.rs:151` 那句「s16 单声道」的注释是它唯一的
// 对外声明。
//
// 位深进阶梯之后，**留下任何一条不带 `WireDepth` 参数的转换入口都是错的**：
// 那条入口会在某次改动里被顺手复用，然后静默地把一个 24 位档的帧编成 16 位，
// 而线上、遥测、UI 全都照旧显示 24 —— 本项目栽过五次的那个形态。
// 所以这里没有「默认深度」，也没有 `..._s16le` 的旧名字：**深度必须写出来。**
//
// # 与 RFC 3190 的一处**故意不同**（做 AES67 互通时会撞上）
//
// RFC 3190 §4 / §8.3 的 `audio/L24` 用**网络序（MSB 优先）**打包 24 位样本。
// 本项目全线小端，`WireDepth::S24` 也是小端。**这不是疏忽**——线上两端都是我们
// 自己，小端与 s16/f32 两档一致，也与 x86/ARM 原生序一致。将来若要与 AES67 /
// RAVENNA 设备直连，这一档**不能**直接对接，要在边界上翻一次字节序。
//
// # §7.2 软件增益兜底与位深的交叉引用（plan §7.2）
//
// 默认路径上「音频流不携带音量、满幅传输、两端不做增益」，所以位深的价值只在
// 听阈那一侧（而听阈上 16 位早已够）。但 §7.2 有一条明文例外：对端真实设备
// **不支持**音量调节时（典型如 macOS 聚合/自定义组合设备），使用端虚拟设备自管
// 音量，**软件增益在发送侧施加**，传输的是带音量的音频。
// ⇒ 那条支路一旦启用，衰减到 −40 dB 的信号在 16 位里只剩约 9 个有效位。
// **该链路的线上位深至少取 24 bit，或在那条路径上加 TPDF dither。**
// 今天那条支路还没有生产代码，这句话是留给启用它的那个人的。

/// 线上样本格式。与 `audiohub_net::packet::Codec` 的 PCM 三个取值一一对应。
///
/// **不叫 `BitDepth`**：`F32` 与 `S24` 在「多少位」这个问题上会给出同一个答案
/// 家族里两个不同的东西（32 位浮点 vs 32 位整数），而遥测要报的恰恰是这个区别。
/// 名字里带「Wire」也是在提醒：它是**线路格式**，与驱动 pin 的格式解耦
/// （见 `docs/design-bitdepth-ladder.md` §2.3）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum WireDepth {
    /// 16 位有符号整数，小端。历史上唯一的线上格式。
    S16,
    /// 24 位有符号整数，**3 字节紧凑打包**，小端（⚠ 与 RFC 3190 的网络序相反）。
    S24,
    /// 32 位浮点，小端。管线内部就是 f32，这一档的编解码退化成字节序搬运——
    /// **线路这一段不做任何量化**。
    F32,
}

impl WireDepth {
    /// 每个样本占多少字节。
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            WireDepth::S16 => 2,
            WireDepth::S24 => 3,
            WireDepth::F32 => 4,
        }
    }

    /// 码率口径用的「位数」。**只用于算带宽，不用于显示**——
    /// 显示要区分 32 位整数与 32 位浮点，那是 [`WireDepth::as_str`] 的事。
    pub const fn bits(self) -> u32 {
        (self.bytes_per_sample() as u32) * 8
    }

    /// 遥测与 IPC 上的拼写。**刻意不报数字 `32`**：`32` 在整数与浮点之间是歧义的，
    /// 而位深进阶梯这件事的全部目的就是消歧。
    pub const fn as_str(self) -> &'static str {
        match self {
            WireDepth::S16 => "s16",
            WireDepth::S24 => "s24",
            WireDepth::F32 => "f32",
        }
    }

    /// `None` = 不是本 build 认识的拼写。**不猜**：猜错会让 UI 报一个线上从没
    /// 出现过的位深，而没有任何一处会报错。
    pub fn parse(s: &str) -> Option<WireDepth> {
        Some(match s {
            "s16" => WireDepth::S16,
            "s24" => WireDepth::S24,
            "f32" => WireDepth::F32,
            _ => return None,
        })
    }
}

/// 解码过程中遇到的**异常**计数。两个都恒为 0 才是正常。
///
/// 它们的价值不在今天（今天两个都恒为 0），而在**将来某次改动让它非零时有人会
/// 看见**。这正是本项目反复栽的那类失效——「算得对所以一直躺着」的推导，
/// 与「坏了但没有任何一处会报错」的静默路径，是同一枚硬币的两面。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeStats {
    /// f32 档解出的非有限值（NaN / ±Inf）个数，已被置 0。
    ///
    /// **这是 f32 档新引入的故障面**：一个 NaN 进了 JB，会经 `mixer_loop` 的求和
    /// 扩散成整段静音或爆音；而 s16/s24 走整数解码，天然不可能产生 NaN。
    pub nonfinite: usize,
    /// 载荷长度不是每样本字节数的整数倍时，被丢弃的残字节数。
    ///
    /// 今天不可能非零（AEAD 保证完整性）。留着它是为了让「将来某次分包/重组
    /// 改动切错了边界」这件事有一个可观测的出口。
    pub ragged: usize,
}

/// 把 f32 样本按指定位深编码成线上字节。分配形态，给探针与测试用。
pub fn encode_pcm(samples: &[f32], depth: WireDepth) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * depth.bytes_per_sample());
    encode_pcm_into(samples, depth, &mut out);
    out
}

/// [`encode_pcm`] 的**零分配**形态：清空 `out` 并就地写入。
///
/// 存在的理由只有一个：`tx_loop` 每 tick 每流调一次，而那条线程上的每一次
/// `malloc` 都是一条尾巴（`docs/spec-latency-floor.md` §9.3 手段 J1）。
/// 调用方持有一个长期复用的缓冲，容量在第一帧之后就不再变。
///
/// `reserve` 在容量够时是一次比较，不是一次分配；写成 `reserve` 而不是
/// `debug_assert!(capacity >= …)` 是因为**换档同时改帧长度与每样本字节数**
/// （48 kHz/16 bit 是 960 字节，48 kHz/32f 是 1920），少给一次容量就变成一次
/// 静默的截断。
///
/// # 三条映射惯例
///
/// - **S16 一字不改**：编码 `× 32767`、解码 `÷ 32768`。改它会让所有既有实测
///   数据不可比。
/// - **S24 同一惯例**：编码用 `2ⁿ⁻¹ − 1`（8 388 607），解码用 `2ⁿ⁻¹`（8 388 608）。
/// - **F32 无量化**：`to_le_bytes()` 直搬，没有 clamp、没有 round。
///   这一档**故意不 clamp**：管线内部就允许过 1.0 的样本，clamp 会在这里
///   偷偷做一次 s16 档才需要的削顶，而削顶率是 Q2 的原料（`ClipMeter`），
///   在这里动手会让那个指标测的是我们自己。
pub fn encode_pcm_into(samples: &[f32], depth: WireDepth, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * depth.bytes_per_sample());
    match depth {
        WireDepth::S16 => {
            for &s in samples {
                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        WireDepth::S24 => {
            for &s in samples {
                let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                // 紧凑打包：取低 3 字节（小端 ⇒ 前 3 个）。
                out.extend_from_slice(&v.to_le_bytes()[..3]);
            }
        }
        WireDepth::F32 => {
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
}

/// 把线上字节按指定位深解回 f32。分配形态，给探针与测试用。
pub fn decode_pcm(bytes: &[u8], depth: WireDepth) -> Vec<f32> {
    let mut out = Vec::with_capacity(bytes.len() / depth.bytes_per_sample());
    decode_pcm_into(bytes, depth, &mut out);
    out
}

/// [`decode_pcm`] 的就地形态：清空 `out` 并就地写入，返回异常计数。
///
/// **返回值不许丢**：`DecodeStats.nonfinite` 是 f32 档唯一的消毒证据。
pub fn decode_pcm_into(bytes: &[u8], depth: WireDepth, out: &mut Vec<f32>) -> DecodeStats {
    let bps = depth.bytes_per_sample();
    out.clear();
    out.reserve(bytes.len() / bps);
    let mut stats = DecodeStats { nonfinite: 0, ragged: bytes.len() % bps };
    match depth {
        WireDepth::S16 => {
            for b in bytes.chunks_exact(2) {
                out.push(i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0);
            }
        }
        WireDepth::S24 => {
            for b in bytes.chunks_exact(3) {
                // 把 3 字节放进 i32 的**高** 3 字节，再算术右移 8：
                // 符号扩展白送，零分支。移位后只有 24 位有效，而 f32 尾数正好
                // 24 位 ⇒ 这一步**精确**，不是近似。
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                out.push(v as f32 / 8_388_608.0);
            }
        }
        WireDepth::F32 => {
            for b in bytes.chunks_exact(4) {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                // 消毒：非有限值一律置 0 并计数。**不许静默**——
                // 一个 NaN 经 `mixer_loop` 的求和会扩散成整段静音或爆音，
                // 而它进来的那一刻在任何日志里都看不见。
                if v.is_finite() {
                    out.push(v);
                } else {
                    stats.nonfinite += 1;
                    out.push(0.0);
                }
            }
        }
    }
    stats
}

/// Stateful linear resampler: carries phase + last sample across calls, so
/// block-split processing equals whole-block processing (within tolerance).
pub struct LinearResampler {
    step: f64, // input samples per output sample
    phase: f64,
    last: f32,
    passthrough: bool,
}

impl LinearResampler {
    pub fn new(src: u32, dst: u32) -> Self {
        LinearResampler {
            step: src as f64 / dst.max(1) as f64,
            phase: 0.0,
            last: 0.0,
            passthrough: src == dst,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        let len = input.len() as f64;
        let mut p = self.phase;
        // position p: 0.0 == previous chunk's last sample, 1.0 == input[0]
        while p < len {
            let i = p.floor() as usize;
            let frac = (p - i as f64) as f32;
            let s0 = if i == 0 { self.last } else { input[i - 1] };
            let s1 = input[i];
            out.push(s0 + (s1 - s0) * frac);
            p += self.step;
        }
        self.phase = p - len;
        self.last = *input.last().unwrap();
    }
}

#[cfg(test)]
mod zero_alloc_tests {
    use super::*;

    const DEPTHS: [WireDepth; 3] = [WireDepth::S16, WireDepth::S24, WireDepth::F32];

    /// `encode_pcm_into` 必须与 `encode_pcm` 逐字节相同，且复用时不再分配。
    ///
    /// **三种深度各跑一遍**：换档同时改帧长度与每样本字节数，只测一种深度的
    /// 版本会漏掉「reserve 按旧的每样本字节数算」这类错误。
    ///
    /// 注入对照：把 `encode_pcm_into` 里的 `out.clear()` 删掉，第二轮的内容
    /// 断言立刻变红（内容会累积）。
    #[test]
    fn converting_in_place_matches_the_allocating_form_and_reuses_the_buffer() {
        let samples: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0) * 2.0 - 1.0).collect();
        for depth in DEPTHS {
            let want = encode_pcm(&samples, depth);
            assert_eq!(
                want.len(),
                samples.len() * depth.bytes_per_sample(),
                "{depth:?} 的每样本字节数与实际写出的字节数对不上"
            );
            let mut out = Vec::new();
            encode_pcm_into(&samples, depth, &mut out);
            assert_eq!(out, want, "{depth:?}");
            let (cap, ptr) = (out.capacity(), out.as_ptr());
            for _ in 0..32 {
                encode_pcm_into(&samples, depth, &mut out);
                assert_eq!(out, want, "{depth:?} 复用之后内容变了（多半是没清空）");
                assert_eq!(out.capacity(), cap, "{depth:?} 缓冲被重新分配了");
                assert_eq!(out.as_ptr(), ptr, "{depth:?} 缓冲搬家了 = 一次 malloc");
            }
            // 换档（rung）会把帧变短：短帧必须只有短帧的内容。
            encode_pcm_into(&samples[..240], depth, &mut out);
            assert_eq!(out, encode_pcm(&samples[..240], depth), "{depth:?} 短帧");
        }
    }

    /// 三种深度的 `encode → decode` 往返精度。
    ///
    /// - S16 / S24：误差 ≤ 1 LSB。
    /// - **F32：逐位相等**（这一档的全部卖点就是「线路这一段不做任何量化」；
    ///   哪怕退化成 `≤ 1 LSB` 也是在放过一个真 bug）。
    ///
    /// 注入对照：把 S24 的编码常数从 `8_388_607.0` 改成 `32767.0`（即偷偷按
    /// 16 位量化再塞进 3 字节），S24 这一支的误差断言立刻变红。
    #[test]
    fn each_wire_depth_round_trips_within_one_lsb_and_f32_is_exact() {
        // 覆盖满幅、零、正负极值附近。
        let samples: Vec<f32> = (0..2048)
            .map(|i| (i as f32 / 1024.0 - 1.0).clamp(-1.0, 1.0))
            .collect();
        for depth in DEPTHS {
            let bytes = encode_pcm(&samples, depth);
            let mut back = Vec::new();
            let st = decode_pcm_into(&bytes, depth, &mut back);
            assert_eq!(st, DecodeStats::default(), "{depth:?} 干净输入不该有异常计数");
            assert_eq!(back.len(), samples.len(), "{depth:?} 样本数变了");
            if depth == WireDepth::F32 {
                for (i, (&a, &b)) in samples.iter().zip(back.iter()).enumerate() {
                    assert_eq!(
                        a.to_bits(),
                        b.to_bits(),
                        "f32 档必须逐位相等，第 {i} 个样本被动过了"
                    );
                }
                continue;
            }
            // 1 LSB = 2 / 2^bits（满量程 [-1, 1) 分成 2^bits 级）。
            let lsb = 2.0f32 / (1u32 << (depth.bits() - 1)) as f32;
            for (i, (&a, &b)) in samples.iter().zip(back.iter()).enumerate() {
                assert!(
                    (a - b).abs() <= lsb,
                    "{depth:?} 第 {i} 个样本往返误差 {} 超过 1 LSB {lsb}",
                    (a - b).abs()
                );
            }
        }
    }

    /// **S24 的量化台阶必须真的是 24 位**，不是「按 16 位量化再塞进 3 字节」。
    ///
    /// 前一条的 `≤ 1 LSB` 是按 24 位的 LSB 算的，所以它已经能抓到这件事；
    /// 这一条从相反方向再钉一次：**相邻两个 24 位台阶必须解出不同的值**。
    /// 若编码端偷偷降到 16 位，两个相邻台阶会解成同一个数。
    #[test]
    fn s24_actually_resolves_twenty_four_bits() {
        let step = 1.0f32 / 8_388_607.0;
        let a = encode_pcm(&[0.5], WireDepth::S24);
        let b = encode_pcm(&[0.5 + step * 4.0], WireDepth::S24);
        assert_ne!(a, b, "相邻的 24 位台阶编成了同一串字节：位深没真的到 24");
        let da = decode_pcm(&a, WireDepth::S24)[0];
        let db = decode_pcm(&b, WireDepth::S24)[0];
        assert!(db > da, "24 位台阶解出来没有单调递增：{da} → {db}");
        assert!(
            (db - da) < 1.0 / 32767.0,
            "两个 24 位台阶的差 {} 大到了 16 位 LSB 的量级：多半是按 16 位量化的",
            db - da
        );
    }

    /// **S24 的负值必须符号扩展正确。** 「3 字节放进高位再算术右移」这个技巧
    /// 一旦写成逻辑右移（或忘了移），负半轴会整体翻成大正数——听感上是持续爆音，
    /// 而正半轴完全正常，只测正值的测试抓不到。
    #[test]
    fn s24_sign_extends_negative_samples() {
        for &v in &[-1.0f32, -0.75, -0.5, -0.001, -1.0 / 8_388_607.0] {
            let back = decode_pcm(&encode_pcm(&[v], WireDepth::S24), WireDepth::S24)[0];
            assert!(back < 0.0, "{v} 解出来成了 {back}：符号扩展错了");
            assert!((back - v).abs() <= 2.0 / 8_388_608.0, "{v} → {back}");
        }
    }

    /// **f32 档的解码必须消毒。** 一个 NaN 进了 JB 会经 `mixer_loop` 的求和
    /// 扩散成整段静音或爆音，而它进来的那一刻在任何日志里都看不见。
    ///
    /// 注入对照：把 `decode_pcm_into` 的 `is_finite()` 分支删成 `out.push(v)`，
    /// 这条立刻变红（`nonfinite` 恒为 0 且输出里有 NaN）。
    #[test]
    fn the_f32_decoder_scrubs_non_finite_values_and_counts_them() {
        let mut bytes = Vec::new();
        for v in [1.0f32, f32::NAN, 0.5, f32::INFINITY, -0.25, f32::NEG_INFINITY] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        let st = decode_pcm_into(&bytes, WireDepth::F32, &mut out);
        assert_eq!(st.nonfinite, 3, "三个非有限值应当各计一次");
        assert_eq!(st.ragged, 0);
        assert!(out.iter().all(|v| v.is_finite()), "输出里还有非有限值：{out:?}");
        assert_eq!(out, vec![1.0, 0.0, 0.5, 0.0, -0.25, 0.0]);
        // 整数档天然不可能产生非有限值——喂同一串字节也不该有计数。
        for depth in [WireDepth::S16, WireDepth::S24] {
            let mut o = Vec::new();
            let s = decode_pcm_into(&bytes, depth, &mut o);
            assert_eq!(s.nonfinite, 0, "{depth:?} 是整数解码，不该产生非有限值");
            assert!(o.iter().all(|v| v.is_finite()));
        }
    }

    /// 残字节被丢弃**并计数**。今天不可能发生（AEAD 保证完整性），
    /// 计数器的价值恰恰在于将来某次改动让它非零时有人会看见。
    #[test]
    fn a_ragged_payload_is_dropped_but_counted() {
        for (depth, extra) in [(WireDepth::S16, 1usize), (WireDepth::S24, 2), (WireDepth::F32, 3)] {
            let mut bytes = encode_pcm(&[0.25, -0.25], depth);
            let full = bytes.len();
            bytes.extend(std::iter::repeat(0u8).take(extra));
            let mut out = Vec::new();
            let st = decode_pcm_into(&bytes, depth, &mut out);
            assert_eq!(st.ragged, extra, "{depth:?} 残字节没被计数");
            assert_eq!(out.len(), full / depth.bytes_per_sample(), "{depth:?} 残字节没被丢弃");
        }
    }

    /// 三个拼写各自能原样转一圈，且**不许出现数字 `32`**。
    ///
    /// `32` 在整数与浮点之间是歧义的，而位深进阶梯这件事的全部目的就是消歧。
    #[test]
    fn the_depth_spellings_round_trip_and_never_report_a_bare_thirty_two() {
        for depth in DEPTHS {
            assert_eq!(WireDepth::parse(depth.as_str()), Some(depth));
            assert!(
                !depth.as_str().contains("32") || depth.as_str() == "f32",
                "{depth:?} 的拼写 {} 里出现了裸的 32",
                depth.as_str()
            );
        }
        assert_eq!(WireDepth::parse("32"), None);
        assert_eq!(WireDepth::parse("s32"), None);
        assert_eq!(WireDepth::parse(""), None);
        assert_eq!(WireDepth::parse("S16"), None, "拼写是精确匹配，不做大小写吸附");
    }
}
