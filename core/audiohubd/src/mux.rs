//! Tier 2: **one** connection carrying media in both directions and the
//! control stream (`docs/design-m8-fallback.md` §4, `docs/plan.md` §16).
//!
//! The premise is a tunnel that only forwards at the application layer: the
//! source address is not the peer's, and only one side can originate a
//! connection. Everything above this module is unchanged — `verify_*` and
//! `SecureChannel` run on a [`MuxControlStream`] instead of a `TcpStream`
//! (`audiohub_net::muxio`), and media frames are the same sealed datagrams UDP
//! and tier 1 carry, byte for byte.
//!
//! ```text
//! TCP (through the tunnel)
//!   └── frame layer: Header(40) ‖ payload        (audiohub_net::framed)
//!         ├── Kind::Media   → engine::handle_datagram, unchanged
//!         ├── Kind::Control → the control byte stream, unchanged
//!         └── Kind::MuxKeepalive
//! ```
//!
//! # Scheduling: strict priority, plus a control credit
//!
//! [`write_loop`] drains media first. That is right almost all of the time —
//! the two classes differ by two orders of magnitude (200 packets/s of ~1 KiB
//! against ~1 Hz of ~200 B) — and the one real risk it creates is that a
//! saturated media queue lets **no control frame out at all**. So the writer
//! guarantees one control frame every [`CONTROL_CREDIT`], and lets control run
//! freely whenever media is idle.
//!
//! Why a time credit and not "one control frame per N media frames":
//! **because `Ping`/`Pong` has to be inside it.** On tier 2 the round-trip time
//! is the only number through which a user can perceive how bad the tunnel is,
//! and coupling it to the media rate would corrupt that number exactly when the
//! link is worst — which is when it is being read.
//!
//! A full fair queue was considered and rejected: it is several hundred lines
//! against a problem one credit removes. Media frames are also **not**
//! fragmented to improve interleaving — the deepest rung's 2032-byte frame is
//! 8 ms of head-of-line at 2 Mbit/s, already inside the credit, and a
//! reassembly layer is a real complexity bought for an imaginary gain.
//!
//! # The cost that cannot be engineered away
//!
//! Both directions share one TCP connection, hence one congestion window and
//! one retransmission timer: loss in one direction stalls delivery in the
//! other. That is inherent to tier 2 and the design's answer is to label it
//! honestly rather than to pretend otherwise (plan §16.1).

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use audiohub_net::control::ControlIo;
use audiohub_net::framed::FrameDecoder;
use audiohub_net::muxio::{MuxControlStream, MuxIo};
use audiohub_net::packet::Kind;
use audiohub_net::secure::SecureChannel;

use crate::tcpmedia::{
    write_one_frame, write_one_queued, TcpMediaLink, TokenBucket, WriteOutcome,
    FRAME_COMPLETION_LIMIT, READ_SLICE, WRITE_SLICE,
};
use crate::{dlog, DaemonInner};

/// The longest a control frame may wait behind media, when media has work.
///
/// Not a rate limit — it is a floor, not a ceiling. Control traffic is ~1 Hz,
/// so in practice the credit is always available when a `Ping` is queued and the
/// frame goes out as soon as the media frame in flight finishes. The 100 ms
/// only binds if control ever becomes bursty, and it is what makes the
/// starvation bound a property of the code rather than of the traffic mix.
const CONTROL_CREDIT: Duration = Duration::from_millis(100);

/// The control stack's end of a connection, whichever transport it is on.
///
/// `SecureChannel<T>` defaults `T` to `TcpStream` so that every type position
/// in the tree keeps its spelling (P1's payoff). The daemon needs two
/// transports, so it names this one instead — an enum and not
/// `Box<dyn ControlIo>` because the set is closed and known, dispatch is a
/// branch rather than a vtable, and P6's WebSocket shell is one more arm the
/// compiler will demand at every match.
pub(crate) enum ControlTransport {
    /// Tier 0/1: the control connection is its own socket.
    Tcp(TcpStream),
    /// Tier 2: the control stream is `Kind::Control` frames on the mux.
    Mux(MuxControlStream),
}

