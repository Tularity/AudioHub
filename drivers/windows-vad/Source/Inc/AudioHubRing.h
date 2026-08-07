/*++

Module Name:

    AudioHubRing.h

Abstract:

    THE FROZEN DATA-PLANE CONTRACT: the shared-memory audio ring between the
    AudioHub virtual audio driver (kernel) and audiohubd (user mode, Rust).

    The layout, the two index semantics and both transfer functions are a
    LITERAL PORT of drivers/macos-hal/src/AudioHubBridge.h:60-206. Only the
    atomic primitives differ (C11 _Atomic -> the WDK's ReadAcquire64 family).
    Everything else -- the 40-byte header, the 64-byte data offset, the 16K
    page alignment, 48 kHz / 500 ms / 2ch out / 1ch in, "full ring drops the
    tail" -- is byte-for-byte the same on both platforms, deliberately:
    `core/audiohubd/src/halbridge.rs` reads BOTH through one set of constants
    (HAL_RING_DATA_OFFSET, HAL_RING_FRAMES, HAL_SPK_CHANNELS, ...) and one
    `RingMem` implementation, and a Windows-specific geometry would fork the
    only piece of the audio path the two platforms genuinely share.

    WHY THE RING CARRIES 32-BIT FLOAT
    =================================
    Not for fidelity -- for IRQL. The transfer below runs in the WaveRT timer
    DPC, i.e. at DISPATCH_LEVEL, and on x64 a kernel-mode driver may not touch
    the FPU/SSE at IRQL >= DISPATCH_LEVEL without bracketing it in
    KeSaveExtendedProcessorState/KeRestoreExtendedProcessorState. Any integer
    PCM format in the WaveRT buffer would force a per-sample int<->float
    conversion right there. Publishing the wave pins as
    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT instead makes the WaveRT buffer ALREADY
    hold what the ring holds, so the DPC is a pure memcpy and the FPU question
    never arises. (The Windows audio engine mixes in float anyway, so this
    removes a conversion rather than adding one.)

    WHAT IS DELIBERATELY *NOT* HERE
    ===============================
    No volume. The samples in these rings are FULL SCALE, always, on both
    platforms -- plan.md 7.2's transmission invariant. The volume a user sets
    on a virtual endpoint travels as a NUMBER over the control plane and is
    applied by the far side to its real device. A gain applied here would be
    applied a second time there, and the double attenuation would be invisible
    to both ends. This is also the entire reason the driver must declare
    KSNODETYPE_VOLUME: without a hardware volume node the audio engine inserts
    a software volume APO of its own, which attenuates BEFORE the data reaches
    this ring -- the same double attenuation, just upstream of where anyone
    would think to look for it.

--*/

#ifndef _AUDIOHUB_RING_H_
#define _AUDIOHUB_RING_H_

#ifdef AUDIOHUB_RING_STANDALONE
//
// Host-compilable shape, used ONLY by the cross-platform contract test (clang
// on macOS). It measures the layout the MSVC/kernel build will produce; it
// never runs the transfer functions.
//
#include <stdint.h>
#include <stddef.h>
#include <string.h>
typedef uint32_t ULONG;
typedef uint64_t ULONG64;
typedef int64_t  LONG64;
typedef uint8_t  UINT8;
#define C_ASSERT(e) _Static_assert((e), #e)
#define AH_RING_FIELD_OFFSET(t, f) ((ULONG)offsetof(t, f))
#else
#define AH_RING_FIELD_OFFSET(t, f) ((ULONG)FIELD_OFFSET(t, f))
#endif

//=============================================================================
// Geometry. Every one of these has an equal-by-construction twin in
// core/audiohubd/src/halbridge.rs, and test/tests/halwire_win.rs pins them.
//=============================================================================

