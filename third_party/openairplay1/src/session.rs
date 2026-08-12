//! Per-connection RAOP session state machine: ANNOUNCE → SETUP → RECORD, the
//! bound UDP channels, and the audio-receiver task that decrypts incoming
//! packets.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use log::{debug, warn};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::clock::{self, ClockModel};
use crate::crypto;
use crate::dmap;
use crate::events::{
    Event, EventSender, LatestRemoteControlSender, LatestVolumeSender, RemoteControl,
    RemoteControlSession, RemoteControlState, VolumeUpdate,
};
use crate::jitter::{Delivery, JitterBuffer};
use crate::player::{Player, PlayerSender};
use crate::rtp::{self, AudioPacket};
use crate::rtsp::{Request, Response};
use crate::sdp::{AlacConfig, Sdp};
use crate::sink::SessionSinkFactory;

/// How often the audio task services retransmit requests and forced skips.
const SERVICE_INTERVAL: Duration = Duration::from_millis(20);
/// Minimum spacing between resend requests for the same sequence number.
const RESEND_BACKOFF: Duration = Duration::from_millis(80);
const FLUSH_QUEUE_CAPACITY: usize = 4;
const RESEND_QUEUE_CAPACITY: usize = 64;
/// `SET_PARAMETER` content type carrying DMAP track metadata.
const DMAP_CONTENT_TYPE: &str = "application/x-dmap-tagged";
/// Realm advertised and accepted by classic RAOP Digest authentication.
const RAOP_AUTH_REALM: &str = "raop";

