//  AudioHubBridge.h — FROZEN wire contract between the AudioHub HAL plug-in
//  (running inside coreaudiod's sandbox) and audiohubd (spec-round2 §B1).
//
//  Both sides of this contract are hand-written: this header is the C side,
//  core/audiohubd/src/halbridge.rs is the Rust mirror. Every constant, struct
//  layout and message id below is duplicated there; the _Static_asserts at the
//  bottom of each section exist so a silent ABI drift fails the build instead
//  of producing noise on someone's speakers.
//
//  TRANSPORT DIRECTION — the driver registers, the daemon looks up. This is the
//  opposite of the obvious arrangement and the inversion is load-bearing; both
//  alternatives were built and measured on real hardware before this one:
//    - daemon as a per-user LaunchAgent owning "com.audiohub.daemon": coreaudiod
//      runs as _coreaudiod in the SYSTEM bootstrap namespace and cannot resolve
//      a gui/<uid> name, so the bridge never connected.
//    - daemon as a system-domain LaunchDaemon owning the same name: the bridge
//      connected, but the daemon lost local-network access — every outbound LAN
//      connect() returned EHOSTUNREACH while the same signed binary launched
//      from the user's shell worked. Local-network consent is bound to a user
//      session; a system-domain job has none, and neither a stable signing
//      identity nor toggling the Privacy setting changes that.
//  Those two requirements are mutually exclusive for one process, so the name
//  moves to the side that is already in the global namespace. The plug-in runs
//  inside coreaudiod, which IS global, so it bootstrap_check_in()s its OWN name
//  and the daemon — a plain user-session process with full network rights and no
//  sudo — bootstrap_look_up()s it. Namespace visibility is one-way: a user
//  process sees global names, not the reverse. Verified against Rogue Amoeba's
//  shipping ARK driver on this machine (look_up of "com.rogueamoeba.ARK.driver"
//  from gui/501 returns KERN_SUCCESS, and that name is absent from
//  `launchctl print gui/501` — i.e. it is global and visible from the session).
//
//  Shared memory is conveyed as mach memory-entry PORTS made with
//  mach_make_memory_entry_64() and MAP_MEM_NAMED_CREATE — NOT as
//  mach_msg_ool_descriptor_t and NOT by naming an existing vm_allocate()d range.
//  Both of those hand the receiver a COPY-ON-WRITE view: the handshake succeeds,
//  both sides see a valid header, and every write stays private to the writer.
//  That is exactly the failure this project already shipped once (the daemon's
//  spk_frames stayed at 0 while audio was playing). MAP_MEM_NAMED_CREATE makes a
//  fresh named VM object that every mapping of the entry shares, so the driver
//  creates the two objects, maps them itself, and sends send rights on the entry
//  ports. The receiving side must map with mach_vm_map(..., copy=FALSE, ...).

#ifndef AUDIOHUB_BRIDGE_H
#define AUDIOHUB_BRIDGE_H

#include <mach/mach.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

// Registered by the PLUG-IN (see above), declared in the bundle's Info.plist
// under AudioServerPlugIn_MachServices. audiohubd looks this name up.
#define kAudioHubDriverMachServiceName "com.audiohub.driver"

// ---------------------------------------------------------------- ring layout

#define AUDIOHUB_RING_MAGIC        0x41485231u // 'AHR1'
#define AUDIOHUB_RING_VERSION      1u
// Samples start here so the header (40 bytes) never shares a cache line with
// frame 0; both sides hard-code it rather than deriving it from sizeof().
#define AUDIOHUB_RING_DATA_OFFSET  64u
#define AUDIOHUB_SAMPLE_RATE       48000u
#define AUDIOHUB_RING_MS           500u
#define AUDIOHUB_RING_FRAMES       ((AUDIOHUB_SAMPLE_RATE / 1000u) * AUDIOHUB_RING_MS) // 24000
#define AUDIOHUB_SPK_CHANNELS      2u
#define AUDIOHUB_MIC_CHANNELS      1u

// 16K covers the Apple-silicon page size and is a multiple of the x86_64 4K
// one, so a single constant keeps both slices' mappings legal.
#define AUDIOHUB_PAGE_ALIGN(n)     (((n) + 16383u) & ~16383u)
#define AUDIOHUB_RING_BYTES(ch)    AUDIOHUB_PAGE_ALIGN(AUDIOHUB_RING_DATA_OFFSET + (AUDIOHUB_RING_FRAMES * (ch) * 4u))
#define AUDIOHUB_SPK_BYTES         AUDIOHUB_RING_BYTES(AUDIOHUB_SPK_CHANNELS)
#define AUDIOHUB_MIC_BYTES         AUDIOHUB_RING_BYTES(AUDIOHUB_MIC_CHANNELS)

