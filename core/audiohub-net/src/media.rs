//! Media plane building blocks (spec-m4a §3): per-stream AEAD, jitter buffer
//! with PLC, frame sources, deterministic loss injection, AUTO quality ladder.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, AeadInPlace, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use audiohub_core::audio::{AudioRx, LiveCapture};
use audiohub_core::dsp::LinearResampler;
use audiohub_core::latency::{DropMode, SourceDepths, StageDepth, StageId, NO_DEPTHS};
use audiohub_core::sysaudio::{self, BackendInfo, SysAudioCapture};

use crate::packet::{Header, HEADER_LEN};

pub const AEAD_TAG_LEN: usize = 16;

/// HKDF info prefix for per-stream media keys; the stream_id is appended LE.
const STREAM_KEY_INFO: &[u8] = b"audiohub-stream-v1";

/// Per-stream media AEAD. nonce = 4B stream_id LE ‖ 4B seq LE ‖ 4B zero,
/// AAD = the 40-byte wire header. `codec` in the header still names the
/// plaintext encoding; the wire payload is ciphertext.
pub struct MediaCrypto {
    cipher: ChaCha20Poly1305,
}

fn media_nonce(stream_id: u32, seq: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&stream_id.to_le_bytes());
    n[4..8].copy_from_slice(&seq.to_le_bytes());
    n
}

impl MediaCrypto {
    pub fn new(key: &[u8; 32]) -> Self {
        MediaCrypto {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// Per-stream key: HKDF-SHA256(ikm = media_key, salt, info =
    /// "audiohub-stream-v1" ‖ stream_id LE). Real streams MUST use this
    /// instead of `new`: the nonce is (peer-chosen stream_id ‖ seq), so under
    /// one connection-wide key a peer that closes and reopens the same
    /// stream_id restarts seq at 0 and repeats the exact keystream. Binding
    /// the key to the opener's fresh per-stream salt keeps the keystream
    /// distinct even when stream_id and seq both repeat.
    pub fn new_for_stream(media_key: &[u8; 32], stream_id: u32, salt: &[u8]) -> MediaCrypto {
        let mut info = Vec::with_capacity(STREAM_KEY_INFO.len() + 4);
        info.extend_from_slice(STREAM_KEY_INFO);
        info.extend_from_slice(&stream_id.to_le_bytes());
        let hk = Hkdf::<Sha256>::new(Some(salt), media_key);
        let mut stream_key = [0u8; 32];
        hk.expand(&info, &mut stream_key).expect("hkdf expand 32B");
        let mc = MediaCrypto {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&stream_key)),
        };
        stream_key.zeroize();
        mc
    }

    /// Builds the full datagram: 40B header (payload_len set to ciphertext
    /// length) followed by the ciphertext.
    pub fn seal(&self, header: &Header, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut datagram = Vec::with_capacity(HEADER_LEN + plaintext.len() + AEAD_TAG_LEN);
        self.seal_into(header, plaintext, &mut datagram)?;
        Ok(datagram)
    }

    /// [`MediaCrypto::seal`] 的**零分配**形态：清空 `out` 并就地封包。
    ///
    /// # 为什么需要它
    ///
    /// `tx_loop` 每 tick 每流封一个包。原来的 `seal` 每次做**两次**堆分配
    /// （`Header::encode` 一次、AEAD 的 `encrypt` 一次），而那条线程上的每一次
    /// `malloc` 都可能撞上分配器的 magazine refill 并陪它等一把锁
    /// —— `docs/spec-latency-floor.md` §9.3 手段 J1 点名的第 3 项。
    ///
    /// # 线格式与 `seal` **逐字节相同**
    ///
    /// `Aead::encrypt` 产出的是 `密文 ‖ 标签`；`encrypt_in_place_detached` 就地
    /// 产出同一段密文并把标签单独返回，接上去即得同一串字节。`tests` 里有一条
    /// 直接对拍的断言 —— 这不是「看起来一样」，两份实现就是两份线格式。
    pub fn seal_into(&self, header: &Header, plaintext: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let mut h = header.clone();
        h.payload_len = (plaintext.len() + AEAD_TAG_LEN) as u32;
        out.clear();
        out.reserve(HEADER_LEN + plaintext.len() + AEAD_TAG_LEN);
        h.encode_into(plaintext, out);
        let nonce = media_nonce(h.stream_id, h.seq);
        // AAD 是那 40 字节头，而它此刻就在 `out` 的前面 —— `split_at_mut` 让
        // 「以头为 AAD 加密其后的载荷」不必再拷一份头出来。
        let (aad, msg) = out.split_at_mut(HEADER_LEN);
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), aad, msg)
            .map_err(|_| anyhow!("media encrypt failed"))?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    /// Parse + authenticate + decrypt one datagram into (header, plaintext).
    pub fn open(&self, datagram: &[u8]) -> Result<(Header, Vec<u8>)> {
        let (h, ct) = Header::parse(datagram).map_err(|e| anyhow!("bad media packet: {e}"))?;
        let nonce = media_nonce(h.stream_id, h.seq);
        let pt = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: ct, aad: &datagram[..HEADER_LEN] },
            )
            .map_err(|_| anyhow!("media decrypt failed (tampered or wrong key)"))?;
        Ok((h, pt))
    }
}

/// 抖动缓冲的水位整定。**全部可通过环境变量覆盖**，Verify 阶段实测寻优时
/// 不必重编（见 [`JbTuning::from_env`]）。单位一律是**帧**（1 帧 = 10 ms）
/// 或**tick**（`pop()` 一次 = 一个 10 ms tick）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JbTuning {
    /// 目标深度的下限。`update_target` 的公式在干净局域网上恒给 1，被它抬到 2。
    pub min_target: u32,
    /// 目标深度的上限，同时也是惩罚项能把水位推到的最高处。
    pub max_target: u32,
    /// 死区宽度（帧）。开始收敛的线是 `target + 1 + slack`，一次收敛吃掉两帧，
    /// 于是稳态**预弹出**深度落在 `[target, target + slack]`。
    /// `slack = 1` ⇒ 稳态 3 帧 ≈ 30 ms（恰好是 AES67 的**强制**下限
    /// 3×packet time），上报值（采样点落在弹出前后之间）≈ 25 ms。
    pub slack: u32,
    /// 两次平滑收敛之间的最小 tick 数。100 tick = 1 s ⇒ 每秒最多吐 10 ms
    /// ⇒ 时间压缩率上限 **ρ = 1 %**，与 `spec-latency-trim.md` §4.3 同一个数，
    /// 低于无参照节奏 JND（2–4 %）2–4 倍。
    pub accel_interval_ticks: u64,
    /// 应急线：`contiguous > target + hard_slack` 时无视限速，每 tick 吐一帧。
    /// 默认 6 —— **就是改动前那条 `target + 6` 硬线**，于是最坏情形不可能比
    /// 改动前更差，只是把硬切换成了交叉淡化。
    pub hard_slack: u32,
    /// `frames.len()` 的绝对上界（内存保护，**与延迟控制分离**）。
    /// 见 [`JitterBuffer::pop`] 里「控制量 ≠ 被控量」那一段。
    pub max_frames: u32,
    /// 一次欠载把目标抬高多少帧。
    pub underrun_step: u32,
    /// 惩罚项上限（帧）。
    pub extra_max: u32,
    /// 无欠载时每多少 tick 把惩罚项衰减 1 帧。30 000 tick = **5 分钟**。
    ///
    /// 慢是故意的，而且这个数直接定义了稳态欠载率的上界：回路停在
    /// 「欠载稀于 1 次 / `extra_decay_ticks`」的深度上（否则目标单调上涨）。
    /// 5 分钟 ⇒ **≤ 0.2 次/分钟**，对照实测基线 4 次/分钟是 20 倍改善。
    /// 先例：PulseAudio `module-loopback` 每小时宽恕一次、NetEq 的
    /// forgetting histogram 时间常数同样远长于单次事件。
    pub extra_decay_ticks: u64,
    /// 交叉淡化长度（样本）。192 = 4 ms，与 `trim::X` 同值同依据。
    pub xfade: usize,
    /// **抵消保护**：两帧尾部互相关低于它就先不拼，等下一 tick 的素材。
    ///
    /// 帧粒度的加速把 `tau` 钉死在一整帧上，**没有 WSOLA 那样挑相位点的自由度**
    /// （`trim::search_tau` 能在 ±8 ms 里找同相位点，我们不能——挑了就会剩下
    /// 半帧残料，而 `pop()` 的契约是整帧进整帧出）。于是当两帧恰好**反相**时
    /// （纯音且 `f × 10 ms` 是半整数，比如 750 Hz），任何凸组合
    /// `g_a·a + g_b·(−a)` 都会在淡化区中点穿过零 —— 等功率律也救不了，
    /// 那是数学事实不是整定问题。
    ///
    /// 阈值 −0.2 的来历：相关系数 ρ 下等功率拼接在中点的功率是 `(1+ρ)·E`，
    /// ρ = −0.2 ⇒ −0.97 dB，ρ = −0.3 ⇒ −1.5 dB（正好撞上 C3 判据线）。
    /// 所以只挡真正会抵消的那一侧；ρ ≈ 0 的宽带素材（真实节目内容的常态）
    /// 等功率律本来就不塌，不该白等。
    pub ncc_floor: f32,
    /// 等素材的死线（tick）。等到就强拼——**不许因为等不到好点位就永远
    /// 不收敛**。500 tick = 5 s，与 `trim::ESCALATE_STEP_US` 同值同理由：
    /// 「等待的代价只是水位继续挂在高位，而 5 s 足以覆盖语音的句间停顿与音乐的
    /// 绝大多数乐句间隙。等待很便宜，别急。」
    ///
    /// 死线**必须明显长于**限速周期（`accel_interval_ticks`，1 s），否则保护
    /// 只是把每一次收敛推迟几百毫秒，一次都挡不掉 —— 那样它看起来在工作，
    /// 实际等于不存在。
    ///
    /// > **不要**用「把 b 整体反相再拼」去消灭这个凹陷。单流听起来确实没问题
    /// > （绝对极性不可闻），但 `mixer_loop` 会把多路流**相加**，一路被翻了极性
    /// > 就会和相关的另一路互相抵消。这条写在这里防止后人「优化」。
    pub ncc_retry_ticks: u32,
}

impl JbTuning {
    /// # `min_target = 4` / `extra_max = 2` 是**实测**定的，不是推出来的
    ///
    /// 2026-08-04 mac→30-win 实链路，四组各 6–12 min（`docs/progress.md` 有全表）。
    /// 每组都是干净千兆局域网：`lost = 0`、RFC 3550 抖动 p50 0.1 ms / p95 0.2 ms。
    ///
    /// | 整定 | JB 深度 | 欠载/min | PLC/min | `sum_ms` p50 |
    /// |---|---|---|---|---|
    /// | 改动前（天花板 `target+6`） | 60 ms | 1.51 | 4.35 | 130.9 |
    /// | `min_target=2, extra_max=0`（纯收敛） | **20 ms** | **3.75** | 9.42 | **82.2** |
    /// | `min_target=2, extra_max=8`（惩罚放开） | 90 ms | 0.33 | 0.50 | 148.1 |
    /// | **`min_target=4, extra_max=2`（本默认）** | **50 ms** | **0.18** | **0.36** | **100.1** |
    ///
    /// 三条必须一起读：
    ///
    /// 1. **深度确实在买抗欠载能力，不是白占的延迟。** 欠载率随深度单调下降，
    ///    20/60/90 ms 三点连成一条 `∝ 1/D` 的曲线。所以「天花板成了地板、白多
    ///    50 ms」这个判断只对了一半：天花板确实没在收敛，但收敛到 `update_target`
    ///    算出来的 2 帧**会把延迟问题换成欠载问题**（3.75 次/min，比改动前差 2.5 倍）。
    ///
    /// 2. **为什么 `update_target` 会算出 2。** 喂给它的是 RFC 3550 的一阶差分
    ///    抖动（EWMA），实测 p95 0.2 ms —— 而同一条链路真实需要 40–50 ms 的
    ///    排队深度。EWMA **看不见突发**：包成串到、成串不到时，一阶差分被平均掉，
    ///    统计量趋近 0。所以 `min_target` 不能信那个估计量，必须由实测钉住。
    ///    真正的修法是换成 NetEq 式的 relative-delay 分布取分位数（见下方 `R`）。
    ///
    /// 3. **`extra_max = 2` 的上界是从「不许比被替换的机制更深」推出来的**：
    ///    改动前的有效天花板是 `target + hard_slack = 2 + 6 = 8` 帧。
    ///    本默认的最深处是 `min_target + extra_max = 4 + 2 = 6` 帧 < 8。
    ///    原来的 `extra_max = 8` 会让有效目标摸到 `2 + 8 = 10` 帧 = 100 ms，
    ///    **比它要替换的天花板还高**——实测就是这么发生的（第三行 90 ms）。
    ///
    /// # 顺带把 `R` 测出来了
    ///
    /// 本类型的文档此前写着「`R` 的真值尚未实测（只有量级估计）」。由上表
    /// `R ≈ D / t_between_underruns` 反解：60 ms/39.7 s、20 ms/16.0 s、90 ms/181.8 s
    /// ⇒ **`R ≈ 500–1500 ppm`**，本默认那一组约 150 ppm。
    ///
    /// **它比晶振公差（±50–100 ppm）高一个数量级**，所以主导项不是两个晶振的
    /// 速率差，而是发送侧 `tx_loop` 的 DLL 把 UDP 发包速率也一起弯了（`corr`
    /// 摆幅 ±500 ppm），而接收侧**没有任何速率匹配**去吸收它。
    ///
    /// 推论（下一轮的方向，本轮没做）：要把 JB 降到 20 ms 且欠载稀于 1 次/5 min，
    /// 需要 `D ≥ R × 300 s`；在 `R = 500 ppm` 下那是 150 ms，比现状还差。
    /// **所以先修 `R`（接收侧自适应重采样，或发送侧按固定时钟发包），再谈削 JB。**
    /// 在 `R` 修好之前，任何进一步下调 `min_target` 都是拿断续换数字。
    ///
    /// # 2026-08-04 追加：发送端停顿尾**已实测**，平台地板是 3 帧不是 4
    ///
    /// 上面整张表都是从**欠载率**反推停顿尾的，而反推依赖「一次停顿恰好换一次
    /// 欠载」这个从未验证的假设。本轮直接测了那条尾：在 mac 上跑一个与 `tx_loop`
    /// **线程配置（QoS `USER_INTERACTIVE`）与等待机制（`recv_timeout` 到截止期）
    /// 逐字同构**的独立探针，10 分钟 / 60 000 个 tick，不碰正在服务用户音频的
    /// daemon：
    ///
    /// ```text
    /// 迟到 <1 ms          59 965 tick   99.9417%
    /// 迟到 ≥10 ms              13 tick   P = 2.2e-4  ⇒ 1.30 次/分
    /// 迟到 ≥20 ms               3 tick   P = 5.0e-5  ⇒ 0.30 次/分
    /// 迟到 ≥30 ms               0 tick   P = 0                        ← 尾在这里断掉
    /// 最大迟到 27.39 ms
    /// ```
    ///
    /// 两条结论，方向相反，必须一起读：
    ///
    /// 1. **平台本身在 30 ms 处就已经零停顿了**（60 000 个 tick 里一个都没有）。
    ///    也就是说 `min_target = 4`（40 ms）里**至少有一帧不是平台逼出来的**。
    ///
    /// 2. ~~但 daemon 自己的尾比裸探针肥 12.5 倍~~ —— **这条已被实测推翻，见下。**
    ///
    /// # 2026-08-04 上机实测 `sched_late.tx`：结论是 `min_target = 4` **保留**
    ///
    /// 上面那条「daemon 的尾肥 12.5 倍、根源是 `tx_loop` 自己挂的 `sendto()` /
    /// `dlog!` / 每 tick `Vec` 分配 / `build_source` 打开 CoreAudio 设备」的猜测
    /// **是错的**。`LateCell` 埋进生产 daemon 跑起来之后（mac ↔ 30-win 真实会话）：
    ///
    /// ```text
    /// 机器静置的 15 分钟窗口   125 747 tick   最大迟到 4.45 ms   ≥5 ms 的 tick：0
    /// 受控静置 300 s 复核        30 012 tick   ≥2 ms 的 tick：0
    /// 我在同一台机器上跑分析脚本的 112 s   11 138 tick
    ///                            ≥10 ms 4 个，其中一个 40.35 ms
    /// ```
    ///
    /// 也就是说：**`tx_loop` 自己是干净的，那条尾是从机器上别的负载导入的。**
    /// 静置时它几乎不产生迟到（12.5 万 tick 里一个 ≥5 ms 都没有）；一旦同机有
    /// 别的进程在跑，尾立刻出现。判据是「谁在跑」，不是「`tx_loop` 里挂了什么」。
    ///
    /// **由此得到的是保留 `min_target = 4`，不是下调到 3。** 发送端停顿 Δ ms ⇒
    /// 对端 JB 净排空 Δ/10 帧（不变量：对端 `mixer_loop` 照常每 10 ms pop）。
    /// 实测在**轻负载**下已经出现单次 **40.35 ms** 的停顿——那恰好排空 4 帧。
    /// `min_target = 4`（40 ms）正好扛住它，`min_target = 3`（30 ms）必然欠载。
    /// 真实用户的机器上跑着别的东西，判据必须按**有负载**取，不能按静置取。
    ///
    /// 附带纠正一处口径错误：整定表那 3.75 次/分是 `min_target=2` 的实验值，
    /// 而**不能**再拿来反推停顿尾——反推依赖「一次停顿恰好换一次欠载」，
    /// 现在有了直接测量，反推该退休了。
    ///
    /// # 2026-08-04 第三轮：`min_target = 3` 上机跑过 36 min，**已回退**
    ///
    /// 上面所有推断都还是统计的。这一轮把它变成了**确定性**的：在 mac→30-win
    /// 的真实会话上用 `SIGSTOP`/`SIGCONT` 给 `audiohubd` 注入精确时长的停顿，
    /// 逐级抬高幅度，看对端 `jb_underruns` 什么时候动。
    ///
    /// ```text
    /// Δ(ms)  20   30      40      50   60      70
    /// mt=4   0/2  0/2     1/2 ✗   0/2  1/2 ✗   2/2 ✗      ← 阈值落在 40
    /// mt=3   0/2  1/2 ✗                                    ← 阈值落到 30
    /// ```
    ///
    /// 12 次注入全部落在同一条规律上（含惩罚项抬高目标之后的复核）：
    ///
    /// > **欠载 ⟺ 连续收不到帧的 tick 数 ≥ `target_effective()`**，
    /// > 即发送端停顿 **Δ ≥ target × 10 ms**。
    ///
    /// 这条已钉成单测 [`water_level_tests::the_stall_tolerance_is_exactly_target_effective_frames`]。
    /// 所以 `min_target` 省下的**每一帧都恰好是一次 10 ms 停顿的抗性**——
    /// 同一个 30 ms 停顿在 `min_target = 4` 下是绿的、在 `min_target = 3` 下是红的。
    ///
    /// 随后按「只降一帧」把对端整定到 `min_target = 3` 跑了 36 min（其中 15.7 min
    /// 满载编译负载），与 `min_target = 4` 的同链路基线对照：
    ///
    /// | | `mt=4` 基线 | `mt=3` | |
    /// |---|---|---|---|
    /// | `jitter_buf` 均值 | 34.3 ms | **29.7 ms** | −4.6 ms |
    /// | `sum_ms` p50（对端自读） | 86.7 ms | **86.4 ms** | **−0.4 ms** |
    /// | `jb_underruns` | 0.101/min | **0.167/min** | **×1.65** |
    /// | `jb_plc` | 0.332/min | **0.667/min** | ×2.0 |
    /// | `jb_silence` | 0.116/min | **1.334/min** | ×11.5 |
    /// | `jitter_buf` 最大值 | 40 ms | **110 ms** | 惩罚项把尾拉长了 |
    /// | `lost` | 0 | 0 | — |
    ///
    /// **收益进了 JB 那一级，却没有进 `sum_ms`**：省下的 4.6 ms 被下游
    /// `play_ring` 与惩罚项爬升吃掉（`target` 分布 3:84.8% / 4:13.9% / 5:1.3%）。
    /// 也就是说 `min_target` 一降，回路就靠**制造欠载**把水位学回去——省下的延迟
    /// 是借来的，利息用断续付。**据此回退到 4，不再下调。**
    ///
    /// 顺带纠正 `docs/spec-latency-floor.md` §9.1 的一个工作点误读：那里写的
    /// `jitter_buf = 50 ms` 是**惩罚项处于激活态**时的快照。链路干净、惩罚项
    /// 归零时两侧同时实测（mac 中继 145 点 / 对端自读 249 点，同一窗口）都是
    /// **p50 30 ms、均值 34.3 ms、只在 3↔4 帧之间摆**——上报的是弹出前深度，
    /// 弹出后是它减一。所以 J1「50 → 15 = −35 ms」的前提里，有 20 ms 从一开始
    /// 就不存在。
    pub const DEFAULT: JbTuning = JbTuning {
        min_target: 4,
        max_target: 12,
        slack: 1,
        accel_interval_ticks: 100,
        hard_slack: 6,
        max_frames: 24,
        underrun_step: 1,
        extra_max: 2,
        extra_decay_ticks: 30_000,
        xfade: 192,
        ncc_floor: -0.2,
        ncc_retry_ticks: 500,
    };

