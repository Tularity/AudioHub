// Unit tests for the HAL's scalar <-> dB taper.
//
// The two functions under test are `static`, so this file includes the driver
// translation unit rather than linking against it. That is deliberate: a test
// that re-declared the curve would pass while the shipped curve was wrong, and
// the whole point here is to pin the numbers the driver actually publishes,
// including kVolume_MinDB / kVolume_MaxDB as the driver defines them.
//
// Build and run: drivers/macos-hal/tests/run.sh

#include "../src/AudioHubDriver.c"

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int gFailures = 0;
static int gChecks = 0;

static void ExpectNear(double inActual, double inExpected, double inTolerance, const char* inWhat)
{
    ++gChecks;
    const double theError = fabs(inActual - inExpected);
    if(!(theError <= inTolerance))
    {
        ++gFailures;
        printf("FAIL  %s\n        expected %.6f +/- %.6f, got %.6f (off by %.6f)\n",
               inWhat, inExpected, inTolerance, inActual, theError);
    }
}

static void ExpectTrue(int inCondition, const char* inWhat)
{
    ++gChecks;
    if(!inCondition)
    {
        ++gFailures;
        printf("FAIL  %s\n", inWhat);
    }
}

static AudioObjectID gRelatedNotifications[16];
static UInt32 gRelatedNotificationCount = 0;

static OSStatus TestHostPropertiesChanged(AudioServerPlugInHostRef inHost,
                                          AudioObjectID inObjectID,
                                          UInt32 inNumberAddresses,
                                          const AudioObjectPropertyAddress* inAddresses)
{
    ExpectTrue(inHost == gPlugIn_Host, "RelatedDevices notification uses the current host");
    ExpectTrue(inNumberAddresses == 1, "RelatedDevices notification carries one address");
    ExpectTrue(inAddresses != NULL, "RelatedDevices notification carries an address");
    if((inNumberAddresses == 1) && (inAddresses != NULL))
    {
        ExpectTrue(inAddresses[0].mSelector == kAudioDevicePropertyRelatedDevices,
                   "capability change announces RelatedDevices");
        ExpectTrue(inAddresses[0].mScope == kAudioObjectPropertyScopeGlobal,
                   "RelatedDevices notification uses global scope");
        ExpectTrue(inAddresses[0].mElement == kAudioObjectPropertyElementMain,
                   "RelatedDevices notification uses the main element");
    }

    const int theLockResult = pthread_mutex_trylock(&gPlugIn_StateMutex);
    ExpectTrue(theLockResult == 0, "PropertiesChanged is called outside the HAL state lock");
    if(theLockResult == 0)
    {
        Boolean theObjectIsListed = false;
        for(uint32_t theSlotIndex = 0; theSlotIndex < kAudioHubMaxSlots; ++theSlotIndex)
        {
            for(uint32_t theDir = 0; theDir < kAudioHubDevsPerSlot; ++theDir)
            {
                const AudioHubDevice* theDevice = &gSlots[theSlotIndex].dev[theDir];
                if(theDevice->listed && (AudioHub_ID(&theDevice->deviceID) == inObjectID))
                {
                    theObjectIsListed = true;
                }
            }
        }
        pthread_mutex_unlock(&gPlugIn_StateMutex);
        ExpectTrue(theObjectIsListed, "RelatedDevices notification targets only a currently listed device");
    }

    if(gRelatedNotificationCount < (sizeof(gRelatedNotifications) / sizeof(gRelatedNotifications[0])))
    {
        gRelatedNotifications[gRelatedNotificationCount] = inObjectID;
    }
    ++gRelatedNotificationCount;
    return kAudioHardwareNoError;
}

// Apple's built-in output, as measured in docs/volume-taper-measured.md.
static double AppleReferenceDecibels(double inScalar)
{
    return -63.5 * (1.0 - sqrt(inScalar));
}

// --------------------------------------------------------------- the criteria

