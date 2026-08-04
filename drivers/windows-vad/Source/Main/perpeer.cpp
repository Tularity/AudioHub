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
// The PDO. Needed for two registry paths: reading the INF's static
// MediaCategories entries out of the driver software key, and reaching each
// per-peer device interface's own key to write the endpoint name into it.
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
// Per-peer endpoint names
//
// See the long comment above AH_DIRWORD_CHARS in perpeer.h for WHY the name is
// delivered as PKEY_Device_DeviceDesc under the interface's EP\0 key, and why
// neither the pin name nor the interface FriendlyName can do it for a speaker.
//
// MediaCategories is still READ here -- it is where the INF keeps the
// localizable direction words -- but it is no longer WRITTEN. Microsoft
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
// Read in that order so the direction words come from wherever KS would have
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
    _In_  ULONG       Location,
    _In_  ACCESS_MASK Access,
    _Out_ PHANDLE     Key
    )
/*++

Routine Description:

    Opens the MediaCategories root at one of the two locations, READ ONLY.

    Nothing writes MediaCategories any more. The only reason to come here is to
    read back the direction words the INF installed.

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
        status = IoOpenDeviceRegistryKey(g_AhPdo, PLUGPLAY_REGKEY_DRIVER, Access, &root);
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
        NTSTATUS status = AhOpenMediaCategories(loc, KEY_READ, &mediaCat);
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

    if (g_AhPdo == NULL)
    {
        return STATUS_DEVICE_NOT_READY;
    }

    UNICODE_STRING refString;
    UNICODE_STRING symlink;

    RtlInitUnicodeString(&refString, ReferenceString);
    RtlZeroMemory(&symlink, sizeof(symlink));

    NTSTATUS status = IoRegisterDeviceInterface(
        g_AhPdo, &KSCATEGORY_AUDIO, &refString, &symlink);
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
    _In_z_ PCWSTR ReferenceString,
    _In_z_ PCWSTR Name
    )
{
    PAGED_CODE();

    HANDLE   ep     = NULL;
    NTSTATUS status = AhOpenEndpointParams(ReferenceString, TRUE, &ep);
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
static VOID
AhClearEndpointName(
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
    NTSTATUS status = AhOpenEndpointParams(ReferenceString, FALSE, &ep);
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
static NTSTATUS
AhComposeEndpointName(
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

//
// THE ONE PLACE A DIRECTION IS NAMED.
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
    PWSTR   Name;               // where the composed name is kept
} AH_NAME_TARGET;

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
    Targets[0].Name            = Slot->NameOut;

    Targets[1].ReferenceString = Slot->TopoNameIn;
    Targets[1].DirectionWord   = g_AhDirWordIn;
    Targets[1].Name            = Slot->NameIn;
}

#pragma code_seg("PAGE")
static VOID
AhRemoveEndpointNames(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    Removes every endpoint-name value this slot wrote.

    Called from the ONE teardown routine rather than from each of its three
    call sites, so "unpairing leaves no registry litter carrying somebody's
    host name" cannot be true at two of them and false at the third.

--*/
{
    PAGED_CODE();

    if (!Slot->NamesWritten)
    {
        return;
    }

    AH_NAME_TARGET targets[AH_NAME_DIRECTIONS];
    AhNameTargets(Slot, targets);

    for (ULONG i = 0; i < AH_NAME_DIRECTIONS; i++)
    {
        AhClearEndpointName(targets[i].ReferenceString);
    }

    Slot->NamesWritten = FALSE;
    //
    // NameFallback is deliberately NOT cleared here. AhApplyEndpointNames calls
    // this routine ON its fallback path -- to take back a half-written pair --
    // and clearing the flag there would erase the decision that had just been
    // made, leaving the reply claiming everything was fine. The flag's lifetime
    // belongs to AhApplyEndpointNames, which resets it at the top of every
    // attempt.
}

#pragma code_seg("PAGE")
static VOID
AhApplyEndpointNames(
    _Inout_ PAH_SLOT Slot,
    _In_    ULONG    Flags
    )
