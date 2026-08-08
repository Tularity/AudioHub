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

pub mod transport;

pub use transport::{
    LatencyTarget, QualityStop, QualityTarget, LATENCY_AUTO, LATENCY_LEGACY_MIN, LATENCY_STOPS_MS,
    QUALITY_AUTO,
};

/// 2 起：daemon 保证 `SessionStats.pipeline` / `.quality` 两个字段**存在**
/// （值可以是 `null`）。这是能力标记，不是不兼容变更——字段全部 `#[serde(default)]`
/// 纯追加，v1 客户端读 v2 的回包没有任何问题。
///
/// 升它的唯一理由（规格 §3.6 / R2）：让 UI 分得清「**daemon 支持但暂无数据**」
/// 与「**daemon 不支持**」。前者显示「测量中」，后者显示「daemon 版本较旧」，
/// 是两个不同的用户动作。
///
/// **3（plan §13 三模式互斥）：这一次是真正的不兼容变更。**
/// `DaemonSettings.consumer_mode` 改名为 `mode` 并新增取值 `share`；
/// `PeerState` 新增 `peer_mode`。旧客户端读新 daemon 会看不到 `consumer_mode`，
/// 于是把模式渲染成默认值 —— 一个「显示 A、实际 Share」的界面比拒连坏得多，
/// 所以照既有的**严格相等**语义拒连（见下方三处同步要求）。
///
/// **4（plan §15 每对端 × 每方向）：又一次真正的不兼容变更。**
/// `DaemonSettings` 的 `latency` / `quality` / `transport_live` **三个字段消失**，
/// 档位搬到 [`PeerState::transport`]（`peers.set_transport` 写、`peers.list` 读）。
/// 旧客户端读新 daemon 会拿到 `undefined` 并把滑条渲染成默认档——一个
/// 「显示 auto、实际 300」的界面与 v3 那次是同一种病，所以同样拒连。
///
/// **5（位深进质量阶梯）：不兼容变更。**
/// 质量档 id 全部改名，两个维度都写进去（`pcm48k` ⇒ `pcm48k16`，另加
/// `pcm48k24` / `pcm48k32f` 两档）；`QualityStop` 新增 `depth`；
/// `QualityStats` / `SessionInfo` 新增 `wire_depth`。
/// 旧客户端读新 daemon 会在 `QUALITY_LABEL_KEY` 里查不到任何一个新 id，于是把
/// 六档全画成原始 id（`pcm48k32f`），并且**永远显示不出位深**——一个「设了
/// 24 bit、界面上没有任何一处提到位深」的界面与 v3/v4 那两次是同一种病，
/// 所以同样拒连。
///
/// ⚠ **必须同步改的两处**（不在本 crate，改这里就得改它们，否则 App 拒连）：
///   - `app/src-tauri/src/main.rs` 的 `const IPC_VERSION: u32`
///   - `app/frontend/src/ipc/client.ts` 的 `export const IPC_VERSION`
/// 两处都做**严格相等**校验（`main.rs` 的 `port_alive` 分支会直接报版本不符），
/// 所以它们与本常量是一个原子的三件套。
pub const IPC_VERSION: u32 = 5;

pub use audiohub_core::audio::DevicesReport;
pub use audiohub_core::dsp::ToneVerdict;
pub use audiohub_core::latency::{DevLatency, DropMode, LatSource};
pub use audiohub_core::permissions::{
    PermissionKind, PermissionState, KIND_LOCAL_NETWORK, KIND_MICROPHONE, KIND_SYSTEM_AUDIO,
};
pub use audiohub_core::sysaudio::VirtualCard;
pub use audiohub_core::volume::VolumeState;
pub use audiohub_net::identity::PairedPeer;
/// The machine-wide mode (plan §13). Defined in `audiohub-net` because it is
/// also a wire type (`SessionMsg::ModeState`) and that crate cannot depend on
/// this one; re-exported here so IPC clients have a single import.
pub use audiohub_net::mode::{Mode, MODE_A, MODE_B, MODE_SHARE};

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
    /// Open this session even though the current mode does not allow the UI to
    /// open one. CLI/probe only. Two modes refuse a plain `session.open`, for
    /// unrelated reasons:
    ///   - `B` (spec-m5b §6.1): the SYSTEM's device selection opens sessions. A
    ///     UI that could also open one by peer would have turned mode B back
    ///     into mode A with different labels.
    ///   - `Share` (plan §13): this machine does not use other machines at all
    ///     while it is the one being used.
    ///
    /// It does NOT override the other half of the exclusion: a peer's
    /// `OpenStream` is refused by the peer's own daemon whenever that peer is
    /// not in `Share`, and no flag on this side can reach that decision. That
    /// asymmetry is deliberate — the probes need to drive their own daemon, and
    /// nothing needs to defeat somebody else's.
    #[serde(default, rename = "override")]
    pub override_mode: bool,
}

