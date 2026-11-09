//! Sizes catalog model profiles for an operator's sizing scenario, a total context with an
//! optional context per request, and judges the advisory memory fit of each profile on a target.
//!
//! The scenario sets the shared context values over each profile's defaults, and the usual
//! resolution then fills the context per request and evaluates sizing, so estimates match what a
//! launch with those values would get. Each estimate is judged against the target's total memory
//! and against its free memory less a headroom, and each running target also gets the largest
//! total context that fits that same free-memory line. A scenario a profile can't serve reads as
//! unsupported with a reason instead of failing, and nothing here admits, reserves, or places a
//! deployment.

use dotcore_catalog::{
    ACCELERATOR_MEMORY_RESOURCE, Catalog, CatalogLocalModelDeclarations, CatalogModelId,
    HOST_MEMORY_RESOURCE,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, MINIMUM_CONTEXT_TOKENS, TOTAL_CONTEXT_PARAMETER_ID,
        filled_context_per_request, is_shared_parameter, maximum_context_tokens,
    },
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, EngineParameterValue, EngineParameterValues, HardwareRequirements,
        Model, ModelProfile, engine_parameter_value::Value as EngineParameterValueKind,
    },
    deployment::v1::MemoryFit,
    model::v1::{
        LocalModelDeploymentConfiguration, ModelProfileSizing,
        ResolvedLocalModelDeploymentSnapshot, SizingScenarioUnsupportedReason,
    },
};
use tracing::debug;

use crate::{
    hardware::{
        capacity::{Estimate, Policy, combined_minimums, demand, fit, largest_total_context},
        selection::SelectedTargetCapabilities,
    },
    model::{
        engine::{unsigned_parameter_range, unsigned_parameter_value},
        resolution::{
            ModelLaunchResolutionError, ResolvedLocalModelValues, apply_profile_defaults,
            resolve_local_values_each_class, resolve_local_values_for_target,
        },
    },
};

/// A validated sizing scenario: a total context its requests share, optionally with a cap on each
/// request's context. Parallel requests keep each profile's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizingScenario {
    total_context_tokens: u32,
    context_tokens_per_request: Option<u32>,
}

impl SizingScenario {
    /// Validates a requested scenario: a total context and a context per request of at least the
    /// minimum context, and a context per request no larger than the total context.
    pub fn new(
        total_context_tokens: u32,
        context_tokens_per_request: Option<u32>,
    ) -> Result<Self, InvalidSizingScenario> {
        if u64::from(total_context_tokens) < MINIMUM_CONTEXT_TOKENS {
            return Err(InvalidSizingScenario::TotalContextBelowMinimum {
                total_context_tokens,
            });
        }
        if let Some(context_tokens_per_request) = context_tokens_per_request
            && u64::from(context_tokens_per_request) < MINIMUM_CONTEXT_TOKENS
        {
            return Err(InvalidSizingScenario::ContextPerRequestBelowMinimum {
                context_tokens_per_request,
            });
        }
        if let Some(context_tokens_per_request) = context_tokens_per_request
            && context_tokens_per_request > total_context_tokens
        {
            return Err(InvalidSizingScenario::ContextPerRequestAboveTotal {
                total_context_tokens,
                context_tokens_per_request,
            });
        }
        Ok(Self {
            total_context_tokens,
            context_tokens_per_request,
        })
    }

    /// Context every request shares, in tokens.
    pub fn total_context_tokens(&self) -> u64 {
        u64::from(self.total_context_tokens)
    }

    /// Context one request may use, in tokens; none leaves it to resolution to fill.
    pub fn context_tokens_per_request(&self) -> Option<u64> {
        self.context_tokens_per_request.map(u64::from)
    }
}

/// A requested sizing scenario outside the accepted input range.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSizingScenario {
    #[error(
        "sizing scenario total context of {total_context_tokens} tokens is below the minimum of {MINIMUM_CONTEXT_TOKENS} tokens"
    )]
    TotalContextBelowMinimum { total_context_tokens: u32 },

    #[error(
        "sizing scenario context per request of {context_tokens_per_request} tokens is below the minimum of {MINIMUM_CONTEXT_TOKENS} tokens"
    )]
    ContextPerRequestBelowMinimum { context_tokens_per_request: u32 },

    #[error(
        "sizing scenario context per request of {context_tokens_per_request} tokens is above its total context of {total_context_tokens} tokens"
    )]
    ContextPerRequestAboveTotal {
        total_context_tokens: u32,
        context_tokens_per_request: u32,
    },
}

