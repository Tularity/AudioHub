//! Control plane: TCP accept + first-frame dispatch (VerifyHello -> M3 verify
//! -> SecureChannel -> SessionMsg loop; PairInit -> pair_responder when
//! pairing mode is active), outbound connects, and session open/close flows.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::prelude::*;

use audiohub_core::sysaudio;
use audiohub_core::volume::{self, SetAction, VolumeState};
use audiohub_ipc::{
    Mode, OpenSessionParams, SessionInfo, KIND_MIC, KIND_SPK, SOURCE_HAL_SPEAKER, SOURCE_MIC,
    SOURCE_SYSAUDIO, SOURCE_TONE,
};
use audiohub_net::control::{read_frame, write_frame, ControlMsg, CONTROL_MAX_FRAME};
use audiohub_net::identity::{PairedPeer, PeerStore};
use audiohub_net::pairing::{
    pair_initiator, pair_responder, verify_initiator, verify_responder, was_self_connection,
    was_unpaired_by_peer, UnpairedByPeer,
};
use audiohub_net::secure::{SecureChannel, SessionMsg};

use crate::engine::{self, SourceSpec, TxCmd};
use crate::peer_transport::{DialPolicy, TransportTier};
use crate::{
    build_session_info, dlog, gen_media_salt, haldev, lk, rd, reconnect, wr, ClockFilter,
    ConnShared, DaemonInner, DaemonState, PeerLatCell, RxStream, SessionEntry, SessionOrigin,
    TxShared, VolumeCell, DIR_RECV, DIR_SEND, MEDIA_SALT_LEN,
};

pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Control writes share the per-conn channel mutex with the reader and with the
/// 1s ticker, so an unresponsive peer must not be able to block either: past
/// this the write fails and the connection is declared dead.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an OpenStream handler waits for the tx thread to actually build the
/// media source before it answers Accept/Reject.
const SOURCE_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Silence that declares a control channel dead (spec-m4c §C wants the drop
/// visible in ~5s). The ticker pings every second, so this is five missed
/// round trips on a channel where the peer answers Ping without doing any work.
pub(crate) const CONTROL_SILENCE_LIMIT: Duration = Duration::from_secs(5);
const MAX_PAIR_FAILURES: u32 = 5;
pub(crate) const MAX_PAIRING_TTL_S: u64 = 300;
const MAX_STREAMS_PER_CONN: usize = 16;
/// Kept under the measured point (~40 streams) where the 10ms mixer/tx loops
/// start missing their deadline, and far above real use (4 paths per peer).
const MAX_STREAMS_TOTAL: usize = 32;
/// Unauthenticated handshake threads allowed at once.
const MAX_PREAUTH_CONNS: usize = 32;
/// Two conns to the same peer created inside this window are treated as one
/// simultaneous connect and resolved by the frozen tie-break; a later one is an
/// ordinary reconnect and replaces whatever is there (self-heal on a dead path).
const SIMULTANEOUS_WINDOW: Duration = Duration::from_secs(10);
/// The frozen default control port, mirrored from the CLI's `DEFAULT_PORT`.
/// `peers.pair` accepts a bare host and has to complete it with something.
const DEFAULT_CONTROL_PORT: u16 = 47810;

// ---------------------------------------------------------------- server

/// Decrements the pre-auth counter however the handshake thread ends.
struct PreauthGuard(Arc<DaemonInner>);

impl Drop for PreauthGuard {
    fn drop(&mut self) {
        self.0.preauth.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn accept_loop(inner: Arc<DaemonInner>, listener: TcpListener) {
    let mut over_warned = false;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, addr)) => {
                if inner.preauth.load(Ordering::SeqCst) >= MAX_PREAUTH_CONNS {
                    if !over_warned {
                        over_warned = true;
                        dlog!(
                            "[audiohubd] control accept: {MAX_PREAUTH_CONNS} handshakes already \
                             in flight, refusing {addr}"
                        );
                    }
                    drop(stream);
                    continue;
                }
                over_warned = false;
                inner.preauth.fetch_add(1, Ordering::SeqCst);
                let i = inner.clone();
                let spawned =
                    std::thread::Builder::new()
                        .name("ahb-conn".into())
                        .spawn(move || {
                            let guard = PreauthGuard(i.clone());
                            // spec §8: one connection thread may not take the
                            // daemon with it — catch, log, drop the connection
                            let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                handle_inbound(&i, stream, addr, guard)
                            }));
                            match r {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => dlog!("[audiohubd] control conn {addr}: {e:#}"),
                                Err(_) => {
                                    dlog!("[audiohubd] control conn {addr}: panicked, dropped")
                                }
                            }
                        });
                if spawned.is_err() {
                    inner.preauth.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                dlog!("[audiohubd] control accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn handle_inbound(
    inner: &Arc<DaemonInner>,
    mut stream: TcpStream,
    addr: SocketAddr,
    preauth: PreauthGuard,
) -> Result<()> {
    stream.set_nonblocking(false)?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    // M8 tier 2: a multiplexed connection announces itself by its first four
    // bytes, and none of the three openings can be confused for another — see
    // `peek_opening`.
    match peek_opening(&stream)? {
        Opening::Framed => return mux_inbound(inner, stream, addr, preauth, false),
        Opening::WebSocket => return mux_inbound(inner, stream, addr, preauth, true),
        Opening::Control => {}
    }

    // dispatch on a peeked copy: verify_responder / pair_responder each read
    // the first frame themselves
    match peek_first(&stream)? {
        ControlMsg::VerifyHello { .. } => {
            let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
            let mut peer = verify_responder(&mut stream, &inner.identity(), &store)?;
            // The peer just told us its computer name (spec-m5b §5.3). Both
            // verify paths refresh it, so a peer that renames its Mac is
            // renamed on ITS virtual devices here at the next connection —
            // whichever side dialled. The address is worth keeping too: it is
            // where a reconnect would have to look.
            peer.last_addr = Some(addr.ip().to_string());
            persist_peer(inner, peer.clone())?;
            let chan = SecureChannel::establish_responder(stream.into(), &inner.identity(), &peer)?;
            // we are the responder, so the peer is the initiator of this TCP
            let initiator_fp = chan.peer().fingerprint.clone();
            let Some(conn) = register_conn(inner, chan, addr.ip(), initiator_fp, None) else {
                return Ok(()); // lost the simultaneous-connect tie-break
            };
            drop(preauth); // verified: no longer an unauthenticated slot
            conn_reader(inner, &conn); // runs on this thread until close
            Ok(())
        }
        ControlMsg::PairInit { .. } => {
            let pin = claim_pairing_pin(inner);
            let Some(pin) = pin else {
                let _ = write_frame(
                    &mut stream,
                    &ControlMsg::Error {
                        message: "pairing not enabled".into(),
                    },
                );
                bail!("pairing attempt while pairing not enabled");
            };
            let outcome = pair_responder(&mut stream, &pin, &inner.identity());
            release_pairing_pin(inner, &pin, outcome.is_ok());
            let mut outcome = outcome?;
            outcome.peer.last_addr = Some(addr.ip().to_string());
            // `PairInit.listen_port` is an ADVERTISEMENT, and the standalone
            // CLI has no listener at all, so it advertises the frozen default
            // (`m3.rs` cmd_pair). Paired over loopback to a daemon that owns
            // that very port, the record produced by believing it points the
            // peer at OUR socket — measured on 2026-07-31, and the reason the
            // session coordinator dialled this daemon itself. Nobody else can
            // be listening where we are listening, so this advertisement is
            // provably false: record 0 = "no port we can believe". The peer
            // stays fully usable — it dials us, and the media port is learned
            // from its keepalives — it just never gets dialled on a lie.
            let advertised = SocketAddr::new(addr.ip(), outcome.peer.port);
            if is_self_endpoint(inner, advertised) {
                dlog!(
                    "[audiohubd] {} advertised {advertised}, which is our own control endpoint; \
                     recording no port for it",
                    outcome.peer.fingerprint
                );
                outcome.peer.port = 0;
            }
            persist_peer(inner, outcome.peer)?; // persist before final Ok (M3 rule)
            write_frame(&mut stream, &ControlMsg::Ok {})?;
            Ok(())
        }
        // M8 tier 1: a second TCP connection for this peer's media. It carries
        // no control stream, so it never becomes a `ConnShared` of its own —
        // the ticket says which existing one it belongs to.
        //
        // The frame is consumed here rather than peeked-and-redone because,
        // unlike the two handshakes above, nothing downstream needs to re-read
        // it: `tcpmedia::accept` takes the socket already positioned at the
        // first media frame.
        ControlMsg::MediaAttach { .. } => {
            let ticket_b64 = match read_frame(&mut stream)? {
                ControlMsg::MediaAttach { ticket_b64 } => ticket_b64,
                other => bail!("media_attach vanished between peek and read: {other:?}"),
            };
            // Claim first, THEN release the preauth slot, THEN serve. Serving
            // runs for the life of the link, so releasing after it would hold
            // an unauthenticated-handshake slot for hours — the bound exists to
            // cap concurrent *handshakes*, and this socket stopped being one
            // the moment the ticket was spent.
            let (conn, claim) = crate::tcpmedia::claim(inner, &mut stream, &ticket_b64)?;
            drop(preauth);
            crate::tcpmedia::serve(inner, &conn, stream, claim)
        }
        other => {
            let _ = write_frame(
                &mut stream,
                &ControlMsg::Error {
                    message: "expected verify_hello or pair_init".into(),
                },
            );
            bail!("unexpected first frame: {other:?}");
        }
    }
}

/// Takes the active PIN and marks pairing busy, so only one PairInit can be in
/// flight: an attacker cannot run parallel guesses against the same window.
fn claim_pairing_pin(inner: &DaemonInner) -> Option<String> {
    let mut st = lk(&inner.state);
    let mut expired = false;
    let pin = match st.pairing.as_mut() {
        Some(p) if Instant::now() >= p.until => {
            expired = true;
            None
        }
        Some(p) if p.in_flight => None,
        Some(p) => {
            p.in_flight = true;
            Some(p.pin.clone())
        }
        None => None,
    };
    if expired {
        st.pairing = None;
    }
    pin
}

/// Consumes the PIN on success (single use) and disables pairing after
/// `MAX_PAIR_FAILURES` wrong attempts.
fn release_pairing_pin(inner: &DaemonInner, pin: &str, ok: bool) {
    let mut st = lk(&inner.state);
    let mut disable = false;
    if let Some(p) = st.pairing.as_mut() {
        p.in_flight = false;
        if p.pin == pin {
            if ok {
                disable = true;
            } else {
                p.fails += 1;
                if p.fails >= MAX_PAIR_FAILURES {
                    dlog!("[audiohubd] pairing disabled after {MAX_PAIR_FAILURES} failed attempts");
                    disable = true;
                }
            }
        }
    }
    if disable {
        st.pairing = None;
    }
}

/// What kind of connection is this, judged by its first four bytes?
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Opening {
    /// Tier 0/1 control: `u32 little-endian length ‖ JSON`.
    Control,
    /// Tier 2 over bare TCP: a 40-byte packet header.
    Framed,
    /// Tier 2 inside a WebSocket: an HTTP upgrade request (P6).
    WebSocket,
}

/// # Why the three can never be confused
///
/// A control connection opens with a little-endian length that
/// [`CONTROL_MAX_FRAME`] caps at 65536. The other two open with values that are
/// nowhere near it when read as one:
///
/// | opening | first four bytes | as a LE u32 |
/// |---|---|---|
/// | mux frame | `b"AUHB"` | 1_112_036_673 |
/// | upgrade   | `b"GET "` | 542_393_671 |
///
/// Seventeen thousand and eight thousand times the cap respectively, so the
/// discriminator is arithmetic rather than heuristic: **no legal control frame
/// can ever take either value.** A build without a branch refuses that kind of
/// connection loudly rather than misreading one — verified by injection
/// (2026-08-08 for the `MAGIC` branch, 2026-08-09 for `GET `): the peer reports
/// `first control frame too large: …` and no pair forms.
///
/// `GET` specifically, and no other method: RFC 6455 §4.1 allows the WebSocket
/// upgrade on nothing else, so widening this would only widen what a stranger
/// can steer into the upgrade path.
///
/// The check is a `peek`, so the bytes stay in the socket for whichever path
/// takes over.
fn peek_opening(stream: &TcpStream) -> Result<Opening> {
    let mut buf = [0u8; 4];
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the first four bytes");
        }
        match stream.peek(&mut buf) {
            Ok(0) => bail!("peer closed before the first frame"),
            Ok(n) if n >= 4 => {
                return Ok(if buf == audiohub_net::packet::MAGIC {
                    Opening::Framed
                } else if &buf == b"GET " {
                    Opening::WebSocket
                } else {
                    Opening::Control
                })
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e).context("peek the first four bytes"),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The tier 2 accept path: bring the mux up, then run the ordinary handshake
/// **inside** it.
///
/// Only `VerifyHello` is accepted as a first message here, and pairing is
/// deliberately not offered. Pairing is a one-shot exchange that finishes
/// before any media exists, so a tunnel forwards it unchanged on a connection
/// of its own (design §9 item 2 froze the flow as unchanged); carrying it here
/// too would be a second code path serving no case the first does not.
fn mux_inbound(
    inner: &Arc<DaemonInner>,
    stream: TcpStream,
    addr: SocketAddr,
    preauth: PreauthGuard,
    websocket: bool,
) -> Result<()> {
    let (link, io) = if websocket {
        crate::mux::accept_ws(inner, stream)?
    } else {
        crate::mux::accept(inner, stream)?
    };
    // Every early return from here on has to take the mux with it: its reader
    // and writer threads are already running, and a `ConnShared` that never
    // gets built is not going to tear them down later.
    let out = mux_handshake(inner, &link, io, addr, preauth);
    if out.is_err() {
        link.kill();
    }
    out
}

fn mux_handshake(
    inner: &Arc<DaemonInner>,
    link: &Arc<crate::mux::MuxLink>,
    mut io: crate::mux::ControlTransport,
    addr: SocketAddr,
    preauth: PreauthGuard,
) -> Result<()> {
    let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
    let mut peer = verify_responder(&mut io, &inner.identity(), &store)?;
    // **The address is the tunnel's, and it is recorded anyway.** On tier 2
    // every peer arrives from the same place — `127.0.0.1` in the local
    // forwarder, the tunnel's exit anywhere else — so this field stops being an
    // identity and becomes a hint about where a reconnect might look. Identity
    // is the fingerprint `verify_responder` just proved, which is precisely
    // what plan §4 settled: "身份基于指纹不基于源地址".
    peer.last_addr = Some(addr.ip().to_string());
    persist_peer(inner, peer.clone())?;
    let chan = SecureChannel::establish_responder(io, &inner.identity(), &peer)?;
    let initiator_fp = chan.peer().fingerprint.clone();
    let Some(conn) = register_conn(inner, chan, addr.ip(), initiator_fp, Some(link.clone())) else {
        link.kill(); // lost the simultaneous-connect tie-break
        return Ok(());
    };
    drop(preauth); // verified: no longer an unauthenticated slot
    conn_reader(inner, &conn); // runs on this thread until close
    link.kill();
    Ok(())
}

fn peek_first(stream: &TcpStream) -> Result<ControlMsg> {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for first control frame");
        }
        let n = match stream.peek(&mut buf) {
            Ok(0) => bail!("peer closed before first frame"),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e).context("peek first control frame"),
        };
        if n >= 4 {
            let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
            if len > CONTROL_MAX_FRAME {
                bail!("first control frame too large: {len} bytes");
            }
            if len + 4 > buf.len() {
                bail!("first control frame exceeds peek window: {len} bytes");
            }
            if n >= 4 + len {
                return serde_json::from_slice(&buf[4..4 + len])
                    .context("parse first control frame");
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn persist_peer(inner: &DaemonInner, peer: PairedPeer) -> Result<()> {
    let _g = lk(&inner.store_lock);
    let mut s = PeerStore::load_at(Some(&inner.cfg_dir))?;
    s.upsert(peer);
    s.save()
}

// ---------------------------------------------------------------- conn loop

/// Registers the connection, or returns `None` when an equally fresh conn to the
/// same peer already won the tie-break. Both peers compare the same key (the
/// initiator's fingerprint), so a simultaneous bidirectional connect converges
/// on one TCP connection; the loser drops its OWN conn and never evicts the
/// peer's, which is what used to let both sides tear down both connections.
fn register_conn(
    inner: &Arc<DaemonInner>,
    chan: crate::mux::ControlChan,
    peer_ip: IpAddr,
    initiator_fp: String,
    // Set only on tier 2: the mux the handshake we just completed travelled on.
    // Passed in rather than discovered because the media path has to be right
    // **before this function returns** — `conn_reader` starts immediately after
    // and a stream opened on the wrong transport keeps it for life (design
    // §5.1). Tier 1 needs a whole latch and an eight-second wait to reach the
    // same state; tier 2 gets it for free, because the connection that carries
    // the media is the one that carried the handshake.
    mux: Option<Arc<crate::mux::MuxLink>>,
) -> Option<Arc<ConnShared>> {
    let peer = chan.peer().clone();
    let mk = chan.media_keys();
    let conn = Arc::new(ConnShared {
        connection_id: inner.next_connection_id.fetch_add(1, Ordering::Relaxed),
        fp: peer.fingerprint.clone(),
        peer_ip,
        // Tier 0 is the default assumption and costs nothing to assume: the
        // control handshake proved TCP works, and only UDP is still unknown
        // (design §5.1). A tier 1 link, if any, replaces this after the fact.
        media_path: Mutex::new(match &mux {
            Some(l) => {
                // The queue was built before the handshake that named the peer
                // — this is the moment the name exists. Without it every tier 2
                // link shares the empty-string key in the ticker's per-link
                // maps, and two degraded peers report each other's backlog.
                l.media().name_peer(&peer.fingerprint);
                crate::tcpmedia::MediaPath::Framed(l.clone())
            }
            None => crate::tcpmedia::MediaPath::Udp(SocketAddr::new(peer_ip, peer.port)),
        }),
        media_gate: crate::tcpmedia::AttachGate::new(),
        media_attaching: AtomicBool::new(false),
        registration_ready: AtomicBool::new(false),
        deferred: Mutex::new(VecDeque::new()),
        tx_key: mk.tx,
        rx_key: mk.rx,
        peer,
        chan: Mutex::new(chan),
        initiator_fp,
        created: Instant::now(),
        pending: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        last_rx_ms: AtomicU64::new(0), // measured from `created`
        clock: Mutex::new(ClockFilter::new()),
        clock_warned: AtomicBool::new(false),
        peer_mode: Mutex::new(crate::PeerModeCell::Unheard),
        peer_audio_capabilities: Mutex::new(crate::PeerAudioCapabilitiesCell::Unheard),
        peer_native_output: Mutex::new(None),
        peer_spatial_output: Mutex::new(None),
    });
    let mut st = lk(&inner.state);
    let keep_existing = st.conns.get(&conn.fp).map_or(false, |old| {
        old.alive.load(Ordering::SeqCst)
            && old.created.elapsed() < SIMULTANEOUS_WINDOW
            && old.initiator_fp <= conn.initiator_fp
    });
    if keep_existing {
        conn.alive.store(false, Ordering::SeqCst);
        dlog!(
            "[audiohubd] dropping our duplicate conn to {} (kept the one initiated by the lower \
             fingerprint)",
            conn.fp
        );
        // On tier 2 the connection we are dropping IS the mux, and its two
        // threads are already running. Nothing else refers to it once this
        // `ConnShared` is discarded, so without this they read and park on a
        // socket forever — one leaked pair per simultaneous connect.
        if let Some(l) = &mux {
            l.kill();
        }
        return None;
    }
    let old = st.conns.insert(conn.fp.clone(), conn.clone());
    drop(st);
    if let Some(o) = old {
        o.alive.store(false, Ordering::SeqCst);
    }
    // plan §13 推论 1, and the single place it is wired: every control channel
    // is born here, whichever side dialled, so putting the advertisement
    // anywhere else would cover one direction and quietly miss the other.
    //
    // Sent before the reader starts, so it is the first thing on the channel:
    // the peer's list is right from its first paint rather than after a
    // round-trip. A failure is not fatal — `send_msg` has already marked the
    // conn dead, and the reader will notice on its next pass.
    let _ = conn.send_msg(&SessionMsg::ModeState {
        mode: haldev::effective_mode(inner).as_str().to_string(),
    });
    let _ = conn.send_msg(&local_audio_capabilities_msg());
    let observation = lk(&inner.native_output).clone();
    let _ = conn.send_msg(&SessionMsg::NativeOutputCapabilities { observation: observation.clone() });
    let _ = conn.send_msg(&SessionMsg::SpatialOutputCapabilities {
        offer: crate::spatial::offer_for(&observation),
    });
    // M8: if this peer is pinned to tier 1, start the media link now — before
    // any stream exists, so the first stream opens straight onto it. A stream
    // that opened first would be pinned to UDP for its whole life (design §5.1
    // rules out switching transports inside a live stream).
    //
    // Skipped outright on tier 2: the media path is already `Framed`, and
    // negotiating a second TCP connection for a peer we reach only through a
    // one-connection tunnel is the one thing tier 2 exists because we cannot
    // do. The guard is on the mux and not on the stored tier so that a tier
    // that changed under us cannot turn this into an eight-second wait for a
    // link that will never come.
    if mux.is_none() {
        crate::tcpmedia::negotiate(inner, &conn);
    }
    // Published-before-negotiate is required so an incoming media ticket can
    // resolve this connection. Opening a stream during that interval is not:
    // it would freeze the provisional UDP path and unheard mono capability for
    // the session's whole lifetime. Release only after negotiation is final;
    // the reader then drains ModeState/AudioCapabilities parked in `deferred`.
    conn.registration_ready.store(true, Ordering::Release);
    Some(conn)
}

fn local_audio_capabilities_msg() -> SessionMsg {
    let presence = audiohub_core::audio::default_device_presence();
    audio_capabilities_msg(presence)
}

fn audio_capabilities_msg(presence: audiohub_core::audio::DefaultDevicePresence) -> SessionMsg {
    SessionMsg::AudioCapabilities {
        default_input: presence.input,
        default_output: presence.output,
        max_media_channels: 2,
        device_volume_version: 1,
        media_frame_tag_version: 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingLocalEndpoint {
    DefaultInput,
    DefaultOutput,
}

impl MissingLocalEndpoint {
    fn label(self) -> &'static str {
        match self {
            Self::DefaultInput => "default input",
            Self::DefaultOutput => "default output",
        }
    }

    fn epoch(self, inner: &DaemonInner) -> u64 {
        match self {
            Self::DefaultInput => inner.dev_in_epoch.load(Ordering::Acquire),
            Self::DefaultOutput => inner.dev_out_epoch.load(Ordering::Acquire),
        }
    }
}

/// The real local endpoint a valid peer open depends on, if any. `dir` is from
/// the opener's wire perspective here (the mirror of the stored local `dir`).
fn required_local_endpoint_for_remote_open(
    kind: &str,
    dir: &str,
    source: Option<&str>,
) -> Option<MissingLocalEndpoint> {
    match (kind, dir) {
        (KIND_SPK, DIR_SEND) => Some(MissingLocalEndpoint::DefaultOutput),
        (KIND_MIC, DIR_RECV) if source.is_none() || source == Some(SOURCE_MIC) => {
            Some(MissingLocalEndpoint::DefaultInput)
        }
        _ => None,
    }
}

/// Return the real local endpoint which this peer-originated session needs but
/// the machine no longer has.
///
/// `dir` is local: a peer's `spk/send` request is stored as `spk/recv`, while a
/// peer's `mic/recv` request is stored as `mic/send`. Only the latter needs its
/// source inspected because an alternate mic source replaces the real default
/// input entirely.
fn missing_local_endpoint_for_peer_session(
    origin: SessionOrigin,
    kind: &str,
    dir: &str,
    source: Option<&str>,
    presence: audiohub_core::audio::DefaultDevicePresence,
) -> Option<MissingLocalEndpoint> {
    if origin != SessionOrigin::Peer {
        return None;
    }
    match (kind, dir) {
        (KIND_SPK, DIR_RECV) if !presence.output => Some(MissingLocalEndpoint::DefaultOutput),
        (KIND_MIC, DIR_SEND)
            if !presence.input && (source.is_none() || source == Some(SOURCE_MIC)) =>
        {
            Some(MissingLocalEndpoint::DefaultInput)
        }
        _ => None,
    }
}

/// Close peer-opened sessions whose owned real endpoint just disappeared.
///
/// The state lock is used only to take the immutable decision snapshot. It is
/// released before `teardown_stream` removes entries and, critically, before
/// `send_msg(CloseStream)` takes the connection's write mutex. This function
/// therefore remains safe if capability observation and a control reader are
/// active at the same time.
pub(crate) fn close_peer_sessions_missing_local_endpoints(
    inner: &Arc<DaemonInner>,
    presence: audiohub_core::audio::DefaultDevicePresence,
) {
    let doomed: Vec<(u32, MissingLocalEndpoint)> = {
        let st = lk(&inner.state);
        st.sessions
            .values()
            .filter_map(|entry| {
                missing_local_endpoint_for_peer_session(
                    entry.origin,
                    &entry.kind,
                    &entry.dir,
                    entry.source.as_deref(),
                    presence,
                )
                .map(|endpoint| (entry.id, endpoint))
            })
            .collect()
    };

    for (stream_id, endpoint) in doomed {
        dlog!(
            "[audiohubd] stream {stream_id}: local {} disappeared; closing peer-originated stream",
            endpoint.label()
        );
        teardown_stream(inner, stream_id, true);
    }
}

/// Commit a peer open only if the endpoint generation sampled before media
/// construction is still current. Media construction can wait seconds for a
/// real capture device, so the watcher may already have advertised capability
/// loss while this open is not yet visible in `state.sessions`.
///
/// The epoch check and insertion share the state lock with the watcher's
/// session snapshot. Therefore either the watcher runs after the insertion and
/// sees it, or it ran first and this check rejects the stale open. A callback
/// racing after the check is also safe: its watcher pass necessarily takes the
/// state lock after this insertion.
fn insert_remote_session_if_endpoint_unchanged(
    inner: &DaemonInner,
    entry: SessionEntry,
    endpoint_guard: Option<(MissingLocalEndpoint, u64)>,
    reservation: &StreamReservation<'_>,
) -> Result<()> {
    let mut st = lk(&inner.state);
    if let Some((endpoint, started_epoch)) = endpoint_guard {
        endpoint_epoch_still_current_at_commit(endpoint, started_epoch, endpoint.epoch(inner))?;
    }
    reservation.publish(&mut st, entry)
}

fn endpoint_epoch_still_current_at_commit(
    endpoint: MissingLocalEndpoint,
    started_epoch: u64,
    current_epoch: u64,
) -> Result<()> {
    if current_epoch != started_epoch {
        bail!(
            "local {} changed while the peer stream was opening; retry the stream",
            endpoint.label()
        );
    }
    Ok(())
}

/// Re-advertise the current real endpoint facts after a platform device epoch
/// changes. Discovery happens once per fan-out and sends happen after the state
/// lock is released, so a slow peer cannot block connection bookkeeping.
pub(crate) fn announce_audio_capabilities(inner: &Arc<DaemonInner>) {
    let presence = audiohub_core::audio::default_device_presence();
    close_peer_sessions_missing_local_endpoints(inner, presence);
    let msg = audio_capabilities_msg(presence);
    let conns: Vec<_> = lk(&inner.state)
        .conns
        .values()
        .filter(|c| c.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    for conn in conns {
        let _ = conn.send_msg(&msg);
    }
}

/// Tell every live peer what mode we are in now, and stop whatever the new mode
/// no longer permits (plan §13 推论 2).
///
/// Called on every mode change. The transition is "disconnect and tell", not
/// "refuse to switch": plan §7.1 freezes mode switching as needing no
/// confirmation, and a switch that could be vetoed by somebody else's session
/// would be a confirmation dialog worn by a peer.
///
/// The two halves are symmetric and both are necessary:
///   - leaving share ⇒ close what PEERS opened on us; they lose their devices,
///     which is the announced consequence, and `teardown_stream(notify=true)`
///     sends each one a `CloseStream` so nothing is left dangling.
///   - entering share ⇒ close what WE opened on them; a share-mode machine does
///     not consume. Without this half, sessions opened in mode A keep running
///     after the switch and the machine is a provider AND a consumer — exactly
///     the state §13 exists to make unreachable, reached by the very control
///     that was supposed to leave it.
///
/// The control channels stay up. Dropping them would take the other direction
/// with them, trigger the reconnect backoff, and — worst — remove the channel
/// the explanation travels on.
pub(crate) fn announce_mode(inner: &Arc<DaemonInner>, mode: Mode) {
    let conns: Vec<Arc<ConnShared>> = lk(&inner.state)
        .conns
        .values()
        .filter(|c| c.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();

    let doomed: Vec<u32> = {
        let st = lk(&inner.state);
        st.sessions
            .values()
            // A session survives only if the new mode would still permit the
            // machinery that opened it. `mode_permits_origin` is the mirror of
            // the two `refuse_*` open gates and lives beside them so the pair
            // cannot drift; see its doc comment for why asking about the SIDE
            // alone (the shape this replaced) let every A→B switch strand its
            // mode-A sender.
            .filter(|e| !haldev::mode_permits_origin(mode, e.origin))
            .map(|e| e.id)
            .collect()
    };
    for id in &doomed {
        teardown_stream(inner, *id, true);
    }
    if !doomed.is_empty() {
        dlog!(
            "[audiohubd] mode is now {mode}: closed {} session(s) the new mode does not permit",
            doomed.len()
        );
    }

    // After the teardown, so a peer that reads both in order sees the closes
    // explained rather than announced in advance and then contradicted.
    for c in conns {
        let _ = c.send_msg(&SessionMsg::ModeState {
            mode: mode.as_str().to_string(),
        });
    }
}

/// Reads this connection until it closes, then ALWAYS tears it down. The
/// catch_unwind is not about surviving a panic — the spawn sites already do
/// that — it is about where the unwind stops: a panic inside a message handler
/// used to fly past `teardown_conn`, leaving the conn registered and "alive"
/// with its sessions and rx_table entries intact, its TxCmd::Remove never sent
/// (the tx engine kept blasting media at a dead peer) and connect_peer handing
/// the zombie back as an online peer.
pub(crate) fn conn_reader(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    let contract_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| loop {
        if inner.shutdown.load(Ordering::SeqCst) || !conn.alive.load(Ordering::SeqCst) {
            break;
        }
        if peer_contract_deadline_expired(
            conn.registration_ready.load(Ordering::Acquire),
            *lk(&conn.peer_mode),
            *lk(&conn.peer_audio_capabilities),
            Instant::now(),
            contract_deadline,
        ) {
            dlog!(
                "[audiohubd] peer {} never completed its mode/audio capability contract within \
                 {HANDSHAKE_TIMEOUT:?}; closing the half-registered connection",
                conn.fp
            );
            break;
        }
        // Anything `register_conn` pulled off the channel while it was bringing
        // a tier 1 media link up comes first, so channel order is preserved:
        // those messages arrived before whatever is still in the socket.
        let parked = lk(&conn.deferred).pop_front();
        // short recv slices so senders can interleave on the chan mutex
        let res = match parked {
            Some(m) => Ok(Some(m)),
            None => {
                let mut ch = lk(&conn.chan);
                ch.recv_timeout(Duration::from_millis(50))
            }
        };
        match res {
            Ok(Some(msg)) => {
                conn.note_rx(); // a complete frame is the only proof of life
                if handle_msg(inner, conn, msg) {
                    break;
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                if conn.alive.load(Ordering::SeqCst) && !inner.shutdown.load(Ordering::SeqCst) {
                    dlog!("[audiohubd] control channel {}: {e:#}", conn.fp);
                }
                break;
            }
        }
    }));
    if r.is_err() {
        dlog!(
            "[audiohubd] control conn {}: reader panicked, tearing down",
            conn.fp
        );
    }
    teardown_conn(inner, conn);
}

/// Per-stream control messages are only meaningful from the connection that
/// owns the stream: stream ids travel in cleartext inside media headers, so any
/// other paired peer could otherwise close or corrupt someone else's stream.
fn owned_session(
    inner: &DaemonInner,
    conn: &Arc<ConnShared>,
    stream_id: u32,
    what: &str,
) -> Option<SessionEntry> {
    let st = lk(&inner.state);
    match st.sessions.get(&stream_id) {
        Some(e) if Arc::ptr_eq(&e.conn, conn) => Some(e.clone()),
        Some(e) => {
            let owner = e.conn.fp.clone();
            drop(st);
            dlog!(
                "[audiohubd] ignoring {what} for stream {stream_id} from {}: the stream belongs \
                 to {owner}",
                conn.fp
            );
            None
        }
        None => None,
    }
}

/// A verified connection can be replaced while its reader is between the
/// alive check and message dispatch. Peer-level controls have no stream id to
/// provide ownership, so they must explicitly prove they are still the one
/// registered connection for this fingerprint.
fn current_peer_connection(inner: &DaemonInner, conn: &Arc<ConnShared>) -> bool {
    conn.alive.load(Ordering::SeqCst)
        && lk(&inner.state)
            .conns
            .get(&conn.fp)
            .is_some_and(|current| Arc::ptr_eq(current, conn))
}

/// true = close this connection.
fn handle_msg(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>, msg: SessionMsg) -> bool {
    match msg {
        SessionMsg::OpenSpatialStream { stream_id, media_salt_b64, contract, rx_latency } => {
            let reply = match handle_remote_spatial_open(
                inner, conn, stream_id, &media_salt_b64, contract, rx_latency.as_deref(),
            ) {
                Ok(()) => SessionMsg::AcceptStream { stream_id },
                Err(error) => SessionMsg::RejectStream { stream_id, reason: format!("{error:#}") },
            };
            let _ = conn.send_msg(&reply);
        }
        SessionMsg::SpatialOutputCapabilities { offer } => {
            if current_peer_connection(inner, conn) {
                let current = {
                    let mut state = lk(&conn.peer_spatial_output);
                    crate::spatial::accept_offer(&mut state, offer);
                    state.clone()
                };
                if let Some(current) = current {
                    crate::spatial::invalidate_peer_transmits(inner, conn.connection_id, &current);
                }
            }
        }
        SessionMsg::OpenStream {
            stream_id,
            kind,
            dir,
            sample_rate,
            channels,
            media_salt_b64,
            verify_freq,
            source,
            freq,
            backend,
            simulate_loss_pct,
            volume_sync,
            rx_latency,
            tx_quality,
            ..
        } => {
            let reply = match handle_remote_open(
                inner,
                conn,
                stream_id,
                &kind,
                &dir,
                sample_rate,
                channels,
                &media_salt_b64,
                verify_freq,
                source.as_deref(),
                freq,
                backend.as_deref(),
                simulate_loss_pct,
                volume_sync,
                rx_latency.as_deref(),
                tx_quality.as_deref(),
            ) {
                Ok(()) => SessionMsg::AcceptStream { stream_id },
                Err(e) => SessionMsg::RejectStream {
                    stream_id,
                    reason: format!("{e:#}"),
                },
            };
            let _ = conn.send_msg(&reply);
        }
        // M8 tier 1 attachment. Both arms are cheap and non-blocking: minting a
        // ticket is 32 bytes of randomness, and dialling happens on a thread of
        // its own. Doing either inline would put a connect() on the reader
        // thread, which also runs device I/O for every other message here.
        SessionMsg::MediaAttachRequest {} => crate::tcpmedia::on_request(inner, conn),
        SessionMsg::MediaAttachTicket { ticket_b64 } => {
            crate::tcpmedia::on_ticket(inner, conn, ticket_b64)
        }
        // Only reachable when the refusal lost the race with `negotiate`'s own
        // pump (it reads this message itself, because it is blocking on it).
        // Logged rather than ignored: a peer that refuses tier 1 is the reason
        // this connection stayed on UDP, and that reason should be findable.
        SessionMsg::MediaAttachRefused { reason } => dlog!(
            "[audiohubd] {} refused a tier 1 media attach: {reason}",
            conn.fp
        ),
        // The peer's half of the automatic downgrade. Handled inline like the
        // attach messages above and for the same reason it is safe to: it does
        // a store write and at most a `drop_conn`, no device I/O and no dial.
        SessionMsg::TransportSwitch { tier, reason } => {
            crate::autotier::on_peer_switch(inner, &conn.fp, &tier, &reason)
        }
        SessionMsg::AcceptStream { stream_id } => notify_pending(conn, stream_id, Ok(())),
        SessionMsg::RejectStream { stream_id, reason } => {
            notify_pending(conn, stream_id, Err(reason))
        }
        SessionMsg::CloseStream { stream_id } => {
            if owned_session(inner, conn, stream_id, "close_stream").is_some() {
                teardown_stream(inner, stream_id, false);
            }
        }
        SessionMsg::Stats {
            stream_id,
            received,
            lost,
            loss_pct,
            jitter_ms,
            spread_ms,
        } => {
            let tx = owned_session(inner, conn, stream_id, "stats").and_then(|e| e.tx);
            if let Some(t) = tx {
                let mut r = lk(&t.remote);
                r.seq += 1;
                // the receiver reports one interval at a time; totals are ours
                // to accumulate for the lifetime display
                r.received = r.received.saturating_add(received);
                r.lost = r.lost.saturating_add(lost);
                r.iv_loss_pct = loss_pct;
                r.iv_jitter_ms = jitter_ms;
                // Stored as it arrived, `None` included: overwriting a `None`
                // with the previous reading would make a peer that stopped
                // reporting look like a peer that is still fine.
                r.iv_spread_ms = spread_ms;
            }
        }
        // plan §15：对端（消费者）要求本机把**执行器在本机这一侧**的档位改掉。
        //
        // 三条断言，全部会计数、都不静默：
        //   A 流必须属于这条连接（`owned_session`）——`stream_id` 在 daemon 内
        //     是全局的，不查归属的话，共享模式下同时服务两台机器时，其中一台
        //     可以去调另一台那条流的 jitter buffer。
        //   B 执行器必须真的在本地这一侧（`rx_latency ⇒ 有 rx`）。不成立说明
        //     对端把交叉的那半边搞反了，而那种错误的自然表现恰恰是「什么都
        //     没发生」。
        //   C §13 互斥：只有共享模式的机器接受外来档位。判据取**本机**的
        //     `effective_mode`，不取对端自报的 `ModeState`——把通告当权威会把
        //     防中继的闸门放到线的错误一侧。
        SessionMsg::SetTransport {
            stream_id,
            rx_latency,
            tx_quality,
        } => {
            // 断言 C **先于**断言 A：§13 问的是「这台机器此刻允不允许被指挥」，
            // 与是哪一条流无关。放在归属校验之后的话，一台处于使用端模式的机器
            // 收到一个陌生 stream_id 时会先撞归属校验，于是那条互斥线**永远不会
            // 被执行到**——一道理论上不可达、实际上也不可达的闸门，等于没有。
            if let Some(why) = haldev::refuse_being_used(haldev::effective_mode(inner)) {
                dlog!(
                    "[audiohubd] 拒绝 {} 对流 {stream_id} 的 set_transport：{why}",
                    conn.fp
                );
                lk(&inner.servo_site).bad_target();
                return false;
            }
            let Some(e) = owned_session(inner, conn, stream_id, "set_transport") else {
                // 归属校验没过（或流已经不在了）。计数，否则这条丢失完全不可见。
                lk(&inner.servo_site).bad_target();
                return false;
            };
            let mut bad = false;
            if let Some(v) = &rx_latency {
                if e.rx.is_some() {
                    *lk(&e.pushed.rx_latency) = audiohub_ipc::LatencyTarget::parse(v);
                } else {
                    bad = true;
                }
            }
            if let Some(v) = &tx_quality {
                if e.tx.is_some() {
                    *lk(&e.pushed.tx_quality) = audiohub_ipc::QualityTarget::parse(v);
                } else {
                    bad = true;
                }
            }
            if bad {
                dlog!(
                    "[audiohubd] {} 对流 {stream_id} 下的档位在本机没有执行器\
                     （rx={} tx={}, rx_latency={rx_latency:?} tx_quality={tx_quality:?}）",
                    conn.fp,
                    e.rx.is_some(),
                    e.tx.is_some()
                );
                lk(&inner.servo_site).bad_target();
            } else {
                // 立刻灌进执行器，不等下一拍：`publish_targets` 每秒也会做一次，
                // 但那一秒里跑的是旧档位，而「修改即生效」是这条路冻结的形状。
                crate::publish_targets(inner, std::slice::from_ref(&e));
            }
        }
        SessionMsg::VolumeSet {
            stream_id,
            scalar,
            muted,
            src,
        } => apply_peer_volume(inner, conn, stream_id, scalar, muted, &src),
        SessionMsg::VolumeState {
            stream_id,
            scalar,
            muted,
            adjustable,
            mute_adjustable,
        } => {
            // consumer side: the provider told us what its device really reads.
            // The direction check is the mirror of apply_peer_volume's: on a
            // stream where WE are the provider this cell holds our OWN device's
            // reading, and a misbehaving peer must not be able to overwrite it
            // with a fabricated value the operator would then be shown.
            if let Some(e) = owned_session(inner, conn, stream_id, "volume_state") {
                let consumer = e.kind == KIND_SPK && e.dir == DIR_SEND;
                if !consumer {
                    dlog!(
                        "[audiohubd] ignoring volume_state for stream {stream_id}: this side owns \
                         the output device"
                    );
                } else if e.volume.enabled {
                    let state = VolumeState {
                        scalar: scalar.clamp(0.0, 1.0),
                        muted,
                        adjustable,
                        mute_adjustable: mute_adjustable.unwrap_or(adjustable),
                    };
                    // plan §7.2：对端设备**没有**可写音量时，这条流的音量改由本机
                    // 的发送侧软件增益兑现。判据只看对端这一句话，模式门单独加：
                    // §7.2 说的是「使用端**虚拟设备**自管音量」，而模式 A 没有虚拟
                    // 设备，它的音量故事是 §7.1 的「以对端为准」——两条一起跑就是
                    // 两个机制在动同一个响度（plan §12.5 的双重衰减）。
                    // Snapshot the connection cell before taking the HAL slot
                    // lock in `peer_software_gain_authority`. Keeping the
                    // temporary MutexGuard alive across `bool::then` would
                    // invert the established haldev -> state -> conn-cell
                    // order used by reconciliation and device-volume commits.
                    let peer_device_volume_v1 =
                        { lk(&conn.peer_audio_capabilities).device_volume_version() >= 1 };
                    let peer_level = (crate::mode_b_in_force(inner) && peer_device_volume_v1)
                        .then(|| haldev::peer_software_gain_authority(inner, &conn.fp))
                        .flatten();
                    let fallback = peer_level.map_or_else(
                        || {
                            volume::authority_for(Some(state)) == volume::VolumeAuthority::SendGain
                                && crate::mode_b_in_force(inner)
                        },
                        |gain| gain.is_some(),
                    );
                    if fallback {
                        // 接手（幂等；只有第一次真的播种起点）。之后**一个字都
                        // 不写进单元格**：对端每秒都会把它那个冻住的读数（聚合
                        // 设备上恒为 0）再送一次，照抄进来就会每秒把用户刚拖到的
                        // 位置拽回去 —— 而 `push_peer_volumes` 会把这个拽回去的
                        // 值一路推到虚拟设备的音量控件上。
                        engage_send_gain(&e);
                        if let Some(wanted) = peer_level.flatten() {
                            if let Err(err) = apply_send_gain(&e, wanted.scalar, Some(wanted.muted))
                            {
                                dlog!(
                                    "[audiohubd] stream {}: restore peer-level software gain: \
                                     {err:#}",
                                    e.id
                                );
                            }
                        }
                    } else {
                        // 交还（同样幂等）：对端换回能调音量的设备之后，增益斜坡
                        // 回到 1.0，显示值重新由对端的读数驱动。
                        release_send_gain(&e);
                        *lk(&e.volume.state) = Some(state);
                        // spec-round2 §B2 reverse direction rides on THIS cell:
                        // the ticker pushes genuine changes into the virtual
                        // speaker's control. Deliberately not sent from here — a
                        // mach send can sit for up to its 500ms timeout, and this
                        // is the thread that reads the peer's control channel.
                        //
                        // plan §7.1 模式 A 「与对端音量同步」 IS applied from here,
                        // and the distinction is not an inconsistency: this writes
                        // the OS volume property (the same call `apply_peer_volume`
                        // already makes on this very thread), not a mach message to
                        // the driver. Doing it on the 1s ticker instead would make
                        // 「以对端为准」 lag a second behind the peer's knob.
                        follow_peer_volume(inner, &e, state);
                    }
                }
            }
        }
        SessionMsg::DeviceVolumeSet {
            endpoint,
            request_id,
            scalar,
            muted,
        } => apply_peer_device_volume(inner, conn, &endpoint, request_id, scalar, muted),
        SessionMsg::DeviceVolumeState {
            endpoint,
            request_id,
            revision,
            scalar,
            muted,
            adjustable,
            mute_adjustable,
        } => {
            if !current_peer_connection(inner, conn) {
                dlog!(
                    "[audiohubd] ignoring device_volume_state from replaced connection {}",
                    conn.fp
                );
                return false;
            }
            let local_consumer = haldev::effective_mode(inner) == Mode::B;
            let peer_provider = lk(&conn.peer_mode).mode() == Some(Mode::Share);
            let protocol = lk(&conn.peer_audio_capabilities).device_volume_version() >= 1;
            let parsed = haldev::DeviceVolumeEndpoint::parse(&endpoint);
            if !local_consumer || !peer_provider || !protocol {
                dlog!(
                    "[audiohubd] ignoring device_volume_state from {}: local_mode={}, \
                     peer_mode={:?}, protocol={}",
                    conn.fp,
                    haldev::effective_mode(inner),
                    lk(&conn.peer_mode).mode(),
                    protocol
                );
            } else if !scalar.is_finite() {
                dlog!(
                    "[audiohubd] ignoring device_volume_state from {}: scalar is not finite",
                    conn.fp
                );
            } else if let Some(endpoint) = parsed {
                let _ = haldev::accept_peer_device_volume(
                    inner,
                    &conn.fp,
                    endpoint,
                    conn.connection_id,
                    revision,
                    request_id,
                    VolumeState {
                        scalar,
                        muted,
                        adjustable,
                        mute_adjustable,
                    },
                );
            } else {
                dlog!(
                    "[audiohubd] ignoring device_volume_state from {}: unknown endpoint {:?}",
                    conn.fp,
                    endpoint
                );
            }
        }
        SessionMsg::Ping { t_us } => {
            // `t_us` 原样回抄（对端时基，我们不解释它）；`peer_t_us` 是**我们**
            // 收到这条 Ping 的时刻，本机时基。两个时基在这条报文里并排放着，
            // 谁也不减谁——换算是发起方的事（`ClockFilter::note_pong`）。
            //
            // 同步应答，就在读取线程里：NTP 的 t3 因此约等于 t2，这正是只发
            // 一个时戳而不是两个的前提（见 `SessionMsg::Pong` 的文档）。
            let _ = conn.send_msg(&SessionMsg::Pong {
                t_us,
                peer_t_us: Some(inner.start.elapsed().as_micros() as u64),
            });
        }
        SessionMsg::Pong { t_us, peer_t_us } => {
            // P1：t1 = 我们发 Ping 的时刻（对端原样回抄），t4 = 现在。
            // **两者都取自 `inner.start`，同一个时基** —— RTT 因此是纯本机量，
            // 对端做什么都改变不了它的基准。t2（对端时基）只进 θ，而 θ 的定义
            // 就是「两个时基之差」。全daemon只有这一处允许两个时基相遇，并且
            // 只做差、不做商（做商就会混进两个基准的相对速率——上一轮 143 ppm
            // 系统误差底的来历）。
            let t4 = inner.start.elapsed().as_micros() as u64;
            let outcome = lk(&conn.clock).note_pong(t_us, t4, peer_t_us);
            match outcome {
                crate::PongOutcome::Ok => {}
                crate::PongOutcome::Stepped => dlog!(
                    "[audiohubd] peer {}: clock offset stepped (peer daemon restart or sleep?), \
                     restarting the min-RTT window",
                    conn.fp
                ),
                // 静默丢弃会让 confidence 永远挂在「测量中」而没人说得出为什么。
                // 一次就够：Pong 是对端发的，刷屏的可能性归它掌握。
                crate::PongOutcome::Implausible => {
                    if conn.first_clock_warning() {
                        dlog!(
                            "[audiohubd] peer {}: ignoring a Pong that echoes a timestamp we \
                             cannot have sent (t_us={t_us}, now={t4}); latency stays 'measuring'",
                            conn.fp
                        );
                    }
                }
            }
        }
        // P0b：对端把它那一侧的分项回传过来了。
        //
        // `owned_session` 是这里的安全边界：stream id 在媒体头里是明文，任何
        // 另一个已配对的对端都可以对它喊话。分项决定用户看到的那个延迟数字，
        // 不属于这条连接的流一律不收。
        SessionMsg::StageReport {
            stream_id,
            stages,
            local_ms,
            dev,
            quality,
            seq_us,
        } => {
            if let Some(e) = owned_session(inner, conn, stream_id, "stage_report") {
                let ipc: Vec<_> = stages.iter().map(crate::from_wire_stage).collect();
                // 落在**这一条流**的格子里（`SessionEntry::peer_lat`），不是一张
                // 按连接的表——见 `PeerLatCell` 上的 R8 说明。
                //
                // `quality` 与分项走同一条报文、同一个安全边界：音质是「对端
                // 那侧听到的东西」，同样只有这条连接自己的流才有资格更新它。
                if let Some(why) = e.peer_lat.accept(seq_us, ipc, local_ms, dev, quality) {
                    dlog!("[audiohubd] stream {stream_id} ({}): {why}", conn.fp);
                }
            }
        }
        SessionMsg::ModeState { mode } => {
            if !current_peer_connection(inner, conn) {
                dlog!(
                    "[audiohubd] ignoring mode state from replaced connection {}",
                    conn.fp
                );
                return false;
            }
            let cell = match Mode::parse(&mode) {
                Some(m) => crate::PeerModeCell::Known(m),
                None => {
                    // Unreachable from any build of ours (the protocol version
                    // is equality-checked at verify), so it is worth a line
                    // rather than a silent shrug — and the peer is treated as
                    // unusable, because "it named something we cannot read" is
                    // not a reason to offer it.
                    dlog!(
                        "[audiohubd] peer {} advertised a mode this build does not know; \
                         treating it as unusable",
                        conn.fp
                    );
                    crate::PeerModeCell::Unrecognised
                }
            };
            let prev = std::mem::replace(&mut *lk(&conn.peer_mode), cell);
            if prev != cell {
                dlog!("[audiohubd] peer {} is now in mode {mode}", conn.fp);
            }
            // A peer that stopped serving takes its own sessions down and tells
            // us with CloseStream; we do not tear anything down from here. The
            // asymmetry is deliberate: acting on a peer's claim would let a
            // misbehaving peer close streams it does not own, and everything
            // that actually has to stop is already stopped by the machine that
            // owns the devices.
        }
        SessionMsg::AudioCapabilities {
            default_input,
            default_output,
            max_media_channels,
            device_volume_version,
            media_frame_tag_version,
        } => {
            if !current_peer_connection(inner, conn) {
                dlog!(
                    "[audiohubd] ignoring audio capabilities from replaced connection {}",
                    conn.fp
                );
                return false;
            }
            let cell = crate::PeerAudioCapabilitiesCell::Known {
                default_input,
                default_output,
                max_media_channels,
                device_volume_version,
                media_frame_tag_version,
            };
            let prev = std::mem::replace(&mut *lk(&conn.peer_audio_capabilities), cell);
            if prev != cell {
                dlog!(
                    "[audiohubd] peer {} audio capabilities: default_input={}, default_output={}, \
                    max_media_channels={}, device_volume_version={}, media_frame_tag_version={}",
                    conn.fp,
                    default_input,
                    default_output,
                    max_media_channels,
                    device_volume_version,
                    media_frame_tag_version
                );
            }
            if device_volume_version < 1 {
                haldev::clear_peer_device_volume_protocol(inner, &conn.fp, conn.connection_id);
            }
        }
        SessionMsg::NativeOutputCapabilities { observation } => {
            if current_peer_connection(inner, conn) {
                crate::output_caps::accept_remote(&mut lk(&conn.peer_native_output), observation);
            }
        }
        SessionMsg::Unpaired {} => {
            // The peer removed us. Its virtual devices here are now a pair of
            // ghosts — permanently offline, permanently silent, redialling a
            // machine that has blacklisted us — so they go with the pairing.
            dlog!(
                "[audiohubd] peer {} unpaired from us; removing it here too",
                conn.fp
            );
            let fp = conn.fp.clone();
            let i = inner.clone();
            // Off-thread: forget_peer tears this very connection down, and
            // doing that from inside its own reader is a re-entrant teardown.
            let _ = std::thread::Builder::new()
                .name("ahb-unpair".into())
                .spawn(move || forget_peer(&i, &fp));
            return true;
        }
        SessionMsg::Bye {} => return true,
    }
    false
}

/// Apply a peer-level mode-B endpoint write on the provider.
///
/// This handler runs only after pair verification and secure-channel upgrade.
/// Local Share mode remains the authority gate; the remote mode/capability
/// checks keep honest mixed-version peers out of a path they cannot complete.
fn apply_peer_device_volume(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    endpoint: &str,
    request_id: u64,
    scalar: f32,
    muted: Option<bool>,
) {
    if !current_peer_connection(inner, conn) {
        dlog!(
            "[audiohubd] ignoring device_volume_set from replaced connection {}",
            conn.fp
        );
        return;
    }
    if haldev::effective_mode(inner) != Mode::Share {
        dlog!(
            "[audiohubd] ignoring device_volume_set from {}: this machine is not sharing",
            conn.fp
        );
        return;
    }
    if lk(&conn.peer_mode).mode() != Some(Mode::B)
        || lk(&conn.peer_audio_capabilities).device_volume_version() < 1
    {
        dlog!(
            "[audiohubd] ignoring device_volume_set from {}: peer is not an eligible mode-B \
             consumer",
            conn.fp
        );
        return;
    }
    if !scalar.is_finite() {
        dlog!(
            "[audiohubd] ignoring device_volume_set from {}: scalar is not finite",
            conn.fp
        );
        return;
    }
    let Some(endpoint) = haldev::DeviceVolumeEndpoint::parse(endpoint) else {
        dlog!(
            "[audiohubd] ignoring device_volume_set from {}: unknown endpoint",
            conn.fp
        );
        return;
    };
    let scalar = scalar.clamp(0.0, 1.0);
    // Keep the platform write and readback in one endpoint order, then stamp
    // the result. Sending happens after the lock: a slow peer must not block
    // another peer's device write, and the consumer rejects reordered revisions.
    let mut ordered = match endpoint {
        haldev::DeviceVolumeEndpoint::DefaultOutput => lk(&inner.device_output_volume_io),
        haldev::DeviceVolumeEndpoint::DefaultInput => lk(&inner.device_input_volume_io),
    };
    if !current_peer_connection(inner, conn)
        || haldev::effective_mode(inner) != Mode::Share
        || lk(&conn.peer_mode).mode() != Some(Mode::B)
        || lk(&conn.peer_audio_capabilities).device_volume_version() < 1
    {
        dlog!(
            "[audiohubd] dropping delayed device_volume_set from {} after its authority changed",
            conn.fp
        );
        return;
    }
    let (set_volume, set_mute, readback): (
        fn(f32) -> Result<()>,
        fn(bool) -> Result<()>,
        fn() -> Result<VolumeState>,
    ) = match endpoint {
        haldev::DeviceVolumeEndpoint::DefaultOutput => (
            volume::set_default_output_volume,
            volume::set_default_output_mute,
            volume::get_default_output_volume,
        ),
        haldev::DeviceVolumeEndpoint::DefaultInput => (
            volume::set_default_input_volume,
            volume::set_default_input_mute,
            volume::get_default_input_volume,
        ),
    };

    if let Err(err) = set_volume(scalar) {
        dlog!(
            "[audiohubd] peer {}: set {} volume: {err:#}",
            conn.fp,
            endpoint.as_wire()
        );
    }
    if let Some(muted) = muted {
        if let Err(err) = set_mute(muted) {
            dlog!(
                "[audiohubd] peer {}: set {} mute: {err:#}",
                conn.fp,
                endpoint.as_wire()
            );
        }
    }
    let Ok(state) = readback() else {
        dlog!(
            "[audiohubd] peer {}: cannot read {} after applying device volume",
            conn.fp,
            endpoint.as_wire()
        );
        return;
    };
    *ordered = (*ordered).wrapping_add(1).max(1);
    let revision = *ordered;
    drop(ordered);
    let _ = conn.send_msg(&SessionMsg::DeviceVolumeState {
        endpoint: endpoint.as_wire().to_string(),
        request_id: Some(request_id),
        revision,
        scalar: state.scalar,
        muted: state.muted,
        adjustable: state.adjustable,
        mute_adjustable: state.mute_adjustable,
    });
}

fn notify_pending(conn: &ConnShared, stream_id: u32, res: std::result::Result<(), String>) {
    if let Some(tx) = lk(&conn.pending).remove(&stream_id) {
        let _ = tx.send(res);
    }
}

/// Inbound VolumeSet (spec-m4b §A2): write the peer's value to THIS machine's
/// default output device. Nothing is sent back — the tracker is armed first so
/// the 1s poller recognises the resulting reading as an echo even if it races
/// the write. The media stream is untouched: no gain is ever applied.
///
/// `muted: None` means the peer sent a volume only (a bare slider drag): the
/// mute control is then LEFT ALONE. Resolving it to `false` would unmute a
/// machine somebody deliberately muted.
fn apply_peer_volume(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    stream_id: u32,
    scalar: f32,
    muted: Option<bool>,
    src: &str,
) {
    let Some(e) = owned_session(inner, conn, stream_id, "volume_set") else {
        return;
    };
    let provider = e.kind == KIND_SPK && e.dir == DIR_RECV;
    if let SetAction::Ignore(why) = volume::classify_set(provider, e.volume.enabled, src) {
        dlog!("[audiohubd] ignoring volume_set for stream {stream_id}: {why}");
        return;
    }
    // `src` never carries anything but SRC_LOCAL: this daemon has exactly one
    // emitter (set_session_volume) and a consumer never re-emits what it
    // receives, so nothing relays and nothing can loop. classify_set (in
    // audiohub-core, not this group's file) still admits SRC_PEER for the relay
    // topology that was specified but never built; refusing it here keeps the
    // set of accepted tags equal to the set of emitted ones.
    if src != volume::SRC_LOCAL {
        dlog!("[audiohubd] ignoring volume_set for stream {stream_id}: src is not 'local'");
        return;
    }
    if !scalar.is_finite() {
        dlog!("[audiohubd] ignoring volume_set for stream {stream_id}: scalar is not finite");
        return;
    }
    let s = scalar.clamp(0.0, 1.0);
    // Arm echo suppression with the mute state that will actually hold after
    // this write, so a poll racing us still recognises the reading as an echo.
    let m = muted.unwrap_or_else(|| volume::get_default_output_volume().map_or(false, |v| v.muted));
    lk(&e.volume.sync).note_peer_apply(s, m);
    if let Err(err) = volume::set_default_output_volume(s) {
        dlog!("[audiohubd] stream {stream_id}: set output volume: {err:#}");
    }
    if let Some(m) = muted {
        if let Err(err) = volume::set_default_output_mute(m) {
            dlog!("[audiohubd] stream {stream_id}: set output mute: {err:#}");
        }
    }
    if let Ok(v) = volume::get_default_output_volume() {
        *lk(&e.volume.state) = Some(v);
    }
}

/// plan §7.1 模式 A 「与对端音量同步」, the inbound half: copy the peer's real
/// output reading onto THIS machine's default output.
///
/// **Peer authoritative, and that is the whole conflict rule.** §7.1 says
/// 「双向同时变更冲突时对端值覆盖本机」; writing every inbound reading
/// unconditionally gives exactly that, with no timestamps and nothing to
/// arbitrate. The mute half obeys §7.1's exception via `classify_follow`.
///
/// Nothing is sent back: `note_peer_apply` is armed BEFORE the write (same
/// contract as `apply_peer_volume`) so the 1s consumer poll recognises the
/// resulting reading as an echo even if it races us, and the reading is folded
/// into the tracker afterwards so a write the device REFUSED cannot be reported
/// to the peer a moment later as if the local user had made it — which would
/// invert the authority §7.1 just granted the peer.
fn follow_peer_volume(inner: &Arc<DaemonInner>, e: &SessionEntry, peer: VolumeState) {
    let consumer = e.kind == KIND_SPK && e.dir == DIR_SEND;
    let opt = crate::mode_a_volume(inner);
    let w = match volume::classify_follow(consumer, crate::mode_a_in_force(inner), opt, peer) {
        volume::FollowAction::Ignore(_) => return,
        volume::FollowAction::Apply(w) => w,
    };
    // The mute value that will HOLD after this write: the peer's when the mute
    // half is synced, otherwise whatever the local device already reads. Arming
    // the tracker with anything else would leave the echo unrecognised.
    let hold = w
        .muted
        .or_else(|| volume::get_default_output_volume().ok().map(|v| v.muted))
        .unwrap_or(false);
    lk(&e.volume.sync).note_peer_apply(w.scalar, hold);
    if let Err(err) = volume::set_default_output_volume(w.scalar) {
        dlog!(
            "[audiohubd] stream {}: cannot follow the peer's output volume: {err:#}",
            e.id
        );
    }
    if let Some(m) = w.muted {
        if let Err(err) = volume::set_default_output_mute(m) {
            dlog!(
                "[audiohubd] stream {}: cannot follow the peer's mute state: {err:#}",
                e.id
            );
        }
    }
    if let Ok(cur) = volume::get_default_output_volume() {
        lk(&e.volume.sync).note_reported(cur);
    }
}

/// plan §7.1 模式 A 「静音本机输出」: mute this machine's output ONCE, at the
/// moment a speaker stream to a peer is established.
///
/// **One-shot, deliberately.** §7.1 freezes it as an action, not a state: after
/// this call nothing maintains the mute, so a user who unmutes has said "this
/// machine should sound too" and is not argued with until the next stream comes
/// up. There is therefore no timer, no reconciliation and no place to put one.
///
/// `capture_backend` is the CONCRETE backend id this stream resolved to (`None`
/// when the source is not a system capture at all). §7.1 makes the capture
/// point the precondition — mute a device whose loopback is read after the mix
/// and the peer gets silence — and `capture_survives_local_mute` is the single
/// place that answers it.
fn mute_local_on_connect(inner: &Arc<DaemonInner>, capture_backend: Option<&str>) {
    let opt = crate::mode_a_volume(inner);
    let survives = capture_backend.and_then(sysaudio::capture_survives_local_mute);
    match volume::classify_mute_on_connect(crate::mode_a_in_force(inner), opt, survives) {
        volume::MuteOnConnect::Skip(why) => {
            // Only worth a line when the user actually asked for it: otherwise
            // every session open on every machine logs a setting being off.
            if opt.mute_local {
                dlog!("[audiohubd] 「静音本机」 did not fire: {why}");
            }
        }
        volume::MuteOnConnect::Mute => {
            if let Err(err) = volume::set_default_output_mute(true) {
                dlog!("[audiohubd] 「静音本机」 could not mute this machine: {err:#}");
            }
        }
    }
}

/// IPC `session.set_volume`: the consumer end of a spk stream asks the provider
/// to move its output volume. Always tagged SRC_LOCAL — the change originates
/// with this machine's user, and this is the daemon's ONLY VolumeSet emitter.
///
/// `muted: None` = do not touch the peer's mute state at all. It is not
/// resolved to a cached value here either: the cache can predate the provider's
/// first report, and guessing `false` audibly unmutes a muted machine.
pub(crate) fn set_session_volume(
    inner: &Arc<DaemonInner>,
    id: u32,
    scalar: f32,
    muted: Option<bool>,
) -> Result<()> {
    let e = lk(&inner.state)
        .sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("unknown session {id}"))?;
    if !(e.kind == KIND_SPK && e.dir == DIR_SEND) {
        bail!("session {id} is not a spk stream this side drives");
    }
    if !e.volume.enabled {
        bail!("session {id} was not opened with volume_sync");
    }
    if !scalar.is_finite() {
        bail!("scalar must be finite");
    }
    let s = scalar.clamp(0.0, 1.0);
    // plan §7.2 兜底支路。对端真实设备没有可写音量时，标量**不下发**——下发也
    // 只会在对端的 `set_default_output_volume` 上失败一次、留一行日志、旋钮照旧
    // 是假的——改为在本机发送侧施加软件增益，线上从此传带音量的音频。
    //
    // 判据取**旗标**而不是当场重算 `authority_for`：接手这件事只在
    // `SessionMsg::VolumeState` 那一处发生（那里还要一并管模式门与起点播种），
    // 两处各判一次就会有「一处认为已接手、另一处还在往线上发」的窗口。
    if e.volume.software_gain.load(Ordering::Relaxed) {
        return apply_send_gain(&e, s, muted);
    }
    e.conn.send_msg(&SessionMsg::VolumeSet {
        stream_id: id,
        scalar: s,
        muted,
        src: volume::SRC_LOCAL.to_string(),
    })?;
    // Optimistic local echo so the UI tracks the slider immediately; the
    // provider's next VolumeState replaces it with what the device really did.
    let last = *lk(&e.volume.state);
    let adjustable = last.map_or(true, |v| v.adjustable);
    let mute_adjustable = last.map_or(adjustable, |v| v.mute_adjustable);
    let shown = muted.or_else(|| last.map(|v| v.muted)).unwrap_or(false);
    *lk(&e.volume.state) = Some(VolumeState {
        scalar: s,
        muted: shown,
        adjustable,
        mute_adjustable,
    });
    Ok(())
}

/// plan §7.2 兜底支路的**写入端**：把音量记到这条流的发送侧增益上，并让显示
/// 单元格跟着走（`push_peer_volumes` 会把它一路推到虚拟设备的音量控件上）。
///
/// **静音折进增益。** 对端设备连音量都写不进去，`set_default_output_mute` 同样
/// 会失败（macOS 聚合设备两个属性一起缺），所以静音这一半也只能在这边兑现。
/// 它走的是同一条 20 ms 斜坡，于是静音/解除静音同样不会爆音。plan §16 把静音
/// 动作定为音量同步的一部分，这里就是它在兜底支路上的形态。
///
/// 显示值里的 `adjustable` 仍然是 `false`：那是关于**对端设备**的事实，没有变。
/// 「旋钮此刻是真的」由 `SessionStats::volume_software_gain` 单独说。
fn apply_send_gain(e: &SessionEntry, scalar: f32, muted: Option<bool>) -> Result<()> {
    let tx = e.tx.as_ref().ok_or_else(|| {
        anyhow!(
            "session {} has no send stream to carry the software gain",
            e.id
        )
    })?;
    let last = *lk(&e.volume.state);
    // `None` = 不动静音态，与 `SessionMsg::VolumeSet` 的语义一致。
    let m = muted.or_else(|| last.map(|v| v.muted)).unwrap_or(false);
    tx.send_gain.store(
        TxShared::gain_bits(if m { 0.0 } else { scalar }),
        Ordering::Relaxed,
    );
    *lk(&e.volume.state) = Some(VolumeState {
        scalar,
        muted: m,
        adjustable: false,
        mute_adjustable: last.is_some_and(|state| state.mute_adjustable),
    });
    Ok(())
}

/// 兜底接手：这条流的音量从此由本机的发送侧增益兑现。
///
/// **起点取 1.0，不取对端上报的那个标量。** 一个真的没有音量属性的设备
///（macOS 聚合设备）上报的 `scalar` 恒为 0 —— `volume.rs::get` 在找不到任何
/// 通道元素时就是这么取的 —— 拿它当增益等于一接手就静音。而且接手的那一刻
/// **没有任何一侧在衰减**：对端写不进去，虚拟设备的音量是纯控制节点、不碰采样
///（`docs/spec-windows-driver.md` §1.4「不存在双重衰减」）。所以 1.0 既是响度上
/// 的**不变**，也是显示上的**实话**——「此刻没有人在衰减」。
fn engage_send_gain(e: &SessionEntry) {
    if e.volume.software_gain.swap(true, Ordering::Relaxed) {
        return; // 已经在兜底里了
    }
    if let Some(tx) = e.tx.as_ref() {
        tx.send_gain
            .store(TxShared::gain_bits(1.0), Ordering::Relaxed);
    }
    *lk(&e.volume.state) = Some(VolumeState {
        scalar: 1.0,
        muted: false,
        adjustable: false,
        mute_adjustable: false,
    });
    dlog!(
        "[audiohubd] stream {}: the peer's output device has no volume we can drive; this side \
         takes the volume over as send-side software gain (plan §7.2)",
        e.id
    );
}

/// 交还：对端换回了能调音量的设备（plan §7.2「对端默认设备切换后重新协商」）。
/// 增益斜坡回到 1.0（`SendGain` 的 20 ms 渐变，不爆音），显示值改由对端驱动。
fn release_send_gain(e: &SessionEntry) {
    if !e.volume.software_gain.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = e.tx.as_ref() {
        tx.send_gain.store(crate::SEND_GAIN_OFF, Ordering::Relaxed);
    }
    dlog!(
        "[audiohubd] stream {}: the peer's output is adjustable again, handing the volume back \
         to it (plan §7.2); the send-side gain ramps to unity",
        e.id
    );
}

/// Apply one peer-level fallback knob to every active mode-B speaker stream
/// for that peer. The desired state lives in `haldev`, so it survives idle
/// periods; sessions are merely the current executors of that persistent
/// intent.
pub(crate) fn sync_peer_software_gain(
    inner: &Arc<DaemonInner>,
    fingerprint: &str,
    desired: Option<VolumeState>,
) {
    let desired = (haldev::effective_mode(inner) == Mode::B)
        .then_some(desired)
        .flatten();
    let sessions: Vec<SessionEntry> = lk(&inner.state)
        .sessions
        .values()
        .filter(|entry| {
            entry.conn.fp == fingerprint
                && entry.kind == KIND_SPK
                && entry.dir == DIR_SEND
                && entry.volume.enabled
        })
        .cloned()
        .collect();
    for entry in sessions {
        if let Some(wanted) = desired {
            engage_send_gain(&entry);
            if let Err(err) = apply_send_gain(&entry, wanted.scalar, Some(wanted.muted)) {
                dlog!(
                    "[audiohubd] stream {}: apply peer-level software gain: {err:#}",
                    entry.id
                );
            }
        } else {
            release_send_gain(&entry);
        }
    }
}

fn decode_media_salt(b64: &str) -> Result<Vec<u8>> {
    let salt = BASE64_STANDARD
        .decode(b64)
        .map_err(|e| anyhow!("media_salt_b64 is not base64: {e}"))?;
    if salt.len() != MEDIA_SALT_LEN {
        bail!(
            "media_salt_b64 must decode to {MEDIA_SALT_LEN} bytes, got {}",
            salt.len()
        );
    }
    Ok(salt)
}

/// Stream-count admission. Each stream costs a fan-out slot in the 10ms tx
/// scheduler and a jitter buffer + pop in the 10ms mixer, so an unbounded peer
/// can starve both loops for every other session.
pub(crate) struct StreamClaim {
    connection_id: u64,
}

fn reserve_pending_stream(
    claims: &mut HashMap<u32, Arc<StreamClaim>>,
    stream_id: u32,
    connection_id: u64,
    active_total: usize,
    active_mine: usize,
    already_used: bool,
) -> Result<Arc<StreamClaim>> {
    if already_used || claims.contains_key(&stream_id) {
        bail!("stream id {stream_id} in use");
    }
    if active_total.saturating_add(claims.len()) >= MAX_STREAMS_TOTAL {
        bail!("daemon stream limit reached ({MAX_STREAMS_TOTAL})");
    }
    let pending_mine = claims.values().filter(|claim| claim.connection_id == connection_id).count();
    if active_mine.saturating_add(pending_mine) >= MAX_STREAMS_PER_CONN {
        bail!("per-connection stream limit reached ({MAX_STREAMS_PER_CONN})");
    }
    let claim = Arc::new(StreamClaim { connection_id });
    claims.insert(stream_id, claim.clone());
    Ok(claim)
}

fn release_pending_stream(claims: &mut HashMap<u32, Arc<StreamClaim>>, stream_id: u32, claim: &Arc<StreamClaim>) {
    if claims.get(&stream_id).is_some_and(|current| Arc::ptr_eq(current, claim)) {
        claims.remove(&stream_id);
    }
}

struct StreamReservation<'a> {
    inner: &'a DaemonInner,
    conn: Arc<ConnShared>,
    stream_id: u32,
    claim: Arc<StreamClaim>,
}

impl StreamReservation<'_> {
    fn publish(&self, state: &mut DaemonState, entry: SessionEntry) -> Result<()> {
        if entry.id != self.stream_id || !Arc::ptr_eq(&entry.conn, &self.conn)
            || !state.stream_claims.get(&self.stream_id).is_some_and(|c| Arc::ptr_eq(c, &self.claim))
        {
            bail!("stream reservation is no longer owned by this opener");
        }
        if !self.conn.alive.load(Ordering::SeqCst)
            || !state.conns.get(&self.conn.fp).is_some_and(|c| Arc::ptr_eq(c, &self.conn))
        {
            bail!("control connection changed while the stream was opening");
        }
        if state.sessions.contains_key(&self.stream_id) {
            bail!("stream id {} was already committed", self.stream_id);
        }
        state.sessions.insert(self.stream_id, entry);
        release_pending_stream(&mut state.stream_claims, self.stream_id, &self.claim);
        Ok(())
    }
}

impl Drop for StreamReservation<'_> {
    fn drop(&mut self) {
        release_pending_stream(&mut lk(&self.inner.state).stream_claims, self.stream_id, &self.claim);
    }
}

fn claim_stream_id<'a>(inner: &'a DaemonInner, conn: &Arc<ConnShared>, stream_id: u32) -> Result<StreamReservation<'a>> {
    let mut state = lk(&inner.state);
    if !conn.alive.load(Ordering::SeqCst)
        || !state.conns.get(&conn.fp).is_some_and(|current| Arc::ptr_eq(current, conn))
    {
        bail!("control connection is no longer current");
    }
    let used = state.sessions.contains_key(&stream_id) || rd(&inner.rx_table).contains_key(&stream_id);
    let total = state.sessions.len();
    let mine = state.sessions.values().filter(|entry| Arc::ptr_eq(&entry.conn, conn)).count();
    let claim = reserve_pending_stream(&mut state.stream_claims, stream_id, conn.connection_id, total, mine, used)?;
    Ok(StreamReservation { inner, conn: conn.clone(), stream_id, claim })
}

