// daemon IPC 的载荷形状。唯一事实来源仍是 core/audiohub-ipc/src/lib.rs；
// 这里只是它在 TS 侧的镜像。
//
// 刻意把几乎所有字段标成可选：这些对象是**外部输入**（另一个进程、可能是另一个
// 版本的 daemon）。把它们声明成必填等于用类型系统假装运行时保证——旧版本少一个
// 字段就会在某个 `.x.y` 上炸掉，而 TS 一句也不会警告。可选 + 严格空检查会逼着
// 每个读取点自己兜底，那正是我们要的。

export interface HalDeviceInfo {
  fingerprint?: string;
  slot?: number;
  generation?: number;
  state?: 'bound' | 'pending' | 'delisted' | 'free' | string;
  observed?: boolean;
  peer_connected?: boolean;
  out_name?: string;
  out_uid?: string;
  in_name?: string;
  in_uid?: string;
  io_in?: boolean;
  io_out?: boolean;
  mic_frames?: number;
  mic_dropped?: number;
  spk_frames?: number;
}

export interface HalStatus {
  registered?: boolean;
  driver_connected?: boolean;
  status_reason?: string | null;
  protocol_version?: number;
  driver_protocol_version?: number;
  devices?: HalDeviceInfo[];
  mic_frames?: number;
  mic_dropped?: number;
  spk_frames?: number;
  last_driver_msg_secs?: number;
}

export interface VirtualCard {
  id?: string;
  name?: string;
  kind?: string;
  present?: boolean;
}

/**
 * 系统音频捕获后端（core/audiohub-core/src/sysaudio.rs 的 `BackendInfo`）。
 *
 * **daemon 目前不上报这个字段**：`list_backends()` 只经 CLI `probe sysaudio --list`
 * 露出来，`DaemonInfo` 里没有它。这里先按 core 的形状声明，是为了让 daemon 补上
 * `sysaudio_backends` 的那一天，UI 无需改动就能把「可用/不可用 + 原因」如实画出来
 * （lib/sysaudio.ts 的 backendOptions 已经按「有就用、没有就承认不知道」写好）。
 */
export interface SysAudioBackend {
  id?: string;
  name?: string;
  available?: boolean;
  excludes_self?: boolean;
  note?: string;
}

export interface DaemonInfo {
  ipc_version?: number;
  fingerprint: string;
  name?: string;
  control_port?: number;
  uptime_s?: number;
  hal?: HalStatus | null;
  output_devices?: string[];
  virtual_cards?: VirtualCard[];
  /** 见 SysAudioBackend：当前 daemon 一律缺席，UI 必须能在没有它时也说得通。 */
  sysaudio_backends?: SysAudioBackend[];
}

/** PeerState.hal_device —— 模式 A 下为 null。 */
export interface PeerHalDevice {
  out_name?: string;
  out_uid?: string;
  in_name?: string;
  in_uid?: string;
  state?: string;
  observed?: boolean;
}

export interface PeerState {
  fingerprint: string;
  name?: string;
  alias?: string | null;
  display_name?: string;
  online?: boolean;
  reconnecting?: boolean;
  retry_in_s?: number;
  last_addr?: string;
  port?: number;
  added_unix?: number;
  public_key_b64?: string;
  hal_device?: PeerHalDevice | null;
  hal_reason?: string | null;
}

export interface VolumeState {
  scalar: number;
  muted: boolean;
  adjustable?: boolean;
}

export interface Verdict {
  detected?: boolean;
  snr_db?: number;
}

/**
 * 一条接收会话的音质三分量（core/audiohub-ipc 的 `QualityStats`，规格 §4）。
 *
 * 测点在 JitterBuffer pop 之后、送进播放环之前——「用户实际会听到的样本」最后一次
 * 可被观测的地方。**不要拿 loss_pct 顶替它**：丢包率是网口上的量，音质是扬声器上
 * 的量，中间差一整条流水线。
 *
 * `clip_ratio` / `clip_excess_db` 用 `| null` 而不是只写 `?`：daemon 对「这一分量
 * 还没测出来」发的就是 JSON `null`，而它与 `0`（测了，一个越界样本都没有）是两个
 * 完全不同的结论。写成 `?: number` 会让 `null` 在类型上无处安放，读取点很容易顺手
 * `?? 0` 一下——那正是这套遥测存在的理由所要消灭的那种填补。
 */
