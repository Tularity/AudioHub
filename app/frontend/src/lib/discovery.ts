// 发现（mDNS 扫描）的时间常量与纯判据。
//
// 这一层刻意只有常量和纯函数：扫描循环本身活在 `state/discovery.ts`（有 IPC、有
// 定时器、有 store 写入），而「多久算陈旧」「什么时候该自动起扫」这类判断是可以
// 单测的，就必须能单测——本项目已经吃过一次「判据散在组件里，改了一处漏了另一处」
// 的亏（见 state/store.ts 顶部那两条规矩）。
//
// 下面四个时间常量都是**判断**，不是测量结论。改它们要连着依据一起改注释，
// 不许只改数字。

import type { DiscoverResult } from '../ipc/types';

/**
 * 一次自动扫描的窗口：60 秒。
 *
 * 依据四条：
 *  1. 一轮的成本 —— `discover.run {secs:2}` + `SCAN_GAP_MS` ≈ 2.4 s/轮，60 s ≈ 25 轮。
 *  2. mDNS 侧的收益上限 —— RFC 6762 §5.2 的连续查询退避是 1→2→4 s，在场的响应者
 *     绝大多数在头两秒内就答复，25 轮远超「还没答复的会答复」这个窗口。
 *  3. 人机往返才是真正的下限 —— 配对是「对端开启配对 → 读 PIN → 回本机输入」，
 *     实测常在 20–40 s。30 s 会在用户还没操作完时熄火，那是最差的一种「智能」。
 *  4. 不取 120 s（= 配对 PIN 的 TTL）—— 那会让共享的那条 IPC 连接在整整两分钟里
 *     有 ~83% 的时间被 `discover.run` 占着。发现与 PIN 有效期是两件事，PIN 到期
 *     有自己的熄灯逻辑，不需要扫描陪着它。
 */
export const SCAN_WINDOW_MS = 60_000;

/**
 * 结果转「陈旧」的年龄：45 秒。
 *
 * 一个完整扫描窗口（60 s）内如果一台主机连续两轮没答复，它多半已经不在了；
 * 45 s ≈ 一轮窗口的 3/4，留了余量给单次丢包。
 */
export const RESULT_STALE_MS = 45_000;

/**
 * 结果被移除的年龄：5 分钟。
 *
 * mDNS 对 SRV/TXT 记录的惯例 TTL 是 120 s（RFC 6762 §10）。取它的 2.5 倍作为
 * 「我们自己这份缓存」的寿命：既比协议侧宽松（不会把还在的主机赶走），又不会把
 * 关机半小时的主机一直挂着——用户点它只会换回一次配对超时。
 */
export const RESULT_EXPIRE_MS = 300_000;

/**
 * 自动起扫的冷却期：15 秒。
 *
 * 它挡的是「在配对页与别处之间来回切」把 `discover.run` 排满那条共享 IPC 连接。
 * 也是「用户手动停过之后要离开多久才允许再次自动起扫」的门槛——否则「停止」
 * 按钮点了等于没点。
 */
export const SCAN_COOLDOWN_MS = 15_000;

/** 单次短扫描之间的间隔：给共享的那一条 IPC 连接留出处理其它请求的空隙。 */
export const SCAN_GAP_MS = 400;

/** 单次 `discover.run` 的时长（秒）。 */
export const SCAN_SECS = 2;

/**
 * 连续失败到这个数就停。
 *
 * 搬出组件之前，失败分支是 `sleep(1000); continue` —— 一个 1 Hz 的无限重试，
 * 靠「用户切走页面时组件卸载」兜着。循环脱离组件生命周期之后没人兜了，
 * 无限重试会变成一个永远跑着的循环。
 */
export const MAX_SCAN_FAILURES = 3;

/** 缓存上限。见 `store.ts` 的 `mergeDiscover`：截断前必须先按 `lastSeen` 排序。 */
export const MAX_RESULTS = 50;

/** 列表里的稳定标识：指纹优先，没有就退回「实例名-端口」。 */
export function discoverKey(d: DiscoverResult): string {
  return d.fingerprint || `${d.instance || 'unknown'}-${d.port}`;
}

/**
 * 记录的年龄（ms）。**没有 `lastSeen` 时返回 null**，不是 0。
 *
 * 0 是一个具体且极新的年龄，而「不知道多久以前见过」是没有读数——两者折成同一个
 * 值就是规格 §3.3 那条红线的另一种犯法。调用方各自决定「不知道」该怎么呈现。
 */
export function resultAge(d: DiscoverResult, now: number): number | null {
  const seen = d.lastSeen;
  if (typeof seen !== 'number' || !isFinite(seen) || seen <= 0) return null;
  return now - seen;
}

/**
 * 「陈旧」。**年龄不明也算陈旧**：不知道多久以前见过的记录，不许和两秒前刚答复的
 * 记录长成同一个样子（§14 裁定 2 / §16.4 第 5 条同源——「读不到」不许渲染成「读到了」）。
 */
