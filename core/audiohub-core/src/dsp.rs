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

// ------------------------------------------------------------- 判音的带内口径
//
// # 为什么单格相干功率是一个**坏量具**（终审 §二.14 / §二.7 同一根因）
//
// 原来的判据是「目标频率那一个 Goertzel 格的功率 / 其余全部功率」，分析窗是
// 矩形的 100 ms。矩形窗 + 单格 ⇒ 判读**只在音调恰好落在格心时才成立**：音调
// 偏离格心 δ 格时，落进那一格的功率是 `sinc²(δ)`，剩下的全被记成噪声，于是
//
// ```text
// snr_db = 10·log10( sinc²(δ) / (1 − sinc²(δ)) )
// ```
//
// 这个式子不是推测，是实测（本机 2026-08-09，跑生产播放路径、48 k↔48 k）：
//
// | 伺服修正 | δ（格） | 上式 | 实测 |
// |---|---|---|---|
// | 10 ppm | 0.002 | 48.81 dB | 48.58 dB |
// | 100 ppm | 0.020 | 28.80 dB | 28.87 dB |
// | 500 ppm | 0.100 | 14.74 dB | **14.74 dB** |
//
// 而 500 ppm 正是 [`crate::audio::PlayServo::MAX_PPM`]——**播放伺服的合法钳位**。
// `LivePlayback::on_device` 让**任何按名字寻址的播放都恒过一次带伺服的变速
// 重采样，即使 48 k 对 48 k**（`audio.rs` 那句「弯比率是环路唯一的执行器」），
// 于是声卡晶振与我们之间那几十到几百 ppm 的失配会原样变成音调的频率偏移。
// 换算成判读：2 kHz 的音调只要伺服在修正 **275 ppm** 就掉到 20 dB 判据上——
// 而 275 ppm 对人耳是 0.48 音分，**听不出来**。
//
// ⇒ 20.0 dB 的判据坐在量具自身的噪声带里，「放音 + 判音」的断言在抛硬币。
//
// # 修法：不动阈值，换量具
//
// 抬阈值只是把硬币换个面：噪声带会随负载/设备/晶振移动，而且掩盖真实的劣化。
// 这里改的是**判读的口径**——从「一个格」改成「一条 Hann 主瓣」：
//
// 1. 先给分析窗乘 Hann。矩形窗的旁瓣只有 −13 dB 且以 6 dB/oct 衰减，偏格的
//    音调会把功率洒满整个频轴；Hann 的旁瓣 −31 dB、18 dB/oct，功率被收进
//    **4 格宽的主瓣**里。
// 2. 带内功率 = 主瓣覆盖的那几格之和，而不是中心一格。
//
// 结果（同一套实测，见 `servo_bend_no_longer_defeats_the_tone_verdict`）：
// 伺服顶到 500 ppm 钳位时，2 kHz 的判读从 14.74 dB 抬到 **45.5 dB**——
// 阈值从「低于噪声带 5 dB」变成「低于最坏合法读数 25 dB」。
//
// # 这两个常数都**不是可调旋钮**
//
// - 格宽恒为 10 Hz：`verify_tone` 的窗恒为 `sr/10`，所以 `sr/win = 10`，
//   **与采样率无关**（16 k/24 k/32 k/44.1 k/48 k 实测同一个读数）。
// - [`LOBE_HALF_BINS`] = 3，**推导如下，不是调出来的**：Hann 主瓣宽 4 格，
//   即以音调**真实频率**为心的 ±2 格。而求和只能落在整数格上，音调的真实
//   频率相对最近格心还有 ±0.5 格的任意偏移 δ ⇒ 要在**任何** δ 下装下整条
//   主瓣，需要 ±(2 + 0.5) 格，取整就是 **±3**。
//
//   ⚠ 先写成 ±2 是错的，而且错得很典型：那等于**默认 δ = 0**——正是让旧量具
//   垮掉的那个假设。守卫测试当场把它抓了出来（4 kHz、−500 ppm 上只剩
//   19.61 dB 余量）。留着这段话是因为下一个人多半会想「±2 就够了」。

/// 判音的分析格宽（Hz）。`verify_tone` 的窗恒为 `sr/10` ⇒ 恒为 10 Hz。
pub const TONE_BIN_HZ: f64 = 10.0;

/// 求和覆盖的半宽（格）。**由窗函数推导，不是可调旋钮**——见模块内那段。
const LOBE_HALF_BINS: i32 = 3;

/// `detected` 的判据（dB）。**故意留在 20.0**：§二.14 的病灶是量具，不是阈值，
/// 抬阈值只会把噪声带换个位置继续抛硬币。它是 `pub` 的，好让守卫测试能断言
/// 「最坏的合法读数离它还有多远」——见
/// `servo_bend_no_longer_defeats_the_tone_verdict::the_threshold_keeps_its_margin_against_every_legal_servo_bend`。
pub const TONE_DETECT_DB: f32 = 20.0;

/// 就地给 `dst` 写入 `chunk` 的 Hann 加窗结果。窗表按 `chunk.len()` 现算。
fn hann_into(chunk: &[f32], dst: &mut Vec<f32>) {
    dst.clear();
    dst.reserve(chunk.len());
    let n = chunk.len() as f64;
    for (i, &x) in chunk.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos();
        dst.push((x as f64 * w) as f32);
    }
}

