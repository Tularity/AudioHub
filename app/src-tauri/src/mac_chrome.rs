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
//! The same holds on the other axis: the button's `origin.x` inside the
//! container is 9.0pt too, and it stays there when the *container* is moved
//! (`probe4.m`), so insetting the container's left edge carries the whole
//! group with it.
//!
//! **So this module writes no button coordinate at all.** It sets the
//! container's frame — height for the vertical placement, `origin.x` for the
//! horizontal — and lets AppKit put the buttons where its own arithmetic says.
//! That is the robust shape of the fix: a layout pass cannot undo it, because
//! the result *is* what a layout pass computes. An earlier revision set the
//! container to 66pt and then forced each `origin.y` to 26pt; that second write
//! was load-bearing (left alone, AppKit puts the centre at `66 - 16 = 50`, 17pt
//! low), which is exactly the fragility being removed here.
//!
//! For the record, since an earlier revision of this comment got it wrong:
//! **tao's `trafficLightPosition.y` does have a defined meaning.**
//! `inset_traffic_lights` (`tao-0.35.3/src/platform_impl/macos/view.rs:1152`)
//! sets the container height to `button_height + y` and then writes only each
//! button's `origin.x` — the loop at `:1177` never touches `origin.y`. Feeding
//! the measured rule through that gives `centre = (14 + y) - 16 = y - 2`, so
//! the config field would land the centre on 33.0 at `y = 35`.
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
//! That ~17pt disagreement is the thing being fixed. `NAV_CENTRE_Y` is the
//! target; `CONTAINER_HEIGHT` is what has to be set to reach it, written as the
//! arithmetic of the measured rule rather than as `49` so the intent survives a
//! change to any of its terms. (A first pass used 32.5, the estimate in
//! `docs/design-ui-chrome.md` §1.2; measuring the built app put both elements at
//! 33.0 instead, so the guess was half a point low.)
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

/// The app's own top-strip centre line, in points down from the window's top
/// edge — what the traffic lights have to share. See the table above.
const NAV_CENTRE_Y: f64 = 33.0;

/// Measured traffic-light diameter.
const BUTTON_DIAMETER: f64 = 14.0;

/// AppKit's own inset for the buttons inside `NSTitlebarContainerView`, the
/// same on both axes: `origin` is `(9.0, 9.0)` and stays there at every
/// container height sampled from 24pt to 80pt (`probe3.m`, 16 samples) and at
/// every container `origin.x` tried (`probe4.m`). This is the constant the
/// whole module leans on — nothing here writes a button coordinate, so every
/// placement is AppKit's own arithmetic applied to a container we sized.
const APPKIT_BUTTON_INSET: f64 = 9.0;

/// Container height that lands the buttons on `NAV_CENTRE_Y`.
///
/// AppKit hangs the buttons from the container's *bottom* edge, so measured
/// down from the window's top the centre is `height - (APPKIT_BUTTON_INSET +
/// BUTTON_DIAMETER / 2)` — i.e. `height - 16`, exactly, with no residual across
/// all 16 samples. Inverting that for a 33.0 centre gives 49.0.
const CONTAINER_HEIGHT: f64 = NAV_CENTRE_Y + APPKIT_BUTTON_INSET + BUTTON_DIAMETER / 2.0;

/// Distance from the window's left edge to the close button's left edge.
///
/// The stock value is 9pt, which after the vertical correction reads as
/// noticeably tighter to the window edge than any content: `#view-root` pads to
/// 28pt. 20pt puts the button group past the corner radius' curved segment and
/// inside the same breathing room the content lives in, without crowding the
/// centred nav pill (the group then ends at 79.5pt, and `--chrome-lead` in
/// `styles.css` reserves 100pt).
const LEADING_INSET: f64 = 20.0;

/// Container `origin.x` that lands the close button on `LEADING_INSET`.
///
/// The buttons keep their local `origin.x` of `APPKIT_BUTTON_INSET` when the
/// container moves, so the container has to start that much further left than
/// the edge we actually want. Idempotent for the same reason: re-running never
/// changes the local origin, so this never accumulates.
const CONTAINER_X: f64 = LEADING_INSET - APPKIT_BUTTON_INSET;

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
    let Some(close) = ns_window.standardWindowButton(NSWindowButton::CloseButton) else {
        return;
    };

    // Button -> NSTitlebarView -> NSTitlebarContainerView. Two hops, same as
    // tao; the container is the one whose frame decides where AppKit puts the
    // buttons. Nothing below touches a button — that is the whole point.
    let Some(titlebar) = close.superview() else { return };
    let Some(container) = titlebar.superview() else { return };

    let window_size = ns_window.frame().size;
    let mut rect = container.frame();
    rect.origin.x = CONTAINER_X;
    // Keep the right edge on the window's, so the strip does not overhang.
    rect.size.width = window_size.width - CONTAINER_X;
    rect.size.height = CONTAINER_HEIGHT;
    // Views are bottom-left origin: pinning the container's top to the window's
    // top means its origin sits a full height down from there.
    rect.origin.y = window_size.height - CONTAINER_HEIGHT;
    container.setFrame(rect);
}
