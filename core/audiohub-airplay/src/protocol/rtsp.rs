//! Strict, bounded HTTP/RTSP request framing for the AirPlay control socket.

use std::error::Error;
use std::fmt;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};
use zeroize::Zeroize;

pub const DEFAULT_MAX_REQUEST_LINE_BYTES: usize = 2 * 1024;
pub const DEFAULT_MAX_HEADER_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    pub max_request_line_bytes: usize,
    pub max_header_bytes: usize,
    pub max_header_count: usize,
    pub default_max_body_bytes: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_request_line_bytes: DEFAULT_MAX_REQUEST_LINE_BYTES,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_header_count: 128,
            default_max_body_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    name: String,
    value: Vec<u8>,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Result<Self, RtspError> {
        let name = name.into();
        let value = value.into();
        validate_header_name(name.as_bytes())?;
        validate_header_value(&value)?;
        Ok(Self { name, value })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("Header");
        out.field("name", &self.name);
        if is_sensitive_header(&self.name) {
            out.field("value", &"<redacted>");
        } else {
            out.field("value", &String::from_utf8_lossy(&self.value));
        }
        out.finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    method: String,
    target: String,
    version: String,
    headers: Vec<Header>,
    body: Vec<u8>,
}

impl Request {
    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(Header::value)
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Erase pairing and stream-key material as soon as dispatch has consumed
    /// it. The allocation may remain for reuse/drop, but its previous bytes do
    /// not remain readable in process memory through this request object.
    pub(crate) fn wipe_body(&mut self) {
        self.body.zeroize();
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("version", &self.version)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Read exactly one request. The callback selects a body cap after the request
/// line and headers are known, allowing authenticated metadata/artwork
/// endpoints and small pairing messages to use different resource budgets.
pub async fn read_request<R, F>(
    reader: &mut R,
    limits: RequestLimits,
    max_body_for_request: F,
) -> Result<Option<Request>, RtspError>
where
    R: AsyncRead + Unpin,
    F: FnOnce(&str, &str, &[Header]) -> Option<usize>,
{
    validate_limits(limits)?;
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    let header_end = loop {
        let read = reader.read(&mut byte).await.map_err(RtspError::Io)?;
        if read == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(RtspError::UnexpectedEofInHeaders);
        }
        head.push(byte[0]);

        let request_line_end = find_crlf(&head);
        if request_line_end.is_none() && head.len() > limits.max_request_line_bytes {
            return Err(RtspError::RequestLineTooLarge {
                max: limits.max_request_line_bytes,
            });
        }
        if head.len() > limits.max_header_bytes {
            return Err(RtspError::HeadersTooLarge {
                max: limits.max_header_bytes,
            });
        }
        if head.ends_with(b"\r\n\r\n") {
            break head.len();
        }
        // Bare LF is intentionally refused: accepting both delimiters creates
        // request-smuggling disagreement with strict downstream components.
        if byte[0] == b'\n' && (head.len() < 2 || head[head.len() - 2] != b'\r') {
            return Err(RtspError::MalformedLineEnding);
        }
    };

    let (method, target, version, headers, content_length) =
        parse_head(&head[..header_end], limits)?;
    let body_cap =
        max_body_for_request(&method, &target, &headers).unwrap_or(limits.default_max_body_bytes);
    if content_length > body_cap {
        return Err(RtspError::BodyTooLarge {
            actual: content_length,
            max: body_cap,
        });
    }

    let mut body = vec![0; content_length];
    if let Err(error) = reader.read_exact(&mut body).await {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Err(RtspError::UnexpectedEofInBody {
                expected: content_length,
            });
        }
        return Err(RtspError::Io(error));
    }

    Ok(Some(Request {
        method,
        target,
        version,
        headers,
        body,
    }))
}

