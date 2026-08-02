// 对端卡片的一级指标区：延迟 + 音质，以及可就地展开的分段明细。
//
// 为什么是「就地展开」而不是跳诊断页（规格 §2.3）：排障时人要**边听边看数字变**，
// 跳页会打断这个循环。诊断页回答的是另一个问题——「这一分钟怎么变的、大头一直在
// 哪一段」，那是时间序列，不是此刻的快照。
//
// 读数一律走 lib/metrics 的两个读取入口；缺失即「—」，绝不用 0 填补
//（见 lib/metrics.ts 顶部的红线）。

import { useState } from 'react';
import { fmt } from '../lib/fmt';
import { t, joinPhrases } from '../i18n';
import { stageChips } from '../lib/stagefacts';
import {
  LATENCY_SEGMENTS, LATENCY_STAGES, SEGMENT_LABEL, QUALITY_DOTS, QUALITY_PARTS,
  QUALITY_PART_DESC, QUALITY_PART_NAME,
  confidenceKey, coversWholeChain, isLowerBound, isQualityMeasuring, latencyGrade,
  latencyGradeKey, latencyTone, latencyValueKey, medianOf5, qualityDots,
  qualityGradeTextKey, qualityTone, qualityWorstKey,
  readLatency, readQuality, stageHost,
} from '../lib/metrics';
import type { LatencyReading, QualityReading, StageReading, StageSpec } from '../lib/metrics';
import { useStore } from '../state/store';
import type { SessionInfo } from '../ipc/types';

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
function LatencyCell({ fp, lat, series, open, onToggle }: {
  fp: string; lat: LatencyReading | undefined; series: number[];
  open: boolean; onToggle: () => void;
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

  let value: string;
  if (typeof ms === 'number') value = t(latencyValueKey(lat), { ms: fmt.int(ms) });
  else if (lat && lat.confidence === 'converging') value = t('metric.latency.measuring');
  else if (lat && lat.confidence === 'unavailable') value = t('metric.latency.unsupported');
  else value = t('metric.latency.none');

  // 「≥」的理由必须能被读到，否则它看起来只是个装饰符号（plan §7.6 补充裁定）。
  const title = joinPhrases([
    scope ? t('metric.latency.scopeLocalWhy') : null,
    lower && whole ? t('metric.latency.lowerBoundWhy') : null,
    lat && lat.deviceUnreliable ? t('latency.conf.deviceUnreliable') : null,
    t('metric.latency.footnote'),
  ]);

  // aria-label **替换**整个可访问名，所以它必须把值一起带上：只写「查看分段」会让
  // 读屏用户永远听不到这次改版唯一新增的一级信息。范围标记同样进名字——对读屏
  // 用户来说，「这不是端到端」是比等级词更要紧的一句。展开态由 aria-expanded 表达。
  return (
    <button
      type="button"
      className="metric-cell"
      data-testid={`metric-latency-${fp}`}
      aria-expanded={open}
      aria-controls={`latency-detail-${fp}`}
      aria-label={joinPhrases([
        t('metric.latency.label'),
        value,
        grade ? t(latencyGradeKey(grade)) : null,
        scope || null,
        t(open ? 'metric.latency.collapse' : 'metric.latency.expand'),
      ])}
      title={title}
      onClick={(e) => { e.stopPropagation(); onToggle(); }}
    >
      <span className="metric-cap">{t('metric.latency.label')}</span>
      {/* 没有等级就没有色阶：给一个只覆盖半条链路的数字上色，等于替它做了那个
          不成立的端到端判断。此时用正文色（既不是 tone-*，也不是「读不到」的暗色）。 */}
      <span
        className={`metric-val${grade ? ` tone-${latencyTone(grade)}` : typeof ms === 'number' ? '' : ' unknown'}`}
        data-testid={`metric-latency-value-${fp}`}
      >
        {value}
      </span>
      <span className="metric-grade" data-testid={`metric-latency-grade-${fp}`} hidden={!grade}>
        {grade ? t(latencyGradeKey(grade)) : ''}
      </span>
      <span className="metric-scope" data-testid={`metric-latency-scope-${fp}`} hidden={!scope}>
        {scope}
      </span>
      <span className={`metric-chev${open ? ' open' : ''}`} aria-hidden="true" />
    </button>
  );
}

function LatencyBand({ fp, lat }: { fp: string; lat: LatencyReading | undefined }) {
  const vals = LATENCY_SEGMENTS.map((id) => (lat ? lat.segments[id] : undefined));
  const known = vals.some((v) => typeof v === 'number');

  return (
    <>
      <div className="metric-band" data-testid={`latency-band-${fp}`} data-empty={known ? undefined : 'true'}>
        {LATENCY_SEGMENTS.map((id, i) => (
          <span
            key={id}
            className={`band-seg band-${id}`}
            data-testid={`latency-band-${id}-${fp}`}
            // 未知时四段等宽：那是「还没测到」的形状，不是「四段一样长」的结论。
            style={{ flexGrow: known ? Math.max(0.001, vals[i] ?? 0) : 1 }}
          />
        ))}
      </div>
      <div className="metric-segs" data-testid={`latency-segs-${fp}`}>
        {LATENCY_SEGMENTS.map((id, i) => {
          const v = vals[i];
          return (
            <span
              key={id}
              className="seg-item"
              data-testid={`latency-seg-${id}-${fp}`}
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
function StageRow({ fp, spec, r, side }: {
  fp: string; spec: StageSpec; r: StageReading | undefined; side: 'send' | 'recv' | undefined;
}) {
  const host = stageHost(spec, r, side);
  const chips = stageChips(r);
  return (
    <div className="stage-item" data-testid={`latency-stage-${spec.id}-${fp}`}>
      <div className="stage-row">
        <span className="stage-name" title={t(spec.descKey)}>{t(spec.nameKey)}</span>
        <span className="stage-host" hidden={!host}>
          {host === 'peer' ? t('latency.stage.onPeer') : host === 'local' ? t('latency.stage.onLocal') : ''}
        </span>
        <span className={`stage-ms${r && typeof r.ms === 'number' ? '' : ' unknown'}`}>{stageText(r)}</span>
      </div>
      <div className="stage-facts" hidden={chips.length === 0} data-testid={`latency-stage-facts-${spec.id}-${fp}`}>
        {chips.map((c) => (
          <span
            key={c.id}
            className={`stage-chip${c.warn ? ' warn' : ''}`}
            data-testid={`latency-stage-${c.id}-${spec.id}-${fp}`}
            title={c.title}
          >
            {c.text}
          </span>
        ))}
      </div>
    </div>
  );
}

function LatencyDetail({ fp, lat }: { fp: string; lat: LatencyReading | undefined }) {
  const stale = lat && typeof lat.peerAgeS === 'number' && lat.peerAgeS > PEER_STALE_S
    ? lat.peerAgeS
    : undefined;
  return (
    <div className="metric-detail" id={`latency-detail-${fp}`} data-testid={`latency-detail-${fp}`}>
      {LATENCY_STAGES.map((s) => (
        <StageRow key={s.id} fp={fp} spec={s} r={lat ? lat.stages[s.id] : undefined} side={lat?.side} />
      ))}
      {/* P1 的实测采样年龄。它与 Σ 各级是两条**独立**路径（墙钟差 vs 分级模型求和），
          差值就是上面那行「未归属」——所以只有它在场时那一行才可能有数。 */}
      <p className="metric-foot" data-testid={`latency-e2e-${fp}`} hidden={typeof lat?.e2eMs !== 'number'}>
        {typeof lat?.e2eMs === 'number' ? t('latency.detail.e2e', { ms: fmt.int(lat.e2eMs) }) : ''}
      </p>
      <p className="metric-foot" data-testid={`latency-peer-stale-${fp}`} hidden={stale === undefined}>
        {stale === undefined ? '' : t('latency.conf.peerStale', { s: fmt.int(stale) })}
      </p>
      <p className="metric-foot" data-testid={`latency-confidence-${fp}`}>
        {/* convergingS 缺失时**不能兜底成 0**：「约 0 秒后可用」与事实正好相反。
            fmt.int(undefined) 自己会出「—」，读作「约 — 秒后可用」——难看，但不撒谎。 */}
        {lat
          ? t(confidenceKey(lat.confidence), { s: fmt.int(lat.convergingS) })
          : t('metric.latency.footnote')}
      </p>
    </div>
  );
}

// ---------------------------------------------------------------- 音质

function QualityCell({ fp, q, open, onToggle }: {
  fp: string; q: QualityReading | undefined; open: boolean; onToggle: () => void;
}) {
  const grade = q ? q.grade : undefined;
  const dots = qualityDots(grade);
  const khz = q ? q.bandwidthKhz : undefined;
  const value = typeof khz === 'number'
    ? t('metric.quality.bandwidth', { khz: fmt.int(khz) })
    : t('metric.quality.none');

  // 三态判定（有等级 / 有读数但等级不成立 / 什么都没有）在 lib/metrics 里，那样它
  // 可以被回归断言——「某个状态压根没被渲染」是类型检查看不见的那类缺陷。
  const measuring = isQualityMeasuring(q);
  const gradeKey = qualityGradeTextKey(q);
  const gradeText = gradeKey ? t(gradeKey) : '';
  const title = joinPhrases([
    measuring ? t('metric.quality.measuringWhy') : null,
    grade && q?.partial ? t('metric.quality.partial') : null,
  ]);

  // 与延迟格同理：可访问名要带上值，「●●●○」是 aria-hidden 的装饰，等级词是它的文字对应物。
  return (
    <button
      type="button"
      className="metric-cell"
      data-testid={`metric-quality-${fp}`}
      aria-expanded={open}
      aria-controls={`quality-detail-${fp}`}
      aria-label={joinPhrases([
        t('metric.quality.label'),
        value,
        gradeText || null,
        t(open ? 'metric.quality.collapse' : 'metric.quality.expand'),
      ])}
      title={title || undefined}
      onClick={(e) => { e.stopPropagation(); onToggle(); }}
    >
      <span className="metric-cap">{t('metric.quality.label')}</span>
      <span
        className="quality-dots"
        data-testid={`metric-quality-dots-${fp}`}
        data-state={measuring ? 'measuring' : undefined}
        aria-hidden="true"
      >
        {Array.from({ length: QUALITY_DOTS }, (_, i) => (
          <span key={i} className={`qdot${i < dots ? ` on tone-${grade ? qualityTone(grade) : 'ok'}` : ''}`} />
        ))}
      </span>
      {/* 与延迟那格同一条规矩：`.unknown` 的暗色只表示**读不到**。带宽在「测量中」
          这一态里是**已经测出来的真读数**，把它调暗等于说它也没测到——而那正是
          这次要修的那种「让不知道和坏消息长得一样」的呈现。 */}
      <span
        className={`metric-val${grade ? ` tone-${qualityTone(grade)}` : typeof khz === 'number' ? '' : ' unknown'}`}
        data-testid={`metric-quality-value-${fp}`}
      >
        {value}
      </span>
      <span
        className={`metric-grade${measuring ? ' measuring' : ''}`}
        data-testid={`metric-quality-grade-${fp}`}
        hidden={!gradeText}
      >
        {gradeText}
      </span>
      <span className={`metric-chev${open ? ' open' : ''}`} aria-hidden="true" />
    </button>
  );
}

function QualityDetail({ fp, q }: { fp: string; q: QualityReading | undefined }) {
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
    return typeof q.bandwidthKhz === 'number'
      ? t('quality.part.bandwidth.value', { khz: fmt.int(q.bandwidthKhz) })
      : t('metric.quality.none');
  }

  return (
    <div className="metric-detail" id={`quality-detail-${fp}`} data-testid={`quality-detail-${fp}`}>
      {QUALITY_PARTS.map((id) => (
        <div key={id} className="stage-row" data-testid={`quality-part-${id}-${fp}`}>
          <span className="stage-name" title={t(QUALITY_PART_DESC[id])}>{t(QUALITY_PART_NAME[id])}</span>
          <span className="stage-note" hidden={q?.worst !== id}>
            {q?.worst === id ? t(qualityWorstKey(id)) : ''}
          </span>
          <span className={`stage-ms${q ? '' : ' unknown'}`}>{partValue(id)}</span>
        </div>
      ))}
      {/* 等级已经触底时，缺一块板改不了结论——于是 grade 有值而 partial 仍为真。
          这两件事都要说：结论成立，但它是在缺一项的情况下得出的（只会更低）。
          等级根本不成立时这行不出，那个状态由格子里的「测量中…」承担。 */}
      <p className="metric-foot" data-testid={`quality-partial-${fp}`} hidden={!(q?.grade && q?.partial)}>
        {q?.grade && q?.partial ? t('metric.quality.partial') : ''}
      </p>
      {/* 窗口长度**没有兜底值**：`?? 10` 会让一个什么都没测的窗口宣称「最近 10 秒」，
          和用 0 填补缺失分项是同一类错误，只是 10 更难被发现。读不到就整行不出。 */}
      <p
        className="metric-foot"
        data-testid={`quality-window-${fp}`}
        hidden={typeof q?.windowS !== 'number'}
      >
        {typeof q?.windowS === 'number' ? t('quality.part.window', { s: fmt.int(q.windowS) }) : ''}
      </p>
    </div>
  );
}

// ---------------------------------------------------------------- 指标区

/**
 * `sess` 是这张卡上用来读指标的那条通路：优先「取对方麦克风」（recv），否则
 * 「送对方扬声器」（send）。取 recv 优先是因为延迟的物理定义就是这一条——从对方
 * 声卡采到、到本机声卡送出（规格 §3.2）。
 *
 * 无会话时整块塌成一行，但**卡片高度不变**（CSS 上有 min-height）：一排卡片里
 * 只有开了通路的那几张变高，扫一眼时像是排版坏了。
 */
export function PeerMetrics({ fp, sess }: { fp: string; sess: SessionInfo | null }) {
  const [open, setOpen] = useState<'latency' | 'quality' | null>(null);
  // 头条数字要平滑（规格 §2.6），而平滑要历史序列——store 里已经有一条 60 点的，
  // 由 pushStats 每秒推一点，缺读数时原地不动（所以序列里不会混进 0）。
  // `EMPTY` 是模块级常量：每次渲染新造一个 `[]` 会让选择器的引用每帧都变。
  const series = useStore((s) => (sess ? s.history[String(sess.id)]?.latency : undefined) ?? EMPTY);
  const lat = readLatency(sess);
  const q = readQuality(sess);
  const sep = t('common.bullet');

  // 整张卡片是可点的（进详情页），但指标区里的东西要**留在原地**：展开明细后想看清
  // 或框选某一级的 ms 数字，冒泡上去就会跳走，展开态一并丢失。设备区（Peers.tsx）
  // 早就防住了同一类问题，指标区不能漏。
  const keep = (e: React.MouseEvent) => e.stopPropagation();

  if (!sess) {
    return (
      <div className="peer-metrics idle" data-testid={`peer-metrics-${fp}`} onClick={keep}>
        <div className="metric-idle">
          <span className="metric-cap">{t('metric.latency.label')}</span>
          <span className="metric-val unknown">{t('metric.latency.none')}</span>
          <span className="metric-sep" aria-hidden="true">{sep}</span>
          <span className="metric-cap">{t('metric.quality.label')}</span>
          <span className="metric-val unknown">{t('metric.quality.none')}</span>
          <span className="metric-sep" aria-hidden="true">{sep}</span>
          <span className="metric-idle-text" data-testid={`peer-no-session-${fp}`}>
            {t('peers.card.noSession')}
          </span>
        </div>
      </div>
    );
  }

  return (
    <div className="peer-metrics" data-testid={`peer-metrics-${fp}`} onClick={keep}>
      <div className="metric-line">
        <LatencyCell
          fp={fp} lat={lat} series={series} open={open === 'latency'}
          onToggle={() => setOpen((v) => (v === 'latency' ? null : 'latency'))}
        />
        <QualityCell
          fp={fp} q={q} open={open === 'quality'}
          onToggle={() => setOpen((v) => (v === 'quality' ? null : 'quality'))}
        />
      </div>
      <LatencyBand fp={fp} lat={lat} />
      {open === 'latency' ? <LatencyDetail fp={fp} lat={lat} /> : null}
      {open === 'quality' ? <QualityDetail fp={fp} q={q} /> : null}
    </div>
  );
}
