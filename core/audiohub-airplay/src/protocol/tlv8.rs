//! Bounded TLV8 framing used by HomeKit-style AirPlay pairing messages.
//!
//! TLV8 uses one-byte tags and one-byte lengths. Values longer than 255 bytes
//! are split into adjacent records with the same tag; a record whose length is
//! less than 255 terminates that logical value. The decoder preserves repeated
//! logical fields while joining only those unambiguous continuation records.

use std::error::Error;
use std::fmt;

/// HomeKit pairing's one-byte state field.
pub const STATE_TAG: u8 = 0x06;

/// Resource limits applied before TLV-controlled allocation can grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv8Limits {
    /// Maximum encoded message size, including every tag and length byte.
    pub max_message_bytes: usize,
    /// Maximum number of logical fields after fragment reassembly.
    pub max_fields: usize,
    /// Maximum reassembled value size for any one field.
    pub max_value_bytes: usize,
}

impl Default for Tlv8Limits {
    fn default() -> Self {
        Self {
            max_message_bytes: 64 * 1024,
            max_fields: 128,
            max_value_bytes: 32 * 1024,
        }
    }
}

/// One logical TLV8 field. Its value may span several wire records.
#[derive(Clone, PartialEq, Eq)]
pub struct Tlv8Field {
    tag: u8,
    value: Vec<u8>,
}

impl Tlv8Field {
    pub fn new(tag: u8, value: impl Into<Vec<u8>>) -> Self {
        Self {
            tag,
            value: value.into(),
        }
    }
}

// Pairing TLVs contain private keys and session material, so Debug must never
// print their byte values.
impl fmt::Debug for Tlv8Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tlv8Field")
            .field("tag", &self.tag)
            .field("value_len", &self.value.len())
            .finish()
    }
}

/// A decoded TLV8 message in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv8Message {
    fields: Vec<Tlv8Field>,
}

impl Tlv8Message {
    /// Validate an in-memory field list against the same rules as decoding.
    pub fn from_fields(
        fields: impl IntoIterator<Item = Tlv8Field>,
        limits: Tlv8Limits,
    ) -> Result<Self, Tlv8Error> {
        let fields: Vec<_> = fields.into_iter().collect();
        validate_fields(&fields, limits)?;
        Ok(Self { fields })
    }

    pub fn values(&self, tag: u8) -> impl Iterator<Item = &[u8]> {
        self.fields
            .iter()
            .filter(move |field| field.tag == tag)
            .map(|field| field.value.as_slice())
    }

    /// Return a field only when its logical tag occurs at most once.
    pub fn get_unique(&self, tag: u8) -> Result<Option<&[u8]>, Tlv8Error> {
        let mut values = self.values(tag);
        let first = values.next();
        if values.next().is_some() {
            return Err(Tlv8Error::DuplicateField { tag });
        }
        Ok(first)
    }

    pub fn require_unique(&self, tag: u8) -> Result<&[u8], Tlv8Error> {
        self.get_unique(tag)?
            .ok_or(Tlv8Error::MissingRequiredField { tag })
    }

    /// Return the pairing state. The parser also validates its scalar shape.
    pub fn state(&self) -> Result<Option<u8>, Tlv8Error> {
        Ok(self.get_unique(STATE_TAG)?.map(|value| value[0]))
    }

    /// Encode this logical message, fragmenting values larger than 255 bytes.
    pub fn encode(&self, limits: Tlv8Limits) -> Result<Vec<u8>, Tlv8Error> {
        validate_fields(&self.fields, limits)?;
        let encoded_len = encoded_len(&self.fields)?;
        if encoded_len > limits.max_message_bytes {
            return Err(Tlv8Error::MessageTooLarge {
                actual: encoded_len,
                max: limits.max_message_bytes,
            });
        }

        let mut output = Vec::with_capacity(encoded_len);
        for field in &self.fields {
            if field.value.is_empty() {
                output.extend_from_slice(&[field.tag, 0]);
                continue;
            }
            for chunk in field.value.chunks(u8::MAX as usize) {
                output.push(field.tag);
                output.push(chunk.len() as u8);
                output.extend_from_slice(chunk);
            }
        }
        Ok(output)
    }
}

