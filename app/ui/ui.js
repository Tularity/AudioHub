// 共享 UI 工具：DOM 构建、内联 SVG 图标、格式化、开关、电平表（rAF）、迷你折线、toast。

export const ACCENT = '#31c8b0';

export const REDUCED_MOTION =
  typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

// 刻意不支持任何形式的 innerHTML 注入：所有内容都以文本节点/子元素落地，
// 后续改动无法在这里造出 XSS 汇点（daemon 返回的 name/addr 都是外部输入）。
function applyAttrs(node, attrs) {
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith('on') && typeof v === 'function') node.addEventListener(k.slice(2), v);
    else if (v === true) node.setAttribute(k, '');
    else node.setAttribute(k, String(v));
  }
}

export function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  applyAttrs(node, attrs);
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

const SVG_NS = 'http://www.w3.org/2000/svg';

export function svgEl(tag, attrs = {}, ...children) {
  const node = document.createElementNS(SVG_NS, tag);
  applyAttrs(node, attrs);
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

const SVG_OPEN =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">';
const ICONS = {
  mic: '<path d="M12 3a3 3 0 0 1 3 3v5a3 3 0 0 1-6 0V6a3 3 0 0 1 3-3z"/><path d="M5 11a7 7 0 0 0 14 0"/><path d="M12 18v3"/>',
  spk: '<path d="M4 9v6h4l5 4V5L8 9H4z"/><path d="M16.5 8.5a5 5 0 0 1 0 7"/>',
  mute: '<path d="M4 9v6h4l5 4V5L8 9H4z"/><path d="M16.5 9.5l5 5m0-5l-5 5"/>',
  monitor: '<path d="M4 13a8 8 0 0 1 16 0"/><rect x="3" y="13" width="4" height="6" rx="1.5"/><rect x="17" y="13" width="4" height="6" rx="1.5"/>',
  peers: '<rect x="3" y="3" width="8" height="8" rx="2"/><rect x="13" y="3" width="8" height="8" rx="2"/><rect x="3" y="13" width="8" height="8" rx="2"/><rect x="13" y="13" width="8" height="8" rx="2"/>',
  pair: '<path d="M9 15l6-6"/><path d="M10.5 6.5L12 5a4 4 0 0 1 5.7 5.7l-1.5 1.5"/><path d="M13.5 17.5L12 19a4 4 0 0 1-5.7-5.7l1.5-1.5"/>',
  stats: '<path d="M4 5v14h16"/><path d="M8 15l3-4 3 2 4-6"/>',
  settings: '<path d="M5 7h14"/><circle cx="9" cy="7" r="2"/><path d="M5 17h14"/><circle cx="15" cy="17" r="2"/>',
  wave: '<path d="M3 12h2l2-5 3 10 3-14 3 12 2-3h3"/>',
  back: '<path d="M15 5l-7 7 7 7"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  scan: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="4"/><path d="M12 12l6.4-6.4"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  plug: '<path d="M9 6v4m6-4v4"/><path d="M7 10h10v2a5 5 0 0 1-10 0v-2z"/><path d="M12 17v4"/>',
  cable: '<rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><path d="M17.5 10v3.5a3 3 0 0 1-3 3H10"/><path d="M12 14.5l-2 2 2 2"/>',
  link: '<path d="M14 4h6v6"/><path d="M20 4l-8.5 8.5"/><path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"/>',
  shield: '<path d="M12 3l7 3v5.5c0 4.4-2.9 7.6-7 8.5-4.1-.9-7-4.1-7-8.5V6l7-3z"/><path d="M9 12l2 2 4-4"/>',
};

export function icon(name, cls = 'ico') {
  const span = document.createElement('span');
  span.className = cls;
  span.innerHTML = SVG_OPEN + (ICONS[name] || '') + '</svg>';
  return span;
}

// ---- 会话用途标签 ----

// 用途只能由 (kind, dir) 联合判定：daemon 存的 kind 是**发起方**视角的标签，
// 只有 dir 被翻成本机视角（core/audiohubd/src/conn.rs handle_remote_open）。
// 只看 kind 会把「对方在取用本机麦克风」显示成「取对方麦克风」——方向正好反了。
const SESSION_FLOW = {
  'mic|recv': { label: '取对方麦克风', short: '对方麦克风', inbound: false },
  'mic|send': { label: '对方取用本机麦克风', short: '本机麦克风', inbound: true },
  'spk|send': { label: '送对方扬声器', short: '对方扬声器', inbound: false },
  'spk|recv': { label: '对方送入本机扬声器', short: '本机扬声器', inbound: true },
};

export function sessionFlow(info) {
  const key = `${info && info.kind}|${info && info.dir}`;
  return SESSION_FLOW[key] || { label: key, short: key, inbound: false };
}

export const fmt = {
  fp: (fp, n = 8) => String(fp || '').slice(0, n),
  pct: (v) => (typeof v === 'number' && isFinite(v) ? v.toFixed(2) : '—'),
  ms: (v) => (typeof v === 'number' && isFinite(v) ? v.toFixed(2) : '—'),
  kbps: (v) => (typeof v === 'number' && isFinite(v) ? String(Math.round(v)) : '—'),
  int: (v) => (typeof v === 'number' && isFinite(v) ? String(Math.round(v)) : '—'),
  uptime(sec) {
    if (typeof sec !== 'number' || !isFinite(sec) || sec < 0) return '—';
    const s = Math.floor(sec);
    const d = Math.floor(s / 86400);
    const hh = String(Math.floor((s % 86400) / 3600)).padStart(2, '0');
    const mm = String(Math.floor((s % 3600) / 60)).padStart(2, '0');
    const ss = String(s % 60).padStart(2, '0');
    return d > 0 ? `${d} 天 ${hh}:${mm}:${ss}` : `${hh}:${mm}:${ss}`;
  },
  clock(ts) {
    try { return new Date(ts).toLocaleTimeString('zh-CN', { hour12: false }); } catch (_) { return '—'; }
  },
  date(unixS) {
    if (!unixS) return '—';
    try { return new Date(unixS * 1000).toLocaleString('zh-CN', { hour12: false }); } catch (_) { return '—'; }
  },
};

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// ---- 外部链接 ----

// 在 Tauri 里绝不能让 <a href> 走默认行为：webview 会把**应用界面本身**导航到外站，
// 而这个窗口没有后退按钮，用户就回不来了。依次尝试 opener / shell 插件与
// window.open；一个都不可用时把地址复制到剪贴板并说明，绝不静默失败。
export async function openExternal(url) {
  const t = window.__TAURI__ || {};
  for (const mod of [t.opener, t.shell]) {
    const fn = mod && (mod.openUrl || mod.open);
    if (typeof fn !== 'function') continue;
    try {
      await fn.call(mod, url);
      return true;
    } catch (_) { /* 换下一种 */ }
  }
  try {
    if (window.open(url, '_blank', 'noopener,noreferrer')) return true;
  } catch (_) { /* 继续兜底 */ }
  try {
    await navigator.clipboard.writeText(url);
    toast('已复制链接，请在浏览器中打开', 'info');
  } catch (_) {
    toast(`请在浏览器中打开：${url}`, 'warn');
  }
  return false;
}

export function extLink(text, url, testid) {
  const a = el('a', {
    class: 'ext-link', href: url, target: '_blank', rel: 'noopener noreferrer',
    title: url, 'data-testid': testid,
  }, text, icon('link', 'ico'));
  a.addEventListener('click', (e) => {
    e.preventDefault();
    e.stopPropagation();
    openExternal(url);
  });
  return a;
}

// ---- 开关（真实滑块） ----

export function switchBtn({ testid, label, checked = false, onToggle }) {
  const b = el('button', {
    class: 'switch' + (checked ? ' on' : ''),
    type: 'button',
    role: 'switch',
    'aria-checked': String(!!checked),
    'aria-label': label || '',
    'data-testid': testid,
  }, el('span', { class: 'knob' }));
  b.addEventListener('click', (e) => {
    e.stopPropagation();
    if (onToggle) onToggle(!b.classList.contains('on'), b);
  });
  return b;
}

export function setSwitch(b, on) {
  b.classList.toggle('on', !!on);
  b.setAttribute('aria-checked', String(!!on));
}

// 请求在飞行中：开关既要看得出「正在处理」，又要拦住重复点击。
export function setPending(b, on) {
  b.classList.toggle('pending', !!on);
  b.setAttribute('aria-busy', String(!!on));
  b.disabled = !!on;
}

// ---- 电平表：requestAnimationFrame 插值逼近最近 stats 值 ----

const meters = new Map(); // el -> {cur, target}
let meterRaf = null;
let meterLast = 0;

export function setMeter(node, target) {
  const t = Math.max(0, Math.min(1, typeof target === 'number' && isFinite(target) ? target : 0));
  if (REDUCED_MOTION) {
    node.style.transform = `scaleX(${t.toFixed(4)})`;
    return;
  }
  const m = meters.get(node) || { cur: 0, target: 0 };
  m.target = t;
  meters.set(node, m);
  if (meterRaf == null) {
    meterLast = performance.now();
    meterRaf = requestAnimationFrame(meterTick);
  }
}

function meterTick(now) {
  meterRaf = null;
  const dt = Math.min(100, now - meterLast);
  meterLast = now;
  const k = 1 - Math.pow(0.02, dt / 1000); // ~1s 收敛到目标，帧率无关
  let active = false;
  for (const [node, m] of meters) {
    if (!node.isConnected) { meters.delete(node); continue; }
    m.cur += (m.target - m.cur) * k;
    if (Math.abs(m.target - m.cur) < 0.0015) m.cur = m.target;
    else active = true;
    node.style.transform = `scaleX(${m.cur.toFixed(4)})`;
  }
  if (active && meters.size) meterRaf = requestAnimationFrame(meterTick);
}

// ---- 60 点 canvas 迷你折线：单主色、无网格、淡区域填充、末点高亮 ----

export function drawSpark(canvas, points, opts = {}) {
  const W = 160, H = 36, N = 60, PAD = 4;
  const dpr = window.devicePixelRatio || 1;
  if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
    canvas.width = W * dpr;
    canvas.height = H * dpr;
  }
  const g = canvas.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const data = (points || []).slice(-N);
  if (!data.length) return;

  let min = Math.min(...data);
  let max = Math.max(...data);
  if (opts.floor != null) min = Math.min(min, opts.floor);
  if (max - min < 1e-9) max = min + 1;

  const x = (i) => PAD + ((W - 2 * PAD) * (i + (N - data.length))) / (N - 1);
  const y = (v) => PAD + (H - 2 * PAD) * (1 - (v - min) / (max - min));

  if (data.length > 1) {
    g.beginPath();
    data.forEach((v, i) => (i ? g.lineTo(x(i), y(v)) : g.moveTo(x(i), y(v))));
    g.strokeStyle = ACCENT;
    g.lineWidth = 1.5;
    g.lineJoin = 'round';
    g.lineCap = 'round';
    g.stroke();

    g.beginPath();
    data.forEach((v, i) => (i ? g.lineTo(x(i), y(v)) : g.moveTo(x(i), y(v))));
    g.lineTo(x(data.length - 1), H - 1);
    g.lineTo(x(0), H - 1);
    g.closePath();
    const grad = g.createLinearGradient(0, 0, 0, H);
    grad.addColorStop(0, 'rgba(49, 200, 176, 0.22)');
    grad.addColorStop(1, 'rgba(49, 200, 176, 0)');
    g.fillStyle = grad;
    g.fill();
  }

  const lx = x(data.length - 1);
  const ly = y(data[data.length - 1]);
  g.beginPath();
  g.arc(lx, ly, 2.5, 0, Math.PI * 2);
  g.fillStyle = ACCENT;
  g.fill();
}

// ---- toast ----

export function toast(msg, kind = 'info') {
  const box = document.getElementById('toasts');
  if (!box) return;
  const t = el('div', { class: `toast ${kind}` }, String(msg));
  box.append(t);
  setTimeout(() => {
    t.classList.add('out');
    setTimeout(() => t.remove(), 350);
  }, 3200);
}
