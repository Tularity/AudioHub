/*++

Module Name:

    perpeer.h

Abstract:

    Per-peer virtual audio endpoints: one pair of KS filters (speaker + mic)
    for every paired AudioHub peer, installed and removed AT RUNTIME.

    v11 (1.0.2 Phase 2): the endpoints no longer live on the root devnode. The
    root ROOT\AudioHubVad is a bus (bus.cpp) and every peer's directions are
    hosted on child devnodes AUDIOHUB\PeerEndpoint, one per PRESENTATION UNIT
    (see AH_UNIT_* below), each a PortCls adapter of its own. This file keeps
    the slot table, decides which unit hosts which direction, and installs a
    direction on its host child the moment both exist -- inside the Set if the
    child is already running, otherwise when the child starts.

    This is the sysvad Bluetooth-HFP / USB-sideband shape, not SwDeviceCreate:
    SwDeviceCreate requires Administrator ("The calling process must have
    Administrator access in order to initiate the enumeration of a software
    device"), and audiohubd is deliberately an ordinary user process.

--*/

#ifndef _AUDIOHUB_PERPEER_H_
#define _AUDIOHUB_PERPEER_H_

#include "common.h"
#include "AudioHubIoctl.h"
#include "speakerformatbank.h"

//
// Template reference strings. These must match the KSNAME_* values in the
// INF's [Strings] section, where they exist ONLY as templates: the INF
// registers each of them once so that
// MigrateDeviceInterfaceTemplateParameters has a registry key to copy the
// second-level (EP\0, FX\0) parameters out of. A template interface is never
// enabled and never becomes an endpoint.
//
// The interface actually published for a peer is "<template>-<fingerprint>"
// while that direction presents VIRTUAL (identical to 1.0.1, so a virtual
// peer keeps its endpoint id across the upgrade), and
// "<template>-<fingerprint>-p<key8>" while it presents PHYSICAL (v10). The
// key is part of the identity because Windows caches an endpoint's name, form
// factor and icon per endpoint id: a different look needs a different id, and
// changing back to an earlier look returns the earlier id together with the
// default-device and per-application settings the system kept for it.
//
#define AH_TEMPLATE_TOPO_OUT    L"AhTopoOut"
#define AH_TEMPLATE_WAVE_OUT    L"AhWaveOut"
#define AH_TEMPLATE_TOPO_IN     L"AhTopoIn"
#define AH_TEMPLATE_WAVE_IN     L"AhWaveIn"

//
// "AhTopoOut-" (10) + 16 hex + "-p" + 8 hex + NUL = 37. 64 leaves room and
// keeps the slot record's arithmetic obvious.
//
#define AH_REFSTRING_MAX        64

