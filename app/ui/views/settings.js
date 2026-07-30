// 设置：模式镜像、端口只读、延迟/质量档、虚拟设备开关与设备清单、配置目录展示。
//
// 模式**选择器本身**不在这一页（plan §7.1：它决定整个主面板的含义，藏进设置页
// 等于把「那排开关为什么没了」的答案藏起来）。这里只放一面只读的镜子 + 去主面板
// 的入口，避免同一个全局状态出现两个可点的控件、两处 pending 态。

import { store } from '../store.js';
import { el, icon, switchBtn, setSwitch, setPending, extLink, toast, openExternal, segmented, fmt } from '../ui.js';
import { bridgeCatalog, vendors } from '../bridge.js';
import { actionOf } from '../permissions.js';
import { permissionRow } from './onboarding.js';
import { halState, requestedMode, effectiveMode, isModeB, modeDowngraded, DEVICE_STATE_LABEL } from '../mode.js';

const MODE_LABEL = { a: 'A · 免驱动', b: 'B · 虚拟设备' };

function settingRow(title, desc, control, badge) {
  return el('div', { class: 'setting-row' },
    el('div', { class: 'setting-text' },
      el('div', { class: 'setting-title' }, title, badge ? el('span', { class: 'tag warn' }, badge) : null),
      el('p', { class: 'setting-desc' }, desc)),
    el('div', { class: 'setting-ctl' }, control));
}

// 授权门只在「必需权限没齐」时出现，可选的系统音频录制因此可能永远等不到那扇门。
// 这张卡就是它进门之后唯一的入口——也是跳过授权的用户回头补授权的地方。
function permissionsCard(ctx) {
  const rows = new Map();
  const list = el('div', { class: 'perm-list', 'data-testid': 'settings-perm-list' });
  const note = el('p', { class: 'muted small', 'data-testid': 'settings-perm-note' });
  const recheck = el('button', { class: 'btn small', type: 'button', 'data-testid': 'settings-perm-recheck' },
    '重新检查');
  recheck.addEventListener('click', () => ctx && ctx.refreshPermissions && ctx.refreshPermissions({ force: true }));

  async function onAction(p) {
    if (actionOf(p) === 'request') {
      store.setPermissionBusy(p.id);
      try {
        const res = await ctx.rpc('daemon.request_permission', { id: p.id });
        await ctx.refreshPermissions({ force: true, seed: res });
      } catch (_) { /* rpc 已 toast */ } finally {
        store.setPermissionBusy(null);
      }
      return;
    }
    if (p.settingsUrl) {
      openExternal(p.settingsUrl);
      if (p.manual) toast(`若系统设置没有自动打开：${p.manual}`, 'info');
    } else {
      toast(p.manual ? `请手动前往：${p.manual}` : '本机服务未提供系统设置入口。', 'warn');
    }
  }

  const card = el('section', { class: 'card block', 'data-testid': 'settings-permissions' },
    el('h3', { class: 'block-title' }, '系统权限'),
    el('p', { class: 'muted' },
      'macOS 的规则是：一项权限被拒绝后，应用无法再次弹窗询问，只能到系统设置里手动打开。'
      + '这里显示的是本机服务实时探测到的状态，不是记住的旧结果。'),
    list, note, recheck);

  function render(st) {
    const perms = st.permissions;
    const seen = new Set();
    for (const p of perms.list) {
      seen.add(p.id);
      let row = rows.get(p.id);
      if (!row) {
        row = permissionRow({ prefix: 'settings-perm', onAction });
        rows.set(p.id, row);
        list.append(row.node);
      }
      row.update(p, perms.busy, false);
    }
    for (const [id, row] of rows) {
      if (!seen.has(id)) { row.node.remove(); rows.delete(id); }
    }
    list.hidden = perms.list.length === 0;
    note.textContent = perms.list.length ? ''
      : perms.supported === false
        ? '当前服务不提供权限查询接口（daemon 版本较旧），无法在此显示或申请权限。'
        : perms.error ? `权限探测失败：${perms.error}`
          : st.conn === 'online' ? '正在探测系统权限…' : '服务未连接，暂无法探测权限状态。';
    note.hidden = !note.textContent;
  }

  return { card, render };
}