//
// Samples start here so the 40-byte header never shares a cache line with
// frame 0. Both sides hard-code it rather than deriving it from sizeof().
//
#define AUDIOHUB_RING_DATA_OFFSET   64u
#define AUDIOHUB_RING_SAMPLE_RATE   48000u
#define AUDIOHUB_RING_MS            500u
#define AUDIOHUB_RING_FRAMES        ((AUDIOHUB_RING_SAMPLE_RATE / 1000u) * AUDIOHUB_RING_MS) // 24000
#define AUDIOHUB_SPK_CHANNELS       2u
#define AUDIOHUB_MIC_CHANNELS       1u

//
// 16K. Windows x64 pages are 4K, so this is a multiple of the page size and
// therefore legal for MmMapLockedPagesSpecifyCache (whose user-mode mapping
// size "must be a multiple of PAGE_SIZE"); it is ALSO what the macOS side uses
// (Apple silicon's 16K page), which is what lets one Rust constant cover both.
//
#define AUDIOHUB_RING_PAGE_ALIGN(n) (((n) + 16383u) & ~16383u)
#define AUDIOHUB_RING_BYTES(ch)     AUDIOHUB_RING_PAGE_ALIGN(AUDIOHUB_RING_DATA_OFFSET + (AUDIOHUB_RING_FRAMES * (ch) * 4u))
#define AUDIOHUB_SPK_BYTES          AUDIOHUB_RING_BYTES(AUDIOHUB_SPK_CHANNELS)   // 196608
#define AUDIOHUB_MIC_BYTES          AUDIOHUB_RING_BYTES(AUDIOHUB_MIC_CHANNELS)   //  98304

//
// Identity written by the driver at ring creation and checked ONCE by the
// daemon at attach. Same values as the macOS bridge: the daemon's check is
// shared code and there is nothing platform-specific to distinguish.
//
#define AUDIOHUB_RING_MAGIC         0x41485231u // 'AHR1'
#define AUDIOHUB_RING_VERSION       1u

//
// Direction encoding, identical to AudioHubBridge.h:238-252 and to
// AudioHubIoctl.h's slot numbering.
//
#define AUDIOHUB_DIR_OUT            0u  // driver WRITES, daemon READS  (virtual speaker)
#define AUDIOHUB_DIR_IN             1u  // daemon WRITES, driver READS  (virtual microphone)
#define AUDIOHUB_RING_INDEX(slot, dir) (((slot) * 2u) + (dir))

//=============================================================================
// The header.
//
// Single producer / single consumer. write_idx and read_idx are FREE-RUNNING
// frame counters -- never wrapped, never reset by the peer. The producer owns
// write_idx, the consumer owns read_idx, and neither ever writes the other's.
// That is what makes every access below wait-free: no CAS loop, no lock, no
// allocation, no wait -- which is exactly the DPC contract.
//=============================================================================

//
// Natural alignment, stated rather than assumed. The struct is six ULONGs
// followed by two ULONG64s: on both ABIs the compiler inserts nothing, and the
// C_ASSERTs below are what proves it rather than the pragma.
//
#if !defined(AUDIOHUB_RING_STANDALONE) && !defined(AUDIOHUB_RING_HOSTTEST)
#include <pshpack8.h>
#endif
typedef struct _AUDIOHUB_RING_HEADER
{
    ULONG   Magic;
    ULONG   Version;
    ULONG   SampleRate;
    ULONG   Channels;
    ULONG   CapacityFrames;
    ULONG   Reserved;       // the padding both ABIs insert anyway, named so the
                            // offset assertions below can see it
    ULONG64 WriteIdx;       // producer only; accessed through ReadAcquire64 &c
    ULONG64 ReadIdx;        // consumer only
} AUDIOHUB_RING_HEADER, *PAUDIOHUB_RING_HEADER;
#if !defined(AUDIOHUB_RING_STANDALONE) && !defined(AUDIOHUB_RING_HOSTTEST)
#include <poppack.h>
#endif

