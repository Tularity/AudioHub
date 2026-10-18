/*++

Module Name:

    perpeer.cpp

Abstract:

    Runtime install / remove of one KS filter pair per paired peer.

    Every routine here runs at PASSIVE_LEVEL under a KMUTEX (not a FAST_MUTEX:
    that would raise IRQL to APC_LEVEL, and IoSetDeviceInterfacePropertyData
    and PcRegisterSubdevice both require PASSIVE_LEVEL).

--*/

#pragma warning (disable : 4127)

#include "definitions.h"
#include "endpoints.h"
#define AUDIOHUB_SPEAKER_FORMAT_BANKS_IMPLEMENTATION
#include "minipairs.h"
#undef AUDIOHUB_SPEAKER_FORMAT_BANKS_IMPLEMENTATION
#include "perpeer.h"
#include "ahrings.h"
#include "ctldevice.h"
#include "minwavert.h"
#include "bus.h"

//-----------------------------------------------------------------------------
// State
//-----------------------------------------------------------------------------

static AH_SLOT          g_AhSlots[AUDIOHUB_WIN_MAX_SLOTS];
static KMUTEX           g_AhSlotLock;
static BOOLEAN          g_AhInitialised = FALSE;

//
// v11 HOSTS: the child devnodes whose PortCls adapter is running. An entry is
// added by AhPerPeerChildStart and removed by AhPerPeerChildDetach, both under
// the slot lock, so while that lock is held an entry's PDO and adapter are
// alive: the child FDO -- and with it the adapter -- cannot be removed before
// its detach, and the PDO cannot be deleted while its FDO is attached.
//
// The PDO is needed for two registry paths: reading the INF's static
// MediaCategories entries out of the child's driver software key, and reaching
// each per-peer device interface's own key (interfaces are registered against
// the PDO of the adapter that installs them) to write the endpoint name.
//
typedef struct _AH_HOST
{
    PDEVICE_OBJECT  Pdo;        // NULL = free entry
    PDEVICE_OBJECT  Fdo;
    PADAPTERCOMMON  Adapter;
    AH_UNIT_SPEC    Spec;       // what the bus said this child is, at start
} AH_HOST, *PAH_HOST;

static AH_HOST          g_AhHosts[AH_BUS_MAX_CHILDREN];

//
// The generic fallback labels, READ BACK from the INF's static MediaCategories
// entries at attach time rather than compiled in.
//
// Two reasons this is worth a registry read. First, the strings are localizable
// resources and the INF's [Strings] section is the only place that can ever
// grow a [Strings.0409]; a copy in a .cpp would be the copy that gets forgotten.
// Second, a .cpp holding non-ASCII source bytes is decoded by MSVC using the
// build machine's ANSI code page unless every build passes /utf-8 -- a silent,
// machine-dependent mojibake risk on a driver that is built on a Chinese-locale
// Windows.
//
static WCHAR            g_AhDirWordOut[AH_DIRWORD_CHARS];
static WCHAR            g_AhDirWordIn[AH_DIRWORD_CHARS];
static BOOLEAN          g_AhDirWordsOk = FALSE;

//
// Monotonic across the whole driver, never per-slot. A generation that
// restarted at 1 for each slot would let a late message from slot 3's previous
// tenant match slot 3's current stamp.
//
static ULONG            g_AhNextGeneration = 1;

//
// v10 TOPOLOGY VARIANTS: the bridge pin categories a physical presentation can
// select, other than the two the shared templates already carry
// (KSNODETYPE_SPEAKER / KSNODETYPE_MICROPHONE_ARRAY).
//
// Each entry is a deep copy of the shared topology filter descriptor and its
// pin array with ONLY the bridge pin's KsPinDescriptor.Category replaced; every
// other pointer (automation tables, nodes, connections, data ranges) still
// targets the immutable shared templates. Built once at DriverEntry and never
// written again: PortCls keeps the descriptor pointers for as long as a
// subdevice lives, and a slot rebind must never rewrite storage that a still-
// referenced subdevice points into. Slots only select a variant.
//
#define AH_TOPO_MAX_PINS    8u

typedef struct _AH_TOPO_VARIANT
{
    BOOLEAN             Render;
    ULONG               FormFactor;     // AH_FF_*
    BOOLEAN             Ready;
    PCFILTER_DESCRIPTOR Filter;
    PCPIN_DESCRIPTOR    Pins[AH_TOPO_MAX_PINS];
} AH_TOPO_VARIANT;

static AH_TOPO_VARIANT  g_AhTopoVariants[] =
{
    { TRUE,  AH_FF_HEADPHONES },
    { TRUE,  AH_FF_HEADSET    },
    { TRUE,  AH_FF_HANDSET    },
    { FALSE, AH_FF_MICROPHONE },
    { FALSE, AH_FF_HEADSET    },
    { FALSE, AH_FF_HANDSET    },
};

#define AH_LOCK()   KeWaitForSingleObject(&g_AhSlotLock, Executive, KernelMode, FALSE, NULL)
#define AH_UNLOCK() KeReleaseMutex(&g_AhSlotLock, FALSE)

// Control transitions share the slot lifecycle mutex. Render copies take only
// Gate, which protects a bounded block and makes PREPARE a producer fence.
// EpochNext is never reset by unbind/rebind or by a daemon restart.
typedef struct _AH_SPEAKER_STATE
{
    KSPIN_LOCK Gate;
    ULONG Generation;
    BOOLEAN Published;
    BOOLEAN Ready;
    BOOLEAN NotifyOnReady;
    ULONG Layout;
    ULONG EpochNext;
    const AH_SPEAKER_FORMAT_BANK *Bank;
    AH_FORMAT_PAYLOAD Transaction;
} AH_SPEAKER_STATE;
static AH_SPEAKER_STATE g_AhSpeaker[AUDIOHUB_WIN_MAX_SLOTS];
static ULONGLONG g_AhSpeakerSessionId = 0;
#define AH_FORMAT_UPDATING 8u // private stage; never sent on the wire

#pragma code_seg()
const AH_SPEAKER_FORMAT_BANK *AhSpeakerFormatSnapshot(ULONG Slot, PULONG Layout)
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS) { return NULL; }
    AH_SPEAKER_STATE *state = &g_AhSpeaker[Slot];
    KIRQL irql;
    KeAcquireSpinLock(&state->Gate, &irql);
    const AH_SPEAKER_FORMAT_BANK *bank = state->Bank;
    if (Layout) { *Layout = state->Layout; }
    KeReleaseSpinLock(&state->Gate, irql);
    return bank;
}

#pragma code_seg()
BOOLEAN AhSpeakerRenderEnter(ULONG Slot, ULONG Channels, PULONG Epoch, PKIRQL Irql)
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS) { return FALSE; }
    AH_SPEAKER_STATE *state = &g_AhSpeaker[Slot];
    KeAcquireSpinLock(&state->Gate, Irql);
    if (!state->Ready || !state->Published ||
        state->Layout != AhSpeakerLayoutFromChannels(Channels))
    {
        KeReleaseSpinLock(&state->Gate, *Irql);
        return FALSE;
    }
    *Epoch = state->Transaction.epoch;
    return TRUE;
}

#pragma code_seg()
VOID AhSpeakerRenderLeave(ULONG Slot, KIRQL Irql)
{
    KeReleaseSpinLock(&g_AhSpeaker[Slot].Gate, Irql);
}

// Caller owns the lifecycle mutex. A render callback cannot cross this fence.
static VOID AhSpeakerPauseLocked(ULONG Slot)
{
    KIRQL irql;
    KeAcquireSpinLock(&g_AhSpeaker[Slot].Gate, &irql);
    g_AhSpeaker[Slot].Ready = FALSE;
    KeReleaseSpinLock(&g_AhSpeaker[Slot].Gate, irql);
}

// v11: the speaker format machinery follows what the slot ACTUALLY publishes,
// read under the slot lock by every path that changes it -- the Set and Clear
// as well as a child devnode starting or going away. Publication is
// asynchronous now, so a value copied out of a Bind reply and applied after
// the lock was dropped could already be stale. Caller owns the slot lock.
static ULONG AhSlotPublishedMask(_In_ const AH_SLOT *Slot);

#pragma code_seg()
static VOID AhSpeakerFormatSyncLocked(ULONG Slot)
{
    PAGED_CODE();
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS) { return; }
    const AH_SLOT *slot = &g_AhSlots[Slot];
    const ULONG Generation = slot->Generation;
    const BOOLEAN Published = (slot->State == AH_SLOT_BOUND &&
                               (AhSlotPublishedMask(slot) & AH_PUB_RENDER) != 0) ? TRUE : FALSE;
    AH_SPEAKER_STATE *state = &g_AhSpeaker[Slot];
    KIRQL irql;
    KeAcquireSpinLock(&state->Gate, &irql);
    if (state->Generation != Generation || state->Published != Published)
    {
        state->Ready = FALSE;
        state->NotifyOnReady = FALSE;
        state->Generation = Generation;
        state->Published = Published;
        // Keep the public bank until the replacement contract is committed.
        // Clearing it here without a PortCls update would leave a stale cache.
        RtlZeroMemory(&state->Transaction, sizeof(state->Transaction));
    }
    KeReleaseSpinLock(&state->Gate, irql);
}

#pragma code_seg()
VOID AhSpeakerFormatDetach(ULONGLONG SessionId)
{
    PAGED_CODE();
    AH_LOCK();
    g_AhSpeakerSessionId = SessionId;
    for (ULONG slot = 0; slot < AUDIOHUB_WIN_MAX_SLOTS; ++slot)
    {
        KIRQL irql;
        KeAcquireSpinLock(&g_AhSpeaker[slot].Gate, &irql);
        g_AhSpeaker[slot].Ready = FALSE;
        g_AhSpeaker[slot].NotifyOnReady = FALSE;
        RtlZeroMemory(&g_AhSpeaker[slot].Transaction, sizeof(AH_FORMAT_PAYLOAD));
        KeReleaseSpinLock(&g_AhSpeaker[slot].Gate, irql);
    }
    AH_UNLOCK();
}

static BOOLEAN AhSameFormatTransaction(const AH_FORMAT_PAYLOAD *A, const AH_FORMAT_PAYLOAD *B)
{
    return A->endpoint == B->endpoint && A->generation == B->generation &&
        A->layout == B->layout && A->supported_mask == B->supported_mask &&
        A->epoch == B->epoch && A->session_id == B->session_id &&
        A->request_id == B->request_id;
}

// Gate is held; the pin-creation path must never take the lifecycle mutex
// while PortCls owns its pin lock. No foreign callback or allocation here.
static ULONG AhSpeakerBeginGateHeld(ULONG Slot, const AH_FORMAT_PAYLOAD *Offer, AH_FORMAT_PAYLOAD *Event)
{
    AH_SPEAKER_STATE *state = &g_AhSpeaker[Slot];
    if (!state->Published || state->Generation != Offer->generation)
    {
        return AH_STATUS_NOT_BOUND;
    }
    if (state->EpochNext == MAXULONG) { return AH_STATUS_INTERNAL; }
    state->Ready = FALSE;
    state->NotifyOnReady = FALSE;
    state->Transaction = *Offer;
    state->Transaction.epoch = ++state->EpochNext;
    state->Transaction.op = AH_FORMAT_PREPARE;
    *Event = state->Transaction;
    return AH_STATUS_OK;
}

#pragma code_seg()
ULONG AhSpeakerFormatControl(const AH_FORMAT_PAYLOAD *Message)
{
    PAGED_CODE();
    if (Message->endpoint >= AUDIOHUB_WIN_MAX_SLOTS * 2 || (Message->endpoint & 1) ||
        Message->generation == 0 || Message->session_id == 0 ||
        Message->layout >= AH_SPEAKER_LAYOUT_COUNT ||
        !AhSpeakerFormatBankMaskIsValid(Message->supported_mask) ||
        !(Message->supported_mask & AH_SPEAKER_LAYOUT_BIT(Message->layout)))
    {
        return AH_STATUS_BAD_ARGUMENT;
    }
    const ULONG slot = Message->endpoint / 2;
    AH_FORMAT_PAYLOAD event = {};
    PPORTEVENTS portEvents = NULL;
    ULONG result = AH_STATUS_BAD_ARGUMENT;
    BOOLEAN commit = FALSE;
    BOOLEAN notify = FALSE;
    AH_LOCK();
    AH_SPEAKER_STATE *state = &g_AhSpeaker[slot];
    KIRQL irql;
    KeAcquireSpinLock(&state->Gate, &irql);
    if (Message->session_id != g_AhSpeakerSessionId)
    {
        result = AH_STATUS_STALE_SESSION;
    }
    else if (!state->Published || state->Generation != Message->generation ||
        !g_AhSlots[slot].OutWave || g_AhSlots[slot].Generation != Message->generation)
    {
        result = AH_STATUS_NOT_BOUND;
    }
    else if (Message->op == AH_FORMAT_OFFER && Message->epoch == 0 && Message->request_id != 0)
    {
        result = AhSpeakerBeginGateHeld(slot, Message, &event);
    }
    else if (AhSameFormatTransaction(Message, &state->Transaction))
    {
        if (Message->op == AH_FORMAT_QUIESCED && state->Transaction.op == AH_FORMAT_PREPARE)
        {
            // Reserve the transition against native pin creation while the
            // lifecycle mutex serializes removal and other IOCTL requests.
            state->Transaction.op = AH_FORMAT_UPDATING;
            commit = TRUE;
        }
        else if (Message->op == AH_FORMAT_ACCEPTED && state->Transaction.op == AH_FORMAT_COMMITTED)
        {
            state->Transaction.op = AH_FORMAT_READY;
            state->Ready = TRUE;
            notify = state->NotifyOnReady;
            state->NotifyOnReady = FALSE;
            event = state->Transaction;
            result = AH_STATUS_OK;
        }
        else if ((Message->op == AH_FORMAT_QUIESCED && state->Transaction.op == AH_FORMAT_COMMITTED) ||
                 (Message->op == AH_FORMAT_ACCEPTED && state->Transaction.op == AH_FORMAT_READY))
        {
            event = state->Transaction;
            result = AH_STATUS_OK;
        }
    }
    KeReleaseSpinLock(&state->Gate, irql);
    if (commit)
    {
        // Both sides are fenced. Do not reset indices before QUIESCED, or an
        // old daemon read can cross the reset and consume a different epoch.
        AhRingsResetDirection(slot, AUDIOHUB_DIR_OUT);
        const AH_SPEAKER_FORMAT_BANK *bank = AhSpeakerFormatBankLookup(Message->supported_mask);
        const BOOLEAN bankChanged = bank != state->Bank;
        NTSTATUS status = STATUS_DEVICE_NOT_READY;
        PUNKNOWN unknown = g_AhSlots[slot].OutWaveMiniport;
        PMINIPORTWAVERT miniport = NULL;
        if (unknown && NT_SUCCESS(unknown->QueryInterface(IID_IMiniportWaveRT, (PVOID *)&miniport)))
        {
            status = static_cast<CMiniportWaveRT *>(miniport)->AhRefreshSpeakerFormats(bank);
            miniport->Release();
        }
        KeAcquireSpinLock(&state->Gate, &irql);
        if (NT_SUCCESS(status))
        {
            state->Bank = bank;
            state->Layout = Message->layout;
        }
        state->Transaction.op = NT_SUCCESS(status) ? AH_FORMAT_COMMITTED : AH_FORMAT_ABORTED;
        state->NotifyOnReady = NT_SUCCESS(status) &&
            (bankChanged || Message->request_id != 0);
        event = state->Transaction;
        KeReleaseSpinLock(&state->Gate, irql);
        result = NT_SUCCESS(status) ? AH_STATUS_OK : AH_STATUS_INTERNAL;
    }
    // A format-change listener may immediately probe or reopen a pin.
    // COMMITTED still rejects that pin as busy; notify only after ACCEPTED
    // makes it READY, so Windows cannot cache a transient format failure.
    // Native layout-only transactions never notify while holding a pin lock.
    if (notify)
    {
        (VOID)g_AhSlots[slot].OutWave->QueryInterface(IID_IPortEvents, (PVOID *)&portEvents);
    }
    AH_UNLOCK();
    if (event.op) { AhCtlRaiseFormat(&event); }
    // Never call PortCls event consumers while holding either driver lock.
    if (portEvents)
    {
        portEvents->GenerateEventList((GUID *)&KSEVENTSETID_PinCapsChange,
            KSEVENT_PINCAPS_FORMATCHANGE, TRUE, KSPIN_WAVE_RENDER3_SINK_SYSTEM, FALSE, 0);
        portEvents->Release();
    }
    return result;
}

