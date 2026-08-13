//! HomeKit transient Pair-Setup primitives used by AirPlay 2.
//!
//! This module implements only the receiver side of the SRP exchange. Random
//! material is supplied by the caller so the protocol layer never chooses a
//! platform RNG or accidentally reuses a salt/server exponent across sessions.

use num_bigint::BigUint;
use sha2::{Digest, Sha512};
use std::error::Error;
use std::fmt;
use std::sync::OnceLock;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

/// The fixed SRP identity used by HomeKit Pair-Setup.
pub const PAIR_SETUP_USERNAME: &[u8] = b"Pair-Setup";

const GROUP_BYTES: usize = 384;
const PROOF_BYTES: usize = 64;

// RFC 5054, Appendix A, 3072-bit group. This public group parameter is
// reproduced from the RFC rather than from an AirPlay implementation.
const GROUP_PRIME_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A",
    "85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);

/// A public Pair-Setup challenge sent in M2.
#[derive(Clone, PartialEq, Eq)]
pub struct PairSetupChallenge {
    salt: [u8; 16],
    server_public_key: Vec<u8>,
}

impl PairSetupChallenge {
    pub fn salt(&self) -> &[u8; 16] {
        &self.salt
    }

    /// Fixed-width, 384-byte unsigned big-endian encoding of SRP public value
    /// `B`, as carried by HomeKit Pair-Setup.
    pub fn server_public_key(&self) -> &[u8] {
        &self.server_public_key
    }
}

impl fmt::Debug for PairSetupChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairSetupChallenge")
            .field("salt", &self.salt)
            .field("server_public_key_bytes", &self.server_public_key.len())
            .finish()
    }
}

/// Errors that terminate one transient Pair-Setup exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    InvalidServerSecret,
    InvalidClientPublicKeyLength { actual: usize },
    InvalidClientPublicKey,
    InvalidScramblingParameter,
    InvalidClientProofLength { actual: usize },
    ClientProofMismatch,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidServerSecret => f.write_str("SRP server secret is zero"),
            Self::InvalidClientPublicKeyLength { actual } => {
                write!(f, "invalid SRP client public-key length: {actual}")
            }
            Self::InvalidClientPublicKey => f.write_str("invalid SRP client public key"),
            Self::InvalidScramblingParameter => f.write_str("invalid SRP scrambling parameter"),
            Self::InvalidClientProofLength { actual } => {
                write!(f, "invalid SRP client-proof length: {actual}")
            }
            Self::ClientProofMismatch => f.write_str("SRP client proof did not verify"),
        }
    }
}

impl Error for PairingError {}

/// One receiver-side SRP-6a exchange.
///
/// This deliberately has no `Debug` implementation: its verifier and private
/// exponent are authentication material. Consume it with [`Self::verify`] so
/// one challenge cannot be retried with multiple proofs.
pub struct PairSetupServer {
    salt: [u8; 16],
    verifier: BigUint,
    private_b: BigUint,
    public_b: BigUint,
}

impl PairSetupServer {
    /// Create a challenge from caller-generated per-session randomness.
    ///
    /// `server_secret` must be 32 random bytes and must never be reused. The
    /// password bytes are hashed during construction and are not retained.
    pub fn new(
        password: &[u8],
        salt: [u8; 16],
        mut server_secret: [u8; 32],
    ) -> Result<Self, PairingError> {
        let n = group_prime();
        let private_b = BigUint::from_bytes_be(&server_secret);
        server_secret.zeroize();
        if private_b == BigUint::from(0u8) {
            return Err(PairingError::InvalidServerSecret);
        }

        let mut identity_password = Sha512::new();
        identity_password.update(PAIR_SETUP_USERNAME);
        identity_password.update(b":");
        identity_password.update(password);
        let mut identity_password_hash: [u8; 64] = identity_password.finalize().into();

        let x = BigUint::from_bytes_be(&hash(&[&salt, &identity_password_hash]));
        identity_password_hash.zeroize();

        let generator = BigUint::from(5u8);
        let verifier = generator.modpow(&x, n);
        let multiplier =
            BigUint::from_bytes_be(&hash(&[&pad_to_group(n), &pad_to_group(&generator)]));
        let public_b = (multiplier * &verifier + generator.modpow(&private_b, n)) % n;

        Ok(Self {
            salt,
            verifier,
            private_b,
            public_b,
        })
    }

