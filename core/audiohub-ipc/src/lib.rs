//! Local IPC contract between audiohubd and thin clients (CLI `ctl`, later the
//! Tauri UI). Transport: WebSocket on 127.0.0.1 (random port), text frames,
//! one JSON object per frame. Endpoint + token live in `<config_dir>/ipc.json`.
//!
//! Frame flow:
//!   client -> {"auth":"<token>"}            first frame, mandatory
//!   server -> {"ok":true,"daemon":DaemonInfo}
//!   client -> {"id":1,"method":"...","params":{...}}
//!   server -> {"id":1,"ok":true,"result":...} | {"id":1,"ok":false,"error":"..."}
//!   server -> {"event":"stats","data":...}   unsolicited after stats.subscribe

use serde::{Deserialize, Serialize};

/// 2 起：daemon 保证 `SessionStats.pipeline` / `.quality` 两个字段**存在**
/// （值可以是 `null`）。这是能力标记，不是不兼容变更——字段全部 `#[serde(default)]`
/// 纯追加，v1 客户端读 v2 的回包没有任何问题。
///
/// 升它的唯一理由（规格 §3.6 / R2）：让 UI 分得清「**daemon 支持但暂无数据**」
/// 与「**daemon 不支持**」。前者显示「测量中」，后者显示「daemon 版本较旧」，
/// 是两个不同的用户动作。
///
/// ⚠ **必须同步改的两处**（不在本 crate，改这里就得改它们，否则 App 拒连）：
///   - `app/src-tauri/src/main.rs` 的 `const IPC_VERSION: u32`
///   - `app/frontend/src/ipc/client.ts` 的 `export const IPC_VERSION`
/// 两处都做**严格相等**校验（`main.rs` 的 `port_alive` 分支会直接报版本不符），
/// 所以它们与本常量是一个原子的三件套。
pub const IPC_VERSION: u32 = 2;

pub use audiohub_core::audio::DevicesReport;
pub use audiohub_core::dsp::ToneVerdict;
pub use audiohub_core::latency::{DevLatency, DropMode, LatSource};
pub use audiohub_core::permissions::{
    PermissionKind, PermissionState, KIND_LOCAL_NETWORK, KIND_MICROPHONE, KIND_SYSTEM_AUDIO,
};
pub use audiohub_core::sysaudio::VirtualCard;
pub use audiohub_core::volume::VolumeState;
pub use audiohub_net::identity::PairedPeer;

/// Where a page served by the daemon's own web UI asks for the endpoint below.
///
/// The daemon serves the UI over HTTP on its CONTROL port, loopback only
/// (`audiohubd::webui`). A page loaded from there has no `?port&token` in its
/// URL and no Tauri bridge to ask, so it `fetch`es this path on its own origin
/// and gets `{"ipc_version","port","token"}` — the same three values
/// `ipc.json` carries, minus `pid`, which is a liveness detail for whoever owns
/// the file and means nothing to a client that just reached the owner.
pub const IPC_ENDPOINT_PATH: &str = "/ipc-endpoint";

/// Written to `<config_dir>/ipc.json` (0600) by the daemon on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEndpoint {
    pub ipc_version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub ipc_version: u32,
    pub name: String,
    pub fingerprint: String,
    pub control_port: u16,
    pub uptime_s: f64,
    /// Named output devices the UI can offer as a bridge target (spec-m4c §B).
    #[serde(default)]
    pub output_devices: Vec<String>,
    /// Third-party virtual sound cards, `present` telling the UI whether the
    /// bridge selector is selectable or greyed out (spec-m4b §C / m4c §B).
    #[serde(default)]
    pub virtual_cards: Vec<VirtualCard>,
    /// 站点级混音健康（规格 §3.5 / §4.6）。挂在这里而不是 `SessionStats` 上，
    /// 是因为它是**求和之后**的量：削顶发生在 N 路相加以后，归不到任何一条
    /// 会话头上。`None` = 本窗口内混音器没有输出过。
    #[serde(default)]
    pub mix_health: Option<MixHealth>,
}

/// `kind` from the CALLER's perspective:
/// - "mic": consume the peer's microphone (media flows peer -> me)
/// - "spk": send audio to the peer's default output (media flows me -> peer)
pub const KIND_MIC: &str = "mic";
pub const KIND_SPK: &str = "spk";

/// Audio source for locally-originated streams.
/// "tone" is the probe source (deviceless); "mic" needs capture permission;
/// "sysaudio" mirrors what this machine is playing (spec-m4b §B2, `backend`).
/// "halspk" is whatever an application played into the addressed peer's own
/// virtual speaker on macOS (spec-m5b §5.4) — one device per paired peer, named
/// after that peer. It needs the HAL bridge to be registered, and yields
/// silence — never a stall — while no driver is attached.
pub const SOURCE_TONE: &str = "tone";
pub const SOURCE_MIC: &str = "mic";
pub const SOURCE_SYSAUDIO: &str = "sysaudio";
pub const SOURCE_HAL_SPEAKER: &str = "halspk";