/// What `ConnShared` holds. Named so the two spellings cannot drift.
pub(crate) type ControlChan = SecureChannel<ControlTransport>;

impl From<TcpStream> for ControlTransport {
    fn from(s: TcpStream) -> ControlTransport {
        ControlTransport::Tcp(s)
    }
}

impl From<MuxControlStream> for ControlTransport {
    fn from(s: MuxControlStream) -> ControlTransport {
        ControlTransport::Mux(s)
    }
}

impl Read for ControlTransport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ControlTransport::Tcp(s) => s.read(buf),
            ControlTransport::Mux(s) => s.read(buf),
        }
    }
}

impl Write for ControlTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ControlTransport::Tcp(s) => s.write(buf),
            ControlTransport::Mux(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ControlTransport::Tcp(s) => s.flush(),
            ControlTransport::Mux(s) => s.flush(),
        }
    }
}

impl ControlIo for ControlTransport {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> std::io::Result<()> {
        match self {
            ControlTransport::Tcp(s) => s.set_read_deadline(deadline),
            ControlTransport::Mux(s) => s.set_read_deadline(deadline),
        }
    }
    fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            ControlTransport::Tcp(s) => ControlIo::peer_addr(s),
            ControlTransport::Mux(s) => ControlIo::peer_addr(s),
        }
    }
    fn set_nodelay(&mut self, nodelay: bool) -> std::io::Result<()> {
        match self {
            ControlTransport::Tcp(s) => ControlIo::set_nodelay(s, nodelay),
            ControlTransport::Mux(s) => ControlIo::set_nodelay(s, nodelay),
        }
    }
}

// ------------------------------------------------------------------ the link

/// One multiplexed connection: the socket, the control queues and the media
/// queue that share it.
pub(crate) struct MuxLink {
    /// The control byte stream's two queues, shared with the [`ControlTransport`]
    /// the control stack holds.
    io: Arc<MuxIo>,
    /// The media half. **The same type tier 1 uses**, because it is the same
    /// problem: a bounded queue that drops the newest, a stale gate that mints
    /// the loss signal TCP erased, and the two counters that explain how a
    /// degraded link sounds. Copying it here would have produced a second stale
    /// gate to keep in step with the first.
    media: Arc<TcpMediaLink>,
    /// Kept so teardown can shut the socket down under a thread blocked in
    /// `read` or `write`; a `try_clone` of the one the reader owns.
    sock: TcpStream,
    alive: AtomicBool,
    keepalives_read: AtomicU64,
    control_frames_written: AtomicU64,
    control_frames_read: AtomicU64,
}

impl MuxLink {
    pub(crate) fn media(&self) -> &Arc<TcpMediaLink> {
        &self.media
    }

