//! RTSP accept loop and request dispatch.
//!
//! Each TCP connection owns one session state machine that advances through
//! ANNOUNCE → SETUP → RECORD and runs the UDP audio receiver. OPTIONS and the
//! Apple-Challenge signature are handled here; the audio methods delegate to
//! the session.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, warn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::events::{EventSender, LatestRemoteControlSender, LatestVolumeSender};
use crate::rtsp::{read_request_with_policy, ReadPolicy, Request, Response};
use crate::session::{AudioObserver, Session, SessionSlot};
use crate::sink::SessionSinkFactory;
use crate::{crypto, Config};

pub const SERVER_ID: &str = "AirTunes/105.1";
pub const PUBLIC_METHODS: &str =
    "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER";

/// Everything a connection handler needs, shared across the server.
pub struct Context {
    pub config: Config,
    /// Creates each stream's audio sink at SETUP.
    pub sink_factory: SessionSinkFactory,
    /// Session milestones for the host.
    pub events: EventSender,
    /// Optional latest-wins native-volume sideband.
    pub latest_volume: Option<LatestVolumeSender>,
    /// Optional latest-wins DACP session/credential sideband.
    pub latest_remote_control: Option<LatestRemoteControlSender>,
}

pub async fn serve(listener: TcpListener, context: Arc<Context>) -> io::Result<()> {
    serve_with_observer(listener, context, None).await
}

/// Like [`serve`], but every decrypted audio packet is also forwarded to
/// `observer`. Used by integration tests to inspect the crypto path; the
/// production entry point passes `None`.
pub async fn serve_with_observer(
    listener: TcpListener,
    context: Arc<Context>,
    observer: Option<AudioObserver>,
) -> io::Result<()> {
    // Shared across connections so only one client can stream at a time.
    let slot = SessionSlot::new();
    let connections = Arc::new(Semaphore::new(context.config.security.max_connections));
    loop {
        let (stream, peer) = listener.accept().await?;
        let permit = match Arc::clone(&connections).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!("[{peer}] RTSP connection limit reached");
                drop(stream);
                continue;
            }
        };
        let context = context.clone();
        let observer = observer.clone();
        let slot = slot.clone();
        tokio::spawn(async move {
            let _permit = permit;
            debug!("[{peer}] connected");
            if let Err(e) = handle_connection(stream, peer, context, observer, slot).await {
                warn!("[{peer}] connection error: {e}");
            }
            debug!("[{peer}] disconnected");
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    context: Arc<Context>,
    observer: Option<AudioObserver>,
    slot: SessionSlot,
) -> io::Result<()> {
    let local_addr = stream.local_addr()?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut session = Session::new(
        observer,
        context.sink_factory.clone(),
        context.events.clone(),
        context.latest_volume.clone(),
        context.latest_remote_control.clone(),
        peer,
        slot,
    );

    loop {
        let security = context.config.security;
        let connection_idle = if session.is_streaming() {
            security.streaming_idle
        } else if session.is_authorized() {
            security.authorized_idle
        } else {
            security.handshake_idle
        };
        // Waiting for the next request is connection idleness, not a stalled
        // request component. Prime at least one byte under the state-specific
        // deadline; after any byte arrives, the parser still applies its short
        // per-line/header/body progress deadline.
        match tokio::time::timeout(connection_idle, reader.fill_buf()).await {
            Ok(Ok([])) => break,
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RTSP connection idle deadline exceeded",
                ));
            }
        }

        let policy = ReadPolicy::new(session.is_authorized(), security.request_io_idle);
        let request = match tokio::time::timeout(
            connection_idle,
            read_request_with_policy(&mut reader, policy),
        )
        .await
        {
            Ok(result) => match result? {
                Some(request) => request,
                None => break,
            },
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "RTSP connection idle deadline exceeded",
                ));
            }
        };
        log_request(&peer, &request);
        let response = dispatch(&mut session, &request, local_addr, &context.config).await;
        debug!("[{peer}] -> {}", response.status());
        tokio::time::timeout(security.request_io_idle, response.write_to(&mut write_half))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "RTSP response write stalled")
            })??;
    }
    Ok(())
}

