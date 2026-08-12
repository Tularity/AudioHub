//! Just enough SDP parsing to pull the fields an AirPlay 1 ANNOUNCE carries.
//!
//! The body looks like:
//! ```text
//! v=0
//! o=iTunes 3413821438 0 IN IP4 192.168.1.2
//! s=iTunes
//! c=IN IP4 192.168.1.10
//! t=0 0
//! m=audio 0 RTP/AVP 96
//! a=rtpmap:96 AppleLossless
//! a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
//! a=rsaaeskey:<base64>
//! a=aesiv:<base64>
//! a=min-latency:11025
//! ```

/// The AirPlay-relevant subset of an ANNOUNCE SDP body. Values are the raw
/// attribute text (after the `a=<name>:` prefix); decoding is the caller's
/// job so parse errors surface with proper RTSP status codes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sdp {
    pub rtpmap: Option<String>,
    pub fmtp: Option<String>,
    pub rsaaeskey: Option<String>,
    pub aesiv: Option<String>,
    pub min_latency: Option<u32>,
}

impl Sdp {
    pub fn parse(body: &str) -> Sdp {
        let mut sdp = Sdp::default();
        for line in body.lines() {
            let Some(attr) = line.strip_prefix("a=") else {
                continue;
            };
            let Some((name, value)) = attr.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name {
                "rtpmap" => sdp.rtpmap = Some(value.to_string()),
                "fmtp" => sdp.fmtp = Some(value.to_string()),
                "rsaaeskey" => sdp.rsaaeskey = Some(value.to_string()),
                "aesiv" => sdp.aesiv = Some(value.to_string()),
                "min-latency" => sdp.min_latency = value.parse().ok(),
                _ => {}
            }
        }
        sdp
    }
}

/// The ALAC configuration carried in `a=fmtp`. The twelve integers are, in
/// order: payload type, frames per packet, compatible-version, bit depth,
/// pb, mb, kb, channels, max-run, max-frame-bytes, avg-bitrate, sample rate.
/// Only the ones we need downstream are named; the rest are kept for the
/// milestone-3 decoder cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlacConfig {
    pub frames_per_packet: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub sample_rate: u32,
    pub raw: [u32; 12],
}

impl AlacConfig {
    /// Parse the space-separated fmtp integers. Requires the full 12 fields
    /// (every Apple sender sends them).
    pub fn parse(fmtp: &str) -> Option<AlacConfig> {
        let mut raw = [0u32; 12];
        let mut tokens = fmtp.split_whitespace();
        for slot in &mut raw {
            *slot = tokens.next()?.parse().ok()?;
        }
        if tokens.next().is_some() {
            return None;
        }
        Some(AlacConfig {
            frames_per_packet: raw[1],
            bit_depth: raw[3] as u8,
            channels: raw[7] as u8,
            sample_rate: raw[11],
            raw,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "v=0\r\n\
        o=iTunes 3413821438 0 IN IP4 192.168.1.2\r\n\
        s=iTunes\r\n\
        c=IN IP4 192.168.1.10\r\n\
        t=0 0\r\n\
        m=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
        a=rsaaeskey:AAECAwQF\r\n\
        a=aesiv:BgcICQoL\r\n\
        a=min-latency:11025\r\n";

    #[test]
    fn parses_all_fields() {
        let sdp = Sdp::parse(BODY);
        assert_eq!(sdp.rtpmap.as_deref(), Some("96 AppleLossless"));
        assert_eq!(
            sdp.fmtp.as_deref(),
            Some("96 352 0 16 40 10 14 2 255 0 0 44100")
        );
        assert_eq!(sdp.rsaaeskey.as_deref(), Some("AAECAwQF"));
        assert_eq!(sdp.aesiv.as_deref(), Some("BgcICQoL"));
        assert_eq!(sdp.min_latency, Some(11025));
    }

    #[test]
    fn unencrypted_body_has_no_keys() {
        let body = "v=0\na=rtpmap:96 AppleLossless\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\n";
        let sdp = Sdp::parse(body);
        assert!(sdp.rsaaeskey.is_none() && sdp.aesiv.is_none());
        assert!(sdp.fmtp.is_some());
    }

    #[test]
    fn parses_alac_config() {
        let sdp = Sdp::parse(BODY);
        let alac = AlacConfig::parse(sdp.fmtp.as_deref().unwrap()).unwrap();
        assert_eq!(alac.frames_per_packet, 352);
        assert_eq!(alac.bit_depth, 16);
        assert_eq!(alac.channels, 2);
        assert_eq!(alac.sample_rate, 44100);
        assert_eq!(alac.raw[0], 96);
    }

    #[test]
    fn rejects_short_fmtp() {
        assert!(AlacConfig::parse("96 352 0 16").is_none());
        assert!(AlacConfig::parse("96 352 x 16 40 10 14 2 255 0 0 44100").is_none());
        assert!(AlacConfig::parse("96 352 0 16 40 10 14 2 255 0 0 44100 1").is_none());
    }
}
