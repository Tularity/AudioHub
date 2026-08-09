import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// 版本号只有一个权威来源：`app/src-tauri/tauri.conf.json` 的 `version`——它就是打进
// .app / .exe 的那个号。前端 package.json 里也有一个 version，但那是 npm 的账本；
// 两份迟早漂开，而设置页「关于」显示给用户、用户贴进 bug 报告的必须是前者。
// 这里读出来，编译期钉进产物（见 src/lib/appInfo.ts）。
const TAURI_CONF = fileURLToPath(new URL('../src-tauri/tauri.conf.json', import.meta.url));
const APP_VERSION: unknown = JSON.parse(readFileSync(TAURI_CONF, 'utf8')).version;
if (typeof APP_VERSION !== 'string' || APP_VERSION.trim() === '') {
  // 悄悄注入一个空版本，界面上会显示成「—」，而那看起来像运行期没连上，
  // 不像构建配置写错了。宁可当场把构建停在这里。
  throw new Error(`vite.config: no usable "version" string in ${TAURI_CONF}`);
}

// 构建产物直接落在 `app/ui/`。这不是随手选的路径：
//   · `app/src-tauri/tauri.conf.json` 的 frontendDist 指向 `../ui`；
//   · `regress/r4_app.sh` 用 `python3 -m http.server` 直接服 `app/ui`，
//     以 `index.html?port&token` 打开做浏览器态回归——那个脚本本轮不可改动。
// 所以 outDir 换成别处会当场打断回归；源码则搬到 app/frontend/ 与产物分家。
export default defineConfig({
  plugins: [react()],
  // 相对路径：无论被 Tauri 以 tauri://localhost 加载，还是被 http.server 从任意
  // 目录服出去，资源都能解析到。
  base: './',
  define: {
    __APP_VERSION__: JSON.stringify(APP_VERSION),
  },
  build: {
    outDir: '../ui',
    emptyOutDir: true,
    // tauri.conf.json 的 CSP 是 `script-src 'self'`（没有 unsafe-inline，也没有
    // nonce 注入）。Vite 默认会为 <link rel=modulepreload> 注入一段**内联**
    // polyfill 脚本，在那条 CSP 下会被直接拦掉——首屏白屏且只在 Tauri 里复现。
    // 关掉它没有代价：modulepreload 只是预取提示，模块本身仍由
    // <script type="module"> 加载（Safari 11+ / WebView2 全支持）。
    modulePreload: { polyfill: false },
    sourcemap: false,
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