#[cfg(test)]
mod spatial_admission_tests {
    use super::*;

    #[test]
    fn spatial_concurrent_openers_cannot_reserve_the_same_stream_id() {
        let claims = Arc::new(Mutex::new(HashMap::new()));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (1..=2).map(|owner| {
            let claims = Arc::clone(&claims);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                reserve_pending_stream(&mut lk(&claims), 42, owner, 0, 0, false).is_ok()
            })
        }).collect();
        barrier.wait();
        assert_eq!(workers.into_iter().map(|worker| usize::from(worker.join().unwrap())).sum::<usize>(), 1);
        assert_eq!(lk(&claims).len(), 1);
    }

    #[test]
    fn spatial_pending_activations_count_toward_global_and_connection_quotas() {
        let mut claims = HashMap::new();
        reserve_pending_stream(&mut claims, 1, 8, MAX_STREAMS_TOTAL - 1, 0, false).unwrap();
        assert!(reserve_pending_stream(&mut claims, 2, 9, MAX_STREAMS_TOTAL - 1, 0, false).is_err());
        claims.clear();
        for id in 0..MAX_STREAMS_PER_CONN as u32 {
            reserve_pending_stream(&mut claims, id, 8, 0, 0, false).unwrap();
        }
        assert!(reserve_pending_stream(&mut claims, 100, 8, 0, 0, false).is_err());
        assert!(reserve_pending_stream(&mut claims, 100, 9, 0, 0, false).is_ok());
        assert!(reserve_pending_stream(&mut claims, 101, 10, 0, 0, true).is_err());
    }

    #[test]
    fn spatial_failed_opener_cannot_release_a_successor_claim_for_the_same_connection() {
        let mut claims = HashMap::new();
        let old = reserve_pending_stream(&mut claims, 42, 8, 0, 0, false).unwrap();
        release_pending_stream(&mut claims, 42, &old);
        let new = reserve_pending_stream(&mut claims, 42, 8, 0, 0, false).unwrap();
        release_pending_stream(&mut claims, 42, &old);
        assert!(Arc::ptr_eq(claims.get(&42).unwrap(), &new));
        release_pending_stream(&mut claims, 42, &new);
        assert!(claims.is_empty());
    }
}

