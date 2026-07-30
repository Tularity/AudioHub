// 音量同步控件（spec-m4b.md §A3）：滑块 + 静音按钮，当前值来自 stats.volume。
//
// 契约（core/audiohub-ipc/src/lib.rs，唯一事实来源）：
//   session.set_volume {id, scalar, muted?} -> {}
//     —— **省略 muted 即保持对端当前静音态**，所以拖动滑块只发 scalar；
//        否则一次拖动就会把对端悄悄解除静音。只有静音按钮显式带 muted。
//   stats.volume: Option<VolumeState{scalar,muted,adjustable}>
//     —— spk 会话两侧都有（提供端报自己**真实设备**的状态，消费端镜像），
//        null = 该会话不同步音量（或首帧还没到）。
//
// 为什么不能把 stats 直接绑到滑块上：daemon 每秒回报提供端真实设备的状态，
// 拖动时回声必然滞后于手指，朴素绑定会让滑块来回跳。这里用「本地意图压住回声」：
// 用户一动就记下意图并立刻渲染，直到 (a) 回报值收敛到意图（容差 EPS，设备会量化
// 音量，回报值和请求值不会逐位相等），或 (b) 超过 HOLD_MS 仍未收敛——那说明设备
// 没照做（被 clamp / 被其他程序改回去），此时如实显示真值而不是继续骗人。

import { el, icon } from './ui.js';

const THROTTLE_MS = 100;   // §A3：拖动最多 100ms 发一次 RPC（首发 + 尾发）
const HOLD_MS = 2000;      // 本地意图压住回声的上限；stats 1s 一帧，给足两帧
const EPS = 0.02;          // 收敛容差：设备量化后回报值与请求值有细微差异
const WAIT_MS = 5000;      // 超过它还没等到 volume，判定该会话没开音量同步
const ERR_MS = 4000;       // 失败提示的停留时长（下一帧 render 自动清）

function pctOf(scalar) {
  const v = typeof scalar === 'number' && isFinite(scalar) ? scalar : 0;
  return Math.round(Math.max(0, Math.min(1, v)) * 100);
}

/**
 * @param {object} o
 * @param {string} o.volumeTestid  滑块的 data-testid（衍生出 -box/-value/-note）
 * @param {string} o.muteTestid    静音按钮的 data-testid
 * @param {string} [o.label]       滑块的无障碍名称
 * @param {(id:number, params:object)=>Promise} o.onSet  发 session.set_volume
 * @returns {{node:HTMLElement, apply:(sess:object|null)=>void, destroy:()=>void}}
 */
