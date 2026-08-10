//! Giving Windows back the things `decorations: false` takes away that the app
//! cannot draw for itself: **Snap Layouts**, the non-client mouse messages that
//! come with them, and the **system window menu** on right-click. It also turns
//! off the one thing `decorations: false` *adds* and should not:
//! WebView2's browser context menu (see `silence_webview_context_menu`).
//!
//! # What this is for
//!
//! The goal on Windows is VS Code's: the client area extends over the title
//! strip, the app's own nav pill is painted there, and min/max/close move into
//! the app's chrome. Tauri has no `titleBarStyle` on Windows —
//! `tauri-utils-2.9.3/src/lib.rs:161` documents it as macOS-only — so the only
//! route is `decorations: false` (see `tauri.windows.conf.json`) plus caption
//! buttons of our own.
//!
//! That costs one thing users notice without being able to name: hovering the
//! maximize button on Windows 11 pops the snap-layout picker. It only appears
//! for a window that answers `WM_NCHITTEST` with `HTMAXBUTTON`, and
//! `grep HTMAXBUTTON` over `tao-0.35.3/src/platform_impl/windows/` returns
//! nothing — tao's hit test (`event_loop.rs:2178`) handles resize borders only,
//! and its `WM_NCCALCSIZE` (`:2124`) returns 0 outright for an undecorated
//! window. So the app has to answer for itself, which means a window subclass.
//! This is the same answer Electron reached; Microsoft's own guidance
//! ("Support snap layouts for desktop apps on Windows 11") gives exactly this
//! `WM_NCHITTEST` snippet.
//!
//! # The part the documentation leaves out
//!
//! Once a point answers `HTMAXBUTTON`, Windows routes the mouse there as
//! *non-client*: the webview stops receiving `mousemove`/`mousedown` over that
//! rectangle, so CSS `:hover` dies and the click never lands. A button that
//! pops the snap picker but does not highlight or depress is worse than no snap
//! picker. `WM_NCMOUSEMOVE` / `WM_NCMOUSELEAVE` / `WM_NCLBUTTONDOWN` /
//! `WM_NCLBUTTONUP` are therefore handled here and the state pushed into the
//! page.
//!
//! **Pushed by `eval`, deliberately.** The obvious channel is `emit` +
//! `listen`, but `listen` is `plugin:event|listen` and this app ships no
//! capabilities file at all (`gen/schemas/capabilities.json` is `{}`) — every
//! window operation goes through an app command precisely so the plugin ACL
//! never enters the picture (`main.rs`, `start_window_drag`). Rust-to-JS `eval`
//! is not ACL-governed, so it keeps that property. The page exposes
//! `window.__audiohubCaption(kind, value)`; when the frontend has not installed
//! it yet the eval is a no-op by construction.
//!
//! # Snap Layouts does not actually work yet, and the reason is not in this file
//!
//! Measured on 30-win (Windows 11 24H2, build 26100, 150% scale) with the code
//! as it stands. Recorded here because everything on our side now looks right,
//! so the next reader would otherwise re-derive it:
//!
//! * `WM_NCHITTEST` sent straight to our window at the maximize button's centre
//!   answers **9 = HTMAXBUTTON**. The rectangle below is correct.
//! * The window has `WS_MAXIMIZEBOX`, `WS_SYSMENU`, `WS_CAPTION` and is not
//!   `WS_POPUP` — every style prerequisite holds.
//! * The window is genuinely foreground when tested, and Notepad on the same
//!   desktop pops the flyout, so the feature is on.
//! * And the flyout still does not appear.
//!
//! `WindowFromPoint` at that same point is what explains it. It does not return
//! our window:
//!
//! ```text
//! TOP   class='Tauri Window'                pid=142840 (audiohub-app)
//! MID   class='Chrome_WidgetWin_1'          pid=114384 (msedgewebview2)
//! LEAF  class='Chrome_RenderWidgetHostHWND' pid=114384 (msedgewebview2)
//! ```
//!
//! WebView2's windowed hosting puts child HWNDs over the whole client area,
//! including the strip we draw caption buttons on, and **they belong to
//! `msedgewebview2.exe`, a different process**. Real mouse input is routed to
//! the deepest child under the cursor, so the OS asks *those* windows to hit
//! test and never consults ours — our HTMAXBUTTON is only ever seen by someone
//! who sends `WM_NCHITTEST` directly. The usual escape hatch, subclassing the
//! child to return `HTTRANSPARENT` over that rectangle, needs a callback inside
//! the owning process, and `SetWindowSubclass` cannot cross a process boundary.
//!
//! So the remaining fix is not a message-handling tweak; it is one of:
//! stop the webview covering the top strip (costs the design), or move wry off
//! windowed hosting onto `ICoreWebView2CompositionController`, where the host
//! owns input routing. Both are larger than this module.
//!
//! What is kept here is still necessary: the `WM_NCMOUSEMOVE` handler below
//! must not swallow the message, because the flyout timer lives in
//! `DefWindowProc`. That was a second, independent blocker, and any future fix
//! needs it gone too.
//!
//! # Geometry
//!
//! The maximize button's rectangle is computed here rather than pushed from the
//! frontend, so hit-testing is correct from the first message rather than from
//! whenever the first frame happens to report its layout. That makes
//! `CAPTION_BUTTON_W` / `CAPTION_BUTTON_H` a contract with `styles.css`;
//! `app/frontend/src/lib/caption.test.ts` reads this file and fails if the two
//! drift.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use tauri::{AppHandle, Manager};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    EnableMenuItem, GetClientRect, GetCursorPos, GetSystemMenu, IsZoomed, PostMessageW,
    SetForegroundWindow, ShowWindow, TrackPopupMenu, HTCLIENT, HTMAXBUTTON, MENU_ITEM_FLAGS,
    MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE, SC_RESTORE,
    SC_SIZE, SW_MAXIMIZE, SW_RESTORE, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN,
    WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE,
    WM_SYSCOMMAND,
};