/// `settings.set` 收得下的**全部**字段名。契约的一部分，不是文档。
///
/// # 为什么这张表必须存在
///
/// `latency` / `quality` 曾经**只有 UI 一条写入路径**：`ipcserv.rs` 读它们，
/// `Settings.tsx` 写它们，而 `audiohub ctl settings` 连这两个 flag 都没有。
/// 用户去命令行核对时拿到的是 `error: unexpected argument '--latency' found`，
/// 于是「档位到底有没有被下发」这件事在不开窗口的情况下无法验证——而这条回路
/// 的失效恰恰是无声的（见 `audiohubd::servo::ServoObs` 里那六种解释）。
///
/// 缺口不是谁写错了一行，是**没有任何一处会因为少一个入口而变红**：
/// 后端有测试（字段读得对）、前端有测试（按钮点得动），两边都绿，中间少一条腿。
///
/// 这张表把「有哪些可写字段」变成一个两端都要对齐的**单一事实**：
///   - daemon 侧 `settings.set` 必须真的honour每一个键（`transport_tests.rs`）；
///   - CLI 侧每一个键必须有一个 flag 到得了（`ctl.rs` 的 tests）。
/// 加字段时忘了任何一端，那一端的测试就红。
pub const SETTINGS_WRITABLE_KEYS: &[&str] = &[
    "mode",
    "remove_virtual_on_disconnect",
    "mark_offline_devices",
];

/// Daemon-owned settings, `settings.get` / `settings.set` (spec-m5b §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonSettings {
    /// What the user asked for (plan §13): `Share` | `A` | `B`.
    ///
    /// Called `consumer_mode` until IPC v3. The rename is not cosmetic: with
    /// `Share` in the set the field no longer names a *consumer* choice, it
    /// names which side of the exclusion this machine is on. A field whose name
    /// contradicts one of its own values is exactly the "identifier drifting
    /// from its referent" failure this project has paid for repeatedly.
    pub mode: Mode,
    /// What is actually in force. `B` only when the driver is usable, so the
    /// two ends can no longer disagree for long about which mode is live.
    /// `Share` is always available — it needs neither driver nor capture
    /// permission, which is why it is also the default.
    pub effective_mode: Mode,
    /// plan §7.3: remove a peer's virtual devices while it is disconnected.
    pub remove_virtual_on_disconnect: bool,
    /// Append `（离线）` to a disconnected peer's device names, so "no sound"
    /// is visible in the system's own device list (spec-m5b OPEN QUESTION 1).
    pub mark_offline_devices: bool,
    /// 延迟滑条的固定档（毫秒，升序）。**daemon 是唯一真值源**——前端不许自己
    /// 写一份，否则两边的「有哪些档」会各自演化，而分歧不会有任何报错。
    ///
    /// ⚠ plan §15 之后**档表仍然是全局的，档位选择不是**：档表是这台机器的
    /// **能力**（这个 build 支持哪几档），档位是用户对**某一台对端某一个方向**
    /// 的选择。两件事不该一起搬，所以 `latency`/`quality` 走了、这两张表留下。
    pub latency_stops_ms: Vec<u16>,
    /// 质量滑条的完整档位表，**含不可用档**（UI 画成灰刻度）。
    pub quality_stops: Vec<QualityStop>,
    /// Virtual-device slots the attached driver offers, and how many are bound.
    pub hal_capacity: u8,
    pub hal_used: u8,
}

/// 一台对端 × 一个方向的两个**目标**档位（plan §15）。
///
/// 「方向」是**用户视角**的收/发，不是执行器所在的那一端——两者交叉，
/// 见 [`PeerTransportView`]。这个类型只在**本机存储与 UI** 之间流动，
/// 线上永远不出现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerTransportDir {
    /// `"auto"` 或 [`LATENCY_STOPS_MS`] 里的毫秒数。**这是目标，不是实测值**——
    /// 设 300 时系统会主动把缓冲填到 300，而不是「系统只能做到这么慢」。
    /// UI 必须把这句话说出来（plan §14 附）。
    pub latency: String,
    /// `"auto"` 或某个可用档位 id（见 [`transport::quality_stops`]）。
    pub quality: String,
    /// 盘上写着一个本 build 不认识的质量档时，**它原来的字符串**；
    /// 此时上面的 `quality` 已经被重置成默认（`auto`）。`None` = 一切正常。
    ///
    /// # 为什么要把它报出来，而不是静默重置
    ///
    /// 这一格的前身是一层**静默翻译**（旧 id `pcm32k` → `pcm32k16`）。那层代码
    /// 自己制造了一个真回归：前端要镜像同一张表，而三条读路径里有一条
    /// （`PeerTransport.transportCells`）漏掉了它，于是同一个存盘值在详情页
    /// 显示「PCM 32 kHz · 16 bit」、在总览里显示裸的 `pcm32k`，
    /// **没有任何一处会报错**。
    ///
    /// 静默重置只是把同一个病换个方向：用户的选择消失了而界面照旧自洽。
    /// 所以重置照做（不能让一个执行不了的值留在那里），但**必须说出来**。
    #[serde(default)]
    pub quality_reset_from: Option<String>,
    /// 同上，延迟档那一格。
    #[serde(default)]
    pub latency_reset_from: Option<String>,
}