fn log_request(peer: &SocketAddr, request: &Request) {
    debug!("[{peer}] {} {}", request.method, request.uri);
    for (name, value) in request.headers.iter() {
        if name.eq_ignore_ascii_case("Authorization") || name.eq_ignore_ascii_case("Active-Remote")
        {
            debug!("[{peer}]   {name}: <redacted>");
        } else {
            debug!("[{peer}]   {name}: {value}");
        }
    }
    if request.body.is_empty() {
        return;
    }
    let content_type = request.headers.get("Content-Type").unwrap_or("");
    let printable = content_type.starts_with("text/")
        || content_type.contains("sdp")
        || content_type.contains("parameters");
    if printable {
        debug!(
            "[{peer}]   body:\n{}",
            String::from_utf8_lossy(&request.body)
        );
    } else {
        debug!(
            "[{peer}]   body: {} bytes of {content_type}",
            request.body.len()
        );
    }
}

async fn dispatch(
    session: &mut Session,
    request: &Request,
    local_addr: SocketAddr,
    config: &Config,
) -> Response {
    // Password / Digest auth gate (the classic AirPlay 1 password), checked
    // before any method runs, mirroring shairport-sync. A protected receiver
    // challenges with 401 + WWW-Authenticate until the client answers with a
    // valid `Authorization: Digest` header; without a configured password
    // every connection is authorized immediately and nothing changes. On a
    // 401 the common headers below (CSeq, Server, Apple-Response) still
    // apply, and the Apple-Challenge is still answered.
    let mut response = match session.authenticate(config.password.as_deref(), request) {
        Some(denied) => denied,
        None => {
            // Every authorized request may refresh either DACP credential.
            // Missing or malformed headers leave the last valid component in
            // place; this happens before dispatch so OPTIONS and extensions
            // receive the same treatment as the core RAOP methods.
            session.observe_remote_control(request);
            match request.method.as_str() {
                "OPTIONS" => Response::ok().header("Public", PUBLIC_METHODS),
                "ANNOUNCE" => session.handle_announce(request),
                "SETUP" => session.handle_setup(request, local_addr.ip()).await,
                "RECORD" => session.handle_record(request),
                _ => session.handle_other(request).unwrap_or_else(|| {
                    warn!("method {} not implemented", request.method);
                    Response::new(501, "Not Implemented")
                }),
            }
        }
    };

    if let Some(cseq) = request.headers.get("CSeq") {
        response = response.header("CSeq", cseq);
    }
    response = response
        .header("Server", SERVER_ID)
        .header("Audio-Jack-Status", "connected; type=analog");

    // Any request may carry a challenge; the client drops the connection if
    // the response is missing or wrong.
    if let Some(challenge) = request.headers.get("Apple-Challenge") {
        match crypto::apple_response(challenge, local_addr.ip(), &config.mac) {
            Ok(value) => response = response.header("Apple-Response", value),
            Err(e) => warn!("cannot answer Apple-Challenge: {e}"),
        }
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioSink, SecurityLimits};
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    struct DiscardSink;

    impl AudioSink for DiscardSink {
        fn write(&mut self, _pcm: &[i16]) {}
        fn flush(&mut self) {}
    }

    #[tokio::test]
    async fn raw_rtsp_connection_limit_rejects_excess_clients() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, _event_rx) = tokio::sync::mpsc::channel(8);
        let context = Arc::new(Context {
            config: Config {
                name: "Limit Test".to_string(),
                port: address.port(),
                mac: [0x02, 0, 0, 0, 0, 1],
                password: None,
                security: SecurityLimits {
                    max_connections: 1,
                    handshake_idle: Duration::from_secs(2),
                    authorized_idle: Duration::from_secs(2),
                    streaming_idle: Duration::from_secs(2),
                    request_io_idle: Duration::from_secs(2),
                },
            },
            sink_factory: Arc::new(|_, _, _| Box::new(DiscardSink)),
            events,
            latest_volume: None,
            latest_remote_control: None,
        });
        let task = tokio::spawn(serve(listener, context));

        let mut held = TcpStream::connect(address).await.unwrap();
        held.write_all(b"OPT").await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut excess = TcpStream::connect(address).await.unwrap();
        let write = excess
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await;
        let mut byte = [0u8; 1];
        let closed = match write {
            Err(_) => true,
            Ok(()) => matches!(
                tokio::time::timeout(Duration::from_millis(250), excess.read(&mut byte)).await,
                Ok(Ok(0)) | Ok(Err(_))
            ),
        };
        assert!(closed, "excess RTSP connection remained active");

        drop(held);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut replacement = TcpStream::connect(address).await.unwrap();
        replacement
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0u8; 256];
        let count = tokio::time::timeout(Duration::from_secs(1), replacement.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&response[..count]).starts_with("RTSP/1.0 200"));

        task.abort();
        let _ = task.await;
    }

    async fn read_response_headers(reader: &mut BufReader<TcpStream>) -> String {
        let mut response = String::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .expect("response timeout")
                .expect("response read failed");
            assert_ne!(read, 0, "EOF inside RTSP response");
            response.push_str(&line);
            if line == "\r\n" {
                return response;
            }
        }
    }

    #[tokio::test]
    async fn open_receiver_accepts_apple_music_artwork_after_handshake() {
        const APPLE_MUSIC_ARTWORK_BYTES: usize = 2_393_210;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, _event_rx) = tokio::sync::mpsc::channel(8);
        let context = Arc::new(Context {
            config: Config {
                name: "Artwork Test".to_string(),
                port: address.port(),
                mac: [0x02, 0, 0, 0, 0, 2],
                password: None,
                security: SecurityLimits {
                    max_connections: 1,
                    handshake_idle: Duration::from_secs(5),
                    authorized_idle: Duration::from_secs(5),
                    streaming_idle: Duration::from_secs(5),
                    request_io_idle: Duration::from_secs(5),
                },
            },
            sink_factory: Arc::new(|_, _, _| Box::new(DiscardSink)),
            events,
            latest_volume: None,
            latest_remote_control: None,
        });
        let task = tokio::spawn(serve(listener, context));
        let mut client = BufReader::new(TcpStream::connect(address).await.unwrap());

        client
            .get_mut()
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        let handshake = read_response_headers(&mut client).await;
        assert!(handshake.starts_with("RTSP/1.0 200"));
        assert!(handshake.contains("CSeq: 1\r\n"));

        let header = format!(
            "SET_PARAMETER * RTSP/1.0\r\nCSeq: 2\r\nContent-Type: image/png\r\nContent-Length: {APPLE_MUSIC_ARTWORK_BYTES}\r\n\r\n"
        );
        client.get_mut().write_all(header.as_bytes()).await.unwrap();
        client
            .get_mut()
            .write_all(&vec![0x5a; APPLE_MUSIC_ARTWORK_BYTES])
            .await
            .unwrap();
        let artwork = read_response_headers(&mut client).await;
        assert!(artwork.starts_with("RTSP/1.0 200"));
        assert!(artwork.contains("CSeq: 2\r\n"));

        client
            .get_mut()
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 3\r\n\r\n")
            .await
            .unwrap();
        let following = read_response_headers(&mut client).await;
        assert!(following.starts_with("RTSP/1.0 200"));
        assert!(following.contains("CSeq: 3\r\n"));

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn quiet_authorized_connection_uses_connection_not_component_idle() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, _event_rx) = tokio::sync::mpsc::channel(8);
        let context = Arc::new(Context {
            config: Config {
                name: "Idle Test".to_string(),
                port: address.port(),
                mac: [0x02, 0, 0, 0, 0, 3],
                password: None,
                security: SecurityLimits {
                    max_connections: 1,
                    handshake_idle: Duration::from_millis(250),
                    authorized_idle: Duration::from_millis(250),
                    streaming_idle: Duration::from_millis(250),
                    request_io_idle: Duration::from_millis(20),
                },
            },
            sink_factory: Arc::new(|_, _, _| Box::new(DiscardSink)),
            events,
            latest_volume: None,
            latest_remote_control: None,
        });
        let task = tokio::spawn(serve(listener, context));
        let mut client = BufReader::new(TcpStream::connect(address).await.unwrap());

        client
            .get_mut()
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response_headers(&mut client)
            .await
            .starts_with("RTSP/1.0 200"));

        tokio::time::sleep(Duration::from_millis(75)).await;
        client
            .get_mut()
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n")
            .await
            .unwrap();
        let following = read_response_headers(&mut client).await;
        assert!(following.starts_with("RTSP/1.0 200"));
        assert!(following.contains("CSeq: 2\r\n"));

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn every_authorized_rtsp_method_can_refresh_dacp_credentials() {
        const SDP: &str = "v=0\r\n\
            o=iTunes 3413821438 0 IN IP4 127.0.0.1\r\n\
            s=iTunes\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 0 RTP/AVP 96\r\n\
            a=rtpmap:96 AppleLossless\r\n\
            a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, mut event_rx) = tokio::sync::mpsc::channel(1);
        events
            .try_send(crate::Event::Flushed { stream_id: 999 })
            .unwrap();
        let (remote_tx, mut remote_rx) = tokio::sync::watch::channel(None);
        let context = Arc::new(Context {
            config: Config {
                name: "DACP Refresh Test".to_string(),
                port: address.port(),
                mac: [0x02, 0, 0, 0, 0, 4],
                password: None,
                security: SecurityLimits {
                    max_connections: 1,
                    handshake_idle: Duration::from_secs(2),
                    authorized_idle: Duration::from_secs(2),
                    streaming_idle: Duration::from_secs(2),
                    request_io_idle: Duration::from_secs(2),
                },
            },
            sink_factory: Arc::new(|_, _, _| Box::new(DiscardSink)),
            events,
            latest_volume: None,
            latest_remote_control: Some(remote_tx),
        });
        let task = tokio::spawn(serve(listener, context));
        let mut client = BufReader::new(TcpStream::connect(address).await.unwrap());

        let announce = format!(
            "ANNOUNCE rtsp://127.0.0.1/1 RTSP/1.0\r\n\
             CSeq: 1\r\nDACP-ID: A1\r\nActive-Remote: 111\r\n\
             Content-Length: {}\r\n\r\n{SDP}",
            SDP.len()
        );
        client
            .get_mut()
            .write_all(announce.as_bytes())
            .await
            .unwrap();
        assert!(read_response_headers(&mut client)
            .await
            .starts_with("RTSP/1.0 200"));
        client
            .get_mut()
            .write_all(
                b"SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\n\
                  CSeq: 2\r\n\
                  Transport: RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002\r\n\r\n",
            )
            .await
            .unwrap();
        assert!(read_response_headers(&mut client)
            .await
            .starts_with("RTSP/1.0 200"));
        remote_rx.changed().await.unwrap();
        let first = remote_rx.borrow_and_update().clone().unwrap();
        let crate::RemoteControlState::Active(first) = first else {
            panic!("SETUP published an unexpected end")
        };
        assert_eq!(
            event_rx.try_recv(),
            Ok(crate::Event::Flushed { stream_id: 999 })
        );
        assert!(event_rx.try_recv().is_err(), "ordinary queue was saturated");

        // OPTIONS has no method-specific session handler. Its refresh proves
        // observation happens at the common post-authentication dispatch gate.
        client
            .get_mut()
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 3\r\nActive-Remote: 222\r\n\r\n")
            .await
            .unwrap();
        assert!(read_response_headers(&mut client)
            .await
            .starts_with("RTSP/1.0 200"));
        remote_rx.changed().await.unwrap();
        let refreshed = remote_rx.borrow_and_update().clone().unwrap();
        let crate::RemoteControlState::Active(refreshed) = refreshed else {
            panic!("OPTIONS refresh published an unexpected end")
        };
        assert_eq!(refreshed.stream_id, first.stream_id);
        assert_eq!(refreshed.peer, first.peer);
        let remote = refreshed.remote_control.unwrap();
        assert_eq!(remote.dacp_id, "A1");
        assert_eq!(remote.active_remote, "222");

        task.abort();
        let _ = task.await;
    }
}
