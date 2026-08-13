//! Narrow interface for the Apple FairPlay `fp-setup` compatibility exchange.
//!
//! AudioHub does not claim the reverse-engineered Apple response tables as its
//! own work. The bundled compatibility records are isolated behind a provider,
//! attributed in `PROVENANCE.md` and called out in the repository `NOTICE.md`.
//! Keeping this boundary explicit prevents an apparently permissive crate
//! license from disguising the provenance and legal status of those bytes.

use std::fmt;
use std::sync::OnceLock;

const MAGIC: &[u8; 4] = b"FPLY";
const VERSION: u8 = 3;
const SETUP_TYPE: u8 = 1;
const PHASE_ONE_LENGTH: usize = 16;
const PHASE_TWO_LENGTH: usize = 164;
const MODE_OFFSET: usize = 14;
const PHASE_TWO_SUFFIX: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FairPlayRequest<'a> {
    PhaseOne { mode: u8 },
    PhaseTwo { suffix: &'a [u8; PHASE_TWO_SUFFIX] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FairPlayError {
    MalformedRequest,
    UnsupportedMode,
    ProviderUnavailable,
    InvalidProviderResponse,
}

impl fmt::Display for FairPlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MalformedRequest => "malformed FairPlay setup request",
            Self::UnsupportedMode => "unsupported FairPlay setup mode",
            Self::ProviderUnavailable => "FairPlay compatibility provider is unavailable",
            Self::InvalidProviderResponse => "FairPlay provider returned an invalid response",
        })
    }
}

impl std::error::Error for FairPlayError {}

pub(crate) trait FairPlayProvider: Send + Sync {
    /// Phase-one responses are exactly 142 bytes and depend on mode 0..=3.
    fn phase_one(&self, mode: u8) -> Result<Vec<u8>, FairPlayError>;

    /// Phase two returns its 12-byte protocol header followed by the supplied
    /// 20-byte suffix, for an exact total of 32 bytes.
    fn phase_two(&self, suffix: &[u8; PHASE_TWO_SUFFIX]) -> Result<Vec<u8>, FairPlayError>;
}

#[derive(Debug, Default)]
pub(crate) struct UnavailableFairPlayProvider;

impl FairPlayProvider for UnavailableFairPlayProvider {
    fn phase_one(&self, _mode: u8) -> Result<Vec<u8>, FairPlayError> {
        Err(FairPlayError::ProviderUnavailable)
    }

    fn phase_two(&self, _suffix: &[u8; PHASE_TWO_SUFFIX]) -> Result<Vec<u8>, FairPlayError> {
        Err(FairPlayError::ProviderUnavailable)
    }
}

/// Compatibility provider backed by the separately attributed opaque records
/// in `fairplay_compat.hex`. Keeping data and parser separate makes the
/// provenance boundary visible in source review and packaging.
#[derive(Debug, Default)]
pub(crate) struct BundledFairPlayProvider;

struct CompatibilityData {
    replies: [[u8; 142]; 4],
    header: [u8; 12],
}

static COMPATIBILITY_DATA: OnceLock<Result<CompatibilityData, FairPlayError>> = OnceLock::new();

impl FairPlayProvider for BundledFairPlayProvider {
    fn phase_one(&self, mode: u8) -> Result<Vec<u8>, FairPlayError> {
        let data = compatibility_data()?;
        data.replies
            .get(usize::from(mode))
            .map(|value| value.to_vec())
            .ok_or(FairPlayError::UnsupportedMode)
    }

    fn phase_two(&self, suffix: &[u8; PHASE_TWO_SUFFIX]) -> Result<Vec<u8>, FairPlayError> {
        let data = compatibility_data()?;
        let mut output = Vec::with_capacity(32);
        output.extend_from_slice(&data.header);
        output.extend_from_slice(suffix);
        Ok(output)
    }
}

fn compatibility_data() -> Result<&'static CompatibilityData, FairPlayError> {
    COMPATIBILITY_DATA
        .get_or_init(|| parse_compatibility_data(include_str!("fairplay_compat.hex")))
        .as_ref()
        .map_err(|error| *error)
}

fn parse_compatibility_data(input: &str) -> Result<CompatibilityData, FairPlayError> {
    let mut values: [Option<Vec<u8>>; 5] = std::array::from_fn(|_| None);
    for line in input.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or(FairPlayError::InvalidProviderResponse)?;
        let index = match name {
            "reply0" => 0,
            "reply1" => 1,
            "reply2" => 2,
            "reply3" => 3,
            "header" => 4,
            _ => return Err(FairPlayError::InvalidProviderResponse),
        };
        if values[index].is_some() {
            return Err(FairPlayError::InvalidProviderResponse);
        }
        values[index] =
            Some(hex::decode(value).map_err(|_| FairPlayError::InvalidProviderResponse)?);
    }
    let mut replies = [[0u8; 142]; 4];
    for (index, reply) in replies.iter_mut().enumerate() {
        *reply = values[index]
            .take()
            .and_then(|value| value.try_into().ok())
            .ok_or(FairPlayError::InvalidProviderResponse)?;
        if reply[..4] != *MAGIC || reply[4] != VERSION || reply[13] != index as u8 {
            return Err(FairPlayError::InvalidProviderResponse);
        }
    }
    let header: [u8; 12] = values[4]
        .take()
        .and_then(|value| value.try_into().ok())
        .ok_or(FairPlayError::InvalidProviderResponse)?;
    if header[..4] != *MAGIC || header[4] != VERSION || header[6] != 4 {
        return Err(FairPlayError::InvalidProviderResponse);
    }
    Ok(CompatibilityData { replies, header })
}

