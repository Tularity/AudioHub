//  AudioHubDriver.c — AudioHub virtual-device AudioServerPlugIn.
//  Architecture mirrors Apple's NullAudio.c sample: one static COM object with
//  the full AudioServerPlugInDriverInterface dispatch table and
//  mach_absolute_time-based zero timestamps. The audiohubd transport lives in
//  AudioHubBridge.c (spec-round2 §B1): the plug-in registers a mach service,
//  audiohubd connects to it, and the plug-in hands over its shared-memory rings.
//
//  ONE PAIR OF DEVICES PER PAIRED PEER (spec-m5b). The object tree is no longer
//  static. A fixed pool of sixteen SLOTS exists from the first Initialize, each
//  with its own (out, in) ring pair that is created once and never released; a
//  slot holds no devices until audiohubd BINDS a peer to it, and publishing a
//  device is nothing but filling in that slot's record. Three consequences worth
//  stating before reading any of the code below:
//
//    - WITH NO DAEMON, THERE ARE NO DEVICES. Not two silent ones: zero. Nothing
//      is published until a Bind arrives, so a driver installed next to a
//      version-skewed or absent daemon adds nothing at all to the system's
//      device list. That is the intended loud failure (spec-m5b §4.1).
//    - OBJECT IDS ARE ALLOCATED, MONOTONIC, AND NEVER REUSED. Four per device,
//      all four stored in the record. Nothing derives a stream/volume/mute id
//      from a device id by arithmetic: slots get reused, and an arithmetic
//      mapping would let an app holding a stale id operate a DIFFERENT peer's
//      device instead of getting the kAudioHardwareBadObjectError it must get.
//    - REMOVAL IS TWO-PHASE. A device leaves the list first and stays fully
//      functional until the HAL has let go of it (see AudioHub_RetireDueSlots).
//
//  Everything the plug-in publishes about a peer — both UIDs and both names —
//  comes down the wire from audiohubd. This code runs in coreaudiod's sandbox,
//  where it can read nothing but its own bundle, so it has neither the computer
//  name nor a localisation table; keeping the naming in one place on the daemon
//  side is not a preference, it is the only option (spec-m5b §3.5).

#include "AudioHubBridge.h"

#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreFoundation/CFPlugInCOM.h>
#include <mach/mach_time.h>
#include <os/log.h>
#include <pthread.h>
#include <math.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

// Must match the CFPlugInFactories key in Info.plist.
#define kAudioHubDriver_FactoryUUIDString "E216324F-6D1C-4B60-9847-A1C501BB479B"

#define kAudioHubDriverLog "[audiohub-hal] "

enum
{
    kObjectID_PlugIn = kAudioObjectPlugInObject
};

// 1 is the plug-in; 2..31 are reserved for whatever fixed objects a later
// version wants, so that no dynamic id can ever collide with one that used to be
// hard-coded. Every id at or above this is allocated from gNextObjectID.
#define kFirstDynamicObjectID    32u
#define kObjectsPerDevice        4u // device, stream, volume, mute
#define kAudioHubDevsPerSlot     2u // [0] = out (speaker), [1] = in (microphone)

// How long a delisted device keeps answering the HAL before it is invalidated
// anyway. Only reached when a client process died or wedged with IO still
// running, since the normal path is the ioRunning-reaches-zero edge.
#define kRetireGraceMsec         5000u

#define kDevice_SampleRate       48000.0
// NullAudio-style zero-timestamp ring: 512-frame period x 32 periods.
#define kDevice_FramesPerPeriod  512u
#define kDevice_RingPeriodCount  32u
#define kDevice_RingFrameCount   (kDevice_FramesPerPeriod * kDevice_RingPeriodCount)

#define kVolume_MinDB (-64.0f)
#define kVolume_MaxDB (0.0f)

// One pending control-plane post. Each is a single 64-bit word so a reader can
// never see the scalar of one post paired with the flags of another; the
// sequence counter is what tells the service thread there is something new, and
// a post that races the send is simply resent on the next pass.
//
// PER DEVICE, not global. These used to be six arrays indexed by a device
// selector; retirement has to be able to drop exactly one slot's pending posts
// without disturbing any other's, and a global array indexed by a computed
// number fails the same silent way a global ring does — it keeps working,
// addressed to the wrong peer.
typedef struct AudioHubMailbox
{
    _Atomic(uint64_t) word;
    _Atomic(uint64_t) seq;  // bumped by the poster
    uint64_t          sent; // service thread only; == seq means nothing pending
} AudioHubMailbox;

typedef struct AudioHubDevice
{
    // ---- identity. Written only by the service thread and only while live == 0,
    // so a reader that got here through Acquire sees a consistent set. All four
    // ids are atomic because the lookup below scans them BEFORE it can take a
    // reference — there is no earlier point at which the scan could be fenced.
    _Atomic(AudioObjectID) deviceID; // 0 = unallocated; the realtime path reads this
    _Atomic(AudioObjectID) streamID;
    _Atomic(AudioObjectID) volumeID;
    _Atomic(AudioObjectID) muteID;

    // ---- constant from the first Initialize onwards
    uint32_t        slotIndex;
    uint32_t        dirIndex;
    uint32_t        endpoint; // AUDIOHUB_ENDPOINT(slotIndex, dirIndex)
    Boolean         isInput;
    // Taken from the ring constants, never spelled out again: AudioHubBridge's
    // write path silently writes NOTHING when the caller's channel count
    // disagrees with the ring's, so a drift here would look exactly like a dead
    // daemon.
    UInt32          channelCount;

    // ---- bound state, guarded by gPlugIn_StateMutex
    BridgeRing*     ring; // this slot's ring for this direction; never re-pointed
    CFStringRef     deviceName; // +1, replaced in place on a rename
    CFStringRef     deviceUID;  // +1, constant for the life of the binding
    CFStringRef     modelUID;   // +1 on a shared CFSTR constant
    Boolean         listed;     // in the published device list

    // ---- publication and references (Dekker, same shape as BridgeRing's)
    _Atomic(uint32_t) live;  // 1 = this record may be resolved by an Acquire
    _Atomic(uint32_t) inuse; // calls currently holding a reference to it

    // ---- control-plane outbox
    AudioHubMailbox vol;
    AudioHubMailbox io;

    // ---- mutable state; scalar/mute/streamIsActive guarded by
    // gPlugIn_StateMutex, IO fields guarded by ioMutex
    pthread_mutex_t ioMutex;
    UInt64          ioRunning;
    Float64         sampleRate;
    Float64         hostTicksPerFrame;
    UInt64          anchorHostTime;
    UInt64          timeStampCount;
    Boolean         streamIsActive;
    Float32         volumeScalar;
    Boolean         muted;
} AudioHubDevice;

typedef enum
{
    kSlotFree = 0,
    kSlotBound,     // published, in the device list
    kSlotDelisted,  // off the list, still answering the HAL in full
    kSlotRetiring   // invalidating; no new reference can be taken
} SlotState;

typedef struct AudioHubSlot
{
    SlotState       state;      // service thread only
    // Bumped on every Free -> Bound transition and stamped on every control
    // message about this slot. It is what lets the daemon throw away a StopIO
    // that was in flight when the slot changed hands, instead of applying it to
    // the new peer and lighting up its microphone indicator (spec-m5b §4.6).
    uint32_t        generation;
    uint64_t        delistedAtMsec;
    char            peerKey[40];  // fingerprint; log and idempotency only
    AudioHubDevice  dev[kAudioHubDevsPerSlot];
    AudioHubMailbox bindState;    // kAudioHubCtl_BindState, retried like the rest
} AudioHubSlot;

static pthread_mutex_t          gPlugIn_StateMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32                   gPlugIn_RefCount   = 0;
static AudioServerPlugInHostRef gPlugIn_Host       = NULL;

// Never moved, never freed; only the metadata inside changes.
static AudioHubSlot gSlots[kAudioHubMaxSlots];

// Guarded by gPlugIn_StateMutex. Never wraps: a bind takes eight, and refusing
// past UINT32_MAX - 8 costs about 34 years at one pairing per second.
static AudioObjectID gNextObjectID = kFirstDynamicObjectID;

// The published device list, rebuilt under gPlugIn_StateMutex whenever the set
// changes and memcpy'd out by both the size and the data property paths.
//
// NOT DOUBLE-BUFFERED. Two alternating snapshots have an ABA problem, and every
// announcement is immediately followed by the host reading the property back
// synchronously, so the flip and the read are naturally interleaved. A set that
// changes BETWEEN a size call and the following data call is benign and
// self-healing instead: growing gives the client a truncated list, shrinking
// gives it a smaller outDataSize, both are legal, and any change at all is
// followed by a PropertiesChanged that makes the client read again.
static AudioObjectID gDeviceList[kAudioHubMaxSlots * kAudioHubDevsPerSlot];
static UInt32        gDeviceListCount = 0;

// AudioHubDriver_Initialize assigns gPlugIn_Host and then starts the bridge
// thread while it is still running, so a daemon already partway up its Hello
// retry ladder can trigger an announcement before the plug-in object is
// registered on the host's side. Nothing announces until this is set at the very
// end of Initialize; binds that arrive first just change state (spec-m5b §3.4).
static _Atomic(uint32_t) gHostReady       = 0;
// Set by anything that changes the published set, cleared by the one announce
// at the end of a service pass.
static _Atomic(uint32_t) gDeviceListDirty = 0;

// Shared by every device of the same direction: the model UID says "what kind of
// thing is this", and every AudioHub peer output is the same kind of thing. It
// is what lets an app that remembers "some AudioHub speaker" recognise one.
#define kModelUID_Out CFSTR("AudioHub_PeerOutput")
#define kModelUID_In  CFSTR("AudioHub_PeerInput")

// Every device UID the daemon may bind starts with this. Enforced so that a
// sender which got past the transport's identity check still cannot publish a
// device claiming to be somebody else's hardware.
#define kAudioHubUIDPrefix "AudioHub:"

// ---------------------------------------------------------------- daemon bridge
//
// Transport lives in AudioHubBridge.c: THIS plug-in owns the mach service
// kAudioHubDriverMachServiceName, declared under "AudioServerPlugIn_MachServices"
// in the bundle's Info.plist, and audiohubd looks it up. The direction is
// inverted on purpose (see the header for the two measured failures of the
// obvious arrangement); no amount of socket code substitutes for it, because
// coreaudiod's sandbox has no other way out.
//
// CONSTRAINT: no bridge_* function may be called while gPlugIn_StateMutex is
// held. The mutex is process-wide across every device and every property call,
// so anything that could stall under it would wedge the whole plug-in (and thus
// coreaudiod). Callers snapshot the state they need inside the lock, unlock,
// then call the bridge — the same discipline the host's PropertiesChanged()
// call follows. Some bridge entry points now genuinely block:
// AudioHubBridge_UnpublishRing spins until no IOProc is inside the ring, so the
// constraint has teeth it did not have when everything was a mailbox post.
//
// THE SAME GOES FOR PropertiesChanged(). The host answers a notification by
// calling straight back into GetPropertyData, on another thread; sending one
// while holding a process-wide lock deadlocks the plug-in against itself.
//
// AND THE TWO LOCKS NEVER NEST. gPlugIn_StateMutex and a device's ioMutex are
// never held at the same time, in either order — retirement reads ioRunning with
// gPlugIn_StateMutex released for exactly this reason.

static Float32 AudioHub_ClampScalar(Float32 inValue); // defined with the other helpers below

typedef enum
{
    kObjectKind_Unknown = 0,
    kObjectKind_PlugIn,
    kObjectKind_Device,
    kObjectKind_Stream,
    kObjectKind_VolumeControl,
    kObjectKind_MuteControl
} ObjectKind;

// ---------------------------------------------------------------- object lookup

static inline AudioObjectID AudioHub_ID(const _Atomic(AudioObjectID)* inField)
{
    return atomic_load_explicit(inField, memory_order_relaxed);
}

