//! An in-memory [`ControlIo`], and the proof that the control stack runs on it.
//!
//! The whole point of the [`ControlIo`] abstraction is that `verify_*` and
//! [`SecureChannel`] stop being socket code. That claim is only worth something
//! if a second implementation exists and the shipped handshake actually runs on
//! it, so this module supplies the smallest possible one: two byte queues and a
//! condition variable, no file descriptor anywhere.
//!
//! Test-only. The production second implementation is the multiplexed control
//! stream of design §4; this one exists so the abstraction is exercised before
//! that lands, and so a regression in it fails here rather than on a tunnel.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::control::ControlIo;

#[derive(Default)]
struct Pipe {
    buf: VecDeque<u8>,
    /// The end that writes into this queue is gone. Readers get EOF, writers
    /// get a broken pipe — the two outcomes a closed socket produces, so the
    /// code above cannot tell the difference.
    closed: bool,
}

type Shared = Arc<(Mutex<Pipe>, Condvar)>;

/// One end of a bidirectional in-memory byte stream.
pub struct MemDuplex {
    rx: Shared,
    tx: Shared,
    read_deadline: Option<Instant>,
}

impl MemDuplex {
    /// Two ends of one connection: what either writes, the other reads.
    pub fn pair() -> (MemDuplex, MemDuplex) {
        let a2b: Shared = Arc::new((Mutex::new(Pipe::default()), Condvar::new()));
        let b2a: Shared = Arc::new((Mutex::new(Pipe::default()), Condvar::new()));
        (
            MemDuplex { rx: b2a.clone(), tx: a2b.clone(), read_deadline: None },
            MemDuplex { rx: a2b, tx: b2a, read_deadline: None },
        )
    }
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Pipe> {
    // A panicking peer thread must not turn every later read into a different
    // failure than the one under test.
    s.0.lock().unwrap_or_else(|e| e.into_inner())
}

impl Read for MemDuplex {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let (_, cv) = &*self.rx;
        let mut p = lock(&self.rx);
        loop {
            if !p.buf.is_empty() {
                let n = out.len().min(p.buf.len());
                for (slot, byte) in out.iter_mut().zip(p.buf.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            if p.closed {
                return Ok(0); // EOF
            }
            match self.read_deadline {
                None => p = cv.wait(p).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "read deadline"));
                    }
                    let (guard, r) = cv.wait_timeout(p, left).unwrap_or_else(|e| e.into_inner());
                    p = guard;
                    if r.timed_out() && p.buf.is_empty() && !p.closed {
                        return Err(io::Error::new(io::ErrorKind::WouldBlock, "read deadline"));
                    }
                }
            }
        }
    }
}

impl Write for MemDuplex {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let (_, cv) = &*self.tx;
        let mut p = lock(&self.tx);
        if p.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the other end is gone"));
        }
        p.buf.extend(data.iter().copied());
        cv.notify_all();
        // Unbounded on purpose: this stands in for a socket send buffer that
        // never fills, which is the boring case. Backpressure belongs to the
        // media transports, not here.
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ControlIo for MemDuplex {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        // No socket option to convert it into: the deadline is simply carried
        // to the next `wait_timeout`. This is the shape the multiplexed
        // transport will have, and the reason the trait says "deadline".
        self.read_deadline = deadline;
        Ok(())
    }

    fn peer_addr(&self) -> io::Result<SocketAddr> {
        // There is no address, so there is no address to report. Inventing one
        // would put a value that reads like a measurement in front of every
        // caller that logs it.
        Err(io::Error::new(io::ErrorKind::Unsupported, "in-memory transport has no peer address"))
    }

    fn set_nodelay(&mut self, _nodelay: bool) -> io::Result<()> {
        Ok(()) // nothing to coalesce, nothing to disable
    }
}

