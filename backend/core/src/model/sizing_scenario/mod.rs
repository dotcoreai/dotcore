//! Sizes catalog model profiles for an operator's sizing scenario, a context per request,
//! parallel requests and a total context, each optional, and judges the advisory memory fit of
//! each profile on a target.
//!
//! The scenario sets its values over each profile's defaults, and the usual resolution then
//! derives the context values left out and evaluates sizing, so estimates match what a launch with
//! those values would get. Each estimate is judged against the target's total memory and against
//! its free memory less a headroom, and each running target also gets the largest total context
//! and context per request that fit that same free-memory line, and the context values a
//! deployment starts from there. A scenario a profile can't serve reads as unsupported with a
//! reason instead of failing, and nothing here admits, reserves, or places a deployment.

use dotcore_catalog::{
    ACCELERATOR_MEMORY_RESOURCE, Catalog, CatalogLocalModelDeclarations, CatalogModelId,
    HOST_MEMORY_RESOURCE,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID, is_shared_parameter, maximum_context_tokens,
    },
    typed_parameters::unsigned_range_contains,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, HardwareRequirements, Model, ModelProfile, ParameterValue,
        ParameterValues, parameter_value::Value as EngineParameterValueKind,
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
        capacity::{
            Estimate, LargestTotalContext, Policy, StartingContext, StartingContextRequest,
            combined_minimums, demand, fit, largest_total_context, starting_context, steps_within,
        },
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

/// A validated sizing scenario: a context per request, the requests generated at once, and a total
/// context they share, each optional. Resolution derives the context values left out, and parallel
/// requests left out keep each profile's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizingScenario {
    total_context_tokens: Option<u64>,
    context_tokens_per_request: Option<u64>,
    parallel_requests: Option<u64>,

    /// Whether a starting deployment's total context follows its context per request rather than
    /// the scenario's total, which then only sizes the estimate.
    starting_total_follows: bool,
}

impl SizingScenario {
    /// Validates a requested scenario: a context per request no larger than the total context. Each
    /// profile checks the values against its engine's ranges, so a value one engine refuses stays a
    /// valid scenario for the others.
    pub fn new(
        total_context_tokens: Option<u64>,
        context_tokens_per_request: Option<u64>,
        parallel_requests: Option<u64>,
    ) -> Result<Self, InvalidSizingScenario> {
        if let (Some(total_context_tokens), Some(context_tokens_per_request)) =
            (total_context_tokens, context_tokens_per_request)
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
            parallel_requests,
            starting_total_follows: false,
        })
    }

    /// A scenario sized at the smallest total context one request of `context_tokens_per_request`
    /// starts from, at each profile's parallel requests, whose starting deployment still takes the
    /// total context following its context per request. Its memory fits tell whether a new
    /// deployment would start at that context per request.
    pub(crate) fn one_request(
        total_context_tokens: u64,
        context_tokens_per_request: u64,
    ) -> Result<Self, InvalidSizingScenario> {
        Ok(Self {
            starting_total_follows: true,
            ..Self::new(
                Some(total_context_tokens),
                Some(context_tokens_per_request),
                None,
            )?
        })
    }

    /// Context every request shares, in tokens; none leaves it to resolution to derive.
    pub fn total_context_tokens(&self) -> Option<u64> {
        self.total_context_tokens
    }

    /// Context one request may use, in tokens; none leaves it to resolution to fill.
    pub fn context_tokens_per_request(&self) -> Option<u64> {
        self.context_tokens_per_request
    }

    /// Requests generated at once; none keeps the profile's.
    pub fn parallel_requests(&self) -> Option<u64> {
        self.parallel_requests
    }
}

