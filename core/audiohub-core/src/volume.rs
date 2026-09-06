//! Default input/output endpoint volume plus the control-plane ping-pong guard
//! both daemons share. The output API remains the spec-m4b §A1 contract; the
//! input API uses the same scalar/mute semantics for mode-B microphone sync.
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
    /// Whether the endpoint's mute property is independently writable. A
    /// fixed scalar does not imply a fixed mute (HDMI/aggregate devices often
    /// split these capabilities), so fallback routing must keep this separate.
    #[serde(default)]
    pub mute_adjustable: bool,
}

/// Which system endpoint owns a volume control.
///
/// Kept private because the public API intentionally names input and output
/// explicitly. Passing a direction at each platform boundary still matters:
/// the two native APIs use the same volume interface/property names, and a
/// forgotten data-flow/scope argument otherwise compiles while silently moving
/// the other endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointFlow {
    Input,
    Output,
}

impl EndpointFlow {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// `SessionMsg::VolumeSet.src`: the change was made by the sender's own user.
pub const SRC_LOCAL: &str = "local";
/// `SessionMsg::VolumeSet.src`: the sender is relaying a change that already
/// came from a peer. Applied like any other, but it must never travel further.
pub const SRC_PEER: &str = "peer";

/// Volumes this close count as the same reading, so a value coming back around
/// the sync loop is not reported to the peer as a fresh local change.
///
/// **The 1/16 grid this used to be blamed on is not a grid the device applies
/// to writes.** Measured 2026-08-09 (`docs/volume-taper-measured.md`, design
/// §9 P0-b): 16 of 16 programmatic scalar writes read back bit-exact, including
/// values placed deliberately between grid points (`0.71`, `0.97531`,
/// `0.500001`); the readback noise floor is 1e-6, not 1/16. The observation
/// behind the old wording was real but misattributed — 1/16 is the step the
/// VOLUME KEYS and the UI slider move in, so a device nobody has written
/// programmatically does sit on k/16.
///
/// What still justifies a tolerance is the round trip through the peer's
/// device, which has a grid of its own and silently clamps (see
/// [`OutputVolumeGuard::set_volume`]). Note the UNIT: this is slider position,
/// not dB. Near the quiet end 0.035 of slider is over 10 dB. The dB-domain
/// threshold is [`SAME_DB`], and neither converts into the other (design §5.1).
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

/// Default input device volume and mute state.
///
/// `adjustable=false` has the same meaning as on the output API: the endpoint
/// exposes no writable scalar, so callers must not treat `scalar` as a level
/// they can send back. Reading endpoint volume does not open an audio stream.
pub fn get_default_input_volume() -> Result<VolumeState> {
    get_endpoint_volume(EndpointFlow::Input, None)
}

/// Clamped to 0..=1. Errors when the device has no writable volume, which is
/// exactly the `adjustable=false` case.
pub fn set_default_output_volume(scalar: f32) -> Result<()> {
    set_output_volume(None, scalar)
}

/// Set the default input device volume. Finite values are clamped to `0..=1`;
/// an endpoint without a writable scalar returns an error.
pub fn set_default_input_volume(scalar: f32) -> Result<()> {
    set_endpoint_volume(EndpointFlow::Input, None, scalar)
}

pub fn set_default_output_mute(muted: bool) -> Result<()> {
    set_output_mute(None, muted)
}

/// Set the default input device mute control.
///
/// An endpoint may expose a writable volume without a writable mute property;
/// that case is reported as an error rather than being treated as unmuted.
pub fn set_default_input_mute(muted: bool) -> Result<()> {
    set_endpoint_mute(EndpointFlow::Input, None, muted)
}

/// Read one AudioHub-owned virtual MMDevice by authenticated peer identity.
///
/// On Windows the driver stamps this peer key into the endpoint property
/// store before AudioEndpointBuilder publishes it.  The key is deliberately
/// independent of the friendly name: names are localised, user-visible and
/// can remain stale across an offline/online rename, while a volume write must
/// never land on another peer's endpoint.  Other platforms do not use this
/// lookup and return an explicit error.
pub fn get_audiohub_peer_endpoint_volume(peer_key: &str, input: bool) -> Result<VolumeState> {
    imp::get_audiohub_peer(
        if input {
            EndpointFlow::Input
        } else {
            EndpointFlow::Output
        },
        peer_key,
    )
}

/// Apply one peer scalar/mute pair through the endpoint API itself, then read
/// back what Windows exposes to applications.
///
/// This is intentionally not a dB conversion in the kernel.  Windows defines
/// `IAudioEndpointVolume` scalar values using an OS-owned audio-tapered curve
/// whose shape is not part of the stable API.  Calling the scalar method is
/// the only way to preserve the exact user-facing control semantics.
pub fn set_audiohub_peer_endpoint_volume(
    peer_key: &str,
    input: bool,
    scalar: f32,
    muted: bool,
) -> Result<VolumeState> {
    let scalar = normalize_scalar(scalar)?;
    imp::set_audiohub_peer(
        if input {
            EndpointFlow::Input
        } else {
            EndpointFlow::Output
        },
        peer_key,
        scalar,
        muted,
    )
}

/// `None` = the system default output, exactly as `get_default_output_volume`.
/// `Some(name)` addresses one output device by its name — the driver's virtual
/// speaker has its own volume control, and reaching it must not require making
/// it the system default.
pub fn get_output_volume(dev: Option<&str>) -> Result<VolumeState> {
    get_endpoint_volume(EndpointFlow::Output, dev)
}

pub fn set_output_volume(dev: Option<&str>, scalar: f32) -> Result<()> {
    set_endpoint_volume(EndpointFlow::Output, dev, scalar)
}

pub fn set_output_mute(dev: Option<&str>, muted: bool) -> Result<()> {
    set_endpoint_mute(EndpointFlow::Output, dev, muted)
}

fn get_endpoint_volume(flow: EndpointFlow, dev: Option<&str>) -> Result<VolumeState> {
    imp::get(flow, dev)
}

fn set_endpoint_volume(flow: EndpointFlow, dev: Option<&str>, scalar: f32) -> Result<()> {
    imp::set_volume(flow, dev, normalize_scalar(scalar)?)
}

fn normalize_scalar(scalar: f32) -> Result<f32> {
    if !scalar.is_finite() {
        bail!("volume scalar must be finite");
    }
    Ok(scalar.clamp(0.0, 1.0))
}

fn set_endpoint_mute(flow: EndpointFlow, dev: Option<&str>, muted: bool) -> Result<()> {
    imp::set_mute(flow, dev, muted)
}

// ------------------------------------ design §4.2: the native dB (gain) path

/// Two dB readings this close count as the same reading (design §5.4).
///
/// 0.75 dB is 1.5× the 0.5 dB grid the Windows endpoint engine quantises to,
/// and it sits under the ~1 dB loudness JND for a direct A/B comparison — so a
/// difference this small is at once indistinguishable from the device's own
/// grid and inaudible. It does **not** need to be device-adaptive: the 1/16
/// slider grid that would have broken that claim is never applied to
/// programmatic writes (design §9 P0-b, see [`SAME_EPS`]).
///
/// **Not a converted [`SAME_EPS`].** That threshold is in slider position, this
/// one is in dB, and near the quiet end 0.035 of slider is over 10 dB. Design
/// §5.1 is named for exactly this substitution.
pub const SAME_DB: f32 = 0.75;

/// `20·log10(gain)` — the wire→device conversion of design §4.2.
///
/// A gain of `0` (or below) is not a level and has no dB: it maps to `-inf`,
/// which is design §3.2's point that no finite dB expresses a true zero.
/// Silence is the mute control's job, not the gain's.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain > 0.0 {
        20.0 * gain.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// `10^(dB/20)` — the device→wire conversion. `-inf` dB maps back to `0.0`.
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// What a device is at, plus — after a write — what it was ASKED for.
///
/// # Why this carries two numbers and not one
///
/// Measured 2026-08-09 (design §9): **a device clamps an out-of-range write and
/// still reports success.** Writing −80 dB to an output whose floor is −63.5 dB
/// returned `noErr` and left the device at −63.5 dB; +6 dB was clamped to 0 dB
/// the same way. Both devices tested behaved identically, and neither muted
/// itself at the floor.
///
/// So "the call succeeded" and "the device did what you asked" are two
/// different facts, and reading the property back is the ONLY way to separate
/// them — there is no status code for it. That makes the readback a
/// correctness requirement rather than a habit, which is why
/// [`set_output_gain`] returns this type instead of `Result<()>`: skipping the
/// readback is not something a caller can do. And it is why both numbers are
/// kept — reporting only the request would hide that the device refused,
/// reporting only the result would hide that anything else was ever asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainState {
    /// The gain that was asked for, clamped to the `0..=1` the wire allows
    /// (design §3.2 — nothing above unity travels). `None` on a plain read:
    /// nothing was asked, so there is nothing to have fallen short of.
    pub requested: Option<f32>,
    /// The gain the device READ BACK: what actually happened, and what design
    /// §4.2 sends to the peer. Deliberately **not** clamped to 1.0 — a device
    /// sitting above unity is a fact, and clamping here would make it
    /// unreportable.
    pub applied: f32,
    /// The dB the device read back, the raw property value `applied` is
    /// derived from. Kept because dB is the unit the device speaks and the unit
    /// differences are compared in ([`SAME_DB`]).
    pub applied_db: f32,
    /// The device's published dB range (`'vdb#'` on macOS, `GetVolumeRange` on
    /// Windows), when it publishes one. This is the fact that EXPLAINS a clamp.
    pub range_db: Option<(f32, f32)>,
    pub muted: bool,
    /// Whether the dB property can be written. Decided by the same predicate
    /// [`set_output_gain`] uses to pick something to write, so the two cannot
    /// contradict each other about one device — the mistake `volume_writable`
    /// was introduced to stop, one selector over.
    ///
    /// This is about the dB property specifically and may differ from
    /// [`VolumeState::adjustable`], which is about the scalar. A device that is
    /// scalar-writable but not dB-writable is the case design §4.4 was written
    /// for; that fallback is deliberately NOT implemented (measured: no such
    /// device found), so such a device simply reads `false` here and
    /// [`set_output_gain`] fails on it.
    pub adjustable: bool,
}

