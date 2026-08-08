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
/// and the setters fail; that is the documented trigger for the plan §7.2
/// software-gain fallback — see [`authority_for`].
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

// ------------------------------------------- plan §7.2 software-gain fallback

/// Who actually applies the volume for a spk stream this side drives (plan §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAuthority {
    /// The default, and the only path with untouched audio quality: the peer's
    /// real device has a volume we can drive, so the wire stays full-scale and
    /// the scalar travels as a `VolumeSet` control message.
    Peer,
    /// plan §7.2's documented exception: the peer's device exposes no writable
    /// volume (aggregate devices, most HDMI/optical outs), so the consumer's
    /// virtual device self-manages and the scalar is applied as SEND-side
    /// software gain. The wire then carries volume — which is the one case where
    /// the bit depth matters, see `dsp::SendGain`.
    SendGain,
}

/// `last` is the peer's most recent `VolumeState` report; `None` = nothing heard
/// from it yet.
///
/// **No report means no fallback.** Engaging on `None` would attenuate audio on
/// the strength of a fact that has not arrived; the provider reports once a
/// second, so the cost of waiting is bounded, and the switch itself is a 20 ms
/// gain ramp. Guessing the other way is not bounded: a stream that opens
/// straight into the fallback multiplies every sample by a scalar nobody has
/// agreed on yet.
///
/// This answers only "what did the peer's device say". Whether the fallback may
/// run at all is a separate question the daemon asks alongside it — §7.2 gives
/// it to mode B, whose virtual device is the thing doing the self-managing.
pub fn authority_for(last: Option<VolumeState>) -> VolumeAuthority {
    match last {
        Some(v) if !v.adjustable => VolumeAuthority::SendGain,
        _ => VolumeAuthority::Peer,
    }
}

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

// -------------------------------------------- plan §7.1 mode A: two switches

/// The two independent mode-A options (plan §7.1), carried as ONE value.
///
/// They are separate switches with separate meanings, but they are not
/// independent at the point of use: §7.1's exception says 「静音本机」 suppresses
/// the *mute* half of 「与对端音量同步」 while leaving the volume half alone.
/// A call site that could read one without the other is a call site that can
/// implement half the rule, which is how that exception goes missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeAVolume {
    /// 「与对端音量同步」. This machine's system output follows the peer's real
    /// output device, and vice versa. **Peer authoritative**: an inbound
    /// reading is written here unconditionally, which is what makes plan §7.1's
    /// 「双向同时变更冲突时对端值覆盖本机」 true without any timestamp, sequence
    /// number or arbitration rule — the last thing the peer said always lands.
    pub sync: bool,
    /// 「静音本机输出」. Mute this machine's output ONCE, at the moment a
    /// speaker stream to the peer is established, and never maintain it: plan
    /// §7.1 reads a later unmute as "this machine should sound too" and
    /// respects it until the next stream comes up.
    pub mute_local: bool,
}

/// A volume change to write somewhere, or to put on the wire.
///
/// `muted: None` is the same "leave the mute control alone" that
/// `SessionMsg::VolumeSet` uses, and it is how §7.1's exception is expressed:
/// with 「静音本机」 on, both directions carry a scalar and no mute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeWrite {
    pub scalar: f32,
    pub muted: Option<bool>,
}

/// What mode-A volume following must do with one reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FollowAction {
    /// Do nothing; the payload is the reason, safe to log.
    Ignore(&'static str),
    Apply(VolumeWrite),
}