/// A requested sizing scenario whose values contradict each other.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSizingScenario {
    #[error(
        "sizing scenario context per request of {context_tokens_per_request} tokens is above its total context of {total_context_tokens} tokens"
    )]
    ContextPerRequestAboveTotal {
        total_context_tokens: u64,
        context_tokens_per_request: u64,
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

    /// Largest context per request at the scenario's parallel requests whose total, by the
    /// engine's rule, fits the same line, in tokens; none under the same conditions.
    pub(crate) largest_context_tokens_per_request: Option<u64>,

    /// Context values a deployment of the scenario starts from there; none when the scenario is
    /// unsupported or the profile isn't sized.
    pub(crate) starting_context: Option<StartingContext>,
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
        sizing.engine_parameter_values = Some(with_derived_context_values(
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

/// Context per request a profile's defaults resolve to: the engine default, capped at the
/// configuration's maximum context. Returns none when the defaults don't resolve or the engine
/// declares no default.
pub(crate) fn default_context_per_request(
    catalog: &Catalog,
    model: &Model,
    profile: &ModelProfile,
) -> Option<u64> {
    let (defaults, declarations) = profile_defaults(catalog, model, profile)?;
    let values = with_derived_context_values(
        &declarations,
        defaults.engine_parameter_values.unwrap_or_default(),
    );
    unsigned_parameter_value(&values, CONTEXT_PER_REQUEST_PARAMETER_ID)
}

/// Smallest total context at which one request of `context_tokens_per_request` can start under the
/// profile's defaults: the smallest total the engine's rule fills that context per request from at
/// the profile's parallel requests. A deployment starts at the parallel requests times the context
/// per request capped at the largest fit, so its context per request stands exactly when this
/// total fits. Returns none when the defaults don't resolve or no such total exists.
pub(crate) fn smallest_total_serving(
    catalog: &Catalog,
    model: &Model,
    profile: &ModelProfile,
    context_tokens_per_request: u64,
) -> Option<u64> {
    let (defaults, declarations) = profile_defaults(catalog, model, profile)?;
    smallest_serving_total(
        &declarations,
        &defaults.engine_parameter_values.unwrap_or_default(),
        context_tokens_per_request,
    )
}

/// Smallest total context at which one request of `context_tokens_per_request` can start at the
/// values' parallel requests, by the engine's rule; none when no total serves it.
fn smallest_serving_total(
    declarations: &CatalogLocalModelDeclarations,
    values: &ParameterValues,
    context_tokens_per_request: u64,
) -> Option<u64> {
    let schema = engine_parameter_schema(declarations)?;
    let rule =
        crate::model::engine::context_rule(declarations.engine_kind, &declarations.configuration);
    let parallel_requests = parallel_requests(schema, values);
    let maximum = u64::from(maximum_context_tokens(
        &declarations.model,
        &declarations.configuration,
    ));
    let serves = |total_context| {
        rule.filled_context_per_request(total_context, parallel_requests, maximum)
            >= context_tokens_per_request
    };
    let (mut rejected, mut accepted) = (
        context_tokens_per_request.saturating_sub(1),
        context_tokens_per_request.saturating_mul(parallel_requests),
    );
    if !serves(accepted) {
        return None;
    }
    // The fill grows with the total context, so the totals that serve form one interval.
    while accepted - rejected > 1 {
        let middle = rejected + (accepted - rejected) / 2;
        if serves(middle) {
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    Some(accepted)
}

/// Judges the memory fit of a profile that runs with its defaults, resolved as `snapshot`, on the
/// target capabilities `selected_target` the compatibility read selected, the largest context
/// values that fit there, and the context values a deployment of the scenario starts from. Both fits compare one estimate, of the profile's sizing scenario on the
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
    let model_id = CatalogModelId(profile_sizing.sizing.model_id.clone());
    let base = profile_sizing
        .configuration
        .as_ref()
        .zip(profile_sizing.declarations.as_ref())
        .map(|(configuration, declarations)| ContextFitBase {
            model_id: &model_id,
            profile_id: &profile_sizing.sizing.profile_id,
            configuration,
            declarations,
        });
    let largest = base
        .filter(|_| evaluated_or_unsupported)
        .map(|base| largest_contexts(catalog, &base, selected_target, minimums.as_ref()))
        .unwrap_or_default();
    let starting_context = base
        .filter(|_| unsupported_reason == SizingScenarioUnsupportedReason::Unspecified)
        .and_then(|base| {
            starting_context_on_target(
                &base,
                largest.floating_total_context_tokens,
                profile_sizing
                    .scenario
                    .is_some_and(|scenario| scenario.starting_total_follows),
            )
        });
    TargetMemoryFit {
        memory_fit: fits.on_total,
        free_memory_fit: fits.on_free,
        unsupported_reason,
        largest_total_context_tokens: largest.total_context_tokens,
        largest_context_tokens_per_request: largest.context_tokens_per_request,
        starting_context,
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

    // A scenario that leaves the total out fits where a deployment would start at its context per
    // request, its total capped at the largest fit: one request needs the smallest total serving
    // it. A total the scenario sets is judged as set.
    let one_request = profile_sizing
        .scenario
        .filter(|scenario| scenario.total_context_tokens().is_none())
        .and_then(|_| {
            one_request_configuration(profile_sizing.declarations.as_ref()?, configuration)
        });
    let model_id = CatalogModelId(profile_sizing.sizing.model_id.clone());
    match resolve_local_values_for_target(
        catalog,
        &model_id,
        &profile_sizing.sizing.profile_id,
        one_request.as_ref().unwrap_or(configuration),
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

/// The configuration with its context per request, as resolution derives it, and the smallest
/// total context one request of it starts from at its parallel requests. Returns none when the
/// configuration resolves no context per request or no total serves it.
fn one_request_configuration(
    declarations: &CatalogLocalModelDeclarations,
    configuration: &LocalModelDeploymentConfiguration,
) -> Option<LocalModelDeploymentConfiguration> {
    let values = with_derived_context_values(
        declarations,
        configuration
            .engine_parameter_values
            .clone()
            .unwrap_or_default(),
    );
    let context_tokens = unsigned_parameter_value(&values, CONTEXT_PER_REQUEST_PARAMETER_ID)?;
    let total_context = smallest_serving_total(declarations, &values, context_tokens)?;
    let mut configuration = configuration.clone();
    let values = configuration
        .engine_parameter_values
        .get_or_insert_with(ParameterValues::default);
    set_unsigned_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID, context_tokens);
    set_unsigned_value(values, TOTAL_CONTEXT_PARAMETER_ID, total_context);
    Some(configuration)
}

/// Returns the largest context values of a complete configuration, with every other value as
/// given, whose estimate fits the target `selected_target`'s free memory less a headroom, as
/// `largest_contexts` judges them for a profile's scenario. The minimums judged are the hardware
/// support's the configuration selects on the target. Returns no values when the configuration's
/// declarations don't resolve or the fit has no answer.
pub(crate) fn configuration_largest_contexts(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
) -> LargestContexts {
    let Some(declarations) = catalog
        .local_model_declarations(
            model_id,
            profile_id,
            &configuration.model_artifact_binding_id,
        )
        .map_err(|error| {
            debug!(
                model_id = %model_id.0,
                profile_id,
                error = %format!("{:#}", anyhow::Error::new(error)),
                "configuration declarations don't resolve for the context's largest fit"
            );
        })
        .ok()
    else {
        return LargestContexts::default();
    };
    largest_contexts(
        catalog,
        &ContextFitBase {
            model_id,
            profile_id,
            configuration,
            declarations: &declarations,
        },
        selected_target,
        minimums,
    )
}

/// The largest context values whose estimate fits a target's free memory less a headroom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LargestContexts {
    /// Largest total context at the configuration's context per request, in tokens.
    pub(crate) total_context_tokens: Option<u64>,

    /// Largest context per request at the configuration's parallel requests whose total, by the
    /// engine's rule, fits, in tokens.
    pub(crate) context_tokens_per_request: Option<u64>,

    /// Largest total context with the context per request following it, the cap a starting
    /// deployment takes; the same as `total_context_tokens` when the configuration leaves the
    /// context per request out.
    floating_total_context_tokens: Option<u64>,
}

/// Finds the base's largest context values. The largest context per request comes from the
/// largest total with the context per request following it, by the engine's rule: the largest
/// value whose derived total stays within it, stepped down to one the engine accepts, then
/// confirmed by its own estimate. Sizing reads the context per request only through the total
/// context it derives, so the search needs no estimate of its own.
fn largest_contexts(
    catalog: &Catalog,
    base: &ContextFitBase<'_>,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
) -> LargestContexts {
    let total_context_tokens =
        largest_fitting_total_context(catalog, base, selected_target, minimums);
    let supplied_context_per_request = base
        .configuration
        .engine_parameter_values
        .as_ref()
        .and_then(|values| unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID));
    let floating = without_context_values(base.configuration);
    let floating_base = ContextFitBase {
        configuration: &floating,
        ..*base
    };
    let floating_total_context_tokens = match supplied_context_per_request {
        Some(_) => {
            largest_fitting_total_context(catalog, &floating_base, selected_target, minimums)
        }
        None => total_context_tokens,
    };
    let context_tokens_per_request = floating_total_context_tokens.and_then(|largest_total| {
        largest_context_per_request(
            catalog,
            &floating_base,
            selected_target,
            minimums,
            largest_total,
        )
    });
    LargestContexts {
        total_context_tokens,
        context_tokens_per_request,
        floating_total_context_tokens,
    }
}

/// Returns the largest context per request whose total, by the engine's rule at the base's
/// parallel requests, stays within `largest_total` and that the engine accepts, once its own
/// estimate is confirmed to fit. Returns none when even the smallest accepted context per request
/// derives more, or the confirmation doesn't fit.
fn largest_context_per_request(
    catalog: &Catalog,
    base: &ContextFitBase<'_>,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
    largest_total: u64,
) -> Option<u64> {
    let declarations = base.declarations;
    let schema = engine_parameter_schema(declarations)?;
    let range = unsigned_parameter_range(schema, CONTEXT_PER_REQUEST_PARAMETER_ID)?;
    let rule =
        crate::model::engine::context_rule(declarations.engine_kind, &declarations.configuration);
    let maximum = u64::from(maximum_context_tokens(
        &declarations.model,
        &declarations.configuration,
    ));
    let values = base
        .configuration
        .engine_parameter_values
        .clone()
        .unwrap_or_default();
    let parallel_requests = parallel_requests(schema, &values);
    let within = |context_tokens| {
        rule.derived_total_context(context_tokens, parallel_requests, maximum) <= largest_total
    };

    let lowest = range.minimum.max(1);
    let highest = range.maximum.unwrap_or(u64::MAX).min(maximum);
    if lowest > highest || !within(lowest) {
        return None;
    }
    let (mut accepted, mut rejected) = (lowest, highest.saturating_add(1));
    while rejected - accepted > 1 {
        let middle = accepted + (rejected - accepted) / 2;
        if within(middle) {
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    let with_context = |context_tokens: u64| {
        let mut configuration = base.configuration.clone();
        set_unsigned_value(
            configuration
                .engine_parameter_values
                .get_or_insert_with(ParameterValues::default),
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            context_tokens,
        );
        configuration
    };
    // An engine may refuse some contexts per request below the bound, as it may refuse totals.
    let largest = (lowest..=accepted).rev().find(|context_tokens| {
        let values = with_context(*context_tokens)
            .engine_parameter_values
            .unwrap_or_default();
        accepts_context_values(
            declarations,
            schema,
            &with_derived_context_values(declarations, values),
        ) == Some(true)
    })?;

    let capacity = &selected_target.capacity;
    let fits = resolve_local_values_for_target(
        catalog,
        base.model_id,
        base.profile_id,
        &with_context(largest),
        selected_target,
    )
    .is_ok_and(|resolved| {
        let demand = demand(
            &Estimate {
                accelerator_bytes: estimate(&resolved, ACCELERATOR_MEMORY_RESOURCE),
                host_bytes: estimate(&resolved, HOST_MEMORY_RESOURCE),
            },
            capacity,
        );
        fit(demand.as_ref(), capacity, minimums, Policy::FREE_NOW) == MemoryFit::Fits
    });
    if !fits {
        debug!(
            model_id = %base.model_id.0,
            profile_id = base.profile_id,
            context_tokens_per_request = largest,
            "the largest context per request doesn't fit on its own estimate"
        );
        return None;
    }
    Some(largest)
}

/// Returns the context values a deployment of the base starts from on the target: its own context
/// per request, as resolution derives it, at its parallel requests, and its total context when it
/// sets one and `total_follows` doesn't ask for the derived one, capped by
/// `floating_total_context`, the largest total that fits with the context per request following
/// it. Returns none when the base resolves no context per request.
fn starting_context_on_target(
    base: &ContextFitBase<'_>,
    floating_total_context: Option<u64>,
    total_follows: bool,
) -> Option<StartingContext> {
    let declarations = base.declarations;
    let schema = engine_parameter_schema(declarations)?;
    let values = with_derived_context_values(
        declarations,
        base.configuration
            .engine_parameter_values
            .clone()
            .unwrap_or_default(),
    );
    let requested = StartingContextRequest {
        context_tokens_per_request: unsigned_parameter_value(
            &values,
            CONTEXT_PER_REQUEST_PARAMETER_ID,
        )?,
        parallel_requests: parallel_requests(schema, &values),
        total_context_tokens: base
            .configuration
            .engine_parameter_values
            .as_ref()
            .and_then(|values| unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID))
            .filter(|_| !total_follows),
        maximum_context_tokens: u64::from(maximum_context_tokens(
            &declarations.model,
            &declarations.configuration,
        )),
    };
    Some(starting_context(
        requested,
        floating_total_context,
        crate::model::engine::context_rule(declarations.engine_kind, &declarations.configuration),
    ))
}

/// Returns the largest total context whose estimate fits the target's free memory less a headroom,
/// at the base's context per request, or with the context per request filled from the maximum
/// context as the total context grows, and the base's other values. The capacity component finds
/// the memory bound from the estimates at the two smallest valid totals, which is exact because
/// sizing is linear in the total context for any values of the other parameters. The result is
/// clamped to the totals the engine accepts and that keep derived values in range, then stepped
/// down to the nearest total the engine accepts with the other values. Returns none when the
/// target's free memory is unknown, the target is below one of the declared `minimums`, nothing
/// fits, or nothing bounds it: memory never binds and neither the engine nor a derived value
/// declares a maximum.
fn largest_fitting_total_context(
    catalog: &Catalog,
    base: &ContextFitBase<'_>,
    selected_target: &SelectedTargetCapabilities,
    minimums: Option<&HardwareRequirements>,
) -> Option<u64> {
    let capacity = &selected_target.capacity;
    let ContextFitBase {
        model_id,
        profile_id,
        configuration,
        declarations,
    } = *base;
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
                .get_or_insert_with(ParameterValues::default),
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
            &with_derived_context_values(declarations, values),
        ) == Some(true)
    };
    let lowest = match supplied_context_per_request {
        Some(context_tokens) => context_tokens.max(total_range.minimum),
        // An engine that splits the total among the parallel requests, such as Strata, accepts
        // nothing below what its smallest context per request derives for them.
        None if !accepts(total_range.minimum) => {
            smallest_floating_total(declarations, schema, configuration)?.max(total_range.minimum)
        }
        None => total_range.minimum,
    };
    if !accepts(lowest) {
        return None;
    }
    let highest = highest_accepted(lowest, total_range.maximum, accepts);

    let evaluate = |total_context| {
        resolve_local_values_for_target(
            catalog,
            model_id,
            profile_id,
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
    if highest == Some(lowest) {
        return Some(lowest);
    }
    // An engine may refuse some totals just above the smallest, so the second estimate is at the
    // next total it accepts; the largest accepted total ends the search.
    let Some(second_total) =
        (lowest.saturating_add(1)..=highest.unwrap_or(u64::MAX)).find(|total| accepts(*total))
    else {
        return Some(lowest);
    };
    let (second, second_derived) = match evaluate(second_total) {
        Ok((Some(second), derived)) => (second, derived),
        // The next total pushes a derived value out of range, so the smallest is the largest.
        Err(ModelLaunchResolutionError::DerivedParameterOutOfRange { .. }) => {
            return Some(lowest);
        }
        _ => return None,
    };

    // Without a memory or engine bound, only a derived value's maximum can still bound the total.
    let mut largest = match largest_total_context(
        capacity,
        minimums,
        Policy::FREE_NOW,
        highest,
        (lowest, &first),
        (second_total, &second),
    )? {
        LargestTotalContext::Bounded(largest) => Some(largest),
        LargestTotalContext::Unbounded => None,
    };
    // Derived values only add non-negative terms, so no slope is negative; a zero slope leaves the
    // bound to the others.
    for value in &first_derived.values {
        if is_shared_parameter(&value.parameter_id) {
            continue;
        }
        let (Some(first_value), Some(second_value), Some(maximum)) = (
            unsigned_parameter_value(&first_derived, &value.parameter_id),
            unsigned_parameter_value(&second_derived, &value.parameter_id),
            unsigned_parameter_range(schema, &value.parameter_id).and_then(|range| range.maximum),
        ) else {
            continue;
        };
        let derived_growth = second_value.checked_sub(first_value)?;
        if let Some(steps) = steps_within(
            maximum.saturating_sub(first_value),
            second_total - lowest,
            derived_growth,
        ) {
            let bound = lowest.saturating_add(steps);
            largest = Some(largest.map_or(bound, |largest| largest.min(bound)));
        }
    }
    // An engine may refuse some totals below the memory bound, so the largest fit steps down to
    // the nearest total it accepts, and a deployment starting from it always resolves.
    largest.map(|largest| {
        (lowest..=largest)
            .rev()
            .find(|total| accepts(*total))
            .unwrap_or(lowest)
    })
}

/// The complete configuration a context's largest fit varies the total context of, and the
/// declarations it selects.
#[derive(Clone, Copy)]
struct ContextFitBase<'a> {
    model_id: &'a CatalogModelId,
    profile_id: &'a str,
    configuration: &'a LocalModelDeploymentConfiguration,
    declarations: &'a CatalogLocalModelDeclarations,
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

/// Sets the scenario's context values and parallel requests it holds over the configuration, after
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
        .get_or_insert_with(ParameterValues::default);
    if let Some(total_context_tokens) = scenario.total_context_tokens() {
        set_unsigned_value(values, TOTAL_CONTEXT_PARAMETER_ID, total_context_tokens);
    }
    if let Some(context_tokens) = scenario.context_tokens_per_request() {
        set_unsigned_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID, context_tokens);
    }
    if let Some(parallel_requests) = scenario.parallel_requests() {
        set_unsigned_value(values, PARALLEL_REQUESTS_PARAMETER_ID, parallel_requests);
    }
    let complete = with_derived_context_values(declarations, values.clone());
    if !accepts_context_values(declarations, schema, &complete)? {
        return Some(Err(SizingScenarioUnsupportedReason::ParameterOutOfRange));
    }
    Some(Ok(configuration))
}

/// Adds the context values resolution derives when the values leave them out, so the reported
/// values are the ones a launch would resolve before sizing derivations.
fn with_derived_context_values(
    declarations: &CatalogLocalModelDeclarations,
    mut values: ParameterValues,
) -> ParameterValues {
    if let Some(schema) = engine_parameter_schema(declarations) {
        let derived = crate::model::engine::derived_context_values(
            declarations.engine_kind,
            &declarations.model,
            &declarations.configuration,
            schema,
            &values,
        );
        values.values.extend(derived);
    }
    values
}

/// Whether complete values hold a total context, a context per request and parallel requests within
/// their ranges that the engine accepts together with the other values. Returns none when one is
/// set without an unsigned definition to check it against.
fn accepts_context_values(
    declarations: &CatalogLocalModelDeclarations,
    schema: &EngineParameterSchema,
    complete: &ParameterValues,
) -> Option<bool> {
    let mut in_range = true;
    for parameter_id in [
        TOTAL_CONTEXT_PARAMETER_ID,
        CONTEXT_PER_REQUEST_PARAMETER_ID,
        PARALLEL_REQUESTS_PARAMETER_ID,
    ] {
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
        in_range &= unsigned_range_contains(unsigned_parameter_range(schema, parameter_id)?, value);
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
/// holds for, or none when no `highest` bounds the totals and it holds up to the largest integer.
/// Engine ranges and rules accept an interval of totals, so a bisection finds its end.
fn highest_accepted(
    lowest: u64,
    highest: Option<u64>,
    accepts: impl Fn(u64) -> bool,
) -> Option<u64> {
    let end = highest.unwrap_or(u64::MAX);
    if accepts(end) {
        return highest;
    }
    let (mut accepted, mut rejected) = (lowest, end);
    while rejected - accepted > 1 {
        let middle = accepted + (rejected - accepted) / 2;
        if accepts(middle) {
            accepted = middle;
        } else {
            rejected = middle;
        }
    }
    Some(accepted)
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

/// The configuration with its total context and context per request left out, so resolution
/// derives the context per request as the total context varies.
fn without_context_values(
    configuration: &LocalModelDeploymentConfiguration,
) -> LocalModelDeploymentConfiguration {
    let mut configuration = configuration.clone();
    if let Some(values) = configuration.engine_parameter_values.as_mut() {
        values.values.retain(|value| {
            value.parameter_id != TOTAL_CONTEXT_PARAMETER_ID
                && value.parameter_id != CONTEXT_PER_REQUEST_PARAMETER_ID
        });
    }
    configuration
}

/// Total context the engine's rule derives for its smallest context per request at the
/// configuration's parallel requests.
fn smallest_floating_total(
    declarations: &CatalogLocalModelDeclarations,
    schema: &EngineParameterSchema,
    configuration: &LocalModelDeploymentConfiguration,
) -> Option<u64> {
    let minimum = unsigned_parameter_range(schema, CONTEXT_PER_REQUEST_PARAMETER_ID)?.minimum;
    let values = configuration
        .engine_parameter_values
        .clone()
        .unwrap_or_default();
    Some(
        crate::model::engine::context_rule(declarations.engine_kind, &declarations.configuration)
            .derived_total_context(
                minimum,
                parallel_requests(schema, &values),
                u64::from(maximum_context_tokens(
                    &declarations.model,
                    &declarations.configuration,
                )),
            ),
    )
}

/// The values' parallel requests, or the engine default without one, and at least one.
fn parallel_requests(schema: &EngineParameterSchema, values: &ParameterValues) -> u64 {
    unsigned_parameter_value(values, PARALLEL_REQUESTS_PARAMETER_ID)
        .or_else(|| {
            unsigned_parameter_range(schema, PARALLEL_REQUESTS_PARAMETER_ID)
                .and_then(|range| range.default_value)
        })
        .unwrap_or(1)
        .max(1)
}

/// Replaces the value of one parameter, or adds it when the values leave it out.
fn set_unsigned_value(values: &mut ParameterValues, parameter_id: &str, value: u64) {
    let value = ParameterValue {
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