    pub fn challenge(&self) -> PairSetupChallenge {
        PairSetupChallenge {
            salt: self.salt,
            // HomeKit transports SRP-3072 B at the fixed group width. Sending
            // a minimal integer works for most peers but fails intermittently
            // whenever B happens to start with zero, because not every Apple
            // sender performs the missing left-padding itself.
            server_public_key: pad_to_group(&self.public_b).to_vec(),
        }
    }

    /// Verify M3 and return the M4 proof plus the shared SRP session key.
    ///
    /// HomeKit uses the RFC 5054 3072-bit group with generator 5, SHA-512 in
    /// place of SHA-1, and hashes the minimally encoded premaster secret into
    /// the 64-byte session key `K`.
    pub fn verify(
        self,
        client_public_key: &[u8],
        client_proof: &[u8],
    ) -> Result<VerifiedPairSetup, PairingError> {
        if client_public_key.is_empty() || client_public_key.len() > GROUP_BYTES {
            return Err(PairingError::InvalidClientPublicKeyLength {
                actual: client_public_key.len(),
            });
        }
        if client_proof.len() != PROOF_BYTES {
            return Err(PairingError::InvalidClientProofLength {
                actual: client_proof.len(),
            });
        }

        let n = group_prime();
        let client_a = BigUint::from_bytes_be(client_public_key);
        // RFC 5054 requires A mod N != 0. Reject non-canonical aliases at or
        // above N as well, so a public value has only one accepted encoding.
        if client_a >= *n || (&client_a % n) == BigUint::from(0u8) {
            return Err(PairingError::InvalidClientPublicKey);
        }

        let scrambling = BigUint::from_bytes_be(&hash(&[
            &pad_to_group(&client_a),
            &pad_to_group(&self.public_b),
        ]));
        if scrambling == BigUint::from(0u8) {
            return Err(PairingError::InvalidScramblingParameter);
        }

        // S = (A * v^u)^b mod N
        let premaster =
            (&client_a * self.verifier.modpow(&scrambling, n)).modpow(&self.private_b, n);
        let premaster_bytes = Zeroizing::new(premaster.to_bytes_be());
        let mut session_key = hash(&[premaster_bytes.as_slice()]);

        let expected_proof = client_evidence(&client_a, &self.public_b, &self.salt, &session_key);
        let supplied_proof: &[u8; PROOF_BYTES] = client_proof
            .try_into()
            .expect("proof length was checked above");
        if !bool::from(expected_proof.ct_eq(supplied_proof)) {
            session_key.zeroize();
            return Err(PairingError::ClientProofMismatch);
        }

        let server_proof = hash(&[&client_a.to_bytes_be(), supplied_proof, &session_key]);
        Ok(VerifiedPairSetup {
            server_proof,
            session_key,
        })
    }
}

/// Successful M3 verification result.
///
/// The session key is erased on drop and the type intentionally cannot be
/// formatted with `Debug`.
pub struct VerifiedPairSetup {
    server_proof: [u8; PROOF_BYTES],
    session_key: [u8; PROOF_BYTES],
}

impl VerifiedPairSetup {
    /// M4 `Proof` / HAMK value returned to the controller.
    pub fn server_proof(&self) -> &[u8; PROOF_BYTES] {
        &self.server_proof
    }

    /// SRP `K`, used as input key material for transient session keys.
    pub fn session_key(&self) -> &[u8; PROOF_BYTES] {
        &self.session_key
    }
}

impl Drop for VerifiedPairSetup {
    fn drop(&mut self) {
        self.session_key.zeroize();
    }
}

fn group_prime() -> &'static BigUint {
    static PRIME: OnceLock<BigUint> = OnceLock::new();
    PRIME.get_or_init(|| {
        BigUint::parse_bytes(GROUP_PRIME_HEX.as_bytes(), 16)
            .expect("RFC 5054 group prime is valid hexadecimal")
    })
}

fn pad_to_group(value: &BigUint) -> [u8; GROUP_BYTES] {
    let bytes = value.to_bytes_be();
    assert!(bytes.len() <= GROUP_BYTES, "value does not fit SRP group");
    let mut padded = [0u8; GROUP_BYTES];
    padded[GROUP_BYTES - bytes.len()..].copy_from_slice(&bytes);
    padded
}

