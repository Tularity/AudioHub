//! AirPlay 2 encrypted control and event channel primitives.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::Sha512;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use zeroize::Zeroize;

/// Maximum plaintext carried by one encrypted HomeKit transport frame.
pub const FRAME_PLAINTEXT_MAX: usize = 1024;

const LENGTH_BYTES: usize = 2;
const TAG_BYTES: usize = 16;
const FRAME_WIRE_MAX: usize = LENGTH_BYTES + FRAME_PLAINTEXT_MAX + TAG_BYTES;

/// Receiver-side key directions.
///
/// `inbound` decrypts bytes written by the sender; `outbound` encrypts bytes
/// read by the sender. Key material is redacted from `Debug` and erased when
/// dropped.
pub struct ChannelKeys {
    inbound: [u8; 32],
    outbound: [u8; 32],
}

impl ChannelKeys {
    pub fn inbound(&self) -> &[u8; 32] {
        &self.inbound
    }

    pub fn outbound(&self) -> &[u8; 32] {
        &self.outbound
    }

    /// Consume the keys into independent per-direction framing state.
    pub fn into_codec(mut self) -> EncryptedChannel {
        let inbound = FrameDecoder::new(self.inbound);
        let outbound = FrameEncoder::new(self.outbound);
        self.inbound.zeroize();
        self.outbound.zeroize();
        EncryptedChannel { inbound, outbound }
    }
}

impl fmt::Debug for ChannelKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelKeys")
            .field("inbound", &"<redacted>")
            .field("outbound", &"<redacted>")
            .finish()
    }
}

impl Drop for ChannelKeys {
    fn drop(&mut self) {
        self.inbound.zeroize();
        self.outbound.zeroize();
    }
}

/// Receiver-side control-channel keys.
///
/// The sender writes with `Control-Write-Encryption-Key` and reads with
/// `Control-Read-Encryption-Key`, hence the apparent direction swap here.
pub fn derive_control_keys(shared_secret: &[u8]) -> Result<ChannelKeys, CryptoError> {
    derive_channel_keys(
        shared_secret,
        b"Control-Salt",
        b"Control-Write-Encryption-Key",
        b"Control-Read-Encryption-Key",
    )
}

/// Receiver-side event-channel keys.
pub fn derive_event_keys(shared_secret: &[u8]) -> Result<ChannelKeys, CryptoError> {
    // Event labels describe the receiver endpoint: the receiver writes with
    // Events-Write, while the sender writes with Events-Read.
    derive_channel_keys(
        shared_secret,
        b"Events-Salt",
        b"Events-Read-Encryption-Key",
        b"Events-Write-Encryption-Key",
    )
}

fn derive_channel_keys(
    shared_secret: &[u8],
    salt: &[u8],
    peer_write_info: &[u8],
    peer_read_info: &[u8],
) -> Result<ChannelKeys, CryptoError> {
    if shared_secret.is_empty() {
        return Err(CryptoError::EmptySharedSecret);
    }
    let hkdf = Hkdf::<Sha512>::new(Some(salt), shared_secret);
    let mut inbound = [0u8; 32];
    let mut outbound = [0u8; 32];
    hkdf.expand(peer_write_info, &mut inbound)
        .map_err(|_| CryptoError::KeyDerivation)?;
    hkdf.expand(peer_read_info, &mut outbound)
        .map_err(|_| CryptoError::KeyDerivation)?;
    Ok(ChannelKeys { inbound, outbound })
}

/// One encrypted bidirectional stream, with independent nonce counters.
pub struct EncryptedChannel {
    pub inbound: FrameDecoder,
    pub outbound: FrameEncoder,
}

impl fmt::Debug for EncryptedChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedChannel")
            .field("inbound", &self.inbound)
            .field("outbound", &self.outbound)
            .finish()
    }
}

/// Errors from key derivation or encrypted transport framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    EmptySharedSecret,
    KeyDerivation,
    EmptyPlaintext,
    CounterExhausted,
    FrameTooLarge { declared: usize },
    AuthenticationFailed,
    PoisonedDecoder,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySharedSecret => f.write_str("empty encryption shared secret"),
            Self::KeyDerivation => f.write_str("channel key derivation failed"),
            Self::EmptyPlaintext => f.write_str("cannot encode an empty encrypted message"),
            Self::CounterExhausted => f.write_str("encrypted channel nonce counter exhausted"),
            Self::FrameTooLarge { declared } => {
                write!(f, "encrypted frame declares {declared} plaintext bytes")
            }
            Self::AuthenticationFailed => f.write_str("encrypted frame authentication failed"),
            Self::PoisonedDecoder => f.write_str("encrypted frame decoder is poisoned"),
        }
    }
}