/// A single-streaming-session gate shared across RTSP connections. AirPlay 1
/// senders assume exclusive use of the receiver; a second one that reaches
/// SETUP while another is streaming is refused.
#[derive(Clone)]
pub struct SessionSlot {
    occupied: Arc<AtomicBool>,
    next_stream_id: Arc<std::sync::atomic::AtomicU64>,
    next_volume_revision: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for SessionSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSlot {
    pub fn new() -> SessionSlot {
        SessionSlot {
            occupied: Arc::new(AtomicBool::new(false)),
            next_stream_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            next_volume_revision: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// Take the slot if free. The returned guard releases it on drop.
    fn try_acquire(&self) -> Option<SlotGuard> {
        self.occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| SlotGuard(self.occupied.clone()))
    }

    fn allocate_stream_id(&self) -> u64 {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed).max(1)
    }

    fn allocate_volume_revision(&self) -> u64 {
        self.next_volume_revision
            .fetch_add(1, Ordering::Relaxed)
            .max(1)
    }
}

struct SlotGuard(Arc<AtomicBool>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The current track's boundaries as RTP timestamps, from the sender's
/// `progress:` line. Shared with the playback thread, which is the only place
/// that knows how far into the track the audio actually is.
pub type TrackAnchor = Arc<Mutex<Option<Track>>>;

/// One track's extent on the RTP timeline.
#[derive(Debug, Clone, Copy)]
pub struct Track {
    /// RTP timestamp where the track starts.
    pub start: u32,
    /// RTP timestamp where it ends.
    pub end: u32,
}

/// A decrypted audio packet, surfaced to an optional observer (the
/// integration tests use it to prove the crypto path; production passes no
/// observer).
#[derive(Debug, Clone)]
pub struct DecryptedAudio {
    /// RTP sequence number of the packet.
    pub sequence: u16,
    /// RTP timestamp (derived from the first packet's anchor).
    pub timestamp: u32,
    /// Whether the plaintext passes a basic ALAC stereo sanity check.
    pub looks_like_alac: bool,
    /// The decrypted ALAC frame, exactly as carried in the RTP payload.
    pub frame: Vec<u8>,
}

/// The bounded sending end of the decrypted-audio observation channel.
pub type AudioObserver = tokio::sync::mpsc::Sender<DecryptedAudio>;

/// Cryptographic and format parameters captured at ANNOUNCE.
#[derive(Clone)]
struct StreamParams {
    encrypted: bool,
    key: [u8; 16],
    iv: [u8; 16],
    alac: AlacConfig,
}

struct PendingArtwork {
    content_type: String,
    data: Vec<u8>,
}

pub struct Session {
    params: Option<StreamParams>,
    tasks: Vec<JoinHandle<()>>,
    observer: Option<AudioObserver>,
    /// Creates the stream's audio sink at SETUP.
    sink_factory: SessionSinkFactory,
    /// Session milestones for the host.
    events: EventSender,
    /// Optional constant-memory path that preserves the final sender volume
    /// even when `events` is full.
    latest_volume: Option<LatestVolumeSender>,
    /// Constant-memory path for the current session's complete DACP
    /// capability. Credential refreshes must not be lossy even when the
    /// ordinary event queue is full.
    latest_remote_control: Option<LatestRemoteControlSender>,
    /// True between SETUP and TEARDOWN/drop; guards the one-time
    /// [`Event::SessionEnded`].
    streaming: bool,
    /// The current track's RTP extent, handed to the playback thread so it
    /// can report where playback really is.
    track: TrackAnchor,
    /// Metadata/artwork that arrived while no session was active (senders
    /// may push them earlier in the handshake, before SETUP). The latest of
    /// each is latched here and delivered right after `SessionStarted`, so
    /// the host only ever sees them inside a session.
    pending_metadata: Option<dmap::TrackMetadata>,
    pending_artwork: Option<PendingArtwork>,
    player: Option<Player>,
    /// Signals the audio task to flush its jitter buffer to a given sequence
    /// (or re-anchor when `None`).
    flush_tx: Option<tokio::sync::mpsc::Sender<Option<u16>>>,
    /// Full RTSP peer address. Retaining its IPv6 scope ID is required for
    /// link-local host callbacks; UDP source filtering uses `peer_ip` below.
    peer_addr: SocketAddr,
    /// The IP the client connected from, for addressing resend requests and
    /// authenticating the negotiated UDP channels.
    peer_ip: IpAddr,
    /// Shared single-session gate and this session's guard once acquired.
    slot: SessionSlot,
    slot_guard: Option<SlotGuard>,
    /// Classic AirPlay 1 password (Digest auth): the per-connection challenge
    /// nonce once issued, and whether this connection has authenticated.
    auth_nonce: Option<String>,
    authorized: bool,
    /// Latest complete DACP capability seen on this authenticated RTSP
    /// connection. Some senders attach it before SETUP, others on SETUP.
    remote_control: Option<RemoteControl>,
    /// Individually valid components are retained across requests, but are
    /// never exposed until both form a complete capability.
    remote_dacp_id: Option<String>,
    remote_active_remote: Option<String>,
    /// Assigned only after SETUP has successfully acquired the receiver slot.
    stream_id: Option<u64>,
    /// Local UDP ports handed to the client in the SETUP response.
    local_audio_port: u16,
    local_control_port: u16,
    local_timing_port: u16,
}

impl Session {
    pub fn new(
        observer: Option<AudioObserver>,
        sink_factory: SessionSinkFactory,
        events: EventSender,
        latest_volume: Option<LatestVolumeSender>,
        latest_remote_control: Option<LatestRemoteControlSender>,
        peer_addr: SocketAddr,
        slot: SessionSlot,
    ) -> Self {
        let peer_ip = peer_addr.ip();
        Session {
            observer,
            sink_factory,
            events,
            latest_volume,
            latest_remote_control,
            streaming: false,
            track: Arc::new(Mutex::new(None)),
            pending_metadata: None,
            pending_artwork: None,
            player: None,
            flush_tx: None,
            peer_addr,
            peer_ip,
            slot,
            slot_guard: None,
            auth_nonce: None,
            authorized: false,
            remote_control: None,
            remote_dacp_id: None,
            remote_active_remote: None,
            stream_id: None,
            params: None,
            tasks: Vec::new(),
            local_audio_port: 0,
            local_control_port: 0,
            local_timing_port: 0,
        }
    }

    pub(crate) fn is_authorized(&self) -> bool {
        self.authorized
    }

    pub(crate) fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Classic AirPlay 1 password check (RFC 2617 Digest), mirroring
    /// shairport-sync's `rtsp_classic_airplay_auth`. A password-protected
    /// receiver answers every request with `401 + WWW-Authenticate` until the
    /// client supplies a valid `Authorization: Digest` header; a connection
    /// that does is marked authorized for the rest of its session. Without a
    /// configured password every connection is authorized immediately.
    /// Returns a `401` response when denied, `None` when the request may
    /// proceed to normal dispatch.
    pub fn authenticate(&mut self, password: Option<&str>, request: &Request) -> Option<Response> {
        if self.authorized {
            return None;
        }
        let Some(password) = password else {
            self.authorized = true;
            return None;
        };
        let Some(auth) = request.headers.get("Authorization") else {
            return Some(self.auth_challenge());
        };
        if !auth.starts_with("Digest ") {
            return Some(self.auth_challenge());
        }
        let (Some(realm), Some(username), Some(nonce), Some(response), Some(uri)) = (
            digest_param(auth, "realm"),
            digest_param(auth, "username"),
            digest_param(auth, "nonce"),
            digest_param(auth, "response"),
            digest_param(auth, "uri"),
        ) else {
            return Some(self.auth_challenge());
        };
        // Bind the response to exactly the challenge and RTSP target this
        // connection is using. In particular, an Authorization header sent
        // before a challenge cannot authenticate with an invented empty
        // nonce, and a digest for one target cannot be replayed against
        // another.
        if realm != RAOP_AUTH_REALM
            || self.auth_nonce.as_deref() != Some(nonce)
            || uri != request.uri.as_str()
        {
            warn!("Authorization failed: Digest parameters do not match the issued challenge");
            return Some(self.auth_challenge());
        }
        let expected = crypto::digest_response(
            username,
            RAOP_AUTH_REALM,
            password,
            &request.method,
            &request.uri,
            nonce,
        );
        if crypto::ct_eq_hex(&expected, response) {
            self.authorized = true;
            None
        } else {
            warn!("Authorization failed: Digest response mismatch");
            Some(self.auth_challenge())
        }
    }

    /// Issue a `401 Unauthorized` carrying the connection's nonce, creating
    /// it on the first challenge so the client can answer it on its next
    /// request.
    fn auth_challenge(&mut self) -> Response {
        let nonce = self.auth_nonce.get_or_insert_with(crypto::make_nonce);
        Response::new(401, "Unauthorized").header(
            "WWW-Authenticate",
            format!("Digest realm=\"{RAOP_AUTH_REALM}\", nonce=\"{nonce}\""),
        )
    }

    /// Handle ANNOUNCE: parse SDP, decrypt the session key, store format and
    /// crypto parameters. Returns the RTSP response (200, or 456 on invalid
    /// crypto material).
    pub fn handle_announce(&mut self, request: &Request) -> Response {
        self.observe_remote_control(request);
        let body = String::from_utf8_lossy(&request.body);
        let sdp = Sdp::parse(&body);

        let Some(alac) = sdp.fmtp.as_deref().and_then(AlacConfig::parse) else {
            warn!("ANNOUNCE without a usable a=fmtp ALAC line");
            return Response::new(400, "Bad Request");
        };
        if !production_raop_format(&sdp, &alac) {
            warn!(
                "ANNOUNCE rejected unsupported format: payload={} frames={} depth={} channels={} rate={}",
                alac.raw[0],
                alac.frames_per_packet,
                alac.raw[3],
                alac.raw[7],
                alac.sample_rate
            );
            return Response::new(400, "Bad Request");
        }

        let params = match (sdp.rsaaeskey.as_deref(), sdp.aesiv.as_deref()) {
            (None, None) => {
                debug!("ANNOUNCE: unencrypted stream, {} Hz", alac.sample_rate);
                StreamParams {
                    encrypted: false,
                    key: [0; 16],
                    iv: [0; 16],
                    alac,
                }
            }
            (Some(rsaaeskey), Some(aesiv)) => match decrypt_stream_key(rsaaeskey, aesiv) {
                Ok((key, iv)) => {
                    debug!("ANNOUNCE: encrypted stream, {} Hz", alac.sample_rate);
                    StreamParams {
                        encrypted: true,
                        key,
                        iv,
                        alac,
                    }
                }
                Err(e) => {
                    warn!("ANNOUNCE crypto material rejected: {e}");
                    return Response::new(456, "Header Field Not Valid for Resource");
                }
            },
            _ => {
                warn!("ANNOUNCE has exactly one of rsaaeskey/aesiv");
                return Response::new(456, "Header Field Not Valid for Resource");
            }
        };

        self.params = Some(params);
        Response::ok()
    }

    /// Handle SETUP: bind the three UDP sockets, spawn the receiver tasks,
    /// and report our ports back in the Transport header.
    pub async fn handle_setup(&mut self, request: &Request, local_ip: IpAddr) -> Response {
        self.observe_remote_control(request);
        let Some(params) = self.params.clone() else {
            warn!("SETUP before ANNOUNCE");
            return Response::new(455, "Method Not Valid in This State");
        };
        // SETUP is a one-shot state transition for this RTSP session. In
        // particular, reject a replay before binding sockets or spawning
        // tasks: otherwise one connection can grow both without bound.
        if self.streaming || self.slot_guard.is_some() {
            warn!("SETUP refused: this session is already streaming");
            return Response::new(455, "Method Not Valid in This State");
        }
        let Some(transport) = request.headers.get("Transport") else {
            return Response::new(400, "Bad Request");
        };
        // Validate every transport property that selects the supported RAOP
        // path before taking the receiver-wide single-session slot. A malformed
        // connection must not be able to exclude a legitimate sender.
        let Some(SetupTransport {
            control_port,
            timing_port,
        }) = parse_setup_transport(transport)
        else {
            warn!("SETUP rejected invalid Transport: {transport:?}");
            return Response::new(400, "Bad Request");
        };

        // One streaming session at a time; refuse a second client. Keep the
        // guard local until setup succeeds so any bind failure releases it.
        let Some(slot_guard) = self.slot.try_acquire() else {
            warn!("SETUP refused: another session is already streaming");
            return Response::new(453, "Not Enough Bandwidth");
        };

        // Bind on the interface the RTSP connection arrived on so the client
        // can reach us; fall back to all-interfaces if that address is odd.
        let bind_ip = if local_ip.is_unspecified() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            local_ip
        };
        let (audio, control, timing) = match bind_three(bind_ip).await {
            Ok(sockets) => sockets,
            Err(e) => {
                warn!("SETUP could not bind UDP sockets: {e}");
                return Response::new(500, "Internal Server Error");
            }
        };
        self.local_audio_port = audio.local_addr().map(|a| a.port()).unwrap_or(0);
        self.local_control_port = control.local_addr().map(|a| a.port()).unwrap_or(0);
        self.local_timing_port = timing.local_addr().map(|a| a.port()).unwrap_or(0);

        debug!(
            "SETUP: client control={control_port} timing={timing_port}; \
             ours audio={} control={} timing={}",
            self.local_audio_port, self.local_control_port, self.local_timing_port
        );

        // Shared clock model, updated by the timing exchange (offset) and the
        // sync packets (anchor), read by the player for latency-correct start.
        let clock = Arc::new(Mutex::new(ClockModel::new(params.alac.sample_rate)));

        // The host's sink for this stream, created with the negotiated format;
        // the player thread decodes and feeds it.
        let stream_id = self.slot.allocate_stream_id();
        self.stream_id = Some(stream_id);
        self.streaming = true;
        self.send_event(Event::SessionStarted {
            stream_id,
            rate: params.alac.sample_rate,
            channels: params.alac.channels,
            peer: self.peer_addr,
            remote_control: self.remote_control.clone(),
        });
        self.publish_remote_control_state();
        // Replay metadata/artwork that arrived before the session started,
        // so they land inside it.
        if let Some(metadata) = self.pending_metadata.take() {
            self.send_event(Event::Metadata {
                stream_id,
                title: metadata.title,
                artist: metadata.artist,
                album: metadata.album,
            });
        }
        if let Some(artwork) = self.pending_artwork.take() {
            self.send_event(Event::Artwork {
                stream_id,
                content_type: artwork.content_type,
                data: artwork.data,
            });
        }
        let sink = (self.sink_factory)(stream_id, params.alac.sample_rate, params.alac.channels);
        let player = Player::spawn(
            stream_id,
            &params.alac,
            sink,
            clock.clone(),
            self.events.clone(),
            self.track.clone(),
        );
        let player_sender = player.sender();
        self.player = Some(player);

        // Share the control socket: the audio task sends resend requests on it
        // while the control task reads sync packets.
        let control = Arc::new(control);
        let timing = Arc::new(timing);
        let mut client_control = self.peer_addr;
        client_control.set_port(control_port);
        let mut client_timing = self.peer_addr;
        client_timing.set_port(timing_port);
        let (flush_tx, flush_rx) = tokio::sync::mpsc::channel(FLUSH_QUEUE_CAPACITY);
        self.flush_tx = Some(flush_tx);
        // Retransmitted audio arrives on the control channel; the control task
        // forwards it to the audio task, which owns the key and the buffer.
        let (resend_tx, resend_rx) = tokio::sync::mpsc::channel(RESEND_QUEUE_CAPACITY);
        self.tasks.push(tokio::spawn(audio_receiver(
            audio,
            params,
            self.observer.clone(),
            Some(player_sender),
            control.clone(),
            client_control,
            AudioInbox {
                flush: flush_rx,
                resends: resend_rx,
            },
        )));
        self.tasks.push(tokio::spawn(control_receiver(
            control,
            clock.clone(),
            resend_tx,
            self.peer_ip,
        )));
        self.tasks
            .push(tokio::spawn(timing_task(timing, client_timing, clock)));
        self.slot_guard = Some(slot_guard);

        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={};server_port={}",
            self.local_control_port, self.local_timing_port, self.local_audio_port
        );
        Response::ok()
            .header("Transport", transport)
            .header("Session", "1")
    }

