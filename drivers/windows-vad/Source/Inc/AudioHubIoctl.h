/*++

Module Name:

    AudioHubIoctl.h

Abstract:

    THE FROZEN CONTROL-PLANE CONTRACT between audiohubd (user mode, Rust) and
    the AudioHub virtual audio driver (kernel mode).

    This file is the AUTHORITY. `core/audiohubd/src/halbridge_win.rs` is a
    hand-maintained Rust mirror of it, and `test/tests/halwire_win.rs` pins the
    two together from the outside so a one-sided edit fails `cargo test`
    instead of failing on a real machine.

    Design notes that are NOT obvious and must not be "simplified" away:

    * Every IOCTL is METHOD_BUFFERED. These are control-plane messages (a few
      per second, not one per 10 ms), so having the I/O manager probe and copy
      the buffers is the SAFEST choice, not a performance mistake. The audio
      data plane will NOT use IOCTLs at all.

    * `display` is UTF-16, not UTF-8. The daemon converts with
      `str::encode_utf16()`, which cannot fail. Doing UTF-8 -> UTF-16 in the
      kernel would add a conversion that can fail, can truncate, and can emit
      half a surrogate pair straight into the registry.

    * The receiver ALWAYS terminates what it receives and never trusts the
      sender's terminator (same rule as drivers/macos-hal/src/AudioHubBridge.h).

    * `peer_key` is the peer FINGERPRINT, not a slot number. It becomes the
      device-interface reference string, which is what Windows hangs the
      default-device choice, per-app assignment, endpoint volume and any
      user rename off. A slot number would let a NEW peer silently inherit a
      PREVIOUS peer's endpoint identity when the slot is recycled.

    Standalone mode: define AUDIOHUB_IOCTL_STANDALONE to compile this header
    with no Windows headers at all (used by the cross-platform contract test,
    which runs clang on macOS).

--*/

#ifndef _AUDIOHUB_IOCTL_H_
#define _AUDIOHUB_IOCTL_H_

#ifdef AUDIOHUB_IOCTL_STANDALONE
//
// Enough of the Windows type vocabulary to compile the structs anywhere. This
// exists ONLY so the contract test can measure the layout the MSVC/kernel build
// will produce, on a machine that has no WDK.
//
#include <stdint.h>
#include <stddef.h>
typedef uint32_t UINT32;
typedef uint64_t UINT64;
typedef char     CHAR;
typedef uint16_t WCHAR;   // MSVC's wchar_t is 16-bit; clang's is 32-bit, so the
                          // probe must NOT use wchar_t here or every offset
                          // past `display` would be measured wrong.
#define C_ASSERT(e) _Static_assert((e), #e)
#define AH_FIELD_OFFSET(t, f) ((UINT32)offsetof(t, f))
#define CTL_CODE(DeviceType, Function, Method, Access) \
    (((DeviceType) << 16) | ((Access) << 14) | ((Function) << 2) | (Method))
#define FILE_DEVICE_UNKNOWN 0x00000022
#define METHOD_BUFFERED     0
#define FILE_READ_DATA      0x0001
#define FILE_WRITE_DATA     0x0002
#else
#define AH_FIELD_OFFSET(t, f) ((UINT32)FIELD_OFFSET(t, f))
#endif // AUDIOHUB_IOCTL_STANDALONE

//=============================================================================
// Versioning and capacity
//=============================================================================

