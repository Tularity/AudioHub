/*++

Copyright (c) Microsoft Corporation All Rights Reserved

Module Name:

    speakerwavtable.h

Abstract:

    Immutable wave format banks for the render endpoints.

--*/

#ifndef _SIMPLEAUDIOSAMPLE_SPEAKERWAVTABLE_H_
#define _SIMPLEAUDIOSAMPLE_SPEAKERWAVTABLE_H_

#include "speakerformatbank.h"

//
// 48 kHz, 16-bit PCM, stereo -- upstream's format, restored.
//
// This was briefly IEEE float, which made the driver and KS interfaces load
// but prevented AudioEndpointBuilder from creating an endpoint. The WaveRT DPC
// conversion uses the documented extended-processor-state save/restore path
// instead, so the public pin remains PCM.
//
// The currently wired bank remains exactly that v7 format. The wider constants
// below prepare descriptor banks only; they do not advertise a wider endpoint
// until a future transaction selects such a bank.
//
#define SPEAKER_DEVICE_MAX_CHANNELS                 2
#define SPEAKER_HOST_MAX_CHANNELS                   2
#define SPEAKER_HOST_MIN_BITS_PER_SAMPLE            16
#define SPEAKER_HOST_MAX_BITS_PER_SAMPLE            16
#define SPEAKER_HOST_MIN_SAMPLE_RATE                48000
#define SPEAKER_HOST_MAX_SAMPLE_RATE                48000

#define SPEAKER_HOST_BLOCK_ALIGN \
    (SPEAKER_HOST_MAX_CHANNELS * (SPEAKER_HOST_MAX_BITS_PER_SAMPLE / 8))
#define SPEAKER_HOST_AVG_BYTES_PER_SEC \
    (SPEAKER_HOST_MAX_SAMPLE_RATE * SPEAKER_HOST_BLOCK_ALIGN)

C_ASSERT(SPEAKER_HOST_BLOCK_ALIGN == 4);
C_ASSERT(SPEAKER_HOST_AVG_BYTES_PER_SEC == 192000);
C_ASSERT(AH_SPEAKER_LAYOUT_COUNT == 4);
C_ASSERT(AH_SPEAKER_FORMAT_BANK_COUNT == 8);
C_ASSERT(AH_SPEAKER_CHANNEL_MASK_STEREO == KSAUDIO_SPEAKER_STEREO);
C_ASSERT(AH_SPEAKER_CHANNEL_MASK_5POINT1 == KSAUDIO_SPEAKER_5POINT1_SURROUND);
C_ASSERT(AH_SPEAKER_CHANNEL_MASK_7POINT1 == KSAUDIO_SPEAKER_7POINT1_SURROUND);
C_ASSERT(AH_SPEAKER_CHANNEL_MASK_7POINT1POINT4 ==
    (KSAUDIO_SPEAKER_7POINT1_SURROUND |
     SPEAKER_TOP_FRONT_LEFT | SPEAKER_TOP_FRONT_RIGHT |
     SPEAKER_TOP_BACK_LEFT | SPEAKER_TOP_BACK_RIGHT));

#define SPEAKER_MAX_INPUT_SYSTEM_STREAMS            1

//
// This header is included by more than one Main translation unit. Exactly one
// of them defines AUDIOHUB_SPEAKER_FORMAT_BANKS_IMPLEMENTATION so the banks
// have one driver-global address rather than one private copy per include.
//
#if defined(AUDIOHUB_SPEAKER_FORMAT_BANKS_IMPLEMENTATION)

typedef struct _AH_SPEAKER_LAYOUT_SPEC
{
    ULONG  Layout;
    USHORT Channels;
    ULONG  ChannelMask;
} AH_SPEAKER_LAYOUT_SPEC;

struct _AH_SPEAKER_FORMAT_BANK
{
    ULONG SupportedLayoutMask;
    USHORT MaximumChannels;
    const PCFILTER_DESCRIPTOR *WaveDescriptor;
    const PIN_DEVICE_FORMATS_AND_MODES *PinDeviceFormatsAndModes;
    ULONG PinDeviceFormatsAndModesCount;
};

