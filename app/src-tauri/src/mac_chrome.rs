//! Where macOS draws the traffic lights, and keeping them there.
//!
//! # The rule AppKit actually follows
//!
//! Measured on macOS 26.5.2 (`test/mac-corner-probe/probe3.m`): sweep the
//! `NSTitlebarContainerView`'s height from 24pt to 80pt, writing no button
//! origin of our own, and read the close button back at each step.
//!
//! * Its `origin.y` **inside the container** is 9.0pt at every height tested.
//!   AppKit does not centre the buttons in the container — it hangs them from
//!   the container's bottom edge at a fixed offset.
//! * So the centre, measured down from the window's top edge, is exactly
//!   `container_height - 16`, with no residual across all 16 samples. The 16
//!   decomposes as `9 + BUTTON_DIAMETER / 2`.
//!
//! Two consequences worth stating, because an earlier revision of this comment
//! got both wrong.
//!
//! **tao's `trafficLightPosition.y` does have a defined meaning.**
//! `inset_traffic_lights` (`tao-0.35.3/src/platform_impl/macos/view.rs:1152`)
//! sets the container height to `button_height + y` and then writes only each
//! button's `origin.x` — the loop at `:1177` never touches `origin.y`. Feeding
//! the measured rule through that gives `centre = (14 + y) - 16 = y - 2`, so
//! the config field would land the centre on 33.0 at `y = 35`.
//!
//! **Our explicit `origin.y` is load-bearing, not belt-and-braces.** This
//! module sets the container to `button_height + 2 * TOP_INSET` (66pt) *and*
//! each `origin.y` to `TOP_INSET` (26pt). Only the second write produces the
//! result: left to itself AppKit would put the centre at `66 - 16 = 50`, 17pt
//! below target. Setting the container to **49pt and writing no `origin.y` at
//! all** reaches the same centre of 33.0 through AppKit's own arithmetic, and
//! is the more robust shape of this fix — it cannot be undone by a layout pass,
//! because it is what a layout pass computes. It is written up rather than
//! adopted here so the change is a deliberate one rather than a drive-by.
//!
//! # Why this is not `tauri.macos.conf.json`'s `trafficLightPosition`
//!
//! Meaning aside, the config field is applied once, from `drawRect:`. tao's
//! only call site is `view.rs:348`, inside the host view's `drawRect:` — a view
//! the WKWebView covers completely, so it may never be asked to draw again.
//! Leaving fullscreen rebuilds the title bar and puts the buttons back at the
//! system position; the upstream fix for that (tao#1254) first shipped in tao
//! 0.36.0, and `tauri-runtime-wry-2.11.4` pins `tao = "0.35.0"`, so we cannot
//! have it without patching the dependency tree. Re-applying from our own
//! window-event handler costs a dozen lines and moves nothing else.
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
//! # The corner radius is a separate knob, and it is inert here
//!
//! Worth recording because it is the obvious guess — bigger radius, buttons
//! pushed clear of the curve — and because on macOS 26 the radius really is
//! reachable, so the guess cannot be dismissed on "you can't change it".
//!
//! *Reachable, but only privately.* The 26.5 SDK has no `cornerRadius` on
//! `NSWindow`; the only public ones are `NSGlassEffectView`, `NSBox` and
//! `CALayer`. `-[NSWindow _setCornerRadius:]` exists and works: driven over
//! 8/16/26/40/60/100pt it tracks 1:1 in a window-server capture
//! (`test/mac-corner-probe/probe2.m`). `CGSSetWindowCornerRadius` — the usual
//! third-party route — is simply absent from the dylib on this release.
//!
//! *Inert with respect to the buttons.* Across that whole range the close
//! button stayed at `x = 9.00, centre_y = 16.00`, unchanged to the hundredth:
//! set before the window was first ordered in (so the first layout pass saw
//! it), set at runtime, and set with `tileAndSetWindowShape:`,
//! `_updateCornerInsets` and `windowCornerMaskChanged` forced afterwards.
//! `_setEffectiveCornerRadius:` likewise. The same harness's positive control —
//! the container height above — moved the centre from 16.00 to 50.00, so the
//! measurement can see movement when there is movement to see.
//!
//! So: two things in the same corner, one cause each, and this one is not it.
//! The radius is 16pt by default (`_cornerRadius` reports it, and a circular
//! fit off a screenshot agrees at 0.44 device-px RMS) — a number to design
//! against, not a lever to pull.

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
