//! P6: the WebSocket shell tier 2 runs inside (`docs/design-m8-fallback.md`
//! §4.3, §6 P6; `docs/plan.md` §16).
//!
//! Tier 2 already carries control and both media directions over one
//! connection. This module changes only what that connection *is*: a
//! `Sec-WebSocket-Key` handshake and RFC 6455 framing instead of a bare TCP
//! stream. Nothing above it changes — [`WsReader`] is a `Read` and [`WsWriter`]
//! is a `Write`, so `mux::read_loop` and `mux::write_loop` see the same shapes
//! they saw in P5.
//!
//! # Why a WebSocket at all
//!
//! Design §4.3, in order of strength:
//!
//! 1. **Paths that only pass HTTP(S) only open for `Upgrade: websocket`.**
//!    Corporate proxies, L7 gateways, Cloudflare Tunnel's http mode, ngrok,
//!    nginx `proxy_pass` + `proxy_http_version 1.1` — none of them forward a
//!    bare TCP connection to port 47810, and many forbid `CONNECT` to
//!    non-443 ports. The upgrade is the one documented way to obtain a
//!    bidirectional byte stream through them.
//! 2. **Message boundaries.** Decision B's one genuine hazard is a
//!    `payload_len` read before anything is authenticated. Here the library
//!    bounds the message (`max_message_size`), and one frame is one binary
//!    message, so the length in our header is checked against a length the
//!    peer's transport already agreed to. `FrameDecoder` still does its own
//!    bounds check — belt and braces, and it is what keeps the raw-TCP mux and
//!    this one identical above the carrier.
//! 3. TLS-terminating gateways re-encrypt. Our AEAD is inside, and identity is
//!    a fingerprint rather than a source address (plan §4), so the rewrite in
//!    the middle changes nothing.
//! 4. Native `Ping`/`Pong`, which is exactly the keepalive plan §16 wants
//!    against tunnel idle timeouts. See [`WsWriter::tick`].
//!
//! # The three things the design fixed in advance
//!
//! - **`permessage-deflate` is not negotiated, and that is asserted rather than
//!   assumed.** Compressing an already-encrypted payload is CPU for nothing and
//!   adds a variable-latency stage. tungstenite has no compression support at
//!   all — no `flate2` in its dependency tree under any feature — so the
//!   extension cannot be offered or accepted by accident; [`assert_no_deflate`]
//!   still checks both the request we send and the response we get, because
//!   "cannot happen" is not the same claim as "did not happen", and a gateway
//!   that injected the header would otherwise be found much later, as
//!   corruption.
//! - **Client→server frames are masked** (a 4-byte key XORed over the whole
//!   payload, RFC 6455 §5.3), and that is mandatory, not a choice: a server
//!   must reject unmasked client frames. At ~200 frames/s of ~1 KiB it is a few
//!   hundred KiB/s of XOR, which is nothing — **but it is written down here so
//!   that a future profile showing an unexplained memcpy-shaped cost in the
//!   send path has an explanation waiting instead of a mystery.** It applies
//!   only to the dialling side; the accepting side sends unmasked.
//! - **The server lives in the daemon, not in the App** (design §9 item 5).
//!   §7.5's layering is about *static pages*; this is peer traffic.
//!
//! # Threading: why there are two `WebSocket` objects and not one
//!
//! The mux has a reader thread parked in `read` and a writer thread that must
//! not wait for it. A single `WebSocket` cannot serve both, and not for the
//! usual reason:
//!
//! - `WebSocketContext::read` **writes** — it flushes queued Pong and Close
//!   replies before every read (tungstenite 0.24, `protocol/mod.rs`). So the
//!   "reader" is a writer too, and two threads sharing one object would need a
//!   lock.
//! - That lock cannot be made cheap here. `try_clone` duplicates a descriptor,
//!   and `SO_RCVTIMEO` lives on the *open file description*, so both halves
//!   share one read timeout. A reader holding the lock across a blocking read
//!   would hold it for that whole timeout and then immediately re-take it,
//!   which is writer starvation with extra steps.
//!
//! So each thread gets its own `WebSocket` over its own duplicated descriptor,
//! and **only the writer's instance is allowed to touch the socket's send
//! side** — the reader's writes go to a sink. Two WebSocket framers writing to
//! one socket would interleave frames and corrupt the stream, so this is not a
//! tidiness rule. The protocol replies the reader would have sent are handed to
//! the writer as *messages* (see [`WsShared`]) and re-serialised by the one
//! framer that owns the wire. Splicing the reader's already-encoded bytes would
//! have worked too, but only with a partial-write rule at every call site;
//! moving a `Vec<u8>` is the same thing without the rule.
//!
//! # One frame, one message
//!
//! `write_one_frame` and `mux::write_control_frame` each hand [`WsWriter`] a
//! whole mux frame in a single `write` call, and this module turns each such
//! call into exactly one binary message. That is what makes point 2 above true,
//! and it buys a property the raw-TCP carrier cannot have: **a frame that
//! cannot go out is dropped whole.** On TCP a half-written frame must be
//! finished or the peer reads the next frame's bytes as this one's payload; on
//! a message transport there is no half. The stale gate is therefore free to
//! fire at any point before acceptance with no chance of desynchronising.

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use audiohub_net::framed::MUX_MAX_FRAME;
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::{Role, WebSocket, WebSocketConfig};
use tungstenite::{Error as WsError, Message};

/// The extension the design forbids, spelled once.
const DEFLATE: &str = "permessage-deflate";

/// The header both sides are checked for.
const EXTENSIONS: &str = "Sec-WebSocket-Extensions";

/// How long a peer has to complete the HTTP upgrade.
///
/// Matches `conn::HANDSHAKE_TIMEOUT`'s role rather than its value: this bound
/// exists because the mux's own handshake deadline (P5's review fix) does not
/// start until the upgrade is done, so without one here a peer could hold a
/// preauth slot by opening a connection and never finishing the request line.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);

