// 统计诊断：每活跃会话的数字块 + 60 点迷你折线；daemon 运行时长；IPC 往返延迟。

import { store } from '../store.js';
import { el, fmt, drawSpark, icon, sessionFlow } from '../ui.js';
import { volumeText } from '../volume.js';

const DIR_LABEL = { send: '发送', recv: '接收' };

const METRICS = [
  { key: 'loss', label: '丢包率', unit: '%', val: (st) => st.loss_pct, fmt: (v) => fmt.pct(v) },
  { key: 'jitter', label: '抖动', unit: 'ms', val: (st) => st.jitter_ms, fmt: (v) => fmt.ms(v) },
  { key: 'bitrate', label: '码率', unit: 'kbps', val: (st) => st.bitrate_kbps, fmt: (v) => fmt.kbps(v) },
  { key: 'rung', label: '质量阶梯', unit: 'RUNG', val: (st) => st.rung, fmt: (v) => fmt.int(v) },
];

export function mount(root) {
  const uptimeEl = el('div', { class: 'tile-num', 'data-testid': 'stats-uptime' }, '—');
  const rttEl = el('div', { class: 'tile-num', 'data-testid': 'stats-rtt' }, '—');
  const cntEl = el('div', { class: 'tile-num', 'data-testid': 'stats-session-count' }, '0');

  const tiles = el('div', { class: 'tile-row' },
    el('div', { class: 'card tile' }, el('div', { class: 'tile-label' }, 'daemon 运行时长'), uptimeEl),
    el('div', { class: 'card tile' }, el('div', { class: 'tile-label' }, 'IPC 往返延迟'), rttEl),
    el('div', { class: 'card tile' }, el('div', { class: 'tile-label' }, '活跃会话'), cntEl));

  const list = el('div', { class: 'session-list' });
  const empty = el('div', { class: 'empty card', 'data-testid': 'stats-empty' },
    icon('stats', 'empty-ico'),
    el('h3', {}, '暂无活跃会话'),
    el('p', {}, '在主面板打开对端卡片上的通路开关，或用 CLI 发起会话后，这里会实时出现指标。'));

  root.append(tiles, list, empty);

  const rows = new Map(); // id -> refs

  function buildRow(info) {
    const metricRefs = {};
    const blocks = METRICS.map((m) => {
      const num = el('div', { class: 'metric-num', 'data-testid': `stat-${m.key}-${info.id}` },
        '—', el('span', { class: 'unit' }, m.unit));
      const canvas = el('canvas', {
        class: 'spark', width: '160', height: '36',
        'data-testid': `spark-${m.key}-${info.id}`,
      });
      metricRefs[m.key] = { num, canvas };
      return el('div', { class: 'metric' },
        el('div', { class: 'metric-label' }, m.label), num, canvas);
    });

    const headMeta = el('span', { class: 'sess-meta' });
    const extra = el('footer', { class: 'session-extra', 'data-testid': `session-extra-${info.id}` });

    // 音量同步回显（只读）：spk 会话两侧都有 stats.volume——dir=send 时那是
    // 对端的真实输出设备，dir=recv 时那是本机自己的。调节入口在对端卡片上。
    const volIco = icon('spk', 'ico');
    const volLabel = el('span', { class: 'vol-readout-label' }, '输出音量');
    const volFill = el('div', { class: 'vol-bar-fill' });
    const volVal = el('span', { class: 'vol-val' }, '—');
    const volTag = el('span', { class: 'tag warn', hidden: true }, '不可调');
    const volRow = el('div', {
      class: 'vol-readout', hidden: true, 'data-testid': `session-volume-${info.id}`,
    }, volIco, volLabel, el('div', { class: 'vol-bar' }, volFill), volVal, volTag);

    const flow = sessionFlow(info);
    const card = el('article', { class: 'card session-card', 'data-testid': `session-row-${info.id}` },
      el('header', { class: 'session-head' },
        el('strong', {}, `会话 #${info.id}`),
        el('span', { class: 'tag accent', title: flow.label, 'data-testid': `session-flow-${info.id}` }, flow.short),
        el('span', { class: 'tag' }, DIR_LABEL[info.dir] || info.dir),
        flow.inbound ? el('span', { class: 'tag warn' }, '对端发起') : null,
        headMeta),
      el('div', { class: 'metrics' }, blocks),
      volRow,
      extra);
    return { card, metricRefs, headMeta, extra, vol: { row: volRow, label: volLabel, fill: volFill, val: volVal, tag: volTag } };
  }

  function updateRow(refs, info, hist) {
    const st = info.stats || {};
    refs.headMeta.textContent = `${info.peer_name || info.peer_fingerprint.slice(0, 8)} · ${info.sample_rate} Hz · ${info.channels} 声道`;
    for (const m of METRICS) {
      const r = refs.metricRefs[m.key];
      const v = m.val(st);
      r.num.firstChild.nodeValue = m.fmt(v);
      drawSpark(r.canvas, hist ? hist[m.key] : [], { floor: 0 });
    }
    const vol = volumeText(st.volume);
    refs.vol.row.hidden = !vol;
    if (vol) {
      refs.vol.label.textContent = info.dir === 'recv' ? '本机输出音量' : '对端输出音量';
      refs.vol.fill.style.transform = `scaleX(${(parseInt(vol.pct, 10) / 100).toFixed(3)})`;
      refs.vol.val.textContent = vol.text;
      refs.vol.row.classList.toggle('muted', vol.muted);
      refs.vol.tag.hidden = vol.adjustable;
    }

    refs.extra.innerHTML = '';
    refs.extra.append(
      el('span', {}, `收包 ${st.received ?? 0}`),
      el('span', {}, `丢包 ${st.lost ?? 0}`),
      el('span', {}, `发包 ${st.sent_packets ?? 0}`),
      el('span', {}, `缓冲 ${st.jb_depth_frames ?? 0} 帧`),
      el('span', {}, `档位变更 ${st.rung_changes ?? 0} 次`));
    if (st.verdict) {
      refs.extra.append(st.verdict.detected
        ? el('span', { class: 'tag ok' }, `校验通过 ${Number(st.verdict.snr_db).toFixed(1)} dB`)
        : el('span', { class: 'tag danger' }, '校验未通过'));
    }
    if (Array.isArray(st.mix_verdicts) && st.mix_verdicts.length) {
      refs.extra.append(el('span', { class: 'tag' }, `混音探针 ${st.mix_verdicts.length} 路`));
    }
  }

  function updateUptime(s) {
    if (s.daemon && s.lastStatusAt) {
      const base = s.daemon.uptime_s + (Date.now() - s.lastStatusAt) / 1000;
      uptimeEl.textContent = fmt.uptime(base);
    } else {
      uptimeEl.textContent = '—';
    }
    rttEl.textContent = typeof s.ipcRttMs === 'number' ? `${s.ipcRttMs.toFixed(1)} ms` : '—';
  }

  function update(s) {
    updateUptime(s);
    cntEl.textContent = String(s.sessions.length);
    empty.hidden = s.sessions.length > 0;

    const alive = new Set();
    for (const info of s.sessions) {
      alive.add(info.id);
      let refs = rows.get(info.id);
      if (!refs) {
        refs = buildRow(info);
        rows.set(info.id, refs);
        list.append(refs.card);
      }
      updateRow(refs, info, s.history[info.id]);
    }
    for (const [id, refs] of rows) {
      if (!alive.has(id)) {
        refs.card.remove();
        rows.delete(id);
      }
    }
  }

  const tick = setInterval(() => updateUptime(store.state), 1000);
  const unsub = store.subscribe(update);
  update(store.state);

  return () => {
    clearInterval(tick);
    unsub();
  };
}
