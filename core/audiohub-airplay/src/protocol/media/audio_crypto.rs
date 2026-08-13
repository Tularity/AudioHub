//! ChaCha20-Poly1305 protection used by AirPlay 2 realtime audio packets.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use std::error::Error;
use std::fmt;
use zeroize::Zeroize;

pub(crate) const RTP_HEADER_BYTES: usize = 12;
pub(crate) const AUTH_TAG_BYTES: usize = 16;
pub(crate) const TRANSMITTED_NONCE_BYTES: usize = 8;
pub(crate) const MAX_ENCRYPTED_AUDIO_PACKET_BYTES: usize = 4096;

const MIN_PACKET_BYTES: usize = RTP_HEADER_BYTES + 1 + AUTH_TAG_BYTES + TRANSMITTED_NONCE_BYTES;

/// A decrypted packet whose fixed RTP header remains separate from its
/// compressed audio payload.
#[derive(PartialEq, Eq)]
pub(crate) struct DecryptedAudioPacket {
    pub(crate) rtp_header: [u8; RTP_HEADER_BYTES],
    pub(crate) payload: Vec<u8>,
}

impl fmt::Debug for DecryptedAudioPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecryptedAudioPacket")
            .field("rtp_header", &self.rtp_header)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

/// Stateless per-packet decryptor. The key is redacted and erased on drop.
pub(crate) struct AudioDecryptor {
    key: [u8; 32],
}

impl AudioDecryptor {
    pub(crate) fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Authenticate and decrypt one type-96 audio datagram.
    ///
    /// Wire format is a fixed 12-byte RTP header, ciphertext plus a 16-byte
    /// tag, and an 8-byte transmitted nonce. Per the protocol, only RTP bytes
    /// 4 through 11 are associated data. The AEAD nonce is four zero bytes
    /// followed by the transmitted 8-byte suffix.
    pub(crate) fn decrypt(&self, packet: &[u8]) -> Result<DecryptedAudioPacket, AudioCryptoError> {
        if packet.len() < MIN_PACKET_BYTES {
            return Err(AudioCryptoError::PacketTooShort {
                actual: packet.len(),
                minimum: MIN_PACKET_BYTES,
            });
        }
        if packet.len() > MAX_ENCRYPTED_AUDIO_PACKET_BYTES {
            return Err(AudioCryptoError::PacketTooLarge {
                actual: packet.len(),
                maximum: MAX_ENCRYPTED_AUDIO_PACKET_BYTES,
            });
        }

        let rtp_header: [u8; RTP_HEADER_BYTES] = packet[..RTP_HEADER_BYTES]
            .try_into()
            .expect("length checked above");
        // This transport profile has a fixed RTP header: V=2, no padding,
        // extension, or CSRC entries, and dynamic payload type 96.
        if rtp_header[0] != 0x80 || rtp_header[1] & 0x7f != 96 {
            return Err(AudioCryptoError::UnsupportedRtpHeader);
        }

        let nonce_start = packet.len() - TRANSMITTED_NONCE_BYTES;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&packet[nonce_start..]);

        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let payload = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &packet[RTP_HEADER_BYTES..nonce_start],
                    aad: &packet[4..RTP_HEADER_BYTES],
                },
            )
            .map_err(|_| AudioCryptoError::AuthenticationFailed)?;

        Ok(DecryptedAudioPacket {
            rtp_header,
            payload,
        })
    }
}

impl fmt::Debug for AudioDecryptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioDecryptor")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Drop for AudioDecryptor {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AudioCryptoError {
    PacketTooShort { actual: usize, minimum: usize },
    PacketTooLarge { actual: usize, maximum: usize },
    UnsupportedRtpHeader,
    AuthenticationFailed,
}

impl fmt::Display for AudioCryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PacketTooShort { actual, minimum } => {
                write!(f, "audio packet is {actual} bytes; minimum is {minimum}")
            }
            Self::PacketTooLarge { actual, maximum } => {
                write!(f, "audio packet is {actual} bytes; maximum is {maximum}")
            }
            Self::UnsupportedRtpHeader => {
                f.write_str("audio packet does not have the supported fixed RTP header")
            }
            Self::AuthenticationFailed => f.write_str("audio packet authentication failed"),
        }
    }
}

