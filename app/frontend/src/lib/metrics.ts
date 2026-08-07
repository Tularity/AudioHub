// 一级指标（延迟 / 音质）的数据形状、分级规则与读取入口。
//
// 贯穿本文件的红线（规格 §3.3 / 附录约束 1）：
//   任一**已声明存在**的分项缺失 ⇒ 该项为 undefined，绝不用 0 填补。
// 用 0 填补会让蓝牙耳机（真实 +150~250 ms）看起来和模拟输出一样好。同一条红线在
// 音质那边的形态是：某个分量还没读数时**等级本身不成立**，不是「按在场的那几项
// 算一个」——见 readQuality 的注释。
//
// 当前状态：
//   - readQuality  已接上 `SessionStats.quality`（daemon 的 P0q），本机无测点时
//     回退到对端回传的 `peer_quality` 并置 `fromPeer`——纯发送的流本机恒无音质读数。
//   - readPeerNet  连接级的网络单程（`PeerState.net_ms`），**只是一段，不是总数**。
//   - readLatency  已接上 `SessionStats.pipeline`（P0a 的本侧分项、P0b 的对端分项、
//     P1 的 net/dev/e2e/residual）。**总数的取法见 readLatency 头部的表**：
//     `sum_ms` 在场才是端到端；只有 `local_ms` 时读数不覆盖整条链路，
//     `coversWholeChain()` 为假，界面据此撤掉等级词并标出「未含对方主机」。

import type { MsgKey } from '../i18n';
import type { DevLatency, PeerState, PipelineStage, SessionInfo } from '../ipc/types';

// ---------------------------------------------------------------- 延迟

/**
 * 一级色带的四段。命名面向用户，不出现 FIFO / JitterBuffer 这类内部词。
 *
 * ## 为什么没有第五段「设备」（`cap_dev` / `play_dev` 的归属，规格 §3.2 级 2 与级 9）
 *
 * 两个设备级分别并进 `capture` 与 `playback`，**不另立一段**。三条理由，第一条
 * 是决定性的：
 *
 * 1. **色带是一条按音频流向排的时间轴，而「设备」在这条轴上出现两次**
 *    （对方的采集声卡在最前，本机的播放声卡在最后）。一个在时间轴上不连续的
 *    集合无法占据一个连续色块——硬画就等于把两端的东西挪到中间，色带从此不再
 *    是「左到右 = 声音走过的路」，四段对齐的小数字也跟着失去意义。
 * 2. **四段之和必须等于总数**，否则色带在撒谎。设备级并进相邻段能保住这条不变式；
 *    单立一段同样能，但要以放弃第 1 条为代价。
 * 3. P0 阶段两个设备级恒为 `unavailable`（平台查询是 P1 的活），**一条永远空着的
 *    色带段会被读成「测过了，是 0」**——那正是本文件顶部那条红线在视觉上的形态。
 *    并进相邻段则退化成「这一段少算了一点」，而那一点正好由「≥」前缀说出来。
 *
 * 设备级并没有因此被藏起来：它们在就地展开的逐级明细里各占一行，缺席时显示
 * 「未知」，并且是「≥」前缀与 `latency.conf.lowerBound` 那句话的唯一来源。
 *
 * 其余归属（本轮核对）：`send_pace` 是提供方的打包节拍 ⇒ `capture`；
 * `bridge_ring` / `hal_mic` 与 `play_ring` 是使用方的三条**并行**尾级 ⇒ 同属
 * `playback`，且归并时取 max 不相加（见 PARALLEL_TAILS）。
 */
export const LATENCY_SEGMENTS = ['network', 'capture', 'buffer', 'playback'] as const;
export type LatencySegmentId = (typeof LATENCY_SEGMENTS)[number];

export const SEGMENT_LABEL: Record<LatencySegmentId, MsgKey> = {
  network: 'latency.seg.network',
  capture: 'latency.seg.capture',
  buffer: 'latency.seg.buffer',
  playback: 'latency.seg.playback',
};

/** 逐级明细的九个测点（规格 §3.2），按音频流向排列。 */
export interface StageSpec {
  id: string;
  nameKey: MsgKey;
  descKey: MsgKey;
  /** 该级所在主机；null = 跨两端（网络）或不可归属（残差）。 */
  host: 'peer' | 'local' | null;
  /** 归入哪一段色带；null = 不进色带（残差是**检验量**，不是分段）。 */
  segment: LatencySegmentId | null;
}

/**
 * `id` 直接取 IPC 契约里 `PipelineStage.id` 的取值（规格 §3.5，snake_case），**不做
 * 大小写转写**。camelCase 版本要在 P0a 的 readLatency 里维护一张映射表，漏一条就是
 * 那一级静默显示「未知」——而「静默缺项」正是本规格反复点名要消灭的失败形态。
 * i18n key 与 id 本来就解耦（key 名由规格 §2.7 钉死，不跟着 id 走）。
 *
 * ⚠ **这张表必须与 Rust 的 `StageId` 枚举逐条对齐**，两侧一个不多一个不少。
 * `core/audiohub-core/src/latency.rs` 的
 * `the_frontend_stage_table_matches_the_rust_enum_exactly` 会**读这个文件**逐条比
 * 对——所以往枚举里加一级而忘了加到这里（或反过来），`cargo test` 直接变红。
 * 在此之前那条测试比对的是一份**手抄进 Rust 的字面量**，它永远不可能为它命名的
 * 那个漂移变红：抄错的和被抄的是同一只手。
 *
 * 顺序按音频流向排（= 展开明细的显示顺序）。`bridge_ring` / `hal_mic` 与
 * `play_ring` **并行**而非串联，所以紧挨着它排在一起——三者只有一条会被计入总数
 * （daemon 侧取 max），排在一起才不会读成「一秒又一秒」。
 */