#pragma code_seg()
NTSTATUS AhSpeakerFormatRequest(ULONG Slot, ULONG Channels)
{
    PAGED_CODE();
    const ULONG layout = AhSpeakerLayoutFromChannels(Channels);
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || layout >= AH_SPEAKER_LAYOUT_COUNT)
    {
        return STATUS_NO_MATCH;
    }
    AH_FORMAT_PAYLOAD event = {};
    AH_FORMAT_PAYLOAD pending = {};
    NTSTATUS status = STATUS_SUCCESS;
    KIRQL irql;
    AH_SPEAKER_STATE *state = &g_AhSpeaker[Slot];
    KeAcquireSpinLock(&state->Gate, &irql);
    if (!AhSpeakerFormatBankFormat(state->Bank, layout)) { status = STATUS_NO_MATCH; }
    else if (state->Transaction.op == AH_FORMAT_PREPARE || state->Transaction.op == AH_FORMAT_COMMITTED ||
             state->Transaction.op == AH_FORMAT_UPDATING)
    {
        if (layout != state->Transaction.layout || state->Transaction.request_id != 0)
        {
            // An OFFER may need PortCls's pin lock to refresh its ranges.
            // Do not wait for it while creating a pin with that lock held.
            status = STATUS_DEVICE_BUSY;
        }
        else { pending = state->Transaction; }
    }
    else if (state->Ready && state->Layout != layout)
    {
        AH_FORMAT_PAYLOAD offer = state->Transaction;
        offer.op = AH_FORMAT_OFFER;
        offer.layout = layout;
        offer.request_id = 0;
        if (AhSpeakerBeginGateHeld(Slot, &offer, &event) != AH_STATUS_OK)
        {
            status = STATUS_DEVICE_NOT_READY;
        }
        else { pending = event; }
    }
    else if (!state->Ready) { status = STATUS_DEVICE_NOT_READY; }
    KeReleaseSpinLock(&state->Gate, irql);
    if (event.op) { AhCtlRaiseFormat(&event); }
    if (NT_SUCCESS(status) && pending.epoch != 0)
    {
        // Only a native layout-only transaction can be waited here; it does
        // not call UpdatePinDescriptor. A superseding OFFER, detach or rebind
        // fails this pin creation instead of opening a permanently silent pin.
        const ULONGLONG deadline = KeQueryInterruptTime() + 20000000ull;
        for (;;)
        {
            KeAcquireSpinLock(&state->Gate, &irql);
            if (!AhSameFormatTransaction(&state->Transaction, &pending) || !state->Published ||
                state->Generation != pending.generation || state->Transaction.op == AH_FORMAT_ABORTED)
            {
                status = STATUS_DEVICE_NOT_READY;
            }
            else if (state->Ready && state->Layout == layout)
            {
                KeReleaseSpinLock(&state->Gate, irql);
                return STATUS_SUCCESS;
            }
            else if (KeQueryInterruptTime() >= deadline)
            {
                state->Ready = FALSE;
                state->Transaction.op = AH_FORMAT_ABORTED;
                event = state->Transaction;
                status = STATUS_IO_TIMEOUT;
            }
            KeReleaseSpinLock(&state->Gate, irql);
            if (!NT_SUCCESS(status)) { break; }
            LARGE_INTEGER interval;
            interval.QuadPart = -100000ll; // 10 ms; bounded, not a busy wait
            (VOID)KeDelayExecutionThread(KernelMode, FALSE, &interval);
        }
        if (status == STATUS_IO_TIMEOUT) { AhCtlRaiseFormat(&event); }
    }
    return status;
}

//
// DEVPKEY_DeviceInterface_FriendlyName -- {026E516E-B814-414B-83CD-856D6FEF4822}, PID 2.
//
// Spelled out rather than pulled from <devpkey.h>: that header's
// DEFINE_DEVPROPKEY only EMITS storage when INITGUID is defined before
// <devpropdef.h> is first included, and portcls.h has already pulled the latter
// in by the time any of our code runs. Defining INITGUID here to work around
// that would also instantiate every other GUID this translation unit sees, and
// collide with adapter.cpp. A plain initialiser has neither problem.
//
static const DEVPROPKEY AhDevpkeyInterfaceFriendlyName = {
    { 0x026e516e, 0xb814, 0x414b, { 0x83, 0xcd, 0x85, 0x6d, 0x6f, 0xef, 0x48, 0x22 } }, 2
};

//-----------------------------------------------------------------------------
// Per-peer endpoint names
//
// See the long comment above AH_DIRWORD_CHARS in perpeer.h for WHY the name is
// delivered as PKEY_Device_DeviceDesc under the interface's EP\0 key, and why
// neither the pin name nor the interface FriendlyName can do it for a speaker.
//
// MediaCategories is still READ here -- it is where the INF keeps the generic
// fallback names -- but it is no longer WRITTEN. Microsoft
// documents the machine-wide key as "reserved for global definitions and
// should not be modified by new drivers ... will not be supported in a future
// OS release", and the per-peer software-key entries it used to hold turned
// out to name only one of the two directions.
//-----------------------------------------------------------------------------

//
// "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}" == 38 characters + NUL.
//
#define AH_GUIDSTR_CHARS    39

//
// The two registry locations KS consults for a pin-name GUID, in the order KS
// consults them:
//
//   0  the device's own software key -- "Starting with Windows 10 October 2018
//      Update, version 1809, when searching the registry, KS first looks for an
//      entry in the device's software key". This is where the INF's
//      HKR,MediaCategories,... entries landed.
//   1  the machine-wide fallback KS drops to when the software key has no entry.
//
// Read in that order so the fallback labels come from wherever KS would have
// taken them, rather than from wherever we happened to look first.
//
#define AH_MEDIACAT_SOFTWAREKEY 0u
#define AH_MEDIACAT_GLOBAL      1u
#define AH_MEDIACAT_COUNT       2u

#define AH_MEDIACAT_SUBKEY_W    L"MediaCategories"
#define AH_MEDIACAT_GLOBAL_W \
    L"\\Registry\\Machine\\SYSTEM\\CurrentControlSet\\Control\\MediaCategories"

//
// PKEY_Device_DeviceDesc == {a45c254e-df1c-4efd-8020-67d146a850e0}, PID 2.
//
// Spelled as the literal value name because that is the form the endpoint
// builder looks for: EP\0 values are named by their property key, not by a
// DEVPROPKEY structure. The two halves of the endpoint name the user reads are
// this property and the devnode's FriendlyName, in that order.
//
#define AH_EP_SUBKEY_EP_W       L"EP"
#define AH_EP_SUBKEY_0_W        L"0"
#define AH_EP_DEVICEDESC_W      L"{a45c254e-df1c-4efd-8020-67d146a850e0},2"
// Copied by AudioEndpointBuilder from the topology interface's EP\0 key into
// the resulting MMDevice property store.  User mode uses this opaque identity
// instead of a localised/stale friendly name when it must address the exact
// virtual endpoint whose volume changed.
#define AH_EP_PEERKEY_W         L"{8ca48324-7d8a-4efa-8dd4-7b7503af964b},2"

#define AH_POOLTAG_PERPEER  'PphA'      // "AhpP"

#pragma code_seg("PAGE")
static VOID
AhFormatGuidKey(
    _In_  const GUID *Guid,
    _Out_writes_(AH_GUIDSTR_CHARS) PWSTR Out
    )
