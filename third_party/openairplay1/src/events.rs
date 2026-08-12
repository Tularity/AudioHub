//! Session events delivered to the host.
//!
//! Sent over a bounded `tokio::sync::mpsc` channel supplied by the host. A
//! full or dropped receiver is tolerated and the intermediate notification
//! is discarded; this prevents metadata or artwork floods from retaining
//! memory without bound. Wire concepts (RTSP, SDP, RTP, sequence numbers)
//! never appear here — playback position, for instance, arrives on the wire
//! as RTP timestamps and is reported here as [`Duration`].

use std::net::SocketAddr;
use std::time::Duration;

/// Bearer credentials supplied by a classic AirPlay sender for its DACP
/// remote-control service.
///
/// The active-remote value is deliberately redacted from `Debug`: it is an
/// authorization token and must never be copied into logs or status snapshots.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteControl {
    /// Identifier used to find `iTunes_Ctrl_<DACP-ID>._dacp._tcp.local.`.
    pub dacp_id: String,
    /// Value for the outbound DACP `Active-Remote` request header.
    pub active_remote: String,
}

/// Latest DACP capability for one concrete streaming RTSP session.
///
/// This travels over a `watch` sideband rather than the ordinary bounded
/// event queue so a credential refresh cannot be lost behind metadata or
/// artwork. `stream_id` is allocated when SETUP succeeds and monotonically
/// increases for the lifetime of the receiver.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteControlSession {
    /// Receiver-local SETUP sequence.
    pub stream_id: u64,
    /// RTSP peer, including an IPv6 link-local scope ID when present.
    pub peer: SocketAddr,
    /// Negotiated sample rate.
    pub rate: u32,
    /// Negotiated channel count.
    pub channels: u8,
    /// Complete current credential pair, or `None` until both components have
    /// appeared on authorized requests.
    pub remote_control: Option<RemoteControl>,
}

impl std::fmt::Debug for RemoteControlSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteControlSession")
            .field("stream_id", &self.stream_id)
            .field("peer", &self.peer)
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("remote_control", &self.remote_control)
            .finish()
    }
}

/// Latest state transition on the lossless DACP sideband.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteControlState {
    /// A stream owns the receiver and may refresh its complete credentials.
    Active(RemoteControlSession),
    /// The identified stream ended. Hosts must ignore a delayed end for an
    /// already-replaced stream.
    Ended {
        /// Receiver-local SETUP sequence that ended.
        stream_id: u64,
    },
}

impl std::fmt::Debug for RemoteControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteControl")
            .field("dacp_id", &self.dacp_id)
            .field("active_remote", &"<redacted>")
            .finish()
    }
}