/// One profile's target-independent sizing, plus the configuration its targets' memory fit
/// resolves again on each target.
pub(crate) struct ProfileSizing {
    /// Sizing reported for the profile.
    pub(crate) sizing: ModelProfileSizing,

    /// Scenario the profile was sized for; none sizes its defaults.
    scenario: Option<SizingScenario>,

    /// Profile and engine defaults with the scenario's values applied; absent when the scenario
    /// is unsupported for the whole profile or can't be applied, or the defaults don't resolve.
    configuration: Option<LocalModelDeploymentConfiguration>,

    /// Declarations the configuration selects; present with the configuration.
    declarations: Option<CatalogLocalModelDeclarations>,
}

/// A profile's advisory memory fit on one running target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TargetMemoryFit {
    /// Fit of the scenario's estimate, or the defaults' without one, against the target's total
    /// memory; unspecified, not evaluated, when the scenario is unsupported.
    pub(crate) memory_fit: MemoryFit,

    /// Fit of the same estimate against the target's free memory less a headroom, the line the
    /// largest fit fills; unspecified when the total fit is.
    pub(crate) free_memory_fit: MemoryFit,

    /// Why the profile can't serve the scenario there; unspecified when it can.
    pub(crate) unsupported_reason: SizingScenarioUnsupportedReason,

    /// Largest total context whose estimate fits the target's free memory less a headroom, in
    /// tokens; none when free memory or the estimate is unknown, or nothing fits.
    pub(crate) largest_total_context_tokens: Option<u64>,
}

/// One estimate's memory fits on a target, against its total and against its free memory.
#[derive(Clone, Copy, Debug)]
struct MemoryFits {
    on_total: MemoryFit,
    on_free: MemoryFit,
}

impl MemoryFits {
    const UNKNOWN: Self = Self {
        on_total: MemoryFit::Unknown,
        on_free: MemoryFit::Unknown,
    };

    const UNSPECIFIED: Self = Self {
        on_total: MemoryFit::Unspecified,
        on_free: MemoryFit::Unspecified,
    };
}

/// Sizes one profile with its default binding and options under the scenario, or under its
/// engine parameter defaults without one, once per supported hardware class. When several reasons
/// make the scenario unsupported, the first in maximum context, parameter range, then derived
/// range order wins; a derived value out of range on some classes marks only those classes. A
/// scenario the declarations can't take leaves the profile without estimates.
pub(crate) fn size_profile(
    catalog: &Catalog,
    model: &Model,
    profile: &ModelProfile,
    scenario: Option<&SizingScenario>,
) -> ProfileSizing {
    let mut sizing = ModelProfileSizing {
        model_id: model.model_id.clone(),
        profile_id: profile.profile_id.clone(),
        ..Default::default()
    };
    let unsized_profile = |sizing| ProfileSizing {
        sizing,
        scenario: scenario.copied(),
        configuration: None,
        declarations: None,
    };
    let Some((defaults, declarations)) = profile_defaults(catalog, model, profile) else {
        return unsized_profile(sizing);
    };
    let configuration = match scenario {
        Some(scenario) => match with_scenario_values(&declarations, defaults, scenario) {
            Some(Ok(configuration)) => configuration,
            Some(Err(reason)) => {
                sizing.set_unsupported_reason(reason);
                return unsized_profile(sizing);
            }
            None => {
                debug!(
                    model_id = %model.model_id,
                    profile_id = %profile.profile_id,
                    "profile declarations can't take a sizing scenario"
                );
                return unsized_profile(sizing);
            }
        },
        None => defaults,
    };

    let model_id = CatalogModelId(model.model_id.clone());
    let classes =
        resolve_local_values_each_class(catalog, &model_id, &profile.profile_id, &configuration)
            .inspect_err(|error| log_sizing_failure(model, profile, None, error))
            .unwrap_or_default();
    // Only classes whose sizing produced a result count toward an unsupported profile.
    let mut sized_classes = 0;
    let mut out_of_range_classes = Vec::new();
    for (hardware_class_id, resolved) in classes {
        match resolved {
            Ok(resolved) => {
                sized_classes += 1;
                sizing
                    .hardware_class_estimates
                    .push(resolved.into_hardware_class_estimate(true));
            }
            Err(ModelLaunchResolutionError::DerivedParameterOutOfRange { .. }) => {
                sized_classes += 1;
                out_of_range_classes.push(hardware_class_id);
            }
            Err(error) => log_sizing_failure(model, profile, Some(&hardware_class_id), &error),
        }
    }
    if scenario.is_some() && sized_classes > 0 && out_of_range_classes.len() == sized_classes {
        sizing.set_unsupported_reason(SizingScenarioUnsupportedReason::DerivedParameterOutOfRange);
    } else {
        sizing.engine_parameter_values = Some(with_filled_context_per_request(
            &declarations,
            configuration
                .engine_parameter_values
                .clone()
                .unwrap_or_default(),
        ));
    }
    if scenario.is_some() {
        sizing.unsupported_hardware_class_ids = out_of_range_classes;
    }
    ProfileSizing {
        sizing,
        scenario: scenario.copied(),
        configuration: Some(configuration),
        declarations: Some(declarations),
    }
}