/// Starts a local media source for `stream_id` and waits for the tx thread to
/// confirm it: the source must exist before we accept the stream, otherwise a
/// mic-permission failure yields an accepted-but-permanently-silent stream.
#[allow(clippy::too_many_arguments)]
fn start_tx_stream(
    inner: &DaemonInner,
    stream_id: u32,
    key: [u8; 32],
    salt: Vec<u8>,
    path: crate::tcpmedia::MediaPath,
    spec: SourceSpec,
    channels: u8,
    loss_pct: f32,
    shared: Arc<TxShared>,
) -> Result<()> {
    let (ack_tx, ack_rx) = mpsc::channel();
    lk(&inner.tx_cmds)
        .send(TxCmd::Add {
            stream_id,
            key,
            salt,
            path,
            spec,
            channels,
            loss_pct,
            shared,
            ack: Some(ack_tx),
        })
        .map_err(|_| anyhow!("media engine unavailable"))?;
    match ack_rx.recv_timeout(SOURCE_ACK_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => bail!("{e}"),
        Err(_) => {
            let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
            bail!("media source did not start within {SOURCE_ACK_TIMEOUT:?}")
        }
    }
}

/// Auto-accept policy (spec §2): the peer is paired+verified by construction, so
/// any well-formed OpenStream within the stream caps is accepted — **provided
/// this machine is in a mode that may be used at all** (plan §13).
///
/// That mode check is the enforcement half of the whole §13 change. Both
/// directions of an inbound `OpenStream` are "a peer using this machine":
/// `dir == send` means the peer plays into our default output, `dir == recv`
/// means the peer consumes our microphone / system audio / virtual speaker.
/// Neither is allowed while we are ourselves a consumer, because our "default
/// device" may then BE another machine's device, and handing it out makes us a
/// relay — and a cycle, if that machine is using us back.
///
/// Refused before `claim_stream_id`, so a refusal leaves no state behind and a
/// peer that keeps retrying cannot exhaust the stream-id table.
#[allow(clippy::too_many_arguments)]
fn handle_remote_open(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    stream_id: u32,
    kind: &str,
    dir: &str,
    sample_rate: u32,
    channels: u8,
    media_salt_b64: &str,
    verify_freq: Option<f32>,
    source: Option<&str>,
    freq: Option<f32>,
    backend: Option<&str>,
    loss: Option<f32>,
    volume_sync: bool,
    rx_latency: Option<&str>,
    tx_quality: Option<&str>,
) -> Result<()> {
    if let Some(why) = haldev::refuse_being_used(haldev::effective_mode(inner)) {
        bail!("{why}");
    }
    if kind != KIND_MIC && kind != KIND_SPK {
        bail!("unknown kind {kind}");
    }
    if sample_rate != 48_000 {
        bail!("OpenStream base sample rate must be 48000, got {sample_rate}");
    }
    if !(1..=2).contains(&channels) {
        bail!("OpenStream channels must be 1 or 2, got {channels}");
    }
    let endpoint = required_local_endpoint_for_remote_open(kind, dir, source);
    let endpoint_guard = loop {
        // Bracket the property query with the relevant epoch. This prevents a
        // disappearance between "present" and sampling the guard from being
        // accidentally blessed as part of the newer generation.
        let before = endpoint.map(|e| e.epoch(inner));
        let presence = audiohub_core::audio::default_device_presence();
        let after = endpoint.map(|e| e.epoch(inner));
        if before != after {
            continue;
        }
        require_local_audio_endpoint(kind, dir, source, presence)?;
        break endpoint.zip(after);
    };
    let salt = decode_media_salt(media_salt_b64)?;
    let reservation = claim_stream_id(inner, conn, stream_id)?;
    // plan §15：档位的初值随 `OpenStream` 一起到，于是从第一个媒体包起对端要的
    // 档位就在生效。只有增量消息的话，这中间有一个窗口跑我们自己的默认值——
    // 那正是「我设的值此刻没有在生效」这一种最难解释的现象。
    //
    // 未表态（`None`）保持默认，**不写成 auto**：两者在这里的执行上相同，但
    // 在 `peers.list` 的回显上不同（「未设定 · 按自动运行」vs「对方要求 auto」）。
    let pushed = Arc::new(crate::PushedTransport::default());
    if let Some(v) = rx_latency {
        *lk(&pushed.rx_latency) = audiohub_ipc::LatencyTarget::parse(v);
    }
    if let Some(v) = tx_quality {
        *lk(&pushed.tx_quality) = audiohub_ipc::QualityTarget::parse(v);
    }
    match dir {
        // opener sends media -> we receive; verify_freq applies here
        DIR_SEND => {
            // ONE read of the path, shared by the stream that is bound to it and
            // by the tier the session reports. Two reads could straddle an
            // attach and leave a stream saying it travels somewhere it does not.
            let path = conn.current_media_path();
            let media_tier = path.tier_wire();
            let media_frame_tag_version =
                lk(&conn.peer_audio_capabilities).media_frame_tag_version();
            let rx = Arc::new(
                RxStream::new(
                    stream_id,
                    &conn.rx_key,
                    &salt,
                    verify_freq,
                    kind == KIND_SPK, // spk-recv joins the mixer
                    false,
                    None, // bridging is the local consumer's choice, never the peer's
                    None, // ...and so is the virtual microphone (spec-m5b §5.4)
                    path,
                    channels,
                )
                .with_media_frame_tag_version(media_frame_tag_version),
            );
            wr(&inner.rx_table).insert(stream_id, rx.clone());
            let entry = SessionEntry {
                id: stream_id,
                conn: conn.clone(),
                kind: kind.to_string(),
                dir: DIR_RECV.to_string(),
                source: source.map(str::to_string),
                rx: Some(rx),
                tx: None,
                // we play this stream out of our own default output, so we
                // are the provider the peer's slider drives
                volume: Arc::new(VolumeCell::new(volume_sync && kind == KIND_SPK)),
                replay: None, // the opener re-opens it after a reconnect
                origin: SessionOrigin::Peer,
                peer_lat: Arc::new(PeerLatCell::new()),
                pushed: pushed.clone(),
                armed_conn_ms: conn.clock_ms(),
                media_tier,
            };
            if let Err(e) =
                insert_remote_session_if_endpoint_unchanged(inner, entry, endpoint_guard, &reservation)
            {
                // This receive stream became externally reachable when it was
                // put in rx_table. Undo that publication before rejecting the
                // OpenStream; no SessionEntry exists for teardown_stream yet.
                wr(&inner.rx_table).remove(&stream_id);
                return Err(e);
            }
        }
        // opener receives -> we are the media source (provider side)
        DIR_RECV => {
            // A peer asking us for `halspk` is asking for OUR virtual speaker
            // for IT — which is the slot we bound in its name.
            let slot = lk(&inner.haldev).slot_of(&conn.fp);
            let spec = source_spec(source, freq, backend, slot)?;
            let path = conn.current_media_path();
            let media_tier = path.tier_wire();
            let shared = Arc::new(TxShared::new_on(path.auto_top_rung()));
            start_tx_stream(
                inner,
                stream_id,
                conn.tx_key,
                salt,
                path,
                spec,
                channels,
                loss.unwrap_or(0.0),
                shared.clone(),
            )?;
            let entry = SessionEntry {
                id: stream_id,
                conn: conn.clone(),
                kind: kind.to_string(),
                dir: DIR_SEND.to_string(),
                source: source.map(str::to_string),
                rx: None,
                tx: Some(shared),
                // mic provider: the opener consumes OUR source, no output
                // device of ours is involved
                volume: Arc::new(VolumeCell::new(false)),
                replay: None,
                origin: SessionOrigin::Peer,
                peer_lat: Arc::new(PeerLatCell::new()),
                pushed: pushed.clone(),
                armed_conn_ms: conn.clock_ms(),
                media_tier,
            };
            if let Err(e) =
                insert_remote_session_if_endpoint_unchanged(inner, entry, endpoint_guard, &reservation)
            {
                // start_tx_stream already installed the source/stream in the
                // media engine. No SessionEntry exists yet, so remove it via
                // the engine command directly before rejecting the open.
                let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
                return Err(e);
            }
        }
        other => bail!("unknown dir {other}"),
    }
    Ok(())
}

