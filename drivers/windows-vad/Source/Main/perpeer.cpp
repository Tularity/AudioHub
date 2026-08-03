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
#include "minipairs.h"
#include "perpeer.h"

//-----------------------------------------------------------------------------
// State
//-----------------------------------------------------------------------------

static AH_SLOT          g_AhSlots[AUDIOHUB_WIN_MAX_SLOTS];
static KMUTEX           g_AhSlotLock;
static PADAPTERCOMMON   g_AhAdapter     = NULL;
static PDEVICE_OBJECT   g_AhDeviceObject = NULL;
static BOOLEAN          g_AhInitialised = FALSE;

//
// The PDO, kept so the per-peer pin-name keys can be written into the device's
// own software key -- the same key the INF's AddReg section wrote the static
// entries into, which is the location proven to work on the target machine.
//
static PDEVICE_OBJECT   g_AhPdo = NULL;

//
// The direction words, READ BACK from the INF's static MediaCategories entries
// at attach time rather than compiled in.
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

#define AH_LOCK()   KeWaitForSingleObject(&g_AhSlotLock, Executive, KernelMode, FALSE, NULL)
#define AH_UNLOCK() KeReleaseMutex(&g_AhSlotLock, FALSE)

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
// Per-peer pin names
//
// See the long comment on AH_DIRWORD_CHARS in perpeer.h for WHY the pin name is
// the half of the endpoint name that can carry the peer's identity, and why the
// bracketed half provably cannot.
//-----------------------------------------------------------------------------

//
// "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}" == 38 characters + NUL.
//
#define AH_GUIDSTR_CHARS    39

#define AH_PIN_DIR_OUT      0u
#define AH_PIN_DIR_IN       1u

//
// The two registry locations KS consults for a pin-name GUID, in the order KS
// consults them:
//
//   0  the device's own software key -- "Starting with Windows 10 October 2018
//      Update, version 1809, when searching the registry, KS first looks for an
//      entry in the device's software key". This is where the INF's
//      HKR,MediaCategories,... entries landed, and where the static direction
//      names are ALREADY MEASURED to work on the target machine.
//   1  the machine-wide fallback KS drops to when the software key has no entry.
//
// Both are written, and it is worth being explicit that this is a hedge rather
// than a belief: the software-key entry is proven for keys the INF created at
// INSTALL time, and nothing observed so far proves KS re-reads that key for one
// created at BIND time. Writing the machine-wide key as well costs one more
// handle on a path that runs a few times per pairing, and both are removed
// together, so the hedge leaves nothing behind. Which one actually served the
// name is answerable at verification time by deleting one and re-reading the
// endpoint name.
//
#define AH_MEDIACAT_SOFTWAREKEY 0u
#define AH_MEDIACAT_GLOBAL      1u
#define AH_MEDIACAT_COUNT       2u

#define AH_MEDIACAT_SUBKEY_W    L"MediaCategories"
#define AH_MEDIACAT_GLOBAL_W \
    L"\\Registry\\Machine\\SYSTEM\\CurrentControlSet\\Control\\MediaCategories"

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
static VOID
AhDerivePinNameGuid(
    _In_z_ PCSTR   PeerKey,
    _In_   ULONG   Direction,
    _Out_  GUID   *Out
    )
/*++

Routine Description:

    The pin-name GUID for one peer and one direction, DERIVED from the
    fingerprint. Never allocated, never random, never stored.

    Layout:

        {9F3C7A21-6B48-4D0d-hhhh-hhhhhhhhhhhh}
         \______________/  |  \______________/
          fixed namespace  |   the 16 hex digits of the peer fingerprint,
                           |   verbatim
                           direction: 0 render, 1 capture

    Determinism is the point, and it is the same argument the reference string
    makes: unpair and re-pair the same machine and it must land on the SAME
    registry key. A random GUID would leave one dead MediaCategories entry per
    pairing cycle, and each one is a string with somebody's host name in it.

    Putting the fingerprint in verbatim also makes the key self-identifying:
    reading MediaCategories in regedit tells you which peer each name belongs to
    without any other lookup.

    Caller has already validated PeerKey as exactly AH_PEERKEY_CHARS of
    lowercase hex, so the parse below cannot fail.

--*/
{
    PAGED_CODE();

    Out->Data1 = 0x9F3C7A21;
    Out->Data2 = 0x6B48;
    Out->Data3 = (USHORT)(0x4D00 | (Direction & 0xF));

    for (ULONG i = 0; i < 8; i++)
    {
        CHAR hi = PeerKey[i * 2];
        CHAR lo = PeerKey[i * 2 + 1];
        UCHAR h = (UCHAR)((hi >= 'a') ? (hi - 'a' + 10) : (hi - '0'));
        UCHAR l = (UCHAR)((lo >= 'a') ? (lo - 'a' + 10) : (lo - '0'));
        Out->Data4[i] = (UCHAR)((h << 4) | l);
    }
}

#pragma code_seg("PAGE")
static NTSTATUS
AhOpenMediaCategories(
    _In_  ULONG       Location,
    _In_  ACCESS_MASK Access,
    _In_  BOOLEAN     Create,
    _Out_ PHANDLE     Key
    )
