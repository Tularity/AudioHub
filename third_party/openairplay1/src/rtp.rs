//! RAOP UDP packet parsing and per-packet AES-CBC audio decryption.
//!
//! Milestone 2 only needs the audio channel decoded far enough to prove the
//! session key/IV are right; control and timing packets are parsed just
//! enough to classify and log them.

use aes::cipher::{BlockDecryptMut, KeyIvInit};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// RTP payload type (low 7 bits of byte 1) for realtime audio data.
pub const PT_AUDIO: u8 = 0x60;
/// RTP payload type for a retransmitted ("resend") audio packet. The real
/// audio packet is wrapped behind a 4-byte header.
pub const PT_RESEND: u8 = 0x56;

/// A parsed audio packet: header fields plus the still-encrypted payload
/// slice within the original datagram.
#[derive(Debug, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub sequence: u16,
    pub timestamp: u32,
    pub is_resend: bool,
    pub payload: &'a [u8],
}

impl<'a> AudioPacket<'a> {
    /// Parse a datagram from the audio socket. Returns `None` if it isn't an
    /// audio/resend packet or is too short to hold a payload.
    pub fn parse(datagram: &'a [u8]) -> Option<AudioPacket<'a>> {
        if datagram.len() < 12 {
            return None;
        }
        let pt = datagram[1] & 0x7f;
        let (frame, is_resend) = match pt {
            PT_AUDIO => (datagram, false),
            // A resend prepends a 4-byte header ahead of the embedded RTP
            // packet; step over it and re-parse the inner header.
            PT_RESEND => {
                let inner = datagram.get(4..)?;
                if inner.len() < 12 {
                    return None;
                }
                (inner, true)
            }
            _ => return None,
        };
        let sequence = u16::from_be_bytes([frame[2], frame[3]]);
        let timestamp = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let payload = &frame[12..];
        if payload.is_empty() {
            return None;
        }
        Some(AudioPacket {
            sequence,
            timestamp,
            is_resend,
            payload,
        })
    }
}

/// Decrypt a RAOP audio payload in place-to-buffer.
///
/// AES-128-CBC over only the whole 16-byte blocks (`len & !0xf`), IV reset to
/// the session `aesiv` for every packet; the trailing `len % 16` bytes are
/// sent in the clear and copied through unchanged. Returns the plaintext
/// ALAC frame (same length as the input payload).
pub fn decrypt_audio(payload: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    let block_len = payload.len() & !0xf;
    let mut out = payload.to_vec();
    if block_len > 0 {
        let mut cipher = Aes128CbcDec::new(key.into(), iv.into());
        // Decrypt whole 16-byte blocks only; unlike the padded variants this
        // never touches or validates a final partial block — exactly the
        // CBC-without-padding scheme RAOP uses.
        for block in out[..block_len].chunks_exact_mut(16) {
            let ga = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
            cipher.decrypt_block_mut(ga);
        }
    }
    out
}

/// Cheap sanity check that a decrypted payload plausibly opens an ALAC
/// channel-pair element. For 44.1 kHz / 16-bit / stereo the first element is
/// a channel pair (element ID 1), so the top three bits of byte 0 are `001`.
/// A wrong key/IV yields random-looking bytes that rarely satisfy this.
pub fn looks_like_alac_stereo(plaintext: &[u8]) -> bool {
    plaintext.first().is_some_and(|b| b >> 5 == 0b001)
}

/// Classification of a non-audio RAOP UDP packet, for logging only.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlKind {
    Sync,
    RetransmitResponse,
    TimingResponse,
    TimingRequest,
    Other(u8),
}

pub fn classify_control(datagram: &[u8]) -> Option<ControlKind> {
    let pt = datagram.get(1)? & 0x7f;
    Some(match pt {
        0x54 => ControlKind::Sync,
        0x56 => ControlKind::RetransmitResponse,
        0x53 => ControlKind::TimingResponse,
        0x52 => ControlKind::TimingRequest,
        other => ControlKind::Other(other),
    })
}

/// Build a RAOP retransmit ("resend") request for `count` packets starting at
/// `first`, to be sent to the client's control port. Layout matches
/// shairport-sync: `80 D5`, our sequence (always 1), the first missing
/// sequence, and the count — all big-endian.
pub fn resend_request(first: u16, count: u16) -> [u8; 8] {
    let mut req = [0u8; 8];
    req[0] = 0x80;
    req[1] = 0x55 | 0x80; // 0xD5, Apple "resend"
    req[2..4].copy_from_slice(&1u16.to_be_bytes());
    req[4..6].copy_from_slice(&first.to_be_bytes());
    req[6..8].copy_from_slice(&count.to_be_bytes());
    req
}