/// `kind` of `daemon.simulate_device_change`.
pub const DEVICE_INPUT: &str = "input";
pub const DEVICE_OUTPUT: &str = "output";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSessionParams {
    pub peer: String,               // fingerprint (prefix allowed, unique)
    pub kind: String,               // KIND_MIC | KIND_SPK
    #[serde(default)]
    pub source: Option<String>,     // for spk / provider tone probes
    #[serde(default)]
    pub freq: Option<f32>,          // tone source frequency
    #[serde(default)]
    pub backend: Option<String>,    // sysaudio source: backend id, None = "auto"
    #[serde(default)]
    pub monitor: bool,              // mic: play received audio locally
    #[serde(default)]
    pub verify_freq: Option<f32>,   // receiver computes ToneVerdict (probe)
    #[serde(default)]
    pub simulate_loss_pct: Option<f32>, // sender-side loss injection (probe)
    #[serde(default)]
    pub volume_sync: bool,          // spk: drive the peer's output volume
    /// mic: ALSO render the decoded peer audio into this NAMED output device
    /// (a third-party virtual card, spec-m4c §B). Independent of `monitor`:
    /// one decode can feed both. A device that cannot be opened fails the
    /// session open — it never falls back to the default output.
    #[serde(default)]
    pub bridge: Option<String>,
    /// mic: ALSO write the decoded peer audio into the virtual microphone this
    /// peer owns, so anything on this Mac that selects "AudioHub – <peer>
    /// 麦克风" hears them (spec-m5b §5.4). Independent of `monitor` and
    /// `bridge` — one decode feeds all three. Explicit by design: it defaults
    /// to false so a session never quietly takes over a virtual microphone.
    ///
    /// WHICH virtual microphone is not expressible here and never will be: the
    /// device belongs to `peer`, and slots are a daemon-internal index that no
    /// IPC client may name (spec-m5b §5.6).
    #[serde(default)]
    pub hal: bool,
    /// Open this session even though mode B owns the session lifecycle
    /// (spec-m5b §6.1). CLI/probe only: in mode B the daemon refuses a plain
    /// `session.open`, because a UI that could open its own sessions would have
    /// turned mode B back into mode A with different labels.
    #[serde(default, rename = "override")]
    pub override_mode: bool,
}

/// Global consumer mode (plan §7.1, frozen): it is a property of THIS machine,
/// not of a peer, so it lives in the daemon and the UI's copy is a cache.
pub const MODE_A: &str = "a";
pub const MODE_B: &str = "b";

/// Daemon-owned settings, `settings.get` / `settings.set` (spec-m5b §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonSettings {
    /// What the user asked for: `MODE_A` | `MODE_B`.
    pub consumer_mode: String,
    /// What is actually in force. `MODE_B` only when the driver is usable, so
    /// the two ends can no longer disagree for long about which mode is live.
    pub effective_mode: String,
    /// plan §7.3: remove a peer's virtual devices while it is disconnected.
    pub remove_virtual_on_disconnect: bool,
    /// Append `（离线）` to a disconnected peer's device names, so "no sound"
    /// is visible in the system's own device list (spec-m5b OPEN QUESTION 1).
    pub mark_offline_devices: bool,
    /// Persisted for the UI; not yet wired to the media plane (the AUTO ladder
    /// still decides both). Kept here so the UI has one home for its settings
    /// instead of localStorage.
    pub latency: String,
    pub quality: String,
    /// Virtual-device slots the attached driver offers, and how many are bound.
    pub hal_capacity: u8,
    pub hal_used: u8,
}

/// One published (or intended) virtual device pair, `hal.devices`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HalDeviceInfo {
    /// Diagnostics only. Never an input anywhere in this contract.
    pub slot: u8,
    pub fingerprint: String,
    pub out_uid: String,
    pub in_uid: String,
    pub out_name: String,
    pub in_name: String,
    pub generation: u32,
    /// "free" | "bound" | "delisted" | "pending" (sent, not yet answered).
    pub state: String,
    /// The system's own device list really contains both UIDs. This is the
    /// closed-loop half: `state == "bound"` alone only says the driver
    /// acknowledged us (spec-m5b §5.2).
    pub observed: bool,
    pub peer_connected: bool,
    pub io_out: bool,
    pub io_in: bool,
    pub spk_frames: u64,
    pub mic_frames: u64,
    pub mic_dropped: u64,
}

