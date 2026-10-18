/*++

Module Name:

    bus.cpp

Abstract:

    v11 (1.0.2 Phase 2): the bus role of audiohubvad.sys. See bus.h for the
    three roles of this image and the IRP routing table.

    LOCKING. One driver-global mutex, g_AhBusLock, guards the bus FDO pointer,
    the child table, every child PDO's extension and the per-slot wanted-unit
    table. The slot lock in perpeer.cpp (AH_LOCK) is ALWAYS taken first when
    both are needed (Set -> AhBusSetSlotUnits); the PnP paths here take only
    g_AhBusLock, so Plug and Play never waits on a Bind and a Bind never waits
    on Plug and Play.

    PROPERTY WRITES. IoSetDevicePropertyData goes into the Plug and Play
    property store and may wait on locks Plug and Play holds while it
    dispatches START/REMOVE/enumeration. Those dispatch paths wait on our locks
    (a child adapter's START/REMOVE takes the slot lock in perpeer.cpp; bus
    enumeration takes g_AhBusLock). So no FriendlyName is ever written while
    ANY driver lock is held: a rename is only marked (RenamePending, NameSeq)
    under g_AhBusLock, and written by AhBusFlushRenames, which callers run
    after releasing every driver lock -- in particular a Bind runs it after
    AH_UNLOCK, never inside the slot lock.

--*/

#pragma warning (disable : 4127)

#include "definitions.h"
#include "endpoints.h"
#include "perpeer.h"
#include "bus.h"
#include "ctldevice.h"
#include <wdmsec.h>

#define AH_BUS_TAG       'BbhA'
#define AH_BUS_FDO_MAGIC 0x46424841u    // 'AHBF'
#define AH_BUS_PDO_MAGIC 0x50424841u    // 'AHBP'

//
// {ecf478b0-7431-4a69-bb5b-b44817e761a2}: the AudioHub bus type, reported in
// IRP_MN_QUERY_BUS_INFORMATION. Our own GUID, generated for this purpose.
//
static const GUID GUID_AH_BUS_TYPE = {
    0xecf478b0, 0x7431, 0x4a69, { 0xbb, 0x5b, 0xb4, 0x48, 0x17, 0xe7, 0x61, 0xa2 }
};

//
// {b966078c-4d5c-4bbd-9ba0-acaf8e7205b1}: the UUIDv5 namespace of child
// ContainerIds. Our own GUID; a container derived from it can never equal a
// container any vendor's device or the system assigns.
//
static const UCHAR g_AhContainerNamespace[16] = {
    0xb9, 0x66, 0x07, 0x8c, 0x4d, 0x5c, 0x4b, 0xbd,
    0x9b, 0xa0, 0xac, 0xaf, 0x8e, 0x72, 0x05, 0xb1
};

//
// DEVPKEY_Device_FriendlyName {a45c254e-df1c-4efd-8020-67d146a850e0},14.
// Spelled out for the reason AhDevpkeyInterfaceFriendlyName is in perpeer.cpp.
//
static const DEVPROPKEY AhDevpkeyDeviceFriendlyName = {
    { 0xa45c254e, 0xdf1c, 0x4efd, { 0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0 } }, 14
};

typedef struct _AH_BUS_FDO_EXT
{
    ULONG           Magic;
    PDEVICE_OBJECT  Self;
    PDEVICE_OBJECT  Lower;
    PDEVICE_OBJECT  Pdo;
    BOOLEAN         Started;
} AH_BUS_FDO_EXT, *PAH_BUS_FDO_EXT;

typedef struct _AH_BUS_PDO_EXT
{
    ULONG               Magic;
    PDEVICE_OBJECT      Self;
    AH_UNIT_SPEC        Spec;
    BOOLEAN             Present;        // the bus reports it in BusRelations
    BOOLEAN             Reported;       // Plug and Play has seen it at least once
    BOOLEAN             Started;
    BOOLEAN             SurpriseRemoved;
    BOOLEAN             DevnodeKnown;   // AddDevice ran: properties may be set
    BOOLEAN             RenamePending;  // Spec.FriendlyName still has to be written
    ULONG               NameSeq;        // bumped by every rename; detects a write that went stale
} AH_BUS_PDO_EXT, *PAH_BUS_PDO_EXT;

static KSPIN_LOCK       g_AhBusObjLock;
static PDEVICE_OBJECT   g_AhBusObjs[AH_BUS_MAX_CHILDREN + 1];
static PDRIVER_DISPATCH g_AhBusSaved[IRP_MJ_MAXIMUM_FUNCTION + 1];

static KMUTEX           g_AhBusLock;
static PAH_BUS_FDO_EXT  g_AhBus = NULL;
static PDEVICE_OBJECT   g_AhBusChildren[AH_BUS_MAX_CHILDREN];
static AH_UNIT_SPEC     g_AhBusWant[AUDIOHUB_WIN_MAX_SLOTS][2];
static ULONG            g_AhBusWantCount[AUDIOHUB_WIN_MAX_SLOTS];
static BOOLEAN          g_AhBusFlushing = FALSE;   // one AhBusFlushRenames owner at a time

#define AH_BUS_LOCK()   KeWaitForSingleObject(&g_AhBusLock, Executive, KernelMode, FALSE, NULL)
#define AH_BUS_UNLOCK() KeReleaseMutex(&g_AhBusLock, FALSE)

#define AH_KIND_NONE    0u
#define AH_KIND_FDO     1u
#define AH_KIND_PDO     2u

//-----------------------------------------------------------------------------
// Object table
//-----------------------------------------------------------------------------

#pragma code_seg()
static VOID
AhBusObjAdd(_In_ PDEVICE_OBJECT Obj)
{
    KIRQL irql;
    KeAcquireSpinLock(&g_AhBusObjLock, &irql);
    for (ULONG i = 0; i < ARRAYSIZE(g_AhBusObjs); i++)
    {
        if (g_AhBusObjs[i] == NULL)
        {
            g_AhBusObjs[i] = Obj;
            break;
        }
    }
    KeReleaseSpinLock(&g_AhBusObjLock, irql);
}

#pragma code_seg()
static VOID
AhBusObjRemove(_In_ PDEVICE_OBJECT Obj)
{
    KIRQL irql;
    KeAcquireSpinLock(&g_AhBusObjLock, &irql);
    for (ULONG i = 0; i < ARRAYSIZE(g_AhBusObjs); i++)
    {
        if (g_AhBusObjs[i] == Obj)
        {
            g_AhBusObjs[i] = NULL;
        }
    }
    KeReleaseSpinLock(&g_AhBusObjLock, irql);
}

