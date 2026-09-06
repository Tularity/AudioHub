//! Bounded verification for classic RAOP Digest authentication.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use md5::{Digest as _, Md5};
use rand::{rngs::OsRng, RngCore};
use std::fmt;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

const REALM: &[u8] = b"raop";
const MAX_AUTHORIZATION_BYTES: usize = 4096;
const NONCE_BYTES: usize = 18;
const NONCE_LIFETIME: Duration = Duration::from_secs(120);

pub(crate) struct DigestChallenge {
    nonce: String,
    issued_at: Instant,
}

impl DigestChallenge {
    pub(crate) fn new() -> Self {
        let mut bytes = [0u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut bytes);
        let nonce = URL_SAFE_NO_PAD.encode(&bytes);
        bytes.zeroize();
        Self {
            nonce,
            issued_at: Instant::now(),
        }
    }

    pub(crate) fn header_value(&self) -> String {
        format!("Digest realm=\"raop\", nonce=\"{}\"", self.nonce)
    }

    /// Verify one classic RFC 2617 no-qop response against this challenge.
    ///
    /// The connection owner must create/store a challenge only when issuing a
    /// 401, enforce retry limits, and keep authentication state connection
    /// local so a new connection never inherits it. After authentication, the
    /// caller owns subsequent session policy. This primitive does not provide
    /// wire issuance or rate limiting.
    pub(crate) fn verify(
        &self,
        password: &str,
        method: &str,
        target: &str,
        authorization: &[u8],
    ) -> Result<(), DigestError> {
        if authorization.len() > MAX_AUTHORIZATION_BYTES {
            return Err(DigestError::HeaderTooLarge);
        }
        if Instant::now()
            .checked_duration_since(self.issued_at)
            .unwrap_or(Duration::ZERO)
            >= NONCE_LIFETIME
        {
            return Err(DigestError::ExpiredChallenge);
        }

        let fields = parse_authorization(authorization)?;
        let username = required_nonempty(fields.username)?;
        let realm = required_nonempty(fields.realm)?;
        let nonce = required_nonempty(fields.nonce)?;
        let uri = required_nonempty(fields.uri)?;
        let response = required_nonempty(fields.response)?;

        if fields.qop.is_some() {
            return Err(DigestError::UnsupportedQop);
        }
        if let Some(algorithm) = fields.algorithm {
            if !algorithm.eq_ignore_ascii_case(b"MD5") {
                return Err(DigestError::UnsupportedAlgorithm);
            }
        }
        if realm.as_slice() != REALM
            || nonce.as_slice() != self.nonce.as_bytes()
            || uri.as_slice() != target.as_bytes()
        {
            return Err(DigestError::InvalidCredentials);
        }

        let mut supplied = Zeroizing::new([0u8; 16]);
        if response.len() != 32
            || hex::decode_to_slice(response.as_slice(), &mut *supplied).is_err()
        {
            return Err(DigestError::MalformedAuthorization);
        }

        let mut ha1_hasher = Md5::new();
        ha1_hasher.update(username.as_slice());
        ha1_hasher.update(b":");
        ha1_hasher.update(REALM);
        ha1_hasher.update(b":");
        ha1_hasher.update(password.as_bytes());
        let mut ha1 = finish_md5(ha1_hasher);

        let mut ha2_hasher = Md5::new();
        ha2_hasher.update(method.as_bytes());
        ha2_hasher.update(b":");
        ha2_hasher.update(uri.as_slice());
        let mut ha2 = finish_md5(ha2_hasher);

        let mut ha1_hex = [0u8; 32];
        let mut ha2_hex = [0u8; 32];
        hex::encode_to_slice(&ha1, &mut ha1_hex).expect("fixed MD5 hex output length");
        hex::encode_to_slice(&ha2, &mut ha2_hex).expect("fixed MD5 hex output length");

        let mut response_hasher = Md5::new();
        response_hasher.update(&ha1_hex);
        response_hasher.update(b":");
        response_hasher.update(nonce.as_slice());
        response_hasher.update(b":");
        response_hasher.update(&ha2_hex);
        let mut expected = finish_md5(response_hasher);

        let valid = bool::from(expected.as_slice().ct_eq(supplied.as_slice()));
        ha1.zeroize();
        ha2.zeroize();
        ha1_hex.zeroize();
        ha2_hex.zeroize();
        expected.zeroize();
        supplied.zeroize();

        if valid {
            Ok(())
        } else {
            Err(DigestError::InvalidCredentials)
        }
    }
}

