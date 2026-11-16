//! Bcrypt form of a secret parameter's value, for services that take a password hash.
//!
//! The hash is computed once, when a deployment is created, with a fresh random salt, and is
//! retained as the authoritative form; it is never recomputed, so relaunches see the same
//! environment. Reading a retained hash back only checks that it is well formed and never verifies
//! it against the value, which would cost a full hash on every read.

use std::str::FromStr;

use bcrypt::HashParts;
use dotcore_catalog::typed_parameters::BCRYPT_MAXIMUM_SECRET_BYTES;

/// Work factor of a new hash; the one services such as n8n use for their own hashes.
const COST: u32 = 10;

/// Version prefix of every hash this form computes.
const VERSION_PREFIX: &str = "$2b$";

/// Lowest and highest work factors a well-formed hash may carry.
const VALID_COSTS: std::ops::RangeInclusive<u32> = 4..=31;

/// Failure to compute the bcrypt form of a value.
#[derive(Debug, thiserror::Error)]
pub(super) enum BcryptFormError {
    #[error("Bcrypt form takes at most {BCRYPT_MAXIMUM_SECRET_BYTES} bytes")]
    TooLong,

    #[error("Bcrypt hashing failed")]
    Hash(#[source] bcrypt::BcryptError),
}

/// Returns a bcrypt hash of `value` with a fresh random salt. A value longer than bcrypt covers is
/// refused before hashing rather than hashed on its first bytes only.
pub(super) fn bcrypt_hash(value: &str) -> Result<String, BcryptFormError> {
    if value.len() > BCRYPT_MAXIMUM_SECRET_BYTES as usize {
        return Err(BcryptFormError::TooLong);
    }
    bcrypt::hash(value, COST).map_err(BcryptFormError::Hash)
}

/// Reports whether `text` is a well-formed bcrypt hash in this form's version, without verifying
/// it against any value.
pub(super) fn is_bcrypt_hash(text: &str) -> bool {
    text.starts_with(VERSION_PREFIX)
        && HashParts::from_str(text).is_ok_and(|parts| VALID_COSTS.contains(&parts.get_cost()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcrypt_hash_verifies_against_its_value_with_a_fresh_salt() {
        let first = bcrypt_hash("retained-password").expect("first hash");
        let second = bcrypt_hash("retained-password").expect("second hash");

        assert!(first.starts_with("$2b$10$"));
        assert!(is_bcrypt_hash(&first));
        assert!(bcrypt::verify("retained-password", &first).expect("verify"));
        assert!(!bcrypt::verify("other-password", &first).expect("verify"));
        assert_ne!(first, second);
    }

    #[test]
    fn bcrypt_hash_covers_72_bytes_and_refuses_longer_values() {
        let longest = "p".repeat(72);
        let hash = bcrypt_hash(&longest).expect("72-byte hash");

        assert!(bcrypt::verify(&longest, &hash).expect("verify"));
        assert!(!bcrypt::verify("p".repeat(71), &hash).expect("verify"));
        assert!(matches!(
            bcrypt_hash(&"p".repeat(73)),
            Err(BcryptFormError::TooLong)
        ));
        assert!(matches!(
            bcrypt_hash(&"é".repeat(37)),
            Err(BcryptFormError::TooLong)
        ));
    }

    #[test]
    fn well_formed_hashes_are_recognized_without_their_value() {
        let valid = "$2b$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

        assert!(is_bcrypt_hash(valid));
        for invalid in [
            "",
            "retained-password",
            "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy",
            "$2b$99$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy",
            "$2b$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhW",
            "$2b$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lh!y",
        ] {
            assert!(!is_bcrypt_hash(invalid), "{invalid:?} is not a hash");
        }
    }
}
