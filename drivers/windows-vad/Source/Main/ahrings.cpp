/*++

Module Name:

    ahrings.cpp

Abstract:

    Allocation, user-mode mapping and DPC-time access for the shared audio
    rings. See ahrings.h for the two-IRQL split and AudioHubRing.h for the
    layout and the transfer functions.

--*/

#pragma warning (disable : 4127)

#include "definitions.h"
#include "endpoints.h"
#include "ahrings.h"

#define AH_RING_POOLTAG  'RbhA'

//
// One ring. `Base` is the kernel address (the header, then the samples);
// `UserVa` is the daemon's view of the SAME pages.
//
typedef struct _AH_RING
{
    PVOID   Base;
    PMDL    Mdl;
    ULONG   Bytes;
    ULONG   Channels;
    PVOID   UserVa;     // NULL while unmapped
} AH_RING, *PAH_RING;

typedef struct _AH_RING_TABLE
{
    AH_RING Ring[2 * AUDIOHUB_WIN_MAX_SLOTS];
} AH_RING_TABLE, *PAH_RING_TABLE;

//
// THE one pointer the DPC reads. Published once with a release store, cleared
// only in DriverUnload. See ahrings.h for why that makes the hot path
// lock-free.
//
static PAH_RING_TABLE volatile g_AhRings = NULL;

//
// Mapping ownership. Guarded by g_AhMapLock, a FAST MUTEX rather than a spin
// lock because the work it protects blocks: pool allocation and
// MmMapLockedPagesSpecifyCache with AccessMode=UserMode, both of which require
// IRQL <= APC_LEVEL. The DPC never takes it and never needs to: it reads
// g_AhRings and the Base pointers inside it, neither of which changes after
// publication.
//
// WHAT THIS LOCK IS NOT: it does not keep the region at PASSIVE_LEVEL.
// ExAcquireFastMutex RAISES IRQL to APC_LEVEL -- that is the entire mechanism
// by which it blocks APC delivery -- so a DDI documented "IRQL == PASSIVE_LEVEL"
// is ILLEGAL between acquire and release even though the caller entered at
// PASSIVE. This comment previously claimed the opposite and named
// ObReferenceObjectByHandle as an example of what the mutex made safe; the
// reference call sat inside the region and Driver Verifier's DDI compliance
// checker bugchecked on it three times for three times running:
//
//   0xC4 (0x12001B, ...)  IrqlObPassive
//   "ObReferenceObjectByHandle should only be called at IRQL = PASSIVE_LEVEL."
//
// Two things hid it. PAGED_CODE() asserts IRQL < DISPATCH_LEVEL, so it passes
// happily at APC_LEVEL; and _IRQL_requires_max_(PASSIVE_LEVEL) on AhRingsMap
// constrains the CALLER, not the body, so static analysis had nothing to say.
// Anything PASSIVE-only belongs outside the region -- see AhRingsMap.
//
static FAST_MUTEX      g_AhMapLock;
static PVOID           g_AhMapOwner  = NULL;
static ULONG64         g_AhSignals   = 0;

//
// The wake event and the ONE lock the DPC ever takes.
//
// A plain "null the pointer, then ObDereferenceObject" is a use-after-free
// here and it took a second reading to see why: a DPC that has already LOADED
// the pointer is holding a raw PKEVENT with no reference of its own, so the
// dereference can free the object between that load and the KeSetEvent. There
// is no ordering of two plain memory operations that closes it.
//
// EX_RUNDOWN_REF would be the textbook answer and it is unavailable:
// ExAcquireRundownProtection is documented IRQL <= APC_LEVEL, and this runs in
// a DPC. A spin lock IS DISPATCH-safe, so the signal path takes it around the
// load AND the KeSetEvent, and the teardown path takes it to swap the pointer
// out. Once the swapper has the lock, every DPC that could have read the old
// pointer has finished with it -- which is exactly the property needed, and it
// costs one uncontended interlocked operation per audio tick.
//
static KSPIN_LOCK      g_AhWakeLock;
static PKEVENT         g_AhWakeEvent = NULL;   // guarded by g_AhWakeLock

