//! Default output device volume (spec-m4b §A1) plus the control-plane
//! ping-pong guard both daemons share.
//!
//! Deliberately dependency-free: the two real backends are hand-written FFI
//! (CoreAudio on macOS, COM/IAudioEndpointVolume on Windows) so the
//! x86_64-pc-windows-gnu link graph keeps its raw-dylib-free shape. Nothing
//! here touches the audio stream — volume is a control-plane property.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// `adjustable=false` means the device exposes no volume we can drive
/// (aggregate devices, most HDMI/optical outs). `scalar` is then display-only
/// and the setters fail; that is the documented trigger for the software-gain
/// fallback plan §7.2 defers to M5.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VolumeState {
    pub scalar: f32,
    pub muted: bool,
    pub adjustable: bool,
}

/// `SessionMsg::VolumeSet.src`: the change was made by the sender's own user.
pub const SRC_LOCAL: &str = "local";
/// `SessionMsg::VolumeSet.src`: the sender is relaying a change that already
/// came from a peer. Applied like any other, but it must never travel further.
pub const SRC_PEER: &str = "peer";

/// Volumes this close count as the same reading. macOS quantises the scalar it
/// stores (1/16 steps on several built-in outputs), so an exact compare would
/// report our own write back to the peer as a fresh local change.
pub const SAME_EPS: f32 = 0.035;

/// How many polls a peer-driven write stays armed for echo suppression. Bounded
/// so a write the device silently refused cannot swallow an unrelated later
/// change that happens to land on the same value.
const PENDING_POLLS: u32 = 3;

pub fn get_default_output_volume() -> Result<VolumeState> {
    get_output_volume(None)
}

/// Clamped to 0..=1. Errors when the device has no writable volume, which is
/// exactly the `adjustable=false` case.
pub fn set_default_output_volume(scalar: f32) -> Result<()> {
    set_output_volume(None, scalar)
}

pub fn set_default_output_mute(muted: bool) -> Result<()> {
    set_output_mute(None, muted)
}

/// `None` = the system default output, exactly as `get_default_output_volume`.
/// `Some(name)` addresses one output device by its name — the driver's virtual
/// speaker has its own volume control, and reaching it must not require making
/// it the system default.
pub fn get_output_volume(dev: Option<&str>) -> Result<VolumeState> {
    imp::get(dev)
}

pub fn set_output_volume(dev: Option<&str>, scalar: f32) -> Result<()> {
    if !scalar.is_finite() {
        bail!("volume scalar must be finite");
    }
    imp::set_volume(dev, scalar.clamp(0.0, 1.0))
}

pub fn set_output_mute(dev: Option<&str>, muted: bool) -> Result<()> {
    imp::set_mute(dev, muted)
}

/// Resolves the name the user typed against the devices a backend enumerated.
/// Shared so both backends refuse a typo the same way instead of silently
/// landing on the wrong card.
#[cfg(any(target_os = "macos", windows))]
fn match_by_name<T>(devices: Vec<(T, String)>, want: &str) -> Result<T> {
    // Exact first: two cards can differ only in case, and then the case the
    // user typed is the one they meant. Case-insensitive is the fallback, and
    // only when it is unambiguous.
    let exact = devices.iter().any(|(_, n)| n == want);
    let fold = want.to_lowercase();
    let (mut hits, rest): (Vec<_>, Vec<_>) = devices.into_iter().partition(|(_, n)| {
        if exact {
            n == want
        } else {
            n.to_lowercase() == fold
        }
    });
    match hits.len() {
        1 => Ok(hits.remove(0).0),
        0 => bail!(
            "no output device named {want:?}; available: {}",
            quoted(rest.iter().map(|(_, n)| n))
        ),
        n => bail!(
            "{n} output devices match {want:?}: {}",
            quoted(hits.iter().map(|(_, n)| n))
        ),
    }
}

#[cfg(any(target_os = "macos", windows))]
fn quoted<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<String> = names.map(|n| format!("{n:?}")).collect();
    if list.is_empty() {
        "(none)".to_string()
    } else {
        list.join(", ")
    }
}