export const LATENCY_STAGES: StageSpec[] = [
  { id: 'cap_ring', nameKey: 'latency.stage.capRing.name', descKey: 'latency.stage.capRing.desc', host: 'peer', segment: 'capture' },
  { id: 'cap_dev', nameKey: 'latency.stage.capDev.name', descKey: 'latency.stage.capDev.desc', host: 'peer', segment: 'capture' },
  { id: 'src_fifo', nameKey: 'latency.stage.srcFifo.name', descKey: 'latency.stage.srcFifo.desc', host: 'peer', segment: 'capture' },
  { id: 'hal_spk', nameKey: 'latency.stage.halSpk.name', descKey: 'latency.stage.halSpk.desc', host: 'peer', segment: 'capture' },
  { id: 'send_pace', nameKey: 'latency.stage.sendPace.name', descKey: 'latency.stage.sendPace.desc', host: 'peer', segment: 'capture' },
  { id: 'network', nameKey: 'latency.stage.network.name', descKey: 'latency.stage.network.desc', host: null, segment: 'network' },
  { id: 'jitter_buf', nameKey: 'latency.stage.jitterBuf.name', descKey: 'latency.stage.jitterBuf.desc', host: 'local', segment: 'buffer' },
  { id: 'post_mix', nameKey: 'latency.stage.postMix.name', descKey: 'latency.stage.postMix.desc', host: 'local', segment: 'buffer' },
  { id: 'play_ring', nameKey: 'latency.stage.playRing.name', descKey: 'latency.stage.playRing.desc', host: 'local', segment: 'playback' },
  { id: 'bridge_ring', nameKey: 'latency.stage.bridgeRing.name', descKey: 'latency.stage.bridgeRing.desc', host: 'local', segment: 'playback' },
  { id: 'hal_mic', nameKey: 'latency.stage.halMic.name', descKey: 'latency.stage.halMic.desc', host: 'local', segment: 'playback' },
  { id: 'play_dev', nameKey: 'latency.stage.playDev.name', descKey: 'latency.stage.playDev.desc', host: 'local', segment: 'playback' },
  { id: 'residual', nameKey: 'latency.stage.residual.name', descKey: 'latency.stage.residual.desc', host: null, segment: null },
];

/**
 * 三条**并行**尾级：一帧解码结果会被同时送进真实输出 / 桥接虚拟声卡 / 虚拟麦克风
 * （它们是独立目的地，不是互斥选项）。在时间上它们并联，用户从任一条听到的延迟
 * 是**那一条**的驻留，不是几条之和。
 *
 * 所以归并四段时它们只取最大的一条。同时开了「监听」与「桥接到虚拟声卡」的会话
 * 会有两条 1 秒环，直接相加就报出 2 秒的假延迟——而 daemon 的 `sum_stage_ms` 用的
 * 正是同一条规则（`StageId::is_output_tail`），两边不一致的话四段之和会对不上总数。
 */
export const PARALLEL_TAILS: readonly string[] = ['play_ring', 'bridge_ring', 'hal_mic'];

/**
 * 漂移标记的门限：折算成**每分钟 1 毫秒**。
 *
 * 不是拍一个「样本/秒」的数——那个量纲随设备速率变，48k 上的 1 样本/秒和 44.1k 上
 * 的 1 样本/秒是两个不同的物理效应。折算成「每分钟多少毫秒」之后门限才对得起
 * 「值不值得看一眼」这个问题：低于 1 ms/min 的斜率要一个多小时才攒出几十毫秒，
 * 而遥测本身的采样噪声就在这个量级。
 */
export const DRIFT_MS_PER_MIN = 1;

/** 单级读数。ms 为 undefined = 这一级读不到，**不是 0**。 */
export interface StageReading {
  ms?: number;
  /**
   * 这一级长在哪台主机上。取自它出现在 `stages` 还是 `peer_stages`——那是**权威
   * 来源**，比静态级表可靠：级表里的「对方主机 / 本机」是按 recv 会话写的物理
   * 定义（提供方 = 对方），而 send 会话上两边正好互换。
   */
  host?: 'local' | 'peer';
  /** 满时丢头 / 丢尾。两者的深度读数一模一样，听感完全不同（规格 §0.2）。 */
  dropMode?: 'oldest' | 'newest' | 'none';
  /**
   * 会话累计丢弃样本。**undefined = 本进程观测不到这一级的丢弃，不是没丢过。**
   * 典型是 `hal_spk`：环满时写不进去的是驱动侧的 IOProc，计数在它那里。
   * 与 `0`（观测得到，确实一个都没丢）是两个不同的结论，界面必须分开讲。
   */
  dropped?: number;
  /** 深度是否贴着容量上限（daemon 判定，≥95%）。 */
  saturated?: boolean;
  /** 当前存量 / 容量 / 速率：`saturated` 那个布尔的原始证据。capacity 为 0 = 无界。 */
  samples?: number;
  capacity?: number;
  rate?: number;
  /**
   * 30 s 窗口深度斜率，样本/秒。**undefined = 样本点不足以判趋势（<3 点或跨度
   * <5 s），不是 0**：「测到了，就是不漂」与「还没测出来」对应完全不同的修法。
   */
  driftSps?: number;
  /** 同一斜率折算成每分钟多少毫秒（正 = 在涨）。`driftSps` 缺席或速率未知时缺席。 */
  driftMsPerMin?: number;
  /** 斜率是否大到值得标出来（见 DRIFT_MS_PER_MIN）。读不到斜率时为 undefined。 */
  drifting?: boolean;
}

export type LatencyGrade = 'imperceptible' | 'conversational' | 'noticeable' | 'unusable';
export type LatencyConfidence = 'full' | 'lowerBound' | 'converging' | 'localOnly' | 'unavailable';

