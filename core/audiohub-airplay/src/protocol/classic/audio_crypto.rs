//! Classic RAOP per-packet AES-128-CBC audio decryption.

use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use std::error::Error;
use std::fmt;
use zeroize::Zeroizing;

type Aes128CbcDecryptor = cbc::Decryptor<aes::Aes128>;

pub(crate) const MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES: usize = 4_096;

/// Decrypts complete CBC blocks and preserves RAOP's protocol-defined clear
/// tail. A fresh cipher is constructed for every call, so the SDP IV is reset
/// for every RTP packet rather than chained across packets.
pub(crate) fn decrypt_audio_payload(
    payload: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Zeroizing<Vec<u8>>, AudioCryptoError> {
    if payload.is_empty() {
        return Err(AudioCryptoError::EmptyPayload);
    }
    if payload.len() > MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES {
        return Err(AudioCryptoError::PayloadTooLarge {
            actual: payload.len(),
            maximum: MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES,
        });
    }

    let encrypted_bytes = payload.len() & !0x0f;
    let mut plaintext = Zeroizing::new(payload.to_vec());
    if encrypted_bytes != 0 {
        let cipher = Aes128CbcDecryptor::new_from_slices(key, iv)
            .map_err(|_| AudioCryptoError::InvalidKeyOrIv)?;
        cipher
            .decrypt_padded_mut::<NoPadding>(&mut plaintext[..encrypted_bytes])
            .map_err(|_| AudioCryptoError::InvalidCiphertext)?;
    }
    Ok(plaintext)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AudioCryptoError {
    EmptyPayload,
    PayloadTooLarge { actual: usize, maximum: usize },
    InvalidKeyOrIv,
    InvalidCiphertext,
}

impl fmt::Display for AudioCryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPayload => f.write_str("classic audio payload is empty"),
            Self::PayloadTooLarge { actual, maximum } => write!(
                f,
                "classic audio payload is {actual} bytes; maximum is {maximum}"
            ),
            Self::InvalidKeyOrIv => f.write_str("classic AES key or IV is invalid"),
            Self::InvalidCiphertext => f.write_str("classic AES ciphertext is invalid"),
        }
    }
}

impl Error for AudioCryptoError {}

#[cfg(test)]
mod tests {
    use super::*;

    // NIST SP 800-38A F.2.2, first AES-128-CBC block.
    const KEY: [u8; 16] = [
        0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f,
        0x3c,
    ];
    const IV: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const CIPHERTEXT: [u8; 16] = [
        0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9, 0x19,
        0x7d,
    ];
    const PLAINTEXT: [u8; 16] = [
        0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17,
        0x2a,
    ];

    #[test]
    fn decrypts_nist_vector_preserves_clear_tail_and_resets_per_packet() {
        let mut packet = CIPHERTEXT.to_vec();
        packet.extend_from_slice(&[0xa5, 0x5a, 0x11, 0x22, 0x33]);

        let first = decrypt_audio_payload(&packet, &KEY, &IV).unwrap();
        let second = decrypt_audio_payload(&packet, &KEY, &IV).unwrap();
        assert_eq!(&first[..16], &PLAINTEXT);
        assert_eq!(&first[16..], &[0xa5, 0x5a, 0x11, 0x22, 0x33]);
        assert_eq!(&*first, &*second);
    }

    #[test]
    fn rejects_empty_and_oversized_payloads() {
        assert_eq!(
            decrypt_audio_payload(&[], &KEY, &IV).unwrap_err(),
            AudioCryptoError::EmptyPayload
        );
        assert!(matches!(
            decrypt_audio_payload(&vec![0; MAX_ENCRYPTED_AUDIO_PAYLOAD_BYTES + 1], &KEY, &IV),
            Err(AudioCryptoError::PayloadTooLarge { .. })
        ));
    }
}