//
// INDEPENDENT NAMESPACE. The macOS bridge is at protocol 2; that number has
// nothing to do with this one and the two must never be "unified".
//
// v2: AH_BIND_REPLY grew `stage` / `nt_status` / `published`, and AH_SLOT_INFO
// grew `published`. The bump is deliberate rather than a compatible append:
// a v1 daemon reading a v2 reply would see the SAME 16 leading bytes and
// therefore keep believing "status == OK" means "both endpoints exist" -- which
// is exactly the belief this version exists to destroy.
//
// v3: PER-PEER DEVICE NAMES, first attempt. The driver derived a pin-name GUID
// from the peer fingerprint and wrote the composed label under MediaCategories.
// `reserved` became `flags`, carrying AH_BINDREPLY_FLAG_NAME_FALLBACK.
//
// v4: THE SAME FEATURE, THROUGH A MECHANISM THAT WORKS IN BOTH DIRECTIONS.
// v3's pin-name route names the microphone and CANNOT name the speaker: the
// endpoint builder hardcodes the name of any endpoint whose bridge pin carries
// KSNODETYPE_SPEAKER, ignoring both the pin's Name GUID and the Category entry
// it would otherwise fall back to (measured; see perpeer.h). v4 delivers the
// name as PKEY_Device_DeviceDesc under the device interface's EP\0 key
// instead, which the endpoint builder applies to render and capture alike.
//
// The bump is what stops a v4 daemon from believing a v3 driver. That pairing
// is the dangerous one and it is SILENT: v3 writes the speaker's name into a
// registry key nothing reads, finds no error to report, and answers
// AH_STATUS_OK with the fallback bit CLEAR -- so the daemon is told every
// device carries its peer's name while every speaker in the list reads
// "<speaker>". A version check is the only place that difference is
// expressible, and it is an EQUALITY test, so the mismatch refuses to bind.
//
#define AUDIOHUB_WIN_PROTOCOL_VERSION   4u

//
// Must equal HAL_MAX_SLOTS in core/audiohubd/src/halbridge.rs. The driver's
// PcAddAdapterDevice budget is derived from this and CANNOT be raised later,
// so this number is a build-time decision on both sides.
//
#define AUDIOHUB_WIN_MAX_SLOTS          16u

//
// Peer fingerprint: SHA256(pubkey)[0..8] as lowercase hex == 16 characters.
// The buffer is 40 to leave room without another protocol version, and to
// mirror `peer_key[40]` in AudioHubBridge.h.
//
#define AH_PEERKEY_CHARS                16u
#define AH_PEERKEY_BUF                  40u

//
// Filter FriendlyName, UTF-16 code units INCLUDING the terminator. The daemon
// clamps on a code-unit boundary that never splits a surrogate pair; the
// driver forces display[AH_DISPLAY_CHARS-1] = 0 regardless.
//
#define AH_DISPLAY_CHARS                128u

//=============================================================================
// Device naming
//=============================================================================

#define AH_CTL_DEVICE_NAME_W    L"\\Device\\AudioHubVadCtl"
#define AH_CTL_SYMLINK_W        L"\\??\\AudioHubVadCtl"
#define AH_CTL_USERMODE_W       L"\\\\.\\AudioHubVadCtl"

//
// The value in the device software key that holds the expected daemon image
// path(s). REG_SZ or REG_MULTI_SZ. Written by the (administrator) install
// script; a normal user cannot write under HKLM\SYSTEM, which is the whole
// integrity argument (§6.2).
//
#define AH_REGVAL_DAEMON_IMAGE  L"AudioHubDaemonImage"
//
// Optional DWORD override, values AH_CLIENT_CHECK_*. Absent => the driver picks
// the strongest level it can actually enforce.
//
#define AH_REGVAL_CLIENT_CHECK  L"AudioHubClientCheck"

//
// Caller-identity enforcement levels, mirroring kAudioHubPeerCheck_* on macOS.
// Reported back in AH_HELLO_REPLY.client_check so the level in force is
// OBSERVABLE rather than silently degraded.
//
#define AH_CLIENT_CHECK_NONE        0u  // debug only; never the default
#define AH_CLIENT_CHECK_ACL_ONLY    1u  // kernel DACL (SY/BA full, IU r/w) only
#define AH_CLIENT_CHECK_IMAGEPATH   2u  // + SeLocateProcessImageName comparison
#define AH_CLIENT_CHECK_SIGNATURE   3u  // + image signature (needs real signing)

//=============================================================================
// IOCTL codes
//=============================================================================

#define AH_DEVICE_TYPE  FILE_DEVICE_UNKNOWN
#define AH_ACCESS       (FILE_READ_DATA | FILE_WRITE_DATA)

