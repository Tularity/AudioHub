/*++

Module Name:

    bus.h

Abstract:

    v11 (1.0.2 Phase 2): the bus role of audiohubvad.sys.

    One driver image plays three roles (design 7.7.2 option A):

      * the FDO of the root devnode ROOT\AudioHubVad -- a plain WDM BUS FDO.
        It owns the control device's lifetime (created at its AddDevice,
        deleted at its REMOVE) and enumerates the children below;
      * the PDOs of those children, AUDIOHUB\PeerEndpoint, one per
        presentation unit of a paired peer (AH_UNIT_* in perpeer.h);
      * the function driver of every child: a PortCls adapter per child, set
        up by adapter.cpp exactly as the single root adapter used to be.

    IRP ROUTING (one DRIVER_OBJECT, spike S4):

      device object                  PNP          POWER          SYSTEM_CONTROL  other
      bus FDO                        AhBusFdoPnp  pass down      pass down       CREATE/CLOSE/CLEANUP ok,
                                                                                 everything else refused
      child PDO                      AhBusPdoPnp  complete       complete as-is  STATUS_INVALID_DEVICE_REQUEST
      child FDO (PortCls adapter)    the saved table: PnpHandler -> PcDispatchIrp, PortCls for the rest
      control device                 the saved table (ctldevice.cpp hooks)

    The router is installed LAST in DriverEntry and saves the whole dispatch
    table as it stands at that point. Our own objects are recognised through a
    driver-global object table plus a magic in the device extension, never by
    DeviceType.

    IDENTITY. Hardware id and device id are AUDIOHUB\PeerEndpoint, the instance
    id is "<peer fingerprint>-<unit>", and the ContainerId is
    UUIDv5(AH_BUS_CONTAINER_NS, "<fingerprint>-<unit key>") computed HERE, in
    the kernel: a caller can choose which unit a direction lands in, but never
    the container GUID, so no request can place an endpoint inside a real
    device's container. Nothing of a vendor's hardware identity is ever used.

--*/

#ifndef _AUDIOHUB_BUS_H_
#define _AUDIOHUB_BUS_H_

#include "common.h"
#include "perpeer.h"

//
// The hardware id / device id of every child. Must match the INF model line
// and tools/audiohub-vad-helper (CHILD_HARDWARE_ID).
//
#define AH_BUS_CHILD_HARDWARE_ID_W  L"AUDIOHUB\\PeerEndpoint"

//
// Children the bus can track at once: two units per slot, plus the same
// number again for units that were unplugged and are waiting for Plug and Play
// to send their REMOVE (a replacement with the same instance id is deferred
// until then -- a second PDO with a live duplicate instance id is bugcheck
// 0xCA).
//
#define AH_BUS_MAX_CHILDREN         (4u * AUDIOHUB_WIN_MAX_SLOTS)

//
// Called from DriverEntry before anything can create a device object.
//
VOID AhBusDriverInit(VOID);

//
// Called LAST in DriverEntry: every IRP goes through the bus router from here
// on, and the router hands what is not ours to the table as it stands now.
//
VOID AhBusInstallRouter(_In_ PDRIVER_OBJECT DriverObject);

//
// TRUE when Pdo is one of our child PDOs. AddDevice uses it to tell the root
// devnode (bus) from a child (PortCls adapter).
//
BOOLEAN AhBusIsChildPdo(_In_ PDEVICE_OBJECT Pdo);

//
// AddDevice for the root devnode: creates and attaches the bus FDO and the
// control device.
//
NTSTATUS AhBusAddDevice(_In_ PDRIVER_OBJECT DriverObject, _In_ PDEVICE_OBJECT Pdo);

//
// AddDevice for a child, before PcAddAdapterDevice: applies the child's
// FriendlyName so it is in place before any endpoint of the child exists.
//
VOID AhBusChildAddDevice(_In_ PDEVICE_OBJECT Pdo);

//
// The unit a child PDO stands for. FALSE when Pdo is not one of ours.
//
BOOLEAN AhBusChildSpec(_In_ PDEVICE_OBJECT Pdo, _Out_ PAH_UNIT_SPEC Spec);

//
// TRUE while the bus FDO is started. Bind answers AH_STATUS_NO_ADAPTER before.
//
BOOLEAN AhBusReady(VOID);

//
// Replaces the set of units slot `Slot` wants (Count == 0: none). Children
// no longer wanted are reported missing, new ones are created, and Plug and
// Play is asked to re-enumerate when anything changed. A unit whose devnode
// is still alive keeps it; a FriendlyName change on a live unit is MARKED here
// and written in place by AhBusFlushRenames. Callers hold the slot lock; the
// bus takes its own lock inside, never the other way round. No Plug and Play
// property is written from here: a child START/REMOVE dispatched by Plug and
// Play waits on the slot lock, so callers MUST call AhBusFlushRenames after
// releasing the slot lock.
//
// Returns a failure only when a wanted child could not be created or tracked
// (AH_STAGE_ENUMERATE); every other unit was still processed.
//
NTSTATUS AhBusSetSlotUnits(
    _In_ ULONG Slot,
    _In_reads_(Count) const AH_UNIT_SPEC *Units,
    _In_ ULONG Count);

//
// Writes every FriendlyName marked by AhBusSetSlotUnits (or by a bus
// re-sync). Callers hold NO driver lock -- neither the slot lock nor the bus
// lock -- because IoSetDevicePropertyData may wait on Plug and Play, whose
// dispatch paths wait on those locks. Never waits for another flush.
//
VOID AhBusFlushRenames(VOID);

//
// Fills Spec->InstanceId and Spec->ContainerId from Spec->PeerKey, Unit, KeyR
// and KeyC. Pure; exported so perpeer.cpp builds specs with ONE implementation.
//
NTSTATUS AhBusFinishUnitSpec(_Inout_ PAH_UNIT_SPEC Spec);

#endif // _AUDIOHUB_BUS_H_