export function isStale(d: DiscoverResult, now: number): boolean {
  const age = resultAge(d, now);
  if (age == null) return true;
  return age >= RESULT_STALE_MS;
}

/**
 * 「过期」，要从列表里移除。
 *
 * 年龄不明的记录**不过期**：删掉一条自己都没法定年的记录，是在没有证据的情况下
 * 做不可逆的事。实践中 `mergeDiscover` 每条都盖时间戳，这一支只是防御；
 * 真有漏网的也被 `MAX_RESULTS` 兜着，不会无限增长。
 */
export function isExpired(d: DiscoverResult, now: number): boolean {
  const age = resultAge(d, now);
  if (age == null) return false;
  return age >= RESULT_EXPIRE_MS;
}

/** 过期的丢掉，其余按最近一次出现倒序。返回新数组，不改入参。 */
export function visibleResults(list: readonly DiscoverResult[], now: number): DiscoverResult[] {
  return list
    .filter((d) => !isExpired(d, now))
    .sort((a, b) => (b.lastSeen || 0) - (a.lastSeen || 0));
}

/** 截断前先排序：原来的 `results.length = 50` 截的是插入序的尾部，满 50 之后新
 *  发现的主机会被直接丢掉——最该留下的那条反而最先被扔。 */
export function capResults(list: readonly DiscoverResult[], now: number): DiscoverResult[] {
  const kept = visibleResults(list, now);
  return kept.length > MAX_RESULTS ? kept.slice(0, MAX_RESULTS) : kept;
}

/** 扫描剩余时间（ms），非负。`deadlineAt <= 0` 表示没有正在跑的窗口。 */
export function remainingMs(deadlineAt: number, now: number): number {
  if (!deadlineAt || deadlineAt <= 0) return 0;
  return Math.max(0, deadlineAt - now);
}

/** 剩余秒数，向上取整——显示 0 秒却还在转是自相矛盾。 */
export function remainingSecs(deadlineAt: number, now: number): number {
  return Math.ceil(remainingMs(deadlineAt, now) / 1000);
}

export interface AutoScanInput {
  /** `AppState.conn`。不是 'online' 就不发请求：那只会排一队必然超时的 RPC。 */
  conn: string;
  running: boolean;
  /** 用户按过「停止扫描」，且还没离开页面够久。 */
  userStopped: boolean;
  /** 上一次停止的时刻（任何原因）。0 = 从未停过。 */
  lastStopAt: number;
  now: number;
}

/** 进入配对页时是否自动起扫（B.11）。 */
export function shouldAutoScan(i: AutoScanInput): boolean {
  if (i.running) return false;
  if (i.conn !== 'online') return false;
  if (i.userStopped) return false;
  if (i.lastStopAt > 0 && i.now - i.lastStopAt < SCAN_COOLDOWN_MS) return false;
  return true;
}

/**
 * 「用户按过停止」这个标志什么时候作废：关掉配对面板超过一个冷却期。
 *
 * 不用「关掉就清」是因为那等于没有这个标志（关掉再开就自动重扫）；
 * 也不用「永远不清」是因为下一次特意打开「添加对端」的人，要的就是扫描。
 */
export function shouldClearUserStop(leftPairPanelAt: number, now: number): boolean {
  return leftPairPanelAt > 0 && now - leftPairPanelAt >= SCAN_COOLDOWN_MS;
}

/** 一次循环迭代该不该继续，不继续是因为什么。 */
export type ScanVerdict = 'go' | 'superseded' | 'deadline' | 'offline';

export interface ScanLoopInput {
  /** 本循环启动时领到的世代号。 */
  mine: number;
  /** 当前世代号。两者不等 = 有人起了新的一轮，本循环作废。 */
  gen: number;
  running: boolean;
  conn: string;
  deadlineAt: number;
  now: number;
}

/**
 * 扫描循环的**全部**停止条件，一处写清。
 *
 * 搬出组件之前这张表只有一条半：世代号不匹配就退出，其余靠「用户切走页面时组件
 * 卸载」兜底。循环成为模块级单例之后没人兜了——泄漏的形态从「多一个循环」变成
 * 「永远跑的循环」，所以每一条都得自己成立，也都得能单测。
 *
 * 判序有意如此：**先看自己是不是还有效**。一个已被作废的循环不该顺手去调
 * `stopScan`，那会把刚起来的新循环一起打死。
 */
export function scanVerdict(i: ScanLoopInput): ScanVerdict {
  if (i.mine !== i.gen || !i.running) return 'superseded';
  if (i.deadlineAt > 0 && i.now >= i.deadlineAt) return 'deadline';
  // daemon 不在线时继续发请求，只会排一队必然超时的 RPC。
  if (i.conn !== 'online') return 'offline';
  return 'go';
}

/** 连续失败到上限就收工。替代了原来那个 1 Hz 的无限重试。 */
export function shouldStopAfterFailure(failures: number): boolean {
  return failures >= MAX_SCAN_FAILURES;
}