//
// Membership in the table first, magic second: a PortCls FDO or the control
// device is never in the table, so their extensions are never read here.
//
#pragma code_seg()
static ULONG
AhBusKind(_In_ PDEVICE_OBJECT Obj)
{
    ULONG kind = AH_KIND_NONE;
    KIRQL irql;
    KeAcquireSpinLock(&g_AhBusObjLock, &irql);
    for (ULONG i = 0; i < ARRAYSIZE(g_AhBusObjs); i++)
    {
        if (g_AhBusObjs[i] == Obj)
        {
            ULONG magic = *(PULONG)Obj->DeviceExtension;
            kind = (magic == AH_BUS_FDO_MAGIC) ? AH_KIND_FDO
                 : (magic == AH_BUS_PDO_MAGIC) ? AH_KIND_PDO
                 : AH_KIND_NONE;
            break;
        }
    }
    KeReleaseSpinLock(&g_AhBusObjLock, irql);
    return kind;
}

#pragma code_seg()
BOOLEAN
AhBusIsChildPdo(_In_ PDEVICE_OBJECT Pdo)
{
    return (AhBusKind(Pdo) == AH_KIND_PDO) ? TRUE : FALSE;
}

#pragma code_seg()
static NTSTATUS
AhBusComplete(_In_ PIRP Irp, _In_ NTSTATUS Status, _In_ ULONG_PTR Information)
{
    Irp->IoStatus.Status = Status;
    Irp->IoStatus.Information = Information;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Status;
}

//-----------------------------------------------------------------------------
// SHA-1 (FIPS 180-4) for RFC 4122 name-based UUIDs, version 5.
//
// Implemented here rather than through CNG: it runs on one short ASCII name
// per child creation, needs no provider handle with its own lifetime, and has
// no failure path. SHA-1's collision weakness is irrelevant to this use -- the
// name is ours and public, and the output only has to be stable and distinct
// per unit, which is exactly what RFC 4122 version 5 specifies.
//-----------------------------------------------------------------------------

typedef struct _AH_SHA1
{
    ULONG     H[5];
    UCHAR     Block[64];
    ULONG     Used;
    ULONGLONG Bits;
} AH_SHA1;

#define AH_ROL32(x, n)  (((x) << (n)) | ((x) >> (32 - (n))))

#pragma code_seg("PAGE")
static VOID
AhSha1Compress(_Inout_ AH_SHA1 *C)
{
    PAGED_CODE();

    ULONG w[80];
    for (ULONG t = 0; t < 16; t++)
    {
        w[t] = ((ULONG)C->Block[4 * t] << 24) | ((ULONG)C->Block[4 * t + 1] << 16) |
               ((ULONG)C->Block[4 * t + 2] << 8) | (ULONG)C->Block[4 * t + 3];
    }
    for (ULONG t = 16; t < 80; t++)
    {
        ULONG v = w[t - 3] ^ w[t - 8] ^ w[t - 14] ^ w[t - 16];
        w[t] = AH_ROL32(v, 1);
    }

    ULONG a = C->H[0], b = C->H[1], c = C->H[2], d = C->H[3], e = C->H[4];
    for (ULONG t = 0; t < 80; t++)
    {
        ULONG f, k;
        if (t < 20)      { f = (b & c) | (~b & d);           k = 0x5A827999u; }
        else if (t < 40) { f = b ^ c ^ d;                    k = 0x6ED9EBA1u; }
        else if (t < 60) { f = (b & c) | (b & d) | (c & d);  k = 0x8F1BBCDCu; }
        else             { f = b ^ c ^ d;                    k = 0xCA62C1D6u; }
        ULONG temp = AH_ROL32(a, 5) + f + e + k + w[t];
        e = d;
        d = c;
        c = AH_ROL32(b, 30);
        b = a;
        a = temp;
    }
    C->H[0] += a; C->H[1] += b; C->H[2] += c; C->H[3] += d; C->H[4] += e;
}

#pragma code_seg("PAGE")
static VOID
AhSha1Init(_Out_ AH_SHA1 *C)
{
    PAGED_CODE();

    RtlZeroMemory(C, sizeof(*C));
    C->H[0] = 0x67452301u;
    C->H[1] = 0xEFCDAB89u;
    C->H[2] = 0x98BADCFEu;
    C->H[3] = 0x10325476u;
    C->H[4] = 0xC3D2E1F0u;
}

#pragma code_seg("PAGE")
static VOID
AhSha1Update(_Inout_ AH_SHA1 *C, _In_reads_bytes_(Length) const UCHAR *Data, _In_ ULONG Length)
{
    PAGED_CODE();

    for (ULONG i = 0; i < Length; i++)
    {
        C->Block[C->Used++] = Data[i];
        C->Bits += 8;
        if (C->Used == 64)
        {
            AhSha1Compress(C);
            C->Used = 0;
        }
    }
}

#pragma code_seg("PAGE")
static VOID
AhSha1Final(_Inout_ AH_SHA1 *C, _Out_writes_bytes_(20) UCHAR *Digest)
{
    PAGED_CODE();

    ULONGLONG bits = C->Bits;
    UCHAR one = 0x80;
    UCHAR zero = 0;
    AhSha1Update(C, &one, 1);
    while (C->Used != 56)
    {
        AhSha1Update(C, &zero, 1);
    }
    UCHAR len[8];
    for (ULONG i = 0; i < 8; i++)
    {
        len[i] = (UCHAR)(bits >> (56 - 8 * i));
    }
    AhSha1Update(C, len, 8);
    for (ULONG i = 0; i < 5; i++)
    {
        Digest[4 * i]     = (UCHAR)(C->H[i] >> 24);
        Digest[4 * i + 1] = (UCHAR)(C->H[i] >> 16);
        Digest[4 * i + 2] = (UCHAR)(C->H[i] >> 8);
        Digest[4 * i + 3] = (UCHAR)(C->H[i]);
    }
}

//
// UUIDv5(g_AhContainerNamespace, Name), RFC 4122 4.3: SHA-1 over the
// namespace's 16 bytes in network order followed by the name, the first 16
// digest bytes, version 5 in the high nibble of byte 6, variant 10xx in byte 8,
// then read back into GUID fields big-endian.
//
#pragma code_seg("PAGE")
static VOID
AhUuidV5(_In_reads_bytes_(NameLength) const UCHAR *Name, _In_ ULONG NameLength, _Out_ GUID *Out)
{
    PAGED_CODE();

    AH_SHA1 ctx;
    UCHAR digest[20];
    AhSha1Init(&ctx);
    AhSha1Update(&ctx, g_AhContainerNamespace, sizeof(g_AhContainerNamespace));
    AhSha1Update(&ctx, Name, NameLength);
    AhSha1Final(&ctx, digest);

    digest[6] = (UCHAR)((digest[6] & 0x0F) | 0x50);
    digest[8] = (UCHAR)((digest[8] & 0x3F) | 0x80);

    Out->Data1 = ((ULONG)digest[0] << 24) | ((ULONG)digest[1] << 16) |
                 ((ULONG)digest[2] << 8) | (ULONG)digest[3];
    Out->Data2 = (USHORT)(((USHORT)digest[4] << 8) | digest[5]);
    Out->Data3 = (USHORT)(((USHORT)digest[6] << 8) | digest[7]);
    RtlCopyMemory(Out->Data4, &digest[8], 8);
}

