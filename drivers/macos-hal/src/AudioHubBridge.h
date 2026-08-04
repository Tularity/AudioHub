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
//   Bind        daemon -> the same service port, fire and forget: binds or
//               clears one slot's peer metadata (spec-m5b §4.2)
#define kAudioHubMsg_Hello       0x41480001 // daemon -> driver, expects a reply
#define kAudioHubMsg_HelloReply  0x41480002 // driver -> daemon, carries every memory entry
#define kAudioHubMsg_Control     0x41480003 // driver -> daemon, fire and forget
#define kAudioHubMsg_Notify      0x41480004 // daemon -> driver, fire and forget
#define kAudioHubMsg_Bind        0x41480005 // daemon -> driver, fire and forget

// v2: per-peer virtual devices (spec-m5b §4). Compared for EQUALITY by the
// driver, in both directions, and a mismatch is a refusal with zero descriptors
// — never a partial mix. A v2 driver publishes NO devices until a daemon binds
// a slot, so version skew presents as "zero AudioHub devices in the system"
// plus a named reason in the daemon, which is a loud failure a user can act on.
// A v1-shaped negotiation cannot be reconstructed from a v2 message: the reply
// grew 104 -> 472 bytes and the control message 48 -> 56, so a compatibility
// shim would have to guess which layout it is holding. There is deliberately
// none.
#define kAudioHubProtocolVersion 2u

// One (spk, mic) ring pair per slot, all created up front and never released;
// binding a peer to a slot is a metadata operation, so the realtime path never
// has to reason about a ring that might be going away (spec-m5b §1).
#define kAudioHubMaxSlots        16u
#define kAudioHubMaxEndpoints    (2u * kAudioHubMaxSlots)

// ENDPOINT ENCODING. What used to be a 1-bit device selector is now
// `slot * 2 + dir`, so one u32 names any of the 32 virtual devices. The
// direction bit stays the low bit precisely so the old fixed pair keeps its
// numbering: slot 0 out == 0, slot 0 in == 1, which is what v1's
// kAudioHubDevice_Speaker / _Mic were. Both of those names are gone: leaving
// them would let a caller pass a plain "1" meaning "the microphone" and have it
// silently addressed to slot 0 of a sixteen-slot pool.
#define kAudioHubDir_Out         0u // driver writes, daemon reads (virtual speaker)
#define kAudioHubDir_In          1u // daemon writes, driver reads (virtual microphone)
#define AUDIOHUB_ENDPOINT(slot, dir) (((slot) * 2u) + (dir))
#define AUDIOHUB_ENDPOINT_SLOT(ep)   ((ep) / 2u)
#define AUDIOHUB_ENDPOINT_DIR(ep)    ((ep) & 1u)

// Control ops (driver -> daemon)
#define kAudioHubCtl_Volume      1u // volume/mute of a virtual device changed locally
#define kAudioHubCtl_Heartbeat   2u // liveness; a failed send is how the driver notices a dead daemon
#define kAudioHubCtl_IOState     3u // an app started/stopped IO on a virtual device
// Another client completed a Hello and has taken this session over. Sent to the
// OUTGOING daemon's control port as the last message on it, immediately before
// the driver deallocates that port; the send is best-effort (one bounded
// mach_msg, a wedged daemon simply misses it).
//   payload: endpoint = 0, generation = 0 (it concerns no single slot),
//            scalar_bits = 0, flags = 0, seq = the usual monotonic sequence.
//   the daemon MUST, on receipt: treat the session as already over — unmap all
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
// The driver's account of one slot: endpoint = slot*2, scalar_bits carries a
// kAudioHubSlot_* state, generation carries that slot's current stamp. It is
// what establishes the generation the daemon then filters every other control
// message against, and what makes publication CLOSED-LOOP: "I sent a Bind and
// mach returned OK" is not evidence a device exists (spec-m5b §4.6).
#define kAudioHubCtl_BindState   5u
// The driver's answer to kAudioHubNotify_Latency: what this endpoint's
// kAudioDevicePropertyLatency ACTUALLY returns right now.
//   payload: endpoint = the one the notify named, generation = that slot's
//            stamp, scalar_bits = the EFFECTIVE frame count (a plain UInt32,
//            NOT float bits -- see kAudioHubNotify_Latency),
//            flags: bit3 (kAudioHubFlag_LatencyPending) set when a newer value
//            has been received but cannot take effect until IO stops.
//
// WHY AN ACK RATHER THAN A PROTOCOL BUMP. A driver that predates this op
// ignores the notify (bridge_handle_notify only dispatches ops it knows) and
// keeps declaring 0 -- silently, which is the shape this project keeps getting
// burned by. The obvious answer, bumping kAudioHubProtocolVersion, is far too
// blunt: a v2 driver refuses a v3 daemon outright, so every AudioHub device on
// the machine disappears until the user reinstalls the plug-in with sudo and
// restarts coreaudiod. That is a heavy, audible break in exchange for a purely
// ADDITIVE capability whose absence costs exactly what today already costs.
// So the loop is closed the same way binding is: the daemon compares what it
// asked for against what came back, re-sends while they differ, and reports
// "this driver never acknowledged a latency" after a bounded number of tries.
// "The send returned KERN_SUCCESS" is not evidence the property changed.
#define kAudioHubCtl_LatencyState 6u

