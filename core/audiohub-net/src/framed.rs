//! Framing for byte-stream transports: TCP media (tier 1) and the single
//! multiplexed connection (tier 2).
//!
//! A frame is `Header(40) ‖ payload`, and the length is the header's own
//! `payload_len` field. **There is no second length prefix**, because there is
//! no second frame format: the 40-byte packet header already carries a length,
//! a class ([`Kind`]) and a stream id, which is everything a demultiplexer
//! needs. A media frame here is therefore *byte-identical* to the datagram UDP
//! would have carried — see [`FrameDecoder`] — so the sealed bytes coming out of
//! `MediaCrypto` need no re-encoding to cross a stream, and the frozen
//! assertions already guarding the header in [`crate::packet`] cover the
//! degraded path for free. A purpose-built mux header would have doubled the
//! test surface to save 40 bytes on a ~1 Hz control channel.
//!
//! # ⚠ The stream reintroduces an attack surface UDP did not have
//!
//! On UDP, `payload_len` is bounded by the datagram the kernel already received:
//! lying about it gets the packet dropped, not memory allocated. On a stream it
//! is **an attacker-controlled allocation instruction that arrives before
//! anything has been authenticated** — the AEAD cannot help, because the frame
//! layer has to decide how many bytes to wait for before it has a frame to
//! open.
//!
//! So [`MUX_MAX_PAYLOAD`] is compared against the declared length **before any
//! sizing decision is made**, and the decoder's buffer is allocated once, at
//! construction, at exactly the largest size a legal frame can reach. A
//! declared length can therefore never become an allocation, however large it
//! is; the worst a hostile peer achieves is one refusal.
//!
//! # What this module is not
//!
//! Nothing here is wired into the daemon: it is a library with no caller yet
//! (design §6, P2). In particular `control::PROTOCOL_VERSION` is unchanged,
//! which is correct precisely because no [`Kind::Control`] frame can reach a
//! peer — see the note on [`Kind`].

use crate::media::{AEAD_TAG_LEN, LADDER};
use crate::packet::{Codec, Header, Kind, PacketError, HEADER_LEN};

/// The largest payload a frame may declare, and the reason a declared length
/// can never turn into an allocation.
///
/// The value is `engine::RECV_BUF_BYTES`, the bound the UDP receive path
/// already lives inside. Keeping the two equal means a frame that fits on this
/// transport fits on that one, so the choice of transport can never be the
/// reason a rung stops working.
pub const MUX_MAX_PAYLOAD: usize = 4096;

/// Header plus the largest legal payload: the size of a [`FrameDecoder`]'s
/// buffer, and the only allocation it ever makes.
pub const MUX_MAX_FRAME: usize = HEADER_LEN + MUX_MAX_PAYLOAD;

/// Where `payload_len` sits in the header.
///
/// The frame layer reads this field before it is willing to parse a header,
/// which is the one place it needs to know the header's internal layout. The
/// dependency is pinned from both ends: statically against [`HEADER_LEN`] just
/// below, and dynamically against the real encoder in
/// `the_payload_length_field_is_where_the_frame_layer_looks_for_it`.
const PAYLOAD_LEN_OFFSET: usize = 36;

const _: () = assert!(
    PAYLOAD_LEN_OFFSET + 4 == HEADER_LEN,
    "payload_len must be the last header field; a field appended after it would \
     leave the frame layer reading the wrong four bytes to size every frame"
);

const _: () = assert!(
    MUX_MAX_PAYLOAD >= LADDER[0].frame_bytes() + AEAD_TAG_LEN,
    "the deepest rung's sealed frame does not fit in a mux frame: tier 1/2 would \
     refuse exactly the rung tier 0 is happiest with, and the symptom would be a \
     frame-layer error rather than anything pointing at the ladder"
);

/// Why a frame could not be encoded or decoded.
///
/// Deliberately allocation-free (no `String`, no `anyhow`): the oversized-length
/// path asserts that refusing a frame allocates *nothing*, and an error type
/// that formatted a message would make that assertion untestable.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum FramedError {
    /// The declared (decode) or supplied (encode) payload exceeds
    /// [`MUX_MAX_PAYLOAD`]. Refused before the length is used for anything.
    #[error("frame declares {declared} payload bytes, over the {MUX_MAX_PAYLOAD} byte limit")]
    PayloadTooLarge { declared: usize },
    /// The 40 bytes were the right length but not a valid header.
    #[error("frame header: {0}")]
    Header(#[from] PacketError),
    /// A previous call already failed. See [`FrameDecoder::next_frame`].
    #[error("this decoder already failed; a framed stream cannot be resynchronised")]
    Poisoned,
}