fn handle_remote_spatial_open(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    stream_id: u32,
    media_salt_b64: &str,
    contract: audiohub_net::spatial_media::SpatialMediaContract,
    rx_latency: Option<&str>,
) -> Result<()> {
    if let Some(reason) = haldev::refuse_being_used(haldev::effective_mode(inner)) {
        bail!("{reason}");
    }
    let latency = rx_latency.map(|value| {
        audiohub_ipc::LatencyTarget::parse(value).ok_or_else(|| anyhow!("invalid spatial latency target"))
    }).transpose()?;
    crate::spatial::require_current(&contract, &lk(&inner.native_output))?;
    let salt = decode_media_salt(media_salt_b64)?;
    let reservation = claim_stream_id(inner, conn, stream_id)?;
    let output_epoch = inner.dev_out_epoch.load(Ordering::Acquire);
    let spatial = crate::spatial::SpatialReceive::start(contract.clone(), output_epoch)?;
    // Native activation is asynchronous; neither a mode change nor a new
    // observation while it was opening can authorize the old contract.
    if let Some(reason) = haldev::refuse_being_used(haldev::effective_mode(inner)) {
        bail!("{reason}");
    }
    crate::spatial::require_current(&contract, &lk(&inner.native_output))?;
    let path = conn.current_media_path();
    let media_tier = path.tier_wire();
    let tuning = engine::jb_tuning_for(&path);
    let mut rx = RxStream::new(
        stream_id, &conn.rx_key, &salt, None,
        false, // Native spatial output never enters the ordinary stereo sum.
        false, None, None, path, 2,
    );
    rx.channels = contract.channels();
    {
        let state = rx.jbs.get_mut().unwrap_or_else(|e| e.into_inner());
        state.channels = contract.channels();
        state.jb = audiohub_net::media::JitterBuffer::with_tuning_channels(2, tuning, contract.channels());
    }
    rx.post.get_mut().unwrap_or_else(|e| e.into_inner()).channels = contract.channels();
    rx.spatial = Some(Mutex::new(spatial));
    let rx = Arc::new(rx);
    let pushed = Arc::new(crate::PushedTransport::default());
    *lk(&pushed.rx_latency) = latency;
    let entry = SessionEntry {
        id: stream_id, conn: conn.clone(), kind: KIND_SPK.to_string(), dir: DIR_RECV.to_string(),
        source: None, rx: Some(rx.clone()), tx: None,
        volume: Arc::new(VolumeCell::new(true)), replay: None, origin: SessionOrigin::Peer,
        peer_lat: Arc::new(PeerLatCell::new()), pushed, armed_conn_ms: conn.clock_ms(), media_tier,
    };
    wr(&inner.rx_table).insert(stream_id, rx);
    if let Err(error) = insert_remote_session_if_endpoint_unchanged(
        inner, entry, Some((MissingLocalEndpoint::DefaultOutput, output_epoch)), &reservation,
    ) {
        wr(&inner.rx_table).remove(&stream_id);
        return Err(error);
    }
    Ok(())
}