/*++

Routine Description:

    The registry key name for a MediaCategories entry.

    Hand-formatted rather than RtlStringFromGUID so there is no pool allocation
    and no failure path on a routine that the teardown path also has to run:
    a cleanup that can fail for want of memory is a cleanup that leaves garbage.

--*/
{
    PAGED_CODE();

    (VOID)RtlStringCchPrintfW(
        Out, AH_GUIDSTR_CHARS,
        L"{%08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X}",
        Guid->Data1, Guid->Data2, Guid->Data3,
        Guid->Data4[0], Guid->Data4[1],
        Guid->Data4[2], Guid->Data4[3], Guid->Data4[4],
        Guid->Data4[5], Guid->Data4[6], Guid->Data4[7]);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhOpenMediaCategories(
    _In_  PDEVICE_OBJECT Pdo,
    _In_  ULONG       Location,
    _In_  ACCESS_MASK Access,
    _Out_ PHANDLE     Key
    )
/*++

Routine Description:

    Opens the MediaCategories root at one of the two locations, READ ONLY.

    Nothing writes MediaCategories any more. The only reason to come here is to
    read back the fallback labels the INF installed.

    Every handle this returns is function-local at the call site ON PURPOSE.
    Bind IOCTLs run in the DAEMON'S process context, and IoOpenDeviceRegistryKey
    hands back a handle in whatever context it was called from; caching one at
    attach time (system context) and closing it later from another process is
    the kind of bug that only ever reproduces on someone else's machine.

--*/
{
    PAGED_CODE();

    *Key = NULL;

    OBJECT_ATTRIBUTES oa;
    UNICODE_STRING    name;
    NTSTATUS          status;
    HANDLE            root = NULL;

    if (Location == AH_MEDIACAT_SOFTWAREKEY)
    {
        if (Pdo == NULL)
        {
            return STATUS_DEVICE_NOT_READY;
        }
        //
        // PLUGPLAY_REGKEY_DRIVER, not _DEVICE: HKR inside a DDInstall AddReg
        // section is the DRIVER software key, and that is where the INF put the
        // static MediaCategories entries this has to sit beside. v11: the
        // child devnode's software key -- the INF section with these entries
        // is the children's, not the bus's.
        //
        status = IoOpenDeviceRegistryKey(Pdo, PLUGPLAY_REGKEY_DRIVER, Access, &root);
        if (!NT_SUCCESS(status))
        {
            return status;
        }

        RtlInitUnicodeString(&name, AH_MEDIACAT_SUBKEY_W);
        InitializeObjectAttributes(&oa, &name,
                                   OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                                   root, NULL);
    }
    else
    {
        RtlInitUnicodeString(&name, AH_MEDIACAT_GLOBAL_W);
        InitializeObjectAttributes(&oa, &name,
                                   OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                                   NULL, NULL);
    }

    status = ZwOpenKey(Key, Access, &oa);

    if (root != NULL)
    {
        ZwClose(root);
    }
    return status;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhReadPinNameValue(
    _In_  PDEVICE_OBJECT Pdo,
    _In_  const GUID *Guid,
    _Out_writes_z_(Chars) PWSTR Out,
    _In_  ULONG       Chars
    )
/*++

Routine Description:

    Reads MediaCategories\<Guid>\Name, trying the software key first and the
    machine-wide key second -- the order KS itself searches in.

    Used ONLY at attach, to read back the fallback labels the INF installed.

--*/
{
    PAGED_CODE();

    WCHAR guidStr[AH_GUIDSTR_CHARS];
    AhFormatGuidKey(Guid, guidStr);

    Out[0] = L'\0';

    for (ULONG loc = 0; loc < AH_MEDIACAT_COUNT; loc++)
    {
        HANDLE mediaCat = NULL;
        NTSTATUS status = AhOpenMediaCategories(Pdo, loc, KEY_READ, &mediaCat);
        if (!NT_SUCCESS(status))
        {
            continue;
        }

        UNICODE_STRING    sub;
        OBJECT_ATTRIBUTES oa;
        HANDLE            key = NULL;

        RtlInitUnicodeString(&sub, guidStr);
        InitializeObjectAttributes(&oa, &sub,
                                   OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                                   mediaCat, NULL);
        status = ZwOpenKey(&key, KEY_READ, &oa);
        ZwClose(mediaCat);
        if (!NT_SUCCESS(status))
        {
            continue;
        }

        UNICODE_STRING valueName;
        ULONG          len = 0;
        RtlInitUnicodeString(&valueName, L"Name");

        status = ZwQueryValueKey(key, &valueName, KeyValuePartialInformation, NULL, 0, &len);
        if ((status == STATUS_BUFFER_TOO_SMALL || status == STATUS_BUFFER_OVERFLOW) && len > 0)
        {
            PKEY_VALUE_PARTIAL_INFORMATION info = (PKEY_VALUE_PARTIAL_INFORMATION)
                ExAllocatePool2(POOL_FLAG_PAGED, len, AH_POOLTAG_PERPEER);
            if (info != NULL)
            {
                status = ZwQueryValueKey(key, &valueName, KeyValuePartialInformation,
                                         info, len, &len);
                if (NT_SUCCESS(status) &&
                    (info->Type == REG_SZ || info->Type == REG_EXPAND_SZ) &&
                    info->DataLength >= sizeof(WCHAR))
                {
                    //
                    // The value may or may not carry its own terminator.
                    // Measure it; never trust it.
                    //
                    ULONG avail = info->DataLength / sizeof(WCHAR);
                    PCWCH src   = (PCWCH)info->Data;
                    ULONG n     = 0;
                    while (n < avail && n < Chars - 1 && src[n] != L'\0')
                    {
                        Out[n] = src[n];
                        n++;
                    }
                    Out[n] = L'\0';
                }
                ExFreePoolWithTag(info, AH_POOLTAG_PERPEER);
            }
        }
        ZwClose(key);

        if (Out[0] != L'\0')
        {
            return STATUS_SUCCESS;
        }
    }

    return STATUS_OBJECT_NAME_NOT_FOUND;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhOpenOrCreateSubkey(
    _In_   HANDLE  Parent,
    _In_z_ PCWSTR  Name,
    _In_   BOOLEAN Create,
    _Out_  PHANDLE Key
    )
{
    PAGED_CODE();

    *Key = NULL;

    UNICODE_STRING    name;
    OBJECT_ATTRIBUTES oa;

    RtlInitUnicodeString(&name, Name);
    InitializeObjectAttributes(&oa, &name,
                               OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                               Parent, NULL);

    if (Create)
    {
        return ZwCreateKey(Key, KEY_READ | KEY_WRITE, &oa, 0, NULL,
                           REG_OPTION_NON_VOLATILE, NULL);
    }
    return ZwOpenKey(Key, KEY_READ | KEY_WRITE, &oa);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhOpenEndpointParams(
    _In_   PDEVICE_OBJECT Pdo,
    _In_z_ PCWSTR  ReferenceString,
    _In_   BOOLEAN Create,
    _Out_  PHANDLE Key
    )
/*++

Routine Description:

    Opens the "EP\0" subkey under one device interface's Device Parameters --
    the key the audio endpoint builder reads in step 5 of its algorithm, AFTER
    it has already chosen a default name in step 3. That ordering is the whole
    reason this works where the pin name does not.

    The interface is registered first, purely to obtain its registry path.
    IoRegisterDeviceInterface is idempotent -- it hands back the existing
    symbolic link for a reference string it has already seen -- and
    MigrateDeviceInterfaceTemplateParameters reaches the template's key exactly
    this way, so this is the house pattern rather than a new trick.

    v11: Pdo is the HOST CHILD's PDO. PortCls registers a subdevice's
    interfaces against the PDO of the adapter that installs it, so the EP\0
    store an endpoint is built from belongs to the child devnode that hosts the
    direction, and the same reference string on another child is another
    interface (and another endpoint id).

    Called BEFORE the filters are installed, which matters twice over. The
    interface is not yet enabled, so the value is in place before any endpoint
    can be built from it; and the later template migration adds the template's
    own EP\0 values alongside ours rather than replacing them (measured -- a
    value written here survived two subsequent binds).

    Handles are function-local at every call site ON PURPOSE: bind IOCTLs run
    in the DAEMON's process context, and a handle cached in one context and
    closed from another is the kind of bug that only reproduces on somebody
    else's machine.

--*/
{
    PAGED_CODE();

    *Key = NULL;

    if (Pdo == NULL)
    {
        return STATUS_DEVICE_NOT_READY;
    }

    UNICODE_STRING refString;
    UNICODE_STRING symlink;

    RtlInitUnicodeString(&refString, ReferenceString);
    RtlZeroMemory(&symlink, sizeof(symlink));

    NTSTATUS status = IoRegisterDeviceInterface(
        Pdo, &KSCATEGORY_AUDIO, &refString, &symlink);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    HANDLE params = NULL;
    status = IoOpenDeviceInterfaceRegistryKey(&symlink, KEY_READ | KEY_WRITE, &params);
    RtlFreeUnicodeString(&symlink);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    //
    // "EP\0" is two levels and ZwCreateKey creates only a leaf, so walk it.
    //
    HANDLE ep = NULL;
    status = AhOpenOrCreateSubkey(params, AH_EP_SUBKEY_EP_W, Create, &ep);
    ZwClose(params);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    status = AhOpenOrCreateSubkey(ep, AH_EP_SUBKEY_0_W, Create, Key);
    ZwClose(ep);
    return status;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhWriteEndpointName(
    _In_   PDEVICE_OBJECT Pdo,
    _In_z_ PCWSTR ReferenceString,
    _In_z_ PCWSTR Name
    )
{
    PAGED_CODE();

    HANDLE   ep     = NULL;
    NTSTATUS status = AhOpenEndpointParams(Pdo, ReferenceString, TRUE, &ep);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    SIZE_T chars = 0;
    while (chars < AH_ENDPOINT_NAME_CHARS - 1 && Name[chars] != L'\0')
    {
        chars++;
    }

    UNICODE_STRING valueName;
    RtlInitUnicodeString(&valueName, AH_EP_DEVICEDESC_W);
    status = ZwSetValueKey(ep, &valueName, 0, REG_SZ,
                           (PVOID)Name, (ULONG)((chars + 1) * sizeof(WCHAR)));
    ZwClose(ep);
    return status;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhWriteEndpointPeerKey(
    _In_   PDEVICE_OBJECT Pdo,
    _In_z_ PCWSTR ReferenceString,
    _In_z_ PCSTR  PeerKey,
    _In_z_ PCWSTR DirectionTag
    )
{
    PAGED_CODE();

    HANDLE ep = NULL;
    NTSTATUS status = AhOpenEndpointParams(Pdo, ReferenceString, TRUE, &ep);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    WCHAR identity[24]; // "v1:" + 16 hex + ":" + "out" + NUL
    status = RtlStringCchPrintfW(identity, ARRAYSIZE(identity),
                                 L"v1:%S:%s", PeerKey, DirectionTag);
    if (!NT_SUCCESS(status))
    {
        ZwClose(ep);
        return status;
    }

    UNICODE_STRING valueName;
    RtlInitUnicodeString(&valueName, AH_EP_PEERKEY_W);
    SIZE_T identityChars = 0;
    while (identityChars < ARRAYSIZE(identity) && identity[identityChars] != L'\0')
    {
        identityChars++;
    }
    status = ZwSetValueKey(ep, &valueName, 0, REG_SZ, identity,
                           (ULONG)((identityChars + 1) * sizeof(WCHAR)));
    ZwClose(ep);
    return status;
}

#pragma code_seg("PAGE")
static VOID
AhClearEndpointName(
    _In_   PDEVICE_OBJECT Pdo,
    _In_z_ PCWSTR ReferenceString
    )
/*++

Routine Description:

    Removes one interface's name value. Only the value: EP\0 also holds the
    parameters the INF's template supplied, which belong to the interface and
    not to any peer.

    The interface REGISTRATION itself cannot be removed from kernel mode -- the
    kernel has no IoUnregisterDeviceInterface -- so the key survives unpairing
    either way. What must not survive is the string inside it, because that
    string is somebody's computer name.

--*/
{
    PAGED_CODE();

    HANDLE   ep     = NULL;
    NTSTATUS status = AhOpenEndpointParams(Pdo, ReferenceString, FALSE, &ep);
    if (!NT_SUCCESS(status))
    {
        return;
    }

    UNICODE_STRING valueName;
    RtlInitUnicodeString(&valueName, AH_EP_DEVICEDESC_W);
    status = ZwDeleteValueKey(ep, &valueName);
    if (!NT_SUCCESS(status) && status != STATUS_OBJECT_NAME_NOT_FOUND)
    {
        DPF(D_ERROR, ("[AhClearEndpointName] %S delete failed 0x%x", ReferenceString, status));
    }
    ZwClose(ep);
}

#pragma code_seg("PAGE")
static VOID
AhClearEndpointPeerKey(
    _In_   PDEVICE_OBJECT Pdo,
    _In_z_ PCWSTR ReferenceString
    )
{
    PAGED_CODE();

    HANDLE ep = NULL;
    NTSTATUS status = AhOpenEndpointParams(Pdo, ReferenceString, FALSE, &ep);
    if (!NT_SUCCESS(status))
    {
        return;
    }

    UNICODE_STRING valueName;
    RtlInitUnicodeString(&valueName, AH_EP_PEERKEY_W);
    status = ZwDeleteValueKey(ep, &valueName);
    if (!NT_SUCCESS(status) && status != STATUS_OBJECT_NAME_NOT_FOUND)
    {
        DPF(D_ERROR, ("[AhClearEndpointPeerKey] %S delete failed 0x%x",
                      ReferenceString, status));
    }
    ZwClose(ep);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhComposeEndpointName(
    _In_z_ PCWSTR Source,
    _In_z_ PCWSTR DirectionWord,
    _Out_writes_z_(Chars) PWSTR Out,
    _In_   ULONG  Chars
    )
/*++

Routine Description:

    Copies the daemon-composed endpoint half verbatim: the physical endpoint
    half for a physical direction (v10), and for a virtual one the daemon's
    endpoint_desc when it sent one (v11, e.g. the localized "Speakers") or the
    peer label otherwise. DirectionWord is retained only because the INF values
    remain the generic fallback when this per-peer property cannot be written.

--*/
{
    PAGED_CODE();

    UNREFERENCED_PARAMETER(DirectionWord);
    return RtlStringCchCopyW(Out, Chars, Source);
}

//
// THE ONE PLACE THE TWO DIRECTIONAL INTERFACES ARE NAMED.
//
// Everything below this table treats the two directions as an array index.
// That is deliberate and it is the actual fix, not decoration: the defect this
// replaces was a naming mechanism that silently worked for the microphone and
// not for the speaker, and what let it hide for a whole acceptance run was that
// nothing in the code said the two were being handled differently -- they went
// through separate statements that merely looked alike. A loop cannot drift.
//
// A test asserts that AhApplyEndpointNames and AhRemoveEndpointNames mention no
// direction-specific identifier at all, so adding a step "just for the speaker"
// is a build-time argument rather than a bug found on a target machine.
//
#define AH_NAME_DIRECTIONS  2u

typedef struct _AH_NAME_TARGET
{
    PCWSTR  ReferenceString;    // the TOPOLOGY interface carrying this endpoint
    PCWSTR  DirectionWord;      // from the INF, read back at attach
    PCWSTR  IdentityTag;        // "out" or "in", never localised
    PWSTR   Name;               // where the composed name is kept
    PCWSTR  Source;             // the endpoint half (AH_NAME_SOURCE)
    ULONG   PubBit;             // AH_PUB_* of this direction
    ULONG   Index;              // AH_PRES_* of this direction
} AH_NAME_TARGET;

//
// The visible endpoint half of one direction. Physical: the composed
// AH_PRESENTATION.endpoint_desc (v10). Virtual: v11's endpoint_desc when the
// daemon sent one, else the peer label exactly as 1.0.1 used it.
//
#define AH_NAME_SOURCE(Slot, Index) \
    (((Slot)->Pres[(Index)].Klass == AH_PRES_PHYSICAL || (Slot)->Pres[(Index)].Desc[0] != L'\0') \
        ? (PCWSTR)(Slot)->Pres[(Index)].Desc \
        : (PCWSTR)(Slot)->Display)

#pragma code_seg("PAGE")
static VOID
AhNameTargets(
    _In_  PAH_SLOT Slot,
    _Out_writes_(AH_NAME_DIRECTIONS) AH_NAME_TARGET *Targets
    )
{
    PAGED_CODE();

    Targets[0].ReferenceString = Slot->TopoNameOut;
    Targets[0].DirectionWord   = g_AhDirWordOut;
    Targets[0].IdentityTag     = L"out";
    Targets[0].Name            = Slot->NameOut;
    Targets[0].Source          = AH_NAME_SOURCE(Slot, AH_PRES_RENDER);
    Targets[0].PubBit          = AH_PUB_RENDER;
    Targets[0].Index           = AH_PRES_RENDER;

    Targets[1].ReferenceString = Slot->TopoNameIn;
    Targets[1].DirectionWord   = g_AhDirWordIn;
    Targets[1].IdentityTag     = L"in";
    Targets[1].Name            = Slot->NameIn;
    Targets[1].Source          = AH_NAME_SOURCE(Slot, AH_PRES_CAPTURE);
    Targets[1].PubBit          = AH_PUB_CAPTURE;
    Targets[1].Index           = AH_PRES_CAPTURE;
}

#pragma code_seg("PAGE")
static VOID
AhRemoveEndpointNames(
    _Inout_ PAH_SLOT       Slot,
    _In_    ULONG          Directions,
    _In_    PDEVICE_OBJECT Pdo
    )
/*++

Routine Description:

    Removes the name and peer-identity values this slot wrote for the selected
    directions from the CURRENT reference strings on the host child Pdo.

    v11: called for every INTENTIONAL removal of a direction -- unpairing, a
    presentation change that gives the direction new reference strings, the
    direction moving to another unit, the capability being withdrawn -- while
    the host child is still alive. The old interface stays registered (the
    kernel cannot unregister it) and its endpoint stays known as not present,
    but it must not keep somebody's host name or an identity that could be
    mistaken for a live endpoint's. They are removed AFTER the endpoints,
    because until the endpoints are gone the names are what is displayed.

--*/
{
    PAGED_CODE();

    AH_NAME_TARGET targets[AH_NAME_DIRECTIONS];
    AhNameTargets(Slot, targets);

    for (ULONG i = 0; i < AH_NAME_DIRECTIONS; i++)
    {
        if ((Directions & targets[i].PubBit) == 0 || !Slot->NamesWritten[targets[i].Index])
        {
            continue;
        }
        AhClearEndpointName(Pdo, targets[i].ReferenceString);
        AhClearEndpointPeerKey(Pdo, targets[i].ReferenceString);
        Slot->NamesWritten[targets[i].Index] = FALSE;
    }
    //
    // NameFallback is deliberately NOT cleared here. Its lifetime belongs to
    // the bind: reset on a fresh one, raised by any direction's failure.
}

#pragma code_seg("PAGE")
static NTSTATUS
AhApplyEndpointNames(
    _Inout_ PAH_SLOT       Slot,
    _In_    ULONG          Directions,
    _In_    PDEVICE_OBJECT Pdo,
    _In_    ULONG          Flags
    )
/*++

Routine Description:

    Writes the authenticated peer key into the selected directions' topology
    EP\0 stores on the host child Pdo, then the visible endpoint half.

    v11: called per direction, immediately before that direction's filters are
    installed on its host child -- the interface only exists once its host
    does. "Before" stays load-bearing: PcRegisterSubdevice is the step that
    ENABLES the interface, the endpoint builder acts on the arrival edge, and
    the composed name is CACHED per endpoint id, so a name written afterwards
    is a race whose losing side is permanent for that peer.

    Peer identity is mandatory: user mode uses it to address exactly one
    MMDevice for scalar volume synchronization. An endpoint with no identity
    could receive another peer's volume or silently stop synchronizing, so
    that failure fails the install of the direction.

    A name that cannot be written is a usable degradation instead: the
    endpoint comes up under the system's generic fallback label, and
    Slot->NameFallback travels back to the daemon as
    AH_BINDREPLY_FLAG_NAME_FALLBACK rather than being absorbed here. (Since
    v11 two directions may live on two devnodes, so naming is per direction
    and no longer all-or-nothing across the pair.)

--*/
{
    PAGED_CODE();

    AH_NAME_TARGET targets[AH_NAME_DIRECTIONS];
    AhNameTargets(Slot, targets);

    for (ULONG i = 0; i < AH_NAME_DIRECTIONS; i++)
    {
        if ((Directions & targets[i].PubBit) == 0)
        {
            continue;
        }

        // Anything written has to be removable, so the flag goes up BEFORE the
        // first write rather than after the last.
        Slot->NamesWritten[targets[i].Index] = TRUE;
        NTSTATUS st = AhWriteEndpointPeerKey(Pdo, targets[i].ReferenceString,
                                             Slot->PeerKey, targets[i].IdentityTag);
        if (!NT_SUCCESS(st))
        {
            DPF(D_ERROR, ("[AhApplyEndpointNames] peer identity %u failed 0x%x", i, st));
            AhRemoveEndpointNames(Slot, targets[i].PubBit, Pdo);
            return st;
        }

        if (Flags & AH_BINDFLAG_FAIL_ENDPOINT_NAME)
        {
            // The negative control affects presentation only. Identity remains
            // present because a fallback-named endpoint must still be safe to use.
            AhClearEndpointName(Pdo, targets[i].ReferenceString);
            Slot->NameFallback = TRUE;
            continue;
        }

        st = AhComposeEndpointName(targets[i].Source, targets[i].DirectionWord,
                                   targets[i].Name, AH_ENDPOINT_NAME_CHARS);
        if (NT_SUCCESS(st))
        {
            st = AhWriteEndpointName(Pdo, targets[i].ReferenceString, targets[i].Name);
        }
        if (!NT_SUCCESS(st))
        {
            //
            // Clear presentation only: the mandatory identity is independent
            // and must survive a generic-name fallback.
            //
            DPF(D_ERROR, ("[AhApplyEndpointNames] name %u failed 0x%x", i, st));
            AhClearEndpointName(Pdo, targets[i].ReferenceString);
            Slot->NameFallback = TRUE;
            continue;
        }
        DPF(D_TERSE, ("[AhApplyEndpointNames] %S", targets[i].Name));
    }
    return STATUS_SUCCESS;
}

//-----------------------------------------------------------------------------
// Helpers
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
BOOLEAN
AhIsValidPeerKey(
    _In_reads_(Length) const CHAR *Key,
    _In_ SIZE_T Length
    )
/*++

Routine Description:

    Exactly AH_PEERKEY_CHARS lowercase hex digits, nothing else.

    This is not defensive decoration. The peer key becomes a device interface
    reference string, and IoRegisterDeviceInterface's contract is that the
    string "must not contain any path separator characters". A whitelist is the
    only formulation that stays correct when someone later widens the field.

--*/
{
    PAGED_CODE();

    if (Key == NULL || Length != AH_PEERKEY_CHARS)
    {
        return FALSE;
    }

    for (SIZE_T i = 0; i < Length; i++)
    {
        CHAR c = Key[i];
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')))
        {
            return FALSE;
        }
    }

    return TRUE;
}

#pragma code_seg("PAGE")
static SIZE_T
AhPeerKeyLength(
    _In_reads_(Max) const CHAR *Key,
    _In_ SIZE_T Max
    )
{
    PAGED_CODE();

    for (SIZE_T i = 0; i < Max; i++)
    {
        if (Key[i] == '\0')
        {
            return i;
        }
    }
    //
    // No terminator inside the field. The caller has already forced one, so
    // reaching here means the field is entirely non-NUL: report Max, which
    // fails the length check in AhIsValidPeerKey.
    //
    return Max;
}

#pragma code_seg("PAGE")
static const GUID *
AhPresentationCategory(
    _In_ BOOLEAN Render,
    _In_ ULONG   FormFactor
    )
/*++

Routine Description:

    v10: the bridge pin category of a PHYSICAL direction, derived from the
    pair (direction, form factor) -- the daemon never names a KS GUID. NULL for
    every pair outside the whitelist in AudioHubIoctl.h (AH_FF_*).

--*/
{
    PAGED_CODE();

    if (Render)
    {
        switch (FormFactor)
        {
        case AH_FF_SPEAKERS:   return &KSNODETYPE_SPEAKER;
        case AH_FF_HEADPHONES: return &KSNODETYPE_HEADPHONES;
        case AH_FF_HEADSET:    return &KSNODETYPE_HEADSET_SPEAKERS;
        case AH_FF_HANDSET:    return &KSNODETYPE_HANDSET;
        default:               return NULL;
        }
    }
    switch (FormFactor)
    {
    case AH_FF_MICROPHONE: return &KSNODETYPE_MICROPHONE;
    case AH_FF_HEADSET:    return &KSNODETYPE_HEADSET_MICROPHONE;
    case AH_FF_HANDSET:    return &KSNODETYPE_HANDSET;
    default:               return NULL;
    }
}

#pragma code_seg("PAGE")
static PCFILTER_DESCRIPTOR *
AhPresentationTopology(
    _In_ BOOLEAN Render,
    _In_ ULONG   Klass,
    _In_ ULONG   FormFactor
    )
/*++

Routine Description:

    v10: the topology filter descriptor one direction installs with. Virtual
    and physical speakers use the shared speaker template (its bridge pin is
    already KSNODETYPE_SPEAKER); virtual capture uses the shared microphone
    array template. Every other physical look selects one of the immutable
    variants built at DriverEntry. NULL = not presentable.

--*/
{
    PAGED_CODE();

    if (Klass == AH_PRES_VIRTUAL)
    {
        return Render ? &SpeakerTopoMiniportFilterDescriptor
                      : &MicArray1TopoMiniportFilterDescriptor;
    }
    if (Klass != AH_PRES_PHYSICAL || AhPresentationCategory(Render, FormFactor) == NULL)
    {
        return NULL;
    }
    if (Render && FormFactor == AH_FF_SPEAKERS)
    {
        return &SpeakerTopoMiniportFilterDescriptor;
    }
    for (ULONG i = 0; i < ARRAYSIZE(g_AhTopoVariants); i++)
    {
        AH_TOPO_VARIANT *v = &g_AhTopoVariants[i];
        if (v->Render == Render && v->FormFactor == FormFactor)
        {
            return v->Ready ? &v->Filter : NULL;
        }
    }
    return NULL;
}

#pragma code_seg("PAGE")
static VOID
AhBuildTopologyVariants(VOID)
/*++

Routine Description:

    Builds g_AhTopoVariants once, at DriverEntry, before any slot can select
    one. A variant whose template does not have exactly one bridge pin of the
    expected category stays not Ready, and a physical request for it is then
    refused rather than installed with the wrong category.

--*/
{
    PAGED_CODE();

    for (ULONG i = 0; i < ARRAYSIZE(g_AhTopoVariants); i++)
    {
        AH_TOPO_VARIANT *v = &g_AhTopoVariants[i];
        const PCFILTER_DESCRIPTOR *src = v->Render ? &SpeakerTopoMiniportFilterDescriptor
                                                   : &MicArray1TopoMiniportFilterDescriptor;
        const GUID *match = v->Render ? &KSNODETYPE_SPEAKER : &KSNODETYPE_MICROPHONE_ARRAY;
        const GUID *replace = AhPresentationCategory(v->Render, v->FormFactor);

        v->Ready = FALSE;
        if (replace == NULL || src->PinCount > AH_TOPO_MAX_PINS ||
            src->PinSize != sizeof(PCPIN_DESCRIPTOR))
        {
            DPF(D_ERROR, ("[AhBuildTopologyVariants] variant %u unusable", i));
            continue;
        }

        v->Filter = *src;
        ULONG replaced = 0;
        for (ULONG p = 0; p < src->PinCount; p++)
        {
            v->Pins[p] = src->Pins[p];
            if (v->Pins[p].KsPinDescriptor.Category != NULL &&
                IsEqualGUID(*v->Pins[p].KsPinDescriptor.Category, *match))
            {
                v->Pins[p].KsPinDescriptor.Category = replace;
                replaced++;
            }
        }
        v->Filter.Pins = v->Pins;
        v->Ready = (replaced == 1) ? TRUE : FALSE;
        if (!v->Ready)
        {
            DPF(D_ERROR, ("[AhBuildTopologyVariants] variant %u replaced %u bridge pins", i, replaced));
        }
    }
}

//
// v11: a REG_SZ / FriendlyName the user reads may not carry control
// characters. Empty is accepted here; the callers decide whether it may be.
//
#pragma code_seg("PAGE")
static BOOLEAN
AhIsDisplayText(
    _In_reads_(AH_DISPLAY_CHARS) PCWSTR Text
    )
{
    PAGED_CODE();

    for (ULONG i = 0; i < AH_DISPLAY_CHARS && Text[i] != L'\0'; i++)
    {
        WCHAR c = Text[i];
        if (c < 0x20 || c == 0x7f)
        {
            return FALSE;
        }
    }
    return TRUE;
}

#pragma code_seg("PAGE")
static BOOLEAN
AhNormalizePresentation(
    _In_  BOOLEAN                Render,
    _In_  const AH_PRESENTATION *In,
    _Out_ PAH_SLOT_PRESENTATION  Out
    )
/*++

Routine Description:

    Validates one direction of the request and reduces it to what the slot
    stores. The caller has already terminated every string field. FALSE =
    malformed; nothing about the slot may change then.

    A physical look is accepted only with a whitelisted form factor for the
    direction, a key of exactly AH_PRESKEY_CHARS lowercase hex digits (it
    becomes part of a device-interface reference string), a non-empty endpoint
    name and (v11) a non-empty child devnode name, neither with control
    characters (they become a REG_SZ the endpoint builder displays and a
    FriendlyName). A virtual look carries no identity of its own; v11 lets it
    carry an endpoint half and a devnode name, both optional.

--*/
{
    PAGED_CODE();

    RtlZeroMemory(Out, sizeof(*Out));

    if (In->reserved != 0 ||
        !AhIsDisplayText(In->endpoint_desc) ||
        !AhIsDisplayText(In->adapter_name))
    {
        return FALSE;
    }
    if (In->klass == AH_PRES_VIRTUAL)
    {
        if (In->key[0] != '\0')
        {
            return FALSE;
        }
        Out->Klass = AH_PRES_VIRTUAL;
        //
        // group is meaningless for a virtual direction: every virtual
        // direction of a peer shares the peer's "v" unit.
        //
        (VOID)RtlStringCchCopyW(Out->Desc, AH_DISPLAY_CHARS, In->endpoint_desc);
        (VOID)RtlStringCchCopyW(Out->AdapterName, AH_DISPLAY_CHARS, In->adapter_name);
        return TRUE;
    }
    if (In->klass != AH_PRES_PHYSICAL ||
        AhPresentationTopology(Render, AH_PRES_PHYSICAL, In->form_factor) == NULL)
    {
        return FALSE;
    }

    for (ULONG i = 0; i < AH_PRESKEY_BUF; i++)
    {
        CHAR c = In->key[i];
        if (i < AH_PRESKEY_CHARS)
        {
            if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f')))
            {
                return FALSE;
            }
        }
        else if (c != '\0')
        {
            return FALSE;
        }
    }

    if (In->endpoint_desc[0] == L'\0' || In->adapter_name[0] == L'\0')
    {
        return FALSE;
    }

    Out->Klass      = AH_PRES_PHYSICAL;
    Out->FormFactor = In->form_factor;
    Out->Group      = In->group;
    RtlCopyMemory(Out->Key, In->key, AH_PRESKEY_CHARS);
    Out->Key[AH_PRESKEY_CHARS] = '\0';
    NTSTATUS st = RtlStringCchCopyW(Out->Desc, AH_DISPLAY_CHARS, In->endpoint_desc);
    if (!NT_SUCCESS(st))
    {
        return FALSE;
    }
    st = RtlStringCchCopyW(Out->AdapterName, AH_DISPLAY_CHARS, In->adapter_name);
    if (!NT_SUCCESS(st))
    {
        return FALSE;
    }
    return TRUE;
}

//
// v10: whether two stored presentations need DIFFERENT endpoints. The name is
// deliberately not part of it: Windows caches a name per endpoint id, so a
// rename under the same key could not take effect anyway, and rebuilding for it
// would cycle a live endpoint for nothing.
//
#pragma code_seg("PAGE")
static BOOLEAN
AhSamePresentationIdentity(
    _In_ const AH_SLOT_PRESENTATION *A,
    _In_ const AH_SLOT_PRESENTATION *B
    )
{
    PAGED_CODE();

    return (A->Klass == B->Klass &&
            A->FormFactor == B->FormFactor &&
            strncmp(A->Key, B->Key, AH_PRESKEY_BUF) == 0) ? TRUE : FALSE;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhBuildRefStrings(
    _Inout_ PAH_SLOT Slot,
    _In_z_  PCSTR    PeerKey
    )
/*++

Routine Description:

    "AhTopoOut-a1b2c3d4e5f60718" and its three siblings, built from the
    slot's stored presentation: a physical direction appends "-p<key8>"
    (v10), so a different look is a different endpoint id and an earlier look
    returns its earlier id. A virtual direction keeps the 1.0.1 string.

    The suffix is the PEER FINGERPRINT, never the slot number. With the slot
    number, freeing slot 3 and giving it to a different peer would hand that
    peer the previous tenant's endpoint id -- and therefore its default-device
    selection, its volume and any name the user typed in mmsys.cpl. That
    failure is silent: no error, no bugcheck, just "the machine I paired
    yesterday somehow became my default speaker".

--*/
{
    PAGED_CODE();

    //
    // RtlStringCbPrintfW's %S on a CHAR* in kernel mode converts ANSI to
    // UTF-16 one byte at a time, which is exactly right for a hex string.
    //
    struct { PWSTR Buffer; PCWSTR Template; ULONG Pres; } map[] = {
        { Slot->TopoNameOut, AH_TEMPLATE_TOPO_OUT, AH_PRES_RENDER  },
        { Slot->WaveNameOut, AH_TEMPLATE_WAVE_OUT, AH_PRES_RENDER  },
        { Slot->TopoNameIn,  AH_TEMPLATE_TOPO_IN,  AH_PRES_CAPTURE },
        { Slot->WaveNameIn,  AH_TEMPLATE_WAVE_IN,  AH_PRES_CAPTURE },
    };

    for (ULONG i = 0; i < ARRAYSIZE(map); i++)
    {
        const AH_SLOT_PRESENTATION *pres = &Slot->Pres[map[i].Pres];
        NTSTATUS st;
        if (pres->Klass == AH_PRES_PHYSICAL)
        {
            //
            // The key was validated as exactly AH_PRESKEY_CHARS lowercase hex
            // digits (AhNormalizePresentation): no path separator can reach
            // IoRegisterDeviceInterface through it either.
            //
            st = RtlStringCbPrintfW(
                map[i].Buffer,
                AH_REFSTRING_MAX * sizeof(WCHAR),
                L"%s-%S-p%S",
                map[i].Template,
                PeerKey,
                pres->Key);
        }
        else
        {
            st = RtlStringCbPrintfW(
                map[i].Buffer,
                AH_REFSTRING_MAX * sizeof(WCHAR),
                L"%s-%S",
                map[i].Template,
                PeerKey);
        }
        if (!NT_SUCCESS(st))
        {
            return st;
        }
    }

    return STATUS_SUCCESS;
}

// The speaker gate raises IRQL while the initial state is reset. The caller
// still enters at PASSIVE_LEVEL, but this routine's instructions must remain
// resident across that critical section.
#pragma code_seg()
static NTSTATUS
AhBuildMinipairs(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    Fills the slot's two ENDPOINT_MINIPAIRs from the static templates.

    Every render pointer graph comes from a driver-global immutable bank.
    Slots share the selected bank; nothing inside a bank varies per peer and
    no bank is rewritten when a slot is rebound.

    v3 deep-copied the two TOPOLOGY filters and their pin arrays so that each
    peer's bridge pin could point at a per-peer Name GUID. The name no longer
    travels through the pin, so the copies are gone, and with them the one
    per-slot lifetime that PortCls holds pointers into.

    What still points INTO the slot record is the FriendlyName property buffer
    and the four reference strings, all of which outlive every endpoint that
    can be attached to them because the slot array is static.

--*/
{
    PAGED_CODE();

    const AH_SPEAKER_FORMAT_BANK *renderBank =
        AhSpeakerFormatBankLookup(AH_SPEAKER_LAYOUT_MASK_STEREO);
    AH_SPEAKER_STATE *speaker = &g_AhSpeaker[Slot - g_AhSlots];
    KIRQL irql;
    KeAcquireSpinLock(&speaker->Gate, &irql);
    speaker->Ready = FALSE;
    speaker->NotifyOnReady = FALSE;
    speaker->Published = FALSE;
    speaker->Bank = renderBank;
    speaker->Layout = AH_SPEAKER_LAYOUT_STEREO;
    RtlZeroMemory(&speaker->Transaction, sizeof(speaker->Transaction));
    KeReleaseSpinLock(&speaker->Gate, irql);
    const PCFILTER_DESCRIPTOR *renderWaveDescriptor =
        AhSpeakerFormatBankWaveDescriptor(renderBank);
    const PIN_DEVICE_FORMATS_AND_MODES *renderFormatsAndModes =
        AhSpeakerFormatBankPinDeviceFormatsAndModes(renderBank);
    const ULONG renderFormatsAndModesCount =
        AhSpeakerFormatBankPinDeviceFormatsAndModesCount(renderBank);
    const USHORT renderMaximumChannels =
        AhSpeakerFormatBankMaximumChannels(renderBank);

    if (renderWaveDescriptor == NULL ||
        renderFormatsAndModes == NULL ||
        renderFormatsAndModesCount == 0 ||
        renderMaximumChannels != SPEAKER_DEVICE_MAX_CHANNELS)
    {
        return STATUS_DEVICE_NOT_READY;
    }

    ULONG displayBytes = 0;
    for (ULONG i = 0; i < AH_DISPLAY_CHARS; i++)
    {
        if (Slot->Display[i] == L'\0')
        {
            displayBytes = (i + 1) * sizeof(WCHAR);
            break;
        }
    }
    ASSERT(displayBytes != 0);

    //
    // DEVPROP_TYPE_STRING, not DEVPROP_TYPE_STRING_INDIRECT.
    //
    // sysvad uses INDIRECT, but DEVPKEY_DeviceInterface_FriendlyName is
    // declared DEVPROP_TYPE_STRING and INDIRECT means "@file,-resourceId".
    // Our name is a runtime literal built from the peer's computer name; there
    // is no resource to point at. sysvad gets away with it only because
    // Windows falls back to returning the unresolved string.
    //
    Slot->OutTopoProps[0].PropertyKey = &AhDevpkeyInterfaceFriendlyName;
    Slot->OutTopoProps[0].Type        = DEVPROP_TYPE_STRING;
    Slot->OutTopoProps[0].BufferSize  = displayBytes;
    Slot->OutTopoProps[0].Buffer      = Slot->Display;

    Slot->InTopoProps[0] = Slot->OutTopoProps[0];

    //
    // The routing tokens every miniport of this slot gets as its DeviceContext.
    // Built here rather than at DriverInit only because this is where the slot
    // index is already in hand; the contents never change afterwards.
    //
    const ULONG slotIndex = (ULONG)(Slot - g_AhSlots);
    Slot->OutCtx.Magic = AH_EP_CONTEXT_MAGIC;
    Slot->OutCtx.Slot  = slotIndex;
    Slot->OutCtx.Input = FALSE;
    Slot->InCtx.Magic  = AH_EP_CONTEXT_MAGIC;
    Slot->InCtx.Slot   = slotIndex;
    Slot->InCtx.Input  = TRUE;

    //
    // Volume starts at UNITY and unmuted, in BOTH directions.
    //
    // Unity, not "whatever the array happened to hold", and this is the whole
    // point of plan 7.2's transmission invariant: the rings carry full scale
    // and the FAR side attenuates. A virtual endpoint that came up at some
    // other level would apply an attenuation the user never asked for and that
    // no slider on either machine would explain.
    //
    for (ULONG c = 0; c < AH_VOLUME_MAX_CHANNELS; c++)
    {
        Slot->VolumeOut[c] = AH_VOLUME_UNITY;
        Slot->VolumeIn[c]  = AH_VOLUME_UNITY;
        Slot->MuteOut[c]   = FALSE;
        Slot->MuteIn[c]    = FALSE;
    }

    //
    // And the declared downstream latency goes back to "never measured".
    //
    // A slot that changes hands is a DIFFERENT peer, reached over a different
    // network, so the number the previous tenant measured describes nothing
    // here. Carrying it over would make the new endpoint's presentation clock
    // wrong by however far apart the two peers happen to be -- with nothing
    // anywhere reporting a change, because the value would look perfectly
    // plausible.
    //
    Slot->LatencyFramesOut = 0;
    Slot->LatencyFramesIn  = 0;

    //
    // Render pair.
    //
    Slot->OutFormatBank = renderBank;
    RtlZeroMemory(&Slot->OutPair, sizeof(Slot->OutPair));
    Slot->OutPair.DeviceType                    = eSpeakerDevice;
    Slot->OutPair.TopoName                      = Slot->TopoNameOut;
    Slot->OutPair.TemplateTopoName              = (PWSTR)AH_TEMPLATE_TOPO_OUT;
    Slot->OutPair.TopoCreateCallback            = CreateMiniportTopologySimpleAudioSample;
    Slot->OutPair.TopoDescriptor                = &SpeakerTopoMiniportFilterDescriptor;
    Slot->OutPair.TopoInterfacePropertyCount    = ARRAYSIZE(Slot->OutTopoProps);
    Slot->OutPair.TopoInterfaceProperties       = Slot->OutTopoProps;
    Slot->OutPair.WaveName                      = Slot->WaveNameOut;
    Slot->OutPair.TemplateWaveName              = (PWSTR)AH_TEMPLATE_WAVE_OUT;
    Slot->OutPair.WaveCreateCallback            = CreateMiniportWaveRTSimpleAudioSample;
    // ENDPOINT_MINIPAIR predates const-correct descriptor fields. The bank API
    // stays const; these casts are confined to the PortCls handoff structure.
    Slot->OutPair.WaveDescriptor                =
        const_cast<PCFILTER_DESCRIPTOR *>(renderWaveDescriptor);
    Slot->OutPair.WaveInterfacePropertyCount    = 0;
    Slot->OutPair.WaveInterfaceProperties       = NULL;
    Slot->OutPair.DeviceMaxChannels             = AH_SPEAKER_CHANNELS_7POINT1POINT4;
    Slot->OutPair.PinDeviceFormatsAndModes      =
        const_cast<PIN_DEVICE_FORMATS_AND_MODES *>(renderFormatsAndModes);
    Slot->OutPair.PinDeviceFormatsAndModesCount = renderFormatsAndModesCount;
    Slot->OutPair.PhysicalConnections           = SpeakerTopologyPhysicalConnections;
    Slot->OutPair.PhysicalConnectionCount       = SIZEOF_ARRAY(SpeakerTopologyPhysicalConnections);
    Slot->OutPair.DeviceFlags                   = ENDPOINT_NO_FLAGS;

    //
    // Capture pair.
    //
    RtlZeroMemory(&Slot->InPair, sizeof(Slot->InPair));
    Slot->InPair.DeviceType                     = eMicArrayDevice1;
    Slot->InPair.TopoName                       = Slot->TopoNameIn;
    Slot->InPair.TemplateTopoName               = (PWSTR)AH_TEMPLATE_TOPO_IN;
    Slot->InPair.TopoCreateCallback             = CreateMicArrayMiniportTopology;
    Slot->InPair.TopoDescriptor                 = &MicArray1TopoMiniportFilterDescriptor;
    Slot->InPair.TopoInterfacePropertyCount     = ARRAYSIZE(Slot->InTopoProps);
    Slot->InPair.TopoInterfaceProperties        = Slot->InTopoProps;
    Slot->InPair.WaveName                       = Slot->WaveNameIn;
    Slot->InPair.TemplateWaveName               = (PWSTR)AH_TEMPLATE_WAVE_IN;
    Slot->InPair.WaveCreateCallback             = CreateMiniportWaveRTSimpleAudioSample;
    Slot->InPair.WaveDescriptor                 = &MicArrayWaveMiniportFilterDescriptor;
    Slot->InPair.WaveInterfacePropertyCount     = 0;
    Slot->InPair.WaveInterfaceProperties        = NULL;
    Slot->InPair.DeviceMaxChannels              = MICARRAY_DEVICE_MAX_CHANNELS;
    Slot->InPair.PinDeviceFormatsAndModes       = MicArrayPinDeviceFormatsAndModes;
    Slot->InPair.PinDeviceFormatsAndModesCount  = SIZEOF_ARRAY(MicArrayPinDeviceFormatsAndModes);
    Slot->InPair.PhysicalConnections            = MicArray1TopologyPhysicalConnections;
    Slot->InPair.PhysicalConnectionCount        = SIZEOF_ARRAY(MicArray1TopologyPhysicalConnections);
    Slot->InPair.DeviceFlags                    = ENDPOINT_NO_FLAGS;

    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
static VOID
AhSelectTopology(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    v10: points each minipair at the topology descriptor its stored
    presentation needs. Only ever selects among immutable descriptors, so it
    is safe to call for a direction that is live: an unchanged presentation
    selects the pointer it already holds.

--*/
{
    PAGED_CODE();

    PCFILTER_DESCRIPTOR *out = AhPresentationTopology(
        TRUE, Slot->Pres[AH_PRES_RENDER].Klass, Slot->Pres[AH_PRES_RENDER].FormFactor);
    PCFILTER_DESCRIPTOR *in = AhPresentationTopology(
        FALSE, Slot->Pres[AH_PRES_CAPTURE].Klass, Slot->Pres[AH_PRES_CAPTURE].FormFactor);
    //
    // Validated before it was stored; the fallback only keeps a pointer that
    // can never be NULL if that invariant were ever broken.
    //
    Slot->OutPair.TopoDescriptor = (out != NULL) ? out : &SpeakerTopoMiniportFilterDescriptor;
    Slot->InPair.TopoDescriptor  = (in  != NULL) ? in  : &MicArray1TopoMiniportFilterDescriptor;
}

#pragma code_seg("PAGE")
static ULONG
AhSlotPublishedMask(
    _In_ const AH_SLOT *Slot
    )
/*++

Routine Description:

    Which directions of this slot the driver ACTUALLY publishes.

    A half counts only when BOTH its filters exist: a topology filter with no
    wave filter (or the reverse) produces no endpoint, and calling that
    "published" would recreate exactly the over-claim this whole change is
    about.

--*/
{
    PAGED_CODE();

    ULONG mask = 0;
    if (Slot->OutTopo != NULL && Slot->OutWave != NULL) { mask |= AH_PUB_RENDER; }
    if (Slot->InTopo  != NULL && Slot->InWave  != NULL) { mask |= AH_PUB_CAPTURE; }
    return mask;
}

// Unlike AhSlotPublishedMask, this includes a direction for which only one of
// its two port objects survived. Such a half-install is not a usable endpoint,
// but it MUST be torn down before an idempotent retry can install that direction
// again. Ignoring it would leak a port while still reporting the requested mask.
static ULONG
AhSlotHeldMask(
    _In_ const AH_SLOT *Slot
    )
{
    PAGED_CODE();

    ULONG mask = 0;
    if (Slot->OutTopo != NULL || Slot->OutWave != NULL) { mask |= AH_PUB_RENDER; }
    if (Slot->InTopo  != NULL || Slot->InWave  != NULL) { mask |= AH_PUB_CAPTURE; }
    return mask;
}

static ULONG
AhWantedPublishedMask(
    _In_ ULONG Flags
    )
{
    ULONG mask = 0;
    if (Flags & AH_BINDFLAG_WANT_RENDER)  { mask |= AH_PUB_RENDER; }
    if (Flags & AH_BINDFLAG_WANT_CAPTURE) { mask |= AH_PUB_CAPTURE; }
    return mask;
}

//
// AH_PUB_RENDER -> AH_PRES_RENDER, AH_PUB_CAPTURE -> AH_PRES_CAPTURE.
//
#define AH_DIR_INDEX(Direction) (((Direction) == AH_PUB_RENDER) ? AH_PRES_RENDER : AH_PRES_CAPTURE)

//-----------------------------------------------------------------------------
// v11 presentation units and hosts
//-----------------------------------------------------------------------------

//
// Which unit each wanted direction belongs to (perpeer.h AH_UNIT_*). Pure.
//
#pragma code_seg("PAGE")
static VOID
AhComputeUnits(
    _In_reads_(2)  const AH_SLOT_PRESENTATION *Pres,
    _In_           ULONG Want,
    _Out_writes_(2) PULONG UnitOf
    )
{
    PAGED_CODE();

    const BOOLEAN r  = (Want & AH_PUB_RENDER)  ? TRUE : FALSE;
    const BOOLEAN c  = (Want & AH_PUB_CAPTURE) ? TRUE : FALSE;
    const BOOLEAN rp = (r && Pres[AH_PRES_RENDER].Klass  == AH_PRES_PHYSICAL) ? TRUE : FALSE;
    const BOOLEAN cp = (c && Pres[AH_PRES_CAPTURE].Klass == AH_PRES_PHYSICAL) ? TRUE : FALSE;

    UnitOf[AH_PRES_RENDER]  = (r && !rp) ? AH_UNIT_V : AH_UNIT_NONE;
    UnitOf[AH_PRES_CAPTURE] = (c && !cp) ? AH_UNIT_V : AH_UNIT_NONE;

    if (rp && cp &&
        Pres[AH_PRES_RENDER].Group != 0 &&
        Pres[AH_PRES_RENDER].Group == Pres[AH_PRES_CAPTURE].Group)
    {
        UnitOf[AH_PRES_RENDER]  = AH_UNIT_RC;
        UnitOf[AH_PRES_CAPTURE] = AH_UNIT_RC;
    }
    else
    {
        if (rp) { UnitOf[AH_PRES_RENDER]  = AH_UNIT_R; }
        if (cp) { UnitOf[AH_PRES_CAPTURE] = AH_UNIT_C; }
    }
}

//
// Builds the slot's wanted units (render's first) from its stored
// presentation, peer key and UnitOf[]. Caller holds the slot lock.
//
#pragma code_seg("PAGE")
static NTSTATUS
AhBuildSlotUnits(
    _Inout_ PAH_SLOT Slot
    )
{
    PAGED_CODE();

    RtlZeroMemory(Slot->Units, sizeof(Slot->Units));
    Slot->UnitCount = 0;

    for (ULONG idx = 0; idx < 2; idx++)
    {
        const ULONG unit = Slot->UnitOf[idx];
        if (unit == AH_UNIT_NONE)
        {
            continue;
        }
        BOOLEAN seen = FALSE;
        for (ULONG u = 0; u < Slot->UnitCount; u++)
        {
            if (Slot->Units[u].Unit == unit) { seen = TRUE; }
        }
        if (seen)
        {
            continue;
        }

        PAH_UNIT_SPEC spec = &Slot->Units[Slot->UnitCount];
        spec->Slot = (ULONG)(Slot - g_AhSlots);
        spec->Unit = unit;
        RtlCopyMemory(spec->PeerKey, Slot->PeerKey, AH_PEERKEY_BUF);
        if (Slot->UnitOf[AH_PRES_RENDER] == unit)  { spec->Directions |= AH_PUB_RENDER; }
        if (Slot->UnitOf[AH_PRES_CAPTURE] == unit) { spec->Directions |= AH_PUB_CAPTURE; }
        if (unit == AH_UNIT_R || unit == AH_UNIT_RC)
        {
            RtlCopyMemory(spec->KeyR, Slot->Pres[AH_PRES_RENDER].Key, AH_PRESKEY_BUF);
        }
        if (unit == AH_UNIT_C || unit == AH_UNIT_RC)
        {
            RtlCopyMemory(spec->KeyC, Slot->Pres[AH_PRES_CAPTURE].Key, AH_PRESKEY_BUF);
        }

        //
        // The devnode name: the first hosted direction's adapter name, render
        // first. A virtual unit without one keeps the peer label, so the
        // bracketed half is never empty and never shared by two peers (S6:
        // equal devnode names make Windows prefix the endpoints "N- ").
        //
        PCWSTR name = NULL;
        for (ULONG d = 0; d < 2 && name == NULL; d++)
        {
            if (Slot->UnitOf[d] == unit && Slot->Pres[d].AdapterName[0] != L'\0')
            {
                name = Slot->Pres[d].AdapterName;
            }
        }
        (VOID)RtlStringCchCopyW(spec->FriendlyName, AH_DISPLAY_CHARS,
                                (name != NULL) ? name : Slot->Display);

        NTSTATUS st = AhBusFinishUnitSpec(spec);
        if (!NT_SUCCESS(st))
        {
            RtlZeroMemory(Slot->Units, sizeof(Slot->Units));
            Slot->UnitCount = 0;
            return st;
        }
        Slot->UnitCount++;
    }
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
static BOOLEAN
AhSameUnit(
    _In_ const AH_UNIT_SPEC *A,
    _In_ const AH_UNIT_SPEC *B
    )
{
    PAGED_CODE();

    UNICODE_STRING a, b;
    RtlInitUnicodeString(&a, A->InstanceId);
    RtlInitUnicodeString(&b, B->InstanceId);
    return (A->Slot == B->Slot &&
            RtlEqualUnicodeString(&a, &b, TRUE) &&
            RtlEqualMemory(&A->ContainerId, &B->ContainerId, sizeof(GUID))) ? TRUE : FALSE;
}

//
// The wanted unit spec of one direction, NULL when the direction is not
// wanted. Caller holds the slot lock.
//
#pragma code_seg("PAGE")
static const AH_UNIT_SPEC *
AhSlotUnitOf(
    _In_ const AH_SLOT *Slot,
    _In_ ULONG          Index
    )
{
    PAGED_CODE();

    for (ULONG u = 0; u < Slot->UnitCount; u++)
    {
        if (Slot->Units[u].Unit == Slot->UnitOf[Index])
        {
            return &Slot->Units[u];
        }
    }
    return NULL;
}

//
// The running host child of one wanted direction, NULL while Plug and Play
// has not started it yet. Caller holds the slot lock.
//
#pragma code_seg("PAGE")
static PAH_HOST
AhFindHost(
    _In_ const AH_SLOT *Slot,
    _In_ ULONG          Index
    )
{
    PAGED_CODE();

    const AH_UNIT_SPEC *want = AhSlotUnitOf(Slot, Index);
    if (want == NULL)
    {
        return NULL;
    }
    for (ULONG h = 0; h < AH_BUS_MAX_CHILDREN; h++)
    {
        if (g_AhHosts[h].Pdo != NULL && AhSameUnit(&g_AhHosts[h].Spec, want))
        {
            return &g_AhHosts[h];
        }
    }
    return NULL;
}

//
// What a Set (or AH_EVENT_PUBLISHED) reports about a bound slot beyond the
// published mask.
//
#pragma code_seg("PAGE")
static ULONG
AhSlotReplyFlags(
    _In_ const AH_SLOT *Slot
    )
{
    PAGED_CODE();

    ULONG flags = 0;
    if (Slot->State != AH_SLOT_BOUND)
    {
        return 0;
    }
    if (Slot->NameFallback)                                    { flags |= AH_BINDREPLY_FLAG_NAME_FALLBACK; }
    if (Slot->Pres[AH_PRES_RENDER].Klass  == AH_PRES_PHYSICAL) { flags |= AH_BINDREPLY_FLAG_RENDER_PHYSICAL; }
    if (Slot->Pres[AH_PRES_CAPTURE].Klass == AH_PRES_PHYSICAL) { flags |= AH_BINDREPLY_FLAG_CAPTURE_PHYSICAL; }
    return flags;
}

//
// v11: tell the daemon the slot's published mask moved outside a Set.
// Caller holds the slot lock. AhCtlRaiseEvent takes only a spin lock.
//
#pragma code_seg("PAGE")
static VOID
AhRaisePublishedLocked(
    _In_ ULONG Slot
    )
{
    PAGED_CODE();

    const AH_SLOT *s = &g_AhSlots[Slot];
    AhCtlRaiseEvent(AH_EVENT_PUBLISHED, Slot, s->Generation, AhSlotReplyFlags(s),
                    AhSlotPublishedMask(s), s->State);
}

// Install exactly one requested direction on its host child. The minipairs
// are prepared before this is called; the endpoint name and identity are
// written here, into the host's interface, immediately before the filters
// exist. Keeping this operation directional is what lets a capability change
// remove a microphone without cycling the peer's still-selected speaker.
static NTSTATUS
AhInstallSlotDirection(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    Direction,
    _In_    PAH_HOST Host,
    _In_    ULONG    Flags,
    _Out_   PULONG   Stage
    )
{
    PAGED_CODE();

    const BOOLEAN render = (Direction == AH_PUB_RENDER);
    const ULONG idx = AH_DIR_INDEX(Direction);
    const ULONG slot = (ULONG)(Slot - g_AhSlots);

    *Stage = AH_STAGE_NONE;

    // Per-slot rings are immortal across endpoint publication changes. Start
    // each restored direction empty so it cannot replay samples queued before
    // that capability was withdrawn.
    AhRingsResetDirection(slot, render ? AUDIOHUB_DIR_OUT : AUDIOHUB_DIR_IN);
    if ((render && (Flags & AH_BINDFLAG_FAIL_RENDER)) ||
        (!render && (Flags & AH_BINDFLAG_FAIL_CAPTURE)))
    {
        *Stage = render ? AH_STAGE_INSTALL_RENDER : AH_STAGE_INSTALL_CAPTURE;
        return STATUS_UNSUCCESSFUL;
    }

    //
    // The host is recorded BEFORE anything is written into it, so a failure
    // anywhere below can be cleaned up through AhRemoveSlotDirections.
    //
    Slot->HostPdo[idx]     = Host->Pdo;
    Slot->HostAdapter[idx] = Host->Adapter;

    //
    // The bracketed half of the endpoint name is the host devnode's name, so
    // the endpoint and the device in Settings say the same thing (S6). Copied
    // into the slot so the buffer PortCls reads is ours and stable.
    //
    {
        PCWSTR src = (Host->Spec.FriendlyName[0] != L'\0') ? Host->Spec.FriendlyName : Slot->Display;
        (VOID)RtlStringCchCopyW(Slot->IfName[idx], AH_DISPLAY_CHARS, src);
        Slot->IfName[idx][AH_DISPLAY_CHARS - 1] = L'\0';
        size_t chars = 0;
        (VOID)RtlStringCchLengthW(Slot->IfName[idx], AH_DISPLAY_CHARS, &chars);
        SIMPLEAUDIOSAMPLE_DEVPROPERTY *prop = render ? &Slot->OutTopoProps[0] : &Slot->InTopoProps[0];
        prop->Buffer     = Slot->IfName[idx];
        prop->BufferSize = (ULONG)((chars + 1) * sizeof(WCHAR));
    }

    NTSTATUS status = AhApplyEndpointNames(Slot, Direction, Host->Pdo, Flags);
    if (!NT_SUCCESS(status))
    {
        *Stage = AH_STAGE_ENDPOINT_NAME;
        return status;
    }

    if (render)
    {
        status = Host->Adapter->InstallEndpointFilters(
            NULL, &Slot->OutPair, &Slot->OutCtx,
            &Slot->OutTopo, &Slot->OutWave, NULL, &Slot->OutWaveMiniport);
        if (NT_SUCCESS(status) && (Slot->OutTopo == NULL || Slot->OutWave == NULL))
        {
            *Stage = AH_STAGE_VERIFY;
            return STATUS_UNSUCCESSFUL;
        }
        *Stage = NT_SUCCESS(status) ? AH_STAGE_NONE : AH_STAGE_INSTALL_RENDER;
    }
    else
    {
        status = Host->Adapter->InstallEndpointFilters(
            NULL, &Slot->InPair, &Slot->InCtx,
            &Slot->InTopo, &Slot->InWave, NULL, NULL);
        if (NT_SUCCESS(status) && (Slot->InTopo == NULL || Slot->InWave == NULL))
        {
            *Stage = AH_STAGE_VERIFY;
            return STATUS_UNSUCCESSFUL;
        }
        *Stage = NT_SUCCESS(status) ? AH_STAGE_NONE : AH_STAGE_INSTALL_CAPTURE;
    }
    return status;
}

//
// Removes the selected directions from their host children.
//
// ClearNames: TRUE for every INTENTIONAL removal (unpair, presentation or
// unit change, capability withdrawn) -- the name and identity values go after
// the endpoints, while the host is still alive. FALSE when the host itself is
// going away (child detach): the values stay with that devnode's interface
// and are rewritten if it starts again.
//
// The ORDER inside RemoveEndpointFilters (disconnect topologies, then
// unregister wave, then unregister topology) is fixed by
// CAdapterCommon::RemoveEndpointFilters and must not be second-guessed here.
// Returns the FIRST failure; the port references are released either way --
// a failed unregister is a leak inside PortCls, and holding our reference on
// top of it would add a second, larger one.
//
static NTSTATUS
AhRemoveSlotDirections(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    Directions,
    _In_    ULONG    DebugFlags,
    _In_    BOOLEAN  ClearNames,
    _Out_   PULONG   FailStage
    )
{
    PAGED_CODE();

    NTSTATUS first = STATUS_SUCCESS;
    ULONG stage = AH_STAGE_NONE;
    const ULONG slot = (ULONG)(Slot - g_AhSlots);

    for (ULONG direction = AH_PUB_RENDER; direction <= AH_PUB_CAPTURE; direction <<= 1)
    {
        if ((Directions & direction) == 0)
        {
            continue;
        }
        const BOOLEAN render = (direction == AH_PUB_RENDER);
        const ULONG idx = AH_DIR_INDEX(direction);
        PADAPTERCOMMON adapter = Slot->HostAdapter[idx];
        PDEVICE_OBJECT pdo     = Slot->HostPdo[idx];
        PUNKNOWN *topo = render ? &Slot->OutTopo : &Slot->InTopo;
        PUNKNOWN *wave = render ? &Slot->OutWave : &Slot->InWave;

        if (render)
        {
            AhSpeakerPauseLocked(slot);
        }
        if (*topo != NULL || *wave != NULL)
        {
            if (adapter != NULL)
            {
                ULONG at = AH_STAGE_NONE;
                NTSTATUS st = adapter->RemoveEndpointFilters(
                    render ? &Slot->OutPair : &Slot->InPair, *topo, *wave, DebugFlags, &at);
                if (!NT_SUCCESS(st) && NT_SUCCESS(first)) { first = st; stage = at; }
                SAFE_RELEASE(*topo);
                SAFE_RELEASE(*wave);
                if (render) { SAFE_RELEASE(Slot->OutWaveMiniport); }
            }
            else
            {
                //
                // Unreachable by construction (ports are only ever held with a
                // host recorded). If it ever happens the adapter that owned the
                // ports is gone; touching them would be a use-after-free.
                //
                DPF(D_ERROR, ("[AhRemoveSlotDirections] slot %u dir 0x%x held without a host",
                              slot, direction));
                *topo = NULL;
                *wave = NULL;
                if (render) { Slot->OutWaveMiniport = NULL; }
            }
        }
        // RemoveEndpointFilters has stopped every WaveRT callback. Reset at
        // this quiet boundary, not before it, or a final callback could refill
        // the otherwise immortal per-slot ring with withdrawn audio.
        AhRingsResetDirection(slot, render ? AUDIOHUB_DIR_OUT : AUDIOHUB_DIR_IN);

        if (pdo != NULL)
        {
            if (ClearNames)
            {
                AhRemoveEndpointNames(Slot, direction, pdo);
            }
            else
            {
                // The values stay with the departing devnode; we can no longer
                // reach them, so nothing here claims to own them any more.
                Slot->NamesWritten[idx] = FALSE;
            }
        }
        Slot->HostPdo[idx]     = NULL;
        Slot->HostAdapter[idx] = NULL;
    }
    *FailStage = stage;
    return first;
}

//
// Installs every wanted direction whose host child is running and that is not
// held yet. A failure is cleaned up per direction (never the surviving one)
// unless AH_BINDFLAG_SKIP_ROLLBACK asks to leave it visible, and recorded in
// LastStage/LastNtStatus. Returns the first failure. Caller holds the lock.
//
#pragma code_seg("PAGE")
static NTSTATUS
AhInstallWaitingDirections(
    _Inout_ PAH_SLOT Slot,
    _In_opt_ PAH_HOST OnlyHost,
    _Out_   PULONG   FailStage
    )
{
    PAGED_CODE();

    NTSTATUS first = STATUS_SUCCESS;
    *FailStage = AH_STAGE_NONE;
    if (Slot->State != AH_SLOT_BOUND)
    {
        return STATUS_SUCCESS;
    }

    const ULONG want = AhWantedPublishedMask(Slot->Flags);
    for (ULONG direction = AH_PUB_RENDER; direction <= AH_PUB_CAPTURE; direction <<= 1)
    {
        if ((want & direction) == 0 || (AhSlotHeldMask(Slot) & direction) != 0)
        {
            continue;
        }
        const ULONG idx = AH_DIR_INDEX(direction);
        PAH_HOST host = AhFindHost(Slot, idx);
        if (host == NULL || (OnlyHost != NULL && host != OnlyHost))
        {
            continue;
        }
        ULONG stage = AH_STAGE_NONE;
        NTSTATUS st = AhInstallSlotDirection(Slot, direction, host, Slot->Flags, &stage);
        if (NT_SUCCESS(st))
        {
            continue;
        }
        DPF(D_ERROR, ("[AhInstallWaitingDirections] slot %u direction 0x%x failed 0x%x (stage %u)",
                      (ULONG)(Slot - g_AhSlots), direction, st, stage));
        if (!(Slot->Flags & AH_BINDFLAG_SKIP_ROLLBACK))
        {
            ULONG cleanupStage = AH_STAGE_NONE;
            NTSTATUS cleanup = AhRemoveSlotDirections(Slot, direction, 0, TRUE, &cleanupStage);
            if (!NT_SUCCESS(cleanup))
            {
                st = cleanup;
                stage = AH_STAGE_ROLLBACK;
            }
        }
        Slot->LastStage    = (stage == AH_STAGE_NONE) ? AH_STAGE_VERIFY : stage;
        Slot->LastNtStatus = st;
        if (NT_SUCCESS(first))
        {
            first = st;
            *FailStage = Slot->LastStage;
        }
    }
    return first;
}

//
// Takes a slot completely down: every direction off its host with names and
// identity cleared, every unit unplugged, the record emptied. Caller holds
// the lock. Returns the first removal failure.
//
#pragma code_seg("PAGE")
static NTSTATUS
AhTeardownSlot(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    DebugFlags,
    _Out_   PULONG   FailStage
    )
{
    PAGED_CODE();

    const ULONG slot = (ULONG)(Slot - g_AhSlots);
    NTSTATUS st = AhRemoveSlotDirections(Slot, AH_PUB_BOTH, DebugFlags, TRUE, FailStage);

    RtlZeroMemory(Slot->UnitOf, sizeof(Slot->UnitOf));
    RtlZeroMemory(Slot->Units, sizeof(Slot->Units));
    Slot->UnitCount = 0;
    (VOID)AhBusSetSlotUnits(slot, NULL, 0);

    Slot->State = AH_SLOT_FREE;
    Slot->Flags = 0;
    RtlZeroMemory(Slot->PeerKey, sizeof(Slot->PeerKey));
    RtlZeroMemory(Slot->Pres, sizeof(Slot->Pres));
    Slot->LastStage = AH_STAGE_NONE;
    Slot->LastNtStatus = STATUS_SUCCESS;
    AhSpeakerFormatSyncLocked(slot);
    return st;
}

//-----------------------------------------------------------------------------
// Lifecycle
//-----------------------------------------------------------------------------

//-----------------------------------------------------------------------------
// Data-plane routing and per-slot volume
//-----------------------------------------------------------------------------

#pragma code_seg()
BOOLEAN
AhEpContextDecode(
    _In_opt_ const void *DeviceContext,
    _Out_ PULONG Slot,
    _Out_ PBOOLEAN Input
    )
{
    const AH_EP_CONTEXT *ctx = (const AH_EP_CONTEXT *)DeviceContext;

    if (ctx == NULL || ctx->Magic != AH_EP_CONTEXT_MAGIC || ctx->Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return FALSE;
    }
    *Slot  = ctx->Slot;
    *Input = ctx->Input;
    return TRUE;
}

#pragma code_seg()
LONG
AhSlotVolumeGet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG Channel
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Channel >= AH_VOLUME_MAX_CHANNELS)
    {
        return AH_VOLUME_UNITY;
    }
    return Input ? g_AhSlots[Slot].VolumeIn[Channel] : g_AhSlots[Slot].VolumeOut[Channel];
}

#pragma code_seg()
BOOLEAN
AhSlotVolumeSet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG Channel,
    _In_ LONG Value
    )
/*++

Routine Description:

    Stores one channel's level and answers whether it CHANGED.

    The return value is the loop breaker for volume sync, and it belongs here
    rather than at either caller. The daemon pushes the peer's level in, the
    driver raises an event when the level moves, the daemon reads the event and
    pushes the level to the peer. Without an "it was already that" answer the
    two ends ratchet against each other forever, and the symptom is a slider
    that creeps rather than an error anyone would look for.

--*/
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Channel >= AH_VOLUME_MAX_CHANNELS)
    {
        return FALSE;
    }

    PLONG cell = Input ? &g_AhSlots[Slot].VolumeIn[Channel] : &g_AhSlots[Slot].VolumeOut[Channel];
    LONG prev = InterlockedExchange(cell, Value);
    return (prev != Value) ? TRUE : FALSE;
}

#pragma code_seg()
BOOLEAN
AhSlotMuteGet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG Channel
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Channel >= AH_VOLUME_MAX_CHANNELS)
    {
        return FALSE;
    }
    return Input ? g_AhSlots[Slot].MuteIn[Channel] : g_AhSlots[Slot].MuteOut[Channel];
}

#pragma code_seg()
BOOLEAN
AhSlotMuteSet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG Channel,
    _In_ BOOLEAN Value
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Channel >= AH_VOLUME_MAX_CHANNELS)
    {
        return FALSE;
    }

    PBOOLEAN cell = Input ? &g_AhSlots[Slot].MuteIn[Channel] : &g_AhSlots[Slot].MuteOut[Channel];
    BOOLEAN prev = *cell;
    *cell = Value;
    return (prev != Value) ? TRUE : FALSE;
}

#pragma code_seg()
ULONG
AhSlotLatencyGet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input
    )
/*++

Routine Description:

    How many frames of DOWNSTREAM latency this endpoint carries -- the interval
    between this driver accepting a frame and that frame being audible, which
    for an AudioHub endpoint spans a network and another machine's sound card.

    Zero means "never measured". It is NOT a claim that the endpoint is
    instantaneous; it is the absence of a claim, which is the only honest thing
    to report before the daemon has measured the chain.

--*/
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return 0;
    }
    LONG v = Input ? g_AhSlots[Slot].LatencyFramesIn : g_AhSlots[Slot].LatencyFramesOut;
    return (v > 0) ? (ULONG)v : 0;
}