C_ASSERT(AH_RING_FIELD_OFFSET(AUDIOHUB_RING_HEADER, WriteIdx) == 24);
C_ASSERT(AH_RING_FIELD_OFFSET(AUDIOHUB_RING_HEADER, ReadIdx) == 32);
C_ASSERT(sizeof(AUDIOHUB_RING_HEADER) == 40);
C_ASSERT(sizeof(AUDIOHUB_RING_HEADER) <= AUDIOHUB_RING_DATA_OFFSET);
C_ASSERT(AUDIOHUB_RING_FRAMES == 24000u);
C_ASSERT(AUDIOHUB_SPK_BYTES == 196608u);
C_ASSERT(AUDIOHUB_MIC_BYTES == 98304u);

#ifndef AUDIOHUB_RING_STANDALONE

//=============================================================================
// Atomics.
//
// x64 aligned 64-bit loads and stores are atomic by the architecture, so the
// only thing these add is the COMPILER/CPU ordering the SPSC argument needs.
// ReadAcquire64 / WriteRelease64 are the WDK's documented spellings and are
// intrinsics, so none of this emits a call, takes a lock or touches the FPU.
//
// They take `volatile LONG64*`; the indices are unsigned because every use is
// a wrapping subtraction. The cast is between two 64-bit integer types of the
// same size and the bit pattern is what both sides actually compare.
//=============================================================================

__forceinline ULONG64 AhRingLoadAcquire(_In_ const ULONG64 *Addr)
{
    return (ULONG64)ReadAcquire64((volatile const LONG64 *)Addr);
}

__forceinline ULONG64 AhRingLoadRelaxed(_In_ const ULONG64 *Addr)
{
    return (ULONG64)ReadNoFence64((volatile const LONG64 *)Addr);
}

__forceinline VOID AhRingStoreRelease(_Inout_ ULONG64 *Addr, _In_ ULONG64 Value)
{
    WriteRelease64((volatile LONG64 *)Addr, (LONG64)Value);
}

__forceinline float *AhRingData(_In_ PAUDIOHUB_RING_HEADER Header, _In_ ULONG DataOffset)
{
    return (float *)(((UINT8 *)Header) + DataOffset);
}

//=============================================================================
// GEOMETRY IS THE CALLER'S, NEVER THE HEADER'S.
//
// The daemon maps this memory READ/WRITE, so CapacityFrames, Channels and
// Magic read back out of the header are values the DAEMON can change. A
// daemon that stored CapacityFrames = 0x40000000 would aim the memcpys below
// gigabytes past the end of the mapping -- from a DPC, in the kernel. So
// DataOffset / CapacityFrames / ChannelCount are passed IN by the caller and
// come from the driver's own private ring record, written once at creation.
// Re-reading Magic here as an "integrity check" would be worse than useless:
// the only party that can corrupt it is the only party that can fake it.
//
// The two indices must stay in shared memory because that is what they are
// for, and every use of them below is bounded by the CALLER's capacity.
//=============================================================================

//
// Producer. Returns frames actually written; a full ring DROPS THE TAIL rather
// than waiting, because the only caller is a DPC and a DPC that waits is a
// system-wide audio glitch, not a local one.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
__forceinline ULONG AhRingWrite(
    _In_ PAUDIOHUB_RING_HEADER Header,
    _In_ ULONG DataOffset,
    _In_ ULONG CapacityFrames,
    _In_ ULONG ChannelCount,
    _In_reads_(FrameCount * ChannelCount) const float *Frames,
    _In_ ULONG FrameCount)
{
    ULONG64 write, read, used;
    ULONG   count, start, first;
    float  *data;

    if ((FrameCount == 0) || (CapacityFrames == 0))
    {
        return 0;
    }

    write = AhRingLoadRelaxed(&Header->WriteIdx);
    read  = AhRingLoadAcquire(&Header->ReadIdx);

    //
    // ReadIdx belongs to the daemon, so it can hold anything at all: a
    // consumer that reset its index on reconnect legitimately reads ahead of
    // us for an instant, and a broken one can plant any value. Clamping to the
    // CALLER's capacity is what keeps the unsigned subtraction from reporting
    // a 2^64-sized backlog.
    //
    used = write - read;
    if (used > CapacityFrames)
    {
        used = CapacityFrames;
    }

    count = CapacityFrames - (ULONG)used;
    if (count > FrameCount)
    {
        count = FrameCount;
    }
    if (count == 0)
    {
        return 0;
    }

    data  = AhRingData(Header, DataOffset);
    start = (ULONG)(write % CapacityFrames);
    first = CapacityFrames - start;
    if (first > count)
    {
        first = count;
    }

    RtlCopyMemory(data + ((SIZE_T)start * ChannelCount),
                  Frames,
                  (SIZE_T)first * ChannelCount * sizeof(float));
    if (count > first)
    {
        RtlCopyMemory(data,
                      Frames + ((SIZE_T)first * ChannelCount),
                      (SIZE_T)(count - first) * ChannelCount * sizeof(float));
    }

    AhRingStoreRelease(&Header->WriteIdx, write + count);
    return count;
}

