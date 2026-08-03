/*++

Module Name:

    ctldevice.cpp

Abstract:

    Control device object + IOCTL dispatch for the AudioHub virtual audio
    driver. This is the whole control plane; there is no data plane yet.

    Three defences, in the order they run:

      1. The kernel DACL, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)". IU is
         Interactive Users -- local and RDP logons only, NOT service accounts
         and NOT network logons. That is the Windows equivalent of the macOS
         bridge's "the audit token's euid must own the console session", except
         the kernel enforces it at open time instead of us enforcing it per
         message. FILE_DEVICE_SECURE_OPEN extends the DACL over the device's
         namespace so \\.\AudioHubVadCtl\anything cannot slip past it.

      2. SeLocateProcessImageName against a path an administrator wrote under
         HKLM. This closes the half of the hole the macOS side documents as
         still open ("it does NOT stop another process of the same user").
         The name comes from EPROCESS, captured by the kernel at process
         creation -- unlike the PEB's ImagePathName, the process cannot rewrite
         it. Crucially this needs no code-signing identity, so it works during
         test-signing; the macOS equivalent is blocked on a Developer ID.

      3. First-open-wins. The FILE_OBJECT *is* the session, so "every later
         message came from the process that completed the handshake" is
         structural rather than re-verified per message. Two mechanisms the
         macOS bridge needs therefore do not exist here at all: Superseded
         (nobody can take the session away, a second daemon is refused) and the
         1 Hz heartbeat (IRP_MJ_CLEANUP is a deterministic death notice).

--*/

#pragma warning (disable : 4127)

#include "definitions.h"
#include "endpoints.h"
#include "perpeer.h"
#include "ctldevice.h"
#include <wdmsec.h>

//
// Declared by hand rather than by including ntifs.h: ntifs.h and the ntddk.h
// that portcls.h drags in cannot both be included in one translation unit.
//
extern "C"
NTSTATUS NTAPI
SeLocateProcessImageName(
    _In_ PEPROCESS Process,
    _Outptr_ PUNICODE_STRING *pImageFileName
    );

//
// {c8f3a5e1-2b74-4a19-9f5d-6d0d2a3f7c41} -- our own device class GUID for the
// control device. Not an audio class: this object is not an endpoint and must
// never be enumerated as one.
//
// Written out rather than declared with DEFINE_GUID, which only EMITS storage
// when INITGUID was defined before the first <guiddef.h> include -- and
// portcls.h has pulled that in long before this file gets a say.
//
static const GUID GUID_DEVCLASS_AUDIOHUB_CTL = {
    0xc8f3a5e1, 0x2b74, 0x4a19, { 0x9f, 0x5d, 0x6d, 0x0d, 0x2a, 0x3f, 0x7c, 0x41 }
};

#define AH_CTL_POOLTAG  'CbhA'

//-----------------------------------------------------------------------------
// State
//-----------------------------------------------------------------------------

static PDEVICE_OBJECT   g_AhCtlDevice   = NULL;
static BOOLEAN          g_AhCtlSymlink  = FALSE;

//
// The PortCls dispatch entries we displaced. Every IRP for anything that is not
// the control device goes straight back to them.
//
static PDRIVER_DISPATCH g_PcCreate      = NULL;
static PDRIVER_DISPATCH g_PcClose       = NULL;
static PDRIVER_DISPATCH g_PcCleanup     = NULL;
static PDRIVER_DISPATCH g_PcDeviceControl = NULL;

//
// The one FILE_OBJECT allowed to drive the control plane. Touched with
// interlocked operations only.
//
static PVOID            g_SessionFile   = NULL;
static ULONGLONG        g_SessionId     = 0;
static BOOLEAN          g_SessionGreeted = FALSE;

//
// The inverted-call IRP (driver -> daemon). At most one outstanding.
//
static KSPIN_LOCK       g_PendLock;
static PIRP             g_PendIrp       = NULL;

