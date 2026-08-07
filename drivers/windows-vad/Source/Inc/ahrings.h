/*++

Module Name:

    ahrings.h

Abstract:

    The data plane's kernel side: 2 * AUDIOHUB_WIN_MAX_SLOTS shared-memory
    audio rings, allocated non-paged and mapped into audiohubd's address space
    by IOCTL_AUDIOHUB_MAP_RINGS.

    Two callers, at two very different IRQLs, and the split below is what keeps
    them apart:

      * AhRingsMap / AhRingsUnmap run at PASSIVE_LEVEL from the IOCTL and
        CLEANUP dispatch routines. They are the ONLY places that allocate,
        map or unmap. MmMapLockedPagesSpecifyCache with AccessMode=UserMode
        requires IRQL <= APC_LEVEL, so this cannot be anywhere else.

      * AhRingsHeader / AhRingsSignal run at DISPATCH_LEVEL from the WaveRT
        timer DPC. They take no lock, allocate nothing and wait for nothing.

    The bridge between them is one pointer, `g_AhRings`, published exactly once
    with a release store after every ring is fully built and never taken down
    until DriverUnload -- at which point no DPC can be running because every
    stream has already been stopped. That is why the hot path needs no
    synchronisation at all: the table it reads is either absent (NULL, meaning
    "no data plane", which the caller renders as silence) or complete and
    immortal. There is deliberately no third state.

--*/

#ifndef _AUDIOHUB_AHRINGS_H_
#define _AUDIOHUB_AHRINGS_H_

#include "common.h"
#include "AudioHubIoctl.h"
#include "AudioHubRing.h"

//
// Called from DriverEntry, before anything else can reach this module.
//
VOID AhRingsDriverInit(VOID);

//
// Called from DriverUnload, after every device object is gone -- which is what
// makes it safe to free memory a DPC could otherwise be inside.
//
VOID AhRingsDriverFree(VOID);

//
// IOCTL_AUDIOHUB_MAP_RINGS. Allocates the rings on first use, maps all of them
// into the CALLING process and fills `Reply`. `Owner` is the FILE_OBJECT that
// won the session; AhRingsUnmap refuses to act for any other.
//
// Idempotent: a second call from the same owner returns the SAME addresses
// rather than making a second mapping. Two user-mode mappings of one MDL is a
// documented no, and "the daemon retried its handshake" is an ordinary event.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
NTSTATUS AhRingsMap(_In_ PVOID Owner, _In_ HANDLE WakeEvent, _Out_ AH_MAP_REPLY *Reply);

//
// IRP_MJ_CLEANUP only. MmUnmapLockedPages must run in the context of the
// process that owns the mapping -- "if the context is incorrect, the unmapping
// operation could delete the address range of a random process" -- and cleanup
// is the one dispatch routine guaranteed to run there.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
VOID AhRingsUnmap(_In_ PVOID Owner);

//
// TRUE once a daemon has mapped. The wave streams use it to decide whether
// there is anywhere to put audio; it is deliberately NOT "is a peer online".
//
BOOLEAN AhRingsMapped(VOID);

//
// The hot path. Returns the KERNEL address of one ring's header, or NULL when
// there is no data plane. `Dir` is AUDIOHUB_DIR_OUT / AUDIOHUB_DIR_IN.
//
// The returned pointer is valid for the life of the driver image: the memory
// is never freed while a stream can exist, so a DPC never has to re-check.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
PAUDIOHUB_RING_HEADER AhRingsHeader(_In_ ULONG Slot, _In_ ULONG Dir);

//
// Wakes the daemon after a DPC moved audio. A no-op when the daemon passed no
// event handle, which is fully supported -- the daemon's mixer is driven by
// its own tick and the event only shortens the wait.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
VOID AhRingsSignal(VOID);

//
// Diagnostics: how many times AhRingsSignal actually set the event. Read by
// the probe so "the wake path is wired" is an observation rather than a
// belief.
//
ULONG64 AhRingsSignalCount(VOID);

#endif // _AUDIOHUB_AHRINGS_H_