#define IOCTL_AUDIOHUB_HELLO        CTL_CODE(AH_DEVICE_TYPE, 0x800, METHOD_BUFFERED, AH_ACCESS)
#define IOCTL_AUDIOHUB_BIND_SET     CTL_CODE(AH_DEVICE_TYPE, 0x801, METHOD_BUFFERED, AH_ACCESS)
#define IOCTL_AUDIOHUB_BIND_CLEAR   CTL_CODE(AH_DEVICE_TYPE, 0x802, METHOD_BUFFERED, AH_ACCESS)
#define IOCTL_AUDIOHUB_QUERY_SLOTS  CTL_CODE(AH_DEVICE_TYPE, 0x803, METHOD_BUFFERED, AH_ACCESS)
#define IOCTL_AUDIOHUB_CONTROL_PEND CTL_CODE(AH_DEVICE_TYPE, 0x804, METHOD_BUFFERED, AH_ACCESS)

//=============================================================================
// Status codes carried INSIDE the reply payload
//
// Distinct from the IRP's NTSTATUS on purpose: "the IOCTL completed" and "the
// driver agreed to do the thing" are different questions, and collapsing them
// is how a Set that hit a capacity limit gets mistaken for a published device.
//=============================================================================

#define AH_STATUS_OK                0u
#define AH_STATUS_BAD_VERSION       1u
#define AH_STATUS_BAD_ARGUMENT      2u
#define AH_STATUS_STALE_SESSION     3u
#define AH_STATUS_NO_ADAPTER        4u  // driver loaded, devnode not started yet
#define AH_STATUS_CAPACITY          5u
#define AH_STATUS_INTERNAL          6u
#define AH_STATUS_NOT_BOUND         7u
//
// A Set failed AND the rollback that should have made it "nothing at all" also
// failed, so the slot is left with one half of a device pair published.
//
// This is the ONLY status under which `published` may be a value other than 0
// or (AH_PUB_RENDER|AH_PUB_CAPTURE). It exists because the alternative --
// reporting the leftover as OK -- is the defect this protocol version was cut
// for: a speaker that never came back while the daemon was told "bound".
//
#define AH_STATUS_PARTIAL           8u

//=============================================================================
// Where a bind failed, and which halves of the pair are actually published.
//
// `stage` + `nt_status` are diagnostics: they never change what the daemon
// DOES (`status` alone decides that), they change what it can SAY. Without
// them the daemon can only report "the driver refused", which is indis-
// tinguishable from a dozen different kernel failures.
//=============================================================================

#define AH_STAGE_NONE               0u  // no failure
#define AH_STAGE_REFSTRINGS         1u  // building "AhTopoOut-<fingerprint>"
#define AH_STAGE_INSTALL_RENDER     2u  // InstallEndpointFilters(speaker)
#define AH_STAGE_INSTALL_CAPTURE    3u  // InstallEndpointFilters(microphone)
#define AH_STAGE_VERIFY             4u  // install said SUCCESS, left a NULL port
#define AH_STAGE_ROLLBACK           5u  // the undo of a failed Set failed too
#define AH_STAGE_DISCONNECT         6u  // UnregisterPhysicalConnection
#define AH_STAGE_UNREGISTER         7u  // UnregisterSubdevice
#define AH_STAGE_ENDPOINT_NAME      8u  // writing PKEY_Device_DeviceDesc into
                                        // the interface's EP\0 key

//
// Set alongside a SUCCESSFUL bind when the per-peer endpoint name could not be
// written and the endpoints therefore carry the system's generic direction
// names ("<speaker>" / "<microphone>") instead of the peer's.
//
// Deliberately a warning bit on an OK reply rather than a failure: a device
// with a generic name is enormously better than no device, and the daemon can
// still tell the user WHY two peers look alike. Deliberately not silent either
// -- "the driver said OK and it was not the whole truth" is the exact defect
// class protocol v2 was cut for, and a naming fallback nobody can observe is
// the same shape of lie in a smaller size.
//
#define AH_BINDREPLY_FLAG_NAME_FALLBACK 0x1u

