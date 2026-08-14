//! Audio-only AirPlay ingress for AudioHub.
//!
//! The crate deliberately separates protocol reception from discovery and
//! playback policy:
//!
//! - [`AirPlayRuntime`] owns the AirPlay 2 network runtime and can be stopped
//!   synchronously.
//! - [`MdnsService`] values contain everything the daemon needs to advertise;
//!   this crate never starts an mDNS responder.
//! - [`PcmBus`] is a bounded, multi-reader 48 kHz mono bus. The receiver sinks
//!   perform only downmixing and sample-rate conversion. AirPlay volume is
//!   emitted as [`AirPlayEvent::Volume`] and is never applied to samples.

mod bus;
mod convert;
mod protocol;
mod runtime;

pub use bus::{BusConfig, PcmBus, PcmRead, PcmReader, AUDIOHUB_FRAME_SAMPLES, AUDIOHUB_RATE};
pub use convert::StreamingPcmConverter;
pub use protocol::server::EventCommandSendError;
pub use runtime::{
    is_control_port_bind_error, AirPlayConfig, AirPlayEvent, AirPlayRuntime, ArtworkSnapshot,
    MdnsService, Protocol, ReceiverVolumeControlSnapshot, RemoteControlInfo, RemoteControlSnapshot,
    RuntimePhase, RuntimeStatus, SenderVolumeSnapshot, SessionInfo,
};
