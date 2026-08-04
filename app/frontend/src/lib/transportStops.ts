// 传输档位滑条的**档表**：从 daemon 的 `settings.get` 派生出 `StopSlider` 要的形状。
//
// # 为什么这段代码住在这里而不是在 Settings.tsx 里
//
// plan §15 之后档位选择搬到了**每对端的详情页**，而档**表**仍然是全局能力
// （`latency_stops_ms` / `quality_stops` 留在 `DaemonSettings`）。于是同一张表
// 有了两个消费者：详情页的四个滑条 + 设置页那张只读总览。
//
// 两处各写一份的话，「有哪些档」会各自演化，而分歧不会有任何报错——只会有一个
// 在某个页面上选得中、在另一个页面上不存在的档。这正是 daemon 坚持自己当档表
// 唯一真值源的同一条理由，只是尺度小了一层。

import { t, type MsgKey } from '../i18n';
import type { DaemonSettings, QualityStop } from '../ipc/types';
import type { Stop } from '../components/StopSlider';

// **daemon 才是档表的唯一真值源**：档位随物理能力与构建选项变，前端写死一份，
// 早晚会画出一个它自己都送不下去的档。下面两张表只在旧服务什么都不上报时兜底
// ——目的是让滑条还有档可选，不是替 daemon 声明它支持什么。
const LATENCY_STOPS_FALLBACK = [0, 10, 20, 30, 50, 75, 100, 150, 200, 300, 500, 750, 1000];

// 兜底表里三档 Opus 一律置为不可用：拿不到 daemon 的应答时，「不知道能不能用」
// 只能按不能用画。反过来（默认可用）会让用户选中一个根本发不出去的档。
const QUALITY_STOPS_FALLBACK: QualityStop[] = [
  { id: 'auto', available: true },
  { id: 'opus64', kbps: 64, available: false, blocked_by: 'opus' },
  { id: 'opus128', kbps: 128, available: false, blocked_by: 'opus' },
  { id: 'opus256', kbps: 256, available: false, blocked_by: 'opus' },
  // kbps = rate × 16 bit（s16 单声道），与 daemon 的 quality_stops() 逐档对齐。
  { id: 'pcm16k', kbps: 256, rate: 16000, available: true },
  { id: 'pcm24k', kbps: 384, rate: 24000, available: true },
  { id: 'pcm32k', kbps: 512, rate: 32000, available: true },
  { id: 'pcm48k', kbps: 768, rate: 48000, available: true },
];

// `Record<string, MsgKey>` 而不是 `Record<QualityId, MsgKey>`：id 由 daemon 定义，
// 它加一档而前端还没跟上时，缺的那条必须能在运行期被发现（见下面的 raw-id 回落），
// 而不是让 TS 强迫我们把一个虚构的联合类型当成契约。
const QUALITY_LABEL_KEY: Record<string, MsgKey> = {
  auto: 'settings.transport.q.auto',
  opus64: 'settings.transport.q.opus64',
  opus128: 'settings.transport.q.opus128',
  opus256: 'settings.transport.q.opus256',
  pcm16k: 'settings.transport.q.pcm16k',
  pcm24k: 'settings.transport.q.pcm24k',
  pcm32k: 'settings.transport.q.pcm32k',
  pcm48k: 'settings.transport.q.pcm48k',
};

/**
 * 旧服务的 `'min'` 与新档表里的 `'0'` 是同一档。数值串统一过一遍 Number，免得
 * `'200.0'` 和 `'200'` 被当成两个不同的档而双双落空。认不出来就原样返回——
 * 硬塞进某一档等于替 daemon 编造一个它没说过的选择。
 */
export function normLatency(v: string): string {
  if (v === 'auto') return 'auto';
  const n = Number(v === 'min' ? '0' : v);
  return Number.isFinite(n) ? String(n) : v;
}

/** 旧服务的 `'pcm'` 指的是全带宽那一档。 */
export function normQuality(v: string): string {
  return v === 'pcm' ? 'pcm48k' : v;
}

export function latencyStops(ds: DaemonSettings | null): Stop[] {
  const raw = ds && Array.isArray(ds.latency_stops_ms) && ds.latency_stops_ms.length
    ? ds.latency_stops_ms
    : LATENCY_STOPS_FALLBACK;
  const out: Stop[] = [{ value: 'auto', label: t('settings.transport.auto') }];
  const seen = new Set<string>(['auto']);
  for (const n of raw) {
    if (typeof n !== 'number' || !Number.isFinite(n)) continue;
    const value = String(n);
    if (seen.has(value)) continue;   // 重复档会撞 React key，也没有任何意义
    seen.add(value);
    out.push({
      value,
      label: n === 0 ? t('settings.transport.latencyLowest') : t('settings.transport.ms', { n }),
    });
  }
  return out;
}

export function qualityStops(ds: DaemonSettings | null): Stop[] {
  const raw = ds && Array.isArray(ds.quality_stops) && ds.quality_stops.length
    ? ds.quality_stops
    : QUALITY_STOPS_FALLBACK;
  const out: Stop[] = [];
  const seen = new Set<string>();
  for (const q of raw) {
    const id = q && typeof q.id === 'string' ? q.id : '';
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const key = QUALITY_LABEL_KEY[id];
    const off = q.available === false;
    out.push({
      value: id,
      // daemon 加了一档而语料还没跟上：把原始 id 画出来。悄悄跳过它，新档位就在
      // 界面上凭空消失了，而没有任何人会收到通知。
      label: key ? t(key) : id,
      available: !off,
      why: off
        ? (q.blocked_by === 'opus'
          ? t('settings.transport.qBlockedOpus')
          : t('settings.transport.qBlocked'))
        : undefined,
    });
  }
  return out;
}

/**
 * 一个档位串的**人类可读标签**，用于只读呈现（设置页总览、共享模式的回显）。
 *
 * `null` / `undefined` = 没有值。**返回 `null` 而不是 `'auto'` 或 `'0'`**：
 * 「未设定」与「设成了 auto」在执行上相同、在界面上不同，调用方必须自己决定
 * 那一格写什么，而不是从这里领一个编造出来的默认档。
 */
export function stopLabel(stops: Stop[], v: string | null | undefined): string | null {
  if (typeof v !== 'string' || !v) return null;
  const hit = stops.find((s) => s.value === v);
  // 表里没有就把原始串亮出来：静默画成某一档，等于替 daemon 编造一个它没说过
  // 的选择（与 `StopSlider` 的值标签同一条规矩）。
  return hit ? hit.label : v;
}
