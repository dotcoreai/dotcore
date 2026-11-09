//! Reports which catalog model profiles can run on which configured model execution targets, so
//! the console can filter and explain profile choices before anything is deployed.
//!
//! Each profile is resolved with its defaults through the same launch resolution that deployment
//! admission uses, so the answer agrees with what admission would accept at that moment. The
//! result is advisory: it admits, reserves, and places nothing, and a target without fresh
//! observations reads as unknown rather than judged on its configuration alone. Each profile is
//! also sized for an optional sizing scenario, and each profile that runs gets a memory fit, the
//! largest total context that fits, and the total context a deployment of it starts from. Memory
//! that deployments still starting on a node are expected to take counts as taken there, so a
//! second read right after a deploy doesn't offer the same memory again.

use std::{collections::BTreeMap, time::SystemTime};

use dotcore_catalog::{Catalog, CatalogModelId, shared_parameters::TOTAL_CONTEXT_PARAMETER_ID};
use dotcore_domain::MemoryArchitecture;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{HardwareRequirements, Model, ModelProfile, OciPlatform, Selector},
    deployment::v1::{ExactPlacement, accelerator_memory_observation},
    model::v1::{
        ListModelProfileCompatibilityResponse, LocalModelDeploymentAdvisory,
        LocalModelDeploymentConfiguration, ModelCompatibilityReason, ModelCompatibilityReasonCode,
        ModelCompatibilityStatus, ModelDeployment, ModelDeploymentStatus,
        ModelProfileCompatibility, StartingTotalContextSource, TargetCompatibility,
        model_deployment_snapshot,
    },
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::TargetCapabilityInventory;
use tracing::{debug, warn};

use crate::{
    hardware::attribution::PROCESS_MEMORY_PENDING,
    hardware::capacity::{
        Estimate, NodeWorkloads, StartingTotalContext, Workload, WorkloadKey, WorkloadPhase,
        starting_total_context,
    },
    hardware::selection::{
        HardwareClassSelectionError, SelectedTargetCapabilities, TargetCapabilitySelectionError,
        select_last_observed_target_capabilities, select_target_capabilities,
    },
    model::{
        advisory::local_deployment_advisories,
        engine::{EngineIntegrationRegistry, engine_artifact_kinds, unsigned_parameter_value},
        resolution::{
            ModelLaunchResolutionError, apply_profile_defaults,
            resolve_local_launch_on_selected_target,
        },
        selection::{EngineArtifactSelectionError, engine_artifact_kind},
        sizing_scenario::{
            ProfileSizing, SizingScenario, TargetMemoryFit, size_profile, target_memory_fit,
        },
    },
};

/// Evaluates every profile of `models` on every target in `targets`, which must be configured
/// model execution targets, and sizes each profile for `scenario`, or its defaults without one.
/// `inventory` is the current target observation, or none when it couldn't be read, which makes
/// every target unknown; `workloads` holds what the deployments of each node hold of its memory.
/// Targets are ordered by node then target identifier, profile sizings by catalog model then
/// profile declaration order, and entries by profile then target.
pub(crate) fn evaluate_profile_compatibility(
    catalog: &Catalog,
    models: &[Model],
    targets: &[ExecutionTarget],
    scenario: Option<&SizingScenario>,
    inventory: Option<&TargetCapabilityInventory>,
    workloads: &NodeWorkloads,
) -> ListModelProfileCompatibilityResponse {
    let checked = check_targets(targets, inventory, workloads, StaleTargets::Fail);
    let mut response = ListModelProfileCompatibilityResponse {
        targets: checked.iter().map(target_compatibility).collect(),
        ..Default::default()
    };

    let registry = EngineIntegrationRegistry::production();
    for model in models {
        let model_id = CatalogModelId(model.model_id.clone());
        for profile in &model.profiles {
            let sizing = size_profile(catalog, model, profile, scenario);
            for checked in &checked {
                let Ok(selected) = &checked.check else {
                    continue;
                };
                let request = ProfileRequest {
                    model_id: &model_id,
                    profile,
                    sizing: &sizing,
                    extra_minimums: None,
                };
                let evaluation =
                    evaluate_profile(catalog, &registry, &request, checked.target, selected);
                response.entries.push(profile_compatibility(
                    &model_id,
                    profile,
                    checked.target,
                    evaluation,
                ));
            }
            response.profiles.push(sizing.sizing);
        }
    }
    response
}

/// One configured model execution target after the checks every profile on it shares.
pub(crate) struct CheckedTarget<'a> {
    pub(crate) target: &'a ExecutionTarget,

    /// The target's fresh capabilities, with what its node's workloads hold of its memory, or the
    /// status and reason that keep every profile from being judged there.
    pub(crate) check: Result<SelectedTargetCapabilities, Failure>,

    /// Whether the target stopped reporting and its check used its last observations instead.
    pub(crate) unreachable: bool,

    /// When the target's runtime last reported its platform.
    observed_at: Option<SystemTime>,
}

/// A status other than runs, with the typed reason for it.
pub(crate) type Failure = (ModelCompatibilityStatus, ModelCompatibilityReason);

/// What target checks do with a target whose platform or accelerator observations are stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaleTargets {
    /// The target fails its check, as admission would.
    Fail,

    /// The target is checked on its last observations and marked unreachable.
    UseLastObserved,
}

/// Confirms each target's capabilities from `inventory` and counts in its pools what
/// `workloads` hold of its node's memory, ordered by node then target identifier. A missing
/// inventory leaves every target unknown; `stale` says what happens to a target that stopped
/// reporting.
pub(crate) fn check_targets<'a>(
    targets: &'a [ExecutionTarget],
    inventory: Option<&TargetCapabilityInventory>,
    workloads: &NodeWorkloads,
    stale: StaleTargets,
) -> Vec<CheckedTarget<'a>> {
    let mut targets = targets.iter().collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        (&left.site_node_id, &left.execution_target_id)
            .cmp(&(&right.site_node_id, &right.execution_target_id))
    });
    targets
        .into_iter()
        .map(|target| {
            let mut unreachable = false;
            let check = match inventory {
                Some(inventory) => match select_target_capabilities(target, inventory) {
                    Err(
                        TargetCapabilitySelectionError::StalePlatform { .. }
                        | TargetCapabilitySelectionError::StaleAccelerator { .. },
                    ) if stale == StaleTargets::UseLastObserved => {
                        unreachable = true;
                        select_last_observed_target_capabilities(target, inventory)
                    }
                    selected => selected,
                }
                .map_err(|error| target_failure(&error)),
                None => Err((
                    ModelCompatibilityStatus::Unknown,
                    reason(ModelCompatibilityReasonCode::TargetNotReporting),
                )),
            };
            let check = check.map(|mut selected| {
                if let Some(workloads) = workloads.get(&target.site_node_id) {
                    selected.capacity =
                        std::mem::take(&mut selected.capacity).with_workloads(workloads);
                }
                selected
            });
            CheckedTarget {
                target,
                check,
                unreachable,
                observed_at: inventory
                    .and_then(|inventory| inventory.for_target(&target.execution_target_id))
                    .and_then(|capabilities| capabilities.platform_observed_at),
            }
        })
        .collect()
}