/// What the host needs to know about the streaming session. Transport
/// handling (FLUSH) is already done inside the library — that variant is
/// informational.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `SETUP` completed; a sink is about to be created and used.
    ///
    /// Deliberately *not* `#[non_exhaustive]`: hosts build this variant in
    /// their own tests (the receiver's TUI tests do), and a non-exhaustive
    /// variant cannot be constructed outside this crate. Matching with `..`
    /// is still the friendlier habit.
    SessionStarted {
        /// Receiver-local identity allocated when this SETUP succeeds.
        stream_id: u64,
        /// Sample rate in Hz (44100 for every stock sender).
        rate: u32,
        /// Channel count (2 for every stock sender).
        channels: u8,
        /// The RTSP address the sender connected from. The port is not part of
        /// sender identity, but retaining the full socket address preserves an
        /// IPv6 link-local scope ID for host-side DACP callbacks.
        peer: SocketAddr,
        /// Optional classic-AirPlay remote-control credentials. Both
        /// components must have appeared validly on authorized RTSP requests
        /// (not necessarily the same request) or this is `None`.
        remote_control: Option<RemoteControl>,
    },
    /// `SET_PARAMETER volume`, in AirPlay dB (0 = full, −144 = mute). The
    /// library does not apply gain — the host maps this onto its own volume
    /// model.
    Volume {
        /// Receiver-local SETUP sequence that authored this value.
        stream_id: u64,
        /// Receiver-wide monotonic sender-command revision. This advances for
        /// every accepted volume line, including repeated identical values.
        revision: u64,
        /// AirPlay volume in dB: 0 = full scale, −30 ≈ minimum, −144 = mute.
        db: f32,
    },
    /// `SET_PARAMETER` track metadata (DMAP). A complete statement about
    /// the current track, not a delta: fields the sender's payload did not
    /// carry are `None` and replace the previous value. Delivered only
    /// between [`Event::SessionStarted`] and [`Event::SessionEnded`].
    Metadata {
        /// Receiver-local SETUP sequence that authored this metadata.
        stream_id: u64,
        /// Track title (DMAP `minm`).
        title: Option<String>,
        /// Track artist (DAAP `asar`).
        artist: Option<String>,
        /// Track album (DAAP `asal`).
        album: Option<String>,
    },
    /// `SET_PARAMETER` cover art, exactly as sent (typically `image/jpeg`
    /// or `image/png`, tens to hundreds of KB). Empty `data` — the
    /// `image/none` content type — means the sender cleared the artwork.
    /// Delivered only between [`Event::SessionStarted`] and
    /// [`Event::SessionEnded`].
    Artwork {
        /// Receiver-local SETUP sequence that authored this artwork.
        stream_id: u64,
        /// The image media type as sent, e.g. `image/jpeg` or `image/png`
        /// (`image/none` accompanies a clear).
        content_type: String,
        /// The image bytes, exactly as sent; empty means cleared.
        data: Vec<u8>,
    },
    /// Where playback is in the current track, reported about once a second
    /// as the audio plays.
    ///
    /// This follows the audio, not the clock on the wall: the sender only
    /// says where a track starts and ends (one capture showed a 251-second
    /// track play to its end without a single further report), so the
    /// position comes from the RTP timestamp of the audio actually being
    /// played. A host can therefore display it as-is and should **not**
    /// extrapolate between events — when a sender pauses, the reports simply
    /// stop and the last one remains true. Delivered only between
    /// [`Event::SessionStarted`] and [`Event::SessionEnded`].
    Progress {
        /// Receiver-local SETUP sequence whose playback position this is.
        stream_id: u64,
        /// How far into the track playback is.
        elapsed: Duration,
        /// The track's total length. Zero when the sender reports a stream
        /// with no known end (live radio, for instance).
        duration: Duration,
    },
    /// `FLUSH` (seek/stop from the sender). The library already reset its
    /// jitter buffer/prebuffer and called [`crate::sink::AudioSink::flush`].
    Flushed {
        /// Receiver-local SETUP sequence that was flushed.
        stream_id: u64,
    },
    /// `TEARDOWN`, or the control connection closed.
    SessionEnded {
        /// Receiver-local SETUP sequence that ended.
        stream_id: u64,
    },
}

/// The sending half handed to the library; the host keeps the receiver.
pub type EventSender = tokio::sync::mpsc::Sender<Event>;

/// Latest-wins volume sideband for hosts that must not lose the final system
/// volume update when the ordinary bounded event queue is saturated.
///
/// The value is `None` until a streaming sender supplies a volume, then a
/// stream-tagged value in AirPlay dB. A [`tokio::sync::watch`] channel has constant
/// storage and coalesces rapid intermediate changes without blocking the RTSP
/// control task.
pub type LatestVolumeSender = tokio::sync::watch::Sender<Option<VolumeUpdate>>;

/// One sender-authored volume tied to its concrete SETUP identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeUpdate {
    /// Receiver-local SETUP sequence.
    pub stream_id: u64,
    /// Receiver-wide monotonic sender-command revision. Gaps mean the watch
    /// channel coalesced intermediate commands, not that the final command
    /// was lost.
    pub revision: u64,
    /// AirPlay volume in dB.
    pub db: f32,
}

/// Latest-wins DACP credential sideband.
///
/// `None` is only the channel's initial state. An active-session update
/// contains the complete current pair; missing headers on a later request
/// therefore never erase a previously-authorized value.
pub type LatestRemoteControlSender = tokio::sync::watch::Sender<Option<RemoteControlState>>;
