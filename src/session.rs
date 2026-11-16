//! Anonymous session ownership. Secret bytes are never retained after hashing.

use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// SHA-256 ownership token for storage/authorization, not public serialization.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnerDigest([u8; 32]);

/// Invalid hello secret, with no copy of the offending input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("session secret must encode exactly 32 bytes as unpadded base64url")]
pub struct InvalidSessionSecret;

impl OwnerDigest {
    /// Decodes a canonical 256-bit secret and hashes its original bytes.
    ///
    /// # Errors
    /// Rejects wrong lengths, padding, standard-base64 characters and invalid encodings.
    pub fn from_secret(secret: &str) -> Result<Self, InvalidSessionSecret> {
        // 32 bytes are exactly 43 characters in unpadded base64url.
        if secret.len() != 43 {
            return Err(InvalidSessionSecret);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(secret)
            .map_err(|_| InvalidSessionSecret)?;
        if bytes.len() != 32 {
            return Err(InvalidSessionSecret);
        }
        Ok(Self(Sha256::digest(bytes).into()))
    }

    /// Digest bytes for parameterized database queries; do not expose to clients/logs.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for OwnerDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnerDigest([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_decoded_bytes_not_the_secret_text() {
        let digest = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([0; 32])).unwrap();
        assert_eq!(
            digest.as_bytes(),
            &[
                0x66, 0x68, 0x7a, 0xad, 0xf8, 0x62, 0xbd, 0x77, 0x6c, 0x8f, 0xc1, 0x8b, 0x8e, 0x9f,
                0x8e, 0x20, 0x08, 0x97, 0x14, 0x85, 0x6e, 0xe2, 0x33, 0xb3, 0x90, 0x2a, 0x59, 0x1d,
                0x0d, 0x5f, 0x29, 0x25,
            ]
        );
    }

    #[test]
    fn distinct_secrets_produce_distinct_ownership() {
        let first = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([0; 32])).unwrap();
        let second = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([1; 32])).unwrap();
        assert_ne!(first, second);
        assert_eq!(format!("{first:?}"), "OwnerDigest([REDACTED])");
    }

    #[test]
    fn rejects_wrong_lengths_padding_alphabet_and_noncanonical_tail() {
        let valid = URL_SAFE_NO_PAD.encode([0; 32]);
        for invalid in [
            String::new(),
            URL_SAFE_NO_PAD.encode([0; 31]),
            URL_SAFE_NO_PAD.encode([0; 33]),
            format!("{valid}="),
            "+".repeat(43),
            "/".repeat(43),
            "A".repeat(42) + "B", // Nonzero trailing padding bits.
        ] {
            assert_eq!(
                OwnerDigest::from_secret(&invalid),
                Err(InvalidSessionSecret)
            );
        }
    }
}