/// Persistent request framing for sockets whose reads may contain arbitrary
/// plaintext boundaries (notably decrypted HomeKit control frames).
///
/// Unlike [`read_request`], this decoder may accept a whole TCP chunk at once:
/// it consumes exactly one request from the front and retains both incomplete
/// prefixes and pipelined requests. Callers should set
/// `default_max_body_bytes` to the largest endpoint body they will ever accept
/// and use the request-aware callback to select stricter per-endpoint limits.
#[derive(Debug)]
pub struct RequestDecoder {
    limits: RequestLimits,
    buffered: Vec<u8>,
    poisoned: bool,
}

impl RequestDecoder {
    pub fn new(limits: RequestLimits) -> Result<Self, RtspError> {
        validate_limits(limits)?;
        limits
            .max_header_bytes
            .checked_add(limits.default_max_body_bytes)
            .ok_or(RtspError::InvalidLimits)?;
        Ok(Self {
            limits,
            buffered: Vec::with_capacity(4096),
            poisoned: false,
        })
    }

    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }

    /// Remove an unparsed remainder when the transport changes framing after
    /// a completed request (Pair-Setup M4 switches the same TCP socket from
    /// plaintext to encrypted HomeKit frames). The decoder remains reusable.
    pub fn take_buffered(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffered)
    }

    /// Change the aggregate body budget after a transport/authentication
    /// boundary. This lets callers keep unauthenticated plaintext at the
    /// small control limit and permit larger metadata only after pairing.
    pub fn set_default_max_body_bytes(&mut self, maximum: usize) -> Result<(), RtspError> {
        if self.poisoned {
            return Err(RtspError::PoisonedDecoder);
        }
        let limits = RequestLimits {
            default_max_body_bytes: maximum,
            ..self.limits
        };
        validate_limits(limits)?;
        let max = limits
            .max_header_bytes
            .checked_add(maximum)
            .ok_or(RtspError::InvalidLimits)?;
        if self.buffered.len() > max {
            self.poisoned = true;
            return Err(RtspError::BufferedDataTooLarge {
                actual: self.buffered.len(),
                max,
            });
        }
        self.limits = limits;
        Ok(())
    }

    /// Append one arbitrary plaintext chunk. The aggregate allocation remains
    /// bounded even if a peer sends a body before its header has been parsed.
    pub fn feed(&mut self, input: &[u8]) -> Result<(), RtspError> {
        if self.poisoned {
            return Err(RtspError::PoisonedDecoder);
        }
        let max = self
            .limits
            .max_header_bytes
            .checked_add(self.limits.default_max_body_bytes)
            .ok_or(RtspError::InvalidLimits)?;
        let actual = self.buffered.len().checked_add(input.len()).ok_or(
            RtspError::BufferedDataTooLarge {
                actual: usize::MAX,
                max,
            },
        )?;
        if actual > max {
            self.poisoned = true;
            return Err(RtspError::BufferedDataTooLarge { actual, max });
        }
        self.buffered.extend_from_slice(input);
        Ok(())
    }

    /// Return the next complete request, retaining an incomplete prefix or any
    /// pipelined remainder. A framing error poisons this connection decoder;
    /// continuing after an ambiguous parse would risk request smuggling.
    pub fn next_request<F>(&mut self, max_body_for_request: F) -> Result<Option<Request>, RtspError>
    where
        F: FnOnce(&str, &str, &[Header]) -> Option<usize>,
    {
        if self.poisoned {
            return Err(RtspError::PoisonedDecoder);
        }
        match parse_buffered_request(&self.buffered, self.limits, max_body_for_request) {
            Ok(Some((request, consumed))) => {
                self.buffered.drain(..consumed);
                Ok(Some(request))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }
}

fn parse_buffered_request<F>(
    buffered: &[u8],
    limits: RequestLimits,
    max_body_for_request: F,
) -> Result<Option<(Request, usize)>, RtspError>
where
    F: FnOnce(&str, &str, &[Header]) -> Option<usize>,
{
    let Some(header_start_end) = buffered.windows(4).position(|window| window == b"\r\n\r\n")
    else {
        if buffered
            .iter()
            .enumerate()
            .any(|(index, byte)| *byte == b'\n' && (index == 0 || buffered[index - 1] != b'\r'))
        {
            return Err(RtspError::MalformedLineEnding);
        }
        if find_crlf(buffered).is_none() && buffered.len() > limits.max_request_line_bytes {
            return Err(RtspError::RequestLineTooLarge {
                max: limits.max_request_line_bytes,
            });
        }
        if buffered.len() > limits.max_header_bytes {
            return Err(RtspError::HeadersTooLarge {
                max: limits.max_header_bytes,
            });
        }
        return Ok(None);
    };
    let header_end = header_start_end + 4;
    if header_end > limits.max_header_bytes {
        return Err(RtspError::HeadersTooLarge {
            max: limits.max_header_bytes,
        });
    }
    let (method, target, version, headers, content_length) =
        parse_head(&buffered[..header_end], limits)?;
    let body_cap =
        max_body_for_request(&method, &target, &headers).unwrap_or(limits.default_max_body_bytes);
    if content_length > body_cap {
        return Err(RtspError::BodyTooLarge {
            actual: content_length,
            max: body_cap,
        });
    }
    let total = header_end
        .checked_add(content_length)
        .ok_or(RtspError::BodyTooLarge {
            actual: usize::MAX,
            max: body_cap,
        })?;
    if buffered.len() < total {
        return Ok(None);
    }
    Ok(Some((
        Request {
            method,
            target,
            version,
            headers,
            body: buffered[header_end..total].to_vec(),
        },
        total,
    )))
}

fn validate_limits(limits: RequestLimits) -> Result<(), RtspError> {
    if limits.max_request_line_bytes == 0
        || limits.max_header_bytes < 4
        || limits.max_request_line_bytes > limits.max_header_bytes
        || limits.max_header_count == 0
    {
        return Err(RtspError::InvalidLimits);
    }
    Ok(())
}

fn parse_head(
    head: &[u8],
    limits: RequestLimits,
) -> Result<(String, String, String, Vec<Header>, usize), RtspError> {
    let request_line_end = find_crlf(head).ok_or(RtspError::MalformedRequestLine)?;
    if request_line_end > limits.max_request_line_bytes {
        return Err(RtspError::RequestLineTooLarge {
            max: limits.max_request_line_bytes,
        });
    }
    let request_line = &head[..request_line_end];
    if !request_line.is_ascii() {
        return Err(RtspError::MalformedRequestLine);
    }
    let mut parts = request_line.split(|byte| *byte == b' ');
    let method = parts.next().filter(|part| !part.is_empty());
    let target = parts.next().filter(|part| !part.is_empty());
    let version = parts.next().filter(|part| !part.is_empty());
    if method.is_none() || target.is_none() || version.is_none() || parts.next().is_some() {
        return Err(RtspError::MalformedRequestLine);
    }
    let method = method.unwrap();
    let target = target.unwrap();
    let version = version.unwrap();
    if !method.iter().all(|byte| is_token(*byte))
        || target.iter().any(|byte| byte.is_ascii_control())
        || !matches!(version, b"RTSP/1.0" | b"HTTP/1.0" | b"HTTP/1.1")
    {
        return Err(RtspError::MalformedRequestLine);
    }

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut offset = request_line_end + 2;
    while offset < head.len() - 2 {
        let relative_end = find_crlf(&head[offset..]).ok_or(RtspError::MalformedHeader)?;
        if relative_end == 0 {
            break;
        }
        if headers.len() == limits.max_header_count {
            return Err(RtspError::TooManyHeaders {
                max: limits.max_header_count,
            });
        }
        let line = &head[offset..offset + relative_end];
        if matches!(line.first(), Some(b' ' | b'\t')) {
            return Err(RtspError::ObsoleteHeaderFolding);
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(RtspError::MalformedHeader)?;
        let name = &line[..colon];
        validate_header_name(name)?;
        let value = trim_ows(&line[colon + 1..]);
        validate_header_value(value)?;
        let name = String::from_utf8(name.to_vec()).expect("validated ASCII header name");

        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RtspError::TransferEncodingUnsupported);
        }
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = parse_content_length(value)?;
            if let Some(previous) = content_length {
                return Err(if previous == parsed {
                    RtspError::DuplicateContentLength
                } else {
                    RtspError::ConflictingContentLength
                });
            }
            content_length = Some(parsed);
        }

        headers.push(Header {
            name,
            value: value.to_vec(),
        });
        offset += relative_end + 2;
    }

    Ok((
        String::from_utf8(method.to_vec()).expect("ASCII method"),
        String::from_utf8(target.to_vec()).expect("ASCII target"),
        String::from_utf8(version.to_vec()).expect("ASCII version"),
        headers,
        content_length.unwrap_or(0),
    ))
}

