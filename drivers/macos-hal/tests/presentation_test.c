// Unit tests for physical presentation (docs/physical-presentation-1.0.2-design.md
// §6.4–§6.5): the driver-side verdict, the property getters it drives, the
// notifications it sends and the ack it posts.
//
// Like taper_test.c this includes the driver translation unit to reach its
// static functions, so what is tested is the code that ships. No host, no
// daemon and no mach traffic: the host is a stub that records notifications,
// and the caller check is passed in explicitly (the strong check itself needs a
// real signed process; it is exercised end to end on the macOS test VM).
//
// Build and run: drivers/macos-hal/tests/run.sh

#include "../src/AudioHubDriver.c"

#include <stdio.h>
#include <string.h>

static int gFailures = 0;
static int gChecks = 0;

static void ExpectTrue(int inCondition, const char* inWhat)
{
    ++gChecks;
    if(!inCondition)
    {
        ++gFailures;
        printf("FAIL  %s\n", inWhat);
    }
}

// ------------------------------------------------------------- host stub

typedef struct Notification
{
    AudioObjectID object;
    AudioObjectPropertySelector selector;
} Notification;

static Notification gNotes[64];
static UInt32 gNoteCount = 0;

static OSStatus TestHostPropertiesChanged(AudioServerPlugInHostRef inHost,
                                          AudioObjectID inObjectID,
                                          UInt32 inNumberAddresses,
                                          const AudioObjectPropertyAddress* inAddresses)
{
    (void)inHost;
    const int theLockResult = pthread_mutex_trylock(&gPlugIn_StateMutex);
    ExpectTrue(theLockResult == 0, "PropertiesChanged is called outside the HAL state lock");
    if(theLockResult == 0)
    {
        pthread_mutex_unlock(&gPlugIn_StateMutex);
    }
    for(UInt32 theIndex = 0; theIndex < inNumberAddresses; ++theIndex)
    {
        ExpectTrue(inAddresses[theIndex].mScope == kAudioObjectPropertyScopeGlobal, "notification uses global scope");
        ExpectTrue(inAddresses[theIndex].mElement == kAudioObjectPropertyElementMain, "notification uses main element");
        if(gNoteCount < (sizeof(gNotes) / sizeof(gNotes[0])))
        {
            gNotes[gNoteCount].object = inObjectID;
            gNotes[gNoteCount].selector = inAddresses[theIndex].mSelector;
        }
        ++gNoteCount;
    }
    return kAudioHardwareNoError;
}

static const AudioServerPlugInHostInterface kTestHost = {
    .PropertiesChanged = TestHostPropertiesChanged,
};

static int Notified(AudioObjectID inObject, AudioObjectPropertySelector inSelector)
{
    for(UInt32 theIndex = 0; (theIndex < gNoteCount) && (theIndex < 64); ++theIndex)
    {
        if((gNotes[theIndex].object == inObject) && (gNotes[theIndex].selector == inSelector))
        {
            return 1;
        }
    }
    return 0;
}

static void ResetNotes(void)
{
    gNoteCount = 0;
    memset(gNotes, 0, sizeof(gNotes));
}

// ------------------------------------------------------------- fixtures

static const AudioObjectID kOutID = 5100, kOutStream = 5101, kInID = 5200, kInStream = 5201;

static void BindFixture(void)
{
    AudioHub_InitSlots();
    AudioHubSlot* theSlot = &gSlots[0];
    theSlot->state = kSlotBound;
    theSlot->generation = 9;
    pthread_mutex_lock(&gPlugIn_StateMutex);
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubDevice* theDevice = &theSlot->dev[theDir];
        atomic_store(&theDevice->deviceID, theDir ? kInID : kOutID);
        atomic_store(&theDevice->streamID, theDir ? kInStream : kOutStream);
        atomic_store(&theDevice->live, 1);
        theDevice->listed = true;
        theDevice->deviceName = CFSTR("AudioHub - WIN-30");
        theDevice->deviceUID = theDir ? CFSTR("AudioHub:0123456789abcdef:in") : CFSTR("AudioHub:0123456789abcdef:out");
        theDevice->modelUID = theDir ? kModelUID_In : kModelUID_Out;
        AudioHub_ResetPresentationLocked(theDevice);
        atomic_store(&theDevice->pres.word, 0);
        atomic_store(&theDevice->pres.seq, 0);
        theDevice->pres.sent = 0;
    }
    AudioHub_RebuildDeviceListLocked();
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    gPlugIn_Host = &kTestHost;
    atomic_store(&gHostReady, 1);
    ResetNotes();
}

