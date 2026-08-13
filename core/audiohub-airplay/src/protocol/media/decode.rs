//! Bounded ALAC decoding for the AirPlay 2 realtime-audio profile.

use alac::{Decoder, StreamInfo};
use std::error::Error;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub(crate) const FRAMES_PER_PACKET: usize = 352;
pub(crate) const CHANNELS: usize = 2;
pub(crate) const SAMPLE_RATE: u32 = 44_100;
pub(crate) const MAX_SAMPLES_PER_PACKET: usize = FRAMES_PER_PACKET * CHANNELS;
/// The encrypted UDP transport has a smaller effective cleartext limit, but
/// keeping the decoder's own boundary explicit makes it safe to call alone.
pub(crate) const MAX_COMPRESSED_PACKET_BYTES: usize = 4_096;

const AIRPLAY_TYPE96_ALAC_FMTP: &str = "352 0 16 40 10 14 2 255 0 0 44100";

/// One decoded stereo frame. Samples are signed 16-bit PCM in interleaved
/// left/right order.
pub(crate) struct DecodedPcm {
    samples: Vec<i16>,
}

impl DecodedPcm {
    pub(crate) fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub(crate) fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }
}

impl fmt::Debug for DecodedPcm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedPcm")
            .field("frames", &self.frames())
            .field("channels", &CHANNELS)
            .field("sample_count", &self.samples.len())
            .finish()
    }
}

/// A decoder fixed to the single ALAC tuple accepted by the type-96 SETUP
/// parser. Neither compressed input nor decoded samples are retained in its
/// debug representation.
pub(crate) struct Type96AlacDecoder {
    stream_info: StreamInfo,
    decoder: Decoder,
    scratch: Vec<i16>,
}

impl Type96AlacDecoder {
    pub(crate) fn new() -> Result<Self, DecodeError> {
        let stream_info = StreamInfo::from_sdp_format_parameters(AIRPLAY_TYPE96_ALAC_FMTP)
            .map_err(|_| DecodeError::InvalidDecoderConfiguration)?;
        if stream_info.max_frames_per_packet() as usize != FRAMES_PER_PACKET
            || stream_info.max_samples_per_packet() as usize != MAX_SAMPLES_PER_PACKET
            || stream_info.bit_depth() != 16
            || stream_info.channels() as usize != CHANNELS
            || stream_info.sample_rate() != SAMPLE_RATE
        {
            return Err(DecodeError::InvalidDecoderConfiguration);
        }

        let decoder = Decoder::new(stream_info.clone());
        Ok(Self {
            stream_info,
            decoder,
            scratch: vec![0; MAX_SAMPLES_PER_PACKET],
        })
    }

    pub(crate) fn decode_packet(&mut self, packet: &[u8]) -> Result<DecodedPcm, DecodeError> {
        if packet.is_empty() {
            return Err(DecodeError::EmptyPacket);
        }
        if packet.len() > MAX_COMPRESSED_PACKET_BYTES {
            return Err(DecodeError::PacketTooLarge {
                actual: packet.len(),
                maximum: MAX_COMPRESSED_PACKET_BYTES,
            });
        }

        // alac 0.5 has a few internal assertions for impossible bitstream
        // states. The network packet is untrusted, so do not let one of those
        // assertions unwind out of AudioHub. Reset scratch state after every
        // failure before accepting another packet.
        self.scratch.fill(0);
        let decoded = catch_unwind(AssertUnwindSafe(|| {
            self.decoder
                .decode_packet(packet, &mut self.scratch)
                .map(|samples| samples.len())
        }));

        let sample_count = match decoded {
            Ok(Ok(sample_count)) => sample_count,
            Ok(Err(_)) => {
                self.reset_decoder();
                return Err(DecodeError::InvalidPacket);
            }
            Err(_) => {
                self.reset_decoder();
                return Err(DecodeError::DecoderPanicked);
            }
        };

        if sample_count == 0
            || sample_count > MAX_SAMPLES_PER_PACKET
            || sample_count % CHANNELS != 0
        {
            self.reset_decoder();
            return Err(DecodeError::InvalidSampleCount {
                actual: sample_count,
                maximum: MAX_SAMPLES_PER_PACKET,
            });
        }

        let samples = self.scratch[..sample_count].to_vec();
        self.scratch.fill(0);
        Ok(DecodedPcm { samples })
    }

    fn reset_decoder(&mut self) {
        self.scratch.fill(0);
        self.decoder = Decoder::new(self.stream_info.clone());
    }
}