//
// PER-PEER ENDPOINT NAMES
// =======================
//
// Windows composes what the user reads as "<DeviceDesc> (<devnode FriendlyName>)".
// Two of the three obvious ways to supply the left half do NOT work, and both
// were MEASURED here rather than reasoned about, because each looks correct in
// the documentation:
//
//   1. DEVPKEY_DeviceInterface_FriendlyName on the KS interface. The value
//      lands in the registry verbatim and no endpoint name uses it. (The
//      bracketed half comes from the DEVNODE's FriendlyName. Until v10 there
//      was exactly one devnode for all peers; since v11 every presentation
//      unit is its own child devnode with its own FriendlyName, and the bus
//      sets it at the child's AddDevice -- spike S6.) v11 still writes the
//      interface property, as a copy of the host devnode's name, so the two
//      never disagree for a reader that looks at the interface.
//
//   2. The pin name, KsPinDescriptor.Name -> MediaCategories\<GUID>\Name.
//      This one works -- FOR CAPTURE ONLY. Step 3 of the audio endpoint
//      builder's algorithm "sets the default properties for the endpoint ...
//      the name, icon, and the form factor", and for a bridge pin whose
//      category is KSNODETYPE_SPEAKER that name is hardcoded:
//
//          "in the case of speaker endpoints, the name has been hardcoded to
//           'Speakers' and cannot be altered by your driver or a third-party
//           application"
//          -- learn.microsoft.com/.../audio/audio-endpoint-builder-algorithm
//
//      Measured on a FRESH peer key with the registry perturbed: renaming the
//      render pin's Name GUID entry changed nothing, and renaming the
//      MediaCategories entry for KSNODETYPE_SPEAKER ITSELF also changed
//      nothing -- the speaker's name is not a registry lookup at all. The same
//      perturbation renamed the capture endpoint both times.
//
//      This is why the defect outlived a full acceptance run: the localized
//      string the hardcode produces is BYTE-IDENTICAL to the string our own
//      INF installs for the render pin, so "the speaker endpoint is named
//      <speaker>" was true whether or not anything we wrote was ever read.
//
// What DOES work, in both directions, is step 5 of the same algorithm:
// "populates the endpoint PropertyStore with property information from the
// registry keys of the audio device interface". Step 5 runs AFTER step 3, so a
// PKEY_Device_DeviceDesc value under the interface's EP\0 key replaces the
// hardcoded name. Measured: with the value in place before the interface is
// enabled, BOTH endpoints came up under it.
//
// So the driver writes, per peer and per direction, into the TOPOLOGY
// interface's own key:
//
//     ...\<AhTopoOut|AhTopoIn>-<fingerprint>\Device Parameters\EP\0
//         "{a45c254e-df1c-4efd-8020-67d146a850e0},2" = <daemon peer label>
//
// and the endpoint reads
//
//     <EP\0 label> (<child devnode FriendlyName>)
//
// v11: the child devnode's FriendlyName is per presentation unit and
// daemon-composed (AH_PRESENTATION.adapter_name, e.g. "Realtek(R) Audio –
// WIN-30" for a physical unit, "AudioHub – WIN-30" for the virtual one), so
// the bracketed half carries the peer and two peers never share it (S6:
// identical devnode names make Windows prefix the endpoints "N- "). A
// physical presentation writes the daemon-composed endpoint half, e.g.
// "Headphones", and moves the bridge pin off the default category (see
// AH_PRESENTATION in AudioHubIoctl.h). EP\0 still overrides the builder's
// name for every category used, measured in spike S2. The child INF
// DeviceDesc, "AudioHub Network Audio", is only the fallback.
//
// The bracketed half of a virtual unit is byte-identical to the macOS device
// name that plan 7.1 froze ("AudioHub – <peer>").
//
// WHY THIS IS THE SYMMETRIC FIX, and the pin name was not:
//
//   * one mechanism, one code path, one call per direction. Neither direction
//     has a branch, a substitution or a fallback that the other lacks;
//   * the pin CATEGORY is untouched, so KSNODETYPE_SPEAKER keeps supplying the
//     form factor, the icon and the default-device rank. The alternative fix
//     -- moving the render bridge pin off KSNODETYPE_SPEAKER to escape the
//     hardcode -- would have been a change made to one direction only, to work
//     around one direction's problem: exactly the small-print asymmetry that
//     produced this bug in the first place;
//   * it stops writing the machine-wide MediaCategories key altogether, which
//     Microsoft documents as "reserved for global definitions and should not
//     be modified by new drivers ... will not be supported in a future OS
//     release".
//
// KNOWN LIMIT, shared with every other mechanism here: the composed name is
// CACHED per endpoint id in the MMDevices property store. Rewriting EP\0 for
// an endpoint id that already exists does not rename it. That matches the
// deliberate "no renaming of live endpoints" rule (disabling an interface to
// force a refresh puts the endpoint into DEVICE_STATE_NOTPRESENT and moves the
// user's default-device choice), and it is why a naming test MUST use a peer
// key it has never used before.
//
// The direction words are NOT part of a successfully named endpoint. Both the
// output and input endpoint use the same daemon-composed peer label;
// Windows already separates them by data flow. The words are still read from
// the INF so its static pin names remain observable as the generic fallback if
// the per-peer PKEY_Device_DeviceDesc write fails.
//
#define AH_DIRWORD_CHARS        32