//
// Caller-identity policy, loaded from the device software key at StartDevice.
//
static ULONG            g_ClientCheck   = AH_CLIENT_CHECK_ACL_ONLY;
static UNICODE_STRING   g_ExpectedImage = { 0, 0, NULL };

//-----------------------------------------------------------------------------
// Identity
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static BOOLEAN
AhSuffixMatchesI(
    _In_ PCUNICODE_STRING Actual,
    _In_ PCUNICODE_STRING Suffix
    )
{
    PAGED_CODE();

    if (Suffix->Length == 0 || Actual->Length < Suffix->Length)
    {
        return FALSE;
    }

    UNICODE_STRING tail;
    tail.Buffer        = (PWCH)((PUCHAR)Actual->Buffer + (Actual->Length - Suffix->Length));
    tail.Length        = Suffix->Length;
    tail.MaximumLength = Suffix->Length;

    return RtlEqualUnicodeString(&tail, Suffix, TRUE) ? TRUE : FALSE;
}

#pragma code_seg("PAGE")
BOOLEAN
AhCtlImagePathMatches(
    _In_ PCUNICODE_STRING Actual,
    _In_ PCUNICODE_STRING Expected
    )
/*++

Routine Description:

    SeLocateProcessImageName's output form is not contractual: depending on the
    build and on how the volume is mounted it can be "\??\C:\dir\app.exe" or
    "\Device\HarddiskVolume3\dir\app.exe". An installer cannot know which, so
    two comparisons are accepted:

      * exact, case-insensitive -- what an operator gets by copying the path
        this driver logs on the first refused open;
      * volume-relative suffix -- the registry value's drive letter is stripped
        and the remainder must terminate the actual path AT A PATH BOUNDARY.

    The boundary check is what stops "C:\x\evil-audiohubd.exe" from matching an
    expected "...\audiohubd.exe"; without it the suffix rule would be worse
    than no rule.

    Residual weakness, stated rather than hidden: the suffix form cannot tell
    two volumes with the same directory layout apart. The deployment answer is
    to install the daemon somewhere only administrators can write, which is a
    property of where the file lives, not of this comparison.

--*/
{
    PAGED_CODE();

    if (Actual == NULL || Expected == NULL ||
        Actual->Buffer == NULL || Expected->Buffer == NULL ||
        Actual->Length == 0 || Expected->Length == 0)
    {
        return FALSE;
    }

    if (RtlEqualUnicodeString((PCUNICODE_STRING)Actual, (PCUNICODE_STRING)Expected, TRUE))
    {
        return TRUE;
    }

    //
    // Strip a leading "X:" (and nothing else) from the expected DOS path.
    //
    UNICODE_STRING rel = *Expected;
    if (rel.Length >= 2 * sizeof(WCHAR) && rel.Buffer[1] == L':')
    {
        rel.Buffer  = rel.Buffer + 2;
        rel.Length  = (USHORT)(rel.Length - 2 * sizeof(WCHAR));
        rel.MaximumLength = rel.Length;
    }

    if (rel.Length == 0 || rel.Buffer[0] != L'\\')
    {
        //
        // Not an absolute remainder: refuse rather than match a bare file name,
        // which would let any copy of the executable in.
        //
        return FALSE;
    }

    return AhSuffixMatchesI(Actual, &rel);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhCheckCallerImage(VOID)
{
    PAGED_CODE();

    if (g_ClientCheck < AH_CLIENT_CHECK_IMAGEPATH)
    {
        return STATUS_SUCCESS;
    }

    PUNICODE_STRING image = NULL;
    NTSTATUS status = SeLocateProcessImageName(PsGetCurrentProcess(), &image);
    if (!NT_SUCCESS(status) || image == NULL)
    {
        DPF(D_ERROR, ("[AhCheckCallerImage] SeLocateProcessImageName failed 0x%x", status));
        return STATUS_ACCESS_DENIED;
    }

    BOOLEAN ok = AhCtlImagePathMatches(image, &g_ExpectedImage);
    if (!ok)
    {
        //
        // Logging the observed path is the bring-up path: an operator reads it
        // out of the debugger / DebugView and writes it verbatim into
        // HKLM\...\Device Parameters\AudioHubDaemonImage.
        //
        DPF(D_ERROR, ("[AhCheckCallerImage] refused '%wZ' (expected '%wZ')", image, &g_ExpectedImage));
    }

    ExFreePool(image);
    return ok ? STATUS_SUCCESS : STATUS_ACCESS_DENIED;
}

#pragma code_seg("PAGE")
VOID
AhCtlLoadPolicy(
    _In_ PDEVICE_OBJECT PhysicalDeviceObject
    )
{
    PAGED_CODE();

    HANDLE hKey = NULL;
    NTSTATUS status = IoOpenDeviceRegistryKey(
        PhysicalDeviceObject, PLUGPLAY_REGKEY_DEVICE, KEY_READ, &hKey);
    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhCtlLoadPolicy] IoOpenDeviceRegistryKey failed 0x%x; ACL-only", status));
        return;
    }

    UNICODE_STRING valueName;
    ULONG len = 0;
    PKEY_VALUE_PARTIAL_INFORMATION info = NULL;

    RtlInitUnicodeString(&valueName, AH_REGVAL_DAEMON_IMAGE);
    status = ZwQueryValueKey(hKey, &valueName, KeyValuePartialInformation, NULL, 0, &len);
    if ((status == STATUS_BUFFER_TOO_SMALL || status == STATUS_BUFFER_OVERFLOW) && len > 0)
    {
        info = (PKEY_VALUE_PARTIAL_INFORMATION)
            ExAllocatePool2(POOL_FLAG_PAGED, len, AH_CTL_POOLTAG);
        if (info != NULL)
        {
            status = ZwQueryValueKey(hKey, &valueName, KeyValuePartialInformation, info, len, &len);
            if (NT_SUCCESS(status) &&
                (info->Type == REG_SZ || info->Type == REG_EXPAND_SZ) &&
                info->DataLength >= sizeof(WCHAR))
            {
                USHORT bytes = (USHORT)min(info->DataLength, (ULONG)(MAXUSHORT - sizeof(WCHAR)));
                PWCH buf = (PWCH)ExAllocatePool2(POOL_FLAG_PAGED, bytes + sizeof(WCHAR), AH_CTL_POOLTAG);
                if (buf != NULL)
                {
                    RtlCopyMemory(buf, info->Data, bytes);
                    buf[bytes / sizeof(WCHAR)] = L'\0';

                    //
                    // The registry value may or may not carry its own
                    // terminator; measure the string rather than trusting it.
                    //
                    USHORT chars = 0;
                    while (chars < bytes / sizeof(WCHAR) && buf[chars] != L'\0')
                    {
                        chars++;
                    }

                    if (chars > 0)
                    {
                        if (g_ExpectedImage.Buffer != NULL)
                        {
                            ExFreePoolWithTag(g_ExpectedImage.Buffer, AH_CTL_POOLTAG);
                        }
                        g_ExpectedImage.Buffer        = buf;
                        g_ExpectedImage.Length        = (USHORT)(chars * sizeof(WCHAR));
                        g_ExpectedImage.MaximumLength = (USHORT)(bytes + sizeof(WCHAR));
                        g_ClientCheck = AH_CLIENT_CHECK_IMAGEPATH;
                        DPF(D_TERSE, ("[AhCtlLoadPolicy] daemon image pinned to '%wZ'", &g_ExpectedImage));
                        buf = NULL;
                    }
                    if (buf != NULL)
                    {
                        ExFreePoolWithTag(buf, AH_CTL_POOLTAG);
                    }
                }
            }
            ExFreePoolWithTag(info, AH_CTL_POOLTAG);
        }
    }

    //
    // Optional explicit override. Only ever used to LOWER the level for
    // debugging, and doing so is visible in every AH_HELLO_REPLY.
    //
    RtlInitUnicodeString(&valueName, AH_REGVAL_CLIENT_CHECK);
    len = 0;
    status = ZwQueryValueKey(hKey, &valueName, KeyValuePartialInformation, NULL, 0, &len);
    if ((status == STATUS_BUFFER_TOO_SMALL || status == STATUS_BUFFER_OVERFLOW) && len > 0)
    {
        info = (PKEY_VALUE_PARTIAL_INFORMATION)
            ExAllocatePool2(POOL_FLAG_PAGED, len, AH_CTL_POOLTAG);
        if (info != NULL)
        {
            status = ZwQueryValueKey(hKey, &valueName, KeyValuePartialInformation, info, len, &len);
            if (NT_SUCCESS(status) && info->Type == REG_DWORD && info->DataLength == sizeof(ULONG))
            {
                ULONG want = *(ULONG UNALIGNED *)info->Data;
                if (want <= AH_CLIENT_CHECK_IMAGEPATH)
                {
                    if (want == AH_CLIENT_CHECK_IMAGEPATH && g_ExpectedImage.Buffer == NULL)
                    {
                        DPF(D_ERROR, ("[AhCtlLoadPolicy] IMAGEPATH requested with no image configured"));
                    }
                    else
                    {
                        g_ClientCheck = want;
                    }
                }
            }
            ExFreePoolWithTag(info, AH_CTL_POOLTAG);
        }
    }

    if (g_ClientCheck < AH_CLIENT_CHECK_IMAGEPATH)
    {
        DPF(D_ERROR, ("[AhCtlLoadPolicy] running at client_check=%u (ACL only): set "
                      "AudioHubDaemonImage in the device software key to raise it",
                      g_ClientCheck));
    }

    ZwClose(hKey);
}