// P1 criterion 1: the number a dB-reading app gets at a mid-low slider position.
// Before this change the driver answered -55.00 dB here, which was 30 dB from
// what the same position means on every real device on the machine.
static void TestAnchorPoint(void)
{
    const double theDecibels = AudioHub_ScalarToDecibels(0.375f);
    ExpectNear(theDecibels, -24.61, 0.5, "scalar 0.375 -> dB is within the P1 acceptance band");
    ExpectNear(theDecibels, AppleReferenceDecibels(0.375), 0.01, "scalar 0.375 -> dB matches Apple exactly");
    ExpectTrue(fabs(theDecibels - (-55.0)) > 25.0, "the old squared taper is gone (was -55.00 dB)");
}

// P1 criterion 2: same scalar, same dB as the built-in device, everywhere --
// not just at the one anchor point. Acceptance is 0.5 dB; -63.5 buys us zero.
static void TestAppleParityAcrossRange(void)
{
    double theWorstError = 0.0;
    double theWorstScalar = 0.0;
    for(int theStep = 0; theStep <= 100000; ++theStep)
    {
        const double theScalar = (double)theStep / 100000.0;
        const double theError = fabs((double)AudioHub_ScalarToDecibels((Float32)theScalar) -
                                     AppleReferenceDecibels(theScalar));
        if(theError > theWorstError)
        {
            theWorstError = theError;
            theWorstScalar = theScalar;
        }
    }
    printf("      worst Apple-parity error %.6f dB (at scalar %.5f)\n", theWorstError, theWorstScalar);
    ExpectTrue(theWorstError <= 0.5, "Apple parity within the 0.5 dB acceptance band across the range");
    ExpectTrue(theWorstError <= 0.01, "Apple parity is exact, not merely acceptable");
}

// The published range is what apps use to interpret our dB. It has to be the
// range the curve actually spans, and it has to be Apple's.
static void TestPublishedRange(void)
{
    ExpectNear(kVolume_MinDB, -63.5, 0.0001, "published dB floor is Apple's -63.5");
    ExpectNear(kVolume_MaxDB, 0.0, 0.0001, "published dB ceiling is 0");
    ExpectNear(AudioHub_ScalarToDecibels(0.0f), kVolume_MinDB, 0.0001, "scalar 0 sits on the published floor");
    ExpectNear(AudioHub_ScalarToDecibels(1.0f), kVolume_MaxDB, 0.0001, "scalar 1 sits on the published ceiling");
    ExpectNear(AudioHub_DecibelsToScalar(kVolume_MinDB), 0.0, 0.0001, "published floor maps back to scalar 0");
    ExpectNear(AudioHub_DecibelsToScalar(kVolume_MaxDB), 1.0, 0.0001, "published ceiling maps back to scalar 1");
}

// P0-d: the volume keys step the SCALAR, 1/16 of travel per press. So the taper
// does not change how far a key press moves the slider, only how many dB that
// press is worth. These are the numbers a changelog should quote -- and they are
// the built-in device's numbers, which is the point.
static void TestVolumeKeyStepSizes(void)
{
    const double theTopStep = (double)AudioHub_ScalarToDecibels(16.0f / 16.0f) -
                              (double)AudioHub_ScalarToDecibels(15.0f / 16.0f);
    const double theBottomStep = (double)AudioHub_ScalarToDecibels(1.0f / 16.0f) -
                                 (double)AudioHub_ScalarToDecibels(0.0f / 16.0f);
    printf("      volume-key steps: top %.2f dB, bottom %.2f dB\n", theTopStep, theBottomStep);
    ExpectNear(theTopStep, 2.02, 0.01, "top volume-key step is 2.02 dB (was 7.75)");
    ExpectNear(theBottomStep, 15.88, 0.01, "bottom volume-key step is 15.88 dB (was 0.25)");
    ExpectTrue(theBottomStep > theTopStep, "coarse steps sit at the bottom of the range, not the top");
}