// IOProc-SAFE, and it has to be: this is on the path of every DoIOOperation.
// At most 32 relaxed loads and one read-modify-write. No lock, no allocation, no
// syscall, no mach traffic, no unbounded loop. At 512 frames / 48kHz that is ~94
// calls a second per running device, so even with all 32 devices running the
// whole cost is microseconds per second.
//
// WHY A REFERENCE AND NOT JUST A POINTER. The records are static, so
// dereferencing one is always memory-safe, and a refcount would be pointless if
// that were the concern. What the reference protects is the BINDING: without it,
// DoIOOperation can resolve slot 3, the service thread can then retire slot 3 and
// re-bind it to peer B, and the call already in flight writes peer A's audio into
// peer B's ring. That is a correctness bug and a privacy bug at once, and it is
// invisible to every test that only plays audio to a single peer.
//
// A caller that gets a non-NULL answer MUST Release it.
static AudioHubDevice* AudioHub_AcquireByDeviceID(AudioObjectID inID)
{
    if(inID < kFirstDynamicObjectID)
    {
        return NULL; // the plug-in object and the reserved range are never devices
    }
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
            if(AudioHub_ID(&theDevice->deviceID) != inID)
            {
                continue;
            }
            // Dekker, the same shape the rings use: take the reference first,
            // then re-read the publication flag. The retiring side clears live
            // and only then waits for inuse to drain, so the two orders cannot
            // both miss each other.
            atomic_fetch_add(&theDevice->inuse, 1); // seq_cst
            if((atomic_load(&theDevice->live) != 0) && (AudioHub_ID(&theDevice->deviceID) == inID))
            {
                return theDevice;
            }
            atomic_fetch_sub_explicit(&theDevice->inuse, 1, memory_order_release);
            return NULL;
        }
    }
    return NULL;
}

static inline void AudioHub_Release(AudioHubDevice* inDevice)
{
    atomic_fetch_sub_explicit(&inDevice->inuse, 1, memory_order_release);
}

static ObjectKind AudioHub_ClassifyObject(const AudioHubDevice* inDevice, AudioObjectID inID)
{
    if(AudioHub_ID(&inDevice->deviceID) == inID) return kObjectKind_Device;
    if(AudioHub_ID(&inDevice->streamID) == inID) return kObjectKind_Stream;
    if(AudioHub_ID(&inDevice->volumeID) == inID) return kObjectKind_VolumeControl;
    if(AudioHub_ID(&inDevice->muteID) == inID)   return kObjectKind_MuteControl;
    return kObjectKind_Unknown;
}

// Same contract as AudioHub_AcquireByDeviceID but across all four ids of every
// device, for the property dispatch. *outOwner is NULL for the plug-in object and
// for anything unknown; a non-NULL *outOwner is a HELD reference and must be
// released. The four ids are compared, never computed: a slot that has been
// reused would make any arithmetic mapping resolve a stale id onto a live object
// belonging to a different machine.
static ObjectKind AudioHub_AcquireObject(AudioObjectID inID, AudioHubDevice** outOwner)
{
    *outOwner = NULL;
    if(inID == kObjectID_PlugIn)
    {
        return kObjectKind_PlugIn;
    }
    if(inID < kFirstDynamicObjectID)
    {
        return kObjectKind_Unknown;
    }
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
            const ObjectKind theKind = AudioHub_ClassifyObject(theDevice, inID);
            if(theKind == kObjectKind_Unknown)
            {
                continue;
            }
            atomic_fetch_add(&theDevice->inuse, 1); // seq_cst
            if((atomic_load(&theDevice->live) != 0) && (AudioHub_ClassifyObject(theDevice, inID) == theKind))
            {
                *outOwner = theDevice;
                return theKind;
            }
            atomic_fetch_sub_explicit(&theDevice->inuse, 1, memory_order_release);
            return kObjectKind_Unknown;
        }
    }
    return kObjectKind_Unknown;
}

// ---------------------------------------------------------------- outbox

static inline uint64_t AudioHub_PackWord(Float32 inScalar, uint32_t inFlags)
{
    uint32_t theBits;
    memcpy(&theBits, &inScalar, sizeof(theBits));
    return ((uint64_t)inFlags << 32) | (uint64_t)theBits;
}

static inline void AudioHub_Post(AudioHubMailbox* inBox, Float32 inScalar, uint32_t inFlags)
{
    atomic_store(&inBox->word, AudioHub_PackWord(inScalar, inFlags));
    atomic_fetch_add(&inBox->seq, 1);
}

// Out direction: hand the mix to the daemon. Discarded while the slot's ring is
// unpublished (plan §7.3: the device stays selectable, nothing is processed).
//
// The ring comes from the device RECORD. "Which device feeds which ring" is data
// rather than a function of the device's identity, and that is the whole point:
// a helper that mapped a device onto a ring is the one shape in which sixteen
// devices pour into a single ring while every test that exercises one peer at a
// time still passes.
static void bridge_write_output(AudioHubDevice* inDevice, const Float32* inBuffer, UInt32 inFrameCount)
{
    AudioHubBridge_WriteRing(inDevice->ring, inBuffer, inFrameCount, inDevice->channelCount);
}

// In direction: pull the peer's audio. Short reads and "no daemon" both come back
// as silence, never as a stalled IO cycle.
static void bridge_read_input(AudioHubDevice* inDevice, Float32* outBuffer, UInt32 inFrameCount)
{
    AudioHubBridge_ReadRing(inDevice->ring, outBuffer, inFrameCount, inDevice->channelCount);
}

// Control plane (plan §7.2 forward direction): the local user moved the virtual
// device's slider, so the daemon must push it to the peer's REAL device. Posts to
// a lock-free mailbox; the bridge thread does the mach send. The caller holds a
// reference to inDevice, which is what guarantees the post cannot land in a
// mailbox the service thread has already cleared for the next peer.
static void bridge_volume_changed(AudioHubDevice* inDevice, Float32 inVolumeScalar, Boolean inMuted)
{
    uint32_t theFlags = inMuted ? kAudioHubFlag_Muted : 0u;
    if(inDevice->isInput)
    {
        theFlags |= kAudioHubFlag_IsInput;
    }
    AudioHub_Post(&inDevice->vol, inVolumeScalar, theFlags);
}

static void bridge_io_state_changed(AudioHubDevice* inDevice, Boolean inRunning)
{
    uint32_t theFlags = inRunning ? kAudioHubFlag_IORunning : 0u;
    if(inDevice->isInput)
    {
        theFlags |= kAudioHubFlag_IsInput;
    }
    AudioHub_Post(&inDevice->io, 0.0f, theFlags);
}

// Returns non-zero while the daemon is still reachable. Only a DELIVERED post
// advances the sent marker: a send that merely timed out was not delivered, and
// marking it sent used to throw the volume change away with no retry.
static int AudioHub_FlushMailbox(AudioHubMailbox* inBox, uint32_t inOp, uint32_t inEndpoint, uint32_t inGeneration)
{
    const uint64_t theSeq = atomic_load(&inBox->seq);
    if(theSeq == inBox->sent)
    {
        return 1;
    }
    const uint32_t theResult =
        AudioHubBridge_SendControl(inOp, inEndpoint, inGeneration, atomic_load(&inBox->word));
    if(theResult == kAudioHubSend_OK)
    {
        inBox->sent = theSeq;
    }
    return (theResult != kAudioHubSend_Dead);
}

// Service thread. The slot's CURRENT generation stamps every message, and that is
// sound rather than lucky: a post can only be made by a caller holding a
// reference, and retirement clears the mailboxes only after every reference has
// drained, so nothing produced under generation N can still be pending once the
// slot has reached N+1. Delisted slots are drained too — that is the whole point
// of two-phase retirement, since the StopIO that closes the daemon's session
// arrives after the device has left the list.
static int AudioHub_FlushOutbox(void)
{
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        AudioHubSlot* theSlot = &gSlots[theSlotIndex];
        const uint32_t theGeneration = theSlot->generation;
        if(!AudioHub_FlushMailbox(&theSlot->bindState, kAudioHubCtl_BindState,
                                  AUDIOHUB_ENDPOINT(theSlotIndex, kAudioHubDir_Out), theGeneration))
        {
            return 0;
        }
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &theSlot->dev[theDir];
            if(!AudioHub_FlushMailbox(&theDevice->vol, kAudioHubCtl_Volume, theDevice->endpoint, theGeneration) ||
               !AudioHub_FlushMailbox(&theDevice->io, kAudioHubCtl_IOState, theDevice->endpoint, theGeneration))
            {
                return 0;
            }
        }
    }
    return 1;
}

static uint32_t AudioHub_IORunningMask(void)
{
    uint32_t theMask = 0;
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
            // The last word POSTED, not ioRunning itself: this is only ever asked
            // for the opt-in ring census, and taking 32 per-device mutexes to
            // answer a debugging question is not a trade worth making.
            if(((atomic_load(&theDevice->io.word) >> 32) & kAudioHubFlag_IORunning) != 0)
            {
                theMask |= (1u << theDevice->endpoint);
            }
        }
    }
    return theMask;
}

// ---------------------------------------------------------------- slots

static uint64_t AudioHub_NowMsec(void)
{
    return clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) / 1000000ull;
}

// hostTicksPerFrame is what turns the zero-timestamp counter into a host clock,
// and there is no diagnostic anywhere for getting it wrong: leave it at 0 and
// theHostTicksPerRingBuffer becomes 0, so GetZeroTimeStamp advances
// timeStampCount on EVERY call and the device's sample time runs away from real
// time, silently. It used to be computed once in Initialize for two static
// devices; every bind has to do it now, which is why it is a function.
static void AudioHub_InitDeviceTiming(AudioHubDevice* inDevice)
{
    struct mach_timebase_info theTimeBaseInfo;
    mach_timebase_info(&theTimeBaseInfo);
    const Float64 theHostClockFrequency =
        ((Float64)theTimeBaseInfo.denom / (Float64)theTimeBaseInfo.numer) * 1000000000.0;
    inDevice->hostTicksPerFrame = theHostClockFrequency / inDevice->sampleRate;
}

// The parts of a slot that never change once the pool exists. Called from
// Initialize, before the bridge thread exists and therefore before anything can
// look at a slot.
static void AudioHub_InitSlots(void)
{
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        AudioHubSlot* theSlot = &gSlots[theSlotIndex];
        theSlot->state = kSlotFree;
        theSlot->generation = 0;
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &theSlot->dev[theDir];
            theDevice->slotIndex      = theSlotIndex;
            theDevice->dirIndex       = theDir;
            theDevice->endpoint       = AUDIOHUB_ENDPOINT(theSlotIndex, theDir);
            theDevice->isInput        = (theDir == kAudioHubDir_In);
            theDevice->channelCount   = theDevice->isInput ? AUDIOHUB_MIC_CHANNELS : AUDIOHUB_SPK_CHANNELS;
            theDevice->sampleRate     = kDevice_SampleRate;
            theDevice->volumeScalar   = 1.0f;
            theDevice->streamIsActive = true;
            pthread_mutex_init(&theDevice->ioMutex, NULL);
        }
    }
}

// gPlugIn_StateMutex held.
static void AudioHub_RebuildDeviceListLocked(void)
{
    UInt32 theCount = 0;
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
            if(theDevice->listed)
            {
                gDeviceList[theCount++] = AudioHub_ID(&theDevice->deviceID);
            }
        }
    }
    gDeviceListCount = theCount;
}

// Ordinary thread (the bridge service thread), NO lock held, and only once the
// host has finished registering the plug-in object.
//
// BOTH PROPERTIES, ALWAYS. Apple's own sample and libASPL send the pair, and the
// reason is visible in the UI: announce only one and Audio MIDI Setup's list and
// an app's device menu disagree with each other indefinitely.
static void AudioHub_AnnounceDeviceList(void)
{
    if((gPlugIn_Host == NULL) || (atomic_load(&gHostReady) == 0))
    {
        return;
    }
    AudioObjectPropertyAddress theAddresses[2];
    theAddresses[0].mSelector = kAudioObjectPropertyOwnedObjects;
    theAddresses[0].mScope    = kAudioObjectPropertyScopeGlobal;
    theAddresses[0].mElement  = kAudioObjectPropertyElementMain;
    theAddresses[1].mSelector = kAudioPlugInPropertyDeviceList;
    theAddresses[1].mScope    = kAudioObjectPropertyScopeGlobal;
    theAddresses[1].mElement  = kAudioObjectPropertyElementMain;
    gPlugIn_Host->PropertiesChanged(gPlugIn_Host, kObjectID_PlugIn, 2, theAddresses);
}

// scalar_bits carries a kAudioHubSlot_* state verbatim on the wire (spec-m5b
// §4.3), so the value is moved through the float-shaped mailbox slot bit for bit
// rather than converted — a conversion would turn 0/1/2 into something that is no
// longer the state.
static void AudioHub_PostBindState(AudioHubSlot* inSlot, uint32_t inState)
{
    Float32 theStateAsScalar;
    memcpy(&theStateAsScalar, &inState, sizeof(theStateAsScalar));
    AudioHub_Post(&inSlot->bindState, theStateAsScalar, 0u);
}