/// A refused upgrade beyond this is an attack, not a long header.
///
/// tungstenite enforces 64 KiB of its own; this is the same bound applied to
/// the bytes we hand it, so [`HeaderOnly`] can never be walked past the end of
/// a header that has no end.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// How often an otherwise idle connection sends a `Ping`.
///
/// **Chosen against the shortest idle timeout we expect to meet, not against a
/// round number.** nginx's `proxy_read_timeout` defaults to 60 s and is the
/// most common thing in front of one of these tunnels; Cloudflare's is ~100 s.
/// 20 s gives two full misses before the tightest of those fires, which is what
/// makes the keepalive survive a lost `Pong` rather than merely a quiet link.
/// plan §16 does not promise exhaustive tunnel compatibility, so this is a
/// default and not a claim.
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(20);


// ------------------------------------------------------------------ addresses

/// A peer address that says what transport it is (`plan.md` §16.2).
///
/// tier 2 is manual precisely because its premise — "the tunnel only forwards
/// at the application layer" — is a property of the user's network that cannot
/// be observed from here: a peer that cannot be dialled and a peer that is
/// switched off produce the same silence. So the *form of the address* is the
/// decision. `ws://host/path` is a request for the WebSocket carrier in the
/// same way `192.168.1.5:47810` is a request for a direct connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WsUrl {
    /// `true` for `wss://`. Recorded so the refusal can be specific.
    pub(crate) tls: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// Path plus query, always starting with `/`.
    pub(crate) path: String,
}

impl WsUrl {
    /// Is this string shaped like a WebSocket URL at all?
    ///
    /// Separate from [`WsUrl::parse`] so that "this is a URL and it is broken"
    /// stays distinguishable from "this is a `host:port`". Without the split a
    /// typo in the scheme would silently fall back to a TCP dial of a host
    /// literally named `ws`.
    pub(crate) fn looks_like_url(s: &str) -> bool {
        let s = s.trim();
        let low = s.to_ascii_lowercase();
        low.starts_with("ws://") || low.starts_with("wss://")
    }

    /// `ws://host[:port][/path]`.
    ///
    /// Hand-parsed rather than pulling in `url`: the grammar we accept is three
    /// fields, and this crate's dependency graph is deliberately small (see the
    /// notes in `audiohub-net/Cargo.toml` about `windows-sys` arriving through
    /// incidental crates).
    pub(crate) fn parse(s: &str) -> Result<WsUrl> {
        let s = s.trim();
        let low = s.to_ascii_lowercase();
        let (tls, rest) = if let Some(r) = low.strip_prefix("ws://") {
            (false, &s[s.len() - r.len()..])
        } else if let Some(r) = low.strip_prefix("wss://") {
            (true, &s[s.len() - r.len()..])
        } else {
            bail!("not a WebSocket URL: {s} (expected ws://host[:port][/path])");
        };
        if rest.is_empty() {
            bail!("WebSocket URL has no host: {s}");
        }
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        if authority.contains('@') {
            bail!("userinfo is not supported in a peer URL: {s}");
        }
        // IPv6 literals are bracketed, and the colon rule differs inside them.
        let (host, port) = if let Some(close) = authority.strip_prefix('[') {
            let Some(i) = close.find(']') else { bail!("unterminated IPv6 literal in {s}") };
            let h = &close[..i];
            let tail = &close[i + 1..];
            let p = match tail.strip_prefix(':') {
                Some(p) => p.parse::<u16>().with_context(|| format!("port in {s}"))?,
                None if tail.is_empty() => default_port(tls),
                None => bail!("unexpected text after the IPv6 literal in {s}"),
            };
            (format!("[{h}]"), p)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => {
                    (h.to_string(), p.parse::<u16>().with_context(|| format!("port in {s}"))?)
                }
                None => (authority.to_string(), default_port(tls)),
            }
        };
        if host.is_empty() || host == "[]" {
            bail!("WebSocket URL has no host: {s}");
        }
        Ok(WsUrl { tls, host, port, path })
    }

    /// What to hand `TcpStream::connect`, before resolution.
    pub(crate) fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The request URI, normalised. Always `ws://` — see [`WsUrl::require_plaintext`].
    fn request_uri(&self) -> String {
        format!("ws://{}:{}{}", self.host, self.port, self.path)
    }

    /// **`wss://` is recognised and refused, on purpose.**
    ///
    /// Terminating TLS here would mean adding `rustls` or `native-tls` to a
    /// dependency graph this project keeps deliberately narrow — the
    /// windows-gnu target's raw-dylib problem is why `dirs` and `gethostname`
    /// are hand-rolled two crates over. None of P6's acceptance criteria touch
    /// TLS, and the deployments §4.3 names (Cloudflare Tunnel, ngrok, an nginx
    /// front) terminate TLS at the gateway and speak plaintext to the origin,
    /// which is where this daemon sits.
    ///
    /// So the honest state is: the *form* parses, so a user's address is
    /// understood and the error names the real gap, rather than the scheme
    /// being unrecognised and the address being read as a hostname.
    pub(crate) fn require_plaintext(&self) -> Result<()> {
        if self.tls {
            bail!(
                "wss:// is not supported yet: this build has no TLS client (adding one is a \
                 dependency decision, not a flag). Point the peer at the tunnel's plaintext \
                 origin with ws://, or terminate TLS in front of this daemon."
            );
        }
        Ok(())
    }
}

fn default_port(tls: bool) -> u16 {
    if tls {
        443
    } else {
        80
    }
}

// ------------------------------------------------------------------ counters

/// What the reader tells the writer, and what both tell the status output.
///
/// The pong queue is the whole reason this type is shared: tungstenite answers
/// an inbound `Ping` on the instance that read it, and that instance's writes
/// go nowhere (see the module note), so the reply has to be re-sent by the
/// writer. Losing it is not subtle — the peer stops seeing us as alive and its
/// tunnel reaps the connection — but it *is* invisible locally, which is why
/// `pongs_written` is counted and asserted rather than assumed.
#[derive(Default)]
pub(crate) struct WsShared {
    pongs_due: Mutex<Vec<Vec<u8>>>,
    pub(crate) pings_written: AtomicU64,
    pub(crate) pongs_written: AtomicU64,
    pub(crate) pings_read: AtomicU64,
    pub(crate) pongs_read: AtomicU64,
    pub(crate) messages_read: AtomicU64,
    pub(crate) messages_written: AtomicU64,
}