/// 一个**已加窗**的分析块里，`freq_hz` 那条主瓣内的功率与总功率。
///
/// 两者同尺度（都是 `goertzel_power` 那个 `mean(x²)/2` 的量纲），所以调用方
/// 既能取比值（信噪比）也能取绝对值（幅度）。
fn lobe_and_total(windowed: &[f32], sample_rate: u32, freq_hz: f32) -> (f64, f64) {
    let bin = sample_rate as f64 / windowed.len() as f64;
    let nyq = sample_rate as f64 / 2.0;
    let k0 = (freq_hz as f64 / bin).round();
    let mut inband = 0.0f64;
    for d in -LOBE_HALF_BINS..=LOBE_HALF_BINS {
        let f = (k0 + d as f64) * bin;
        if f <= 0.0 || f >= nyq {
            continue;
        }
        inband += goertzel_power(windowed, sample_rate, f as f32) as f64;
    }
    let total: f64 = windowed.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
        / (windowed.len() as f64)
        / 2.0;
    (inband, total)
}

/// 带内功率占比，dB。这是**判音的唯一口径**——`verify_tone` 与
/// [`tone_amplitude`] 都只经过它。
///
/// 静音（带内与带外都在 eps 以下）读 0 dB，与旧口径一致。
pub fn tone_snr_db(chunk: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    let mut w = Vec::new();
    hann_into(chunk, &mut w);
    let (inband, total) = lobe_and_total(&w, sample_rate, freq_hz);
    let eps = 1e-12f64;
    let noise = (total - inband).max(0.0) + eps;
    (10.0 * (inband.max(eps) / noise).log10()) as f32
}

