// 离散档滑条：**拖动时无极、松手回弹到最近档**。
//
// # 为什么不再是一条原生 `<input type="range" step="1">`
//
// 那一版拖起来是一格一格跳的，用户的原话是「十分机械」。原因是结构性的而不是
// 调参能解决的：`step=1` 让原生 thumb 只允许停在整数位置，指针在两档之间时
// thumb 已经吸附到了某一档——「无极」在 `step=1` 下**不存在**。
//
// 用户裁定的形状是（plan §14 裁定 4，2026-08-04 澄清）：
//   1. 拖动过程中 thumb **自由跟手**，不吸附；
//   2. 松手后**回弹**到最近的（可选）档；
//   3. **拖动过程中读数就跟着更新**，实时显示当前位置最近那一档的值。
//
// 于是指针交互由本组件自己实现，原生 `<input type="range">` 保留下来只当
// **无障碍与键盘的本体**（焦点、方向键、Home/End、读屏的 role/value）。
// 它被 `pointer-events: none` 挡住，不参与拖动——两套指针逻辑同时生效的话，
// 原生那套会在我们的连续位置之上再吸附一次，回到「机械」。
//
// # 第 3 条曾被读反，这里记下来防止它再被读反一次
//
// 上一版把第 3 条实现成了「松手之前**不显示**所在位置对应的数值」，还为它写了
// 一整段辩护（档距 ≈ 19.5 px、thumb ≈ 17.5 px，点亮的那一格看不见）。
// 用户的原话是：
//
// > 这次拖动条确实变得「无极」了，但**仍然不能滚动的同时预览「值」**，
// > 仍然要松手后才能看到对应档位的「值」。
//
// 也就是说，被否掉的那个「点亮刻度」的方案不够，**要的是数值本身跟着走**。
// 值标签是 14 px 的独立文字，不在 thumb 底下，那段关于遮挡的论证对它不成立。
//
// # 预览与下发是两件事，只有前者跟手
//
// 拖动中值标签每帧更新，**但 RPC 仍然只在松手后发一次**。横拖一次 13 档
// = 13 次 `peers.set_transport`，而这两档**立即作用于运行中的媒体面**：
// 每一次都会重建 JB 或重建重采样器。「看得见」与「已生效」是两个不同的
// 承诺，界面靠 `.previewing` 的字重把它们分开。
//
// # 其余三处刻意的设计（与上一版相同，理由未变）
//
//   1. 值标签在**不拖动**时读的是已确认的档，不是飞行中那一档。
//   2. 键盘操作没有指针按下过程，即时下发。
//   3. 禁用档照画不误，只是选不中；吸附规则是确定的（见 snapToEnabled）。
//
// # prefers-reduced-motion
//
// 回弹是一次 CSS `transition`，被 styles.css 末尾那条全局规则整个关掉 ⇒
// 变成瞬时归位。这里**不需要**像导航胶囊那样再由 JS 判一次：胶囊的收拢本身
// 是一个动作，关掉过渡只剩一次生硬跳变；而这里的终态（停在某一档上）无论
// 有没有过渡都是对的，瞬时归位就是这个交互在减少动效下的正确形态。

import { useCallback, useEffect, useRef, useState } from 'react';

/** 滑条上的一档。`available === false` = 画得出来但选不中。 */
export interface Stop {
  /** 回传给调用方（进而下发给 daemon）的值。 */
  value: string;
  /** 刻度标签，同时用作 aria-valuetext——读屏念一个裸下标毫无用处。 */
  label: string;
  /**
   * 可选的**定性**副标签，画在主标签下面一行（例：主「PCM 48 kHz · 24 bit」，
   * 副「全带宽 · 高精度」）。
   *
   * 存在的理由：业界一致用定性词做档位名，而本项目按用户裁定把参数写在主位。
   * 两条信息都要在，于是参数在上、定性在下。
   *
   * ⚠ 副标签**只用词、不用数**：再写一个带单位的数字，就会与主标签上的数字
   * 被拿去互相比较——那正是本项目栽过的那次 48/24 误读的形状。
   */
  sublabel?: string;
  available?: boolean;
  /** 置灰原因，落在刻度与滑条的 title 上。 */
  why?: string;
}

const THUMB = 14; // 与 styles.css 的 .stop-thumb 尺寸一致

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