/// Caption button box, in logical pixels. **Mirrored in `styles.css` as
/// `--caption-btn-w` / `--caption-btn-h`** — Windows 11's own caption buttons
/// are 46x32 DIP, and matching them is what makes the strip read as native.
/// A test cross-checks the two sides; see the module docs.
const CAPTION_BUTTON_W: f64 = 46.0;
const CAPTION_BUTTON_H: f64 = 32.0;

/// Right-to-left order in the strip: close, maximize, minimize. Maximize is the
/// only one Windows needs to know about, and it is the second box in.
const MAXIMIZE_SLOT_FROM_RIGHT: f64 = 1.0;

const SUBCLASS_ID: usize = 0x4155_4448; // 'AUDH'

static APP: OnceLock<AppHandle> = OnceLock::new();
static HOVERED: AtomicBool = AtomicBool::new(false);
static PRESSED: AtomicBool = AtomicBool::new(false);
/// Last maximized state pushed to the page. `WM_SIZE` arrives on every frame of
/// a live drag-resize; this keeps that from turning into a JS eval per frame.
/// -1 means "nothing pushed yet", so the first message always reports.
static LAST_ZOOMED: AtomicIsize = AtomicIsize::new(-1);

/// Attach the subclass. Safe to call once, at setup.
pub fn install(app: &AppHandle, window: &tauri::Window) {
    let _ = APP.set(app.clone());
    let Ok(hwnd) = window.hwnd() else { return };
    // SAFETY: `hwnd` is this window's live handle and the proc is a plain
    // `extern "system"` function with no captured state.
    unsafe {
        let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
    }
}

/// Turn off WebView2's own context menu.
///
/// Right-clicking anywhere in the page — including the title strip, which is
/// client area here — otherwise pops Edge's browser menu: Back, Refresh, Save
/// as, Print, *Send tab to your devices*. In a desktop app that menu is not a
/// rough edge, it is a category error: half its entries act on a "page" the
/// user has no model of, and `Save as` writes the app's own UI to disk.
///
/// There is no Tauri setting for it. `WindowConfig` has `devtools`,
/// `browser_extensions_enabled`, `zoom_hotkeys_enabled` and
/// `additional_browser_args` (`tauri-utils-2.9.3/src/config.rs:2083-2257`) but
/// nothing for context menus, and the Windows attribute translation in
/// `tauri-runtime-wry-2.11.4/src/lib.rs:5053` never calls the wry builder
/// method that would do it (`WebViewBuilderExtWindows::with_default_context_menus`,
/// `wry-0.55.1/src/lib.rs:1728`). Tauri owns the builder, so the only way in is
/// the live settings object behind `with_webview` — which is what wry's own
/// builder ends up touching anyway (`wry-0.55.1/src/webview2/mod.rs:571`).
///
/// `devtools: false` is not a substitute: it removes *Inspect* and leaves the
/// rest.
///
/// The trade-off, stated because it is a real one: this also removes the
/// cut/copy/paste menu inside text inputs. The keyboard shortcuts are
/// unaffected, and a browser menu on a text field is a much smaller wrong than
/// a browser menu on the title bar.
pub fn silence_webview_context_menu(window: &tauri::WebviewWindow) {
    let _ = window.with_webview(|webview| {
        // SAFETY: every call below is an inherent method on a live COM
        // interface handed to us by Tauri, on the thread that owns the
        // webview; each returns a `Result` we check before going deeper.
        unsafe {
            let Ok(core) = webview.controller().CoreWebView2() else {
                return;
            };
            let Ok(settings) = core.Settings() else { return };
            let settings: ICoreWebView2Settings = settings;
            let _ = settings.SetAreDefaultContextMenusEnabled(false);
        }
    });
}