//-----------------------------------------------------------------------------
// Unit specs
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static PCSTR
AhUnitCode(_In_ ULONG Unit)
{
    PAGED_CODE();

    switch (Unit)
    {
    case AH_UNIT_V:  return "v";
    case AH_UNIT_R:  return "r";
    case AH_UNIT_C:  return "c";
    case AH_UNIT_RC: return "rc";
    default:         return NULL;
    }
}

#pragma code_seg("PAGE")
NTSTATUS
AhBusFinishUnitSpec(_Inout_ PAH_UNIT_SPEC Spec)
{
    PAGED_CODE();

    PCSTR code = AhUnitCode(Spec->Unit);
    Spec->PeerKey[AH_PEERKEY_BUF - 1] = '\0';
    Spec->KeyR[AH_PRESKEY_BUF - 1] = '\0';
    Spec->KeyC[AH_PRESKEY_BUF - 1] = '\0';
    if (code == NULL || !AhIsValidPeerKey(Spec->PeerKey, strlen(Spec->PeerKey)))
    {
        return STATUS_INVALID_PARAMETER;
    }

    //
    // Instance id "<fp>-<unit>": stable across presentation-key changes of the
    // same unit kind, so Windows keeps the devnode (and S6: refreshes its name
    // on re-plug) while the endpoint ids still follow the reference strings.
    //
    NTSTATUS status = RtlStringCchPrintfW(Spec->InstanceId, AH_INSTANCE_ID_CHARS,
                                          L"%S-%S", Spec->PeerKey, code);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    //
    // ContainerId name "<fp>-<unit key>": unit key "v", "r-<keyR>", "c-<keyC>"
    // or "rc-<keyR>-<keyC>". The presentation keys are part of it, so a
    // different physical look is a different "device" in Settings, and going
    // back to an earlier look returns the earlier container.
    //
    CHAR name[96];
    switch (Spec->Unit)
    {
    case AH_UNIT_V:
        status = RtlStringCchPrintfA(name, ARRAYSIZE(name), "%s-v", Spec->PeerKey);
        break;
    case AH_UNIT_R:
        status = RtlStringCchPrintfA(name, ARRAYSIZE(name), "%s-r-%s", Spec->PeerKey, Spec->KeyR);
        break;
    case AH_UNIT_C:
        status = RtlStringCchPrintfA(name, ARRAYSIZE(name), "%s-c-%s", Spec->PeerKey, Spec->KeyC);
        break;
    default:
        status = RtlStringCchPrintfA(name, ARRAYSIZE(name), "%s-rc-%s-%s",
                                     Spec->PeerKey, Spec->KeyR, Spec->KeyC);
        break;
    }
    if (!NT_SUCCESS(status))
    {
        return status;
    }
    AhUuidV5((const UCHAR *)name, (ULONG)strlen(name), &Spec->ContainerId);
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
static BOOLEAN
AhSameInstanceId(_In_z_ PCWSTR A, _In_z_ PCWSTR B)
{
    PAGED_CODE();

    UNICODE_STRING a, b;
    RtlInitUnicodeString(&a, A);
    RtlInitUnicodeString(&b, B);
    return RtlEqualUnicodeString(&a, &b, TRUE);
}

//
// FriendlyName writes. A rename is MARKED under g_AhBusLock (RenamePending,
// NameSeq) and WRITTEN by AhBusFlushRenames with no driver lock held (see
// PROPERTY WRITES above). Holding a lock across IoSetDevicePropertyData would
// let the writer (holding our lock, waiting inside the property store on Plug
// and Play) and a Plug and Play thread (holding its own locks while it
// dispatches START/REMOVE/enumeration, waiting on our lock) wait on each other.
//
// Correctness without holding the lock across the call:
//   * the PDO is referenced (ObReferenceObject) for the duration of the write,
//     so its memory stays valid even if it is deleted meanwhile;
//   * DevnodeKnown && Present are re-checked under the lock immediately before
//     the call, and every deletion path clears Present (and DevnodeKnown)
//     under the lock before IoDeleteDevice, so a write never starts on a PDO
//     whose deletion has begun;
//   * the name is copied under the lock together with NameSeq; if NameSeq has
//     moved when the call returns, a newer name arrived and RenamePending is
//     set again, so the newest name is always the last one written.
//

// Caller holds g_AhBusLock.
#pragma code_seg("PAGE")
static VOID
AhBusMarkRenameLocked(_Inout_ PAH_BUS_PDO_EXT X)
{
    PAGED_CODE();

    X->NameSeq++;
    if (X->DevnodeKnown && X->Present)
    {
        X->RenamePending = TRUE;
    }
}

// Caller holds g_AhBusLock: the PDO can no longer be written.
#pragma code_seg("PAGE")
static VOID
AhBusRetirePdoLocked(_Inout_ PAH_BUS_PDO_EXT X)
{
    PAGED_CODE();

    X->Present = FALSE;
    X->DevnodeKnown = FALSE;
    X->RenamePending = FALSE;
}

// Caller holds NO driver lock and a reference on Pdo.
#pragma code_seg("PAGE")
static VOID
AhBusWriteFriendlyName(_In_ PDEVICE_OBJECT Pdo)
{
    PAGED_CODE();

    PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)Pdo->DeviceExtension;
    WCHAR name[AH_DISPLAY_CHARS];
    WCHAR id[AH_INSTANCE_ID_CHARS];
    ULONG seq = 0;
    BOOLEAN apply = FALSE;

    AH_BUS_LOCK();
    if (x->DevnodeKnown && x->Present)
    {
        RtlCopyMemory(name, x->Spec.FriendlyName, sizeof(name));
        RtlCopyMemory(id, x->Spec.InstanceId, sizeof(id));
        name[AH_DISPLAY_CHARS - 1] = L'\0';
        id[AH_INSTANCE_ID_CHARS - 1] = L'\0';
        seq = x->NameSeq;
        apply = TRUE;
    }
    AH_BUS_UNLOCK();

    if (!apply)
    {
        return;
    }

    size_t cch = 0;
    (VOID)RtlStringCchLengthW(name, AH_DISPLAY_CHARS, &cch);
    NTSTATUS st = IoSetDevicePropertyData(
        Pdo, &AhDevpkeyDeviceFriendlyName, LOCALE_NEUTRAL,
        PLUGPLAY_PROPERTY_PERSISTENT, DEVPROP_TYPE_STRING,
        (ULONG)((cch + 1) * sizeof(WCHAR)), name);
    if (!NT_SUCCESS(st))
    {
        DPF(D_ERROR, ("[AhBus] FriendlyName for %S failed 0x%x", id, st));
    }

    AH_BUS_LOCK();
    if (x->NameSeq != seq && x->DevnodeKnown && x->Present)
    {
        x->RenamePending = TRUE;     // renamed while we wrote: write the newer name too
    }
    AH_BUS_UNLOCK();
}

