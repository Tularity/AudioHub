import { useEffect, useReducer, useSyncExternalStore } from 'react';

/** 每 ms 触发一次重渲（倒计时、uptime 这类「时间自己在走」的显示）。 */
export function useTick(ms: number, enabled = true): void {
  const [, bump] = useReducer((x: number) => x + 1, 0);
  useEffect(() => {
    if (!enabled) return;
    const t = setInterval(bump, ms);
    return () => clearInterval(t);
  }, [ms, enabled]);
}

/**
 * 「进行中的操作」集合。刻意做成模块级而不是组件 state：一次 session.open 最坏
 * 25s，期间用户完全可能切到别的页面再回来——挂在组件上，pending 态会随卸载消失，
 * 回来后开关看着是空闲的，再点一次就发出第二个请求。
 */
export function createBusySet() {
  const set = new Set<string>();
  const listeners = new Set<() => void>();
  let snapshot: readonly string[] = [];
  const refresh = () => {
    snapshot = [...set];
    for (const fn of [...listeners]) fn();
  };
  return {
    has: (k: string) => set.has(k),
    add(k: string) { set.add(k); refresh(); },
    delete(k: string) { set.delete(k); refresh(); },
    /** 组件订阅：返回当前 key 列表（引用只在集合变化时更新）。 */
    use(): readonly string[] {
      return useSyncExternalStore(
        (fn) => { listeners.add(fn); return () => listeners.delete(fn); },
        () => snapshot,
      );
    },
  };
}