/// `freq_hz` 附近的**等效正弦幅度**（与 `--vi-amp`、与 peak 同尺度）。
///
/// 存在的理由与 [`tone_snr_db`] 完全相同，而且这条路上的病更重：
/// `volindep` 把整段 2 秒当一个窗量，格宽 0.5 Hz，1 kHz 上 500 ppm 的伺服修正
/// 就是**整整一格**——单格 Goertzel 在那里读到的是一个零点。换成主瓣求和之后，
/// 通带覆盖整条主瓣（见 [`LOBE_HALF_BINS`]），同一段信号读回原幅度。
///
/// Hann 窗的相干增益是 0.5、功率增益是 3/8，所以主瓣功率之和 = `3A²/32`
/// （中心格 `A²/16` + 两侧各 `A²/64`），反解 `A = sqrt(32·P/3)`。
pub fn tone_amplitude(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut w = Vec::new();
    hann_into(samples, &mut w);
    let (inband, _) = lobe_and_total(&w, sample_rate, freq_hz);
    ((inband.max(0.0) * 32.0 / 3.0).sqrt()) as f32
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
    // 复用同一块加窗缓冲：窗长恒定，第一块之后不再分配。
    let mut w: Vec<f32> = Vec::with_capacity(win);
    for chunk in usable.chunks(win) {
        if chunk.len() < win {
            break;
        }
        analyzed += chunk.len();
        hann_into(chunk, &mut w);
        let (inband, total) = lobe_and_total(&w, sample_rate, freq_hz);
        let eps = 1e-12f64;
        let noise = (total - inband).max(0.0) + eps;
        snrs.push((10.0 * (inband.max(eps) / noise).log10()) as f32);
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
        detected: snr_db > TONE_DETECT_DB,
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
//
// 那条支路现在有生产代码了，就是本文件的 [`SendGain`]。plan 那句「至少 24 bit
// **或** TPDF dither」在它里面是**按档分工**兑现的，不是二选一：
//
// - rung 0（48 k/f32）与 rung 1（48 k/s24）本身就满足「至少 24 bit」，
//   [`dither_lsb`] 对这两档返回 0，一个字节的噪声都不掺；
// - rung 2..5 全是 s16（48/32/24/16 kHz），由 [`SendGain`] 就地加高通 TPDF。
//
// **没有**改成「兜底一启用就把格号钳到 ≤1」。阶梯存在的全部理由是让链路在坏
// 网络上活着；让一次拖动滑块把线上码率从 256 kbps 顶到 1152 kbps，等于拿
// 「还有声音」去换「音量能调」——而兜底本身只是一个音量开关。

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

// ------------------------------------------------------ §7.2 发送侧软件增益

/// 增益的**转移速率**：走完满量程要这么久。
///
/// 是恒定**斜率**，不是恒定时长。拖滑块产生的是一串小步，恒定时长下每一步都要
/// 摊满 20 ms，下一步到来时上一步还没走完 ⇒ 增益永远追不上手指；恒定斜率下小步
/// 几乎立刻到位，而一次 0→1 的大跳仍然被摊到 20 ms 上（这才是防爆音要的那一半）。
const GAIN_SLEW_MS: u32 = 20;

/// 这一档的量化台阶（f32 域）——也就是 dither 的幅度基准。0 = 这一档不掺。
///
/// **只有 s16 非零。** 这是模块头那条「至少 24 bit **或** TPDF dither」的落地
/// 分工：f32 档线路上根本不量化，s24 档的量化噪声在 −144 dBFS，衰减 40 dB 之后
/// 仍在听阈以下——这两档自己就占住了「至少 24 bit」那一半，再掺 dither 只是白白
/// 抬高噪声底。低采样率的三个格（32/24/16 kHz）都是 s16，一并由这里覆盖。
fn dither_lsb(depth: WireDepth) -> f32 {
    match depth {
        // `encode_pcm_into` 的 s16 惯例是「× 32767 再 round」⇒ 台阶就是 1/32767。
        WireDepth::S16 => 1.0 / 32767.0,
        WireDepth::S24 | WireDepth::F32 => 0.0,
    }
}

/// plan §7.2 的**兜底支路**：对端真实设备没有我们驱得动的音量时（典型如 macOS
/// 音频 MIDI 设置拼出来的聚合设备），使用端虚拟设备自管音量，增益在**发送侧**
/// 施加——线上从此传的是带音量的音频。
///
/// # 为什么状态是「每条流一份」而不是「每个源一份」
///
/// 采集源在 `engine.rs` 里是**扇出**的：一个 `SourceEnt` 的 `frame` 每 tick 读一次、
/// 发给挂在它上面的 N 条流（物理队列只有一份）。而增益是**对端**属性——同一支
/// 麦克风送给两个对端，两个对端的增益可以不同。所以：
///
/// - 斜坡进度与 dither 相位挂在 `TxStream` 上，与 `rung`、重采样器同级；
/// - [`SendGain::apply`] 的入参是 `&[f32]`，**结构上无法就地改写那份共享帧**。
///
/// 就地改写的后果不是「串台一次」而是**逐帧复利**：0.5 的增益在第 k 帧上累成
/// 0.5ᵏ，而包数、丢包率、音调探针全绿——本项目栽过的那个形状。
///
/// # 默认路径一次乘法都不做
///
/// `tx_loop` 是 10 ms 截止期线程，而**满幅是默认、增益是例外**。[`SendGain::apply`]
/// 在透明态把入参**原样**还回去（同一个指针）：不复制、不分配、不乘。
///
/// 透明的判据是**增益值**，不是「兜底有没有启用」：兜底启用而用户把音量拉到
/// 100 % 时，线上应当与默认路径逐位相同，那时也不该有乘法和 dither。
pub struct SendGain {
    /// 此刻真正施加的增益。斜坡的状态量，跨调用保持（与 [`LinearResampler`] 同理）。
    cur: f32,
    /// 斜坡的去向。
    target: f32,
    /// dither 的 LCG 状态。常数与 `audiohub_net::media::LossInjector` 同一套。
    rng: u64,
    /// 上一个均匀抽样。高通 TPDF 靠「相邻两个抽样之差」构造三角分布，见
    /// [`SendGain::dither`]。
    prev_u: f32,
}

impl Default for SendGain {
    fn default() -> Self {
        SendGain::new()
    }
}

impl SendGain {
    /// 透明（满幅）。**这是默认态**，兜底不启用的流一辈子停在这里。
    pub fn new() -> SendGain {
        SendGain {
            cur: 1.0,
            target: 1.0,
            // 非零即可；固定值 ⇒ dither 序列可复现，测试因此不靠运气。
            rng: 0x2545_F491_4F6C_DD1D,
            prev_u: 0.0,
        }
    }

    /// 设定去向。`1.0` = 把音量交还给对端真实设备（兜底未启用或刚解除）。
    ///
    /// 非有限值一律读作 1.0。这个值是从一个原子量穿过来的，而 10 ms 线程上唯一
    /// 比「音量没反应」更糟的是**把 NaN 乘进音频**——一个 NaN 进了对端 JB 会经
    /// `mixer_loop` 的求和扩散成整段静音或爆音（见 [`DecodeStats::nonfinite`]）。
    /// 真正的校验在写入侧（`conn.rs` 拒收非有限 scalar）；这里只是不给自己留一条
    /// 能把 NaN 送上线的路。
    pub fn set_target(&mut self, gain: f32) {
        self.target = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 1.0 };
    }

    /// 此刻**真正施加**的增益（斜坡的当前值，不是去向）。
    pub fn current(&self) -> f32 {
        self.cur
    }

    /// 这一刻线上与默认路径逐位相同 ⇒ [`SendGain::apply`] 是一次比较加一次返回。
    pub fn is_transparent(&self) -> bool {
        self.cur == 1.0 && self.target == 1.0
    }

    /// 施加增益。透明态返回 `input` **本身**；否则把结果写进 `out` 并返回它。
    ///
    /// `rate_hz` 是**线上**采样率（这一步在重采样之后），斜坡因此在任何格号上都
    /// 走同样的毫秒数；`depth` 决定要不要掺 dither（见 [`dither_lsb`]）。
    ///
    /// `out` 由调用方长期复用：这条线程上的每一次 `malloc` 都是一条尾巴
    /// （`docs/spec-latency-floor.md` §9.3 手段 J1），与 `encode_pcm_into` 同一条
    /// 纪律。
    pub fn apply<'a>(
        &mut self,
        input: &'a [f32],
        rate_hz: u32,
        depth: WireDepth,
        out: &'a mut Vec<f32>,
    ) -> &'a [f32] {
        if self.is_transparent() {
            return input;
        }
        out.clear();
        out.reserve(input.len());
        // 每样本的最大位移。`max(1)` 只是不让荒谬的小采样率把步长除成 0
        //（步长为 0 的斜坡永远收敛不了，增益会永久卡在起点）。
        let slew = 1.0 / ((rate_hz as u64 * GAIN_SLEW_MS as u64 / 1000).max(1) as f32);
        // **静音就是数字静音。** 斜坡已经到 0 且不打算离开时不再掺 dither，
        // 否则「静音」在 s16 档上听起来是一段 −90 dBFS 的嘶声。
        let lsb = if self.cur == 0.0 && self.target == 0.0 {
            0.0
        } else {
            dither_lsb(depth)
        };
        for &x in input {
            if self.cur != self.target {
                let d = self.target - self.cur;
                // 剩下的路不够一步就直接落到目标：否则会在目标附近永久抖动。
                self.cur = if d.abs() <= slew { self.target } else { self.cur + slew.copysign(d) };
            }
            // **一次乘法，不是两次。** 写成 `x * self.cur * something` 就是
            // plan §12.5 那条「不存在双重衰减」在本机这一侧的失效形态。
            let y = x * self.cur;
            out.push(if lsb > 0.0 { y + self.dither(lsb) } else { y });
        }
        out
    }

    /// 一次高通 TPDF 抽样，幅度 ±1 LSB。
    ///
    /// TPDF（三角概率密度）是把量化误差从「与信号相关的失真」变成「与信号无关的
    /// 噪声」所需的最小 dither，而这正是模块头那句警告的落点：衰减 40 dB 之后
    /// s16 只剩约 9 个有效位，此时**难听的不是噪声底而是相关失真**。它还有一个
    /// 更硬的性质——dither 之后量化器的**平均值无偏**，也就是说「拖到 30 % 就真的
    /// 是 30 %」，而无 dither 的量化器在低电平上有一个确定的直流偏差。
    ///
    /// 用「相邻两个均匀抽样之差」而不是「两个独立抽样之和」：
    ///
    /// 1. 两者分布同为 [−1, 1] 上的三角分布，去相关效果相同；
    /// 2. 差的形式**每样本只走一次 LCG**，和的形式要两次；
    /// 3. 差在频域上是一阶高通 ⇒ 噪声被推到听感最不敏感的高频。
    ///
    /// 这就是通常说的 highpass TPDF。
    fn dither(&mut self, lsb: f32) -> f32 {
        // Knuth MMIX 常数。
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // 取高 24 位：LCG 的低位周期短，而 24 位正好是 f32 尾数的宽度。
        let u = (self.rng >> 40) as f32 / (1u64 << 24) as f32; // [0, 1)
        let d = u - self.prev_u;
        self.prev_u = u;
        d * lsb
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

/// plan §7.2 兜底支路（[`SendGain`]）。
#[cfg(test)]
mod send_gain_tests {
    use super::*;

    const SR: u32 = 48_000;
    const FRAME: usize = 480; // 10 ms @ 48 kHz，与 `tx_loop` 的一 tick 同长

    /// 把增益推到目标并让斜坡走完，返回稳态之后的**一帧**输出。
    fn settle(g: &mut SendGain, input: &[f32], depth: WireDepth) -> Vec<f32> {
        let mut scratch = Vec::new();
        // 满量程斜坡要 GAIN_SLEW_MS = 20 ms = 两帧；跑四帧留足余量。
        for _ in 0..4 {
            g.apply(input, SR, depth, &mut scratch);
        }
        g.apply(input, SR, depth, &mut scratch).to_vec()
    }

    /// **默认路径一次乘法、一次复制、一次分配都不做。**
    ///
    /// 判据是**指针相等**：返回的就是入参那块内存，而不是一份内容相同的拷贝。
    /// 内容相等的断言抓不到「老老实实复制了一遍」——而那正是要避免的开销。
    ///
    /// 注入对照：把 `apply` 开头的 `if self.is_transparent() { return input; }`
    /// 删掉，指针断言与「暂存必须还是空的」同时变红。
    #[test]
    fn the_default_path_returns_the_input_slice_itself_and_never_touches_the_scratch() {
        let input: Vec<f32> = gen_sine(1_000.0, SR, FRAME, 0.9);
        for depth in [WireDepth::S16, WireDepth::S24, WireDepth::F32] {
            let mut g = SendGain::new();
            assert!(g.is_transparent(), "新建的 SendGain 必须是透明的");
            let mut scratch = Vec::new();
            let out = g.apply(&input, SR, depth, &mut scratch);
            assert_eq!(
                out.as_ptr(),
                input.as_ptr(),
                "{depth:?} 默认路径复制了一份 —— 10 ms 线程上白付一次遍历"
            );
            assert!(scratch.is_empty(), "{depth:?} 默认路径动了暂存缓冲");
            assert_eq!(scratch.capacity(), 0, "{depth:?} 默认路径分配了内存");
        }
        // 兜底启用但音量在 100 %：仍然必须是透明的（线上与默认路径逐位相同）。
        let mut g = SendGain::new();
        g.set_target(1.0);
        assert!(g.is_transparent(), "增益 = 1.0 却不透明：白付一次乘法 + 一次 dither");
    }

    /// **增益变更必须走斜坡。** 一次跳变就是一次阶跃，阶跃就是爆音；
    /// plan §7.2 明写「增益渐变，避免爆音」。
    ///
    /// 直流输入 1.0 + f32 档（不掺 dither）⇒ 输出逐样本就是增益本身，
    /// 于是「有没有爆音」这个问题变成了一句可断言的话。
    ///
    /// 注入对照：把 `apply` 里的斜坡换成 `self.cur = self.target;`，
    /// 「单样本位移」这一条立刻变红（第一个样本就跳满 1.0）。
    #[test]
    fn a_volume_change_slews_instead_of_stepping() {
        let dc = vec![1.0f32; FRAME];
        let mut g = SendGain::new();
        g.set_target(0.0);
        let mut scratch = Vec::new();
        let out = g.apply(&dc, SR, WireDepth::F32, &mut scratch).to_vec();
        // 满量程 / 20 ms ⇒ 48 kHz 上每样本至多走 1/960。
        let max_step = 1.0f32 / 960.0;
        let mut prev = 1.0f32;
        for (i, &y) in out.iter().enumerate() {
            assert!(
                (prev - y) <= max_step * 1.000_01,
                "第 {i} 个样本增益从 {prev} 跳到 {y}，超过一步 {max_step} —— 这是一次爆音"
            );
            assert!(y <= prev, "斜坡在往回走：第 {i} 个样本 {prev} -> {y}");
            prev = y;
        }
        // 一帧 = 10 ms = 半程。**这一条钉住的是「够慢」**：只断言不跳变的话，
        // 一个 0.999 的步长照样过关，而那与跳变听不出区别。
        assert!(
            (g.current() - 0.5).abs() < 1e-3,
            "10 ms 之后增益应当刚好走完满量程的一半，实际 {}",
            g.current()
        );
        // 再一帧走完，并且**精确停在**目标上（不越过、不抖动、不留残差）。
        //
        // 「精确」是承重的而不是洁癖：斜坡回到 1.0 时若留下 1e-6 的残差，
        // [`SendGain::is_transparent`] 就永远回不了 true，于是一条音量已经拉满的
        // 流会永远多付一次乘法加一次 dither。第三帧（而不是第二帧）才断言相等，
        // 是因为 960 次 1/960 的浮点累加会差出几个 ULP，最后那一步要等下一个
        // 样本才吸附得上 —— 也就是「至多 20 ms + 1 个样本」。
        g.apply(&dc, SR, WireDepth::F32, &mut scratch);
        assert!(g.current().abs() < 1e-4, "两帧之后还差 {}", g.current());
        g.apply(&dc, SR, WireDepth::F32, &mut scratch);
        assert_eq!(g.current(), 0.0, "斜坡没有精确落到目标上（或越了过去）");
        // 回程同理：必须精确回到 1.0，否则再也变不回透明。
        g.set_target(1.0);
        for _ in 0..3 {
            g.apply(&dc, SR, WireDepth::F32, &mut scratch);
        }
        assert_eq!(g.current(), 1.0, "回程没有精确回到满幅");
        assert!(g.is_transparent(), "回到满幅之后没有恢复透明 —— 白付乘法与 dither");
    }

    /// 斜坡时长与**格号无关**：低采样率格上必须还是 20 ms，不是 20 ms × 3。
    ///
    /// 注入对照：把 `apply` 里的 `rate_hz` 换成常数 48_000，16 kHz 那一支变红。
    #[test]
    fn the_slew_takes_the_same_milliseconds_on_every_rung() {
        for rate in [48_000u32, 32_000, 24_000, 16_000] {
            let mut g = SendGain::new();
            g.set_target(0.0);
            // 20 ms 的样本数 = 满量程斜坡的长度（+1 是浮点累加的吸附样本，
            // 见上一条测试里的说明）。
            let n = (rate as usize * GAIN_SLEW_MS as usize) / 1000 + 1;
            let dc = vec![1.0f32; n];
            let mut scratch = Vec::new();
            g.apply(&dc, rate, WireDepth::F32, &mut scratch);
            assert_eq!(g.current(), 0.0, "{rate} Hz 上 20 ms 没走完满量程斜坡");
            let mut g = SendGain::new();
            g.set_target(0.0);
            let half = vec![1.0f32; n / 2];
            g.apply(&half, rate, WireDepth::F32, &mut scratch);
            assert!(
                (g.current() - 0.5).abs() < 0.02,
                "{rate} Hz 上 10 ms 走了 {}，不是半程 —— 斜坡按样本数而不是按时间算的",
                1.0 - g.current()
            );
        }
    }

    /// **扇出：一个源、两条流、两个增益。**
    ///
    /// 同一支麦克风送给两个对端，两个对端的增益可以不同。这条钉三件事：
    /// ① 共享帧一个样本都没被改；② 透明的那条流拿到的还是原帧；
    /// ③ 衰减的那条流**只衰减一次**（不是逐帧复利，也不是双重衰减）。
    ///
    /// 注入对照（③）：把 `apply` 里的 `x * self.cur` 写成 `x * self.cur * self.cur`
    ///（plan §12.5「不存在双重衰减」的失效形态），第三条立刻变红。
    #[test]
    fn one_source_frame_fans_out_to_two_streams_with_independent_gains() {
        let source: Vec<f32> = gen_sine(440.0, SR, FRAME, 0.8);
        let pristine = source.clone();
        let mut quiet = SendGain::new();
        quiet.set_target(0.25);
        let mut loud = SendGain::new(); // 透明：这条流仍然要满幅

        let mut sa = Vec::new();
        let mut sb = Vec::new();
        // 九帧：就地改写的话，`source` 会被复利成 0.25⁹ ≈ 4e-6。
        let mut ya = Vec::new();
        let mut yb = Vec::new();
        for _ in 0..9 {
            ya = quiet.apply(&source, SR, WireDepth::F32, &mut sa).to_vec();
            yb = loud.apply(&source, SR, WireDepth::F32, &mut sb).to_vec();
        }
        assert_eq!(source, pristine, "共享的源帧被就地改写了 —— 扇出的其余流全被带偏");
        assert_eq!(yb, pristine, "透明的那条流没有拿到原帧");
        assert_eq!(ya.len(), source.len(), "输出长度变了 —— 暂存多半没被清空");
        for (i, (&x, &y)) in pristine.iter().zip(ya.iter()).enumerate() {
            assert!(
                (y - x * 0.25).abs() < 1e-6,
                "第 {i} 个样本衰减了不止一次：期望 {}，实际 {y}",
                x * 0.25
            );
        }
    }

    /// **s16 档掺 dither，深档不掺。** 模块头那条「至少 24 bit **或** dither」
    /// 的按档分工，正着反着各钉一次。
    ///
    /// 注入对照：`dither_lsb` 的 S16 分支返回 0.0 ⇒ 第一条变红；
    /// 让 S24 分支返回非零 ⇒ 深档那条变红。
    #[test]
    fn only_the_sixteen_bit_rungs_get_dither() {
        let quiet = vec![0.0f32; 4096];
        let mut g = SendGain::new();
        g.set_target(0.5);
        let y = settle(&mut g, &quiet, WireDepth::S16);
        assert!(
            y.iter().any(|v| *v != 0.0),
            "s16 档没有掺 dither —— 衰减之后量化误差会与信号相关（听感是失真而不是噪声）"
        );
        let lsb = 1.0f32 / 32767.0;
        assert!(
            y.iter().all(|v| v.abs() <= lsb * 1.000_01),
            "dither 幅度超过 1 LSB：{:?}",
            y.iter().cloned().fold(0.0f32, |a, b| a.max(b.abs())) / lsb
        );
        for depth in [WireDepth::S24, WireDepth::F32] {
            let mut g = SendGain::new();
            g.set_target(0.5);
            let y = settle(&mut g, &quiet, depth);
            assert!(
                y.iter().all(|v| *v == 0.0),
                "{depth:?} 掺了 dither —— 这一档本身就满足「至少 24 bit」，白抬噪声底"
            );
        }
    }

    /// **§7.2 那句警告的验收。** dither 之后的量化器**平均值无偏**：
    /// 把音量拖到 x，线上解出来的就真的是 x；无 dither 的量化器在低电平上有一个
    /// **确定的**直流偏差（不是噪声，是偏差 —— 它不会随时间平均掉）。
    ///
    /// 构造：直流输入 1.0，增益取到「100.3 个 16 位码字」这个刻意的非整数位置。
    /// 无 dither ⇒ 每个样本都 round 到 100，偏差恒为 0.3 LSB；
    /// 有 dither ⇒ 均值收敛回 100.3。
    ///
    /// 注入对照：`dither_lsb` 的 S16 分支返回 0.0，这条立刻变红
    ///（生产路径的偏差跳到 0.3 LSB，与那条对照路径一模一样）。
    #[test]
    fn dither_makes_the_quantiser_unbiased_at_low_level() {
        const CODES: f32 = 100.3; // 刻意落在两个码字之间
        let want = CODES / 32767.0;
        let dc = vec![1.0f32; SR as usize]; // 1 s，够均值收敛
        // 对照：直接乘、不掺 dither，就是模块头警告的那条路。
        let plain: Vec<f32> = dc.iter().map(|x| x * want).collect();
        let plain_back = decode_pcm(&encode_pcm(&plain, WireDepth::S16), WireDepth::S16);
        let lsb = 1.0f32 / 32767.0;
        let plain_bias = (mean(&plain_back) - want).abs() / lsb;
        assert!(
            plain_bias > 0.2,
            "对照路径没有偏差（{plain_bias} LSB），这条测试就什么都没证明 —— \
             多半是 CODES 落在了码字上"
        );
        // 生产路径。
        let mut g = SendGain::new();
        g.set_target(want);
        let mut scratch = Vec::new();
        g.apply(&dc[..960], SR, WireDepth::S16, &mut scratch); // 先把斜坡走完
        let gained = g.apply(&dc, SR, WireDepth::S16, &mut scratch).to_vec();
        let back = decode_pcm(&encode_pcm(&gained, WireDepth::S16), WireDepth::S16);
        let bias = (mean(&back) - want).abs() / lsb;
        assert!(
            bias < 0.05,
            "dither 之后量化器仍有 {bias} LSB 的直流偏差（对照路径是 {plain_bias}）：\
             拖到 {CODES} 个码字，线上并不是 {CODES} 个码字"
        );
    }

    fn mean(xs: &[f32]) -> f32 {
        (xs.iter().map(|&x| x as f64).sum::<f64>() / xs.len() as f64) as f32
    }

    /// **静音是数字静音，不是一段嘶声。** 静音随音量同步一起走（plan §16），
    /// 在兜底支路上它的形态就是增益 0；而 0 加 dither 会变成 −90 dBFS 的噪声。
    ///
    /// 注入对照：删掉 `apply` 里那条 `cur == 0.0 && target == 0.0` 的判断，
    /// 这条立刻变红。
    #[test]
    fn muting_is_digital_silence_not_a_dither_hiss() {
        let tone: Vec<f32> = gen_sine(440.0, SR, FRAME, 0.8);
        let mut g = SendGain::new();
        g.set_target(0.0);
        let y = settle(&mut g, &tone, WireDepth::S16);
        assert!(
            y.iter().all(|v| *v == 0.0),
            "静音之后线上还有 {} 的残留",
            y.iter().cloned().fold(0.0f32, |a, b| a.max(b.abs()))
        );
    }

    /// **非有限增益永远上不了线。** 值是从一个原子量穿过来的，而一个 NaN 进了
    /// 对端 JB 会经 `mixer_loop` 的求和扩散成整段静音或爆音。
    ///
    /// 注入对照：把 `set_target` 写成 `self.target = gain.clamp(0.0, 1.0);`
    ///（`f32::clamp` 对 NaN 返回 NaN），这条立刻变红。
    #[test]
    fn a_non_finite_target_can_never_reach_the_wire() {
        let tone: Vec<f32> = gen_sine(440.0, SR, FRAME, 0.8);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut g = SendGain::new();
            g.set_target(0.5);
            settle(&mut g, &tone, WireDepth::F32);
            g.set_target(bad);
            let y = settle(&mut g, &tone, WireDepth::F32);
            assert!(y.iter().all(|v| v.is_finite()), "{bad} 把非有限样本送上了线");
            assert_eq!(g.current(), 1.0, "{bad} 之后增益没有回到满幅");
        }
        // 越界值被钳住，而不是被当成非有限值扔掉。
        let mut g = SendGain::new();
        g.set_target(-0.5);
        assert_eq!(g.target, 0.0);
        g.set_target(4.0);
        assert_eq!(g.target, 1.0);
    }
}