    /// Handle RECORD: streaming starts. Report the minimum latency.
    pub fn handle_record(&mut self, request: &Request) -> Response {
        if let Some(info) = request.headers.get("RTP-Info") {
            let seq = transport_param(info, "seq");
            let rtptime = info
                .split(';')
                .find_map(|kv| kv.trim().strip_prefix("rtptime="))
                .and_then(|v| v.parse::<u32>().ok());
            debug!("RECORD: initial seq={seq:?} rtptime={rtptime:?}");
        }
        Response::ok().header("Audio-Latency", "11025")
    }

    /// Handle FLUSH/TEARDOWN/SET_PARAMETER/GET_PARAMETER. TEARDOWN stops the
    /// UDP tasks. Returns None for methods this session doesn't own.
    pub fn handle_other(&mut self, request: &Request) -> Option<Response> {
        self.observe_remote_control(request);
        match request.method.as_str() {
            "TEARDOWN" => {
                self.stop_tasks();
                self.player = None;
                self.flush_tx = None;
                self.end_session();
                // Publish the ended sideband before another connection can
                // acquire the slot and publish its newer credentials.
                self.slot_guard = None; // release the streaming slot immediately
                debug!("TEARDOWN: session closed");
                Some(Response::ok())
            }
            "FLUSH" => {
                if !self.streaming {
                    return Some(Response::new(455, "Method Not Valid in This State"));
                }
                // Clear buffered audio up to the RTP-Info seq (if any) so a
                // seek/pause doesn't play stale audio.
                let flush_to = request
                    .headers
                    .get("RTP-Info")
                    .and_then(|info| transport_param(info, "seq"));
                if let Some(tx) = &self.flush_tx {
                    let _ = tx.try_send(flush_to);
                }
                if let Some(stream_id) = self.stream_id {
                    self.send_event(Event::Flushed { stream_id });
                }
                Some(Response::ok())
            }
            "SET_PARAMETER" => {
                self.set_parameter(request.headers.get("Content-Type"), &request.body);
                Some(Response::ok())
            }
            "GET_PARAMETER" => Some(Response::ok()),
            _ => None,
        }
    }

    /// Apply a `SET_PARAMETER` body, dispatched on its `Content-Type`:
    /// DMAP track metadata, cover art, or (the default) `text/parameters`
    /// lines — currently the volume.
    fn set_parameter(&mut self, content_type: Option<&str>, body: &[u8]) {
        // Strip any parameters ("; charset=...") from the media type.
        let media_type = content_type.map(|ct| ct.split(';').next().unwrap_or(ct).trim());
        match media_type {
            Some(ct) if ct.eq_ignore_ascii_case(DMAP_CONTENT_TYPE) => self.set_metadata(body),
            Some(ct)
                if ct
                    .get(..6)
                    .is_some_and(|p| p.eq_ignore_ascii_case("image/")) =>
            {
                self.set_artwork(ct, body)
            }
            _ => self.set_text_parameters(body),
        }
    }

    /// The `text/parameters` flavor: the volume line and the playback
    /// position. The library does not apply gain; the host owns that path.
    fn set_text_parameters(&mut self, body: &[u8]) {
        let text = String::from_utf8_lossy(body);
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("volume:") {
                if !self.streaming {
                    continue;
                }
                if let Ok(db) = v.trim().parse::<f32>() {
                    if db.is_finite() {
                        debug!("SET_PARAMETER volume {db} dB");
                        if let Some(stream_id) = self.stream_id {
                            let revision = self.slot.allocate_volume_revision();
                            if let Some(latest_volume) = &self.latest_volume {
                                let _ = latest_volume.send(Some(VolumeUpdate {
                                    stream_id,
                                    revision,
                                    db,
                                }));
                            } else {
                                self.send_event(Event::Volume {
                                    stream_id,
                                    revision,
                                    db,
                                });
                            }
                        }
                    }
                }
            } else if let Some(v) = line.strip_prefix("progress:") {
                self.set_progress(v.trim());
            }
        }
    }

    /// `progress: <start>/<current>/<end>` — three RTP timestamps. Converted
    /// to durations with the stream's sample rate so no wire concept reaches
    /// the host; reported only while a session is running, since a position
    /// without a stream means nothing.
    fn set_progress(&mut self, value: &str) {
        let Some(params) = &self.params else { return };
        if !self.streaming {
            return;
        }
        let rate = params.alac.sample_rate;
        let mut parts = value.split('/').map(|p| p.trim().parse::<u32>());
        let (Some(Ok(start)), Some(Ok(current)), Some(Ok(end)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            debug!("SET_PARAMETER progress: unparseable value {value:?}");
            return;
        };
        // The sender tells us where the track begins and ends on the RTP
        // timeline; the playback thread turns that into a position that
        // actually follows the audio.
        *self.track.lock().unwrap() = Some(Track { start, end });

        // RTP timestamps wrap and a seek can put `current` before `start`;
        // saturating subtraction keeps both readings sane rather than
        // reporting a position of ~27 hours.
        let elapsed = frames_to_duration(current.saturating_sub(start), rate);
        let duration = frames_to_duration(end.saturating_sub(start), rate);
        debug!(
            "SET_PARAMETER progress {:.1}s / {:.1}s",
            elapsed.as_secs_f32(),
            duration.as_secs_f32()
        );
        if let Some(stream_id) = self.stream_id {
            self.send_event(Event::Progress {
                stream_id,
                elapsed,
                duration,
            });
        }
    }

    /// DMAP track metadata. Metadata is decoration: an unparseable payload
    /// is dropped with a debug log, never an error to the sender.
    fn set_metadata(&mut self, body: &[u8]) {
        let Some(meta) = dmap::parse(body) else {
            debug!(
                "SET_PARAMETER metadata: unrecognized DMAP payload ({} bytes)",
                body.len()
            );
            return;
        };
        debug!(
            "SET_PARAMETER metadata: title={:?} artist={:?} album={:?}",
            meta.title, meta.artist, meta.album
        );
        if let Some(stream_id) = self.stream_id.filter(|_| self.streaming) {
            self.send_event(Event::Metadata {
                stream_id,
                title: meta.title,
                artist: meta.artist,
                album: meta.album,
            });
        } else {
            self.pending_metadata = Some(meta);
        }
    }

    /// Cover art, forwarded as-is (`image/none`/empty means cleared).
    fn set_artwork(&mut self, content_type: &str, body: &[u8]) {
        debug!(
            "SET_PARAMETER artwork: {content_type}, {} bytes",
            body.len()
        );
        if let Some(stream_id) = self.stream_id.filter(|_| self.streaming) {
            self.send_event(Event::Artwork {
                stream_id,
                content_type: content_type.to_string(),
                data: body.to_vec(),
            });
        } else {
            self.pending_artwork = Some(PendingArtwork {
                content_type: content_type.to_string(),
                data: body.to_vec(),
            });
        }
    }

    fn send_event(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    pub(crate) fn observe_remote_control(&mut self, request: &Request) {
        let update = remote_control_headers(request);
        if update.dacp_id.is_none() && update.active_remote.is_none() {
            return;
        }

        let previous = self.remote_control.clone();
        if let Some(value) = update.dacp_id {
            self.remote_dacp_id = Some(value);
        }
        if let Some(value) = update.active_remote {
            self.remote_active_remote = Some(value);
        }
        if let (Some(dacp_id), Some(active_remote)) = (
            self.remote_dacp_id.clone(),
            self.remote_active_remote.clone(),
        ) {
            let current = RemoteControl {
                dacp_id,
                active_remote,
            };
            if previous.as_ref() != Some(&current) {
                self.remote_control = Some(current);
                if self.streaming {
                    self.publish_remote_control_state();
                }
            }
        }
    }

    fn publish_remote_control_state(&self) {
        let (Some(sender), Some(stream_id), Some(params)) = (
            &self.latest_remote_control,
            self.stream_id,
            self.params.as_ref(),
        ) else {
            return;
        };
        let _ = sender.send(Some(RemoteControlState::Active(RemoteControlSession {
            stream_id,
            peer: self.peer_addr,
            rate: params.alac.sample_rate,
            channels: params.alac.channels,
            remote_control: self.remote_control.clone(),
        })));
    }

    /// Report [`Event::SessionEnded`] once per started session.
    fn end_session(&mut self) {
        *self.track.lock().unwrap() = None;
        if self.streaming {
            self.streaming = false;
            let stream_id = self
                .stream_id
                .expect("streaming sessions always have a SETUP identity");
            if let Some(latest_volume) = &self.latest_volume {
                let _ = latest_volume.send(None);
            }
            if let Some(latest_remote_control) = &self.latest_remote_control {
                let _ = latest_remote_control.send(Some(RemoteControlState::Ended { stream_id }));
            }
            self.send_event(Event::SessionEnded { stream_id });
        }
        self.stream_id = None;
    }

    fn stop_tasks(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop_tasks();
        self.end_session();
    }
}

/// Convert a frame count at `rate` Hz to a wall-clock duration.
fn frames_to_duration(frames: u32, rate: u32) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(frames as f64 / rate as f64)
}