/// A peer's virtual devices, as `PeerState.hal_device`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerHalDevice {
    pub out_name: String,
    pub in_name: String,
    pub out_uid: String,
    pub in_uid: String,
    pub state: String,
    pub observed: bool,
}

/// Who opened a session, reported as `SessionInfo.origin`.
pub const ORIGIN_USER: &str = "user";
pub const ORIGIN_HAL: &str = "hal";
pub const ORIGIN_PEER: &str = "peer";

/// Health of the macOS HAL bridge, reported by `daemon.status` as `hal`.
/// Absent/null everywhere the bridge does not exist (any non-macOS host, or a
/// macOS host without the LaunchDaemon), which is the normal case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HalStatus {
    /// launchd handed us the mach name: the driver can find us.
    pub registered: bool,
    /// A HAL plug-in completed the handshake and holds live rings.
    pub driver_connected: bool,
    /// Speaker-direction frames handed to the media engine, over every slot.
    pub spk_frames: u64,
    /// Microphone-direction frames accepted by the rings, over every slot.
    pub mic_frames: u64,
    /// Microphone frames the rings had no room for (driver not draining).
    pub mic_dropped: u64,
    /// Seconds since the last message from the driver, `None` if it never spoke.
    #[serde(default)]
    pub last_driver_msg_secs: Option<f64>,
    /// What this daemon speaks, and what the driver said it speaks when it
    /// refused us. A mismatch is the one driver problem only a reinstall fixes.
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub driver_protocol_version: Option<u32>,
    /// Machine-readable reason there is no live bridge, e.g.
    /// `driver_protocol_mismatch`. `None` while connected.
    #[serde(default)]
    pub status_reason: Option<String>,
    /// Per-peer virtual devices. The three counters above are the sums of the
    /// per-slot ones here (spec-m5b §6.1).
    #[serde(default)]
    pub devices: Vec<HalDeviceInfo>,
}

// ------------------------------------------------------- 逐级延迟会计 (P0a)

/// 管线上一级缓冲的瞬时读数（规格 §3.2 / §3.5）。
///
/// `id` 的取值与前端 `app/frontend/src/lib/metrics.ts` 的 `LATENCY_STAGES[].id`
/// **逐字一致**（snake_case，不做大小写转写）：
/// `cap_ring` | `cap_dev` | `src_fifo` | `hal_spk` | `send_pace` | `network`
/// | `jitter_buf` | `post_mix` | `play_ring` | `play_dev` | `residual`
///
/// 中间不留映射表——映射表漏一条就是那一级静默显示「未知」，而「静默缺项」
/// 正是本规格反复点名要消灭的失败形态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    pub id: String,
    pub samples: u32,
    /// 该级容量；0 = 无界 / 不适用。
    pub capacity: u32,
    /// 该级**消费者**的标称速率(Hz)。播放环走**设备**速率（可能 44.1k），
    /// 混用 48000 会引入 −8.8% 的系统性偏差。`rate == 0` 即判该级读数无效。
    pub rate: u32,
    /// `samples * 1000 / rate`，daemon 算好直接给。
    ///
    /// 冗余但值得：UI 自己除一遍就多一个用错 rate 的机会，而那个错误
    /// （拿 48000 除 44.1k 设备的读数）恰好是 −8.8%，小到不会有人发现。
    /// `None` = 这一级读不到，**不是 0 ms**。
    #[serde(default)]
    pub ms: Option<f64>,
    /// 会话累计丢弃样本数。**`None` = 本进程观测不到这一级的丢弃，不是没丢过。**
    /// 典型是 `hal_spk`：环满时写不进去的是驱动侧的 IOProc，计数在它那里。
    #[serde(default)]
    pub dropped: Option<u64>,
    /// 满时丢哪一头。**必填**：规格 §0.2 已证明四个 1 秒 FIFO 的丢弃方向不同，
    /// 而它们**饱和时的深度读数完全简并**——三个源侧 FIFO 丢最旧（听感「恒定
    /// 迟到但连续」），播放环与采集环丢最新（听感「迟到 + 断续」）。少了这个
    /// 标签，遥测只能说「有一秒卡在某处」，说不出那一秒是怎么卡的。
    pub drop_mode: DropMode,
    /// 深度贴着容量上限（≥95%）。
    pub saturated: bool,
    /// 30 s 窗口深度斜率，样本/秒（规格 §3.3）。`None` = 样本点不足以判趋势
    /// （<3 点或跨度 <5 s）——**不是 0**：「测到了，就是不漂」与「还没测出来」
    /// 是两个不同的结论，而它们对应完全不同的修法：
    ///   - ≈0 + 饱和 + `dropped` 冻结  ⇒ 曾被一次卡顿灌满，之后收支平衡但永远迟到
    ///   - ≈0 + 饱和 + `dropped` 增长  ⇒ 稳态产销速率失配
    ///   - 持续同号且未饱和          ⇒ 正在走向饱和
    #[serde(default)]
    pub drift_sps: Option<f64>,
}