fn parse_content_length(value: &[u8]) -> Result<usize, RtspError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(RtspError::InvalidContentLength);
    }
    let text = std::str::from_utf8(value).expect("digits are utf-8");
    text.parse().map_err(|_| RtspError::InvalidContentLength)
}

fn find_crlf(value: &[u8]) -> Option<usize> {
    value.windows(2).position(|window| window == b"\r\n")
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn validate_header_name(name: &[u8]) -> Result<(), RtspError> {
    if name.is_empty() || !name.iter().all(|byte| is_token(*byte)) {
        return Err(RtspError::MalformedHeader);
    }
    Ok(())
}

fn validate_header_value(value: &[u8]) -> Result<(), RtspError> {
    // RFC-style field values may contain HTAB, SP/VCHAR and opaque bytes
    // 0x80..=0xff. Refuse every other control byte (including ESC and DEL),
    // so even accidentally logged non-sensitive headers cannot inject terminal
    // control sequences.
    if value
        .iter()
        .any(|byte| !matches!(*byte, b'\t' | b' '..=b'~' | 0x80..=0xff))
    {
        return Err(RtspError::MalformedHeader);
    }
    Ok(())
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn is_sensitive_header(name: &str) -> bool {
    matches_ignore_ascii_case(
        name,
        &[
            "authorization",
            "proxy-authorization",
            "www-authenticate",
            "proxy-authenticate",
            "apple-challenge",
            "apple-response",
            "x-apple-hkp",
            "x-apple-hkdf",
            "active-remote",
        ],
    ) || name.to_ascii_lowercase().contains("token")
        || name.to_ascii_lowercase().contains("secret")
        || name.to_ascii_lowercase().contains("key")
}

fn matches_ignore_ascii_case(value: &str, choices: &[&str]) -> bool {
    choices
        .iter()
        .any(|choice| value.eq_ignore_ascii_case(choice))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: impl Into<String>) -> Self {
        Self {
            version: "RTSP/1.0".to_owned(),
            status,
            reason: reason.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Encode a response with one authoritative Content-Length. Callers may
    /// supply CSeq and media headers but may not override framing.
    pub fn encode(&self) -> Result<Vec<u8>, RtspError> {
        if !matches!(self.version.as_str(), "RTSP/1.0" | "HTTP/1.0" | "HTTP/1.1")
            || !(100..=999).contains(&self.status)
            || self.reason.is_empty()
            || !self
                .reason
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        {
            return Err(RtspError::InvalidResponse);
        }

        let mut encoded = Vec::new();
        encoded.extend_from_slice(self.version.as_bytes());
        encoded.extend_from_slice(format!(" {} {}\r\n", self.status, self.reason).as_bytes());
        for header in &self.headers {
            validate_header_name(header.name.as_bytes())?;
            validate_header_value(&header.value)?;
            if header.name.eq_ignore_ascii_case("content-length")
                || header.name.eq_ignore_ascii_case("transfer-encoding")
            {
                return Err(RtspError::ReservedResponseHeader {
                    name: header.name.clone(),
                });
            }
            encoded.extend_from_slice(header.name.as_bytes());
            encoded.extend_from_slice(b": ");
            encoded.extend_from_slice(&header.value);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded
            .extend_from_slice(format!("Content-Length: {}\r\n\r\n", self.body.len()).as_bytes());
        encoded.extend_from_slice(&self.body);
        Ok(encoded)
    }
}

#[derive(Debug)]
pub enum RtspError {
    Io(io::Error),
    InvalidLimits,
    RequestLineTooLarge { max: usize },
    HeadersTooLarge { max: usize },
    TooManyHeaders { max: usize },
    BodyTooLarge { actual: usize, max: usize },
    BufferedDataTooLarge { actual: usize, max: usize },
    PoisonedDecoder,
    UnexpectedEofInHeaders,
    UnexpectedEofInBody { expected: usize },
    MalformedLineEnding,
    MalformedRequestLine,
    MalformedHeader,
    ObsoleteHeaderFolding,
    InvalidContentLength,
    DuplicateContentLength,
    ConflictingContentLength,
    TransferEncodingUnsupported,
    InvalidResponse,
    ReservedResponseHeader { name: String },
}

impl PartialEq for RtspError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            (Self::InvalidLimits, Self::InvalidLimits)
            | (Self::UnexpectedEofInHeaders, Self::UnexpectedEofInHeaders)
            | (Self::MalformedLineEnding, Self::MalformedLineEnding)
            | (Self::MalformedRequestLine, Self::MalformedRequestLine)
            | (Self::MalformedHeader, Self::MalformedHeader)
            | (Self::ObsoleteHeaderFolding, Self::ObsoleteHeaderFolding)
            | (Self::InvalidContentLength, Self::InvalidContentLength)
            | (Self::DuplicateContentLength, Self::DuplicateContentLength)
            | (Self::ConflictingContentLength, Self::ConflictingContentLength)
            | (Self::TransferEncodingUnsupported, Self::TransferEncodingUnsupported)
            | (Self::InvalidResponse, Self::InvalidResponse)
            | (Self::PoisonedDecoder, Self::PoisonedDecoder) => true,
            (Self::RequestLineTooLarge { max: a }, Self::RequestLineTooLarge { max: b })
            | (Self::HeadersTooLarge { max: a }, Self::HeadersTooLarge { max: b })
            | (Self::TooManyHeaders { max: a }, Self::TooManyHeaders { max: b }) => a == b,
            (
                Self::BodyTooLarge {
                    actual: aa,
                    max: am,
                },
                Self::BodyTooLarge {
                    actual: ba,
                    max: bm,
                },
            ) => aa == ba && am == bm,
            (
                Self::BufferedDataTooLarge {
                    actual: aa,
                    max: am,
                },
                Self::BufferedDataTooLarge {
                    actual: ba,
                    max: bm,
                },
            ) => aa == ba && am == bm,
            (
                Self::UnexpectedEofInBody { expected: a },
                Self::UnexpectedEofInBody { expected: b },
            ) => a == b,
            (
                Self::ReservedResponseHeader { name: a },
                Self::ReservedResponseHeader { name: b },
            ) => a == b,
            _ => false,
        }
    }
}

impl Eq for RtspError {}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "control socket I/O failed: {error}"),
            Self::InvalidLimits => f.write_str("invalid RTSP resource limits"),
            Self::RequestLineTooLarge { max } => write!(f, "request line exceeds {max} bytes"),
            Self::HeadersTooLarge { max } => write!(f, "request headers exceed {max} bytes"),
            Self::TooManyHeaders { max } => write!(f, "request exceeds {max} headers"),
            Self::BodyTooLarge { actual, max } => {
                write!(f, "request body is {actual} bytes; endpoint limit is {max}")
            }
            Self::BufferedDataTooLarge { actual, max } => {
                write!(
                    f,
                    "buffered control plaintext is {actual} bytes; limit is {max}"
                )
            }
            Self::PoisonedDecoder => f.write_str("control request decoder is poisoned"),
            Self::UnexpectedEofInHeaders => f.write_str("connection ended inside request headers"),
            Self::UnexpectedEofInBody { expected } => {
                write!(
                    f,
                    "connection ended before the {expected}-byte body completed"
                )
            }
            Self::MalformedLineEnding => f.write_str("request contains a bare LF"),
            Self::MalformedRequestLine => f.write_str("malformed request line"),
            Self::MalformedHeader => f.write_str("malformed request header"),
            Self::ObsoleteHeaderFolding => f.write_str("folded request headers are unsupported"),
            Self::InvalidContentLength => f.write_str("invalid Content-Length"),
            Self::DuplicateContentLength => f.write_str("duplicate Content-Length is refused"),
            Self::ConflictingContentLength => f.write_str("conflicting Content-Length values"),
            Self::TransferEncodingUnsupported => f.write_str("Transfer-Encoding is unsupported"),
            Self::InvalidResponse => f.write_str("invalid response status line"),
            Self::ReservedResponseHeader { name } => {
                write!(f, "response header {name} is owned by the encoder")
            }
        }
    }
}

