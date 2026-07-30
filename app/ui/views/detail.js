// 对端详情：完整指纹、地址历史、会话列表（可逐个关闭）、解除配对（本期 CLI 限定）。

import { store } from '../store.js';
import { el, icon, fmt, toast, sessionFlow } from '../ui.js';
import { volumeText } from '../volume.js';

const DIR_LABEL = { send: '发送', recv: '接收' };

export function mount(root, ctx) {
  const fp = store.state.route.peerFp;

  const backBtn = el('button', { class: 'btn ghost', type: 'button', 'data-testid': 'detail-back' },
    icon('back'), '返回主面板');
  backBtn.addEventListener('click', () => ctx.navigate('peers'));

  const peer0 = store.state.peers.find((p) => p.fingerprint === fp);
  if (!fp || !peer0) {
    root.append(
      el('div', { class: 'detail-top' }, backBtn),
      el('div', { class: 'empty card' },
        el('h3', {}, '未找到该对端'),
        el('p', {}, '对端可能已被移除，或 daemon 尚未返回列表。')));
    return () => {};
  }

  const dot = el('span', { class: 'dot' });
  const nameEl = el('h2', { class: 'detail-name' }, peer0.name || '未命名主机');
  const onlineText = el('span', { class: 'detail-online', 'data-testid': 'detail-online' });
  const head = el('div', { class: 'detail-top' }, backBtn,
    el('div', { class: 'detail-title' }, dot, nameEl, onlineText));

  const copyBtn = el('button', { class: 'btn ghost small', type: 'button', 'data-testid': 'detail-copy-fp' },
    icon('copy'), '复制');
  copyBtn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(fp);
      toast('已复制完整指纹', 'ok');
    } catch (_) {
      toast('复制失败，请手动选择文本', 'warn');
    }
  });

  const metaPort = el('span', {}, '—');
  const metaAdded = el('span', {}, '—');
  const metaKey = el('code', { class: 'mono dim' }, '—');
  const infoCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '身份'),
    el('div', { class: 'fp-row' },
      el('code', { class: 'fp-full', 'data-testid': 'detail-fingerprint' }, fp), copyBtn),
    el('div', { class: 'kv' },
      el('div', { class: 'kv-row' }, el('span', { class: 'kv-k' }, '默认端口'), metaPort),
      el('div', { class: 'kv-row' }, el('span', { class: 'kv-k' }, '配对时间'), metaAdded),
      el('div', { class: 'kv-row' }, el('span', { class: 'kv-k' }, '公钥'), metaKey)));

  const addrList = el('ul', { class: 'addr-list', 'data-testid': 'detail-addrs' });
  const addrCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '地址历史'),
    addrList,
    el('p', { class: 'muted small' }, '来自 daemon 记录的最近地址与本次 UI 会话内观察到的变化。'));

  const tbody = el('tbody');
  const sessEmpty = el('p', { class: 'muted', 'data-testid': 'detail-sessions-empty', hidden: true }, '与该对端暂无活跃会话。');
  const sessCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '活跃会话'),
    el('div', { class: 'table-wrap' },
      el('table', { class: 'table', 'data-testid': 'detail-sessions' },
        el('thead', {}, el('tr', {},
          el('th', {}, '会话'), el('th', {}, '用途'), el('th', {}, '方向'),
          el('th', {}, '码率'), el('th', {}, 'RUNG'), el('th', {}, '丢包'),
          el('th', {}, '抖动'), el('th', {}, '音量'), el('th', {}, '校验'),
          el('th', {}, '操作'))),
        tbody)),
    sessEmpty);

  // M4a 的 IPC 没有解除配对方法：按钮直接禁用并给出 CLI 命令，
  // 不再走「危险确认」再什么都不做的假流程。
  const unpairBtn = el('button', {
    class: 'btn danger looks-disabled', type: 'button', 'data-testid': 'detail-unpair', disabled: true,
    title: '解除配对暂无 IPC 方法（M4a），本期请使用 CLI',
  }, '解除配对');
  const dangerCard = el('section', { class: 'card block danger-block' },
    el('h3', { class: 'block-title danger-title' }, '危险操作'),
    el('p', { class: 'muted' }, '解除配对将移除双向信任与虚拟设备（如有）。当前版本（M4a）IPC 尚无该方法，界面上不可执行。'),
    el('div', { class: 'field-btn' }, unpairBtn, el('span', { class: 'tag warn' }, '本期 CLI 限定')),
    el('p', { class: 'cli-hint', 'data-testid': 'detail-unpair-hint' },
      `本期请用 CLI：audiohub unpair --fingerprint ${fp}`));

  root.append(head, infoCard, addrCard, sessCard, dangerCard);

  // 只读回显：这张表每帧（1s）整体重建，放可拖动的滑块会在手指下被销毁重建。
  // 交互式音量控件在对端卡片上（spec-m4b §A3）；这里只如实显示同步到的值。
  // dir=send 是本机在驱动对端设备，dir=recv 是对端在驱动本机设备——标题要分清。
  function volumeCell(info) {
    const v = volumeText(info.stats && info.stats.volume);
    if (!v) return el('span', { class: 'dim' }, '—');
    return el('span', {
      class: 'vol-cell' + (v.muted ? ' muted' : ''),
      title: info.dir === 'recv' ? '本机输出设备音量' : '对端输出设备音量',
    },
      el('span', {}, v.text),
      v.adjustable ? null : el('span', { class: 'tag warn' }, '不可调'));
  }

  function verdictCell(v) {
    if (!v) return el('span', { class: 'dim' }, '—');
    return v.detected
      ? el('span', { class: 'tag ok' }, `通过 ${Number(v.snr_db).toFixed(1)} dB`)
      : el('span', { class: 'tag danger' }, '未通过');
  }

  async function closeSession(id, btn) {
    btn.disabled = true;
    try {
      await ctx.rpc('session.close', { id });
      store.removeSession(id);
      toast(`会话 #${id} 已关闭`, 'ok');
    } catch (_) {
      btn.disabled = false;
    }
  }

  function update(s) {
    const p = s.peers.find((x) => x.fingerprint === fp) || peer0;
    // 重连中要和「离线」分开说：daemon 还在按退避重拨，不是放弃了。
    // 倒计时留给对端卡片——这张页面每秒重绘，而 retry_in_s 只随 peers.list 刷新，
    // 在这里逐秒显示只会显得卡住。
    const reconnecting = !p.online && !!p.reconnecting;
    dot.className = 'dot ' + (p.online ? 'online' : reconnecting ? 'connecting' : 'offline');
    nameEl.textContent = p.name || '未命名主机';
    onlineText.textContent = p.online ? '在线' : reconnecting ? '重连中…' : '离线';
    onlineText.className = 'detail-online ' + (p.online ? 'ok' : 'dim');
    metaPort.textContent = String(p.port ?? '—');
    metaAdded.textContent = fmt.date(p.added_unix);
    const pk = p.public_key_b64 || '';
    metaKey.textContent = pk ? pk.slice(0, 24) + (pk.length > 24 ? '…' : '') : '—';
    metaKey.title = pk;

    addrList.innerHTML = '';
    const hist = (s.addrHistory[fp] || []).slice().sort((a, b) => b.seenAt - a.seenAt);
    if (!hist.length && p.last_addr) hist.push({ addr: p.last_addr, seenAt: null });
    if (!hist.length) {
      addrList.append(el('li', { class: 'muted' }, '暂无地址记录'));
    } else {
      for (const h of hist) {
        addrList.append(el('li', {},
          el('code', { class: 'mono' }, h.addr),
          el('span', { class: 'dim small' }, h.seenAt ? ` 最近见于 ${fmt.clock(h.seenAt)}` : ' daemon 记录')));
      }
    }

    const sessions = s.sessions.filter((x) => x.peer_fingerprint === fp);
    sessEmpty.hidden = sessions.length > 0;
    tbody.innerHTML = '';
    for (const info of sessions) {
      const st = info.stats || {};
      const closeBtn = el('button', {
        class: 'btn ghost small', type: 'button', 'data-testid': `session-close-${info.id}`,
      }, icon('close'), '关闭');
      closeBtn.addEventListener('click', () => closeSession(info.id, closeBtn));
      const flow = sessionFlow(info);
      tbody.append(el('tr', { 'data-testid': `session-row-${info.id}`, class: flow.inbound ? 'inbound' : null },
        el('td', {}, el('code', { class: 'mono' }, `#${info.id}`)),
        el('td', { 'data-testid': `session-flow-${info.id}` },
          flow.label,
          flow.inbound ? el('span', { class: 'tag warn' }, '对端发起') : null),
        el('td', {}, DIR_LABEL[info.dir] || info.dir),
        el('td', {}, `${fmt.kbps(st.bitrate_kbps)} kbps`),
        el('td', {}, fmt.int(st.rung)),
        el('td', {}, `${fmt.pct(st.loss_pct)}%`),
        el('td', {}, `${fmt.ms(st.jitter_ms)} ms`),
        el('td', { 'data-testid': `session-volume-${info.id}` }, volumeCell(info)),
        el('td', {}, verdictCell(st.verdict)),
        el('td', {}, closeBtn)));
    }
  }

  const unsub = store.subscribe(update);
  update(store.state);
  return () => unsub();
}