/// 这个延迟读数**能信到什么程度**。
///
/// 序列化取值与前端 `metrics.ts` 的
/// `LatencyConfidence = 'full' | 'lowerBound' | 'converging' | 'localOnly' | 'unavailable'`
/// **逐字一致**（故意用 camelCase 而非 Rust 习惯的 snake_case：全部字符串枚举
/// 都直穿前端，一张映射表都不留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LatConfidence {
    /// 各分项齐全，e2e 与 Σ 闭合。
    Full,
    /// 缺声卡缓冲项，读数是下限。UI 加「≥」。
    LowerBound,
    /// 时钟偏移 θ 尚未收敛。UI 显示「测量中」。
    Converging,
    /// 对端未上报分项（旧 daemon，或 P0a 这种单端部署）。只显示本机段。
    LocalOnly,
    /// 无法测量。
    Unavailable,
}

/// 一条会话的逐级延迟会计（规格 §3.5）。
///
/// **P0a 阶段的取值**：`stages` 只有本侧的级，`peer_stages` 为空，
/// `confidence = LocalOnly`，`sum_ms = None`（对端分项缺失），`local_ms` 是本侧
/// Σ——那是这一期唯一能显示的数字。`net_ms` / `e2e_ms` / `residual_ms` /
/// `clock_offset_us` 全部 `None`，它们是 P0b / P1 的活。
///
/// **绝不用 0 填补缺失分项**：任一已声明存在的分项测不到 ⇒ 相应的和为 `None`。
/// 用 0 填补会让蓝牙耳机（真实 +150~250 ms）看起来和模拟输出一样好。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineLatency {
    /// "send" | "recv"：本侧在这条流里是发送端还是接收端，决定 `stages` 里
    /// 会出现哪几级。
    pub side: String,
    /// 本侧各级，按数据流顺序。
    pub stages: Vec<PipelineStage>,
    /// 本侧 Σ。任一本侧级 `rate == 0` ⇒ `None`。
    #[serde(default)]
    pub local_ms: Option<f64>,
    /// 本侧声卡固有延迟。P0 恒为 `Unavailable`（平台查询是 P1 的活）——
    /// 保留字段是为了让「缺项 ⇒ 带『≥』」这条链路现在就成立。
    #[serde(default)]
    pub dev: Option<DevLatency>,

    /// 对端分项（控制面回传，P0b 起）。P0a 恒为空。
    #[serde(default)]
    pub peer_stages: Vec<PipelineStage>,
    #[serde(default)]
    pub peer_local_ms: Option<f64>,
    #[serde(default)]
    pub peer_dev: Option<DevLatency>,
    /// 对端读数的年龄（秒）。>3 即视为陈旧，UI 标注。
    #[serde(default)]
    pub peer_age_s: Option<f64>,

    /// 单向网络 = 控制面 min-RTT / 2。**只作一段，绝不作总数。**
    ///
    /// 红线（规格 §3.1）：实测 RTT 0.58 ms vs 感知 ~1000 ms，比值 1700 倍，
    /// 两者之间不存在任何单调关系。任何情况下不得用 RTT 冒充或填补总延迟。
    #[serde(default)]
    pub net_ms: Option<f64>,
    /// 交叉校验：|net_ms − rtt/2|。超过 5 ms 或超过读数 10% ⇒ UI 降级为「约」。
    #[serde(default)]
    pub rtt_cross_check_ms: Option<f64>,

    /// Σ 各级（含对端）。任一已声明分项缺失即 `None`。
    #[serde(default)]
    pub sum_ms: Option<f64>,
    /// P1：真实采样年龄。
    #[serde(default)]
    pub e2e_ms: Option<f64>,
    /// P1：`e2e_ms − sum_ms`。|residual| > 20 ms 即存在未建模的缓冲级。
    #[serde(default)]
    pub residual_ms: Option<f64>,

    #[serde(default)]
    pub clock_offset_us: Option<i64>,
    #[serde(default)]
    pub clock_unc_us: Option<u32>,
    pub confidence: LatConfidence,
}

// ------------------------------------------------------------ 音质 (P0q)