impl WsShared {
    fn owe_pong(&self, payload: Vec<u8>) {
        let mut q = self.pongs_due.lock().unwrap_or_else(|e| e.into_inner());
        // A peer that pings faster than we drain cannot make us allocate: the
        // newest answer is the only one worth sending, and RFC 6455 §5.5.3
        // says exactly that — "if more than one Ping is received before a
        // response, respond to the most recent".
        if q.len() >= 4 {
            q.remove(0);
        }
        q.push(payload);
    }

    fn take_pongs(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.pongs_due.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

// ------------------------------------------------------------------ handshake

/// A reader that stops dead at the end of the HTTP header.
///
/// # Why this exists at all
///
/// A tier 2 dialler sends `VerifyHello` the instant the upgrade completes, so
/// the peer's first frame is routinely in the socket already when the handshake
/// finishes reading. `tungstenite::client`/`accept` read in 4 KiB chunks, so
/// they see those bytes, and what happens next differs by side — **both bad,
/// and neither the same bad**:
///
/// - **Server: a hard refusal.** `accept` rejects any tail outright with
///   `Junk after client request`. So without this wrapper the accept path does
///   not merely lose data, it **does not work at all** whenever the dialler's
///   request and first frame land in one segment — which is the normal case,
///   not an edge one. Measured, not reasoned: see the injection control on
///   `the_handshake_leaves_no_bytes_behind`.
/// - **Client: a silent loss.** `client` keeps the tail inside the returned
///   `WebSocket` (`StageResult::DoneReading { tail, .. }`) and there is no
///   accessor for it, so splitting the socket afterwards — which the threading
///   model above requires — drops those bytes with no error anywhere.
///
/// Feeding tungstenite only up to and including `\r\n\r\n` makes the tail empty
/// *by construction*, which removes both.
///
/// # Why it peeks instead of reading one byte at a time
///
/// One byte per call is the obvious way to guarantee no over-read, and
/// tungstenite rejects it: `AttackCheck` fails a handshake once
/// `packets * 128 > bytes` past 64 packets, so a ~200-byte header dribbled a
/// byte at a time is refused as an attack. (Found by reading
/// `handshake/machine.rs` before writing this, not by debugging it afterwards.)
/// `peek` gives the same guarantee in whole chunks: look at what is there,
/// consume only as far as the terminator.
struct HeaderOnly {
    sock: TcpStream,
    /// How much of `\r\n\r\n` the bytes consumed so far end with.
    matched: usize,
    done: bool,
    total: usize,
}

impl HeaderOnly {
    fn new(sock: TcpStream) -> HeaderOnly {
        HeaderOnly { sock, matched: 0, done: false, total: 0 }
    }
}

/// Advance the `\r\n\r\n` state machine over `data`, returning how many bytes
/// may be consumed and the state after them.
fn scan_header_end(start: usize, data: &[u8]) -> (usize, usize, bool) {
    let mut m = start;
    for (i, &b) in data.iter().enumerate() {
        m = match (m, b) {
            (0, b'\r') => 1,
            (1, b'\n') => 2,
            (2, b'\r') => 3,
            (3, b'\n') => 4,
            (_, b'\r') => 1,
            _ => 0,
        };
        if m == 4 {
            return (i + 1, 4, true);
        }
    }
    (data.len(), m, false)
}

impl Read for HeaderOnly {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            // The header is complete; anything further belongs to the frame
            // layer. `WouldBlock` rather than EOF because EOF would be read as
            // a truncated handshake.
            return Err(io::Error::new(ErrorKind::WouldBlock, "header complete"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        // Peek into the caller's buffer, decide how far the header goes, then
        // consume exactly that much. The re-read overwrites the same bytes.
        let n = (&self.sock).peek(buf)?;
        if n == 0 {
            return Ok(0);
        }
        let (take, matched, done) = scan_header_end(self.matched, &buf[..n]);
        (&self.sock).read_exact(&mut buf[..take])?;
        self.matched = matched;
        self.done = done;
        self.total += take;
        if self.total > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("HTTP upgrade header exceeded {MAX_HEADER_BYTES} bytes"),
            ));
        }
        Ok(take)
    }
}

impl Write for HeaderOnly {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.sock).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&self.sock).flush()
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        // Eager: a buffered audio frame is a late audio frame.
        write_buffer_size: 0,
        // Back-pressure rather than growth, the same rule the media queue
        // follows. Two frames of slack so a `Ping` queued behind a media frame
        // is not refused.
        max_write_buffer_size: MUX_MAX_FRAME * 4,
        // **Design §4.3 point 2, as a number.** One mux frame is one message,
        // so the transport's own bound is exactly the frame layer's.
        max_message_size: Some(MUX_MAX_FRAME),
        max_frame_size: Some(MUX_MAX_FRAME),
        // RFC 6455: a server must reject unmasked client frames. Some libraries
        // send them anyway; we are not one of them and do not tolerate them.
        accept_unmasked_frames: false,
        ..Default::default()
    }
}

/// Refuse a connection that negotiated compression.
///
/// Reached for both the request we send and the response we get. **Failing
/// loudly is the point**: tungstenite cannot decompress, so a peer that
/// believed the extension was on would send frames with RSV1 set and every one
/// of them would be rejected mid-stream, long after the cause. Naming it at the
/// handshake turns a corrupted session into a refused one.
fn assert_no_deflate(where_: &str, headers: &tungstenite::http::HeaderMap) -> Result<()> {
    for v in headers.get_all(EXTENSIONS).iter() {
        let s = String::from_utf8_lossy(v.as_bytes()).to_ascii_lowercase();
        if s.contains(DEFLATE) {
            bail!(
                "{where_} carries {EXTENSIONS}: {s}, which offers or accepts {DEFLATE}; \
                 compressing an already-encrypted payload buys nothing and adds a \
                 variable-latency stage, so the design forbids it (§4.3)"
            );
        }
    }
    Ok(())
}

