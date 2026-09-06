//! The classic unicast UDP Transport offer; destination IP comes from RTSP.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Transport {
    pub(crate) control_port: u16,
    pub(crate) timing_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransportError;

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unsupported classic unicast UDP transport")
    }
}

impl std::error::Error for TransportError {}

fn port(value: &str) -> Result<u16, TransportError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TransportError);
    }
    let port = value.parse::<u16>().map_err(|_| TransportError)?;
    if port == 0 {
        return Err(TransportError);
    }
    Ok(port)
}

impl Transport {
    pub(crate) fn parse(header: &[u8]) -> Result<Self, TransportError> {
        if header.len() > 1024
            || !header.is_ascii()
            || header.iter().any(|b| b.is_ascii_control() && *b != b'\t')
        {
            return Err(TransportError);
        }
        let text = std::str::from_utf8(header).map_err(|_| TransportError)?;
        let mut fields = text.split(';').map(str::trim);
        if fields.next() != Some("RTP/AVP/UDP") {
            return Err(TransportError);
        }
        let (mut unicast, mut mode, mut control, mut timing) = (false, false, None, None);
        let mut interleaved = false;
        for field in fields {
            if field == "unicast" && !unicast {
                unicast = true;
                continue;
            }
            let (name, value) = field.split_once('=').ok_or(TransportError)?;
            match name {
                // Apple Music includes this legacy marker in its UDP offer.
                // The explicit UDP profile still owns three UDP sockets;
                // this does not enable RTSP-interleaved TCP media.
                "interleaved" if !interleaved && value == "0-1" => interleaved = true,
                "mode" if !mode && matches!(value, "record" | "\"record\"") => mode = true,
                "control_port" if control.is_none() => control = Some(port(value)?),
                "timing_port" if timing.is_none() => timing = Some(port(value)?),
                _ => return Err(TransportError),
            }
        }
        if !unicast || !mode {
            return Err(TransportError);
        }
        Ok(Self {
            control_port: control.ok_or(TransportError)?,
            timing_port: timing.ok_or(TransportError)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OFFER: &str = "RTP/AVP/UDP;unicast;mode=record;control_port=55000;timing_port=55001";

    #[test]
    fn accepts_the_supported_offer_without_trusting_destination_addresses() {
        assert_eq!(
            Transport::parse(OFFER.as_bytes()).unwrap(),
            Transport {
                control_port: 55000,
                timing_port: 55001
            }
        );
        let quoted = OFFER.replace("mode=record", "mode=\"record\"");
        assert!(Transport::parse(quoted.as_bytes()).is_ok());
    }

    #[test]
    fn accepts_the_observed_apple_music_udp_offer() {
        let offer =
            b"RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002";
        assert_eq!(
            Transport::parse(offer).unwrap(),
            Transport {
                control_port: 6001,
                timing_port: 6002,
            }
        );
    }

    #[test]
    fn rejects_missing_duplicate_unknown_and_multicast_fields() {
        for input in [
            OFFER.replace(";unicast", ""),
            OFFER.replace(";mode=record", ""),
            OFFER.replace(";timing_port=55001", ""),
            OFFER.replace("unicast", "multicast"),
            OFFER.replace("RTP/AVP/UDP", "RTP/AVP/TCP"),
            format!("{OFFER};unicast"),
            format!("{OFFER};control_port=5"),
            format!("{OFFER};interleaved=0-1;interleaved=0-1"),
            format!("{OFFER};interleaved=1-2"),
            format!("{OFFER};interleaved=0"),
            format!("{OFFER};interleaved=0-1-2"),
            format!(
                "{};interleaved=0-1",
                OFFER.replace("RTP/AVP/UDP", "RTP/AVP/TCP")
            ),
            format!("{OFFER};destination=192.0.2.1"),
            format!("{OFFER};"),
        ] {
            assert_eq!(Transport::parse(input.as_bytes()), Err(TransportError));
        }
    }

    #[test]
    fn rejects_port_ranges_overflow_controls_and_oversized_headers() {
        for value in ["0", "65536", "-1", "+42", "42-43", " 42", "x"] {
            assert_eq!(
                Transport::parse(OFFER.replace("55000", value).as_bytes()),
                Err(TransportError)
            );
        }
        assert_eq!(
            Transport::parse(format!("{OFFER}\r\nInjected: x").as_bytes()),
            Err(TransportError)
        );
        assert_eq!(Transport::parse(&vec![b'x'; 1025]), Err(TransportError));
    }
}
