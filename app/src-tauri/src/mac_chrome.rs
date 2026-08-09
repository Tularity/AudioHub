//! Where macOS draws the traffic lights, and keeping them there.
//!
//! # Why this is not `tauri.macos.conf.json`'s `trafficLightPosition`
//!
//! That config field exists and does reach tao, but two properties of the
//! version we are locked to make it the wrong lever here.
//!
//! **Its `y` has no defined meaning.** tao's `inset_traffic_lights`
//! (`tao-0.35.3/src/platform_impl/macos/view.rs:1152`) does *not* set the
//! buttons' `origin.y`. It only stretches the title bar container view to
//! `button_height + y` and lets AppKit re-lay-out inside it. So the resulting
//! top inset is `y - b`, where `b` is whatever local `origin.y` AppKit leaves
//! the buttons at — and nothing in the source pins `b`. Whether `y` behaves as
//! "top inset" or as "twice the top inset" therefore depends on an AppKit
//! layout pass we do not control.
//!
//! This module sidesteps the question instead of measuring it. It sets the
//! container height to `button_height + 2 * TOP_INSET` **and** each button's
//! `origin.y` to `TOP_INSET`. Under both hypotheses the answer is the same:
//!
//! * AppKit leaves our origins alone  -> inset = H - b - h = TOP_INSET
//! * AppKit re-centres in the container -> b = (H - h)/2 = TOP_INSET, same
//!
//! **It is applied once, from `drawRect:`.** tao's only call site is
//! `view.rs:348`, inside the host view's `drawRect:` — a view the WKWebView
//! covers completely, so it may never be asked to draw again. Leaving fullscreen
//! rebuilds the title bar and puts the buttons back at the system position; the
//! upstream fix for that (tao#1254) first shipped in tao 0.36.0, and
//! `tauri-runtime-wry-2.11.4` pins `tao = "0.35.0"`, so we cannot have it
//! without patching the dependency tree. Re-applying from our own window-event
//! handler costs a dozen lines and moves nothing else.
//!
//! # The geometry, and where the numbers come from
//!
//! Measured off a real screenshot of the running app (2000x1400 device px for a
//! 1000x700pt window, so DPR is exactly 2; cross-checked against `#nav`'s
//! `top: 11px` landing exactly 22 device rows below the window's top edge).
//! The app's own top-strip elements share one centre line:
//!
//! | element | top..bottom (pt) | centre |
//! |---|---|---|
//! | nav pill `#nav`      | 11.0 .. 55.0 | **33.0** |
//! | daemon badge         | 18.0 .. 48.0 | **33.0** |
//! | traffic lights, stock | 9.0 .. 23.0 | 16.0 |
//!
//! That ~17pt disagreement is the thing being fixed. `TOP_INSET` is therefore
//! `33 - 14/2`, where 14pt is the measured button diameter — written as the
//! arithmetic rather than as `26` so the intent survives a change to either
//! number. (A first pass used 32.5, the estimate in `docs/design-ui-chrome.md`
//! §1.2; measuring the built app put both elements at 33.0 instead, so the
//! guess was half a point low. Verified afterwards: button top lands at exactly
//! `TOP_INSET` and the close button's left edge at exactly `LEADING_INSET`, so
//! the two constants mean what they say.)
//!
//! Note what is *not* here: the window's corner radius. It was measured at 16pt
//! (least-squares circular fit, 0.44 device-px RMS) but AppKit draws it in
//! `NSThemeFrame` and exposes no knob, it is not a published constant, and it
//! has no bearing on button placement — AppKit centres the buttons in the
//! container view, and the radius does not enter that. Two things in the same
//! corner, one cause each.

#![cfg(target_os = "macos")]

use objc2::MainThreadMarker;
use objc2_app_kit::{NSWindow, NSWindowButton};

/// Distance from the window's top edge to the top of the buttons, in points.
///
/// `NAV_CENTRE_Y` is the app's own top-strip centre line; `BUTTON_DIAMETER` is
/// the measured traffic-light size. Keeping the subtraction visible is the
/// point — the target is "share the nav pill's centre line", not "26".
const NAV_CENTRE_Y: f64 = 33.0;
const BUTTON_DIAMETER: f64 = 14.0;
const TOP_INSET: f64 = NAV_CENTRE_Y - BUTTON_DIAMETER / 2.0;

/// Distance from the window's left edge to the close button's left edge.
///
/// The stock value is 9pt, which after the vertical correction reads as
/// noticeably tighter to the window edge than any content: `#view-root` pads to
/// 28pt. 20pt puts the button group past the corner radius' curved segment and
/// inside the same breathing room the content lives in, without crowding the
/// centred nav pill (the group then ends at 79.5pt, and `--chrome-lead` in
/// `styles.css` reserves 100pt).
const LEADING_INSET: f64 = 20.0;

/// Move the traffic lights onto the app's top-strip centre line.
///
/// Silently does nothing when the window has no standard buttons (which is the
/// state during a fullscreen transition, among others) — this runs on every
/// resize, so a failure must be a no-op rather than a log flood.
///
/// # Thread
/// Must run on the main thread. Every caller is either Tauri's `setup` or a
/// window-event callback, both of which the event loop dispatches on main;
/// `MainThreadMarker::new()` returns `None` anywhere else and we bail.
pub fn apply(window: &tauri::Window) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    // Fullscreen hands the buttons to the menu-bar overlay; the container we
    // would resize is not the one on screen. The `Resized` that follows the
    // transition back is what puts them right.
    if window.is_fullscreen().unwrap_or(false) {
        return;
    }
    let Ok(ptr) = window.ns_window() else { return };
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ns_window()` hands back the `NSWindow` this window is backed by,
    // alive for as long as the window is; we only borrow it for this call, and
    // the main-thread requirement of every message below is checked above.
    unsafe {
        let ns_window: &NSWindow = &*(ptr as *const NSWindow);
        apply_to(ns_window);
    }
}

unsafe fn apply_to(ns_window: &NSWindow) {
    let (Some(close), Some(mini), Some(zoom)) = (
        ns_window.standardWindowButton(NSWindowButton::CloseButton),
        ns_window.standardWindowButton(NSWindowButton::MiniaturizeButton),
        ns_window.standardWindowButton(NSWindowButton::ZoomButton),
    ) else {
        return;
    };

    // Button -> NSTitlebarView -> NSTitlebarContainerView. Two hops, same as
    // tao; the container is the one whose height AppKit centres within.
    let Some(titlebar) = close.superview() else { return };
    let Some(container) = titlebar.superview() else { return };

    let close_rect = close.frame();
    let button_h = close_rect.size.height;

    // Read the spacing before moving anything, and derive it rather than
    // hard-coding 23pt: this runs again on every resize, and by then close is
    // already at LEADING_INSET, so the difference is still the true gap.
    let spacing = mini.frame().origin.x - close_rect.origin.x;

    let container_h = button_h + 2.0 * TOP_INSET;
    let mut rect = container.frame();
    rect.size.height = container_h;
    // Views are bottom-left origin: pinning the container's top to the window's
    // top means its origin sits a full height down from there.
    rect.origin.y = ns_window.frame().size.height - container_h;
    container.setFrame(rect);

    for (i, button) in [&close, &mini, &zoom].into_iter().enumerate() {
        let mut origin = button.frame().origin;
        origin.x = LEADING_INSET + i as f64 * spacing;
        // Explicit, so the result does not depend on whether AppKit re-centres
        // after the frame change. See the module docs.
        origin.y = TOP_INSET;
        button.setFrameOrigin(origin);
    }
}
