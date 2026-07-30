// 主面板：全局模式栏 + 对端卡片列表 + 手动添加对端。
//
// 模式栏放在这里而不是设置页，是 plan §7.1 的直接后果：模式决定了下面每张卡片的
// 含义（模式 A 在卡片上选对端，模式 B 在系统声音设置里选设备），把它藏进设置页
// 就等于把「这些开关为什么消失了」的答案藏起来。

import { store } from '../store.js';
import { el, icon, fmt, switchBtn, setSwitch, setPending, setMeter, toast, segmented } from '../ui.js';
import { volumeControl } from '../volume.js';
import { bridgeControl, bridgeTargets } from '../bridge.js';
import {
  MODE_A, MODE_B, halState, requestedMode, effectiveMode, isModeB,
  modeDowngraded, peerDeviceRows, halReasonText,
} from '../mode.js';

const MODE_DESC = {
  // §6.2 冻结文案。两段都必须说清「用哪一台对端在哪里选」——这正是两种模式的分界。
  a: '捕获本机系统音频送到对端播放（两端同时发声）；取用对端麦克风需借助已安装的第三方虚拟声卡。'
    + '用哪一台对端，在下方各张卡片上选择。',
  b: '每台已配对的主机都会作为一对设备出现在系统音频设备列表里。用哪一台主机，'
    + '就在「系统设置 › 声音」或任意应用里选它的设备——本界面不再提供对端选择。'
    + '调节该设备的音量即调节对端真实设备的音量。',
};

const DEVICE_FOOT = '在「系统设置 › 声音」或任意应用的音频设备菜单里选中它即可使用；'
  + '音量在系统里调节，会同步到这台主机的真实设备。';

// 进行中的通路操作（`${fp}:mic` / `${fp}:spk`），也是开关 pending 态的唯一判据。
const busy = new Set();

// mic / monitor / bridge 操作的是同一条 mic 通路，必须共用一把锁，否则会互相关掉
// 对方刚开的会话。
function busyKey(fp, kind) {
  return `${fp}:${kind === 'monitor' || kind === 'bridge' ? 'mic' : kind}`;
}

// 本机取用对端麦克风的开启参数。monitor（本机监听）与 bridge（写入虚拟声卡）是
// 同一条通路上的两个去向，daemon 允许同时开，所以两者都从偏好里读当前值。
function micParams(fp) {
  const s = store.state;
  const p = { peer: fp, kind: 'mic', monitor: !!s.monitorPref[fp] };
  const b = s.bridgePref[fp];
  // 只在选了卡、且这张卡此刻确实可写时才带 bridge：OpenSessionParams.bridge 是
  // Option<String>，空串或已消失的设备名都会被当成设备名去找，daemon 找不到就直接
  // 开会话失败（规格明确禁止静默回落）。声卡被拔掉/改名后偏好还留着，不在这里挡下
  // 来就等于每次开麦克风都必然失败。偏好本身保留：卡装回来就照旧生效。
  if (b) {
    if (bridgeTargets(s.daemon).some((c) => c.name === b)) p.bridge = b;
    else toast(`虚拟声卡「${b}」当前不可用，本次不桥接。`, 'warn');
  }
  return p;
}

// 重连倒计时：peers.list 10s 一轮，retry_in_s 只在那一刻是准的，中间自己往下走。
// 每当 daemon 报来一个新值就重新对时，绝不在本地凭空续命。
const retryAnchor = new Map(); // fp -> {value, at}

function anchorRetry(fp, v) {
  const cur = retryAnchor.get(fp);
  if (!cur || cur.value !== v) retryAnchor.set(fp, { value: v, at: Date.now() });
}

function reconnectLabel(fp) {
  const a = retryAnchor.get(fp);
  if (!a || typeof a.value !== 'number' || !isFinite(a.value)) return '重连中…';
  const remain = a.value - (Date.now() - a.at) / 1000;
  // 走到 0 说明这一拨已经在飞，而下一次的间隔还没报回来：显示「0s 后重试」会像卡死。
  return remain >= 1 ? `重连中…（${Math.ceil(remain)}s 后重试）` : '重连中…';
}