//
// The endpoint-name buffers keep the historical extra room even though the
// successful name is now exactly Display. They are internal driver storage,
// not wire ABI; retaining their size keeps this naming-only change isolated.
//
#define AH_ENDPOINT_NAME_CHARS       (AH_DISPLAY_CHARS + AH_DIRWORD_CHARS + 2)

//
// Per-slot, per-direction volume storage width covers the largest speaker
// layout. The microphone remains stereo at KS level with a mono bridge ring.
//
#define AH_VOLUME_MAX_CHANNELS  12

//
// What every miniport of a slot receives as its `DeviceContext`. See the note
// on AhEpContextDecode below for why the routing goes through here rather than
// through CONTAINING_RECORD on the minipair.
//
//
// Unity gain in the KS volume unit (1/65536 dB), i.e. 0 dB. Spelled through
// the sample's own constant so the two never drift apart.
//
#define AH_VOLUME_UNITY     VOLUME_SIGNED_MAXIMUM

#define AH_EP_CONTEXT_MAGIC 0x41484550u  // 'AHEP'

typedef struct _AH_EP_CONTEXT
{
    ULONG   Magic;      // AH_EP_CONTEXT_MAGIC
    ULONG   Slot;
    BOOLEAN Input;      // TRUE = the virtual MICROPHONE
} AH_EP_CONTEXT, *PAH_EP_CONTEXT;

//
// A slot's four name buffers, its display name, its two ENDPOINT_MINIPAIRs and
// its property arrays all live INSIDE this record, and the record lives in a
// static array for the lifetime of the driver image.
//
// This is deliberately unlike sysvad, which heap-allocates a custom minipair
// per endpoint and has to free it in exactly the right order after
// RemoveEndpointFilters. PcRegisterSubdevice's contract is that the Name
// buffer "must remain valid for the lifetime of the device object", so a
// premature free is a use-after-free whose symptom is a bugcheck ten peers
// later. Static storage makes that failure mode unrepresentable.
//
//
// v10: one direction's installed presentation. Klass == AH_PRES_VIRTUAL means
// today's look; FormFactor, Key and Desc are zero/empty then.
//
typedef struct _AH_SLOT_PRESENTATION
{
    ULONG   Klass;                      // AH_PRES_*
    ULONG   FormFactor;                 // AH_FF_* when physical, else 0
    ULONG   Group;                      // v11: physical grouping, 0 when virtual
    CHAR    Key[AH_PRESKEY_BUF];        // NUL-terminated [0-9a-f]{8} when physical
    WCHAR   Desc[AH_DISPLAY_CHARS];     // EP\0 name; v11: may be set when virtual
    WCHAR   AdapterName[AH_DISPLAY_CHARS]; // v11: child devnode FriendlyName
} AH_SLOT_PRESENTATION, *PAH_SLOT_PRESENTATION;

//
// v11 PRESENTATION UNITS. A unit is one child devnode, i.e. one "device" in
// Settings and one Chromium groupId. A peer has at most two:
//
//   AH_UNIT_V   every VIRTUAL direction of the peer (both, usually)
//   AH_UNIT_R   a physical render direction on a device of its own
//   AH_UNIT_C   a physical capture direction on a device of its own
//   AH_UNIT_RC  physical render AND capture whose AH_PRESENTATION.group is the
//               same non-zero value (a headset is one device)
//
// The unit code is part of the child's instance id ("<fp>-v", "<fp>-r", ...),
// so a direction keeps its devnode across presentation-key changes while the
// unit kind stays the same; the presentation key itself still lives only in
// the interface reference strings (v10) and in the ContainerId input.
//
#define AH_UNIT_NONE    0u
#define AH_UNIT_V       1u
#define AH_UNIT_R       2u
#define AH_UNIT_C       3u
#define AH_UNIT_RC      4u

