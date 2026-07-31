// 格式化 / 会话用途标签 / 动画开关。
//
// 数字与日期一律走 Intl，不再手搓 toFixed / padStart 拼接：小数点、千分位、日期
// 字段顺序在不同语种里都不一样，硬编码等于把「只支持中文」焊进渲染层。
// 唯一保留手工拼装的是 hh:mm:ss，因为那是等宽计时器而不是自然语言时间；连接它的
// 那句话（要不要「天」）仍然由语料决定。

import { getLocale, t } from '../i18n';

export const ACCENT = '#31c8b0';

export const REDUCED_MOTION =
  typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

// ---- 会话用途标签 ----

// 用途只能由 (kind, dir) 联合判定：daemon 存的 kind 是**发起方**视角的标签，
// 只有 dir 被翻成本机视角（core/audiohubd/src/conn.rs handle_remote_open）。
// 只看 kind 会把「对方在取用本机麦克风」显示成「取对方麦克风」——方向正好反了。
const SESSION_FLOW = {
  'mic|recv': { label: 'session.flow.micRecv', short: 'session.short.micRecv', inbound: false },
  'mic|send': { label: 'session.flow.micSend', short: 'session.short.micSend', inbound: true },
  'spk|send': { label: 'session.flow.spkSend', short: 'session.short.spkSend', inbound: false },
  'spk|recv': { label: 'session.flow.spkRecv', short: 'session.short.spkRecv', inbound: true },
} as const;

export function sessionFlow(info: { kind?: string; dir?: string } | null | undefined) {
  const key = `${info && info.kind}|${info && info.dir}` as keyof typeof SESSION_FLOW;
  const hit = SESSION_FLOW[key];
  // 认不出的组合原样显示 `kind|dir`：那是诊断信息，不是给用户读的散文，不进语料。
  if (!hit) return { label: key, short: key, inbound: false };
  return { label: t(hit.label), short: t(hit.short), inbound: hit.inbound };
}

export function dirLabel(dir: string | undefined): string {
  if (dir === 'send') return t('session.dir.send');
  if (dir === 'recv') return t('session.dir.recv');
  return String(dir ?? '');
}

// ---- 数字 ----

const numOk = (v: unknown): v is number => typeof v === 'number' && isFinite(v);

const nfCache = new Map<string, Intl.NumberFormat>();

function nf(opts: Intl.NumberFormatOptions): Intl.NumberFormat {
  const loc = getLocale();
  const key = loc + JSON.stringify(opts);
  let f = nfCache.get(key);
  if (!f) {
    f = new Intl.NumberFormat(loc, opts);
    nfCache.set(key, f);
  }
  return f;
}

const dtCache = new Map<string, Intl.DateTimeFormat>();

function dtf(opts: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  const loc = getLocale();
  const key = loc + JSON.stringify(opts);
  let f = dtCache.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(loc, opts);
    dtCache.set(key, f);
  }
  return f;
}

const DASH = () => t('common.dash');

export const fmt = {
  fp: (fp: string | null | undefined, n = 8) => String(fp || '').slice(0, n),
  /** 两位小数（丢包率、抖动）。 */
  pct: (v: unknown) => (numOk(v) ? nf({ minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(v) : DASH()),
  ms: (v: unknown) => (numOk(v) ? nf({ minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(v) : DASH()),
  kbps: (v: unknown) => (numOk(v) ? nf({ maximumFractionDigits: 0 }).format(Math.round(v)) : DASH()),
  int: (v: unknown) => (numOk(v) ? nf({ maximumFractionDigits: 0 }).format(Math.round(v)) : DASH()),
  /** 计数（收包/丢包/帧数）。null/undefined 记 0——那是「还没有」而不是「不知道」。 */
  count: (v: unknown) => nf({ maximumFractionDigits: 0 }).format(numOk(v) ? v : 0),
  decimal1: (v: unknown) => (numOk(v)
    ? nf({ minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(v)
    : DASH()),

  uptime(sec: unknown): string {
    if (!numOk(sec) || sec < 0) return DASH();
    const s = Math.floor(sec);
    const d = Math.floor(s / 86400);
    const two = nf({ minimumIntegerDigits: 2, useGrouping: false });
    const hh = two.format(Math.floor((s % 86400) / 3600));
    const mm = two.format(Math.floor((s % 3600) / 60));
    const ss = two.format(s % 60);
    return d > 0
      ? t('time.uptimeDays', { d: nf({ maximumFractionDigits: 0 }).format(d), hh, mm, ss })
      : t('time.uptime', { hh, mm, ss });
  },

  clock(ts: number): string {
    try { return dtf({ hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false }).format(new Date(ts)); } catch { return DASH(); }
  },

  date(unixS: number | null | undefined): string {
    if (!unixS) return DASH();
    try {
      return dtf({
        year: 'numeric', month: '2-digit', day: '2-digit',
        hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false,
      }).format(new Date(unixS * 1000));
    } catch { return DASH(); }
  },
};

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

export const IS_MAC = /mac/i.test(navigator.platform || '') || /Macintosh/i.test(navigator.userAgent || '');
