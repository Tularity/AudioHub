//! Read-only CoreAudio probe for `device_facts` (design §3.4).
//!
//! Property reads on AudioObjects only: no IOProc, no stream start, nothing
//! that needs microphone permission. Each `AudioObjectGetPropertyData` call
//! here is a metadata query, the same kind `devlat.rs` and
//! `output_capabilities_macos.rs` already make.
//!
//! Kept separate from `devlat.rs` on purpose (see that module's note on why
//! the CoreAudio layers are not merged): the selectors here — plug-in device
//! lists, aggregate sub-devices, taps, related devices — are this gate's alone.
//!
//! ## How the owning plug-in is found (measured, macOS 26.6 x86_64 VM)
//!
//! Neither obvious property works in a client process:
//! - `kAudioObjectPropertyCreator` of a BlackHole 2ch device (an
//!   AudioServerPlugIn driver) is "com.apple.audio.CoreAudio" — the host,
//!   not the driver;
//! - `kAudioObjectPropertyOwner` of every device is the system object (1),
//!   never a plug-in object.
//!
//! What does work: `kAudioHardwarePropertyPlugInList` on the system object,
//! then each plug-in's `kAudioPlugInPropertyDeviceList`. BlackHole's device
//! is listed by the plug-in whose `kAudioPlugInPropertyBundleID` is
//! "audio.existential.BlackHole2ch". A HAL-created aggregate (class 'aagg',
//! transport 'grup', Creator com.apple.audio.CoreAudio) is listed by no
//! plug-in; the snapshot records that only when every plug-in that has a
//! device list was read successfully.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use super::{
    Direction, EndpointProbe, MacDeviceSnapshot, RawFactsSnapshot, MAC_MAX_AGGREGATE_DEPTH,
};

type OSStatus = i32;
type AudioObjectID = u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct PropAddr {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
const UNKNOWN_OBJECT: AudioObjectID = 0; // kAudioObjectUnknown
const ELEM_MAIN: u32 = 0; // kAudioObjectPropertyElementMain

const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_INPUT: u32 = fourcc(b"inpt");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");

const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
const SEL_DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
const SEL_PLUGIN_LIST: u32 = fourcc(b"plg#"); // kAudioHardwarePropertyPlugInList
const SEL_PLUGIN_DEVICE_LIST: u32 = fourcc(b"dev#"); // kAudioPlugInPropertyDeviceList
const SEL_PLUGIN_BUNDLE_ID: u32 = fourcc(b"piid"); // kAudioPlugInPropertyBundleID
const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName
const SEL_MANUFACTURER: u32 = fourcc(b"lmak"); // kAudioObjectPropertyManufacturer
const SEL_CLASS: u32 = fourcc(b"clas"); // kAudioObjectPropertyClass
const SEL_CREATOR: u32 = fourcc(b"oplg"); // kAudioObjectPropertyCreator (diagnostic only)
const SEL_UID: u32 = fourcc(b"uid "); // kAudioDevicePropertyDeviceUID
const SEL_TRANSPORT: u32 = fourcc(b"tran"); // kAudioDevicePropertyTransportType
const SEL_STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams
const SEL_TERMINAL_TYPE: u32 = fourcc(b"term"); // kAudioStreamPropertyTerminalType
const SEL_RELATED: u32 = fourcc(b"akin"); // kAudioDevicePropertyRelatedDevices
const SEL_ACTIVE_SUBDEVICES: u32 = fourcc(b"agrp"); // kAudioAggregateDevicePropertyActiveSubDeviceList
/// kAudioAggregateDevicePropertyTapList (macOS 14.2+): a CFArray of tap UID
/// CFStrings — NOT an AudioObjectID array (measured: an 8-byte CFArrayRef).
const SEL_TAP_LIST: u32 = fourcc(b"tap#");

const TT_AGGREGATE: u32 = fourcc(b"grup");

const CF_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectHasProperty(id: AudioObjectID, addr: *const PropAddr) -> u8;
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
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(len: isize, encoding: u32) -> isize;
    fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, encoding: u32) -> u8;
}

fn at(selector: u32, scope: u32) -> PropAddr {
    PropAddr {
        selector,
        scope,
        element: ELEM_MAIN,
    }
}

fn get_u32(obj: AudioObjectID, a: &PropAddr) -> Result<u32, OSStatus> {
    let mut v: u32 = 0;
    let mut sz: u32 = 4;
    let st = unsafe {
        AudioObjectGetPropertyData(obj, a, 0, null(), &mut sz, &mut v as *mut u32 as *mut c_void)
    };
    match (st, sz) {
        (0, 4) => Ok(v),
        (0, _) => Err(-1),
        (e, _) => Err(e),
    }
}