export interface LatencyReading {
  /** Σ 各级。任一已声明分项缺失即 undefined。 */
  totalMs?: number;
  segments: Partial<Record<LatencySegmentId, number>>;
  stages: Record<string, StageReading | undefined>;
  confidence: LatencyConfidence;
  /** 本机在这条流里是发送端还是接收端。决定静态级表的「提供方/使用方」落在哪台主机。 */
  side?: 'send' | 'recv';
  /** θ 收敛倒计时（秒），仅 confidence === 'converging' 时有意义。 */
  convergingS?: number;
  /** 输出设备是蓝牙 / HDMI / AirPlay：系统少报延迟，**永远**带「≥」。 */
  deviceUnreliable?: boolean;
  /** 对端分项的年龄（秒）。daemon 说 >3 即视为陈旧。 */
  peerAgeS?: number;
  /** 单向网络（min-RTT/2）。**只是一段**，任何情况下不得当总数。 */
  netMs?: number;
  /** P1：实测采样年龄。与 Σ 各级是两条独立路径，差值即 residual。 */
  e2eMs?: number;
  /** P1：`e2e_ms − sum_ms`。|值| > 20 ms 说明链路上还有没被建模的缓冲级。 */
  residualMs?: number;
}

const GRADE_LABEL: Record<LatencyGrade, MsgKey> = {
  imperceptible: 'metric.latency.grade.imperceptible',
  conversational: 'metric.latency.grade.conversational',
  noticeable: 'metric.latency.grade.noticeable',
  unusable: 'metric.latency.grade.unusable',
};

/**
 * 分级门限（规格 §2.6）。这四条线不是拍脑袋：
 *   40 ms  人耳开始察觉回声 / 梳状滤波的量级
 *   120 ms ITU-T G.114 单向语音「舒适」门限
 *   300 ms G.114 过渡区上界，之上「多数应用不可接受」
 * 它们建立量级直觉，**不打好坏标签**——所以文案是「可用于对话」而不是「良」。
 */
export function latencyGrade(ms: number): LatencyGrade {
  if (ms <= 40) return 'imperceptible';
  if (ms <= 120) return 'conversational';
  if (ms <= 300) return 'noticeable';
  return 'unusable';
}

export function latencyGradeKey(g: LatencyGrade): MsgKey {
  return GRADE_LABEL[g];
}

/** 色带与数字共用的色阶类名。 */
export function latencyTone(g: LatencyGrade): string {
  return g === 'imperceptible' ? 'ok' : g === 'conversational' ? 'accent' : g === 'noticeable' ? 'warn' : 'danger';
}

/**
 * 读数是否只能算下限（要带「≥」）。三种情况：设备项缺失（`lowerBound`）、输出
 * 设备少报延迟（蓝牙/HDMI）、以及**只有本机侧**（`localOnly`——缺的是对端整条
 * 半程，那更是下限）。
 */
export function isLowerBound(r: LatencyReading | undefined): boolean {
  return !!r && (r.confidence === 'lowerBound' || r.confidence === 'localOnly' || !!r.deviceUnreliable);
}

/**
 * 这个读数是否覆盖**整条链路**（对端那一半在场）。
 *
 * 它与 `isLowerBound` 的区别，是这次改版的核心判据：
 *
 * - `lowerBound` 短的是**有界的一小截**——两个声卡固有缓冲，量级 10~20 ms
 *   （规格 §3.4）。在这样的读数上下「可用于对话 / 明显延迟」的结论，最多错半档，
 *   而「≥」已经把这半档说出来了。
 * - `localOnly` 短的是**对端整整一半管线**，量级无上界（对端那台机器上可以有一个
 *   灌满的 1 秒 FIFO）。在这样的读数上下端到端结论，正是「让用户以为总延迟只有
 *   188 ms」的那个错误本身。
 *
 * 所以：**等级词只在本函数为真时才显示**；为假时界面改挂一个「未含对方主机」的
 * 范围标记，数字照给（它是一个诚实的下限），但不作端到端判断。
 */
export function coversWholeChain(r: LatencyReading | undefined): boolean {
  return !!r && r.confidence !== 'localOnly';
}

/**
 * 主数值该用哪条文案（带不带「≥」）。抽出来是为了让对端卡片、统计页数字块、
 * 延迟瀑布三处共用同一条判据——同一个数在两个页面上一处带「≥」一处不带，
 * 等于其中一页宣称了它没有的精度。
 */
export function latencyValueKey(r: LatencyReading | undefined): MsgKey {
  return isLowerBound(r) ? 'metric.latency.valueLower' : 'metric.latency.value';
}

const CONF_LABEL: Record<LatencyConfidence, MsgKey> = {
  full: 'latency.conf.full',
  lowerBound: 'latency.conf.lowerBound',
  converging: 'latency.conf.converging',
  localOnly: 'latency.conf.localOnly',
  unavailable: 'metric.latency.unsupported',
};

export function confidenceKey(c: LatencyConfidence): MsgKey {
  return CONF_LABEL[c];
}

// ---------------------------------------------------------------- 音质

export const QUALITY_PARTS = ['continuity', 'level', 'bandwidth'] as const;
export type QualityPartId = (typeof QUALITY_PARTS)[number];

export const QUALITY_PART_NAME: Record<QualityPartId, MsgKey> = {
  continuity: 'quality.part.continuity.name',
  level: 'quality.part.level.name',
  bandwidth: 'quality.part.bandwidth.name',
};

export const QUALITY_PART_DESC: Record<QualityPartId, MsgKey> = {
  continuity: 'quality.part.continuity.desc',
  level: 'quality.part.level.desc',
  bandwidth: 'quality.part.bandwidth.desc',
};

export type QualityGrade = 'excellent' | 'good' | 'fair' | 'poor';

