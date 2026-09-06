//! Provider for the separately attributed AirPort Express compatibility key.
//!
//! The embedded PEM is a widely leaked Apple private key copied from the fixed
//! shairport-sync revision recorded in `PROVENANCE.md`. It is not authored or
//! licensed by AudioHub. This module is an explicit provenance boundary, not
//! a claim of clean-room implementation, MFi approval, or legal entitlement.

use super::apple_challenge::{AppleChallengeError, AppleResponseProvider};
use rand::rngs::OsRng;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, Pkcs1v15Sign, RsaPrivateKey};
use std::sync::OnceLock;
use zeroize::Zeroizing;

#[derive(Debug, Default)]
pub(crate) struct BundledAirPortExpressProvider;

static PRIVATE_KEY: OnceLock<Option<RsaPrivateKey>> = OnceLock::new();

fn private_key() -> Option<&'static RsaPrivateKey> {
    PRIVATE_KEY
        .get_or_init(|| {
            RsaPrivateKey::from_pkcs1_pem(include_str!("airport_express_private_key.pem")).ok()
        })
        .as_ref()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassicKeyError {
    ProviderUnavailable,
    InvalidCiphertext,
}

impl std::fmt::Display for ClassicKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ProviderUnavailable => "classic compatibility key provider unavailable",
            Self::InvalidCiphertext => "invalid classic session key",
        })
    }
}

impl std::error::Error for ClassicKeyError {}

pub(crate) trait ClassicKeyProvider: Send + Sync {
    fn unwrap_key(&self, ciphertext: &[u8]) -> Result<Zeroizing<[u8; 16]>, ClassicKeyError>;
}

impl ClassicKeyProvider for BundledAirPortExpressProvider {
    fn unwrap_key(&self, ciphertext: &[u8]) -> Result<Zeroizing<[u8; 16]>, ClassicKeyError> {
        let expected = super::classic::sdp::WRAPPED_KEY_BYTES;
        if ciphertext.len() != expected {
            return Err(ClassicKeyError::InvalidCiphertext);
        }
        let key = private_key().ok_or(ClassicKeyError::ProviderUnavailable)?;
        if key.size() != expected {
            return Err(ClassicKeyError::ProviderUnavailable);
        }
        // Classic RAOP uses OAEP/SHA-1, not AP2's media-key agreement. Keep
        // private operations blinded and plaintext owned by zeroizing storage.
        let plaintext = Zeroizing::new(
            key.decrypt_blinded(&mut OsRng, Oaep::new::<sha1::Sha1>(), ciphertext)
                .map_err(|_| ClassicKeyError::InvalidCiphertext)?,
        );
        let aes_key = plaintext
            .as_slice()
            .try_into()
            .map_err(|_| ClassicKeyError::InvalidCiphertext)?;
        Ok(Zeroizing::new(aes_key))
    }
}

impl AppleResponseProvider for BundledAirPortExpressProvider {
    fn private_pkcs1_v15(&self, input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
        let key = private_key().ok_or(AppleChallengeError::ProviderUnavailable)?;
        key.sign_with_rng(&mut OsRng, Pkcs1v15Sign::new_unprefixed(), input)
            .map_err(|_| AppleChallengeError::InvalidProviderResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use base64::Engine as _;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn classic_key_unwrap_requires_exact_ciphertext_and_plaintext_lengths() {
        let provider = BundledAirPortExpressProvider;
        let public = private_key().unwrap().to_public_key();
        let aes_key = [0x35; 16];
        let ciphertext = public
            .encrypt(&mut OsRng, Oaep::new::<sha1::Sha1>(), &aes_key)
            .unwrap();
        assert_eq!(&*provider.unwrap_key(&ciphertext).unwrap(), &aes_key);
        for size in [0, 15, 17, 32] {
            let ciphertext = public
                .encrypt(&mut OsRng, Oaep::new::<sha1::Sha1>(), &vec![0x42; size])
                .unwrap();
            assert_eq!(
                provider.unwrap_key(&ciphertext).unwrap_err(),
                ClassicKeyError::InvalidCiphertext
            );
        }
        for size in [0, 1, 255, 256, 257] {
            assert_eq!(
                provider.unwrap_key(&vec![0; size]).unwrap_err(),
                ClassicKeyError::InvalidCiphertext
            );
        }
    }

    #[test]
    fn classic_key_unwrap_rejects_another_oaep_hash() {
        let public = private_key().unwrap().to_public_key();
        let ciphertext = public
            .encrypt(&mut OsRng, Oaep::new::<sha2::Sha256>(), &[0x35; 16])
            .unwrap();
        assert_eq!(
            BundledAirPortExpressProvider
                .unwrap_key(&ciphertext)
                .unwrap_err(),
            ClassicKeyError::InvalidCiphertext
        );
    }

    #[test]
    fn fixed_provider_matches_independent_openssl_vector() {
        RsaPrivateKey::from_pkcs1_pem(include_str!("airport_express_private_key.pem"))
            .expect("bundled PKCS#1 PEM must parse");
        let challenge = STANDARD_NO_PAD.encode((0u8..16).collect::<Vec<_>>());
        let prepared = super::super::apple_challenge::prepare(
            Some(challenge.as_bytes()),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
        )
        .unwrap()
        .unwrap();
        let response =
            super::super::apple_challenge::respond(&BundledAirPortExpressProvider, prepared)
                .unwrap();

        // Filled with an OpenSSL EVP_PKEY_sign/PKCS#1 v1.5 result derived
        // independently from the fixed key and the byte layout above.
        assert_eq!(
            response,
            "a1GshT/1CI1CI82oTl71MO/cPcz1/yhlDQRoVeEi6YInWHEdrcoirkpnFzEdHy6zFaxtXWyD/cejnJ/a6kLl1C+U68JG8juH4aOcLzhkfnOEk7BUq29sn4ceBGb9nJ1xLZDIRI6em+OgHKuTcO+Pd+fJm3P+ARuCpQ+5M5gzVHtBb9nc4ZeClJTqHaOJ4r40BfiK1T/io6r3/1Z/p8jEDUtpVfry0Rsoo7jB4wcoKFwHc0ZJ4vJrylMaWYDpJdvyAzYFlG+0PxA7n7y1EOY41bVnNjdheSsSrO9QEpvIbWk50f26fAUK5UoCw6swNanmCK6tEY1Y534xknnXL5pLjA"
        );
    }
}
