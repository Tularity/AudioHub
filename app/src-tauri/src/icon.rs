//! The tray glyph and the dock tile, as functions of connection state and theme.
//!
//! # Two icons, two entirely different rules about light and dark
//!
//! It is tempting to treat "light mode icon" as one problem with one answer.
//! It is not, and getting this wrong is the main thing this module exists to
//! prevent.
//!
//! **The menu bar wants a template image, not a light variant.** A macOS
//! template image carries its whole shape in alpha; AppKit throws the RGB away
//! and recolours the glyph itself — dark on a light menu bar, light on a dark
//! one, inverted again while the menu is open, and dimmed when the app is in
//! the background. Shipping our own light and dark tray icons would be strictly
//! worse than that: we would match two of those four cases and get the
//! highlight and vibrancy states wrong, on every macOS appearance Apple ships
//! next. So `tray-*.rgba` are pure alpha masks and `icon_as_template(true)`
//! stays exactly where it is (`main.rs`).
//!
//! Windows has no template concept — the notification area takes a literal
//! bitmap. There the polarity really is ours to choose, so [`tray_image`] tints
//! the same mask at call time: a light glyph for the default dark taskbar, a
//! dark glyph for a light one. One asset, both polarities, no second set of
//! files to keep in step.
//!
//! **The dock tile is the opposite case.** It is a full-colour plate, nothing
//! recolours it for us, and a dark plate on a light desktop is exactly the
//! thing the request was about. So there the two-variant approach is right, and
//! `dock-bg-{dark,light}.rgba` are genuinely two images.
//!
//! # Why plate and mark ship separately
//!
//! Theme picks the plate, state picks the mark, and they vary independently.
//! Two plates plus four single-byte masks is 786 KB embedded; the eight
//! pre-composited tiles they stand in for would be 2 MB. [`dock_rgba`] does the
//! source-over at runtime, which is the same twenty lines the Windows tray tint
//! already needs.
//!
//! Nothing here decodes an image format: every asset is raw, so the shell still
//! builds without tauri's `image-png` feature. See `icons/make-icons.py`.

use tauri::image::Image;

/// Accent teal, shared with `make-icons.py` and the UI's `--accent`.
const ACCENT: [u8; 3] = [0x31, 0xC8, 0xB0];

const TRAY_PX: u32 = 44;
/// Public because the Windows taskbar path in `main.rs` builds a tauri `Image`
/// from [`dock_rgba`] and needs the dimensions that go with it.
pub const DOCK_PX: u32 = 256;

/// Points, not pixels: the 256 px plate is the 2x rep of a 128 pt tile.
#[cfg(target_os = "macos")]
const DOCK_PT: f64 = 128.0;

const TRAY_OFFLINE: &[u8] = include_bytes!("../icons/tray-offline.rgba");
const TRAY_CONNECTING: &[u8] = include_bytes!("../icons/tray-connecting.rgba");
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray-idle.rgba");
const TRAY_ACTIVE: &[u8] = include_bytes!("../icons/tray-active.rgba");

const DOCK_BG_DARK: &[u8] = include_bytes!("../icons/dock-bg-dark.rgba");
const DOCK_BG_LIGHT: &[u8] = include_bytes!("../icons/dock-bg-light.rgba");

const DOCK_WAVE_OFFLINE: &[u8] = include_bytes!("../icons/dock-wave-offline.a8");
const DOCK_WAVE_CONNECTING: &[u8] = include_bytes!("../icons/dock-wave-connecting.a8");
const DOCK_WAVE_IDLE: &[u8] = include_bytes!("../icons/dock-wave-idle.a8");
const DOCK_WAVE_ACTIVE: &[u8] = include_bytes!("../icons/dock-wave-active.a8");

// A regenerated icon set that changed geometry is a compile error, not a
// runtime surprise — the same bargain `TRAY_PX` already had.
const TRAY_LEN: usize = (TRAY_PX * TRAY_PX * 4) as usize;
const DOCK_RGBA_LEN: usize = (DOCK_PX * DOCK_PX * 4) as usize;
const DOCK_A8_LEN: usize = (DOCK_PX * DOCK_PX) as usize;
const _: () = assert!(TRAY_OFFLINE.len() == TRAY_LEN);
const _: () = assert!(TRAY_CONNECTING.len() == TRAY_LEN);
const _: () = assert!(TRAY_IDLE.len() == TRAY_LEN);
const _: () = assert!(TRAY_ACTIVE.len() == TRAY_LEN);
const _: () = assert!(DOCK_BG_DARK.len() == DOCK_RGBA_LEN);
const _: () = assert!(DOCK_BG_LIGHT.len() == DOCK_RGBA_LEN);
const _: () = assert!(DOCK_WAVE_OFFLINE.len() == DOCK_A8_LEN);
const _: () = assert!(DOCK_WAVE_CONNECTING.len() == DOCK_A8_LEN);
const _: () = assert!(DOCK_WAVE_IDLE.len() == DOCK_A8_LEN);
const _: () = assert!(DOCK_WAVE_ACTIVE.len() == DOCK_A8_LEN);

