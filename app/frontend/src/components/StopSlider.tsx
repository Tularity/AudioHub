// 离散档滑条：一条原生 <input type="range">，但索引寻址的是**档位表**而不是连续量。
// min=0 / max=stops.length-1 / step=1，滑块位置只是下标，档位的含义全在 label 里。
//
// 与 VolumeControl 的滑块正好相反，这一个是**受控**的。那边的难点是「有时候别跟随」
// ——每秒一帧的 stats 会把滑块从手指底下抢走，所以它必须非受控。这里没有任何流在推值，
// 权威值只来自 settings.set 的回包，于是难点变成另一件事：**飞行期间别回弹**。
// 请求在路上时 thumb 停在用户拖到的位置；落地之后——无论成功还是被拒——一律交还给
// daemon 的值。被拒时 thumb 自己退回去，界面上绝不会留下一个 daemon 从没接受过的档。
//
// 三处刻意的设计：
//   1. 值标签读的是**已确认**的档，不是 thumb 所在的档。thumb 可以为了跟手先跑，
//      但「当前是哪一档」这句陈述必须等回包——这是项目里反复写过的同一条规矩。
//   2. 拖动过程中只移动 thumb、不发 RPC，松手才发一次。否则横拖一次 13 档就是 13 次
//      settings.set，而这两档现在是**立即作用于运行中的媒体面**的。键盘操作没有指针
//      按下过程，即时下发。
//   3. 禁用档照画不误，只是选不中；吸附规则是确定的（见 snapToEnabled）。把「哪一档」
//      交给循环的命中顺序，等于让同一次点击在不同档表上落到不同结果。

import { useCallback, useEffect, useRef, useState } from 'react';

/** 滑条上的一档。`available === false` = 画得出来但选不中。 */
export interface Stop {
  /** 回传给调用方（进而下发给 daemon）的值。 */
  value: string;
  /** 刻度标签，同时用作 aria-valuetext——读屏念一个裸下标毫无用处。 */
  label: string;
  available?: boolean;
  /** 置灰原因，落在刻度与滑条的 title 上。 */
  why?: string;
}

const THUMB = 14; // 与 styles.css 的 .stop-slider thumb 尺寸一致

function enabledAt(stops: Stop[], i: number): boolean {
  return i >= 0 && i < stops.length && stops[i].available !== false;
}

/**
 * 把任意下标吸附到最近的**可选**档。等距时**取低位**——这一条不是审美偏好而是
 * 确定性要求：换成「先命中的赢」，结果就取决于循环写法，同一次拖动在不同档表上
 * 会落到不同档，而且没人能从代码上一眼看出会落到哪。
 *
 * 全表禁用时返回 -1，调用方据此不发任何请求。
 */
export function snapToEnabled(stops: Stop[], want: number): number {
  if (!stops.length) return -1;
  const from = Math.max(0, Math.min(stops.length - 1, Math.round(want)));
  if (enabledAt(stops, from)) return from;
  // 向两侧同步扩张：每一个距离 d 上先看低位再看高位，于是等距必然低位胜出。
  for (let d = 1; d < stops.length; d += 1) {
    if (enabledAt(stops, from - d)) return from - d;
    if (enabledAt(stops, from + d)) return from + d;
  }
  return -1;
}

function fracOf(i: number, n: number): number {
  return n > 1 ? Math.max(0, Math.min(1, i / (n - 1))) : 0;
}

/** thumb 中心的横向位置。轨道两端各被 thumb 的一半占住，刻度要对齐就得算进去。 */
function trackPos(frac: number): string {
  return `calc(${THUMB / 2}px + (100% - ${THUMB}px) * ${frac})`;
}

