//! Application parameter resolution at deployment creation.
//!
//! Merges operator-supplied values over the defaults an application declares, rejecting values
//! the application does not declare or does not accept, and generates a random value for each
//! secret the operator leaves out. A parameter disabled by its switch keeps no value: a supplied
//! one is rejected and no secret is generated for it. The result is the set a deployment retains
//! for its whole life, in the application's declaration order.

use std::collections::BTreeSet;

use dotcore_catalog::parameter_groups::{enabled_parameters, enabler};
use dotcore_catalog::typed_parameters::{check_value, default_value};
use dotcore_proto::dotcore::catalog::v1::{
    ApplicationGraph, ParameterValue, parameter_definition::ParameterType, parameter_value::Value,
};

use super::errors::{ApplicationLifecycleError, ParameterRejection, configuration};
use crate::SecretGenerator;

/// Resolves the parameter values of a new deployment from the operator's `supplied` overrides,
/// the graph's declared defaults, and a generated value for each secret left out. Only enabled
/// parameters get a value; an enabling parameter decides from its supplied or default value.
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

    // Enabling parameters are never disabled themselves, so their supplied or default values
    // decide the enabled set before any secret is generated.
    let candidates = graph
        .parameters
        .iter()
        .filter_map(|definition| {
            let value = supplied_value(supplied, &definition.parameter_id)
                .or_else(|| default_value(definition))?;
            Some(ParameterValue {
                parameter_id: definition.parameter_id.clone(),
                value: Some(value),
            })
        })
        .collect::<Vec<_>>();
    let enabled = enabled_parameters(&graph.parameters, &graph.parameter_groups, &candidates)
        .ok_or_else(|| configuration("Application parameter switch value is missing"))?;

    let mut resolved = Vec::with_capacity(enabled.len());
    for definition in &graph.parameters {
        let supplied = supplied_value(supplied, &definition.parameter_id);
        if !enabled.contains(definition.parameter_id.as_str()) {
            if supplied.is_some() {
                return Err(ApplicationLifecycleError::InvalidParameter {
                    parameter_id: definition.parameter_id.clone(),
                    reason: ParameterRejection::Disabled {
                        enabled_by: enabler(definition, &graph.parameter_groups)
                            .unwrap_or_default()
                            .to_owned(),
                    },
                });
            }
            continue;
        }
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

/// Returns the operator's value for `parameter_id`, if supplied.
fn supplied_value(supplied: &[ParameterValue], parameter_id: &str) -> Option<Value> {
    supplied
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| value.value.clone())
}
