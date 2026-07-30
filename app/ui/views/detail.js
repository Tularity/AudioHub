// 对端详情：完整指纹、别名、虚拟设备、地址历史、会话列表、解除配对。

import { store } from '../store.js';
import { el, icon, fmt, toast, sessionFlow, confirmDialog } from '../ui.js';
import { volumeText } from '../volume.js';
import { peerDeviceRows, halReasonText, halDeviceOf, DEVICE_STATE_LABEL, isModeB } from '../mode.js';

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
  const nameEl = el('h2', { class: 'detail-name' }, peer0.display_name || peer0.name || '未命名主机');
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

  // ---- 别名 ----
  // 改名走「同 UID 就地更新」（spec-m5b §3.5）：AudioObjectID 不变、设备列表不变，
  // 任何应用已记住的设备选择完全不受影响。这一点必须在界面上说出来，否则用户会
  // 因为怕搞乱 Zoom 里的选择而不敢改名。
  const aliasInput = el('input', {
    class: 'input', 'data-testid': 'detail-alias-input', maxlength: '48',
    placeholder: peer0.name || '对端主机名', autocomplete: 'off', spellcheck: 'false',
  });
  const aliasSave = el('button', { class: 'btn primary small', type: 'button', 'data-testid': 'detail-alias-save' }, '保存');
  const aliasClear = el('button', { class: 'btn ghost small', type: 'button', 'data-testid': 'detail-alias-clear' }, '清除');
  const aliasNote = el('p', { class: 'muted small', 'data-testid': 'detail-alias-note' });
  const aliasCard = el('section', { class: 'card block', 'data-testid': 'detail-alias' },
    el('h3', { class: 'block-title' }, '别名'),
    el('div', { class: 'form-row' },
      el('label', { class: 'field grow' },
        el('span', { class: 'field-label' }, '显示名称'), aliasInput),
      el('span', { class: 'field-btn' }, aliasSave, aliasClear)),
    aliasNote);

  let aliasBusy = false;
  async function setAlias(value) {
    if (aliasBusy) return;
    aliasBusy = true;
    aliasSave.disabled = true;
    aliasClear.disabled = true;
    try {
      const res = await ctx.rpc('peers.set_alias', { peer: fp, alias: value });
      toast(value ? `已改名为「${(res && res.display_name) || value}」` : '已恢复为对端主机名', 'ok');
      await ctx.refreshPeers();
    } catch (_) { /* rpc 已 toast */ } finally {
      aliasBusy = false;
      update(store.state);
    }
  }
  aliasSave.addEventListener('click', () => {
    const v = aliasInput.value.trim();
    setAlias(v || null);
  });
  aliasClear.addEventListener('click', () => { aliasInput.value = ''; setAlias(null); });
  aliasInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); aliasSave.click(); }
  });

  // ---- 虚拟设备 ----
  const devRows = el('div', { class: 'dev-list' });
  const devNote = el('p', { class: 'muted small', 'data-testid': 'detail-hal-note' });
  const devMeta = el('code', { class: 'mono dim', 'data-testid': 'detail-hal-meta' }, '');
  const devCard = el('section', { class: 'card block', 'data-testid': 'detail-hal-devices' },
    el('div', { class: 'dev-inv-head' },
      el('h3', { class: 'block-title' }, '虚拟设备'), devMeta),
    devRows, devNote);

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

  const unpairBtn = el('button', { class: 'btn danger', type: 'button', 'data-testid': 'detail-unpair' }, '解除配对');
  const dangerCard = el('section', { class: 'card block danger-block' },
    el('h3', { class: 'block-title danger-title' }, '危险操作'),
    el('p', { class: 'muted' },
      '解除配对会撤销双向信任、立即关闭全部会话，并无条件从系统移除这台对端的虚拟设备。'
      + '对端也会收到通知并移除本机的设备——它的系统列表里不会留下一对永远离线的幽灵设备。'),
    el('div', { class: 'field-btn' }, unpairBtn),
    el('p', { class: 'muted small' }, '若之后想再用这台主机，需要重新走一次配对流程。'));

  unpairBtn.addEventListener('click', async () => {
    const p = store.state.peers.find((x) => x.fingerprint === fp) || peer0;
    const dev = p.hal_device;
    const body = [
      `将解除与「${p.display_name || p.name || fp}」的配对，并撤销双向信任。`,
      dev
        ? `解除配对会立即从系统移除「${dev.out_name}」与「${dev.in_name}」。`
          + '若其中之一正是当前默认设备，系统会自动切换到其它设备。'
        : '该对端当前没有虚拟设备，只会移除信任与已建立的会话。',
    ];
    if (!await confirmDialog({
      title: '解除配对？', body, confirmText: '解除配对', danger: true, testid: 'confirm-unpair',
    })) return;
    unpairBtn.disabled = true;
    try {
      await ctx.rpc('peers.unpair', { peer: fp });
      toast('已解除配对', 'ok');
      await ctx.refreshPeers();
      ctx.navigate('peers');
    } catch (_) {
      unpairBtn.disabled = false;
    }
  });

  root.append(head, infoCard, aliasCard, devCard, addrCard, sessCard, dangerCard);

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
    nameEl.textContent = p.display_name || p.name || '未命名主机';
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

    // 别名：输入框只在用户没有在编辑时跟随 daemon，否则每秒一帧会把正在输入的字冲掉。
    if (document.activeElement !== aliasInput) aliasInput.value = p.alias || '';
    aliasSave.disabled = aliasBusy;
    aliasClear.disabled = aliasBusy || !p.alias;
    aliasNote.textContent = p.alias
      ? `虚拟设备名称使用别名「${p.alias}」；清除后恢复为对端上报的主机名「${p.name || '—'}」。`
      : '设置别名会改写这台对端在系统设备列表中的名字。改名是同 UID 就地进行的：'
        + '设备身份不变，任何应用已记住的选择都不受影响。';

    renderDevices(s, p);

    const sessions = s.sessions.filter((x) => x.peer_fingerprint === fp);
    sessEmpty.hidden = sessions.length > 0;
    tbody.innerHTML = '';
    for (const info of sessions) {
      const st = info.stats || {};
      const flow = sessionFlow(info);
      // origin=hal 的会话是「某个应用选中了这台对端的虚拟设备」的结果。从背后把它
      // 关掉，应用的设备选择还留在那儿——它会继续对着一台不再出声的设备播放，
      // 而系统里没有任何地方能解释这件事。所以这里不给关闭入口，只说清楚怎么停。
      const managed = info.origin === 'hal';
      const action = managed
        ? el('span', { class: 'dim small', 'data-testid': `session-managed-${info.id}` }, '由系统设备选择驱动')
        : el('button', {
          class: 'btn ghost small', type: 'button', 'data-testid': `session-close-${info.id}`,
        }, icon('close'), '关闭');
      if (!managed) action.addEventListener('click', () => closeSession(info.id, action));
      tbody.append(el('tr', { 'data-testid': `session-row-${info.id}`, class: flow.inbound ? 'inbound' : null },
        el('td', {}, el('code', { class: 'mono' }, `#${info.id}`)),
        el('td', { 'data-testid': `session-flow-${info.id}` },
          flow.label,
          flow.inbound ? el('span', { class: 'tag warn' }, '对端发起') : null,
          managed ? el('span', { class: 'tag accent', title: info.hal_device || '' }, '虚拟设备') : null),
        el('td', {}, DIR_LABEL[info.dir] || info.dir),
        el('td', {}, `${fmt.kbps(st.bitrate_kbps)} kbps`),
        el('td', {}, fmt.int(st.rung)),
        el('td', {}, `${fmt.pct(st.loss_pct)}%`),
        el('td', {}, `${fmt.ms(st.jitter_ms)} ms`),
        el('td', { 'data-testid': `session-volume-${info.id}` }, volumeCell(info)),
        el('td', {}, verdictCell(st.verdict)),
        el('td', {}, action)));
    }
  }

  function renderDevices(s, p) {
    const rows = peerDeviceRows(p, s.daemon);
    const info = halDeviceOf(s.daemon, fp);
    devRows.innerHTML = '';
    devRows.hidden = rows.length === 0;
    devMeta.textContent = info ? `槽位 ${info.slot} · 代号 ${info.generation}` : '';
    if (!rows.length) {
      devNote.textContent = isModeB(s)
        ? halReasonText(p.hal_reason)
        : '当前是模式 A：虚拟设备只在模式 B 下存在。在主面板顶部切换模式后，'
          + '这台对端会作为一对设备出现在系统音频设备列表里。';
      return;
    }
    const published = p.hal_device.state === 'bound' && p.hal_device.observed;
    for (const r of rows) {
      devRows.append(el('div', { class: 'dev-row', 'data-testid': `detail-device-${r.dir}` },
        icon(r.icon, 'ico dev-ico'),
        el('div', { class: 'dev-text' },
          el('span', { class: 'dev-name' }, r.name || '—'),
          el('code', { class: 'dev-uid mono' }, r.uid || '')),
        el('span', { class: 'dev-frames mono' },
          `${fmt.int(r.frames)} 帧` + (r.dropped ? ` · 丢 ${fmt.int(r.dropped)}` : '')),
        el('span', { class: 'dev-state ' + (r.io ? 'live' : published ? 'idle' : 'pending') },
          r.io ? '● 使用中' : published ? '○ 未使用' : '○ 等待系统发布')));
    }
    const stateText = DEVICE_STATE_LABEL[p.hal_device.state] || p.hal_device.state;
    devNote.textContent = published
      ? (p.online
        ? '两台设备已在系统音频设备列表中，可被任意应用直接选用。'
        : '⚠ 对端离线：设备仍在系统中可选，但不处理任何声音。')
      : `驱动状态「${stateText}」，系统设备列表${p.hal_device.observed ? '已' : '尚未'}列出它们。`;
  }

  const unsub = store.subscribe(update);
  update(store.state);
  return () => unsub();
}
