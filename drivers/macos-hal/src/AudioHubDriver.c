//  AudioHubDriver.c — AudioHub virtual-device AudioServerPlugIn.
//  Architecture mirrors Apple's NullAudio.c sample: one static COM object with
//  the full AudioServerPlugInDriverInterface dispatch table, static object tree,
//  mach_absolute_time-based zero timestamps. The audiohubd transport lives in
//  AudioHubBridge.c (spec-round2 §B1): the plug-in registers a mach service,
//  audiohubd connects to it, and the plug-in hands over its two shared-memory
//  rings. With no daemon present the devices stay alive and simply run silent —
//  see the bridge_* wrappers below.
//
//  Object tree (IDs are static, published via kAudioPlugInPropertyDeviceList):
//    1 PlugIn
//    2 Device  "AudioHub Speaker"    (output, 48kHz stereo f32)
//    3   Stream (output)
//    4   Volume control (output scope)
//    5   Mute control   (output scope)
//    6 Device  "AudioHub Microphone" (input, 48kHz mono f32)
//    7   Stream (input)
//    8   Volume control (input scope)
//    9   Mute control   (input scope)

#include "AudioHubBridge.h"

#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreFoundation/CFPlugInCOM.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <math.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

// Must match the CFPlugInFactories key in Info.plist.
#define kAudioHubDriver_FactoryUUIDString "E216324F-6D1C-4B60-9847-A1C501BB479B"

enum
{
    kObjectID_PlugIn            = kAudioObjectPlugInObject,
    kObjectID_Device_Speaker    = 2,
    kObjectID_Stream_Speaker    = 3,
    kObjectID_Volume_Speaker    = 4,
    kObjectID_Mute_Speaker      = 5,
    kObjectID_Device_Microphone = 6,
    kObjectID_Stream_Microphone = 7,
    kObjectID_Volume_Microphone = 8,
    kObjectID_Mute_Microphone   = 9
};

#define kDevice_SampleRate       48000.0
// NullAudio-style zero-timestamp ring: 512-frame period x 32 periods.
#define kDevice_FramesPerPeriod  512u
#define kDevice_RingPeriodCount  32u
#define kDevice_RingFrameCount   (kDevice_FramesPerPeriod * kDevice_RingPeriodCount)

#define kVolume_MinDB (-64.0f)
#define kVolume_MaxDB (0.0f)

