import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { initAppearance } from './lib/appearanceHost';
import './styles.css';

// 主题与语种在 **React 挂载之前**落地。
//
// 主题必须在这里：`<html data-theme>` 定的是整页配色，晚一帧写就等于让用户看见一帧
// 错误主题再闪一下。这也是把偏好放 localStorage 而不是 daemon settings 的直接原因
// ——后者要等 IPC 连上才知道，那一帧闪白是结构性的（见 lib/appearance.ts 的说明）。
//
// 语种顺带在这里定 `<html lang>`：屏幕阅读器的发音、字体回退、断行规则都跟着它走。
// 原先是 `setLocale(navigator.language)` 一行，现在多一层「跟随系统 / 指定语言」的
// 用户偏好，解析规则在 lib/appearance.ts。
initAppearance();

const host = document.getElementById('root');
if (!host) throw new Error('missing #root');
createRoot(host).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