/// How an error should name the device the caller asked for.
#[cfg(any(target_os = "macos", windows))]
fn label(dev: Option<&str>) -> String {
    match dev {
        None => "the default output device".to_string(),
        Some(name) => format!("output device {name:?}"),
    }
}

// ------------------------------------------------------------ ping-pong guard

/// What a daemon must do with an inbound `SessionMsg::VolumeSet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetAction {
    /// Write it to the real default output device, and report nothing back.
    Apply,
    /// Drop it; the payload is the reason, safe to log (no peer text in it).
    Ignore(&'static str),
}

/// Admission for a received VolumeSet. Only the provider of a spk stream owns
/// the output device the peer is asking about: a peer must not be able to move
/// this machine's volume through a mic stream, or through a stream that never
/// negotiated `volume_sync`. An unknown `src` tag is refused rather than
/// guessed at — the tag is what stops a relayed change from looping.
pub fn classify_set(is_spk_provider: bool, sync_enabled: bool, src: &str) -> SetAction {
    if !is_spk_provider {
        return SetAction::Ignore("volume_set is only valid on a spk stream we provide");
    }
    if !sync_enabled {
        return SetAction::Ignore("stream was not opened with volume_sync");
    }
    if src != SRC_LOCAL && src != SRC_PEER {
        return SetAction::Ignore("unknown volume_set src tag");
    }
    SetAction::Apply
}

/// Provider-side tracker: decides which device readings the peer must hear
/// about. A reading that merely echoes a write the peer itself asked for is
/// swallowed (spec §A2 source tagging); a genuine local change is reported.
pub struct VolumeSync {
    reported: Option<VolumeState>,
    /// Value written on the peer's behalf, plus its remaining lifetime in polls.
    pending: Option<(f32, bool, u32)>,
}

impl Default for VolumeSync {
    fn default() -> Self {
        VolumeSync::new()
    }
}

impl VolumeSync {
    pub fn new() -> VolumeSync {
        VolumeSync { reported: None, pending: None }
    }

    /// Records that `scalar`/`muted` were just written because the PEER asked.
    /// Called BEFORE the write so a poll racing it still recognises the echo.
    pub fn note_peer_apply(&mut self, scalar: f32, muted: bool) {
        self.pending = Some((scalar.clamp(0.0, 1.0), muted, PENDING_POLLS));
    }

    /// Feeds a fresh device reading. `Some(state)` = tell the peer; `None` =
    /// unchanged, or the echo of a peer-driven write.
    pub fn poll(&mut self, cur: VolumeState) -> Option<VolumeState> {
        if let Some((s, m, left)) = self.pending {
            if same(s, cur.scalar) && m == cur.muted {
                // the peer already knows this value: move our baseline silently
                self.pending = None;
                self.reported = Some(cur);
                return None;
            }
            self.pending = (left > 1).then_some((s, m, left - 1));
        }
        let unchanged = self.reported.map_or(false, |p| {
            same(p.scalar, cur.scalar) && p.muted == cur.muted && p.adjustable == cur.adjustable
        });
        if unchanged {
            return None;
        }
        self.reported = Some(cur);
        self.pending = None;
        Some(cur)
    }

    /// Marks `state` as already reported without emitting it (periodic refresh).
    pub fn note_reported(&mut self, state: VolumeState) {
        self.reported = Some(state);
    }

    pub fn last_reported(&self) -> Option<VolumeState> {
        self.reported
    }
}

