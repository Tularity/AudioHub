import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { setLocale } from './i18n';
import './styles.css';

// `<html lang>` 由活动语种决定，而不是写死在 index.html 里：将来加语种时，
// 屏幕阅读器的发音、字体回退、断行规则都跟着这一处走。
setLocale(navigator.language);

const host = document.getElementById('root');
if (!host) throw new Error('missing #root');
createRoot(host).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