typedef struct AudioHubDevice
{
    // fixed identity
    AudioObjectID   deviceID;
    AudioObjectID   streamID;
    AudioObjectID   volumeID;
    AudioObjectID   muteID;
    Boolean         isInput;
    UInt32          channelCount;
    CFStringRef     deviceName;
    CFStringRef     deviceUID;
    CFStringRef     modelUID;

    // mutable state; scalar/mute/streamIsActive guarded by gPlugIn_StateMutex,
    // IO fields guarded by ioMutex
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

static pthread_mutex_t          gPlugIn_StateMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32                   gPlugIn_RefCount   = 0;
static AudioServerPlugInHostRef gPlugIn_Host       = NULL;

static AudioHubDevice gSpeaker =
{
    .deviceID       = kObjectID_Device_Speaker,
    .streamID       = kObjectID_Stream_Speaker,
    .volumeID       = kObjectID_Volume_Speaker,
    .muteID         = kObjectID_Mute_Speaker,
    .isInput        = false,
    // Taken from the ring constant, never spelled out again: AudioHubRing_Write
    // silently writes NOTHING when the header's channel count disagrees with the
    // caller's, so a drift here would look exactly like a dead daemon.
    .channelCount   = AUDIOHUB_SPK_CHANNELS,
    .deviceName     = CFSTR("AudioHub Speaker"),
    .deviceUID      = CFSTR("AudioHubSpeaker_UID"),
    .modelUID       = CFSTR("AudioHubSpeaker_ModelUID"),
    .ioMutex        = PTHREAD_MUTEX_INITIALIZER,
    .ioRunning      = 0,
    .sampleRate     = kDevice_SampleRate,
    .streamIsActive = true,
    .volumeScalar   = 1.0f,
    .muted          = false
};

static AudioHubDevice gMicrophone =
{
    .deviceID       = kObjectID_Device_Microphone,
    .streamID       = kObjectID_Stream_Microphone,
    .volumeID       = kObjectID_Volume_Microphone,
    .muteID         = kObjectID_Mute_Microphone,
    .isInput        = true,
    .channelCount   = AUDIOHUB_MIC_CHANNELS, // see the speaker's note
    .deviceName     = CFSTR("AudioHub Microphone"),
    .deviceUID      = CFSTR("AudioHubMicrophone_UID"),
    .modelUID       = CFSTR("AudioHubMicrophone_ModelUID"),
    .ioMutex        = PTHREAD_MUTEX_INITIALIZER,
    .ioRunning      = 0,
    .sampleRate     = kDevice_SampleRate,
    .streamIsActive = true,
    .volumeScalar   = 1.0f,
    .muted          = false
};

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
// held. The mutex is process-wide across both devices and every property call,
// so anything that could stall under it would wedge the whole plug-in (and thus
// coreaudiod). Callers snapshot the state they need inside the lock, unlock,
// then call the bridge — the same discipline the host's PropertiesChanged()
// call follows. The bridge entry points are themselves non-blocking (mailbox
// posts and atomics only); the constraint stands so that stays a local property
// of this file rather than a promise about someone else's code.

static Float32 AudioHub_ClampScalar(Float32 inValue); // defined with the other helpers below

static UInt32 AudioHub_BridgeDeviceOf(AudioObjectID inDeviceID)
{
    return (inDeviceID == kObjectID_Device_Microphone) ? kAudioHubDevice_Mic : kAudioHubDevice_Speaker;
}

// Speaker direction: hand the mix to the daemon. Discarded when no daemon is
// attached (plan §7.3: the device stays selectable, nothing is processed).
static void bridge_write_output(const Float32* inBuffer, UInt32 inFrameCount, UInt32 inChannelCount, Float64 inSampleTime)
{
    (void)inSampleTime;
    AudioHubBridge_WriteSpeaker(inBuffer, inFrameCount, inChannelCount);
}

// Microphone direction: pull the peer's audio. Short reads and "no daemon" both
// come back as silence, never as a stalled IO cycle.
static void bridge_read_input(Float32* outBuffer, UInt32 inFrameCount, UInt32 inChannelCount, Float64 inSampleTime)
{
    (void)inSampleTime;
    AudioHubBridge_ReadMicrophone(outBuffer, inFrameCount, inChannelCount);
}

// Control plane (plan §7.2 forward direction): the local user moved the virtual
// device's slider, so the daemon must push it to the peer's REAL device. Posts
// to a lock-free mailbox; the bridge thread does the mach send.
static void bridge_volume_changed(AudioObjectID inDeviceID, Float32 inVolumeScalar, Boolean inMuted)
{
    AudioHubBridge_PostVolume(AudioHub_BridgeDeviceOf(inDeviceID), inVolumeScalar, inMuted ? 1 : 0);
}

static void bridge_io_state_changed(AudioObjectID inDeviceID, Boolean inRunning)
{
    AudioHubBridge_PostIOState(AudioHub_BridgeDeviceOf(inDeviceID), inRunning ? 1 : 0);
}

// Control plane (plan §7.2 reverse direction): the peer's real device changed,
// so the virtual control must report the new value. Runs on the bridge thread.
// Deliberately does NOT call bridge_volume_changed — echoing a value we were
// just handed is exactly the ping-pong the source tagging is meant to prevent.
static void AudioHub_ApplyDaemonVolume(uint32_t inDevice, float inScalar, int inMuted)
{
    AudioHubDevice* theDevice = (inDevice == kAudioHubDevice_Mic) ? &gMicrophone : &gSpeaker;
    Float32 theScalar = AudioHub_ClampScalar(inScalar);
    Boolean theMuted = (inMuted != 0);

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
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    if(gPlugIn_Host == NULL)
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
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, theDevice->volumeID, 2, theAddresses);
    }
    if(theMuteChanged)
    {
        AudioObjectPropertyAddress theAddress;
        theAddress.mSelector = kAudioBooleanControlPropertyValue;
        theAddress.mScope    = kAudioObjectPropertyScopeGlobal;
        theAddress.mElement  = kAudioObjectPropertyElementMain;
        gPlugIn_Host->PropertiesChanged(gPlugIn_Host, theDevice->muteID, 1, &theAddress);
    }
}

