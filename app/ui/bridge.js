// 「桥接到虚拟声卡」（spec-m4c §B，交互按 plan §7.1 冻结）。
//
// 取用对端麦克风时，把音频写进本机已装的第三方虚拟声卡的**播放端**；任意应用选中
// 该声卡的**输入端**，就等于选中了对端的麦克风。签名负担在第三方，本项目零驱动。
//
// 冻结的交互只有一条：**检测到才可选，未检测到置灰并给官网链接**。我们不代装、
// 不主动引导安装，所以这里既没有「一键安装」，也没有装不装的催促文案。
//
// 数据来自 daemon.status：
//   virtual_cards: [{id,name,kind,present}]  目录 + 是否装了
//   output_devices: [string]                 daemon 真能打开的输出设备名
// 两个字段都可能缺席（旧 daemon）——那就等同「未检测到」，控件保持置灰，
// 绝不假装可用。

import { el, icon, extLink } from './ui.js';

const VENDORS = [
  { id: 'blackhole', label: 'BlackHole（macOS）', url: 'https://existential.audio/blackhole/', mac: true },
  { id: 'vbcable', label: 'VB-Cable（Windows）', url: 'https://vb-audio.com/Cable/', mac: false },
];

function isMac() {
  return /mac/i.test(navigator.platform || '') || /Macintosh/i.test(navigator.userAgent || '');
}

/** 本机平台对应的厂商排在前面——两个都列出，跨平台用户不会被藏起来。 */
export function vendors() {
  const mac = isMac();
  return VENDORS.slice().sort((a, b) => Number(b.mac === mac) - Number(a.mac === mac));
}

function outputSet(daemon) {
  const outs = daemon && daemon.output_devices;
  if (!Array.isArray(outs)) return null; // 字段缺席：无从判断，别乱标不可用
  return new Set(outs.filter((x) => typeof x === 'string').map((x) => x.trim().toLowerCase()));
}

/** daemon.status 上报的整份目录（含未安装项），设置页按它列检测结果。 */
export function bridgeCatalog(daemon) {
  const cards = daemon && daemon.virtual_cards;
  if (!Array.isArray(cards)) return null; // null = daemon 根本没上报这个字段
  const outs = outputSet(daemon);
  return cards.filter((c) => c && c.name).map((c) => ({
    id: String(c.id || c.name),
    name: String(c.name),
    kind: String(c.kind || 'other'),
    present: !!c.present,
    // output_devices 是 daemon 真正能打开的输出设备名单。名字在目录里、却不在名单
    // 里的卡，选了必然开不起来（daemon 按名字找不到设备就报错，不会静默回落），
    // 与其让会话失败，不如当场标出来。
    usable: !!c.present && (!outs || outs.has(c.name.trim().toLowerCase())),
  }));
}

/** 可以真正桥接进去的卡。 */
export function bridgeTargets(daemon) {
  return (bridgeCatalog(daemon) || []).filter((c) => c.usable);
}

const NONE = '';

/**
 * @param {object} o
 * @param {string} o.testid            select 的 data-testid（衍生 -box/-note/-links/-link-*）
 * @param {(value:string)=>void} o.onChange  选中的声卡名，'' = 不桥接
 * @returns {{node:HTMLElement, apply:(daemon:object|null, value:string, pending?:boolean)=>void}}
 */
export function bridgeControl({ testid, label = '桥接到虚拟声卡', onChange }) {
  const select = el('select', { class: 'select', 'data-testid': testid, 'aria-label': label });
  const note = el('p', { class: 'bridge-note', 'data-testid': `${testid}-note` });
  const links = el('div', { class: 'bridge-links', 'data-testid': `${testid}-links`, hidden: true });
  for (const v of vendors()) links.append(extLink(v.label, v.url, `${testid}-link-${v.id}`));

  const node = el('div', { class: 'bridge-box', 'data-testid': `${testid}-box` },
    el('label', { class: 'bridge-row' },
      icon('cable'), el('span', { class: 'bridge-label' }, label), select),
    note, links);
  // 对端卡片整体可点击（进入详情）：控件里的点击不能冒泡上去，否则一选就被导航走。
  node.addEventListener('click', (e) => e.stopPropagation());

  select.addEventListener('change', () => {
    if (onChange) onChange(select.value);
  });

  let optKey = null;

  function rebuild(list, value, hasField) {
    // 选项签名没变就不动 DOM：下拉展开时重建会把菜单从用户手里合上。
    const key = JSON.stringify([hasField, value, list.map((c) => [c.name, c.usable])]);
    if (key === optKey) return;
    optKey = key;
    select.innerHTML = '';
    // 选中的卡刚被拔掉/改名：留一个如实标注的条目，别让选择悄悄跳回「不桥接」。
    // 一张可用的卡都不剩时同样要留——只放占位项的话 select.value 会落回 ''，
    // 界面看着像「没桥接」，而偏好里那张卡还在。
    const stale = !!value && !list.some((c) => c.name === value);
    if (!list.length) {
      select.append(el('option', { value: NONE }, hasField ? '未检测到虚拟声卡' : '服务未上报'));
      if (stale) select.append(el('option', { value }, `${value}（未检测到）`));
      return;
    }
    select.append(el('option', { value: NONE }, '不桥接'));
    for (const c of list) {
      select.append(el('option', { value: c.name }, c.name));
    }
    if (stale) {
      select.append(el('option', { value }, `${value}（未检测到）`));
    }
  }

  return {
    node,

    apply(daemon, value, pending = false) {
      const catalog = bridgeCatalog(daemon);
      const list = (catalog || []).filter((c) => c.usable);
      const has = list.length > 0;
      // 选中的卡此刻不在可用名单里 = 这条偏好已经发不出去了（peers.js 的
      // micParams 会拦下来），文案必须说明白，不能继续讲「将写入…」。
      const stale = !!value && !list.some((c) => c.name === value);
      rebuild(list, value || NONE, catalog != null);
      select.value = value || NONE;
      select.disabled = !has || pending;
      node.classList.toggle('unavailable', !has);
      links.hidden = has;

      if (stale) {
        note.textContent = `「${value}」当前未检测到：开启「取对方麦克风」时不会桥接。`
          + (has ? '请重新选择一张可用的声卡。' : '装回该声卡后重开本应用即可恢复。');
        return;
      }
      if (!has) {
        const known = (catalog || []).filter((c) => c.present && !c.usable);
        note.textContent = catalog == null
          ? '当前服务未上报虚拟声卡信息，无法桥接。'
          : known.length
            ? `检测到 ${known.map((c) => c.name).join('、')}，但它不在系统输出设备列表里，无法写入。`
            : '未检测到虚拟声卡。AudioHub 不会替你安装任何驱动——如需此功能，请自行安装下列任一款后重开本应用。';
        return;
      }
      note.textContent = value
        ? `对端麦克风将写入「${value}」的播放端；任意应用选择它的输入端即可当作对端麦克风使用。`
        : '选择一张虚拟声卡后，对端麦克风会写入它的播放端，供其他应用当作输入设备使用。';
    },
  };
}
