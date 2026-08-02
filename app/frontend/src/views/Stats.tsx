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
  latencyValueKey, readLatency, readQuality,
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
    fmt: (v) => fmt.pct(v),
  },
  { key: 'loss', labelKey: 'stats.metric.loss', unitKey: 'stats.unit.pct', val: (i) => i.stats?.loss_pct, fmt: (v) => fmt.pct(v) },
  { key: 'jitter', labelKey: 'stats.metric.jitter', unitKey: 'stats.unit.ms', val: (i) => i.stats?.jitter_ms, fmt: (v) => fmt.ms(v) },
  { key: 'bitrate', labelKey: 'stats.metric.bitrate', unitKey: 'stats.unit.kbps', val: (i) => i.stats?.bitrate_kbps, fmt: (v) => fmt.kbps(v) },
  { key: 'rung', labelKey: 'stats.metric.rung', unitKey: 'stats.unit.rung', val: (i) => i.stats?.rung, fmt: (v) => fmt.int(v) },
];

function peerLabel(info: SessionInfo): string {
  return info.peer_name || String(info.peer_fingerprint || '').slice(0, 8);
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

// ---------------------------------------------------------------- 会话卡

function SessionCard({ info, hist }: { info: SessionInfo; hist: MetricHistory | undefined }) {
  const st = info.stats || {};
  const flow = sessionFlow(info);
  const origin = info.origin ? ORIGIN_TAG[info.origin] : undefined;
  const vol = volumeText(st.volume);

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
            t('stats.meta.sampleRate', { v: fmt.count(info.sample_rate) }),
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
        <span>{t('stats.extra.jbDepth', { n: fmt.count(st.jb_depth_frames) })}</span>
        <span>{t('stats.extra.rungChanges', { n: fmt.count(st.rung_changes) })}</span>
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
