//! Outbound AirPlay 2 event-channel commands owned by AudioHub.
//!
//! Event framing and encryption belong to the server transport. This module
//! only validates command values and produces the binary property-list body.

use plist::{Dictionary, Value};
use std::{error::Error, fmt};

const MEDIA_REMOTE_COMMAND: &str = "sendMediaRemoteCommand";
const DEVICE_VOLUME_COMMAND: &str = "dvlc";
const AIRPLAY_VOLUME_MIN_SCALAR: f32 = 0.0;
const AIRPLAY_VOLUME_MAX_SCALAR: f32 = 1.0;

/// Failure to construct an outbound AirPlay 2 event command.
#[derive(Debug)]
pub(crate) enum EventCommandError {
    InvalidVolume(f32),
    Encode(plist::Error),
}

impl fmt::Display for EventCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVolume(volume) => write!(
                formatter,
                "AirPlay event volume must be finite and within 0..=1, got {volume}"
            ),
            Self::Encode(error) => {
                write!(formatter, "failed to encode AirPlay event plist: {error}")
            }
        }
    }
}

impl Error for EventCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidVolume(_) => None,
            Self::Encode(error) => Some(error),
        }
    }
}

/// Encode a device-volume (`dvlc`) media-remote command as a binary plist.
///
/// Unlike `/info`'s `initialVolume` and the RTSP text `volume` parameter,
/// Apple's event handler consumes this field as the visible slider scalar in
/// `0..=1` and performs the scalar-to-dB conversion itself. Both the root and
/// `params` carry volume and mute state. Keeping the two
/// representations identical accommodates senders that inspect either shape
/// without allowing them to disagree.
pub(crate) fn encode_device_volume_command(
    volume_scalar: f32,
    is_muted: bool,
) -> Result<Vec<u8>, EventCommandError> {
    if !volume_scalar.is_finite()
        || !(AIRPLAY_VOLUME_MIN_SCALAR..=AIRPLAY_VOLUME_MAX_SCALAR).contains(&volume_scalar)
    {
        return Err(EventCommandError::InvalidVolume(volume_scalar));
    }

    // Canonicalize negative zero so equivalent silence states have identical
    // wire representations.
    let volume = if volume_scalar == 0.0 {
        0.0
    } else {
        volume_scalar
    };
    let volume = Value::Real(f64::from(volume));
    let muted = Value::Boolean(is_muted);

    let mut params = Dictionary::new();
    params.insert("volume".into(), volume.clone());
    params.insert("isMuted".into(), muted.clone());

    let mut command = Dictionary::new();
    command.insert("type".into(), Value::String(MEDIA_REMOTE_COMMAND.into()));
    command.insert("value".into(), Value::String(DEVICE_VOLUME_COMMAND.into()));
    command.insert("volume".into(), volume);
    command.insert("isMuted".into(), muted);
    command.insert("params".into(), Value::Dictionary(params));

    let mut body = Vec::new();
    Value::Dictionary(command)
        .to_writer_binary(&mut body)
        .map_err(EventCommandError::Encode)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn decode(body: &[u8]) -> Dictionary {
        Value::from_reader(Cursor::new(body))
            .unwrap()
            .into_dictionary()
            .unwrap()
    }

    #[test]
    fn device_volume_command_has_the_complete_wire_shape() {
        let body = encode_device_volume_command(0.375, true).unwrap();
        assert!(body.starts_with(b"bplist00"));

        let root = decode(&body);
        assert_eq!(root.len(), 5);
        assert_eq!(
            root.get("type").and_then(Value::as_string),
            Some("sendMediaRemoteCommand")
        );
        assert_eq!(root.get("value").and_then(Value::as_string), Some("dvlc"));
        assert_eq!(root.get("volume").and_then(Value::as_real), Some(0.375));
        assert_eq!(root.get("isMuted").and_then(Value::as_boolean), Some(true));

        let params = root.get("params").and_then(Value::as_dictionary).unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params.get("volume").and_then(Value::as_real), Some(0.375));
        assert_eq!(
            params.get("isMuted").and_then(Value::as_boolean),
            Some(true)
        );
    }

    #[test]
    fn endpoint_volumes_remain_reals_and_mute_state_stays_consistent() {
        for (volume, is_muted) in [(0.0, true), (1.0, false)] {
            let root = decode(&encode_device_volume_command(volume, is_muted).unwrap());
            let params = root.get("params").and_then(Value::as_dictionary).unwrap();

            assert_eq!(
                root.get("volume").and_then(Value::as_real),
                Some(f64::from(volume))
            );
            assert_eq!(
                params.get("volume").and_then(Value::as_real),
                Some(f64::from(volume))
            );
            assert_eq!(
                root.get("isMuted").and_then(Value::as_boolean),
                Some(is_muted)
            );
            assert_eq!(
                params.get("isMuted").and_then(Value::as_boolean),
                Some(is_muted)
            );
        }
    }

    #[test]
    fn non_binary_decimal_is_exactly_widened_from_f32() {
        let input = 0.1_f32;
        let expected = f64::from(input);
        let root = decode(&encode_device_volume_command(input, false).unwrap());
        let root_volume = root.get("volume").and_then(Value::as_real).unwrap();
        let params_volume = root
            .get("params")
            .and_then(Value::as_dictionary)
            .and_then(|params| params.get("volume"))
            .and_then(Value::as_real)
            .unwrap();

        assert_eq!(root_volume.to_bits(), expected.to_bits());
        assert_eq!(params_volume.to_bits(), expected.to_bits());
    }

    #[test]
    fn values_immediately_inside_and_outside_the_endpoints_are_classified_exactly() {
        let immediately_above_zero = f32::from_bits(1);
        let immediately_below_one = f32::from_bits(1.0_f32.to_bits() - 1);
        for volume in [immediately_above_zero, immediately_below_one] {
            let root = decode(&encode_device_volume_command(volume, false).unwrap());
            assert_eq!(
                root.get("volume").and_then(Value::as_real),
                Some(f64::from(volume))
            );
        }

        let immediately_below_zero = -f32::from_bits(1);
        let immediately_above_one = f32::from_bits(1.0_f32.to_bits() + 1);
        for volume in [immediately_below_zero, immediately_above_one] {
            assert!(matches!(
                encode_device_volume_command(volume, false),
                Err(EventCommandError::InvalidVolume(rejected))
                    if rejected.to_bits() == volume.to_bits()
            ));
        }
    }

    #[test]
    fn mute_state_is_independent_of_volume_endpoints() {
        for (volume, is_muted) in [(0.0, false), (1.0, true)] {
            let root = decode(&encode_device_volume_command(volume, is_muted).unwrap());
            let params = root.get("params").and_then(Value::as_dictionary).unwrap();

            assert_eq!(
                root.get("volume").and_then(Value::as_real),
                Some(f64::from(volume))
            );
            assert_eq!(
                params.get("volume").and_then(Value::as_real),
                Some(f64::from(volume))
            );
            assert_eq!(
                root.get("isMuted").and_then(Value::as_boolean),
                Some(is_muted)
            );
            assert_eq!(
                params.get("isMuted").and_then(Value::as_boolean),
                Some(is_muted)
            );
        }
    }

    #[test]
    fn negative_zero_is_canonicalized() {
        let root = decode(&encode_device_volume_command(-0.0, true).unwrap());
        let root_volume = root.get("volume").and_then(Value::as_real).unwrap();
        let params_volume = root
            .get("params")
            .and_then(Value::as_dictionary)
            .and_then(|params| params.get("volume"))
            .and_then(Value::as_real)
            .unwrap();

        assert_eq!(root_volume.to_bits(), 0.0f64.to_bits());
        assert_eq!(params_volume.to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn non_finite_and_out_of_range_volumes_are_rejected() {
        for volume in [f32::NAN, f32::NEG_INFINITY, f32::INFINITY, -0.001, 1.001] {
            assert!(matches!(
                encode_device_volume_command(volume, false),
                Err(EventCommandError::InvalidVolume(rejected))
                    if rejected.to_bits() == volume.to_bits()
            ));
        }
    }
}