impl Error for RtspError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(bytes: &[u8]) -> impl AsyncRead + Unpin + '_ {
        std::io::Cursor::new(bytes)
    }

    async fn parse(bytes: &[u8]) -> Result<Option<Request>, RtspError> {
        read_request(&mut reader(bytes), RequestLimits::default(), |_, _, _| None).await
    }

    #[tokio::test]
    async fn parses_binary_body_and_leaves_pipelined_request_unread() {
        let bytes = b"POST /pair-setup HTTP/1.1\r\nContent-Length: 3\r\nCSeq: 7\r\n\r\n\x00\xff\x01OPTIONS * RTSP/1.0\r\n\r\n";
        let mut input = reader(bytes);
        let first = read_request(&mut input, RequestLimits::default(), |_, _, _| None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.method(), "POST");
        assert_eq!(first.target(), "/pair-setup");
        assert_eq!(first.version(), "HTTP/1.1");
        assert_eq!(first.header("cseq"), Some(&b"7"[..]));
        assert_eq!(first.body(), b"\x00\xff\x01");
        let second = read_request(&mut input, RequestLimits::default(), |_, _, _| None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.method(), "OPTIONS");
    }

    #[test]
    fn incremental_decoder_preserves_arbitrary_splits_and_pipeline_remainder() {
        let wire = b"POST /pair-setup HTTP/1.1\r\nContent-Length: 3\r\nCSeq: 7\r\n\r\n\x00\xff\x01OPTIONS * RTSP/1.0\r\nCSeq: 8\r\n\r\n";
        for split in 0..=wire.len() {
            let mut decoder = RequestDecoder::new(RequestLimits::default()).unwrap();
            decoder.feed(&wire[..split]).unwrap();
            let first = decoder.next_request(|_, _, _| None).unwrap();
            decoder.feed(&wire[split..]).unwrap();
            let first = match first {
                Some(request) => request,
                None => decoder.next_request(|_, _, _| None).unwrap().unwrap(),
            };
            assert_eq!(first.target(), "/pair-setup", "split={split}");
            assert_eq!(first.body(), b"\x00\xff\x01", "split={split}");
            let second = decoder.next_request(|_, _, _| None).unwrap().unwrap();
            assert_eq!(second.method(), "OPTIONS", "split={split}");
            assert_eq!(second.header("CSeq"), Some(&b"8"[..]), "split={split}");
            assert_eq!(decoder.buffered_len(), 0, "split={split}");
        }
    }

    #[test]
    fn incremental_decoder_retains_partial_body_and_poison_is_terminal() {
        let mut limits = RequestLimits::default();
        limits.default_max_body_bytes = 8;
        let mut decoder = RequestDecoder::new(limits).unwrap();
        decoder
            .feed(b"POST /x RTSP/1.0\r\nContent-Length: 4\r\n\r\n12")
            .unwrap();
        assert!(decoder.next_request(|_, _, _| None).unwrap().is_none());
        assert!(decoder.buffered_len() > 2);
        decoder.feed(b"34").unwrap();
        assert_eq!(
            decoder
                .next_request(|_, _, _| None)
                .unwrap()
                .unwrap()
                .body(),
            b"1234"
        );

        let mut malformed = RequestDecoder::new(limits).unwrap();
        malformed.feed(b"GET / HTTP/1.1\n\n").unwrap();
        assert_eq!(
            malformed.next_request(|_, _, _| None).unwrap_err(),
            RtspError::MalformedLineEnding
        );
        assert_eq!(
            malformed.next_request(|_, _, _| None).unwrap_err(),
            RtspError::PoisonedDecoder
        );
    }

    #[test]
    fn incremental_decoder_can_transfer_remainder_at_encryption_boundary() {
        let mut decoder = RequestDecoder::new(RequestLimits::default()).unwrap();
        decoder
            .feed(b"POST /pair-setup RTSP/1.0\r\nContent-Length: 0\r\n\r\n\x11\x22")
            .unwrap();
        assert!(decoder.next_request(|_, _, _| None).unwrap().is_some());
        assert_eq!(decoder.take_buffered(), vec![0x11, 0x22]);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn incremental_decoder_bounds_bytes_before_header_or_body_completion() {
        let limits = RequestLimits {
            max_request_line_bytes: 16,
            max_header_bytes: 32,
            max_header_count: 4,
            default_max_body_bytes: 8,
        };
        let mut decoder = RequestDecoder::new(limits).unwrap();
        let error = decoder.feed(&[b'x'; 41]).unwrap_err();
        assert_eq!(
            error,
            RtspError::BufferedDataTooLarge {
                actual: 41,
                max: 40,
            }
        );
        assert_eq!(decoder.feed(b"x").unwrap_err(), RtspError::PoisonedDecoder);
    }

    #[test]
    fn incremental_decoder_body_budget_can_be_promoted_after_authentication() {
        let large = vec![0u8; 128 * 1024];

        let mut plaintext = RequestDecoder::new(RequestLimits::default()).unwrap();
        assert!(matches!(
            plaintext.feed(&large),
            Err(RtspError::BufferedDataTooLarge { .. })
        ));

        let mut authenticated = RequestDecoder::new(RequestLimits::default()).unwrap();
        authenticated
            .set_default_max_body_bytes(5 * 1024 * 1024)
            .unwrap();
        authenticated.feed(&large).unwrap();
        assert_eq!(authenticated.buffered_len(), large.len());
    }

    #[tokio::test]
    async fn applies_method_specific_body_cap_before_allocation() {
        let bytes = b"POST /x RTSP/1.0\r\nContent-Length: 9\r\n\r\n123456789";
        assert_eq!(
            read_request(
                &mut reader(bytes),
                RequestLimits::default(),
                |method, target, _| {
                    assert_eq!(target, "/x");
                    (method == "POST").then_some(8)
                },
            )
            .await
            .unwrap_err(),
            RtspError::BodyTooLarge { actual: 9, max: 8 }
        );
    }

    #[tokio::test]
    async fn refuses_duplicate_conflicting_and_invalid_lengths() {
        for (headers, expected) in [
            (
                "Content-Length: 2\r\nContent-Length: 2",
                RtspError::DuplicateContentLength,
            ),
            (
                "Content-Length: 2\r\nContent-Length: 3",
                RtspError::ConflictingContentLength,
            ),
            ("Content-Length: +2", RtspError::InvalidContentLength),
            ("Content-Length: 2, 2", RtspError::InvalidContentLength),
        ] {
            let wire = format!("POST / RTSP/1.0\r\n{headers}\r\n\r\n");
            assert_eq!(parse(wire.as_bytes()).await.unwrap_err(), expected);
        }
    }

    #[tokio::test]
    async fn refuses_transfer_encoding_even_when_identity_or_content_length_exists() {
        for value in ["chunked", "identity"] {
            let wire = format!(
                "POST / RTSP/1.0\r\nTransfer-Encoding: {value}\r\nContent-Length: 0\r\n\r\n"
            );
            assert_eq!(
                parse(wire.as_bytes()).await.unwrap_err(),
                RtspError::TransferEncodingUnsupported
            );
        }
    }

    #[tokio::test]
    async fn enforces_request_line_header_byte_and_count_limits() {
        let mut limits = RequestLimits::default();
        limits.max_request_line_bytes = 8;
        limits.max_header_bytes = 64;
        assert_eq!(
            read_request(
                &mut reader(b"OPTIONS * RTSP/1.0\r\n\r\n"),
                limits,
                |_, _, _| None,
            )
            .await
            .unwrap_err(),
            RtspError::RequestLineTooLarge { max: 8 }
        );

        limits.max_request_line_bytes = 32;
        limits.max_header_bytes = 32;
        assert_eq!(
            read_request(
                &mut reader(b"GET / RTSP/1.0\r\nLong: 12345678901234567890\r\n\r\n"),
                limits,
                |_, _, _| None,
            )
            .await
            .unwrap_err(),
            RtspError::HeadersTooLarge { max: 32 }
        );

        limits.max_header_bytes = 128;
        limits.max_header_count = 1;
        assert_eq!(
            read_request(
                &mut reader(b"GET / RTSP/1.0\r\nA: 1\r\nB: 2\r\n\r\n"),
                limits,
                |_, _, _| None,
            )
            .await
            .unwrap_err(),
            RtspError::TooManyHeaders { max: 1 }
        );
    }

    #[tokio::test]
    async fn rejects_bare_lf_obs_fold_and_truncated_messages() {
        assert_eq!(
            parse(b"GET / RTSP/1.0\n\n").await.unwrap_err(),
            RtspError::MalformedLineEnding
        );
        assert_eq!(
            parse(b"GET / RTSP/1.0\r\nX: a\r\n b\r\n\r\n")
                .await
                .unwrap_err(),
            RtspError::ObsoleteHeaderFolding
        );
        assert_eq!(
            parse(b"GET / RTSP/1.0\r\nX: a").await.unwrap_err(),
            RtspError::UnexpectedEofInHeaders
        );
        assert_eq!(
            parse(b"POST / RTSP/1.0\r\nContent-Length: 2\r\n\r\n1")
                .await
                .unwrap_err(),
            RtspError::UnexpectedEofInBody { expected: 2 }
        );
    }

    #[tokio::test]
    async fn clean_eof_is_not_a_parse_error() {
        assert!(parse(b"").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn debug_redacts_credentials_and_body() {
        let request = parse(
            b"POST / RTSP/1.0\r\nAuthorization: Basic very-secret\r\nX-Session-Token: token-secret\r\nUser-Agent: safe\r\nContent-Length: 11\r\n\r\nbody-secret",
        )
        .await
        .unwrap()
        .unwrap();
        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("safe"));
        assert!(debug.contains("body_len"));
        assert!(!debug.contains("very-secret"));
        assert!(!debug.contains("token-secret"));
        assert!(!debug.contains("body-secret"));
    }

    #[test]
    fn response_encoder_owns_framing_and_preserves_binary_body() {
        let mut response = Response::new(200, "OK");
        response.headers.push(Header::new("CSeq", "9").unwrap());
        response.body = vec![0, 255, 1];
        assert_eq!(
            response.encode().unwrap(),
            b"RTSP/1.0 200 OK\r\nCSeq: 9\r\nContent-Length: 3\r\n\r\n\x00\xff\x01"
        );

        response
            .headers
            .push(Header::new("Content-Length", "99").unwrap());
        assert_eq!(
            response.encode().unwrap_err(),
            RtspError::ReservedResponseHeader {
                name: "Content-Length".to_owned(),
            }
        );
    }

    #[test]
    fn header_constructor_rejects_response_splitting() {
        assert_eq!(
            Header::new("X-Test", b"ok\r\nInjected: yes".to_vec()).unwrap_err(),
            RtspError::MalformedHeader
        );
        assert_eq!(
            Header::new("Bad Name", "x").unwrap_err(),
            RtspError::MalformedHeader
        );
        assert_eq!(
            Header::new("X-Test", b"safe\x1b[31m".to_vec()).unwrap_err(),
            RtspError::MalformedHeader
        );
        assert_eq!(
            Header::new("X-Test", b"safe\x7f".to_vec()).unwrap_err(),
            RtspError::MalformedHeader
        );
    }
}
