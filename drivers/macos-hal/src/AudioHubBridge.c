//  AudioHubBridge.c — daemon transport for the AudioHub HAL plug-in
//  (spec-round2 §B1/§B2). See AudioHubBridge.h for the frozen wire contract and
//  for WHY the plug-in is the one that registers a mach name.
//
//  THE IOProc RULE. AudioHubBridge_WriteSpeaker / AudioHubBridge_ReadMicrophone
//  are called from coreaudiod's realtime IO thread. Inside them there is no
//  lock, no allocation, no syscall, no mach traffic and no unbounded loop —
//  only atomic loads/stores and a memcpy into an already-mapped page. Every
//  expensive thing (bootstrap_check_in, mach_msg, ring allocation, mapping)
//  runs on the private service thread below. A daemon that is absent, dead or
//  merely slow therefore costs exactly one atomic load: the device stays alive
//  and plays/records silence (plan §7.3), it never stalls the IO cycle.
//
//  The plug-in OWNS the two rings for its whole lifetime — they are created once
//  and never unmapped, so an IOProc can never race an unmap. What a connect and
//  a disconnect flip is only the per-ring `valid` publish flag, plus a reset of
//  the indices while no IOProc holds the ring:
//    IOProc:  inuse++ (seq_cst) ; if (valid) use hdr ; inuse--
//    service: valid=0 (seq_cst) ; wait until inuse==0 ; reset indices
//  which is Dekker's pattern — hence seq_cst on exactly those two pairs. The
//  waiting side is the service thread, which is allowed to block. Keeping the
//  rings unpublished while no daemon is attached is also what stops the speaker
//  ring from filling with 500ms of stale audio that a reconnecting daemon would
//  otherwise play out.

#include "AudioHubBridge.h"

#include <bsm/libbsm.h> // audit_token_to_euid / _to_pid: the sender's identity, from the kernel
#include <mach/mach_vm.h>
#include <mach/memory_object_types.h> // MAP_MEM_NAMED_CREATE
#include <mach/vm_map.h>              // mach_make_memory_entry_64
#include <os/log.h>
#include <pthread.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
#include <servers/bootstrap.h>
#pragma clang diagnostic pop

#define kBridgeCheckInRetryUsec  (2 * 1000 * 1000) // between failed bootstrap_check_in attempts
#define kBridgeCheckInMaxTries   30                // ~60s; see bridge_thread for why this is bounded
#define kBridgeRecvTimeoutMsec   200               // also paces the service loop
#define kBridgeHeartbeatMsec     1000
#define kBridgeSendTimeoutMsec   500
#define kBridgeStatIntervalMsec  5000
#define kBridgeErrorBackoffUsec  (50 * 1000) // after a receive error, so a hostile queue cannot spin us
#define kBridgeDrainPerPass      32          // messages consumed per bridge_serve() call
#define kBridgeMaxSendTimeouts   3           // consecutive full-queue sends before we call the daemon dead

// Env var (not an Info.plist key: editing the plist of an installed bundle
// breaks its code signature, which coreaudiod is entitled to care about).
// Enable with `sudo launchctl setenv AUDIOHUB_HAL_RING_LOG 1` + a coreaudiod
// restart; read once on the service thread.
#define kBridgeRingLogEnv "AUDIOHUB_HAL_RING_LOG"

#define kBridgeLog "[audiohub-hal] "

// ---------------------------------------------------------------- peer policy
//
// WHO IS ALLOWED TO COMPLETE A HELLO. The service name is registered in the
// GLOBAL bootstrap namespace (it has to be — see the header), so
// bootstrap_look_up("com.audiohub.driver") succeeds from ANY local process:
// verified from an unsigned, unentitled uid-501 binary, kr = 0. A caller that
// gets a Hello accepted receives both ring entries, i.e. it can read everything
// played to "AudioHub Speaker" (in mode B that is the user's default output —
// all system audio) and inject into "AudioHub Microphone", with no TCC prompt
// at all. That bypasses exactly the consent a Core Audio tap or ScreenCaptureKit
// would have had to obtain, so the sender must be authenticated, and from the
// KERNEL's account of it: client_pid in the Hello is attacker-supplied and is
// only ever logged.
//
// STRICTNESS. kAudioHubPeerCheck_SessionOwner is what is enforceable today:
// the audit token's euid must own the console session. It stops every other
// user and every system service; it does NOT stop another process of the same
// user, which is the remaining half of the hole.
// Closing that half needs a code-signing requirement, and none can be stated
// today that the project's own daemon satisfies:
//   - drivers/macos-hal/build.sh signs the plug-in ad-hoc, and cargo/ld sign
//     audiohubd ad-hoc too, so its designated requirement is a per-BUILD
//     cdhash (measured: `cdhash H"1159a8ca…"`, flags 0x20002 adhoc+linker-signed).
//   - scripts/sign-dev.sh can pin it to a self-signed "AudioHub Dev" leaf, but
//     that identity is CSSMERR_TP_NOT_TRUSTED on this machine and signing is
//     an opt-in dev step, so requiring it would lock the normal dev build out
//     of its own driver.
//   - an identifier-only requirement is forgeable by any caller in one
//     codesign invocation, so it would buy nothing.
// Once a real Developer ID exists (M7, with notarisation), raise the level to
// kAudioHubPeerCheck_Requirement and implement it as SecCodeCreateWithAuditToken
// on the trailer's token + SecCodeCheckValidity against kAudioHubClientRequirement
// — the audit token is the only sound input for that call, which is the other
// reason the trailer is requested below. Note when doing so that this code runs
// inside the sandboxed Core Audio driver host: whether it may read the client's
// executable to validate it on disk has to be measured, not assumed.
#define kAudioHubPeerCheck_SessionOwner 1
#define kAudioHubPeerCheck_Requirement  2
#define kAudioHubPeerCheckLevel         kAudioHubPeerCheck_SessionOwner
#define kAudioHubClientRequirement \
    "identifier \"com.audiohub.daemon\" and anchor apple generic and " \
    "certificate leaf[subject.OU] = \"<TEAMID>\""

