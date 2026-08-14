//! Bounded parsers for AirPlay now-playing metadata.
//!
//! AirPlay senders expose two independent metadata paths:
//!
//! - classic `SET_PARAMETER application/x-dmap-tagged`, where an `mlit`
//!   container carries `minm` (title), `asar` (artist), and `asal` (album);
//! - AirPlay 2 `POST /command application/x-apple-binary-plist`, where an
//!   `updateMRNowPlayingInfo` command wraps an `npi-text` dictionary.
//!
//! This module deliberately knows nothing about RTSP sessions or runtime
//! events.  It only validates and normalizes bounded wire bodies.  Callers can
//! therefore acknowledge malformed decorative metadata without letting it
//! affect the audio stream.

use plist::{Dictionary, Value};
use std::fmt;
use std::io::Cursor;

/// Largest classic DMAP body accepted by the parser.
pub(crate) const MAX_DMAP_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Largest rich AirPlay 2 command body accepted by the parser.  The extra
/// envelope headroom permits one [`MAX_ARTWORK_BYTES`] image plus plist tables.
pub(crate) const MAX_NOW_PLAYING_PLIST_BYTES: usize = 5 * 1024 * 1024;
/// Maximum retained cover image.  Only one current image should be kept by the
/// runtime; this parser never keeps an image history.
pub(crate) const MAX_ARTWORK_BYTES: usize = 4 * 1024 * 1024;
/// Metadata strings are UI labels, not arbitrary documents.
pub(crate) const MAX_METADATA_TEXT_BYTES: usize = 4 * 1024;

const MAX_CONTENT_TYPE_BYTES: usize = 128;
const MAX_PLIST_DEPTH: usize = 16;
const MAX_PLIST_NODES: usize = 4096;
const MAX_ARTWORK_DIMENSION: u32 = 8192;
const MAX_ARTWORK_PIXELS: u64 = 25_000_000;
const BINARY_PLIST_MAGIC: &[u8; 8] = b"bplist00";
const MR_TITLE: &str = "kMRMediaRemoteNowPlayingInfoTitle";
const MR_ARTIST: &str = "kMRMediaRemoteNowPlayingInfoArtist";
const MR_ALBUM: &str = "kMRMediaRemoteNowPlayingInfoAlbum";
const MR_DURATION: &str = "kMRMediaRemoteNowPlayingInfoDuration";
const MR_ELAPSED_TIME: &str = "kMRMediaRemoteNowPlayingInfoElapsedTime";
const MR_PLAYBACK_RATE: &str = "kMRMediaRemoteNowPlayingInfoPlaybackRate";
const MR_ARTWORK_DATA: &str = "kMRMediaRemoteNowPlayingInfoArtworkData";
const MR_ARTWORK_MIME: &str = "kMRMediaRemoteNowPlayingInfoArtworkMIMEType";

/// Text fields carried by a classic DMAP `mlit` container.
///
/// A successfully parsed DMAP payload is a complete statement about the
/// current track: an absent field is not inherited from the previous track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrackMetadata {
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
}

/// Merge behavior requested by a rich AirPlay 2 now-playing command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergePolicy {
    /// Missing fields clear the previous now-playing snapshot.
    Replace,
    /// Missing fields leave the previous snapshot unchanged.
    Update,
}

/// Presence-aware field in a rich metadata patch.
///
/// Binary plists have no general-purpose `null` value.  AirPlay uses an empty
/// string/data value (or `image/none`) when it needs to clear a field, so those
/// values normalize to `Clear` while an omitted dictionary key remains
/// `Missing`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum PatchField<T> {
    #[default]
    Missing,
    Clear,
    Value(T),
}

/// A validated image derived from the paired `artworkData` and
/// `artworkMIMEType` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Artwork {
    pub(crate) content_type: String,
    pub(crate) data: Vec<u8>,
}

/// Normalized contents of one `updateMRNowPlayingInfo` command.
///
/// The consumer combines each [`PatchField`] with
/// [`merge_policy`](Self::merge_policy): `Missing` clears on `Replace` and
/// remains untouched on `Update`; `Clear` always clears.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NowPlayingUpdate {
    pub(crate) merge_policy: MergePolicy,
    pub(crate) title: PatchField<String>,
    pub(crate) artist: PatchField<String>,
    pub(crate) album: PatchField<String>,
    /// Seconds, as carried by MediaRemote.
    pub(crate) duration: PatchField<f64>,
    /// Seconds, as carried by MediaRemote.
    pub(crate) elapsed_time: PatchField<f64>,
    pub(crate) playback_rate: PatchField<f64>,
    pub(crate) artwork: PatchField<Artwork>,
}

/// Structural or resource-bound failure while parsing decorative metadata.
/// The error intentionally contains no sender-provided title or artwork bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataParseError {
    BodyTooLarge {
        actual: usize,
        maximum: usize,
    },
    MalformedDmap,
    UnsupportedPlistEncoding,
    MalformedPlist,
    PlistTooDeep {
        maximum: usize,
    },
    PlistTooComplex {
        maximum: usize,
    },
    WrongFieldType {
        field: &'static str,
        expected: &'static str,
    },
    FieldTooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    InvalidNumber {
        field: &'static str,
    },
    UnsupportedArtworkType,
    InvalidArtwork,
}

impl fmt::Display for MetadataParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "metadata body is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::MalformedDmap => formatter.write_str("malformed DMAP metadata"),
            Self::UnsupportedPlistEncoding => {
                formatter.write_str("now-playing command is not a binary plist")
            }
            Self::MalformedPlist => formatter.write_str("malformed now-playing plist"),
            Self::PlistTooDeep { maximum } => {
                write!(formatter, "now-playing plist exceeds depth {maximum}")
            }
            Self::PlistTooComplex { maximum } => {
                write!(formatter, "now-playing plist exceeds {maximum} values")
            }
            Self::WrongFieldType { field, expected } => {
                write!(formatter, "now-playing field {field} is not {expected}")
            }
            Self::FieldTooLarge {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "now-playing field {field} is {actual} bytes; maximum is {maximum}"
            ),
            Self::InvalidNumber { field } => {
                write!(
                    formatter,
                    "now-playing field {field} is not a usable number"
                )
            }
            Self::UnsupportedArtworkType => {
                formatter.write_str("unsupported now-playing artwork type")
            }
            Self::InvalidArtwork => formatter.write_str("invalid now-playing artwork"),
        }
    }
}

