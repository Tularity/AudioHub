//! System-audio capture backends (spec-m4b §B, spec-round2 §A) and third-party
//! virtual-card detection (spec-m4b §C).
//!
//! Windows is hand-rolled COM FFI on purpose: every `windows`/`windows-sys`
//! release new enough to expose ActivateAudioInterfaceAsync + the process
//! loopback activation params links its imports with raw-dylib, which the
//! x86_64-pc-windows-gnu toolchain this project ships on cannot link. Only
//! kernel32 and ole32 are linked (both present in the self-contained mingw
//! bundle); Mmdevapi/ntdll entry points are resolved with GetProcAddress.
//!
//! macOS goes through objc2 instead: `CATapDescription` is an Objective-C
//! class, and objc2/objc2-foundation/objc2-core-audio are already in the tree
//! via cpal. Both platforms end at the same shape — a real-time callback that
//! downmixes the front pair and pushes into a lock-free ring, a poller that
//! drains it, and a `failed()` slot so a dead capture reports instead of
//! streaming silence.

use anyhow::{anyhow, bail, Result};

/// Pick the first `available` backend in `list_backends()` order.
pub const BACKEND_AUTO: &str = "auto";
pub const BACKEND_WIN_PROC_EXCLUDE: &str = "win-proc-exclude";
pub const BACKEND_WIN_DEVICE_LOOPBACK: &str = "win-device-loopback";
pub const BACKEND_MAC_CATAP: &str = "mac-catap";
pub const BACKEND_MAC_SCK: &str = "mac-sck";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    pub available: bool,
    /// True when the backend keeps this process tree's own playback out of the
    /// capture — the hard condition for using it while we also play the peer's
    /// audio (plan §5 feedback loop).
    pub excludes_self: bool,
    pub note: String,
}

pub trait SysAudioCapture: Send {
    /// Appends mono f32 at `sample_rate()`, returns how many samples it added.
    fn read(&mut self, out: &mut Vec<f32>) -> usize;
    fn sample_rate(&self) -> u32;
    /// `Some(reason)` once the capture has died unrecoverably (endpoint
    /// invalidated, device unplugged); `read()` then keeps returning 0. Callers
    /// must surface this: a dead capture is indistinguishable from silence in
    /// the stream itself, so without it the peer receives digital silence with
    /// a perfectly healthy 0% loss report.
    fn failed(&self) -> Option<String> {
        None
    }
}

// ------------------------------------------------------------ downmix rule
//
// Pure geometry, kept out of the Windows-only module so it stays unit-testable
// on any host.
mod downmix {
    #![cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]

    /// dwChannelMask bits of the front pair (ksmedia.h).
    pub const SPEAKER_FRONT_LEFT: u32 = 0x1;
    pub const SPEAKER_FRONT_RIGHT: u32 = 0x2;

    /// Index of the channel carrying `bit` within an interleaved frame:
    /// WAVEFORMATEXTENSIBLE orders channels by ascending dwChannelMask bit, so a
    /// speaker sits at the number of set bits below its own.
    pub fn mask_channel_index(mask: u32, bit: u32) -> Option<usize> {
        (mask & bit != 0).then(|| (mask & (bit - 1)).count_ones() as usize)
    }

    /// The two interleaved channel indices to average into mono. Windows' shared
    /// mixer feeds ordinary stereo into FL/FR only and leaves the surround
    /// channels at zero, so averaging all `chans` would attenuate everything a
    /// 5.1/7.1 endpoint (HDMI to an AVR, most docks) plays by chans/2 — 9.5dB on
    /// 6 channels, 12dB on 8. Formats without a channel mask fall back to the
    /// first two channels; mono repeats channel 0 so the average is a passthrough.
    pub fn front_pair(mask: u32, chans: usize) -> (usize, usize) {
        if chans <= 1 {
            return (0, 0);
        }
        match (
            mask_channel_index(mask, SPEAKER_FRONT_LEFT),
            mask_channel_index(mask, SPEAKER_FRONT_RIGHT),
        ) {
            (Some(l), Some(r)) if l < chans && r < chans => (l, r),
            _ => (0, 1),
        }
    }

    /// Writes the mono downmix of `interleaved` (`chans` channels per frame,
    /// front pair at `pair`) into `out`, returning how many samples it wrote —
    /// `min(frames, out.len())`. `pair` must come from `front_pair` with the
    /// same `chans`, which is what keeps the indexing in range.
    ///
    /// Slice-shaped rather than Vec-shaped because macOS runs it inside a
    /// Core Audio IOProc, where allocating is forbidden.
    pub fn front_pair_mono_into(
        interleaved: &[f32],
        chans: usize,
        pair: (usize, usize),
        out: &mut [f32],
    ) -> usize {
        if chans == 0 {
            return 0;
        }
        let (a, b) = pair;
        let n = (interleaved.len() / chans).min(out.len());
        for (f, slot) in out[..n].iter_mut().enumerate() {
            let base = f * chans;
            *slot = (interleaved[base + a] + interleaved[base + b]) * 0.5;
        }
        n
    }

    /// Appends the mono downmix of `interleaved` to `out`. Only the Windows
    /// capture can grow a Vec from its callback thread; macOS goes through the
    /// slice form above.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn front_pair_mono(
        interleaved: &[f32],
        chans: usize,
        pair: (usize, usize),
        out: &mut Vec<f32>,
    ) {
        if chans == 0 {
            return;
        }
        let start = out.len();
        let frames = interleaved.len() / chans;
        out.resize(start + frames, 0.0);
        front_pair_mono_into(interleaved, chans, pair, &mut out[start..]);
    }
}

/// Written once by a capture's real-time side when the stream dies
/// unrecoverably, read by `SysAudioCapture::failed`. Shared by both platform
/// implementations.
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Default)]
struct FailSlot(std::sync::OnceLock<String>);

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl FailSlot {
    /// Records the reason and logs it once; later calls are no-ops, so the
    /// loop unwinding after the first fatal status cannot spam stderr.
    fn fail(&self, reason: String) {
        if self.0.set(reason.clone()).is_ok() {
            eprintln!("[audiohub] sysaudio capture stopped: {reason}");
        }
    }

    fn reason(&self) -> Option<String> {
        self.0.get().cloned()
    }

    fn is_failed(&self) -> bool {
        self.0.get().is_some()
    }
}

fn entry(id: &str, name: &str, available: bool, excludes_self: bool, note: &str) -> BackendInfo {
    BackendInfo {
        id: id.to_string(),
        name: name.to_string(),
        available,
        excludes_self,
        note: note.to_string(),
    }
}

/// All known backends in priority order (BACKEND_AUTO takes the first
/// available one), which on macOS is mac-catap before mac-sck (spec-round2
/// §A3).
///
/// Listing must never open a capture: on macOS creating the tap is exactly
/// what raises the TCC consent dialog, so availability is decided from the
/// runtime presence of the `CATapDescription` class and the OS version alone.
pub fn list_backends() -> Vec<BackendInfo> {
    vec![
        proc_exclude_info(),
        device_loopback_info(),
        catap_info(),
        sck_info(),
    ]
}

#[cfg(target_os = "windows")]
fn proc_exclude_info() -> BackendInfo {
    let build = win::os_build();
    let ok = build >= win::MIN_PROC_LOOPBACK_BUILD && win::has_activate_async();
    let note = if ok {
        "ActivateAudioInterfaceAsync process loopback, excluding this process tree".to_string()
    } else {
        format!(
            "needs Windows 10 2004 (build {}) with Mmdevapi!ActivateAudioInterfaceAsync; this host reports build {build}",
            win::MIN_PROC_LOOPBACK_BUILD
        )
    };
    entry(
        BACKEND_WIN_PROC_EXCLUDE,
        "Windows process loopback (self-excluded)",
        ok,
        true,
        &note,
    )
}

#[cfg(not(target_os = "windows"))]
fn proc_exclude_info() -> BackendInfo {
    entry(
        BACKEND_WIN_PROC_EXCLUDE,
        "Windows process loopback (self-excluded)",
        false,
        true,
        "Windows only",
    )
}

#[cfg(target_os = "windows")]
fn device_loopback_info() -> BackendInfo {
    entry(
        BACKEND_WIN_DEVICE_LOOPBACK,
        "Windows default render device loopback",
        true,
        false,
        "WASAPI endpoint loopback; captures our own playback too (feedback risk)",
    )
}