    pub(crate) fn peer(&self) -> SocketAddr {
        self.media.peer
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub(crate) fn control_frames_written(&self) -> u64 {
        self.control_frames_written.load(Ordering::Relaxed)
    }

    pub(crate) fn control_frames_read(&self) -> u64 {
        self.control_frames_read.load(Ordering::Relaxed)
    }

    pub(crate) fn keepalives_read(&self) -> u64 {
        self.keepalives_read.load(Ordering::Relaxed)
    }

    /// A link with no threads behind it, for tests that need a
    /// [`crate::tcpmedia::MediaPath::Framed`] to exist rather than to carry
    /// anything. The socket is a loopback client whose far end the caller keeps
    /// alive (or does not — nothing reads it).
    #[cfg(test)]
    pub(crate) fn new_for_test(media: Arc<TcpMediaLink>, peer: SocketAddr) -> Arc<MuxLink> {
        let sock = TcpStream::connect(peer).expect("loopback socket for a link with no threads");
        Arc::new(MuxLink {
            io: Arc::new(MuxIo::new(peer)),
            media,
            sock,
            alive: AtomicBool::new(true),
            keepalives_read: AtomicU64::new(0),
            control_frames_written: AtomicU64::new(0),
            control_frames_read: AtomicU64::new(0),
        })
    }

    /// End the connection: both threads, both queues, the socket.
    ///
    /// Closing the control inbox is what turns a dead mux into a dead control
    /// channel — the reader above sees EOF and `SecureChannel` reports
    /// "connection closed by peer" through the path it already had. Nothing has
    /// to notice a mux specifically, which is the point: on tier 2 the control
    /// channel and the media path die together **because they are one
    /// connection**, and modelling that as one event is modelling the truth.
    pub(crate) fn kill(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        self.media.kill();
        self.io.close();
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

/// Dial a tier 2 peer and bring the mux up.
///
/// Returns the link and the control transport the handshake is to run on. The
/// handshake has not happened yet: on tier 2 it travels **inside** the mux, so
/// the frame layer necessarily moves bytes before anything is authenticated
/// (see `muxio`'s module note — the exposure is the one tier 0's cleartext
/// handshake already has, and it ends at the same instant).
pub(crate) fn dial(
    inner: &Arc<DaemonInner>,
    addr: SocketAddr,
) -> Result<(Arc<MuxLink>, ControlTransport)> {
    let s = TcpStream::connect_timeout(&addr, crate::conn::CONNECT_TIMEOUT)
        .with_context(|| format!("connect {addr} for a multiplexed (tier 2) connection"))?;
    start(inner, s)
}

/// Take over an accepted socket whose first bytes are a frame header.
pub(crate) fn accept(
    inner: &Arc<DaemonInner>,
    s: TcpStream,
) -> Result<(Arc<MuxLink>, ControlTransport)> {
    start(inner, s)
}

fn start(
    inner: &Arc<DaemonInner>,
    mut s: TcpStream,
) -> Result<(Arc<MuxLink>, ControlTransport)> {
    // **Media plane rules apply to the whole connection**, because on tier 2
    // there is only one. Nagle would coalesce 10 ms frames into ACK-bound
    // bursts — ~40 ms of jitter with no visible cause in any of our own
    // numbers — so it is a hard failure here rather than the `let _ =` the
    // control plane can afford at 1 Hz.
    s.set_nodelay(true).context("tier 2 requires TCP_NODELAY")?;
    s.set_nonblocking(false)?;
    s.set_write_timeout(Some(WRITE_SLICE))?;
    s.set_read_timeout(Some(READ_SLICE))?;
    let peer = s.peer_addr()?;

    let io = Arc::new(MuxIo::new(peer));
    // The fingerprint is not known yet — it arrives in the handshake this link
    // is about to carry — so the media half is labelled by address until
    // `conn::register_conn` has a name for it. Diagnostics only; nothing
    // dispatches on it.
    let media = Arc::new(TcpMediaLink::new(String::new(), peer, inner.tx_bps));
    let wsock = s.try_clone().context("clone the mux socket for the writer")?;
    let ksock = s.try_clone().context("clone the mux socket for teardown")?;

    let link = Arc::new(MuxLink {
        io: io.clone(),
        media,
        sock: ksock,
        alive: AtomicBool::new(true),
        keepalives_read: AtomicU64::new(0),
        control_frames_written: AtomicU64::new(0),
        control_frames_read: AtomicU64::new(0),
    });

    let wlink = link.clone();
    let winner = inner.clone();
    let mut wsock = wsock;
    std::thread::Builder::new()
        .name("ahb-mux-tx".into())
        .spawn(move || {
            crate::engine::raise_audio_thread_qos("mux_write_loop");
            write_loop(&wlink, &mut wsock, &winner.shutdown);
        })
        .context("spawn the tier 2 writer")?;

    let rlink = link.clone();
    let rinner = inner.clone();
    std::thread::Builder::new()
        .name("ahb-mux-rx".into())
        .spawn(move || {
            read_loop(&rinner, &rlink, &mut s, peer);
            // Whatever ended the reader ends the connection: the control stream
            // and the media path are the same socket, so there is no state in
            // which one of them survives.
            rlink.kill();
        })
        .context("spawn the tier 2 reader")?;

    Ok((link, MuxControlStream::new(io).into()))
}

// ------------------------------------------------------------------ write path

/// Put one control frame on the wire.
///
/// **Control frames are not stale-gated.** The gate exists because audio that
/// arrives after its slot is worse than a gap; a control message that arrives
/// late is still exactly as correct as it was, and dropping one would punch a
/// hole in a byte stream that has no way to resynchronise around it. The only
/// bound is therefore [`FRAME_COMPLETION_LIMIT`], and reaching it means the
/// connection is gone.
fn write_control_frame<W: Write>(
    link: &MuxLink,
    w: &mut W,
    shutdown: &AtomicBool,
    bucket: &mut TokenBucket,
) -> Option<WriteOutcome> {
    let frame = link.io.take_control_frame()?;
    bucket.gate(shutdown);
    let hard_at = Instant::now() + FRAME_COMPLETION_LIMIT;
    Some(match write_one_frame(w, &frame, hard_at, hard_at, shutdown) {
        WriteOutcome::Sent => {
            bucket.charge(frame.len());
            link.control_frames_written.fetch_add(1, Ordering::Relaxed);
            WriteOutcome::Sent
        }
        // Not one byte of it moved in five seconds. The stream is still in
        // sync, so the frame goes back at the head — but the link is finished
        // either way, and saying so is better than retrying forever.
        WriteOutcome::Stale => {
            link.io.requeue_control_frame(frame);
            WriteOutcome::Dead
        }
        WriteOutcome::Dead => WriteOutcome::Dead,
    })
}

/// Is a control frame allowed to go out right now?
///
/// Two independent grounds, and they are different rules rather than two
/// spellings of one: media being idle means there is nothing to prioritise over
/// (so control runs at full speed on a quiet link), while the credit is the
/// bound that holds when media is *not* idle.
///
/// **Injection control for design §6 P5 acceptance 3**: delete the
/// `last_control.elapsed() >= CONTROL_CREDIT` disjunct. Strict priority alone
/// remains, and a saturated media queue then starves the control plane
/// completely — `Ping` never reaches the wire, no `Pong` comes back, and the
/// channel dies of silence.
fn control_may_go(link: &MuxLink, last_control: Instant) -> bool {
    link.io.control_pending()
        && (link.media.queued() == 0 || last_control.elapsed() >= CONTROL_CREDIT)
}

/// The writer thread: strict priority to media, with the control credit.
///
/// Generic over the sink so the starvation property can be exercised against a
/// writer that blocks on command rather than against a network.
pub(crate) fn write_loop<W: Write>(link: &MuxLink, w: &mut W, shutdown: &AtomicBool) {
    link.media.adopt_writer_thread();
    link.io.writer.adopt_current();
    // One bucket for the whole connection, because there is one wire. Charging
    // media only would make the control plane free exactly in the test that
    // exists to prove it is not.
    let mut bucket = TokenBucket::new(link.media.tx_bps());
    let mut last_control = Instant::now();
    loop {
        let mut fatal = false;
        loop {
            if control_may_go(link, last_control) {
                match write_control_frame(link, w, shutdown, &mut bucket) {
                    Some(WriteOutcome::Sent) => {
                        last_control = Instant::now();
                        continue;
                    }
                    Some(_) => {
                        fatal = true;
                        break;
                    }
                    None => {}
                }
            }
            // **The credit binds inside the media frame too, not only between
            // frames.** A frame blocked in `write` would otherwise hold the
            // wire for a whole stale budget, and the guarantee would silently
            // become "100 ms plus however long one frame blocks". Giving up on
            // a frame that has not put a byte out is invisible to the peer —
            // the stale gate's own argument — and the `seq` hole it leaves is
            // concealed as the loss it truthfully is.
            let cap = link
                .io
                .control_pending()
                .then(|| last_control + CONTROL_CREDIT);
            match write_one_queued(&link.media, w, shutdown, &mut bucket, cap) {
                Some(WriteOutcome::Dead) => {
                    fatal = true;
                    break;
                }
                Some(_) => {}
                None => break, // media queue empty; the loop above drained control
            }
            if !link.media.is_alive() || shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
        if fatal {
            // **The whole link, not just the media queue.** A writer that can no
            // longer put bytes on the socket leaves a connection that still
            // receives — the reader thread is untouched and the control inbox
            // still fills — so the peer stays `online`, `recv_timeout` keeps
            // returning messages, and every reply is queued into an outbox
            // nobody drains. That state lasts until the outbox bound is reached,
            // which at ~1 Hz is a minute of a peer that looks connected and
            // cannot answer. Killing here turns it into the ordinary dead
            // channel the reconnect machinery already knows how to rebuild.
            link.kill();
        }
        if !link.media.is_alive() || shutdown.load(Ordering::SeqCst) || link.io.is_closed() {
            return;
        }
        // Parked on the media queue's flag, re-checking control in the same
        // breath: both producers wake this thread, so both have to be part of
        // the "is there work?" question or one of them loses its wakeup.
        link.media.park_writer(WRITE_SLICE, || link.io.control_pending());
    }
}

// ------------------------------------------------------------------ read path

/// Decode frames and hand each class to the code that already owns it.
///
/// Media goes to the **same** `handle_datagram` UDP and tier 1 use — decision
/// B's payoff, and the reason the frozen header assertions in `packet.rs` cover
/// this transport for free. Control goes into the byte queue the control stack
/// reads from, with no interpretation at all: this layer does not know what a
/// `ControlMsg` is and must not learn.
fn read_loop(inner: &Arc<DaemonInner>, link: &MuxLink, s: &mut TcpStream, from: SocketAddr) {
    let mut dec = FrameDecoder::new();
    let mut scratch = [0u8; 8192];
    loop {
        if !link.is_alive() || inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let n = match s.read(&mut scratch) {
            Ok(0) => {
                dlog!("[audiohubd] tier2 mux {from}: the peer closed the connection");
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if crate::tcpmedia::blocked(e.kind()) => continue,
            Err(e) => {
                dlog!("[audiohubd] tier2 mux {from}: read: {e}");
                return;
            }
        };
        let mut off = 0usize;
        while off < n {
            off += dec.push(&scratch[off..n]);
            loop {
                let frame = match dec.next_frame() {
                    Ok(Some(f)) => f,
                    Ok(None) => break,
                    // Terminal by construction: a framed stream has no
                    // delimiter to resynchronise onto, so continuing would mean
                    // resynchronising on boundaries somebody else chose.
                    Err(e) => {
                        dlog!("[audiohubd] tier2 mux {from}: framing: {e}");
                        return;
                    }
                };
                match frame.header.kind {
                    Kind::Media => {
                        link.media.note_frame_read();
                        crate::engine::handle_datagram(inner, frame.bytes(), from);
                    }
                    Kind::Control => {
                        link.control_frames_read.fetch_add(1, Ordering::Relaxed);
                        if !link.io.deliver_control(frame.payload()) {
                            dlog!(
                                "[audiohubd] tier2 mux {from}: the control inbox would not take \
                                 the frame ({}); dropping the connection",
                                if link.io.overflowed() {
                                    "it overflowed — nobody is reading it"
                                } else {
                                    "already closed"
                                }
                            );
                            return;
                        }
                    }
                    Kind::MuxKeepalive => {
                        link.keepalives_read.fetch_add(1, Ordering::Relaxed);
                    }
                    // `PullReq` is the interesting one: tier 2 has no UDP flow
                    // to hold open and never sends it, so its arrival means the
                    // peer believes it is on a transport we are not. Counted
                    // and not fatal — the frame parsed, so the stream is in
                    // sync and the connection is still usable.
                    _ => link.media.note_unexpected_kind(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_net::framed::{control_header, encode_frame, MUX_MAX_PAYLOAD};
    use audiohub_net::packet::{Codec, Header};
    use std::sync::Mutex;

    /// A sink that decodes what it is given exactly as the peer would, and
    /// records each frame's class and arrival instant.
    ///
    /// One long-lived decoder, not one per `write`: the writer is free to split
    /// a frame across calls, and a fresh decoder each time would silently
    /// reassemble from the wrong boundary — which is the failure being measured
    /// here, so it must not also be the measuring instrument's.
    #[derive(Default)]
    struct Recorder {
        frames: Arc<Mutex<Vec<(Kind, Instant)>>>,
        dec: FrameDecoder,
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut off = 0usize;
            while off < buf.len() {
                off += self.dec.push(&buf[off..]);
                while let Some(f) = self.dec.next_frame().expect("decode") {
                    self.frames.lock().unwrap().push((f.header.kind, Instant::now()));
                }
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const MEDIA_PAYLOAD: usize = 960;

    fn media_frame(seq: u32) -> Vec<u8> {
        // `payload_len` matters: unlike `framed::encode_frame`, `Header::encode`
        // takes the field as given, so a zero here makes the peer read the next
        // frame's header as this frame's payload. Cost the first run of these
        // tests a `PayloadTooLarge { declared: 0xA5A5A5A5 }` — the payload byte,
        // read as a length, which is exactly the desynchronisation the frame
        // layer's "the payload is the sole authority" rule exists to prevent
        // on the paths that do go through `encode_frame`.
        Header {
            kind: Kind::Media,
            codec: Codec::PcmS16le,
            channels: 1,
            sample_rate: 48_000,
            session_id: 1,
            stream_id: 1,
            seq,
            timestamp_us: seq as u64 * 10_000,
            payload_len: MEDIA_PAYLOAD as u32,
        }
        .encode(&vec![0xA5u8; MEDIA_PAYLOAD])
    }

    /// A link with no socket: the scheduler is a pure state machine and this is
    /// what lets it be tested as one.
    fn test_link() -> Arc<MuxLink> {
        let peer: SocketAddr = "127.0.0.1:47899".parse().unwrap();
        let (a, _b) = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| {
                let addr = l.local_addr()?;
                let c = TcpStream::connect(addr)?;
                let (s, _) = l.accept()?;
                Ok((c, s))
            })
            .expect("loopback pair for a socket handle the scheduler never touches");
        Arc::new(MuxLink {
            io: Arc::new(MuxIo::new(peer)),
            media: Arc::new(TcpMediaLink::new("fp".into(), peer, 0)),
            sock: a,
            alive: AtomicBool::new(true),
            keepalives_read: AtomicU64::new(0),
            control_frames_written: AtomicU64::new(0),
            control_frames_read: AtomicU64::new(0),
        })
    }

    fn queue_control(link: &MuxLink, body: &[u8]) {
        let mut frame = Vec::new();
        encode_frame(&control_header(), body, &mut frame).expect("encode");
        // Straight into the outbox rather than through `MuxControlStream`: the
        // subject here is the scheduler, and going through the stream would
        // make the test also depend on the buffering rules above it.
        link.io.requeue_control_frame(frame);
    }

    fn queue_media(link: &MuxLink, n: usize) {
        let owner = Arc::new(crate::TxShared::new());
        let now = Instant::now();
        for seq in 0..n {
            let f = media_frame(seq as u32);
            assert!(
                link.media.enqueue(now, &owner, 960, |b| {
                    b.clear();
                    b.extend_from_slice(&f);
                    true
                }),
                "the media queue refused frame {seq}"
            );
        }
    }

    /// Drive the scheduler until it has nothing left to do, without spawning a
    /// thread: one pass of the inner drain loop.
    fn drain(link: &MuxLink, w: &mut Recorder, bucket: &mut TokenBucket, last_control: &mut Instant) {
        let shutdown = AtomicBool::new(false);
        loop {
            if control_may_go(link, *last_control) {
                if let Some(WriteOutcome::Sent) = write_control_frame(link, w, &shutdown, bucket) {
                    *last_control = Instant::now();
                    continue;
                }
            }
            match write_one_queued(&link.media, w, &shutdown, bucket, None) {
                Some(_) => {}
                None => break,
            }
        }
    }

    /// Media wins when both are queued and the credit has just been spent —
    /// the "strict priority" half of the rule.
    #[test]
    fn media_goes_first_while_the_control_credit_is_not_due() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);

        queue_media(&link, 8);
        queue_control(&link, b"ping");
        // Credit spent this instant, so it is not due.
        let mut last = Instant::now();
        drain(&link, &mut w, &mut bucket, &mut last);

        let kinds: Vec<Kind> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds.len(), 9, "every queued frame should have gone out");
        assert!(
            kinds[..8].iter().all(|k| *k == Kind::Media),
            "control jumped the media queue while the credit was not due: {kinds:?}"
        );
        assert_eq!(kinds[8], Kind::Control, "control never went out at all");
    }

    /// …and control goes first once the credit is due, even with media waiting.
    /// This is the assertion the injection control in `control_may_go` breaks.
    #[test]
    fn control_overtakes_media_once_the_credit_is_due() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);

        queue_media(&link, 8);
        queue_control(&link, b"ping");
        let mut last = Instant::now() - CONTROL_CREDIT - Duration::from_millis(1);
        drain(&link, &mut w, &mut bucket, &mut last);

        let kinds: Vec<Kind> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds[0],
            Kind::Control,
            "an overdue control frame did not overtake the media backlog: {kinds:?}"
        );
        assert_eq!(kinds.len(), 9);
    }

    /// A quiet media queue lets control run at full speed: no credit wait on an
    /// idle link, which is the case every tier 2 connection spends most of its
    /// life in.
    #[test]
    fn control_is_not_delayed_when_media_is_idle() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);

        for i in 0..5 {
            queue_control(&link, format!("msg{i}").as_bytes());
        }
        let mut last = Instant::now(); // credit *not* due
        let t0 = Instant::now();
        drain(&link, &mut w, &mut bucket, &mut last);

        assert_eq!(seen.lock().unwrap().len(), 5, "an idle link held control frames back");
        assert!(
            t0.elapsed() < CONTROL_CREDIT,
            "five control frames on an idle link took {:?}; the credit is a floor, not a rate limit",
            t0.elapsed()
        );
    }

    /// The starvation bound itself, stated as the scheduler's invariant: with
    /// media permanently backlogged, the gap between consecutive control frames
    /// never exceeds the credit.
    ///
    /// Deliberately not a timing test against a socket — this is the property,
    /// and the end-to-end round-trip figure (design §6 P5 acceptance 3) is
    /// measured against two real daemons in `regress/m8-p5-mux.sh`.
    #[test]
    fn a_saturated_media_queue_cannot_starve_control_past_the_credit() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);
        let shutdown = AtomicBool::new(false);

        let mut last = Instant::now() - CONTROL_CREDIT;
        // Keep media permanently backlogged and control permanently pending for
        // several credit windows. **Real time, not iterations**: the credit is
        // a duration, and forty passes through a loop that costs microseconds
        // spans none of it — the first version of this test did exactly that
        // and concluded the scheduler had emitted one control frame, which was
        // true and meant nothing.
        let window = CONTROL_CREDIT * 4;
        let until = Instant::now() + window;
        while Instant::now() < until {
            if link.media.queued() < 8 {
                queue_media(&link, 8 - link.media.queued());
            }
            if !link.io.control_pending() {
                queue_control(&link, b"ping");
            }
            if control_may_go(&link, last) {
                if let Some(WriteOutcome::Sent) =
                    write_control_frame(&link, &mut w, &shutdown, &mut bucket)
                {
                    last = Instant::now();
                }
            }
            write_one_queued(&link.media, &mut w, &shutdown, &mut bucket, None)
                .expect("the media queue must never run dry in this test");
            std::thread::sleep(Duration::from_millis(2));
        }

        let frames = seen.lock().unwrap();
        let control: Vec<Instant> = frames
            .iter()
            .filter(|(k, _)| *k == Kind::Control)
            .map(|(_, t)| *t)
            .collect();
        assert!(
            control.len() >= 3,
            "only {} control frames got out across {window:?} of permanent media backlog; the \
             credit guarantees roughly one per {CONTROL_CREDIT:?}",
            control.len()
        );
        for pair in control.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(
                gap <= CONTROL_CREDIT + Duration::from_millis(50),
                "control frames were {gap:?} apart, past the {CONTROL_CREDIT:?} credit"
            );
        }
        assert!(
            frames.iter().any(|(k, _)| *k == Kind::Media),
            "the media class was starved instead, which is the opposite bug"
        );
    }