// Single producer / single consumer. write_idx and read_idx are free-running
// frame counters (never wrapped, never reset by the peer): the producer owns
// write_idx, the consumer owns read_idx, and neither ever writes the other's.
// That is what makes every access below wait-free — no CAS loop, no lock.
typedef struct AudioHubRingHeader
{
    uint32_t          magic;
    uint32_t          version;
    uint32_t          sample_rate;
    uint32_t          channels;
    uint32_t          capacity_frames;
    uint32_t          reserved;   // the padding both ABIs insert anyway, named so the offsets are assertable
    _Atomic(uint64_t) write_idx;  // producer only
    _Atomic(uint64_t) read_idx;   // consumer only
} AudioHubRingHeader;

_Static_assert(offsetof(AudioHubRingHeader, write_idx) == 24, "ring header ABI drift");
_Static_assert(offsetof(AudioHubRingHeader, read_idx) == 32, "ring header ABI drift");
_Static_assert(sizeof(AudioHubRingHeader) == 40, "ring header ABI drift");
_Static_assert(sizeof(AudioHubRingHeader) <= AUDIOHUB_RING_DATA_OFFSET, "header overlaps sample area");
_Static_assert(AUDIOHUB_RING_FRAMES == 24000u, "500ms @ 48k");

static inline float* AudioHubRing_Data(AudioHubRingHeader* inHeader, uint32_t inDataOffset)
{
    return (float*)(((uint8_t*)inHeader) + inDataOffset);
}

// GEOMETRY IS THE CALLER'S, NEVER THE HEADER'S — this applies to both helpers
// below. The peer maps this memory READ/WRITE, so capacity_frames, channels and
// magic read back out of the header are values the PEER chooses: a peer that
// stores capacity_frames = 0x40000000 and write_idx = 0x3FFFFFFF aims the
// memcpys below ~8GB past the end of the mapping — inside coreaudiod, on the
// realtime thread, with a controlled offset and controlled bytes; on the mic
// side the same trick mirrors arbitrary coreaudiod memory out to any app
// recording the virtual microphone. inDataOffset / inCapacityFrames /
// inChannelCount therefore come from the driver's own BridgeRing record, which
// is private to this process and written once at ring creation. Re-reading
// magic and channels here as "integrity checks" was worse than useless: the
// only party that can corrupt those fields is the only party that can fake
// them. The two free-running indices stay in shared memory because they have
// to, and every use of them below is bounded by the caller's capacity.

// Producer side. Returns the number of frames actually written; a full ring
// drops the tail rather than waiting, because the only caller that matters is
// a HAL IOProc and a late IOProc is a system-wide audio glitch.
static inline uint32_t AudioHubRing_Write(AudioHubRingHeader* inHeader, uint32_t inDataOffset, uint32_t inCapacityFrames, uint32_t inChannelCount, const float* inFrames, uint32_t inFrameCount)
{
    if((inFrameCount == 0) || (inCapacityFrames == 0))
    {
        return 0;
    }
    const uint32_t theCapacity = inCapacityFrames;
    const uint64_t theWrite = atomic_load_explicit(&inHeader->write_idx, memory_order_relaxed);
    const uint64_t theRead  = atomic_load_explicit(&inHeader->read_idx, memory_order_acquire);
    // read_idx belongs to the peer, so it can hold anything at all — a consumer
    // that reset its index on reconnect legitimately reads ahead of us for an
    // instant, and a hostile one can plant any value. Clamping to the caller's
    // capacity is what keeps the unsigned subtraction from reporting a
    // 2^64-sized backlog; theWrite below is ours, so the index stays in range.
    uint64_t theUsed = theWrite - theRead;
    if(theUsed > theCapacity)
    {
        theUsed = theCapacity;
    }
    uint32_t theCount = theCapacity - (uint32_t)theUsed;
    if(theCount > inFrameCount)
    {
        theCount = inFrameCount;
    }
    if(theCount == 0)
    {
        return 0;
    }
    float* theData = AudioHubRing_Data(inHeader, inDataOffset);
    const uint32_t theStart = (uint32_t)(theWrite % theCapacity);
    uint32_t theFirst = theCapacity - theStart;
    if(theFirst > theCount)
    {
        theFirst = theCount;
    }
    memcpy(theData + ((size_t)theStart * inChannelCount), inFrames, (size_t)theFirst * inChannelCount * sizeof(float));
    if(theCount > theFirst)
    {
        memcpy(theData, inFrames + ((size_t)theFirst * inChannelCount), (size_t)(theCount - theFirst) * inChannelCount * sizeof(float));
    }
    atomic_store_explicit(&inHeader->write_idx, theWrite + theCount, memory_order_release);
    return theCount;
}

