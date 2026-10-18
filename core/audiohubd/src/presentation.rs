//! Consumer-side physical device presentation policy,
//! docs/physical-presentation-1.0.2-design.md §5 (platform target values of
//! §6.1–6.2 and §7.2–7.5).
//!
//! Everything that decides HOW a peer's virtual device is presented lives here
//! and is pure: [`presentation_for`] (the single decision), the naming rules,
//! the macOS / Windows target descriptors the driver layers consume, the
//! microphone mismatch gate and the rebuild scheduler. `haldev` feeds it once
//! per coordinator pass and carries the result to the drivers; the drivers
//! report back what they actually present ([`PresentationState::mark_applied`]).
//!
//! Fail-closed throughout: every doubt (no facts, invalid facts, a peer that
//! does not speak DeviceFacts, a driver that cannot or may not present, the
//! consumer switch off, an injected record in a build that refuses it) ends
//! in [`Presentation::Virtual`]. There is no partial physical presentation.
//!
//! The microphone gate closes the one dangerous window: a device that still
//! LOOKS physical (what the driver presents) while the facts no longer say so.
//! For as long as that holds, the samples written into that peer's virtual
//! microphone are silence (design §5.4). The speaker direction is not gated:
//! playing into a remote device is not the laundering direction.

// The driver-facing half (target descriptors, `set_driver`, `mark_applied`,
// the whitelists) is consumed by the platform layers that send the HAL
// presentation message and the VAD10 bind (design §11 T5/T6); until they are
// wired, a non-test build only reaches the decision, the gate and status.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use audiohub_core::device_facts::{
    reason, DeviceClass, Direction, DirectionFacts, FactsOrigin, FormFactor, PhysicalDescriptor,
    PhysicalTransport,
};

use crate::device_facts_pub::{local_reason, PeerFactsView};
use crate::haldev::HAL_MAX_SLOTS;

/// Index of a direction in every `[_; 2]` of this module and of
/// `haldev::DesiredDevice::presentation`: output first, as in the design.
pub(crate) const OUT: usize = 0;
pub(crate) const IN: usize = 1;
const DIRECTIONS: [Direction; 2] = [Direction::Output, Direction::Input];

pub(crate) fn dir_index(direction: Direction) -> usize {
    match direction {
        Direction::Output => OUT,
        Direction::Input => IN,
    }
}

// ------------------------------------------------------------- decision

/// What one direction of a peer's virtual device looks like to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presentation {
    /// Today's look: transport Virtual, "AudioHub – <peer>".
    Virtual,
    /// Mirror the provider's real device (descriptive fields only).
    Physical(PhysicalDescriptor),
}

impl Default for Presentation {
    fn default() -> Self {
        Presentation::Virtual
    }
}

impl Presentation {
    /// The provider device this presentation stands for; `None` = virtual.
    /// Two presentations with equal identity describe the same real device
    /// (a rename of that device is not a different device).
    pub(crate) fn identity(&self) -> Option<&str> {
        match self {
            Presentation::Virtual => None,
            Presentation::Physical(d) => Some(&d.device_token),
        }
    }

    pub(crate) fn descriptor(&self) -> Option<&PhysicalDescriptor> {
        match self {
            Presentation::Virtual => None,
            Presentation::Physical(d) => Some(d),
        }
    }

    pub(crate) fn is_physical(&self) -> bool {
        matches!(self, Presentation::Physical(_))
    }

    pub(crate) fn same_device(&self, other: &Presentation) -> bool {
        self.identity() == other.identity()
    }

    fn to_json(&self) -> Value {
        match self {
            Presentation::Virtual => json!({ "class": "virtual" }),
            Presentation::Physical(d) => json!({
                "class": "physical",
                "name": d.name,
                "transport": d.transport,
                "form_factor": d.form_factor,
                "manufacturer": d.manufacturer,
                "device_token": d.device_token,
                "group": d.group,
            }),
        }
    }
}

/// What the attached driver allows for this daemon. Supplied by the platform
/// layers (macOS: a presentation ack has been seen; Windows: VAD10 caps and
/// the strongest caller check). Anything but `Supported` forces Virtual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DriverPresentation {
    /// No driver, or one that predates presentation (HAL4 without ack, VAD9).
    #[default]
    Unsupported,
    /// The driver understands presentation but the caller check is weaker
    /// than the level physical presentation requires (design §6.6, §7.6);
    /// it would force Virtual anyway.
    CallerCheckInsufficient,
    Supported,
}

impl DriverPresentation {
    pub(crate) fn label(self) -> &'static str {
        match self {
            DriverPresentation::Unsupported => "unsupported",
            DriverPresentation::CallerCheckInsufficient => "caller_check_insufficient",
            DriverPresentation::Supported => "supported",
        }
    }
}

/// Reasons this module adds to the provider's own (`[a-z0-9_]`, like
/// `audiohub_core::device_facts::reason`).
pub(crate) mod why {
    pub const PHYSICAL: &str = "physical";
    pub const NOT_EVALUATED: &str = "not_evaluated";
    /// `present_physical_devices` is off on this (consumer) machine.
    pub const CONSUMER_DISABLED: &str = "consumer_disabled";
    pub const DRIVER_UNSUPPORTED: &str = "driver_unsupported";
    pub const DRIVER_CALLER_CHECK: &str = "driver_caller_check";
    /// Neither live nor persisted facts exist for this direction.
    pub const NO_FACTS: &str = "no_facts";
}

pub(crate) struct PresentationInputs<'a> {
    /// Setting `present_physical_devices`.
    pub consumer_enabled: bool,
    pub driver: DriverPresentation,
    /// The live connection's `device_facts_version`; `None` = not connected
    /// or not yet heard.
    pub peer_facts_version: Option<u8>,
    /// Live facts, else the persisted ones (design §4.5).
    pub facts: Option<&'a DirectionFacts>,
    /// `device_facts_pub::ACCEPT_INJECTED`.
    pub accept_injected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub presentation: Presentation,
    /// Why (for logs, status and the UI's explanation).
    pub reason: String,
}

impl Default for Decision {
    fn default() -> Self {
        Decision {
            presentation: Presentation::Virtual,
            reason: why::NOT_EVALUATED.to_string(),
        }
    }
}

/// [`presentation_for`] plus the reason.
pub(crate) fn decide(i: &PresentationInputs) -> Decision {
    let virtual_ = |why: &str| Decision {
        presentation: Presentation::Virtual,
        reason: why.to_string(),
    };
    if !i.consumer_enabled {
        return virtual_(why::CONSUMER_DISABLED);
    }
    match i.driver {
        DriverPresentation::Unsupported => return virtual_(why::DRIVER_UNSUPPORTED),
        DriverPresentation::CallerCheckInsufficient => return virtual_(why::DRIVER_CALLER_CHECK),
        DriverPresentation::Supported => {}
    }
    if i.peer_facts_version == Some(0) {
        return virtual_(local_reason::PEER_UNSUPPORTED);
    }
    let Some(f) = i.facts else {
        return virtual_(why::NO_FACTS);
    };
    // Re-validated here even though the receive path sanitises: persisted
    // records come from disk, and this function is the gate.
    if !f.is_valid() {
        return virtual_(reason::INVALID_FACTS);
    }
    if f.origin == FactsOrigin::Injected && !i.accept_injected {
        return virtual_(local_reason::INJECTED_REFUSED);
    }
    match (f.class, f.physical_descriptor()) {
        (DeviceClass::Physical, Some(d)) => Decision {
            presentation: Presentation::Physical(d.clone()),
            reason: why::PHYSICAL.to_string(),
        },
        _ => virtual_(&f.reason),
    }
}

/// The single decision of design §5.1: Physical only when every condition
/// holds, otherwise Virtual.
pub(crate) fn presentation_for(i: &PresentationInputs) -> Presentation {
    decide(i).presentation
}

// --------------------------------------------------------------- naming

/// macOS `char[128]` name buffers: UTF-8 bytes without the terminator.
pub(crate) const MAC_NAME_MAX_BYTES: usize = 127;
/// Windows `AH_DISPLAY_CHARS` (UTF-16 units including the terminator).
pub(crate) const WIN_NAME_MAX_UNITS: usize = crate::halbridge_win::wire::DISPLAY_CHARS - 1;

#[derive(Debug, Clone, Copy)]
enum Measure {
    Utf8Bytes,
    Utf16Units,
}

impl Measure {
    fn len(self, s: &str) -> usize {
        match self {
            Measure::Utf8Bytes => s.len(),
            Measure::Utf16Units => s.encode_utf16().count(),
        }
    }

    /// The longest prefix of `s` that measures at most `max`, cut on a
    /// CHARACTER boundary (never half a code point or surrogate pair).
    fn clamp(self, s: &str, max: usize) -> &str {
        let mut used = 0;
        for (i, ch) in s.char_indices() {
            let w = match self {
                Measure::Utf8Bytes => ch.len_utf8(),
                Measure::Utf16Units => ch.len_utf16(),
            };
            if used + w > max {
                return &s[..i];
            }
            used += w;
        }
        s
    }
}

/// Control characters removed (a remote name is validated, a persisted one
/// is re-validated, but the label is local and may come from anywhere),
/// surrounding whitespace trimmed.
fn clean(s: &str) -> String {
    let kept: String = s.chars().filter(|c| !c.is_control()).collect();
    kept.trim().to_string()
}

/// `"{left}{open}{label}{close}{mark}"`, measured by `m` and at most `budget`.
///
/// Priorities when it does not fit: the offline mark is never cut (it is the
/// state the user needs to see); the peer label keeps at least half of what
/// is left (it is the disambiguator — two "Speakers" without it are the
/// confusion the policy exists to avoid); the device name gets the rest.
fn compose(
    left: &str,
    label: &str,
    open: &str,
    close: &str,
    mark: &str,
    m: Measure,
    budget: usize,
) -> String {
    let left = clean(left);
    let label = clean(label);
    let mark_len = m.len(mark);
    if label.is_empty() || left.is_empty() {
        let only = if left.is_empty() { label } else { left };
        let only = if only.is_empty() {
            "Unknown".to_string()
        } else {
            only
        };
        let room = budget.saturating_sub(mark_len);
        return format!("{}{mark}", m.clamp(&only, room).trim_end());
    }
    let avail = budget.saturating_sub(m.len(open) + m.len(close) + mark_len);
    let (ll, bl) = (m.len(&left), m.len(&label));
    let (left_c, label_c) = if ll + bl <= avail {
        (left.as_str(), label.as_str())
    } else {
        let label_cap = avail.saturating_sub(ll).max(avail / 2).min(bl);
        let label_c = m.clamp(&label, label_cap).trim_end();
        let left_c = m
            .clamp(&left, avail.saturating_sub(m.len(label_c)))
            .trim_end();
        (left_c, label_c)
    };
    if left_c.is_empty() {
        return format!("{label_c}{mark}");
    }
    format!("{left_c}{open}{label_c}{close}{mark}")
}

/// macOS device name of a physical presentation (design §5.2, Q12):
/// `"{provider device name} ({peer label})"` plus the unchanged localized
/// offline mark, e.g. "MacBook Pro Speakers (客厅 Mac)". At most 127 UTF-8
/// bytes. `peer_label` is the disambiguated label haldev already uses
/// (alias or computer name, ` (2)` for duplicates), without any prefix.
pub(crate) fn mac_device_name(
    d: &PhysicalDescriptor,
    peer_label: &str,
    offline: bool,
    locale: &str,
) -> String {
    let mark = if offline {
        crate::haldev::offline_mark(locale)
    } else {
        ""
    };
    compose(
        &d.name,
        peer_label,
        " (",
        ")",
        mark,
        Measure::Utf8Bytes,
        MAC_NAME_MAX_BYTES,
    )
}

/// Windows Phase 2 endpoint half (EP\0 `PKEY_Device_DeviceDesc`):
/// `"{endpoint_desc}"` (+ offline mark), e.g. "Headphones". The system
/// appends the child devnode's FriendlyName ([`windows_adapter_name`], which
/// carries the peer label) as the bracketed second half, so the full name
/// reads "Headphones (Realtek(R) Audio – WIN-30)" (design §5.2, Windows P2).
/// Falls back to the full provider name when the provider sent no endpoint
/// half (a macOS provider). At most 127 UTF-16 units.
pub(crate) fn windows_endpoint_desc(d: &PhysicalDescriptor, offline: bool, locale: &str) -> String {
    let mark = if offline {
        crate::haldev::offline_mark(locale)
    } else {
        ""
    };
    let left = d.endpoint_desc.as_deref().unwrap_or(&d.name);
    compose(left, "", "", "", mark, Measure::Utf16Units, WIN_NAME_MAX_UNITS)
}

/// The Windows endpoint half of a VIRTUAL direction (v11): the direction's
/// plain noun in the UI language (+ offline mark), e.g. "Speakers" or
/// "麦克风（离线）". The bracketed half is the peer's virtual devnode,
/// [`windows_virtual_adapter_name`], so the full name reads
/// "Speakers (AudioHub – WIN-30)".
pub(crate) fn windows_virtual_endpoint_desc(direction: Direction, offline: bool, locale: &str) -> String {
    let noun = match (direction, locale == "en-US") {
        (Direction::Output, true) => "Speakers",
        (Direction::Input, true) => "Microphone",
        (Direction::Output, false) => "扬声器",
        (Direction::Input, false) => "麦克风",
    };
    let mark = if offline {
        crate::haldev::offline_mark(locale)
    } else {
        ""
    };
    compose(noun, "", "", "", mark, Measure::Utf16Units, WIN_NAME_MAX_UNITS)
}