//-----------------------------------------------------------------------------
// The pending (inverted-call) IRP
//-----------------------------------------------------------------------------

#pragma code_seg()
static VOID
AhCtlCancelPend(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP Irp
    )
{
    UNREFERENCED_PARAMETER(DeviceObject);

    IoReleaseCancelSpinLock(Irp->CancelIrql);

    KIRQL irql;
    KeAcquireSpinLock(&g_PendLock, &irql);
    if (g_PendIrp == Irp)
    {
        g_PendIrp = NULL;
    }
    KeReleaseSpinLock(&g_PendLock, irql);

    Irp->IoStatus.Status = STATUS_CANCELLED;
    Irp->IoStatus.Information = 0;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
}

#pragma code_seg()
static VOID
AhCtlDrainPend(
    _In_ NTSTATUS Status
    )
{
    KIRQL irql;
    PIRP irp = NULL;

    KeAcquireSpinLock(&g_PendLock, &irql);
    if (g_PendIrp != NULL)
    {
        if (IoSetCancelRoutine(g_PendIrp, NULL) != NULL)
        {
            irp = g_PendIrp;
        }
        //
        // If IoSetCancelRoutine returns NULL the cancel routine has already
        // taken ownership and will complete the IRP itself; touching it here
        // would be a double completion.
        //
        g_PendIrp = NULL;
    }
    KeReleaseSpinLock(&g_PendLock, irql);

    if (irp != NULL)
    {
        irp->IoStatus.Status = Status;
        irp->IoStatus.Information = 0;
        IoCompleteRequest(irp, IO_NO_INCREMENT);
    }
}