/// Enforce the endpoint on the machine that owns it, independently of what it
/// advertised earlier. `kind` is the OpenStream sender's intent and `dir` is
/// media flow relative to that sender: mic/recv consumes our input, while
/// spk/send renders to our output. A synthetic/alternate mic source explicitly
/// replaces the default input and therefore does not require one.
fn require_local_audio_endpoint(
    kind: &str,
    dir: &str,
    source: Option<&str>,
    presence: audiohub_core::audio::DefaultDevicePresence,
) -> Result<()> {
    match (kind, dir) {
        (KIND_MIC, DIR_RECV) => {
            let uses_default_input = source.is_none() || source == Some(SOURCE_MIC);
            if uses_default_input && !presence.input {
                bail!("this machine has no default input audio device");
            }
        }
        (KIND_SPK, DIR_SEND) => {
            if !presence.output {
                bail!("this machine has no default output audio device");
            }
        }
        (KIND_MIC, other) => {
            bail!("kind '{KIND_MIC}' requires dir '{DIR_RECV}', received '{other}'");
        }
        (KIND_SPK, other) => {
            bail!("kind '{KIND_SPK}' requires dir '{DIR_SEND}', received '{other}'");
        }
        (other, _) => bail!("unknown kind {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod audio_endpoint_capability_tests {
    use super::*;
    use audiohub_core::audio::DefaultDevicePresence;

    #[test]
    fn remote_open_requires_the_real_endpoint_selected_by_kind_and_direction() {
        let output_only = DefaultDevicePresence {
            input: false,
            output: true,
        };
        assert!(require_local_audio_endpoint(KIND_SPK, DIR_SEND, None, output_only).is_ok());
        let err = require_local_audio_endpoint(KIND_MIC, DIR_RECV, None, output_only)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default input"), "{err}");

        let input_only = DefaultDevicePresence {
            input: true,
            output: false,
        };
        assert!(require_local_audio_endpoint(KIND_MIC, DIR_RECV, None, input_only).is_ok());
        let err = require_local_audio_endpoint(KIND_SPK, DIR_SEND, None, input_only)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default output"), "{err}");
    }

    #[test]
    fn alternate_mic_sources_stay_deviceless_but_direction_cannot_be_forged() {
        let none = DefaultDevicePresence::default();
        for source in [SOURCE_TONE, SOURCE_SYSAUDIO, SOURCE_HAL_SPEAKER] {
            assert!(require_local_audio_endpoint(KIND_MIC, DIR_RECV, Some(source), none).is_ok());
        }
        let err = require_local_audio_endpoint(KIND_MIC, DIR_SEND, Some(SOURCE_TONE), none)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires dir 'recv'"), "{err}");
        let err = require_local_audio_endpoint(KIND_SPK, DIR_RECV, None, none)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires dir 'send'"), "{err}");
    }

    #[test]
    fn capability_loss_closes_only_the_exact_peer_originated_default_endpoint_matrix() {
        let none = DefaultDevicePresence::default();
        let input_only = DefaultDevicePresence {
            input: true,
            output: false,
        };
        let output_only = DefaultDevicePresence {
            input: false,
            output: true,
        };
        let both = DefaultDevicePresence {
            input: true,
            output: true,
        };

        // Peer-opened speaker receive is backed by this machine's output.
        assert_eq!(
            missing_local_endpoint_for_peer_session(
                SessionOrigin::Peer,
                KIND_SPK,
                DIR_RECV,
                None,
                none,
            ),
            Some(MissingLocalEndpoint::DefaultOutput)
        );
        assert_eq!(
            missing_local_endpoint_for_peer_session(
                SessionOrigin::Peer,
                KIND_SPK,
                DIR_RECV,
                None,
                input_only,
            ),
            Some(MissingLocalEndpoint::DefaultOutput)
        );
        assert_eq!(
            missing_local_endpoint_for_peer_session(
                SessionOrigin::Peer,
                KIND_SPK,
                DIR_RECV,
                None,
                output_only,
            ),
            None
        );

        // Peer-opened mic send uses the default input only for omitted/`mic`
        // source. Every alternate source survives input capability loss.
        for source in [None, Some(SOURCE_MIC)] {
            assert_eq!(
                missing_local_endpoint_for_peer_session(
                    SessionOrigin::Peer,
                    KIND_MIC,
                    DIR_SEND,
                    source,
                    none,
                ),
                Some(MissingLocalEndpoint::DefaultInput)
            );
            assert_eq!(
                missing_local_endpoint_for_peer_session(
                    SessionOrigin::Peer,
                    KIND_MIC,
                    DIR_SEND,
                    source,
                    output_only,
                ),
                Some(MissingLocalEndpoint::DefaultInput)
            );
        }
        for source in [SOURCE_TONE, SOURCE_SYSAUDIO, SOURCE_HAL_SPEAKER] {
            assert_eq!(
                missing_local_endpoint_for_peer_session(
                    SessionOrigin::Peer,
                    KIND_MIC,
                    DIR_SEND,
                    Some(source),
                    none,
                ),
                None,
                "alternate source {source} must not depend on the default input"
            );
        }

        // Direction and origin are part of the safety boundary: malformed
        // pairings and locally opened/HAL sessions are not owned by this path.
        for (origin, kind, dir, source) in [
            (SessionOrigin::Peer, KIND_SPK, DIR_SEND, None),
            (SessionOrigin::Peer, KIND_MIC, DIR_RECV, None),
            (SessionOrigin::User, KIND_SPK, DIR_RECV, None),
            (SessionOrigin::User, KIND_MIC, DIR_SEND, None),
            (SessionOrigin::Hal { slot: 0 }, KIND_SPK, DIR_RECV, None),
            (SessionOrigin::Hal { slot: 0 }, KIND_MIC, DIR_SEND, None),
        ] {
            assert_eq!(
                missing_local_endpoint_for_peer_session(origin, kind, dir, source, none),
                None,
                "origin={origin:?} kind={kind} dir={dir}"
            );
        }

        assert_eq!(
            missing_local_endpoint_for_peer_session(
                SessionOrigin::Peer,
                KIND_MIC,
                DIR_SEND,
                None,
                input_only,
            ),
            None
        );
        assert_eq!(
            missing_local_endpoint_for_peer_session(
                SessionOrigin::Peer,
                KIND_SPK,
                DIR_RECV,
                None,
                both,
            ),
            None
        );
    }

    #[test]
    fn endpoint_epoch_change_during_media_start_rejects_the_final_commit() {
        // `started_epoch` is sampled before `start_tx_stream`, which may wait
        // for SOURCE_ACK_TIMEOUT. A watcher callback during that wait bumps the
        // endpoint epoch. The final commit must reject even though its earlier
        // presence query succeeded and the watcher could not yet see a
        // SessionEntry in the table.
        let epoch = Arc::new(AtomicU64::new(41));
        let started_epoch = epoch.load(Ordering::Relaxed);
        let (media_started_tx, media_started_rx) = mpsc::channel();
        let (release_media_tx, release_media_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker_epoch = Arc::clone(&epoch);
        let worker = std::thread::spawn(move || {
            media_started_tx.send(()).expect("media-start barrier");
            release_media_rx.recv().expect("source ack barrier");
            result_tx
                .send(endpoint_epoch_still_current_at_commit(
                    MissingLocalEndpoint::DefaultInput,
                    started_epoch,
                    worker_epoch.load(Ordering::Relaxed),
                ))
                .expect("commit result");
        });
        media_started_rx.recv().expect("media construction started");
        epoch.fetch_add(1, Ordering::Relaxed); // watcher callback during the wait
        release_media_tx
            .send(())
            .expect("let source construction finish");
        let err = result_rx
            .recv()
            .expect("commit result")
            .unwrap_err()
            .to_string();
        worker.join().expect("media-start worker");
        assert!(
            err.contains("changed while the peer stream was opening"),
            "{err}"
        );

        assert!(
            endpoint_epoch_still_current_at_commit(MissingLocalEndpoint::DefaultInput, 41, 41,)
                .is_ok()
        );

        let err = endpoint_epoch_still_current_at_commit(
            MissingLocalEndpoint::DefaultOutput,
            u64::MAX,
            0,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("default output"), "{err}");
    }
}

/// `peer_slot` is the slot whose virtual devices belong to the peer at the
/// other end of this stream, when it has any. Only `halspk` uses it, and it is
/// resolved by the daemon from the peer's FINGERPRINT — a slot is never named
/// on the wire or in the IPC contract (spec-m5b §5.6).
fn source_spec(
    source: Option<&str>,
    freq: Option<f32>,
    backend: Option<&str>,
    peer_slot: Option<u8>,
) -> Result<SourceSpec> {
    match source {
        Some(SOURCE_TONE) => Ok(SourceSpec::tone(freq.unwrap_or(1000.0))),
        Some(SOURCE_MIC) | None => Ok(SourceSpec::Mic),
        Some(SOURCE_SYSAUDIO) => {
            let want = backend
                .filter(|b| !b.is_empty())
                .unwrap_or(sysaudio::BACKEND_AUTO);
            // Resolved here, not in the tx thread: an unknown/absent backend
            // must be an OpenStream rejection with a reason, not a stream that
            // is accepted and then fails its source ack five seconds later.
            // Storing the concrete id also keeps "auto" and the id it resolves
            // to sharing ONE capture instead of opening the device twice.
            let info = sysaudio::resolve_backend(want)?;
            Ok(SourceSpec::SysAudio { backend: info.id })
        }
        Some("airplay") => bail!("AirPlay 音频始终在接收端本机播放，不能作为 AudioHub peer source"),
        // spec-m5b §5.4: whatever an app played into THIS PEER's virtual
        // speaker. The bridge check belongs to build_source (it is the thread
        // that owns the ring), and a missing bridge fails the open there with
        // its reason. A peer with no slot is refused here instead: the ring
        // that would carry this stream does not exist, and accepting it would
        // produce a session that is silent forever with nothing saying why.
        Some(SOURCE_HAL_SPEAKER) => {
            let slot = peer_slot.ok_or_else(|| {
                anyhow!(
                    "this peer has no virtual devices, so there is no speaker ring to read \
                     (mode B is not in force, the driver is absent, or the slot pool is full)"
                )
            })?;
            Ok(SourceSpec::HalSpeaker { slot })
        }
        Some(other) => bail!("unknown source {other}"),
    }
}

fn preferred_source_channels(source: Option<&str>) -> u8 {
    match source {
        Some(SOURCE_SYSAUDIO) | Some(SOURCE_HAL_SPEAKER) => 2,
        _ => 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerOpenContract {
    Waiting,
    Ready(u8),
}

fn peer_contract_deadline_expired(
    registration_ready: bool,
    mode: crate::PeerModeCell,
    capabilities: crate::PeerAudioCapabilitiesCell,
    now: Instant,
    deadline: Instant,
) -> bool {
    registration_ready
        && (matches!(mode, crate::PeerModeCell::Unheard)
            || matches!(capabilities, crate::PeerAudioCapabilitiesCell::Unheard))
        && now >= deadline
}

fn peer_open_contract(
    registration_ready: bool,
    mode: crate::PeerModeCell,
    capabilities: crate::PeerAudioCapabilitiesCell,
) -> PeerOpenContract {
    if !registration_ready
        || matches!(mode, crate::PeerModeCell::Unheard)
        || matches!(capabilities, crate::PeerAudioCapabilitiesCell::Unheard)
    {
        return PeerOpenContract::Waiting;
    }
    // Hearing the mode is part of the registration contract, but it is not a
    // caller-side authority check.  A non-share peer must receive OpenStream
    // and refuse it from its own effective-mode gate; otherwise a stale or
    // forged ModeState could replace the provider's real enforcement decision.
    // The mode-B coordinator separately selects only known Share peers.
    PeerOpenContract::Ready(capabilities.max_media_channels())
}

fn peer_media_channels_for_open(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) -> Result<u8> {
    // Tier-1 negotiation can legally occupy eight seconds while ModeState and
    // AudioCapabilities wait in `deferred`. The connection must be visible so
    // an attach ticket can resolve it, but a stream must not consume that
    // half-registered connection: doing so freezes its provisional UDP path
    // and mono fallback for the whole session. Every protocol-compatible peer,
    // including 1.0.0, sends both advertisements; the old missing channel field
    // deserialises as one after the real message arrives.
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if !conn.alive.load(Ordering::Acquire) {
            bail!("peer connection closed before its audio contract became ready");
        }
        let mode = *lk(&conn.peer_mode);
        let capabilities = *lk(&conn.peer_audio_capabilities);
        match peer_open_contract(
            conn.registration_ready.load(Ordering::Acquire),
            mode,
            capabilities,
        ) {
            PeerOpenContract::Ready(channels) => return Ok(channels),
            PeerOpenContract::Waiting => {}
        }
        if Instant::now() >= deadline {
            // This exact Arc was published before negotiation so the media
            // ticket could resolve it. If its contract never followed, leaving
            // it alive would make every mode-B open wait/skip forever while the
            // UI still called the peer online.
            teardown_conn(inner, conn);
            bail!(
                "peer connection did not finish media/mode/audio-capability registration within \
                 {HANDSHAKE_TIMEOUT:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod peer_open_contract_tests {
    use super::{peer_contract_deadline_expired, peer_open_contract, PeerOpenContract};
    use crate::{PeerAudioCapabilitiesCell, PeerModeCell};
    use audiohub_ipc::Mode;
    use std::time::{Duration, Instant};

    fn capabilities(channels: u8) -> PeerAudioCapabilitiesCell {
        PeerAudioCapabilitiesCell::Known {
            default_input: true,
            default_output: true,
            max_media_channels: channels,
            device_volume_version: 1,
            media_frame_tag_version: 1,
        }
    }

    #[test]
    fn attach_and_both_advertisements_are_one_stream_open_gate() {
        let share = PeerModeCell::Known(Mode::Share);
        let stereo = capabilities(2);
        assert_eq!(
            peer_open_contract(false, share, stereo),
            PeerOpenContract::Waiting,
            "published-before-attach connections must not leak provisional UDP/mono state"
        );
        assert_eq!(
            peer_open_contract(true, PeerModeCell::Unheard, stereo),
            PeerOpenContract::Waiting
        );
        assert_eq!(
            peer_open_contract(true, share, PeerAudioCapabilitiesCell::Unheard),
            PeerOpenContract::Waiting
        );
        assert_eq!(
            peer_open_contract(true, share, stereo),
            PeerOpenContract::Ready(2)
        );
    }

    #[test]
    fn legacy_mono_is_chosen_from_its_message_not_from_a_timeout() {
        assert_eq!(
            peer_open_contract(true, PeerModeCell::Known(Mode::Share), capabilities(1)),
            PeerOpenContract::Ready(1)
        );
    }

    #[test]
    fn a_heard_non_provider_reaches_the_providers_authoritative_open_gate() {
        for mode in [Mode::A, Mode::B] {
            assert_eq!(
                peer_open_contract(true, PeerModeCell::Known(mode), capabilities(2)),
                PeerOpenContract::Ready(2)
            );
        }
    }

    #[test]
    fn unheard_contract_expires_but_a_real_legacy_mono_contract_does_not() {
        let deadline = Instant::now();
        assert!(!peer_contract_deadline_expired(
            true,
            PeerModeCell::Unheard,
            PeerAudioCapabilitiesCell::Unheard,
            deadline - Duration::from_millis(1),
            deadline,
        ));
        assert!(peer_contract_deadline_expired(
            true,
            PeerModeCell::Known(Mode::Share),
            PeerAudioCapabilitiesCell::Unheard,
            deadline,
            deadline,
        ));
        assert!(!peer_contract_deadline_expired(
            true,
            PeerModeCell::Known(Mode::Share),
            capabilities(1),
            deadline + Duration::from_secs(1),
            deadline,
        ));
    }
}

pub(crate) fn teardown_stream(inner: &DaemonInner, stream_id: u32, notify_remote: bool) {
    let entry = lk(&inner.state).sessions.remove(&stream_id);
    let Some(e) = entry else { return };
    if let Some(rx) = &e.rx {
        if let Some(spatial) = &rx.spatial {
            lk(spatial).stop();
        }
        wr(&inner.rx_table).remove(&stream_id);
        if let Some(name) = &rx.bridge {
            engine::release_bridge(inner, name);
        }
    }
    if e.tx.is_some() {
        if let Some(tx) = &e.tx { tx.revoke_media(); }
        let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
    }
    if notify_remote && e.conn.alive.load(Ordering::SeqCst) {
        let _ = e.conn.send_msg(&SessionMsg::CloseStream { stream_id });
    }
}

fn teardown_conn(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    conn.alive.store(false, Ordering::SeqCst);
    // Tier 2: the control channel and the media path are the same socket, so
    // this connection ending ends the mux — including when the end came from
    // this side (`drop_conn`, `ping_and_reap`, a mode change), where nothing on
    // the mux itself would ever notice. Idempotent, so the reader thread
    // reaching the same call is harmless.
    if let crate::tcpmedia::MediaPath::Framed(l) = conn.current_media_path() {
        l.kill();
    }
    for (_, tx) in lk(&conn.pending).drain() {
        let _ = tx.send(Err("connection closed".into()));
    }
    // capture what WE opened before the entries are dropped: that list is the
    // recovery plan (spec-m4c §C); peer-originated streams carry no origin and
    // are the peer's to re-open.
    //
    // `replay` 有值只说明「这是本机开的会话、这是它的参数」，**不等于**「该由
    // 通用重放机制救回来」：模式 B 的 `Hal` 会话归设备协调器管，它自己就会重开。
    // 两套机制都开一路 = 同一个 spec 的两条流、逐字节相同的载荷、对端 +6 dB 削顶。
    // 这个判断是 `reconnect::recoverable_by_replay`，理由写在那里。
    let mut mine: Vec<(u32, reconnect::PlannedSession)> = Vec::new();
    let ids: Vec<u32> = {
        let st = lk(&inner.state);
        st.sessions
            .iter()
            .filter(|(_, e)| Arc::ptr_eq(&e.conn, conn))
            .map(|(id, e)| {
                if let Some(o) = &e.replay {
                    if reconnect::recoverable_by_replay(e.origin) {
                        mine.push((
                            *id,
                            reconnect::PlannedSession {
                                params: (**o).clone(),
                                origin: e.origin,
                            },
                        ));
                    }
                }
                *id
            })
            .collect()
    };
    for id in ids {
        teardown_stream(inner, id, false);
    }
    let replaced = {
        let mut st = lk(&inner.state);
        if st
            .conns
            .get(&conn.fp)
            .map_or(false, |c| Arc::ptr_eq(c, conn))
        {
            st.conns.remove(&conn.fp);
        }
        // a newer conn to the same peer already took over
        st.conns
            .get(&conn.fp)
            .map_or(false, |c| c.alive.load(Ordering::SeqCst))
    };
    mine.sort_by_key(|(id, _)| *id); // deterministic replay order
    let mine: Vec<reconnect::PlannedSession> = mine.into_iter().map(|(_, p)| p).collect();
    if replaced {
        // The CONNECTION was replaced, the sessions were not: they were torn
        // down just now and nothing else re-opens them (register_conn inserts
        // the newcomer before this conn is marked dead, so this is the ordinary
        // path for a returning user, not a race). Arming with no delay lands on
        // the supervisor's next tick, where connect_peer finds the live conn and
        // replays the set on it.
        reconnect::arm_now(inner, &conn.fp, mine);
    } else {
        reconnect::arm(inner, &conn.fp, mine);
    }
}

/// Control-plane liveness, once per ticker second. A peer that vanishes without
/// an RST — cable pulled, VM paused, laptop lid — leaves the TCP looking
/// perfectly healthy until the kernel stops retransmitting, which is on the
/// order of 15 minutes; the peer stays `online:true` and never enters the
/// reconnect loop. Ping makes silence meaningful (every AudioHub daemon answers
/// one synchronously), so nothing but a dead path can stay quiet this long.
pub(crate) fn ping_and_reap(inner: &Arc<DaemonInner>) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let conns: Vec<Arc<ConnShared>> = lk(&inner.state).conns.values().cloned().collect();
    let t_us = inner.start.elapsed().as_micros() as u64;
    for c in conns {
        if !c.alive.load(Ordering::SeqCst) {
            continue;
        }
        let silent = c.silent_for();
        if silent >= CONTROL_SILENCE_LIMIT {
            dlog!(
                "[audiohubd] peer {}: no control traffic for {:.1}s, declaring the connection dead",
                c.fp,
                silent.as_secs_f64()
            );
            c.alive.store(false, Ordering::SeqCst);
            teardown_conn(inner, &c); // the reader thread's own teardown is then a no-op
            continue;
        }
        let _ = c.send_msg(&SessionMsg::Ping { t_us });
    }
}

/// Share-side endpoint snapshots for mode-B peers, independent of media
/// session lifetime. Called once per daemon ticker second so physical volume
/// and mute changes converge while every virtual device is idle.
pub(crate) fn poll_peer_device_volumes(inner: &Arc<DaemonInner>) {
    if haldev::effective_mode(inner) != Mode::Share || inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let conns: Vec<Arc<ConnShared>> = lk(&inner.state)
        .conns
        .values()
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    let conns: Vec<Arc<ConnShared>> = conns
        .into_iter()
        .filter(|conn| lk(&conn.peer_mode).mode() == Some(Mode::B))
        .filter(|conn| lk(&conn.peer_audio_capabilities).device_volume_version() >= 1)
        .collect();
    if conns.is_empty() {
        return;
    }

    for endpoint in [
        haldev::DeviceVolumeEndpoint::DefaultOutput,
        haldev::DeviceVolumeEndpoint::DefaultInput,
    ] {
        // See apply_peer_device_volume: platform reads/writes get a monotonic
        // endpoint revision. Sends remain outside so one stuck connection
        // cannot hold every other peer's physical volume control hostage.
        let mut ordered = match endpoint {
            haldev::DeviceVolumeEndpoint::DefaultOutput => lk(&inner.device_output_volume_io),
            haldev::DeviceVolumeEndpoint::DefaultInput => lk(&inner.device_input_volume_io),
        };
        let state = match endpoint {
            haldev::DeviceVolumeEndpoint::DefaultOutput => volume::get_default_output_volume(),
            haldev::DeviceVolumeEndpoint::DefaultInput => volume::get_default_input_volume(),
        };
        let Ok(state) = state else { continue };
        if !state.scalar.is_finite() {
            continue;
        }
        *ordered = (*ordered).wrapping_add(1).max(1);
        let revision = *ordered;
        drop(ordered);
        for conn in &conns {
            let _ = conn.send_msg(&SessionMsg::DeviceVolumeState {
                endpoint: endpoint.as_wire().to_string(),
                request_id: None,
                revision,
                scalar: state.scalar.clamp(0.0, 1.0),
                muted: state.muted,
                adjustable: state.adjustable,
                mute_adjustable: state.mute_adjustable,
            });
        }
    }
}

/// Tears down the control channel to `fp` on purpose (explicit disconnect, or a
/// connect that raced a disconnect). Callers disarm the retry loop FIRST, so
/// `teardown_conn` cannot re-arm what the user just asked us to stop.
pub(crate) fn drop_conn(inner: &Arc<DaemonInner>, fp: &str, why: &str) {
    let existing = lk(&inner.state).conns.get(fp).cloned();
    let Some(c) = existing else { return };
    dlog!("[audiohubd] peer {fp}: closing control channel ({why})");
    let _ = c.send_msg(&SessionMsg::Bye {});
    c.alive.store(false, Ordering::SeqCst);
    teardown_conn(inner, &c); // the reader thread's own teardown is then a no-op
}

/// Fingerprint prefix lookup across live conns, retry entries and the peer
/// store: `peers.disconnect` must still work for a peer that was just unpaired.
pub(crate) fn resolve_fingerprint(inner: &DaemonInner, selector: &str) -> Result<String> {
    let mut cands: Vec<String> = lk(&inner.state).conns.keys().cloned().collect();
    cands.extend(lk(&inner.recon).keys().cloned());
    if let Ok(s) = PeerStore::load_at(Some(&inner.cfg_dir)) {
        cands.extend(s.list().iter().map(|p| p.fingerprint.clone()));
    }
    cands.sort();
    cands.dedup();
    cands.retain(|f| f.starts_with(selector));
    match cands.len() {
        0 => bail!("no known peer matches '{selector}'"),
        1 => Ok(cands.remove(0)),
        n => bail!("'{selector}' is ambiguous ({n} peers)"),
    }
}

/// What [`retier`] did about a stored tier that just changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retier {
    /// No live control connection: the new tier is on disk and the next
    /// connection will negotiate with it.
    Stored,
    /// The control connection was dropped and the retry loop owns rebuilding
    /// it — typically back within the first backoff rung (~1 s), sessions
    /// replayed onto the new transport.
    Reconnecting,
    /// The control connection was dropped, but **this** daemon is not the side
    /// that dials this peer, so nothing here will rebuild it. The peer's own
    /// reconnect will, and the new tier applies then.
    AwaitingPeer,
}

impl Retier {
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Retier::Stored => "stored",
            Retier::Reconnecting => "reconnecting",
            Retier::AwaitingPeer => "awaiting_peer",
        }
    }
}

/// Apply a changed transport tier to a peer **without restarting the daemon**.
///
/// # Why this drops the control connection instead of attaching in place
///
/// `tcpmedia::negotiate` runs exactly once per control connection, from inside
/// `register_conn`, **before `conn_reader` starts** — and that is not an
/// accident of where it was put. It is synchronous precisely so that no stream
/// can be opened while the media path is being replaced; P3's review round
/// reproduced the alternative and found every stream in a ~200 ms window
/// pinned to UDP for its whole life, on links where UDP is the transport tier 1
/// exists because it cannot use. Attaching on a live connection would need a
/// second, differently-ordered version of that dance, and the failure it can
/// produce is silent by construction.
///
/// Dropping the connection re-enters the negotiated path instead. It is not
/// even expensive relative to what a tier change costs anyway: media never
/// switches transport inside a live stream (design §5.1), so **every stream has
/// to be re-opened regardless** — and `teardown_conn` already arms the retry
/// loop with exactly those streams. This is the same route `tcpmedia::serve`
/// takes when a media link dies, for the same reason.
pub(crate) fn retier(inner: &Arc<DaemonInner>, fp: &str) -> Retier {
    let live = lk(&inner.state)
        .conns
        .get(fp)
        .map_or(false, |c| c.alive.load(Ordering::SeqCst));
    if !live {
        return Retier::Stored;
    }
    drop_conn(inner, fp, "transport tier changed");
    // `teardown_conn` only arms peers we dial out to; for the rest there is no
    // entry and nothing here will bring the connection back.
    if lk(&inner.recon).contains_key(fp) {
        Retier::Reconnecting
    } else {
        Retier::AwaitingPeer
    }
}

pub(crate) fn disconnect_peer(inner: &Arc<DaemonInner>, selector: &str) -> Result<String> {
    let fp = resolve_fingerprint(inner, selector)?;
    reconnect::disarm(inner, &fp); // an explicit disconnect is not a failure
    drop_conn(inner, &fp, "explicit disconnect");
    Ok(fp)
}

/// `peers.pair`: the INITIATOR half of M3 pairing, run by the daemon.
///
/// It used to live in the CLI, which meant a pairing was a foreign process
/// writing `paired_peers.json` behind the daemon's back — fine when a pairing
/// only changed a list, wrong now that it has to make a pair of audio devices
/// appear. Doing it here means the device coordinator sees the new peer on its
/// very next pass.
///
/// `addr` is `host` or `host:port`; the bare form uses the frozen default port.
pub(crate) fn pair_with(inner: &Arc<DaemonInner>, addr: &str, pin: &str) -> Result<String> {
    let target = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_CONTROL_PORT}")
    };
    let sa = target
        .to_socket_addrs()
        .with_context(|| format!("resolve {target}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {target}"))?;
    // Same guard as connect_peer, one layer earlier: pairing with ourselves
    // would write a store record for our own fingerprint, which every later
    // dial and every verify would then have to treat as a stranger.
    if is_self_endpoint(inner, sa) {
        bail!("refusing to pair with {sa}: that is this daemon's own control endpoint");
    }
    let mut stream = TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT)
        .with_context(|| format!("connect {sa}"))?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    // We advertise the port WE listen on, which for a daemon is its real one —
    // the CLI had to guess the default here because it has no listener at all.
    let mut outcome = pair_initiator(&mut stream, pin, &inner.identity(), inner.control_port)?;
    outcome.peer.last_addr = Some(sa.ip().to_string());
    outcome.peer.port = sa.port();
    let fp = outcome.peer.fingerprint.clone();
    persist_peer(inner, outcome.peer)?;
    dlog!("[audiohubd] paired with {fp} at {sa}");
    Ok(fp)
}

