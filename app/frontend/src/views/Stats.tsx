// 统计诊断：延迟构成瀑布 + 每会话的数字块与 60 点迷你折线；daemon 运行时长；IPC 往返延迟。
//
// 分组从「会话导向」改为「**先按对端聚合、再按会话展开**」（规格 §2.5）。分工是明确的：
// 对端卡片回答「现在多少」，这一页回答「这一分钟怎么变的、大头一直在哪一段」。
// 会话号是 daemon 的内部计数，用户认得的是主机——所以默认按对端分组。

import { useState } from 'react';
import { Icon } from '../components/Icon';
import { Segmented, Spark } from '../components/Controls';
import { volumeText } from '../components/VolumeControl';
import { fmt, sessionFlow, dirLabel } from '../lib/fmt';
import { useTick } from '../lib/hooks';
import {
  LATENCY_STAGES, PARALLEL_TAILS, countedTail, coversWholeChain, isLowerBound,
  latencyValueKey, qualityDepthKey, readLatency, readQuality,
} from '../lib/metrics';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { useStore } from '../state/store';
import { isModeB } from '../state/mode';
import type { MetricHistory } from '../state/store';
import type { SessionInfo } from '../ipc/types';

const ORIGIN_TAG: Record<string, { cls: string; textKey: MsgKey; titleKey: MsgKey }> = {
  hal: { cls: 'tag accent', textKey: 'stats.origin.hal', titleKey: 'stats.origin.halTitle' },
  peer: { cls: 'tag warn', textKey: 'stats.origin.peer', titleKey: 'stats.origin.peerTitle' },
};

type GroupBy = 'peer' | 'session';

