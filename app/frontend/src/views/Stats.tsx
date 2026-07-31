// 统计诊断：每活跃会话的数字块 + 60 点迷你折线；daemon 运行时长；IPC 往返延迟。

import { Icon } from '../components/Icon';
import { Spark } from '../components/Controls';
import { volumeText } from '../components/VolumeControl';
import { fmt, sessionFlow, dirLabel } from '../lib/fmt';
import { useTick } from '../lib/hooks';
import { t, joinPhrases } from '../i18n';
import type { MsgKey } from '../i18n';
import { useStore } from '../state/store';
import { isModeB } from '../state/mode';
import type { MetricHistory } from '../state/store';
import type { SessionInfo, SessionStats } from '../ipc/types';

const ORIGIN_TAG: Record<string, { cls: string; textKey: MsgKey; titleKey: MsgKey }> = {
  hal: { cls: 'tag accent', textKey: 'stats.origin.hal', titleKey: 'stats.origin.halTitle' },
  peer: { cls: 'tag warn', textKey: 'stats.origin.peer', titleKey: 'stats.origin.peerTitle' },
};

const METRICS: {
  key: 'loss' | 'jitter' | 'bitrate' | 'rung';
  labelKey: MsgKey;
  unitKey: MsgKey;
  val: (st: SessionStats) => number | undefined;
  fmt: (v: unknown) => string;
}[] = [
  { key: 'loss', labelKey: 'stats.metric.loss', unitKey: 'stats.unit.pct', val: (st) => st.loss_pct, fmt: (v) => fmt.pct(v) },
  { key: 'jitter', labelKey: 'stats.metric.jitter', unitKey: 'stats.unit.ms', val: (st) => st.jitter_ms, fmt: (v) => fmt.ms(v) },
  { key: 'bitrate', labelKey: 'stats.metric.bitrate', unitKey: 'stats.unit.kbps', val: (st) => st.bitrate_kbps, fmt: (v) => fmt.kbps(v) },
  { key: 'rung', labelKey: 'stats.metric.rung', unitKey: 'stats.unit.rung', val: (st) => st.rung, fmt: (v) => fmt.int(v) },
];

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
            info.peer_name || String(info.peer_fingerprint || '').slice(0, 8),
            t('stats.meta.sampleRate', { v: fmt.count(info.sample_rate) }),
            t('stats.meta.channels', { v: fmt.count(info.channels) }),
          ])}
        </span>
      </header>

      <div className="metrics">
        {METRICS.map((m) => (
          <div className="metric" key={m.key}>
            <div className="metric-label">{t(m.labelKey)}</div>
            <div className="metric-num" data-testid={`stat-${m.key}-${info.id}`}>
              {m.fmt(m.val(st))}
              <span className="unit">{t(m.unitKey)}</span>
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

export function StatsView() {
  const sessions = useStore((s) => s.sessions);
  const history = useStore((s) => s.history);
  const daemon = useStore((s) => s.daemon);
  const lastStatusAt = useStore((s) => s.lastStatusAt);
  const rtt = useStore((s) => s.ipcRttMs);
  const modeB = useStore(isModeB);
  useTick(1000);

  const uptime = daemon && lastStatusAt
    ? fmt.uptime((daemon.uptime_s ?? 0) + (Date.now() - lastStatusAt) / 1000)
    : t('common.dash');

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

      <div className="session-list">
        {sessions.map((info) => (
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
