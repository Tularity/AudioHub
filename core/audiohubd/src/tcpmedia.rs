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
/// # Why 440 ms — and why it moved from 200 in P4
///
/// It is bracketed by two numbers **the receiver of these frames actually lives
/// by**, and on tier 1 that is `JbTuning::DEGRADED`, not `JbTuning::DEFAULT`:
///
/// - `max_target = 40` frames = **400 ms** is the deepest steady-state target
///   the jitter buffer will aim for. A frame older than that has already missed
///   the slot it was going to be played in, whatever the receiver does.
/// - `max_frames = 48` frames = **480 ms** is the hard ceiling at which the
///   jitter buffer itself starts discarding the oldest. Dropping *below* that
///   keeps the decision — and the counter — on the side that can explain it.
///   Above it, the same audio would still be discarded, silently, by the peer.
///
/// **The profile is the whole argument, so getting it wrong is not a tuning
/// error, it is a subject error.** P3 bracketed this budget by
/// `JbTuning::DEFAULT` because tier 1 used `DEFAULT`; P4 gives tier 1 its own
/// profile, and a 200 ms budget under a 400 ms target would throw away audio
/// the receiver was going to play — the exact failure the lower bound exists to
/// prevent. The assertion below reads the profile rather than a literal, so
/// this cannot drift again without failing to compile.
pub(crate) const STALE_BUDGET: Duration = Duration::from_millis(440);

/// The two numbers the budget above is bracketed by, **read from the jitter
/// buffer profile tier 1 receivers run** rather than copied.
///
/// The first version of this assertion spelled them `12 * 10` and `24 * 10`.
/// It went red when `STALE_BUDGET` moved and stayed green when the tuning
/// moved — which is backwards, because the failure message asserts a
/// *relationship* between them, and only one side of that relationship was
/// actually being read. Verified by mutation on 2026-08-08: with the literals,
/// dropping `max_frames` to 16 compiled clean; with the constants below it
/// fails to compile.
///
/// The subject is [`JB_PROFILE`], and that binding is the second half of the
/// same lesson: P4 introduced `DEGRADED` while this assertion still named
/// `DEFAULT`, so for one edit the gate was bracketed by a profile nothing on
/// this transport uses. `tier1_jb_tuning` and this constant now read the same
/// name, and `the_stale_gate_is_bracketed_by_the_profile_tier_1_receivers_use`
/// asserts they still do.
const JB_PROFILE: audiohub_net::media::JbTuning = audiohub_net::media::JbTuning::DEGRADED;
const JB_DEEPEST_TARGET_MS: u64 = JB_PROFILE.max_target as u64 * crate::engine::FRAME_MS;
const JB_HARD_CEILING_MS: u64 = JB_PROFILE.max_frames as u64 * crate::engine::FRAME_MS;

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
pub(crate) const WRITE_SLICE: Duration = Duration::from_millis(20);

/// Same, for the reader — it only needs to notice shutdown.
pub(crate) const READ_SLICE: Duration = Duration::from_millis(200);

/// A frame that has already put bytes on the wire **must** be finished, or the
/// stream desynchronises and every following byte is misread. So the stale gate
/// cannot apply mid-frame, and this is the backstop instead: a frame that
/// cannot be completed in this long means the connection is gone, whatever the
/// socket believes.
pub(crate) const FRAME_COMPLETION_LIMIT: Duration = Duration::from_secs(5);

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
    /// Tier 2: the one multiplexed connection, shared with the control stream.
    ///
    /// Holds the mux rather than a [`TcpMediaLink`] directly even though the
    /// media queue inside it is the same type, because the two are not
    /// interchangeable anywhere it matters: killing a tier 1 link ends a media
    /// connection, killing a mux ends the control channel with it.
    Framed(Arc<crate::mux::MuxLink>),
}

impl MediaPath {
    /// The highest rung AUTO may occupy on this transport (plan §16.3).
    ///
    /// **AUTO's ceiling is a property of the transport, not a global
    /// constant.** On a degraded link the deep rungs cost more (protocol
    /// overhead plus head-of-line blocking) for a difference nobody can hear,
    /// and the user's ruling is that a degraded link prioritises *having sound*
    /// over having deep sound.
    pub(crate) fn auto_top_rung(&self) -> u32 {
        match self {
            MediaPath::Udp(_) => audiohub_net::media::AUTO_TOP_RUNG,
            MediaPath::Tcp(_) | MediaPath::Framed(_) => {
                audiohub_net::media::AUTO_TOP_RUNG_STREAMED
            }
        }
    }