export interface QualityStats {
  /** 滚动窗口的真实跨度（秒），不是标称的 10。 */
  window_s?: number;
  /** Q1：`(plc + 3*silence) / total`，[0,1]。silence 权重 3 见 Rust 侧论证。 */
  conceal_ratio?: number;
  plc_ticks?: number;
  silence_ticks?: number;
  popped_ticks?: number;
  /** 二级证据，**不参与定级**：解释等级为何低，不定义等级。 */
  underruns?: number;
  jb_dropped?: number;
  /** Q2：本流送进混音前 |v| > 0.8 的采样占比。`null` = 这一页还没攒满。 */
  clip_ratio?: number | null;
  clip_excess_db?: number | null;
  /** Q3：`rung_rate / 2`（Nyquist）。 */
  bandwidth_hz?: number;
  /**
   * "excellent" | "good" | "fair" | "poor" | **"unknown"**。
   *
   * `"unknown"` = 等级不成立（某个分量还没读数，在场分量的 min 只是上界）。
   * **绝不可回退到某个具体等级**：`Grade::Excellent` 是最大值，
   * `min(q1, Excellent, q3) ≡ min(q1, q3)`，正是这一条恒等式让缺席一度被静默
   * 读作「良好」。UI 侧的处理是不给等级（`readQuality` 返回 `grade: undefined`）。
   */
  grade?: string;
  /** "continuity" | "level" | "bandwidth" | "none"：拖后腿的那一项。 */
  worst?: string;
  /** 本次合成少了至少一块板。等级触底时它可以与一个**确定**的 grade 并存。 */
  partial?: boolean;
}

/**
 * 声卡自身的固有延迟（core/audiohub-core 的 `DevLatency`）。
 *
 * `source` 不是修饰而是判据：`"unavailable"` 必须让总和变成缺失，`"unreliable"`
 * （蓝牙 / HDMI / AirPlay，真实 150~250 ms 而系统常只报 20~30 ms）必须让 UI 永远
 * 带「≥」。把 `unavailable` 当 0 采信，读数会漂亮且完全错误。
 */
export interface DevLatency {
  frames?: number;
  rate?: number;
  source?: 'api' | 'assumed' | 'unreliable' | 'unavailable' | string;
}

/**
 * 管线上一级缓冲的瞬时读数（core/audiohub-ipc 的 `PipelineStage`，规格 §3.2）。
 *
 * `id` 与 `lib/metrics.ts` 的 `LATENCY_STAGES[].id` 逐字一致（snake_case），中间
 * 不留映射表——`cargo test` 那边有一条测试**读 metrics.ts 那张表**逐条比对。
 *
 * `dropped` / `drift_sps` 用 `| null`：`null` 与 `0` 是两个不同的结论。
 * `dropped: null` = 这一级的丢弃发生在别的进程里（典型是 hal_spk，环满时写不进去
 * 的是驱动的 IOProc），**不是没丢过**；`drift_sps: null` = 样本点不足以判趋势，
 * **不是不漂移**。规格 §3.3 的三条诊断正是靠这两个字段的「有值 / 无值」分开的。
 */
export interface PipelineStage {
  id?: string;
  samples?: number;
  /** 该级容量；0 = 无界 / 不适用。 */
  capacity?: number;
  /** 该级**消费者**的标称速率(Hz)。播放环走设备速率（可能 44.1k）。0 = 读数无效。 */
  rate?: number;
  /** `samples * 1000 / rate`，daemon 算好直接给。`null` = 这一级读不到，不是 0 ms。 */
  ms?: number | null;
  dropped?: number | null;
  /** 满时丢哪一头。深度读数在丢头/丢尾两种语义下**完全简并**，只能靠它区分。 */
  drop_mode?: 'oldest' | 'newest' | 'none' | string;
  saturated?: boolean;
  /** 30 s 窗口深度斜率，样本/秒。 */
  drift_sps?: number | null;
}

