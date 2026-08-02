use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SupportedStreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// One system audio device as this process sees it.
///
/// `uid` (kAudioDevicePropertyDeviceUID) is the opaque, stable handle a device
/// keeps across renames and reboots; `id` (AudioObjectID) is only meaningful
/// inside this process' lifetime and is never recycled by our own driver, so a
/// change of `id` under an unchanged `uid` is a genuine re-creation. Devices
/// whose NAME is generated at runtime — one pair per paired peer — can only be
/// addressed by `uid`, which is why it is reported at all.
///
/// Both are `None` off macOS: WASAPI endpoints have identifiers of their own,
/// but nothing addresses devices by them yet and inventing one from the
/// friendly name would be a lie a script could not detect.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceEntry {
    pub name: String,
    pub uid: Option<String>,
    pub id: Option<u32>,
    pub is_input: bool,
    pub is_output: bool,
}

// Gated with the watcher that is its only caller: only macOS observes the
// device list today, and an ungated copy would just be dead code elsewhere.
#[cfg(target_os = "macos")]
impl DeviceEntry {
    /// Identity for diffing two snapshots. Deliberately excludes the name: a
    /// rename is an in-place update of one device, not a removal plus an
    /// addition, and reporting it as churn would make the watcher's whole
    /// "zero events across a daemon restart" assertion meaningless.
    fn same_device(&self, other: &DeviceEntry) -> bool {
        self.id == other.id && self.uid == other.uid
    }
}

#[derive(Debug, serde::Serialize)]
pub struct DevicesReport {
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub output_config: Option<String>,
    pub input_config: Option<String>,
    /// Appended AFTER the four keys above, which regression scripts parse: the
    /// existing fields serialize byte-for-byte as before.
    pub devices: Vec<DeviceEntry>,
}

fn describe(cfg: &SupportedStreamConfig) -> String {
    format!(
        "{}Hz {}ch {}",
        cfg.sample_rate().0,
        cfg.channels(),
        format!("{:?}", cfg.sample_format()).to_lowercase()
    )
}

pub fn default_devices_report() -> Result<DevicesReport> {
    let host = cpal::default_host();
    let out_dev = host.default_output_device();
    let in_dev = host.default_input_device();
    let default_output = out_dev.as_ref().and_then(|d| d.name().ok());
    let default_input = in_dev.as_ref().and_then(|d| d.name().ok());
    let output_config = out_dev
        .as_ref()
        .and_then(|d| d.default_output_config().ok())
        .map(|c| describe(&c));
    // On coreaudio BOTH default_input_config() and supported_input_configs()
    // build an input AudioUnit (AudioDeviceCreateIOProcID), which blocks behind
    // the mic-permission (TCC) machinery when consent is absent. A listing
    // probe must never touch the input unit: report the name only.
    let input_config = None;
    Ok(DevicesReport {
        default_output,
        default_input,
        output_config,
        input_config,
        // Property reads only — same permission-free contract as the names.
        devices: list_devices_detailed(),
    })
}

// ----------------------------------------------------------- named devices

/// Which side of a device a listing, a lookup or a watcher is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceKind {
    Input,
    Output,
}

impl DeviceKind {
    fn word(self) -> &'static str {
        match self {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        }
    }
}

/// Names of every device that can play audio, deduplicated, enumeration order.
pub fn list_output_devices() -> Vec<String> {
    list_names(DeviceKind::Output)
}

/// Names of every device that can capture audio, deduplicated, enumeration
/// order. Listing never opens a device, so it never trips a permission prompt.
pub fn list_input_devices() -> Vec<String> {
    list_names(DeviceKind::Input)
}

/// Every device with its UID and AudioObjectID, raw enumeration order,
/// duplicates kept. Like the name listings this only reads properties, so it
/// never opens a unit and never trips a permission prompt.
pub fn list_devices_detailed() -> Vec<DeviceEntry> {
    devices::list_detailed()
}

/// The name behind a UID, without opening anything. Exists so a probe can
/// report WHICH device a UID actually addressed: the name is the only part a
/// human can check, and with runtime-generated names it is not knowable in
/// advance.
pub fn device_name_for_uid(kind: DeviceKind, uid: &str) -> Result<String> {
    devices::name_for_uid(kind, uid)
}

/// THE DUPLICATE-NAME RULE: presentation deduplicates, resolution does not.
/// Callers (UI dropdowns, `daemon.status`) get this collapsed list, while
/// `resolve_name` always runs over the RAW enumeration. Two distinct cards
/// sharing one name therefore stay visible to the ambiguity check and are
/// rejected, instead of collapsing into a single entry that would silently
/// resolve to whichever of them cpal happened to enumerate first.
fn list_names(kind: DeviceKind) -> Vec<String> {
    dedup_in_order(devices::list_all(kind))
}