/// 一条会话的音质三分量（规格 §4）。
///
/// **音质 = 保真度**：最终送进扬声器的样本流，相对于对端采集到的原始波形被
/// 损坏了多少。测点在 JitterBuffer pop 之后、送进播放环之前。
///
/// 明确拒绝用丢包率当音质：丢包 2% 在 PLC 修得住时几乎不可闻，丢包 0% 时两路
/// 重复流相加照样把声音削烂。丢包率是**网口上的量**，音质是**扬声器上的量**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityStats {
    /// 实际统计窗口秒数（滚动窗口的真实跨度，不是标称的 10）。
    pub window_s: f64,
    /// Q1：`(plc + 3*silence) / total`，[0,1]。
    ///
    /// silence 权重 3 的依据：PLC 是「上一帧 ×0.7 重复」，仍有能量、仍连续；
    /// silence 是彻底的真空。ITU-T G.113 附录 I 对帧擦除给出有隐藏 / 无隐藏
    /// 两条 Ie 曲线，同一丢失率下无隐藏的损伤值约为有隐藏的 2.5~3 倍。
    pub conceal_ratio: f64,
    pub plc_ticks: u64,
    pub silence_ticks: u64,
    pub popped_ticks: u64,
    /// 二级证据，**不参与定级**：它们解释等级为何低，不定义等级。
    pub underruns: u64,
    pub jb_dropped: u64,
    /// Q2：本流送进混音前 |v| > 0.8 的采样占比。
    ///
    /// **`None` = 本窗口还没攒够一整页，这一分量「还没测」。** 它与
    /// `Some(0.0)`（「测了，确实一个越界样本都没有」）是两个完全不同的结论，
    /// 而这里曾经用 `f64` 承载、缺席时填 0——于是流开头约 10~20 秒里，一条正在
    /// 爆音的流会拿到 `grade_clip(0.0) = Excellent`，而 min 合成下一个被钉成
    /// Excellent 的分量**永远拉不低总分**，整条流报「良好」。
    /// 这与 Q1「窗口不够就整体 None」的口径也自相矛盾（同一个函数上下两行）。
    #[serde(default)]
    pub clip_ratio: Option<f64>,
    /// `20*log10(pre_clip_peak / 0.8)`，负值表示根本没碰到削顶阈值。
    /// `None` 的含义同 `clip_ratio`。
    #[serde(default)]
    pub clip_excess_db: Option<f64>,
    /// Q3：`rung_rate / 2`（Nyquist）。
    pub bandwidth_hz: u32,
    /// "excellent" | "good" | "fair" | "poor" | "unknown"
    ///
    /// **三分量取 min（木桶），不是加权平均**：三家损伤在感知上不可互相补偿。
    /// 加权平均会把「两路重复流把声音削烂」（Q2=差、Q1=优、Q3=优）稀释成
    /// 「良」，恰好掩盖用户要抓的那个 bug。
    ///
    /// **`"unknown"` = 等级不成立**，不是「一般般」也不是「没有会话」。分量缺席
    /// 时在场分量的 min 只是**上界**，真实等级落在 `[差, 上界]` 这个区间里，
    /// 而区间不是等级。UI 必须把它渲染成「测量中」一类的措辞，**绝不可回退到
    /// 某个具体等级**——那正是这个字段此前的失败形态：`min(q1, Excellent, q3)`
    /// 与 `min(q1, q3)` 逐值相同，于是缺席被静默读作「良好」。
    /// 唯一的例外由 daemon 侧判掉：上界已经贴着地板时等级确定，照常给出 "poor"。
    pub grade: String,
    /// "continuity" | "level" | "bandwidth" | "none"：argmin，拖后腿的那一项。
    ///
    /// **缺席的分量不会出现在这里**：`clip_ratio == None` 时 `worst` 永远不是
    /// "level"，因为那一项根本还没被测量，说不上它拖没拖后腿。
    /// `grade == "unknown"` 时恒为 "none"（等级都没定，谈不上谁拖后腿）。
    pub worst: String,
    /// 本次合成是不是**少了至少一块板**（目前只可能是 Q2 的削顶页没攒满）。
    ///
    /// 与 `grade` 的关系：`partial` 为真时 `grade` 通常是 `"unknown"`，
    /// **但两者不是同义词**——上界已经触底（"poor"）时等级确定而 `partial`
    /// 仍为真。UI 若想说明「这个结论是在缺一项的情况下得出的」，读这个字段；
    /// 若只是决定要不要显示等级，读 `grade == "unknown"` 就够。
    #[serde(default)]
    pub partial: bool,
}

