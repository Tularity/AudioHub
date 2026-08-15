//! The macOS status item, built on `tray-icon` + `muda` instead of Tauri's
//! wrappers — because a menu bar volume slider needs the real `NSMenu`.
//!
//! # Why this module exists at all
//!
//! Putting an `NSSlider` in the tray menu means putting a *view-based*
//! `NSMenuItem` into the `NSMenu` AppKit pops. Tauri 2.11 hands out no route to
//! that object:
//!
//! * `tauri::menu::Menu::inner()` — the muda handle — is `pub(crate)`.
//! * `tauri::menu::ContextMenu` is sealed by `sealed::ContextMenuBase`
//!   (`tauri-2.11.5/src/menu/mod.rs:722,745-760`), so `inner_context()` is
//!   unreachable from here.
//! * `tauri::tray::TrayIcon` exposes no platform handle at all
//!   (`tauri-2.11.5/src/tray/mod.rs`).
//!
//! The crates *underneath* Tauri do expose it:
//! `tray_icon::TrayIcon::ns_status_item()` (`tray-icon-0.24.2/src/lib.rs:531`)
//! and `muda::ContextMenu::ns_menu()` (`muda-0.19.3/src/lib.rs:455`), both on
//! objc2-app-kit 0.3 — the same version this crate already links. Tauri itself
//! depends on both at these exact versions, so declaring them directly adds
//! compile units, not dependencies.
//!
//! So macOS builds its status item here and Windows keeps
//! `tauri::tray::TrayIconBuilder` untouched in `main.rs`. The two never share a
//! code path; `set_tray_status` is the single seam and it is `#[cfg]`-split.
//!
//! # How menu clicks still reach the app
//!
//! Deliberately *not* via `muda::MenuEvent::set_event_handler`: Tauri installs
//! its own global handler when the app is built (`tauri-2.11.5/src/app.rs:2350`)
//! and replacing it would silently kill the application menu's Settings item.
//!
//! It turns out nothing needs replacing. Tauri forwards every muda event to its
//! global menu listeners with **no id-registry lookup** at all
//! (`tauri-2.11.5/src/app.rs:2588-2600`: it iterates `global_event_listeners`
//! unconditionally). Items created here are therefore delivered to
//! `Builder::on_menu_event` in `main.rs` just like Tauri's own, on the main
//! thread, and that is where `SHOW_ID` / `QUIT_UI_ID` / `QUIT_ALL_ID` are
//! handled.
//!
//! # The volume row is hidden, not removed
//!
//! `set_tray_status` is called on every frontend state change, so the row's
//! visibility flips often. Rebuilding the menu each time would yank a slider out
//! from under a dragging finger, and splicing items in and out of the `NSMenu`
//! behind muda's back would desynchronise muda's own item indices from AppKit's.
//!
//! Both problems disappear if the structure never changes: the separator and the
//! slider row are inserted **once**, at build time, and toggled with
//! `-[NSMenuItem setHidden:]`. A hidden item is not laid out and not rendered —
//! from the user's side the row is gone, which is the whole of the contract —
//! while muda's model keeps the six items it built, at the indices it built them
//! at, forever.