/// plan §7.1 mode A 「与对端音量同步」 — **one rule, both directions**.
///
/// §7.1 says the two outputs 「互相跟随」, so the same admission and the same
/// mute exception have to hold whichever way a reading is travelling. Fed the
/// peer's reading it says what to write on this machine; fed this machine's own
/// reading it says what to send to the peer. Writing the two directions as two
/// functions is how they drift, and a mute exception that holds in only one
/// direction still mutes the peer.
///
/// Gates, in the order their reasons differ for the operator:
///
/// - `is_spk_consumer`: only the consumer of a speaker stream has a peer whose
///   *output* device this is about. On a stream we provide, the output device
///   is ours and the peer's slider already drives it through `classify_set`;
///   following there would close the two into a loop.
/// - `mode_is_a`: this is a mode-A option. In mode B the virtual speaker is the
///   volume control (§7.2) and in share mode this machine is not a consumer at
///   all, so in both cases "make the system output follow a peer" would be a
///   change nobody asked for on a device they are using for something else.
/// - `opt.sync`: the switch itself, off by default.
/// - `state.adjustable`: see below. It is the gate that keeps the two outputs
///   from following each other into silence.
///
/// # Why `adjustable == false` means "no reading", not "a reading of zero"
///
/// On a device with no volume control there is nothing for `scalar` to be read
/// from, and every backend has to put *something* in the field: [`imp::get`]
/// averages whatever channel elements exist, and on a macOS aggregate device
/// there are none, so it reports `0.0`. That zero is an **absence, not a
/// level** — and following it writes silence onto a perfectly good speaker.
///
/// Which is unrecoverable rather than merely wrong: the user turns the knob
/// back up, this side reports the change, the peer's `set_default_output_volume`
/// fails on a device that has no volume to set, and the peer's next reading —
/// still `0.0` — pulls it straight back down. All while the interface says
/// "not muted, volume 0%".
///
/// One rule, both directions, so this single gate covers both halves: it stops
/// us adopting a peer's absent reading, and it stops us pushing our own absent
/// reading onto a peer whose speaker works fine. It is also the same fact
/// [`authority_for`] reads — `adjustable == false` means "this device's volume
/// is not a thing anyone can drive". §7.2 answers it with send-side software
/// gain because mode B has a virtual device to self-manage; mode A has none, so
/// the only honest answer here is to leave both devices alone.
pub fn classify_follow(
    is_spk_consumer: bool,
    mode_is_a: bool,
    opt: ModeAVolume,
    state: VolumeState,
) -> FollowAction {
    if !is_spk_consumer {
        return FollowAction::Ignore("only the consumer of a spk stream follows a peer's volume");
    }
    if !mode_is_a {
        return FollowAction::Ignore("volume following is a mode A option (plan §7.1)");
    }
    if !opt.sync {
        return FollowAction::Ignore("「与对端音量同步」 is off");
    }
    if !state.adjustable {
        return FollowAction::Ignore(
            "that device has no volume control, so its reported scalar is an absence, not a level",
        );
    }
    FollowAction::Apply(VolumeWrite {
        scalar: state.scalar.clamp(0.0, 1.0),
        // plan §7.1 的例外：与「静音本机」同时启用时**只同步音量、不同步静音**
        // ——否则连接时那一次性静音会顺着这条同步把对端也静掉，而镜像的全部
        // 意义就是让对端出声。
        muted: (!opt.mute_local).then_some(state.muted),
    })
}

/// What to do about 「静音本机」 when a mode-A speaker stream comes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteOnConnect {
    /// Payload is the reason, safe to log.
    Skip(&'static str),
    Mute,
}

/// plan §7.1 「静音本机输出」, evaluated exactly once per established stream.
///
/// `capture_survives_mute` answers §7.1's stated technical precondition —
/// 「捕获点位需在音量/静音之前，否则静音连镜像一起无声」 — for the backend this
/// stream actually captures with. `None` = not established for that backend (or
/// the source is not a system capture at all, e.g. the microphone), and the
/// user's explicit request wins: doing nothing while they watch the switch sit
/// in the "on" position is the worse of the two failures, and unmuting undoes
/// it. `Some(false)` is the one case that must NOT fire — plan names Windows
/// device loopback as post-mix, and muting there takes the mirror down with it,
/// which is a failure the user cannot attribute to this switch.
pub fn classify_mute_on_connect(
    mode_is_a: bool,
    opt: ModeAVolume,
    capture_survives_mute: Option<bool>,
) -> MuteOnConnect {
    if !opt.mute_local {
        return MuteOnConnect::Skip("「静音本机」 is off");
    }
    if !mode_is_a {
        return MuteOnConnect::Skip("「静音本机」 is a mode A option (plan §7.1)");
    }
    if capture_survives_mute == Some(false) {
        return MuteOnConnect::Skip(
            "this capture backend reads the stream AFTER the output volume, so muting here \
             would silence the mirror too (plan §7.1)",
        );
    }
    MuteOnConnect::Mute
}

