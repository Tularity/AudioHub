/*++

Copyright (c) Microsoft Corporation All Rights Reserved

Module Name:

    minipairs.h

Abstract:

    Local audio endpoint filter definitions. 
--*/

#ifndef _SIMPLEAUDIOSAMPLE_MINIPAIRS_H_
#define _SIMPLEAUDIOSAMPLE_MINIPAIRS_H_

#include "speakertopo.h"
#include "speakertoptable.h"
#include "speakerwavtable.h"

#include "micarraytopo.h"
#include "micarray1toptable.h"
#include "micarraywavtable.h"


NTSTATUS
CreateMiniportWaveRTSimpleAudioSample
( 
    _Out_       PUNKNOWN *,
    _In_        REFCLSID,
    _In_opt_    PUNKNOWN,
    _In_        POOL_FLAGS,
    _In_        PUNKNOWN,
    _In_opt_    PVOID,
    _In_        PENDPOINT_MINIPAIR
);

NTSTATUS
CreateMiniportTopologySimpleAudioSample
( 
    _Out_       PUNKNOWN *,
    _In_        REFCLSID,
    _In_opt_    PUNKNOWN,
    _In_        POOL_FLAGS,
    _In_        PUNKNOWN,
    _In_opt_    PVOID,
    _In_        PENDPOINT_MINIPAIR
);

//
// Render miniports.
//

/*********************************************************************
* Topology/Wave bridge connection for speaker (internal)             *
*                                                                    *
*              +------+                +------+                      *
*              | Wave |                | Topo |                      *
*              |      |                |      |                      *
* System   --->|0    1|--------------->|0    1|---> Line Out         *
*              |      |                |      |                      *
*              +------+                +------+                      *
*********************************************************************/
static
PHYSICALCONNECTIONTABLE SpeakerTopologyPhysicalConnections[] =
{
    {
        KSPIN_TOPO_WAVEOUT_SOURCE,  // TopologyIn
        KSPIN_WAVE_RENDER3_SOURCE,   // WaveOut
        CONNECTIONTYPE_WAVE_OUTPUT
    }
};

//
// Capture miniports.
//

/*********************************************************************
* Topology/Wave bridge connection for mic array  1 (front)           *
*                                                                    *
*              +------+    +------+                                  *
*              | Topo |    | Wave |                                  *
*              |      |    |      |                                  *
*  Mic in  --->|0    1|===>|0    1|---> Capture Host Pin             *
*              |      |    |      |                                  *
*              +------+    +------+                                  *
*********************************************************************/
static
PHYSICALCONNECTIONTABLE MicArray1TopologyPhysicalConnections[] =
{
    {
        KSPIN_TOPO_BRIDGE,          // TopologyOut
        KSPIN_WAVE_BRIDGE,          // WaveIn
        CONNECTIONTYPE_TOPOLOGY_OUTPUT
    }
};

//
// The static ENDPOINT_MINIPAIRs the sample declared here are GONE.
//
// A minipair now belongs to a SLOT, not to the driver image: perpeer.cpp fills
// one per direction per paired peer, pointing at the shared descriptors and
// tables above and at that slot's own reference strings and FriendlyName.
//
// The descriptors and the pin/format tables stay shared. sysvad deep-copies
// them per endpoint only because its Bluetooth path rewrites each endpoint's
// pin Category at runtime; nothing here varies per peer, so copying them would
// buy nothing and add a lifetime to get wrong.
//

//=============================================================================
//
// AudioHub publishes one pair of endpoints PER PAIRED PEER, at runtime, so each
// peer costs 4 miniports: render topology + render wave + capture topology +
// capture wave.
//
// This number reaches PcAddAdapterDevice ONCE, from AddDevice, and cannot be
// raised afterwards -- "This count sets the upper limit to the total number of
// miniport objects that the adapter driver can instantiate."
//
// AUDIOHUB_WIN_MAX_SLOTS must equal HAL_MAX_SLOTS in halbridge.rs. The C_ASSERT
// is here so that changing either side has to be deliberate, instead of showing
// up as PcRegisterSubdevice failing on the second peer with an error code that
// says nothing about peers.
//
#include "AudioHubIoctl.h"

#define AUDIOHUB_MINIPORTS_PER_PEER 4
#define g_MaxAudioHubMiniports      (AUDIOHUB_MINIPORTS_PER_PEER * AUDIOHUB_WIN_MAX_SLOTS)
C_ASSERT(g_MaxAudioHubMiniports == 64);

#endif // _SIMPLEAUDIOSAMPLE_MINIPAIRS_H_