/// Appends one frame to `out`.
///
/// `header.payload_len` is **ignored and overwritten** with `payload.len()`.
/// The two cannot disagree, because a frame whose declared length does not
/// match its payload is not a frame at all — it silently reinterprets every
/// following byte in the stream, and the peer discovers this several frames
/// later as a header that fails to parse. Making the payload the sole authority
/// removes that failure mode rather than testing for it.
///
/// Appends rather than replaces, unlike [`Header::encode_into`]: a writer with
/// several frames to send builds them into one buffer and issues one write.
///
/// **Media does not need this.** A sealed media datagram is already a frame,
/// byte for byte; a media writer forwards those bytes untouched. This exists
/// for [`Kind::Control`] and [`Kind::MuxKeepalive`], which have no datagram to
/// start from.
pub fn encode_frame(
    header: &Header,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), FramedError> {
    if payload.len() > MUX_MAX_PAYLOAD {
        return Err(FramedError::PayloadTooLarge { declared: payload.len() });
    }
    let mut h = header.clone();
    h.payload_len = payload.len() as u32;
    h.encode_append(payload, out);
    Ok(())
}

/// The header for a [`Kind::Control`] frame.
///
/// Every audio field is filler and **a receiver must not read any of them**:
/// there is no codec, no sample rate and no stream behind a control frame. They
/// are fixed here, in one place, so that the tier 1 and tier 2 writers cannot
/// each invent their own filler and produce two dialects of the same frame.
/// [`Codec::Passthrough`] is the value that means "not audio"; it has to be
/// *some* valid codec because [`Header::parse`] validates that byte.
pub fn control_header() -> Header {
    Header {
        kind: Kind::Control,
        codec: Codec::Passthrough,
        channels: 0,
        sample_rate: 0,
        session_id: 0,
        stream_id: 0,
        seq: 0,
        timestamp_us: 0,
        payload_len: 0,
    }
}

/// The header for a [`Kind::MuxKeepalive`] frame. Payload is always empty.
pub fn keepalive_header() -> Header {
    Header { kind: Kind::MuxKeepalive, ..control_header() }
}

/// One decoded frame, borrowed from the decoder's buffer.
///
/// Borrowed rather than owned so that a hot receive path costs no allocation
/// per frame. It holds the decoder mutably for as long as it lives, which is
/// what makes the deferred consumption in [`FrameDecoder::next_frame`] safe:
/// the bytes cannot be reclaimed while anyone can still see them.
#[derive(Debug)]
pub struct Frame<'a> {
    pub header: Header,
    frame: &'a [u8],
}

impl<'a> Frame<'a> {
    /// The whole frame, header included.
    ///
    /// This is what a media frame's consumer wants: `MediaCrypto` authenticates
    /// the 40-byte header as associated data, so handing it only the payload
    /// would fail every open. It is also, by construction, exactly the datagram
    /// UDP would have delivered.
    pub fn bytes(&self) -> &'a [u8] {
        self.frame
    }

    /// The frame without its header.
    pub fn payload(&self) -> &'a [u8] {
        &self.frame[HEADER_LEN..]
    }
}