/// 终审 §二.7 / §二.14 的**同一个**根因，以及它现在被治住了这件事。
///
/// # 病史
///
/// §二.7 把 `control-loopback` 的 FAIL 记成**采集后端**的毛病，理由是
/// r6_sysaudio.sh 自己打出来的一句
///「on an endpoint measured QUIET … so the capture itself is wrong」。
/// 实测把这个归因推翻了（30-win，2026-08-09）：
///
/// | 播放路径 | 采集后端 | 2 kHz 判定（**旧量具**） |
/// |---|---|---|
/// | `probe tone` 默认设备（`play_samples_blocking`，**无伺服**） | win-device-loopback | **107.96 dB PASS** |
/// | `probe sysaudio --self-tone`（`LivePlayback` + 播放环伺服） | win-device-loopback | **14.74 dB FAIL**（6/6） |
///
/// 同一台机器、同一个端点、同一个后端：采集是**逐样本无损**的。变红的是
/// **播放**——那一刻 `play_servo` 读数是 `corr_ppm = -500`、
/// `clamped = 1988 / updates = 1989`，即速率伺服整段贴在 [`MAX_PPM`] 钳位上，
/// 把自己播出去的音调整体弯了 500 ppm。
///
/// 那一轮**只在那一条断言上换了判据，没有推广**。代价在 §二.14 兑现：
/// 20.0 dB 的阈值坐在量具自身的噪声带（19.2–20.8 dB）里，一批「放音 + 判音」
/// 的断言在抛硬币。
///
/// # 现在
///
/// 判读换成了 Hann 主瓣带内功率（见本文件「判音的带内口径」那一段），
/// 于是本模块的主张**整个翻过来**：
///
/// > 播放伺服的合法弯速率**不再**能单独把判定打红，而且离阈值有 ≥20 dB 余量。
///
/// 三条测试分别钉：干净音调仍然干净、弯到钳位仍然过、**余量可断言**。
/// 最后一条是 §二.7 当年缺的那一步——噪声带将来漂过来时它会变红，
/// 而不是又开始抛硬币。
///
/// [`MAX_PPM`]: crate::audio::PlayServo::MAX_PPM
#[cfg(test)]
mod servo_bend_no_longer_defeats_the_tone_verdict {
    use super::*;
    use crate::audio::PlayServo;