    /// Tier 1/2 profile: the same loop, given room to reach a depth a TCP
    /// retransmission cannot punch through (`docs/design-m8-fallback.md` §3.3).
    ///
    /// # The one number this profile exists for: `max_target = 40`
    ///
    /// A TCP retransmission timeout is **at least 200 ms** (Linux clamps RTO to
    /// 200 ms, Windows to 300 ms). [`JbTuning::DEFAULT`]'s `max_target = 12` is
    /// 120 ms. So under `DEFAULT` *every single retransmission* drains the
    /// buffer dry — not as a tail risk, as arithmetic. Measured on the real
    /// mac ↔ 30-win tier 1 link (2026-08-08, 150 s A/B against the same pair of
    /// counters on tier 0): **+4 underruns and +13 jitter-buffer drops on tier
    /// 1, +0 and +0 on tier 0**, i.e. ~1.6 underruns/min against a flat
    /// baseline, with `stale_dropped`, `dropped` and `queued` all zero the
    /// whole time — so the loss was not the send-side gate, it was delivery
    /// timing at the receiver, which is exactly what depth buys.
    ///
    /// 400 ms covers a Windows RTO (300 ms) with a margin; it is a magnitude
    /// derived from the RTO floors, not a measured optimum, and §8 item 3 of
    /// the design says so.
    ///
    /// # `extra_max = 36`: without it the ceiling above is decoration
    ///
    /// Depth is not set by these numbers, it is *found* by the underrun
    /// penalty loop — one frame per underrun, one frame back per five clean
    /// minutes, settling wherever underruns become rarer than that (see
    /// [`JitterBuffer`]'s "不需要事先知道 R" argument, which is unchanged
    /// here). `target_effective` is `clamp(target + extra, min, max)`, so
    /// `extra_max` is what limits how far the loop may climb: `DEFAULT`'s 2
    /// would pin tier 1 at `min_target + 2` = 60 ms and `max_target = 40` would
    /// never be reachable at all. 36 = `max_target − min_target` makes the
    /// ceiling the only clamp, which is the point of raising it.
    ///
    /// **`underrun_step` is deliberately left at 1.** Faster convergence is
    /// tempting (at ~1.6 underruns/min the loop needs tens of minutes to walk
    /// 36 frames), but the design says the penalty loop itself is unchanged and
    /// the honest lever for "deep enough immediately" is `min_target`, which is
    /// a starting depth rather than a learning rate.
    ///
    /// # What is *not* raised, and why that is a decision
    ///
    /// `min_target` stays at 4. It was measured on the tier 0 link (see the
    /// long note on [`JbTuning::DEFAULT`]) and nothing about TCP makes the
    /// *floor* wrong; raising it would add its whole value to the steady-state
    /// latency of every tier 1 link, including the good tunnels where the
    /// penalty loop never needs to climb at all. Overridable for measurement
    /// via `AUDIOHUB_JB_DEGRADED_MIN_TARGET` (see [`JbTuning::from_env`]).
    pub const DEGRADED: JbTuning = JbTuning {
        min_target: 4,
        max_target: 40,
        extra_max: 36,
        // The memory ceiling has to clear the latency-control line, exactly as
        // in `from_env`'s tail: `max_target + hard_slack + 1` = 47.
        max_frames: 48,
        ..JbTuning::DEFAULT
    };

    /// `AUDIOHUB_JB_*` 覆盖 [`JbTuning::DEFAULT`]。进程内只读一次（`env::var`
    /// 会拿进程级环境锁）。
    ///
    /// | 变量 | 默认 |
    /// |---|---|
    /// | `AUDIOHUB_JB_MIN_TARGET` | 4 |
    /// | `AUDIOHUB_JB_MAX_TARGET` | 12 |
    /// | `AUDIOHUB_JB_SLACK` | 1 |
    /// | `AUDIOHUB_JB_ACCEL_INTERVAL_TICKS` | 100（= ρ 1 %） |
    /// | `AUDIOHUB_JB_HARD_SLACK` | 6 |
    /// | `AUDIOHUB_JB_MAX_FRAMES` | 24 |
    /// | `AUDIOHUB_JB_UNDERRUN_STEP` | 1 |
    /// | `AUDIOHUB_JB_EXTRA_MAX` | 2 |
    /// | `AUDIOHUB_JB_EXTRA_DECAY_TICKS` | 30000（= 5 min） |
    /// | `AUDIOHUB_JB_XFADE` | 192（= 4 ms） |
    /// | `AUDIOHUB_JB_NCC_FLOOR` | -0.2 |
    /// | `AUDIOHUB_JB_NCC_RETRY_TICKS` | 500（= 5 s） |
    ///
    /// [`JbTuning::degraded_from_env`] reads the **same twelve names with an
    /// `AUDIOHUB_JB_DEGRADED_` prefix** over [`JbTuning::DEGRADED`]. Two
    /// prefixes and one implementation, so a knob can never exist for one
    /// profile and quietly do nothing for the other.
    pub fn from_env() -> JbTuning {
        JbTuning::env_over(JbTuning::DEFAULT, "AUDIOHUB_JB_")
    }

    /// The tier 1/2 profile with `AUDIOHUB_JB_DEGRADED_*` applied. See
    /// [`JbTuning::from_env`] for the variable names.
    pub fn degraded_from_env() -> JbTuning {
        JbTuning::env_over(JbTuning::DEGRADED, "AUDIOHUB_JB_DEGRADED_")
    }