//
// "<16 hex>-rc" is 19 characters; the buffer leaves room and keeps the
// arithmetic obvious.
//
#define AH_INSTANCE_ID_CHARS    32u

typedef struct _AH_UNIT_SPEC
{
    ULONG   Slot;
    ULONG   Unit;                       // AH_UNIT_*
    ULONG   Directions;                 // AH_PUB_* this unit hosts
    CHAR    PeerKey[AH_PEERKEY_BUF];
    CHAR    KeyR[AH_PRESKEY_BUF];       // physical render key, "" otherwise
    CHAR    KeyC[AH_PRESKEY_BUF];       // physical capture key, "" otherwise
    WCHAR   InstanceId[AH_INSTANCE_ID_CHARS];
    WCHAR   FriendlyName[AH_DISPLAY_CHARS];
    GUID    ContainerId;                // UUIDv5(NS, "<fp>-<unitkey>"), bus.cpp
} AH_UNIT_SPEC, *PAH_UNIT_SPEC;

typedef struct _AH_SLOT
{
    ULONG               State;          // AH_SLOT_FREE | AH_SLOT_BOUND
    ULONG               Generation;     // bumped on every successful SET
    ULONG               Flags;          // AH_BINDFLAG_* as last received

    CHAR                PeerKey[AH_PEERKEY_BUF];        // NUL-terminated hex

    // Reference strings. Never reused across peers: they carry the FINGERPRINT,
    // so a recycled slot cannot make a new peer inherit the previous peer's
    // endpoint id (and with it the user's default-device choice, per-app
    // assignment, endpoint volume and any rename).
    WCHAR               TopoNameOut[AH_REFSTRING_MAX];
    WCHAR               WaveNameOut[AH_REFSTRING_MAX];
    WCHAR               TopoNameIn[AH_REFSTRING_MAX];
    WCHAR               WaveNameIn[AH_REFSTRING_MAX];

    // The peer label exactly as the daemon composed it, prefix included and
    // direction suffix excluded.
    WCHAR               Display[AH_DISPLAY_CHARS];

    // Two copies of the same per-peer endpoint label.
    // Each goes into its direction's own EP\0 as PKEY_Device_DeviceDesc.
    WCHAR               NameOut[AH_ENDPOINT_NAME_CHARS];
    WCHAR               NameIn[AH_ENDPOINT_NAME_CHARS];

    // v11: per direction, TRUE once this slot may have written the name and
    // identity values into the EP\0 store of the CURRENT reference strings on
    // the host child (index AH_PRES_RENDER / AH_PRES_CAPTURE). Raised before
    // the first write and cleared when the values are taken back.
    BOOLEAN             NamesWritten[2];

    // v11: per direction, the topology interface FriendlyName (the bracketed
    // half of the endpoint name): the host child's devnode FriendlyName as it
    // was when the direction was installed. The topology property buffer
    // points here; PortCls reads it at PcRegisterSubdevice.
    WCHAR               IfName[2][AH_DISPLAY_CHARS];

    // TRUE when this slot's endpoints are published under the INF's generic
    // fallback labels instead of the peer's. Reported to the daemon as
    // AH_BINDREPLY_FLAG_NAME_FALLBACK.
    BOOLEAN             NameFallback;

    //
    // PER-SLOT VOLUME AND MUTE, one set per direction.
    //
    // These exist because the sample's storage does NOT work here. Upstream
    // keeps volume in CSimpleAudioSample::m_VolumeControls[MAX_TOPOLOGY_NODES]
    // (hw.h), one array for the whole ADAPTER, indexed by the topology NODE id.
    // Every slot's volume node is node 0, and there is one adapter, so all
    // sixteen peers and both directions shared a single cell: moving the
    // speaker slider on peer A moved it on peer B, and neither device list nor
    // log said anything.
    //
    // Units are the KS unit throughout -- 1/65536 dB, with
    // VOLUME_SIGNED_MAXIMUM/-VOLUME_SIGNED_MAXIMUM style extremes -- because
    // that is what KSPROPERTY_AUDIO_VOLUMELEVEL carries and converting on
    // storage would mean converting back on every read.
    //
    LONG                VolumeOut[AH_VOLUME_MAX_CHANNELS];
    LONG                VolumeIn[AH_VOLUME_MAX_CHANNELS];
    BOOLEAN             MuteOut[AH_VOLUME_MAX_CHANNELS];
    BOOLEAN             MuteIn[AH_VOLUME_MAX_CHANNELS];

    //
    // DOWNSTREAM LATENCY, per direction, in frames: how long after this driver
    // accepts a frame it is audible at the far end. Written by
    // IOCTL_AUDIOHUB_LATENCY, read ONCE by each stream when it is created.
    //
    // Zero is the cold start and means "never measured", which is a different
    // claim from "instantaneous" -- it is only what can honestly be said before
    // a measurement exists. Reset to zero on every bind: a new peer is a
    // different machine at the end of a different network, so the previous
    // tenant's distance describes nothing here.
    //
    LONG                LatencyFramesOut;
    LONG                LatencyFramesIn;

    // Handed to every miniport of this slot as its DeviceContext. By value, so
    // the address is stable for the life of the driver image -- PortCls holds
    // it for as long as the miniport lives.
    AH_EP_CONTEXT       OutCtx;
    AH_EP_CONTEXT       InCtx;

    // v10: what each direction presents, [AH_PRES_RENDER], [AH_PRES_CAPTURE].
    // Stored when the direction's reference strings are built, so it always
    // describes the identity the CURRENT reference strings carry, published
    // or not.
    AH_SLOT_PRESENTATION Pres[2];

    // NOTE: there are deliberately NO per-slot filter descriptors here. v3 kept
    // a private copy of each TOPOLOGY filter and its pin array so the bridge
    // pin's KsPinDescriptor.Name could point at a per-peer GUID. v10 needs a
    // different bridge pin CATEGORY per presentation, and gets it from a small
    // set of driver-global copies, one per category, built once at DriverEntry
    // and never written again (perpeer.cpp g_AhTopoVariants). A slot only
    // selects one, so no PortCls-held pointer ever targets storage that a
    // rebind rewrites -- the lifetime hazard the v3 copies had.

    SIMPLEAUDIOSAMPLE_DEVPROPERTY   OutTopoProps[1];
    SIMPLEAUDIOSAMPLE_DEVPROPERTY   InTopoProps[1];

    // The selected render bank is driver-global and immutable after
    // DriverEntry. The slot owns only this selection; no PortCls pointer ever
    // targets storage that can be rewritten when the slot is rebound.
    const AH_SPEAKER_FORMAT_BANK *OutFormatBank;

    ENDPOINT_MINIPAIR   OutPair;
    ENDPOINT_MINIPAIR   InPair;

    // Held between Install and Remove; NULL when not installed.
    PUNKNOWN            OutTopo;
    PUNKNOWN            OutWave;
    PUNKNOWN            OutWaveMiniport;
    PUNKNOWN            InTopo;
    PUNKNOWN            InWave;

    // v11: which unit each WANTED direction belongs to (AH_UNIT_NONE when the
    // direction is not wanted), and the child devnode that hosts it while its
    // ports are held. HostPdo/HostAdapter are valid exactly while the host
    // child's adapter is registered (perpeer.cpp g_AhHosts), which is also
    // when the port pointers above may be non-NULL.
    ULONG               UnitOf[2];
    PDEVICE_OBJECT      HostPdo[2];
    PADAPTERCOMMON      HostAdapter[2];

    // v11: the units UnitOf[] names, render's first, exactly as handed to the
    // bus (instance id, ContainerId, FriendlyName included).
    AH_UNIT_SPEC        Units[2];
    ULONG               UnitCount;

    // v11: the last install failure of a wanted direction, kept so a Set that
    // follows an asynchronous (child start) failure can report it.
    ULONG               LastStage;
    NTSTATUS            LastNtStatus;
} AH_SLOT, *PAH_SLOT;