    const SR: u32 = 48_000;
    const FREQ: f32 = 2000.0;
    /// 与 r6 step 4 同长：12 s。判据取的是各 100 ms 窗的中位数，所以窗数要够。
    const SECS: usize = 12;

    /// 判据与**最坏的合法读数**之间必须留的余量。
    ///
    /// 20 dB 不是一个凑出来的数：旧量具在钳位上的读数是 14.74 dB，也就是
    /// **比阈值低 5 dB**；要求新量具反过来高出 20 dB，等于要求这次的修法把
    /// 判读从「错边 5 dB」搬到「对边 20 dB」。低于这个数就说明修法在退化。
    const MIN_MARGIN_DB: f32 = 20.0;

    /// 本项目实际用过的探针频率。余量必须在**每一个**上都成立——
    /// 单格判读的病灶随频率线性放大（δ ∝ f·ppm），只测一个频率会漏掉高频那端。
    const PROBE_FREQS: [f32; 5] = [440.0, 1000.0, 1200.0, 2000.0, 4000.0];

    fn tone_at(freq: f32) -> Vec<f32> {
        gen_sine(freq, SR, SR as usize * SECS, 0.5)
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// 基线：没弯速率时判定必须是干净的。这一条先绿，下一条才有意义。
    ///
    /// 注入对照：把 `gen_sine` 的相位累加从 f64 改成 f32（正是它自己注释里
    /// 警告的那件事），这一条变红。
    #[test]
    fn an_unbent_tone_is_detected_with_a_huge_margin() {
        let v = verify_tone(&tone_at(FREQ), SR, FREQ);
        assert!(v.detected, "unbent {FREQ} Hz must be detected, got {v:?}");
        assert!(
            v.snr_db > 60.0,
            "unbent tone should be essentially pure, got {:.2} dB",
            v.snr_db
        );
    }

    /// **主张（与本模块旧版相反）**：把音调弯 `MAX_PPM`（伺服贴钳位时的实际
    /// 行为）**不再**足以让判定翻红。
    ///
    /// 注入对照：把 `LOBE_HALF_BINS` 从 2 改回 0（= 旧的单格判读），
    /// 这一条立刻变红（2 kHz 掉回 14.74 dB，detected=false）。
    #[test]
    fn bending_the_tone_by_the_servo_clamp_no_longer_flips_the_verdict() {
        for f in PROBE_FREQS {
            let bent = f * (1.0 - PlayServo::MAX_PPM as f32 * 1e-6);
            let v = verify_tone(&tone_at(bent), SR, f);
            assert!(
                v.detected,
                "a {} ppm bend must NOT be able to fail the verdict on its own \
                 (that was the §二.7 / §二.14 defect), but {f} Hz read {:.2} dB",
                PlayServo::MAX_PPM,
                v.snr_db
            );
        }
    }

    /// **本模块存在的理由。** 阈值与最坏合法读数之间的距离必须是一个可断言的
    /// 数，否则噪声带哪天漂过来又是抛硬币——而没有任何一条断言会先变红。
    ///
    /// 扫两个维度，缺一不可：
    ///
    /// 1. **伺服弯量** `|corr| ≤ MAX_PPM`——伺服自己声明的合法范围。扫整条而
    ///    不是只扫端点，才挡得住「换了个插值核，中间某处塌了一块」。
    /// 2. **探针频率相对格心的位置 δ**。格宽 10 Hz，而 `--freq` 是个自由参数，
    ///    没有任何一处要求它是 10 的倍数。**旧量具垮掉的根因就是默认 δ = 0**，
    ///    所以守卫不扫 δ 就等于没守到点子上（`LOBE_HALF_BINS` 第一版写成 2、
    ///    在 4 kHz 上只剩 19.61 dB 余量，就是漏了这一维的直接后果）。
    ///
    /// 注入对照（两个方向都验过，见报告）：
    /// - 把 `LOBE_HALF_BINS` 改成 0（= 旧的单格判读）⇒ 变红；
    /// - 把 `PlayServo::MAX_PPM` 从 500 抬到 5000 ⇒ 变红（量具跟不上新钳位）。
    #[test]
    fn the_threshold_keeps_its_margin_against_every_legal_servo_bend() {
        let mut worst = (f32::INFINITY, 0.0f32, 0.0f64);
        for f0 in PROBE_FREQS {
            // δ：把标称频率在一格（TONE_BIN_HZ）内平移，覆盖任意的格心相对位置。
            for d in 0..5 {
                let f = f0 + TONE_BIN_HZ as f32 * (d as f32 / 5.0);
                // 11 个点铺满 [−MAX_PPM, +MAX_PPM]。
                for i in 0..=10 {
                    let ppm = -PlayServo::MAX_PPM + PlayServo::MAX_PPM * 2.0 * (i as f64 / 10.0);
                    let bent = f * (1.0 + ppm as f32 * 1e-6);
                    let v = verify_tone(&tone_at(bent), SR, f);
                    if v.snr_db < worst.0 {
                        worst = (v.snr_db, f, ppm);
                    }
                }
            }
        }
        let margin = worst.0 - TONE_DETECT_DB;
        assert!(
            margin >= MIN_MARGIN_DB,
            "the {TONE_DETECT_DB} dB verdict has only {margin:.2} dB of margin \
             against a legal servo bend (worst: {:.2} dB at {} Hz, {:.0} ppm). \
             Below {MIN_MARGIN_DB} dB the instrument is back inside its own \
             noise band and every play-then-verify assertion is a coin flip \
             again — this is exactly the §二.14 failure. Do NOT fix this by \
             moving TONE_DETECT_DB: fix the instrument.",
            worst.0,
            worst.1,
            worst.2
        );
    }

    /// 放宽通带**不是白拿的**：判读现在容忍 ±3 格，所以「音调确实不在」这个
    /// 方向必须重新钉一次，否则这次修法就是拿假阳性换假阴性。
    ///
    /// 三条：真静音、错频率（探针实际用的那几个两两组合）、白噪声，
    /// 全部必须**远低于**判据。
    ///
    /// 注入对照：把 `LOBE_HALF_BINS` 抬到 40（≈ ±400 Hz 的通带），
    /// 1000 Hz 当 1200 Hz 判会变成 detected ⇒ 这一条变红。
    #[test]
    fn widening_the_band_did_not_cost_the_rejection_side() {
        let silence = vec![0.0f32; SR as usize * 3];
        let v = verify_tone(&silence, SR, FREQ);
        assert!(!v.detected, "digital silence read as a tone: {v:?}");

        for want in PROBE_FREQS {
            for actual in PROBE_FREQS {
                if (actual - want).abs() < 1.0 {
                    continue;
                }
                let v = verify_tone(&tone_at(actual), SR, want);
                assert!(
                    !v.detected,
                    "a {actual} Hz tone was accepted as {want} Hz at {:.2} dB — \
                     the passband is too wide",
                    v.snr_db
                );
                assert!(
                    v.snr_db < TONE_DETECT_DB - MIN_MARGIN_DB,
                    "a {actual} Hz tone read {:.2} dB when asked for {want} Hz; \
                     that is inside {MIN_MARGIN_DB} dB of the verdict, i.e. the \
                     rejection side is now the coin flip",
                    v.snr_db
                );
            }
        }

        // 白噪声：功率铺满全谱，带内只该拿到 (2·LOBE_HALF_BINS+1) 格的份额。
        let mut noise = vec![0.0f32; SR as usize * 3];
        let mut r = 0x2545_F491_4F6C_DD1Du64;
        for n in noise.iter_mut() {
            r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *n = ((r >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.5;
        }
        let v = verify_tone(&noise, SR, FREQ);
        assert!(
            v.snr_db < TONE_DETECT_DB - MIN_MARGIN_DB,
            "white noise read {:.2} dB at {FREQ} Hz — too close to the verdict",
            v.snr_db
        );
    }

    /// 而 rms 看不出任何异常——这正是 r6 的「端点是安静的，所以一定是采集坏了」
    /// 那句推理失效的地方：它拿能量当证据，而弯速率不动能量。
    ///
    /// 两条断言缺一不可：
    /// 1. 弯前弯后的 rms 相等 —— 「能量证据是瞎的」这个主张本身；
    /// 2. **两者都等于解析值 `amp/√2`** —— 只比 1 会让任何**共模**改动（给
    ///    生成器加窗、整体改幅度）从两边同时消掉，测试照绿。r6 的
    ///    `TONE_RMS = 0.5/sqrt(2)` 与 `LEAK_MIN` 两个常数正是钉在这个绝对值上，
    ///    所以它必须被断言，不能只断言差。
    ///
    /// 注入对照：给 `gen_sine` 加一个 Hann 窗（一个看起来像「减少频谱泄漏」的
    /// 改进），第 2 条立刻变红；只有第 1 条时它是绿的。
    #[test]
    fn the_bend_is_invisible_in_rms_which_is_what_the_regress_used_as_evidence() {
        let clean = rms(&tone_at(FREQ));
        let bent = rms(&tone_at(FREQ * (1.0 - PlayServo::MAX_PPM as f32 * 1e-6)));
        assert!(
            (clean - bent).abs() < 1e-4,
            "rms must not move ({clean:.6} vs {bent:.6}): the energy evidence \
             cannot tell a bent tone from a clean one"
        );
        // r6_sysaudio.sh 的 TONE_RMS：amp 0.5 的纯音 = 0.5/sqrt(2)。
        let want = 0.5 / 2f64.sqrt();
        for (label, got) in [("clean", clean), ("bent", bent)] {
            assert!(
                (got - want).abs() < 1e-3,
                "{label} rms {got:.6} != the analytic full-scale {want:.6}; \
                 r6's TONE_RMS/LEAK_MIN thresholds are pinned to this value"
            );
        }
    }
}