// Consumer side. Returns the number of frames actually read; the caller is
// responsible for filling the remainder with silence.
static inline uint32_t AudioHubRing_Read(AudioHubRingHeader* inHeader, uint32_t inDataOffset, uint32_t inCapacityFrames, uint32_t inChannelCount, float* outFrames, uint32_t inFrameCount)
{
    if((inFrameCount == 0) || (inCapacityFrames == 0))
    {
        return 0;
    }
    const uint32_t theCapacity = inCapacityFrames;
    const uint64_t theRead  = atomic_load_explicit(&inHeader->read_idx, memory_order_relaxed);
    const uint64_t theWrite = atomic_load_explicit(&inHeader->write_idx, memory_order_acquire);
    uint64_t theAvail = theWrite - theRead;
    if(theAvail > theCapacity)
    {
        // Producer got more than a full buffer ahead (we stalled): skip to the
        // newest full buffer instead of replaying stale audio. write_idx is the
        // peer's, so this clamp is also the bound on theEffectiveRead below —
        // and taking it modulo the caller's capacity keeps the index in range
        // whatever the peer stores there.
        theAvail = theCapacity;
    }
    uint32_t theCount = (theAvail > inFrameCount) ? inFrameCount : (uint32_t)theAvail;
    if(theCount == 0)
    {
        return 0;
    }
    const uint64_t theEffectiveRead = theWrite - theAvail;
    const float* theData = AudioHubRing_Data(inHeader, inDataOffset);
    const uint32_t theStart = (uint32_t)(theEffectiveRead % theCapacity);
    uint32_t theFirst = theCapacity - theStart;
    if(theFirst > theCount)
    {
        theFirst = theCount;
    }
    memcpy(outFrames, theData + ((size_t)theStart * inChannelCount), (size_t)theFirst * inChannelCount * sizeof(float));
    if(theCount > theFirst)
    {
        memcpy(outFrames + ((size_t)theFirst * inChannelCount), theData, (size_t)(theCount - theFirst) * inChannelCount * sizeof(float));
    }
    atomic_store_explicit(&inHeader->read_idx, theEffectiveRead + theCount, memory_order_release);
    return theCount;
}

// ---------------------------------------------------------------- messages

// Who sends what, and to which port:
//   Hello       daemon -> driver's registered service port, expects a reply
//   HelloReply  driver -> the send-once reply port carried by Hello
//   Control     driver -> the daemon port handed over in Hello.control_port
//   Notify      daemon -> driver's registered service port (it already has a
//               send right on it from bootstrap_look_up)
#define kAudioHubMsg_Hello       0x41480001 // daemon -> driver, expects a reply
#define kAudioHubMsg_HelloReply  0x41480002 // driver -> daemon, carries the two memory entries
#define kAudioHubMsg_Control     0x41480003 // driver -> daemon, fire and forget
#define kAudioHubMsg_Notify      0x41480004 // daemon -> driver, fire and forget

#define kAudioHubProtocolVersion 1u

// Control ops (driver -> daemon)
#define kAudioHubCtl_Volume      1u // volume/mute of a virtual device changed locally
#define kAudioHubCtl_Heartbeat   2u // liveness; a failed send is how the driver notices a dead daemon
#define kAudioHubCtl_IOState     3u // an app started/stopped IO on a virtual device
// Another client completed a Hello and has taken this session over. Sent to the
// OUTGOING daemon's control port as the last message on it, immediately before
// the driver deallocates that port; the send is best-effort (one bounded
// mach_msg, a wedged daemon simply misses it).
//   payload: device = kAudioHubDevice_Speaker, scalar_bits = 0, flags = 0,
//            seq = the usual monotonic control sequence.
//   the daemon MUST, on receipt: treat the session as already over — unmap both
//   rings, drop its send right on the driver port, and stop producing/consuming
//   at once. It must NOT count this as liveness and must not re-Hello on the
//   strength of it (an immediate reconnect just displaces the daemon that
//   displaced it, which is the oscillation this op exists to stop).
// WHY IT EXISTS: the entry ports are the same kernel objects across reconnects,
// so a superseded daemon's MAPPINGS stay valid — without this op it keeps
// draining the speaker ring and writing the mic ring until its own 5s silence
// timer fires, then reconnects and displaces the newcomer, forever. That is
// reachable with no attacker at all: an app-launched daemon plus one started
// from a shell, which this project supports on purpose.
// An older daemon that does not know this op ignores it (its control dispatch
// has a catch-all), so sending it is always safe.
#define kAudioHubCtl_Superseded  4u

// Notify ops (daemon -> driver)
#define kAudioHubNotify_Volume   1u // peer's real device reported a new volume: update the control's value
#define kAudioHubNotify_Ping     2u

// Device selectors, shared by both directions.
#define kAudioHubDevice_Speaker  0u
#define kAudioHubDevice_Mic      1u

// Reply status codes
#define kAudioHubStatus_OK              0u
#define kAudioHubStatus_BadVersion      1u
#define kAudioHubStatus_NoMemory        2u