// 本机发起的会话：mic = 取对方麦克风（媒体 对方→我，dir recv）；spk = 送对方扬声器（dir send）。
// 必须 (kind,dir) 联合过滤：daemon 存的 kind 是发起方视角，对端发起的 mic 会话 dir 是 send
// （= 对方在取用本机麦克风），只按 kind 匹配会把它误当成本机的通路。
function matching(state, fp, kind) {
  const dir = kind === 'mic' ? 'recv' : 'send';
  return state.sessions.filter((x) => x.peer_fingerprint === fp && x.kind === kind && x.dir === dir);
}

function findSess(state, fp, kind) {
  return matching(state, fp, kind)[0] || null;
}

// 对端发起、正在取用本机麦克风的会话（kind=mic + dir=send）。隐私相关，必须显式可见。
function inboundMic(state, fp) {
  return state.sessions.filter((x) => x.peer_fingerprint === fp && x.kind === 'mic' && x.dir === 'send');
}

// 模式栏：通栏、置顶。testid 保留 `settings-consumer-mode`（回归已依赖），
// 外层另给别名 `consumer-mode`。
function modeBanner(ctx) {
  const seg = segmented('settings-consumer-mode',
    [
      { value: MODE_A, label: 'A · 免驱动' },
      {
        value: MODE_B,
        label: 'B · 虚拟设备',
        // 置灰必须是**真禁用**（点击不改变任何状态），判据见 mode.js halState()。
        disabled: () => !halState(store.state.daemon).available,
        why: () => halState(store.state.daemon).why || '',
      },
    ],
    () => effectiveMode(store.state),
    (v) => setMode(v));

  const desc = el('p', { class: 'mode-desc', 'data-testid': 'consumer-mode-desc' });
  const note = el('p', { class: 'mode-note', 'data-testid': 'settings-mode-note' });
  const warn = el('p', { class: 'mode-warn', 'data-testid': 'consumer-mode-downgraded', hidden: true });

  const node = el('section', { class: 'card block mode-bar', 'data-testid': 'consumer-mode' },
    el('div', { class: 'mode-head' },
      el('div', { class: 'mode-title-wrap' },
        el('h3', { class: 'block-title' }, '使用端模式'),
        el('p', { class: 'mode-sub' }, '全局设置：决定本机怎么使用别人的音频设备，对全部对端一起生效。')),
      seg),
    desc, note, warn);

  async function setMode(v) {
    if (v === requestedMode(store.state)) return;
    try {
      await ctx.applySettings({ consumer_mode: v });
      toast(v === MODE_B
        ? '已切换到模式 B：已配对主机将作为音频设备出现在系统里。'
        : '已切换到模式 A：全部 AudioHub 虚拟设备已从系统移除。', 'ok');
    } catch (_) { /* rpc 已 toast */ }
  }

  function update(s) {
    seg.sync();
    const mode = effectiveMode(s);
    desc.textContent = MODE_DESC[mode];
    const st = halState(s.daemon);
    note.textContent = st.text;
    note.className = `mode-note tone-${st.tone}`;
    // 用户存的是 B、daemon 只能给 A：这是**降级**，不是「他选了 A」。不说出来的话，
    // 界面会显示成他从没选过 B。
    const down = modeDowngraded(s);
    warn.hidden = !down;
    if (down) warn.textContent = '你选择的是模式 B，但当前不可用，已临时按模式 A 运行。';
  }

  return { node, update };
}

