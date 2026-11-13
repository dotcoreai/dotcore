//! Application parameter resolution at deployment creation.
//!
//! Merges operator-supplied values over the defaults an application declares, rejecting values
//! the application does not declare or does not accept, and generates a random value for each
//! secret the operator leaves out. The result is the complete set a deployment retains for its
//! whole life, in the application's declaration order.

use std::collections::BTreeSet;

use dotcore_catalog::typed_parameters::{check_value, default_value};
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationGraph, ParameterValue, parameter_definition::ParameterType, parameter_value::Value,
};

use super::errors::{ApplicationLifecycleError, ParameterRejection, configuration};
use crate::SecretGenerator;

/// Resolves the complete parameter values of a new deployment from the operator's `supplied`
/// overrides, the graph's declared defaults, and a generated value for each secret left out.
pub(super) fn resolve_parameter_values(
    graph: &ApplicationGraph,
    supplied: &[ParameterValue],
    secrets: &dyn SecretGenerator,
) -> Result<Vec<ParameterValue>, ApplicationLifecycleError> {
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

    let mut resolved = Vec::with_capacity(graph.parameters.len());
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
                Value::String(value)
            }
            (None, _) => default_value(definition)
                .ok_or_else(|| configuration("Application parameter default is missing"))?,
        };
        resolved.push(ParameterValue {
            parameter_id: definition.parameter_id.clone(),
            value: Some(value),
        });
    }
    Ok(resolved)
}