/// The FriendlyName of a peer's virtual child devnode (v11): the
/// cross-platform device name `"AudioHub – {peer label}"`, byte-identical to
/// the macOS virtual device name (plan 7.1) and unique per peer because the
/// label is disambiguated. No offline mark: the devnode name is fixed when
/// the child is created, and the mark lives in the endpoint half.
pub(crate) fn windows_virtual_adapter_name(peer_label: &str) -> String {
    let stem = crate::haldev::device_name_stem(&clean(peer_label));
    Measure::Utf16Units.clamp(&stem, WIN_NAME_MAX_UNITS).trim_end().to_string()
}

/// Windows Phase 2 child-devnode FriendlyName (the endpoint name's second
/// half): `"{adapter_name} – {peer label}"`, e.g. "Realtek(R) Audio – WIN-30";
/// just the label when the provider has no adapter half. No offline mark:
/// it is fixed when the child node is created. At most 127 UTF-16 units.
pub(crate) fn windows_adapter_name(d: &PhysicalDescriptor, peer_label: &str) -> String {
    match d.adapter_name.as_deref() {
        Some(adapter) => compose(
            adapter,
            peer_label,
            " \u{2013} ",
            "",
            "",
            Measure::Utf16Units,
            WIN_NAME_MAX_UNITS,
        ),
        None => compose(
            "",
            peer_label,
            "",
            "",
            "",
            Measure::Utf16Units,
            WIN_NAME_MAX_UNITS,
        ),
    }
}

/// Whether the provider sent a usable adapter half (a Windows provider).
fn has_adapter_half(d: &PhysicalDescriptor) -> bool {
    d.adapter_name.as_deref().is_some_and(|a| !clean(a).is_empty())
}

/// Endpoint half of a standalone physical unit whose provider sent no adapter
/// half (a macOS provider): the shaped form factor's plain noun in the UI
/// language (+ offline mark), e.g. "Headphones" or "麦克风". `form_factor` is
/// the EndpointFormFactor the bind carries for this direction.
pub(crate) fn windows_standalone_endpoint_desc(
    direction: Direction,
    form_factor: u32,
    offline: bool,
    locale: &str,
) -> String {
    use win::ff;
    let en = locale == "en-US";
    let noun = match (direction, form_factor, en) {
        (Direction::Output, ff::HEADPHONES, true) => "Headphones",
        (Direction::Output, ff::HEADPHONES, false) => "耳机",
        (Direction::Output, ff::HEADSET, true) => "Headset",
        (Direction::Output, ff::HEADSET, false) => "头戴式耳机",
        (Direction::Output, ff::HANDSET, true) => "Handset",
        (Direction::Output, ff::HANDSET, false) => "听筒",
        (Direction::Output, _, true) => "Speakers",
        (Direction::Output, _, false) => "扬声器",
        (Direction::Input, ff::HEADSET, true) => "Headset Microphone",
        (Direction::Input, ff::HEADSET, false) => "耳机麦克风",
        (Direction::Input, ff::HANDSET, true) => "Handset",
        (Direction::Input, ff::HANDSET, false) => "听筒",
        (Direction::Input, _, true) => "Microphone",
        (Direction::Input, _, false) => "麦克风",
    };
    let mark = if offline {
        crate::haldev::offline_mark(locale)
    } else {
        ""
    };
    compose(noun, "", "", "", mark, Measure::Utf16Units, WIN_NAME_MAX_UNITS)
}

/// Child-devnode FriendlyName of a standalone physical unit without an adapter
/// half: `"{provider device name} – {peer label}"`, e.g. "MacBook Pro
/// Microphone – Mac". A macOS provider's ungrouped output and input are two
/// child devnodes; naming both after the peer alone made Windows prefix the
/// second endpoint with "2- " (identical devnode names, spike S6).
pub(crate) fn windows_standalone_adapter_name(d: &PhysicalDescriptor, peer_label: &str) -> String {
    compose(
        &d.name,
        peer_label,
        " \u{2013} ",
        "",
        "",
        Measure::Utf16Units,
        WIN_NAME_MAX_UNITS,
    )
}

/// Q10: a class-driver placeholder is not a vendor. The provider already
/// drops the Windows ones; this re-applies the rule to whatever arrives.
fn trustworthy_manufacturer(d: &PhysicalDescriptor) -> Option<String> {
    let text = clean(d.manufacturer.as_deref()?);
    let generic = text.is_empty()
        || (text.starts_with('(') && text.ends_with(')'))
        || text.eq_ignore_ascii_case("microsoft")
        || text.eq_ignore_ascii_case("microsoft corporation")
        || text.eq_ignore_ascii_case("generic")
        || text.eq_ignore_ascii_case("unknown");
    (!generic).then_some(text)
}

pub(crate) const UNKNOWN_MANUFACTURER: &str = "Unknown";

/// Both directions present one provider device iff both are physical and
/// carry the same non-zero group (design §6.1 RelatedDevices, §7.3 group).
pub(crate) fn related_group(out: &Presentation, inp: &Presentation) -> u32 {
    match (out.descriptor(), inp.descriptor()) {
        (Some(o), Some(i)) if o.group != 0 && o.group == i.group => u32::from(o.group),
        _ => 0,
    }
}

// ------------------------------------------------------- macOS target