/// Run the client upgrade, then split the socket.
///
/// The returned reader/writer pair is positioned at the first WebSocket frame,
/// with nothing buffered anywhere else — see [`HeaderOnly`].
pub(crate) fn connect(sock: TcpStream, url: &WsUrl) -> Result<(WsReader, WsWriter)> {
    url.require_plaintext()?;
    let uri = url.request_uri();
    let req = uri
        .as_str()
        .into_client_request()
        .with_context(|| format!("build a WebSocket upgrade request for {uri}"))?;
    // **We do not offer it**, and this is where that is a fact about the bytes
    // going out rather than a belief about a library's defaults.
    assert_no_deflate("the upgrade request we were about to send", req.headers())?;

    arm_handshake_timeouts(&sock)?;
    let hs = HeaderOnly::new(sock.try_clone().context("clone the socket for the upgrade")?);
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    let mut attempt = tungstenite::client::client_with_config(req, hs, Some(ws_config()));
    let (done, resp) = loop {
        match attempt {
            Ok(v) => break v,
            // Resumed, never restarted: the request has already gone out and a
            // second one would be a second handshake on the same socket.
            Err(tungstenite::handshake::HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline {
                    bail!("the WebSocket upgrade to {uri} did not complete within {UPGRADE_TIMEOUT:?}");
                }
                attempt = mid.handshake();
            }
            Err(e) => bail!("the WebSocket upgrade to {uri} failed: {e}"),
        }
    };
    assert_no_deflate("the upgrade response", resp.headers())?;
    drop(done);
    split(sock, Role::Client)
}

/// Run the server upgrade on an accepted socket, then split it.
pub(crate) fn accept(sock: TcpStream) -> Result<(WsReader, WsWriter)> {
    arm_handshake_timeouts(&sock)?;
    let hs = HeaderOnly::new(sock.try_clone().context("clone the socket for the upgrade")?);
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    let mut attempt = tungstenite::accept_hdr_with_config(hs, decline_extensions, Some(ws_config()));
    let done = loop {
        match attempt {
            Ok(v) => break v,
            Err(tungstenite::handshake::HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline {
                    bail!("an inbound WebSocket upgrade did not complete within {UPGRADE_TIMEOUT:?}");
                }
                attempt = mid.handshake();
            }
            Err(e) => bail!("an inbound WebSocket upgrade failed: {e}"),
        }
    };
    drop(done);
    split(sock, Role::Server)
}

/// The server-side half of the no-compression rule.
///
/// A client is free to **offer** extensions; a server declines by not echoing
/// them, which tungstenite does by construction because it has no compression
/// code to echo with. The check is therefore on **our own response**, which is
/// the artefact that decides what is negotiated — asserting on the peer's offer
/// would be asserting on something we do not control and that does not bind us.
fn decline_extensions(
    req: &Request,
    resp: Response,
) -> std::result::Result<Response, ErrorResponse> {
    let offered: Vec<String> = req
        .headers()
        .get_all(EXTENSIONS)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if !offered.is_empty() {
        crate::dlog!(
            "[audiohubd] tier2 ws: the peer offered {EXTENSIONS}: {}; declining all of them",
            offered.join(", ")
        );
    }
    if assert_no_deflate("the upgrade response we were about to send", resp.headers()).is_err() {
        let mut err = ErrorResponse::new(Some(
            "this endpoint never negotiates permessage-deflate (design §4.3)".into(),
        ));
        *err.status_mut() = tungstenite::http::StatusCode::BAD_REQUEST;
        return Err(err);
    }
    Ok(resp)
}

/// Bound the upgrade at the socket, so a peer that stops mid-header produces
/// `WouldBlock` and the loops above can enforce [`UPGRADE_TIMEOUT`].
///
/// Without this the dialling side has no read timeout at all at this point
/// (`TcpStream::connect_timeout` sets none), and a silent peer would hold the
/// thread forever — the same shape as the preauth hole P5's review found in
/// `verify_*`, arriving one layer lower.
fn arm_handshake_timeouts(sock: &TcpStream) -> Result<()> {
    sock.set_nonblocking(false)?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    sock.set_write_timeout(Some(Duration::from_millis(200)))?;
    Ok(())
}

/// Build the two half-duplex WebSocket framers over duplicated descriptors.
///
/// Called only after a handshake that left no tail (see [`HeaderOnly`]), so
/// `from_raw_socket` is exact rather than approximate.
fn split(sock: TcpStream, role: Role) -> Result<(WsReader, WsWriter)> {
    let rd = sock.try_clone().context("clone the socket for the ws reader")?;
    let wr = sock.try_clone().context("clone the socket for the ws writer")?;
    let shared = Arc::new(WsShared::default());
    let reader = WsReader {
        ws: WebSocket::from_raw_socket(RecvHalf { sock: rd }, role, Some(ws_config())),
        rem: Vec::new(),
        off: 0,
        shared: shared.clone(),
    };
    let writer = WsWriter {
        ws: WebSocket::from_raw_socket(SendHalf { sock: wr }, role, Some(ws_config())),
        shared,
        interval: PING_INTERVAL,
        next_ping: Instant::now() + PING_INTERVAL,
    };
    Ok((reader, writer))
}

// ------------------------------------------------------------------ the halves

/// The reader's view: reads from the socket, and **discards** what the framer
/// would have written.
///
/// See the module note. The discarded bytes are queued Pong/Close replies,
/// which are re-sent as messages by the writer's framer instead; letting them
/// out here would put two independent framers on one socket.
struct RecvHalf {
    sock: TcpStream,
}

impl Read for RecvHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.sock).read(buf)
    }
}

impl Write for RecvHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The writer's view: writes to the socket and never reads.
struct SendHalf {
    sock: TcpStream,
}

impl Read for SendHalf {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        // Not EOF: `WebSocketContext` treats a closed read as a terminated
        // connection, and this half is simply not the one that reads.
        Err(io::Error::new(ErrorKind::WouldBlock, "the ws writer half does not read"))
    }
}

impl Write for SendHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.sock).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&self.sock).flush()
    }
}