//
// Consumer. Returns frames actually read; the caller fills the remainder with
// silence.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
__forceinline ULONG AhRingRead(
    _In_ PAUDIOHUB_RING_HEADER Header,
    _In_ ULONG DataOffset,
    _In_ ULONG CapacityFrames,
    _In_ ULONG ChannelCount,
    _Out_writes_(FrameCount * ChannelCount) float *Frames,
    _In_ ULONG FrameCount)
{
    ULONG64 read, write, avail, effective;
    ULONG   count, start, first;
    const float *data;

    if ((FrameCount == 0) || (CapacityFrames == 0))
    {
        return 0;
    }

    read  = AhRingLoadRelaxed(&Header->ReadIdx);
    write = AhRingLoadAcquire(&Header->WriteIdx);

    avail = write - read;
    if (avail > CapacityFrames)
    {
        //
        // The producer got more than a full buffer ahead (we stalled): skip to
        // the newest full buffer instead of replaying stale audio. WriteIdx is
        // the daemon's, so this clamp is also the bound on `effective` below.
        //
        avail = CapacityFrames;
    }

    count = (avail > FrameCount) ? FrameCount : (ULONG)avail;
    if (count == 0)
    {
        return 0;
    }

    effective = write - avail;
    data      = AhRingData(Header, DataOffset);
    start     = (ULONG)(effective % CapacityFrames);
    first     = CapacityFrames - start;
    if (first > count)
    {
        first = count;
    }

    RtlCopyMemory(Frames,
                  data + ((SIZE_T)start * ChannelCount),
                  (SIZE_T)first * ChannelCount * sizeof(float));
    if (count > first)
    {
        RtlCopyMemory(Frames + ((SIZE_T)first * ChannelCount),
                      data,
                      (SIZE_T)(count - first) * ChannelCount * sizeof(float));
    }

    AhRingStoreRelease(&Header->ReadIdx, effective + count);
    return count;
}

//
// Occupancy, WITHOUT moving either index. Same expression as the two `used` /
// `avail` computations above (including the min-with-capacity and the wrapping
// semantics), which is the point: a telemetry read that used a different
// formula would report a depth the transfer path does not agree with.
//
// This is the ONLY read a telemetry caller may make. Observing must not
// consume -- the AudioHub ring is the one stage whose residency ceiling is
// exactly 500 ms, and an "observation" that advanced ReadIdx would zero the
// very quantity it claims to measure.
//
_IRQL_requires_max_(DISPATCH_LEVEL)
__forceinline ULONG AhRingOccupancy(
    _In_ PAUDIOHUB_RING_HEADER Header,
    _In_ ULONG CapacityFrames)
{
    ULONG64 read  = AhRingLoadRelaxed(&Header->ReadIdx);
    ULONG64 write = AhRingLoadAcquire(&Header->WriteIdx);
    ULONG64 used  = write - read;

    if (used > CapacityFrames)
    {
        used = CapacityFrames;
    }
    return (ULONG)used;
}

#endif // !AUDIOHUB_RING_STANDALONE

#endif // _AUDIOHUB_RING_H_