/// Pop the real system window menu (Restore / Move / Size / Minimize /
/// Maximize / Close) at the cursor.
///
/// `decorations: false` means Windows never does this for us: the standard
/// right-click-the-title-bar gesture reaches a caption we draw ourselves, in
/// client area, so no `WM_NCRBUTTONUP` is ever generated. The frontend's
/// `contextmenu` handler on the title strip calls this instead.
///
/// `GetSystemMenu(hwnd, false)` — `false` matters: `true` means *revert*, which
/// destroys the window's copy and hands back null.
pub fn show_system_menu(window: &tauri::Window) {
    let Ok(hwnd) = window.hwnd() else { return };
    // SAFETY: `hwnd` is this window's live handle; see the per-call notes.
    unsafe { show_system_menu_at(hwnd) }
}

unsafe fn show_system_menu_at(hwnd: HWND) {
    // SAFETY: plain query on a live handle. `false` = do not revert.
    let menu = unsafe { GetSystemMenu(hwnd, false) };
    if menu.is_invalid() {
        return;
    }

    // Windows keeps these in sync for a window with a real frame. Ours has
    // none, so an unmanaged menu would offer Maximize while already maximized.
    // SAFETY: plain query on a live handle.
    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
    let state = |enabled: bool| -> MENU_ITEM_FLAGS {
        MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED }
    };
    // SAFETY: `menu` is the window's live system menu, checked above.
    unsafe {
        let _ = EnableMenuItem(menu, SC_RESTORE, state(zoomed));
        let _ = EnableMenuItem(menu, SC_MOVE, state(!zoomed));
        let _ = EnableMenuItem(menu, SC_SIZE, state(!zoomed));
        let _ = EnableMenuItem(menu, SC_MINIMIZE, state(true));
        let _ = EnableMenuItem(menu, SC_MAXIMIZE, state(!zoomed));
        let _ = EnableMenuItem(menu, SC_CLOSE, state(true));
    }

    let mut pt = POINT::default();
    // SAFETY: `pt` is initialised; the call fills it with screen coordinates.
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return;
    }

    // Required by `TrackPopupMenu`'s contract: without it the menu does not
    // dismiss when the user clicks away. muda and tray-icon both do this.
    // SAFETY: plain command on a live handle.
    let _ = unsafe { SetForegroundWindow(hwnd) };

    // SAFETY: live menu and window handle; `TPM_RETURNCMD` makes this return
    // the chosen command instead of posting `WM_COMMAND`.
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
    };
    // With `TPM_RETURNCMD` the return value is the command id, not a success
    // flag; 0 means the user dismissed without choosing.
    if chosen.0 == 0 {
        return;
    }
    // Post rather than send: `SC_MOVE` and `SC_SIZE` start their own modal
    // loops, and they must not begin until `TrackPopupMenu` has unwound.
    // SAFETY: plain message post to a live handle.
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_SYSCOMMAND,
            WPARAM(chosen.0 as usize),
            LPARAM(0),
        )
    };
}

/// Push one piece of caption state into the page.
///
/// The call is written so a page that has not installed the hook (or has been
/// navigated away) evaluates to `undefined` instead of throwing.
fn push(kind: &str, on: bool) {
    let Some(app) = APP.get() else { return };
    let Some(window) = app.get_webview_window(crate::MAIN_WINDOW) else {
        return;
    };
    let _ = window.eval(&format!(
        "window.__audiohubCaption&&window.__audiohubCaption('{kind}',{})",
        if on { "true" } else { "false" }
    ));
}