/// A +1 CF object property (released by the caller through [`CfOwned`]).
fn get_cf(obj: AudioObjectID, a: &PropAddr) -> Option<CfOwned> {
    let mut cf: *const c_void = null_mut();
    let mut sz = std::mem::size_of::<*const c_void>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            a,
            0,
            null(),
            &mut sz,
            &mut cf as *mut *const c_void as *mut c_void,
        )
    };
    if st != 0 || cf.is_null() || sz as usize != std::mem::size_of::<*const c_void>() {
        if !cf.is_null() {
            unsafe { CFRelease(cf) };
        }
        return None;
    }
    Some(CfOwned(cf))
}

struct CfOwned(*const c_void);

impl Drop for CfOwned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

fn get_cf_string(obj: AudioObjectID, a: &PropAddr) -> Option<String> {
    let cf = get_cf(obj, a)?;
    unsafe { cf_to_string(cf.0) }.filter(|s| !s.is_empty())
}

unsafe fn cf_to_string(cf: *const c_void) -> Option<String> {
    let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(cf), CF_UTF8);
    if max <= 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u8; max as usize + 1];
    if CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8) == 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    String::from_utf8(buf).ok()
}

/// An `AudioObjectID[]` property. `None` = the read failed (as opposed to an
/// empty, successfully read list).
fn get_id_list(obj: AudioObjectID, a: &PropAddr) -> Option<Vec<AudioObjectID>> {
    let mut bytes: u32 = 0;
    if unsafe { AudioObjectGetPropertyDataSize(obj, a, 0, null(), &mut bytes) } != 0 {
        return None;
    }
    let n = bytes as usize / std::mem::size_of::<AudioObjectID>();
    if n == 0 {
        return Some(Vec::new());
    }
    let mut ids = vec![0u32; n];
    let mut io = bytes;
    let st = unsafe {
        AudioObjectGetPropertyData(obj, a, 0, null(), &mut io, ids.as_mut_ptr() as *mut c_void)
    };
    if st != 0 {
        return None;
    }
    ids.truncate(io as usize / std::mem::size_of::<AudioObjectID>());
    Some(ids)
}

fn has(obj: AudioObjectID, a: &PropAddr) -> bool {
    unsafe { AudioObjectHasProperty(obj, a) != 0 }
}

fn scope_of(direction: Direction) -> u32 {
    match direction {
        Direction::Input => SCOPE_INPUT,
        Direction::Output => SCOPE_OUTPUT,
    }
}

/// Which HAL plug-in lists which devices, read once per probe.
struct PluginMap {
    plugins: Vec<(Option<String>, Vec<AudioObjectID>)>,
    /// Every plug-in that has a device list was read successfully. Without
    /// that, "listed by no plug-in" cannot be concluded.
    complete: bool,
}

impl PluginMap {
    fn read() -> PluginMap {
        let Some(ids) = get_id_list(SYSTEM_OBJECT, &at(SEL_PLUGIN_LIST, SCOPE_GLOBAL)) else {
            return PluginMap {
                plugins: Vec::new(),
                complete: false,
            };
        };
        let mut complete = true;
        let mut plugins = Vec::with_capacity(ids.len());
        for p in ids {
            let list_addr = at(SEL_PLUGIN_DEVICE_LIST, SCOPE_GLOBAL);
            // The built-in host plug-in (com.apple.audio.CoreAudio) has no
            // device list property at all; that is not a failed read.
            if !has(p, &list_addr) {
                continue;
            }
            match get_id_list(p, &list_addr) {
                Some(devices) => plugins.push((
                    get_cf_string(p, &at(SEL_PLUGIN_BUNDLE_ID, SCOPE_GLOBAL)),
                    devices,
                )),
                None => complete = false,
            }
        }
        PluginMap { plugins, complete }
    }

    /// `(bundle id of the listing plug-in, listed by none and the map is complete)`.
    /// A listing plug-in without a readable bundle ID yields `(None, false)`.
    fn owner_of(&self, dev: AudioObjectID) -> (Option<String>, bool) {
        for (bundle, devices) in &self.plugins {
            if devices.contains(&dev) {
                return (bundle.clone(), false);
            }
        }
        (None, self.complete)
    }
}

/// Terminal type of the first stream in `direction`, if any.
fn terminal_type(dev: AudioObjectID, direction: Direction) -> Option<u32> {
    let streams = get_id_list(dev, &at(SEL_STREAMS, scope_of(direction)))?;
    let first = *streams.first()?;
    get_u32(first, &at(SEL_TERMINAL_TYPE, SCOPE_GLOBAL)).ok()
}