/// Provider-side tracker: decides which device readings the peer must hear
/// about. A reading that merely echoes a write the peer itself asked for is
/// swallowed (spec §A2 source tagging); a genuine local change is reported.
///
/// Used by the mode-A consumer too (plan §7.1), on its own default output and
/// in the same shape: a reading it just adopted from the peer must not be
/// reported back as if the local user had made it.
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

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod mode_a_tests {
    //! plan §7.1 / §7.2 的两个模式 A 开关。判定是纯函数，接线在
    //! `audiohubd::conn` 与 `audiohubd::poll_consumer_volume`——那里另有守卫
    //! 测试，因为一个写对了却没人调的判定函数在这里照样全绿。

    use super::*;

    fn peer(scalar: f32, muted: bool) -> VolumeState {
        VolumeState { scalar, muted, adjustable: true }
    }

    const BOTH_OFF: ModeAVolume = ModeAVolume { sync: false, mute_local: false };
    const SYNC_ONLY: ModeAVolume = ModeAVolume { sync: true, mute_local: false };
    const BOTH_ON: ModeAVolume = ModeAVolume { sync: true, mute_local: true };

    fn applied(a: FollowAction) -> VolumeWrite {
        match a {
            FollowAction::Apply(w) => w,
            FollowAction::Ignore(why) => panic!("expected an Apply, got Ignore({why})"),
        }
    }

    /// The switch is an OPTION (plan §7.1), so off is off — including the case
    /// that used to be hard-coded on: a speaker stream on a mode-A consumer.
    #[test]
    fn nothing_is_followed_until_the_switch_is_on() {
        assert!(matches!(
            classify_follow(true, true, BOTH_OFF, peer(0.5, false)),
            FollowAction::Ignore(_)
        ));
        // ...and 「静音本机」 alone does not smuggle the sync in.
        let mute_only = ModeAVolume { sync: false, mute_local: true };
        assert!(matches!(
            classify_follow(true, true, mute_only, peer(0.5, false)),
            FollowAction::Ignore(_)
        ));
    }

    /// plan §7.1 gives both switches to mode A only. In mode B the virtual
    /// speaker IS the volume control (§7.2) and in share mode this machine is
    /// not a consumer, so following there would move a device the user is
    /// using for something else.
    #[test]
    fn following_belongs_to_mode_a_only() {
        assert!(matches!(
            classify_follow(true, false, BOTH_ON, peer(0.5, false)),
            FollowAction::Ignore(_)
        ));
    }

    /// Only the CONSUMER of a speaker stream has a peer whose output device
    /// this is about. On a stream we provide, the peer's slider already drives
    /// our device through `classify_set`; following as well closes a loop.
    #[test]
    fn a_provider_never_follows_its_own_stream() {
        assert!(matches!(
            classify_follow(false, true, BOTH_ON, peer(0.5, false)),
            FollowAction::Ignore(_)
        ));
    }

    /// plan §7.2 的那条例外，两半都要断言：**音量照旧同步，静音不同步。**
    ///
    /// 只断言「muted 是 None」是不够的——一个把整条同步关掉的实现也能满足它，
    /// 而那正好是例外要防的反面（例外说的是「只同步音量」，不是「什么都不同步」）。
    #[test]
    fn muting_this_machine_suppresses_the_mute_half_and_only_that_half() {
        let with = applied(classify_follow(true, true, SYNC_ONLY, peer(0.25, true)));
        assert_eq!(with.muted, Some(true), "with 「静音本机」 off the mute travels");
        assert_eq!(with.scalar, 0.25);

        let without = applied(classify_follow(true, true, BOTH_ON, peer(0.25, true)));
        assert_eq!(
            without.muted, None,
            "plan §7.2: 启用「静音本机」时只同步音量、不同步静音——否则连接时那一次性\
             静音会顺着同步把对端也静掉"
        );
        assert_eq!(
            without.scalar, 0.25,
            "the volume half must survive the exception; 「只同步音量」 is the point of it"
        );
    }

    /// One rule, both directions (§7.1 「互相跟随」). Fed our own reading it has
    /// to say the same thing it says about the peer's — a mute exception that
    /// held inbound only would still mute the peer.
    #[test]
    fn the_same_rule_answers_for_the_outbound_direction() {
        let local = VolumeState { scalar: 0.8, muted: true, adjustable: true };
        assert_eq!(applied(classify_follow(true, true, BOTH_ON, local)).muted, None);
        assert_eq!(
            applied(classify_follow(true, true, SYNC_ONLY, local)).muted,
            Some(true)
        );
    }

    #[test]
    fn a_peer_scalar_outside_the_range_is_clamped_not_refused() {
        assert_eq!(applied(classify_follow(true, true, SYNC_ONLY, peer(9.0, false))).scalar, 1.0);
        assert_eq!(applied(classify_follow(true, true, SYNC_ONLY, peer(-9.0, false))).scalar, 0.0);
    }

    /// A device with no volume control reports the scalar it does not have as
    /// `0.0` (`imp::get` averages an empty set of channel elements — a macOS
    /// aggregate device is exactly that). Following that reading writes silence
    /// onto a working speaker, and the loop that follows is unrecoverable: the
    /// user turns it back up, the peer's setter fails on a device with nothing
    /// to set, and the next reading — still `0.0` — pulls it down again.
    ///
    /// One rule, both directions, so this one gate also stops us pushing our
    /// own absent reading onto a peer whose speaker is fine.
    #[test]
    fn a_device_with_no_volume_control_reports_an_absence_not_a_level() {
        let aggregate = VolumeState { scalar: 0.0, muted: false, adjustable: false };
        assert!(
            matches!(classify_follow(true, true, SYNC_ONLY, aggregate), FollowAction::Ignore(_)),
            "an aggregate device's 0.0 was adopted as a volume: this mutes the other machine, \
             and the user cannot turn it back up"
        );
        // Not about the value: any reading from a device with no control is an
        // absence, even one that happens to look plausible.
        assert!(matches!(
            classify_follow(true, true, SYNC_ONLY, VolumeState {
                scalar: 0.4,
                muted: false,
                adjustable: false
            }),
            FollowAction::Ignore(_)
        ));
    }

    #[test]
    fn the_one_shot_mute_needs_the_switch_and_mode_a() {
        assert_eq!(
            classify_mute_on_connect(true, SYNC_ONLY, None),
            MuteOnConnect::Skip("「静音本机」 is off")
        );
        assert!(matches!(
            classify_mute_on_connect(false, BOTH_ON, None),
            MuteOnConnect::Skip(_)
        ));
        assert_eq!(classify_mute_on_connect(true, BOTH_ON, None), MuteOnConnect::Mute);
    }

    /// plan §7.1 的技术前提：捕获点位在音量之后的后端上，静音会连镜像一起静掉。
    /// 那种失效在对端看来是「没声音」，与网络故障无从分辨，所以这条不许开火。
    #[test]
    fn a_post_mix_capture_backend_blocks_the_one_shot_mute() {
        assert!(matches!(
            classify_mute_on_connect(true, BOTH_ON, Some(false)),
            MuteOnConnect::Skip(_)
        ));
        assert_eq!(classify_mute_on_connect(true, BOTH_ON, Some(true)), MuteOnConnect::Mute);
        // Unknown is NOT treated as post-mix: the user asked, unmuting undoes
        // it, and a switch that silently does nothing is the worse failure.
        assert_eq!(classify_mute_on_connect(true, BOTH_ON, None), MuteOnConnect::Mute);
    }

    /// The consumer's echo suppression is the provider's, reused (plan §7.1
    /// 「复用 §7.2 的 volume_set/volume_state 消息与来源标记防乒乓」): a reading
    /// that is merely the device confirming what we just adopted from the peer
    /// must not travel back as a local change.
    #[test]
    fn a_value_adopted_from_the_peer_is_not_reported_back_as_ours() {
        let mut s = VolumeSync::new();
        s.note_peer_apply(0.4, false);
        assert_eq!(s.poll(peer(0.4, false)), None, "that is the peer's own value coming back");
        // A genuine move afterwards still travels.
        assert!(s.poll(peer(0.9, false)).is_some());
    }
}

