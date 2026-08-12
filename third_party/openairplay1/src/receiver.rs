//! The embedding facade: configure a [`Receiver`], hand it a sink factory
//! and an event channel, and run it on your own tokio runtime.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use log::warn;
use tokio::net::TcpListener;

use crate::events::{EventSender, LatestRemoteControlSender, LatestVolumeSender};
use crate::server::{serve, Context};
use crate::sink::AudioSink;
use crate::{avahi, mac, Config, SecurityLimits};

const DEFAULT_NAME: &str = "OpenAirPlay";
const DEFAULT_PORT: u16 = 5000;
/// Locally-administered fallback (starts with 0x02) when discovery finds no
/// interface MAC.
const FALLBACK_MAC: [u8; 6] = [0x02, 0x4f, 0x41, 0x50, 0x31, 0x00];

/// Configures a [`Receiver`]. Create one with [`Receiver::builder`].
///
/// AirPlay 1 has no per-receiver identity — every receiver uses the
/// well-known AirPort Express RSA key embedded in the library — so every
/// setting has a default and [`build`](Self::build) cannot really fail.
pub struct ReceiverBuilder {
    name: String,
    port: u16,
    mac: Option<[u8; 6]>,
    advertise: bool,
    password: Option<String>,
    security: SecurityLimits,
}

impl ReceiverBuilder {
    fn new() -> ReceiverBuilder {
        ReceiverBuilder {
            name: DEFAULT_NAME.to_string(),
            port: DEFAULT_PORT,
            mac: None,
            advertise: true,
            password: None,
            security: SecurityLimits::default(),
        }
    }

    /// The receiver name senders see in the AirPlay picker.
    /// Default: `OpenAirPlay`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The RTSP control port to listen on (also advertised). Default: 5000.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The MAC used in the `_raop._tcp` service name and the
    /// `Apple-Challenge` signature (clients verify against it, so both must
    /// match). Default: discovered from the first up, non-loopback network
    /// interface, with a fixed locally-administered fallback.
    pub fn mac(mut self, mac: [u8; 6]) -> Self {
        self.mac = Some(mac);
        self
    }

    /// Whether [`Receiver::run`] registers `_raop._tcp` with the system
    /// Avahi daemon (default `true`). Pass `false` when the host owns its
    /// mDNS registration and use [`Receiver::txt_records`] to advertise.
    pub fn advertise(mut self, advertise: bool) -> Self {
        self.advertise = advertise;
        self
    }

    /// Require a password before the receiver will stream to a sender. When
    /// set, the receiver advertises `pw=true` in `_raop._tcp` so Apple senders
    /// know to authenticate; unauthenticated senders are refused at SETUP.
    /// Default: `None` (no password, `pw=false`, accept anything).
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Override receiver-edge connection and idle bounds.
    pub fn security_limits(mut self, limits: SecurityLimits) -> Self {
        self.security = limits;
        self
    }

    /// Resolve the MAC and produce a runnable [`Receiver`].
    pub fn build(self) -> io::Result<Receiver> {
        let security = self.security.validate()?;
        let mac = self.mac.or_else(mac::discover).unwrap_or_else(|| {
            warn!("no network interface MAC found, using a fixed fallback");
            FALLBACK_MAC
        });
        Ok(Receiver {
            config: Config {
                name: self.name,
                port: self.port,
                mac,
                password: self.password,
                security,
            },
            advertise: self.advertise,
        })
    }
}

/// An AirPlay 1 (RAOP) audio receiver: senders discover it, handshake with
/// it, and stream ALAC to it; the host gets decoded PCM (via its
/// [`AudioSink`]) and [`Event`](crate::Event)s. One sender → one stream →
/// one output.
pub struct Receiver {
    config: Config,
    advertise: bool,
}

impl Receiver {
    /// Start configuring a receiver. See [`ReceiverBuilder`] for the
    /// options; everything has a default.
    pub fn builder() -> ReceiverBuilder {
        ReceiverBuilder::new()
    }

    /// The resolved configuration (name, port, MAC).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The `_raop._tcp` TXT records, for hosts that own their mDNS
    /// registration (built with `advertise(false)`). Advertise these on
    /// [`Config::port`] under the service name
    /// [`Config::service_name`] (`<MAC>@<Name>`). `pw` reflects the
    /// configured password: `pw=true` when one is set, `pw=false` otherwise.
    pub fn txt_records(&self) -> Vec<String> {
        avahi::txt_records(self.config.password.as_deref())
    }

    /// Serve AirPlay on the caller's runtime until a listener error.
    ///
    /// `sink_factory` is invoked at `SETUP` with the negotiated sample rate
    /// and channel count, once per stream; the sink is then fed from a
    /// dedicated playback thread (see [`AudioSink`]). Session milestones are
    /// reported on `events`; dropping the receiving half is allowed and
    /// simply discards them.
    ///
    /// Cancellation: dropping the returned future (e.g. in `select!`) stops
    /// accepting new connections and withdraws the Avahi advertisement;
    /// already-accepted connections are detached and end when their sockets
    /// close.
    pub async fn run<F>(self, sink_factory: F, events: EventSender) -> io::Result<()>
    where
        F: Fn(u32, u8) -> Box<dyn AudioSink> + Send + Sync + 'static,
    {
        self.run_inner(
            move |_stream_id, rate, channels| sink_factory(rate, channels),
            events,
            None,
            None,
        )
        .await
    }