//
// One bit per half of a peer's device pair. The invariant a bound slot must
// satisfy is `published == AH_PUB_BOTH`; anything else is a bug that is now
// ASSERTABLE from user mode instead of only visible by looking at the system's
// device list with human eyes.
//
#define AH_PUB_RENDER               0x1u
#define AH_PUB_CAPTURE              0x2u
#define AH_PUB_BOTH                 (AH_PUB_RENDER | AH_PUB_CAPTURE)

//=============================================================================
// Slot states (mirrors HalSlotState in halbridge.rs)
//=============================================================================

#define AH_SLOT_FREE        0u
#define AH_SLOT_BOUND       1u
#define AH_SLOT_DELISTED    2u

//=============================================================================
// HELLO
//=============================================================================

typedef struct _AH_HELLO_REQUEST {
    UINT32 protocol_version;
    UINT32 flags;               // MBZ
    UINT32 client_pid;          // driver LOGGING only; every decision uses the
                                // IRP's real caller, never this field
    UINT32 reserved;            // MBZ
} AH_HELLO_REQUEST;

typedef struct _AH_HELLO_REPLY {
    UINT32 status;              // AH_STATUS_*
    UINT32 protocol_version;    // the driver's; the daemon compares for EQUALITY
    UINT32 slot_count;          // the daemon takes min(its own, this)
    UINT32 caps;                // AH_CAP_*
    UINT64 session_id;          // increments on every accepted Hello
    UINT32 sample_rate;         // 0 while AH_CAP_DATAPLANE is clear
    UINT32 out_channels;
    UINT32 in_channels;
    UINT32 client_check;        // AH_CLIENT_CHECK_* actually in force
} AH_HELLO_REPLY;

#define AH_CAP_DATAPLANE    0x1u    // audio rings exist (0 until M6-4)

//=============================================================================
// BIND
//=============================================================================

#define AH_BIND_CLEAR   0u
#define AH_BIND_SET     1u

//
// The peer is connected. LOGGING ONLY on the driver's side: plan §7.3 says a
// paired peer's devices are published whether it is online or not, and the
// difference between online and offline lives entirely in the data plane.
// Explicitly NOT expressed as jack-detection or a disabled interface -- both
// would take the endpoint out of DEVICE_STATE_ACTIVE and make Windows move the
// user's default-device choice somewhere else.
//
#define AH_BINDFLAG_ONLINE  0x1u

//
// FAULT INJECTION. Never set by audiohubd -- only by `audiohub probe winvad`
// and by the regression harness, and the driver logs every use.
//
// These exist because "the driver must report a half-failed install honestly"
// is not testable without a way to MAKE one half fail: the natural failure is
// a kernel condition nobody can summon on demand. A test that can only observe
// the happy path is how the original defect survived a full acceptance run.
//
// The privilege argument: reaching this device already lets the caller create
// and destroy virtual audio endpoints. Being able to make that creation fail is
// strictly less power than being able to do it at all.
//
#define AH_BINDFLAG_FAIL_RENDER     0x100u  // SET: fail the speaker half
#define AH_BINDFLAG_FAIL_CAPTURE    0x200u  // SET: fail the microphone half
#define AH_BINDFLAG_SKIP_ROLLBACK   0x400u  // SET: leave the partial install in
                                            // place, so AH_STATUS_PARTIAL and
                                            // the `published` mask can be seen
#define AH_BINDFLAG_LEGACY_UNBIND   0x800u  // CLEAR: unregister the physical
                                            // connection through the TOPOLOGY
                                            // port even when the connection was
                                            // registered from the WAVE port --
                                            // i.e. reproduce the M6-2 defect on
                                            // demand (see common.cpp
                                            // DisconnectTopologies)
