// Minimise / maximise / close, drawn by the app because Windows no longer
// draws them (`decorations: false`, see `tauri.windows.conf.json`).
//
// The three are not symmetric, and the asymmetry is the whole design:
//
//   * **Minimise and close are ordinary client-area buttons.** The webview sees
//     the mouse, so `:hover` and `onClick` work with no help from Rust.
//   * **Maximise is not.** `win_chrome.rs` answers `WM_NCHITTEST` with
//     `HTMAXBUTTON` over its rectangle so that Windows 11 pops the snap-layout
//     picker on hover -- and the price is that the same rectangle stops
//     delivering mouse events to the page. Its hover, press, and the click
//     itself all arrive from Rust instead (`lib/caption.ts`).
//
// So this component renders one button that behaves normally and one whose
// visual state is entirely remote-controlled, and they have to look identical.
//
// Close means hide, matching the window's `CloseRequested` handler: the daemon
// keeps running and the tray brings the UI back.

import { useEffect, useState } from 'react';
import { RawIcon } from './Icon';
import { onCaptionSignal } from '../lib/caption';
import { appDrawsWindowControls } from '../lib/platform';
import { tauriInvoke } from '../ipc/endpoint';
import { toast } from './Toasts';
import { t } from '../i18n';

type WindowCmd = 'minimize_window' | 'toggle_window_zoom' | 'hide_window';

// Same reasoning as `lib/drag.ts`: a shell command that silently does nothing
// is the failure mode that costs the most time to diagnose, but repeating the
// toast on every click just makes it noisier.
let reported = false;

async function call(cmd: WindowCmd): Promise<void> {
  try {
    await tauriInvoke(cmd);
  } catch (err) {
    if (reported) return;
    reported = true;
    toast(t('chrome.captionFailed', { message: err instanceof Error ? err.message : String(err) }), 'error');
  }
}

export function CaptionButtons() {
  const [maximized, setMaximized] = useState(false);
  const [hover, setHover] = useState(false);
  const [press, setPress] = useState(false);

  const shown = appDrawsWindowControls();

  useEffect(() => {
    if (!shown) return;
    // Ask once at mount: the window can already be maximised when the UI
    // reloads, and Rust only pushes on *change*.
    void tauriInvoke<boolean>('is_window_maximized').then(setMaximized).catch(() => {});
    return onCaptionSignal((kind, on) => {
      if (kind === 'hover') setHover(on);
      else if (kind === 'press') setPress(on);
      else setMaximized(on);
    });
  }, [shown]);

  if (!shown) return null;

  return (
    <div className="caption-buttons" data-testid="caption-buttons">
      <button
        type="button"
        className="caption-btn"
        data-testid="caption-minimize"
        aria-label={t('chrome.minimize')}
        title={t('chrome.minimize')}
        onClick={() => void call('minimize_window')}
      >
        <RawIcon name="winMin" />
      </button>

      {/* The odd one out. Over this rectangle `win_chrome.rs` answers
          `WM_NCHITTEST` with `HTMAXBUTTON` so Windows 11 pops the snap-layout
          picker on hover — and the price is that Windows then routes the mouse
          there as *non-client*: no `mousemove`, no `click`, no CSS `:hover`.
          The `hover`/`press` classes therefore arrive from Rust.

          The `onClick` stays anyway, and that is not belt-and-braces for its
          own sake: exactly one of the two paths can fire (if the point really
          is non-client the webview never sees the click; if the hit test ever
          fails to claim it, the webview does), so keeping both is what makes
          "the maximize button does nothing" unreachable. Keyboard users reach
          it through the same handler. */}
      <button
        type="button"
        className={`caption-btn remote${hover ? ' hover' : ''}${press ? ' press' : ''}`}
        data-testid="caption-maximize"
        data-state={maximized ? 'restore' : 'maximize'}
        aria-label={maximized ? t('chrome.restore') : t('chrome.maximize')}
        title={maximized ? t('chrome.restore') : t('chrome.maximize')}
        onClick={() => void call('toggle_window_zoom')}
      >
        <RawIcon name={maximized ? 'winRestore' : 'winMax'} />
      </button>

      <button
        type="button"
        className="caption-btn danger"
        data-testid="caption-close"
        aria-label={t('chrome.close')}
        title={t('chrome.close')}
        onClick={() => void call('hide_window')}
      >
        <RawIcon name="close" />
      </button>
    </div>
  );
}