/// Values the macOS HAL presentation message carries (design §6.4), for T5.
pub(crate) mod mac {
    use super::*;

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }

    pub(crate) const KLASS_VIRTUAL: u32 = 0;
    pub(crate) const KLASS_PHYSICAL: u32 = 1;

    /// kAudioDeviceTransportType*. `WHITELIST` is the set §6.5 lets the
    /// driver accept; Bluetooth/BLE/AirPlay/Aggregate/AVB/Continuity are never
    /// produced here.
    pub(crate) mod transport {
        use super::fourcc;
        pub(crate) const VIRTUAL: u32 = fourcc(b"virt");
        pub(crate) const BUILT_IN: u32 = fourcc(b"bltn");
        pub(crate) const USB: u32 = fourcc(b"usb ");
        pub(crate) const HDMI: u32 = fourcc(b"hdmi");
        pub(crate) const DISPLAYPORT: u32 = fourcc(b"dprt");
        pub(crate) const THUNDERBOLT: u32 = fourcc(b"thun");
        pub(crate) const PCI: u32 = fourcc(b"pci ");
        pub(crate) const FIREWIRE: u32 = fourcc(b"1394");
        pub(crate) const WHITELIST: [u32; 8] = [
            VIRTUAL,
            BUILT_IN,
            USB,
            HDMI,
            DISPLAYPORT,
            THUNDERBOLT,
            PCI,
            FIREWIRE,
        ];
    }

    /// kAudioStreamTerminalType*.
    pub(crate) mod terminal {
        use super::fourcc;
        pub(crate) const SPEAKER: u32 = fourcc(b"spkr");
        pub(crate) const HEADPHONES: u32 = fourcc(b"hdph");
        pub(crate) const RECEIVER_SPEAKER: u32 = fourcc(b"rspk");
        pub(crate) const LINE: u32 = fourcc(b"line");
        pub(crate) const DIGITAL_AUDIO_INTERFACE: u32 = fourcc(b"spdf");
        pub(crate) const HDMI: u32 = fourcc(b"hdmi");
        pub(crate) const DISPLAYPORT: u32 = fourcc(b"dprt");
        pub(crate) const MICROPHONE: u32 = fourcc(b"micr");
        pub(crate) const HEADSET_MICROPHONE: u32 = fourcc(b"hmic");
        pub(crate) const RECEIVER_MICROPHONE: u32 = fourcc(b"rmic");
        pub(crate) const OUTPUT_WHITELIST: [u32; 7] = [
            SPEAKER,
            HEADPHONES,
            RECEIVER_SPEAKER,
            LINE,
            DIGITAL_AUDIO_INTERFACE,
            HDMI,
            DISPLAYPORT,
        ];
        pub(crate) const INPUT_WHITELIST: [u32; 5] = [
            MICROPHONE,
            HEADSET_MICROPHONE,
            RECEIVER_MICROPHONE,
            LINE,
            DIGITAL_AUDIO_INTERFACE,
        ];
    }

    /// `icon_kind` of the message.
    pub(crate) mod icon {
        pub(crate) const NONE: u32 = 0;
        pub(crate) const SPEAKERS: u32 = 1;
        pub(crate) const HEADPHONES: u32 = 2;
        pub(crate) const HEADSET: u32 = 3;
        pub(crate) const MICROPHONE: u32 = 4;
    }

    /// `char manufacturer[64]` / `char model_suffix[32]`, terminator included.
    pub(crate) const MANUFACTURER_BUF: usize = 64;
    pub(crate) const MODEL_SUFFIX_BUF: usize = 32;
    pub(crate) const VIRTUAL_MANUFACTURER: &str = "AudioHub";
    /// ModelUID stays in this project's namespace (never the provider's).
    pub(crate) const MODEL_UID_PREFIX: &str = "AudioHub_Remote_";
    /// Today's ModelUIDs, kept for virtual presentation.
    pub(crate) const VIRTUAL_MODEL_UID_OUT: &str = "AudioHub_PeerOutput";
    pub(crate) const VIRTUAL_MODEL_UID_IN: &str = "AudioHub_PeerInput";

    /// Q3: Bluetooth (and BLE) present as USB — the local link is a network,
    /// not A2DP/HFP, and Bluetooth would trigger system Bluetooth behaviour.
    pub(crate) fn transport_for(t: PhysicalTransport) -> u32 {
        match t {
            PhysicalTransport::BuiltIn => transport::BUILT_IN,
            PhysicalTransport::Usb | PhysicalTransport::Bluetooth => transport::USB,
            PhysicalTransport::Hdmi => transport::HDMI,
            PhysicalTransport::DisplayPort => transport::DISPLAYPORT,
            PhysicalTransport::Thunderbolt => transport::THUNDERBOLT,
            PhysicalTransport::Pci => transport::PCI,
            PhysicalTransport::FireWire => transport::FIREWIRE,
        }
    }

    /// (terminal type, icon, model suffix) for one direction. A form factor
    /// that cannot exist in that direction degrades to the plain one.
    fn shape(direction: Direction, d: &PhysicalDescriptor) -> (u32, u32, &'static str) {
        match direction {
            Direction::Output => match d.form_factor {
                FormFactor::Headphones => (terminal::HEADPHONES, icon::HEADPHONES, "Headphones"),
                FormFactor::Headset => (terminal::HEADPHONES, icon::HEADSET, "Headset"),
                FormFactor::Handset => (terminal::RECEIVER_SPEAKER, icon::SPEAKERS, "Handset"),
                FormFactor::LineLevel => (terminal::LINE, icon::SPEAKERS, "LineLevel"),
                FormFactor::Spdif => (terminal::DIGITAL_AUDIO_INTERFACE, icon::SPEAKERS, "Spdif"),
                FormFactor::DigitalDisplay if d.transport == PhysicalTransport::DisplayPort => {
                    (terminal::DISPLAYPORT, icon::SPEAKERS, "DigitalDisplay")
                }
                FormFactor::DigitalDisplay => (terminal::HDMI, icon::SPEAKERS, "DigitalDisplay"),
                FormFactor::Speakers | FormFactor::Microphone | FormFactor::Unknown => {
                    (terminal::SPEAKER, icon::SPEAKERS, "Speakers")
                }
            },
            Direction::Input => match d.form_factor {
                FormFactor::Headset => (terminal::HEADSET_MICROPHONE, icon::HEADSET, "Headset"),
                FormFactor::Handset => (terminal::RECEIVER_MICROPHONE, icon::MICROPHONE, "Handset"),
                FormFactor::LineLevel => (terminal::LINE, icon::MICROPHONE, "LineLevel"),
                FormFactor::Spdif => (terminal::DIGITAL_AUDIO_INTERFACE, icon::MICROPHONE, "Spdif"),
                FormFactor::Microphone
                | FormFactor::Speakers
                | FormFactor::Headphones
                | FormFactor::DigitalDisplay
                | FormFactor::Unknown => (terminal::MICROPHONE, icon::MICROPHONE, "Microphone"),
            },
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct MacPresentationTarget {
        pub klass: u32,
        pub transport: u32,
        pub terminal_type: u32,
        pub icon_kind: u32,
        pub related_group: u32,
        /// At most `MANUFACTURER_BUF - 1` UTF-8 bytes.
        pub manufacturer: String,
        /// `[A-Za-z]+`, at most `MODEL_SUFFIX_BUF - 1` bytes; empty when
        /// virtual (the driver keeps today's ModelUID).
        pub model_suffix: &'static str,
    }

    impl MacPresentationTarget {
        pub(crate) fn model_uid(&self, direction: Direction) -> String {
            if self.klass == KLASS_VIRTUAL {
                match direction {
                    Direction::Output => VIRTUAL_MODEL_UID_OUT.to_string(),
                    Direction::Input => VIRTUAL_MODEL_UID_IN.to_string(),
                }
            } else {
                format!("{MODEL_UID_PREFIX}{}", self.model_suffix)
            }
        }
    }

    /// The message fields for one direction. The NAME is not here: it travels
    /// on the existing Bind/rename path (design §6.4) — `haldev` puts
    /// [`mac_device_name`] into the Bind for a physical presentation.
    pub(crate) fn target(
        direction: Direction,
        p: &Presentation,
        related_group: u32,
    ) -> MacPresentationTarget {
        match p {
            Presentation::Virtual => MacPresentationTarget {
                klass: KLASS_VIRTUAL,
                transport: transport::VIRTUAL,
                terminal_type: match direction {
                    Direction::Output => terminal::SPEAKER,
                    Direction::Input => terminal::MICROPHONE,
                },
                icon_kind: icon::NONE,
                related_group: 0,
                manufacturer: VIRTUAL_MANUFACTURER.to_string(),
                model_suffix: "",
            },
            Presentation::Physical(d) => {
                let (terminal_type, icon_kind, model_suffix) = shape(direction, d);
                let vendor =
                    trustworthy_manufacturer(d).unwrap_or_else(|| UNKNOWN_MANUFACTURER.to_string());
                MacPresentationTarget {
                    klass: KLASS_PHYSICAL,
                    transport: transport_for(d.transport),
                    terminal_type,
                    icon_kind,
                    related_group,
                    manufacturer: Measure::Utf8Bytes
                        .clamp(&vendor, MANUFACTURER_BUF - 1)
                        .trim_end()
                        .to_string(),
                    model_suffix,
                }
            }
        }
    }
}

// ----------------------------------------------------- Windows target

/// Values the Windows VAD10 `AH_PRESENTATION` carries and the per-slot pin
/// choice (design §7.2–7.4), for T6.
pub(crate) mod win {
    use super::*;

    pub(crate) const PRES_VIRTUAL: u32 = 0;
    pub(crate) const PRES_PHYSICAL: u32 = 1;

    /// EndpointFormFactor values (mmdeviceapi.h), the `form_factor` field.
    /// The complete table, including values this mapping never produces.
    #[allow(dead_code)]
    pub(crate) mod ff {
        pub(crate) const SPEAKERS: u32 = 1;
        pub(crate) const LINE_LEVEL: u32 = 2;
        pub(crate) const HEADPHONES: u32 = 3;
        pub(crate) const MICROPHONE: u32 = 4;
        pub(crate) const HEADSET: u32 = 5;
        pub(crate) const HANDSET: u32 = 6;
        pub(crate) const SPDIF: u32 = 8;
        pub(crate) const DIGITAL_AUDIO_DISPLAY_DEVICE: u32 = 9;
    }

    /// The bridge pin's KS category for the slot's topology filter.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum PinCategory {
        Speaker,
        Headphones,
        HeadsetSpeakers,
        Handset,
        Microphone,
        HeadsetMicrophone,
        MicrophoneArray,
    }

    impl PinCategory {
        /// The ksmedia.h identifier the driver maps this to.
        pub(crate) fn ksnodetype(self) -> &'static str {
            match self {
                PinCategory::Speaker => "KSNODETYPE_SPEAKER",
                PinCategory::Headphones => "KSNODETYPE_HEADPHONES",
                PinCategory::HeadsetSpeakers => "KSNODETYPE_HEADSET_SPEAKERS",
                PinCategory::Handset => "KSNODETYPE_HANDSET",
                PinCategory::Microphone => "KSNODETYPE_MICROPHONE",
                PinCategory::HeadsetMicrophone => "KSNODETYPE_HEADSET_MICROPHONE",
                PinCategory::MicrophoneArray => "KSNODETYPE_MICROPHONE_ARRAY",
            }
        }
    }

    /// `perpeer.h` `AH_REFSTRING_MAX`, WCHARs INCLUDING the terminator.
    pub(crate) const REFSTRING_MAX: usize = 64;
    /// `perpeer.h` `AH_TEMPLATE_*`.
    pub(crate) const TEMPLATES: [&str; 4] = ["AhTopoOut", "AhWaveOut", "AhTopoIn", "AhWaveIn"];
    /// Hex characters of a presentation key (`key8`).
    pub(crate) const PRESKEY_HEX: usize = 8;
    /// Design §7.3 `AH_PRESKEY_BUF`.
    pub(crate) const PRESKEY_BUF: usize = 24;
    /// Design §7.3 `AH_MFG_CHARS`, UTF-16 units including the terminator.
    pub(crate) const MFG_CHARS: usize = 64;

    // The policy's constants ARE the wire's (halbridge_win::wire mirrors
    // AudioHubIoctl.h); a drift is a build failure, not a refused bind.
    const _: () = {
        use crate::halbridge_win::wire as w;
        assert!(PRES_VIRTUAL == w::PRES_VIRTUAL && PRES_PHYSICAL == w::PRES_PHYSICAL);
        assert!(PRESKEY_HEX == w::PRESKEY_CHARS && PRESKEY_BUF == w::PRESKEY_BUF);
        assert!(MFG_CHARS == w::MFG_CHARS);
        assert!(ff::SPEAKERS == w::FF_SPEAKERS && ff::HEADPHONES == w::FF_HEADPHONES);
        assert!(ff::MICROPHONE == w::FF_MICROPHONE && ff::HEADSET == w::FF_HEADSET);
        assert!(ff::HANDSET == w::FF_HANDSET);
    };

    fn is_hex(s: &str, n: usize) -> bool {
        s.len() == n
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// `key8 = hex(SHA-256(fp || device_token || form_factor)[..4])`
    /// (design §7.4, T6 decision): `fp` and `device_token` are fixed-width
    /// lowercase hex (16 + 16) and the EndpointFormFactor code follows as
    /// decimal ASCII ("3" for headphones), so plain concatenation is
    /// unambiguous.
    ///
    /// The form factor is part of the key because Windows ties an endpoint's
    /// form factor to its id: a form-factor change under the same reference
    /// string makes the system delete the endpoint and create one with a
    /// random id, losing its settings. With the code in the key such a change
    /// is always a new reference string, and changing back returns the
    /// earlier id with its settings.
    pub(crate) fn presentation_key(fingerprint: &str, device_token: &str, form_factor: u32) -> String {
        let mut h = Sha256::new();
        h.update(fingerprint.as_bytes());
        h.update(device_token.as_bytes());
        h.update(form_factor.to_string().as_bytes());
        h.finalize()[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The device-interface reference string: `"<template>-<fp>"` when
    /// virtual (identical to 1.0.1, so a virtual peer keeps its endpoint id
    /// across the upgrade), `"<template>-<fp>-p<key8>"` when physical.
    /// `None` when an input is malformed or the result would not fit
    /// `REFSTRING_MAX`; the caller then presents Virtual.
    pub(crate) fn reference_string(
        template: &str,
        fingerprint: &str,
        key8: Option<&str>,
    ) -> Option<String> {
        if !TEMPLATES.contains(&template)
            || !crate::halbridge_win::wire::valid_peer_key(fingerprint)
        {
            return None;
        }
        let s = match key8 {
            None => format!("{template}-{fingerprint}"),
            Some(k) if is_hex(k, PRESKEY_HEX) => format!("{template}-{fingerprint}-p{k}"),
            Some(_) => return None,
        };
        (s.encode_utf16().count() < REFSTRING_MAX).then_some(s)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct WinPresentationTarget {
        pub klass: u32,
        /// EndpointFormFactor the endpoint will report.
        pub form_factor: u32,
        pub pin_category: PinCategory,
        /// Phase 2: equal non-zero on both directions = one child devnode.
        pub group: u32,
        /// `key8` when physical, empty when virtual.
        pub key: String,
        /// EP\0 DeviceDesc, ≤127 UTF-16 units. v11 virtual: the localized
        /// direction noun ([`super::windows_virtual_endpoint_desc`]).
        pub endpoint_desc: String,
        /// v11 child devnode FriendlyName, ≤127 UTF-16 units. Virtual:
        /// `"AudioHub – <peer>"` ([`super::windows_virtual_adapter_name`]).
        pub adapter_name: String,
        /// Phase 2, informational; ≤63 UTF-16 units; empty when virtual.
        pub manufacturer: String,
    }

    fn shape(direction: Direction, f: FormFactor) -> (u32, PinCategory) {
        match direction {
            Direction::Output => match f {
                FormFactor::Headphones => (ff::HEADPHONES, PinCategory::Headphones),
                FormFactor::Headset => (ff::HEADSET, PinCategory::HeadsetSpeakers),
                FormFactor::Handset => (ff::HANDSET, PinCategory::Handset),
                // T6 decision: a digital display renders as speakers.
                // KSNODETYPE_HDMI_INTERFACE leaves its name behind after the
                // endpoint is removed (spike S2), and the driver refuses it.
                FormFactor::DigitalDisplay
                | FormFactor::Speakers
                | FormFactor::LineLevel
                | FormFactor::Spdif
                | FormFactor::Microphone
                | FormFactor::Unknown => (ff::SPEAKERS, PinCategory::Speaker),
            },
            Direction::Input => match f {
                FormFactor::Headset => (ff::HEADSET, PinCategory::HeadsetMicrophone),
                FormFactor::Handset => (ff::HANDSET, PinCategory::Handset),
                _ => (ff::MICROPHONE, PinCategory::Microphone),
            },
        }
    }

    impl WinPresentationTarget {
        /// The `AH_PRESENTATION` the Windows bind carries for this target.
        pub(crate) fn to_wire(&self) -> crate::halbridge_win::wire::WirePresentation {
            crate::halbridge_win::wire::WirePresentation {
                klass: self.klass,
                form_factor: if self.klass == PRES_PHYSICAL { self.form_factor } else { 0 },
                group: self.group,
                key: self.key.clone(),
                endpoint_desc: self.endpoint_desc.clone(),
                adapter_name: self.adapter_name.clone(),
                manufacturer: self.manufacturer.clone(),
            }
        }
    }

    /// Inputs of the Windows names. There is deliberately no offline flag:
    /// Windows freezes an endpoint's name when the endpoint id is first
    /// created (the EP\0 half) and reads the devnode half only when the child
    /// starts, so a mark written while the peer is offline would stay after it
    /// connects, and one never written could not appear later (T6 finding 1).
    /// The offline state is shown by the AudioHub UI instead.
    pub(crate) struct NameCtx<'a> {
        pub peer_label: &'a str,
        pub locale: &'a str,
    }

    pub(crate) fn target(
        direction: Direction,
        fingerprint: &str,
        p: &Presentation,
        group: u32,
        names: &NameCtx,
    ) -> WinPresentationTarget {
        let virtual_ = || WinPresentationTarget {
            klass: PRES_VIRTUAL,
            form_factor: match direction {
                Direction::Output => ff::SPEAKERS,
                Direction::Input => ff::MICROPHONE,
            },
            pin_category: match direction {
                Direction::Output => PinCategory::Speaker,
                Direction::Input => PinCategory::MicrophoneArray,
            },
            group: 0,
            key: String::new(),
            // v11: the virtual unit is a child devnode too; its name and the
            // endpoint half are composed here like a physical one's.
            endpoint_desc: windows_virtual_endpoint_desc(direction, false, names.locale),
            adapter_name: windows_virtual_adapter_name(names.peer_label),
            manufacturer: String::new(),
        };
        let Presentation::Physical(d) = p else {
            return virtual_();
        };
        let (form_factor, pin_category) = shape(direction, d.form_factor);
        let key = presentation_key(fingerprint, &d.device_token, form_factor);
        // Fail closed: an identity that cannot be built is not presented.
        if TEMPLATES
            .iter()
            .any(|t| reference_string(t, fingerprint, Some(&key)).is_none())
        {
            return virtual_();
        }
        let vendor =
            trustworthy_manufacturer(d).unwrap_or_else(|| UNKNOWN_MANUFACTURER.to_string());
        // A grouped (RC) unit, or a provider that sent the adapter half, keeps
        // the §5.2 split. A standalone unit without an adapter half carries the
        // device name in the devnode half so that two such units of one peer
        // never share a devnode name (Windows would prefix "2- ").
        let (endpoint_desc, adapter_name) = if group == 0 && !has_adapter_half(d) {
            (
                windows_standalone_endpoint_desc(direction, form_factor, false, names.locale),
                windows_standalone_adapter_name(d, names.peer_label),
            )
        } else {
            (
                windows_endpoint_desc(d, false, names.locale),
                windows_adapter_name(d, names.peer_label),
            )
        };
        WinPresentationTarget {
            klass: PRES_PHYSICAL,
            form_factor,
            pin_category,
            group,
            key,
            endpoint_desc,
            adapter_name,
            manufacturer: Measure::Utf16Units
                .clamp(&vendor, MFG_CHARS - 1)
                .trim_end()
                .to_string(),
        }
    }
}

// ------------------------------------------------------- mismatch gate

/// Per-slot "silence this peer's virtual microphone" flags, read by the
/// mixer once per slot per 10 ms tick. One relaxed load: the flag is a
/// standalone fact, nothing else is published through it.
#[derive(Debug, Default)]
pub(crate) struct MicPresentationGate {
    mask: AtomicU16,
}

const _: () = assert!(HAL_MAX_SLOTS <= 16, "the gate mask has one bit per slot");

impl MicPresentationGate {
    pub(crate) fn new() -> MicPresentationGate {
        MicPresentationGate::default()
    }

    #[inline]
    pub(crate) fn is_gated(&self, slot: usize) -> bool {
        slot < 16 && self.mask.load(Ordering::Relaxed) & (1u16 << slot) != 0
    }

    pub(crate) fn mask(&self) -> u16 {
        self.mask.load(Ordering::Relaxed)
    }

    fn store(&self, mask: u16) {
        self.mask.store(mask, Ordering::Relaxed);
    }
}

/// The mixer's half of the gate: the block still goes into the ring (its
/// cadence and water level are unchanged), only its content is silence.
#[inline]
pub(crate) fn silence_if_gated(gated: bool, out: &mut [f32]) {
    if gated {
        out.fill(0.0);
    }
}

/// What the driver currently presents for one direction, as last reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Applied {
    /// Not (yet) reported since the daemon or the driver attachment started.
    /// The driver keeps bindings across a daemon restart, so it may still be
    /// presenting a previous physical look: treated as possibly physical.
    #[default]
    Unknown,
    Known(Presentation),
}

/// A driver that cannot (or may not) present physical presents Virtual,
/// whatever it last reported.
pub(crate) fn effective_applied(driver: DriverPresentation, applied: &Applied) -> Applied {
    if driver == DriverPresentation::Supported {
        applied.clone()
    } else {
        Applied::Known(Presentation::Virtual)
    }
}

/// Design §5.4: the microphone is silenced while what the system sees may be
/// physical and does not match the facts — physical→virtual, physical A→B,
/// an unknown applied state, or (`provisional`) a persisted physical look on
/// a live connection whose facts have not arrived yet (§4.5).
pub(crate) fn mic_gate_engaged(
    applied: &Applied,
    target: &Presentation,
    provisional: bool,
) -> bool {
    match applied {
        Applied::Unknown => true,
        Applied::Known(Presentation::Virtual) => false,
        Applied::Known(a @ Presentation::Physical(_)) => provisional || !a.same_device(target),
    }
}

// ---------------------------------------------------- rebuild scheduler

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeClass {
    /// Virtual → physical.
    Upgrade,
    /// (Possibly) physical → virtual.
    Withdrawal,
    /// (Possibly) physical A → physical B.
    Change,
}

pub(crate) fn classify_change(applied: &Applied, target: &Presentation) -> Option<ChangeClass> {
    match applied {
        Applied::Known(a) if a.same_device(target) => None,
        Applied::Known(Presentation::Virtual) => Some(ChangeClass::Upgrade),
        _ if target.is_physical() => Some(ChangeClass::Change),
        _ => Some(ChangeClass::Withdrawal),
    }
}

/// Design §7.5 timing. "Downgrade" = Withdrawal or Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RebuildPolicy {
    pub upgrade_stability: Duration,
    pub upgrade_waits_idle: bool,
    /// Indexed `[OUT, IN]`.
    pub downgrade_stability: [Duration; 2],
    pub downgrade_waits_idle: [bool; 2],
}

