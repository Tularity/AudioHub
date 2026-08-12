//! An embeddable AirPlay 1 (RAOP / AirTunes) audio **receiver**: stock
//! Apple senders (iPhone, iPad, Mac, iTunes) discover it, handshake with
//! it, and stream ALAC to it; the host application gets decoded PCM and
//! session events. The library owns network → PCM (discovery advertisement,
//! the RTSP handshake with `Apple-Challenge`, the three RTP channels,
//! decrypt, the jitter buffer with retransmits, the NTP clock model, ALAC
//! decode, and the prebuffer/latency-correct start); the host owns PCM →
//! speaker via an [`AudioSink`].
//!
//! Deliberate scope: one sender → one stream → one output. No AirPlay wire
//! concepts (RTSP, SDP, RTP, sequence numbers) appear in this API.
//!
//! ```no_run
//! use openairplay1::{AudioSink, Event, Receiver};
//!
//! struct MySink;
//!
//! impl AudioSink for MySink {
//!     fn write(&mut self, pcm: &[i16]) { /* blocking write paces playback */ }
//!     fn flush(&mut self) { /* seek: drop device state */ }
//! }
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let receiver = Receiver::builder().name("Office").build()?;
//!     let (events, mut rx) = tokio::sync::mpsc::channel(64);
//!     tokio::spawn(async move {
//!         while let Some(event) = rx.recv().await {
//!             if let Event::Volume { db, .. } = event { /* your gain path */ }
//!         }
//!     });
//!     receiver.run(|_rate, _channels| Box::new(MySink), events).await
//! }
//! ```

#![warn(missing_docs)]

mod avahi;
mod decode;
mod dmap;
mod events;
mod jitter;
mod mac;
mod player;
mod receiver;
mod rtp;
mod rtsp;
mod sdp;
mod session;
mod sink;

// Sender-facing pieces the integration tests (and a future test-sender)
// drive the real server with. Public so the tests can reach them, but not
// part of the documented embedding API.
#[doc(hidden)]
pub mod clock;
#[doc(hidden)]
pub mod crypto;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub use session::{AudioObserver, DecryptedAudio};

pub use avahi::txt_records;
pub use events::{
    Event, EventSender, LatestRemoteControlSender, LatestVolumeSender, RemoteControl,
    RemoteControlSession, RemoteControlState, VolumeUpdate,
};
pub use receiver::{Receiver, ReceiverBuilder};
pub use sink::{AudioSink, SessionSinkFactory, SinkFactory};

use std::fmt;
use std::time::Duration;

/// Resource and idle limits applied at the untrusted RAOP network edge.
///
/// The defaults leave ample room for stock Apple senders and large cover art
/// while bounding slow or malicious LAN clients. Custom values are primarily
/// useful to shorten integration-test deadlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityLimits {
    /// Maximum concurrently accepted RTSP control connections.
    pub max_connections: usize,
    /// Maximum silence while an unauthenticated connection is handshaking.
    pub handshake_idle: Duration,
    /// Maximum silence after authentication but before a stream exists.
    pub authorized_idle: Duration,
    /// Maximum silence on the RTSP connection while its UDP stream is live.
    pub streaming_idle: Duration,
    /// Maximum time without progress while reading one request component or
    /// writing one response.
    pub request_io_idle: Duration,
}

impl Default for SecurityLimits {
    fn default() -> Self {
        Self {
            max_connections: 16,
            handshake_idle: Duration::from_secs(15),
            authorized_idle: Duration::from_secs(60),
            // The RTSP control channel can legitimately stay silent during a
            // long track; it is bounded without breaking ordinary playback.
            streaming_idle: Duration::from_secs(30 * 60),
            request_io_idle: Duration::from_secs(10),
        }
    }
}

impl SecurityLimits {
    pub(crate) fn validate(self) -> std::io::Result<Self> {
        if self.max_connections == 0
            || self.handshake_idle.is_zero()
            || self.authorized_idle.is_zero()
            || self.streaming_idle.is_zero()
            || self.request_io_idle.is_zero()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "RAOP security limits must be nonzero",
            ));
        }
        Ok(self)
    }
}

/// Receiver-wide configuration, resolved by [`ReceiverBuilder::build`].
/// Shared by the RTSP server and the mDNS advertisement: the MAC address
/// must be the same in both places — clients verify the `Apple-Response`
/// signature against the MAC embedded in the service name.
#[derive(Clone)]
pub struct Config {
    /// The receiver name senders see in the AirPlay picker.
    pub name: String,
    /// TCP port of the RTSP control server.
    pub port: u16,
    /// The MAC address used in the service name and the `Apple-Challenge`
    /// signature.
    pub mac: [u8; 6],
    /// `Some` → the receiver requires a password to stream (advertised as
    /// `pw=true` in `_raop._tcp`); `None` (the default) → open (`pw=false`).
    /// The password itself never appears in the advertisement or any log.
    pub password: Option<String>,
    /// Network resource and idle bounds.
    pub security: SecurityLimits,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("name", &self.name)
            .field("port", &self.port)
            .field("mac", &self.mac)
            .field("password_set", &self.password.is_some())
            .field("security", &self.security)
            .finish()
    }
}

impl Config {
    /// MAC as 12 uppercase hex digits, e.g. `AABBCCDDEEFF`, as used in the
    /// `_raop._tcp` service name.
    pub fn mac_hex(&self) -> String {
        self.mac.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// The mDNS service name: `<MAC>@<FriendlyName>`.
    pub fn service_name(&self) -> String {
        format!("{}@{}", self.mac_hex(), self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_debug_reports_password_presence_without_disclosing_it() {
        let password = "vendor-raop-password-7f3d";
        let config = Config {
            name: "Bedroom".into(),
            port: 5000,
            mac: [0x02, 0x4f, 0x41, 0x50, 0x31, 0x00],
            password: Some(password.into()),
            security: SecurityLimits::default(),
        };

        let debug = format!("{config:?}");
        assert!(debug.contains("password_set: true"));
        assert!(!debug.contains(password));
    }
}