impl Drop for MemDuplex {
    fn drop(&mut self) {
        for side in [&self.rx, &self.tx] {
            lock(side).closed = true;
            side.1.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{LocalIdentity, PairedPeer, PeerStore};
    use crate::pairing::{verify_initiator, verify_responder};
    use crate::secure::{SecureChannel, SessionMsg};
    use anyhow::Result;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Long enough that a scheduling hiccup cannot fail the test, short enough
    /// that a real deadlock is reported as a failure rather than a hung suite.
    const HANDSHAKE_GUARD: Duration = Duration::from_secs(10);

    struct Party {
        id: LocalIdentity,
        dir: PathBuf,
    }

    /// The directory names carry this as well as the clock. `SystemTime` on
    /// macOS does not actually resolve to nanoseconds, so two tests running in
    /// parallel and asking for the same tag drew the SAME path — and then one
    /// `Party::drop` deleted the other's identity, producing a failure
    /// ("identity: No such file or directory") that had nothing to do with
    /// whatever was under test. A counter cannot collide within a process.
    static PARTY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl Party {
        fn new(tag: &str) -> Party {
            let n = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let seq = PARTY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ahb-{tag}-{}-{n}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let id = LocalIdentity::load_or_create_at(Some(&dir)).expect("identity");
            Party { id, dir }
        }

        fn trust(&self, other: &Party) {
            let mut s = PeerStore::load_at(Some(&self.dir)).expect("store");
            s.upsert(PairedPeer {
                name: other.id.name.clone(),
                fingerprint: other.id.fingerprint.clone(),
                public_key_b64: other.id.public_key_b64(),
                // Deliberately no address: nothing in this exchange may need
                // one, and a test that supplied a plausible one would hide a
                // dependency on it.
                last_addr: None,
                port: 0,
                added_unix: 0,
                alias: None,
            });
            s.save().expect("save store");
        }
    }

    impl Drop for Party {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A verified, encrypted channel on each end of one in-memory connection —
    /// produced by the SHIPPED `verify_*` and `SecureChannel::establish_*`, not
    /// by a stand-in. If those ever reacquire a socket dependency, this stops
    /// compiling or stops passing.
    fn established_pair() -> (SecureChannel<MemDuplex>, SecureChannel<MemDuplex>) {
        let a = Party::new("mem-init");
        let b = Party::new("mem-resp");
        a.trust(&b);
        b.trust(&a);

        let (mut end_a, mut end_b) = MemDuplex::pair();
        let guard = Instant::now() + HANDSHAKE_GUARD;
        end_a.set_read_deadline(Some(guard)).expect("arm initiator");
        end_b.set_read_deadline(Some(guard)).expect("arm responder");

        let b_dir = b.dir.clone();
        let responder = std::thread::spawn(move || -> Result<SecureChannel<MemDuplex>> {
            let id = LocalIdentity::load_or_create_at(Some(&b_dir))?;
            let store = PeerStore::load_at(Some(&b_dir))?;
            let peer = verify_responder(&mut end_b, &id, &store)?;
            SecureChannel::establish_responder(end_b, &id, &peer)
        });

        let store_a = PeerStore::load_at(Some(&a.dir)).expect("initiator store");
        let peer_b = verify_initiator(&mut end_a, &a.id, &store_a).expect("verify as initiator");
        assert_eq!(peer_b.fingerprint, b.id.fingerprint, "verified the wrong peer");
        let ch_a =
            SecureChannel::establish_initiator(end_a, &a.id, &peer_b).expect("initiator establish");
        let ch_b = responder
            .join()
            .expect("responder thread")
            .expect("responder establish");
        (ch_a, ch_b)
    }

    /// P1's acceptance test: the full verify exchange, the `SecureChannel`
    /// handshake and a `SessionMsg` round trip, with no socket created at any
    /// point.
    ///
    /// The transport under it cannot even name a peer, which is the structural
    /// witness that nothing here fell back to a socket: a `TcpStream` always
    /// has an address, and this asserts the channel has none.
    #[test]
    fn the_control_stack_completes_a_session_round_trip_with_no_socket_beneath_it() {
        let (mut ch_a, mut ch_b) = established_pair();

        assert_eq!(
            ch_a.peer_addr().expect_err("an in-memory channel has no address").kind(),
            io::ErrorKind::Unsupported,
            "if this channel has an address it is running on a socket, and the test proves nothing"
        );

        ch_a.send(&SessionMsg::Ping { t_us: 424_242 }).expect("initiator sends");
        match ch_b
            .recv_timeout(Duration::from_secs(5))
            .expect("responder read")
            .expect("a message before the deadline")
        {
            SessionMsg::Ping { t_us } => assert_eq!(t_us, 424_242),
            other => panic!("expected the Ping we sent, got {other:?}"),
        }

        ch_b
            .send(&SessionMsg::Pong { t_us: 424_242, peer_t_us: Some(7) })
            .expect("responder answers");
        match ch_a
            .recv_timeout(Duration::from_secs(5))
            .expect("initiator read")
            .expect("a message before the deadline")
        {
            SessionMsg::Pong { t_us, peer_t_us } => {
                assert_eq!(t_us, 424_242, "the reply did not carry our own timestamp back");
                assert_eq!(peer_t_us, Some(7));
            }
            other => panic!("expected a Pong, got {other:?}"),
        }
    }

    /// `recv_timeout`'s bound used to be `SO_RCVTIMEO`. This pins that it is
    /// now a property of the trait: a transport with no socket option under it
    /// still returns `None` at the deadline instead of blocking forever.
    ///
    /// Both channels are held: dropping the far end would produce `None`'s
    /// evil twin, an EOF, and the test would pass for the wrong reason.
    #[test]
    fn a_read_deadline_is_honoured_by_a_transport_that_has_no_socket_option() {
        let (mut ch_a, _ch_b) = established_pair();
        let t0 = Instant::now();
        let got = ch_a
            .recv_timeout(Duration::from_millis(200))
            .expect("a quiet channel is not an error");
        let waited = t0.elapsed();
        assert!(got.is_none(), "nothing was sent, so nothing may be returned: {got:?}");
        assert!(
            waited >= Duration::from_millis(150),
            "returned after {waited:?}: the deadline was not waited out, so it is not being applied"
        );
        assert!(
            waited < Duration::from_secs(3),
            "returned after {waited:?}: the deadline did not bound the read"
        );
    }
}