impl fmt::Debug for DigestChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigestChallenge")
            .field("nonce", &"<redacted>")
            .field("issued_at", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestError {
    HeaderTooLarge,
    MalformedAuthorization,
    MissingField,
    UnsupportedQop,
    UnsupportedAlgorithm,
    ExpiredChallenge,
    InvalidCredentials,
}

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HeaderTooLarge => "Digest authorization header is too large",
            Self::MalformedAuthorization => "malformed Digest authorization",
            Self::MissingField => "Digest authorization is missing a required field",
            Self::UnsupportedQop => "Digest qop is unsupported",
            Self::UnsupportedAlgorithm => "Digest algorithm is unsupported",
            Self::ExpiredChallenge => "Digest challenge has expired",
            Self::InvalidCredentials => "Digest credentials are invalid",
        })
    }
}

impl std::error::Error for DigestError {}

#[derive(Default)]
struct DigestFields {
    username: Option<Zeroizing<Vec<u8>>>,
    realm: Option<Zeroizing<Vec<u8>>>,
    nonce: Option<Zeroizing<Vec<u8>>>,
    uri: Option<Zeroizing<Vec<u8>>>,
    response: Option<Zeroizing<Vec<u8>>>,
    algorithm: Option<Zeroizing<Vec<u8>>>,
    qop: Option<Zeroizing<Vec<u8>>>,
}

fn parse_authorization(input: &[u8]) -> Result<DigestFields, DigestError> {
    if input.len() > MAX_AUTHORIZATION_BYTES {
        return Err(DigestError::HeaderTooLarge);
    }
    std::str::from_utf8(input).map_err(|_| DigestError::MalformedAuthorization)?;
    if input.iter().any(|byte| *byte < 0x20 || *byte == 0x7f) {
        return Err(DigestError::MalformedAuthorization);
    }

    let mut cursor = 0;
    skip_spaces(input, &mut cursor);
    let scheme_start = cursor;
    while cursor < input.len() && input[cursor].is_ascii_alphabetic() {
        cursor += 1;
    }
    if !input[scheme_start..cursor].eq_ignore_ascii_case(b"Digest")
        || cursor == input.len()
        || input[cursor] != b' '
    {
        return Err(DigestError::MalformedAuthorization);
    }
    skip_spaces(input, &mut cursor);
    if cursor == input.len() {
        return Err(DigestError::MalformedAuthorization);
    }

    let mut fields = DigestFields::default();
    loop {
        let name_start = cursor;
        while cursor < input.len() && input[cursor].is_ascii_alphanumeric() {
            cursor += 1;
        }
        if name_start == cursor {
            return Err(DigestError::MalformedAuthorization);
        }
        let name = &input[name_start..cursor];
        skip_spaces(input, &mut cursor);
        if cursor == input.len() || input[cursor] != b'=' {
            return Err(DigestError::MalformedAuthorization);
        }
        cursor += 1;
        skip_spaces(input, &mut cursor);
        let value = parse_value(input, &mut cursor)?;

        let slot = if name.eq_ignore_ascii_case(b"username") {
            &mut fields.username
        } else if name.eq_ignore_ascii_case(b"realm") {
            &mut fields.realm
        } else if name.eq_ignore_ascii_case(b"nonce") {
            &mut fields.nonce
        } else if name.eq_ignore_ascii_case(b"uri") {
            &mut fields.uri
        } else if name.eq_ignore_ascii_case(b"response") {
            &mut fields.response
        } else if name.eq_ignore_ascii_case(b"algorithm") {
            &mut fields.algorithm
        } else if name.eq_ignore_ascii_case(b"qop") {
            &mut fields.qop
        } else {
            return Err(DigestError::MalformedAuthorization);
        };
        if slot.replace(value).is_some() {
            return Err(DigestError::MalformedAuthorization);
        }

        skip_spaces(input, &mut cursor);
        if cursor == input.len() {
            break;
        }
        if input[cursor] != b',' {
            return Err(DigestError::MalformedAuthorization);
        }
        cursor += 1;
        skip_spaces(input, &mut cursor);
        if cursor == input.len() {
            return Err(DigestError::MalformedAuthorization);
        }
    }
    Ok(fields)
}