// ---------------------------------------------------------------- helpers

typedef enum
{
    kObjectKind_Unknown = 0,
    kObjectKind_PlugIn,
    kObjectKind_Device,
    kObjectKind_Stream,
    kObjectKind_VolumeControl,
    kObjectKind_MuteControl
} ObjectKind;

static ObjectKind AudioHub_KindOf(AudioObjectID inObjectID, AudioHubDevice** outOwner)
{
    AudioHubDevice* theOwner = NULL;
    ObjectKind theKind = kObjectKind_Unknown;
    switch(inObjectID)
    {
        case kObjectID_PlugIn:            theKind = kObjectKind_PlugIn;                                   break;
        case kObjectID_Device_Speaker:    theKind = kObjectKind_Device;        theOwner = &gSpeaker;      break;
        case kObjectID_Stream_Speaker:    theKind = kObjectKind_Stream;        theOwner = &gSpeaker;      break;
        case kObjectID_Volume_Speaker:    theKind = kObjectKind_VolumeControl; theOwner = &gSpeaker;      break;
        case kObjectID_Mute_Speaker:      theKind = kObjectKind_MuteControl;   theOwner = &gSpeaker;      break;
        case kObjectID_Device_Microphone: theKind = kObjectKind_Device;        theOwner = &gMicrophone;   break;
        case kObjectID_Stream_Microphone: theKind = kObjectKind_Stream;        theOwner = &gMicrophone;   break;
        case kObjectID_Volume_Microphone: theKind = kObjectKind_VolumeControl; theOwner = &gMicrophone;   break;
        case kObjectID_Mute_Microphone:   theKind = kObjectKind_MuteControl;   theOwner = &gMicrophone;   break;
        default: break;
    }
    if(outOwner != NULL)
    {
        *outOwner = theOwner;
    }
    return theKind;
}