static void TestMonotonicAndClamped(void)
{
    Float32 thePreviousDecibels = -1.0e9f;
    int theMonotonic = 1;
    for(int theStep = 0; theStep <= 100000; ++theStep)
    {
        const Float32 theDecibels = AudioHub_ScalarToDecibels((Float32)theStep / 100000.0f);
        if(theDecibels < thePreviousDecibels) theMonotonic = 0;
        thePreviousDecibels = theDecibels;
    }
    ExpectTrue(theMonotonic, "scalar -> dB is monotonically non-decreasing");

    Float32 thePreviousScalar = -1.0f;
    theMonotonic = 1;
    for(int theStep = 0; theStep <= 100000; ++theStep)
    {
        const Float32 theDecibels = (Float32)(kVolume_MinDB + ((double)theStep / 100000.0) * (kVolume_MaxDB - kVolume_MinDB));
        const Float32 theScalar = AudioHub_DecibelsToScalar(theDecibels);
        if(theScalar < thePreviousScalar) theMonotonic = 0;
        thePreviousScalar = theScalar;
    }
    ExpectTrue(theMonotonic, "dB -> scalar is monotonically non-decreasing");

    ExpectNear(AudioHub_ScalarToDecibels(-5.0f), kVolume_MinDB, 0.0001, "scalar below 0 clamps to the floor");
    ExpectNear(AudioHub_ScalarToDecibels(5.0f), kVolume_MaxDB, 0.0001, "scalar above 1 clamps to the ceiling");
    ExpectNear(AudioHub_DecibelsToScalar(-500.0f), 0.0, 0.0001, "dB below the floor clamps to scalar 0");
    ExpectNear(AudioHub_DecibelsToScalar(60.0f), 1.0, 0.0001, "dB above the ceiling clamps to scalar 1");
}

