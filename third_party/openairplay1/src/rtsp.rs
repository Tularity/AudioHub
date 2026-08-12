//! Minimal RTSP/1.0 message parsing and serialization, covering the Apple
//! dialect used by AirPlay 1 senders. Requests arrive on a long-lived TCP
//! connection, one after another; responses echo the request's CSeq.

use std::fmt;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const MAX_REQUEST_LINE: usize = 2 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_PREAUTH_BODY: usize = 32 * 1024;
const MAX_ANNOUNCE_BODY: usize = 32 * 1024;
const MAX_TEXT_BODY: usize = 16 * 1024;
// Keep metadata and artwork independently bounded: a real Windows Apple Music
// sender produced a 308,060-byte DMAP update followed by a 2,393,210-byte PNG.
// Artwork is accepted only after authentication and only for SET_PARAMETER.
const MAX_DMAP_BODY: usize = 2 * 1024 * 1024;
const MAX_ARTWORK_BODY: usize = 4 * 1024 * 1024;
const MAX_OTHER_BODY: usize = 64 * 1024;
#[cfg(test)]
const DEFAULT_IO_IDLE: Duration = Duration::from_secs(10);

/// Parser policy selected from the connection's authenticated state.
#[derive(Debug, Clone, Copy)]
pub struct ReadPolicy {
    authenticated: bool,
    io_idle: Duration,
}

impl ReadPolicy {
    pub fn new(authenticated: bool, io_idle: Duration) -> Self {
        Self {
            authenticated,
            io_idle,
        }
    }
}

pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[derive(Default)]
pub struct Headers(Vec<(String, String)>);

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut headers = f.debug_list();
        for (name, value) in &self.0 {
            let value = if is_sensitive_header(name) {
                "<redacted>"
            } else {
                value.as_str()
            };
            headers.entry(&(name.as_str(), value));
        }
        headers.finish()
    }
}

fn is_sensitive_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("Authorization") || name.eq_ignore_ascii_case("Active-Remote")
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    fn push(&mut self, name: String, value: String) {
        self.0.push((name, value));
    }

    fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|(key, _)| key.eq_ignore_ascii_case(name))
    }
}

/// Read one RTSP request. Returns `Ok(None)` on a clean EOF at a message
/// boundary (client closed the connection).
#[cfg(test)]
pub async fn read_request<R>(reader: &mut BufReader<R>) -> io::Result<Option<Request>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    read_request_with_policy(reader, ReadPolicy::new(false, DEFAULT_IO_IDLE)).await
}

/// Read one request with authentication-aware body and I/O idle bounds.
pub async fn read_request_with_policy<R>(
    reader: &mut BufReader<R>,
    policy: ReadPolicy,
) -> io::Result<Option<Request>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let request_line = match read_line(reader, MAX_REQUEST_LINE, policy.io_idle).await? {
        None => return Ok(None),
        Some(line) if line.is_empty() => return Ok(None),
        Some(line) => line,
    };

    let mut parts = request_line.splitn(3, ' ');
    let (method, uri, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(u), Some(v)) if !m.is_empty() => (m, u, v),
        _ => return Err(bad_data("malformed RTSP request line".to_string())),
    };
    if version != "RTSP/1.0" {
        return Err(bad_data("unsupported RTSP request version".to_string()));
    }
    if !method
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        || uri.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(bad_data("invalid RTSP method or URI".to_string()));
    }

    let mut headers = Headers::default();
    let mut header_bytes = 0usize;
    loop {
        let line = read_line(reader, MAX_HEADER_LINE, policy.io_idle)
            .await?
            .ok_or_else(|| bad_data("EOF inside headers".to_string()))?;
        if line.is_empty() {
            break;
        }
        header_bytes = header_bytes
            .checked_add(line.len())
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or_else(|| bad_data("header bytes overflow".to_string()))?;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(bad_data("headers are too large".to_string()));
        }
        if headers.0.len() >= MAX_HEADERS {
            return Err(bad_data("too many headers".to_string()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad_data("malformed RTSP header line".to_string()))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value
                .bytes()
                .any(|byte| byte == 0 || (byte.is_ascii_control() && byte != b'\t'))
        {
            return Err(bad_data("invalid RTSP header".to_string()));
        }
        if name.eq_ignore_ascii_case("Content-Length") && headers.contains("Content-Length") {
            return Err(bad_data("duplicate Content-Length".to_string()));
        }
        if name.eq_ignore_ascii_case("Transfer-Encoding") {
            return Err(bad_data("Transfer-Encoding is not supported".to_string()));
        }
        headers.push(name.to_string(), value.to_string());
    }

    let mut body = Vec::new();
    if let Some(len) = headers.get("Content-Length") {
        if len.is_empty() || !len.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(bad_data("invalid Content-Length".to_string()));
        }
        let len: usize = len
            .parse()
            .map_err(|_| bad_data("invalid Content-Length".to_string()))?;
        let limit = body_limit(method, &headers, policy.authenticated);
        if len > limit {
            return Err(bad_data(format!(
                "RTSP body of {len} bytes is too large: exceeds the {limit}-byte limit \
                 (authenticated={})",
                policy.authenticated
            )));
        }
        body.resize(len, 0);
        let mut offset = 0;
        while offset < len {
            let count = tokio::time::timeout(policy.io_idle, reader.read(&mut body[offset..]))
                .await
                .map_err(|_| timed_out("RTSP body stalled"))??;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF inside RTSP body",
                ));
            }
            offset += count;
        }
    }

    Ok(Some(Request {
        method: method.to_string(),
        uri: uri.to_string(),
        headers,
        body,
    }))
}