//
// Called from DriverEntry, before anything else can touch the table.
//
VOID AhPerPeerDriverInit(VOID);

//
// v11 CHILD ADAPTERS. Called from a child devnode's StartDevice once its
// adapter exists, and from PnpHandler's REMOVE / SURPRISE_REMOVAL /
// STOP_DEVICE branches for a CHILD FDO, BEFORE the adapter is Cleanup()'d and
// Released. Start installs every waiting direction this child hosts; detach
// removes every direction installed on it (leaving a slot holding port
// pointers past the adapter's death is failure mode B3). Neither changes a
// slot's binding: the daemon's pairing state is not the devnode's state.
//
// Start never fails the child: a child with no working endpoint still starts,
// because the bus, not the install, decides how long the devnode lives.
//
NTSTATUS AhPerPeerChildStart(_In_ PDEVICE_OBJECT Fdo, _In_ PADAPTERCOMMON Adapter);
VOID     AhPerPeerChildDetach(_In_ PDEVICE_OBJECT Fdo);

//
// Where an operation failed and what it left behind. Every bind path fills one
// of these, and the IOCTL layer copies it straight into AH_BIND_REPLY.
//
// `Published` is the DRIVER'S OWN account of which halves it holds port
// pointers for -- not an intention, not a request echo. It is the field that
// makes "the driver said OK but only the microphone appeared" a detectable
// state instead of something a human has to notice in the system's device
// list.
//
typedef struct _AH_OP_RESULT
{
    ULONG    Stage;         // AH_STAGE_*
    NTSTATUS NtStatus;      // raw kernel status for Stage; 0 when NONE
    ULONG    Published;     // AH_PUB_* after the call
    ULONG    Flags;         // AH_BINDREPLY_FLAG_* -- degradations that do not
                            // make the call a failure but must not be silent
} AH_OP_RESULT, *PAH_OP_RESULT;