/// What one profile gives on one target that passed the target-level checks.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ProfileEvaluation {
    /// The profile resolves there, with its memory fit.
    Runs(RunningProfile),

    /// The profile can't run there, or whether it can is unknown.
    Fails(Failure),
}

/// A profile that resolves on a target, with its advisories and advisory memory fit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunningProfile {
    pub(crate) advisories: Vec<LocalModelDeploymentAdvisory>,

    /// Hardware class the launch resolved to.
    pub(crate) hardware_class_id: String,

    pub(crate) fit: TargetMemoryFit,

    /// Total context a deployment of the profile starts from there; absent without a largest fit
    /// or a profile default.
    pub(crate) starting_total_context: Option<StartingTotalContext>,
}

/// One model profile to evaluate on targets, with its target-independent sizing.
pub(crate) struct ProfileRequest<'a> {
    pub(crate) model_id: &'a CatalogModelId,

    pub(crate) profile: &'a ModelProfile,

    pub(crate) sizing: &'a ProfileSizing,

    /// Memory minimums the caller adds on top of the configuration's own, such as a model
    /// recommendation's; none adds nothing.
    pub(crate) extra_minimums: Option<&'a HardwareRequirements>,
}

/// Resolves one profile with its defaults and default artifact binding on one target that passed
/// the target-level checks with the capabilities `selected`, then judges the memory fit of its
/// sizing there when it runs, under the configuration's minimums and the request's extra ones.
pub(crate) fn evaluate_profile(
    catalog: &Catalog,
    registry: &EngineIntegrationRegistry,
    request: &ProfileRequest<'_>,
    target: &ExecutionTarget,
    selected: &SelectedTargetCapabilities,
) -> ProfileEvaluation {
    let ProfileRequest {
        model_id,
        profile,
        sizing,
        extra_minimums,
    } = *request;
    let defaults = apply_profile_defaults(
        catalog,
        model_id,
        &profile.profile_id,
        &LocalModelDeploymentConfiguration::default(),
    );
    let default_total_context = defaults
        .as_ref()
        .ok()
        .and_then(|configuration| configuration.engine_parameter_values.as_ref())
        .and_then(|values| unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID));
    let resolved = defaults.and_then(|configuration| {
        resolve_local_launch_on_selected_target(
            catalog,
            model_id,
            &profile.profile_id,
            &configuration,
            target,
            selected,
            None,
        )
    });
    let snapshot = match resolved {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return ProfileEvaluation::Fails(profile_failure(
                catalog, model_id, profile, target, &error,
            ));
        }
    };

    // An advisory failure never hides that the profile runs.
    let advisories = local_deployment_advisories(registry, &snapshot)
        .inspect_err(|error| {
            warn!(
                model_id = %model_id.0,
                profile_id = %profile.profile_id,
                execution_target_id = %target.execution_target_id,
                error = %format!("{error:#}"),
                "profile compatibility advisories failed"
            );
        })
        .unwrap_or_default();
    let fit = target_memory_fit(catalog, sizing, &snapshot, selected, extra_minimums);
    ProfileEvaluation::Runs(RunningProfile {
        advisories,
        hardware_class_id: snapshot
            .hardware_support
            .map(|support| support.hardware_class_id)
            .unwrap_or_default(),
        starting_total_context: starting_total_context(
            fit.largest_total_context_tokens,
            default_total_context,
        ),
        fit,
    })
}

/// Returns the local deployments of each node as workloads holding its memory, in deployment
/// order: each with its resolved estimate on its target's devices and its node's host memory, and
/// the accelerator memory its node attributes to it. Deployments queued, preparing, downloading,
/// loading or warming up are starting and claim their estimate, an unknown size without a
/// resolved estimate, and so do ready ones whose memory no reading has measured yet; other ready
/// ones count their measured use, and the others only what is attributed to them.
pub(crate) fn node_workloads(
    deployments: &[ModelDeployment],
    inventory: Option<&TargetCapabilityInventory>,
) -> NodeWorkloads {
    let mut workloads = NodeWorkloads::new();
    for deployment in deployments {
        let Some(status) = deployment.state.as_ref().map(|state| state.status()) else {
            continue;
        };
        let memory = deployment
            .accelerator_memory
            .as_ref()
            .and_then(|memory| memory.result.as_ref());
        let phase = match status {
            ModelDeploymentStatus::Queued
            | ModelDeploymentStatus::Preparing
            | ModelDeploymentStatus::Downloading
            | ModelDeploymentStatus::Loading
            | ModelDeploymentStatus::WarmingUp => WorkloadPhase::Starting,
            // Its estimate counts until a reading measures it, rather than offering memory the
            // next reading will show in use.
            ModelDeploymentStatus::Ready if memory_pending(memory) => WorkloadPhase::Starting,
            ModelDeploymentStatus::Ready => WorkloadPhase::Ready,
            _ => WorkloadPhase::Other,
        };
        let local = match deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(model_deployment_snapshot::Source::Local(local)) => local,
            _ => continue,
        };
        let (Some(placement), Some(selected_target)) = (
            deployment
                .intent
                .as_ref()
                .and_then(|intent| intent.placement.as_ref()),
            local.selected_target.as_ref(),
        ) else {
            continue;
        };
        let Some(memory_architecture) =
            MemoryArchitecture::from_proto(selected_target.accelerator_memory_architecture())
        else {
            continue;
        };
        let device_ids = inventory
            .and_then(|inventory| inventory.for_target(&placement.execution_target_id))
            .and_then(|capabilities| capabilities.accelerator_device_ids.clone())
            .unwrap_or_default();
        let attributed = match memory {
            Some(accelerator_memory_observation::Result::Available(usage)) => Some(
                usage
                    .devices
                    .iter()
                    .filter(|device| device.managed_node_id == placement.site_node_id)
                    .map(|device| (device.device_id.clone(), device.used_memory_bytes))
                    .collect::<BTreeMap<_, _>>(),
            )
            .filter(|attributed| !attributed.is_empty()),
            _ => None,
        };
        workloads
            .entry(placement.site_node_id.clone())
            .or_default()
            .push(Workload {
                key: WorkloadKey::inference(deployment.model_deployment_id.clone()),
                phase,
                expects_use: phase != WorkloadPhase::Other,
                memory_architecture,
                device_ids,
                estimate: local
                    .sizing
                    .as_ref()
                    .map(|sizing| Estimate {
                        accelerator_bytes: sizing.accelerator_memory_bytes,
                        host_bytes: sizing.host_memory_bytes,
                    })
                    .unwrap_or_default(),
                attributed,
            });
    }
    workloads
}

/// Whether a deployment's accelerator memory has no reading yet only because none was taken since
/// its process started.
fn memory_pending(memory: Option<&accelerator_memory_observation::Result>) -> bool {
    matches!(
        memory,
        Some(accelerator_memory_observation::Result::Unavailable(unavailable))
            if unavailable.code == PROCESS_MEMORY_PENDING
    )
}

