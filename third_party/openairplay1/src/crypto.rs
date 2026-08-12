//! The AirPort Express RSA key and the Apple-Challenge/Apple-Response
//! computation.
//!
//! The private key was extracted from an AirPort Express by James Laird and
//! is shipped by every third-party AirPlay 1 receiver (this copy is taken
//! verbatim from shairport-sync's `common.c`). Clients prove they are
//! talking to a "real" AirPort by sending 16 random bytes in an
//! `Apple-Challenge` header; we must sign challenge+address material with
//! this key and return it in `Apple-Response`.

use std::fmt;
use std::net::IpAddr;
use std::sync::OnceLock;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::{Oaep, Pkcs1v15Sign, RsaPrivateKey};

pub const AIRPORT_PRIVATE_KEY_PEM: &str = include_str!("airport.pem");

pub fn airport_private_key() -> &'static RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| {
        RsaPrivateKey::from_pkcs1_pem(AIRPORT_PRIVATE_KEY_PEM)
            .expect("embedded AirPort RSA key must parse")
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChallengeError {
    InvalidBase64,
    Oversized(usize),
}

impl fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChallengeError::InvalidBase64 => write!(f, "Apple-Challenge is not valid base64"),
            ChallengeError::Oversized(n) => {
                write!(f, "Apple-Challenge decodes to {n} bytes (max 16)")
            }
        }
    }
}

impl std::error::Error for ChallengeError {}

/// Compute the `Apple-Response` value for an `Apple-Challenge`.
///
/// The signed message is: challenge bytes (≤16) ‖ the IP address the client
/// connected to (4 bytes for IPv4, 16 for IPv6, with v4-mapped IPv6
/// contributing its 4 IPv4 bytes) ‖ our 6-byte MAC address, zero-padded to
/// at least 32 bytes. The signature is raw PKCS#1 v1.5 (no digest) with the
/// AirPort key, base64-encoded without padding.
pub fn apple_response(
    challenge_b64: &str,
    local_ip: IpAddr,
    mac: &[u8; 6],
) -> Result<String, ChallengeError> {
    // Clients send the challenge with or without base64 '=' padding.
    let challenge = STANDARD_NO_PAD
        .decode(challenge_b64.trim().trim_end_matches('='))
        .map_err(|_| ChallengeError::InvalidBase64)?;
    if challenge.len() > 16 {
        return Err(ChallengeError::Oversized(challenge.len()));
    }

    let mut buf = Vec::with_capacity(48);
    buf.extend_from_slice(&challenge);
    match local_ip {
        IpAddr::V4(a) => buf.extend_from_slice(&a.octets()),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => buf.extend_from_slice(&v4.octets()),
            None => buf.extend_from_slice(&a.octets()),
        },
    }
    buf.extend_from_slice(mac);
    if buf.len() < 32 {
        buf.resize(32, 0);
    }

    let signature = airport_private_key()
        .sign(Pkcs1v15Sign::new_unprefixed(), &buf)
        .expect("RSA signing of a 32..48 byte message cannot fail");
    Ok(STANDARD_NO_PAD.encode(signature))
}