#[cfg(not(target_os = "windows"))]
fn device_loopback_info() -> BackendInfo {
    entry(
        BACKEND_WIN_DEVICE_LOOPBACK,
        "Windows default render device loopback",
        false,
        false,
        "Windows only",
    )
}

/// `available` is deliberately NOT gated on consent (spec-round2 §A1): TCC has
/// no public preflight for system-audio recording, and the only way to learn
/// the answer is to create a tap — which is the prompt. So the entry stays
/// available on every OS that has the API, `start_backend` reports the denial,
/// and the note carries whatever the last start attempt learned.
#[cfg(target_os = "macos")]
fn catap_info() -> BackendInfo {
    let (maj, min, patch) = mac::os_version();
    let (available, note) = if mac::api_present() {
        (true, mac::consent_note())
    } else {
        (
            false,
            format!(
                "needs macOS 14.2+ with Core Audio process taps; this host reports macOS {maj}.{min}.{patch}"
            ),
        )
    };
    entry(
        BACKEND_MAC_CATAP,
        "macOS Core Audio process tap",
        available,
        // The tap is built with initStereoGlobalTapButExcludeProcesses: on our
        // own audio process object, and start() refuses to run without it.
        true,
        &note,
    )
}

#[cfg(not(target_os = "macos"))]
fn catap_info() -> BackendInfo {
    entry(
        BACKEND_MAC_CATAP,
        "macOS Core Audio process tap",
        false,
        true,
        "macOS only",
    )
}

/// Declared, not implemented (spec-round2 §A2 allows this explicitly). A
/// correct SCStream audio path needs ScreenCaptureKit + CoreMedia + block2 +
/// a runtime-defined SCStreamOutput delegate class — four crates that are not
/// in the tree, for a backend nothing could exercise without screen-recording
/// consent. mac-catap covers every macOS this project targets; an SCK path
/// that has never run once would be worse than an honest gap.
fn sck_info() -> BackendInfo {
    entry(
        BACKEND_MAC_SCK,
        "macOS ScreenCaptureKit system audio",
        false,
        true,
        if cfg!(target_os = "macos") {
            "not implemented; use mac-catap (macOS 14.2+). ScreenCaptureKit would additionally need screen-recording consent"
        } else {
            "macOS only"
        },
    )
}

/// Resolves `id` (or BACKEND_AUTO / "") to a concrete backend description.
/// A known-but-unavailable id still resolves, so callers can report `note`.
pub fn resolve_backend(id: &str) -> Result<BackendInfo> {
    let all = list_backends();
    if id.is_empty() || id == BACKEND_AUTO {
        return all
            .into_iter()
            .find(|b| b.available)
            .ok_or_else(|| anyhow!("no system-audio capture backend is available on this platform"));
    }
    all.into_iter()
        .find(|b| b.id == id)
        .ok_or_else(|| anyhow!("unknown sysaudio backend '{id}'"))
}

pub fn start_backend(id: &str) -> Result<Box<dyn SysAudioCapture>> {
    let b = resolve_backend(id)?;
    if !b.available {
        bail!("sysaudio backend '{}' is not available: {}", b.id, b.note);
    }
    start_resolved(&b.id)
}

#[cfg(target_os = "windows")]
fn start_resolved(id: &str) -> Result<Box<dyn SysAudioCapture>> {
    match id {
        BACKEND_WIN_PROC_EXCLUDE => win::start(true),
        BACKEND_WIN_DEVICE_LOOPBACK => win::start(false),
        other => bail!("sysaudio backend '{other}' has no implementation"),
    }
}