/// Judges the memory fit of a profile that runs with its defaults, resolved as `snapshot`, on the
/// target capabilities `selected_target` the compatibility read selected, and the largest total
/// context that fits there. Both fits compare one estimate, of the profile's sizing scenario on the
/// target's hardware class or the snapshot's own without a scenario: one with the target's total
/// memory, the other with its free memory less a headroom. The minimums judged are the resolved
/// hardware support's, raised by `extra_minimums` when a caller adds its own.
pub(crate) fn target_memory_fit(
    catalog: &Catalog,
    profile_sizing: &ProfileSizing,
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    selected_target: &SelectedTargetCapabilities,
    extra_minimums: Option<&HardwareRequirements>,
) -> TargetMemoryFit {
    let minimums = combined_minimums(hardware_requirements(snapshot), extra_minimums);
    let (fits, unsupported_reason) = scenario_memory_fit(
        catalog,
        profile_sizing,
        snapshot,
        selected_target,
        minimums.as_ref(),
    );
    // An unsupported scenario is not evaluated, but the totals that fit still guide the operator.
    let evaluated_or_unsupported = matches!(fits.on_total, MemoryFit::Fits | MemoryFit::Exceeds)
        || unsupported_reason != SizingScenarioUnsupportedReason::Unspecified;
    let largest_total_context_tokens = evaluated_or_unsupported
        .then(|| {
            largest_fitting_total_context(
                catalog,
                profile_sizing,
                selected_target,
                minimums.as_ref(),
            )
        })
        .flatten();
    TargetMemoryFit {
        memory_fit: fits.on_total,
        free_memory_fit: fits.on_free,
        unsupported_reason,
        largest_total_context_tokens,
    }
}

