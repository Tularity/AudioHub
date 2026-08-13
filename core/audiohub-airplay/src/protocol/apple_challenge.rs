//! Strict construction of the legacy `Apple-Response` compatibility header.
//!
//! The request parsing and byte layout here are AudioHub-owned Rust. The RSA
//! private operation is deliberately abstracted behind a provider so the
//! separately sourced AirPort Express key cannot be mistaken for Apache-2.0
//! project material. See `airport_express.rs`, `PROVENANCE.md`, and the root
//! `NOTICE.md` before redistributing a build with the bundled provider.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use std::fmt;
use std::net::IpAddr;

const MAX_CHALLENGE_BYTES: usize = 16;
const MIN_SIGNED_INPUT_BYTES: usize = 32;
const EXPECTED_SIGNATURE_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppleChallengeError {
    MalformedChallenge,
    ProviderUnavailable,
    InvalidProviderResponse,
}

impl fmt::Display for AppleChallengeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MalformedChallenge => "malformed Apple challenge",
            Self::ProviderUnavailable => "AirPort Express compatibility provider is unavailable",
            Self::InvalidProviderResponse => {
                "AirPort Express compatibility provider returned an invalid response"
            }
        })
    }
}

impl std::error::Error for AppleChallengeError {}

/// Isolates the private PKCS#1 v1.5 operation from AudioHub's request logic.
pub(crate) trait AppleResponseProvider: Send + Sync {
    fn private_pkcs1_v15(&self, input: &[u8]) -> Result<Vec<u8>, AppleChallengeError>;
}

/// Validated bytes awaiting the deliberately isolated private-key operation.
///
/// Keeping preparation separate lets the network boundary reject malformed
/// input before reserving one of the bounded blocking-worker permits.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PreparedAppleResponse {
    input: Vec<u8>,
}

/// Validate and prepare an `Apple-Challenge` only when the header is present.
///
/// Stock senders omit Base64 padding, while some interoperable senders retain
/// it. Accept either exact canonical standard representation, but never mixed,
/// malformed, or non-canonical tail bits. The signed message is challenge ||
/// local socket IP || receiver MAC, zero padded to at least 32 bytes.
pub(crate) fn prepare(
    encoded_challenge: Option<&[u8]>,
    local_ip: IpAddr,
    receiver_mac: [u8; 6],
) -> Result<Option<PreparedAppleResponse>, AppleChallengeError> {
    let Some(encoded_challenge) = encoded_challenge else {
        return Ok(None);
    };
    if encoded_challenge.is_empty() {
        return Err(AppleChallengeError::MalformedChallenge);
    }

    let padded = encoded_challenge.contains(&b'=');
    let challenge = if padded {
        STANDARD.decode(encoded_challenge)
    } else {
        STANDARD_NO_PAD.decode(encoded_challenge)
    }
    .map_err(|_| AppleChallengeError::MalformedChallenge)?;
    if challenge.is_empty() || challenge.len() > MAX_CHALLENGE_BYTES {
        return Err(AppleChallengeError::MalformedChallenge);
    }
    let canonical = if padded {
        STANDARD.encode(&challenge)
    } else {
        STANDARD_NO_PAD.encode(&challenge)
    };
    if canonical.as_bytes() != encoded_challenge {
        return Err(AppleChallengeError::MalformedChallenge);
    }

    let ip_len = match local_ip {
        IpAddr::V4(_) => 4,
        IpAddr::V6(_) => 16,
    };
    let unpadded_len = challenge.len() + ip_len + receiver_mac.len();
    let mut input = Vec::with_capacity(unpadded_len.max(MIN_SIGNED_INPUT_BYTES));
    input.extend_from_slice(&challenge);
    match local_ip {
        IpAddr::V4(address) => input.extend_from_slice(&address.octets()),
        IpAddr::V6(address) => input.extend_from_slice(&address.octets()),
    }
    input.extend_from_slice(&receiver_mac);
    input.resize(unpadded_len.max(MIN_SIGNED_INPUT_BYTES), 0);

    Ok(Some(PreparedAppleResponse { input }))
}

