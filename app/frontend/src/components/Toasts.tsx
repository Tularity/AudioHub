// toast：命令式 API（连接层、RPC 失败处都要能直接喊一声），React 只负责渲染。
// 因此队列存在模块里，用 useSyncExternalStore 订阅——不是 React state。

import { useCallback, useEffect, useRef, useSyncExternalStore } from 'react';
import { useExitMotion } from '../lib/dialogMotion';

export type ToastKind = 'info' | 'ok' | 'warn' | 'error';

interface ToastItem { id: number; msg: string; kind: ToastKind; out: boolean }

let items: ToastItem[] = [];
let nextId = 1;
const listeners = new Set<() => void>();

function emit(): void {
  for (const fn of [...listeners]) fn();
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function toast(msg: unknown, kind: ToastKind = 'info'): void {
  const id = nextId++;
  items = [...items, { id, msg: String(msg), kind, out: false }];
  emit();
  setTimeout(() => {
    items = items.map((t) => (t.id === id ? { ...t, out: true } : t));
    emit();
  }, 3200);
}

function ToastEntry({ item }: { item: ToastItem }) {
  const ref = useRef<HTMLDivElement>(null);
  const finish = useCallback(() => {
    items = items.filter(t => t.id !== item.id);
    emit();
  }, [item.id]);
  useExitMotion(item.out, ref, null, finish);
  return <div ref={ref} className={`toast ${item.kind}${item.out ? ' out' : ''}`}>{item.msg}</div>;
}

export function Toasts() {
  const list = useSyncExternalStore(subscribe, () => items);
  // 卸载时把队列清空，免得残留的定时器往一个不存在的容器里塞节点。
  useEffect(() => () => { items = []; }, []);
  return (
    <div id="toasts" aria-live="polite">
      {list.map((t) => (
        <ToastEntry key={t.id} item={t} />
      ))}
    </div>
  );
}