#if kAudioHubPeerCheckLevel >= kAudioHubPeerCheck_Requirement
#error "kAudioHubPeerCheck_Requirement has no implementation yet; see the comment above"
#endif

// macOS reserves everything below 500 for system and service accounts; a client
// of this driver is by construction a user-session process (the header explains
// why a system-domain daemon was abandoned), so anything lower is not ours.
#define kBridgeFirstUserUID 500u

// ---------------------------------------------------------------- rings

typedef struct BridgeRing
{
    _Atomic(uint32_t)   inuse;    // IOProc calls in flight
    _Atomic(uint32_t)   valid;    // publish flag; hdr is only usable while set
    AudioHubRingHeader* hdr;      // set once at creation, never changes afterwards
    mach_vm_address_t   addr;
    mach_vm_size_t      size;
    mach_port_t         entry;    // memory entry port we own and hand out per Hello
    // Geometry, written once at creation and never again. Every bound the
    // IOProc path indexes with comes from HERE and not from the shared header,
    // which the peer maps read/write — see AudioHubRing_Write in the header.
    uint32_t            channels;
    uint32_t            capacityFrames;
    uint32_t            dataOffset;
} BridgeRing;

static BridgeRing gSpkRing;
static BridgeRing gMicRing;

static inline BridgeRing* bridge_acquire(BridgeRing* inRing)
{
    atomic_fetch_add(&inRing->inuse, 1); // seq_cst: pairs with the unpublish wait
    if(atomic_load(&inRing->valid) == 0)
    {
        atomic_fetch_sub_explicit(&inRing->inuse, 1, memory_order_release);
        return NULL;
    }
    return inRing;
}

static inline void bridge_release(BridgeRing* inRing)
{
    atomic_fetch_sub_explicit(&inRing->inuse, 1, memory_order_release);
}

// Service thread only. Returns with no IOProc inside the ring.
static void bridge_ring_unpublish(BridgeRing* inRing)
{
    if(atomic_load(&inRing->valid) == 0)
    {
        return;
    }
    atomic_store(&inRing->valid, 0); // seq_cst: pairs with the IOProc's inuse++
    while(atomic_load(&inRing->inuse) != 0)
    {
        usleep(200); // service thread only; an IOProc holds this for a memcpy
    }
}

// Service thread only, and only while the ring is unpublished: the IOProc is the
// producer of one index and the consumer of the other, so this would be a data
// race if anything could still be inside AudioHubRing_Write/Read.
//
// The identifying fields are RE-STAMPED, not just the indices. Nothing in this
// process ever reads them any more (the IO path takes its geometry from
// BridgeRing), but the peer maps this page read/write and the daemon
// cross-checks magic/version at attach time — so one crashed or malicious
// client that scribbles on the header would otherwise poison it permanently
// and make every future handshake fail that cross-check, with no way back short
// of restarting coreaudiod. A disconnect is the one moment we know no IOProc
// and no peer is looking, so it is where the header goes back to what we
// promised in the Hello reply.
static void bridge_ring_reset(BridgeRing* inRing)
{
    if(inRing->hdr == NULL)
    {
        return;
    }
    inRing->hdr->magic           = AUDIOHUB_RING_MAGIC;
    inRing->hdr->version         = AUDIOHUB_RING_VERSION;
    inRing->hdr->sample_rate     = AUDIOHUB_SAMPLE_RATE;
    inRing->hdr->channels        = inRing->channels;
    inRing->hdr->capacity_frames = inRing->capacityFrames;
    inRing->hdr->reserved        = 0;
    atomic_store_explicit(&inRing->hdr->write_idx, 0, memory_order_relaxed);
    atomic_store_explicit(&inRing->hdr->read_idx, 0, memory_order_release);
}

static void bridge_ring_publish(BridgeRing* inRing)
{
    if(inRing->hdr != NULL)
    {
        atomic_store(&inRing->valid, 1);
    }
}