/// Decode one complete TLV8 message.
pub fn decode(input: &[u8], limits: Tlv8Limits) -> Result<Tlv8Message, Tlv8Error> {
    if input.len() > limits.max_message_bytes {
        return Err(Tlv8Error::MessageTooLarge {
            actual: input.len(),
            max: limits.max_message_bytes,
        });
    }

    let mut fields: Vec<Tlv8Field> = Vec::new();
    let mut offset = 0usize;
    let mut previous_record_len = 0usize;
    let mut state_seen = false;

    while offset < input.len() {
        if input.len() - offset < 2 {
            return Err(Tlv8Error::TruncatedRecordHeader { offset });
        }
        let tag = input[offset];
        let value_len = input[offset + 1] as usize;
        offset += 2;
        if input.len() - offset < value_len {
            return Err(Tlv8Error::TruncatedRecordValue {
                tag,
                declared: value_len,
                remaining: input.len() - offset,
            });
        }

        let is_continuation = previous_record_len == u8::MAX as usize
            && fields.last().is_some_and(|field| field.tag == tag);
        let value = &input[offset..offset + value_len];
        if is_continuation {
            let field = fields.last_mut().expect("continuation requires a field");
            let reassembled = field
                .value
                .len()
                .checked_add(value_len)
                .ok_or(Tlv8Error::LengthOverflow)?;
            if reassembled > limits.max_value_bytes {
                return Err(Tlv8Error::ValueTooLarge {
                    tag,
                    actual: reassembled,
                    max: limits.max_value_bytes,
                });
            }
            field.value.extend_from_slice(value);
        } else {
            if fields.len() == limits.max_fields {
                return Err(Tlv8Error::TooManyFields {
                    max: limits.max_fields,
                });
            }
            if value_len > limits.max_value_bytes {
                return Err(Tlv8Error::ValueTooLarge {
                    tag,
                    actual: value_len,
                    max: limits.max_value_bytes,
                });
            }
            if tag == STATE_TAG {
                if state_seen {
                    return Err(Tlv8Error::DuplicateStateField);
                }
                state_seen = true;
            }
            fields.push(Tlv8Field::new(tag, value));
        }

        offset += value_len;
        previous_record_len = value_len;
    }

    validate_state(&fields)?;
    Ok(Tlv8Message { fields })
}

/// Validate and encode a borrowed field list.
pub fn encode(fields: &[Tlv8Field], limits: Tlv8Limits) -> Result<Vec<u8>, Tlv8Error> {
    Tlv8Message::from_fields(fields.iter().cloned(), limits)?.encode(limits)
}

fn validate_fields(fields: &[Tlv8Field], limits: Tlv8Limits) -> Result<(), Tlv8Error> {
    if fields.len() > limits.max_fields {
        return Err(Tlv8Error::TooManyFields {
            max: limits.max_fields,
        });
    }

    let mut state_seen = false;
    for (index, field) in fields.iter().enumerate() {
        if field.value.len() > limits.max_value_bytes {
            return Err(Tlv8Error::ValueTooLarge {
                tag: field.tag,
                actual: field.value.len(),
                max: limits.max_value_bytes,
            });
        }
        if field.tag == STATE_TAG {
            if state_seen {
                return Err(Tlv8Error::DuplicateStateField);
            }
            state_seen = true;
        }

        // Without a separator, a following record with the same tag is
        // indistinguishable from continuation when this value ends on 255.
        if !field.value.is_empty()
            && field.value.len() % (u8::MAX as usize) == 0
            && fields
                .get(index + 1)
                .is_some_and(|next| next.tag == field.tag)
        {
            return Err(Tlv8Error::AmbiguousAdjacentFields { tag: field.tag });
        }
    }
    validate_state(fields)?;

    let len = encoded_len(fields)?;
    if len > limits.max_message_bytes {
        return Err(Tlv8Error::MessageTooLarge {
            actual: len,
            max: limits.max_message_bytes,
        });
    }
    Ok(())
}