#define AH_BINDFLAG_FAIL_ENDPOINT_NAME 0x1000u
                                            // SET: skip the per-peer endpoint
                                            // name write, so the fallback path
                                            // and AH_BINDREPLY_FLAG_NAME_FALLBACK
                                            // can be observed without breaking
                                            // the registry by hand.
                                            //
                                            // This is also the NEGATIVE CONTROL
                                            // for the naming test: the same
                                            // assertion that must pass with the
                                            // name written must FAIL with this
                                            // bit set. Without it, a test can
                                            // only prove that some string is
                                            // present, not that this driver put
                                            // it there.

#define AH_BINDFLAG_DEBUG_MASK \
    (AH_BINDFLAG_FAIL_RENDER | AH_BINDFLAG_FAIL_CAPTURE | \
     AH_BINDFLAG_SKIP_ROLLBACK | AH_BINDFLAG_LEGACY_UNBIND | \
     AH_BINDFLAG_FAIL_ENDPOINT_NAME)

typedef struct _AH_BIND_REQUEST {
    UINT32 op;                          // AH_BIND_SET | AH_BIND_CLEAR
    UINT32 slot;                        // 0 .. slot_count-1
    UINT32 flags;                       // AH_BINDFLAG_*
    UINT32 generation;                  // CLEAR: what the daemon believes.
                                        // SET: 0, the driver allocates.
    UINT64 session_id;                  // != HelloReply => AH_STATUS_STALE_SESSION
    CHAR   peer_key[AH_PEERKEY_BUF];    // ASCII hex; driver enforces [0-9a-f]{16}
    WCHAR  display[AH_DISPLAY_CHARS];   // UTF-16LE. The peer's BASE name, i.e.
                                        // "AudioHub - <host>" WITH the prefix
                                        // and WITHOUT any direction suffix.
                                        // The driver appends the direction word
                                        // (read from the INF, never hardcoded)
                                        // to build each pin's label, and also
                                        // sets this verbatim as the filter's
                                        // DEVPKEY_DeviceInterface_FriendlyName.
} AH_BIND_REQUEST;

typedef struct _AH_BIND_REPLY {
    UINT32 status;
    UINT32 slot;
    UINT32 generation;      // after a successful SET, the newly allocated stamp
    UINT32 state;           // AH_SLOT_*
    UINT32 stage;           // AH_STAGE_* -- NONE unless something failed
    UINT32 nt_status;       // the raw NTSTATUS of that stage, 0 when NONE
    UINT32 published;       // AH_PUB_* bitmask as it stands AFTER this call.
                            // status==OK on a SET requires AH_PUB_BOTH;
                            // status==OK on a CLEAR requires 0.
    UINT32 flags;           // AH_BINDREPLY_FLAG_* (v3; MBZ reserved in v2)
} AH_BIND_REPLY;

//=============================================================================
// QUERY_SLOTS -- the reconciliation read. The macOS bridge gets the same
// information pushed at it as CTL_BIND_STATE; here the daemon pulls it, which
// is what lets it detect a slot it leaked (Set with no Clear) after a restart.
//=============================================================================

typedef struct _AH_SLOT_INFO {
    UINT32 state;                       // AH_SLOT_*
    UINT32 generation;
    CHAR   peer_key[AH_PEERKEY_BUF];    // NUL-filled when free
    UINT32 published;                   // AH_PUB_* as the driver's own port
                                        // pointers stand right now. A slot that
                                        // says BOUND with anything but
                                        // AH_PUB_BOTH is the failure this field
                                        // was added to make detectable.
} AH_SLOT_INFO;

typedef struct _AH_QUERY_SLOTS_REPLY {
    UINT32       status;
    UINT32       slot_count;
    UINT64       session_id;
    AH_SLOT_INFO slots[AUDIOHUB_WIN_MAX_SLOTS];
} AH_QUERY_SLOTS_REPLY;

//=============================================================================
// CONTROL_PEND -- the inverted call (driver -> daemon).
//
// M6-2 defines it but never completes it: there is no volume node and no data
// plane yet. It is nailed down NOW so that adding either in M6-3/M6-4 does not
// force a protocol version bump.
//
// THE HANDLE THIS ARRIVES ON MUST BE OPENED FILE_FLAG_OVERLAPPED. A synchronous
// file object serialises every request on it, so this deliberately-pending IRP
// would park every subsequent BIND_SET behind itself forever. The failure is a
// deadlock, not an error code.
//=============================================================================