pub(crate) fn parse_request(bytes: &[u8]) -> Result<FairPlayRequest<'_>, FairPlayError> {
    if bytes.len() < 7
        || bytes.get(..4) != Some(MAGIC.as_slice())
        || bytes[4] != VERSION
        || bytes[5] != SETUP_TYPE
    {
        return Err(FairPlayError::MalformedRequest);
    }
    match bytes[6] {
        1 if bytes.len() == PHASE_ONE_LENGTH => {
            let mode = bytes[MODE_OFFSET];
            if mode > 3 {
                return Err(FairPlayError::UnsupportedMode);
            }
            Ok(FairPlayRequest::PhaseOne { mode })
        }
        3 if bytes.len() == PHASE_TWO_LENGTH => {
            let suffix: &[u8; PHASE_TWO_SUFFIX] = bytes[bytes.len() - PHASE_TWO_SUFFIX..]
                .try_into()
                .expect("the exact request length proves the suffix length");
            Ok(FairPlayRequest::PhaseTwo { suffix })
        }
        _ => Err(FairPlayError::MalformedRequest),
    }
}

pub(crate) fn respond(
    provider: &dyn FairPlayProvider,
    request: &[u8],
) -> Result<Vec<u8>, FairPlayError> {
    let response = match parse_request(request)? {
        FairPlayRequest::PhaseOne { mode } => provider.phase_one(mode)?,
        FairPlayRequest::PhaseTwo { suffix } => provider.phase_two(suffix)?,
    };
    let expected = if request[6] == 1 { 142 } else { 32 };
    if response.len() != expected {
        return Err(FairPlayError::InvalidProviderResponse);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct TestProvider;

    impl FairPlayProvider for TestProvider {
        fn phase_one(&self, mode: u8) -> Result<Vec<u8>, FairPlayError> {
            Ok(vec![mode; 142])
        }

        fn phase_two(&self, suffix: &[u8; PHASE_TWO_SUFFIX]) -> Result<Vec<u8>, FairPlayError> {
            let mut output = vec![0; 12];
            output.extend_from_slice(suffix);
            Ok(output)
        }
    }

    fn phase_one(mode: u8) -> [u8; PHASE_ONE_LENGTH] {
        let mut request = [0; PHASE_ONE_LENGTH];
        request[..4].copy_from_slice(MAGIC);
        request[4] = VERSION;
        request[5] = SETUP_TYPE;
        request[6] = 1;
        request[MODE_OFFSET] = mode;
        request
    }

    #[test]
    fn parser_accepts_only_exact_setup_shapes() {
        assert_eq!(
            parse_request(&phase_one(2)).unwrap(),
            FairPlayRequest::PhaseOne { mode: 2 }
        );
        assert_eq!(
            parse_request(&phase_one(9)).unwrap_err(),
            FairPlayError::UnsupportedMode
        );
        let mut trailing = phase_one(1).to_vec();
        trailing.push(0);
        assert_eq!(
            parse_request(&trailing).unwrap_err(),
            FairPlayError::MalformedRequest
        );
    }

    #[test]
    fn phase_two_exposes_only_the_protocol_suffix() {
        let mut request = [0; PHASE_TWO_LENGTH];
        request[..4].copy_from_slice(MAGIC);
        request[4] = VERSION;
        request[5] = SETUP_TYPE;
        request[6] = 3;
        for (offset, value) in request[PHASE_TWO_LENGTH - PHASE_TWO_SUFFIX..]
            .iter_mut()
            .enumerate()
        {
            *value = offset as u8;
        }
        let response = respond(&TestProvider, &request).unwrap();
        assert_eq!(response.len(), 32);
        assert_eq!(&response[12..], &request[144..]);
    }

    #[test]
    fn default_provider_fails_closed_without_apple_material() {
        assert_eq!(
            respond(&UnavailableFairPlayProvider, &phase_one(0)).unwrap_err(),
            FairPlayError::ProviderUnavailable
        );
    }

    #[test]
    fn attributed_compatibility_data_has_exact_shapes() {
        for mode in 0..=3 {
            let response = respond(&BundledFairPlayProvider, &phase_one(mode)).unwrap();
            assert_eq!(response.len(), 142);
            assert_eq!(&response[..4], MAGIC);
            assert_eq!(response[13], mode);
        }
    }

    #[test]
    fn compatibility_data_parser_rejects_missing_duplicate_and_unknown_records() {
        assert!(parse_compatibility_data("reply0=00").is_err());
        assert!(parse_compatibility_data("unknown=00").is_err());
        let duplicated =
            include_str!("fairplay_compat.hex").to_string() + "\nheader=46504c590301040000000014\n";
        assert!(parse_compatibility_data(&duplicated).is_err());
    }

    #[test]
    fn attributed_data_hash_matches_the_provenance_ledger() {
        let hash = Sha256::digest(include_bytes!("fairplay_compat.hex"));
        assert_eq!(
            hex::encode(hash),
            "3cb7cb2ef785458c7f54f1e32907f2e1b6d7d843138f866341fe23221c0642dd"
        );
    }
}
