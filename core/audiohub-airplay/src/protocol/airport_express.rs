//! Provider for the separately attributed AirPort Express compatibility key.
//!
//! The embedded PEM is a widely leaked Apple private key copied from the fixed
//! shairport-sync revision recorded in `PROVENANCE.md`. It is not authored or
//! licensed by AudioHub. This module is an explicit provenance boundary, not
//! a claim of clean-room implementation, MFi approval, or legal entitlement.

use super::apple_challenge::{AppleChallengeError, AppleResponseProvider};
use rand::rngs::OsRng;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use std::sync::OnceLock;

#[derive(Debug, Default)]
pub(crate) struct BundledAirPortExpressProvider;

static PRIVATE_KEY: OnceLock<Option<RsaPrivateKey>> = OnceLock::new();

impl AppleResponseProvider for BundledAirPortExpressProvider {
    fn private_pkcs1_v15(&self, input: &[u8]) -> Result<Vec<u8>, AppleChallengeError> {
        let key = PRIVATE_KEY
            .get_or_init(|| {
                RsaPrivateKey::from_pkcs1_pem(include_str!("airport_express_private_key.pem")).ok()
            })
            .as_ref()
            .ok_or(AppleChallengeError::ProviderUnavailable)?;
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