fn validate_state(fields: &[Tlv8Field]) -> Result<(), Tlv8Error> {
    if let Some(state) = fields.iter().find(|field| field.tag == STATE_TAG) {
        if state.value.len() != 1 {
            return Err(Tlv8Error::InvalidStateLength {
                actual: state.value.len(),
            });
        }
    }
    Ok(())
}

fn encoded_len(fields: &[Tlv8Field]) -> Result<usize, Tlv8Error> {
    fields.iter().try_fold(0usize, |total, field| {
        let records = if field.value.is_empty() {
            1
        } else {
            field.value.len().div_ceil(u8::MAX as usize)
        };
        total
            .checked_add(field.value.len())
            .and_then(|value| value.checked_add(records.checked_mul(2)?))
            .ok_or(Tlv8Error::LengthOverflow)
    })
}

/// TLV8 syntax, resource-limit, or pairing-state validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tlv8Error {
    MessageTooLarge {
        actual: usize,
        max: usize,
    },
    TooManyFields {
        max: usize,
    },
    ValueTooLarge {
        tag: u8,
        actual: usize,
        max: usize,
    },
    TruncatedRecordHeader {
        offset: usize,
    },
    TruncatedRecordValue {
        tag: u8,
        declared: usize,
        remaining: usize,
    },
    DuplicateStateField,
    InvalidStateLength {
        actual: usize,
    },
    DuplicateField {
        tag: u8,
    },
    MissingRequiredField {
        tag: u8,
    },
    AmbiguousAdjacentFields {
        tag: u8,
    },
    LengthOverflow,
}

impl fmt::Display for Tlv8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessageTooLarge { actual, max } => {
                write!(f, "TLV8 message is {actual} bytes; limit is {max}")
            }
            Self::TooManyFields { max } => write!(f, "TLV8 message exceeds {max} fields"),
            Self::ValueTooLarge { tag, actual, max } => write!(
                f,
                "TLV8 tag {tag:#04x} is {actual} bytes after reassembly; limit is {max}"
            ),
            Self::TruncatedRecordHeader { offset } => {
                write!(f, "truncated TLV8 record header at byte {offset}")
            }
            Self::TruncatedRecordValue {
                tag,
                declared,
                remaining,
            } => write!(
                f,
                "TLV8 tag {tag:#04x} declares {declared} bytes but only {remaining} remain"
            ),
            Self::DuplicateStateField => write!(f, "TLV8 message contains duplicate state fields"),
            Self::InvalidStateLength { actual } => {
                write!(f, "TLV8 state field must contain one byte, got {actual}")
            }
            Self::DuplicateField { tag } => {
                write!(f, "TLV8 tag {tag:#04x} occurs more than once")
            }
            Self::MissingRequiredField { tag } => {
                write!(f, "required TLV8 tag {tag:#04x} is missing")
            }
            Self::AmbiguousAdjacentFields { tag } => write!(
                f,
                "adjacent TLV8 tag {tag:#04x} fields are ambiguous after a 255-byte fragment"
            ),
            Self::LengthOverflow => write!(f, "TLV8 encoded length overflowed usize"),
        }
    }
}