fn body_limit(method: &str, headers: &Headers, authenticated: bool) -> usize {
    if !authenticated {
        return MAX_PREAUTH_BODY;
    }
    if method == "ANNOUNCE" {
        return MAX_ANNOUNCE_BODY;
    }
    if method != "SET_PARAMETER" {
        return MAX_OTHER_BODY;
    }
    let content_type = headers
        .get("Content-Type")
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim();
    if content_type.eq_ignore_ascii_case("image/png")
        || content_type.eq_ignore_ascii_case("image/jpeg")
    {
        MAX_ARTWORK_BODY
    } else if content_type.eq_ignore_ascii_case("application/x-dmap-tagged") {
        MAX_DMAP_BODY
    } else if content_type.eq_ignore_ascii_case("text/parameters") {
        MAX_TEXT_BODY
    } else {
        MAX_OTHER_BODY
    }
}

async fn read_line<R>(
    reader: &mut BufReader<R>,
    max_bytes: usize,
    io_idle: Duration,
) -> io::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = tokio::time::timeout(io_idle, reader.fill_buf())
            .await
            .map_err(|_| timed_out("RTSP line stalled"))??;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF inside RTSP line",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > max_bytes {
            return Err(bad_data(format!("RTSP line exceeds {max_bytes} bytes")));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    if line
        .iter()
        .any(|byte| *byte == 0 || *byte == b'\r' || *byte == b'\n')
    {
        return Err(bad_data("invalid control byte in RTSP line".to_string()));
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| bad_data("RTSP line is not UTF-8".to_string()))
}

fn bad_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn timed_out(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, msg)
}