/// Turns a byte stream back into frames.
///
/// Feed it whatever the transport produced with [`FrameDecoder::push`], then
/// drain with [`FrameDecoder::next_frame`] until it yields `None`:
///
/// ```ignore
/// let n = stream.read(&mut scratch)?;
/// let mut off = 0;
/// while off < n {
///     off += dec.push(&scratch[off..n]);
///     while let Some(frame) = dec.next_frame()? {
///         dispatch(frame.header.kind, frame.bytes());
///     }
/// }
/// ```
///
/// The inner loop is not optional. [`FrameDecoder::push`] takes only what fits
/// in the fixed buffer and reports how much it took, so a caller that skips the
/// drain can stall; a caller that performs it cannot, because the buffer is a
/// whole frame larger than the largest frame it will ever hold (see
/// `the_decoder_cannot_stall_with_a_full_buffer_and_no_frame`).
pub struct FrameDecoder {
    /// Always `MUX_MAX_FRAME` bytes long, allocated once in
    /// [`FrameDecoder::new`] and never resized. This is what makes "a declared
    /// length is never an allocation" a structural property rather than a
    /// promise: there is no code path that sizes anything from the wire.
    buf: Vec<u8>,
    /// Bytes of `buf` that are live input.
    len: usize,
    /// Length of the frame handed out by the last [`FrameDecoder::next_frame`],
    /// dropped at the start of the next call. Deferred because the caller is
    /// still looking at those bytes.
    consumed: usize,
    poisoned: bool,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder { buf: vec![0u8; MUX_MAX_FRAME], len: 0, consumed: 0, poisoned: false }
    }

    /// Copies as much of `input` as fits and returns how much that was.
    ///
    /// A short return is normal, not an error: it means "drain me first". A
    /// return of 0 on a non-empty input means the buffer holds a complete frame
    /// that [`FrameDecoder::next_frame`] has not been asked for yet — or that
    /// the decoder is poisoned, in which case the next `next_frame` says so.
    pub fn push(&mut self, input: &[u8]) -> usize {
        if self.poisoned {
            return 0;
        }
        self.drop_consumed();
        let n = (MUX_MAX_FRAME - self.len).min(input.len());
        self.buf[self.len..self.len + n].copy_from_slice(&input[..n]);
        self.len += n;
        n
    }

    /// The next complete frame, or `None` if more bytes are needed.
    ///
    /// # Errors are terminal
    ///
    /// A framing error poisons the decoder, and every later call returns
    /// [`FramedError::Poisoned`]. This is not defensiveness: a stream that
    /// desynchronised cannot be resynchronised, because there is no delimiter to
    /// search for and every 40-byte window is a candidate header. A caller that
    /// logged the error and carried on would be resynchronising onto boundaries
    /// an attacker chose. The only correct response is to drop the connection,
    /// so the type refuses to offer any other.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, FramedError> {
        if self.poisoned {
            return Err(FramedError::Poisoned);
        }
        self.drop_consumed();
        if self.len < HEADER_LEN {
            return Ok(None);
        }

        // The bound check comes first, ahead of every other use of this number.
        // Not for tidiness: the alternative orderings all read as "size
        // something, then check" — and the whole point of MUX_MAX_PAYLOAD is
        // that a stranger's four bytes must never size anything at all.
        let declared = u32::from_le_bytes(
            self.buf[PAYLOAD_LEN_OFFSET..PAYLOAD_LEN_OFFSET + 4].try_into().unwrap(),
        ) as usize;
        if declared > MUX_MAX_PAYLOAD {
            self.poisoned = true;
            return Err(FramedError::PayloadTooLarge { declared });
        }

        let total = HEADER_LEN + declared;
        if self.len < total {
            return Ok(None);
        }

        // Everything else — magic, version, kind, codec — is `Header::parse`'s
        // job, on the exact bytes of the frame. The frame layer deliberately
        // owns no second opinion about what a header is.
        let (header, _) = match Header::parse(&self.buf[..total]) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.poisoned = true;
                return Err(FramedError::Header(e));
            }
        };
        self.consumed = total;
        Ok(Some(Frame { header, frame: &self.buf[..total] }))
    }

    /// Live input bytes held, frames already handed out excluded.
    pub fn buffered(&self) -> usize {
        self.len - self.consumed
    }

    fn drop_consumed(&mut self) {
        if self.consumed > 0 {
            self.buf.copy_within(self.consumed..self.len, 0);
            self.len -= self.consumed;
            self.consumed = 0;
        }
    }
}

/// Counts bytes allocated **by the calling thread**, so that "refusing a
/// 1 GiB frame allocates nothing" can be asserted rather than asserted-about.
///
/// Per-thread and not global on purpose: the test suite runs in parallel, and a
/// global counter would be measuring every other test at the same time. Only
/// the thread running the decoder is charged for the decoder.
///
/// A source-text assertion would not do here. This repository's grep guards are
/// blind to comments and have been fooled before; the property under test is
/// "no memory was obtained", and the only witness to that is the allocator.
#[cfg(test)]
mod alloc_meter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        // `const` init: no lazy initialisation and no destructor, so reading it
        // cannot itself allocate. An allocator that allocates does not return.
        static BYTES: Cell<usize> = const { Cell::new(0) };
    }

    pub fn bytes_allocated_here() -> usize {
        BYTES.try_with(|c| c.get()).unwrap_or(0)
    }

    fn charge(n: usize) {
        let _ = BYTES.try_with(|c| c.set(c.get().saturating_add(n)));
    }

    struct Meter;

    unsafe impl GlobalAlloc for Meter {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            charge(l.size());
            System.alloc(l)
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            charge(l.size());
            System.alloc_zeroed(l)
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            charge(new.saturating_sub(l.size()));
            System.realloc(p, l, new)
        }
    }

    #[global_allocator]
    static METER: Meter = Meter;
}

