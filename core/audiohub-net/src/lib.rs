pub mod control;
pub mod discovery;
pub mod echo;
/// Framing for byte-stream transports (design §2 decision B). A library with no
/// caller yet: nothing in the daemon reads or writes a frame.
pub mod framed;
pub mod identity;
pub mod media;
/// A second `ControlIo` implementation, built out of byte queues, and the proof
/// that the control stack runs on one. Test-only: it exists so "the handshake
/// no longer needs a socket" is a thing that runs, not a thing a comment says.
#[cfg(test)]
mod memduplex;
pub mod mode;
pub mod packet;
pub mod pairing;
pub mod secure;
pub mod session;
pub mod stats;
