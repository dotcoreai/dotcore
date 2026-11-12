//! Application parameter resolution at deployment creation.
//!
//! Merges operator-supplied values over the defaults an application declares, rejecting values
//! the application does not declare or does not accept. The result is the complete set a
//! deployment retains for its whole life, in the application's declaration order.

use std::collections::BTreeSet;

use dotcore_catalog::typed_parameters::{check_value, default_value};
use dotcore_proto::dotcore::catalog::v1::{ApplicationGraph, ParameterValue};

use super::errors::{ApplicationLifecycleError, ParameterRejection, configuration};

/// Resolves the complete parameter values of a new deployment from the operator's `supplied`
/// overrides and the graph's declared defaults.
pub(super) fn resolve_parameter_values(
    graph: &ApplicationGraph,
    supplied: &[ParameterValue],
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

    graph
        .parameters
        .iter()
        .map(|definition| {
            let value = match supplied
                .iter()
                .find(|value| value.parameter_id == definition.parameter_id)
            {
                Some(supplied) => supplied.value.clone(),
                None => default_value(definition),
            }
            .ok_or_else(|| configuration("Application parameter default is missing"))?;
            Ok(ParameterValue {
                parameter_id: definition.parameter_id.clone(),
                value: Some(value),
            })
        })
        .collect()
}