// Creates one shared ring: a named VM object, our own mapping of it, and the
// entry port we hand to the daemon.
//
// MAP_MEM_NAMED_CREATE is the whole point. Naming an EXISTING mach_vm_allocate()d
// range instead (no flag) yields an entry whose mappings are copy-on-write, so
// the two tasks silently diverge after the first store — the handshake succeeds,
// both sides read a valid header, and the consumer's frame counter never moves.
// mach_vm_map's `copy` argument below must stay FALSE for the same reason.
static int bridge_ring_create(BridgeRing* inRing, uint32_t inChannels, mach_vm_size_t inBytes)
{
    memory_object_size_t theSize = inBytes;
    mach_port_t theEntry = MACH_PORT_NULL;
    kern_return_t theResult = mach_make_memory_entry_64(mach_task_self(),
                                                        &theSize,
                                                        0,
                                                        MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE,
                                                        &theEntry,
                                                        MACH_PORT_NULL);
    if((theResult != KERN_SUCCESS) || (theEntry == MACH_PORT_NULL) || (theSize < inBytes))
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "mach_make_memory_entry_64(%llu) failed (%d)",
                     (unsigned long long)inBytes, theResult);
        if(theEntry != MACH_PORT_NULL)
        {
            mach_port_deallocate(mach_task_self(), theEntry);
        }
        return 0;
    }

    mach_vm_address_t theAddr = 0;
    theResult = mach_vm_map(mach_task_self(),
                            &theAddr,
                            inBytes,
                            0,
                            VM_FLAGS_ANYWHERE,
                            theEntry,
                            0,
                            FALSE, // NOT a copy: this mapping must be the shared object itself
                            VM_PROT_READ | VM_PROT_WRITE,
                            VM_PROT_READ | VM_PROT_WRITE,
                            VM_INHERIT_NONE);
    if(theResult != KERN_SUCCESS)
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "mapping our own ring failed (%d)", theResult);
        mach_port_deallocate(mach_task_self(), theEntry);
        return 0;
    }

    // A MAP_MEM_NAMED_CREATE object is zero-filled on demand, so only the
    // identifying fields need writing; the indices legitimately start at 0.
    AudioHubRingHeader* theHeader = (AudioHubRingHeader*)theAddr;
    theHeader->magic           = AUDIOHUB_RING_MAGIC;
    theHeader->version         = AUDIOHUB_RING_VERSION;
    theHeader->sample_rate     = AUDIOHUB_SAMPLE_RATE;
    theHeader->channels        = inChannels;
    theHeader->capacity_frames = AUDIOHUB_RING_FRAMES;
    theHeader->reserved        = 0;
    atomic_store(&theHeader->write_idx, 0);
    atomic_store(&theHeader->read_idx, 0);

    inRing->hdr            = theHeader;
    inRing->addr           = theAddr;
    inRing->size           = inBytes;
    inRing->entry          = theEntry;
    inRing->channels       = inChannels;
    inRing->capacityFrames = AUDIOHUB_RING_FRAMES;
    inRing->dataOffset     = AUDIOHUB_RING_DATA_OFFSET;
    return 1;
}

// ---------------------------------------------------------------- IOProc path

// The device's channel count is checked against the RING's, not against the
// header's: the two used to be compared inside AudioHubRing_Write, where the
// value being matched came out of memory the peer can write. Disagreement here
// means the caller and the ring were built for different stream formats, which
// is a driver bug, so the frames are dropped rather than reinterpreted.
void AudioHubBridge_WriteSpeaker(const float* inFrames, uint32_t inFrameCount, uint32_t inChannelCount)
{
    BridgeRing* theRing = bridge_acquire(&gSpkRing);
    if(theRing == NULL)
    {
        return; // no daemon: the frames are simply discarded (plan §7.3)
    }
    if(theRing->channels == inChannelCount)
    {
        AudioHubRing_Write(theRing->hdr, theRing->dataOffset, theRing->capacityFrames, theRing->channels,
                           inFrames, inFrameCount);
    }
    bridge_release(theRing);
}

void AudioHubBridge_ReadMicrophone(float* outFrames, uint32_t inFrameCount, uint32_t inChannelCount)
{
    const size_t theTotal = (size_t)inFrameCount * inChannelCount;
    uint32_t theGot = 0;
    BridgeRing* theRing = bridge_acquire(&gMicRing);
    if(theRing != NULL)
    {
        if(theRing->channels == inChannelCount)
        {
            theGot = AudioHubRing_Read(theRing->hdr, theRing->dataOffset, theRing->capacityFrames, theRing->channels,
                                       outFrames, inFrameCount);
        }
        bridge_release(theRing);
    }
    if(theGot < inFrameCount)
    {
        // underrun or no daemon at all: silence, never a stall
        const size_t theOffset = (size_t)theGot * inChannelCount;
        memset(outFrames + theOffset, 0, (theTotal - theOffset) * sizeof(float));
    }
}

// ---------------------------------------------------------------- outbox
//
// Control-plane posts come from coreaudiod's property dispatch, which must not
// block on a dead daemon either. Each slot is one 64-bit word so a reader can
// never see a scalar from one post paired with the mute flag of another; the
// sequence counter tells the service thread there is something new. A post that
// races the send is simply resent on the next pass.

#define kOutboxSlots 2 // indexed by kAudioHubDevice_*

static _Atomic(uint64_t) gVolWord[kOutboxSlots];
static _Atomic(uint64_t) gVolSeq[kOutboxSlots];
static uint64_t          gVolSent[kOutboxSlots];
static _Atomic(uint64_t) gIOWord[kOutboxSlots];
static _Atomic(uint64_t) gIOSeq[kOutboxSlots];
static uint64_t          gIOSent[kOutboxSlots];

static inline uint64_t bridge_pack(float inScalar, uint32_t inFlags)
{
    uint32_t theBits;
    memcpy(&theBits, &inScalar, sizeof(theBits));
    return ((uint64_t)inFlags << 32) | (uint64_t)theBits;
}

void AudioHubBridge_PostVolume(uint32_t inDevice, float inScalar, int inMuted)
{
    if(inDevice >= kOutboxSlots)
    {
        return;
    }
    atomic_store(&gVolWord[inDevice], bridge_pack(inScalar, inMuted ? kAudioHubFlag_Muted : 0u));
    atomic_fetch_add(&gVolSeq[inDevice], 1);
}

void AudioHubBridge_PostIOState(uint32_t inDevice, int inRunning)
{
    if(inDevice >= kOutboxSlots)
    {
        return;
    }
    atomic_store(&gIOWord[inDevice], bridge_pack(0.0f, inRunning ? kAudioHubFlag_IORunning : 0u));
    atomic_fetch_add(&gIOSeq[inDevice], 1);
}

// ---------------------------------------------------------------- service thread

static pthread_once_t            gBridgeOnce = PTHREAD_ONCE_INIT;
static AudioHubBridge_NotifyProc gNotifyProc = NULL;
static mach_port_t               gServicePort = MACH_PORT_NULL; // our receive right, from bootstrap_check_in
static mach_port_t               gDaemonPort  = MACH_PORT_NULL; // send right handed over by Hello
static uint64_t                  gControlSeq  = 0;