    fn env_over(d: JbTuning, prefix: &str) -> JbTuning {
        let u32v = |key: &str, dv: u32| -> u32 {
            std::env::var(format!("{prefix}{key}"))
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(dv)
        };
        let u64v = |key: &str, dv: u64| -> u64 {
            std::env::var(format!("{prefix}{key}"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(dv)
        };
        let mut t = JbTuning {
            min_target: u32v("MIN_TARGET", d.min_target).max(1),
            max_target: u32v("MAX_TARGET", d.max_target).max(1),
            slack: u32v("SLACK", d.slack),
            accel_interval_ticks: u64v("ACCEL_INTERVAL_TICKS", d.accel_interval_ticks),
            hard_slack: u32v("HARD_SLACK", d.hard_slack),
            max_frames: u32v("MAX_FRAMES", d.max_frames).max(2),
            underrun_step: u32v("UNDERRUN_STEP", d.underrun_step),
            extra_max: u32v("EXTRA_MAX", d.extra_max),
            extra_decay_ticks: u64v("EXTRA_DECAY_TICKS", d.extra_decay_ticks).max(1),
            xfade: u32v("XFADE", d.xfade as u32) as usize,
            ncc_floor: std::env::var(format!("{prefix}NCC_FLOOR"))
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .unwrap_or(d.ncc_floor)
                .clamp(-1.0, 1.0),
            ncc_retry_ticks: u32v("NCC_RETRY_TICKS", d.ncc_retry_ticks),
        };
        t.max_target = t.max_target.max(t.min_target);
        // 内存上界永远不许低于延迟控制线，否则内存保护会去抢延迟控制的活，
        // 而它是**硬切**且用 `len()` 判据（会误删洞后面的真音频）。
        t.max_frames = t.max_frames.max(t.max_target + t.hard_slack + 1);
        t
    }

    fn cached() -> JbTuning {
        static CACHE: std::sync::OnceLock<JbTuning> = std::sync::OnceLock::new();
        *CACHE.get_or_init(JbTuning::from_env)
    }
}

/// The invariant `from_env` enforces at runtime, enforced at **compile** time
/// for the one profile no environment variable has to pass through.
///
/// Below this line the hard `len()` trim starts doing the latency controller's
/// job — and it cuts without a crossfade, by `len()`, so it deletes real audio
/// that arrived after a hole.
const _: () = assert!(
    JbTuning::DEGRADED.max_frames
        >= JbTuning::DEGRADED.max_target + JbTuning::DEGRADED.hard_slack + 1,
    "JbTuning::DEGRADED's memory ceiling sits below its latency-control line"
);

/// Raising `max_target` buys nothing if the penalty loop cannot climb to it:
/// `target_effective` is `clamp(target + extra, min, max)`, so the reachable
/// depth is `min_target + extra_max` and *that* would be the real ceiling.
const _: () = assert!(
    JbTuning::DEGRADED.min_target + JbTuning::DEGRADED.extra_max >= JbTuning::DEGRADED.max_target,
    "the underrun penalty cannot reach JbTuning::DEGRADED.max_target"
);

/// One frame, in milliseconds. `audiohubd`'s `engine::FRAME_MS` is the same
/// number one layer up and asserts equality with this one at compile time —
/// this crate cannot reach that constant, and a second literal 10 is how the
/// two would drift.
pub const FRAME_MS: u64 = 10;

/// Per-receive-stream jitter buffer holding 48k mono f32 frames (one frame =
/// one 10ms tick). pop() once per tick after warm-up; missing frames get PLC
/// (repeat last frame decayed 30% per repeat), silence after 5 consecutive
/// misses.
///
/// # 水位是**受控量**，不是「撞到上限的自由积分器」
///
/// 改动前这里只有一条单边硬限幅：`while frames.len() > target + 6 { 丢最老 }`。
/// 它有三个后果，实测（2026-08-03，mac↔win，零丢包的千兆局域网）全部命中：
///
/// 1. **天花板成了地板。** `target` 由 `update_target` 从实测抖动算出 = 2 帧，
///    而水位由那条 `+6` 唯一决定，稳态稳定在 7–8 帧（70–80 ms）。
///    `target` 这个变量对实际水位**没有任何影响力**。
/// 2. **没有设定点就没有回复力。** 同一分钟里，零丢包的链路上，缓冲既撞了
///    天花板（5 次丢最旧）又撞了地板（4 次欠载）——两端有硬墙、中间没有
///    回复力的自由积分器的标准征象。
/// 3. **追赶是无淡化的硬切**（每帧 10 ms 直接扔），每次咔哒一下。
///
/// 现在的结构（对齐 WebRTC NetEq / PipeWire / AES67 的共同做法：**收敛到**一个
/// 目标，而不是**撞到**一个上限）：
///
/// | 机制 | 作用 | 守什么 |
/// |---|---|---|
/// | 平滑收敛（[`JitterBuffer::accelerate`]） | `contiguous > target+1+slack` ⇒ 吃两帧、交叉淡化拼成一帧 | 深度真的落到 `target`，且**不是硬切** |
/// | 限速（`accel_interval_ticks`） | 每秒最多一次 ⇒ ρ = 1 % | 时间压缩率低于节奏 JND |
/// | 应急线（`hard_slack`，默认 6） | 越过就无视限速逐 tick 吐 | 最坏情形不比改动前差 |
/// | **欠载惩罚**（`extra`） | 一次欠载 ⇒ 目标 +1 帧；无欠载每 60 s 退 1 帧 | **削过头会自己长回来**，见下 |
/// | 内存上界（`max_frames`） | `len()` 的绝对界，独立于延迟控制 | 有洞时不误删真音频 |
///
/// # 为什么「削到 target」不会把延迟问题换成欠载问题（构造性论证）
///
/// 无回复力的缓冲，欠载率 ∝ `R / D`（`R` = 未被校正的净速率误差，`D` = 深度），
/// 是一条直线不是一个阈值。所以「削深度 ⇒ 欠载变多」在**固定 `R`** 下确实成立，
/// 单独削这里就是拿延迟换欠载。本轮不是单独削：
///
/// - 先修掉 `halbridge::trim::expected_frames` 那个把**伺服自己的排水动作**记成
///   生产侧漏写的正反馈环。它让 mac 发送侧的 `corr` 在稳态就在 ±500 ppm 之间摆，
///   而 `corr` 同时决定 UDP 发包速率 ⇒ 那正是把本缓冲顶上天花板的扰动。
///   修后 `R` 从 ~600 ppm 塌到晶振级（消费级件 ±50 ppm、劣质件 ±100 ppm）。
/// - `D/R`：现状 80 ms / 600 ppm = 133 s；修后 30 ms / 100 ppm = **300 s**。
///   **延迟降了，抗欠载能力同时改善一倍以上。不是取舍。**
///
/// 而 `R` 的真值**尚未实测**（只有量级估计）。所以安全性不押在那个估计上，押在
/// 惩罚项的这条构造上：
///
/// > 一次欠载把目标抬 `underrun_step` 帧；无欠载时每 `extra_decay_ticks`（5 min）退 1 帧。
/// > 若稳态欠载率**高于** 1 次 / 5 min，目标就单调上涨，直到
/// > `max_target`（= 改动前天花板之上）。所以系统要么停在一个欠载稀于
/// > 1 次/60 s 的深度上，要么长回到不劣于改动前的深度。
/// > **无论 `R` 是多少，稳态欠载率都被这条回路夹住，不需要事先知道 `R`。**
///
/// 代价是「`R` 很坏时延迟自己长回来」——有界（`max_target`）且可观测
/// （`jb_target_frames` 上报的就是含惩罚的有效目标）。
pub struct JitterBuffer {
    frames: BTreeMap<u32, Vec<f32>>,
    next_seq: Option<u32>,
    /// Holding output to rebuild depth: true before the first frame ever plays
    /// and again after every underrun.
    prebuffering: bool,
    /// 由 `update_target` 从实测抖动导出的**基线**目标。
    target: u32,
    /// 欠载惩罚项（帧）。有效目标 = `clamp(target + extra, min, max)`。
    extra: u32,
    cfg: JbTuning,
    /// `pop()` 的调用次数 —— 本结构唯一的时钟。`pop()` 由 `mixer_loop` 按 10 ms
    /// 节拍调用，所以 1 tick = 10 ms。**不用 `Instant`**：那会让限速与衰减
    /// 在测试里不可控，而这两条恰恰是「削过头会不会自愈」的承重结构。
    tick: u64,
    next_decay_tick: u64,
    last_accel_tick: u64,
    /// 连续因为「素材会抵消」而推迟收敛的次数。到 `ncc_retry_ticks` 就强拼。
    ncc_defer: u32,
    frame_len: usize,
    last_frame: Vec<f32>,
    plc_run: u32,
    pub popped: u64,
    pub plc_count: u64,
    pub silence_count: u64,
    pub dropped: u64, // late arrivals + catch-up drops
    pub underruns: u64,
    /// 平滑收敛的次数与吃掉的帧数。**与 `dropped` 是包含关系**：每次收敛也计
    /// 一次 `dropped`（它的语义「catch-up drops」没变，删掉的确实是真音频），
    /// 这两个新计数器回答的是「这些 catch-up 里有多少走了交叉淡化那条平滑路径」。
    pub accel_events: u64,
    pub accel_frames: u64,
    /// 想收敛但被「素材会抵消」挡下的 tick 数。持续增长 = 对端在送一段
    /// 恰好反相的稳态纯音，此时收敛速度降到死线节律（200 ms 一次）。
    pub accel_deferred: u64,
}

impl JitterBuffer {
    /// The two bounds kept for outside readers. **Derived from
    /// [`JbTuning::DEFAULT`]** — never a second hand-written copy of the
    /// tuning table.
    ///
    /// This comment used to warn that `quality.rs`'s "excellent" conceal edge
    /// of 0.2 % was derived from `MIN_TARGET = 2` and that "if the two drift
    /// apart nobody will notice". Both halves came true: `min_target` became 4
    /// and nobody noticed, and the derivation itself turned out never to have
    /// been right (the initial pre-buffer conceals *nothing* — `pop()` returns
    /// `None` until `start_playback` runs, so it emits no tick and moves no
    /// counter).
    ///
    /// The coupling that does exist runs the other way: one underrun costs
    /// `min_target + 1` concealed frames, so raising `min_target` makes a
    /// single stall look *worse* to Q1, not better. That relationship is now
    /// executed rather than narrated — `quality.rs` re-derives the cost from a
    /// real `JitterBuffer` at `JbTuning::DEFAULT` and fails the suite if it
    /// leaves the "good" band. Change `min_target` and the test will tell you.
    pub const MIN_TARGET: u32 = JbTuning::DEFAULT.min_target;
    pub const MAX_TARGET: u32 = JbTuning::DEFAULT.max_target;
    const DEFAULT_FRAME_LEN: usize = 480; // 48k @ 10ms

    pub fn new(target: u32) -> Self {
        Self::with_tuning(target, JbTuning::cached())
    }

    /// 显式整定。生产走 [`JitterBuffer::new`]（读环境变量），**测试走这条**——
    /// `env::var` 是进程级的，并行测试里改它就是互相踩。
    pub fn with_tuning(target: u32, cfg: JbTuning) -> Self {
        JitterBuffer {
            frames: BTreeMap::new(),
            next_seq: None,
            prebuffering: true,
            target: target.clamp(1, cfg.max_target),
            extra: 0,
            cfg,
            tick: 0,
            next_decay_tick: cfg.extra_decay_ticks,
            last_accel_tick: 0,
            ncc_defer: 0,
            frame_len: Self::DEFAULT_FRAME_LEN,
            last_frame: Vec::new(),
            plc_run: 0,
            popped: 0,
            plc_count: 0,
            silence_count: 0,
            dropped: 0,
            underruns: 0,
            accel_events: 0,
            accel_frames: 0,
            accel_deferred: 0,
        }
    }

    pub fn tuning(&self) -> JbTuning {
        self.cfg
    }

    pub fn push(&mut self, seq: u32, frame: Vec<f32>) {
        if let Some(next) = self.next_seq {
            if seq < next {
                self.dropped += 1; // too late, already played/PLC'd
                return;
            }
        }
        if !frame.is_empty() {
            self.frame_len = frame.len();
        }
        self.frames.insert(seq, frame);
    }

    /// One 10ms tick. None before the very first frame plays (initial
    /// pre-buffering). Once started it always yields a frame: real when the
    /// next seq is buffered, otherwise PLC/silence — including while
    /// re-buffering after an underrun, so the output cadence never stalls.
    pub fn pop(&mut self) -> Option<Vec<f32>> {
        self.tick = self.tick.wrapping_add(1);
        if self.tick >= self.next_decay_tick {
            self.next_decay_tick = self.tick + self.cfg.extra_decay_ticks;
            self.extra = self.extra.saturating_sub(1);
        }
        if self.prebuffering {
            // 恢复判据用 `depth()` 而**不是** `contiguous()`：欠载之后 `next_seq`
            // 停在那个缺失的 seq 上，它可能永远不会到（真丢包），此时
            // `contiguous()` 恒为 0 ⇒ 用它当判据就是永远不再起播。
            if self.depth() < self.target_effective() {
                // Nothing has played yet => no cadence to keep and nothing to
                // conceal with; once started, hold the tick with PLC/silence.
                return match self.next_seq {
                    None => None,
                    Some(_) => Some(self.conceal()),
                };
            }
            self.prebuffering = false;
            self.start_playback();
        }
        // ---- 内存上界：**只保护内存，不做延迟控制** ----
        //
        // 判据必须是 `len()`（那才是内存占用），而延迟控制的判据必须是
        // `contiguous()`（那才是排队深度）。改动前两件事共用一条 `len() > target+6`
        // 的线，于是有洞时会误删真音频：`next_seq = 100` 未到、表 = {101..109}
        // ⇒ `len() = 9 > 8` ⇒ 删掉 101，而此刻真实排队深度 `contiguous()` 是 **0**；
        // 若 100/101 只是乱序、下一 tick 就到，两者都会因 `seq < next` 被判 late
        // 再丢一次 —— 白扔 20 ms 真实音频。
        //
        // 现在这条线抬到 `max_frames`（默认 18 帧 = 改动前的绝对最坏值，34 KB），
        // 正常运行永远够不着它，也就不会和延迟控制打架。
        // `.max(1)`：循环入口保证 `len ≥ 2`，删掉一个之后表里至少还剩一项，
        // 于是 `next_seq` 不可能被置成 `None`（下面那句 `expect("started")`
        // 押在这上面）。整定给出 0 也不会把它变成一颗定时炸弹。
        while self.frames.len() as u32 > self.cfg.max_frames.max(1) {
            if let Some((&oldest, _)) = self.frames.iter().next() {
                self.frames.remove(&oldest);
                self.dropped += 1;
            }
            self.next_seq = self.frames.keys().next().copied();
        }
        // ---- 平滑收敛：让深度真的落到 target，而不是停在天花板上 ----
        if let Some(out) = self.try_accelerate() {
            return Some(out);
        }
        let seq = self.next_seq.expect("started");
        match self.frames.remove(&seq) {
            Some(frame) => {
                self.next_seq = Some(seq.wrapping_add(1));
                self.plc_run = 0;
                self.last_frame = frame.clone();
                self.popped += 1;
                Some(frame)
            }
            None => {
                // Underrun. Rebuild depth before playing again and leave
                // next_seq parked on the missing seq: marching it on every
                // starved tick is what made later arrivals look "late" and got
                // them dropped, pinning the buffer at depth 0 forever.
                self.underruns += 1;
                self.prebuffering = true;
                // §6.3 式的惩罚项：**已经付出过的代价不许被忘掉**。这是「削过头
                // 会自己长回来」的唯一承重结构 —— 目标不是靠我们猜 `R` 猜对的，
                // 是靠这条回路测出来的。
                self.extra = (self.extra + self.cfg.underrun_step).min(self.cfg.extra_max);
                self.next_decay_tick = self.tick + self.cfg.extra_decay_ticks;
                Some(self.conceal())
            }
        }
    }

    /// 含惩罚项的**有效**目标深度（帧）。
    pub fn target_effective(&self) -> u32 {
        (self.target + self.extra).clamp(self.cfg.min_target, self.cfg.max_target)
    }

    /// 欠载惩罚项当前值（帧）。0 = 这条链路还没让我们付过代价。
    pub fn underrun_penalty(&self) -> u32 {
        self.extra
    }

    /// 起播那一刻把深度钳到目标。
    ///
    /// **只在初次起播时硬钳**：那时还没有任何音频被交付，不存在可破坏的连续性，
    /// 所以硬 flush 是零听感代价的（与 `spec-latency-trim.md` §4.4「开流 flush 到
    /// target 而不是 flush 到 0」是同一条道理，方向相反：那边是别排空过头，
    /// 这边是别起播过深）。改动前这里恒取表里最老的一帧起播，起播深度 =
    /// 预缓冲期间攒了多少，**不受控**。
    ///
    /// 欠载之后的重新起播**不钳**：那时 PLC/静音已经在交付，硬丢会再加一处硬切；
    /// 多出来的深度交给 [`Self::try_accelerate`] 带交叉淡化慢慢吐。
    fn start_playback(&mut self) {
        let Some(&last) = self.frames.keys().next_back() else {
            self.next_seq = self.frames.keys().next().copied();
            return;
        };
        if self.next_seq.is_some() {
            self.next_seq = self.frames.keys().next().copied();
            return;
        }
        let want = self.target_effective().max(1);
        // 从最新一帧往回数 `want` 帧，**不跨洞**（跨过去就是起播即欠载）。
        let mut start = last;
        let mut n = 1u32;
        while n < want {
            let prev = start.wrapping_sub(1);
            if !self.frames.contains_key(&prev) {
                break;
            }
            start = prev;
            n += 1;
        }
        let stale: Vec<u32> = self.frames.range(..start).map(|(&k, _)| k).collect();
        for k in stale {
            self.frames.remove(&k);
            self.dropped += 1;
        }
        self.next_seq = Some(start);
    }

    /// 深度高于设定点时吃掉两帧、交叉淡化拼成一帧输出 —— 深度净减 1 帧，
    /// 输出节拍不变，**没有硬切**。
    ///
    /// 这就是 NetEq 的 *Accelerate*，也是 `halbridge::trim::splice` 的
    /// `tau = frame_len` 特例（同一条淡化律，见 [`Self::splice_two`]）。
    /// 不直接复用那份代码是因为方向不通：`trim` 在 `audiohubd` 里，
    /// 而 `audiohubd` 依赖本 crate，反过来不行。
    ///
    /// 触发与限速：
    /// - **正常档**：`contiguous ≥ target + 1 + slack`，且距上次收敛 ≥
    ///   `accel_interval_ticks`（默认 100 tick = 1 s ⇒ ρ = 1 %）。
    /// - **应急档**：`contiguous > target + hard_slack`（默认就是改动前那条
    ///   `target + 6`）⇒ 无视限速，每 tick 吐一帧。改动前这里是**一个 `while`
    ///   循环在同一个 tick 里把超出的全部硬丢**，现在是同样的收敛速度（1 帧/tick）
    ///   但每一帧都带 4 ms 交叉淡化。**最坏情形不可能比改动前更差。**
    ///
    /// 只在下一帧与再下一帧都在手上时才动手（`contiguous ≥ 2` 由触发线保证），
    /// 有洞时一帧都不动 —— 有洞意味着下一 tick 必欠载，此刻删东西是纯损失。
    ///
    /// **抵消保护**：两帧尾部互相关低于 `ncc_floor` 时推迟（见该字段的推导），
    /// 最多推迟 `ncc_retry_ticks` 个 tick（5 s）就强拼 —— 等素材可以，永不收敛不行。
    /// 应急档不走这条：那一档的对照物是「一个 tick 里全部硬丢」，等不起。
    fn try_accelerate(&mut self) -> Option<Vec<f32>> {
        let d = self.contiguous();
        let target = self.target_effective();
        let emergency = d > target.saturating_add(self.cfg.hard_slack);
        let over = d >= target.saturating_add(1).saturating_add(self.cfg.slack);
        if !over && !emergency {
            return None;
        }
        if !emergency && self.tick.saturating_sub(self.last_accel_tick) < self.cfg.accel_interval_ticks
        {
            return None;
        }
        if d < 2 {
            return None;
        }
        let seq = self.next_seq?;
        // ---- 抵消保护：先看素材，再决定动不动手 ----
        //
        // 必须在 `remove` **之前**看：拿走了再发现不该拼，放回去会把
        // `next_seq` 的推进逻辑搅成一团。
        if !emergency {
            let x = self.cfg.xfade;
            let ok = match (self.frames.get(&seq), self.frames.get(&seq.wrapping_add(1))) {
                (Some(a), Some(b)) if x > 0 && a.len() == b.len() && !a.is_empty() => {
                    ncc_tail(a, b, x.min(a.len())) >= self.cfg.ncc_floor
                }
                _ => true,
            };
            if !ok && self.ncc_defer < self.cfg.ncc_retry_ticks {
                self.ncc_defer += 1;
                self.accel_deferred += 1;
                return None;
            }
        }
        self.ncc_defer = 0;
        let a = self.frames.remove(&seq)?;
        let Some(b) = self.frames.remove(&seq.wrapping_add(1)) else {
            // 触发线保证了 contiguous ≥ 2，走不到这里；真走到就把 a 放回去。
            self.frames.insert(seq, a);
            return None;
        };
        self.next_seq = Some(seq.wrapping_add(2));
        self.last_accel_tick = self.tick;
        self.accel_events += 1;
        self.accel_frames += 1;
        self.dropped += 1; // 语义未变：这确实是一次 catch-up drop
        self.popped += 1;
        self.plc_run = 0;
        let out = self.splice_two(&a, &b);
        self.last_frame = out.clone();
        Some(out)
    }

    /// 两帧交叉淡化成一帧。`out[i] = a[i]`（`i < F−X`），
    /// `out[i] = g_a·a[i] + g_b·b[i]`（`i ∈ [F−X, F)`）。
    ///
    /// 淡化律与 `halbridge::trim::splice` 逐字相同：
    /// `u = 0.5(1−cos(π(k+0.5)/X))`、`p = 0.5 + 0.5·clamp(NCC,0,1)`、
    /// `g_a = (1−u)^p`、`g_b = u^p`。一条公式覆盖两种教科书情形——
    /// `NCC = 1`（两段完全相关）⇒ `p = 1` ⇒ 等增益，对相同内容是逐样本恒等变换；
    /// `NCC = 0`（噪声类内容）⇒ `p = 0.5` ⇒ 等功率，避免不相关内容在淡化区
    /// 中点掉 −3 dB。半样本偏移 `+0.5` 让两端斜率为零（C¹ 连续）。
    ///
    /// **明确否掉「淡出到静音再淡入」**：那会在拼接点制造一个宽 `2X` 的振幅凹陷
    /// （−∞ dB 谷底），比硬切更难听。
    ///
    /// 两帧长度不等（流中途换采样率的窗口期）时退化为「丢 `a` 保 `b`」——
    /// 那一刻本来就有一处不连续，不值得为它发明第二套重采样对齐。
    fn splice_two(&self, a: &[f32], b: &[f32]) -> Vec<f32> {
        let f = a.len();
        if f == 0 || b.len() != f {
            return b.to_vec();
        }
        let x = self.cfg.xfade.min(f);
        let mut out = Vec::with_capacity(f);
        out.extend_from_slice(&a[..f - x]);
        if x == 0 {
            return out;
        }
        let ncc = ncc_tail(a, b, x);
        let p = 0.5 + 0.5 * ncc.clamp(0.0, 1.0);
        // `p` 恰为 1 时不走 `powf`：`powf(v, 1.0)` 未必逐位返回 v，而「相关时
        // 逐样本恒等」这条不变量正押在这一位上。
        let equal_gain = p >= 1.0 - 1e-6;
        for k in 0..x {
            let i = f - x + k;
            let u = 0.5 * (1.0 - (std::f32::consts::PI * (k as f32 + 0.5) / x as f32).cos());
            let (ga, gb) = if equal_gain {
                (1.0 - u, u)
            } else {
                ((1.0 - u).powf(p), u.powf(p))
            };
            out.push(ga * a[i] + gb * b[i]);
        }
        out
    }

    /// PLC (decayed repeat of the last real frame) for up to 5 consecutive
    /// starved ticks, silence afterwards.
    fn conceal(&mut self) -> Vec<f32> {
        self.plc_run += 1;
        if self.plc_run <= 5 && !self.last_frame.is_empty() {
            for s in self.last_frame.iter_mut() {
                *s *= 0.7;
            }
            self.plc_count += 1;
            self.last_frame.clone()
        } else {
            self.silence_count += 1;
            vec![0.0; self.frame_len]
        }
    }

    pub fn depth(&self) -> u32 {
        self.frames.len() as u32
    }

    /// 从 `next_seq` 起**连续**的帧数——真正的排队深度（规格 §7.2 R10）。
    ///
    /// `depth()` 返回的是 `BTreeMap` 的条目数，乱序到达时它**不等于**「队首样本
    /// 前面还排着多少样本」：若 `next_seq` 缺失而更远期的 seq 已经入表，
    /// `len()` 把「洞之后的帧」也算进去，深度被高估。举例：next_seq=10 缺失，
    /// 表里有 {11,12,13} ⇒ `depth()=3`（谎报 30 ms 排队），而 `contiguous()=0`
    /// ——下一个 tick 一定 underrun，排队深度实际是 0。
    ///
    /// 两个都要上报：`depth()` 是「占了多少内存/多少帧在手上」，`contiguous()`
    /// 是「按现在的节拍还能连续放多久」。**延迟分项用 contiguous**，因为它才是
    /// 那条「以已知速率排空 ⇒ N/rate 就是确切驻留时间」的定理成立的前提；
    /// 有洞的部分并不会以 100 帧/秒的速度被放出来。
    pub fn contiguous(&self) -> u32 {
        let Some(&first) = self.frames.keys().next() else {
            return 0;
        };
        let start = match self.next_seq {
            // 队首还没到：一个样本都排不上队，哪怕表里有一堆更晚的帧。
            Some(n) => {
                if !self.frames.contains_key(&n) {
                    return 0;
                }
                n
            }
            // 还没起播（初始预缓冲）：从表里最小的 seq 起算，那正是起播后的队首。
            None => first,
        };
        let mut n = 0u32;
        let mut want = start;
        // 表长受 `JbTuning::max_frames` 约束（默认 ≤18 项），所以这个遍历是常数级的。
        // seq 回绕在 100 帧/秒下要 497 天，`push` 的 `seq < next` 比较同样没做
        // 回绕处理——两处口径一致，不在这里单独发明一套。
        for (&seq, _) in self.frames.range(start..) {
            if seq != want {
                break;
            }
            n += 1;
            want = want.wrapping_add(1);
        }
        n
    }

    /// True while holding output to (re)build depth to target.
    pub fn prebuffering(&self) -> bool {
        self.prebuffering
    }

    /// 对外一律报**有效**目标（含欠载惩罚）——那才是环路真正在收敛的那个数，
    /// 也是 `engine.rs` 重建 JB 时该继承的那个数（重建不该把学到的裕度扔掉）。
    /// 基线值只有 [`Self::update_target`] 自己用得着。
    pub fn target(&self) -> u32 {
        self.target_effective()
    }

    /// AUTO profile: retarget from observed jitter p95。
    ///
    /// ⚠ **喂进来的统计量口径是错的，这里只是没有权限改它**：调用方给的是
    /// RFC 3550 式的逐包到达间隔抖动（一阶差分），而缓冲定深要的是「到达时刻
    /// 相对最早到达的离散度」的高分位（NetEq 的 *relative delay* 直方图）。
    /// 缓慢振荡（比如 ±500 ppm 的发包速率摆动）下一阶差分逐包几乎为 0，
    /// 而真实 spread 可达数十毫秒 —— **正是本现场踩到的那一种**。
    /// 这条不省延迟（两种口径在当前链路上都给 1 帧），但它决定了网络变差时
    /// 公式给不给得出对的数。改它要动 `engine.rs` 的 `jit_win`，不在本轮范围。
    pub fn update_target(&mut self, jitter_p95_ms: f64, frame_ms: f64) {
        if frame_ms <= 0.0 {
            return;
        }
        let t = (jitter_p95_ms / frame_ms).ceil() as i64 + 1;
        self.target = (t.max(0) as u32).clamp(self.cfg.min_target, self.cfg.max_target);
    }
}

/// 两帧尾部 `x` 个样本的归一化互相关，用来给交叉淡化选增益律。
/// 与 `halbridge::trim::ncc_at` 同一条公式（`Σab / sqrt(Σa²·Σb² + ε)`）。
fn ncc_tail(a: &[f32], b: &[f32], x: usize) -> f32 {
    let f = a.len().min(b.len());
    let s = f.saturating_sub(x);
    let (mut num, mut ea, mut eb) = (0.0f32, 0.0f32, 0.0f32);
    for i in s..f {
        num += a[i] * b[i];
        ea += a[i] * a[i];
        eb += b[i] * b[i];
    }
    let den = (ea * eb).sqrt();
    if den <= 1e-12 {
        // 两段里至少有一段是纯静音 ⇒ 没有相位可以对齐，也没有内容会被抵消。
        // 取 1（等增益）：对静音而言等增益与等功率都不改变听到的东西，
        // 而等增益顺带保证了「直流/常数输入 trim 后恒等」这条不变量。
        return 1.0;
    }
    (num / den).clamp(-1.0, 1.0)
}

/// 10ms-frame audio source for the send scheduler.
pub trait FrameSource {
    /// REPLACES the contents of `out` with exactly one frame — implementations
    /// must clear it first. Appending instead is silently destructive: the
    /// engine truncates an over-long frame back to one frame's worth, so the
    /// stream keeps re-sending whatever the FIRST call produced while every
    /// counter and probe still looks healthy.
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool;
    fn sample_rate(&self) -> u32;

    /// 本源在「交给发送调度器之前」还压着多少音频（规格 §3.2 的级 1 / 3 / 3′）。
    ///
    /// 默认 `NO_DEPTHS` = 这个源没有任何可观测的排队。`ToneSource` 就属此类：
    /// 它是即时合成的，不存在队列——**这不是「测不到」，是「确实没有」**，
    /// 所以给空数组而不是给 0 样本的假读数。
    ///
    /// 返回定长数组而非 `Vec`：本方法在 10ms 节拍上被调用，那里不允许分配
    /// （规格附录约束 3）。
    fn depths(&self) -> SourceDepths {
        NO_DEPTHS
    }
}

/// Phase-continuous sine source.
pub struct ToneSource {
    rate: u32,
    frame_samples: usize,
    amp: f64,
    step: f64,
    phase: f64,
}

impl ToneSource {
    pub fn new(freq_hz: f32, amp: f32, sample_rate: u32, frame_ms: u32) -> Self {
        ToneSource {
            rate: sample_rate,
            frame_samples: (sample_rate as u64 * frame_ms as u64 / 1000) as usize,
            amp: amp as f64,
            step: 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64,
            phase: 0.0,
        }
    }
}

impl FrameSource for ToneSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        out.clear();
        for _ in 0..self.frame_samples {
            out.push((self.amp * self.phase.sin()) as f32);
            self.phase += self.step;
        }
        // rem_euclid keeps continuity exactly (sin is 2π-periodic) while
        // bounding f64 magnitude over long runs
        self.phase = self.phase.rem_euclid(2.0 * std::f64::consts::PI);
        true
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }
}

/// Default-microphone source, resampled to 48k. Underruns emit silence so the
/// send cadence never stalls.
///
/// # 它**不**持有 cpal 流 —— 这一条是承重的
///
/// `LiveCapture` 里是一个 `cpal::Stream`，在 macOS 上 **`!Send`**。只要这个
/// 结构体里有它，`MicSource` 就是 `!Send`，于是「开设备」这件事就只能发生在
/// 最终消费它的那条线程上 —— 也就是 `tx_loop` 的 10 ms 截止期线程。
/// 而开一次 CoreAudio 输入设备的量级与实测停顿直方图的 110–600 ms 正好对得上
/// （`docs/spec-latency-floor.md` §9.3 手段 J1）。
///
/// 所以拆开：**开设备的线程持有 `LiveCapture`，音频线程只拿 `AudioRx`**
/// （无锁环的消费端，`Send`）。[`MicSource::open`] 把两者一起返回，调用方必须
/// 让那个 `LiveCapture` 活着——丢掉它 = 关掉采集流 = 这个源从此只出静音。
pub struct MicSource {
    rx: AudioRx,
    resampler: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    frame_samples: usize,
    /// FIFO 满时丢掉的样本数（累计）。方向是 **`DropMode::Oldest`**
    /// （`while len > CAP { pop_front() }`）：饱和时驻留恰好 = CAP/48000 = 1 秒，
    /// 音频连续，听感是「恒定迟到但不断」。
    dropped: u64,
}

impl MicSource {
    pub const OUT_RATE: u32 = 48000;
    const FIFO_CAP: usize = 48000; // 1s: bound added latency

    /// 开默认输入设备。**返回的 `LiveCapture` 必须被调用方保管好**（见类型
    /// 文档）：它是 `!Send` 的 cpal 流，只能留在开它的这条线程上，而
    /// `MicSource` 可以被送去任何线程消费。
    pub fn open(frame_ms: u32) -> Result<(MicSource, LiveCapture)> {
        let (cap, rx, rate) = LiveCapture::start()?;
        Ok((MicSource::from_rx(rx, rate, frame_ms), cap))
    }

    /// 从一个已经在跑的采集环造源。速率取自设备，不是假定 48k。
    pub fn from_rx(rx: AudioRx, rate: u32, frame_ms: u32) -> MicSource {
        let resampler = if rate == Self::OUT_RATE {
            None
        } else {
            Some(LinearResampler::new(rate, Self::OUT_RATE))
        };
        MicSource {
            rx,
            resampler,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: (Self::OUT_RATE as u64 * frame_ms as u64 / 1000) as usize,
            dropped: 0,
        }
    }

