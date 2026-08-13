//! AirPlay 2 discovery and `/info` facts advertised by AudioHub.
//!
//! Sources are protocol observations and the shairport-sync AirPlay 2 profile,
//! not copied receiver code.  Only capabilities implemented by the current
//! milestone belong here; adding a feature bit is an interoperability promise.

use super::identity::ReceiverIdentity;
use plist::{Dictionary, Value};
use std::io;

pub(crate) const SOURCE_VERSION: &str = "366.0";
pub(crate) const MODEL: &str = "AudioHub,1";
/// Audio-only profile measured against Music 1.6.5 on macOS 26.5.2. It
/// advertises only capabilities implemented by this receiver: audio, unified
/// control, buffered audio and PTP-session negotiation. Pairing is negotiated
/// transiently on the control connection; persistent HomeKit pairing and
/// grouping remain disabled (`acl=0`, `gcgl=0`, `igl=0`).
pub(crate) const FEATURES: u64 = 0x0001_0340_405C_4A00;
pub(crate) const STATUS_AUDIO_ATTACHED: u32 = 0x4;
pub(crate) const STATUS_PASSWORD_REQUIRED: u32 = 1 << 7;

pub(crate) struct InfoProfile<'a> {
    pub(crate) name: &'a str,
    pub(crate) mac: [u8; 6],
    pub(crate) password_required: bool,
    pub(crate) identity: &'a ReceiverIdentity,
    pub(crate) features: u64,
    pub(crate) initial_volume_db: Option<f32>,
}

impl InfoProfile<'_> {
    pub(crate) fn status_flags(&self) -> u32 {
        STATUS_AUDIO_ATTACHED
            | if self.password_required {
                STATUS_PASSWORD_REQUIRED
            } else {
                0
            }
    }

    pub(crate) fn device_id(&self) -> String {
        self.mac
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    pub(crate) fn txt_records(&self) -> Vec<String> {
        let pi = self.identity.public_identifier();
        vec![
            "acl=0".into(),
            format!("deviceid={}", self.device_id()),
            format!(
                "features=0x{:X},0x{:X}",
                self.features as u32,
                self.features >> 32
            ),
            format!("flags=0x{:X}", self.status_flags()),
            format!("gid={pi}"),
            "gcgl=0".into(),
            "igl=0".into(),
            format!("model={MODEL}"),
            "protovers=1.1".into(),
            format!("pi={pi}"),
            format!("pk={}", self.identity.public_key_hex()),
            format!("srcvers={SOURCE_VERSION}"),
            "vv=2".into(),
        ]
    }

    pub(crate) fn dictionary(&self) -> io::Result<Dictionary> {
        let mut root = Dictionary::new();
        root.insert("vv".into(), Value::Integer(2.into()));
        root.insert("protocolVersion".into(), Value::String("1.1".into()));
        root.insert("volumeControlType".into(), Value::Integer(3.into()));
        root.insert("canRecordScreenStream".into(), Value::Boolean(false));
        root.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(false));
        root.insert("screenDemoMode".into(), Value::Boolean(false));
        root.insert("deviceID".into(), Value::String(self.device_id()));
        root.insert("name".into(), Value::String(self.name.to_string()));
        root.insert("model".into(), Value::String(MODEL.into()));
        root.insert("sourceVersion".into(), Value::String(SOURCE_VERSION.into()));
        root.insert("features".into(), Value::Integer(self.features.into()));
        root.insert(
            "statusFlags".into(),
            Value::Integer(u64::from(self.status_flags()).into()),
        );
        root.insert(
            "pi".into(),
            Value::String(self.identity.public_identifier()),
        );
        root.insert(
            "pk".into(),
            Value::Data(self.identity.public_key().to_vec()),
        );
        let mut supported_formats = Dictionary::new();
        supported_formats.insert("audioStream".into(), Value::Integer(0x0004_0000u64.into()));
        supported_formats.insert("bufferStream".into(), Value::Integer(0x0040_0000u64.into()));
        root.insert(
            "supportedFormats".into(),
            Value::Dictionary(supported_formats),
        );
        root.insert(
            "txtAirPlay".into(),
            Value::Data(pack_txt(&self.txt_records())?),
        );

        if let Some(volume) = self.initial_volume_db {
            root.insert("initialVolume".into(), Value::Real(f64::from(volume)));
        }

        Ok(root)
    }

    pub(crate) fn binary_plist(&self) -> io::Result<Vec<u8>> {
        let root = self.dictionary()?;

        let mut output = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut output)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(output)
    }
}

fn pack_txt(records: &[String]) -> io::Result<Vec<u8>> {
    let mut packed = Vec::new();
    for record in records {
        let length = u8::try_from(record.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay TXT record exceeds 255 bytes",
            )
        })?;
        packed.push(length);
        packed.extend_from_slice(record.as_bytes());
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_profile_is_consistent_with_info() {
        let identity = ReceiverIdentity::generate();
        let profile = InfoProfile {
            name: "AudioHub Lab",
            mac: [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee],
            password_required: false,
            identity: &identity,
            features: FEATURES,
            initial_volume_db: Some(-18.0),
        };
        let txt = profile.txt_records();
        assert!(txt
            .iter()
            .any(|value| value == "features=0x405C4A00,0x10340"));
        assert!(txt.iter().any(|value| value == "flags=0x4"));
        assert!(!txt.iter().any(|value| value.starts_with("pw=")));

        let value =
            Value::from_reader(std::io::Cursor::new(profile.binary_plist().unwrap())).unwrap();
        let dictionary = value.as_dictionary().unwrap();
        assert_eq!(
            dictionary
                .get("features")
                .and_then(Value::as_unsigned_integer),
            Some(FEATURES)
        );
        assert_eq!(
            dictionary
                .get("volumeControlType")
                .and_then(Value::as_unsigned_integer),
            Some(3)
        );
        assert_eq!(
            dictionary.get("initialVolume").and_then(Value::as_real),
            Some(-18.0)
        );
        assert_eq!(
            dictionary.get("pk").and_then(Value::as_data),
            Some(identity.public_key().as_slice())
        );
        let formats = dictionary
            .get("supportedFormats")
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            formats
                .get("audioStream")
                .and_then(Value::as_unsigned_integer),
            Some(0x0004_0000)
        );
        assert_eq!(
            formats
                .get("bufferStream")
                .and_then(Value::as_unsigned_integer),
            Some(0x0040_0000)
        );
    }

    #[test]
    fn password_required_is_one_status_bit_not_a_secret() {
        let identity = ReceiverIdentity::generate();
        let profile = InfoProfile {
            name: "Protected",
            mac: [2, 3, 4, 5, 6, 7],
            password_required: true,
            identity: &identity,
            features: FEATURES,
            initial_volume_db: None,
        };
        assert_eq!(profile.status_flags(), 0x84);
        assert!(profile
            .txt_records()
            .iter()
            .any(|value| value == "flags=0x84"));
    }

    #[test]
    fn txt_wire_encoding_rejects_oversized_records() {
        assert!(pack_txt(&["x".repeat(256)]).is_err());
    }
}
