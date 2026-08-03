/*++

Module Name:

    ctldevice.h

Abstract:

    The control device object audiohubd talks to: \\.\AudioHubVadCtl.

--*/

#ifndef _AUDIOHUB_CTLDEVICE_H_
#define _AUDIOHUB_CTLDEVICE_H_

#include "common.h"
#include "AudioHubIoctl.h"

//
// Called from DriverEntry AFTER PcInitializeAdapterDriver (which is what fills
// the MajorFunction table we hook).
//
// A failure here must NOT fail DriverEntry: a driver that loads with no control
// plane still publishes nothing and hurts nobody, whereas a driver that refuses
// to load takes the whole devnode out and has to be recovered over SSH.
//
NTSTATUS AhCtlCreateDevice(_In_ PDRIVER_OBJECT DriverObject);

//
// Called from DriverUnload, before the port-class unload routine.
//
VOID AhCtlDeleteDevice(_In_ PDRIVER_OBJECT DriverObject);

//
// Called from StartDevice with the devnode's PDO. Reads the expected daemon
// image path out of the device software key, which only an administrator can
// write. Absent value => the check degrades to ACL-only and says so in every
// AH_HELLO_REPLY, rather than degrading silently.
//
VOID AhCtlLoadPolicy(_In_ PDEVICE_OBJECT PhysicalDeviceObject);

//
// Exported for the identity comparison and its test: TRUE when `Actual` (as
// SeLocateProcessImageName reports it) denotes the same file as `Expected` (a
// DOS path from the registry).
//
BOOLEAN AhCtlImagePathMatches(_In_ PCUNICODE_STRING Actual, _In_ PCUNICODE_STRING Expected);

//
// DRIVER -> DAEMON EVENTS (the inverted call).
//
// Posts one AH_CONTROL_EVENT. If the daemon has an IOCTL_AUDIOHUB_CONTROL_PEND
// IRP parked, it is completed with this event immediately; otherwise the event
// is QUEUED and handed to the next PEND that arrives.
//
// THE QUEUE IS THE POINT, not an optimisation. The daemon re-issues its pending
// IRP only after the previous one completes, so there is always a window in
// which no IRP is parked. Delivering only to a parked IRP would silently drop
// whatever landed in that window -- and the event that matters most,
// AH_EVENT_IOSTATE "an application just started playing", happens exactly once.
// Losing it means the session never opens and nothing anywhere reports an
// error: the user simply gets no sound. (That failure shape -- everything
// reports success, nothing happens -- is the one this project keeps hitting;
// see the note at the top of speakerwavtable.h for the previous instance.)
//
// Safe at IRQL <= DISPATCH_LEVEL: one spin lock, one IoCompleteRequest.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
VOID AhCtlRaiseEvent(
    _In_ ULONG Kind,            // AH_EVENT_*
    _In_ ULONG Slot,
    _In_ ULONG Generation,
    _In_ ULONG Flags,           // AH_EVFLAG_*
    _In_ ULONG ScalarQ16,
    _In_ ULONG State);          // AH_SLOT_* for AH_EVENT_SLOT, else 0

//
// Convenience wrapper for the one call site in the wave stream. Reads the
// slot's current generation itself, so a stream cannot report a stale one.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
VOID AhCtlRaiseIoState(_In_ ULONG Slot, _In_ BOOLEAN Input, _In_ BOOLEAN Running);

//
// How many events had to be discarded because the queue was full. Reported by
// the probe: a non-zero value means the daemon is not draining, and every
// conclusion drawn from event-driven behaviour after that point is suspect.
//
ULONG64 AhCtlEventsDropped(VOID);

#endif // _AUDIOHUB_CTLDEVICE_H_