/// Returns the memory fits of the profile's scenario on the target under `minimums`, or
/// unevaluated fits with the reason the scenario is unsupported there, and an unspecified reason
/// otherwise.
fn scenario_memory_fit(
    catalog: &Catalog,
    profile_sizing: &ProfileSizing,
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
) -> (MemoryFits, SizingScenarioUnsupportedReason) {
    let supported = |fits| (fits, SizingScenarioUnsupportedReason::Unspecified);
    let capacity = &selected_target.capacity;
    let judge = |accelerator_bytes, host_bytes| {
        let demand = demand(
            &Estimate {
                accelerator_bytes,
                host_bytes,
            },
            capacity,
        );
        MemoryFits {
            on_total: fit(demand.as_ref(), capacity, minimums, Policy::TOTAL),
            on_free: fit(demand.as_ref(), capacity, minimums, Policy::FREE_NOW),
        }
    };
    if profile_sizing.scenario.is_none() {
        let sizing = snapshot.sizing.as_ref();
        return supported(judge(
            sizing.and_then(|sizing| sizing.accelerator_memory_bytes),
            sizing.and_then(|sizing| sizing.host_memory_bytes),
        ));
    }
    let Some(configuration) = profile_sizing.configuration.as_ref() else {
        return match profile_sizing.sizing.unsupported_reason() {
            SizingScenarioUnsupportedReason::Unspecified => supported(MemoryFits::UNKNOWN),
            reason => (MemoryFits::UNSPECIFIED, reason),
        };
    };

    let model_id = CatalogModelId(profile_sizing.sizing.model_id.clone());
    match resolve_local_values_for_target(
        catalog,
        &model_id,
        &profile_sizing.sizing.profile_id,
        configuration,
        selected_target,
    ) {
        Ok(resolved) => supported(judge(
            estimate(&resolved, ACCELERATOR_MEMORY_RESOURCE),
            estimate(&resolved, HOST_MEMORY_RESOURCE),
        )),
        Err(ModelLaunchResolutionError::DerivedParameterOutOfRange { .. }) => (
            MemoryFits::UNSPECIFIED,
            SizingScenarioUnsupportedReason::DerivedParameterOutOfRange,
        ),
        Err(error) => {
            debug!(
                model_id = %model_id.0,
                profile_id = %profile_sizing.sizing.profile_id,
                hardware_class_id = %snapshot
                    .hardware_support
                    .as_ref()
                    .map(|support| support.hardware_class_id.as_str())
                    .unwrap_or_default(),
                error = %format!("{error:#}"),
                "sizing scenario doesn't resolve on the target"
            );
            supported(MemoryFits::UNKNOWN)
        }
    }
}

/// Returns the largest total context whose estimate fits the target's free memory less a headroom,
/// at the scenario's context per request, or with the context per request filled from the maximum
/// context as the total context grows, and the profile's other values. The capacity component
/// finds the memory bound from the estimates at the two smallest valid totals, and the result is
/// clamped to the totals the engine accepts and that keep derived values in range. Returns none
/// when the declarations or the target's free memory are unknown, the target is below one of the
/// declared `minimums`, or nothing fits.
fn largest_fitting_total_context(
    catalog: &Catalog,
    profile_sizing: &ProfileSizing,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
) -> Option<u64> {
    let capacity = &selected_target.capacity;
    let configuration = profile_sizing.configuration.as_ref()?;
    let declarations = profile_sizing.declarations.as_ref()?;
    let schema = engine_parameter_schema(declarations)?;
    let total_range = unsigned_parameter_range(schema, TOTAL_CONTEXT_PARAMETER_ID)?;
    let supplied_context_per_request = configuration
        .engine_parameter_values
        .as_ref()
        .and_then(|values| unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID));

    // A supplied context per request stays fixed and bounds the total from below; an omitted one
    // floats with the total context.
    let with_total = |total_context: u64| {
        let mut configuration = configuration.clone();
        set_unsigned_value(
            configuration
                .engine_parameter_values
                .get_or_insert_with(EngineParameterValues::default),
            TOTAL_CONTEXT_PARAMETER_ID,
            total_context,
        );
        configuration
    };
    let accepts = |total_context: u64| {
        let values = with_total(total_context)
            .engine_parameter_values
            .unwrap_or_default();
        accepts_context_values(
            declarations,
            schema,
            &with_filled_context_per_request(declarations, values),
        ) == Some(true)
    };
    let lowest = supplied_context_per_request.map_or(*total_range.start(), |context_tokens| {
        context_tokens.max(*total_range.start())
    });
    if !accepts(lowest) {
        return None;
    }
    let highest = highest_accepted(lowest, *total_range.end(), accepts);

    let model_id = CatalogModelId(profile_sizing.sizing.model_id.clone());
    let evaluate = |total_context| {
        resolve_local_values_for_target(
            catalog,
            &model_id,
            &profile_sizing.sizing.profile_id,
            &with_total(total_context),
            selected_target,
        )
        .map(|resolved| {
            let demand = demand(
                &Estimate {
                    accelerator_bytes: estimate(&resolved, ACCELERATOR_MEMORY_RESOURCE),
                    host_bytes: estimate(&resolved, HOST_MEMORY_RESOURCE),
                },
                capacity,
            );
            (demand, resolved.derived_engine_parameter_values)
        })
    };
    let (Some(first), first_derived) = evaluate(lowest).ok()? else {
        return None;
    };
    if fit(Some(&first), capacity, minimums, Policy::FREE_NOW) != MemoryFit::Fits {
        return None;
    }
    if highest == lowest {
        return Some(lowest);
    }
    let second_total = lowest + 1;
    let (second, second_derived) = match evaluate(second_total) {
        Ok((Some(second), derived)) => (second, derived),
        // The next total pushes a derived value out of range, so the smallest is the largest.
        Err(ModelLaunchResolutionError::DerivedParameterOutOfRange { .. }) => {
            return Some(lowest);
        }
        _ => return None,
    };

    let mut largest = largest_total_context(
        capacity,
        minimums,
        Policy::FREE_NOW,
        lowest,
        highest,
        &first,
        &second,
    )?;
    // Derived values only add non-negative terms, so no slope is negative; a zero slope leaves the
    // bound to the others.
    for value in &first_derived.values {
        if is_shared_parameter(&value.parameter_id) {
            continue;
        }
        let (Some(first_value), Some(second_value), Some(range)) = (
            unsigned_parameter_value(&first_derived, &value.parameter_id),
            unsigned_parameter_value(&second_derived, &value.parameter_id),
            unsigned_parameter_range(schema, &value.parameter_id),
        ) else {
            continue;
        };
        let derived_slope = second_value.checked_sub(first_value)?;
        if let Some(steps) = (range.end() - first_value).checked_div(derived_slope) {
            largest = largest.min(lowest + steps);
        }
    }
    Some(largest)
}

