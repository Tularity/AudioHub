//! Strict parsing for the AirPlay 2 realtime-audio SETUP phase.

use plist::{Dictionary, Value};
use std::error::Error;
use std::fmt;
use std::io::Cursor;
use zeroize::Zeroize;

pub(crate) const STREAM_TYPE_REALTIME_AUDIO: u64 = 96;
pub(crate) const AUDIO_FORMAT_ALAC_44K1_STEREO_16_BIT: u64 = 0x0004_0000;
pub(crate) const COMPRESSION_TYPE_ALAC: u64 = 2;
pub(crate) const SAMPLES_PER_FRAME: u64 = 352;
pub(crate) const SAMPLE_RATE: u64 = 44_100;

const SHARED_KEY_BYTES: usize = 32;
const MAX_SETUP_BODY_BYTES: usize = 64 * 1024;

/// Owns property-list secret data while the remaining untrusted fields are
/// validated. Every return path after `shk` extraction runs this destructor.
struct SecretData(Vec<u8>);

impl SecretData {
    fn wipe(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for SecretData {
    fn drop(&mut self) {
        self.wipe();
        #[cfg(test)]
        SECRET_WIPE_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
static SECRET_WIPE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Validated parameters for the one realtime-audio profile AudioHub supports.
///
/// The negotiated shared key is deliberately not cloneable, is erased on drop,
/// and never appears in debug output.
pub(crate) struct Type96Setup {
    pub(crate) remote_control_port: Option<u16>,
    shared_key: [u8; SHARED_KEY_BYTES],
}

impl Type96Setup {
    pub(crate) fn shared_key(&self) -> &[u8; SHARED_KEY_BYTES] {
        &self.shared_key
    }
}

impl fmt::Debug for Type96Setup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Type96Setup")
            .field("stream_type", &STREAM_TYPE_REALTIME_AUDIO)
            .field("audio_format", &AUDIO_FORMAT_ALAC_44K1_STEREO_16_BIT)
            .field("compression_type", &COMPRESSION_TYPE_ALAC)
            .field("samples_per_frame", &SAMPLES_PER_FRAME)
            .field("sample_rate", &SAMPLE_RATE)
            .field("remote_control_port", &self.remote_control_port)
            .field("shared_key", &"<redacted>")
            .finish()
    }
}

impl Drop for Type96Setup {
    fn drop(&mut self) {
        self.shared_key.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupError {
    EmptyBody,
    BodyTooLarge { actual: usize, maximum: usize },
    NotBinaryPlist,
    MalformedPlist,
    RootNotDictionary,
    MissingField(&'static str),
    WrongFieldType(&'static str),
    StreamCount { actual: usize },
    UnsupportedValue { field: &'static str, actual: u64 },
    InvalidSharedKeyLength { actual: usize },
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBody => f.write_str("SETUP body is empty"),
            Self::BodyTooLarge { actual, maximum } => {
                write!(f, "SETUP body is {actual} bytes; maximum is {maximum}")
            }
            Self::NotBinaryPlist => f.write_str("SETUP body is not a binary property list"),
            Self::MalformedPlist => f.write_str("SETUP body is not a valid property list"),
            Self::RootNotDictionary => f.write_str("SETUP property-list root is not a dictionary"),
            Self::MissingField(field) => write!(f, "SETUP is missing required field {field}"),
            Self::WrongFieldType(field) => write!(f, "SETUP field {field} has the wrong type"),
            Self::StreamCount { actual } => {
                write!(
                    f,
                    "SETUP contains {actual} streams; exactly one is supported"
                )
            }
            Self::UnsupportedValue { field, actual } => {
                write!(f, "SETUP field {field} has unsupported value {actual}")
            }
            Self::InvalidSharedKeyLength { actual } => write!(
                f,
                "SETUP shared key has {actual} bytes; exactly {SHARED_KEY_BYTES} are required"
            ),
        }
    }
}

impl Error for SetupError {}

/// Parse an encrypted-session SETUP phase-two body.
///
/// Transport metadata not involved in codec selection is retained by the
/// sender and may vary by OS release, so it is ignored. The codec tuple itself
/// is exact: type 96, ALAC 44.1 kHz stereo 16-bit, compression type 2, and 352
/// samples per frame. A present sample-rate field must be 44.1 kHz. The
/// sender may provide its retransmission `controlPort`; when present it must be
/// an unsigned integer in the usable UDP port range.
pub(crate) fn parse_phase2(body: &[u8]) -> Result<Type96Setup, SetupError> {
    if body.is_empty() {
        return Err(SetupError::EmptyBody);
    }
    if body.len() > MAX_SETUP_BODY_BYTES {
        return Err(SetupError::BodyTooLarge {
            actual: body.len(),
            maximum: MAX_SETUP_BODY_BYTES,
        });
    }
    if !body.starts_with(b"bplist00") {
        return Err(SetupError::NotBinaryPlist);
    }

    let mut value =
        Value::from_reader(Cursor::new(body)).map_err(|_| SetupError::MalformedPlist)?;
    let root = value
        .as_dictionary_mut()
        .ok_or(SetupError::RootNotDictionary)?;
    let streams = root
        .get_mut("streams")
        .ok_or(SetupError::MissingField("streams"))?
        .as_array_mut()
        .ok_or(SetupError::WrongFieldType("streams"))?;
    if streams.len() != 1 {
        return Err(SetupError::StreamCount {
            actual: streams.len(),
        });
    }
    let stream = streams[0]
        .as_dictionary_mut()
        .ok_or(SetupError::WrongFieldType("streams[0]"))?;

    // Remove secret material from the general plist tree first. Its guard is
    // then wiped if any codec or transport validation below returns early.
    let shared_key_value = stream
        .remove("shk")
        .ok_or(SetupError::MissingField("shk"))?;
    let Value::Data(shared_key_data) = shared_key_value else {
        return Err(SetupError::WrongFieldType("shk"));
    };
    let shared_key_data = SecretData(shared_key_data);

    require_integer(stream, "type", STREAM_TYPE_REALTIME_AUDIO)?;
    require_integer(stream, "audioFormat", AUDIO_FORMAT_ALAC_44K1_STEREO_16_BIT)?;
    require_integer(stream, "ct", COMPRESSION_TYPE_ALAC)?;
    require_integer(stream, "spf", SAMPLES_PER_FRAME)?;
    if stream.contains_key("sr") {
        require_integer(stream, "sr", SAMPLE_RATE)?;
    }
    let remote_control_port = optional_port(stream, "controlPort")?;
    if shared_key_data.0.len() != SHARED_KEY_BYTES {
        let actual = shared_key_data.0.len();
        return Err(SetupError::InvalidSharedKeyLength { actual });
    }
    let mut shared_key = [0u8; SHARED_KEY_BYTES];
    shared_key.copy_from_slice(&shared_key_data.0);

    Ok(Type96Setup {
        remote_control_port,
        shared_key,
    })
}

fn require_integer(
    dictionary: &Dictionary,
    field: &'static str,
    expected: u64,
) -> Result<(), SetupError> {
    let actual = dictionary
        .get(field)
        .ok_or(SetupError::MissingField(field))?
        .as_unsigned_integer()
        .ok_or(SetupError::WrongFieldType(field))?;
    if actual != expected {
        return Err(SetupError::UnsupportedValue { field, actual });
    }
    Ok(())
}

fn optional_port(dictionary: &Dictionary, field: &'static str) -> Result<Option<u16>, SetupError> {
    let Some(value) = dictionary.get(field) else {
        return Ok(None);
    };
    let actual = value
        .as_unsigned_integer()
        .ok_or(SetupError::WrongFieldType(field))?;
    let port = u16::try_from(actual)
        .ok()
        .filter(|port| *port != 0)
        .ok_or(SetupError::UnsupportedValue { field, actual })?;
    Ok(Some(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_stream() -> Dictionary {
        [
            ("type", Value::from(STREAM_TYPE_REALTIME_AUDIO)),
            (
                "audioFormat",
                Value::from(AUDIO_FORMAT_ALAC_44K1_STEREO_16_BIT),
            ),
            ("ct", Value::from(COMPRESSION_TYPE_ALAC)),
            ("spf", Value::from(SAMPLES_PER_FRAME)),
            ("shk", Value::Data((0u8..32).collect())),
        ]
        .into_iter()
        .collect()
    }

    fn body_with_stream(stream: Dictionary) -> Vec<u8> {
        let root: Dictionary = [("streams", Value::Array(vec![Value::Dictionary(stream)]))]
            .into_iter()
            .collect();
        let mut body = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut body).unwrap();
        body
    }

    #[test]
    fn accepts_exact_profile_with_implicit_sample_rate() {
        let setup = parse_phase2(&body_with_stream(valid_stream())).unwrap();
        assert_eq!(setup.shared_key(), &(0u8..32).collect::<Vec<_>>()[..]);
        assert_eq!(setup.remote_control_port, None);
    }

    #[test]
    fn accepts_explicit_44100_sender_control_port_and_unrelated_metadata() {
        let mut stream = valid_stream();
        stream.insert("sr".into(), Value::from(SAMPLE_RATE));
        stream.insert("controlPort".into(), Value::from(65_535u64));
        stream.insert("audioMode".into(), Value::String("default".into()));
        let setup = parse_phase2(&body_with_stream(stream)).unwrap();
        assert_eq!(setup.remote_control_port, Some(65_535));
    }

    #[test]
    fn rejects_every_unknown_codec_tuple_value() {
        for (field, replacement) in [
            ("type", 103),
            ("audioFormat", 0x0004_0001),
            ("ct", 4),
            ("spf", 480),
            ("sr", 48_000),
        ] {
            let mut stream = valid_stream();
            stream.insert(field.into(), Value::from(replacement));
            assert_eq!(
                parse_phase2(&body_with_stream(stream)).unwrap_err(),
                SetupError::UnsupportedValue {
                    field,
                    actual: replacement,
                }
            );
        }
    }

    #[test]
    fn rejects_missing_or_mistyped_codec_fields() {
        for field in ["type", "audioFormat", "ct", "spf", "shk"] {
            let mut stream = valid_stream();
            stream.remove(field);
            assert_eq!(
                parse_phase2(&body_with_stream(stream)).unwrap_err(),
                SetupError::MissingField(field)
            );
        }

        let mut stream = valid_stream();
        stream.insert("ct".into(), Value::String("2".into()));
        assert_eq!(
            parse_phase2(&body_with_stream(stream)).unwrap_err(),
            SetupError::WrongFieldType("ct")
        );
    }

    #[test]
    fn rejects_unusable_or_mistyped_control_port() {
        for port in [0, 65_536, 70_001, u64::MAX] {
            let mut stream = valid_stream();
            stream.insert("controlPort".into(), Value::from(port));
            assert_eq!(
                parse_phase2(&body_with_stream(stream)).unwrap_err(),
                SetupError::UnsupportedValue {
                    field: "controlPort",
                    actual: port,
                }
            );
        }

        let mut stream = valid_stream();
        stream.insert("controlPort".into(), Value::String("7001".into()));
        assert_eq!(
            parse_phase2(&body_with_stream(stream)).unwrap_err(),
            SetupError::WrongFieldType("controlPort")
        );
    }

    #[test]
    fn wipes_valid_shared_key_when_later_codec_validation_fails() {
        use std::sync::atomic::Ordering;

        let before = SECRET_WIPE_COUNT.load(Ordering::SeqCst);
        let mut stream = valid_stream();
        stream.insert("shk".into(), Value::Data(vec![0xa5; SHARED_KEY_BYTES]));
        stream.insert("spf".into(), Value::from(480u64));

        let error = parse_phase2(&body_with_stream(stream)).unwrap_err();
        assert_eq!(
            error,
            SetupError::UnsupportedValue {
                field: "spf",
                actual: 480,
            }
        );
        assert!(SECRET_WIPE_COUNT.load(Ordering::SeqCst) > before);
        assert!(!format!("{error:?}").contains("a5"));
    }

    #[test]
    fn shared_key_must_be_exact_and_is_never_formatted() {
        for length in [0, 31, 33, 512] {
            let mut stream = valid_stream();
            stream.insert("shk".into(), Value::Data(vec![0xa5; length]));
            let error = parse_phase2(&body_with_stream(stream)).unwrap_err();
            assert_eq!(error, SetupError::InvalidSharedKeyLength { actual: length });
            assert!(!format!("{error:?}").contains("a5"));
        }

        let setup = parse_phase2(&body_with_stream(valid_stream())).unwrap();
        let debug = format!("{setup:?}");
        assert!(debug.contains("remote_control_port: None"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("00010203"));

        let mut stream = valid_stream();
        stream.insert("controlPort".into(), Value::from(7_001u64));
        let debug = format!("{:?}", parse_phase2(&body_with_stream(stream)).unwrap());
        assert!(debug.contains("remote_control_port: Some(7001)"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn rejects_nonbinary_malformed_and_oversized_inputs() {
        assert_eq!(parse_phase2(b"").unwrap_err(), SetupError::EmptyBody);
        assert_eq!(
            parse_phase2(b"<?xml version=\"1.0\"?><plist/>").unwrap_err(),
            SetupError::NotBinaryPlist
        );
        assert_eq!(
            parse_phase2(b"bplist00not-a-plist").unwrap_err(),
            SetupError::MalformedPlist
        );
        assert_eq!(
            parse_phase2(&vec![0; MAX_SETUP_BODY_BYTES + 1]).unwrap_err(),
            SetupError::BodyTooLarge {
                actual: MAX_SETUP_BODY_BYTES + 1,
                maximum: MAX_SETUP_BODY_BYTES,
            }
        );
    }

    #[test]
    fn requires_one_dictionary_stream() {
        let mut body = Vec::new();
        Value::Dictionary(Dictionary::new())
            .to_writer_binary(&mut body)
            .unwrap();
        assert_eq!(
            parse_phase2(&body).unwrap_err(),
            SetupError::MissingField("streams")
        );

        let root: Dictionary = [("streams", Value::Array(Vec::new()))]
            .into_iter()
            .collect();
        body.clear();
        Value::Dictionary(root).to_writer_binary(&mut body).unwrap();
        assert_eq!(
            parse_phase2(&body).unwrap_err(),
            SetupError::StreamCount { actual: 0 }
        );
    }
}
