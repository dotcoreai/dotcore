//! Bcrypt form of a secret parameter's value, for services that take a password hash.
//!
//! The hash is computed once, when a deployment is created, with a fresh random salt, and is
//! retained as the authoritative form; it is never recomputed, so relaunches see the same
//! environment. Reading a retained hash back only checks that it is well formed and never verifies
//! it against the value, which would cost a full hash on every read.

use std::str::FromStr;

use bcrypt::HashParts;

/// Work factor of a new hash; the one services such as n8n use for their own hashes.
const COST: u32 = 10;

/// Version prefix of every hash this form computes.
const VERSION_PREFIX: &str = "$2b$";

/// Lowest and highest work factors a well-formed hash may carry.
const VALID_COSTS: std::ops::RangeInclusive<u32> = 4..=31;

/// Failure to compute the bcrypt form of a value.
#[derive(Debug, thiserror::Error)]
pub(super) enum BcryptFormError {
    #[error("Bcrypt hashing failed")]
    Hash(#[source] bcrypt::BcryptError),

    #[error("Bcrypt hash does not verify against its value")]
    Mismatch,
}

/// Returns a bcrypt hash of `value` with a fresh random salt, verified against `value` once.
pub(super) fn bcrypt_hash(value: &str) -> Result<String, BcryptFormError> {
    let hash = bcrypt::hash(value, COST).map_err(BcryptFormError::Hash)?;
    match bcrypt::verify(value, &hash).map_err(BcryptFormError::Hash)? {
        true => Ok(hash),
        false => Err(BcryptFormError::Mismatch),
    }
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
