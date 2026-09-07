/*++

Module Name:

    speakerformatbank.h

Abstract:

    Immutable render format-bank selection for per-peer speaker endpoints.

--*/

#ifndef _AUDIOHUB_SPEAKERFORMATBANK_H_
#define _AUDIOHUB_SPEAKERFORMATBANK_H_

#include "common.h"

#define AH_SPEAKER_LAYOUT_STEREO        0u
#define AH_SPEAKER_LAYOUT_5POINT1       1u
#define AH_SPEAKER_LAYOUT_7POINT1       2u
#define AH_SPEAKER_LAYOUT_7POINT1POINT4 3u
#define AH_SPEAKER_LAYOUT_COUNT         4u

#define AH_SPEAKER_LAYOUT_BIT(_Layout) (1u << (_Layout))
#define AH_SPEAKER_LAYOUT_MASK_STEREO \
    AH_SPEAKER_LAYOUT_BIT(AH_SPEAKER_LAYOUT_STEREO)
#define AH_SPEAKER_LAYOUT_MASK_ALL \
    ((1u << AH_SPEAKER_LAYOUT_COUNT) - 1u)
#define AH_SPEAKER_FORMAT_BANK_COUNT   8u

#define AH_SPEAKER_CHANNELS_STEREO        2u
#define AH_SPEAKER_CHANNELS_5POINT1       6u
#define AH_SPEAKER_CHANNELS_7POINT1       8u
#define AH_SPEAKER_CHANNELS_7POINT1POINT4 12u

#define AH_SPEAKER_CHANNEL_MASK_STEREO        0x00000003u
#define AH_SPEAKER_CHANNEL_MASK_5POINT1       0x0000060Fu
#define AH_SPEAKER_CHANNEL_MASK_7POINT1       0x0000063Fu
#define AH_SPEAKER_CHANNEL_MASK_7POINT1POINT4 0x0002D63Fu

typedef struct _AH_SPEAKER_FORMAT_BANK AH_SPEAKER_FORMAT_BANK;

//
// DriverEntry calls this once before the control device or any endpoint can
// expose a bank. It allocates nothing and publishes only fully-built banks.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
VOID AhSpeakerFormatBanksInitialize(VOID);

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN AhSpeakerFormatBankMaskIsValid(_In_ ULONG SupportedLayoutMask);

_IRQL_requires_max_(DISPATCH_LEVEL)
_Ret_maybenull_
const AH_SPEAKER_FORMAT_BANK *
AhSpeakerFormatBankLookup(_In_ ULONG SupportedLayoutMask);

_IRQL_requires_max_(DISPATCH_LEVEL)
_Ret_maybenull_
const PCFILTER_DESCRIPTOR *
AhSpeakerFormatBankWaveDescriptor(_In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank);

_IRQL_requires_max_(DISPATCH_LEVEL)
_Ret_maybenull_
const PIN_DEVICE_FORMATS_AND_MODES *
AhSpeakerFormatBankPinDeviceFormatsAndModes(_In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank);

_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG
AhSpeakerFormatBankPinDeviceFormatsAndModesCount(_In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank);

_IRQL_requires_max_(DISPATCH_LEVEL)
USHORT
AhSpeakerFormatBankMaximumChannels(_In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank);

#endif // _AUDIOHUB_SPEAKERFORMATBANK_H_