    pub fn fifo_len(&self) -> u32 {
        self.fifo.len() as u32
    }

    pub fn fifo_cap(&self) -> u32 {
        Self::FIFO_CAP as u32
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl FrameSource for MicSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.rx.pop(&mut self.raw);
        match self.resampler.as_mut() {
            None => self.fifo.extend(self.raw.iter().copied()),
            Some(rs) => {
                self.staged.clear();
                rs.process(&self.raw, &mut self.staged);
                self.fifo.extend(self.staged.iter().copied());
            }
        }
        while self.fifo.len() > Self::FIFO_CAP {
            self.fifo.pop_front();
            self.dropped += 1; // 丢弃行为未改，只是现在数得出来
        }
        out.clear();
        if self.fifo.len() >= self.frame_samples {
            out.extend(self.fifo.drain(..self.frame_samples));
        } else {
            out.resize(self.frame_samples, 0.0);
        }
        true
    }

    fn sample_rate(&self) -> u32 {
        Self::OUT_RATE
    }

    /// 麦克风源横跨两级：声卡采集环（2 s，丢最新）→ 重采样 → 发送 FIFO
    /// （1 s，丢最旧）。两级的**速率不同**（采集环走设备速率，FIFO 已经是
    /// 48k），所以必须分开上报，不能相加成一个数。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::CapRing,
                samples: self.rx.queued(),
                capacity: self.rx.capacity(),
                rate: self.rx.rate(),
                dropped: Some(self.rx.dropped()),
                drop_mode: DropMode::Newest,
            }),
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo_len(),
                capacity: self.fifo_cap(),
                rate: Self::OUT_RATE,
                dropped: Some(self.dropped),
                drop_mode: DropMode::Oldest,
            }),
        ]
    }
}

/// System-audio source (spec-m4b §B): whatever this machine is playing,
/// resampled to 48k mono. Same shape as MicSource — underruns emit silence so
/// the send cadence never stalls, and the FIFO is bounded so a stalled reader
/// costs audio, not latency. `excludes_self()` reports whether the chosen
/// backend keeps our own playback out of the capture; a false there while we
/// are also playing the peer's audio is the feedback loop of plan §5.
pub struct SysAudioSource {
    cap: Box<dyn SysAudioCapture>,
    info: BackendInfo,
    resampler: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
    frame_samples: usize,
    /// 同 `MicSource::dropped`：方向是 `DropMode::Oldest`。
    dropped: u64,
}

impl SysAudioSource {
    pub const OUT_RATE: u32 = 48000;
    const FIFO_CAP: usize = 48000; // 1s

    /// `backend` is a backend id or `sysaudio::BACKEND_AUTO` ("auto").
    pub fn new(frame_ms: u32, backend: &str) -> Result<Self> {
        let info = sysaudio::resolve_backend(backend)?;
        let cap = sysaudio::start_backend(&info.id)?;
        let rate = cap.sample_rate();
        let resampler = if rate == Self::OUT_RATE {
            None
        } else {
            Some(LinearResampler::new(rate, Self::OUT_RATE))
        };
        Ok(SysAudioSource {
            cap,
            info,
            resampler,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: (Self::OUT_RATE as u64 * frame_ms as u64 / 1000) as usize,
            dropped: 0,
        })
    }

    pub fn backend(&self) -> &BackendInfo {
        &self.info
    }

    pub fn excludes_self(&self) -> bool {
        self.info.excludes_self
    }

    /// Rate the backend actually captures at (before the 48k conversion).
    pub fn capture_rate(&self) -> u32 {
        self.cap.sample_rate()
    }

    pub fn fifo_len(&self) -> u32 {
        self.fifo.len() as u32
    }

    pub fn fifo_cap(&self) -> u32 {
        Self::FIFO_CAP as u32
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl FrameSource for SysAudioSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.cap.read(&mut self.raw);
        match self.resampler.as_mut() {
            None => self.fifo.extend(self.raw.iter().copied()),
            Some(rs) => {
                self.staged.clear();
                rs.process(&self.raw, &mut self.staged);
                self.fifo.extend(self.staged.iter().copied());
            }
        }
        while self.fifo.len() > Self::FIFO_CAP {
            self.fifo.pop_front();
            self.dropped += 1;
        }
        out.clear();
        if self.fifo.len() >= self.frame_samples {
            out.extend(self.fifo.drain(..self.frame_samples));
        } else {
            out.resize(self.frame_samples, 0.0);
        }
        true
    }

    fn sample_rate(&self) -> u32 {
        Self::OUT_RATE
    }

    /// 只有发送 FIFO 一级：系统音频后端自己的内部缓冲不经过 `AudioRx`，
    /// 从这里读不到——**所以不报**，而不是报 0（规格 §7.2 R11 记着这条口径缺口：
    /// Windows loopback 交付的样本尚未经过本机 DAC，那部分是 P1 的活）。
    fn depths(&self) -> SourceDepths {
        [
            Some(StageDepth {
                id: StageId::SrcFifo,
                samples: self.fifo_len(),
                capacity: self.fifo_cap(),
                rate: Self::OUT_RATE,
                dropped: Some(self.dropped),
                drop_mode: DropMode::Oldest,
            }),
            None,
        ]
    }
}

/// Deterministic sender-side loss injection (LCG seeded by stream_id).
pub struct LossInjector {
    state: u64,
    loss_pct: f64,
}

impl LossInjector {
    pub fn new(stream_id: u32, loss_pct: f32) -> Self {
        LossInjector {
            state: stream_id as u64,
            loss_pct: loss_pct.clamp(0.0, 100.0) as f64,
        }
    }

    /// Advance the LCG; true = drop this packet before sending.
    pub fn should_drop(&mut self) -> bool {
        // Knuth MMIX constants
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let u = (self.state >> 33) as f64 / (1u64 << 31) as f64; // [0,1)
        u * 100.0 < self.loss_pct
    }
}

// ---------------------------------------------------------------- 质量阶梯
//
// # 一条阶梯，两个维度
//
// 这里曾经只有 `AUTO_RATES: [u32; 4]`——阶梯只管**采样率**，位深写死 16 位，
// 而「写死 16 位」这件事没有任何一处代码说得出来。用户裁定位深必须有选项，
// 且 (kHz, bit) 两个维度**合并成一条按码率排序的阶梯**（不是两个滑条）。
//
// # 排序准则：先把采样率买满 48 kHz，再买位深
//
// 主序是音频码率升序。但 `32 kHz × 24 bit` 与 `48 kHz × 16 bit` **都是
// 768 kbps、数据报还逐字节等长**，码率排不了序，必须另给准则。最强的一条依据
// 只依赖本仓库的代码：
//
//   **48 kHz 是唯一不经重采样的档**（`engine.rs` 的 tx 侧只在 `rung != 0` 建
//   重采样器，rx 侧 `if h.sample_rate == 48000` 直通），而 [`LinearResampler`]
//   是**纯线性插值、没有任何抗混叠低通**。48 k → 16 k 抽取时 12 kHz 的分量
//   折回 4 kHz 只被压约 1.8 dB —— 比 16 位量化噪声底高约 90 dB 量级。
//   ⇒ 拿 48 kHz 换位深 = 用 90 dB 的损伤换 48 dB 的改善。
//
// ⇒ 阶梯是「链」不是「网格」：4 采样率 × 3 位深的全网格会在 384 / 512 / 768
// kbps 上撞出**三处精确并列**，而滑条这个控件的前提是全序。
//
// # 反着走 = 先扔最不值钱的
//
// 从顶往下是 `32f → 24 bit → 16 bit → 32 kHz → 24 kHz → 16 kHz`，
// 即**先掉位深（听不出），再掉带宽（听得出）**。这是排序准则的副产品，
// 所以 `AutoLadder` 不需要第二套规则来决定「网络变差先降哪个」。

use audiohub_core::dsp::WireDepth;

/// 阶梯上的一格：(采样率, 线上位深)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WireFormat {
    pub rate_hz: u32,
    pub depth: WireDepth,
}

impl WireFormat {
    /// 一帧（`FRAME_MS = 10 ms`）单声道的**明文**字节数。
    pub const fn frame_bytes(&self) -> usize {
        (self.rate_hz as usize / 100) * self.depth.bytes_per_sample()
    }

    /// **音频**码率（kbps）= `rate × depth_bits / 1000`，单声道。
    ///
    /// ⚠ **不含协议开销**，与改动前 `rate * 16 / 1000` 同一口径（单调性因此
    /// 保住）。深档的开销比例更高：它们按 5 ms 分包，每 10 ms 要付**两份**
    /// 56 字节的头 + 标签。拿这个数去反推实测带宽对不上是正常的，不是 bug。
    pub const fn kbps(&self) -> u32 {
        self.rate_hz / 1000 * self.depth.bits()
    }

    /// 线上是不是按 5 ms 分成两个包发（见 [`WireFormat::wire_packets_per_frame`]）。
    pub const fn splits_frame(&self) -> bool {
        self.frame_bytes() > SINGLE_PACKET_PAYLOAD_MAX
    }

    /// 一帧上线拆成几个数据报。今天只有 1 或 2。
    pub const fn wire_packets_per_frame(&self) -> usize {
        if self.splits_frame() {
            2
        } else {
            1
        }
    }
}

/// 一个数据报能装的最大明文字节数，**留足以太网 MTU 的余量**。
///
/// 1500 − 28 (IPv4 + UDP) − 40 (`HEADER_LEN`) − 16 (AEAD 标签) = 1416。
/// 这里取 1200：常见隧道 MTU（WireGuard 1420 / PPPoE 1492）下也不分片。
/// 超过它的档在线上按 5 ms 分成两个包（`engine.rs` 的 `tx_loop`）——
/// **`FRAME_MS` 一个字不改，只动线路层的包时长**。
///
/// 这就是 AES67「缩短包时长」那条正解，只用在线路层：AES67 没有我们这种抖动
/// 缓冲，所以它把「包时长」与「调度节拍」当成一件事；我们不必。
pub const SINGLE_PACKET_PAYLOAD_MAX: usize = 1200;

/// 质量阶梯。**rung 0 = 最好**（与 `AutoLadder` 的 `rung += 1` 是降档方向一致）。
///
/// | rung | 采样率 | 位深 | 音频码率 | 每帧明文 | 线上分包 |
/// |---|---|---|---|---|---|
/// | 0 | 48 kHz | 32 float | 1536 kbps | 1920 B | 2 × 5 ms |
/// | 1 | 48 kHz | 24 | 1152 kbps | 1440 B | 2 × 5 ms |
/// | 2 | 48 kHz | 16 | 768 kbps | 960 B | 1 × 10 ms |
/// | 3 | 32 kHz | 16 | 512 kbps | 640 B | 1 × 10 ms |
/// | 4 | 24 kHz | 16 | 384 kbps | 480 B | 1 × 10 ms |
/// | 5 | 16 kHz | 16 | 256 kbps | 320 B | 1 × 10 ms |
///
/// 下半段四档与位深进阶梯之前**逐位相同**（那四个采样率正对 ITU-T 的
/// 窄带/宽带/超宽带/全频带分级，`quality.rs` 的 Q3 直接映射它）。
/// 上半段两档是位深维度上仅有的两个有工业意义的取值：24 bit 是 AES67 /
/// RFC 3190 的标准交换深度，32 bit 浮点是本管线的原生格式。
pub const LADDER: [WireFormat; 6] = [
    WireFormat { rate_hz: 48000, depth: WireDepth::F32 },
    WireFormat { rate_hz: 48000, depth: WireDepth::S24 },
    WireFormat { rate_hz: 48000, depth: WireDepth::S16 },
    WireFormat { rate_hz: 32000, depth: WireDepth::S16 },
    WireFormat { rate_hz: 24000, depth: WireDepth::S16 },
    WireFormat { rate_hz: 16000, depth: WireDepth::S16 },
];

/// AUTO 能升到的**最高**格（数值最小 = 最好）。
///
/// = `pcm48k16`，即位深进阶梯之前 AUTO 的顶档。
///
/// # 为什么 AUTO 不去拿那两个深档
///
/// **AUTO 不该在用户没要求时把带宽翻倍。** 今天 AUTO 的稳态是 768 kbps；
/// 若顶档变成 1536 kbps，所有 AUTO 用户的带宽**静默翻倍**，而收益听不出来。
/// AUTO 的职责是「别浪费」，深档是「用户明确要求」。
///
/// ⇒ 改成 `0` 就能让 AUTO 也升到 32 位浮点，代价只有「默认带宽翻倍」这一件事。
pub const AUTO_TOP_RUNG: u32 = 2;

/// AUTO 在**降级链路**（Tier 1/2）上能升到的最高格。plan §16.3 冻结为 3。
///
/// 用户裁定的理由逐字：TCP 上深档更吃亏（协议开销 + 队头阻塞），
/// **降级链路优先保证有声音，不优先保证深档**。用户明确接受由此产生的语义
/// 后果：**同一个 AUTO 在不同 tier 上顶档不同**。
///
/// 与已否决 #18（「AUTO 不自动升深档」）**方向相同、幅度更保守**：rung 号越大
/// 码率越低，这是把天花板再压低一格，不是把「自动升深档」端回来。
pub const AUTO_TOP_RUNG_STREAMED: u32 = 3;

const _: () = assert!(
    AUTO_TOP_RUNG_STREAMED > AUTO_TOP_RUNG && (AUTO_TOP_RUNG_STREAMED as usize) < LADDER.len(),
    "the degraded ceiling has to be a real rung and it has to be *lower quality* than tier 0's \
     (rung numbers count downwards in bitrate): a value above AUTO_TOP_RUNG would make AUTO \
     spend more bandwidth on the worse transport, which is backwards"
);

/// Tier 1/2 降档线：发送侧队列积压毫秒数（[`AutoLadder::feed_streamed`] 的主
/// 信号）。连续 [`STREAMED_HOT_PERIODS`] 个统计周期越线才降一格。
///
/// # 为什么是 20 ms，以及它为什么在丢音之前就开火
///
/// 队列积压是 `loss_pct` 在 TCP 上的**直接对应物**：UDP 下「跟不上」表现为对端
/// 丢包，TCP 下表现为字节堆在我们自己的队列里（TCP 把丢包信号擦掉了）。
/// 20 ms 是一帧的两倍，也是媒体 socket 的 `SO_SNDTIMEO`（一次写阻塞的粒度）
/// —— 低于它就分不清「积压」与「一次正常的写阻塞」。
///
/// 而它远低于陈旧闸门的预算（`tcpmedia::STALE_BUDGET`，几百毫秒量级）：
/// **AUTO 因此在闸门开始丢音频之前就降档**，这正是这个信号比丢包好的地方
/// —— 丢包信号只有在音频已经没了之后才存在。
pub const STREAMED_WRITEQ_HOT_MS: f64 = 20.0;

/// 升档线：积压回到这个数以下才算一个干净周期。取降档线的一半。
///
/// # 这个数是实测定的，第一版（2.0 ms）实测**让阶梯永远升不回去**
///
/// AUTO 消费的是**每秒的峰值**（`take_writeq_peak_ms`）——峰值是对的选择，
/// 积压是事件，用均值会把它抹平。但峰值同时意味着这条判据看到的是尾部：
/// 2026-08-08 本机双 daemon、tier 1、链路完全空闲的 20 s 窗口里，每秒峰值实测
/// **0.3 / 0.4 / 0.4 / 1.3 / 2.2 ms**。也就是说 2.0 ms 这条线在一条**毫无积压**
/// 的链路上大约每十几秒就被越过一次，而升档要求**连续 10 个**干净周期
/// ⇒ 实测 34 秒零积压、`rung_changes` 一次都没动，阶梯钉死在最低档。
///
/// 失效形态值得记：它不报错、不掉音、听起来"稳定"，只是永远比链路能给的差
/// 一到两档。判据用尾部统计量去证明"没有问题"，本来就该给调度器留余量。
///
/// 取 `HOT / 2` 而不是另写一个数：两条线之间保持一个明确的死区，谁改降档线
/// 谁就同时改了升档线，不会只动一半。
pub const STREAMED_WRITEQ_CLEAN_MS: f64 = STREAMED_WRITEQ_HOT_MS / 2.0;

/// 升档的辅助闸门：接收侧单向时延展布（`stats::SpreadWindow`）。
///
/// 干净线取 [`JbTuning::DEGRADED`] 的**最浅**深度：展布小于抖动缓冲在它最浅
/// 处就能吸收的量 ⇒ 这条链路的交付时序没有让接收端付过代价，可以谈加带宽。
/// 与 `min_target` 绑定而不是写死一个数：整定一动，这条线跟着动。
pub const STREAMED_SPREAD_CLEAN_MS: f64 =
    (JbTuning::DEGRADED.min_target as u64 * FRAME_MS) as f64;

/// 连续几个周期越线才降一格。
///
/// 3 而不是 1：TCP 上一次重传就能把一帧压在队列里 200–300 ms，而重传是**正常**
/// 的（tier 1 存在的前提就是这条链路只能跑 TCP）。一次就降档等于让每一次重传
/// 都砍一格带宽，几秒钟就踩到阶梯底部——那是 §14「一次抖动尖峰不该直接打到
/// 最低档」在 TCP 上的同型病。
pub const STREAMED_HOT_PERIODS: u32 = 3;

/// 格号 → 格式。越界钳到最低档（最差那一格）。
pub fn rung_format(rung: u32) -> WireFormat {
    LADDER[(rung as usize).min(LADDER.len() - 1)]
}

/// 格号 → 采样率。[`rung_format`] 的投影，留着是因为只关心采样率的调用点很多。
pub fn rung_rate(rung: u32) -> u32 {
    rung_format(rung).rate_hz
}

/// (采样率, 位深) → 格号。`None` = 这个组合不在阶梯上。
///
/// **刻意不做就近吸附**：找不到就是找不到，调用方得决定怎么办。
/// 吸附会让一个不存在的档静默变成一个存在的档，而 UI 显示的还是原来那个。
///
/// ⚠ 位深进阶梯之后**不存在「采样率 → 格号」这个函数**：48000 在阶梯上出现
/// 三次，速率不再唯一标识一档。凡是从速率反查格号的老代码都必须补上位深。
pub fn rung_of(rate_hz: u32, depth: WireDepth) -> Option<u32> {
    LADDER
        .iter()
        .position(|f| f.rate_hz == rate_hz && f.depth == depth)
        .map(|i| i as u32)
}

/// Pure sender-side ladder state machine, fed once per 1s stats period.
/// Demote fast (loss>5% or jitter>15ms), promote after 10 clean periods
/// (loss<0.5% and jitter<5ms); middling stats reset the clean streak.
///
/// 阶梯变长（4 → 6 格）**没有改这个状态机一行逻辑**，只是把两个边界常数从
/// 「字面量 / `AUTO_RATES.len()`」换成 [`AUTO_TOP_RUNG`] 与 [`LADDER`]。
pub struct AutoLadder {
    rung: u32,
    clean: u32,
    /// 连续越过 [`STREAMED_WRITEQ_HOT_MS`] 的周期数。**只有
    /// [`AutoLadder::feed_streamed`] 碰它**；Tier 0 的路径上它恒为 0。
    hot: u32,
    /// 这台阶梯的天花板。Tier 0 = [`AUTO_TOP_RUNG`]，降级链路 =
    /// [`AUTO_TOP_RUNG_STREAMED`]（plan §16.3）。
    ///
    /// # 为什么是字段而不是让两条路径各读各的常量
    ///
    /// 让 `feed_stats` 继续读常量、只在 `feed_streamed` 里读天花板，会留下
    /// 一个安静的洞：一台降级阶梯若被喂了 Tier 0 的信号（调用方拿错方法），
    /// 它会一路升到 rung 2 —— 也就是这条裁定要禁的那件事，而没有任何一处会
    /// 报错。天花板做成状态之后，**拿错方法最坏只是判据用错，档位边界仍然
    /// 由构造时那次选择说了算**。
    ///
    /// Tier 0 的行为逐位不变：`new()` 把它初始化成 `AUTO_TOP_RUNG`，于是
    /// `feed_stats` 的比较与改动前是同一个数。
    top_rung: u32,
    pub rung_changes: u32,
}

impl AutoLadder {
    pub fn new() -> Self {
        // 从 AUTO 的天花板起步，不是从阶梯顶端起步。
        AutoLadder { rung: AUTO_TOP_RUNG, clean: 0, hot: 0, top_rung: AUTO_TOP_RUNG, rung_changes: 0 }
    }