// 延迟与完整度排在最前：它们是用户真正会问的两件事，丢包/抖动/码率是解释它们的材料。
// 取值一律走 lib/metrics 的读取入口（读不到 ⇒ 显示「—」、折线不画）。
const METRICS: {
  key: keyof MetricHistory;
  labelKey: MsgKey;
  /** 单位另起一个 <span>；`fmtFull` 自带单位的行留空（见延迟）。 */
  unitKey?: MsgKey;
  val: (info: SessionInfo) => number | undefined;
  fmt: (v: unknown) => string;
  /** 整句渲染（值 + 单位一起由语料给）。只有延迟需要——「≥」在 DOM 拼接里没有位置。 */
  fmtFull?: (info: SessionInfo, v: number | undefined) => string;
  titleKey?: MsgKey;
  /** 随读数变化的 title（延迟：范围不足时要说清缺了谁）。优先于 titleKey。 */
  titleOf?: (info: SessionInfo) => string;
}[] = [
  {
    key: 'latency',
    labelKey: 'stats.metric.latency',
    titleKey: 'metric.latency.footnote',
    val: (info) => readLatency(info)?.totalMs,
    fmt: (v) => fmt.int(v),
    // 延迟是唯一一个「≥」承载语义的量（plan §7.6 补充裁定：P0 阶段恒带该前缀）。
    // 走既有的 数字 + <span class=unit> 拼接就没有任何位置能放它，落地后同一个数会
    // 在卡片上显示 ≥186 ms、在这里显示 186 ms——诊断页反而宣称了它没有的精度。
    // 判据走 latencyValueKey()，与对端卡片、下面的瀑布共用同一行代码。
    fmtFull: (info, v) => (typeof v === 'number'
      ? t(latencyValueKey(readLatency(info)), { ms: fmt.int(v) })
      : t('metric.latency.none')),
    // 这一页没有地方挂「未含对方主机」那枚标记（数字块的版式是 标签/数字/折线），
    // 于是它进 title。少了这句，同一个 localOnly 读数在卡片上明说「不是端到端」、
    // 在诊断页却只是一个孤零零的 ≥474 ms——两页给的结论不一样，而诊断页那页是错的。
    titleOf: (info) => {
      const lat = readLatency(info);
      return joinPhrases([
        lat && !coversWholeChain(lat) ? t('metric.latency.scopeLocalWhy') : null,
        lat && isLowerBound(lat) && coversWholeChain(lat) ? t('metric.latency.lowerBoundWhy') : null,
        t('metric.latency.footnote'),
      ]);
    },
  },
  {
    key: 'intact',
    labelKey: 'stats.metric.intact',
    unitKey: 'stats.unit.pct',
    titleKey: 'quality.part.continuity.desc',
    val: (info) => {
      const c = readQuality(info)?.concealPct;
      return typeof c === 'number' ? 100 - c : undefined;
    },
    // 这一格现在也可能来自**对端**的测量（纯发送的流本机量不到音质，
    // 见 `readQuality`）。数字本身是诚实的——连续性是「样本落地那一端」的
    // 属性——但**谁量的**必须说出来，否则页面就在不声不响地换信源。
    // 卡片上是一枚「对端测得」徽章，这里没有徽章的位置，就进 title。
    titleOf: (info) => {
      const q = readQuality(info);
      const base = t('quality.part.continuity.desc');
      return q?.fromPeer ? `${base}\n\n${t('metric.quality.fromPeerWhy')}` : base;
    },
    fmt: (v) => fmt.pct(v),
  },
  { key: 'loss', labelKey: 'stats.metric.loss', unitKey: 'stats.unit.pct', val: (i) => i.stats?.loss_pct, fmt: (v) => fmt.pct(v) },
  { key: 'jitter', labelKey: 'stats.metric.jitter', unitKey: 'stats.unit.ms', val: (i) => i.stats?.jitter_ms, fmt: (v) => fmt.ms(v) },
  // `?? undefined`：`bitrate_kbps` 是滑动窗口，窗口不够长时是 `null`。
  // 两者在这里都该画成「—」，而 `null` 不是本表的缺席表示。
  { key: 'bitrate', labelKey: 'stats.metric.bitrate', unitKey: 'stats.unit.kbps', val: (i) => i.stats?.bitrate_kbps ?? undefined, fmt: (v) => fmt.kbps(v) },
  {
    key: 'rung',
    labelKey: 'stats.metric.rung',
    unitKey: 'stats.unit.rung',
    val: (i) => i.stats?.rung,
    fmt: (v) => fmt.int(v),
    // 位深进阶梯之后**这个裸数字的含义静默换了**：改动前阶梯只有四档采样率，
    // AUTO 的稳态是 `0`；现在阶梯是六档 (采样率, 位深)，AUTO 的稳态是 `2`
    // （`AUTO_TOP_RUNG`），而 `0` 变成了 48 kHz/32 位浮点。同一个位置、同一个
    // 数字、不同的意思，界面上一个字都没说。所以把它对应的格式写进 title——
    // 拿的是这条流**实测**的两个维度，不是按格号反查一张前端自己的表。
    titleOf: (info) => {
      const k = qualityDepthKey(info.wire_depth);
      const hz = info.sample_rate;
      if (typeof hz !== 'number' || hz <= 0) return t('stats.metric.rungWhy');
      const f = k
        ? t('stats.meta.sampleRateDepth', { v: fmt.count(hz), depth: t(k) })
        : t('stats.meta.sampleRate', { v: fmt.count(hz) });
      return `${t('stats.metric.rungWhy')}\n\n${f}`;
    },
  },
];

function peerLabel(info: SessionInfo): string {
  return info.peer_name || String(info.peer_fingerprint || '').slice(0, 8);
}

/**
 * 会话头那一格的**线上格式**：采样率 + 位深，两个维度一起写。
 *
 * # 为什么这一格不能只写采样率
 *
 * 位深进阶梯之后 `线上 48000 Hz` 一句话对应阶梯上**三档**（48k/f32、48k/s24、
 * 48k/s16），码率从 768 一直到 1536 kbps。这是本轮改完之后界面上仅存的一处
 * 裸采样率读数——滑条标签、卡片一级格、详情页实测行三处都已经两维写全，
 * 唯独这一格漏了，等于把要消灭的那个歧义从滑条搬到了统计页。
 *
 * 三种态各有自己的写法，**一种都不能合并**：
 *   两个都读得到 ⇒ `线上 48000 Hz · 24 bit`
 *   只读得到速率 ⇒ `线上 48000 Hz`（旧 daemon 不发 `wire_depth`，**不猜 16 bit**）
 *   两个都读不到 ⇒ 「读不到」（**不显示 0 Hz、不兜底 48000**）
 */