// Re-post everything the daemon needs to know about one slot, taken from the
// device's ACTUAL state rather than from whatever happens to be in the mailbox.
//
// This is the replacement for the old global "repost everything on Hello", and it
// has to live here rather than there: at Hello time no slot has been bound in the
// new session yet, so every message sent then would carry a generation the daemon
// has no record of and its own filter would drop the lot — a failure mode
// indistinguishable from doing nothing at all. The absence of this call is what
// turns "audiohubd was upgraded while Zoom was recording" into Zoom silently
// recording air.
static void AudioHub_ReplaySlotState(AudioHubSlot* inSlot)
{
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubDevice* theDevice = &inSlot->dev[theDir];
        pthread_mutex_lock(&gPlugIn_StateMutex);
        const Float32 theScalar = theDevice->volumeScalar;
        const Boolean theMuted = theDevice->muted;
        pthread_mutex_unlock(&gPlugIn_StateMutex);
        pthread_mutex_lock(&theDevice->ioMutex);
        const Boolean theRunning = (theDevice->ioRunning > 0);
        pthread_mutex_unlock(&theDevice->ioMutex);

        theDevice->vol.sent = 0;
        theDevice->io.sent = 0;
        bridge_volume_changed(theDevice, theScalar, theMuted);
        bridge_io_state_changed(theDevice, theRunning);
    }
}

// Publish (or re-publish) the rings of a bound slot. Service thread only.
static void AudioHub_PublishSlotRings(AudioHubSlot* inSlot)
{
    if(!AudioHubBridge_SessionActive())
    {
        return;
    }
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubDevice* theDevice = &inSlot->dev[theDir];
        if(theDevice->ring == NULL)
        {
            continue;
        }
        if(theDevice->isInput)
        {
            // We are this ring's consumer, and anything left in it belongs to a
            // session that is over. Rendering it would play up to 500ms of the
            // previous daemon's microphone audio into whatever is recording.
            AudioHubBridge_FlushRingConsumer(theDevice->ring);
        }
        AudioHubBridge_PublishRing(theDevice->ring);
    }
}

// One string out of a fixed-size wire field. The terminator is imposed HERE and
// never trusted from the sender, and invalid UTF-8 comes back as NULL so the
// whole Bind can be refused rather than half-applied.
static CFStringRef AudioHub_CopyWireString(const char* inField, size_t inCapacity)
{
    char theBuffer[256];
    if((inCapacity == 0) || (inCapacity > sizeof(theBuffer)))
    {
        return NULL;
    }
    memcpy(theBuffer, inField, inCapacity);
    theBuffer[inCapacity - 1] = '\0';
    const size_t theLength = strlen(theBuffer);
    if(theLength == 0)
    {
        return NULL;
    }
    return CFStringCreateWithBytes(NULL, (const UInt8*)theBuffer, (CFIndex)theLength, kCFStringEncodingUTF8, false);
}

// The transport has already established that the sender is the process which
// completed the current Hello. This is the second half of that: even the real
// daemon may only publish devices under the project's own UID namespace, so a
// bug (or a compromised daemon) cannot put "MacBook Pro Speakers" into the
// system's audio picker.
static Boolean AudioHub_IsWellFormedUID(const char* inField, size_t inCapacity)
{
    char theBuffer[256];
    if((inCapacity == 0) || (inCapacity > sizeof(theBuffer)))
    {
        return false;
    }
    memcpy(theBuffer, inField, inCapacity);
    theBuffer[inCapacity - 1] = '\0';
    return (strncmp(theBuffer, kAudioHubUIDPrefix, strlen(kAudioHubUIDPrefix)) == 0) &&
           (strlen(theBuffer) > strlen(kAudioHubUIDPrefix));
}

// gPlugIn_StateMutex held. Non-zero if any device OTHER than inExcept's already
// publishes this UID: two slots claiming one UID would make TranslateUIDToDevice
// answer with whichever it met first, so an app's remembered device selection
// would follow a coin toss.
static Boolean AudioHub_UIDInUseLocked(CFStringRef inUID, const AudioHubSlot* inExcept)
{
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        if(&gSlots[theSlotIndex] == inExcept)
        {
            continue;
        }
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            CFStringRef theUID = gSlots[theSlotIndex].dev[theDir].deviceUID;
            if((theUID != NULL) && CFEqual(theUID, inUID))
            {
                return true;
            }
        }
    }
    return false;
}

// Free -> Bound. Service thread. Returns non-zero on success; on failure the slot
// is untouched and stays Free.
static int AudioHub_BindSlot(AudioHubSlot* inSlot, const AudioHubBindMsg* inMsg)
{
    CFStringRef theUIDs[kAudioHubDevsPerSlot];
    CFStringRef theNames[kAudioHubDevsPerSlot];
    theUIDs[kAudioHubDir_Out]  = AudioHub_CopyWireString(inMsg->out_uid, sizeof(inMsg->out_uid));
    theUIDs[kAudioHubDir_In]   = AudioHub_CopyWireString(inMsg->in_uid, sizeof(inMsg->in_uid));
    theNames[kAudioHubDir_Out] = AudioHub_CopyWireString(inMsg->out_name, sizeof(inMsg->out_name));
    theNames[kAudioHubDir_In]  = AudioHub_CopyWireString(inMsg->in_name, sizeof(inMsg->in_name));

    int theOK = (theUIDs[0] != NULL) && (theUIDs[1] != NULL) && (theNames[0] != NULL) && (theNames[1] != NULL);
    if(theOK && CFEqual(theUIDs[0], theUIDs[1]))
    {
        theOK = 0; // one UID for both directions would collapse the pair into one device
    }

    pthread_mutex_lock(&gPlugIn_StateMutex);
    if(theOK && (AudioHub_UIDInUseLocked(theUIDs[0], inSlot) || AudioHub_UIDInUseLocked(theUIDs[1], inSlot)))
    {
        theOK = 0;
    }
    if(theOK && (gNextObjectID > (UINT32_MAX - (kAudioHubDevsPerSlot * kObjectsPerDevice))))
    {
        // NEVER wrap. A reused id is precisely the failure this allocator exists
        // to prevent, so running out has to be a refusal and not a rollover. At
        // one pairing per second this is about 34 years away.
        theOK = 0;
        os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "object id space exhausted; refusing to bind slot %u",
                     inSlot->dev[0].slotIndex);
    }
    if(theOK)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &inSlot->dev[theDir];
            // Four ids, all four STORED. Nothing downstream may recompute one
            // from another.
            atomic_store_explicit(&theDevice->deviceID, gNextObjectID + 0, memory_order_relaxed);
            atomic_store_explicit(&theDevice->streamID, gNextObjectID + 1, memory_order_relaxed);
            atomic_store_explicit(&theDevice->volumeID, gNextObjectID + 2, memory_order_relaxed);
            atomic_store_explicit(&theDevice->muteID, gNextObjectID + 3, memory_order_relaxed);
            gNextObjectID += kObjectsPerDevice;

            theDevice->ring       = AudioHubBridge_RingForEndpoint(theDevice->endpoint);
            theDevice->deviceUID  = theUIDs[theDir];  // the +1 from CopyWireString, kept
            theDevice->deviceName = theNames[theDir]; // likewise
            theDevice->modelUID   = CFRetain(theDevice->isInput ? kModelUID_In : kModelUID_Out);
            theDevice->listed     = true;

            theDevice->sampleRate     = kDevice_SampleRate;
            AudioHub_InitDeviceTiming(theDevice);
            theDevice->volumeScalar   = 1.0f;
            theDevice->muted          = false;
            theDevice->streamIsActive = true;
            theDevice->anchorHostTime = 0;
            theDevice->timeStampCount = 0;
            theDevice->ioRunning      = 0;

            theDevice->vol.sent = 0;
            theDevice->io.sent = 0;
            atomic_store(&theDevice->vol.word, 0);
            atomic_store(&theDevice->vol.seq, 0);
            atomic_store(&theDevice->io.word, 0);
            atomic_store(&theDevice->io.seq, 0);

            atomic_store(&theDevice->live, 1); // last: everything above is visible by now
        }
        AudioHub_RebuildDeviceListLocked();
    }
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    if(!theOK)
    {
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            if(theUIDs[theDir] != NULL) CFRelease(theUIDs[theDir]);
            if(theNames[theDir] != NULL) CFRelease(theNames[theDir]);
        }
        return 0;
    }

    // Outside the lock, and only now: the rings are still unpublished and the
    // daemon was last told this slot was Free, so zeroing their indices cannot be
    // observed mid-read by anyone.
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubBridge_ResetRing(inSlot->dev[theDir].ring);
    }
    AudioHub_PublishSlotRings(inSlot);

    memcpy(inSlot->peerKey, inMsg->peer_key, sizeof(inSlot->peerKey));
    inSlot->peerKey[sizeof(inSlot->peerKey) - 1] = '\0';
    // THE SECOND OF THE TWO BUMPS. Retirement bumps as well (spec-m5b §3.7/§4.6),
    // and both are needed for different reasons: bumping there is what makes
    // BindState{Free} carry a generation the daemon can SEE change, which is its
    // cue to flush the slot's ring before the next peer produces into it;
    // bumping here is what keeps a live binding off generation 0, which is also
    // the wire's "this message concerns no slot" value. A binding stamped 0
    // would sail through a generation filter that is meant to catch it.
    ++inSlot->generation;
    inSlot->state = kSlotBound;
    atomic_store(&gDeviceListDirty, 1);
    AudioHub_PostBindState(inSlot, kAudioHubSlot_Bound);
    return 1;
}

// Bound -> Delisted, phase one of retirement. Service thread.
//
// DELIST BEFORE INVALIDATE, never the reverse. The HAL still calls StopIO and
// RemoveDeviceClient on a device AFTER it has left the device list — Apple's own
// sample defers destruction behind a refcount for exactly that reason — and
// bridge_io_state_changed(false) hangs off the ioRunning-reaches-zero edge inside
// StopIO. Invalidate first and that edge becomes unreachable: the daemon never
// learns the device stopped, its session never closes, and the peer's microphone
// indicator stays lit indefinitely. Delisting first also keeps the realtime path
// free of BadObjectError, and makes "removed while in use" behave exactly like
// unplugging a USB interface.
static void AudioHub_DelistSlot(AudioHubSlot* inSlot)
{
    pthread_mutex_lock(&gPlugIn_StateMutex);
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        inSlot->dev[theDir].listed = false; // live STAYS 1, on purpose
    }
    AudioHub_RebuildDeviceListLocked();
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    // Unpublishing spins until no IOProc is inside the ring, so it has to happen
    // outside the process-wide mutex.
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubBridge_UnpublishRing(inSlot->dev[theDir].ring);
    }

    inSlot->state = kSlotDelisted;
    inSlot->delistedAtMsec = AudioHub_NowMsec();
    atomic_store(&gDeviceListDirty, 1);
    AudioHub_PostBindState(inSlot, kAudioHubSlot_Delisted);
}