// Slot states carried in kAudioHubCtl_BindState's scalar_bits.
#define kAudioHubSlot_Free       0u
#define kAudioHubSlot_Bound      1u
#define kAudioHubSlot_Delisted   2u // off the device list, still answering the HAL

// Notify ops (daemon -> driver)
#define kAudioHubNotify_Volume   1u // peer's real device reported a new volume: update the control's value
#define kAudioHubNotify_Ping     2u
// How many frames of latency this endpoint must declare to CoreAudio, i.e.
// how long after an application hands us a frame it is audible on the PEER's
// speakers. Carried in scalar_bits as a PLAIN UInt32 -- that field is already
// polymorphic (a kAudioHubSlot_* state in BindState), and reinterpreting a
// frame count as float bits would make 7200 arrive as 1.0e-41.
//
// The daemon derives it from the same `sum_ms` the UI shows, so the number the
// system is told and the number the user reads are the same measurement.
// The driver applies it to kAudioDevicePropertyLatency ONLY, never to
// kAudioDevicePropertySafetyOffset (that one participates in IO scheduling),
// and only while that endpoint's IO is stopped -- see the long comment at
// `case kAudioDevicePropertyLatency` in AudioHubDriver.c.
#define kAudioHubNotify_Latency  3u

// Bind ops (daemon -> driver), in AudioHubBindMsg.op
#define kAudioHubBind_Clear      0u // retire the slot; generation must match the current one
#define kAudioHubBind_Set        1u // bind (or idempotently re-bind) the slot to a peer

// Reply status codes
#define kAudioHubStatus_OK              0u
#define kAudioHubStatus_BadVersion      1u
#define kAudioHubStatus_NoMemory        2u
// A Bind that names a session the driver has since replaced. Whoever sent it
// was superseded and does not know yet; acting on it would let a departed
// daemon's queued Binds retire a live daemon's slots.
#define kAudioHubStatus_StaleSession    3u
#define kAudioHubStatus_BadRequest      4u

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

// Reply, sent BY THE DRIVER. Complex with exactly kAudioHubMaxEndpoints port
// descriptors when status == kAudioHubStatus_OK; on any other status it is a
// PLAIN message with msgh_descriptor_count 0 and the whole array zeroed, so the
// daemon must test MACH_MSGH_BITS_COMPLEX and status before touching entries[].
//
// ONE reply carries all 32 entries rather than 16 follow-up RPCs, and that is
// measured rather than assumed: core/audiohubd/src/halbridge.rs's
// `a_single_reply_can_carry_thirty_two_memory_entries` sends this exact shape
// through the kernel, maps every entry, and proves the 32 objects stay 32
// distinct objects; `descriptor_counts_well_past_thirty_two_still_fit_one_message`
// shows 128 works too, so 32 is nowhere near a ceiling.
//
// GEOMETRY IS ONE SET OF SCALARS FOR ALL SLOTS. Every out ring is 48k/2ch and
// every in ring 48k/1ch (risk 8: capability mirroring would need another
// version bump), so the reply describes them once instead of 32 times. What is
// per-slot is only the entry port.
typedef struct AudioHubHelloReply
{
    mach_msg_header_t          header;
    mach_msg_body_t            body;
    // entries[2*s]   = slot s's out ring (driver writes, daemon reads)
    // entries[2*s+1] = slot s's in ring  (daemon writes, driver reads)
    // i.e. indexed by the same endpoint number the control plane uses.
    mach_msg_port_descriptor_t entries[kAudioHubMaxEndpoints];
    uint32_t                   status;
    uint32_t                   protocol_version;
    uint32_t                   slot_count; // how many of entries[] are populated, as PAIRS
    uint32_t                   data_offset;
    uint32_t                   spk_capacity_frames;
    uint32_t                   spk_channels;
    uint32_t                   mic_capacity_frames;
    uint32_t                   mic_channels;
    uint32_t                   sample_rate; // shared by every ring in both directions
    // The descriptor array ends at 412, which is 4 mod 8 — so an ODD number of
    // u32 has to follow before the first u64 or the compiler inserts four bytes
    // of padding that a hand-written wire layout does not know about, and the
    // two ends read the ring geometry four bytes apart with neither side's
    // build failing. Nine of them, and the offset asserts below are what keeps
    // that true after any edit.
    uint64_t                   session_id; // bumped on every accepted Hello
    uint64_t                   spk_bytes;
    uint64_t                   mic_bytes;
} AudioHubHelloReply;

