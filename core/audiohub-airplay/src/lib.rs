//! Audio-only AirPlay ingress for AudioHub.
//!
//! The crate deliberately separates protocol reception from discovery and
//! playback policy:
//!
//! - [`AirPlayRuntime`] owns the AirPlay 2 network runtime and can be stopped
//!   synchronously.
//! - [`MdnsService`] values contain everything the daemon needs to advertise;
//!   this crate never starts an mDNS responder.
//! - [`PcmBus`] is a bounded, multi-reader 48 kHz stereo-frame bus. Receiver
//!   sinks preserve L/R through sample-rate conversion. AirPlay volume is
//!   emitted as [`AirPlayEvent::Volume`] and is never applied to samples.
//! - PCM-bus telemetry fields retain their existing `*_samples` schema names,
//!   but each count is one 48 kHz stereo frame. Decoder `output_samples` stays
//!   an interleaved scalar-sample count.

mod bus;
mod convert;
mod protocol;
mod runtime;
mod telemetry;

pub use bus::{BusConfig, PcmBus, PcmRead, PcmReader, AUDIOHUB_FRAME_SAMPLES, AUDIOHUB_RATE};
pub use convert::{StreamingPcmConverter, StreamingStereoPcmConverter};
pub use protocol::server::EventCommandSendError;
pub use runtime::{
    is_control_port_bind_error, AirPlayConfig, AirPlayEvent, AirPlayRuntime, ArtworkSnapshot,
    MdnsService, Protocol, ReceiverVolumeControlSnapshot, RemoteControlInfo, RemoteControlSnapshot,
    RuntimePhase, RuntimeStatus, SenderVolumeSnapshot, SessionInfo,
};
pub use telemetry::{
    reset as reset_telemetry, snapshot as telemetry_snapshot, AirPlayControlSessionIdentity,
    AirPlayTelemetryCounters, AirPlayTelemetryEvent, AirPlayTelemetrySnapshot,
    MediaTelemetryCounters,
};