fn hash(parts: &[&[u8]]) -> [u8; PROOF_BYTES] {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn client_evidence(
    client_a: &BigUint,
    server_b: &BigUint,
    salt: &[u8; 16],
    session_key: &[u8; PROOF_BYTES],
) -> [u8; PROOF_BYTES] {
    let n_hash = hash(&[&group_prime().to_bytes_be()]);
    let generator_hash = hash(&[&[5u8]]);
    let mut group_evidence = [0u8; PROOF_BYTES];
    for ((output, n), generator) in group_evidence.iter_mut().zip(n_hash).zip(generator_hash) {
        *output = n ^ generator;
    }

    let identity_hash = hash(&[PAIR_SETUP_USERNAME]);
    hash(&[
        &group_evidence,
        &identity_hash,
        salt,
        &client_a.to_bytes_be(),
        &server_b.to_bytes_be(),
        session_key,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(value: &str) -> Vec<u8> {
        assert_eq!(value.len() % 2, 0);
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    fn fixed_server() -> PairSetupServer {
        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = 0x21 + index as u8;
        }
        PairSetupServer::new(b"3939", core::array::from_fn(|i| i as u8), secret).unwrap()
    }

    #[test]
    fn matches_independently_generated_homekit_srp_vector() {
        // Generated independently with Python's hashlib.sha512 and pow using
        // RFC 5054's 3072-bit N/g=5. The vector deliberately uses A=5 to
        // exercise the distinction between padded u inputs and minimal M1.
        let server = fixed_server();
        let challenge = server.challenge();
        assert_eq!(challenge.salt(), &core::array::from_fn(|i| i as u8));
        assert_eq!(
            hash(&[challenge.server_public_key()]).as_slice(),
            decode_hex(
                "bf17cfffeaab6d25a7f3a298735aefbacb19ad9294c77fd81411aafd8e9d743f\
                 4f2308c7831808be8e6d47b4a3642d945ac996015a273e1e2cdba1ec0ac55e80"
            )
        );

        let client_proof = decode_hex(
            "7aab1db3df11ac82c8c7ed5f9ecbde7c8f57f3f68d0d9eb6c74b8fb6eef1b16\
             c2b61c481b900f96d086c1692652a8b1b0db638fbe26a58dcb36c7b81bf024284",
        );
        let verified = server.verify(&[5], &client_proof).unwrap();
        assert_eq!(
            verified.session_key().as_slice(),
            decode_hex(
                "d333689e720a053db972f7be3592b68010286fd6f422ce8746c16bb730dbf45b\
                 edbe712dacb0ec5ce89e1a56f383252ec86c0c3fd8cc738dac733513352c4931"
            )
        );
        assert_eq!(
            verified.server_proof().as_slice(),
            decode_hex(
                "00323dc501dc05a3512ef2fc86678569a4cdaff03604a846eaad9c11412dd83bc\
                 40bb42a2266e0a743cd5b57e3685af8b7cebb3885ee51f5e95c2f2fc29c8699"
            )
        );
    }

    #[test]
    fn rejects_wrong_proof_without_disclosing_expected_value() {
        let error = fixed_server().verify(&[5], &[0x55; PROOF_BYTES]).err();
        assert_eq!(error, Some(PairingError::ClientProofMismatch));
    }

    #[test]
    fn rejects_zero_and_modulus_public_values() {
        assert_eq!(
            fixed_server().verify(&[0], &[0; PROOF_BYTES]).err(),
            Some(PairingError::InvalidClientPublicKey)
        );
        assert_eq!(
            fixed_server()
                .verify(&group_prime().to_bytes_be(), &[0; PROOF_BYTES])
                .err(),
            Some(PairingError::InvalidClientPublicKey)
        );
    }

    #[test]
    fn bounds_untrusted_srp_fields() {
        assert_eq!(
            fixed_server().verify(&[], &[0; PROOF_BYTES]).err(),
            Some(PairingError::InvalidClientPublicKeyLength { actual: 0 })
        );
        assert_eq!(
            fixed_server()
                .verify(&vec![1; GROUP_BYTES + 1], &[0; PROOF_BYTES])
                .err(),
            Some(PairingError::InvalidClientPublicKeyLength {
                actual: GROUP_BYTES + 1
            })
        );
        assert_eq!(
            fixed_server().verify(&[5], &[0; PROOF_BYTES - 1]).err(),
            Some(PairingError::InvalidClientProofLength {
                actual: PROOF_BYTES - 1
            })
        );
    }

    #[test]
    fn refuses_zero_server_exponent() {
        assert!(matches!(
            PairSetupServer::new(b"password", [0; 16], [0; 32]),
            Err(PairingError::InvalidServerSecret)
        ));
    }
}