impl GainState {
    /// The dB that was asked for. `Some(-inf)` when the request was `0.0`.
    pub fn requested_db(&self) -> Option<f32> {
        self.requested.map(gain_to_db)
    }

    /// How far the device landed from the request, in dB (`applied −
    /// requested`; negative means quieter than asked). `None` on a plain read.
    ///
    /// A request of `0.0` yields `+inf`, and that is not a defect: gain 0 asks
    /// for silence, no finite dB is silence, and every device therefore falls
    /// infinitely short of it. Design §3.2 answers that with the mute control.
    pub fn shortfall_db(&self) -> Option<f32> {
        Some(self.applied_db - self.requested_db()?)
    }

    /// True when the device did something other than what was asked by more
    /// than a grid step. This is the third state design §4.3 names: not
    /// "refused" (that is an `Err`) and not "did it", but "reported success and
    /// landed somewhere else".
    pub fn clamped(&self) -> bool {
        // Negated comparison so a NaN shortfall counts as clamped rather than
        // as compliance.
        self.shortfall_db().is_some_and(|d| !(d.abs() <= SAME_DB))
    }
}

/// Reads a device's native dB volume as a wire gain (design §4.2). `None` = the
/// system default output, same convention as [`get_output_volume`].
///
/// **Errors when the device publishes no dB property at all**, instead of
/// reporting `gain = 0.0`. [`classify_follow`] documents at length what a
/// fabricated zero costs: it is an absence, and following it writes silence
/// onto a working speaker. `Result` has a place to say "this device cannot
/// answer"; the `applied` field does not.
pub fn get_output_gain(dev: Option<&str>) -> Result<GainState> {
    imp::get_gain(dev)
}

/// Writes `gain` as dB and returns WHAT THE DEVICE READ BACK.
///
/// The readback is the return value on purpose: measured, an out-of-range write
/// is clamped and still reports success (see [`GainState`]), so `Result<()>`
/// would be a signature that cannot express what happened. Callers get the
/// clamp handed to them rather than having to remember to look for it.
///
/// `gain` is clamped to `0.0..=1.0` — design §3.2 puts nothing above unity on
/// the wire. `gain == 0.0` asks for silence: it writes the device's own dB
/// floor, because the device is the one that knows how quiet it can be, and the
/// returned `applied` says how quiet that turned out to be. **It does not
/// mute** — measured, a device sitting at its dB floor keeps `mute = 0`, so
/// §3.2's "also set the mute control" is the caller's explicit action and not a
/// side effect buried in a gain write.
pub fn set_output_gain(dev: Option<&str>, gain: f32) -> Result<GainState> {
    if !gain.is_finite() {
        bail!("gain must be finite");
    }
    imp::set_gain(dev, gain.clamp(0.0, 1.0))
}

// -------------------------------------------------- borrow-and-return a knob

/// Borrows an output device's volume/mute, and puts them back.
///
/// The volume knob this touches is a knob the USER owns: on the machine this
/// runs on it may be the one carrying their music right now. Every tool that
/// moves it for a measurement therefore has to move it back, including on the
/// paths nobody writes code for — an early `?`, a panic in the middle of a
/// measurement leg, a probe that is `^C`'d halfway. That is what makes this a
/// guard and not a pair of helper functions: `Drop` runs on all three (the
/// workspace unwinds — nothing sets `panic = "abort"`), so the only way to
/// leave the volume moved is to `std::mem::forget` this value or to kill the
/// process with SIGKILL.
///
/// [`Self::restore`] exists on top of `Drop` for the one thing `Drop` cannot
/// do: report that putting it back FAILED. A caller that cares (a probe writing
/// a verdict) should call it explicitly and surface the error; `Drop` is the
/// net underneath, and is silent by design because a panicking `Drop` during an
/// unwind aborts the process.
///
/// # It only guards writes made THROUGH IT
///
/// The restore is armed by [`Self::set_volume`] / [`Self::set_mute`], not by
/// holding the guard. Code that captures one and then moves the same device by
/// another route — [`set_output_volume`], [`set_output_gain`], a shell command
/// — gets an `Ok(())` from `restore` that did nothing, which looks exactly like
/// a successful restore. If you need a knob put back no matter who turned it,
/// capture the state and restore it unconditionally instead of reaching for
/// this type.
pub struct OutputVolumeGuard {
    /// `None` = the system default output, same convention as [`get_output_volume`].
    device: Option<String>,
    original: VolumeState,
    /// Cleared by `restore` so `Drop` does not write the device a second time.
    outstanding: bool,
}

impl OutputVolumeGuard {
    /// Reads the current state and takes responsibility for restoring it.
    ///
    /// Fails when the device exposes no writable volume (`adjustable == false`):
    /// a guard over a knob that cannot be turned would hand the caller a
    /// measurement in which nothing was ever varied, and "the level did not
    /// change" would read as independence.
    pub fn capture(device: Option<&str>) -> Result<OutputVolumeGuard> {
        let original = get_output_volume(device)?;
        if !original.adjustable {
            bail!(
                "{} exposes no writable volume, so nothing can be varied",
                match device {
                    None => "the default output device".to_string(),
                    Some(name) => format!("output device {name:?}"),
                }
            );
        }
        Ok(OutputVolumeGuard {
            device: device.map(str::to_string),
            original,
            outstanding: false,
        })
    }

    pub fn original(&self) -> VolumeState {
        self.original
    }

    fn dev(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// Sets the volume and returns what the device READ BACK, which is not
    /// always what was written.
    ///
    /// The reason is NOT quantisation — measured 2026-08-09, a programmatic
    /// scalar write reads back bit-exact (design §9 P0-b, see [`SAME_EPS`]).
    /// It is that **a device silently CLAMPS and still reports success**: the
    /// same session wrote −80 dB to an output whose floor is −63.5 dB, got
    /// `noErr`, and found the device sitting at −63.5 dB; +6 dB clamped to 0 dB
    /// the same way, on both devices tested. So the status code cannot tell
    /// "it did what you asked" apart from "it did something else and said
    /// fine" — only the readback can, which is why this hands back the reading
    /// instead of `()`. [`set_output_gain`] is the same decision, in dB.
    pub fn set_volume(&mut self, scalar: f32) -> Result<VolumeState> {
        self.outstanding = true;
        set_output_volume(self.dev(), scalar)?;
        get_output_volume(self.dev())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<VolumeState> {
        self.outstanding = true;
        set_output_mute(self.dev(), muted)?;
        get_output_volume(self.dev())
    }

    /// Puts the volume and mute back where they were found, and reports failure.
    /// Idempotent: a second call (or the `Drop` that follows) is a no-op.
    pub fn restore(&mut self) -> Result<()> {
        if !self.outstanding {
            return Ok(());
        }
        // Unmute LAST: restoring the volume while muted is inaudible, whereas
        // the reverse order can put a full-scale scalar onto live speakers for
        // the width of one call.
        let vol = set_output_volume(self.dev(), self.original.scalar);
        let mute = set_output_mute(self.dev(), self.original.muted);
        self.outstanding = false;
        vol.and(mute)
    }
}

impl Drop for OutputVolumeGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Resolves the name the user typed against the devices a backend enumerated.
/// Shared so both backends refuse a typo the same way instead of silently
/// landing on the wrong card.
#[cfg(any(target_os = "macos", windows))]
fn match_by_name<T>(flow: EndpointFlow, devices: Vec<(T, String)>, want: &str) -> Result<T> {
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
            "no {} device named {want:?}; available: {}",
            flow.label(),
            quoted(rest.iter().map(|(_, n)| n))
        ),
        n => bail!(
            "{n} {} devices match {want:?}: {}",
            flow.label(),
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
fn label(flow: EndpointFlow, dev: Option<&str>) -> String {
    match dev {
        None => format!("the default {} device", flow.label()),
        Some(name) => format!("{} device {name:?}", flow.label()),
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
        VolumeSync {
            reported: None,
            pending: None,
        }
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
    //! CoreAudio: default input/output device ->
    //! kAudioDevicePropertyVolumeScalar / kAudioDevicePropertyMute on the
    //! matching scope. Master element first, then the per-channel elements;
    //! `adjustable` is the property's IsSettable. A named device resolves
    //! through kAudioHardwarePropertyDevices first; the probing below is
    //! identical either way.

    use super::{
        db_to_gain, gain_to_db, label, match_by_name, EndpointFlow, GainState, VolumeState,
        SAME_EPS,
    };
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
    const SEL_DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName (= DeviceNameCFString)
    const SEL_STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams
    const SEL_VOLUME_SCALAR: u32 = fourcc(b"volm"); // kAudioDevicePropertyVolumeScalar
    /// The device's real dB, and design §4.2's whole point. Not to be confused
    /// with the TRANSLATION properties `'v2db'` / `'db2v'`, which measurement
    /// caught lying (a linear curve contradicting the device by 15.6 dB) or
    /// erroring outright — this one is device STATE, and reads and writes true.
    const SEL_VOLUME_DB: u32 = fourcc(b"vold"); // kAudioDevicePropertyVolumeDecibels
    /// Read-only by nature: the range is the device telling us its floor and
    /// ceiling, and it is what makes a clamp explicable rather than mysterious.
    const SEL_VOLUME_DB_RANGE: u32 = fourcc(b"vdb#"); // kAudioDevicePropertyVolumeRangeDecibels
    const SEL_MUTE: u32 = fourcc(b"mute"); // kAudioDevicePropertyMute
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
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
        PropAddr {
            selector,
            scope,
            element,
        }
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

    /// `what` names the selector for the error text only — a failed write says
    /// which property it was, and `'volm'` and `'vold'` fail for different
    /// reasons.
    fn set_f32(dev: AudioObjectID, a: &PropAddr, v: f32, what: &str) -> Result<()> {
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
            bail!("AudioObjectSetPropertyData({what}) failed: OSStatus {st}");
        }
        Ok(())
    }

    /// `AudioValueRange`, the shape `'vdb#'` answers in: two `Float64`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ValueRange {
        min: f64,
        max: f64,
    }

    fn get_range(dev: AudioObjectID, a: &PropAddr) -> Option<(f32, f32)> {
        let mut r = ValueRange { min: 0.0, max: 0.0 };
        let want = std::mem::size_of::<ValueRange>() as u32;
        let mut sz = want;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                a,
                0,
                std::ptr::null(),
                &mut sz,
                &mut r as *mut ValueRange as *mut c_void,
            )
        };
        (st == 0 && sz == want).then(|| (r.min as f32, r.max as f32))
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
        let st = unsafe { AudioObjectGetPropertyDataSize(dev, a, 0, std::ptr::null(), &mut sz) };
        (st == 0).then_some(sz)
    }