/// The compatibility read's status of one checked target.
fn target_compatibility(checked: &CheckedTarget<'_>) -> TargetCompatibility {
    let (status, reason) = match &checked.check {
        Ok(_) => (ModelCompatibilityStatus::Runs, None),
        Err((status, reason)) => (*status, Some(reason.clone())),
    };
    TargetCompatibility {
        target: Some(placement(checked.target)),
        status: status as i32,
        reason,
        observed_at: checked
            .observed_at
            .map(crate::model::service::timestamp_from),
    }
}

/// The compatibility read's entry for one profile's evaluation on one target.
fn profile_compatibility(
    model_id: &CatalogModelId,
    profile: &ModelProfile,
    target: &ExecutionTarget,
    evaluation: ProfileEvaluation,
) -> ModelProfileCompatibility {
    let mut entry = ModelProfileCompatibility {
        model_id: model_id.0.clone(),
        profile_id: profile.profile_id.clone(),
        target: Some(placement(target)),
        ..Default::default()
    };
    match evaluation {
        ProfileEvaluation::Runs(running) => {
            let fit = running.fit;
            entry.set_status(ModelCompatibilityStatus::Runs);
            entry.advisories = running
                .advisories
                .into_iter()
                .map(|advisory| advisory as i32)
                .collect();
            entry.hardware_class_id = running.hardware_class_id;
            entry.set_memory_fit(fit.memory_fit);
            entry.set_free_memory_fit(fit.free_memory_fit);
            entry.set_unsupported_reason(fit.unsupported_reason);
            // Totals stay within the engine's 32-bit-range maxima.
            entry.largest_total_context_tokens = fit
                .largest_total_context_tokens
                .and_then(|tokens| u32::try_from(tokens).ok());
            let starting = running
                .starting_total_context
                .and_then(|starting| Some((starting, u32::try_from(starting.tokens()).ok()?)));
            if let Some((starting, tokens)) = starting {
                entry.starting_total_context_tokens = Some(tokens);
                entry.set_starting_total_context_source(match starting {
                    StartingTotalContext::LargestFit(_) => StartingTotalContextSource::LargestFit,
                    StartingTotalContext::ProfileDefault(_) => {
                        StartingTotalContextSource::ProfileDefault
                    }
                });
            }
        }
        ProfileEvaluation::Fails((status, reason)) => {
            entry.set_status(status);
            entry.reason = Some(reason);
        }
    }
    entry
}

/// Maps a resolution failure of one profile on one target to its status and typed reason.
fn profile_failure(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile: &ModelProfile,
    target: &ExecutionTarget,
    error: &ModelLaunchResolutionError,
) -> (ModelCompatibilityStatus, ModelCompatibilityReason) {
    let cannot_run = |reason| (ModelCompatibilityStatus::CannotRun, reason);
    match error {
        // Observations can expire between the target-level check and resolution.
        ModelLaunchResolutionError::TargetCapability(error) => target_failure(error),
        ModelLaunchResolutionError::EngineArtifactSelection(
            EngineArtifactSelectionError::NoMatch { .. },
        ) => cannot_run(ModelCompatibilityReason {
            supported_selectors: supported_selectors(catalog, model_id, profile, target),
            ..reason(ModelCompatibilityReasonCode::NoMatchingEngineArtifact)
        }),
        ModelLaunchResolutionError::EngineArtifactSelection(
            EngineArtifactSelectionError::DriverBelowMinimum {
                observed_major,
                required_major,
            },
        ) => cannot_run(ModelCompatibilityReason {
            minimum_nvidia_driver_major_version: *required_major,
            observed_nvidia_driver_major_version: *observed_major,
            ..reason(ModelCompatibilityReasonCode::NvidiaDriverBelowMinimum)
        }),
        ModelLaunchResolutionError::HardwareClassSelection { source, .. }
            if matches!(**source, HardwareClassSelectionError::NoMatch { .. }) =>
        {
            cannot_run(reason(
                ModelCompatibilityReasonCode::NoSupportedHardwareClass,
            ))
        }
        ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
            device_count,
            required,
            ..
        } => cannot_run(ModelCompatibilityReason {
            accelerator_device_count: u32::try_from(*device_count).unwrap_or(u32::MAX),
            required_accelerator_device_count: *required,
            ..reason(ModelCompatibilityReasonCode::AcceleratorDeviceCountMismatch)
        }),
        error => {
            if matches!(
                error,
                ModelLaunchResolutionError::EngineArtifactSelection(
                    EngineArtifactSelectionError::MultipleMatches { .. }
                )
            ) {
                warn!(
                    model_id = %model_id.0,
                    profile_id = %profile.profile_id,
                    execution_target_id = %target.execution_target_id,
                    error = %error,
                    "catalog declares several engine artifacts for one target"
                );
            } else {
                debug!(
                    model_id = %model_id.0,
                    profile_id = %profile.profile_id,
                    execution_target_id = %target.execution_target_id,
                    error = %format!("{error:#}"),
                    "profile doesn't resolve on the target"
                );
            }
            cannot_run(reason(ModelCompatibilityReasonCode::ProfileUnresolvable))
        }
    }
}

/// Maps a target-level check failure to the status and typed reason shared by every profile on
/// the target.
fn target_failure(
    error: &TargetCapabilitySelectionError,
) -> (ModelCompatibilityStatus, ModelCompatibilityReason) {
    let unknown = |code| (ModelCompatibilityStatus::Unknown, reason(code));
    let cannot_run = |reason| (ModelCompatibilityStatus::CannotRun, reason);
    match error {
        TargetCapabilitySelectionError::MissingObservation { .. }
        | TargetCapabilitySelectionError::StalePlatform { .. }
        | TargetCapabilitySelectionError::PlatformUnavailable { .. }
        | TargetCapabilitySelectionError::StaleAccelerator { .. } => {
            unknown(ModelCompatibilityReasonCode::TargetNotReporting)
        }
        TargetCapabilitySelectionError::NvidiaDriverUnavailable { .. } => {
            unknown(ModelCompatibilityReasonCode::NvidiaDriverUnknown)
        }
        TargetCapabilitySelectionError::AcceleratorMemoryUnavailable { .. } => {
            unknown(ModelCompatibilityReasonCode::AcceleratorMemoryUnknown)
        }
        TargetCapabilitySelectionError::MissingTargetPlatform
        | TargetCapabilitySelectionError::InvalidTargetPlatform(_) => {
            cannot_run(reason(ModelCompatibilityReasonCode::TargetPlatformInvalid))
        }
        TargetCapabilitySelectionError::PlatformMismatch {
            observed_operating_system,
            observed_architecture,
            required_operating_system,
            required_architecture,
            ..
        } => cannot_run(ModelCompatibilityReason {
            configured_platform: Some(oci_platform(
                required_operating_system,
                required_architecture,
            )),
            observed_platform: Some(oci_platform(
                observed_operating_system,
                observed_architecture,
            )),
            ..reason(ModelCompatibilityReasonCode::PlatformMismatch)
        }),
        // The observation can't tell a node without accelerators from one whose probe failed.
        TargetCapabilitySelectionError::AcceleratorUnavailable { .. } => {
            unknown(ModelCompatibilityReasonCode::NoAccelerator)
        }
    }
}

