// 对端卡片的一级指标区：**按方向分成两块**，每块一行延迟 + 音质 + 电平/码率，
// 可就地展开该方向的分段明细。
//
// ## 为什么分方向（2026-08-04 事故，本文件是事故现场）
//
// 改版前这里是：
//
//     <PeerMetrics fp={fp} peer={peer} sess={micS || spkS} />
//
// `micS`（取对方麦克风，recv）优先，**`spk/send` 那条从未被渲染过**。现场两条
// 通路同时在跑：`spk/send` sum≈105 ms、`mic/recv` sum≈170 ms，屏幕上只有 170，
// 而四段色带把 `hal_mic` 的 136 ms 归进 `playback` 段、显示成「播放 136」——
// **没有一个字说明这是麦克风方向的尾级**。一个理性的用户读到「延迟 170、
// 播放占 136」，唯一自然的结论就是「扬声器那边慢」，而扬声器那条实测 105，
// 快 62 %，且一次都没上过屏。
//
// 所以病灶不是 `readLatency`（它诚实地报告了喂给它的那条会话），是**上层用
// `||` 在两条真实存在的通路里选了一条，还没在界面上留下"这是哪一条"的痕迹**。
// 修法只能是：两条都渲染，各自带方向标签。
//
// ## 顺带修掉的第二个缺陷
//
// `micS`/`spkS` 只匹配**使用端**的两种 `(kind, dir)` 组合。共享模式下本机跑的
// 是 `mic/send` 与 `spk/recv`，两者都匹配不上 ⇒ 指标区恒显示「未建立通路」，
// 而隔壁 `inbound-mic-<fp>` 隐私横幅同时亮着。同一张卡上下自相矛盾。
// 分栏之后判据换成 `dir`，四种组合**全部**落进某一栏。
//
// ## 同方向可以有多条会话
//
// `conn.rs` 的 `MAX_STREAMS_PER_CONN = 16`，去重只按 `stream_id`，**从不按
// `(peer, kind, dir)`**；`peers.card.inboundMicN`（「对方正在取用本机麦克风
// （{n} 路）」）这条语料的存在本身就是证据。所以这里**不许再用 `find`**——
// 那只是把「两个方向里选一个」降级成「同方向 N 条里选第一条」，同一个 bug
// 换了个轴。见 `pickWorst`。
//
// 读数一律走 lib/metrics 的两个读取入口；缺失即「—」，绝不用 0 填补
//（见 lib/metrics.ts 顶部的红线）。

import { useState } from 'react';
import { fmt } from '../lib/fmt';
import { t, joinPhrases } from '../i18n';
import { stageChips } from '../lib/stagefacts';
import { Meter } from './Controls';
import {
  LATENCY_SEGMENTS, LATENCY_STAGES, SEGMENT_LABEL, QUALITY_DOTS, QUALITY_PARTS,
  QUALITY_PART_DESC, QUALITY_PART_NAME,
  confidenceKey, coversWholeChain, isLowerBound, isQualityMeasuring, latencyGrade,
  latencyGradeKey, latencyTone, latencyValueKey, medianOf5, qualityDots,
  qualityDepthKey, qualityGradeTextKey, qualityTone, qualityWorstKey,
  pickWorst, readLatency, readPeerNet, readQuality, segmentDominantStage, stageHost,
} from '../lib/metrics';
import type {
  Dir, LatencyReading, PeerNetReading, QualityReading, StageReading, StageSpec,
} from '../lib/metrics';
import { TIER_LABEL, TIER_WHY, effectiveTier, isDegradedTier } from '../lib/tier';
import type { EffectiveTier } from '../lib/tier';
import { useStore } from '../state/store';
import type { PeerState, SessionInfo } from '../ipc/types';

/** 对端分项超过这个岁数就当陈旧（daemon 侧同一口径，规格 §3.5）。 */
const PEER_STALE_S = 3;

/** 空序列的**共享**实例：选择器每次返回新数组会让 zustand 判定状态变了，逐帧重渲。 */
const EMPTY: number[] = [];

// 小于 10 ms 的分段（典型是网络单程 0.3 ms）取整会全变成 0，读起来像「没有」。
function segNum(ms: number): string {
  return ms < 10 ? fmt.decimal1(ms) : fmt.int(ms);
}

function stageText(r: StageReading | undefined): string {
  return r && typeof r.ms === 'number'
    ? t('latency.stage.ms', { ms: segNum(r.ms) })
    : t('latency.stage.unknown');
}

// ---------------------------------------------------------------- 延迟

/**
 * `ms` 不是直接取 `lat.totalMs`，而是取**最近 5 个采样的中位数**（规格 §2.6）：
 * 瞬时队列深度抖 ±10 ms，直接显示会让数字每秒乱跳，用户会以为界面在骗人。中位数
 * 比均值更合适——一次 GC 停顿造成的单点尖峰不该把读数拉走。
 *
 * 历史序列由 store 提供（`pushMaybe` 保证缺读数时不入列，所以序列里不会混进 0）。
 * 就地展开的明细**保持瞬时**：排障看的就是那一下的抖动，中位数会把它抹平。
 */