function liveFormatPhrase(info: SessionInfo): string {
  const hz = info.sample_rate;
  if (typeof hz !== 'number' || hz <= 0) return t('stats.meta.sampleRateNone');
  const k = qualityDepthKey(info.wire_depth);
  if (!k) return t('stats.meta.sampleRate', { v: fmt.count(hz) });
  return t('stats.meta.sampleRateDepth', { v: fmt.count(hz), depth: t(k) });
}

// ---------------------------------------------------------------- 延迟瀑布

/**
 * 每通路一条 stacked bar，横轴 ms，按级配色。S1 阶段全部分项未知，所以每条都退化为
 * 一条中性底条——**不画出任何比例**，因为随便给一组比例就等于宣称测到了它们。
 */
function Waterfall({ sessions, hidden }: { sessions: SessionInfo[]; hidden: boolean }) {
  return (
    <section className="card block" data-testid="stats-waterfall" hidden={hidden}>
      <h3 className="block-title">{t('stats.waterfall.title')}</h3>
      <div className="waterfall" hidden={sessions.length === 0}>
        {sessions.map((info) => {
          const lat = readLatency(info);
          const known = !!lat && LATENCY_STAGES.some((s) => typeof lat.stages[s.id]?.ms === 'number');
          const total = lat?.totalMs;
          // 三条并行尾级只有最大的那条计入总数（同一帧同时进真实输出/桥/虚拟麦克风，
          // 时间上并联）。三条都画就会让条子之和大于它右边标的总数——同一行自相矛盾。
          // 另外两条不是没了，它们在卡片的逐级明细里各占一行。
          const tail = lat ? countedTail(lat.stages) : undefined;
          return (
            <div className="wf-row" key={info.id} data-testid={`wf-row-${info.id}`}>
              <span className="wf-label" title={sessionFlow(info).label}>
                {joinPhrases([peerLabel(info), sessionFlow(info).short])}
              </span>
              <div className="wf-bar" data-empty={known ? undefined : 'true'}>
                {LATENCY_STAGES.map((s) => {
                  const counted = !PARALLEL_TAILS.includes(s.id) || s.id === tail;
                  return (
                    <span
                      key={s.id}
                      className={`wf-seg wf-${s.segment || 'residual'}`}
                      data-testid={`wf-seg-${s.id}-${info.id}`}
                      title={t(s.nameKey)}
                      style={{
                        flexGrow: known && counted ? Math.max(0.001, lat?.stages[s.id]?.ms ?? 0) : 0,
                      }}
                    />
                  );
                })}
              </div>
              {/* 走 latencyValueKey() 而不是 latency.stage.ms：这里是**总数**，
                  与卡片头条是同一个量，必须带同一个「≥」。分项那一行才用 stage.ms。 */}
              <span
                className="wf-total"
                data-testid={`wf-total-${info.id}`}
                title={lat && !coversWholeChain(lat) ? t('metric.latency.scopeLocalWhy') : undefined}
              >
                {typeof total === 'number'
                  ? t(latencyValueKey(lat), { ms: fmt.int(total) })
                  : t('latency.stage.unknown')}
              </span>
            </div>
          );
        })}
      </div>
      <p className="muted small" data-testid="stats-waterfall-empty" hidden={sessions.length > 0}>
        {t('stats.waterfall.empty')}
      </p>
      <p className="muted small" hidden={sessions.length === 0}>{t('metric.latency.footnote')}</p>
    </section>
  );
}

// ------------------------------------------------------------ 降级链路诊断

/**
 * 降级链路（Tier 1 的专用 TCP / Tier 2 复用连接的媒体半边）的现场数字。
 *
 * ## 为什么它非在这一页不可
 *
 * design §5.2 第 4 条：`writeq_ms` 与 `stale_dropped` 是**解释「降级链路为什么
 * 难听」的唯一两个数字，别处看不到**。它们不属于任何一条会话——一条链路由这台
 * 对端的所有流共用——所以既进不了上面的会话卡，也进不了对端卡片那两栏。
 *
 * 怎么读（daemon 侧 `lib.rs` 那段注释的同一份账）：
 *   - `dropped` 涨   = 写线程卡的时间已经把队列灌满。
 *   - `stale_dropped` 涨 = 出队时帧已超过 200 ms 预算、被主动丢弃。**这不是一个
 *     新的丢包源**，是把 TCP 抹掉的丢包信号按需造回来，好让对端 JB 正确隐藏。
 *   - 两者都为 0 而对端仍在欠载 ⇒ 病不在这条链路的发送侧。
 *
 * ## 三态，一个都不能合并
 *
 * `tcp_media` 是数组且非空 ⇒ 逐条列出；
 * 是数组但为空 ⇒ 「没有对端在降级链路上」，这是**一条结论**（daemon 侧逐字：
 * 「空数组 = 没有任何对端在降级链路上，**不是**读不到」）；
 * 整个键缺席 ⇒ 「这一版服务不上报」，那才是读不到。把后两者合并成一句「无」，
 * 就是用一个缺席的字段去证明一切正常。
 */