fn production_raop_format(sdp: &Sdp, alac: &AlacConfig) -> bool {
    let mut rtpmap = sdp.rtpmap.as_deref().unwrap_or_default().split_whitespace();
    let rtpmap_ok = matches!(
        (rtpmap.next(), rtpmap.next(), rtpmap.next()),
        (Some("96"), Some(codec), None) if codec.eq_ignore_ascii_case("AppleLossless")
    );
    rtpmap_ok
        && alac.raw[0] == 96
        && (128..=4096).contains(&alac.frames_per_packet)
        && alac.raw[3] == 16
        && alac.raw[7] == 2
        && alac.sample_rate == 44_100
}

/// Decrypt the AES key (RSA-OAEP) and decode the IV (base64, 16 bytes).
fn decrypt_stream_key(
    rsaaeskey_b64: &str,
    aesiv_b64: &str,
) -> Result<([u8; 16], [u8; 16]), String> {
    let iv_bytes = sdp_base64(aesiv_b64).map_err(|_| "aesiv is not valid base64".to_string())?;
    let iv: [u8; 16] = iv_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("aesiv is {} bytes, wanted 16", iv_bytes.len()))?;

    let key_ct =
        sdp_base64(rsaaeskey_b64).map_err(|_| "rsaaeskey is not valid base64".to_string())?;
    let key = crypto::decrypt_aes_key(&key_ct).map_err(|e| e.to_string())?;
    Ok((key, iv))
}

/// Decode a base64 value from an ANNOUNCE SDP field. Apple sends `aesiv` and
/// `rsaaeskey` on the standard alphabet but *without* `=` padding; tolerate
/// its presence or absence so both stock senders and padded encoders work.
fn sdp_base64(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD_NO_PAD.decode(value.trim().trim_end_matches('='))
}

fn transport_param(header: &str, name: &str) -> Option<u16> {
    header
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")))
        .and_then(|v| v.trim().parse().ok())
}

/// Extract the two bearer values as one all-or-nothing capability. Stock Apple
/// senders use a hexadecimal DACP id and a decimal Active-Remote token. Keeping
/// that grammar tight prevents an untrusted RTSP header from becoming an mDNS
/// service name or an outbound HTTP header with attacker-controlled syntax.
#[derive(Debug, Default, PartialEq, Eq)]
struct RemoteControlHeaders {
    dacp_id: Option<String>,
    active_remote: Option<String>,
}

fn remote_control_headers(request: &Request) -> RemoteControlHeaders {
    let dacp_id = request.headers.get("DACP-ID").and_then(|value| {
        let value = value.trim();
        (!value.is_empty()
            && value.len() <= 16
            && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_string())
    });
    let active_remote = request.headers.get("Active-Remote").and_then(|value| {
        let value = value.trim();
        (!value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.to_string())
    });
    RemoteControlHeaders {
        dacp_id,
        active_remote,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SetupTransport {
    control_port: u16,
    timing_port: u16,
}

/// Parse the sender-facing SETUP transport without accepting an ambiguous or
/// unusable endpoint. Unknown key/value extensions remain allowed, as stock
/// senders add optional RAOP fields that do not change this UDP/unicast path.
fn parse_setup_transport(header: &str) -> Option<SetupTransport> {
    let mut parts = header.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/UDP") {
        return None;
    }

    let mut unicast = false;
    let mut mode_record = false;
    let mut control_port = None;
    let mut timing_port = None;
    for part in parts {
        if part.eq_ignore_ascii_case("unicast") {
            if unicast {
                return None;
            }
            unicast = true;
            continue;
        }
        if part.eq_ignore_ascii_case("multicast") {
            return None;
        }
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("mode") {
            if mode_record || !value.trim_matches('"').eq_ignore_ascii_case("record") {
                return None;
            }
            mode_record = true;
        } else if key.eq_ignore_ascii_case("control_port") {
            if control_port.is_some() {
                return None;
            }
            control_port = value.parse::<u16>().ok().filter(|port| *port != 0);
            control_port?;
        } else if key.eq_ignore_ascii_case("timing_port") {
            if timing_port.is_some() {
                return None;
            }
            timing_port = value.parse::<u16>().ok().filter(|port| *port != 0);
            timing_port?;
        }
    }

    Some(SetupTransport {
        control_port: control_port.filter(|_| unicast && mode_record)?,
        timing_port: timing_port?,
    })
}

/// Extract `key="value"` from an RFC 2617 `Authorization: Digest` header.
fn digest_param<'a>(auth: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = auth.find(&needle)?;
    let value = &auth[start + needle.len()..];
    let end = value.find('"')?;
    Some(&value[..end])
}

async fn bind_three(ip: IpAddr) -> io::Result<(UdpSocket, UdpSocket, UdpSocket)> {
    let bind = |ip| async move { UdpSocket::bind(SocketAddr::new(ip, 0)).await };
    Ok((bind(ip).await?, bind(ip).await?, bind(ip).await?))
}

/// Receive the next UDP datagram from the host that owns the authenticated
/// RTSP connection. RAOP negotiates its media sockets over RTSP, but UDP
/// itself has no session binding; accepting every source would let another
/// LAN host inject audio, sync, or timing packets into an active stream.
async fn recv_peer_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
    peer_ip: IpAddr,
) -> io::Result<usize> {
    loop {
        let (n, source) = socket.recv_from(buf).await?;
        if same_host_ip(source.ip(), peer_ip) {
            return Ok(n);
        }
        debug!(
            "ignored UDP datagram from unexpected peer {} (expected {peer_ip})",
            source.ip()
        );
    }
}

fn same_host_ip(left: IpAddr, right: IpAddr) -> bool {
    fn canonical(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            IpAddr::V4(_) => ip,
        }
    }

    canonical(left) == canonical(right)
}

/// Rate-limits resend requests so each missing sequence is asked for at most
/// once per [`RESEND_BACKOFF`].
#[derive(Default)]
struct ResendTracker {
    last: HashMap<u16, Instant>,
}

impl ResendTracker {
    /// Return the subset of `missing` due for a (re)request now, recording the
    /// request time. Entries no longer missing are pruned.
    fn due(&mut self, missing: &[u16], now: Instant) -> Vec<u16> {
        let live: std::collections::HashSet<u16> = missing.iter().copied().collect();
        self.last.retain(|seq, _| live.contains(seq));
        let mut due = Vec::new();
        for &seq in missing {
            let ready = self
                .last
                .get(&seq)
                .is_none_or(|&t| now.duration_since(t) >= RESEND_BACKOFF);
            if ready {
                self.last.insert(seq, now);
                due.push(seq);
            }
        }
        due
    }

    fn clear(&mut self) {
        self.last.clear();
    }
}

