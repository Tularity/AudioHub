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
  /**
   * 对端上报的模式（plan §13 推论 1）。
   *
   * **缺席 / null = 不知道**，界面必须什么都不说——既不是「可用」也不是
   * 「不可用」。对端离线时恒为 null（记忆里的模式是关于过去的陈述，而这个字段
   * 只用来决定此刻能不能用它）。
   */
  peer_mode?: 'share' | 'a' | 'b' | string | null;
  /**
   * 对端明确告诉我们它现在不能被使用。与 `peer_mode` 分开由 daemon 给，是因为
   * `peer_mode == null` 有两种成因、需要相反的处理：还没上报（什么都别说）vs
   * 上报了一个本版本不认识的模式（别提供它）。详见 `audiohub-ipc` 的
   * `PeerState::peer_unusable` 上那张表。
   */
  peer_unusable?: boolean;
  /**
   * plan §15：这台对端的四个传输档位（收/发 × 延迟/音质）+ 它推给本机的两个。
   *
   * **不在 `PairedPeer` 那一层**：那里装的是「对方告诉我的身份」，而这里是
   * 「我自己设的」。混成一层之后界面就再也分不出这两件事。
   */
  transport?: PeerTransportView;
  /**
   * 控制面 Ping/Pong 的**单向**网络延迟估计（min-RTT / 2，毫秒）。
   *
   * 它挂在**连接**上，与有没有媒体会话无关——这正是它存在的理由：
   * `SessionStats.pipeline` 的延迟是**按流**的，没有会话就整块没有，于是「已连上
   * 但还没人在用」时界面上一个数字都没有，用户分不清「没连上」「连上了但坏了」
   * 「连上了只是闲着」。
   *
   * ⚠ **它不是端到端总延迟，读取点必须把这一句说出来。** 实测网络 RTT 0.58 ms 而
   * 感知延迟约 1000 ms，相差三个数量级：延迟的绝大部分在缓冲与声卡侧，而那两段
   * 要等真的有音频在流动时才量得到。把它渲染进端到端总数那个槽位（同字号、同格式、
   * 同标签），就是让用户把「1 ms」读成「总延迟 1 ms」。
   *
   * `null` = min-RTT 窗口还没攒够样本。对端离线时恒为 null（记忆里的读数是关于
   * 过去的陈述，而这个字段只用来说明此刻这条连接有多快）。
   */
  net_ms?: number | null;
  /** 最近一次 Pong 的往返（毫秒），交叉校验用。`null` 的含义同 `net_ms`。 */
  rtt_ms?: number | null;
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
  /**
   * 对端在**它那一侧**测到的音质，经控制通道回传。
   *
   * 音质三分量全是**接收侧**概念（PLC、欠载、静音填充只在收端发生），所以一条纯
   * 发送的流 `quality` **结构上恒为 null**——不是这条链路不好，是本机压根量不到。
   * 少了这个字段，「送对方扬声器」那条通路的音质格永远空着，而它其实好得很；
   * 这与本项目栽过的 `jb_underruns = 0` 假象是同一个病：把「我这侧无从观测」误当
   * 成了「链路无损」。
   *
   * 与 `quality` **至多一个**非空（一条流不会两端都是接收端）。UI 取
   * `quality ?? peer_quality`，并**标出**这一格来自对端的测量——不标就等于把对端
   * 的读数冒充成本机量到的。
   */
  peer_quality?: QualityStats | null;
  /**
   * plan §15 / §14 裁定 4：**这条流此刻在执行的目标档**。
   *
   * 存在的理由逐字来自 plan §14 附：「用户看到 300 ms 时必须能分辨这是自己
   * 设定的目标而非系统能力不足」。没有它，界面只能拿全局设置去猜某一条流的
   * 目标——而 §15 之后全局设置根本不存在了。
   *
   * `latency_target` 只在**接收**流上非空（延迟的执行器在接收端），
   * `quality_target` 只在**发送**流上非空（音质的执行器在发送端）。
   * `null` 也可能是 AUTO：两者的界面表现相同（没有固定目标）。
   */
  latency_target?: string | null;
  quality_target?: string | null;
  /**
   * 目标是谁定的：`'local'`（本机是消费者，自己设的）| `'peer'`（本机是提供者，
   * 档位由使用方推来）。**这一个字段就是「两个来源绝不合并」那条规矩的可执行
   * 形式**——合并之后「这个 300 是我设的还是对端要求的」就再也答不出来。
   */
  target_from?: string | null;
  /** 目标够不到，已经贴在物理下限 / 上限上。**只在闭环（真有实测值）时为真。** */
  at_floor?: boolean;
  at_ceiling?: boolean;
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