/// Windows: every change is a rebuild (a new endpoint), so it waits until the
/// target has been stable for 2 s (absorbing the provider's transient
/// Unknown). Microphone withdrawals/changes then run at once — the gate is
/// already silencing them and the gate requires it; everything else waits
/// for the endpoint to be idle (virtual is the safe state, no upper bound).
pub(crate) const WINDOWS_POLICY: RebuildPolicy = RebuildPolicy {
    upgrade_stability: Duration::from_secs(2),
    upgrade_waits_idle: true,
    downgrade_stability: [Duration::from_secs(2), Duration::from_secs(2)],
    downgrade_waits_idle: [true, false],
};

/// macOS: a change is a live property update that does not interrupt IO, so
/// withdrawals and changes apply at once (§5.4). Upgrades still wait for 2 s
/// of stability so the provider's transient Unknown does not flap the
/// transport type back and forth.
pub(crate) const MACOS_POLICY: RebuildPolicy = RebuildPolicy {
    upgrade_stability: Duration::from_secs(2),
    upgrade_waits_idle: false,
    downgrade_stability: [Duration::ZERO, Duration::ZERO],
    downgrade_waits_idle: [false, false],
};

pub(crate) fn platform_policy() -> RebuildPolicy {
    if cfg!(windows) {
        WINDOWS_POLICY
    } else {
        MACOS_POLICY
    }
}

/// One direction's scheduler. It never executes anything: it holds at most
/// one outstanding request (`requested`) that the platform layer carries out
/// and acknowledges through [`PresentationState::mark_applied`].
#[derive(Debug, Clone, Default)]
pub(crate) struct RebuildTracker {
    /// Identity of the target being timed, and since when.
    candidate: Option<(Option<String>, Instant)>,
    requested: Option<Presentation>,
}

impl RebuildTracker {
    pub(crate) fn requested(&self) -> Option<&Presentation> {
        self.requested.as_ref()
    }

    /// One pass. `Some` = a request was issued now.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        policy: &RebuildPolicy,
        direction: Direction,
        target: &Presentation,
        applied: &Applied,
        idle: bool,
    ) -> Option<ChangeClass> {
        // A request that no longer describes the target is withdrawn; the
        // new target has to earn its own stability.
        if self
            .requested
            .as_ref()
            .is_some_and(|r| !r.same_device(target))
        {
            self.requested = None;
        }
        let Some(class) = classify_change(applied, target) else {
            self.candidate = None;
            self.requested = None;
            return None;
        };
        let identity = target.identity().map(str::to_string);
        let since = match &self.candidate {
            Some((id, since)) if *id == identity => *since,
            _ => {
                self.candidate = Some((identity, now));
                now
            }
        };
        if let Some(r) = self.requested.as_mut() {
            // Same device, possibly refreshed descriptive fields.
            *r = target.clone();
            return None;
        }
        let d = dir_index(direction);
        let (stability, waits_idle) = match class {
            ChangeClass::Upgrade => (policy.upgrade_stability, policy.upgrade_waits_idle),
            ChangeClass::Withdrawal | ChangeClass::Change => (
                policy.downgrade_stability[d],
                policy.downgrade_waits_idle[d],
            ),
        };
        if now.saturating_duration_since(since) >= stability && (!waits_idle || idle) {
            self.requested = Some(target.clone());
            return Some(class);
        }
        None
    }

    fn applied(&mut self, p: &Presentation) {
        if self.requested.as_ref().is_some_and(|r| r.same_device(p)) {
            self.requested = None;
        }
    }
}

// ------------------------------------------------------ persisted facts

/// The last accepted facts of one peer, persisted in `hal_slots.json` v4
/// (design §4.5) so a restart or reconnect does not flap a device through
/// virtual. Revision stripped: it is connection-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CachedFacts {
    pub input: DirectionFacts,
    pub output: DirectionFacts,
}

impl CachedFacts {
    pub(crate) fn new(input: DirectionFacts, output: DirectionFacts) -> CachedFacts {
        CachedFacts { input, output }
    }

    pub(crate) fn get(&self, direction: Direction) -> &DirectionFacts {
        match direction {
            Direction::Input => &self.input,
            Direction::Output => &self.output,
        }
    }

    /// Each invalid direction becomes `Virtual("invalid_facts")`.
    pub(crate) fn sanitized(self) -> CachedFacts {
        CachedFacts {
            input: self.input.sanitized(),
            output: self.output.sanitized(),
        }
    }
}

pub(crate) enum CacheUpdate {
    Set(CachedFacts),
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FactsSource {
    /// The live connection's facts (or its explicit "unsupported").
    Live,
    /// Persisted facts.
    Cache,
    /// Connected, facts not here yet, nothing persisted.
    Pending,
    /// Offline and nothing persisted.
    #[default]
    None,
}

impl FactsSource {
    fn label(self) -> &'static str {
        match self {
            FactsSource::Live => "live",
            FactsSource::Cache => "cache",
            FactsSource::Pending => "pending",
            FactsSource::None => "none",
        }
    }
}

struct ResolvedFacts {
    facts: [Option<DirectionFacts>; 2],
    source: FactsSource,
    provisional: bool,
    version: Option<u8>,
    cache: Option<CacheUpdate>,
}

/// Design §4.5: which record each direction is decided on.
fn resolve_facts(view: Option<&PeerFactsView>, cached: Option<&CachedFacts>) -> ResolvedFacts {
    let from_cache = |c: &CachedFacts| DIRECTIONS.map(|d| Some(c.get(d).clone()));
    let Some(v) = view else {
        return ResolvedFacts {
            facts: cached.map(from_cache).unwrap_or([None, None]),
            source: if cached.is_some() {
                FactsSource::Cache
            } else {
                FactsSource::None
            },
            provisional: false,
            version: None,
            cache: None,
        };
    };
    match (v.facts_version, &v.observation) {
        (Some(0), _) => ResolvedFacts {
            facts: DIRECTIONS.map(|d| Some(v.effective(d))),
            source: FactsSource::Live,
            provisional: false,
            version: Some(0),
            // An old peer: whatever was persisted for it no longer applies.
            cache: cached.map(|_| CacheUpdate::Clear),
        },
        (version, Some(o)) => {
            let fresh = CachedFacts::new(o.input.clone(), o.output.clone());
            ResolvedFacts {
                facts: DIRECTIONS.map(|d| Some(fresh.get(d).clone())),
                source: FactsSource::Live,
                provisional: false,
                version,
                cache: (cached != Some(&fresh)).then_some(CacheUpdate::Set(fresh)),
            }
        }
        (version, None) => match cached {
            // Keep the persisted look, but provisionally: the microphone is
            // silent until the live facts confirm it.
            Some(c) => ResolvedFacts {
                facts: from_cache(c),
                source: FactsSource::Cache,
                provisional: true,
                version,
                cache: None,
            },
            None => ResolvedFacts {
                facts: DIRECTIONS.map(|d| Some(v.effective(d))),
                source: FactsSource::Pending,
                provisional: false,
                version,
                cache: None,
            },
        },
    }
}

// ------------------------------------------------------- daemon state

/// One peer's input to an evaluation pass.
pub(crate) struct PeerInputs {
    pub fingerprint: String,
    pub slot: Option<u8>,
    /// Disambiguated peer label (no prefix, no offline mark).
    pub label: String,
    pub marked_offline: bool,
    /// `device_facts_pub::peer_facts`; `None` = not connected.
    pub view: Option<PeerFactsView>,
    pub cached: Option<CachedFacts>,
    /// `[OUT, IN]`: no application has that endpoint open.
    pub idle: [bool; 2],
}

pub(crate) struct EvalInputs {
    pub now: Instant,
    pub consumer_enabled: bool,
    pub accept_injected: bool,
    pub locale: String,
    pub peers: Vec<PeerInputs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RebuildRequest {
    pub fingerprint: String,
    pub slot: Option<u8>,
    pub direction: Direction,
    pub class: ChangeClass,
    pub to: Presentation,
}

#[derive(Default)]
pub(crate) struct EvalOutcome {
    pub requests: Vec<RebuildRequest>,
    pub cache: Vec<(String, CacheUpdate)>,
    pub gate_mask: u16,
}

#[derive(Debug, Clone, Default)]
struct DirState {
    decision: Decision,
    applied: Applied,
    tracker: RebuildTracker,
    mic_gated: bool,
}

#[derive(Debug, Clone, Default)]
struct PeerState {
    slot: Option<u8>,
    label: String,
    marked_offline: bool,
    source: FactsSource,
    provisional: bool,
    dirs: [DirState; 2],
}

/// Consumer-side presentation state, owned by `haldev::HalDevState`.
#[derive(Debug)]
pub(crate) struct PresentationState {
    driver: DriverPresentation,
    policy: RebuildPolicy,
    consumer_enabled: bool,
    locale: String,
    peers: HashMap<String, PeerState>,
    gate: Arc<MicPresentationGate>,
    passes: u64,
}

impl Default for PresentationState {
    fn default() -> Self {
        PresentationState::new()
    }
}

impl PresentationState {
    pub(crate) fn new() -> PresentationState {
        PresentationState {
            driver: DriverPresentation::Unsupported,
            policy: platform_policy(),
            consumer_enabled: true,
            locale: String::new(),
            peers: HashMap::new(),
            gate: Arc::new(MicPresentationGate::new()),
            passes: 0,
        }
    }

    /// The mixer lifts this once and reads it lock-free.
    pub(crate) fn gate(&self) -> Arc<MicPresentationGate> {
        Arc::clone(&self.gate)
    }

    pub(crate) fn driver(&self) -> DriverPresentation {
        self.driver
    }