impl fmt::Debug for Type96AlacDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Type96AlacDecoder")
            .field("frames_per_packet", &FRAMES_PER_PACKET)
            .field("sample_rate", &SAMPLE_RATE)
            .field("channels", &CHANNELS)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    InvalidDecoderConfiguration,
    EmptyPacket,
    PacketTooLarge { actual: usize, maximum: usize },
    InvalidPacket,
    DecoderPanicked,
    InvalidSampleCount { actual: usize, maximum: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDecoderConfiguration => {
                f.write_str("the fixed type-96 ALAC decoder configuration is invalid")
            }
            Self::EmptyPacket => f.write_str("ALAC packet is empty"),
            Self::PacketTooLarge { actual, maximum } => {
                write!(f, "ALAC packet is {actual} bytes; maximum is {maximum}")
            }
            Self::InvalidPacket => f.write_str("ALAC packet is invalid"),
            Self::DecoderPanicked => f.write_str("ALAC decoder rejected an impossible bitstream"),
            Self::InvalidSampleCount { actual, maximum } => write!(
                f,
                "ALAC packet decoded to {actual} samples; maximum is {maximum}"
            ),
        }
    }
}

impl Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated independently from receiver implementations with FFmpeg 7.1.1:
    // ffmpeg -f lavfi -i 'anullsrc=r=44100:cl=stereo' \
    //   -af 'atrim=end_sample=352' -c:a alac vector.m4a
    // ffmpeg -i vector.m4a -map 0:a:0 -c copy -f data packet.alac
    const SILENCE_352: &str = concat!(
        "200010000002c000000f0801000000000000000f080100000000000000",
        "ff80afbfe02bfc"
    );

    #[test]
    fn decodes_independently_generated_fixed_vector() {
        let packet = hex::decode(SILENCE_352).unwrap();
        let pcm = Type96AlacDecoder::new()
            .unwrap()
            .decode_packet(&packet)
            .unwrap();

        assert_eq!(pcm.frames(), FRAMES_PER_PACKET);
        assert_eq!(pcm.samples().len(), MAX_SAMPLES_PER_PACKET);
        assert!(pcm.samples().iter().all(|sample| *sample == 0));
    }

    #[test]
    fn rejects_empty_oversized_and_corrupt_packets_without_unwinding() {
        let mut decoder = Type96AlacDecoder::new().unwrap();
        assert_eq!(
            decoder.decode_packet(&[]).unwrap_err(),
            DecodeError::EmptyPacket
        );
        assert_eq!(
            decoder
                .decode_packet(&vec![0; MAX_COMPRESSED_PACKET_BYTES + 1])
                .unwrap_err(),
            DecodeError::PacketTooLarge {
                actual: MAX_COMPRESSED_PACKET_BYTES + 1,
                maximum: MAX_COMPRESSED_PACKET_BYTES,
            }
        );

        // 0b110 selects a filler element and the zero extended count reaches
        // an assertion-prone path in some alac 0.5 builds. The adapter must
        // turn either implementation outcome into an ordinary error.
        let result = catch_unwind(AssertUnwindSafe(|| decoder.decode_packet(&[0xde, 0x00])));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());

        // A rejected packet must not poison the reusable decoder.
        let packet = hex::decode(SILENCE_352).unwrap();
        assert_eq!(
            decoder.decode_packet(&packet).unwrap().frames(),
            FRAMES_PER_PACKET
        );
    }

    #[test]
    fn debug_output_never_contains_input_or_pcm() {
        let decoder_debug = format!("{:?}", Type96AlacDecoder::new().unwrap());
        assert!(!decoder_debug.contains(SILENCE_352));

        let packet = hex::decode(SILENCE_352).unwrap();
        let pcm = Type96AlacDecoder::new()
            .unwrap()
            .decode_packet(&packet)
            .unwrap();
        let pcm_debug = format!("{pcm:?}");
        assert!(pcm_debug.contains("sample_count: 704"));
        assert!(!pcm_debug.contains("[0, 0"));
    }

    #[test]
    fn decoder_does_not_reuse_pcm_from_an_earlier_packet() {
        // If output scratch were ever exposed without a complete overwrite,
        // this sentinel would reveal it. Successful decoding must return only
        // the current packet and clear reusable storage before the next call.
        let mut decoder = Type96AlacDecoder::new().unwrap();
        decoder.scratch.fill(i16::MIN);
        let packet = hex::decode(SILENCE_352).unwrap();
        let pcm = decoder.decode_packet(&packet).unwrap();
        assert!(pcm.samples().iter().all(|sample| *sample == 0));
        assert!(decoder.scratch.iter().all(|sample| *sample == 0));
    }
}
