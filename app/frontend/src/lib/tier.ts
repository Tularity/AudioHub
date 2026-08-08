// 连通性档位（Tier 0/1/2）的**现状**判定与文案映射。plan §16.4 的落点。
//
// # tier 不是诊断字段，是归因工具
//
// plan §16.4 逐字：同一个「延迟 210 ms」，旁边什么都不写 ⇒ 用户唯一可得的结论是
// 「这软件不行」；旁边写着「经 TCP 中转」⇒ 结论变成「这条网络只能这样」。后者才
// 是真话。所以降级档位是**一级信息**、必须与延迟数字相邻，而不是二级页面里
// 一个可查的诊断项。
//
// # 用户的选择 vs 链路的现状：两个量，永远不许互相冒充
//
// | 读哪里 | 是什么 | 谁写的 |
// |---|---|---|
// | `PeerState.transport.tier` | **用户的选择**：`auto` / `tier0` / `tier1` / `tier2` | 用户，经 `peers.set_tier` |
// | 本文件的 `effectiveTier()` | **链路的现状**：字节此刻在哪条通路上 | daemon 的 `MediaPath` |
//
// 选了「自动」的对端此刻完全可能正跑在 Tier 1 上，而 `transport.tier` 照旧是
// `"auto"`。daemon 侧那份契约（`PeerTransportView::tier`）自己写着这一点，并且
// 写着「两者不得互相冒充」。`PeerTransport.tsx` 里那组按钮显示的是前者，本文件
// 判定的是后者，两处在界面上也分别标注。
//
// # 为什么现状是**推导**出来的，以及它推导自什么
//
// daemon 至今没有一个「现状」字段。（`PeerState.auto_tier` 存在，但它是自动
// 降级**记在案的判定**，不是现状：钉死 tier 0 的对端判定可以是 `tier1` 而现状
// 仍是 Tier 0。拿它当现状会在那一格上说反。）
// 但 daemon 有一份**按连接枚举 `MediaPath` 得到的**实时表：
// `daemon.status.latency_guard.{tcp_media, mux}`。一条链路在那里出现，等价于
// 那台对端的 `MediaPath` 不是 `Udp` —— 这不是启发式，是同一个枚举的另一种投影。
//
// 判据（三行，缺一不可）：
//   1. fp 在 `mux[]` 里             ⇒ Tier 2（`MediaPath::Framed`）
//   2. fp 在 `tcp_media[]` 里       ⇒ Tier 1（Tier 2 也在这张表里，故 1 必须先判）
//   3. 都不在，且对端**在线**       ⇒ Tier 0（有连接、而它的 MediaPath 是 Udp）
//
// 其余一律 `null` = **未判定**，包括：`latency_guard` 缺席（旧服务）、
// `tcp_media` 不是数组、对端离线/等待入站（根本没有连接，无从判起）。
//
// ⚠ `mux` 键缺席时**不把 `tcp_media` 里的行判成「不知道」**：`mux_status()` 与
// `MediaPath::Framed` 是同一个提交落地的，所以一个不报 `mux` 的 daemon 根本
// 跑不出 Tier 2 —— 那一行只可能是 Tier 1。这是版本推论，不是猜测。
//
// # 未来
//
// 若将来 daemon 报出一个真正的**按对端**的「现状」字段（按 `MediaPath` 直接
// 投影，而不是 `auto_tier` 那种判定记录），`effectiveTier()` 应当整体换成读那
// 一个字段，**判定不再由 UI 做**。届时本文件的三行判据全部删掉，调用点一行不动
// —— 这正是它被收进单个函数的理由。
//
// ⚠ **`SessionStats.transport` 不是那个字段，不要拿它来换。** 它确实是按
// `MediaPath` 直接投影的现状（见 [`sessionTier`]），但它**按会话**：一台连着而
// 此刻没有任何会话的对端仍然要答得出「现在怎么连的」，而那时一条会话都没有，
// 换过去只会让对端卡片在空闲时倒退回「未判定」。两者并存，各答各的问题。
//
// 而 `auto_tier` / `auto_tier_reason` / `auto_tier_since` 是**另一件事**，
// 归 plan §16.4 的二级页面：现状回答「为什么慢」，判定回答「凭什么这么判的」。

import type { MsgKey } from '../i18n';
import type {
  DaemonInfo, LatencyGuardStatus, MuxLinkStatus, PeerState, TcpMediaLinkStatus,
} from '../ipc/types';

/** 链路**现状**的三个取值。用户的选择另有 `auto`，那是另一个量，不在这里。 */
export type EffectiveTier = 'tier0' | 'tier1' | 'tier2';

function guard(daemon: DaemonInfo | null | undefined): LatencyGuardStatus | undefined {
  const g = daemon?.latency_guard;
  return g && typeof g === 'object' ? g : undefined;
}

function findByFp<T extends { fingerprint?: string }>(
  list: T[] | undefined,
  fp: string,
): T | undefined {
  if (!Array.isArray(list) || !fp) return undefined;
  return list.find((x) => x && x.fingerprint === fp);
}

/** 这台对端的降级媒体链路（Tier 1 或 Tier 2 的媒体半边）。`undefined` = 没有。 */
export function tcpMediaLink(
  daemon: DaemonInfo | null | undefined,
  fp: string,
): TcpMediaLinkStatus | undefined {
  return findByFp(guard(daemon)?.tcp_media, fp);
}

/** 这台对端的 Tier 2 复用连接。`undefined` = 不是 Tier 2（或这一版不报 mux）。 */
export function muxLink(
  daemon: DaemonInfo | null | undefined,
  fp: string,
): MuxLinkStatus | undefined {
  return findByFp(guard(daemon)?.mux, fp);
}