static void UnbindFixture(void)
{
    gPlugIn_Host = NULL;
    atomic_store(&gHostReady, 0);
    pthread_mutex_lock(&gPlugIn_StateMutex);
    for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
    {
        AudioHubDevice* theDevice = &gSlots[0].dev[theDir];
        AudioHub_ResetPresentationLocked(theDevice);
        theDevice->listed = false;
        theDevice->deviceName = NULL; // CFSTR constants: never released
        theDevice->deviceUID = NULL;
        theDevice->modelUID = NULL;
        atomic_store(&theDevice->live, 0);
        atomic_store(&theDevice->deviceID, kAudioObjectUnknown);
        atomic_store(&theDevice->streamID, kAudioObjectUnknown);
    }
    AudioHub_RebuildDeviceListLocked();
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    gSlots[0].state = kSlotFree;
}

static AudioHubPresentationMsg PhysicalRequest(uint32_t inEndpoint, uint32_t inTransport, uint32_t inTerminal)
{
    AudioHubPresentationMsg theMsg;
    memset(&theMsg, 0, sizeof(theMsg));
    theMsg.endpoint = inEndpoint;
    theMsg.generation = 9;
    theMsg.klass = kAudioHubPresent_Physical;
    theMsg.transport = inTransport;
    theMsg.terminal_type = inTerminal;
    theMsg.icon_kind = kAudioHubPresentIcon_Headset;
    theMsg.related_group = 7;
    theMsg.request_id = 0x12345;
    snprintf(theMsg.name, sizeof(theMsg.name), "%s", "Jabra Evolve2 65 (WIN-30)");
    snprintf(theMsg.manufacturer, sizeof(theMsg.manufacturer), "%s", "GN Audio A/S");
    snprintf(theMsg.model_suffix, sizeof(theMsg.model_suffix), "%s", "Headset");
    return theMsg;
}

