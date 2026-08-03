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
// PER-PEER PIN NAMES
// ==================
//
// Windows composes what the user reads as "<pin name> (<devnode FriendlyName>)".
// Measured on the target machine, the bracketed half comes from the DEVNODE's
// FriendlyName -- there is one devnode shared by every peer, so that half can
// never differ per peer. (Setting DEVPKEY_DeviceInterface_FriendlyName on the
// KS interface, which the spec assumed supplied it, changed nothing: the value
// lands in the registry verbatim and no endpoint name uses it.)
//
// The half that IS per-filter is the pin name, and it is resolved through
// KsPinDescriptor.Name -> HKR\MediaCategories\<GUID>\Name, a lookup already
// proven to work here (the capture endpoint went from "Microphone Array" to
// "Microphone" when the static GUID was introduced in M6-1 -> M6-2).
//
// So each slot gets its OWN pin-name GUID, derived from the peer fingerprint,
// and the driver writes that key at bind time. The result reads
//
//     AudioHub - WIN-IR01HVEFU7G <speaker> (AudioHub Virtual Audio)
//
// whose leading run is byte-identical to the macOS name that plan §7.1 froze.
//
// The direction word is NOT hardcoded here: it is READ BACK from the INF's
// static MediaCategories entries (AH_PIN_NAME_OUT / AH_PIN_NAME_IN) at attach
// time, so the localizable strings stay in the INF's [Strings] section, which
// is the only place that can ever grow a [Strings.0409].
//
#define AH_DIRWORD_CHARS        32

//
// AH_DISPLAY_CHARS (128, incl. terminator) + ' ' + a direction word + NUL.
// The peer's half is what gets truncated when this overflows -- never the
// direction word, because two devices distinguished only by a host name that
// got cut off are worse than one whose host name is short.
//
#define AH_PINLABEL_CHARS       (AH_DISPLAY_CHARS + AH_DIRWORD_CHARS + 2)

//
// Both topology filters declare 2 pins today. The per-slot copy is sized with
// headroom and perpeer.cpp C_ASSERTs the real tables against it, so growing a
// table is a compile error rather than a silent overrun of a static array.
//
#define AH_MAX_TOPO_PINS        8

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

    // Per-peer pin-name GUIDs, DERIVED from PeerKey (never allocated, never
    // random). Determinism matters for the same reason the reference string
    // carries the fingerprint: unpair and re-pair the same machine and it must
    // land on the same registry key, so no garbage accumulates and the name is
    // stable across the endpoint id that is itself stable.
    GUID                PinGuidOut;
    GUID                PinGuidIn;

    // What gets written to MediaCategories\<GUID>\Name.
    WCHAR               PinLabelOut[AH_PINLABEL_CHARS];
    WCHAR               PinLabelIn[AH_PINLABEL_CHARS];

    // TRUE once the registry keys exist, i.e. once there is something to clean
    // up. Checked rather than inferred from State: a Set that failed after
    // writing the names still has to remove them.
    BOOLEAN             PinNamesWritten;

    // TRUE when this slot's endpoints are published under the INF's generic
    // direction names instead of the peer's. Reported to the daemon as
    // AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK.
    BOOLEAN             PinNameFallback;

    // Per-slot copies of the two TOPOLOGY filter descriptors and their pin
    // arrays.
    //
    // These are the ONLY descriptors that get copied. Everything else -- node
    // tables, connection tables, data ranges, automation tables, and both WAVE
    // filters -- stays shared, because nothing in them varies per peer. The
    // topology pin array does: the bridge pin's KsPinDescriptor.Name has to
    // point at THIS slot's GUID. sysvad's Bluetooth path deep-copies for the
    // identical reason (it rewrites each endpoint's pin Category).
    //
    // Storage is inside the slot record, which lives for the driver image's
    // lifetime, so there is no ordering to get wrong between freeing a
    // descriptor and PortCls still holding the pointer.
    PCPIN_DESCRIPTOR    OutTopoPins[AH_MAX_TOPO_PINS];
    PCPIN_DESCRIPTOR    InTopoPins[AH_MAX_TOPO_PINS];
    PCFILTER_DESCRIPTOR OutTopoFilter;
    PCFILTER_DESCRIPTOR InTopoFilter;

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

#endif // _AUDIOHUB_PERPEER_H_