    /// 降级链路（Tier 1/2）上的阶梯：天花板压到
    /// [`AUTO_TOP_RUNG_STREAMED`]，判据换成 [`AutoLadder::feed_streamed`]。
    pub fn new_streamed() -> Self {
        AutoLadder {
            rung: AUTO_TOP_RUNG_STREAMED,
            clean: 0,
            hot: 0,
            top_rung: AUTO_TOP_RUNG_STREAMED,
            rung_changes: 0,
        }
    }

    /// 这台阶梯能升到的最高格。UI 上「AUTO 能升到哪」这句话必须按它说，
    /// **不得照抄 Tier 0 的 2**（plan §16.3 的语义后果）。
    pub fn top_rung(&self) -> u32 {
        self.top_rung
    }

    pub fn rung(&self) -> u32 {
        self.rung
    }

    pub fn sample_rate(&self) -> u32 {
        rung_rate(self.rung)
    }

    pub fn format(&self) -> WireFormat {
        rung_format(self.rung)
    }

    /// Some(new rung index) only when the rung actually changed.
    pub fn feed_stats(&mut self, loss_pct: f64, jitter_ms: f64) -> Option<u32> {
        if loss_pct > 5.0 || jitter_ms > 15.0 {
            self.clean = 0;
            if self.rung < LADDER.len() as u32 - 1 {
                self.rung += 1;
                self.rung_changes += 1;
                return Some(self.rung);
            }
            return None;
        }
        if loss_pct < 0.5 && jitter_ms < 5.0 {
            self.clean = self.clean.saturating_add(1);
            // `> self.top_rung` 而不是 `> 0`：AUTO 不许自己走进深档。
            // Tier 0 上 `top_rung == AUTO_TOP_RUNG`，与改动前是同一个比较。
            if self.clean >= 10 && self.rung > self.top_rung {
                self.clean = 0;
                self.rung -= 1;
                self.rung_changes += 1;
                return Some(self.rung);
            }
            return None;
        }
        self.clean = 0;
        None
    }

    /// 降级链路（Tier 1/2）上的一个统计周期。**与 [`AutoLadder::feed_stats`]
    /// 互斥使用**，由构造函数选定：`new()` 配前者，`new_streamed()` 配这个。
    ///
    /// # 为什么 TCP 上必须换判据（这是本方法存在的全部理由）
    ///
    /// - `loss_pct` 在 TCP 上**恒为 0**。重传把丢包变成延迟，于是 Tier 0 的
    ///   主判据在这条链路上是一个常数——它不是变迟钝了，是完全没有信号。
    /// - `jitter_ms` 是 RFC 3550 一阶差分，而 TCP 的失效形态是「停顿一下、
    ///   然后**成串**送达」：串内相邻差分近似 0，只有停顿后第一个包扛住全部
    ///   延迟 ⇒ 窗口分位数被**系统性低估**。这与 `engine.rs` 那条「两个半包
    ///   共用时间戳把一半抖动样本压成 0」的教训完全同型。
    ///
    /// # 两个信号的分工
    ///
    /// - **主：`writeq_ms`**（发送侧队列积压）。它是 `loss_pct` 在 TCP 上的
    ///   直接对应物，而且**本地可得**——不必等对端的 `Stats` 回传，所以降档
    ///   比 Tier 0 早一整个统计周期。
    /// - **辅：`spread_ms`**（接收侧单向时延展布，p95 − 同窗口最小值）。
    ///   **只参与升档，不参与降档**：tier 1 上一次重传就会如实推高展布，而
    ///   重传是这条链路的常态；拿它降档等于每次重传砍一格，几秒钟就到底。
    ///
    /// `spread_ms = None`（对端没报 / 窗口还不够长）**按干净处理**。理由与
    /// 本仓反复吃过的「缺席不等于坏」是同一条：主信号是本地的、永远有值，
    /// 而让一个沉默的对端把档位永久钉住，是把「没有测量」当成了「测到很糟」。
    pub fn feed_streamed(&mut self, writeq_ms: f64, spread_ms: Option<f64>) -> Option<u32> {
        if writeq_ms > STREAMED_WRITEQ_HOT_MS {
            self.clean = 0;
            self.hot = self.hot.saturating_add(1);
            if self.hot >= STREAMED_HOT_PERIODS {
                self.hot = 0;
                if self.rung < LADDER.len() as u32 - 1 {
                    self.rung += 1;
                    self.rung_changes += 1;
                    return Some(self.rung);
                }
            }
            return None;
        }
        // 一个不越线的周期就把连击清零：判据是「**连续** 3 个」。
        self.hot = 0;
        let spread_ok = spread_ms.is_none_or(|s| s < STREAMED_SPREAD_CLEAN_MS);
        if writeq_ms <= STREAMED_WRITEQ_CLEAN_MS && spread_ok {
            self.clean = self.clean.saturating_add(1);
            if self.clean >= 10 && self.rung > self.top_rung {
                self.clean = 0;
                self.rung -= 1;
                self.rung_changes += 1;
                return Some(self.rung);
            }
            return None;
        }
        self.clean = 0;
        None
    }
}