/*++

Routine Description:

    Opens (or creates) the MediaCategories root at one of the two locations.

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
        if (g_AhPdo == NULL)
        {
            return STATUS_DEVICE_NOT_READY;
        }
        //
        // PLUGPLAY_REGKEY_DRIVER, not _DEVICE: HKR inside a DDInstall AddReg
        // section is the DRIVER software key, and that is where the INF put the
        // static MediaCategories entries this has to sit beside.
        //
        status = IoOpenDeviceRegistryKey(
            g_AhPdo, PLUGPLAY_REGKEY_DRIVER,
            Access | KEY_CREATE_SUB_KEY, &root);
        //
        // KEY_CREATE_SUB_KEY is added because the caller's Access says what it
        // wants on MediaCategories itself, not on the driver key it hangs off.
        //
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

    if (Create)
    {
        status = ZwCreateKey(Key, Access, &oa, 0, NULL, REG_OPTION_NON_VOLATILE, NULL);
    }
    else
    {
        status = ZwOpenKey(Key, Access, &oa);
    }

    if (root != NULL)
    {
        ZwClose(root);
    }
    return status;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhReadPinNameValue(
    _In_  const GUID *Guid,
    _Out_writes_z_(Chars) PWSTR Out,
    _In_  ULONG       Chars
    )
/*++

Routine Description:

    Reads MediaCategories\<Guid>\Name, trying the software key first and the
    machine-wide key second -- the order KS itself searches in.

    Used ONLY at attach, to read back the direction words the INF installed.

--*/
{
    PAGED_CODE();

    WCHAR guidStr[AH_GUIDSTR_CHARS];
    AhFormatGuidKey(Guid, guidStr);

    Out[0] = L'\0';

    for (ULONG loc = 0; loc < AH_MEDIACAT_COUNT; loc++)
    {
        HANDLE mediaCat = NULL;
        NTSTATUS status = AhOpenMediaCategories(loc, KEY_READ, FALSE, &mediaCat);
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
AhWritePinNameAt(
    _In_ ULONG        Location,
    _In_ const GUID  *Guid,
    _In_z_ PCWSTR     Label
    )
{
    PAGED_CODE();

    WCHAR guidStr[AH_GUIDSTR_CHARS];
    AhFormatGuidKey(Guid, guidStr);

    //
    // KEY_READ as well as KEY_WRITE: a child is opened THROUGH this handle, and
    // KEY_WRITE alone carries no KEY_ENUMERATE_SUB_KEYS.
    //
    HANDLE   mediaCat = NULL;
    NTSTATUS status   = AhOpenMediaCategories(Location, KEY_READ | KEY_WRITE, TRUE, &mediaCat);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    UNICODE_STRING    sub;
    OBJECT_ATTRIBUTES oa;
    HANDLE            key = NULL;

    RtlInitUnicodeString(&sub, guidStr);
    InitializeObjectAttributes(&oa, &sub,
                               OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                               mediaCat, NULL);
    status = ZwCreateKey(&key, KEY_SET_VALUE, &oa, 0, NULL, REG_OPTION_NON_VOLATILE, NULL);
    ZwClose(mediaCat);
    if (!NT_SUCCESS(status))
    {
        return status;
    }

    SIZE_T chars = 0;
    while (chars < AH_PINLABEL_CHARS && Label[chars] != L'\0')
    {
        chars++;
    }

    UNICODE_STRING valueName;
    RtlInitUnicodeString(&valueName, L"Name");
    status = ZwSetValueKey(key, &valueName, 0, REG_SZ,
                           (PVOID)Label, (ULONG)((chars + 1) * sizeof(WCHAR)));

    if (NT_SUCCESS(status))
    {
        //
        // Mirrors the INF's `HKR,MediaCategories\<guid>,Display,1,00,00,00,00`
        // for the static entries. Written the same way for the same reason it
        // is written there at all: this key is meant to be indistinguishable
        // from one the INF created.
        //
        UCHAR display[4] = { 0, 0, 0, 0 };
        UNICODE_STRING displayName;
        RtlInitUnicodeString(&displayName, L"Display");
        (VOID)ZwSetValueKey(key, &displayName, 0, REG_BINARY, display, sizeof(display));
    }

    ZwClose(key);
    return status;
}

#pragma code_seg("PAGE")
static VOID
AhDeletePinNameAt(
    _In_ ULONG       Location,
    _In_ const GUID *Guid
    )
{
    PAGED_CODE();

    WCHAR guidStr[AH_GUIDSTR_CHARS];
    AhFormatGuidKey(Guid, guidStr);

    HANDLE   mediaCat = NULL;
    NTSTATUS status   = AhOpenMediaCategories(Location, KEY_READ, FALSE, &mediaCat);
    if (!NT_SUCCESS(status))
    {
        return;
    }

    UNICODE_STRING    sub;
    OBJECT_ATTRIBUTES oa;
    HANDLE            key = NULL;

    RtlInitUnicodeString(&sub, guidStr);
    InitializeObjectAttributes(&oa, &sub,
                               OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE,
                               mediaCat, NULL);
    status = ZwOpenKey(&key, DELETE, &oa);
    ZwClose(mediaCat);
    if (!NT_SUCCESS(status))
    {
        return;
    }

    status = ZwDeleteKey(key);
    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhDeletePinNameAt] loc %u %S delete failed 0x%x", Location, guidStr, status));
    }
    ZwClose(key);
}

#pragma code_seg("PAGE")
static NTSTATUS
AhComposePinLabel(
    _In_z_ PCWSTR Display,
    _In_z_ PCWSTR DirectionWord,
    _Out_writes_z_(Chars) PWSTR Out,
    _In_   ULONG  Chars
    )