#pragma code_seg()
static NTSTATUS
AhCtlQueuePend(
    _In_ PIRP Irp
    )
{
    KIRQL irql;

    KeAcquireSpinLock(&g_PendLock, &irql);

    if (g_PendIrp != NULL)
    {
        KeReleaseSpinLock(&g_PendLock, irql);
        return STATUS_DEVICE_BUSY;
    }

    IoSetCancelRoutine(Irp, AhCtlCancelPend);

    if (Irp->Cancel)
    {
        if (IoSetCancelRoutine(Irp, NULL) != NULL)
        {
            KeReleaseSpinLock(&g_PendLock, irql);
            return STATUS_CANCELLED;
        }
        //
        // The cancel routine is already running and owns the IRP.
        //
        KeReleaseSpinLock(&g_PendLock, irql);
        IoMarkIrpPending(Irp);
        return STATUS_PENDING;
    }

    IoMarkIrpPending(Irp);
    g_PendIrp = Irp;
    KeReleaseSpinLock(&g_PendLock, irql);

    return STATUS_PENDING;
}

//-----------------------------------------------------------------------------
// Dispatch
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
static NTSTATUS
AhCompleteIrp(
    _In_ PIRP Irp,
    _In_ NTSTATUS Status,
    _In_ ULONG_PTR Information
    )
{
    PAGED_CODE();

    Irp->IoStatus.Status = Status;
    Irp->IoStatus.Information = Information;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Status;
}

