//! Post-verify encryption upgrade for the control channel (spec-m4a §2).
//! Callers run M3 verify first, then establish_* on the same TcpStream.

use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::*;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use audiohub_core::latency::{DevLatency, DropMode};

use crate::control::{read_frame, write_frame, ControlMsg, CONTROL_MAX_FRAME};
use crate::identity::{verify_sig, LocalIdentity, PairedPeer};

const SEC_LABEL_I: &[u8] = b"audiohub-sec-i";
const SEC_LABEL_R: &[u8] = b"audiohub-sec-r";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Length of the per-stream media salt carried by OpenStream.
pub const MEDIA_SALT_LEN: usize = 16;

/// Fresh `media_salt_b64` for an OpenStream (16 random bytes, base64).
pub fn new_media_salt_b64() -> String {
    let mut salt = [0u8; MEDIA_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    BASE64_STANDARD.encode(salt)
}

/// Decode a peer-supplied `media_salt_b64`. Rejects anything but exactly
/// MEDIA_SALT_LEN bytes so a peer cannot force a short/empty HKDF salt.
pub fn decode_media_salt(media_salt_b64: &str) -> Result<[u8; MEDIA_SALT_LEN]> {
    let raw = b64d(media_salt_b64).context("media_salt_b64")?;
    raw.try_into()
        .map_err(|_| anyhow!("media_salt_b64 must decode to {MEDIA_SALT_LEN} bytes"))
}

/// 对端管线上**一级**缓冲的瞬时读数（`StageReport` 的载荷单元，规格 §3.5）。
///
/// 与 IPC 的 `PipelineStage` 是两个类型，故意的：`audiohub-ipc` 依赖本 crate，
/// 反向依赖不成立；而且这两层要回答的问题不同——IPC 那层是「给 UI 看什么」，
/// 这一层是「给对端**足够重算一遍**的原始量」。
///
/// ## 为什么线上没有 `ms`
///
/// `ms = samples * 1000 / rate` 由**接收方**自己算，不收对端算好的数。
/// 规则只有一条真值：`rate == 0` ⇒ 这一级读不到 ⇒ `None`（绝不当 0）。若把 `ms`
/// 放上线，一个 `{rate: 0, ms: 0.0}` 的报文就能把「测不到」伪装成「没有延迟」，
/// 而那正是整套遥测明令禁止的 0 填补。少一个字段就少一个说谎面。
///
/// `drop_mode` **没有** serde default：缺了它整条 `StageReport` 反序列化失败、
/// 被 `recv_timeout` 跳过、`confidence` 停在 `LocalOnly`——这是安全的方向。
/// 规格 §0.2 已证明四个 1 秒 FIFO 饱和时的深度读数完全简并，只能靠这个标签区分，
/// 给它编一个默认值等于替对端瞎猜听感。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReading {
    /// 稳定级 id（`audiohub_core::latency::StageId::as_str()` 的取值）。
    /// 收到本版本不认识的 id 是**正常的**（对端更新）：照样展示、照样按串联
    /// 计入总数，保守方向。
    pub id: String,
    pub samples: u32,
    /// 该级容量；0 = 无界 / 不适用。
    #[serde(default)]
    pub capacity: u32,
    /// 该级**消费者**的标称速率(Hz)。`0` = 这一级读不到。
    pub rate: u32,
    /// 会话累计丢弃样本数。`None` = 对端观测不到这一级的丢弃，**不是没丢过**。
    #[serde(default)]
    pub dropped: Option<u64>,
    pub drop_mode: DropMode,
    #[serde(default)]
    pub saturated: bool,
    #[serde(default)]
    pub drift_sps: Option<f64>,
}