    fn scope(flow: EndpointFlow) -> u32 {
        match flow {
            EndpointFlow::Input => SCOPE_INPUT,
            EndpointFlow::Output => SCOPE_OUTPUT,
        }
    }

    fn default_selector(flow: EndpointFlow) -> u32 {
        match flow {
            EndpointFlow::Input => SEL_DEFAULT_INPUT,
            EndpointFlow::Output => SEL_DEFAULT_OUTPUT,
        }
    }

    fn default_device(flow: EndpointFlow) -> Result<AudioObjectID> {
        let a = at(default_selector(flow), SCOPE_GLOBAL, ELEM_MAIN);
        let dev = get_u32(SYSTEM_OBJECT, &a)
            .ok_or_else(|| anyhow!("cannot read the default {} device", flow.label()))?;
        if dev == 0 {
            bail!("no default {} device", flow.label());
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

    /// Every device in one data flow. A present stream in that scope is the same
    /// test CoreAudio itself uses to decide which endpoint list contains it.
    fn endpoint_devices(flow: EndpointFlow) -> Vec<(AudioObjectID, String)> {
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
                prop_size(d, &at(SEL_STREAMS, scope(flow), ELEM_MAIN)).is_some_and(|n| n > 0)
            })
            .filter_map(|d| device_name(d).map(|n| (d, n)))
            .collect()
    }

    fn resolve(flow: EndpointFlow, dev: Option<&str>) -> Result<AudioObjectID> {
        match dev {
            None => default_device(flow),
            Some(name) => match_by_name(flow, endpoint_devices(flow), name),
        }
    }

    /// Present channel elements of `selector`, in element order.
    fn channel_elements(flow: EndpointFlow, dev: AudioObjectID, selector: u32) -> Vec<PropAddr> {
        (1..=MAX_CHANNEL_ELEMENTS)
            .map(|ch| at(selector, scope(flow), ch))
            .filter(|a| has(dev, a))
            .collect()
    }

    /// True exactly when `set_volume` would find something to write: the master
    /// element, else any per-channel element. Shared so `adjustable` cannot
    /// disagree with the setter — a read-only master over writable channels used
    /// to report `adjustable=false` while `set_volume` happily worked, greying
    /// the slider out for no reason.
    fn volume_writable(flow: EndpointFlow, dev: AudioObjectID) -> bool {
        let master = at(SEL_VOLUME_SCALAR, scope(flow), ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return true;
        }
        channel_elements(flow, dev, SEL_VOLUME_SCALAR)
            .iter()
            .any(|a| settable(dev, a))
    }

    fn mute_writable(flow: EndpointFlow, dev: AudioObjectID) -> bool {
        let master = at(SEL_MUTE, scope(flow), ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return true;
        }
        channel_elements(flow, dev, SEL_MUTE)
            .iter()
            .any(|a| settable(dev, a))
    }

    pub fn get(flow: EndpointFlow, target: Option<&str>) -> Result<VolumeState> {
        let dev = resolve(flow, target)?;
        let master = at(SEL_VOLUME_SCALAR, scope(flow), ELEM_MAIN);
        let adjustable = volume_writable(flow, dev);
        let scalar = if has(dev, &master) {
            get_f32(dev, &master).unwrap_or(0.0)
        } else {
            // No master volume: aggregate devices and many HDMI/optical outs
            // land here. Average whatever channels exist.
            let vals: Vec<f32> = channel_elements(flow, dev, SEL_VOLUME_SCALAR)
                .iter()
                .filter_map(|a| get_f32(dev, a))
                .collect();
            if vals.is_empty() {
                0.0
            } else {
                vals.iter().sum::<f32>() / vals.len() as f32
            }
        };
        Ok(VolumeState {
            scalar: scalar.clamp(0.0, 1.0),
            muted: read_mute(flow, dev),
            adjustable,
            mute_adjustable: mute_writable(flow, dev),
        })
    }

    /// Master mute if the device has one, else "any channel is muted". Shared
    /// by the scalar and dB readers so the two cannot come back disagreeing
    /// about whether one device is muted.
    fn read_mute(flow: EndpointFlow, dev: AudioObjectID) -> bool {
        let master = at(SEL_MUTE, scope(flow), ELEM_MAIN);
        if has(dev, &master) {
            return get_u32(dev, &master).unwrap_or(0) != 0;
        }
        channel_elements(flow, dev, SEL_MUTE)
            .iter()
            .filter_map(|a| get_u32(dev, a))
            .any(|v| v != 0)
    }

    pub fn set_volume(flow: EndpointFlow, target: Option<&str>, scalar: f32) -> Result<()> {
        let dev = resolve(flow, target)?;
        let master = at(SEL_VOLUME_SCALAR, scope(flow), ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return set_f32(dev, &master, scalar, "volm");
        }
        let mut wrote = 0usize;
        for a in channel_elements(flow, dev, SEL_VOLUME_SCALAR) {
            if settable(dev, &a) {
                set_f32(dev, &a, scalar, "volm")?;
                wrote += 1;
            }
        }
        if wrote == 0 {
            bail!("{} has no adjustable volume", label(flow, target));
        }
        Ok(())
    }

    pub fn set_mute(flow: EndpointFlow, target: Option<&str>, muted: bool) -> Result<()> {
        let dev = resolve(flow, target)?;
        let v = u32::from(muted);
        let master = at(SEL_MUTE, scope(flow), ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return set_u32(dev, &master, v);
        }
        let mut wrote = 0usize;
        for a in channel_elements(flow, dev, SEL_MUTE) {
            if settable(dev, &a) {
                set_u32(dev, &a, v)?;
                wrote += 1;
            }
        }
        if wrote == 0 {
            bail!("{} has no mute control", label(flow, target));
        }
        Ok(())
    }

    pub fn get_audiohub_peer(_flow: EndpointFlow, _peer_key: &str) -> Result<VolumeState> {
        bail!("AudioHub peer-key endpoint lookup is only supported on Windows")
    }

    pub fn set_audiohub_peer(
        _flow: EndpointFlow,
        _peer_key: &str,
        _scalar: f32,
        _muted: bool,
    ) -> Result<VolumeState> {
        bail!("AudioHub peer-key endpoint lookup is only supported on Windows")
    }

    #[cfg(test)]
    mod endpoint_flow_tests {
        use super::*;

        #[test]
        fn input_and_output_select_different_coreaudio_endpoints_and_scopes() {
            assert_eq!(default_selector(EndpointFlow::Input), fourcc(b"dIn "));
            assert_eq!(default_selector(EndpointFlow::Output), fourcc(b"dOut"));
            assert_eq!(scope(EndpointFlow::Input), fourcc(b"inpt"));
            assert_eq!(scope(EndpointFlow::Output), fourcc(b"outp"));
            assert_ne!(
                default_selector(EndpointFlow::Input),
                default_selector(EndpointFlow::Output)
            );
            assert_ne!(scope(EndpointFlow::Input), scope(EndpointFlow::Output));
        }
    }

    // ------------------------------------------- design §4.2 native dB path

    /// A dB far below any real floor, for the one case that has no dB at all:
    /// `gain == 0` on a device that publishes no range. Measured (design §9),
    /// an out-of-range write lands on the floor and reports success, so this
    /// arrives at the same place the published floor would have.
    const SILENT_DB: f32 = -200.0;

    /// The elements `'vold'` answers on, master first — the same master →
    /// per-channel order the scalar path probes in. Empty means the device
    /// publishes no dB reading at all.
    fn db_elements(dev: AudioObjectID) -> Vec<PropAddr> {
        let master = at(SEL_VOLUME_DB, SCOPE_OUTPUT, ELEM_MAIN);
        if has(dev, &master) {
            return vec![master];
        }
        channel_elements(EndpointFlow::Output, dev, SEL_VOLUME_DB)
    }

    /// True exactly when `set_gain` would find something to write, so
    /// `GainState::adjustable` cannot disagree with the setter. Same discipline
    /// as `volume_writable`, one selector over: a read-only master over
    /// writable channels must not grey the control out.
    fn gain_writable(dev: AudioObjectID) -> bool {
        let master = at(SEL_VOLUME_DB, SCOPE_OUTPUT, ELEM_MAIN);
        if has(dev, &master) && settable(dev, &master) {
            return true;
        }
        channel_elements(EndpointFlow::Output, dev, SEL_VOLUME_DB)
            .iter()
            .any(|a| settable(dev, a))
    }