impl Error for CryptoError {}

/// Stateful outbound HomeKit frame encoder.
pub struct FrameEncoder {
    key: [u8; 32],
    counter: u64,
}

impl FrameEncoder {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key, counter: 0 }
    }

    /// Encode a logical message into one or more independently authenticated
    /// frames. An empty message is rejected because it has no wire framing.
    pub fn encode(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if plaintext.is_empty() {
            return Err(CryptoError::EmptyPlaintext);
        }
        let frame_count = plaintext.len().div_ceil(FRAME_PLAINTEXT_MAX);
        if (frame_count as u128) > (u64::MAX as u128 - self.counter as u128) {
            return Err(CryptoError::CounterExhausted);
        }

        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let mut encoded = Vec::with_capacity(
            plaintext
                .len()
                .saturating_add(frame_count * (LENGTH_BYTES + TAG_BYTES)),
        );
        for chunk in plaintext.chunks(FRAME_PLAINTEXT_MAX) {
            let aad = (chunk.len() as u16).to_le_bytes();
            let encrypted = cipher
                .encrypt(
                    &nonce(self.counter),
                    Payload {
                        msg: chunk,
                        aad: &aad,
                    },
                )
                .map_err(|_| CryptoError::AuthenticationFailed)?;
            encoded.extend_from_slice(&aad);
            encoded.extend_from_slice(&encrypted);
            self.counter += 1;
        }
        Ok(encoded)
    }
}

impl fmt::Debug for FrameEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncoder")
            .field("key", &"<redacted>")
            .field("counter", &self.counter)
            .finish()
    }
}

impl Drop for FrameEncoder {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

/// Incremental inbound HomeKit frame decoder.
///
/// `feed` accepts arbitrary TCP chunk boundaries. Complete frames are returned
/// immediately; a trailing partial frame remains bounded in the decoder. Any
/// malformed/authentication-failing frame poisons the decoder because its
/// nonce position can no longer be trusted.
pub struct FrameDecoder {
    key: [u8; 32],
    counter: u64,
    buffered: VecDeque<u8>,
    poisoned: bool,
}

impl FrameDecoder {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            counter: 0,
            buffered: VecDeque::with_capacity(FRAME_WIRE_MAX),
            poisoned: false,
        }
    }

    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }

    /// Feed one TCP chunk and return all plaintext from complete frames.
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if self.poisoned {
            return Err(CryptoError::PoisonedDecoder);
        }
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let mut plaintext = Vec::new();
        let mut remaining = input;
        loop {
            if self.buffered.len() < LENGTH_BYTES {
                let take = (LENGTH_BYTES - self.buffered.len()).min(remaining.len());
                self.buffered.extend(remaining[..take].iter().copied());
                remaining = &remaining[take..];
                if self.buffered.len() < LENGTH_BYTES {
                    break;
                }
            }
            let declared = self.validate_declared_length()?;
            let wire_len = LENGTH_BYTES + declared + TAG_BYTES;
            if self.buffered.len() < wire_len {
                let take = (wire_len - self.buffered.len()).min(remaining.len());
                self.buffered.extend(remaining[..take].iter().copied());
                remaining = &remaining[take..];
                if self.buffered.len() < wire_len {
                    // A valid incomplete frame cannot retain more than 1042 bytes.
                    break;
                }
            }
            if self.counter == u64::MAX {
                return self.poison(CryptoError::CounterExhausted);
            }

            let frame: Vec<u8> = self.buffered.drain(..wire_len).collect();
            let aad = &frame[..LENGTH_BYTES];
            let clear = cipher
                .decrypt(
                    &nonce(self.counter),
                    Payload {
                        msg: &frame[LENGTH_BYTES..],
                        aad,
                    },
                )
                .map_err(|_| CryptoError::AuthenticationFailed);
            let clear = match clear {
                Ok(clear) => clear,
                Err(error) => return self.poison(error),
            };
            plaintext.extend_from_slice(&clear);
            self.counter += 1;
            if remaining.is_empty() {
                break;
            }
        }
        Ok(plaintext)
    }

    fn validate_declared_length(&mut self) -> Result<usize, CryptoError> {
        let declared = u16::from_le_bytes([self.buffered[0], self.buffered[1]]) as usize;
        if declared > FRAME_PLAINTEXT_MAX {
            return self.poison(CryptoError::FrameTooLarge { declared });
        }
        Ok(declared)
    }

    fn poison<T>(&mut self, error: CryptoError) -> Result<T, CryptoError> {
        self.poisoned = true;
        self.buffered.clear();
        Err(error)
    }
}