// Big enough for every inbound message plus the largest trailer the kernel can
// append. Anything larger is DESTROYED by the kernel and reported as
// MACH_RCV_TOO_LARGE — which is the reason MACH_RCV_LARGE is deliberately not
// requested here: with it the oversized message would stay queued and the port
// would wedge permanently. Discarding it is right, but it is not free: the
// error returns instantly, so a sender that keeps the queue stocked with
// oversized messages would spin this thread at 100% CPU inside coreaudiod.
// bridge_serve therefore backs off on any receive result that is not success
// or MACH_RCV_TIMED_OUT.
typedef union BridgeRcvBuf
{
    mach_msg_header_t    hdr;
    AudioHubHelloRequest hello;
    AudioHubControlMsg   ctl;
    uint8_t              raw[512];
} BridgeRcvBuf;

_Static_assert(sizeof(BridgeRcvBuf) >= sizeof(AudioHubHelloRequest) + sizeof(mach_msg_max_trailer_t),
               "receive buffer too small for hello + trailer");
_Static_assert(sizeof(BridgeRcvBuf) >= sizeof(AudioHubControlMsg) + sizeof(mach_msg_max_trailer_t),
               "receive buffer too small for notify + trailer");

static uint64_t bridge_now_msec(void)
{
    return clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) / 1000000ull;
}

static kern_return_t bridge_send_control(uint32_t inOp, uint32_t inDevice, uint64_t inWord); // defined just below

// Consecutive MACH_SEND_TIMED_OUT results on the daemon's control port. A full
// queue means a wedged daemon rather than a dead one — but only for a bounded
// number of tries. Treating every timeout as "alive" meant a daemon that had
// stopped draining its port was NEVER disconnected and held both rings
// indefinitely. The daemon applies the same bound (MAX_SEND_TIMEOUTS = 3) to
// its own sends in the opposite direction.
static uint32_t gSendTimeouts = 0;

// inTellSuperseded: post kAudioHubCtl_Superseded on the outgoing control port
// before it is deallocated, so a daemon we are replacing detaches NOW instead
// of waiting out its own 5s silence timer. Without it the loser's ring mappings
// stay valid for those 5s (the entry ports are the same kernel objects across
// reconnects, so nothing invalidates them), both daemons consume the speaker
// ring and both produce into the mic ring, and then the loser reconnects and
// displaces the winner — a permanent flip-flop. See the op's documentation in
// AudioHubBridge.h; the send is best-effort and safe against a daemon too old
// to know the op.
static void bridge_disconnect(int inTellSuperseded)
{
    if(inTellSuperseded && (gDaemonPort != MACH_PORT_NULL))
    {
        (void)bridge_send_control(kAudioHubCtl_Superseded, kAudioHubDevice_Speaker, 0);
    }
    bridge_ring_unpublish(&gSpkRing);
    bridge_ring_unpublish(&gMicRing);
    bridge_ring_reset(&gSpkRing);
    bridge_ring_reset(&gMicRing);
    if(gDaemonPort != MACH_PORT_NULL)
    {
        mach_port_deallocate(mach_task_self(), gDaemonPort);
        gDaemonPort = MACH_PORT_NULL;
    }
    gSendTimeouts = 0;
}

// Returns the raw kern_return_t: the outbox has to tell "queue full" from "port
// is dead", because a post that timed out was not delivered and must be retried
// rather than marked sent.
static kern_return_t bridge_send_control(uint32_t inOp, uint32_t inDevice, uint64_t inWord)
{
    AudioHubControlMsg theMsg;
    memset(&theMsg, 0, sizeof(theMsg));
    theMsg.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0);
    theMsg.header.msgh_size = sizeof(theMsg);
    theMsg.header.msgh_remote_port = gDaemonPort;
    theMsg.header.msgh_local_port = MACH_PORT_NULL;
    theMsg.header.msgh_id = kAudioHubMsg_Control;
    theMsg.op = inOp;
    theMsg.device = inDevice;
    theMsg.scalar_bits = (uint32_t)(inWord & 0xFFFFFFFFu);
    theMsg.flags = (uint32_t)(inWord >> 32);
    theMsg.seq = ++gControlSeq;

    return mach_msg(&theMsg.header,
                    MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                    sizeof(theMsg),
                    0,
                    MACH_PORT_NULL,
                    kBridgeSendTimeoutMsec,
                    MACH_PORT_NULL);
}

// Returns non-zero while the daemon is still reachable, and is the only place
// gSendTimeouts moves.
static int bridge_send_alive(kern_return_t inResult)
{
    if(inResult == MACH_MSG_SUCCESS)
    {
        gSendTimeouts = 0;
        return 1;
    }
    if(inResult != MACH_SEND_TIMED_OUT)
    {
        return 0;
    }
    return (++gSendTimeouts < kBridgeMaxSendTimeouts);
}

// Returns non-zero while the daemon is still reachable.
static int bridge_flush_outbox(void)
{
    int theAlive = 1;
    for(uint32_t theDevice = 0; theAlive && (theDevice < kOutboxSlots); ++theDevice)
    {
        uint64_t theSeq = atomic_load(&gVolSeq[theDevice]);
        if(theSeq != gVolSent[theDevice])
        {
            // Only a DELIVERED post advances the sent marker. Advancing it
            // unconditionally meant a send that timed out — which
            // bridge_send_control reports as a full queue, i.e. survivable —
            // silently threw the volume/mute change away with no retry.
            const kern_return_t theResult =
                bridge_send_control(kAudioHubCtl_Volume, theDevice, atomic_load(&gVolWord[theDevice]));
            if(theResult == MACH_MSG_SUCCESS)
            {
                gVolSent[theDevice] = theSeq;
            }
            theAlive = bridge_send_alive(theResult);
        }
        theSeq = atomic_load(&gIOSeq[theDevice]);
        if(theAlive && (theSeq != gIOSent[theDevice]))
        {
            const kern_return_t theResult =
                bridge_send_control(kAudioHubCtl_IOState, theDevice, atomic_load(&gIOWord[theDevice]));
            if(theResult == MACH_MSG_SUCCESS)
            {
                gIOSent[theDevice] = theSeq;
            }
            theAlive = bridge_send_alive(theResult);
        }
    }
    return theAlive;
}

