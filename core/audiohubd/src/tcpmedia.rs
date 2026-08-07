//! Tier 1: media over a **second** TCP connection to the peer
//! (`docs/design-m8-fallback.md` §3, `docs/plan.md` §16).
//!
//! The premise is "L4 reaches the peer but UDP does not". The control channel
//! already proved TCP works to that address and port, so a second TCP
//! connection to the *same* address and port needs no new firewall hole — which
//! is why tier 1 is a second connection and not a multiplexing layer over the
//! control channel (design decision A; multiplexing costs 1.99× on the wire and
//! puts 1600 lock acquisitions per second in front of `Ping`).
//!
//! Frames on this connection are **byte-identical to the UDP datagrams**. The
//! 40-byte packet header already carries a length, a class and a stream id, so
//! it is its own frame delimiter; `audiohub_net::framed` does the decoding, and
//! the sender simply writes the sealed bytes it would have handed to `sendto`.
//!
//! # The one hard guarantee is the stale gate, and here is why it needs one
//!
//! A kernel send buffer is 64–128 KiB by default, which is 0.6–1.3 seconds of
//! rung-2 audio — **ten times the entire jitter budget**, in one buffer. And
//! TCP delivers in order, so those stale frames arrive with *consecutive* `seq`
//! numbers and the receiver's jitter buffer cannot tell them from fresh ones.
//!
//! So every queued frame carries the instant it was queued, and
//! [`write_loop`] refuses to write one that has aged past [`STALE_BUDGET`].
//! The by-product matters as much as the drop: skipping a frame leaves a hole
//! in `seq`, and the receiver's jitter buffer conceals a `seq` hole correctly,
//! as real packet loss. **TCP erases the loss signal and we mint it back
//! exactly where it is true.** That is not a workaround; it is the same
//! argument `engine.rs`'s send queue already makes about why dropping beats
//! queueing — "一次卡顿不该变成一串永久的陈音频".
//!
//! # What is deliberately *not* here
//!
//! **`SO_SNDBUF` is not shrunk.** Design §3.2 defence 3 suggests ~8 KiB to make
//! `WouldBlock` happen sooner, and design §8 item 1 records that nobody has
//! measured what macOS and Windows actually do with such a request — whether
//! they clamp it up, and what it costs throughput. `std::net::TcpStream` does
//! not expose the option at all, so implementing it means `libc` (unix-only in
//! this crate) or a new dependency, spent on an optimisation the design
//! explicitly forbids resting the guarantee on. The guarantee is the stale
//! gate; it does not get better or worse with the buffer size, only more or
//! less often exercised.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::prelude::*;

use audiohub_net::control::{read_frame, write_frame, ControlMsg};
use audiohub_net::framed::FrameDecoder;
use audiohub_net::packet::Kind;
use audiohub_net::secure::SessionMsg;


use crate::peer_transport::TransportTier;
use crate::rtsafe::SpscRing;
use crate::{dlog, lk, ConnShared, DaemonInner, TxShared};

// ------------------------------------------------------------------ constants

/// How old a queued frame may be when the writer picks it up. Older ⇒ dropped
/// and counted as [`TcpMediaLink::stale_dropped`].
///
/// # Why 200 ms
///
/// It is bracketed by two numbers the receiver already lives by
/// (`media.rs`'s `JbTuning::DEFAULT`):
///
/// - `max_target = 12` frames = **120 ms** is the deepest steady-state target
///   the jitter buffer will aim for. A frame older than that has already missed
///   the slot it was going to be played in, whatever the receiver does.
/// - `max_frames = 24` frames = **240 ms** is the hard ceiling at which the
///   jitter buffer itself starts discarding the oldest. Dropping *below* that
///   keeps the decision — and the counter — on the side that can explain it.
///   Above it, the same audio would still be discarded, silently, by the peer.
///
/// 200 ms sits between them, which is also the figure design §3.2 defence 1
/// names for the tier 1 profile. It is a budget, not a measurement, and it is
/// stated here rather than derived so that P4's `JbTuning::DEGRADED`
/// (`max_target = 40`) forces somebody to name which profile this budget
/// belongs to instead of letting it move underneath them.
pub(crate) const STALE_BUDGET: Duration = Duration::from_millis(200);

/// The two numbers the budget above is bracketed by, **read from the jitter
/// buffer** rather than copied.
///
/// The first version of this assertion spelled them `12 * 10` and `24 * 10`.
/// It went red when `STALE_BUDGET` moved and stayed green when
/// `JbTuning::DEFAULT` moved — which is backwards, because the failure message
/// asserts a *relationship* between them, and only one side of that
/// relationship was actually being read. Verified by mutation on 2026-08-08:
/// with the literals, dropping `max_frames` to 16 compiled clean; with the
/// constants below it fails to compile.
const JB_DEEPEST_TARGET_MS: u64 =
    audiohub_net::media::JbTuning::DEFAULT.max_target as u64 * crate::engine::FRAME_MS;
const JB_HARD_CEILING_MS: u64 =
    audiohub_net::media::JbTuning::DEFAULT.max_frames as u64 * crate::engine::FRAME_MS;

const _: () = assert!(
    STALE_BUDGET.as_millis() as u64 > JB_DEEPEST_TARGET_MS
        && (STALE_BUDGET.as_millis() as u64) < JB_HARD_CEILING_MS,
    "the stale budget left the window between the jitter buffer's deepest target and its hard \
     ceiling: below the target it drops audio the receiver would have played, above the ceiling \
     the receiver drops it anyway and nothing on this side counts it"
);

/// Queue depth in frames. Must be a power of two ([`SpscRing`]'s hard rule).
///
/// Same 128 as the UDP send queue, for the same reason: at one frame per 10 ms
/// per stream it is 1.28 s for a single stream and 80 ms across sixteen — both
/// far past the point where the receiver has already underrun, which is what
/// makes dropping the *newest* the right choice rather than merely the easy
/// one. See the three-option table at `engine.rs`'s `UdpSender`; it applies
/// here verbatim, and the stale gate below is the part that is new.
const SEND_SLOTS: usize = 128;

/// Bytes reserved per slot, sized by the deepest rung's sealed frame exactly as
/// `engine::SEND_SLOT_BYTES` is — and for the same reason: a slot that has to
/// grow does its `malloc` inside `tx_loop`, the 10 ms deadline thread.
const SLOT_BYTES: usize = 2048;

const _: () = assert!(
    SLOT_BYTES >= crate::engine::DEEPEST_SEALED_FRAME_BYTES,
    "a tier 1 slot cannot hold the deepest rung: switching to it would make all 128 slots \
     reallocate at once, on the 10 ms deadline thread"
);

/// How long a single `write` may block before the writer thread gets control
/// back to re-check the stale gate.
///
/// The socket stays **blocking** and gets this as `SO_SNDTIMEO` rather than
/// being made non-blocking. Both halves of the connection share one file
/// description, so `O_NONBLOCK` would apply to the reader too and turn it into
/// a spin; a short send timeout gives the writer the same "do not sit here
/// forever" property with no spin anywhere. 20 ms is a tenth of
/// [`STALE_BUDGET`], so the gate can overshoot by at most that much.
const WRITE_SLICE: Duration = Duration::from_millis(20);

/// Same, for the reader — it only needs to notice shutdown.
const READ_SLICE: Duration = Duration::from_millis(200);

/// A frame that has already put bytes on the wire **must** be finished, or the
/// stream desynchronises and every following byte is misread. So the stale gate
/// cannot apply mid-frame, and this is the backstop instead: a frame that
/// cannot be completed in this long means the connection is gone, whatever the
/// socket believes.
const FRAME_COMPLETION_LIMIT: Duration = Duration::from_secs(5);

