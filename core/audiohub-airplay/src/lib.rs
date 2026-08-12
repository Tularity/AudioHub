//! Audio-only AirPlay ingress for AudioHub.
//!
//! The crate deliberately separates protocol reception from discovery and
//! playback policy:
//!
//! - [`AirPlayRuntime`] owns AirPlay 1 and (optionally) experimental AirPlay 2
//!   network runtimes and can be stopped synchronously.
//! - [`MdnsService`] values contain everything the daemon needs to advertise;
//!   this crate never starts an mDNS responder.
//! - [`PcmBus`] is a bounded, multi-reader 48 kHz mono bus. The receiver sinks
//!   perform only downmixing and sample-rate conversion. AirPlay volume is
//!   emitted as [`AirPlayEvent::Volume`] and is never applied to samples.

mod bus;
mod convert;
mod runtime;

pub use bus::{BusConfig, PcmBus, PcmRead, PcmReader, AUDIOHUB_FRAME_SAMPLES, AUDIOHUB_RATE};
pub use convert::StreamingPcmConverter;
pub use runtime::{
    AirPlayConfig, AirPlayEvent, AirPlayRuntime, MdnsService, Protocol, RemoteControlInfo,
    RemoteControlSnapshot, RuntimePhase, RuntimeStatus, SenderVolumeSnapshot, SessionInfo,
};
