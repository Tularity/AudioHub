import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

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