// Phase two, once the HAL has genuinely let go (or the grace period is up).
// Delisted -> Retiring -> Free. Service thread, once per service pass.
//
// Between phase one and here the devices answer EVERYTHING normally:
// GetZeroTimeStamp keeps advancing the timeline, DoIOOperation succeeds (the out
// frames are dropped because the ring is unpublished, the in frames come back as
// silence), StopIO and RemoveDeviceClient return noErr. That is byte for byte the
// behaviour of a peer that has gone offline, which is already frozen in plan
// §7.3 — no new concept, and nothing for a client to distinguish.
static void AudioHub_RetireDueSlots(void)
{
    const uint64_t theNow = AudioHub_NowMsec();
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        AudioHubSlot* theSlot = &gSlots[theSlotIndex];
        if(theSlot->state != kSlotDelisted)
        {
            continue;
        }
        UInt64 theRunning = 0;
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &theSlot->dev[theDir];
            // ioMutex is taken WITHOUT gPlugIn_StateMutex held: the two locks
            // never nest, in either order, anywhere in this file.
            pthread_mutex_lock(&theDevice->ioMutex);
            theRunning += theDevice->ioRunning;
            pthread_mutex_unlock(&theDevice->ioMutex);
        }
        const Boolean theExpired = ((theNow - theSlot->delistedAtMsec) > kRetireGraceMsec);
        if((theRunning != 0) && !theExpired)
        {
            continue; // still in use, and meanwhile it keeps answering normally
        }
        if(theRunning != 0)
        {
            // A client process that died or wedged without the HAL cleaning up
            // after it. Late calls from here on get kAudioHardwareBadObjectError,
            // which is exactly what they already got before any of this existed —
            // never a use-after-free, because the records are static.
            os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "slot %u forced through retirement after %ums with "
                                                            "IO still running",
                         theSlotIndex, kRetireGraceMsec);
        }

        theSlot->state = kSlotRetiring;
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            atomic_store(&theSlot->dev[theDir].live, 0); // seq_cst, pairs with Acquire's inuse++
        }
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            // One counter per DEVICE — deliberately not one per slot and not one
            // global. A spin that ends only when a counter is OBSERVED at zero
            // gets harder to satisfy the more traffic is folded into it, and the
            // per-device contention profile is the one already measured.
            while(atomic_load(&theSlot->dev[theDir].inuse) != 0)
            {
                usleep(200); // service thread; a holder is doing a memcpy or a property read
            }
        }

        // Nothing can be in flight now: every producer of a post holds a
        // reference, and there are none left. So the mailboxes can be cleared
        // without a cross-generation message escaping afterwards.
        pthread_mutex_lock(&gPlugIn_StateMutex);
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            AudioHubDevice* theDevice = &theSlot->dev[theDir];
            atomic_store_explicit(&theDevice->deviceID, 0, memory_order_relaxed);
            atomic_store_explicit(&theDevice->streamID, 0, memory_order_relaxed);
            atomic_store_explicit(&theDevice->volumeID, 0, memory_order_relaxed);
            atomic_store_explicit(&theDevice->muteID, 0, memory_order_relaxed);
            if(theDevice->deviceName != NULL) { CFRelease(theDevice->deviceName); theDevice->deviceName = NULL; }
            if(theDevice->deviceUID != NULL)  { CFRelease(theDevice->deviceUID);  theDevice->deviceUID = NULL; }
            if(theDevice->modelUID != NULL)   { CFRelease(theDevice->modelUID);   theDevice->modelUID = NULL; }
            theDevice->listed         = false;
            theDevice->volumeScalar   = 1.0f;
            theDevice->muted          = false;
            theDevice->streamIsActive = true;
            theDevice->ioRunning      = 0;
            theDevice->timeStampCount = 0;
            theDevice->anchorHostTime = 0;
            atomic_store(&theDevice->vol.word, 0);
            atomic_store(&theDevice->vol.seq, 0);
            atomic_store(&theDevice->io.word, 0);
            atomic_store(&theDevice->io.seq, 0);
            theDevice->vol.sent = 0;
            theDevice->io.sent = 0;
        }
        AudioHub_RebuildDeviceListLocked();
        pthread_mutex_unlock(&gPlugIn_StateMutex);

        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            // Still unpublished, so zeroing the indices is safe — and necessary,
            // so the next peer to take this slot starts from a clean ring rather
            // than half a second of its predecessor's audio.
            AudioHubBridge_ResetRing(theSlot->dev[theDir].ring);
        }

        memset(theSlot->peerKey, 0, sizeof(theSlot->peerKey));
        ++theSlot->generation;
        atomic_store(&theSlot->bindState.word, 0);
        atomic_store(&theSlot->bindState.seq, 0);
        theSlot->bindState.sent = 0;
        theSlot->state = kSlotFree;
        AudioHub_PostBindState(theSlot, kAudioHubSlot_Free);
    }
}

// A Bind naming the UIDs this slot already has, with a different name: update the
// strings in place and tell the host the NAME changed. Deliberately does not
// touch the device id, the UID, the device list or the ring — an app that
// remembered this device keeps its selection and no audio is interrupted.
static void AudioHub_RenameSlot(AudioHubSlot* inSlot, CFStringRef* inNames)
{
    AudioObjectID theChanged[kAudioHubDevsPerSlot];
    uint32_t theChangedCount = 0;

    pthread_mutex_lock(&gPlugIn_StateMutex);
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubDevice* theDevice = &inSlot->dev[theDir];
        if((theDevice->deviceName != NULL) && CFEqual(theDevice->deviceName, inNames[theDir]))
        {
            continue;
        }
        if(theDevice->deviceName != NULL)
        {
            CFRelease(theDevice->deviceName);
        }
        theDevice->deviceName = CFRetain(inNames[theDir]);
        theChanged[theChangedCount++] = AudioHub_ID(&theDevice->deviceID);
    }
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    for(uint32_t theIndex = 0; (gPlugIn_Host != NULL) && (theIndex < theChangedCount); ++theIndex)
    {
        AudioObjectPropertyAddress theAddress;
        theAddress.mSelector = kAudioObjectPropertyName;
        theAddress.mScope    = kAudioObjectPropertyScopeGlobal;
        theAddress.mElement  = kAudioObjectPropertyElementMain;
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, theChanged[theIndex], 1, &theAddress);
    }
}

// gPlugIn_StateMutex held.
static Boolean AudioHub_SlotHasUIDsLocked(const AudioHubSlot* inSlot, CFStringRef* inUIDs)
{
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        CFStringRef theUID = inSlot->dev[theDir].deviceUID;
        if((theUID == NULL) || !CFEqual(theUID, inUIDs[theDir]))
        {
            return false;
        }
    }
    return true;
}

static void AudioHub_HandleBindSet(AudioHubSlot* inSlot, const AudioHubBindMsg* inMsg)
{
    const uint32_t theSlotIndex = inSlot->dev[0].slotIndex;
    if((inSlot->state == kSlotDelisted) || (inSlot->state == kSlotRetiring))
    {
        // Mid-retirement. Restate where the slot is and let the daemon re-send
        // once it sees Free; binding on top of a record the HAL has not let go of
        // is how a live client ends up holding the next peer's audio.
        AudioHub_PostBindState(inSlot, kAudioHubSlot_Delisted);
        return;
    }

    if(!AudioHub_IsWellFormedUID(inMsg->out_uid, sizeof(inMsg->out_uid)) ||
       !AudioHub_IsWellFormedUID(inMsg->in_uid, sizeof(inMsg->in_uid)))
    {
        os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "refused a bind for slot %u: a UID is empty or does not "
                                                        "start with \"" kAudioHubUIDPrefix "\"",
                     theSlotIndex);
        return;
    }

    if(inSlot->state == kSlotBound)
    {
        CFStringRef theUIDs[kAudioHubDevsPerSlot];
        CFStringRef theNames[kAudioHubDevsPerSlot];
        theUIDs[kAudioHubDir_Out]  = AudioHub_CopyWireString(inMsg->out_uid, sizeof(inMsg->out_uid));
        theUIDs[kAudioHubDir_In]   = AudioHub_CopyWireString(inMsg->in_uid, sizeof(inMsg->in_uid));
        theNames[kAudioHubDir_Out] = AudioHub_CopyWireString(inMsg->out_name, sizeof(inMsg->out_name));
        theNames[kAudioHubDir_In]  = AudioHub_CopyWireString(inMsg->in_name, sizeof(inMsg->in_name));
        const Boolean theWellFormed =
            (theUIDs[0] != NULL) && (theUIDs[1] != NULL) && (theNames[0] != NULL) && (theNames[1] != NULL);
        Boolean theSameUIDs = false;
        if(theWellFormed)
        {
            pthread_mutex_lock(&gPlugIn_StateMutex);
            theSameUIDs = AudioHub_SlotHasUIDsLocked(inSlot, theUIDs);
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theSameUIDs)
            {
                AudioHub_RenameSlot(inSlot, theNames);
            }
        }
        for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
        {
            if(theUIDs[theDir] != NULL) CFRelease(theUIDs[theDir]);
            if(theNames[theDir] != NULL) CFRelease(theNames[theDir]);
        }
        if(!theWellFormed)
        {
            // A malformed message must not be able to tear down a working
            // binding: "the strings did not parse" and "this is a different
            // peer" are the same value of theSameUIDs, and treating the first as
            // the second would let one bad Bind remove a device that is in use.
            os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "ignored a malformed re-bind for slot %u; the existing "
                                                            "binding is untouched",
                         theSlotIndex);
            return;
        }
        if(!theSameUIDs)
        {
            // A different peer for a slot that already has one. NEVER edited in
            // place: an app holding this device's AudioObjectID would quietly
            // start driving another machine. Retire it properly instead and let
            // the daemon re-bind once the slot reports Free.
            os_log(OS_LOG_DEFAULT, kAudioHubDriverLog "slot %u rebound to a different peer; retiring it first",
                   theSlotIndex);
            AudioHub_DelistSlot(inSlot);
            return;
        }
        // Idempotent hit — the case that makes a daemon restart cost nothing:
        // same ids, same UIDs, the user's chosen default device untouched, and
        // the daemon relearns the IO and volume state it lost.
        AudioHub_PublishSlotRings(inSlot);
        AudioHub_PostBindState(inSlot, kAudioHubSlot_Bound);
        AudioHub_ReplaySlotState(inSlot);
        return;
    }

    if(!AudioHub_BindSlot(inSlot, inMsg))
    {
        os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "refused a bind for slot %u: bad strings, a duplicate UID, "
                                                        "or no object ids left",
                     theSlotIndex);
        AudioHub_PostBindState(inSlot, kAudioHubSlot_Free);
        return;
    }
    AudioHub_ReplaySlotState(inSlot);
}

// Service thread, from the transport, once the message's shape, its sender and
// its session have all been checked there.
static void AudioHub_HandleBind(const AudioHubBindMsg* inMsg)
{
    if(inMsg->slot >= kAudioHubMaxSlots)
    {
        os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "refused a bind for slot %u: only %u slots exist",
                     inMsg->slot, kAudioHubMaxSlots);
        return;
    }
    AudioHubSlot* theSlot = &gSlots[inMsg->slot];
    if(inMsg->op == kAudioHubBind_Set)
    {
        AudioHub_HandleBindSet(theSlot, inMsg);
        return;
    }
    if(inMsg->op != kAudioHubBind_Clear)
    {
        os_log_error(OS_LOG_DEFAULT, kAudioHubDriverLog "refused an unknown bind op %u for slot %u",
                     inMsg->op, inMsg->slot);
        return;
    }
    if(theSlot->state != kSlotBound)
    {
        // Already on its way out, or never bound. Restate where it is so a daemon
        // that missed a BindState converges anyway.
        AudioHub_PostBindState(theSlot,
                               (theSlot->state == kSlotFree) ? kAudioHubSlot_Free : kAudioHubSlot_Delisted);
        return;
    }
    if(inMsg->generation != theSlot->generation)
    {
        // A Clear built before the slot changed hands. Obeying it would tear down
        // a binding its sender has never heard of.
        os_log(OS_LOG_DEFAULT, kAudioHubDriverLog "ignored a clear for slot %u at generation %u (now %u)",
               inMsg->slot, inMsg->generation, theSlot->generation);
        return;
    }
    AudioHub_DelistSlot(theSlot);
}

// Control plane (plan §7.2 reverse direction): the peer's real device changed, so
// the virtual control must report the new value. Runs on the bridge thread.
// Deliberately does NOT call bridge_volume_changed — echoing a value we were just
// handed is exactly the ping-pong the source tagging is meant to prevent.
static void AudioHub_ApplyDaemonVolume(uint32_t inEndpoint, uint32_t inGeneration, float inScalar, int inMuted)
{
    if(inEndpoint >= kAudioHubMaxEndpoints)
    {
        return;
    }
    AudioHubSlot* theSlot = &gSlots[AUDIOHUB_ENDPOINT_SLOT(inEndpoint)];
    if((theSlot->state != kSlotBound) || (inGeneration != theSlot->generation))
    {
        // A volume for a peer that no longer owns this slot. Applying it would
        // move a different machine's slider.
        return;
    }
    AudioHubDevice* theDevice = &theSlot->dev[AUDIOHUB_ENDPOINT_DIR(inEndpoint)];
    const Float32 theScalar = AudioHub_ClampScalar(inScalar);
    const Boolean theMuted = (inMuted != 0);

    Boolean theVolumeChanged = false;
    Boolean theMuteChanged = false;
    pthread_mutex_lock(&gPlugIn_StateMutex);
    if(theDevice->volumeScalar != theScalar)
    {
        theDevice->volumeScalar = theScalar;
        theVolumeChanged = true;
    }
    if(theDevice->muted != theMuted)
    {
        theDevice->muted = theMuted;
        theMuteChanged = true;
    }
    // Snapshotted WITH the values rather than re-read after the unlock. Handing
    // coreaudiod an id that has since been retired is how this function would
    // have broken once ids stopped being compile-time constants; the generation
    // re-check below is what makes that impossible instead of merely unlikely.
    // (Retirement runs on this same thread today, so the check cannot fail — it
    // is there so that stays true if that ever stops being so.)
    const AudioObjectID theVolumeID = AudioHub_ID(&theDevice->volumeID);
    const AudioObjectID theMuteID = AudioHub_ID(&theDevice->muteID);
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    if((gPlugIn_Host == NULL) || (theSlot->generation != inGeneration))
    {
        return;
    }
    if(theVolumeChanged)
    {
        AudioObjectPropertyAddress theAddresses[2];
        theAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
        theAddresses[0].mScope    = kAudioObjectPropertyScopeGlobal;
        theAddresses[0].mElement  = kAudioObjectPropertyElementMain;
        theAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
        theAddresses[1].mScope    = kAudioObjectPropertyScopeGlobal;
        theAddresses[1].mElement  = kAudioObjectPropertyElementMain;
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, theVolumeID, 2, theAddresses);
    }
    if(theMuteChanged)
    {
        AudioObjectPropertyAddress theAddress;
        theAddress.mSelector = kAudioBooleanControlPropertyValue;
        theAddress.mScope    = kAudioObjectPropertyScopeGlobal;
        theAddress.mElement  = kAudioObjectPropertyElementMain;
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, theMuteID, 1, &theAddress);
    }
}