function DegradedLinks() {
  const daemon = useStore((s) => s.daemon);
  const peers = useStore((s) => s.peers);
  const list = daemon?.latency_guard?.tcp_media;
  const muxes = daemon?.latency_guard?.mux;
  const supported = Array.isArray(list);

  function nameOf(fp: string | undefined): string {
    const p = fp ? peers.find((x) => x.fingerprint === fp) : undefined;
    return p?.display_name || p?.name || (fp ? fp.slice(0, 8) : t('common.dash'));
  }

  return (
    <section className="card block" data-testid="stats-degraded">
      <h3 className="block-title">{t('stats.degraded.title')}</h3>
      <p className="muted small" data-testid="stats-degraded-note">{t('stats.degraded.note')}</p>
      <div className="degraded-list" hidden={!supported || list!.length === 0}>
        {(supported ? list! : []).map((l, i) => {
          // Tier 2 的对端**同时**出现在两张表里（媒体半边在 tcp_media，控制帧计数
          // 在 mux）。所以档位判据是「在不在 mux 里」，不是「在不在 tcp_media 里」。
          const isMux = Array.isArray(muxes) && muxes.some((m) => m && m.fingerprint === l.fingerprint);
          const key = l.fingerprint || String(i);
          return (
            <div className="degraded-row" key={key} data-testid={`degraded-link-${key}`}>
              <div className="degraded-head">
                <strong>{nameOf(l.fingerprint)}</strong>
                <span className="tag warn" data-testid={`degraded-tier-${key}`}>
                  {t(isMux ? 'tier.now.tier2' : 'tier.now.tier1')}
                </span>
                <span className={`tag${l.alive === false ? ' danger' : ''}`} hidden={l.alive == null}>
                  {t(l.alive === false ? 'tier.now.linkDead' : 'tier.now.linkAlive')}
                </span>
                <code className="mono dim">{l.peer || t('common.dash')}</code>
              </div>
              {/* 两个头条数字用 metric 版式（与会话卡同一套），其余计数走脚注行。
                  一律 `fmt.int` / `fmt.decimal1`：读不到画「—」，**不折成 0**。 */}
              <div className="metrics">
                <div className="metric">
                  <div className="metric-label">{t('stats.degraded.writeq')}</div>
                  <div
                    className="metric-num"
                    data-testid={`degraded-writeq-${key}`}
                    title={t('stats.degraded.writeqWhy')}
                  >
                    {fmt.decimal1(l.writeq_ms)}
                    <span className="unit">{t('stats.unit.ms')}</span>
                  </div>
                </div>
                <div className="metric">
                  <div className="metric-label">{t('stats.degraded.stale')}</div>
                  <div
                    className="metric-num"
                    data-testid={`degraded-stale-${key}`}
                    title={t('stats.degraded.staleWhy')}
                  >
                    {fmt.int(l.stale_dropped)}
                  </div>
                </div>
              </div>
              <footer className="session-extra" data-testid={`degraded-extra-${key}`}>
                <span>{t('stats.degraded.writeqPeak', { v: fmt.decimal1(l.writeq_peak_ms) })}</span>
                <span>{t('stats.degraded.writeqAuto', { v: fmt.decimal1(l.writeq_auto_ms) })}</span>
                <span>{t('stats.degraded.queued', { n: fmt.int(l.queued), cap: fmt.int(l.capacity) })}</span>
                <span>{t('stats.degraded.dropped', { n: fmt.int(l.dropped) })}</span>
                <span>{t('stats.degraded.frames', { w: fmt.int(l.frames_written), r: fmt.int(l.frames_read) })}</span>
                {typeof l.unexpected_kind === 'number' && l.unexpected_kind > 0
                  ? (
                    <span className="tag danger">
                      {t('stats.degraded.unexpected', { n: fmt.int(l.unexpected_kind) })}
                    </span>
                  )
                  : null}
              </footer>
            </div>
          );
        })}
      </div>
      <p className="muted small" data-testid="stats-degraded-empty" hidden={!supported || list!.length > 0}>
        {t('stats.degraded.empty')}
      </p>
      <p className="muted small" data-testid="stats-degraded-unsupported" hidden={supported}>
        {t('stats.degraded.unsupported')}
      </p>
    </section>
  );
}

