//! Strict ANNOUNCE envelope for the supported encrypted stereo ALAC profile.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use std::fmt;
use zeroize::Zeroizing;

pub(crate) const MAX_SDP_BYTES: usize = 4096;
pub(crate) const WRAPPED_KEY_BYTES: usize = 256;
const MAX_LINE_BYTES: usize = 512;
const MAX_LATENCY_FRAMES: u32 = 44_100 * 10;
const ALAC_PROFILE: [u32; 12] = [96, 352, 0, 16, 40, 10, 14, 2, 255, 0, 0, 44100];

pub(crate) struct Announcement {
    pub(crate) wrapped_key: Zeroizing<Vec<u8>>,
    pub(crate) iv: Zeroizing<[u8; 16]>,
    pub(crate) min_latency: Option<u32>,
    pub(crate) max_latency: Option<u32>,
}

impl fmt::Debug for Announcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Announcement")
            .field("key_material", &"<redacted>")
            .field("min_latency", &self.min_latency)
            .field("max_latency", &self.max_latency)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SdpError {
    InvalidEnvelope,
    DuplicateAttribute,
    UnsupportedProfile,
    MissingEncryption,
    InvalidEncryption,
    InvalidLatency,
}

impl fmt::Display for SdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEnvelope => "invalid classic SDP envelope",
            Self::DuplicateAttribute => "duplicate classic SDP attribute",
            Self::UnsupportedProfile => "unsupported classic audio profile",
            Self::MissingEncryption => "classic encrypted audio is required",
            Self::InvalidEncryption => "invalid classic encryption parameters",
            Self::InvalidLatency => "invalid classic latency bounds",
        })
    }
}

impl std::error::Error for SdpError {}

fn set_once<'a>(slot: &mut Option<&'a str>, value: &'a str) -> Result<(), SdpError> {
    if slot.replace(value).is_some() {
        return Err(SdpError::DuplicateAttribute);
    }
    Ok(())
}

fn decode_parameter(value: &str, expected: usize) -> Result<Zeroizing<Vec<u8>>, SdpError> {
    if value.len() > expected.div_ceil(3) * 4 {
        return Err(SdpError::InvalidEncryption);
    }
    let decoded = if value.ends_with('=') {
        STANDARD.decode(value)
    } else {
        STANDARD_NO_PAD.decode(value)
    };
    let decoded = Zeroizing::new(decoded.map_err(|_| SdpError::InvalidEncryption)?);
    if decoded.len() != expected {
        return Err(SdpError::InvalidEncryption);
    }
    Ok(decoded)
}

fn latency(value: Option<&str>) -> Result<Option<u32>, SdpError> {
    value
        .map(|value| {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(SdpError::InvalidLatency);
            }
            let frames = value.parse::<u32>().map_err(|_| SdpError::InvalidLatency)?;
            if frames > MAX_LATENCY_FRAMES {
                return Err(SdpError::InvalidLatency);
            }
            Ok(frames)
        })
        .transpose()
}