impl Error for AudioCryptoError {}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    fn fixed_packet() -> Vec<u8> {
        // Generated independently with Node/OpenSSL's chacha20-poly1305:
        // key 00..1f, nonce 000000001021324354657687,
        // AAD 01020304a1a2a3a4, and the plaintext asserted below.
        hex::decode(concat!(
            "80e0123401020304a1a2a3a4",
            "d36b0b8d793b232a3822c35460b5a983fefc75ebd373204c",
            "04daf39bdc808fa9556d6616f106165d",
            "1021324354657687"
        ))
        .unwrap()
    }

    #[test]
    fn decrypts_independent_fixed_vector() {
        let clear = AudioDecryptor::new(KEY).decrypt(&fixed_packet()).unwrap();
        assert_eq!(
            clear.rtp_header,
            hex::decode("80e0123401020304a1a2a3a4").unwrap().as_slice()
        );
        assert_eq!(
            clear.payload,
            hex::decode("00112233445566778899aabbccddeeff1020304050607080").unwrap()
        );
    }

    #[test]
    fn authenticates_only_rtp_bytes_four_through_eleven() {
        let decryptor = AudioDecryptor::new(KEY);

        let mut marker_changed = fixed_packet();
        marker_changed[1] ^= 0x80;
        assert!(decryptor.decrypt(&marker_changed).is_ok());

        let mut timestamp_changed = fixed_packet();
        timestamp_changed[4] ^= 1;
        assert_eq!(
            decryptor.decrypt(&timestamp_changed).unwrap_err(),
            AudioCryptoError::AuthenticationFailed
        );
    }

    #[test]
    fn rejects_tampered_ciphertext_tag_and_nonce() {
        let decryptor = AudioDecryptor::new(KEY);
        for index in [
            RTP_HEADER_BYTES,
            fixed_packet().len() - 9,
            fixed_packet().len() - 1,
        ] {
            let mut packet = fixed_packet();
            packet[index] ^= 1;
            assert_eq!(
                decryptor.decrypt(&packet).unwrap_err(),
                AudioCryptoError::AuthenticationFailed
            );
        }
    }

    #[test]
    fn enforces_packet_bounds_and_fixed_rtp_profile() {
        let decryptor = AudioDecryptor::new(KEY);
        assert_eq!(
            decryptor
                .decrypt(&vec![0; MIN_PACKET_BYTES - 1])
                .unwrap_err(),
            AudioCryptoError::PacketTooShort {
                actual: MIN_PACKET_BYTES - 1,
                minimum: MIN_PACKET_BYTES,
            }
        );
        assert_eq!(
            decryptor
                .decrypt(&vec![0; MAX_ENCRYPTED_AUDIO_PACKET_BYTES + 1])
                .unwrap_err(),
            AudioCryptoError::PacketTooLarge {
                actual: MAX_ENCRYPTED_AUDIO_PACKET_BYTES + 1,
                maximum: MAX_ENCRYPTED_AUDIO_PACKET_BYTES,
            }
        );

        let mut packet = fixed_packet();
        packet[0] = 0x90;
        assert_eq!(
            decryptor.decrypt(&packet).unwrap_err(),
            AudioCryptoError::UnsupportedRtpHeader
        );
        packet = fixed_packet();
        packet[1] = 97;
        assert_eq!(
            decryptor.decrypt(&packet).unwrap_err(),
            AudioCryptoError::UnsupportedRtpHeader
        );
    }

    #[test]
    fn debug_output_redacts_key() {
        let debug = format!("{:?}", AudioDecryptor::new(KEY));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("00010203"));
    }

    #[test]
    fn decrypted_packet_debug_does_not_expose_payload() {
        let packet = DecryptedAudioPacket {
            rtp_header: [0x80; RTP_HEADER_BYTES],
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let debug = format!("{packet:?}");
        assert!(debug.contains("payload_len: 4"));
        assert!(!debug.contains("222"));
        assert!(!debug.contains("173"));
        assert!(!debug.contains("190"));
        assert!(!debug.contains("239"));
    }
}