static AudioHubDevice* AudioHub_DeviceByID(AudioObjectID inObjectID)
{
    if(inObjectID == kObjectID_Device_Speaker)
    {
        return &gSpeaker;
    }
    if(inObjectID == kObjectID_Device_Microphone)
    {
        return &gMicrophone;
    }
    return NULL;
}

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

    struct mach_timebase_info theTimeBaseInfo;
    mach_timebase_info(&theTimeBaseInfo);
    Float64 theHostClockFrequency = ((Float64)theTimeBaseInfo.denom / (Float64)theTimeBaseInfo.numer) * 1000000000.0;
    gSpeaker.hostTicksPerFrame    = theHostClockFrequency / gSpeaker.sampleRate;
    gMicrophone.hostTicksPerFrame = theHostClockFrequency / gMicrophone.sampleRate;

    // Starts the private bridge thread. It never fails the initialization: a
    // daemon that is missing (or arrives an hour later) must leave two working,
    // silent devices behind, not a plug-in that refuses to load.
    AudioHubBridge_Start(AudioHub_ApplyDaemonVolume);
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
    if(AudioHub_DeviceByID(inDeviceObjectID) == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo)
{
    (void)inClientInfo;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    if(AudioHub_DeviceByID(inDeviceObjectID) == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
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
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    struct mach_timebase_info theTimeBaseInfo;
    mach_timebase_info(&theTimeBaseInfo);
    Float64 theHostClockFrequency = ((Float64)theTimeBaseInfo.denom / (Float64)theTimeBaseInfo.numer) * 1000000000.0;
    pthread_mutex_lock(&theDevice->ioMutex);
    theDevice->hostTicksPerFrame = theHostClockFrequency / theDevice->sampleRate;
    pthread_mutex_unlock(&theDevice->ioMutex);
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
    if(AudioHub_DeviceByID(inDeviceObjectID) == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
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
            *outDataSize = 2 * sizeof(AudioObjectID);
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
            *((CFStringRef*)outData) = CFSTR("AudioHub");
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
        {
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
            if(theNumberItemsToFetch > 2)
            {
                theNumberItemsToFetch = 2;
            }
            AudioObjectID* theList = (AudioObjectID*)outData;
            if(theNumberItemsToFetch > 0) theList[0] = kObjectID_Device_Speaker;
            if(theNumberItemsToFetch > 1) theList[1] = kObjectID_Device_Microphone;
            *outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
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
            if(theUID != NULL)
            {
                if(CFEqual(theUID, gSpeaker.deviceUID))
                {
                    theDeviceID = gSpeaker.deviceID;
                }
                else if(CFEqual(theUID, gMicrophone.deviceUID))
                {
                    theDeviceID = gMicrophone.deviceID;
                }
            }
            *((AudioObjectID*)outData) = theDeviceID;
            *outDataSize = sizeof(AudioObjectID);
            break;
        }
        case kAudioPlugInPropertyResourceBundle:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = CFSTR("");
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
            *outDataSize = 2 * sizeof(AudioObjectID);
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
            *outDataSize = sizeof(AudioObjectID);
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
        case kAudioObjectPropertyName:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = inDevice->deviceName;
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyManufacturer:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = CFSTR("AudioHub");
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        {
            UInt32 theNumberItems = AudioHub_ScopeMatchesDevice(inDevice, inAddress->mScope) ? 3 : 0;
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
            if(theNumberItemsToFetch > theNumberItems)
            {
                theNumberItemsToFetch = theNumberItems;
            }
            AudioObjectID* theList = (AudioObjectID*)outData;
            if(theNumberItemsToFetch > 0) theList[0] = inDevice->streamID;
            if(theNumberItemsToFetch > 1) theList[1] = inDevice->volumeID;
            if(theNumberItemsToFetch > 2) theList[2] = inDevice->muteID;
            *outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
            break;
        }
        case kAudioObjectPropertyControlList:
        {
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
            if(theNumberItemsToFetch > 2)
            {
                theNumberItemsToFetch = 2;
            }
            AudioObjectID* theList = (AudioObjectID*)outData;
            if(theNumberItemsToFetch > 0) theList[0] = inDevice->volumeID;
            if(theNumberItemsToFetch > 1) theList[1] = inDevice->muteID;
            *outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
            break;
        }
        case kAudioDevicePropertyDeviceUID:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = inDevice->deviceUID;
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioDevicePropertyModelUID:
            if(inDataSize < sizeof(CFStringRef)) return kAudioHardwareBadPropertySizeError;
            *((CFStringRef*)outData) = inDevice->modelUID;
            *outDataSize = sizeof(CFStringRef);
            break;
        case kAudioDevicePropertyTransportType:
            if(inDataSize < sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
            *((UInt32*)outData) = kAudioDeviceTransportTypeVirtual;
            *outDataSize = sizeof(UInt32);
            break;
        case kAudioDevicePropertyRelatedDevices:
        {
            UInt32 theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
            if(theNumberItemsToFetch > 1)
            {
                theNumberItemsToFetch = 1;
            }
            if(theNumberItemsToFetch > 0)
            {
                ((AudioObjectID*)outData)[0] = inDevice->deviceID;
            }
            *outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
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
                ((AudioObjectID*)outData)[0] = inDevice->streamID;
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
            *((AudioObjectID*)outData) = inDevice->deviceID;
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
            *((AudioObjectID*)outData) = inDevice->deviceID;
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
                bridge_volume_changed(inDevice->deviceID, theNotifyScalar, theNotifyMuted);
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
                bridge_volume_changed(inDevice->deviceID, theNotifyScalar, theNotifyMuted);
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
    switch(AudioHub_KindOf(inObjectID, &theDevice))
    {
        case kObjectKind_PlugIn:
            return AudioHub_HasPlugInProperty(inAddress);
        case kObjectKind_Device:
            return AudioHub_HasDeviceProperty(inAddress);
        case kObjectKind_Stream:
            return AudioHub_HasStreamProperty(inAddress);
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            return AudioHub_HasControlProperty(AudioHub_KindOf(inObjectID, NULL), inAddress);
        default:
            return false;
    }
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
    ObjectKind theKind = AudioHub_KindOf(inObjectID, &theDevice);
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            if(!AudioHub_HasPlugInProperty(inAddress))
            {
                return kAudioHardwareUnknownPropertyError;
            }
            *outIsSettable = false;
            return kAudioHardwareNoError;
        case kObjectKind_Device:
            return AudioHub_IsDevicePropertySettable(inAddress, outIsSettable);
        case kObjectKind_Stream:
            return AudioHub_IsStreamPropertySettable(inAddress, outIsSettable);
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            return AudioHub_IsControlPropertySettable(theKind, inAddress, outIsSettable);
        default:
            return kAudioHardwareBadObjectError;
    }
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
    ObjectKind theKind = AudioHub_KindOf(inObjectID, &theDevice);
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            return AudioHub_GetPlugInPropertyDataSize(inAddress, outDataSize);
        case kObjectKind_Device:
            return AudioHub_GetDevicePropertyDataSize(theDevice, inAddress, outDataSize);
        case kObjectKind_Stream:
            return AudioHub_GetStreamPropertyDataSize(inAddress, outDataSize);
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            return AudioHub_GetControlPropertyDataSize(theKind, inAddress, outDataSize);
        default:
            return kAudioHardwareBadObjectError;
    }
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
    ObjectKind theKind = AudioHub_KindOf(inObjectID, &theDevice);
    switch(theKind)
    {
        case kObjectKind_PlugIn:
            return AudioHub_GetPlugInPropertyData(inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
        case kObjectKind_Device:
            return AudioHub_GetDevicePropertyData(theDevice, inAddress, inDataSize, outDataSize, outData);
        case kObjectKind_Stream:
            return AudioHub_GetStreamPropertyData(theDevice, inAddress, inDataSize, outDataSize, outData);
        case kObjectKind_VolumeControl:
        case kObjectKind_MuteControl:
            return AudioHub_GetControlPropertyData(theKind, theDevice, inAddress, inDataSize, outDataSize, outData);
        default:
            return kAudioHardwareBadObjectError;
    }
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
    ObjectKind theKind = AudioHub_KindOf(inObjectID, &theDevice);
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
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
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
        bridge_io_state_changed(theDevice->deviceID, true);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
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
        bridge_io_state_changed(theDevice->deviceID, false);
    }
    return theAnswer;
}

static OSStatus AudioHubDriver_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    if((outSampleTime == NULL) || (outHostTime == NULL) || (outSeed == NULL))
    {
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
    return kAudioHardwareNoError;
}

static OSStatus AudioHubDriver_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace)
{
    (void)inClientID;
    if(inDriver != gAudioServerPlugInDriverRef)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    if((outWillDo == NULL) || (outWillDoInPlace == NULL))
    {
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
    if(AudioHub_DeviceByID(inDeviceObjectID) == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
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
    AudioHubDevice* theDevice = AudioHub_DeviceByID(inDeviceObjectID);
    if(theDevice == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    if(inStreamObjectID != theDevice->streamID)
    {
        return kAudioHardwareBadStreamError;
    }
    if((inIOCycleInfo == NULL) || (ioMainBuffer == NULL))
    {
        return kAudioHardwareIllegalOperationError;
    }
    switch(inOperationID)
    {
        case kAudioServerPlugInIOOperationReadInput:
            if(!theDevice->isInput)
            {
                return kAudioHardwareIllegalOperationError;
            }
            bridge_read_input((Float32*)ioMainBuffer, inIOBufferFrameSize, theDevice->channelCount, inIOCycleInfo->mInputTime.mSampleTime);
            break;
        case kAudioServerPlugInIOOperationWriteMix:
            if(theDevice->isInput)
            {
                return kAudioHardwareIllegalOperationError;
            }
            bridge_write_output((const Float32*)ioMainBuffer, inIOBufferFrameSize, theDevice->channelCount, inIOCycleInfo->mOutputTime.mSampleTime);
            break;
        default:
            return kAudioHardwareIllegalOperationError;
    }
    return kAudioHardwareNoError;
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
    if(AudioHub_DeviceByID(inDeviceObjectID) == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    return kAudioHardwareNoError;
}
