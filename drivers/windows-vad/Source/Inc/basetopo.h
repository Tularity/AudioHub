
/*++

Copyright (c) Microsoft Corporation All Rights Reserved

Module Name:

    basetopo.h

Abstract:

    Declaration of topology miniport.
--*/

#ifndef _SIMPLEAUDIOSAMPLE_BASETOPO_H_
#define _SIMPLEAUDIOSAMPLE_BASETOPO_H_

//=============================================================================
// Classes
//=============================================================================

///////////////////////////////////////////////////////////////////////////////
// CMiniportTopologySimpleAudioSample
//

class CMiniportTopologySimpleAudioSample
{
  protected:
    PADAPTERCOMMON              m_AdapterCommon;        // Adapter common object.
    PPCFILTER_DESCRIPTOR        m_FilterDescriptor;     // Filter descriptor.
    PPORTEVENTS                 m_PortEvents;           // Event interface.
    USHORT                      m_DeviceMaxChannels;    // Max device channels.

    //
    // WHICH PEER'S ENDPOINT THIS IS. Decoded from the AH_EP_CONTEXT the
    // miniport was created with; AUDIOHUB_WIN_MAX_SLOTS means "not one of
    // ours", which falls back to the sample's adapter-wide storage.
    //
    // This exists because the sample's volume storage cannot work here.
    // Upstream keeps levels in CSimpleAudioSample::m_VolumeControls[], one
    // array for the whole ADAPTER indexed by the topology NODE id -- and every
    // slot's volume node is node 0, and there is one adapter. Sixteen peers and
    // two directions therefore shared a single cell: moving peer A's speaker
    // slider moved peer B's, with nothing in any device list or log to say so.
    //
    ULONG                       m_AhSlot;
    BOOLEAN                     m_AhInput;

  public:
    //
    // Called immediately after construction by whichever Create* function made
    // this object, with the DeviceContext PortCls threaded down from
    // InstallEndpointFilters. Separate from the constructor because the two
    // topology classes have different constructors and only one of them was
    // passing DeviceContext through at all.
    //
    VOID                        SetAhEndpointContext(_In_opt_ PVOID DeviceContext);

    //
    // KSPROPERTY_AUDIO_VOLUMELEVEL / _MUTE against this slot's own storage.
    // Public because AhTopoRaiseVolumeEvent (a free function) is its partner.
    //
    NTSTATUS                    AhPropertyHandlerSlotVolume(_In_ PPCPROPERTY_REQUEST PropertyRequest);

    CMiniportTopologySimpleAudioSample(
        _In_        PCFILTER_DESCRIPTOR    *FilterDesc,
        _In_        USHORT                  DeviceMaxChannels
        );
    
    ~CMiniportTopologySimpleAudioSample();

    NTSTATUS                    GetDescription
    (   
        _Out_ PPCFILTER_DESCRIPTOR *  Description
    );

    NTSTATUS                    DataRangeIntersection
    (   
        _In_  ULONG             PinId,
        _In_  PKSDATARANGE      ClientDataRange,
        _In_  PKSDATARANGE      MyDataRange,
        _In_  ULONG             OutputBufferLength,
        _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength)
              PVOID             ResultantFormat OPTIONAL,
        _Out_ PULONG            ResultantFormatLength
    );

    NTSTATUS                    Init
    ( 
        _In_  PUNKNOWN          UnknownAdapter,
        _In_  PPORTTOPOLOGY     Port_ 
    );

    // PropertyHandlers.
    NTSTATUS                    PropertyHandlerGeneric
    (
        _In_  PPCPROPERTY_REQUEST PropertyRequest
    );

    NTSTATUS                    PropertyHandlerMuxSource
    (
        _In_  PPCPROPERTY_REQUEST PropertyRequest
    );

    NTSTATUS                    PropertyHandlerDevSpecific
    (
        _In_  PPCPROPERTY_REQUEST PropertyRequest
    );

    VOID                        AddEventToEventList
    (
        _In_  PKSEVENT_ENTRY    EventEntry 
    );
    
    VOID                        GenerateEventList
    (
        _In_opt_    GUID       *Set,
        _In_        ULONG       EventId,
        _In_        BOOL        PinEvent,
        _In_        ULONG       PinId,
        _In_        BOOL        NodeEvent,
        _In_        ULONG       NodeId
    );
};

#endif