    /// Platform layers (T5/T6) report what the attached driver allows.
    pub(crate) fn set_driver(&mut self, driver: DriverPresentation) {
        if self.driver != driver {
            crate::dlog!(
                "[audiohubd] presentation: driver capability {} -> {}",
                self.driver.label(),
                driver.label()
            );
            self.driver = driver;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_policy(&mut self, policy: RebuildPolicy) {
        self.policy = policy;
    }

    /// The driver acknowledged it now presents `p` for `fp`'s `direction`.
    pub(crate) fn mark_applied(&mut self, fp: &str, direction: Direction, p: Presentation) {
        let ds = &mut self.peers.entry(fp.to_string()).or_default().dirs[dir_index(direction)];
        ds.tracker.applied(&p);
        ds.applied = Applied::Known(p);
    }

    /// Windows (design §4.5): adopt the decided PHYSICAL look as the starting
    /// account of every direction whose driver-side look is unknown — daemon
    /// start, a driver re-attach, a new pairing — so the next Set publishes
    /// that look directly instead of binding Virtual and upgrading after the
    /// stability/idle wait. Without this every daemon start rebuilt each
    /// physical endpoint twice (physical -> virtual -> physical): the driver
    /// keeps its bindings across a daemon restart, and an Unknown account
    /// is presented as Virtual. The wait exists to spare an endpoint in use;
    /// a look the daemon does not know yet is not one it is about to churn.
    /// The Set's reply is still authoritative: a refused or forced-virtual
    /// answer replaces this account through `mark_applied`. Returns the
    /// directions adopted, `[OUT, IN]`.
    #[cfg(any(windows, test))]
    pub(crate) fn adopt_physical_when_unknown(&mut self, fp: &str) -> [bool; 2] {
        let mut adopted = [false, false];
        if self.driver != DriverPresentation::Supported {
            return adopted;
        }
        let Some(p) = self.peers.get_mut(fp) else {
            return adopted;
        };
        for (d, ds) in p.dirs.iter_mut().enumerate() {
            if matches!(ds.applied, Applied::Unknown)
                && ds.tracker.requested().is_none()
                && ds.decision.presentation.is_physical()
            {
                ds.applied = Applied::Known(ds.decision.presentation.clone());
                adopted[d] = true;
            }
        }
        if adopted.iter().any(|a| *a) {
            crate::dlog!(
                "[audiohubd] presentation: {fp} adopts the decided physical look for an unknown driver account (out={} in={})",
                adopted[OUT],
                adopted[IN]
            );
        }
        adopted
    }

    /// A driver (re)attachment: nothing it presents is known any more.
    pub(crate) fn mark_all_unknown(&mut self) {
        for ds in self.peers.values_mut().flat_map(|p| p.dirs.iter_mut()) {
            ds.applied = Applied::Unknown;
        }
    }

    /// What the platform layer should present for `fp` right now,
    /// `[OUT, IN]`: the outstanding request, else the applied look refreshed
    /// with the target's descriptive fields when it is the same device, else
    /// the applied look (Virtual when unknown).
    pub(crate) fn current(&self, fp: &str) -> [Presentation; 2] {
        let Some(p) = self.peers.get(fp) else {
            return [Presentation::Virtual, Presentation::Virtual];
        };
        let driver = self.driver;
        p.dirs.clone().map(|ds| current_of(driver, &ds))
    }

    /// Whether `fp`'s microphone is currently silenced by the gate.
    pub(crate) fn mic_gated(&self, fp: &str) -> bool {
        self.peers.get(fp).is_some_and(|p| p.dirs[IN].mic_gated)
    }

    pub(crate) fn target(&self, fp: &str, direction: Direction) -> Option<&Decision> {
        self.peers
            .get(fp)
            .map(|p| &p.dirs[dir_index(direction)].decision)
    }

    /// One evaluation pass over every peer the caller considers. Peers not
    /// listed are forgotten.
    pub(crate) fn evaluate(&mut self, inputs: EvalInputs) -> EvalOutcome {
        self.passes += 1;
        self.consumer_enabled = inputs.consumer_enabled;
        self.locale = inputs.locale;
        let (driver, policy, now) = (self.driver, self.policy, inputs.now);
        let mut out = EvalOutcome::default();
        let mut seen = HashSet::new();
        for p in inputs.peers {
            let r = resolve_facts(p.view.as_ref(), p.cached.as_ref());
            if let Some(update) = r.cache {
                out.cache.push((p.fingerprint.clone(), update));
            }
            seen.insert(p.fingerprint.clone());
            let st = self.peers.entry(p.fingerprint.clone()).or_default();
            st.slot = p.slot;
            st.label = p.label;
            st.marked_offline = p.marked_offline;
            st.source = r.source;
            st.provisional = r.provisional;
            for (d, direction) in DIRECTIONS.into_iter().enumerate() {
                let decision = decide(&PresentationInputs {
                    consumer_enabled: inputs.consumer_enabled,
                    driver,
                    peer_facts_version: r.version,
                    facts: r.facts[d].as_ref(),
                    accept_injected: inputs.accept_injected,
                });
                let ds = &mut st.dirs[d];
                let applied = effective_applied(driver, &ds.applied);
                let gated =
                    d == IN && mic_gate_engaged(&applied, &decision.presentation, r.provisional);
                if gated != ds.mic_gated {
                    crate::dlog!(
                        "[audiohubd] presentation: {} microphone gate {} (target {}, {})",
                        p.fingerprint,
                        if gated { "ENGAGED" } else { "released" },
                        if decision.presentation.is_physical() {
                            "physical"
                        } else {
                            "virtual"
                        },
                        decision.reason
                    );
                }
                if let Some(class) = ds.tracker.tick(
                    now,
                    &policy,
                    direction,
                    &decision.presentation,
                    &applied,
                    p.idle[d],
                ) {
                    out.requests.push(RebuildRequest {
                        fingerprint: p.fingerprint.clone(),
                        slot: p.slot,
                        direction,
                        class,
                        to: decision.presentation.clone(),
                    });
                }
                if gated {
                    if let Some(slot) = p.slot.filter(|&s| usize::from(s) < HAL_MAX_SLOTS) {
                        out.gate_mask |= 1u16 << slot;
                    }
                }
                ds.decision = decision;
                ds.mic_gated = gated;
            }
        }
        self.peers.retain(|fp, _| seen.contains(fp));
        self.gate.store(out.gate_mask);
        for r in &out.requests {
            crate::dlog!(
                "[audiohubd] presentation: {} {:?} {:?} -> {}",
                r.fingerprint,
                r.direction,
                r.class,
                if r.to.is_physical() {
                    "physical"
                } else {
                    "virtual"
                }
            );
        }
        out
    }

    /// `daemon.status.presentation`: read-only view for regression scripts.
    pub(crate) fn status_json(&self) -> Value {
        let mut peers = serde_json::Map::new();
        for (fp, p) in &self.peers {
            let current = p.dirs.clone().map(|ds| current_of(self.driver, &ds));
            let group = related_group(&current[OUT], &current[IN]);
            let names = win::NameCtx {
                peer_label: &p.label,
                locale: &self.locale,
            };
            let mut dirs = serde_json::Map::new();
            for (d, direction) in DIRECTIONS.into_iter().enumerate() {
                let ds = &p.dirs[d];
                let applied = effective_applied(self.driver, &ds.applied);
                let mac = mac::target(direction, &current[d], group);
                let winv = win::target(direction, fp, &current[d], group, &names);
                dirs.insert(
                    match direction {
                        Direction::Output => "output".into(),
                        Direction::Input => "input".into(),
                    },
                    json!({
                        "target": ds.decision.presentation.to_json(),
                        "reason": ds.decision.reason,
                        "applied": match &applied {
                            Applied::Unknown => json!("unknown"),
                            Applied::Known(a) => a.to_json(),
                        },
                        "requested": ds.tracker.requested().map(Presentation::to_json),
                        "current": current[d].to_json(),
                        "pending_republish": classify_change(&applied, &ds.decision.presentation).is_some(),
                        "mic_gate": ds.mic_gated,
                        "name": current[d].descriptor().map(|desc| mac_device_name(desc, &p.label, p.marked_offline, &self.locale)),
                        "mac": {
                            "klass": mac.klass,
                            "transport": fourcc_text(mac.transport),
                            "terminal_type": fourcc_text(mac.terminal_type),
                            "icon_kind": mac.icon_kind,
                            "related_group": mac.related_group,
                            "manufacturer": mac.manufacturer,
                            "model_uid": mac.model_uid(direction),
                        },
                        "windows": {
                            "klass": winv.klass,
                            "form_factor": winv.form_factor,
                            "pin_category": winv.pin_category.ksnodetype(),
                            "group": winv.group,
                            "key": winv.key,
                            "endpoint_desc": winv.endpoint_desc,
                            "adapter_name": winv.adapter_name,
                            "manufacturer": winv.manufacturer,
                        },
                    }),
                );
            }
            let mut entry = json!({
                "slot": p.slot,
                "facts_source": p.source.label(),
                "provisional": p.provisional,
            });
            if let Some(obj) = entry.as_object_mut() {
                obj.extend(dirs);
            }
            peers.insert(fp.clone(), entry);
        }
        json!({
            "driver": self.driver.label(),
            "consumer_enabled": self.consumer_enabled,
            "policy": if self.policy == WINDOWS_POLICY { "windows" } else { "macos" },
            "passes": self.passes,
            "mic_gate_mask": self.gate.mask(),
            "peers": peers,
        })
    }
}

fn current_of(driver: DriverPresentation, ds: &DirState) -> Presentation {
    if let Some(r) = ds.tracker.requested() {
        return r.clone();
    }
    match effective_applied(driver, &ds.applied) {
        Applied::Known(a) if a.same_device(&ds.decision.presentation) => {
            ds.decision.presentation.clone()
        }
        Applied::Known(a) => a,
        Applied::Unknown => Presentation::Virtual,
    }
}

fn fourcc_text(v: u32) -> String {
    let b = v.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        format!("{v:#010x}")
    }
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_core::device_facts::{DeviceFactsObservation, PhysicalTransport as T};

    const FP: &str = "0123456789abcdef";

    fn desc(token: &str) -> PhysicalDescriptor {
        PhysicalDescriptor {
            transport: T::Usb,
            form_factor: FormFactor::Headset,
            name: "Jabra Evolve2 65".into(),
            endpoint_desc: Some("Headset Earphone".into()),
            adapter_name: Some("Jabra Evolve2 65".into()),
            manufacturer: Some("Jabra".into()),
            device_token: token.into(),
            group: 3,
        }
    }

    fn physical(token: &str) -> DirectionFacts {
        DirectionFacts::physical(desc(token))
    }

    fn injected(token: &str) -> DirectionFacts {
        let mut f = physical(token);
        f.origin = FactsOrigin::Injected;
        f
    }

    fn invalid_physical() -> DirectionFacts {
        let mut f = physical("aaaaaaaaaaaaaaaa");
        if let Some(d) = f.physical.as_mut() {
            d.device_token = "NOT-HEX".into();
        }
        f
    }

    fn inputs<'a>(
        enabled: bool,
        driver: DriverPresentation,
        version: Option<u8>,
        facts: Option<&'a DirectionFacts>,
        accept: bool,
    ) -> PresentationInputs<'a> {
        PresentationInputs {
            consumer_enabled: enabled,
            driver,
            peer_facts_version: version,
            facts,
            accept_injected: accept,
        }
    }

    // ------------------------------------------------------ truth table