#[derive(Debug)]
pub struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: &'static str) -> Self {
        Response {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn ok() -> Self {
        Response::new(200, "OK")
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        if !self.body.is_empty() {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        writer.write_all(&out).await?;
        writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn parse(input: &[u8]) -> io::Result<Option<Request>> {
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        read_request(&mut reader).await
    }

    async fn parse_as(input: &[u8], authenticated: bool) -> io::Result<Option<Request>> {
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        read_request_with_policy(
            &mut reader,
            ReadPolicy::new(authenticated, Duration::from_secs(1)),
        )
        .await
    }

    #[tokio::test]
    async fn parses_request_with_headers_and_body() {
        let req = parse(
            b"ANNOUNCE rtsp://192.168.1.2/1234 RTSP/1.0\r\n\
              CSeq: 2\r\n\
              Content-Type: application/sdp\r\n\
              Content-Length: 5\r\n\
              \r\n\
              hello",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(req.method, "ANNOUNCE");
        assert_eq!(req.uri, "rtsp://192.168.1.2/1234");
        assert_eq!(req.headers.get("cseq"), Some("2"));
        assert_eq!(req.headers.get("CONTENT-TYPE"), Some("application/sdp"));
        assert_eq!(req.body, b"hello");
    }

    #[tokio::test]
    async fn parses_two_requests_on_one_connection() {
        let input = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nOPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        let first = read_request(&mut reader).await.unwrap().unwrap();
        let second = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(first.headers.get("CSeq"), Some("1"));
        assert_eq!(second.headers.get("CSeq"), Some("2"));
        assert!(read_request(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_returns_none() {
        assert!(parse(b"").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_non_rtsp() {
        assert!(parse(b"GET / HTTP/1.1\r\n\r\n").await.is_err());
        assert!(parse(b"garbage\r\n\r\n").await.is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_request_and_header_lines_without_unbounded_reads() {
        let mut request_line = vec![b'A'; MAX_REQUEST_LINE + 1];
        request_line.extend_from_slice(b"\r\n");
        let error = parse(&request_line).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line exceeds"));

        let mut header = b"OPTIONS * RTSP/1.0\r\nX-Test: ".to_vec();
        header.extend(std::iter::repeat_n(b'a', MAX_HEADER_LINE));
        header.extend_from_slice(b"\r\n\r\n");
        let error = parse(&header).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line exceeds"));
    }

    #[tokio::test]
    async fn rejects_header_count_total_and_ambiguous_framing() {
        let mut count = b"OPTIONS * RTSP/1.0\r\n".to_vec();
        for index in 0..=MAX_HEADERS {
            count.extend_from_slice(format!("X-{index}: value\r\n").as_bytes());
        }
        count.extend_from_slice(b"\r\n");
        assert!(parse(&count)
            .await
            .unwrap_err()
            .to_string()
            .contains("too many"));

        let mut total = b"OPTIONS * RTSP/1.0\r\n".to_vec();
        for index in 0..5 {
            total.extend_from_slice(format!("X-{index}: ").as_bytes());
            total.extend(std::iter::repeat_n(b'a', 7_000));
            total.extend_from_slice(b"\r\n");
        }
        total.extend_from_slice(b"\r\n");
        assert!(parse(&total)
            .await
            .unwrap_err()
            .to_string()
            .contains("headers are too large"));

        assert!(
            parse(b"OPTIONS * RTSP/1.0\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap_err()
                .to_string()
                .contains("duplicate Content-Length")
        );
        assert!(
            parse(b"OPTIONS * RTSP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap_err()
                .to_string()
                .contains("Transfer-Encoding")
        );
    }

    #[tokio::test]
    async fn malformed_sensitive_headers_are_redacted_from_loggable_errors() {
        for (name, secret) in [
            ("Active-Remote", "1986535575"),
            (
                "Authorization",
                "Digest username=audiohub, response=0123456789abcdef",
            ),
        ] {
            let request = format!("OPTIONS * RTSP/1.0\r\n{name} {secret}\r\n\r\n");
            let error = parse(request.as_bytes()).await.unwrap_err();
            let display = error.to_string();
            let debug = format!("{error:?}");

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(display, "malformed RTSP header line");
            for rendered in [&display, &debug] {
                assert!(!rendered.contains(name), "header name leaked: {rendered}");
                assert!(
                    !rendered.contains(secret),
                    "header value leaked: {rendered}"
                );
            }
        }
    }

    #[tokio::test]
    async fn malformed_request_lines_are_redacted_from_loggable_errors() {
        for (line, secret) in [
            ("Active-Remote 1986535575", "1986535575"),
            (
                "Authorization: Digest response=0123456789abcdef",
                "0123456789abcdef",
            ),
        ] {
            let request = format!("{line}\r\n\r\n");
            let error = parse(request.as_bytes()).await.unwrap_err();
            let display = error.to_string();
            let debug = format!("{error:?}");

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            for rendered in [&display, &debug] {
                assert!(!rendered.contains(line), "request line leaked: {rendered}");
                assert!(!rendered.contains(secret), "secret leaked: {rendered}");
            }
        }
    }

    #[tokio::test]
    async fn framing_errors_do_not_echo_untrusted_header_values() {
        let cases = [
            (
                "OPTIONS * RTSP/1.0\r\nContent-Length: bearer-secret-length\r\n\r\n".to_string(),
                "bearer-secret-length",
            ),
            (
                format!(
                    "SET_PARAMETER * RTSP/1.0\r\nContent-Type: bearer-secret-type\r\n\
                     Content-Length: {}\r\n\r\n",
                    MAX_PREAUTH_BODY + 1
                ),
                "bearer-secret-type",
            ),
        ];

        for (request, secret) in cases {
            let error = parse(request.as_bytes()).await.unwrap_err();
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(
                    !rendered.contains(secret),
                    "header value leaked: {rendered}"
                );
            }
        }
    }

    #[tokio::test]
    async fn request_and_headers_debug_redact_authorization_and_active_remote() {
        let authorization = "Digest username=audiohub, response=0123456789abcdef0123456789abcdef";
        let active_remote = "1986535575";
        let raw = format!(
            "OPTIONS * RTSP/1.0\r\nAuthorization: {authorization}\r\n\
             Active-Remote: {active_remote}\r\nCSeq: 9\r\n\r\n"
        );
        let request = parse(raw.as_bytes()).await.unwrap().unwrap();

        for debug in [format!("{:?}", request.headers), format!("{request:?}")] {
            assert!(debug.contains("<redacted>"));
            assert!(debug.contains("CSeq"));
            assert!(!debug.contains(authorization));
            assert!(!debug.contains(active_remote));
        }
    }

    #[tokio::test]
    async fn authentication_state_selects_the_body_cap() {
        let header = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            MAX_PREAUTH_BODY + 1
        );
        assert!(parse_as(header.as_bytes(), false)
            .await
            .unwrap_err()
            .to_string()
            .contains("too large"));

        let body = vec![0x5a; MAX_PREAUTH_BODY + 1];
        let mut request = header.into_bytes();
        request.extend_from_slice(&body);
        let parsed = parse_as(&request, true).await.unwrap().unwrap();
        assert_eq!(parsed.body, body);

        let too_large = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
            MAX_ARTWORK_BODY + 1
        );
        assert!(parse_as(too_large.as_bytes(), true).await.is_err());
    }

    #[tokio::test]
    async fn authenticated_dmap_accepts_apple_music_metadata_and_preserves_framing() {
        const APPLE_MUSIC_METADATA_BYTES: usize = 308_060;

        let body = vec![0x5a; APPLE_MUSIC_METADATA_BYTES];
        let mut stream = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: application/x-dmap-tagged\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        stream.extend_from_slice(&body);
        stream.extend_from_slice(b"OPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n");

        let mut reader = BufReader::new(Cursor::new(stream));
        let metadata =
            read_request_with_policy(&mut reader, ReadPolicy::new(true, Duration::from_secs(1)))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(metadata.body, body);

        let following =
            read_request_with_policy(&mut reader, ReadPolicy::new(true, Duration::from_secs(1)))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(following.method, "OPTIONS");
        assert_eq!(following.headers.get("CSeq"), Some("2"));

        let oversized = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: application/x-dmap-tagged\r\nContent-Length: {}\r\n\r\n",
            MAX_DMAP_BODY + 1
        );
        let error = parse_as(oversized.as_bytes(), true).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn authenticated_artwork_accepts_apple_music_png_and_preserves_framing() {
        const APPLE_MUSIC_ARTWORK_BYTES: usize = 2_393_210;

        let body = vec![0x5a; APPLE_MUSIC_ARTWORK_BYTES];
        let mut stream = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        stream.extend_from_slice(&body);
        stream.extend_from_slice(b"OPTIONS * RTSP/1.0\r\nCSeq: 3\r\n\r\n");

        let mut reader = BufReader::new(Cursor::new(stream));
        let artwork =
            read_request_with_policy(&mut reader, ReadPolicy::new(true, Duration::from_secs(1)))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(artwork.body, body);

        let following =
            read_request_with_policy(&mut reader, ReadPolicy::new(true, Duration::from_secs(1)))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(following.method, "OPTIONS");
        assert_eq!(following.headers.get("CSeq"), Some("3"));

        let oversized = format!(
            "SET_PARAMETER * RTSP/1.0\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            MAX_ARTWORK_BODY + 1
        );
        let error = parse_as(oversized.as_bytes(), true).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn artwork_cap_is_narrowly_scoped_to_known_authenticated_set_parameter_images() {
        fn limit(method: &str, content_type: &str, authenticated: bool) -> usize {
            let mut headers = Headers::default();
            headers.push("Content-Type".into(), content_type.into());
            body_limit(method, &headers, authenticated)
        }

        assert_eq!(
            limit("SET_PARAMETER", "IMAGE/PNG; charset=binary", true),
            MAX_ARTWORK_BODY
        );
        assert_eq!(limit("SET_PARAMETER", "image/jpeg", true), MAX_ARTWORK_BODY);
        assert_eq!(
            limit("SET_PARAMETER", "image/svg+xml", true),
            MAX_OTHER_BODY
        );
        assert_eq!(limit("SET_PARAMETER", "image/none", true), MAX_OTHER_BODY);
        assert_eq!(limit("POST", "image/png", true), MAX_OTHER_BODY);
        assert_eq!(limit("SET_PARAMETER", "image/png", false), MAX_PREAUTH_BODY);
    }

    #[tokio::test]
    async fn partial_raw_request_hits_the_component_idle_deadline() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"OPTIONS").await.unwrap();
        let mut reader = BufReader::new(server);
        let error = read_request_with_policy(
            &mut reader,
            ReadPolicy::new(false, Duration::from_millis(20)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn serializes_response() {
        let mut out = Vec::new();
        Response::ok()
            .header("CSeq", "7")
            .header("Public", "OPTIONS")
            .write_to(&mut out)
            .await
            .unwrap();
        assert_eq!(
            out,
            b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nPublic: OPTIONS\r\n\r\n"
        );
    }
}