// A slot always reserves two device records, but capability flags may publish
// either, both, or neither. RelatedDevices must describe the published topology,
// never leak the id of a delisted sibling, and cached relationships on visible
// devices must be invalidated when those flags change.
static void TestRelatedDevicesCapabilityTopology(void)
{
    AudioHub_InitSlots();
    AudioHubSlot* theSlot = &gSlots[0];
    AudioHubDevice* theOutput = &theSlot->dev[kAudioHubDir_Out];
    AudioHubDevice* theInput = &theSlot->dev[kAudioHubDir_In];
    const AudioObjectID theOutputID = 4100;
    const AudioObjectID theInputID = 4200;
    theSlot->state = kSlotBound;
    theSlot->generation = 17;

    pthread_mutex_lock(&gPlugIn_StateMutex);
    atomic_store(&theOutput->deviceID, theOutputID);
    atomic_store(&theInput->deviceID, theInputID);
    atomic_store(&theOutput->live, 1);
    atomic_store(&theInput->live, 1);
    theOutput->listed = true;
    theInput->listed = true;
    AudioHub_RebuildDeviceListLocked();
    pthread_mutex_unlock(&gPlugIn_StateMutex);

    AudioObjectPropertyAddress theAddress;
    theAddress.mSelector = kAudioDevicePropertyRelatedDevices;
    theAddress.mScope    = kAudioObjectPropertyScopeGlobal;
    theAddress.mElement  = kAudioObjectPropertyElementMain;

    UInt32 theDataSize = 0;
    ExpectTrue(AudioHub_GetDevicePropertyDataSize(theOutput, &theAddress, &theDataSize) == kAudioHardwareNoError,
               "RelatedDevices size succeeds for a fully listed pair");
    ExpectTrue(theDataSize == (2 * sizeof(AudioObjectID)), "a fully listed pair reports two related ids");

    AudioObjectID theIDs[2] = { 0, 0 };
    ExpectTrue(AudioHub_GetDevicePropertyData(theOutput, &theAddress, sizeof(theIDs), &theDataSize, theIDs) ==
                   kAudioHardwareNoError,
               "RelatedDevices data succeeds for a fully listed pair");
    ExpectTrue(theDataSize == sizeof(theIDs), "a fully listed pair returns two related ids");
    ExpectTrue((theIDs[0] == theOutputID) && (theIDs[1] == theInputID),
               "the fully listed pair returns output then input ids");

    AudioObjectID thePartial = 0;
    ExpectTrue(AudioHub_GetDevicePropertyData(theOutput, &theAddress, sizeof(thePartial), &theDataSize, &thePartial) ==
                   kAudioHardwareNoError,
               "RelatedDevices supports a one-id caller buffer");
    ExpectTrue((theDataSize == sizeof(AudioObjectID)) && (thePartial == theOutputID),
               "a short RelatedDevices read is truncated without leaking a sibling");

    static const AudioServerPlugInHostInterface kTestHost = {
        .PropertiesChanged = TestHostPropertiesChanged,
    };
    gRelatedNotificationCount = 0;
    memset(gRelatedNotifications, 0, sizeof(gRelatedNotifications));
    gPlugIn_Host = &kTestHost;
    atomic_store(&gHostReady, 1);

    AudioHub_UpdateSlotDirections(theSlot, kAudioHubBindFlag_Out);
    ExpectTrue(gRelatedNotificationCount == 1, "removing one direction notifies the remaining device once");
    ExpectTrue(gRelatedNotifications[0] == theOutputID, "removing input notifies only the listed output");

    ExpectTrue(AudioHub_GetDevicePropertyDataSize(theOutput, &theAddress, &theDataSize) == kAudioHardwareNoError,
               "RelatedDevices size succeeds after one direction is removed");
    ExpectTrue(theDataSize == sizeof(AudioObjectID), "an output-only slot reports one related id");
    theIDs[0] = 0;
    theIDs[1] = 0xFFFFFFFFu;
    ExpectTrue(AudioHub_GetDevicePropertyData(theInput, &theAddress, sizeof(theIDs), &theDataSize, theIDs) ==
                   kAudioHardwareNoError,
               "a delisted sibling still answers RelatedDevices during retirement grace");
    ExpectTrue((theDataSize == sizeof(AudioObjectID)) && (theIDs[0] == theOutputID),
               "a delisted sibling reports only the slot's currently listed output");
    ExpectTrue(theIDs[1] == 0xFFFFFFFFu, "RelatedDevices does not write an unlisted sibling id");

    AudioHub_UpdateSlotDirections(theSlot, kAudioHubBindFlag_Out);
    ExpectTrue(gRelatedNotificationCount == 1, "an idempotent capability update sends no notification");

    AudioHub_UpdateSlotDirections(theSlot, kAudioHubBindFlag_In);
    ExpectTrue(gRelatedNotificationCount == 2, "swapping directions notifies the newly visible topology once");
    ExpectTrue(gRelatedNotifications[1] == theInputID, "an input-only topology notifies only the listed input");

    AudioHub_UpdateSlotDirections(theSlot, kAudioHubBindFlag_Out | kAudioHubBindFlag_In);
    ExpectTrue(gRelatedNotificationCount == 4, "adding the sibling notifies both visible devices");
    ExpectTrue((gRelatedNotifications[2] == theOutputID) && (gRelatedNotifications[3] == theInputID),
               "a restored pair notifies its output and input ids");

    AudioHub_UpdateSlotDirections(theSlot, 0);
    ExpectTrue(gRelatedNotificationCount == 4, "removing every direction does not notify delisted objects");
    ExpectTrue(AudioHub_GetDevicePropertyDataSize(theOutput, &theAddress, &theDataSize) == kAudioHardwareNoError,
               "RelatedDevices size succeeds for a fully delisted slot");
    ExpectTrue(theDataSize == 0, "a fully delisted slot exposes no related ids");

    AudioHub_AnnounceRelatedDevices(theSlot, theSlot->generation - 1, &theOutputID, 1);
    ExpectTrue(gRelatedNotificationCount == 4, "a stale generation cannot notify a recycled object id");
    theSlot->state = kSlotDelisted;
    AudioHub_AnnounceRelatedDevices(theSlot, theSlot->generation, &theOutputID, 1);
    ExpectTrue(gRelatedNotificationCount == 4, "a retiring slot cannot emit RelatedDevices notifications");

    gPlugIn_Host = NULL;
    atomic_store(&gHostReady, 0);
    atomic_store(&gDeviceListDirty, 0);
    pthread_mutex_lock(&gPlugIn_StateMutex);
    theOutput->listed = false;
    theInput->listed = false;
    atomic_store(&theOutput->live, 0);
    atomic_store(&theInput->live, 0);
    atomic_store(&theOutput->deviceID, kAudioObjectUnknown);
    atomic_store(&theInput->deviceID, kAudioObjectUnknown);
    AudioHub_RebuildDeviceListLocked();
    pthread_mutex_unlock(&gPlugIn_StateMutex);
    theSlot->state = kSlotFree;
}