//
// Writes every pending FriendlyName. Callers hold NO driver lock (neither
// g_AhBusLock nor the slot lock). Never waits on another flusher: if one is
// running it returns at once, and the running one re-scans until nothing is
// pending, so a rename marked before this call is always written. Nothing in
// a Plug and Play dispatch path ever waits for a flush.
//
#pragma code_seg("PAGE")
VOID
AhBusFlushRenames(VOID)
{
    PAGED_CODE();

    AH_BUS_LOCK();
    if (g_AhBusFlushing)
    {
        AH_BUS_UNLOCK();
        return;
    }
    g_AhBusFlushing = TRUE;
    for (;;)
    {
        PDEVICE_OBJECT pdo = NULL;
        for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN && pdo == NULL; c++)
        {
            PDEVICE_OBJECT candidate = g_AhBusChildren[c];
            if (candidate == NULL)
            {
                continue;
            }
            PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)candidate->DeviceExtension;
            if (x->RenamePending)
            {
                x->RenamePending = FALSE;
                ObReferenceObject(candidate);
                pdo = candidate;
            }
        }
        if (pdo == NULL)
        {
            g_AhBusFlushing = FALSE;
            break;
        }
        AH_BUS_UNLOCK();
        AhBusWriteFriendlyName(pdo);
        ObDereferenceObject(pdo);
        AH_BUS_LOCK();
    }
    AH_BUS_UNLOCK();
}

//-----------------------------------------------------------------------------
// Child table
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static NTSTATUS
AhBusCreatePdo(_In_ PAH_BUS_FDO_EXT Bus, _In_ const AH_UNIT_SPEC *Spec, _Out_ PDEVICE_OBJECT *Out)
{
    PAGED_CODE();

    *Out = NULL;
    PDEVICE_OBJECT pdo = NULL;
    NTSTATUS status = IoCreateDeviceSecure(
        Bus->Self->DriverObject, sizeof(AH_BUS_PDO_EXT), NULL, FILE_DEVICE_BUS_EXTENDER,
        FILE_AUTOGENERATED_DEVICE_NAME | FILE_DEVICE_SECURE_OPEN, FALSE,
        &SDDL_DEVOBJ_SYS_ALL_ADM_ALL, NULL, &pdo);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)pdo->DeviceExtension;
    RtlZeroMemory(x, sizeof(*x));
    x->Magic   = AH_BUS_PDO_MAGIC;
    x->Self    = pdo;
    x->Spec    = *Spec;
    x->Present = TRUE;
    pdo->Flags |= DO_POWER_PAGABLE;
    pdo->Flags &= ~DO_DEVICE_INITIALIZING;
    AhBusObjAdd(pdo);
    *Out = pdo;
    return STATUS_SUCCESS;
}

//
// Diffs the wanted-unit table against the child table. Caller holds
// g_AhBusLock and has checked the bus is started. *Changed is set when
// BusRelations should be re-queried. A FriendlyName change is only MARKED;
// the caller runs AhBusFlushRenames once it holds no driver lock.
//
#pragma code_seg("PAGE")
static NTSTATUS
AhBusSyncLocked(_Out_ PBOOLEAN Changed)
{
    PAGED_CODE();

    NTSTATUS result = STATUS_SUCCESS;
    PAH_BUS_FDO_EXT bus = g_AhBus;
    *Changed = FALSE;

    //
    // Unplug what is no longer wanted, or wanted under another container (a
    // different presentation key of the same unit kind: same instance id,
    // so the replacement waits for this one's REMOVE).
    //
    for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
    {
        PDEVICE_OBJECT pdo = g_AhBusChildren[c];
        if (pdo == NULL)
        {
            continue;
        }
        PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)pdo->DeviceExtension;
        if (!x->Present)
        {
            continue;
        }
        const AH_UNIT_SPEC *match = NULL;
        ULONG slot = x->Spec.Slot;
        if (slot < AUDIOHUB_WIN_MAX_SLOTS)
        {
            for (ULONG u = 0; u < g_AhBusWantCount[slot]; u++)
            {
                if (AhSameInstanceId(g_AhBusWant[slot][u].InstanceId, x->Spec.InstanceId))
                {
                    match = &g_AhBusWant[slot][u];
                    break;
                }
            }
        }
        if (match != NULL &&
            RtlEqualMemory(&match->ContainerId, &x->Spec.ContainerId, sizeof(GUID)))
        {
            //
            // Same devnode, same container. A changed FriendlyName (the peer
            // was renamed) is applied in place: re-plugging only to rename
            // would make the endpoints NOTPRESENT and move the user's default.
            //
            BOOLEAN rename = (wcsncmp(match->FriendlyName, x->Spec.FriendlyName, AH_DISPLAY_CHARS) != 0)
                           ? TRUE : FALSE;
            x->Spec = *match;
            if (rename)
            {
                AhBusMarkRenameLocked(x);
            }
            continue;
        }

        x->Present = FALSE;
        *Changed = TRUE;
        DPF(D_TERSE, ("[AhBus] unplug %S", x->Spec.InstanceId));
        if (!x->Reported)
        {
            //
            // Plug and Play never saw it, so no REMOVE will ever come for it.
            // Safe to delete now: BusRelations is built under this same lock.
            //
            g_AhBusChildren[c] = NULL;
            AhBusRetirePdoLocked(x);
            AhBusObjRemove(pdo);
            IoDeleteDevice(pdo);
        }
    }

    //
    // Plug what is wanted and not present.
    //
    for (ULONG slot = 0; slot < AUDIOHUB_WIN_MAX_SLOTS; slot++)
    {
        for (ULONG u = 0; u < g_AhBusWantCount[slot]; u++)
        {
            const AH_UNIT_SPEC *want = &g_AhBusWant[slot][u];
            BOOLEAN have = FALSE;
            BOOLEAN pendingRemove = FALSE;
            ULONG freeIdx = AH_BUS_MAX_CHILDREN;
            for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
            {
                PDEVICE_OBJECT pdo = g_AhBusChildren[c];
                if (pdo == NULL)
                {
                    if (freeIdx == AH_BUS_MAX_CHILDREN) { freeIdx = c; }
                    continue;
                }
                PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)pdo->DeviceExtension;
                if (AhSameInstanceId(want->InstanceId, x->Spec.InstanceId))
                {
                    if (x->Present) { have = TRUE; } else { pendingRemove = TRUE; }
                }
            }
            if (have)
            {
                continue;
            }
            if (pendingRemove)
            {
                //
                // A second PDO with the same instance id while the old devnode
                // is still alive is a duplicate-PDO bugcheck (0xCA). The old
                // one's REMOVE re-runs this sync and plugs the replacement.
                //
                DPF(D_TERSE, ("[AhBus] plug %S deferred until the previous devnode is removed",
                              want->InstanceId));
                continue;
            }
            if (freeIdx == AH_BUS_MAX_CHILDREN)
            {
                DPF(D_ERROR, ("[AhBus] child table full; %S not plugged", want->InstanceId));
                result = STATUS_INSUFFICIENT_RESOURCES;
                continue;
            }
            PDEVICE_OBJECT pdo = NULL;
            NTSTATUS st = AhBusCreatePdo(bus, want, &pdo);
            if (!NT_SUCCESS(st))
            {
                DPF(D_ERROR, ("[AhBus] IoCreateDeviceSecure for %S failed 0x%x", want->InstanceId, st));
                result = st;
                continue;
            }
            g_AhBusChildren[freeIdx] = pdo;
            *Changed = TRUE;
            DPF(D_TERSE, ("[AhBus] plug %S (slot %u, dirs 0x%x)",
                          want->InstanceId, want->Slot, want->Directions));
        }
    }
    return result;
}