//
// Bind operations. Both are idempotent in the way plan §7.3 needs:
//
//  * SET on a slot already bound to the SAME peer key and exact requested mask
//    returns the current generation and does nothing else -- no
//    re-registration, no rewriting of the persistent FriendlyName property.
//    The daemon re-Sets a slot whenever the peer's online flag changes, and
//    under "paired means published, disconnect keeps the device" that would
//    otherwise be pure registry churn.
//
//    A differing mask is reconciled per direction. The surviving endpoint is
//    not cycled, so Windows keeps the user's default-device selection.
//
//  * SET succeeds only when `Published` exactly equals the requested render /
//    capture mask. One direction by itself is a valid capability, not a partial
//    failure. A fresh-bind install failure still rolls back the call's work.
//
//  * CLEAR on a free slot succeeds. CLEAR quoting a stale generation is
//    IGNORED, so a Clear delayed past a re-bind cannot cut down the binding
//    that replaced it.
//
//
// Presentation must be the request's two entries, already terminated, and
// already forced virtual by the caller when the caller check does not allow a
// physical look (ctldevice.cpp). Malformed entries are refused here with
// AH_STATUS_BAD_ARGUMENT / AH_STAGE_PRESENTATION before anything changes.
//
NTSTATUS AhSlotBindSet(
    _In_  ULONG   Slot,
    _In_z_ PCSTR  PeerKey,
    _In_  PCWSTR  Display,
    _In_reads_(2) const AH_PRESENTATION *Presentation,
    _In_  ULONG   Flags,
    _Out_ PULONG  Generation,
    _Out_ PULONG  State,
    _Out_ PULONG  AhStatus,
    _Out_ PAH_OP_RESULT Result);