#pragma code_seg("PAGE")
extern "C"
NTSTATUS
AhCtlCreate(
    _In_ PDEVICE_OBJECT DeviceObject,
    _Inout_ PIRP Irp
    )
{
    PAGED_CODE();

    if (DeviceObject != g_AhCtlDevice)
    {
        return g_PcCreate(DeviceObject, Irp);
    }

    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(Irp);

    NTSTATUS status = AhCheckCallerImage();
    if (!NT_SUCCESS(status))
    {
        return AhCompleteIrp(Irp, status, 0);
    }

    //
    // First open wins. NOT a takeover: the incumbent daemon keeps working and
    // the newcomer is told the device is busy. A takeover would need the whole
    // "Superseded" apparatus the macOS bridge carries, and its only purpose
    // there is to stop the displaced daemon from oscillating.
    //
    PVOID prev = InterlockedCompareExchangePointer(&g_SessionFile, stack->FileObject, NULL);
    if (prev != NULL)
    {
        DPF(D_ERROR, ("[AhCtlCreate] refused: a control session is already open"));
        return AhCompleteIrp(Irp, STATUS_DEVICE_BUSY, 0);
    }

    g_SessionGreeted = FALSE;
    DPF(D_TERSE, ("[AhCtlCreate] control session opened by pid %p",
                  PsGetCurrentProcessId()));

    return AhCompleteIrp(Irp, STATUS_SUCCESS, 0);
}

#pragma code_seg("PAGE")
extern "C"
NTSTATUS
AhCtlClose(
    _In_ PDEVICE_OBJECT DeviceObject,
    _Inout_ PIRP Irp
    )
{
    PAGED_CODE();

    if (DeviceObject != g_AhCtlDevice)
    {
        return g_PcClose(DeviceObject, Irp);
    }

    return AhCompleteIrp(Irp, STATUS_SUCCESS, 0);
}

#pragma code_seg("PAGE")
extern "C"
NTSTATUS
AhCtlCleanup(
    _In_ PDEVICE_OBJECT DeviceObject,
    _Inout_ PIRP Irp
    )
/*++

Routine Description:

    Runs in the context of the process closing the handle, which makes it a
    deterministic death notice -- no heartbeat needed.

    BINDINGS DELIBERATELY SURVIVE. plan §7.3 says a paired peer's devices stay
    in the system list whether or not anything is connected, and "the daemon
    died" is not "the peer was unpaired". Tearing the endpoints down here would
    make every daemon restart yank the user's default output away.

--*/
{
    PAGED_CODE();

    if (DeviceObject != g_AhCtlDevice)
    {
        return g_PcCleanup(DeviceObject, Irp);
    }

    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(Irp);

    if (InterlockedCompareExchangePointer(&g_SessionFile, NULL, stack->FileObject)
        == stack->FileObject)
    {
        g_SessionGreeted = FALSE;
        AhCtlDrainPend(STATUS_CANCELLED);
        DPF(D_TERSE, ("[AhCtlCleanup] control session closed; bindings kept"));
    }

    return AhCompleteIrp(Irp, STATUS_SUCCESS, 0);
}

