// App identity that is not the name: the build-time version.
//
// 左上角的品牌区（logo + 名字）已经删掉，改由背景水印表达标识（见 Chrome.tsx 的
// Watermark）。于是「这是哪个 App、哪个版本」在界面里没有了常驻落点——macOS 还有
// 菜单栏与 Dock 兜着，Windows 一旦按 docs/design-ui-chrome.md §3 去掉系统顶栏，
// 连窗口标题都不剩。设置页的「关于」区块就是那个落点，而它显示的版本必须来自
// **打包时的权威来源**：`app/src-tauri/tauri.conf.json` 的 `version`。
// 前端 package.json 里也写着一个 version，但那是 npm 的账本，不是发给用户的号；
// 两份迟早漂开，而用户报 bug 时贴给我们的是这一处。
//
// 注入方式是 vite.config.ts 的 `define`，即**编译期文本替换**。因此：
//   · 生产构建里 `__APP_VERSION__` 已经是一个字面量字符串；
//   · vitest 走的是 vitest.config.ts，那份 define 不生效，标识符在运行期根本不存在。
// 所以下面用 `typeof` 做守卫——对一个**未声明**的标识符，`typeof` 是唯一不抛
// ReferenceError 的读法（`__APP_VERSION__ === undefined` 会当场炸）。

declare const __APP_VERSION__: string | undefined;

/**
 * 打包时写入的应用版本。
 *
 * **拿不到就返回 `null`，绝不返回 `'0.0.0'` / `'dev'` 之类的占位版本**：一个编出来
 * 的版本号会被用户原样贴进 bug 报告，届时我们会照着一个不存在的版本去查，比一个
 * 明摆着的「—」难查得多。调用方负责把 `null` 渲染成 `common.dash`。
 */
export function appVersion(): string | null {
  const raw = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : '';
  const trimmed = raw.trim();
  return trimmed.length > 0 ? trimmed : null;
}