// One shape for both fire-and-forget directions; msgh_id says which.
typedef struct AudioHubControlMsg
{
    mach_msg_header_t header;
    uint32_t          op;
    uint32_t          endpoint;    // slot*2 + dir; ops that concern no slot send 0
    uint32_t          scalar_bits; // IEEE-754 f32 bits, 0..1 scalar; a kAudioHubSlot_* state in BindState
    uint32_t          flags;       // bit0 = muted, bit1 = io running, bit2 = endpoint is an input
    // The slot's stamp at the moment this message was produced. 0 means "no
    // slot" (Heartbeat / Superseded / Ping). The receiver drops anything whose
    // stamp is not the one it currently holds for that slot, which is what
    // stops a late StopIO from lighting up the NEXT peer's microphone
    // indicator after the slot has been reused (spec-m5b §4.6).
    uint32_t          generation;
    uint32_t          reserved; // MBZ
    uint64_t          seq;
} AudioHubControlMsg;

// daemon -> driver. Binds one slot to a peer, or retires it. Fire and forget:
// the driver's answer is a kAudioHubCtl_BindState on the control port, so the
// daemon learns the outcome (and the new generation) from the same closed loop
// that reports every other slot transition, not from mach's send status.
//
// The strings are FIXED-SIZE and the receiver terminates them itself — it never
// trusts the sender's terminator. They are sized to the longest thing each can
// legitimately hold: peer_key for a fingerprint, the uids for "AudioHub:<fp>:out",
// and the names for a peer's computer name plus a disambiguating suffix.
typedef struct AudioHubBindMsg
{
    mach_msg_header_t header;
    uint32_t          op;         // kAudioHubBind_Set | kAudioHubBind_Clear
    uint32_t          slot;       // 0..slot_count-1
    uint32_t          flags;      // bit0 = peer is online (logging only)
    uint32_t          generation; // Clear: the stamp the daemon believes is current. Set: 0, the driver assigns
    uint64_t          session_id; // the HelloReply's; a mismatch is kAudioHubStatus_StaleSession
    char              peer_key[40];
    char              out_uid[64];
    char              in_uid[64];
    char              out_name[128];
    char              in_name[128];
} AudioHubBindMsg;

_Static_assert(sizeof(mach_msg_header_t) == 24, "mach header ABI drift");
_Static_assert(sizeof(mach_msg_body_t) == 4, "mach body ABI drift");
_Static_assert(sizeof(mach_msg_port_descriptor_t) == 12, "port descriptor ABI drift");
_Static_assert(offsetof(AudioHubHelloRequest, control_port) == 28, "hello ABI drift");
_Static_assert(offsetof(AudioHubHelloRequest, protocol_version) == 40, "hello ABI drift");
_Static_assert(sizeof(AudioHubHelloRequest) == 48, "hello ABI drift");