/// Perform the private operation and build an unpadded `Apple-Response`.
pub(crate) fn respond(
    provider: &dyn AppleResponseProvider,
    prepared: PreparedAppleResponse,
) -> Result<String, AppleChallengeError> {
    let signature = provider.private_pkcs1_v15(&prepared.input)?;
    if signature.len() != EXPECTED_SIGNATURE_BYTES {
        return Err(AppleChallengeError::InvalidProviderResponse);
    }
    let padded = STANDARD.encode(signature);
    Ok(padded.trim_end_matches('=').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingProvider {
        input: Mutex<Vec<u8>>,
    }

    impl AppleResponseProvider for RecordingProvider {
        fn private_pkcs1_v15(&self, input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
            *self.input.lock().unwrap() = input.to_vec();
            Ok(vec![0xa5; EXPECTED_SIGNATURE_BYTES])
        }
    }

    #[test]
    fn absent_header_does_not_call_provider() {
        assert_eq!(
            prepare(None, IpAddr::V4(Ipv4Addr::LOCALHOST), [0; 6]).unwrap(),
            None
        );
    }

    #[test]
    fn ipv4_layout_is_zero_padded_to_32_bytes() {
        let provider = RecordingProvider::default();
        let prepared = prepare(
            Some(b"AQIDBA"),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
        )
        .unwrap()
        .unwrap();
        let response = respond(&provider, prepared).unwrap();
        assert!(!response.contains('='));
        assert_eq!(STANDARD_NO_PAD.decode(response).unwrap(), vec![0xa5; 256]);

        let input = provider.input.lock().unwrap();
        assert_eq!(input.len(), 32);
        assert_eq!(&input[..4], &[1, 2, 3, 4]);
        assert_eq!(&input[4..8], &[192, 168, 1, 20]);
        assert_eq!(&input[8..14], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert!(input[14..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn full_ipv6_layout_is_38_bytes_without_truncation() {
        let provider = RecordingProvider::default();
        let challenge: Vec<u8> = (0..16).collect();
        let encoded = STANDARD_NO_PAD.encode(&challenge);
        let address = Ipv6Addr::new(0x2001, 0x0db8, 1, 2, 3, 4, 5, 6);
        let prepared = prepare(
            Some(encoded.as_bytes()),
            IpAddr::V6(address),
            [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee],
        )
        .unwrap()
        .unwrap();
        respond(&provider, prepared).unwrap();

        let input = provider.input.lock().unwrap();
        assert_eq!(input.len(), 38);
        assert_eq!(&input[..16], challenge);
        assert_eq!(&input[16..32], &address.octets());
        assert_eq!(&input[32..], &[0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
    }

    #[test]
    fn canonical_padded_and_unpadded_challenges_are_both_accepted() {
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let unpadded = prepare(Some(b"AQIDBA"), address, [0; 6]).unwrap().unwrap();
        let padded = prepare(Some(b"AQIDBA=="), address, [0; 6])
            .unwrap()
            .unwrap();
        assert_eq!(unpadded.input, padded.input);
    }

    #[test]
    fn malformed_noncanonical_and_oversize_challenges_fail_closed() {
        let provider = RecordingProvider::default();
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for challenge in [
            b"".as_slice(),
            b"AQIDBA=".as_slice(),
            b"AQIDBA===".as_slice(),
            b"AZ".as_slice(),
            b"not/base64!".as_slice(),
            b"A".as_slice(),
        ] {
            assert_eq!(
                prepare(Some(challenge), address, [0; 6]).unwrap_err(),
                AppleChallengeError::MalformedChallenge
            );
        }

        let oversized = STANDARD_NO_PAD.encode([0x55; 17]);
        assert_eq!(
            prepare(Some(oversized.as_bytes()), address, [0; 6]).unwrap_err(),
            AppleChallengeError::MalformedChallenge
        );
        assert!(provider.input.lock().unwrap().is_empty());
    }

    #[test]
    fn provider_output_must_match_the_2048_bit_key_size() {
        struct ShortProvider;
        impl AppleResponseProvider for ShortProvider {
            fn private_pkcs1_v15(&self, _input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
                Ok(vec![0; 255])
            }
        }
        let prepared = prepare(Some(b"AQ"), IpAddr::V4(Ipv4Addr::LOCALHOST), [0; 6])
            .unwrap()
            .unwrap();
        assert_eq!(
            respond(&ShortProvider, prepared).unwrap_err(),
            AppleChallengeError::InvalidProviderResponse
        );
    }
}