#pragma code_seg("PAGE")
extern "C"
NTSTATUS
AhCtlDeviceControl(
    _In_ PDEVICE_OBJECT DeviceObject,
    _Inout_ PIRP Irp
    )
{
    PAGED_CODE();

    if (DeviceObject != g_AhCtlDevice)
    {
        return g_PcDeviceControl(DeviceObject, Irp);
    }

    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(Irp);
    ULONG code    = stack->Parameters.DeviceIoControl.IoControlCode;
    ULONG inLen   = stack->Parameters.DeviceIoControl.InputBufferLength;
    ULONG outLen  = stack->Parameters.DeviceIoControl.OutputBufferLength;
    PVOID buffer  = Irp->AssociatedIrp.SystemBuffer;

    //
    // Every message must come from the FILE_OBJECT that won the session. This
    // replaces the macOS bridge's per-message audit-token comparison with a
    // pointer compare the I/O manager already did the work for.
    //
    if (stack->FileObject != g_SessionFile)
    {
        return AhCompleteIrp(Irp, STATUS_INVALID_DEVICE_STATE, 0);
    }

    switch (code)
    {
    case IOCTL_AUDIOHUB_HELLO:
    {
        //
        // EXACT sizes, never ">=". The geometry belongs to this driver, not to
        // whatever the caller happened to send.
        //
        if (inLen != sizeof(AH_HELLO_REQUEST) || outLen != sizeof(AH_HELLO_REPLY) || buffer == NULL)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_PARAMETER, 0);
        }

        AH_HELLO_REQUEST req = *(AH_HELLO_REQUEST *)buffer;
        AH_HELLO_REPLY  *rep = (AH_HELLO_REPLY *)buffer;

        RtlZeroMemory(rep, sizeof(*rep));
        rep->protocol_version = AUDIOHUB_WIN_PROTOCOL_VERSION;
        rep->slot_count       = AUDIOHUB_WIN_MAX_SLOTS;
        rep->caps             = 0;      // no data plane yet
        rep->sample_rate      = 0;
        rep->out_channels     = 0;
        rep->in_channels      = 0;
        rep->client_check     = g_ClientCheck;

        if (req.protocol_version != AUDIOHUB_WIN_PROTOCOL_VERSION)
        {
            rep->status     = AH_STATUS_BAD_VERSION;
            rep->session_id = 0;
            return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
        }

        g_SessionId++;
        g_SessionGreeted = TRUE;
        rep->status     = AH_STATUS_OK;
        rep->session_id = g_SessionId;

        DPF(D_TERSE, ("[AhCtl] HELLO ok, session %I64u, client_check %u",
                      g_SessionId, g_ClientCheck));
        return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
    }

    case IOCTL_AUDIOHUB_BIND_SET:
    case IOCTL_AUDIOHUB_BIND_CLEAR:
    {
        if (inLen != sizeof(AH_BIND_REQUEST) || outLen != sizeof(AH_BIND_REPLY) || buffer == NULL)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_PARAMETER, 0);
        }
        if (!g_SessionGreeted)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_DEVICE_STATE, 0);
        }

        //
        // Copy out of the shared buffer before touching it: the reply is
        // written into the SAME buffer (METHOD_BUFFERED), so reading the
        // request after starting to build the reply reads the reply back.
        //
        AH_BIND_REQUEST req = *(AH_BIND_REQUEST *)buffer;
        AH_BIND_REPLY  *rep = (AH_BIND_REPLY *)buffer;

        //
        // The receiver terminates what it receives. Never the sender.
        //
        req.peer_key[AH_PEERKEY_BUF - 1] = '\0';
        req.display[AH_DISPLAY_CHARS - 1] = L'\0';

        RtlZeroMemory(rep, sizeof(*rep));
        rep->slot = req.slot;

        if (req.session_id != g_SessionId)
        {
            rep->status = AH_STATUS_STALE_SESSION;
            return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
        }

        ULONG ahStatus = AH_STATUS_INTERNAL;
        ULONG generation = 0;
        ULONG state = AH_SLOT_FREE;
        AH_OP_RESULT result;
        RtlZeroMemory(&result, sizeof(result));

        if (code == IOCTL_AUDIOHUB_BIND_SET)
        {
            if (req.op != AH_BIND_SET)
            {
                rep->status = AH_STATUS_BAD_ARGUMENT;
                return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
            }
            (VOID)AhSlotBindSet(req.slot, req.peer_key, req.display, req.flags,
                                &generation, &state, &ahStatus, &result);
        }
        else
        {
            if (req.op != AH_BIND_CLEAR)
            {
                rep->status = AH_STATUS_BAD_ARGUMENT;
                return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
            }
            (VOID)AhSlotBindClear(req.slot, req.generation, req.flags,
                                  &state, &ahStatus, &result);
        }

        //
        // Last line of defence for the invariant. Everything below this point
        // has already been checked inside perpeer.cpp, but the promise
        // "AH_STATUS_OK on a SET means both endpoints exist" is what the daemon
        // and every test lean on, so it is re-checked at the boundary where it
        // is actually made. A driver that gets this wrong has to say so.
        //
        if (ahStatus == AH_STATUS_OK && code == IOCTL_AUDIOHUB_BIND_SET &&
            result.Published != AH_PUB_BOTH)
        {
            DPF(D_ERROR, ("[AhCtlDeviceControl] slot %u would have reported OK with published 0x%x",
                          req.slot, result.Published));
            ahStatus = AH_STATUS_PARTIAL;
            if (result.Stage == AH_STAGE_NONE) { result.Stage = AH_STAGE_VERIFY; }
            state = AH_SLOT_FREE;
        }

        rep->status     = ahStatus;
        rep->slot       = req.slot;
        rep->generation = generation;
        rep->state      = state;
        rep->stage      = result.Stage;
        rep->nt_status  = (UINT32)result.NtStatus;
        rep->published  = result.Published;
        //
        // Degradations that do not make the call a failure. Today that is only
        // AH_BINDREPLY_FLAG_NAME_FALLBACK, and it is relayed rather than
        // dropped for the same reason `published` exists at all: a driver that
        // answers OK owes the caller everything it knows about how far short of
        // "OK" the result actually fell.
        //
        rep->flags      = result.Flags;
        return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(*rep));
    }

    case IOCTL_AUDIOHUB_QUERY_SLOTS:
    {
        if (inLen != 0 || outLen != sizeof(AH_QUERY_SLOTS_REPLY) || buffer == NULL)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_PARAMETER, 0);
        }
        AhSlotQuery((AH_QUERY_SLOTS_REPLY *)buffer, g_SessionId);
        return AhCompleteIrp(Irp, STATUS_SUCCESS, sizeof(AH_QUERY_SLOTS_REPLY));
    }

    case IOCTL_AUDIOHUB_CONTROL_PEND:
    {
        if (inLen != 0 || outLen != sizeof(AH_CONTROL_EVENT) || buffer == NULL)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_PARAMETER, 0);
        }
        if (!g_SessionGreeted)
        {
            return AhCompleteIrp(Irp, STATUS_INVALID_DEVICE_STATE, 0);
        }

        //
        // M6-2 has nothing to report: no volume node, no data plane. The IRP is
        // parked and completed on cleanup. Defining the call now is what stops
        // M6-3/M6-4 from having to bump the protocol version.
        //
        NTSTATUS status = AhCtlQueuePend(Irp);
        if (status == STATUS_PENDING)
        {
            return STATUS_PENDING;
        }
        return AhCompleteIrp(Irp, status, 0);
    }

    default:
        return AhCompleteIrp(Irp, STATUS_INVALID_DEVICE_REQUEST, 0);
    }
}