#pragma code_seg("PAGE")
BOOLEAN
AhBusReady(VOID)
{
    PAGED_CODE();

    AH_BUS_LOCK();
    BOOLEAN ready = (g_AhBus != NULL && g_AhBus->Started) ? TRUE : FALSE;
    AH_BUS_UNLOCK();
    return ready;
}

#pragma code_seg("PAGE")
NTSTATUS
AhBusSetSlotUnits(
    _In_ ULONG Slot,
    _In_reads_(Count) const AH_UNIT_SPEC *Units,
    _In_ ULONG Count
    )
{
    PAGED_CODE();

    if (Slot >= AUDIOHUB_WIN_MAX_SLOTS || Count > 2)
    {
        return STATUS_INVALID_PARAMETER;
    }

    AH_BUS_LOCK();
    for (ULONG u = 0; u < Count; u++)
    {
        g_AhBusWant[Slot][u] = Units[u];
    }
    for (ULONG u = Count; u < 2; u++)
    {
        RtlZeroMemory(&g_AhBusWant[Slot][u], sizeof(g_AhBusWant[Slot][u]));
    }
    g_AhBusWantCount[Slot] = Count;

    NTSTATUS status = STATUS_SUCCESS;
    if (g_AhBus != NULL && g_AhBus->Started)
    {
        BOOLEAN changed = FALSE;
        status = AhBusSyncLocked(&changed);
        if (changed)
        {
            // Asynchronous: queues a re-enumeration, never waits for it.
            IoInvalidateDeviceRelations(g_AhBus->Pdo, BusRelations);
        }
    }
    AH_BUS_UNLOCK();
    //
    // NOT AhBusFlushRenames here: the caller holds the slot lock, and a child
    // START/REMOVE dispatched by Plug and Play waits on that lock. The caller
    // flushes after AH_UNLOCK.
    //
    return status;
}

#pragma code_seg("PAGE")
BOOLEAN
AhBusChildSpec(_In_ PDEVICE_OBJECT Pdo, _Out_ PAH_UNIT_SPEC Spec)
{
    PAGED_CODE();

    RtlZeroMemory(Spec, sizeof(*Spec));
    if (!AhBusIsChildPdo(Pdo))
    {
        return FALSE;
    }
    AH_BUS_LOCK();
    *Spec = ((PAH_BUS_PDO_EXT)Pdo->DeviceExtension)->Spec;
    AH_BUS_UNLOCK();
    return TRUE;
}

#pragma code_seg("PAGE")
VOID
AhBusChildAddDevice(_In_ PDEVICE_OBJECT Pdo)
{
    PAGED_CODE();

    if (!AhBusIsChildPdo(Pdo))
    {
        return;
    }
    AH_BUS_LOCK();
    PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)Pdo->DeviceExtension;
    x->DevnodeKnown = TRUE;
    x->RenamePending = FALSE;    // written right below, synchronously
    ObReferenceObject(Pdo);
    AH_BUS_UNLOCK();
    //
    // S6: set here, before the child's adapter exists, the FriendlyName is
    // the bracketed half of every endpoint name of this child and the device
    // name in Settings. QUERY_DEVICE_TEXT reports the same text. Written
    // directly (no flusher hand-off) so it is in place before this AddDevice
    // returns; a rename that races it is caught by NameSeq and flushed.
    //
    AhBusWriteFriendlyName(Pdo);
    ObDereferenceObject(Pdo);
    AhBusFlushRenames();
}

