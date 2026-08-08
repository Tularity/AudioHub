//! The control byte stream of a multiplexed connection (tier 2, design §4).
//!
//! On tier 2 there is **one** connection and three kinds of traffic on it:
//! media in both directions plus the control stream. This module owns the
//! control half — the queues a demultiplexing reader delivers into and a
//! scheduling writer drains — and exposes it as a [`ControlIo`], so `verify_*`
//! and `SecureChannel` run over a tunnel with no changes at all. The threads,
//! the socket and the media queue live in the daemon (`audiohubd::mux`); this
//! is the part that is pure protocol, and it is here so that the control stack
//! never has to know a mux exists.
//!
//! # A control frame is a slice of the stream, not a message
//!
//! [`Kind::Control`]'s payload is a chunk of the existing `u32 length ‖ JSON`
//! byte stream, cut wherever the writer chose to cut it. [`MUX_MAX_PAYLOAD`] is
//! 4096 and `CONTROL_MAX_FRAME` is 65536, so a large control message **spans
//! several frames by construction** and reassembly belongs to the reader above
//! this layer — which gets it for free, because that reader is
//! `SecureChannel::take_frame`, which has always parsed a length prefix out of
//! a byte buffer.
//!
//! # ⚠ Framing here is unauthenticated, and it has to be
//!
//! The control stream carries its own handshake, so the frame layer must be
//! moving bytes **before** `SecureChannel` exists — there is no key to
//! authenticate a frame with until frames have already carried the key
//! exchange. This is the same exposure the cleartext control handshake has on
//! tier 0 today (`verify_*` runs on a bare `TcpStream`), and it ends at the
//! same instant: once the channel is established every control payload is
//! AEAD-sealed inside `ControlMsg::Enc` before it reaches a frame.
//!
//! What the frame layer must therefore never do is *trust* a header. It does
//! not: `FrameDecoder` refuses a declared length over [`MUX_MAX_PAYLOAD`]
//! before that length sizes anything, and the worst a stranger achieves on this
//! path is the same thing they achieve by connecting to the control port today
//! — a refused handshake.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::control::ControlIo;
use crate::framed::{control_header, encode_frame, MUX_MAX_PAYLOAD};

/// Encoded control frames allowed to sit in the outbox before a write is
/// refused.
///
/// # Why refusing beats growing
///
/// The outbox exists because the writer is busy with media; it is not a buffer
/// for a peer that has stopped reading. At ~1 Hz of control traffic this is
/// **64 seconds** of backlog, an order of magnitude past the five seconds of
/// silence `conn::CONTROL_SILENCE_LIMIT` already treats as a dead channel. So a
/// full outbox is never congestion — it is a connection that is already gone,
/// and the honest report is a write error, which marks the `SecureChannel`
/// poisoned and drops the connection. An unbounded queue would instead keep the
/// peer looking online while every message aged in memory.
const MAX_PENDING_FRAMES: usize = 64;

/// Bytes of undelivered control stream allowed to accumulate in the inbox.
///
/// The mirror of [`MAX_PENDING_FRAMES`] on the receive side, and sized against
/// the one legitimate consumer: `SecureChannel::recv_timeout` drains everything
/// available on every 50 ms pass, so any real backlog here is a reader that has
/// stopped. Generous enough to hold several maximum control frames
/// (`CONTROL_MAX_FRAME` is 65536) so a big legitimate message is never the
/// thing that trips it.
const MAX_INBOX_BYTES: usize = 1 << 20;

#[derive(Default)]
struct Inbox {
    buf: VecDeque<u8>,
    /// The connection is finished. Readers drain what is left and then see EOF,
    /// which is what a closed socket does, so `SecureChannel` reports
    /// "connection closed by peer" through its ordinary path.
    closed: bool,
    /// The inbox overflowed. Distinct from `closed` so the reason survives to
    /// the log: silently closing would make a broken peer indistinguishable
    /// from a peer that hung up politely.
    overflowed: bool,
}

/// Parks and unparks the mux writer thread.
///
/// Same shape and the same `SeqCst` fence pairing as `UdpSender::wake` and
/// `TcpMediaLink::wake`, and for the same reason: the writer sets `parked`,
/// then re-checks its queues, so a wake that lands in between is not lost. The
/// symptom of getting this wrong is one control frame going out a park slice
/// late, i.e. a `Pong` that is inexplicably 20 ms slow.
#[derive(Default)]
pub struct WriterPark {
    thread: OnceLock<std::thread::Thread>,
    parked: AtomicBool,
}