// Periodic ring census, service thread only — it just observes the two atomics,
// so it costs the IOProc nothing. This exists because "the daemon reports zero
// frames while audio is playing" is a symptom this project already spent a
// debugging cycle on, and it is unattributable from the daemon's side alone:
// the ONLY thing that distinguishes "the HAL never handed us a WriteMix" from
// "we produced frames the daemon never consumed" is whether write_idx moves
// here.
//
// OPT-IN. It used to run in release builds, so a system daemon emitted a line
// into the system log every 5s for as long as anyone played audio. It is a
// debugging aid for one specific investigation, not telemetry: gRingLog gates
// it (see kBridgeRingLogEnv).
static int      gRingLog      = 0;
static uint64_t gLastSpkWrite = 0;
static uint64_t gLastMicRead  = 0;

static void bridge_report_rings(void)
{
    if((gRingLog == 0) || (gSpkRing.hdr == NULL) || (gMicRing.hdr == NULL))
    {
        return;
    }
    const uint64_t theSpkWrite = atomic_load_explicit(&gSpkRing.hdr->write_idx, memory_order_relaxed);
    const uint64_t theSpkRead  = atomic_load_explicit(&gSpkRing.hdr->read_idx, memory_order_relaxed);
    const uint64_t theMicWrite = atomic_load_explicit(&gMicRing.hdr->write_idx, memory_order_relaxed);
    const uint64_t theMicRead  = atomic_load_explicit(&gMicRing.hdr->read_idx, memory_order_relaxed);

    // read_idx belongs to the daemon, so this is a display value only — clamped
    // because a reconnecting (or hostile) peer can leave it ahead of write_idx.
    const uint64_t theBacklog = (theSpkWrite >= theSpkRead) ? (theSpkWrite - theSpkRead) : 0;

    if((theSpkWrite != gLastSpkWrite) || (theMicRead != gLastMicRead))
    {
        os_log(OS_LOG_DEFAULT, kBridgeLog "rings: spk driver wrote %llu / daemon took %llu (backlog %llu) | "
                                          "mic daemon wrote %llu / driver took %llu",
               (unsigned long long)theSpkWrite, (unsigned long long)theSpkRead,
               (unsigned long long)theBacklog,
               (unsigned long long)theMicWrite, (unsigned long long)theMicRead);
    }
    else if((atomic_load(&gIOWord[kAudioHubDevice_Speaker]) >> 32) & kAudioHubFlag_IORunning)
    {
        // A frozen write_idx has two completely different causes and the whole
        // point of this census is to say WHICH. A full ring stalls the producer,
        // so it looks identical from write_idx alone — and it is the likelier of
        // the two in the field, because it only takes the daemon pausing its
        // drain for 500ms. Blaming the HAL for it, at error level, pointed every
        // reader at the wrong subsystem.
        if(theBacklog >= gSpkRing.capacityFrames)
        {
            os_log_error(OS_LOG_DEFAULT, kBridgeLog "rings: speaker IO is running but the ring has been full "
                                                    "for %ums (backlog %llu of %u frames) — audiohubd has "
                                                    "stopped draining it",
                         kBridgeStatIntervalMsec, (unsigned long long)theBacklog, gSpkRing.capacityFrames);
        }
        else
        {
            os_log_error(OS_LOG_DEFAULT, kBridgeLog "rings: speaker IO is running, the ring has room "
                                                    "(backlog %llu of %u frames), and the driver produced no "
                                                    "frames in %ums — the HAL is not delivering WriteMix",
                         (unsigned long long)theBacklog, gSpkRing.capacityFrames, kBridgeStatIntervalMsec);
        }
    }
    gLastSpkWrite = theSpkWrite;
    gLastMicRead  = theMicRead;
}

// A fresh daemon must learn the current control state without waiting for the
// user to touch a slider.
static void bridge_repost_state(void)
{
    for(uint32_t theDevice = 0; theDevice < kOutboxSlots; ++theDevice)
    {
        gVolSent[theDevice] = 0;
        gIOSent[theDevice] = 0;
        atomic_fetch_add(&gVolSeq[theDevice], 1);
        atomic_fetch_add(&gIOSeq[theDevice], 1);
    }
}

// The audit trailer the kernel appended behind the message, or NULL if it is
// not the shape we asked for. The trailer sits at the message's rounded-up size
// and is the ONLY account of the sender that the sender cannot write.
static const mach_msg_audit_trailer_t* bridge_audit_trailer(const mach_msg_header_t* inHeader)
{
    const mach_msg_trailer_t* theTrailer =
        (const mach_msg_trailer_t*)(((const uint8_t*)inHeader) + round_msg(inHeader->msgh_size));
    if((theTrailer->msgh_trailer_type != MACH_MSG_TRAILER_FORMAT_0) ||
       (theTrailer->msgh_trailer_size < sizeof(mach_msg_audit_trailer_t)))
    {
        return NULL;
    }
    return (const mach_msg_audit_trailer_t*)theTrailer;
}