    fn range_of(dev: AudioObjectID, elem: u32) -> Option<(f32, f32)> {
        get_range(dev, &at(SEL_VOLUME_DB_RANGE, SCOPE_OUTPUT, elem))
    }

    /// The quietest dB this element can honestly be asked for (design §3.2:
    /// the device is the one that knows how quiet it can be).
    fn silence_db(dev: AudioObjectID, elem: u32) -> f32 {
        range_of(dev, elem).map_or(SILENT_DB, |(min, _)| min)
    }

    fn read_gain(
        dev: AudioObjectID,
        target: Option<&str>,
        requested: Option<f32>,
    ) -> Result<GainState> {
        let elems = db_elements(dev);
        if elems.is_empty() {
            bail!(
                "{} publishes no dB volume ('vold'), so the native dB path does not apply to it",
                label(EndpointFlow::Output, target)
            );
        }
        let dbs: Vec<(u32, f32)> = elems
            .iter()
            .filter_map(|a| get_f32(dev, a).map(|v| (a.element, v)))
            .collect();
        let Some(&(first_elem, first_db)) = dbs.first() else {
            bail!(
                "{} has a dB volume property that will not read",
                label(EndpointFlow::Output, target)
            );
        };
        // Averaged in the GAIN domain, not the dB domain. dB is logarithmic,
        // so averaging it is a geometric mean of amplitude and would call a
        // {0 dB, −60 dB} pair −30 dB, which is neither channel. Channels we
        // write we write together, so a spread only shows up on a device
        // something else set.
        let applied_db = if dbs.len() == 1 {
            first_db
        } else {
            gain_to_db(dbs.iter().map(|&(_, d)| db_to_gain(d)).sum::<f32>() / dbs.len() as f32)
        };
        Ok(GainState {
            requested,
            applied: db_to_gain(applied_db),
            applied_db,
            range_db: range_of(dev, first_elem),
            muted: read_mute(EndpointFlow::Output, dev),
            adjustable: gain_writable(dev),
        })
    }

    pub fn get_gain(target: Option<&str>) -> Result<GainState> {
        let dev = resolve(EndpointFlow::Output, target)?;
        read_gain(dev, target, None)
    }