#[cfg(target_os = "macos")]
fn start_resolved(id: &str) -> Result<Box<dyn SysAudioCapture>> {
    match id {
        BACKEND_MAC_CATAP => mac::start(),
        other => bail!("sysaudio backend '{other}' has no implementation"),
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn start_resolved(id: &str) -> Result<Box<dyn SysAudioCapture>> {
    bail!("sysaudio backend '{id}' has no implementation on this platform")
}

// ------------------------------------------------------------ virtual cards

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VirtualCard {
    pub id: String,
    pub name: String,
    /// "blackhole" | "vbcable" | "other"
    pub kind: String,
    pub present: bool,
}

/// (id, display name when absent, kind, lowercase name patterns)
const CATALOG: &[(&str, &str, &str, &[&str])] = &[
    ("blackhole", "BlackHole", "blackhole", &["blackhole"]),
    (
        "vbcable",
        "VB-Audio Virtual Cable",
        "vbcable",
        &["vb-audio", "vb audio", "cable input", "cable output", "voicemeeter"],
    ),
];

/// Conservative extra patterns reported as kind "other" when present.
const OTHER_PATTERNS: &[&str] = &["soundflower", "loopback audio", "virtual audio", "virtual cable"];

/// Device-name enumeration only (spec-m4b §C): never opens a device, so it can
/// never trigger a permission prompt. Catalog entries are always returned (with
/// `present: false` when missing) so the UI can offer the download link.
pub fn detect_virtual_cards() -> Vec<VirtualCard> {
    let names = device_names();
    let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let mut out: Vec<VirtualCard> = Vec::new();
    for (id, label, kind, pats) in CATALOG {
        let hit = lower.iter().position(|n| pats.iter().any(|p| n.contains(p)));
        out.push(VirtualCard {
            id: (*id).to_string(),
            name: hit.map_or_else(|| (*label).to_string(), |i| names[i].clone()),
            kind: (*kind).to_string(),
            present: hit.is_some(),
        });
    }
    for (i, n) in lower.iter().enumerate() {
        if CATALOG.iter().any(|(_, _, _, pats)| pats.iter().any(|p| n.contains(p))) {
            continue;
        }
        if !OTHER_PATTERNS.iter().any(|p| n.contains(p)) {
            continue;
        }
        let id = format!("other:{}", slug(n));
        if out.iter().any(|c| c.id == id) {
            continue;
        }
        out.push(VirtualCard {
            id,
            name: names[i].clone(),
            kind: "other".to_string(),
            present: true,
        });
    }
    out
}

fn slug(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !s.is_empty() {
            s.push('-');
            dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    s
}

/// Names of every audio device the default host can see. Name-only: building a
/// config (let alone a stream) is what triggers TCC on macOS.
fn device_names() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let mut out: Vec<String> = Vec::new();
    let host = cpal::default_host();
    if let Ok(devs) = host.devices() {
        for d in devs {
            if let Ok(n) = d.name() {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------ windows impl

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::{c_void, CString};
    use std::mem::size_of;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::{bail, Result};
    use ringbuf::traits::{Consumer, Observer, Producer, Split};
    use ringbuf::{HeapCons, HeapProd, HeapRb};

    use super::{FailSlot, SysAudioCapture};

    pub const MIN_PROC_LOOPBACK_BUILD: u32 = 19041; // Windows 10 2004

    type Hresult = i32;
    type Handle = *mut c_void;

    const S_OK: Hresult = 0;
    const S_FALSE: Hresult = 1;
    const E_POINTER: Hresult = -2147467261i32; // 0x80004003
    const E_NOINTERFACE: Hresult = -2147467262i32; // 0x80004002
    const COINIT_MULTITHREADED: u32 = 0;
    const CLSCTX_ALL: u32 = 23;
    const WAIT_OBJECT_0: u32 = 0;
    const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
    const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
    const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
    const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
    const BUFFER_DURATION_HNS: i64 = 2_000_000; // 200ms, in 100ns units
    const VT_BLOB: u16 = 65;
    /// Ring holds 4s of 48k mono: a stalled reader drops audio, never blocks
    /// the WASAPI thread.
    const RING_SAMPLES: usize = 48000 * 4;

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const IID_IUNKNOWN: Guid = Guid { d1: 0, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
    const IID_IAGILE_OBJECT: Guid = Guid {
        d1: 0x94EA2B94,
        d2: 0xE9CC,
        d3: 0x49E0,
        d4: [0xC0, 0xFF, 0xEE, 0x64, 0xCA, 0x8F, 0x5B, 0x90],
    };
    const IID_IAUDIOCLIENT: Guid = Guid {
        d1: 0x1CB9AD4C,
        d2: 0xDBFA,
        d3: 0x4C32,
        d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
    };
    const IID_IAUDIOCAPTURECLIENT: Guid = Guid {
        d1: 0xC8ADBD64,
        d2: 0xE71E,
        d3: 0x48A0,
        d4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
    };
    const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid {
        d1: 0xBCDE0395,
        d2: 0xE52F,
        d3: 0x467C,
        d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMMDEVICE_ENUMERATOR: Guid = Guid {
        d1: 0xA95664D2,
        d2: 0x9614,
        d3: 0x4F35,
        d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_ACTIVATE_COMPLETION: Guid = Guid {
        d1: 0x41D949AB,
        d2: 0x9862,
        d3: 0x444A,
        d4: [0x80, 0xF6, 0xC2, 0x61, 0x33, 0x4D, 0xA5, 0xEB],
    };
    const SUBTYPE_PCM: Guid = Guid {
        d1: 1,
        d2: 0,
        d3: 0x10,
        d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    };
    const SUBTYPE_IEEE_FLOAT: Guid = Guid {
        d1: 3,
        d2: 0,
        d3: 0x10,
        d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const i8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn CreateEventW(attrs: *mut c_void, manual: i32, initial: i32, name: *const u16) -> Handle;
        fn SetEvent(h: Handle) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> Hresult;
        fn CoUninitialize();
        fn CoCreateInstance(
            clsid: *const Guid,
            outer: *mut c_void,
            ctx: u32,
            iid: *const Guid,
            out: *mut *mut c_void,
        ) -> Hresult;
        fn CoTaskMemFree(p: *mut c_void);
    }

    // ---- COM vtables (first three slots are always IUnknown; slots we never
    // call still have to be declared so the ones after them land right)

    #[allow(dead_code)]
    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct IAudioClientVtbl {
        base: IUnknownVtbl,
        initialize: unsafe extern "system" fn(
            *mut c_void,
            i32,
            u32,
            i64,
            i64,
            *const WaveFormatEx,
            *const Guid,
        ) -> Hresult,
        get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
        get_stream_latency: unsafe extern "system" fn(*mut c_void, *mut i64) -> Hresult,
        get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
        is_format_supported: unsafe extern "system" fn(
            *mut c_void,
            i32,
            *const WaveFormatEx,
            *mut *mut WaveFormatEx,
        ) -> Hresult,
        get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> Hresult,
        get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> Hresult,
        start: unsafe extern "system" fn(*mut c_void) -> Hresult,
        stop: unsafe extern "system" fn(*mut c_void) -> Hresult,
        reset: unsafe extern "system" fn(*mut c_void) -> Hresult,
        set_event_handle: unsafe extern "system" fn(*mut c_void, Handle) -> Hresult,
        get_service:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct IAudioCaptureClientVtbl {
        base: IUnknownVtbl,
        get_buffer: unsafe extern "system" fn(
            *mut c_void,
            *mut *mut u8,
            *mut u32,
            *mut u32,
            *mut u64,
            *mut u64,
        ) -> Hresult,
        release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> Hresult,
        get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints:
            unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> Hresult,
        get_default_audio_endpoint:
            unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> Hresult,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const Guid,
            u32,
            *mut c_void,
            *mut *mut c_void,
        ) -> Hresult,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct IActivateOperationVtbl {
        base: IUnknownVtbl,
        get_activate_result:
            unsafe extern "system" fn(*mut c_void, *mut Hresult, *mut *mut c_void) -> Hresult,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct HandlerVtbl {
        base: IUnknownVtbl,
        activate_completed: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Hresult,
    }

    // WAVEFORMATEX is #pragma pack(1) in mmreg.h: 18 bytes, and
    // WAVEFORMATEXTENSIBLE's SubFormat sits at offset 24.
    #[allow(dead_code)]
    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct WaveFormatEx {
        w_format_tag: u16,
        n_channels: u16,
        n_samples_per_sec: u32,
        n_avg_bytes_per_sec: u32,
        n_block_align: u16,
        w_bits_per_sample: u16,
        cb_size: u16,
    }
    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const CHANNEL_MASK_OFFSET: usize = 20;
    const SUBFORMAT_OFFSET: usize = 24;

    #[allow(dead_code)]
    #[repr(C)]
    struct ActivationParams {
        activation_type: u32, // 1 = AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
        target_process_id: u32,
        loopback_mode: u32, // 1 = PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
    }

    /// PROPVARIANT carrying a VT_BLOB (24 bytes on x64, matching the union).
    #[allow(dead_code)]
    #[repr(C)]
    struct PropVariantBlob {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        cb_size: u32,
        _pad: u32,
        p_blob: *mut u8,
    }

    #[allow(dead_code)]
    #[repr(C)]
    struct OsVersionInfoExW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform_id: u32,
        csd: [u16; 128],
        sp_major: u16,
        sp_minor: u16,
        suite_mask: u16,
        product_type: u8,
        reserved: u8,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum SampleKind {
        S16,
        F32,
    }

    unsafe fn vtbl<T>(p: *mut c_void) -> *const T {
        *(p as *mut *const T)
    }

    /// Owns one COM interface pointer; every interface shares IUnknown's slots.
    struct Com(*mut c_void);

    impl Drop for Com {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { ((*vtbl::<IUnknownVtbl>(self.0)).release)(self.0) };
            }
        }
    }

    struct Ev(Handle);

    impl Drop for Ev {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hr_err(what: &str, hr: Hresult) -> String {
        format!("{what} failed: 0x{:08X}", hr as u32)
    }

    unsafe fn load_proc(dll: &str, name: &str) -> Option<*mut c_void> {
        let dll = CString::new(dll).ok()?;
        let module = LoadLibraryA(dll.as_ptr());
        if module.is_null() {
            return None;
        }
        let name = CString::new(name).ok()?;
        let p = GetProcAddress(module, name.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }

    /// Real OS build (GetVersionEx lies without a manifest; RtlGetVersion does not).
    pub fn os_build() -> u32 {
        unsafe {
            let Some(p) = load_proc("ntdll.dll", "RtlGetVersion") else {
                return 0;
            };
            let f: unsafe extern "system" fn(*mut OsVersionInfoExW) -> i32 = std::mem::transmute(p);
            let mut vi: OsVersionInfoExW = std::mem::zeroed();
            vi.size = size_of::<OsVersionInfoExW>() as u32;
            if f(&mut vi) == 0 {
                vi.build
            } else {
                0
            }
        }
    }

    pub fn has_activate_async() -> bool {
        unsafe { load_proc("Mmdevapi.dll", "ActivateAudioInterfaceAsync").is_some() }
    }

    // ---- completion handler (a minimal agile COM object)

    #[repr(C)]
    struct Handler {
        vtbl: *const HandlerVtbl,
        refs: AtomicU32,
        event: Handle,
    }

    /// The handler owns the event: COM may hold a reference past our own
    /// release, and a late ActivateCompleted must never SetEvent a closed
    /// (possibly recycled) handle.
    impl Drop for Handler {
        fn drop(&mut self) {
            if !self.event.is_null() {
                unsafe { CloseHandle(self.event) };
            }
        }
    }

    unsafe extern "system" fn handler_qi(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> Hresult {
        if out.is_null() {
            return E_POINTER;
        }
        let want = *iid;
        if want == IID_IUNKNOWN || want == IID_ACTIVATE_COMPLETION || want == IID_IAGILE_OBJECT {
            handler_add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn handler_add_ref(this: *mut c_void) -> u32 {
        (*(this as *mut Handler)).refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn handler_release(this: *mut c_void) -> u32 {
        let h = this as *mut Handler;
        let prev = (*h).refs.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            drop(Box::from_raw(h));
            0
        } else {
            prev - 1
        }
    }

    /// Called on a COM worker thread; the activation result is read by the
    /// thread that waited on `event`.
    unsafe extern "system" fn handler_completed(this: *mut c_void, _op: *mut c_void) -> Hresult {
        SetEvent((*(this as *mut Handler)).event);
        S_OK
    }

    static HANDLER_VTBL: HandlerVtbl = HandlerVtbl {
        base: IUnknownVtbl {
            query_interface: handler_qi,
            add_ref: handler_add_ref,
            release: handler_release,
        },
        activate_completed: handler_completed,
    };

    // ---- activation

    /// ActivateAudioInterfaceAsync on the process-loopback pseudo-device with
    /// EXCLUDE_TARGET_PROCESS_TREE against our own pid: the capture then holds
    /// everything the machine plays *except* what we play ourselves.
    unsafe fn activate_process_loopback() -> std::result::Result<Com, String> {
        let p = load_proc("Mmdevapi.dll", "ActivateAudioInterfaceAsync")
            .ok_or_else(|| "Mmdevapi!ActivateAudioInterfaceAsync not found".to_string())?;
        let activate: unsafe extern "system" fn(
            *const u16,
            *const Guid,
            *mut c_void,
            *mut c_void,
            *mut *mut c_void,
        ) -> Hresult = std::mem::transmute(p);

        let event = CreateEventW(ptr::null_mut(), 0, 0, ptr::null());
        if event.is_null() {
            return Err("CreateEventW (activation) failed".into());
        }
        let handler = Box::into_raw(Box::new(Handler {
            vtbl: &HANDLER_VTBL,
            refs: AtomicU32::new(1),
            event,
        }));

        let params = ActivationParams {
            activation_type: 1,
            target_process_id: GetCurrentProcessId(),
            loopback_mode: 1,
        };
        let mut pv = PropVariantBlob {
            vt: VT_BLOB,
            r1: 0,
            r2: 0,
            r3: 0,
            cb_size: size_of::<ActivationParams>() as u32,
            _pad: 0,
            p_blob: &params as *const ActivationParams as *mut u8,
        };
        let path = wide("VAD\\Process_Loopback");
        let mut op: *mut c_void = ptr::null_mut();
        let hr = activate(
            path.as_ptr(),
            &IID_IAUDIOCLIENT,
            &mut pv as *mut PropVariantBlob as *mut c_void,
            handler as *mut c_void,
            &mut op,
        );
        if hr != S_OK || op.is_null() {
            handler_release(handler as *mut c_void);
            return Err(hr_err("ActivateAudioInterfaceAsync", hr));
        }
        let op = Com(op);
        let waited = WaitForSingleObject(event, 3000);
        handler_release(handler as *mut c_void);
        if waited != WAIT_OBJECT_0 {
            return Err("process-loopback activation timed out".into());
        }
        let mut activate_hr: Hresult = S_OK;
        let mut client: *mut c_void = ptr::null_mut();
        let hr = ((*vtbl::<IActivateOperationVtbl>(op.0)).get_activate_result)(
            op.0,
            &mut activate_hr,
            &mut client,
        );
        if hr != S_OK {
            return Err(hr_err("GetActivateResult", hr));
        }
        if activate_hr != S_OK || client.is_null() {
            return Err(hr_err("process-loopback activation", activate_hr));
        }
        Ok(Com(client))
    }

    /// Plain WASAPI loopback on the default render endpoint (captures our own
    /// playback as well — the fallback backend).
    unsafe fn activate_device_loopback() -> std::result::Result<Com, String> {
        let mut enumerator: *mut c_void = ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator,
        );
        if hr != S_OK || enumerator.is_null() {
            return Err(hr_err("CoCreateInstance(MMDeviceEnumerator)", hr));
        }
        let enumerator = Com(enumerator);
        let mut device: *mut c_void = ptr::null_mut();
        let hr = ((*vtbl::<IMMDeviceEnumeratorVtbl>(enumerator.0)).get_default_audio_endpoint)(
            enumerator.0,
            0, // eRender
            0, // eConsole
            &mut device,
        );
        if hr != S_OK || device.is_null() {
            return Err(hr_err("GetDefaultAudioEndpoint(eRender)", hr));
        }
        let device = Com(device);
        let mut client: *mut c_void = ptr::null_mut();
        let hr = ((*vtbl::<IMMDeviceVtbl>(device.0)).activate)(
            device.0,
            &IID_IAUDIOCLIENT,
            CLSCTX_ALL,
            ptr::null_mut(),
            &mut client,
        );
        if hr != S_OK || client.is_null() {
            return Err(hr_err("IMMDevice::Activate(IAudioClient)", hr));
        }
        Ok(Com(client))
    }

    fn fixed_format(kind: SampleKind, channels: u16, rate: u32) -> WaveFormatEx {
        let bits: u16 = match kind {
            SampleKind::S16 => 16,
            SampleKind::F32 => 32,
        };
        let block = channels * bits / 8;
        WaveFormatEx {
            w_format_tag: match kind {
                SampleKind::S16 => WAVE_FORMAT_PCM,
                SampleKind::F32 => WAVE_FORMAT_IEEE_FLOAT,
            },
            n_channels: channels,
            n_samples_per_sec: rate,
            n_avg_bytes_per_sec: rate * block as u32,
            n_block_align: block,
            w_bits_per_sample: bits,
            cb_size: 0,
        }
    }

    #[derive(Clone, Copy)]
    struct Format {
        kind: SampleKind,
        channels: u16,
        rate: u32,
        /// dwChannelMask; 0 when the format is not WAVEFORMATEXTENSIBLE, i.e.
        /// when the speaker layout is unknown.
        mask: u32,
    }

    unsafe fn parse_format(p: *const WaveFormatEx) -> Option<Format> {
        let wf = ptr::read_unaligned(p);
        let (tag, channels, rate, bits) = (
            wf.w_format_tag,
            wf.n_channels,
            wf.n_samples_per_sec,
            wf.w_bits_per_sample,
        );
        let mut mask = 0u32;
        let kind = match tag {
            WAVE_FORMAT_PCM if bits == 16 => SampleKind::S16,
            WAVE_FORMAT_IEEE_FLOAT if bits == 32 => SampleKind::F32,
            WAVE_FORMAT_EXTENSIBLE => {
                mask = ptr::read_unaligned(
                    (p as *const u8).add(CHANNEL_MASK_OFFSET) as *const u32
                );
                let sub: Guid =
                    ptr::read_unaligned((p as *const u8).add(SUBFORMAT_OFFSET) as *const Guid);
                if sub == SUBTYPE_IEEE_FLOAT && bits == 32 {
                    SampleKind::F32
                } else if sub == SUBTYPE_PCM && bits == 16 {
                    SampleKind::S16
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(Format { kind, channels, rate, mask })
    }

    // ---- capture handle

    pub struct WinCapture {
        cons: HeapCons<f32>,
        rate: u32,
        stop: Arc<AtomicBool>,
        fail: Arc<FailSlot>,
        join: Option<JoinHandle<()>>,
    }

    impl SysAudioCapture for WinCapture {
        fn read(&mut self, out: &mut Vec<f32>) -> usize {
            // Whatever is still in the ring when the endpoint died is at most one
            // packet; reporting 0 from the failure onwards is the contract the
            // caller keys its error handling off.
            if self.fail.is_failed() {
                return 0;
            }
            let avail = self.cons.occupied_len();
            if avail == 0 {
                return 0;
            }
            let start = out.len();
            out.resize(start + avail, 0.0);
            let got = self.cons.pop_slice(&mut out[start..]);
            out.truncate(start + got);
            got
        }

        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn failed(&self) -> Option<String> {
            self.fail.reason()
        }
    }

    impl Drop for WinCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    pub fn start(exclude_self: bool) -> Result<Box<dyn SysAudioCapture>> {
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<u32, String>>();
        let (prod, cons) = HeapRb::<f32>::new(RING_SAMPLES).split();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let fail = Arc::new(FailSlot::default());
        let fail_thread = Arc::clone(&fail);
        let join = std::thread::Builder::new()
            .name("audiohub-sysaudio".into())
            .spawn(move || capture_thread(exclude_self, prod, stop_thread, ready_tx, fail_thread))?;
        // Under the daemon's 5s source-ack budget (conn.rs): a backend that
        // cannot start must surface as an error there, not as an ack timeout.
        match ready_rx.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok(rate)) => Ok(Box::new(WinCapture {
                cons,
                rate,
                stop,
                fail,
                join: Some(join),
            })),
            Ok(Err(e)) => {
                let _ = join.join();
                bail!("{e}")
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                bail!("sysaudio backend did not start within 4s")
            }
        }
    }

    fn capture_thread(
        exclude_self: bool,
        mut prod: HeapProd<f32>,
        stop: Arc<AtomicBool>,
        ready: mpsc::Sender<std::result::Result<u32, String>>,
        fail: Arc<FailSlot>,
    ) {
        unsafe {
            // Activation must run in the MTA; this thread is ours, so the only
            // way CoInitializeEx fails is a mode clash we did not create.
            let hr = CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED);
            let owned = hr == S_OK || hr == S_FALSE;
            if let Err(e) = run_capture(exclude_self, &mut prod, &stop, &ready, &fail) {
                // Startup failure: `start()` is still waiting and reports `e`
                // itself. A failure after that lands in the slot instead (the
                // receiver is gone by then, so this send is a no-op).
                let _ = ready.send(Err(e));
            }
            if owned {
                CoUninitialize();
            }
        }
    }

    unsafe fn run_capture(
        exclude_self: bool,
        prod: &mut HeapProd<f32>,
        stop: &AtomicBool,
        ready: &mpsc::Sender<std::result::Result<u32, String>>,
        fail: &FailSlot,
    ) -> std::result::Result<(), String> {
        // Process loopback ignores the endpoint mix format and demands an
        // explicit one; device loopback demands exactly the mix format. A
        // client whose Initialize failed is spent (documented), so every
        // format candidate gets a freshly activated one.
        let (client, fmt) = if exclude_self {
            let mut last = String::new();
            let mut opened = None;
            for cand in [
                (SampleKind::S16, 2u16, 48000u32),
                (SampleKind::F32, 2, 48000),
                (SampleKind::S16, 2, 44100),
            ] {
                let c = activate_process_loopback()?;
                let fmt = fixed_format(cand.0, cand.1, cand.2);
                let hr = ((*vtbl::<IAudioClientVtbl>(c.0)).initialize)(
                    c.0,
                    AUDCLNT_SHAREMODE_SHARED,
                    STREAM_FLAGS,
                    BUFFER_DURATION_HNS,
                    0,
                    &fmt,
                    ptr::null(),
                );
                if hr == S_OK {
                    // Our own fixed format carries no channel mask; it is always
                    // plain interleaved stereo.
                    opened = Some((
                        c,
                        Format { kind: cand.0, channels: cand.1, rate: cand.2, mask: 0 },
                    ));
                    break;
                }
                last = hr_err("IAudioClient::Initialize (process loopback)", hr);
            }
            match opened {
                Some(o) => o,
                None => return Err(last),
            }
        } else {
            let client = activate_device_loopback()?;
            let cv = vtbl::<IAudioClientVtbl>(client.0);
            let mut mix: *mut WaveFormatEx = ptr::null_mut();
            let hr = ((*cv).get_mix_format)(client.0, &mut mix);
            if hr != S_OK || mix.is_null() {
                return Err(hr_err("GetMixFormat", hr));
            }
            let parsed = parse_format(mix);
            let hr = ((*cv).initialize)(
                client.0,
                AUDCLNT_SHAREMODE_SHARED,
                STREAM_FLAGS,
                BUFFER_DURATION_HNS,
                0,
                mix,
                ptr::null(),
            );
            CoTaskMemFree(mix as *mut c_void);
            if hr != S_OK {
                return Err(hr_err("IAudioClient::Initialize (device loopback)", hr));
            }
            let f = parsed.ok_or_else(|| "unsupported endpoint mix format".to_string())?;
            (client, f)
        };
        let Format { kind, channels, rate, mask } = fmt;
        let cv = vtbl::<IAudioClientVtbl>(client.0);
        if channels == 0 || rate == 0 {
            return Err("device reported a zero-channel/zero-rate format".into());
        }

        let ev = Ev(CreateEventW(ptr::null_mut(), 0, 0, ptr::null()));
        if ev.0.is_null() {
            return Err("CreateEventW (buffer) failed".into());
        }
        let hr = ((*cv).set_event_handle)(client.0, ev.0);
        if hr != S_OK {
            return Err(hr_err("SetEventHandle", hr));
        }
        let mut capture: *mut c_void = ptr::null_mut();
        let hr = ((*cv).get_service)(client.0, &IID_IAUDIOCAPTURECLIENT, &mut capture);
        if hr != S_OK || capture.is_null() {
            return Err(hr_err("GetService(IAudioCaptureClient)", hr));
        }
        let capture = Com(capture);
        let capv = vtbl::<IAudioCaptureClientVtbl>(capture.0);
        let hr = ((*cv).start)(client.0);
        if hr != S_OK {
            return Err(hr_err("IAudioClient::Start", hr));
        }
        let _ = ready.send(Ok(rate));

        let chans = channels as usize;
        let pair = super::downmix::front_pair(mask, chans);
        let mut mono: Vec<f32> = Vec::with_capacity(4096);
        let mut wide: Vec<f32> = Vec::with_capacity(4096 * chans);
        // A negative HRESULT here is terminal (AUDCLNT_E_DEVICE_INVALIDATED when
        // the default endpoint changes or the DAC is unplugged): WASAPI keeps
        // returning it forever, so the session must end with a reason instead of
        // spinning on the 100ms wait and feeding the peer silence.
        'session: while !stop.load(Ordering::SeqCst) {
            // Loopback only raises the event while something is rendering, so
            // the timeout is what keeps the loop (and the stop check) alive.
            WaitForSingleObject(ev.0, 100);
            loop {
                let mut next: u32 = 0;
                let hr = ((*capv).get_next_packet_size)(capture.0, &mut next);
                if hr < 0 {
                    fail.fail(hr_err("GetNextPacketSize", hr));
                    break 'session;
                }
                if next == 0 {
                    break;
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames: u32 = 0;
                let mut bflags: u32 = 0;
                let hr = ((*capv).get_buffer)(
                    capture.0,
                    &mut data,
                    &mut frames,
                    &mut bflags,
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
                if hr < 0 {
                    fail.fail(hr_err("IAudioCaptureClient::GetBuffer", hr));
                    break 'session;
                }
                mono.clear();
                if frames > 0 {
                    if bflags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
                        mono.resize(frames as usize, 0.0);
                    } else {
                        wide.clear();
                        let n = frames as usize * chans;
                        wide.reserve(n);
                        for i in 0..n {
                            wide.push(match kind {
                                SampleKind::S16 => {
                                    let v: i16 =
                                        ptr::read_unaligned(data.add(i * 2) as *const i16);
                                    v as f32 / 32768.0
                                }
                                SampleKind::F32 => {
                                    ptr::read_unaligned(data.add(i * 4) as *const f32)
                                }
                            });
                        }
                        super::downmix::front_pair_mono(&wide, chans, pair, &mut mono);
                    }
                }
                ((*capv).release_buffer)(capture.0, frames);
                if !mono.is_empty() {
                    let _ = prod.push_slice(&mono); // full ring drops, never blocks WASAPI
                }
            }
        }
        ((*cv).stop)(client.0);
        Ok(())
    }
}

// ------------------------------------------------------------ macos impl

/// mac-catap (spec-round2 §A1): a Core Audio process tap that excludes this
/// process, carried by a private aggregate device whose IOProc we drain.
///
/// Nothing in here may run during `list_backends()`: creating the tap is the
/// call that raises the system-audio-recording TCC dialog, and listing must
/// never prompt. Availability therefore comes from `api_present()` (a class
/// lookup) and `os_version()` only.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_void, CStr};
    use std::mem::size_of;
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail, Result};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{AnyThread, Message};
    use objc2_core_audio::{
        kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
        kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
        kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
        kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
        kAudioDevicePermissionsError, kAudioDevicePropertyDeviceUID,
        kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
        kAudioHardwareIllegalOperationError, kAudioHardwareUnspecifiedError,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
        kAudioObjectUnknown, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
        kAudioSubTapUIDKey, kAudioTapPropertyFormat, kAudioTapPropertyUID,
        AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
        AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
        AudioObjectID, AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
    };
    use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSProcessInfo, NSString};
    use ringbuf::traits::{Consumer, Observer, Producer, Split};
    use ringbuf::{HeapCons, HeapProd, HeapRb};

    use super::{downmix, FailSlot, SysAudioCapture};

    /// objc2-core-audio keeps its own alias crate-private; this is the same
    /// `i32` every CoreAudio entry point here returns.
    type OSStatus = i32;

    /// 4s of 48k mono, like the Windows ring: a stalled reader costs audio, it
    /// never blocks the IOProc.
    const RING_SAMPLES: usize = 48000 * 4;
    /// Frames downmixed per pass. Core Audio hands over far less than this per
    /// cycle; anything larger is chunked, so the IOProc never has to allocate.
    const SCRATCH_FRAMES: usize = 4096;
    /// A running aggregate device calls its IOProc every cycle whether or not
    /// anything is playing, so this much silence from the callback means the
    /// device died — exactly what `failed()` exists to report.
    const STALL: Duration = Duration::from_secs(10);

    const CONSENT_UNKNOWN: u8 = 0;
    const CONSENT_GRANTED: u8 = 1;
    const CONSENT_REFUSED: u8 = 2;
    /// What the last `start()` learned about consent. TCC exposes no public
    /// preflight for system-audio recording, so this memo is the only honest
    /// thing `list_backends()` can say without prompting.
    static CONSENT: AtomicU8 = AtomicU8::new(CONSENT_UNKNOWN);

    /// True once macOS is new enough to have Core Audio process taps (14.2+).
    /// A class lookup: it touches no audio object and cannot prompt.
    pub fn api_present() -> bool {
        AnyClass::get(c"CATapDescription").is_some()
    }

    pub fn os_version() -> (i64, i64, i64) {
        let v = NSProcessInfo::processInfo().operatingSystemVersion();
        (
            v.majorVersion as i64,
            v.minorVersion as i64,
            v.patchVersion as i64,
        )
    }

    pub fn consent_note() -> String {
        match CONSENT.load(Ordering::Relaxed) {
            CONSENT_GRANTED => {
                "Core Audio process tap, excluding this process (consent granted)".to_string()
            }
            CONSENT_REFUSED => concat!(
                "Core Audio process tap; the last attempt was refused — allow system audio ",
                "recording for this binary (or the app that launched it) in System Settings > ",
                "Privacy & Security, then retry"
            )
            .to_string(),
            _ => concat!(
                "Core Audio process tap, excluding this process; the first capture asks for ",
                "system-audio-recording consent"
            )
            .to_string(),
        }
    }

    // ---- property plumbing

    /// `'nope'` reads better than `1852796517` in an error the user sees.
    fn fourcc(v: OSStatus) -> String {
        let b = (v as u32).to_be_bytes();
        if b.iter().all(|c| (0x20..0x7f).contains(c)) {
            format!("'{}' ({v})", b.iter().map(|c| *c as char).collect::<String>())
        } else {
            format!("{v}")
        }
    }

    fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    unsafe fn get_prop<T: Copy>(
        obj: AudioObjectID,
        mut a: AudioObjectPropertyAddress,
    ) -> std::result::Result<T, OSStatus> {
        let mut out = std::mem::zeroed::<T>();
        let mut size = size_of::<T>() as u32;
        let st = AudioObjectGetPropertyData(
            obj,
            NonNull::from(&mut a),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        );
        if st == 0 {
            Ok(out)
        } else {
            Err(st)
        }
    }

    /// Same, for the properties that take a qualifier (pid -> process object).
    unsafe fn get_prop_q<T: Copy, Q: Copy>(
        obj: AudioObjectID,
        mut a: AudioObjectPropertyAddress,
        qualifier: &Q,
    ) -> std::result::Result<T, OSStatus> {
        let mut out = std::mem::zeroed::<T>();
        let mut size = size_of::<T>() as u32;
        let st = AudioObjectGetPropertyData(
            obj,
            NonNull::from(&mut a),
            size_of::<Q>() as u32,
            (qualifier as *const Q).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        );
        if st == 0 {
            Ok(out)
        } else {
            Err(st)
        }
    }

    /// A CFStringRef-valued property. The HAL hands back a +1 reference and
    /// CFString is toll-free bridged with NSString, so `Retained` owns it.
    unsafe fn get_string(
        obj: AudioObjectID,
        a: AudioObjectPropertyAddress,
    ) -> std::result::Result<Retained<NSString>, OSStatus> {
        let raw: *mut NSString = get_prop(obj, a)?;
        Retained::from_raw(raw).ok_or(kAudioHardwareUnspecifiedError)
    }

    /// How many input AudioBuffers `dev` contributes. An aggregate lists its
    /// sub-device's input streams before the taps', so this is where the tap's
    /// buffer starts — non-zero whenever the default output is a combo device
    /// (a USB headset with a mic, a dock) rather than speakers alone.
    unsafe fn input_buffer_count(dev: AudioObjectID) -> usize {
        let mut a = addr(
            kAudioDevicePropertyStreamConfiguration,
            kAudioObjectPropertyScopeInput,
        );
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(
            dev,
            NonNull::from(&mut a),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        ) != 0
            || (size as usize) < size_of::<u32>()
        {
            return 0;
        }
        // u64 elements: an AudioBufferList carries pointers and must be
        // 8-aligned, which a Vec<u8> does not promise.
        let mut buf = vec![0u64; (size as usize + 7) / 8];
        let mut io = size;
        if AudioObjectGetPropertyData(
            dev,
            NonNull::from(&mut a),
            0,
            ptr::null(),
            NonNull::from(&mut io),
            NonNull::new(buf.as_mut_ptr()).unwrap().cast(),
        ) != 0
        {
            return 0;
        }
        (*buf.as_ptr().cast::<AudioBufferList>()).mNumberBuffers as usize
    }

    /// This process' Core Audio process object — the thing the tap excludes.
    /// `None` when the HAL knows of no audio client for our pid.
    unsafe fn process_object() -> Option<AudioObjectID> {
        let pid: i32 = std::process::id() as i32;
        let obj: AudioObjectID = get_prop_q(
            kAudioObjectSystemObject as AudioObjectID,
            addr(
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                kAudioObjectPropertyScopeGlobal,
            ),
            &pid,
        )
        .ok()?;
        (obj != kAudioObjectUnknown).then_some(obj)
    }

    /// Erases an Objective-C object to the root of the hierarchy so mixed-type
    /// dictionary values type-check.
    fn any<T: Message>(o: &T) -> &AnyObject {
        unsafe { &*(o as *const T).cast::<AnyObject>() }
    }

    fn key(k: &CStr) -> Retained<NSString> {
        NSString::from_str(k.to_str().unwrap_or_default())
    }

    // ---- owned handles (so an early return never leaks a system object)

    struct Tap(AudioObjectID);

    impl Tap {
        fn take(mut self) -> AudioObjectID {
            std::mem::replace(&mut self.0, kAudioObjectUnknown)
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            if self.0 != kAudioObjectUnknown {
                unsafe { AudioHardwareDestroyProcessTap(self.0) };
            }
        }
    }

    struct Agg(AudioObjectID);

    impl Agg {
        fn take(mut self) -> AudioObjectID {
            std::mem::replace(&mut self.0, kAudioObjectUnknown)
        }
    }

    impl Drop for Agg {
        fn drop(&mut self) {
            if self.0 != kAudioObjectUnknown {
                unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
            }
        }
    }

    // ---- the IOProc

    struct IoCtx {
        prod: HeapProd<f32>,
        skip: usize,
        tap_channels: usize,
        ticks: Arc<AtomicU64>,
    }

    /// Real-time context: no allocation, no locks, no panics (`front_pair`
    /// keeps every index it produces inside `chans`).
    unsafe extern "C-unwind" fn io_proc(
        _device: AudioObjectID,
        _now: NonNull<AudioTimeStamp>,
        input: NonNull<AudioBufferList>,
        _input_time: NonNull<AudioTimeStamp>,
        _output: NonNull<AudioBufferList>,
        _output_time: NonNull<AudioTimeStamp>,
        client: *mut c_void,
    ) -> OSStatus {
        let Some(ctx) = client.cast::<IoCtx>().as_mut() else {
            return 0;
        };
        ctx.ticks.fetch_add(1, Ordering::Relaxed);

        let list = input.as_ref();
        let count = list.mNumberBuffers as usize;
        if count == 0 {
            return 0;
        }
        // mBuffers is C's trailing variable-length array: declared [_; 1],
        // actually mNumberBuffers long.
        let bufs = list.mBuffers.as_ptr();
        let idx = ctx.skip.min(count - 1);
        let first = *bufs.add(idx);
        if first.mData.is_null() {
            return 0;
        }
        let mut scratch = [0f32; SCRATCH_FRAMES];
        let samples = |b: &objc2_core_audio_types::AudioBuffer| -> &[f32] {
            std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        };

        if first.mNumberChannels >= 2 {
            // Interleaved tap buffer. A tap's format carries no channel mask,
            // so `front_pair` falls back to channels 0/1 — which is the front
            // pair in every Core Audio layout (L R C LFE Ls Rs ...).
            let chans = first.mNumberChannels as usize;
            let data = samples(&first);
            let pair = downmix::front_pair(0, chans);
            let frames = data.len() / chans;
            let mut off = 0;
            while off < frames {
                let take = (frames - off).min(SCRATCH_FRAMES);
                let n = downmix::front_pair_mono_into(
                    &data[off * chans..(off + take) * chans],
                    chans,
                    pair,
                    &mut scratch[..take],
                );
                ctx.prod.push_slice(&scratch[..n]);
                off += take;
            }
        } else {
            // Deinterleaved tap: one mono AudioBuffer per channel. Average the
            // first two — the same front pair, laid out differently.
            let left = samples(&first);
            let right = if ctx.tap_channels >= 2 && idx + 1 < count {
                let second = *bufs.add(idx + 1);
                if second.mNumberChannels == 1 && !second.mData.is_null() {
                    samples(&second)
                } else {
                    left
                }
            } else {
                left
            };
            let frames = left.len().min(right.len());
            let mut off = 0;
            while off < frames {
                let take = (frames - off).min(SCRATCH_FRAMES);
                for i in 0..take {
                    scratch[i] = (left[off + i] + right[off + i]) * 0.5;
                }
                ctx.prod.push_slice(&scratch[..take]);
                off += take;
            }
        }
        0
    }

    // ---- capture handle

    pub struct CatapCapture {
        cons: HeapCons<f32>,
        rate: u32,
        fail: Arc<FailSlot>,
        stop: Arc<AtomicBool>,
        watcher: Option<JoinHandle<()>>,
        tap: AudioObjectID,
        agg: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        /// Handed to Core Audio as the IOProc's client data; freed in `Drop`
        /// only after the IOProc has been destroyed and can no longer run.
        ctx: *mut IoCtx,
    }

    /// The only non-Send field is the context pointer, which is never
    /// dereferenced outside the IOProc that Core Audio owns.
    unsafe impl Send for CatapCapture {}

    impl SysAudioCapture for CatapCapture {
        fn read(&mut self, out: &mut Vec<f32>) -> usize {
            if self.fail.is_failed() {
                return 0;
            }
            let avail = self.cons.occupied_len();
            if avail == 0 {
                return 0;
            }
            let start = out.len();
            out.resize(start + avail, 0.0);
            let got = self.cons.pop_slice(&mut out[start..]);
            out.truncate(start + got);
            got
        }

        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn failed(&self) -> Option<String> {
            self.fail.reason()
        }
    }

    impl Drop for CatapCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(j) = self.watcher.take() {
                let _ = j.join();
            }
            unsafe {
                AudioDeviceStop(self.agg, self.proc_id);
                AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
                drop(Box::from_raw(self.ctx));
                AudioHardwareDestroyAggregateDevice(self.agg);
                AudioHardwareDestroyProcessTap(self.tap);
            }
        }
    }

    // ---- start

    pub fn start() -> Result<Box<dyn SysAudioCapture>> {
        if !api_present() {
            let (maj, min, patch) = os_version();
            bail!(
                "Core Audio process taps need macOS 14.2+; this host reports macOS {maj}.{min}.{patch}"
            );
        }
        unsafe { start_inner() }
    }

    unsafe fn start_inner() -> Result<Box<dyn SysAudioCapture>> {
        // The aggregate needs a sub-device for its clock, and that sub-device
        // is the output the user is actually listening to.
        let out_dev: AudioObjectID = get_prop(
            kAudioObjectSystemObject as AudioObjectID,
            addr(
                kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
            ),
        )
        .map_err(|st| anyhow!("no default output device to tap (status {})", fourcc(st)))?;
        if out_dev == kAudioObjectUnknown {
            bail!("no default output device to tap");
        }
        let out_uid = get_string(
            out_dev,
            addr(kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal),
        )
        .map_err(|st| anyhow!("default output device has no UID (status {})", fourcc(st)))?;

        // Self-exclusion is the hard condition (plan §5): the daemon plays the
        // peer's audio out of this very process, so a tap that cannot leave us
        // out is a feedback loop. Refuse rather than howl.
        let me = process_object().ok_or_else(|| {
            anyhow!(
                "cannot self-exclude: the HAL reports no Core Audio process object for pid {} \
                 (this process has never opened an audio client), and a tap without our own \
                 process excluded would capture the peer audio we play back",
                std::process::id()
            )
        })?;

        let excluded = NSArray::from_retained_slice(&[NSNumber::new_u32(me)]);
        let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &excluded,
        );
        desc.setName(&NSString::from_str("AudioHub system capture"));
        // Private: visible only to us, and torn down with the process if we
        // die without running Drop, so a crash leaves no stray tap behind.
        desc.setPrivate(true);
        // Explicit: the tapped processes must keep reaching the speakers. This
        // is a mirror, not an interception.
        desc.setMuteBehavior(CATapMuteBehavior::Unmuted);

        let mut tap_id: AudioObjectID = kAudioObjectUnknown;
        let st = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id);
        if st != 0 || tap_id == kAudioObjectUnknown {
            if st == kAudioHardwareIllegalOperationError || st == kAudioDevicePermissionsError {
                CONSENT.store(CONSENT_REFUSED, Ordering::Relaxed);
            }
            bail!(
                "needs system audio recording consent: AudioHardwareCreateProcessTap returned \
                 status {}. Allow system audio recording for this binary (or the app that \
                 launched it) in System Settings > Privacy & Security, then retry",
                fourcc(st)
            );
        }
        CONSENT.store(CONSENT_GRANTED, Ordering::Relaxed);
        let tap = Tap(tap_id);

        let asbd: AudioStreamBasicDescription = get_prop(
            tap_id,
            addr(kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal),
        )
        .map_err(|st| anyhow!("tap format unreadable (status {})", fourcc(st)))?;
        let tap_uid = get_string(
            tap_id,
            addr(kAudioTapPropertyUID, kAudioObjectPropertyScopeGlobal),
        )
        .map_err(|st| anyhow!("tap UID unreadable (status {})", fourcc(st)))?;

        let agg = Agg(create_aggregate(&out_uid, &tap_uid)?);

        // The IOProc delivers at the aggregate's clock, not the tap's own
        // nominal format; the ASBD is only the fallback.
        let rate = get_prop::<f64>(
            agg.0,
            addr(
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            ),
        )
        .ok()
        .filter(|r| r.is_finite() && *r > 0.0)
        .unwrap_or(asbd.mSampleRate);
        if !(rate.is_finite() && rate > 0.0) {
            bail!("tap reported a zero sample rate");
        }

        let (prod, cons) = HeapRb::<f32>::new(RING_SAMPLES).split();
        let ticks = Arc::new(AtomicU64::new(0));
        let ctx = Box::into_raw(Box::new(IoCtx {
            prod,
            skip: input_buffer_count(out_dev),
            tap_channels: asbd.mChannelsPerFrame as usize,
            ticks: Arc::clone(&ticks),
        }));

        let mut proc_id: AudioDeviceIOProcID = None;
        let st = AudioDeviceCreateIOProcID(
            agg.0,
            Some(io_proc),
            ctx.cast(),
            NonNull::from(&mut proc_id),
        );
        if st != 0 || proc_id.is_none() {
            drop(Box::from_raw(ctx));
            bail!("AudioDeviceCreateIOProcID failed (status {})", fourcc(st));
        }
        let st = AudioDeviceStart(agg.0, proc_id);
        if st != 0 {
            AudioDeviceDestroyIOProcID(agg.0, proc_id);
            drop(Box::from_raw(ctx));
            bail!("AudioDeviceStart failed (status {})", fourcc(st));
        }

        let fail = Arc::new(FailSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        // Unwound by hand: the IOProc is live from here on, so an early return
        // must retire it before the guards below destroy the device under it.
        let watcher = match watch(Arc::clone(&fail), Arc::clone(&stop), ticks, me) {
            Ok(w) => w,
            Err(e) => {
                AudioDeviceStop(agg.0, proc_id);
                AudioDeviceDestroyIOProcID(agg.0, proc_id);
                drop(Box::from_raw(ctx));
                return Err(e);
            }
        };
        Ok(Box::new(CatapCapture {
            cons,
            rate: rate.round() as u32,
            fail,
            stop,
            watcher: Some(watcher),
            tap: tap.take(),
            agg: agg.take(),
            proc_id,
            ctx,
        }))
    }

    /// The private aggregate that carries the tap. Private so it never shows up
    /// in the user's device list and dies with us; auto-start so the tap runs
    /// as soon as the device does.
    unsafe fn create_aggregate(out_uid: &NSString, tap_uid: &NSString) -> Result<AudioObjectID> {
        let sub = NSDictionary::from_slices(&[&*key(kAudioSubDeviceUIDKey)], &[any(out_uid)]);
        let tap_entry = NSDictionary::from_slices(
            &[
                &*key(kAudioSubTapUIDKey),
                &*key(kAudioSubTapDriftCompensationKey),
            ],
            &[any(tap_uid), any(&*NSNumber::new_bool(true))],
        );
        let subs = NSArray::from_slice(&[&*sub]);
        let taps = NSArray::from_slice(&[&*tap_entry]);
        let uid = NSString::from_str(&format!("com.audiohub.systemtap.{}", std::process::id()));
        let name = NSString::from_str("AudioHub System Capture");
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);
        let desc = NSDictionary::from_slices(
            &[
                &*key(kAudioAggregateDeviceNameKey),
                &*key(kAudioAggregateDeviceUIDKey),
                &*key(kAudioAggregateDeviceMainSubDeviceKey),
                &*key(kAudioAggregateDeviceIsPrivateKey),
                &*key(kAudioAggregateDeviceIsStackedKey),
                &*key(kAudioAggregateDeviceTapAutoStartKey),
                &*key(kAudioAggregateDeviceSubDeviceListKey),
                &*key(kAudioAggregateDeviceTapListKey),
            ],
            &[
                any(&*name),
                any(&*uid),
                any(out_uid),
                any(&*yes),
                any(&*no),
                any(&*yes),
                any(&*subs),
                any(&*taps),
            ],
        );
        let mut id: AudioObjectID = kAudioObjectUnknown;
        // NSDictionary is toll-free bridged with CFDictionary.
        let cf = &*Retained::as_ptr(&desc).cast::<CFDictionary>();
        let st = AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut id));
        if st != 0 || id == kAudioObjectUnknown {
            bail!(
                "AudioHardwareCreateAggregateDevice failed (status {})",
                fourcc(st)
            );
        }
        Ok(id)
    }

    /// Watches the two ways a live tap can go quietly wrong: the device stops
    /// running (silence with a healthy loss report), or our process object is
    /// replaced so the exclusion list points at nothing and our own playback
    /// starts feeding back. Both end the session with a reason.
    fn watch(
        fail: Arc<FailSlot>,
        stop: Arc<AtomicBool>,
        ticks: Arc<AtomicU64>,
        expected: AudioObjectID,
    ) -> Result<JoinHandle<()>> {
        Ok(std::thread::Builder::new()
            .name("audiohub-catap".into())
            .spawn(move || {
                let mut last = ticks.load(Ordering::Relaxed);
                let mut since = Instant::now();
                let mut pass: u32 = 0;
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(500));
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let now = ticks.load(Ordering::Relaxed);
                    if now != last {
                        last = now;
                        since = Instant::now();
                    } else if since.elapsed() >= STALL {
                        fail.fail(
                            "the tap's aggregate device stopped running (no IOProc callback for 10s)"
                                .to_string(),
                        );
                        return;
                    }
                    // Every 2s, not every pass: this one crosses into
                    // coreaudiod. A transient failure to translate is not
                    // evidence of anything; only a different, valid object is.
                    pass = pass.wrapping_add(1);
                    if pass % 4 != 0 {
                        continue;
                    }
                    if let Some(cur) = unsafe { process_object() } {
                        if cur != expected {
                            fail.fail(format!(
                                "self-exclusion is stale: this process' Core Audio process object \
                                 changed ({expected} -> {cur}), so our own playback of the peer's \
                                 audio would be captured back into the tap"
                            ));
                            return;
                        }
                    }
                }
            })?)
    }
}

// The downmix rule is platform-independent arithmetic, so it is testable on the
// mac dev host even though only the Windows capture uses it. Masks are the
// KSAUDIO_SPEAKER_* layouts from ksmedia.h.
#[cfg(test)]
mod tests {
    use super::downmix::{front_pair, front_pair_mono, front_pair_mono_into, mask_channel_index};

    const STEREO: u32 = 0x3; // FL|FR
    const QUAD: u32 = 0x33; // FL|FR|BL|BR
    const S5_1: u32 = 0x3F; // FL|FR|FC|LFE|BL|BR
    const S5_1_SURROUND: u32 = 0x60F; // FL|FR|FC|LFE|SL|SR
    const S7_1: u32 = 0x63F;

    #[test]
    fn channel_index_counts_lower_bits() {
        assert_eq!(mask_channel_index(S5_1, 0x1), Some(0)); // FL
        assert_eq!(mask_channel_index(S5_1, 0x2), Some(1)); // FR
        assert_eq!(mask_channel_index(S5_1, 0x4), Some(2)); // FC
        assert_eq!(mask_channel_index(S5_1, 0x8), Some(3)); // LFE
        assert_eq!(mask_channel_index(S5_1, 0x10), Some(4)); // BL
        assert_eq!(mask_channel_index(S5_1_SURROUND, 0x200), Some(4)); // SL
        assert_eq!(mask_channel_index(S5_1_SURROUND, 0x10), None); // no BL here
    }

    #[test]
    fn front_pair_is_the_front_pair_on_every_layout() {
        for (mask, chans) in [(STEREO, 2), (QUAD, 4), (S5_1, 6), (S5_1_SURROUND, 6), (S7_1, 8)] {
            assert_eq!(front_pair(mask, chans), (0, 1), "mask 0x{mask:X}");
        }
    }

    #[test]
    fn front_pair_falls_back_without_a_mask() {
        assert_eq!(front_pair(0, 2), (0, 1));
        assert_eq!(front_pair(0, 6), (0, 1));
        // a layout carrying no front pair at all (FC|LFE|BL|BR)
        assert_eq!(front_pair(0x3C, 4), (0, 1));
        // mask claims channels the format does not have
        assert_eq!(front_pair(S7_1, 2), (0, 1));
    }

    #[test]
    fn front_pair_is_mono_passthrough() {
        assert_eq!(front_pair(0x4, 1), (0, 0));
        let mut out = Vec::new();
        front_pair_mono(&[0.5, -0.25, 1.0], 1, (0, 0), &mut out);
        assert_eq!(out, vec![0.5, -0.25, 1.0]);
    }

    /// The regression: Windows' shared mixer leaves the surround channels at
    /// zero, so a 5.1 endpoint must still hand the peer (L+R)/2, not (L+R)/6.
    #[test]
    fn surround_downmix_does_not_attenuate_stereo_content() {
        let frame = [0.8f32, 0.4, 0.0, 0.0, 0.0, 0.0];
        let mut interleaved = Vec::new();
        for _ in 0..3 {
            interleaved.extend_from_slice(&frame);
        }
        let mut out = Vec::new();
        front_pair_mono(&interleaved, 6, front_pair(S5_1, 6), &mut out);
        assert_eq!(out, vec![0.6, 0.6, 0.6]);
    }

    #[test]
    fn stereo_downmix_is_unchanged() {
        let mut out = Vec::new();
        front_pair_mono(&[1.0, 0.0, -1.0, 1.0], 2, front_pair(STEREO, 2), &mut out);
        assert_eq!(out, vec![0.5, 0.0]);
    }

    /// The macOS IOProc cannot allocate, so it downmixes through the slice
    /// form and chunks anything longer than its stack scratch. Chunking must
    /// give bit-identical output to one pass over the whole buffer.
    #[test]
    fn slice_downmix_matches_the_vec_form_and_chunks_cleanly() {
        let interleaved: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();
        let (chans, pair) = (4usize, front_pair(QUAD, 4));
        let mut want = Vec::new();
        front_pair_mono(&interleaved, chans, pair, &mut want);
        assert_eq!(want.len(), 16);

        let mut got = Vec::new();
        let mut scratch = [0f32; 3]; // deliberately not a divisor of 16
        let mut off = 0;
        while off < want.len() {
            let take = (want.len() - off).min(scratch.len());
            let n = front_pair_mono_into(
                &interleaved[off * chans..(off + take) * chans],
                chans,
                pair,
                &mut scratch[..take],
            );
            got.extend_from_slice(&scratch[..n]);
            off += take;
        }
        assert_eq!(got, want);
    }

    /// A short `out` truncates instead of writing past its end: the IOProc's
    /// stack scratch is what bounds a pass, and overrunning it would be a
    /// real-time buffer overflow rather than a dropped sample.
    #[test]
    fn slice_downmix_is_bounded_by_the_output() {
        let mut out = [0f32; 2];
        let n = front_pair_mono_into(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0], 2, (0, 1), &mut out);
        assert_eq!(n, 2);
        assert_eq!(out, [0.5, 0.5]);
        assert_eq!(front_pair_mono_into(&[1.0, 1.0], 0, (0, 1), &mut out), 0);
    }

    #[test]
    fn downmix_ignores_a_trailing_partial_frame_and_zero_channels() {
        let mut out = Vec::new();
        front_pair_mono(&[1.0, 1.0, 1.0], 2, (0, 1), &mut out);
        assert_eq!(out, vec![1.0]);
        front_pair_mono(&[1.0, 1.0], 0, (0, 1), &mut out);
        assert_eq!(out, vec![1.0]);
    }
}
