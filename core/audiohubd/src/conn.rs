//! Control plane: TCP accept + first-frame dispatch (VerifyHello -> M3 verify
//! -> SecureChannel -> SessionMsg loop; PairInit -> pair_responder when
//! pairing mode is active), outbound connects, and session open/close flows.

use std::collections::HashMap;
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
    OpenSessionParams, SessionInfo, KIND_MIC, KIND_SPK, SOURCE_HAL_SPEAKER, SOURCE_MIC,
    SOURCE_SYSAUDIO, SOURCE_TONE,
};
use audiohub_net::control::{write_frame, ControlMsg, CONTROL_MAX_FRAME};
use audiohub_net::identity::{PairedPeer, PeerStore};
use audiohub_net::pairing::{
    pair_initiator, pair_responder, verify_initiator, verify_responder, was_self_connection,
    was_unpaired_by_peer, UnpairedByPeer,
};
use audiohub_net::secure::{SecureChannel, SessionMsg};

use crate::engine::{self, SourceSpec, TxCmd};
use crate::{
    build_session_info, dlog, gen_media_salt, haldev, lk, rd, reconnect, wr, ConnShared,
    DaemonInner, DaemonState, RxStream, SessionEntry, SessionOrigin, TxShared, VolumeCell,
    DIR_RECV, DIR_SEND, MEDIA_SALT_LEN,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Control writes share the per-conn channel mutex with the reader and with the
/// 1s ticker, so an unresponsive peer must not be able to block either: past
/// this the write fails and the connection is declared dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an OpenStream handler waits for the tx thread to actually build the
/// media source before it answers Accept/Reject.
const SOURCE_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Silence that declares a control channel dead (spec-m4c §C wants the drop
/// visible in ~5s). The ticker pings every second, so this is five missed
/// round trips on a channel where the peer answers Ping without doing any work.
const CONTROL_SILENCE_LIMIT: Duration = Duration::from_secs(5);
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
                let spawned = std::thread::Builder::new()
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

    // dispatch on a peeked copy: verify_responder / pair_responder each read
    // the first frame themselves
    match peek_first(&stream)? {
        ControlMsg::VerifyHello { .. } => {
            let store = PeerStore::load_at(Some(&inner.cfg_dir))?;
            let mut peer = verify_responder(&mut stream, &inner.id, &store)?;
            // The peer just told us its computer name (spec-m5b §5.3). Both
            // verify paths refresh it, so a peer that renames its Mac is
            // renamed on ITS virtual devices here at the next connection —
            // whichever side dialled. The address is worth keeping too: it is
            // where a reconnect would have to look.
            peer.last_addr = Some(addr.ip().to_string());
            persist_peer(inner, peer.clone())?;
            let chan = SecureChannel::establish_responder(stream, &inner.id, &peer)?;
            // we are the responder, so the peer is the initiator of this TCP
            let initiator_fp = chan.peer().fingerprint.clone();
            let Some(conn) = register_conn(inner, chan, addr.ip(), initiator_fp) else {
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
                    &ControlMsg::Error { message: "pairing not enabled".into() },
                );
                bail!("pairing attempt while pairing not enabled");
            };
            let outcome = pair_responder(&mut stream, &pin, &inner.id);
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
        other => {
            let _ = write_frame(
                &mut stream,
                &ControlMsg::Error { message: "expected verify_hello or pair_init".into() },
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
                    dlog!(
                        "[audiohubd] pairing disabled after {MAX_PAIR_FAILURES} failed attempts"
                    );
                    disable = true;
                }
            }
        }
    }
    if disable {
        st.pairing = None;
    }
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
    chan: SecureChannel,
    peer_ip: IpAddr,
    initiator_fp: String,
) -> Option<Arc<ConnShared>> {
    let peer = chan.peer().clone();
    let mk = chan.media_keys();
    let conn = Arc::new(ConnShared {
        fp: peer.fingerprint.clone(),
        media_dest: SocketAddr::new(peer_ip, peer.port),
        tx_key: mk.tx,
        rx_key: mk.rx,
        peer,
        chan: Mutex::new(chan),
        initiator_fp,
        created: Instant::now(),
        pending: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        last_rx_ms: AtomicU64::new(0), // measured from `created`
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
        return None;
    }
    let old = st.conns.insert(conn.fp.clone(), conn.clone());
    drop(st);
    if let Some(o) = old {
        o.alive.store(false, Ordering::SeqCst);
    }
    Some(conn)
}

/// Reads this connection until it closes, then ALWAYS tears it down. The
/// catch_unwind is not about surviving a panic — the spawn sites already do
/// that — it is about where the unwind stops: a panic inside a message handler
/// used to fly past `teardown_conn`, leaving the conn registered and "alive"
/// with its sessions and rx_table entries intact, its TxCmd::Remove never sent
/// (the tx engine kept blasting media at a dead peer) and connect_peer handing
/// the zombie back as an online peer.
pub(crate) fn conn_reader(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| loop {
        if inner.shutdown.load(Ordering::SeqCst) || !conn.alive.load(Ordering::SeqCst) {
            break;
        }
        // short recv slices so senders can interleave on the chan mutex
        let res = {
            let mut ch = lk(&conn.chan);
            ch.recv_timeout(Duration::from_millis(50))
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
        dlog!("[audiohubd] control conn {}: reader panicked, tearing down", conn.fp);
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

/// true = close this connection.
fn handle_msg(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>, msg: SessionMsg) -> bool {
    match msg {
        SessionMsg::OpenStream {
            stream_id,
            kind,
            dir,
            media_salt_b64,
            verify_freq,
            source,
            freq,
            backend,
            simulate_loss_pct,
            volume_sync,
            ..
        } => {
            let reply = match handle_remote_open(
                inner,
                conn,
                stream_id,
                &kind,
                &dir,
                &media_salt_b64,
                verify_freq,
                source.as_deref(),
                freq,
                backend.as_deref(),
                simulate_loss_pct,
                volume_sync,
            ) {
                Ok(()) => SessionMsg::AcceptStream { stream_id },
                Err(e) => SessionMsg::RejectStream { stream_id, reason: format!("{e:#}") },
            };
            let _ = conn.send_msg(&reply);
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
        SessionMsg::Stats { stream_id, received, lost, loss_pct, jitter_ms } => {
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
            }
        }
        SessionMsg::VolumeSet { stream_id, scalar, muted, src } => {
            apply_peer_volume(inner, conn, stream_id, scalar, muted, &src)
        }
        SessionMsg::VolumeState { stream_id, scalar, muted, adjustable } => {
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
                    *lk(&e.volume.state) = Some(VolumeState {
                        scalar: scalar.clamp(0.0, 1.0),
                        muted,
                        adjustable,
                    });
                    // spec-round2 §B2 reverse direction rides on THIS cell: the
                    // ticker pushes genuine changes into the virtual speaker's
                    // control. Deliberately not sent from here — a mach send
                    // can sit for up to its 500ms timeout, and this is the
                    // thread that reads the peer's control channel.
                }
            }
        }
        SessionMsg::Ping { t_us } => {
            let _ = conn.send_msg(&SessionMsg::Pong { t_us });
        }
        SessionMsg::Pong { .. } => {}
        SessionMsg::Unpaired {} => {
            // The peer removed us. Its virtual devices here are now a pair of
            // ghosts — permanently offline, permanently silent, redialling a
            // machine that has blacklisted us — so they go with the pairing.
            dlog!("[audiohubd] peer {} unpaired from us; removing it here too", conn.fp);
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
    let Some(e) = owned_session(inner, conn, stream_id, "volume_set") else { return };
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
    let m = muted.unwrap_or_else(|| {
        volume::get_default_output_volume().map_or(false, |v| v.muted)
    });
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
    let shown = muted.or_else(|| last.map(|v| v.muted)).unwrap_or(false);
    *lk(&e.volume.state) = Some(VolumeState { scalar: s, muted: shown, adjustable });
    Ok(())
}

fn decode_media_salt(b64: &str) -> Result<Vec<u8>> {
    let salt = BASE64_STANDARD
        .decode(b64)
        .map_err(|e| anyhow!("media_salt_b64 is not base64: {e}"))?;
    if salt.len() != MEDIA_SALT_LEN {
        bail!("media_salt_b64 must decode to {MEDIA_SALT_LEN} bytes, got {}", salt.len());
    }
    Ok(salt)
}

/// Stream-count admission. Each stream costs a fan-out slot in the 10ms tx
/// scheduler and a jitter buffer + pop in the 10ms mixer, so an unbounded peer
/// can starve both loops for every other session.
fn check_stream_admission(st: &DaemonState, conn: &Arc<ConnShared>) -> Result<()> {
    if st.sessions.len() >= MAX_STREAMS_TOTAL {
        bail!("daemon stream limit reached ({MAX_STREAMS_TOTAL})");
    }
    let mine = st
        .sessions
        .values()
        .filter(|e| Arc::ptr_eq(&e.conn, conn))
        .count();
    if mine >= MAX_STREAMS_PER_CONN {
        bail!("per-connection stream limit reached ({MAX_STREAMS_PER_CONN})");
    }
    Ok(())
}

fn claim_stream_id(inner: &DaemonInner, conn: &Arc<ConnShared>, stream_id: u32) -> Result<()> {
    if rd(&inner.rx_table).contains_key(&stream_id) {
        bail!("stream id {stream_id} in use");
    }
    let st = lk(&inner.state);
    if st.sessions.contains_key(&stream_id) {
        bail!("stream id {stream_id} in use");
    }
    check_stream_admission(&st, conn)
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
    dest: SocketAddr,
    spec: SourceSpec,
    loss_pct: f32,
    shared: Arc<TxShared>,
) -> Result<()> {
    let (ack_tx, ack_rx) = mpsc::channel();
    lk(&inner.tx_cmds)
        .send(TxCmd::Add {
            stream_id,
            key,
            salt,
            dest,
            spec,
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
/// any well-formed OpenStream within the stream caps is accepted.
#[allow(clippy::too_many_arguments)]
fn handle_remote_open(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    stream_id: u32,
    kind: &str,
    dir: &str,
    media_salt_b64: &str,
    verify_freq: Option<f32>,
    source: Option<&str>,
    freq: Option<f32>,
    backend: Option<&str>,
    loss: Option<f32>,
    volume_sync: bool,
) -> Result<()> {
    if kind != KIND_MIC && kind != KIND_SPK {
        bail!("unknown kind {kind}");
    }
    let salt = decode_media_salt(media_salt_b64)?;
    claim_stream_id(inner, conn, stream_id)?;
    match dir {
        // opener sends media -> we receive; verify_freq applies here
        DIR_SEND => {
            let rx = Arc::new(RxStream::new(
                stream_id,
                &conn.rx_key,
                &salt,
                verify_freq,
                kind == KIND_SPK, // spk-recv joins the mixer
                false,
                None, // bridging is the local consumer's choice, never the peer's
                None, // ...and so is the virtual microphone (spec-m5b §5.4)
                conn.media_dest,
            ));
            wr(&inner.rx_table).insert(stream_id, rx.clone());
            lk(&inner.state).sessions.insert(
                stream_id,
                SessionEntry {
                    id: stream_id,
                    conn: conn.clone(),
                    kind: kind.to_string(),
                    dir: DIR_RECV.to_string(),
                    rx: Some(rx),
                    tx: None,
                    // we play this stream out of our own default output, so we
                    // are the provider the peer's slider drives
                    volume: Arc::new(VolumeCell::new(volume_sync && kind == KIND_SPK)),
                    replay: None, // the opener re-opens it after a reconnect
                    origin: SessionOrigin::Peer,
                },
            );
        }
        // opener receives -> we are the media source (provider side)
        DIR_RECV => {
            // A peer asking us for `halspk` is asking for OUR virtual speaker
            // for IT — which is the slot we bound in its name.
            let slot = lk(&inner.haldev).slot_of(&conn.fp);
            let spec = source_spec(source, freq, backend, slot)?;
            let shared = Arc::new(TxShared::new());
            start_tx_stream(
                inner,
                stream_id,
                conn.tx_key,
                salt,
                conn.media_dest,
                spec,
                loss.unwrap_or(0.0),
                shared.clone(),
            )?;
            lk(&inner.state).sessions.insert(
                stream_id,
                SessionEntry {
                    id: stream_id,
                    conn: conn.clone(),
                    kind: kind.to_string(),
                    dir: DIR_SEND.to_string(),
                    rx: None,
                    tx: Some(shared),
                    // mic provider: the opener consumes OUR source, no output
                    // device of ours is involved
                    volume: Arc::new(VolumeCell::new(false)),
                    replay: None,
                    origin: SessionOrigin::Peer,
                },
            );
        }
        other => bail!("unknown dir {other}"),
    }
    Ok(())
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
            let want = backend.filter(|b| !b.is_empty()).unwrap_or(sysaudio::BACKEND_AUTO);
            // Resolved here, not in the tx thread: an unknown/absent backend
            // must be an OpenStream rejection with a reason, not a stream that
            // is accepted and then fails its source ack five seconds later.
            // Storing the concrete id also keeps "auto" and the id it resolves
            // to sharing ONE capture instead of opening the device twice.
            let info = sysaudio::resolve_backend(want)?;
            Ok(SourceSpec::SysAudio { backend: info.id })
        }
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

pub(crate) fn teardown_stream(inner: &DaemonInner, stream_id: u32, notify_remote: bool) {
    let entry = lk(&inner.state).sessions.remove(&stream_id);
    let Some(e) = entry else { return };
    if let Some(rx) = &e.rx {
        wr(&inner.rx_table).remove(&stream_id);
        if let Some(name) = &rx.bridge {
            engine::release_bridge(inner, name);
        }
    }
    if e.tx.is_some() {
        let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
    }
    if notify_remote && e.conn.alive.load(Ordering::SeqCst) {
        let _ = e.conn.send_msg(&SessionMsg::CloseStream { stream_id });
    }
}

fn teardown_conn(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    conn.alive.store(false, Ordering::SeqCst);
    for (_, tx) in lk(&conn.pending).drain() {
        let _ = tx.send(Err("connection closed".into()));
    }
    // capture what WE opened before the entries are dropped: that list is the
    // recovery plan (spec-m4c §C); peer-originated streams carry no origin and
    // are the peer's to re-open
    let mut mine: Vec<(u32, OpenSessionParams)> = Vec::new();
    let ids: Vec<u32> = {
        let st = lk(&inner.state);
        st.sessions
            .iter()
            .filter(|(_, e)| Arc::ptr_eq(&e.conn, conn))
            .map(|(id, e)| {
                if let Some(o) = &e.replay {
                    mine.push((*id, (**o).clone()));
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
        if st.conns.get(&conn.fp).map_or(false, |c| Arc::ptr_eq(c, conn)) {
            st.conns.remove(&conn.fp);
        }
        // a newer conn to the same peer already took over
        st.conns
            .get(&conn.fp)
            .map_or(false, |c| c.alive.load(Ordering::SeqCst))
    };
    mine.sort_by_key(|(id, _)| *id); // deterministic replay order
    let mine: Vec<OpenSessionParams> = mine.into_iter().map(|(_, p)| p).collect();
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
    let mut outcome = pair_initiator(&mut stream, pin, &inner.id, inner.control_port)?;
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
            let ip = peer.last_addr.as_deref().ok_or_else(|| {
                anyhow!("no known address for {} (pass addr)", peer.fingerprint)
            })?;
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
    let addr = target_addr(&peer, addr_override)?;
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
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .with_context(|| format!("connect {addr}"))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let verified = match verify_initiator(&mut stream, &inner.id, &store) {
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
            let signed_by = e.downcast_ref::<UnpairedByPeer>().map(|u| u.fingerprint.clone());
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
    let chan = SecureChannel::establish_initiator(stream, &inner.id, &verified)?;
    // we opened this TCP, so our own fingerprint is the tie-break key
    let initiator_fp = inner.id.fingerprint.clone();
    match register_conn(inner, chan, addr.ip(), initiator_fp) {
        Some(conn) => {
            let i = inner.clone();
            let c = conn;
            let _ = std::thread::Builder::new()
                .name("ahb-conn".into())
                .spawn(move || {
                    // spec §8: a panic on one conn must not reach the daemon
                    if std::panic::catch_unwind(AssertUnwindSafe(|| conn_reader(&i, &c))).is_err() {
                        dlog!("[audiohubd] control conn {}: panicked, dropped", c.fp);
                        c.alive.store(false, Ordering::SeqCst);
                    }
                });
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
        if lk(&inner.state).sessions.contains_key(&id) {
            continue;
        }
        return id;
    }
}

/// IPC and CLI entry point. Everything opened this way is a USER session: the
/// device coordinator may never close one (spec-m5b §5.6).
pub(crate) fn open_session(
    inner: &Arc<DaemonInner>,
    params: &OpenSessionParams,
) -> Result<SessionInfo> {
    open_session_from(inner, params, SessionOrigin::User)
}

pub(crate) fn open_session_from(
    inner: &Arc<DaemonInner>,
    params: &OpenSessionParams,
    origin: SessionOrigin,
) -> Result<SessionInfo> {
    if params.kind != KIND_MIC && params.kind != KIND_SPK {
        bail!("kind must be '{KIND_MIC}' or '{KIND_SPK}'");
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

    let stream_id = alloc_stream_id(inner);
    // the same caps a remote opener is held to (B7): locally driven opens must
    // not be the way to starve the 10ms loops either
    {
        let st = lk(&inner.state);
        check_stream_admission(&st, &conn)?;
    }
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

    // register the receive side before OpenStream goes out so no early media
    // is dropped once the provider accepts
    let rx_arc = if consuming {
        let rx = Arc::new(RxStream::new(
            stream_id,
            &conn.rx_key,
            &salt,
            params.verify_freq,
            false,
            params.monitor,
            bridge.clone(),
            hal_slot,
            conn.media_dest,
        ));
        wr(&inner.rx_table).insert(stream_id, rx.clone());
        Some(rx)
    } else {
        None
    };
    let unwind = |inner: &DaemonInner, conn: &ConnShared| {
        lk(&conn.pending).remove(&stream_id);
        if consuming {
            wr(&inner.rx_table).remove(&stream_id);
        }
        if let Some(name) = &bridge {
            engine::release_bridge(inner, name);
        }
    };

    let open = SessionMsg::OpenStream {
        stream_id,
        kind: params.kind.clone(),
        dir: if consuming { DIR_RECV } else { DIR_SEND }.to_string(),
        sample_rate: 48000,
        channels: 1,
        media_salt_b64: BASE64_STANDARD.encode(salt),
        verify_freq: params.verify_freq,
        source: params.source.clone(),
        freq: params.freq,
        backend: params.backend.clone(),
        simulate_loss_pct: params.simulate_loss_pct,
        volume_sync: vol_sync,
    };
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

    // exactly what a reconnect replays; the fresh stream id and media salt are
    // minted by this function, not carried here (spec-m4c §C)
    let replay = Some(Arc::new(params.clone()));
    let entry = if consuming {
        SessionEntry {
            id: stream_id,
            conn: conn.clone(),
            kind: params.kind.clone(),
            dir: DIR_RECV.to_string(),
            rx: rx_arc,
            tx: None,
            volume: Arc::new(VolumeCell::new(false)), // mic: no remote output
            replay,
            origin,
        }
    } else {
        let shared = Arc::new(TxShared::new());
        // the peer already accepted, so a local source failure has to be undone
        // on the wire too — never leave it waiting on a stream we cannot feed
        if let Err(e) = start_tx_stream(
            inner,
            stream_id,
            conn.tx_key,
            salt.to_vec(),
            conn.media_dest,
            spec.expect("validated above"),
            params.simulate_loss_pct.unwrap_or(0.0),
            shared.clone(),
        ) {
            unwind(inner, &conn);
            let _ = conn.send_msg(&SessionMsg::CloseStream { stream_id });
            return Err(e.context("start media source"));
        }
        SessionEntry {
            id: stream_id,
            conn: conn.clone(),
            kind: params.kind.clone(),
            dir: DIR_SEND.to_string(),
            rx: None,
            tx: Some(shared),
            // consumer of a spk stream: our slider drives the PEER's output
            volume: Arc::new(VolumeCell::new(vol_sync)),
            replay,
            origin,
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
            && st.conns.get(&conn.fp).map_or(false, |c| Arc::ptr_eq(c, &conn));
        if live {
            st.sessions.insert(stream_id, entry.clone());
        }
        live
    };
    if !inserted {
        unwind(inner, &conn);
        if !consuming {
            let _ = lk(&inner.tx_cmds).send(TxCmd::Remove { stream_id });
        }
        bail!(
            "control channel to {} closed while the stream was being opened",
            conn.fp
        );
    }
    Ok(build_session_info(&entry, &[], None, None))
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