impl WriterPark {
    /// Called once by the writer thread, before its first park.
    pub fn adopt_current(&self) {
        let _ = self.thread.set(std::thread::current());
    }

    /// Park up to `timeout` unless `ready()` says there is already work.
    pub fn park_unless(&self, timeout: Duration, ready: impl Fn() -> bool) {
        self.parked.store(true, Ordering::SeqCst);
        if !ready() {
            std::thread::park_timeout(timeout);
        }
        self.parked.store(false, Ordering::SeqCst);
    }

    pub fn wake(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) {
            if let Some(t) = self.thread.get() {
                t.unpark();
            }
        }
    }
}

/// The two control queues of one multiplexed connection, shared between the
/// reader thread, the writer thread and the [`MuxControlStream`] handle the
/// control stack holds.
pub struct MuxIo {
    inbox: Mutex<Inbox>,
    arrived: Condvar,
    outbox: Mutex<VecDeque<Vec<u8>>>,
    /// Where the byte stream physically goes. **This is the tunnel's address,
    /// not the peer's** — on tier 2 the two are different by definition, which
    /// is the whole premise (plan §4: identity is the fingerprint, never the
    /// source address).
    peer_addr: SocketAddr,
    pub writer: WriterPark,
}

impl MuxIo {
    pub fn new(peer_addr: SocketAddr) -> MuxIo {
        MuxIo {
            inbox: Mutex::new(Inbox::default()),
            arrived: Condvar::new(),
            outbox: Mutex::new(VecDeque::new()),
            peer_addr,
            writer: WriterPark::default(),
        }
    }

    fn lock_inbox(&self) -> std::sync::MutexGuard<'_, Inbox> {
        // A panicking thread elsewhere must not turn every later read into a
        // different failure than the one under test.
        self.inbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_outbox(&self) -> std::sync::MutexGuard<'_, VecDeque<Vec<u8>>> {
        self.outbox.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The demultiplexing reader hands a `Kind::Control` payload over.
    ///
    /// Returns `false` when the inbox is over [`MAX_INBOX_BYTES`], which the
    /// caller must treat as fatal for the connection: there is no way to apply
    /// back-pressure to one stream of a mux without stalling the others, so the
    /// only alternative to dropping the connection is dropping bytes out of the
    /// middle of a control stream — and a control stream with a hole in it
    /// desynchronises exactly like a framed one.
    pub fn deliver_control(&self, bytes: &[u8]) -> bool {
        let mut ib = self.lock_inbox();
        if ib.closed {
            return false;
        }
        if ib.buf.len() + bytes.len() > MAX_INBOX_BYTES {
            ib.overflowed = true;
            ib.closed = true;
            drop(ib);
            self.arrived.notify_all();
            return false;
        }
        ib.buf.extend(bytes.iter().copied());
        drop(ib);
        self.arrived.notify_all();
        true
    }

    /// The connection is over. Readers see EOF once they have drained what
    /// already arrived; a parked writer wakes to notice.
    pub fn close(&self) {
        self.lock_inbox().closed = true;
        self.arrived.notify_all();
        self.writer.wake();
    }

    pub fn is_closed(&self) -> bool {
        self.lock_inbox().closed
    }

    /// True when the inbox was closed because it overflowed rather than because
    /// the connection ended.
    pub fn overflowed(&self) -> bool {
        self.lock_inbox().overflowed
    }

    /// Is a control frame waiting to go out? Read by the writer's scheduler on
    /// every pass, so it takes the lock and nothing else.
    pub fn control_pending(&self) -> bool {
        !self.lock_outbox().is_empty()
    }

    /// The next encoded control frame, oldest first.
    pub fn take_control_frame(&self) -> Option<Vec<u8>> {
        self.lock_outbox().pop_front()
    }

    /// Put a frame back at the head after a write failed part-way through.
    ///
    /// Only correct when **nothing** of it reached the wire: a frame that is
    /// half out cannot be retried, because the peer is already reading its
    /// payload. The one caller checks that.
    pub fn requeue_control_frame(&self, frame: Vec<u8>) {
        self.lock_outbox().push_front(frame);
    }
}

