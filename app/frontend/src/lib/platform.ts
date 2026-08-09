// Which end of the title strip the *operating system* owns, and which end is
// therefore ours.
//
// This is the single direction variable the whole chrome is driven by. macOS
// puts its window controls (the traffic lights) at the leading edge; Windows
// puts min/max/close at the trailing edge. Everything else in the top strip --
// the daemon badge, the reserved gutters, the caption buttons we draw
// ourselves -- is derived from this one fact rather than from a per-platform
// copy of the layout. There is exactly one `<header>`, one `DaemonBadge`, one
// set of CSS rules; only `document.body[data-window-controls]` differs, and
// `styles.css` reads it in one place per property.
//
// The alternative -- branching the markup on `IS_MAC` -- was rejected on
// purpose: `docs/plan.md` §3.1 asks for a unified two-platform layout, and two
// JSX branches would drift the moment either one is edited alone.
//
// `docs/spec-ui.md` §2 used to freeze the layout as pixel-identical across both
// platforms, which this contradicts; that clause is now narrowed to "same
// structure, gutters mirrored to follow the platform's own controls". The
// project's own wording is "unified layout *preferred*", and the difference
// here is not a style choice -- it is where Microsoft and Apple put their
// buttons.

import { IS_MAC } from './fmt';
import { isTauri } from '../ipc/endpoint';

/** Which edge of the window an element is pinned to. */
export type ChromeSide = 'left' | 'right';

/**
 * Where the OS (or, on Windows, our stand-in for it) draws the window
 * controls. macOS: leading. Windows: trailing.
 */
export const WINDOW_CONTROLS_SIDE: ChromeSide = IS_MAC ? 'left' : 'right';

/** The free end -- always the opposite of the controls. The daemon badge lives here. */
export const STATUS_SIDE: ChromeSide = WINDOW_CONTROLS_SIDE === 'left' ? 'right' : 'left';

/**
 * True when *we* are responsible for painting min/max/close.
 *
 * Two conditions, both required. macOS keeps its real traffic lights (they are
 * drawn by AppKit above the webview and we only move them), so the app never
 * draws its own. And in web-access mode (`settings.web`, plan §7.5) the UI runs
 * in somebody's browser tab, where a close button would be a lie -- there is no
 * window to close.
 */
export function appDrawsWindowControls(): boolean {
  return isTauri() && WINDOW_CONTROLS_SIDE === 'right';
}

/**
 * Publish the direction to CSS. Called once at boot next to the `is-tauri`
 * toggle; the attribute -- not a JS style object -- is what the stylesheet
 * keys off, so the swap costs one selector rather than a second layout.
 */
export function applyChromeDirection(): void {
  const b = document.body;
  b.dataset.windowControls = WINDOW_CONTROLS_SIDE;
  b.classList.toggle('app-captions', appDrawsWindowControls());
}