#pragma code_seg()
BOOLEAN
AhSlotLatencySet(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG Frames
    )
/*++

Routine Description:

    Stores the downstream latency for one endpoint. FALSE means nothing was
    stored -- an unknown slot, or a value past AH_LATENCY_MAX_FRAMES.

    The ceiling is not a policy about plausible links (the daemon owns that); it
    bounds what one corrupted word can do to the clock the audio engine derives
    from GetPresentationPosition. It is generous on purpose: macOS AirPlay
    declares 2.0 s in the equivalent place, so a "reasonable" ceiling here would
    be a second, undocumented policy silently overruling the first.

    Streams already running are UNAFFECTED: each captured its own copy when it
    was created and holds it until it stops. A presentation clock whose offset
    moves can appear to run backwards, and monotonicity is the one property
    u64PositionInBlocks may never lose.

--*/
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Frames > AH_LATENCY_MAX_FRAMES)
    {
        return FALSE;
    }

    PLONG cell = Input ? &g_AhSlots[Slot].LatencyFramesIn : &g_AhSlots[Slot].LatencyFramesOut;
    InterlockedExchange(cell, (LONG)Frames);
    return TRUE;
}

#pragma code_seg()
ULONG
AhSlotGeneration(
    _In_ ULONG Slot
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return 0;
    }
    return g_AhSlots[Slot].Generation;
}