// ---------------------------------------------------------------- 会话卡

function SessionCard({ info, hist }: { info: SessionInfo; hist: MetricHistory | undefined }) {
  const st = info.stats || {};
  const flow = sessionFlow(info);
  const origin = info.origin ? ORIGIN_TAG[info.origin] : undefined;
  const vol = volumeText(st.volume);
  // 抖动缓冲这一级的 ms，由 daemon 算好（`jitter_buf`）。用来给下面那格帧数配一个
  // 与延迟档同量纲的读数；读不到就不配（不拿帧数 ×10 编一个）。
  const jbMs = readLatency(info)?.stages?.jitter_buf?.ms;

  return (
    <article className="card session-card" data-testid={`session-row-${info.id}`}>
      <header className="session-head">
        <strong>{t('stats.session', { id: info.id })}</strong>
        <span className="tag accent" title={flow.label} data-testid={`session-flow-${info.id}`}>{flow.short}</span>
        <span className="tag">{dirLabel(info.dir)}</span>
        {/* origin=hal 的会话必须写出是**哪一台**虚拟设备触发的：模式 B 下这是用户唯一
            能把「统计页这一行」和「我刚在系统里选的那台设备」对上的线索。 */}
        {origin
          ? (
            <span
              className={origin.cls}
              title={info.hal_device || t(origin.titleKey)}
              data-testid={`session-origin-${info.id}`}
            >
              {t(origin.textKey)}
            </span>
          )
          : (flow.inbound ? <span className="tag warn">{t('session.tag.peerInitiated')}</span> : null)}
        <span className="sess-device" data-testid={`session-device-${info.id}`} hidden={!info.hal_device}>
          {info.hal_device || ''}
        </span>
        <span className="sess-meta">
          {joinPhrases([
            peerLabel(info),
            // `sample_rate` 现在是**线上**速率（随质量档变），不再是硬编码的
            // 48000。0 = 两侧都报不出来 ⇒ 说「读不到」，**不显示 0 Hz、也不兜底
            // 成 48000**——那个兜底正是这一格此前恒写 48000 的来路。
            //
            // 位深与它**成对**：`线上 48000 Hz` 一句话对应阶梯上三档
            // （f32 / s24 / s16，码率差到 2 倍）。位深读不到就退回只写采样率的
            // 那一条——**不猜 16 bit**（滑条与卡片两处同一条规矩）。
            liveFormatPhrase(info),
            t('stats.meta.channels', { v: fmt.count(info.channels) }),
          ])}
        </span>
      </header>

      <div className="metrics">
        {METRICS.map((m) => (
          <div className="metric" key={m.key}>
            <div className="metric-label">{t(m.labelKey)}</div>
            <div
              className="metric-num"
              data-testid={`stat-${m.key}-${info.id}`}
              title={m.titleOf ? m.titleOf(info) : m.titleKey ? t(m.titleKey) : undefined}
            >
              {m.fmtFull ? m.fmtFull(info, m.val(info)) : m.fmt(m.val(info))}
              <span className="unit" hidden={!m.unitKey}>{m.unitKey ? t(m.unitKey) : ''}</span>
            </div>
            <Spark testid={`spark-${m.key}-${info.id}`} points={hist ? hist[m.key] : []} />
          </div>
        ))}
      </div>

      {/* 音量同步回显（只读）：spk 会话两侧都有 stats.volume——dir=send 时那是
          对端的真实输出设备，dir=recv 时那是本机自己的。调节入口在对端卡片上。 */}
      <div
        className={`vol-readout${vol && vol.muted ? ' muted' : ''}`}
        hidden={!vol}
        data-testid={`session-volume-${info.id}`}
      >
        <Icon name="spk" />
        <span className="vol-readout-label">
          {info.dir === 'recv' ? t('stats.vol.local') : t('stats.vol.remote')}
        </span>
        <div className="vol-bar">
          <div
            className="vol-bar-fill"
            style={{ transform: `scaleX(${vol ? (vol.scalarPct / 100).toFixed(3) : '0'})` }}
          />
        </div>
        <span className="vol-val">{vol ? vol.text : t('common.dash')}</span>
        <span className="tag warn" hidden={!vol || vol.adjustable}>{t('volume.notAdjustable.tag')}</span>
      </div>

      <footer className="session-extra" data-testid={`session-extra-${info.id}`}>
        <span>{t('stats.extra.received', { n: fmt.count(st.received) })}</span>
        <span>{t('stats.extra.lost', { n: fmt.count(st.lost) })}</span>
        <span>{t('stats.extra.sent', { n: fmt.count(st.sent_packets) })}</span>
        {/* 缓冲深度**同时给帧与毫秒**，理由与音质那一格的「带宽（采样率 …）」逐字
            相同：延迟档的设置单位是 ms，而这一格此前只有帧，用户设了 300 ms 之后
            没有任何办法把「12 帧」与它对上。
            ms **不由帧数 ×10 推**——那是把 FRAME_MS 这个后端常数刻一份在前端。
            取的是 daemon 已经算好的 `jitter_buf` 级读数（同一条会话、同一拍）；
            它拿不到时就只显示帧数，不编一个毫秒出来。 */}
        <span>
          {typeof jbMs === 'number'
            ? t('stats.extra.jbDepthMs', { n: fmt.count(st.jb_depth_frames), ms: fmt.int(jbMs) })
            : t('stats.extra.jbDepth', { n: fmt.count(st.jb_depth_frames) })}
        </span>
        <span>{t('stats.extra.rungChanges', { n: fmt.count(st.rung_changes) })}</span>
        {/* ---- 两个**静默降级**计数器：非零才显示，且非零就是坏消息 ----------
            两个都是「JB 的五个计数器全部一片正常，而声音已经坏了」的那类故障，
            所以它们必须有自己的位置——挂在别人身上就等于没有。
            恒显示会让两个恒为 0 的数占住这一行的位置并训练用户忽略它们；
            所以按 `> 0` 显形，并用 danger/warn 的语气，因为非零没有良性解释。 */}
        {typeof st.jb_half_conceal === 'number' && st.jb_half_conceal > 0
          ? (
            <span className="tag warn" title={t('stats.extra.halfConcealWhy')}>
              {t('stats.extra.halfConceal', { n: fmt.count(st.jb_half_conceal) })}
            </span>
          )
          : null}
        {typeof st.format_mismatch === 'number' && st.format_mismatch > 0
          ? (
            <span className="tag danger" title={t('stats.extra.formatMismatchWhy')}>
              {t('stats.extra.formatMismatch', { n: fmt.count(st.format_mismatch) })}
            </span>
          )
          : null}
        {st.verdict
          ? (st.verdict.detected
            ? <span className="tag ok">{t('stats.extra.verdictPass', { snr: fmt.decimal1(st.verdict.snr_db) })}</span>
            : <span className="tag danger">{t('stats.extra.verdictFail')}</span>)
          : null}
        {Array.isArray(st.mix_verdicts) && st.mix_verdicts.length
          ? <span className="tag">{t('stats.extra.mixProbes', { n: fmt.count(st.mix_verdicts.length) })}</span>
          : null}
      </footer>
    </article>
  );
}