// ------------------------------------------------- exhaustive mutual inversion

// Every float32 in [0, 1] -- all 1,065,353,217 of them -- through
// scalar -> dB -> scalar. Not a sample of the domain, the domain.
//
// The tolerance is ABSOLUTE, and has to be: dB = -63.5 + t*63.5 absorbs any t
// below ~3e-8 into the leading -63.5 (one ulp there is 3.8e-6), so tiny scalars
// come back as 0. That is a 1e-16 absolute error and a 100% relative one, which
// is why a relative bound here would be a bound on floating-point noise rather
// than on the curve.
static void TestExhaustiveScalarRoundTrip(void)
{
    double theWorstError = 0.0;
    float theWorstInput = 0.0f;
    for(uint32_t theBits = 0x00000000u; theBits <= 0x3F800000u; ++theBits)
    {
        float theScalar;
        memcpy(&theScalar, &theBits, sizeof(theScalar));
        const Float32 theRoundTrip = AudioHub_DecibelsToScalar(AudioHub_ScalarToDecibels(theScalar));
        const double theError = fabs((double)theRoundTrip - (double)theScalar);
        if(theError > theWorstError)
        {
            theWorstError = theError;
            theWorstInput = theScalar;
        }
    }
    printf("      exhaustive scalar round-trip: worst error %.3e (at scalar %.9f)\n", theWorstError, theWorstInput);
    ExpectTrue(theWorstError <= 1.0e-6, "scalar -> dB -> scalar is the identity within 1e-6 for every float32 in [0,1]");
}

// Every float32 in [-63.5, 0] through dB -> scalar -> dB.
static void TestExhaustiveDecibelRoundTrip(void)
{
    double theWorstError = 0.0;
    float theWorstInput = 0.0f;
    // 0x80000000 is -0.0f; walking up the negative encodings from there runs
    // from -0.0 down to -63.5 (0xC27E0000).
    for(uint32_t theBits = 0x80000000u; theBits <= 0xC27E0000u; ++theBits)
    {
        float theDecibels;
        memcpy(&theDecibels, &theBits, sizeof(theDecibels));
        if(theDecibels < kVolume_MinDB) continue;
        const Float32 theRoundTrip = AudioHub_ScalarToDecibels(AudioHub_DecibelsToScalar(theDecibels));
        const double theError = fabs((double)theRoundTrip - (double)theDecibels);
        if(theError > theWorstError)
        {
            theWorstError = theError;
            theWorstInput = theDecibels;
        }
    }
    printf("      exhaustive dB round-trip: worst error %.3e dB (at %.6f dB)\n", theWorstError, theWorstInput);
    ExpectTrue(theWorstError <= 1.0e-3, "dB -> scalar -> dB is the identity within 0.001 dB for every float32 in range");
}

// An optional argv[1] runs only the tests whose name contains it, so a bug can
// be injected and exactly the test that should catch it can be run.
int main(int argc, char** argv)
{
    static const struct { const char* name; void (*fn)(void); } kTests[] = {
        { "AnchorPoint",              TestAnchorPoint },
        { "AppleParityAcrossRange",   TestAppleParityAcrossRange },
        { "PublishedRange",           TestPublishedRange },
        { "VolumeKeyStepSizes",       TestVolumeKeyStepSizes },
        { "MonotonicAndClamped",      TestMonotonicAndClamped },
        { "RelatedDevicesCapabilityTopology", TestRelatedDevicesCapabilityTopology },
        { "ExhaustiveScalarRoundTrip",  TestExhaustiveScalarRoundTrip },
        { "ExhaustiveDecibelRoundTrip", TestExhaustiveDecibelRoundTrip },
    };
    const char* theFilter = (argc > 1) ? argv[1] : NULL;

    printf("AudioHub macOS HAL taper tests%s%s\n", theFilter ? " -- filter: " : "", theFilter ? theFilter : "");
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