/// Completes the profile's default configuration with its profile and engine defaults, and reads
/// the declarations it selects. Returns none, logging why, when either doesn't resolve.
fn profile_defaults(
    catalog: &Catalog,
    model: &Model,
    profile: &ModelProfile,
) -> Option<(
    LocalModelDeploymentConfiguration,
    CatalogLocalModelDeclarations,
)> {
    let model_id = CatalogModelId(model.model_id.clone());
    let log = |error: anyhow::Error| {
        debug!(
            model_id = %model.model_id,
            profile_id = %profile.profile_id,
            error = %format!("{error:#}"),
            "profile defaults don't resolve for sizing"
        );
    };
    let defaults = apply_profile_defaults(
        catalog,
        &model_id,
        &profile.profile_id,
        &LocalModelDeploymentConfiguration::default(),
    )
    .map_err(|error| log(anyhow::Error::new(error)))
    .ok()?;
    let declarations = catalog
        .local_model_declarations(
            &model_id,
            &profile.profile_id,
            &defaults.model_artifact_binding_id,
        )
        .map_err(|error| log(anyhow::Error::new(error)))
        .ok()?;
    Some((defaults, declarations))
}

/// Sets the scenario's total context and context per request over the configuration, after
/// checking the context per request against the maximum context, then every value the scenario
/// sets or fills against its parameter range and the engine's rules for values used together.
/// Returns none when a shared context parameter the scenario sets has no unsigned definition to
/// check it against.
fn with_scenario_values(
    declarations: &CatalogLocalModelDeclarations,
    mut configuration: LocalModelDeploymentConfiguration,
    scenario: &SizingScenario,
) -> Option<Result<LocalModelDeploymentConfiguration, SizingScenarioUnsupportedReason>> {
    let maximum_context_tokens =
        maximum_context_tokens(&declarations.model, &declarations.configuration);
    if scenario
        .context_tokens_per_request()
        .is_some_and(|context_tokens| context_tokens > u64::from(maximum_context_tokens))
    {
        return Some(Err(SizingScenarioUnsupportedReason::ContextAboveMaximum));
    }
    let schema = engine_parameter_schema(declarations)?;

    let values = configuration
        .engine_parameter_values
        .get_or_insert_with(EngineParameterValues::default);
    set_unsigned_value(
        values,
        TOTAL_CONTEXT_PARAMETER_ID,
        scenario.total_context_tokens(),
    );
    if let Some(context_tokens) = scenario.context_tokens_per_request() {
        set_unsigned_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID, context_tokens);
    }
    let complete = with_filled_context_per_request(declarations, values.clone());
    if !accepts_context_values(declarations, schema, &complete)? {
        return Some(Err(SizingScenarioUnsupportedReason::ParameterOutOfRange));
    }
    Some(Ok(configuration))
}