/// 站点级混音健康（规格 §3.5）。**求和之后**的量，不可归属到单条会话，
/// 所以挂在 `DaemonInfo` 上而不是 `SessionStats` 上。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixHealth {
    pub window_s: f64,
    /// 求和后、`soft_clip` 之前 |v| > 0.8 的采样占比。
    ///
    /// **`None` = 削顶页还没攒满，「还没测」**——与 `Some(0.0)`（「测了，没有一个
    /// 样本越界」）是两个不同的结论。这里曾经 `unwrap_or(0.0)`，把启动后头 10 秒
    /// 的空窗报成「混音正常」。
    #[serde(default)]
    pub clip_ratio: Option<f64>,
    /// `20*log10(pre_clip_peak / 0.8)`。`None` 的含义同 `clip_ratio`。
    #[serde(default)]
    pub clip_excess_db: Option<f64>,
    /// 本窗口内单 tick 参与求和的最大流数。
    pub max_contrib: u32,
    /// 前两路参与求和的帧在零延迟上的归一化互相关峰值。
    /// `None` = 本窗口内就没有过两路同时求和，无从相关。
    #[serde(default)]
    pub corr_peak: Option<f64>,
    /// `corr_peak > 0.98` 且窗口内占比 > 90%。
    ///
    /// 这条判据之所以严谨，恰恰因为它**对阈值不敏感**：正常素材峰值 −3 dBFS 时
    /// 越过 0.8 的采样占比是 1e-4 量级，而两路相同信号相加等于整段波形 ×2，
    /// 正常电平的音乐立刻有百分之几十的采样越界。两者之间隔着 3 个数量级的
    /// 真空，阈值放在这个空隙里的任何位置结论都一样。
    pub duplicate_suspect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub received: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub bitrate_kbps: f64,
    pub jb_depth_frames: u32,       // current jitter buffer depth (recv side)
    pub sent_packets: u64,          // send side
    pub rung: u32,                  // current AUTO ladder rung (0 = best)
    pub rung_changes: u32,
    pub verdict: Option<ToneVerdict>,
    pub mix_verdicts: Option<Vec<ToneVerdict>>, // provider mixer taps (probe)
    /// spk sessions opened with `volume_sync`: the provider's real output
    /// device state. Present on BOTH sides — the provider fills it from its own
    /// device, the consumer from the provider's VolumeState reports. `None`
    /// means the session does not sync volume (or nothing arrived yet).
    #[serde(default)]
    pub volume: Option<VolumeState>,

    // ---- 以下为 P0a / P0q 追加。**纯追加**：既有 12 个字段的顺序与语义未动。
    //
    // 兼容性（规格 §0.1，已对 secure.rs:422-437 核对）：认证成功但无法反序列化
    // 的 SessionMsg 会被跳过且连接存活，故媒体面/控制面新增变体无需升协议版本。
    // IPC 侧同理——全部 #[serde(default)]，v1 客户端读 v2 回包毫无问题。
    // `IPC_VERSION` 1→2 只是能力标记，见该常量的注释。
    /// 逐级延迟会计。`None` = 本端未采集（非媒体会话，或该会话尚无可读的级）。
    #[serde(default)]
    pub pipeline: Option<PipelineLatency>,
    /// 音质三分量。`None` = 窗口还不够长 / 这条流没有接收侧（发送会话没有
    /// 「送进扬声器的样本」可言）。**不是 0 分**。
    #[serde(default)]
    pub quality: Option<QualityStats>,

    // JitterBuffer 内部早已存在却从未导出的五个计数器（media.rs:115-119），
    // 零成本补齐。这些是 **lifetime 累计值**，窗口化由 `quality` 承担——
    // 用 lifetime 算隐藏率会让一次早期抖动永远压着等级，那与 `take_interval`
    // 已经吸取过的教训是同一条。
    #[serde(default)]
    pub jb_popped: u64,
    #[serde(default)]
    pub jb_underruns: u64,
    #[serde(default)]
    pub jb_dropped: u64,
    #[serde(default)]
    pub jb_plc: u64,
    #[serde(default)]
    pub jb_silence: u64,
    #[serde(default)]
    pub jb_target_frames: u32,
    #[serde(default)]
    pub jb_prebuffering: bool,
    /// 从 `next_seq` 起**连续**的帧数（规格 §7.2 R10）。
    ///
    /// 与 `jb_depth_frames` 的区别不是精度而是含义：后者是 `BTreeMap` 的条目数，
    /// 乱序到达时把「洞之后的帧」也算了进去。next_seq 缺失而 {n+1,n+2,n+3} 已
    /// 到达时，`jb_depth_frames = 3`（谎报 30 ms 排队），`jb_contiguous_frames = 0`
    /// ——而下一个 tick 一定 underrun。**延迟分项用的是这个数。**
    #[serde(default)]
    pub jb_contiguous_frames: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub peer_fingerprint: String,
    pub peer_name: String,
    pub kind: String,               // KIND_MIC | KIND_SPK
    pub dir: String,                // "send" | "recv"
    pub sample_rate: u32,
    pub channels: u8,
    pub stats: SessionStats,
    /// ORIGIN_USER | ORIGIN_HAL | ORIGIN_PEER. A `hal` session exists because
    /// an application selected a virtual device; closing it from the UI would
    /// leave that application's device selection pointing at silence, which is
    /// why the detail page hides its close button (spec-m5b §6.2).
    #[serde(default)]
    pub origin: String,
    /// Diagnostics only, and only on a `hal` session: which slot's rings this
    /// session is wired to. Never accepted as input.
    #[serde(default)]
    pub hal_slot: Option<u8>,
    /// The virtual device's display name, for the stats page.
    #[serde(default)]
    pub hal_device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerState {
    #[serde(flatten)]
    pub peer: PairedPeer,
    pub online: bool,               // live control channel right now
    /// A retry loop is armed for this peer (spec-m4c §C). Only ever true for a
    /// peer THIS daemon has connected to itself.
    #[serde(default)]
    pub reconnecting: bool,
    /// Seconds until the next retry, when `reconnecting`.
    #[serde(default)]
    pub retry_in_s: Option<f64>,
    /// The virtual devices this peer owns, `None` when it has none (mode A, no
    /// driver, or the slot pool is full — see `hal_reason`).
    #[serde(default)]
    pub hal_device: Option<PeerHalDevice>,
    /// Why this peer has no virtual devices: "mode_a" | "no_driver" |
    /// "capacity" | "removed_while_offline".
    #[serde(default)]
    pub hal_reason: Option<String>,
    /// The name the virtual devices carry: the alias if the user set one, the
    /// peer's own computer name otherwise, with ` (2)` appended when two peers
    /// would otherwise be indistinguishable (spec-m5b §5.3).
    #[serde(default)]
    pub display_name: String,
}