//-----------------------------------------------------------------------------
// Construction / destruction
//-----------------------------------------------------------------------------

#pragma code_seg("INIT")
NTSTATUS
AhCtlCreateDevice(
    _In_ PDRIVER_OBJECT DriverObject
    )
{
    NTSTATUS status;
    UNICODE_STRING deviceName;
    UNICODE_STRING symlinkName;
    UNICODE_STRING sddl;

    RtlInitUnicodeString(&deviceName, AH_CTL_DEVICE_NAME_W);
    RtlInitUnicodeString(&symlinkName, AH_CTL_SYMLINK_W);

    //
    // SY + BA get everything; IU (interactive logons) get read+write, which is
    // all the daemon needs. Deliberately NOT SDDL_DEVOBJ_SYS_ALL_ADM_ALL: that
    // one is documented as "No other users may access the device", and would
    // lock out the very process this device exists for.
    //
    RtlInitUnicodeString(&sddl, L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

    KeInitializeSpinLock(&g_PendLock);
    g_PendIrp     = NULL;
    g_SessionFile = NULL;
    g_SessionId   = 0;

    status = IoCreateDeviceSecure(
        DriverObject,
        0,                          // no device extension: all state is static
        &deviceName,
        FILE_DEVICE_UNKNOWN,
        FILE_DEVICE_SECURE_OPEN,    // the DACL covers the device's namespace too
        FALSE,                      // not exclusive; first-open-wins is ours
        &sddl,
        (LPCGUID)&GUID_DEVCLASS_AUDIOHUB_CTL,
        &g_AhCtlDevice);
    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhCtlCreateDevice] IoCreateDeviceSecure failed 0x%x", status));
        g_AhCtlDevice = NULL;
        return status;
    }

    status = IoCreateSymbolicLink(&symlinkName, &deviceName);
    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhCtlCreateDevice] IoCreateSymbolicLink failed 0x%x", status));
        IoDeleteDevice(g_AhCtlDevice);
        g_AhCtlDevice = NULL;
        return status;
    }
    g_AhCtlSymlink = TRUE;

    //
    // Hook the four entries we need, keeping PortCls's originals so every IRP
    // for an audio device object goes straight back to it. Same shape as the
    // IRP_MJ_PNP hook adapter.cpp already installs.
    //
    g_PcCreate        = DriverObject->MajorFunction[IRP_MJ_CREATE];
    g_PcClose         = DriverObject->MajorFunction[IRP_MJ_CLOSE];
    g_PcCleanup       = DriverObject->MajorFunction[IRP_MJ_CLEANUP];
    g_PcDeviceControl = DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL];

    DriverObject->MajorFunction[IRP_MJ_CREATE]         = AhCtlCreate;
    DriverObject->MajorFunction[IRP_MJ_CLOSE]          = AhCtlClose;
    DriverObject->MajorFunction[IRP_MJ_CLEANUP]        = AhCtlCleanup;
    DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL] = AhCtlDeviceControl;

    g_AhCtlDevice->Flags &= ~DO_DEVICE_INITIALIZING;

    DPF(D_TERSE, ("[AhCtlCreateDevice] %S ready", AH_CTL_SYMLINK_W));
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
VOID
AhCtlDeleteDevice(
    _In_ PDRIVER_OBJECT DriverObject
    )
{
    PAGED_CODE();

    UNREFERENCED_PARAMETER(DriverObject);

    AhCtlDrainPend(STATUS_DELETE_PENDING);

    if (g_AhCtlSymlink)
    {
        UNICODE_STRING symlinkName;
        RtlInitUnicodeString(&symlinkName, AH_CTL_SYMLINK_W);
        IoDeleteSymbolicLink(&symlinkName);
        g_AhCtlSymlink = FALSE;
    }

    if (g_AhCtlDevice != NULL)
    {
        IoDeleteDevice(g_AhCtlDevice);
        g_AhCtlDevice = NULL;
    }

    if (g_ExpectedImage.Buffer != NULL)
    {
        ExFreePoolWithTag(g_ExpectedImage.Buffer, AH_CTL_POOLTAG);
        RtlZeroMemory(&g_ExpectedImage, sizeof(g_ExpectedImage));
    }
}