    /// Serve AirPlay like [`run`](Self::run), with a constant-memory,
    /// latest-wins volume sideband in addition to the ordinary event queue.
    ///
    /// When supplied, [`crate::Event::Volume`] is delivered through
    /// `latest_volume` instead of `events`; all other events keep their normal
    /// bounded `try_send` behavior. This lets a host synchronize native system
    /// volume without losing the final value behind metadata or artwork.
    pub async fn run_with_latest_volume<F>(
        self,
        sink_factory: F,
        events: EventSender,
        latest_volume: LatestVolumeSender,
    ) -> io::Result<()>
    where
        F: Fn(u32, u8) -> Box<dyn AudioSink> + Send + Sync + 'static,
    {
        self.run_inner(
            move |_stream_id, rate, channels| sink_factory(rate, channels),
            events,
            Some(latest_volume),
            None,
        )
        .await
    }

    /// Serve AirPlay with constant-memory latest-wins sidebands for both
    /// sender volume and the active session's DACP credentials. This callback
    /// receives `(stream_id, sample_rate, channels)` so the concrete PCM sink
    /// can be correlated with those asynchronous sidebands without guessing
    /// from delivery order.
    pub async fn run_with_latest_volume_and_remote_control<F>(
        self,
        sink_factory: F,
        events: EventSender,
        latest_volume: LatestVolumeSender,
        latest_remote_control: LatestRemoteControlSender,
    ) -> io::Result<()>
    where
        F: Fn(u64, u32, u8) -> Box<dyn AudioSink> + Send + Sync + 'static,
    {
        self.run_inner(
            sink_factory,
            events,
            Some(latest_volume),
            Some(latest_remote_control),
        )
        .await
    }

    async fn run_inner<F>(
        self,
        sink_factory: F,
        events: EventSender,
        latest_volume: Option<LatestVolumeSender>,
        latest_remote_control: Option<LatestRemoteControlSender>,
    ) -> io::Result<()>
    where
        F: Fn(u64, u32, u8) -> Box<dyn AudioSink> + Send + Sync + 'static,
    {
        // Prefer a dual-stack socket (IPv4 clients arrive as v4-mapped
        // addresses, which the challenge code handles); fall back to
        // IPv4-only.
        let port = self.config.port;
        let listener =
            match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).await {
                Ok(l) => l,
                Err(_) => TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
                    .await
                    .map_err(|e| {
                        io::Error::new(e.kind(), format!("cannot bind RTSP port {port}: {e}"))
                    })?,
            };

        let _advertisement = if self.advertise {
            match avahi::publish(&self.config).await {
                Ok(ad) => Some(ad),
                Err(e) => {
                    warn!("avahi advertisement disabled ({e}); is avahi-daemon running?");
                    None
                }
            }
        } else {
            None
        };

        let context = Arc::new(Context {
            config: self.config,
            sink_factory: Arc::new(sink_factory),
            events,
            latest_volume,
            latest_remote_control,
        });
        serve(listener, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_applies_defaults_and_overrides() {
        let receiver = Receiver::builder()
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .build()
            .unwrap();
        let config = receiver.config();
        assert_eq!(config.name, "OpenAirPlay");
        assert_eq!(config.port, 5000);
        assert_eq!(config.mac_hex(), "AABBCCDDEEFF");
        assert_eq!(config.service_name(), "AABBCCDDEEFF@OpenAirPlay");

        let receiver = Receiver::builder()
            .name("Living Room")
            .port(5001)
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .advertise(false)
            .password("4321")
            .build()
            .unwrap();
        assert_eq!(receiver.config().name, "Living Room");
        assert_eq!(receiver.config().port, 5001);
        assert_eq!(receiver.config().password.as_deref(), Some("4321"));
        assert!(!receiver.advertise);
    }

    #[test]
    fn password_defaults_to_none() {
        let receiver = Receiver::builder().build().unwrap();
        assert_eq!(receiver.config().password, None);
    }

    #[test]
    fn txt_records_match_shairport_classic_mode() {
        let receiver = Receiver::builder().build().unwrap();
        let records = receiver.txt_records();
        assert!(records.iter().any(|r| r == "txtvers=1"));
        assert!(records.iter().any(|r| r == "cn=0,1"));
        assert!(records.iter().any(|r| r == "sr=44100"));
        assert!(records.iter().any(|r| r == "et=0,1"));
        // No password: the receiver is advertised as open.
        assert!(records.iter().any(|r| r == "pw=false"));
        assert!(!records.iter().any(|r| r.starts_with("pw=true")));
    }

    #[test]
    fn txt_records_advertise_protection_when_password_set() {
        let receiver = Receiver::builder().password("1234").build().unwrap();
        let records = receiver.txt_records();
        assert!(records.iter().any(|r| r == "pw=true"));
        assert!(!records.iter().any(|r| r == "pw=false"));
        // The password itself is never in the advertisement.
        assert!(!records.iter().any(|r| r.contains("1234")));
    }
}