#define AH_EVENT_NONE       0u
#define AH_EVENT_VOLUME     1u
#define AH_EVENT_IOSTATE    2u
#define AH_EVENT_SLOT       3u

#define AH_EVFLAG_INPUT     0x1u    // the virtual MICROPHONE, else the speaker
#define AH_EVFLAG_MUTED     0x2u
#define AH_EVFLAG_RUNNING   0x4u

typedef struct _AH_CONTROL_EVENT {
    UINT32 kind;            // AH_EVENT_*
    UINT32 slot;
    UINT32 generation;      // the stamp the event belongs to; the daemon drops
                            // anything that does not match the slot's current
                            // one, so a late event from a slot's previous
                            // tenant cannot light up the next peer's device
    UINT32 flags;           // AH_EVFLAG_*
    UINT32 scalar_q16;      // volume as 16.16 fixed point (0x10000 == 1.0).
                            // Fixed point, not float: kernel code must not use
                            // the FPU without saving state.
    UINT32 state;           // AH_SLOT_* for AH_EVENT_SLOT, else 0
} AH_CONTROL_EVENT;

//=============================================================================
// Layout assertions. These are the whole point of the file: the Rust mirror
// asserts the same numbers, and test/tests/halwire_win.rs asserts both against
// a third, independently transcribed copy.
//=============================================================================

C_ASSERT(sizeof(AH_HELLO_REQUEST) == 16);
C_ASSERT(sizeof(AH_HELLO_REPLY) == 40);
C_ASSERT(sizeof(AH_BIND_REQUEST) == 320);
C_ASSERT(sizeof(AH_BIND_REPLY) == 32);
C_ASSERT(sizeof(AH_SLOT_INFO) == 52);
C_ASSERT(sizeof(AH_QUERY_SLOTS_REPLY) == 848);
C_ASSERT(sizeof(AH_CONTROL_EVENT) == 24);

C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REPLY, stage) == 16);
C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REPLY, nt_status) == 20);
C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REPLY, published) == 24);
C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REPLY, flags) == 28);
C_ASSERT(AH_FIELD_OFFSET(AH_SLOT_INFO, published) == 48);

C_ASSERT(AH_FIELD_OFFSET(AH_HELLO_REPLY, session_id) == 16);
C_ASSERT(AH_FIELD_OFFSET(AH_HELLO_REPLY, sample_rate) == 24);
C_ASSERT(AH_FIELD_OFFSET(AH_HELLO_REPLY, client_check) == 36);

C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REQUEST, session_id) == 16);
C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REQUEST, peer_key) == 24);
C_ASSERT(AH_FIELD_OFFSET(AH_BIND_REQUEST, display) == 64);

C_ASSERT(AH_FIELD_OFFSET(AH_SLOT_INFO, peer_key) == 8);
C_ASSERT(AH_FIELD_OFFSET(AH_QUERY_SLOTS_REPLY, session_id) == 8);
C_ASSERT(AH_FIELD_OFFSET(AH_QUERY_SLOTS_REPLY, slots) == 16);

//
// The IOCTL codes as literals. CTL_CODE is a macro on both sides, so this is
// the only place the ARITHMETIC gets checked -- and the daemon has to reproduce
// exactly these u32s with no Windows headers in sight.
//
C_ASSERT(IOCTL_AUDIOHUB_HELLO        == 0x0022E000);
C_ASSERT(IOCTL_AUDIOHUB_BIND_SET     == 0x0022E004);
C_ASSERT(IOCTL_AUDIOHUB_BIND_CLEAR   == 0x0022E008);
C_ASSERT(IOCTL_AUDIOHUB_QUERY_SLOTS  == 0x0022E00C);
C_ASSERT(IOCTL_AUDIOHUB_CONTROL_PEND == 0x0022E010);

#endif // _AUDIOHUB_IOCTL_H_
