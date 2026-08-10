// 图标状态的判定规则（纯函数，不碰 DOM、不读 store）。
//
// 与 lib/appearance.ts 对 appearanceHost.ts 是同一种分法：规则可单测，取值的那
// 一半留在 state/connection.ts 的 syncTray 里。
//
// **这里刻意不新造语义。** 四个取值全部来自已有的两处事实：
//
//   offline / connecting  ——  ConnState 本身（state/store.ts）
//   idle / active         ——  会话列表空不空，即 sessions 视图已经显示着的事
//
// 于是菜单栏图标和界面永远不可能各说各话：它们读的是同一个量。真正的判断只有
// 一处，就是 `starting` 与 `connecting` 合并——对用户是同一句话（还没连上，正在
// 弄），分开只会让图标在启动路径上多抖一次。

import type { ConnState } from '../state/store';

/** 图标能表达的状态。顺序即「信号由弱到强」，与 icons/make-icons.py 的 STATES
 *  和 src-tauri/src/icon.rs 的 IconState 一致——三处按名字对齐，改名要一起改。 */
export type IconState = 'offline' | 'connecting' | 'idle' | 'active';

export interface IconStateInput {
  conn: ConnState;
  /** 活跃会话数。只看空不空，不看方向也不看 kind。 */
  sessionCount: number;
}

/**
 * 连接状态 + 会话数 → 图标状态。
 *
 * `active` 要求 conn === 'online'：会话列表在掉线后不会立刻清空（close 事件先到
 * 还是 conn 先翻是竞态），若只看 sessionCount，掉线瞬间会短暂显示成「正在传输」。
 */
export function iconStateFrom({ conn, sessionCount }: IconStateInput): IconState {
  if (conn === 'offline') return 'offline';
  if (conn !== 'online') return 'connecting';
  return sessionCount > 0 ? 'active' : 'idle';
}