impl Error for Tlv8Error {}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Tlv8Limits {
        Tlv8Limits {
            max_message_bytes: 4_096,
            max_fields: 8,
            max_value_bytes: 1_024,
        }
    }

    #[test]
    fn fragments_and_reassembles_values_larger_than_255_bytes() {
        let value: Vec<u8> = (0..700).map(|n| (n % 251) as u8).collect();
        let message =
            Tlv8Message::from_fields([Tlv8Field::new(1, value.clone())], limits()).unwrap();
        let encoded = message.encode(limits()).unwrap();

        assert_eq!(encoded[0..2], [1, 255]);
        assert_eq!(encoded[257..259], [1, 255]);
        assert_eq!(encoded[514..516], [1, 190]);
        let decoded = decode(&encoded, limits()).unwrap();
        assert_eq!(decoded.require_unique(1).unwrap(), value);
    }

    #[test]
    fn joins_only_unambiguous_same_tag_continuations() {
        let mut wire = vec![2, 255];
        wire.extend(std::iter::repeat_n(9, 255));
        wire.extend_from_slice(&[2, 2, 7, 8, 2, 1, 6]);

        let decoded = decode(&wire, limits()).unwrap();
        let values: Vec<_> = decoded.values(2).collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].len(), 257);
        assert_eq!(&values[0][255..], &[7, 8]);
        assert_eq!(values[1], &[6]);
        assert_eq!(
            decoded.get_unique(2),
            Err(Tlv8Error::DuplicateField { tag: 2 })
        );
    }

    #[test]
    fn rejects_duplicate_and_non_scalar_state_fields() {
        assert_eq!(
            decode(&[STATE_TAG, 1, 1, STATE_TAG, 1, 2], limits()),
            Err(Tlv8Error::DuplicateStateField)
        );
        assert_eq!(
            decode(&[STATE_TAG, 2, 1, 2], limits()),
            Err(Tlv8Error::InvalidStateLength { actual: 2 })
        );
    }

    #[test]
    fn returns_valid_state_and_supports_empty_values() {
        let decoded = decode(&[STATE_TAG, 1, 3, 10, 0], limits()).unwrap();
        assert_eq!(decoded.state().unwrap(), Some(3));
        assert_eq!(decoded.require_unique(10).unwrap(), &[] as &[u8]);
        assert_eq!(decoded.encode(limits()).unwrap(), [STATE_TAG, 1, 3, 10, 0]);
    }

    #[test]
    fn applies_message_value_and_field_limits() {
        let mut tight = limits();
        tight.max_message_bytes = 3;
        assert_eq!(
            decode(&[1, 2, 3, 4], tight),
            Err(Tlv8Error::MessageTooLarge { actual: 4, max: 3 })
        );

        tight = limits();
        tight.max_value_bytes = 1;
        assert_eq!(
            decode(&[1, 2, 3, 4], tight),
            Err(Tlv8Error::ValueTooLarge {
                tag: 1,
                actual: 2,
                max: 1,
            })
        );

        tight = limits();
        tight.max_fields = 1;
        assert_eq!(
            decode(&[1, 1, 3, 2, 1, 4], tight),
            Err(Tlv8Error::TooManyFields { max: 1 })
        );
    }

    #[test]
    fn cumulative_fragment_limit_is_checked_before_extension() {
        let mut tight = limits();
        tight.max_value_bytes = 255;
        let mut wire = vec![1, 255];
        wire.extend(std::iter::repeat_n(0, 255));
        wire.extend_from_slice(&[1, 1, 0]);
        assert_eq!(
            decode(&wire, tight),
            Err(Tlv8Error::ValueTooLarge {
                tag: 1,
                actual: 256,
                max: 255,
            })
        );
    }

    #[test]
    fn reports_truncated_record_parts() {
        assert_eq!(
            decode(&[1], limits()),
            Err(Tlv8Error::TruncatedRecordHeader { offset: 0 })
        );
        assert_eq!(
            decode(&[1, 3, 8, 9], limits()),
            Err(Tlv8Error::TruncatedRecordValue {
                tag: 1,
                declared: 3,
                remaining: 2,
            })
        );
    }

    #[test]
    fn rejects_ambiguous_adjacent_fields_when_encoding() {
        let fields = [Tlv8Field::new(4, vec![0; 255]), Tlv8Field::new(4, vec![1])];
        assert_eq!(
            encode(&fields, limits()),
            Err(Tlv8Error::AmbiguousAdjacentFields { tag: 4 })
        );
    }

    #[test]
    fn debug_does_not_expose_pairing_material() {
        let field = Tlv8Field::new(3, b"do-not-log-this-secret".to_vec());
        let debug = format!("{field:?}");
        assert!(debug.contains("value_len"));
        assert!(!debug.contains("do-not-log"));
    }
}