/**
 * 值标签此刻该念哪一段文字（plan §14 裁定 4）。
 *
 * **拆成纯函数是为了让「拖动中要预览」这件事第一次可断言。** 它此前是组件里
 * 的一个三元表达式，而组件层在这个项目里是零覆盖的那一层——2026-08-04 的事故
 * 就长在组件接线层（`sess={micS || spkS}`），「有测试的那层没坏、坏的那层没
 * 测试」正是六次「一切都报成功、什么都没发生」的机制本身。
 *
 * 优先级：**预览 > 已确认 > 原始串**。
 * - `previewIdx >= 0`（正在拖）⇒ 念松手会落到的那一档，每帧跟着变；
 * - 否则念 daemon 已确认的那一档；
 * - 两者都没有（daemon 给了个表里没有的值）⇒ 把原始串亮出来，
 *   静默画成某一档等于替 daemon 编造一个它没说过的选择。
 */
export function shownStopLabel(
  stops: Stop[],
  confirmedIdx: number,
  previewIdx: number,
  rawValue: string,
): string {
  if (previewIdx >= 0 && stops[previewIdx]) return stops[previewIdx].label;
  if (confirmedIdx >= 0 && stops[confirmedIdx]) return stops[confirmedIdx].label;
  return rawValue;
}

/**
 * 与 [`shownStopLabel`] 同一套选取规则的副标签。**认不出的档没有副标签**
 * （不像主标签那样回落到 `rawValue`）：副标签是可选的，编一个出来毫无意义。
 */
export function shownStopSublabel(
  stops: Stop[],
  confirmedIdx: number,
  previewIdx: number,
): string | undefined {
  if (previewIdx >= 0 && stops[previewIdx]) return stops[previewIdx].sublabel;
  if (confirmedIdx >= 0 && stops[confirmedIdx]) return stops[confirmedIdx].sublabel;
  return undefined;
}

function fracOf(i: number, n: number): number {
  return n > 1 ? Math.max(0, Math.min(1, i / (n - 1))) : 0;
}

/** thumb 中心的横向位置。轨道两端各被 thumb 的一半占住，刻度要对齐就得算进去。 */
function trackPos(frac: number): string {
  return `calc(${THUMB / 2}px + (100% - ${THUMB}px) * ${frac})`;
}

/**
 * 指针横坐标 → 连续下标（可以是小数，这就是「无极」）。
 *
 * 减掉 thumb 的半宽是必须的：thumb 的**中心**才是位置，轨道两端各有半个 thumb
 * 的余量。不减的话，指针放在最左端得到的不是 0，整条轨道都会有一个常量偏移。
 * 轨道窄到装不下一个 thumb 时（窗口极窄）分母会 ≤ 0，此时退回 0，不做除法。
 */