/*++

Routine Description:

    Composes this slot's endpoint names and writes each one into its own
    TOPOLOGY interface's EP\0 key.

    Sets Slot->NameFallback when the peer's name could NOT be made to appear, in
    which case the endpoints come up under the system's generic direction names.
    That is a real degradation -- with two peers paired the user sees two
    identically named speakers -- so it travels back to the daemon as
    AH_BINDREPLY_FLAG_NAME_FALLBACK rather than being absorbed here.

    Failing the whole bind instead was considered and rejected: a device with a
    generic name is far more useful than no device, and the peer is already
    paired by the time this runs.

--*/
{
    PAGED_CODE();

    Slot->NameFallback = FALSE;

    if (!g_AhDirWordsOk)
    {
        //
        // The INF's own entries could not be read, so there is no direction
        // word to append. Composing without one would publish a speaker and a
        // microphone under the SAME string -- strictly worse than generic
        // names, which at least still say which is which.
        //
        DPF(D_ERROR, ("[AhApplyEndpointNames] no direction words; per-peer naming disabled"));
        Slot->NameFallback = TRUE;
        return;
    }

    AH_NAME_TARGET targets[AH_NAME_DIRECTIONS];
    AhNameTargets(Slot, targets);

    for (ULONG i = 0; i < AH_NAME_DIRECTIONS; i++)
    {
        NTSTATUS st = AhComposeEndpointName(Slot->Display, targets[i].DirectionWord,
                                            targets[i].Name, AH_ENDPOINT_NAME_CHARS);
        if (!NT_SUCCESS(st))
        {
            DPF(D_ERROR, ("[AhApplyEndpointNames] compose %u failed 0x%x", i, st));
            Slot->NameFallback = TRUE;
            return;
        }
    }

    if (Flags & AH_BINDFLAG_FAIL_ENDPOINT_NAME)
    {
        //
        // Fault injection, and the negative control the naming test needs: with
        // this bit set the assertion that must pass on the happy path has to
        // FAIL. A naming test without it can only show that some string is
        // present, not that this driver is what put it there.
        //
        Slot->NameFallback = TRUE;
        return;
    }

    //
    // Anything written has to be removable, so the flag goes up BEFORE the
    // first write rather than after the last.
    //
    Slot->NamesWritten = TRUE;

    for (ULONG i = 0; i < AH_NAME_DIRECTIONS; i++)
    {
        NTSTATUS st = AhWriteEndpointName(targets[i].ReferenceString, targets[i].Name);
        if (!NT_SUCCESS(st))
        {
            //
            // ALL OR NOTHING, for the same reason the install is: one direction
            // named after the peer and the other not is a device pair that lies
            // about what it is. Take back whatever landed FIRST, then record the
            // decision -- AhRemoveEndpointNames must not be in a position to
            // observe, or undo, a flag it does not own.
            //
            DPF(D_ERROR, ("[AhApplyEndpointNames] write %u failed 0x%x", i, st));
            AhRemoveEndpointNames(Slot);
            Slot->NameFallback = TRUE;
            return;
        }
    }

    DPF(D_TERSE, ("[AhApplyEndpointNames] %S / %S", Slot->NameOut, Slot->NameIn));
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

#pragma code_seg("PAGE")
static NTSTATUS
AhBuildMinipairs(
    _Inout_ PAH_SLOT Slot
    )
/*++

Routine Description:

    Fills the slot's two ENDPOINT_MINIPAIRs from the static templates.

    EVERY DESCRIPTOR IS SHARED -- filters, pin arrays, node tables, connection
    tables, data ranges, automation tables and the format-and-modes tables.
    Nothing in them varies per peer.

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

    The slot's endpoint-name values go here too, and not at the three call
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
        AhRemoveEndpointNames(Slot);
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

    AhRemoveEndpointNames(Slot);

    if (FailStage != NULL) { *FailStage = stage; }
    return firstError;
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
// handler. So the mapping is a 33-entry table over the scalar's top bits, with
// linear interpolation in between. The error against the true curve is under
// 0.35 dB everywhere above -60 dB, which is far below what the 1 dB granularity
// of the Windows volume slider can express.
//
// THE TABLE IS A STARTING POINT AND MUST BE CHECKED AGAINST THE REAL SYSTEM.
// The claim being made is "the number IAudioEndpointVolume::GetMasterVolume-
// LevelScalar reports equals the scalar the peer applies", and that is a
// statement about two pieces of software neither of which is documented to
// this precision. Deriving it from the formula and declaring victory is how a
// volume that is consistently 3 dB off ships.
//
static const LONG g_AhDbTable[33] = {
    //
    // index i corresponds to scalar = i/32; value is round(20*log10(scalar) * 65536),
    // clamped at the KS floor. Index 0 is silence.
    //
    VOLUME_SIGNED_MINIMUM,          // 0.000
    -2097152,  // 0.03125  -> -30.10 dB
    -1703936,  // 0.0625   -> -24.08 dB
    -1474560,  // 0.09375  -> -20.56 dB
    -1310720,  // 0.125    -> -18.06 dB
    -1183744,  // 0.15625  -> -16.12 dB
    -1081344,  // 0.1875   -> -14.54 dB
    -993280,   // 0.21875  -> -13.20 dB
    -917504,   // 0.250    -> -12.04 dB
    -851968,   // 0.28125  -> -11.02 dB
    -790528,   // 0.3125   -> -10.10 dB
    -737280,   // 0.34375  ->  -9.27 dB
    -688128,   // 0.375    ->  -8.52 dB
    -643072,   // 0.40625  ->  -7.82 dB
    -602112,   // 0.4375   ->  -7.17 dB
    -565248,   // 0.46875  ->  -6.57 dB
    -524288,   // 0.500    ->  -6.02 dB
    -495616,   // 0.53125  ->  -5.49 dB
    -462848,   // 0.5625   ->  -5.00 dB
    -434176,   // 0.59375  ->  -4.53 dB
    -405504,   // 0.625    ->  -4.08 dB
    -380928,   // 0.65625  ->  -3.66 dB
    -356352,   // 0.6875   ->  -3.25 dB
    -331776,   // 0.71875  ->  -2.86 dB
    -311296,   // 0.750    ->  -2.50 dB
    -290816,   // 0.78125  ->  -2.14 dB
    -270336,   // 0.8125   ->  -1.80 dB
    -249856,   // 0.84375  ->  -1.47 dB
    -233472,   // 0.875    ->  -1.16 dB
    -212992,   // 0.90625  ->  -0.85 dB
    -196608,   // 0.9375   ->  -0.56 dB
    -180224,   // 0.96875  ->  -0.28 dB
    AH_VOLUME_UNITY // 1.000 -> 0 dB
};

#pragma code_seg()
LONG
AhScalarQ16ToKsVolume(
    _In_ ULONG ScalarQ16
    )
{
    if (ScalarQ16 == 0)
    {
        return VOLUME_SIGNED_MINIMUM;
    }
    if (ScalarQ16 >= 0x10000u)
    {
        return AH_VOLUME_UNITY;
    }

    //
    // 0..65535 -> table index 0..32 plus a fraction, all integer.
    //
    ULONG scaled = ScalarQ16 * 32u;         // < 2^21, no overflow
    ULONG idx    = scaled >> 16;            // 0..31
    ULONG frac   = scaled & 0xFFFFu;

    LONG lo = g_AhDbTable[idx];
    LONG hi = g_AhDbTable[idx + 1];

    return lo + (LONG)(((LONGLONG)(hi - lo) * (LONGLONG)frac) >> 16);
}

#pragma code_seg()
ULONG
AhKsVolumeToScalarQ16(
    _In_ LONG Level
    )
{
    if (Level >= AH_VOLUME_UNITY)
    {
        return 0x10000u;
    }
    if (Level <= VOLUME_SIGNED_MINIMUM)
    {
        return 0;
    }

    //
    // Inverse of the table, by search. 33 comparisons at property-set rate is
    // not worth a second table, and a second table is a second thing to keep
    // in step with the first.
    //
    for (ULONG i = 32; i > 0; i--)
    {
        if (Level >= g_AhDbTable[i - 1])
        {
            LONG lo = g_AhDbTable[i - 1];
            LONG hi = g_AhDbTable[i];
            ULONG base = (i - 1) * 2048u;   // (i-1)/32 in Q16
            if (hi == lo)
            {
                return base;
            }
            ULONG frac = (ULONG)(((LONGLONG)(Level - lo) << 16) / (LONGLONG)(hi - lo));
            if (frac > 0xFFFFu) { frac = 0xFFFFu; }
            return base + ((frac * 2048u) >> 16);
        }
    }
    return 0;
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

#pragma code_seg("PAGE")
VOID
AhPerPeerDriverInit(VOID)
{
    PAGED_CODE();

    RtlZeroMemory(g_AhSlots, sizeof(g_AhSlots));
    RtlZeroMemory(g_AhTopoObj, sizeof(g_AhTopoObj));
    KeInitializeSpinLock(&g_AhTopoLock);
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
            // AH_BINDREPLY_FLAG_NAME_FALLBACK, so the daemon can say that
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
                if (s->NameFallback)
                {
                    Result->Flags |= AH_BINDREPLY_FLAG_NAME_FALLBACK;
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
    // AFTER the reference strings (the name is written into the interface those
    // strings identify) and BEFORE the filters exist.
    //
    // "Before" is load-bearing and not merely tidy. PcRegisterSubdevice is the
    // step that ENABLES the interface, and the endpoint builder acts on the
    // arrival edge; a name written afterwards is a race whose losing side is a
    // device published under the wrong name with nothing to say so. Worse, the
    // composed name is CACHED per endpoint id, so losing that race once is
    // permanent for that peer rather than something the next bind repairs.
    //
    AhApplyEndpointNames(s, Flags);
    if (s->NameFallback)
    {
        Result->Flags |= AH_BINDREPLY_FLAG_NAME_FALLBACK;
    }

    status = AhBuildMinipairs(s);
    if (!NT_SUCCESS(status))
    {
        Result->Stage = AH_STAGE_ENDPOINT_NAME;
        Result->NtStatus = status;
        *AhStatus = AH_STATUS_INTERNAL;
        AhRemoveEndpointNames(s);
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
            &s->OutCtx,             // DeviceContext -> every miniport of this
                                    // endpoint learns its slot and direction
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
            &s->InCtx,
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
                  Slot, s->NameFallback ? s->Display : s->NameOut,
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