// The v2 wire sizes below are literals on purpose: they are what the daemon's
// receive buffer, its own mirrored structs and test/tests/halwire.rs are all
// sized against, so a slot-count edit must be a deliberate, visible change to
// every one of them rather than something that silently reshapes the message.
_Static_assert(kAudioHubMaxSlots == 16u, "the frozen v2 message sizes below assume 16 slots");
_Static_assert(offsetof(AudioHubHelloReply, entries) == 28, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, status) == 412, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, session_id) == 448, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, spk_bytes) == 456, "hello reply ABI drift");
_Static_assert(offsetof(AudioHubHelloReply, mic_bytes) == 464, "hello reply ABI drift");
_Static_assert(sizeof(AudioHubHelloReply) == 472, "hello reply ABI drift");

_Static_assert(offsetof(AudioHubControlMsg, endpoint) == 28, "control ABI drift");
_Static_assert(offsetof(AudioHubControlMsg, generation) == 40, "control ABI drift");
_Static_assert(offsetof(AudioHubControlMsg, seq) == 48, "control ABI drift");
_Static_assert(sizeof(AudioHubControlMsg) == 56, "control ABI drift");

_Static_assert(offsetof(AudioHubBindMsg, session_id) == 40, "bind ABI drift");
_Static_assert(offsetof(AudioHubBindMsg, peer_key) == 48, "bind ABI drift");
_Static_assert(offsetof(AudioHubBindMsg, out_uid) == 88, "bind ABI drift");
_Static_assert(offsetof(AudioHubBindMsg, in_uid) == 152, "bind ABI drift");
_Static_assert(offsetof(AudioHubBindMsg, out_name) == 216, "bind ABI drift");
_Static_assert(offsetof(AudioHubBindMsg, in_name) == 344, "bind ABI drift");
_Static_assert(sizeof(AudioHubBindMsg) == 472, "bind ABI drift");

#define kAudioHubFlag_Muted     0x1u
#define kAudioHubFlag_IORunning 0x2u
#define kAudioHubFlag_IsInput   0x4u
// kAudioHubCtl_LatencyState only: the driver holds a value it has not been able
// to install yet because an application is doing IO on that endpoint. The
// declared latency is fixed for the life of an open stream ON PURPOSE (VLC and
// Chromium both read the property exactly once, at open, and neither installs a
// listener; changing it mid-stream would force the host to stop and restart IO
// for a value nobody re-reads). This bit is how the daemon can SAY "sent, takes
// effect when the app releases the device" instead of looking stuck.
#define kAudioHubFlag_LatencyPending 0x8u

// ---------------------------------------------------------------- driver API
//
// Everything below is implemented in AudioHubBridge.c and called from
// AudioHubDriver.c. The two IO entry points are the ONLY ones an IOProc may
// call: they take no lock, allocate nothing, block on nothing and never touch
// mach. Everything expensive (bootstrap_check_in, mach_msg, ring allocation and
// mapping) happens on the private service thread started by
// AudioHubBridge_Start().
//
// WHERE THE LINE IS. This file is transport — the mach service, the rings, the
// handshake, the control sends. AudioHubDriver.c owns the slot pool, the device
// records and everything coreaudiod can see. Nothing here includes CoreAudio and
// nothing there touches mach, which is not tidiness for its own sake: it is what
// lets the slot state machine (bind, two-phase retirement, object-ID allocation)
// be exercised in a plain test binary with these entry points stubbed out, on a
// machine where installing a HAL plug-in costs sudo and every app's audio.

// One (spk, mic) ring pair per slot, all created before the first Hello can be
// answered and never released (spec-m5b §1) — so an IOProc can never race an
// unmap and the realtime path needs no reclamation scheme at all.
//
// OPAQUE ON PURPOSE. The driver stores the pointer its device was bound to and
// hands it straight back, so "which device feeds which ring" is DATA in the
// device record rather than a function of the device's identity. A helper that
// mapped a device onto a ring instead is the exact shape in which sixteen
// devices silently share one ring while every positive test still passes.
typedef struct BridgeRing BridgeRing;

// The ring backing one endpoint (slot*2 + dir), constant for the life of the
// process. NULL only before the service thread has built them, which is strictly
// before any Bind can be dispatched, so a bind handler never sees NULL.
BridgeRing* AudioHubBridge_RingForEndpoint(uint32_t inEndpoint);

// IOProc-safe. Out direction: frames an app played into a peer's virtual
// speaker. A ring nobody is attached to discards them (plan §7.3).
void AudioHubBridge_WriteRing(BridgeRing* inRing, const float* inFrames, uint32_t inFrameCount, uint32_t inChannelCount);

