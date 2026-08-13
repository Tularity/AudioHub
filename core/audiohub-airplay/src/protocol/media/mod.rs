//! AirPlay 2 audio-media protocol primitives.

pub(crate) mod audio_crypto;
pub(crate) mod buffered;
pub(crate) mod buffered_clock;
pub(crate) mod clock;
pub(crate) mod decode;
pub(crate) mod engine;
pub(crate) mod jitter;
pub(crate) mod packet;
pub(crate) mod ptp;
#[cfg(target_os = "macos")]
pub(crate) mod ptp_macos;
pub(crate) mod setup;