/**
 * 这台对端的媒体**此刻实际**走在哪一档。
 *
 * `null` = **未判定**，调用方必须把它渲染成灰色的「—」，
 * **绝不能当成 Tier 0**（plan §16.4 第 5 条：「已判定为直连」与「不知道」
 * 是两件事，不得渲染成同一个样子）。
 */
export function effectiveTier(
  daemon: DaemonInfo | null | undefined,
  peer: PeerState | null | undefined,
): EffectiveTier | null {
  const fp = peer?.fingerprint;
  if (!fp) return null;
  const g = guard(daemon);
  // 这一版服务根本不报这张表 ⇒ 不知道。**不是**「没有降级链路，所以直连」：
  // 那就是拿一个缺席的字段去证明一个结论。
  if (!g || !Array.isArray(g.tcp_media)) return null;
  if (muxLink(daemon, fp)) return 'tier2';
  if (findByFp(g.tcp_media, fp)) return 'tier1';
  // 没有连接就没有 `MediaPath`，也就没有任何东西被判定过。离线的对端上写
  // 「直连」是在陈述一件此刻不成立的事。
  return peer?.online ? 'tier0' : null;
}

/**
 * 「未判定」的两个成因。只在 `effectiveTier()` 返回 `null` 时有意义。
 *
 * 两者需要**相反的下一步**：`unsupported` 是「这一版服务给不出这个信息」（升级
 * 服务），`offline` 是「此刻根本没有通路可判」（连上就有了）。合成一句「未知」
 * 会让前者看起来像一个暂时状态，用户会一直等一个永远不会到来的值。
 */
export type TierUnknownWhy = 'unsupported' | 'offline';

export function tierUnknownWhy(daemon: DaemonInfo | null | undefined): TierUnknownWhy {
  // 不看 peer：`effectiveTier()` 已经证明结果是 `null`，而在服务**报得出**这张表
  // 的前提下，剩下的唯一成因就是这台对端此刻没有连接。再查一遍 `peer.online`
  // 只会让两处判据可能分岔。
  const g = guard(daemon);
  return !g || !Array.isArray(g.tcp_media) ? 'unsupported' : 'offline';
}

/** Tier 1/2 才是降级。Tier 0 与「未判定」都不是——两者的理由完全不同。
 *
 *  写成类型谓词，好让调用点 `isDegradedTier(t) ? <Banner tier={t}/> : null`
 *  不需要一个 `as`：那个 `as` 会在有人把返回类型改宽时安静地继续编译。 */
export function isDegradedTier(tier: EffectiveTier | null): tier is 'tier1' | 'tier2' {
  return tier === 'tier1' || tier === 'tier2';
}

/**
 * 一级界面上那句话：**说传输形态，不说内部代号**（§16.4 第 2 条）。
 * 「`tier1`」对用户不解释任何事，而解释正是这条要求的全部目的。
 */
export const TIER_LABEL: Record<EffectiveTier, MsgKey> = {
  tier0: 'tier.now.tier0',
  tier1: 'tier.now.tier1',
  tier2: 'tier.now.tier2',
};

/**
 * 贴在标签后面的**后果**一句。
 *
 * ⚠ 写的是「更容易卡顿」，**不是「延迟更高」**。TCP 的握手和 UDP 差不多快，
 * 初始延迟不一定高；真正变差的是抖动下的表现——一次 RTO 就是 200–300 ms，
 * 于是更容易出现可闻停顿。跨机实测（150 s 增量）：Tier 1 上 `jb_underruns` +4、
 * `jb_dropped` +13，Tier 0 上两者都是 +0。把它写成「延迟更高」会让用户去盯那个
 * 毫秒数，而毫秒数可能一点没变——然后他会得出「这个提示是假的」。
 */
export const TIER_WHY: Record<EffectiveTier, MsgKey> = {
  tier0: 'tier.now.tier0Why',
  tier1: 'tier.now.tier1Why',
  tier2: 'tier.now.tier2Why',
};

// -------------------------------------------------------- 按会话的现状（M8 §9）

/**
 * **这一条会话**的字节走在哪一档，取自 daemon 的 `SessionStats.transport`。
 *
 * `null` = **未判定**（这一版服务不上报这个字段）。调用方必须渲染成灰色的「—」，
 * **绝不能当成 Tier 0**——与 [`effectiveTier`] 同一条红线（plan §16.4 第 5 条）。
 *
 * # 为什么这是一个函数而不是 `info.stats?.transport as EffectiveTier`
 *
 * 那个 `as` 会把 daemon 某天新增的第四档（或一个打错的串）直接当成合法档位交给
 * `TIER_LABEL`，查表落空、界面上出现一个空白徽标而没有任何地方会报错。这里做
 * 一次白名单收敛：认不出来的串一律回到「未判定」，与本项目在 `tierPickLabel`
 * 上的处置同一条规矩。
 *
 * # 它读的是链路，不是设置
 *
 * daemon 侧这个字段来自该流绑定的 `MediaPath`（`SessionEntry::media_tier`），
 * 不是 `PeerState.transport.tier`。两者在日常运行中就分岔：钉在 tier 1 而对端
 * 钉在 tier 0 的机器，设置读 `tier1`、这里读 `tier0`。**不许互相冒充**。
 */
export function sessionTier(
  info: { stats?: { transport?: string | null } | null } | null | undefined,
): EffectiveTier | null {
  const t = info?.stats?.transport;
  return t === 'tier0' || t === 'tier1' || t === 'tier2' ? t : null;
}

