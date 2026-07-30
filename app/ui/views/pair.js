// 配对向导：左「我要被发现」（PIN + 倒计时环），右「我要连别人」（发现列表 + 手动发起占位）。

import { store } from '../store.js';
import { el, svgEl, icon, fmt, toast, sleep } from '../ui.js';

const PAIR_TTL_S = 120;
const RING_R = 30;
const RING_C = 2 * Math.PI * RING_R;
// 单次短扫描之间的间隔：给共享的那一条 IPC 连接留出处理其它请求的空隙。
const SCAN_GAP_MS = 400;

function discKey(d) {
  return d.fingerprint || `${d.instance || 'unknown'}-${d.port}`;
}

export function mount(root, ctx) {
  let alive = true;

  // ---- 左栏：我要被发现 ----

  const pinBox = el('div', { class: 'pin-display', 'data-testid': 'pin-display' });
  let lastPin = null;

  const ringFg = svgEl('circle', {
    class: 'ring-fg', cx: 36, cy: 36, r: RING_R,
    'stroke-dasharray': RING_C.toFixed(2), 'stroke-dashoffset': 0,
  });
  const ringText = svgEl('text', { class: 'ring-text', x: 36, y: 41, 'text-anchor': 'middle' }, '--');
  const ringWrap = el('div', { class: 'ring-wrap' },
    svgEl('svg', { class: 'ring', viewBox: '0 0 72 72', 'data-testid': 'pin-countdown' },
      svgEl('circle', { class: 'ring-bg', cx: 36, cy: 36, r: RING_R }),
      ringFg, ringText));

  const enableBtn = el('button', { class: 'btn primary big', type: 'button', 'data-testid': 'pairing-enable' }, '开启配对模式');
  const disableBtn = el('button', { class: 'btn', type: 'button', 'data-testid': 'pairing-disable' }, '停止配对');

  const idleBox = el('div', { class: 'pair-idle' },
    el('p', { class: 'muted' }, '开启后，本机将在局域网内可被发现（pairing.enable），并生成一次性 PIN 供对方输入。'),
    enableBtn);
  const activeBox = el('div', { class: 'pair-active', hidden: true },
    ringWrap, pinBox,
    el('p', { class: 'pair-tip' }, '请对方在其配对界面输入以上 PIN 完成双向信任。'),
    disableBtn);

  const leftCard = el('section', { class: 'card block pair-col' },
    el('h3', { class: 'block-title' }, '我要被发现'),
    idleBox, activeBox);

  enableBtn.addEventListener('click', async () => {
    enableBtn.disabled = true;
    try {
      const res = await ctx.rpc('pairing.enable', { ttl_s: PAIR_TTL_S });
      const pin = String((res && res.pin) ?? '');
      store.update((s) => {
        s.pairing = { pin, ttlS: PAIR_TTL_S, expiresAt: Date.now() + PAIR_TTL_S * 1000 };
      });
    } catch (_) { /* rpc 已 toast */ } finally {
      enableBtn.disabled = false;
    }
  });

  disableBtn.addEventListener('click', async () => {
    try { await ctx.rpc('pairing.disable', {}); } catch (_) { /* ignore */ }
    store.update((s) => { s.pairing = null; });
  });

  function renderPin(pin) {
    pinBox.textContent = '';
    [...String(pin)].forEach((ch, i) => {
      const d = el('span', { class: 'pin-digit' }, ch);
      d.style.setProperty('--i', String(i)); // CSSOM，避免内联 style 属性（CSP）
      pinBox.append(d);
    });
  }

  const ringTimer = setInterval(() => {
    const p = store.state.pairing;
    if (!p) return;
    const remain = (p.expiresAt - Date.now()) / 1000;
    if (remain <= 0) {
      store.update((s) => { s.pairing = null; });
      toast('配对模式已到期', 'info');
      return;
    }
    ringFg.style.strokeDashoffset = String(RING_C * (1 - remain / p.ttlS));
    ringText.textContent = String(Math.ceil(remain));
  }, 250);

  // ---- 右栏：我要连别人 ----

  const scanBtn = el('button', { class: 'btn', type: 'button', 'data-testid': 'discover-run' }, icon('scan'), '开始扫描');
  const scanSpin = el('span', { class: 'spinner', hidden: true });
  const discList = el('div', { class: 'disc-list', 'data-testid': 'discover-list' });
  const discEmpty = el('p', { class: 'muted', 'data-testid': 'discover-empty' },
    '尚未发现主机。点击「开始扫描」在局域网内查找（discover.run）。');

  const addrIn = el('input', {
    class: 'input', 'data-testid': 'manual-pair-addr',
    placeholder: 'IP 或 IP:端口', autocomplete: 'off', spellcheck: 'false',
  });
  const pinIn = el('input', {
    class: 'input pin-input', 'data-testid': 'manual-pair-pin',
    placeholder: '对方 PIN', inputmode: 'numeric', maxlength: '8', autocomplete: 'off',
  });
  const goBtn = el('button', {
    class: 'btn primary looks-disabled', type: 'button',
    'data-testid': 'manual-pair-btn',
    title: '发起端配对暂无 IPC 方法（M4a），本期请使用 CLI',
  }, '发起配对');
  const cliHint = el('p', { class: 'cli-hint', 'data-testid': 'pair-cli-hint', hidden: true });

  goBtn.addEventListener('click', async () => {
    if (ctx.isTauri()) {
      try { await ctx.ensureDaemon(); } catch (_) { /* 提示为主，失败不阻断 */ }
    }
    const addr = addrIn.value.trim() || '<addr>';
    const pin = pinIn.value.trim() || '<pin>';
    cliHint.hidden = false;
    cliHint.textContent = `发起端配对暂无 IPC 方法（M4a）。当前版本请用 CLI：audiohub pair --to ${addr} --pin ${pin}`;
  });

  const steps = ['建立连接', '校验 PIN', '交换密钥', '完成配对'];
  const progress = el('ol', { class: 'pair-steps', 'data-testid': 'pair-progress' },
    steps.map((t) => el('li', {}, el('span', { class: 'step-dot' }), t)));

  const rightCard = el('section', { class: 'card block pair-col' },
    el('h3', { class: 'block-title' }, '我要连别人'),
    el('div', { class: 'scan-row' }, scanBtn, scanSpin),
    discList, discEmpty,
    el('div', { class: 'divider' }),
    el('div', { class: 'manual-pair' },
      el('div', { class: 'form-row' },
        el('label', { class: 'field grow' }, el('span', { class: 'field-label' }, '对方地址'), addrIn),
        el('label', { class: 'field' }, el('span', { class: 'field-label' }, 'PIN'), pinIn),
        el('span', { class: 'field-btn' }, goBtn, el('span', { class: 'tag warn' }, '本期 CLI 限定'))),
      cliHint,
      progress,
      el('p', { class: 'muted small' }, '配对进度为占位展示，待发起端 IPC（M4b）接入后启用。')));

  root.append(el('div', { class: 'pair-grid' }, leftCard, rightCard));

  // 循环短扫描（discover.run {secs:2}）合并结果，近似实时刷进。
  // scanGen 是唯一的"谁还有效"判据：反复点按钮只会作废旧循环，绝不叠加并发循环——
  // 多个 discover.run 会在同一条 IPC 连接上串成队头阻塞，把其它请求全拖到超时。
  let scanGen = 0;
  let scanChain = null;

  async function scanLoop(gen) {
    while (alive && gen === scanGen && store.state.discover.running) {
      try {
        const res = await ctx.rpc('discover.run', { secs: 2 }, { silent: true, timeoutMs: 15000 });
        if (!alive || gen !== scanGen) break;
        mergeResults(res);
      } catch (_) {
        if (!alive || gen !== scanGen) break;
        await sleep(1000);
        continue;
      }
      await sleep(SCAN_GAP_MS);
    }
  }

  function startScan() {
    const gen = ++scanGen; // 作废上一轮
    const prev = scanChain;
    // 串到上一轮之后，保证同一时刻只有一个循环体在跑（旧循环在下个 await 边界退出）
    scanChain = (async () => {
      if (prev) await prev.catch(() => {});
      if (!alive || gen !== scanGen || !store.state.discover.running) return;
      await scanLoop(gen);
    })();
  }

  function stopScan() {
    scanGen++;
    store.update((s) => { s.discover.running = false; });
  }

  function mergeResults(list) {
    if (!Array.isArray(list)) return;
    store.update((s) => {
      for (const d of list) {
        const key = discKey(d);
        const i = s.discover.results.findIndex((x) => discKey(x) === key);
        const entry = { ...d, lastSeen: Date.now() };
        if (i >= 0) s.discover.results[i] = entry;
        else s.discover.results.push(entry);
      }
      if (s.discover.results.length > 50) s.discover.results.length = 50;
    });
  }

  scanBtn.addEventListener('click', () => {
    if (store.state.discover.running) {
      stopScan();
      return;
    }
    store.update((s) => { s.discover.running = true; });
    startScan();
  });

  function update(s) {
    // 左栏状态
    const p = s.pairing;
    idleBox.hidden = !!p;
    activeBox.hidden = !p;
    if (p && p.pin !== lastPin) {
      lastPin = p.pin;
      renderPin(p.pin);
    }
    if (!p) lastPin = null;

    // 右栏扫描
    scanBtn.innerHTML = '';
    scanBtn.append(icon('scan'), s.discover.running ? '停止扫描' : '开始扫描');
    scanBtn.classList.toggle('primary', s.discover.running);
    scanSpin.hidden = !s.discover.running;

    discEmpty.hidden = s.discover.results.length > 0;
    discList.innerHTML = '';
    const sorted = s.discover.results.slice().sort((a, b) => (b.lastSeen || 0) - (a.lastSeen || 0));
    for (const d of sorted) {
      const key = discKey(d);
      const addr = d.addrs && d.addrs.length ? `${d.addrs[0]}:${d.port}` : `端口 ${d.port}`;
      const item = el('button', { class: 'disc-item card', type: 'button', 'data-testid': `discover-item-${key}` },
        el('div', { class: 'disc-main' },
          el('strong', {}, d.name || d.instance || '未知主机'),
          d.paired ? el('span', { class: 'tag ok' }, '已配对') : el('span', { class: 'tag' }, '未配对')),
        el('div', { class: 'disc-sub' },
          addr + (d.fingerprint ? ` · ${fmt.fp(d.fingerprint, 12)}` : '')));
      item.addEventListener('click', () => {
        if (d.addrs && d.addrs.length) addrIn.value = `${d.addrs[0]}:${d.port}`;
        pinIn.focus();
      });
      discList.append(item);
    }
  }

  const unsub = store.subscribe(update);
  update(store.state);

  return () => {
    alive = false;
    clearInterval(ringTimer);
    stopScan();
    unsub();
  };
}