impl Announcement {
    pub(crate) fn parse(body: &[u8]) -> Result<Self, SdpError> {
        if body.is_empty() || body.len() > MAX_SDP_BYTES {
            return Err(SdpError::InvalidEnvelope);
        }
        let text = std::str::from_utf8(body).map_err(|_| SdpError::InvalidEnvelope)?;
        let (mut version, mut media, mut rtpmap, mut fmtp) = (None, None, None, None);
        let (mut wrapped_key, mut iv, mut min_latency, mut max_latency) = (None, None, None, None);
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_LINE_BYTES
                || line.bytes().any(|b| b.is_ascii_control() && b != b'\t')
                || !line.as_bytes()[0].is_ascii_lowercase()
                || line.as_bytes().get(1) != Some(&b'=')
            {
                return Err(SdpError::InvalidEnvelope);
            }
            if let Some(value) = line.strip_prefix("v=") {
                set_once(&mut version, value)?;
            } else if let Some(value) = line.strip_prefix("m=") {
                set_once(&mut media, value)?;
            } else if let Some(attribute) = line.strip_prefix("a=") {
                let Some((name, value)) = attribute.split_once(':') else {
                    if matches!(
                        attribute,
                        "rtpmap" | "fmtp" | "rsaaeskey" | "aesiv" | "min-latency" | "max-latency"
                    ) {
                        return Err(SdpError::InvalidEnvelope);
                    }
                    continue;
                };
                let slot = match name {
                    "rtpmap" => &mut rtpmap,
                    "fmtp" => &mut fmtp,
                    "rsaaeskey" => &mut wrapped_key,
                    "aesiv" => &mut iv,
                    "min-latency" => &mut min_latency,
                    "max-latency" => &mut max_latency,
                    _ => continue,
                };
                set_once(slot, value)?;
            }
        }
        if version != Some("0") || media != Some("audio 0 RTP/AVP 96") {
            return Err(SdpError::UnsupportedProfile);
        }
        if rtpmap != Some("96 AppleLossless") {
            return Err(SdpError::UnsupportedProfile);
        }
        let mut tokens = fmtp
            .ok_or(SdpError::UnsupportedProfile)?
            .split_ascii_whitespace();
        for expected in ALAC_PROFILE {
            let token = tokens.next().ok_or(SdpError::UnsupportedProfile)?;
            if !token.bytes().all(|b| b.is_ascii_digit())
                || token.parse::<u32>().ok() != Some(expected)
            {
                return Err(SdpError::UnsupportedProfile);
            }
        }
        if tokens.next().is_some() {
            return Err(SdpError::UnsupportedProfile);
        }
        let min_latency = latency(min_latency)?;
        let max_latency = latency(max_latency)?;
        if matches!((min_latency, max_latency), (Some(min), Some(max)) if min > max) {
            return Err(SdpError::InvalidLatency);
        }
        let wrapped_key = decode_parameter(
            wrapped_key.ok_or(SdpError::MissingEncryption)?,
            WRAPPED_KEY_BYTES,
        )?;
        let iv_bytes = decode_parameter(iv.ok_or(SdpError::MissingEncryption)?, 16)?;
        let iv = Zeroizing::new(
            iv_bytes
                .as_slice()
                .try_into()
                .map_err(|_| SdpError::InvalidEncryption)?,
        );
        Ok(Self {
            wrapped_key,
            iv,
            min_latency,
            max_latency,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(padded: bool) -> String {
        let encode = |data: &[u8]| {
            if padded {
                STANDARD.encode(data)
            } else {
                STANDARD_NO_PAD.encode(data)
            }
        };
        format!(
            "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\na=rsaaeskey:{}\r\na=aesiv:{}\r\na=min-latency:11025\r\na=max-latency:88200\r\n",
            encode(&[0x5a; WRAPPED_KEY_BYTES]), encode(&[0x19; 16]),
        )
    }

    #[test]
    fn accepts_exact_encrypted_profile_with_both_base64_forms() {
        for padded in [false, true] {
            let result = Announcement::parse(body(padded).as_bytes()).unwrap();
            assert_eq!(&*result.wrapped_key, &[0x5a; WRAPPED_KEY_BYTES]);
            assert_eq!(&*result.iv, &[0x19; 16]);
            assert_eq!(result.min_latency, Some(11025));
            assert_eq!(result.max_latency, Some(88200));
        }
    }

    #[test]
    fn rejects_duplicates_instead_of_overwriting_security_fields() {
        for line in [
            "v=0",
            "m=audio 0 RTP/AVP 96",
            "a=rtpmap:96 AppleLossless",
            "a=fmtp:96 352",
            "a=rsaaeskey:AA",
            "a=aesiv:AA",
            "a=min-latency:0",
            "a=max-latency:0",
        ] {
            let input = format!("{}{line}\r\n", body(true));
            assert_eq!(
                Announcement::parse(input.as_bytes()).unwrap_err(),
                SdpError::DuplicateAttribute
            );
        }
    }

    #[test]
    fn refuses_unencrypted_and_partial_key_material() {
        for omit in ["a=rsaaeskey:", "a=aesiv:"] {
            let input = body(true)
                .lines()
                .filter(|line| !line.starts_with(omit))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                Announcement::parse(input.as_bytes()).unwrap_err(),
                SdpError::MissingEncryption
            );
        }
    }

    #[test]
    fn rejects_unsupported_or_truncated_profile_without_integer_narrowing() {
        for (old, new) in [
            ("audio 0 RTP/AVP 96", "video 0 RTP/AVP 96"),
            ("96 AppleLossless", "96 L16/44100/2"),
            (" 16 40 ", " 272 40 "),
            (" 2 255 ", " 258 255 "),
            ("44100", "48000"),
            ("352 0 16", "352 16"),
        ] {
            assert_eq!(
                Announcement::parse(body(true).replace(old, new).as_bytes()).unwrap_err(),
                SdpError::UnsupportedProfile
            );
        }
    }

    #[test]
    fn bounds_latency_and_envelope_before_acceptance() {
        for input in [
            body(true).replace("88200", "441001"),
            body(true).replace("11025", "90000"),
            body(true).replace("11025", "+11025"),
        ] {
            assert_eq!(
                Announcement::parse(input.as_bytes()).unwrap_err(),
                SdpError::InvalidLatency
            );
        }
        for input in [
            vec![b'x'; MAX_SDP_BYTES + 1],
            b"v=0\0\n".to_vec(),
            b"v=0\ra=x\n".to_vec(),
            vec![0xff],
        ] {
            assert_eq!(
                Announcement::parse(&input).unwrap_err(),
                SdpError::InvalidEnvelope
            );
        }
    }

    #[test]
    fn rejects_bad_key_lengths_and_redacts_debug() {
        let input = body(true);
        let key = STANDARD.encode([0x5a; WRAPPED_KEY_BYTES]);
        for value in [
            "!".to_string(),
            STANDARD.encode([1u8; 255]),
            format!("{key}="),
        ] {
            assert_eq!(
                Announcement::parse(input.replace(&key, &value).as_bytes()).unwrap_err(),
                SdpError::InvalidEncryption
            );
        }
        let debug = format!("{:?}", Announcement::parse(input.as_bytes()).unwrap());
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&key));
        assert!(!debug.contains(&STANDARD.encode([0x19; 16])));
    }
}
