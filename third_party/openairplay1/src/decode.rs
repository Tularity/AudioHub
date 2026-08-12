//! ALAC decoding, wrapping the `alac` crate.
//!
//! RAOP delivers raw ALAC packets (no container); each decodes to a block of
//! interleaved `i16` PCM. The decoder is configured from the SDP `fmtp`
//! parameters captured at ANNOUNCE.

use alac::{Decoder, StreamInfo};

use crate::sdp::AlacConfig;

pub struct AlacDecoder {
    decoder: Decoder,
    channels: usize,
    max_samples: usize,
    scratch: Vec<i16>,
}

#[derive(Debug)]
pub enum DecodeError {
    Config,
    Packet,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Config => write!(f, "invalid ALAC configuration"),
            DecodeError::Packet => write!(f, "malformed ALAC packet"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl AlacDecoder {
    /// Build a decoder from the ANNOUNCE fmtp config. Uses the 11 fields
    /// after the payload type (`raw[1..12]`), which is exactly what the
    /// `alac` crate's `from_sdp_format_parameters` expects.
    pub fn new(config: &AlacConfig) -> Result<AlacDecoder, DecodeError> {
        let params = config.raw[1..12]
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let info =
            StreamInfo::from_sdp_format_parameters(&params).map_err(|_| DecodeError::Config)?;
        let channels = info.channels() as usize;
        let max_samples = info.max_samples_per_packet() as usize;
        Ok(AlacDecoder {
            decoder: Decoder::new(info),
            channels,
            max_samples,
            scratch: vec![0i16; max_samples],
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode one raw ALAC packet into interleaved `i16` PCM. The returned
    /// slice borrows the decoder's scratch buffer until the next call.
    pub fn decode(&mut self, packet: &[u8]) -> Result<&[i16], DecodeError> {
        // Defensive: `decode_packet` asserts the output is large enough, so
        // never let a malformed cookie/packet shrink it.
        if self.scratch.len() < self.max_samples {
            self.scratch.resize(self.max_samples, 0);
        }
        let decoded = self
            .decoder
            .decode_packet(packet, &mut self.scratch)
            .map_err(|_| DecodeError::Packet)?;
        let len = decoded.len();
        Ok(&self.scratch[..len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    fn config_from_fmtp(fmtp: &str) -> AlacConfig {
        // Prepend a payload type so it matches the ANNOUNCE `a=fmtp` shape
        // our AlacConfig parses (12 fields incl. the leading 96).
        AlacConfig::parse(&format!("96 {fmtp}")).unwrap()
    }

    #[test]
    fn builds_decoder_from_raop_fmtp() {
        let config = config_from_fmtp("352 0 16 40 10 14 2 255 0 0 44100");
        let dec = AlacDecoder::new(&config).unwrap();
        assert_eq!(dec.channels(), 2);
        assert_eq!(dec.max_samples, 352 * 2);
    }

    #[test]
    fn decodes_golden_packet_to_expected_pcm() {
        let fmtp = include_str!("testdata/golden_fmtp.txt").trim();
        let packet = STANDARD
            .decode(
                include_str!("testdata/golden_packet.b64")
                    .split_whitespace()
                    .collect::<String>(),
            )
            .unwrap();
        let expected_bytes = STANDARD
            .decode(
                include_str!("testdata/golden_pcm_i16le.b64")
                    .split_whitespace()
                    .collect::<String>(),
            )
            .unwrap();
        let expected: Vec<i16> = expected_bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();

        let config = config_from_fmtp(fmtp);
        let mut dec = AlacDecoder::new(&config).unwrap();
        let pcm = dec.decode(&packet).unwrap();
        assert_eq!(pcm.len(), expected.len());
        assert_eq!(pcm, expected.as_slice());
    }

    #[test]
    fn rejects_garbage_packet() {
        let config = config_from_fmtp("4096 0 16 40 10 14 2 0 16388 1411200 44100");
        let mut dec = AlacDecoder::new(&config).unwrap();
        assert!(dec.decode(&[0xff; 8]).is_err());
    }
}
