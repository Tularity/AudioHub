/*++

Copyright (c) Microsoft Corporation All Rights Reserved

Module Name:

    speakertopo.cpp

Abstract:

    Implementation of topology miniport for the speaker (internal).
--*/

#pragma warning (disable : 4127)

#include "definitions.h"
#include "endpoints.h"
#include "mintopo.h"
#include "speakertopo.h"
#include "speakertoptable.h"


#pragma code_seg("PAGE")
//=============================================================================
NTSTATUS
PropertyHandler_SpeakerTopoFilter
( 
    _In_ PPCPROPERTY_REQUEST      PropertyRequest 
)
/*++

Routine Description:

  Redirects property request to miniport object

Arguments:

  PropertyRequest - 

Return Value:

  NT status code.

--*/
{
    PAGED_CODE();

    ASSERT(PropertyRequest);

    DPF_ENTER(("[PropertyHandler_SpeakerTopoFilter]"));

    // PropertryRequest structure is filled by portcls. 
    // MajorTarget is a pointer to miniport object for miniports.
    //
    NTSTATUS            ntStatus = STATUS_INVALID_DEVICE_REQUEST;
    PCMiniportTopology  pMiniport = (PCMiniportTopology)PropertyRequest->MajorTarget;

    if (IsEqualGUIDAligned(*PropertyRequest->PropertyItem->Set, KSPROPSETID_Jack))
    {
        if (PropertyRequest->PropertyItem->Id == KSPROPERTY_JACK_DESCRIPTION)
        {
            ntStatus = pMiniport->PropertyHandlerJackDescription(
                PropertyRequest,
                ARRAYSIZE(SpeakerJackDescriptions),
                SpeakerJackDescriptions
                );
        }
        else if (PropertyRequest->PropertyItem->Id == KSPROPERTY_JACK_DESCRIPTION2)
        {
            ntStatus = pMiniport->PropertyHandlerJackDescription2(
                PropertyRequest,
                ARRAYSIZE(SpeakerJackDescriptions),
                SpeakerJackDescriptions,
                0 // jack capabilities
                );
        }
    }

    return ntStatus;
} // PropertyHandler_SpeakerTopoFilter

//=============================================================================
NTSTATUS
PropertyHandler_SpeakerTopology
(
    _In_ PPCPROPERTY_REQUEST      PropertyRequest
)
/*++

Routine Description:

  Redirects property request to miniport object

Arguments:

  PropertyRequest -

Return Value:

  NT status code.

--*/
{
    PAGED_CODE();

    ASSERT(PropertyRequest);

    DPF_ENTER(("[PropertyHandler_SpeakerTopology]"));

    // PropertryRequest structure is filled by portcls. 
    // MajorTarget is a pointer to miniport object for miniports.
    //
    PCMiniportTopology pMiniport = (PCMiniportTopology)PropertyRequest->MajorTarget;

    return pMiniport->PropertyHandlerGeneric(PropertyRequest);
} // PropertyHandler_SpeakerTopology

//=============================================================================
//
// NOT pageable, unlike every property handler above it. KS calls event handlers
// with PCEVENT_VERB_REMOVE from ks!FreeEventListSynchronize, which runs inside
// ks!PerformLockedOperation -- i.e. at DISPATCH_LEVEL, where the page fault
// that would fetch a trimmed code page cannot be serviced:
//
//   0xD1 AV_VRF_CODE_AV_PAGED_IP_audiohubvad!EventHandler_SpeakerTopology
//
// measured under Driver Verifier on 2026-08-09, closing a topology filter.
// Upstream already knew this: CMiniportWaveRT_EventHandler_PinCapsChange in
// minwavert.cpp sits in code_seg() for the same reason. The handlers added with
// the volume-change events did not copy the convention.
//
#pragma code_seg()
NTSTATUS
EventHandler_SpeakerTopology
(
    _In_ PPCEVENT_REQUEST      EventRequest
)
/*++

Routine Description:

  Redirects an event request to the miniport object, exactly as
  PropertyHandler_SpeakerTopology does for properties. The cast is the same one
  because MajorTarget is the same pointer portcls passes for both.

--*/
{
    //
    // No PAGED_CODE() here on purpose: it asserts IRQL < DISPATCH_LEVEL, and
    // the REMOVE verb legitimately arrives AT DISPATCH_LEVEL. The assertion was
    // present and was wrong.
    //
    ASSERT(EventRequest);

    DPF_ENTER(("[EventHandler_SpeakerTopology]"));

    PCMiniportTopology pMiniport = (PCMiniportTopology)EventRequest->MajorTarget;

    return pMiniport->AhEventHandlerSlotVolume(EventRequest);
} // EventHandler_SpeakerTopology

#pragma code_seg()