// ---------------------------------------------------------------- 对端分组

function PeerGroup({ fp, name, list, history }: {
  fp: string; name: string; list: SessionInfo[]; history: Record<string, MetricHistory>;
}) {
  const [open, setOpen] = useState(true);
  return (
    <section className="peer-group" data-testid={`stats-group-${fp}`}>
      <button
        type="button"
        className="peer-group-head"
        data-testid={`stats-group-toggle-${fp}`}
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className={`metric-chev${open ? ' open' : ''}`} aria-hidden="true" />
        <strong className="peer-group-name">{name}</strong>
        <span className="peer-group-count">{t('stats.group.sessions', { n: list.length })}</span>
      </button>
      <div className="peer-group-body" hidden={!open}>
        {list.map((info) => (
          <SessionCard key={info.id} info={info} hist={history[String(info.id)]} />
        ))}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------- 视图

export function StatsView() {
  const sessions = useStore((s) => s.sessions);
  const history = useStore((s) => s.history);
  const daemon = useStore((s) => s.daemon);
  const lastStatusAt = useStore((s) => s.lastStatusAt);
  const rtt = useStore((s) => s.ipcRttMs);
  const modeB = useStore(isModeB);
  const [groupBy, setGroupBy] = useState<GroupBy>('peer');
  useTick(1000);

  const uptime = daemon && lastStatusAt
    ? fmt.uptime((daemon.uptime_s ?? 0) + (Date.now() - lastStatusAt) / 1000)
    : t('common.dash');

  // 保持 daemon 给的会话顺序，只按首次出现的对端归堆——重排会让 1Hz 的刷新看起来在跳。
  const groups: { fp: string; name: string; list: SessionInfo[] }[] = [];
  for (const info of sessions) {
    const fp = String(info.peer_fingerprint || '');
    let g = groups.find((x) => x.fp === fp);
    if (!g) {
      g = { fp, name: peerLabel(info), list: [] };
      groups.push(g);
    }
    g.list.push(info);
  }

  return (
    <>
      <div className="tile-row">
        <div className="card tile">
          <div className="tile-label">{t('stats.uptime')}</div>
          <div className="tile-num" data-testid="stats-uptime">{uptime}</div>
        </div>
        <div className="card tile">
          <div className="tile-label">{t('stats.rtt')}</div>
          <div className="tile-num" data-testid="stats-rtt">
            {typeof rtt === 'number' ? t('stats.rttValue', { v: fmt.decimal1(rtt) }) : t('common.dash')}
          </div>
        </div>
        <div className="card tile">
          <div className="tile-label">{t('stats.sessionCount')}</div>
          <div className="tile-num" data-testid="stats-session-count">{fmt.count(sessions.length)}</div>
        </div>
      </div>

      {/* 零会话时整块收起：它自己的空态（stats.waterfall.empty）与页面底部那张
          stats-empty 卡说的是同一件事，两条空态叠在一起只会让人以为出了两个问题。
          DOM 与 testid 都保留，只是 hidden。 */}
      <Waterfall sessions={sessions} hidden={sessions.length === 0} />

      {/* 降级链路的两个数字（design §5.2 第 4 条）。**不随会话数收起**：一条链路
          可以在没有任何会话时仍然存在并积压，而那正是需要被看见的时刻。 */}
      <DegradedLinks />

      {/* 工具条只留分段选择器：这里原先复用了「活跃会话」那条 key 当标签，可上面
          已经有一张带数字的同名 tile，而这里既没数字也不说明分组维度——正是本次
          要消灭的那类重复内容。Segmented 自己已经写着「按对端 / 按会话」。 */}
      <div className="toolbar" hidden={sessions.length === 0}>
        <Segmented<GroupBy>
          testid="stats-group-by"
          value={groupBy}
          onSelect={setGroupBy}
          options={[
            { value: 'peer', label: t('stats.groupBy.peer') },
            { value: 'session', label: t('stats.groupBy.session') },
          ]}
        />
      </div>

      <div className="session-list">
        {groupBy === 'peer'
          ? groups.map((g) => (
            <PeerGroup key={g.fp} fp={g.fp} name={g.name} list={g.list} history={history} />
          ))
          : sessions.map((info) => (
            <SessionCard key={info.id} info={info} hist={history[String(info.id)]} />
          ))}
      </div>

      {/* 空态文案要按模式分开：模式 B 下「去打开卡片上的开关」是一句错误的指引——
          那排开关根本不存在，会话由系统的设备选择创建。 */}
      <div className="empty card" data-testid="stats-empty" hidden={sessions.length > 0}>
        <Icon name="stats" cls="empty-ico" />
        <h3>{t('stats.empty.title')}</h3>
        <p data-testid="stats-empty-hint">
          {modeB ? t('stats.empty.hintModeB') : t('stats.empty.hintModeA')}
        </p>
      </div>
    </>
  );
}