static CFStringRef GetString(AudioHubDevice* inDevice, AudioObjectPropertySelector inSelector)
{
    AudioObjectPropertyAddress theAddress = { inSelector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFStringRef theValue = NULL;
    UInt32 theSize = 0;
    if(AudioHub_GetDevicePropertyData(inDevice, &theAddress, sizeof(theValue), &theSize, &theValue) != kAudioHardwareNoError)
    {
        return NULL;
    }
    return theValue; // +1
}

static int StringIs(AudioHubDevice* inDevice, AudioObjectPropertySelector inSelector, CFStringRef inExpected)
{
    CFStringRef theValue = GetString(inDevice, inSelector);
    const int theSame = (theValue != NULL) && CFEqual(theValue, inExpected);
    if(theValue != NULL) CFRelease(theValue);
    return theSame;
}

static UInt32 GetDeviceU32(AudioHubDevice* inDevice, AudioObjectPropertySelector inSelector)
{
    AudioObjectPropertyAddress theAddress = { inSelector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    UInt32 theValue = 0xDEADBEEFu;
    UInt32 theSize = 0;
    (void)AudioHub_GetDevicePropertyData(inDevice, &theAddress, sizeof(theValue), &theSize, &theValue);
    return theValue;
}

static UInt32 GetTerminal(AudioHubDevice* inDevice)
{
    AudioObjectPropertyAddress theAddress = { kAudioStreamPropertyTerminalType, kAudioObjectPropertyScopeGlobal,
                                              kAudioObjectPropertyElementMain };
    UInt32 theValue = 0xDEADBEEFu;
    UInt32 theSize = 0;
    (void)AudioHub_GetStreamPropertyData(inDevice, &theAddress, sizeof(theValue), &theSize, &theValue);
    return theValue;
}

static UInt32 RelatedCount(AudioHubDevice* inDevice)
{
    AudioObjectPropertyAddress theAddress = { kAudioDevicePropertyRelatedDevices, kAudioObjectPropertyScopeGlobal,
                                              kAudioObjectPropertyElementMain };
    UInt32 theSize = 0;
    (void)AudioHub_GetDevicePropertyDataSize(inDevice, &theAddress, &theSize);
    return theSize / (UInt32)sizeof(AudioObjectID);
}

static void Present(uint32_t inDir, const AudioHubPresentationMsg* inMsg, uint32_t inCallerCheck)
{
    AudioHubSlot* theSlot = &gSlots[0];
    AudioHubPresentation thePresentation;
    AudioHub_EvaluatePresentation(theSlot->dev[inDir].isInput, theSlot->dev[inDir].listed, inCallerCheck, inMsg,
                                  &thePresentation);
    AudioHub_InstallPresentation(theSlot, inDir, theSlot->generation, &thePresentation, inMsg->request_id, true);
}

// ------------------------------------------------------------- tests

// The verdict alone: every rejection rule, and the caller check taking
// precedence over the descriptor.
static void TestEvaluateRules(void)
{
    AudioHubPresentation theP;
    AudioHubPresentationMsg theMsg = PhysicalRequest(0, kAudioDeviceTransportTypeUSB, kAudioStreamTerminalTypeHeadphones);

    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theMsg, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical && !theP.forced, "a verified, valid request is Physical");
    ExpectTrue(theP.transport == kAudioDeviceTransportTypeUSB, "the requested transport is kept");
    ExpectTrue(theP.terminal == kAudioStreamTerminalTypeHeadphones, "the requested terminal type is kept");
    ExpectTrue(theP.modelUID != NULL && CFEqual(theP.modelUID, CFSTR("AudioHub_Remote_Headset")),
               "the ModelUID stays in the AudioHub namespace");
    ExpectTrue(theP.relatedGroup == 7 && theP.icon == kAudioHubPresentIcon_Headset, "group and icon are kept");
    AudioHub_ReleasePresentation(&theP);

    static const uint32_t kCallerReasons[] = {
        kAudioHubPresentReason_CallerNoSession, kAudioHubPresentReason_CallerNoCode,
        kAudioHubPresentReason_CallerPinUnavailable, kAudioHubPresentReason_CallerRequirement,
        kAudioHubPresentReason_CallerPath, kAudioHubPresentReason_CallerNotRuntime,
        kAudioHubPresentReason_CallerInvalid,
    };
    for(size_t theIndex = 0; theIndex < sizeof(kCallerReasons) / sizeof(kCallerReasons[0]); ++theIndex)
    {
        AudioHub_EvaluatePresentation(false, true, kCallerReasons[theIndex], &theMsg, &theP);
        ExpectTrue(theP.klass == kAudioHubPresent_Virtual && theP.forced && theP.reason == kCallerReasons[theIndex],
                   "an unverified caller is forced Virtual with the caller-check reason");
        ExpectTrue(theP.transport == kAudioDeviceTransportTypeVirtual && theP.name == NULL && theP.modelUID == NULL,
                   "a forced Virtual verdict carries no physical field");
    }

    AudioHubPresentationMsg theBad = theMsg;
    theBad.transport = 0;
    theBad.name[0] = '\0';
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_CallerPath, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_CallerPath,
               "the caller check is reported before any descriptor rejection");

    static const uint32_t kRefusedTransports[] = {
        kAudioDeviceTransportTypeVirtual, kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
        kAudioDeviceTransportTypeAirPlay, kAudioDeviceTransportTypeAggregate, 'fgrp' /* AutoAggregate */,
        kAudioDeviceTransportTypeAVB, kAudioDeviceTransportTypeUnknown, 0x12345678u,
    };
    for(size_t theIndex = 0; theIndex < sizeof(kRefusedTransports) / sizeof(kRefusedTransports[0]); ++theIndex)
    {
        theBad = theMsg;
        theBad.transport = kRefusedTransports[theIndex];
        AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
        ExpectTrue(theP.klass == kAudioHubPresent_Virtual && theP.forced &&
                       theP.reason == kAudioHubPresentReason_BadTransport,
                   "a transport outside the whitelist is refused");
    }
    static const uint32_t kAllowedTransports[] = {
        kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeUSB, kAudioDeviceTransportTypeHDMI,
        kAudioDeviceTransportTypeDisplayPort, kAudioDeviceTransportTypeThunderbolt, kAudioDeviceTransportTypePCI,
        kAudioDeviceTransportTypeFireWire,
    };
    for(size_t theIndex = 0; theIndex < sizeof(kAllowedTransports) / sizeof(kAllowedTransports[0]); ++theIndex)
    {
        theBad = theMsg;
        theBad.transport = kAllowedTransports[theIndex];
        AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
        ExpectTrue(theP.klass == kAudioHubPresent_Physical, "a whitelisted transport is accepted");
        AudioHub_ReleasePresentation(&theP);
    }

    theBad = theMsg;
    theBad.terminal_type = kAudioStreamTerminalTypeMicrophone;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadTerminal, "an input terminal type on an output is refused");
    theBad.terminal_type = kAudioStreamTerminalTypeLFESpeaker;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadTerminal, "an LFE speaker is refused");
    theBad.terminal_type = kAudioStreamTerminalTypeReceiverSpeaker;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical, "a receiver speaker (handset) is accepted on an output");
    AudioHub_ReleasePresentation(&theP);
    theBad.terminal_type = kAudioStreamTerminalTypeReceiverMicrophone;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadTerminal, "a receiver microphone is refused on an output");
    theBad.terminal_type = kAudioStreamTerminalTypeHeadsetMicrophone;
    AudioHub_EvaluatePresentation(true, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical && theP.terminal == kAudioStreamTerminalTypeHeadsetMicrophone,
               "a headset microphone is accepted on an input");
    AudioHub_ReleasePresentation(&theP);
    theBad.terminal_type = kAudioStreamTerminalTypeTTY;
    AudioHub_EvaluatePresentation(true, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadTerminal, "a TTY terminal is refused");
    theBad.terminal_type = 0;
    AudioHub_EvaluatePresentation(true, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical && theP.terminal == kAudioStreamTerminalTypeMicrophone,
               "terminal type 0 means the direction's default");
    AudioHub_ReleasePresentation(&theP);

    theBad = theMsg;
    theBad.icon_kind = kAudioHubPresentIcon_Count;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadIcon, "an unknown icon kind is refused");

    theBad = theMsg;
    theBad.name[0] = '\0';
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical && theP.name == NULL,
               "an empty name is accepted and leaves the Bind name in place");
    AudioHub_ReleasePresentation(&theP);
    theBad = theMsg;
    theBad.name[3] = '\n';
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadName, "a control character in the name is refused");
    theBad = theMsg;
    theBad.name[0] = (char)0xC3; theBad.name[1] = (char)0x28; // invalid UTF-8
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadName, "invalid UTF-8 in the name is refused");
    theBad = theMsg;
    memset(theBad.name, 'a', sizeof(theBad.name)); // no terminator: the receiver imposes one
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Physical && CFStringGetLength(theP.name) == 127,
               "an unterminated name is cut at 127 bytes, never read past the field");
    AudioHub_ReleasePresentation(&theP);

    theBad = theMsg;
    theBad.manufacturer[0] = '\0';
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_BadManufacturer && theP.name == NULL,
               "an empty manufacturer is refused and releases the name");

    // The Bind-name path: Physical with an empty name shows the binding's name.
    BindFixture();
    AudioHubPresentationMsg theNoName = theMsg;
    theNoName.name[0] = '\0';
    Present(kAudioHubDir_Out, &theNoName, kAudioHubPresentReason_None);
    ExpectTrue(GetDeviceU32(&gSlots[0].dev[kAudioHubDir_Out], kAudioDevicePropertyTransportType) ==
                   kAudioDeviceTransportTypeUSB &&
                   StringIs(&gSlots[0].dev[kAudioHubDir_Out], kAudioObjectPropertyName, CFSTR("AudioHub - WIN-30")),
               "Physical with an empty name keeps the Bind name");
    UnbindFixture();

    static const char* kBadSuffixes[] = { "", "Head-set", "Head set", "Head/../x", "\xC3\xA9" };
    for(size_t theIndex = 0; theIndex < sizeof(kBadSuffixes) / sizeof(kBadSuffixes[0]); ++theIndex)
    {
        theBad = theMsg;
        memset(theBad.model_suffix, 0, sizeof(theBad.model_suffix));
        snprintf(theBad.model_suffix, sizeof(theBad.model_suffix), "%s", kBadSuffixes[theIndex]);
        AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
        ExpectTrue(theP.reason == kAudioHubPresentReason_BadModel && theP.manufacturer == NULL,
                   "a model suffix outside [A-Za-z0-9]+ is refused");
    }

    theBad = theMsg;
    theBad.klass = 2;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_None, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Virtual && theP.forced && theP.reason == kAudioHubPresentReason_BadClass,
               "an unknown class is refused");

    AudioHub_EvaluatePresentation(false, false, kAudioHubPresentReason_None, &theMsg, &theP);
    ExpectTrue(theP.reason == kAudioHubPresentReason_NotListed, "an unpublished direction cannot be Physical");

    theBad = theMsg;
    theBad.klass = kAudioHubPresent_Virtual;
    theBad.transport = kAudioDeviceTransportTypeBluetooth;
    AudioHub_EvaluatePresentation(false, true, kAudioHubPresentReason_CallerPath, &theBad, &theP);
    ExpectTrue(theP.klass == kAudioHubPresent_Virtual && !theP.forced && theP.reason == kAudioHubPresentReason_None &&
                   theP.transport == kAudioDeviceTransportTypeVirtual,
               "a Virtual request ignores every other field and needs no verification");
}