impl Default for AutoLadder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;
    use crate::packet::{Codec, HEADER_LEN};

    /// **MTU 预算钉成断言**：每档「每帧明文字节数」与设计表一致，且**每个
    /// 数据报**（含 40 B 头 + 16 B AEAD 标签 + 28 B IP/UDP）都进得去 1500 MTU。
    ///
    /// 没有它，下次有人加一档（96 kHz？立体声？）会撞同一堵墙，而失效形态是
    /// **Windows 上收流线程每个超长包睡 100 ms**、日志里只有一行 `udp recv:`。
    #[test]
    fn every_rung_fits_in_one_ethernet_datagram() {
        const MTU: usize = 1500;
        const IP_UDP: usize = 28;
        const AEAD_TAG: usize = 16;
        let want_frame_bytes = [1920usize, 1440, 960, 640, 480, 320];
        let want_kbps = [1536u32, 1152, 768, 512, 384, 256];
        assert_eq!(LADDER.len(), want_frame_bytes.len());
        for (i, f) in LADDER.iter().enumerate() {
            assert_eq!(f.frame_bytes(), want_frame_bytes[i], "rung {i} 的每帧明文变了");
            assert_eq!(f.kbps(), want_kbps[i], "rung {i} 的音频码率变了");
            // 分包之后每个数据报装 frame_bytes / n。
            let per_packet = f.frame_bytes() / f.wire_packets_per_frame();
            let ip_datagram = HEADER_LEN + per_packet + AEAD_TAG + IP_UDP;
            assert!(
                ip_datagram <= MTU,
                "rung {i} 的 IP 报文 {ip_datagram} B 超过 MTU {MTU}：\
                 要么调 SINGLE_PACKET_PAYLOAD_MAX，要么这一档不该存在"
            );
            // 分包必须切得整齐：半帧的样本数与字节数都得是整数。
            assert_eq!(
                f.frame_bytes() % f.wire_packets_per_frame(),
                0,
                "rung {i} 的帧切不成等长的两半"
            );
        }
    }

    /// 阶梯的形状：码率**严格递减**（rung 0 最好）、格式两两不同、
    /// 且下半段四档与位深进阶梯之前逐位相同。
    #[test]
    fn the_ladder_is_strictly_ordered_and_keeps_the_four_legacy_rungs() {
        for w in LADDER.windows(2) {
            assert!(
                w[0].kbps() > w[1].kbps(),
                "阶梯码率不是严格递减：{} !> {}",
                w[0].kbps(),
                w[1].kbps()
            );
        }
        let mut seen: Vec<(u32, &str)> =
            LADDER.iter().map(|f| (f.rate_hz, f.depth.as_str())).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "阶梯上有两格是同一个 (采样率, 位深)");
        // 位深进阶梯之前的四档：48/32/24/16 kHz，全 s16。
        let legacy: Vec<u32> = LADDER[AUTO_TOP_RUNG as usize..]
            .iter()
            .inspect(|f| assert_eq!(f.depth, WireDepth::S16, "老四档必须仍是 s16"))
            .map(|f| f.rate_hz)
            .collect();
        assert_eq!(legacy, vec![48000, 32000, 24000, 16000], "老四档的采样率被动过了");
    }

    /// **AUTO 从 `AUTO_TOP_RUNG` 起步，且升档永不越过它。**
    ///
    /// 注入对照：把 `feed_stats` 的 `self.rung > AUTO_TOP_RUNG` 改回 `> 0`，
    /// 这条立刻变红——而那次改动在生产上的表现是「所有 AUTO 用户的带宽
    /// 静默翻倍」，没有任何一处会报错。
    #[test]
    fn auto_starts_at_its_ceiling_and_never_promotes_past_it() {
        let mut l = AutoLadder::new();
        assert_eq!(l.rung(), AUTO_TOP_RUNG, "AUTO 起步格不是天花板");
        assert_eq!(l.format(), WireFormat { rate_hz: 48000, depth: WireDepth::S16 });
        // 一直干净：升到天花板就停住，绝不进深档。
        for _ in 0..500 {
            l.feed_stats(0.0, 0.0);
            assert!(l.rung() >= AUTO_TOP_RUNG, "AUTO 升进了深档 rung {}", l.rung());
        }
        assert_eq!(l.rung(), AUTO_TOP_RUNG);
        // 一直很差：降到最低档就停住，不会越界。
        for _ in 0..500 {
            l.feed_stats(50.0, 100.0);
        }
        assert_eq!(l.rung(), LADDER.len() as u32 - 1, "降档没停在最低格");
        // 再一路干净：回到天花板，仍不越过。
        for _ in 0..500 {
            l.feed_stats(0.0, 0.0);
        }
        assert_eq!(l.rung(), AUTO_TOP_RUNG, "回升没停在天花板");
    }

    /// **升降各自一次只走一格**，且 `feed_stats` 的返回值如实报出每一步。
    ///
    /// 上面那条只比 500 次之后的**终点**，所以「一次坏统计直接踩到最低档」
    /// 与「逐格下降」在它眼里完全相同 —— 它对降档路径免疫。而那两种行为在
    /// 生产上差得很远：一次抖动尖峰（≥15 ms，正常链路波动就能到）若直接把
    /// 48 kHz 打到 16 kHz，中间的 32 k / 24 k 两档形同虚设，然后要 30 个干净
    /// 周期（≈30 s）才爬得回来；顺带 `rung_changes` 从 3 变 1，界面上的
    /// 「换档次数」跟着说谎。
    ///
    /// 判据用**返回值**而不是 `rung()`：`feed_stats` 返回 `Some(新格号)`
    /// 正是为此存在，只看终态就等于把它的契约放空。
    #[test]
    fn each_stats_period_moves_the_rung_by_exactly_one_step() {
        let mut l = AutoLadder::new();
        // 降档：从天花板一路到最低格，每次恰好一格。
        for want in (AUTO_TOP_RUNG + 1)..LADDER.len() as u32 {
            assert_eq!(l.feed_stats(50.0, 100.0), Some(want), "降档跳格了：一次坏统计只许走一格");
        }
        assert_eq!(l.feed_stats(50.0, 100.0), None, "到底之后不该再报变化");
        let down_steps = LADDER.len() as u32 - 1 - AUTO_TOP_RUNG;
        assert_eq!(l.rung_changes, down_steps, "换档次数与实际走过的格数对不上");

        // 升档：10 个干净周期换一格，同样一次一格。
        for want in (AUTO_TOP_RUNG..LADDER.len() as u32 - 1).rev() {
            for i in 0..9 {
                assert_eq!(l.feed_stats(0.0, 0.0), None, "第 {i} 个干净周期就升档了：升档必须保守");
            }
            assert_eq!(l.feed_stats(0.0, 0.0), Some(want), "升档跳格了");
        }
        assert_eq!(l.feed_stats(0.0, 0.0), None, "到天花板之后不该再报变化");
        assert_eq!(l.rung_changes, down_steps * 2, "一降一升，换档次数应当翻倍");
    }

    /// The tier 0 trajectory is **frozen against the pre-P4 build**, step by
    /// step.
    ///
    /// # Why a recorded sequence and not another set of properties
    ///
    /// P4 adds a second judgement path to this state machine, and the risk it
    /// carries is not "the new path is wrong" — that path has its own tests —
    /// but "the old path moved a little while nobody was looking". Every
    /// existing test here asserts a *property* (starts at the ceiling, one step
    /// per period, floors at the bottom), and a property-shaped test cannot see
    /// a change in *when* the steps happen. This one can: it is the same 600
    /// periods fed to both builds, compared element by element.
    ///
    /// The expected values were minted by compiling `feed_stats` **as it stood
    /// at HEAD 5169ab3**, verbatim, into a standalone binary and running this
    /// exact driver against it (2026-08-08). They are not this build's output
    /// written down — that would assert only that the code equals itself.
    ///
    /// Run-length encoded because 600 integers of which 105 are consecutive
    /// fives is not more convincing at full length, only longer.
    #[test]
    fn the_tier_zero_rung_trajectory_is_unchanged_step_for_step() {
        #[rustfmt::skip]
        const GOLDEN_RLE: [(u32, usize); 30] = [
            (2, 3), (3, 4), (4, 12), (5, 51), (4, 10), (3, 2), (4, 5), (5, 12),
            (4, 3), (5, 29), (4, 14), (5, 30), (4, 17), (5, 38), (4, 21), (5, 105),
            (4, 5), (5, 10), (4, 15), (5, 31), (4, 19), (3, 23), (4, 8), (5, 51),
            (4, 8), (5, 37), (4, 6), (5, 20), (4, 9), (5, 2),
        ];
        let want: Vec<u32> =
            GOLDEN_RLE.iter().flat_map(|&(r, n)| std::iter::repeat_n(r, n)).collect();
        assert_eq!(want.len(), 600, "the golden sequence lost or gained periods");

        let mut l = AutoLadder::new();
        let mut x: u64 = 0x5169_A3B3_C4D5_E6F7; // xorshift64, seeded by the HEAD it was minted on
        for (i, expect) in want.iter().enumerate() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let (loss, jit) = match (x >> 17) % 100 {
                0..=5 => (20.0, 2.0),  // demotes on loss
                6..=9 => (1.0, 30.0),  // demotes on jitter
                10..=19 => (2.0, 8.0), // middling: no move, and resets the streak
                _ => (0.1, 1.0),       // clean
            };
            l.feed_stats(loss, jit);
            assert_eq!(
                l.rung(),
                *expect,
                "tier 0 AUTO diverged from the pre-P4 build at period {i} \
                 (loss {loss}, jitter {jit})"
            );
        }
        assert_eq!(l.rung_changes, 29, "tier 0 changed rungs a different number of times");
    }

    /// The degraded path demotes on backlog and **only after it persists**.
    #[test]
    fn a_streamed_ladder_demotes_after_three_consecutive_backlogged_periods() {
        let mut l = AutoLadder::new_streamed();
        assert_eq!(l.rung(), AUTO_TOP_RUNG_STREAMED, "degraded AUTO did not start at its ceiling");
        let hot = STREAMED_WRITEQ_HOT_MS + 1.0;
        for i in 1..STREAMED_HOT_PERIODS {
            assert_eq!(l.feed_streamed(hot, None), None, "demoted after only {i} hot period(s)");
        }
        assert_eq!(
            l.feed_streamed(hot, None),
            Some(AUTO_TOP_RUNG_STREAMED + 1),
            "three consecutive hot periods must demote exactly one rung"
        );
        // A single clean period breaks the run: the criterion is CONSECUTIVE.
        // Without this the counter is "three hot periods ever", and a link that
        // hiccups once a minute walks to the bottom of the ladder in an hour.
        for _ in 0..(STREAMED_HOT_PERIODS - 1) {
            assert_eq!(l.feed_streamed(hot, None), None);
        }
        assert_eq!(l.feed_streamed(0.0, None), None, "a clean period moved the rung by itself");
        for _ in 0..(STREAMED_HOT_PERIODS - 1) {
            assert_eq!(l.feed_streamed(hot, None), None, "the hot run was not restarted");
        }
    }

    /// Promotion needs ten clean periods **and** a spread the receiver can
    /// absorb; the ceiling is the degraded one, not tier 0's.
    #[test]
    fn a_streamed_ladder_promotes_slowly_and_stops_at_the_degraded_ceiling() {
        let mut l = AutoLadder::new_streamed();
        for _ in 0..(STREAMED_HOT_PERIODS * 3) {
            l.feed_streamed(STREAMED_WRITEQ_HOT_MS + 1.0, None);
        }
        let floor = LADDER.len() as u32 - 1;
        assert_eq!(l.rung(), floor, "setup did not reach the bottom rung");

        // A high spread holds promotion off even with an empty queue: the
        // backlog says we could send more, the spread says the peer is not
        // receiving what we already send on time.
        for i in 0..50 {
            assert_eq!(
                l.feed_streamed(0.0, Some(STREAMED_SPREAD_CLEAN_MS + 1.0)),
                None,
                "promoted on period {i} while the receiver's delay spread was over the line"
            );
        }
        // Spread clears: ten clean periods, then one rung.
        for i in 0..9 {
            assert_eq!(l.feed_streamed(0.0, Some(0.0)), None, "promoted on clean period {i}");
        }
        assert_eq!(l.feed_streamed(0.0, Some(0.0)), Some(floor - 1));

        // All the way up, then stop — at 3, not at tier 0's 2.
        for _ in 0..500 {
            l.feed_streamed(0.0, None);
            assert!(
                l.rung() >= AUTO_TOP_RUNG_STREAMED,
                "degraded AUTO climbed past its ceiling to rung {} — plan §16.3 caps it at {}",
                l.rung(),
                AUTO_TOP_RUNG_STREAMED
            );
        }
        assert_eq!(l.rung(), AUTO_TOP_RUNG_STREAMED);
    }

    /// A ladder built for the degraded transport keeps its ceiling **even if it
    /// is fed the other path's signals**.
    ///
    /// This is why the ceiling is a field rather than each path reading its own
    /// constant: picking the wrong method at a call site is a plain mistake,
    /// and its consequence must not be "AUTO quietly spends 50% more bandwidth
    /// on the transport that can least afford it".
    #[test]
    fn the_ceiling_belongs_to_the_ladder_not_to_the_method() {
        let mut l = AutoLadder::new_streamed();
        for _ in 0..500 {
            l.feed_stats(0.0, 0.0);
        }
        assert_eq!(l.rung(), AUTO_TOP_RUNG_STREAMED, "tier 0 signals lifted a degraded ladder");
        assert_eq!(l.top_rung(), AUTO_TOP_RUNG_STREAMED);
        assert_eq!(AutoLadder::new().top_rung(), AUTO_TOP_RUNG);
    }

    /// A missing `spread_ms` is read as clean, deliberately.
    ///
    /// Injection control: make `feed_streamed` treat `None` as "over the line"
    /// and this goes red — which is the behaviour where a peer that never
    /// reports the field pins the rung for the whole session. Absence of a
    /// measurement is not a measurement of trouble; the repo has paid for that
    /// confusion before (`jb_underruns = 0` on a direction that cannot observe).
    #[test]
    fn an_unreported_spread_does_not_block_promotion_forever() {
        let mut l = AutoLadder::new_streamed();
        for _ in 0..STREAMED_HOT_PERIODS {
            l.feed_streamed(STREAMED_WRITEQ_HOT_MS + 1.0, None);
        }
        assert_eq!(l.rung(), AUTO_TOP_RUNG_STREAMED + 1);
        for _ in 0..9 {
            assert_eq!(l.feed_streamed(0.0, None), None);
        }
        assert_eq!(l.feed_streamed(0.0, None), Some(AUTO_TOP_RUNG_STREAMED));
    }

    /// `rung_of` 不做就近吸附；**采样率单独不再能标识一档**。
    #[test]
    fn a_format_outside_the_ladder_is_refused_not_snapped() {
        assert_eq!(rung_of(48000, WireDepth::F32), Some(0));
        assert_eq!(rung_of(48000, WireDepth::S24), Some(1));
        assert_eq!(rung_of(48000, WireDepth::S16), Some(2));
        assert_eq!(rung_of(32000, WireDepth::S16), Some(3));
        assert_eq!(rung_of(24000, WireDepth::S16), Some(4));
        assert_eq!(rung_of(16000, WireDepth::S16), Some(5));
        // 48 kHz 在阶梯上出现三次 ⇒ 只给速率查不到唯一一格，这正是
        // `rung_of_rate` 必须消失的理由。
        for bad in [
            (32000, WireDepth::S24),
            (16000, WireDepth::F32),
            (44100, WireDepth::S16),
            (0, WireDepth::S16),
            (96000, WireDepth::F32),
        ] {
            assert_eq!(rung_of(bad.0, bad.1), None, "{bad:?} 不在阶梯上，不该给出格号");
        }
        // 往返：每一格都查得回自己。
        for (i, f) in LADDER.iter().enumerate() {
            assert_eq!(rung_of(f.rate_hz, f.depth), Some(i as u32));
        }
    }

    /// **阶梯上线的 codec 集合一变，协议版本号必须跟着变。**
    ///
    /// 这条是 `PROTOCOL_VERSION` 与线上字节之间唯一的机械耦合。它挡的不是
    /// 「加了个新 codec」，而是「加了个新 codec **却没升版本号**」——那次
    /// 改动的失效形态取决于新值撞不撞上老对端认识的枚举值：
    ///
    /// - 撞不上（如 `PcmS24le = 3` 之于 v2）⇒ 老对端 `BadCodec` 无日志早退，
    ///   **全程静音、零诊断**；
    /// - 撞上了（如 `PcmF32le = 1` 之于 v2 —— 那个值一直在，只是从没人发过）
    ///   ⇒ 老对端**根本不看 codec**，按 s16 静默错解，满长度垃圾帧全音量播出。
    ///
    /// 两种都不会有任何一处报错，而握手时那一次严格相等比较是唯一的闸门。
    ///
    /// # 判据从「版本号 == 3」改成了「集合被钉住 + 版本号 ≥ 集合冻结时的版本」
    ///
    /// 原来第二条断言写的是 `PROTOCOL_VERSION == 3`。它**对本条要挡的那次改动
    /// 没有贡献任何检测力**：改了集合而忘了升版本，第一条断言（集合被钉死）
    /// 一定会先红。它唯一的实际效果是——**任何一次因为别的理由升版本，都会让
    /// 这条测试变红**，于是修的人被推着去改那个数字，而不是去想集合。
    ///
    /// 2026-08-07 就发生了一次：M8 把版本升到 4（新增 `Kind::Control` /
    /// `MuxKeepalive` 会上线、老对端 `Header::parse` 失败后无日志早退），
    /// codec 集合一个字没动，这条却红了。**这正是本仓「测试是戏剧」那一类的
    /// 前兆**：一条对着无关改动尖叫的守卫，教会人把它当噪声。
    ///
    /// 所以改成：集合照旧钉死（这才是有检测力的那一半），版本号只断言
    /// **不低于**集合被冻结时的那一版。改集合的人会被第一条拦下，而拦下时的
    /// 消息里写着「顺手把下面那个常量也改了」。
    #[test]
    fn changing_the_set_of_wire_codecs_forces_a_protocol_bump() {
        use crate::control::PROTOCOL_VERSION;
        /// 当前这套 codec 集合是在哪一版协议上定型的。**改了下面钉住的集合，
        /// 就必须同时升 `PROTOCOL_VERSION` 并把这个数改成新的版本号。**
        const CODEC_SET_FROZEN_AT: u32 = 3;

        let mut on_wire: Vec<u8> =
            LADDER.iter().map(|f| Codec::for_depth(f.depth) as u8).collect();
        on_wire.sort_unstable();
        on_wire.dedup();
        assert_eq!(
            on_wire,
            vec![Codec::PcmS16le as u8, Codec::PcmF32le as u8, Codec::PcmS24le as u8]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            "阶梯上线的 codec 集合变了：必须升 PROTOCOL_VERSION（当前 {PROTOCOL_VERSION}），\
             并把本测试里的 CODEC_SET_FROZEN_AT 改成新版本号。否则老对端要么静音、\
             要么把新载荷按 s16 错解，两种都零报错"
        );
        assert!(
            PROTOCOL_VERSION >= CODEC_SET_FROZEN_AT,
            "协议版本 {PROTOCOL_VERSION} 比 codec 集合定型时的 {CODEC_SET_FROZEN_AT} 还低"
        );
    }

    /// 每一格的位深都有一个 codec 承载它，且**分包判据只由帧长决定**。
    #[test]
    fn each_rung_has_a_codec_and_only_the_two_deep_rungs_split() {
        for (i, f) in LADDER.iter().enumerate() {
            assert_eq!(
                Codec::for_depth(f.depth).wire_depth(),
                Some(f.depth),
                "rung {i} 的位深没有 codec 承载"
            );
        }
        let split: Vec<usize> = (0..LADDER.len()).filter(|&i| LADDER[i].splits_frame()).collect();
        assert_eq!(split, vec![0, 1], "分包的格变了：分包判据必须只由帧长决定");
        // 钳位：越界的格号落到最低档，不是 panic、不是回绕到 rung 0。
        assert_eq!(rung_format(LADDER.len() as u32), *LADDER.last().unwrap());
        assert_eq!(rung_format(u32::MAX), *LADDER.last().unwrap());
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    fn frame() -> Vec<f32> {
        vec![0.1; 480]
    }

    /// 规格 §7.2 R10：有洞时 `depth()` 高估，`contiguous()` 才是真排队深度。
    /// 这是把「延迟分项用哪个数」这条决定钉死的断言。
    #[test]
    fn depth_overcounts_across_a_hole_but_contiguous_does_not() {
        // 显式整定：本测试量的是 `depth()`/`contiguous()` 的语义，与设定点无关。
        // 走 `new()` 会跟着 `DEFAULT.min_target` 漂，起播条件一变测试就假红。
        let mut jb = JitterBuffer::with_tuning(2, JbTuning { min_target: 2, ..JbTuning::DEFAULT });
        // 先起播，让 next_seq 落在 10
        jb.push(10, frame());
        jb.push(11, frame());
        assert!(jb.pop().is_some());
        assert_eq!(jb.next_seq, Some(11));

        // 11 已在表里，12 缺失，13/14 提前到达
        jb.push(13, frame());
        jb.push(14, frame());
        assert_eq!(jb.depth(), 3, "len() 把洞之后的两帧也算进去了");
        assert_eq!(jb.contiguous(), 1, "从 next_seq=11 起只有一帧是连续的");
    }

    /// 队首本身缺失 = 一个样本都排不上队，哪怕表里堆了一串更晚的帧。
    /// 此时 `depth()` 谎报 30 ms 排队，而下一个 tick 一定 underrun。
    #[test]
    fn a_missing_head_means_zero_queue_however_full_the_map_is() {
        let mut jb = JitterBuffer::with_tuning(2, JbTuning { min_target: 2, ..JbTuning::DEFAULT });
        jb.push(10, frame());
        jb.push(11, frame());
        assert!(jb.pop().is_some()); // 放掉 10，next_seq = 11
        jb.frames.remove(&11); // 制造队首空洞
        jb.push(12, frame());
        jb.push(13, frame());
        jb.push(14, frame());
        assert_eq!(jb.depth(), 3);
        assert_eq!(jb.contiguous(), 0, "队首没到就是 0，不是 3");
    }

    /// 还没起播时，队首就是表里最小的 seq。
    #[test]
    fn before_playback_starts_contiguous_counts_from_the_lowest_seq() {
        let mut jb = JitterBuffer::new(4);
        jb.push(100, frame());
        jb.push(101, frame());
        jb.push(103, frame());
        assert_eq!(jb.next_seq, None, "还没起播");
        assert_eq!(jb.depth(), 3);
        assert_eq!(jb.contiguous(), 2, "100,101 连续，103 之前有洞");
    }

    #[test]
    fn an_empty_buffer_is_zero_on_both_readings() {
        let jb = JitterBuffer::new(2);
        assert_eq!(jb.depth(), 0);
        assert_eq!(jb.contiguous(), 0);
    }

    /// 无洞时两者必须一致——否则 UI 会看到两个互相矛盾的深度。
    #[test]
    fn contiguous_equals_depth_when_nothing_is_missing() {
        let mut jb = JitterBuffer::new(2);
        for seq in 0..6 {
            jb.push(seq, frame());
        }
        assert!(jb.pop().is_some());
        assert_eq!(jb.depth(), jb.contiguous());
    }

    // ------------------------------------------------ 源侧 depths() 的接线
    //
    // 下面几条**必须真的调用 `FrameSource::depths()`** 并断言它返回的东西。
    // 上一版这里写的是「构造一个 StageDepth 字面量，再断言它等于自己刚写下的
    // 那个 DropMode」——生产代码把 `src_fifo` 标成 `Newest` 它一声不吭，而
    // 那个标签正是「恒定迟到但连续」与「迟到 + 周期性断续」唯一的区分
    // （规格 §0.2：两者深度读数完全简并）。

    /// 站在 cpal 采集回调的位置上写环，站在系统音频后端的位置上交样本。
    /// 环、FIFO、重采样器全是**真的**，只有设备是假的。
    struct FakeSys {
        rate: u32,
        /// 每次 `read` 交出多少样本。
        chunk: usize,
        /// 单调递增的样本值：靠它能分辨 FIFO 里剩下的到底是**早**的还是**晚**
        /// 的那一批，也就是丢弃方向。全填同一个常数就永远看不出来。
        n: u32,
    }

    impl SysAudioCapture for FakeSys {
        fn read(&mut self, out: &mut Vec<f32>) -> usize {
            for _ in 0..self.chunk {
                self.n += 1;
                out.push(self.n as f32);
            }
            self.chunk
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
    }

    fn fake_backend() -> BackendInfo {
        BackendInfo {
            id: "fake".to_string(),
            name: "fake".to_string(),
            available: true,
            excludes_self: true,
            note: String::new(),
        }
    }

    fn sys_source(rate: u32, chunk: usize) -> SysAudioSource {
        SysAudioSource {
            cap: Box::new(FakeSys { rate, chunk, n: 0 }),
            info: fake_backend(),
            resampler: (rate != SysAudioSource::OUT_RATE)
                .then(|| LinearResampler::new(rate, SysAudioSource::OUT_RATE)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        }
    }

    /// `MicSource::depths()` 的两级接线：采集环（**2 秒**、设备速率、丢**最新**）
    /// 与发送 FIFO（1 秒、48k、丢**最旧**）。两级速率不同，绝不能合并成一个数。
    ///
    /// 环与 FIFO 都是真的：环由 `AudioRx::detached_for_test` 造出（与
    /// `LiveCapture::on_device` 同构），FIFO 由真正跑一遍 `next_frame()` 填出。
    #[test]
    fn mic_source_reports_a_2s_capture_ring_and_a_1s_send_fifo() {
        // 设备速率故意取 44100：采集环那一级若被硬写成 48000，ms 会偏 −8.8%。
        let (rx, mut feed) = AudioRx::detached_for_test(44_100);
        let mut mic = MicSource {
            rx,
            resampler: Some(LinearResampler::new(44_100, MicSource::OUT_RATE)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };

        // 声卡交来 4410 个样本（100 ms @44.1k），还没被取走。
        assert_eq!(feed.write(&vec![0.5; 4_410]), 4_410);
        let [cap, fifo] = mic.depths();
        let cap = cap.expect("麦克风源必须报采集环这一级");
        assert_eq!(cap.id, StageId::CapRing);
        assert_eq!(cap.samples, 4_410, "环里此刻积着的就是刚写进去的那些");
        assert_eq!(cap.capacity, 88_200, "2 秒 @44.1k —— 不是 1 秒（规格 §0.4）");
        assert_eq!(cap.rate, 44_100, "采集环走**设备**速率，不是 48000");
        assert_eq!(cap.ms(), Some(100.0), "4410 / 44100 = 100 ms");
        assert_eq!(
            cap.drop_mode,
            DropMode::Newest,
            "采集环是 push_slice 短写：丢的是新样本，听感是断续"
        );
        assert_eq!(cap.dropped, Some(0), "还没溢出过 —— 0 是真读数，不是『观测不到』");
        // FIFO 那一级此刻是空的，但它必须**存在**（0 样本 ≠ 这一级不存在）。
        let fifo = fifo.expect("麦克风源必须报发送 FIFO 这一级");
        assert_eq!(fifo.id, StageId::SrcFifo);
        assert_eq!(fifo.samples, 0);
        assert_eq!(fifo.rate, MicSource::OUT_RATE, "FIFO 已经是 48k 了");
        assert_eq!(fifo.capacity, 48_000, "1 秒 @48k");
        assert_eq!(
            fifo.drop_mode,
            DropMode::Oldest,
            "`while len > CAP {{ pop_front() }}` 丢的是最旧的：恒定迟到但连续"
        );

        // 跑一个 tick：环被 pop 排空、样本经重采样进 FIFO、取走一帧 480。
        let mut out = Vec::new();
        mic.next_frame(&mut out);
        assert_eq!(out.len(), 480);
        let [cap, fifo] = mic.depths();
        assert_eq!(cap.unwrap().samples, 0, "AudioRx::pop 全量排空（规格 §0.4）");
        let fifo = fifo.unwrap();
        // 4410 @44.1k -> 48k 约 4800 个样本，取走 480 后剩下的都还压在 FIFO 里。
        assert!(
            (4_000..=4_400).contains(&fifo.samples),
            "重采样后 4800 减去取走的 480，got {}",
            fifo.samples
        );
        assert_eq!(fifo.ms(), Some(fifo.samples as f64 * 1000.0 / 48_000.0));
    }

    /// 采集环溢出丢的是**新**样本，且计数穿过 `depths()` 原样上报。
    /// 这一级溢出丢的是真实音频——它是**音质**指标的输入，不是延迟嫌疑。
    #[test]
    fn mic_source_capture_ring_overflow_is_counted_as_newest_dropped() {
        let (rx, mut feed) = AudioRx::detached_for_test(48_000);
        let mic = MicSource {
            rx,
            resampler: None,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };
        // 环 = 2 秒 = 96000。灌 100000 进去，最后 4000 个写不下。
        let wrote = feed.write(&vec![0.5; 100_000]);
        let cap = mic.depths()[0].expect("采集环这一级");
        assert_eq!(cap.samples as usize, wrote, "写进去多少就积着多少");
        assert_eq!(
            cap.dropped,
            Some(100_000 - wrote as u64),
            "短写的部分必须数得出来 —— 这里以前是全链路的遥测黑洞"
        );
        assert!(cap.dropped.unwrap() > 0, "确实溢出了");
        assert!(cap.saturated());
    }

    /// **任务点名：把 1 秒 FIFO 与 2 秒采集环分别灌满，看遥测报出多少。**
    ///
    /// 这两级都在提供方（mac→win 那条链路的发送端），是「秒级延迟」最可能的
    /// 藏身处之二。下面把真的 `VecDeque` 与真的 `HeapRb` 灌到**恰好容量**，
    /// 断言 `depths()` 报出 1000.0 / 2000.0 ms 而不是沉默。
    ///
    /// 为什么要单独有这一条：稳态下跑 `next_frame()` 观测到的上限是 47_520
    /// （990 ms）——修剪到 48000 之后本 tick 又被取走一帧。那 10 ms 是**相位
    /// 约定**（读数取自「排在这一帧前面的样本数」），不是误差。这条测试把
    /// 「FIFO 真的满着的时候读数是多少」单独钉死，免得 990 这个数字被后人
    /// 当成 1 秒 FIFO 的物理上限。
    #[test]
    fn a_brimming_send_fifo_reads_exactly_one_second_and_the_capture_ring_two() {
        let (rx, mut feed) = AudioRx::detached_for_test(48_000);
        let mut mic = MicSource {
            rx,
            resampler: None,
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
            frame_samples: 480,
            dropped: 0,
        };

        // 1 秒发送 FIFO 装到**恰好**容量。样本是真的、队列是真的。
        mic.fifo.extend(std::iter::repeat(0.3f32).take(MicSource::FIFO_CAP));
        // 2 秒采集环同样装到恰好容量（96000 @48k）。
        assert_eq!(feed.write(&vec![0.3f32; 96_000]), 96_000);

        let [cap, fifo] = mic.depths();
        let cap = cap.expect("采集环这一级");
        let fifo = fifo.expect("发送 FIFO 这一级");
        assert_eq!(fifo.samples, 48_000);
        assert_eq!(
            fifo.ms(),
            Some(1000.0),
            "满载的 1 秒发送 FIFO = 1000 ms —— 这一级要是不报，用户的那一秒就没人说"
        );
        assert!(fifo.saturated());
        assert_eq!(fifo.drop_mode, DropMode::Oldest, "丢最旧 ⇒ 恒定迟到但连续");
        assert_eq!(cap.samples, 96_000);
        assert_eq!(
            cap.ms(),
            Some(2000.0),
            "采集环是 **2 秒**（规格 §0.4 的修正三），满载就是 2000 ms"
        );
        assert!(cap.saturated());
        assert_eq!(cap.drop_mode, DropMode::Newest, "push_slice 短写 ⇒ 迟到 + 断续");

        // 稳态跑一 tick 之后回落到 990 ms —— 相位约定，不是读数变坏。
        let mut out = Vec::new();
        mic.next_frame(&mut out);
        assert_eq!(mic.depths()[1].unwrap().ms(), Some(990.0));
    }

    /// 发送 FIFO 溢出丢的是**最旧**的，饱和时驻留恰好 1.000 秒。
    /// 真的跑 `next_frame()` 把 FIFO 灌爆，而不是手填一个 `samples: 48_000`。
    #[test]
    fn source_fifo_drops_oldest_and_saturates_at_exactly_one_second() {
        // 每 tick 交 5000 个样本、只被取走 480 —— 十几个 tick 就撑爆 1 秒上限。
        let mut src = sys_source(48_000, 5_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            src.next_frame(&mut out);
        }
        let [fifo, second] = src.depths();
        assert!(second.is_none(), "系统音频源只有 FIFO 一级，后端内部缓冲读不到");
        let fifo = fifo.expect("发送 FIFO 这一级");
        assert_eq!(fifo.id, StageId::SrcFifo);
        // 修剪到 CAP=48000，随即本 tick 的 480 被取走 ⇒ 47520 = 990 ms。
        assert_eq!(fifo.samples, 47_520, "贴着 1 秒上限（刚被取走一帧）");
        assert_eq!(fifo.capacity, 48_000);
        assert_eq!(fifo.rate, 48_000);
        assert!(fifo.saturated(), "≥95% 容量");
        assert_eq!(fifo.ms(), Some(990.0), "1 秒 FIFO 被灌满 = 将近 1000 ms 驻留");
        assert_eq!(
            fifo.drop_mode,
            DropMode::Oldest,
            "pop_front 丢最旧：听感是恒定迟到但连续，与播放环的丢最新完全不同"
        );
        // 20 tick 收 100000、放 9600、还剩 47520，其余被丢。
        let dropped = fifo.dropped.expect("源侧的丢弃本进程数得出来");
        assert_eq!(dropped, 100_000 - 9_600 - 47_520);
        assert!(dropped > 0, "没溢出就谈不上方向");

        // **丢的确实是最旧的**：源交出的是 1,2,3,… 的递增序列，取出的这一帧
        // 必须落在序列的**尾部**。若真丢了最新的（`DropMode::Newest`），这里
        // 拿到的会是最开头那 480 个样本。
        src.next_frame(&mut out);
        assert!(
            out[0] > 50_000.0,
            "FIFO 里留下的必须是晚到的样本，got {} —— 丢弃方向反了",
            out[0]
        );
    }

    /// 不溢出时不能凭空记丢弃——`dropped` 的斜率是「稳态产销失配」与「曾被一次
    /// 卡顿灌满」两种病理的唯一区分（规格 §3.3），虚报它就把诊断毁了。
    #[test]
    fn a_source_fifo_within_budget_drops_nothing() {
        let mut src = sys_source(48_000, 480); // 收 480 放 480，收支刚好平衡
        let mut out = Vec::new();
        for _ in 0..50 {
            src.next_frame(&mut out);
        }
        let fifo = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(fifo.dropped, Some(0));
        assert!(!fifo.saturated());
        assert_eq!(fifo.samples, 0, "来多少走多少");
    }

    /// 后端速率不是 48k 时，FIFO 那一级仍然按 **48000** 换算——它在重采样
    /// **之后**。这条与采集环那条（走设备速率）是一对，方向相反，写反了
    /// 任何一个都会静默偏 ±8.8%。
    #[test]
    fn the_send_fifo_converts_at_48k_even_when_the_backend_runs_at_44k1() {
        let mut src = sys_source(44_100, 4_410); // 100 ms @44.1k / tick
        let mut out = Vec::new();
        src.next_frame(&mut out);
        let fifo = src.depths()[0].expect("发送 FIFO 这一级");
        assert_eq!(fifo.rate, 48_000, "FIFO 在重采样之后，是 48k");
        assert_eq!(fifo.ms(), Some(fifo.samples as f64 * 1000.0 / 48_000.0));
        // 100 ms 进来、10 ms 被取走 ⇒ 剩约 90 ms，而不是按 44.1k 算的 98 ms。
        let ms = fifo.ms().unwrap();
        assert!((ms - 90.0).abs() < 2.0, "约 90 ms，got {ms:.2}");
    }

    /// 即时合成的源没有队列——给空数组，不是给 0 样本的假读数。
    #[test]
    fn a_synthesised_source_reports_no_stages_at_all() {
        let t = ToneSource::new(1000.0, 0.5, 48_000, 10);
        assert!(t.depths().iter().all(|s| s.is_none()));
    }
}

#[cfg(test)]
mod water_level_tests {
    //! 抖动缓冲的**水位控制**：工作点、收敛、限速、拼接质量、欠载裕度、自愈。
    //!
    //! 立场：**判据必须能在削过头时变红**。每一条「安全」断言都配一条同结构的
    //! 反向注入（把水位削到理论上会欠载的值、把交叉淡化关成硬切、把限速器
    //! 绕开），并在同一个测试里断言那个注入**确实**被抓到。本项目此前五次
    //! 出现过「摆设测试」，这里不靠人相信。

    use super::*;

    const F: usize = 480; // 一帧 = 10 ms @48k

    /// 三种测试素材，各有分工（**频率选择是承重的，不是随手写的**）：
    ///
    /// | 频率 | 一帧几个周期 | 帧间 NCC | 干什么用 |
    /// |---|---|---|---|
    /// | 1000 Hz | 10.0 | **+1** | 前后帧逐样本相同 ⇒ 拼接必须是恒等变换 |
    /// | **1010 Hz** | 10.1 | **+0.81** | 通用素材：够平滑（硬切会被 C2 抓住），相位差 36° 是真的不连续 |
    /// | 750 Hz | 7.5 | **−1** | 恰好**反相**：任何凸组合都穿过零 ⇒ 抵消保护必须挡住它 |
    ///
    /// 用 1 kHz 当通用素材是测不出东西的——它逐样本相同，拼接退化成恒等；
    /// 用白噪声也不行——噪声本来就满斜率，C2 判据分辨不出硬切。
    fn tone(hz: f64) -> impl Fn(u32) -> Vec<f32> {
        move |idx: u32| {
            let n0 = idx as usize * F;
            (0..F)
                .map(|i| {
                    let t = (n0 + i) as f64 / 48_000.0;
                    (2.0 * std::f64::consts::PI * hz * t).sin() as f32
                })
                .collect()
        }
    }

    fn dc(v: f32) -> impl Fn(u32) -> Vec<f32> {
        move |_| vec![v; F]
    }

    fn max_slope(x: &[f32]) -> f32 {
        x.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
    }

    /// 短时 RMS 的连续性（dB）。窗 2 ms、跳 0.5 ms，邻居取**一个整窗之外**的
    /// 左右两窗。窗必须比被测事件（4 ms 淡化区）窄，否则判据会把自己要抓的
    /// 凹陷平均掉；邻居必须隔开一个整窗，否则 75 % 重叠会让差值恒为 0。
    fn worst_level_step_db(x: &[f32]) -> f32 {
        let (win, hop) = (96usize, 24usize);
        let lv: Vec<f32> = (0..)
            .map(|i| i * hop)
            .take_while(|&s| s + win <= x.len())
            .map(|s| {
                let e: f32 = x[s..s + win].iter().map(|v| v * v).sum();
                (e / win as f32).sqrt().max(1e-9)
            })
            .collect();
        let k = win / hop;
        if lv.len() <= 2 * k {
            return 0.0;
        }
        let db = |v: f32| 20.0 * v.log10();
        (k..lv.len() - k)
            .map(|i| (db(lv[i]) - 0.5 * (db(lv[i - k]) + db(lv[i + k]))).abs())
            .fold(0.0f32, f32::max)
    }

    /// 假网络 + **真** `JitterBuffer`。时钟是 `pop()` 的调用次数（生产代码里
    /// 也是它），所以 5 分钟的衰减可以在几毫秒里跑完，走的仍是同一条生产路径。
    struct Sim {
        jb: JitterBuffer,
        next_push: u32,
        gen: Box<dyn Fn(u32) -> Vec<f32>>,
        /// 每个 tick **弹出前**的 `contiguous()` —— 这就是被上报为
        /// `jitter_buf.ms` 的那个量。
        depths: Vec<u32>,
        out: Option<Vec<f32>>,
    }

    impl Sim {
        fn new(cfg: JbTuning, gen: impl Fn(u32) -> Vec<f32> + 'static, collect: bool) -> Sim {
            Sim {
                jb: JitterBuffer::with_tuning(cfg.min_target, cfg),
                next_push: 0,
                gen: Box::new(gen),
                depths: Vec::new(),
                out: collect.then(Vec::new),
            }
        }

        fn push(&mut self, n: usize) {
            for _ in 0..n {
                let f = (self.gen)(self.next_push);
                self.jb.push(self.next_push, f);
                self.next_push += 1;
            }
        }

        /// 一个 tick：先到 `arrivals` 帧，再弹一次。
        fn tick(&mut self, arrivals: usize) {
            self.push(arrivals);
            self.depths.push(self.jb.contiguous());
            let got = self.jb.pop();
            if let (Some(o), Some(g)) = (self.out.as_mut(), got.as_ref()) {
                o.extend_from_slice(g);
            }
        }

        /// 跑到起播为止（每 tick 到一帧）。
        fn warm_up(&mut self) {
            let mut n = 0;
            while self.jb.prebuffering() {
                self.tick(1);
                n += 1;
                assert!(n < 100, "起播不了");
            }
        }

        fn run(&mut self, n: usize) {
            for _ in 0..n {
                self.tick(1);
            }
        }
    }

    fn cfg_min(min_target: u32) -> JbTuning {
        JbTuning { min_target, ..JbTuning::DEFAULT }
    }

    // ============================================================ 工作点

    /// **工作点就是设定点，不是天花板。**
    ///
    /// 这一条钉死本轮削减的全部收益。改动前：`target` 由实测抖动算出 = 2 帧，
    /// 而水位由 `target + 6` 那条单边限幅线唯一决定，实测稳态 7–8 帧
    /// （70–80 ms）—— `target` 对水位没有任何影响力。
    /// 改动后：稳态**预弹出**深度 = `target_effective()`，上界 `+ slack`。
    #[test]
    fn the_steady_operating_point_is_the_setpoint_not_the_ceiling() {
        let cfg = JbTuning::DEFAULT;
        let mut sim = Sim::new(cfg, tone(1010.0), false);
        sim.warm_up();
        sim.run(2_000); // 20 s
        let tail = &sim.depths[sim.depths.len() - 500..];
        let lo = *tail.iter().min().unwrap();
        let hi = *tail.iter().max().unwrap();
        assert_eq!(sim.jb.underruns, 0);
        assert_eq!(
            (lo, hi),
            (cfg.min_target, cfg.min_target),
            "干净链路的稳态预弹出深度必须恰好是设定点 {} 帧（= {} ms）",
            cfg.min_target,
            cfg.min_target * 10
        );
    }

    /// 起播那一次**硬钳到 target**：预缓冲期间攒了多少不该决定起播深度。
    /// 改动前是 `next_seq = frames.keys().next()`（最老的一帧），起播深度 =
    /// 攒了多少，完全不受控。
    #[test]
    fn the_very_first_playout_starts_at_the_target_not_at_whatever_piled_up() {
        let mut sim = Sim::new(JbTuning::DEFAULT, tone(1010.0), false);
        sim.push(12); // 预缓冲期间一口气到 12 帧
        sim.tick(0); // 起播
        assert!(!sim.jb.prebuffering());
        assert_eq!(sim.depths[0], 12, "弹出**之前**表里确实攒了 12 帧");
        assert_eq!(
            sim.jb.contiguous(),
            JbTuning::DEFAULT.min_target - 1,
            "起播深度必须恰好是设定点（这里读的是弹出**之后**，故 −1），\
             而不是攒下的 12 帧"
        );
        // 从整定推，不写死：钳位丢的是「攒下的 − 设定点」。写死一个 10 会在
        // `min_target` 改动时假红，掩盖真正该守的不变量。
        let piled = 12;
        assert_eq!(
            sim.jb.dropped,
            (piled - JbTuning::DEFAULT.min_target) as u64,
            "多出来的 {} 帧必须被记成 dropped —— 静默扔掉音频是遥测黑洞",
            piled - JbTuning::DEFAULT.min_target,
        );
    }

    // ============================================================ 收敛

    /// **积压真的会被吐掉，且是平滑地吐。** 起播后注入 8 帧突发
    /// （= 改动前的实测稳态工作点），断言深度收敛回设定点、全程零欠载、
    /// 且走的是拼接路径而不是硬丢。
    #[test]
    fn a_burst_converges_back_to_the_setpoint_without_underrunning() {
        let cfg = JbTuning::DEFAULT;
        let mut sim = Sim::new(cfg, tone(1010.0), false);
        sim.warm_up();
        sim.push(8);
        sim.run(1_500); // 15 s
        let tail = &sim.depths[sim.depths.len() - 200..];
        let hi = *tail.iter().max().unwrap();
        assert_eq!(sim.jb.underruns, 0, "收敛过程中不许欠载");
        assert!(
            hi <= cfg.min_target + cfg.slack,
            "深度没有收敛：稳态最深 {hi} 帧，设定点 {} + slack {}",
            cfg.min_target,
            cfg.slack
        );
        assert!(
            sim.jb.accel_events >= 4,
            "8 帧积压至少要吐 4 次，实际 {}",
            sim.jb.accel_events
        );
        assert_eq!(
            sim.jb.accel_frames, sim.jb.accel_events,
            "一次收敛净吐一帧"
        );
    }

    /// **抗停顿深度 = `target_effective()` 帧，一帧不多一帧不少。**
    ///
    /// 这条不是从欠载率反推的，是 2026-08-04 在 mac→30-win **真实会话**上用
    /// `SIGSTOP`/`SIGCONT` 精确注入发送端停顿量出来的（阶梯见 `JbTuning::DEFAULT`
    /// 的文档）：发送端停顿 Δ ms ⇒ 对端连续 Δ/10 个 tick 一帧都收不到 ⇒
    /// **恰好 `gap >= target_effective` 时欠载**，少一个 tick 都不欠。
    /// 上机阶梯 12/12 命中，且同一个 Δ = 30 ms 在 `min_target = 4` 下是绿的、
    /// 在 `min_target = 3` 下是红的。
    ///
    /// 之所以必须把它钉成单测：`min_target` 是全链路上唯一一个「改小就立刻省
    /// 延迟」的常数，而它省下的**每一帧都恰好是一次 10 ms 停顿的抗性**。谁把
    /// 这条不变量改松了（比如把重新预缓冲的判据从 `depth() < target` 改成 `<=`，
    /// 或者让 `start_playback` 少钳一帧），延迟表会立刻变好看，代价却要到真实
    /// 链路上、在别人机器有负载的时候才浮出来。
    #[test]
    fn the_stall_tolerance_is_exactly_target_effective_frames() {
        for min_target in [2u32, 3, 4, 5] {
            let cfg = cfg_min(min_target);
            for gap in 1..=(min_target + 2) {
                let mut sim = Sim::new(cfg, tone(1010.0), false);
                sim.warm_up();
                sim.run(50); // 进稳态
                assert_eq!(
                    sim.jb.target_effective(),
                    min_target,
                    "前提：稳态有效目标就是 min_target（惩罚项此刻必须是 0）"
                );
                let before = sim.jb.underruns;
                for _ in 0..gap {
                    sim.tick(0); // 一帧都不到 —— 这就是发送端停顿 gap × 10 ms
                }
                let starved = sim.jb.underruns > before;
                assert_eq!(
                    starved,
                    gap >= min_target,
                    "min_target={min_target}（保护 {} ms）遇到 {} ms 停顿：\
                     期望{}欠载，实际{}欠载",
                    min_target * 10,
                    gap * 10,
                    if gap >= min_target { "" } else { "不" },
                    if starved { "" } else { "不" },
                );
            }
        }
    }

    /// **限速：ρ ≤ 1 %。** 把应急线抬到够不着，只留软档，然后持续超供
    /// （每 20 个 tick 多来一帧 = +5 %，远超 ρ）。断言吐的总量被
    /// `ticks / accel_interval_ticks` 夹住；没有限速器的实现会几乎每 tick
    /// 都吐，差两个数量级。
    #[test]
    fn the_rate_limiter_caps_the_time_compression_at_one_percent() {
        let cfg = JbTuning {
            hard_slack: 100_000,
            max_frames: 100_000,
            ..JbTuning::DEFAULT
        };
        let mut sim = Sim::new(cfg, tone(1010.0), false);
        sim.warm_up();
        let base = sim.jb.accel_events;
        let ticks = 4_000usize; // 40 s
        for i in 0..ticks {
            sim.tick(if i % 20 == 0 { 2 } else { 1 });
        }
        let fired = sim.jb.accel_events - base;
        let cap = ticks as u64 / cfg.accel_interval_ticks + 1;
        assert!(
            fired <= cap,
            "限速器没起作用：{ticks} 个 tick 吐了 {fired} 次，上限 {cap}"
        );
        assert!(fired >= cap - 2, "压力没造出来，测不到限速：{fired} / {cap}");
    }

    /// **应急档不比改动前慢。** 改动前是「一个 tick 里 `while` 循环把超出
    /// `target + 6` 的全部硬丢」；现在是每 tick 吐一帧、每帧带 4 ms 交叉淡化。
    /// 断言 12 帧突发在 10 个 tick（100 ms）内回到应急线以内 —— 与改动前
    /// 同量级的收敛，但没有硬切。
    #[test]
    fn the_emergency_line_sheds_at_least_as_fast_as_the_old_hard_drop() {
        let cfg = JbTuning::DEFAULT;
        let mut sim = Sim::new(cfg, tone(1010.0), false);
        sim.warm_up();
        sim.push(12);
        let hard = sim.jb.target_effective() + cfg.hard_slack;
        let mut used = 0;
        for _ in 0..10 {
            sim.tick(1);
            used += 1;
            if sim.jb.contiguous() <= hard {
                break;
            }
        }
        assert!(
            sim.jb.contiguous() <= hard,
            "10 个 tick 之后仍在应急线之上：{} > {hard}",
            sim.jb.contiguous()
        );
        assert!(used <= 10, "{used} 个 tick 才降下来");
        assert_eq!(sim.jb.underruns, 0);
    }

    // ============================================================ 拼接质量

    /// **收敛不是硬切**（C2 斜率 + C3 电平），且配反向注入。
    ///
    /// - 参考流：同一段素材、同样的 tick 数，把收敛线抬到够不着（一次都不吐）。
    /// - 被测流：默认整定，中途发生真实的平滑收敛。
    /// - **反向注入**：`xfade = 0` —— 那正是**改动前的行为**（无淡化硬丢一帧），
    ///   断言同一条 C2 判据**变红**。没有这一步，一条写错的判据会永远通过。
    #[test]
    fn the_convergence_splice_is_not_a_hard_cut_and_the_criterion_catches_one() {
        let run = |cfg: JbTuning| -> (f32, f32, u64) {
            let mut sim = Sim::new(cfg, tone(1010.0), true);
            sim.warm_up();
            sim.push(4);
            sim.run(600);
            assert_eq!(sim.jb.underruns, 0);
            let o = sim.out.take().unwrap();
            (max_slope(&o), worst_level_step_db(&o), sim.jb.accel_events)
        };
        let (ref_slope, ref_db, ref_accel) = run(JbTuning { slack: 200, ..JbTuning::DEFAULT });
        assert_eq!(ref_accel, 0, "参考组不许吐");

        let (soft_slope, soft_db, soft_accel) = run(JbTuning::DEFAULT);
        assert!(soft_accel > 0, "被测组必须真的吐过");
        assert!(
            soft_slope <= 1.10 * ref_slope,
            "C2 斜率判据：交叉淡化后 {soft_slope:.4} > 1.10 × 参考 {ref_slope:.4}"
        );
        assert!(
            soft_db <= ref_db.max(1.5),
            "C3 电平判据：拼接点附近塌了 {soft_db:.2} dB（等功率律没生效？）"
        );

        let (hard_slope, _, hard_accel) = run(JbTuning { xfade: 0, ..JbTuning::DEFAULT });
        assert!(hard_accel > 0);
        assert!(
            hard_slope > 1.10 * ref_slope,
            "判据是摆设：把交叉淡化关成硬切，C2 仍然通过\
             （硬切 {hard_slope:.4} vs 参考 {ref_slope:.4}）"
        );
    }

    /// 两段完全相关（`NCC = 1`）时拼接是**逐样本恒等**变换 —— 等增益律。
    /// 权重不归一、或者「淡出到静音再淡入」，这条立刻红。
    #[test]
    fn a_fully_correlated_splice_is_sample_identical() {
        // 1 kHz @48k：一帧整 10 个周期 ⇒ 相邻帧逐样本相同 ⇒ NCC = 1。
        let mut sim = Sim::new(JbTuning::DEFAULT, tone(1_000.0), true);
        sim.warm_up();
        sim.push(4);
        sim.run(600);
        assert!(sim.jb.accel_events > 0, "这一路没走到拼接");
        assert_eq!(sim.jb.underruns, 0);
        let want = tone(1_000.0)(0);
        for (i, v) in sim.out.unwrap().iter().enumerate() {
            let r = want[i % F];
            assert!(
                (v - r).abs() <= 1e-6,
                "第 {i} 个样本偏了 {:.2e}：相关时必须逐样本恒等",
                (v - r).abs()
            );
        }
    }

    /// 直流输入拼接后恒等（权重必须归一，`g_a + g_b ≡ 1`）。
    #[test]
    fn a_dc_input_survives_the_splice_unchanged() {
        let mut sim = Sim::new(JbTuning::DEFAULT, dc(0.25), true);
        sim.warm_up();
        sim.push(4);
        sim.run(600);
        assert!(sim.jb.accel_events > 0);
        assert_eq!(sim.jb.underruns, 0, "有欠载就会混进 PLC 的 0.7 衰减，测的就不是拼接了");
        for v in sim.out.unwrap() {
            assert!((v - 0.25).abs() <= 1e-6, "直流被拼接改坏了：{v}");
        }
    }

    /// **抵消保护，配反向注入。**
    ///
    /// 750 Hz 的相邻两帧恰好反相（NCC = −1）。帧粒度的加速把 `tau` 钉死在一整帧
    /// 上，没有 WSOLA 挑相位点的自由度，于是任何凸组合都会在淡化区中点穿过零。
    /// 保护的做法是**先等素材**（`ncc_floor`），等不到再强拼（`ncc_retry_ticks`）。
    ///
    /// - 有保护：收敛被推迟，`accel_deferred` 涨。
    /// - **反向注入**：`ncc_floor = -1.0`（等于关掉保护，永不推迟）⇒
    ///   同一段素材上 C3 电平判据**变红**（实测塌 8.8 dB）。
    ///   没有这一步，一条永远不开火的保护看起来和一条有效的保护一模一样。
    #[test]
    fn anti_phase_material_is_deferred_and_dropping_the_guard_collapses_the_level() {
        let run = |cfg: JbTuning| -> (f32, u64, u64) {
            let mut sim = Sim::new(cfg, tone(750.0), true);
            sim.warm_up();
            sim.push(4);
            sim.run(600);
            assert_eq!(sim.jb.underruns, 0);
            let o = sim.out.take().unwrap();
            (worst_level_step_db(&o), sim.jb.accel_events, sim.jb.accel_deferred)
        };
        let (guarded_db, guarded_accel, deferred) = run(JbTuning::DEFAULT);
        assert!(deferred > 0, "反相素材一次都没被挡下 —— 保护没接上");

        let (naked_db, naked_accel, naked_deferred) = run(JbTuning {
            ncc_floor: -1.0,
            ..JbTuning::DEFAULT
        });
        assert_eq!(naked_deferred, 0, "注入组不该推迟");
        assert!(naked_accel > 0);
        assert!(
            naked_db > 1.5,
            "判据是摆设：关掉抵消保护、拿反相素材去拼，C3 仍然通过（{naked_db:.2} dB）"
        );
        // 有保护的一路仍会在死线上强拼几次（不许因为等不到就永不收敛），
        // 所以它并非零塌陷 —— 它换来的是**次数**少了一个数量级。
        assert!(guarded_accel > 0, "保护不许把收敛完全饿死");
        assert!(
            guarded_accel < naked_accel,
            "死线比限速周期还短 ⇒ 保护只是把每次收敛推迟一点，一次都没挡掉\
             （有保护 {guarded_accel} 次 vs 无保护 {naked_accel} 次）"
        );
        // **诚实记账**：保护降的是凹陷的**频次**，不是**深度**。死线一到照样强拼，
        // 那一次的凹陷和无保护时一模一样（实测两边都是 8.8 dB）。帧粒度下没有
        // 别的办法——真要消灭它得让 `tau` 能偏离整帧，那要求 `pop()` 维护
        // 子帧读指针，是另一个量级的改动。这条断言把这个事实钉住，
        // 免得后人以为保护上了就没凹陷了。
        assert!(
            (guarded_db - naked_db).abs() < 0.1,
            "保护不该改变单次凹陷的深度（{guarded_db:.2} vs {naked_db:.2} dB）"
        );
    }

    // ============================================================ 控制量 = 被控量

    /// **有洞时一帧都不动。** 改动前的 `len() > target + 6` 会在这里删掉洞
    /// 后面的真音频：`next_seq = 1` 未到、表 = {2..10} ⇒ `len() = 9 > 8` ⇒ 删 2，
    /// 而此刻真实排队深度 `contiguous()` 是 **0**；若 1/2 只是乱序、下一 tick
    /// 就到，两者还会因 `seq < next` 被判 late 再丢一次 —— 白扔 20 ms 真音频。
    #[test]
    fn a_hole_at_the_head_freezes_the_latency_control() {
        // 显式 2 帧整定：本测试要复现的是「改动前 `len() > target+6` 会开火」
        // 那个现场，它由 `target=2` + 表里 9 帧构成。跟着 `DEFAULT.min_target`
        // 漂会让起播条件变化、场景本身消失（而不是判据失效）。
        let cfg = JbTuning { min_target: 2, ..JbTuning::DEFAULT };
        let mut jb = JitterBuffer::with_tuning(cfg.min_target, cfg);
        jb.push(0, vec![0.1; F]);
        jb.push(1, vec![0.1; F]);
        jb.pop(); // 起播，next_seq = 1
        jb.frames.remove(&1); // 队首空洞
        for seq in 2..=10 {
            jb.push(seq, vec![0.1; F]);
        }
        assert_eq!(jb.depth(), 9, "改动前这就是那条 len() 判据会开火的现场");
        assert_eq!(jb.contiguous(), 0, "队首没到就是 0");
        let before = jb.dropped;
        for _ in 0..cfg.accel_interval_ticks + 1 {
            jb.pop(); // 限速器早已放行，仍然一帧都不许删
        }
        assert_eq!(
            jb.dropped, before,
            "有洞时删了 {} 帧真音频",
            jb.dropped - before
        );
    }

    /// **洞在队列中段时同样不许收敛** —— 这一条守的是「控制量必须是
    /// `contiguous()` 而不是 `len()`」。
    ///
    /// 上一条（洞在队首）其实被 `frames.remove(&seq)?` 顺带挡住了，所以它对
    /// 判据本身没有约束力。真正区分两个读数的是**洞在中段**：队首在手、
    /// `len() = 9` 越过收敛线，而真实排队深度只有 2。用 `len()` 判据就会在
    /// 这里吃掉仅有的两帧，**下一个 tick 立刻欠载** —— 拿真音频换了一次
    /// 毫无意义的「追延迟」。
    #[test]
    fn a_hole_in_the_middle_must_not_trigger_a_convergence() {
        // 限速器开到全放行，把「该不该收敛」这个决定单独暴露出来。
        let cfg = JbTuning { accel_interval_ticks: 1, min_target: 2, ..JbTuning::DEFAULT };
        let mut jb = JitterBuffer::with_tuning(cfg.min_target, cfg);
        jb.push(0, vec![0.1; F]);
        jb.push(1, vec![0.1; F]);
        jb.pop(); // 起播 ⇒ next_seq = 1
        jb.push(2, vec![0.1; F]);
        for seq in 4..=10 {
            jb.push(seq, vec![0.1; F]); // 3 缺席
        }
        assert_eq!(jb.depth(), 9, "len() 把洞后面的 7 帧也算进来了");
        assert_eq!(jb.contiguous(), 2, "真排队深度只有 seq 1 与 2");
        jb.pop();
        assert_eq!(
            jb.accel_events, 0,
            "真排队深度只有 2 帧却触发了收敛 —— 判据用的是 len() 不是 contiguous()"
        );
        jb.pop();
        assert_eq!(jb.underruns, 0, "两帧本该正常放完；收敛把它们吃掉了就会提前欠载");
    }

    /// 内存上界仍然存在，且与延迟控制线**分开**：只有 `len()` 越过
    /// `max_frames` 才硬丢。两条线分开正是上一条得以成立的前提。
    #[test]
    fn the_memory_bound_still_binds_but_far_above_the_latency_line() {
        let cfg = JbTuning::DEFAULT;
        let mut jb = JitterBuffer::with_tuning(cfg.min_target, cfg);
        jb.push(0, vec![0.1; F]);
        jb.push(1, vec![0.1; F]);
        jb.pop();
        jb.frames.remove(&1);
        for seq in 2..=60 {
            jb.push(seq, vec![0.1; F]);
        }
        jb.pop();
        assert!(
            jb.depth() <= cfg.max_frames,
            "内存上界失守：{} > {}",
            jb.depth(),
            cfg.max_frames
        );
        assert!(cfg.max_frames > cfg.max_target + cfg.hard_slack, "两条线必须分开");
    }

    // ============================================================ 欠载裕度

    /// `(稳态预弹出深度, 能扛住的最长生产侧停顿 tick 数)`。
    /// 「能扛住」= 停顿期间一次欠载都没有。
    fn margin(cfg: JbTuning) -> (u32, usize) {
        let steady = {
            let mut s = Sim::new(cfg, tone(1010.0), false);
            s.warm_up();
            s.run(400);
            *s.depths.last().unwrap()
        };
        let mut survived = 0usize;
        for stall in 0..16 {
            let mut s = Sim::new(cfg, tone(1010.0), false);
            s.warm_up();
            s.run(400);
            let u0 = s.jb.underruns;
            for _ in 0..stall {
                s.tick(0);
            }
            if s.jb.underruns == u0 {
                survived = stall;
            } else {
                break;
            }
        }
        (steady, survived)
    }

    /// **裕度是构造性的，不是经验性的。**
    ///
    /// 稳态预弹出深度 `D` 帧 ⇒ 弹出后手上还剩 `D−1` 帧 ⇒ 生产侧停顿
    /// `D−1` 个 tick 之内一次欠载都不会有，第 `D` 个 tick 必然欠载。
    /// **两侧都断言**：只证「不欠载」的测试在水位被削到 0 时照样通过，
    /// 只证「会欠载」的测试在水位被定到 12 帧时照样通过。等号才有信息量。
    #[test]
    fn the_stall_margin_is_exactly_the_steady_depth_minus_one_frame() {
        for min_target in [2u32, 3, 5] {
            let cfg = cfg_min(min_target);
            let (steady, survived) = margin(cfg);
            assert_eq!(steady, min_target, "工作点不是设定点");
            assert_eq!(
                survived,
                steady as usize - 1,
                "设定点 {min_target}：稳态 {steady} 帧，实测扛住 {survived} 个 tick 的停顿"
            );
        }
    }

    /// **注入：把水位削到会欠载的值，测试必须变红。**
    ///
    /// 拿一个固定长度的停顿去打三种整定：默认（2 帧）扛得住 1 个 tick，
    /// 被削到 1 帧的**扛不住**；而默认整定在 2 个 tick 的停顿下**也扛不住**
    /// —— 最后这一条是本轮削减的代价，明写在测试里，不藏。
    /// 谁把 `min_target` 或 `slack` 再调小而忘了这个代价，本测试立刻红。
    #[test]
    fn shaving_the_setpoint_below_the_stall_length_does_produce_underruns() {
        let hit = |cfg: JbTuning, stall: usize| -> u64 {
            let mut s = Sim::new(cfg, tone(1010.0), false);
            s.warm_up();
            s.run(400);
            let u0 = s.jb.underruns;
            for _ in 0..stall {
                s.tick(0);
            }
            s.jb.underruns - u0
        };
        // 不变量：稳态 D 帧 ⇒ 恰好扛得住 D−1 个 tick 的停顿，第 D 个必欠载。
        // 参数化跑，别把某一个 `min_target` 的值焊死在断言里。
        for d in [1u32, 2, 3, 5] {
            assert_eq!(
                hit(cfg_min(d), (d - 1) as usize),
                0,
                "设定点 {d} 帧该扛得住 {} 个 tick 的停顿",
                d - 1,
            );
            assert!(
                hit(cfg_min(d), d as usize) > 0,
                "设定点 {d} 帧居然扛住了 {d} 个 tick 的停顿？判据是摆设"
            );
        }
        // 现行默认同样受这条约束 —— 这就是本轮削减的代价，明写不藏。
        let dflt = JbTuning::DEFAULT.min_target;
        assert_eq!(hit(JbTuning::DEFAULT, (dflt - 1) as usize), 0);
        assert!(
            hit(JbTuning::DEFAULT, dflt as usize) > 0,
            "默认设定点 {dflt} 帧扛住了 {dflt} 个 tick 的停顿？判据是摆设"
        );
    }

    /// **削过头会自己长回来 —— 整轮削减的安全性押在这条上。**
    ///
    /// 目标不是靠我们猜 `R`（净速率误差）猜对的，是靠这条回路测出来的。
    /// 场景：链路每 200 个 tick 就来一次 2 tick 的生产侧停顿，而默认整定
    /// （2 帧）扛不住。断言惩罚项把目标抬起来，且抬到之后**同样的停顿不再
    /// 造成欠载**。
    #[test]
    fn an_underrun_raises_the_setpoint_until_the_same_stall_stops_hurting() {
        let cfg = JbTuning::DEFAULT;
        let mut s = Sim::new(cfg, tone(1010.0), false);
        s.warm_up();
        let t0 = s.jb.target_effective();
        let mut per_round = Vec::new();
        for _ in 0..6 {
            let u0 = s.jb.underruns;
            s.run(200);
            for _ in 0..2 {
                s.tick(0);
            }
            s.run(40);
            per_round.push(s.jb.underruns - u0);
        }
        let t1 = s.jb.target_effective();
        assert!(t1 > t0, "欠载没有抬高目标：{t0} -> {t1}");
        assert_eq!(
            per_round.last().copied().unwrap(),
            0,
            "自愈失败：抬到 {t1} 帧之后同样的停顿仍在欠载（逐轮 {per_round:?}）"
        );
        // 5 分钟无欠载之后才退一帧 —— 慢是故意的，代价已经付过一次了。
        s.run(cfg.extra_decay_ticks as usize / 2);
        assert_eq!(s.jb.target_effective(), t1, "惩罚项退得太快");
        s.run(cfg.extra_decay_ticks as usize / 2 + 10);
        assert_eq!(s.jb.target_effective(), t1 - 1, "惩罚项不退了？那就成了棘轮");
    }

    /// 惩罚项有上界，不会因为一串欠载把延迟推到天上。
    #[test]
    fn the_penalty_is_bounded() {
        let cfg = JbTuning { extra_max: 3, ..JbTuning::DEFAULT };
        let mut s = Sim::new(cfg, tone(1010.0), false);
        s.warm_up();
        for _ in 0..30 {
            s.run(20);
            for _ in 0..3 {
                s.tick(0);
            }
        }
        assert!(s.jb.underrun_penalty() <= cfg.extra_max, "惩罚项越界：{}", s.jb.underrun_penalty());
        assert!(s.jb.target_effective() <= cfg.max_target);
    }

    /// 默认值就是文档里那几个数（改一个就该有人看见），以及 `from_env` 的
    /// 下限保护：内存上界永远压不到延迟控制线以下。
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let d = JbTuning::DEFAULT;
        assert_eq!(
            d.min_target, 4,
            "设定点 4 帧 = 40 ms —— **实测**定的（见 DEFAULT 的文档表）。\n\
             不是 update_target 算出的 2：那个值来自 RFC 3550 一阶差分抖动，\n\
             对突发失明（实测 p95 0.2 ms 而真实需要 40 ms）。收敛到 2 实测欠载 3.75 次/min。"
        );
        assert_eq!(d.slack, 1, "死区 1 帧");
        assert_eq!(d.accel_interval_ticks, 100, "ρ = 1 %");
        assert_eq!(d.hard_slack, 6, "应急线 = 改动前那条 target+6");
        assert_eq!(d.extra_decay_ticks, 30_000, "5 分钟 ⇒ 稳态欠载率 ≤ 0.2 次/分钟");
        assert_eq!(d.xfade, 192, "4 ms，与 trim::X 同值");
        assert_eq!(d.ncc_floor, -0.2, "抵消保护线");
        assert_eq!(d.max_frames, 24, "内存上界必须严格高于应急延迟线（12+6）");
        assert_eq!(
            d.extra_max, 2,
            "惩罚上界：有效目标最深 min_target+extra_max = 6 帧，**必须 < 改动前的\n\
             有效天花板 target+hard_slack = 8 帧** —— 新机制不许比它替换的那个更深。"
        );
        assert!(
            d.min_target + d.extra_max < JbTuning::DEFAULT.hard_slack + 2,
            "回归护栏：{}+{} 已经够到改动前的天花板 2+{} 帧了",
            d.min_target, d.extra_max, JbTuning::DEFAULT.hard_slack,
        );
        let t = JbTuning::from_env();
        assert!(t.max_frames >= t.max_target + t.hard_slack + 1);
    }

    /// The tier 1/2 profile: the documented numbers, and the two invariants
    /// that make them mean anything.
    ///
    /// The compile-time assertions next to `DEGRADED` cover the constant. This
    /// covers the **constructed** value, because `degraded_from_env` can be
    /// handed anything: `AUDIOHUB_JB_DEGRADED_MAX_TARGET=200` must still come
    /// back with a memory ceiling above the latency-control line, exactly as
    /// `from_env` guarantees for tier 0.
    #[test]
    fn the_degraded_profile_is_the_documented_one_and_stays_self_consistent() {
        let d = JbTuning::DEGRADED;
        assert_eq!(
            d.max_target, 40,
            "400 ms is the whole reason this profile exists: a TCP RTO is >=200 ms (Linux) or \
             300 ms (Windows), so DEFAULT's 120 ms ceiling means every retransmission underruns"
        );
        assert_eq!(
            d.extra_max, 36,
            "the underrun penalty is what FINDS the depth; capping it below max_target - \
             min_target makes the raised ceiling unreachable and therefore decorative"
        );
        assert_eq!(d.max_frames, 48, "memory ceiling must clear max_target + hard_slack + 1 = 47");
        assert_eq!(
            d.underrun_step,
            JbTuning::DEFAULT.underrun_step,
            "the penalty LOOP is unchanged by design; the profile only changes what it may reach"
        );
        assert_eq!(
            (d.min_target, d.slack, d.hard_slack, d.accel_interval_ticks, d.extra_decay_ticks),
            (
                JbTuning::DEFAULT.min_target,
                JbTuning::DEFAULT.slack,
                JbTuning::DEFAULT.hard_slack,
                JbTuning::DEFAULT.accel_interval_ticks,
                JbTuning::DEFAULT.extra_decay_ticks
            ),
            "only the three fields above are supposed to differ from DEFAULT"
        );
        let t = JbTuning::degraded_from_env();
        assert!(
            t.max_frames >= t.max_target + t.hard_slack + 1,
            "degraded_from_env broke the invariant from_env enforces: {} < {} + {} + 1",
            t.max_frames,
            t.max_target,
            t.hard_slack
        );
        // The promotion gate AUTO uses on this transport is derived from this
        // profile, not written down twice.
        assert_eq!(STREAMED_SPREAD_CLEAN_MS, d.min_target as f64 * FRAME_MS as f64);
    }
}

#[cfg(test)]
mod zero_alloc_wire_tests {
    //! `seal_into` / `encode_into` 是为了让 `tx_loop` 每 tick 不再 `malloc`
    //! （`docs/spec-latency-floor.md` §9.3 手段 J1 的第 3 项）而加的。
    //!
    //! **两份实现就是两份线格式**，所以这里不测「看起来对」，只测两件事：
    //! ① 与原实现逐字节相同；② 复用缓冲时真的不再重新分配。

    use super::*;
    use crate::packet::{Codec, Kind};

    fn hdr(seq: u32, len: usize) -> Header {
        Header {
            kind: Kind::Media,
            codec: Codec::PcmS16le,
            channels: 1,
            sample_rate: 48000,
            session_id: 7,
            stream_id: 7,
            seq,
            timestamp_us: 123_456,
            payload_len: len as u32,
        }
    }

    /// **逐字节相同**。注入对照：把 `seal_into` 里的 `out.extend_from_slice(&tag)`
    /// 挪到 `encode_into` 之前（= 标签跑到密文前面），本条立刻变红。
    #[test]
    fn sealing_in_place_produces_the_very_same_datagram() {
        let mc = MediaCrypto::new_for_stream(&[9u8; 32], 7, b"salt-16-bytes!!!");
        let mut buf = Vec::new();
        for seq in 0..4u32 {
            let plain: Vec<u8> = (0..960u32).map(|i| (i.wrapping_mul(seq + 1)) as u8).collect();
            let want = mc.seal(&hdr(seq, plain.len()), &plain).expect("seal");
            mc.seal_into(&hdr(seq, plain.len()), &plain, &mut buf)
                .expect("seal_into");
            assert_eq!(buf, want, "seq {seq}：就地封包与原实现产出的字节不同");
            // 而且它必须真的能被解开（对拍相同还不够：两边一起错也会相同）。
            let (h, pt) = mc.open(&buf).expect("open");
            assert_eq!(h.seq, seq);
            assert_eq!(pt, plain);
        }
    }

    /// **复用缓冲之后不再分配**：容量与首地址都不许变。
    ///
    /// 注入对照：把 `seal_into` 开头的 `out.clear()` 换成
    /// `*out = Vec::new()`（= 每次重新分配），`ptr` 断言变红。
    #[test]
    fn a_reused_buffer_stops_reallocating_after_the_first_frame() {
        let mc = MediaCrypto::new_for_stream(&[1u8; 32], 3, b"0123456789abcdef");
        let plain = vec![0u8; 960];
        let mut buf = Vec::new();
        mc.seal_into(&hdr(0, plain.len()), &plain, &mut buf).unwrap();
        let (cap, ptr) = (buf.capacity(), buf.as_ptr());
        for seq in 1..64u32 {
            mc.seal_into(&hdr(seq, plain.len()), &plain, &mut buf).unwrap();
            assert_eq!(buf.capacity(), cap, "seq {seq}：缓冲被重新分配了");
            assert_eq!(buf.as_ptr(), ptr, "seq {seq}：缓冲搬家了 = 一次 malloc");
        }
    }

    /// `Header::encode_into` 与 `Header::encode` 逐字节相同（含载荷拼接）。
    #[test]
    fn encoding_a_header_in_place_matches_the_allocating_form() {
        let payload = [1u8, 2, 3, 4, 5];
        let h = hdr(11, payload.len());
        let mut out = Vec::new();
        h.encode_into(&payload, &mut out);
        assert_eq!(out, h.encode(&payload));
        // 复用：第二次写短载荷必须把上一次的尾巴清掉，不能残留。
        h.encode_into(&[], &mut out);
        assert_eq!(out, h.encode(&[]), "encode_into 没有清空 out");
    }
}
