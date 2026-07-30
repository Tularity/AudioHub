// 首启授权门（用户指令：mac app 的通行做法是第一页就把权限要齐再进主界面）。
//
// 挡人的规矩只有一条：**必需 + 状态可知 + 尚未授权** 才挡（permissions.js isBlocking）。
// macOS 不提供本地网络授权的查询接口，它永远不会返回 granted——拿它当门闩会把每个
// 用户永久锁死，所以它只展示、不挡人。可选项（系统音频录制）同理。
//
// 另一条硬规矩：**不落任何「已看过」标记**。门是每次启动重新探测出来的，
// 用户在系统设置里撤销授权后，下次启动照样会拦住他。

import { store } from '../store.js';
import { el, icon, toast, openExternal } from '../ui.js';
import {
  actionOf, actionLabel, isBlocking, requestQueue, STATUS_LABEL, STATUS_TAG,
} from '../permissions.js';

/**
 * 一行权限。授权门与设置页共用同一份渲染，避免两处文案/状态判定各说各话。
 * @param {object} o
 * @param {string} o.prefix   data-testid 前缀（onboarding / settings-perm）
 * @param {(p:object)=>void} o.onAction
 * @param {(p:object)=>void} [o.onDefer]   可选项的「稍后再说」；不传则不渲染
 * @returns {{node:HTMLElement, update:(p:object, busy:string|null, deferred:boolean)=>void}}
 */
export function permissionRow({ prefix, onAction, onDefer }) {
  const nameEl = el('span', { class: 'perm-name' });
  const kindTag = el('span', { class: 'tag' });
  const chip = el('span', { class: 'tag' });
  const why = el('p', { class: 'perm-why' });
  const note = el('p', { class: 'perm-note' });
  const ico = el('span', { class: 'perm-ico' });
  const action = el('button', { class: 'btn small', type: 'button' });
  const defer = onDefer ? el('button', { class: 'btn ghost small', type: 'button' }, '稍后再说') : null;
  const actions = el('div', { class: 'perm-actions' }, action, defer);
  const node = el('div', { class: 'perm-row' },
    ico,
    el('div', { class: 'perm-text' },
      el('div', { class: 'perm-head' }, nameEl, kindTag, chip),
      why, note),
    actions);

  let cur = null;
  action.addEventListener('click', () => { if (cur) onAction(cur); });
  if (defer) defer.addEventListener('click', () => { if (cur) onDefer(cur); });

  return {
    node,
    update(p, busy, deferred) {
      cur = p;
      const testids = {
        row: `${prefix}-row-${p.id}`,
        status: `${prefix}-status-${p.id}`,
        action: `${prefix}-action-${p.id}`,
        defer: `${prefix}-defer-${p.id}`,
        note: `${prefix}-note-${p.id}`,
      };
      node.setAttribute('data-testid', testids.row);
      chip.setAttribute('data-testid', testids.status);
      action.setAttribute('data-testid', testids.action);
      note.setAttribute('data-testid', testids.note);
      if (defer) defer.setAttribute('data-testid', testids.defer);

      ico.innerHTML = '';
      ico.append(icon(p.icon));
      nameEl.textContent = p.name;
      kindTag.textContent = p.required ? '必需' : '可选';
      kindTag.className = p.required ? 'tag accent' : 'tag';
      chip.textContent = STATUS_LABEL[p.status] || '未知';
      chip.className = STATUS_TAG[p.status] || 'tag';
      why.textContent = p.why;

      node.classList.toggle('granted', p.status === 'granted');
      node.classList.toggle('blocking', isBlocking(p));

      // 说明行的优先级：daemon 的补充 > 不可查询的实话 > 各状态的下一步。
      // 「不知道」的那一行必须明说系统查不到，不能拿一句含糊的提示假装知情。
      let text = p.note || '';
      if (!text && !p.knowable) {
        text = p.unknownNote || '系统不提供查询接口，无法在此显示当前状态。';
      }
      if (!text) {
        if (p.status === 'undetermined') text = '点击「授权」后由 macOS 弹窗询问；系统只会问这一次。';
        else if (p.status === 'denied') text = `已被拒绝：macOS 不允许再次弹窗，只能手动打开。${p.manual ? `路径：${p.manual}` : ''}`;
        else if (p.status === 'restricted') text = `受系统策略（如描述文件或屏幕使用时间）限制，本应用无法请求。${p.manual ? `路径：${p.manual}` : ''}`;
      }
      note.textContent = text;
      note.hidden = !text;
      // 琥珀色留给「真的出了问题、需要你去处理」的那两种；「系统查不到」和
      // 「点了会弹窗」都只是说明，染成警告色等于天天在喊狼来了。
      note.classList.toggle('warn', p.status === 'denied' || p.status === 'restricted');

      const act = actionOf(p);
      const pending = busy === p.id || busy === '*';
      action.hidden = act === 'none';
      action.textContent = pending ? '请求中…' : actionLabel(p);
      action.disabled = pending;
      action.classList.toggle('primary', act === 'request' && p.required);
      if (defer) {
        // 「稍后再说」只在这一项还没到位、且它确实不挡人时才有意义。
        defer.hidden = act === 'none' || deferred;
        defer.textContent = '稍后再说';
      }
      if (deferred) {
        action.hidden = true;
        chip.textContent = `${STATUS_LABEL[p.status] || '未知'} · 稍后再说`;
      }
    },
  };
}