#![cfg(target_os = "macos")]

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
// `MainThreadOnly` is the trait that gives `NSView`/`NSSlider`/`NSTextField`
// their `alloc(mtm)`; `AnyThread` gives the plain `alloc()` used for the
// target object, which is not a view.
use objc2::{define_class, msg_send, sel, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSFont, NSMenu, NSMenuItem, NSSlider, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use tauri::{AppHandle, Manager};
use tray_icon::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::icon::{self, IconState, IconTheme};
use crate::{NativeLocale, MAIN_WINDOW, TRAY_ID};

/// Menu ids. Identical strings to the ones the Tauri tray used, so the handler
/// arms in `main.rs` read the same on both platforms.
pub const SHOW_ID: &str = "show";
pub const QUIT_UI_ID: &str = "quit_ui";
pub const QUIT_ALL_ID: &str = "quit_all";

/// DOM event the dragged slider value is delivered on. Same shape as
/// `SETTINGS_MENU_EVENT`: a fixed event dispatched straight into our own
/// webview, so the frontend needs no Tauri package to listen for it.
const TRAY_VOLUME_EVENT: &str = "audiohub://tray-volume";

/// At most one value per 100 ms while dragging, with the tail always delivered.
/// Matches `THROTTLE_MS` in `app/frontend/src/components/VolumeControl.tsx`, so
/// the two volume surfaces put the same load on `session.set_volume`.
const THROTTLE: Duration = Duration::from_millis(100);

/// How long a local drag suppresses the value echoed back by the frontend, and
/// how close the echo must be to count as agreement. Same reasoning — and the
/// same constants — as `HOLD_MS` / `EPS` in `VolumeControl.tsx`: the daemon
/// reports the provider's real device roughly once a second, so a naive
/// write-back lags the finger and makes the knob jump backwards mid-drag.
const ECHO_HOLD: Duration = Duration::from_millis(2000);
const ECHO_EPS: f64 = 0.02;

/// Geometry of the custom row, in points. The width only has to be plausible —
/// AppKit sizes the menu to its widest item, and a view-based item does not
/// stretch — so this is picked to sit comfortably beside the localised quit
/// strings rather than measured from anything.
const ROW_WIDTH: f64 = 240.0;
const ROW_HEIGHT: f64 = 44.0;
/// Left inset lines the label up with the text of the ordinary items above it;
/// AppKit indents those for the (absent) checkmark column and a view-based item
/// gets no such treatment, so it is reproduced by hand.
const ROW_INSET_LEADING: f64 = 21.0;
const ROW_INSET_TRAILING: f64 = 12.0;

// ---- main-thread ownership -------------------------------------------------

/// Send + Sync by *not letting anything happen off the main thread*.
///
/// objc2 0.6 moved its own `MainThreadBound` out to the `dispatch2` crate; this
/// is the same fifteen lines rather than a new dependency. The payload is only
/// reachable through `get`, which demands a `MainThreadMarker`, so no reference
/// to it can exist anywhere else.
///
/// The other half of the invariant is that it is only ever stored in a `static`,
/// and Rust never drops statics — so `T`'s destructor never runs, on any thread.
/// That is not a leak we tolerate, it is the behaviour we want: releasing an
/// `NSMenu`/`NSSlider` off-main is not allowed, and dropping the `TrayIcon`
/// would pull the status item straight out of the menu bar.
struct MainThreadBound<T>(T);

// SAFETY: see the type's documentation — `get` gates every access behind a
// `MainThreadMarker`, and the value is never dropped.
unsafe impl<T> Send for MainThreadBound<T> {}
unsafe impl<T> Sync for MainThreadBound<T> {}

impl<T> MainThreadBound<T> {
    fn new(value: T, _mtm: MainThreadMarker) -> Self {
        Self(value)
    }

    fn get(&self, _mtm: MainThreadMarker) -> &T {
        &self.0
    }
}

/// The one status item this process owns. A `static` rather than
/// `app.manage(..)` for the drop reason above; there is exactly one tray per
/// process either way.
static TRAY: OnceLock<MainThreadBound<MacTray>> = OnceLock::new();

struct MacTray {
    show: MenuItem,
    status: MenuItem,
    quit_ui: MenuItem,
    quit_all: MenuItem,
    volume: VolumeRow,
    /// Held so the muda menu outlives the `NSMenu` borrowed from it, and so the
    /// status item is never left pointing at a freed menu.
    _menu: Menu,
    /// Held because dropping it removes the icon from the menu bar.
    tray: TrayIcon,
}

struct VolumeRow {
    /// The separator above the row. Hidden and shown together with `item`, so
    /// hiding the slider does not leave two separators stacked up.
    separator: Retained<NSMenuItem>,
    item: Retained<NSMenuItem>,
    label: Retained<NSTextField>,
    slider: Retained<NSSlider>,
    /// AppKit holds its `target` **unretained**. Nothing else references this
    /// object, so without this field it would be released the moment the row is
    /// built and the first drag would message freed memory.
    _target: Retained<VolumeTarget>,
}

// ---- the slider's target ---------------------------------------------------

define_class!(
    // SAFETY:
    // - NSObject imposes no subclassing requirements.
    // - `VolumeTarget` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "AudioHubTrayVolumeTarget"]
    struct VolumeTarget;

    impl VolumeTarget {
        /// Continuous action: AppKit sends this for every step of the drag, on
        /// the main thread. Throttling and delivery are handled downstream so
        /// this stays a plain read of the control's current value.
        #[unsafe(method(audiohubVolumeChanged:))]
        fn volume_changed(&self, sender: &NSSlider) {
            report_slider_value(sender.doubleValue());
        }
    }
);

impl VolumeTarget {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ---- frontend delivery -----------------------------------------------------

/// Everything the slider's action needs, published once the row exists.
struct VolumeBridge {
    app: AppHandle,
    throttle: Mutex<Throttle>,
}

#[derive(Default)]
struct Throttle {
    /// When the last value actually went to the frontend.
    last_emit: Option<Instant>,
    /// Value withheld by the rate limit, waiting for the trailing edge.
    pending: Option<f64>,
    /// A trailing-edge timer is already in flight; only ever one at a time.
    timer_armed: bool,
    /// Last value the user set here, and when — the echo suppressor's input.
    local_value: Option<f64>,
    local_at: Option<Instant>,
}

static BRIDGE: OnceLock<VolumeBridge> = OnceLock::new();

fn report_slider_value(value: f64) {
    let Some(bridge) = BRIDGE.get() else { return };
    let now = Instant::now();
    let mut throttle = bridge
        .throttle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Remember the local intent before anything else: the echo suppressor has to
    // hold even for the frames the rate limit drops, or the frontend's report of
    // the *previous* value would still land on the knob.
    throttle.local_value = Some(value);
    throttle.local_at = Some(now);

    let due = throttle.last_emit.map_or(now, |at| at + THROTTLE);
    if now >= due {
        throttle.last_emit = Some(now);
        throttle.pending = None;
        drop(throttle);
        dispatch_volume(&bridge.app, value);
        return;
    }

    throttle.pending = Some(value);
    if throttle.timer_armed {
        return;
    }
    throttle.timer_armed = true;
    let wait = due.saturating_duration_since(now);
    drop(throttle);

    // A short-lived thread per throttle window (one alive at a time, ~10 per
    // second of dragging) rather than a timer source: the same shape as the
    // other small helper threads in this crate, and it needs no runtime.
    if std::thread::Builder::new()
        .name("audiohub-tray-volume".to_string())
        .spawn(move || {
            std::thread::sleep(wait);
            flush_pending_volume();
        })
        .is_err()
    {
        // Could not arm the trailing edge. Emit now rather than swallow the
        // value: overshooting the rate limit once is recoverable, dropping the
        // value the user released on is not — the peer would keep the volume
        // from the middle of the drag.
        flush_pending_volume();
    }
}

fn flush_pending_volume() {
    let Some(bridge) = BRIDGE.get() else { return };
    let mut throttle = bridge
        .throttle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    throttle.timer_armed = false;
    let Some(value) = throttle.pending.take() else {
        return;
    };
    throttle.last_emit = Some(Instant::now());
    drop(throttle);
    dispatch_volume(&bridge.app, value);
}

/// Hand the value to the frontend and let *it* talk to the daemon.
///
/// Deliberately not an IPC call from here: the frontend already owns the
/// `session.set_volume` path, including which session is addressed and the
/// "omitting `muted` preserves the peer's mute state" rule that
/// `VolumeControl.tsx` documents. A second, native caller of that RPC would be a
/// second place for those rules to drift.
///
/// `eval` is a channel send into the webview and is safe from any thread, which
/// matters because the trailing edge arrives on a helper thread.
fn dispatch_volume(app: &AppHandle, scalar: f64) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    // JSON for both halves so the literal stays correct whatever the event name
    // and payload contain — same reasoning as `open_native_settings`.
    let Ok(event) = serde_json::to_string(TRAY_VOLUME_EVENT) else {
        return;
    };
    let Ok(detail) = serde_json::to_string(&serde_json::json!({ "scalar": scalar })) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent({event},{{detail:{detail}}}))"
    ));
}