typedef struct _AH_SPEAKER_FORMAT_BANK_STORAGE
{
    AH_SPEAKER_FORMAT_BANK Public;
    KSDATAFORMAT_WAVEFORMATEXTENSIBLE Formats[AH_SPEAKER_LAYOUT_COUNT];
    MODE_AND_DEFAULT_FORMAT Modes[1];
    PIN_DEVICE_FORMATS_AND_MODES PinDeviceFormatsAndModes[2];
    KSDATARANGE_AUDIO StreamRanges[AH_SPEAKER_LAYOUT_COUNT];
    PKSDATARANGE StreamRangePointers[AH_SPEAKER_LAYOUT_COUNT * 2];
    KSDATARANGE BridgeRange;
    PKSDATARANGE BridgeRangePointers[1];
    PCPIN_DESCRIPTOR Pins[2];
    PCCONNECTION_DESCRIPTOR Connections[1];
    PCFILTER_DESCRIPTOR FilterDescriptor;
} AH_SPEAKER_FORMAT_BANK_STORAGE;

static const AH_SPEAKER_LAYOUT_SPEC AhSpeakerLayoutSpecs[AH_SPEAKER_LAYOUT_COUNT] =
{
    { AH_SPEAKER_LAYOUT_STEREO,        AH_SPEAKER_CHANNELS_STEREO,        AH_SPEAKER_CHANNEL_MASK_STEREO },
    { AH_SPEAKER_LAYOUT_5POINT1,       AH_SPEAKER_CHANNELS_5POINT1,       AH_SPEAKER_CHANNEL_MASK_5POINT1 },
    { AH_SPEAKER_LAYOUT_7POINT1,       AH_SPEAKER_CHANNELS_7POINT1,       AH_SPEAKER_CHANNEL_MASK_7POINT1 },
    { AH_SPEAKER_LAYOUT_7POINT1POINT4, AH_SPEAKER_CHANNELS_7POINT1POINT4, AH_SPEAKER_CHANNEL_MASK_7POINT1POINT4 },
};

//
// This is the exact pre-bank stereo format. Every bank starts from this value
// and changes only channel geometry for its additional canonical layouts.
//
static const KSDATAFORMAT_WAVEFORMATEXTENSIBLE AhSpeakerStereoFormatTemplate =
{
    {
        sizeof(KSDATAFORMAT_WAVEFORMATEXTENSIBLE),
        0,
        0,
        0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)
    },
    {
        {
            WAVE_FORMAT_EXTENSIBLE,
            SPEAKER_HOST_MAX_CHANNELS,
            SPEAKER_HOST_MAX_SAMPLE_RATE,
            SPEAKER_HOST_AVG_BYTES_PER_SEC,
            SPEAKER_HOST_BLOCK_ALIGN,
            SPEAKER_HOST_MAX_BITS_PER_SAMPLE,
            sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)
        },
        SPEAKER_HOST_MAX_BITS_PER_SAMPLE,
        KSAUDIO_SPEAKER_STEREO,
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM)
    }
};

static const KSDATARANGE_AUDIO AhSpeakerStreamRangeTemplate =
{
    {
        sizeof(KSDATARANGE_AUDIO),
        KSDATARANGE_ATTRIBUTES,
        0,
        0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)
    },
    SPEAKER_HOST_MAX_CHANNELS,
    SPEAKER_HOST_MIN_BITS_PER_SAMPLE,
    SPEAKER_HOST_MAX_BITS_PER_SAMPLE,
    SPEAKER_HOST_MIN_SAMPLE_RATE,
    SPEAKER_HOST_MAX_SAMPLE_RATE
};

static const KSDATARANGE AhSpeakerBridgeRangeTemplate =
{
    sizeof(KSDATARANGE),
    0,
    0,
    0,
    STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
    STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
    STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE)
};