/// The control stack's end of a multiplexed connection: a [`ControlIo`] whose
/// bytes travel as [`Kind::Control`] frames.
///
/// [`Kind::Control`]: crate::packet::Kind::Control
pub struct MuxControlStream {
    io: std::sync::Arc<MuxIo>,
    /// Bytes written but not yet cut into frames. See [`MuxControlStream::flush`].
    pending: Vec<u8>,
    read_deadline: Option<Instant>,
}

impl MuxControlStream {
    pub fn new(io: std::sync::Arc<MuxIo>) -> MuxControlStream {
        MuxControlStream { io, pending: Vec::new(), read_deadline: None }
    }

    pub fn io(&self) -> &std::sync::Arc<MuxIo> {
        &self.io
    }
}

impl Read for MuxControlStream {
    /// Blocks until bytes are available, the connection closes, or the deadline
    /// set through [`ControlIo::set_read_deadline`] passes.
    ///
    /// Reaching the deadline reports [`io::ErrorKind::WouldBlock`] **having
    /// consumed nothing**, which is the contract `SO_RCVTIMEO` offers and the
    /// one `SecureChannel::recv_timeout` is written against.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut ib = self.io.lock_inbox();
        loop {
            if !ib.buf.is_empty() {
                let n = ib.buf.len().min(out.len());
                for (slot, byte) in out[..n].iter_mut().zip(ib.buf.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            // Drained first, closed second: bytes that arrived before the
            // connection ended are still bytes the peer sent.
            if ib.closed {
                return Ok(0);
            }
            let Some(deadline) = self.read_deadline else {
                ib = self.io.arrived.wait(ib).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "mux read deadline"));
            }
            let (g, _) = self
                .io
                .arrived
                .wait_timeout(ib, left)
                .unwrap_or_else(|e| e.into_inner());
            ib = g;
        }
    }
}

impl Write for MuxControlStream {
    /// Buffers. **Nothing reaches the wire until [`MuxControlStream::flush`].**
    ///
    /// `control::write_frame` writes the length and the body separately and
    /// then flushes, so framing per `write` call would put a 4-byte payload in
    /// a 40-byte header and cut every message in two for no reason. Buffering
    /// makes the common case one frame per control message.
    ///
    /// The contract this rests on is that every control writer flushes, which
    /// `control::write_frame` and `SecureChannel::send_raw_payload` (through it)
    /// both do. `a_message_that_is_never_flushed_never_reaches_the_wire` pins
    /// the consequence of breaking it, so it is a documented property rather
    /// than a lurking stall.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // Cut into frames first, queue them second: a message that does not fit
        // must not leave half of itself in the outbox, because the peer would
        // reassemble a truncated `u32 length ‖ JSON` and desynchronise rather
        // than fail.
        let mut frames = Vec::new();
        for chunk in self.pending.chunks(MUX_MAX_PAYLOAD) {
            let mut frame = Vec::with_capacity(chunk.len() + 40);
            encode_frame(&control_header(), chunk, &mut frame)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            frames.push(frame);
        }
        {
            let mut ob = self.io.lock_outbox();
            if ob.len() + frames.len() > MAX_PENDING_FRAMES {
                drop(ob);
                // Left in `pending` deliberately: the caller's `write_frame`
                // fails, `SecureChannel` marks itself poisoned and the
                // connection is dropped. Clearing here would discard a message
                // the layer above still believes it may retry.
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the multiplexed control queue is full; the peer has stopped reading",
                ));
            }
            ob.extend(frames);
        }
        self.pending.clear();
        self.io.writer.wake();
        Ok(())
    }
}