/// 对端在**它那一侧**测到的音质原料（`StageReport` 的可选载荷）。
///
/// # 为什么必须回传，而不是各看各的
///
/// 音质三分量（遮蔽率 Q1 / 削顶 Q2 / 带宽 Q3）本质上是**接收侧**的概念：
/// PLC、欠载、静音填充只在收端发生。于是一台纯发送的机器对自己这条流的音质
/// **没有定义**——`build_quality` 需要一个抖动缓冲，而发送侧没有。
///
/// 实测后果：`[spk/send] origin=hal quality = None`，界面上音质那一格永远空着。
/// 这与本项目栽过的 `jb_underruns = 0` 假象**是同一个病**：只看本机方向，
/// 就会把「我这侧无从观测」误当成「链路无损」。真正的音质在对端的接收会话上。
///
/// # 为什么送原料而不是送等级
///
/// 与 [`StageReading`] 同一条理由：**给对端足够重算一遍的量**，评级规则在收方
/// 本机执行。等级是我们的口径，收方的 build 可能有不同的门限；把等级放上线，
/// 两端就会对同一条流给出不同的字，而谁也说不清哪个对。
///
/// 缺席语义与本机侧逐字相同：`clip_ratio` 的 `None` 是「**还没测**」，
/// 不是「测了，一个越界样本都没有」——这条红线在线上也必须活着，所以它是
/// `Option` 而不是一个填了 0 的 `f64`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReading {
    /// 滚动窗口的真实跨度（秒）。
    pub window_s: f64,
    /// Q1 原料。
    pub conceal_ratio: f64,
    pub plc_ticks: u64,
    pub silence_ticks: u64,
    pub popped_ticks: u64,
    pub underruns: u64,
    pub jb_dropped: u64,
    /// Q2 原料。`None` = 本窗口还没攒够一整页，**不是 0**。
    #[serde(default)]
    pub clip_ratio: Option<f64>,
    #[serde(default)]
    pub clip_excess_db: Option<f64>,
    /// Q3 原料：对端实际收到的音频带宽（Hz，= 线上采样率 / 2）。
    pub bandwidth_hz: u32,
    /// 对端实际收到的**线上采样率**（Hz），取自媒体包头 `h.sample_rate`。
    ///
    /// # 为什么与 `bandwidth_hz` 并存，而不是让读方 ×2 推出来
    ///
    /// 今天 `bandwidth_hz ≡ wire_rate_hz / 2` 是一条恒等式（Q3 是从阶梯格号算出
    /// 来的标称值，树里没有任何频谱分析）。既然如此，"读方 ×2" 现在能得到正确
    /// 答案 —— **但那是把奈奎斯特关系又刻了一份在读方**。规格 §4.2 允许 Q3 将来
    /// 换成真实频谱测量；那一天恒等式失效，而所有 ×2 的读方会把「2 × 实测带宽」
    /// 当成采样率报出去，并且**没有任何一处会报错**。这正是本项目反复栽的那种
    /// 「看起来对、其实是编的」。所以线上采样率是一等原料，与带宽各走各的。
    ///
    /// `#[serde(default)]` ⇒ 旧对端省略它，收方得到 0。收方**不许**把 0 当采样率
    /// 用；`grade_peer_quality` 只在这一种情况下回退到 `bandwidth_hz * 2`，
    /// 并且那条回退路径带着「这是旧对端」的注释，不会扩散到别处。
    #[serde(default)]
    pub wire_rate_hz: u32,
    /// 对端实际收到的**线上位深**，取自媒体包头的 `codec` 字节。
    /// 取值 `"s16" | "s24" | "f32"`；`""` = 旧对端/未知。
    ///
    /// # 为什么是一等字段，而不是让读方从 codec 推
    ///
    /// 理由与上面 `wire_rate_hz` **逐字相同**：今天「从 codec 推位深」算得对，
    /// **正因为算得对它会一直躺着**。让前端做这一步推导，就是在前端复刻一份
    /// codec → 位深的映射表；两处一漂**没有任何地方会报错**，界面只会安静地
    /// 报一个线上从没出现过的位深。
    ///
    /// 缺席语义与 `wire_rate_hz` 的 `0` 是同一套：`""` 是「对面没说」，
    /// **不是「16 位」**。读方**不许**把 `""` 当成 s16 —— 位深进阶梯之前
    /// 线上恒为 16 位，所以那个猜测今天碰巧是对的，而它正是这个字段要消灭的
    /// 那种「看起来对、其实是编的」。
    ///
    /// ⚠ **不报数字 `32`**：`32` 在整数与浮点之间是歧义的，而位深进阶梯这件事
    /// 的全部目的就是消歧。
    #[serde(default)]
    pub wire_depth: String,
    /// 对端是否判定本流与另一路重复（站点级一票否决，规格 §4.4）。
    #[serde(default)]
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMsg {
    OpenStream {
        stream_id: u32,
        kind: String, // "mic" | "spk", from the OpenStream sender's perspective
        dir: String,  // "send" | "recv": media flow relative to the OpenStream sender
        sample_rate: u32,
        channels: u8,
        /// Required. 16 random bytes, base64. The stream OPENER generates one
        /// per stream regardless of direction; both sides feed it to
        /// MediaCrypto::new_for_stream so reopening a stream_id cannot repeat
        /// a media keystream. No serde default: a peer omitting it is invalid.
        media_salt_b64: String,
        #[serde(default)]
        verify_freq: Option<f32>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        freq: Option<f32>,
        /// `source == "sysaudio"` only: which capture backend the provider must
        /// use. Absent = `sysaudio::BACKEND_AUTO`.
        #[serde(default)]
        backend: Option<String>,
        #[serde(default)]
        simulate_loss_pct: Option<f32>,
        /// spk only: mirror the consumer's slider onto the provider's real
        /// default output device (spec-m4b §A2). serde default so a peer that
        /// predates the field still opens streams.
        #[serde(default)]
        volume_sync: bool,
        /// plan §15 的初值，与 [`SessionMsg::SetTransport`] 同名同义
        /// （**按执行器命名**，见那里的文档）。
        ///
        /// 为什么初值和增量两条路都要：只有增量的话，`OpenStream` 到第一条
        /// `SetTransport` 之间有一个窗口，对端跑它自己的默认值——那正是
        /// 「我设的值此刻没有在生效」这一种最难解释的现象。
        #[serde(default)]
        rx_latency: Option<String>,
        #[serde(default)]
        tx_quality: Option<String>,
    },
    AcceptStream {
        stream_id: u32,
    },
    RejectStream {
        stream_id: u32,
        reason: String,
    },
    CloseStream {
        stream_id: u32,
    },
    Stats {
        stream_id: u32,
        received: u64,
        lost: u64,
        loss_pct: f64,
        jitter_ms: f64,
    },
    /// Consumer -> provider: apply this to the provider's real default output
    /// device (spec-m4b §A2). Never applies any gain to the media stream —
    /// volume is a control-plane property.
    ///
    /// `muted` absent/null = volume only, LEAVE the provider's mute control
    /// alone. A bare slider drag must not unmute a machine somebody muted.
    ///
    /// `src` is the frozen anti-ping-pong tag. In practice it is always
    /// `volume::SRC_LOCAL`: a consumer never re-emits a change it received, so
    /// nothing is ever relayed and `volume::SRC_PEER` is never put on the wire
    /// by any AudioHub build. The field stays because the wire shape is frozen
    /// and because the daemon refuses anything but `local`.
    VolumeSet {
        stream_id: u32,
        scalar: f32,
        #[serde(default)]
        muted: Option<bool>,
        src: String,
    },
    /// Provider -> consumer: what the provider's output device actually reads
    /// now. `adjustable=false` means the device has no volume we can drive, so
    /// the consumer shows the value but disables its slider.
    VolumeState {
        stream_id: u32,
        scalar: f32,
        muted: bool,
        adjustable: bool,
    },
    Ping {
        t_us: u64,
    },
    /// `t_us` 原样回抄发起方的 `Ping.t_us`（**发起方时基**，只被发起方读）。
    ///
    /// `peer_t_us` 是**应答方自己**收到那条 Ping 的时刻（应答方时基）。有了它，
    /// 发起方就能按 NTP 的四时戳法估时钟偏移（规格 §3.3 P1）：
    ///
    /// ```text
    /// t1 = 发起方发出 Ping        (发起方时基)
    /// t2 = 应答方收到 Ping        (应答方时基)  ← 就是这个字段
    /// t4 = 发起方收到 Pong        (发起方时基)
    /// θ  = (t1 + t4)/2 − t2       「把对端时戳 + θ 就换算成本机时基」
    /// ```
    ///
    /// 这里省掉了 NTP 的 t3（应答方发出 Pong 的时刻）：`Ping` 在读取线程里被
    /// 同步应答，t3 − t2 是几微秒，而下面的 min-RTT 滤波本来就靠挑最干净的那个
    /// 样本把这一项吃掉。多一个字段换不来精度，只多一处可以说谎的地方。
    ///
    /// ## 为什么是 `Option<u64>` 而不是规格草案写的 `#[serde(default)] u64`
    ///
    /// `u64` 的默认值是 **0**，与「对端真的报了 0」逐位相同。P1 之前的对端不发
    /// 这个字段，于是 θ 会被算成 (t1+t4)/2，一个纯粹的垃圾值——而它长得完全像
    /// 个正常读数。这正是全规格反复禁止的 0 填补。`None` 让「没有这个字段」在
    /// 类型上就无法被当成一个测量值。
    Pong {
        t_us: u64,
        #[serde(default)]
        peer_t_us: Option<u64>,
    },
    /// 双向：把**本侧**这条流的逐级缓冲读数回传给对端（P0b，规格 §3.3）。
    ///
    /// 没有它，任何一端都只知道自己这一半：`sum_ms` 无从合成，`confidence` 只能
    /// 停在 `LocalOnly`。本次故障（mac→win 端到端约 1 秒，发送侧只解释了约
    /// 188 ms）缺的正是对端那一半。
    ///
    /// ## 按流，一条流一份，**永不合并**（规格 §7.2 R8）
    ///
    /// 扇出时一个源被 N 条流引用，物理队列只有一份，于是 N 条流报的
    /// `src_fifo` 是**同一个数**——这是正确的物理事实。由此得出的硬约束是：
    /// 分项只能按流合成，跨流求和会得到 N 倍假延迟。`stream_id` 在这里不是
    /// 路由细节，是那条约束的载体：收方按它落到**那一条**会话的格子里。
    ///
    /// ## 新增变体不需要升协议版本
    ///
    /// 老对端认证通过、反序列化失败，于是被 `recv_timeout` 跳过并计入
    /// `bad_session_msgs`，连接照常存活（见本文件 `Unpaired` 上那段说明与
    /// `recv_timeout` 的实现）。它只是永远停在 `LocalOnly`，不掉线。
    StageReport {
        stream_id: u32,
        /// 发送方本侧各级，按数据流顺序。空列表 = 这条流本侧一级都读不到。
        stages: Vec<StageReading>,
        /// 发送方自己算的本侧 Σ。**收方不拿它当权威**——收方用同一份 `stages`
        /// 按自己的规则重算（并行尾级取 max、`rate==0` 判缺项），只把这个值当
        /// 交叉校验：两端求和口径若分了岔，这里会对不上。
        #[serde(default)]
        local_ms: Option<f64>,
        /// 发送方声卡固有延迟。P0 恒为 `Unavailable`（平台查询是 P1 的活）。
        #[serde(default)]
        dev: Option<DevLatency>,
        /// 发送方**接收侧**的音质原料。`None` 有两种成因，收方不区分：
        /// 对端是不带这个字段的旧版本，或者这条流在对端也是纯发送方向
        /// （于是它那侧同样没有抖动缓冲可测）。两种都只意味着「拿不到」，
        /// 而拿不到必须显示成拿不到 —— 见 [`QualityReading`]。
        ///
        /// `#[serde(default)]` ⇒ 老对端发来的报文照样解析，只是这一格是空的；
        /// 反之老对端收到带这个字段的报文时 serde 忽略未知字段，也不掉线。
        /// 所以这条不需要升 P2P 协议版本。
        #[serde(default)]
        quality: Option<QualityReading>,
        /// 采样时刻，**发送方时基**（µs since its daemon start）。
        ///
        /// ⚠ 只允许与**同一个发送方**的其它 `seq_us` 比较（去重、判乱序）。
        /// 拿它和本机的时钟相减是跨时基运算，得到的是两个 daemon 启动时刻之差
        /// ——一个长得很像「年龄」的垃圾数。读数年龄一律用**本机**收到它的
        /// `Instant` 量（见 `PeerLatCell`）。
        seq_us: u64,
    },
    /// "This is the mode I am in right now" (plan §13 推论 1).
    ///
    /// Sent by BOTH sides immediately after the secure channel is established,
    /// and again on every mode change for as long as the channel lives. Without
    /// it a peer lists a machine sitting in mode A/B as a usable audio device
    /// and only discovers otherwise when its `OpenStream` comes back rejected —
    /// an error at the moment of use, where the interface had promised one.
    ///
    /// ## Why it is *advertised* and not *trusted*
    ///
    /// This message is a UI affordance, not a safety boundary. The exclusion is
    /// enforced by the machine being asked, in `handle_remote_open`, which does
    /// not consult anything the peer said. A peer that lies about its mode gets
    /// exactly what an honest one gets: its own machine still refuses to serve
    /// while it is a consumer, and ours still refuses while we are. Treating
    /// the advertisement as authority would put the relay guard on the wrong
    /// side of the wire.
    ///
    /// ## Why this variant did need a protocol version bump
    ///
    /// Every other variant here is pure addition: an old peer cannot decode it,
    /// `recv_timeout` skips it, the connection survives, and the only loss is
    /// telemetry (see `Unpaired` below, where that guarantee is written down).
    /// Mode is different in kind — the *absence* of this message is not "no
    /// data", it is a peer that predates the exclusion and therefore both
    /// serves and consumes. Degrading silently would leave the §13 relay leg
    /// open with nothing on either screen to say so, so `control.rs` bumped
    /// `PROTOCOL_VERSION` and refuses the handshake instead. This variant is
    /// consequently only ever seen by a peer that understands it.
    ModeState {
        /// `mode::Mode` spelled on the wire (`"share" | "a" | "b"`). Carried as
        /// a string rather than the enum so a mode this build does not know
        /// arrives as itself: an unknown mode must be reported as unknown, and
        /// deserialising into `Mode` would fail the whole frame and make the
        /// peer look like it never advertised at all.
        mode: String,
    },
    /// "I have unpaired from you." Sent immediately before `Bye` when the local
    /// user removes a pairing while the channel is up (plan §7.1, ruled in
    /// 2026-07-31).
    ///
    /// The refusal at the next verify (`ControlMsg::Unpaired`) covers the peer
    /// that dials US; this covers the peer that never does, which would
    /// otherwise never find out and would keep a pair of virtual devices in our
    /// name in its system list forever.
    ///
    /// 老版本对端**不会**因为这个变体断连：它认证通过、反序列化失败，于是被
    /// `recv_timeout` 跳过并计入 `bad_session_msgs`，连接照常存活（见本文件
    /// `recv_timeout` 里 `Ok(sm) => ... , Err(_) => { bad_session_msgs += 1; continue }`
    /// 那一段）。它只是不知道自己被解除配对了，随后那条 `Bye` 才关掉通道。
    ///
    /// 这条实现给出的兼容性保证，值得写死在这里：**往 `SessionMsg` 里新增变体
    /// 不需要升协议版本**。老对端遇到不认识的变体会跳过它继续跑，不会掉线、
    /// 不会把整条控制通道判死；只有解密失败和分帧错误才允许拆连接。所有
    /// 「新增遥测消息可以单端先上线、混合版本自动降级」的结论都押在这一点上。
    ///
    /// （此处原有注释写的是「老对端解析失败会丢弃通道」，与实现相反。若将来
    /// 有人照那句话把行为改回断连，上面这条保证会静默失效。）
    Unpaired {},
    /// 消费者 -> 提供者：这条流上**执行器在你那一侧**的档位，照办（plan §15）。
    ///
    /// # 字段名说的是执行器，不是用户看到的「收 / 发」
    ///
    /// 两者在这条消息上**恰好相反**：延迟的执行器是接收侧的 jitter buffer，
    /// 质量的执行器是发送侧的阶梯格号。于是消费者的 `send.latency` 要治的是
    /// **对端的 rx**，`recv.quality` 要治的是**对端的 tx**。按用户视角命名的话，
    /// 收方要先回答「他说的 send 是他的 send 还是我的 send」——那个问题没有
    /// 正确答案，而错误的那个答案会让一个方向静默失效、界面全绿。
    ///
    /// 一条流上这两项**至多一项有值**：持 `rx` 的那端没有 `tx`，反之亦然。
    /// 收方对此有硬校验（执行器不在本地就计数拒绝，不静默忽略）。
    ///
    /// # `Option` 而不是 `String`
    ///
    /// `None` = 发送方没有对这一项表态，收方保持现状；`Some("auto")` = 明确
    /// 要求 AUTO。两者不同，且前者正是「只改了延迟没改音质」的增量更新形态。
    /// 用空串表达「未表态」会与「档位串是空的」逐位相同。
    SetTransport {
        stream_id: u32,
        /// 这条流在**你的接收侧**要达到的端到端总延迟目标
        /// （`LatencyTarget::as_wire()`）。只有持 `rx` 的那一端可执行。
        #[serde(default)]
        rx_latency: Option<String>,
        /// 这条流**你的发送侧**的线上质量档（`QualityTarget::as_wire()`）。
        /// 只有持 `tx` 的那一端可执行。
        #[serde(default)]
        tx_quality: Option<String>,
    },
    Bye {},
}