    /// A control frame is never dropped for being old. The stale gate is an
    /// audio rule: a late control message is still correct, and a hole in a
    /// byte stream is not recoverable the way a `seq` gap is.
    #[test]
    fn the_stale_gate_does_not_apply_to_control_frames() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);
        let shutdown = AtomicBool::new(false);

        queue_control(&link, b"stale-but-still-true");
        std::thread::sleep(crate::tcpmedia::STALE_BUDGET + Duration::from_millis(20));
        let out = write_control_frame(&link, &mut w, &shutdown, &mut bucket);

        assert_eq!(out, Some(WriteOutcome::Sent), "an aged control frame was discarded");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(link.control_frames_written(), 1);
    }

    /// Media frames, meanwhile, still are gated — the same gate, in the same
    /// place, reached through the same function tier 1 uses. Without this the
    /// test above could pass because the gate had been removed altogether.
    #[test]
    fn the_stale_gate_still_applies_to_media_frames_on_the_mux() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);
        let shutdown = AtomicBool::new(false);

        let owner = Arc::new(crate::TxShared::new());
        let old = Instant::now() - crate::tcpmedia::STALE_BUDGET - Duration::from_millis(50);
        let f = media_frame(1);
        assert!(link.media.enqueue(old, &owner, 960, |b| {
            b.clear();
            b.extend_from_slice(&f);
            true
        }));

        assert_eq!(
            write_one_queued(&link.media, &mut w, &shutdown, &mut bucket, None),
            Some(WriteOutcome::Stale)
        );
        assert_eq!(seen.lock().unwrap().len(), 0, "a stale media frame reached the wire");
        assert_eq!(link.media.stale_dropped(), 1, "the drop was not counted");
    }

    /// The reader routes by `Kind` and nothing else, including the kinds that
    /// should not be here. A `PullReq` is the live case: it means the peer
    /// thinks it is on a transport with a UDP flow, and it must be counted
    /// rather than silently ignored or treated as fatal.
    #[test]
    fn an_unexpected_kind_is_counted_and_leaves_the_stream_in_sync() {
        let link = test_link();
        assert_eq!(link.media.unexpected_kind(), 0);
        link.media.note_unexpected_kind();
        assert_eq!(link.media.unexpected_kind(), 1);
        assert!(link.is_alive(), "an unexpected kind is not a reason to drop the connection");
    }

    /// Killing the link closes the control inbox, which is how a dead mux
    /// becomes a dead control channel with no extra machinery: the reader above
    /// sees EOF exactly as it would from a closed socket.
    #[test]
    fn killing_the_link_ends_the_control_stream_too() {
        let link = test_link();
        let mut ctl = MuxControlStream::new(link.io.clone());
        assert!(link.is_alive());
        link.kill();
        assert!(!link.is_alive());
        assert!(!link.media.is_alive(), "the media queue outlived the connection");

        let mut buf = [0u8; 8];
        assert_eq!(ctl.read(&mut buf).expect("read"), 0, "the control stream did not see EOF");
        // Idempotent: teardown reaches this from both threads.
        link.kill();
    }

    /// A control message too big for one frame still travels, because the
    /// writer is handed whole frames and the chunking happened above it. Guards
    /// the boundary between `muxio`'s cutting and this module's scheduling.
    #[test]
    fn an_oversized_control_message_reaches_the_wire_as_several_frames() {
        let link = test_link();
        let mut w = Recorder::default();
        let seen = w.frames.clone();
        let mut bucket = TokenBucket::new(0);
        let mut last = Instant::now() - CONTROL_CREDIT;

        let mut ctl = MuxControlStream::new(link.io.clone());
        let body = vec![0x5Au8; MUX_MAX_PAYLOAD * 2 + 11];
        ctl.write_all(&(body.len() as u32).to_le_bytes()).expect("len");
        ctl.write_all(&body).expect("body");
        ctl.flush().expect("flush");

        drain(&link, &mut w, &mut bucket, &mut last);
        let kinds: Vec<Kind> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds.len(), 3, "expected three control frames, got {kinds:?}");
        assert!(kinds.iter().all(|k| *k == Kind::Control));
    }
}