// The uid that owns the graphical session coreaudiod is serving. loginwindow
// chowns /dev/console to that user at login, so one stat answers it without
// SystemConfiguration — which matters because this code runs inside the
// sandboxed Core Audio driver host, where a mach round-trip to configd is not
// something we get to assume is permitted. Re-stat'd per handshake so a fast
// user switch is picked up; the last resolved value is kept if the stat fails,
// and 0 means "never resolved" (nobody logged in yet, or the stat was denied).
static uid_t gSessionOwner = 0;

static uid_t bridge_session_owner(void)
{
    struct stat theStat;
    if((stat("/dev/console", &theStat) == 0) && ((uint32_t)theStat.st_uid >= kBridgeFirstUserUID))
    {
        gSessionOwner = theStat.st_uid;
    }
    return gSessionOwner;
}

// Non-zero if this sender may be handed the rings; see the peer policy block at
// the top of this file for what is and is not enforceable today. outEUID/outPID
// are filled in whatever the verdict, for the log line.
static int bridge_peer_allowed(const mach_msg_audit_trailer_t* inTrailer, uid_t* outEUID, pid_t* outPID)
{
    *outEUID = (uid_t)-1;
    *outPID  = -1;
    if(inTrailer == NULL)
    {
        return 0; // our own receive is misconfigured; nothing to authenticate against
    }
    audit_token_t theToken = inTrailer->msgh_audit;
    *outEUID = audit_token_to_euid(theToken);
    *outPID  = audit_token_to_pid(theToken);

    const uid_t theOwner = bridge_session_owner();
    if(theOwner != 0)
    {
        return (*outEUID == theOwner);
    }
    // Owner unresolvable: fall back to "some real user account" rather than
    // refusing everyone. A check that cannot be evaluated must not brick audio,
    // and this still rejects every system/service account — _coreaudiod
    // included.
    return ((uint32_t)*outEUID >= kBridgeFirstUserUID);
}

static void bridge_handle_hello(BridgeRcvBuf* inBuf, mach_msg_size_t inSize, const mach_msg_audit_trailer_t* inTrailer)
{
    AudioHubHelloRequest* theRequest = &inBuf->hello;
    const mach_port_t theReplyPort = theRequest->header.msgh_remote_port;

    uid_t thePeerEUID = (uid_t)-1;
    pid_t thePeerPID = -1;
    if(!bridge_peer_allowed(inTrailer, &thePeerEUID, &thePeerPID))
    {
        // No reply of any kind: an unauthenticated caller should not even learn
        // that it was refused, and the reply must not become a probing oracle.
        // The frozen wire format has no status code for this and does not need
        // one — the real daemon runs as the session owner and cannot reach here.
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "refused hello from uid %u pid %d: the rings only go to the "
                                                "owner of the audio session (uid %u)",
                     thePeerEUID, thePeerPID, bridge_session_owner());
        mach_msg_destroy(&theRequest->header);
        return;
    }

    if((inSize < sizeof(AudioHubHelloRequest)) ||
       ((theRequest->header.msgh_bits & MACH_MSGH_BITS_COMPLEX) == 0) ||
       (theRequest->body.msgh_descriptor_count != 1) ||
       (theRequest->control_port.type != MACH_MSG_PORT_DESCRIPTOR) ||
       (theReplyPort == MACH_PORT_NULL))
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "malformed hello (size=%u bits=0x%x)",
                     inSize, theRequest->header.msgh_bits);
        mach_msg_destroy(&theRequest->header); // releases the reply port and any descriptor
        return;
    }

    const mach_port_t theDaemon = theRequest->control_port.name; // ours now
    const uint32_t theClaimedPID = theRequest->client_pid;       // sender-supplied: log only, never a decision

    uint32_t theStatus = kAudioHubStatus_OK;
    if(theRequest->protocol_version != kAudioHubProtocolVersion)
    {
        theStatus = kAudioHubStatus_BadVersion;
    }
    else if((gSpkRing.entry == MACH_PORT_NULL) || (gMicRing.entry == MACH_PORT_NULL))
    {
        theStatus = kAudioHubStatus_NoMemory;
    }

    if(theStatus == kAudioHubStatus_OK)
    {
        // Whoever is talking to us now supersedes whoever was before — a daemon
        // that died without us noticing yet must not keep the rings published.
        // ONLY a handshake that is about to be accepted, though: tearing the
        // session down before the status was consulted meant a Hello the driver
        // was about to REFUSE still unpublished both rings and dropped the live
        // daemon's port. One local process could hold the devices down forever
        // by sending protocol_version = 99 in a loop, and a version-skewed
        // daemon killed a working session on every attempt.
        bridge_disconnect(1);
    }

    AudioHubHelloReply theReply;
    memset(&theReply, 0, sizeof(theReply));
    // MACH_MSG_TYPE_MOVE_SEND_ONCE consumes the reply right the request carried,
    // which is what makes the send-once accounting balance on both sides.
    theReply.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
    theReply.header.msgh_size = sizeof(theReply);
    theReply.header.msgh_remote_port = theReplyPort;
    theReply.header.msgh_local_port = MACH_PORT_NULL;
    theReply.header.msgh_id = kAudioHubMsg_HelloReply;
    theReply.status = theStatus;
    theReply.protocol_version = kAudioHubProtocolVersion;
    theReply.data_offset = AUDIOHUB_RING_DATA_OFFSET;
    theReply.spk_capacity_frames = AUDIOHUB_RING_FRAMES;
    theReply.spk_channels = AUDIOHUB_SPK_CHANNELS;
    theReply.spk_sample_rate = AUDIOHUB_SAMPLE_RATE;
    theReply.mic_capacity_frames = AUDIOHUB_RING_FRAMES;
    theReply.mic_channels = AUDIOHUB_MIC_CHANNELS;
    theReply.mic_sample_rate = AUDIOHUB_SAMPLE_RATE;
    theReply.spk_bytes = AUDIOHUB_SPK_BYTES;
    theReply.mic_bytes = AUDIOHUB_MIC_BYTES;
    if(theStatus == kAudioHubStatus_OK)
    {
        theReply.header.msgh_bits |= MACH_MSGH_BITS_COMPLEX;
        theReply.body.msgh_descriptor_count = 2;
        // COPY_SEND: the entry ports stay ours across any number of reconnects.
        theReply.spk_entry.name = gSpkRing.entry;
        theReply.spk_entry.disposition = MACH_MSG_TYPE_COPY_SEND;
        theReply.spk_entry.type = MACH_MSG_PORT_DESCRIPTOR;
        theReply.mic_entry.name = gMicRing.entry;
        theReply.mic_entry.disposition = MACH_MSG_TYPE_COPY_SEND;
        theReply.mic_entry.type = MACH_MSG_PORT_DESCRIPTOR;
    }

    kern_return_t theResult = mach_msg(&theReply.header,
                                       MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                                       sizeof(theReply),
                                       0,
                                       MACH_PORT_NULL,
                                       kBridgeSendTimeoutMsec,
                                       MACH_PORT_NULL);
    if(theResult != MACH_MSG_SUCCESS)
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "hello reply to pid %d failed (0x%x)", thePeerPID, theResult);
        if(theResult != MACH_SEND_INVALID_DEST) // that error already destroyed the message
        {
            // A send that fails any other way is pseudo-received: the kernel
            // hands the reply right AND both COPY_SEND entry names back into
            // this space. Deallocating only theReplyPort therefore leaked one
            // uref on each of gSpkRing.entry / gMicRing.entry per occurrence.
            // mach_msg_destroy releases every right in the message according to
            // its disposition, which is exactly the set the pseudo-receive
            // returned.
            mach_msg_destroy(&theReply.header);
        }
        mach_port_deallocate(mach_task_self(), theDaemon);
        return;
    }
    if(theStatus != kAudioHubStatus_OK)
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "rejected hello from pid %d (status=%u)", thePeerPID, theStatus);
        mach_port_deallocate(mach_task_self(), theDaemon);
        return;
    }

    // Only now do the rings go live: the daemon holds both entries and has been
    // told the layout, so anything the IOProc writes from here on is readable.
    gDaemonPort = theDaemon;
    gSendTimeouts = 0;
    bridge_ring_publish(&gSpkRing);
    bridge_ring_publish(&gMicRing);
    bridge_repost_state();
    os_log(OS_LOG_DEFAULT, kBridgeLog "audiohubd attached (uid %u, pid %d, claims pid %u), rings live",
           thePeerEUID, thePeerPID, theClaimedPID);
}

