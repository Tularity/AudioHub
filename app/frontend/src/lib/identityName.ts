// 本机名称的前端规则（用户 2026-08-10 第 9 条：本机身份可自定义，默认用本机名称）。
//
// 这个字符串不只是本机界面上的一行字：它会成为**每一台对端系统设备列表里那两台
// 虚拟设备的名字**（plan §7.1）。所以它要在前端与 daemon **各拒一次**——前端这一
// 层管长度与空白，daemon 那一层管它自己的合法性。两层都拒不是重复劳动：前端拒的
// 那次是为了当场给反馈，daemon 拒的那次是为了 CLI / 旧前端 / 手改配置文件也拦得住。

/**
 * 长度上限。与对端别名框（`views/Detail.tsx` 的 `maxLength`）取同一个数：两者最终
 * 都落在系统设备名上，给它们两个不同的上限没有任何道理。
 */
export const NAME_MAX = 48;

/**
 * 把用户敲进去的东西收敛成可以送给 daemon 的值。
 *
 * - 换行与控制字符**直接删掉**，不是替换成空格：它们进了设备名会让 macOS 的声音
 *   设置里出现一行断掉的名字，而用户在输入框里根本看不出来自己粘了个 `\n` 进来。
 * - `trim()` 之后为空 ⇒ 返回空串，语义是**清除覆盖**（回到本机名称），
 *   不是「把名字设成空的」。
 * - 超长**截断**而不是拒绝：用户粘一段长文本进来，截断能让他立刻看见结果，
 *   报错只能让他自己去数字数。
 */
export function normalizeLocalName(raw: string): string {
  const stripped = raw.replace(/[\u0000-\u001f\u007f]/g, '');
  return stripped.trim().slice(0, NAME_MAX);
}

/** 名字从哪儿来的。daemon 回包里的 `name_source`。 */
export type NameSource = 'env' | 'custom' | 'hostname';

export function parseNameSource(v: unknown): NameSource {
  return v === 'env' || v === 'custom' ? v : 'hostname';
}

/**
 * 这个输入框此刻能不能编辑。
 *
 * `AUDIOHUB_NAME` 生效时**只读**：那个环境变量是 regress 的多实例调试用来区分守护
 * 进程的，优先级必须最高。允许在界面上改一个改不动的值，只会让用户以为自己保存
 * 失败了。
 */
export function nameEditable(source: NameSource): boolean {
  return source !== 'env';
}

/**
 * 「保存」按钮该不该亮。
 *
 * 与当前生效值相同就不该亮——一个按下去什么也不会发生的按钮，用户点完只能靠
 * 「界面没变化」去猜是成功了还是没反应。
 */
export function nameDirty(draft: string, current: string): boolean {
  return normalizeLocalName(draft) !== normalizeLocalName(current);
}

/**
 * 「恢复默认」按钮该不该亮：只有当前确实存在一份用户覆盖时才有东西可恢复。
 */
export function canRestoreDefault(source: NameSource): boolean {
  return source === 'custom';
}