// IOProc-safe. In direction: fills outFrames, zero-padding whatever the daemon
// has not supplied (including "no daemon at all").
void AudioHubBridge_ReadRing(BridgeRing* inRing, float* outFrames, uint32_t inFrameCount, uint32_t inChannelCount);

// Service thread only. Unpublish returns with no IOProc inside the ring, so the
// three below it are safe to call immediately afterwards.
void AudioHubBridge_PublishRing(BridgeRing* inRing);
void AudioHubBridge_UnpublishRing(BridgeRing* inRing);
// Re-stamps the header AND zeroes both indices: only legal on a ring that is
// unpublished AND that no daemon is looking at, i.e. slot retirement and slot
// binding. Zeroing write_idx under a live reader makes its next read compute a
// 2^64 backlog, clamp to capacity and replay 500ms of the previous peer's audio
// (spec-m5b §4.6) — the reason this is not called on disconnect any more.
void AudioHubBridge_ResetRing(BridgeRing* inRing);
// Consumer-side only (the driver consumes the IN rings): drops whatever is
// queued by snapping read_idx up to write_idx. Used when a new daemon attaches,
// so the first frames a virtual microphone renders are not the previous
// session's. Unpublished rings only.
void AudioHubBridge_FlushRingConsumer(BridgeRing* inRing);

// Non-zero while a daemon is attached. Only the service thread may ask: it is
// the only thread that can act on the answer without it going stale.
int AudioHubBridge_SessionActive(void);

// What one control send did. "Retry" is a full queue — the message was NOT
// delivered, so the caller must leave its sent marker alone and try again;
// "Dead" means the session is over and the caller should stop sending. The
// distinction is not cosmetic: collapsing Retry into OK silently threw away
// volume changes, and collapsing it into Dead disconnected a daemon that was
// merely busy.
#define kAudioHubSend_OK    0u
#define kAudioHubSend_Retry 1u
#define kAudioHubSend_Dead  2u
uint32_t AudioHubBridge_SendControl(uint32_t inOp, uint32_t inEndpoint, uint32_t inGeneration, uint64_t inWord);

// Everything the transport calls back into the driver for. All of these run on
// the bridge service thread, one at a time, and none of them may block: this is
// the thread that also answers the heartbeat, and a daemon that stops hearing it
// for five seconds re-Hellos and re-binds every slot.
typedef struct AudioHubBridgeHooks
{
    // A daemon completed a handshake and holds every entry port. The driver
    // republishes the rings of the slots it still has bound (bindings survive a
    // daemon restart — spec-m5b §5.7) and drops anything stale in the IN rings.
    void (*attached)(void);
    // The daemon went away or was superseded. Every ring is already unpublished;
    // the bindings stay, so the devices remain listed and simply run silent.
    void (*detached)(void);
    // One Bind, already checked for shape, sender identity and session id. The
    // driver validates slot number and strings, since those need CoreFoundation.
    void (*bind)(const AudioHubBindMsg* inMsg);
    // The peer's real device reported a new volume. Must NOT post a volume back
    // or the two sides ping-pong forever.
    void (*notify_volume)(uint32_t inEndpoint, uint32_t inGeneration, float inScalar, int inMuted);
    // How many frames this endpoint should declare as kAudioDevicePropertyLatency.
    // Unlike notify_volume this one MUST answer, with kAudioHubCtl_LatencyState:
    // the daemon has no other way to learn whether the property actually moved,
    // and a silent no-op is precisely what this whole mechanism exists to end.
    void (*notify_latency)(uint32_t inEndpoint, uint32_t inGeneration, uint32_t inFrames);
    // Drain the per-device outboxes. Returns non-zero while the daemon is alive.
    int (*flush)(void);
    // End of one service pass: retire whatever is due and announce a device-list
    // change at most once, however many binds the pass carried.
    void (*tick)(void);
    // Bit `endpoint` set while that endpoint has IO running. Debug census only
    // (kBridgeRingLogEnv), so it may answer from cached state.
    uint32_t (*io_running_mask)(void);
} AudioHubBridgeHooks;

// inHooks must outlive the process (a static in the driver); it is read from the
// service thread without further synchronisation, which is sound because it is
// installed before the thread exists.
void AudioHubBridge_Start(const AudioHubBridgeHooks* inHooks);

#endif // AUDIOHUB_BRIDGE_H