/*++

Routine Description:

    "AudioHub - WIN-30" + " " + "<speaker>".

    TRUNCATION RULE: the PEER's half is what gets cut, never the direction word.
    A pair of devices that both read "AudioHub - some-very-long-hostna" is
    merely ugly; a pair that both read "AudioHub - host" with no direction left
    is unusable, because the only thing distinguishing a speaker from a
    microphone in the list would be gone.

--*/
{
    PAGED_CODE();

    SIZE_T dirLen = 0;
    while (DirectionWord[dirLen] != L'\0') { dirLen++; }

    if (dirLen == 0 || Chars < dirLen + 3)
    {
        return STATUS_INVALID_PARAMETER;
    }

    //
    // Room for " " + direction word + NUL.
    //
    SIZE_T budget = Chars - dirLen - 2;
    SIZE_T n      = 0;
    while (n < budget - 1 && Display[n] != L'\0')
    {
        Out[n] = Display[n];
        n++;
    }
    //
    // Never end a truncated name on a high surrogate: half a pair is not valid
    // UTF-16 and this string is about to be written into the registry. Chinese
    // is in the BMP, but a peer's computer name may hold an emoji -- macOS
    // allows it -- and the daemon's own clamp guards the same boundary.
    //
    if (n > 0 && Out[n - 1] >= 0xD800 && Out[n - 1] <= 0xDBFF)
    {
        n--;
    }
    Out[n++] = L' ';
    for (SIZE_T i = 0; i < dirLen; i++)
    {
        Out[n++] = DirectionWord[i];
    }
    Out[n] = L'\0';
    return STATUS_SUCCESS;
}