/// Number of taps on an aggregate. `None` = the property exists but could
/// not be read; `Some(0)` also when the OS predates process taps.
fn tap_count(dev: AudioObjectID) -> Option<u32> {
    let addr = at(SEL_TAP_LIST, SCOPE_GLOBAL);
    if !has(dev, &addr) {
        return Some(0);
    }
    let array = get_cf(dev, &addr)?;
    let n = unsafe { CFArrayGetCount(array.0) };
    u32::try_from(n).ok()
}

fn snapshot(
    dev: AudioObjectID,
    direction: Direction,
    plugins: &PluginMap,
    depth: usize,
    visited: &mut Vec<AudioObjectID>,
) -> MacDeviceSnapshot {
    visited.push(dev);
    let transport_type = get_u32(dev, &at(SEL_TRANSPORT, SCOPE_GLOBAL)).ok();
    let (plugin_bundle_id, unlisted_by_plugins) = plugins.owner_of(dev);
    let mut snap = MacDeviceSnapshot {
        direction,
        object_id: dev,
        uid: get_cf_string(dev, &at(SEL_UID, SCOPE_GLOBAL)),
        name: get_cf_string(dev, &at(SEL_NAME, SCOPE_GLOBAL)),
        manufacturer: get_cf_string(dev, &at(SEL_MANUFACTURER, SCOPE_GLOBAL)),
        transport_type,
        class_id: get_u32(dev, &at(SEL_CLASS, SCOPE_GLOBAL)).ok(),
        plugin_bundle_id,
        unlisted_by_plugins,
        creator_bundle_id: get_cf_string(dev, &at(SEL_CREATOR, SCOPE_GLOBAL)),
        terminal_type: terminal_type(dev, direction),
        related_devices: get_id_list(dev, &at(SEL_RELATED, SCOPE_GLOBAL)).unwrap_or_default(),
        sub_devices: Vec::new(),
        sub_devices_complete: false,
        tap_count: 0,
    };
    if transport_type != Some(TT_AGGREGATE) {
        return snap;
    }
    // Aggregate: every active member is read; an unreadable tap list, any
    // member read failure, a cycle or the depth bound leaves
    // `sub_devices_complete` false (fail closed).
    let Some(taps) = tap_count(dev) else {
        return snap;
    };
    snap.tap_count = taps;
    if depth >= MAC_MAX_AGGREGATE_DEPTH {
        return snap;
    }
    let Some(members) = get_id_list(dev, &at(SEL_ACTIVE_SUBDEVICES, SCOPE_GLOBAL)) else {
        return snap;
    };
    let mut complete = true;
    for member in members {
        if member == UNKNOWN_OBJECT || visited.contains(&member) {
            complete = false;
            continue;
        }
        snap.sub_devices
            .push(snapshot(member, direction, plugins, depth + 1, visited));
    }
    snap.sub_devices_complete = complete;
    snap
}

fn probe_direction(direction: Direction, plugins: &PluginMap) -> EndpointProbe<MacDeviceSnapshot> {
    let sel = match direction {
        Direction::Input => SEL_DEFAULT_INPUT,
        Direction::Output => SEL_DEFAULT_OUTPUT,
    };
    match get_u32(SYSTEM_OBJECT, &at(sel, SCOPE_GLOBAL)) {
        Ok(UNKNOWN_OBJECT) => EndpointProbe::Absent,
        Ok(dev) => EndpointProbe::Present {
            snapshot: snapshot(dev, direction, plugins, 0, &mut Vec::new()),
        },
        Err(st) => EndpointProbe::Failed {
            error: format!("default {direction:?} device query failed: OSStatus {st}"),
        },
    }
}

pub(super) fn probe_raw() -> RawFactsSnapshot {
    let plugins = PluginMap::read();
    RawFactsSnapshot::Macos {
        input: probe_direction(Direction::Input, &plugins),
        output: probe_direction(Direction::Output, &plugins),
    }
}

/// Every device on the host, for the explicit VM probe test only.
#[cfg(test)]
pub(super) fn probe_all_devices_for_test() -> Vec<MacDeviceSnapshot> {
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    let plugins = PluginMap::read();
    get_id_list(SYSTEM_OBJECT, &at(SEL_DEVICES, SCOPE_GLOBAL))
        .unwrap_or_default()
        .into_iter()
        .map(|d| snapshot(d, Direction::Output, &plugins, 0, &mut Vec::new()))
        .collect()
}