impl std::error::Error for MetadataParseError {}

/// Parse a classic AirPlay DMAP payload.
///
/// Unknown tags are ignored, duplicate wanted tags use the last value, and
/// invalid UTF-8 is replaced lossily.  A truncated entry rejects the whole
/// payload rather than publishing a misleading partial track.  `Ok(None)`
/// means that the body contained no `mlit` track container.
pub(crate) fn parse_dmap(body: &[u8]) -> Result<Option<TrackMetadata>, MetadataParseError> {
    enforce_body_limit(body, MAX_DMAP_BODY_BYTES)?;

    let mut metadata = TrackMetadata::default();
    let mut saw_listing_item = false;
    walk_dmap(body, |tag, payload| {
        if tag != *b"mlit" {
            return Ok(());
        }
        saw_listing_item = true;
        walk_dmap(payload, |tag, value| {
            let slot = match &tag {
                b"minm" => &mut metadata.title,
                b"asar" => &mut metadata.artist,
                b"asal" => &mut metadata.album,
                _ => return Ok(()),
            };
            *slot = Some(parse_dmap_text(value)?);
            Ok(())
        })
    })?;

    Ok(saw_listing_item.then_some(metadata))
}

/// Parse a rich AirPlay 2 `POST /command` binary plist.
///
/// Valid commands other than `updateMRNowPlayingInfo`, and recognized command
/// envelopes whose subtype is not `npi-text`, return `Ok(None)`.  Unknown
/// dictionary members are ignored.  Missing `mergePolicy` (and unrecognized
/// policy strings) follow Apple's receiver behavior and mean `Update`.
pub(crate) fn parse_now_playing_command(
    body: &[u8],
) -> Result<Option<NowPlayingUpdate>, MetadataParseError> {
    enforce_body_limit(body, MAX_NOW_PLAYING_PLIST_BYTES)?;
    if !body.starts_with(BINARY_PLIST_MAGIC) {
        return Err(MetadataParseError::UnsupportedPlistEncoding);
    }
    preflight_binary_plist(body)?;

    let value =
        Value::from_reader(Cursor::new(body)).map_err(|_| MetadataParseError::MalformedPlist)?;
    validate_plist_shape(&value)?;

    let command = expect_dictionary(&value, "root")?;
    let Some(command_type) = optional_string(command, "type", "type", MAX_METADATA_TEXT_BYTES)?
    else {
        return Err(MetadataParseError::WrongFieldType {
            field: "type",
            expected: "a string",
        });
    };
    if command_type != "updateMRNowPlayingInfo" {
        return Ok(None);
    }

    let envelope = required_dictionary(command, "params", "params")?;
    if let Some(subtype) =
        optional_string(envelope, "type", "params.type", MAX_METADATA_TEXT_BYTES)?
    {
        if subtype != "npi-text" {
            return Ok(None);
        }
    }
    let fields = required_dictionary(envelope, "params", "params.params")?;

    let merge_policy = match optional_string(
        envelope,
        "mergePolicy",
        "params.mergePolicy",
        MAX_METADATA_TEXT_BYTES,
    )? {
        Some(policy) if policy.eq_ignore_ascii_case("replace") => MergePolicy::Replace,
        _ => MergePolicy::Update,
    };

    Ok(Some(NowPlayingUpdate {
        merge_policy,
        title: patch_string_alias(fields, MR_TITLE, "title", "title")?,
        artist: patch_string_alias(fields, MR_ARTIST, "artist", "artist")?,
        album: patch_string_alias(fields, MR_ALBUM, "album", "album")?,
        duration: patch_nonnegative_number_alias(fields, MR_DURATION, "duration", "duration")?,
        elapsed_time: patch_nonnegative_number_alias(
            fields,
            MR_ELAPSED_TIME,
            "elapsedTime",
            "elapsedTime",
        )?,
        playback_rate: patch_finite_number_alias(
            fields,
            MR_PLAYBACK_RATE,
            "playbackRate",
            "playbackRate",
        )?,
        artwork: parse_artwork(fields)?,
    }))
}