impl Default for PeerTransportDir {
    fn default() -> Self {
        PeerTransportDir {
            latency: LATENCY_AUTO.to_string(),
            quality: QUALITY_AUTO.to_string(),
            quality_reset_from: None,
            latency_reset_from: None,
        }
    }
}

/// 一台对端的四个档位 + 对端推给本机的那两个（plan §15）。
///
/// # 为什么「本机设的」与「对端推来的」是两组字段，绝不合并
///
/// 合并之后「这个 300 是我设的还是对端要求的」就再也答不出来，而那正是共享
/// 模式的详情页唯一要回答的问题：本次事故里 30-win 的档位是 `min` 且从未被
/// 设过，这件事**在两台机器的任何一个界面上都不可见**。
///
/// # 交叉的那一半（照字面实现会造出一条永不生效的回路）
///
/// 两个档位的执行器在**相反的端**上：延迟的执行器是**接收侧**的 jitter
/// buffer，质量的执行器是**发送侧**的阶梯格号。于是消费者设的四个值里，
/// 跨到线上的是交叉的一半：
///
/// | 用户设的 | 执行器在 | 走线 |
/// |---|---|---|
/// | `recv.latency` | 本机 rx 的 JB | 本地 |
/// | `recv.quality` | **对端** tx 的阶梯 | **推给对端**（`tx_quality`） |
/// | `send.latency` | **对端** rx 的 JB | **推给对端**（`rx_latency`） |
/// | `send.quality` | 本机 tx 的阶梯 | 本地 |
///
/// 一句话记法：**每一端推的是「执行器在对面」的那个旋钮**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerTransportView {
    /// 本机作为**消费者**时对这台对端设的档位。共享模式下照存不误、只是不生效
    /// （切回 A/B 时它是这台对端的既有设置，丢掉等于每次切模式重设一遍）。
    #[serde(default)]
    pub recv: PeerTransportDir,
    #[serde(default)]
    pub send: PeerTransportDir,
    /// 对端推来、**执行器在本机接收侧**的延迟档（= 对端的 `send.latency`）。
    ///
    /// 字段名说的是**执行器**，不是用户看到的收/发。`None` = 对端没有对这一项
    /// 表态 ⇒ UI 显示「未设定 · 按自动运行」，**不显示 0、不显示本机存的值**。
    #[serde(default)]
    pub peer_rx_latency: Option<String>,
    /// 对端推来、**执行器在本机发送侧**的质量档（= 对端的 `recv.quality`）。
    #[serde(default)]
    pub peer_tx_quality: Option<String>,
    /// 连通性档位（plan §16.2）：`"auto"` | `"tier0"` | `"tier1"`。
    ///
    /// **每对端一个，不分方向**——两个方向共用一条控制连接，降级之后也共用一条
    /// 媒体传输。它与 `recv`/`send` **并列**而不是嵌进去：嵌进去等于在契约表面
    /// 上宣布「可以一个方向 tier 0、另一个 tier 1」，而那是 `peer_transport.rs`
    /// 明确要防止后人去实现的一句话。
    ///
    /// 这是**用户的选择**，不是链路的现状：`"auto"` 说的是「让 daemon 决定」，
    /// 不是「现在跑在 tier 0 上」。现状要另一个字段（`transport_tier`，尚未落地，
    /// plan §16.4），两者**不得互相冒充**——§16.4 第 5 条那条红线就是这个。
    #[serde(default = "default_tier")]
    pub tier: String,
    /// 装载时不被认识、已被重置的连通性档串。`None` = 一切正常。与
    /// `*_reset_from` 同纪律：静默重置与静默翻译是同一个病。
    #[serde(default)]
    pub tier_reset_from: Option<String>,
    /// 哪一侧可以发起连接：`"both"` | `"outbound_only"` | `"inbound_only"`。
    ///
    /// 与 `tier` 并列而不是从它推导：tier 2 的隧道**不一定**是单向的，单向的
    /// 通路也**不一定**是 tier 2（一台钉在 tier 0 的机器照样可能只能被拨）。
    /// 从一个推另一个会在两种情形里各错一次。
    #[serde(default = "default_dial_policy")]
    pub dial_policy: String,
    #[serde(default)]
    pub dial_policy_reset_from: Option<String>,
}