//-----------------------------------------------------------------------------
// Child PDO
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static PWCHAR
AhBusPoolString(_In_z_ PCWSTR Src, _In_ BOOLEAN MultiSz)
{
    PAGED_CODE();

    size_t len = 0;
    (VOID)RtlStringCchLengthW(Src, 512, &len);
    SIZE_T cb = (len + (MultiSz ? 2 : 1)) * sizeof(WCHAR);
    PWCHAR p = (PWCHAR)ExAllocatePool2(POOL_FLAG_PAGED, cb, AH_BUS_TAG);
    if (p != NULL)
    {
        RtlCopyMemory(p, Src, len * sizeof(WCHAR));
        p[len] = L'\0';
        if (MultiSz)
        {
            p[len + 1] = L'\0';
        }
    }
    return p;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhBusPdoPnp(_In_ PDEVICE_OBJECT Pdo, _Inout_ PIRP Irp)
{
    PAGED_CODE();

    PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)Pdo->DeviceExtension;
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);
    NTSTATUS status = Irp->IoStatus.Status;
    ULONG_PTR info = Irp->IoStatus.Information;

    switch (sl->MinorFunction)
    {
    case IRP_MN_START_DEVICE:
        AH_BUS_LOCK();
        x->Started = TRUE;
        x->DevnodeKnown = TRUE;
        AH_BUS_UNLOCK();
        status = STATUS_SUCCESS;
        break;

    case IRP_MN_STOP_DEVICE:
        x->Started = FALSE;
        status = STATUS_SUCCESS;
        break;

    case IRP_MN_QUERY_STOP_DEVICE:
    case IRP_MN_CANCEL_STOP_DEVICE:
    case IRP_MN_QUERY_REMOVE_DEVICE:
    case IRP_MN_CANCEL_REMOVE_DEVICE:
    case IRP_MN_EJECT:
        status = STATUS_SUCCESS;
        break;

    case IRP_MN_SURPRISE_REMOVAL:
        x->SurpriseRemoved = TRUE;
        x->Started = FALSE;
        status = STATUS_SUCCESS;
        break;

    case IRP_MN_REMOVE_DEVICE:
    {
        //
        // Delete the PDO only when the bus no longer reports it. A REMOVE for
        // a child the bus still reports (disabled in Device Manager, driver
        // update) keeps the PDO: Plug and Play will start it again.
        //
        BOOLEAN del = FALSE;
        AH_BUS_LOCK();
        x->Started = FALSE;
        if (!x->Present)
        {
            for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
            {
                if (g_AhBusChildren[c] == Pdo)
                {
                    g_AhBusChildren[c] = NULL;
                }
            }
            AhBusRetirePdoLocked(x);
            del = TRUE;
        }
        AH_BUS_UNLOCK();
        DPF(D_TERSE, ("[AhBus] REMOVE %S present=%u delete=%u",
                      x->Spec.InstanceId, (ULONG)!del, (ULONG)del));

        Irp->IoStatus.Status = STATUS_SUCCESS;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        if (del)
        {
            AhBusObjRemove(Pdo);
            IoDeleteDevice(Pdo);

            //
            // A replacement with the same instance id may have been deferred
            // behind this devnode. Plug it now.
            //
            AH_BUS_LOCK();
            if (g_AhBus != NULL && g_AhBus->Started)
            {
                BOOLEAN changed = FALSE;
                (VOID)AhBusSyncLocked(&changed);
                if (changed)
                {
                    IoInvalidateDeviceRelations(g_AhBus->Pdo, BusRelations);
                }
            }
            AH_BUS_UNLOCK();
            AhBusFlushRenames();     // no driver lock held here
        }
        return STATUS_SUCCESS;
    }

    case IRP_MN_QUERY_CAPABILITIES:
    {
        PDEVICE_CAPABILITIES caps = sl->Parameters.DeviceCapabilities.Capabilities;
        if (caps->Version != 1 || caps->Size < sizeof(DEVICE_CAPABILITIES))
        {
            status = STATUS_UNSUCCESSFUL;
            break;
        }
        caps->DeviceState[PowerSystemWorking] = PowerDeviceD0;
        for (int s = PowerSystemSleeping1; s < PowerSystemMaximum; s++)
        {
            caps->DeviceState[s] = PowerDeviceD3;
        }
        caps->DeviceWake = PowerDeviceUnspecified;
        caps->SystemWake = PowerSystemUnspecified;
        caps->DeviceD1 = FALSE;
        caps->DeviceD2 = FALSE;
        caps->WakeFromD0 = caps->WakeFromD1 = caps->WakeFromD2 = caps->WakeFromD3 = FALSE;
        caps->LockSupported = FALSE;
        caps->EjectSupported = FALSE;
        //
        // Removable + SurpriseRemovalOK: a unit is an independent device that
        // comes and goes with the pairing, and Windows does not list it under
        // "Safely Remove Hardware". The ContainerId below is honoured either
        // way (spike S5).
        //
        caps->Removable = TRUE;
        caps->SurpriseRemovalOK = TRUE;
        //
        // The instance id carries the peer fingerprint and the unit, so it is
        // unique system-wide: Windows uses it verbatim
        // (AUDIOHUB\PEERENDPOINT\<fp>-<unit>) instead of prefixing the
        // parent's identity, which keeps a unit's devnode -- and its endpoint
        // ids -- stable if the root devnode is ever re-created.
        //
        caps->UniqueID = TRUE;
        caps->SilentInstall = TRUE;
        caps->RawDeviceOK = FALSE;
        caps->DockDevice = FALSE;
        caps->HardwareDisabled = FALSE;
        caps->NoDisplayInUI = FALSE;
        caps->Address = 0xFFFFFFFF;
        caps->UINumber = 0xFFFFFFFF;
        caps->D1Latency = caps->D2Latency = caps->D3Latency = 0;
        status = STATUS_SUCCESS;
        break;
    }

    case IRP_MN_QUERY_ID:
    {
        PWCHAR p = NULL;
        BUS_QUERY_ID_TYPE t = sl->Parameters.QueryId.IdType;
        AH_BUS_LOCK();
        switch (t)
        {
        case BusQueryDeviceID:
            p = AhBusPoolString(AH_BUS_CHILD_HARDWARE_ID_W, FALSE);
            break;
        case BusQueryHardwareIDs:
            p = AhBusPoolString(AH_BUS_CHILD_HARDWARE_ID_W, TRUE);
            break;
        case BusQueryInstanceID:
            p = AhBusPoolString(x->Spec.InstanceId, FALSE);
            break;
        case BusQueryContainerID:
        {
            //
            // ALWAYS answered: without it a Removable device gets a random
            // container and a non-removable one falls into the computer's
            // (spike S5).
            //
            WCHAR g[40];
            const GUID *u = &x->Spec.ContainerId;
            if (NT_SUCCESS(RtlStringCbPrintfW(
                    g, sizeof(g), L"{%08lx-%04hx-%04hx-%02x%02x-%02x%02x%02x%02x%02x%02x}",
                    u->Data1, u->Data2, u->Data3, u->Data4[0], u->Data4[1], u->Data4[2],
                    u->Data4[3], u->Data4[4], u->Data4[5], u->Data4[6], u->Data4[7])))
            {
                p = AhBusPoolString(g, FALSE);
            }
            break;
        }
        default:
            // CompatibleIDs and the rest: leave the IRP as it arrived.
            break;
        }
        AH_BUS_UNLOCK();
        if (p != NULL)
        {
            status = STATUS_SUCCESS;
            info = (ULONG_PTR)p;
        }
        else if (t == BusQueryDeviceID || t == BusQueryHardwareIDs ||
                 t == BusQueryInstanceID || t == BusQueryContainerID)
        {
            status = STATUS_INSUFFICIENT_RESOURCES;
        }
        break;
    }

    case IRP_MN_QUERY_DEVICE_TEXT:
    {
        PWCHAR p = NULL;
        DEVICE_TEXT_TYPE t = sl->Parameters.QueryDeviceText.DeviceTextType;
        AH_BUS_LOCK();
        if (t == DeviceTextDescription)
        {
            p = AhBusPoolString(x->Spec.FriendlyName, FALSE);
        }
        AH_BUS_UNLOCK();
        if (p != NULL)
        {
            status = STATUS_SUCCESS;
            info = (ULONG_PTR)p;
        }
        break;
    }

    case IRP_MN_QUERY_BUS_INFORMATION:
    {
        PPNP_BUS_INFORMATION bi = (PPNP_BUS_INFORMATION)
            ExAllocatePool2(POOL_FLAG_PAGED, sizeof(PNP_BUS_INFORMATION), AH_BUS_TAG);
        if (bi == NULL)
        {
            status = STATUS_INSUFFICIENT_RESOURCES;
            break;
        }
        bi->BusTypeGuid = GUID_AH_BUS_TYPE;
        bi->LegacyBusType = PNPBus;
        bi->BusNumber = 0;
        status = STATUS_SUCCESS;
        info = (ULONG_PTR)bi;
        break;
    }

    case IRP_MN_QUERY_DEVICE_RELATIONS:
        if (sl->Parameters.QueryDeviceRelations.Type == TargetDeviceRelation)
        {
            PDEVICE_RELATIONS r = (PDEVICE_RELATIONS)
                ExAllocatePool2(POOL_FLAG_PAGED, sizeof(DEVICE_RELATIONS), AH_BUS_TAG);
            if (r == NULL)
            {
                status = STATUS_INSUFFICIENT_RESOURCES;
                break;
            }
            r->Count = 1;
            r->Objects[0] = Pdo;
            ObReferenceObject(Pdo);
            status = STATUS_SUCCESS;
            info = (ULONG_PTR)r;
        }
        break;

    case IRP_MN_QUERY_PNP_DEVICE_STATE:
        status = STATUS_SUCCESS;
        break;

    default:
        // QUERY_INTERFACE, resources, DEVICE_ENUMERATED, ...: as they arrived.
        break;
    }

    return AhBusComplete(Irp, status, info);
}