//-----------------------------------------------------------------------------
// Volume unit conversion
//-----------------------------------------------------------------------------

//
// KS carries volume as a signed LONG in 1/65536 dB; the daemon and every peer
// carry it as a 0..1 amplitude scalar. Both directions of the conversion live
// here so there is exactly ONE mapping in the driver.
//
// dB = 20*log10(scalar), which cannot be computed here -- kernel code must not
// touch the FPU at raised IRQL, and these are reachable from the property
// handler. So it is a table with integer interpolation. WHAT the table is over
// is the part that matters, and the part the first version got wrong.
//
// A table spread over the SCALAR cannot work. 20*log10(s) runs to negative
// infinity as s approaches zero, so the bottom segment -- scalar 0 to 1/32 --
// must carry everything from the -96 dB floor up to -30.1 dB on one straight
// line. At scalar 0.0042 (-47.5 dB, an unremarkable setting) that line reads
// -87.3 dB: 39.9 dB of error, worst exactly where the curve is steepest, which
// is where a volume control does its work.
//
// Splitting the power of two off first removes the singularity. Every scalar is
// 2^exp * m with m in [1,2), so dB = exp*AH_DB_PER_OCTAVE_Q16 + f(m), and only
// f needs tabulating -- over one octave, where it is gentle. Same 33 entries,
// same integer-only arithmetic, and the error becomes UNIFORM across the whole
// range instead of exploding at the bottom: worst case 0.00104 dB.
//
// WHAT IS NOW CHECKED, AND BY WHAT. The mirror of this mapping lives in
// core/audiohubd/src/halbridge_win.rs (mod volmap), operation for operation
// including every rounding step, and its tests measure it against 20*log10 on
// all 65536 inputs. test/tests/halwire_win.rs reads THIS TABLE out of THIS FILE
// and asserts it equals the Rust one, so the two cannot drift and neither can
// be edited alone. That is the check the previous version of this comment asked
// for and did not get; when it was finally run against the shipped table, 31 of
// the 32 entries disagreed with the dB value written on their own line.
//
// WHAT IS STILL NOT CHECKED. All of the above establishes that this is the
// function it claims to be. It does NOT establish the bigger claim -- that the
// level the Windows audio engine derives from this KS node equals the scalar
// the peer applies to its real device. That is a statement about the engine's
// own curve, is undocumented to this precision, and can only be settled by
// reading GetMasterVolumeLevelScalar back off a real endpoint fed by a real
// installed driver. THAT MEASUREMENT HAS NOT BEEN MADE.
//

