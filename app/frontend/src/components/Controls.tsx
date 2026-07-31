// 通用控件：开关、分段选择器、电平表、迷你折线、外链。

import { useCallback, useEffect, useRef, useState } from 'react';
import { Icon } from './Icon';
import { ACCENT, REDUCED_MOTION } from '../lib/fmt';
import { toast } from './Toasts';
import { openExternal } from '../lib/external';

// ---- 开关（真实滑块） ----

export function Switch({
  testid, label, checked, pending = false, disabled = false, onToggle,
}: {
  testid: string;
  label?: string;
  checked: boolean;
  pending?: boolean;
  disabled?: boolean;
  onToggle: (want: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      className={`switch${checked ? ' on' : ''}${pending ? ' pending' : ''}`}
      aria-checked={checked}
      aria-busy={pending}
      aria-label={label || ''}
      data-testid={testid}
      // 请求在飞行中：开关既要看得出「正在处理」，又要拦住重复点击。
      disabled={pending || disabled}
      onClick={(e) => { e.stopPropagation(); onToggle(!checked); }}
    >
      <span className="knob" />
    </button>
  );
}

// ---- 分段选择器 ----

export interface SegOption<T extends string> {
  value: T;
  label: string;
  /** 每次渲染都重新求值，不是建控件时的一次性快照——模式 B 的可用性会随 status 变。 */
  disabled?: boolean;
  why?: string;
}

/**
 * set() 可返回 Promise：写 daemon 的选择器在飞行期间必须整体禁用，否则连点两下
 * 会发出两次 settings.set，第二次的回包可能比第一次先到，界面停在错的档上。
 */
export function Segmented<T extends string>({
  testid, options, value, onSelect,
}: {
  testid: string;
  options: SegOption<T>[];
  value: T;
  onSelect: (v: T) => void | Promise<unknown>;
}) {
  const [busy, setBusy] = useState(false);
  const alive = useRef(true);
  useEffect(() => () => { alive.current = false; }, []);

  const click = useCallback(async (o: SegOption<T>) => {
    if (o.disabled || busy) return;
    const r = onSelect(o.value);
    if (r && typeof (r as Promise<unknown>).then === 'function') {
      setBusy(true);
      try { await r; } catch { /* 调用方已提示 */ } finally {
        if (alive.current) setBusy(false);
      }
    }
  }, [busy, onSelect]);

  return (
    <div className={`segmented${busy ? ' busy' : ''}`} role="radiogroup" data-testid={testid}>
      {options.map((o) => {
        const off = !!o.disabled;
        const on = !off && o.value === value;
        return (
          <button
            key={o.value}
            type="button"
            role="radio"
            data-value={o.value}
            data-testid={`${testid}-${o.value}`}
            className={`seg${off ? ' off' : ''}${on ? ' on' : ''}`}
            // 置灰必须是**真禁用**（点击不改变任何状态、不发任何 RPC）。
            disabled={off || busy}
            aria-checked={on}
            title={off ? (o.why || '') : ''}
            onClick={(e) => { e.stopPropagation(); void click(o); }}
          >
            {o.label}
          </button>
        );
      })}
    </div>
  );
}

// ---- 电平表：requestAnimationFrame 插值逼近最近 stats 值 ----

const meters = new Map<HTMLElement, { cur: number; target: number }>();
let meterRaf: number | null = null;
let meterLast = 0;

function meterTick(now: number): void {
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

/**
 * 电平条。60fps 的插值直接改 DOM style，**故意不走 React state**：让 React 每帧
 * 重渲一次只为了改一个 transform，代价和收益完全不成比例。
 */
export function Meter({ testid, value }: { testid: string; value: number }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    const t = Math.max(0, Math.min(1, isFinite(value) ? value : 0));
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
  }, [value]);

  useEffect(() => {
    const node = ref.current;
    return () => { if (node) meters.delete(node); };
  }, []);

  return (
    <div className="meter">
      <div className="meter-fill" data-testid={testid} ref={ref} />
    </div>
  );
}

// ---- 60 点 canvas 迷你折线：单主色、无网格、淡区域填充、末点高亮 ----

export function Spark({ testid, points }: { testid: string; points: number[] | undefined }) {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const W = 160, H = 36, N = 60, PAD = 4;
    const dpr = window.devicePixelRatio || 1;
    if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
      canvas.width = W * dpr;
      canvas.height = H * dpr;
    }
    const g = canvas.getContext('2d');
    if (!g) return;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    const data = (points || []).slice(-N);
    if (!data.length) return;

    let min = Math.min(...data, 0); // floor: 0
    let max = Math.max(...data);
    if (max - min < 1e-9) max = min + 1;

    const x = (i: number) => PAD + ((W - 2 * PAD) * (i + (N - data.length))) / (N - 1);
    const y = (v: number) => PAD + (H - 2 * PAD) * (1 - (v - min) / (max - min));

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
  }, [points]);

  return <canvas className="spark" width={160} height={36} data-testid={testid} ref={ref} />;
}

// ---- 外链 ----

export function ExtLink({ text, url, testid }: { text: string; url: string; testid: string }) {
  return (
    <a
      className="ext-link"
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      title={url}
      data-testid={testid}
      onClick={(e) => { e.preventDefault(); e.stopPropagation(); void openExternal(url); }}
    >
      {text}
      <Icon name="link" />
    </a>
  );
}

export { toast };