/// Media-plane AEAD keys, mapped to this endpoint's send/recv direction.
#[derive(Clone)]
pub struct MediaKeys {
    pub tx: [u8; 32],
    pub rx: [u8; 32],
}

struct DerivedKeys {
    c_tx: [u8; 32],
    c_rx: [u8; 32],
    media: MediaKeys,
}

impl Drop for DerivedKeys {
    fn drop(&mut self) {
        self.c_tx.zeroize();
        self.c_rx.zeroize();
        self.media.tx.zeroize();
        self.media.rx.zeroize();
    }
}

fn hkdf_expand(hk: &Hkdf<Sha256>, label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    hk.expand(label, &mut out).expect("hkdf expand 32B");
    out
}

fn derive_keys(ss: &[u8], nonce_i: &[u8], nonce_r: &[u8], initiator: bool) -> DerivedKeys {
    let mut salt = Vec::with_capacity(nonce_i.len() + nonce_r.len());
    salt.extend_from_slice(nonce_i);
    salt.extend_from_slice(nonce_r);
    let hk = Hkdf::<Sha256>::new(Some(&salt), ss);
    let mut c_i2r = hkdf_expand(&hk, b"c-i2r");
    let mut c_r2i = hkdf_expand(&hk, b"c-r2i");
    let mut m_i2r = hkdf_expand(&hk, b"m-i2r");
    let mut m_r2i = hkdf_expand(&hk, b"m-r2i");
    let out = if initiator {
        DerivedKeys {
            c_tx: c_i2r,
            c_rx: c_r2i,
            media: MediaKeys { tx: m_i2r, rx: m_r2i },
        }
    } else {
        DerivedKeys {
            c_tx: c_r2i,
            c_rx: c_i2r,
            media: MediaKeys { tx: m_r2i, rx: m_i2r },
        }
    };
    // [u8; 32] is Copy, so the struct above holds copies: wipe the locals.
    c_i2r.zeroize();
    c_r2i.zeroize();
    m_i2r.zeroize();
    m_r2i.zeroize();
    out
}