/// Method names (params -> result):
/// - "daemon.status"     {}                    -> DaemonInfo + `hal`
///       `hal` is a `HalStatus` object (spec-round2 §B2) or null where no HAL
///       bridge exists. It is added to the DaemonInfo object by the daemon, not
///       carried as a DaemonInfo field, so a client that predates it sees
///       exactly what it saw before.
/// - "daemon.shutdown"   {}                    -> {}
/// - "daemon.simulate_device_change" {kind}    -> {kind, epoch}
///       kind = "input" | "output". Drives the same rebuild path a real
///       default-device change takes, without touching any system device.
/// - "daemon.permissions" {}                   -> Vec<PermissionState>
///       Status of every OS permission the app needs, for the first-run gate
///       page. NEVER prompts, so the UI may poll it freely. `granted: null`
///       means "unknown", which on macOS is the permanent steady state for
///       local network and system audio — neither has a query API. The gate
///       must therefore treat null as "尚未确认，让用户点一次授权", never as a
///       block, or it can never be passed. Only `granted: false` is a real
///       denial, and only System Settings (`settings_url`) can undo it.
/// - "daemon.request_permission" {kind}        -> PermissionState
///       kind = "microphone" | "local_network" | "system_audio" (also accepted
///       under the key "id", which is what the UI calls it). THE ONLY
///       PROMPTING METHOD: raises the OS consent dialog, so it must be driven
///       by a user click, never on load. Blocks while the dialog is up (the
///       microphone case waits ~20s for an answer, the system-audio case as
///       long as Core Audio takes) and answers with the post-attempt state.
///       A `granted` that is still null means the user had not answered yet —
///       keep polling "daemon.permissions". Errors are user-facing text
///       (already denied, no input device, tap refused).
/// - "peers.list"        {}                    -> Vec<PeerState>
/// - "peers.connect"     {peer, addr?}         -> PeerState        (verify by fingerprint)
/// - "peers.disconnect"  {peer}                -> {fingerprint}
///       Drops the control channel AND disarms the reconnect loop for that
///       peer: an explicit disconnect is not a failure to recover from.
/// - "pairing.enable"    {pin?, ttl_s?}        -> {pin}
/// - "pairing.disable"   {}                    -> {}
/// - "discover.run"      {secs?}               -> Vec<DiscoveredPeer-json>
/// - "session.open"      OpenSessionParams     -> SessionInfo
/// - "session.close"     {id}                  -> {}
/// - "session.list"      {}                    -> Vec<SessionInfo>
/// - "session.set_volume" {id, scalar, muted?} -> {}   (spk consumer side only;
///       the result shows up as `stats.volume` on the next session.list/event.
///       An omitted `muted` leaves the peer's mute control untouched — it is
///       never resolved to a default, which would unmute a muted machine)
/// - "stats.subscribe"   {interval_ms?}        -> {} (then "stats" events with Vec<SessionInfo>)
/// - "settings.get"      {}                    -> DaemonSettings
/// - "settings.set"      {consumer_mode?, remove_virtual_on_disconnect?,
///                        mark_offline_devices?, latency?, quality?}
///                                             -> DaemonSettings
///       The mode is DAEMON-owned global state (plan §7.1): switching to
///       mode A removes every virtual device, switching to B recreates them
///       under the same UIDs.
/// - "peers.pair"        {addr, pin}           -> PeerState
///       The initiator half of M3 pairing, moved out of the CLI so a pairing
///       done anywhere is visible to the device coordinator immediately.
/// - "peers.unpair"      {peer}                -> {fingerprint}
///       Removes the pairing, tells the PEER (so its copy of our devices goes
///       away too), closes sessions and removes the virtual devices.
/// - "peers.set_alias"   {peer, alias}         -> {fingerprint, display_name}
///       Renames the peer's virtual devices in place: same UID, same
///       AudioObjectID, so an application's device selection survives it.
pub mod methods {
    pub const DAEMON_STATUS: &str = "daemon.status";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    pub const DAEMON_SIMULATE_DEVICE_CHANGE: &str = "daemon.simulate_device_change";
    pub const DAEMON_PERMISSIONS: &str = "daemon.permissions";
    pub const DAEMON_REQUEST_PERMISSION: &str = "daemon.request_permission";
    pub const PEERS_LIST: &str = "peers.list";
    pub const PEERS_CONNECT: &str = "peers.connect";
    pub const PEERS_DISCONNECT: &str = "peers.disconnect";
    pub const PAIRING_ENABLE: &str = "pairing.enable";
    pub const PAIRING_DISABLE: &str = "pairing.disable";
    pub const DISCOVER_RUN: &str = "discover.run";
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_SET_VOLUME: &str = "session.set_volume";
    pub const STATS_SUBSCRIBE: &str = "stats.subscribe";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const PEERS_PAIR: &str = "peers.pair";
    pub const PEERS_UNPAIR: &str = "peers.unpair";
    pub const PEERS_SET_ALIAS: &str = "peers.set_alias";
}