/// Should a value reported by the frontend be written onto the knob?
///
/// No, while the user is still dragging and the report has not caught up with
/// what they asked for. Yes once it agrees (within `ECHO_EPS`), or once
/// `ECHO_HOLD` has passed without agreement — at that point the device is not
/// doing what was asked and showing the truth beats continuing to lie.
fn accepts_echo(value: f64) -> bool {
    let Some(bridge) = BRIDGE.get() else {
        return true;
    };
    let mut throttle = bridge
        .throttle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (Some(local), Some(at)) = (throttle.local_value, throttle.local_at) else {
        return true;
    };
    if at.elapsed() >= ECHO_HOLD {
        throttle.local_value = None;
        throttle.local_at = None;
        return true;
    }
    if (value - local).abs() <= ECHO_EPS {
        // Converged: stop holding, so a change made from anywhere else lands
        // immediately instead of waiting out the rest of the window.
        throttle.local_value = None;
        throttle.local_at = None;
        return true;
    }
    false
}

// ---- construction ----------------------------------------------------------

fn tray_icon_image(state: IconState, theme: IconTheme) -> Option<Icon> {
    let image = icon::tray_image(state, theme);
    let (width, height) = (image.width(), image.height());
    Icon::from_rgba(image.rgba().to_vec(), width, height).ok()
}