export interface QualityReading {
  /**
   * 三分量取 min（木桶），不是加权平均——三家损伤感知上不可互相补偿（规格 §4.4）。
   *
   * **undefined = 等级不成立**（daemon 报 `"unknown"`：某个分量还没读数，在场分量
   * 的 min 只是上界）。界面据此不显示等级词、不点亮任何一颗点，**不许退回一个
   * 具体等级**——那正是「一条正在爆音的流在开头 10~20 秒报『良好』」的来路。
   */
  grade?: QualityGrade;
  /** argmin：拖后腿的那一项。`grade` 不成立时它也不成立。 */
  worst?: QualityPartId;
  /**
   * 有效带宽 kHz = 线上采样率 / 2（奈奎斯特上限）。**只进明细，不上一级界面。**
   *
   * 它与 `wireRateKhz` 差 2 倍且都以 kHz 呈现——一级界面上放它，用户会拿它和
   * 自己刚设的「PCM 48 kHz」比，得出「设置没生效」（2026-08-04 实测报告）。
   */
  bandwidthKhz?: number;
  /**
   * 线上采样率 kHz。**与设置里的质量档同量纲**（`pcm48k` ⇒ 48），所以它才是
   * 一级界面该显示的那个数：用户设 48，界面写 48。
   *
   * 由 daemon 一等上报，**绝不由 `bandwidthKhz * 2` 推**（理由见 types.ts）。
   */
  wireRateKhz?: number;
  /**
   * 线上位深：`'s16' | 's24' | 'f32'`。`undefined` = 旧 daemon / 读不到。
   *
   * 与 `wireRateKhz` 成对呈现：位深进阶梯之后，只写采样率的读数是有歧义的
   * （`48 kHz` 说不出它是 16 位还是 24 位）。
   *
   * **一行推导都没有**：daemon 一等上报，前端不从 codec / 码率反推
   * （理由与 `wireRateKhz` 逐字相同，见 types.ts）。
   */
  wireDepth?: string;
  /** 加权隐藏率（%）。 */
  concealPct?: number;
  /** 削顶占比（%）与超出深度（dB）：占比说明多广，深度说明多狠。 */
  clipPct?: number;
  clipExcessDb?: number;
  /** 统计窗口秒数。 */
  windowS?: number;
  /**
   * 本次合成少了至少一块板（目前只可能是削顶那一页没攒满）。
   *
   * 与 `grade === undefined` **不是同义词**：等级已经触底（差）时缺席改不了结论，
   * 于是 `grade` 有值而 `partial` 仍为真。想说「这个结论是在缺一项的情况下得出的」
   * 读这个；只想决定要不要显示等级读 `grade` 就够。
   */
  partial?: boolean;
  /**
   * 这份读数来自**对端**（`SessionStats.peer_quality`），不是本机量的。
   *
   * 界面必须把它标出来。不标不是「省一个标签」，而是把对方主机的测量冒充成本机
   * 的观测：同一条通路上，本机是发送侧、根本没有测点，凭什么给出一个音质结论？
   * 标了之后这一格才说得通——数是真的，量它的人在对面。
   */
  fromPeer?: boolean;
}

const QUALITY_GRADE_LABEL: Record<QualityGrade, MsgKey> = {
  excellent: 'metric.quality.grade.excellent',
  good: 'metric.quality.grade.good',
  fair: 'metric.quality.grade.fair',
  poor: 'metric.quality.grade.poor',
};

const QUALITY_WORST_LABEL: Record<QualityPartId, MsgKey> = {
  continuity: 'metric.quality.worst.continuity',
  level: 'metric.quality.worst.level',
  bandwidth: 'metric.quality.worst.bandwidth',
};

export function qualityGradeKey(g: QualityGrade): MsgKey {
  return QUALITY_GRADE_LABEL[g];
}

export function qualityWorstKey(p: QualityPartId): MsgKey {
  return QUALITY_WORST_LABEL[p];
}

export function qualityTone(g: QualityGrade): string {
  return g === 'excellent' ? 'ok' : g === 'good' ? 'accent' : g === 'fair' ? 'warn' : 'danger';
}

/**
 * **有读数、但等级不成立**（daemon 报 `grade: "unknown"`：某个分量还没攒够窗口）。
 *
 * 这个状态必须在界面上有自己的样子。没有它的时候，它长成「有 kHz 数、没有等级词、
 * 四颗点全空」——而「四颗空心点」与「一颗亮点 = 差」在 7px 尺寸下几乎是同一个形状，
 * 用户只能读成**测出来很差**。把「还不知道」呈现成一个具体且悲观的结论，与用 0
 * 填补缺失分项是同一类错误，只是方向相反。
 */
export function isQualityMeasuring(q: QualityReading | undefined): boolean {
  return !!q && !q.grade;
}

/**
 * 等级位显示哪条文案。**三态**，少一态就是上面那个 bug：
 *   有等级       ⇒ 等级词（优 / 良好 / 一般 / 差）
 *   有读数没等级 ⇒ 「测量中…」
 *   连读数都没有 ⇒ undefined（整段隐藏，一级那格已经是「—」了）
 *
 * 抽成纯函数是为了让这三态**可被回归断言**——它原先是 JSX 里的一个三元表达式，
 * 而「某个状态压根没被渲染」恰恰是类型检查看不见的那类缺陷。
 */
export function qualityGradeTextKey(q: QualityReading | undefined): MsgKey | undefined {
  if (q && q.grade) return qualityGradeKey(q.grade);
  if (q) return 'metric.quality.measuring';
  return undefined;
}

/**
 * 线上位深 → 文案键。**全应用只此一份。**
 *
 * 位深只有三个合法取值，冒出第四个说明两端版本对不上 ⇒ 返回 `undefined`，
 * 调用方就只写采样率。**不把原始串画出来**：画一个 `s48` 出来只会让人以为
 * 那是一档。**更不许兜底成 16 bit**——位深进阶梯之前线上恒为 16 位，那个兜底
 * 今天碰巧总是对的，正因为碰巧总是对，它会一直躺着直到某天不对了也没人发现。
 *
 * # 为什么必须是一份而不是三份
 *
 * 这张表原本在 `PeerMetrics.tsx`（返回字符串）与 `PeerTransport.tsx`
 * （返回 MsgKey）各有一份、形状还不同，统计页正要写第三份。三处一漂，
 * 界面上同一条流的位深就会有三种写法，而没有任何一处会报错——与 daemon
 * 坚持自己当档表唯一真值源是同一条理由。
 */
export function qualityDepthKey(d: string | null | undefined): MsgKey | undefined {
  switch (d) {
    case 's16': return 'metric.quality.depth.s16';
    case 's24': return 'metric.quality.depth.s24';
    case 'f32': return 'metric.quality.depth.f32';
    default: return undefined;
  }
}