/// What the icon is saying. Deliberately the existing vocabulary: three of these
/// are `ConnState` (`state/store.ts`) with `connecting`/`starting` folded
/// together, and the fourth is "online **and** at least one session is open".
/// No new semantics — `Active` is the same fact the sessions list already shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconState {
    Offline,
    Connecting,
    Idle,
    Active,
}

impl IconState {
    /// Unknown strings fall back to `Connecting`, the state that claims least:
    /// a version-skewed frontend should look like it is still working things
    /// out, not assert a connection it cannot vouch for.
    pub fn parse(s: &str) -> Self {
        match s {
            "offline" => Self::Offline,
            "idle" => Self::Idle,
            "active" => Self::Active,
            _ => Self::Connecting,
        }
    }

    fn tray_bytes(self) -> &'static [u8] {
        match self {
            Self::Offline => TRAY_OFFLINE,
            Self::Connecting => TRAY_CONNECTING,
            Self::Idle => TRAY_IDLE,
            Self::Active => TRAY_ACTIVE,
        }
    }

    fn dock_wave(self) -> &'static [u8] {
        match self {
            Self::Offline => DOCK_WAVE_OFFLINE,
            Self::Connecting => DOCK_WAVE_CONNECTING,
            Self::Idle => DOCK_WAVE_IDLE,
            Self::Active => DOCK_WAVE_ACTIVE,
        }
    }
}

/// Which way round the surface behind the icon is.
///
/// This is the *frontend's* `activeTheme()` — user preference folded over the
/// OS setting — not the OS setting alone. A user who has pinned light while the
/// system is dark gets a light UI, and the icon should agree with the window,
/// not with the system. See `lib/appearanceHost.ts`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconTheme {
    Light,
    Dark,
}

impl IconTheme {
    /// Dark is the fallback because it is the stylesheet's base `:root`, the
    /// same default `readSystemDark()` takes when `matchMedia` is missing.
    pub fn parse(s: &str) -> Self {
        match s {
            "light" => Self::Light,
            _ => Self::Dark,
        }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn dock_plate(self) -> &'static [u8] {
        match self {
            Self::Light => DOCK_BG_LIGHT,
            Self::Dark => DOCK_BG_DARK,
        }
    }
}

/// The menu-bar / notification-area glyph.
///
/// On macOS the returned image is the untouched alpha mask, to be handed to
/// AppKit as a template (see the module note). On Windows it is tinted for the
/// taskbar polarity, because nothing else will do it.
pub fn tray_image(state: IconState, theme: IconTheme) -> Image<'static> {
    let src = state.tray_bytes();

    #[cfg(target_os = "macos")]
    {
        let _ = theme;
        Image::new(src, TRAY_PX, TRAY_PX)
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Alpha is the whole shape; RGB in the asset is zero. A light taskbar
        // wants a near-black glyph, a dark one wants near-white. Not pure
        // black/white: the tray sits on translucent chrome that both extremes
        // tend to buzz against.
        let lum: u8 = match theme {
            IconTheme::Light => 0x1A,
            IconTheme::Dark => 0xF2,
        };
        let mut out = src.to_vec();
        for px in out.chunks_exact_mut(4) {
            px[0] = lum;
            px[1] = lum;
            px[2] = lum;
        }
        Image::new_owned(out, TRAY_PX, TRAY_PX)
    }
}

/// Plate for `theme` with the mark for `state` composited over it, straight
/// RGBA. Source-over on non-premultiplied pixels, matching `blend()` in
/// `make-icons.py` so the runtime tile and the bundled master agree.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn dock_rgba(state: IconState, theme: IconTheme) -> Vec<u8> {
    let mut out = theme.dock_plate().to_vec();
    let wave = state.dock_wave();
    for (i, &wa) in wave.iter().enumerate() {
        if wa == 0 {
            continue;
        }
        let a = wa as f32 / 255.0;
        let p = i * 4;
        let da = out[p + 3] as f32 / 255.0;
        let na = a + da * (1.0 - a);
        if na <= 0.0 {
            continue;
        }
        for k in 0..3 {
            let s = ACCENT[k] as f32 / 255.0;
            let d = out[p + k] as f32 / 255.0;
            out[p + k] = (((s * a + d * da * (1.0 - a)) / na) * 255.0).round() as u8;
        }
        out[p + 3] = (na * 255.0).round() as u8;
    }
    out
}