// ---------------------------------------------------------------- bridge hooks

static void AudioHub_BridgeAttached(void)
{
    for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
    {
        if(gSlots[theSlotIndex].state == kSlotBound)
        {
            AudioHub_PublishSlotRings(&gSlots[theSlotIndex]);
        }
    }
}

static void AudioHub_BridgeDetached(void)
{
    // Nothing to undo. The transport has already unpublished every ring, and the
    // bindings deliberately survive: removing sixteen devices and putting them
    // back on every daemon restart would discard the user's chosen default output
    // each time, silently (spec-m5b §5.7).
}

static void AudioHub_BridgeTick(void)
{
    AudioHub_RetireDueSlots();
    if(atomic_exchange(&gDeviceListDirty, 0) != 0)
    {
        AudioHub_AnnounceDeviceList();
    }
}

static const AudioHubBridgeHooks gBridgeHooks =
{
    .attached        = AudioHub_BridgeAttached,
    .detached        = AudioHub_BridgeDetached,
    .bind            = AudioHub_HandleBind,
    .notify_volume   = AudioHub_ApplyDaemonVolume,
    .flush           = AudioHub_FlushOutbox,
    .tick            = AudioHub_BridgeTick,
    .io_running_mask = AudioHub_IORunningMask
};

// ---------------------------------------------------------------- helpers

static AudioObjectPropertyScope AudioHub_DeviceScope(const AudioHubDevice* inDevice)
{
    return inDevice->isInput ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
}

static Boolean AudioHub_ScopeMatchesDevice(const AudioHubDevice* inDevice, AudioObjectPropertyScope inScope)
{
    return (inScope == kAudioObjectPropertyScopeGlobal) || (inScope == AudioHub_DeviceScope(inDevice));
}

static void AudioHub_FillStreamFormat(const AudioHubDevice* inDevice, AudioStreamBasicDescription* outFormat)
{
    outFormat->mSampleRate       = inDevice->sampleRate;
    outFormat->mFormatID         = kAudioFormatLinearPCM;
    outFormat->mFormatFlags      = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    outFormat->mBytesPerPacket   = sizeof(Float32) * inDevice->channelCount;
    outFormat->mFramesPerPacket  = 1;
    outFormat->mBytesPerFrame    = sizeof(Float32) * inDevice->channelCount;
    outFormat->mChannelsPerFrame = inDevice->channelCount;
    outFormat->mBitsPerChannel   = 32;
    outFormat->mReserved         = 0;
}

static Float32 AudioHub_ClampScalar(Float32 inValue)
{
    if(inValue < 0.0f) return 0.0f;
    if(inValue > 1.0f) return 1.0f;
    return inValue;
}

// NullAudio-style squared taper between scalar and dB.
static Float32 AudioHub_ScalarToDecibels(Float32 inScalar)
{
    Float32 theValue = AudioHub_ClampScalar(inScalar);
    theValue *= theValue;
    return kVolume_MinDB + (theValue * (kVolume_MaxDB - kVolume_MinDB));
}

static Float32 AudioHub_DecibelsToScalar(Float32 inDecibels)
{
    Float32 theValue = inDecibels;
    if(theValue < kVolume_MinDB) theValue = kVolume_MinDB;
    if(theValue > kVolume_MaxDB) theValue = kVolume_MaxDB;
    theValue = (theValue - kVolume_MinDB) / (kVolume_MaxDB - kVolume_MinDB);
    return sqrtf(theValue);
}

// ---------------------------------------------------------------- entry points

static HRESULT  AudioHubDriver_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface);
static ULONG    AudioHubDriver_AddRef(void* inDriver);
static ULONG    AudioHubDriver_Release(void* inDriver);
static OSStatus AudioHubDriver_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost);
static OSStatus AudioHubDriver_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID);
static OSStatus AudioHubDriver_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID);
static OSStatus AudioHubDriver_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus AudioHubDriver_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus AudioHubDriver_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static OSStatus AudioHubDriver_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static Boolean  AudioHubDriver_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus AudioHubDriver_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus AudioHubDriver_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus AudioHubDriver_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus AudioHubDriver_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData);
static OSStatus AudioHubDriver_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus AudioHubDriver_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus AudioHubDriver_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed);
static OSStatus AudioHubDriver_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace);
static OSStatus AudioHubDriver_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);
static OSStatus AudioHubDriver_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer);
static OSStatus AudioHubDriver_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);

static AudioServerPlugInDriverInterface gAudioServerPlugInDriverInterface =
{
    NULL,
    AudioHubDriver_QueryInterface,
    AudioHubDriver_AddRef,
    AudioHubDriver_Release,
    AudioHubDriver_Initialize,
    AudioHubDriver_CreateDevice,
    AudioHubDriver_DestroyDevice,
    AudioHubDriver_AddDeviceClient,
    AudioHubDriver_RemoveDeviceClient,
    AudioHubDriver_PerformDeviceConfigurationChange,
    AudioHubDriver_AbortDeviceConfigurationChange,
    AudioHubDriver_HasProperty,
    AudioHubDriver_IsPropertySettable,
    AudioHubDriver_GetPropertyDataSize,
    AudioHubDriver_GetPropertyData,
    AudioHubDriver_SetPropertyData,
    AudioHubDriver_StartIO,
    AudioHubDriver_StopIO,
    AudioHubDriver_GetZeroTimeStamp,
    AudioHubDriver_WillDoIOOperation,
    AudioHubDriver_BeginIOOperation,
    AudioHubDriver_DoIOOperation,
    AudioHubDriver_EndIOOperation
};
static AudioServerPlugInDriverInterface* gAudioServerPlugInDriverInterfacePtr = &gAudioServerPlugInDriverInterface;
static AudioServerPlugInDriverRef        gAudioServerPlugInDriverRef          = &gAudioServerPlugInDriverInterfacePtr;

// CFPlugIn factory, registered by name for kAudioHubDriver_FactoryUUIDString in Info.plist.
void* AudioHubDriver_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID);
void* AudioHubDriver_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID)
{
    (void)inAllocator;
    void* theAnswer = NULL;
    if((inRequestedTypeUUID != NULL) && CFEqual(inRequestedTypeUUID, kAudioServerPlugInTypeUUID))
    {
        theAnswer = (void*)gAudioServerPlugInDriverRef;
    }
    return theAnswer;
}

// ---------------------------------------------------------------- COM plumbing

static HRESULT AudioHubDriver_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface)
{
    if((inDriver != gAudioServerPlugInDriverRef) || (outInterface == NULL))
    {
        return (HRESULT)kAudioHardwareIllegalOperationError;
    }
    CFUUIDRef theRequestedUUID = CFUUIDCreateFromUUIDBytes(NULL, inUUID);
    if(theRequestedUUID == NULL)
    {
        return (HRESULT)kAudioHardwareIllegalOperationError;
    }
    HRESULT theAnswer = 0;
    if(CFEqual(theRequestedUUID, IUnknownUUID) || CFEqual(theRequestedUUID, kAudioServerPlugInDriverInterfaceUUID))
    {
        pthread_mutex_lock(&gPlugIn_StateMutex);
        ++gPlugIn_RefCount;
        pthread_mutex_unlock(&gPlugIn_StateMutex);
        *outInterface = (LPVOID)gAudioServerPlugInDriverRef;
    }
    else
    {
        theAnswer = E_NOINTERFACE;
    }
    CFRelease(theRequestedUUID);
    return theAnswer;
}

static ULONG AudioHubDriver_AddRef(void* inDriver)
{
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return 0;
    }
    pthread_mutex_lock(&gPlugIn_StateMutex);
    if(gPlugIn_RefCount < UINT32_MAX)
    {
        ++gPlugIn_RefCount;
    }
    ULONG theAnswer = gPlugIn_RefCount;
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    return theAnswer;
}

static ULONG AudioHubDriver_Release(void* inDriver)
{
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return 0;
    }
    pthread_mutex_lock(&gPlugIn_StateMutex);
    if(gPlugIn_RefCount > 0)
    {
        // the object is static; the count never triggers deallocation
        --gPlugIn_RefCount;
    }
    ULONG theAnswer = gPlugIn_RefCount;
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    return theAnswer;
}

// ---------------------------------------------------------------- lifecycle

static OSStatus AudioHubDriver_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost)
{
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    gPlugIn_Host = inHost;
    AudioHub_InitSlots();

    // Starts the private bridge thread. It never fails the initialization: a
    // daemon that is missing (or arrives an hour later) must leave a plug-in
    // that loads and publishes nothing, not one that refuses to load.
    AudioHubBridge_Start(&gBridgeHooks);

    // LAST. gPlugIn_Host is live several lines above and the bridge thread is
    // already running, so a daemon partway up its Hello retry ladder can get a
    // Bind in before this function has even returned — and announcing a device
    // list before the host has finished registering the plug-in object is a
    // race with no upside. Binds that arrive first only change state; this flag
    // plus the dirty bit make the first service tick announce them.
    atomic_store(&gHostReady, 1);
    atomic_store(&gDeviceListDirty, 1);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID)
{
    // transport-manager-only entry point
    (void)inDescription;
    (void)inClientInfo;
    (void)outDeviceObjectID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AudioHubDriver_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID)
{
    (void)inDeviceObjectID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AudioHubDriver_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo)
{
    (void)inClientInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

// Called by the HAL AFTER the device has left the device list, which is the
// whole reason retirement is two-phase: a delisted device still answers this
// with noErr, exactly as it did while listed.
static OSStatus AudioHubDriver_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo)
{
    (void)inClientInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo)
{
    // Sample rate is fixed at 48kHz in the scaffold, so no change actions are
    // ever requested; keep the timing recompute so the mechanism stays honest.
    (void)inChangeAction;
    (void)inChangeInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    pthread_mutex_lock(&theDevice->ioMutex);
    AudioHub_InitDeviceTiming(theDevice);
    pthread_mutex_unlock(&theDevice->ioMutex);
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo)
{
    (void)inChangeAction;
    (void)inChangeInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------- plugin properties

static Boolean AudioHub_HasPlugInProperty(const AudioObjectPropertyAddress* inAddress)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
        case kAudioPlugInPropertyTranslateUIDToDevice:
        case kAudioPlugInPropertyResourceBundle:
            return true;
        default:
            return false;
    }
}