// frozen preimages (spec-m4a §2)
fn sig_preimage_i(eph_i: &[u8; 32], nonce_i: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SEC_LABEL_I.len() + 32 + nonce_i.len());
    m.extend_from_slice(SEC_LABEL_I);
    m.extend_from_slice(eph_i);
    m.extend_from_slice(nonce_i);
    m
}

fn sig_preimage_r(eph_r: &[u8; 32], nonce_r: &[u8], eph_i: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SEC_LABEL_R.len() + 64 + nonce_r.len());
    m.extend_from_slice(SEC_LABEL_R);
    m.extend_from_slice(eph_r);
    m.extend_from_slice(nonce_r);
    m.extend_from_slice(eph_i);
    m
}

fn b64d(s: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(s)
        .map_err(|e| anyhow!("bad base64: {e}"))
}

fn arr32(v: &[u8]) -> Result<[u8; 32]> {
    v.try_into().map_err(|_| anyhow!("expected 32 bytes"))
}

fn ctr_nonce(n: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&n.to_le_bytes());
    out
}

pub struct SecureChannel {
    stream: TcpStream,
    tx_cipher: ChaCha20Poly1305,
    rx_cipher: ChaCha20Poly1305,
    tx_n: u64,
    rx_seen: Option<u64>, // highest accepted n; anything <= is a replay
    /// Set when a write fails: the peer may have received a partial frame, and
    /// the nonce reserved for it is burned. The channel is unusable afterwards.
    poisoned: bool,
    /// Session messages that decrypted fine but did not parse (unknown variant
    /// from a newer peer, truncated JSON). Counted and skipped, never fatal.
    bad_session_msgs: u64,
    media: MediaKeys,
    peer: PairedPeer,
    rd_buf: Vec<u8>,
}

