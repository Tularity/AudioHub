//! Native default-output facts, separate from AudioHub's accepted media formats.
//!
//! A platform renderer being available does not make the current stereo network
//! sink support that renderer. Never derive a transport capability from these
//! observations without implementing and negotiating its matching sink.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NativeOutputCapabilities {
    #[default]
    Unknown,
    Unavailable,
    Error {
        message: String,
    },
    Available {
        endpoint_id: String,
        mix_format: OutputMixFormat,
        spatial_audio: SpatialAudioCapabilities,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
    /// Windows speaker-position bitmap when the native format provides one.
    pub channel_mask: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SpatialAudioCapabilities {
    #[default]
    Unknown,
    Unsupported,
    Inactive {
        configured_format: Option<String>,
    },
    Available {
        /// Opaque native format GUID, not a guessed Dolby certification flag.
        active_format: Option<String>,
        configured_format: Option<String>,
        max_dynamic_objects: u32,
        static_object_mask: u32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NativeOutputObservation {
    /// Monotonic only within the live control connection; never persist it.
    pub revision: u64,
    pub capabilities: NativeOutputCapabilities,
}

fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_format(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|s| bounded_text(s, 128))
}

impl NativeOutputCapabilities {
    /// Reject malformed remote observations before exposing them to consumers.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Unknown | Self::Unavailable => true,
            Self::Error { message } => bounded_text(message, 512),
            Self::Available {
                endpoint_id,
                mix_format,
                spatial_audio,
            } => {
                bounded_text(endpoint_id, 1024)
                    && (8_000..=768_000).contains(&mix_format.sample_rate)
                    && (1..=64).contains(&mix_format.channels)
                    && bounded_text(&mix_format.sample_format, 32)
                    && spatial_audio.is_valid()
            }
        }
    }
}

impl SpatialAudioCapabilities {
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Unknown | Self::Unsupported => true,
            Self::Inactive { configured_format } => valid_format(configured_format),
            Self::Available {
                active_format,
                configured_format,
                max_dynamic_objects,
                ..
            } => {
                active_format
                    .as_ref()
                    .is_some_and(|format| bounded_text(format, 128))
                    && valid_format(configured_format)
                    && *max_dynamic_objects <= 1024
            }
            Self::Error { message } => bounded_text(message, 512),
        }
    }
}

/// Reads properties only. It must not initialize an audio stream or prompt for
/// microphone, screen-recording, or administrator permission.
pub fn default_output_capabilities() -> NativeOutputCapabilities {
    platform::query()
}

#[cfg(target_os = "windows")]
#[path = "output_capabilities_windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "output_capabilities_macos.rs"]
mod platform;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    pub(super) fn query() -> super::NativeOutputCapabilities {
        super::NativeOutputCapabilities::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "explicit native endpoint observation in an isolated test VM"]
    fn native_output_observation_in_test_vm() {
        let capabilities = default_output_capabilities();
        println!("{}", serde_json::to_string(&capabilities).unwrap());
        assert!(capabilities.is_valid());
        assert!(matches!(
            capabilities,
            NativeOutputCapabilities::Available { .. }
        ));
    }

    #[test]
    fn observations_keep_unknown_unavailable_and_errors_distinct() {
        let unknown = NativeOutputCapabilities::Unknown;
        let absent = NativeOutputCapabilities::Unavailable;
        let error = NativeOutputCapabilities::Error {
            message: "endpoint query failed".into(),
        };
        for value in [&unknown, &absent, &error] {
            let encoded = serde_json::to_vec(value).unwrap();
            let decoded: NativeOutputCapabilities = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(&decoded, value);
            assert!(decoded.is_valid());
        }
        assert_ne!(unknown, absent);
        assert_ne!(absent, error);
    }

    #[test]
    fn invalid_native_facts_are_not_usable_advertisements() {
        let mut value = NativeOutputCapabilities::Available {
            endpoint_id: "test-output".into(),
            mix_format: OutputMixFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_format: "f32".into(),
                channel_mask: Some(3),
            },
            spatial_audio: SpatialAudioCapabilities::Available {
                active_format: Some("{1459AC38-3875-49BF-BB59-0FE80F4D395D}".into()),
                configured_format: None,
                max_dynamic_objects: 128,
                static_object_mask: 0xFFFFE,
            },
        };
        assert!(value.is_valid());
        if let NativeOutputCapabilities::Available { mix_format, .. } = &mut value {
            mix_format.channels = 0;
        }
        assert!(!value.is_valid());
        assert!(!SpatialAudioCapabilities::Available {
            active_format: None,
            configured_format: None,
            max_dynamic_objects: u32::MAX,
            static_object_mask: 0,
        }
        .is_valid());
        assert!(!NativeOutputCapabilities::Error {
            message: "error\nforged log".into()
        }
        .is_valid());
    }
}