static OSStatus AudioHub_GetPlugInPropertyDataSize(const AudioObjectPropertyAddress* inAddress, UInt32* outDataSize)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyManufacturer:
        case kAudioPlugInPropertyResourceBundle:
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
            // Read under the lock, like the data path. A set that changes between
            // this call and the following data call is benign and self-healing:
            // both a truncated list and a smaller outDataSize are legal, and any
            // change is followed by a PropertiesChanged that makes the client
            // read again. Double buffering would add an ABA problem to solve a
            // problem that is not there.
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *outDataSize = gDeviceListCount * (UInt32)sizeof(AudioObjectID);
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            break;
        case kAudioPlugInPropertyTranslateUIDToDevice:
            *outDataSize = sizeof(AudioObjectID);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetPlugInPropertyData(const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioObjectClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioPlugInClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            if(inDataSize < sizeof(AudioObjectID)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectID*)outData) = kAudioObjectUnknown;
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyManufacturer:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = CFRetain(CFSTR("AudioHub"));
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
        {
            UInt32 theNumberItemsToFetch = inDataSize / (UInt32)sizeof(AudioObjectID);
            pthread_mutex_lock(&gPlugIn_StateMutex);
            if(theNumberItemsToFetch > gDeviceListCount)
            {
                theNumberItemsToFetch = gDeviceListCount;
            }
            memcpy(outData, gDeviceList, theNumberItemsToFetch * sizeof(AudioObjectID));
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = theNumberItemsToFetch * (UInt32)sizeof(AudioObjectID);
            break;
        }
        case kAudioPlugInPropertyTranslateUIDToDevice:
        {
            if(inQualifierDataSize != sizeof(CFStringRef) || inQualifierData == NULL)
            {
                return kAudioHardwareBadPropertySizeError;
            }
            if(inDataSize < sizeof(AudioObjectID)) return kAudioHardwareBadPropertySizeError;
            CFStringRef theUID = *((CFStringRef*)inQualifierData);
            AudioObjectID theDeviceID = kAudioObjectUnknown;
            // THE ENTRY POINT NOBODY SEES FAIL. This is how an app that
            // remembered a device (Zoom, OBS, Logic) and how coreaudiod restoring
            // the default device turn a stored UID back into an object, and
            // AudioHardwareBase.h says a miss answers kAudioObjectUnknown rather
            // than an error — so leaving it hard-coded to a vanished pair would
            // have been completely silent, and no probe that selects devices by
            // NAME would ever have caught it.
            if(theUID != NULL)
            {
                pthread_mutex_lock(&gPlugIn_StateMutex);
                for(uint32_t theSlotIndex = 0;
                    (theDeviceID == kAudioObjectUnknown) && (theSlotIndex < kAudioHubMaxSlots); ++theSlotIndex)
                {
                    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
                    {
                        AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
                        // Listed devices only: a delisted one is on its way out
                        // of the system, and handing its id back would let an app
                        // re-select a device that is about to stop existing.
                        if(theDevice->listed && (theDevice->deviceUID != NULL) &&
                           CFEqual(theUID, theDevice->deviceUID))
                        {
                            theDeviceID = AudioHub_ID(&theDevice->deviceID);
                            break;
                        }
                    }
                }
                pthread_mutex_unlock(&gPlugIn_StateMutex);
            }
            *((AudioObjectID*)outData) = theDeviceID;
            *outDataSize = sizeof(AudioObjectID);
            break;
        }
        case kAudioPlugInPropertyResourceBundle:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = CFRetain(CFSTR(""));
            *outDataSize = sizeof(CFStringRef);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------- device properties

static Boolean AudioHub_HasDeviceProperty(const AudioObjectPropertyAddress* inAddress)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioObjectPropertyControlList:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyRelatedDevices:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertyStreams:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyNominalSampleRate:
        case kAudioDevicePropertyAvailableNominalSampleRates:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyPreferredChannelsForStereo:
        case kAudioDevicePropertyPreferredChannelLayout:
        case kAudioDevicePropertyZeroTimeStampPeriod:
            return true;
        default:
            return false;
    }
}

