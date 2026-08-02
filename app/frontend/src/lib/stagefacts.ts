// 逐级明细里那一行「事实标签」的内容判定。
//
// 它是**呈现逻辑但不是 JSX**：给一级读数，回一组 {短标签, 长解释, 要不要警示}。
// 从 PeerMetrics.tsx 里抽出来的唯一理由是**可被回归断言**——「某条事实压根没被
// 渲染出来」是类型检查看不见的那类缺陷，而这四条事实恰恰是排障时唯一能把几种
// 病理学假说分开的东西（规格 §0.2 / §3.3）。抽成不含 JSX 的模块之后，
// regress/metrics-latency.mjs 用 esbuild 一转就能在 Node 里直接跑它。

import { fmt } from './fmt';
import { t } from '../i18n';
import type { MsgKey } from '../i18n';
import type { StageReading } from './metrics';

export interface Chip {
  id: string;
  text: string;
  title: string;
  /** 需要引起注意（已满 / 有丢弃 / 正在漂移）。 */
  warn?: boolean;
}

/**
 * 这一级是**一条真的队列**吗？只有队列才谈得上容量占用 / 丢弃 / 深度趋势。
 *
 * 三类不是队列的级：
 * - `cap_dev` / `play_dev`：声卡固有延迟，是一个**常数属性**，没有可丢的东西；
 * - `network` / `residual`：由 RTT 与减法算出来的标量，压根没有存量；
 * - `send_pace`：打包节拍的期望值（半个 tick），同样是常数。
 *
 * 判据取 daemon 已经给的两个字段，不另立名单：有 `drop_mode` 说明它有丢弃语义，
 * 有 `capacity` 说明它有容量。上面三类在 readLatency 里本就不带 `drop_mode`
 * （常数级 daemon 报 `none` 且容量为 0），于是自然落在门外——**加级的人不必回来
 * 维护一张白名单**，那种名单漏一条就是一行静默的噪声或静默的缺席。
 */
function hasQueue(r: StageReading): boolean {
  if (!r.dropMode) return false;
  return r.dropMode !== 'none' || !!r.capacity;
}

/**
 * 一级的四条事实：容量占用（`saturated` 的原始证据）、丢弃方向、丢弃量、深度趋势。
 *
 * 为什么这四条必须同时在场（规格 §0.2 / §3.3）：
 *
 * - 深度读数本身在**丢头 / 丢尾**两种语义下完全简并——两者饱和时都恰好等于
 *   cap/rate，但一个听起来是「恒定迟到但连续」，另一个是「迟到 + 周期性断续」。
 *   只有 `drop_mode` 能把它们分开。
 * - 饱和之后，「曾被一次卡顿灌满、之后收支平衡」与「稳态产销速率失配」两种病
 *   的深度读数也一模一样，只有 `dropped` 是不是还在涨能分开——而它们的修法完全
 *   不同。所以 `dropped` 为 0 时也照样出一条「未丢弃」，沉默会被读成「没在丢」，
 *   可沉默同时也是「数不到」的样子。
 * - `dropped` / `drift_sps` 的**缺席**是有信息的：前者 = 丢弃发生在驱动那一侧，
 *   本机数不到；后者 = 样本点还不够判趋势。两者都不等于「零」。
 *
 * 反过来，**不是队列的级一条标签都不出**（见 hasQueue）：给「声卡播放缓冲」挂一句
 * 「丢弃数不可见」是纯噪声——那一级根本没有队列可丢，说它「数不到」等于暗示那里
 * 有个我们看不见的丢弃点。十二行明细里每行多三条无意义标签，真正在漂的那一级就
 * 被淹掉了。
 */
export function stageChips(r: StageReading | undefined): Chip[] {
  if (!r || !hasQueue(r)) return [];
  const out: Chip[] = [];

  // 容量占用。无界的级（capacity 为 0，如打包节拍）没有这条。
  if (r.capacity && r.samples !== undefined) {
    const pct = (r.samples / r.capacity) * 100;
    out.push({
      id: 'fill',
      text: r.saturated
        ? t('latency.stage.fullAt', { pct: fmt.int(pct) })
        : t('latency.stage.fill', { pct: fmt.int(pct) }),
      title: t('latency.stage.fillWhy', { n: fmt.count(r.samples), cap: fmt.count(r.capacity) }),
      warn: r.saturated,
    });
  }

  const DROP: Record<string, { short: MsgKey; why: MsgKey }> = {
    oldest: { short: 'latency.stage.dropOldestShort', why: 'latency.stage.dropOldest' },
    newest: { short: 'latency.stage.dropNewestShort', why: 'latency.stage.dropNewest' },
    none: { short: 'latency.stage.dropNoneShort', why: 'latency.stage.dropNone' },
  };
  const drop = r.dropMode ? DROP[r.dropMode] : undefined;
  if (drop) out.push({ id: 'drop', text: t(drop.short), title: t(drop.why) });

  // 丢弃量。三态：数得到且在丢 / 数得到且没丢 / 数不到。
  // 只在这一级**会**丢的时候说——`drop_mode: none` 的级（有界但从不饱和）
  // 已经由上面那条「不丢弃」讲完了，再补一句丢弃量是自相矛盾。
  if (r.dropMode === 'none') {
    // 无话可说
  } else if (r.dropped === undefined) {
    out.push({
      id: 'dropped',
      text: t('latency.stage.droppedUnknown'),
      title: t('latency.stage.droppedUnknownWhy'),
    });
  } else {
    out.push({
      id: 'dropped',
      text: r.dropped > 0
        ? t('latency.stage.droppedN', { n: fmt.count(r.dropped) })
        : t('latency.stage.droppedNone'),
      title: t('latency.stage.droppedWhy'),
      warn: r.dropped > 0,
    });
  }

  // 深度趋势。折算成 ms/min 才好读；原始的 样本/秒 留在 title 里。
  if (r.driftMsPerMin === undefined) {
    out.push({
      id: 'drift',
      text: t('latency.stage.driftUnknown'),
      title: t('latency.stage.driftUnknownWhy'),
    });
  } else {
    const why = t('latency.stage.driftWhy', { sps: fmt.decimal1(r.driftSps) });
    out.push({
      id: 'drift',
      text: !r.drifting
        ? t('latency.stage.driftFlat')
        : t(r.driftMsPerMin > 0 ? 'latency.stage.driftUp' : 'latency.stage.driftDown',
          { ms: fmt.decimal1(Math.abs(r.driftMsPerMin)) }),
      title: why,
      warn: !!r.drifting && r.driftMsPerMin > 0,
    });
  }

  return out;
}