impl SecureChannel {
    pub fn establish_initiator(
        mut s: TcpStream,
        id: &LocalIdentity,
        peer: &PairedPeer,
    ) -> Result<SecureChannel> {
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let eph_secret = StaticSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph_secret);
        let mut nonce_i = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_i);
        let sig = id.sign(&sig_preimage_i(eph_pub.as_bytes(), &nonce_i));
        write_frame(
            &mut s,
            &ControlMsg::SecInit {
                eph_pub_b64: BASE64_STANDARD.encode(eph_pub.as_bytes()),
                nonce_b64: BASE64_STANDARD.encode(nonce_i),
                sig_b64: BASE64_STANDARD.encode(sig),
            },
        )?;

        let (eph_r_b64, nonce_r_b64, sig_r_b64) = match read_frame(&mut s)? {
            ControlMsg::SecResp { eph_pub_b64, nonce_b64, sig_b64 } => {
                (eph_pub_b64, nonce_b64, sig_b64)
            }
            ControlMsg::Error { message } => bail!("{message}"),
            other => bail!("unexpected message: {other:?}"),
        };
        let eph_r = arr32(&b64d(&eph_r_b64)?)?;
        let nonce_r = b64d(&nonce_r_b64)?;
        let sig_r = b64d(&sig_r_b64)?;
        let m_r = sig_preimage_r(&eph_r, &nonce_r, eph_pub.as_bytes());
        if !verify_sig(&peer.public_key_b64, &m_r, &sig_r) {
            let _ = write_frame(
                &mut s,
                &ControlMsg::Error { message: "secure handshake signature invalid".into() },
            );
            bail!("secure handshake signature invalid");
        }

        let ss = eph_secret.diffie_hellman(&PublicKey::from(eph_r));
        if !ss.was_contributory() {
            bail!("degenerate x25519 shared secret");
        }
        let keys = derive_keys(ss.as_bytes(), &nonce_i, &nonce_r, true);
        Ok(SecureChannel::from_parts(s, keys, peer.clone()))
    }

    pub fn establish_responder(
        mut s: TcpStream,
        id: &LocalIdentity,
        peer: &PairedPeer,
    ) -> Result<SecureChannel> {
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let (eph_i_b64, nonce_i_b64, sig_i_b64) = match read_frame(&mut s)? {
            ControlMsg::SecInit { eph_pub_b64, nonce_b64, sig_b64 } => {
                (eph_pub_b64, nonce_b64, sig_b64)
            }
            ControlMsg::Error { message } => bail!("{message}"),
            other => bail!("unexpected message: {other:?}"),
        };
        let eph_i = arr32(&b64d(&eph_i_b64)?)?;
        let nonce_i = b64d(&nonce_i_b64)?;
        let sig_i = b64d(&sig_i_b64)?;
        if !verify_sig(&peer.public_key_b64, &sig_preimage_i(&eph_i, &nonce_i), &sig_i) {
            let _ = write_frame(
                &mut s,
                &ControlMsg::Error { message: "secure handshake signature invalid".into() },
            );
            bail!("secure handshake signature invalid");
        }

        let eph_secret = StaticSecret::random_from_rng(OsRng);
        let eph_pub = PublicKey::from(&eph_secret);
        let mut nonce_r = [0u8; 16];
        OsRng.fill_bytes(&mut nonce_r);
        let sig = id.sign(&sig_preimage_r(eph_pub.as_bytes(), &nonce_r, &eph_i));
        write_frame(
            &mut s,
            &ControlMsg::SecResp {
                eph_pub_b64: BASE64_STANDARD.encode(eph_pub.as_bytes()),
                nonce_b64: BASE64_STANDARD.encode(nonce_r),
                sig_b64: BASE64_STANDARD.encode(sig),
            },
        )?;

        let ss = eph_secret.diffie_hellman(&PublicKey::from(eph_i));
        if !ss.was_contributory() {
            bail!("degenerate x25519 shared secret");
        }
        let keys = derive_keys(ss.as_bytes(), &nonce_i, &nonce_r, false);
        Ok(SecureChannel::from_parts(s, keys, peer.clone()))
    }

    fn from_parts(stream: TcpStream, keys: DerivedKeys, peer: PairedPeer) -> SecureChannel {
        SecureChannel {
            stream,
            tx_cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.c_tx)),
            rx_cipher: ChaCha20Poly1305::new(Key::from_slice(&keys.c_rx)),
            tx_n: 0,
            rx_seen: None,
            poisoned: false,
            bad_session_msgs: 0,
            media: keys.media.clone(),
            peer,
            rd_buf: Vec::new(),
        }
        // `keys` (and the media copy inside it) is wiped by DerivedKeys::drop.
    }

    pub fn send(&mut self, msg: &SessionMsg) -> Result<()> {
        let plain = serde_json::to_vec(msg).context("serialize session message")?;
        self.send_raw_payload(&plain)
    }

    /// Sends an already-serialized session payload, including shapes this build
    /// cannot construct (a variant from a newer peer). Same nonce discipline as
    /// send(); exists so forward-compat handling is testable.
    #[doc(hidden)]
    pub fn send_raw_payload(&mut self, plain: &[u8]) -> Result<()> {
        if self.poisoned {
            bail!("secure channel unusable after an earlier write failure");
        }
        // Reserve the counter BEFORE encrypting: on any failure below, n is
        // burned rather than reused, because a partial write may already have
        // put ciphertext for this nonce on the wire.
        let n = self.tx_n;
        self.tx_n += 1;
        let ct = self
            .tx_cipher
            .encrypt(Nonce::from_slice(&ctr_nonce(n)), plain.as_ref())
            .map_err(|_| anyhow!("control encrypt failed"))?;
        if let Err(e) = write_frame(
            &mut self.stream,
            &ControlMsg::Enc { n, data_b64: BASE64_STANDARD.encode(ct) },
        ) {
            self.poisoned = true;
            return Err(e).context("write secure control frame");
        }
        Ok(())
    }

    /// None = nothing valid arrived before the timeout. Replayed frames
    /// (n <= highest seen) and unparseable session messages are skipped;
    /// tampering, framing errors and plaintext frames are errors.
    pub fn recv_timeout(&mut self, t: Duration) -> Result<Option<SessionMsg>> {
        if self.poisoned {
            bail!("secure channel unusable after an earlier write failure");
        }
        let deadline = Instant::now() + t;
        loop {
            while let Some(body) = self.take_frame()? {
                // Fixed message on purpose: serde quotes the offending input
                // (e.g. "unknown variant `...`"), which would echo peer text.
                let msg: ControlMsg = match serde_json::from_slice(&body) {
                    Ok(m) => m,
                    Err(_) => bail!("malformed control frame on encrypted channel"),
                };
                match msg {
                    ControlMsg::Enc { n, data_b64 } => {
                        if self.rx_seen.map_or(false, |seen| n <= seen) {
                            continue; // replay
                        }
                        let ct = b64d(&data_b64)?;
                        let pt = self
                            .rx_cipher
                            .decrypt(Nonce::from_slice(&ctr_nonce(n)), ct.as_ref())
                            .map_err(|_| anyhow!("control decrypt failed (tampered frame?)"))?;
                        self.rx_seen = Some(n);
                        match serde_json::from_slice::<SessionMsg>(&pt) {
                            Ok(sm) => return Ok(Some(sm)),
                            Err(_) => {
                                // Authenticated but undecodable (newer peer,
                                // truncated JSON): skip it. Only decrypt and
                                // framing failures may drop the whole conn.
                                self.bad_session_msgs += 1;
                                if self.bad_session_msgs == 1 {
                                    eprintln!(
                                        "[audiohub-net] secure: skipping undecodable session \
                                         message(s) from peer; further ones are only counted"
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    // Post-handshake the peer speaks only Enc. Anything else is
                    // an off-path injection: fail with a FIXED message and never
                    // echo peer-supplied text (a plaintext Error{message} used
                    // to tear the session down and print the attacker's string).
                    _ => bail!("plaintext control frame on encrypted channel"),
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = (deadline - now).max(Duration::from_millis(1));
            self.stream.set_read_timeout(Some(remaining))?;
            let mut tmp = [0u8; 4096];
            match self.stream.read(&mut tmp) {
                Ok(0) => bail!("connection closed by peer"),
                Ok(k) => self.rd_buf.extend_from_slice(&tmp[..k]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) => return Err(e).context("read secure channel"),
            }
        }
    }

    // one length-prefixed frame from rd_buf, if complete
    fn take_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if self.rd_buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.rd_buf[..4].try_into().unwrap()) as usize;
        if len > CONTROL_MAX_FRAME {
            bail!("control frame too large: {len} bytes");
        }
        if self.rd_buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.rd_buf[4..4 + len].to_vec();
        self.rd_buf.drain(..4 + len);
        Ok(Some(body))
    }

    pub fn media_keys(&self) -> MediaKeys {
        self.media.clone()
    }

    pub fn peer(&self) -> &PairedPeer {
        &self.peer
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Next nonce counter to be reserved by send(); also the number of nonces
    /// consumed so far (a failed send burns one).
    pub fn tx_counter(&self) -> u64 {
        self.tx_n
    }

    /// Decrypted-but-undecodable session messages skipped so far.
    pub fn bad_session_msgs(&self) -> u64 {
        self.bad_session_msgs
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

#[cfg(test)]
mod wire_compat_tests {
    use super::*;

    /// `SessionMsg` 在 P0b / P1 之前的形状。整套「新增变体 / 新增字段不必升协议
    /// 版本」的结论就押在这个类型上——所以它必须真的存在于测试里，而不是活在
    /// 注释里。
    ///
    /// 只列到这次改动碰过的两个变体（`Pong` 与「没有 `StageReport`」），其余
    /// 变体在 `control.rs` 的同名测试里已有同构覆盖。
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacySessionMsg {
        Ping { t_us: u64 },
        /// 老版本的 `Pong`：只有 `t_us`。
        Pong { t_us: u64 },
        Bye {},
    }

    fn reading() -> StageReading {
        StageReading {
            id: "src_fifo".into(),
            samples: 48_000,
            capacity: 48_000,
            rate: 48_000,
            dropped: Some(7),
            drop_mode: DropMode::Oldest,
            saturated: true,
            drift_sps: Some(0.0),
        }
    }

    /// **老对端收到新 `Pong`**：多出来的 `peer_t_us` 被 serde 忽略，照常解析。
    /// 若谁给 `SessionMsg` 加了 `deny_unknown_fields`，这条会立刻变红。
    #[test]
    fn a_peer_that_predates_peer_t_us_still_parses_our_pong() {
        let json = serde_json::to_string(&SessionMsg::Pong {
            t_us: 123,
            peer_t_us: Some(456),
        })
        .unwrap();
        match serde_json::from_str::<LegacySessionMsg>(&json).expect("老对端必须解析得动") {
            LegacySessionMsg::Pong { t_us } => assert_eq!(t_us, 123),
            other => panic!("解析成了 {other:?}"),
        }
    }

    /// **新端收到老 `Pong`**：缺字段 ⇒ `None`，**不是 `Some(0)`**。
    ///
    /// 这条是 0 填补禁令在协议层的执行点。把字段改回规格草案里的
    /// `#[serde(default)] peer_t_us: u64` 会让它变成 0，而 0 是一个完全合法的
    /// 时戳取值——θ 会被算成 (t1+t4)/2，一个长得像正常读数的垃圾。
    #[test]
    fn an_old_pong_yields_none_not_a_zero_timestamp() {
        let legacy = serde_json::to_string(&LegacySessionMsg::Pong { t_us: 99 }).unwrap();
        match serde_json::from_str::<SessionMsg>(&legacy).expect("新端必须解析得动老报文") {
            SessionMsg::Pong { t_us, peer_t_us } => {
                assert_eq!(t_us, 99);
                assert!(
                    peer_t_us.is_none(),
                    "老对端没有这个字段 ⇒ 必须是 None，绝不能落成一个可用的 0"
                );
            }
            other => panic!("解析成了 {other:?}"),
        }
    }

    /// **老对端收到 `StageReport`**：解析失败。这正是我们要的——`recv_timeout`
    /// 把它计入 `bad_session_msgs` 并跳过，连接存活，对端永远停在 `LocalOnly`。
    ///
    /// 断言的是「失败」而不是「成功」：若它意外解析成了别的变体（比如有人把
    /// `SessionMsg` 改成 `#[serde(untagged)]`），老对端会拿一条遥测报文当成
    /// 别的指令执行。
    #[test]
    fn a_peer_that_predates_stage_report_rejects_it_and_that_is_the_safe_outcome() {
        let json = serde_json::to_string(&SessionMsg::StageReport {
            stream_id: 5,
            stages: vec![reading()],
            local_ms: Some(1005.0),
            dev: None,
            quality: None,
            seq_us: 1_000_000,
        })
        .unwrap();
        assert!(
            serde_json::from_str::<LegacySessionMsg>(&json).is_err(),
            "老对端必须认不出它（随后被跳过），而不是错认成另一个变体"
        );
    }

    /// `StageReport` 自己往返一趟，字段不掉。
    #[test]
    fn a_stage_report_round_trips() {
        let msg = SessionMsg::StageReport {
            stream_id: 7,
            stages: vec![reading()],
            local_ms: Some(1005.0),
            dev: Some(DevLatency::unavailable()),
            quality: None,
            seq_us: 42,
        };
        let back: SessionMsg = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        match back {
            SessionMsg::StageReport { stream_id, stages, local_ms, seq_us, .. } => {
                assert_eq!(stream_id, 7);
                assert_eq!(seq_us, 42);
                assert_eq!(local_ms, Some(1005.0));
                assert_eq!(stages.len(), 1);
                assert_eq!(stages[0].id, "src_fifo");
                assert_eq!(stages[0].samples, 48_000);
                assert_eq!(stages[0].rate, 48_000);
                assert_eq!(stages[0].dropped, Some(7));
                assert!(matches!(stages[0].drop_mode, DropMode::Oldest));
            }
            other => panic!("解析成了 {other:?}"),
        }
    }

    /// **音质原料穿过线缆之后一个字段都不许掉**，尤其是两个 `Option`。
    ///
    /// `clip_ratio: None`（还没测）与 `Some(0.0)`（测了，一个越界样本都没有）
    /// 在本项目里是两个结论，混同过一次、代价是一条正在爆音的流报「良好」。
    /// 那条红线在**线上**也必须活着，所以这里逐个断言，不用 `..` 糊过去。
    #[test]
    fn a_quality_reading_survives_the_wire_including_the_difference_between_none_and_zero() {
        for (clip, excess) in [(None, None), (Some(0.0), Some(-120.0)), (Some(0.031), Some(2.5))] {
            let msg = SessionMsg::StageReport {
                stream_id: 9,
                stages: vec![],
                local_ms: None,
                dev: None,
                quality: Some(QualityReading {
                    window_s: 10.5,
                    conceal_ratio: 0.012,
                    plc_ticks: 3,
                    silence_ticks: 1,
                    popped_ticks: 1000,
                    underruns: 2,
                    jb_dropped: 4,
                    clip_ratio: clip,
                    clip_excess_db: excess,
                    bandwidth_hz: 24_000,
                    wire_rate_hz: 48_000,
                    wire_depth: "s24".to_string(),
                    duplicate: true,
                }),
                seq_us: 7,
            };
            let back: SessionMsg =
                serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            let SessionMsg::StageReport { quality: Some(q), .. } = back else {
                panic!("音质原料没活过线缆")
            };
            assert_eq!(q.window_s, 10.5);
            assert_eq!(q.conceal_ratio, 0.012);
            assert_eq!((q.plc_ticks, q.silence_ticks, q.popped_ticks), (3, 1, 1000));
            assert_eq!((q.underruns, q.jb_dropped), (2, 4));
            assert_eq!(q.bandwidth_hz, 24_000);
            // 位深与采样率**各走各的**，两个都得活过线缆。少了位深，
            // 收方就得从 codec 推一遍——而那份推导是第二处真值源。
            assert_eq!(q.wire_depth, "s24", "线上位深没活过线缆");
            // **带宽与采样率是两个数，差 2 倍。** 两者必须各自穿过线缆：
            // 若哪天有人把 `wire_rate_hz` 删掉、让读方拿 `bandwidth_hz * 2` 顶替，
            // 今天它仍然算得对（恒等式），但 Q3 一旦换成实测频谱（规格 §4.2 允许）
            // 就会把「2 × 实测带宽」当采样率报出去，且不会有任何一处报错。
            assert_eq!(q.wire_rate_hz, 48_000, "线上采样率没活过线缆");
            assert_ne!(
                q.wire_rate_hz, q.bandwidth_hz,
                "采样率与带宽被当成了同一个数——正是 2026-08-04 那次误读的根"
            );
            assert!(q.duplicate);
            assert_eq!(q.clip_ratio, clip, "「还没测」与「测了是 0」被线缆混为一谈");
            assert_eq!(q.clip_excess_db, excess);
        }
    }

    /// 旧对端不发 `wire_rate_hz` ⇒ 解析出 0。**这一条是回退路径的前提**：
    /// `grade_peer_quality` 只在读到 0 时才由 `bandwidth_hz * 2` 反推，
    /// 而它凭什么认定「0 = 旧对端」就靠这里的 `#[serde(default)]`。
    /// 若有人给这个字段加上别的默认值，那条回退会对着新对端的真读数生效。
    #[test]
    fn a_quality_reading_from_an_old_peer_has_wire_rate_zero() {
        let json = r#"{"window_s":10.0,"conceal_ratio":0.0,"plc_ticks":0,
                       "silence_ticks":0,"popped_ticks":100,"underruns":0,
                       "jb_dropped":0,"bandwidth_hz":16000}"#;
        let q: QualityReading = serde_json::from_str(json).expect("旧对端的原料必须照常解析");
        assert_eq!(q.bandwidth_hz, 16_000);
        assert_eq!(q.wire_rate_hz, 0, "缺席必须是 0，收方据此才认得出旧对端");
    }

    /// 不带 `quality` 的老报文照常解析，那一格是 `None`。
    /// 若哪天有人把 `#[serde(default)]` 拿掉，整条 `StageReport` 会解析失败、
    /// 被 `recv_timeout` 跳过 —— 延迟分项也会跟着一起消失，而没人会想到是这里。
    #[test]
    fn a_stage_report_without_quality_still_parses() {
        let json = r#"{"type":"stage_report","stream_id":1,"stages":[],
                       "local_ms":null,"dev":null,"seq_us":5}"#;
        let msg: SessionMsg = serde_json::from_str(json).expect("老报文必须照常解析");
        let SessionMsg::StageReport { quality, stream_id, .. } = msg else {
            panic!("解析成了别的变体")
        };
        assert_eq!(stream_id, 1);
        assert!(quality.is_none(), "缺席就是缺席");
    }

    /// `drop_mode` 缺席 ⇒ 整条报文解析失败 ⇒ 被跳过 ⇒ 停在 `LocalOnly`。
    ///
    /// 规格 §0.2：四个 1 秒 FIFO 饱和时的深度读数完全简并，只有丢弃方向能区分
    /// 「恒定迟到但连续」与「迟到 + 断续」。给它编个默认值 = 替对端瞎猜听感，
    /// 所以这个字段故意**没有** serde default。
    #[test]
    fn a_stage_without_a_drop_mode_is_refused_rather_than_guessed() {
        let json = r#"{"type":"stage_report","stream_id":1,"seq_us":1,"stages":[
            {"id":"src_fifo","samples":48000,"rate":48000}]}"#;
        assert!(serde_json::from_str::<SessionMsg>(json).is_err());
    }

    /// 线上**没有** `ms` 字段：`ms` 只能由收方从 `samples`/`rate` 重算。
    ///
    /// 若谁把它加回去，`{"rate":0,"ms":0.0}` 这种报文就能把「这一级读不到」
    /// 伪装成「这一级没有延迟」——蓝牙耳机那 150~250 ms 就是这么消失的。
    #[test]
    fn the_wire_carries_no_precomputed_ms() {
        let json = serde_json::to_string(&reading()).unwrap();
        assert!(
            !json.contains("\"ms\""),
            "分项的 ms 必须由收方自己算，线上不许带：{json}"
        );
    }
}

impl Drop for SecureChannel {
    fn drop(&mut self) {
        // Control-plane keys live inside the ChaCha ciphers (wiped by their own
        // Drop); the media keys are our plain copies.
        self.media.tx.zeroize();
        self.media.rx.zeroize();
    }
}