export function mount(root, ctx) {
  const modeUi = modeBanner(ctx);
  const summary = el('div', { class: 'toolbar-note', 'data-testid': 'peers-summary' });
  const addBtn = el('button', { class: 'btn primary', type: 'button', 'data-testid': 'add-peer-btn' },
    icon('plus'), '添加手动对端');
  const toolbar = el('div', { class: 'toolbar' }, summary, addBtn);

  const peerInput = el('input', {
    class: 'input', 'data-testid': 'add-peer-peer', list: 'ah-peer-fps',
    placeholder: '对端指纹（可输前缀）', autocomplete: 'off', spellcheck: 'false',
  });
  const datalist = el('datalist', { id: 'ah-peer-fps' });
  const addrInput = el('input', {
    class: 'input', 'data-testid': 'add-peer-input',
    placeholder: 'IP 或 IP:端口（留空使用最近地址）', autocomplete: 'off', spellcheck: 'false',
  });
  const connectBtn = el('button', { class: 'btn primary', type: 'submit', 'data-testid': 'add-peer-connect' }, '连接');
  const form = el('form', { class: 'card add-peer-form', hidden: true, 'data-testid': 'add-peer-form' },
    el('div', { class: 'form-row' },
      el('label', { class: 'field' }, el('span', { class: 'field-label' }, '对端指纹'), peerInput, datalist),
      el('label', { class: 'field grow' }, el('span', { class: 'field-label' }, '地址'), addrInput),
      connectBtn),
    el('p', { class: 'form-note' }, '通过 peers.connect 主动连接已配对对端：daemon 会按指纹校验对端身份，跨网段亦可用。'));

  addBtn.addEventListener('click', () => {
    form.hidden = !form.hidden;
    if (!form.hidden) peerInput.focus();
  });

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const peer = peerInput.value.trim();
    const addr = addrInput.value.trim();
    if (!peer) {
      toast('请填写对端指纹（可输前缀）', 'warn');
      peerInput.focus();
      return;
    }
    connectBtn.disabled = true;
    connectBtn.textContent = '连接中…';
    try {
      // 超时由 ws.js 的方法级表给出（daemon 最坏 TCP 5s + 握手 10s）。
      await ctx.rpc('peers.connect', addr ? { peer, addr } : { peer });
      toast('连接请求已完成', 'ok');
      form.hidden = true;
      addrInput.value = '';
      ctx.refreshPeers();
    } catch (_) { /* rpc 已 toast */ } finally {
      connectBtn.disabled = false;
      connectBtn.textContent = '连接';
    }
  });

  const grid = el('div', { class: 'peer-grid' });
  // 首次启动的空态：不能是一片空白，必须把「两台设备先配对」这件事说清楚。
  const emptyModeB = el('p', { class: 'empty-mode-b', 'data-testid': 'peers-empty-mode-b', hidden: true },
    '配对完成后，对方主机会立刻作为一对音频设备出现在「系统设置 › 声音」里；'
    + '把输出切到它，或在任意应用里选它，就是在使用那台主机。');
  const empty = el('div', { class: 'empty card', 'data-testid': 'peers-empty', hidden: true },
    icon('pair', 'empty-ico'),
    el('h3', {}, '先在两台设备上完成配对'),
    el('p', {}, 'AudioHub 通过配对建立两台设备之间的互信，之后才能共享麦克风与扬声器。'),
    el('ol', { class: 'empty-steps' },
      el('li', {}, '在两台设备上都打开 AudioHub'),
      el('li', {}, '本机打开「配对向导」生成 6 位 PIN'),
      el('li', {}, '另一台设备发现本机后输入同一个 PIN')),
    emptyModeB,
    el('button', { class: 'btn primary', type: 'button', 'data-testid': 'peers-empty-pair' },
      icon('pair'), '打开配对向导'));
  empty.querySelector('[data-testid="peers-empty-pair"]').addEventListener('click', () => ctx.navigate('pair'));

  root.append(modeUi.node, toolbar, form, grid, empty);

  const cards = new Map(); // fp -> refs
  let gridKey = null;

  function toggleRow(ico, label, sw) {
    return el('div', { class: 'toggle-row' }, ico, el('span', { class: 'toggle-label' }, label), sw);
  }

  // session.open 在 daemon 侧最坏要 30s（见 ws.js 的 METHOD_TIMEOUT_MS 推算）：
  // 期间开关停在可见的 pending 态、busy 键一直握着，结束后一律用 daemon 的会话列表
  // 对账，绝不乐观翻转——否则超时报错时会话其实已经建立，开关就跟真实状态脱节了。
  async function settle(key) {
    busy.delete(key);
    try { await ctx.refreshSessions(); } catch (_) { /* ignore */ }
    update(store.state);
  }

  async function toggleSession(fp, kind, want) {
    const key = busyKey(fp, kind);
    if (busy.has(key)) return;
    busy.add(key);
    update(store.state);
    try {
      if (want) {
        // spk 一律带 volume_sync（spec-m4b §A3-4）：daemon 只对开了它的会话
        // 接受 session.set_volume，不带就等于卡片上的滑块必然报错。
        const params = kind === 'mic'
          ? micParams(fp)
          : { peer: fp, kind: 'spk', source: 'mic', volume_sync: true };
        const info = await ctx.rpc('session.open', params);
        store.upsertSession(info);
      } else {
        for (const sess of matching(store.state, fp, kind)) {
          await ctx.rpc('session.close', { id: sess.id });
          store.removeSession(sess.id);
        }
      }
    } catch (_) { /* rpc 已 toast */ } finally {
      await settle(key);
    }
  }

  // 切换监听 / 换桥接目标 = 换一条 mic 会话。先开新的、成功后才关旧的：反过来
  // 一旦重开失败，音频就断了而且界面上一条会话都不剩。
  async function reopenMic(fp, kind, rollback) {
    const key = busyKey(fp, kind);
    if (busy.has(key)) return; // mic 通路正忙，偏好也先别动
    const active = matching(store.state, fp, 'mic');
    if (!active.length) {
      update(store.state); // 只记偏好，下次打开 mic 通路时生效
      return;
    }
    busy.add(key);
    update(store.state);
    try {
      const info = await ctx.rpc('session.open', micParams(fp));
      store.upsertSession(info);
      for (const sess of active) {
        if (sess.id === info.id) continue;
        try {
          await ctx.rpc('session.close', { id: sess.id }, { silent: true });
          store.removeSession(sess.id);
        } catch (_) {
          toast(`旧会话 #${sess.id} 未能关闭，请在对端详情页手动关闭。`, 'warn');
        }
      }
    } catch (_) {
      rollback(); // 新会话没开起来，偏好回滚
    } finally {
      await settle(key);
    }
  }

  function toggleMonitor(fp, want) {
    if (busy.has(busyKey(fp, 'monitor'))) return;
    const prev = !!store.state.monitorPref[fp];
    store.update((s) => { s.monitorPref[fp] = want; });
    return reopenMic(fp, 'monitor', () => store.update((s) => { s.monitorPref[fp] = prev; }));
  }

  // 桥接目标：'' = 不桥接。未检测到虚拟声卡时控件本身是禁用的，走不到这里。
  function setBridge(fp, want) {
    if (busy.has(busyKey(fp, 'bridge'))) return;
    const prev = store.state.bridgePref[fp] || '';
    if (want === prev) return;
    store.update((s) => { s.bridgePref[fp] = want; });
    return reopenMic(fp, 'bridge', () => store.update((s) => { s.bridgePref[fp] = prev; }));
  }

  function buildStream(fp, kind, label) {
    const rate = el('span', { class: 'stream-rate' }, '空闲');
    const fill = el('div', { class: 'meter-fill', 'data-testid': `level-${kind}-${fp}` });
    const row = el('div', { class: 'stream', 'data-testid': `stream-${kind}-${fp}` },
      el('span', { class: 'stream-label' }, label),
      el('div', { class: 'meter' }, fill),
      rate);
    return { row, fill, rate };
  }

  // 模式 B 的对端卡片主体：这台对端的两台系统设备 + 它们此刻的状态。
  // 这里**没有任何选择器**——选哪台对端 = 在系统里选哪台设备（plan §7.1 冻结）。
  function buildDevices(fp) {
    const rows = {};
    const list = el('div', { class: 'dev-list' });
    for (const dir of ['out', 'in']) {
      const name = el('span', { class: 'dev-name' }, '—');
      const uid = el('code', { class: 'dev-uid mono' }, '');
      const state = el('span', { class: 'dev-state' }, '');
      const row = el('div', { class: 'dev-row', 'data-testid': `peer-device-${dir}-${fp}` },
        icon(dir === 'out' ? 'spk' : 'mic', 'ico dev-ico'),
        el('div', { class: 'dev-text' }, name, uid),
        state);
      rows[dir] = { row, name, uid, state };
      list.append(row);
    }
    const note = el('p', { class: 'dev-note', 'data-testid': `peer-devices-note-${fp}` });
    const foot = el('p', { class: 'dev-foot' }, DEVICE_FOOT);
    const node = el('div', { class: 'peer-devices', 'data-testid': `peer-devices-${fp}`, hidden: true },
      el('div', { class: 'dev-head' }, icon('device', 'ico'), el('span', {}, '系统设备')),
      list, note, foot);
    // 卡片整体可点（进入详情）；设备区里的文本要能选中复制 UID，别一点就被导航走。
    node.addEventListener('click', (e) => e.stopPropagation());

    return {
      node,
      apply(peer, daemon) {
        const devs = peerDeviceRows(peer, daemon);
        const has = devs.length > 0;
        list.hidden = !has;
        foot.hidden = !has;
        if (!has) {
          note.textContent = halReasonText(peer.hal_reason);
          note.className = 'dev-note warn';
          return;
        }
        for (const d of devs) {
          const r = rows[d.dir];
          r.name.textContent = d.name || '—';
          r.name.title = d.name || '';
          r.uid.textContent = d.uid || '';
          r.uid.title = d.uid || '';
          // 三层状态，含义各不相同：正在被应用使用 / 已发布但没人用 / 还没真正出现在系统里。
          const published = peer.hal_device.state === 'bound' && peer.hal_device.observed;
          r.state.className = 'dev-state ' + (d.io ? 'live' : published ? 'idle' : 'pending');
          r.state.textContent = d.io ? '● 使用中' : published ? '○ 未使用' : '○ 等待系统发布';
          r.row.classList.toggle('active', d.io);
        }
        // 对端离线时设备仍在系统里可选，只是不处理声音——这是 plan §7.3 的既定语义，
        // 必须在卡片上说出来，否则「没声音」在系统里完全不可观测。
        if (!peer.online) {
          note.textContent = '⚠ 对端离线：设备仍在系统中可选，但不处理任何声音。';
          note.className = 'dev-note warn';
        } else if (!peer.hal_device.observed) {
          note.textContent = '设备已下发，正在等待系统的设备列表刷新（最多 1 秒）。';
          note.className = 'dev-note';
        } else {
          note.textContent = '';
          note.className = 'dev-note';
        }
        note.hidden = !note.textContent;
      },
    };
  }

  function buildCard(p) {
    const fp = p.fingerprint;
    const dot = el('span', { class: 'dot' });
    const meta = el('div', { class: 'peer-meta' });

    const micSw = switchBtn({ testid: `toggle-mic-${fp}`, label: '取对方麦克风', onToggle: (w) => toggleSession(fp, 'mic', w) });
    const spkSw = switchBtn({ testid: `toggle-spk-${fp}`, label: '送对方扬声器', onToggle: (w) => toggleSession(fp, 'spk', w) });
    const monSw = switchBtn({ testid: `toggle-monitor-${fp}`, label: '监听接收音频', onToggle: (w) => toggleMonitor(fp, w) });

    const mic = buildStream(fp, 'mic', '接收');
    const spk = buildStream(fp, 'spk', '发送');

    // 「送对方扬声器」下方的音量同步控件：会话激活后出现，值来自 stats.volume。
    const vol = volumeControl({
      volumeTestid: `volume-${fp}`,
      muteTestid: `mute-${fp}`,
      label: `${p.name || fp} 的扬声器音量`,
      // silent：拖动会连发，失败提示由控件自己在框内给一条，不刷 toast。
      onSet: (id, params) => ctx.rpc('session.set_volume', { id, ...params }, { silent: true }),
    });

    // 「取对方麦克风」的第二个去向（plan §7.1）：写入第三方虚拟声卡的播放端。
    const bridge = bridgeControl({
      testid: `bridge-${fp}`,
      onChange: (value) => setBridge(fp, value),
    });

    // 控制通道断了但没被解除配对：daemon 正在按退避重拨，界面必须说出来，
    // 否则「离线」看着就像放弃了。
    const reconnectText = el('span', { class: 'reconnect-text' });
    const reconnect = el('div', {
      class: 'peer-reconnect', 'data-testid': `reconnecting-${fp}`, hidden: true, role: 'status',
    }, el('span', { class: 'spinner tiny' }), reconnectText);

    // 对端正在取用本机麦克风：隐私相关，必须在卡片上直接看见
    const inboundText = el('span', { class: 'inbound-text' });
    const inbound = el('div', {
      class: 'peer-inbound', 'data-testid': `inbound-mic-${fp}`, hidden: true, role: 'status',
    }, el('span', { class: 'dot live' }), icon('mic', 'ico'), inboundText);

    // 模式 A 专属的一整排通路控件。模式 B 下它们全部**下线**（不是变灰）：
    // 取谁的麦克风、送谁的扬声器由 App 在系统里选设备决定，这些开关既不反映那个
    // 选择、也无法表达它；留着只会让用户以为自己在这里做了什么。
    const toggles = el('div', { class: 'peer-toggles', 'data-testid': `peer-toggles-${fp}` },
      toggleRow(icon('mic'), '取对方麦克风', micSw),
      toggleRow(icon('spk'), '送对方扬声器', spkSw),
      vol.node,
      toggleRow(icon('monitor'), '监听接收音频', monSw),
      bridge.node);

    const devices = buildDevices(fp);
    const alias = el('span', { class: 'peer-alias', 'data-testid': `peer-alias-${fp}`, hidden: true });

    const card = el('article', {
      class: 'card peer-card', 'data-testid': `peer-card-${fp}`, tabindex: '0', role: 'button',
      'aria-label': `查看 ${p.name || fp} 详情`,
    },
      el('header', { class: 'peer-head' },
        dot,
        el('h3', { class: 'peer-name' }, p.display_name || p.name || '未命名主机'),
        alias,
        el('code', { class: 'peer-fp', title: fp }, fmt.fp(fp, 16))),
      meta,
      reconnect,
      inbound,
      toggles,
      devices.node,
      el('div', { class: 'peer-streams' }, mic.row, spk.row));

    card.addEventListener('click', () => ctx.navigate('detail', fp));
    card.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && e.target === card) ctx.navigate('detail', fp);
    });
    return {
      card, dot, meta, micSw, spkSw, monSw, mic, spk, vol, bridge,
      reconnect, reconnectText, inbound, inboundText, toggles, devices, alias,
    };
  }

  function updStream(refs, sess) {
    refs.row.classList.toggle('active', !!sess);
    const kbps = sess && sess.stats ? sess.stats.bitrate_kbps : 0;
    refs.rate.textContent = sess ? `${fmt.kbps(kbps)} kbps` : '空闲';
    setMeter(refs.fill, (kbps || 0) / 900);
  }

  function update(s) {
    modeUi.update(s);
    const modeB = isModeB(s);
    const key = s.peers
      .map((p) => [p.fingerprint, p.display_name, p.name, p.online, p.last_addr, p.port].join(''))
      .join('\n');
    if (key !== gridKey) {
      gridKey = key;
      grid.innerHTML = '';
      datalist.innerHTML = '';
      for (const r of cards.values()) r.vol.destroy(); // window 上的指针监听要撤掉
      cards.clear();
      // 按指纹存的本地状态必须跟着对端一起消失：解除配对后不清，既是无限增长，
      // 也会在同一指纹重新配对时把上一轮的倒计时接着显示出来。
      const alive = new Set(s.peers.map((p) => p.fingerprint));
      for (const fp of retryAnchor.keys()) {
        if (!alive.has(fp)) retryAnchor.delete(fp);
      }
      for (const p of s.peers) {
        const refs = buildCard(p);
        cards.set(p.fingerprint, refs);
        grid.append(refs.card);
        datalist.append(el('option', { value: p.fingerprint }, p.name || ''));
      }
    }
    empty.hidden = s.peers.length > 0;
    emptyModeB.hidden = !modeB;
    grid.classList.toggle('mode-b', modeB);
    const online = s.peers.filter((p) => p.online).length;
    const retrying = s.peers.filter((p) => !p.online && p.reconnecting).length;
    const published = modeB ? s.peers.filter((p) => p.hal_device).length : 0;
    summary.textContent = s.peers.length
      ? `已配对 ${s.peers.length} 台 · 在线 ${online} 台`
        + (retrying ? ` · 重连中 ${retrying} 台` : '')
        + (modeB ? ` · 系统设备 ${published * 2} 台` : '')
      : '暂无已配对对端';

    for (const p of s.peers) {
      const r = cards.get(p.fingerprint);
      if (!r) continue;
      // 重连中不是「离线」：给和「连接中」同一种呼吸点，别让用户以为已经放弃了。
      const reconnecting = !p.online && !!p.reconnecting;
      r.dot.className = 'dot ' + (p.online ? 'online' : reconnecting ? 'connecting' : 'offline');
      r.meta.textContent = `${p.last_addr ? '最近地址 ' + p.last_addr : '暂无地址记录'} · 默认端口 ${p.port}`;
      const fp = p.fingerprint;
      r.reconnect.hidden = !reconnecting;
      if (reconnecting) {
        anchorRetry(fp, p.retry_in_s);
        r.reconnectText.textContent = reconnectLabel(fp);
      } else {
        retryAnchor.delete(fp);
      }
      r.alias.hidden = !p.alias;
      if (p.alias) r.alias.textContent = `别名 · ${p.name || fp}`;

      // 模式 B：整排通路开关下线，换成这台对端的两台系统设备。
      // monitorPref / bridgePref 在模式 B 下**不读不写**（数据保留，切回 A 复用）。
      r.toggles.hidden = modeB;
      r.devices.node.hidden = !modeB;
      if (modeB) r.devices.apply(p, s.daemon);

      const micS = findSess(s, fp, 'mic');
      const spkS = findSess(s, fp, 'spk');
      if (!modeB) {
        setSwitch(r.micSw, !!micS);
        setSwitch(r.spkSw, !!spkS);
        setSwitch(r.monSw, !!s.monitorPref[fp]);
        // pending 从 busy 集合重建：卡片可能在操作进行中被整体重建
        const micBusy = busy.has(busyKey(fp, 'mic'));
        setPending(r.micSw, micBusy);
        setPending(r.monSw, micBusy);
        setPending(r.spkSw, busy.has(busyKey(fp, 'spk')));
        r.vol.apply(spkS);
        r.bridge.apply(s.daemon, s.bridgePref[fp] || '', micBusy);
      }
      // 电平条两种模式都要：模式 B 的会话由系统的设备选择创建，那条流一样是这台
      // 对端在收发，藏起来只会让「选了设备但没声音」少一个可看的地方。
      updStream(r.mic, micS);
      updStream(r.spk, spkS);

      const inbound = inboundMic(s, fp);
      r.inbound.hidden = inbound.length === 0;
      if (inbound.length) {
        r.inboundText.textContent = inbound.length > 1
          ? `对方正在取用本机麦克风（${inbound.length} 路）`
          : '对方正在取用本机麦克风';
      }
    }
  }

  // 倒计时只重写文本：整轮 update 每秒跑一次会把音量滑块从手指下重建掉。
  const retryTimer = setInterval(() => {
    for (const [fp, r] of cards) {
      if (!r.reconnect.hidden) r.reconnectText.textContent = reconnectLabel(fp);
    }
  }, 1000);

  const unsub = store.subscribe(update);
  update(store.state);
  return () => {
    unsub();
    clearInterval(retryTimer);
    for (const r of cards.values()) r.vol.destroy();
    cards.clear();
    retryAnchor.clear(); // 模块级 Map：不清就跨越挂载活下来，回到本页时倒计时是旧的
  };
}