fn same(a: f32, b: f32) -> bool {
    (a - b).abs() <= SAME_EPS
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod imp {
    //! CoreAudio: default output device -> kAudioDevicePropertyVolumeScalar /
    //! kAudioDevicePropertyMute on the Output scope. Master element first, then
    //! the per-channel elements; `adjustable` is the property's IsSettable.
    //! A named device resolves through kAudioHardwarePropertyDevices first; the
    //! probing below is identical either way.

    use super::{label, match_by_name, VolumeState};
    use anyhow::{anyhow, bail, Result};
    use std::ffi::c_void;

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropAddr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const ELEM_MAIN: u32 = 0; // kAudioObjectPropertyElementMain
    /// Channel elements probed when the device has no master volume.
    const MAX_CHANNEL_ELEMENTS: u32 = 16;

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName (= DeviceNameCFString)
    const SEL_STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams
    const SEL_VOLUME_SCALAR: u32 = fourcc(b"volm"); // kAudioDevicePropertyVolumeScalar
    const SEL_MUTE: u32 = fourcc(b"mute"); // kAudioDevicePropertyMute
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");

    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectHasProperty(id: AudioObjectID, addr: *const PropAddr) -> u8;
        fn AudioObjectIsPropertySettable(
            id: AudioObjectID,
            addr: *const PropAddr,
            out: *mut u8,
        ) -> OSStatus;
        fn AudioObjectGetPropertyDataSize(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            out_size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            io_size: *mut u32,
            out: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectSetPropertyData(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            size: u32,
            data: *const c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFStringGetLength(s: CFStringRef) -> isize;
        fn CFStringGetMaximumSizeForEncoding(len: isize, encoding: u32) -> isize;
        fn CFStringGetCString(s: CFStringRef, buf: *mut u8, size: isize, encoding: u32) -> u8;
    }

    fn at(selector: u32, scope: u32, element: u32) -> PropAddr {
        PropAddr { selector, scope, element }
    }

    fn has(dev: AudioObjectID, a: &PropAddr) -> bool {
        unsafe { AudioObjectHasProperty(dev, a) != 0 }
    }

    fn settable(dev: AudioObjectID, a: &PropAddr) -> bool {
        let mut out: u8 = 0;
        let st = unsafe { AudioObjectIsPropertySettable(dev, a, &mut out) };
        st == 0 && out != 0
    }

    fn get_u32(dev: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut sz: u32 = 4;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                a,
                0,
                std::ptr::null(),
                &mut sz,
                &mut v as *mut u32 as *mut c_void,
            )
        };
        (st == 0 && sz == 4).then_some(v)
    }

    fn get_f32(dev: AudioObjectID, a: &PropAddr) -> Option<f32> {
        let mut v: f32 = 0.0;
        let mut sz: u32 = 4;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                a,
                0,
                std::ptr::null(),
                &mut sz,
                &mut v as *mut f32 as *mut c_void,
            )
        };
        (st == 0 && sz == 4).then_some(v)
    }

    fn set_f32(dev: AudioObjectID, a: &PropAddr, v: f32) -> Result<()> {
        let st = unsafe {
            AudioObjectSetPropertyData(
                dev,
                a,
                0,
                std::ptr::null(),
                4,
                &v as *const f32 as *const c_void,
            )
        };
        if st != 0 {
            bail!("AudioObjectSetPropertyData(volm) failed: OSStatus {st}");
        }
        Ok(())
    }

    fn set_u32(dev: AudioObjectID, a: &PropAddr, v: u32) -> Result<()> {
        let st = unsafe {
            AudioObjectSetPropertyData(
                dev,
                a,
                0,
                std::ptr::null(),
                4,
                &v as *const u32 as *const c_void,
            )
        };
        if st != 0 {
            bail!("AudioObjectSetPropertyData(mute) failed: OSStatus {st}");
        }
        Ok(())
    }

    fn prop_size(dev: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut sz: u32 = 0;
        let st = unsafe {
            AudioObjectGetPropertyDataSize(dev, a, 0, std::ptr::null(), &mut sz)
        };
        (st == 0).then_some(sz)
    }

    fn default_output_device() -> Result<AudioObjectID> {
        let a = at(SEL_DEFAULT_OUTPUT, SCOPE_GLOBAL, ELEM_MAIN);
        let dev = get_u32(SYSTEM_OBJECT, &a)
            .ok_or_else(|| anyhow!("cannot read the default output device"))?;
        if dev == 0 {
            bail!("no default output device");
        }
        Ok(dev)
    }

    /// AudioObjectGetPropertyData hands back a +1 CFStringRef here, so the
    /// release is ours; the string is copied out before it is dropped.
    fn device_name(dev: AudioObjectID) -> Option<String> {
        let a = at(SEL_NAME, SCOPE_GLOBAL, ELEM_MAIN);
        let mut s: CFStringRef = std::ptr::null();
        let mut sz: u32 = std::mem::size_of::<CFStringRef>() as u32;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                &a,
                0,
                std::ptr::null(),
                &mut sz,
                &mut s as *mut CFStringRef as *mut c_void,
            )
        };
        if st != 0 || s.is_null() {
            return None;
        }
        let name = cf_string(s);
        unsafe { CFRelease(s) };
        name
    }

    fn cf_string(s: CFStringRef) -> Option<String> {
        let cap = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), UTF8) } + 1;
        if cap <= 1 {
            return Some(String::new());
        }
        let mut buf = vec![0u8; cap as usize];
        if unsafe { CFStringGetCString(s, buf.as_mut_ptr(), cap, UTF8) } == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        String::from_utf8(buf).ok()
    }

    /// Every device that can play: a present output stream is the same test
    /// CoreAudio itself uses to decide a device belongs in the output list.
    fn output_devices() -> Vec<(AudioObjectID, String)> {
        let a = at(SEL_DEVICES, SCOPE_GLOBAL, ELEM_MAIN);
        let Some(bytes) = prop_size(SYSTEM_OBJECT, &a) else {
            return Vec::new();
        };
        let mut ids = vec![0u32; bytes as usize / 4];
        let mut sz = bytes;
        let st = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                std::ptr::null(),
                &mut sz,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return Vec::new();
        }
        ids.truncate(sz as usize / 4);
        ids.into_iter()
            .filter(|&d| {
                prop_size(d, &at(SEL_STREAMS, SCOPE_OUTPUT, ELEM_MAIN)).is_some_and(|n| n > 0)
            })
            .filter_map(|d| device_name(d).map(|n| (d, n)))
            .collect()
    }

    fn resolve(dev: Option<&str>) -> Result<AudioObjectID> {
        match dev {
            None => default_output_device(),
            Some(name) => match_by_name(output_devices(), name),
        }
    }

    /// Present channel elements of `selector`, in element order.
    fn channel_elements(dev: AudioObjectID, selector: u32) -> Vec<PropAddr> {
        (1..=MAX_CHANNEL_ELEMENTS)
            .map(|ch| at(selector, SCOPE_OUTPUT, ch))
            .filter(|a| has(dev, a))
            .collect()
    }

    /// True exactly when `set_volume` would find something to write: the master
    /// element, else any per-channel element. Shared so `adjustable` cannot
    /// disagree with the setter — a read-only master over writable channels used
    /// to report `adjustable=false` while `set_volume` happily worked, greying
    /// the slider out for no reason.
    fn volume_writable(dev: AudioObjectID) -> bool {
        let master = at(SEL_VOLUME_SCALAR, SCOPE_OUTPUT, ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return true;
        }
        channel_elements(dev, SEL_VOLUME_SCALAR)
            .iter()
            .any(|a| settable(dev, a))
    }

    pub fn get(target: Option<&str>) -> Result<VolumeState> {
        let dev = resolve(target)?;
        let master = at(SEL_VOLUME_SCALAR, SCOPE_OUTPUT, ELEM_MAIN);
        let adjustable = volume_writable(dev);
        let scalar = if has(dev, &master) {
            get_f32(dev, &master).unwrap_or(0.0)
        } else {
            // No master volume: aggregate devices and many HDMI/optical outs
            // land here. Average whatever channels exist.
            let vals: Vec<f32> = channel_elements(dev, SEL_VOLUME_SCALAR)
                .iter()
                .filter_map(|a| get_f32(dev, a))
                .collect();
            if vals.is_empty() {
                0.0
            } else {
                vals.iter().sum::<f32>() / vals.len() as f32
            }
        };
        let mute = at(SEL_MUTE, SCOPE_OUTPUT, ELEM_MAIN);
        let muted = if has(dev, &mute) {
            get_u32(dev, &mute).unwrap_or(0) != 0
        } else {
            channel_elements(dev, SEL_MUTE)
                .iter()
                .filter_map(|a| get_u32(dev, a))
                .any(|v| v != 0)
        };
        Ok(VolumeState {
            scalar: scalar.clamp(0.0, 1.0),
            muted,
            adjustable,
        })
    }

    pub fn set_volume(target: Option<&str>, scalar: f32) -> Result<()> {
        let dev = resolve(target)?;
        let master = at(SEL_VOLUME_SCALAR, SCOPE_OUTPUT, ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return set_f32(dev, &master, scalar);
        }
        let mut wrote = 0usize;
        for a in channel_elements(dev, SEL_VOLUME_SCALAR) {
            if settable(dev, &a) {
                set_f32(dev, &a, scalar)?;
                wrote += 1;
            }
        }
        if wrote == 0 {
            bail!("{} has no adjustable volume", label(target));
        }
        Ok(())
    }

    pub fn set_mute(target: Option<&str>, muted: bool) -> Result<()> {
        let dev = resolve(target)?;
        let v = u32::from(muted);
        let master = at(SEL_MUTE, SCOPE_OUTPUT, ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return set_u32(dev, &master, v);
        }
        let mut wrote = 0usize;
        for a in channel_elements(dev, SEL_MUTE) {
            if settable(dev, &a) {
                set_u32(dev, &a, v)?;
                wrote += 1;
            }
        }
        if wrote == 0 {
            bail!("{} has no mute control", label(target));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    //! COM by hand: MMDeviceEnumerator -> GetDefaultAudioEndpoint(eRender,
    //! eConsole) -> Activate(IAudioEndpointVolume). A named device swaps the
    //! middle step for EnumAudioEndpoints + PKEY_Device_FriendlyName. Vtable
    //! layouts are the frozen ABI of mmdeviceapi.h / endpointvolume.h; slots we
    //! never call are declared as `usize` so nothing can be invoked through
    //! them by accident.

    use super::{label, match_by_name, VolumeState};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::ptr;

    type HRESULT = i32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct GUID {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xBCDE0395,
        d2: 0xE52F,
        d3: 0x467C,
        d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xA95664D2,
        d2: 0x9614,
        d3: 0x4F35,
        d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IAUDIO_ENDPOINT_VOLUME: GUID = GUID {
        d1: 0x5CDF2C82,
        d2: 0x841E,
        d3: 0x4546,
        d4: [0x97, 0x22, 0x0C, 0xF7, 0x40, 0x78, 0x22, 0x9A],
    };

    /// PKEY_Device_FriendlyName — the string the Sound control panel shows,
    /// and therefore the one a user can type back at us.
    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0xA45C254E,
            d2: 0xDF1C,
            d3: 0x4EFD,
            d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
        },
        pid: 14,
    };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const CLSCTX_ALL: u32 = 0x17;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106
    const E_RENDER: u32 = 0; // EDataFlow::eRender
    const E_CONSOLE: u32 = 0; // ERole::eConsole
    const DEVICE_STATE_ACTIVE: u32 = 0x1;
    const STGM_READ: u32 = 0x0;
    const VT_LPWSTR: u16 = 31;

    #[repr(C)]
    struct PropertyKey {
        fmtid: GUID,
        pid: u32,
    }

    /// PROPVARIANT: 2-byte vt, three pads, then the 8-aligned union. Only the
    /// VT_LPWSTR case is read, and PropVariantClear owns the teardown.
    #[repr(C)]
    struct PropVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        val: [u64; 2],
    }

    impl PropVariant {
        fn empty() -> PropVariant {
            PropVariant { vt: 0, r1: 0, r2: 0, r3: 0, val: [0; 2] }
        }
    }

    impl Drop for PropVariant {
        fn drop(&mut self) {
            unsafe { PropVariantClear(self) };
        }
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> HRESULT;
        fn CoUninitialize();
        fn CoCreateInstance(
            clsid: *const GUID,
            outer: *mut c_void,
            ctx: u32,
            iid: *const GUID,
            out: *mut *mut c_void,
        ) -> HRESULT;
        fn PropVariantClear(pvar: *mut PropVariant) -> HRESULT;
    }

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
        get_default_audio_endpoint:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
        get_device: usize,
        register_endpoint_notification_callback: usize,
        unregister_endpoint_notification_callback: usize,
    }

    #[repr(C)]
    struct IMMDeviceCollectionVtbl {
        base: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            u32,
            *mut c_void,
            *mut *mut c_void,
        ) -> HRESULT,
        open_property_store:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
        get_id: usize,
        get_state: usize,
    }

    #[repr(C)]
    struct IPropertyStoreVtbl {
        base: IUnknownVtbl,
        get_count: usize,
        get_at: usize,
        get_value: unsafe extern "system" fn(
            *mut c_void,
            *const PropertyKey,
            *mut PropVariant,
        ) -> HRESULT,
        set_value: usize,
        commit: usize,
    }

    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        base: IUnknownVtbl,
        register_control_change_notify: usize,
        unregister_control_change_notify: usize,
        get_channel_count: usize,
        set_master_volume_level: usize,
        set_master_volume_level_scalar:
            unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
        get_master_volume_level: usize,
        get_master_volume_level_scalar:
            unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
        set_channel_volume_level: usize,
        set_channel_volume_level_scalar: usize,
        get_channel_volume_level: usize,
        get_channel_volume_level_scalar: usize,
        set_mute: unsafe extern "system" fn(*mut c_void, i32, *const GUID) -> HRESULT,
        get_mute: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
        get_volume_step_info: usize,
        volume_step_up: usize,
        volume_step_down: usize,
        query_hardware_support: usize,
        get_volume_range: usize,
    }

    /// Balances CoInitializeEx. A thread another library already put in a
    /// different apartment (cpal's WASAPI backend does this) reports
    /// RPC_E_CHANGED_MODE: that apartment is fine for us, and it is not ours
    /// to tear down.
    struct Apartment {
        owned: bool,
    }

    impl Apartment {
        fn enter() -> Apartment {
            let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
            Apartment { owned: hr >= 0 }
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.owned {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct ComPtr(*mut c_void);

    impl ComPtr {
        fn null() -> ComPtr {
            ComPtr(ptr::null_mut())
        }

        /// Safety: the caller must name the interface this pointer really is.
        unsafe fn vtbl<V>(&self) -> *const V {
            *(self.0 as *const *const V)
        }
    }

    impl Drop for ComPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let v = self.vtbl::<IUnknownVtbl>();
                    ((*v).release)(self.0);
                }
            }
        }
    }

    /// Field order is the drop order: the interface must Release before the
    /// apartment it was created in goes away.
    struct Endpoint {
        vol: ComPtr,
        _apt: Apartment,
    }

    fn check(hr: HRESULT, what: &str) -> Result<()> {
        if hr < 0 {
            bail!("{what} failed: HRESULT 0x{:08X}", hr as u32);
        }
        Ok(())
    }

    /// Safety: `p` must be a NUL-terminated wide string, which is what a
    /// VT_LPWSTR PROPVARIANT holds.
    unsafe fn wide_string(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(p, n)).ok()
    }

    fn friendly_name(device: &ComPtr) -> Option<String> {
        let mut store = ComPtr::null();
        let hr = unsafe {
            let v = device.vtbl::<IMMDeviceVtbl>();
            ((*v).open_property_store)(device.0, STGM_READ, &mut store.0)
        };
        if hr < 0 {
            return None;
        }
        let mut pv = PropVariant::empty();
        let hr = unsafe {
            let v = store.vtbl::<IPropertyStoreVtbl>();
            ((*v).get_value)(store.0, &PKEY_DEVICE_FRIENDLY_NAME, &mut pv)
        };
        if hr < 0 || pv.vt != VT_LPWSTR {
            return None;
        }
        unsafe { wide_string(pv.val[0] as *const u16) }
    }

    /// Every active render endpoint, paired with the name the user sees.
    /// Endpoints whose name will not read are dropped: an unnameable device
    /// cannot be the one that was asked for.
    fn render_devices(enumerator: &ComPtr) -> Result<Vec<(ComPtr, String)>> {
        let mut coll = ComPtr::null();
        check(
            unsafe {
                let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                ((*v).enum_audio_endpoints)(
                    enumerator.0,
                    E_RENDER,
                    DEVICE_STATE_ACTIVE,
                    &mut coll.0,
                )
            },
            "EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)",
        )?;
        let mut count: u32 = 0;
        check(
            unsafe {
                let v = coll.vtbl::<IMMDeviceCollectionVtbl>();
                ((*v).get_count)(coll.0, &mut count)
            },
            "IMMDeviceCollection::GetCount",
        )?;
        let mut out = Vec::new();
        for i in 0..count {
            let mut dev = ComPtr::null();
            let hr = unsafe {
                let v = coll.vtbl::<IMMDeviceCollectionVtbl>();
                ((*v).item)(coll.0, i, &mut dev.0)
            };
            if hr < 0 {
                continue;
            }
            if let Some(name) = friendly_name(&dev) {
                out.push((dev, name));
            }
        }
        Ok(out)
    }

    fn endpoint_volume(target: Option<&str>) -> Result<Endpoint> {
        let apt = Apartment::enter();
        let mut enumerator = ComPtr::null();
        check(
            unsafe {
                CoCreateInstance(
                    &CLSID_MM_DEVICE_ENUMERATOR,
                    ptr::null_mut(),
                    CLSCTX_INPROC_SERVER,
                    &IID_IMM_DEVICE_ENUMERATOR,
                    &mut enumerator.0,
                )
            },
            "CoCreateInstance(MMDeviceEnumerator)",
        )?;
        let device = match target {
            None => {
                let mut device = ComPtr::null();
                check(
                    unsafe {
                        let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                        ((*v).get_default_audio_endpoint)(
                            enumerator.0,
                            E_RENDER,
                            E_CONSOLE,
                            &mut device.0,
                        )
                    },
                    "GetDefaultAudioEndpoint(eRender, eConsole)",
                )?;
                device
            }
            Some(name) => match_by_name(render_devices(&enumerator)?, name)?,
        };
        let mut vol = ComPtr::null();
        check(
            unsafe {
                let v = device.vtbl::<IMMDeviceVtbl>();
                ((*v).activate)(
                    device.0,
                    &IID_IAUDIO_ENDPOINT_VOLUME,
                    CLSCTX_ALL,
                    ptr::null_mut(),
                    &mut vol.0,
                )
            },
            "IMMDevice::Activate(IAudioEndpointVolume)",
        )?;
        Ok(Endpoint { vol, _apt: apt })
    }

    pub fn get(target: Option<&str>) -> Result<VolumeState> {
        let ep = endpoint_volume(target)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        let mut scalar: f32 = 0.0;
        check(
            unsafe { ((*v).get_master_volume_level_scalar)(ep.vol.0, &mut scalar) },
            "GetMasterVolumeLevelScalar",
        )?;
        let mut muted: i32 = 0;
        let muted = match unsafe { ((*v).get_mute)(ep.vol.0, &mut muted) } {
            hr if hr >= 0 => muted != 0,
            _ => false, // endpoint without a mute control: never report muted
        };
        // The WASAPI endpoint volume is the shared-mode software volume, so a
        // successfully activated endpoint is by definition writable.
        Ok(VolumeState {
            scalar: scalar.clamp(0.0, 1.0),
            muted,
            adjustable: true,
        })
    }

    pub fn set_volume(target: Option<&str>, scalar: f32) -> Result<()> {
        let ep = endpoint_volume(target)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_master_volume_level_scalar)(ep.vol.0, scalar, ptr::null()) },
            &format!("SetMasterVolumeLevelScalar on {}", label(target)),
        )
    }

    pub fn set_mute(target: Option<&str>, muted: bool) -> Result<()> {
        let ep = endpoint_volume(target)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_mute)(ep.vol.0, i32::from(muted), ptr::null()) },
            &format!("SetMute on {}", label(target)),
        )
    }
}

// ---------------------------------------------------------------- other

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::VolumeState;
    use anyhow::{bail, Result};

    pub fn get(_target: Option<&str>) -> Result<VolumeState> {
        Ok(VolumeState { scalar: 0.0, muted: false, adjustable: false })
    }

    pub fn set_volume(_target: Option<&str>, _scalar: f32) -> Result<()> {
        bail!("output volume control is not implemented on this platform");
    }

    pub fn set_mute(_target: Option<&str>, _muted: bool) -> Result<()> {
        bail!("output mute control is not implemented on this platform");
    }
}