fn ns(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

/// Build the label + slider row. Infallible once we are on the main thread; the
/// caller has already turned "no main thread" into "tray without a volume row"
/// rather than "no tray".
fn build_volume_row(mtm: MainThreadMarker, locale: NativeLocale) -> VolumeRow {
    let content_width = ROW_WIDTH - ROW_INSET_LEADING - ROW_INSET_TRAILING;

    let container = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(ROW_WIDTH, ROW_HEIGHT),
        ),
    );

    // `labelWithString:` is AppKit's own factory for exactly the configuration a
    // menu caption wants — not bezeled, no background, not editable, not
    // selectable — so it is used instead of setting those four properties by
    // hand and risking one of them drifting.
    let label = NSTextField::labelWithString(&ns(locale.volume()), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(ROW_INSET_LEADING, 25.0),
        NSSize::new(content_width, 15.0),
    ));
    // Size 0 asks NSFont for the *current* system menu size rather than a
    // hardcoded point size, so the row tracks the user's menu bar text size.
    label.setFont(Some(&NSFont::menuFontOfSize(0.0)));

    let slider = NSSlider::initWithFrame(
        NSSlider::alloc(mtm),
        NSRect::new(
            NSPoint::new(ROW_INSET_LEADING, 3.0),
            NSSize::new(content_width, 21.0),
        ),
    );
    slider.setMinValue(0.0);
    slider.setMaxValue(1.0);
    // Continuous: the action fires throughout the drag, which is what makes the
    // peer follow the finger instead of jumping once on release.
    slider.setContinuous(true);

    let target = VolumeTarget::new();
    // SAFETY: `setTarget:` does not retain, which is why `VolumeRow._target`
    // owns the object for as long as the row exists; the selector below is the
    // one `define_class!` registered on that exact class.
    unsafe {
        slider.setTarget(Some(&target));
        slider.setAction(Some(sel!(audiohubVolumeChanged:)));
    }

    container.addSubview(&label);
    container.addSubview(&slider);

    let item = NSMenuItem::new(mtm);
    item.setView(Some(&container));

    let separator = NSMenuItem::separatorItem(mtm);

    // Start hidden: the frontend has reported nothing yet, and a slider with no
    // session behind it would be a control that does nothing.
    separator.setHidden(true);
    item.setHidden(true);

    VolumeRow {
        separator,
        item,
        label,
        slider,
        _target: target,
    }
}

/// Create the status item. Runs from Tauri's `setup`, i.e. on the main thread.
///
/// Nothing here is fatal to the app except failing to create the tray itself: if
/// the `NSStatusItem` or its `NSMenu` cannot be reached the menu is left exactly
/// as it is today, minus the volume row.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let locale = crate::stored_native_locale();

    let show = MenuItem::with_id(SHOW_ID, locale.show(), true, None);
    // Informational only; disabled so it cannot be "clicked".
    let status = MenuItem::with_id("status", locale.connecting(), false, None);
    let quit_ui = MenuItem::with_id(QUIT_UI_ID, locale.quit_ui(), true, None);
    let quit_all = MenuItem::with_id(QUIT_ALL_ID, locale.quit_all(), true, None);
    let separator_top = PredefinedMenuItem::separator();
    let separator_bottom = PredefinedMenuItem::separator();

    let menu = Menu::with_items(&[
        &show as &dyn IsMenuItem,
        &separator_top,
        &status,
        &separator_bottom,
        &quit_ui,
        &quit_all,
    ])?;

    let mut builder = TrayIconBuilder::new()
        .with_id(TRAY_ID)
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("AudioHub")
        // macOS template image: AppKit keeps only the alpha channel and
        // recolours it for the current menu bar appearance. See icon.rs on why
        // this is the right answer for the menu bar and the wrong one for the
        // dock.
        .with_icon_as_template(true)
        .with_menu_on_left_click(true);
    // Before the first frontend report, claim nothing: `Connecting` is what the
    // store starts at too (`ConnState` initial is 'connecting').
    if let Some(image) = tray_icon_image(IconState::Connecting, IconTheme::Dark) {
        builder = builder.with_icon(image);
    }
    let tray = builder.build()?;

    let Some(mtm) = MainThreadMarker::new() else {
        // Cannot touch AppKit from here. The tray itself is already up and fully
        // functional; only the slider is missing, and `apply` will find no state
        // and leave it that way.
        crate::warn("tray volume row skipped: not on the main thread");
        return Ok(());
    };

    // The status item's own menu, i.e. the object AppKit will actually pop —
    // `tray-icon` sets it from `ContextMenu::ns_menu()`
    // (`tray-icon-0.24.2/src/platform_impl/macos/mod.rs:67`), so this is muda's
    // menu, reached through the handle that proves the whole chain is wired.
    let Some(ns_menu) = tray
        .ns_status_item()
        .and_then(|item| item.menu(mtm))
        .filter(|menu: &Retained<NSMenu>| menu.numberOfItems() >= 6)
    else {
        crate::warn("tray volume row skipped: status item menu unavailable");
        return Ok(());
    };

    let volume = build_volume_row(mtm, locale);
    // Directly after the status line and before the quit items: information
    // above, destructive actions below. Index 3 is the separator muda placed
    // after `status`; inserting pushes it down, which is why the row's own
    // separator goes in first and ends up *above* the slider.
    ns_menu.insertItem_atIndex(&volume.separator, 3);
    ns_menu.insertItem_atIndex(&volume.item, 4);

    let _ = BRIDGE.set(VolumeBridge {
        app: app.clone(),
        throttle: Mutex::new(Throttle::default()),
    });
    let _ = TRAY.set(MainThreadBound::new(
        MacTray {
            show,
            status,
            quit_ui,
            quit_all,
            volume,
            _menu: menu,
            tray,
        },
        mtm,
    ));
    Ok(())
}