#pragma code_seg("PAGE")
static VOID
AhRemovePinNames(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    Removes both of a slot's MediaCategories entries, at every location they
    could have been written to.

    Called from the ONE teardown routine rather than from each of its three call
    sites, so "unpairing leaves no registry litter carrying somebody's host
    name" cannot be true at two of them and false at the third.

--*/
{
    PAGED_CODE();

    if (!Slot->PinNamesWritten)
    {
        return;
    }

    for (ULONG loc = 0; loc < AH_MEDIACAT_COUNT; loc++)
    {
        AhDeletePinNameAt(loc, &Slot->PinGuidOut);
        AhDeletePinNameAt(loc, &Slot->PinGuidIn);
    }

    Slot->PinNamesWritten = FALSE;
    //
    // PinNameFallback is deliberately NOT cleared here. AhApplyPinNames calls
    // this routine ON its fallback path -- to take back a half-written pair --
    // and clearing the flag there would erase the decision that had just been
    // made, leaving the pins pointed at GUIDs with no registry entry AND the
    // reply claiming everything was fine. The flag's lifetime belongs to
    // AhApplyPinNames, which resets it at the top of every attempt.
}

#pragma code_seg("PAGE")
static VOID
AhApplyPinNames(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    Flags
    )
/*++

Routine Description:

    Derives this slot's two pin-name GUIDs, composes the labels and writes them.

    Sets Slot->PinNameFallback when the peer's name could NOT be made to appear,
    in which case AhBuildMinipairs leaves the INF's static GUIDs in the pin
    descriptors and the endpoints come out named with the plain direction words.
    That is a real degradation -- with two peers paired the user sees two
    identically named speakers -- so it travels back to the daemon as
    AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK rather than being absorbed here.

    Failing the whole bind instead was considered and rejected: a device with a
    generic name is enormously more useful than no device, and the peer is
    already paired by the time this runs.

--*/
{
    PAGED_CODE();

    Slot->PinNameFallback = FALSE;

    if (!g_AhDirWordsOk)
    {
        //
        // The INF's own entries could not be read, so there is no direction
        // word to append. Composing without one would publish a speaker and a
        // microphone under the SAME string -- strictly worse than generic
        // names, which at least still say which is which.
        //
        DPF(D_ERROR, ("[AhApplyPinNames] no direction words; per-peer naming disabled"));
        Slot->PinNameFallback = TRUE;
        return;
    }

    AhDerivePinNameGuid(Slot->PeerKey, AH_PIN_DIR_OUT, &Slot->PinGuidOut);
    AhDerivePinNameGuid(Slot->PeerKey, AH_PIN_DIR_IN,  &Slot->PinGuidIn);

    NTSTATUS st1 = AhComposePinLabel(Slot->Display, g_AhDirWordOut,
                                     Slot->PinLabelOut, AH_PINLABEL_CHARS);
    NTSTATUS st2 = AhComposePinLabel(Slot->Display, g_AhDirWordIn,
                                     Slot->PinLabelIn, AH_PINLABEL_CHARS);
    if (!NT_SUCCESS(st1) || !NT_SUCCESS(st2))
    {
        DPF(D_ERROR, ("[AhApplyPinNames] compose failed 0x%x / 0x%x", st1, st2));
        Slot->PinNameFallback = TRUE;
        return;
    }

    //
    // Written at BOTH locations, and a location counts as written only if BOTH
    // directions land there. One direction named and the other not is the same
    // half-published shape the whole of protocol v2 exists to make impossible.
    //
    ULONG written = 0;
    for (ULONG loc = 0; loc < AH_MEDIACAT_COUNT && !(Flags & AH_BINDFLAG_FAIL_PIN_NAME); loc++)
    {
        NTSTATUS a = AhWritePinNameAt(loc, &Slot->PinGuidOut, Slot->PinLabelOut);
        NTSTATUS b = AhWritePinNameAt(loc, &Slot->PinGuidIn,  Slot->PinLabelIn);
        if (NT_SUCCESS(a) && NT_SUCCESS(b))
        {
            written++;
        }
        else
        {
            DPF(D_ERROR, ("[AhApplyPinNames] loc %u write failed 0x%x / 0x%x", loc, a, b));
        }
    }

    //
    // Anything written has to be removable, whether or not the naming worked.
    //
    Slot->PinNamesWritten = TRUE;

    if (written == 0)
    {
        //
        // Take back anything a partly-successful write left behind FIRST, then
        // record the decision: AhRemovePinNames must not be in a position to
        // observe -- or undo -- a flag it does not own.
        //
        AhRemovePinNames(Slot);
        Slot->PinNameFallback = TRUE;
        DPF(D_ERROR, ("[AhApplyPinNames] slot pin names unwritable; falling back to generic names"));
        return;
    }

    DPF(D_TERSE, ("[AhApplyPinNames] %S / %S (%u location(s))",
                  Slot->PinLabelOut, Slot->PinLabelIn, written));
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
static NTSTATUS
AhBuildRefStrings(
    _Inout_ PAH_SLOT Slot,
    _In_z_  PCSTR    PeerKey
    )
/*++

Routine Description:

    "AhTopoOut-a1b2c3d4e5f60718" and its three siblings.

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
    struct { PWSTR Buffer; PCWSTR Template; } map[] = {
        { Slot->TopoNameOut, AH_TEMPLATE_TOPO_OUT },
        { Slot->WaveNameOut, AH_TEMPLATE_WAVE_OUT },
        { Slot->TopoNameIn,  AH_TEMPLATE_TOPO_IN  },
        { Slot->WaveNameIn,  AH_TEMPLATE_WAVE_IN  },
    };

    for (ULONG i = 0; i < ARRAYSIZE(map); i++)
    {
        NTSTATUS st = RtlStringCbPrintfW(
            map[i].Buffer,
            AH_REFSTRING_MAX * sizeof(WCHAR),
            L"%s-%S",
            map[i].Template,
            PeerKey);
        if (!NT_SUCCESS(st))
        {
            return st;
        }
    }

    return STATUS_SUCCESS;
}

//
// Both topology pin tables must fit the per-slot copy, and PortCls must be able
// to walk the copy with the template's own stride.
//
C_ASSERT(SIZEOF_ARRAY(SpeakerTopoMiniportPins) <= AH_MAX_TOPO_PINS);
C_ASSERT(SIZEOF_ARRAY(MicArray1TopoMiniportPins) <= AH_MAX_TOPO_PINS);

#pragma code_seg("PAGE")
static NTSTATUS
AhCloneTopoFilter(
    _In_  const PCFILTER_DESCRIPTOR *Template,
    _In_  const GUID  *Marker,
    _In_opt_ const GUID *Replacement,
    _Out_writes_(AH_MAX_TOPO_PINS) PCPIN_DESCRIPTOR *Pins,
    _Out_ PCFILTER_DESCRIPTOR *Out
    )
/*++

Routine Description:

    Copies a topology filter descriptor and its pin array into the slot, and
    repoints the bridge pin's KsPinDescriptor.Name at this peer's GUID.

    The pin is found by MATCHING the static Name GUID the INF registered, not by
    index. An index would be a second, invisible copy of a fact that already
    lives in the topology table, and reordering that table would silently rename
    the wrong pin -- one of the few mistakes here that produces no error at all,
    just a device whose name belongs to something else.

    Replacement == NULL leaves the marker in place, which is exactly the
    fallback behaviour: the endpoint keeps the INF's generic direction name.

--*/
{
    PAGED_CODE();

    if (Template->PinCount > AH_MAX_TOPO_PINS ||
        Template->PinSize != sizeof(PCPIN_DESCRIPTOR))
    {
        return STATUS_INVALID_PARAMETER;
    }

    RtlCopyMemory(Pins, Template->Pins, Template->PinCount * sizeof(PCPIN_DESCRIPTOR));
    *Out = *Template;
    Out->Pins = Pins;

    if (Replacement == NULL)
    {
        return STATUS_SUCCESS;
    }

    for (ULONG i = 0; i < Template->PinCount; i++)
    {
        const GUID *name = Pins[i].KsPinDescriptor.Name;
        if (name != NULL && RtlEqualMemory(name, Marker, sizeof(GUID)))
        {
            Pins[i].KsPinDescriptor.Name = Replacement;
            return STATUS_SUCCESS;
        }
    }

    //
    // The table lost its marker. Reported rather than ignored: silently keeping
    // the shared descriptor would publish every peer under one name and nothing
    // would say why.
    //
    return STATUS_NOT_FOUND;
}

#pragma code_seg("PAGE")
static NTSTATUS
AhBuildMinipairs(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    Fills the slot's two ENDPOINT_MINIPAIRs from the static templates.

    The TOPOLOGY filter descriptors and their pin arrays are copied per slot,
    because the bridge pin's Name GUID is this peer's (AhApplyPinNames). That is
    the same reason sysvad's Bluetooth path deep-copies -- it rewrites each
    endpoint's pin Category at runtime.

    Everything else stays SHARED: node tables, connection tables, data ranges,
    automation tables, the format-and-modes tables and both WAVE filters. None
    of them varies per peer, and each one copied would be another lifetime to
    get wrong for no gain.

    Both the FriendlyName property and the pin Name GUID point INTO the slot
    record, which outlives every endpoint they can be attached to.

--*/
{
    PAGED_CODE();

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
    // Per-slot topology descriptors. NULL replacement == keep the INF's static
    // GUID == generic direction names, which is what the fallback path wants.
    //
    NTSTATUS nameStatus = AhCloneTopoFilter(
        &SpeakerTopoMiniportFilterDescriptor,
        &AH_PIN_NAME_OUT,
        Slot->PinNameFallback ? NULL : &Slot->PinGuidOut,
        Slot->OutTopoPins,
        &Slot->OutTopoFilter);
    if (NT_SUCCESS(nameStatus))
    {
        nameStatus = AhCloneTopoFilter(
            &MicArray1TopoMiniportFilterDescriptor,
            &AH_PIN_NAME_IN,
            Slot->PinNameFallback ? NULL : &Slot->PinGuidIn,
            Slot->InTopoPins,
            &Slot->InTopoFilter);
    }
    if (!NT_SUCCESS(nameStatus))
    {
        return nameStatus;
    }

    //
    // Render pair.
    //
    RtlZeroMemory(&Slot->OutPair, sizeof(Slot->OutPair));
    Slot->OutPair.DeviceType                    = eSpeakerDevice;
    Slot->OutPair.TopoName                      = Slot->TopoNameOut;
    Slot->OutPair.TemplateTopoName              = (PWSTR)AH_TEMPLATE_TOPO_OUT;
    Slot->OutPair.TopoCreateCallback            = CreateMiniportTopologySimpleAudioSample;
    Slot->OutPair.TopoDescriptor                = &Slot->OutTopoFilter;
    Slot->OutPair.TopoInterfacePropertyCount    = ARRAYSIZE(Slot->OutTopoProps);
    Slot->OutPair.TopoInterfaceProperties       = Slot->OutTopoProps;
    Slot->OutPair.WaveName                      = Slot->WaveNameOut;
    Slot->OutPair.TemplateWaveName              = (PWSTR)AH_TEMPLATE_WAVE_OUT;
    Slot->OutPair.WaveCreateCallback            = CreateMiniportWaveRTSimpleAudioSample;
    Slot->OutPair.WaveDescriptor                = &SpeakerWaveMiniportFilterDescriptor;
    Slot->OutPair.WaveInterfacePropertyCount    = 0;
    Slot->OutPair.WaveInterfaceProperties       = NULL;
    Slot->OutPair.DeviceMaxChannels             = SPEAKER_DEVICE_MAX_CHANNELS;
    Slot->OutPair.PinDeviceFormatsAndModes      = SpeakerPinDeviceFormatsAndModes;
    Slot->OutPair.PinDeviceFormatsAndModesCount = SIZEOF_ARRAY(SpeakerPinDeviceFormatsAndModes);
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
    Slot->InPair.TopoDescriptor                 = &Slot->InTopoFilter;
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
static ULONG
AhSlotPublishedMask(
    _In_ const AH_SLOT *Slot
    )
/*++

Routine Description:

    Which halves of this slot's device pair the driver ACTUALLY holds.

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

#pragma code_seg("PAGE")
static NTSTATUS
AhRemoveSlotEndpoints(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    DebugFlags,
    _Out_opt_ PULONG FailStage
    )
/*++

Routine Description:

    Tears one slot's two endpoints down. Caller holds the lock and has already
    checked that an adapter is attached.

    The ORDER inside RemoveEndpointFilters (disconnect topologies, then
    unregister wave, then unregister topology) is fixed by
    CAdapterCommon::RemoveEndpointFilters and must not be second-guessed here:
    "Failure to unregister the subdevice's physical connections can cause
    memory leaks".

    Returns the FIRST failure. The port references are released either way --
    a failed unregister is a leak inside PortCls, and holding our reference on
    top of it would add a second, larger one.

    The slot's MediaCategories entries go here too, and not at the three call
    sites: "unpairing leaves no registry litter carrying somebody's host name"
    must not be true at two of them and false at the third. They are removed
    AFTER the endpoints, because until the endpoints are gone the names are
    still what the system is displaying.

--*/
{
    PAGED_CODE();

    if (FailStage != NULL) { *FailStage = AH_STAGE_NONE; }

    if (g_AhAdapter == NULL)
    {
        //
        // The adapter went away first. The port objects it owned died with it;
        // releasing them here would be a use-after-free. Drop the references
        // without touching them -- AhPerPeerDetachAdapter is the only path
        // that reaches this, and it runs before the adapter is Released.
        //
        Slot->OutTopo = Slot->OutWave = Slot->InTopo = Slot->InWave = NULL;
        //
        // The registry entries outlive the adapter and are still ours to
        // remove: they live under the PDO's software key and the machine-wide
        // key, neither of which the adapter owns.
        //
        AhRemovePinNames(Slot);
        return STATUS_SUCCESS;
    }

    NTSTATUS firstError = STATUS_SUCCESS;
    ULONG    stage      = AH_STAGE_NONE;
    ULONG    st         = AH_STAGE_NONE;

    if (Slot->OutTopo != NULL || Slot->OutWave != NULL)
    {
        NTSTATUS s1 = g_AhAdapter->RemoveEndpointFilters(
            &Slot->OutPair, Slot->OutTopo, Slot->OutWave, DebugFlags, &st);
        if (!NT_SUCCESS(s1) && NT_SUCCESS(firstError)) { firstError = s1; stage = st; }
    }
    if (Slot->InTopo != NULL || Slot->InWave != NULL)
    {
        NTSTATUS s2 = g_AhAdapter->RemoveEndpointFilters(
            &Slot->InPair, Slot->InTopo, Slot->InWave, DebugFlags, &st);
        if (!NT_SUCCESS(s2) && NT_SUCCESS(firstError)) { firstError = s2; stage = st; }
    }

    SAFE_RELEASE(Slot->OutTopo);
    SAFE_RELEASE(Slot->OutWave);
    SAFE_RELEASE(Slot->InTopo);
    SAFE_RELEASE(Slot->InWave);

    AhRemovePinNames(Slot);

    if (FailStage != NULL) { *FailStage = stage; }
    return firstError;
}

//-----------------------------------------------------------------------------
// Lifecycle
//-----------------------------------------------------------------------------

#pragma code_seg("PAGE")
VOID
AhPerPeerDriverInit(VOID)
{
    PAGED_CODE();

    RtlZeroMemory(g_AhSlots, sizeof(g_AhSlots));
    KeInitializeMutex(&g_AhSlotLock, 0);
    g_AhAdapter = NULL;
    g_AhDeviceObject = NULL;
    g_AhNextGeneration = 1;
    g_AhInitialised = TRUE;
}

#pragma code_seg("PAGE")
NTSTATUS
AhPerPeerAttachAdapter(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PADAPTERCOMMON Adapter
    )
{
    PAGED_CODE();

    NTSTATUS status = STATUS_SUCCESS;

    ASSERT(g_AhInitialised);

    AH_LOCK();

    if (g_AhAdapter != NULL)
    {
        //
        // Root enumeration produces exactly one devnode, so this cannot
        // legitimately happen. Refusing loudly beats quietly serving whichever
        // adapter happened to arrive last.
        //
        DPF(D_ERROR, ("[AhPerPeerAttachAdapter] a second adapter tried to attach; refused"));
        status = STATUS_DEVICE_ALREADY_ATTACHED;
    }
    else
    {
        g_AhDeviceObject = DeviceObject;
        g_AhAdapter = Adapter;
        g_AhPdo = Adapter->GetPhysicalDeviceObject();
        //
        // No AddRef: the reference the device extension holds is what keeps the
        // adapter alive, and PnpHandler tears us down before releasing it.
        //

        //
        // Read the direction words back out of the INF's own static
        // MediaCategories entries. Reading rather than hardcoding keeps the
        // localizable strings in [Strings], the only place that can ever grow a
        // [Strings.0409].
        //
        NTSTATUS o = AhReadPinNameValue(&AH_PIN_NAME_OUT, g_AhDirWordOut, AH_DIRWORD_CHARS);
        NTSTATUS i = AhReadPinNameValue(&AH_PIN_NAME_IN,  g_AhDirWordIn,  AH_DIRWORD_CHARS);
        g_AhDirWordsOk = (BOOLEAN)(NT_SUCCESS(o) && NT_SUCCESS(i));
        if (!g_AhDirWordsOk)
        {
            //
            // NOT fatal, and NOT silent. Every bind from here on reports
            // AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK, so the daemon can say that
            // the devices are unnamed because the INF's own strings are
            // missing -- which is an install problem, not a pairing problem,
            // and the two are otherwise indistinguishable from the outside.
            //
            DPF(D_ERROR, ("[AhPerPeerAttachAdapter] INF direction names unreadable "
                          "(0x%x / 0x%x); per-peer device names disabled", o, i));
        }
        else
        {
            DPF(D_TERSE, ("[AhPerPeerAttachAdapter] direction names '%S' / '%S'",
                          g_AhDirWordOut, g_AhDirWordIn));
        }

        DPF(D_TERSE, ("[AhPerPeerAttachAdapter] ready, %u slots", AUDIOHUB_WIN_MAX_SLOTS));
    }

    AH_UNLOCK();
    return status;
}

#pragma code_seg("PAGE")
VOID
AhPerPeerDetachAdapter(VOID)
{
    PAGED_CODE();

    if (!g_AhInitialised)
    {
        return;
    }

    AH_LOCK();

    for (ULONG i = 0; i < AUDIOHUB_WIN_MAX_SLOTS; i++)
    {
        if (g_AhSlots[i].State != AH_SLOT_FREE)
        {
            (VOID)AhRemoveSlotEndpoints(&g_AhSlots[i], 0, NULL);
            g_AhSlots[i].State = AH_SLOT_FREE;
            RtlZeroMemory(g_AhSlots[i].PeerKey, sizeof(g_AhSlots[i].PeerKey));
        }
    }

    g_AhAdapter = NULL;
    g_AhDeviceObject = NULL;
    //
    // Cleared LAST: AhRemoveSlotEndpoints above needs it to reach the software
    // key, and a stale PDO here would be handed to IoOpenDeviceRegistryKey
    // after the devnode has gone.
    //
    g_AhPdo = NULL;
    g_AhDirWordsOk = FALSE;

    AH_UNLOCK();
}

#pragma code_seg("PAGE")
BOOLEAN
AhPerPeerAdapterReady(VOID)
{
    PAGED_CODE();

    if (!g_AhInitialised)
    {
        return FALSE;
    }

    AH_LOCK();
    BOOLEAN ready = (g_AhAdapter != NULL);
    AH_UNLOCK();
    return ready;
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
    _In_  ULONG   Flags,
    _Out_ PULONG  Generation,
    _Out_ PULONG  State,
    _Out_ PULONG  AhStatus,
    _Out_ PAH_OP_RESULT Result
    )
/*++

Routine Description:

    Publishes one peer's pair of endpoints, ALL OR NOTHING.

    The invariant this routine exists to guarantee:

        AhStatus == AH_STATUS_OK  =>  Result->Published == AH_PUB_BOTH

    Half a pair is worthless to a user -- a machine that can be spoken to but
    not heard from, published under a name that promises both -- and it is not
    representable in the daemon's model either. So a failure on either half
    removes the other half again and reports the failure with the stage and the
    kernel status that caused it.

    The one escape hatch is AH_BINDFLAG_SKIP_ROLLBACK, which exists so a test
    can OBSERVE the partial state (reported as AH_STATUS_PARTIAL, never as OK).

--*/
{
    PAGED_CODE();

    NTSTATUS status = STATUS_SUCCESS;
    ULONG    stage  = AH_STAGE_NONE;

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

    if (g_AhAdapter == NULL)
    {
        *AhStatus = AH_STATUS_NO_ADAPTER;
        status = STATUS_DEVICE_NOT_READY;
        goto Done;
    }

    if (s->State == AH_SLOT_BOUND)
    {
        if (strncmp(s->PeerKey, PeerKey, AH_PEERKEY_BUF) == 0)
        {
            ULONG have = AhSlotPublishedMask(s);
            if (have == AH_PUB_BOTH)
            {
                //
                // Idempotent re-Set. The daemon re-Sets whenever a peer's
                // online flag moves, and under "paired means published" that
                // flag does not change anything the driver publishes.
                // Re-registering or rewriting the PERSISTENT FriendlyName
                // property here would turn every connect/disconnect into
                // registry churn.
                //
                // Renaming a LIVE endpoint is deliberately not attempted:
                // sysvad has never changed the FriendlyName of an
                // already-enabled interface, and the only way to force a
                // refresh -- disable the interface and re-enable it -- puts the
                // endpoint into DEVICE_STATE_NOTPRESENT and makes Windows move
                // the user's default-device choice elsewhere.
                //
                s->Flags    = Flags;
                *Generation = s->Generation;
                *State      = AH_SLOT_BOUND;
                *AhStatus   = AH_STATUS_OK;
                Result->Published = have;
                //
                // Re-reported on EVERY Set, not only the first. The daemon
                // re-Sets whenever a peer goes on or offline, and a warning
                // that appears once and then disappears is a warning nobody
                // acts on.
                //
                if (s->PinNameFallback)
                {
                    Result->Flags |= AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK;
                }
                goto Done;
            }

            //
            // Bound but not whole. Repair rather than report success: an
            // idempotent operation has to converge on the intended state, and
            // "do nothing, it is already bound" would leave the user with a
            // permanently missing speaker that no retry could ever fix.
            //
            DPF(D_ERROR, ("[AhSlotBindSet] slot %u bound but published=0x%x; reinstalling", Slot, have));
            (VOID)AhRemoveSlotEndpoints(s, 0, NULL);
            s->State = AH_SLOT_FREE;
        }
        else
        {
            //
            // Same slot, different peer. The daemon is supposed to Clear first;
            // doing it here keeps the driver's table authoritative either way.
            //
            DPF(D_TERSE, ("[AhSlotBindSet] slot %u re-targeted; removing previous peer", Slot));
            (VOID)AhRemoveSlotEndpoints(s, 0, NULL);
            s->State = AH_SLOT_FREE;
        }
    }

    RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));
    RtlCopyMemory(s->PeerKey, PeerKey, keyLen);

    RtlZeroMemory(s->Display, sizeof(s->Display));
    status = RtlStringCchCopyW(s->Display, AH_DISPLAY_CHARS - 1, Display);
    if (status == STATUS_BUFFER_OVERFLOW)
    {
        //
        // Truncation is fine, an unterminated buffer is not. RtlStringCchCopyW
        // terminates on overflow, and the explicit store below is the belt to
        // that braces.
        //
        status = STATUS_SUCCESS;
    }
    s->Display[AH_DISPLAY_CHARS - 1] = L'\0';
    if (!NT_SUCCESS(status))
    {
        *AhStatus = AH_STATUS_BAD_ARGUMENT;
        goto Done;
    }

    if (s->Display[0] == L'\0')
    {
        //
        // An empty FriendlyName would make Windows compose "Speakers ()".
        // Fall back to something that at least identifies the peer.
        //
        (VOID)RtlStringCchPrintfW(s->Display, AH_DISPLAY_CHARS, L"AudioHub %S", s->PeerKey);
    }

    status = AhBuildRefStrings(s, s->PeerKey);
    if (!NT_SUCCESS(status))
    {
        Result->Stage = AH_STAGE_REFSTRINGS;
        Result->NtStatus = status;
        *AhStatus = AH_STATUS_INTERNAL;
        goto Done;
    }

    //
    // BEFORE the filters exist. KS resolves KSPROPERTY_PIN_NAME when the audio
    // endpoint builder asks -- which happens once PcRegisterSubdevice has
    // enabled the interface -- so the key has to be in place by then. Writing
    // it afterwards would be a race against the endpoint builder, and the
    // losing side of that race is a device published under the wrong name with
    // nothing to say so.
    //
    AhApplyPinNames(s, Flags);
    if (s->PinNameFallback)
    {
        Result->Flags |= AH_BINDREPLY_FLAG_PIN_NAME_FALLBACK;
    }

    status = AhBuildMinipairs(s);
    if (!NT_SUCCESS(status))
    {
        Result->Stage = AH_STAGE_PINNAME;
        Result->NtStatus = status;
        *AhStatus = AH_STATUS_INTERNAL;
        AhRemovePinNames(s);
        goto Done;
    }

    //
    // Render first, then capture.
    //
    if (Flags & AH_BINDFLAG_FAIL_RENDER)
    {
        status = STATUS_UNSUCCESSFUL;
        stage  = AH_STAGE_INSTALL_RENDER;
    }
    else
    {
        status = g_AhAdapter->InstallEndpointFilters(
            NULL,                   // no IRP: this is a dynamic install, exactly as
                                    // sysvad's Bluetooth path does it
            &s->OutPair,
            NULL,
            &s->OutTopo,
            &s->OutWave,
            NULL, NULL);
        if (NT_SUCCESS(status) && (s->OutTopo == NULL || s->OutWave == NULL))
        {
            //
            // "It returned success" and "there is a filter" are different
            // facts. Checking the second one is what makes a silently
            // half-installed endpoint impossible to report as bound.
            //
            status = STATUS_UNSUCCESSFUL;
            stage  = AH_STAGE_VERIFY;
        }
        else if (!NT_SUCCESS(status))
        {
            stage = AH_STAGE_INSTALL_RENDER;
        }
    }

    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u render install failed 0x%x (stage %u)", Slot, status, stage));
        goto Failed;
    }

    if (Flags & AH_BINDFLAG_FAIL_CAPTURE)
    {
        status = STATUS_UNSUCCESSFUL;
        stage  = AH_STAGE_INSTALL_CAPTURE;
    }
    else
    {
        status = g_AhAdapter->InstallEndpointFilters(
            NULL,
            &s->InPair,
            NULL,
            &s->InTopo,
            &s->InWave,
            NULL, NULL);
        if (NT_SUCCESS(status) && (s->InTopo == NULL || s->InWave == NULL))
        {
            status = STATUS_UNSUCCESSFUL;
            stage  = AH_STAGE_VERIFY;
        }
        else if (!NT_SUCCESS(status))
        {
            stage = AH_STAGE_INSTALL_CAPTURE;
        }
    }

    if (!NT_SUCCESS(status))
    {
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u capture install failed 0x%x (stage %u)", Slot, status, stage));
        goto Failed;
    }

    //
    // The invariant, checked rather than assumed.
    //
    Result->Published = AhSlotPublishedMask(s);
    if (Result->Published != AH_PUB_BOTH)
    {
        status = STATUS_UNSUCCESSFUL;
        stage  = AH_STAGE_VERIFY;
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u published=0x%x after two successful installs",
                      Slot, Result->Published));
        goto Failed;
    }

    s->Generation = g_AhNextGeneration++;
    if (g_AhNextGeneration == 0)
    {
        g_AhNextGeneration = 1;     // 0 means "no generation" on the wire
    }
    s->Flags = Flags;
    s->State = AH_SLOT_BOUND;

    *Generation = s->Generation;
    *State      = AH_SLOT_BOUND;
    *AhStatus   = AH_STATUS_OK;

    DPF(D_TERSE, ("[AhSlotBindSet] slot %u -> '%S' gen %u published 0x%x flags 0x%x",
                  Slot, s->PinNameFallback ? s->Display : s->PinLabelOut,
                  s->Generation, Result->Published, Result->Flags));
    goto Done;