fn dedup_in_order(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Case-insensitive exact name first, then a case-insensitive PREFIX match, so
/// `BlackHole` resolves `BlackHole 2ch` and `blackhole 2ch` still takes the
/// fast path instead of being punished as an ambiguous prefix of a longer
/// sibling. An exact hit wins over every prefix hit. Anything else — several
/// prefix hits, or several devices carrying the one name — is an error listing
/// the candidates: picking one would be a coin flip on which card audio lands.
/// `names` must be the raw list (duplicates included); see `list_names`.
fn resolve_name(names: &[String], query: &str, kind: DeviceKind) -> Result<String> {
    let q = query.trim();
    if q.is_empty() {
        bail!("empty {} device name", kind.word());
    }
    let ql = q.to_lowercase();
    let exact: Vec<&String> = names.iter().filter(|n| n.to_lowercase() == ql).collect();
    let hits = if exact.is_empty() {
        names
            .iter()
            .filter(|n| n.to_lowercase().starts_with(&ql))
            .collect::<Vec<&String>>()
    } else {
        exact
    };
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!(
            "no {} device matches {q:?}; available: [{}]",
            kind.word(),
            dedup_in_order(names.iter().cloned()).join(", ")
        ),
        n if hits.iter().all(|h| h.as_str() == hits[0].as_str()) => bail!(
            "{} device name {q:?} is ambiguous: {n} devices are named {:?}; \
             rename one of them in the system settings",
            kind.word(),
            hits[0]
        ),
        _ => bail!(
            "{} device name {q:?} is ambiguous; candidates: [{}]",
            kind.word(),
            hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Resolves `query` against the devices of `kind` and hands back the cpal
/// handle. There is deliberately no fallback to the default device: a bridge
/// that quietly played into the speakers instead of the virtual card would be
/// worse than a failed session.
fn find_device(kind: DeviceKind, query: &str) -> Result<(cpal::Device, String)> {
    let names = devices::list_all(kind);
    let resolved = resolve_name(&names, query, kind)?;
    let dev = devices::find(kind, &resolved)
        .ok_or_else(|| anyhow!("audio device {resolved:?} vanished between listing and open"))?;
    Ok((dev, resolved))
}

/// Same contract as `find_device` but keyed on the device UID, matched EXACTLY
/// and case-sensitively: a UID is an opaque identifier, not a display string,
/// so the prefix and case leniency that makes names typeable would here only
/// invent matches that the system itself would not make. No fallback to the
/// default device either — a mistyped UID must say so.
fn find_device_by_uid(kind: DeviceKind, uid: &str) -> Result<(cpal::Device, String)> {
    devices::find_by_uid(kind, uid)
}

/// macOS listing goes straight to CoreAudio properties. cpal's own
/// `input_devices()` filter builds an *input* AudioUnit per device, which is
/// what blocks behind the microphone TCC machinery — the same reason
/// `default_devices_report` refuses to fill `input_config`. A listing must stay
/// permission-free.
#[cfg(target_os = "macos")]
mod devices {
    use super::{DeviceEntry, DeviceKind};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{addr_of, null, null_mut};

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropAddr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[repr(C)]
    struct AudioBuffer {
        num_channels: u32,
        byte_size: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct AudioBufferList {
        num_buffers: u32,
        first: AudioBuffer,
    }

    const SYSTEM_OBJECT: AudioObjectID = 1;
    const ELEM_MAIN: u32 = 0;

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const SEL_STREAM_CONFIG: u32 = fourcc(b"slay"); // kAudioDevicePropertyStreamConfiguration
    const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioDevicePropertyDeviceNameCFString
    const SEL_UID: u32 = fourcc(b"uid "); // kAudioDevicePropertyDeviceUID
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");

    const CF_UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
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
        fn CFStringGetLength(s: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(len: isize, enc: u32) -> isize;
        fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, enc: u32) -> u8;
    }

    fn at(selector: u32, scope: u32) -> PropAddr {
        PropAddr { selector, scope, element: ELEM_MAIN }
    }

    fn prop_size(dev: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut sz: u32 = 0;
        let st = unsafe { AudioObjectGetPropertyDataSize(dev, a, 0, null(), &mut sz) };
        (st == 0).then_some(sz)
    }

    fn device_ids() -> Vec<AudioObjectID> {
        let a = at(SEL_DEVICES, SCOPE_GLOBAL);
        let Some(sz) = prop_size(SYSTEM_OBJECT, &a) else {
            return Vec::new();
        };
        let n = sz as usize / size_of::<AudioObjectID>();
        if n == 0 {
            return Vec::new();
        }
        let mut ids = vec![0u32; n];
        let mut io = sz;
        let st = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                null(),
                &mut io,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return Vec::new();
        }
        ids.truncate(io as usize / size_of::<AudioObjectID>());
        ids
    }

    /// Total channel count the device exposes in `scope`; 0 means it does not
    /// do that direction at all (this is exactly how cpal decides the same).
    fn scope_channels(dev: AudioObjectID, scope: u32) -> u32 {
        let a = at(SEL_STREAM_CONFIG, scope);
        let Some(sz) = prop_size(dev, &a) else {
            return 0;
        };
        if (sz as usize) < size_of::<u32>() {
            return 0;
        }
        // u64 backing: AudioBufferList is 8-aligned because AudioBuffer holds a
        // pointer, so `mBuffers` starts at offset 8, not 4.
        let mut backing = vec![0u64; (sz as usize + 7) / 8];
        let mut io = sz;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                &a,
                0,
                null(),
                &mut io,
                backing.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return 0;
        }
        let list = backing.as_ptr() as *const AudioBufferList;
        let n = unsafe { (*list).num_buffers } as usize;
        let first = unsafe { addr_of!((*list).first) };
        let offset = first as usize - list as usize;
        if offset + n * size_of::<AudioBuffer>() > io as usize {
            return 0;
        }
        (0..n)
            .map(|i| unsafe { (*first.add(i)).num_channels })
            .sum()
    }

    /// The CFStringRef comes back owned (AudioObject "get" of a CF object hands
    /// the caller a +1 reference), so it must be released here.
    fn device_name(dev: AudioObjectID) -> Option<String> {
        // Scope Output first: that is what cpal's Device::name() asks for, and
        // the two lists have to agree or find_device() could not match.
        for scope in [SCOPE_OUTPUT, SCOPE_GLOBAL] {
            let a = at(SEL_NAME, scope);
            let mut cf: *const c_void = null_mut();
            let mut io = size_of::<*const c_void>() as u32;
            let st = unsafe {
                AudioObjectGetPropertyData(
                    dev,
                    &a,
                    0,
                    null(),
                    &mut io,
                    &mut cf as *mut *const c_void as *mut c_void,
                )
            };
            if st != 0 || cf.is_null() {
                continue;
            }
            let s = unsafe { cf_to_string(cf) };
            unsafe { CFRelease(cf) };
            if let Some(s) = s {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }

    unsafe fn cf_to_string(cf: *const c_void) -> Option<String> {
        let len = CFStringGetLength(cf);
        let max = CFStringGetMaximumSizeForEncoding(len, CF_UTF8);
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

    /// The device UID (kAudioDevicePropertyDeviceUID). Same ownership rule as
    /// `device_name`: a "get" of a CF object hands the caller a +1 reference,
    /// so it is released here.
    fn device_uid(dev: AudioObjectID) -> Option<String> {
        let a = at(SEL_UID, SCOPE_GLOBAL);
        let mut cf: *const c_void = null_mut();
        let mut io = size_of::<*const c_void>() as u32;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                &a,
                0,
                null(),
                &mut io,
                &mut cf as *mut *const c_void as *mut c_void,
            )
        };
        if st != 0 || cf.is_null() {
            return None;
        }
        let s = unsafe { cf_to_string(cf) };
        unsafe { CFRelease(cf) };
        s.filter(|s| !s.is_empty())
    }

    fn scope_of(kind: DeviceKind) -> u32 {
        match kind {
            DeviceKind::Input => SCOPE_INPUT,
            DeviceKind::Output => SCOPE_OUTPUT,
        }
    }

    /// Every device, in raw enumeration order. Unlike `list_all` this keeps
    /// devices that have no usable name (they are still addressable by UID) and
    /// devices that do neither direction (both flags false) — a listing that
    /// hid them would be lying about what the system contains.
    pub fn list_detailed() -> Vec<DeviceEntry> {
        device_ids()
            .into_iter()
            .map(|id| DeviceEntry {
                name: device_name(id).unwrap_or_default(),
                uid: device_uid(id),
                id: Some(id),
                // "has streams in that scope" is exactly how cpal decides the
                // same question, so the two views cannot disagree.
                is_input: scope_channels(id, SCOPE_INPUT) > 0,
                is_output: scope_channels(id, SCOPE_OUTPUT) > 0,
            })
            .collect()
    }

    /// Finds `uid` inside an already-taken snapshot and returns its INDEX plus
    /// the device name. The index is what lets `find_by_uid` reach the matching
    /// cpal handle; taking the snapshot outside is what lets it prove the
    /// snapshot did not move underneath.
    fn locate_uid(ids: &[AudioObjectID], kind: DeviceKind, uid: &str) -> Result<(usize, String)> {
        let scope = scope_of(kind);
        let Some(i) = ids.iter().position(|&d| device_uid(d).as_deref() == Some(uid)) else {
            bail!(
                "no {} device matches UID {uid:?}; available: [{}]",
                kind.word(),
                uid_catalog(ids, scope)
            );
        };
        let name = device_name(ids[i]).unwrap_or_default();
        if scope_channels(ids[i], scope) == 0 {
            bail!(
                "device with UID {uid:?} ({name:?}) has no {} streams",
                kind.word()
            );
        }
        Ok((i, name))
    }

    /// What a mistyped UID gets told. Names ride along because a UID alone is
    /// unrecognisable to a human trying to spot their own typo.
    fn uid_catalog(ids: &[AudioObjectID], scope: u32) -> String {
        ids.iter()
            .filter(|&&d| scope_channels(d, scope) > 0)
            .map(|&d| match (device_uid(d), device_name(d)) {
                (Some(u), Some(n)) => format!("{u} ({n})"),
                (Some(u), None) => u,
                (None, Some(n)) => format!("<no uid> ({n})"),
                (None, None) => "<no uid>".to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn name_for_uid(kind: DeviceKind, uid: &str) -> Result<String> {
        let ids = device_ids();
        locate_uid(&ids, kind, uid).map(|(_, name)| name)
    }

    /// cpal's coreaudio `Devices` iterator is an unfiltered, order-preserving
    /// map of the very kAudioHardwarePropertyDevices array `device_ids()`
    /// reads, so index i of one is index i of the other — but only within a
    /// consistent snapshot. Hence the re-read: if the device list moved while
    /// we were enumerating, the whole attempt is thrown away rather than
    /// indexing into a shifted list. The name cross-check is the belt to that
    /// braces; opening the WRONG card is the single outcome a UID lookup must
    /// never produce, and with per-peer devices it would mean audio landing on
    /// somebody else's machine.
    pub fn find_by_uid(kind: DeviceKind, uid: &str) -> Result<(cpal::Device, String)> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        for _ in 0..3 {
            let ids = device_ids();
            let mut handles: Vec<cpal::Device> = match host.devices() {
                Ok(it) => it.collect(),
                Err(e) => bail!("enumerate audio devices: {e}"),
            };
            if handles.len() != ids.len() || device_ids() != ids {
                continue; // the list changed mid-read; indices are meaningless
            }
            let (i, name) = locate_uid(&ids, kind, uid)?;
            let got = handles[i].name().unwrap_or_default();
            if got != name {
                bail!(
                    "UID {uid:?} is AudioObjectID {} ({name:?}) but cpal's device #{i} is {got:?}; \
                     refusing to open a device that may not be the one asked for",
                    ids[i]
                );
            }
            return Ok((handles.swap_remove(i), name));
        }
        bail!("the audio device list kept changing while resolving UID {uid:?}")
    }

    /// Raw enumeration order, duplicates kept: an unnamed device can never be
    /// typed so it is dropped, but two cards with the same name must both stay
    /// visible to the ambiguity check.
    pub fn list_all(kind: DeviceKind) -> Vec<String> {
        let scope = scope_of(kind);
        device_ids()
            .into_iter()
            .filter(|&d| scope_channels(d, scope) > 0)
            .filter_map(device_name)
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// A coreaudio device carries both directions, so the direction was already
    /// settled by `list`; only the name has to match here.
    pub fn find(_kind: DeviceKind, name: &str) -> Option<cpal::Device> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .devices()
            .ok()?
            .find(|d| d.name().map_or(false, |n| n == name))
    }
}

/// WASAPI answers the direction question from the endpoint's data flow alone
/// (cpal overrides `supports_input`/`supports_output` to do exactly that), so
/// no device is opened and no permission is involved.
#[cfg(not(target_os = "macos"))]
mod devices {
    use super::{DeviceEntry, DeviceKind};
    use anyhow::{bail, Result};
    use cpal::traits::{DeviceTrait, HostTrait};

    /// A WASAPI render endpoint and a capture endpoint are two different
    /// objects even when they share one friendly name, so they are listed as
    /// two entries rather than merged into one duplex device. `uid`/`id` stay
    /// absent: see `DeviceEntry`.
    pub fn list_detailed() -> Vec<DeviceEntry> {
        let mut out = Vec::new();
        for (kind, is_input) in [(DeviceKind::Input, true), (DeviceKind::Output, false)] {
            for name in list_all(kind) {
                out.push(DeviceEntry {
                    name,
                    uid: None,
                    id: None,
                    is_input,
                    is_output: !is_input,
                });
            }
        }
        out
    }

    /// Refuses rather than silently ignoring the request: a script that asked
    /// for one specific device must not be handed a different one.
    pub fn name_for_uid(_kind: DeviceKind, _uid: &str) -> Result<String> {
        bail!("addressing an audio device by UID is only supported on macOS");
    }

    pub fn find_by_uid(_kind: DeviceKind, _uid: &str) -> Result<(cpal::Device, String)> {
        bail!("addressing an audio device by UID is only supported on macOS");
    }

    /// Raw enumeration order, duplicates kept: two WASAPI endpoints can carry
    /// the same friendly name and both must stay visible to the ambiguity
    /// check. Unnamed endpoints can never be typed, so they are dropped.
    pub fn list_all(kind: DeviceKind) -> Vec<String> {
        let host = cpal::default_host();
        let names: Vec<String> = match kind {
            DeviceKind::Input => host
                .input_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default(),
            DeviceKind::Output => host
                .output_devices()
                .map(|it| it.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default(),
        };
        names.into_iter().filter(|n| !n.is_empty()).collect()
    }

    /// Searched inside the direction the caller asked for: a WASAPI render and
    /// capture endpoint can carry the same friendly name, and picking the wrong
    /// flow would hand back a device that cannot do the job.
    pub fn find(kind: DeviceKind, name: &str) -> Option<cpal::Device> {
        let host = cpal::default_host();
        let hit = |d: &cpal::Device| d.name().map_or(false, |n| n == name);
        match kind {
            DeviceKind::Input => host.input_devices().ok()?.find(hit),
            DeviceKind::Output => host.output_devices().ok()?.find(hit),
        }
    }
}

/// Stateful naive linear resampler (carries phase + last sample across chunks).
struct Resampler {
    step: f64, // input samples per output sample
    phase: f64,
    last: f32,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Resampler {
            step: src_rate as f64 / dst_rate as f64,
            phase: 0.0,
            last: 0.0,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;
        let mut p = self.phase;
        // position p: 0.0 == previous chunk's last sample, 1.0 == input[0]
        while p < len {
            let i = p.floor() as usize;
            let frac = (p - i as f64) as f32;
            let s0 = if i == 0 { self.last } else { input[i - 1] };
            let s1 = input[i];
            out.push(s0 + (s1 - s0) * frac);
            p += self.step;
        }
        self.phase = p - len;
        self.last = *input.last().unwrap();
    }
}

fn resample_all(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate {
        return samples.to_vec();
    }
    let mut rs = Resampler::new(src_rate, dst_rate);
    let mut out = Vec::with_capacity(samples.len() * dst_rate as usize / src_rate as usize + 8);
    rs.process(samples, &mut out);
    out
}

// ==================================================== 跨时钟速率伺服（play_ring）
//
// # 这一段治的是什么病
//
// `play_ring` 是全链路上**唯一真正跨时钟**的一级，也是治法 A 落地之后唯一还在
// **无界积累**的一级：
//
// - **写侧**：`mixer_loop` 每 10 ms 本地单调时钟推 480 个样本 ⇒ 严格 48000
//   样本/本地秒。它的节拍源是 mac 发来的媒体流经 JB 定拍后的本地 tick。
// - **读侧**：cpal/WASAPI 输出回调按**声卡晶振**取样本 ⇒ 48000·(1+ε) 样本/物理秒。
//
// 两个独立振荡器，ε 不可能为零。ε = 50 ppm ⇒ 2.4 帧/秒 ⇒ 180 ms/小时 ⇒
// 1.000 秒的环约 **5.4 小时**灌满（10 ppm → 27 h，100 ppm → 2.7 h）。
// 更糟的是这个环是 **drop-newest**：饱和之后丢的是**最新**的音频，听感是
// 「迟到 + 周期性断续」，而不是「恒定迟到但连续」。
//
// mac 侧那套「同时钟所以不漂」的论证在这里**不成立**——那边两侧同为
// `mach_absolute_time`，这里两侧是 mac 的时基和 Windows 声卡的晶振。
//
// # 结构：一个 DLL，一个执行器
//
// 执行器是 `AudioTx` 自己的重采样器比率。`push()` 是 `play_ring` 的**唯一**写点，
// 所以把环路整个塞进 `AudioTx` 里，`mixer_loop` 的调用点一个字都不用改。
//
// 先例：PipeWire `spa/plugins/alsa/alsa-pcm.c`（`err = delay − target` 喂
// `spa_dll`，`corr` 去改 `rate_match->rate`）、zita-ajbridge/njbridge
// （同构控制律 + `set_rratio`）。控制律本身是 Adriaensen 的公开论文，
// `spa_dll` 是 MIT，**没有链接 `zita-resampler`（GPL-3）**。

/// PipeWire `spa_dll` 的移植（`spa/include/spa/utils/dll.h`，MIT）。
///
/// 契约：输入 `err` 单位是**样本**，输出是围绕 1.0 的**相对速率修正因子**。
///
/// `w0 = 1 − exp(−20w)` 是前置一阶平滑器，转角在 20×环路带宽：只滤测量噪声，
/// 不动环路动力学。我们的深度读数带着「声卡按块取、我们按 tick 读」的锯齿
/// 量化噪声，不先滤一下会直接灌进积分器。
///
/// `w1 = w·1.5/period` 里的 `period` 会与 `w = 2π·bw·period/rate` 的 period
/// 约掉，所以稳态比例增益只有 `3π·bw/rate`——**但 `w0`/`w2` 要的是真实的更新
/// 间隔**，因此 `period/rate` 必须等于两次 `update()` 之间的秒数，不能乱填。
#[derive(Debug, Clone)]
pub struct Dll {
    bw: f64,
    period: u32,
    rate: u32,
    z1: f64,
    z2: f64,
    z3: f64,
    w0: f64,
    w1: f64,
    w2: f64,
}

impl Dll {
    /// PipeWire `SPA_DLL_BW_MAX`：捕获/重同步之后用它快速锁定（ζ=0.75，τ≈1.7 s）。
    pub const BW_MAX: f64 = 0.128;
    /// PipeWire `SPA_DLL_BW_MIN`：稳态用它，τ≈13 s，把测量噪声压到最低。
    pub const BW_MIN: f64 = 0.016;

    pub fn new(bw: f64, period: u32, rate: u32) -> Dll {
        let mut d = Dll {
            bw: 0.0,
            period: 0,
            rate: 0,
            z1: 0.0,
            z2: 0.0,
            z3: 0.0,
            w0: 0.0,
            w1: 0.0,
            w2: 0.0,
        };
        d.set_bw(bw, period, rate);
        d
    }

    pub fn set_bw(&mut self, bw: f64, period: u32, rate: u32) {
        let period = period.max(1);
        let rate = rate.max(1);
        let w = 2.0 * std::f64::consts::PI * bw * period as f64 / rate as f64;
        self.w0 = 1.0 - (-20.0 * w).exp();
        self.w1 = w * 1.5 / period as f64; // k=1.5 ⇒ ζ=0.75（PipeWire 的整定）
        self.w2 = w / 1.5;
        self.bw = bw;
        self.period = period;
        self.rate = rate;
    }

    pub fn bw(&self) -> f64 {
        self.bw
    }

    pub fn period(&self) -> u32 {
        self.period
    }

    /// 清掉三个积分器，保留带宽整定。**跳变/重同步之后必须调**——
    /// PipeWire (`node-driver.c:487–494`) 与 PulseAudio (`fast_adjust` 之后
    /// `return`) 两个独立实现都在粗调之后强制复位细调状态，否则积分器残留
    /// 会在跳变后继续输出错误修正。
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
        self.z3 = 0.0;
    }

    pub fn update(&mut self, err: f64) -> f64 {
        self.z1 += self.w0 * (self.w1 * err - self.z1);
        self.z2 += self.w0 * (self.z1 - self.z2);
        self.z3 += self.w2 * self.z2;
        1.0 - (self.z2 + self.z3)
    }
}

/// 变比率重采样器：4 点 Catmull-Rom（三次 Hermite）。
///
/// # 为什么不是现成的 `dsp::LinearResampler`
///
/// 机制上它**够用**：相位累加器 + 跨块携带的历史样本都在，`step` 就是一个
/// `f64`，改它就是改比率——伺服不需要任何新依赖就能跑起来。真正的问题在质量：
///
/// 线性插值的幅频响应**随小数相位 φ 变化**。φ=0 时是恒等（0 dB），φ=0.5 时
/// 在 f/fs 处衰减 `20·log10(cos(πf/fs))`：10 kHz@48k 是 −2.0 dB，15 kHz 是
/// −5.1 dB。而比率≈1 时 φ **缓慢扫过 [0,1)**，扫一圈需要 `1/|corr−1|` 个样本
/// ——500 ppm 时约 42 ms（≈24 Hz），典型 20 ppm 时约 1 s。于是高频内容会以
/// 亚赫兹到几十赫兹的速率「呼吸」，在镲片/齿音上是能听出来的。
///
/// 这不是线性插值特有的缺陷之外的东西：**任何**比率≈1 的分数延迟器都要扫相位，
/// 区别在于好核的幅度对 φ 几乎不敏感。Catmull-Rom 是 15 行、零依赖、无授权
/// 问题的教科书核，把上面两个数字压到 −0.53 dB / −2.5 dB（见
/// `cubic_beats_linear_on_the_phase_swept_hf_droop`，那条测试是实测不是引用）。
///
/// 再往上就要窗 sinc（`rubato` / `soxr`）——**本轮不引入**：CPU 与依赖的代价
/// 换来的是 20 kHz 附近最后那 2 dB，与「唯一还在无界积累的病灶」不成比例。
/// 若将来听感走查发现 HF 呼吸仍可闻，换核只需要动这个 struct，环路一行不改。
///
/// # 数据结构
///
/// 虚拟输入序列 `v = [hist[0], hist[1], hist[2]] ++ input`。输出位置 `p`
/// 在 `v` 坐标里，`i = floor(p)`，取 4 点窗口 `v[i-1..=i+2]` 在 `v[i]` 与
/// `v[i+1]` 之间插值 ⇒ 必须 `1 ≤ i ≤ v.len()−3`，即 `p ∈ [1, n+1)`。
/// 收尾时 `phase = p − n`，恰好落回 `[1, 1+step)`，不变式自洽。
pub struct VarResampler {
    /// 标称步长 = src_rate / dst_rate（输入样本 / 输出样本）。
    nominal: f64,
    /// 实际步长 = nominal / corr。
    step: f64,
    /// 见 struct 文档：不变式 `phase ≥ 1`。
    phase: f64,
    hist: [f32; 3],
}

impl VarResampler {
    pub fn new(src: u32, dst: u32) -> VarResampler {
        let nominal = src as f64 / dst.max(1) as f64;
        VarResampler {
            nominal,
            step: nominal,
            phase: 1.0,
            hist: [0.0; 3],
        }
    }

    /// 施加 DLL 的修正因子。
    ///
    /// **方向推导（写错就是正反馈）**：`step` 是「每产出一个输出样本消耗多少
    /// 输入样本」。`step` 变大 ⇒ 同样多的输入产出**更少**的输出 ⇒ 写进环里的
    /// 少了 ⇒ 水位下降。所以 `step = nominal / corr`：`corr < 1` ⇒ 步长变大
    /// ⇒ 水位下降。与 `PlayServo` 的 `err = downstream − target` 串起来正好是
    /// 负反馈，见那边的推导。
    pub fn set_correction(&mut self, corr: f64) {
        self.step = self.nominal / corr;
    }

    pub fn step(&self) -> f64 {
        self.step
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len();
        if n == 0 {
            return;
        }
        let hist = self.hist;
        let at = |idx: usize| -> f32 {
            if idx < 3 {
                hist[idx]
            } else {
                input[idx - 3]
            }
        };
        let vlen = n + 3;
        let mut p = self.phase;
        // i = floor(p) 必须满足 i+2 ≤ vlen−1，即 p < (vlen−3)+1 = n+1。
        let limit = (n + 1) as f64;
        while p < limit {
            let i = p as usize; // p ≥ 1 保证 i ≥ 1
            let t = (p - i as f64) as f32;
            let y0 = at(i - 1);
            let y1 = at(i);
            let y2 = at(i + 1);
            let y3 = at(i + 2);
            // Catmull-Rom：t=0 精确返回 y1（所以 step==1 且 phase 整数时是
            // 逐样本无损直通，只差一个固定的两样本延迟）。
            let a0 = y1;
            let a1 = 0.5 * (y2 - y0);
            let a2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
            let a3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
            out.push(((a3 * t + a2) * t + a1) * t + a0);
            p += self.step;
        }
        // 新历史 = v 的最后三个。必须在改 self.hist 之前全部取出来。
        let h = [at(vlen - 3), at(vlen - 2), at(vlen - 1)];
        self.hist = h;
        // p 的坐标原点右移 n 个输入样本。`max(1.0)` 只在 n < step 的病态小块
        // 下才生效（我们的块恒为 480），保住 `phase ≥ 1` 不变式。
        self.phase = (p - n as f64).max(1.0);
    }
}

/// 声卡侧发布给伺服的观测量。写者是输出回调（实时线程），读者是
/// `AudioTx::push`（mixer 线程）。
///
/// 全部用 `Relaxed`：这是一个**估计器**，不是同步原语。最坏情况是把上一个
/// 回调的 `consumed` 和这一个回调的时刻凑成一对，误差上界是一个声卡周期，
/// 而 `Dll` 的 `w0` 前置平滑器正是为这一类噪声准备的。用 `SeqCst` 会在实时
/// 回调里加内存栅栏，代价真实、收益为零。
#[derive(Debug)]
pub struct DeviceClock {
    /// `Instant` 不是原子的，所以时刻一律以「相对 base 的纳秒」发布。
    base: Instant,
    /// 声卡累计取走的样本数。
    consumed: AtomicU64,
    /// 最后一次回调的时刻（纳秒 since base）。
    last_cb_ns: AtomicU64,
    /// `playback − callback`：写进去的数据预计多久之后真的被 DAC 播出——
    /// 也就是 Snapcast `stream.cpp:305` 那个 `outputBufferDacTime`。
    dac_lag_ns: AtomicU64,
    /// 近 `BLOCK_WINDOW` 次回调里取走的最大样本数（声卡周期），用来定目标
    /// 水位的下限。
    ///
    /// 取**滑动窗**而不是全程最大：有些宿主开流时会来一次超大的预热回调，
    /// 全程最大会让那一次把目标水位**永久**顶高——正是本项目在治的那种棘轮。
    /// 窗口一满就从当前值重新起算，异常值约 1.3 秒后自然老化掉。
    block_max: AtomicUsize,
    cb_count: AtomicU64,
    /// 环里不够回调取、被补了静音的次数。
    underruns: AtomicU64,
}

impl DeviceClock {
    /// 滑动窗长度（回调次数）。128 × ≈10 ms ≈ 1.3 秒。
    const BLOCK_WINDOW: u64 = 128;

    fn new() -> Arc<DeviceClock> {
        Arc::new(DeviceClock {
            base: Instant::now(),
            consumed: AtomicU64::new(0),
            last_cb_ns: AtomicU64::new(0),
            dac_lag_ns: AtomicU64::new(0),
            block_max: AtomicUsize::new(0),
            cb_count: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
        })
    }

    /// 输出回调每次调用后报一次。`got < wanted` 就是一次欠载。
    fn note_callback(&self, now: Instant, wanted: usize, got: usize, dac_lag: Option<Duration>) {
        self.consumed.fetch_add(got as u64, Ordering::Relaxed);
        self.last_cb_ns.store(
            now.saturating_duration_since(self.base).as_nanos() as u64,
            Ordering::Relaxed,
        );
        if let Some(d) = dac_lag {
            self.dac_lag_ns
                .store(d.as_nanos() as u64, Ordering::Relaxed);
        }
        let n = self.cb_count.fetch_add(1, Ordering::Relaxed);
        if n % Self::BLOCK_WINDOW == 0 || wanted > self.block_max.load(Ordering::Relaxed) {
            self.block_max.store(wanted, Ordering::Relaxed);
        }
        if got < wanted {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn since_last_callback(&self, now: Instant) -> Option<Duration> {
        let last = self.last_cb_ns.load(Ordering::Relaxed);
        if last == 0 {
            return None; // 还没有过一次回调
        }
        let now_ns = now.saturating_duration_since(self.base).as_nanos() as u64;
        Some(Duration::from_nanos(now_ns.saturating_sub(last)))
    }

    fn dac_lag_samples(&self, rate: u32) -> f64 {
        let ns = self.dac_lag_ns.load(Ordering::Relaxed) as f64;
        ns * rate as f64 / 1e9
    }

    fn block_max(&self) -> usize {
        self.block_max.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

/// 速率伺服的全部整定量与状态。单位一律是**设备速率样本**。
pub struct PlayServo {
    dll: Dll,
    enabled: bool,
    /// 目标水位。基线 + 声卡周期一被观测到就抬到 `2·block + 1 tick`。
    target: f64,
    base_target: f64,
    /// 喂进 DLL 之前把 err 钳到这个幅度（PipeWire `max_error`）。
    max_error: f64,
    /// 超过这个幅度不再靠弯速率，直接硬跳（PipeWire `max_resync`）。
    max_resync: f64,
    /// 深端重同步进行中：在丢输入，DLL 冻结。
    resyncing_deep: bool,
    /// 上一次 `set_bw` 用的 period，块长变了要重算。
    tuned_period: u32,
    /// 捕获阶段（开流 / 重同步后）剩余的更新次数，期间用 `BW_MAX` 快锁。
    capture_left: u32,
    corr: f64,
    /// 最近一次的误差（设备率样本）。**每实例**一份——进程级那份是
    /// 「最后写者赢」，多个环同时在跑时读它读到的是别人的数。
    last_err: f64,
    /// **只给测试**：+1.0 是生产符号，−1.0 把误差取反。
    ///
    /// 为什么要在生产结构里留这一格：调研点名「误差符号是落地时最容易写错、
    /// 且错了会直接把水位推到饱和的一处」，并要求「加一条会在符号写反时变红的
    /// 测试」。要让这条测试**真的**盯住生产代码里那个减号，它必须跑
    /// `servo_step` 本身；把控制律另抄一份到测试里去证明，证明的是抄件。
    /// 所以留一个开关，让同一个环路能被两种符号各跑一遍：
    /// `error_sign_is_negative_feedback_flipping_it_diverges` 断言 +1 收敛
    /// **且** −1 发散——把生产那一行改成 `target − downstream`，两条断言同时红。
    error_sign: f64,
}

impl PlayServo {
    /// 速率钳位 **±500 ppm**。
    ///
    /// 依据三条：
    /// 1. Snapcast `stream.cpp:416` 的硬上限就是 500 ppm——同型系统的既有取值。
    /// 2. 音高上它是 `1200·log2(1.0005)` = **0.87 音分**，远低于人耳约 5–10
    ///    音分的辨别阈，弯到顶也不可闻。
    /// 3. 覆盖面够：消费级音频晶振典型 ±50 ppm、劣质件 ±100 ppm，两端相加
    ///    最坏 200 ppm，500 ppm 留了 2.5 倍余量给稳态误差与整定超调。
    ///
    /// 比它更大的误差**不该**用弯速率去追：500 ppm 只能吐 0.5 ms/s，
    /// 一秒的存量要 2000 秒才排得完。那种量级归 `max_resync` 的硬跳管。
    pub const MAX_PPM: f64 = 500.0;

    /// 目标水位基线。播放环写侧是 10 ms 一块，读侧是一个声卡周期一块，
    /// 目标必须同时盖住两者的相位差与调度抖动。构造后一旦观测到真实的声卡
    /// 周期就抬到 `2·block + 480`（≈两个周期 + 一个 tick）。
    const BASE_TARGET_MS: f64 = 30.0;
    /// 目标水位上限：任何自动抬升都不许把延迟推过这条线。
    const MAX_TARGET_MS: f64 = 120.0;
    /// 喂 DLL 之前的误差钳位。
    const MAX_ERROR_MS: f64 = 15.0;
    /// 硬跳阈值。
    const MAX_RESYNC_MS: f64 = 150.0;
    /// 开流 / 重同步之后用 `BW_MAX` 快锁多少个 10 ms 更新（≈4 s，抄 zita 的
    /// `_count == 4 * _ppsec` 切换点）。
    const CAPTURE_UPDATES: u32 = 400;

    fn new(dev_rate: u32, enabled: bool) -> PlayServo {
        let ms = |v: f64| v * dev_rate as f64 / 1000.0;
        let period = (dev_rate / 100).max(1); // 10 ms 的 mixer tick
        PlayServo {
            dll: Dll::new(Dll::BW_MAX, period, dev_rate),
            enabled,
            target: ms(Self::BASE_TARGET_MS),
            base_target: ms(Self::BASE_TARGET_MS),
            max_error: ms(Self::MAX_ERROR_MS),
            max_resync: ms(Self::MAX_RESYNC_MS),
            resyncing_deep: false,
            tuned_period: period,
            capture_left: Self::CAPTURE_UPDATES,
            corr: 1.0,
            last_err: 0.0,
            error_sign: 1.0,
        }
    }

    /// 浅端硬跳的触发线 = `max(target − max_resync, target/2)`。
    ///
    /// 取 `max` 而不是 `min`：`max_resync` 的定义是「超出这个幅度就别弯速率了，
    /// 直接跳」，所以它越小、越该早跳。以本文件的整定（target≈30 ms、
    /// max_resync=150 ms）`target − max_resync` 是负数，永远不触发，实际生效的
    /// 是**半个目标**——1.000 秒的环本来也容不下一个 150 ms 的下溢。
    fn starve_mark(&self) -> f64 {
        (self.target - self.max_resync).max(self.target * 0.5)
    }

    fn rearm_capture(&mut self) {
        self.dll.reset();
        self.capture_left = Self::CAPTURE_UPDATES;
        self.dll
            .set_bw(Dll::BW_MAX, self.tuned_period, self.dll.rate);
        self.corr = 1.0;
    }
}

/// 环路对本块的裁决。
enum ServoAction {
    /// 正常写。
    Go,
    /// 深端硬跳：整块丢掉（drop-newest，但是**有意的、被计数的**）。
    Skip,
    /// 浅端硬跳：先补 N 个静音样本把水位顶回目标，再正常写。
    Pad(usize),
}

/// 伺服的进程级累计读数（IPC `latency_guard.play_servo`）。
///
/// ## 口径警告
///
/// 这是**进程级**聚合，不是每环一份。计数类字段（`updates` / `resync_*` /
/// `clamped` / `stalled`）是所有 `AudioTx` 的和；瞬时类字段（`corr_ppm` /
/// `err_samples` / `target_samples` / `downstream_samples`）是**最后一个写者
/// 赢**。站点播放环之外只有桥接环也在跑伺服，所以在没开桥的常态下这就是站点
/// 环的读数。之所以只能这样：`AudioTx` 是 `mixer_loop` 的**栈上局部**，
/// daemon 侧拿不到它的引用，而 `mixer_loop` 所在的 `engine.rs` 不在本轮的
/// 改动范围内。要做到每环一份，得给 `LivePlayback::start` 加一个 id 参数。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct PlayServoCounters {
    /// 环路更新次数。
    pub updates: u64,
    /// 最近一次的速率修正，ppm（正 = 输出变快 = 水位涨）。
    pub corr_ppm: i64,
    /// 最近一次的误差 `downstream − target`（设备率样本，正 = 偏深）。
    pub err_samples: i64,
    /// 当前目标水位（设备率样本）。
    pub target_samples: u64,
    /// 最近一次实测的执行器下游总缓冲（设备率样本）。
    pub downstream_samples: u64,
    /// 修正打到 ±500 ppm 上限的次数。持续非零 = 失配超出钳位，该查硬件。
    pub clamped: u64,
    /// 硬跳次数（两个方向合计）。
    pub resync_events: u64,
    /// 深端硬跳丢掉的输入样本。
    pub resync_skipped: u64,
    /// 浅端硬跳补进的静音样本。
    pub resync_padded: u64,
    /// 因为声卡回调停摆而跳过的更新次数（读数不可信，宁可不动）。
    pub stalled: u64,
    /// 声卡取不满、被补静音的回调次数。持续增长 = 目标水位定低了。
    pub dev_underruns: u64,
    /// 最近一次的 `outputBufferDacTime`（设备率样本）。恒为 0 = 平台没给。
    pub dac_lag_samples: u64,
}

#[derive(Debug, Default)]
struct ServoCell {
    updates: AtomicU64,
    corr_ppm: AtomicI64,
    err_samples: AtomicI64,
    target_samples: AtomicU64,
    downstream_samples: AtomicU64,
    clamped: AtomicU64,
    resync_events: AtomicU64,
    resync_skipped: AtomicU64,
    resync_padded: AtomicU64,
    stalled: AtomicU64,
    dev_underruns: AtomicU64,
    dac_lag_samples: AtomicU64,
}

static PLAY_SERVO: ServoCell = ServoCell {
    updates: AtomicU64::new(0),
    corr_ppm: AtomicI64::new(0),
    err_samples: AtomicI64::new(0),
    target_samples: AtomicU64::new(0),
    downstream_samples: AtomicU64::new(0),
    clamped: AtomicU64::new(0),
    resync_events: AtomicU64::new(0),
    resync_skipped: AtomicU64::new(0),
    resync_padded: AtomicU64::new(0),
    stalled: AtomicU64::new(0),
    dev_underruns: AtomicU64::new(0),
    dac_lag_samples: AtomicU64::new(0),
};

/// 播放环速率伺服的现场读数（IPC / probe 用）。见 [`PlayServoCounters`] 的口径警告。
pub fn play_servo_counters() -> PlayServoCounters {
    let g = &PLAY_SERVO;
    PlayServoCounters {
        updates: g.updates.load(Ordering::Relaxed),
        corr_ppm: g.corr_ppm.load(Ordering::Relaxed),
        err_samples: g.err_samples.load(Ordering::Relaxed),
        target_samples: g.target_samples.load(Ordering::Relaxed),
        downstream_samples: g.downstream_samples.load(Ordering::Relaxed),
        clamped: g.clamped.load(Ordering::Relaxed),
        resync_events: g.resync_events.load(Ordering::Relaxed),
        resync_skipped: g.resync_skipped.load(Ordering::Relaxed),
        resync_padded: g.resync_padded.load(Ordering::Relaxed),
        stalled: g.stalled.load(Ordering::Relaxed),
        dev_underruns: g.dev_underruns.load(Ordering::Relaxed),
        dac_lag_samples: g.dac_lag_samples.load(Ordering::Relaxed),
    }
}

// ------------------------------------------------------------ stream health

/// What the cpal error callback writes and the owner of the stream reads. cpal
/// reports a fatal stream error and then stops calling the data callback for
/// good, so `dead` is one-way: a dead stream is never revived, only replaced.
/// Without this the death of e.g. an unplugged bridge card was a mere log line
/// and the writer kept pushing into a stream nobody drains — silent forever.
#[derive(Default)]
struct StreamHealth {
    dead: AtomicBool,
    err: Mutex<Option<String>>,
}

impl StreamHealth {
    fn new() -> Arc<StreamHealth> {
        Arc::new(StreamHealth::default())
    }

    /// Called from the platform's stream-error path. Allocating and locking
    /// here is safe precisely because it happens once, when the stream is
    /// already finished. The first error is the diagnosis; later ones are
    /// fallout, so they only get logged.
    fn fail(&self, what: &str, e: &cpal::StreamError) {
        let msg = format!("{what} stream error: {e}");
        eprintln!("[audiohub] {msg}");
        {
            let mut slot = self.err.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(msg);
            }
        }
        // Published last: a reader that observes the death can then take the
        // message that explains it.
        self.dead.store(true, Ordering::Release);
    }

    fn is_alive(&self) -> bool {
        !self.dead.load(Ordering::Acquire)
    }

    fn take_error(&self) -> Option<String> {
        self.err.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

fn err_sink(
    health: Arc<StreamHealth>,
    what: &'static str,
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| health.fail(what, &e)
}

fn build_output_stream_f32(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    supported_format: SampleFormat,
    health: &Arc<StreamHealth>,
    // 多带一个 `&OutputCallbackInfo`：`timestamp().playback − .callback` 是
    // 声卡硬件缓冲的滞后量（Snapcast 的 `outputBufferDacTime`），是速率伺服
    // 误差信号里唯一取不到就只能算 0 的那一项。此前这个参数被 `_` 丢掉了。
    mut fill_mono: impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let mut mono: Vec<f32> = Vec::new();
    match supported_format {
        SampleFormat::I16 => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    mono.resize(frames, 0.0);
                    fill_mono(&mut mono, info);
                    for (frame, &s) in data.chunks_mut(channels).zip(mono.iter()) {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        frame.fill(v);
                    }
                },
                err_sink(Arc::clone(health), "output"),
                None,
            )?;
            Ok(stream)
        }
        // F32 native, or ask for f32 anyway and let the host convert.
        _ => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    mono.resize(frames, 0.0);
                    fill_mono(&mut mono, info);
                    for (frame, &s) in data.chunks_mut(channels).zip(mono.iter()) {
                        frame.fill(s);
                    }
                },
                err_sink(Arc::clone(health), "output"),
                None,
            )?;
            Ok(stream)
        }
    }
}

pub fn play_samples_blocking(samples: &[f32], src_rate: u32) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let supported = device
        .default_output_config()
        .context("default output config")?;
    let config: cpal::StreamConfig = supported.config();
    let dev_rate = config.sample_rate.0;

    let resampled = Arc::new(resample_all(samples, src_rate, dev_rate));
    let pos = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(samples.is_empty()));

    let data = Arc::clone(&resampled);
    let pos_cb = Arc::clone(&pos);
    let done_cb = Arc::clone(&done);
    let health = StreamHealth::new();
    let stream = build_output_stream_f32(
        &device,
        &config,
        supported.sample_format(),
        &health,
        move |mono, _info| {
            let mut p = pos_cb.load(Ordering::Relaxed);
            for m in mono.iter_mut() {
                *m = if p < data.len() {
                    let s = data[p];
                    p += 1;
                    s
                } else {
                    0.0
                };
            }
            pos_cb.store(p, Ordering::Relaxed);
            if p >= data.len() {
                done_cb.store(true, Ordering::Relaxed);
            }
        },
    )?;
    stream.play()?;
    // bounded wait: a stalled/removed device must not hang the caller forever
    let expected = Duration::from_secs_f64(resampled.len() as f64 / dev_rate.max(1) as f64);
    let drain_deadline = Instant::now() + expected + Duration::from_secs(2);
    while !done.load(Ordering::Relaxed) {
        // A dead stream never reaches `done`; report why instead of waiting out
        // the deadline and blaming a stall.
        if let Some(e) = health.take_error() {
            return Err(anyhow!(e));
        }
        if Instant::now() >= drain_deadline {
            return Err(anyhow!("output stream stalled (no progress before deadline)"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // let the device buffer drain before tearing the stream down
    std::thread::sleep(Duration::from_millis(150));
    drop(stream);
    Ok(())
}

pub struct LivePlayback {
    _stream: cpal::Stream,
    health: Arc<StreamHealth>,
}

pub struct AudioTx {
    prod: HeapProd<f32>,
    /// 伺服关闭且收发同速率时是 `None`（逐样本直通，与加伺服之前逐字同构）；
    /// 只要伺服开着就一定在场——弯速率是它唯一的执行器。
    resampler: Option<VarResampler>,
    staging: Vec<f32>,
    /// 跨时钟速率伺服。见模块中部「跨时钟速率伺服（play_ring）」那一节。
    servo: PlayServo,
    /// 声卡侧发布的观测量（`consumed` / 回调时刻 / DAC 滞后 / 欠载）。
    dev: Arc<DeviceClock>,
    /// 环的容量与消费者速率，都是**设备**速率。播放环的 cap 恰好 = dev_rate
    /// = 1.000 秒，所以拿 48000 硬算 44.1k 设备会把 1 秒报成 918 ms
    /// （规格 §3.5 点名的 −8.8% 偏差）。
    dev_rate: u32,
    /// 送进来的样本的速率（线上恒 48k）。伺服要用它把输入块长换算成设备率
    /// 样本，好把环路带宽整定在真实的更新间隔上。
    src_rate: u32,
    /// 写不进去而被丢掉的样本数（累计）。
    ///
    /// 这里的丢弃方向是 **`DropMode::Newest`**：`push_slice` 满了就短写，
    /// 新采样根本没进环。深度读数与「丢最旧」的源侧 FIFO 完全简并，听感却是
    /// 「迟到 + 周期性断续」而不是「恒定迟到但连续」（规格 §0.2）。
    ///
    /// 在此之前这里是全链路唯一**完全无遥测**的丢弃点：`let _ = push_slice(..)`
    /// 静默丢尾、零计数、零日志。丢弃**行为本身没有任何改变**，只是现在数得出来。
    dropped: Arc<AtomicU64>,
}

impl AudioTx {
    pub fn push(&mut self, mono_samples: &[f32]) {
        self.push_at(mono_samples, Instant::now());
    }

    /// `push` 的可注入时钟版本。生产代码只走 `push`；测试用它跑**虚拟时间**
    /// （`t0 + Duration::from_millis(10*tick)`），这样一小时的漂移可以在几秒里
    /// 跑完，而走的仍然是同一条生产路径、同一个真 `HeapRb`、同一个真重采样器。
    #[doc(hidden)]
    pub fn push_at(&mut self, mono_samples: &[f32], now: Instant) {
        if self.servo.enabled {
            match self.servo_step(mono_samples.len(), now) {
                ServoAction::Skip => return, // 深端硬跳：本块整块不写
                ServoAction::Pad(n) => self.write_silence(n),
                ServoAction::Go => {}
            }
        }
        match self.resampler.as_mut() {
            None => {
                let wrote = self.prod.push_slice(mono_samples);
                self.note_short_write(mono_samples.len(), wrote);
            }
            Some(rs) => {
                self.staging.clear();
                rs.process(mono_samples, &mut self.staging);
                let wrote = self.prod.push_slice(&self.staging);
                self.note_short_write(self.staging.len(), wrote);
            }
        }
    }

    /// 跑一次环路。返回本块该怎么处理。
    ///
    /// # 误差信号：只统计**执行器下游**的缓冲
    ///
    /// 调研要求「误差信号必须取全链路之和」，先例是 Snapcast `stream.cpp:305`
    /// 连 `outputBufferDacTime` 都算进 `age`。这条要求的**正确一般化**是：
    ///
    /// > 误差里必须包含执行器**下游**的每一级缓冲；执行器应当尽量靠前放。
    ///
    /// Snapcast 之所以能把整条链算进去，是因为它的执行器就在客户端最前端，
    /// 客户端里的一切都在它下游。我们这个执行器在 `AudioTx` 里，下游只有两级：
    ///
    /// | 项 | 取不取得到 | 怎么取 |
    /// |---|---|---|
    /// | `play_ring` 深度 | ✅ 精确 | `prod.occupied_len()`，整数样本，无估计成分 |
    /// | 声卡回调相位（写侧按 10 ms、读侧按声卡周期，读数带锯齿） | ✅ 可剔除 | 时间插值：减掉「距上次回调这段时间里声卡本该取走的量」 |
    /// | 声卡硬件缓冲 `outputBufferDacTime` | ⚠️ 平台给才有 | cpal `OutputCallbackInfo::timestamp()` 的 `playback − callback`；给不出就恒 0 |
    /// | `jitter_buf` / `post_mix`（**上游**） | ✅ 取得到，但**故意不算** | 见下 |
    ///
    /// 上游两级取得到（`RxStream` 里就有），但**必须排除**：它们由 `mixer_loop`
    /// 按本地单调时钟每 tick 定量 pop，弯声卡比率**一个样本都动不了它们**。
    /// 把一个本执行器无权限的量放进误差，唯一的后果是积分器缠绕到 ±500 ppm
    /// 钳位、把播放环推向欠载或饱和——正好是我们要治的病。JB 那一级另有自己的
    /// 界（`media.rs` 的 `while frames.len() > target + 6`，封顶 180 ms 且自愈），
    /// 归它自己管。
    ///
    /// 时间插值那一条是三个独立实现的共同技巧：zita 的
    /// `(_k_a1 − _k_a0)·d1/d2`、PipeWire `alsa-pcm.c` 的 `snd_pcm_htimestamp`、
    /// PipeWire `node-driver.c` 的 `time_since_nsec`（源码注释：
    /// "increases the control loop stability"）。**喂进环路之前先把测量自身的
    /// 调度抖动剔掉。**
    ///
    /// # 误差符号（写反就是正反馈，一路推到饱和）
    ///
    /// `spa_dll_update` 的定义是 `z1 += w0(w1·err − z1)`（`w1 > 0`），返回
    /// `1 − (z2+z3)`，所以 **`err > 0 ⇒ corr < 1`**。
    ///
    /// 我们的执行器是 `step = nominal / corr`：`corr < 1` ⇒ 步长变大 ⇒ 同样多
    /// 的输入产出更少的输出 ⇒ 写进环里的少了 ⇒ **水位下降**。
    ///
    /// 所以要「水位偏深 ⇒ 水位下降」，就要「水位偏深 ⇒ `corr < 1`」，
    /// 即「水位偏深 ⇒ `err > 0`」，即
    ///
    /// ```text
    /// err = downstream − target        // ★ 生产者 / playback 语义
    /// ```
    ///
    /// **这与 mac 侧 `tx_loop` 的符号相反，而且必须相反。** 那边是 HAL 环的
    /// **消费者**（capture 语义），执行器是唤醒周期：水位偏深要**读得更多**
    /// ⇒ 周期变短 ⇒ `next_time += T/corr` 要 `corr > 1` ⇒ 那边写
    /// `err = target − depth`。PipeWire 自己就是按生产/消费分开取符号的
    /// （`alsa-pcm.c:3032–3035`：playback 用 `delay − target`，capture 用
    /// `target − delay`，且 `rate_match` 一边取 `corr`、一边取 `1.0/corr`）。
    /// 我们是**往设备写**，所以取 playback 那一支。
    ///
    /// 守这条的是 `flipping_the_error_sign_turns_the_servo_into_a_divergence_engine`。
    fn servo_step(&mut self, n_in: usize, now: Instant) -> ServoAction {
        let rate = self.dev_rate as f64;
        // ---- 目标水位：一观测到真实的声卡周期就抬到 2·block + 1 tick ----
        // 每 tick 重算，**上下都跟**——不写成「只涨不落」。只涨不落就是一个
        // 棘轮：一次异常的大回调会把目标永久抬高，而这正是本项目在治的病。
        // 目标降下来时多出的那点水位由细调正常吐掉（几毫秒，几秒内完事）。
        let block = self.dev.block_max();
        if block > 0 {
            let want = (2 * block) as f64 + rate * 0.01;
            let cap = PlayServo::MAX_TARGET_MS * rate / 1000.0;
            self.servo.target = want.max(self.servo.base_target).min(cap);
        }
        // ---- 环路带宽跟着真实块长走（`w0`/`w2` 要的是真的更新间隔）----
        let n_out = (n_in as f64 * rate / self.src_rate as f64).round().max(1.0) as u32;
        if n_out.abs_diff(self.servo.tuned_period) * 10 > self.servo.tuned_period {
            self.servo.tuned_period = n_out;
            let bw = if self.servo.capture_left > 0 { Dll::BW_MAX } else { Dll::BW_MIN };
            self.servo.dll.set_bw(bw, n_out, self.dev_rate);
        }
        // ---- 误差信号 ----
        let ring = self.prod.occupied_len() as f64;
        let dac = self.dev.dac_lag_samples(self.dev_rate);
        let inflight = match self.dev.since_last_callback(now) {
            // 回调停摆（设备刚开、被拔掉、或者线程饿死）：读数不可信，
            // 宁可不动也不要拿一个错的量去积分。
            Some(d) if d > Duration::from_millis(250) => {
                PLAY_SERVO.stalled.fetch_add(1, Ordering::Relaxed);
                return ServoAction::Go;
            }
            Some(d) => d.as_secs_f64() * rate,
            None => {
                PLAY_SERVO.stalled.fetch_add(1, Ordering::Relaxed);
                return ServoAction::Go;
            }
        };
        let downstream = ring - inflight + dac;
        // ★ 符号推导见上。`error_sign` 恒为 +1.0，只有那条守着这个减号的测试
        //   会把它翻成 −1.0。
        let err = (downstream - self.servo.target) * self.servo.error_sign;
        // ---- 三档：死区内喂 DLL / 钳位后喂 DLL / 硬跳 ----
        let mut action = ServoAction::Go;
        // 进入条件是 `err > max_resync`，**退出条件是 `err ≤ 0`**——两个不同的
        // 阈值，这不是笔误：
        //
        // 若退出条件也写成 `err ≤ max_resync`，硬跳就只会把水位放到
        // `target + max_resync`（我们的整定下 = 181 ms），剩下那 150 ms 交给
        // 细调去吐——而 500 ppm 只有 0.5 ms/s，吐完要 **300 秒**。等于「跳」了
        // 一个寂寞：听感上照样是五分钟的高延迟。所以一旦决定跳，就跳到目标为止
        // （PipeWire `alsa_sync` 与 zita `rd_commit(k)` 都是一次 seek 到位）。
        // 迟滞由 `resyncing_deep` 这个状态位提供，不会在阈值边缘反复进出。
        if err > self.servo.max_resync || (self.servo.resyncing_deep && err > 0.0) {
            // 深端硬跳：丢掉输入，让声卡把存量放完。这就是治法 A 的
            // `rd_commit(k)` / `alsa_sync`，只是发生在生产者这一端。
            // DLL 在整个丢弃期间**冻结**，退出时才重置——重置 N 次等于没重置。
            self.servo.resyncing_deep = true;
            PLAY_SERVO
                .resync_skipped
                .fetch_add(n_in as u64, Ordering::Relaxed);
            self.publish(err, downstream);
            return ServoAction::Skip;
        }
        if self.servo.resyncing_deep {
            self.servo.resyncing_deep = false;
            self.servo.rearm_capture();
            PLAY_SERVO.resync_events.fetch_add(1, Ordering::Relaxed);
        } else if downstream < self.servo.starve_mark() {
            // 浅端硬跳（`expand`）。触发线见 `PlayServo::starve_mark`
            // ——以本文件的整定（target≈30 ms、max_resync=150 ms）实际生效的是
            // **半个目标**，因为 1.000 秒的环根本容不下一个 150 ms 的下溢。
            //
            // 为什么需要它，而不是让细调慢慢补：500 ppm 只能补 0.5 ms/s，
            // 把 15 ms 的亏空补回来要 30 秒，这 30 秒里环一直贴着底反复欠载。
            // 它同时承担**开流冷启动**——刚打开时环是空的，没有这一跳，
            // 头一分钟全在欠载。
            //
            // 补的是静音而不是插值出来的音频：样本本来就不存在，声卡刚刚已经
            // 播了等长的静音，这里只是把「谁来补、补多少」从声卡手里收回来，
            // 好让水位重新可控。
            let need = (self.servo.target - downstream).max(0.0).round() as usize;
            if need > 0 {
                PLAY_SERVO
                    .resync_padded
                    .fetch_add(need as u64, Ordering::Relaxed);
                PLAY_SERVO.resync_events.fetch_add(1, Ordering::Relaxed);
                action = ServoAction::Pad(need);
            }
            self.servo.rearm_capture();
            self.publish(err, downstream);
            return action;
        }
        // ---- 细调 ----
        let fed = err.clamp(-self.servo.max_error, self.servo.max_error);
        let raw = self.servo.dll.update(fed);
        let lo = 1.0 - PlayServo::MAX_PPM * 1e-6;
        let hi = 1.0 + PlayServo::MAX_PPM * 1e-6;
        let corr = raw.clamp(lo, hi);
        if corr != raw {
            PLAY_SERVO.clamped.fetch_add(1, Ordering::Relaxed);
        }
        self.servo.corr = corr;
        if let Some(rs) = self.resampler.as_mut() {
            rs.set_correction(corr);
        }
        if self.servo.capture_left > 0 {
            self.servo.capture_left -= 1;
            if self.servo.capture_left == 0 {
                // 抄 zita：4 秒锁定期结束就收窄带宽，把测量噪声压下去。
                self.servo
                    .dll
                    .set_bw(Dll::BW_MIN, self.servo.tuned_period, self.dev_rate);
            }
        }
        PLAY_SERVO.updates.fetch_add(1, Ordering::Relaxed);
        PLAY_SERVO
            .corr_ppm
            .store(((corr - 1.0) * 1e6).round() as i64, Ordering::Relaxed);
        self.publish(err, downstream);
        action
    }

    fn publish(&mut self, err: f64, downstream: f64) {
        self.servo.last_err = err;
        let g = &PLAY_SERVO;
        g.err_samples.store(err.round() as i64, Ordering::Relaxed);
        g.downstream_samples
            .store(downstream.max(0.0).round() as u64, Ordering::Relaxed);
        g.target_samples
            .store(self.servo.target.round() as u64, Ordering::Relaxed);
        g.dev_underruns
            .store(self.dev.underruns(), Ordering::Relaxed);
        g.dac_lag_samples.store(
            self.dev.dac_lag_samples(self.dev_rate).round() as u64,
            Ordering::Relaxed,
        );
    }

    fn write_silence(&mut self, n: usize) {
        // 分块写，避免为一次重同步分配一整秒的零。
        let mut left = n;
        let chunk = [0.0f32; 480];
        while left > 0 {
            let k = left.min(chunk.len());
            let wrote = self.prod.push_slice(&chunk[..k]);
            if wrote == 0 {
                break; // 环满了，补不进去也就不用补了
            }
            left -= wrote;
        }
    }

    fn note_short_write(&self, wanted: usize, wrote: usize) {
        if wrote < wanted {
            self.dropped
                .fetch_add((wanted - wrote) as u64, Ordering::Relaxed);
        }
    }

    /// 此刻环里排队等着送进声卡的样本数（规格 §3.2 的级 8 `play_ring`）。
    /// 整数样本计数，单机单时钟内读取，**无任何估计成分**。
    pub fn queued(&self) -> u32 {
        self.prod.occupied_len() as u32
    }

    /// 环容量（样本）。播放环恰好 = 1 秒设备速率。
    pub fn capacity(&self) -> u32 {
        self.prod.capacity().get() as u32
    }

    /// 消费者（声卡）的标称速率。换算 ms 必须用它，不能用 48000。
    pub fn dev_rate(&self) -> u32 {
        self.dev_rate
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// **测试用**：不开设备，造一个与 `LivePlayback::on_device` 同构的播放环
    /// （同一个 `HeapRb`、同样 = `dev_rate` 的 1 秒容量、同一个短写丢弃计数），
    /// 外加一个替代声卡回调的消费端。
    ///
    /// `#[doc(hidden)] pub` 而不是 `#[cfg(test)]`：`publish_play_ring` 在
    /// **另一个 crate**（audiohubd），`cfg(test)` 只对本 crate 自己的测试生效，
    /// 到不了那里。没有这个口子，「播放环那一级的速率/深度/丢弃到底接对没有」
    /// 就只能靠断言手写字面量——那正是这套遥测要消灭的失败形态。
    /// **伺服关闭**——也就是**加伺服之前**的行为。既有的注入 A–D 全部建立在
    /// 这个形态上（「全链路六级无一做深度伺服」），所以它保持原样；要测生产
    /// 形态用 [`AudioTx::detached_for_test_with_servo`]。
    #[doc(hidden)]
    pub fn detached_for_test(dev_rate: u32) -> (AudioTx, PlayRingSink) {
        AudioTx::detached(dev_rate, false)
    }

    /// **伺服打开**，与 `on_device` 的生产配置逐字一致。
    #[doc(hidden)]
    pub fn detached_for_test_with_servo(dev_rate: u32) -> (AudioTx, PlayRingSink) {
        AudioTx::detached(dev_rate, true)
    }

    fn detached(dev_rate: u32, servo: bool) -> (AudioTx, PlayRingSink) {
        // 与 `on_device` 逐字相同的一行：>= 500ms required; use 1s of device-rate samples
        let rb = HeapRb::<f32>::new(dev_rate.max(8000) as usize);
        let (prod, cons) = rb.split();
        let dev = DeviceClock::new();
        (
            AudioTx {
                prod,
                resampler: servo.then(|| VarResampler::new(48_000, dev_rate)),
                staging: Vec::new(),
                servo: PlayServo::new(dev_rate, servo),
                dev: dev.clone(),
                dev_rate,
                src_rate: 48_000,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            PlayRingSink { cons, dev },
        )
    }

    /// 当前的速率修正，ppm。测试用来断言方向与钳位。
    #[doc(hidden)]
    pub fn servo_corr_ppm(&self) -> f64 {
        (self.servo.corr - 1.0) * 1e6
    }

    /// 当前目标水位（设备率样本）。
    #[doc(hidden)]
    pub fn servo_target(&self) -> u32 {
        self.servo.target.round() as u32
    }

    /// 最近一次喂进环路的误差（设备率样本）。**每实例**，与进程级那份
    /// 「最后写者赢」的 `play_servo_counters().err_samples` 不同 —— 测试要用
    /// 这一个，否则并行跑的兄弟测试会互相覆盖读数。
    #[doc(hidden)]
    pub fn servo_last_err(&self) -> f64 {
        self.servo.last_err
    }

    /// **只给那条守符号的测试**。见 `PlayServo::error_sign`。
    #[doc(hidden)]
    pub fn servo_invert_error_for_test(&mut self) {
        self.servo.error_sign = -1.0;
    }
}

/// 测试用的播放环消费端，站在声卡输出回调的位置上。见
/// [`AudioTx::detached_for_test`]。
#[doc(hidden)]
pub struct PlayRingSink {
    cons: HeapCons<f32>,
    dev: Arc<DeviceClock>,
}

impl PlayRingSink {
    /// 像输出回调那样取走最多 `n` 个样本，返回真正取到的数量。
    ///
    /// **不**发布 `DeviceClock`：既有注入测试用的是这一条，它们的语义是
    /// 「一个没有伺服的环」，不该被时钟观测量影响。要模拟真声卡用
    /// [`PlayRingSink::drain_at`]。
    pub fn drain(&mut self, n: usize) -> usize {
        let mut buf = vec![0.0f32; n];
        self.cons.pop_slice(&mut buf)
    }

    /// 像真的输出回调那样取样本**并发布时钟观测量**：取走的量、回调时刻、
    /// DAC 滞后、欠载。`now` 是虚拟时间，见 [`AudioTx::push_at`]。
    #[doc(hidden)]
    pub fn drain_at(&mut self, n: usize, now: Instant, dac_lag: Option<Duration>) -> usize {
        let mut buf = vec![0.0f32; n];
        let got = self.cons.pop_slice(&mut buf);
        self.dev.note_callback(now, n, got, dac_lag);
        got
    }
}

impl LivePlayback {
    /// `false` once the device reported a fatal stream error — the card is gone
    /// or the host killed the stream, and nothing pushed into the paired
    /// `AudioTx` can reach it any more. One-way: a dead playback must be
    /// dropped and reopened, never waited on.
    pub fn is_alive(&self) -> bool {
        self.health.is_alive()
    }

    /// The recorded cause, handed to the first caller that asks so a supervisor
    /// reports it exactly once. `is_alive` stays `false` afterwards.
    pub fn take_error(&self) -> Option<String> {
        self.health.take_error()
    }

    pub fn start(src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        LivePlayback::on_device(&device, src_rate)
    }

    /// Plays to the named output device. Errors when the name does not resolve
    /// or the device will not open — never falls back to the default device.
    pub fn start_on(device_name: &str, src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let (device, resolved) = find_device(DeviceKind::Output, device_name)?;
        LivePlayback::on_device(&device, src_rate)
            .with_context(|| format!("open output device {resolved:?}"))
    }

    /// Plays to the output device carrying this UID. The name-based sibling
    /// cannot address a device whose name is generated at runtime, nor tell two
    /// identically named cards apart; the UID can do both.
    pub fn start_on_uid(uid: &str, src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let (device, resolved) = find_device_by_uid(DeviceKind::Output, uid)?;
        LivePlayback::on_device(&device, src_rate)
            .with_context(|| format!("open output device {resolved:?} (UID {uid:?})"))
    }

    fn on_device(device: &cpal::Device, src_rate: u32) -> Result<(LivePlayback, AudioTx)> {
        let supported = device
            .default_output_config()
            .context("default output config")?;
        let config: cpal::StreamConfig = supported.config();
        let dev_rate = config.sample_rate.0;

        // >= 500ms required; use 1s of device-rate samples
        let rb = HeapRb::<f32>::new(dev_rate.max(8000) as usize);
        let (prod, mut cons) = rb.split();

        let health = StreamHealth::new();
        // 声卡侧的观测量。回调是这条链上**唯一**知道「设备真的取走了多少、
        // 什么时候取的、写进去的东西还要多久才出 DAC」的地方；伺服要的三个量
        // 全部只能在这里采。
        let dev = DeviceClock::new();
        let dev_cb = dev.clone();
        let stream = build_output_stream_f32(
            device,
            &config,
            supported.sample_format(),
            &health,
            move |mono, info| {
                let got = cons.pop_slice(mono);
                for m in &mut mono[got..] {
                    *m = 0.0; // underrun -> silence
                }
                // `playback − callback` = Snapcast 的 `outputBufferDacTime`。
                // cpal 在 coreaudio 用 `mach_absolute_time`、wasapi 用 QPC 填这
                // 两个时刻；平台给不出时它们相等，`duration_since` 得 0，
                // 于是这一项自然退化成「不计」而不是乱猜。
                let ts = info.timestamp();
                dev_cb.note_callback(
                    Instant::now(),
                    mono.len(),
                    got,
                    ts.playback.duration_since(&ts.callback),
                );
            },
        )?;
        stream.play()?;

        Ok((
            LivePlayback { _stream: stream, health },
            AudioTx {
                prod,
                // 伺服开着 ⇒ 重采样器**必须**在场，哪怕 src_rate == dev_rate：
                // 弯比率是环路唯一的执行器，没有它环路就没有手。
                resampler: Some(VarResampler::new(src_rate, dev_rate)),
                staging: Vec::new(),
                servo: PlayServo::new(dev_rate, true),
                dev,
                dev_rate,
                src_rate,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        ))
    }
}

pub struct LiveCapture {
    _stream: cpal::Stream,
    health: Arc<StreamHealth>,
}

pub struct AudioRx {
    cons: HeapCons<f32>,
    /// 采集设备的速率。环是 `rate * 2`（2 秒），换算必须用它。
    rate: u32,
    /// 采集回调写不进去而丢掉的样本数（累计）。方向同样是
    /// **`DropMode::Newest`**（`push_slice` 短写）。
    ///
    /// 规格 §0.4：`pop` 每次调用**全量排空**，所以这个环的稳态驻留只有一个
    /// tick 加回调突发（10–20 ms 量级），**不是延迟嫌疑**。但它溢出时丢的是
    /// 真实音频，那是**音质**指标的输入——所以计数必须有，只是不该被当成
    /// 延迟证据。丢弃行为本身未改。
    dropped: Arc<AtomicU64>,
}

impl AudioRx {
    pub fn pop(&mut self, out: &mut Vec<f32>) -> usize {
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

    /// 此刻环里积压的样本数（规格 §3.2 的级 1 `cap_ring`）。
    pub fn queued(&self) -> u32 {
        self.cons.occupied_len() as u32
    }

    /// 环容量（样本）。注意是 **2 秒**，不是 1 秒（规格 §0.4 的修正四）。
    pub fn capacity(&self) -> u32 {
        self.cons.capacity().get() as u32
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// **测试用**：不开设备，造一个与 `LiveCapture::on_device` 同构的 **2 秒**
    /// 采集环（规格 §0.4 的修正四：这个环是 2 秒不是 1 秒），外加一个替代采集
    /// 回调的写入端。
    ///
    /// `#[doc(hidden)] pub` 而不是 `#[cfg(test)]`：`MicSource` 在另一个 crate
    /// （audiohub-net），`cfg(test)` 到不了那里。
    #[doc(hidden)]
    pub fn detached_for_test(rate: u32) -> (AudioRx, CaptureFeed) {
        let rb = HeapRb::<f32>::new((rate as usize) * 2); // 2s，与 on_device 同一行
        let (prod, cons) = rb.split();
        let dropped = Arc::new(AtomicU64::new(0));
        (
            AudioRx { cons, rate, dropped: Arc::clone(&dropped) },
            CaptureFeed { prod, dropped },
        )
    }
}

/// 测试用的采集环写入端，站在 cpal 采集回调的位置上。见
/// [`AudioRx::detached_for_test`]。
#[doc(hidden)]
pub struct CaptureFeed {
    prod: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
}

impl CaptureFeed {
    /// 与采集回调最后那两行**逐字相同**的写入语义：环满就短写，丢的是**新**
    /// 样本（`DropMode::Newest`），并把短掉的数量记进同一个计数器。
    pub fn write(&mut self, mono: &[f32]) -> usize {
        let wrote = self.prod.push_slice(mono);
        if wrote < mono.len() {
            self.dropped
                .fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
        }
        wrote
    }
}

impl LiveCapture {
    /// `false` once the device reported a fatal stream error — the paired
    /// `AudioRx` will never produce another sample. One-way, like playback:
    /// silence from a dead capture is indistinguishable from a quiet room, so
    /// the owner has to ask rather than infer.
    pub fn is_alive(&self) -> bool {
        self.health.is_alive()
    }

    /// The recorded cause, handed to the first caller that asks.
    pub fn take_error(&self) -> Option<String> {
        self.health.take_error()
    }

    pub fn start() -> Result<(LiveCapture, AudioRx, u32)> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        LiveCapture::on_device(&device)
    }

    /// Captures from the named input device (e.g. the input side of a virtual
    /// card). Errors rather than falling back to the default microphone.
    pub fn start_on(device_name: &str) -> Result<(LiveCapture, AudioRx, u32)> {
        let (device, resolved) = find_device(DeviceKind::Input, device_name)?;
        LiveCapture::on_device(&device).with_context(|| format!("open input device {resolved:?}"))
    }

    /// Captures from the input device carrying this UID — the only stable way
    /// to name a per-peer virtual microphone, whose display name changes with
    /// the peer it belongs to.
    pub fn start_on_uid(uid: &str) -> Result<(LiveCapture, AudioRx, u32)> {
        let (device, resolved) = find_device_by_uid(DeviceKind::Input, uid)?;
        LiveCapture::on_device(&device)
            .with_context(|| format!("open input device {resolved:?} (UID {uid:?})"))
    }

    fn on_device(device: &cpal::Device) -> Result<(LiveCapture, AudioRx, u32)> {
        let supported = device
            .default_input_config()
            .context("default input config")?;
        let config: cpal::StreamConfig = supported.config();
        let rate = config.sample_rate.0;
        let channels = config.channels as usize;

        let rb = HeapRb::<f32>::new((rate as usize) * 2); // 2s
        let (mut prod, cons) = rb.split();

        // 采集回调与 AudioRx 各持一份：回调是唯一的写入方，AudioRx 是唯一的
        // 读取方。回调里只有一次 `fetch_add`，且仅在真的短写时才执行。
        let dropped = Arc::new(AtomicU64::new(0));
        let cb_dropped = Arc::clone(&dropped);

        let health = StreamHealth::new();
        let mut mono: Vec<f32> = Vec::new();
        let stream = match supported.sample_format() {
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().map(|&v| v as f32 / 32768.0).sum();
                        mono.push(sum / channels as f32);
                    }
                    let wrote = prod.push_slice(&mono);
                    if wrote < mono.len() {
                        cb_dropped.fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
                    }
                },
                err_sink(Arc::clone(&health), "input"),
                None,
            )?,
            _ => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().sum();
                        mono.push(sum / channels as f32);
                    }
                    let wrote = prod.push_slice(&mono);
                    if wrote < mono.len() {
                        cb_dropped.fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
                    }
                },
                err_sink(Arc::clone(&health), "input"),
                None,
            )?,
        };
        stream.play()?;
        Ok((
            LiveCapture { _stream: stream, health },
            AudioRx { cons, rate, dropped },
            rate,
        ))
    }
}

// ------------------------------------------------- default device hot-swap

/// Signal the OS notification thread raises and our worker thread consumes.
/// Repeated notifications collapse into one wakeup: the callback carries no
/// payload, it only means "re-query".
struct Fanout {
    state: Mutex<FanoutState>,
    cv: Condvar,
}

#[derive(Default)]
struct FanoutState {
    pending: bool,
    stop: bool,
}

impl Fanout {
    fn new() -> Fanout {
        Fanout { state: Mutex::new(FanoutState::default()), cv: Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FanoutState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called ON the platform notification thread. The only thing it may touch
    /// is this uncontended mutex — the user callback runs on our worker.
    fn signal(&self) {
        self.lock().pending = true;
        self.cv.notify_all();
    }

    fn stop(&self) {
        self.lock().stop = true;
        self.cv.notify_all();
    }

    /// `true` = one or more notifications arrived, `false` = shut down.
    fn wait(&self) -> bool {
        let mut s = self.lock();
        loop {
            if s.stop {
                return false;
            }
            if s.pending {
                s.pending = false;
                return true;
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Lets `start` report a registration failure that happens on the worker.
struct Handshake {
    slot: Mutex<Option<std::result::Result<(), String>>>,
    cv: Condvar,
}

impl Handshake {
    fn new() -> Handshake {
        Handshake { slot: Mutex::new(None), cv: Condvar::new() }
    }

    fn publish(&self, r: std::result::Result<(), String>) {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(r);
        self.cv.notify_all();
    }

    /// `None` = the worker is still inside registration. It is then NOT joinable
    /// without reintroducing the unbounded wait this deadline exists to avoid.
    fn take(&self) -> Option<std::result::Result<(), String>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = g.take() {
                return Some(r);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Fires the callback whenever the system default device of `kind` changes.
/// Dropping it deregisters the platform listener and joins the worker, so the
/// callback provably cannot run once `drop` has returned.
pub struct DeviceChangeWatcher {
    fanout: Arc<Fanout>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DeviceChangeWatcher {
    pub fn start(kind: DeviceKind, cb: Box<dyn Fn() + Send + 'static>) -> Result<DeviceChangeWatcher> {
        let fanout = Arc::new(Fanout::new());
        let ready = Arc::new(Handshake::new());
        let f = Arc::clone(&fanout);
        let r = Arc::clone(&ready);
        // Registration, service and deregistration all live on this one thread:
        // the Windows COM apartment is thread-affine, and joining the thread is
        // what makes "no callback after drop" a fact rather than a hope.
        let worker = std::thread::Builder::new()
            .name("audiohub-devwatch".to_string())
            .spawn(move || {
                let reg = match watch_imp::register(kind, Arc::clone(&f)) {
                    Ok(reg) => {
                        r.publish(Ok(()));
                        reg
                    }
                    Err(e) => {
                        r.publish(Err(format!("{e:#}")));
                        return;
                    }
                };
                while f.wait() {
                    cb();
                }
                watch_imp::unregister(reg);
            })
            .context("spawn device watcher thread")?;

        match ready.take() {
            Some(Ok(())) => Ok(DeviceChangeWatcher { fanout, worker: Some(worker) }),
            Some(Err(msg)) => {
                // The worker already returned, so this join is immediate.
                fanout.stop();
                let _ = worker.join();
                Err(anyhow!(msg))
            }
            None => {
                // Still stuck in registration: ask it to unwind and let it go
                // rather than block the caller for however long that takes.
                fanout.stop();
                drop(worker);
                Err(anyhow!("device watcher did not register before the deadline"))
            }
        }
    }
}

impl Drop for DeviceChangeWatcher {
    fn drop(&mut self) {
        self.fanout.stop();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The AudioObject property-listener ABI, declared exactly once. Two modules
/// register listeners (default-device selectors, and the device list) and each
/// declaring its own `PropAddr` would make the two `ListenerProc` types
/// distinct — which the clashing-extern-declarations lint reports and which
/// would become a real hazard the moment somebody edited one copy.
#[cfg(target_os = "macos")]
mod ca_listener {
    use std::ffi::c_void;

    pub type OSStatus = i32;
    pub type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PropAddr {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    pub const SYSTEM_OBJECT: AudioObjectID = 1;
    pub const ELEM_MAIN: u32 = 0;

    pub const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    pub const SCOPE_GLOBAL: u32 = fourcc(b"glob");

    pub type ListenerProc =
        unsafe extern "C" fn(AudioObjectID, u32, *const PropAddr, *mut c_void) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        pub fn AudioObjectAddPropertyListener(
            id: AudioObjectID,
            addr: *const PropAddr,
            listener: ListenerProc,
            data: *mut c_void,
        ) -> OSStatus;
        pub fn AudioObjectRemovePropertyListener(
            id: AudioObjectID,
            addr: *const PropAddr,
            listener: ListenerProc,
            data: *mut c_void,
        ) -> OSStatus;
    }
}

#[cfg(target_os = "macos")]
mod watch_imp {
    //! AudioObjectAddPropertyListener on the system object's
    //! kAudioHardwarePropertyDefaultInput/OutputDevice.

    use super::ca_listener::{
        fourcc, AudioObjectAddPropertyListener, AudioObjectID, AudioObjectRemovePropertyListener,
        OSStatus, PropAddr, ELEM_MAIN, SCOPE_GLOBAL, SYSTEM_OBJECT,
    };
    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::sync::Arc;

    const SEL_DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice

    /// Raw pointer inside: created and consumed on the watcher thread only.
    pub struct Registration {
        addr: PropAddr,
        ctx: *const Fanout,
    }

    unsafe extern "C" fn on_change(
        _id: AudioObjectID,
        _n: u32,
        _addrs: *const PropAddr,
        data: *mut c_void,
    ) -> OSStatus {
        if !data.is_null() {
            (*(data as *const Fanout)).signal();
        }
        0
    }

    pub fn register(kind: DeviceKind, fanout: Arc<Fanout>) -> Result<Registration> {
        let selector = match kind {
            DeviceKind::Input => SEL_DEFAULT_INPUT,
            DeviceKind::Output => SEL_DEFAULT_OUTPUT,
        };
        let addr = PropAddr { selector, scope: SCOPE_GLOBAL, element: ELEM_MAIN };
        // The HAL keeps this pointer until the listener is removed, so the Arc
        // strong count has to stay raised for exactly that long.
        let ctx = Arc::into_raw(fanout);
        let st = unsafe {
            AudioObjectAddPropertyListener(SYSTEM_OBJECT, &addr, on_change, ctx as *mut c_void)
        };
        if st != 0 {
            unsafe { drop(Arc::from_raw(ctx)) };
            bail!("AudioObjectAddPropertyListener failed: OSStatus {st}");
        }
        Ok(Registration { addr, ctx })
    }

    pub fn unregister(reg: Registration) {
        let st = unsafe {
            AudioObjectRemovePropertyListener(
                SYSTEM_OBJECT,
                &reg.addr,
                on_change,
                reg.ctx as *mut c_void,
            )
        };
        if st == 0 {
            unsafe { drop(Arc::from_raw(reg.ctx)) };
        } else {
            // Removal failed, so the HAL may still hold the pointer: leaking one
            // Fanout beats handing it a dangling one.
            eprintln!("[audiohub] AudioObjectRemovePropertyListener failed: OSStatus {st}");
        }
    }
}

#[cfg(windows)]
mod watch_imp {
    //! A hand-rolled IMMNotificationClient registered on an MMDeviceEnumerator.
    //! Vtable layout is the frozen ABI of mmdeviceapi.h; the object is a normal
    //! refcounted COM object, so MMDevAPI's own reference keeps it alive for as
    //! long as it needs and the last Release frees it.

    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    type HRESULT = i32;
    const S_OK: HRESULT = 0;
    const E_POINTER: HRESULT = -2147467261; // 0x80004003
    const E_NOINTERFACE: HRESULT = -2147467262; // 0x80004002

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq)]
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
    const IID_IMM_NOTIFICATION_CLIENT: GUID = GUID {
        d1: 0x7991EEC9,
        d2: 0x7E89,
        d3: 0x4D85,
        d4: [0x83, 0x90, 0x6C, 0x70, 0x3C, 0xEC, 0x60, 0xC0],
    };
    const IID_IUNKNOWN: GUID =
        GUID { d1: 0, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const E_RENDER: i32 = 0; // EDataFlow::eRender
    const E_CAPTURE: i32 = 1; // EDataFlow::eCapture
    const ROLE_CONSOLE: i32 = 0; // ERole::eConsole -- the role cpal and the
                                 // volume backend both resolve the default with

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
        enum_audio_endpoints: usize,
        get_default_audio_endpoint: usize,
        get_device: usize,
        register_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        unregister_endpoint_notification_callback:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropertyKey {
        fmtid: GUID,
        pid: u32,
    }

    #[repr(C)]
    struct IMMNotificationClientVtbl {
        base: IUnknownVtbl,
        on_device_state_changed:
            unsafe extern "system" fn(*mut c_void, *const u16, u32) -> HRESULT,
        on_device_added: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        on_device_removed: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        on_default_device_changed:
            unsafe extern "system" fn(*mut c_void, i32, i32, *const u16) -> HRESULT,
        on_property_value_changed:
            unsafe extern "system" fn(*mut c_void, *const u16, PropertyKey) -> HRESULT,
    }

    #[repr(C)]
    struct NotifyClient {
        vtbl: *const IMMNotificationClientVtbl,
        refs: AtomicU32,
        fanout: Arc<Fanout>,
        flow: i32,
    }

    unsafe extern "system" fn nc_qi(
        this: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        if iid.is_null() {
            *out = ptr::null_mut();
            return E_POINTER;
        }
        let want = *iid;
        if want == IID_IUNKNOWN || want == IID_IMM_NOTIFICATION_CLIENT {
            nc_add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn nc_add_ref(this: *mut c_void) -> u32 {
        (*(this as *mut NotifyClient)).refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn nc_release(this: *mut c_void) -> u32 {
        let c = this as *mut NotifyClient;
        let prev = (*c).refs.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            drop(Box::from_raw(c));
            0
        } else {
            prev - 1
        }
    }

    unsafe extern "system" fn nc_state(_t: *mut c_void, _id: *const u16, _s: u32) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_added(_t: *mut c_void, _id: *const u16) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_removed(_t: *mut c_void, _id: *const u16) -> HRESULT {
        S_OK
    }
    unsafe extern "system" fn nc_prop(
        _t: *mut c_void,
        _id: *const u16,
        _k: PropertyKey,
    ) -> HRESULT {
        S_OK
    }

    /// Runs on an MMDevAPI thread: only ever hands the news to the Fanout.
    unsafe extern "system" fn nc_default_changed(
        this: *mut c_void,
        flow: i32,
        role: i32,
        _id: *const u16,
    ) -> HRESULT {
        let c = &*(this as *const NotifyClient);
        if flow == c.flow && role == ROLE_CONSOLE {
            c.fanout.signal();
        }
        S_OK
    }

    static NC_VTBL: IMMNotificationClientVtbl = IMMNotificationClientVtbl {
        base: IUnknownVtbl { query_interface: nc_qi, add_ref: nc_add_ref, release: nc_release },
        on_device_state_changed: nc_state,
        on_device_added: nc_added,
        on_device_removed: nc_removed,
        on_default_device_changed: nc_default_changed,
        on_property_value_changed: nc_prop,
    };

    /// Balances CoInitializeEx on the watcher thread. RPC_E_CHANGED_MODE means
    /// somebody else already picked the apartment; that one is fine to borrow
    /// and is not ours to tear down.
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

    unsafe fn release(p: *mut c_void) {
        if !p.is_null() {
            let v = *(p as *const *const IUnknownVtbl);
            ((*v).release)(p);
        }
    }

    /// Field order is drop order, and COM demands it: the enumerator has to go
    /// before the apartment it was created in.
    pub struct Registration {
        client: *mut NotifyClient,
        enumerator: *mut c_void,
        _apt: Apartment,
    }

    pub fn register(kind: DeviceKind, fanout: Arc<Fanout>) -> Result<Registration> {
        let apt = Apartment::enter();
        let mut enumerator: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut enumerator,
            )
        };
        if hr < 0 {
            bail!("CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}", hr as u32);
        }
        let flow = match kind {
            DeviceKind::Input => E_CAPTURE,
            DeviceKind::Output => E_RENDER,
        };
        let client = Box::into_raw(Box::new(NotifyClient {
            vtbl: &NC_VTBL,
            refs: AtomicU32::new(1),
            fanout,
            flow,
        }));
        let hr = unsafe {
            let v = *(enumerator as *const *const IMMDeviceEnumeratorVtbl);
            ((*v).register_endpoint_notification_callback)(enumerator, client as *mut c_void)
        };
        if hr < 0 {
            unsafe {
                nc_release(client as *mut c_void);
                release(enumerator);
            }
            bail!("RegisterEndpointNotificationCallback failed: HRESULT 0x{:08X}", hr as u32);
        }
        Ok(Registration { client, enumerator, _apt: apt })
    }

    pub fn unregister(reg: Registration) {
        unsafe {
            let v = *(reg.enumerator as *const *const IMMDeviceEnumeratorVtbl);
            let hr = ((*v).unregister_endpoint_notification_callback)(
                reg.enumerator,
                reg.client as *mut c_void,
            );
            if hr < 0 {
                eprintln!(
                    "[audiohub] UnregisterEndpointNotificationCallback failed: HRESULT 0x{:08X}",
                    hr as u32
                );
            }
            // Our own reference goes last: MMDevAPI may hold one of its own and
            // whichever Release lands second is the one that frees the object.
            nc_release(reg.client as *mut c_void);
            release(reg.enumerator);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod watch_imp {
    use super::{DeviceKind, Fanout};
    use anyhow::{bail, Result};
    use std::sync::Arc;

    pub struct Registration;

    pub fn register(_kind: DeviceKind, _fanout: Arc<Fanout>) -> Result<Registration> {
        bail!("default device change notifications are not implemented on this platform");
    }

    pub fn unregister(_reg: Registration) {}
}

// ------------------------------------------------ device list event stream

/// One appearance or disappearance of a system audio device.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceEvent {
    /// `"added"` or `"removed"`.
    pub kind: &'static str,
    /// Milliseconds since the watch started, from a monotonic clock: this is
    /// the number that says whether two events fell inside the same restart
    /// window, and a wall clock that steps cannot corrupt it.
    pub t_ms: u64,
    /// The same instant on the wall clock, only so events can be lined up
    /// against `log show` output.
    pub unix_ms: u64,
    pub name: String,
    pub uid: Option<String>,
    pub id: Option<u32>,
    pub is_input: bool,
    pub is_output: bool,
}

#[cfg(target_os = "macos")]
fn unix_ms_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Everything the notification callback and the caller share. Behind a Mutex
/// because the diff runs on the HAL's notification thread.
#[cfg(target_os = "macos")]
struct WatchState {
    known: Vec<DeviceEntry>,
    events: Vec<DeviceEvent>,
    cb: Box<dyn FnMut(&DeviceEvent) + Send>,
    start: Instant,
}

#[cfg(target_os = "macos")]
impl WatchState {
    /// Re-enumerates and diffs against the last snapshot. This runs INSIDE the
    /// property listener, not on a poll loop, and that is the entire point: a
    /// device that appears and disappears between two polls is invisible to a
    /// sampler, so a sampler can never establish "nothing changed" — it can
    /// only establish "nothing was changed at the instants I happened to look".
    ///
    /// Re-entering the HAL from a listener is the documented way to service one
    /// (the callback carries no payload; the whole notification means "re-query
    /// me"), and notifications for one object are dispatched serially, so this
    /// lock cannot be taken twice on the same thread.
    fn diff(&mut self) {
        self.diff_against(list_devices_detailed());
    }

    /// The diff itself, split out so it can be tested against a synthetic
    /// snapshot instead of whatever cards the test machine happens to have.
    fn diff_against(&mut self, now: Vec<DeviceEntry>) {
        let t_ms = self.start.elapsed().as_millis() as u64;
        let unix_ms = unix_ms_now();
        let mk = |kind: &'static str, d: &DeviceEntry| DeviceEvent {
            kind,
            t_ms,
            unix_ms,
            name: d.name.clone(),
            uid: d.uid.clone(),
            id: d.id,
            is_input: d.is_input,
            is_output: d.is_output,
        };
        // Removals first, so a slot re-created under one notification reads as
        // "removed then added" rather than two bare additions.
        let mut fresh: Vec<DeviceEvent> = self
            .known
            .iter()
            .filter(|old| !now.iter().any(|n| n.same_device(old)))
            .map(|old| mk("removed", old))
            .collect();
        fresh.extend(
            now.iter()
                .filter(|n| !self.known.iter().any(|old| old.same_device(n)))
                .map(|n| mk("added", n)),
        );
        self.known = now;
        for e in fresh {
            (self.cb)(&e);
            self.events.push(e);
        }
    }
}

/// Watches the system device list for `secs` and returns every add/remove seen,
/// oldest first. Terminates on its own, deregisters the listener before
/// returning, and treats an unchanged window as a successful run with an empty
/// result — proving that nothing happened is exactly what this is for.
///
/// `on_event` fires on the HAL notification thread as each change lands.
#[cfg(target_os = "macos")]
pub fn watch_device_list(
    secs: f32,
    on_event: Box<dyn FnMut(&DeviceEvent) + Send + 'static>,
) -> Result<Vec<DeviceEvent>> {
    let state = Arc::new(Mutex::new(WatchState {
        known: list_devices_detailed(),
        events: Vec::new(),
        cb: on_event,
        start: Instant::now(),
    }));
    let reg = devlist_imp::register(Arc::clone(&state))?;
    std::thread::sleep(Duration::from_secs_f32(secs.max(0.0)));
    let released = devlist_imp::unregister(reg);
    let events = {
        let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
        // One last diff once the listener is gone: a change that landed in the
        // final milliseconds is still a change inside the window, and rounding
        // it away would be the one kind of miss this tool must not have.
        g.diff();
        std::mem::take(&mut g.events)
    };
    if !released {
        // Deregistration failed, so the HAL may still call us: leaking the
        // state keeps that pointer valid instead of dangling.
        std::mem::forget(state);
    }
    Ok(events)
}

#[cfg(not(target_os = "macos"))]
pub fn watch_device_list(
    _secs: f32,
    _on_event: Box<dyn FnMut(&DeviceEvent) + Send + 'static>,
) -> Result<Vec<DeviceEvent>> {
    bail!("watching the audio device list is only implemented on macOS");
}

#[cfg(target_os = "macos")]
mod devlist_imp {
    //! AudioObjectAddPropertyListener on the system object's
    //! kAudioHardwarePropertyDevices — the property that changes when a device
    //! is published or withdrawn.

    use super::ca_listener::{
        fourcc, AudioObjectAddPropertyListener, AudioObjectID, AudioObjectRemovePropertyListener,
        OSStatus, PropAddr, ELEM_MAIN, SCOPE_GLOBAL, SYSTEM_OBJECT,
    };
    use super::WatchState;
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices

    pub struct Registration {
        addr: PropAddr,
        ctx: *const Mutex<WatchState>,
    }

    unsafe extern "C" fn on_devices_changed(
        _id: AudioObjectID,
        _n: u32,
        _addrs: *const PropAddr,
        data: *mut c_void,
    ) -> OSStatus {
        if !data.is_null() {
            let st = &*(data as *const Mutex<WatchState>);
            st.lock().unwrap_or_else(|e| e.into_inner()).diff();
        }
        0
    }

    pub fn register(state: Arc<Mutex<WatchState>>) -> Result<Registration> {
        let addr = PropAddr { selector: SEL_DEVICES, scope: SCOPE_GLOBAL, element: ELEM_MAIN };
        // The HAL keeps this pointer until the listener is removed, so the Arc
        // strong count has to stay raised for exactly that long.
        let ctx = Arc::into_raw(state);
        let st = unsafe {
            AudioObjectAddPropertyListener(
                SYSTEM_OBJECT,
                &addr,
                on_devices_changed,
                ctx as *mut c_void,
            )
        };
        if st != 0 {
            unsafe { drop(Arc::from_raw(ctx)) };
            bail!("AudioObjectAddPropertyListener(dev#) failed: OSStatus {st}");
        }
        Ok(Registration { addr, ctx })
    }

    /// `true` when the HAL provably let go of the context pointer.
    pub fn unregister(reg: Registration) -> bool {
        let st = unsafe {
            AudioObjectRemovePropertyListener(
                SYSTEM_OBJECT,
                &reg.addr,
                on_devices_changed,
                reg.ctx as *mut c_void,
            )
        };
        if st == 0 {
            unsafe { drop(Arc::from_raw(reg.ctx)) };
            true
        } else {
            eprintln!(
                "[audiohub] AudioObjectRemovePropertyListener(dev#) failed: OSStatus {st}"
            );
            false
        }
    }
}

#[cfg(test)]
impl DeviceChangeWatcher {
    /// Does exactly what the platform listener does, so a test can prove the
    /// hand-off without moving the machine's real default device.
    fn test_signal(&self) {
        self.fanout.signal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `unwrap_err` would demand Debug on the stream handles.
    fn err_of<T>(r: Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error, got a live stream"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn dedup_keeps_first_occurrence_order() {
        let got = dedup_in_order(names(&["b", "a", "b", "", "c"]));
        assert_eq!(got, names(&["b", "a", "c"]));
    }

    #[test]
    fn prefix_match_is_case_insensitive() {
        let devs = names(&["BlackHole 2ch", "MacBook Pro Speakers"]);
        for q in ["BlackHole", "blackhole", "BLACKHOLE 2", "BlackHole 2ch"] {
            assert_eq!(
                resolve_name(&devs, q, DeviceKind::Output).unwrap(),
                "BlackHole 2ch",
                "query {q:?}"
            );
        }
    }

    #[test]
    fn exact_match_wins_over_a_longer_device_it_prefixes() {
        let devs = names(&["Loopback Audio", "Loopback Audio 2"]);
        assert_eq!(
            resolve_name(&devs, "Loopback Audio", DeviceKind::Output).unwrap(),
            "Loopback Audio"
        );
    }

    #[test]
    fn exact_match_is_case_insensitive_and_still_beats_prefix_siblings() {
        let devs = names(&["Loopback Audio", "Loopback Audio 2", "MMAudio Device", "MMAudio Device (UI Sounds)"]);
        for (q, want) in [
            ("loopback audio", "Loopback Audio"),
            ("LOOPBACK AUDIO", "Loopback Audio"),
            ("lOoPbAcK aUdIo", "Loopback Audio"),
            ("mmaudio device", "MMAudio Device"),
            ("  Loopback Audio  ", "Loopback Audio"), // trimmed, then exact
        ] {
            assert_eq!(
                resolve_name(&devs, q, DeviceKind::Output).unwrap(),
                want,
                "query {q:?}"
            );
        }
    }

    #[test]
    fn two_devices_with_one_name_are_ambiguous_not_a_coin_flip() {
        let raw = names(&["USB Audio", "MacBook Pro Speakers", "USB Audio"]);
        // presentation collapses them, resolution must not
        assert_eq!(dedup_in_order(raw.clone()), names(&["USB Audio", "MacBook Pro Speakers"]));
        for q in ["USB Audio", "usb audio", "USB"] {
            let e = err_of(resolve_name(&raw, q, DeviceKind::Output));
            assert!(e.contains("ambiguous"), "query {q:?}: {e}");
            assert!(e.contains("2 devices are named"), "query {q:?}: {e}");
        }
        // the unaffected sibling still resolves
        assert_eq!(
            resolve_name(&raw, "macbook", DeviceKind::Output).unwrap(),
            "MacBook Pro Speakers"
        );
    }

    #[test]
    fn case_only_duplicates_are_ambiguous_too() {
        let raw = names(&["Focusrite", "FOCUSRITE"]);
        let e = err_of(resolve_name(&raw, "focusrite", DeviceKind::Input));
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("Focusrite") && e.contains("FOCUSRITE"), "{e}");
    }

    #[test]
    fn ambiguous_prefix_errors_and_lists_the_candidates() {
        let devs = names(&["BlackHole 2ch", "BlackHole 16ch", "ADAM Audio D3V"]);
        let e = err_of(resolve_name(&devs, "black", DeviceKind::Output));
        assert!(e.contains("ambiguous"), "{e}");
        assert!(e.contains("BlackHole 2ch") && e.contains("BlackHole 16ch"), "{e}");
        assert!(!e.contains("ADAM"), "{e}");
    }

    #[test]
    fn no_match_errors_and_never_resolves_to_something_else() {
        let devs = names(&["BlackHole 2ch", "MacBook Pro Speakers"]);
        let e = err_of(resolve_name(&devs, "VB-Cable", DeviceKind::Input));
        assert!(e.contains("no input device matches"), "{e}");
        assert!(e.contains("BlackHole 2ch"), "{e}");
        assert!(resolve_name(&devs, "  ", DeviceKind::Output).is_err());
        // a suffix is not a prefix
        assert!(resolve_name(&devs, "2ch", DeviceKind::Output).is_err());
    }

    // ---- reality checks against the machine running the tests

    #[test]
    fn every_listed_device_resolves_to_itself_and_opens_by_name() {
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            let raw = devices::list_all(kind);
            for n in list_names(kind) {
                let twins = raw.iter().filter(|x| x.to_lowercase() == n.to_lowercase()).count();
                if twins > 1 {
                    // This machine really has two cards under one name: the only
                    // honest answer is a refusal, not one of them at random.
                    let e = err_of(resolve_name(&raw, &n, kind));
                    assert!(e.contains("ambiguous"), "{kind:?} {n:?}: {e}");
                    continue;
                }
                assert_eq!(resolve_name(&raw, &n, kind).unwrap(), n);
                // the same name in the wrong case must take the same fast path
                assert_eq!(resolve_name(&raw, &n.to_lowercase(), kind).unwrap(), n);
                let (_, resolved) = find_device(kind, &n)
                    .unwrap_or_else(|e| panic!("{kind:?} {n:?} not findable: {e:#}"));
                assert_eq!(resolved, n);
            }
        }
    }

    #[test]
    fn output_list_is_not_empty_and_holds_the_default() {
        let outs = list_output_devices();
        assert!(!outs.is_empty(), "no output devices at all");
        let rep = default_devices_report().unwrap();
        if let Some(d) = rep.default_output {
            assert!(outs.contains(&d), "default output {d:?} missing from {outs:?}");
        }
        if let Some(d) = rep.default_input {
            let ins = list_input_devices();
            assert!(ins.contains(&d), "default input {d:?} missing from {ins:?}");
        }
    }

    fn virtual_output() -> Option<String> {
        list_output_devices()
            .into_iter()
            .find(|n| n.to_lowercase().starts_with("blackhole"))
    }

    #[test]
    fn start_on_opens_the_named_virtual_card() {
        let Some(card) = virtual_output() else {
            eprintln!("[audiohub] skip: no BlackHole output on this machine");
            return;
        };
        // Prefix form is the interesting one: "BlackHole" must land on the real
        // "BlackHole 2ch". Nothing is pushed, so the card only sees silence.
        let (pb, mut tx) = LivePlayback::start_on("BlackHole", 48000).unwrap();
        tx.push(&[0.0f32; 480]);
        std::thread::sleep(Duration::from_millis(50));
        drop(pb);
        assert!(card.to_lowercase().starts_with("blackhole"));
    }

    #[test]
    fn start_on_refuses_an_unknown_name_instead_of_using_the_default() {
        let e = err_of(LivePlayback::start_on("No Such Device 9x", 48000));
        assert!(e.contains("no output device matches"), "{e}");
        let e = err_of(LiveCapture::start_on("No Such Device 9x"));
        assert!(e.contains("no input device matches"), "{e}");
    }

    #[test]
    fn start_on_refuses_a_device_that_cannot_do_that_direction() {
        let outs = list_output_devices();
        let Some(input_only) = list_input_devices().into_iter().find(|n| !outs.contains(n)) else {
            eprintln!("[audiohub] skip: every input device is also an output");
            return;
        };
        let e = err_of(LivePlayback::start_on(&input_only, 48000));
        assert!(e.contains("no output device matches"), "{input_only:?}: {e}");
    }

    // ---- stream health (the seam the daemon watches)

    #[test]
    fn health_starts_alive_and_death_is_one_way() {
        let h = StreamHealth::new();
        assert!(h.is_alive());
        assert!(h.take_error().is_none());

        h.fail("output", &cpal::StreamError::DeviceNotAvailable);
        assert!(!h.is_alive(), "a reported stream error must kill the stream");
        let first = h.take_error().expect("the cause is kept, not just printed");
        assert!(first.contains("output stream error"), "{first}");

        // taking the message must not resurrect the stream, and the cause is
        // handed out exactly once
        assert!(!h.is_alive());
        assert!(h.take_error().is_none());
    }

    #[test]
    fn health_keeps_the_first_cause_not_the_fallout() {
        let h = StreamHealth::new();
        h.fail("input", &cpal::StreamError::DeviceNotAvailable);
        h.fail(
            "input",
            &cpal::StreamError::BackendSpecific {
                err: cpal::BackendSpecificError { description: "later fallout".into() },
            },
        );
        let msg = h.take_error().unwrap();
        assert!(!msg.contains("later fallout"), "{msg}");
        assert!(!h.is_alive());
    }

    #[test]
    fn health_is_visible_across_threads() {
        let h = StreamHealth::new();
        let w = Arc::clone(&h);
        std::thread::spawn(move || w.fail("output", &cpal::StreamError::DeviceNotAvailable))
            .join()
            .unwrap();
        assert!(!h.is_alive());
        assert!(h.take_error().unwrap().contains("stream error"));
    }

    #[test]
    fn a_freshly_opened_stream_reports_itself_alive() {
        let Some(card) = virtual_output() else {
            eprintln!("[audiohub] skip: no BlackHole output on this machine");
            return;
        };
        // Silence into a virtual card: nothing audible, no device state touched.
        let (pb, mut tx) = LivePlayback::start_on(&card, 48000).unwrap();
        tx.push(&[0.0f32; 480]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(pb.is_alive(), "a healthy stream must not look dead");
        assert!(pb.take_error().is_none());
        drop(pb);
    }

    // ---- watcher

    // ---- device inventory (uid / AudioObjectID)

    fn entry(id: u32, uid: &str, name: &str) -> DeviceEntry {
        DeviceEntry {
            name: name.to_string(),
            uid: Some(uid.to_string()),
            id: Some(id),
            is_input: false,
            is_output: true,
        }
    }

    #[test]
    fn detailed_listing_agrees_with_the_name_listings() {
        let all = list_devices_detailed();
        assert!(!all.is_empty(), "no audio devices at all");
        for (kind, names) in [
            (DeviceKind::Output, list_output_devices()),
            (DeviceKind::Input, list_input_devices()),
        ] {
            for n in names {
                let hit = all.iter().find(|d| d.name == n);
                let d = hit.unwrap_or_else(|| panic!("{kind:?} {n:?} missing from the detailed listing"));
                match kind {
                    DeviceKind::Output => assert!(d.is_output, "{n:?} listed as output but is_output=false"),
                    DeviceKind::Input => assert!(d.is_input, "{n:?} listed as input but is_input=false"),
                }
            }
        }
    }

    /// The whole point of reporting a UID is that it identifies ONE device.
    #[test]
    fn uids_are_unique_across_the_machine() {
        let all = list_devices_detailed();
        let mut seen: Vec<&String> = Vec::new();
        for uid in all.iter().filter_map(|d| d.uid.as_ref()) {
            assert!(!seen.contains(&uid), "two devices share the UID {uid:?}");
            seen.push(uid);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_uid_resolves_back_to_its_own_device() {
        for d in list_devices_detailed() {
            let Some(uid) = d.uid.as_deref() else { continue };
            for (kind, applies) in [
                (DeviceKind::Output, d.is_output),
                (DeviceKind::Input, d.is_input),
            ] {
                if !applies {
                    // asking the wrong direction must be refused, never coerced
                    assert!(device_name_for_uid(kind, uid).is_err(), "{uid:?} {kind:?}");
                    continue;
                }
                assert_eq!(device_name_for_uid(kind, uid).unwrap(), d.name, "{uid:?}");
                let (_, resolved) = find_device_by_uid(kind, uid)
                    .unwrap_or_else(|e| panic!("{uid:?} {kind:?} not findable: {e:#}"));
                assert_eq!(resolved, d.name, "{uid:?}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_unknown_uid_errors_and_lists_the_real_ones() {
        let e = err_of(find_device_by_uid(DeviceKind::Output, "NoSuchUID"));
        assert!(e.contains("no output device matches UID"), "{e}");
        // the message has to be usable for spotting a typo
        if let Some(real) = list_devices_detailed()
            .into_iter()
            .find(|d| d.is_output && d.uid.is_some())
        {
            assert!(e.contains(real.uid.as_deref().unwrap()), "{e}");
        }
        assert!(err_of(LivePlayback::start_on_uid("NoSuchUID", 48000))
            .contains("no output device matches UID"));
        assert!(err_of(LiveCapture::start_on_uid("NoSuchUID"))
            .contains("no input device matches UID"));
    }

    /// A UID is an opaque identifier, not a display string: the case-insensitive
    /// prefix leniency that makes NAMES typeable would here invent matches the
    /// system itself would never make.
    #[cfg(target_os = "macos")]
    #[test]
    fn uid_matching_is_exact_and_case_sensitive() {
        let Some(d) = list_devices_detailed().into_iter().find(|d| {
            d.is_output && d.uid.as_deref().map_or(false, |u| u.chars().any(|c| c.is_alphabetic()))
        }) else {
            eprintln!("[audiohub] skip: no output device with an alphabetic UID");
            return;
        };
        let uid = d.uid.unwrap();
        assert_eq!(device_name_for_uid(DeviceKind::Output, &uid).unwrap(), d.name);
        for bad in [uid.to_uppercase(), uid.to_lowercase()] {
            if bad == uid {
                continue;
            }
            assert!(device_name_for_uid(DeviceKind::Output, &bad).is_err(), "{bad:?}");
        }
        // a prefix is not a match either
        assert!(device_name_for_uid(DeviceKind::Output, &uid[..uid.len() - 1]).is_err());
    }

    // ---- device list event stream

    #[cfg(target_os = "macos")]
    fn watch_state(known: Vec<DeviceEntry>) -> (WatchState, mpsc::Receiver<DeviceEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            WatchState {
                known,
                events: Vec::new(),
                cb: Box::new(move |e: &DeviceEvent| {
                    let _ = tx.send(e.clone());
                }),
                start: Instant::now(),
            },
            rx,
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn diff_reports_additions_and_removals_once_each() {
        let (mut st, rx) = watch_state(vec![entry(40, "A", "Card A"), entry(41, "B", "Card B")]);
        st.diff_against(vec![entry(41, "B", "Card B"), entry(42, "C", "Card C")]);
        let kinds: Vec<(&str, Option<String>)> =
            st.events.iter().map(|e| (e.kind, e.uid.clone())).collect();
        assert_eq!(
            kinds,
            vec![
                ("removed", Some("A".into())),
                ("added", Some("C".into()))
            ]
        );
        // the live callback saw exactly the same events
        assert_eq!(rx.try_iter().count(), 2);

        // a second diff over an unchanged list must add nothing: an event is
        // reported once, when it happens, not on every notification after it
        st.diff_against(vec![entry(41, "B", "Card B"), entry(42, "C", "Card C")]);
        assert_eq!(st.events.len(), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_rename_is_not_churn_but_a_new_object_id_is() {
        let (mut st, _rx) = watch_state(vec![entry(40, "A", "Living Room Mac")]);
        st.diff_against(vec![entry(40, "A", "Living Room Mac (offline)")]);
        assert!(st.events.is_empty(), "a rename must not read as add/remove");

        // same UID, new AudioObjectID = the device really was re-created
        st.diff_against(vec![entry(77, "A", "Living Room Mac")]);
        let kinds: Vec<&str> = st.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec!["removed", "added"]);
        assert_eq!(st.events[0].id, Some(40));
        assert_eq!(st.events[1].id, Some(77));
    }

    /// Proving a NEGATIVE is the whole job: a window in which nothing happens
    /// must succeed with zero events, and must leave no listener behind.
    #[cfg(target_os = "macos")]
    #[test]
    fn watching_an_unchanged_window_yields_no_events() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let events = watch_device_list(
            0.3,
            Box::new(move |_: &DeviceEvent| {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .expect("registering the device-list listener");
        assert!(events.is_empty(), "unexpected device churn: {events:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn fanout_coalesces_and_stops() {
        let f = Fanout::new();
        f.signal();
        f.signal();
        assert!(f.wait());
        f.stop();
        assert!(!f.wait(), "stop must win once set");
    }

    #[test]
    fn watcher_registers_and_delivers_off_the_platform_thread() {
        for kind in [DeviceKind::Output, DeviceKind::Input] {
            let (tx, rx) = mpsc::channel::<std::thread::ThreadId>();
            let w = DeviceChangeWatcher::start(
                kind,
                Box::new(move || {
                    let _ = tx.send(std::thread::current().id());
                }),
            )
            .unwrap_or_else(|e| panic!("{kind:?} watcher: {e:#}"));
            w.test_signal();
            let cb_thread = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("{kind:?} callback never ran: {e}"));
            assert_ne!(cb_thread, std::thread::current().id());
            drop(w); // deregisters + joins; a second drop path must not exist
        }
    }

    #[test]
    fn watcher_callback_cannot_run_after_drop() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let w = DeviceChangeWatcher::start(
            DeviceKind::Output,
            Box::new(move || {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        w.test_signal();
        std::thread::sleep(Duration::from_millis(100));
        drop(w);
        let after = hits.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hits.load(Ordering::SeqCst), after);
        assert!(after >= 1);
    }
}

/// 跨时钟速率伺服的验收：**两个不同速率的时钟，跑足够长的模拟时间**。
///
/// # 纪律：对照组必须是「修复前」，而不是一段注释
///
/// 每一条收敛断言都配一条**同参数、只关掉伺服**的发散断言。关掉伺服的那条
/// 走的是与本次改动之前逐字相同的代码路径（`resampler: None` + 直接
/// `push_slice`），所以它复现的就是病本身。只断言「打开之后收敛」证明不了
/// 任何事——一个恒等于 target 的假读数也能让它绿。
///
/// # 时间怎么造
///
/// 全程用**虚拟时间**：`t0 + Duration`。`AudioTx::push_at` /
/// `PlayRingSink::drain_at` 收 `Instant`，所以一小时的漂移在几秒里跑完，而
/// 走的仍是生产路径、真的 `HeapRb`、真的重采样器、真的 `Dll`。
///
/// # 两个时钟怎么错开
///
/// - **写侧**（= `mixer_loop`）：每 10 ms 虚拟时间推 480 个样本，严格
///   48000 样本/虚拟秒。这是「mac 的发送节拍经 JB 定拍之后的本地 tick」。
/// - **读侧**（= 声卡回调）：每 `512 / (48000·(1+ε))` 秒取 512 帧。ε 就是
///   晶振失配。**回调周期（≈10.67 ms）与 mixer tick（10 ms）故意不整除**，
///   这样深度读数带着真实的相位锯齿，时间插值那一项才有活干。
///
/// `AudioTx` 只知道标称 48000，ε 对它不可见——这正是现实里的信息结构。
#[cfg(test)]
mod rate_servo {
    use super::*;

    /// 一个 mixer tick 的样本数 @48k。
    const F: usize = 480;
    /// 声卡一次回调取多少帧（典型的 512 帧周期）。
    const CB: usize = 512;

    struct Sim {
        tx: AudioTx,
        sink: PlayRingSink,
        t0: Instant,
        now_ns: u64,
        /// 下一次声卡回调的时刻。用 f64 累加，别用整数——按周期截断会给声卡
        /// 掺进一个我们没打算注入的额外 ppm，正好污染这套测试要量的东西。
        next_cb_ns: f64,
        last_cb_ns: f64,
        /// 声卡的真实周期（纳秒）——晶振失配全部体现在这里。
        cb_period_ns: f64,
        /// 是否给声卡缓冲滞后（`outputBufferDacTime`）一个非零值。
        dac_lag: Option<Duration>,
        /// **观测量**：上一个 tick 里、在 push 之前测到的执行器下游缓冲量（ms）。
        ///
        /// 不能直接拿 `queued()` 当观测量：写侧 10 ms 一块、读侧 10.67 ms 一块，
        /// 裸深度带着一个 ±512 样本（±10.7 ms）的相位锯齿，再加上刚推进去还没
        /// 被取走的那一帧（+10 ms）。按分钟采样会把这个锯齿混叠成一条假的
        /// 「缓慢下降然后跳回」的锯齿波，跟真的漂移长得一模一样。
        /// 这里按与生产代码相同的口径（深度 − 距上次回调的应耗量）测，
        /// 量的才是环路真正在控的那个量。
        downstream_ms: f64,
    }

    impl Sim {
        /// `ppm > 0` = 声卡晶振**快**于我们的写侧（环会见底）；
        /// `ppm < 0` = 声卡**慢**（环会涨，直到 drop-newest）。
        fn new(ppm: f64, servo: bool) -> Sim {
            let (tx, sink) = if servo {
                AudioTx::detached_for_test_with_servo(48_000)
            } else {
                AudioTx::detached_for_test(48_000)
            };
            Sim {
                tx,
                sink,
                t0: Instant::now(),
                now_ns: 0,
                next_cb_ns: 0.0,
                last_cb_ns: 0.0,
                cb_period_ns: CB as f64 / (48_000.0 * (1.0 + ppm * 1e-6)) * 1e9,
                dac_lag: None,
                downstream_ms: 0.0,
            }
        }

        fn at(&self, ns: u64) -> Instant {
            self.t0 + Duration::from_nanos(ns)
        }

        /// 只放声卡，不推——用来造「写侧停摆」。
        fn run_device_until(&mut self, end_ns: u64) {
            while self.next_cb_ns <= end_ns as f64 {
                let t = self.at(self.next_cb_ns as u64);
                self.sink.drain_at(CB, t, self.dac_lag);
                self.last_cb_ns = self.next_cb_ns;
                self.next_cb_ns += self.cb_period_ns;
            }
            self.now_ns = end_ns;
        }

        /// 跑一个 10 ms 的 mixer tick：先把这段虚拟时间里到期的声卡回调放掉，
        /// 再推一帧。顺序与现实一致（声卡是独立线程，不等我们）。
        fn tick(&mut self) {
            self.run_device_until(self.now_ns + 10_000_000);
            let inflight =
                (self.now_ns as f64 - self.last_cb_ns) * 48_000.0 / 1e9;
            let dac = self.dac_lag.map_or(0.0, |d| d.as_secs_f64() * 48_000.0);
            self.downstream_ms = (self.tx.queued() as f64 - inflight + dac) / 48.0;
            self.tx.push_at(&[0.25f32; F], self.at(self.now_ns));
        }

        /// 环里此刻的裸样本数（只在需要「真的见底了吗」这种问题时用）。
        fn queued(&self) -> u32 {
            self.tx.queued()
        }

        /// 跑 `minutes` 分钟虚拟时间，每分钟采一次观测量（ms）。
        fn run(&mut self, minutes: u64) -> Vec<f64> {
            let mut trace = Vec::new();
            for _ in 0..minutes {
                for _ in 0..6_000 {
                    self.tick();
                }
                trace.push(self.downstream_ms);
            }
            trace
        }
    }

    /// 现实里最坏的一对晶振：两块各 ±100 ppm 的消费级件反向叠加。
    /// 50 ppm（典型值）同样发散，只是要跑 4 倍长的模拟时间才看得出同样的量。
    const PPM: f64 = 200.0;
    /// 模拟时长。200 ppm × 20 min = 11 520 样本 = **240 ms** 的应有漂移，
    /// 与 30 ms 的目标水位差一个数量级，绿/红一眼可判。
    const MINUTES: u64 = 20;

    // ============================================================== 注入 E-1
    //
    // **修复前的行为**：声卡晶振慢 200 ppm，写多读少，水位单调爬升。
    // 这一条**必须先绿**，注入 E-2 才有意义。

    #[test]
    fn injection_e1_without_the_servo_a_slow_device_crystal_fills_the_ring_forever() {
        let mut s = Sim::new(-PPM, false);
        let trace = s.run(MINUTES);
        // 单调不降，且末值 ≈ 理论漂移量。
        assert!(
            trace.windows(2).all(|w| w[1] >= w[0]),
            "无伺服 ⇒ 水位只涨不落，got {trace:?}"
        );
        let expect = 1000.0 * (MINUTES * 60) as f64 * PPM * 1e-6; // ms
        let last = *trace.last().unwrap();
        assert!(
            (last - expect).abs() < expect * 0.1,
            "{MINUTES} 分钟 × {PPM} ppm 应涨约 {expect:.0} ms，实测 {last:.0} ms"
        );
        // 20 分钟只是它跑到饱和路上的一段。1.000 秒的环按这个斜率约 83 分钟
        // 灌满，之后就是 drop-newest —— 「迟到 + 周期性断续」。
        assert!(last < 1000.0, "20 分钟还没到饱和，这条测的是斜率不是封顶");
    }

    // ============================================================== 注入 E-2
    //
    // **修复后**：同样的两个时钟，伺服打开 ⇒ 水位收敛到目标并**待在那儿**。

    #[test]
    fn injection_e2_the_servo_holds_the_ring_at_target_against_a_slow_crystal() {
        let mut s = Sim::new(-PPM, true);
        let trace = s.run(MINUTES);
        let target_ms = s.tx.servo_target() as f64 / 48.0;
        for (i, &d) in trace.iter().enumerate() {
            assert!(
                (d - target_ms).abs() < 5.0,
                "第 {} 分钟水位 {d:.1} ms，目标 {target_ms:.1} ms（全程 {trace:?}）",
                i + 1
            );
        }
        // 稳态修正应当≈把失配补掉：设备慢 200 ppm ⇒ 我们要少写 200 ppm。
        let corr = s.tx.servo_corr_ppm();
        assert!(
            (corr + PPM).abs() < 40.0,
            "稳态修正应 ≈ {:.0} ppm，实测 {corr:.0} ppm",
            -PPM
        );
    }

    // ============================================================== 注入 E-3
    //
    // 反向：声卡晶振**快**。无伺服 ⇒ 环见底、声卡一直补静音（听感是持续
    // 断续）；有伺服 ⇒ 撑住目标。这一条钉的是「双向」——mac 侧只需要单向削，
    // 这里深了要削、浅了要补。

    #[test]
    fn injection_e3_without_the_servo_a_fast_device_crystal_starves_the_ring() {
        let mut s = Sim::new(PPM, false);
        // 先垫 400 ms，好让 20 分钟的亏空（240 ms）全程可见而不撞到 0 —— 撞到
        // 0 之后水位就被环底钳住，量到的是钳位不是漂移。
        s.tx.push_at(&vec![0.25f32; 48_000 * 2 / 5], s.at(0));
        let trace = s.run(MINUTES);
        assert!(
            trace.windows(2).all(|w| w[1] < w[0]),
            "无伺服 ⇒ 水位只落不涨，got {trace:?}"
        );
        let drop = trace[0] - *trace.last().unwrap();
        let expect = 1000.0 * ((MINUTES - 1) * 60) as f64 * PPM * 1e-6;
        assert!(
            (drop - expect).abs() < expect * 0.1,
            "应当掉约 {expect:.0} ms，实测 {drop:.0} ms（全程 {trace:?}）"
        );
        // 照这个斜率，垫进去的 400 ms 约 33 分钟见底，之后声卡永远在补静音。
    }

    #[test]
    fn injection_e3_the_servo_refills_against_a_fast_crystal_and_bends_the_other_way() {
        let mut s = Sim::new(PPM, true);
        let trace = s.run(MINUTES);
        let target_ms = s.tx.servo_target() as f64 / 48.0;
        for (i, &d) in trace.iter().enumerate() {
            assert!(
                (d - target_ms).abs() < 5.0,
                "第 {} 分钟水位 {d:.1} ms，目标 {target_ms:.1} ms（全程 {trace:?}）",
                i + 1
            );
        }
        let corr = s.tx.servo_corr_ppm();
        assert!(
            (corr - PPM).abs() < 40.0,
            "设备快 ⇒ 修正必须为**正**（多写），应 ≈ +{PPM:.0} ppm，实测 {corr:.0} ppm"
        );
    }

    // ============================================================== 符号
    //
    // 调研点名的那一处：「误差符号是落地时最容易写错、且错了会直接把水位推到
    // 饱和的一处」。

    #[test]
    fn error_sign_is_negative_feedback_flipping_it_diverges() {
        // 正确符号（`err = downstream − target`，生产代码那一行）：收敛。
        let mut ok = Sim::new(-PPM, true);
        let ok_trace = ok.run(5);
        let target_ms = ok.tx.servo_target() as f64 / 48.0;
        assert!(
            (ok_trace.last().unwrap() - target_ms).abs() < 5.0,
            "生产符号必须收敛到目标，got {ok_trace:?}"
        );
        assert!(
            ok.tx.servo_corr_ppm() > -PlayServo::MAX_PPM + 1.0,
            "生产符号下修正不该贴着钳位，实测 {:.0} ppm",
            ok.tx.servo_corr_ppm()
        );

        // 取反（`err = target − downstream`，也就是把 mac 侧 tx_loop 的
        // capture 语义照抄过来）：正反馈。水位偏深 ⇒ err<0 ⇒ corr>1 ⇒ 写得
        // 更多 ⇒ 更深。修正一路推到 +500 ppm 钳位，水位比无伺服还涨得快。
        let mut bad = Sim::new(-PPM, true);
        bad.tx.servo_invert_error_for_test();
        let bad_trace = bad.run(5);
        assert!(
            bad.tx.servo_corr_ppm() > PlayServo::MAX_PPM - 1.0,
            "符号反了必须缠绕到 +500 ppm 钳位，实测 {:.0} ppm",
            bad.tx.servo_corr_ppm()
        );
        assert!(
            *bad_trace.last().unwrap() > ok_trace.last().unwrap() + 50.0,
            "符号反了必须显著发散：正确 {ok_trace:?} vs 反了 {bad_trace:?}"
        );
    }

    // ============================================================== 钳位

    #[test]
    fn the_correction_is_clamped_at_500_ppm_even_under_an_absurd_mismatch() {
        // 3000 ppm：远超任何真实晶振对，环路会一路顶到钳位并停在那儿。
        let mut s = Sim::new(-3_000.0, true);
        let mut worst: f64 = 0.0;
        for _ in 0..5 {
            for _ in 0..6_000 {
                s.tick();
                worst = worst.max(s.tx.servo_corr_ppm().abs());
            }
        }
        assert!(
            worst <= PlayServo::MAX_PPM + 1e-6,
            "修正冲出了 ±500 ppm：{worst:.1} ppm"
        );
        assert!(
            worst > PlayServo::MAX_PPM - 1.0,
            "3000 ppm 失配下应当**顶到**钳位，实测只有 {worst:.1} ppm"
        );
        // 顶到钳位仍然吐不完 3000 ppm，剩下的归深端硬跳；这里只钉「钳位守住了」。
        assert!(
            play_servo_counters().clamped > 0,
            "钳位必须被计数，否则现场无从判断失配是否超出可校正范围"
        );
    }

    // ============================================================== 时间插值

    /// 深度读数的相位锯齿必须在喂进环路之前被剔掉（zita 的
    /// `(_k_a1−_k_a0)·d1/d2`、PipeWire 的 `time_since_nsec`）。
    ///
    /// 造法：让声卡周期与 mixer tick 严重不整除（这里 CB=512 @48k ≈ 10.67 ms
    /// vs 10 ms），于是**裸** `queued()` 每 tick 在 ±512 样本（±10.7 ms）之间
    /// 跳。若不做插值，这个锯齿会整个灌进积分器。
    #[test]
    fn time_interpolation_removes_the_callback_phase_sawtooth() {
        let mut s = Sim::new(0.0, true);
        for _ in 0..12_000 {
            s.tick();
        }
        // 裸读数的锯齿幅度：连续 200 个 tick 里最大与最小的差。
        let mut raw: Vec<f64> = Vec::new();
        for _ in 0..200 {
            s.tick();
            raw.push(s.tx.queued() as f64);
        }
        let spread = raw.iter().cloned().fold(f64::MIN, f64::max)
            - raw.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            spread > CB as f64 * 0.5,
            "这条测试的前提是裸读数真的在跳，实测跨度只有 {spread:.0} 样本"
        );
        // 而环路看到的误差被插值压平：稳态误差远小于锯齿幅度。
        // 用**每实例**的读数，不用进程级那份——后者是「最后写者赢」，
        // 并行跑的兄弟测试会把它覆盖掉。
        let err = s.tx.servo_last_err().abs();
        assert!(
            err < CB as f64 * 0.5,
            "插值之后的误差应当远小于 {} 样本的锯齿，实测 {err:.0}",
            CB
        );
    }

    // ============================================================== 声卡缓冲项

    /// `outputBufferDacTime` 是执行器下游的一级，必须计入误差——不计入的话
    /// 稳态水位会恰好高出这一项，也就是 Snapcast `stream.cpp:305` 那条先例
    /// 在防的事。
    #[test]
    fn the_dac_buffer_counts_as_downstream_buffering() {
        let mut with_lag = Sim::new(0.0, true);
        with_lag.dac_lag = Some(Duration::from_millis(10)); // 10 ms = 480 样本
        with_lag.run(10);
        let mut without = Sim::new(0.0, true);
        without.run(10);
        // 环路控的是「下游总量」，两边都该落在同一个目标上……
        let d = (without.downstream_ms - with_lag.downstream_ms).abs();
        assert!(d < 3.0, "下游总量必须同样收敛到目标，实测差 {d:.1} ms");
        // ……而声卡里压着的那 10 ms 就实实在在从环里让出来了。
        // 不把它算进误差的话，这 10 ms 会白白叠在总延迟上。
        let ring = (without.queued() as f64 - with_lag.queued() as f64) / 48.0;
        assert!(
            (ring - 10.0).abs() < 3.0,
            "声卡里压着 10 ms ⇒ 环里就该少 10 ms（总量守恒），实测差 {ring:.1} ms"
        );
    }

    // ============================================================== 目标水位

    /// 目标水位跟着**实测的声卡周期**走，而且**上下都跟**。
    ///
    /// 造法：开流时来一次超大的预热回调（8192 帧），之后回到正常的 512。
    /// 若目标是「只涨不落」，那一次异常就会把播放环的稳态延迟永久顶到
    /// 120 ms 的上限——一个由单次异常制造的永久性延迟棘轮，正是本项目在治的病。
    #[test]
    fn one_outlier_callback_cannot_ratchet_the_target_up_forever() {
        let mut s = Sim::new(0.0, true);
        // 预热：一次 8192 帧的大回调。
        s.sink.drain_at(8_192, s.at(0), None);
        s.tick();
        let spiked = s.tx.servo_target();
        assert!(
            spiked as f64 / 48.0 > 100.0,
            "先确认异常回调真的把目标顶起来了：{:.0} ms",
            spiked as f64 / 48.0
        );
        // 之后一切正常，异常值应当在一个滑动窗（≈1.3 秒）之后老化掉。
        for _ in 0..500 {
            s.tick();
        }
        let settled = s.tx.servo_target();
        assert_eq!(
            settled,
            2 * CB as u32 + 480,
            "异常值必须老化掉，目标该回到 512 帧那一档（≈31 ms），实测 {:.0} ms",
            settled as f64 / 48.0
        );

        // 水位跟着目标**回落**，但回落是慢的 —— 而且这个「慢」是 ±500 ppm 钳位
        // 的直接推论，不是缺陷：多出来的约 87 ms 只能按 0.5 ms/s 吐，需要约
        // 174 秒。这条断言把这个时间常数**钉成一条被测性质**，免得将来有人
        // 看到「目标降了水位没跟上」以为是 bug。
        //
        // 为什么不给「目标下调」也配一次硬跳：硬跳是要丢样本的（可闻），
        // 而这里的超额是我们**自己**改设定值造成的，不是链路故障。为一次内部
        // 参数变化制造一次可闻的跳进，代价和收益不成比例。
        let before_bleed = s.downstream_ms;
        assert!(before_bleed > 90.0, "先确认水位确实被顶到了高位：{before_bleed:.0} ms");
        for _ in 0..30_000 {
            s.tick(); // 300 秒
        }
        assert!(
            (s.downstream_ms - settled as f64 / 48.0).abs() < 8.0,
            "300 秒内必须吐回目标：{:.0} ms vs 目标 {:.0} ms（起点 {before_bleed:.0} ms）",
            s.downstream_ms,
            settled as f64 / 48.0
        );
    }

    /// 目标水位由声卡周期决定：512 帧的设备与 1024 帧的设备该拿到不同的目标，
    /// 而不是一个写死的常数。
    #[test]
    fn the_target_is_two_device_periods_plus_a_tick() {
        let mut s = Sim::new(0.0, true);
        for _ in 0..50 {
            s.tick();
        }
        // CB=512 ⇒ 2·512 + 480 = 1504 样本 = 31.3 ms。
        assert_eq!(s.tx.servo_target(), 2 * CB as u32 + 480);
    }

    // ============================================================== 重采样核

    /// `LinearResampler` 到底够不够用：够，但**相位扫过时高频会呼吸**。
    ///
    /// 造法：把同一段 10 kHz 正弦分别按 φ=0（恒等）与 φ=0.5（最坏相位）取样，
    /// 量幅度。比率≈1 的伺服会让 φ 缓慢扫过 [0,1)，于是这两个数之间的差就是
    /// 高频包络的起伏深度。
    #[test]
    fn cubic_beats_linear_on_the_phase_swept_hf_droop() {
        const N: usize = 4_096;
        let f = 10_000.0f64;
        let sr = 48_000.0f64;
        let x: Vec<f32> = (0..N)
            .map(|n| (2.0 * std::f64::consts::PI * f * n as f64 / sr).sin() as f32)
            .collect();
        // 半样本延迟下的幅度（取中段，避开两端的历史填充暂态）。
        let amp = |y: &[f32]| -> f64 {
            y[N / 4..N * 3 / 4]
                .iter()
                .fold(0.0f64, |m, &v| m.max(v.abs() as f64))
        };
        let half = |mut shift: Box<dyn FnMut(&[f32]) -> Vec<f32>>| -> f64 { amp(&shift(&x)) };

        let linear = half(Box::new(|x: &[f32]| {
            // dsp::LinearResampler 的核：y = x0 + (x1−x0)·0.5
            x.windows(2).map(|w| w[0] + (w[1] - w[0]) * 0.5).collect()
        }));
        let cubic = half(Box::new(|x: &[f32]| {
            // VarResampler 的核在 t=0.5 处：[−1/16, 9/16, 9/16, −1/16]
            x.windows(4)
                .map(|w| -0.0625 * w[0] + 0.5625 * w[1] + 0.5625 * w[2] - 0.0625 * w[3])
                .collect()
        }));
        let db = |g: f64| 20.0 * g.log10();
        // 实测：linear ≈ −2.0 dB，cubic ≈ −0.5 dB @10 kHz。
        assert!(db(linear) < -1.5, "线性插值在 φ=0.5 处应衰减约 2 dB，实测 {:.2} dB", db(linear));
        assert!(
            db(cubic) > db(linear) + 1.0,
            "三次核必须明显平于线性：cubic {:.2} dB vs linear {:.2} dB",
            db(cubic),
            db(linear)
        );
    }

    /// 比率恰为 1 且不修正时，`VarResampler` 是**逐样本无损**的（只差固定的
    /// 两样本延迟）——不然「伺服关着也要过一遍插值」就是白白掉音质。
    #[test]
    fn the_cubic_kernel_is_lossless_at_unity_ratio() {
        let mut rs = VarResampler::new(48_000, 48_000);
        let x: Vec<f32> = (0..1_000).map(|n| ((n * 37) % 101) as f32 / 101.0 - 0.5).collect();
        let mut y = Vec::new();
        rs.process(&x[..500], &mut y);
        rs.process(&x[500..], &mut y);
        assert_eq!(y.len(), x.len(), "1:1 ⇒ 样本数一一对应");
        for (i, (&a, &b)) in x[..x.len() - 2].iter().zip(&y[2..]).enumerate() {
            assert!((a - b).abs() < 1e-6, "第 {i} 个样本 {a} != {b}");
        }
    }

    /// 分块处理必须等价于整块处理：跨块的历史与相位都得带过去。
    #[test]
    fn the_cubic_kernel_is_block_split_invariant() {
        let x: Vec<f32> = (0..3_000).map(|n| (n as f32 * 0.017).sin()).collect();
        let mut whole = Vec::new();
        VarResampler::new(48_000, 44_100).process(&x, &mut whole);
        let mut split = Vec::new();
        let mut rs = VarResampler::new(48_000, 44_100);
        for chunk in x.chunks(480) {
            rs.process(chunk, &mut split);
        }
        assert_eq!(whole.len(), split.len());
        for (i, (&a, &b)) in whole.iter().zip(&split).enumerate() {
            assert!((a - b).abs() < 1e-5, "第 {i} 个样本 {a} != {b}");
        }
    }

    // ============================================================== 硬跳

    /// 一次性灌进 800 ms（注入 A 的病理）之后，伺服必须把它**排掉**而不是
    /// 靠 500 ppm 慢慢吐（那要 1600 秒）。这就是 PipeWire 的 `alsa_sync` /
    /// zita 的 `rd_commit(k)`。
    #[test]
    fn a_huge_backlog_is_resynced_not_bled_off_at_500_ppm() {
        let mut s = Sim::new(0.0, true);
        for _ in 0..100 {
            s.tick(); // 先锁定
        }
        s.tx.push_at(&vec![0.25f32; 48_000 * 4 / 5], s.at(s.now_ns)); // 灌 800 ms
        assert!(s.queued() as f64 / 48.0 > 700.0, "起点得真的是 800 ms 量级");
        for _ in 0..3_000 {
            s.tick(); // 30 秒
        }
        let target_ms = s.tx.servo_target() as f64 / 48.0;
        assert!(
            (s.downstream_ms - target_ms).abs() < 8.0,
            "30 秒内必须跳回目标 {target_ms:.0} ms，实测 {:.0} ms（纯靠 500 ppm 要 1600 秒）",
            s.downstream_ms
        );
        assert!(
            play_servo_counters().resync_skipped > 0,
            "硬跳丢掉的样本必须被计数 —— 这是一次可闻的跳进，不能静默"
        );
    }

    /// 一次长欠载（写侧停摆 2 秒）之后，伺服必须把水位**补**回目标，
    /// 而不是留在底部反复欠载。这是「双向」的另一半。
    #[test]
    fn a_long_underrun_is_expanded_back_to_target() {
        let mut s = Sim::new(0.0, true);
        for _ in 0..100 {
            s.tick();
        }
        // 写侧停摆 2 秒：只放声卡，不推。
        let stall_to = s.now_ns + 2_000_000_000;
        s.run_device_until(stall_to);
        assert_eq!(s.queued(), 0, "停摆 2 秒 ⇒ 环见底");
        for _ in 0..50 {
            s.tick();
        }
        let target_ms = s.tx.servo_target() as f64 / 48.0;
        assert!(
            (s.downstream_ms - target_ms).abs() < 8.0,
            "补回目标 {target_ms:.0} ms，实测 {:.0} ms",
            s.downstream_ms
        );
        assert!(
            play_servo_counters().resync_padded > 0,
            "补进去的静音必须被计数"
        );
    }

    // ============================================================== 控制律

    /// `spa_dll` 的移植正确性：方向对、且**积分项保持稳态速率偏置**。
    ///
    /// 后半句是这个环能用的全部理由，别写成「误差归零后修正衰减回 1.0」——
    /// 那是纯比例环的性质，而纯比例环有稳态误差：晶振差 200 ppm，它就只能
    /// 停在一个恒定偏深/偏浅的水位上。`z3 += w2·z2` 是一个**不衰减**的积分器，
    /// 正是它让「误差为零」与「修正为 −200 ppm」可以同时成立。
    #[test]
    fn the_dll_holds_a_rate_bias_at_zero_error() {
        // err > 0（水位偏深）⇒ corr < 1 ⇒ 少写 ⇒ 水位降。
        assert!(Dll::new(Dll::BW_MAX, 480, 48_000).update(100.0) < 1.0);
        // err < 0（水位偏浅）⇒ corr > 1 ⇒ 多写。
        assert!(Dll::new(Dll::BW_MAX, 480, 48_000).update(-100.0) > 1.0);

        let mut d = Dll::new(Dll::BW_MAX, 480, 48_000);
        for _ in 0..200 {
            d.update(100.0);
        }
        let learned = d.update(0.0);
        assert!(learned < 1.0, "学到的偏置方向该是「少写」，got {learned}");
        // 比例项（z2）按 `1−w0` 每步几何衰减，两万步之后早已归零；此后读到的
        // 就是纯积分项。再跑两万步它必须**一动不动**——纯比例环这里会回到
        // 精确的 1.0，那样的环有稳态误差，撑不住一个恒定的晶振失配。
        for _ in 0..20_000 {
            d.update(0.0);
        }
        let a = d.update(0.0);
        for _ in 0..20_000 {
            d.update(0.0);
        }
        let b = d.update(0.0);
        assert!(
            (a - b).abs() < 1e-12,
            "积分项必须不衰减：第 2 万步 {a}，第 4 万步 {b}"
        );
        assert!(
            a < 1.0 - 1e-6,
            "零误差下偏置必须还在（刚学到 {learned}，静置之后 {a}）"
        );
        // 反向误差能把它解开——否则积分器就是个只进不出的陷阱。
        for _ in 0..4_000 {
            d.update(-100.0);
        }
        assert!(d.update(0.0) > 1.0, "反向误差必须能把偏置翻过去");
    }

    /// `reset` 必须真的清干净——粗调之后不清，积分器残留会在跳变后继续输出
    /// 错误修正（PipeWire `node-driver.c:487–494` / PulseAudio `fast_adjust`）。
    #[test]
    fn resetting_the_dll_clears_the_integrators() {
        let mut d = Dll::new(Dll::BW_MAX, 480, 48_000);
        for _ in 0..500 {
            d.update(2_000.0);
        }
        assert!((d.update(0.0) - 1.0).abs() > 1e-6, "先确认它确实积了东西");
        d.reset();
        assert_eq!(d.update(0.0), 1.0, "reset 之后第一次零误差更新必须给出 1.0");
    }
}
