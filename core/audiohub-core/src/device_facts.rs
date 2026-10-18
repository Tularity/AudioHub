//! Physical/virtual classification of this machine's current default audio
//! endpoints (docs/physical-presentation-1.0.2-design.md §3).
//!
//! A provider publishes, per direction, whether its current default device is
//! a real piece of hardware and, only if it is, a small descriptive record a
//! consumer may mirror. The decision is a gate, so every path that is not a
//! positive, fully read, whitelisted observation lands on a non-physical
//! class: unreadable property, unresolved devnode, unknown enumerator, third
//! party plug-in, aggregate with any doubtful member, AudioHub's own device.
//!
//! Layout:
//! - the wire model ([`DeviceFactsObservation`] and friends) and its
//!   validation;
//! - raw, local-only property snapshots ([`WindowsEndpointSnapshot`],
//!   [`MacDeviceSnapshot`]) that carry vendor identity such as hardware IDs,
//!   devnode instance IDs and CoreAudio UIDs. They exist so the classifier can
//!   be exercised with recorded fixtures on any OS and are NEVER sent to a
//!   peer: the classifier copies descriptive text only, and the device
//!   identity it emits is a one-way token ([`device_token`]);
//! - the pure classifiers ([`classify_windows`], [`classify_macos`],
//!   [`classify_raw`]);
//! - the platform probes (`device_facts_windows.rs`, `device_facts_macos.rs`),
//!   which read properties only: no stream is opened and no permission prompt
//!   can be raised, the same contract as `output_capabilities`.
//!
//! What the gate is and is not: it keeps an unmodified AudioHub build from
//! presenting a virtual source (VB-Cable, BlackHole, an AirPlay relay,
//! AudioHub's own devices) as physical. It is not an anti-forgery boundary;
//! a hand-written peer can claim anything (design §2.4).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::audio::DeviceKind;

/// Value a peer advertises in `AudioCapabilities.device_facts_version` when it
/// sends and understands `SessionMsg::DeviceFacts`. Zero means neither.
pub const DEVICE_FACTS_VERSION: u8 = 1;

/// Upper bound for every descriptive string on the wire, in UTF-8 bytes. Both
/// consumer drivers carry names in 128-unit buffers including the terminator.
pub const MAX_TEXT_BYTES: usize = 127;

const MAX_REASON_BYTES: usize = 64;
const TOKEN_LABEL: &[u8] = b"audiohub-device-token-v1";

// --------------------------------------------------------------- wire model

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    Physical,
    Virtual,
    /// Not (yet) known. Used by the publisher while a probe is pending or
    /// timed out. Consumers treat it exactly like `Virtual`.
    #[default]
    Unknown,
    /// The provider has no default device in this direction.
    Absent,
}