async fn audio_receiver(
    socket: UdpSocket,
    params: StreamParams,
    observer: Option<AudioObserver>,
    player: Option<PlayerSender>,
    control: Arc<UdpSocket>,
    client_control: SocketAddr,
    mut inbox: AudioInbox,
) {
    // Real RAOP uses 352-frame packets (~1.4 KiB), but the announced frame
    // length can be larger; 16 KiB holds a 4096-frame 16-bit stereo packet
    // and then some. Too-small a buffer silently truncates the datagram and
    // the payload fails to decode.
    let mut buf = vec![0u8; 16 * 1024];
    let frames_per_packet = params.alac.frames_per_packet;
    let mut jitter = JitterBuffer::default();
    let mut resend = ResendTracker::default();
    // (seq, timestamp) of the first packet, to derive delivered timestamps.
    let mut anchor: Option<(u16, u32)> = None;
    let mut received: u64 = 0;
    let mut service = tokio::time::interval(SERVICE_INTERVAL);

    loop {
        tokio::select! {
            result = recv_peer_datagram(&socket, &mut buf, client_control.ip()) => {
                let n = match result {
                    Ok(n) => n,
                    Err(e) => {
                        warn!("audio socket error: {e}");
                        return;
                    }
                };
                let Some(packet) = AudioPacket::parse(&buf[..n]) else {
                    debug!("audio: ignored {n}-byte non-audio datagram");
                    continue;
                };
                received += 1;
                anchor.get_or_insert((packet.sequence, packet.timestamp));

                let frame = if params.encrypted {
                    rtp::decrypt_audio(packet.payload, &params.key, &params.iv)
                } else {
                    packet.payload.to_vec()
                };
                if received <= 3 || received.is_multiple_of(250) {
                    debug!(
                        "audio: {received} pkts, seq={} ts={} {} bytes",
                        packet.sequence, packet.timestamp, frame.len()
                    );
                }
                jitter.insert(packet.sequence, frame);
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
            }
            datagram = inbox.resends.recv() => {
                // A packet we asked for, arriving on the control channel.
                let Some(datagram) = datagram else { return }; // session dropped
                let Some(packet) = AudioPacket::parse(&datagram) else {
                    debug!("audio: unparseable {}-byte resend reply", datagram.len());
                    continue;
                };
                let frame = if params.encrypted {
                    rtp::decrypt_audio(packet.payload, &params.key, &params.iv)
                } else {
                    packet.payload.to_vec()
                };
                debug!(
                    "audio: resend reply seq={} ts={} {} bytes",
                    packet.sequence, packet.timestamp, frame.len()
                );
                jitter.insert(packet.sequence, frame);
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
            }
            _ = service.tick() => {
                drain_deliveries(&mut jitter, &player, &observer, anchor, frames_per_packet);
                request_resends(&jitter, &mut resend, &control, client_control).await;
            }
            flush = inbox.flush.recv() => {
                let Some(flush_to) = flush else { return }; // session dropped
                debug!("audio: FLUSH to {flush_to:?}");
                jitter.reset(flush_to);
                resend.clear();
                anchor = None;
                if let Some(player) = &player {
                    player.flush();
                }
            }
        }
    }
}

/// Release ready packets from the jitter buffer to the player (and test
/// observer), concealing losses with silence.
fn drain_deliveries(
    jitter: &mut JitterBuffer,
    player: &Option<PlayerSender>,
    observer: &Option<AudioObserver>,
    anchor: Option<(u16, u32)>,
    frames_per_packet: u32,
) {
    for delivery in jitter.pop_ready() {
        match delivery {
            Delivery::Packet { seq, frame } => {
                let ts = derive_ts(seq, anchor, frames_per_packet);
                let looks_like_alac = rtp::looks_like_alac_stereo(&frame);
                match (player, observer) {
                    (Some(player), Some(observer)) => {
                        player.frame(ts, frame.clone());
                        let _ = observer.try_send(decrypted(seq, ts, frame, looks_like_alac));
                    }
                    (Some(player), None) => player.frame(ts, frame),
                    (None, Some(observer)) => {
                        let _ = observer.try_send(decrypted(seq, ts, frame, looks_like_alac));
                    }
                    (None, None) => {}
                }
            }
            Delivery::Lost { seq } => {
                debug!("audio: concealing lost packet seq={seq}");
                if let Some(player) = player {
                    player.silence(derive_ts(seq, anchor, frames_per_packet));
                }
            }
        }
    }
}

/// The RTP timestamp for a delivered sequence number: RAOP timestamps advance
/// by `frames_per_packet` per sequence number from the first packet's anchor.
fn derive_ts(seq: u16, anchor: Option<(u16, u32)>, frames_per_packet: u32) -> u32 {
    anchor.map_or(0, |(a_seq, a_ts)| {
        let ahead = crate::jitter::seq_diff(seq, a_seq).max(0) as u32;
        a_ts.wrapping_add(ahead.wrapping_mul(frames_per_packet))
    })
}

fn decrypted(seq: u16, timestamp: u32, frame: Vec<u8>, looks_like_alac: bool) -> DecryptedAudio {
    DecryptedAudio {
        sequence: seq,
        timestamp,
        looks_like_alac,
        frame,
    }
}

/// Ask the client to resend any missing packets, respecting per-seq backoff.
async fn request_resends(
    jitter: &JitterBuffer,
    resend: &mut ResendTracker,
    control: &UdpSocket,
    client_control: SocketAddr,
) {
    let missing = jitter.missing();
    if missing.is_empty() {
        return;
    }
    for seq in resend.due(&missing, Instant::now()) {
        let req = rtp::resend_request(seq, 1);
        if let Err(e) = control.send_to(&req, client_control).await {
            debug!("audio: resend request for seq={seq} failed: {e}");
        }
    }
}

/// What the RTSP path and the control task send to the audio task: FLUSH
/// boundaries (a sequence to flush to, or `None` to re-anchor) and the
/// retransmitted audio packets that arrive on the control channel.
struct AudioInbox {
    flush: tokio::sync::mpsc::Receiver<Option<u16>>,
    resends: tokio::sync::mpsc::Receiver<Vec<u8>>,
}

/// Read the control channel: parse `0xd4` sync packets and update the clock
/// model's anchor (the frame at the DAC at a given client-clock instant), and
/// hand retransmitted audio packets to the audio task.
///
/// A sender answers our resend requests **here**, on the control channel — not
/// on the audio channel — so a reply that stops at this task is a gap the
/// jitter buffer can never fill.
async fn control_receiver(
    socket: Arc<UdpSocket>,
    clock: Arc<Mutex<ClockModel>>,
    resends: tokio::sync::mpsc::Sender<Vec<u8>>,
    peer_ip: IpAddr,
) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = recv_peer_datagram(&socket, &mut buf, peer_ip).await {
        if let Some(sync) = rtp::parse_sync(&buf[..n]) {
            clock
                .lock()
                .unwrap()
                .set_anchor(sync.remote_time_ns, sync.rtp_at_dac);
        } else if let Some(kind) = rtp::classify_control(&buf[..n]) {
            if kind == rtp::ControlKind::RetransmitResponse {
                // The audio task owns the session key and the jitter buffer.
                match resends.try_send(buf[..n].to_vec()) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        return; // audio task gone: the session is over
                    }
                }
            } else {
                debug!("control: {kind:?} ({n} bytes)");
            }
        }
    }
}