NTSTATUS AhSlotBindClear(
    _In_  ULONG   Slot,
    _In_  ULONG   Generation,
    _In_  ULONG   Flags,
    _Out_ PULONG  State,
    _Out_ PULONG  AhStatus,
    _Out_ PAH_OP_RESULT Result);

VOID AhSlotQuery(_Out_ AH_QUERY_SLOTS_REPLY *Reply, _In_ ULONGLONG SessionId);

// Per-speaker format state is private kernel memory, never ring-header data.
// All control operations are PASSIVE_LEVEL. The DPC holds the short per-slot
// gate only while copying one bounded block; QUIESCED is also a reader fence.
VOID AhSpeakerFormatDetach(_In_ ULONGLONG SessionId = 0);
ULONG AhSpeakerFormatControl(_In_ const AH_FORMAT_PAYLOAD *Message);
NTSTATUS AhSpeakerFormatRequest(_In_ ULONG Slot, _In_ ULONG Channels);
_IRQL_requires_max_(DISPATCH_LEVEL)
const AH_SPEAKER_FORMAT_BANK *AhSpeakerFormatSnapshot(_In_ ULONG Slot, _Out_opt_ PULONG Layout);
_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSpeakerRenderEnter(_In_ ULONG Slot, _In_ ULONG Channels, _Out_ PULONG Epoch, _Out_ PKIRQL Irql);
_IRQL_requires_max_(DISPATCH_LEVEL)
VOID AhSpeakerRenderLeave(_In_ ULONG Slot, _In_ KIRQL Irql);

//
// Validators, exported so the IOCTL layer and the tests use ONE implementation.
//
BOOLEAN AhIsValidPeerKey(_In_reads_(Length) const CHAR *Key, _In_ SIZE_T Length);

//
// DATA-PLANE ROUTING.
//
// Every miniport this driver creates -- wave and topology, render and capture
// -- is handed the address of the AH_EP_CONTEXT belonging to its slot and
// direction, through the `DeviceContext` parameter that PortCls already
// threads from InstallEndpointFilters all the way to the miniport constructor
// (common.h:434 -> common.cpp:2228 -> PFNCREATEMINIPORT). Upstream passes NULL
// there and every miniport UNREFERENCED_PARAMETERs it, so the channel is free.
//
// This carries slot AND direction explicitly, which the alternatives do not:
//   * CONTAINING_RECORD off ENDPOINT_MINIPAIR requires the caller to already
//     know whether it is holding OutPair or InPair, and guessing wrong yields
//     a plausible slot index that is off by the distance between the two
//     members -- a wrong-peer bug with no symptom but wrong audio;
//   * the topology miniports never see the minipair at all, so that route
//     could not have served the volume node in any case.
//
// The record lives inside AH_SLOT, i.e. in a static array for the lifetime of
// the driver image, so the pointer cannot dangle.
//
//
// Decodes a DeviceContext. Returns FALSE (and leaves the outputs untouched)
// for NULL or for anything whose magic does not match -- so a miniport created
// by some future path that forgets to pass one degrades to "no data plane"
// instead of indexing the slot table with whatever the pointer happened to
// point at.
//
// Callable at DISPATCH_LEVEL: two loads, no locks.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhEpContextDecode(_In_opt_ const void *DeviceContext, _Out_ PULONG Slot, _Out_ PBOOLEAN Input);

//
// Per-slot, per-direction volume, in the KS unit (1/65536 dB, and
// AH_VOLUME_SILENCE for muted-by-attenuation). Replaces the adapter-wide
// hw.cpp array, which is indexed by NODE id and therefore had ONE cell shared
// by every peer: moving one peer's slider moved all of them.
//
// Read and written from the topology property handler (PASSIVE) and from the
// NOTIFY IOCTL (PASSIVE). Interlocked because those are different threads, not
// because anything here is a hot path.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
LONG AhSlotVolumeGet(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG Channel);

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSlotVolumeSet(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG Channel, _In_ LONG Value);

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSlotMuteGet(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG Channel);

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSlotMuteSet(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG Channel, _In_ BOOLEAN Value);