/// What a consumer may present. Deliberately narrower than `devlat::Transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalTransport {
    BuiltIn,
    Usb,
    Bluetooth,
    Hdmi,
    DisplayPort,
    Thunderbolt,
    Pci,
    FireWire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFactor {
    Speakers,
    Headphones,
    Headset,
    Microphone,
    Handset,
    LineLevel,
    Spdif,
    DigitalDisplay,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalDescriptor {
    pub transport: PhysicalTransport,
    pub form_factor: FormFactor,
    /// Platform-neutral display name of the real device ("MacBook Pro Speakers",
    /// or the Windows composite "Speakers (Realtek(R) Audio)").
    pub name: String,
    /// Windows only: endpoint half (PKEY_Device_DeviceDesc, e.g. "Speakers").
    #[serde(default)]
    pub endpoint_desc: Option<String>,
    /// Windows only: adapter half (PKEY_DeviceInterface_FriendlyName, e.g. "Realtek(R) Audio").
    #[serde(default)]
    pub adapter_name: Option<String>,
    /// `None` when the provider has no trustworthy vendor string (Windows
    /// generic class-driver placeholders are dropped, design Q10).
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// 16 lowercase hex characters, see [`device_token`]. Opaque, stable per
    /// (provider, real device); never reversible to vendor identity.
    pub device_token: String,
    /// 0 = no grouping info. Equal non-zero values on input and output mean
    /// "same physical device" on the provider (shared ContainerId on Windows,
    /// same AudioObjectID or RelatedDevices on macOS). Never the raw container GUID.
    #[serde(default)]
    pub group: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FactsOrigin {
    /// Produced by this module's probe and classifier.
    #[default]
    Probe,
    /// Produced by the test-only injection path (design §8). Release consumers
    /// refuse it; this module never emits it.
    Injected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectionFacts {
    pub class: DeviceClass,
    /// Machine-readable reason, for logs/UI only (e.g. "adapter_enumerator_virtual",
    /// "owner_third_party"). `[a-z0-9_]{1,64}`.
    pub reason: String,
    /// Present iff class == Physical (data minimisation: virtual devices' names are not sent).
    #[serde(default)]
    pub physical: Option<PhysicalDescriptor>,
    #[serde(default)]
    pub origin: FactsOrigin,
}

impl Default for DirectionFacts {
    fn default() -> Self {
        DirectionFacts::unknown(reason::NOT_PROBED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceFactsObservation {
    /// Monotonic within one live connection, like NativeOutputObservation::revision.
    pub revision: u64,
    pub input: DirectionFacts,
    pub output: DirectionFacts,
}

/// Machine-readable reasons. Kept as constants so the publisher, the consumer
/// and the UI spell them identically.
pub mod reason {
    pub const PHYSICAL: &str = "physical";
    pub const NOT_PROBED: &str = "not_probed";
    pub const NO_DEFAULT_DEVICE: &str = "no_default_device";
    pub const PROBE_FAILED: &str = "probe_failed";
    pub const UNSUPPORTED_PLATFORM: &str = "unsupported_platform";
    pub const FAKE_AUDIO_BACKEND: &str = "fake_audio_backend";
    pub const INVALID_FACTS: &str = "invalid_facts";
    pub const AUDIOHUB_OWN: &str = "audiohub_own";
    pub const NAME_UNREADABLE: &str = "name_unreadable";
    pub const IDENTITY_UNREADABLE: &str = "identity_unreadable";
    // Windows
    pub const ADAPTER_UNRESOLVED: &str = "adapter_unresolved";
    pub const ADAPTER_ENUMERATOR_UNREADABLE: &str = "adapter_enumerator_unreadable";
    pub const ADAPTER_ENUMERATOR_VIRTUAL: &str = "adapter_enumerator_virtual";
    pub const ADAPTER_ENUMERATOR_UNRECOGNIZED: &str = "adapter_enumerator_unrecognized";
    pub const VIRTUAL_ANCESTOR: &str = "virtual_ancestor";
    pub const ANCESTOR_UNREADABLE: &str = "ancestor_unreadable";
    pub const ANCESTOR_ENUMERATOR_UNRECOGNIZED: &str = "ancestor_enumerator_unrecognized";
    pub const ANCESTOR_CHAIN_INCOMPLETE: &str = "ancestor_chain_incomplete";
    pub const CAPTURE_FORM_FACTOR: &str = "capture_form_factor";
    pub const REMOTE_NETWORK_FORM_FACTOR: &str = "remote_network_form_factor";
    // macOS
    pub const OWNER_UNREADABLE: &str = "owner_unreadable";
    pub const OWNER_THIRD_PARTY: &str = "owner_third_party";
    pub const TRANSPORT_UNREADABLE: &str = "transport_unreadable";
    pub const TRANSPORT_VIRTUAL: &str = "transport_virtual";
    pub const TRANSPORT_AIRPLAY: &str = "transport_airplay";
    pub const TRANSPORT_CONTINUITY: &str = "transport_continuity";
    pub const TRANSPORT_UNRECOGNIZED: &str = "transport_unrecognized";
    pub const AGGREGATE_EMPTY: &str = "aggregate_empty";
    pub const AGGREGATE_INCOMPLETE: &str = "aggregate_incomplete";
    pub const AGGREGATE_TOO_DEEP: &str = "aggregate_too_deep";
    pub const AGGREGATE_HAS_TAP: &str = "aggregate_has_tap";
    pub const AGGREGATE_MEMBER_NOT_PHYSICAL: &str = "aggregate_member_not_physical";
}

fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_optional_text(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|s| bounded_text(s, MAX_TEXT_BYTES))
}

fn valid_reason(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REASON_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_token(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl PhysicalDescriptor {
    pub fn is_valid(&self) -> bool {
        bounded_text(&self.name, MAX_TEXT_BYTES)
            && valid_optional_text(&self.endpoint_desc)
            && valid_optional_text(&self.adapter_name)
            && valid_optional_text(&self.manufacturer)
            && valid_token(&self.device_token)
    }
}

impl DirectionFacts {
    fn with_class(class: DeviceClass, why: &str) -> DirectionFacts {
        DirectionFacts {
            class,
            reason: why.to_string(),
            physical: None,
            origin: FactsOrigin::Probe,
        }
    }

    pub fn virtual_(why: &str) -> DirectionFacts {
        DirectionFacts::with_class(DeviceClass::Virtual, why)
    }

    pub fn unknown(why: &str) -> DirectionFacts {
        DirectionFacts::with_class(DeviceClass::Unknown, why)
    }

    pub fn absent() -> DirectionFacts {
        DirectionFacts::with_class(DeviceClass::Absent, reason::NO_DEFAULT_DEVICE)
    }

    pub fn physical(descriptor: PhysicalDescriptor) -> DirectionFacts {
        DirectionFacts {
            class: DeviceClass::Physical,
            reason: reason::PHYSICAL.to_string(),
            physical: Some(descriptor),
            origin: FactsOrigin::Probe,
        }
    }

    /// Structural validity of a (possibly remote) record. A record that fails
    /// this must be treated as `Virtual`, see [`DirectionFacts::sanitized`].
    pub fn is_valid(&self) -> bool {
        if !valid_reason(&self.reason) {
            return false;
        }
        match (&self.class, &self.physical) {
            (DeviceClass::Physical, Some(d)) => d.is_valid(),
            (DeviceClass::Physical, None) => false,
            (_, Some(_)) => false,
            (_, None) => true,
        }
    }

    /// `self` if valid, otherwise `Virtual("invalid_facts")`. The origin of an
    /// invalid record is not preserved: there is nothing left to present.
    pub fn sanitized(self) -> DirectionFacts {
        if self.is_valid() {
            self
        } else {
            DirectionFacts::virtual_(reason::INVALID_FACTS)
        }
    }

    /// The descriptor a consumer may present, if and only if this record is a
    /// valid physical observation. The injected-origin policy is the
    /// consumer's (design §5.1) and deliberately not applied here.
    pub fn physical_descriptor(&self) -> Option<&PhysicalDescriptor> {
        if self.class == DeviceClass::Physical && self.is_valid() {
            self.physical.as_ref()
        } else {
            None
        }
    }
}

impl DeviceFactsObservation {
    pub fn is_valid(&self) -> bool {
        self.input.is_valid() && self.output.is_valid()
    }

    /// Each invalid direction becomes `Virtual("invalid_facts")`; a valid one
    /// is kept, so one malformed half does not discard the other.
    pub fn sanitized(self) -> DeviceFactsObservation {
        DeviceFactsObservation {
            revision: self.revision,
            input: self.input.sanitized(),
            output: self.output.sanitized(),
        }
    }
}

/// `hex(SHA-256(label || 0x00 || provider_fp || 0x00 || native_endpoint_id)[..8])`.
///
/// The NUL separators make the preimage unambiguous (no (fp, id) pair can
/// collide with another by moving bytes across the boundary). The native id
/// (Windows endpoint ID, CoreAudio UID) may embed vendor IDs and serial
/// numbers; only this digest leaves the machine.
pub fn device_token(provider_fp: &str, native_endpoint_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(TOKEN_LABEL);
    h.update([0u8]);
    h.update(provider_fp.as_bytes());
    h.update([0u8]);
    h.update(native_endpoint_id.as_bytes());
    let digest = h.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Display text as it may go on the wire: control characters removed,
/// trimmed, cut to [`MAX_TEXT_BYTES`] on a character boundary. `None` when
/// nothing is left.
pub fn clamp_text(value: &str) -> Option<String> {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    let mut end = trimmed.len().min(MAX_TEXT_BYTES);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let out = trimmed[..end].trim_end();
    (!out.is_empty()).then(|| out.to_string())
}

fn clamp_opt(value: &Option<String>) -> Option<String> {
    value.as_deref().and_then(clamp_text)
}

// -------------------------------------------------------- raw local snapshots

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,
    Output,
}

impl From<DeviceKind> for Direction {
    fn from(kind: DeviceKind) -> Direction {
        match kind {
            DeviceKind::Input => Direction::Input,
            DeviceKind::Output => Direction::Output,
        }
    }
}

/// Outcome of reading one direction's default device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EndpointProbe<T> {
    /// The OS reports no default device in this direction.
    Absent,
    /// The default device could not be read at all.
    Failed { error: String },
    Present { snapshot: T },
}

/// One Windows devnode, as read through cfgmgr32. Local only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WindowsDevnode {
    /// DEVPKEY_Device_InstanceId, e.g. `USB\VID_0B0E&PID_24C8&MI_00\7&...`.
    pub instance_id: String,
    /// DEVPKEY_Device_EnumeratorName (`USB`, `HDAUDIO`, `ROOT`, ...).
    #[serde(default)]
    pub enumerator: Option<String>,
    /// DEVPKEY_Device_HardwareIds. Local classification only (AudioHub's own
    /// devnodes are recognised here); never copied into `DirectionFacts`.
    #[serde(default)]
    pub hardware_ids: Vec<String>,
    /// DEVPKEY_Device_Service.
    #[serde(default)]
    pub service: Option<String>,
    /// DEVPKEY_Device_Manufacturer.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// DEVPKEY_Device_ContainerId, lowercase `{xxxxxxxx-...}`. Grouping only;
    /// never copied into `DirectionFacts`.
    #[serde(default)]
    pub container_id: Option<String>,
}

/// Properties of the Windows default endpoint and of the KS adapter devnode
/// behind it (design §3.3). Local only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsEndpointSnapshot {
    pub direction: Direction,
    /// IMMDevice::GetId. Hashed into the token, never sent.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// PKEY_Device_FriendlyName, e.g. "Speakers (Realtek(R) Audio)".
    #[serde(default)]
    pub friendly_name: Option<String>,
    /// PKEY_Device_DeviceDesc, e.g. "Speakers".
    #[serde(default)]
    pub device_desc: Option<String>,
    /// PKEY_DeviceInterface_FriendlyName, e.g. "Realtek(R) Audio".
    #[serde(default)]
    pub interface_friendly_name: Option<String>,
    /// PKEY_AudioEndpoint_FormFactor (EndpointFormFactor).
    #[serde(default)]
    pub form_factor: Option<u32>,
    /// PKEY_Device_EnumeratorName from the endpoint property store. Recorded
    /// for fixtures only; the gate uses the adapter devnode.
    #[serde(default)]
    pub endpoint_enumerator: Option<String>,
    /// AudioHub's private endpoint identity `{8ca48324-...},2`.
    #[serde(default)]
    pub audiohub_peer_key: Option<String>,
    /// The KS adapter devnode the endpoint's topology connects to.
    #[serde(default)]
    pub adapter: Option<WindowsDevnode>,
    /// Parent first. The probe stops after the first node that terminates the
    /// chain (see [`windows_chain_terminal`]) or on a read failure.
    #[serde(default)]
    pub ancestors: Vec<WindowsDevnode>,
    /// Why `adapter` or `ancestors` is missing or short, for logs.
    #[serde(default)]
    pub error: Option<String>,
}

/// One CoreAudio device. Local only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacDeviceSnapshot {
    pub direction: Direction,
    pub object_id: u32,
    /// kAudioDevicePropertyDeviceUID. Hashed into the token, never sent.
    #[serde(default)]
    pub uid: Option<String>,
    /// kAudioObjectPropertyName.
    #[serde(default)]
    pub name: Option<String>,
    /// kAudioObjectPropertyManufacturer.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// kAudioDevicePropertyTransportType fourcc.
    #[serde(default)]
    pub transport_type: Option<u32>,
    /// kAudioObjectPropertyClass (`adev`, `aagg`, ...).
    #[serde(default)]
    pub class_id: Option<u32>,
    /// kAudioPlugInPropertyBundleID of the HAL plug-in whose
    /// kAudioPlugInPropertyDeviceList lists this device. This is what the
    /// gate trusts (see device_facts_macos.rs for why neither Creator nor
    /// Owner can be used).
    #[serde(default)]
    pub plugin_bundle_id: Option<String>,
    /// True only when no plug-in lists the device AND every plug-in device
    /// list was read. That is the signature of a HAL-created aggregate.
    #[serde(default)]
    pub unlisted_by_plugins: bool,
    /// kAudioObjectPropertyCreator. Diagnostic only: for AudioServerPlugIn
    /// drivers it names the host ("com.apple.audio.CoreAudio"), not the driver,
    /// so it must never decide Apple-vs-third-party.
    #[serde(default)]
    pub creator_bundle_id: Option<String>,
    /// kAudioStreamPropertyTerminalType of the first stream in `direction`.
    #[serde(default)]
    pub terminal_type: Option<u32>,
    /// kAudioDevicePropertyRelatedDevices.
    #[serde(default)]
    pub related_devices: Vec<u32>,
    /// Aggregate devices only: kAudioAggregateDevicePropertyActiveSubDeviceList,
    /// each read recursively.
    #[serde(default)]
    pub sub_devices: Vec<MacDeviceSnapshot>,
    /// False when the sub-device list could not be read completely (read
    /// failure, recursion limit, cycle). Meaningless for non-aggregates.
    #[serde(default)]
    pub sub_devices_complete: bool,
    /// Aggregate devices only: number of process taps
    /// (kAudioAggregateDevicePropertyTapList). A tap means captured system
    /// audio, which is never physical.
    #[serde(default)]
    pub tap_count: u32,
}

/// Everything the probe read on this host. `devices facts --raw` prints it and
/// the classifier fixtures are recorded in this exact shape. Local only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum RawFactsSnapshot {
    Windows {
        input: EndpointProbe<WindowsEndpointSnapshot>,
        output: EndpointProbe<WindowsEndpointSnapshot>,
    },
    Macos {
        input: EndpointProbe<MacDeviceSnapshot>,
        output: EndpointProbe<MacDeviceSnapshot>,
    },
    Unsupported,
}

// ----------------------------------------------------------------- Windows

/// EndpointFormFactor values (mmdeviceapi.h).
mod win_ff {
    pub const REMOTE_NETWORK_DEVICE: u32 = 0;
    pub const SPEAKERS: u32 = 1;
    pub const LINE_LEVEL: u32 = 2;
    pub const HEADPHONES: u32 = 3;
    pub const MICROPHONE: u32 = 4;
    pub const HEADSET: u32 = 5;
    pub const HANDSET: u32 = 6;
    pub const SPDIF: u32 = 8;
    pub const DIGITAL_AUDIO_DISPLAY_DEVICE: u32 = 9;
}

const WIN_ROOT_DEVNODE: &str = r"HTREE\ROOT\0";
const WIN_ACPI_HAL_PREFIX: &str = r"ROOT\ACPI_HAL\";
const WIN_NULL_CONTAINER: &str = "{00000000-0000-0000-0000-000000000000}";

fn upper(value: &Option<String>) -> Option<String> {
    value.as_ref().map(|s| s.trim().to_ascii_uppercase())
}

/// Whether the walk up from an adapter may stop at `node`: a PCI- or
/// ACPI-enumerated node is real platform hardware, and the device tree root
/// has nothing above it. Shared by the probe (when to stop reading) and the
/// classifier (when to stop checking).
pub(crate) fn windows_chain_terminal(node: &WindowsDevnode) -> bool {
    if node.instance_id.trim().eq_ignore_ascii_case(WIN_ROOT_DEVNODE) {
        return true;
    }
    matches!(upper(&node.enumerator).as_deref(), Some("PCI" | "ACPI"))
}

/// AudioHub's own devnodes: the 1.0.x root devnode `ROOT\AudioHubVad` and the
/// planned Phase 2 children in the `AUDIOHUB\` namespace. Identified by
/// hardware ID, instance ID and service name; never by display name.
fn windows_is_audiohub_node(node: &WindowsDevnode) -> bool {
    let instance = node.instance_id.trim().to_ascii_uppercase();
    instance.starts_with(r"AUDIOHUB\")
        || node.hardware_ids.iter().any(|id| {
            let id = id.trim().to_ascii_uppercase();
            id == r"ROOT\AUDIOHUBVAD" || id.starts_with(r"AUDIOHUB\")
        })
        || node
            .service
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("AudioHubVad"))
}

/// Enumerators whose adapters are accepted as physical (design §3.3 step 4).
fn windows_adapter_transport(enumerator: &str, form_factor: Option<u32>) -> Option<PhysicalTransport> {
    match enumerator {
        "HDAUDIO" | "INTELAUDIO" => Some(
            if form_factor == Some(win_ff::DIGITAL_AUDIO_DISPLAY_DEVICE) {
                PhysicalTransport::Hdmi
            } else {
                PhysicalTransport::BuiltIn
            },
        ),
        "USB" => Some(PhysicalTransport::Usb),
        "BTHENUM" | "BTHHFENUM" | "BTHLE" | "BTHLEDEVICE" => Some(PhysicalTransport::Bluetooth),
        "PCI" => Some(PhysicalTransport::Pci),
        "ACPI" => Some(PhysicalTransport::BuiltIn),
        _ => None,
    }
}

fn windows_virtual_enumerator(enumerator: &str) -> bool {
    matches!(enumerator, "ROOT" | "SWD" | "MMDEVAPI" | "VMBUS" | "HTREE")
}

/// Bus enumerators that may sit between a physical adapter and the PCI/ACPI
/// node that ends the walk. Anything else fails closed: a virtual host
/// controller can live in its own namespace, not only under ROOT/SWD.
fn windows_ancestor_allowed(enumerator: &str) -> bool {
    matches!(
        enumerator,
        "USB" | "USB4" | "HDAUDIO" | "INTELAUDIO" | "BTH" | "BTHENUM" | "BTHHFENUM" | "BTHLE"
            | "BTHLEDEVICE"
    )
}

fn windows_form_factor(value: Option<u32>) -> FormFactor {
    match value {
        Some(win_ff::SPEAKERS) => FormFactor::Speakers,
        Some(win_ff::LINE_LEVEL) => FormFactor::LineLevel,
        Some(win_ff::HEADPHONES) => FormFactor::Headphones,
        Some(win_ff::MICROPHONE) => FormFactor::Microphone,
        Some(win_ff::HEADSET) => FormFactor::Headset,
        Some(win_ff::HANDSET) => FormFactor::Handset,
        Some(win_ff::SPDIF) => FormFactor::Spdif,
        Some(win_ff::DIGITAL_AUDIO_DISPLAY_DEVICE) => FormFactor::DigitalDisplay,
        _ => FormFactor::Unknown,
    }
}

/// Class-driver placeholders are not a vendor (design Q10): "(Generic USB
/// Audio)", "(Standard system devices)", "Microsoft".
fn windows_manufacturer(value: &Option<String>) -> Option<String> {
    let text = clamp_opt(value)?;
    let generic = (text.starts_with('(') && text.ends_with(')'))
        || text.eq_ignore_ascii_case("Microsoft")
        || text.eq_ignore_ascii_case("Generic");
    (!generic).then_some(text)
}

/// Pure Windows gate for one direction (design §3.3). `group` is left 0;
/// [`classify_raw`] assigns it across the two directions.
pub fn classify_windows(s: &WindowsEndpointSnapshot, provider_fp: &str) -> DirectionFacts {
    // Step 1: our own endpoint, by its private identity property.
    if s.audiohub_peer_key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
        return DirectionFacts::virtual_(reason::AUDIOHUB_OWN);
    }
    // A network endpoint is by definition not local hardware.
    if s.form_factor == Some(win_ff::REMOTE_NETWORK_DEVICE) {
        return DirectionFacts::virtual_(reason::REMOTE_NETWORK_FORM_FACTOR);
    }
    // Step 2: the KS adapter devnode.
    let Some(adapter) = &s.adapter else {
        return DirectionFacts::virtual_(reason::ADAPTER_UNRESOLVED);
    };
    // Step 3: our own devnodes anywhere on the path, before any whitelist.
    if windows_is_audiohub_node(adapter) || s.ancestors.iter().any(windows_is_audiohub_node) {
        return DirectionFacts::virtual_(reason::AUDIOHUB_OWN);
    }
    // Step 4: adapter enumerator whitelist.
    let Some(enumerator) = upper(&adapter.enumerator).filter(|e| !e.is_empty()) else {
        return DirectionFacts::virtual_(reason::ADAPTER_ENUMERATOR_UNREADABLE);
    };
    let Some(transport) = windows_adapter_transport(&enumerator, s.form_factor) else {
        return DirectionFacts::virtual_(if windows_virtual_enumerator(&enumerator) {
            reason::ADAPTER_ENUMERATOR_VIRTUAL
        } else {
            reason::ADAPTER_ENUMERATOR_UNRECOGNIZED
        });
    };
    // Step 5: ancestor chain up to the first PCI/ACPI node or the tree root.
    let mut terminated = false;
    for node in &s.ancestors {
        if windows_chain_terminal(node) {
            terminated = true;
            break;
        }
        let instance = node.instance_id.trim().to_ascii_uppercase();
        if instance.starts_with(WIN_ACPI_HAL_PREFIX) {
            continue;
        }
        let Some(en) = upper(&node.enumerator).filter(|e| !e.is_empty()) else {
            return DirectionFacts::virtual_(reason::ANCESTOR_UNREADABLE);
        };
        if matches!(en.as_str(), "ROOT" | "SWD" | "VMBUS") {
            return DirectionFacts::virtual_(reason::VIRTUAL_ANCESTOR);
        }
        if !windows_ancestor_allowed(&en) {
            return DirectionFacts::virtual_(reason::ANCESTOR_ENUMERATOR_UNRECOGNIZED);
        }
    }
    if !terminated {
        return DirectionFacts::virtual_(reason::ANCESTOR_CHAIN_INCOMPLETE);
    }
    // Step 6: capture must be a microphone-shaped pin. Excludes "Stereo Mix"
    // style loopback pins on real codecs (and, conservatively, real line-in).
    if s.direction == Direction::Input
        && !matches!(
            s.form_factor,
            Some(win_ff::MICROPHONE | win_ff::HEADSET | win_ff::HANDSET)
        )
    {
        return DirectionFacts::virtual_(reason::CAPTURE_FORM_FACTOR);
    }
    let endpoint_desc = clamp_opt(&s.device_desc);
    let adapter_name = clamp_opt(&s.interface_friendly_name);
    let name = clamp_opt(&s.friendly_name).or_else(|| match (&endpoint_desc, &adapter_name) {
        (Some(d), Some(a)) => clamp_text(&format!("{d} ({a})")),
        (Some(d), None) => Some(d.clone()),
        _ => None,
    });
    let Some(name) = name else {
        return DirectionFacts::virtual_(reason::NAME_UNREADABLE);
    };
    let Some(endpoint_id) = s.endpoint_id.as_deref().filter(|i| !i.trim().is_empty()) else {
        return DirectionFacts::virtual_(reason::IDENTITY_UNREADABLE);
    };
    DirectionFacts::physical(PhysicalDescriptor {
        transport,
        form_factor: windows_form_factor(s.form_factor),
        name,
        endpoint_desc,
        adapter_name,
        manufacturer: windows_manufacturer(&adapter.manufacturer),
        device_token: device_token(provider_fp, endpoint_id),
        group: 0,
    })
}

fn windows_container(s: &EndpointProbe<WindowsEndpointSnapshot>) -> Option<String> {
    let EndpointProbe::Present { snapshot } = s else {
        return None;
    };
    let id = snapshot.adapter.as_ref()?.container_id.as_ref()?;
    let id = id.trim().to_ascii_lowercase();
    (!id.is_empty() && id != WIN_NULL_CONTAINER).then_some(id)
}

/// True when the input and output adapters report the same ContainerId.
/// The computer container (built-in codec) also groups, matching how the
/// local machine itself treats its built-in microphone and speakers (Q6).
fn windows_same_device(
    input: &EndpointProbe<WindowsEndpointSnapshot>,
    output: &EndpointProbe<WindowsEndpointSnapshot>,
) -> bool {
    matches!((windows_container(input), windows_container(output)), (Some(a), Some(b)) if a == b)
}

/// Pure decoding helpers for the Windows probe, kept here (not in the
/// cfg(windows) file) so they are unit-tested on every host.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod win_text {
    fn utf16_from_bytes(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// DEVPROP_TYPE_STRING payload (UTF-16LE, NUL terminated).
    pub(crate) fn string_from_bytes(bytes: &[u8]) -> Option<String> {
        let units = utf16_from_bytes(bytes);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16(&units[..end]).ok().filter(|s| !s.is_empty())
    }

    /// DEVPROP_TYPE_STRING_LIST payload (REG_MULTI_SZ layout).
    pub(crate) fn string_list_from_bytes(bytes: &[u8]) -> Vec<String> {
        utf16_from_bytes(bytes)
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| String::from_utf16(s).ok())
            .collect()
    }

    /// DEVPROP_TYPE_GUID payload -> lowercase `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
    pub(crate) fn guid_from_bytes(bytes: &[u8]) -> Option<String> {
        if bytes.len() != 16 {
            return None;
        }
        let d1 = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let d2 = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
        let d3 = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
        let d4 = &bytes[8..16];
        Some(format!(
            "{{{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
            d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
        ))
    }

    /// `\\?\HDAUDIO#FUNC_01&...#4&...&0001#{guid}\ref` -> `HDAUDIO\FUNC_01&...\4&...&0001`.
    /// The documented symbolic-link encoding; used only when cfgmgr32 cannot
    /// answer for the interface.
    pub(crate) fn instance_id_from_interface_path(path: &str) -> Option<String> {
        let body = path
            .strip_prefix(r"\\?\")
            .or_else(|| path.strip_prefix(r"\??\"))?;
        let end = body.find("#{")?;
        let id = body[..end].replace('#', r"\");
        (!id.is_empty()).then_some(id)
    }

    /// `{1}.HDAUDIO\...` -> `HDAUDIO\...` (the fallback endpoint property).
    pub(crate) fn instance_id_from_numbered_prefix(value: &str) -> Option<String> {
        let rest = value.strip_prefix('{')?;
        let close = rest.find("}.")?;
        if close == 0 || !rest[..close].bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let id = &rest[close + 2..];
        (!id.is_empty()).then(|| id.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn utf16le(s: &str) -> Vec<u8> {
            s.encode_utf16().flat_map(u16::to_le_bytes).collect()
        }

        #[test]
        fn device_facts_win_text_decodes_cfgmgr32_payloads() {
            assert_eq!(string_from_bytes(&utf16le("USB\0")).as_deref(), Some("USB"));
            assert_eq!(string_from_bytes(&utf16le("\0")), None);
            assert_eq!(
                string_list_from_bytes(&utf16le("ROOT\\AudioHubVad\0AUDIOHUB\\X\0\0")),
                vec!["ROOT\\AudioHubVad".to_string(), "AUDIOHUB\\X".to_string()]
            );
            let bytes = [
                0x06, 0xD2, 0x7E, 0x8C, 0x8A, 0x3F, 0x27, 0x48, 0xB3, 0xAB, 0xAE, 0x9E, 0x1F, 0xAE,
                0xFC, 0x6C,
            ];
            assert_eq!(
                guid_from_bytes(&bytes).as_deref(),
                Some("{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c}")
            );
            assert_eq!(guid_from_bytes(&bytes[..15]), None);
        }

        #[test]
        fn device_facts_win_text_decodes_interface_paths_and_fallback_values() {
            assert_eq!(
                instance_id_from_interface_path(
                    r"\\?\HDAUDIO#FUNC_01&VEN_10EC&DEV_0256#4&2E3C9E2A&0&0001#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\eSpeakerWave"
                )
                .as_deref(),
                Some(r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0256\4&2E3C9E2A&0&0001")
            );
            assert_eq!(
                instance_id_from_interface_path(
                    r"\\?\ROOT#MEDIA#0003#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\AhTopoOut-0123456789abcdef"
                )
                .as_deref(),
                Some(r"ROOT\MEDIA\0003")
            );
            assert_eq!(instance_id_from_interface_path("not a path"), None);
            assert_eq!(instance_id_from_interface_path(r"\\?\#{x}"), None);
            assert_eq!(
                instance_id_from_numbered_prefix(r"{1}.USB\VID_0B0E&PID_24C8&MI_00\7&2A1B3C4D&0&0000")
                    .as_deref(),
                Some(r"USB\VID_0B0E&PID_24C8&MI_00\7&2A1B3C4D&0&0000")
            );
            assert_eq!(instance_id_from_numbered_prefix("{x}.USB"), None);
            assert_eq!(instance_id_from_numbered_prefix("{}.USB"), None);
            assert_eq!(instance_id_from_numbered_prefix("{1}."), None);
        }
    }
}

// ------------------------------------------------------------------- macOS

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// kAudioDeviceTransportType* (AudioHardwareBase.h).
mod mac_tt {
    use super::fourcc;
    pub const UNKNOWN: u32 = 0;
    pub const BUILT_IN: u32 = fourcc(b"bltn");
    pub const AGGREGATE: u32 = fourcc(b"grup");
    pub const VIRTUAL: u32 = fourcc(b"virt");
    pub const PCI: u32 = fourcc(b"pci ");
    pub const USB: u32 = fourcc(b"usb ");
    pub const FIREWIRE: u32 = fourcc(b"1394");
    pub const BLUETOOTH: u32 = fourcc(b"blue");
    pub const BLUETOOTH_LE: u32 = fourcc(b"blea");
    pub const HDMI: u32 = fourcc(b"hdmi");
    pub const DISPLAYPORT: u32 = fourcc(b"dprt");
    pub const AIRPLAY: u32 = fourcc(b"airp");
    pub const AVB: u32 = fourcc(b"eavb");
    pub const THUNDERBOLT: u32 = fourcc(b"thun");
    pub const CONTINUITY_WIRED: u32 = fourcc(b"ccwd");
    pub const CONTINUITY_WIRELESS: u32 = fourcc(b"ccwl");
}

/// kAudioStreamTerminalType* plus the USB terminal types (IOAudioTypes.h)
/// some drivers pass through unchanged.
fn mac_form_factor(terminal: Option<u32>) -> FormFactor {
    let Some(t) = terminal else {
        return FormFactor::Unknown;
    };
    if t == fourcc(b"spkr") || t == fourcc(b"lfes") || matches!(t, 0x0301 | 0x0304 | 0x0305) {
        FormFactor::Speakers
    } else if t == fourcc(b"hdph") || t == 0x0302 {
        FormFactor::Headphones
    } else if t == fourcc(b"hmic") || t == 0x0402 {
        FormFactor::Headset
    } else if t == fourcc(b"micr") || matches!(t, 0x0201..=0x0205) {
        FormFactor::Microphone
    } else if t == fourcc(b"rspk") || t == fourcc(b"rmic") || t == 0x0401 {
        FormFactor::Handset
    } else if t == fourcc(b"line") || t == 0x0603 {
        FormFactor::LineLevel
    } else if t == fourcc(b"spdf") || t == 0x0605 {
        FormFactor::Spdif
    } else if t == fourcc(b"hdmi") || t == fourcc(b"dprt") {
        FormFactor::DigitalDisplay
    } else {
        FormFactor::Unknown
    }
}

const MAC_AUDIOHUB_UID_PREFIX: &str = "AudioHub:";
const MAC_AUDIOHUB_BUNDLE_ID: &str = "com.audiohub.driver";
const MAC_APPLE_BUNDLE_PREFIX: &str = "com.apple.";
/// Recursion bound for nested aggregates (design §3.4 step 4).
pub(crate) const MAC_MAX_AGGREGATE_DEPTH: usize = 4;

fn mac_is_audiohub(s: &MacDeviceSnapshot) -> bool {
    s.uid
        .as_deref()
        .is_some_and(|u| u.starts_with(MAC_AUDIOHUB_UID_PREFIX))
        || s
            .plugin_bundle_id
            .as_deref()
            .is_some_and(|b| b.trim().eq_ignore_ascii_case(MAC_AUDIOHUB_BUNDLE_ID))
}

/// kAudioAggregateDeviceClassID.
const MAC_CLASS_AGGREGATE: u32 = fourcc(b"aagg");

/// An aggregate created by the HAL itself: aggregate class and transport, and
/// provably not listed by any plug-in (a third-party driver claiming to be an
/// aggregate is still listed by its own plug-in).
fn mac_is_hal_aggregate(s: &MacDeviceSnapshot) -> bool {
    s.unlisted_by_plugins
        && s.class_id == Some(MAC_CLASS_AGGREGATE)
        && s.transport_type == Some(mac_tt::AGGREGATE)
}

fn mac_contains_audiohub(s: &MacDeviceSnapshot) -> bool {
    mac_is_audiohub(s) || s.sub_devices.iter().any(mac_contains_audiohub)
}

/// The physical transport of one device or aggregate member, or the reason
/// it is not physical. Direction-independent: it is also used on aggregate
/// members, which may each contribute only one direction.
fn mac_physical_transport(s: &MacDeviceSnapshot, depth: usize) -> Result<PhysicalTransport, &'static str> {
    // Step 1 runs before any self-reported property: once this feature ships,
    // our own devices report BuiltIn/USB.
    if mac_is_audiohub(s) {
        return Err(reason::AUDIOHUB_OWN);
    }
    // Step 2: TransportType is self-reported by the plug-in; only Apple's
    // plug-ins are trusted to report it honestly. A device listed by no
    // plug-in is accepted only as the HAL's own aggregate, whose members are
    // then checked one by one.
    match s.plugin_bundle_id.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        Some(bundle) if bundle.starts_with(MAC_APPLE_BUNDLE_PREFIX) => {}
        Some(_) => return Err(reason::OWNER_THIRD_PARTY),
        None if mac_is_hal_aggregate(s) => {}
        None => return Err(reason::OWNER_UNREADABLE),
    }
    let Some(tt) = s.transport_type else {
        return Err(reason::TRANSPORT_UNREADABLE);
    };
    match tt {
        mac_tt::BUILT_IN => Ok(PhysicalTransport::BuiltIn),
        mac_tt::USB => Ok(PhysicalTransport::Usb),
        mac_tt::BLUETOOTH | mac_tt::BLUETOOTH_LE => Ok(PhysicalTransport::Bluetooth),
        mac_tt::FIREWIRE => Ok(PhysicalTransport::FireWire),
        mac_tt::PCI => Ok(PhysicalTransport::Pci),
        mac_tt::THUNDERBOLT => Ok(PhysicalTransport::Thunderbolt),
        mac_tt::HDMI => Ok(PhysicalTransport::Hdmi),
        mac_tt::DISPLAYPORT => Ok(PhysicalTransport::DisplayPort),
        // AVB is networked real hardware; presented as USB (design §3.4).
        mac_tt::AVB => Ok(PhysicalTransport::Usb),
        // Already a network relay, possibly to another AudioHub.
        mac_tt::AIRPLAY => Err(reason::TRANSPORT_AIRPLAY),
        // A real iPhone microphone, but a wireless relay; first release keeps it virtual (Q8).
        mac_tt::CONTINUITY_WIRED | mac_tt::CONTINUITY_WIRELESS => Err(reason::TRANSPORT_CONTINUITY),
        mac_tt::VIRTUAL | mac_tt::UNKNOWN => Err(reason::TRANSPORT_VIRTUAL),
        mac_tt::AGGREGATE => mac_aggregate_transport(s, depth),
        _ => Err(reason::TRANSPORT_UNRECOGNIZED),
    }
}

fn mac_aggregate_transport(s: &MacDeviceSnapshot, depth: usize) -> Result<PhysicalTransport, &'static str> {
    if depth >= MAC_MAX_AGGREGATE_DEPTH {
        return Err(reason::AGGREGATE_TOO_DEEP);
    }
    if s.tap_count > 0 {
        return Err(reason::AGGREGATE_HAS_TAP);
    }
    if !s.sub_devices_complete {
        return Err(reason::AGGREGATE_INCOMPLETE);
    }
    if s.sub_devices.is_empty() {
        return Err(reason::AGGREGATE_EMPTY);
    }
    let mut common: Option<PhysicalTransport> = None;
    let mut mixed = false;
    for sub in &s.sub_devices {
        let t = match mac_physical_transport(sub, depth + 1) {
            Ok(t) => t,
            Err(r) if r == reason::AUDIOHUB_OWN => return Err(reason::AUDIOHUB_OWN),
            Err(_) => return Err(reason::AGGREGATE_MEMBER_NOT_PHYSICAL),
        };
        match common {
            None => common = Some(t),
            Some(c) if c != t => mixed = true,
            Some(_) => {}
        }
    }
    Ok(if mixed {
        PhysicalTransport::Usb
    } else {
        common.unwrap_or(PhysicalTransport::Usb)
    })
}

/// Pure macOS gate for one direction (design §3.4). `group` is left 0;
/// [`classify_raw`] assigns it across the two directions.
pub fn classify_macos(s: &MacDeviceSnapshot, provider_fp: &str) -> DirectionFacts {
    if mac_contains_audiohub(s) {
        return DirectionFacts::virtual_(reason::AUDIOHUB_OWN);
    }
    let transport = match mac_physical_transport(s, 0) {
        Ok(t) => t,
        Err(r) => return DirectionFacts::virtual_(r),
    };
    let Some(name) = clamp_opt(&s.name) else {
        return DirectionFacts::virtual_(reason::NAME_UNREADABLE);
    };
    let Some(uid) = s.uid.as_deref().filter(|u| !u.trim().is_empty()) else {
        return DirectionFacts::virtual_(reason::IDENTITY_UNREADABLE);
    };
    DirectionFacts::physical(PhysicalDescriptor {
        transport,
        form_factor: mac_form_factor(s.terminal_type),
        name,
        endpoint_desc: None,
        adapter_name: None,
        manufacturer: clamp_opt(&s.manufacturer),
        device_token: device_token(provider_fp, uid),
        group: 0,
    })
}

fn mac_same_device(input: &MacDeviceSnapshot, output: &MacDeviceSnapshot) -> bool {
    input.object_id != 0
        && output.object_id != 0
        && (input.object_id == output.object_id
            || input.related_devices.contains(&output.object_id)
            || output.related_devices.contains(&input.object_id))
}

// ------------------------------------------------------------ both platforms

fn classify_probe<T>(
    probe: &EndpointProbe<T>,
    classify: impl Fn(&T) -> DirectionFacts,
) -> DirectionFacts {
    match probe {
        EndpointProbe::Absent => DirectionFacts::absent(),
        EndpointProbe::Failed { .. } => DirectionFacts::virtual_(reason::PROBE_FAILED),
        EndpointProbe::Present { snapshot } => classify(snapshot),
    }
}

/// Group both directions when both are physical and the platform says they
/// are one device. The only group value used today is 1.
fn apply_group(input: &mut DirectionFacts, output: &mut DirectionFacts, same_device: bool) {
    if !same_device {
        return;
    }
    if let (Some(i), Some(o)) = (input.physical.as_mut(), output.physical.as_mut()) {
        i.group = 1;
        o.group = 1;
    }
}

/// Classify a full raw snapshot into the observation a provider publishes
/// (revision 0; the publisher stamps its own). Pure.
pub fn classify_raw(raw: &RawFactsSnapshot, provider_fp: &str) -> DeviceFactsObservation {
    let (input, output) = match raw {
        RawFactsSnapshot::Windows { input, output } => {
            let mut i = classify_probe(input, |s| classify_windows(s, provider_fp));
            let mut o = classify_probe(output, |s| classify_windows(s, provider_fp));
            apply_group(&mut i, &mut o, windows_same_device(input, output));
            (i, o)
        }
        RawFactsSnapshot::Macos { input, output } => {
            let mut i = classify_probe(input, |s| classify_macos(s, provider_fp));
            let mut o = classify_probe(output, |s| classify_macos(s, provider_fp));
            if let (EndpointProbe::Present { snapshot: si }, EndpointProbe::Present { snapshot: so }) =
                (input, output)
            {
                let same = mac_same_device(si, so);
                apply_group(&mut i, &mut o, same);
                // A Bluetooth device serving both directions is a headset (§3.4 step 5).
                if same {
                    if let (Some(pi), Some(po)) = (i.physical.as_mut(), o.physical.as_mut()) {
                        if pi.transport == PhysicalTransport::Bluetooth
                            && po.transport == PhysicalTransport::Bluetooth
                        {
                            pi.form_factor = FormFactor::Headset;
                            po.form_factor = FormFactor::Headset;
                        }
                    }
                }
            }
            (i, o)
        }
        RawFactsSnapshot::Unsupported => (
            DirectionFacts::virtual_(reason::UNSUPPORTED_PLATFORM),
            DirectionFacts::virtual_(reason::UNSUPPORTED_PLATFORM),
        ),
    };
    DeviceFactsObservation {
        revision: 0,
        input,
        output,
    }
}

// ------------------------------------------------------------------- probes

/// Read the raw properties of both current default devices. Property reads
/// only; never opens a stream, never prompts. Local only — see
/// [`RawFactsSnapshot`].
pub fn probe_raw() -> RawFactsSnapshot {
    platform::probe_raw()
}

/// Probe and classify both current default devices (revision 0). With the
/// test-only fake audio backend active, the native platform is never touched
/// and both directions are `Virtual("fake_audio_backend")`.
pub fn probe_default_facts(provider_fp: &str) -> DeviceFactsObservation {
    #[cfg(feature = "fake-audio")]
    if crate::audio::fake::active().is_some() {
        return DeviceFactsObservation {
            revision: 0,
            input: DirectionFacts::virtual_(reason::FAKE_AUDIO_BACKEND),
            output: DirectionFacts::virtual_(reason::FAKE_AUDIO_BACKEND),
        };
    }
    classify_raw(&probe_raw(), provider_fp)
}

#[cfg(windows)]
#[path = "device_facts_windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "device_facts_macos.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub(super) fn probe_raw() -> super::RawFactsSnapshot {
        super::RawFactsSnapshot::Unsupported
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "provider-fingerprint-a";
    const WIN_COMPUTER_CONTAINER: &str = "{00000000-0000-0000-ffff-ffffffffffff}";

    fn node(instance: &str, enumerator: &str) -> WindowsDevnode {
        WindowsDevnode {
            instance_id: instance.into(),
            enumerator: Some(enumerator.into()),
            ..WindowsDevnode::default()
        }
    }

    fn pci_root() -> Vec<WindowsDevnode> {
        vec![node(r"PCI\VEN_8086&DEV_A0ED&SUBSYS_0A1F1028&REV_20\3&11583659&0&A0", "PCI")]
    }

    fn win(direction: Direction, adapter: WindowsDevnode, ancestors: Vec<WindowsDevnode>, ff: u32) -> WindowsEndpointSnapshot {
        WindowsEndpointSnapshot {
            direction,
            endpoint_id: Some("{0.0.0.00000000}.{6a1f3c3e-1111-4c1d-9e5a-3b1c4c5d6e7f}".into()),
            friendly_name: Some("Speakers (Realtek(R) Audio)".into()),
            device_desc: Some("Speakers".into()),
            interface_friendly_name: Some("Realtek(R) Audio".into()),
            form_factor: Some(ff),
            endpoint_enumerator: adapter.enumerator.clone(),
            audiohub_peer_key: None,
            adapter: Some(adapter),
            ancestors,
            error: None,
        }
    }

    fn usb_adapter() -> WindowsDevnode {
        WindowsDevnode {
            instance_id: r"USB\VID_0B0E&PID_24C8&MI_00\7&2A1B3C4D&0&0000".into(),
            enumerator: Some("USB".into()),
            hardware_ids: vec![r"USB\VID_0B0E&PID_24C8&REV_0100&MI_00".into(), r"USB\VID_0B0E&PID_24C8&MI_00".into()],
            service: Some("usbaudio".into()),
            manufacturer: Some("Jabra".into()),
            container_id: Some("{8D3C2A11-5E4F-4A1B-9C2D-1E2F3A4B5C6D}".into()),
        }
    }

    fn usb_chain() -> Vec<WindowsDevnode> {
        let mut chain = vec![
            node(r"USB\VID_0B0E&PID_24C8\SERIAL0123456789", "USB"),
            node(r"USB\ROOT_HUB30\4&1B2C3D4E&0&0", "USB"),
        ];
        chain.extend(pci_root());
        chain
    }

    fn hda_adapter() -> WindowsDevnode {
        WindowsDevnode {
            instance_id: r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0256&SUBSYS_10280A1F&REV_1000\4&2E3C9E2A&0&0001".into(),
            enumerator: Some("HDAUDIO".into()),
            hardware_ids: vec![r"HDAUDIO\FUNC_01&VEN_10EC&DEV_0256&SUBSYS_10280A1F&REV_1000".into()],
            service: Some("IntcAzAudAddService".into()),
            manufacturer: Some("Realtek".into()),
            container_id: Some(WIN_COMPUTER_CONTAINER.into()),
        }
    }

    fn expect_physical(f: &DirectionFacts) -> &PhysicalDescriptor {
        assert_eq!(f.class, DeviceClass::Physical, "{f:?}");
        assert!(f.is_valid(), "{f:?}");
        f.physical_descriptor().expect("physical descriptor")
    }

    fn expect_virtual(f: &DirectionFacts, why: &str) {
        assert_eq!(f.class, DeviceClass::Virtual, "{f:?}");
        assert_eq!(f.reason, why, "{f:?}");
        assert!(f.physical.is_none());
        assert!(f.is_valid());
    }

    // ------------------------------------------------------------- Windows

    #[test]
    fn windows_usb_headset_microphone_is_physical_usb() {
        let mut s = win(Direction::Input, usb_adapter(), usb_chain(), win_ff::HEADSET);
        s.friendly_name = Some("Headset Microphone (Jabra Evolve2 65)".into());
        let f = classify_windows(&s, FP);
        let d = expect_physical(&f);
        assert_eq!(d.transport, PhysicalTransport::Usb);
        assert_eq!(d.form_factor, FormFactor::Headset);
        assert_eq!(d.name, "Headset Microphone (Jabra Evolve2 65)");
        assert_eq!(d.manufacturer.as_deref(), Some("Jabra"));
        assert_eq!(d.device_token, device_token(FP, s.endpoint_id.as_deref().unwrap()));
        assert_eq!(d.group, 0);
    }

    #[test]
    fn windows_hdaudio_speakers_are_physical_built_in() {
        let s = win(Direction::Output, hda_adapter(), pci_root(), win_ff::SPEAKERS);
        let d = expect_physical(&classify_windows(&s, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::BuiltIn);
        assert_eq!(d.form_factor, FormFactor::Speakers);
        assert_eq!(d.endpoint_desc.as_deref(), Some("Speakers"));
        assert_eq!(d.adapter_name.as_deref(), Some("Realtek(R) Audio"));
    }

    #[test]
    fn windows_hdaudio_display_endpoint_is_hdmi() {
        let s = win(Direction::Output, hda_adapter(), pci_root(), win_ff::DIGITAL_AUDIO_DISPLAY_DEVICE);
        let d = expect_physical(&classify_windows(&s, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::Hdmi);
        assert_eq!(d.form_factor, FormFactor::DigitalDisplay);
    }

    #[test]
    fn windows_bluetooth_a2dp_and_hfp_are_physical_bluetooth() {
        let bt_chain = {
            let mut c = vec![
                node(r"BTH\MS_BTHBRB\7&1A2B3C4D&0&1", "BTH"),
                node(r"USB\VID_8087&PID_0026\5&2B3C4D5E&0&10", "USB"),
                node(r"USB\ROOT_HUB30\4&1B2C3D4E&0&0", "USB"),
            ];
            c.extend(pci_root());
            c
        };
        let a2dp = WindowsDevnode {
            instance_id: r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&200E\8&1A2B3C4D&0&A1B2C3D4E5F6_C00000000".into(),
            enumerator: Some("BTHENUM".into()),
            ..WindowsDevnode::default()
        };
        let s = win(Direction::Output, a2dp, bt_chain.clone(), win_ff::HEADPHONES);
        let d = expect_physical(&classify_windows(&s, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::Bluetooth);
        assert_eq!(d.form_factor, FormFactor::Headphones);

        let hfp = WindowsDevnode {
            instance_id: r"BTHHFENUM\BTHHFPAUDIO\9&1A2B3C4D&0&97".into(),
            enumerator: Some("BTHHFENUM".into()),
            ..WindowsDevnode::default()
        };
        let mut hfp_chain = vec![node(r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_LOCALMFG&0002\8&1&0&A1B2C3D4E5F6_C00000000", "BTHENUM")];
        hfp_chain.extend(bt_chain);
        let s = win(Direction::Input, hfp, hfp_chain, win_ff::HEADSET);
        assert_eq!(expect_physical(&classify_windows(&s, FP)).transport, PhysicalTransport::Bluetooth);
    }

    #[test]
    fn windows_root_and_swd_adapters_are_virtual() {
        // VB-Cable style: a ROOT-enumerated PortCls adapter.
        let vb = WindowsDevnode {
            instance_id: r"ROOT\MEDIA\0001".into(),
            enumerator: Some("ROOT".into()),
            hardware_ids: vec![r"VBAudioVACWDM".into()],
            service: Some("VBAudioVACMME".into()),
            manufacturer: Some("VB-Audio Software".into()),
            container_id: Some(WIN_COMPUTER_CONTAINER.into()),
        };
        let mut s = win(Direction::Output, vb, vec![node(WIN_ROOT_DEVNODE, "HTREE")], win_ff::SPEAKERS);
        s.friendly_name = Some("CABLE Input (VB-Audio Virtual Cable)".into());
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_ENUMERATOR_VIRTUAL);

        let swd = node(r"SWD\MMDEVAPI\{0.0.0.00000000}.{11111111-2222-3333-4444-555555555555}", "SWD");
        let s = win(Direction::Output, swd, vec![node(WIN_ROOT_DEVNODE, "HTREE")], win_ff::SPEAKERS);
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_ENUMERATOR_VIRTUAL);
    }

    #[test]
    fn windows_usb_device_under_a_virtual_host_controller_is_virtual() {
        // USB/IP or VirtualHere: the USB tree hangs off a ROOT-enumerated HC.
        let chain = vec![
            node(r"USB\VID_0B0E&PID_24C8\SERIAL0123456789", "USB"),
            node(r"USB\ROOT_HUB30\5&3C4D5E6F&0&0", "USB"),
            node(r"ROOT\USBIP_WIN2\0000", "ROOT"),
            node(WIN_ROOT_DEVNODE, "HTREE"),
        ];
        let s = win(Direction::Input, usb_adapter(), chain, win_ff::MICROPHONE);
        expect_virtual(&classify_windows(&s, FP), reason::VIRTUAL_ANCESTOR);
    }

    #[test]
    fn windows_unrecognized_ancestor_namespace_fails_closed() {
        let chain = vec![
            node(r"VHUSB3HC\ROOT_HUB\0000", "VHUSB3HC"),
            node(WIN_ROOT_DEVNODE, "HTREE"),
        ];
        let s = win(Direction::Output, usb_adapter(), chain, win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::ANCESTOR_ENUMERATOR_UNRECOGNIZED);
    }

    #[test]
    fn windows_incomplete_or_unreadable_chains_fail_closed() {
        let short = vec![node(r"USB\VID_0B0E&PID_24C8\SERIAL0123456789", "USB")];
        let s = win(Direction::Output, usb_adapter(), short, win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::ANCESTOR_CHAIN_INCOMPLETE);

        let s = win(Direction::Output, usb_adapter(), Vec::new(), win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::ANCESTOR_CHAIN_INCOMPLETE);

        let mut unreadable = usb_chain();
        unreadable[0].enumerator = None;
        let s = win(Direction::Output, usb_adapter(), unreadable, win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::ANCESTOR_UNREADABLE);
    }

    #[test]
    fn windows_acpi_hal_is_skipped_on_the_way_to_the_tree_root() {
        let acpi = node(r"ACPI\ESSX8336\1", "ACPI");
        let chain = vec![node(r"ROOT\ACPI_HAL\0000", "ROOT"), node(WIN_ROOT_DEVNODE, "HTREE")];
        let s = win(Direction::Output, acpi, chain, win_ff::SPEAKERS);
        assert_eq!(expect_physical(&classify_windows(&s, FP)).transport, PhysicalTransport::BuiltIn);
    }

    #[test]
    fn windows_stereo_mix_loopback_capture_is_virtual() {
        // A real HDAUDIO codec, but the capture pin is a loopback mix.
        let mut s = win(Direction::Input, hda_adapter(), pci_root(), win_ff::LINE_LEVEL);
        s.friendly_name = Some("Stereo Mix (Realtek(R) Audio)".into());
        expect_virtual(&classify_windows(&s, FP), reason::CAPTURE_FORM_FACTOR);
        s.form_factor = Some(10); // UnknownFormFactor
        expect_virtual(&classify_windows(&s, FP), reason::CAPTURE_FORM_FACTOR);
        s.form_factor = None;
        expect_virtual(&classify_windows(&s, FP), reason::CAPTURE_FORM_FACTOR);
        // The genuine microphone on the same codec stays physical.
        s.form_factor = Some(win_ff::MICROPHONE);
        s.friendly_name = Some("Microphone Array (Realtek(R) Audio)".into());
        expect_physical(&classify_windows(&s, FP));
    }

    #[test]
    fn windows_hyperv_synthetic_audio_is_virtual() {
        let vmbus = node(r"VMBUS\{F8E65716-3CB3-4A06-9A60-1889C5CCCAB5}\{2B3C4D5E-6F70-4A81-9B2C-3D4E5F607182}", "VMBUS");
        let s = win(Direction::Output, vmbus, vec![node(WIN_ROOT_DEVNODE, "HTREE")], win_ff::SPEAKERS);
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_ENUMERATOR_VIRTUAL);
        // A passed-through-looking USB tree behind VMBUS is still virtual.
        let chain = vec![
            node(r"USB\ROOT_HUB30\5&1&0&0", "USB"),
            node(r"VMBUS\{44C4F61D-4444-4400-9D52-802E27EDE19F}\{1}", "VMBUS"),
            node(WIN_ROOT_DEVNODE, "HTREE"),
        ];
        let s = win(Direction::Output, usb_adapter(), chain, win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::VIRTUAL_ANCESTOR);
    }

    #[test]
    fn windows_audiohub_own_endpoint_is_virtual_by_identity_not_name() {
        // Private peer key wins even if every other property looks physical.
        let mut s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADPHONES);
        s.audiohub_peer_key = Some("v1:0123456789abcdef:out".into());
        expect_virtual(&classify_windows(&s, FP), reason::AUDIOHUB_OWN);

        // Key missing, but the adapter is the AudioHub root devnode.
        let ah = WindowsDevnode {
            instance_id: r"ROOT\MEDIA\0003".into(),
            enumerator: Some("ROOT".into()),
            hardware_ids: vec![r"ROOT\AudioHubVad".into()],
            service: Some("AudioHubVad".into()),
            manufacturer: Some("AudioHub".into()),
            container_id: Some(WIN_COMPUTER_CONTAINER.into()),
        };
        let mut s = win(Direction::Output, ah.clone(), vec![node(WIN_ROOT_DEVNODE, "HTREE")], win_ff::HEADPHONES);
        s.friendly_name = Some("Headphones (Jabra Evolve2 65 – WIN-30)".into());
        expect_virtual(&classify_windows(&s, FP), reason::AUDIOHUB_OWN);

        // Phase 2 child devnode in the AUDIOHUB namespace, under the root devnode.
        let child = WindowsDevnode {
            instance_id: r"AUDIOHUB\PEERENDPOINT\0123456789ABCDEF-V".into(),
            enumerator: Some("USB".into()), // even a lying enumerator does not help
            hardware_ids: vec![r"AUDIOHUB\PeerEndpoint".into()],
            ..WindowsDevnode::default()
        };
        let s = win(Direction::Output, child, vec![ah, node(WIN_ROOT_DEVNODE, "HTREE")], win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::AUDIOHUB_OWN);
    }

    #[test]
    fn windows_missing_reads_fail_closed() {
        let mut s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADPHONES);
        s.adapter = None;
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_UNRESOLVED);

        let mut s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADPHONES);
        s.adapter.as_mut().unwrap().enumerator = None;
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_ENUMERATOR_UNREADABLE);

        let mut odd = usb_adapter();
        odd.enumerator = Some("DISPLAY".into());
        let s = win(Direction::Output, odd, usb_chain(), win_ff::HEADPHONES);
        expect_virtual(&classify_windows(&s, FP), reason::ADAPTER_ENUMERATOR_UNRECOGNIZED);

        let mut s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADPHONES);
        s.endpoint_id = None;
        expect_virtual(&classify_windows(&s, FP), reason::IDENTITY_UNREADABLE);

        let mut s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADPHONES);
        s.friendly_name = None;
        s.device_desc = None;
        expect_virtual(&classify_windows(&s, FP), reason::NAME_UNREADABLE);

        let s = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::REMOTE_NETWORK_DEVICE);
        expect_virtual(&classify_windows(&s, FP), reason::REMOTE_NETWORK_FORM_FACTOR);
    }

    #[test]
    fn windows_name_falls_back_to_the_composite_and_generic_vendors_are_dropped() {
        let mut adapter = usb_adapter();
        adapter.manufacturer = Some("(Generic USB Audio)".into());
        let mut s = win(Direction::Output, adapter, usb_chain(), win_ff::HEADPHONES);
        s.friendly_name = None;
        s.device_desc = Some("Headphones".into());
        s.interface_friendly_name = Some("USB Audio Device".into());
        let d = expect_physical(&classify_windows(&s, FP)).clone();
        assert_eq!(d.name, "Headphones (USB Audio Device)");
        assert_eq!(d.manufacturer, None);
    }

    #[test]
    fn windows_same_container_groups_both_directions() {
        let input = win(Direction::Input, usb_adapter(), usb_chain(), win_ff::HEADSET);
        let output = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADSET);
        let raw = RawFactsSnapshot::Windows {
            input: EndpointProbe::Present { snapshot: input.clone() },
            output: EndpointProbe::Present { snapshot: output.clone() },
        };
        let obs = classify_raw(&raw, FP);
        assert_eq!(expect_physical(&obs.input).group, 1);
        assert_eq!(expect_physical(&obs.output).group, 1);

        // A different container: no group.
        let mut other = output;
        other.adapter.as_mut().unwrap().container_id = Some("{11111111-2222-3333-4444-555555555555}".into());
        let raw = RawFactsSnapshot::Windows {
            input: EndpointProbe::Present { snapshot: input },
            output: EndpointProbe::Present { snapshot: other },
        };
        let obs = classify_raw(&raw, FP);
        assert_eq!(expect_physical(&obs.input).group, 0);
        assert_eq!(expect_physical(&obs.output).group, 0);
    }

    #[test]
    fn windows_absent_and_failed_probes_are_not_physical() {
        let raw = RawFactsSnapshot::Windows {
            input: EndpointProbe::Absent,
            output: EndpointProbe::Failed { error: "CoCreateInstance failed".into() },
        };
        let obs = classify_raw(&raw, FP);
        assert_eq!(obs.input.class, DeviceClass::Absent);
        expect_virtual(&obs.output, reason::PROBE_FAILED);
        assert!(obs.is_valid());
    }

    /// Fixture in the exact `devices facts --raw` shape, inline JSON.
    #[test]
    fn windows_recorded_fixture_shape_classifies() {
        let fixture = r#"{
          "platform": "windows",
          "input": {"state": "absent"},
          "output": {"state": "present", "snapshot": {
            "direction": "output",
            "endpoint_id": "{0.0.0.00000000}.{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
            "friendly_name": "Speakers (Realtek(R) Audio)",
            "device_desc": "Speakers",
            "interface_friendly_name": "Realtek(R) Audio",
            "form_factor": 1,
            "endpoint_enumerator": "HDAUDIO",
            "adapter": {
              "instance_id": "HDAUDIO\\FUNC_01&VEN_10EC&DEV_0256&SUBSYS_10280A1F&REV_1000\\4&2E3C9E2A&0&0001",
              "enumerator": "HDAUDIO",
              "hardware_ids": ["HDAUDIO\\FUNC_01&VEN_10EC&DEV_0256&SUBSYS_10280A1F&REV_1000"],
              "manufacturer": "Realtek",
              "container_id": "{00000000-0000-0000-ffff-ffffffffffff}"
            },
            "ancestors": [
              {"instance_id": "PCI\\VEN_8086&DEV_A0C8&SUBSYS_0A1F1028&REV_20\\3&11583659&0&FB", "enumerator": "PCI"}
            ]
          }}
        }"#;
        let raw: RawFactsSnapshot = serde_json::from_str(fixture).unwrap();
        let obs = classify_raw(&raw, FP);
        assert_eq!(obs.input.class, DeviceClass::Absent);
        assert_eq!(expect_physical(&obs.output).transport, PhysicalTransport::BuiltIn);
    }

    /// Vendor identity never reaches the wire record: no hardware ID, instance
    /// ID, container GUID, endpoint ID or service name in the serialized facts.
    #[test]
    fn transmitted_facts_carry_no_vendor_identity() {
        let input = win(Direction::Input, usb_adapter(), usb_chain(), win_ff::HEADSET);
        let output = win(Direction::Output, usb_adapter(), usb_chain(), win_ff::HEADSET);
        let raw = RawFactsSnapshot::Windows {
            input: EndpointProbe::Present { snapshot: input.clone() },
            output: EndpointProbe::Present { snapshot: output },
        };
        let json = serde_json::to_string(&classify_raw(&raw, FP)).unwrap();
        for secret in [
            "VID_", "PID_", "SERIAL", "8D3C2A11", "usbaudio", "6a1f3c3e", "ROOT_HUB", "PCI\\", "{0.0.0",
        ] {
            assert!(!json.to_ascii_uppercase().contains(&secret.to_ascii_uppercase()), "{secret} leaked: {json}");
        }

        let mac = mac_dev(Direction::Output, 42, "AppleUSBAudioEngine:Jabra:Evolve2 65:A1B2C3D4E5:2", mac_tt::USB);
        let raw = RawFactsSnapshot::Macos {
            input: EndpointProbe::Absent,
            output: EndpointProbe::Present { snapshot: mac },
        };
        let json = serde_json::to_string(&classify_raw(&raw, FP)).unwrap();
        assert!(!json.contains("A1B2C3D4E5"), "{json}");
        assert!(!json.contains("AppleUSBAudioEngine"), "{json}");
        assert!(!json.contains("com.apple"), "{json}");
    }

    // --------------------------------------------------------------- macOS

    fn mac_dev(direction: Direction, id: u32, uid: &str, tt: u32) -> MacDeviceSnapshot {
        MacDeviceSnapshot {
            direction,
            object_id: id,
            uid: Some(uid.into()),
            name: Some("Device".into()),
            manufacturer: Some("Apple Inc.".into()),
            transport_type: Some(tt),
            class_id: Some(fourcc(b"adev")),
            plugin_bundle_id: Some("com.apple.audio.IOAudio2".into()),
            unlisted_by_plugins: false,
            creator_bundle_id: Some("com.apple.audio.CoreAudio".into()),
            terminal_type: None,
            related_devices: Vec::new(),
            sub_devices: Vec::new(),
            sub_devices_complete: false,
            tap_count: 0,
        }
    }

    fn aggregate(members: Vec<MacDeviceSnapshot>) -> MacDeviceSnapshot {
        // Shape measured on the test VM: class 'aagg', listed by no plug-in.
        MacDeviceSnapshot {
            name: Some("Studio Aggregate".into()),
            class_id: Some(MAC_CLASS_AGGREGATE),
            plugin_bundle_id: None,
            unlisted_by_plugins: true,
            sub_devices: members,
            sub_devices_complete: true,
            ..mac_dev(Direction::Output, 90, "~:AMS2_Aggregate:0", mac_tt::AGGREGATE)
        }
    }

    #[test]
    fn mac_built_in_usb_and_bluetooth_are_physical() {
        let mut builtin = mac_dev(Direction::Output, 50, "BuiltInSpeakerDevice", mac_tt::BUILT_IN);
        builtin.name = Some("MacBook Pro Speakers".into());
        builtin.terminal_type = Some(fourcc(b"spkr"));
        let d = expect_physical(&classify_macos(&builtin, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::BuiltIn);
        assert_eq!(d.form_factor, FormFactor::Speakers);
        assert_eq!(d.name, "MacBook Pro Speakers");
        assert_eq!(d.manufacturer.as_deref(), Some("Apple Inc."));
        assert_eq!(d.device_token, device_token(FP, "BuiltInSpeakerDevice"));

        let mut usb = mac_dev(Direction::Input, 51, "AppleUSBAudioEngine:Jabra:Evolve2:1:1", mac_tt::USB);
        usb.plugin_bundle_id = Some("com.apple.audio.IOUSBAudio".into());
        usb.terminal_type = Some(0x0402);
        let d = expect_physical(&classify_macos(&usb, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::Usb);
        assert_eq!(d.form_factor, FormFactor::Headset);

        for tt in [mac_tt::BLUETOOTH, mac_tt::BLUETOOTH_LE] {
            let mut bt = mac_dev(Direction::Output, 52, "A1-B2-C3-D4-E5-F6:output", tt);
            bt.terminal_type = Some(fourcc(b"hdph"));
            let d = expect_physical(&classify_macos(&bt, FP)).clone();
            assert_eq!(d.transport, PhysicalTransport::Bluetooth);
            assert_eq!(d.form_factor, FormFactor::Headphones);
        }

        for (tt, want) in [
            (mac_tt::HDMI, PhysicalTransport::Hdmi),
            (mac_tt::DISPLAYPORT, PhysicalTransport::DisplayPort),
            (mac_tt::THUNDERBOLT, PhysicalTransport::Thunderbolt),
            (mac_tt::PCI, PhysicalTransport::Pci),
            (mac_tt::FIREWIRE, PhysicalTransport::FireWire),
            (mac_tt::AVB, PhysicalTransport::Usb),
        ] {
            let s = mac_dev(Direction::Output, 53, "x", tt);
            assert_eq!(expect_physical(&classify_macos(&s, FP)).transport, want);
        }
    }

    #[test]
    fn mac_virtual_airplay_continuity_and_unknown_are_virtual() {
        let blackhole = MacDeviceSnapshot {
            plugin_bundle_id: Some("audio.existential.BlackHole2ch".into()),
            ..mac_dev(Direction::Output, 60, "BlackHole2ch_UID", mac_tt::VIRTUAL)
        };
        expect_virtual(&classify_macos(&blackhole, FP), reason::OWNER_THIRD_PARTY);
        let apple_virtual = mac_dev(Direction::Output, 61, "x", mac_tt::VIRTUAL);
        expect_virtual(&classify_macos(&apple_virtual, FP), reason::TRANSPORT_VIRTUAL);
        let unknown = mac_dev(Direction::Output, 62, "x", mac_tt::UNKNOWN);
        expect_virtual(&classify_macos(&unknown, FP), reason::TRANSPORT_VIRTUAL);
        let airplay = MacDeviceSnapshot {
            plugin_bundle_id: Some("com.apple.audio.AirPlay".into()),
            ..mac_dev(Direction::Output, 63, "AirPlay:1A2B3C", mac_tt::AIRPLAY)
        };
        expect_virtual(&classify_macos(&airplay, FP), reason::TRANSPORT_AIRPLAY);
        for tt in [mac_tt::CONTINUITY_WIRED, mac_tt::CONTINUITY_WIRELESS] {
            let iphone = mac_dev(Direction::Input, 64, "iPhoneMic", tt);
            expect_virtual(&classify_macos(&iphone, FP), reason::TRANSPORT_CONTINUITY);
        }
        let odd = mac_dev(Direction::Output, 65, "x", fourcc(b"zzzz"));
        expect_virtual(&classify_macos(&odd, FP), reason::TRANSPORT_UNRECOGNIZED);
    }

    #[test]
    fn mac_third_party_plugin_claiming_built_in_is_virtual() {
        let liar = MacDeviceSnapshot {
            plugin_bundle_id: Some("com.example.FakeSpeakers".into()),
            name: Some("MacBook Pro Speakers".into()),
            ..mac_dev(Direction::Output, 70, "FakeBuiltIn", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&liar, FP), reason::OWNER_THIRD_PARTY);
        let unknown_owner = MacDeviceSnapshot {
            plugin_bundle_id: None,
            ..mac_dev(Direction::Output, 71, "x", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&unknown_owner, FP), reason::OWNER_UNREADABLE);
        let no_transport = MacDeviceSnapshot {
            transport_type: None,
            ..mac_dev(Direction::Output, 72, "x", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&no_transport, FP), reason::TRANSPORT_UNREADABLE);

        // Measured on the VM: BlackHole's Creator names the CoreAudio host.
        // Only the listing plug-in decides, so a lying driver stays virtual.
        let creator_says_apple = MacDeviceSnapshot {
            plugin_bundle_id: Some("audio.existential.BlackHole2ch".into()),
            creator_bundle_id: Some("com.apple.audio.CoreAudio".into()),
            ..mac_dev(Direction::Output, 73, "BlackHole2ch_UID", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&creator_says_apple, FP), reason::OWNER_THIRD_PARTY);

        // A third-party "aggregate": listed by its own plug-in.
        let fake_aggregate = MacDeviceSnapshot {
            plugin_bundle_id: Some("com.example.FakeAggregate".into()),
            class_id: Some(MAC_CLASS_AGGREGATE),
            sub_devices: vec![mac_dev(Direction::Output, 74, "bltn", mac_tt::BUILT_IN)],
            sub_devices_complete: true,
            ..mac_dev(Direction::Output, 75, "fake-agg", mac_tt::AGGREGATE)
        };
        expect_virtual(&classify_macos(&fake_aggregate, FP), reason::OWNER_THIRD_PARTY);

        // Unlisted, but the plug-in lists were not all readable / not an aggregate.
        let mut unprovable = MacDeviceSnapshot {
            plugin_bundle_id: None,
            unlisted_by_plugins: false,
            class_id: Some(MAC_CLASS_AGGREGATE),
            sub_devices: vec![mac_dev(Direction::Output, 76, "bltn", mac_tt::BUILT_IN)],
            sub_devices_complete: true,
            ..mac_dev(Direction::Output, 77, "agg", mac_tt::AGGREGATE)
        };
        expect_virtual(&classify_macos(&unprovable, FP), reason::OWNER_UNREADABLE);
        unprovable.unlisted_by_plugins = true;
        unprovable.class_id = Some(fourcc(b"adev"));
        expect_virtual(&classify_macos(&unprovable, FP), reason::OWNER_UNREADABLE);
        let unlisted_device = MacDeviceSnapshot {
            plugin_bundle_id: None,
            unlisted_by_plugins: true,
            ..mac_dev(Direction::Output, 78, "x", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&unlisted_device, FP), reason::OWNER_UNREADABLE);
    }

    #[test]
    fn mac_audiohub_own_device_is_virtual_even_when_it_reports_built_in() {
        // After this feature our own devices may report BuiltIn/USB.
        let by_uid = MacDeviceSnapshot {
            plugin_bundle_id: Some("com.apple.audio.CoreAudio".into()),
            ..mac_dev(Direction::Output, 80, "AudioHub:0123456789abcdef:out", mac_tt::BUILT_IN)
        };
        expect_virtual(&classify_macos(&by_uid, FP), reason::AUDIOHUB_OWN);
        let by_bundle = MacDeviceSnapshot {
            plugin_bundle_id: Some("com.audiohub.driver".into()),
            ..mac_dev(Direction::Output, 81, "SomethingElse", mac_tt::USB)
        };
        expect_virtual(&classify_macos(&by_bundle, FP), reason::AUDIOHUB_OWN);
    }

    #[test]
    fn mac_aggregate_of_physical_members_is_physical() {
        let mut usb = mac_dev(Direction::Input, 11, "usb-in", mac_tt::USB);
        usb.plugin_bundle_id = Some("com.apple.audio.IOUSBAudio".into());
        let usb2 = mac_dev(Direction::Output, 12, "usb-out", mac_tt::USB);
        let agg = aggregate(vec![usb.clone(), usb2]);
        let d = expect_physical(&classify_macos(&agg, FP)).clone();
        assert_eq!(d.transport, PhysicalTransport::Usb);
        assert_eq!(d.name, "Studio Aggregate");

        // Mixed transports present as USB; same transports keep theirs.
        let builtin = mac_dev(Direction::Output, 13, "bltn", mac_tt::BUILT_IN);
        let hdmi = mac_dev(Direction::Output, 14, "hdmi", mac_tt::HDMI);
        assert_eq!(
            expect_physical(&classify_macos(&aggregate(vec![builtin.clone(), hdmi]), FP)).transport,
            PhysicalTransport::Usb
        );
        assert_eq!(
            expect_physical(&classify_macos(&aggregate(vec![builtin.clone(), builtin.clone()]), FP)).transport,
            PhysicalTransport::BuiltIn
        );
        // Nested aggregate within the depth bound.
        let nested = aggregate(vec![aggregate(vec![builtin.clone()]), usb]);
        expect_physical(&classify_macos(&nested, FP));
    }

    #[test]
    fn mac_aggregate_with_any_doubtful_member_is_virtual() {
        let builtin = mac_dev(Direction::Output, 13, "bltn", mac_tt::BUILT_IN);
        let blackhole = MacDeviceSnapshot {
            plugin_bundle_id: Some("audio.existential.BlackHole2ch".into()),
            ..mac_dev(Direction::Output, 15, "BlackHole2ch_UID", mac_tt::VIRTUAL)
        };
        let agg = aggregate(vec![builtin.clone(), blackhole]);
        expect_virtual(&classify_macos(&agg, FP), reason::AGGREGATE_MEMBER_NOT_PHYSICAL);

        let ours = mac_dev(Direction::Output, 16, "AudioHub:0123456789abcdef:out", mac_tt::VIRTUAL);
        expect_virtual(&classify_macos(&aggregate(vec![builtin.clone(), ours]), FP), reason::AUDIOHUB_OWN);

        let airplay = mac_dev(Direction::Output, 17, "AirPlay:1", mac_tt::AIRPLAY);
        expect_virtual(&classify_macos(&aggregate(vec![builtin.clone(), airplay]), FP), reason::AGGREGATE_MEMBER_NOT_PHYSICAL);

        expect_virtual(&classify_macos(&aggregate(Vec::new()), FP), reason::AGGREGATE_EMPTY);

        let mut incomplete = aggregate(vec![builtin.clone()]);
        incomplete.sub_devices_complete = false;
        expect_virtual(&classify_macos(&incomplete, FP), reason::AGGREGATE_INCOMPLETE);

        // A process-tap aggregate carries captured system audio.
        let mut tapped = aggregate(vec![builtin.clone()]);
        tapped.tap_count = 1;
        expect_virtual(&classify_macos(&tapped, FP), reason::AGGREGATE_HAS_TAP);

        let mut deep = builtin;
        for _ in 0..=MAC_MAX_AGGREGATE_DEPTH {
            deep = aggregate(vec![deep]);
        }
        expect_virtual(&classify_macos(&deep, FP), reason::AGGREGATE_MEMBER_NOT_PHYSICAL);
    }

    #[test]
    fn mac_bluetooth_serving_both_directions_is_one_grouped_headset() {
        let mut out = mac_dev(Direction::Output, 30, "AA-BB:output", mac_tt::BLUETOOTH);
        out.terminal_type = Some(fourcc(b"hdph"));
        let mut inp = mac_dev(Direction::Input, 30, "AA-BB:output", mac_tt::BLUETOOTH);
        inp.terminal_type = Some(fourcc(b"micr"));
        let raw = RawFactsSnapshot::Macos {
            input: EndpointProbe::Present { snapshot: inp },
            output: EndpointProbe::Present { snapshot: out },
        };
        let obs = classify_raw(&raw, FP);
        for f in [&obs.input, &obs.output] {
            let d = expect_physical(f);
            assert_eq!(d.group, 1);
            assert_eq!(d.form_factor, FormFactor::Headset);
        }

        // Built-in mic + speakers are related devices: grouped, form factors kept.
        let mut spk = mac_dev(Direction::Output, 40, "BuiltInSpeakerDevice", mac_tt::BUILT_IN);
        spk.terminal_type = Some(fourcc(b"spkr"));
        spk.related_devices = vec![40, 41];
        let mut mic = mac_dev(Direction::Input, 41, "BuiltInMicrophoneDevice", mac_tt::BUILT_IN);
        mic.terminal_type = Some(fourcc(b"micr"));
        let raw = RawFactsSnapshot::Macos {
            input: EndpointProbe::Present { snapshot: mic.clone() },
            output: EndpointProbe::Present { snapshot: spk },
        };
        let obs = classify_raw(&raw, FP);
        assert_eq!(expect_physical(&obs.input).group, 1);
        assert_eq!(expect_physical(&obs.input).form_factor, FormFactor::Microphone);
        assert_eq!(expect_physical(&obs.output).form_factor, FormFactor::Speakers);

        // Unrelated devices are not grouped.
        let usb = mac_dev(Direction::Output, 42, "usb", mac_tt::USB);
        let raw = RawFactsSnapshot::Macos {
            input: EndpointProbe::Present { snapshot: mic },
            output: EndpointProbe::Present { snapshot: usb },
        };
        let obs = classify_raw(&raw, FP);
        assert_eq!(expect_physical(&obs.input).group, 0);
        assert_eq!(expect_physical(&obs.output).group, 0);
    }

    #[test]
    fn mac_recorded_fixture_shape_classifies() {
        let fixture = r#"{
          "platform": "macos",
          "input": {"state": "failed", "error": "kAudioHardwarePropertyDefaultInputDevice: 2003332927"},
          "output": {"state": "present", "snapshot": {
            "direction": "output", "object_id": 73, "uid": "BuiltInSpeakerDevice",
            "name": "MacBook Pro Speakers", "manufacturer": "Apple Inc.",
            "transport_type": 1651274862, "plugin_bundle_id": "com.apple.audio.CoreAudio",
            "terminal_type": 1936747378
          }}
        }"#;
        let raw: RawFactsSnapshot = serde_json::from_str(fixture).unwrap();
        let obs = classify_raw(&raw, FP);
        expect_virtual(&obs.input, reason::PROBE_FAILED);
        let d = expect_physical(&obs.output);
        assert_eq!(d.transport, PhysicalTransport::BuiltIn);
        assert_eq!(d.form_factor, FormFactor::Speakers);
    }

    // ---------------------------------------------------- wire model rules

    #[test]
    fn token_is_stable_opaque_and_provider_scoped() {
        let a = device_token("fp-a", "BuiltInSpeakerDevice");
        assert_eq!(a, device_token("fp-a", "BuiltInSpeakerDevice"));
        assert_eq!(a.len(), 16);
        assert!(valid_token(&a));
        assert_ne!(a, device_token("fp-b", "BuiltInSpeakerDevice"));
        assert_ne!(a, device_token("fp-a", "BuiltInMicrophoneDevice"));
        // The separator keeps the (fp, id) boundary unambiguous.
        assert_ne!(device_token("ab", "c"), device_token("a", "bc"));
    }

    fn sample_descriptor() -> PhysicalDescriptor {
        PhysicalDescriptor {
            transport: PhysicalTransport::Usb,
            form_factor: FormFactor::Headset,
            name: "Jabra Evolve2 65".into(),
            endpoint_desc: None,
            adapter_name: None,
            manufacturer: Some("Jabra".into()),
            device_token: "0123456789abcdef".into(),
            group: 1,
        }
    }

    #[test]
    fn validation_enforces_the_physical_iff_descriptor_rule_and_text_bounds() {
        assert!(DirectionFacts::physical(sample_descriptor()).is_valid());
        let mut no_desc = DirectionFacts::physical(sample_descriptor());
        no_desc.physical = None;
        assert!(!no_desc.is_valid());
        let mut virtual_with_desc = DirectionFacts::virtual_("x");
        virtual_with_desc.physical = Some(sample_descriptor());
        assert!(!virtual_with_desc.is_valid());

        for bad in [
            PhysicalDescriptor { device_token: "0123456789ABCDEF".into(), ..sample_descriptor() },
            PhysicalDescriptor { device_token: "0123".into(), ..sample_descriptor() },
            PhysicalDescriptor { name: String::new(), ..sample_descriptor() },
            PhysicalDescriptor { name: "a".repeat(128), ..sample_descriptor() },
            PhysicalDescriptor { name: "bad\nname".into(), ..sample_descriptor() },
            PhysicalDescriptor { manufacturer: Some(String::new()), ..sample_descriptor() },
            PhysicalDescriptor { adapter_name: Some("x\u{7}".into()), ..sample_descriptor() },
        ] {
            assert!(!DirectionFacts::physical(bad.clone()).is_valid(), "{bad:?}");
            assert!(DirectionFacts::physical(bad).physical_descriptor().is_none());
        }
        let mut bad_reason = DirectionFacts::virtual_("x");
        bad_reason.reason = "Not Snake".into();
        assert!(!bad_reason.is_valid());
        bad_reason.reason = String::new();
        assert!(!bad_reason.is_valid());
    }

    #[test]
    fn sanitized_downgrades_only_the_invalid_direction() {
        let mut broken = DirectionFacts::physical(sample_descriptor());
        broken.physical = None;
        let obs = DeviceFactsObservation {
            revision: 9,
            input: broken,
            output: DirectionFacts::physical(sample_descriptor()),
        };
        assert!(!obs.is_valid());
        let fixed = obs.sanitized();
        assert_eq!(fixed.revision, 9);
        expect_virtual(&fixed.input, reason::INVALID_FACTS);
        assert!(fixed.output.physical_descriptor().is_some());
    }

    #[test]
    fn clamp_text_trims_controls_and_cuts_on_a_char_boundary() {
        assert_eq!(clamp_text("  Speakers\u{0}\n "), Some("Speakers".into()));
        assert_eq!(clamp_text("\t\n"), None);
        let long = "é".repeat(100); // 200 bytes
        let cut = clamp_text(&long).unwrap();
        assert!(cut.len() <= MAX_TEXT_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn wire_shape_round_trips_and_defaults_optional_fields() {
        let obs = DeviceFactsObservation {
            revision: 3,
            input: DirectionFacts::virtual_(reason::CAPTURE_FORM_FACTOR),
            output: DirectionFacts::physical(sample_descriptor()),
        };
        let json = serde_json::to_value(&obs).unwrap();
        assert_eq!(json["input"]["class"], "virtual");
        assert_eq!(json["output"]["class"], "physical");
        assert_eq!(json["output"]["physical"]["transport"], "usb");
        assert_eq!(json["output"]["physical"]["form_factor"], "headset");
        assert_eq!(json["output"]["origin"], "probe");
        let back: DeviceFactsObservation = serde_json::from_value(json).unwrap();
        assert_eq!(back, obs);

        // Optional fields may be omitted by a sender.
        let minimal = serde_json::json!({
            "revision": 1,
            "input": {"class": "absent", "reason": "no_default_device"},
            "output": {"class": "physical", "reason": "physical", "physical": {
                "transport": "built_in", "form_factor": "speakers",
                "name": "Speakers", "device_token": "fedcba9876543210"
            }}
        });
        let parsed: DeviceFactsObservation = serde_json::from_value(minimal).unwrap();
        assert_eq!(parsed.output.origin, FactsOrigin::Probe);
        let d = parsed.output.physical_descriptor().unwrap();
        assert_eq!((d.group, d.manufacturer.as_deref()), (0, None));
        assert!(parsed.is_valid());

        // Default observation is "not probed", never physical.
        let d = DeviceFactsObservation::default();
        assert_eq!(d.input.class, DeviceClass::Unknown);
        assert!(d.is_valid());
    }

    #[test]
    fn unsupported_platform_is_virtual() {
        let obs = classify_raw(&RawFactsSnapshot::Unsupported, FP);
        expect_virtual(&obs.input, reason::UNSUPPORTED_PLATFORM);
        expect_virtual(&obs.output, reason::UNSUPPORTED_PLATFORM);
    }

    /// Reads the real default devices of the host (property reads only, no
    /// stream, no prompt). Run explicitly in a test VM:
    /// `cargo test -p audiohub-core device_facts -- --ignored`.
    #[test]
    #[ignore = "explicit native property probe in an isolated test VM"]
    fn device_facts_native_probe_in_test_vm() {
        let raw = probe_raw();
        println!("raw: {}", serde_json::to_string_pretty(&raw).unwrap());
        let obs = classify_raw(&raw, FP);
        println!("facts: {}", serde_json::to_string_pretty(&obs).unwrap());
        assert!(obs.is_valid());
        #[cfg(target_os = "macos")]
        for dev in platform::probe_all_devices_for_test() {
            let f = classify_macos(&dev, FP);
            println!(
                "device {} {:?} uid={:?} tt={:?} plugin={:?} unlisted={} creator={:?} -> {:?}/{}",
                dev.object_id,
                dev.name,
                dev.uid,
                dev.transport_type.map(|t| String::from_utf8_lossy(&t.to_be_bytes()).into_owned()),
                dev.plugin_bundle_id,
                dev.unlisted_by_plugins,
                dev.creator_bundle_id,
                f.class,
                f.reason
            );
        }
    }
}