#[cfg(test)]
mod version_contract_tests {
    use super::IPC_VERSION;

    /// 读仓库里另一处（非本 crate）的源文件。读不到就 panic —— 绝不 skip：
    /// 一条「文件没了就悄悄通过」的守卫，正好在文件被改名的那一刻失效。
    fn read_sibling(rel: &str) -> String {
        let path = format!("{}/../../{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "读不到 {rel}（{e}）。文件被改名/挪走了就把这条测试一起更新，\
                 不要让它退化成一条恒真断言"
            )
        })
    }

    /// 取 `needle` 之后紧跟的十进制整数，并要求 `needle` 在全文**恰好出现一次**。
    ///
    /// 唯一性不是洁癖：常量名出现在注释里非常常见（这两个文件里都有），
    /// 「匹配第一个」会让守卫在别人加一行注释时开始读错地方，而且照样是绿的。
    fn sole_int_after(src: &str, rel: &str, needle: &str) -> u32 {
        let hits = src.matches(needle).count();
        assert_eq!(hits, 1, "{rel} 里 `{needle}` 出现了 {hits} 次，期望恰好 1 次");
        let tail = &src[src.find(needle).unwrap() + needle.len()..];
        let digits: String = tail
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("{rel} 的 `{needle}` 后面没有跟十进制整数"))
    }

    /// `IPC_VERSION` 的三处声明必须相等 —— 见该常量上方的契约注释。
    ///
    /// 为什么这条守卫非有不可：`app/src-tauri` **不是根 workspace 的成员**
    /// （根 Cargo.toml 的 members 里没有它），前端更不经过 cargo。于是
    /// `cargo build --release`、`cargo test --workspace`、`tsc --noEmit`、
    /// `npm run build` **四样全绿**，而三处声明可以互不相同 —— 没有任何本地信号。
    ///
    /// 2026-08-01 部署实测到的后果：daemon 报 v2、配对在线、音频零丢包一切正常，
    /// 两端 UI 却被「AudioHub 服务版本不兼容」的模态整个挡死。两处校验都是
    /// **严格相等**（`main.rs` 的 `port_alive` 分支、`client.ts` 的 `v !== IPC_VERSION`），
    /// 所以落后一版不是「少显示一点数据」，是拒连。
    #[test]
    fn the_three_ipc_version_declarations_agree() {
        const RS: &str = "app/src-tauri/src/main.rs";
        const TS: &str = "app/frontend/src/ipc/client.ts";
        let shell = sole_int_after(&read_sibling(RS), RS, "const IPC_VERSION: u32 =");
        let front = sole_int_after(&read_sibling(TS), TS, "export const IPC_VERSION =");
        assert_eq!(
            (shell, front),
            (IPC_VERSION, IPC_VERSION),
            "三处 IPC_VERSION 不一致（本 crate={IPC_VERSION}、{RS}={shell}、{TS}={front}）。\
             App 会以「服务版本不兼容」拒连：音频照跑，界面全死。"
        );
    }
}