fn scale(hwnd: HWND) -> f64 {
    // SAFETY: plain query on a live handle.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / 96.0
    }
}

/// Is this client-area point inside the maximize button?
fn in_maximize_button(hwnd: HWND, pt: POINT) -> bool {
    let mut client = RECT::default();
    // SAFETY: plain query on a live handle; `client` is fully initialised by it.
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        return false;
    }
    let s = scale(hwnd);
    let w = CAPTION_BUTTON_W * s;
    let h = CAPTION_BUTTON_H * s;
    let right = f64::from(client.right) - MAXIMIZE_SLOT_FROM_RIGHT * w;
    let left = right - w;
    let x = f64::from(pt.x);
    let y = f64::from(pt.y);
    x >= left && x < right && y >= 0.0 && y < h
}

fn lo_word(v: isize) -> i32 {
    i32::from((v & 0xffff) as u16 as i16)
}

fn hi_word(v: isize) -> i32 {
    i32::from(((v >> 16) & 0xffff) as u16 as i16)
}

fn set_hovered(hwnd: HWND, on: bool) {
    if HOVERED.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if on {
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE | TME_NONCLIENT,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: `track` is fully initialised and outlives the call.
        unsafe {
            let _ = TrackMouseEvent(&mut track);
        }
    } else {
        PRESSED.store(false, Ordering::Relaxed);
        push("press", false);
    }
    push("hover", on);
}

fn report_zoomed(hwnd: HWND) {
    // SAFETY: plain query on a live handle.
    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
    let next = isize::from(zoomed);
    if LAST_ZOOMED.swap(next, Ordering::Relaxed) != next {
        push("max", zoomed);
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // Ask tao first and only override `HTCLIENT`. The button sits in the
            // top-right corner, where the outer few pixels legitimately answer
            // HTTOP / HTTOPRIGHT — stealing those would cost the resize grip,
            // which is a worse trade than a slightly smaller snap target.
            let hit = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            if hit.0 != HTCLIENT as isize {
                return hit;
            }
            let mut pt = POINT {
                x: lo_word(lparam.0),
                y: hi_word(lparam.0),
            };
            // SAFETY: `pt` is initialised; the call rewrites it in place.
            if unsafe { ScreenToClient(hwnd, &mut pt) }.as_bool() && in_maximize_button(hwnd, pt) {
                return LRESULT(HTMAXBUTTON as isize);
            }
            hit
        }

        WM_NCMOUSEMOVE => {
            set_hovered(hwnd, wparam.0 == HTMAXBUTTON as usize);
            // Must reach the default window procedure, including — especially —
            // while the pointer is over the maximize button. The snap-layout
            // flyout is on a timer that `DefWindowProc` starts from this
            // message; an earlier revision returned 0 here to keep the default
            // handling out of the way, and the cost was the entire feature this
            // module exists for. Hover state is ours to push, but the message
            // is not ours to eat.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }

        WM_NCMOUSELEAVE => {
            set_hovered(hwnd, false);
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }

        WM_NCLBUTTONDOWN if wparam.0 == HTMAXBUTTON as usize => {
            // Swallowed on purpose: the default handling of a non-client press
            // starts a window drag / system menu, and the button would never
            // see a matching button-up.
            PRESSED.store(true, Ordering::Relaxed);
            push("press", true);
            LRESULT(0)
        }

        WM_NCLBUTTONUP if wparam.0 == HTMAXBUTTON as usize => {
            // Deliberately **not** gated on having seen the matching
            // `WM_NCLBUTTONDOWN`: Windows only routes a non-client button-up to
            // the window whose hit test claimed that point, so "released here"
            // is "clicked here", and a gate could only ever turn a real click
            // into nothing. The frontend keeps an `onClick` as well — over an
            // HTMAXBUTTON rectangle the webview never sees the click, so
            // exactly one of the two paths can fire, and the button cannot end
            // up dead if the message routing surprises us.
            PRESSED.store(false, Ordering::Relaxed);
            push("press", false);
            // SAFETY: plain state query / command on a live handle.
            let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
            let _ = unsafe { ShowWindow(hwnd, if zoomed { SW_RESTORE } else { SW_MAXIMIZE }) };
            LRESULT(0)
        }

        WM_SIZE => {
            let r = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            report_zoomed(hwnd);
            r
        }

        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}
