//! Finds the largest fit of each engine memory parameter a resource estimate on a target reports:
//! the largest value at which the configuration still fits the target, with every other value as
//! supplied.
//!
//! The capacity component owns the room, the search and its limits; this module only re-evaluates
//! the configuration's sizing with one value changed and turns the answers into the API's terms.
//! Like the placement preview, the fits are advisory and transient.

use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_proto::dotcore::{
    catalog::v1::{
        MemoryPool, ParameterValue, ParameterValues, UnsignedIntegerParameterSchema,
        parameter_definition::ParameterType as EngineParameterType,
        parameter_value::Value as EngineParameterValueKind,
    },
    model::v1::{LocalModelDeploymentConfiguration, MemoryParameterFit},
};

use crate::{
    hardware::{
        capacity::{EstimatePart, MemoryParameterRange, Policy, largest_fitting_value},
        selection::SelectedTargetCapabilities,
    },
    model::resolution::{ResolvedLocalModelValues, apply_profile_defaults, evaluate_values},
};

/// Returns the largest fit on the target `selected` of each memory parameter of `resolved`'s
/// engine, judged on free memory less the headroom as the context's largest fit is. `supplied`
/// holds the values as the caller gave them, and `completed` the same with the profile's
/// defaults; a parameter whose value never changes the estimate has no fit.
pub(crate) fn memory_parameter_fits(
    catalog: &Catalog,
    profile_id: &str,
    supplied: &LocalModelDeploymentConfiguration,
    completed: &LocalModelDeploymentConfiguration,
    resolved: &ResolvedLocalModelValues,
    selected: &SelectedTargetCapabilities,
) -> Vec<MemoryParameterFit> {
    let selection = &resolved.selection;
    let minimums = selection.hardware_support.hardware_requirements.as_ref();
    resolved
        .engine_parameter_schema
        .parameters
        .iter()
        .filter_map(|parameter| {
            let Some(EngineParameterType::UnsignedInteger(schema)) = &parameter.parameter_type
            else {
                return None;
            };
            let declared = declared_estimate(schema)?;
            let parameter_id = parameter.parameter_id.as_str();
            let range = MemoryParameterRange {
                minimum: schema.minimum,
                maximum: schema.maximum,
                sized: sized_value(catalog, profile_id, supplied, resolved, parameter_id),
                declared,
            };
            let probe = |value| {
                let probed = with_value(completed, parameter_id, value);
                evaluate_values(catalog, profile_id, &probed, selection)
                    .ok()
                    .map(|values| values.estimate())
            };
            let largest_fit = largest_fitting_value(
                &selected.capacity,
                minimums,
                Policy::FREE_NOW,
                range,
                probe,
            )?;
            Some(MemoryParameterFit {
                parameter_id: parameter_id.to_owned(),
                largest_fit,
            })
        })
        .collect()
}

/// The estimate a memory parameter's value feeds, none when it isn't a memory parameter.
fn declared_estimate(schema: &UnsignedIntegerParameterSchema) -> Option<EstimatePart> {
    schema.memory_pool?;
    match schema.memory_pool() {
        MemoryPool::Accelerator => Some(EstimatePart::Accelerator),
        MemoryPool::Host => Some(EstimatePart::Host),
        MemoryPool::Unspecified => None,
    }
}

/// The value the parameter takes when the caller leaves it empty: the profile's default or the
/// one sizing derives. When the caller supplies it, the configuration is evaluated once more
/// without it.
fn sized_value(
    catalog: &Catalog,
    profile_id: &str,
    supplied: &LocalModelDeploymentConfiguration,
    resolved: &ResolvedLocalModelValues,
    parameter_id: &str,
) -> Option<u64> {
    let is_supplied = supplied
        .engine_parameter_values
        .as_ref()
        .is_some_and(|values| {
            values
                .values
                .iter()
                .any(|value| value.parameter_id == parameter_id)
        });
    if !is_supplied {
        return unsigned_value(&resolved.engine_parameter_values, parameter_id);
    }

    let mut left_empty = supplied.clone();
    if let Some(values) = left_empty.engine_parameter_values.as_mut() {
        values
            .values
            .retain(|value| value.parameter_id != parameter_id);
    }
    let model_id = CatalogModelId(resolved.selection.model.model_id.clone());
    let left_empty = apply_profile_defaults(catalog, &model_id, profile_id, &left_empty).ok()?;
    let evaluated = evaluate_values(catalog, profile_id, &left_empty, &resolved.selection).ok()?;
    unsigned_value(&evaluated.engine_parameter_values, parameter_id)
}

/// `configuration` with the parameter set to `value`, whether it held one or not.
fn with_value(
    configuration: &LocalModelDeploymentConfiguration,
    parameter_id: &str,
    value: u64,
) -> LocalModelDeploymentConfiguration {
    let mut configuration = configuration.clone();
    let values = configuration
        .engine_parameter_values
        .get_or_insert_with(ParameterValues::default);
    values
        .values
        .retain(|value| value.parameter_id != parameter_id);
    values.values.push(ParameterValue {
        parameter_id: parameter_id.to_owned(),
        value: Some(EngineParameterValueKind::UnsignedInteger(value)),
    });
    configuration
}

fn unsigned_value(values: &ParameterValues, parameter_id: &str) -> Option<u64> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value {
            Some(EngineParameterValueKind::UnsignedInteger(value)) => Some(value),
            _ => None,
        })
}