/// Returns the distinct selectors of the profile's approved engine artifacts whose kind both the
/// engine and the target's runtime can launch, in catalog order.
fn supported_selectors(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile: &ModelProfile,
    target: &ExecutionTarget,
) -> Vec<Selector> {
    let Ok(declarations) = catalog.local_model_declarations(
        model_id,
        &profile.profile_id,
        &profile.default_model_artifact_binding_id,
    ) else {
        return Vec::new();
    };
    let runtime_kinds = WorkloadArtifactKind::realizable_by(target);
    let engine_kinds = engine_artifact_kinds(declarations.engine_kind);
    let mut selectors = Vec::new();
    for artifact in declarations
        .approved_engine_artifacts
        .iter()
        .map(|approved| &approved.artifact)
    {
        if !engine_artifact_kind(artifact)
            .is_some_and(|kind| runtime_kinds.contains(&kind) && engine_kinds.contains(&kind))
        {
            continue;
        }
        for selector in &artifact.selectors {
            if !selectors.contains(selector) {
                selectors.push(selector.clone());
            }
        }
    }
    selectors
}

fn reason(code: ModelCompatibilityReasonCode) -> ModelCompatibilityReason {
    ModelCompatibilityReason {
        code: code as i32,
        ..Default::default()
    }
}

fn placement(target: &ExecutionTarget) -> ExactPlacement {
    ExactPlacement {
        site_node_id: target.site_node_id.clone(),
        execution_target_id: target.execution_target_id.clone(),
    }
}

