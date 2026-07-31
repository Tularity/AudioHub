// 「桥接到虚拟声卡」（spec-m4c §B，交互按 plan §7.1 冻结）的纯数据部分。
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
// 两个字段都可能缺席（旧 daemon）——那就等同「未检测到」，控件保持置灰。

import type { DaemonInfo } from '../ipc/types';
import { IS_MAC } from './fmt';
import { t } from '../i18n';

export interface Vendor { id: string; label: string; url: string; mac: boolean }

// 厂商名本身是商标，不翻译；但括号里的平台注记是自然语言，所以整条走语料。
const VENDORS = [
  { id: 'blackhole', labelKey: 'vendor.blackhole', url: 'https://existential.audio/blackhole/', mac: true },
  { id: 'vbcable', labelKey: 'vendor.vbcable', url: 'https://vb-audio.com/Cable/', mac: false },
] as const;

/** 本机平台对应的厂商排在前面——两个都列出，跨平台用户不会被藏起来。 */
export function vendors(): Vendor[] {
  return VENDORS
    .map((v) => ({ id: v.id, label: t(v.labelKey), url: v.url, mac: v.mac }))
    .sort((a, b) => Number(b.mac === IS_MAC) - Number(a.mac === IS_MAC));
}

export interface CardEntry {
  id: string;
  name: string;
  kind: string;
  present: boolean;
  usable: boolean;
}

function outputSet(daemon: DaemonInfo | null | undefined): Set<string> | null {
  const outs = daemon && daemon.output_devices;
  if (!Array.isArray(outs)) return null; // 字段缺席：无从判断，别乱标不可用
  return new Set(outs.filter((x) => typeof x === 'string').map((x) => x.trim().toLowerCase()));
}

/** daemon.status 上报的整份目录（含未安装项），设置页按它列检测结果。 */
export function bridgeCatalog(daemon: DaemonInfo | null | undefined): CardEntry[] | null {
  const cards = daemon && daemon.virtual_cards;
  if (!Array.isArray(cards)) return null; // null = daemon 根本没上报这个字段
  const outs = outputSet(daemon);
  return cards.filter((c) => c && c.name).map((c) => ({
    id: String(c.id || c.name),
    name: String(c.name),
    kind: String(c.kind || 'other'),
    present: !!c.present,
    // output_devices 是 daemon 真正能打开的输出设备名单。名字在目录里、却不在名单
    // 里的卡，选了必然开不起来（daemon 按名字找不到设备就报错，不会静默回落）。
    usable: !!c.present && (!outs || outs.has(String(c.name).trim().toLowerCase())),
  }));
}

/** 可以真正桥接进去的卡。 */
export function bridgeTargets(daemon: DaemonInfo | null | undefined): CardEntry[] {
  return (bridgeCatalog(daemon) || []).filter((c) => c.usable);
}