static void bridge_handle_notify(BridgeRcvBuf* inBuf, mach_msg_size_t inSize)
{
    AudioHubControlMsg* theMsg = &inBuf->ctl;
    // A complex message lays its descriptor array over exactly the bytes op /
    // device / scalar_bits occupy, so a port name would parse as `device`.
    // Nothing on this path carries rights, so whatever that is, it is not ours;
    // mach_msg_destroy below still disposes of it correctly. The daemon's mirror
    // of this dispatch rejects the same shape.
    if(((theMsg->header.msgh_bits & MACH_MSGH_BITS_COMPLEX) == 0) &&
       (inSize >= sizeof(AudioHubControlMsg)) &&
       (theMsg->op == kAudioHubNotify_Volume) &&
       (gNotifyProc != NULL))
    {
        float theScalar;
        memcpy(&theScalar, &theMsg->scalar_bits, sizeof(theScalar));
        gNotifyProc(theMsg->device, theScalar, (theMsg->flags & kAudioHubFlag_Muted) != 0);
    }
    mach_msg_destroy(&theMsg->header);
}

// Blocks up to kBridgeRecvTimeoutMsec waiting for the first message, then drains
// what else is queued without waiting again. That first bounded wait is what
// paces the whole service loop — nothing here may be allowed to spin, a hot loop
// inside coreaudiod is a system-wide problem.
//
// Two things keep an unfriendly sender from turning that into one. The drain is
// bounded per pass: with theTimeout = 0 an endless queue of unknown-id messages
// would otherwise never let the caller reach the heartbeat or the census, so
// after kBridgeDrainPerPass messages we hand control back and the next pass
// starts with the pacing wait again. And any receive result that is neither
// success nor a timeout — MACH_RCV_TOO_LARGE above all, which returns instantly
// — backs off first, the same 50ms discipline the daemon applies to its own
// receive loop.
static void bridge_serve(void)
{
    mach_msg_timeout_t theTimeout = kBridgeRecvTimeoutMsec;
    for(uint32_t theDrained = 0; theDrained < kBridgeDrainPerPass; ++theDrained)
    {
        BridgeRcvBuf theBuf;
        memset(&theBuf, 0, sizeof(theBuf));
        // The audit trailer is requested on every receive: it is the kernel's
        // account of who sent the message, and the only input to the Hello
        // authentication that the sender cannot forge.
        kern_return_t theResult = mach_msg(&theBuf.hdr,
                                           MACH_RCV_MSG | MACH_RCV_TIMEOUT |
                                               MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
                                               MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT),
                                           0,
                                           sizeof(theBuf),
                                           gServicePort,
                                           theTimeout,
                                           MACH_PORT_NULL);
        if(theResult != MACH_MSG_SUCCESS)
        {
            if(theResult != MACH_RCV_TIMED_OUT) // the normal exit; anything else must not spin
            {
                usleep(kBridgeErrorBackoffUsec);
            }
            return;
        }
        switch(theBuf.hdr.msgh_id)
        {
            case kAudioHubMsg_Hello:
                bridge_handle_hello(&theBuf, theBuf.hdr.msgh_size, bridge_audit_trailer(&theBuf.hdr));
                break;
            case kAudioHubMsg_Notify:
                bridge_handle_notify(&theBuf, theBuf.hdr.msgh_size);
                break;
            default:
                mach_msg_destroy(&theBuf.hdr);
                break;
        }
        theTimeout = 0; // drain the rest of the queue without another pacing wait
    }
}