impl fmt::Debug for FrameDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("key", &"<redacted>")
            .field("counter", &self.counter)
            .field("buffered_bytes", &self.buffered.len())
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl Drop for FrameDecoder {
    fn drop(&mut self) {
        self.key.zeroize();
        self.buffered.clear();
    }
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn hkdf_matches_independent_sha512_vector() {
        // Generated independently from RFC 5869 extract/expand equations.
        let secret: Vec<u8> = (0..32).collect();
        let control = derive_control_keys(&secret).unwrap();
        assert_eq!(
            control.inbound().as_slice(),
            decode_hex("c3ca130c7033dbe5e7ff7f91d117ead869bac476994c7a48ca170c111136ed96")
        );
        assert_eq!(
            control.outbound().as_slice(),
            decode_hex("c09403ef8aa6c5045cbd8cf9bf3e665b2caed623af2be0e87c8f80f519914d3d")
        );

        let events = derive_event_keys(&secret).unwrap();
        assert_eq!(
            events.inbound().as_slice(),
            decode_hex("77c905af437e45acea18c03d6d5b7791fa9b9da65f3d4d63de8ce7ab3ea3fa5a")
        );
        assert_eq!(
            events.outbound().as_slice(),
            decode_hex("24ee7bb1f775cc08cc23e25bee48afa439250299bc76549e80c0b54d9cc60b61")
        );
    }

    #[test]
    fn framing_matches_openssl_chacha20_poly1305_vector() {
        // Independently generated with OpenSSL EVP_chacha20_poly1305 using
        // key 00..1f, nonce 0, AAD 09 00 and plaintext "AirPlay 2".
        let wire = FrameEncoder::new(core::array::from_fn(|i| i as u8))
            .encode(b"AirPlay 2")
            .unwrap();
        assert_eq!(
            wire,
            decode_hex("090059d13061c187dff1219a99f4f60cf769657aca25f0ba7f8c99")
        );
    }

    #[test]
    fn arbitrary_tcp_splits_decode_one_message() {
        let key = [0x4a; 32];
        let input: Vec<u8> = (0..2500).map(|value| value as u8).collect();
        let wire = FrameEncoder::new(key).encode(&input).unwrap();
        let mut decoder = FrameDecoder::new(key);
        let mut output = Vec::new();
        for chunk in wire.chunks(37) {
            output.extend(decoder.feed(chunk).unwrap());
            assert!(decoder.buffered_len() <= FRAME_WIRE_MAX);
        }
        assert_eq!(output, input);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn directions_use_independent_keys_and_counters() {
        let secret = [0x77; 32];
        let receiver = derive_control_keys(&secret).unwrap();
        let mut receiver_in = FrameDecoder::new(*receiver.inbound());
        let mut receiver_out = FrameEncoder::new(*receiver.outbound());
        let mut sender_out = FrameEncoder::new(*receiver.inbound());
        let mut sender_in = FrameDecoder::new(*receiver.outbound());

        assert_eq!(
            receiver_in
                .feed(&sender_out.encode(b"request").unwrap())
                .unwrap(),
            b"request"
        );
        assert_eq!(
            sender_in
                .feed(&receiver_out.encode(b"response").unwrap())
                .unwrap(),
            b"response"
        );
    }

    #[test]
    fn truncated_frame_waits_without_advancing_nonce() {
        let key = [0x19; 32];
        let wire = FrameEncoder::new(key).encode(b"incremental").unwrap();
        let mut decoder = FrameDecoder::new(key);
        assert_eq!(decoder.feed(&wire[..wire.len() - 1]).unwrap(), b"");
        assert_eq!(decoder.buffered_len(), wire.len() - 1);
        assert_eq!(
            decoder.feed(&wire[wire.len() - 1..]).unwrap(),
            b"incremental"
        );
    }

    #[test]
    fn tampering_is_fatal_and_cannot_retry_nonce() {
        let key = [0xa5; 32];
        let mut wire = FrameEncoder::new(key).encode(b"authenticated").unwrap();
        *wire.last_mut().unwrap() ^= 1;
        let mut decoder = FrameDecoder::new(key);
        assert_eq!(decoder.feed(&wire), Err(CryptoError::AuthenticationFailed));
        assert_eq!(decoder.feed(&wire), Err(CryptoError::PoisonedDecoder));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn oversize_header_is_rejected_before_buffering_body() {
        let mut decoder = FrameDecoder::new([0; 32]);
        assert_eq!(
            decoder.feed(&(1025u16).to_le_bytes()),
            Err(CryptoError::FrameTooLarge { declared: 1025 })
        );
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn debug_never_formats_key_bytes() {
        let keys = derive_control_keys(&[0x11; 32]).unwrap();
        let text = format!("{keys:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("17, 17"));
    }
}
