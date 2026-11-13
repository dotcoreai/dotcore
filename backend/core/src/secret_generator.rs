//! Random values for deployment secret parameters.
//!
//! Application and model deployments generate a value for each secret parameter an operator
//! leaves out when the deployment is created. Generators are injected, so tests can predict the
//! values, while production draws them from the operating system's cryptographic random source.

/// Source of the random values generated for secret parameters an operator leaves out.
pub trait SecretGenerator: Send + Sync {
    /// Returns `length` characters drawn uniformly from ASCII letters and digits.
    fn generate(&self, length: usize) -> anyhow::Result<String>;
}

/// Generates secret values from the operating system's cryptographic random source.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomSecretGenerator;

impl SecretGenerator for RandomSecretGenerator {
    fn generate(&self, length: usize) -> anyhow::Result<String> {
        // 248 is the largest multiple of the alphabet size within a byte; rejecting bytes above
        // it keeps every character equally likely.
        const ACCEPTED_BYTES: u8 =
            (u8::MAX / SECRET_ALPHABET.len() as u8) * SECRET_ALPHABET.len() as u8;
        let mut value = String::with_capacity(length);
        let mut bytes = [0_u8; 64];
        while value.len() < length {
            getrandom::fill(&mut bytes).map_err(|source| {
                anyhow::Error::new(EntropyError(source)).context("Secret entropy failed")
            })?;
            value.extend(
                bytes
                    .iter()
                    .filter(|byte| **byte < ACCEPTED_BYTES)
                    .map(|byte| {
                        char::from(SECRET_ALPHABET[usize::from(*byte) % SECRET_ALPHABET.len()])
                    })
                    .take(length - value.len()),
            );
        }
        Ok(value)
    }
}

/// Characters a generated secret is drawn from: safe in environment values, URLs and shells.
const SECRET_ALPHABET: &[u8; 62] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EntropyError(getrandom::Error);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_secrets_have_the_requested_length_from_the_alphanumeric_alphabet() {
        let generator = RandomSecretGenerator;
        for length in [1, 32, 200] {
            let value = generator.generate(length).expect("generated secret");
            assert_eq!(value.len(), length);
            assert!(
                value.bytes().all(|byte| SECRET_ALPHABET.contains(&byte)),
                "{value}"
            );
        }
        assert_ne!(
            generator.generate(32).expect("first secret"),
            generator.generate(32).expect("second secret")
        );
    }
}