// What coreaudiod actually sees: the getters, the notifications and the ack.
static void TestInstallDrivesGettersAndAck(void)
{
    BindFixture();
    AudioHubDevice* theOut = &gSlots[0].dev[kAudioHubDir_Out];

    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual,
               "a fresh binding is Virtual");
    ExpectTrue(StringIs(theOut, kAudioObjectPropertyManufacturer, CFSTR("AudioHub")), "Virtual manufacturer is AudioHub");
    ExpectTrue(StringIs(theOut, kAudioDevicePropertyModelUID, CFSTR("AudioHub_PeerOutput")), "Virtual ModelUID unchanged");
    ExpectTrue(GetTerminal(theOut) == kAudioStreamTerminalTypeSpeaker, "Virtual output terminal is Speaker");

    AudioHubPresentationMsg theMsg = PhysicalRequest(0, kAudioDeviceTransportTypeUSB, kAudioStreamTerminalTypeHeadphones);
    Present(kAudioHubDir_Out, &theMsg, kAudioHubPresentReason_None);

    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeUSB,
               "Physical TransportType is the presented one");
    ExpectTrue(StringIs(theOut, kAudioObjectPropertyName, CFSTR("Jabra Evolve2 65 (WIN-30)")), "Physical Name");
    ExpectTrue(StringIs(theOut, kAudioObjectPropertyManufacturer, CFSTR("GN Audio A/S")), "Physical Manufacturer");
    ExpectTrue(StringIs(theOut, kAudioDevicePropertyModelUID, CFSTR("AudioHub_Remote_Headset")), "Physical ModelUID");
    ExpectTrue(GetTerminal(theOut) == kAudioStreamTerminalTypeHeadphones, "Physical stream TerminalType");
    ExpectTrue(StringIs(theOut, kAudioDevicePropertyDeviceUID, CFSTR("AudioHub:0123456789abcdef:out")),
               "DeviceUID never changes");
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyClockDomain) == 0, "ClockDomain stays truthful");
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertySafetyOffset) == 0, "SafetyOffset stays truthful");
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyIsHidden) == 0, "IsHidden stays 0");

    ExpectTrue(Notified(kOutID, kAudioObjectPropertyName) && Notified(kOutID, kAudioObjectPropertyManufacturer) &&
                   Notified(kOutID, kAudioDevicePropertyModelUID) && Notified(kOutID, kAudioDevicePropertyTransportType),
               "every changed device property is announced");
    ExpectTrue(Notified(kOutStream, kAudioStreamPropertyTerminalType), "the stream's TerminalType change is announced");
    ExpectTrue(Notified(kOutID, kAudioDevicePropertyRelatedDevices) && Notified(kInID, kAudioDevicePropertyRelatedDevices),
               "a pair that stops being related announces RelatedDevices on both halves");

    const uint64_t theWord = atomic_load(&theOut->pres.word);
    const uint32_t theFlags = (uint32_t)(theWord >> 32);
    ExpectTrue(atomic_load(&theOut->pres.seq) == 1, "one request posts one ack");
    ExpectTrue((uint32_t)theWord == kAudioDeviceTransportTypeUSB, "the ack carries the EFFECTIVE transport");
    ExpectTrue((theFlags & kAudioHubPresentFlag_Physical) != 0 && (theFlags & kAudioHubPresentFlag_Forced) == 0,
               "the ack says Physical and not forced");
    ExpectTrue((theFlags >> kAudioHubPresentRequestShift) == 0x2345u, "the ack echoes the request id's low 16 bits");
    ExpectTrue((theFlags & kAudioHubFlag_IsInput) == 0, "an output ack has no input flag");
    ExpectTrue((theFlags & kAudioHubPresentFlag_CallerVerified) == 0,
               "with no live session the ack never claims a verified caller");

    // Re-sending the same request changes nothing the HAL can see.
    ResetNotes();
    theMsg.request_id = 0x12346;
    Present(kAudioHubDir_Out, &theMsg, kAudioHubPresentReason_None);
    ExpectTrue(gNoteCount == 0, "an identical presentation sends no notification");
    ExpectTrue((uint32_t)(atomic_load(&theOut->pres.word) >> (32 + kAudioHubPresentRequestShift)) == 0x2346u,
               "but it is still acknowledged");

    // Withdrawal: back to the binding's own record, announced live.
    ResetNotes();
    AudioHubPresentationMsg theVirtual = theMsg;
    theVirtual.klass = kAudioHubPresent_Virtual;
    theVirtual.request_id = 0x12347;
    Present(kAudioHubDir_Out, &theVirtual, kAudioHubPresentReason_None);
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual,
               "withdrawal restores TransportType Virtual");
    ExpectTrue(StringIs(theOut, kAudioObjectPropertyName, CFSTR("AudioHub - WIN-30")), "withdrawal restores the name");
    ExpectTrue(StringIs(theOut, kAudioObjectPropertyManufacturer, CFSTR("AudioHub")), "withdrawal restores the manufacturer");
    ExpectTrue(StringIs(theOut, kAudioDevicePropertyModelUID, CFSTR("AudioHub_PeerOutput")), "withdrawal restores ModelUID");
    ExpectTrue(GetTerminal(theOut) == kAudioStreamTerminalTypeSpeaker, "withdrawal restores the terminal type");
    ExpectTrue(Notified(kOutID, kAudioDevicePropertyTransportType) && Notified(kOutStream, kAudioStreamPropertyTerminalType),
               "withdrawal is announced live");
    ExpectTrue(RelatedCount(theOut) == 2, "a Virtual pair is related again");

    // A refused request from an unverified session is Virtual with the reason.
    ResetNotes();
    theMsg.request_id = 0x12348;
    Present(kAudioHubDir_Out, &theMsg, kAudioHubPresentReason_CallerNotRuntime);
    const uint32_t theForcedFlags = (uint32_t)(atomic_load(&theOut->pres.word) >> 32);
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual,
               "a refused request leaves the device Virtual");
    ExpectTrue((theForcedFlags & kAudioHubPresentFlag_Forced) != 0 && (theForcedFlags & kAudioHubPresentFlag_Physical) == 0,
               "the ack reports forced Virtual");
    ExpectTrue(((theForcedFlags >> kAudioHubPresentReasonShift) & kAudioHubPresentReasonMask) ==
                   kAudioHubPresentReason_CallerNotRuntime,
               "the ack carries the refusal reason");
    ExpectTrue(gNoteCount == 0, "a refusal on an already Virtual device announces nothing");

    UnbindFixture();
}