// Claims the receive right for our own service name. This only succeeds because
// the bundle's Info.plist declares that name under AudioServerPlugIn_MachServices
// and coreaudiod (global bootstrap namespace) is the process we live in.
static int bridge_check_in(void)
{
    mach_port_t theBootstrap = MACH_PORT_NULL;
    if(task_get_special_port(mach_task_self(), TASK_BOOTSTRAP_PORT, &theBootstrap) != KERN_SUCCESS)
    {
        return 0;
    }
    mach_port_t thePort = MACH_PORT_NULL;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    kern_return_t theResult = bootstrap_check_in(theBootstrap, kAudioHubDriverMachServiceName, &thePort);
#pragma clang diagnostic pop
    mach_port_deallocate(mach_task_self(), theBootstrap);
    if((theResult != BOOTSTRAP_SUCCESS) || (thePort == MACH_PORT_NULL))
    {
        return 0;
    }
    gServicePort = thePort;
    return 1;
}

static void* bridge_thread(void* inArg)
{
    (void)inArg;
    pthread_setname_np("com.audiohub.hal.bridge");

    if(!bridge_ring_create(&gSpkRing, AUDIOHUB_SPK_CHANNELS, AUDIOHUB_SPK_BYTES) ||
       !bridge_ring_create(&gMicRing, AUDIOHUB_MIC_CHANNELS, AUDIOHUB_MIC_BYTES))
    {
        // Nothing to hand a daemon, so there is nothing to serve. The devices
        // stay listed and silent, which is the required degraded behaviour.
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "shared rings unavailable; devices stay silent");
        return NULL;
    }

    // BOUNDED. The transient case worth retrying is a previous instance of this
    // bundle whose port has not been reaped yet, which clears in seconds. The
    // other two — the name missing from a stale installed Info.plist, or claimed
    // by someone else — never clear, and retrying them forever meant a 2s sleep
    // loop and a periodic error line inside coreaudiod for the life of the
    // machine. Giving up leaves the plug-in in its documented degraded state:
    // both devices listed, output discarded, input silent.
    for(uint32_t theAttempt = 0; (gServicePort == MACH_PORT_NULL) && (theAttempt < kBridgeCheckInMaxTries); ++theAttempt)
    {
        if(!bridge_check_in())
        {
            if(theAttempt == 0)
            {
                os_log_error(OS_LOG_DEFAULT, kBridgeLog "bootstrap_check_in(%s) failed; retrying",
                             kAudioHubDriverMachServiceName);
            }
            usleep(kBridgeCheckInRetryUsec);
        }
    }
    if(gServicePort == MACH_PORT_NULL)
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "bootstrap_check_in(%s) still failing after %u attempts; "
                                                "giving up, devices stay silent until coreaudiod restarts",
                     kAudioHubDriverMachServiceName, kBridgeCheckInMaxTries);
        return NULL;
    }
    gRingLog = (getenv(kBridgeRingLogEnv) != NULL);
    os_log(OS_LOG_DEFAULT, kBridgeLog "registered %s, waiting for audiohubd", kAudioHubDriverMachServiceName);

    uint64_t theLastBeat = bridge_now_msec();
    uint64_t theLastStat = theLastBeat;
    for(;;)
    {
        if(gDaemonPort != MACH_PORT_NULL)
        {
            int theAlive = bridge_flush_outbox();
            const uint64_t theNow = bridge_now_msec();
            if(theAlive && ((theNow - theLastBeat) >= kBridgeHeartbeatMsec))
            {
                theAlive = bridge_send_alive(bridge_send_control(kAudioHubCtl_Heartbeat, kAudioHubDevice_Speaker, 0));
                theLastBeat = theNow;
            }
            if(theAlive && ((theNow - theLastStat) >= kBridgeStatIntervalMsec))
            {
                bridge_report_rings();
                theLastStat = theNow;
            }
            if(!theAlive)
            {
                // Either the port is dead or the daemon stopped draining it for
                // kBridgeMaxSendTimeouts consecutive sends; from here the two
                // are the same thing. No supersede op — there is nobody taking
                // over and nobody listening.
                os_log(OS_LOG_DEFAULT, kBridgeLog "audiohubd went away, devices continue in silence");
                bridge_disconnect(0);
            }
        }
        bridge_serve(); // blocks up to kBridgeRecvTimeoutMsec
    }
    return NULL;
}

static void bridge_start_once(void)
{
    pthread_attr_t theAttr;
    pthread_attr_init(&theAttr);
    pthread_attr_setdetachstate(&theAttr, PTHREAD_CREATE_DETACHED);
    pthread_t theThread;
    if(pthread_create(&theThread, &theAttr, bridge_thread, NULL) != 0)
    {
        os_log_error(OS_LOG_DEFAULT, kBridgeLog "could not start the bridge thread; devices stay silent");
    }
    pthread_attr_destroy(&theAttr);
}

void AudioHubBridge_Start(AudioHubBridge_NotifyProc inNotifyProc)
{
    gNotifyProc = inNotifyProc; // set before the thread can call it
    pthread_once(&gBridgeOnce, bridge_start_once);
}