// ---- updates ---------------------------------------------------------------

/// Everything `set_tray_status` can change, in one hop to the main thread.
///
/// One struct rather than a setter per surface for the reason `set_tray_status`
/// itself gives: the frontend dedupes on a single key covering all of it, so
/// splitting these would mean several main-thread round trips per transition and
/// several chances for the surfaces to disagree mid-flight.
pub struct TrayUpdate {
    pub locale: NativeLocale,
    pub online: bool,
    pub port: Option<u16>,
    /// `None` when the frontend sent no `state`, matching the existing rule that
    /// a report without one only refreshes copy.
    pub icon: Option<(IconState, IconTheme)>,
    /// `None` hides the whole row; `Some` shows it at that value.
    pub volume: Option<f64>,
    pub muted: bool,
}

/// Apply an update to the status item.
///
/// `set_tray_status` is a Tauri command and is normally already on the main
/// thread, but that is an implementation detail of the invoke system rather than
/// a guarantee — and every call below is AppKit or an AppKit-backed muda item.
/// `run_on_main_thread` is therefore unconditional; when already on main it
/// posts to the event loop rather than blocking, so it cannot deadlock.
pub fn apply(app: &AppHandle, update: TrayUpdate) {
    let _ = app.run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // Absent when `build` could not reach AppKit. Nothing to update, and
        // nothing broken — the tray is either up and static, or was never made.
        let Some(tray) = TRAY.get() else { return };
        let tray = tray.get(mtm);

        tray.show.set_text(update.locale.show());
        tray.status
            .set_text(update.locale.status(update.online, update.port));
        tray.quit_ui.set_text(update.locale.quit_ui());
        tray.quit_all.set_text(update.locale.quit_all());
        tray.volume.label.setStringValue(&ns(update.locale.volume()));

        match update.volume {
            Some(value) => {
                let value = value.clamp(0.0, 1.0);
                tray.volume.separator.setHidden(false);
                tray.volume.item.setHidden(false);
                // Muted greys the control out but leaves the knob where it is:
                // muting does not change the volume level, which is this
                // project's established semantics (see VolumeControl.tsx).
                tray.volume.slider.setEnabled(!update.muted);
                if accepts_echo(value) {
                    tray.volume.slider.setDoubleValue(value);
                }
            }
            None => {
                tray.volume.separator.setHidden(true);
                tray.volume.item.setHidden(true);
            }
        }

        if let Some((state, theme)) = update.icon {
            if let Some(image) = tray_icon_image(state, theme) {
                // set_icon + set_icon_as_template as two calls renders twice and
                // flickers on macOS; the combined setter is there for exactly
                // this.
                let _ = tray.tray.set_icon_with_as_template(Some(image), true);
            }
        }
    });
}