/// The NTP timing exchange: periodically send a `0xd2` request to the client's
/// timing port and fold each `0xd3` reply into the clock model's offset. An
/// initial fast burst converges quickly, then it settles to ~every 3 s.
async fn timing_task(
    socket: Arc<UdpSocket>,
    client_timing: SocketAddr,
    clock: Arc<Mutex<ClockModel>>,
) {
    let mut buf = [0u8; 2048];
    let mut request_count: u64 = 0;

    loop {
        // Send a timing request and record when it left (t1).
        let departure_ns = clock::now_ns();
        if let Err(e) = socket.send_to(&rtp::timing_request(), client_timing).await {
            debug!("timing: request send failed: {e}");
        }
        request_count += 1;
        let interval = if request_count <= 3 {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(3)
        };

        // Collect replies until it's time to send the next request.
        let deadline = tokio::time::sleep(interval);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                result = recv_peer_datagram(&socket, &mut buf, client_timing.ip()) => {
                    let Ok(n) = result else { return };
                    let arrival_ns = clock::now_ns();
                    if let Some(reply) = rtp::parse_timing_reply(&buf[..n]) {
                        clock.lock().unwrap().add_timing(
                            departure_ns,
                            reply.receive_ns,
                            reply.transmit_ns,
                            arrival_ns,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    use tokio::io::BufReader;

    struct DiscardSink;

    impl crate::sink::AudioSink for DiscardSink {
        fn write(&mut self, _pcm: &[i16]) {}
        fn flush(&mut self) {}
    }

    fn session() -> (Session, tokio::sync::mpsc::Receiver<Event>) {
        session_with_slot(SessionSlot::new())
    }

    fn session_with_slot(slot: SessionSlot) -> (Session, tokio::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let factory: SessionSinkFactory =
            Arc::new(|_stream_id, _rate, _channels| Box::new(DiscardSink));
        let session = Session::new(
            None,
            factory,
            tx,
            None,
            None,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
            slot,
        );
        (session, rx)
    }

    async fn request(raw: &str) -> Request {
        let mut reader = BufReader::new(Cursor::new(raw.as_bytes().to_vec()));
        crate::rtsp::read_request(&mut reader)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn udp_channels_ignore_datagrams_from_a_different_host() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();

        sender.send_to(b"foreign", receiver_addr).await.unwrap();
        let mut buf = [0u8; 32];
        let foreign = tokio::time::timeout(
            Duration::from_millis(100),
            recv_peer_datagram(&receiver, &mut buf, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
        )
        .await;
        assert!(
            foreign.is_err(),
            "a datagram from an unexpected source must not be returned"
        );

        sender.send_to(b"expected", receiver_addr).await.unwrap();
        let n = tokio::time::timeout(
            Duration::from_secs(1),
            recv_peer_datagram(&receiver, &mut buf, IpAddr::V4(Ipv4Addr::LOCALHOST)),
        )
        .await
        .expect("the expected peer's datagram must be received")
        .unwrap();

        assert_eq!(&buf[..n], b"expected");
    }

    #[test]
    fn udp_peer_matching_treats_ipv4_mapped_ipv6_as_the_same_host() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 8).to_ipv6_mapped());

        assert!(same_host_ip(v4, mapped));
        assert!(!same_host_ip(v4, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9))));
    }

    const SDP_BODY: &str = "v=0\r\n\
        o=iTunes 3413821438 0 IN IP4 192.168.1.2\r\n\
        s=iTunes\r\n\
        c=IN IP4 192.168.1.10\r\n\
        t=0 0\r\n\
        m=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

    const AUTH_PASSWORD: &str = "correct horse battery staple";
    const AUTH_USERNAME: &str = "AudioHubProbe";
    const AUTH_URI: &str = "rtsp://audiohub.local/1";

    async fn issue_auth_challenge(session: &mut Session) -> String {
        let unauthenticated = request("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n").await;
        let denied = session
            .authenticate(Some(AUTH_PASSWORD), &unauthenticated)
            .expect("protected session must challenge");
        assert_eq!(denied.status(), 401);
        session
            .auth_nonce
            .clone()
            .expect("challenge must install a nonce")
    }

    async fn digest_request(
        realm: &str,
        header_nonce: &str,
        header_uri: &str,
        digest_nonce: &str,
    ) -> Request {
        let response = crypto::digest_response(
            AUTH_USERNAME,
            realm,
            AUTH_PASSWORD,
            "ANNOUNCE",
            header_uri,
            digest_nonce,
        );
        request(&format!(
            "ANNOUNCE {AUTH_URI} RTSP/1.0\r\n\
             CSeq: 2\r\n\
             Authorization: Digest username=\"{AUTH_USERNAME}\", realm=\"{realm}\", nonce=\"{header_nonce}\", uri=\"{header_uri}\", response=\"{response}\"\r\n\
             \r\n"
        ))
        .await
    }

    fn assert_auth_denied(session: &Session, response: Option<Response>, nonce: &str) {
        assert_eq!(
            response
                .expect("invalid Digest must be challenged")
                .status(),
            401
        );
        assert!(!session.is_authorized());
        assert_eq!(session.auth_nonce.as_deref(), Some(nonce));
    }

    #[tokio::test]
    async fn digest_auth_rejects_authorization_before_challenge() {
        let (mut session, _events) = session();
        let authorization = digest_request(RAOP_AUTH_REALM, "", AUTH_URI, "").await;

        let denied = session.authenticate(Some(AUTH_PASSWORD), &authorization);

        assert_eq!(
            denied.expect("a challenge must be issued first").status(),
            401
        );
        assert!(!session.is_authorized());
        assert!(session
            .auth_nonce
            .as_deref()
            .is_some_and(|nonce| !nonce.is_empty()));
    }

    #[tokio::test]
    async fn digest_auth_rejects_mismatched_realm() {
        let (mut session, _events) = session();
        let nonce = issue_auth_challenge(&mut session).await;
        // This response is internally valid for the forged realm. The old
        // implementation accepted it because it trusted the header realm.
        let authorization = digest_request("not-raop", &nonce, AUTH_URI, &nonce).await;

        let denied = session.authenticate(Some(AUTH_PASSWORD), &authorization);

        assert_auth_denied(&session, denied, &nonce);
    }

    #[tokio::test]
    async fn digest_auth_rejects_mismatched_nonce() {
        let (mut session, _events) = session();
        let nonce = issue_auth_challenge(&mut session).await;
        // Compute the otherwise-correct response with the issued nonce, but
        // lie in the Authorization parameter. It must still be rejected.
        let authorization =
            digest_request(RAOP_AUTH_REALM, "not-the-issued-nonce", AUTH_URI, &nonce).await;

        let denied = session.authenticate(Some(AUTH_PASSWORD), &authorization);

        assert_auth_denied(&session, denied, &nonce);
    }

    #[tokio::test]
    async fn digest_auth_rejects_mismatched_request_uri() {
        let (mut session, _events) = session();
        let nonce = issue_auth_challenge(&mut session).await;
        let other_uri = "rtsp://audiohub.local/other";
        // This response is internally valid for `other_uri`; authentication
        // must bind HA2 to the actual request target instead.
        let authorization = digest_request(RAOP_AUTH_REALM, &nonce, other_uri, &nonce).await;

        let denied = session.authenticate(Some(AUTH_PASSWORD), &authorization);

        assert_auth_denied(&session, denied, &nonce);
    }

    #[tokio::test]
    async fn digest_auth_accepts_matching_challenge_and_request_uri() {
        let (mut session, _events) = session();
        let nonce = issue_auth_challenge(&mut session).await;
        let authorization = digest_request(RAOP_AUTH_REALM, &nonce, AUTH_URI, &nonce).await;

        assert!(session
            .authenticate(Some(AUTH_PASSWORD), &authorization)
            .is_none());
        assert!(session.is_authorized());
    }

    #[tokio::test]
    async fn dacp_capability_is_strict_case_insensitive_and_redacted() {
        let valid = request(
            "OPTIONS * RTSP/1.0\r\n\
             CSeq: 1\r\n\
             dacp-id: 0000A1B2C3D4E5F6\r\n\
             active-remote: 1986535575\r\n\r\n",
        )
        .await;
        let headers = remote_control_headers(&valid);
        let remote = RemoteControl {
            dacp_id: headers.dacp_id.expect("valid DACP id"),
            active_remote: headers.active_remote.expect("valid Active-Remote"),
        };
        assert_eq!(remote.dacp_id, "0000A1B2C3D4E5F6");
        assert_eq!(remote.active_remote, "1986535575");
        let debug = format!("{remote:?}");
        assert!(!debug.contains("1986535575"));
        assert!(debug.contains("<redacted>"));

        assert_eq!(
            remote_control_headers(&request("OPTIONS * RTSP/1.0\r\nDACP-ID: A1\r\n\r\n").await),
            RemoteControlHeaders {
                dacp_id: Some("A1".into()),
                active_remote: None,
            }
        );
        for raw in [
            "OPTIONS * RTSP/1.0\r\nDACP-ID: not-hex\r\nActive-Remote: token\r\n\r\n",
            "OPTIONS * RTSP/1.0\r\nDACP-ID: 11111111111111111\r\nActive-Remote: token\r\n\r\n",
        ] {
            assert_eq!(
                remote_control_headers(&request(raw).await),
                RemoteControlHeaders::default(),
                "{raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn dacp_capability_seen_before_setup_is_latched_into_the_session() {
        let (mut session, mut events) = session();
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\n\
             DACP-ID: 0000A1B2C3D4E5F6\r\n\
             Active-Remote: 1986535575\r\n\
             Content-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        assert_eq!(session.handle_announce(&announce).status(), 200);
        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n",
        )
        .await;
        assert_eq!(
            session
                .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
                .await
                .status(),
            200
        );
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                remote_control: Some(RemoteControl { dacp_id, active_remote }),
                ..
            }) if dacp_id == "0000A1B2C3D4E5F6" && active_remote == "1986535575"
        ));
    }

    #[tokio::test]
    async fn dacp_components_refresh_losslessly_across_authorized_requests() {
        let (event_tx, mut events) = tokio::sync::mpsc::channel(1);
        event_tx
            .try_send(Event::Flushed { stream_id: 999 })
            .unwrap();
        let (remote_tx, mut remote_rx) = tokio::sync::watch::channel(None);
        let sink_stream_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen_stream_id = Arc::clone(&sink_stream_id);
        let factory: SessionSinkFactory = Arc::new(move |stream_id, _rate, _channels| {
            seen_stream_id.store(stream_id, Ordering::Release);
            Box::new(DiscardSink)
        });
        let mut session = Session::new(
            None,
            factory,
            event_tx,
            None,
            Some(remote_tx),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
            SessionSlot::new(),
        );

        // The two components may arrive on separate authorized requests.
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 1\r\nDACP-ID: 0000A1B2C3D4E5F6\r\n\
             Content-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        assert_eq!(session.handle_announce(&announce).status(), 200);
        let token =
            request("OPTIONS * RTSP/1.0\r\nCSeq: 2\r\nActive-Remote: 1986535575\r\n\r\n").await;
        session.observe_remote_control(&token);

        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n",
        )
        .await;
        assert_eq!(
            session
                .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
                .await
                .status(),
            200
        );
        assert_eq!(events.try_recv(), Ok(Event::Flushed { stream_id: 999 }));
        assert!(events.try_recv().is_err(), "SessionStarted queue was full");

        let first = remote_rx
            .borrow_and_update()
            .clone()
            .expect("lossless SETUP sideband");
        let RemoteControlState::Active(first) = first else {
            panic!("SETUP unexpectedly published an end")
        };
        let first_remote = first.remote_control.expect("complete split capability");
        assert_eq!(first.stream_id, 1);
        assert_eq!(sink_stream_id.load(Ordering::Acquire), first.stream_id);
        assert_eq!(first_remote.dacp_id, "0000A1B2C3D4E5F6");
        assert_eq!(first_remote.active_remote, "1986535575");

        // Missing and malformed values preserve the last valid pair.
        let missing = request("OPTIONS * RTSP/1.0\r\nCSeq: 4\r\n\r\n").await;
        session.observe_remote_control(&missing);
        let malformed = request(
            "OPTIONS * RTSP/1.0\r\nCSeq: 5\r\nDACP-ID: bad!\r\nActive-Remote: token\r\n\r\n",
        )
        .await;
        session.observe_remote_control(&malformed);
        assert!(!remote_rx.has_changed().unwrap());

        // A token-only refresh keeps the DACP id and publishes the complete
        // new snapshot despite the still-full ordinary event queue.
        let refreshed =
            request("OPTIONS * RTSP/1.0\r\nCSeq: 6\r\nActive-Remote: 2718281828\r\n\r\n").await;
        session.observe_remote_control(&refreshed);
        remote_rx.changed().await.unwrap();
        let second = remote_rx.borrow_and_update().clone().unwrap();
        let RemoteControlState::Active(second) = second else {
            panic!("refresh unexpectedly published an end")
        };
        let debug = format!("{second:?}");
        let second_remote = second.remote_control.unwrap();
        assert_eq!(second.stream_id, first.stream_id);
        assert_eq!(second.peer, first.peer);
        assert_eq!(second_remote.dacp_id, first_remote.dacp_id);
        assert_eq!(second_remote.active_remote, "2718281828");
        assert!(!debug.contains("1986535575"));
        assert!(!debug.contains("2718281828"));

        drop(session);
        remote_rx.changed().await.unwrap();
        assert_eq!(
            remote_rx.borrow_and_update().clone(),
            Some(RemoteControlState::Ended { stream_id: 1 })
        );
    }

    #[tokio::test]
    async fn session_lifecycle_emits_the_events() {
        let (mut session, mut events) = session();

        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        assert_eq!(session.handle_announce(&announce).status(), 200);
        assert!(events.try_recv().is_err(), "no event before SETUP");

        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        let response = session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                stream_id: 1,
                rate: 44100,
                channels: 2,
                peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
                remote_control: None,
            })
        );

        let set_parameter = request(
            "SET_PARAMETER rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 4\r\nContent-Type: text/parameters\r\nContent-Length: 18\r\n\
             \r\nvolume: -20.000000",
        )
        .await;
        assert_eq!(session.handle_other(&set_parameter).unwrap().status(), 200);
        assert_eq!(
            events.try_recv(),
            Ok(Event::Volume {
                stream_id: 1,
                revision: 1,
                db: -20.0,
            })
        );

        let flush = request("FLUSH rtsp://192.168.1.10/1 RTSP/1.0\r\nCSeq: 5\r\n\r\n").await;
        assert_eq!(session.handle_other(&flush).unwrap().status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::Flushed { stream_id: 1 }));

        let teardown = request("TEARDOWN rtsp://192.168.1.10/1 RTSP/1.0\r\nCSeq: 6\r\n\r\n").await;
        assert_eq!(session.handle_other(&teardown).unwrap().status(), 200);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded { stream_id: 1 }));

        // The end is reported exactly once: drop after TEARDOWN adds nothing.
        drop(session);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropping_a_streaming_session_ends_it() {
        let (mut session, mut events) = session();
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        session.handle_announce(&announce);
        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                stream_id: 1,
                rate: 44100,
                channels: 2,
                peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
                remote_control: None,
            })
        );

        // The client disconnected without TEARDOWN.
        drop(session);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded { stream_id: 1 }));
    }

    #[tokio::test]
    async fn repeated_setup_is_rejected_before_allocating_more_stream_resources() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;
        assert!(session.streaming);
        assert!(session.slot_guard.is_some());
        assert_eq!(session.tasks.len(), 3);
        let ports = (
            session.local_audio_port,
            session.local_control_port,
            session.local_timing_port,
        );

        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 4\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6101;timing_port=6102\r\n\
             \r\n",
        )
        .await;
        let response = session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;

        assert_eq!(response.status(), 455);
        assert_eq!(session.tasks.len(), 3, "no extra UDP tasks were spawned");
        assert_eq!(
            (
                session.local_audio_port,
                session.local_control_port,
                session.local_timing_port,
            ),
            ports,
            "the existing UDP endpoints were not replaced"
        );
        assert!(
            events.try_recv().is_err(),
            "no second session was announced"
        );
    }

    #[tokio::test]
    async fn invalid_setup_transport_does_not_take_the_global_streaming_slot() {
        let slot = SessionSlot::new();
        let (mut malformed, mut malformed_events) = session_with_slot(slot.clone());
        let (mut legitimate, mut legitimate_events) = session_with_slot(slot);

        for session in [&mut malformed, &mut legitimate] {
            let announce = request(&format!(
                "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
                 CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
                SDP_BODY.len()
            ))
            .await;
            assert_eq!(session.handle_announce(&announce).status(), 200);
        }

        let invalid = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;multicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        let response = malformed
            .handle_setup(&invalid, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(response.status(), 400);
        assert!(malformed.slot_guard.is_none());
        assert!(malformed.tasks.is_empty());
        assert_eq!(
            (
                malformed.local_audio_port,
                malformed.local_control_port,
                malformed.local_timing_port,
            ),
            (0, 0, 0),
            "invalid transport was rejected before UDP bind"
        );
        assert!(malformed_events.try_recv().is_err());

        let valid = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        let response = legitimate
            .handle_setup(&valid, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(response.status(), 200, "the global slot remained free");
        assert!(matches!(
            legitimate_events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));
    }

    #[test]
    fn setup_transport_requires_supported_unambiguous_nonzero_udp_endpoints() {
        assert_eq!(
            parse_setup_transport(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002"
            ),
            Some(SetupTransport {
                control_port: 6001,
                timing_port: 6002,
            })
        );

        for invalid in [
            "RTP/AVP/TCP;unicast;mode=record;control_port=6001;timing_port=6002",
            "RTP/AVP/UDP;multicast;mode=record;control_port=6001;timing_port=6002",
            "RTP/AVP/UDP;unicast;mode=play;control_port=6001;timing_port=6002",
            "RTP/AVP/UDP;unicast;mode=record;control_port=0;timing_port=6002",
            "RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=0",
            "RTP/AVP/UDP;unicast;mode=record;control_port=6001",
            "RTP/AVP/UDP;unicast;mode=record;timing_port=6002",
            "RTP/AVP/UDP;unicast;mode=record;control_port=6001;control_port=6003;timing_port=6002",
            "RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002;timing_port=6004",
        ] {
            assert_eq!(
                parse_setup_transport(invalid),
                None,
                "invalid transport was accepted: {invalid}"
            );
        }
    }

    /// A one-track DMAP payload: `mlit` wrapping title/artist/album.
    fn dmap_track(title: &str) -> Vec<u8> {
        fn entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut e = tag.to_vec();
            e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            e.extend_from_slice(payload);
            e
        }
        let inner = [
            entry(b"minm", title.as_bytes()),
            entry(b"asar", b"The Artist"),
            entry(b"asal", b"The Album"),
        ]
        .concat();
        entry(b"mlit", &inner)
    }

    /// Drive ANNOUNCE → SETUP so the session is streaming, and consume the
    /// resulting `SessionStarted`.
    async fn start_session(session: &mut Session, events: &mut tokio::sync::mpsc::Receiver<Event>) {
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{SDP_BODY}",
            SDP_BODY.len()
        ))
        .await;
        session.handle_announce(&announce);
        let setup = request(
            "SETUP rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 3\r\n\
             Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\
             \r\n",
        )
        .await;
        session
            .handle_setup(&setup, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;
        assert_eq!(
            events.try_recv(),
            Ok(Event::SessionStarted {
                stream_id: 1,
                rate: 44100,
                channels: 2,
                peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
                remote_control: None,
            })
        );
    }

    #[tokio::test]
    async fn set_parameter_dispatches_on_content_type() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Song"));
        assert_eq!(
            events.try_recv(),
            Ok(Event::Metadata {
                stream_id: 1,
                title: Some("Song".into()),
                artist: Some("The Artist".into()),
                album: Some("The Album".into()),
            })
        );

        // Artwork is forwarded byte-for-byte, content type included.
        session.set_parameter(Some("image/png"), b"\x89PNG");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                stream_id: 1,
                content_type: "image/png".into(),
                data: b"\x89PNG".to_vec(),
            })
        );

        // image/none with an empty body is the sender clearing the art.
        session.set_parameter(Some("image/none"), b"");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Artwork {
                stream_id: 1,
                content_type: "image/none".into(),
                data: Vec::new(),
            })
        );

        // The volume path is unchanged, with or without a charset parameter.
        session.set_parameter(Some("text/parameters; charset=utf-8"), b"volume: -12.5\r\n");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Volume {
                stream_id: 1,
                revision: 1,
                db: -12.5,
            })
        );
        session.set_parameter(None, b"volume: -6.0\r\n");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Volume {
                stream_id: 1,
                revision: 2,
                db: -6.0,
            })
        );

        // Anything else is acknowledged but produces no event.
        session.set_parameter(Some("application/octet-stream"), b"\x00\x01");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn latest_volume_bypasses_full_event_queue_and_coalesces() {
        let (events, mut event_rx) = tokio::sync::mpsc::channel(1);
        events.try_send(Event::Flushed { stream_id: 999 }).unwrap();
        let (latest_tx, mut latest_rx) = tokio::sync::watch::channel(None);
        let factory: SessionSinkFactory =
            Arc::new(|_stream_id, _rate, _channels| Box::new(DiscardSink));
        let mut session = Session::new(
            None,
            factory,
            events,
            Some(latest_tx),
            None,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000),
            SessionSlot::new(),
        );
        session.streaming = true;
        session.stream_id = Some(7);

        session.set_text_parameters(b"volume: -30.0\r\nvolume: -20.0\r\nvolume: -6.0\r\n");

        assert_eq!(event_rx.try_recv(), Ok(Event::Flushed { stream_id: 999 }));
        latest_rx.changed().await.unwrap();
        assert_eq!(
            *latest_rx.borrow_and_update(),
            Some(VolumeUpdate {
                stream_id: 7,
                revision: 3,
                db: -6.0,
            })
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), latest_rx.changed())
                .await
                .is_err(),
            "intermediate volume values were not coalesced"
        );
    }

    #[tokio::test]
    async fn progress_is_reported_as_durations() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        // start/current/end as RTP timestamps at 44100 Hz: 10 s in, 180 s
        // long. Senders send this line on its own or next to the volume.
        session.set_parameter(
            Some("text/parameters"),
            b"progress: 1000/442000/7939000\r\n",
        );
        let Ok(Event::Progress {
            stream_id: 1,
            elapsed,
            duration,
        }) = events.try_recv()
        else {
            panic!("expected a Progress event");
        };
        assert!(
            (elapsed.as_secs_f64() - 10.0).abs() < 0.01,
            "elapsed was {elapsed:?}"
        );
        assert!(
            (duration.as_secs_f64() - 180.0).abs() < 0.01,
            "duration was {duration:?}"
        );

        // A seek backwards past the anchor, and a stream with no known end,
        // must not produce a ~27-hour reading from a wrapped subtraction.
        session.set_parameter(Some("text/parameters"), b"progress: 5000/1000/5000");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Progress {
                stream_id: 1,
                elapsed: Duration::ZERO,
                duration: Duration::ZERO,
            })
        );

        // Malformed values are ignored, and the volume line in the same body
        // still works.
        for bad in [
            "progress: 1/2\r\n",
            "progress: 1/2/3/4\r\n",
            "progress: a/b/c\r\n",
            "progress:\r\n",
        ] {
            session.set_parameter(Some("text/parameters"), bad.as_bytes());
        }
        assert!(events.try_recv().is_err(), "no event from bad progress");

        session.set_parameter(
            Some("text/parameters"),
            b"volume: -8.0\r\nprogress: 1000/45100/7939000\r\n",
        );
        assert_eq!(
            events.try_recv(),
            Ok(Event::Volume {
                stream_id: 1,
                revision: 1,
                db: -8.0,
            })
        );
        assert!(matches!(events.try_recv(), Ok(Event::Progress { .. })));
    }

    #[tokio::test]
    async fn progress_outside_a_session_is_dropped() {
        // A position without a stream means nothing, and before ANNOUNCE
        // there is no sample rate to convert it with.
        let (mut session, mut events) = session();
        session.set_parameter(Some("text/parameters"), b"progress: 1000/442000/7939000");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn volume_outside_a_stream_or_nonfinite_is_dropped() {
        let (mut session, mut events) = session();
        session.set_parameter(
            Some("text/parameters"),
            b"volume: -12.0\r\nvolume: NaN\r\nvolume: inf\r\n",
        );
        assert!(events.try_recv().is_err());

        start_session(&mut session, &mut events).await;
        session.set_parameter(Some("text/parameters"), b"volume: NaN\r\nvolume: -inf\r\n");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn announce_accepts_only_the_production_raop_alac_format() {
        let cases = [
            ("a=rtpmap:96 AppleLossless", "a=rtpmap:97 AppleLossless"),
            ("a=fmtp:96 352", "a=fmtp:97 352"),
            ("a=fmtp:96 352 0 16", "a=fmtp:96 64 0 16"),
            ("a=fmtp:96 352 0 16", "a=fmtp:96 4097 0 16"),
            ("a=fmtp:96 352 0 16", "a=fmtp:96 352 0 24"),
            ("40 10 14 2 255", "40 10 14 1 255"),
            ("0 0 44100", "0 0 48000"),
        ];
        for (from, to) in cases {
            let body = SDP_BODY.replace(from, to);
            let announce = request(&format!(
                "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
                 CSeq: 2\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ))
            .await;
            let (mut session, _events) = session();
            assert_eq!(
                session.handle_announce(&announce).status(),
                400,
                "unsupported SDP mutation was accepted: {to}"
            );
        }

        let body = SDP_BODY.replace("a=fmtp:96 352", "a=fmtp:96 4096");
        let announce = request(&format!(
            "ANNOUNCE rtsp://192.168.1.10/1 RTSP/1.0\r\n\
             CSeq: 2\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let (mut session, _events) = session();
        assert_eq!(session.handle_announce(&announce).status(), 200);
    }

    #[tokio::test]
    async fn session_started_carries_the_sender_address() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await; // asserts peer
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn metadata_before_setup_is_latched_until_session_start() {
        // Senders may push metadata during the handshake, but the host's
        // contract is that it only arrives inside a session — so the latest
        // of each is held and replayed after SessionStarted.
        let (mut session, mut events) = session();
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("First"));
        session.set_parameter(Some(DMAP_CONTENT_TYPE), &dmap_track("Second"));
        session.set_parameter(Some("image/jpeg"), b"JPEG");
        assert!(events.try_recv().is_err(), "nothing before SessionStarted");

        start_session(&mut session, &mut events).await;

        // Latest metadata wins, and each is replayed exactly once.
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Metadata { title: Some(t), .. }) if t == "Second"
        ));
        assert!(matches!(events.try_recv(), Ok(Event::Artwork { .. })));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_metadata_is_ignored_and_never_ends_the_session() {
        let (mut session, mut events) = session();
        start_session(&mut session, &mut events).await;

        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"");
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"garbage, not dmap");
        // An mlit whose declared length runs off the end of the payload.
        session.set_parameter(Some(DMAP_CONTENT_TYPE), b"mlit\x00\x00\xff\xff");
        assert!(events.try_recv().is_err(), "no event from bad payloads");

        // The session is still live and still handling parameters.
        session.set_parameter(Some("text/parameters"), b"volume: -6.0\r\n");
        assert_eq!(
            events.try_recv(),
            Ok(Event::Volume {
                stream_id: 1,
                revision: 1,
                db: -6.0,
            })
        );
    }

    #[test]
    fn extracts_transport_ports() {
        let hdr = "RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002";
        assert_eq!(transport_param(hdr, "control_port"), Some(6001));
        assert_eq!(transport_param(hdr, "timing_port"), Some(6002));
        assert_eq!(transport_param(hdr, "server_port"), None);
    }

    #[test]
    fn stream_key_requires_16_byte_iv() {
        // Valid base64 but only 8 bytes.
        let err = decrypt_stream_key("AAECAwQF", &STANDARD.encode([0u8; 8])).unwrap_err();
        assert!(err.contains("aesiv is 8 bytes"), "got: {err}");
    }

    #[test]
    fn sdp_base64_tolerates_missing_padding() {
        // Apple sends aesiv/rsaaeskey without '=' padding; both forms must
        // decode to the same 16 bytes. (This is the bug a real Mac hit.)
        let iv: [u8; 16] = std::array::from_fn(|i| i as u8);
        let padded = STANDARD.encode(iv); // "...=="
        let unpadded = padded.trim_end_matches('=');
        assert_eq!(unpadded.len(), 22, "16 bytes unpadded is 22 base64 chars");
        assert_eq!(sdp_base64(&padded).unwrap(), iv);
        assert_eq!(sdp_base64(unpadded).unwrap(), iv);
    }
}