    pub fn set_gain(target: Option<&str>, gain: f32) -> Result<GainState> {
        let dev = resolve(EndpointFlow::Output, target)?;
        let master = at(SEL_VOLUME_DB, SCOPE_OUTPUT, ELEM_MAIN);
        let writable: Vec<PropAddr> = if has(dev, &master) && settable(dev, &master) {
            vec![master]
        } else {
            channel_elements(EndpointFlow::Output, dev, SEL_VOLUME_DB)
                .into_iter()
                .filter(|a| settable(dev, a))
                .collect()
        };
        if writable.is_empty() {
            bail!(
                "{} has no writable dB volume",
                label(EndpointFlow::Output, target)
            );
        }
        for a in &writable {
            // gain 0 has no finite dB, so ask the element for its own floor
            // instead. This does NOT mute: measured, a device at its floor
            // keeps mute = 0 (design §9), and §3.2 makes muting an explicit
            // action of the caller.
            let db = if gain > 0.0 {
                gain_to_db(gain)
            } else {
                silence_db(dev, a.element)
            };
            set_f32(dev, a, db, "vold")?;
        }
        // Never `Ok(())`: the write above can be silently clamped and still
        // return noErr, so the reading is the only truthful answer.
        read_gain(dev, target, Some(gain))
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    //! COM by hand: MMDeviceEnumerator -> GetDefaultAudioEndpoint(flow,
    //! eConsole) -> Activate(IAudioEndpointVolume). A named device swaps the
    //! middle step for EnumAudioEndpoints(flow) + PKEY_Device_FriendlyName.
    //! Vtable layouts are the frozen ABI of mmdeviceapi.h / endpointvolume.h;
    //! slots we never call are declared as `usize` so nothing can be invoked
    //! through them by accident.

    use super::{
        db_to_gain, gain_to_db, label, match_by_name, EndpointFlow, GainState, VolumeState,
        SAME_EPS,
    };
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

    /// AudioHub's private, read-only endpoint identity.  The Windows driver
    /// writes a versioned peer-fingerprint/direction value to this EP\0
    /// property before the endpoint is published; AudioEndpointBuilder then
    /// copies it into the MMDevice property store.  Render/capture enumeration
    /// independently supplies the expected direction, so a swapped property
    /// tag fails closed instead of selecting the opposite endpoint.
    const PKEY_AUDIOHUB_PEER_KEY: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0x8CA48324,
            d2: 0x7D8A,
            d3: 0x4EFA,
            d4: [0x8D, 0xD4, 0x7B, 0x75, 0x03, 0xAF, 0x96, 0x4B],
        },
        pid: 2,
    };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const CLSCTX_ALL: u32 = 0x17;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106
    const E_RENDER: u32 = 0; // EDataFlow::eRender
    const E_CAPTURE: u32 = 1; // EDataFlow::eCapture
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
            PropVariant {
                vt: 0,
                r1: 0,
                r2: 0,
                r3: 0,
                val: [0; 2],
            }
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
        get_value:
            unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT,
        set_value: usize,
        commit: usize,
    }

    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        base: IUnknownVtbl,
        register_control_change_notify: usize,
        unregister_control_change_notify: usize,
        get_channel_count: usize,
        /// `SetMasterVolumeLevel(float dB, LPCGUID ctx)` — design §4.2's write
        /// end on Windows. Declaring it costs no layout change: the slot was
        /// always here at this offset, it was only spelled `usize` while
        /// nothing called it.
        set_master_volume_level:
            unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
        set_master_volume_level_scalar:
            unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
        /// `GetMasterVolumeLevel(float *dB)` — the readback half.
        get_master_volume_level: unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
        get_master_volume_level_scalar: unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
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
        /// `GetVolumeRange(float *mindB, float *maxdB, float *incrementdB)` —
        /// the endpoint's own floor and ceiling, which is what explains a
        /// clamp. Last slot in the interface, so enabling it cannot shift
        /// anything above it either.
        get_volume_range:
            unsafe extern "system" fn(*mut c_void, *mut f32, *mut f32, *mut f32) -> HRESULT,
    }

    /// Balances CoInitializeEx. A thread another library already put in a
    /// different apartment (cpal's WASAPI backend does this) reports
    /// RPC_E_CHANGED_MODE: that apartment is fine for us, and it is not ours
    /// to tear down.
    struct Apartment {
        owned: bool,
    }

    impl Apartment {
        fn enter() -> Result<Apartment> {
            let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
            if hr >= 0 {
                return Ok(Apartment { owned: true });
            }
            if hr == RPC_E_CHANGED_MODE {
                // Another library already selected this thread's apartment.
                // COM is initialized and usable, but that initialization is
                // not ours to balance.
                return Ok(Apartment { owned: false });
            }
            bail!("CoInitializeEx failed: HRESULT 0x{:08X}", hr as u32)
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

    fn string_property(device: &ComPtr, key: &PropertyKey) -> Option<String> {
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
            ((*v).get_value)(store.0, key, &mut pv)
        };
        if hr < 0 || pv.vt != VT_LPWSTR {
            return None;
        }
        unsafe { wide_string(pv.val[0] as *const u16) }
    }

    fn friendly_name(device: &ComPtr) -> Option<String> {
        string_property(device, &PKEY_DEVICE_FRIENDLY_NAME)
    }

    fn audiohub_identity(device: &ComPtr) -> Option<String> {
        string_property(device, &PKEY_AUDIOHUB_PEER_KEY)
    }

    fn data_flow(flow: EndpointFlow) -> u32 {
        match flow {
            EndpointFlow::Input => E_CAPTURE,
            EndpointFlow::Output => E_RENDER,
        }
    }

    /// Every active endpoint in one data flow.  Identity-based AudioHub lookup
    /// must retain an endpoint even if its display name is temporarily
    /// unreadable; the private peer-key property, not presentation, decides.
    fn active_endpoints(enumerator: &ComPtr, flow: EndpointFlow) -> Result<Vec<ComPtr>> {
        let mut coll = ComPtr::null();
        check(
            unsafe {
                let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                ((*v).enum_audio_endpoints)(
                    enumerator.0,
                    data_flow(flow),
                    DEVICE_STATE_ACTIVE,
                    &mut coll.0,
                )
            },
            &format!(
                "EnumAudioEndpoints(e{}, DEVICE_STATE_ACTIVE)",
                if flow == EndpointFlow::Input {
                    "Capture"
                } else {
                    "Render"
                }
            ),
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
            out.push(dev);
        }
        Ok(out)
    }

    /// Named lookup is a user-facing compatibility API.  Unlike the private
    /// peer-key path, an unnameable endpoint cannot be selected by a name the
    /// caller supplied and is therefore omitted.
    fn endpoint_devices(enumerator: &ComPtr, flow: EndpointFlow) -> Result<Vec<(ComPtr, String)>> {
        Ok(active_endpoints(enumerator, flow)?
            .into_iter()
            .filter_map(|dev| friendly_name(&dev).map(|name| (dev, name)))
            .collect())
    }

    fn endpoint_volume(flow: EndpointFlow, target: Option<&str>) -> Result<Endpoint> {
        let apt = Apartment::enter()?;
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
                            data_flow(flow),
                            E_CONSOLE,
                            &mut device.0,
                        )
                    },
                    &format!(
                        "GetDefaultAudioEndpoint(e{}, eConsole)",
                        if flow == EndpointFlow::Input {
                            "Capture"
                        } else {
                            "Render"
                        }
                    ),
                )?;
                device
            }
            Some(name) => match_by_name(flow, endpoint_devices(&enumerator, flow)?, name)?,
        };
        activate_endpoint(apt, device)
    }

    fn activate_endpoint(apt: Apartment, device: ComPtr) -> Result<Endpoint> {
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

    fn valid_peer_key(peer_key: &str) -> bool {
        peer_key.len() == 16
            && peer_key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// Resolve one AudioHub endpoint without consulting its display name.
    /// Friendly names can be localised, renamed by the user, or retain an
    /// offline suffix until AudioEndpointBuilder recreates the endpoint.  The
    /// driver property is the authenticated peer/direction identity and the
    /// MMDevice enumerator supplies the data-flow direction independently.
    fn audiohub_endpoint_volume(flow: EndpointFlow, peer_key: &str) -> Result<Endpoint> {
        if !valid_peer_key(peer_key) {
            bail!("AudioHub peer key must be exactly 16 lowercase hexadecimal characters");
        }
        let apt = Apartment::enter()?;
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

        let mut matches = Vec::new();
        let mut catalog = Vec::new();
        let expected_identity = format!(
            "v1:{peer_key}:{}",
            if flow == EndpointFlow::Input {
                "in"
            } else {
                "out"
            }
        );
        for device in active_endpoints(&enumerator, flow)? {
            let found_identity = audiohub_identity(&device);
            let name = friendly_name(&device).unwrap_or_else(|| "<unnamed>".to_string());
            catalog.push(format!(
                "{} [{}]",
                name,
                found_identity
                    .as_deref()
                    .unwrap_or("no AudioHub endpoint identity")
            ));
            if found_identity.as_deref() == Some(expected_identity.as_str()) {
                matches.push(device);
            }
        }
        if matches.len() != 1 {
            bail!(
                "expected exactly one active AudioHub {} endpoint for peer {}, found {}; available: [{}]",
                flow.label(),
                peer_key,
                matches.len(),
                catalog.join(", ")
            );
        }
        activate_endpoint(apt, matches.pop().expect("length checked"))
    }

    fn read_endpoint(ep: &Endpoint) -> Result<VolumeState> {
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        let mut scalar: f32 = 0.0;
        check(
            unsafe { ((*v).get_master_volume_level_scalar)(ep.vol.0, &mut scalar) },
            "GetMasterVolumeLevelScalar",
        )?;
        let mut muted_raw: i32 = 0;
        let (muted, mute_adjustable) = match unsafe { ((*v).get_mute)(ep.vol.0, &mut muted_raw) } {
            hr if hr >= 0 => (muted_raw != 0, true),
            _ => (false, false), // endpoint without a mute control
        };
        Ok(VolumeState {
            scalar: scalar.clamp(0.0, 1.0),
            muted,
            adjustable: true,
            mute_adjustable,
        })
    }

    pub fn get(flow: EndpointFlow, target: Option<&str>) -> Result<VolumeState> {
        let ep = endpoint_volume(flow, target)?;
        read_endpoint(&ep)
    }

    pub fn get_audiohub_peer(flow: EndpointFlow, peer_key: &str) -> Result<VolumeState> {
        let ep = audiohub_endpoint_volume(flow, peer_key)?;
        read_endpoint(&ep)
    }

    pub fn set_audiohub_peer(
        flow: EndpointFlow,
        peer_key: &str,
        scalar: f32,
        muted: bool,
    ) -> Result<VolumeState> {
        let ep = audiohub_endpoint_volume(flow, peer_key)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_master_volume_level_scalar)(ep.vol.0, scalar, ptr::null()) },
            &format!("SetMasterVolumeLevelScalar on AudioHub peer {peer_key}"),
        )?;
        check(
            unsafe { ((*v).set_mute)(ep.vol.0, i32::from(muted), ptr::null()) },
            &format!("SetMute on AudioHub peer {peer_key}"),
        )?;
        let state = read_endpoint(&ep)?;
        if (state.scalar - scalar).abs() > SAME_EPS {
            bail!(
                "AudioHub peer {peer_key} {} endpoint refused scalar={scalar:.6}; read back {:.6}",
                flow.label(),
                state.scalar
            );
        }
        if state.muted != muted {
            bail!(
                "AudioHub peer {peer_key} {} endpoint refused mute={muted}; read back {}",
                flow.label(),
                state.muted
            );
        }
        Ok(state)
    }

    pub fn set_volume(flow: EndpointFlow, target: Option<&str>, scalar: f32) -> Result<()> {
        let ep = endpoint_volume(flow, target)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_master_volume_level_scalar)(ep.vol.0, scalar, ptr::null()) },
            &format!("SetMasterVolumeLevelScalar on {}", label(flow, target)),
        )
    }

    pub fn set_mute(flow: EndpointFlow, target: Option<&str>, muted: bool) -> Result<()> {
        let ep = endpoint_volume(flow, target)?;
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_mute)(ep.vol.0, i32::from(muted), ptr::null()) },
            &format!("SetMute on {}", label(flow, target)),
        )
    }

    #[cfg(test)]
    mod endpoint_flow_tests {
        use super::*;

        #[test]
        fn input_and_output_select_the_correct_mmdevice_data_flow() {
            assert_eq!(data_flow(EndpointFlow::Input), E_CAPTURE);
            assert_eq!(data_flow(EndpointFlow::Output), E_RENDER);
            assert_ne!(
                data_flow(EndpointFlow::Input),
                data_flow(EndpointFlow::Output)
            );
            assert_eq!(E_CONSOLE, 0, "both flows must use the Console role");
        }

        #[test]
        fn audiohub_endpoint_identity_is_the_wire_fingerprint_not_a_name() {
            assert!(valid_peer_key("0123456789abcdef"));
            for bad in [
                "0123456789abcde",
                "0123456789abcdef0",
                "0123456789ABCDEf",
                "0123456789abcdeg",
                "AudioHub speaker",
            ] {
                assert!(!valid_peer_key(bad), "{bad:?}");
            }
            assert_eq!(PKEY_AUDIOHUB_PEER_KEY.pid, 2);
            assert_eq!(PKEY_AUDIOHUB_PEER_KEY.fmtid.d1, 0x8CA48324);
        }
    }

    // ------------------------------------------- design §4.2 native dB path

    /// Same role as the macOS constant: the fallback for `gain == 0` on an
    /// endpoint that will not say where its floor is.
    const SILENT_DB: f32 = -200.0;

    /// The published increment is read and discarded. Nothing in the design
    /// consumes a device-published step, and carrying a field that is always
    /// `None` on macOS would invite a call site that only works on Windows.
    fn volume_range_db(ep: &Endpoint) -> Option<(f32, f32)> {
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        let (mut lo, mut hi, mut step) = (0.0f32, 0.0f32, 0.0f32);
        let hr = unsafe { ((*v).get_volume_range)(ep.vol.0, &mut lo, &mut hi, &mut step) };
        (hr >= 0).then_some((lo, hi))
    }

    fn read_gain(ep: &Endpoint, target: Option<&str>, requested: Option<f32>) -> Result<GainState> {
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        let mut db: f32 = 0.0;
        check(
            unsafe { ((*v).get_master_volume_level)(ep.vol.0, &mut db) },
            &format!(
                "GetMasterVolumeLevel on {}",
                label(EndpointFlow::Output, target)
            ),
        )?;
        let mut m: i32 = 0;
        let muted = match unsafe { ((*v).get_mute)(ep.vol.0, &mut m) } {
            hr if hr >= 0 => m != 0,
            _ => false, // endpoint without a mute control: never report muted
        };
        Ok(GainState {
            requested,
            applied: db_to_gain(db),
            applied_db: db,
            range_db: volume_range_db(ep),
            muted,
            // Same reason `get` reports `adjustable`: this is the shared-mode
            // software volume, so an endpoint that activated is by definition
            // writable.
            adjustable: true,
        })
    }

    pub fn get_gain(target: Option<&str>) -> Result<GainState> {
        let ep = endpoint_volume(EndpointFlow::Output, target)?;
        read_gain(&ep, target, None)
    }

    pub fn set_gain(target: Option<&str>, gain: f32) -> Result<GainState> {
        let ep = endpoint_volume(EndpointFlow::Output, target)?;
        let range = volume_range_db(&ep);
        // gain 0 has no finite dB, so ask for the endpoint's own floor.
        let want = if gain > 0.0 {
            gain_to_db(gain)
        } else {
            range.map_or(SILENT_DB, |(lo, _)| lo)
        };
        // Unlike CoreAudio, SetMasterVolumeLevel REJECTS a dB outside the
        // published range rather than clamping it. Clamping here gives the two
        // platforms one behaviour — land on the floor, and let the readback
        // report the shortfall — instead of "macOS clamps, Windows errors",
        // which would make an out-of-range request mean two different things
        // depending on which end of the link ran it.
        let db = match range {
            Some((lo, hi)) => want.clamp(lo, hi),
            None => want,
        };
        let v = unsafe { ep.vol.vtbl::<IAudioEndpointVolumeVtbl>() };
        check(
            unsafe { ((*v).set_master_volume_level)(ep.vol.0, db, ptr::null()) },
            &format!(
                "SetMasterVolumeLevel on {}",
                label(EndpointFlow::Output, target)
            ),
        )?;
        read_gain(&ep, target, Some(gain))
    }
}