// ---------------------------------------------------------------- outbound

/// True when `ip` names an interface of THIS host.
///
/// Asks the routing table rather than pattern-matching the text: a
/// `127.0.0.1` compare would miss `::1`, `localhost`, the machine's own LAN
/// address and a NAT hairpin, all of which land back here just as squarely.
/// The UDP socket sends nothing — `connect` on a datagram socket only picks the
/// route — and if the source address the kernel would use IS the target, the
/// target is one of ours.
fn ip_is_ours(ip: IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let bind: SocketAddr = if ip.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    UdpSocket::bind(bind)
        .and_then(|s| {
            s.connect(SocketAddr::new(ip, 9))?; // discard port; no datagram is sent
            s.local_addr()
        })
        .map_or(false, |local| local.ip() == ip)
}

/// True when `addr` is this daemon's own control endpoint.
///
/// The cheap half of the self-dial guard (`SelfConnection` from the handshake
/// is the authoritative half): it costs no TCP connection and catches the case
/// that actually occurred — a peer record whose address and port resolved to
/// the very socket we accept on, so the session coordinator dialled us, we
/// answered our own `VerifyHello` with `Unpaired`, and we deleted the pairing
/// on our own say-so.
fn is_self_endpoint(inner: &DaemonInner, addr: SocketAddr) -> bool {
    addr.port() == inner.control_port && ip_is_ours(addr.ip())
}

fn resolve_peer(store: &PeerStore, selector: &str) -> Result<PairedPeer> {
    let matches: Vec<&PairedPeer> = store
        .list()
        .iter()
        .filter(|p| p.fingerprint.starts_with(selector))
        .collect();
    match matches.len() {
        0 => bail!("no paired peer matches '{selector}'"),
        1 => Ok(matches[0].clone()),
        n => bail!("'{selector}' is ambiguous ({n} peers)"),
    }
}

fn target_addr(peer: &PairedPeer, addr_override: Option<&str>) -> Result<SocketAddr> {
    let s = match addr_override {
        Some(a) if a.contains(':') => a.to_string(),
        Some(a) => format!("{}:{}", a, peer.port),
        None => {
            let ip = peer
                .last_addr
                .as_deref()
                .ok_or_else(|| anyhow!("no known address for {} (pass addr)", peer.fingerprint))?;
            // Port 0 is how the store records "this peer never told us a port we
            // could believe" (see the PairInit arm of handle_inbound). Dialling
            // it is meaningless, and the message has to say so: the peer is
            // still perfectly usable — it just has to be the one to dial, or be
            // given an explicit address.
            if peer.port == 0 {
                bail!(
                    "no reachable port recorded for {} (it paired without advertising one); \
                     pass addr as host:port, or let it connect to us",
                    peer.fingerprint
                );
            }
            format!("{}:{}", ip, peer.port)
        }
    };
    resolve(&s)
}

fn resolve(s: &str) -> Result<SocketAddr> {
    s.to_socket_addrs()
        .with_context(|| format!("resolve {s}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {s}"))
}

/// Who asked for this outbound connect. Only `User` makes the peer eligible for
/// the retry loop — the retry path itself must never (re-)create the entry, or
/// a connect racing an explicit disconnect would resurrect it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ConnectOrigin {
    User,
    Retry,
}

/// Kills a freshly dialled tier 2 mux unless something took ownership of it.
///
/// Between the dial and `register_conn` there are seven ways out of
/// `connect_peer` — an unpaired refusal, a fingerprint mismatch, a failed
/// `SecureChannel`, three `?`s and a `bail!` — and a mux abandoned on any of
/// them leaves **two threads and a socket** running for the life of the daemon,
/// once per attempt, on a path the retry loop walks every few seconds. Naming
/// all seven by hand is how one gets missed; the same argument
/// `tcpmedia::AttachClaim` makes about its own half-dozen exits.
struct MuxOnTrial(Option<Arc<crate::mux::MuxLink>>);

impl MuxOnTrial {
    /// The mux now belongs to somebody else. Nothing to kill on the way out.
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for MuxOnTrial {
    fn drop(&mut self) {
        if let Some(l) = &self.0 {
            l.kill();
        }
    }
}

pub(crate) fn connect_peer(
    inner: &Arc<DaemonInner>,
    selector: &str,
    addr_override: Option<&str>,
    origin: ConnectOrigin,
) -> Result<PairedPeer> {
    let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
    let peer = resolve_peer(&store, selector)?;
    // A user-initiated connect SUPERSEDES an armed retry, so it inherits that
    // retry's recovery plan: note_outbound hands the sessions back and they are
    // replayed here, exactly as the retry loop would have. Clearing the timer
    // and leaving them stored was a silent, permanent loss — the 30s rung
    // recurs forever, so the window a manual connect lands in never closes.
    let note = |fp: &str| {
        if origin == ConnectOrigin::User {
            let sessions = reconnect::note_outbound(inner, fp, addr_override);
            reconnect::spawn_replay(inner, fp, sessions);
        }
    };
    {
        let st = lk(&inner.state);
        if let Some(c) = st.conns.get(&peer.fingerprint) {
            if c.alive.load(Ordering::SeqCst) {
                drop(st);
                note(&peer.fingerprint);
                return Ok(peer);
            }
        }
    }
    // plan §16 / design §4.2 item 1: some tunnels only let one side originate a
    // connection, and this peer is on the far side of one. Dialling would fail
    // in a way indistinguishable from a machine that is switched off, so the
    // refusal names the real reason and the retry loop never arms (see
    // `reconnect::may_dial`). The peer is not offline — it is expected to
    // arrive, which is a third state and is reported as one.
    if lk(&inner.peer_transport).dial_policy(&peer.fingerprint) == DialPolicy::InboundOnly {
        bail!(
            "{} is set to inbound-only: this machine cannot originate a connection to it \
             (the tunnel only carries connections the other way), so it has to connect to us",
            peer.fingerprint
        );
    }
    // P6: a URL-shaped address both *is* the address and *selects* the carrier
    // (plan §16.2). Taken before `target_addr` because it answers the same
    // question — where do we dial — from a source `paired_peers.json` cannot
    // hold (see `peer_transport::PeerTransport::endpoint`).
    let endpoint = match addr_override {
        // An explicit URL on the call wins, and is not persisted here: the
        // caller asked for one connection, not for a setting.
        Some(a) if crate::wsshell::WsUrl::looks_like_url(a) => {
            Some(crate::wsshell::WsUrl::parse(a)?)
        }
        Some(_) => None,
        None => lk(&inner.peer_transport).endpoint(&peer.fingerprint),
    };
    if let Some(u) = &endpoint {
        u.require_plaintext()?;
    }
    let addr = match &endpoint {
        Some(u) => resolve(&u.authority())?,
        None => target_addr(&peer, addr_override)?,
    };
    // Never dial ourselves. Measured on 2026-07-31: a peer whose record pointed
    // at this daemon's own control endpoint made the session coordinator open a
    // TCP to us, we answered our own VerifyHello with `Unpaired` (our own
    // fingerprint is not in our own store, and never can be) and the initiator
    // half then deleted the pairing — the daemon told itself the trust was dead
    // and destroyed it. Refused before the connect so no handshake, no
    // reconnect rung and no store write can come of it.
    if is_self_endpoint(inner, addr) {
        bail!(
            "refusing to dial {addr} for {}: that is this daemon's own control endpoint (the \
             peer's recorded address is wrong — it must dial us, or be given the right one)",
            peer.fingerprint
        );
    }
    // M8 tier 2: one connection for everything, and the handshake below travels
    // inside it. Decided here, from the stored tier, because the transport has
    // to be chosen before the first byte — plan §16.2 makes tier 2 manual
    // precisely because nothing observable distinguishes "the tunnel is L7
    // only" from "the peer is off", so there is nothing to probe for.
    // A URL is a tier 2 request on its own — it is the only thing the user can
    // say that means "this peer is behind an application-layer tunnel", and
    // requiring a second setting to agree with it would only create a state
    // where the two disagree.
    let tier2 = endpoint.is_some()
        || lk(&inner.peer_transport).tier(&peer.fingerprint) == TransportTier::Tier2;
    let (mut trial, mut stream) = if tier2 {
        let (link, io) = crate::mux::dial(inner, addr, endpoint.as_ref())?;
        (MuxOnTrial(Some(link)), io)
    } else {
        let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .with_context(|| format!("connect {addr}"))?;
        let _ = s.set_nodelay(true);
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        s.set_write_timeout(Some(WRITE_TIMEOUT))?;
        (MuxOnTrial(None), s.into())
    };
    let verified = match verify_initiator(&mut stream, &inner.identity(), &store) {
        Ok(v) => v,
        Err(e) => {
            // The peer has removed us. Keeping the pairing would leave a pair
            // of virtual devices in this machine's system list bearing that
            // peer's name — permanently offline, permanently silent — plus a
            // reconnect loop dialling a machine that has blacklisted us
            // (spec-m5b OPEN QUESTION 5).
            //
            // Two things must hold before a trust relationship is deleted over
            // a frame that arrives before any key exchange. `verify_initiator`
            // checked the first: the refusal is signed by the key we have on
            // file for the fingerprint that signed it. This checks the second:
            // that fingerprint is the peer we set out to reach. Without it a
            // refusal legitimately signed by peer X, replayed or misdirected on
            // the connection we opened toward peer Y, would delete Y.
            let signed_by = e
                .downcast_ref::<UnpairedByPeer>()
                .map(|u| u.fingerprint.clone());
            match signed_by {
                Some(fp) if fp == peer.fingerprint => {
                    dlog!(
                        "[audiohubd] peer {} has unpaired from us (signed refusal); dropping our \
                         side too",
                        peer.fingerprint
                    );
                    forget_peer(inner, &peer.fingerprint);
                }
                Some(fp) => dlog!(
                    "[audiohubd] {addr} refused us as unpaired signed by {fp}, but we dialled {}; \
                     keeping the pairing",
                    peer.fingerprint
                ),
                // Unsigned, or signed by a key we cannot tie to that peer. The
                // refusal proves nothing, so this is just a failed connection.
                None if was_unpaired_by_peer(&e) => dlog!(
                    "[audiohubd] {addr} refused us as unpaired without proof; keeping the pairing \
                     with {}",
                    peer.fingerprint
                ),
                // The handshake came back with our own key: whatever the
                // address said, this connection is a loop. Never a pairing's
                // fault, so nothing is touched.
                None if was_self_connection(&e) => dlog!(
                    "[audiohubd] refusing the connection to {addr} for {}: it came back to this \
                     daemon ({e:#})",
                    peer.fingerprint
                ),
                None => {}
            }
            return Err(e);
        }
    };
    if verified.fingerprint != peer.fingerprint {
        bail!(
            "peer at {addr} is {} (expected {})",
            verified.fingerprint,
            peer.fingerprint
        );
    }
    let chan = SecureChannel::establish_initiator(stream, &inner.identity(), &verified)?;
    // we opened this TCP, so our own fingerprint is the tie-break key
    let initiator_fp = inner.identity().fingerprint.clone();
    let link = trial.0.clone();
    // From here the mux's fate belongs to `register_conn` and the reader thread
    // below, both of which handle it explicitly.
    trial.keep();
    match register_conn(inner, chan, addr.ip(), initiator_fp, link.clone()) {
        Some(conn) => {
            let i = inner.clone();
            let c = conn.clone();
            let reader_link = link.clone();
            let spawned = std::thread::Builder::new()
                .name("ahb-conn".into())
                .spawn(move || {
                    // spec §8: a panic on one conn must not reach the daemon
                    if std::panic::catch_unwind(AssertUnwindSafe(|| conn_reader(&i, &c))).is_err() {
                        dlog!("[audiohubd] control conn {}: panicked, dropped", c.fp);
                        c.alive.store(false, Ordering::SeqCst);
                    }
                    // On tier 2 the control channel and the media path are one
                    // connection, so the reader ending ends both. Without this
                    // the mux's own two threads would outlive the conn they
                    // serve and sit on a socket nobody reads.
                    if let Some(l) = &reader_link {
                        l.kill();
                    }
                });
            if let Err(err) = spawned {
                // Registration already published this exact Arc. A failed
                // reader spawn must retire it now; otherwise it remains alive,
                // answers every online lookup, and can never consume the peer's
                // mode/capability contract or any stream response.
                teardown_conn(inner, &conn);
                return Err(err).context("spawn control connection reader");
            }
        }
        // the peer connected to us at the same moment and its conn won: it is
        // already registered under this fingerprint, so callers find it
        None => {
            note(&verified.fingerprint);
            return Ok(verified);
        }
    }
    // remember the address that just worked
    {
        let _g = lk(&inner.store_lock);
        if let Ok(mut s) = PeerStore::load_at(Some(&inner.cfg_dir)) {
            let mut p = verified.clone();
            p.last_addr = Some(addr.ip().to_string());
            s.upsert(p);
            let _ = s.save();
        }
    }
    note(&verified.fingerprint);
    Ok(verified)
}

// ---------------------------------------------------------------- sessions

fn alloc_stream_id(inner: &DaemonInner) -> u32 {
    use rand_core::RngCore;
    loop {
        let id = rand_core::OsRng.next_u32();
        if id == 0 || rd(&inner.rx_table).contains_key(&id) {
            continue;
        }
        let state = lk(&inner.state);
        if state.sessions.contains_key(&id) || state.stream_claims.contains_key(&id) {
            continue;
        }
        return id;
    }
}

/// plan §15：这条流上**要推给对端**的那半边档位。
///
/// # 每一端推的是「执行器在对面」的那个旋钮
///
/// 两个档位的执行器在相反的端上（延迟 = 接收侧的 jitter buffer，质量 =
/// 发送侧的阶梯格号），所以消费者设的四个值里跨到线上的是**交叉的一半**：
///
/// | 本机开的这条流 | 对端手上有 | 推过去的 |
/// |---|---|---|
/// | `consuming`（mic：本机收） | `tx` | `tx_quality` = 本机的 `recv.quality` |
/// | 否（spk：本机发） | `rx` | `rx_latency` = 本机的 `send.latency` |
///
/// 另一半（`recv.latency` / `send.quality`）的执行器就在本机，由
/// `publish_targets` 直接灌进本机那条流的原子量，**不上线**。
///
/// 照 plan §15 裁定 3 的字面「把 `send.*` 整组推过去」会得到：`send.quality`
/// 推到对端后无处执行（那条流在对端是 `dir=recv`，没有 `tx`），而本机的
/// `tx.rung` 没人写 ⇒ 拖「发送音质」滑条线上采样率纹丝不动。设了、存了、
/// 回显了、媒体面一个字节没变——本项目栽过六次的那个形状。
fn wire_transport(
    inner: &DaemonInner,
    fp: &str,
    consuming: bool,
) -> (Option<String>, Option<String>) {
    let t = lk(&inner.peer_transport).get(fp);
    if consuming {
        (None, Some(t.recv.quality_target().as_wire()))
    } else {
        (Some(t.send.latency_target().as_wire()), None)
    }
}

/// 档位变了：把交叉的那半边推给对端**已经在跑的**每一条流。
///
/// 走增量消息而不是重开流：重开 = 新 `stream_id` + 新 media salt + JB 重建 +
/// 重新预缓冲，听感上是一次明显的断续，而「修改即生效」是这条路冻结的形状。
///
/// 只推**本机开的**会话（`origin != Peer`）：本机是提供者的那些流上，档位由
/// 对端决定，我们没有资格发言。
pub(crate) fn push_transport(inner: &Arc<DaemonInner>, fp: &str) {
    for e in crate::snapshot_sessions(inner) {
        if e.conn.fp != fp || e.origin == SessionOrigin::Peer {
            continue;
        }
        let (rx_latency, tx_quality) = wire_transport(inner, fp, e.rx.is_some());
        let _ = e.conn.send_msg(&SessionMsg::SetTransport {
            stream_id: e.id,
            rx_latency,
            tx_quality,
        });
    }
}

/// Why a stream is being opened — orthogonal to [`SessionOrigin`], which says
/// *whose* session it is and survives a replay unchanged.
///
/// It exists for plan §7.1's one-shot mute, and that is the whole of its job.
/// §7.1 fires 「静音本机」 at 「与对端建立连接的那一刻」 and then respects a later
/// unmute as "this machine should sound too". A reconnect replay is not that
/// moment in any sense the user would recognise: the network blipped, or a tier
/// changed under them, and the mute they deliberately cancelled comes back.
/// `SessionOrigin` cannot answer this — a replayed user session is still a user
/// session — so the cause is carried separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCause {
    /// A person, or the mode-B device coordinator, asked for this stream now.
    Fresh,
    /// The reconnect loop is restoring a stream that already existed.
    Replay,
}

