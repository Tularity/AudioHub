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