impl ControlIo for MuxControlStream {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.read_deadline = deadline;
        Ok(())
    }

    /// The **tunnel's** address, which on tier 2 is not the peer's.
    ///
    /// Reported rather than refused because there genuinely is one — unlike
    /// `MemDuplex`, which has no address at all and says so. What callers must
    /// not do is treat it as identity: on tier 2 it is routinely `127.0.0.1`
    /// for every peer at once, and `plan.md` §4 settles that question in the
    /// only direction that survives a tunnel — identity is the fingerprint.
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.io.peer_addr)
    }

    /// The mux owns the socket and disabled Nagle on it once, when it was
    /// created. A per-stream request here has nothing to act on and no reason
    /// to fail.
    fn set_nodelay(&mut self, _nodelay: bool) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framed::FrameDecoder;
    use crate::packet::Kind;
    use std::sync::Arc;

    fn io() -> Arc<MuxIo> {
        Arc::new(MuxIo::new("127.0.0.1:47870".parse().unwrap()))
    }

    /// Decode every queued frame and concatenate the control payloads, i.e. do
    /// what the peer's reader thread does.
    fn drain_stream(io: &MuxIo) -> Vec<u8> {
        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        while let Some(frame) = io.take_control_frame() {
            let mut off = 0;
            while off < frame.len() {
                off += dec.push(&frame[off..]);
                while let Some(f) = dec.next_frame().expect("decode") {
                    assert_eq!(f.header.kind, Kind::Control, "the writer emitted a non-control kind");
                    out.extend_from_slice(f.payload());
                }
            }
        }
        out
    }

    /// A control message written through this stream comes back byte for byte
    /// after being cut into frames — the property the whole tier rests on.
    #[test]
    fn a_control_message_survives_the_round_trip_through_frames() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        let msg = br#"{"type":"ping","t_us":123456}"#;
        s.write_all(&(msg.len() as u32).to_le_bytes()).expect("len");
        s.write_all(msg).expect("body");
        s.flush().expect("flush");

        let bytes = drain_stream(&io);
        let mut want = (msg.len() as u32).to_le_bytes().to_vec();
        want.extend_from_slice(msg);
        assert_eq!(bytes, want, "the control byte stream did not survive framing");
    }

    /// One control message becomes **one** frame, not one per `write` call.
    /// Without the buffering in `write`, `control::write_frame`'s two calls
    /// would put an 80-byte header cost on every ~200-byte message.
    #[test]
    fn a_flush_emits_one_frame_per_message_not_one_per_write() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        s.write_all(&[0u8; 4]).expect("len");
        s.write_all(&[7u8; 200]).expect("body");
        assert!(!io.control_pending(), "bytes reached the outbox before the flush");
        s.flush().expect("flush");

        let mut frames = 0;
        while io.take_control_frame().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 1, "one message produced {frames} frames");
    }

    /// A message larger than one frame spans several, and the reader reassembles
    /// it. This is the case `MUX_MAX_PAYLOAD` (4096) < `CONTROL_MAX_FRAME`
    /// (65536) makes reachable with an ordinary message, not a hostile one:
    /// a `SessionMsg::Stats` with base64 in it clears 4 KiB without trying.
    #[test]
    fn a_message_larger_than_one_frame_spans_several_and_reassembles() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        let body: Vec<u8> = (0..MUX_MAX_PAYLOAD * 2 + 37).map(|i| (i % 251) as u8).collect();
        s.write_all(&(body.len() as u32).to_le_bytes()).expect("len");
        s.write_all(&body).expect("body");
        s.flush().expect("flush");

        let queued = io.lock_outbox().len();
        assert_eq!(queued, 3, "expected three frames for a 2×payload + 41 byte stream");

        let bytes = drain_stream(&io);
        assert_eq!(&bytes[..4], &(body.len() as u32).to_le_bytes());
        assert_eq!(&bytes[4..], &body[..], "reassembly lost or reordered bytes");
    }

    /// Delivered bytes are readable, and a read that finds nothing before its
    /// deadline reports `WouldBlock` **without consuming anything** — the
    /// contract `SecureChannel::recv_timeout` is written against.
    #[test]
    fn a_read_past_its_deadline_reports_would_block_and_consumes_nothing() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        assert!(io.deliver_control(b"abc"));

        let mut buf = [0u8; 8];
        s.set_read_deadline(Some(Instant::now() + Duration::from_secs(5))).expect("arm");
        assert_eq!(s.read(&mut buf).expect("read"), 3);
        assert_eq!(&buf[..3], b"abc");

        s.set_read_deadline(Some(Instant::now())).expect("arm");
        let e = s.read(&mut buf).expect_err("an expired deadline must not block");
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);

        // ...and the stream is still usable afterwards, which is what "consumed
        // nothing" means in practice.
        assert!(io.deliver_control(b"de"));
        s.set_read_deadline(Some(Instant::now() + Duration::from_secs(5))).expect("arm");
        assert_eq!(s.read(&mut buf).expect("read"), 2);
        assert_eq!(&buf[..2], b"de");
    }

    /// Closing drains first and reports EOF second. `SecureChannel` turns
    /// `Ok(0)` into "connection closed by peer", which is how a dead mux
    /// becomes a dead control connection with no extra plumbing.
    #[test]
    fn closing_delivers_what_already_arrived_and_then_reports_eof() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        assert!(io.deliver_control(b"tail"));
        io.close();

        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).expect("read"), 4, "bytes that arrived before the close were lost");
        assert_eq!(&buf[..4], b"tail");
        assert_eq!(s.read(&mut buf).expect("read"), 0, "a closed inbox must report EOF");
        assert!(!io.deliver_control(b"more"), "a closed inbox must refuse further bytes");
    }

    /// A blocked reader wakes when bytes arrive, rather than sitting out its
    /// whole deadline. Without the notify this passes only by timing out, so
    /// the assertion is on the elapsed time, not on the bytes.
    #[test]
    fn a_blocked_reader_wakes_when_bytes_arrive() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        s.set_read_deadline(Some(Instant::now() + Duration::from_secs(10))).expect("arm");

        let feeder = io.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            feeder.deliver_control(b"late");
        });

        let t0 = Instant::now();
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).expect("read"), 4);
        assert!(t0.elapsed() < Duration::from_secs(5), "the reader slept through the delivery");
    }

    /// A peer that stops reading fills the outbox, and the write **fails**
    /// rather than growing. The failure is what marks the `SecureChannel`
    /// poisoned and drops the connection; an unbounded queue would keep the
    /// peer looking online while every message aged in memory.
    #[test]
    fn a_full_outbox_refuses_the_write_instead_of_growing() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        for i in 0..MAX_PENDING_FRAMES {
            s.write_all(b"xxxx").expect("write");
            s.flush().unwrap_or_else(|e| panic!("flush {i} failed early: {e}"));
        }
        s.write_all(b"xxxx").expect("write");
        let e = s.flush().expect_err("the outbox must refuse frame 65");
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(io.lock_outbox().len(), MAX_PENDING_FRAMES, "the bound was exceeded anyway");
    }

    /// The inbox is bounded too, and overflow closes the connection with a
    /// reason attached. Dropping bytes instead would leave a hole in a byte
    /// stream that has no way to resynchronise.
    #[test]
    fn an_overflowing_inbox_closes_the_connection_and_says_why() {
        let io = io();
        let chunk = vec![0u8; MUX_MAX_PAYLOAD];
        let mut delivered = 0usize;
        loop {
            if !io.deliver_control(&chunk) {
                break;
            }
            delivered += chunk.len();
            assert!(delivered <= MAX_INBOX_BYTES, "the inbox grew past its bound");
        }
        assert!(io.is_closed(), "an overflow must end the connection");
        assert!(io.overflowed(), "the reason for the close was lost");
    }

    /// Written-but-unflushed bytes stay put. Documented rather than fixed: the
    /// alternative (framing on every `write`) doubles the frame count for every
    /// control message, and every writer in this tree goes through
    /// `control::write_frame`, which flushes.
    #[test]
    fn a_message_that_is_never_flushed_never_reaches_the_wire() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        s.write_all(b"unflushed").expect("write");
        assert!(!io.control_pending(), "an unflushed write reached the outbox");
        s.flush().expect("flush");
        assert!(io.control_pending(), "the flush did not deliver the buffered bytes");
    }

    /// A frame the writer could not start is retried whole. The head of the
    /// queue is where it has to go back — appending would reorder the control
    /// stream, and a reordered byte stream is a corrupt one.
    #[test]
    fn a_requeued_frame_goes_back_to_the_head_of_the_queue() {
        let io = io();
        let mut s = MuxControlStream::new(io.clone());
        s.write_all(b"one").expect("w");
        s.flush().expect("f");
        s.write_all(b"two").expect("w");
        s.flush().expect("f");

        let first = io.take_control_frame().expect("first");
        io.requeue_control_frame(first.clone());
        assert_eq!(io.take_control_frame().expect("again"), first, "the retry lost its place");
    }
}