static const PCPIN_DESCRIPTOR AhSpeakerPinTemplates[] =
{
    {
        SPEAKER_MAX_INPUT_SYSTEM_STREAMS,
        SPEAKER_MAX_INPUT_SYSTEM_STREAMS,
        0,
        NULL,
        {
            0,
            NULL,
            0,
            NULL,
            0,
            NULL,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_SINK,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    },
    {
        0,
        0,
        0,
        NULL,
        {
            0,
            NULL,
            0,
            NULL,
            0,
            NULL,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            NULL,
            0
        }
    },
};

static const PCCONNECTION_DESCRIPTOR AhSpeakerConnectionTemplate =
{
    PCFILTER_NODE,
    KSPIN_WAVE_RENDER3_SINK_SYSTEM,
    PCFILTER_NODE,
    KSPIN_WAVE_RENDER3_SOURCE
};

static PCPROPERTY_ITEM PropertiesSpeakerWaveFilter[] =
{
    {
        &KSPROPSETID_Pin,
        KSPROPERTY_PIN_PROPOSEDATAFORMAT,
        KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_WaveFilter
    },
    {
        &KSPROPSETID_Pin,
        KSPROPERTY_PIN_PROPOSEDATAFORMAT2,
        KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandler_WaveFilter
    }
};

NTSTATUS CMiniportWaveRT_EventHandler_PinCapsChange(_In_ PPCEVENT_REQUEST EventRequest);

static PCEVENT_ITEM EventsSpeakerWaveFilter[] =
{
    { &KSEVENTSETID_PinCapsChange, KSEVENT_PINCAPS_FORMATCHANGE,
      KSEVENT_TYPE_ENABLE | KSEVENT_TYPE_BASICSUPPORT,
      CMiniportWaveRT_EventHandler_PinCapsChange }
};
DEFINE_PCAUTOMATION_TABLE_PROP_EVENT(AutomationSpeakerWaveFilter,
    PropertiesSpeakerWaveFilter, EventsSpeakerWaveFilter);

static AH_SPEAKER_FORMAT_BANK_STORAGE
    AhSpeakerFormatBanks[AH_SPEAKER_FORMAT_BANK_COUNT];
static volatile LONG AhSpeakerFormatBanksState = 0;

#pragma code_seg("PAGE")
static VOID
AhSpeakerFormatBankInitializeOne(
    _Out_ AH_SPEAKER_FORMAT_BANK_STORAGE *Storage,
    _In_  ULONG SupportedLayoutMask
    )
{
    PAGED_CODE();

    RtlZeroMemory(Storage, sizeof(*Storage));

    ULONG formatCount = 0;
    USHORT maximumChannels = 0;

    for (ULONG layout = 0; layout < AH_SPEAKER_LAYOUT_COUNT; layout++)
    {
        if ((SupportedLayoutMask & AH_SPEAKER_LAYOUT_BIT(layout)) == 0)
        {
            continue;
        }

        const AH_SPEAKER_LAYOUT_SPEC *spec = &AhSpeakerLayoutSpecs[layout];
        ASSERT(spec->Layout == layout);
        KSDATAFORMAT_WAVEFORMATEXTENSIBLE *format = &Storage->Formats[formatCount];
        KSDATARANGE_AUDIO *range = &Storage->StreamRanges[formatCount];

        *format = AhSpeakerStereoFormatTemplate;
        format->WaveFormatExt.Format.nChannels = spec->Channels;
        format->WaveFormatExt.Format.nBlockAlign =
            (WORD)(spec->Channels * (SPEAKER_HOST_MAX_BITS_PER_SAMPLE / 8));
        format->WaveFormatExt.Format.nAvgBytesPerSec =
            SPEAKER_HOST_MAX_SAMPLE_RATE * format->WaveFormatExt.Format.nBlockAlign;
        format->WaveFormatExt.dwChannelMask = spec->ChannelMask;

        *range = AhSpeakerStreamRangeTemplate;
        range->MaximumChannels = spec->Channels;
        // KS consumes the following pointer as attributes for this range.
        Storage->StreamRangePointers[formatCount * 2] = (PKSDATARANGE)range;
        Storage->StreamRangePointers[formatCount * 2 + 1] =
            (PKSDATARANGE)&PinDataRangeAttributeList;

        if (spec->Channels > maximumChannels)
        {
            maximumChannels = spec->Channels;
        }
        formatCount++;
    }

    ASSERT(formatCount != 0);
    ASSERT(Storage->Formats[0].WaveFormatExt.Format.nChannels ==
           AH_SPEAKER_CHANNELS_STEREO);

    Storage->Modes[0].Mode = AUDIO_SIGNALPROCESSINGMODE_DEFAULT;
    Storage->Modes[0].DefaultFormat = &Storage->Formats[0].DataFormat;

    Storage->PinDeviceFormatsAndModes[0].PinType = SystemRenderPin;
    Storage->PinDeviceFormatsAndModes[0].WaveFormats = Storage->Formats;
    Storage->PinDeviceFormatsAndModes[0].WaveFormatsCount = formatCount;
    Storage->PinDeviceFormatsAndModes[0].ModeAndDefaultFormat = Storage->Modes;
    Storage->PinDeviceFormatsAndModes[0].ModeAndDefaultFormatCount =
        SIZEOF_ARRAY(Storage->Modes);
    Storage->PinDeviceFormatsAndModes[1].PinType = BridgePin;

    Storage->BridgeRange = AhSpeakerBridgeRangeTemplate;
    Storage->BridgeRangePointers[0] = &Storage->BridgeRange;

    Storage->Pins[0] = AhSpeakerPinTemplates[0];
    Storage->Pins[0].KsPinDescriptor.DataRangesCount = formatCount * 2;
    Storage->Pins[0].KsPinDescriptor.DataRanges = Storage->StreamRangePointers;
    Storage->Pins[1] = AhSpeakerPinTemplates[1];
    Storage->Pins[1].KsPinDescriptor.DataRangesCount =
        SIZEOF_ARRAY(Storage->BridgeRangePointers);
    Storage->Pins[1].KsPinDescriptor.DataRanges = Storage->BridgeRangePointers;

    Storage->Connections[0] = AhSpeakerConnectionTemplate;

    Storage->FilterDescriptor.Version = 0;
    Storage->FilterDescriptor.AutomationTable = &AutomationSpeakerWaveFilter;
    Storage->FilterDescriptor.PinSize = sizeof(PCPIN_DESCRIPTOR);
    Storage->FilterDescriptor.PinCount = SIZEOF_ARRAY(Storage->Pins);
    Storage->FilterDescriptor.Pins = Storage->Pins;
    Storage->FilterDescriptor.NodeSize = sizeof(PCNODE_DESCRIPTOR);
    Storage->FilterDescriptor.NodeCount = 0;
    Storage->FilterDescriptor.Nodes = NULL;
    Storage->FilterDescriptor.ConnectionCount =
        SIZEOF_ARRAY(Storage->Connections);
    Storage->FilterDescriptor.Connections = Storage->Connections;
    Storage->FilterDescriptor.CategoryCount = 0;
    Storage->FilterDescriptor.Categories = NULL;

    Storage->Public.SupportedLayoutMask = SupportedLayoutMask;
    Storage->Public.MaximumChannels = maximumChannels;
    Storage->Public.WaveDescriptor = &Storage->FilterDescriptor;
    Storage->Public.PinDeviceFormatsAndModes = Storage->PinDeviceFormatsAndModes;
    Storage->Public.PinDeviceFormatsAndModesCount =
        SIZEOF_ARRAY(Storage->PinDeviceFormatsAndModes);
}

#pragma code_seg("PAGE")
VOID
AhSpeakerFormatBanksInitialize(VOID)
{
    PAGED_CODE();

    // State 1 reserves the one permitted construction pass; state 2 publishes
    // the complete pointer graphs. A repeated call never rewrites a bank.
    if (InterlockedCompareExchange(&AhSpeakerFormatBanksState, 1, 0) != 0)
    {
        return;
    }

    for (ULONG bank = 0; bank < AH_SPEAKER_FORMAT_BANK_COUNT; bank++)
    {
        const ULONG supportedLayoutMask = (bank << 1) | AH_SPEAKER_LAYOUT_MASK_STEREO;
        AhSpeakerFormatBankInitializeOne(
            &AhSpeakerFormatBanks[bank],
            supportedLayoutMask);
    }

    KeMemoryBarrier();
    InterlockedExchange(&AhSpeakerFormatBanksState, 2);
}

#pragma code_seg()
BOOLEAN
AhSpeakerFormatBankMaskIsValid(
    _In_ ULONG SupportedLayoutMask
    )
{
    return
        (SupportedLayoutMask & AH_SPEAKER_LAYOUT_MASK_STEREO) != 0 &&
        (SupportedLayoutMask & ~AH_SPEAKER_LAYOUT_MASK_ALL) == 0;
}

#pragma code_seg()
const AH_SPEAKER_FORMAT_BANK *
AhSpeakerFormatBankLookup(
    _In_ ULONG SupportedLayoutMask
    )
{
    if (!AhSpeakerFormatBankMaskIsValid(SupportedLayoutMask) ||
        InterlockedCompareExchange(&AhSpeakerFormatBanksState, 0, 0) != 2)
    {
        return NULL;
    }

    const ULONG bank = SupportedLayoutMask >> 1;
    ASSERT(bank < AH_SPEAKER_FORMAT_BANK_COUNT);
    ASSERT(AhSpeakerFormatBanks[bank].Public.SupportedLayoutMask ==
           SupportedLayoutMask);
    return &AhSpeakerFormatBanks[bank].Public;
}

#pragma code_seg()
static BOOLEAN
AhSpeakerFormatBankIsOwned(
    _In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank
    )
{
    if (Bank == NULL ||
        InterlockedCompareExchange(&AhSpeakerFormatBanksState, 0, 0) != 2)
    {
        return FALSE;
    }

    for (ULONG bank = 0; bank < AH_SPEAKER_FORMAT_BANK_COUNT; bank++)
    {
        if (Bank == &AhSpeakerFormatBanks[bank].Public)
        {
            return TRUE;
        }
    }
    return FALSE;
}

#pragma code_seg()
const PCFILTER_DESCRIPTOR *
AhSpeakerFormatBankWaveDescriptor(
    _In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank
    )
{
    return AhSpeakerFormatBankIsOwned(Bank) ? Bank->WaveDescriptor : NULL;
}

#pragma code_seg()
const PIN_DEVICE_FORMATS_AND_MODES *
AhSpeakerFormatBankPinDeviceFormatsAndModes(
    _In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank
    )
{
    return AhSpeakerFormatBankIsOwned(Bank) ?
        Bank->PinDeviceFormatsAndModes : NULL;
}

#pragma code_seg()
ULONG
AhSpeakerFormatBankPinDeviceFormatsAndModesCount(
    _In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank
    )
{
    return AhSpeakerFormatBankIsOwned(Bank) ?
        Bank->PinDeviceFormatsAndModesCount : 0;
}

#pragma code_seg()
USHORT
AhSpeakerFormatBankMaximumChannels(
    _In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank
    )
{
    return AhSpeakerFormatBankIsOwned(Bank) ? Bank->MaximumChannels : 0;
}

#pragma code_seg()
ULONG AhSpeakerLayoutFromChannels(_In_ ULONG Channels)
{
    for (ULONG layout = 0; layout < AH_SPEAKER_LAYOUT_COUNT; ++layout)
    {
        if (AhSpeakerLayoutSpecs[layout].Channels == Channels) { return layout; }
    }
    return AH_SPEAKER_LAYOUT_COUNT;
}

#pragma code_seg()
ULONG AhSpeakerLayoutChannelMask(_In_ ULONG Layout)
{
    return Layout < AH_SPEAKER_LAYOUT_COUNT ? AhSpeakerLayoutSpecs[Layout].ChannelMask : 0;
}

#pragma code_seg()
const KSDATAFORMAT_WAVEFORMATEXTENSIBLE *
AhSpeakerFormatBankFormat(_In_opt_ const AH_SPEAKER_FORMAT_BANK *Bank, _In_ ULONG Layout)
{
    if (!AhSpeakerFormatBankIsOwned(Bank) || Layout >= AH_SPEAKER_LAYOUT_COUNT ||
        !(Bank->SupportedLayoutMask & AH_SPEAKER_LAYOUT_BIT(Layout))) { return NULL; }
    const PIN_DEVICE_FORMATS_AND_MODES *pin = Bank->PinDeviceFormatsAndModes;
    for (ULONG i = 0; i < pin[0].WaveFormatsCount; ++i)
    {
        const KSDATAFORMAT_WAVEFORMATEXTENSIBLE *format = &pin[0].WaveFormats[i];
        if (format->WaveFormatExt.Format.nChannels == AhSpeakerLayoutSpecs[Layout].Channels)
        {
            return format;
        }
    }
    return NULL;
}

#endif // AUDIOHUB_SPEAKER_FORMAT_BANKS_IMPLEMENTATION

#endif // _SIMPLEAUDIOSAMPLE_SPEAKERWAVTABLE_H_