#[cfg(test)]
mod tests {
    use super::alloc_meter::bytes_allocated_here;
    use super::*;
    use crate::packet::MAGIC;

    fn media_header(seq: u32, payload_len: usize) -> Header {
        Header {
            kind: Kind::Media,
            codec: Codec::PcmS16le,
            channels: 1,
            sample_rate: 48_000,
            session_id: 42,
            stream_id: 42,
            seq,
            timestamp_us: 1_000_000 + seq as u64 * 10_000,
            payload_len: payload_len as u32,
        }
    }

    fn ctl(payload_len: usize) -> Header {
        Header { payload_len: payload_len as u32, ..control_header() }
    }

    /// Frames of every awkward size: empty, one byte, a real rung-2 datagram,
    /// the deepest rung's sealed frame, and one sitting exactly on the limit.
    fn sample_frames() -> Vec<(Header, Vec<u8>)> {
        let deepest = LADDER[0].frame_bytes() + AEAD_TAG_LEN;
        vec![
            (ctl(13), br#"{"type":"ok"}"#.to_vec()),
            (Header { payload_len: 0, ..keepalive_header() }, Vec::new()),
            (media_header(1, 960), vec![0xA5; 960]),
            (ctl(1), vec![7u8]),
            (media_header(2, deepest), vec![0x11; deepest]),
            (media_header(3, MUX_MAX_PAYLOAD), vec![0x5A; MUX_MAX_PAYLOAD]),
            (Header { payload_len: 0, ..keepalive_header() }, Vec::new()),
        ]
    }

    fn encode_all(frames: &[(Header, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (h, p) in frames {
            encode_frame(h, p, &mut out).expect("encode");
        }
        out
    }

    /// Feeds `stream` in chunks of `chunk` bytes. Returns the frames recovered
    /// and the most any single `push` yielded — the observable proof that a
    /// chunk straddled a frame boundary, since one push cannot complete two
    /// frames without having contained the boundary between them.
    fn decode_in_chunks(stream: &[u8], chunk: usize) -> (Vec<(Header, Vec<u8>)>, usize) {
        let mut dec = FrameDecoder::new();
        let mut got = Vec::new();
        let mut best_batch = 0usize;
        let mut off = 0usize;
        while off < stream.len() {
            let end = (off + chunk).min(stream.len());
            let taken = dec.push(&stream[off..end]);
            let mut batch = 0usize;
            loop {
                let next = match dec.next_frame().expect("decode") {
                    Some(f) => (f.header.clone(), f.payload().to_vec()),
                    None => break,
                };
                got.push(next);
                batch += 1;
            }
            best_batch = best_batch.max(batch);
            assert!(taken > 0 || batch > 0, "no progress at offset {off} with chunk {chunk}");
            off += taken;
        }
        (got, best_batch)
    }

    /// The meter has to be shown to work before anything is allowed to rest on
    /// it. An allocation counter wired up wrong counts zero, and a test whose
    /// subject is "this allocated zero" would then pass for the one reason that
    /// proves nothing.
    #[test]
    fn the_allocation_meter_actually_counts() {
        let before = bytes_allocated_here();
        let v: Vec<u8> = Vec::with_capacity(8 << 20);
        let after = bytes_allocated_here();
        assert!(
            after - before >= 8 << 20,
            "the meter saw {} bytes for an 8 MiB allocation; every zero-allocation \
             assertion in this file is vacuous until this passes",
            after - before
        );
        drop(v);
    }

    /// Acceptance 1 (design §6, P2): a known frame sequence fed one byte at a
    /// time comes back byte for byte, and so does the same sequence fed in
    /// chunks that cut across frame boundaries.
    ///
    /// Injection control: make `write_frame`'s length little-endian read as big
    /// endian — or, closer to home, change `PAYLOAD_LEN_OFFSET` by one — and
    /// this goes red on the first frame.
    #[test]
    fn a_frame_sequence_survives_being_fed_one_byte_at_a_time() {
        let frames = sample_frames();
        let stream = encode_all(&frames);

        let (got, batch) = decode_in_chunks(&stream, 1);
        assert_eq!(got, frames, "one byte at a time must reproduce the input exactly");
        assert_eq!(batch, 1, "a single byte cannot complete two frames");

        // Chunk sizes chosen to land inside headers, inside payloads and across
        // frame boundaries: none of them divides any frame length here.
        for chunk in [2usize, 3, 7, 13, 39, 41, 97, 1000, 4137, 65536] {
            let (got, batch) = decode_in_chunks(&stream, chunk);
            assert_eq!(got, frames, "chunk size {chunk} changed the frame sequence");
            if chunk >= 97 {
                assert!(
                    batch >= 2,
                    "chunk size {chunk} never delivered two frames at once, so nothing \
                     proves a chunk ever straddled a frame boundary"
                );
            }
        }
    }

    /// Acceptance 2 (design §6, P2): a frame declaring a gigabyte of payload,
    /// with 8 bytes actually supplied, is refused **before anything is
    /// allocated**.
    ///
    /// Injection control (run 2026-08-07): put
    /// `let _v: Vec<u8> = Vec::with_capacity(declared);` above the
    /// `declared > MUX_MAX_PAYLOAD` check in `next_frame`. The delta becomes
    /// ~1 GiB and this goes red — which is the shape of the bug, a length off
    /// the wire sizing a buffer before anyone has decided it is plausible.
    #[test]
    fn an_oversized_payload_length_is_refused_before_anything_is_allocated() {
        const DECLARED: usize = 1 << 30;
        let mut evil = Vec::new();
        Header { payload_len: DECLARED as u32, ..media_header(1, 0) }
            .encode_append(&[], &mut evil);
        assert_eq!(evil.len(), HEADER_LEN, "the header must be intact and complete");
        evil.extend_from_slice(&[0u8; 8]); // ...and 8 bytes of the promised gigabyte

        // Constructed outside the measured window: `new` is the one allocation
        // this type is allowed, and the claim is about what the wire can cause.
        let mut dec = FrameDecoder::new();

        let before = bytes_allocated_here();
        let taken = dec.push(&evil);
        let err = dec.next_frame().expect_err("a gigabyte payload must be refused");
        let spent = bytes_allocated_here() - before;

        assert_eq!(taken, evil.len(), "the bytes were accepted; it is the length that is rejected");
        assert_eq!(err, FramedError::PayloadTooLarge { declared: DECLARED });
        assert_eq!(
            spent, 0,
            "refusing the frame allocated {spent} bytes; a declared length must never \
             reach an allocator, however plausible the code path looks"
        );
    }

    /// The refusal survives the frame being dribbled in, which is the case that
    /// matters: an attacker sends the header and then nothing, and a decoder
    /// that sized a buffer up front would be holding a gigabyte while waiting
    /// for bytes that never come.
    #[test]
    fn the_oversized_refusal_does_not_wait_for_the_promised_bytes() {
        let mut evil = Vec::new();
        Header { payload_len: u32::MAX, ..media_header(1, 0) }.encode_append(&[], &mut evil);

        let mut dec = FrameDecoder::new();
        let before = bytes_allocated_here();
        for (i, byte) in evil.iter().enumerate() {
            assert_eq!(dec.push(std::slice::from_ref(byte)), 1);
            match dec.next_frame() {
                Ok(None) => assert!(i < HEADER_LEN - 1, "refusal must come as the header completes"),
                Err(FramedError::PayloadTooLarge { declared }) => {
                    assert_eq!(declared, u32::MAX as usize);
                    assert_eq!(i, HEADER_LEN - 1, "refused at the wrong byte");
                    assert_eq!(bytes_allocated_here() - before, 0, "the wait allocated");
                    return;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("the decoder never refused a payload of u32::MAX");
    }

    /// The frame layer's one piece of private knowledge about the header — where
    /// `payload_len` lives — checked against the real encoder rather than against
    /// a copy of the same constant.
    ///
    /// Injection control: swap the order in which `encode_append` writes
    /// `timestamp_us` and `payload_len`. `HEADER_LEN` is still 40, so the static
    /// assertion beside `PAYLOAD_LEN_OFFSET` stays green — that one only pins
    /// the offset against the header's *length*, and a reordering leaves the
    /// length alone. This test is what notices that the four bytes at offset 36
    /// stopped being the length.
    #[test]
    fn the_payload_length_field_is_where_the_frame_layer_looks_for_it() {
        for len in [0usize, 1, 40, 960, MUX_MAX_PAYLOAD] {
            let mut bytes = Vec::new();
            media_header(1, len).encode_append(&vec![0u8; len], &mut bytes);
            let read = u32::from_le_bytes(
                bytes[PAYLOAD_LEN_OFFSET..PAYLOAD_LEN_OFFSET + 4].try_into().unwrap(),
            ) as usize;
            assert_eq!(read, len, "the frame layer reads the wrong four bytes as the length");
            assert_eq!(bytes.len(), HEADER_LEN + len);
        }
    }

    /// A sealed media datagram is already a frame. This is decision B's whole
    /// payoff — no re-encode on the media path — so it is asserted rather than
    /// left as a remark, using the encoder the UDP path actually uses.
    #[test]
    fn a_udp_datagram_is_already_a_valid_frame() {
        let payload = vec![0xC3; 960];
        let datagram = media_header(9, payload.len()).encode(&payload);

        let mut dec = FrameDecoder::new();
        assert_eq!(dec.push(&datagram), datagram.len());
        let frame = dec.next_frame().expect("decode").expect("one whole frame");
        assert_eq!(frame.bytes(), &datagram[..], "the frame is the datagram, byte for byte");
        assert_eq!(frame.payload(), &payload[..]);
        assert_eq!(&frame.bytes()[..4], &MAGIC, "the header travels with the payload as AAD");
    }

    /// The documented drain loop cannot stall. The buffer is exactly one
    /// maximum frame, so "full" and "no complete frame" cannot both hold —
    /// anything that would need more room was refused as it was declared.
    #[test]
    fn the_decoder_cannot_stall_with_a_full_buffer_and_no_frame() {
        let biggest = {
            let mut v = Vec::new();
            encode_frame(&media_header(1, 0), &vec![0u8; MUX_MAX_PAYLOAD], &mut v).expect("encode");
            v
        };
        assert_eq!(biggest.len(), MUX_MAX_FRAME, "the largest legal frame fills the buffer exactly");

        let mut dec = FrameDecoder::new();
        assert_eq!(dec.push(&biggest), MUX_MAX_FRAME);
        assert_eq!(dec.push(b"more"), 0, "the buffer really is full at this point");
        assert!(
            dec.next_frame().expect("decode").is_some(),
            "a full buffer must always contain a frame, or the caller has no way to make progress"
        );
        assert_eq!(dec.push(b"more"), 4, "draining a frame must free the room it occupied");
    }

    /// A framing error is terminal. The alternative — log and continue — means
    /// resynchronising on boundaries chosen by whoever corrupted the stream.
    #[test]
    fn a_framing_error_is_terminal() {
        let mut bytes = Vec::new();
        encode_frame(&media_header(1, 0), b"hello", &mut bytes).expect("encode");
        bytes[0] ^= 0xFF; // break MAGIC

        let mut dec = FrameDecoder::new();
        dec.push(&bytes);
        assert_eq!(
            dec.next_frame().expect_err("bad magic must be refused"),
            FramedError::Header(PacketError::BadMagic),
            "header validation belongs to Header::parse and must reach the caller intact"
        );

        // A valid frame arriving afterwards must not resurrect the stream.
        let mut good = Vec::new();
        encode_frame(&media_header(2, 0), b"hello", &mut good).expect("encode");
        assert_eq!(dec.push(&good), 0, "a poisoned decoder accepts nothing further");
        assert_eq!(dec.next_frame().expect_err("still poisoned"), FramedError::Poisoned);
    }

    /// An unknown `Kind` reaches the caller as a refusal rather than as a frame,
    /// which is what makes a version mismatch loud on this transport.
    #[test]
    fn an_unknown_kind_is_refused_by_the_frame_layer() {
        let mut bytes = Vec::new();
        encode_frame(&media_header(1, 0), b"x", &mut bytes).expect("encode");
        bytes[5] = 7; // one past MuxKeepalive

        let mut dec = FrameDecoder::new();
        dec.push(&bytes);
        assert_eq!(
            dec.next_frame().expect_err("kind 7 is not a kind"),
            FramedError::Header(PacketError::BadKind)
        );
    }

    /// The encoder refuses what the decoder would refuse. Without this, an
    /// oversized control chunk becomes a frame that only fails at the far end,
    /// where the diagnosis is "the peer sent garbage".
    #[test]
    fn the_encoder_refuses_a_payload_the_decoder_would_reject() {
        let mut out = Vec::new();
        let err = encode_frame(&control_header(), &vec![0u8; MUX_MAX_PAYLOAD + 1], &mut out)
            .expect_err("one byte over the limit must not encode");
        assert_eq!(err, FramedError::PayloadTooLarge { declared: MUX_MAX_PAYLOAD + 1 });
        assert!(out.is_empty(), "a refused frame must not leave a partial frame in the buffer");

        encode_frame(&control_header(), &vec![0u8; MUX_MAX_PAYLOAD], &mut out)
            .expect("exactly at the limit must encode");
        assert_eq!(out.len(), MUX_MAX_FRAME);
    }

    /// The payload is the sole authority on the declared length. A header whose
    /// `payload_len` disagrees is corrected, not honoured — the alternative
    /// desynchronises the stream and is discovered frames later.
    #[test]
    fn the_encoder_ignores_a_header_payload_len_that_disagrees_with_the_payload() {
        let lying = Header { payload_len: 9999, ..media_header(1, 0) };
        let mut out = Vec::new();
        encode_frame(&lying, b"four", &mut out).expect("encode");
        assert_eq!(out.len(), HEADER_LEN + 4);

        let mut dec = FrameDecoder::new();
        dec.push(&out);
        let frame = dec.next_frame().expect("decode").expect("a frame");
        assert_eq!(frame.header.payload_len, 4, "the payload decides, not the header field");
        assert_eq!(frame.payload(), b"four");
    }

    /// Control and keepalive headers are frozen in one place so tier 1 and tier
    /// 2 cannot drift into two dialects. The audio fields are filler; the point
    /// of pinning them is that they are pinned *somewhere*, not their values.
    #[test]
    fn the_control_and_keepalive_headers_are_pinned_in_one_place() {
        let c = control_header();
        assert_eq!(c.kind, Kind::Control);
        assert_eq!(c.codec, Codec::Passthrough, "a control frame is not audio");
        assert_eq!((c.channels, c.sample_rate, c.stream_id, c.session_id), (0, 0, 0, 0));

        let k = keepalive_header();
        assert_eq!(k.kind, Kind::MuxKeepalive);
        assert_eq!(k.codec, c.codec, "keepalive must not invent a second dialect");

        // Both survive a round trip, which is the part that would break if a
        // filler value were ever set to something Header::parse rejects.
        for h in [c, k] {
            let mut out = Vec::new();
            encode_frame(&h, &[], &mut out).expect("encode");
            let mut dec = FrameDecoder::new();
            dec.push(&out);
            let f = dec.next_frame().expect("decode").expect("a frame");
            assert_eq!(f.header.kind, h.kind);
            assert!(f.payload().is_empty());
        }
    }

    /// Steady-state decoding allocates nothing at all: the buffer is obtained
    /// once and every frame is a borrow out of it. Guards against a later
    /// "simplification" that returns owned payloads.
    #[test]
    fn decoding_a_stream_allocates_nothing_after_construction() {
        let stream = encode_all(&sample_frames());
        let mut dec = FrameDecoder::new();

        let before = bytes_allocated_here();
        let mut seen = 0usize;
        let mut off = 0usize;
        while off < stream.len() {
            off += dec.push(&stream[off..]);
            loop {
                match dec.next_frame().expect("decode") {
                    Some(frame) => {
                        std::hint::black_box(frame.bytes());
                        seen += 1;
                    }
                    None => break,
                }
            }
        }
        let spent = bytes_allocated_here() - before;

        assert_eq!(seen, sample_frames().len(), "every frame must have been decoded");
        assert_eq!(spent, 0, "decoding allocated {spent} bytes; it is supposed to be a borrow");
    }
}