fn oci_platform(operating_system: &str, architecture: &str) -> OciPlatform {
    OciPlatform {
        operating_system: operating_system.to_owned(),
        architecture: architecture.to_owned(),
        variant: None,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::timing::Timing;
    use std::{fs, path::Path, time::SystemTime};

    use dotcore_catalog::{Catalog, CatalogModelId, OciArtifactPolicy};
    use dotcore_domain::{ExecutionPlatform, MemoryArchitecture};
    use dotcore_proto::dotcore::{
        catalog::v1::{OciPlatform, Selector},
        deployment::v1::{ExactPlacement, MemoryFit},
        model::v1::{
            HardwareClassResourceEstimate, ListModelProfileCompatibilityResponse,
            LocalModelDeploymentAdvisory, ModelCompatibilityReason, ModelCompatibilityReasonCode,
            ModelCompatibilityStatus, ModelProfileCompatibility, ModelProfileSizing,
            SizingScenarioUnsupportedReason, TargetCompatibility,
        },
        site::v1::{
            ExecutionTarget, ModelRuntimeBinding, execution_target::RuntimeBinding,
            model_runtime_binding::Adapter,
        },
    };
    use dotcore_site::node::target_inventory::{
        NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
        TargetCapabilityInventory, TargetCapabilityReconciler,
    };

    use super::{
        StartingTotalContextSource, evaluate_profile_compatibility, node_workloads,
        profile_failure, reason,
    };
    use crate::{
        model::{resolution::ModelLaunchResolutionError, selection::EngineArtifactSelectionError},
        test_support::unsigned_parameter_values,
    };

    #[test]
    fn local_deployments_hold_their_node_memory_by_phase_and_claim_it_until_ready() {
        use dotcore_proto::dotcore::model::v1::{ModelDeploymentStatus, model_deployment_snapshot};

        use crate::hardware::capacity::{Estimate, Workload, WorkloadKey, WorkloadPhase};

        let (target, inventory) =
            crate::test_support::execution_target_capabilities(ExecutionPlatform::linux_amd64());
        let deployment = |status: ModelDeploymentStatus, attributed: Option<(&str, u64)>| {
            sized_deployment(
                &target,
                status,
                attributed.map(|(node, bytes)| attributed_memory(node, bytes)),
            )
        };
        let workload = |status: ModelDeploymentStatus, phase, attributed: Option<u64>| Workload {
            key: WorkloadKey::inference(status.as_str_name()),
            phase,
            // Only starting and ready deployments are expected to hold memory.
            expects_use: phase != WorkloadPhase::Other,
            memory_architecture: MemoryArchitecture::Dedicated,
            device_ids: vec!["GPU-0".to_owned()],
            estimate: Estimate {
                accelerator_bytes: Some(3 * GIB),
                host_bytes: Some(GIB),
            },
            attributed: attributed.map(|bytes| [("GPU-0".to_owned(), bytes)].into()),
        };
        let mut unsized_deployment = deployment(ModelDeploymentStatus::Downloading, None);
        if let Some(model_deployment_snapshot::Source::Local(local)) = unsized_deployment
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        {
            local.sizing = None;
        }

        let workloads = node_workloads(
            &[
                deployment(ModelDeploymentStatus::Queued, None),
                deployment(
                    ModelDeploymentStatus::Loading,
                    Some((&target.site_node_id, GIB)),
                ),
                // Use reported on another node never counts as this deployment's.
                deployment(ModelDeploymentStatus::WarmingUp, Some(("other-node", GIB))),
                deployment(ModelDeploymentStatus::Ready, None),
                deployment(ModelDeploymentStatus::Paused, None),
                deployment(ModelDeploymentStatus::Failed, None),
                deployment(ModelDeploymentStatus::Unavailable, None),
                // Not sized, it still claims, of a size nothing knows.
                unsized_deployment,
            ],
            Some(&inventory),
        );

        use ModelDeploymentStatus::{
            Downloading, Failed, Loading, Paused, Queued, Ready, Unavailable, WarmingUp,
        };
        use WorkloadPhase::{Other, Starting};
        assert_eq!(
            workloads,
            [(
                target.site_node_id.clone(),
                vec![
                    workload(Queued, Starting, None),
                    workload(Loading, Starting, Some(GIB)),
                    workload(WarmingUp, Starting, None),
                    workload(Ready, WorkloadPhase::Ready, None),
                    workload(Paused, Other, None),
                    workload(Failed, Other, None),
                    workload(Unavailable, Other, None),
                    Workload {
                        estimate: Estimate::default(),
                        ..workload(Downloading, Starting, None)
                    },
                ],
            )]
            .into()
        );
    }

    #[test]
    fn a_ready_deployment_keeps_its_claim_until_its_memory_is_measured() {
        use dotcore_proto::dotcore::{
            deployment::v1::AcceleratorMemoryObservation, model::v1::ModelDeploymentStatus,
        };
        use dotcore_site::node::target_inventory::{
            CapabilityObservation, DeviceMemoryFacts, HostMemoryFacts, NodeMemoryFacts,
        };

        use crate::hardware::{
            attribution::unavailable,
            capacity::{NodeHardware, Segment, SegmentSource, TargetCapacity, WorkloadKey},
        };

        const RESERVED: u64 = 512 * 1024 * 1024;
        let (target, inventory) =
            crate::test_support::execution_target_capabilities(ExecutionPlatform::linux_amd64());
        // The ready deployment's device pool when the GPU reads `used_bytes`, holding one segment.
        let pool = |memory: AcceleratorMemoryObservation, used_bytes: u64| {
            let workloads = node_workloads(
                &[sized_deployment(
                    &target,
                    ModelDeploymentStatus::Ready,
                    Some(memory),
                )],
                Some(&inventory),
            );
            let hardware =
                NodeHardware::observed(&CapabilityObservation::Available(NodeMemoryFacts {
                    devices: vec![DeviceMemoryFacts {
                        device_id: "GPU-0".to_owned(),
                        model: String::new(),
                        vendor_index: Some(0),
                        total_bytes: Some(32 * GIB),
                        used_bytes: Some(used_bytes),
                    }],
                    host: Some(HostMemoryFacts {
                        total_bytes: 64 * GIB,
                        available_bytes: 60 * GIB,
                    }),
                }));
            let capacity = TargetCapacity::for_target(
                dotcore_domain::MemoryArchitecture::Dedicated,
                &hardware,
                Some(&["GPU-0".to_owned()]),
            )
            .with_workloads(&workloads[&target.site_node_id]);
            let pool = capacity
                .accelerator_pools()
                .next()
                .expect("device pool")
                .occupancy
                .clone();
            (pool.claimed_bytes, pool.workloads)
        };
        let segment = |source, bytes| {
            vec![Segment {
                source,
                workload: Some(WorkloadKey::inference("MODEL_DEPLOYMENT_STATUS_READY")),
                bytes,
            }]
        };

        // Ready before a reading shows it: the reading holds only the reserved memory and the
        // whole estimate stays claimed beside it.
        assert_eq!(
            pool(
                unavailable("WORKLOAD_PROCESS_MEMORY_PENDING", "pending"),
                RESERVED
            ),
            (Some(3 * GIB), segment(SegmentSource::Claimed, 3 * GIB))
        );

        // Measured, the claim ends and its use is what the reading attributes to it.
        assert_eq!(
            pool(
                attributed_memory(&target.site_node_id, 3 * GIB),
                RESERVED + 3 * GIB
            ),
            (Some(0), segment(SegmentSource::Measured, 3 * GIB))
        );

        // Not observed by a reading that should show it, the claim ends too: the estimate stands
        // only for what the reading measures.
        assert_eq!(
            pool(
                unavailable("PROCESS_NOT_OBSERVED", "not observed"),
                RESERVED
            ),
            (Some(0), segment(SegmentSource::Estimated, RESERVED))
        );

        // A reading that failed is not pending, so the claim ends rather than counting twice.
        assert_eq!(
            pool(
                unavailable("WORKLOAD_PROCESS_MEMORY_UNAVAILABLE", "no reading"),
                RESERVED
            ),
            (Some(0), segment(SegmentSource::Estimated, RESERVED))
        );
    }

    #[test]
    fn profiles_run_with_their_advisories_in_deterministic_order() {
        let catalog = catalog(|model| {
            model
                .replace(
                    r#"minimumSystemMemoryBytes: "1""#,
                    "minimumSystemMemoryBytes: \"1\"\n          minimumMemoryBytesPerDevice: \"64000000000\"",
                )
                .replace(
                    "profiles:\n",
                    "profiles:\n  - profileId: \"second\"\n    displayName: \"Second\"\n    configurationId: \"fixture-configuration\"\n    defaultModelArtifactBindingId: \"fixture-artifact\"\n    engineParameterDefaults: { values: [{ parameterId: \"prompt-cache-memory-mib\", unsignedInteger: \"2048\" }] }\n",
                )
        });
        let now = SystemTime::now();
        let dedicated = docker_target("dedicated", "node-b", ExecutionPlatform::linux_amd64(), 1);
        let unified = docker_target("unified", "node-a", ExecutionPlatform::linux_arm64(), 1);
        let mut inventory = observed(
            &dedicated,
            ExecutionPlatform::linux_amd64(),
            nvidia(&dedicated, "580.173.02", now),
            now,
        );
        inventory.extend(observed(
            &unified,
            ExecutionPlatform::linux_arm64(),
            nvidia(&unified, "580.173.02", now)
                .with_memory_architecture(Some(MemoryArchitecture::Unified))
                .with_host_memory(34_000_000_000, 34_000_000_000),
            now,
        ));

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &[dedicated.clone(), unified.clone()],
            None,
            Some(&inventory),
            &Default::default(),
        );

        let runs = |profile: &str, target: &ExecutionTarget, advisories: Vec<i32>| {
            ModelProfileCompatibility {
                model_id: "fixture-model".to_owned(),
                profile_id: profile.to_owned(),
                target: Some(placement(target)),
                status: ModelCompatibilityStatus::Runs as i32,
                reason: None,
                advisories,
                hardware_class_id: "fixture-nvidia".to_owned(),
                // The 64 GB per-device minimum is above both targets' memory.
                memory_fit: MemoryFit::Exceeds as i32,
                free_memory_fit: MemoryFit::Exceeds as i32,
                unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
                largest_total_context_tokens: None,
                // Without a largest fit, a deployment starts from the profile's default.
                starting_total_context_tokens: Some(32_768),
                starting_total_context_source: StartingTotalContextSource::ProfileDefault as i32,
            }
        };
        let below_minimum =
            vec![LocalModelDeploymentAdvisory::UnifiedMemoryPoolBelowDeviceMinimum as i32];
        assert_eq!(
            response,
            ListModelProfileCompatibilityResponse {
                targets: vec![
                    target_status(&unified, ModelCompatibilityStatus::Runs, None, now),
                    target_status(&dedicated, ModelCompatibilityStatus::Runs, None, now),
                ],
                entries: vec![
                    runs("second", &unified, below_minimum.clone()),
                    runs("second", &dedicated, Vec::new()),
                    runs("fixture-profile", &unified, below_minimum),
                    runs("fixture-profile", &dedicated, Vec::new()),
                ],
                profiles: vec![
                    unsized_profile("second", 2048),
                    unsized_profile("fixture-profile", 0)
                ],
            }
        );
    }

    #[test]
    fn each_single_gpu_target_of_a_node_runs_a_single_gpu_profile() {
        let catalog = catalog(|model| model);
        let now = SystemTime::now();
        let gpu0 = docker_target("gpu0", "node", ExecutionPlatform::linux_amd64(), 1);
        let gpu1 = ExecutionTarget {
            accelerator_device_ids: vec!["nvidia.com/gpu=1".to_owned()],
            ..docker_target("gpu1", "node", ExecutionPlatform::linux_amd64(), 1)
        };
        let mut inventory = observed(
            &gpu0,
            ExecutionPlatform::linux_amd64(),
            nvidia(&gpu0, "580.173.02", now),
            now,
        );
        inventory.extend(observed(
            &gpu1,
            ExecutionPlatform::linux_amd64(),
            nvidia(&gpu1, "580.173.02", now),
            now,
        ));

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &[gpu0.clone(), gpu1.clone()],
            None,
            Some(&inventory),
            &Default::default(),
        );

        assert_eq!(
            response
                .entries
                .iter()
                .map(|entry| (
                    entry.target.clone(),
                    ModelCompatibilityStatus::try_from(entry.status).expect("status")
                ))
                .collect::<Vec<_>>(),
            [
                (Some(placement(&gpu0)), ModelCompatibilityStatus::Runs),
                (Some(placement(&gpu1)), ModelCompatibilityStatus::Runs),
            ]
        );
    }

    #[test]
    fn target_failures_are_reported_once_on_the_target() {
        let catalog = catalog(|model| model);
        let now = SystemTime::now();
        let mismatch = docker_target("mismatch", "node-a", ExecutionPlatform::linux_arm64(), 1);
        let no_accelerator = docker_target(
            "no-accelerator",
            "node-b",
            ExecutionPlatform::linux_amd64(),
            1,
        );
        let stale = docker_target("stale", "node-c", ExecutionPlatform::linux_amd64(), 1);
        let missing = docker_target("missing", "node-d", ExecutionPlatform::linux_amd64(), 1);
        let no_driver = docker_target("no-driver", "node-e", ExecutionPlatform::linux_amd64(), 1);
        let mut inventory = observed(
            &mismatch,
            ExecutionPlatform::linux_amd64(),
            nvidia(&mismatch, "580.173.02", now),
            now,
        );
        inventory.extend(observed(
            &no_accelerator,
            ExecutionPlatform::linux_amd64(),
            NodeAcceleratorObservation::unavailable(&no_accelerator.site_node_id),
            now,
        ));
        inventory.extend(TargetCapabilityReconciler::reconcile(
            [(
                stale.execution_target_id.as_str(),
                stale.site_node_id.as_str(),
            )],
            [RuntimePlatformObservation::stale_at(
                &stale.execution_target_id,
                ExecutionPlatform::linux_amd64(),
                now,
            )],
            [nvidia(&stale, "580.173.02", now)],
            Timing::test().node_control.capability_max_age,
            now,
        ));
        inventory.extend(observed(
            &no_driver,
            ExecutionPlatform::linux_amd64(),
            nvidia(&no_driver, "not-a-version", now),
            now,
        ));
        let targets = [
            mismatch.clone(),
            no_accelerator.clone(),
            stale.clone(),
            missing.clone(),
            no_driver.clone(),
        ];

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &targets,
            None,
            Some(&inventory),
            &Default::default(),
        );

        let unknown = ModelCompatibilityStatus::Unknown;
        let cannot_run = ModelCompatibilityStatus::CannotRun;
        assert_eq!(
            response,
            ListModelProfileCompatibilityResponse {
                targets: vec![
                    target_status(
                        &mismatch,
                        cannot_run,
                        Some(ModelCompatibilityReason {
                            configured_platform: Some(oci("linux", "arm64")),
                            observed_platform: Some(oci("linux", "amd64")),
                            ..reason(ModelCompatibilityReasonCode::PlatformMismatch)
                        }),
                        now,
                    ),
                    target_status(
                        &no_accelerator,
                        unknown,
                        Some(reason(ModelCompatibilityReasonCode::NoAccelerator)),
                        now,
                    ),
                    target_status(
                        &stale,
                        unknown,
                        Some(reason(ModelCompatibilityReasonCode::TargetNotReporting)),
                        now,
                    ),
                    TargetCompatibility {
                        observed_at: None,
                        ..target_status(
                            &missing,
                            unknown,
                            Some(reason(ModelCompatibilityReasonCode::TargetNotReporting)),
                            now,
                        )
                    },
                    target_status(
                        &no_driver,
                        unknown,
                        Some(reason(ModelCompatibilityReasonCode::NvidiaDriverUnknown)),
                        now,
                    ),
                ],
                entries: Vec::new(),
                profiles: vec![unsized_profile("fixture-profile", 0)],
            }
        );

        let unobserved = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            std::slice::from_ref(&mismatch),
            None,
            None,
            &Default::default(),
        );
        assert_eq!(
            unobserved,
            ListModelProfileCompatibilityResponse {
                targets: vec![TargetCompatibility {
                    observed_at: None,
                    ..target_status(
                        &mismatch,
                        unknown,
                        Some(reason(ModelCompatibilityReasonCode::TargetNotReporting)),
                        now,
                    )
                }],
                entries: Vec::new(),
                profiles: vec![unsized_profile("fixture-profile", 0)],
            }
        );
    }

    #[test]
    fn profile_failures_carry_typed_reasons() {
        let catalog = catalog(|model| model);
        let now = SystemTime::now();
        let apple = docker_target("apple", "node-a", ExecutionPlatform::linux_amd64(), 1);
        let old_driver = docker_target("old-driver", "node-b", ExecutionPlatform::linux_amd64(), 1);
        let native = ExecutionTarget {
            runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(Adapter::NativeRuntime(Default::default())),
            })),
            ..docker_target("native", "node-c", ExecutionPlatform::linux_amd64(), 1)
        };
        let two_devices =
            docker_target("two-devices", "node-d", ExecutionPlatform::linux_amd64(), 2);
        let no_devices = docker_target("no-devices", "node-e", ExecutionPlatform::linux_amd64(), 0);
        let mut inventory = observed(
            &apple,
            ExecutionPlatform::linux_amd64(),
            NodeAcceleratorObservation::available_at(
                &apple.site_node_id,
                NodeAcceleratorFacts::apple(),
                now,
            ),
            now,
        );
        inventory.extend(observed(
            &old_driver,
            ExecutionPlatform::linux_amd64(),
            nvidia(&old_driver, "570.86.10", now),
            now,
        ));
        inventory.extend(observed(
            &native,
            ExecutionPlatform::linux_amd64(),
            nvidia(&native, "580.173.02", now),
            now,
        ));
        inventory.extend(observed(
            &two_devices,
            ExecutionPlatform::linux_amd64(),
            nvidia(&two_devices, "580.173.02", now),
            now,
        ));
        inventory.extend(observed(
            &no_devices,
            ExecutionPlatform::linux_amd64(),
            nvidia(&no_devices, "580.173.02", now),
            now,
        ));

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &[
                apple.clone(),
                old_driver.clone(),
                native.clone(),
                two_devices.clone(),
                no_devices.clone(),
            ],
            None,
            Some(&inventory),
            &Default::default(),
        );

        let cannot_run =
            |profile: &str, target: &ExecutionTarget, reason| ModelProfileCompatibility {
                model_id: "fixture-model".to_owned(),
                profile_id: profile.to_owned(),
                target: Some(placement(target)),
                status: ModelCompatibilityStatus::CannotRun as i32,
                reason: Some(reason),
                advisories: Vec::new(),
                hardware_class_id: String::new(),
                memory_fit: MemoryFit::Unspecified as i32,
                free_memory_fit: MemoryFit::Unspecified as i32,
                unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
                largest_total_context_tokens: None,
                starting_total_context_tokens: None,
                starting_total_context_source: StartingTotalContextSource::Unspecified as i32,
            };
        let linux_nvidia = |architecture: &str| Selector {
            platform: Some(oci("linux", architecture)),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            ..Default::default()
        };
        assert_eq!(
            response.entries,
            [
                cannot_run(
                    "fixture-profile",
                    &apple,
                    ModelCompatibilityReason {
                        supported_selectors: vec![linux_nvidia("amd64"), linux_nvidia("arm64")],
                        ..reason(ModelCompatibilityReasonCode::NoMatchingEngineArtifact)
                    }
                ),
                cannot_run(
                    "fixture-profile",
                    &old_driver,
                    ModelCompatibilityReason {
                        minimum_nvidia_driver_major_version: 580,
                        observed_nvidia_driver_major_version: 570,
                        ..reason(ModelCompatibilityReasonCode::NvidiaDriverBelowMinimum)
                    }
                ),
                cannot_run(
                    "fixture-profile",
                    &native,
                    reason(ModelCompatibilityReasonCode::NoMatchingEngineArtifact)
                ),
                cannot_run(
                    "fixture-profile",
                    &two_devices,
                    ModelCompatibilityReason {
                        accelerator_device_count: 2,
                        required_accelerator_device_count: 1,
                        ..reason(ModelCompatibilityReasonCode::AcceleratorDeviceCountMismatch)
                    }
                ),
                cannot_run(
                    "fixture-profile",
                    &no_devices,
                    ModelCompatibilityReason {
                        accelerator_device_count: 0,
                        required_accelerator_device_count: 1,
                        ..reason(ModelCompatibilityReasonCode::AcceleratorDeviceCountMismatch)
                    }
                ),
            ]
        );
        assert!(
            response
                .targets
                .iter()
                .all(|target| target.status == ModelCompatibilityStatus::Runs as i32),
            "{:?}",
            response.targets
        );
    }

    #[test]
    fn mac_targets_report_no_matching_artifact_for_nvidia_only_oci_profiles() {
        let catalog = catalog(|model| model);
        let now = SystemTime::now();
        let docker_desktop = docker_target(
            "docker-desktop",
            "mac-a",
            ExecutionPlatform::linux_arm64(),
            1,
        );
        let darwin = ExecutionPlatform::darwin_arm64();
        let native = ExecutionTarget {
            runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(Adapter::NativeRuntime(Default::default())),
            })),
            ..docker_target("native", "mac-b", darwin.clone(), 1)
        };
        let apple = |target: &ExecutionTarget| {
            NodeAcceleratorObservation::available_at(
                &target.site_node_id,
                NodeAcceleratorFacts::apple(),
                now,
            )
        };
        let mut inventory = observed(
            &docker_desktop,
            ExecutionPlatform::linux_arm64(),
            apple(&docker_desktop),
            now,
        );
        inventory.extend(observed(&native, darwin, apple(&native), now));

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &[docker_desktop.clone(), native.clone()],
            None,
            Some(&inventory),
            &Default::default(),
        );

        let linux_nvidia = |architecture: &str| Selector {
            platform: Some(oci("linux", architecture)),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            ..Default::default()
        };
        let reasons = response
            .entries
            .iter()
            .map(|entry| (entry.target.clone(), entry.status, entry.reason.clone()))
            .collect::<Vec<_>>();
        let cannot_run = ModelCompatibilityStatus::CannotRun as i32;
        assert_eq!(
            reasons,
            [
                (
                    Some(placement(&docker_desktop)),
                    cannot_run,
                    Some(ModelCompatibilityReason {
                        supported_selectors: vec![linux_nvidia("amd64"), linux_nvidia("arm64")],
                        ..reason(ModelCompatibilityReasonCode::NoMatchingEngineArtifact)
                    })
                ),
                (
                    Some(placement(&native)),
                    cannot_run,
                    Some(reason(
                        ModelCompatibilityReasonCode::NoMatchingEngineArtifact
                    ))
                ),
            ]
        );
    }

    #[test]
    fn profiles_whose_hardware_classes_miss_a_matched_target_report_no_supported_class() {
        let root = catalog_root(|model| {
            model.replace(
                r#"hardwareClassId: "fixture-nvidia""#,
                r#"hardwareClassId: "fixture-nvidia-arm64""#,
            )
        });
        fs::write(
            root.path()
                .join("hardware-classes/fixture-nvidia-arm64.yaml"),
            "hardwareClassId: \"fixture-nvidia-arm64\"\ndisplayName: \"Fixture NVIDIA arm64\"\nselector:\n  platform: { operatingSystem: \"linux\", architecture: \"arm64\" }\n  accelerators: [\"nvidia\"]\n",
        )
        .expect("narrow hardware class");
        let catalog = load_catalog(&root);
        let now = SystemTime::now();
        let amd64 = docker_target("amd64", "node-a", ExecutionPlatform::linux_amd64(), 1);
        let arm64 = docker_target("arm64", "node-b", ExecutionPlatform::linux_arm64(), 1);
        let mut inventory = observed(
            &amd64,
            ExecutionPlatform::linux_amd64(),
            nvidia(&amd64, "580.173.02", now),
            now,
        );
        inventory.extend(observed(
            &arm64,
            ExecutionPlatform::linux_arm64(),
            nvidia(&arm64, "580.173.02", now),
            now,
        ));

        let response = evaluate_profile_compatibility(
            &catalog,
            &catalog.list_model_releases(),
            &[amd64.clone(), arm64.clone()],
            None,
            Some(&inventory),
            &Default::default(),
        );

        assert_eq!(
            response
                .entries
                .iter()
                .map(|entry| (entry.target.clone(), entry.status, entry.reason.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    Some(placement(&amd64)),
                    ModelCompatibilityStatus::CannotRun as i32,
                    Some(reason(
                        ModelCompatibilityReasonCode::NoSupportedHardwareClass
                    )),
                ),
                (
                    Some(placement(&arm64)),
                    ModelCompatibilityStatus::Runs as i32,
                    None,
                ),
            ]
        );
    }

    #[test]
    fn launch_rule_failures_and_catalog_defects_read_as_unresolvable() {
        let catalog = catalog(|model| model);
        let model = catalog.list_model_releases().remove(0);
        let target = docker_target("target", "node", ExecutionPlatform::linux_amd64(), 1);
        for error in [
            ModelLaunchResolutionError::LaunchPlan(anyhow::anyhow!(
                "vLLM launch plan has no target memory-pool total"
            )),
            ModelLaunchResolutionError::EngineArtifactSelection(
                EngineArtifactSelectionError::MultipleMatches {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    accelerator: "nvidia",
                },
            ),
        ] {
            assert_eq!(
                profile_failure(
                    &catalog,
                    &CatalogModelId(model.model_id.clone()),
                    &model.profiles[0],
                    &target,
                    &error,
                ),
                (
                    ModelCompatibilityStatus::CannotRun,
                    reason(ModelCompatibilityReasonCode::ProfileUnresolvable)
                ),
                "{error}"
            );
        }
    }

    /// Loads the simulated-site fixture catalog with its model declaration rewritten by `edit`.
    fn catalog(edit: impl FnOnce(String) -> String) -> Catalog {
        load_catalog(&catalog_root(edit))
    }

    /// Writes the simulated-site fixture catalog with its model declaration rewritten by `edit`.
    fn catalog_root(edit: impl FnOnce(String) -> String) -> tempfile::TempDir {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog");
        let root = tempfile::tempdir().expect("catalog root");
        for directory in ["engines", "models", "hardware-classes"] {
            fs::create_dir_all(root.path().join(directory)).expect("catalog directory");
        }
        fs::copy(
            fixture.join("hardware-classes/fixture-nvidia.yaml"),
            root.path().join("hardware-classes/fixture-nvidia.yaml"),
        )
        .expect("fixture hardware class");
        fs::copy(
            fixture.join("engines/fixture.yaml"),
            root.path().join("engines/fixture.yaml"),
        )
        .expect("fixture engine");
        let model =
            fs::read_to_string(fixture.join("models/fixture-model.yaml")).expect("fixture model");
        fs::write(root.path().join("models/fixture-model.yaml"), edit(model))
            .expect("edited model");
        root
    }

    fn load_catalog(root: &tempfile::TempDir) -> Catalog {
        Catalog::load_with_policy(
            root.path(),
            OciArtifactPolicy::Development,
            crate::model::engine::declaration_validator(),
        )
        .expect("edited fixture catalog")
    }

    fn docker_target(
        target_id: &str,
        node_id: &str,
        platform: ExecutionPlatform,
        devices: usize,
    ) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: target_id.to_owned(),
            display_name: target_id.to_owned(),
            site_node_id: node_id.to_owned(),
            platform: Some(platform.to_oci_platform()),
            accelerator_device_ids: (0..devices)
                .map(|device| format!("nvidia.com/gpu={device}"))
                .collect(),
            runtime_binding: Some(crate::test_support::docker_model_runtime_binding()),
        }
    }

    /// NVIDIA accelerator observed at `now` with a 34 GB memory pool.
    fn nvidia(
        target: &ExecutionTarget,
        driver: &str,
        now: SystemTime,
    ) -> NodeAcceleratorObservation {
        NodeAcceleratorObservation::available_at(
            &target.site_node_id,
            NodeAcceleratorFacts::nvidia(driver).with_device_memory(
                "GPU-0",
                Some(34_000_000_000),
                None,
            ),
            now,
        )
    }

    /// Inventory for one target whose runtime reports `platform` at `now`, read at `now`.
    fn observed(
        target: &ExecutionTarget,
        platform: ExecutionPlatform,
        accelerator: NodeAcceleratorObservation,
        now: SystemTime,
    ) -> TargetCapabilityInventory {
        TargetCapabilityReconciler::reconcile(
            [target],
            [RuntimePlatformObservation::available_at(
                &target.execution_target_id,
                platform,
                now,
            )],
            [accelerator],
            Timing::test().node_control.capability_max_age,
            now,
        )
    }

    fn target_status(
        target: &ExecutionTarget,
        status: ModelCompatibilityStatus,
        reason: Option<ModelCompatibilityReason>,
        observed_at: SystemTime,
    ) -> TargetCompatibility {
        TargetCompatibility {
            target: Some(placement(target)),
            status: status as i32,
            reason,
            observed_at: Some(crate::model::service::timestamp_from(observed_at)),
        }
    }

    /// Sizing of a fixture profile at its defaults: the given prompt cache, then the engine default
    /// total context and parallel requests and the context per request filled from them, on the
    /// fixture class, which declares no sizing.
    fn unsized_profile(profile_id: &str, prompt_cache_mib: u64) -> ModelProfileSizing {
        ModelProfileSizing {
            model_id: "fixture-model".to_owned(),
            profile_id: profile_id.to_owned(),
            engine_parameter_values: Some(unsigned_parameter_values([
                ("prompt-cache-memory-mib", prompt_cache_mib),
                ("total-context", 32_768),
                ("parallel-requests", 4),
                ("context-per-request", 32_768),
            ])),
            hardware_class_estimates: vec![HardwareClassResourceEstimate {
                hardware_class_id: "fixture-nvidia".to_owned(),
                estimates: Vec::new(),
                derived_engine_parameter_values: Some(unsigned_parameter_values([(
                    "context-per-request",
                    32_768,
                )])),
                launch_rejection: None,
            }],
            unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
            unsupported_hardware_class_ids: Vec::new(),
        }
    }

    fn placement(target: &ExecutionTarget) -> ExactPlacement {
        ExactPlacement {
            site_node_id: target.site_node_id.clone(),
            execution_target_id: target.execution_target_id.clone(),
        }
    }

    fn oci(operating_system: &str, architecture: &str) -> OciPlatform {
        OciPlatform {
            operating_system: operating_system.to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A local deployment on `target` in `status`, named after it, whose dedicated estimate is
    /// 3 GiB of accelerator memory and 1 GiB of host memory, with the accelerator memory its node
    /// reports.
    fn sized_deployment(
        target: &ExecutionTarget,
        status: dotcore_proto::dotcore::model::v1::ModelDeploymentStatus,
        accelerator_memory: Option<
            dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryObservation,
        >,
    ) -> dotcore_proto::dotcore::model::v1::ModelDeployment {
        use dotcore_proto::dotcore::{
            model::v1::{
                ModelDeployment, ModelDeploymentIntent, ModelDeploymentState, ResolvedSizing,
                model_deployment_snapshot,
            },
            observation::v1::AcceleratorMemoryArchitecture,
        };

        let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
        let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local snapshot");
        };
        local.sizing = Some(ResolvedSizing {
            accelerator_memory_bytes: Some(3 * GIB),
            host_memory_bytes: Some(GIB),
            ..Default::default()
        });
        local
            .selected_target
            .as_mut()
            .expect("selected target")
            .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Dedicated as i32;
        ModelDeployment {
            model_deployment_id: status.as_str_name().to_owned(),
            intent: Some(ModelDeploymentIntent {
                placement: Some(placement(target)),
                ..Default::default()
            }),
            snapshot: Some(snapshot),
            state: Some(ModelDeploymentState {
                status: status as i32,
                ..Default::default()
            }),
            accelerator_memory,
            ..Default::default()
        }
    }

    /// Accelerator memory a node attributes on its `GPU-0`.
    fn attributed_memory(
        node: &str,
        bytes: u64,
    ) -> dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryObservation {
        use dotcore_proto::dotcore::deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation, AcceleratorMemoryUsageSet,
            accelerator_memory_observation,
        };

        AcceleratorMemoryObservation {
            result: Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![AcceleratorMemoryDeviceUsage {
                        managed_node_id: node.to_owned(),
                        device_id: "GPU-0".to_owned(),
                        used_memory_bytes: bytes,
                    }],
                },
            )),
            ..Default::default()
        }
    }
}