/**
 * 一条会话的逐级延迟会计（core/audiohub-ipc 的 `PipelineLatency`，规格 §3.5）。
 *
 * **`side` 决定 `stages` 里那几级长在哪台主机上**：`"send"` 时本机是提供方
 * （采集/发送侧），`"recv"` 时本机是使用方（播放侧）。静态级表里的
 * 「对方主机 / 本机」是按 recv 写的物理定义，send 会话上正好相反。
 *
 * `sum_ms` 是全链路 Σ，`local_ms` 只是本机这一侧。**两者不可互相顶替**：
 * 拿 `local_ms` 当总延迟显示，就是让用户以为端到端只有本机侧那点数。
 */
export interface PipelineLatency {
  side?: 'send' | 'recv' | string;
  stages?: PipelineStage[];
  local_ms?: number | null;
  dev?: DevLatency | null;
  /** 对端分项（控制面回传，P0b 起）。旧 daemon / 单端部署时为空数组。 */
  peer_stages?: PipelineStage[];
  peer_local_ms?: number | null;
  peer_dev?: DevLatency | null;
  /** 对端读数的年龄（秒）。>3 即视为陈旧。 */
  peer_age_s?: number | null;
  /** 单向网络 = 控制面 min-RTT / 2。**只作一段，绝不作总数。** */
  net_ms?: number | null;
  rtt_cross_check_ms?: number | null;
  /** Σ 各级（含对端）。任一已声明分项缺失即 `null`——**绝不用 0 填补**。 */
  sum_ms?: number | null;
  /** P1：真实采样年龄。 */
  e2e_ms?: number | null;
  /** P1：`e2e_ms − sum_ms`。|residual| > 20 ms 即存在未建模的缓冲级。 */
  residual_ms?: number | null;
  clock_offset_us?: number | null;
  clock_unc_us?: number | null;
  /** "full" | "lowerBound" | "converging" | "localOnly" | "unavailable"（camelCase 直穿）。 */
  confidence?: string;
}

export interface SessionStats {
  loss_pct?: number;
  jitter_ms?: number;
  bitrate_kbps?: number;
  rung?: number;
  received?: number;
  lost?: number;
  sent_packets?: number;
  jb_depth_frames?: number;
  rung_changes?: number;
  volume?: VolumeState | null;
  verdict?: Verdict | null;
  mix_verdicts?: unknown[];
  /**
   * `null` = 这条会话没有任何可读的级（旧 daemon，或非媒体会话）。
   * **不是「延迟为 0」。**
   */
  pipeline?: PipelineLatency | null;
  /**
   * `null` = 这条会话没有音质读数：发送会话本来就没有（测点在接收侧），
   * 接收会话在 Q1 的 10 s 窗口攒够之前也没有。**不是「音质为 0」。**
   */
  quality?: QualityStats | null;
}

export type SessionKind = 'mic' | 'spk' | string;
export type SessionDir = 'send' | 'recv' | string;

export interface SessionInfo {
  id: number;
  peer_fingerprint: string;
  peer_name?: string;
  kind: SessionKind;
  dir: SessionDir;
  origin?: 'hal' | 'peer' | string | null;
  hal_device?: string | null;
  sample_rate?: number;
  channels?: number;
  stats?: SessionStats | null;
  /**
   * **daemon 目前不回这两个字段**（`SessionInfo` 里只有 kind/dir）。UI 因此无法从
   * 会话列表反推「这条 spk 流送的是系统音频还是麦克风」，只能记本地偏好——CLI/probe
   * 开的会话就会显示成本地偏好的样子。补上后这里立刻变成权威来源，见 Peers.tsx
   * 的 sessionSource()。
   */
  source?: string | null;
  backend?: string | null;
}

/** settings.get / settings.set 的回包（daemon 拥有的全局设置）。 */
export interface DaemonSettings {
  consumer_mode?: 'a' | 'b' | string;
  effective_mode?: 'a' | 'b' | string;
  latency?: string;
  quality?: string;
  remove_virtual_on_disconnect?: boolean;
  mark_offline_devices?: boolean;
  hal_capacity?: number;
  hal_used?: number;
}

export interface DiscoverResult {
  fingerprint?: string;
  instance?: string;
  name?: string;
  port?: number;
  addrs?: string[];
  paired?: boolean;
  lastSeen?: number;
}

export interface IpcEndpoint {
  port: number;
  token: string;
}
