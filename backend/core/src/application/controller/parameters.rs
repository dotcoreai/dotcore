//! Application parameter resolution at deployment creation.
//!
//! Merges operator-supplied values over the defaults an application declares, rejecting values
//! the application does not declare or does not accept, and generates a random value for each
//! secret the operator leaves out. The result is the complete set a deployment retains for its
//! whole life, in the application's declaration order, with the secrets that were generated.

use std::collections::BTreeSet;

use dotcore_catalog::typed_parameters::{check_value, default_value};
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationGraph, ParameterValue, parameter_definition::ParameterType, parameter_value::Value,
};

use super::errors::{ApplicationLifecycleError, ParameterRejection, configuration};

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

/// The parameter values a new deployment retains and the secrets among them that were generated.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ResolvedParameters {
    pub(super) values: Vec<ParameterValue>,
    pub(super) generated_parameter_ids: Vec<String>,
}

/// Resolves the complete parameter values of a new deployment from the operator's `supplied`
/// overrides, the graph's declared defaults, and a generated value for each secret left out.
pub(super) fn resolve_parameter_values(
    graph: &ApplicationGraph,
    supplied: &[ParameterValue],
    secrets: &dyn SecretGenerator,
) -> Result<ResolvedParameters, ApplicationLifecycleError> {
    let mut supplied_ids = BTreeSet::new();
    for value in supplied {
        let reject = |reason| ApplicationLifecycleError::InvalidParameter {
            parameter_id: value.parameter_id.clone(),
            reason,
        };
        if !supplied_ids.insert(value.parameter_id.as_str()) {
            return Err(reject(ParameterRejection::Duplicate));
        }
        let definition = graph
            .parameters
            .iter()
            .find(|definition| definition.parameter_id == value.parameter_id)
            .ok_or_else(|| reject(ParameterRejection::Unknown))?;
        let inner = value
            .value
            .as_ref()
            .ok_or_else(|| reject(ParameterRejection::MissingValue))?;
        check_value(definition, inner)
            .map_err(|source| reject(ParameterRejection::InvalidValue(source)))?;
    }

    let mut resolved = ResolvedParameters {
        values: Vec::with_capacity(graph.parameters.len()),
        generated_parameter_ids: Vec::new(),
    };
    for definition in &graph.parameters {
        let supplied = supplied
            .iter()
            .find(|value| value.parameter_id == definition.parameter_id)
            .and_then(|value| value.value.clone());
        let value = match (supplied, definition.parameter_type.as_ref()) {
            (Some(value), _) => value,
            (None, Some(ParameterType::Secret(schema))) => {
                let value =
                    secrets
                        .generate(schema.generated_length as usize)
                        .map_err(|source| {
                            ApplicationLifecycleError::Configuration(
                                source.context("Application secret parameter generation failed"),
                            )
                        })?;
                resolved
                    .generated_parameter_ids
                    .push(definition.parameter_id.clone());
                Value::String(value)
            }
            (None, _) => default_value(definition)
                .ok_or_else(|| configuration("Application parameter default is missing"))?,
        };
        resolved.values.push(ParameterValue {
            parameter_id: definition.parameter_id.clone(),
            value: Some(value),
        });
    }
    Ok(resolved)
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
