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

#endif // _AUDIOHUB_CTLDEVICE_H_