// RelatedDevices: the browser groupId rule of design §6.1.
static void TestRelatedGroups(void)
{
    BindFixture();
    AudioHubDevice* theOut = &gSlots[0].dev[kAudioHubDir_Out];
    AudioHubDevice* theIn = &gSlots[0].dev[kAudioHubDir_In];
    ExpectTrue(RelatedCount(theOut) == 2 && RelatedCount(theIn) == 2, "a Virtual pair is related");

    AudioHubPresentationMsg theOutMsg = PhysicalRequest(0, kAudioDeviceTransportTypeUSB, kAudioStreamTerminalTypeHeadphones);
    AudioHubPresentationMsg theInMsg = PhysicalRequest(1, kAudioDeviceTransportTypeUSB, kAudioStreamTerminalTypeHeadsetMicrophone);
    Present(kAudioHubDir_Out, &theOutMsg, kAudioHubPresentReason_None);
    ExpectTrue(RelatedCount(theOut) == 1 && RelatedCount(theIn) == 1, "a mixed Physical/Virtual pair is not related");

    ResetNotes();
    Present(kAudioHubDir_In, &theInMsg, kAudioHubPresentReason_None);
    ExpectTrue(RelatedCount(theOut) == 2 && RelatedCount(theIn) == 2, "a Physical pair with one non-zero group is related");
    ExpectTrue(Notified(kOutID, kAudioDevicePropertyRelatedDevices) && Notified(kInID, kAudioDevicePropertyRelatedDevices),
               "becoming related is announced on both halves");
    const uint32_t theInFlags = (uint32_t)(atomic_load(&theIn->pres.word) >> 32);
    ExpectTrue((theInFlags & kAudioHubFlag_IsInput) != 0, "an input ack carries the input flag");

    theInMsg.related_group = 8;
    Present(kAudioHubDir_In, &theInMsg, kAudioHubPresentReason_None);
    ExpectTrue(RelatedCount(theOut) == 1 && RelatedCount(theIn) == 1, "different groups are not related");

    theInMsg.related_group = 0;
    theOutMsg.related_group = 0;
    Present(kAudioHubDir_Out, &theOutMsg, kAudioHubPresentReason_None);
    Present(kAudioHubDir_In, &theInMsg, kAudioHubPresentReason_None);
    ExpectTrue(RelatedCount(theOut) == 1 && RelatedCount(theIn) == 1, "group 0 is never related");

    // The session ends: every endpoint goes back to Virtual, announced, with no
    // ack posted to a daemon that is no longer there.
    ResetNotes();
    const uint64_t theSeqBefore = atomic_load(&theOut->pres.seq);
    AudioHub_ResetAllPresentations();
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual &&
                   GetDeviceU32(theIn, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual,
               "detach returns every endpoint to Virtual");
    ExpectTrue(Notified(kOutID, kAudioDevicePropertyTransportType) && Notified(kInID, kAudioDevicePropertyTransportType),
               "the detach reset is announced");
    ExpectTrue(atomic_load(&theOut->pres.seq) == theSeqBefore, "the detach reset posts no ack");
    ExpectTrue(RelatedCount(theOut) == 2, "after the reset the pair is related again");
    ResetNotes();
    AudioHub_ResetAllPresentations();
    ExpectTrue(gNoteCount == 0, "resetting an all-Virtual slot announces nothing");

    // Withdrawing a direction forgets its presentation.
    Present(kAudioHubDir_In, &theInMsg, kAudioHubPresentReason_None);
    AudioHub_UpdateSlotDirections(&gSlots[0], kAudioHubBindFlag_Out);
    pthread_mutex_lock(&gPlugIn_StateMutex);
    const Boolean theReset = (theIn->presentClass == kAudioHubPresent_Virtual) && (theIn->presentName == NULL);
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    ExpectTrue(theReset, "a withdrawn direction is reset to Virtual");
    AudioHub_UpdateSlotDirections(&gSlots[0], kAudioHubBindFlag_Out | kAudioHubBindFlag_In);

    UnbindFixture();
}

