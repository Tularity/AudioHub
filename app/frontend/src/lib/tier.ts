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
  DaemonInfo, LatencyGuardStatus, MuxLinkStatus, PeerState, SessionInfo, TcpMediaLinkStatus,
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
 *
 * # 为什么参数写 `SessionInfo` 而不是 `{ stats?: { transport?: string | null } }`
 *
 * 后者是本函数原先的写法，它有两个问题。一是**与同族函数不一致**：本文件的
 * `effectiveTier()` 收的是 `DaemonInfo` / `PeerState` 这样的具名镜像类型，只有
 * 这里手写了一个结构类型，而两者是同一件事（「现状」）的两个投影。二是那个手写
 * 结构与 `SessionStats` **没有任何类型上的联系**：它只是碰巧长得一样。真正的
 * 调用点（`Stats.tsx`）传的从来都是 `SessionInfo`，写成具名类型之后，镜像那边
 * 改了字段名这里会当场编译失败，而不是继续编译、把每条会话都读成「未判定」。
 */
export function sessionTier(
  info: SessionInfo | null | undefined,
): EffectiveTier | null {
  const t = info?.stats?.transport;
  return t === 'tier0' || t === 'tier1' || t === 'tier2' ? t : null;
}

// ---------------------------------------------------------------- 用户的选择
//
// 上面到此为止都是**现状**。下面是另一个量：**用户在选择器上钉的那一档**。
// 两者不得互相冒充（本文件开头那张表）。

/**
 * 「连通方式」选择器上的四个互斥选项。
 *
 * # 为什么 tier 2 是一个**真的能点**的档，而不是一个置灰的档
 *
 * 曾经有人提议把它画成灰的、旁边写「需要隧道地址」。**那句话是假的**，而且
 * 有三条各自独立的依据：
 *
 * 1. **plan §16.2 逐字**：「**手动覆盖恒可用**：任何对端都可以被钉在指定 tier 上，
 *    包括钉回 Tier 0。」「任何」与「指定」两个词把 tier 2 也包在里面了。
 *    同一节里那句「由对端地址的形态决定」讲的是**自动**那条路为什么不存在
 *    （隧道的属性我们观测不到），它是在解释 tier 2 没有自动入口，
 *    不是在宣布 tier 2 只有地址这一个入口。
 * 2. **daemon 就是这么实现的**：`conn.rs` 里选承载的那一行是
 *    `endpoint.is_some() || tier == Tier2` —— 两个**或**的条件，各自成立。
 *    钉 tier 2 而地址是普通 `IP:端口` ⇒ 走**裸 TCP 上的单连接复用**，
 *    与 §4.3 给 tier 2 的定义（「单连接复用」）完全一致，少的只是 WebSocket 外壳，
 *    而外壳在 §4.3 里本来就写着是「**选项**」。
 * 3. **它跑得通，且有测试**：`transport_tests.rs::tier_two_pair` 钉了 tier 2 之后
 *    直接用普通 `127.0.0.1:端口` 配对连接，双向媒体在那条复用连接上过，
 *    全程没有任何 URL。（同文件另一处钉 tier 2 的 `tier_two_ws_pair` **不算**
 *    这条的证据：它虽然也用普通地址配对，但连接前给对端存了 `ws://` endpoint，
 *    走的是「外壳」那条路。一处足以成立，不要把它数成两处。）
 *
 * 所以置灰会同时违反三件事：一句 plan 原文、一个 daemon 行为、一组绿着的测试。
 * 「点了引导用户去改地址」同样不行——它把一个此刻就能生效的选择变成一次跳转，
 * 用户改完地址回来会发现自己得到的是**另一种** tier 2（带外壳的那种）。
 *
 * 诚实的做法是两个入口都给、并且把它们的差别说清楚：这一组按钮给「复用」，
 * 下面那一格给「外壳 + 隧道地址」，而地址一旦填了就**盖过**这一组
 * （见 [`endpointShadowsTier`]）。
 */
export type TierChoice = 'auto' | 'tier0' | 'tier1' | 'tier2';

/** 选择器上的顺序：自动在先，其余按「越往后越降级」。 */
export const TIER_CHOICES: readonly TierChoice[] = ['auto', 'tier0', 'tier1', 'tier2'];

/** 选项标签。**说人话，不显示内部代号**（§16.4 第 2 条）。 */
export const TIER_PICK_LABEL: Record<TierChoice, MsgKey> = {
  auto: 'detail.transport.tierAuto',
  tier0: 'detail.transport.tier0',
  tier1: 'detail.transport.tier1',
  tier2: 'detail.transport.tier2',
};