// ------------------------------------------------------------------ read side

/// The mux reader's carrier. Yields the payload of each binary message.
pub(crate) struct WsReader {
    ws: WebSocket<RecvHalf>,
    rem: Vec<u8>,
    off: usize,
    shared: Arc<WsShared>,
}

impl WsReader {
    pub(crate) fn shared(&self) -> &Arc<WsShared> {
        &self.shared
    }
}

fn would_block(what: &'static str) -> io::Error {
    io::Error::new(ErrorKind::WouldBlock, what)
}

fn blocked(e: &WsError) -> bool {
    matches!(e, WsError::Io(io) if crate::tcpmedia::blocked(io.kind()))
}

fn to_io(e: WsError) -> io::Error {
    match e {
        WsError::Io(io) => io,
        other => io::Error::new(ErrorKind::Other, other.to_string()),
    }
}

impl Read for WsReader {
    /// `mux::read_loop` reads this exactly as it reads a `TcpStream`, so the
    /// contract has to be a stream's: hand back message payloads in order, and
    /// report the protocol traffic between them as "nothing yet" rather than as
    /// end of stream.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.off < self.rem.len() {
                let n = out.len().min(self.rem.len() - self.off);
                out[..n].copy_from_slice(&self.rem[self.off..self.off + n]);
                self.off += n;
                return Ok(n);
            }
            match self.ws.read() {
                Ok(Message::Binary(b)) => {
                    self.shared.messages_read.fetch_add(1, Ordering::Relaxed);
                    self.rem = b;
                    self.off = 0;
                    if self.rem.is_empty() {
                        // A zero-length message is legal and is not EOF.
                        return Err(would_block("empty binary message"));
                    }
                }
                Ok(Message::Ping(p)) => {
                    self.shared.pings_read.fetch_add(1, Ordering::Relaxed);
                    self.shared.owe_pong(p);
                    return Err(would_block("ping"));
                }
                Ok(Message::Pong(_)) => {
                    self.shared.pongs_read.fetch_add(1, Ordering::Relaxed);
                    return Err(would_block("pong"));
                }
                Ok(Message::Close(_)) => return Ok(0),
                // Nothing on this transport is text, and a peer sending it is
                // not a peer of ours having a bad day — it is something else
                // entirely on the socket.
                Ok(Message::Text(_)) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "a text message arrived on a binary-only transport",
                    ))
                }
                Ok(Message::Frame(_)) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "a raw frame surfaced from a reading WebSocket",
                    ))
                }
                Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => return Ok(0),
                Err(e) if blocked(&e) => return Err(would_block("no message yet")),
                Err(e) => return Err(to_io(e)),
            }
        }
    }
}

// ------------------------------------------------------------------ write side

/// The mux writer's carrier. One `write` call is one binary message.
pub(crate) struct WsWriter {
    ws: WebSocket<SendHalf>,
    shared: Arc<WsShared>,
    interval: Duration,
    next_ping: Instant,
}

impl WsWriter {
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<WsShared> {
        &self.shared
    }

    /// Shorten the heartbeat for a test.
    ///
    /// **Deliberately not an env knob.** An `AUDIOHUB_TEST_WS_PING_MS` in the
    /// shape of `AUDIOHUB_TEST_TX_KBPS` was written and then removed: this
    /// suite runs its daemons in-process and in parallel, so `set_var` reaches
    /// into every other test's connection, and the one test that would have
    /// used it (the 90 s acceptance) needs the real interval anyway. A hook
    /// nothing calls is a comment claiming a capability that does not exist.
    #[cfg(test)]
    pub(crate) fn set_ping_interval(&mut self, d: Duration) {
        self.interval = d;
        self.next_ping = Instant::now() + d;
    }

    /// Push whatever the framer still holds.
    ///
    /// `Ok` means its buffer is empty, which is what makes the enqueue in
    /// [`Write::write`] unable to be refused, and therefore what makes
    /// "`WouldBlock` ⇒ nothing of this frame was taken" true.
    fn pump(&mut self) -> io::Result<()> {
        match self.ws.flush() {
            Ok(()) => Ok(()),
            Err(e) if blocked(&e) => Err(would_block("send buffer full")),
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                Err(io::Error::new(ErrorKind::BrokenPipe, "websocket closed"))
            }
            Err(e) => Err(to_io(e)),
        }
    }

    /// Hand one message to the framer.
    ///
    /// **A blocked flush inside `write` is not a refusal.** tungstenite buffers
    /// the message first and only then tries to push it, so an `Io(WouldBlock)`
    /// out of `WebSocket::write` means "accepted, not yet on the wire". Mapping
    /// it to `WouldBlock` would make `write_one_frame` retry from offset zero
    /// and put the frame on the wire **twice** — the whole reason this is a
    /// method with a comment rather than a `?`.
    fn enqueue(&mut self, m: Message) -> io::Result<()> {
        match self.ws.write(m) {
            Ok(()) => Ok(()),
            Err(e) if blocked(&e) => Ok(()),
            Err(WsError::WriteBufferFull(_)) => Err(would_block("ws write buffer full")),
            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                Err(io::Error::new(ErrorKind::BrokenPipe, "websocket closed"))
            }
            Err(e) => Err(to_io(e)),
        }
    }

    /// The heartbeat, and the reply to the peer's.
    ///
    /// Called from the mux writer's park cycle, so it runs ~50 times a second
    /// on an idle link and costs two atomic loads when nothing is due.
    ///
    /// Both halves matter and they fail differently: **not sending our `Ping`**
    /// lets an idle tunnel reap us; **not answering theirs** lets it reap us
    /// from the other side, and that half is the one this design could lose
    /// silently, because the reply tungstenite generated was thrown away with
    /// the reader's writes.
    pub(crate) fn tick(&mut self) -> io::Result<()> {
        for p in self.shared.take_pongs() {
            self.enqueue(Message::Pong(p))?;
            self.shared.pongs_written.fetch_add(1, Ordering::Relaxed);
        }
        if Instant::now() >= self.next_ping {
            self.enqueue(Message::Ping(Vec::new()))?;
            self.shared.pings_written.fetch_add(1, Ordering::Relaxed);
            self.next_ping = Instant::now() + self.interval;
        }
        match self.pump() {
            // Nothing to retry: the frames are in the framer and the next tick
            // pushes them. A heartbeat that cannot get out past a saturated
            // link is not a reason to tear the link down.
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            other => other,
        }
    }
}