// Handshake request, sent BY THE DAEMON to the driver's registered service port.
// msgh_local_port must be a send-once reply port. control_port hands the driver
// a send right on a port the daemon receives on, which is where every
// kAudioHubMsg_Control (volume, io state, heartbeat) goes; a failed send on it
// is how the driver notices the daemon died. Reconnecting is just another Hello:
// the driver drops the previous daemon, resets both rings and replies again.
typedef struct AudioHubHelloRequest
{
    mach_msg_header_t          header;
    mach_msg_body_t            body;
    mach_msg_port_descriptor_t control_port; // send right, daemon receives Control here
    uint32_t                   protocol_version;
    uint32_t                   client_pid;
} AudioHubHelloRequest;

// Reply, sent BY THE DRIVER. Complex with exactly two port descriptors when
// status == kAudioHubStatus_OK; on any other status it is a PLAIN message with
// msgh_descriptor_count 0 and both descriptors zeroed, so the daemon must test
// MACH_MSGH_BITS_COMPLEX and status before touching spk_entry/mic_entry.
typedef struct AudioHubHelloReply
{
    mach_msg_header_t          header;
    mach_msg_body_t            body;
    mach_msg_port_descriptor_t spk_entry; // driver writes, daemon reads
    mach_msg_port_descriptor_t mic_entry; // daemon writes, driver reads
    uint32_t                   status;
    uint32_t                   protocol_version;
    uint32_t                   data_offset;
    uint32_t                   spk_capacity_frames;
    uint32_t                   spk_channels;
    uint32_t                   spk_sample_rate;
    uint32_t                   mic_capacity_frames;
    uint32_t                   mic_channels;
    uint32_t                   mic_sample_rate;
    // nine u32 above land the u64 pair on offset 88 with no implicit padding;
    // the asserts below are what keeps that true after any edit
    uint64_t                   spk_bytes;
    uint64_t                   mic_bytes;
} AudioHubHelloReply;

// One shape for both fire-and-forget directions; msgh_id says which.
typedef struct AudioHubControlMsg
{
    mach_msg_header_t header;
    uint32_t          op;
    uint32_t          device;
    uint32_t          scalar_bits; // IEEE-754 f32 bits, 0..1 scalar
    uint32_t          flags;       // bit0 = muted, bit1 = io running
    uint64_t          seq;
} AudioHubControlMsg;

_Static_assert(sizeof(mach_msg_header_t) == 24, "mach header ABI drift");
_Static_assert(sizeof(mach_msg_port_descriptor_t) == 12, "port descriptor ABI drift");
_Static_assert(offsetof(AudioHubHelloRequest, protocol_version) == 40, "hello ABI drift");
_Static_assert(sizeof(AudioHubHelloRequest) == 48, "hello ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, status) == 52, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, spk_bytes) == 88, "hello reply ABI drift");
_Static_assert(sizeof(AudioHubHelloReply) == 104, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubControlMsg, seq) == 40, "control ABI drift");
_Static_assert(sizeof(AudioHubControlMsg) == 48, "control ABI drift");

#define kAudioHubFlag_Muted     0x1u
#define kAudioHubFlag_IORunning 0x2u

// ---------------------------------------------------------------- driver API
//
// Everything below is implemented in AudioHubBridge.c and called from
// AudioHubDriver.c. The two IO entry points are the ONLY ones an IOProc may
// call: they take no lock, allocate nothing, block on nothing and never touch
// mach. Everything expensive (bootstrap_check_in, mach_msg, ring allocation and
// mapping) happens on the private service thread started by
// AudioHubBridge_Start().

// Applied by the driver when the daemon reports the peer's real device volume.
// Runs on the bridge thread; must not call back into AudioHubBridge_PostVolume
// or the two sides ping-pong forever.
typedef void (*AudioHubBridge_NotifyProc)(uint32_t inDevice, float inScalar, int inMuted);

void AudioHubBridge_Start(AudioHubBridge_NotifyProc inNotifyProc);

// IOProc-safe. Speaker direction: frames an app played into "AudioHub Speaker".
void AudioHubBridge_WriteSpeaker(const float* inFrames, uint32_t inFrameCount, uint32_t inChannelCount);

// IOProc-safe. Microphone direction: fills outFrames, zero-padding whatever the
// daemon has not supplied (including "no daemon at all").
void AudioHubBridge_ReadMicrophone(float* outFrames, uint32_t inFrameCount, uint32_t inChannelCount);

// Non-blocking mailbox posts, safe from a property-set call.
void AudioHubBridge_PostVolume(uint32_t inDevice, float inScalar, int inMuted);
void AudioHubBridge_PostIOState(uint32_t inDevice, int inRunning);

#endif // AUDIOHUB_BRIDGE_H