export function mount(root, ctx) {
  const rows = new Map();      // id -> {node, update}
  const deferred = new Set();  // 本次会话内点过「稍后再说」的可选项；只在内存里

  const list = el('div', { class: 'perm-list', 'data-testid': 'onboarding-list' });

  const grantAll = el('button', { class: 'btn primary', type: 'button', 'data-testid': 'onboarding-grant-all' },
    icon('shield'), '全部授权');
  const recheck = el('button', { class: 'btn', type: 'button', 'data-testid': 'onboarding-recheck' }, '重新检查');
  const enter = el('button', { class: 'btn primary big', type: 'button', 'data-testid': 'onboarding-enter' },
    '进入主界面');
  const skip = el('button', { class: 'btn ghost small', type: 'button', 'data-testid': 'onboarding-skip' },
    '跳过（部分功能不可用）');
  const hint = el('p', { class: 'gate-hint', 'data-testid': 'onboarding-hint' });
  const skipNote = el('p', { class: 'gate-skip-note', 'data-testid': 'onboarding-skip-note' });

  const section = el('section', { class: 'view onboarding', 'data-testid': 'view-onboarding' },
    el('div', { class: 'gate-head' },
      el('span', { class: 'gate-logo' }, icon('shield')),
      el('div', {},
        el('h2', { class: 'gate-title' }, '开始之前，先完成授权'),
        el('p', { class: 'gate-sub' },
          'AudioHub 要把声音在两台设备之间搬运，因此需要下面这些系统权限。'
          + 'macOS 的规则是：一项权限被拒绝后，应用就无法再弹窗询问，只能到系统设置里手动打开——'
          + '所以请在这里一次给齐。'))),
    list,
    el('div', { class: 'gate-actions' },
      grantAll, recheck, el('span', { class: 'gate-spacer' }), enter),
    hint,
    el('div', { class: 'gate-foot' }, skip, skipNote));

  // 门盖住了整个窗口，连带盖掉了 index.html 里那块拖拽把手：macOS 的
  // titleBarStyle=Overlay 下没有系统标题栏，不补一条用户就搬不动这个窗口。
  root.append(el('div', { class: 'gate-drag', 'data-tauri-drag-region': true }), section);

  grantAll.addEventListener('click', () => runQueue());
  recheck.addEventListener('click', () => ctx.refresh({ force: true }));
  enter.addEventListener('click', () => ctx.dismiss(false));
  skip.addEventListener('click', () => {
    ctx.dismiss(true);
    toast('已跳过授权：未授权的功能会在使用时直接失败。可在「设置 → 系统权限」重新授权。', 'warn');
  });

  async function request(p) {
    store.setPermissionBusy(p.id);
    try {
      // 弹窗停在用户屏幕上时这个请求一直挂着（ws.js 给了 3 分钟），
      // 所以这里不能拿默认超时去卡它。
      const res = await ctx.rpc('daemon.request_permission', { id: p.id });
      await ctx.refresh({ force: true, seed: res });
    } catch (_) {
      // rpc 已经 toast 过；状态以下一轮复查为准。
    } finally {
      store.setPermissionBusy(null);
    }
  }

  async function runQueue() {
    const queue = requestQueue(store.state.permissions.list);
    if (!queue.length) {
      // 一个都请求不了 = 剩下的只能去系统设置。别让按钮点下去毫无反应。
      toast('没有可以直接弹窗请求的权限了，请用「打开系统设置」逐项开启。', 'warn');
      return;
    }
    store.setPermissionBusy('*');
    try {
      for (const p of queue) {
        try {
          const res = await ctx.rpc('daemon.request_permission', { id: p.id });
          await ctx.refresh({ force: true, seed: res });
        } catch (_) { /* 单项失败不阻断后面的：用户点的是「全部」 */ }
      }
    } finally {
      store.setPermissionBusy(null);
    }
    await ctx.refresh({ force: true });
    const left = store.state.permissions.list.filter(isBlocking);
    if (left.length) {
      toast(`仍有 ${left.length} 项必需权限未授权：${left.map((p) => p.name).join('、')}`, 'warn');
    } else {
      toast('必需权限已全部授权', 'ok');
    }
  }

  function openSettings(p) {
    if (!p.settingsUrl) {
      toast(p.manual ? `请手动前往：${p.manual}` : '本机服务未提供系统设置入口。', 'warn');
      return;
    }
    openExternal(p.settingsUrl);
    // 深链能否打开取决于 webview 与系统，所以路径文案照给不误——
    // 用户不该因为一个链接没反应就无路可走。
    if (p.manual) toast(`若系统设置没有自动打开：${p.manual}`, 'info');
  }

  function onAction(p) {
    if (actionOf(p) === 'request') request(p);
    else openSettings(p);
  }

  function onDefer(p) {
    deferred.add(p.id);
    render(store.state);
  }

  function render(s) {
    const perms = s.permissions;
    const items = perms.list;
    const seen = new Set();

    for (const p of items) {
      seen.add(p.id);
      let row = rows.get(p.id);
      if (!row) {
        // 可选项才给「稍后再说」：必需项给了就是自相矛盾（它挡着门）。
        row = permissionRow({
          prefix: 'onboarding',
          onAction,
          onDefer: p.required ? null : onDefer,
        });
        rows.set(p.id, row);
        list.append(row.node);
      }
      row.update(p, perms.busy, deferred.has(p.id));
    }
    for (const [id, row] of rows) {
      if (!seen.has(id)) { row.node.remove(); rows.delete(id); }
    }

    const blocking = items.filter(isBlocking);
    const busy = !!perms.busy;
    enter.disabled = blocking.length > 0 || busy;
    grantAll.disabled = busy || requestQueue(items).length === 0;
    recheck.disabled = busy;

    if (busy) {
      hint.textContent = '正在等待系统授权对话框…请在弹出的窗口中选择「允许」。';
    } else if (blocking.length) {
      hint.textContent = `还差 ${blocking.length} 项必需权限：${blocking.map((p) => p.name).join('、')}。`
        + '授权后可直接进入主界面；若你在系统设置里改过，回到本窗口会自动重新检查。';
    } else {
      hint.textContent = '必需权限已就绪，可以进入主界面。可选权限稍后也能在「设置 → 系统权限」里补上。';
    }

    const optionalLeft = items.filter((p) => !p.required && p.status !== 'granted').map((p) => p.name);
    skipNote.textContent = blocking.length
      ? `跳过后仍可使用界面，但${blocking.map((p) => p.name).join('、')}相关的功能会在使用时直接报错而不是静默失败。`
        + '本设置不会记住——下次启动仍会先来这一页。'
      : optionalLeft.length
        ? `可选权限（${optionalLeft.join('、')}）未授权：对应的共享来源会在选用时报错，其余功能不受影响。`
        : '';
    skip.hidden = blocking.length === 0;
  }

  const unsub = store.subscribe(render);
  render(store.state);
  return () => unsub();
}