    /// The UDP destination, or `None` on a transport that has none. Callers use
    /// this to skip UDP-only work; nobody may invent an address for the `None`
    /// case.
    ///
    /// On [`MediaPath::Framed`] the absence is not merely "we would rather
    /// not": `conn.peer_ip` is the **tunnel's** address and `peer.port` is a
    /// number the peer advertised about a listener the tunnel does not expose,
    /// so the address that would have been computed here is a well-formed
    /// address belonging to somebody else. That is the failure this enum
    /// replaced, and it is why the variant carries no `SocketAddr` at all.
    pub(crate) fn udp_dest(&self) -> Option<SocketAddr> {
        match self {
            MediaPath::Udp(a) => Some(*a),
            MediaPath::Tcp(_) | MediaPath::Framed(_) => None,
        }
    }

    /// The frame queue this path writes media into, on the transports that have
    /// one. `None` on tier 0, where the queue is the shared `UdpSender`.
    pub(crate) fn media_link(&self) -> Option<&Arc<TcpMediaLink>> {
        match self {
            MediaPath::Udp(_) => None,
            MediaPath::Tcp(l) => Some(l),
            MediaPath::Framed(m) => Some(m.media()),
        }
    }
}

impl std::fmt::Debug for MediaPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaPath::Udp(a) => write!(f, "udp({a})"),
            MediaPath::Tcp(l) => write!(f, "tcp({})", l.peer),
            MediaPath::Framed(m) => write!(f, "mux({})", m.media().peer),
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
    ///
    /// Deferred rather than required at construction, because tier 2 builds the
    /// queue **before** the handshake that names the peer — the mux is what
    /// carries that handshake. Written exactly once, by whichever side learns
    /// the name first; the alternative (leaving it empty on tier 2) would make
    /// every tier 2 link share one key in the per-tick maps that index on it,
    /// so two degraded peers would report each other's backlog.
    fp: OnceLock<String>,
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
    /// How long the frame the writer most recently picked up had been waiting,
    /// in microseconds. **This is `writeq_ms`** — AUTO's primary demote signal
    /// on this transport (design §3.4 signal 1).
    wait_us_last: AtomicU64,
    /// The same, running maximum since the link came up. For the status page,
    /// which needs a number that a 1 Hz poll cannot miss.
    wait_us_peak: AtomicU64,
    /// The same, running maximum since the **last read**. `swap(0)` by the
    /// ticker, once per link per tick.
    ///
    /// # Why a peak and not the instantaneous value
    ///
    /// AUTO samples at 1 Hz and the queue drains in tens of milliseconds, so an
    /// instantaneous read is a coin flip about whether it lands on the backlog.
    /// That is the same mistake `RateWindow` was written to undo, one rung
    /// down: a measure whose window does not cover the event it is measuring.
    wait_us_window: AtomicU64,
    /// The last value [`TcpMediaLink::take_writeq_peak_ms`] handed out, i.e.
    /// **exactly what AUTO saw** on its last evaluation.
    ///
    /// Exposed rather than inferred: the window peak is consumed by the taker,
    /// so without this the status page could only show a quantity nobody steers
    /// on, and "why did it not promote" would have no answer anywhere.
    wait_us_taken: AtomicU64,
    /// Optional test-only rate limit, bytes per second, 0 = off. See
    /// [`token_bucket_from_env`].
    tx_bps: u64,
}

