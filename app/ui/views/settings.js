// 设置：端口只读、延迟/质量档与虚拟设备开关（M4b 占位）、配置目录展示。

import { store } from '../store.js';
import { el, icon, switchBtn, setSwitch, extLink, toast, openExternal } from '../ui.js';
import { bridgeCatalog, vendors } from '../bridge.js';
import { actionOf } from '../permissions.js';
import { permissionRow } from './onboarding.js';

// options[].disabled 是**每次 sync 都重新求值**的谓词，不是建控件时的一次性快照：
// 模式 B 的可用性取决于 daemon 有没有连上 HAL 驱动，而那要等第一个 status 回包才
// 知道，装/卸驱动后还会再变。wrap.sync 因此挂在元素上，供 update() 复位。
function segmented(testid, options, get, set) {
  const wrap = el('div', { class: 'segmented', role: 'radiogroup', 'data-testid': testid });
  const btns = options.map((o) => {
    const b = el('button', { class: 'seg', type: 'button', role: 'radio', 'data-value': o.value }, o.label);
    b.addEventListener('click', () => {
      if (b.disabled) return;
      set(o.value);
      sync();
    });
    return b;
  });
  function sync() {
    const v = get();
    for (let i = 0; i < btns.length; i += 1) {
      const b = btns[i];
      const off = typeof options[i].disabled === 'function' && options[i].disabled();
      b.disabled = off;
      b.classList.toggle('off', off);
      b.title = off ? (options[i].why || '') : '';
      const on = !off && b.dataset.value === v;
      b.classList.toggle('on', on);
      b.setAttribute('aria-checked', String(on));
    }
  }
  wrap.append(...btns);
  wrap.sync = sync;
  sync();
  return wrap;
}

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

  // 使用端模式（plan §7.1）：A/B 并列、独立选择，B 在驱动不可用时置灰。
  // 判据取 daemon.status 的 hal.driver_connected——注册了名字但没连上（驱动装了、
  // coreaudiod 还没加载完，或插件版本对不上）同样用不了 B，所以只认 connected。
  const halOk = () => !!(store.state.daemon && store.state.daemon.hal
    && store.state.daemon.hal.driver_connected);
  const modeSeg = segmented('settings-consumer-mode',
    [
      { value: 'a', label: 'A · 免驱动' },
      {
        value: 'b',
        label: 'B · 虚拟设备',
        disabled: () => !halOk(),
        why: '未检测到 AudioHub 驱动，无法使用模式 B',
      },
    ],
    // 存过 'b' 不等于现在能用 B（换机器、卸了驱动都会变），生效模式因此在这里回落。
    () => (store.state.settings.consumerMode === 'b' && halOk() ? 'b' : 'a'),
    (v) => { store.update((x) => { x.settings.consumerMode = v; }); store.saveSettings(); });
  const modeNote = el('p', { class: 'muted small', 'data-testid': 'settings-mode-note' });
  const modeCard = el('section', { class: 'card block', 'data-testid': 'settings-mode' },
    el('h3', { class: 'block-title' }, '使用端模式'),
    settingRow('模式',
      'A：不装驱动，捕获本机系统音频送到对端播放——本机与对端同时发声；'
      + '取用对端麦克风需借助已安装的第三方虚拟声卡（见下方「虚拟声卡桥接」）。'
      + 'B：由 AudioHub 驱动在系统音频设备列表中注入一对虚拟设备，任意应用直接选用，'
      + '调节虚拟设备音量即调节对端真实设备。',
      modeSeg, null),
    modeNote);

  const portVal = el('code', { class: 'mono', 'data-testid': 'settings-port' }, '—');
  const ipcVal = el('code', { class: 'mono', 'data-testid': 'settings-ipc-port' }, '—');
  const netCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '网络'),
    settingRow('控制端口', 'daemon 对外的 TCP 控制端口（TLS + 指纹校验）。M4a 为只读展示，暂不支持修改。',
      portVal, '只读 · M4a'),
    settingRow('IPC 端口', '本机回环 WebSocket 端口，随 daemon 启动随机分配，写入 ipc.json。',
      ipcVal, null));

  const latencySeg = segmented('settings-latency',
    [{ value: 'lowest', label: '最低延迟' }, { value: 'auto', label: 'AUTO' }],
    () => store.state.settings.latency,
    (v) => { store.update((x) => { x.settings.latency = v; }); store.saveSettings(); });
  const qualitySeg = segmented('settings-quality',
    [{ value: 'pcm', label: 'PCM' }, { value: 'auto', label: 'AUTO' }],
    () => store.state.settings.quality,
    (v) => { store.update((x) => { x.settings.quality = v; }); store.saveSettings(); });

  const transportCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '传输'),
    settingRow('延迟档', '最低：固定最小缓冲，追求最低听感延迟；AUTO：按网络质量自适应加深缓冲。推荐保持最低。',
      latencySeg, '即将生效于 M4b'),
    settingRow('质量档', 'PCM：无损 PCM_S16LE 固定码率；AUTO：按丢包与带宽在质量阶梯（rung）上自动升降。',
      qualitySeg, '即将生效于 M4b'),
    el('p', { class: 'muted small' }, '以上选择当前仅保存于本机 UI，IPC 尚无 settings.* 方法，不会下发到 daemon。'));

  const removeSw = switchBtn({
    testid: 'settings-remove-virtual',
    label: '断开后移除虚拟设备',
    checked: s.settings.removeVirtual,
    onToggle(want, b) {
      store.update((x) => { x.settings.removeVirtual = want; });
      store.saveSettings();
      setSwitch(b, want);
    },
  });
  const deviceCard = el('section', { class: 'card block' },
    el('h3', { class: 'block-title' }, '虚拟设备'),
    settingRow('断开后移除虚拟设备',
      '关闭时：断开仅显示离线，虚拟设备保留在系统设备列表；开启时：断开即移除。解除配对总是无条件移除。',
      removeSw, '即将生效于 M4b'));

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

  function renderModeNote(st) {
    modeSeg.sync();
    const hal = st.daemon ? st.daemon.hal : null;
    if (!st.daemon) {
      modeNote.textContent = '服务未连接，暂时无法判断驱动是否可用。';
      return;
    }
    if (hal && hal.driver_connected) {
      modeNote.textContent = '已连接 AudioHub 驱动，模式 B 可用。';
      return;
    }
    // 装了但没连上和压根没装，用户要做的事不一样，不能合并成一句话。
    modeNote.textContent = hal && hal.registered
      ? '检测到驱动已注册但尚未连接：请稍候，或重启 AudioHub 服务后重试。'
      : '未检测到 AudioHub 驱动，模式 B 不可用；安装驱动后重启本应用即可选择。';
  }

  function update(st) {
    portVal.textContent = st.daemon ? String(st.daemon.control_port) : '—';
    ipcVal.textContent = st.endpoint ? String(st.endpoint.port) : '—';
    setSwitch(removeSw, st.settings.removeVirtual);
    renderModeNote(st);
    renderBridgeStatus(st);
    perms.render(st);
  }

  const unsub = store.subscribe(update);
  update(store.state);
  return () => unsub();
}