    #[test]
    fn presentation_for_truth_table_is_physical_only_when_every_condition_holds() {
        let records: Vec<(&str, Option<DirectionFacts>)> = vec![
            ("none", None),
            (
                "virtual",
                Some(DirectionFacts::virtual_("adapter_enumerator_virtual")),
            ),
            ("unknown", Some(DirectionFacts::unknown("probe_pending"))),
            ("absent", Some(DirectionFacts::absent())),
            ("physical", Some(physical("1111111111111111"))),
            ("invalid", Some(invalid_physical())),
            ("injected", Some(injected("2222222222222222"))),
        ];
        let drivers = [
            DriverPresentation::Unsupported,
            DriverPresentation::CallerCheckInsufficient,
            DriverPresentation::Supported,
        ];
        let mut physical_rows = 0;
        let mut rows = 0;
        for enabled in [false, true] {
            for driver in drivers {
                for version in [None, Some(0u8), Some(1), Some(2)] {
                    for (label, rec) in &records {
                        for accept in [false, true] {
                            rows += 1;
                            let i = inputs(enabled, driver, version, rec.as_ref(), accept);
                            let got = presentation_for(&i);
                            let expect = enabled
                                && driver == DriverPresentation::Supported
                                && version != Some(0)
                                && (*label == "physical" || (*label == "injected" && accept));
                            assert_eq!(
                                got.is_physical(),
                                expect,
                                "enabled={enabled} driver={driver:?} version={version:?} \
                                 facts={label} accept_injected={accept}"
                            );
                            if expect {
                                physical_rows += 1;
                                assert_eq!(decide(&i).reason, why::PHYSICAL);
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(rows, 2 * 3 * 4 * 7 * 2);
        // enabled × supported × {None,1,2} × (physical×2 + injected×1)
        assert_eq!(physical_rows, 3 * 3);
    }

    #[test]
    fn virtual_decisions_carry_the_reason_the_ui_explains() {
        let p = physical("1111111111111111");
        let s = DriverPresentation::Supported;
        let reason = |i: PresentationInputs| decide(&i).reason;
        assert_eq!(
            reason(inputs(false, s, Some(1), Some(&p), false)),
            why::CONSUMER_DISABLED
        );
        assert_eq!(
            reason(inputs(
                true,
                DriverPresentation::Unsupported,
                Some(1),
                Some(&p),
                false
            )),
            why::DRIVER_UNSUPPORTED
        );
        assert_eq!(
            reason(inputs(
                true,
                DriverPresentation::CallerCheckInsufficient,
                Some(1),
                Some(&p),
                false
            )),
            why::DRIVER_CALLER_CHECK
        );
        assert_eq!(
            reason(inputs(true, s, Some(0), Some(&p), false)),
            local_reason::PEER_UNSUPPORTED
        );
        assert_eq!(reason(inputs(true, s, Some(1), None, false)), why::NO_FACTS);
        let inv = invalid_physical();
        assert_eq!(
            reason(inputs(true, s, Some(1), Some(&inv), false)),
            reason::INVALID_FACTS
        );
        let inj = injected("2222222222222222");
        assert_eq!(
            reason(inputs(true, s, Some(1), Some(&inj), false)),
            local_reason::INJECTED_REFUSED
        );
        let v = DirectionFacts::virtual_("owner_third_party");
        assert_eq!(
            reason(inputs(true, s, Some(1), Some(&v), false)),
            "owner_third_party"
        );
        // A physical class without a descriptor is structurally invalid.
        let mut broken = physical("1111111111111111");
        broken.physical = None;
        assert!(!presentation_for(&inputs(true, s, Some(1), Some(&broken), true)).is_physical());
    }

    // ----------------------------------------------------------- naming

    #[test]
    fn mac_names_mirror_the_device_and_disambiguate_with_the_peer_label() {
        let mut d = desc("1111111111111111");
        d.name = "MacBook Pro Speakers".into();
        assert_eq!(
            mac_device_name(&d, "客厅 Mac", false, "zh-CN"),
            "MacBook Pro Speakers (客厅 Mac)"
        );
        assert_eq!(
            mac_device_name(&d, "客厅 Mac", true, "zh-CN"),
            "MacBook Pro Speakers (客厅 Mac)（离线）"
        );
        assert_eq!(
            mac_device_name(&d, "Office", true, "en-US"),
            "MacBook Pro Speakers (Office) (Offline)"
        );
        // Control characters and surrounding blanks never reach a device name.
        d.name = "  Speakers\u{7}\n ".into();
        assert_eq!(
            mac_device_name(&d, "\tWIN-30 ", false, "en-US"),
            "Speakers (WIN-30)"
        );
        // No label: just the device name; no name: just the label.
        assert_eq!(mac_device_name(&d, "  ", false, "en-US"), "Speakers");
        d.name = " ".into();
        assert_eq!(mac_device_name(&d, "WIN-30", false, "en-US"), "WIN-30");
    }

    #[test]
    fn long_names_fit_the_driver_buffers_and_keep_the_mark_and_label() {
        let mut d = desc("1111111111111111");
        for name in [
            "x".repeat(200),
            "扬".repeat(120),
            "🎧".repeat(60),
            format!("{}é", "a".repeat(125)),
        ] {
            d.name = name.clone();
            d.endpoint_desc = Some(name.clone());
            d.adapter_name = Some(name.clone());
            for label in [
                "WIN-30",
                "客厅的 Mac mini（2）",
                &"L".repeat(150),
                &"🖥".repeat(80),
            ] {
                for offline in [false, true] {
                    for locale in ["zh-CN", "en-US"] {
                        let m = mac_device_name(&d, label, offline, locale);
                        assert!(m.len() <= MAC_NAME_MAX_BYTES, "{} bytes: {m}", m.len());
                        assert!(!m.is_empty());
                        if offline {
                            assert!(m.ends_with(crate::haldev::offline_mark(locale)), "{m}");
                        }
                        if label.len() < 40 {
                            assert!(m.contains(&format!("({label})")), "label lost: {m}");
                        }
                        let w = windows_endpoint_desc(&d, offline, locale);
                        assert!(w.encode_utf16().count() <= WIN_NAME_MAX_UNITS, "{w}");
                        let a = windows_adapter_name(&d, label);
                        assert!(a.encode_utf16().count() <= WIN_NAME_MAX_UNITS, "{a}");
                        // Truncation never leaves half a character.
                        for s in [&m, &w, &a] {
                            assert!(std::str::from_utf8(s.as_bytes()).is_ok());
                            assert!(
                                String::from_utf16(&s.encode_utf16().collect::<Vec<_>>()).is_ok()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn windows_names_use_the_endpoint_and_adapter_halves() {
        let mut d = desc("1111111111111111");
        d.endpoint_desc = Some("Speakers".into());
        d.adapter_name = Some("Realtek(R) Audio".into());
        assert_eq!(windows_endpoint_desc(&d, false, "en-US"), "Speakers");
        assert_eq!(
            windows_adapter_name(&d, "WIN-30"),
            "Realtek(R) Audio \u{2013} WIN-30"
        );
        // A macOS provider sends no halves.
        d.endpoint_desc = None;
        d.adapter_name = None;
        d.name = "MacBook Pro Speakers".into();
        assert_eq!(
            windows_endpoint_desc(&d, true, "zh-CN"),
            "MacBook Pro Speakers（离线）"
        );
        assert_eq!(windows_adapter_name(&d, "Mac"), "Mac");
    }

    #[test]
    fn windows_virtual_names_put_the_peer_in_the_devnode_half() {
        assert_eq!(
            windows_virtual_endpoint_desc(Direction::Output, false, "en-US"),
            "Speakers"
        );
        assert_eq!(
            windows_virtual_endpoint_desc(Direction::Input, true, "zh-CN"),
            "麦克风（离线）"
        );
        assert_eq!(windows_virtual_adapter_name("WIN-30"), "AudioHub \u{2013} WIN-30");
        assert_eq!(
            windows_virtual_adapter_name("WIN-30"),
            crate::haldev::device_name_stem("WIN-30"),
            "the virtual devnode carries the cross-platform device name"
        );
        let long = windows_virtual_adapter_name(&"🖥".repeat(100));
        assert!(long.encode_utf16().count() <= WIN_NAME_MAX_UNITS);
        assert!(String::from_utf16(&long.encode_utf16().collect::<Vec<_>>()).is_ok());
    }

    // ---------------------------------------------------- mac targets

    #[test]
    fn mac_targets_map_transport_with_bluetooth_as_usb_and_stay_in_our_namespace() {
        let cases = [
            (T::BuiltIn, mac::transport::BUILT_IN),
            (T::Usb, mac::transport::USB),
            (T::Bluetooth, mac::transport::USB),
            (T::Hdmi, mac::transport::HDMI),
            (T::DisplayPort, mac::transport::DISPLAYPORT),
            (T::Thunderbolt, mac::transport::THUNDERBOLT),
            (T::Pci, mac::transport::PCI),
            (T::FireWire, mac::transport::FIREWIRE),
        ];
        for (t, want) in cases {
            let mut d = desc("1111111111111111");
            d.transport = t;
            let got = mac::target(Direction::Output, &Presentation::Physical(d), 0);
            assert_eq!(got.transport, want, "{t:?}");
            assert!(mac::transport::WHITELIST.contains(&got.transport));
            assert_ne!(got.transport, u32::from_be_bytes(*b"blue"));
        }
        for ff in [
            FormFactor::Speakers,
            FormFactor::Headphones,
            FormFactor::Headset,
            FormFactor::Microphone,
            FormFactor::Handset,
            FormFactor::LineLevel,
            FormFactor::Spdif,
            FormFactor::DigitalDisplay,
            FormFactor::Unknown,
        ] {
            let mut d = desc("1111111111111111");
            d.form_factor = ff;
            let p = Presentation::Physical(d);
            let o = mac::target(Direction::Output, &p, 0);
            let i = mac::target(Direction::Input, &p, 0);
            assert!(
                mac::terminal::OUTPUT_WHITELIST.contains(&o.terminal_type),
                "{ff:?}"
            );
            assert!(
                mac::terminal::INPUT_WHITELIST.contains(&i.terminal_type),
                "{ff:?}"
            );
            for (t, dir) in [(&o, Direction::Output), (&i, Direction::Input)] {
                let uid = t.model_uid(dir);
                assert!(uid.starts_with(mac::MODEL_UID_PREFIX), "{uid}");
                assert!(!t.model_suffix.is_empty());
                assert!(t.model_suffix.len() < mac::MODEL_SUFFIX_BUF);
                assert!(
                    t.model_suffix.bytes().all(|b| b.is_ascii_alphabetic()),
                    "{}",
                    t.model_suffix
                );
                assert_eq!(t.klass, mac::KLASS_PHYSICAL);
            }
        }
        let mut d = desc("1111111111111111");
        d.form_factor = FormFactor::Headset;
        let p = Presentation::Physical(d);
        assert_eq!(
            mac::target(Direction::Input, &p, 0).terminal_type,
            mac::terminal::HEADSET_MICROPHONE
        );
        assert_eq!(
            mac::target(Direction::Output, &p, 0).icon_kind,
            mac::icon::HEADSET
        );
        let v = mac::target(Direction::Output, &Presentation::Virtual, 7);
        assert_eq!(
            (
                v.klass,
                v.transport,
                v.related_group,
                v.manufacturer.as_str()
            ),
            (mac::KLASS_VIRTUAL, mac::transport::VIRTUAL, 0, "AudioHub")
        );
        assert_eq!(v.model_uid(Direction::Output), "AudioHub_PeerOutput");
        assert_eq!(
            mac::target(Direction::Input, &Presentation::Virtual, 0).model_uid(Direction::Input),
            "AudioHub_PeerInput"
        );
    }

    #[test]
    fn manufacturer_placeholders_become_unknown_and_long_vendors_fit() {
        let target = |m: Option<&str>| {
            let mut d = desc("1111111111111111");
            d.manufacturer = m.map(str::to_string);
            mac::target(Direction::Output, &Presentation::Physical(d), 0).manufacturer
        };
        assert_eq!(target(Some("Jabra")), "Jabra");
        assert_eq!(target(Some("Apple Inc.")), "Apple Inc.");
        for generic in [
            None,
            Some("(Generic USB Audio)"),
            Some("Microsoft"),
            Some("generic"),
            Some("  "),
        ] {
            assert_eq!(target(generic), UNKNOWN_MANUFACTURER, "{generic:?}");
        }
        let long = target(Some(&"Ü".repeat(60)));
        assert!(long.len() < mac::MANUFACTURER_BUF);
        assert!(long.starts_with('Ü'));
    }

    #[test]
    fn related_group_needs_both_directions_physical_with_one_nonzero_group() {
        let p = |g: u8| {
            let mut d = desc("1111111111111111");
            d.group = g;
            Presentation::Physical(d)
        };
        assert_eq!(related_group(&p(3), &p(3)), 3);
        assert_eq!(related_group(&p(3), &p(4)), 0);
        assert_eq!(related_group(&p(0), &p(0)), 0);
        assert_eq!(related_group(&p(3), &Presentation::Virtual), 0);
        assert_eq!(
            related_group(&Presentation::Virtual, &Presentation::Virtual),
            0
        );
    }

    // ------------------------------------------------ windows targets

    #[test]
    fn windows_presentation_key_and_reference_strings_fit_the_driver_limit() {
        let token = "0011223344556677";
        let key = win::presentation_key(FP, token, win::ff::HEADPHONES);
        assert_eq!(key.len(), win::PRESKEY_HEX);
        assert!(key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        // SHA-256("0123456789abcdef00112233445566773")[..4], computed
        // independently of this function: fp, token, then the form-factor
        // code as decimal ASCII.
        let mut h = Sha256::new();
        h.update(format!("{FP}{token}3").as_bytes());
        let want: String = h.finalize()[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(key, want);
        // A fixed vector, so a PowerShell or C reimplementation used by the
        // VM evidence can be checked against the daemon byte for byte.
        assert_eq!(win::presentation_key(FP, token, 1), {
            let mut h = Sha256::new();
            h.update(b"0123456789abcdef00112233445566771");
            h.finalize()[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()
        });
        assert_eq!(key, win::presentation_key(FP, token, 3), "deterministic");
        assert_ne!(key, win::presentation_key(FP, "0011223344556678", 3));
        assert_ne!(key, win::presentation_key("fedcba9876543210", token, 3));
        // The same device under another form factor is another endpoint.
        assert_ne!(key, win::presentation_key(FP, token, win::ff::SPEAKERS));
        assert_ne!(key, win::presentation_key(FP, token, win::ff::HEADSET));
        assert!(key.len() + 1 <= win::PRESKEY_BUF);

        for t in win::TEMPLATES {
            assert_eq!(
                win::reference_string(t, FP, None).unwrap(),
                format!("{t}-{FP}")
            );
            let phys = win::reference_string(t, FP, Some(&key)).unwrap();
            assert_eq!(phys, format!("{t}-{FP}-p{key}"));
            assert!(phys.len() < win::REFSTRING_MAX, "{phys}");
        }
        let longest = win::TEMPLATES.iter().map(|t| t.len()).max().unwrap();
        assert!(longest + 1 + 16 + 2 + win::PRESKEY_HEX < win::REFSTRING_MAX);
        assert_eq!(win::reference_string("AhOther", FP, None), None);
        assert_eq!(win::reference_string("AhTopoOut", "not-a-fp", None), None);
        assert_eq!(
            win::reference_string("AhTopoOut", FP, Some("ABCDEF01")),
            None
        );
        assert_eq!(win::reference_string("AhTopoOut", FP, Some("abc")), None);
        assert_eq!(
            win::reference_string("AhTopoOut", FP, Some("abcdef0/")),
            None
        );
    }

    #[test]
    fn windows_targets_choose_form_factor_and_pin_category_per_direction() {
        let names = win::NameCtx {
            peer_label: "WIN-30",
            locale: "en-US",
        };
        let target = |dir, ff| {
            let mut d = desc("0011223344556677");
            d.form_factor = ff;
            win::target(dir, FP, &Presentation::Physical(d), 0, &names)
        };
        use win::PinCategory as P;
        let out = [
            (FormFactor::Speakers, 1, P::Speaker),
            (FormFactor::Headphones, 3, P::Headphones),
            (FormFactor::Headset, 5, P::HeadsetSpeakers),
            (FormFactor::Handset, 6, P::Handset),
            // T6 decision: digital displays render as speakers.
            (FormFactor::DigitalDisplay, 1, P::Speaker),
            (FormFactor::LineLevel, 1, P::Speaker),
            (FormFactor::Unknown, 1, P::Speaker),
        ];
        for (ff, want_ff, want_pin) in out {
            let t = target(Direction::Output, ff);
            assert_eq!(
                (t.form_factor, t.pin_category),
                (want_ff, want_pin),
                "{ff:?}"
            );
            assert_eq!(t.klass, win::PRES_PHYSICAL);
            assert_eq!(t.key, win::presentation_key(FP, "0011223344556677", want_ff));
            // What the driver accepts, exactly: same rules on both sides.
            let wire = t.to_wire();
            assert!(wire.valid_for(true), "{ff:?}: {wire:?}");
            assert!(wire.is_physical());
            assert_eq!(wire.form_factor, want_ff);
        }
        let inp = [
            (FormFactor::Microphone, 4, P::Microphone),
            (FormFactor::Headset, 5, P::HeadsetMicrophone),
            (FormFactor::Handset, 6, P::Handset),
            (FormFactor::LineLevel, 4, P::Microphone),
            (FormFactor::Unknown, 4, P::Microphone),
        ];
        for (ff, want_ff, want_pin) in inp {
            let t = target(Direction::Input, ff);
            assert_eq!(
                (t.form_factor, t.pin_category),
                (want_ff, want_pin),
                "{ff:?}"
            );
            assert!(t.to_wire().valid_for(false), "{ff:?}");
        }
        // Each direction keys its OWN form factor, so a provider's speakers
        // (1) and microphone (4) with the same token still differ.
        assert_ne!(
            target(Direction::Output, FormFactor::Speakers).key,
            target(Direction::Input, FormFactor::Microphone).key
        );
        let t = target(Direction::Output, FormFactor::Headset);
        assert_eq!(t.endpoint_desc, "Headset Earphone");
        assert_eq!(t.adapter_name, "Jabra Evolve2 65 \u{2013} WIN-30");
        assert_eq!(t.manufacturer, "Jabra");
        let v = win::target(Direction::Input, FP, &Presentation::Virtual, 3, &names);
        assert_eq!(
            (
                v.klass,
                v.form_factor,
                v.pin_category,
                v.group,
                v.key.as_str()
            ),
            (win::PRES_VIRTUAL, 4, P::MicrophoneArray, 0, "")
        );
        assert_eq!(v.pin_category.ksnodetype(), "KSNODETYPE_MICROPHONE_ARRAY");
        // A virtual target carries no identity (no key, class virtual, so the
        // same reference strings as 1.0.1), only v11's two names: the
        // direction noun and the peer's virtual devnode name.
        let wire = v.to_wire();
        assert_eq!(
            (wire.klass, wire.form_factor, wire.group, wire.key.as_str()),
            (win::PRES_VIRTUAL, 0, 0, "")
        );
        assert_eq!(wire.endpoint_desc, "Microphone");
        assert_eq!(wire.adapter_name, "AudioHub \u{2013} WIN-30");
        assert!(wire.valid_for(false));
        let v = win::target(Direction::Output, FP, &Presentation::Virtual, 0, &names);
        assert_eq!((v.form_factor, v.pin_category), (1, P::Speaker));
        assert_eq!(v.to_wire().endpoint_desc, "Speakers");
        assert!(v.to_wire().valid_for(true));
        // A fingerprint the driver could not build an identity from: virtual.
        let bad = win::target(
            Direction::Output,
            "zz",
            &Presentation::Physical(desc("0011223344556677")),
            0,
            &names,
        );
        assert_eq!(bad.klass, win::PRES_VIRTUAL);
    }

    #[test]
    fn windows_standalone_units_without_an_adapter_half_get_distinct_devnode_names() {
        // A macOS provider: no endpoint/adapter halves, ungrouped output and
        // input. Two children of one peer must not share a devnode name.
        let zh = win::NameCtx {
            peer_label: "Mac",
            locale: "zh-CN",
        };
        let mut out = desc("0011223344556677");
        out.endpoint_desc = None;
        out.adapter_name = None;
        out.name = "T6 USB Headphones".into();
        out.form_factor = FormFactor::Headphones;
        let mut inp = out.clone();
        inp.name = "T6 USB Microphone".into();
        inp.form_factor = FormFactor::Microphone;
        inp.device_token = "8899aabbccddeeff".into();
        let o = win::target(Direction::Output, FP, &Presentation::Physical(out.clone()), 0, &zh);
        let i = win::target(Direction::Input, FP, &Presentation::Physical(inp.clone()), 0, &zh);
        assert_eq!((o.endpoint_desc.as_str(), o.adapter_name.as_str()), ("耳机", "T6 USB Headphones \u{2013} Mac"));
        assert_eq!((i.endpoint_desc.as_str(), i.adapter_name.as_str()), ("麦克风", "T6 USB Microphone \u{2013} Mac"));
        assert_ne!(o.adapter_name, i.adapter_name);
        let en = win::NameCtx {
            peer_label: "Mac",
            locale: "en-US",
        };
        let o = win::target(Direction::Output, FP, &Presentation::Physical(out.clone()), 0, &en);
        // Never an offline mark on Windows (frozen with the endpoint id).
        assert_eq!(o.endpoint_desc, "Headphones");
        assert!(o.to_wire().valid_for(true));
        // Grouped (one RC child): the device name stays in the endpoint half
        // and the devnode is named after the peer, as §5.2 has it.
        let g = win::target(Direction::Output, FP, &Presentation::Physical(out.clone()), 7, &zh);
        assert_eq!((g.endpoint_desc.as_str(), g.adapter_name.as_str()), ("T6 USB Headphones", "Mac"));
        // A Windows provider (adapter half present) keeps the §5.2 split.
        let mut w = out;
        w.endpoint_desc = Some("Headphones".into());
        w.adapter_name = Some("Realtek(R) Audio".into());
        let t = win::target(Direction::Output, FP, &Presentation::Physical(w), 0, &zh);
        assert_eq!((t.endpoint_desc.as_str(), t.adapter_name.as_str()), ("Headphones", "Realtek(R) Audio \u{2013} Mac"));
    }

    // ------------------------------------------------------------ gate

    #[test]
    fn the_mic_gate_engages_exactly_when_the_look_may_lie() {
        let a = Presentation::Physical(desc("aaaaaaaaaaaaaaaa"));
        let b = Presentation::Physical(desc("bbbbbbbbbbbbbbbb"));
        let v = Presentation::Virtual;
        let known = |p: &Presentation| Applied::Known(p.clone());
        // Virtual look: never gated (a virtual look cannot launder).
        for t in [&v, &a, &b] {
            for prov in [false, true] {
                assert!(!mic_gate_engaged(&known(&v), t, prov));
            }
        }
        // Physical A matching facts: open; withdrawn or replaced: gated.
        assert!(!mic_gate_engaged(&known(&a), &a, false));
        assert!(mic_gate_engaged(&known(&a), &v, false));
        assert!(mic_gate_engaged(&known(&a), &b, false));
        // Persisted look on a live connection without facts yet: gated.
        assert!(mic_gate_engaged(&known(&a), &a, true));
        // Unknown look: gated whatever the target.
        for t in [&v, &a] {
            assert!(mic_gate_engaged(&Applied::Unknown, t, false));
        }
        // A rename of the same device is not a mismatch.
        let mut renamed = desc("aaaaaaaaaaaaaaaa");
        renamed.name = "Renamed".into();
        assert!(!mic_gate_engaged(
            &known(&a),
            &Presentation::Physical(renamed),
            false
        ));
        // A driver that cannot present physical presents virtual.
        for driver in [
            DriverPresentation::Unsupported,
            DriverPresentation::CallerCheckInsufficient,
        ] {
            assert_eq!(effective_applied(driver, &Applied::Unknown), known(&v));
            assert_eq!(effective_applied(driver, &known(&a)), known(&v));
        }
        assert_eq!(
            effective_applied(DriverPresentation::Supported, &known(&a)),
            known(&a)
        );
    }

    #[test]
    fn the_gate_flags_are_per_slot_and_silence_only_the_gated_block() {
        let g = MicPresentationGate::new();
        assert_eq!(g.mask(), 0);
        g.store(0b1000_0000_0000_0101);
        assert!(g.is_gated(0) && !g.is_gated(1) && g.is_gated(2) && g.is_gated(15));
        assert!(
            !g.is_gated(16),
            "out-of-range slot is not gated (and does not panic)"
        );
        let mut block = [0.5f32; 480];
        silence_if_gated(false, &mut block);
        assert!(block.iter().all(|&s| s == 0.5));
        silence_if_gated(true, &mut block);
        assert!(block.iter().all(|&s| s == 0.0));
    }

    // ------------------------------------------------------- scheduler

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn windows_waits_two_seconds_and_absorbs_a_transient_unknown() {
        let p = WINDOWS_POLICY;
        let a = Presentation::Physical(desc("aaaaaaaaaaaaaaaa"));
        let applied = Applied::Known(a.clone());
        let mut tr = RebuildTracker::default();
        let start = t0();
        // The provider publishes Unknown (→ Virtual) for 300 ms, then A again.
        assert_eq!(
            tr.tick(
                start,
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        let ms = |n| start + Duration::from_millis(n);
        assert_eq!(
            tr.tick(
                ms(300),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        assert_eq!(
            tr.tick(ms(400), &p, Direction::Input, &a, &applied, false),
            None
        );
        assert!(tr.requested().is_none());
        // A real withdrawal: the mic one runs after 2 s even while busy.
        assert_eq!(
            tr.tick(
                ms(1000),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        assert_eq!(
            tr.tick(
                ms(2999),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        assert_eq!(
            tr.tick(
                ms(3000),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            Some(ChangeClass::Withdrawal)
        );
        assert_eq!(tr.requested(), Some(&Presentation::Virtual));
        // Issued once; acknowledged → nothing outstanding.
        assert_eq!(
            tr.tick(
                ms(3200),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        tr.applied(&Presentation::Virtual);
        assert!(tr.requested().is_none());
        assert_eq!(
            tr.tick(
                ms(3400),
                &p,
                Direction::Input,
                &Presentation::Virtual,
                &Applied::Known(Presentation::Virtual),
                false
            ),
            None
        );
    }

    #[test]
    fn windows_speaker_changes_and_all_upgrades_wait_for_idle() {
        let p = WINDOWS_POLICY;
        let a = Presentation::Physical(desc("aaaaaaaaaaaaaaaa"));
        let b = Presentation::Physical(desc("bbbbbbbbbbbbbbbb"));
        let start = t0();
        let later = start + Duration::from_secs(5);
        // Speaker A → B: stable but busy waits, idle goes.
        let mut tr = RebuildTracker::default();
        let applied = Applied::Known(a.clone());
        assert_eq!(
            tr.tick(start, &p, Direction::Output, &b, &applied, false),
            None
        );
        assert_eq!(
            tr.tick(later, &p, Direction::Output, &b, &applied, false),
            None
        );
        assert_eq!(
            tr.tick(later, &p, Direction::Output, &b, &applied, true),
            Some(ChangeClass::Change)
        );
        // Speaker withdrawal also waits.
        let mut tr = RebuildTracker::default();
        assert_eq!(
            tr.tick(
                start,
                &p,
                Direction::Output,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        assert_eq!(
            tr.tick(
                later,
                &p,
                Direction::Output,
                &Presentation::Virtual,
                &applied,
                false
            ),
            None
        );
        assert_eq!(
            tr.tick(
                later,
                &p,
                Direction::Output,
                &Presentation::Virtual,
                &applied,
                true
            ),
            Some(ChangeClass::Withdrawal)
        );
        // Mic upgrade waits for idle; mic A → B does not.
        let v = Applied::Known(Presentation::Virtual);
        let mut tr = RebuildTracker::default();
        assert_eq!(tr.tick(start, &p, Direction::Input, &a, &v, false), None);
        assert_eq!(tr.tick(later, &p, Direction::Input, &a, &v, false), None);
        assert_eq!(
            tr.tick(later, &p, Direction::Input, &a, &v, true),
            Some(ChangeClass::Upgrade)
        );
        let mut tr = RebuildTracker::default();
        assert_eq!(
            tr.tick(start, &p, Direction::Input, &b, &applied, false),
            None
        );
        assert_eq!(
            tr.tick(later, &p, Direction::Input, &b, &applied, false),
            Some(ChangeClass::Change)
        );
    }

    #[test]
    fn a_target_that_moves_restarts_stability_and_withdraws_a_stale_request() {
        let p = WINDOWS_POLICY;
        let a = Presentation::Physical(desc("aaaaaaaaaaaaaaaa"));
        let b = Presentation::Physical(desc("bbbbbbbbbbbbbbbb"));
        let c = Presentation::Physical(desc("cccccccccccccccc"));
        let applied = Applied::Known(a.clone());
        let start = t0();
        let s = |n| start + Duration::from_millis(n);
        let mut tr = RebuildTracker::default();
        assert_eq!(
            tr.tick(s(0), &p, Direction::Input, &b, &applied, true),
            None
        );
        assert_eq!(
            tr.tick(s(1500), &p, Direction::Input, &c, &applied, true),
            None
        );
        assert_eq!(
            tr.tick(s(2500), &p, Direction::Input, &c, &applied, true),
            None,
            "c only started at 1.5 s"
        );
        assert_eq!(
            tr.tick(s(3500), &p, Direction::Input, &c, &applied, true),
            Some(ChangeClass::Change)
        );
        assert_eq!(tr.requested(), Some(&c));
        // The target moves on before the platform acted: the request goes.
        assert_eq!(
            tr.tick(s(3600), &p, Direction::Input, &b, &applied, true),
            None
        );
        assert!(tr.requested().is_none());
        // Back to the applied device: nothing to do at all.
        assert_eq!(
            tr.tick(s(3700), &p, Direction::Input, &a, &applied, true),
            None
        );
        assert!(tr.requested().is_none());
    }

    #[test]
    fn macos_withdraws_at_once_and_upgrades_after_stability_without_idle() {
        let p = MACOS_POLICY;
        let a = Presentation::Physical(desc("aaaaaaaaaaaaaaaa"));
        let start = t0();
        for dir in DIRECTIONS {
            let mut tr = RebuildTracker::default();
            assert_eq!(
                tr.tick(
                    start,
                    &p,
                    dir,
                    &Presentation::Virtual,
                    &Applied::Known(a.clone()),
                    false
                ),
                Some(ChangeClass::Withdrawal),
                "{dir:?}"
            );
            let mut tr = RebuildTracker::default();
            let v = Applied::Known(Presentation::Virtual);
            assert_eq!(tr.tick(start, &p, dir, &a, &v, false), None);
            assert_eq!(
                tr.tick(start + Duration::from_secs(2), &p, dir, &a, &v, false),
                Some(ChangeClass::Upgrade)
            );
            // Unknown applied state is re-asserted at once.
            let mut tr = RebuildTracker::default();
            assert_eq!(
                tr.tick(start, &p, dir, &a, &Applied::Unknown, false),
                Some(ChangeClass::Change)
            );
        }
    }

    // --------------------------------------------- state / persistence

    fn view(version: Option<u8>, obs: Option<(DirectionFacts, DirectionFacts)>) -> PeerFactsView {
        PeerFactsView {
            facts_version: version,
            observation: obs.map(|(input, output)| DeviceFactsObservation {
                revision: 7,
                input,
                output,
            }),
        }
    }

    fn peer(view: Option<PeerFactsView>, cached: Option<CachedFacts>) -> PeerInputs {
        PeerInputs {
            fingerprint: FP.into(),
            slot: Some(2),
            label: "WIN-30".into(),
            marked_offline: false,
            view,
            cached,
            idle: [true, true],
        }
    }

    fn eval(st: &mut PresentationState, now: Instant, p: PeerInputs) -> EvalOutcome {
        st.evaluate(EvalInputs {
            now,
            consumer_enabled: true,
            accept_injected: false,
            locale: "en-US".into(),
            peers: vec![p],
        })
    }

    #[test]
    fn persisted_facts_follow_the_section_4_5_rules() {
        let a = physical("aaaaaaaaaaaaaaaa");
        let v = DirectionFacts::virtual_("transport_virtual");
        let cached = CachedFacts::new(a.clone(), v.clone());
        // Offline: the cache stands, not provisional.
        let r = resolve_facts(None, Some(&cached));
        assert_eq!((r.source, r.provisional), (FactsSource::Cache, false));
        assert_eq!(r.facts[IN].as_ref(), Some(&a));
        // Connected old peer: virtual, cache cleared.
        let r = resolve_facts(Some(&view(Some(0), None)), Some(&cached));
        assert!(matches!(r.cache, Some(CacheUpdate::Clear)));
        assert_eq!(r.version, Some(0));
        // Supporting peer, facts in flight: cache, provisional.
        for ver in [None, Some(1)] {
            let r = resolve_facts(Some(&view(ver, None)), Some(&cached));
            assert_eq!(
                (r.source, r.provisional),
                (FactsSource::Cache, true),
                "{ver:?}"
            );
            assert!(r.cache.is_none());
        }
        // Facts arrive: they win and replace the cache only when different.
        let r = resolve_facts(
            Some(&view(Some(1), Some((v.clone(), v.clone())))),
            Some(&cached),
        );
        assert_eq!(r.source, FactsSource::Live);
        assert!(matches!(r.cache, Some(CacheUpdate::Set(ref c)) if c.input == v));
        let r = resolve_facts(
            Some(&view(Some(1), Some((a.clone(), v.clone())))),
            Some(&cached),
        );
        assert!(r.cache.is_none(), "unchanged facts are not rewritten");
        // Nothing persisted and nothing live: no physical anywhere.
        let r = resolve_facts(Some(&view(Some(1), None)), None);
        assert_eq!(r.source, FactsSource::Pending);
        assert_eq!(
            r.facts[IN].as_ref().map(|f| f.class),
            Some(DeviceClass::Unknown)
        );
    }

    #[test]
    fn evaluation_gates_a_withdrawn_microphone_until_the_driver_confirms_virtual() {
        let a = physical("aaaaaaaaaaaaaaaa");
        let v = DirectionFacts::virtual_("transport_virtual");
        let live = |input: &DirectionFacts| {
            peer(Some(view(Some(1), Some((input.clone(), v.clone())))), None)
        };
        let start = t0();
        // Nothing reported by the driver yet (e.g. right after a daemon
        // restart, the driver may still present an old physical look): gated.
        {
            let mut fresh = PresentationState::new();
            fresh.set_policy(MACOS_POLICY);
            fresh.set_driver(DriverPresentation::Supported);
            let g = fresh.gate();
            eval(&mut fresh, start, live(&a));
            assert!(fresh.mic_gated(FP) && g.is_gated(2));
        }
        let mut st = PresentationState::new();
        st.set_policy(MACOS_POLICY);
        st.set_driver(DriverPresentation::Supported);
        let gate = st.gate();
        st.mark_applied(FP, Direction::Input, Presentation::Virtual);
        st.mark_applied(FP, Direction::Output, Presentation::Virtual);
        // Physical microphone arrives on a virtual look: open, upgrade waits
        // for 2 s of stability, the facts are persisted.
        let out = eval(&mut st, start, live(&a));
        assert!(!st.mic_gated(FP) && !gate.is_gated(2));
        assert!(out.requests.is_empty());
        assert!(matches!(out.cache.as_slice(), [(fp, CacheUpdate::Set(_))] if fp == FP));
        assert_eq!(st.current(FP)[IN], Presentation::Virtual);
        let out = eval(&mut st, start + Duration::from_secs(2), live(&a));
        assert!(matches!(out.requests.as_slice(),
            [r] if r.class == ChangeClass::Upgrade && r.direction == Direction::Input && r.slot == Some(2)));
        let physical_a = st.current(FP)[IN].clone();
        assert!(physical_a.is_physical());
        assert_eq!(st.current(FP)[OUT], Presentation::Virtual);
        // The driver presents it: audio flows.
        st.mark_applied(FP, Direction::Input, physical_a);
        eval(&mut st, start + Duration::from_secs(2), live(&a));
        assert!(!gate.is_gated(2), "matching physical look: audio flows");
        // Withdrawal: gated in the very pass that sees it, and (macOS) the
        // virtual look is requested at once.
        let out = eval(&mut st, start + Duration::from_secs(3), live(&v));
        assert!(gate.is_gated(2) && st.mic_gated(FP));
        assert!(matches!(out.requests.as_slice(), [r] if r.class == ChangeClass::Withdrawal));
        assert_eq!(st.current(FP)[IN], Presentation::Virtual);
        let status = st.status_json();
        assert_eq!(status["peers"][FP]["input"]["mic_gate"], true, "{status}");
        assert_eq!(status["mic_gate_mask"], 4, "{status}");
        // Confirmed virtual: released.
        st.mark_applied(FP, Direction::Input, Presentation::Virtual);
        eval(&mut st, start + Duration::from_secs(3), live(&v));
        assert!(!gate.is_gated(2));
        let status = st.status_json();
        let input = &status["peers"][FP]["input"];
        assert_eq!(input["mic_gate"], false, "{status}");
        assert_eq!(input["target"]["class"], "virtual");
        assert_eq!(input["mac"]["transport"], "virt");
        assert_eq!(status["driver"], "supported");
        assert_eq!(st.driver(), DriverPresentation::Supported);
    }

    #[test]
    fn a_reconnect_keeps_the_persisted_look_but_silences_it_until_facts_confirm() {
        let mut st = PresentationState::new();
        st.set_policy(MACOS_POLICY);
        st.set_driver(DriverPresentation::Supported);
        let a = physical("aaaaaaaaaaaaaaaa");
        let v = DirectionFacts::virtual_("transport_virtual");
        let cached = CachedFacts::new(a.clone(), v.clone());
        let start = t0();
        let pa = Presentation::Physical(a.physical.clone().unwrap());
        st.mark_applied(FP, Direction::Input, pa.clone());
        st.mark_applied(FP, Direction::Output, Presentation::Virtual);
        // Offline: cache stands, audio could not flow anyway, not gated.
        eval(&mut st, start, peer(None, Some(cached.clone())));
        assert!(!st.mic_gated(FP));
        assert_eq!(st.current(FP)[IN], pa);
        // Connected, facts in flight: same look, provisional → gated.
        eval(
            &mut st,
            start,
            peer(Some(view(Some(1), None)), Some(cached.clone())),
        );
        assert!(st.mic_gated(FP));
        assert_eq!(st.current(FP)[IN], pa, "no flap through virtual");
        // Facts confirm → open.
        eval(
            &mut st,
            start,
            peer(
                Some(view(Some(1), Some((a.clone(), v.clone())))),
                Some(cached.clone()),
            ),
        );
        assert!(!st.mic_gated(FP));
        // An old peer instead: withdrawn, gated, cache cleared.
        let out = eval(
            &mut st,
            start,
            peer(Some(view(Some(0), None)), Some(cached)),
        );
        assert!(st.mic_gated(FP));
        assert!(matches!(out.cache.as_slice(), [(_, CacheUpdate::Clear)]));
        assert_eq!(
            st.target(FP, Direction::Input).unwrap().reason,
            local_reason::PEER_UNSUPPORTED
        );
    }

    /// T6 (Windows, design §4.5): after a daemon start or a driver re-attach
    /// the cached physical look is published at once, not Virtual first and
    /// physical two seconds later (two endpoint rebuilds on every start).
    #[test]
    fn windows_adopts_the_cached_physical_look_when_the_driver_account_is_unknown() {
        let mut st = PresentationState::new();
        st.set_policy(WINDOWS_POLICY);
        let a = physical("aaaaaaaaaaaaaaaa");
        let v = DirectionFacts::virtual_("transport_virtual");
        let cached = CachedFacts::new(v.clone(), a.clone()); // input virtual, output physical
        let pa = Presentation::Physical(a.physical.clone().unwrap());
        let start = t0();
        // Driver not (yet) known to present: nothing is adopted, all virtual.
        eval(&mut st, start, peer(None, Some(cached.clone())));
        assert_eq!(st.adopt_physical_when_unknown(FP), [false, false]);
        assert_eq!(st.current(FP), [Presentation::Virtual, Presentation::Virtual]);
        // Supported driver, account Unknown (fresh start): the decided output
        // look is adopted immediately; the virtual input is left alone.
        st.set_driver(DriverPresentation::Supported);
        eval(&mut st, start, peer(None, Some(cached.clone())));
        assert_eq!(st.adopt_physical_when_unknown(FP), [true, false]);
        assert_eq!(st.current(FP)[OUT], pa);
        // Nothing left to schedule: the tracker sees no change.
        let out = eval(&mut st, start, peer(None, Some(cached.clone())));
        assert!(out.requests.is_empty());
        // A known account is never overridden (only Unknown is adopted).
        st.mark_applied(FP, Direction::Output, Presentation::Virtual);
        assert_eq!(st.adopt_physical_when_unknown(FP), [false, false]);
        assert_eq!(st.current(FP)[OUT], Presentation::Virtual);
        // A re-attach makes the account Unknown again: adopted again.
        st.mark_all_unknown();
        eval(&mut st, start, peer(None, Some(cached.clone())));
        assert_eq!(st.adopt_physical_when_unknown(FP), [true, false]);
        // A caller check below IMAGEPATH presents nothing physical.
        st.mark_all_unknown();
        st.set_driver(DriverPresentation::CallerCheckInsufficient);
        eval(&mut st, start, peer(None, Some(cached)));
        assert_eq!(st.adopt_physical_when_unknown(FP), [false, false]);
        assert_eq!(st.current(FP), [Presentation::Virtual, Presentation::Virtual]);
    }

    #[test]
    fn without_a_presenting_driver_nothing_is_physical_or_gated() {
        let mut st = PresentationState::new();
        let a = physical("aaaaaaaaaaaaaaaa");
        for driver in [
            DriverPresentation::Unsupported,
            DriverPresentation::CallerCheckInsufficient,
        ] {
            st.set_driver(driver);
            let out = eval(
                &mut st,
                t0(),
                peer(Some(view(Some(1), Some((a.clone(), a.clone())))), None),
            );
            assert!(out.requests.is_empty());
            assert_eq!(out.gate_mask, 0);
            assert_eq!(
                st.current(FP),
                [Presentation::Virtual, Presentation::Virtual]
            );
        }
        // Peers that disappear from the pass are forgotten.
        st.evaluate(EvalInputs {
            now: t0(),
            consumer_enabled: true,
            accept_injected: false,
            locale: "en-US".into(),
            peers: vec![],
        });
        assert!(st.target(FP, Direction::Input).is_none());
    }

    #[test]
    fn the_consumer_switch_off_withdraws_and_gates_a_physical_microphone() {
        let mut st = PresentationState::new();
        st.set_policy(WINDOWS_POLICY);
        st.set_driver(DriverPresentation::Supported);
        let a = physical("aaaaaaaaaaaaaaaa");
        st.mark_applied(
            FP,
            Direction::Input,
            Presentation::Physical(a.physical.clone().unwrap()),
        );
        let out = st.evaluate(EvalInputs {
            now: t0(),
            consumer_enabled: false,
            accept_injected: false,
            locale: "en-US".into(),
            peers: vec![peer(
                Some(view(Some(1), Some((a.clone(), a.clone())))),
                None,
            )],
        });
        assert!(st.mic_gated(FP));
        assert_eq!(out.gate_mask, 1 << 2);
        assert_eq!(
            st.target(FP, Direction::Input).unwrap().reason,
            why::CONSUMER_DISABLED
        );
    }

    #[test]
    fn cached_facts_round_trip_and_sanitize() {
        let c = CachedFacts::new(physical("aaaaaaaaaaaaaaaa"), DirectionFacts::absent());
        let text = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<CachedFacts>(&text).unwrap(), c);
        let bad = CachedFacts::new(invalid_physical(), DirectionFacts::absent()).sanitized();
        assert_eq!(bad.input.class, DeviceClass::Virtual);
        assert_eq!(bad.input.reason, reason::INVALID_FACTS);
        assert_eq!(bad.output, DirectionFacts::absent());
    }
}