/// Bytes/second the writer may put on the wire, from `AUDIOHUB_TEST_TX_KBPS`.
/// `0` (the default, and any unparseable value) = unlimited.
///
/// # Why a token bucket in the product binary rather than a real constriction
///
/// The acceptance criterion for AUTO on tier 1 is "starve the link and watch
/// the rung come down". Doing that for real needs a traffic shaper — `dnctl`
/// on macOS, QoS policy on Windows — which means root, a system-wide
/// configuration change, and two platform-specific scripts that cannot run
/// against the daemon serving the user's audio. This is **zero system
/// configuration**: one environment variable, one process, no privileges, and
/// identical behaviour on both platforms.
///
/// It sits on the writer thread, i.e. **after** the queue and the stale gate,
/// so everything downstream of it — backlog, `writeq_ms`, the gate firing,
/// AUTO's reaction — is the production path unmodified. What it does *not*
/// reproduce is a congested network: no retransmissions, no RTT growth, no
/// receiver-side spread. That is the honest limit of it, and it is why the
/// receive-side signal is measured on a real cross-machine link instead.
///
/// **Accuracy is approximate and slightly conservative.** It waits with
/// `thread::sleep`, whose overshoot at a per-frame cost of 4–14 ms is a few
/// percent; measured 2026-08-08 at a nominal 400 kbps, the link carried
/// **362.7 kbps** of datagram bytes over a 39 s steady-state window — 91% of
/// the budget. Tests must therefore compare against a threshold with margin,
/// never treat the nominal figure as an exact link capacity.
pub(crate) fn token_bucket_from_env() -> u64 {
    std::env::var("AUDIOHUB_TEST_TX_KBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|kbps| kbps * 1000 / 8)
        .unwrap_or(0)
}

impl TcpMediaLink {
    /// Reachable from `mux` as well: tier 2's media half is this queue, this
    /// stale gate and these counters, driven by a different writer.
    pub(crate) fn new(fp: String, peer: SocketAddr, tx_bps: u64) -> TcpMediaLink {
        let name = OnceLock::new();
        if !fp.is_empty() {
            let _ = name.set(fp);
        }
        TcpMediaLink {
            fp: name,
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
            wait_us_last: AtomicU64::new(0),
            wait_us_peak: AtomicU64::new(0),
            wait_us_window: AtomicU64::new(0),
            wait_us_taken: AtomicU64::new(0),
            tx_bps,
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

    /// One `Kind::Media` frame arrived. Counted on the way in, **before** the
    /// AEAD has a say, which is why `SessionStats.auth_failed` exists beside
    /// it: `frames_read >= received` would otherwise be green while injected
    /// traffic made up the difference.
    pub(crate) fn note_frame_read(&self) {
        self.frames_read.fetch_add(1, Ordering::Relaxed);
    }

    /// A frame arrived whose `Kind` does not belong on this transport.
    pub(crate) fn note_unexpected_kind(&self) {
        self.unexpected_kind.fetch_add(1, Ordering::Relaxed);
    }

    /// The test-only rate limit this queue was built with
    /// ([`token_bucket_from_env`]), so a writer that does not own the queue can
    /// still build the same bucket.
    pub(crate) fn tx_bps(&self) -> u64 {
        self.tx_bps
    }

    /// How long the most recently dequeued frame waited, in milliseconds.
    ///
    /// # What this measures, and why it is a wait and not a depth
    ///
    /// The design called the quantity `writeq_ms` and the natural reading is
    /// "queue depth converted to time". That conversion does not exist here:
    /// [`TcpMediaLink::queued`] counts **wire packets**, and one packet is 10 ms
    /// of audio on the shallow rungs but 5 ms on the deep ones (they split each
    /// frame in two), while the queue is shared by **every stream to this
    /// peer** — so `queued × FRAME_MS` is wrong by a factor of the live stream
    /// count *and* by a factor of two, and both factors move at runtime. A
    /// number that silently changes meaning when a second stream opens is worse
    /// than no number.
    ///
    /// The wait needs neither factor. Every slot already carries the instant it
    /// was queued (the stale gate's input), so the writer knows exactly how long
    /// the frame it is holding sat in the queue — one subtraction it was already
    /// performing. That figure is directly comparable to [`STALE_BUDGET`], which
    /// is the other thing on this transport measured in the same unit.
    pub(crate) fn writeq_ms(&self) -> f64 {
        self.wait_us_last.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Peak wait since the link came up, milliseconds. Never reset.
    pub(crate) fn writeq_peak_ms(&self) -> f64 {
        self.wait_us_peak.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Peak wait since the previous call, milliseconds. **Consuming** — exactly
    /// one caller per link per tick (the ticker), and the status page reads
    /// [`TcpMediaLink::writeq_peak_ms`] instead precisely so that two readers
    /// never share one consuming counter.
    pub(crate) fn take_writeq_peak_ms(&self) -> f64 {
        let us = self.wait_us_window.swap(0, Ordering::Relaxed);
        self.wait_us_taken.store(us, Ordering::Relaxed);
        us as f64 / 1000.0
    }

    /// What AUTO saw last time it looked. Non-consuming.
    pub(crate) fn writeq_auto_ms(&self) -> f64 {
        self.wait_us_taken.load(Ordering::Relaxed) as f64 / 1000.0
    }

    fn note_wait(&self, waited: Duration) {
        let us = waited.as_micros().min(u64::MAX as u128) as u64;
        self.wait_us_last.store(us, Ordering::Relaxed);
        self.wait_us_peak.fetch_max(us, Ordering::Relaxed);
        self.wait_us_window.fetch_max(us, Ordering::Relaxed);
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The peer's fingerprint, or `""` before it is known (tier 2, between the
    /// socket coming up and the handshake completing on it).
    pub(crate) fn fp(&self) -> &str {
        self.fp.get().map_or("", String::as_str)
    }

    /// Name the peer this queue belongs to. Called once, from
    /// `conn::register_conn`, on the tier 2 path where the queue predates the
    /// name. A second call is ignored — the first name is the right one.
    pub(crate) fn name_peer(&self, fp: &str) {
        let _ = self.fp.set(fp.to_string());
    }

    /// A link with no socket behind it, for tests that need a
    /// [`MediaPath::Tcp`] to exist rather than to carry anything.
    #[cfg(test)]
    pub(crate) fn new_for_test(fp: String, peer: SocketAddr) -> TcpMediaLink {
        TcpMediaLink::new(fp, peer, 0)
    }

    pub(crate) fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.get() {
            t.unpark();
        }
    }

    /// Register the calling thread as the one [`TcpMediaLink::wake`] unparks.
    /// Called once by whichever writer owns this queue — [`write_loop`] on tier
    /// 1, `mux::write_loop` on tier 2.
    pub(crate) fn adopt_writer_thread(&self) {
        let _ = self.thread.get_or_init(std::thread::current);
    }

    /// Park until woken or `timeout`, unless the queue already has work. The
    /// `parked` flag is set before the re-check, pairing with
    /// [`TcpMediaLink::wake`]'s publish-then-read; skipping the re-check is the
    /// classic lost wakeup, and its symptom is one frame arriving a park slice
    /// late with nothing to point at.
    pub(crate) fn park_writer(&self, timeout: Duration, also_ready: impl Fn() -> bool) {
        self.parked.store(true, Ordering::SeqCst);
        if self.q.len() == 0 && !also_ready() {
            std::thread::park_timeout(timeout);
        }
        self.parked.store(false, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ write path

/// Why a frame did not go out whole.
#[derive(Debug, PartialEq)]
pub(crate) enum WriteOutcome {
    Sent,
    /// Aged past its budget before a single byte reached the socket. The frame
    /// is dropped; the `seq` hole it leaves is what the receiver conceals.
    Stale,
    /// The connection is finished.
    Dead,
}

pub(crate) fn blocked(k: ErrorKind) -> bool {
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
pub(crate) fn write_one_frame<W: Write>(
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

/// Test-only rate limit, expressed as "the earliest instant the link could
/// accept another byte". Off (`bps == 0`) it compiles to two predictable
/// branches and never touches the clock.
///
/// No burst allowance on purpose: a bucket that hands out a second of credit
/// up front would let the first second of a measurement run at full speed and
/// the backlog would appear only afterwards, which is precisely the shape that
/// makes a 60 s acceptance window ambiguous.
pub(crate) struct TokenBucket {
    bps: u64,
    next_at: Option<Instant>,
}

impl TokenBucket {
    pub(crate) fn new(bps: u64) -> TokenBucket {
        TokenBucket { bps, next_at: None }
    }

    /// Block until the simulated link is free. Sliced so shutdown is still
    /// noticed promptly; the queue backing up behind this is the point.
    pub(crate) fn gate(&mut self, shutdown: &AtomicBool) {
        if self.bps == 0 {
            return;
        }
        while let Some(t) = self.next_at {
            let now = Instant::now();
            if now >= t || shutdown.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep((t - now).min(WRITE_SLICE));
        }
    }

    pub(crate) fn charge(&mut self, bytes: usize) {
        if self.bps == 0 {
            return;
        }
        let cost = Duration::from_secs_f64(bytes as f64 / self.bps as f64);
        let base = self.next_at.unwrap_or_else(Instant::now).max(Instant::now());
        self.next_at = Some(base + cost);
    }
}

/// Take **one** queued frame and put it on the wire, applying the stale gate
/// and the accounting. `None` = the queue was empty.
///
/// Split out of [`write_loop`] rather than inlined there because tier 2's
/// writer (`mux::write_loop`) has to interleave control frames between media
/// frames, and it must interleave them into *this* gate rather than a second
/// copy of it. A copy is what the guard at the bottom of this file exists to
/// prevent one directory over; it would be a poor answer here.
///
/// `give_up_at` caps the pre-first-byte deadline. Tier 1 passes `None` and gets
/// the stale budget alone; the mux passes the instant its control credit falls
/// due, because otherwise a media frame blocked in `write` holds the wire for
/// **up to a whole stale budget** and the credit — which is only ever checked
/// between frames — silently becomes "100 ms plus however long one frame
/// blocks".
///
/// **A defensive bound, not a measured fix.** An earlier version of this
/// docblock attributed it to an observed 5-second round trip on a saturated
/// tier 2 link. That attribution does not survive review: the rig that produced
/// it was rate-limiting *downstream* of the socket, which parks the backlog in
/// the kernel send buffer where nothing here can reorder it and which produced
/// eight-second round trips with the scheduler working perfectly (see
/// `transport_tests::tier_two_pair`). The two are most likely the same artefact
/// counted twice. What survives is the mechanism above, which is an argument
/// about this function rather than about a network, and which
/// `the_cap_gives_up_on_a_blocked_frame_before_the_stale_budget_would` pins.
/// The window where the cap is the deciding factor is genuinely narrow — the
/// frame must still be fresh *and* the send window fully shut — so it is
/// carried as a bound on the worst case, not as a routine optimisation.
///
/// It caps only the deadline that applies while **nothing has been written**.
/// Once a byte is out the frame must be completed whatever else is waiting —
/// the header carries the length, so an abandoned frame desynchronises the
/// stream.
pub(crate) fn write_one_queued<W: Write>(
    link: &TcpMediaLink,
    w: &mut W,
    shutdown: &AtomicBool,
    bucket: &mut TokenBucket,
    give_up_at: Option<Instant>,
) -> Option<WriteOutcome> {
    let mut seen = None;
    link.q.consume(|slot| {
        let owner = slot.owner.take(); // dropped on THIS thread
        let queued_at = slot.queued_at;
        // The stale gate's subtraction, reused as the backlog gauge. One
        // reading per dequeue covers a stalled writer too: with
        // `SO_SNDTIMEO` at `WRITE_SLICE`, a blocked write returns, the
        // frame ages out, the gate drops it and the next frame is dequeued
        // already old — so the gauge climbs towards `STALE_BUDGET` rather
        // than freezing at whatever it read before the stall.
        bucket.gate(shutdown);
        let waited = queued_at.elapsed();
        link.note_wait(waited);
        let stale_at = match give_up_at {
            Some(cap) => cap.min(queued_at + STALE_BUDGET),
            None => queued_at + STALE_BUDGET,
        };
        let outcome = if waited > STALE_BUDGET || Instant::now() >= stale_at {
            WriteOutcome::Stale
        } else {
            write_one_frame(
                w,
                &slot.buf,
                stale_at,
                queued_at + FRAME_COMPLETION_LIMIT,
                shutdown,
            )
        };
        match outcome {
            WriteOutcome::Sent => {
                // Only bytes that reached the wire spend the budget: a frame
                // the gate dropped never occupied the link.
                bucket.charge(slot.buf.len());
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
            WriteOutcome::Dead => {}
        }
        seen = Some(outcome);
    });
    seen
}

/// The writer thread's body, generic over the sink so the ratchet property can
/// be tested against a writer that blocks on command rather than against a
/// network.
///
/// Returns when the link is dead or the daemon is shutting down.
fn write_loop<W: Write>(link: &TcpMediaLink, w: &mut W, shutdown: &AtomicBool) {
    link.thread.get_or_init(std::thread::current);
    let mut bucket = TokenBucket::new(link.tx_bps);
    loop {
        // Drain everything queued, applying the gate to each frame as it comes
        // off — not once per batch. A batch can span the whole budget.
        let mut fatal = false;
        while let Some(outcome) = write_one_queued(link, w, shutdown, &mut bucket, None) {
            if outcome == WriteOutcome::Dead {
                fatal = true;
            }
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
                dlog!("[audiohubd] tier1 media {}: peer closed the media connection", link.fp());
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if blocked(e.kind()) => continue,
            Err(e) => {
                dlog!("[audiohubd] tier1 media {}: read: {e}", link.fp());
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
                        dlog!("[audiohubd] tier1 media {}: framing: {e}", link.fp());
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

    let link = Arc::new(TcpMediaLink::new(conn.fp.clone(), peer, inner.tx_bps));
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
    match *lk(&conn.media_path) {
        // already attached; a second link would be a second writer
        MediaPath::Tcp(_) => return,
        // Tier 2 has no second connection to offer. The address a ticket would
        // be redeemed at is `conn.peer_ip` — the tunnel's — plus a port the
        // peer advertised about a listener the tunnel does not expose, i.e.
        // well-formed and somebody else's, which is the failure
        // `MediaPath::Framed` carries no `SocketAddr` to prevent. Today nothing
        // reaches here on tier 2 because `register_conn` skips `negotiate`
        // when a mux is present, but that guard is two files away and is about
        // the mux rather than about the path; this one is about the path.
        MediaPath::Framed(_) => return,
        MediaPath::Udp(_) => {}
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
    // The dial below computes `SocketAddr::new(conn.peer_ip, conn.peer.port)`,
    // which on tier 2 is the tunnel's address and a port belonging to a
    // listener behind it. Refused on the path rather than on the tier, because
    // the path is what the dial would corrupt: a link that came up here
    // overwrites `media_path` with `Tcp`, and its teardown writes back
    // `MediaPath::Udp(...)` — synthesising, on a connection that reached us
    // through a tunnel, exactly the UDP destination `Framed` exists to deny.
    if matches!(*lk(&conn.media_path), MediaPath::Framed(_)) {
        dlog!(
            "[audiohubd] ignoring a media attach ticket from {}: this connection is multiplexed, \
             so there is no address to redeem it at",
            conn.fp
        );
        return;
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
        TcpMediaLink::new("fp".into(), "127.0.0.1:1".parse().unwrap(), 0)
    }

    /// `give_up_at` really does shorten the pre-first-byte deadline.
    ///
    /// The parameter had **no coverage at all**: only `mux::write_loop` passes
    /// it non-`None`, no test drives that loop, and every other call site
    /// passes `None` — so deleting the whole `give_up_at` branch left the suite
    /// green. This is the narrow window in which it is the deciding factor: the
    /// frame is fresh, so `STALE_BUDGET` is far away, and the send window is
    /// shut, so nothing has reached the wire. Capped, the writer gives up at the
    /// cap and goes to serve control; uncapped, it holds the wire for the whole
    /// 440 ms stale budget and the control credit becomes "100 ms plus however
    /// long one frame blocks".
    ///
    /// Giving up here is invisible to the peer — not one byte was written, so
    /// the `seq` hole is indistinguishable from the loss it is reported as.
    #[test]
    fn the_cap_gives_up_on_a_blocked_frame_before_the_stale_budget_would() {
        let l = link();
        let owner = Arc::new(TxShared::new());
        let shutdown = AtomicBool::new(false);

        // Blocked far past both candidate deadlines, so the only thing that can
        // end the wait is a deadline rather than the sink relenting.
        let mut sink = FakeSink::new(4096);
        sink.blocked_until = Some(Instant::now() + STALE_BUDGET * 4);

        let queued_at = Instant::now();
        assert!(push(&l, queued_at, &owner, 0));

        let cap = queued_at + Duration::from_millis(60);
        let mut bucket = TokenBucket::new(0);
        let t0 = Instant::now();
        let out = write_one_queued(&l, &mut sink, &shutdown, &mut bucket, Some(cap));
        let waited = t0.elapsed();

        assert_eq!(out, Some(WriteOutcome::Stale), "a frame that never got a byte out must be dropped");
        assert!(
            waited < STALE_BUDGET / 2,
            "the writer held the wire for {waited:?}: the cap was ignored and the frame ran to \
             the {STALE_BUDGET:?} stale budget instead"
        );
        assert_eq!(sink.written.len(), 0, "a frame the gate dropped reached the wire");
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

    /// The stale gate reads the frame's own queue time, and **both** writers go
    /// through it.
    ///
    /// The behavioural test above is the real one; this catches the shape of
    /// the regression it cannot — a "simplification" that keeps a gate but
    /// times it from something that is not the frame's own age (the current
    /// tick, the batch start, a fixed counter), which stays green on a stall
    /// short enough to fit in one batch.
    ///
    /// # Why the subject moved from `write_loop` to `write_one_queued`
    ///
    /// P5 needed the gate to run between interleaved control frames, so the
    /// per-frame body was lifted out of `write_loop` into `write_one_queued`
    /// and both writers now call it. This guard went red on that move, which is
    /// what it is for; retargeting it is only safe **together with** the two
    /// assertions below, which say the callers still reach it. Without those,
    /// tier 2 could grow a second, gateless write path and this would stay
    /// green — the copy-instead-of-reference failure the grep guard at the
    /// bottom of this file exists to prevent, one layer up.
    #[test]
    fn the_stale_gate_times_each_frame_from_when_that_frame_was_queued() {
        let src = code();
        let at = src.find("fn write_one_queued").expect("write_one_queued is gone");
        let body = &src[at..];
        let end = body.find("\n}\n").expect("write_one_queued has no end");
        let body = &body[..end];
        assert!(
            body.contains("slot.queued_at"),
            "the writer no longer reads the frame's own queue time, so whatever it is gating on \
             is not that frame's age"
        );
        assert!(
            body.contains("STALE_BUDGET"),
            "the stale gate is gone from the writer; the ratchet has no hard bound left"
        );
        assert!(
            body.contains("stale_dropped.fetch_add"),
            "frames are dropped without being counted, which is the observability hole the whole \
             design says not to reopen"
        );

        // ...and every writer still reaches it. A gate nothing calls is a gate
        // that is not there, and each of these files owns one whole tier's
        // media egress.
        for (what, text) in [
            ("tier 1 (tcpmedia::write_loop)", src.clone()),
            ("tier 2 (mux::write_loop)", mux_code()),
        ] {
            assert!(
                text.contains("write_one_queued("),
                "{what} no longer goes through the stale gate, so that tier's media queue has no \
                 bound on how old a frame may be when it reaches the wire"
            );
        }
    }

    /// `mux.rs`'s production text, comments stripped, for the cross-file half
    /// of the guard above. Referenced rather than copied: a second gate is
    /// precisely what these guards exist to prevent.
    fn mux_code() -> String {
        let src = include_str!("mux.rs");
        let cut = src
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("mux.rs's test module marker moved; this would scan its tests too");
        crate::engine::tests::strip_comments(&src[..cut])
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

    /// **The gate's bracket and tier 1's jitter buffer must name the same
    /// profile.**
    ///
    /// This is the assertion whose *subject* keeps drifting. P3 bracketed the
    /// budget by `JbTuning::DEFAULT`, correctly, because tier 1 receivers ran
    /// `DEFAULT`. P4 gives tier 1 `DEGRADED` — and had this test not existed,
    /// the compile-time bracket would have gone on comparing against a profile
    /// nothing on this transport uses, while still reading as rigorous.
    ///
    /// So the two sides are read from where they are actually used:
    /// `engine::jb_tuning_for` on a `MediaPath::Tcp` is what a receiving stream
    /// is configured with, and [`JB_PROFILE`] is what the budget is bracketed
    /// by. Injection control: point `JB_PROFILE` back at `DEFAULT` and this
    /// goes red (the compile-time assertion alone does not — 200 ms sits inside
    /// `DEFAULT`'s window just as 440 sits inside `DEGRADED`'s).
    #[test]
    fn the_stale_gate_is_bracketed_by_the_profile_tier_one_receivers_use() {
        let link = TcpMediaLink::new_for_test("fp".into(), "127.0.0.1:1".parse().unwrap());
        let receiver = crate::engine::jb_tuning_for(&MediaPath::Tcp(Arc::new(link)));
        assert_eq!(
            (receiver.max_target, receiver.max_frames),
            (JB_PROFILE.max_target, JB_PROFILE.max_frames),
            "the stale gate is bracketed by one jitter-buffer profile and tier 1 receivers run              another; the budget's whole justification is the receiver's two numbers, so a gate              derived from a profile nobody uses is a number with no argument behind it"
        );
        let ms = STALE_BUDGET.as_millis() as u64;
        assert!(
            ms > JB_DEEPEST_TARGET_MS && ms < JB_HARD_CEILING_MS,
            "{ms} ms is outside ({JB_DEEPEST_TARGET_MS}, {JB_HARD_CEILING_MS})"
        );
    }

    /// The backlog gauge reports the **wait**, and it survives a stalled writer.
    ///
    /// A gauge sampled only on successful writes would read whatever it read
    /// before a stall and stay there for the stall's whole duration — i.e. be
    /// blind precisely when AUTO needs it. The gate's own retry loop is what
    /// saves it: the frame ages out, gets dropped, and the next dequeue reads a
    /// wait at least as large.
    #[test]
    fn the_backlog_gauge_climbs_while_the_writer_is_stalled() {
        let l = link();
        let owner = Arc::new(TxShared::new());
        assert_eq!(l.writeq_ms(), 0.0, "a fresh link claims a backlog");

        // A sink that never accepts anything, so every frame ages out.
        struct Wall;
        impl Write for Wall {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(5));
                Err(std::io::Error::new(ErrorKind::WouldBlock, "wall"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Queued as if they had been sitting there for a whole budget already.
        let old = Instant::now() - STALE_BUDGET - Duration::from_millis(50);
        for _ in 0..4 {
            assert!(push(&l, old, &owner, 200));
        }
        let shutdown = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                write_loop(&l, &mut Wall, &shutdown);
                done.store(true, Ordering::SeqCst);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while l.queued() > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            l.kill();
            shutdown.store(true, Ordering::SeqCst);
        });
        assert!(done.load(Ordering::SeqCst));
        assert!(
            l.writeq_ms() >= STALE_BUDGET.as_millis() as f64,
            "the gauge read {:.1} ms for frames the gate itself judged stale",
            l.writeq_ms()
        );
        assert!(l.stale_dropped() >= 4, "the gate did not fire on frames it had to");
        // Peak is retained, window peak is taken.
        assert!(l.writeq_peak_ms() >= l.writeq_ms() - 0.001);
        let taken = l.take_writeq_peak_ms();
        assert!(taken > 0.0, "the window peak was empty right after a stall");
        assert_eq!(l.take_writeq_peak_ms(), 0.0, "the window peak was not reset by the take");
        assert!(l.writeq_peak_ms() > 0.0, "taking the window peak also cleared the lifetime peak");
    }

    /// The test token bucket limits throughput to roughly what it is asked for.
    ///
    /// Verified against the mechanism rather than against a link: it is the
    /// thing standing in for a congested network in the P4 acceptance run, so
    /// "does it actually shape" cannot be assumed from the fact that the rung
    /// came down — that would be assuming the conclusion.
    #[test]
    fn the_test_token_bucket_shapes_the_writer_to_its_budget() {
        const BPS: u64 = 100_000; // 800 kbps
        let mut b = TokenBucket::new(BPS);
        let shutdown = AtomicBool::new(false);
        let t0 = Instant::now();
        for _ in 0..20 {
            b.gate(&shutdown);
            b.charge(1_000);
        }
        // 20 KB at 100 KB/s is 200 ms; the first one is free (no debt yet).
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_millis(170) && took < Duration::from_millis(400),
            "20 KB at {BPS} B/s took {took:?}; the bucket is not shaping to its budget"
        );
        // Off means off, and means no clock reads at all.
        let mut off = TokenBucket::new(0);
        let t1 = Instant::now();
        for _ in 0..1000 {
            off.gate(&shutdown);
            off.charge(100_000);
        }
        assert!(t1.elapsed() < Duration::from_millis(50), "a disabled bucket still slept");
    }
}