/// Ticket lifetime. Long enough for a dial plus a handshake on a bad link,
/// short enough that a leaked one is worthless by the time it is read out of a
/// log.
const TICKET_TTL: Duration = Duration::from_secs(10);

const TICKET_LEN: usize = 32;

/// How long `register_conn` will block waiting for the tier 1 link.
///
/// Must exceed `conn::CONNECT_TIMEOUT` plus a round trip, because the dial it
/// is waiting on gets that long to fail on its own — a backstop shorter than
/// the thing it backstops would fire first and every time. It is a backstop and
/// not the normal exit: a refusal, a dead channel and a failed dial all end the
/// wait in one round trip.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(8);

const _: () = assert!(
    ATTACH_TIMEOUT.as_secs() > crate::conn::CONNECT_TIMEOUT.as_secs(),
    "the attach backstop is shorter than the dial it backstops, so it fires before the dial can \
     report what actually went wrong"
);

/// Slice for both waits during negotiation: how long one `recv_timeout` blocks
/// while pumping for the ticket, and the longest a `Condvar` wait sits without
/// re-reading `alive`.
const PUMP_SLICE: Duration = Duration::from_millis(50);

// ------------------------------------------------------------------ MediaPath

/// Where a stream's media goes.
///
/// # Why this is an enum and not a `SocketAddr` with a flag
///
/// `ConnShared` used to hold `media_dest: SocketAddr`, computed unconditionally
/// as peer IP + the port the peer advertised. On a transport with no UDP
/// destination that field cannot hold the truth, and the value it *would* hold
/// is a perfectly well-formed address that media would be sent to and silently
/// dropped. Making the absence of a destination unrepresentable is the point:
/// `send_pullreq` and `refresh_dest` cannot compile against a
/// [`MediaPath::Tcp`], so they cannot forget to skip it.
#[derive(Clone)]
pub(crate) enum MediaPath {
    /// Tier 0: one shared UDP socket, this datagram destination.
    Udp(SocketAddr),
    /// Tier 1: this peer's dedicated media TCP connection.
    Tcp(Arc<TcpMediaLink>),
}

impl MediaPath {
    /// The UDP destination, or `None` on a transport that has none. Callers use
    /// this to skip UDP-only work; nobody may invent an address for the `None`
    /// case.
    pub(crate) fn udp_dest(&self) -> Option<SocketAddr> {
        match self {
            MediaPath::Udp(a) => Some(*a),
            MediaPath::Tcp(_) => None,
        }
    }
}

impl std::fmt::Debug for MediaPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaPath::Udp(a) => write!(f, "udp({a})"),
            MediaPath::Tcp(l) => write!(f, "tcp({})", l.peer),
        }
    }
}

// ------------------------------------------------------------------ the queue

/// One frame waiting to be written.
struct SendSlot {
    /// The sealed datagram. Allocated once at construction, rewritten in place.
    buf: Vec<u8>,
    /// When this frame was handed to us. **The stale gate's only input**, and
    /// the reason it is an `Instant` and not the header's `timestamp_us`: that
    /// field is relative to `tx_loop`'s own start, an epoch the writer thread
    /// has no access to, and reconstructing it here would be a subtraction
    /// between two clocks that only look like one.
    queued_at: Instant,
    payload_len: usize,
    /// Accounting target, taken by the consumer so the `Arc` decrement happens
    /// on the writer thread rather than the deadline thread.
    owner: Option<Arc<TxShared>>,
}

/// A live tier 1 media connection.
///
/// One per peer, shared by both directions — the transport is per peer once a
/// downgrade has happened (see [`TransportTier`]). Exactly one producer
/// (`tx_loop`) and exactly one consumer ([`write_loop`]), which is [`SpscRing`]'s
/// safety precondition and not a stylistic preference.
pub(crate) struct TcpMediaLink {
    /// Fingerprint of the peer this belongs to.
    pub(crate) fp: String,
    /// The remote end, for diagnostics.
    pub(crate) peer: SocketAddr,
    q: SpscRing<SendSlot>,
    thread: OnceLock<std::thread::Thread>,
    parked: AtomicBool,
    alive: AtomicBool,
    stale_dropped: AtomicU64,
    frames_written: AtomicU64,
    frames_read: AtomicU64,
    /// Frames that arrived on this connection with a `Kind` other than
    /// `Media`. Counted rather than tolerated silently: tier 1 carries media
    /// only, so anything else means the peer is running a protocol we do not.
    unexpected_kind: AtomicU64,
}

impl TcpMediaLink {
    fn new(fp: String, peer: SocketAddr) -> TcpMediaLink {
        TcpMediaLink {
            fp,
            peer,
            q: SpscRing::new(SEND_SLOTS, |_| SendSlot {
                buf: Vec::with_capacity(SLOT_BYTES),
                queued_at: Instant::now(),
                payload_len: 0,
                owner: None,
            }),
            thread: OnceLock::new(),
            parked: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            stale_dropped: AtomicU64::new(0),
            frames_written: AtomicU64::new(0),
            frames_read: AtomicU64::new(0),
            unexpected_kind: AtomicU64::new(0),
        }
    }

