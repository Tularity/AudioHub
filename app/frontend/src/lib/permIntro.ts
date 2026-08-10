// 「第一次运行时自动弹一次系统权限面板」的判据（用户 2026-08-10 第 22 条）。
//
// ⚠ 与 `state/store.ts` 那条「权限全部只活在内存里，不落任何『已看过』标记」的硬
// 规矩的边界，必须在这里说死，否则下一个人会顺手把这份落盘物接到授权门上——
// 而那正是 `views/Onboarding.tsx` 明令禁止的事：
//
//   · 这里落盘的是**介绍弹层的确认签名**，它**不参与 `gateVisible()`**
//     （`state/connection.ts` 一个字不改）；
//   · 它只影响这一个 Sheet 的**自动**弹出，用户随时能从「设置 › 杂项」手动打开；
//   · 它随「未授权集合」的变化**自动失效**——所以它不是一个「以后别再提醒我」的
//     永久开关，而是一句「这一组权限我看过了」。
//
// 为什么是**签名**而不是布尔量：布尔量会把「不再提醒」永久钉死。日后新增一项权限、
// 或者用户在系统设置里**撤销**了麦克风，未授权集合就变了——签名随之不匹配，面板
// 该弹就弹，而那正是应该再弹的时刻。

/** localStorage 的键。App 侧偏好，不进 IPC 契约（与快捷键 override 同类）。 */
export const PERM_SEEN_KEY = 'audiohub.ui.permSeen';

export interface PermLike { id: string; status: string }

/**
 * 当前「未授权集合」的签名：所有 `status !== 'granted'` 的 id 排序后用 `|` 连起来。
 *
 * 排序是必需的：daemon 回包的顺序不保证稳定，不排序会让同一组权限在两次启动里
 * 算出两个签名，于是每次启动都弹。
 *
 * 全绿时返回空串——空串同时也是「没有任何理由弹」的判据（见下）。
 */
export function pendingSignature(list: readonly PermLike[]): string {
  if (!Array.isArray(list)) return '';
  return list
    .filter((p) => p && p.status !== 'granted')
    .map((p) => String(p.id))
    .sort()
    .join('|');
}

export interface AutoOpenInput {
  /** 服务连上了吗。没连上时权限也探不出来。 */
  online: boolean;
  /** 探测有结果了吗。没结果就弹，弹出来是一屏「正在探测…」。 */
  probed: boolean;
  /** 首启授权门此刻挡着吗。 */
  gateVisible: boolean;
  /** 是不是 Tauri 宿主（浏览器态不自动弹，理由见下）。 */
  tauri: boolean;
  /** `pendingSignature()` 的结果。 */
  signature: string;
  /** localStorage 里存着的签名，没有则 null。 */
  seen: string | null;
}

/**
 * 该不该自动弹。五条**全**满足才弹。
 *
 * | # | 条件 | 为什么 |
 * |---|---|---|
 * | 1 | 在线且已探测 | 没探测出结果就弹，弹出来是一屏「正在探测…」 |
 * | 2 | 授权门没挡着 | Sheet 的 z-index 60 高于门的 40，会浮在门之上；两层叠着是最糟的首启体验，而门本身已经在说同一件事 |
 * | 3 | 至少一项未授权 | **单独保证「已授权过的用户永远不会被弹」**——全绿就不弹，不需要任何标记 |
 * | 4 | 落盘签名与当前未授权集合不匹配 | 只解决一种情况：用户看过并关掉了，但那一项在 macOS 上**永远不会变成 granted**（`local_network` / `system_audio` 都没有查询 API）。没有这一条，那类用户每次启动都会被弹——这是本条唯一的失败模式 |
 * | 5 | Tauri 宿主 | 浏览器态点「授权」会在**服务端那台机器**上弹系统对话框，而点按钮的人多半不在那台机器前 |
 */
export function shouldAutoOpenPermissions(i: AutoOpenInput): boolean {
  if (!i.tauri) return false;
  if (!i.online || !i.probed) return false;
  if (i.gateVisible) return false;
  if (i.signature === '') return false;
  return i.seen !== i.signature;
}

/**
 * localStorage 在若干真实配置下会**直接抛异常**（Safari 无痕、内嵌源被禁三方
 * 存储）。一个「弹过没有」的偏好不值得把应用带下去，所以读写都兜住：
 * 读失败当没看过（顶多多弹一次），写失败当没存住（下次再弹一次）。
 */
export function readPermSeen(): string | null {
  try { return window.localStorage.getItem(PERM_SEEN_KEY); } catch { return null; }
}

export function writePermSeen(signature: string): void {
  try { window.localStorage.setItem(PERM_SEEN_KEY, signature); } catch { /* 偏好而已 */ }
}