fn preflight_binary_plist(body: &[u8]) -> Result<(), MetadataParseError> {
    const TRAILER_BYTES: usize = 32;
    let trailer_start = body
        .len()
        .checked_sub(TRAILER_BYTES)
        .filter(|start| *start >= BINARY_PLIST_MAGIC.len())
        .ok_or(MetadataParseError::MalformedPlist)?;
    let trailer = &body[trailer_start..];
    let offset_size = trailer[6] as usize;
    let reference_size = trailer[7] as usize;
    if !(1..=8).contains(&offset_size) || !(1..=8).contains(&reference_size) {
        return Err(MetadataParseError::MalformedPlist);
    }
    let object_count = read_be_usize(&trailer[8..16])?;
    if object_count == 0 || object_count > MAX_PLIST_NODES {
        return Err(MetadataParseError::PlistTooComplex {
            maximum: MAX_PLIST_NODES,
        });
    }
    let top_object = read_be_usize(&trailer[16..24])?;
    let offset_table = read_be_usize(&trailer[24..32])?;
    if top_object >= object_count || offset_table < BINARY_PLIST_MAGIC.len() {
        return Err(MetadataParseError::MalformedPlist);
    }
    let offset_bytes = object_count
        .checked_mul(offset_size)
        .ok_or(MetadataParseError::MalformedPlist)?;
    if offset_table
        .checked_add(offset_bytes)
        .filter(|end| *end <= trailer_start)
        .is_none()
    {
        return Err(MetadataParseError::MalformedPlist);
    }

    let mut offsets = Vec::with_capacity(object_count);
    for index in 0..object_count {
        let start = offset_table + index * offset_size;
        let offset = read_be_usize(&body[start..start + offset_size])?;
        if offset < BINARY_PLIST_MAGIC.len() || offset >= offset_table {
            return Err(MetadataParseError::MalformedPlist);
        }
        offsets.push(offset);
    }

    let mut stack = vec![(top_object, 1usize)];
    let mut expanded_nodes = 0usize;
    while let Some((object, depth)) = stack.pop() {
        expanded_nodes = expanded_nodes
            .checked_add(1)
            .ok_or(MetadataParseError::MalformedPlist)?;
        if expanded_nodes > MAX_PLIST_NODES {
            return Err(MetadataParseError::PlistTooComplex {
                maximum: MAX_PLIST_NODES,
            });
        }
        if depth > MAX_PLIST_DEPTH {
            return Err(MetadataParseError::PlistTooDeep {
                maximum: MAX_PLIST_DEPTH,
            });
        }
        let offset = offsets[object];
        let marker = *body.get(offset).ok_or(MetadataParseError::MalformedPlist)?;
        let kind = marker >> 4;
        if !matches!(kind, 0xA | 0xC | 0xD) {
            continue;
        }
        let (count, header_bytes) = binary_plist_object_length(body, offset, marker & 0x0f)?;
        let reference_count = if kind == 0xD {
            count
                .checked_mul(2)
                .ok_or(MetadataParseError::MalformedPlist)?
        } else {
            count
        };
        if reference_count > MAX_PLIST_NODES.saturating_sub(expanded_nodes) {
            return Err(MetadataParseError::PlistTooComplex {
                maximum: MAX_PLIST_NODES,
            });
        }
        let references_start = offset
            .checked_add(header_bytes)
            .ok_or(MetadataParseError::MalformedPlist)?;
        let references_bytes = reference_count
            .checked_mul(reference_size)
            .ok_or(MetadataParseError::MalformedPlist)?;
        if references_start
            .checked_add(references_bytes)
            .filter(|end| *end <= offset_table)
            .is_none()
        {
            return Err(MetadataParseError::MalformedPlist);
        }
        for index in 0..reference_count {
            let start = references_start + index * reference_size;
            let child = read_be_usize(&body[start..start + reference_size])?;
            if child >= object_count {
                return Err(MetadataParseError::MalformedPlist);
            }
            stack.push((child, depth + 1));
        }
    }
    Ok(())
}

fn binary_plist_object_length(
    body: &[u8],
    offset: usize,
    inline: u8,
) -> Result<(usize, usize), MetadataParseError> {
    if inline < 0x0f {
        return Ok((inline as usize, 1));
    }
    let marker = *body
        .get(offset + 1)
        .ok_or(MetadataParseError::MalformedPlist)?;
    if marker >> 4 != 0x1 {
        return Err(MetadataParseError::MalformedPlist);
    }
    let exponent = marker & 0x0f;
    if exponent > 3 {
        return Err(MetadataParseError::MalformedPlist);
    }
    let bytes = 1usize << exponent;
    let start = offset + 2;
    let end = start
        .checked_add(bytes)
        .ok_or(MetadataParseError::MalformedPlist)?;
    let value = read_be_usize(
        body.get(start..end)
            .ok_or(MetadataParseError::MalformedPlist)?,
    )?;
    Ok((value, 2 + bytes))
}

fn read_be_usize(bytes: &[u8]) -> Result<usize, MetadataParseError> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(MetadataParseError::MalformedPlist);
    }
    let mut value = 0u64;
    for byte in bytes {
        value = value
            .checked_shl(8)
            .ok_or(MetadataParseError::MalformedPlist)?
            | u64::from(*byte);
    }
    usize::try_from(value).map_err(|_| MetadataParseError::MalformedPlist)
}

fn enforce_body_limit(body: &[u8], maximum: usize) -> Result<(), MetadataParseError> {
    if body.len() > maximum {
        return Err(MetadataParseError::BodyTooLarge {
            actual: body.len(),
            maximum,
        });
    }
    Ok(())
}

fn walk_dmap(
    mut data: &[u8],
    mut visit: impl FnMut([u8; 4], &[u8]) -> Result<(), MetadataParseError>,
) -> Result<(), MetadataParseError> {
    while !data.is_empty() {
        let header = data.get(..8).ok_or(MetadataParseError::MalformedDmap)?;
        let tag: [u8; 4] = header[..4]
            .try_into()
            .map_err(|_| MetadataParseError::MalformedDmap)?;
        let length = u32::from_be_bytes(
            header[4..8]
                .try_into()
                .map_err(|_| MetadataParseError::MalformedDmap)?,
        ) as usize;
        let rest = &data[8..];
        let payload = rest
            .get(..length)
            .ok_or(MetadataParseError::MalformedDmap)?;
        visit(tag, payload)?;
        data = &rest[length..];
    }
    Ok(())
}