static OSStatus AudioHub_IsDevicePropertySettable(const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
    switch(inAddress->mSelector)
    {
        case kAudioDevicePropertyNominalSampleRate:
            *outIsSettable = true;
            break;
        default:
            if(!AudioHub_HasDeviceProperty(inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            *outIsSettable = false;
            break;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetDevicePropertyDataSize(const AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32* outDataSize)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
            *outDataSize = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? (3 * sizeof(AudioObjectID)) : 0;
            break;
        case kAudioObjectPropertyControlList:
            // Scope-gated like OwnedObjects just above. It used to be an
            // unconditional 2, which meant asking an output device for its
            // INPUT-scope control list got two output controls back.
            *outDataSize = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? (2 * sizeof(AudioObjectID)) : 0;
            break;
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyZeroTimeStampPeriod:
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyRelatedDevices:
            // BOTH devices of this slot. This is the only native way macOS has of
            // saying "these two are the same machine", and it is what makes Audio
            // MIDI Setup group a peer's speaker and microphone together.
            *outDataSize = kAudioHubDevsPerSlot * sizeof(AudioObjectID);
            break;
        case kAudioDevicePropertyStreams:
            *outDataSize = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? sizeof(AudioObjectID) : 0;
            break;
        case kAudioDevicePropertyNominalSampleRate:
            *outDataSize = sizeof(Float64);
            break;
        case kAudioDevicePropertyAvailableNominalSampleRates:
            *outDataSize = sizeof(AudioValueRange);
            break;
        case kAudioDevicePropertyPreferredChannelsForStereo:
            *outDataSize = 2 * sizeof(UInt32);
            break;
        case kAudioDevicePropertyPreferredChannelLayout:
            *outDataSize = (UInt32)(offsetof(AudioChannelLayout, mChannelDescriptions) + (inDevice->channelCount * sizeof(AudioChannelDescription)));
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetDevicePropertyData(AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioObjectClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioDeviceClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            if(inDataSize < sizeof(AudioObjectID)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectID*)outData) = kObjectID_PlugIn;
            *outDataSize = sizeof(AudioObjectID);
            break;
        // THE THREE +1 GETTERS. The HAL releases the CFStringRef it is handed, so
        // these hand back a retained one. Under the old fixed pair every string
        // was a CFSTR constant, which is immune to an over-release and therefore
        // proved nothing; now they are heap strings owned by the device record and
        // a rename releases the old one while a client may still be holding it.
        // libASPL retains in its getters and BlackHole hands out the +1 from
        // CFStringCreateWithFormat, which is the same rule stated twice. The
        // retain happens INSIDE the lock: outside it, a rename could free the
        // string between the read and the retain.
        case kAudioObjectPropertyName:
        {
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            CFStringRef theName = (inDevice->deviceName != NULL) ? CFRetain(inDevice->deviceName) : NULL;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theName == NULL) return kAudioHardwareBadObjectError;
            *((CFStringRef*)outData) = theName;
            *outDataSize = sizeof(CFStringRef);
            break;
        }
        case kAudioObjectPropertyManufacturer:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = CFRetain(CFSTR("AudioHub"));
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        {
            UInt32 theNumberItems = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? 3 : 0;
            UInt32 theNumberItemsToFetch = inDataSize / (UInt32)sizeof(AudioObjectID);
            if(theNumberItemsToFetch > theNumberItems)
            {
                theNumberItemsToFetch = theNumberItems;
            }
            AudioObjectID* theList = (AudioObjectID*)outData;
            if(theNumberItemsToFetch > 0) theList[0] = AudioHub_ID(&inDevice->streamID);
            if(theNumberItemsToFetch > 1) theList[1] = AudioHub_ID(&inDevice->volumeID);
            if(theNumberItemsToFetch > 2) theList[2] = AudioHub_ID(&inDevice->muteID);
            *outDataSize = theNumberItemsToFetch * (UInt32)sizeof(AudioObjectID);
            break;
        }
        case kAudioObjectPropertyControlList:
        {
            UInt32 theNumberItems = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? 2 : 0;
            UInt32 theNumberItemsToFetch = inDataSize / (UInt32)sizeof(AudioObjectID);
            if(theNumberItemsToFetch > theNumberItems)
            {
                theNumberItemsToFetch = theNumberItems;
            }
            AudioObjectID* theList = (AudioObjectID*)outData;
            if(theNumberItemsToFetch > 0) theList[0] = AudioHub_ID(&inDevice->volumeID);
            if(theNumberItemsToFetch > 1) theList[1] = AudioHub_ID(&inDevice->muteID);
            *outDataSize = theNumberItemsToFetch * (UInt32)sizeof(AudioObjectID);
            break;
        }
        case kAudioDevicePropertyDeviceUID:
        {
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            CFStringRef theUID = (inDevice->deviceUID != NULL) ? CFRetain(inDevice->deviceUID) : NULL;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theUID == NULL) return kAudioHardwareBadObjectError;
            *((CFStringRef*)outData) = theUID;
            *outDataSize = sizeof(CFStringRef);
            break;
        }
        case kAudioDevicePropertyModelUID:
        {
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            CFStringRef theModelUID = (inDevice->modelUID != NULL) ? CFRetain(inDevice->modelUID) : NULL;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theModelUID == NULL) return kAudioHardwareBadObjectError;
            *((CFStringRef*)outData) = theModelUID;
            *outDataSize = sizeof(CFStringRef);
            break;
        }
        case kAudioDevicePropertyTransportType:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = kAudioDeviceTransportTypeVirtual;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyRelatedDevices:
        {
            // Both halves of the pair, self included. The sibling is found by
            // structure (same slot, other direction), never by arithmetic on an
            // object id.
            UInt32 theNumberItemsToFetch = inDataSize / (UInt32)sizeof(AudioObjectID);
            if(theNumberItemsToFetch > kAudioHubDevsPerSlot)
            {
                theNumberItemsToFetch = kAudioHubDevsPerSlot;
            }
            AudioHubSlot* theSlot = &gSlots[inDevice->slotIndex];
            AudioObjectID* theList = (AudioObjectID*)outData;
            for(UInt32 theIndex = 0; theIndex < theNumberItemsToFetch; ++theIndex)
            {
                theList[theIndex] = AudioHub_ID(&theSlot->dev[theIndex].deviceID);
            }
            *outDataSize = theNumberItemsToFetch * (UInt32)sizeof(AudioObjectID);
            break;
        }
        case kAudioDevicePropertyClockDomain:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 0;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyDeviceIsAlive:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 1;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyDeviceIsRunning:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&inDevice->ioMutex);
            *((UInt32*)outData) = (inDevice->ioRunning > 0) ? 1 : 0;
            pthread_mutex_unlock(&inDevice->ioMutex);
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 1;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 0;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyStreams:
        {
            UInt32 theNumberItems = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? 1 : 0;
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
            if(theNumberItemsToFetch > theNumberItems)
            {
                theNumberItemsToFetch = theNumberItems;
            }
            if(theNumberItemsToFetch > 0)
            {
                ((AudioObjectID*)outData)[0] = AudioHub_ID(&inDevice->streamID);
            }
            *outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
            break;
        }
        case kAudioDevicePropertyNominalSampleRate:
            if(inDataSize < sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *((Float64*)outData) = inDevice->sampleRate;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(Float64);
            break;
        case kAudioDevicePropertyAvailableNominalSampleRates:
        {
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioValueRange);
            if(theNumberItemsToFetch > 1)
            {
                theNumberItemsToFetch = 1;
            }
            if(theNumberItemsToFetch > 0)
            {
                AudioValueRange* theRange = (AudioValueRange*)outData;
                theRange->mMinimum = kDevice_SampleRate;
                theRange->mMaximum = kDevice_SampleRate;
            }
            *outDataSize = theNumberItemsToFetch * sizeof(AudioValueRange);
            break;
        }
        case kAudioDevicePropertyIsHidden:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 0;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyPreferredChannelsForStereo:
            if(inDataSize < (2 * sizeof(UInt32))) return kAudioHardwareBadPropertySizeError;
            ((UInt32*)outData)[0] = 1;
            ((UInt32*)outData)[1] = (inDevice->channelCount > 1) ? 2 : 1;
            *outDataSize = 2 * sizeof(UInt32);
            break;
        case kAudioDevicePropertyPreferredChannelLayout:
        {
            UInt32 theACLSize = (UInt32)(offsetof(AudioChannelLayout, mChannelDescriptions) + (inDevice->channelCount * sizeof(AudioChannelDescription)));
            if(inDataSize < theACLSize) return kAudioHardwareBadPropertySizeError;
            AudioChannelLayout* theACL = (AudioChannelLayout*)outData;
            theACL->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
            theACL->mChannelBitmap = 0;
            theACL->mNumberChannelDescriptions = inDevice->channelCount;
            for(UInt32 theIndex = 0; theIndex < inDevice->channelCount; ++theIndex)
            {
                AudioChannelDescription* theDescription = &theACL->mChannelDescriptions[theIndex];
                theDescription->mChannelFlags = 0;
                theDescription->mCoordinates[0] = 0.0f;
                theDescription->mCoordinates[1] = 0.0f;
                theDescription->mCoordinates[2] = 0.0f;
                if(inDevice->channelCount == 1)
                {
                    theDescription->mChannelLabel = kAudioChannelLabel_Mono;
                }
                else
                {
                    theDescription->mChannelLabel = (theIndex == 0) ? kAudioChannelLabel_Left : kAudioChannelLabel_Right;
                }
            }
            *outDataSize = theACLSize;
            break;
        }
        case kAudioDevicePropertyZeroTimeStampPeriod:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = kDevice_RingFrameCount;
            *outDataSize = sizeof(UInt32);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_SetDevicePropertyData(AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, const void* inData)
{
    (void)inDevice;
    switch(inAddress->mSelector)
    {
        case kAudioDevicePropertyNominalSampleRate:
        {
            if(inDataSize != sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
            Float64 theNewSampleRate = *((const Float64*)inData);
            // fixed-rate scaffold: only the current rate is accepted, so no
            // RequestDeviceConfigurationChange round-trip is ever needed here
            if(theNewSampleRate != kDevice_SampleRate)
            {
                return kAudioHardwareIllegalOperationError;
            }
            break;
        }
        default:
            if(!AudioHub_HasDeviceProperty(inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            return kAudioHardwareUnsupportedOperationError;
    }
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------- stream properties

static Boolean AudioHub_HasStreamProperty(const AudioObjectPropertyAddress* inAddress)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        // mandatory on every AudioObject even when the list is empty; omitting it
        // makes coreaudiod log property errors for this object
        case kAudioObjectPropertyOwnedObjects:
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyPhysicalFormat:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            return true;
        default:
            return false;
    }
}

static OSStatus AudioHub_IsStreamPropertySettable(const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
    switch(inAddress->mSelector)
    {
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            *outIsSettable = true;
            break;
        default:
            if(!AudioHub_HasStreamProperty(inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            *outIsSettable = false;
            break;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetStreamPropertyDataSize(const AudioObjectPropertyAddress* inAddress, UInt32* outDataSize)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyOwnedObjects:
            // a stream owns nothing: empty list, not an unknown property
            *outDataSize = 0;
            break;
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            *outDataSize = sizeof(AudioStreamBasicDescription);
            break;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            *outDataSize = sizeof(AudioStreamRangedDescription);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetStreamPropertyData(AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioObjectClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = kAudioStreamClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            if(inDataSize < sizeof(AudioObjectID)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectID*)outData) = AudioHub_ID(&inDevice->deviceID);
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyOwnedObjects:
            // empty list: report zero bytes written and touch nothing in outData
            *outDataSize = 0;
            break;
        case kAudioStreamPropertyIsActive:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *((UInt32*)outData) = inDevice->streamIsActive ? 1 : 0;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyDirection:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = inDevice->isInput ? 1 : 0;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyTerminalType:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = inDevice->isInput ? kAudioStreamTerminalTypeMicrophone : kAudioStreamTerminalTypeSpeaker;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyStartingChannel:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 1;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyLatency:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = 0;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            if(inDataSize < sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            AudioHub_FillStreamFormat(inDevice, (AudioStreamBasicDescription*)outData);
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(AudioStreamBasicDescription);
            break;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
        {
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioStreamRangedDescription);
            if(theNumberItemsToFetch > 1)
            {
                theNumberItemsToFetch = 1;
            }
            if(theNumberItemsToFetch > 0)
            {
                AudioStreamRangedDescription* theDescription = (AudioStreamRangedDescription*)outData;
                AudioHub_FillStreamFormat(inDevice, &theDescription->mFormat);
                theDescription->mSampleRateRange.mMinimum = kDevice_SampleRate;
                theDescription->mSampleRateRange.mMaximum = kDevice_SampleRate;
            }
            *outDataSize = theNumberItemsToFetch * sizeof(AudioStreamRangedDescription);
            break;
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_SetStreamPropertyData(AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
    switch(inAddress->mSelector)
    {
        case kAudioStreamPropertyIsActive:
        {
            if(inDataSize != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            Boolean theNewIsActive = (*((const UInt32*)inData) != 0);
            pthread_mutex_lock(&gPlugIn_StateMutex);
            if(inDevice->streamIsActive != theNewIsActive)
            {
                inDevice->streamIsActive = theNewIsActive;
                *outNumberPropertiesChanged = 1;
                outChangedAddresses[0].mSelector = kAudioStreamPropertyIsActive;
                outChangedAddresses[0].mScope    = kAudioObjectPropertyScopeGlobal;
                outChangedAddresses[0].mElement  = kAudioObjectPropertyElementMain;
            }
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            break;
        }
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
        {
            if(inDataSize != sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
            const AudioStreamBasicDescription* theNewFormat = (const AudioStreamBasicDescription*)inData;
            AudioStreamBasicDescription theCurrentFormat;
            AudioHub_FillStreamFormat(inDevice, &theCurrentFormat);
            if((theNewFormat->mFormatID != theCurrentFormat.mFormatID) ||
               (theNewFormat->mFormatFlags != theCurrentFormat.mFormatFlags) ||
               (theNewFormat->mSampleRate != theCurrentFormat.mSampleRate) ||
               (theNewFormat->mBytesPerPacket != theCurrentFormat.mBytesPerPacket) ||
               (theNewFormat->mFramesPerPacket != theCurrentFormat.mFramesPerPacket) ||
               (theNewFormat->mBytesPerFrame != theCurrentFormat.mBytesPerFrame) ||
               (theNewFormat->mChannelsPerFrame != theCurrentFormat.mChannelsPerFrame) ||
               (theNewFormat->mBitsPerChannel != theCurrentFormat.mBitsPerChannel))
            {
                return kAudioDeviceUnsupportedFormatError;
            }
            // identical to the one and only supported format: nothing to change
            break;
        }
        default:
            if(!AudioHub_HasStreamProperty(inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            return kAudioHardwareUnsupportedOperationError;
    }
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------- control properties

static Boolean AudioHub_HasControlProperty(ObjectKind inKind, const AudioObjectPropertyAddress* inAddress)
{
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        // mandatory on every AudioObject even when the list is empty; omitting it
        // makes coreaudiod log property errors for this object
        case kAudioObjectPropertyOwnedObjects:
        case kAudioControlPropertyScope:
        case kAudioControlPropertyElement:
            return true;
        case kAudioLevelControlPropertyScalarValue:
        case kAudioLevelControlPropertyDecibelValue:
        case kAudioLevelControlPropertyDecibelRange:
        case kAudioLevelControlPropertyConvertScalarToDecibels:
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            return inKind == kObjectKind_VolumeControl;
        case kAudioBooleanControlPropertyValue:
            return inKind == kObjectKind_MuteControl;
        default:
            return false;
    }
}

static OSStatus AudioHub_IsControlPropertySettable(ObjectKind inKind, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
    switch(inAddress->mSelector)
    {
        case kAudioLevelControlPropertyScalarValue:
        case kAudioLevelControlPropertyDecibelValue:
            if(inKind != kObjectKind_VolumeControl) return kAudioHardwareUnknownPropertyError;
            *outIsSettable = true;
            break;
        case kAudioBooleanControlPropertyValue:
            if(inKind != kObjectKind_MuteControl) return kAudioHardwareUnknownPropertyError;
            *outIsSettable = true;
            break;
        default:
            if(!AudioHub_HasControlProperty(inKind, inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            *outIsSettable = false;
            break;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetControlPropertyDataSize(ObjectKind inKind, const AudioObjectPropertyAddress* inAddress, UInt32* outDataSize)
{
    if(!AudioHub_HasControlProperty(inKind, inAddress))
    {
        return kAudioHardwareUnknownPropertyError;
    }
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyOwnedObjects:
            // a control owns nothing: empty list, not an unknown property
            *outDataSize = 0;
            break;
        case kAudioControlPropertyScope:
            *outDataSize = sizeof(AudioObjectPropertyScope);
            break;
        case kAudioControlPropertyElement:
            *outDataSize = sizeof(AudioObjectPropertyElement);
            break;
        case kAudioLevelControlPropertyScalarValue:
        case kAudioLevelControlPropertyDecibelValue:
        case kAudioLevelControlPropertyConvertScalarToDecibels:
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            *outDataSize = sizeof(Float32);
            break;
        case kAudioLevelControlPropertyDecibelRange:
            *outDataSize = sizeof(AudioValueRange);
            break;
        case kAudioBooleanControlPropertyValue:
            *outDataSize = sizeof(UInt32);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_GetControlPropertyData(ObjectKind inKind, AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
    if(!AudioHub_HasControlProperty(inKind, inAddress))
    {
        return kAudioHardwareUnknownPropertyError;
    }
    switch(inAddress->mSelector)
    {
        case kAudioObjectPropertyBaseClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = (inKind == kObjectKind_VolumeControl) ? kAudioLevelControlClassID : kAudioBooleanControlClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyClass:
            if(inDataSize < sizeof(AudioClassID)) return kAudioHardwareBadPropertySizeError;
            *((AudioClassID*)outData) = (inKind == kObjectKind_VolumeControl) ? kAudioVolumeControlClassID : kAudioMuteControlClassID;
            *outDataSize = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            if(inDataSize < sizeof(AudioObjectID)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectID*)outData) = AudioHub_ID(&inDevice->deviceID);
            *outDataSize = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyOwnedObjects:
            // empty list: report zero bytes written and touch nothing in outData
            *outDataSize = 0;
            break;
        case kAudioControlPropertyScope:
            if(inDataSize < sizeof(AudioObjectPropertyScope)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectPropertyScope*)outData) = AudioHub_DeviceScope(inDevice);
            *outDataSize = sizeof(AudioObjectPropertyScope);
            break;
        case kAudioControlPropertyElement:
            if(inDataSize < sizeof(AudioObjectPropertyElement)) return kAudioHardwareBadPropertySizeError;
            *((AudioObjectPropertyElement*)outData) = kAudioObjectPropertyElementMain;
            *outDataSize = sizeof(AudioObjectPropertyElement);
            break;
        case kAudioLevelControlPropertyScalarValue:
            if(inDataSize < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *((Float32*)outData) = inDevice->volumeScalar;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(Float32);
            break;
        case kAudioLevelControlPropertyDecibelValue:
            if(inDataSize < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *((Float32*)outData) = AudioHub_ScalarToDecibels(inDevice->volumeScalar);
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(Float32);
            break;
        case kAudioLevelControlPropertyDecibelRange:
            if(inDataSize < sizeof(AudioValueRange)) return kAudioHardwareBadPropertySizeError;
            ((AudioValueRange*)outData)->mMinimum = kVolume_MinDB;
            ((AudioValueRange*)outData)->mMaximum = kVolume_MaxDB;
            *outDataSize = sizeof(AudioValueRange);
            break;
        case kAudioLevelControlPropertyConvertScalarToDecibels:
            // the value to convert comes in through outData
            if(inDataSize < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            *((Float32*)outData) = AudioHub_ScalarToDecibels(*((Float32*)outData));
            *outDataSize = sizeof(Float32);
            break;
        case kAudioLevelControlPropertyConvertDecibelsToScalar:
            if(inDataSize < sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            *((Float32*)outData) = AudioHub_DecibelsToScalar(*((Float32*)outData));
            *outDataSize = sizeof(Float32);
            break;
        case kAudioBooleanControlPropertyValue:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            *((UInt32*)outData) = inDevice->muted ? 1 : 0;
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            *outDataSize = sizeof(UInt32);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHub_SetControlPropertyData(ObjectKind inKind, AudioHubDevice* inDevice, const AudioObjectPropertyAddress* inAddress, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
    switch(inAddress->mSelector)
    {
        case kAudioLevelControlPropertyScalarValue:
        case kAudioLevelControlPropertyDecibelValue:
        {
            if(inKind != kObjectKind_VolumeControl) return kAudioHardwareUnknownPropertyError;
            if(inDataSize != sizeof(Float32)) return kAudioHardwareBadPropertySizeError;
            Float32 theNewScalar;
            if(inAddress->mSelector == kAudioLevelControlPropertyScalarValue)
            {
                theNewScalar = AudioHub_ClampScalar(*((const Float32*)inData));
            }
            else
            {
                theNewScalar = AudioHub_DecibelsToScalar(*((const Float32*)inData));
            }
            // snapshot under the lock, notify the daemon after unlocking: the
            // bridge may block, and gPlugIn_StateMutex is process-wide
            Boolean theDidChange = false;
            Float32 theNotifyScalar = 0.0f;
            Boolean theNotifyMuted = false;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            if(inDevice->volumeScalar != theNewScalar)
            {
                inDevice->volumeScalar = theNewScalar;
                theDidChange     = true;
                theNotifyScalar  = inDevice->volumeScalar;
                theNotifyMuted   = inDevice->muted;
                *outNumberPropertiesChanged = 2;
                outChangedAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
                outChangedAddresses[0].mScope    = kAudioObjectPropertyScopeGlobal;
                outChangedAddresses[0].mElement  = kAudioObjectPropertyElementMain;
                outChangedAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
                outChangedAddresses[1].mScope    = kAudioObjectPropertyScopeGlobal;
                outChangedAddresses[1].mElement  = kAudioObjectPropertyElementMain;
            }
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theDidChange)
            {
                bridge_volume_changed(inDevice, theNotifyScalar, theNotifyMuted);
            }
            break;
        }
        case kAudioBooleanControlPropertyValue:
        {
            if(inKind != kObjectKind_MuteControl) return kAudioHardwareUnknownPropertyError;
            if(inDataSize != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            Boolean theNewMuted = (*((const UInt32*)inData) != 0);
            // snapshot under the lock, notify the daemon after unlocking (see above)
            Boolean theDidChange = false;
            Float32 theNotifyScalar = 0.0f;
            Boolean theNotifyMuted = false;
            pthread_mutex_lock(&gPlugIn_StateMutex);
            if(inDevice->muted != theNewMuted)
            {
                inDevice->muted = theNewMuted;
                theDidChange    = true;
                theNotifyScalar = inDevice->volumeScalar;
                theNotifyMuted  = inDevice->muted;
                *outNumberPropertiesChanged = 1;
                outChangedAddresses[0].mSelector = kAudioBooleanControlPropertyValue;
                outChangedAddresses[0].mScope    = kAudioObjectPropertyScopeGlobal;
                outChangedAddresses[0].mElement  = kAudioObjectPropertyElementMain;
            }
            pthread_mutex_unlock(&gPlugIn_StateMutex);
            if(theDidChange)
            {
                bridge_volume_changed(inDevice, theNotifyScalar, theNotifyMuted);
            }
            break;
        }
        default:
            if(!AudioHub_HasControlProperty(inKind, inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            return kAudioHardwareUnsupportedOperationError;
    }
    return kAudioHardwareNoError;
}

// ---------------------------------------------------------------- property dispatch

static Boolean AudioHubDriver_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
    (void)inClientProcessID;
    if((inDriver != gAudioServerPlugInDriverRef) || (inAddress == NULL))
    {
        return false;
    }
    AudioHubDevice* theDevice = NULL;
    const ObjectKind theKind = AudioHub_AcquireObject(inObjectID, &theDevice);
    Boolean theAnswer = false;
    switch(theKind)
    {
        case kObjectKind_PlugIn:        theAnswer = AudioHub_HasPlugInProperty(inAddress);           break;
        case kObjectKind_Device:        theAnswer = AudioHub_HasDeviceProperty(inAddress);           break;
        case kObjectKind_Stream:        theAnswer = AudioHub_HasStreamProperty(inAddress);           break;
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:   theAnswer = AudioHub_HasControlProperty(theKind, inAddress); break;
        default: break;
    }
    if(theDevice != NULL)
    {
        AudioHub_Release(theDevice);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
    (void)inClientProcessID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    if((inAddress == NULL) || (outIsSettable == NULL))
    {
        return kAudioHardwareIllegalOperationError;
    }
    AudioHubDevice* theDevice = NULL;
    const ObjectKind theKind = AudioHub_AcquireObject(inObjectID, &theDevice);
    OSStatus theAnswer;
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            theAnswer = AudioHub_HasPlugInProperty(inAddress) ? kAudioHardwareNoError
                                                             : kAudioHardwareUnknownPropertyError;
            if(theAnswer == kAudioHardwareNoError)
            {
                *outIsSettable = false;
            }
            break;
        case kObjectKind_Device:
            theAnswer = AudioHub_IsDevicePropertySettable(inAddress, outIsSettable);
            break;
        case kObjectKind_Stream:
            theAnswer = AudioHub_IsStreamPropertySettable(inAddress, outIsSettable);
            break;
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            theAnswer = AudioHub_IsControlPropertySettable(theKind, inAddress, outIsSettable);
            break;
        default:
            theAnswer = kAudioHardwareBadObjectError;
            break;
    }
    if(theDevice != NULL)
    {
        AudioHub_Release(theDevice);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
    (void)inClientProcessID;
    (void)inQualifierDataSize;
    (void)inQualifierData;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    if((inAddress == NULL) || (outDataSize == NULL))
    {
        return kAudioHardwareIllegalOperationError;
    }
    AudioHubDevice* theDevice = NULL;
    const ObjectKind theKind = AudioHub_AcquireObject(inObjectID, &theDevice);
    OSStatus theAnswer;
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            theAnswer = AudioHub_GetPlugInPropertyDataSize(inAddress, outDataSize);
            break;
        case kObjectKind_Device:
            theAnswer = AudioHub_GetDevicePropertyDataSize(theDevice, inAddress, outDataSize);
            break;
        case kObjectKind_Stream:
            theAnswer = AudioHub_GetStreamPropertyDataSize(inAddress, outDataSize);
            break;
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            theAnswer = AudioHub_GetControlPropertyDataSize(theKind, inAddress, outDataSize);
            break;
        default:
            theAnswer = kAudioHardwareBadObjectError;
            break;
    }
    if(theDevice != NULL)
    {
        AudioHub_Release(theDevice);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
    (void)inClientProcessID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    if((inAddress == NULL) || (outDataSize == NULL) || (outData == NULL))
    {
        return kAudioHardwareIllegalOperationError;
    }
    AudioHubDevice* theDevice = NULL;
    const ObjectKind theKind = AudioHub_AcquireObject(inObjectID, &theDevice);
    OSStatus theAnswer;
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            theAnswer = AudioHub_GetPlugInPropertyData(inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
            break;
        case kObjectKind_Device:
            theAnswer = AudioHub_GetDevicePropertyData(theDevice, inAddress, inDataSize, outDataSize, outData);
            break;
        case kObjectKind_Stream:
            theAnswer = AudioHub_GetStreamPropertyData(theDevice, inAddress, inDataSize, outDataSize, outData);
            break;
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            theAnswer = AudioHub_GetControlPropertyData(theKind, theDevice, inAddress, inDataSize, outDataSize, outData);
            break;
        default:
            theAnswer = kAudioHardwareBadObjectError;
            break;
    }
    if(theDevice != NULL)
    {
        AudioHub_Release(theDevice);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData)
{
    (void)inClientProcessID;
    (void)inQualifierDataSize;
    (void)inQualifierData;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    if((inAddress == NULL) || (inData == NULL))
    {
        return kAudioHardwareIllegalOperationError;
    }
    OSStatus theAnswer = kAudioHardwareNoError;
    UInt32 theNumberPropertiesChanged = 0;
    AudioObjectPropertyAddress theChangedAddresses[2];
    memset(theChangedAddresses, 0, sizeof(theChangedAddresses));

    AudioHubDevice* theDevice = NULL;
    const ObjectKind theKind = AudioHub_AcquireObject(inObjectID, &theDevice);
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            theAnswer = AudioHub_HasPlugInProperty(inAddress) ? kAudioHardwareUnsupportedOperationError : kAudioHardwareUnknownPropertyError;
            break;
        case kObjectKind_Device:
            theAnswer = AudioHub_SetDevicePropertyData(theDevice, inAddress, inDataSize, inData);
            break;
        case kObjectKind_Stream:
            theAnswer = AudioHub_SetStreamPropertyData(theDevice, inAddress, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
            break;
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            theAnswer = AudioHub_SetControlPropertyData(theKind, theDevice, inAddress, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
            break;
        default:
            theAnswer = kAudioHardwareBadObjectError;
            break;
    }
    if(theDevice != NULL)
    {
        // Released BEFORE the notification: PropertiesChanged reenters this
        // plug-in on another thread, and holding a reference across it would put
        // a retiring slot's spin behind a call that is waiting on us.
        AudioHub_Release(theDevice);
    }
    if((theAnswer == kAudioHardwareNoError) && (theNumberPropertiesChanged > 0) && (gPlugIn_Host != NULL))
    {
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, inObjectID, theNumberPropertiesChanged, theChangedAddresses);
    }
    return theAnswer;
}

// ---------------------------------------------------------------- IO

static OSStatus AudioHubDriver_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    OSStatus theAnswer = kAudioHardwareNoError;
    Boolean theBecameRunning = false;
    pthread_mutex_lock(&theDevice->ioMutex);
    if(theDevice->ioRunning == UINT64_MAX)
    {
        theAnswer = kAudioHardwareIllegalOperationError;
    }
    else if(theDevice->ioRunning == 0)
    {
        theDevice->ioRunning = 1;
        theDevice->timeStampCount = 0;
        theDevice->anchorHostTime = mach_absolute_time();
        theBecameRunning = true;
    }
    else
    {
        ++theDevice->ioRunning;
    }
    pthread_mutex_unlock(&theDevice->ioMutex);
    if(theBecameRunning)
    {
        bridge_io_state_changed(theDevice, true);
    }
    AudioHub_Release(theDevice);
    return theAnswer;
}

static OSStatus AudioHubDriver_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    // THE EDGE THAT CLOSES THE DAEMON'S SESSION. This is reachable for a device
    // that has already left the device list precisely because retirement delists
    // first and invalidates second: invalidate first and this Acquire fails, the
    // ioRunning-reaches-zero edge below never happens, and the daemon keeps a
    // session — and the peer's microphone — open forever.
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    OSStatus theAnswer = kAudioHardwareNoError;
    Boolean theBecameIdle = false;
    pthread_mutex_lock(&theDevice->ioMutex);
    if(theDevice->ioRunning == 0)
    {
        theAnswer = kAudioHardwareIllegalOperationError;
    }
    else
    {
        --theDevice->ioRunning;
        theBecameIdle = (theDevice->ioRunning == 0);
    }
    pthread_mutex_unlock(&theDevice->ioMutex);
    if(theBecameIdle)
    {
        bridge_io_state_changed(theDevice, false);
    }
    AudioHub_Release(theDevice);
    return theAnswer;
}

static OSStatus AudioHubDriver_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    if((outSampleTime == NULL) || (outHostTime == NULL) || (outSeed == NULL))
    {
        AudioHub_Release(theDevice);
        return kAudioHardwareIllegalOperationError;
    }
    pthread_mutex_lock(&theDevice->ioMutex);
    UInt64 theCurrentHostTime = mach_absolute_time();
    Float64 theHostTicksPerRingBuffer = theDevice->hostTicksPerFrame * ((Float64)kDevice_RingFrameCount);
    Float64 theHostTickOffset = ((Float64)(theDevice->timeStampCount + 1)) * theHostTicksPerRingBuffer;
    UInt64 theNextHostTime = theDevice->anchorHostTime + ((UInt64)theHostTickOffset);
    if(theNextHostTime <= theCurrentHostTime)
    {
        ++theDevice->timeStampCount;
    }
    *outSampleTime = (Float64)(theDevice->timeStampCount * kDevice_RingFrameCount);
    *outHostTime = theDevice->anchorHostTime + ((UInt64)(((Float64)theDevice->timeStampCount) * theHostTicksPerRingBuffer));
    *outSeed = 1;
    pthread_mutex_unlock(&theDevice->ioMutex);
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    if((outWillDo == NULL) || (outWillDoInPlace == NULL))
    {
        AudioHub_Release(theDevice);
        return kAudioHardwareIllegalOperationError;
    }
    Boolean theWillDo = false;
    switch(inOperationID)
    {
        case kAudioServerPlugInIOOperationReadInput:
            theWillDo = theDevice->isInput;
            break;
        case kAudioServerPlugInIOOperationWriteMix:
            theWillDo = !theDevice->isInput;
            break;
        default:
            break;
    }
    *outWillDo = theWillDo;
    *outWillDoInPlace = true;
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo)
{
    (void)inClientID;
    (void)inOperationID;
    (void)inIOBufferFrameSize;
    (void)inIOCycleInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer)
{
    (void)inClientID;
    (void)ioSecondaryBuffer;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    // Realtime thread. Everything from here to the Release is lock-free,
    // allocation-free, syscall-free and bounded — a regression here is not an
    // AudioHub glitch, it is a system-wide one.
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    OSStatus theAnswer = kAudioHardwareNoError;
    if(inStreamObjectID != AudioHub_ID(&theDevice->streamID))
    {
        theAnswer = kAudioHardwareBadStreamError;
    }
    else if((inIOCycleInfo == NULL) || (ioMainBuffer == NULL))
    {
        theAnswer = kAudioHardwareIllegalOperationError;
    }
    else
    {
        switch(inOperationID)
        {
            case kAudioServerPlugInIOOperationReadInput:
                if(theDevice->isInput)
                {
                    bridge_read_input(theDevice, (Float32*)ioMainBuffer, inIOBufferFrameSize);
                }
                else
                {
                    theAnswer = kAudioHardwareIllegalOperationError;
                }
                break;
            case kAudioServerPlugInIOOperationWriteMix:
                if(!theDevice->isInput)
                {
                    bridge_write_output(theDevice, (const Float32*)ioMainBuffer, inIOBufferFrameSize);
                }
                else
                {
                    theAnswer = kAudioHardwareIllegalOperationError;
                }
                break;
            default:
                theAnswer = kAudioHardwareIllegalOperationError;
                break;
        }
    }
    AudioHub_Release(theDevice);
    return theAnswer;
}

static OSStatus AudioHubDriver_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo)
{
    (void)inClientID;
    (void)inOperationID;
    (void)inIOBufferFrameSize;
    (void)inIOCycleInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_AcquireByDeviceID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHub_Release(theDevice);
    return kAudioHardwareNoError;
}