function LatencyCell({ fp, dir, lat, series }: {
  fp: string; dir: Dir; lat: LatencyReading | undefined; series: number[];
}) {
  // 序列还没攒起来（首帧）就退回瞬时值——**不退回 undefined**：那会让刚建立的
  // 通路在第一秒显示「—」，而我们明明已经有一个读数了。
  const ms = lat && typeof lat.totalMs === 'number'
    ? (medianOf5(series) ?? lat.totalMs)
    : undefined;
  const lower = isLowerBound(lat);
  // 等级是**端到端判断**。只覆盖本机侧的读数上不成立：缺的不是几十毫秒的声卡
  // 缓冲，而是对方整整一半管线，量级无上界（见 metrics.ts 的 coversWholeChain）。
  const whole = coversWholeChain(lat);
  const grade = typeof ms === 'number' && whole ? latencyGrade(ms) : undefined;
  // 撤掉等级词之后必须补一句话说清「撤掉的原因是范围不够，不是没测到」，
  // 否则 ≥474 ms 孤零零挂在那里，仍然会被读成端到端总延迟。
  const scope = typeof ms === 'number' && !whole ? t('metric.latency.scopeLocal') : '';

  const why = joinPhrases([
    lower && whole ? t('metric.latency.lowerBoundWhy') : null,
    lat && lat.deviceUnreliable ? t('latency.conf.deviceUnreliable') : null,
    // `full` 在 2026-08-04 之前**不可达**（两级声卡固有延迟根本没查）。现在它
    // 可达了，而一个不带「≥」的数字与一个带「≥」的数字在界面上只差一个字符——
    // 必须有一句话说出这次是凭什么去掉的，否则用户无从判断该不该信这个精度。
    lat && lat.confidence === 'full' ? t('latency.conf.fullWhy') : null,
  ]);

  let value: string;
  if (typeof ms === 'number') value = t(latencyValueKey(lat), { ms: fmt.int(ms) });
  else if (lat && lat.confidence === 'converging') value = t('metric.latency.measuring');
  else if (lat && lat.confidence === 'unavailable') value = t('metric.latency.unsupported');
  else value = t('metric.latency.none');

  return (
    <span className="metric-cell" data-testid={`metric-latency-${dir}-${fp}`}>
      <span className="metric-cap">{t('metric.latency.label')}</span>
      {/* 没有等级就没有色阶：给一个只覆盖半条链路的数字上色，等于替它做了那个
          不成立的端到端判断。此时用正文色（既不是 tone-*，也不是「读不到」的暗色）。 */}
      {/* 「≥」的理由挂在数字自己身上。此前它是数字旁边一枚独立的 `?` 角标——
          实测在 320px 卡宽上，那 12px + 一处 gap 正好把等级词挤成
          「可用于...」，而等级词才是这一格面向用户的那一半，数字只是它的依据。
          理由没有丢：它进了这个 span 的 title，而「≥」本身就是可见的提示符。 */}
      <span
        className={`metric-val${grade ? ` tone-${latencyTone(grade)}` : typeof ms === 'number' ? '' : ' unknown'}`}
        data-testid={`metric-latency-value-${dir}-${fp}`}
        title={why || undefined}
      >
        {value}
      </span>
      <span className="metric-grade" data-testid={`metric-latency-grade-${dir}-${fp}`} hidden={!grade}>
        {grade ? t(latencyGradeKey(grade)) : ''}
      </span>
      <span className="metric-scope" data-testid={`metric-latency-scope-${dir}-${fp}`} hidden={!scope}>
        {scope}
      </span>
    </span>
  );
}

/**
 * 四段色带 + 四段数字。**双栏之后它下沉进展开区**，一级界面上不再出现。
 *
 * 这不是为简洁而藏，是因为它在一级界面上会**撒谎**：段名是按物理流向定的
 * （`lib/metrics.ts` 的 `LATENCY_STAGES`），`hal_mic`（虚拟麦克风环）与
 * `play_ring`（真实播放环）并列归在 `playback` 段。接收方向的 136 ms 全在
 * `hal_mic` 上，一级界面却写着「播放 136」——用户据此去查扬声器，一查一个准
 * 地查错方向。放进展开区之后它紧挨着 `StageRow`（带级名与「本机 / 对方主机」），
 * 段名不再是唯一线索。`title` 里再点名这一段此刻的大头是哪一级，双保险。
 */
function LatencyBand({ fp, dir, lat }: { fp: string; dir: Dir; lat: LatencyReading | undefined }) {
  const vals = LATENCY_SEGMENTS.map((id) => (lat ? lat.segments[id] : undefined));
  const known = vals.some((v) => typeof v === 'number');

  return (
    <>
      <div className="metric-band" data-testid={`latency-band-${dir}-${fp}`} data-empty={known ? undefined : 'true'}>
        {LATENCY_SEGMENTS.map((id, i) => (
          <span
            key={id}
            className={`band-seg band-${id}`}
            data-testid={`latency-band-${id}-${dir}-${fp}`}
            // 未知时四段等宽：那是「还没测到」的形状，不是「四段一样长」的结论。
            style={{ flexGrow: known ? Math.max(0.001, vals[i] ?? 0) : 1 }}
          />
        ))}
      </div>
      <div className="metric-segs" data-testid={`latency-segs-${dir}-${fp}`}>
        {LATENCY_SEGMENTS.map((id, i) => {
          const v = vals[i];
          // 这一段此刻的大头是哪一级。段名是静态的、按物理流向定的，而同一段里
          // 并列着好几级（`playback` 段有 play_ring / bridge_ring / hal_mic 三条
          // **并行**尾级）——不点名就会把「虚拟麦克风环」读成「播放」。
          const dom = lat ? segmentDominantStage(lat, id) : undefined;
          return (
            <span
              key={id}
              className="seg-item"
              data-testid={`latency-seg-${id}-${dir}-${fp}`}
              title={dom ? t('latency.seg.dominant', { name: t(dom.nameKey) }) : undefined}
              style={{ flexGrow: known ? Math.max(0.001, v ?? 0) : 1 }}
            >
              <span className="seg-name">{t(SEGMENT_LABEL[id])}</span>
              <span className="seg-num">{typeof v === 'number' ? segNum(v) : t('metric.latency.none')}</span>
            </span>
          );
        })}
      </div>
    </>
  );
}