/// plan §7.2 兜底支路的**接手判据**。
#[cfg(test)]
mod send_gain_authority_tests {
    use super::*;

    /// 正反两面各钉一次。
    ///
    /// 注入对照：把 `authority_for` 的 `None` 分支改成 `SendGain`（即「还没听到
    /// 对端就先兜底」），第一条断言立刻变红。
    #[test]
    fn the_software_gain_fallback_waits_for_the_peer_to_say_so() {
        // 还没听到对端 ⇒ 绝不兜底：那会拿一个尚未到达的事实去衰减音频。
        assert_eq!(authority_for(None), VolumeAuthority::Peer);
        // 对端设备能调 ⇒ 走控制面（线上满幅），这是默认路径。
        assert_eq!(
            authority_for(Some(VolumeState { scalar: 0.4, muted: false, adjustable: true })),
            VolumeAuthority::Peer
        );
        // 对端设备不能调 ⇒ 本机接手（macOS 聚合设备：scalar 恒为 0 且不可写）。
        assert_eq!(
            authority_for(Some(VolumeState { scalar: 0.0, muted: false, adjustable: false })),
            VolumeAuthority::SendGain
        );
        // 静音态与判据无关：能不能调是设备的事，静没静音是状态。
        assert_eq!(
            authority_for(Some(VolumeState { scalar: 0.9, muted: true, adjustable: false })),
            VolumeAuthority::SendGain
        );
    }
}