impl Write for WsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Empty the framer first, so the enqueue below cannot be refused and a
        // `WouldBlock` from here always means "none of this frame was taken".
        // That is exactly the precondition `write_one_frame`'s stale gate
        // depends on, and on this carrier it is stronger than on TCP: there is
        // no such thing as a half-sent message, so a dropped frame can never
        // desynchronise the peer.
        self.pump()?;
        // `to_vec` per frame: tungstenite 0.24's `Message::Binary` owns its
        // payload and has no borrowed form, so this carrier allocates ~1 KiB
        // per media frame where the bare-TCP one allocates nothing. It is on
        // the mux writer thread, not on `tx_loop` — the real-time thread hands
        // frames to a ring and never reaches here (the grep guard in
        // `engine.rs` is what keeps that true) — so this is throughput, not
        // jitter. Recorded because "why does tier 2 allocate?" should have an
        // answer here rather than in a profile.
        self.enqueue(Message::Binary(buf.to_vec()))?;
        self.shared.messages_written.fetch_add(1, Ordering::Relaxed);
        // Best effort: the bytes are ours to finish now, and `pump` at the top
        // of the next call (or `tick`) finishes them.
        let _ = self.pump();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.pump() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            other => other,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_net::framed::{control_header, encode_frame};
    use std::net::{TcpListener, TcpStream};

    // ------------------------------------------------------------ URL parsing

    #[test]
    fn a_peer_url_is_parsed_into_the_three_things_a_dial_needs() {
        let u = WsUrl::parse("ws://tunnel.example.com/audiohub").expect("parse");
        assert!(!u.tls);
        assert_eq!(u.host, "tunnel.example.com");
        assert_eq!(u.port, 80, "ws:// defaults to 80, like every other HTTP client");
        assert_eq!(u.path, "/audiohub");
        assert_eq!(u.authority(), "tunnel.example.com:80");

        let u = WsUrl::parse("ws://127.0.0.1:47899").expect("parse");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("127.0.0.1", 47899, "/"));

        let u = WsUrl::parse("wss://x.example/a/b?c=d").expect("parse");
        assert!(u.tls);
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/a/b?c=d");

        // IPv6 literals: the colon rule inside the brackets is not the colon
        // rule outside them, and getting that wrong turns `::1` into a port.
        let u = WsUrl::parse("ws://[::1]:9000/p").expect("parse");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("[::1]", 9000, "/p"));
        let u = WsUrl::parse("ws://[::1]").expect("parse");
        assert_eq!((u.host.as_str(), u.port), ("[::1]", 80));
    }

    /// A malformed URL must not degrade into a hostname.
    ///
    /// Without [`WsUrl::looks_like_url`] the caller's fallback is a TCP dial,
    /// so `wsx://host/p` would be resolved as a machine literally called
    /// `wsx` — a name-resolution failure standing in for a typo, which is the
    /// least useful error the user could be given.
    #[test]
    fn a_broken_url_is_refused_rather_than_read_as_a_hostname() {
        for s in ["ws://", "ws:///path", "ws://host:notaport", "ws://[::1/p"] {
            assert!(WsUrl::parse(s).is_err(), "{s} parsed and should not have");
        }
        assert!(WsUrl::looks_like_url("WS://Host/p"), "the scheme is case-insensitive");
        assert!(!WsUrl::looks_like_url("192.168.1.5:47810"));
        assert!(!WsUrl::looks_like_url("wsx://host/p"));
    }

    /// `wss://` parses and is refused **by name**, which is the whole point:
    /// the gap is a missing TLS client, not an unrecognised address.
    #[test]
    fn wss_is_understood_and_then_declined_with_the_real_reason() {
        let u = WsUrl::parse("wss://gate.example/p").expect("wss:// must parse");
        let e = format!("{:#}", u.require_plaintext().expect_err("wss:// must be refused"));
        assert!(e.contains("no TLS client"), "the refusal does not name the gap: {e}");
        WsUrl::parse("ws://gate.example/p").unwrap().require_plaintext().expect("ws:// is fine");
    }

    // ------------------------------------------------------------ the carrier

    /// A loopback socket pair, upgraded to WebSocket on both ends.
    ///
    /// The client connects **before** the server thread is joined and there is
    /// no handshake between them: the first version signalled from inside the
    /// accepting thread and waited for that signal before dialling, which is a
    /// deadlock — `accept` cannot return until somebody connects. Cost four
    /// hung tests before it was obvious.
    fn ws_pair() -> ((WsReader, WsWriter), (WsReader, WsWriter)) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().expect("accept");
            accept(s).expect("server upgrade")
        });
        let c = TcpStream::connect(addr).expect("connect");
        let url = WsUrl::parse(&format!("ws://{addr}/audiohub")).expect("url");
        let client = connect(c, &url).expect("client upgrade");
        (client, h.join().expect("server thread"))
    }

    fn read_one(r: &mut WsReader) -> Vec<u8> {
        let mut buf = vec![0u8; MUX_MAX_FRAME];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match r.read(&mut buf) {
                Ok(0) => panic!("the carrier reported EOF"),
                Ok(n) => return buf[..n].to_vec(),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "nothing arrived in five seconds");
                }
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// One `write` call is one binary message, in both directions, and the
    /// bytes come back whole.
    #[test]
    fn a_frame_crosses_the_shell_byte_for_byte_in_both_directions() {
        let ((mut cr, mut cw), (mut sr, mut sw)) = ws_pair();
        let mut frame = Vec::new();
        encode_frame(&control_header(), b"hello tunnel", &mut frame).expect("encode");

        assert_eq!(cw.write(&frame).expect("client write"), frame.len());
        cw.flush().expect("flush");
        assert_eq!(read_one(&mut sr), frame, "the server got different bytes");

        assert_eq!(sw.write(&frame).expect("server write"), frame.len());
        sw.flush().expect("flush");
        assert_eq!(read_one(&mut cr), frame, "the client got different bytes");

        // The 1:1 mapping decision B's payoff depends on. Two frames written,
        // two messages counted — not one coalesced message and not three.
        cw.write(&frame).expect("second");
        cw.flush().expect("flush");
        assert_eq!(read_one(&mut sr), frame);
        assert_eq!(cw.shared().messages_written.load(Ordering::Relaxed), 2);
        assert_eq!(sr.shared().messages_read.load(Ordering::Relaxed), 2);
    }

    /// **The handshake must consume the header and not one byte more.**
    ///
    /// The peer's first frame is usually already in the socket when the upgrade
    /// response goes out — on the accepting side that is the normal case, since
    /// the tier 2 dialler sends `VerifyHello` immediately. A handshake that
    /// over-reads keeps those bytes inside a `WebSocket` that is about to be
    /// dropped, and they are gone.
    ///
    /// Injection control (run 2026-08-09): replace `HeaderOnly` in [`accept`]
    /// with the bare `TcpStream` ⇒ **red immediately**, with tungstenite's own
    /// diagnosis: `an inbound WebSocket upgrade failed: WebSocket protocol
    /// error: Junk after client request`. Every other test in this module still
    /// passes, because none of the others put bytes behind the header in the
    /// same write — which is exactly why this one exists.
    #[test]
    fn the_handshake_leaves_no_bytes_behind() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().expect("accept");
            accept(s).expect("server upgrade")
        });

        // Hand-rolled client so the request and the first frame leave in **one**
        // `write`, which is what puts them in the same segment and makes the
        // over-read reachable. A well-behaved client cannot be relied on to
        // produce this; a tunnel coalescing two writes will.
        let mut c = TcpStream::connect(addr).expect("connect");
        let mut frame = Vec::new();
        encode_frame(&control_header(), b"first frame, no waiting", &mut frame).expect("encode");
        let mut masked = Vec::new();
        // A client frame must be masked (RFC 6455 §5.3); the key here is
        // constant because this is a test, not a security boundary.
        let key = [0x11u8, 0x22, 0x33, 0x44];
        masked.push(0x82); // FIN + binary
        masked.push(0x80 | 126); // masked, 16-bit length
        masked.extend_from_slice(&(frame.len() as u16).to_be_bytes());
        masked.extend_from_slice(&key);
        masked.extend(frame.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));

        let mut out = format!(
            "GET /audiohub HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n\r\n"
        )
        .into_bytes();
        out.extend_from_slice(&masked);
        c.write_all(&out).expect("request and frame in one write");
        c.flush().expect("flush");

        let (mut sr, _sw) = h.join().expect("server thread");
        assert_eq!(read_one(&mut sr), frame, "the first frame did not survive the handshake");
    }

    // -------------------------------------------------------- no compression

    /// Design §4.3: `permessage-deflate` is never offered and never accepted.
    ///
    /// Asserted on the actual request bytes and on the actual response, not on
    /// the library's reputation — tungstenite has no compression code at all,
    /// which makes the property true but does not make it *checked*, and a
    /// gateway rewriting the response is outside the library entirely.
    #[test]
    fn the_upgrade_neither_offers_nor_accepts_permessage_deflate() {
        let req = "ws://127.0.0.1:1/p".into_client_request().expect("request");
        assert!(
            req.headers().get(EXTENSIONS).is_none(),
            "the outgoing upgrade request carries {EXTENSIONS}: {:?}",
            req.headers().get(EXTENSIONS)
        );
        assert_no_deflate("the request", req.headers()).expect("a clean request must pass");

        // ...and the response really is inspected: a header carrying the
        // extension is refused, which is the injection control for the check
        // itself. Without it the assertion above would only prove that today's
        // tungstenite does not add the header.
        let mut h = tungstenite::http::HeaderMap::new();
        h.insert(EXTENSIONS, "permessage-deflate; client_max_window_bits".parse().unwrap());
        let e = format!("{:#}", assert_no_deflate("the response", &h).expect_err("must refuse"));
        assert!(e.contains(DEFLATE), "the refusal does not name the extension: {e}");

        // A different extension is still refused only if it is deflate — the
        // check must not be "any extension at all", or a future negotiation of
        // something harmless would be a mystery failure.
        let mut h = tungstenite::http::HeaderMap::new();
        h.insert(EXTENSIONS, "bbf-usp-protocol".parse().unwrap());
        assert_no_deflate("the response", &h).expect("an unrelated extension is not this rule");
    }

    /// The live version of the check: a real handshake, and the response
    /// tungstenite actually produced.
    #[test]
    fn a_real_upgrade_response_lists_no_extensions() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().expect("accept");
            accept(s).expect("server upgrade")
        });
        let c = TcpStream::connect(addr).expect("connect");
        arm_handshake_timeouts(&c).expect("timeouts");
        let hs = HeaderOnly::new(c.try_clone().expect("clone"));
        let url = WsUrl::parse(&format!("ws://{addr}/p")).expect("url");
        let req = url.request_uri().into_client_request().expect("request");
        let (done, resp) =
            tungstenite::client::client_with_config(req, hs, Some(ws_config())).expect("upgrade");
        assert_eq!(resp.status().as_u16(), 101);
        assert!(
            resp.headers().get_all(EXTENSIONS).iter().next().is_none(),
            "the negotiated extension list is not empty: {:?}",
            resp.headers()
        );
        assert_no_deflate("the live response", resp.headers()).expect("clean");
        drop(done);
        drop(h.join().expect("server thread"));
    }

    /// A server that **does** accept the extension is refused by our client.
    ///
    /// This is the injection control for acceptance 2 made permanent. The two
    /// tests above show the header is absent; this one shows the absence is
    /// *checked* rather than merely observed — without [`assert_no_deflate`] on
    /// the response, the upgrade below succeeds, the peer starts setting RSV1,
    /// and the failure surfaces later as unreadable frames.
    ///
    /// The server is hand-rolled because tungstenite cannot be made to echo the
    /// extension: it has no compression code at all. A gateway can, which is
    /// the case being modelled.
    #[test]
    fn a_server_that_accepts_deflate_is_refused_at_the_handshake() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().expect("accept");
            let mut seen = Vec::new();
            let mut one = [0u8; 1];
            while !seen.ends_with(b"\r\n\r\n") {
                if (&s).read(&mut one).expect("read request") == 0 {
                    return;
                }
                seen.push(one[0]);
            }
            let req = String::from_utf8_lossy(&seen).into_owned();
            let key = req
                .lines()
                .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                .expect("the client must send a key")
                .trim()
                .to_string();
            // tungstenite's own accept-key derivation, so the response is
            // correct in every respect **except** the extension — which makes
            // the refusal below attributable to nothing else.
            let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n{EXTENSIONS}: permessage-deflate\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(200));
        });

        let c = TcpStream::connect(addr).expect("connect");
        let url = WsUrl::parse(&format!("ws://{addr}/p")).expect("url");
        let e = match connect(c, &url) {
            Ok(_) => panic!("a response accepting {DEFLATE} was allowed through"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains(DEFLATE), "the refusal does not name the extension: {e}");
        let _ = h.join();
    }

    // ------------------------------------------------------------- heartbeat

    /// The keepalive, both halves of it.
    ///
    /// A tunnel reaps an idle connection from either end, so both must work:
    /// we have to *send* `Ping` and we have to *answer* theirs. The second half
    /// is the one this design could lose silently — tungstenite generates the
    /// reply on the reading instance, whose writes go to a sink — so it is
    /// asserted from the peer's side, as a `Pong` that actually arrived.
    ///
    /// Injection controls (run 2026-08-09, both red):
    ///   - delete the `take_pongs` loop from [`WsWriter::tick`] ⇒
    ///     "the peer never answered our pings" (`pongs_read` stays 0).
    ///   - delete the `next_ping` branch ⇒ "we never sent a ping"
    ///     (`pings_written` stays 0), and the peer's `pings_read` stays 0 too.
    #[test]
    fn the_heartbeat_pings_and_answers_pings() {
        let ((mut cr, mut cw), (mut sr, mut sw)) = ws_pair();
        cw.set_ping_interval(Duration::from_millis(40));
        sw.set_ping_interval(Duration::from_millis(40));

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cw.tick().expect("client tick");
            sw.tick().expect("server tick");
            // The reader is what surfaces Ping/Pong; nothing counts until it
            // runs, exactly as in the mux where a dedicated thread does it.
            let _ = cr.read(&mut [0u8; 64]);
            let _ = sr.read(&mut [0u8; 64]);
            let c = cr.shared();
            let s = sr.shared();
            if c.pongs_read.load(Ordering::Relaxed) >= 2
                && s.pongs_read.load(Ordering::Relaxed) >= 2
            {
                break;
            }
            assert!(Instant::now() < deadline, "the heartbeat did not complete two round trips");
            std::thread::sleep(Duration::from_millis(5));
        }

        let c = cr.shared();
        assert!(c.pings_written.load(Ordering::Relaxed) >= 2, "we never sent a ping");
        assert!(c.pongs_read.load(Ordering::Relaxed) >= 2, "the peer never answered our pings");
        assert!(c.pings_read.load(Ordering::Relaxed) >= 2, "the peer never pinged us");
        assert!(c.pongs_written.load(Ordering::Relaxed) >= 2, "we never answered the peer's pings");
    }

    /// Protocol traffic must not look like data, and must not look like EOF.
    ///
    /// `mux::read_loop` treats `Ok(0)` as "the peer closed" and tears the link
    /// down. If a `Pong` surfaced as an empty read, every heartbeat would kill
    /// the connection it exists to keep alive.
    #[test]
    fn a_pong_is_not_end_of_stream_and_not_a_frame() {
        let ((mut cr, _cw), (_sr, mut sw)) = ws_pair();
        sw.enqueue(Message::Pong(vec![1, 2, 3])).expect("enqueue");
        sw.flush().expect("flush");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match cr.read(&mut [0u8; 64]) {
                Ok(0) => panic!("a pong was reported as end of stream"),
                Ok(n) => panic!("a pong surfaced as {n} bytes of frame data"),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if cr.shared().pongs_read.load(Ordering::Relaxed) > 0 {
                        return;
                    }
                    assert!(Instant::now() < deadline, "the pong never arrived");
                }
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// A closed peer is EOF, which is how `mux::read_loop` learns to stop.
    #[test]
    fn a_close_message_is_reported_as_end_of_stream() {
        let ((mut cr, _cw), (_sr, mut sw)) = ws_pair();
        sw.enqueue(Message::Close(None)).expect("enqueue");
        sw.flush().expect("flush");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match cr.read(&mut [0u8; 64]) {
                Ok(0) => return,
                Ok(n) => panic!("close surfaced as {n} bytes of frame data"),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the close never arrived")
                }
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// The `\r\n\r\n` scanner, including the case that only appears when a
    /// header straddles two reads: the terminator split across the boundary.
    #[test]
    fn the_header_scanner_stops_on_the_terminator_wherever_it_falls() {
        let (take, m, done) = scan_header_end(0, b"GET / HTTP/1.1\r\n\r\nBODY");
        assert!(done);
        assert_eq!(take, 18, "the scanner consumed into the body");
        assert_eq!(m, 4);

        // Split three ways, with the terminator cut in the middle.
        let (t1, m1, d1) = scan_header_end(0, b"A: b\r\n\r");
        assert!(!d1);
        assert_eq!((t1, m1), (7, 3));
        let (t2, m2, d2) = scan_header_end(m1, b"\nrest");
        assert!(d2);
        assert_eq!((t2, m2), (1, 4), "the split terminator was not recognised");

        // A lone CR restarts the match rather than continuing it.
        let (_, m3, d3) = scan_header_end(0, b"\r\n\r\rx");
        assert!(!d3);
        assert_eq!(m3, 0, "a CR followed by a non-LF must not leave the machine armed");
    }
}