// A stale generation or an unbound slot is ignored without an ack.
static void TestHandleFiltersGeneration(void)
{
    BindFixture();
    AudioHubDevice* theOut = &gSlots[0].dev[kAudioHubDir_Out];
    AudioHubPresentationMsg theMsg = PhysicalRequest(0, kAudioDeviceTransportTypeUSB, 0);
    theMsg.generation = 8;
    AudioHub_HandlePresentation(&theMsg);
    ExpectTrue(atomic_load(&theOut->pres.seq) == 0, "a stale generation is not acknowledged");
    theMsg.generation = 9;
    AudioHub_HandlePresentation(&theMsg);
    ExpectTrue(atomic_load(&theOut->pres.seq) == 1, "the current generation is acknowledged");
    ExpectTrue(GetDeviceU32(theOut, kAudioDevicePropertyTransportType) == kAudioDeviceTransportTypeVirtual,
               "with no verified session the transport handler forces Virtual");
    const uint32_t theFlags = (uint32_t)(atomic_load(&theOut->pres.word) >> 32);
    ExpectTrue(((theFlags >> kAudioHubPresentReasonShift) & kAudioHubPresentReasonMask) ==
                   kAudioHubPresentReason_CallerNoSession,
               "the transport-level handler consults the session's caller check");
    UnbindFixture();
}