    /// Queue one sealed frame. `fill` writes it into the slot in place;
    /// returning `false` voids that frame (the consumer never sees it).
    ///
    /// **Zero allocation, zero locks, zero syscalls** — this runs on `tx_loop`.
    /// Returns `false` when the queue is full (counted) or the link is dead.
    pub(crate) fn enqueue(
        &self,
        queued_at: Instant,
        owner: &Arc<TxShared>,
        payload_len: usize,
        fill: impl FnOnce(&mut Vec<u8>) -> bool,
    ) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        self.q.produce(|slot| {
            if !fill(&mut slot.buf) {
                return false;
            }
            slot.queued_at = queued_at;
            slot.payload_len = payload_len;
            slot.owner = Some(owner.clone());
            true
        })
    }

    /// Wake the writer. At most once per tick, after every stream has been
    /// queued — same contract, and the same `SeqCst` fence pairing, as
    /// `UdpSender::wake`.
    pub(crate) fn wake(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) {
            if let Some(t) = self.thread.get() {
                t.unpark();
            }
        }
    }

    pub(crate) fn queued(&self) -> usize {
        self.q.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.q.capacity()
    }

    /// Frames refused because the queue was full. Same meaning as
    /// `UdpSender::dropped`: `enqueue` never retries, so refused is dropped.
    pub(crate) fn dropped(&self) -> u64 {
        self.q.rejected()
    }

    /// Frames the stale gate refused to write. **This and `dropped` are the two
    /// numbers that explain why tier 1 sounds the way it does**; they exist
    /// nowhere else.
    pub(crate) fn stale_dropped(&self) -> u64 {
        self.stale_dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn frames_written(&self) -> u64 {
        self.frames_written.load(Ordering::Relaxed)
    }

    pub(crate) fn frames_read(&self) -> u64 {
        self.frames_read.load(Ordering::Relaxed)
    }

    pub(crate) fn unexpected_kind(&self) -> u64 {
        self.unexpected_kind.load(Ordering::Relaxed)
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// A link with no socket behind it, for tests that need a
    /// [`MediaPath::Tcp`] to exist rather than to carry anything.
    #[cfg(test)]
    pub(crate) fn new_for_test(fp: String, peer: SocketAddr) -> TcpMediaLink {
        TcpMediaLink::new(fp, peer)
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.get() {
            t.unpark();
        }
    }
}

// ------------------------------------------------------------------ write path

/// Why a frame did not go out whole.
#[derive(Debug, PartialEq)]
enum WriteOutcome {
    Sent,
    /// Aged past its budget before a single byte reached the socket. The frame
    /// is dropped; the `seq` hole it leaves is what the receiver conceals.
    Stale,
    /// The connection is finished.
    Dead,
}

fn blocked(k: ErrorKind) -> bool {
    matches!(k, ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Write one whole frame, or give up.
///
/// Two deadlines, and the difference between them is the whole correctness
/// argument:
///
/// - **`stale_at`** applies only while *nothing* has been written. Up to that
///   point the frame exists solely in our queue and dropping it is invisible to
///   the wire.
/// - Once one byte is out, the frame **must** be completed: the header carries
///   the length, so an abandoned frame makes the peer read the next frame's
///   bytes as this frame's payload and every byte after that is garbage. So
///   from then on the only bound is [`FRAME_COMPLETION_LIMIT`], and exceeding
///   it kills the connection rather than desynchronising it.
fn write_one_frame<W: Write>(
    w: &mut W,
    buf: &[u8],
    stale_at: Instant,
    hard_at: Instant,
    shutdown: &AtomicBool,
) -> WriteOutcome {
    let mut off = 0usize;
    while off < buf.len() {
        match w.write(&buf[off..]) {
            Ok(0) => return WriteOutcome::Dead, // peer closed
            Ok(n) => off += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if blocked(e.kind()) => {
                if shutdown.load(Ordering::SeqCst) {
                    return WriteOutcome::Dead;
                }
                let now = Instant::now();
                if off == 0 {
                    if now >= stale_at {
                        return WriteOutcome::Stale;
                    }
                } else if now >= hard_at {
                    return WriteOutcome::Dead;
                }
            }
            Err(_) => return WriteOutcome::Dead,
        }
    }
    WriteOutcome::Sent
}

/// The writer thread's body, generic over the sink so the ratchet property can
/// be tested against a writer that blocks on command rather than against a
/// network.
///
/// Returns when the link is dead or the daemon is shutting down.
fn write_loop<W: Write>(link: &TcpMediaLink, w: &mut W, shutdown: &AtomicBool) {
    link.thread.get_or_init(std::thread::current);
    loop {
        // Drain everything queued, applying the gate to each frame as it comes
        // off — not once per batch. A batch can span the whole budget.
        let mut fatal = false;
        while link.q.consume(|slot| {
            let owner = slot.owner.take(); // dropped on THIS thread
            let queued_at = slot.queued_at;
            let outcome = if queued_at.elapsed() > STALE_BUDGET {
                WriteOutcome::Stale
            } else {
                write_one_frame(
                    w,
                    &slot.buf,
                    queued_at + STALE_BUDGET,
                    queued_at + FRAME_COMPLETION_LIMIT,
                    shutdown,
                )
            };
            match outcome {
                WriteOutcome::Sent => {
                    link.frames_written.fetch_add(1, Ordering::Relaxed);
                    if let Some(o) = owner {
                        o.sent_packets.fetch_add(1, Ordering::Relaxed);
                        o.sent_bytes.fetch_add(slot.buf.len() as u64, Ordering::Relaxed);
                        o.sent_payload_bytes
                            .fetch_add(slot.payload_len as u64, Ordering::Relaxed);
                    }
                }
                WriteOutcome::Stale => {
                    link.stale_dropped.fetch_add(1, Ordering::Relaxed);
                }
                WriteOutcome::Dead => fatal = true,
            }
        }) {
            if fatal || !link.alive.load(Ordering::Relaxed) {
                break;
            }
        }
        if fatal {
            link.kill();
        }
        if !link.alive.load(Ordering::Relaxed) || shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Set the flag, then re-check the queue; pairs with `wake`'s
        // publish-then-read. Skipping the re-check is the classic lost wakeup,
        // and its symptom here is one frame arriving a backstop late — i.e.
        // jitter at the peer, with nothing to point at.
        link.parked.store(true, Ordering::SeqCst);
        if link.q.len() == 0 {
            std::thread::park_timeout(WRITE_SLICE);
        }
        link.parked.store(false, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ read path

/// Read frames off the connection and feed them to the **same**
/// `handle_datagram` the UDP path uses.
///
/// That reuse is the payoff of decision B: a media frame here is byte for byte
/// the datagram UDP would have delivered, so there is one decrypt path, one
/// jitter buffer path, one set of statistics — and the frozen header assertions
/// in `packet.rs` cover this transport for free.
fn read_loop(inner: &Arc<DaemonInner>, link: &TcpMediaLink, s: &mut TcpStream, from: SocketAddr) {
    let mut dec = FrameDecoder::new();
    let mut scratch = [0u8; 8192];
    loop {
        if !link.alive.load(Ordering::Relaxed) || inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let n = match s.read(&mut scratch) {
            Ok(0) => {
                dlog!("[audiohubd] tier1 media {}: peer closed the media connection", link.fp);
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if blocked(e.kind()) => continue,
            Err(e) => {
                dlog!("[audiohubd] tier1 media {}: read: {e}", link.fp);
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
                    // A framing error is terminal by construction: there is no
                    // delimiter to resynchronise onto, so carrying on would mean
                    // resynchronising on boundaries somebody else chose.
                    Err(e) => {
                        dlog!("[audiohubd] tier1 media {}: framing: {e}", link.fp);
                        return;
                    }
                };
                if frame.header.kind == Kind::Media {
                    link.frames_read.fetch_add(1, Ordering::Relaxed);
                    crate::engine::handle_datagram(inner, frame.bytes(), from);
                } else {
                    // Tier 1 carries media only. `Kind::Control` /
                    // `MuxKeepalive` belong to tier 2 (P5) and `PullReq` is not
                    // sent on this transport at all, so anything here means the
                    // peer is speaking a protocol we are not. Counted, not
                    // fatal: the frame parsed, so the stream is still in sync.
                    link.unexpected_kind.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// ------------------------------------------------------------------ attachment

/// Turn an accepted-or-dialled socket into this peer's media path, then serve
/// it until it dies. **Runs the reader on the calling thread** and spawns one
/// thread for the writer.
///
/// The writer gets its own thread because writing must never happen on
/// `tx_loop`: a `write` into the kernel has no predictable upper bound, and
/// putting one on the 10 ms deadline thread is precisely the thing
/// `udp_send_loop` was created to undo.
pub(crate) fn serve(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    mut s: TcpStream,
    // Held, never read: it is this function's exclusive right to own
    // `conn.media_path`, and it is released by `Drop` when serving ends —
    // including on any `?` below. See [`AttachClaim`].
    _claim: AttachClaim,
) -> Result<()> {
    // **Media plane: Nagle is not optional.** The control plane can afford
    // `let _ = set_nodelay(...)` at ~1 Hz; here Nagle coalesces 10 ms frames
    // into bursts that wait for an ACK — roughly 40 ms of jitter with no
    // visible cause anywhere in our own numbers. A link we cannot turn it off
    // on is a link we refuse to promote.
    s.set_nodelay(true).context("tier 1 media requires TCP_NODELAY")?;
    s.set_nonblocking(false)?;
    s.set_write_timeout(Some(WRITE_SLICE))?;
    s.set_read_timeout(Some(READ_SLICE))?;
    let peer = s.peer_addr()?;

    let link = Arc::new(TcpMediaLink::new(conn.fp.clone(), peer));
    let mut wsock = s.try_clone().context("clone the media socket for the writer")?;

    let wlink = link.clone();
    let winner = inner.clone();
    let writer = std::thread::Builder::new()
        .name("ahb-tcpmedia-tx".into())
        .spawn(move || {
            crate::engine::raise_audio_thread_qos("tcpmedia_write_loop");
            write_loop(&wlink, &mut wsock, &winner.shutdown);
        })
        .context("spawn the tier 1 media writer")?;

    *lk(&conn.media_path) = MediaPath::Tcp(link.clone());
    // Wakes `negotiate`, which is holding `register_conn` open precisely so
    // that no stream is created before this line runs.
    conn.media_gate.announce();
    dlog!("[audiohubd] tier1 media attached to {} via {peer}", conn.fp);

    read_loop(inner, &link, &mut s, peer);

    // Teardown. Order matters: kill first so the writer stops re-queuing, then
    // shut the socket down so a writer parked inside `write` returns.
    link.kill();
    let _ = s.shutdown(Shutdown::Both);
    let _ = writer.join();

    // Put the connection's path back so a *future* stream does not attach to a
    // corpse. Streams already running keep their own handle to the dead link
    // and their `enqueue` starts failing — which is why the control connection
    // is dropped below rather than left up: the existing reconnect + replay
    // machinery is the only thing that can rebuild both the link and the
    // streams, and using it makes a dead media path as loud as a dead control
    // path instead of a peer that is connected and silent.
    *lk(&conn.media_path) = MediaPath::Udp(SocketAddr::new(conn.peer_ip, conn.peer.port));
    // The link is no longer in flight either, so a waiter that is still around
    // stops believing one is coming.
    conn.media_attaching.store(false, Ordering::SeqCst);
    conn.media_gate.announce();
    if !inner.shutdown.load(Ordering::SeqCst) && conn.alive.load(Ordering::SeqCst) {
        dlog!(
            "[audiohubd] tier1 media to {} is gone; dropping the control connection so the \
             normal reconnect path rebuilds both",
            conn.fp
        );
        conn.alive.store(false, Ordering::SeqCst);
    }
    Ok(())
}

// ------------------------------------------------------------------ tickets

/// A minted, unspent attach ticket.
pub(crate) struct MediaTicket {
    bytes: [u8; TICKET_LEN],
    /// Which connection this ticket may attach to. A ticket is useless against
    /// any other peer even before it expires.
    fp: String,
    expires: Instant,
}

/// Mint a ticket without going through the offer path, so a test can produce a
/// *second* one — `offer_ticket` suppresses those on purpose.
#[cfg(test)]
pub(crate) fn mint_ticket_for_test(inner: &Arc<DaemonInner>, fp: &str) -> String {
    mint_ticket(inner, fp)
}

fn mint_ticket(inner: &Arc<DaemonInner>, fp: &str) -> String {
    let mut bytes = [0u8; TICKET_LEN];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
    let b64 = BASE64_STANDARD.encode(bytes);
    let mut t = lk(&inner.media_tickets);
    let now = Instant::now();
    t.retain(|x| x.expires > now);
    t.push(MediaTicket { bytes, fp: fp.to_string(), expires: now + TICKET_TTL });
    b64
}

/// Spend a ticket, returning the fingerprint it was minted for.
///
/// Compared in constant time and by linear scan rather than by hash lookup. The
/// scan is over at most a handful of live tickets, and the alternative would
/// make the timing of a probe depend on how much of the secret was guessed
/// right — a small thing to give away for no gain at all.
fn claim_ticket(inner: &Arc<DaemonInner>, ticket_b64: &str) -> Option<String> {
    let raw = BASE64_STANDARD.decode(ticket_b64).ok()?;
    if raw.len() != TICKET_LEN {
        return None;
    }
    let now = Instant::now();
    let mut t = lk(&inner.media_tickets);
    t.retain(|x| x.expires > now);
    let idx = t
        .iter()
        .position(|x| bool::from(subtle::ConstantTimeEq::ct_eq(&x.bytes[..], &raw[..])))?;
    Some(t.swap_remove(idx).fp)
}

// ------------------------------------------------------------------ negotiation

/// Is this daemon the one that opened the control TCP to this peer?
///
/// The media link may only be dialled in that direction: the control handshake
/// proved `us → them` works and said nothing about the reverse. Dialling the
/// other way would produce a failure indistinguishable from a peer that is off.
fn we_dialled(inner: &DaemonInner, conn: &ConnShared) -> bool {
    conn.initiator_fp == inner.id.fingerprint
}

/// Called once per connection, right after it is registered — and it **blocks
/// until the link is up or the attempt is over**. Manual pinning only;
/// automatic downgrade detection is P4.
///
/// # Why this is synchronous
///
/// It was not, and that was the bug. Attaching takes a request, a ticket, a
/// dial and a handshake: ~200 ms on loopback, measured 2026-08-08, against a
/// `connect_peer` that returns the moment the *control* handshake is done.
/// Every stream opened in that window binds itself to the UDP path for its
/// whole life (design §5.1 rules out switching inside a live stream), so on the
/// links tier 1 exists for — the ones where UDP is blocked — the result is two
/// healthy ends, an all-green screen and total silence.
///
/// And the window is not a race that "usually" resolves the right way: it is
/// hit every single time by `session.open` to a peer that is not connected yet,
/// and by `reconnect::replay_sessions`, which re-opens every stream as soon as
/// `connect_peer` returns. That second one closes a loop — [`serve`]'s teardown
/// deliberately drops the control connection so replay rebuilds both — so
/// without this wait, every tier 1 link death would rebuild the streams pinned
/// back onto UDP, permanently, until a human re-opened them by hand.
///
/// Costs nothing on tier 0: the tier check below returns before anything is
/// sent, so a connection that is not pinned never waits at all.
pub(crate) fn negotiate(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    if lk(&inner.peer_transport).tier(&conn.fp) != TransportTier::Tier1 {
        return;
    }
    let deadline = Instant::now() + ATTACH_TIMEOUT;
    let dialling = if we_dialled(inner, conn) {
        // We can dial, so we ask for the ticket that lets us — and then read
        // the answer off the channel ourselves, because `conn_reader` does not
        // exist yet (`register_conn` starts it only after we return).
        if conn.send_msg(&SessionMsg::MediaAttachRequest {}).is_err() {
            return;
        }
        if !pump_for_ticket(inner, conn, deadline) {
            return;
        }
        true
    } else {
        // We cannot dial this peer, so we offer the peer a ticket unprompted.
        // A peer pinned to tier 0 will ignore it, which is the correct outcome:
        // an offer is not an instruction.
        offer_ticket(inner, conn);
        false
    };
    await_attach(conn, deadline, dialling);
}

/// Read the control channel until the attach ticket shows up.
///
/// Returns `true` when a dial is now under way. Everything that is not an
/// answer to our request is **parked**, not handled: an `OpenStream` handled
/// here would bind a stream to the media path we are in the middle of
/// replacing, which is the whole failure this wait exists to prevent.
/// `conn_reader` drains the park before it touches the socket, so channel order
/// survives.
fn pump_for_ticket(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if matches!(*lk(&conn.media_path), MediaPath::Tcp(_)) {
            return true; // a ticket arrived some other way and won the race
        }
        if !conn.alive.load(Ordering::SeqCst) || inner.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let res = {
            let mut ch = lk(&conn.chan);
            ch.recv_timeout(PUMP_SLICE)
        };
        match res {
            Ok(Some(SessionMsg::MediaAttachTicket { ticket_b64 })) => {
                conn.note_rx();
                on_ticket(inner, conn, ticket_b64);
                return true;
            }
            Ok(Some(SessionMsg::MediaAttachRefused { reason })) => {
                conn.note_rx();
                dlog!(
                    "[audiohubd] {} refused a tier 1 media attach: {reason}; staying on tier 0",
                    conn.fp
                );
                return false;
            }
            Ok(Some(other)) => {
                conn.note_rx();
                lk(&conn.deferred).push_back(other);
            }
            Ok(None) => {}
            Err(e) => {
                dlog!("[audiohubd] control channel {} while attaching tier 1: {e:#}", conn.fp);
                return false;
            }
        }
    }
    dlog!(
        "[audiohubd] {} never answered our tier 1 media attach request within {ATTACH_TIMEOUT:?}; \
         media stays on UDP",
        conn.fp
    );
    false
}

/// Block until the media path really is tier 1, or until we know it will not be.
///
/// `dialling` says whether the local dial thread is the thing we are waiting on.
/// If it is, its `media_attaching` flag doubles as "still trying": the dial
/// clears it on failure, which is what lets a refused or unreachable peer end
/// the wait in one round trip instead of one timeout. The accepting side has no
/// such signal — it is waiting for the *peer* to dial in — so it waits out the
/// deadline.
fn await_attach(conn: &Arc<ConnShared>, deadline: Instant, dialling: bool) {
    let mut path = lk(&conn.media_path);
    loop {
        if matches!(*path, MediaPath::Tcp(_)) {
            return;
        }
        if dialling && !conn.media_attaching.load(Ordering::SeqCst) {
            break; // the dial gave up and said so
        }
        if !conn.alive.load(Ordering::SeqCst) {
            break;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        if left.is_zero() {
            break;
        }
        // Capped slices so `alive` and `media_attaching` are re-read even if a
        // notify is lost — the wait is a deadline, not a handshake.
        let (p, _) = conn
            .media_gate
            .settled
            .wait_timeout(path, left.min(PUMP_SLICE))
            .unwrap_or_else(|e| e.into_inner());
        path = p;
    }
    drop(path);
    dlog!(
        "[audiohubd] {} is pinned to tier 1 but no media link came up; its media will go over \
         UDP, which is the transport tier 1 exists because it cannot use",
        conn.fp
    );
}

fn offer_ticket(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    if matches!(*lk(&conn.media_path), MediaPath::Tcp(_)) {
        return; // already attached; a second link would be a second writer
    }
    // One live ticket per peer. A negotiation can reach here twice — the
    // responder offers unprompted and then the initiator's request arrives —
    // and the second ticket is spent by nobody yet lives out its whole TTL.
    // Both paths run over the same reliable control channel, so the first
    // ticket is certain to be delivered and the second is certain to be waste.
    {
        let now = Instant::now();
        let t = lk(&inner.media_tickets);
        if t.iter().any(|x| x.fp == conn.fp && x.expires > now) {
            return;
        }
    }
    let ticket_b64 = mint_ticket(inner, &conn.fp);
    let _ = conn.send_msg(&SessionMsg::MediaAttachTicket { ticket_b64 });
}

/// Peer asked for a ticket. We are, by the rule above, the side it dialled.
pub(crate) fn on_request(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>) {
    // **Advertisement is not authorisation.** A peer asking for tier 1 does not
    // get it if this machine is pinned to tier 0; the same discipline
    // `handle_remote_open` applies to `ModeState`.
    if lk(&inner.peer_transport).tier(&conn.fp) == TransportTier::Tier0 {
        dlog!(
            "[audiohubd] {} asked to attach a tier 1 media link; this machine is pinned to \
             tier 0, refusing",
            conn.fp
        );
        // Said out loud rather than dropped: the asker is blocking on this
        // answer, and a refusal it has to infer from a timeout is a refusal
        // that costs it the whole attach budget.
        let _ = conn.send_msg(&SessionMsg::MediaAttachRefused {
            reason: "this peer is pinned to tier 0".into(),
        });
        return;
    }
    offer_ticket(inner, conn);
}

/// Peer handed us a ticket. Dial the media connection on a thread of its own.
pub(crate) fn on_ticket(inner: &Arc<DaemonInner>, conn: &Arc<ConnShared>, ticket_b64: String) {
    if lk(&inner.peer_transport).tier(&conn.fp) == TransportTier::Tier0 {
        return; // pinned; ignore the offer
    }
    if !we_dialled(inner, conn) {
        dlog!(
            "[audiohubd] ignoring a media attach ticket from {}: we did not open this control \
             connection, so we have no proven route back to it",
            conn.fp
        );
        return;
    }
    // Both sides can end up sending — the initiator asks and the responder may
    // also offer unprompted — so claim the right to dial exactly once.
    if conn
        .media_attaching
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let dest = SocketAddr::new(conn.peer_ip, conn.peer.port);
    let owned_inner = inner.clone();
    let owned_conn = conn.clone();
    let spawned = std::thread::Builder::new()
        .name("ahb-tcpmedia".into())
        .spawn(move || {
            if let Err(e) = dial_and_serve(&owned_inner, &owned_conn, dest, &ticket_b64) {
                dlog!("[audiohubd] tier 1 media to {} ({dest}): {e:#}", owned_conn.fp);
                // Let a later ticket try again. Leaving the flag set would make
                // one failed dial permanent for the life of the connection.
                //
                // Clearing it is also how `await_attach` learns the attempt is
                // over: it is holding `register_conn` open, and without the
                // notify it would sit out the entire backstop for a failure we
                // already know about.
                owned_conn.media_attaching.store(false, Ordering::SeqCst);
                owned_conn.media_gate.announce();
            }
        });
    if spawned.is_err() {
        conn.media_attaching.store(false, Ordering::SeqCst);
        conn.media_gate.announce();
    }
}

fn dial_and_serve(
    inner: &Arc<DaemonInner>,
    conn: &Arc<ConnShared>,
    dest: SocketAddr,
    ticket_b64: &str,
) -> Result<()> {
    if dest.port() == 0 {
        // `conn.rs` records port 0 for a peer whose advertised port we proved
        // false. It is not a port we can dial, and dialling 0 would fail with
        // an error naming nothing.
        bail!("no port recorded for this peer, so there is nothing to dial");
    }
    // Claimed before the socket exists, so a dial and an inbound attach racing
    // on the same connection cannot both install a link.
    let Some(claim) = AttachClaim::take(&conn.media_gate) else {
        bail!("a media link is already attached to this connection");
    };
    let mut s = TcpStream::connect_timeout(&dest, crate::conn::CONNECT_TIMEOUT)
        .with_context(|| format!("connect {dest}"))?;
    s.set_read_timeout(Some(crate::conn::HANDSHAKE_TIMEOUT))?;
    s.set_write_timeout(Some(crate::conn::WRITE_TIMEOUT))?;
    write_frame(&mut s, &ControlMsg::MediaAttach { ticket_b64: ticket_b64.to_string() })
        .context("send media_attach")?;
    match read_frame(&mut s).context("read media_attach reply")? {
        ControlMsg::Ok {} => {}
        ControlMsg::Error { message } => bail!("peer refused the media attach: {message}"),
        other => bail!("unexpected reply to media_attach: {other:?}"),
    }
    serve(inner, conn, s, claim)
}

/// One connection's media-link latch, and the condvar that announces its
/// `media_path` settling.
///
/// The two live together because they are the two halves of one question — "is
/// a link being installed, and has it finished?" — and every waiter needs both:
/// the flag says whether to keep waiting, the condvar says when to look again.
pub(crate) struct AttachGate {
    claimed: AtomicBool,
    /// Signalled on every write to `ConnShared::media_path`. Pairs with that
    /// `Mutex`, not with the flag above.
    settled: Condvar,
}

impl AttachGate {
    pub(crate) fn new() -> Arc<AttachGate> {
        Arc::new(AttachGate { claimed: AtomicBool::new(false), settled: Condvar::new() })
    }

    /// Wake everyone waiting for `media_path` to settle.
    pub(crate) fn announce(&self) {
        self.settled.notify_all();
    }
}

/// The right to install a media link on one connection, held for as long as the
/// install lasts and put back on **every** exit path.
///
/// # Why a latch and not a look
///
/// The check it replaces read `media_path` and released the lock, while the
/// install happened later, in [`serve`] — with a `set_nodelay`, a `try_clone`
/// and a thread spawn in between. Two attaches arriving together could both
/// pass the look and the second would overwrite the first's `media_path`,
/// leaving one link with a writer nobody reads and no counter anywhere saying
/// so. Check and install are one operation, so they are one CAS.
///
/// Released by `Drop`, which is the point: the install has half a dozen `?`
/// exits (socket options, clone, spawn) and a hand-written release would have
/// to name all of them. Missing one would not fail — it would make the
/// connection refuse every future attach, for its whole life.
pub(crate) struct AttachClaim(Arc<AttachGate>);

impl AttachClaim {
    /// `None` when an attach is already installed or being installed.
    fn take(gate: &Arc<AttachGate>) -> Option<AttachClaim> {
        gate.claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| AttachClaim(gate.clone()))
    }
}

impl Drop for AttachClaim {
    fn drop(&mut self) {
        self.0.claimed.store(false, Ordering::SeqCst);
        self.0.announce();
    }
}

/// Inbound half: spend a ticket and hand back the connection it belongs to.
///
/// Separate from [`serve`] so the caller can release its preauth slot in
/// between — serving lasts the life of the link, and a slot meant to bound
/// concurrent *handshakes* must not be held for it.
pub(crate) fn claim(
    inner: &Arc<DaemonInner>,
    s: &mut TcpStream,
    ticket_b64: &str,
) -> Result<(Arc<ConnShared>, AttachClaim)> {
    let refuse = |s: &mut TcpStream, why: &str| {
        let _ = write_frame(s, &ControlMsg::Error { message: why.into() });
    };
    let Some(fp) = claim_ticket(inner, ticket_b64) else {
        refuse(s, "unknown or expired media attach ticket");
        bail!("media_attach with an unknown or expired ticket");
    };
    let conn = lk(&inner.state).conns.get(&fp).cloned();
    let Some(conn) = conn.filter(|c| c.alive.load(Ordering::SeqCst)) else {
        refuse(s, "no live control connection for that ticket");
        bail!("media_attach for {fp}, which has no live control connection");
    };
    // **The ticket is not the only credential; the address is the other one.**
    //
    // Installing a link is not the write-only privilege the ticket's own
    // documentation used to claim. `serve` overwrites `conn.media_path`, so
    // whoever attaches receives this peer's entire media egress — the real peer
    // then hears nothing, and the attacker gets the ciphertext stream's timing
    // and lengths for free. It can also kill the control connection at will, by
    // closing the socket it was handed.
    //
    // So the attach is bound to the address the control handshake proved, for
    // exactly the reason `handle_datagram`'s `PullReq` arm refuses a keepalive
    // whose source is not `conn.peer_ip`. A ticket is a far stronger credential
    // than a cleartext header, but "stronger" is not "a reason to skip the
    // check".
    match s.peer_addr() {
        Ok(a) if a.ip() == conn.peer_ip => {}
        Ok(a) => {
            refuse(s, "media attach from an address that is not the control peer");
            bail!("media_attach for {fp} from {a}, whose control peer is {}", conn.peer_ip);
        }
        Err(e) => {
            refuse(s, "media attach socket has no peer address");
            bail!("media_attach for {fp}: peer_addr: {e}");
        }
    }
    let Some(claim) = AttachClaim::take(&conn.media_gate) else {
        refuse(s, "a media link is already attached");
        bail!("second media_attach for {fp} while one is already attached");
    };
    write_frame(s, &ControlMsg::Ok {}).context("ack media_attach")?;
    Ok((conn, claim))
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A sink that can be told to block, and that records what got through.
    ///
    /// Models the real socket rather than an ideal one: while blocked it
    /// consumes the `SO_SNDTIMEO` slice and reports `TimedOut`, exactly as a
    /// blocking socket with a send timeout does.
    struct FakeSink {
        blocked_until: Option<Instant>,
        written: Vec<Vec<u8>>,
        /// Bytes accepted per call, to exercise partial writes.
        chunk: usize,
        partial: Vec<u8>,
    }

    impl FakeSink {
        fn new(chunk: usize) -> FakeSink {
            FakeSink { blocked_until: None, written: Vec::new(), chunk, partial: Vec::new() }
        }
    }

    impl Write for FakeSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(t) = self.blocked_until {
                if Instant::now() < t {
                    std::thread::sleep(WRITE_SLICE);
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "blocked"));
                }
                self.blocked_until = None;
            }
            let n = self.chunk.min(buf.len());
            self.partial.extend_from_slice(&buf[..n]);
            if n == buf.len() {
                self.written.push(std::mem::take(&mut self.partial));
            }
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A 200-byte frame whose **first eight bytes are the moment it was
    /// queued**, in microseconds since a `t0` the whole test shares.
    ///
    /// Carrying the stamp in the payload rather than looking it up by index at
    /// the far end is not decoration; the first version of this test indexed a
    /// table by `payload[0]` and read the wrong row for every frame past 255,
    /// then reported a 3 s age that was pure arithmetic. And one shared `t0`
    /// rather than one per thread, because two `Instant::now()` calls on two
    /// threads are two epochs and their difference is not an age.
    fn frame_at(t0: Instant, at: Instant) -> Vec<u8> {
        let mut v = vec![0u8; 200];
        let us = at.duration_since(t0).as_micros() as u64;
        v[..8].copy_from_slice(&us.to_le_bytes());
        v
    }

    fn queued_us(buf: &[u8]) -> u64 {
        u64::from_le_bytes(buf[..8].try_into().unwrap())
    }

    fn push_at(link: &TcpMediaLink, t0: Instant, owner: &Arc<TxShared>) -> bool {
        let at = Instant::now();
        link.enqueue(at, owner, 100, |b| {
            b.clear();
            b.extend_from_slice(&frame_at(t0, at));
            true
        })
    }

    /// A frame filled with a recognisable constant, for the tests that care
    /// about identity rather than age.
    fn frame(n: usize) -> Vec<u8> {
        vec![(n & 0xFF) as u8; 200]
    }

    fn push(link: &TcpMediaLink, at: Instant, owner: &Arc<TxShared>, n: usize) -> bool {
        link.enqueue(at, owner, 100, |b| {
            b.clear();
            b.extend_from_slice(&frame(n));
            true
        })
    }

    fn link() -> TcpMediaLink {
        TcpMediaLink::new("fp".into(), "127.0.0.1:1".parse().unwrap())
    }

    /// The queue is bounded, drops the newest, and counts it — the shape
    /// `engine.rs`'s `UdpSender` argues for, reproduced here because the
    /// argument is about the transport's consequences, not about UDP.
    #[test]
    fn the_write_queue_is_bounded_and_drops_rather_than_waits() {
        let l = link();
        let owner = Arc::new(TxShared::new());
        let now = Instant::now();
        assert_eq!(l.capacity(), SEND_SLOTS);
        for i in 0..SEND_SLOTS + 5 {
            assert_eq!(push(&l, now, &owner, i), i < SEND_SLOTS, "slot {i} decided wrong");
        }
        assert_eq!(l.queued(), SEND_SLOTS, "the queue grew past its capacity");
        assert_eq!(l.dropped(), 5, "the overflow was not counted");
    }

    /// A voided fill (a failed seal) never becomes visible to the writer.
    #[test]
    fn a_failed_seal_never_reaches_the_wire() {
        let l = link();
        let owner = Arc::new(TxShared::new());
        assert!(!l.enqueue(Instant::now(), &owner, 0, |b| {
            b.clear();
            b.extend_from_slice(b"half");
            false
        }));
        assert_eq!(l.queued(), 0);
        assert_eq!(l.dropped(), 0, "voiding is not refusal; they are different events");
    }

    /// A dead link accepts nothing. Without this, a stream whose link died
    /// keeps filling a queue nobody drains, and the first thing anyone notices
    /// is the drop counter climbing for no visible reason.
    #[test]
    fn a_dead_link_refuses_new_frames() {
        let l = link();
        let owner = Arc::new(TxShared::new());
        assert!(push(&l, Instant::now(), &owner, 1));
        l.kill();
        assert!(!push(&l, Instant::now(), &owner, 2));
    }

    /// **Acceptance 3 (design §6, P3), the ratchet criterion.**
    ///
    /// A two second write stall, then release. Asserts:
    ///   (a) the backlog is back to zero shortly after the stall ends;
    ///   (b) `stale_dropped > 0` — the gate actually fired, rather than the
    ///       queue happening to be small enough to ride it out;
    ///   (c) **every frame that did reach the sink was fresh when it was
    ///       written**. This is the part that matters: a queue that drains is
    ///       worthless if what it drains is two seconds of stale audio.
    ///
    /// Injection control (run 2026-08-07): delete the `queued_at.elapsed() >
    /// STALE_BUDGET` arm in `write_loop` (always take the `write_one_frame`
    /// branch) and comment out the `off == 0 && now >= stale_at` return in
    /// `write_one_frame`. (b) goes red immediately and (c) goes red with frames
    /// aged ~2 s — which is the failure this design exists to prevent, stated
    /// as a number.
    #[test]
    fn a_two_second_write_stall_does_not_leave_stale_audio_queued() {
        let l = Arc::new(link());
        let owner = Arc::new(TxShared::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        // One epoch, shared by the producer that stamps each frame and the tap
        // that ages it.
        let t0 = Instant::now();

        let (report_tx, report_rx) = mpsc::channel::<Duration>();
        let wl = l.clone();
        let ws = shutdown.clone();
        let writer = std::thread::spawn(move || {
            let mut sink = FakeSink::new(4096);
            sink.blocked_until = Some(Instant::now() + Duration::from_secs(2));
            // Reports how old each frame was when the sink accepted its last
            // byte. The stamp rides in the payload, so no lookup and no second
            // clock epoch are involved.
            struct Tap<'a> {
                inner: &'a mut FakeSink,
                tx: mpsc::Sender<Duration>,
                t0: Instant,
            }
            impl Write for Tap<'_> {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    let n = self.inner.write(buf)?;
                    if n == buf.len() {
                        let age = self
                            .t0
                            .elapsed()
                            .saturating_sub(Duration::from_micros(queued_us(buf)));
                        let _ = self.tx.send(age);
                    }
                    Ok(n)
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut tap = Tap { inner: &mut sink, tx: report_tx, t0 };
            write_loop(&wl, &mut tap, &ws);
        });

        // Feed at roughly one frame per 10 ms for three seconds: one second
        // past the end of the stall, so the recovery is observed and not just
        // the stall.
        for _ in 0..300usize {
            push_at(&l, t0, &owner);
            l.wake();
            std::thread::sleep(Duration::from_millis(10));
        }

        // (a) the backlog is gone
        let deadline = Instant::now() + Duration::from_secs(5);
        while l.queued() > 0 && Instant::now() < deadline {
            l.wake();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(l.queued(), 0, "the queue never drained after the stall ended");

        shutdown.store(true, Ordering::SeqCst);
        l.wake();
        writer.join().expect("writer");

        // (b) the gate fired
        assert!(
            l.stale_dropped() > 0,
            "no frame was ever refused as stale, so nothing here tested the gate"
        );
        // ...and it was the gate, not the queue overflowing, that did the work:
        // 200 frames arrive during a 2 s stall and only 128 slots exist, so both
        // counters move — but the ones that matter are the ones the gate caught.
        assert!(l.frames_written() > 0, "nothing was ever written at all");

        // (c) nothing stale reached the sink
        let mut worst = Duration::ZERO;
        let mut seen = 0usize;
        while let Ok(age) = report_rx.try_recv() {
            worst = worst.max(age);
            seen += 1;
        }
        assert!(seen > 0, "no frame's age was ever reported, so (c) tested nothing");
        assert!(
            worst <= STALE_BUDGET + WRITE_SLICE * 3,
            "a frame reached the wire {worst:?} after it was queued; the stale gate is supposed \
             to cap that at {STALE_BUDGET:?} (plus at most one send slice of overshoot)"
        );
    }

    /// A frame with bytes already on the wire is finished even while the sink
    /// keeps timing out. Abandoning it mid-way would desynchronise the stream:
    /// the header carries the length, so the peer would read the *next* frame's
    /// bytes as this one's payload and never recover.
    #[test]
    fn a_partially_written_frame_is_always_completed() {
        let l = Arc::new(link());
        let owner = Arc::new(TxShared::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        // 1 byte per call, so a 200 byte frame needs 200 calls and cannot
        // possibly finish inside the stale budget.
        let mut sink = FakeSink::new(1);
        push(&l, Instant::now(), &owner, 7);
        let wl = l.clone();
        let ws = shutdown.clone();
        let h = std::thread::spawn(move || {
            let mut s = FakeSink::new(1);
            std::mem::swap(&mut s, &mut sink);
            write_loop(&wl, &mut s, &ws);
            s.written
        });
        std::thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::SeqCst);
        l.kill();
        let written = h.join().expect("writer");
        assert_eq!(written.len(), 1, "the frame was abandoned part-written");
        assert_eq!(written[0], frame(7), "the frame that arrived is not the frame that was sent");
        assert_eq!(l.stale_dropped(), 0, "a frame already on the wire was counted as stale");
    }

    /// A ticket is single use, scoped to one peer, and dies with its TTL.
    #[test]
    fn a_ticket_is_single_use() {
        let mut store: Vec<MediaTicket> = Vec::new();
        let bytes = [7u8; TICKET_LEN];
        store.push(MediaTicket {
            bytes,
            fp: "aa11".into(),
            expires: Instant::now() + TICKET_TTL,
        });
        let b64 = BASE64_STANDARD.encode(bytes);
        let raw = BASE64_STANDARD.decode(&b64).unwrap();
        let idx = store
            .iter()
            .position(|x| bool::from(subtle::ConstantTimeEq::ct_eq(&x.bytes[..], &raw[..])));
        assert_eq!(idx, Some(0));
        store.swap_remove(0);
        assert!(store.is_empty(), "spending a ticket must remove it");
    }

    // ------------------------------------------------- source guards

    /// This file's **production text**: comments stripped, test module cut off.
    ///
    /// Both halves are load-bearing and this repository has been bitten by
    /// missing either. Comments must go because a guard spelled
    /// `!contains("...")` is satisfied by commenting the subject line out — the
    /// function is gone, the substring is not. The test module must go because
    /// every assertion below writes the very substring it is looking for, as a
    /// string literal, in its own failure message: without the cut, this file's
    /// guards find themselves and pass (or, as on the first run, fail for a
    /// reason that has nothing to do with the code).
    fn code() -> String {
        let src = include_str!("tcpmedia.rs");
        let cut = src
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("the test module's start marker moved; code() would scan the tests too");
        crate::engine::tests::strip_comments(&src[..cut])
    }

    /// The cut and the strip both actually happen. Every `contains` guard below
    /// is vacuous until this passes.
    #[test]
    fn the_source_guard_sees_only_production_text() {
        let c = code();
        assert!(c.contains("fn write_loop"), "code() removed the code as well");
        assert!(
            !c.contains("the stale gate is supposed to cap"),
            "code() still contains this module's assertion messages, so every guard below is \
             matching against its own text"
        );
        assert!(
            !c.contains("A kernel send buffer is"),
            "code() did not strip comments, so a commented-out subject line would satisfy the \
             guards below"
        );
    }

    /// **Acceptance 4 (design §6, P3): `set_nodelay` failure must refuse the
    /// link, not be swallowed.**
    ///
    /// `secure.rs` writes `let _ = s.set_nodelay(true)` on the control plane and
    /// that is acceptable at ~1 Hz. On the media plane it is not: Nagle
    /// coalesces 10 ms frames into ~40 ms ACK-bound bursts, and the symptom is
    /// jitter with no source anywhere in our numbers.
    ///
    /// Guards the source because the failure needs a socket that refuses the
    /// option — there is no such socket to hand. Comments are stripped first;
    /// this repository's grep guards have been fooled by commenting the
    /// subject line out before.
    #[test]
    fn the_media_socket_refuses_to_run_with_nagle_enabled() {
        let src = code();
        let lines: Vec<&str> = src.lines().filter(|l| l.contains("set_nodelay(")).collect();
        assert!(!lines.is_empty(), "set_nodelay disappeared from the media socket entirely");
        for l in lines {
            assert!(
                !l.contains("let _"),
                "the media socket ignores set_nodelay's result: {l}\nNagle would then be on with \
                 nothing to say so"
            );
            assert!(
                l.contains('?'),
                "set_nodelay's failure is not propagated: {l}\nA link we cannot disable Nagle on \
                 must be refused, not promoted"
            );
        }
    }

    /// The stale gate is in `write_loop` and reads the frame's own queue time.
    ///
    /// The behavioural test above is the real one; this catches the shape of
    /// the regression it cannot — a "simplification" that keeps a gate but
    /// times it from something that is not the frame's own age (the current
    /// tick, the batch start, a fixed counter), which stays green on a stall
    /// short enough to fit in one batch.
    #[test]
    fn the_stale_gate_times_each_frame_from_when_that_frame_was_queued() {
        let src = code();
        let at = src.find("fn write_loop").expect("write_loop is gone");
        let body = &src[at..];
        let end = body.find("\n}\n").expect("write_loop has no end");
        let body = &body[..end];
        assert!(
            body.contains("slot.queued_at"),
            "write_loop no longer reads the frame's own queue time, so whatever it is gating on \
             is not that frame's age"
        );
        assert!(
            body.contains("STALE_BUDGET"),
            "the stale gate is gone from write_loop; the ratchet has no hard bound left"
        );
        assert!(
            body.contains("stale_dropped.fetch_add"),
            "frames are dropped without being counted, which is the observability hole the whole \
             design says not to reopen"
        );
    }

    /// **The attach latch is taken once and put back on every exit.**
    ///
    /// Covers what the end-to-end refusal test cannot: that test asserts a
    /// second attach is refused, and the `matches!(media_path, Tcp)` look this
    /// replaced refused it too — in the *unraced* case. What the look could not
    /// do is refuse an attach that arrives while the first one is between its
    /// check and `serve`'s write of `media_path`, which is a `set_nodelay`, a
    /// `try_clone` and a thread spawn wide. A CAS has no such interval, and a
    /// test cannot demonstrate the absence of an interval; it can only pin the
    /// mechanism that has none.
    ///
    /// The `Drop` half is the one worth a test on its own. Getting it wrong
    /// does not fail — it makes the connection refuse every future attach for
    /// the rest of its life, which reads exactly like a peer that stopped
    /// wanting tier 1.
    #[test]
    fn the_attach_latch_admits_one_holder_and_is_returned_when_it_is_dropped() {
        let gate = AttachGate::new();
        let first = AttachClaim::take(&gate).expect("the first claim must succeed");
        assert!(
            AttachClaim::take(&gate).is_none(),
            "two holders got the right to install a link on one connection; the second would \
             overwrite the first's media_path and leave its writer with no reader"
        );
        drop(first);
        assert!(
            AttachClaim::take(&gate).is_some(),
            "the latch was not returned, so this connection now refuses every attach it will \
             ever be offered — which looks exactly like a peer that stopped wanting tier 1"
        );
    }

    /// **An attach is bound to the address the control handshake proved.**
    ///
    /// Guarded on the source because it cannot be guarded on behaviour here:
    /// every socket in a loopback test comes from 127.0.0.1, which is also
    /// every `conn.peer_ip`, so a build with the check and a build without it
    /// are indistinguishable to any test this repository can run unprivileged.
    ///
    /// Why the check has to exist at all — the design's own description of the
    /// ticket ("whoever steals one gets to inject bytes that fail AEAD") turned
    /// out to understate it. `serve` *replaces* `conn.media_path`, so attaching
    /// takes over the peer's whole media egress: the real peer goes silent, the
    /// holder gets the ciphertext stream's timing and lengths, and it can drop
    /// the control connection whenever it likes by closing the socket.
    /// `handle_datagram` refuses a `PullReq` from the wrong source IP for the
    /// weaker version of the same reason.
    #[test]
    fn an_inbound_attach_must_come_from_the_control_peers_address() {
        let src = code();
        let at = src.find("pub(crate) fn claim(").expect("claim is gone");
        let end = at + src[at..].find("\n}\n").expect("claim has no end");
        let body = &src[at..end];
        assert!(
            body.contains("peer_addr()"),
            "claim no longer looks at where the attach came from, so any host that gets hold of \
             a ticket can take over this peer's media egress"
        );
        assert!(
            body.contains("conn.peer_ip"),
            "claim reads the attaching socket's address but never compares it with the address \
             the control handshake proved; reading it without comparing it is a diagnostic, not \
             a check"
        );
        // ...and that it decides by taking the latch, not by looking at
        // `media_path` and installing later. The unit test above pins what the
        // latch does; this pins that `claim` is the thing using it.
        assert!(
            body.contains("AttachClaim::take("),
            "claim decides whether a link is already attached by some means other than taking \
             the latch, so the check and the install are two operations again"
        );
    }

    /// **The deadline-thread ban list follows `tx_loop`'s calls into this
    /// file.**
    ///
    /// `engine.rs`'s guard scans three function bodies, all of them in
    /// `engine.rs`. But `tx_loop` calls [`TcpMediaLink::enqueue`] and
    /// [`TcpMediaLink::wake`] **synchronously**, and they live here — so the
    /// ban stops at the file boundary and everything past it is unguarded.
    /// Whatever these two do happens on the 10 ms deadline thread just as much
    /// as if it had been written inline.
    ///
    /// Uses `engine`'s table rather than a copy of it. A copy is the failure
    /// this whole guard family exists to prevent, one level up: somebody adds a
    /// transport, adds a row over there, and this side keeps passing because it
    /// is checking yesterday's list.
    ///
    /// Injection control (run 2026-08-08): put `let _ = w.write(b"x");` in
    /// `enqueue` ⇒ red, naming `write(`. Comment the same line out ⇒ green,
    /// because [`code`] strips comments first.
    #[test]
    fn the_queueing_calls_tx_loop_makes_into_this_file_are_guarded_too() {
        let src = code();
        for f in ["fn enqueue(", "fn wake("] {
            let at = src.find(f).unwrap_or_else(|| {
                panic!("{f} is gone from tcpmedia.rs; tx_loop's entry point moved and this guard \
                        is now checking nothing")
            });
            let open = at + src[at..].find(" {\n").expect("no signature end") + 3;
            let end = open + src[open..].find("\n    }\n").expect("no function end");
            let body = &src[open..end];
            assert!(!body.is_empty(), "{f}'s body came out empty, so every check below is vacuous");
            for (needle, why) in crate::engine::deadline_thread_guards::BANNED_ON_THE_DEADLINE_THREAD
            {
                assert!(
                    !body.contains(needle),
                    "tcpmedia's `{f}` contains `{needle}` — {why}.\nIt is called synchronously \
                     from tx_loop, so this is the 10 ms deadline thread; the write belongs in \
                     write_loop."
                );
            }
        }
    }

    /// Exactly one producer and one consumer on the tier 1 queue — the entire
    /// basis for `SpscRing`'s `unsafe`.
    #[test]
    fn the_tier_one_queue_has_exactly_one_producer_and_one_consumer() {
        let src = code();
        assert_eq!(
            src.matches("self.q.produce(").count(),
            1,
            "a second producer on the tier 1 queue voids SpscRing's safety precondition"
        );
        assert_eq!(
            src.matches("link.q.consume(").count(),
            1,
            "a second consumer on the tier 1 queue voids SpscRing's safety precondition"
        );
    }
}