function indexFromClientX(x: number, rect: DOMRect, n: number): number {
  const span = rect.width - THUMB;
  if (span <= 0 || n <= 1) return 0;
  const f = Math.max(0, Math.min(1, (x - rect.left - THUMB / 2) / span));
  return f * (n - 1);
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
  // 三种「不听 daemon」的状态，各有各的结束条件，但对渲染的要求是同一句：以它为准。
  //   dragFrac  拖动中的连续下标（小数）——结束于松手
  //   pending   已松手、请求在飞的那一档（整数）——结束于回包
  // ref 与 state 并存：松手可能发生在 window 事件里，那里读不到闭包外的最新 state。
  const [dragFrac, setDragFrac] = useState<number | null>(null);
  const [pending, setPending] = useState<number | null>(null);
  const dragRef = useRef<number | null>(null);
  const dragging = useRef(false);
  const alive = useRef(true);
  const seq = useRef(0);
  const rowRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => () => { alive.current = false; }, []);

  const putDrag = useCallback((v: number | null) => {
    dragRef.current = v;
    setDragFrac(v);
  }, []);

  const confirmed = stops.findIndex((s) => s.value === value);
  const usable = !disabled && stops.some((s) => s.available !== false);

  // thumb 落点：拖动中听连续量，飞行中听 pending，否则听 daemon；daemon 给了个
  // 不认识的值时停在第一个可选档上（thumb 总得有个位置），但值标签会照实说那个
  // 陌生的值。
  const fallback = snapToEnabled(stops, 0);
  const settled = pending != null ? pending
    : confirmed >= 0 ? confirmed
      : fallback >= 0 ? fallback : 0;
  // 连续位置（0..1）。拖动中是任意小数，其余时候恰好落在某一档上。
  const frac = dragFrac != null ? fracOf(dragFrac, stops.length) : fracOf(settled, stops.length);
  // 拖动中「松手会落到哪一档」。**这是预览的唯一来源**：值标签与点亮的刻度
  // 都读它，于是两者不可能各说各话。
  //
  // 用 `snapToEnabled` 而不是 `Math.round`：禁用档（未实现的 Opus 三档）
  // 不该被预览成一个落不下去的目标。预览与落点因此逐字同源——预览一个档、
  // 松手落到另一个档，比不预览更糟。
  const preview = dragFrac != null ? snapToEnabled(stops, dragFrac) : -1;
  // 点亮的那一格：拖动中跟着预览走，其余时候是已落定的那一档。
  const lit = preview >= 0 ? preview : settled;

  const fire = useCallback((want: number) => {
    if (want < 0 || want >= stops.length) return;
    // 吸附回原位（拖了一下又拖回来）：没有任何变化，不必打扰 daemon。
    if (want === confirmed) { setPending(null); return; }
    setPending(want);
    const mine = seq.current + 1;
    seq.current = mine;
    const r = onSelect(stops[want].value);
    if (r && typeof (r as Promise<unknown>).then === 'function') {
      const done = (): void => {
        // 只有最新一次请求有权交还控制权。否则连拖两下时，先落地的那次会把 thumb
        // 拽回上一档，而第二次请求还在路上——用户会看到滑块自己往回跳一下。
        if (alive.current && seq.current === mine) setPending(null);
      };
      void (r as Promise<unknown>).then(done, done);
    } else {
      setPending(null);
    }
  }, [confirmed, onSelect, stops]);

  // `release` 每渲染换一个身份（依赖 stops/confirmed），走 ref 转发才不会每帧
  // 重挂一次 window 监听。
  const fireRef = useRef(fire);
  fireRef.current = fire;

  const release = useCallback(() => {
    if (!dragging.current) return;
    dragging.current = false;
    const d = dragRef.current;
    // **先清 draft 再下发**：清掉之后 thumb 立刻由 `pending`（整数档）定位，
    // 那一步位移带 CSS transition ⇒ 这就是「回弹」。留着 draft 的话 thumb 会
    // 停在手指松开的那个小数位置，直到回包才跳过去，看起来像卡了一下。
    putDrag(null);
    if (d != null) fireRef.current(snapToEnabled(stops, d));
  }, [putDrag, stops]);
  const releaseRef = useRef(release);
  releaseRef.current = release;

  // 指针在滑条外松开同样算松手。设了 pointer capture 之后正常路径会回到本元素，
  // 但 capture 也可能被系统悄悄收走（切换空间 / 弹出系统对话框），那时只有
  // window 上的这一份兜得住。`release` 自身幂等，两条路都到达也只执行一次。
  useEffect(() => {
    const h = (): void => releaseRef.current();
    window.addEventListener('pointerup', h);
    window.addEventListener('pointercancel', h);
    return () => {
      window.removeEventListener('pointerup', h);
      window.removeEventListener('pointercancel', h);
    };
  }, []);

  function atClientX(x: number): void {
    const rect = rowRef.current?.getBoundingClientRect();
    if (!rect) return;
    putDrag(indexFromClientX(x, rect, stops.length));
  }

  function onPointerDown(e: React.PointerEvent<HTMLDivElement>): void {
    if (!usable || e.button !== 0) return;
    // 不让浏览器把这次按下解释成选中文字或拖走窗口（`lib/drag.ts` 会放过带
    // `role=slider` 的元素，但轨道容器不是那个元素）。
    e.preventDefault();
    dragging.current = true;
    try { e.currentTarget.setPointerCapture(e.pointerId); } catch { /* 不支持就靠 window 兜底 */ }
    // 点过之后方向键要能接着用：原生 input 被 pointer-events 挡住，拿不到焦点。
    inputRef.current?.focus();
    atClientX(e.clientX);
  }

  function onPointerMove(e: React.PointerEvent<HTMLDivElement>): void {
    if (!dragging.current) return;
    atClientX(e.clientX);
  }

  /** 键盘 / 任何没有按下过程的变更：吸附并**即时**下发。 */
  function onChange(raw: number): void {
    if (dragging.current) return;          // 拖动中原生 input 不该被驱动
    const want = snapToEnabled(stops, raw);
    if (want < 0) return;                  // 全表禁用：不动，也不发请求
    fire(want);
  }

  // title 说的是**当前落定的那一档**为什么不可用，不是拖到哪一档。
  // 拖动中本来也不会有 tooltip（按键没抬起来），跟着手指变只会多一个状态。
  const cur = stops[settled];
  const inFlight = pending != null && pending !== confirmed;
  // **拖动中显示预览档的文案**（plan §14 裁定 4）。松手之后立刻回到已确认档
  // ——预览是一个关于「松手会怎样」的陈述，手一松它就该让位给事实。
  const previewing = preview >= 0 && stops[preview] != null;
  const shownLabel = shownStopLabel(stops, confirmed, preview, value);
  const shownSub = shownStopSublabel(stops, confirmed, preview);

  return (
    <div className="stop-slider-box" data-testid={`${testid}-box`}>
      {/*
        `data-dragging` 拖动中必须**关掉过渡**，否则 thumb 会以 220 ms 的延迟
        追手指，手感比原来一格一格跳更糟。回弹靠的正是松手那一刻这个属性消失、
        位置同时跳到整数档 —— 过渡只在那一步生效。
        判据用 state 而不是 `dragging` 那个 ref：ref 变了不会重渲染，属性根本
        不会更新（写成 ref 的那一版看起来完全正常，只是永远没有回弹）。
      */}
      <div
        className="stop-slider-row"
        ref={rowRef}
        data-dragging={dragFrac != null ? 'true' : undefined}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={release}
        onPointerCancel={release}
      >
        {/*
          无障碍与键盘的本体。**视觉上完全不可见**（原生 thumb / track 都被
          CSS 抹掉），位置也不由它决定——它只负责 role=slider、焦点环的宿主、
          方向键、以及读屏念的那个值。
          `value` 用**已落定**的档（不是拖动中的连续位置）：读屏不该被告知一个
          用户还没松手确认的值。
        */}
        <input
          ref={inputRef}
          type="range"
          className="stop-slider"
          data-testid={testid}
          min={0}
          max={Math.max(0, stops.length - 1)}
          step={1}
          value={settled}
          disabled={!usable}
          aria-label={label}
          // 读屏跟着预览走：不跟的话，这次修正对键盘 / 读屏用户等于没做。
          // 用 `preview` 而不是 `settled`，与视觉上那个数字是同一个来源。
          aria-valuetext={shownStopLabel(stops, settled, preview, value)}
          aria-busy={inFlight}
          title={cur && cur.available === false ? (cur.why || '') : ''}
          onChange={(e) => onChange(e.currentTarget.valueAsNumber)}
        />
        <div className="stop-track" aria-hidden="true">
          <div className="stop-fill" style={{ width: trackPos(frac) }} />
          {stops.map((s, i) => (
            <span
              key={s.value}
              className={`stop-tick${s.available === false ? ' off' : ''}${i === lit ? ' on' : ''}`}
              style={{ left: trackPos(fracOf(i, stops.length)) }}
              title={s.available === false
                ? (s.why || '')
                : (s.sublabel ? `${s.label} — ${s.sublabel}` : s.label)}
            />
          ))}
          <div
            className="stop-thumb"
            data-testid={`${testid}-thumb`}
            style={{ left: trackPos(frac) }}
          />
        </div>
      </div>
      {/* 「看得见」与「已生效」是两个不同的承诺：`.previewing` 用字重把它们分开
          （下发仍在松手之后，见文件头）。同一个 testid 上两种状态，所以
          `data-preview` 把状态也导出去——只断言文本的回归分不出「实时预览」
          与「已经下发了」，而那正是这次要修的那件事。 */}
      <span
        className={`stop-slider-val${inFlight ? ' pending' : ''}${previewing ? ' previewing' : ''}`}
        data-testid={`${testid}-value`}
        data-preview={previewing ? 'true' : undefined}
      >
        {shownLabel}
        {/* 副标签只在有的时候出现。没有就什么都不画——**不拿主标签、id 或
            采样率凑一个**，那等于替 daemon 编一个它没说过的描述。 */}
        {shownSub ? (
          <span className="stop-slider-sub" data-testid={`${testid}-sub`}>{shownSub}</span>
        ) : null}
      </span>
    </div>
  );
}