/// Decrypt the AES session key from a SDP `a=rsaaeskey` value.
///
/// The client RSA-OAEP-encrypts a 16-byte AES key with the AirPort *public*
/// key; we recover it with the private key. OAEP uses SHA-1 for both the
/// hash and MGF1, matching shairport-sync's `RSA_MODE_KEY`.
pub fn decrypt_aes_key(ciphertext: &[u8]) -> Result<[u8; 16], KeyError> {
    let plaintext = airport_private_key()
        .decrypt(Oaep::new::<sha1::Sha1>(), ciphertext)
        .map_err(|_| KeyError::Decrypt)?;
    plaintext
        .as_slice()
        .try_into()
        .map_err(|_| KeyError::WrongLength(plaintext.len()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    Decrypt,
    WrongLength(usize),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Decrypt => write!(f, "rsaaeskey failed RSA-OAEP decryption"),
            KeyError::WrongLength(n) => write!(f, "decrypted AES key is {n} bytes, wanted 16"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Lowercase hex of the MD5 digest of the concatenated `parts`.
///
/// This is the primitive of the classic AirPlay 1 password: the RAOP
/// `Authorization: Digest` response is nested MD5 (HA1/HA2, below), exactly
/// as shairport-sync's `rtsp_classic_airplay_auth` computes it.
fn md5_hex(parts: &[&[u8]]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compute the RFC 2617 `Authorization: Digest` response for the classic
/// AirPlay 1 password, mirroring shairport-sync:
///
/// ```text
/// HA1      = MD5(username ":" realm ":" password)
/// HA2      = MD5(method ":" uri)
/// response = MD5(hex(HA1) ":" nonce ":" hex(HA2))
/// ```
pub fn digest_response(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
) -> String {
    let ha1 = md5_hex(&[
        username.as_bytes(),
        b":",
        realm.as_bytes(),
        b":",
        password.as_bytes(),
    ]);
    let ha2 = md5_hex(&[method.as_bytes(), b":", uri.as_bytes()]);
    md5_hex(&[ha1.as_bytes(), b":", nonce.as_bytes(), b":", ha2.as_bytes()])
}

/// A fixed-length, non-short-circuiting comparison for the digest `response`,
/// so a wrong password cannot leak byte position through timing. Both sides
/// are the 32-char lowercase hex of a 16-byte digest, so the length guard
/// carries no secret-dependent information.
pub fn ct_eq_hex(expected: &str, provided: &str) -> bool {
    let e = expected.as_bytes();
    let p = provided.as_bytes();
    if e.len() != p.len() {
        return false;
    }
    let mut acc = 0u8;
    for i in 0..e.len() {
        acc |= e[i] ^ p[i];
    }
    acc == 0
}

/// A fresh, unpredictable per-connection nonce for the `WWW-Authenticate`
/// challenge (8 random bytes, base64), shaped like shairport-sync's.
pub fn make_nonce() -> String {
    let mut random = [0u8; 8];
    getrandom::getrandom(&mut random).expect("getrandom cannot fail");
    STANDARD.encode(random)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const MAC: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

    fn verify(challenge: &[u8], ip_bytes: &[u8], response_b64: &str) {
        let mut buf = Vec::new();
        buf.extend_from_slice(challenge);
        buf.extend_from_slice(ip_bytes);
        buf.extend_from_slice(&MAC);
        if buf.len() < 32 {
            buf.resize(32, 0);
        }
        let signature = STANDARD_NO_PAD.decode(response_b64).unwrap();
        airport_private_key()
            .to_public_key()
            .verify(Pkcs1v15Sign::new_unprefixed(), &buf, &signature)
            .expect("Apple-Response signature must verify");
    }

    #[test]
    fn signs_ipv4_challenge() {
        let challenge = [7u8; 16];
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let resp = apple_response(&STANDARD.encode(challenge), ip, &MAC).unwrap();
        assert!(
            !resp.contains('='),
            "response must not carry base64 padding"
        );
        verify(&challenge, &[192, 168, 1, 10], &resp);
    }

    #[test]
    fn accepts_unpadded_challenge() {
        let challenge = [1u8; 16];
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let padded = STANDARD.encode(challenge);
        let unpadded = padded.trim_end_matches('=').to_string();
        assert_eq!(
            apple_response(&padded, ip, &MAC).unwrap(),
            apple_response(&unpadded, ip, &MAC).unwrap()
        );
    }

    #[test]
    fn short_challenge_is_zero_padded_to_32() {
        let challenge = [9u8; 4];
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let resp = apple_response(&STANDARD.encode(challenge), ip, &MAC).unwrap();
        verify(&challenge, &[10, 0, 0, 1], &resp);
    }

    #[test]
    fn v4_mapped_ipv6_uses_ipv4_bytes() {
        let challenge = [3u8; 16];
        let mapped = IpAddr::V6(Ipv4Addr::new(172, 16, 0, 2).to_ipv6_mapped());
        let plain = IpAddr::V4(Ipv4Addr::new(172, 16, 0, 2));
        let b64 = STANDARD.encode(challenge);
        assert_eq!(
            apple_response(&b64, mapped, &MAC).unwrap(),
            apple_response(&b64, plain, &MAC).unwrap()
        );
    }

    #[test]
    fn real_ipv6_uses_all_16_bytes() {
        let challenge = [5u8; 16];
        let ip6 = Ipv6Addr::new(0xfe80, 0, 0, 0, 0x1234, 0x5678, 0x9abc, 0xdef0);
        let resp = apple_response(&STANDARD.encode(challenge), IpAddr::V6(ip6), &MAC).unwrap();
        verify(&challenge, &ip6.octets(), &resp);
    }

    #[test]
    fn rejects_oversized_challenge() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let err = apple_response(&STANDARD.encode([0u8; 17]), ip, &MAC).unwrap_err();
        assert_eq!(err, ChallengeError::Oversized(17));
    }

    #[test]
    fn rejects_bad_base64() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let err = apple_response("!!!not-base64!!!", ip, &MAC).unwrap_err();
        assert_eq!(err, ChallengeError::InvalidBase64);
    }

    #[test]
    fn decrypts_oaep_wrapped_aes_key() {
        let key: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        // A real client encrypts with the AirPort *public* key; do the same.
        let public = airport_private_key().to_public_key();
        let mut rng = rand::thread_rng();
        let ciphertext = public
            .encrypt(&mut rng, Oaep::new::<sha1::Sha1>(), &key)
            .unwrap();
        assert_eq!(decrypt_aes_key(&ciphertext).unwrap(), key);
    }

    #[test]
    fn rejects_garbage_key_ciphertext() {
        // Random bytes of the right RSA size decrypt-fail under OAEP.
        let bogus = vec![0x42u8; 256];
        assert_eq!(decrypt_aes_key(&bogus).unwrap_err(), KeyError::Decrypt);
    }

    #[test]
    fn md5_hex_matches_known_vectors() {
        assert_eq!(md5_hex(&[]), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(&[b"abc"]), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn digest_response_matches_reference() {
        // Independent vector, computed by hand with Python's hashlib (not the
        // md5 crate): HA1 = MD5(dev:raop:1234), HA2 = MD5(SETUP:rtsp://…),
        // response = MD5(HA1:nonce:HA2).
        let got = digest_response(
            "dev",
            "raop",
            "1234",
            "SETUP",
            "rtsp://127.0.0.1/1",
            "NjM4MDAwMDAx",
        );
        assert_eq!(got, "556e89d3c27fc12841e76d2dc2d67dc4");
    }

    #[test]
    fn ct_eq_hex_matches_and_differs() {
        let a = "556e89d3c27fc12841e76d2dc2d67dc4";
        let b = "556e89d3c27fc12841e76d2dc2d67dc5";
        assert!(ct_eq_hex(a, a));
        assert!(!ct_eq_hex(a, b));
        assert!(!ct_eq_hex(a, "deadbeef")); // wrong length
    }
}