//
// 20*log10(2) * 65536, rounded. One octave -- one halving of the scalar.
//
#define AH_DB_PER_OCTAVE_Q16    394566

//
// 20*log10(1 + i/32) * 65536 for i = 0..32: the mantissa's contribution alone.
// Entry 32 is one octave exactly; if it ever stops being AH_DB_PER_OCTAVE_Q16
// the mapping is discontinuous at every power of two.
//
static const LONG g_AhMantissaDbTable[33] = {
         0,   17516,   34510,   51011,   67047,   82643,   97824,  112610,
    127022,  141078,  154795,  168190,  181276,  194069,  206580,  218822,
    230806,  242544,  254044,  265316,  276370,  287213,  297853,  308298,
    318555,  328630,  338530,  348261,  357828,  367237,  376493,  385601,
    AH_DB_PER_OCTAVE_Q16
};

#pragma code_seg()
LONG
AhScalarQ16ToKsVolume(
    _In_ ULONG ScalarQ16
    )
/*++

Routine Description:

    Scalar (16.16) -> KSPROPERTY_AUDIO_VOLUMELEVEL (1/65536 dB).

    Silence is the one input the formula cannot express: scalar 0 is dB
    negative infinity, which no LONG holds. It maps to the advertised floor,
    and AhKsVolumeToScalarQ16 maps that floor back to 0, so silence survives a
    round trip. What -96 dB is NOT is silent -- it is amplitude 1.6e-5. Muting
    is KSPROPERTY_AUDIO_MUTE, a separate store with its own event flag, for
    exactly this reason.

    The result never leaves [VOLUME_SIGNED_MINIMUM, VOLUME_SIGNED_MAXIMUM],
    which is the range this driver publishes through KSPROPERTY_MEMBERSLIST in
    kshelper.cpp. Applying the formula unclamped would not: it is unbounded
    below and the endpoint's floor is -96 dB.

--*/
{
    ULONG bit, norm, frac, idx, sub;
    LONG  exp, lo, hi, interp, db;

    if (ScalarQ16 == 0)
    {
        return VOLUME_SIGNED_MINIMUM;
    }
    if (ScalarQ16 >= 0x10000u)
    {
        return AH_VOLUME_UNITY;
    }

    //
    // scalar = 2^exp * (1 + frac/32768), with exp in -16..-1. The index of the
    // top set bit, by shifting: at most 15 iterations on a path that runs when
    // a human moves a slider. BitScanReverse would be one instruction, but it
    // is an intrinsic this tree does not otherwise use and cannot be confirmed
    // declared without a WDK build -- not a trade worth making here.
    //
    bit = 0;
    {
        ULONG v = ScalarQ16 >> 1;               // ScalarQ16 != 0, checked above
        while (v != 0) { v >>= 1; bit++; }      // 0..15
    }
    exp  = (LONG)bit - 16;
    norm = ScalarQ16 << (15 - bit);             // 0x8000..0xFFFF
    frac = norm & 0x7FFFu;

    idx = frac >> 10;                           // 0..31
    sub = frac & 0x3FFu;                        // 0..1023
    lo  = g_AhMantissaDbTable[idx];
    hi  = g_AhMantissaDbTable[idx + 1];

    //
    // Round, not truncate: see the note on the inverse. (hi-lo) <= 17516 and
    // sub <= 1023, so the product is under 2^25 and stays in a LONG.
    //
    interp = lo + (((hi - lo) * (LONG)sub + 512) >> 10);

    db = exp * AH_DB_PER_OCTAVE_Q16 + interp;
    return (db < VOLUME_SIGNED_MINIMUM) ? VOLUME_SIGNED_MINIMUM : db;
}