#pragma code_seg("INIT")
VOID
AhRingsDriverInit(VOID)
{
    ExInitializeFastMutex(&g_AhMapLock);
    KeInitializeSpinLock(&g_AhWakeLock);
    g_AhRings     = NULL;
    g_AhMapOwner  = NULL;
    g_AhWakeEvent = NULL;
    g_AhSignals   = 0;
}

//
// Swaps the wake event under the DPC's lock and returns the displaced one for
// the caller to dereference AFTER the lock is dropped (ObDereferenceObject can
// run at DISPATCH_LEVEL, but doing it inside the lock would hold off every
// audio tick on the machine for the duration).
//
#pragma code_seg()
static PKEVENT
AhRingsSwapWake(
    _In_opt_ PKEVENT New
    )
{
    KIRQL irql;
    PKEVENT old;

    KeAcquireSpinLock(&g_AhWakeLock, &irql);
    old = g_AhWakeEvent;
    g_AhWakeEvent = New;
    KeReleaseSpinLock(&g_AhWakeLock, irql);

    return old;
}

//-----------------------------------------------------------------------------
// Allocation
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static VOID
AhRingsFreeTable(
    _In_ PAH_RING_TABLE Table
    )
{
    PAGED_CODE();

    for (ULONG i = 0; i < ARRAYSIZE(Table->Ring); i++)
    {
        if (Table->Ring[i].Mdl != NULL)
        {
            IoFreeMdl(Table->Ring[i].Mdl);
            Table->Ring[i].Mdl = NULL;
        }
        if (Table->Ring[i].Base != NULL)
        {
            ExFreePoolWithTag(Table->Ring[i].Base, AH_RING_POOLTAG);
            Table->Ring[i].Base = NULL;
        }
    }
    ExFreePoolWithTag(Table, AH_RING_POOLTAG);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhRingsAllocate(
    _Outptr_ PAH_RING_TABLE *Out
    )
/*++

Routine Description:

    Builds every ring. Called at most once per driver load, from the first
    MAP_RINGS.

    LAZY rather than at DriverEntry: 4.5 MB of non-paged pool is not free, and
    a machine with the driver installed but no daemon running -- which is every
    machine between reboot and logon, and every machine of a user who has not
    paired anything -- has no use for it. Allocating here also means an
    allocation failure is reportable to the caller as a status instead of
    turning into a driver that failed to load.

    NEVER freed until DriverUnload, though: the DPC's rule is that the table it
    reads is either absent or immortal, and a ring freed while a stream is
    running would be neither.

--*/
{
    PAGED_CODE();

    PAH_RING_TABLE table = (PAH_RING_TABLE)
        ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(AH_RING_TABLE), AH_RING_POOLTAG);
    if (table == NULL)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    for (ULONG i = 0; i < ARRAYSIZE(table->Ring); i++)
    {
        const ULONG dir      = i & 1u;
        const ULONG channels = (dir == AUDIOHUB_DIR_OUT) ? AUDIOHUB_SPK_CHANNELS : AUDIOHUB_MIC_CHANNELS;
        const ULONG bytes    = (dir == AUDIOHUB_DIR_OUT) ? AUDIOHUB_SPK_BYTES     : AUDIOHUB_MIC_BYTES;

        //
        // ExAllocatePool2 zeroes unless POOL_FLAG_UNINITIALIZED is passed, and
        // that is load-bearing rather than incidental: MmMapLockedPagesSpecify-
        // Cache's documentation requires that "uninitialized buffers ... must
        // be explicitly filled with zeros before they are mapped", because
        // whatever the pool last held would otherwise be handed to user mode.
        //
        // The size is a multiple of 16K and therefore of PAGE_SIZE, so the
        // returned block is page-aligned -- which the user-mode mapping
        // requires and which IoAllocateMdl assumes here.
        //
        PVOID base = ExAllocatePool2(POOL_FLAG_NON_PAGED, bytes, AH_RING_POOLTAG);
        if (base == NULL)
        {
            AhRingsFreeTable(table);
            return STATUS_INSUFFICIENT_RESOURCES;
        }

        PMDL mdl = IoAllocateMdl(base, bytes, FALSE, FALSE, NULL);
        if (mdl == NULL)
        {
            ExFreePoolWithTag(base, AH_RING_POOLTAG);
            AhRingsFreeTable(table);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        MmBuildMdlForNonPagedPool(mdl);

        PAUDIOHUB_RING_HEADER hdr = (PAUDIOHUB_RING_HEADER)base;
        hdr->Magic          = AUDIOHUB_RING_MAGIC;
        hdr->Version        = AUDIOHUB_RING_VERSION;
        hdr->SampleRate     = AUDIOHUB_RING_SAMPLE_RATE;
        hdr->Channels       = channels;
        hdr->CapacityFrames = AUDIOHUB_RING_FRAMES;
        hdr->Reserved       = 0;
        hdr->WriteIdx       = 0;
        hdr->ReadIdx        = 0;

        table->Ring[i].Base     = base;
        table->Ring[i].Mdl      = mdl;
        table->Ring[i].Bytes    = bytes;
        table->Ring[i].Channels = channels;
        table->Ring[i].UserVa   = NULL;
    }

    *Out = table;
    return STATUS_SUCCESS;
}

//-----------------------------------------------------------------------------
// Mapping
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static VOID
AhRingsUnmapAll(
    _In_ PAH_RING_TABLE Table
    )
{
    PAGED_CODE();

    for (ULONG i = 0; i < ARRAYSIZE(Table->Ring); i++)
    {
        if (Table->Ring[i].UserVa != NULL)
        {
            //
            // Guarded because the caller may be tearing down after a crash and
            // the address range may already be gone with the process.
            //
            __try
            {
                MmUnmapLockedPages(Table->Ring[i].UserVa, Table->Ring[i].Mdl);
            }
            __except (EXCEPTION_EXECUTE_HANDLER)
            {
                DPF(D_ERROR, ("[AhRingsUnmapAll] ring %u unmap raised 0x%x",
                              i, GetExceptionCode()));
            }
            Table->Ring[i].UserVa = NULL;
        }
    }
}

#pragma code_seg("PAGE")
NTSTATUS
AhRingsMap(
    _In_ PVOID Owner,
    _In_ HANDLE WakeEvent,
    _Out_ AH_MAP_REPLY *Reply
    )
{
    PAGED_CODE();

    NTSTATUS status = STATUS_SUCCESS;

    //
    // Referenced BEFORE the mutex, and the ordering is the whole fix rather
    // than a tidiness preference: ExAcquireFastMutex raises IRQL to APC_LEVEL
    // and ObReferenceObjectByHandle is documented PASSIVE_LEVEL only. See the
    // g_AhMapLock comment for the bugcheck this cost.
    //
    // Referencing eagerly -- before we know whether we are the owner who will
    // consume it -- is deliberate. The alternative, deferring until the
    // ownership test has passed, puts the call back inside the region and
    // recreates the bug. The cost is one reference that the non-owner paths
    // have to drop, which the single release at the end does for all of them.
    //
    NT_ASSERT(KeGetCurrentIrql() == PASSIVE_LEVEL);

    PKEVENT wakeRef = NULL;
    if (WakeEvent != NULL)
    {
        NTSTATUS evStatus = ObReferenceObjectByHandle(
            WakeEvent,
            EVENT_MODIFY_STATE,
            *ExEventObjectType,
            UserMode,
            (PVOID *)&wakeRef,
            NULL);
        if (!NT_SUCCESS(evStatus))
        {
            //
            // NOT a failure. The event is an accelerator; a daemon whose handle
            // we could not reference still gets its audio, it just waits on its
            // own tick for it. Refusing the whole mapping over this would trade
            // a working data plane for a slightly shorter latency.
            //
            DPF(D_ERROR, ("[AhRingsMap] wake event ref failed 0x%x; polling only", evStatus));
            wakeRef = NULL;
        }
    }

    ExAcquireFastMutex(&g_AhMapLock);

    PAH_RING_TABLE table = g_AhRings;
    if (table == NULL)
    {
        status = AhRingsAllocate(&table);
        if (!NT_SUCCESS(status))
        {
            DPF(D_ERROR, ("[AhRingsMap] allocation failed 0x%x", status));
            goto Done;
        }
        //
        // Release store. Everything the DPC will read through this pointer was
        // written above; publishing it with ordinary assignment would let a
        // concurrent DPC on another core observe the pointer before the
        // headers.
        //
        InterlockedExchangePointer((PVOID volatile *)&g_AhRings, table);
    }

    if (g_AhMapOwner != NULL && g_AhMapOwner != Owner)
    {
        //
        // Cannot happen while first-open-wins holds, but if it ever does, a
        // second user-mode mapping of one MDL is a documented no and the
        // consequence is a bugcheck rather than an error.
        //
        status = STATUS_DEVICE_BUSY;
        goto Done;
    }

    if (g_AhMapOwner == NULL)
    {
        for (ULONG i = 0; i < ARRAYSIZE(table->Ring); i++)
        {
            PVOID va = NULL;

            //
            // AccessMode=UserMode RAISES on failure; it does not return NULL.
            // Every documented example wraps it, and an unwrapped call is a
            // bugcheck the first time the caller is out of address space.
            //
            __try
            {
                va = MmMapLockedPagesSpecifyCache(
                        table->Ring[i].Mdl,
                        UserMode,
                        MmCached,
                        NULL,
                        FALSE,
                        (MM_PAGE_PRIORITY)(NormalPagePriority | MdlMappingNoExecute));
            }
            __except (EXCEPTION_EXECUTE_HANDLER)
            {
                status = GetExceptionCode();
                DPF(D_ERROR, ("[AhRingsMap] ring %u map raised 0x%x", i, status));
                va = NULL;
            }

            if (va == NULL)
            {
                if (NT_SUCCESS(status)) { status = STATUS_INSUFFICIENT_RESOURCES; }
                AhRingsUnmapAll(table);     // all or nothing
                goto Done;
            }

            table->Ring[i].UserVa = va;

            //
            // A fresh daemon starts from an empty ring. Without this the new
            // consumer inherits the previous one's backlog -- up to half a
            // second of the PREVIOUS session's audio, played at reconnect.
            //
            // Racy against a running DPC by construction, and harmlessly so:
            // both transfer functions clamp the difference of the two indices
            // to the capacity, so the worst outcome is one pass that moves no
            // frames. The macOS bridge has exactly the same property.
            //
            PAUDIOHUB_RING_HEADER hdr = (PAUDIOHUB_RING_HEADER)table->Ring[i].Base;
            AhRingStoreRelease(&hdr->ReadIdx, 0);
            AhRingStoreRelease(&hdr->WriteIdx, 0);
        }

        if (wakeRef != NULL)
        {
            PKEVENT old = AhRingsSwapWake(wakeRef);
            wakeRef = NULL;             // consumed; the swap owns it now
            if (old != NULL) { ObDereferenceObject(old); }
        }

        g_AhMapOwner = Owner;
        DPF(D_TERSE, ("[AhRingsMap] %u rings mapped", (ULONG)ARRAYSIZE(table->Ring)));
    }

    RtlZeroMemory(Reply, sizeof(*Reply));
    Reply->status          = AH_STATUS_OK;
    Reply->ring_count      = (UINT32)ARRAYSIZE(table->Ring);
    Reply->data_offset     = AUDIOHUB_RING_DATA_OFFSET;
    Reply->capacity_frames = AUDIOHUB_RING_FRAMES;
    Reply->sample_rate     = AUDIOHUB_RING_SAMPLE_RATE;
    Reply->spk_channels    = AUDIOHUB_SPK_CHANNELS;
    Reply->mic_channels    = AUDIOHUB_MIC_CHANNELS;
    Reply->spk_bytes       = AUDIOHUB_SPK_BYTES;
    Reply->mic_bytes       = AUDIOHUB_MIC_BYTES;
    for (ULONG i = 0; i < ARRAYSIZE(table->Ring); i++)
    {
        Reply->va[i] = (UINT64)(ULONG_PTR)table->Ring[i].UserVa;
    }

Done:
    ExReleaseFastMutex(&g_AhMapLock);

    //
    // Still set means nobody took it: an error path bailed, or this was a
    // re-map by the owner that already has a wake event installed (the swap
    // deliberately happens only on the FIRST map, so a retried handshake does
    // not displace a working event). Either way the reference taken above is
    // ours to drop, and dropping it here rather than at each `goto Done` is
    // what keeps every one of those paths from leaking it.
    //
    // Outside the mutex on purpose: ObDereferenceObject can run at
    // DISPATCH_LEVEL so the placement is not required for correctness, but a
    // final dereference can free the object and run its delete routine, and
    // that is not work to do while holding a lock the next MAP_RINGS wants.
    //
    if (wakeRef != NULL)
    {
        ObDereferenceObject(wakeRef);
    }
    return status;
}

#pragma code_seg("PAGE")
VOID
AhRingsUnmap(
    _In_ PVOID Owner
    )
{
    PAGED_CODE();

    ExAcquireFastMutex(&g_AhMapLock);

    if (g_AhMapOwner == Owner && g_AhRings != NULL)
    {
        AhRingsUnmapAll(g_AhRings);
        g_AhMapOwner = NULL;

        PKEVENT old = AhRingsSwapWake(NULL);
        if (old != NULL) { ObDereferenceObject(old); }

        DPF(D_TERSE, ("[AhRingsUnmap] rings unmapped"));
    }

    ExReleaseFastMutex(&g_AhMapLock);
}

#pragma code_seg("PAGE")
VOID
AhRingsDriverFree(VOID)
{
    PAGED_CODE();

    PAH_RING_TABLE table = (PAH_RING_TABLE)
        InterlockedExchangePointer((PVOID volatile *)&g_AhRings, NULL);
    if (table != NULL)
    {
        AhRingsUnmapAll(table);
        AhRingsFreeTable(table);
    }

    PKEVENT old = AhRingsSwapWake(NULL);
    if (old != NULL) { ObDereferenceObject(old); }
    g_AhMapOwner = NULL;
}

//-----------------------------------------------------------------------------
// Hot path
//-----------------------------------------------------------------------------

#pragma code_seg()
BOOLEAN
AhRingsMapped(VOID)
{
    return (g_AhRings != NULL && g_AhMapOwner != NULL) ? TRUE : FALSE;
}

#pragma code_seg()
PAUDIOHUB_RING_HEADER
AhRingsHeader(
    _In_ ULONG Slot,
    _In_ ULONG Dir
    )
{
    PAH_RING_TABLE table = g_AhRings;

    if (table == NULL || Slot >= AUDIOHUB_WIN_MAX_SLOTS || Dir > AUDIOHUB_DIR_IN)
    {
        return NULL;
    }
    return (PAUDIOHUB_RING_HEADER)table->Ring[AUDIOHUB_RING_INDEX(Slot, Dir)].Base;
}

#pragma code_seg()
VOID
AhRingsSignal(VOID)
{
    KIRQL irql;

    KeAcquireSpinLock(&g_AhWakeLock, &irql);
    if (g_AhWakeEvent != NULL)
    {
        //
        // Wait=FALSE is what makes this legal up to DISPATCH_LEVEL. With
        // Wait=TRUE the caller must be at PASSIVE and about to call
        // KeWaitForSingleObject, which a DPC is not and never will be.
        //
        // Inside the lock on purpose -- see the note on g_AhWakeLock. The load
        // and the use have to be one indivisible step or the object can be
        // freed between them.
        //
        KeSetEvent(g_AhWakeEvent, IO_NO_INCREMENT, FALSE);
        g_AhSignals++;
    }
    KeReleaseSpinLock(&g_AhWakeLock, irql);
}

#pragma code_seg()
ULONG64
AhRingsSignalCount(VOID)
{
    return g_AhSignals;
}