/// IPC and CLI entry point. Everything opened this way is a USER session: the
/// device coordinator may never close one (spec-m5b §5.6).
pub(crate) fn open_session(
    inner: &Arc<DaemonInner>,
    params: &OpenSessionParams,
) -> Result<SessionInfo> {
    open_session_from(inner, params, SessionOrigin::User, OpenCause::Fresh, None)
}

pub(crate) fn open_session_from(
    inner: &Arc<DaemonInner>,
    params: &OpenSessionParams,
    origin: SessionOrigin,
    cause: OpenCause,
    spatial_layout: Option<audiohub_core::spatial_output::SpeakerLayout>,
) -> Result<SessionInfo> {
    if spatial_layout.is_some() && (origin.slot().is_none() || params.kind != KIND_SPK
        || params.source.as_deref() != Some(SOURCE_HAL_SPEAKER)
        || haldev::effective_mode(inner) != Mode::B)
    {
        bail!("spatial PCM transmission requires a mode-B virtual speaker source");
    }
    if params.kind != KIND_MIC && params.kind != KIND_SPK {
        bail!("kind must be '{KIND_MIC}' or '{KIND_SPK}'");
    }
    if params.source.as_deref() == Some("airplay") {
        bail!("AirPlay 音频始终在接收端本机播放，不能作为 AudioHub peer source");
    }
    let consuming = params.kind == KIND_MIC; // media flows peer -> us
                                             // Volume sync is a spk-only property: it drives the output device of
                                             // whoever PLAYS the stream, and on a mic stream that is us, not the peer.
    let vol_sync = params.volume_sync && params.kind == KIND_SPK;
    if params.volume_sync && !vol_sync {
        dlog!("[audiohubd] volume_sync ignored: only spk sessions carry it");
    }
    // spec-m4c §B: the bridge renders the PEER's audio into a named device on
    // THIS machine, which only exists in the mic direction. An empty selector
    // is a UI "no bridge", not a device named "".
    let bridge = params
        .bridge
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if bridge.is_some() && !consuming {
        bail!("bridge applies to a '{KIND_MIC}' session only (it renders the peer's audio here)");
    }
    // spec-round2 §B2, mic direction: the same "the peer's audio is rendered
    // HERE" rule as the bridge, into the HAL mic ring instead of a named card.
    if params.hal && !consuming {
        bail!("hal applies to a '{KIND_MIC}' session only (it feeds the virtual microphone here)");
    }
    // Refused up front rather than accepted-and-silent: an operator who asked
    // for the virtual microphone and got a session that feeds nothing has no
    // way to tell from the session list.
    //
    // BEFORE the slot lookup below, and deliberately: "there is no bridge on
    // this machine" and "this peer has no device" are different problems with
    // different fixes, and a host with no driver at all must be told the first
    // one — the slot could never exist there whatever the peer or the mode.
    let wants_hal =
        params.hal || params.source.as_deref() == Some(audiohub_ipc::SOURCE_HAL_SPEAKER);
    if wants_hal && inner.hal().is_none() {
        bail!(
            "the macOS HAL bridge is not available (no LaunchDaemon holding '{}', or \
             AUDIOHUB_HAL_BRIDGE=off)",
            crate::halbridge::HAL_SERVICE_NAME
        );
    }
    // auto-connect if offline; a locally opened session is by definition one we
    // originated, so the peer joins the retry set
    let peer = connect_peer(inner, &params.peer, None, ConnectOrigin::User)?;
    // Resolved from the FINGERPRINT, after the selector has been resolved to a
    // real peer: `hal` and `halspk` name a peer, never a slot (spec-m5b §5.6).
    let peer_slot = lk(&inner.haldev).slot_of(&peer.fingerprint);
    let spec = if consuming {
        None
    } else {
        // validate (and resolve the sysaudio backend) before opening
        Some(source_spec(
            params.source.as_deref(),
            params.freq,
            params.backend.as_deref(),
            peer_slot,
        )?)
    };
    // The CONCRETE backend id, captured before `spec` is consumed below. plan
    // §7.1's 「静音本机」 precondition is a property of the backend that actually
    // got picked, and `"auto"` is a different backend on every host.
    let capture_backend = match &spec {
        Some(SourceSpec::SysAudio { backend }) => Some(backend.clone()),
        _ => None,
    };
    let hal_slot = if params.hal {
        Some(peer_slot.ok_or_else(|| {
            anyhow!(
                "no virtual microphone is bound to {} (mode B is not in force, the driver is \
                 absent, or the slot pool is full)",
                peer.fingerprint
            )
        })?)
    } else {
        None
    };
    let conn = lk(&inner.state)
        .conns
        .get(&peer.fingerprint)
        .cloned()
        .ok_or_else(|| anyhow!("no live connection to {}", peer.fingerprint))?;

    // Missing capability means a 1.0.0 peer and therefore mono. New peers use
    // stereo only for sources which carry spatial information; this also keeps
    // microphone/tone bandwidth identical to 1.0.0.
    let desired_channels = spec
        .as_ref()
        .map(SourceSpec::preferred_channels)
        .unwrap_or_else(|| preferred_source_channels(params.source.as_deref()));
    let peer_channels = peer_media_channels_for_open(inner, &conn)?;
    let spatial_contract = spatial_layout.map(|layout| {
        if peer_slot != origin.slot() {
            bail!("spatial source slot does not belong to the selected peer");
        }
        lk(&conn.peer_spatial_output).as_ref()
            .and_then(|offer| offer.contracts.iter().find(|contract| contract.layout == layout))
            .cloned().ok_or_else(|| anyhow!("peer has no current contract for the selected spatial layout"))
    }).transpose()?;
    if spatial_contract.is_some() && matches!(lk(&inner.peer_transport).get(&conn.fp).send.quality_target(), audiohub_ipc::QualityTarget::Fixed(rung) if rung != 0) {
        bail!("the selected quality cannot satisfy the 48 kHz/F32 spatial PCM contract");
    }
    let media_channels = spatial_contract.as_ref().map_or(desired_channels.min(peer_channels).clamp(1, 2), |c| c.channels());
    let media_frame_tag_version = lk(&conn.peer_audio_capabilities).media_frame_tag_version();

    let stream_id = alloc_stream_id(inner);
    // the same caps a remote opener is held to (B7): locally driven opens must
    // not be the way to starve the 10ms loops either
    let reservation = claim_stream_id(inner, &conn, stream_id)?;
    // Opened before any stream state exists (every later failure path runs
    // `unwind`), and never resolved to the default output: a bridge that
    // silently played out of the speakers would look like it worked while no
    // app could pick the audio up. From here on the RESOLVED name is the only
    // one used, so releasing frees exactly what opening took.
    let bridge = match &bridge {
        Some(name) => Some(engine::open_bridge(inner, name)?),
        None => None,
    };
    // we are the opener, so we mint this stream's media salt
    let salt = gen_media_salt();
    let (ptx, prx) = mpsc::channel();
    lk(&conn.pending).insert(stream_id, ptx);

    // ONE read of the path for this session, whichever half of it gets built:
    // the stream is bound to it and the session reports its tier, and two reads
    // straddling an attach would leave a stream saying it travels somewhere it
    // does not.
    let path = conn.current_media_path();
    let media_tier = path.tier_wire();

    // register the receive side before OpenStream goes out so no early media
    // is dropped once the provider accepts
    let rx_arc = if consuming {
        let rx = Arc::new(
            RxStream::new(
                stream_id,
                &conn.rx_key,
                &salt,
                params.verify_freq,
                false,
                params.monitor,
                bridge.clone(),
                hal_slot,
                path.clone(),
                media_channels,
            )
            .with_media_frame_tag_version(media_frame_tag_version),
        );
        wr(&inner.rx_table).insert(stream_id, rx.clone());
        Some(rx)
    } else {
        None
    };
    let unwind = |inner: &DaemonInner, conn: &ConnShared| {
        lk(&conn.pending).remove(&stream_id);
        if consuming {
            wr(&inner.rx_table).remove(&stream_id);
        } else {
            // The send side is brought up BEFORE `OpenStream` goes out (see
            // below), so every failure path from here on has a media source to
            // take back down. Unknown ids are a no-op in `TxState::remove_stream`,
            // which is what makes this safe on the paths that failed earlier.
            let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
        }
        if let Some(name) = &bridge {
            engine::release_bridge(inner, name);
        }
    };

    // plan §16.2 的自动降级判定是**接收侧**的：对端在收到 `OpenStream` 的那一刻
    // 就给这条流上膛（`handle_remote_open` 的 `DIR_SEND` 臂），此后 600 ms 没有
    // 任何媒体数据报到达就是一条「这条链路过不去 UDP」的证据。
    //
    // 所以源必须在**发出这条消息之前**跑起来。建源本身可以慢：`start_tx_stream`
    // 等的是 `SOURCE_ACK_TIMEOUT`（5 s），而 mac 上的 CATap 要建进程 tap + 聚合
    // 设备，首次还要走一遍 TCC 授权——把它放在 accept 之后，对端的表从 0 开始
    // 跑而我们最多 5 s 之后才发第一个包，判定在 1 s 出结果，代价是一条好链路被
    // **落盘一小时**的降级钉在 TCP 上。
    //
    // 这也正是 `handle_remote_open` 早就守着的纪律（源建不出来就不 accept，
    // 而不是 accept 一条永远没有声音的流）；两个开流方向从此同一条规矩。
    //
    // 「跑起来」到此为止是**源**跑起来，不是数据报出门：`armed = false` 让这条流
    // 在对端 accept 之前一个包都不发。那段时间里包无处可去（对端还没有接收流，
    // 收到也只能丢），而发了就会记进本机的 payload 计数、永远记不进对端的——
    // 两个本该逐字相等的计数器之间从此挂着一段固定的差
    // （`the_two_ends_agree_on_what_a_wire_byte_is` 盯的正是这个差）。
    let mut tx_shared: Option<Arc<TxShared>> = None;
    if !consuming {
        let mut shared = TxShared::new_on(path.auto_top_rung());
        if let Some(contract) = spatial_contract.clone() {
            shared = match shared.with_spatial_contract(contract) {
                Ok(shared) => shared,
                Err(error) => { unwind(inner, &conn); return Err(error); }
            };
            shared.transport.publish_quality(lk(&inner.peer_transport).get(&conn.fp).send.quality_target());
        }
        let shared = Arc::new(shared);
        shared.armed.store(false, Ordering::SeqCst);
        if let Err(e) = start_tx_stream(
            inner,
            stream_id,
            conn.tx_key,
            salt.to_vec(),
            path,
            spec.expect("validated above"),
            media_channels,
            params.simulate_loss_pct.unwrap_or(0.0),
            shared.clone(),
        ) {
            unwind(inner, &conn);
            return Err(e.context("start media source"));
        }
        tx_shared = Some(shared);
    }

    // plan §15：档位在**发这条消息的这一刻**从 `inner.peer_transport` 现读，
    // **不进 `OpenSessionParams`**。
    //
    // 进了参数就会被 `SessionEntry.replay` 冻结，而重放是断线重连唯一的开流
    // 路径——于是「用户在断线期间改的档位会被静默还原成断线前的值」。三个
    // 开流入口（UI / 模式 B 设备协调器 / 断线重放）因此天然一致。
    let (rx_latency, tx_quality) = wire_transport(inner, &conn.fp, consuming);
    let open = if let Some(contract) = spatial_contract.clone() {
        SessionMsg::OpenSpatialStream { stream_id, media_salt_b64: BASE64_STANDARD.encode(salt), contract, rx_latency }
    } else { SessionMsg::OpenStream {
        stream_id,
        kind: params.kind.clone(),
        dir: if consuming { DIR_RECV } else { DIR_SEND }.to_string(),
        sample_rate: 48000,
        channels: media_channels,
        media_salt_b64: BASE64_STANDARD.encode(salt),
        verify_freq: params.verify_freq,
        source: params.source.clone(),
        freq: params.freq,
        backend: params.backend.clone(),
        simulate_loss_pct: params.simulate_loss_pct,
        volume_sync: vol_sync,
        rx_latency,
        tx_quality,
    }};
    if let Err(e) = conn.send_msg(&open) {
        unwind(inner, &conn);
        return Err(e.context("send OpenStream"));
    }
    match prx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            unwind(inner, &conn);
            bail!("peer rejected stream: {reason}");
        }
        Err(_) => {
            unwind(inner, &conn);
            bail!("open timed out after {OPEN_TIMEOUT:?}");
        }
    }
    if spatial_contract.as_ref().is_some_and(|contract| {
        !lk(&conn.peer_spatial_output).as_ref().is_some_and(|offer| offer.contracts.contains(contract))
    }) {
        unwind(inner, &conn);
        let _ = conn.send_msg(&SessionMsg::CloseStream { stream_id });
        bail!("provider spatial contract changed while the stream was opening");
    }
    // exactly what a reconnect replays; the fresh stream id and media salt are
    // minted by this function, not carried here (spec-m4c §C)
    let replay = Some(Arc::new(params.clone()));
    let entry = if consuming {
        SessionEntry {
            id: stream_id,
            conn: conn.clone(),
            kind: params.kind.clone(),
            dir: DIR_RECV.to_string(),
            source: params.source.clone(),
            rx: rx_arc,
            tx: None,
            volume: Arc::new(VolumeCell::new(false)), // mic: no remote output
            replay,
            origin,
            peer_lat: Arc::new(PeerLatCell::new()),
            pushed: Arc::new(crate::PushedTransport::default()),
            armed_conn_ms: conn.clock_ms(),
            media_tier,
        }
    } else {
        SessionEntry {
            id: stream_id,
            conn: conn.clone(),
            kind: params.kind.clone(),
            dir: DIR_SEND.to_string(),
            source: params.source.clone(),
            rx: None,
            // Built above, before `OpenStream` went out.
            tx: Some(tx_shared.expect("the send side is built before OpenStream")),
            // consumer of a spk stream: our slider drives the PEER's output
            volume: Arc::new(VolumeCell::new(vol_sync)),
            replay,
            origin,
            peer_lat: Arc::new(PeerLatCell::new()),
            pushed: Arc::new(crate::PushedTransport::default()),
            armed_conn_ms: conn.clock_ms(),
            media_tier,
        }
    };
    // Liveness is re-checked under the SAME lock that inserts. A peer that
    // answers AcceptStream and then closes the TCP is the DEFAULT outcome, not
    // a lucky interleaving: SecureChannel reads EOF as an error, so its
    // teardown runs microseconds after notify_pending while this thread is
    // still opening the device. An entry inserted after that teardown is never
    // torn down again — it holds the microphone open and blasts media forever.
    let inserted = {
        let mut st = lk(&inner.state);
        let live = conn.alive.load(Ordering::SeqCst)
            && st
                .conns
                .get(&conn.fp)
                .map_or(false, |c| Arc::ptr_eq(c, &conn))
            && spatial_contract.as_ref().is_none_or(|contract| {
                lk(&conn.peer_spatial_output).as_ref().is_some_and(|offer| offer.contracts.contains(contract))
            });
        live && reservation.publish(&mut st, entry.clone()).is_ok()
    };
    if !inserted {
        // `unwind` takes the media source down too, so nothing extra is needed
        // here — and the peer, which did accept, is told by the CloseStream the
        // teardown of this control channel already implies.
        unwind(inner, &conn);
        bail!(
            "control channel to {} closed while the stream was being opened",
            conn.fp
        );
    }
    // Restore a fixed peer output's persistent virtual-device gain BEFORE the
    // first media packet is armed. Otherwise a knob set while idle would play
    // at unity until the first per-session volume report (up to one second).
    if !consuming && params.kind == KIND_SPK && vol_sync {
        if let Some(desired) = haldev::peer_software_gain_authority(inner, &conn.fp) {
            sync_peer_software_gain(inner, &conn.fp, desired);
        }
    }
    // The peer accepted and the local entry now exists with its initial gain
    // installed, so media can leave. Keeping `armed=false` through the setup
    // above preserves the invariant that no full-volume prefix escapes.
    if let Some(shared) = &entry.tx {
        if shared.spatial.is_some() {
            shared.transport.publish_quality(lk(&inner.peer_transport).get(&conn.fp).send.quality_target());
            if shared.transport.quality_rung().is_some_and(|rung| rung != 0) {
                teardown_stream(inner, stream_id, true);
                bail!("quality target changed before the spatial stream could be armed");
            }
        }
        if shared.media_failed.load(Ordering::Acquire) {
            teardown_stream(inner, stream_id, true);
            bail!("media contract failed before the stream could be armed");
        }
        shared.armed.store(true, Ordering::SeqCst);
    }
    // plan §7.1 「静音本机」: the stream is established exactly here — the peer
    // accepted, the source is running and the entry is in the table. Firing it
    // any earlier would mute this machine for a session that then failed to
    // open, leaving it silent with nothing to point at.
    //
    // Speaker direction only: the mute exists so this machine and the peer do
    // not both play the mirror, and a mic session mirrors nothing.
    //
    // And never on a replay. §7.1 reads a manual unmute as "this machine should
    // sound too" and respects it 「直到下一次连接建立」 — a reconnect after a
    // network blip, an automatic tier downgrade or a tier the user just changed
    // is not a next connection in any sense they would recognise, and re-muting
    // there is this switch overruling them from a place they cannot see.
    if !consuming && params.kind == KIND_SPK && cause == OpenCause::Fresh {
        mute_local_on_connect(inner, capture_backend.as_deref());
    }
    Ok(build_session_info(inner, &entry, &[], None, None))
}

/// Removes every trace of a peer this daemon is no longer paired with: the
/// store entry, the retry loop, the live control channel, its sessions and its
/// virtual devices.
///
/// Order matters. The connection goes LAST, because dropping it re-arms the
/// retry loop from `teardown_conn` unless the entry is already gone.
pub(crate) fn forget_peer(inner: &Arc<DaemonInner>, fp: &str) {
    {
        let _g = lk(&inner.store_lock);
        if let Ok(mut s) = PeerStore::load_at(Some(&inner.cfg_dir)) {
            if s.remove_by_fingerprint(fp) {
                let _ = s.save();
            }
        }
    }
    // plan §15：这台对端的四个档位一并清掉。留着的话，重新配对同一台机器会
    // **静默继承**上一段关系的档位——「我明明没设过 300」的又一种成因。
    {
        let mut t = lk(&inner.peer_transport);
        if t.remove(fp) {
            let _ = t.save(&inner.cfg_dir);
        }
    }
    reconnect::disarm(inner, fp);
    haldev::release_peer(inner, fp);
    // Tell it, if we still can: the peer that never dials us would otherwise
    // never learn, and its copy of our devices would outlive the pairing.
    if let Some(c) = lk(&inner.state).conns.get(fp).cloned() {
        let _ = c.send_msg(&SessionMsg::Unpaired {});
    }
    drop_conn(inner, fp, "unpaired");
}

pub(crate) fn close_session(inner: &Arc<DaemonInner>, id: u32) -> Result<()> {
    if !lk(&inner.state).sessions.contains_key(&id) {
        bail!("unknown session {id}");
    }
    teardown_stream(inner, id, true);
    Ok(())
}