/// Build an NTP timing request (`80 D2`) to send to the client's timing port.
/// All three NTP fields are zero; the client fills in its receive/transmit
/// times in the reply. Record the local send time to pair with the reply.
pub fn timing_request() -> [u8; 32] {
    let mut req = [0u8; 32];
    req[0] = 0x80;
    req[1] = 0x52 | 0x80; // 0xD2, timing request
    req[2..4].copy_from_slice(&7u16.to_be_bytes()); // seqno, shairport uses 7
    req
}

/// The two client-clock instants recovered from a `0xd3` timing reply, in
/// nanoseconds: `receive` (t2) and `transmit` (t3).
#[derive(Debug, PartialEq, Eq)]
pub struct TimingReply {
    pub receive_ns: u64,
    pub transmit_ns: u64,
}

/// Parse a `0xd3` timing reply (32 bytes): NTP receive at `[16..24]` and NTP
/// transmit at `[24..32]`, each a 32.32 seconds/fraction pair.
pub fn parse_timing_reply(datagram: &[u8]) -> Option<TimingReply> {
    if datagram.len() < 32 || datagram[1] & 0x7f != 0x53 {
        return None;
    }
    let ntp = |o: usize| {
        crate::clock::ntp_to_ns(
            u32::from_be_bytes(datagram[o..o + 4].try_into().unwrap()),
            u32::from_be_bytes(datagram[o + 4..o + 8].try_into().unwrap()),
        )
    };
    Some(TimingReply {
        receive_ns: ntp(16),
        transmit_ns: ntp(24),
    })
}

/// A parsed `0xd4` sync packet.
#[derive(Debug, PartialEq, Eq)]
pub struct SyncInfo {
    /// Client-clock instant of the sync, in nanoseconds.
    pub remote_time_ns: u64,
    /// RTP timestamp at the DAC at that instant (current minus latency).
    pub rtp_at_dac: u32,
    /// The "current" RTP timestamp (used to derive latency = current − at_dac).
    pub rtp_current: u32,
}