/// Replace the dock tile.
///
/// Tauri exposes no API for this at 2.11 — `AppHandle` has `default_window_icon`
/// (a getter) and nothing else — so this goes through AppKit directly, using the
/// `objc2-app-kit` that `mac_chrome.rs` already links.
///
/// The change is not persistent: it lasts for the life of the process, and the
/// Finder/Dock icon of the bundle itself is untouched.
#[cfg(target_os = "macos")]
pub fn set_dock_icon(state: IconState, theme: IconTheme) {
    use objc2::rc::Retained;
    // `AnyThread` is what carries `alloc()` for classes that are not
    // main-thread-only; NSImage and NSBitmapImageRep both qualify.
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{
        NSApplication, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage, NSImageRep,
    };
    use objc2_foundation::NSSize;

    // AppKit is main-thread-only. Callers reach this from a Tauri command, which
    // is not necessarily on main, so bail rather than risk it — the next state
    // change will try again.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    let px = dock_rgba(state, theme);
    let w = DOCK_PX as isize;

    let rep: Retained<NSBitmapImageRep> = unsafe {
        // A null `planes` makes AppKit own the buffer; we then copy into it.
        // Handing it a pointer to our Vec instead would be a dangling read the
        // moment `px` drops — the rep does not take a copy.
        let Some(rep) = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            w,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            w * 4,
            32,
        ) else {
            return;
        };
        let dst = rep.bitmapData();
        if dst.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), dst, px.len());
        rep
    };

    let img = NSImage::initWithSize(NSImage::alloc(), NSSize::new(DOCK_PT, DOCK_PT));
    unsafe {
        img.addRepresentation(&rep as &NSBitmapImageRep as &NSImageRep);
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&img));
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon(_state: IconState, _theme: IconTheme) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_parse_is_total_and_defaults_to_connecting() {
        assert_eq!(IconState::parse("offline"), IconState::Offline);
        assert_eq!(IconState::parse("idle"), IconState::Idle);
        assert_eq!(IconState::parse("active"), IconState::Active);
        assert_eq!(IconState::parse("connecting"), IconState::Connecting);
        // The point of the fallback: a state this build has never heard of must
        // not read as "connected".
        assert_eq!(IconState::parse("teleporting"), IconState::Connecting);
        assert_eq!(IconState::parse(""), IconState::Connecting);
    }

    #[test]
    fn theme_parse_defaults_to_dark() {
        assert_eq!(IconTheme::parse("light"), IconTheme::Light);
        assert_eq!(IconTheme::parse("dark"), IconTheme::Dark);
        assert_eq!(IconTheme::parse("sepia"), IconTheme::Dark);
    }

    #[test]
    fn every_state_has_a_distinct_tray_glyph() {
        let all = [
            IconState::Offline,
            IconState::Connecting,
            IconState::Idle,
            IconState::Active,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(
                    a.tray_bytes(),
                    b.tray_bytes(),
                    "{a:?} and {b:?} render identically — the state would be invisible"
                );
            }
        }
    }

    #[test]
    fn tray_assets_are_pure_alpha_masks() {
        // If RGB ever stops being zero, the macOS template contract is broken
        // and the Windows tint below would be compositing against stale colour.
        for s in [
            IconState::Offline,
            IconState::Connecting,
            IconState::Idle,
            IconState::Active,
        ] {
            for px in s.tray_bytes().chunks_exact(4) {
                assert_eq!([px[0], px[1], px[2]], [0, 0, 0], "{s:?} carries colour");
            }
        }
    }

    #[test]
    fn amplitude_ordering_is_monotonic() {
        // The states are meant to read as a progression. Total ink is the
        // cheapest proxy for "taller wave", and it catches an asset set
        // regenerated with the amplitudes shuffled.
        let ink =
            |s: IconState| -> u64 { s.tray_bytes().chunks_exact(4).map(|p| p[3] as u64).sum() };
        assert!(ink(IconState::Offline) < ink(IconState::Connecting));
        assert!(ink(IconState::Connecting) < ink(IconState::Idle));
        assert!(ink(IconState::Idle) < ink(IconState::Active));
    }

    #[test]
    fn dock_composite_is_opaque_and_takes_the_accent() {
        let px = dock_rgba(IconState::Active, IconTheme::Dark);
        assert_eq!(px.len(), DOCK_RGBA_LEN);
        // The mark must actually appear: at least one pixel close to ACCENT.
        let hit = px.chunks_exact(4).any(|p| {
            p[3] > 200
                && (p[0] as i16 - ACCENT[0] as i16).abs() < 24
                && (p[1] as i16 - ACCENT[1] as i16).abs() < 24
                && (p[2] as i16 - ACCENT[2] as i16).abs() < 24
        });
        assert!(hit, "composited tile has no accent-coloured mark");
    }

    #[test]
    fn light_and_dark_plates_differ_where_the_plate_is() {
        let l = dock_rgba(IconState::Idle, IconTheme::Light);
        let d = dock_rgba(IconState::Idle, IconTheme::Dark);
        assert_ne!(l, d);
        // Sample bare plate: horizontally centred, high enough to clear the
        // mark but well inside the rounded square (which is inset by 25 px).
        let p = (40 * DOCK_PX + DOCK_PX / 2) as usize * 4;
        assert_eq!(l[p + 3], 255, "sample point is not on the plate");
        assert_ne!(&l[p..p + 3], &d[p..p + 3]);
    }
}