#pragma code_seg()
ULONG
AhKsVolumeToScalarQ16(
    _In_ LONG Level
    )
/*++

Routine Description:

    KSPROPERTY_AUDIO_VOLUMELEVEL (1/65536 dB) -> scalar (16.16).

    Inverts the table by searching it rather than carrying a second one: 33
    comparisons at property-set rate is nothing, and a second table is a second
    thing that can fall out of step with the first.

    THE FINAL SHIFT ROUNDS. That is load-bearing, not tidiness. Truncating
    loses a count on every pass, and the two ends of volume sync pass values
    back and forth: this driver raises an event, the daemon relays it, the
    daemon pushes a level back, this driver stores it. With truncation that
    loop walks the volume DOWN one count per exchange for any scalar below
    about -69 dB -- a slider that creeps toward silence with nobody touching
    it. The guard is `the_sync_loop_reaches_a_fixed_point_in_one_settle` in
    the Rust mirror, and it fails on the truncating form.

--*/
{
    LONG  exp, rem, lo, hi, sub, shift;
    ULONG f = 0, i, norm;

    if (Level >= AH_VOLUME_UNITY)
    {
        return 0x10000u;
    }
    if (Level <= VOLUME_SIGNED_MINIMUM)
    {
        return 0;
    }

    //
    // Floor division, not truncation: Level is negative and the octave index
    // has to round DOWN so the remainder stays in [0, one octave).
    //
    exp = Level / AH_DB_PER_OCTAVE_Q16;
    rem = Level - exp * AH_DB_PER_OCTAVE_Q16;
    if (rem < 0)
    {
        exp -= 1;
        rem += AH_DB_PER_OCTAVE_Q16;
    }

    for (i = 32; i > 0; i--)
    {
        if (rem >= g_AhMantissaDbTable[i - 1])
        {
            lo = g_AhMantissaDbTable[i - 1];
            hi = g_AhMantissaDbTable[i];
            sub = (hi == lo) ? 0 : (((rem - lo) * 1024 + (hi - lo) / 2) / (hi - lo));
            if (sub > 1023) { sub = 1023; }
            f = ((i - 1) << 10) + (ULONG)sub;
            break;
        }
    }

    shift = -1 - exp;                           // 0..15 over the live range
    if (shift < 0)  { return 0x10000u; }
    if (shift > 16) { return 0; }

    norm = 0x8000u + f;                         // 0x8000..0xFFFF
    if (shift == 0)
    {
        return norm;
    }
    return (norm + (1u << (shift - 1))) >> shift;
}

//-----------------------------------------------------------------------------
// Volume change events (driver -> daemon)
//-----------------------------------------------------------------------------

//
// One registered topology miniport per slot per direction. Registered when the
// miniport initialises and cleared when it goes away, both under the spin lock
// so that a NOTIFY arriving while an endpoint is being torn down cannot raise
// an event on a released object.
//
static KSPIN_LOCK g_AhTopoLock;
static PVOID      g_AhTopoObj[AUDIOHUB_WIN_MAX_SLOTS][2];

#pragma code_seg()
VOID
AhTopoRegister(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_opt_ PVOID Topology
    )
{
    KIRQL irql;

    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return;
    }

    KeAcquireSpinLock(&g_AhTopoLock, &irql);
    g_AhTopoObj[Slot][Input ? 1 : 0] = Topology;
    KeReleaseSpinLock(&g_AhTopoLock, irql);
}

#pragma code_seg()
VOID
AhTopoUnregister(
    _In_ PVOID Topology
    )
{
    KIRQL irql;

    KeAcquireSpinLock(&g_AhTopoLock, &irql);
    for (ULONG s = 0; s < AUDIOHUB_WIN_MAX_SLOTS; s++)
    {
        for (ULONG d = 0; d < 2; d++)
        {
            if (g_AhTopoObj[s][d] == Topology)
            {
                g_AhTopoObj[s][d] = NULL;
            }
        }
    }
    KeReleaseSpinLock(&g_AhTopoLock, irql);
}

#pragma code_seg()
PVOID
AhTopoLookup(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input
    )
{
    KIRQL irql;
    PVOID obj = NULL;

    if (Slot < AUDIOHUB_WIN_MAX_SLOTS)
    {
        KeAcquireSpinLock(&g_AhTopoLock, &irql);
        obj = g_AhTopoObj[Slot][Input ? 1 : 0];
        KeReleaseSpinLock(&g_AhTopoLock, irql);
    }
    return obj;
}

//-----------------------------------------------------------------------------
// WaveRT buffer residency (the SECOND buffering stage)
//-----------------------------------------------------------------------------

//
// WHY THIS EXISTS AS A SEPARATE NUMBER
//
// Windows puts TWO buffers in series where macOS has one: the WaveRT circular
// buffer the audio engine shares with this driver, and the AudioHub ring the
// driver shares with the daemon. spec-windows-driver.md:574 forbids reporting
// them as one figure, and the reason is not tidiness -- the two stages fail
// differently and are fixed differently. A single "12 ms" cannot say whether
// the engine is handing us packets late (WaveRT deep, ring empty: nothing the
// trim algorithm can do) or the daemon is draining slowly (WaveRT shallow, ring
// deep: exactly what trim exists for). The daemon already measures the ring
// stage from its own side (halbridge_win.rs spk_readable / mic_occupied); only
// the driver can see this one.
//
// Written from UpdatePosition, which runs at DISPATCH_LEVEL inside the stream's
// position spin lock, and read from an IOCTL at PASSIVE. That rules out a lock:
// taking g_AhTopoLock-style protection here would put an IOCTL thread and the
// DPC on the same lock at audio rate. Each field is a LONG written with
// InterlockedExchange instead, so a reader sees whole fields; `updates` is
// bumped last and is what makes a torn or stale SET detectable rather than
// merely improbable.
//

typedef struct _AH_WAVERT_SNAPSHOT {
    volatile LONG present;          // 1 while a stream holds this slot+direction
    volatile LONG buffer_bytes;     // WaveRT buffer size
    volatile LONG resident_bytes;   // queued in THIS stage right now
    volatile LONG frame_bytes;      // so the reader can convert to frames/ms
    volatile LONG sample_rate;
    volatile LONG packet_bytes;     // 0 when the client is not packet-driven
    volatile LONG updates;
} AH_WAVERT_SNAPSHOT;

static AH_WAVERT_SNAPSHOT g_AhWaveRt[AUDIOHUB_WIN_MAX_SLOTS][2];

#pragma code_seg()
VOID
AhWaveRtPublish(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _In_ ULONG BufferBytes,
    _In_ ULONG ResidentBytes,
    _In_ ULONG FrameBytes,
    _In_ ULONG SampleRate,
    _In_ ULONG PacketBytes
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return;
    }

    AH_WAVERT_SNAPSHOT *snap = &g_AhWaveRt[Slot][Input ? 1 : 0];

    //
    // Clamped rather than trusted. ResidentBytes is derived from two positions
    // that a stream reset can move independently, and a wrapped subtraction
    // would otherwise publish a residency of nearly 4 GB -- which the daemon
    // would convert into a latency figure and act on.
    //
    if (BufferBytes != 0 && ResidentBytes > BufferBytes)
    {
        ResidentBytes = BufferBytes;
    }

    InterlockedExchange(&snap->buffer_bytes,   (LONG)BufferBytes);
    InterlockedExchange(&snap->resident_bytes, (LONG)ResidentBytes);
    InterlockedExchange(&snap->frame_bytes,    (LONG)FrameBytes);
    InterlockedExchange(&snap->sample_rate,    (LONG)SampleRate);
    InterlockedExchange(&snap->packet_bytes,   (LONG)PacketBytes);
    InterlockedExchange(&snap->present,        1);
    InterlockedIncrement(&snap->updates);
}

#pragma code_seg()
VOID
AhWaveRtClear(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input
    )
{
    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return;
    }

    AH_WAVERT_SNAPSHOT *snap = &g_AhWaveRt[Slot][Input ? 1 : 0];

    //
    // present goes to 0 and the numbers go with it. "No stream" must not read
    // as "0 ms resident": one is a stage that does not exist right now and the
    // other is a stage that is empty, and a daemon that confused them would
    // report a healthy latency for a device nothing is playing to.
    //
    InterlockedExchange(&snap->present,        0);
    InterlockedExchange(&snap->buffer_bytes,   0);
    InterlockedExchange(&snap->resident_bytes, 0);
    InterlockedExchange(&snap->packet_bytes,   0);
    InterlockedIncrement(&snap->updates);
}

#pragma code_seg()
BOOLEAN
AhWaveRtSample(
    _In_ ULONG Slot,
    _In_ BOOLEAN Input,
    _Out_ ULONG *BufferBytes,
    _Out_ ULONG *ResidentBytes,
    _Out_ ULONG *FrameBytes,
    _Out_ ULONG *SampleRate,
    _Out_ ULONG *PacketBytes,
    _Out_ ULONG *Updates
    )
{
    *BufferBytes = *ResidentBytes = *FrameBytes = 0;
    *SampleRate  = *PacketBytes   = *Updates    = 0;

    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        return FALSE;
    }

    AH_WAVERT_SNAPSHOT *snap = &g_AhWaveRt[Slot][Input ? 1 : 0];

    *Updates       = (ULONG)InterlockedCompareExchange(&snap->updates, 0, 0);
    *BufferBytes   = (ULONG)InterlockedCompareExchange(&snap->buffer_bytes, 0, 0);
    *ResidentBytes = (ULONG)InterlockedCompareExchange(&snap->resident_bytes, 0, 0);
    *FrameBytes    = (ULONG)InterlockedCompareExchange(&snap->frame_bytes, 0, 0);
    *SampleRate    = (ULONG)InterlockedCompareExchange(&snap->sample_rate, 0, 0);
    *PacketBytes   = (ULONG)InterlockedCompareExchange(&snap->packet_bytes, 0, 0);

    return InterlockedCompareExchange(&snap->present, 0, 0) != 0;
}

#pragma code_seg("PAGE")
VOID
AhPerPeerDriverInit(VOID)
{
    PAGED_CODE();

    AhSpeakerFormatBanksInitialize();
    RtlZeroMemory(g_AhSlots, sizeof(g_AhSlots));
    RtlZeroMemory(g_AhSpeaker, sizeof(g_AhSpeaker));
    RtlZeroMemory(g_AhHosts, sizeof(g_AhHosts));
    for (ULONG slot = 0; slot < AUDIOHUB_WIN_MAX_SLOTS; ++slot)
    {
        KeInitializeSpinLock(&g_AhSpeaker[slot].Gate);
        g_AhSpeaker[slot].Bank = AhSpeakerFormatBankLookup(AH_SPEAKER_LAYOUT_MASK_STEREO);
    }
    RtlZeroMemory(g_AhTopoObj, sizeof(g_AhTopoObj));
    RtlZeroMemory((PVOID)g_AhWaveRt, sizeof(g_AhWaveRt));
    KeInitializeSpinLock(&g_AhTopoLock);
    KeInitializeMutex(&g_AhSlotLock, 0);
    AhBuildTopologyVariants();
    g_AhNextGeneration = 1;
    g_AhDirWordsOk = FALSE;
    g_AhInitialised = TRUE;
}