/// Parse a `0xd4` sync packet (20 bytes): `rtp_at_dac` at `[4..8]`, NTP
/// `remote_time` at `[8..16]`, `rtp_current` at `[16..20]`.
pub fn parse_sync(datagram: &[u8]) -> Option<SyncInfo> {
    if datagram.len() < 20 || datagram[1] & 0x7f != 0x54 {
        return None;
    }
    let rtp_at_dac = u32::from_be_bytes(datagram[4..8].try_into().unwrap());
    let remote_time_ns = crate::clock::ntp_to_ns(
        u32::from_be_bytes(datagram[8..12].try_into().unwrap()),
        u32::from_be_bytes(datagram[12..16].try_into().unwrap()),
    );
    let rtp_current = u32::from_be_bytes(datagram[16..20].try_into().unwrap());
    Some(SyncInfo {
        remote_time_ns,
        rtp_at_dac,
        rtp_current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    const KEY: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const IV: [u8; 16] = [
        0x0f, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
        0x00,
    ];

    fn encrypt(plaintext: &[u8]) -> Vec<u8> {
        let block_len = plaintext.len() & !0xf;
        let mut out = plaintext.to_vec();
        let mut cipher = Aes128CbcEnc::new(&KEY.into(), &IV.into());
        for block in out[..block_len].chunks_exact_mut(16) {
            let ga = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
            cipher.encrypt_block_mut(ga);
        }
        out
    }

    fn audio_header(seq: u16, ts: u32) -> Vec<u8> {
        let mut h = vec![0x80, PT_AUDIO];
        h.extend_from_slice(&seq.to_be_bytes());
        h.extend_from_slice(&ts.to_be_bytes());
        h.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // SSRC
        h
    }

    #[test]
    fn parses_audio_packet() {
        let mut pkt = audio_header(1234, 0x0009_A3F0);
        pkt.extend_from_slice(&[1, 2, 3, 4]);
        let parsed = AudioPacket::parse(&pkt).unwrap();
        assert_eq!(parsed.sequence, 1234);
        assert_eq!(parsed.timestamp, 0x0009_A3F0);
        assert!(!parsed.is_resend);
        assert_eq!(parsed.payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn parses_resend_packet_skipping_four_bytes() {
        let mut inner = audio_header(7, 42);
        inner.extend_from_slice(&[9, 9]);
        let mut pkt = vec![0x80, PT_RESEND, 0x00, 0x01];
        pkt.extend_from_slice(&inner);
        let parsed = AudioPacket::parse(&pkt).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert!(parsed.is_resend);
        assert_eq!(parsed.payload, &[9, 9]);
    }

    #[test]
    fn rejects_non_audio_and_short_packets() {
        assert!(AudioPacket::parse(&[0x80, 0x54, 0, 0]).is_none());
        assert!(AudioPacket::parse(&audio_header(1, 1)).is_none()); // no payload
        assert!(AudioPacket::parse(&[0x80]).is_none());
    }

    #[test]
    fn decrypts_block_aligned_payload() {
        let plaintext: Vec<u8> = (0..32).collect();
        let cipher = encrypt(&plaintext);
        assert_ne!(cipher, plaintext);
        assert_eq!(decrypt_audio(&cipher, &KEY, &IV), plaintext);
    }

    #[test]
    fn decrypts_payload_with_plaintext_tail() {
        // 37 bytes: two encrypted blocks (32) + a 5-byte cleartext tail.
        let plaintext: Vec<u8> = (0..37).collect();
        let cipher = encrypt(&plaintext);
        assert_eq!(
            &cipher[32..],
            &plaintext[32..],
            "tail must stay in the clear"
        );
        assert_eq!(decrypt_audio(&cipher, &KEY, &IV), plaintext);
    }

    #[test]
    fn iv_resets_per_packet() {
        // Same plaintext encrypted twice must decrypt identically, proving
        // the IV isn't carried between calls.
        let plaintext: Vec<u8> = vec![0xAB; 48];
        let cipher = encrypt(&plaintext);
        assert_eq!(decrypt_audio(&cipher, &KEY, &IV), plaintext);
        assert_eq!(decrypt_audio(&cipher, &KEY, &IV), plaintext);
    }

    #[test]
    fn short_payload_is_passed_through() {
        let payload = [1, 2, 3]; // < 16 bytes, no whole block
        assert_eq!(decrypt_audio(&payload, &KEY, &IV), payload);
    }

    #[test]
    fn alac_sanity_check() {
        assert!(looks_like_alac_stereo(&[0b0010_0000, 0, 0]));
        assert!(!looks_like_alac_stereo(&[0b1110_0000]));
        assert!(!looks_like_alac_stereo(&[]));
    }

    #[test]
    fn classifies_control_packets() {
        assert_eq!(classify_control(&[0x80, 0xd4]), Some(ControlKind::Sync));
        assert_eq!(
            classify_control(&[0x80, 0x53]),
            Some(ControlKind::TimingResponse)
        );
        assert_eq!(
            classify_control(&[0x80, 0x11]),
            Some(ControlKind::Other(0x11))
        );
    }

    #[test]
    fn encodes_resend_request() {
        // Request 3 packets starting at seq 0x1234.
        assert_eq!(
            resend_request(0x1234, 3),
            [0x80, 0xD5, 0x00, 0x01, 0x12, 0x34, 0x00, 0x03]
        );
    }

    #[test]
    fn encodes_timing_request() {
        let req = timing_request();
        assert_eq!(req[0], 0x80);
        assert_eq!(req[1], 0xD2);
        assert_eq!(&req[2..4], &7u16.to_be_bytes());
        assert!(req[4..].iter().all(|&b| b == 0), "NTP fields must be zero");
    }

    #[test]
    fn parses_timing_reply() {
        let mut pkt = [0u8; 32];
        pkt[0] = 0x80;
        pkt[1] = 0xD3;
        pkt[16..20].copy_from_slice(&5u32.to_be_bytes()); // receive: 5.5 s
        pkt[20..24].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        pkt[24..28].copy_from_slice(&6u32.to_be_bytes()); // transmit: 6.0 s
        let reply = parse_timing_reply(&pkt).unwrap();
        assert_eq!(reply.receive_ns, 5_500_000_000);
        assert_eq!(reply.transmit_ns, 6_000_000_000);
        assert!(parse_timing_reply(&pkt[..20]).is_none(), "too short");
    }

    #[test]
    fn parses_sync_packet() {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x80;
        pkt[1] = 0xD4;
        pkt[4..8].copy_from_slice(&1000u32.to_be_bytes()); // rtp at DAC
        pkt[8..12].copy_from_slice(&10u32.to_be_bytes()); // remote time: 10 s
        pkt[16..20].copy_from_slice(&12205u32.to_be_bytes()); // rtp current
        let sync = parse_sync(&pkt).unwrap();
        assert_eq!(sync.rtp_at_dac, 1000);
        assert_eq!(sync.remote_time_ns, 10_000_000_000);
        assert_eq!(sync.rtp_current, 12205);
        // latency = current − at_dac = 11205 frames.
        assert_eq!(sync.rtp_current - sync.rtp_at_dac, 11205);
    }

    #[test]
    fn sync_ignores_wrong_type() {
        let pkt = [
            0x80, 0xD3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(parse_sync(&pkt).is_none());
    }
}
