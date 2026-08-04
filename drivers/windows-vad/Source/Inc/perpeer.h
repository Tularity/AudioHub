/*++

Module Name:

    perpeer.h

Abstract:

    Per-peer virtual audio endpoints: one pair of KS filters (speaker + mic)
    for every paired AudioHub peer, installed and removed AT RUNTIME inside a
    single root devnode.

    This is the sysvad Bluetooth-HFP / USB-sideband shape, not SwDeviceCreate:
    SwDeviceCreate requires Administrator ("The calling process must have
    Administrator access in order to initiate the enumeration of a software
    device"), and audiohubd is deliberately an ordinary user process.

--*/

#ifndef _AUDIOHUB_PERPEER_H_
#define _AUDIOHUB_PERPEER_H_

#include "common.h"
#include "AudioHubIoctl.h"

//
// Template reference strings. These must match the KSNAME_* values in the
// INF's [Strings] section, where they exist ONLY as templates: the INF
// registers each of them once so that
// MigrateDeviceInterfaceTemplateParameters has a registry key to copy the
// second-level (EP\0, FX\0) parameters out of. A template interface is never
// enabled and never becomes an endpoint.
//
// The interface actually published for a peer is "<template>-<fingerprint>".
//
#define AH_TEMPLATE_TOPO_OUT    L"AhTopoOut"
#define AH_TEMPLATE_WAVE_OUT    L"AhWaveOut"
#define AH_TEMPLATE_TOPO_IN     L"AhTopoIn"
#define AH_TEMPLATE_WAVE_IN     L"AhWaveIn"

//
// "AhTopoOut-" (10) + 16 hex + NUL = 27. 64 leaves room and keeps the slot
// record's arithmetic obvious.
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
//      bracketed half comes from the DEVNODE's FriendlyName, of which there is
//      exactly one for all peers, so that half can never carry a peer name.)
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
//         "{a45c254e-df1c-4efd-8020-67d146a850e0},2" = "AudioHub - <host> <dir>"
//
// and the endpoint reads
//
//     AudioHub - WIN-IR01HVEFU7G <speaker> (AudioHub Virtual Audio)
//
// whose leading run is byte-identical to the macOS name that plan 7.1 froze.
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
// The direction word is NOT hardcoded here: it is READ BACK from the INF's
// static MediaCategories entries (AH_PIN_NAME_OUT / AH_PIN_NAME_IN) at attach
// time, so the localizable strings stay in the INF's [Strings] section, which
// is the only place that can ever grow a [Strings.0409]. Those entries keep
// earning their place as the pin-name fallback; only the PER-PEER writes are
// gone.
//
#define AH_DIRWORD_CHARS        32

//
// AH_DISPLAY_CHARS (128, incl. terminator) + ' ' + a direction word + NUL.
// The peer's half is what gets truncated when this overflows -- never the
// direction word, because two devices distinguished only by a host name that
// got cut off are worse than one whose host name is short.
//
#define AH_ENDPOINT_NAME_CHARS       (AH_DISPLAY_CHARS + AH_DIRWORD_CHARS + 2)

//
// Per-slot, per-direction volume storage width. Both endpoints are stereo at
// the KS level (the microphone's mono ring is splatted to two channels in the
// DPC), so two cells cover every channel either direction can present.
//
#define AH_VOLUME_MAX_CHANNELS  2

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

    // The peer's base name as the daemon composed it: "AudioHub - <host>",
    // prefix included, direction suffix excluded.
    WCHAR               Display[AH_DISPLAY_CHARS];

    // The composed per-peer endpoint names, "AudioHub - <host> <direction>".
    // These are what goes into EP\0 as PKEY_Device_DeviceDesc, and they are
    // also what a test asserts the system's device list contains, verbatim.
    WCHAR               NameOut[AH_ENDPOINT_NAME_CHARS];
    WCHAR               NameIn[AH_ENDPOINT_NAME_CHARS];

    // TRUE once the registry values exist, i.e. once there is something to
    // clean up. Checked rather than inferred from State: a Set that failed
    // after writing the names still has to remove them.
    BOOLEAN             NamesWritten;

    // TRUE when this slot's endpoints are published under the system's generic
    // direction names instead of the peer's. Reported to the daemon as
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

    // NOTE: there are deliberately NO per-slot filter descriptors here. v3 kept
    // a private copy of each TOPOLOGY filter and its pin array so the bridge
    // pin's KsPinDescriptor.Name could point at a per-peer GUID. The name no
    // longer travels through the pin, so every descriptor is shared again --
    // which also retires the lifetime hazard that copy created (PortCls holds
    // these pointers for as long as the device object lives).

    SIMPLEAUDIOSAMPLE_DEVPROPERTY   OutTopoProps[1];
    SIMPLEAUDIOSAMPLE_DEVPROPERTY   InTopoProps[1];

    ENDPOINT_MINIPAIR   OutPair;
    ENDPOINT_MINIPAIR   InPair;

    // Held between Install and Remove; NULL when not installed.
    PUNKNOWN            OutTopo;
    PUNKNOWN            OutWave;
    PUNKNOWN            InTopo;
    PUNKNOWN            InWave;
} AH_SLOT, *PAH_SLOT;

//
// Called from DriverEntry, before anything else can touch the table.
//
VOID AhPerPeerDriverInit(VOID);

//
// Called from StartDevice once the adapter exists, and from PnpHandler's
// REMOVE / SURPRISE_REMOVAL / STOP_DEVICE branches BEFORE the adapter is
// Cleanup()'d and Released. Detaching tears every published endpoint down
// first: leaving a slot holding port pointers past the adapter's death is
// failure mode B3.
//
NTSTATUS AhPerPeerAttachAdapter(_In_ PDEVICE_OBJECT DeviceObject, _In_ PADAPTERCOMMON Adapter);
VOID     AhPerPeerDetachAdapter(VOID);

//
// TRUE once an adapter is attached. Bind IOCTLs answer AH_STATUS_NO_ADAPTER
// until then, and the daemon retries -- the daemon can legitimately start
// before the devnode does.
//
BOOLEAN  AhPerPeerAdapterReady(VOID);

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
//  * SET on a slot already bound to the SAME peer key AND fully published
//    returns the current generation and does nothing else -- no
//    re-registration, no rewriting of the persistent FriendlyName property.
//    The daemon re-Sets a slot whenever the peer's online flag changes, and
//    under "paired means published, disconnect keeps the device" that would
//    otherwise be pure registry churn.
//
//    A slot that is bound but NOT fully published is repaired instead:
//    torn down and reinstalled. "Idempotent" must mean "converges on the
//    intended state", not "never touches a broken one".
//
//  * SET IS ALL OR NOTHING. If either half fails to install, the other half is
//    removed again and the call reports failure. Half a device pair is useless
//    to a user and unrepresentable in the daemon's model, so the driver never
//    produces one; `Published == AH_PUB_BOTH` on success is an invariant a
//    test can assert.
//
//  * CLEAR on a free slot succeeds. CLEAR quoting a stale generation is
//    IGNORED, so a Clear delayed past a re-bind cannot cut down the binding
//    that replaced it.
//
NTSTATUS AhSlotBindSet(
    _In_  ULONG   Slot,
    _In_z_ PCSTR  PeerKey,
    _In_  PCWSTR  Display,
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

#endif // _AUDIOHUB_PERPEER_H_
