//! Where macOS 26 draws the traffic lights, and letting it.
//!
//! # The rule AppKit actually follows on macOS 26
//!
//! On Tahoe the window's corner radius and the traffic lights are not two
//! things — they are one. Measured on 26.5.2
//! (`test/mac-corner-probe/probe5.m`, `probe7.m`), writing no coordinate of our
//! own and reading both back:
//!
//! | window style | radius | close centre y | close left x |
//! |---|---|---|---|
//! | titlebar only (no toolbar) | 16 | **16.00** | 9.00 |
//! | toolbar, `unifiedCompact`   | 20 | **20.00** | 12.00 |
//! | toolbar, `unified`          | 26 | **26.00** | 19.00 |
//!
//! The button's centre lands on the corner radius exactly, at every step. That
//! is the literal meaning of the design Apple describes in the WWDC 2025
//! session *Build an AppKit app with the new design*:
//!
//! > In the new design system, windows now have a softer, more generous corner
//! > radius, **which varies based on the style of window**. Windows with
//! > toolbars now use a larger radius, which is designed to wrap concentrically
//! > around the glass toolbar elements, scaling to match the size of the
//! > toolbar. Titlebar-only windows retain a smaller corner radius, **wrapping
//! > compactly around the window controls**.
//!
//! So the causality runs style → radius → button placement. The radius is not
//! an input you set; it is what AppKit derives from the kind of window you
//! declared, and the buttons are then placed concentrically inside it.
//!
//! # Which is why this module sets a window *style*, and no geometry
//!
//! Everything below is two property writes: attach an `NSToolbar`, and ask for
//! the `unified` style. No frame, no origin, no height — not for a button, and
//! not for the container that holds them. The placement that results is the one
//! AppKit computes for a standard document window, the same as Finder, Safari,
//! Mail and Notes.
//!
//! That is also why there is no re-apply hook any more. The previous revision
//! resized `NSTitlebarContainerView` and had to redo it on every `Resized` and
//! `Focused`, because a layout pass would recompute what we had overwritten. A
//! toolbar is a persistent property of the window, not a geometry write that
//! layout can undo, so applying it once at startup is the whole of it.
//! `probe7.m` confirms the placement is untouched across resizes from 860 to
//! 1160 wide with nothing re-applied.
//!
//! # There is no public "set the corner radius" on 26.5 — and none is needed
//!
//! Worth recording, because setting the radius directly is the obvious first
//! guess. Checked against the installed 26.5 SDK headers: the only public
//! `cornerRadius` in AppKit is on `NSGlassEffectView` and `NSBox`. `NSWindow.h`
//! declares no corner or shape property at all. The `NSViewCornerConfiguration`
//! family that the online documentation shows is marked macOS 27.0+ Beta and is
//! absent from this SDK.
//!
//! The private `-[NSWindow _setCornerRadius:]` does exist and does change the
//! rendered radius (`probe2.m` drove it over 8..100pt), but it is inert with
//! respect to the buttons: across that whole range the close button stayed at
//! `x = 9.00, centre_y = 16.00`, unchanged to the hundredth. Which fits the
//! rule above rather than contradicting it — `_setCornerRadius:` repaints the
//! corner, it does not re-declare the window's style, so nothing re-runs the
//! placement. The supported lever is the style, and the style is public.
//!
//! # What the app's own top strip has to match
//!
//! The native line is **26.0pt** down from the window top, with the button
//! group spanning x 19.0 .. 79.0 (`probe7.m`). `styles.css` centres the nav
//! pill and the daemon badge on that line and reserves `--chrome-lead: 100px`
//! for the group. The strip is `--chrome-h: 52px`, which is the height AppKit
//! reserves for a unified toolbar (`contentLayoutRect`, `probe8.m`) — so both
//! our elements and the traffic lights are centred in the same native band.
//!
//! # Cost of adopting the toolbar: none measured
//!
//! `probe8.m` rendered the frame view before and after attaching the toolbar
//! and compared a column of pixels down the top 70pt: identical. With
//! `titlebarAppearsTransparent` (which tao's `Overlay` title-bar style already
//! sets) the toolbar paints nothing. Hit-testing at the window's centre-x
//! reaches the content view at every depth from 6pt to 80pt both before and
//! after, so it steals no clicks; at the far left it now correctly reports
//! `_NSThemeCloseWidget` at y=26, which is the button actually being there.

#![cfg(target_os = "macos")]

use objc2::MainThreadMarker;
use objc2_app_kit::{NSToolbar, NSWindow, NSWindowToolbarStyle};

/// Give the window the standard macOS 26 document-window style, so AppKit
/// places the traffic lights — and rounds the corners — the way it does for
/// every system app.
///
/// Idempotent: if the window already has a toolbar this returns without
/// touching anything, so calling it twice cannot stack up state.
///
/// # Thread
/// Must run on the main thread. The only caller is Tauri's `setup`, which the
/// event loop dispatches on main; `MainThreadMarker::new()` returns `None`
/// anywhere else and we bail rather than message AppKit off-main.
pub fn apply(window: &tauri::Window) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Ok(ptr) = window.ns_window() else { return };
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ns_window()` hands back the `NSWindow` backing this window,
    // alive for as long as the window is; we only borrow it for this call, and
    // the main-thread requirement of both messages below is checked above.
    unsafe {
        let ns_window: &NSWindow = &*(ptr as *const NSWindow);
        if ns_window.toolbar().is_some() {
            return;
        }
        // An item-less toolbar is enough: the style is what AppKit reads to
        // pick the radius, not the contents. Verified with a nil delegate and
        // zero items in `probe9.m` — radius 26, close button at (19.0, 26.0).
        let toolbar = NSToolbar::new(mtm);
        ns_window.setToolbar(Some(&toolbar));
        ns_window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    }
}