export function mount(root, ctx) {
  const s = store.state;

  // daemon 的值优先，拿不到才回落到本地缓存——反过来会让界面显示一个 daemon
  // 根本不认的档位。
  function settingValue(key, dft) {
    const d = store.state.daemonSettings;
    if (d && typeof d[key] === 'string' && d[key]) return d[key];
    const local = store.state.settings[key];
    return typeof local === 'string' && local ? local : dft;
  }

  // 全部写操作走同一条路：回包就是新的权威值，不做乐观翻转——开关先翻过去、
  // 请求再失败的话，界面显示的是一个 daemon 从没接受过的设置。
  let writing = 0;
  async function pushSetting(patch, sw) {
    writing += 1;
    if (sw) setPending(sw, true);
    try {
      await ctx.applySettings(patch);
    } catch (_) { /* rpc 已 toast */ } finally {
      writing -= 1;
      if (sw) setPending(sw, false);
      update(store.state); // 成功按回包重画，失败按旧值复位
    }
  }

  // 只读镜子：值取 daemon 的 effective_mode，切换入口在主面板。
  const modeCur = el('span', { class: 'mode-mirror', 'data-testid': 'settings-mode-current' }, '—');
  const modeGoto = el('button', { class: 'btn small', type: 'button', 'data-testid': 'settings-mode-goto' },
    '前往主面板切换');
  modeGoto.addEventListener('click', () => ctx.navigate('peers'));
  const modeNote = el('p', { class: 'muted small', 'data-testid': 'settings-mode-note' });
  const modeCard = el('section', { class: 'card block', 'data-testid': 'settings-mode' },
    el('h3', { class: 'block-title' }, '使用端模式'),
    settingRow('当前模式',
      'A：不装驱动，捕获本机系统音频送到对端播放——本机与对端同时发声；'
      + '取用对端麦克风需借助已安装的第三方虚拟声卡（见下方「虚拟声卡桥接」）。'
      + 'B：每台已配对主机作为一对设备出现在系统音频设备列表中，任意应用直接选用，'
      + '调节该设备音量即调节对端真实设备。'
      + '模式是全局设置，由本机服务持有；切换入口在主面板顶部。',
      el('div', { class: 'field-btn' }, modeCur, modeGoto), null),
    modeNote);

  const portVal = el('code', { class: 'mono', 'data-testid': 'settings-port' }, '—');
  const ipcVal = el('code', { class: 'mono', 'data-testid': 'settings-ipc-port' }, '—');
  const netCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '网络'),
    settingRow('控制端口', 'daemon 对外的 TCP 控制端口（TLS + 指纹校验）。M4a 为只读展示，暂不支持修改。',
      portVal, '只读 · M4a'),
    settingRow('IPC 端口', '本机回环 WebSocket 端口，随 daemon 启动随机分配，写入 ipc.json。',
      ipcVal, null));

  // 延迟/质量：**真的下发并落盘**（settings.json），但媒体面还没读它——两者都还
  // 由 AUTO 阶梯决定。角标写「已保存 · 暂未生效」而不是隐藏：藏起来会让下一版
  // 接上时用户以为是新功能，而这里保存的值那时会突然开始起作用。
  const latencySeg = segmented('settings-latency',
    [{ value: 'min', label: '最低延迟' }, { value: 'auto', label: 'AUTO' }],
    () => settingValue('latency', 'min'),
    (v) => pushSetting({ latency: v }));
  const qualitySeg = segmented('settings-quality',
    [{ value: 'pcm', label: 'PCM' }, { value: 'auto', label: 'AUTO' }],
    () => settingValue('quality', 'auto'),
    (v) => pushSetting({ quality: v }));

  const transportCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '传输'),
    settingRow('延迟档', '最低：固定最小缓冲，追求最低听感延迟；AUTO：按网络质量自适应加深缓冲。推荐保持最低。',
      latencySeg, '已保存 · 暂未生效'),
    settingRow('质量档', 'PCM：无损 PCM_S16LE 固定码率；AUTO：按丢包与带宽在质量阶梯（rung）上自动升降。',
      qualitySeg, '已保存 · 暂未生效'),
    el('p', { class: 'muted small' },
      '两档已随 settings.set 下发并由本机服务持久化，但媒体面尚未读取它们：'
      + '当前编解码与缓冲深度仍由 AUTO 阶梯自行决定。'));

  const removeSw = switchBtn({
    testid: 'settings-remove-virtual',
    label: '断开后移除虚拟设备',
    checked: s.settings.removeVirtual,
    onToggle: (want, b) => pushSetting({ remove_virtual_on_disconnect: want }, b),
  });
  const offlineSw = switchBtn({
    testid: 'settings-mark-offline',
    label: '离线时在设备名后标注（离线）',
    onToggle: (want, b) => pushSetting({ mark_offline_devices: want }, b),
  });

  const devCount = el('span', { class: 'dev-count', 'data-testid': 'settings-hal-count' }, '—');
  const devList = el('div', { class: 'dev-inventory', 'data-testid': 'settings-hal-devices' });
  const devNote = el('p', { class: 'muted small', 'data-testid': 'settings-hal-note' });

  const deviceCard = el('section', { class: 'card block', 'data-testid': 'settings-devices' },
    el('h3', { class: 'block-title' }, '虚拟设备'),
    settingRow('断开后移除虚拟设备',
      '关闭时：断开仅显示离线，虚拟设备保留在系统设备列表；开启时：断开即移除，重连后以相同 UID 恢复。'
      + '解除配对总是无条件移除。',
      removeSw, null),
    settingRow('离线时标注设备名',
      '开启时，对端断开期间设备名后追加「（离线）」——同一 UID 就地改名，'
      + '不影响任何应用已记住的设备选择。关闭则名字恒定，代价是「没声音」在系统里无从分辨。',
      offlineSw, null),
    el('div', { class: 'dev-inventory-head' },
      el('span', { class: 'dev-inventory-title' }, '设备清单'), devCount),
    devList, devNote);

  // 虚拟声卡桥接（spec-m4c §B / plan §7.1）：这里只报「检测到了什么」并给官网链接，
  // 真正的选择在主面板的对端卡片上——桥接目标是**按对端**决定的。
  // 冻结的口径：不代装、不主动引导安装，所以这里没有任何安装按钮或催促文案。
  const bridgeStatus = el('div', { class: 'bridge-status', 'data-testid': 'settings-bridge-status' });
  const bridgeLinks = el('div', { class: 'bridge-links', 'data-testid': 'settings-bridge-links' });
  for (const v of vendors()) bridgeLinks.append(extLink(v.label, v.url, `settings-bridge-link-${v.id}`));
  const bridgeCard = el('section', { class: 'card block', 'data-testid': 'settings-bridge' },
    el('h3', { class: 'block-title' }, '虚拟声卡桥接'),
    el('p', { class: 'muted' },
      '取用对端麦克风时，可把音频写入本机已安装的第三方虚拟声卡的播放端；'
      + '任意应用选择该声卡的输入端，就等于选中了对端的麦克风。'),
    bridgeStatus,
    el('p', { class: 'muted small' },
      'AudioHub 不会替你安装任何驱动：这些虚拟声卡由第三方签名与维护，'
      + '安装后重新打开本应用即可被检测到。选择哪一张卡在主面板的对端卡片上单独设置。'),
    bridgeLinks);

  const isMac = /mac/i.test(navigator.platform || '') || /Macintosh/i.test(navigator.userAgent || '');
  const cfgDir = isMac ? '~/Library/Application Support/AudioHub' : '%APPDATA%\\AudioHub';
  const pathCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '路径'),
    settingRow('配置目录', 'daemon 身份、配对表与 ipc.json 所在目录；可用环境变量 AUDIOHUB_CONFIG_DIR 覆盖。',
      el('code', { class: 'mono', 'data-testid': 'settings-config-dir' }, cfgDir), null));

  const perms = permissionsCard(ctx);

  root.append(perms.card, modeCard, netCard, transportCard, bridgeCard, deviceCard, pathCard);

  let statusKey = null;

  function renderBridgeStatus(st) {
    const catalog = bridgeCatalog(st.daemon);
    const key = JSON.stringify(catalog);
    if (key === statusKey) return;
    statusKey = key;
    bridgeStatus.innerHTML = '';
    if (catalog == null) {
      bridgeStatus.append(el('p', { class: 'muted small', 'data-testid': 'settings-bridge-none' },
        st.daemon ? '当前服务未上报虚拟声卡信息（daemon.status 无 virtual_cards）。' : '服务未连接，暂无检测结果。'));
      return;
    }
    if (!catalog.length) {
      bridgeStatus.append(el('p', { class: 'muted small', 'data-testid': 'settings-bridge-none' },
        '未检测到任何虚拟声卡。'));
      return;
    }
    for (const c of catalog) {
      // present 但不在 output_devices 里：装是装了，daemon 却打不开它，
      // 说「已检测到」就成了骗人。
      const tag = c.usable ? el('span', { class: 'tag ok' }, '已检测到')
        : c.present ? el('span', { class: 'tag warn' }, '不在输出列表')
          : el('span', { class: 'tag' }, '未检测到');
      bridgeStatus.append(el('div', {
        class: 'bridge-status-row' + (c.usable ? ' on' : ''),
        'data-testid': `settings-bridge-card-${c.id}`,
      }, icon('cable'), el('span', { class: 'bridge-status-name' }, c.name), tag));
    }
  }

  function renderMode(st) {
    const eff = effectiveMode(st);
    modeCur.textContent = MODE_LABEL[eff] || eff;
    modeCur.className = 'mode-mirror mode-' + eff;
    const hs = halState(st.daemon);
    modeNote.textContent = modeDowngraded(st)
      ? `你选择的是「${MODE_LABEL[requestedMode(st)]}」，但当前不可用，已临时按模式 A 运行。${hs.text}`
      : hs.text;
    modeNote.className = `muted small tone-${hs.tone}`;
  }

  let devKey = null;

  function renderDevices(st) {
    const ds = st.daemonSettings;
    const hal = st.daemon ? st.daemon.hal : null;
    const list = hal && Array.isArray(hal.devices) ? hal.devices : [];
    const cap = ds ? ds.hal_capacity : (hal ? 16 : 0);
    const used = ds ? ds.hal_used : list.length;
    devCount.textContent = cap ? `已用 ${used} / ${cap}` : '不可用';

    const key = JSON.stringify([list, used, cap, isModeB(st)]);
    if (key === devKey) return;
    devKey = key;

    devList.innerHTML = '';
    for (const d of list) {
      const peer = st.peers.find((p) => p.fingerprint === d.fingerprint);
      const owner = (peer && (peer.display_name || peer.name)) || d.fingerprint.slice(0, 12);
      // state 与 observed 是两件事：前者是驱动应答了我们，后者是系统真的列出了它。
      // 只报前者，就会把「发过 Bind 但设备没出现」显示成一切正常——这恰恰是本轮
      // 引入闭环观测要抓的那种故障。
      const published = d.state === 'bound' && d.observed;
      const tag = published
        ? el('span', { class: 'tag ok' }, '已发布')
        : d.state === 'bound'
          ? el('span', { class: 'tag warn' }, '未出现在系统中')
          : el('span', { class: 'tag' }, DEVICE_STATE_LABEL[d.state] || d.state);

      const rows = [
        { dir: 'out', ico: 'spk', name: d.out_name, uid: d.out_uid, io: d.io_out, frames: d.spk_frames, drop: null },
        { dir: 'in', ico: 'mic', name: d.in_name, uid: d.in_uid, io: d.io_in, frames: d.mic_frames, drop: d.mic_dropped },
      ].map((r) => el('div', { class: 'dev-inv-row', 'data-testid': `settings-hal-${r.dir}-${d.fingerprint}` },
        icon(r.ico, 'ico dev-ico'),
        el('div', { class: 'dev-text' },
          el('span', { class: 'dev-name' }, r.name || '—'),
          el('code', { class: 'dev-uid mono' }, r.uid || '')),
        el('span', { class: 'dev-frames mono' },
          `${fmt.int(r.frames)} 帧` + (r.drop ? ` · 丢 ${fmt.int(r.drop)}` : '')),
        el('span', { class: 'dev-state ' + (r.io ? 'live' : 'idle') }, r.io ? '● 使用中' : '○ 未使用')));

      devList.append(el('div', { class: 'dev-inv-card', 'data-testid': `settings-hal-device-${d.fingerprint}` },
        el('div', { class: 'dev-inv-head' },
          el('strong', {}, owner),
          el('code', { class: 'mono dim' }, `槽位 ${d.slot} · 代号 ${d.generation}`),
          d.peer_connected ? el('span', { class: 'tag ok' }, '在线') : el('span', { class: 'tag' }, '离线'),
          tag),
        rows));
    }

    devList.hidden = list.length === 0;
    devNote.textContent = list.length
      ? '「已发布」= 驱动确认绑定且系统的设备列表里确实能查到这两个 UID。'
      : !hal
        ? '本机未安装 AudioHub 驱动（或服务未加载桥接），没有虚拟设备。'
        : isModeB(st)
          ? '当前没有任何虚拟设备：配对一台对端后，它会立刻出现在系统音频设备列表里。'
          : '当前是模式 A：虚拟设备只在模式 B 下存在。';
  }

  function update(st) {
    portVal.textContent = st.daemon ? String(st.daemon.control_port) : '—';
    ipcVal.textContent = st.endpoint ? String(st.endpoint.port) : '—';
    const ds = st.daemonSettings;
    setSwitch(removeSw, ds ? ds.remove_virtual_on_disconnect : st.settings.removeVirtual);
    setSwitch(offlineSw, ds ? ds.mark_offline_devices : true);
    // 没有 settings.* 的旧服务：开关点了也不会有任何效果，禁用比假装能用诚实。
    // 正在写的时候不碰 disabled——那是 setPending 的地盘，抢过来会让 pending 态
    // 在第一次 store.emit 时就被清掉。
    if (!writing) {
      const noSettings = st.settingsSupported === false;
      for (const b of [removeSw, offlineSw]) b.disabled = noSettings;
    }
    latencySeg.sync();
    qualitySeg.sync();
    renderMode(st);
    renderDevices(st);
    // 第三方虚拟声卡与「AudioHub – X 麦克风」做的是同一件事：模式 B 下整张卡下线，
    // 否则用户会以为还得再装一张卡才能用对端麦克风。
    bridgeCard.hidden = isModeB(st);
    renderBridgeStatus(st);
    perms.render(st);
  }

  const unsub = store.subscribe(update);
  update(store.state);
  return () => unsub();
}