// ---------------------------------------------------------------- other

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::{EndpointFlow, GainState, VolumeState};
    use anyhow::{bail, Result};

    pub fn get(_flow: EndpointFlow, _target: Option<&str>) -> Result<VolumeState> {
        Ok(VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            mute_adjustable: false,
        })
    }

    pub fn get_audiohub_peer(_flow: EndpointFlow, _peer_key: &str) -> Result<VolumeState> {
        bail!("AudioHub peer-key endpoint lookup is only supported on Windows")
    }

    pub fn set_audiohub_peer(
        _flow: EndpointFlow,
        _peer_key: &str,
        _scalar: f32,
        _muted: bool,
    ) -> Result<VolumeState> {
        bail!("AudioHub peer-key endpoint lookup is only supported on Windows")
    }

    /// Errors rather than reporting a level: a platform with no volume backend
    /// has no dB reading, and `gain = 0.0` would be an absence dressed up as
    /// silence — see [`super::get_output_gain`].
    pub fn get_gain(_target: Option<&str>) -> Result<GainState> {
        bail!("output volume control is not implemented on this platform");
    }

    pub fn set_gain(_target: Option<&str>, _gain: f32) -> Result<GainState> {
        bail!("output volume control is not implemented on this platform");
    }

    pub fn set_volume(flow: EndpointFlow, _target: Option<&str>, _scalar: f32) -> Result<()> {
        bail!(
            "{} volume control is not implemented on this platform",
            flow.label()
        );
    }

    pub fn set_mute(flow: EndpointFlow, _target: Option<&str>, _muted: bool) -> Result<()> {
        bail!(
            "{} mute control is not implemented on this platform",
            flow.label()
        );
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod endpoint_api_tests {
    use super::*;

    #[test]
    fn input_api_has_the_same_public_shape_as_the_default_output_api() {
        let _: fn() -> Result<VolumeState> = get_default_input_volume;
        let _: fn(f32) -> Result<()> = set_default_input_volume;
        let _: fn(bool) -> Result<()> = set_default_input_mute;

        assert_eq!(EndpointFlow::Input.label(), "input");
        assert_eq!(EndpointFlow::Output.label(), "output");
    }

    #[test]
    fn both_endpoint_directions_share_finite_clamped_scalar_admission() {
        assert_eq!(normalize_scalar(-1.0).unwrap(), 0.0);
        assert_eq!(normalize_scalar(0.25).unwrap(), 0.25);
        assert_eq!(normalize_scalar(2.0).unwrap(), 1.0);
        assert!(normalize_scalar(f32::NAN).is_err());
        assert!(normalize_scalar(f32::INFINITY).is_err());
        assert!(normalize_scalar(f32::NEG_INFINITY).is_err());
    }
}

#[cfg(test)]
mod mode_a_tests {
    //! plan §7.1 / §7.2 的两个模式 A 开关。判定是纯函数，接线在
    //! `audiohubd::conn` 与 `audiohubd::poll_consumer_volume`——那里另有守卫
    //! 测试，因为一个写对了却没人调的判定函数在这里照样全绿。

    use super::*;

    fn peer(scalar: f32, muted: bool) -> VolumeState {
        VolumeState {
            scalar,
            muted,
            adjustable: true,
            mute_adjustable: true,
        }
    }

    const BOTH_OFF: ModeAVolume = ModeAVolume {
        sync: false,
        mute_local: false,
    };
    const SYNC_ONLY: ModeAVolume = ModeAVolume {
        sync: true,
        mute_local: false,
    };
    const BOTH_ON: ModeAVolume = ModeAVolume {
        sync: true,
        mute_local: true,
    };

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
        let mute_only = ModeAVolume {
            sync: false,
            mute_local: true,
        };
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
        assert_eq!(
            with.muted,
            Some(true),
            "with 「静音本机」 off the mute travels"
        );
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
        let local = VolumeState {
            scalar: 0.8,
            muted: true,
            adjustable: true,
            mute_adjustable: true,
        };
        assert_eq!(
            applied(classify_follow(true, true, BOTH_ON, local)).muted,
            None
        );
        assert_eq!(
            applied(classify_follow(true, true, SYNC_ONLY, local)).muted,
            Some(true)
        );
    }

    #[test]
    fn a_peer_scalar_outside_the_range_is_clamped_not_refused() {
        assert_eq!(
            applied(classify_follow(true, true, SYNC_ONLY, peer(9.0, false))).scalar,
            1.0
        );
        assert_eq!(
            applied(classify_follow(true, true, SYNC_ONLY, peer(-9.0, false))).scalar,
            0.0
        );
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
        let aggregate = VolumeState {
            scalar: 0.0,
            muted: false,
            adjustable: false,
            mute_adjustable: false,
        };
        assert!(
            matches!(
                classify_follow(true, true, SYNC_ONLY, aggregate),
                FollowAction::Ignore(_)
            ),
            "an aggregate device's 0.0 was adopted as a volume: this mutes the other machine, \
             and the user cannot turn it back up"
        );
        // Not about the value: any reading from a device with no control is an
        // absence, even one that happens to look plausible.
        assert!(matches!(
            classify_follow(
                true,
                true,
                SYNC_ONLY,
                VolumeState {
                    scalar: 0.4,
                    muted: false,
                    adjustable: false,
                    mute_adjustable: false,
                }
            ),
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
        assert_eq!(
            classify_mute_on_connect(true, BOTH_ON, None),
            MuteOnConnect::Mute
        );
    }

    /// plan §7.1 的技术前提：捕获点位在音量之后的后端上，静音会连镜像一起静掉。
    /// 那种失效在对端看来是「没声音」，与网络故障无从分辨，所以这条不许开火。
    #[test]
    fn a_post_mix_capture_backend_blocks_the_one_shot_mute() {
        assert!(matches!(
            classify_mute_on_connect(true, BOTH_ON, Some(false)),
            MuteOnConnect::Skip(_)
        ));
        assert_eq!(
            classify_mute_on_connect(true, BOTH_ON, Some(true)),
            MuteOnConnect::Mute
        );
        // Unknown is NOT treated as post-mix: the user asked, unmuting undoes
        // it, and a switch that silently does nothing is the worse failure.
        assert_eq!(
            classify_mute_on_connect(true, BOTH_ON, None),
            MuteOnConnect::Mute
        );
    }

    /// The consumer's echo suppression is the provider's, reused (plan §7.1
    /// 「复用 §7.2 的 volume_set/volume_state 消息与来源标记防乒乓」): a reading
    /// that is merely the device confirming what we just adopted from the peer
    /// must not travel back as a local change.
    #[test]
    fn a_value_adopted_from_the_peer_is_not_reported_back_as_ours() {
        let mut s = VolumeSync::new();
        s.note_peer_apply(0.4, false);
        assert_eq!(
            s.poll(peer(0.4, false)),
            None,
            "that is the peer's own value coming back"
        );
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
            authority_for(Some(VolumeState {
                scalar: 0.4,
                muted: false,
                adjustable: true,
                mute_adjustable: true,
            })),
            VolumeAuthority::Peer
        );
        // 对端设备不能调 ⇒ 本机接手（macOS 聚合设备：scalar 恒为 0 且不可写）。
        assert_eq!(
            authority_for(Some(VolumeState {
                scalar: 0.0,
                muted: false,
                adjustable: false,
                mute_adjustable: false,
            })),
            VolumeAuthority::SendGain
        );
        // 静音态与判据无关：能不能调是设备的事，静没静音是状态。
        assert_eq!(
            authority_for(Some(VolumeState {
                scalar: 0.9,
                muted: true,
                adjustable: false,
                mute_adjustable: false,
            })),
            VolumeAuthority::SendGain
        );
    }
}

/// design §4.2 的原生 dB 支路：单位换算与「夹取必须可见」这条类型级性质。
#[cfg(test)]
mod gain_tests {
    use super::*;

    /// The two conversions are each other's inverse across the wire range, and
    /// both ends mean what design §3.2 says they mean.
    ///
    /// 注入对照：把 `gain_to_db` 的 `20.0` 写成 `10.0`（功率 vs 幅度那个经典
    /// 错误），第二条断言 −6.02 → −3.01 立刻变红。
    #[test]
    fn gain_and_db_are_each_others_inverse() {
        assert_eq!(gain_to_db(1.0), 0.0, "unity is 0 dB");
        assert!(
            (gain_to_db(0.5) - (-6.0206)).abs() < 1e-3,
            "half amplitude is −6.02 dB, not −3.01: this is amplitude, not power. got {}",
            gain_to_db(0.5)
        );
        assert!((gain_to_db(db_to_gain(-30.0)) - (-30.0)).abs() < 1e-3);
        // gain 0 is not a level: no finite dB is silence (design §3.2).
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
        assert_eq!(gain_to_db(-1.0), f32::NEG_INFINITY);
        assert_eq!(db_to_gain(f32::NEG_INFINITY), 0.0);
        for g in [1.0f32, 0.5, 0.25, 0.1, 0.0316, 1e-4] {
            let back = db_to_gain(gain_to_db(g));
            assert!((back - g).abs() <= g * 1e-4, "{g} round-tripped to {back}");
        }
    }

    /// ⭐ The property [`GainState`] exists for, and the one design §10 P2a
    /// calls out by name: a device that clamps and still reports success must
    /// leave **both** numbers readable. Reporting only the request hides that
    /// the device refused; reporting only the result hides that anything else
    /// was ever asked for.
    ///
    /// The numbers are the measured ones (design §9, 2026-08-09): −80 dB
    /// written to `MacBook Pro Speakers`, floor −63.5 dB, `SetPropertyData`
    /// returned `noErr`, device landed at −63.5 dB and did NOT mute itself.
    ///
    /// 注入对照：把 `clamped()` 改成 `false`，或让 `requested_db()` 返回
    /// `Some(self.applied_db)`，本测试变红。
    #[test]
    fn a_silent_clamp_leaves_both_the_request_and_the_result_readable() {
        let clamped = GainState {
            requested: Some(db_to_gain(-80.0)),
            applied: db_to_gain(-63.5),
            applied_db: -63.5,
            range_db: Some((-63.5, 0.0)),
            muted: false,
            adjustable: true,
        };
        let asked = clamped
            .requested_db()
            .expect("a write must remember what it asked for");
        assert!(
            (asked - (-80.0)).abs() <= SAME_DB,
            "the request was lost, only the result survived: {asked} dB"
        );
        assert!(
            (clamped.applied_db - (-63.5)).abs() <= SAME_DB,
            "the result was lost, only the request survived: {} dB",
            clamped.applied_db
        );
        assert!(
            clamped.clamped(),
            "16.5 dB of clamp reported as compliance — this is the state that has no status code"
        );
        assert!((clamped.shortfall_db().expect("a write has a shortfall") - 16.5).abs() <= SAME_DB);

        // ...and a device that DID do what was asked must not look clamped,
        // or the flag says "clamped" about everything and means nothing.
        let complied = GainState {
            requested: Some(db_to_gain(-30.0)),
            applied: db_to_gain(-30.0),
            applied_db: -30.0,
            ..clamped
        };
        assert!(
            !complied.clamped(),
            "a compliant device must not read as clamped"
        );

        // A plain read asked for nothing, so it cannot have fallen short of it.
        let read = GainState {
            requested: None,
            ..clamped
        };
        assert_eq!(read.requested_db(), None);
        assert_eq!(read.shortfall_db(), None);
        assert!(!read.clamped());
    }

    /// design §5.1: [`SAME_EPS`] and [`SAME_DB`] are thresholds in DIFFERENT
    /// UNITS, and converting one into the other is the mistake that section is
    /// named for. Near the quiet end a `SAME_EPS`-sized step of slider is more
    /// than an order of magnitude past `SAME_DB`, so reusing the slider
    /// tolerance in the dB domain would swallow an 11 dB change as "unchanged".
    #[test]
    fn the_slider_tolerance_is_not_the_db_tolerance_in_disguise() {
        let quiet = 0.01f32;
        let step_db = gain_to_db(quiet + SAME_EPS) - gain_to_db(quiet);
        assert!(
            step_db > SAME_DB * 10.0,
            "SAME_EPS near the quiet end is {step_db:.2} dB; it is not SAME_DB ({SAME_DB} dB) \
             in another unit and must never be substituted for it"
        );
    }
}

/// design §10 P2a 的硬件判据：只有真实设备能回答的那几条。
#[cfg(test)]
mod output_gain_hardware_tests {
    //! # Why every test here is `#[ignore]`d, and the interlocks around them
    //!
    //! These WRITE a real device's volume. On this machine the system default
    //! output is a virtual speaker carrying the user's live audio to a peer, so
    //! a "turn it to −30 dB" test there is not a test, it is an outage. Three
    //! interlocks, in the order they fire:
    //!
    //! 1. **The device is required and read from the environment.** No default,
    //!    and in particular no fallback to the system default.
    //! 2. **The named device must not BE the system default output.** Checked
    //!    against the live default at the start of every test; naming it is a
    //!    panic, not a warning. This mirrors the hard refusal in
    //!    `regress/volume-taper/macdbwrite.c`, which is field-proven.
    //! 3. **[`GainGuard`] holds the knob**, so the early `?` and the panic
    //!    paths put it back too. It restores the SCALAR, and a dB write moves
    //!    the scalar with it (measured: −30 dB ⇒ scalar 0.278319 on Apple
    //!    built-in), so restoring one restores the other.
    //!
    //! And every test ends by re-reading the system default output and
    //! asserting it did not move — the "check the whole table afterwards" half
    //! of the same contract, aimed at the one device that matters.
    //!
    //! **Known gap:** unlike the C tools, there is no SIGINT/SIGTERM handler —
    //! `Drop` does not run on a signal, so `^C` mid-test leaves the named
    //! device moved. Same gap as `output_volume_guard_hardware_tests` above,
    //! accepted for the same reason: the device under test is one that carries
    //! nothing, by interlock 2.
    //!
    //! ```text
    //! AUDIOHUB_GAIN_DEVICE="BlackHole 2ch" \
    //!   cargo test -p audiohub-core output_gain_hardware -- --ignored --nocapture --test-threads=1
    //! ```
    //!
    //! `--test-threads=1` matters if `AUDIOHUB_VOLUME_GUARD_DEVICE` names the
    //! same device: the two suites hold different mutexes.

    use super::*;
    use std::sync::{Mutex, MutexGuard};

    const ENV: &str = "AUDIOHUB_GAIN_DEVICE";

    /// One device, and `cargo test` runs these on parallel threads by default.
    static DEVICE: Mutex<()> = Mutex::new(());

    /// Poison-tolerant: an assertion failure in one test must not turn the
    /// others into spurious failures about a poisoned lock.
    fn lock() -> MutexGuard<'static, ()> {
        DEVICE.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn default_output_name() -> Option<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .default_output_device()
            .and_then(|d| d.name().ok())
    }

    fn output_device_names() -> Vec<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .output_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Interlocks 1 and 2.
    fn device() -> String {
        let name = std::env::var(ENV).unwrap_or_else(|_| {
            panic!(
                "set {ENV} to an OUTPUT DEVICE NAME before running this. There is no default \
                 on purpose: the system default output is where the user's audio is."
            )
        });
        if default_output_name().as_deref() == Some(name.as_str()) {
            panic!(
                "{name:?} is the CURRENT DEFAULT OUTPUT. Refusing to write to it: that device \
                 is carrying live audio. Point {ENV} at something inaudible."
            );
        }
        name
    }

    /// Captures a device's volume and puts it back UNCONDITIONALLY on `Drop`.
    ///
    /// Deliberately not [`OutputVolumeGuard`]. That one arms its restore inside
    /// its OWN `set_volume`/`set_mute`, and everything here writes through
    /// [`set_output_gain`], which it never sees — so every `restore()` returns
    /// `Ok(())` having done nothing, which is indistinguishable from a restore
    /// that worked. Measured the hard way: the first draft of this module used
    /// it, all three tests passed, and two devices were left sitting at their
    /// dB floor.
    struct GainGuard {
        device: String,
        original: VolumeState,
        done: bool,
    }

    impl GainGuard {
        fn capture(device: &str) -> GainGuard {
            let original = get_output_volume(Some(device))
                .unwrap_or_else(|e| panic!("read {device:?} before touching it: {e}"));
            assert!(
                original.adjustable,
                "{device:?} exposes no writable volume, so nothing here could be put back"
            );
            GainGuard {
                device: device.to_string(),
                original,
                done: false,
            }
        }

        /// Puts it back and VERIFIES it landed. `Drop` is the net underneath
        /// and is silent, because a panicking `Drop` during an unwind aborts
        /// the process.
        fn restore(&mut self) -> std::result::Result<(), String> {
            if self.done {
                return Ok(());
            }
            self.done = true;
            let d = Some(self.device.as_str());
            set_output_volume(d, self.original.scalar).map_err(|e| e.to_string())?;
            let now = get_output_volume(d).map_err(|e| e.to_string())?;
            // Tight, not `SAME_EPS`: a programmatic scalar write is bit-exact
            // (design §9 P0-b), so anything looser would let a real failure
            // through as rounding.
            if (now.scalar - self.original.scalar).abs() > 1e-4 {
                return Err(format!(
                    "{:?} left at {:.6}, was {:.6}",
                    self.device, now.scalar, self.original.scalar
                ));
            }
            // Nothing here writes the mute control, so a change would mean a
            // gain write touched it — which design §3.2 says it must not.
            if now.muted != self.original.muted {
                return Err(format!(
                    "{:?} mute went {} -> {} without anyone writing it",
                    self.device, self.original.muted, now.muted
                ));
            }
            Ok(())
        }
    }

    impl Drop for GainGuard {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    /// Proof that a test which moved some other device did not move THIS one.
    struct DefaultOutputWitness(Option<VolumeState>);

    impl DefaultOutputWitness {
        fn take() -> DefaultOutputWitness {
            DefaultOutputWitness(get_output_volume(None).ok())
        }

        fn check(&self) {
            assert_eq!(
                self.0,
                get_output_volume(None).ok(),
                "the SYSTEM DEFAULT OUTPUT changed while this test ran. If you moved it \
                 yourself, that is what this is reporting; otherwise a write escaped the \
                 interlocks and landed on the user's live device"
            );
        }
    }

    /// design §10 P2a, first criterion: a −30 dB request lands within 0.75 dB.
    ///
    /// The threshold is 1.5× the 0.5 dB grid a device may quantise to, which is
    /// the physical floor — loosening it further would stop distinguishing "the
    /// dB property works" from "the dB property is decorative".
    #[test]
    #[ignore = "writes a real device's volume; needs AUDIOHUB_GAIN_DEVICE"]
    fn a_minus_30_db_request_lands_within_the_device_grid() {
        let _serial = lock();
        let dev = device();
        let witness = DefaultOutputWitness::take();

        let before = get_output_gain(Some(&dev)).expect("read the dB volume before touching it");
        assert!(
            before.adjustable,
            "{dev:?} exposes no writable dB volume: {before:?}"
        );

        let after = {
            let mut g = GainGuard::capture(&dev);
            let after = set_output_gain(Some(&dev), db_to_gain(-30.0)).expect("write −30 dB");
            g.restore().expect("restore must report its own failure");
            after
        };

        assert!(
            (after.applied_db - (-30.0)).abs() <= SAME_DB,
            "asked {dev:?} for −30 dB, it landed at {:.4} dB (threshold {SAME_DB} dB)",
            after.applied_db
        );
        assert!(
            !after.clamped(),
            "a −30 dB request should be in range: {after:?}"
        );
        assert_eq!(
            after.requested_db().map(f32::round),
            Some(-30.0),
            "the write must report what it asked for, not only what happened"
        );
        witness.check();
    }

    /// ⭐ design §10 P2a's extra criterion, straight out of the §9 finding: an
    /// out-of-range request must come back reporting BOTH numbers. The device
    /// returns success either way, so this readback is the only place the
    /// difference exists at all.
    #[test]
    #[ignore = "writes a real device's volume; needs AUDIOHUB_GAIN_DEVICE"]
    fn an_out_of_range_request_reports_both_what_was_asked_and_what_happened() {
        let _serial = lock();
        let dev = device();
        let witness = DefaultOutputWitness::take();

        let before = get_output_gain(Some(&dev)).expect("read the dB volume before touching it");
        assert!(
            before.adjustable,
            "{dev:?} exposes no writable dB volume: {before:?}"
        );
        let (floor, _) = before.range_db.unwrap_or_else(|| {
            panic!("{dev:?} publishes no dB range, so \"out of range\" has no meaning on it")
        });
        assert!(
            floor > -80.0,
            "{dev:?} reaches {floor} dB, so −80 dB is not out of range for it and this test \
             would assert nothing. Pick a device whose floor is above −80 dB."
        );

        let got = {
            let mut g = GainGuard::capture(&dev);
            let got = set_output_gain(Some(&dev), db_to_gain(-80.0)).expect("write −80 dB");
            g.restore().expect("restore must report its own failure");
            got
        };

        let asked = got
            .requested_db()
            .expect("a write must remember what it asked for");
        assert!(
            (asked - (-80.0)).abs() <= SAME_DB,
            "the request was lost: −80 dB came back as {asked} dB"
        );
        assert!(
            (got.applied_db - floor).abs() <= SAME_DB,
            "the result was lost: {dev:?} floor is {floor} dB, readback says {:.4} dB",
            got.applied_db
        );
        assert!(
            got.clamped(),
            "{:.2} dB of clamp reported as if the device had complied — the underlying call \
             returns success, so this flag is the only thing that can say otherwise",
            got.shortfall_db().unwrap_or(f32::NAN)
        );
        // Measured (design §9): a device at its dB floor does NOT mute itself,
        // so §3.2's "also set the mute control" stays the caller's job and must
        // not be smuggled in here.
        assert_eq!(
            got.muted, before.muted,
            "a gain write must not touch the mute control"
        );
        witness.check();
    }

    /// design §10 P2a, last criterion: `adjustable` and `set_output_gain` must
    /// not contradict each other on ANY output device.
    ///
    /// The write is a NO-OP — each device is written the dB it already reads —
    /// so "did the setter find something to write" becomes observable without
    /// moving a single device, which is the only way to run this over a whole
    /// machine's device list. The default output is skipped outright: even a
    /// no-op is a write.
    #[test]
    #[ignore = "no-op writes across every non-default output device"]
    fn adjustable_and_the_setter_never_contradict_each_other() {
        let _serial = lock();
        let witness = DefaultOutputWitness::take();
        let default = default_output_name();
        let mut checked = Vec::new();
        // Skips are printed, not swallowed: "5 devices agreed" means nothing
        // without "and here is what was never asked".
        let mut skipped: Vec<(String, String)> = Vec::new();
        for name in output_device_names() {
            if default.as_deref() == Some(name.as_str()) {
                skipped.push((name, "is the default output (interlock)".into()));
                continue;
            }
            // No dB property at all: not on this path, and `get` says so by
            // failing rather than by inventing a level.
            let state = match get_output_gain(Some(&name)) {
                Ok(state) => state,
                Err(e) => {
                    skipped.push((name, format!("{e}")));
                    continue;
                }
            };
            // A device above unity would be pulled DOWN by the wire's 0..=1
            // clamp, so the no-op would stop being one. Leave it alone.
            if state.applied > 1.0 {
                skipped.push((
                    name,
                    format!("sits above unity ({:.2} dB)", state.applied_db),
                ));
                continue;
            }
            let wrote = set_output_gain(Some(&name), state.applied);
            assert_eq!(
                state.adjustable,
                wrote.is_ok(),
                "{name:?}: adjustable={} but set_output_gain {}. The two are supposed to be \
                 the same predicate; one of them is greying out a control that works, or \
                 promising one that does not",
                state.adjustable,
                match &wrote {
                    Ok(g) => format!("succeeded ({:.2} dB)", g.applied_db),
                    Err(e) => format!("failed: {e}"),
                }
            );
            // "No-op" is checked, not asserted by construction. The value goes
            // dB -> gain -> dB, so it is exact only to f32 rounding (~1e-5 dB);
            // this is what makes running the sweep over a stranger's device
            // list defensible, so it is verified rather than assumed.
            if let Ok(g) = &wrote {
                assert!(
                    (g.applied_db - state.applied_db).abs() <= SAME_DB,
                    "{name:?} MOVED: this sweep is only allowed to write devices the value \
                     they already had, and {:.4} dB became {:.4} dB",
                    state.applied_db,
                    g.applied_db
                );
            }
            checked.push((name, state.adjustable));
        }
        assert!(
            !checked.is_empty(),
            "no non-default output device answered the dB path, so this asserted nothing"
        );
        eprintln!("checked {} device(s): {checked:?}", checked.len());
        eprintln!("skipped {} device(s): {skipped:#?}", skipped.len());
        witness.check();
    }
}

#[cfg(test)]
mod output_volume_guard_hardware_tests {
    //! The one property of [`OutputVolumeGuard`] no pure test can reach: that a
    //! REAL device's volume comes back after a real write, including when the
    //! body between the write and the restore panics.
    //!
    //! # Why every test here is `#[ignore]`d and needs a device named
    //!
    //! It turns a volume knob. On a machine running AudioHub the system default
    //! output is the user's live audio path — on the development host it is
    //! currently a mode B virtual speaker carrying real audio to a peer, where
    //! a 「turn it to 25% and mute it」 test is not a test, it is an outage. So
    //! the device is **required** and read from the environment: there is no
    //! default, and in particular no fallback to the system default.
    //!
    //! Point it at something inaudible (a virtual card with nothing behind it):
    //!
    //! ```text
    //! AUDIOHUB_VOLUME_GUARD_DEVICE="BlackHole 2ch" \
    //!   cargo test -p audiohub-core output_volume_guard_hardware -- --ignored --nocapture
    //! ```

    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Mutex, MutexGuard};

    const ENV: &str = "AUDIOHUB_VOLUME_GUARD_DEVICE";

    /// The device is one shared piece of global state and `cargo test` runs
    /// these on parallel threads by default.
    ///
    /// This is not tidiness — without it the suite is theatre. Measured: with
    /// `Drop`'s restore deliberately deleted, both tests still passed, because
    /// the other test's restore ran between the panic and the read-back and
    /// handed it a clean device. A test that stays green while the property it
    /// names is gone is worse than no test.
    static DEVICE: Mutex<()> = Mutex::new(());

    /// Poison-tolerant: an assertion failure in one test must not turn the
    /// other into a spurious failure about a poisoned lock.
    fn lock() -> MutexGuard<'static, ()> {
        DEVICE.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn device() -> String {
        std::env::var(ENV).unwrap_or_else(|_| {
            panic!(
                "set {ENV} to an OUTPUT DEVICE NAME before running this. There is no default \
                 on purpose: the system default output is where the user's audio is."
            )
        })
    }

    /// `restore` is only meaningful if the write it undoes actually landed. A
    /// device whose volume control is a no-op knob would make every restore
    /// assertion pass without anything having happened, so the mid-run reading
    /// is checked first and the test refuses such a device by name.
    #[test]
    #[ignore = "moves a real volume knob; needs AUDIOHUB_VOLUME_GUARD_DEVICE"]
    fn a_real_write_lands_and_is_undone() {
        let _serial = lock();
        let dev = device();
        let before = get_output_volume(Some(&dev)).expect("read the device before touching it");
        assert!(before.adjustable, "{dev:?} reports no writable volume");
        assert!(
            before.scalar > 0.2,
            "{dev:?} is at {:.3}; raise it before running, this test only turns volumes down",
            before.scalar
        );

        let mid = {
            let mut g = OutputVolumeGuard::capture(Some(&dev)).expect("capture the guard");
            let mid = g
                .set_volume(before.scalar * 0.25)
                .expect("write the volume");
            g.restore().expect("restore must report its own failure");
            mid
        };
        assert!(
            (mid.scalar - before.scalar).abs() > SAME_EPS,
            "the write did not land: {dev:?} read back {:.4} where it was {:.4}. Its volume \
             control is a no-op knob, so nothing this test asserts afterwards means anything",
            mid.scalar,
            before.scalar
        );

        let after = get_output_volume(Some(&dev)).expect("read the device back");
        assert!(
            (after.scalar - before.scalar).abs() <= SAME_EPS,
            "volume left at {:.4}, was {:.4}",
            after.scalar,
            before.scalar
        );
        assert_eq!(after.muted, before.muted);
    }

    /// The path nobody writes code for: a measurement leg panics halfway. `Drop`
    /// is the only thing standing between that and a user whose speakers are
    /// left muted at a quarter volume.
    #[test]
    #[ignore = "moves a real volume knob; needs AUDIOHUB_VOLUME_GUARD_DEVICE"]
    fn a_panic_between_the_write_and_the_restore_still_puts_it_back() {
        let _serial = lock();
        let dev = device();
        let before = get_output_volume(Some(&dev)).expect("read the device before touching it");
        assert!(
            before.adjustable && before.scalar > 0.2,
            "{dev:?}: {before:?}"
        );

        let caught = catch_unwind(AssertUnwindSafe(|| {
            let mut g = OutputVolumeGuard::capture(Some(&dev)).expect("capture the guard");
            g.set_volume(before.scalar * 0.25)
                .expect("write the volume");
            g.set_mute(true).expect("write the mute");
            panic!("injected: a measurement leg died here");
        }));
        assert!(caught.is_err(), "the injected panic did not happen");

        let after = get_output_volume(Some(&dev)).expect("read the device back");
        assert!(
            (after.scalar - before.scalar).abs() <= SAME_EPS,
            "volume left at {:.4} after a panic, was {:.4}",
            after.scalar,
            before.scalar
        );
        assert_eq!(
            after.muted, before.muted,
            "mute left at {} after a panic, was {}",
            after.muted, before.muted
        );
    }
}