/** 一级的「●●●○」：四点，点数由等级定。读不到时 0 点全空。 */
export const QUALITY_DOTS = 4;

export function qualityDots(g: QualityGrade | undefined): number {
  if (!g) return 0;
  return g === 'excellent' ? 4 : g === 'good' ? 3 : g === 'fair' ? 2 : 1;
}

// ---------------------------------------------------------------- 读取入口

/**
 * 最近 5 个采样的中位数（规格 §2.6）。瞬时队列深度抖 ±10 ms，直接显示会让数字每秒
 * 乱跳，用户会以为界面在骗人；中位数比均值更合适，因为一次 GC 停顿造成的单点尖峰
 * 不该把读数拉走。
 *
 * **P0a 的用法**：历史序列由 `state/store.ts` 的 `MetricHistory.latency` 提供（那里
 * 已经是 60 点序列，`pushMaybe` 保证缺读数时不入列），取其末 5 点喂给本函数即可，
 * readLatency 本身不必持有状态。样本不足 5 个时按现有点数取中位数，**不补零**。
 */
export function medianOf5(samples: number[]): number | undefined {
  const tail = samples.filter((v) => typeof v === 'number' && isFinite(v)).slice(-5);
  if (!tail.length) return undefined;
  const sorted = [...tail].sort((a, b) => a - b);
  const mid = sorted.length >> 1;
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

const CONFIDENCES: readonly string[] = ['full', 'lowerBound', 'converging', 'localOnly', 'unavailable'];
const DROP_MODES: readonly string[] = ['oldest', 'newest', 'none'];

/** 把一条 IPC 级读数翻成 UI 读数。`host` 由调用方给——它取决于来自哪个数组。 */
function toStage(s: PipelineStage, host: 'local' | 'peer'): StageReading {
  const rate = num(s.rate);
  const driftSps = num(s.drift_sps);
  // 折算成 ms/min 需要速率；速率为 0（daemon 判该级读数无效）时**不折算**，
  // 而不是拿 48000 顶上——那正好是 44.1k 设备上 −8.8% 的那种「小到没人发现」的错。
  const driftMsPerMin = driftSps !== undefined && rate ? (driftSps * 60000) / rate : undefined;
  return {
    ms: num(s.ms),
    host,
    dropMode: DROP_MODES.includes(String(s.drop_mode))
      ? (s.drop_mode as StageReading['dropMode'])
      : undefined,
    dropped: num(s.dropped),
    saturated: s.saturated === true,
    samples: num(s.samples),
    capacity: num(s.capacity),
    rate,
    driftSps,
    driftMsPerMin,
    drifting: driftMsPerMin === undefined ? undefined : Math.abs(driftMsPerMin) >= DRIFT_MS_PER_MIN,
  };
}

/** 设备固有延迟 → 一个只有 ms 的级读数。`unavailable` / 速率为 0 ⇒ 没有读数。 */
function devStage(d: DevLatency | null | undefined, host: 'local' | 'peer'): StageReading | undefined {
  if (!d) return undefined;
  const rate = num(d.rate);
  const frames = num(d.frames);
  const usable = d.source !== 'unavailable' && !!rate && frames !== undefined;
  // **不给 dropMode**：声卡固有延迟是一个常数属性，不是队列，没有可丢的东西。
  // 给它一个 'none' 会让明细行冒出「不丢弃 / 丢弃数不可见」这类纯噪声标签
  // （见 lib/stagefacts.ts 的 hasQueue）。
  return {
    // 读不到就是读不到：**绝不填 0**。填 0 会让蓝牙耳机（真实 +150~250 ms）
    // 看起来和模拟输出一样好，而这一级恰好是「≥」前缀的唯一来源。
    ms: usable ? (frames * 1000) / rate : undefined,
    host,
    rate,
  };
}

/**
 * 逐级 → 四段。**串联各级求和，并行尾级取 max**（见 PARALLEL_TAILS）。
 *
 * 只有真读到的级才进和；一段里一个级都没读到 ⇒ 该段缺席（界面「—」），
 * 不是 0。累加器里的 `?? 0` 是求和的初值，不是给缺失分项填的值——它只在
 * 至少有一个真读数时才会被写进结果。
 */
function segmentTotals(stages: Record<string, StageReading | undefined>): Partial<Record<LatencySegmentId, number>> {
  const out: Partial<Record<LatencySegmentId, number>> = {};
  const tails: Partial<Record<LatencySegmentId, number>> = {};
  for (const spec of LATENCY_STAGES) {
    const seg = spec.segment;
    if (!seg) continue; // residual 是**检验量**，不是分段
    const ms = stages[spec.id]?.ms;
    if (typeof ms !== 'number') continue;
    if (PARALLEL_TAILS.includes(spec.id)) tails[seg] = Math.max(tails[seg] ?? 0, ms);
    else out[seg] = (out[seg] ?? 0) + ms;
  }
  for (const seg of LATENCY_SEGMENTS) {
    const tail = tails[seg];
    if (tail !== undefined) out[seg] = (out[seg] ?? 0) + tail;
  }
  return out;
}

/**
 * 三条并行尾级里**计入总数**的那一条（ms 最大的），没有尾级读数时为 undefined。
 *
 * 逐级明细要把三条都列出来（用户得看见桥接环里堆了多少），但凡是**画总数的构成**
 * 的地方——延迟瀑布、分段色带——只能算这一条，否则条子的长度之和大于它自己标的
 * 总数，同一页自相矛盾。
 */
export function countedTail(stages: Record<string, StageReading | undefined>): string | undefined {
  let best: string | undefined;
  let bestMs = -1;
  for (const id of PARALLEL_TAILS) {
    const ms = stages[id]?.ms;
    if (typeof ms === 'number' && ms > bestMs) {
      bestMs = ms;
      best = id;
    }
  }
  return best;
}

// ------------------------------------------------- 卡片指标区的分栏规则
//
// 这一段是 2026-08-04 事故的正面防线，所以它在**这里**而不是在组件里。
//
// 事故：`PeerMetrics` 拿到的是 `sess={micS || spkS}` —— 两条真实存在的通路里
// 用 `||` 选了一条，界面上不留任何痕迹。回归覆盖当时的分布是：
// `lib/metrics.ts` 的纯函数（`readLatency` 等）测得很扎实，**组件接线层零覆盖**，
// 而 bug 正好长在接线层。「有测试的那层没坏，坏的那层没测试」——把接线规则搬进
// 纯函数，是让它第一次变得可断言的唯一办法。

/** 本机视角的方向。testid 里的 `<dir>` 段沿用 `peer-device-out-<fp>` 的约定。 */
export type Dir = 'out' | 'in';

/**
 * 一张卡片的两栏各装哪些会话。
 *
 * 判据是 **`dir`（本机视角）**，不是 `kind`（开流方视角）。四种 `(kind, dir)`
 * 组合全部有归属：
 *
 * | kind | dir | 含义 | 落进 |
 * |---|---|---|---|
 * | `mic` | `recv` | 我取对方麦克风（使用端·收） | `recv` |
 * | `spk` | `send` | 我送对方扬声器（使用端·发） | `send` |
 * | `mic` | `send` | 对方取本机麦克风（共享端·发） | `send` |
 * | `spk` | `recv` | 对方送本机扬声器（共享端·收） | `recv` |
 *
 * 后两种此前**一个都匹配不上**（旧代码只找 `mic/recv` 与 `spk/send`），
 * 于是共享模式下指标区恒显示「未建立通路」，而隔壁隐私横幅同时亮着。
 *
 * 返回**数组**而不是单条：同一对端同一方向可以有多条会话
 *（`MAX_STREAMS_PER_CONN = 16`，daemon 去重只按 `stream_id`）。返回单条就是把
 * 「两个方向里选一个」的 bug 降级成「同方向 N 条里选第一条」。
 */
export function splitByDirection(
  sessions: readonly SessionInfo[],
  fp: string,
): { send: SessionInfo[]; recv: SessionInfo[] } {
  const send: SessionInfo[] = [];
  const recv: SessionInfo[] = [];
  for (const s of sessions) {
    if (s.peer_fingerprint !== fp) continue;
    if (s.dir === 'send') send.push(s);
    else if (s.dir === 'recv') recv.push(s);
    // 认不出的 dir（旧 / 新 daemon 的其它取值）**两栏都不进**。猜一个方向
    // 比不显示更糟：那正是本次事故的形态——一个没有来源的数字挂在某一栏里。
  }
  return { send, recv };
}

/**
 * 同一方向的 N 条会话里，**显示最慢的那条**。
 *
 * 三条候选规则里只有这一条站得住：
 *
 * - 「取第一条」= `find()` —— 那就是本次事故的同构形态，只是轴从 `dir` 换成了
 *   会话顺序。而 daemon 按 `stream id` 升序返回（`lib.rs` 的 `sort_by_key`），
 *   「第一条」= id 最小 = **最老**的那条，断线重连时它可能已经死了。
 * - 「取最快」—— 把一个正在拖后腿的通路藏起来，与「不得用 0 填补缺失分项」
 *   同一类错误：让坏消息长得像没消息。
 * - 「取最慢」—— 用户在这张卡上问的是「这条方向用起来好不好」，而多路并行时
 *   体感由最差的一路决定。
 *
 * 无论取哪条，界面都会把「一共几路」说出来（`peer-dir-count-<dir>-<fp>`）：
 * 一个不标来源的数字背后站着 N 个候选，正是这次要消灭的形态。
 */
export function pickWorst(list: readonly SessionInfo[]): SessionInfo | null {
  if (list.length === 0) return null;
  let best = list[0];
  let bestMs = -1;
  for (const s of list) {
    const ms = readLatency(s)?.totalMs;
    // 读不到延迟的会话不参与比较，但**也不会被排除**：全都读不到时仍然要有
    // 一条被渲染，否则「N 条会话在跑却显示未开通」比选错一条更糟。
    if (typeof ms === 'number' && ms > bestMs) {
      bestMs = ms;
      best = s;
    }
  }
  return best;
}

/**
 * 一段色带里**此刻占大头**的那一级，没有任何读数时为 undefined。
 *
 * 存在的理由是段名会撒谎。段名是静态的、按物理流向定的，而一段里并列着好几级：
 * `playback` 段同时装着 `play_ring`（真实播放环）、`bridge_ring`（第三方虚拟
 * 声卡）和 `hal_mic`（**虚拟麦克风环**）三条并行尾级。2026-08-04 的现场里
 * 接收方向 136 ms 全在 `hal_mic` 上，界面写着「播放 136」——用户据此去查扬声器，
 * 一查一个准地查错方向，而扬声器那条通路实测只有 24 ms。
 *
 * 并行尾级取 max（与 `segmentTotals` 同一条规则），串联级取最大的那一个：
 * 两种情形下"这一段的大头"都是同一句话。
 */
export function segmentDominantStage(
  r: LatencyReading,
  seg: LatencySegmentId,
): StageSpec | undefined {
  let best: StageSpec | undefined;
  let bestMs = -1;
  for (const spec of LATENCY_STAGES) {
    if (spec.segment !== seg) continue;
    const ms = r.stages[spec.id]?.ms;
    if (typeof ms === 'number' && ms > bestMs) {
      bestMs = ms;
      best = spec;
    }
  }
  return best;
}

/**
 * 这一级显示成「对方主机」还是「本机」。
 *
 * 有读数就用读数带的 `host`（它来自 `stages` / `peer_stages` 的数组归属，权威）。
 * 没读数时退回静态级表——但级表是按 **recv** 会话写的物理定义（提供方 = 对方），
 * 所以 send 会话要整体翻面，否则一条 send 会话会把本机的虚拟扬声器环标成
 * 「对方主机」，排障时指错机器。
 */
export function stageHost(
  spec: StageSpec,
  r: StageReading | undefined,
  side: 'send' | 'recv' | undefined,
): 'local' | 'peer' | null {
  if (r && r.host) return r.host;
  if (!spec.host) return null;
  if (side === 'send') return spec.host === 'peer' ? 'local' : 'peer';
  return spec.host;
}

/**
 * 从一条会话读延迟（daemon 的 `SessionStats.pipeline`，规格 §3）。
 *
 * ## 总数怎么取（这张表是本函数唯一的要害）
 *
 * | daemon 给的 | totalMs | confidence | 界面 |
 * |---|---|---|---|
 * | `sum_ms` 有值 | `sum_ms` | full / lowerBound | 数字 + 等级词，lowerBound 带「≥」|
 * | `sum_ms` 为 null、`confidence = localOnly` | `local_ms` | localOnly | 数字带「≥」，**撤掉等级词**，标「未含对方主机」|
 * | 两者都没有 | undefined | 原样 | 「—」 |
 *
 * **`local_ms` 顶替 `sum_ms` 是本函数最危险的一行**，所以它同时把 confidence 原样
 * 传出去：`coversWholeChain()` 为假 ⇒ 上层撤等级、加范围标记。少了那一半，这一行
 * 就变成「让用户以为端到端只有本机侧那点数」——现场实读 `local_ms≈474 ms` 而对端
 * 播放侧一无所知，正是这个场景。
 *
 * 另外三处「不是分项、但要落到某一级上」的映射：
 *   - `net_ms`（min-RTT/2）→ `network` 级。它在 IPC 里是独立字段而不是 `stages`
 *     的一员，不映射的话「网络」那一段永远是「—」。**只作一段，绝不作总数。**
 *   - `dev` / `peer_dev` → `play_dev` / `cap_dev`，按 side 决定谁是谁。
 *   - `residual_ms` → `residual` 级（P1 的完整性检验量）。
 *
 * 返回的是**瞬时值**。头条数字的 5 点中位数平滑在组件侧做（数据源是
 * `store.history[id].latency`），明细保持瞬时——排障看的就是那一下的抖动。
 */
export function readLatency(sess: SessionInfo | null | undefined): LatencyReading | undefined {
  const p = sess && sess.stats && sess.stats.pipeline;
  if (!p) return undefined;

  // 认不出的取值（旧/新 daemon 的其它档）一律当「无法测量」，不猜。
  const confidence = (CONFIDENCES.includes(String(p.confidence))
    ? p.confidence
    : 'unavailable') as LatencyConfidence;
  const side = p.side === 'send' || p.side === 'recv' ? p.side : undefined;

  const stages: Record<string, StageReading | undefined> = {};
  for (const s of Array.isArray(p.stages) ? p.stages : []) {
    if (s && s.id) stages[s.id] = toStage(s, 'local');
  }
  for (const s of Array.isArray(p.peer_stages) ? p.peer_stages : []) {
    if (s && s.id) stages[s.id] = toStage(s, 'peer');
  }

  // 网络：两端共有，不归任何一台主机（host 留空）。也不是队列，故无 dropMode。
  const netMs = num(p.net_ms);
  if (netMs !== undefined) stages.network = { ms: netMs };

  // 设备级：send 会话的本机是**提供方**，`dev` 是采集声卡；recv 反过来。
  const localDevId = side === 'send' ? 'cap_dev' : 'play_dev';
  const peerDevId = side === 'send' ? 'play_dev' : 'cap_dev';
  const localDev = devStage(p.dev, 'local');
  const peerDev = devStage(p.peer_dev, 'peer');
  if (localDev) stages[localDevId] = localDev;
  if (peerDev) stages[peerDevId] = peerDev;

  const residualMs = num(p.residual_ms);
  if (residualMs !== undefined) stages.residual = { ms: residualMs };

  const sumMs = num(p.sum_ms);
  const localMs = num(p.local_ms);
  const totalMs = sumMs !== undefined ? sumMs : (confidence === 'localOnly' ? localMs : undefined);

  // 输出设备少报延迟（蓝牙 / HDMI / AirPlay）⇒ **永远**带「≥」。哪一个 dev 是
  // 输出设备取决于 side，和上面的映射同一套判据。
  const outDev = side === 'send' ? p.peer_dev : p.dev;

  return {
    totalMs,
    segments: segmentTotals(stages),
    stages,
    confidence,
    side,
    deviceUnreliable: !!outDev && outDev.source === 'unreliable',
    peerAgeS: num(p.peer_age_s),
    netMs,
    e2eMs: num(p.e2e_ms),
    residualMs,
  };
}

// ------------------------------------------------- 连接级网络延迟（无会话时）

/**
 * 一条**连接**上的网络单程读数。它与 `LatencyReading` 是两个量，故意不共用类型：
 * 前者是端到端总延迟（音频真正走完全程要多久），这里只是其中**一段**。
 */
export interface PeerNetReading {
  /** min-RTT / 2，毫秒。**undefined = 还在攒样本**（界面「测量中…」），不是 0。 */
  ms?: number;
  /** 最近一次往返，交叉校验用。缺席即不显示。 */
  rttMs?: number;
}

/**
 * 连上了、但还没有任何会话时，这条连接唯一能给出的延迟读数（`PeerState.net_ms`）。
 *
 * 三态，两条红线各管一态：
 *
 * - **离线 ⇒ `undefined`**：整块不渲染。daemon 在离线时把两个字段都清成 null，
 *   这里再兜一层——记忆里的往返时间是关于过去的陈述，挂在一台离线主机的卡片上
 *   会被读成「它现在还这么快」。
 * - **在线但 `net_ms` 为 null ⇒ `{ ms: undefined }`**：min-RTT 窗口还没攒够。
 *   界面显示「测量中…」，**绝不落成 0 ms**。
 * - 在线且有值 ⇒ `{ ms }`。
 *
 * ⚠ 调用方必须把「这只是网络那一段」标在**数字旁边**，不是藏进 title。缓冲与声卡
 * 占了延迟的绝大部分（实测网络 0.58 ms vs 感知约 1000 ms），而它们要等真的有音频
 * 在流动时才量得到。把这个数放进端到端总数的槽位就是撒谎。
 */
export function readPeerNet(peer: PeerState | null | undefined): PeerNetReading | undefined {
  if (!peer || !peer.online) return undefined;
  // 两个键**整个缺席** = 旧 daemon 根本不上报这一项 ⇒ 什么都不渲染，退回改版前的
  // 「延迟 —」。这与 `null` 是两回事：`null` 是新 daemon 在说「我在测，还没攒够」，
  // 而缺席时显示「测量中…」就是一句永远不会兑现的承诺（同 `transport_live` 那条）。
  if (peer.net_ms === undefined && peer.rtt_ms === undefined) return undefined;
  return { ms: num(peer.net_ms), rttMs: num(peer.rtt_ms) };
}

/** 有限数才算读数。`null` / `undefined` / `NaN` 一律是「没读到」，不是 0。 */
function num(v: unknown): number | undefined {
  return typeof v === 'number' && isFinite(v) ? v : undefined;
}

/** 比例 → 百分数。只在真有读数时换算，`undefined` 原样传下去。 */
function pctOf(ratio: unknown): number | undefined {
  const v = num(ratio);
  return v === undefined ? undefined : v * 100;
}

const QUALITY_GRADES: readonly string[] = ['excellent', 'good', 'fair', 'poor'];

/**
 * 从一条会话读音质（daemon 的 `SessionStats.quality`，规格 §4）。
 *
 * 三条纪律，每一条都对应一个已经犯过的错：
 *
 * 1. **缺失分量不填补。** 削顶那一页没攒满时 daemon 发的是 `clip_ratio: null`，
 *    这里就让 `clipPct` 保持 `undefined`，界面显示「—」。`?? 0` 会把「还没测」
 *    讲成「测了，一点没削」。
 * 2. **`grade: "unknown"` 不许回退成一个具体等级。** 分量缺席时在场分量的 min
 *    只是**上界**（`Grade::Excellent` 是最大值，`min(q1, Excellent, q3)` 与
 *    `min(q1, q3)` 逐值相同），真实等级落在 `[差, 上界]` 这个区间里。区间不是
 *    等级 ⇒ `grade` 留空，`qualityDots()` 因此给 0 颗点、等级词整段隐藏。
 *    这一条是「流开头 10~20 秒里一条正在爆音的流报『良好』」的正面防线。
 * 3. **不拿 `stats.loss_pct` 顶替。** 丢包率是**网口上的量**，音质是**扬声器上的
 *    量**，中间差一整条流水线（规格 §4.1）。丢包 2% 在 PLC 修得住时几乎不可闻；
 *    丢包 0% 时两路重复流相加照样把声音削烂。
 *
 * ## 第四条：本机没有测点时，用对端的读数，并**标明出处**
 *
 * 音质三分量全是接收侧概念，于是一条纯发送的流 `quality` **结构上恒为 null**——
 * 不是链路不好，是本机压根量不到。此前这里只读 `quality`，「送对方扬声器」那条
 * 通路的音质格因此**永远**是「—」，而它其实好得很。daemon 现在把对端在它那侧测到
 * 的同一条流的音质经控制通道回传（`peer_quality`，与 `quality` 至多一个非空），
 * 这里取 `quality ?? peer_quality` 并置 `fromPeer`，由界面把「谁量的」说出来。
 *
 * 兜底顺序**不能反**：本机有测点时本机的读数才是第一手的。
 *
 * 两者都没有（旧 daemon、Q1 窗口还没攒够、对端还没回传）⇒ `undefined` ⇒ 「—」。
 */
export function readQuality(sess: SessionInfo | null | undefined): QualityReading | undefined {
  const st = sess && sess.stats;
  // 本机的读数优先；`|| undefined` 是把 daemon 的 JSON `null` 收敛成一个值，
  // 好让下面那行的「有没有本机读数」只需判一次。
  const own = (st && st.quality) || undefined;
  const q = own || (st && st.peer_quality) || undefined;
  if (!q) return undefined;

  // 认不出的等级串（含 "unknown"，以及将来某个更新的 daemon 发来的新档）一律
  // 当作「没有等级」。宁可少说一句，也不把一个猜的等级写在一级界面上。
  const grade = QUALITY_GRADES.includes(String(q.grade))
    ? (q.grade as QualityGrade)
    : undefined;
  // `worst` 是「限制住等级的那一项」——等级都不成立时它无从谈起。daemon 此时发
  // 的本来就是 "none"，这里再兜一层：这个字段来自另一个进程，可能是另一个版本。
  const worst =
    grade && (QUALITY_PARTS as readonly string[]).includes(String(q.worst))
      ? (q.worst as QualityPartId)
      : undefined;

  const hz = num(q.bandwidth_hz);
  // 线上采样率是**独立读取**的字段。这里一行 `* 2` 都不许出现：今天
  // `bandwidth_hz ≡ wire_rate_hz / 2` 成立，所以推导现在算得对——正因为算得对，
  // 它会一直躺在这里，直到 Q3 换成实测频谱（规格 §4.2 允许）的那天开始撒谎，
  // 而那一天不会有任何一处报错。旧 daemon 不发这个字段 ⇒ undefined ⇒ 「—」。
  const rateHz = num(q.wire_rate_hz);
  // 位深同样是**独立读取**的字段。空串 = 「对面没说」，不是 s16。
  const depth = typeof q.wire_depth === 'string' && q.wire_depth ? q.wire_depth : undefined;
  return {
    grade,
    worst,
    // Hz → kHz。0 Hz 不是带宽读数（rung 解析不出来才会是 0），照样算没读到。
    bandwidthKhz: hz !== undefined && hz > 0 ? hz / 1000 : undefined,
    wireRateKhz: rateHz !== undefined && rateHz > 0 ? rateHz / 1000 : undefined,
    wireDepth: depth,
    concealPct: pctOf(q.conceal_ratio),
    clipPct: pctOf(q.clip_ratio),
    clipExcessDb: num(q.clip_excess_db),
    windowS: num(q.window_s),
    partial: q.partial === true,
    fromPeer: !own,
  };
}