//
// Per-slot, per-direction downstream latency in FRAMES (see AH_SLOT).
//
// Get is called from CMiniportWaveRTStream's constructor, i.e. once per stream,
// and never again for that stream's life: presentation position is a clock, and
// an offset that moved underneath a running clock could make it appear to go
// backwards. Set comes from IOCTL_AUDIOHUB_LATENCY at PASSIVE. Interlocked
// because those are different threads.
//
// Set REFUSES anything above AH_LATENCY_MAX_FRAMES and returns FALSE, so
// "the driver stored it" is answerable rather than assumed.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG AhSlotLatencyGet(_In_ ULONG Slot, _In_ BOOLEAN Input);

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSlotLatencySet(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG Frames);

//
// The slot's current generation, or 0 when it is free. Events carry it so a
// late report about a slot's PREVIOUS tenant cannot be applied to the next.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG AhSlotGeneration(_In_ ULONG Slot);

//
// The ONE mapping between the KS volume unit (signed 1/65536 dB) and the 0..1
// amplitude scalar the daemon and every peer speak. Integer-only: reachable
// from the property handler, and kernel code must not use the FPU at raised
// IRQL.
//
// The table behind these is an approximation of 20*log10 and IS EXPECTED TO BE
// CALIBRATED AGAINST THE REAL SYSTEM rather than trusted. See the comment on
// g_AhDbTable in perpeer.cpp.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
LONG  AhScalarQ16ToKsVolume(_In_ ULONG ScalarQ16);

_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG AhKsVolumeToScalarQ16(_In_ LONG Level);

//
// Registry of live topology miniports, so a volume change arriving over the
// control plane can raise KSEVENT_CONTROL_CHANGE on the right endpoint.
//
// PVOID rather than the class type because perpeer.cpp must not depend on the
// topology class's layout; basetopo.cpp casts it back.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
VOID  AhTopoRegister(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_opt_ PVOID Topology);

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID  AhTopoUnregister(_In_ PVOID Topology);

_IRQL_requires_max_(DISPATCH_LEVEL)
PVOID AhTopoLookup(_In_ ULONG Slot, _In_ BOOLEAN Input);

//
// Implemented in basetopo.cpp (it is the only place that knows how to raise
// one). A no-op when the endpoint has no live topology miniport, which is the
// ordinary state for a paired-but-never-opened device.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
VOID  AhTopoRaiseVolumeEvent(_In_ ULONG Slot, _In_ BOOLEAN Input);

//
// WaveRT buffer residency -- the second of the two buffering stages, reported
// SEPARATELY from the AudioHub ring per spec-windows-driver.md:574. Publish is
// called from the stream's DISPATCH_LEVEL position update; Sample is called
// from IOCTL_AUDIOHUB_STREAMSTAT at PASSIVE. Sample returns FALSE when no
// stream holds that slot+direction, which is NOT the same as zero residency.
//
VOID    AhWaveRtPublish(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ ULONG BufferBytes,
                        _In_ ULONG ResidentBytes, _In_ ULONG FrameBytes,
                        _In_ ULONG SampleRate, _In_ ULONG PacketBytes);
VOID    AhWaveRtClear(_In_ ULONG Slot, _In_ BOOLEAN Input);
BOOLEAN AhWaveRtSample(_In_ ULONG Slot, _In_ BOOLEAN Input, _Out_ ULONG *BufferBytes,
                       _Out_ ULONG *ResidentBytes, _Out_ ULONG *FrameBytes,
                       _Out_ ULONG *SampleRate, _Out_ ULONG *PacketBytes,
                       _Out_ ULONG *Updates);

#endif // _AUDIOHUB_PERPEER_H_