fn parse_dmap_text(bytes: &[u8]) -> Result<String, MetadataParseError> {
    if bytes.len() > MAX_METADATA_TEXT_BYTES {
        return Err(MetadataParseError::FieldTooLarge {
            field: "DMAP text",
            actual: bytes.len(),
            maximum: MAX_METADATA_TEXT_BYTES,
        });
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn validate_plist_shape(root: &Value) -> Result<(), MetadataParseError> {
    let mut values = vec![(root, 1usize)];
    let mut visited = 0usize;
    while let Some((value, depth)) = values.pop() {
        if depth > MAX_PLIST_DEPTH {
            return Err(MetadataParseError::PlistTooDeep {
                maximum: MAX_PLIST_DEPTH,
            });
        }
        visited = visited.saturating_add(1);
        if visited > MAX_PLIST_NODES {
            return Err(MetadataParseError::PlistTooComplex {
                maximum: MAX_PLIST_NODES,
            });
        }
        match value {
            Value::Array(array) => {
                values.extend(array.iter().map(|value| (value, depth + 1)));
            }
            Value::Dictionary(dictionary) => {
                values.extend(dictionary.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn expect_dictionary<'a>(
    value: &'a Value,
    field: &'static str,
) -> Result<&'a Dictionary, MetadataParseError> {
    value
        .as_dictionary()
        .ok_or(MetadataParseError::WrongFieldType {
            field,
            expected: "a dictionary",
        })
}

fn required_dictionary<'a>(
    dictionary: &'a Dictionary,
    key: &str,
    field: &'static str,
) -> Result<&'a Dictionary, MetadataParseError> {
    dictionary
        .get(key)
        .and_then(Value::as_dictionary)
        .ok_or(MetadataParseError::WrongFieldType {
            field,
            expected: "a dictionary",
        })
}

fn optional_string(
    dictionary: &Dictionary,
    key: &str,
    field: &'static str,
    maximum: usize,
) -> Result<Option<String>, MetadataParseError> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    let string = value
        .as_string()
        .ok_or(MetadataParseError::WrongFieldType {
            field,
            expected: "a string",
        })?;
    if string.len() > maximum {
        return Err(MetadataParseError::FieldTooLarge {
            field,
            actual: string.len(),
            maximum,
        });
    }
    Ok(Some(string.to_owned()))
}

fn aliased_value<'a>(
    dictionary: &'a Dictionary,
    canonical: &str,
    compatibility: &str,
) -> Option<&'a Value> {
    dictionary
        .get(canonical)
        .or_else(|| dictionary.get(compatibility))
}

fn patch_string_alias(
    dictionary: &Dictionary,
    canonical: &str,
    compatibility: &str,
    field: &'static str,
) -> Result<PatchField<String>, MetadataParseError> {
    let Some(value) = aliased_value(dictionary, canonical, compatibility) else {
        return Ok(PatchField::Missing);
    };
    let string = value
        .as_string()
        .ok_or(MetadataParseError::WrongFieldType {
            field,
            expected: "a string",
        })?;
    if string.len() > MAX_METADATA_TEXT_BYTES {
        return Err(MetadataParseError::FieldTooLarge {
            field,
            actual: string.len(),
            maximum: MAX_METADATA_TEXT_BYTES,
        });
    }
    Ok(if string.is_empty() {
        PatchField::Clear
    } else {
        PatchField::Value(string.to_owned())
    })
}

fn aliased_finite_number(
    dictionary: &Dictionary,
    canonical: &str,
    compatibility: &str,
    field: &'static str,
) -> Result<Option<f64>, MetadataParseError> {
    let Some(value) = aliased_value(dictionary, canonical, compatibility) else {
        return Ok(None);
    };
    let number = match value {
        Value::Real(number) => *number,
        Value::Integer(integer) => integer
            .as_signed()
            .map(|number| number as f64)
            .or_else(|| integer.as_unsigned().map(|number| number as f64))
            .ok_or(MetadataParseError::InvalidNumber { field })?,
        _ => {
            return Err(MetadataParseError::WrongFieldType {
                field,
                expected: "a number",
            })
        }
    };
    if !number.is_finite() {
        return Err(MetadataParseError::InvalidNumber { field });
    }
    Ok(Some(number))
}

fn patch_finite_number_alias(
    dictionary: &Dictionary,
    canonical: &str,
    compatibility: &str,
    field: &'static str,
) -> Result<PatchField<f64>, MetadataParseError> {
    Ok(
        match aliased_finite_number(dictionary, canonical, compatibility, field)? {
            None => PatchField::Missing,
            Some(number) => PatchField::Value(number),
        },
    )
}

fn patch_nonnegative_number_alias(
    dictionary: &Dictionary,
    canonical: &str,
    compatibility: &str,
    field: &'static str,
) -> Result<PatchField<f64>, MetadataParseError> {
    let number = aliased_finite_number(dictionary, canonical, compatibility, field)?;
    if number.is_some_and(|number| number < 0.0) {
        return Err(MetadataParseError::InvalidNumber { field });
    }
    Ok(match number {
        None => PatchField::Missing,
        Some(number) => PatchField::Value(number),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtworkMime {
    Jpeg,
    Png,
    None,
}

fn parse_artwork(fields: &Dictionary) -> Result<PatchField<Artwork>, MetadataParseError> {
    let data = match aliased_value(fields, MR_ARTWORK_DATA, "artworkData") {
        None => None,
        Some(Value::Data(data)) => {
            if data.len() > MAX_ARTWORK_BYTES {
                return Err(MetadataParseError::FieldTooLarge {
                    field: "artworkData",
                    actual: data.len(),
                    maximum: MAX_ARTWORK_BYTES,
                });
            }
            Some(data.as_slice())
        }
        Some(_) => {
            return Err(MetadataParseError::WrongFieldType {
                field: "artworkData",
                expected: "data",
            })
        }
    };
    let mime = match patch_string_alias(
        fields,
        MR_ARTWORK_MIME,
        "artworkMIMEType",
        "artworkMIMEType",
    )? {
        PatchField::Missing => None,
        PatchField::Clear => Some(ArtworkMime::None),
        PatchField::Value(content_type) if content_type.len() <= MAX_CONTENT_TYPE_BYTES => {
            Some(parse_artwork_mime(&content_type)?)
        }
        PatchField::Value(content_type) => {
            return Err(MetadataParseError::FieldTooLarge {
                field: "artworkMIMEType",
                actual: content_type.len(),
                maximum: MAX_CONTENT_TYPE_BYTES,
            })
        }
    };

    match (data, mime) {
        (None, None) => Ok(PatchField::Missing),
        (None, Some(ArtworkMime::None)) | (Some([]), _) => Ok(PatchField::Clear),
        (Some(data), Some(ArtworkMime::None)) => {
            let _ = data;
            Err(MetadataParseError::InvalidArtwork)
        }
        (None, Some(ArtworkMime::Jpeg | ArtworkMime::Png)) => {
            Err(MetadataParseError::InvalidArtwork)
        }
        (Some(data), Some(format)) => build_artwork(format, data).map(PatchField::Value),
        (Some(data), None) => {
            let format = infer_artwork_mime(data).ok_or(MetadataParseError::InvalidArtwork)?;
            build_artwork(format, data).map(PatchField::Value)
        }
    }
}

fn parse_artwork_mime(content_type: &str) -> Result<ArtworkMime, MetadataParseError> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if media_type.eq_ignore_ascii_case("image/jpeg") || media_type.eq_ignore_ascii_case("image/jpg")
    {
        Ok(ArtworkMime::Jpeg)
    } else if media_type.eq_ignore_ascii_case("image/png") {
        Ok(ArtworkMime::Png)
    } else if media_type.eq_ignore_ascii_case("image/none") {
        Ok(ArtworkMime::None)
    } else {
        Err(MetadataParseError::UnsupportedArtworkType)
    }
}

fn infer_artwork_mime(data: &[u8]) -> Option<ArtworkMime> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ArtworkMime::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ArtworkMime::Png)
    } else {
        None
    }
}