/// Adds the context per request resolution fills when the values leave it out, so the reported
/// values are the ones a launch would resolve before sizing derivations.
fn with_filled_context_per_request(
    declarations: &CatalogLocalModelDeclarations,
    mut values: EngineParameterValues,
) -> EngineParameterValues {
    if let Some(schema) = engine_parameter_schema(declarations) {
        let filled = filled_context_per_request(
            schema,
            &values,
            maximum_context_tokens(&declarations.model, &declarations.configuration),
        );
        values.values.extend(filled);
    }
    values
}

/// Whether complete values hold a total context and a context per request within their ranges
/// that the engine accepts together with the other values. Returns none when either is set
/// without an unsigned definition to check it against.
fn accepts_context_values(
    declarations: &CatalogLocalModelDeclarations,
    schema: &EngineParameterSchema,
    complete: &EngineParameterValues,
) -> Option<bool> {
    let mut in_range = true;
    for parameter_id in [TOTAL_CONTEXT_PARAMETER_ID, CONTEXT_PER_REQUEST_PARAMETER_ID] {
        let Some(value) = complete
            .values
            .iter()
            .find(|value| value.parameter_id == parameter_id)
        else {
            continue;
        };
        let Some(EngineParameterValueKind::UnsignedInteger(value)) = value.value else {
            return None;
        };
        in_range &= unsigned_parameter_range(schema, parameter_id)?.contains(&value);
    }
    Some(
        in_range
            && crate::model::engine::validate_parameter_values(
                declarations.engine_kind,
                &declarations.model,
                &declarations.configuration,
                complete,
            )
            .is_ok(),
    )
}

/// Returns the largest total between `lowest`, which `accepts` holds for, and `highest` that it
/// holds for. Engine ranges and rules accept an interval of totals, so a bisection finds its end.
fn highest_accepted(lowest: u64, highest: u64, accepts: impl Fn(u64) -> bool) -> u64 {
    if accepts(highest) {
        return highest;
    }
    let (mut accepted, mut rejected) = (lowest, highest);
    while rejected - accepted > 1 {
        let middle = accepted + (rejected - accepted) / 2;
        if accepts(middle) {
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    accepted
}

/// Parameter schema of the selection's engine, which every approved release shares.
fn engine_parameter_schema(
    declarations: &CatalogLocalModelDeclarations,
) -> Option<&EngineParameterSchema> {
    declarations
        .approved_engine_artifacts
        .first()?
        .engine_release
        .parameter_schema
        .as_ref()
}

/// Replaces the value of one parameter, or adds it when the values leave it out.
fn set_unsigned_value(values: &mut EngineParameterValues, parameter_id: &str, value: u64) {
    let value = EngineParameterValue {
        parameter_id: parameter_id.to_owned(),
        value: Some(EngineParameterValueKind::UnsignedInteger(value)),
    };
    match values
        .values
        .iter_mut()
        .find(|existing| existing.parameter_id == parameter_id)
    {
        Some(existing) => *existing = value,
        None => values.values.push(value),
    }
}

/// Memory minimums of the snapshot's resolved hardware support, absent when it declares none.
fn hardware_requirements(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Option<&HardwareRequirements> {
    snapshot
        .hardware_support
        .as_ref()
        .and_then(|support| support.hardware_requirements.as_ref())
}

fn estimate(resolved: &ResolvedLocalModelValues, resource: &str) -> Option<u64> {
    resolved
        .estimates
        .iter()
        .find(|estimate| estimate.resource == resource)
        .map(|estimate| estimate.value)
}

fn log_sizing_failure(
    model: &Model,
    profile: &ModelProfile,
    hardware_class_id: Option<&str>,
    error: &ModelLaunchResolutionError,
) {
    debug!(
        model_id = %model.model_id,
        profile_id = %profile.profile_id,
        hardware_class_id = hardware_class_id.unwrap_or_default(),
        error = %format!("{error:#}"),
        "profile sizing doesn't resolve"
    );
}

#[cfg(test)]
mod tests;
