// 开机自启开关的状态判定（plan M9）。
//
// 拆成纯函数而不是留在 `Settings.tsx` 里，是因为这里有一格**极易写错、且写错
// 之后界面看上去完全正常**：daemon 报的 `supported` 与 `enabled` 是两个正交的量，
// 四种组合全都真实存在。最要命的一格是 `supported=false && enabled=true`——
// 登录项活得比写下它的那个 bundle 长（在装好的 .app 里开过自启，之后换成开发
// 构建跑 daemon，或者 bundle 被挪走）。
//
// 那一格上，「按 supported 置灰」会得到一个**用户关不掉的开关**：系统每次登录
// 照样把 App 拉起来，而界面上唯一的入口是灰的。daemon 侧对应地放开了撤销方向
// （`core/audiohubd/src/autostart.rs` 的 `plan_set`：注册要形态，撤销不要），
// 界面这一侧必须跟上，否则那条修复根本没有入口能触达。

/** daemon 上报的四个字段，抄自 `DaemonSettings`（此处只取用得到的那几个）。 */
export type AutostartFields = {
  autostart?: boolean;
  autostart_supported?: boolean;
  autostart_target?: string | null;
  autostart_reason?: string | null;
};

/**
 * 说明文字该说哪一句。**判别式而不是布尔组合**：让「说什么」这件事在一处收敛，
 * 视图只负责把它翻成语料键。
 */
export type AutostartNote =
  /** 字段整个缺席 = 旧 daemon。与「这台机器不行」是两回事，用户的下一步也不同。 */
  | 'unknown'
  /** 注册不了，也确实没注册。带理由。 */
  | 'unsupported'
  /** ⚠ 注册着，但当前形态注册不了新的：这条是别的形态留下的，可以关掉。 */
  | 'orphaned'
  /** 能开，没开。 */
  | 'off'
  /** 开着，一切正常。无需解释。 */
  | 'on';

export type AutostartView = {
  /** daemon 提供这组字段吗。 */
  known: boolean;
  /** 当前形态能不能**注册**。 */
  supported: boolean;
  /** 此刻真的注册着（daemon 探测出来的事实）。 */
  on: boolean;
  /** 登录项指向哪里；空串表示无可显示。 */
  target: string;
  /** `supported === false` 时 daemon 给的人话理由。 */
  reason: string;
  /** 开关能不能点。 */
  enabled: boolean;
  note: AutostartNote;
};

/**
 * 开关能不能点。
 *
 * **`supported || on`**，不是 `supported`。第二项就是那条修复的入口：一条已经
 * 注册着的登录项，无论当前形态配不配注册，都必须关得掉。daemon 的 `plan_set`
 * 对同一件事的表述是「注册要形态，撤销不要」。
 */
export function autostartToggleable(supported: boolean, on: boolean): boolean {
  return supported || on;
}

export function autostartNote(known: boolean, supported: boolean, on: boolean): AutostartNote {
  if (!known) return 'unknown';
  if (supported) return on ? 'on' : 'off';
  return on ? 'orphaned' : 'unsupported';
}

export function autostartView(ds: AutostartFields | null | undefined): AutostartView {
  // 字段整个缺席 = 旧 daemon。用 `!== undefined` 而不是真值判断：`false` 是一个
  // 合法且有意义的回答（「这台机器不行」），把它折进「不知道」会让理由那一句
  // 无处显示。
  const known = !!ds && ds.autostart_supported !== undefined;
  const supported = !!(ds && ds.autostart_supported);
  const on = !!(ds && ds.autostart);
  return {
    known,
    supported,
    on,
    target: (ds && ds.autostart_target) || '',
    reason: (ds && ds.autostart_reason) || '',
    enabled: autostartToggleable(supported, on),
    note: autostartNote(known, supported, on),
  };
}
