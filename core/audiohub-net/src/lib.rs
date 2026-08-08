pub mod control;
pub mod discovery;
pub mod echo;
/// Framing for byte-stream transports (design §2 decision B): tier 1's media
/// connection and tier 2's multiplexed one both decode with this.
pub mod framed;
pub mod identity;
pub mod media;
/// A second `ControlIo` implementation, built out of byte queues, and the proof
/// that the control stack runs on one. Test-only: it exists so "the handshake
/// no longer needs a socket" is a thing that runs, not a thing a comment says.
#[cfg(test)]
mod memduplex;
pub mod mode;
/// The control byte stream of a multiplexed connection (tier 2), as a
/// `ControlIo`. The production second implementation `memduplex` was written to
/// anticipate.
pub mod muxio;
pub mod packet;
pub mod pairing;
pub mod secure;
pub mod session;
pub mod stats;