fn parse_value(input: &[u8], cursor: &mut usize) -> Result<Zeroizing<Vec<u8>>, DigestError> {
    if *cursor == input.len() {
        return Err(DigestError::MalformedAuthorization);
    }
    if input[*cursor] != b'"' {
        let start = *cursor;
        while *cursor < input.len() && input[*cursor] != b',' && input[*cursor] != b' ' {
            if !is_token_byte(input[*cursor]) {
                return Err(DigestError::MalformedAuthorization);
            }
            *cursor += 1;
        }
        if start == *cursor {
            return Err(DigestError::MalformedAuthorization);
        }
        return Ok(Zeroizing::new(input[start..*cursor].to_vec()));
    }

    *cursor += 1;
    let mut value = Zeroizing::new(Vec::new());
    loop {
        if *cursor == input.len() {
            return Err(DigestError::MalformedAuthorization);
        }
        match input[*cursor] {
            b'"' => {
                *cursor += 1;
                return Ok(value);
            }
            b'\\' => {
                *cursor += 1;
                if *cursor == input.len() {
                    return Err(DigestError::MalformedAuthorization);
                }
                value.push(input[*cursor]);
                *cursor += 1;
            }
            byte => {
                value.push(byte);
                *cursor += 1;
            }
        }
    }
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn skip_spaces(input: &[u8], cursor: &mut usize) {
    while *cursor < input.len() && input[*cursor] == b' ' {
        *cursor += 1;
    }
}

fn required_nonempty(value: Option<Zeroizing<Vec<u8>>>) -> Result<Zeroizing<Vec<u8>>, DigestError> {
    match value {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(DigestError::MissingField),
    }
}

fn finish_md5(hasher: Md5) -> [u8; 16] {
    let mut output = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&output);
    output.as_mut_slice().zeroize();
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "NjM4MDAwMDAx";
    const TARGET: &str = "rtsp://127.0.0.1/1";
    const KNOWN_AUTHORIZATION: &str = concat!(
        "Digest username=\"dev\", realm=\"raop\", nonce=\"NjM4MDAwMDAx\", ",
        "uri=\"rtsp://127.0.0.1/1\", response=\"556e89d3c27fc12841e76d2dc2d67dc4\""
    );

    fn challenge() -> DigestChallenge {
        DigestChallenge {
            nonce: NONCE.to_owned(),
            issued_at: Instant::now(),
        }
    }

    fn response_for(username: &str, password: &str, method: &str, uri: &str) -> String {
        let ha1 = Md5::digest(format!("{username}:raop:{password}").as_bytes());
        let ha2 = Md5::digest(format!("{method}:{uri}").as_bytes());
        let material = format!("{:x}:{NONCE}:{:x}", ha1, ha2);
        format!("{:x}", Md5::digest(material.as_bytes()))
    }

    #[test]
    fn verifies_independent_classic_digest_vector() {
        challenge()
            .verify("1234", "SETUP", TARGET, KNOWN_AUTHORIZATION.as_bytes())
            .unwrap();
    }

    #[test]
    fn accepts_uppercase_response_hex() {
        let authorization = KNOWN_AUTHORIZATION.replace(
            "556e89d3c27fc12841e76d2dc2d67dc4",
            "556E89D3C27FC12841E76D2DC2D67DC4",
        );
        challenge()
            .verify("1234", "SETUP", TARGET, authorization.as_bytes())
            .unwrap();
    }

    #[test]
    fn rejects_wrong_password_realm_nonce_method_and_target() {
        assert_eq!(
            challenge().verify("wrong", "SETUP", TARGET, KNOWN_AUTHORIZATION.as_bytes()),
            Err(DigestError::InvalidCredentials)
        );
        for (needle, replacement) in [("realm=\"raop\"", "realm=\"other\""), (NONCE, "other")] {
            let authorization = KNOWN_AUTHORIZATION.replace(needle, replacement);
            assert_eq!(
                challenge().verify("1234", "SETUP", TARGET, authorization.as_bytes()),
                Err(DigestError::InvalidCredentials)
            );
        }
        assert_eq!(
            challenge().verify("1234", "RECORD", TARGET, KNOWN_AUTHORIZATION.as_bytes()),
            Err(DigestError::InvalidCredentials)
        );
        assert_eq!(
            challenge().verify(
                "1234",
                "SETUP",
                "rtsp://127.0.0.1/2",
                KNOWN_AUTHORIZATION.as_bytes(),
            ),
            Err(DigestError::InvalidCredentials)
        );
    }

    #[test]
    fn rejects_duplicate_malformed_unknown_and_overlong_headers() {
        let duplicate = format!("{KNOWN_AUTHORIZATION}, username=\"again\"");
        assert_eq!(
            challenge().verify("1234", "SETUP", TARGET, duplicate.as_bytes()),
            Err(DigestError::MalformedAuthorization)
        );
        for malformed in [
            b"Basic abc".as_slice(),
            b"Digest username=\"unterminated".as_slice(),
            b"Digest username=\"trailing\\".as_slice(),
            b"Digest username=\"dev\",".as_slice(),
            b"Digest username=\"dev\", opaque=\"secret\"".as_slice(),
            b"Digest username=\"dev\" realm=\"raop\"".as_slice(),
            b"Digest username=\"dev\"\r\nrealm=\"raop\"".as_slice(),
            b"Digest username=\xff".as_slice(),
        ] {
            assert_eq!(
                challenge().verify("1234", "SETUP", TARGET, malformed),
                Err(DigestError::MalformedAuthorization)
            );
        }
        let overlong = vec![b'a'; MAX_AUTHORIZATION_BYTES + 1];
        assert_eq!(
            challenge().verify("1234", "SETUP", TARGET, &overlong),
            Err(DigestError::HeaderTooLarge)
        );
    }

    #[test]
    fn unescapes_quoted_quote_and_backslash() {
        let username = "dev\"ops\\lab";
        let response = response_for(username, "1234", "SETUP", TARGET);
        let authorization = format!(
            "Digest username=\"dev\\\"ops\\\\lab\", realm=\"raop\", nonce=\"{NONCE}\", \
             uri=\"{TARGET}\", response=\"{response}\""
        );
        challenge()
            .verify("1234", "SETUP", TARGET, authorization.as_bytes())
            .unwrap();
    }

    #[test]
    fn rejects_unsupported_qop_and_algorithm() {
        for directive in ["qop=auth", "algorithm=SHA-256"] {
            let authorization = format!("{KNOWN_AUTHORIZATION}, {directive}");
            assert!(matches!(
                challenge().verify("1234", "SETUP", TARGET, authorization.as_bytes()),
                Err(DigestError::UnsupportedQop | DigestError::UnsupportedAlgorithm)
            ));
        }
        let with_md5 = format!("{KNOWN_AUTHORIZATION}, algorithm=MD5");
        challenge()
            .verify("1234", "SETUP", TARGET, with_md5.as_bytes())
            .unwrap();
    }

    #[test]
    fn expired_nonce_is_rejected_without_sleeping() {
        let expired = DigestChallenge {
            nonce: NONCE.to_owned(),
            issued_at: Instant::now() - NONCE_LIFETIME,
        };
        assert_eq!(
            expired.verify("1234", "SETUP", TARGET, KNOWN_AUTHORIZATION.as_bytes()),
            Err(DigestError::ExpiredChallenge)
        );
    }

    #[test]
    fn challenge_header_is_classic_and_debug_is_redacted() {
        let challenge = challenge();
        assert_eq!(
            challenge.header_value(),
            "Digest realm=\"raop\", nonce=\"NjM4MDAwMDAx\""
        );
        let debug = format!("{challenge:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(NONCE));
        assert!(!debug.contains("1234"));
    }
}