/**
 * 质量档滑条上的一档。**含不可用档**：不可用的也要发过来，UI 才画得出那条灰刻度。
 * 把它们在服务端就滤掉，用户看到的是一条短滑条，而「本机缺 libopus」这件事无从得知。
 */
export interface QualityStop {
  /** 'auto' | 'opus64' | 'opus128' | 'opus256' | 'pcm16k' | 'pcm24k' | 'pcm32k' | 'pcm48k' */
  id: string;
  /** 码率（kbps）。'auto' 没有。 */
  kbps?: number | null;
  /** 采样率（Hz）。Opus 档没有。 */
  rate?: number | null;
  /** false = 画出来但选不中。 */
  available: boolean;
  /** 'opus' = 需要 libopus，本次构建没有链接。 */
  blocked_by?: string | null;
}

/** 一台对端 × 一个方向的两个**目标**档位（plan §15）。 */
export interface PeerTransportDir {
  /**
   * `'auto'`，或 `latency_stops_ms` 中某一档的十进制毫秒串（`'0'`/`'200'`…）。
   *
   * ⚠ **这是目标，不是实测值。** 设 300 时系统会主动把缓冲填到 300，
   * 而不是「系统只能做到这么慢」。界面必须把这句话说出来（plan §14 附）——
   * 不说的话，用户看到 300 的第一反应是「这条链路很慢」。
   */
  latency?: string;
  /** `'auto'`，或某个 `available` 的 `QualityStop.id`。 */
  quality?: string;
}

/**
 * 一台对端的四个档位 + 它推给本机的那两个（plan §15）。
 *
 * # 两组来源分开，**绝不合并**
 *
 * 合并之后「这个 300 是我设的还是对端要求的」就再也答不出来，而那正是共享
 * 模式的详情页唯一要回答的问题：本次事故里 30-win 的档位是 `min` 且从未被
 * 设过，这件事在两台机器的任何一个界面上都不可见。
 *
 * # 交叉的那一半
 *
 * 两个档位的执行器在**相反的端**上：延迟的执行器是**接收侧**的 jitter
 * buffer，音质的执行器是**发送侧**的阶梯格号。于是消费者设的四个值里，
 * 跨到线上的是交叉的一半（`recv.quality` 与 `send.latency`）。
 * 下面两个 `peer_*` 字段按**执行器**命名，不按用户视角的收/发。
 */
export interface PeerTransportView {
  /** 本机**收**这台对端（我取它的麦克风）。 */
  recv?: PeerTransportDir;
  /** 本机**发**给这台对端（我送它的扬声器）。 */
  send?: PeerTransportDir;
  /**
   * 对端推来、执行器在**本机接收侧**的延迟档（= 对端的 `send.latency`）。
   *
   * `null` / 缺席 = 对端没有对这一项表态 ⇒ 界面显示「未设定 · 按自动运行」，
   * **不显示 0、不显示本机存的那一份**（本机那份在共享模式下不生效，
   * 显示它就是撒谎）。
   */
  peer_rx_latency?: string | null;
  /** 对端推来、执行器在**本机发送侧**的音质档（= 对端的 `recv.quality`）。 */
  peer_tx_quality?: string | null;
}

/** settings.get / settings.set 的回包（daemon 拥有的全局设置）。 */
export interface DaemonSettings {
  /**
   * 用户请求的模式（plan §13）。IPC v3 前叫 `consumer_mode`——加进 `share` 之后
   * 那个名字与自己的取值相矛盾，故改名。
   */
  mode?: 'share' | 'a' | 'b' | string;
  effective_mode?: 'share' | 'a' | 'b' | string;
  /**
   * 延迟档滑条的固定档（毫秒，升序），`0` = 「尽可能低」。
   *
   * ⚠ plan §15 之后**档表仍是全局的，档位选择不是**：档表是这台机器的能力，
   * 档位是用户对某一台对端某一个方向的选择（见 `PeerState.transport`）。
   * **daemon 是唯一真值源**：档表随物理能力变，前端写死一份就会给出它自己都
   * 送不下去的档。缺席（旧服务）时前端回落到内置常量，见 Settings.tsx。
   */
  latency_stops_ms?: number[];
  /** 质量档的档位表，含不可用档（用于画灰色刻度）。缺席时同样回落。 */
  quality_stops?: QualityStop[];
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
