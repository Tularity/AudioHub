//! The seam between the AirPlay session and the host's audio output.
//!
//! The library owns network → PCM (RTSP handshake, decrypt, jitter buffer,
//! clock model, decode, prebuffer and the latency-correct start); the host
//! owns PCM → speaker. A host provides an [`AudioSink`] and the library's
//! playback thread feeds it interleaved `i16` samples that should actually
//! play — PCM is held in the prebuffer until release and the start instant,
//! and a FLUSH resets the prebuffer before calling [`AudioSink::flush`].

use std::sync::Arc;

/// Where decoded audio goes. Implemented by the host (the receiver binary's
/// sink is ALSA), called from a dedicated library-managed playback thread.
pub trait AudioSink: Send + 'static {
    /// Play interleaved `i16` PCM. `write` may block — blocking is the pacing
    /// mechanism: the sink drains at the hardware's rate and the playback
    /// thread waits on it.
    fn write(&mut self, pcm: &[i16]);

    /// FLUSH (seek/stop): immediately drop anything the sink has queued or
    /// buffered of its own (hardware buffers). The library has already reset
    /// its prebuffer/start gate when this is called.
    fn flush(&mut self);
}

/// Creates the sink for a stream, invoked at `SETUP` with the negotiated
/// sample rate and channel count — once per stream.
pub type SinkFactory = Arc<dyn Fn(u32, u8) -> Box<dyn AudioSink> + Send + Sync>;

/// Creates a sink with the receiver-local SETUP identity as well as the
/// negotiated format. Hosts that correlate asynchronous control sidebands
/// with a concrete PCM sink should use this form.
pub type SessionSinkFactory = Arc<dyn Fn(u64, u32, u8) -> Box<dyn AudioSink> + Send + Sync>;