Failed:
    Result->Stage    = stage;
    Result->NtStatus = status;

    if (Flags & AH_BINDFLAG_SKIP_ROLLBACK)
    {
        //
        // Test-only: leave the wreckage visible. Still never AH_STATUS_OK.
        //
        // The slot stays BOUND even though the bind failed, because the driver
        // is still HOLDING port objects: marking it FREE here would make the
        // next AhSlotBindClear take its "already gone" early return and leave
        // the surviving half published with nothing in the table pointing at
        // it. A state that no Clear can clean up is a worse test artefact than
        // the one being tested.
        //
        Result->Published = AhSlotPublishedMask(s);
        s->Generation = g_AhNextGeneration++;
        if (g_AhNextGeneration == 0) { g_AhNextGeneration = 1; }
        s->State    = AH_SLOT_BOUND;
        *Generation = s->Generation;
        *State      = AH_SLOT_BOUND;
        *AhStatus   = AH_STATUS_PARTIAL;
        DPF(D_ERROR, ("[AhSlotBindSet] slot %u rollback SKIPPED by request; published=0x%x",
                      Slot, Result->Published));
        goto Done;
    }

    {
        ULONG rbStage = AH_STAGE_NONE;
        NTSTATUS rb = AhRemoveSlotEndpoints(s, 0, &rbStage);
        Result->Published = AhSlotPublishedMask(s);
        s->State = AH_SLOT_FREE;
        RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));

        if (!NT_SUCCESS(rb) || Result->Published != 0)
        {
            //
            // The rollback itself failed. This is strictly worse than the
            // original failure and must not be reported with the original
            // failure's vocabulary: the daemon has to know that something is
            // still published under this peer's identity.
            //
            Result->Stage    = AH_STAGE_ROLLBACK;
            Result->NtStatus = NT_SUCCESS(rb) ? STATUS_UNSUCCESSFUL : rb;
            *AhStatus        = AH_STATUS_PARTIAL;
            DPF(D_ERROR, ("[AhSlotBindSet] slot %u ROLLBACK FAILED 0x%x stage %u published 0x%x",
                          Slot, rb, rbStage, Result->Published));
        }
        else
        {
            *AhStatus = AH_STATUS_INTERNAL;
        }
        *State = AH_SLOT_FREE;
    }

Done:
    AH_UNLOCK();
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

    if (g_AhAdapter == NULL)
    {
        *AhStatus = AH_STATUS_NO_ADAPTER;
        goto Done;
    }

    {
        ULONG stage = AH_STAGE_NONE;
        NTSTATUS st = AhRemoveSlotEndpoints(s, Flags, &stage);

        s->State = AH_SLOT_FREE;
        s->Flags = 0;
        RtlZeroMemory(s->PeerKey, sizeof(s->PeerKey));

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
        Reply->slots[i].state      = g_AhSlots[i].State;
        Reply->slots[i].generation = g_AhSlots[i].Generation;
        Reply->slots[i].published  = AhSlotPublishedMask(&g_AhSlots[i]);
        RtlCopyMemory(Reply->slots[i].peer_key, g_AhSlots[i].PeerKey, AH_PEERKEY_BUF);
    }
    AH_UNLOCK();
}