export function StopSlider({
  testid, label, stops, value, disabled = false, onSelect,
}: {
  testid: string;
  /** aria-label：控件是什么，不是它现在的值。 */
  label: string;
  stops: Stop[];
  /** daemon 已确认的值。不在表内 = 版本错位，界面如实显示原始串而不硬塞进某一档。 */
  value: string;
  disabled?: boolean;
  onSelect: (v: string) => void | Promise<unknown>;
}) {
  // draft 同时充当两种角色：拖动中的 thumb 位置，以及请求飞行期间的「别回弹」。
  // 两者的结束条件不同（松手 / 回包），但对渲染的要求是同一句：以它为准。
  // ref 与 state 并存：松手发生在 window 事件里，那里读不到闭包外的最新 state。
  const [draft, setDraft] = useState<number | null>(null);
  const draftRef = useRef<number | null>(null);
  const dragging = useRef(false);
  const alive = useRef(true);
  const seq = useRef(0);

  useEffect(() => () => { alive.current = false; }, []);

  const put = useCallback((v: number | null) => {
    draftRef.current = v;
    setDraft(v);
  }, []);

  const confirmed = stops.findIndex((s) => s.value === value);
  const usable = !disabled && stops.some((s) => s.available !== false);

  // thumb 落点：飞行/拖动中听 draft，否则听 daemon；daemon 给了个不认识的值时
  // 停在第一个可选档上（thumb 总得有个位置），但值标签会照实说那个陌生的值。
  const fallback = snapToEnabled(stops, 0);
  const shown = draft != null ? draft
    : confirmed >= 0 ? confirmed
      : fallback >= 0 ? fallback : 0;

  const fire = useCallback((want: number) => {
    if (want < 0 || want >= stops.length) return;
    // 吸附回原位（拖了一下又拖回来）：没有任何变化，不必打扰 daemon。
    if (want === confirmed) { put(null); return; }
    const mine = seq.current + 1;
    seq.current = mine;
    const r = onSelect(stops[want].value);
    if (r && typeof (r as Promise<unknown>).then === 'function') {
      const done = () => {
        // 只有最新一次请求有权交还控制权。否则连拖两下时，先落地的那次会把 thumb
        // 拽回上一档，而第二次请求还在路上——用户会看到滑块自己往回跳一下。
        if (alive.current && seq.current === mine) put(null);
      };
      void (r as Promise<unknown>).then(done, done);
    } else {
      put(null);
    }
  }, [confirmed, onSelect, put, stops]);

  // 指针在滑条外松开同样算松手，所以监听挂在 window 上。fire 每渲染都换一个身份
  // （依赖 stops/confirmed），走 ref 转发才不会每帧重挂一次监听。
  const fireRef = useRef(fire);
  fireRef.current = fire;

  useEffect(() => {
    function release(): void {
      if (!dragging.current) return;
      dragging.current = false;
      const d = draftRef.current;
      if (d != null) fireRef.current(d);
    }
    window.addEventListener('pointerup', release);
    window.addEventListener('pointercancel', release);
    return () => {
      window.removeEventListener('pointerup', release);
      window.removeEventListener('pointercancel', release);
    };
  }, []);

  function onChange(raw: number): void {
    const want = snapToEnabled(stops, raw);
    if (want < 0) return;               // 全表禁用：thumb 不动，也不发请求
    put(want);
    if (!dragging.current) fire(want);  // 键盘 / 无按下过程的变更：即时下发
  }

  const cur = stops[shown];
  const frac = fracOf(shown, stops.length);
  // 已确认档的文案。表里没有就把原始串亮出来——静默地画成某一档，等于替 daemon
  // 编造了一个它没说过的选择。
  const confirmedLabel = confirmed >= 0 ? stops[confirmed].label : value;
  const inFlight = draft != null && draft !== confirmed;

  return (
    <div className="stop-slider-box" data-testid={`${testid}-box`}>
      <div className="stop-slider-row">
        <input
          type="range"
          className="stop-slider"
          data-testid={testid}
          min={0}
          max={Math.max(0, stops.length - 1)}
          step={1}
          value={shown}
          disabled={!usable}
          aria-label={label}
          aria-valuetext={cur ? cur.label : ''}
          aria-busy={inFlight}
          title={cur && cur.available === false ? (cur.why || '') : ''}
          style={{ ['--fill' as string]: trackPos(frac) }}
          onPointerDown={() => { dragging.current = true; }}
          onBlur={() => { dragging.current = false; }}
          onChange={(e) => onChange(e.currentTarget.valueAsNumber)}
        />
        <div className="stop-ticks" aria-hidden="true">
          {stops.map((s, i) => (
            <span
              key={s.value}
              className={`stop-tick${s.available === false ? ' off' : ''}${i === shown ? ' on' : ''}`}
              style={{ left: trackPos(fracOf(i, stops.length)) }}
              title={s.available === false ? (s.why || '') : s.label}
            />
          ))}
        </div>
      </div>
      <span
        className={`stop-slider-val${inFlight ? ' pending' : ''}`}
        data-testid={`${testid}-value`}
      >
        {confirmedLabel}
      </span>
    </div>
  );
}