/**
 * 一行一级。**两行结构**：第一行 名称 / 主机 / ms，第二行是可换行的事实标签。
 *
 * 改版前四条事实挤在一个 `.stage-note` 里，而那个 span 是 `white-space: nowrap` +
 * `text-overflow: ellipsis`——「满时丢弃最早的音频（听感：恒定迟到但连续）」一条
 * 就把整行占满，**饱和 / 丢弃量 / 漂移在实际渲染里从来没有被看见过**。字段接上了
 * 但看不到，和没接上是同一个结果。
 */
function StageRow({ fp, dir, spec, r, side }: {
  fp: string; dir: Dir; spec: StageSpec; r: StageReading | undefined; side: 'send' | 'recv' | undefined;
}) {
  const host = stageHost(spec, r, side);
  const chips = stageChips(r);
  return (
    <div className="stage-item" data-testid={`latency-stage-${spec.id}-${dir}-${fp}`}>
      <div className="stage-row">
        <span className="stage-name" title={t(spec.descKey)}>{t(spec.nameKey)}</span>
        <span className="stage-host" hidden={!host}>
          {host === 'peer' ? t('latency.stage.onPeer') : host === 'local' ? t('latency.stage.onLocal') : ''}
        </span>
        <span className={`stage-ms${r && typeof r.ms === 'number' ? '' : ' unknown'}`}>{stageText(r)}</span>
      </div>
      <div className="stage-facts" hidden={chips.length === 0} data-testid={`latency-stage-facts-${spec.id}-${dir}-${fp}`}>
        {chips.map((c) => (
          <span
            key={c.id}
            className={`stage-chip${c.warn ? ' warn' : ''}`}
            data-testid={`latency-stage-${c.id}-${spec.id}-${dir}-${fp}`}
            title={c.title}
          >
            {c.text}
          </span>
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- 音质

/**
 * ## 这一格显示的是**采样率**，不是带宽（2026-08-04 用户实测报告）
 *
 * 曾经这里写 `bandwidthKhz`。用户在详情页把音质设成 `PCM 48 kHz`，卡片上这一格
 * 显示 `24 kHz`，于是判定「设置没生效」——而设置生效得好好的：24 kHz 是 48 kHz
 * 采样率对应的奈奎斯特带宽，两个数**差 2 倍且都写作 kHz**。
 *
 * 病灶不是哪个数算错了（都对），是**同一个界面上设置与显示用了不同量纲**。
 * 一级界面这一格与设置那个滑条是用户唯一会去对照的一对，所以它必须与滑条同量纲：
 * 设 48，这里就写 48。
 *
 * 带宽没有丢：它进了展开明细的 Q3 行，并且在那里**与采样率并排显示**
 * （`quality.part.bandwidth.value`），2 倍关系是看得见的，不需要用户自己去推。
 */
/**
 * 线上位深的人类可读串。`undefined` = 对面没说（或拼写认不出）—— 调用方
 * 就只写采样率。映射表本身在 `lib/metrics` 的 `qualityDepthKey`，
 * **全应用只此一份**，理由见那里。
 */
function depthLabel(d: string | undefined): string | undefined {
  const k = qualityDepthKey(d);
  return k ? t(k) : undefined;
}

function QualityCell({ fp, dir, q }: { fp: string; dir: Dir; q: QualityReading | undefined }) {
  const grade = q ? q.grade : undefined;
  const dots = qualityDots(grade);
  const khz = q ? q.wireRateKhz : undefined;
  // 位深进阶梯之后**两个维度一起写**：`48 kHz` 单独说不出它是 16 位还是 24 位，
  // 而这两档现在都存在、码率差 50 %。位深读不到（旧 daemon）就只写采样率——
  // **不许猜一个 16 bit 填上**，那正是这个字段要消灭的那种「看起来对、其实是编的」。
  const depthText = depthLabel(q?.wireDepth);
  const value = typeof khz !== 'number'
    ? t('metric.quality.none')
    : depthText
      ? t('metric.quality.rateDepth', { khz: fmt.int(khz), depth: depthText })
      : t('metric.quality.rate', { khz: fmt.int(khz) });

  // 三态判定（有等级 / 有读数但等级不成立 / 什么都没有）在 lib/metrics 里，那样它
  // 可以被回归断言——「某个状态压根没被渲染」是类型检查看不见的那类缺陷。
  const measuring = isQualityMeasuring(q);
  const gradeKey = qualityGradeTextKey(q);
  const gradeText = gradeKey ? t(gradeKey) : '';
  // 出处标记。**必须在数字旁边**，不能只进 title：本机在发送方向上没有音质测点、
  // 根本量不到，不标就等于让这张卡宣称了一个它测不出来的结论。
  //
  // 它**只在一栏出现**这件事本身就在教用户「两个方向不对称」，比任何一句解释都省。
  // 注意判据是 `fromPeer`（数据驱动，见 metrics.ts 的 `fromPeer: !own`），
  // **不是方向**：一条 recv 会话回退到 `peer_quality` 时它同样该出现。
  const fromPeer = !!q?.fromPeer;

  return (
    <span className="metric-cell" data-testid={`metric-quality-${dir}-${fp}`}>
      <span className="metric-cap">{t('metric.quality.label')}</span>
      <span
        className="quality-dots"
        data-testid={`metric-quality-dots-${dir}-${fp}`}
        data-state={measuring ? 'measuring' : undefined}
        aria-hidden="true"
      >
        {Array.from({ length: QUALITY_DOTS }, (_, i) => (
          <span key={i} className={`qdot${i < dots ? ` on tone-${grade ? qualityTone(grade) : 'ok'}` : ''}`} />
        ))}
      </span>
      {/* 与延迟那格同一条规矩：`.unknown` 的暗色只表示**读不到**。采样率在「测量中」
          这一态里是**已经读到的真值**，把它调暗等于说它也没读到——而那正是
          这次要修的那种「让不知道和坏消息长得一样」的呈现。
          title 点明这个数是哪一个量：一个孤立的「48 kHz」既可以被读成采样率、
          也可以被读成带宽，而这两件事在这套界面上正好差 2 倍。 */}
      <span
        className={`metric-val small${grade ? ` tone-${qualityTone(grade)}` : typeof khz === 'number' ? '' : ' unknown'}`}
        data-testid={`metric-quality-value-${dir}-${fp}`}
        title={typeof khz === 'number' ? t('metric.quality.rateWhy') : undefined}
      >
        {value}
      </span>
      {/* `title` 是 `metric.quality.rateDepth` 那条注释里承诺的兜底，必须真的存在。
          这一行的宽度账：`.dir-head` 是 `overflow:hidden`，`.metric-val` 是
          `white-space:nowrap; flex:none` ⇒ **等级词是这一行里唯一可收缩的元素**。
          值从「48 kHz」变成「48 kHz · 32 bit 浮点」之后，多出来的宽度全部从这里
          扣，而 styles.css 里已经记载过「18px 的 gap 就会把两个等级词全挤成省略号」
          ——余量本来就是零。被截断时至少还能悬停读回来。 */}
      <span
        className={`metric-grade${measuring ? ' measuring' : ''}`}
        data-testid={`metric-quality-grade-${dir}-${fp}`}
        title={gradeText || undefined}
        hidden={!gradeText}
      >
        {gradeText}
      </span>
      {/* 一枚克制的角标，与「未含对方主机」同一套视觉语言（小字 + 边框），但用
          dim 而不是 warn：这不是警告——读数是真的，只是量它的人在对面。 */}
      <span
        className="metric-origin"
        data-testid={`quality-frompeer-${dir}-${fp}`}
        title={t('metric.quality.fromPeerWhy')}
        hidden={!fromPeer}
      >
        {fromPeer ? t('metric.quality.fromPeer') : ''}
      </span>
    </span>
  );
}

function QualityParts({ fp, dir, q }: { fp: string; dir: Dir; q: QualityReading | undefined }) {
  // 三分量物理上互不换算，所以逐条列出而不是给一个 0–100 分：分数假装可加，
  // 而「73 分」回答不了「哪一项拖后腿」（规格 §4.4）。
  function partValue(id: (typeof QUALITY_PARTS)[number]): string {
    if (!q) return t('metric.quality.none');
    if (id === 'continuity') {
      return typeof q.concealPct === 'number'
        ? t('quality.part.continuity.value', { pct: fmt.pct(q.concealPct) })
        : t('metric.quality.none');
    }
    if (id === 'level') {
      return typeof q.clipPct === 'number'
        ? t('quality.part.level.value', { pct: fmt.pct(q.clipPct), db: fmt.decimal1(q.clipExcessDb) })
        : t('metric.quality.none');
    }
    // Q3 这一行**同时给两个数**：带宽（本分量本身）与它的来源采样率。
    //
    // 只给带宽 ⇒ 就是一级界面那次误读的形态（用户设 48、读到 24）。
    // 只给采样率 ⇒ 丢掉 Q3 本身，而 Q3 是三分量之一、等级的输入之一。
    // 并排给 ⇒ 2 倍关系当场可见，不需要用户自己去推，也不需要他先知道奈奎斯特。
    // 采样率读不到（旧 daemon）时退回只给带宽——**不拿 ×2 顶替**。
    if (typeof q.bandwidthKhz !== 'number') return t('metric.quality.none');
    return typeof q.wireRateKhz === 'number'
      ? t('quality.part.bandwidth.valueWithRate', {
        khz: fmt.int(q.bandwidthKhz),
        rate: fmt.int(q.wireRateKhz),
      })
      : t('quality.part.bandwidth.value', { khz: fmt.int(q.bandwidthKhz) });
  }

  return (
    <div className="quality-parts" data-testid={`quality-detail-${dir}-${fp}`}>
      {QUALITY_PARTS.map((id) => (
        <div key={id} className="stage-row" data-testid={`quality-part-${id}-${dir}-${fp}`}>
          <span className="stage-name" title={t(QUALITY_PART_DESC[id])}>{t(QUALITY_PART_NAME[id])}</span>
          <span className="stage-note" hidden={q?.worst !== id}>
            {q?.worst === id ? t(qualityWorstKey(id)) : ''}
          </span>
          <span className={`stage-ms${q ? '' : ' unknown'}`}>{partValue(id)}</span>
        </div>
      ))}
      {/* 出处整句。角标只有四个字（「对端测得」），展开明细的人要的是那句完整的
          解释——为什么本机给不出这三个数字。 */}
      <p className="metric-foot" data-testid={`quality-frompeer-note-${dir}-${fp}`} hidden={!q?.fromPeer}>
        {q?.fromPeer ? t('metric.quality.fromPeerWhy') : ''}
      </p>
      {/* 等级已经触底时，缺一块板改不了结论——于是 grade 有值而 partial 仍为真。
          这两件事都要说：结论成立，但它是在缺一项的情况下得出的（只会更低）。 */}
      <p className="metric-foot" data-testid={`quality-partial-${dir}-${fp}`} hidden={!(q?.grade && q?.partial)}>
        {q?.grade && q?.partial ? t('metric.quality.partial') : ''}
      </p>
      {/* 窗口长度**没有兜底值**：`?? 10` 会让一个什么都没测的窗口宣称「最近 10 秒」，
          和用 0 填补缺失分项是同一类错误，只是 10 更难被发现。读不到就整行不出。 */}
      <p className="metric-foot" data-testid={`quality-window-${dir}-${fp}`} hidden={typeof q?.windowS !== 'number'}>
        {typeof q?.windowS === 'number' ? t('quality.part.window', { s: fmt.int(q.windowS) }) : ''}
      </p>
    </div>
  );
}

// ------------------------------------------------- 连接级网络延迟（无会话时）

/**
 * 「连上了，但还没人在用」时唯一能给出的延迟读数。
 *
 * 它属于**连接**，不属于任何一个方向（`PeerState.net_ms` 是控制面 min-RTT/2，
 * 一条连接一个值）——所以它在**卡片级**，绝不进任一方向栏：进了栏就等于宣称
 * 「这个方向的网络是 0.6 ms、那个方向不是」，而它们是同一条链路。
 *
 * 这一格与有会话时那个端到端总数是**两个量**，所以它在四个维度上都长得不一样，
 * 任何一个维度单独被看到都不会误读：
 *
 *   1. 标签不是「延迟」而是「网络单程」；
 *   2. 值自带「（仅网络）」后缀——被截图、被复制走时它跟着走；
 *   3. 旁边一枚 warn 色标记「不是总延迟」（与「未含对方主机」同一套语言）；
 *   4. 下面一行说清缺的是哪两段、以及为什么现在量不到。
 *
 * 缺一个都不行：只改标签，用户仍会把「0.6 ms」记成总延迟；只写进 title，鼠标不
 * 悬停就永远读不到。而这个数与真实感知延迟差三个数量级（0.58 ms vs 约 1000 ms），
 * 误读的代价不是「看起来好一点」，是完全相反的结论。
 */
function NetOnly({ fp, net }: { fp: string; net: PeerNetReading }) {
  const ms = net.ms;
  const known = typeof ms === 'number';
  // 读不到时是「测量中…」而**不是 0 ms**：min-RTT 还在攒样本，不是这条链路快到 0。
  const value = known
    ? t('metric.latency.netOnlyValue', { ms: segNum(ms) })
    : t('metric.latency.measuring');
  const title = joinPhrases([
    t('metric.latency.netOnlyWhy'),
    known ? null : t('metric.latency.netOnlyMeasuringWhy'),
    typeof net.rttMs === 'number'
      ? t('metric.latency.netOnlyRtt', { ms: segNum(net.rttMs) })
      : null,
  ]);
  return (
    <span className="metric-netonly" data-testid={`peer-netonly-${fp}`} title={title}>
      <span className="metric-cap">{t('metric.latency.netOnlyLabel')}</span>
      {/* 不上色阶：色阶是「这个延迟好不好用」的判断，而在一段网络时间上做那个
          判断本身就不成立——缓冲与声卡还一个数都没有。 */}
      <span className={`metric-val${known ? '' : ' unknown'}`} data-testid={`peer-netonly-value-${fp}`}>
        {value}
      </span>
      <span className="metric-scope">{t('metric.latency.netOnlyScope')}</span>
    </span>
  );
}

// ------------------------------------------------------------ 连通方式（tier）

/**
 * 降级链路的**归因条**。plan §16.4 的落点，也是这次改动的全部理由。
 *
 * ## 为什么它在这里，而不是卡片角上
 *
 * §16.4 第 1 条：降级 tier 是一级信息，**必须与延迟数字相邻**——「若用户需要移开
 * 视线、或需要点进二级页面才能把『慢』和『为什么慢』联系起来，本条即未被满足」。
 * 所以它是 `.peer-metrics` 的第一个孩子：紧接着它下面就是两个方向的延迟读数，
 * 因与果落在同一视野里。挂到卡片头部（主机名那一行）就已经不满足了——那一行
 * 与延迟数字之间隔着整个状态区。
 *
 * ## 为什么**一张卡只画一条**
 *
 * §16.1：降级是**每对端一个状态，不是每方向**。两个方向共用同一条连接，降级之后
 * 在物理上就是每对端的。给每个方向块各画一条，等于在界面上宣布「可以一个方向
 * Tier 0、另一个 Tier 1」——而那正是 daemon 侧那份契约明确要防止后人去实现的
 * 一句话。方向上的不对称（只有入向 UDP 被封）由原因文字承载，不由这里承载。
 *
 * ## 为什么 Tier 0 **不出现**
 *
 * §16.4 第 3 条：给每台正常对端挂一个「一切正常」的标记，只会训练用户忽略这个
 * 位置——而这里正是我们指望它在降级时被看见的地方。徽标的信息量来自稀有性，
 * 常驻会把它清零。
 *
 * ## 「未判定」为什么也不出现在**卡片**上
 *
 * 同一条理由：一张离线卡片上常驻一个灰色的「连通方式 —」是纯噪声。§16.4 第 5 条
 * 要求的是「未判定与 Tier 0 不得渲染成同一个样子」，而它的落点是**二级页面那一
 * 行状态**（`PeerTransport` 的 `detail-transport-now`）——那里 Tier 0 写「直连
 * （UDP）」、未判定写灰色的「—」，三态各有各的样子。卡片这一条只承担降级归因，
 * 它的缺席在两种情形下都不构成任何断言。
 */
function TierBanner({ fp, tier }: { fp: string; tier: EffectiveTier }) {
  return (
    <p
      className={`metric-tier ${tier}`}
      data-testid={`peer-tier-${fp}`}
      data-tier={tier}
      role="status"
      title={t('tier.now.title')}
    >
      <span className="metric-tier-label" data-testid={`peer-tier-label-${fp}`}>{t(TIER_LABEL[tier])}</span>
      {/* 后果那半句**必须看得见**，不能只进 title：不悬停鼠标的人拿到的仍然只有
          一个传输形态名词，而他要的是「所以呢」。措辞见 `TIER_WHY` 的注释——
          写的是「更容易卡顿」而不是「延迟更高」，两者对用户的含义不同。 */}
      <span className="metric-tier-why" data-testid={`peer-tier-why-${fp}`}>{t(TIER_WHY[tier])}</span>
    </p>
  );
}

// ---------------------------------------------------------------- 一个方向

/**
 * 一个方向的完整呈现：标题行（方向 + 延迟 + 音质）、流量行（电平 + 码率）、
 * 可展开的明细（该方向的分段 + 逐级 + 音质三分量 + 主导权说明）。
 *
 * ## 「未开通」与「读不到」必须长得不一样
 *
 * `data-empty` 那条等宽色带只表示「有会话、四段都还没读到」。**没有会话的方向
 * 根本不画色带**，整块塌成一行灰字。两态共用一个形状的话，双栏会把这个混淆放大
 * 成「左边有条右边没条，是坏了吗」。
 *
 * ## 为什么「未开通」的那一栏也要渲染
 *
 * 不做「只有一条时隐藏另一条」：隐藏会让用户失去「另一条没开」这条信息，
 * 而「没开」恰恰是他下一步可能要做的操作。
 */
function DirBlock({ fp, dir, list, open, onToggle, ready }: {
  fp: string;
  dir: Dir;
  list: SessionInfo[];
  open: boolean;
  onToggle: () => void;
  /** 仅接收方向传真：通路已就绪、只是还没有应用在用它。 */
  ready?: boolean;
}) {
  const sess = pickWorst(list);
  const series = useStore((s) => (sess ? s.history[String(sess.id)]?.latency : undefined) ?? EMPTY);
  const lat = readLatency(sess);
  const q = readQuality(sess);
  // `null` / 缺席保持原样交给 `fmt.kbps`（它画「—」）。**不折成 0**：
  // 「窗口还不够长」与「没有码率」在这一格上是两件事。
  const kbps = sess?.stats?.bitrate_kbps ?? undefined;
  const idleReady = !sess && !!ready;

  const dirLabel = t(dir === 'out' ? 'peers.card.streamOut' : 'peers.card.streamIn');
  // 方向语义 + 延迟档主导权。**这一句是把 Settings 里那条教训搬到卡片上**：
  // `servo_pass` 只遍历本机的接收流，发送方向的 jitter buffer 在对端、由对端
  // 自己的档位管。不说的话，一台只发不收的使用端拖了延迟滑条会看到「两栏里
  // 只有一栏在动」，唯一自然的结论是「设置只生效了一半」——而系统是对的。
  const govKey = dir === 'in' ? 'peers.card.dirGovLocal' : 'peers.card.dirGovPeer';

  // 这条流此刻在执行的**延迟目标**，以及它是谁定的。
  // `null` = AUTO（没有固定目标）⇒ 整行不出：一句「目标：自动」是噪声，
  // 而这一行存在的意义是解释一个**看起来异常大**的数字从哪来。
  const targetMs = sess?.stats?.latency_target;
  const fromPeer = sess?.stats?.target_from === 'peer';
  const targetText = typeof targetMs === 'string' && targetMs !== 'auto'
    ? t(fromPeer ? 'peers.card.targetByPeer' : 'peers.card.targetMine', { ms: targetMs })
    : '';

  // ## 没有会话时**两格指标照旧占位**（plan §14 裁定 2）
  //
  // 上一版在这里整块换成一行文字（「通路就绪 · 暂无应用在录音」），裁定 2
  // 逐字否掉了那种形态：
  //
  // > 麦克风未使用时，延迟与音质**仍然占位显示**，以灰色表示无数据。
  // > **不得**用「通路就绪 · 暂无应用在录音」这类纯文本把指标整块换掉——
  // > 那让用户无法在同一位置对比两个方向。
  //
  // 于是 `LatencyCell` / `QualityCell` 照旧渲染，只是喂 `undefined`：
  // 两者对空读数的既有行为就是「—」+ `.unknown` 暗色。**灰 ≠ 0**，这条红线
  // 由那两个组件自己保证（它们从不用 0 填补），这里不许绕过它们自己画一个数。
  //
  // 那句状态文字并没有丢，它降级成指标行下面的一行说明——状态是状态，
  // 数据是数据，两者不该互相顶替。
  if (!sess) {
    return (
      <div className={`dir-block idle${idleReady ? ' ready' : ''}`} data-dir={dir} data-testid={`metric-dir-${dir}-${fp}`}>
        <div className="dir-head idle">
          <span className="dir-name" title={t(govKey)}>
            <span className="dir-arrow" aria-hidden="true">{dir === 'out' ? '↑' : '↓'}</span>
            {dirLabel}
          </span>
          <LatencyCell fp={fp} dir={dir} lat={undefined} series={EMPTY} />
          <QualityCell fp={fp} dir={dir} q={undefined} />
        </div>
        <p className="metric-idle-text" data-testid={`peer-dir-idle-${dir}-${fp}`}>
          {idleReady ? t('peers.card.micReady') : t('peers.card.dirIdle')}
        </p>
        {/* 就绪那句的理由。`mic-idle-<fp>` 这个 testid 保持不变：它标的是
            「虚拟麦克风通了但没人用」这个**状态**，与分栏无关。 */}
        <p className="stream-ready" data-testid={`mic-idle-${fp}`} hidden={!idleReady}>
          {idleReady ? t('peers.card.micReadyWhy') : ''}
        </p>
      </div>
    );
  }

  return (
    <div className="dir-block active" data-dir={dir} data-testid={`metric-dir-${dir}-${fp}`}>
      <button
        type="button"
        className="dir-head"
        aria-expanded={open}
        aria-controls={`latency-detail-${dir}-${fp}`}
        aria-label={joinPhrases([
          dirLabel,
          t(open ? 'metric.latency.collapse' : 'metric.latency.expand'),
        ])}
        title={joinPhrases([t(govKey), t('metric.latency.footnote')])}
        onClick={(e) => { e.stopPropagation(); onToggle(); }}
      >
        <span className="dir-name">
          <span className="dir-arrow" aria-hidden="true">{dir === 'out' ? '↑' : '↓'}</span>
          {dirLabel}
        </span>
        {/* 同方向 N 条会话：把「一共几路、显示的是哪一路」说出来。不说的话，
            一个不标来源的数字背后站着 N 个候选——正是这次事故的形态。 */}
        <span
          className="dir-count"
          data-testid={`peer-dir-count-${dir}-${fp}`}
          hidden={list.length < 2}
          title={t('peers.card.dirMultiWhy')}
        >
          {list.length > 1 ? t('peers.card.dirMulti', { n: list.length }) : ''}
        </span>
        <LatencyCell fp={fp} dir={dir} lat={lat} series={series} />
        <QualityCell fp={fp} dir={dir} q={q} />
        <span className={`metric-chev${open ? ' open' : ''}`} aria-hidden="true" />
      </button>
      {/* plan §14 附：**用户看到 300 ms 时必须能分辨这是自己设定的目标**，
          而不是「系统只能做到这样」。当前界面对此一个字都没说，正是本次误判
          的直接成因。
          目标取自**这条流的执行器**（`SessionStats.latency_target`），不是全局
          设置——§15 之后全局设置根本不存在了，而拿别的流的目标来解释这一条，
          就是「一个数替另一条撒谎」换了个地方。
          `target_from` 让共享模式的机器说得出「这是对方要求的」。 */}
      <p className="metric-target" data-testid={`metric-target-${dir}-${fp}`} hidden={!targetText}>
        {targetText}
      </p>
      <div className="dir-stream" data-testid={`stream-${dir}-${fp}`}>
        {/* ⚠ 这条不是电平，是**码率除以 900**（`Meter` 只接一个标量）。所以它与
            右边那个 kbps 是同一个数的两种画法，不是「一件事的两个尺度」。
            别在文案里把它讲成电平——那会让用户以为静音时它会掉下去。 */}
        <Meter testid={`level-${dir}-${fp}`} value={(kbps || 0) / 900} />
        <span className="stream-rate">{t('peers.card.kbps', { v: fmt.kbps(kbps) })}</span>
      </div>
      {open ? (
        <div className="dir-detail" id={`latency-detail-${dir}-${fp}`} data-testid={`latency-detail-${dir}-${fp}`}>
          <LatencyBand fp={fp} dir={dir} lat={lat} />
          {LATENCY_STAGES.map((s) => (
            <StageRow key={s.id} fp={fp} dir={dir} spec={s} r={lat ? lat.stages[s.id] : undefined} side={lat?.side} />
          ))}
          {/* P1 的实测采样年龄。它与 Σ 各级是两条**独立**路径（墙钟差 vs 分级模型
              求和），差值就是上面那行「未归属」——所以只有它在场时那一行才可能有数。 */}
          <p className="metric-foot" data-testid={`latency-e2e-${dir}-${fp}`} hidden={typeof lat?.e2eMs !== 'number'}>
            {typeof lat?.e2eMs === 'number' ? t('latency.detail.e2e', { ms: fmt.int(lat.e2eMs) }) : ''}
          </p>
          <p
            className="metric-foot"
            data-testid={`latency-peer-stale-${dir}-${fp}`}
            hidden={!(lat && typeof lat.peerAgeS === 'number' && lat.peerAgeS > PEER_STALE_S)}
          >
            {lat && typeof lat.peerAgeS === 'number' && lat.peerAgeS > PEER_STALE_S
              ? t('latency.conf.peerStale', { s: fmt.int(lat.peerAgeS) })
              : ''}
          </p>
          <p className="metric-foot" data-testid={`latency-confidence-${dir}-${fp}`}>
            {/* convergingS 缺失时**不能兜底成 0**：「约 0 秒后可用」与事实正好相反。 */}
            {lat ? t(confidenceKey(lat.confidence), { s: fmt.int(lat.convergingS) }) : t('metric.latency.footnote')}
          </p>
          {/* 延迟档对这个方向到底有没有作用对象。一级界面上不说（那是噪声），
              但展开明细的人正是在排「我拖了滑条为什么没反应」这条障。 */}
          <p className="metric-foot gov" data-testid={`latency-gov-${dir}-${fp}`}>{t(govKey)}</p>
          <QualityParts fp={fp} dir={dir} q={q} />
        </div>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------- 指标区

/**
 * 两个方向各一块。分栏轴是 **`dir`（本机视角）**，不是 `kind`。
 *
 * `kind` 是**开流方**视角，`dir` 是本机视角。`progress.md:311` 记着这个坑已经
 * 栽过一次（「对端正在取用本机麦克风」被标成「取对方麦克风」），按 `kind` 分栏
 * 就是再栽一次。卡片底部原有的 `.peer-streams` 本来就是按 `dir` 分的——只是
 * 指标区没参与进去，这次把它整个吸收进来。
 *
 * 全卡**同一时刻至多一个展开面板**（`open` 是一个 `Dir | null`，不是两个布尔）：
 * `spec-telemetry-ia` §2.1 冻结了「卡片就地展开只承载分段明细」，双栏之后若给
 * 每个方向各留延迟 / 音质两个面板，一张卡会长出四个。
 */
export function PeerMetrics({ fp, peer, sendList, recvList, micReady }: {
  fp: string;
  peer: PeerState | null;
  /** 本机在**发**的会话（`dir === 'send'`），含共享模式的 `mic/send`。 */
  sendList: SessionInfo[];
  /** 本机在**收**的会话（`dir === 'recv'`），含共享模式的 `spk/recv`。 */
  recvList: SessionInfo[];
  micReady: boolean;
}) {
  const [open, setOpen] = useState<Dir | null>(null);
  const any = sendList.length > 0 || recvList.length > 0;
  // 链路**现状**（不是用户在详情页选的那一档）。判据只此一份，见 lib/tier.ts。
  // `daemon.status` 本来就是 5 秒一轮的既有轮询，这里不新增任何请求。
  const daemon = useStore((s) => s.daemon);
  const tier = effectiveTier(daemon, peer);

  // 整张卡片是可点的（进详情页），但指标区里的东西要**留在原地**：展开明细后想看清
  // 或框选某一级的 ms 数字，冒泡上去就会跳走，展开态一并丢失。
  const keep = (e: React.MouseEvent) => e.stopPropagation();

  // 无会话时唯一还活着的延迟读数：控制面的网络单程。**离线即 undefined**——
  // 记忆里的往返时间是关于过去的陈述，挂在一台离线主机上会被读成「它现在这么快」。
  //
  // 有会话时它已经作为 `network` 段进了色带，卡片级那格随即让位给方向块，避免重复。
  const net = any ? undefined : readPeerNet(peer);

  return (
    <div className={`peer-metrics${any ? '' : ' idle'}`} data-testid={`peer-metrics-${fp}`} onClick={keep}>
      {/* 降级归因条：**只在降级时出现**，且紧贴着下面两个延迟读数（§16.4 第 1、3 条）。
          Tier 0 与「未判定」都不画——理由分别见 `TierBanner` 的注释。 */}
      {isDegradedTier(tier) ? <TierBanner fp={fp} tier={tier} /> : null}
      <DirBlock
        fp={fp} dir="out" list={sendList}
        open={open === 'out'} onToggle={() => setOpen((v) => (v === 'out' ? null : 'out'))}
      />
      <DirBlock
        fp={fp} dir="in" list={recvList} ready={micReady}
        open={open === 'in'} onToggle={() => setOpen((v) => (v === 'in' ? null : 'in'))}
      />
      <div className="metric-netfoot" hidden={!net}>
        {net ? <NetOnly fp={fp} net={net} /> : null}
        {/* 缺的是哪两段、为什么现在量不到——这句必须**看得见**。只写进 title 的话，
            不悬停鼠标的人拿到的仍然是一个孤零零的毫秒数。 */}
        <p className="metric-foot" data-testid={`peer-netonly-note-${fp}`}>
          {net ? t('metric.latency.netOnlyNote') : ''}
        </p>
      </div>
    </div>
  );
}