export function volumeControl({ volumeTestid, muteTestid, label = '对方扬声器音量', onSet }) {
  const spkIco = icon('spk', 'ico');
  const muteIco = icon('mute', 'ico');
  muteIco.hidden = true;
  const muteBtn = el('button', {
    type: 'button', class: 'icon-btn vol-mute', 'data-testid': muteTestid,
    'aria-pressed': 'false', 'aria-label': '静音', title: '静音',
  }, spkIco, muteIco);

  const slider = el('input', {
    type: 'range', min: '0', max: '100', step: '1', value: '0',
    class: 'vol-slider', 'data-testid': volumeTestid, 'aria-label': label,
  });
  const valEl = el('span', { class: 'vol-val', 'data-testid': `${volumeTestid}-value` }, '—');
  const note = el('p', { class: 'vol-note', 'data-testid': `${volumeTestid}-note`, hidden: true });
  const node = el('div', { class: 'volume-box', hidden: true, 'data-testid': `${volumeTestid}-box` },
    el('div', { class: 'volume-row' }, muteBtn, slider, valEl),
    note);

  // 对端卡片整体可点击（进入详情）：控件里的点击绝不能冒泡上去，
  // 否则拖完滑块就被导航走了。
  node.addEventListener('click', (e) => e.stopPropagation());

  let sessionId = null;
  let reported = null;     // 最近一次 stats.volume
  let intent = null;       // {scalar, muted, at} —— 用户意图，压住回声
  let seenAt = 0;          // 会话首次出现的时刻（区分「首帧未到」与「没开同步」）
  let errAt = 0;
  let held = false;        // 指针正按在滑块上：这期间绝不回写 value
  let lastSendAt = -Infinity;
  let flushTimer = null;
  let queued = null;

  function current() {
    return intent || reported;
  }

  // 意图的生命周期只在这里推进：收敛或超时即交还给真实值。
  function settleIntent() {
    if (!intent) return;
    if (reported
      && Math.abs(reported.scalar - intent.scalar) <= EPS
      && reported.muted === intent.muted) {
      intent = null;
    } else if (Date.now() - intent.at > HOLD_MS) {
      intent = null;
    }
  }

  function render() {
    settleIntent();
    node.hidden = sessionId == null;
    if (sessionId == null) return;

    const cur = current();
    const adjustable = reported ? reported.adjustable !== false : false;
    const usable = !!reported && adjustable;

    slider.disabled = !usable;
    muteBtn.disabled = !usable;
    node.classList.toggle('unadjustable', !!reported && !adjustable);

    const pct = cur ? pctOf(cur.scalar) : 0;
    const muted = !!(cur && cur.muted);
    // 拖动中（指针按住或意图仍在压制回声）不回写 value：那会把滑块从手指下抢走。
    if (!held && !intent) slider.value = String(pct);
    slider.style.setProperty('--vol', `${pct}%`);
    slider.classList.toggle('muted', muted);
    valEl.textContent = cur ? (muted ? '已静音' : `${pct}%`) : '—';

    spkIco.hidden = muted;
    muteIco.hidden = !muted;
    muteBtn.classList.toggle('on', muted);
    muteBtn.setAttribute('aria-pressed', String(muted));
    const mLabel = muted ? '取消静音' : '静音';
    muteBtn.setAttribute('aria-label', mLabel);
    muteBtn.title = mLabel;

    let text = '';
    if (errAt && Date.now() - errAt < ERR_MS) text = '音量调节失败，请稍后重试';
    else if (reported && !adjustable) text = '对端设备不支持音量调节';
    else if (!reported) {
      text = Date.now() - seenAt < WAIT_MS ? '正在读取对端音量…' : '该会话未启用音量同步';
    }
    note.textContent = text;
    note.hidden = !text;
  }

  function fire(args) {
    lastSendAt = performance.now();
    queued = null;
    const id = sessionId;
    if (id == null || !onSet) return;
    const params = { scalar: args.scalar };
    if (args.muted != null) params.muted = args.muted;
    Promise.resolve(onSet(id, params)).catch(() => {
      // 失败就立刻放弃本地意图，让下一帧真实值接管——绝不留一个骗人的滑块位置。
      if (sessionId !== id) return;
      intent = null;
      errAt = Date.now();
      render();
    });
  }

  // 节流：首发即时、尾发必达，两次 RPC 之间至少 THROTTLE_MS。
  function schedule(scalar, muted) {
    const base = current();
    intent = {
      scalar,
      muted: muted == null ? !!(base && base.muted) : muted,
      at: Date.now(),
    };
    render();
    if (sessionId == null) return;
    queued = { scalar, muted };
    const wait = THROTTLE_MS - (performance.now() - lastSendAt);
    if (wait <= 0 && flushTimer == null) {
      fire(queued);
      return;
    }
    if (flushTimer == null) {
      flushTimer = setTimeout(() => {
        flushTimer = null;
        if (queued) fire(queued);
      }, Math.max(0, wait));
    }
  }

  slider.addEventListener('input', () => {
    // muted 传 null = 不下发 muted 字段：拖动音量不该改变对端静音态。
    schedule(slider.valueAsNumber / 100, null);
  });
  slider.addEventListener('pointerdown', () => { held = true; });
  const release = () => {
    if (!held) return;
    held = false;
    render();
  };
  window.addEventListener('pointerup', release);
  window.addEventListener('pointercancel', release);
  slider.addEventListener('blur', release);

  muteBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const cur = current();
    if (!cur || sessionId == null) return;
    schedule(cur.scalar, !cur.muted);
  });

  function reset() {
    intent = null;
    reported = null;
    queued = null;
    errAt = 0;
    held = false;
    clearTimeout(flushTimer);
    flushTimer = null;
  }

  return {
    node,

    /** @param {object|null} sess 本机驱动的那条 spk 会话（SessionInfo），无则 null */
    apply(sess) {
      const id = sess && sess.id != null ? sess.id : null;
      if (id !== sessionId) {
        // 换了一条会话（或会话没了）：旧的意图/节流状态一律作废，
        // 否则尾发会打到一个已经不存在的 id 上。
        reset();
        sessionId = id;
        seenAt = id == null ? 0 : Date.now();
      }
      if (id != null) {
        const v = sess.stats ? sess.stats.volume : null;
        reported = v || null;
      }
      render();
    },

    destroy() {
      reset();
      sessionId = null;
      window.removeEventListener('pointerup', release);
      window.removeEventListener('pointercancel', release);
    },
  };
}

/** 只读音量回显（详情/统计页用）：不发 RPC，只把 VolumeState 翻成中文。 */
export function volumeText(v) {
  if (!v) return null;
  const pct = `${pctOf(v.scalar)}%`;
  return { pct, muted: !!v.muted, adjustable: v.adjustable !== false, text: v.muted ? `已静音 · ${pct}` : pct };
}