#pragma code_seg("PAGE")
NTSTATUS
AhPerPeerChildStart(
    _In_ PDEVICE_OBJECT Fdo,
    _In_ PADAPTERCOMMON Adapter
    )
{
    PAGED_CODE();

    ASSERT(g_AhInitialised);

    PDEVICE_OBJECT pdo = Adapter->GetPhysicalDeviceObject();
    AH_UNIT_SPEC spec;
    if (pdo == NULL || !AhBusChildSpec(pdo, &spec))
    {
        //
        // Only bus children are adapters since v11. Not failing: a devnode
        // that cannot start is worse than one that publishes nothing.
        //
        DPF(D_ERROR, ("[AhPerPeerChildStart] FDO %p is not on an AudioHub child", Fdo));
        return STATUS_SUCCESS;
    }

    AH_LOCK();

    PAH_HOST host = NULL;
    for (ULONG h = 0; h < AH_BUS_MAX_CHILDREN; h++)
    {
        if (g_AhHosts[h].Pdo == pdo)
        {
            // A restart without a detach would be a PnP ordering we never
            // expect; take the newest adapter rather than a dangling one.
            host = &g_AhHosts[h];
            break;
        }
    }
    for (ULONG h = 0; host == NULL && h < AH_BUS_MAX_CHILDREN; h++)
    {
        if (g_AhHosts[h].Pdo == NULL)
        {
            host = &g_AhHosts[h];
        }
    }
    if (host == NULL)
    {
        AH_UNLOCK();
        DPF(D_ERROR, ("[AhPerPeerChildStart] host table full; %S publishes nothing",
                      spec.InstanceId));
        return STATUS_SUCCESS;
    }
    host->Pdo     = pdo;
    host->Fdo     = Fdo;
    host->Adapter = Adapter;
    host->Spec    = spec;

    if (!g_AhDirWordsOk)
    {
        //
        // The generic fallback labels, read back out of the INF's own static
        // MediaCategories entries. They remain the fallback if writing the
        // per-peer name fails; the happy path does not use them. NOT fatal.
        //
        NTSTATUS o = AhReadPinNameValue(pdo, &AH_PIN_NAME_OUT, g_AhDirWordOut, AH_DIRWORD_CHARS);
        NTSTATUS i = AhReadPinNameValue(pdo, &AH_PIN_NAME_IN,  g_AhDirWordIn,  AH_DIRWORD_CHARS);
        g_AhDirWordsOk = (BOOLEAN)(NT_SUCCESS(o) && NT_SUCCESS(i));
        if (!g_AhDirWordsOk)
        {
            DPF(D_ERROR, ("[AhPerPeerChildStart] INF fallback names unreadable (0x%x / 0x%x)", o, i));
        }
    }

    //
    // Every direction that was waiting for exactly this child goes in now.
    // The names were decided at the Set; they are written into this child's
    // interfaces immediately before each filter exists.
    //
    ULONG slot = spec.Slot;
    if (slot < AUDIOHUB_WIN_MAX_SLOTS)
    {
        PAH_SLOT s = &g_AhSlots[slot];
        ULONG before = AhSlotPublishedMask(s);
        ULONG stage = AH_STAGE_NONE;
        (VOID)AhInstallWaitingDirections(s, host, &stage);
        ULONG after = AhSlotPublishedMask(s);
        DPF(D_TERSE, ("[AhPerPeerChildStart] %S slot %u published 0x%x -> 0x%x",
                      spec.InstanceId, slot, before, after));
        if (after != before)
        {
            AhSpeakerFormatSyncLocked(slot);
            AhRaisePublishedLocked(slot);
        }
    }

    AH_UNLOCK();
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
VOID
AhPerPeerChildDetach(
    _In_ PDEVICE_OBJECT Fdo
    )
{
    PAGED_CODE();

    if (!g_AhInitialised)
    {
        return;
    }

    AH_LOCK();

    PAH_HOST host = NULL;
    for (ULONG h = 0; h < AH_BUS_MAX_CHILDREN; h++)
    {
        if (g_AhHosts[h].Pdo != NULL && g_AhHosts[h].Fdo == Fdo)
        {
            host = &g_AhHosts[h];
            break;
        }
    }
    if (host == NULL)
    {
        AH_UNLOCK();
        return;
    }

    //
    // Every direction installed on this child goes FIRST, while the adapter
    // that owns the port objects is still alive. A slot still holding port
    // pointers after Cleanup()/Release() is a dangling reference, and the
    // bugcheck it produces names none of the code that caused it. The slot's
    // BINDING is untouched: the daemon still has this peer paired, and the
    // directions go back in when the child starts again.
    //
    for (ULONG slot = 0; slot < AUDIOHUB_WIN_MAX_SLOTS; slot++)
    {
        PAH_SLOT s = &g_AhSlots[slot];
        ULONG directions = 0;
        for (ULONG direction = AH_PUB_RENDER; direction <= AH_PUB_CAPTURE; direction <<= 1)
        {
            if (s->HostPdo[AH_DIR_INDEX(direction)] == host->Pdo)
            {
                directions |= direction;
            }
        }
        if (directions == 0)
        {
            continue;
        }
        ULONG stage = AH_STAGE_NONE;
        NTSTATUS st = AhRemoveSlotDirections(s, directions, 0, FALSE, &stage);
        if (!NT_SUCCESS(st))
        {
            DPF(D_ERROR, ("[AhPerPeerChildDetach] slot %u remove 0x%x failed 0x%x stage %u",
                          slot, directions, st, stage));
        }
        AhSpeakerFormatSyncLocked(slot);
        AhRaisePublishedLocked(slot);
    }

    DPF(D_TERSE, ("[AhPerPeerChildDetach] %S detached", host->Spec.InstanceId));
    RtlZeroMemory(host, sizeof(*host));

    AH_UNLOCK();
}

//-----------------------------------------------------------------------------
// Bind
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
NTSTATUS
AhSlotBindSet(
    _In_  ULONG   Slot,
    _In_z_ PCSTR  PeerKey,
    _In_  PCWSTR  Display,
    _In_reads_(2) const AH_PRESENTATION *Presentation,
    _In_  ULONG   Flags,
    _Out_ PULONG  Generation,
    _Out_ PULONG  State,
    _Out_ PULONG  AhStatus,
    _Out_ PAH_OP_RESULT Result
    )
/*++

Routine Description:

    Records exactly the endpoint directions requested for one peer, and
    publishes as many of them as can be published now.

    v11: every direction belongs to a presentation unit (AH_UNIT_*), and a
    unit is a child devnode the bus enumerates. The Set:

      1. takes down every held direction that must move -- not wanted any
         more, a presentation identity change (new reference strings), or a
         different unit (a different devnode or container) -- with its names;
      2. stores the new desired state and hands the slot's units to the bus,
         which plugs and unplugs child devnodes and asks Plug and Play to
         re-enumerate;
      3. installs, right here, every wanted direction whose unit's child is
         already running.

    What remains waits for its child to start (AhPerPeerChildStart), and is
    reported by AH_EVENT_PUBLISHED. The invariant this routine guarantees:

        AhStatus == AH_STATUS_OK      => Result->Published == wanted mask
        AhStatus == AH_STATUS_PENDING => Result->Published is a subset of it,
                                         nothing failed, the rest is coming

    A failed install or removal is AH_STATUS_PARTIAL, never PENDING. Unlike
    v10 a failed FRESH bind is not rolled back to a free slot: the pairing is
    the daemon's, the slot stays bound, and the next identical Set retries the
    failed direction. AH_BINDFLAG_SKIP_ROLLBACK still leaves a half-install
    visible instead of cleaning it.

    The generation moves only when what is wanted changes (fresh bind, mask,
    presentation identity or unit), never for an identical re-Set: the daemon
    re-Sets every second while a Set answers PENDING.

--*/
{
    PAGED_CODE();

    NTSTATUS status = STATUS_SUCCESS;

    *Generation = 0;
    *State      = AH_SLOT_FREE;
    *AhStatus   = AH_STATUS_INTERNAL;
    RtlZeroMemory(Result, sizeof(*Result));

    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        *AhStatus = AH_STATUS_CAPACITY;
        return STATUS_INVALID_PARAMETER;
    }

    SIZE_T keyLen = AhPeerKeyLength(PeerKey, AH_PEERKEY_BUF);
    if (!AhIsValidPeerKey(PeerKey, keyLen))
    {
        *AhStatus = AH_STATUS_BAD_ARGUMENT;
        return STATUS_INVALID_PARAMETER;
    }

    //
    // Validated and reduced BEFORE the lock and before anything changes, so a
    // malformed presentation is refused without touching the slot.
    //
    AH_SLOT_PRESENTATION pres[2];
    if (!AhNormalizePresentation(TRUE,  &Presentation[AH_PRES_RENDER],  &pres[AH_PRES_RENDER]) ||
        !AhNormalizePresentation(FALSE, &Presentation[AH_PRES_CAPTURE], &pres[AH_PRES_CAPTURE]))
    {
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u malformed presentation", Slot));
        Result->Stage = AH_STAGE_PRESENTATION;
        *AhStatus = AH_STATUS_BAD_ARGUMENT;
        return STATUS_INVALID_PARAMETER;
    }

    if (Flags & AH_BINDFLAG_DEBUG_MASK)
    {
        //
        // Loud on purpose. audiohubd never sets these; seeing one in a field
        // log means somebody was running the fault-injection harness.
        //
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u FAULT INJECTION flags 0x%x", Slot, Flags));
    }

    AH_LOCK();

    PAH_SLOT s = &g_AhSlots[Slot];
    const ULONG want = AhWantedPublishedMask(Flags);
    ULONG failStage = AH_STAGE_NONE;
    NTSTATUS failStatus = STATUS_SUCCESS;

    if (!AhBusReady())
    {
        *AhStatus = AH_STATUS_NO_ADAPTER;
        status = STATUS_DEVICE_NOT_READY;
        goto Done;
    }

    if (s->State == AH_SLOT_BOUND && strncmp(s->PeerKey, PeerKey, AH_PEERKEY_BUF) != 0)
    {
        //
        // Same slot, different peer. The daemon is supposed to Clear first;
        // doing it here keeps the driver's table authoritative either way.
        //
        DPF(D_TERSE, ("[AhSlotBindSet] slot %u re-targeted; removing previous peer", Slot));
        ULONG tdStage = AH_STAGE_NONE;
        (VOID)AhTeardownSlot(s, 0, &tdStage);
    }

    const BOOLEAN fresh = (s->State != AH_SLOT_BOUND) ? TRUE : FALSE;
    BOOLEAN changed = fresh;

    ULONG unitOf[2];
    AhComputeUnits(pres, want, unitOf);

    if (!fresh)
    {
        //
        // Same peer. Decide which held directions must come down before the
        // new state is stored -- their names live under the CURRENT reference
        // strings on the CURRENT host.
        //
        const ULONG held = AhSlotHeldMask(s);
        const ULONG have = AhSlotPublishedMask(s);
        const ULONG oldWant = AhWantedPublishedMask(s->Flags);

        ULONG rebuild = 0;
        if (!AhSamePresentationIdentity(&s->Pres[AH_PRES_RENDER], &pres[AH_PRES_RENDER]))
        {
            rebuild |= AH_PUB_RENDER;
        }
        if (!AhSamePresentationIdentity(&s->Pres[AH_PRES_CAPTURE], &pres[AH_PRES_CAPTURE]))
        {
            rebuild |= AH_PUB_CAPTURE;
        }

        //
        // A held direction also moves when its unit's devnode changes without
        // its own identity changing: an "rc" unit's instance id and container
        // name carry BOTH keys, so the other direction's key moving it is a
        // different devnode. (A FriendlyName change is applied in place by the
        // bus; it never moves a direction.)
        //
        ULONG move = 0;
        for (ULONG direction = AH_PUB_RENDER; direction <= AH_PUB_CAPTURE; direction <<= 1)
        {
            const ULONG idx = AH_DIR_INDEX(direction);
            const ULONG other = (idx == AH_PRES_RENDER) ? AH_PRES_CAPTURE : AH_PRES_RENDER;
            if ((held & direction) == 0)
            {
                continue;
            }
            if ((want & direction) == 0 || (rebuild & direction) != 0 ||
                unitOf[idx] != s->UnitOf[idx])
            {
                move |= direction;
                continue;
            }
            if (unitOf[idx] == AH_UNIT_RC &&
                !RtlEqualMemory(pres[other].Key, s->Pres[other].Key, AH_PRESKEY_BUF))
            {
                move |= direction;
            }
        }

        // Plus every broken half-install: it must be clean before
        // InstallEndpointFilters can safely retry that direction.
        ULONG remove = move | (held & ~have);
        if (remove != 0)
        {
            ULONG removeStage = AH_STAGE_NONE;
            NTSTATUS st = AhRemoveSlotDirections(s, remove, 0, TRUE, &removeStage);
            if (!NT_SUCCESS(st))
            {
                failStage = (removeStage == AH_STAGE_NONE) ? AH_STAGE_UNREGISTER : removeStage;
                failStatus = st;
            }
            DPF(D_TERSE, ("[AhSlotBindSet] slot %u took down 0x%x (rebuild 0x%x)",
                          Slot, remove, rebuild));
        }

        if (want != oldWant || rebuild != 0 || move != 0 ||
            unitOf[AH_PRES_RENDER] != s->UnitOf[AH_PRES_RENDER] ||
            unitOf[AH_PRES_CAPTURE] != s->UnitOf[AH_PRES_CAPTURE])
        {
            changed = TRUE;
        }
    }
    else
    {
        RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));
        RtlCopyMemory(s->PeerKey, PeerKey, keyLen);

        RtlZeroMemory(s->Display, sizeof(s->Display));
        status = RtlStringCchCopyW(s->Display, AH_DISPLAY_CHARS, Display);
        if (status == STATUS_BUFFER_OVERFLOW)
        {
            //
            // Truncation is fine, an unterminated buffer is not.
            //
            status = STATUS_SUCCESS;
        }
        s->Display[AH_DISPLAY_CHARS - 1] = L'\0';
        if (!NT_SUCCESS(status))
        {
            RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));
            *AhStatus = AH_STATUS_BAD_ARGUMENT;
            goto Done;
        }
        if (s->Display[0] == L'\0')
        {
            //
            // An empty name would make Windows compose "Speakers ()". Fall
            // back to something that at least identifies the peer.
            //
            (VOID)RtlStringCchPrintfW(s->Display, AH_DISPLAY_CHARS, L"AudioHub %S", s->PeerKey);
        }
        RtlZeroMemory(s->NamesWritten, sizeof(s->NamesWritten));
        RtlZeroMemory(s->HostPdo, sizeof(s->HostPdo));
        RtlZeroMemory(s->HostAdapter, sizeof(s->HostAdapter));
        s->NameFallback = FALSE;
    }

    //
    // The new desired state. The presentation decides the reference strings
    // (deterministic: an unchanged direction rebuilds the strings it already
    // has), the topology variant and the units.
    //
    s->Pres[AH_PRES_RENDER]  = pres[AH_PRES_RENDER];
    s->Pres[AH_PRES_CAPTURE] = pres[AH_PRES_CAPTURE];
    status = AhBuildRefStrings(s, s->PeerKey);
    if (!NT_SUCCESS(status))
    {
        Result->Stage = AH_STAGE_REFSTRINGS;
        Result->NtStatus = status;
        *AhStatus = AH_STATUS_INTERNAL;
        if (fresh)
        {
            RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));
            RtlZeroMemory(s->Pres, sizeof(s->Pres));
        }
        goto Done;
    }
    if (fresh)
    {
        status = AhBuildMinipairs(s);
        if (!NT_SUCCESS(status))
        {
            Result->Stage = AH_STAGE_ENDPOINT_NAME;
            Result->NtStatus = status;
            *AhStatus = AH_STATUS_INTERNAL;
            RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));
            RtlZeroMemory(s->Pres, sizeof(s->Pres));
            goto Done;
        }
    }
    AhSelectTopology(s);

    s->UnitOf[AH_PRES_RENDER]  = unitOf[AH_PRES_RENDER];
    s->UnitOf[AH_PRES_CAPTURE] = unitOf[AH_PRES_CAPTURE];
    status = AhBuildSlotUnits(s);
    if (NT_SUCCESS(status))
    {
        status = AhBusSetSlotUnits(Slot, s->Units, s->UnitCount);
    }
    if (!NT_SUCCESS(status) && NT_SUCCESS(failStatus))
    {
        failStage = AH_STAGE_ENUMERATE;
        failStatus = status;
    }
    status = STATUS_SUCCESS;

    if (fresh)
    {
        s->State = AH_SLOT_BOUND;
        s->LastStage = AH_STAGE_NONE;
        s->LastNtStatus = STATUS_SUCCESS;
    }
    if (changed)
    {
        s->Generation = g_AhNextGeneration++;
        if (g_AhNextGeneration == 0)
        {
            g_AhNextGeneration = 1;     // 0 means "no generation" on the wire
        }
    }
    s->Flags = Flags;

    //
    // Whatever can be published now, is.
    //
    {
        ULONG installStage = AH_STAGE_NONE;
        NTSTATUS st = AhInstallWaitingDirections(s, NULL, &installStage);
        if (!NT_SUCCESS(st) && NT_SUCCESS(failStatus))
        {
            failStage = installStage;
            failStatus = st;
        }
    }

    Result->Published = AhSlotPublishedMask(s);
    *Generation = s->Generation;
    *State      = AH_SLOT_BOUND;
    if (!NT_SUCCESS(failStatus))
    {
        Result->Stage    = (failStage == AH_STAGE_NONE) ? AH_STAGE_VERIFY : failStage;
        Result->NtStatus = failStatus;
        *AhStatus = AH_STATUS_PARTIAL;
    }
    else if (Result->Published == want && AhSlotHeldMask(s) == want)
    {
        *AhStatus = AH_STATUS_OK;
    }
    else
    {
        *AhStatus = AH_STATUS_PENDING;
    }
    AhSpeakerFormatSyncLocked(Slot);

    DPF(D_TERSE, ("[AhSlotBindSet] slot %u gen %u status %u published 0x%x want 0x%x units %u",
                  Slot, s->Generation, *AhStatus, Result->Published, want, s->UnitCount));

Done:
    //
    // What the slot presents, on every answer about a bound slot -- the
    // daemon's account of the look is the driver's, never its own request.
    //
    if (*State == AH_SLOT_BOUND)
    {
        Result->Flags |= AhSlotReplyFlags(s);
    }
    AH_UNLOCK();
    //
    // FriendlyName changes marked by AhBusSetSlotUnits are written only now,
    // with the slot lock released: a child START/REMOVE waits on that lock
    // while Plug and Play holds its own, and the property write may wait on
    // Plug and Play (bus.h).
    //
    AhBusFlushRenames();
    return status;
}

#pragma code_seg("PAGE")
NTSTATUS
AhSlotBindClear(
    _In_  ULONG   Slot,
    _In_  ULONG   Generation,
    _In_  ULONG   Flags,
    _Out_ PULONG  State,
    _Out_ PULONG  AhStatus,
    _Out_ PAH_OP_RESULT Result
    )
{
    PAGED_CODE();

    *State    = AH_SLOT_FREE;
    *AhStatus = AH_STATUS_INTERNAL;
    RtlZeroMemory(Result, sizeof(*Result));

    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS)
    {
        *AhStatus = AH_STATUS_CAPACITY;
        return STATUS_INVALID_PARAMETER;
    }

    if (Flags & AH_BINDFLAG_DEBUG_MASK)
    {
        DPF(D_ERROR, ("[AhSlotBindClear] slot %u FAULT INJECTION flags 0x%x", Slot, Flags));
    }

    AH_LOCK();

    PAH_SLOT s = &g_AhSlots[Slot];

    if (s->State == AH_SLOT_FREE)
    {
        //
        // Already gone. Success, not an error: the daemon's coordinator is
        // closed-loop and re-sends a Clear it has not seen acknowledged.
        //
        *AhStatus = AH_STATUS_OK;
        Result->Published = AhSlotPublishedMask(s);
        goto Done;
    }

    if (Generation != 0 && Generation != s->Generation)
    {
        //
        // A Clear that overtook a re-bind. Ignoring it is the whole reason the
        // generation exists -- honouring it would tear down the binding that
        // REPLACED the one the daemon meant to remove.
        //
        DPF(D_TERSE, ("[AhSlotBindClear] slot %u stale gen %u (current %u); ignored",
                      Slot, Generation, s->Generation));
        *State    = s->State;
        *AhStatus = AH_STATUS_STALE_SESSION;
        Result->Published = AhSlotPublishedMask(s);
        goto Done;
    }

    {
        //
        // v11: allowed whether or not the bus is running. The directions come
        // off their hosts with their names, and the units are unplugged: an
        // unpaired peer's devices leave the system (plan 7.3).
        //
        ULONG stage = AH_STAGE_NONE;
        NTSTATUS st = AhTeardownSlot(s, Flags, &stage);

        *State = AH_SLOT_FREE;
        Result->Published = AhSlotPublishedMask(s);

        if (!NT_SUCCESS(st))
        {
            //
            // The teardown left something behind inside PortCls. Reported, not
            // swallowed: a remove that half-succeeds is what makes the NEXT
            // install half-fail, and the daemon can only correlate the two if
            // it hears about the first one.
            //
            Result->Stage    = stage;
            Result->NtStatus = st;
            *AhStatus        = AH_STATUS_PARTIAL;
            DPF(D_ERROR, ("[AhSlotBindClear] slot %u remove failed 0x%x stage %u", Slot, st, stage));
        }
        else
        {
            *AhStatus = AH_STATUS_OK;
            DPF(D_TERSE, ("[AhSlotBindClear] slot %u cleared", Slot));
        }
    }

Done:
    AH_UNLOCK();
    AhBusFlushRenames();     // after AH_UNLOCK, never inside it (bus.h)
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
VOID
AhSlotQuery(
    _Out_ AH_QUERY_SLOTS_REPLY *Reply,
    _In_  ULONGLONG SessionId
    )
{
    PAGED_CODE();

    RtlZeroMemory(Reply, sizeof(*Reply));
    Reply->status     = AH_STATUS_OK;
    Reply->slot_count = AUDIOHUB_WIN_MAX_SLOTS;
    Reply->session_id = SessionId;

    AH_LOCK();
    for (ULONG i = 0; i < AUDIOHUB_WIN_MAX_SLOTS; i++)
    {
        const AH_SLOT *s = &g_AhSlots[i];
        Reply->slots[i].state      = s->State;
        Reply->slots[i].generation = s->Generation;
        Reply->slots[i].published  = AhSlotPublishedMask(s);
        Reply->slots[i].flags      = AhSlotReplyFlags(s);
        if (s->State == AH_SLOT_BOUND &&
            Reply->slots[i].published != AhWantedPublishedMask(s->Flags))
        {
            Reply->slots[i].flags |= AH_SLOTINFO_FLAG_PENDING;
        }
        RtlCopyMemory(Reply->slots[i].peer_key, s->PeerKey, AH_PEERKEY_BUF);
    }
    AH_UNLOCK();
}