#pragma code_seg()
static NTSTATUS
AhBusPdoDispatch(_In_ PDEVICE_OBJECT Pdo, _Inout_ PIRP Irp)
{
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);

    switch (sl->MajorFunction)
    {
    case IRP_MJ_PNP:
        return AhBusPdoPnp(Pdo, Irp);

    case IRP_MJ_POWER:
    {
        NTSTATUS status = Irp->IoStatus.Status;
        if (sl->MinorFunction == IRP_MN_SET_POWER)
        {
            if (sl->Parameters.Power.Type == DevicePowerState)
            {
                (VOID)PoSetPowerState(Pdo, DevicePowerState, sl->Parameters.Power.State);
            }
            status = STATUS_SUCCESS;
        }
        else if (sl->MinorFunction == IRP_MN_QUERY_POWER)
        {
            status = STATUS_SUCCESS;
        }
        PoStartNextPowerIrp(Irp);
        Irp->IoStatus.Status = status;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return status;
    }

    case IRP_MJ_SYSTEM_CONTROL:
    {
        // We are not a WMI provider: complete without touching the status.
        NTSTATUS status = Irp->IoStatus.Status;
        IoCompleteRequest(Irp, IO_NO_INCREMENT);
        return status;
    }

    default:
        return AhBusComplete(Irp, STATUS_INVALID_DEVICE_REQUEST, 0);
    }
}

//-----------------------------------------------------------------------------
// Bus FDO
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static NTSTATUS
AhBusFdoPnp(_In_ PDEVICE_OBJECT Fdo, _Inout_ PIRP Irp)
{
    PAGED_CODE();

    PAH_BUS_FDO_EXT bus = (PAH_BUS_FDO_EXT)Fdo->DeviceExtension;
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);
    NTSTATUS status;

    switch (sl->MinorFunction)
    {
    case IRP_MN_START_DEVICE:
        status = IoForwardIrpSynchronously(bus->Lower, Irp) ? Irp->IoStatus.Status
                                                             : STATUS_UNSUCCESSFUL;
        if (NT_SUCCESS(status))
        {
            AH_BUS_LOCK();
            bus->Started = TRUE;
            BOOLEAN changed = FALSE;
            (VOID)AhBusSyncLocked(&changed);
            AH_BUS_UNLOCK();
            AhBusFlushRenames();     // no driver lock held here
            DPF(D_TERSE, ("[AhBus] started"));
        }
        return AhBusComplete(Irp, status, Irp->IoStatus.Information);

    case IRP_MN_QUERY_DEVICE_RELATIONS:
        if (sl->Parameters.QueryDeviceRelations.Type == BusRelations)
        {
            AH_BUS_LOCK();
            PDEVICE_RELATIONS old = (PDEVICE_RELATIONS)Irp->IoStatus.Information;
            ULONG prev = (old != NULL) ? old->Count : 0;
            ULONG present = 0;
            for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
            {
                if (g_AhBusChildren[c] != NULL &&
                    ((PAH_BUS_PDO_EXT)g_AhBusChildren[c]->DeviceExtension)->Present)
                {
                    present++;
                }
            }
            SIZE_T cb = FIELD_OFFSET(DEVICE_RELATIONS, Objects) +
                        sizeof(PDEVICE_OBJECT) * (prev + present + 1);
            PDEVICE_RELATIONS r = (PDEVICE_RELATIONS)ExAllocatePool2(POOL_FLAG_PAGED, cb, AH_BUS_TAG);
            if (r == NULL)
            {
                AH_BUS_UNLOCK();
                return AhBusComplete(Irp, STATUS_INSUFFICIENT_RESOURCES, Irp->IoStatus.Information);
            }
            r->Count = 0;
            for (ULONG i = 0; i < prev; i++)
            {
                r->Objects[r->Count++] = old->Objects[i];
            }
            for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
            {
                PDEVICE_OBJECT pdo = g_AhBusChildren[c];
                if (pdo == NULL)
                {
                    continue;
                }
                PAH_BUS_PDO_EXT x = (PAH_BUS_PDO_EXT)pdo->DeviceExtension;
                if (!x->Present)
                {
                    continue;
                }
                x->Reported = TRUE;
                r->Objects[r->Count++] = pdo;
                ObReferenceObject(pdo);
            }
            if (old != NULL)
            {
                ExFreePool(old);
            }
            AH_BUS_UNLOCK();
            Irp->IoStatus.Information = (ULONG_PTR)r;
            Irp->IoStatus.Status = STATUS_SUCCESS;
        }
        IoSkipCurrentIrpStackLocation(Irp);
        return IoCallDriver(bus->Lower, Irp);

    case IRP_MN_QUERY_STOP_DEVICE:
    case IRP_MN_CANCEL_STOP_DEVICE:
    case IRP_MN_QUERY_REMOVE_DEVICE:
    case IRP_MN_CANCEL_REMOVE_DEVICE:
        Irp->IoStatus.Status = STATUS_SUCCESS;
        IoSkipCurrentIrpStackLocation(Irp);
        return IoCallDriver(bus->Lower, Irp);

    case IRP_MN_STOP_DEVICE:
    case IRP_MN_SURPRISE_REMOVAL:
        AH_BUS_LOCK();
        bus->Started = FALSE;
        AH_BUS_UNLOCK();
        Irp->IoStatus.Status = STATUS_SUCCESS;
        IoSkipCurrentIrpStackLocation(Irp);
        return IoCallDriver(bus->Lower, Irp);

    case IRP_MN_REMOVE_DEVICE:
    {
        AH_BUS_LOCK();
        bus->Started = FALSE;
        g_AhBus = NULL;
        AH_BUS_UNLOCK();

        Irp->IoStatus.Status = STATUS_SUCCESS;
        IoSkipCurrentIrpStackLocation(Irp);
        status = IoCallDriver(bus->Lower, Irp);

        //
        // Plug and Play has already removed every child stack. What is left
        // in the table are PDOs it still considered present; they go with us.
        //
        ULONG deleted = 0;
        AH_BUS_LOCK();
        for (ULONG c = 0; c < AH_BUS_MAX_CHILDREN; c++)
        {
            PDEVICE_OBJECT pdo = g_AhBusChildren[c];
            if (pdo == NULL)
            {
                continue;
            }
            g_AhBusChildren[c] = NULL;
            AhBusRetirePdoLocked((PAH_BUS_PDO_EXT)pdo->DeviceExtension);
            AhBusObjRemove(pdo);
            IoDeleteDevice(pdo);
            deleted++;
        }
        AH_BUS_UNLOCK();
        DPF(D_TERSE, ("[AhBus] REMOVE: %u child PDOs deleted", deleted));

        //
        // The control device lives exactly as long as the bus (spike S4):
        // created in DriverEntry it kept the image loaded forever, and every
        // driver package change needed a reboot.
        //
        AhCtlDeleteDevice(Fdo->DriverObject);

        IoDetachDevice(bus->Lower);
        AhBusObjRemove(Fdo);
        IoDeleteDevice(Fdo);
        return status;
    }

    default:
        IoSkipCurrentIrpStackLocation(Irp);
        return IoCallDriver(bus->Lower, Irp);
    }
}