int main(int argc, char** argv)
{
    static const struct { const char* name; void (*fn)(void); } kTests[] = {
        { "EvaluateRules",              TestEvaluateRules },
        { "InstallDrivesGettersAndAck", TestInstallDrivesGettersAndAck },
        { "RelatedGroups",              TestRelatedGroups },
        { "HandleFiltersGeneration",    TestHandleFiltersGeneration },
    };
    const char* theFilter = (argc > 1) ? argv[1] : NULL;
    printf("AudioHub macOS HAL presentation tests%s%s\n", theFilter ? " -- filter: " : "", theFilter ? theFilter : "");
    int theRan = 0;
    for(size_t theIndex = 0; theIndex < sizeof(kTests) / sizeof(kTests[0]); ++theIndex)
    {
        if((theFilter != NULL) && (strstr(kTests[theIndex].name, theFilter) == NULL)) continue;
        printf("  - %s\n", kTests[theIndex].name);
        kTests[theIndex].fn();
        ++theRan;
    }
    if(theRan == 0)
    {
        printf("FAIL  filter '%s' matched no test\n", theFilter);
        return 1;
    }
    if(gFailures == 0)
    {
        printf("PASS  %d checks\n", gChecks);
        return 0;
    }
    printf("FAIL  %d of %d checks\n", gFailures, gChecks);
    return 1;
}