fn build_artwork(format: ArtworkMime, data: &[u8]) -> Result<Artwork, MetadataParseError> {
    let (expected, content_type) = match format {
        ArtworkMime::Jpeg => (ArtworkMime::Jpeg, "image/jpeg"),
        ArtworkMime::Png => (ArtworkMime::Png, "image/png"),
        ArtworkMime::None => return Err(MetadataParseError::InvalidArtwork),
    };
    if infer_artwork_mime(data) != Some(expected) {
        return Err(MetadataParseError::InvalidArtwork);
    }
    let (width, height) = match format {
        ArtworkMime::Jpeg => jpeg_dimensions(data),
        ArtworkMime::Png => png_dimensions(data),
        ArtworkMime::None => None,
    }
    .ok_or(MetadataParseError::InvalidArtwork)?;
    if width == 0
        || height == 0
        || width > MAX_ARTWORK_DIMENSION
        || height > MAX_ARTWORK_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_ARTWORK_PIXELS
    {
        return Err(MetadataParseError::InvalidArtwork);
    }
    Ok(Artwork {
        content_type: content_type.to_owned(),
        data: data.to_vec(),
    })
}

pub(crate) fn parse_artwork_bytes(
    content_type: &str,
    data: &[u8],
) -> Result<Artwork, MetadataParseError> {
    if data.len() > MAX_ARTWORK_BYTES {
        return Err(MetadataParseError::BodyTooLarge {
            actual: data.len(),
            maximum: MAX_ARTWORK_BYTES,
        });
    }
    build_artwork(parse_artwork_mime(content_type)?, data)
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24
        || !data.starts_with(b"\x89PNG\r\n\x1a\n")
        || data.get(8..12)? != 13u32.to_be_bytes()
        || data.get(12..16)? != b"IHDR"
    {
        return None;
    }
    Some((
        u32::from_be_bytes(data.get(16..20)?.try_into().ok()?),
        u32::from_be_bytes(data.get(20..24)?.try_into().ok()?),
    ))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut cursor = 2usize;
    while cursor < data.len() {
        while data.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let marker = *data.get(cursor)?;
        cursor += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let length = u16::from_be_bytes(data.get(cursor..cursor + 2)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let payload = cursor + 2;
        let end = cursor.checked_add(length)?;
        if end > data.len() {
            return None;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 8 {
                return None;
            }
            let height = u16::from_be_bytes(data.get(payload + 1..payload + 3)?.try_into().ok()?);
            let width = u16::from_be_bytes(data.get(payload + 3..payload + 5)?.try_into().ok()?);
            return Some((u32::from(width), u32::from(height)));
        }
        cursor = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dmap_entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut entry = Vec::with_capacity(8 + payload.len());
        entry.extend_from_slice(tag);
        entry.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        entry.extend_from_slice(payload);
        entry
    }

    fn dmap_listing(entries: &[Vec<u8>]) -> Vec<u8> {
        dmap_entry(b"mlit", &entries.concat())
    }

    fn now_playing_plist(
        fields: Dictionary,
        merge_policy: Option<Value>,
        subtype: Option<Value>,
    ) -> Vec<u8> {
        let mut envelope = Dictionary::new();
        if let Some(subtype) = subtype {
            envelope.insert("type".into(), subtype);
        }
        envelope.insert("params".into(), Value::Dictionary(fields));
        if let Some(policy) = merge_policy {
            envelope.insert("mergePolicy".into(), policy);
        }

        let mut command = Dictionary::new();
        command.insert(
            "type".into(),
            Value::String("updateMRNowPlayingInfo".into()),
        );
        command.insert("params".into(), Value::Dictionary(envelope));
        binary_plist(Value::Dictionary(command))
    }

    fn binary_plist(value: Value) -> Vec<u8> {
        let mut body = Vec::new();
        value.to_writer_binary(&mut body).unwrap();
        body
    }

    fn repeated_reference_plist(reference_count: usize) -> Vec<u8> {
        assert!(reference_count <= u16::MAX as usize);
        let mut body = BINARY_PLIST_MAGIC.to_vec();
        let array_offset = body.len();
        body.extend_from_slice(&[0xaf, 0x11]);
        body.extend_from_slice(&(reference_count as u16).to_be_bytes());
        body.extend(std::iter::repeat(1u8).take(reference_count));
        let scalar_offset = body.len();
        body.push(0x08); // object 1: false

        let offset_table = body.len();
        body.extend_from_slice(&(array_offset as u16).to_be_bytes());
        body.extend_from_slice(&(scalar_offset as u16).to_be_bytes());
        let mut trailer = [0u8; 32];
        trailer[6] = 2; // two-byte object offsets
        trailer[7] = 1; // one-byte object references
        trailer[8..16].copy_from_slice(&2u64.to_be_bytes());
        trailer[16..24].copy_from_slice(&0u64.to_be_bytes());
        trailer[24..32].copy_from_slice(&(offset_table as u64).to_be_bytes());
        body.extend_from_slice(&trailer);
        body
    }

    fn jpeg() -> Vec<u8> {
        vec![
            0xff, 0xd8, // SOI
            0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x02, 0x00, 0x03, 0x03, 0x01, 0x11, 0x00, 0x02,
            0x11, 0x00, 0x03, 0x11, 0x00, // 3x2 baseline SOF
            0xff, 0xd9,
        ]
    }

    fn png() -> Vec<u8> {
        let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
        image.extend_from_slice(&13u32.to_be_bytes());
        image.extend_from_slice(b"IHDR");
        image.extend_from_slice(&3u32.to_be_bytes());
        image.extend_from_slice(&2u32.to_be_bytes());
        image.extend_from_slice(&[8, 6, 0, 0, 0]);
        image
    }

    #[test]
    fn parses_classic_dmap_track_and_ignores_unknown_tags() {
        let body = dmap_listing(&[
            dmap_entry(b"astm", &180_000u32.to_be_bytes()),
            dmap_entry(b"minm", b"Track"),
            dmap_entry(b"asar", "Artist".as_bytes()),
            dmap_entry(b"asal", "Album".as_bytes()),
        ]);

        assert_eq!(
            parse_dmap(&body).unwrap(),
            Some(TrackMetadata {
                title: Some("Track".into()),
                artist: Some("Artist".into()),
                album: Some("Album".into()),
            })
        );
    }

    #[test]
    fn dmap_is_complete_last_value_wins_and_invalid_utf8_is_lossy() {
        let body = dmap_listing(&[
            dmap_entry(b"minm", b"old"),
            dmap_entry(b"minm", &[0xff, b'N']),
        ]);
        let metadata = parse_dmap(&body).unwrap().unwrap();
        assert_eq!(metadata.title.as_deref(), Some("\u{fffd}N"));
        assert_eq!(metadata.artist, None);
        assert_eq!(metadata.album, None);
    }

    #[test]
    fn dmap_without_listing_item_is_not_a_track() {
        assert_eq!(parse_dmap(&dmap_entry(b"minm", b"loose")).unwrap(), None);
        assert_eq!(parse_dmap(&[]).unwrap(), None);
    }

    #[test]
    fn rejects_truncated_or_oversized_dmap_without_partial_output() {
        let mut truncated = dmap_listing(&[dmap_entry(b"minm", b"Track")]);
        truncated.extend_from_slice(b"asal\0\0\0\x10short");
        assert_eq!(
            parse_dmap(&truncated),
            Err(MetadataParseError::MalformedDmap)
        );

        let oversized = vec![0; MAX_DMAP_BODY_BYTES + 1];
        assert_eq!(
            parse_dmap(&oversized),
            Err(MetadataParseError::BodyTooLarge {
                actual: MAX_DMAP_BODY_BYTES + 1,
                maximum: MAX_DMAP_BODY_BYTES,
            })
        );
    }

    #[test]
    fn rejects_oversized_dmap_text() {
        let body = dmap_listing(&[dmap_entry(
            b"minm",
            &vec![b'x'; MAX_METADATA_TEXT_BYTES + 1],
        )]);
        assert_eq!(
            parse_dmap(&body),
            Err(MetadataParseError::FieldTooLarge {
                field: "DMAP text",
                actual: MAX_METADATA_TEXT_BYTES + 1,
                maximum: MAX_METADATA_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn accepts_observed_large_classic_dmap_body() {
        const OBSERVED_BODY_BYTES: usize = 308_060;
        let title = dmap_entry(b"minm", b"Track");
        let unknown_header_bytes = 8usize;
        let listing_header_bytes = 8usize;
        let padding =
            OBSERVED_BODY_BYTES - listing_header_bytes - title.len() - unknown_header_bytes;
        let body = dmap_listing(&[title, dmap_entry(b"zzzz", &vec![0; padding])]);
        assert_eq!(body.len(), OBSERVED_BODY_BYTES);
        assert_eq!(
            parse_dmap(&body).unwrap().unwrap().title.as_deref(),
            Some("Track")
        );
    }

    #[test]
    fn parses_all_rich_fields_and_replace_policy() {
        let mut fields = Dictionary::new();
        fields.insert("title".into(), Value::String("Track".into()));
        fields.insert("artist".into(), Value::String("Artist".into()));
        fields.insert("album".into(), Value::String("Album".into()));
        fields.insert("duration".into(), Value::Real(245.5));
        fields.insert("elapsedTime".into(), Value::Integer(12u64.into()));
        fields.insert("playbackRate".into(), Value::Integer(1i64.into()));
        fields.insert("artworkData".into(), Value::Data(jpeg()));
        fields.insert(
            "artworkMIMEType".into(),
            Value::String("IMAGE/JPG; sender=ios".into()),
        );
        fields.insert("ignored".into(), Value::String("safe to skip".into()));

        let update = parse_now_playing_command(&now_playing_plist(
            fields,
            Some(Value::String("replace".into())),
            Some(Value::String("npi-text".into())),
        ))
        .unwrap()
        .unwrap();

        assert_eq!(update.merge_policy, MergePolicy::Replace);
        assert_eq!(update.title, PatchField::Value("Track".into()));
        assert_eq!(update.artist, PatchField::Value("Artist".into()));
        assert_eq!(update.album, PatchField::Value("Album".into()));
        assert_eq!(update.duration, PatchField::Value(245.5));
        assert_eq!(update.elapsed_time, PatchField::Value(12.0));
        assert_eq!(update.playback_rate, PatchField::Value(1.0));
        assert_eq!(
            update.artwork,
            PatchField::Value(Artwork {
                content_type: "image/jpeg".into(),
                data: jpeg(),
            })
        );
    }

    #[test]
    fn parses_mediaremote_canonical_keys_and_prefers_them_over_short_aliases() {
        let mut fields = Dictionary::new();
        fields.insert("title".into(), Value::String("compat title".into()));
        fields.insert(MR_TITLE.into(), Value::String("Canonical Track".into()));
        fields.insert(MR_ARTIST.into(), Value::String("Canonical Artist".into()));
        fields.insert(MR_ALBUM.into(), Value::String("Canonical Album".into()));
        fields.insert(MR_DURATION.into(), Value::Real(210.25));
        fields.insert(MR_ELAPSED_TIME.into(), Value::Real(12.5));
        fields.insert(MR_PLAYBACK_RATE.into(), Value::Real(1.0));
        fields.insert(MR_ARTWORK_DATA.into(), Value::Data(jpeg()));
        fields.insert(MR_ARTWORK_MIME.into(), Value::String("image/jpeg".into()));

        let update = parse_now_playing_command(&now_playing_plist(fields, None, None))
            .unwrap()
            .unwrap();
        assert_eq!(update.title, PatchField::Value("Canonical Track".into()));
        assert_eq!(update.artist, PatchField::Value("Canonical Artist".into()));
        assert_eq!(update.album, PatchField::Value("Canonical Album".into()));
        assert_eq!(update.duration, PatchField::Value(210.25));
        assert_eq!(update.elapsed_time, PatchField::Value(12.5));
        assert_eq!(update.playback_rate, PatchField::Value(1.0));
        assert!(matches!(update.artwork, PatchField::Value(_)));
    }

    #[test]
    fn defaults_missing_or_unknown_merge_policy_to_update() {
        let empty = Dictionary::new();
        let missing = parse_now_playing_command(&now_playing_plist(empty.clone(), None, None))
            .unwrap()
            .unwrap();
        assert_eq!(missing.merge_policy, MergePolicy::Update);
        assert_eq!(missing.title, PatchField::Missing);
        assert_eq!(missing.artwork, PatchField::Missing);

        let unknown = parse_now_playing_command(&now_playing_plist(
            empty,
            Some(Value::String("future-policy".into())),
            Some(Value::String("npi-text".into())),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(unknown.merge_policy, MergePolicy::Update);
    }

    #[test]
    fn preserves_missing_clear_and_value_field_states() {
        let mut fields = Dictionary::new();
        fields.insert("title".into(), Value::String(String::new()));
        fields.insert("artist".into(), Value::String("Artist".into()));

        let update = parse_now_playing_command(&now_playing_plist(
            fields,
            Some(Value::String("update".into())),
            Some(Value::String("npi-text".into())),
        ))
        .unwrap()
        .unwrap();

        assert_eq!(update.title, PatchField::Clear);
        assert_eq!(update.artist, PatchField::Value("Artist".into()));
        assert_eq!(update.album, PatchField::Missing);
        assert_eq!(update.duration, PatchField::Missing);
        assert_eq!(update.artwork, PatchField::Missing);
    }

    #[test]
    fn ignores_other_commands_and_now_playing_subtypes() {
        let mut other = Dictionary::new();
        other.insert(
            "type".into(),
            Value::String("updateMRSupportedCommands".into()),
        );
        assert_eq!(
            parse_now_playing_command(&binary_plist(Value::Dictionary(other))).unwrap(),
            None
        );

        let body = now_playing_plist(
            Dictionary::new(),
            None,
            Some(Value::String("npi-future".into())),
        );
        assert_eq!(parse_now_playing_command(&body).unwrap(), None);
    }

    #[test]
    fn artwork_can_be_inferred_cleared_and_parsed_as_png() {
        let mut inferred = Dictionary::new();
        inferred.insert("artworkData".into(), Value::Data(jpeg()));
        let update = parse_now_playing_command(&now_playing_plist(inferred, None, None))
            .unwrap()
            .unwrap();
        assert!(matches!(
            update.artwork,
            PatchField::Value(Artwork { ref content_type, .. }) if content_type == "image/jpeg"
        ));

        let mut clear = Dictionary::new();
        clear.insert("artworkData".into(), Value::Data(Vec::new()));
        clear.insert("artworkMIMEType".into(), Value::String("image/none".into()));
        let update = parse_now_playing_command(&now_playing_plist(clear, None, None))
            .unwrap()
            .unwrap();
        assert_eq!(update.artwork, PatchField::Clear);

        let mut png_fields = Dictionary::new();
        png_fields.insert("artworkData".into(), Value::Data(png()));
        png_fields.insert("artworkMIMEType".into(), Value::String("image/png".into()));
        let update = parse_now_playing_command(&now_playing_plist(png_fields, None, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            update.artwork,
            PatchField::Value(Artwork {
                content_type: "image/png".into(),
                data: png(),
            })
        );
    }

    #[test]
    fn accepts_observed_multi_megabyte_png_artwork() {
        const OBSERVED_ARTWORK_BYTES: usize = 2_393_210;
        let mut image = png();
        image.resize(OBSERVED_ARTWORK_BYTES, 0);
        let mut fields = Dictionary::new();
        fields.insert("artworkData".into(), Value::Data(image.clone()));
        fields.insert("artworkMIMEType".into(), Value::String("image/png".into()));

        let update = parse_now_playing_command(&now_playing_plist(fields, None, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            update.artwork,
            PatchField::Value(Artwork {
                content_type: "image/png".into(),
                data: image,
            })
        );
    }

    #[test]
    fn rejects_wrong_known_field_types_and_unusable_numbers() {
        let mut wrong_title = Dictionary::new();
        wrong_title.insert("title".into(), Value::Integer(1u64.into()));
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(wrong_title, None, None)),
            Err(MetadataParseError::WrongFieldType {
                field: "title",
                expected: "a string",
            })
        );

        let mut negative_duration = Dictionary::new();
        negative_duration.insert("duration".into(), Value::Real(-1.0));
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(negative_duration, None, None)),
            Err(MetadataParseError::InvalidNumber { field: "duration" })
        );

        let mut nan_rate = Dictionary::new();
        nan_rate.insert("playbackRate".into(), Value::Real(f64::NAN));
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(nan_rate, None, None)),
            Err(MetadataParseError::InvalidNumber {
                field: "playbackRate",
            })
        );
    }

    #[test]
    fn rejects_untrusted_artwork_type_magic_and_size() {
        let mut unsupported = Dictionary::new();
        unsupported.insert("artworkData".into(), Value::Data(jpeg()));
        unsupported.insert(
            "artworkMIMEType".into(),
            Value::String("image/svg+xml".into()),
        );
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(unsupported, None, None)),
            Err(MetadataParseError::UnsupportedArtworkType)
        );

        let mut mismatch = Dictionary::new();
        mismatch.insert("artworkData".into(), Value::Data(png()));
        mismatch.insert("artworkMIMEType".into(), Value::String("image/jpeg".into()));
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(mismatch, None, None)),
            Err(MetadataParseError::InvalidArtwork)
        );

        let mut oversized = Dictionary::new();
        oversized.insert(
            "artworkData".into(),
            Value::Data(vec![0; MAX_ARTWORK_BYTES + 1]),
        );
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(oversized, None, None)),
            Err(MetadataParseError::FieldTooLarge {
                field: "artworkData",
                actual: MAX_ARTWORK_BYTES + 1,
                maximum: MAX_ARTWORK_BYTES,
            })
        );

        let mut dimension_bomb = png();
        dimension_bomb[16..20].copy_from_slice(&8_192u32.to_be_bytes());
        dimension_bomb[20..24].copy_from_slice(&8_192u32.to_be_bytes());
        assert_eq!(
            parse_artwork_bytes("image/png", &dimension_bomb),
            Err(MetadataParseError::InvalidArtwork)
        );

        let mut oversized_dimension = jpeg();
        oversized_dimension[9..11].copy_from_slice(&8_193u16.to_be_bytes());
        assert_eq!(
            parse_artwork_bytes("image/jpeg", &oversized_dimension),
            Err(MetadataParseError::InvalidArtwork)
        );
    }

    #[test]
    fn rejects_non_binary_malformed_and_oversized_plists() {
        assert_eq!(
            parse_now_playing_command(b"<?xml version=\"1.0\"?><plist/>"),
            Err(MetadataParseError::UnsupportedPlistEncoding)
        );
        assert_eq!(
            parse_now_playing_command(b"bplist00broken"),
            Err(MetadataParseError::MalformedPlist)
        );
        let oversized = vec![0; MAX_NOW_PLAYING_PLIST_BYTES + 1];
        assert_eq!(
            parse_now_playing_command(&oversized),
            Err(MetadataParseError::BodyTooLarge {
                actual: MAX_NOW_PLAYING_PLIST_BYTES + 1,
                maximum: MAX_NOW_PLAYING_PLIST_BYTES,
            })
        );
    }

    #[test]
    fn rejects_excessive_plist_depth_before_field_extraction() {
        let mut nested = Value::String("leaf".into());
        for _ in 0..=MAX_PLIST_DEPTH {
            nested = Value::Array(vec![nested]);
        }
        let mut fields = Dictionary::new();
        fields.insert("ignored".into(), nested);
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(fields, None, None)),
            Err(MetadataParseError::PlistTooDeep {
                maximum: MAX_PLIST_DEPTH,
            })
        );
    }

    #[test]
    fn rejects_excessive_plist_node_count() {
        let mut fields = Dictionary::new();
        fields.insert(
            "ignored".into(),
            Value::Array(
                (0..MAX_PLIST_NODES)
                    .map(|_| Value::Boolean(false))
                    .collect(),
            ),
        );
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(fields, None, None)),
            Err(MetadataParseError::PlistTooComplex {
                maximum: MAX_PLIST_NODES,
            })
        );
    }

    #[test]
    fn rejects_repeated_reference_amplification_before_decode() {
        let body = repeated_reference_plist(MAX_PLIST_NODES + 1);
        assert_eq!(
            parse_now_playing_command(&body),
            Err(MetadataParseError::PlistTooComplex {
                maximum: MAX_PLIST_NODES,
            })
        );
    }

    #[test]
    fn rejects_oversized_text_and_structural_type_confusion() {
        let mut fields = Dictionary::new();
        fields.insert(
            "title".into(),
            Value::String("x".repeat(MAX_METADATA_TEXT_BYTES + 1)),
        );
        assert_eq!(
            parse_now_playing_command(&now_playing_plist(fields, None, None)),
            Err(MetadataParseError::FieldTooLarge {
                field: "title",
                actual: MAX_METADATA_TEXT_BYTES + 1,
                maximum: MAX_METADATA_TEXT_BYTES,
            })
        );

        let mut command = Dictionary::new();
        command.insert(
            "type".into(),
            Value::String("updateMRNowPlayingInfo".into()),
        );
        command.insert("params".into(), Value::Array(Vec::new()));
        assert_eq!(
            parse_now_playing_command(&binary_plist(Value::Dictionary(command))),
            Err(MetadataParseError::WrongFieldType {
                field: "params",
                expected: "a dictionary",
            })
        );
    }
}