#pragma code_seg()
static NTSTATUS
AhBusFdoDispatch(_In_ PDEVICE_OBJECT Fdo, _Inout_ PIRP Irp)
{
    PAH_BUS_FDO_EXT bus = (PAH_BUS_FDO_EXT)Fdo->DeviceExtension;
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);

    switch (sl->MajorFunction)
    {
    case IRP_MJ_PNP:
        return AhBusFdoPnp(Fdo, Irp);
    case IRP_MJ_POWER:
        PoStartNextPowerIrp(Irp);
        IoSkipCurrentIrpStackLocation(Irp);
        return PoCallDriver(bus->Lower, Irp);
    case IRP_MJ_SYSTEM_CONTROL:
        IoSkipCurrentIrpStackLocation(Irp);
        return IoCallDriver(bus->Lower, Irp);
    case IRP_MJ_CREATE:
    case IRP_MJ_CLOSE:
    case IRP_MJ_CLEANUP:
        //
        // The bus FDO has no name and registers no interface; the control
        // plane is the separate control device. Opening the devnode through
        // some other path gets a handle that can do nothing.
        //
        return AhBusComplete(Irp, STATUS_SUCCESS, 0);
    default:
        return AhBusComplete(Irp, STATUS_INVALID_DEVICE_REQUEST, 0);
    }
}

#pragma code_seg("PAGE")
NTSTATUS
AhBusAddDevice(_In_ PDRIVER_OBJECT DriverObject, _In_ PDEVICE_OBJECT Pdo)
{
    PAGED_CODE();

    AH_BUS_LOCK();
    BOOLEAN busy = (g_AhBus != NULL) ? TRUE : FALSE;
    AH_BUS_UNLOCK();
    if (busy)
    {
        //
        // One bus per image: the control device name and the slot table are
        // driver-global. A second ROOT\AudioHubVad devnode is a broken
        // install, and refusing it is visible in Device Manager.
        //
        DPF(D_ERROR, ("[AhBus] a second root devnode was refused"));
        return STATUS_DEVICE_ALREADY_ATTACHED;
    }

    //
    // An UNNAMED FDO: IoCreateDeviceSecure refuses an unnamed object without
    // FILE_AUTOGENERATED_DEVICE_NAME (STATUS_INVALID_PARAMETER, devnode
    // problem 31), and an FDO must not be autogenerated-named. Nothing opens
    // the bus by name; the stack's security comes from the INF HW AddReg
    // (DeviceType/Security), which PnP applies to every object in the stack.
    //
    PDEVICE_OBJECT fdo = NULL;
    NTSTATUS status = IoCreateDevice(
        DriverObject, sizeof(AH_BUS_FDO_EXT), NULL, FILE_DEVICE_BUS_EXTENDER,
        FILE_DEVICE_SECURE_OPEN, FALSE, &fdo);
    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhBus] IoCreateDevice failed 0x%x", status));
        return status;
    }

    PAH_BUS_FDO_EXT bus = (PAH_BUS_FDO_EXT)fdo->DeviceExtension;
    RtlZeroMemory(bus, sizeof(*bus));
    bus->Magic = AH_BUS_FDO_MAGIC;
    bus->Self  = fdo;
    bus->Pdo   = Pdo;
    bus->Lower = IoAttachDeviceToDeviceStack(fdo, Pdo);
    if (bus->Lower == NULL)
    {
        IoDeleteDevice(fdo);
        return STATUS_NO_SUCH_DEVICE;
    }
    AhBusObjAdd(fdo);
    fdo->Flags |= DO_POWER_PAGABLE;
    fdo->Flags &= ~DO_DEVICE_INITIALIZING;

    AH_BUS_LOCK();
    g_AhBus = bus;
    AH_BUS_UNLOCK();

    //
    // Control plane. A failure is NOT fatal: the bus without it publishes
    // nothing and hurts nobody, whereas failing AddDevice takes the devnode
    // down and has to be recovered by hand on a machine that only answers
    // over SSH.
    //
    //
    // Caller-identity policy lives in the ROOT devnode's hardware key
    // (AudioHubDaemonImage), which only an administrator can write. Loaded
    // BEFORE the control device exists, so there is no window in which a
    // caller can open it under a weaker check than the configured one.
    //
    AhCtlLoadPolicy(Pdo);

    NTSTATUS ctl = AhCtlCreateDevice(DriverObject);
    if (!NT_SUCCESS(ctl))
    {
        DPF(D_ERROR, ("[AhBus] AhCtlCreateDevice failed 0x%x; no control plane", ctl));
    }
    return STATUS_SUCCESS;
}

//-----------------------------------------------------------------------------
// Router
//-----------------------------------------------------------------------------

#pragma code_seg()
static NTSTATUS
AhBusRoute(_In_ PDEVICE_OBJECT DeviceObject, _Inout_ PIRP Irp)
{
    ULONG kind = AhBusKind(DeviceObject);
    if (kind == AH_KIND_FDO)
    {
        return AhBusFdoDispatch(DeviceObject, Irp);
    }
    if (kind == AH_KIND_PDO)
    {
        return AhBusPdoDispatch(DeviceObject, Irp);
    }
    UCHAR mj = IoGetCurrentIrpStackLocation(Irp)->MajorFunction;
    return g_AhBusSaved[mj](DeviceObject, Irp);
}

#pragma code_seg("PAGE")
VOID
AhBusInstallRouter(_In_ PDRIVER_OBJECT DriverObject)
{
    PAGED_CODE();

    for (ULONG i = 0; i <= IRP_MJ_MAXIMUM_FUNCTION; i++)
    {
        g_AhBusSaved[i] = DriverObject->MajorFunction[i];
        DriverObject->MajorFunction[i] = AhBusRoute;
    }
}

#pragma code_seg("PAGE")
VOID
AhBusDriverInit(VOID)
{
    PAGED_CODE();

    KeInitializeSpinLock(&g_AhBusObjLock);
    KeInitializeMutex(&g_AhBusLock, 0);
    RtlZeroMemory(g_AhBusObjs, sizeof(g_AhBusObjs));
    RtlZeroMemory(g_AhBusChildren, sizeof(g_AhBusChildren));
    RtlZeroMemory(g_AhBusWant, sizeof(g_AhBusWant));
    RtlZeroMemory(g_AhBusWantCount, sizeof(g_AhBusWantCount));
    g_AhBus = NULL;
}
#pragma code_seg()