fn default_dial_policy() -> String {
    "both".to_string()
}

fn default_tier() -> String {
    "auto".to_string()
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
    /// How many `Bind Set` / `Bind Clear` the driver refused, or performed
    /// only halfway. Monotonic for the life of the daemon.
    ///
    /// This exists because a driver can be perfectly connected and still fail
    /// to publish a device: `driver_connected` stays true, the frame counters
    /// keep moving, and `devices[].state` reports whatever the daemon last
    /// intended. Windows M6-2 shipped exactly that shape — `state: "bound"`,
    /// `driver_connected: true`, and an empty system speaker list. Nothing in
    /// this contract could express it, so nothing upstream could show it.
    #[serde(default)]
    pub bind_failures: u64,
    /// The most recent bind failure in words, including the driver's own
    /// failure stage and NTSTATUS. `None` once a bind succeeds again.
    #[serde(default)]
    pub last_bind_error: Option<String>,
    /// How many binds SUCCEEDED but had to fall back to the generic direction
    /// names because the peer's own name could not be applied (Windows only).
    /// Monotonic for the life of the daemon.
    ///
    /// Deliberately not folded into `bind_failures`: these devices exist and
    /// work, so calling them failures would make a naming problem look like an
    /// outage. Deliberately not omitted either — every peer's devices then read
    /// alike, and a user with two machines paired has no way to tell which
    /// speaker is which and nowhere that says why.
    #[serde(default)]
    pub endpoint_name_fallbacks: u64,
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
    /// Q3：可用带宽（Hz）= 线上采样率的一半（Nyquist 上限）。
    ///
    /// ⚠ **它与 `wire_rate_hz` 差 2 倍，而两者在界面上都会写成「kHz」。**
    /// 2026-08-04 用户实测：设置里选了 `PCM 48 kHz`（采样率），卡片显示 `24 kHz`
    /// （本字段），于是判定「设置没生效」——而设置生效得好好的。任何呈现本字段
    /// 的地方**必须同时说明它是带宽**，否则它会被读成用户刚设的那个数字的反例。
    /// 面向用户的那一格显示 `wire_rate_hz`（与设置同量纲），本字段进明细。
    pub bandwidth_hz: u32,
    /// 这条流**线上的采样率**（Hz）。与设置里的质量档同量纲、同数字：
    /// `pcm48k` ⇒ 48000。
    ///
    /// 之所以是一等字段而不是 `bandwidth_hz * 2`：见 `audiohub_net::secure::
    /// QualityReading::wire_rate_hz` 的论证（今天是恒等式，将来 Q3 换成实测频谱
    /// 就不是了，而 ×2 的读方届时不会报错，只会撒谎）。
    ///
    /// `#[serde(default)]` ⇒ 旧 daemon 省略它 ⇒ 0 ⇒ UI 显示「—」。
    /// **0 不是采样率**，读取方不许拿它当数用。
    #[serde(default)]
    pub wire_rate_hz: u32,
    /// 这条流**线上的位深**：`"s16" | "s24" | "f32"`。`""` = 未知。
    ///
    /// 与 `wire_rate_hz` 一样是一等字段，理由逐字相同：让读方从 codec 推一遍，
    /// 就是在读方复刻一份 codec → 位深的映射表，两处一漂**没有任何地方会报错**。
    ///
    /// ⚠ **`""` 是「不知道」，不是「16 位」。** 位深进阶梯之前线上恒为 16 位，
    /// 所以「空就当 s16」这个兜底今天碰巧是对的——而它正是本字段要消灭的那种
    /// 「看起来对、其实是编的」。UI 上空值必须显示成「—」。
    ///
    /// ⚠ **不报数字 `32`**：`32` 在整数与浮点之间是歧义的，而位深进阶梯这件事
    /// 的全部目的就是消歧。32 位浮点报 `"f32"`。
    #[serde(default)]
    pub wire_depth: String,
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
    /// 对端在**它那一侧**测到的音质（`SessionMsg::StageReport` 回传）。
    ///
    /// # 为什么非有不可
    ///
    /// 音质三分量全是**接收侧**概念：PLC、欠载、静音填充只在收端发生。于是
    /// 一条纯发送的流，`quality` 恒为 `None`——实测 `[spk/send] quality = None`，
    /// 界面上音质那一格**永远**空着，而链路其实好得很。
    ///
    /// 这与本项目栽过的 `jb_underruns = 0` 假象是同一个病：只看本机方向，
    /// 就把「我这侧无从观测」误当成了「链路无损」。
    ///
    /// `quality` 与本字段**至多一个**非空（一条流不会两侧都是接收端）。
    /// UI 取 `quality ?? peer_quality`，并标出这一格来自对端的测量。
    #[serde(default)]
    pub peer_quality: Option<QualityStats>,
    pub received: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    /// 单向时延**展布**：滚动窗口内 `transit` 的 p95 减同窗口最小值，毫秒。
    /// `None` = 窗口还不够长（**不是 0**）。
    ///
    /// # 它与 `jitter_ms` 并存，不是替代
    ///
    /// `jitter_ms` 是 RFC 3550 的一阶差分 EWMA；`media.rs` 的 `update_target`
    /// 早就自陈要的是另一个量（相对最早到达的离散度高分位，NetEq 的
    /// relative delay）。在 UDP 上这是精度问题，在 **TCP 上是有无问题**：
    /// TCP 的失效形态是「停顿一下、然后成串送达」，串内相邻差分近似 0，
    /// 于是一阶差分的分位数**恰好在链路最糟时系统性低估**。
    ///
    /// 所以 Tier 1/2 的 AUTO 用这一个，**Tier 0 继续用 `jitter_ms`**——换掉
    /// Tier 0 会同时改变今天所有用户的 AUTO 与 JB 定深，而本轮没有对照数据
    /// （design §3.4 的范围纪律）。两个数并存上报，正是为了攒出那份数据。
    ///
    /// 接收会话报本机测的；发送会话报**对端**测的（`SessionMsg::Stats`
    /// 回传）——纯发送的流本机没有接收侧，与 `peer_quality` 同一条不对称。
    #[serde(default)]
    pub spread_ms: Option<f64>,
    /// **Payload** bitrate over the last ~3 s. `None` = not enough window yet.
    ///
    /// # Three things this field used to get wrong
    ///
    /// 1. **It was a lifetime average**, so it had no inverse. On a session that
    ///    had been up 2400 s, switching the rung changed 20 s of traffic — 0.8%
    ///    of the denominator — and the reading moved 0.34%. Measured: the three
    ///    48 kHz rungs, whose true payload rates are 768 / 1152 / 1536 kbps, all
    ///    read back as 1469.2 / 1464.3 / 1467.0. Both directions were affected;
    ///    the receive side reported 1452.0 / 1447.8 / 1448.9 for the same three,
    ///    i.e. +89.1% / +25.7% / −5.7% against the truth.
    /// 2. **The two directions used different numerators**: send counted whole
    ///    datagrams, receive counted plaintext. One stream read 1525 kbps on one
    ///    machine and 1458 on the other and nothing could flag it.
    /// 3. `wire_bytes`, added specifically so that (1) could be worked around by
    ///    differencing, **was never assigned on the send side at all** — it was
    ///    a hard zero on every `send` session.
    ///
    /// Now: a sliding window over payload bytes, same numerator on both sides.
    /// Directly comparable to the `kbps` of the quality stop the user picked
    /// (`rate x depth`, mono). For what the link actually costs, including
    /// framing, difference [`SessionStats::datagram_bytes`].
    ///
    /// `None` rather than `0.0` when the window is too short: "no measurement
    /// yet" and "nothing is flowing" are different claims.
    #[serde(default)]
    pub bitrate_kbps: Option<f64>,
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

    // ---- plan §15 / §14 裁定 4：**这条流此刻在执行的目标档**。
    //
    // 存在的理由逐字来自 plan §14 附：「用户看到 300 ms 时必须能分辨**这是自己
    // 设定的目标**而非系统能力不足——当前界面对此一个字都没说，是本次误判的
    // 直接成因」。没有这两个字段，UI 只能拿全局设置去猜某一条流的目标，而
    // §15 之后全局设置根本不存在了。
    /// 这条**接收**流的延迟目标（`LatencyTarget::as_wire()`）。
    /// `None` = AUTO 或本流没有接收侧（延迟的执行器只在接收端）。
    #[serde(default)]
    pub latency_target: Option<String>,
    /// 这条**发送**流的质量目标。`None` = AUTO（阶梯当家）或没有发送侧。
    #[serde(default)]
    pub quality_target: Option<String>,
    /// 目标是谁定的：`"local"`（本机是消费者，自己设的）| `"peer"`（本机是
    /// 提供者，档位由使用方推来）。`None` = 这条流上没有目标在执行。
    ///
    /// **这一个字段就是「两个来源绝不合并」那条规矩的可执行形式。** 合并之后
    /// 「这个 300 是我设的还是对端要求的」就再也答不出来，而共享模式的机器
    /// 需要回答的正是这个问题。
    #[serde(default)]
    pub target_from: Option<String>,
    /// 目标够不到，已经贴在物理下限 / 上限上。**只在闭环（真有实测值）时为真**：
    /// 开环下地板是假设的 0，拿它断言物理事实等于凭空宣布一个没测过的结论。
    #[serde(default)]
    pub at_floor: bool,
    #[serde(default)]
    pub at_ceiling: bool,
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

    // 水位控制的现场读数。没有它们就没法把「深度低」和「收敛机制在跑」分开：
    // 一条从来没进过高水位的链路和一条被平滑吐回来的链路，深度读数完全一样。
    /// 平滑收敛（两帧交叉淡化拼成一帧）的次数。
    #[serde(default)]
    pub jb_accel_events: u64,
    /// 平滑收敛累计吃掉的帧数（每次 1 帧）。**已计入 `jb_dropped`**——
    /// 那个字段的语义「late + catch-up drops」没变，这里回答的是
    /// 「其中有多少走了交叉淡化那条平滑路径」。
    #[serde(default)]
    pub jb_accel_frames: u64,
    /// 想收敛但因为素材会抵消而推迟的 tick 数。持续增长 = 对端在送一段恰好
    /// 反相的稳态纯音，收敛降到死线节律（5 s 一次）。
    #[serde(default)]
    pub jb_accel_deferred: u64,
    /// 欠载惩罚项（帧）。`jb_target_frames` 里含它。**非零 = 这条链路让我们
    /// 付过代价，水位是它自己长上去的**，不是整定拍出来的。
    #[serde(default)]
    pub jb_underrun_penalty_frames: u32,

    /// 这条流的**明文载荷字节数**（lifetime 累计，AEAD 之内、不含包头与标签）。
    ///
    /// **两个方向都有**：接收侧数解密后的 `plain.len()`，发送侧数
    /// `TxShared::sent_payload_bytes`（同一条「内核收下了才算」的判据）。
    /// 此前只有接收侧赋值，发送侧恒为 0 —— 于是本字段承诺的那条「唯一硬证据」
    /// 在 `send` 会话上根本不存在，而契约文本照旧写着它是硬证据。
    ///
    /// # 它与 [`SessionStats::bitrate_kbps`] 的分工
    ///
    /// `bitrate_kbps` 已经是滑动窗口，日常读它就够。这个计数器是**可差分**的
    /// 原始量：取两次、除以两次之间的墙钟，得到的是不受任何窗口整定影响的
    /// 稳态码率——回归脚本与跨机验证要的是这一个，因为它的误差**恰好为零**
    /// （`Δbytes / Δpkts` 里不含墙钟）。
    #[serde(default)]
    pub wire_bytes: u64,

    /// 这条流的**整数据报字节数**（lifetime 累计，含包头与 AEAD 标签）。
    ///
    /// 与 `wire_bytes` 分成两个字段而不是共用一个名字：这两个量在同一条流上
    /// 差着约 56 B/包，而深档按 5 ms 分包、每 10 ms 付两份包头 —— 于是「开销
    /// 占比」本身随档位变。此前发送侧报前者、接收侧报后者却共用一个字段名，
    /// 两端的数对不上而没有任何一处会报错。
    ///
    /// 要回答「这条链路实际吃多少带宽」用它；要回答「位深有没有生效」用
    /// `wire_bytes`。**仍不含 IP/UDP 头**（28 B/包）：那两层不归本进程所有，
    /// 报出来就是在替内核估算。
    #[serde(default)]
    pub datagram_bytes: u64,

    // ------------------------------------------------ 位深进阶梯带来的两个降级
    /// 深档（5 ms 分包）里「搭档半帧没来、按半帧隐藏交付」的次数（lifetime）。
    ///
    /// **必须单独上报**：半帧隐藏交付的是一个**长度完整**的帧，JB 因此不记 PLC、
    /// 不记 underrun —— 若不单独计数，「深档丢了一半的包」这件事在整套遥测上
    /// 一个字都不会出现。它同时以 0.5 帧的权重计入 Q1 隐藏率
    /// （`quality::conceal_ratio`）：一次半帧隐藏正好伪造了半帧音频。
    #[serde(default)]
    pub jb_half_conceal: u64,
    /// 包头声明的线上格式与载荷长度对不上、因而被丢弃的包数（lifetime）。
    ///
    /// **非零 = 两端对线上格式的理解分了岔**（典型：一端发 s16 却把包头写成
    /// s24）。这个态在耳朵里是周期性静音洞，而 `DecodeStats.ragged` 与 JB 的
    /// 五个计数器对它**全部免疫**——判据见 `engine::handle_datagram`。
    #[serde(default)]
    pub format_mismatch: u64,
    /// 通过了包头解析、却**没通过 AEAD** 因而被丢弃的媒体包数（lifetime）。
    ///
    /// 在 UDP 上这是「路径上有人往这个 stream id 里灌字节」的唯一证据。
    /// 在 Tier 1 上它还多一层用处：`tcp_media.frames_read` 对**每一个**
    /// `Kind::Media` 帧递增，认证过不过都算，于是注入流量会同时抬高
    /// `frames_read` 而这条会话的 `received` 不动 —— 两个数之间那道缺口
    /// 只有这个字段能命名。
    ///
    /// `control.rs` 的 `MediaAttach` 文档承诺偷到票的人「只能注入会被
    /// **计数并丢弃**的字节」。这就是那个计数器；在它存在之前那句承诺是
    /// 假的（丢弃是真的，计数不存在）。
    #[serde(default)]
    pub auth_failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub peer_fingerprint: String,
    pub peer_name: String,
    pub kind: String,               // KIND_MIC | KIND_SPK
    pub dir: String,                // "send" | "recv"
    /// 这条流**线上的采样率**（Hz）。`0` = 两侧都报不出。
    pub sample_rate: u32,
    /// 这条流**线上的位深**：`"s16" | "s24" | "f32"`；`""` = 报不出。
    ///
    /// 与 `sample_rate` 成对：位深进阶梯之后，只写一个维度的读数是**有歧义的**
    /// （`48 kHz` 说不出它是 16 还是 24 位）。呈现时两个维度必须一起写全，
    /// 为了短而只写一个，就回到了这次改动要消灭的那个歧义。
    #[serde(default)]
    pub wire_depth: String,
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
    /// **The third connection state** (design §4.2 item 2, plan §16): this peer
    /// is set to inbound-only, so this machine never dials it and it is
    /// expected to connect to us.
    ///
    /// `online: false, awaiting_inbound: true` does **not** mean offline. On a
    /// tunnel that only carries connections one way, a peer sitting here is a
    /// peer whose setup is working exactly as configured; rendering it as
    /// offline would put a permanent fault marker on a working system, and the
    /// user's only available conclusion would be that the software is broken.
    ///
    /// Carried rather than derived from `reconnecting == false`, because that
    /// is also what a peer we simply never dialled looks like — the two need
    /// opposite words, and only the daemon can tell them apart.
    #[serde(default)]
    pub awaiting_inbound: bool,
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
    /// 控制面 Ping/Pong 的**单向**网络延迟估计（min-RTT / 2，毫秒）。
    ///
    /// # 它与媒体会话无关，这正是它存在的理由
    ///
    /// `SessionStats.pipeline` 里的延迟是**按流**的，没有会话就整块没有，
    /// 于是「已连上但还没人在用」时界面上一个数字都没有——用户看到的是一片空白，
    /// 分不清「没连上」「连上了但坏了」「连上了只是闲着」。这个字段挂在
    /// **连接**上（`ConnShared::clock`），配对连上就有。
    ///
    /// ⚠ **它不是端到端总延迟，UI 必须把这一点说出来。** 实测 RTT 0.58 ms 而
    /// 感知延迟约 1000 ms，相差三个数量级（plan §7.6 严谨性红线：不得以网络
    /// RTT 冒充音频延迟）。延迟的绝大部分在缓冲与设备侧。
    ///
    /// `None` = min-RTT 窗口还没攒够样本（约 8 拍）。**宁可 None 也不拿一个
    /// 未滤波的 RTT 顶上。**
    #[serde(default)]
    pub net_ms: Option<f64>,
    /// 最近一次 Pong 的 RTT（毫秒），交叉校验用。`None` 同上。
    #[serde(default)]
    pub rtt_ms: Option<f64>,
    /// The mode this peer last told us it is in (plan §13 推论 1), from
    /// `SessionMsg::ModeState` on the live control channel.
    ///
    /// **`None` = we do not know**, and the UI must render that as nothing at
    /// all — never as "usable" and never as "unusable". It is `None` while the
    /// peer is offline (a mode remembered from a previous connection is a
    /// statement about the past, and this field is used to decide what to
    /// offer *now*), and for the few milliseconds between a channel coming up
    /// and its first advertisement landing.
    ///
    /// Deliberately not persisted with the peer record for the same reason.
    #[serde(default)]
    pub peer_mode: Option<Mode>,
    /// This peer has told us it is in a mode that cannot serve us.
    ///
    /// Carried rather than derived by the UI from `peer_mode`, because the two
    /// reasons `peer_mode` can be `None` need opposite treatment and only the
    /// daemon can tell them apart:
    ///
    /// | `peer_mode` | `peer_unusable` | meaning |
    /// |---|---|---|
    /// | `Some(Share)` | `false` | usable |
    /// | `Some(A)` / `Some(B)` | `true`  | it is a consumer right now |
    /// | `None` | `false` | offline, or nothing advertised yet — say nothing |
    /// | `None` | `true`  | it advertised a mode this build cannot name |
    ///
    /// That last row is why the flag exists: an unrecognised mode must fall to
    /// "do not offer it", and a UI deriving usability from `peer_mode` alone
    /// would have to read `None` as "fine" to keep the offline case quiet.
    #[serde(default)]
    pub peer_unusable: bool,
    /// plan §15：这台对端的四个传输档位（收/发 × 延迟/音质）+ 它推给本机的两个。
    ///
    /// **不放进 `peer`（那个 `#[serde(flatten)]` 的 `PairedPeer`）**：那里装的
    /// 是「对方告诉我的身份」，而这里是「我自己设的」。混成一层之后 UI 就再也
    /// 分不出这两件事——而共享模式的详情页存在的唯一意义正是分出它们。
    #[serde(default)]
    pub transport: PeerTransportView,
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
/// - "settings.set"      {mode?, remove_virtual_on_disconnect?,
///                        mark_offline_devices?}
///                                             -> DaemonSettings
///       `latency` / `quality` **不再在这里**（plan §15）：它们是每对端 × 每
///       方向的选择，走 "peers.set_transport"。旧客户端传这两个键会被拒绝，
///       而不是被静默收下——静默收下正是本项目栽过六次的那个形状。
///       The mode is DAEMON-owned global state (plan §7.1/§13) and the three
///       modes are mutually exclusive, so setting it is never only a display
///       change. Switching AWAY from `share` closes every session a peer opened
///       on us and tells those peers why; switching TO `share`, or to `a`,
///       removes every virtual device; switching to `b` recreates them under
///       the same UIDs. No confirmation dialog (plan §7.1, frozen).
/// - "peers.pair"        {addr, pin}           -> PeerState
///       The initiator half of M3 pairing, moved out of the CLI so a pairing
///       done anywhere is visible to the device coordinator immediately.
/// - "peers.unpair"      {peer}                -> {fingerprint}
///       Removes the pairing, tells the PEER (so its copy of our devices goes
///       away too), closes sessions and removes the virtual devices.
/// - "peers.set_alias"   {peer, alias}         -> {fingerprint, display_name}
///       Renames the peer's virtual devices in place: same UID, same
///       AudioObjectID, so an application's device selection survives it.
/// - "peers.set_transport" {peer, dir, latency?, quality?} -> PeerTransportView
///       plan §15。`dir` = "recv" | "send"，**本机视角**（收 = 我取对方的音）。
///       只传要改的那一项，另一项保持原值。立刻生效：写盘之后马上把
///       「执行器在对面」的那半边（见 `PeerTransportView`）推给对端，
///       本地那半边灌进本机每条流的原子量。**没有重启、没有重连。**
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
    pub const PEERS_SET_TRANSPORT: &str = "peers.set_transport";
    /// 连通性档位（`auto` | `tier0` | `tier1` | `tier2`），**每对端一个**，
    /// 可选带 `dial_policy`（`both` | `outbound_only` | `inbound_only`）。
    ///
    /// 两者同一个入口、同一次落盘：它们合起来才是「这台对端怎么连」这一个决定，
    /// 而每一次写入都要拆一次控制连接——分两次下等于让对端经历两次重连，
    /// 中间那一刻还是一个用户从没要求过的组合。
    ///
    /// # 为什么它不是 `peers.set_transport` 上的第三个字段
    ///
    /// 那个方法的 `dir` 是必填的，而 tier **不是每方向的**（两个方向共用同一条
    /// `ConnShared`：tier 0 下共用一个 UDP socket，tier 1 下共用那条媒体 TCP）。
    /// 把它塞进一个要求 `dir` 的调用里，等于在协议表面上宣布「可以一个方向
    /// tier 0、另一个 tier 1」——而 `peer_transport.rs` 上那段注释正是为了防止
    /// 有人日后去实现这句话才写的。落点仍是同一张表、同一个文件、同一条
    /// 「修改即生效」的路径，只是不撒这个谎。
    pub const PEERS_SET_TIER: &str = "peers.set_tier";
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