/** 选项下面那一行**后果**。每一档都必须有一句，理由见 `styles.css` 上的注释：
 *  一个只写着代号的选项等于让用户选一个他不知道会付什么代价的东西。 */
export const TIER_PICK_HINT: Record<TierChoice, MsgKey> = {
  auto: 'detail.transport.tierAutoHint',
  tier0: 'detail.transport.tier0Hint',
  tier1: 'detail.transport.tier1Hint',
  tier2: 'detail.transport.tier2Hint',
};

/**
 * 把 daemon 报来的 tier 串映射成选项标签。
 *
 * 认不出来的串退回「自动」那一条，**不是**编造一个标签：daemon 装载时就把不
 * 认识的串重置成 `auto` 并通过 `tier_reset_from` 说明，界面在这里跟着说同一
 * 句话，两边不会分岔。
 */
export function tierPickLabel(tier: string | null | undefined): MsgKey {
  return tier && Object.prototype.hasOwnProperty.call(TIER_PICK_LABEL, tier)
    ? TIER_PICK_LABEL[tier as TierChoice]
    : TIER_PICK_LABEL.auto;
}

/** 存了一个隧道地址（`ws://…`）。空串 / 缺席 = 没存。 */
export function hasEndpoint(endpoint: string | null | undefined): boolean {
  return typeof endpoint === 'string' && endpoint.trim() !== '';
}

/**
 * 下一次**本机主动拨号**会不会走单连接复用。
 *
 * **逐字镜像 daemon**（`core/audiohubd/src/conn.rs` 选承载的那一行）：
 *
 * ```rust
 * let tier2 = endpoint.is_some()
 *     || lk(&inner.peer_transport).tier(&peer.fingerprint) == TransportTier::Tier2;
 * ```
 *
 * 写成**或**而不是「tier 说了算」：地址本身就是一次传输选择（plan §16.2），
 * 再要求一个开关去附和它只会造出一个两者打架的状态。界面必须按 daemon 的这条
 * 规则说话，否则就会出现「按钮上写着直连、字节走在复用连接上」——正是本仓
 * 反复审的那类「界面处处自洽、事实不是这样」。
 *
 * ⚠ 只管**出站**。对端拨过来的那条连接由对端的设置决定，本机说了不算。
 */
export function dialsMultiplexed(
  tier: string | null | undefined,
  endpoint: string | null | undefined,
): boolean {
  return hasEndpoint(endpoint) || tier === 'tier2';
}

/**
 * 隧道地址是否**盖过**了选择器上那一档。
 *
 * 为真时界面必须说出来。不说的话，一个选着「直连（UDP）」又填了 `ws://` 的
 * 用户会一直以为自己在直连——而 [`dialsMultiplexed`] 那条规则说他不是。
 */
export function endpointShadowsTier(
  tier: string | null | undefined,
  endpoint: string | null | undefined,
): boolean {
  return hasEndpoint(endpoint) && tier !== 'tier2';
}

/**
 * 隧道地址那一格**显不显示**（用户 2026-08-10 第 19 条：「隧道地址属于单连接复用」）。
 *
 * # 为什么判据不是照字面的 `tier === 'tier2'`
 *
 * 照字面写会造出一个**存得下、看不见、删不掉**的设置。daemon 选承载的判据是
 * 「或」（[`dialsMultiplexed`] 逐字镜像的那一行）：一个选着「直连（UDP）」却存了
 * `ws://` 的对端**此刻已经在走复用**。按 tier 隐藏，会把 [`endpointShadowsTier`]
 * 那条专为这一态写的警告、连同唯一的「清除」按钮一起藏掉——用户既看不见它生效，
 * 也没有地方撤销它。
 *
 * 所以第一条判据直接取 [`dialsMultiplexed`] 而不是另写一遍：**只要下一次拨号会
 * 走复用，这一格就必须在屏幕上**。两者共用一个函数，就不可能再分岔成两套判据。
 *
 * 第三条是 `endpoint_reset_from`——daemon 清掉一个读不懂的地址之后 `endpoint`
 * 恰好是空串，而那句「你存的地址被清掉了」必须还有地方说得出口。静默隐藏它
 * 等于用户的设置消失了，而界面处处自洽。
 */
export function endpointVisible(
  tier: string | null | undefined,
  endpoint: string | null | undefined,
  resetFrom: unknown,
): boolean {
  return dialsMultiplexed(tier, endpoint) || typeof resetFrom === 'string';
}
