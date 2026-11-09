//! Inference controller lifecycle state projection and error mapping.

use dotcore_artifact::{ModelArtifactProgress, ModelArtifactStage};
use dotcore_domain::{real_time, valid_id};
use dotcore_proto::dotcore::model::v1::{
    CurrentModelDeployment, ModelDeploymentActivation, ModelDeploymentActivationPhase,
    ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentFailure,
    ModelDeploymentProgress, ModelDeploymentProgressUnit, ModelDeploymentState,
    ModelDeploymentStatus, ObservedPlacement,
};
use dotcore_proto::google::protobuf::Timestamp;
use dotcore_runtime::{ObservedWorkload, WorkloadObservation};
use dotcore_site::repository::SiteRepositoryError;

use super::ModelLifecycleError;
use crate::{
    inference::{EngineBinding, EngineReadiness},
    model::{engine::EngineIntegrationRegistry, state::CurrentStateError},
};

pub(super) enum RegisteredObservationStatus {
    Immediate(ModelDeploymentStatus),
    Bound {
        binding: EngineBinding,
        expected_model_id: String,
    },
}

pub(super) fn register_observation_status(
    engine_integrations: &EngineIntegrationRegistry,
    desired: &ModelDeploymentDeclaration,
    observation: &WorkloadObservation,
) -> Result<RegisteredObservationStatus, ModelLifecycleError> {
    let lifecycle = match observation {
        WorkloadObservation::Starting(_) => ModelDeploymentStatus::Loading,
        WorkloadObservation::Running(workload) => {
            let resolved = crate::model::resolved::ResolvedModelLaunch::from_declaration(desired)
                .map_err(catalog_configuration)?;
            let service = workload
                .services
                .iter()
                .find(|service| service.reference.service_name == "http")
                .ok_or_else(|| configuration("Runtime returned no http service binding"))?;
            let binding = engine_integrations
                .bind_workload(&resolved, &desired.model_deployment_id, service)
                .map_err(|error| ModelLifecycleError::EngineIntegration(error.into()))?;
            return Ok(RegisteredObservationStatus::Bound {
                binding,
                expected_model_id: resolved.model_id.0,
            });
        }
        WorkloadObservation::Stopping(_) => ModelDeploymentStatus::Unloading,
        WorkloadObservation::Stopped { .. } | WorkloadObservation::Unavailable(_) => {
            ModelDeploymentStatus::Unavailable
        }
        WorkloadObservation::Absent(_) | WorkloadObservation::Unknown { .. } => {
            return Err(configuration("Runtime returned no lifecycle observation"));
        }
    };
    Ok(RegisteredObservationStatus::Immediate(lifecycle))
}

pub(super) async fn resolve_observation_status(
    registered: RegisteredObservationStatus,
) -> Result<(ModelDeploymentStatus, Option<EngineBinding>), ModelLifecycleError> {
    let RegisteredObservationStatus::Bound {
        binding,
        expected_model_id,
    } = registered
    else {
        let RegisteredObservationStatus::Immediate(lifecycle) = registered else {
            unreachable!()
        };
        return Ok((lifecycle, None));
    };
    let (_, cancellation) = crate::inference::inference_cancellation();
    let readiness = binding
        .probe()
        .probe(cancellation)
        .await
        .map_err(|error| ModelLifecycleError::EngineIntegration(error.into()))?;
    Ok(match readiness {
        EngineReadiness::Loading => (ModelDeploymentStatus::Loading, None),
        EngineReadiness::Ready { served_model_id } if served_model_id == expected_model_id => {
            (ModelDeploymentStatus::Ready, Some(binding))
        }
        EngineReadiness::Ready { .. } => (ModelDeploymentStatus::Unavailable, None),
    })
}

#[cfg(test)]
pub(super) async fn status_from_observation(
    engine_integrations: &EngineIntegrationRegistry,
    desired: &ModelDeploymentDeclaration,
    observation: &WorkloadObservation,
) -> Result<(ModelDeploymentStatus, Option<EngineBinding>), ModelLifecycleError> {
    resolve_observation_status(register_observation_status(
        engine_integrations,
        desired,
        observation,
    )?)
    .await
}

pub(super) fn apply_startup_failure<S: super::CurrentStateRepository>(
    state: &S,
    desired: &ModelDeploymentDeclaration,
    diagnostics: Vec<String>,
) -> Result<bool, ModelLifecycleError> {
    let snapshot = state.snapshot().map_err(current_error)?;
    let existing = snapshot
        .model_deployments
        .into_iter()
        .find(|value| value.model_deployment_id == desired.model_deployment_id);
    let Some(previous) = existing.as_ref().and_then(|record| record.state.as_ref()) else {
        return Ok(false);
    };
    if !startup_pending(previous) {
        return Ok(false);
    }
    state
        .upsert(
            CurrentModelDeployment {
                model_deployment_id: desired.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(engine_startup_failed_state(previous, diagnostics)),
            },
            existing.map(|value| value.record_revision),
        )
        .map_err(current_error)?;
    Ok(true)
}

pub(super) fn apply_observation<S: super::CurrentStateRepository>(
    state: &S,
    desired: &ModelDeploymentDeclaration,
    observation: &WorkloadObservation,
    lifecycle: ModelDeploymentStatus,
    execution_target: Option<&dotcore_runtime::ExecutionTarget>,
) -> Result<(), ModelLifecycleError> {
    let snapshot = state.snapshot().map_err(current_error)?;
    let existing = snapshot
        .model_deployments
        .into_iter()
        .find(|value| value.model_deployment_id == desired.model_deployment_id);
    let mut next_state = deployment_status_state(
        lifecycle,
        observation_identity(observation),
        execution_target,
    );
    let prior = existing.as_ref().and_then(|record| record.state.as_ref());
    let activation = prior
        .and_then(|state| state.activation)
        .filter(|activation| {
            activation.workload_generation.is_none_or(|generation| {
                observation_identity(observation)
                    .is_some_and(|workload| workload.correlation.reconcile_generation == generation)
            })
        });
    if reconciled_generation(existing.as_ref(), desired)
        && prior.is_some_and(|state| {
            state.status == lifecycle as i32
                && state.observed_placements == next_state.observed_placements
                && (lifecycle != ModelDeploymentStatus::Loading || state.activation == activation)
        })
    {
        return Ok(());
    }
    if lifecycle == ModelDeploymentStatus::Loading {
        next_state.activation = activation;
    }
    state
        .upsert(
            CurrentModelDeployment {
                model_deployment_id: desired.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(next_state),
            },
            existing.map(|value| value.record_revision),
        )
        .map_err(current_error)?;
    Ok(())
}

/// Reports whether a retained record already reflects the declaration's lifecycle generation, so
/// an unchanged status may skip its write. A record from another generation, or one written
/// before generations were recorded, must be rewritten to carry the current generation.
pub(super) fn reconciled_generation(
    record: Option<&CurrentModelDeployment>,
    desired: &ModelDeploymentDeclaration,
) -> bool {
    record.is_some_and(|record| {
        record.reconciled_lifecycle_generation == Some(desired.lifecycle_generation)
    })
}

/// Rebinds a retained record whose Resume activation or failure belongs to another lifecycle
/// generation. An unconfirmed Start from newer intent the manager lost becomes an uncertain
/// failure instead of a retry, while one superseded by a newer Resume restarts that Resume as a
/// paused record would. Any other activation moves to the declared generation, and a failure
/// stays terminal. Returns `None` when no rebinding applies.
pub(super) fn record_for_declared_generation(
    record: &CurrentModelDeployment,
    desired: &ModelDeploymentDeclaration,
) -> Option<CurrentModelDeployment> {
    let generation = desired.lifecycle_generation;
    let state = record.state.as_ref()?;
    let state = match state.activation.as_ref() {
        Some(activation) if activation.lifecycle_generation != generation => {
            if resume_phase(state) == ModelDeploymentActivationPhase::StartInFlight {
                if activation.lifecycle_generation > generation {
                    resume_start_uncertain_state(state)
                } else {
                    // A pause and resume the node never observed separate the unconfirmed
                    // Start from the declared Resume, which waits for its own Start.
                    resuming_state(generation)
                }
            } else {
                let mut state = state.clone();
                if let Some(activation) = state.activation.as_mut() {
                    activation.lifecycle_generation = generation;
                }
                state
            }
        }
        _ if state.status == ModelDeploymentStatus::Failed as i32
            && record.reconciled_lifecycle_generation != Some(generation)
            && !resumes_after_unobserved_pause(record, desired) =>
        {
            state.clone()
        }
        _ => return None,
    };
    Some(CurrentModelDeployment {
        reconciled_lifecycle_generation: Some(generation),
        state: Some(state),
        ..record.clone()
    })
}

/// Reports whether an explicit Resume at a newer generation follows a failure whose pause was
/// never observed, so the failed record resumes as a paused one would.
pub(super) fn resumes_after_unobserved_pause(
    record: &CurrentModelDeployment,
    desired: &ModelDeploymentDeclaration,
) -> bool {
    record
        .reconciled_lifecycle_generation
        .is_some_and(|generation| generation < desired.lifecycle_generation)
        && desired.lifecycle_transition
            == dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Resume as i32
}

/// Plain message shown for a public status when no more specific progress message applies.
pub(super) fn status_message(status: ModelDeploymentStatus) -> &'static str {
    match status {
        ModelDeploymentStatus::Queued => "Queued",
        ModelDeploymentStatus::Preparing => "Preparing",
        ModelDeploymentStatus::Downloading => "Downloading",
        ModelDeploymentStatus::Loading => "Loading",
        ModelDeploymentStatus::WarmingUp => "Warming up",
        ModelDeploymentStatus::Ready => "Ready",
        ModelDeploymentStatus::Unloading => "Unloading",
        ModelDeploymentStatus::Failed => "Failed",
        ModelDeploymentStatus::Unavailable => "Unavailable",
        ModelDeploymentStatus::Pausing => "Pausing",
        ModelDeploymentStatus::Paused => "Paused",
        ModelDeploymentStatus::Unspecified => "Unknown",
    }
}

const CHECKING_MODEL_ARTIFACTS: &str = "Checking model artifacts";
const VERIFYING_MODEL_ARTIFACTS: &str = "Verifying model artifacts";
const PUBLISHING_MODEL_ARTIFACTS: &str = "Publishing model artifacts";
const REUSING_MODEL_ARTIFACTS: &str = "Reusing model artifacts";
const DOWNLOADING_MODEL_ARTIFACTS: &str = "Downloading model artifacts";
const ACQUIRING_ENGINE_IMAGE: &str = "Acquiring engine image";
const STARTING_MODEL_REALIZATION: &str = "Starting model realization";
// Compared by the controller to find a realization awaiting startup.
const PREPARING_MODEL_REALIZATION: &str = "Preparing model realization";
// Messages a preparation stage writes to the record. The "Preparing model realization"
// placeholder and runtime-provided download texts are not stage messages and stay excluded.
const PREPARATION_MESSAGES: [&str; 7] = [
    CHECKING_MODEL_ARTIFACTS,
    VERIFYING_MODEL_ARTIFACTS,
    PUBLISHING_MODEL_ARTIFACTS,
    REUSING_MODEL_ARTIFACTS,
    DOWNLOADING_MODEL_ARTIFACTS,
    ACQUIRING_ENGINE_IMAGE,
    STARTING_MODEL_REALIZATION,
];

pub(super) fn loading_state() -> ModelDeploymentState {
    deployment_status_state(ModelDeploymentStatus::Loading, None, None)
}

pub(super) fn first_realization_pending_state() -> ModelDeploymentState {
    download_progress_state(
        None,
        ModelArtifactProgress {
            completed: 0,
            total: None,
        },
        PREPARING_MODEL_REALIZATION,
    )
}

pub(super) fn first_realization_pending(state: &ModelDeploymentState) -> bool {
    ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Downloading)
        && state.status_message == PREPARING_MODEL_REALIZATION
}

pub(super) fn realization_startup_admitted_state(
    previous: Option<&ModelDeploymentState>,
) -> Option<ModelDeploymentState> {
    let previous = previous.filter(|state| first_realization_pending(state))?;
    let mut state = previous.clone();
    state.status_message = STARTING_MODEL_REALIZATION.to_owned();
    state.updated_at = Some(now());
    Some(state)
}

pub(super) fn checking_model_artifacts_state(
    previous: Option<&ModelDeploymentState>,
) -> Option<ModelDeploymentState> {
    let previous = previous?;
    if ModelDeploymentStatus::try_from(previous.status) != Ok(ModelDeploymentStatus::Downloading) {
        return None;
    }

    let mut state = previous.clone();
    state.status = ModelDeploymentStatus::Loading as i32;
    state.status_message = CHECKING_MODEL_ARTIFACTS.to_owned();
    state.progress = None;
    state.updated_at = Some(now());
    state.failure = None;
    Some(state)
}
pub(super) fn artifact_preparation_stage_state(
    previous: Option<&ModelDeploymentState>,
    stage: ModelArtifactStage,
) -> Option<ModelDeploymentState> {
    let previous = previous?;
    let status = ModelDeploymentStatus::try_from(previous.status).ok()?;
    if !matches!(
        status,
        ModelDeploymentStatus::Loading | ModelDeploymentStatus::Downloading
    ) && !(status == ModelDeploymentStatus::Ready && stage == ModelArtifactStage::Publish)
    {
        return None;
    }

    let message = match stage {
        ModelArtifactStage::Verify => VERIFYING_MODEL_ARTIFACTS,
        ModelArtifactStage::Publish => PUBLISHING_MODEL_ARTIFACTS,
        ModelArtifactStage::Reuse => REUSING_MODEL_ARTIFACTS,
    };
    if status == ModelDeploymentStatus::Loading
        && previous.status_message == message
        && previous.progress.is_none()
    {
        return None;
    }

    let mut state = previous.clone();
    state.status = ModelDeploymentStatus::Loading as i32;
    state.status_message = message.to_owned();
    state.progress = None;
    state.updated_at = Some(now());
    state.failure = None;
    Some(state)
}

pub(super) fn downloading_state(
    previous: Option<&ModelDeploymentState>,
    progress: ModelArtifactProgress,
) -> ModelDeploymentState {
    download_progress_state(previous, progress, DOWNLOADING_MODEL_ARTIFACTS)
}

pub(super) fn runtime_startup_progress_state(
    previous: Option<&ModelDeploymentState>,
    progress: ModelArtifactProgress,
    status_message: &str,
) -> ModelDeploymentState {
    download_progress_state(previous, progress, status_message)
}

fn download_progress_state(
    previous: Option<&ModelDeploymentState>,
    progress: ModelArtifactProgress,
    status_message: &str,
) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    let percent = progress
        .total
        .map(|total| ((u128::from(progress.completed) * 100) / u128::from(total)) as u32);
    state.status = ModelDeploymentStatus::Downloading as i32;
    state.status_message = status_message.to_owned();
    state.observed_placements.clear();
    state.progress = Some(ModelDeploymentProgress {
        percent,
        completed: progress.completed,
        total: progress.total,
        unit: ModelDeploymentProgressUnit::Bytes as i32,
    });
    state.updated_at = Some(now());
    state.failure = None;
    state
}

pub(super) fn resuming_state(lifecycle_generation: u64) -> ModelDeploymentState {
    let mut state = loading_state();
    state.status_message = "Resuming".to_owned();
    state.activation = Some(ModelDeploymentActivation {
        phase: ModelDeploymentActivationPhase::AwaitingStart as i32,
        workload_generation: None,
        lifecycle_generation,
    });
    state
}

pub(super) fn resume_phase(state: &ModelDeploymentState) -> ModelDeploymentActivationPhase {
    state
        .activation
        .as_ref()
        .and_then(|activation| ModelDeploymentActivationPhase::try_from(activation.phase).ok())
        .unwrap_or(ModelDeploymentActivationPhase::Unspecified)
}

pub(super) fn resume_matches_generation(state: &ModelDeploymentState, generation: u64) -> bool {
    state
        .activation
        .as_ref()
        .is_some_and(|activation| activation.workload_generation == Some(generation))
}
pub(super) fn resume_matches_lifecycle(
    state: &ModelDeploymentState,
    lifecycle_generation: u64,
) -> bool {
    state.activation.as_ref().is_some_and(|activation| {
        lifecycle_generation != 0 && activation.lifecycle_generation == lifecycle_generation
    })
}

pub(super) fn resume_start_state(
    previous: &ModelDeploymentState,
    phase: ModelDeploymentActivationPhase,
    generation: u64,
) -> ModelDeploymentState {
    let mut state = previous.clone();
    state.activation = (phase != ModelDeploymentActivationPhase::Unspecified).then_some(
        ModelDeploymentActivation {
            phase: phase as i32,
            workload_generation: Some(generation),
            lifecycle_generation: previous
                .activation
                .as_ref()
                .map_or(0, |activation| activation.lifecycle_generation),
        },
    );
    state.updated_at = Some(now());
    state
}

pub(super) fn startup_pending(state: &ModelDeploymentState) -> bool {
    ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Loading)
        && !matches!(
            resume_phase(state),
            ModelDeploymentActivationPhase::AwaitingStart
                | ModelDeploymentActivationPhase::StartInFlight
        )
}

pub(super) fn resume_start_uncertain_state(
    previous: &ModelDeploymentState,
) -> ModelDeploymentState {
    let mut state = previous.clone();
    state.status = ModelDeploymentStatus::Failed as i32;
    state.status_message = "Resume start outcome uncertain".to_owned();
    state.progress = None;
    state.updated_at = Some(now());
    state.activation = None;
    state.failure = Some(ModelDeploymentFailure {
        code: "resume_start_uncertain".to_owned(),
        message: "Could not confirm whether the retained workload started; inspect the workload before changing the deployment".to_owned(),
        diagnostics: Vec::new(),
    });
    state
}

/// Terminal state for a Resume that has neither retained operational state nor an observed
/// workload, such as a teardown the manager lost and recovered as older Present intent. Nothing
/// identifies what to resume, so the deployment waits for an explicit lifecycle change.
pub(super) fn resume_recovery_required_state() -> ModelDeploymentState {
    failed_state(
        None,
        "Recovery required",
        ModelDeploymentFailure {
            code: "resume_recovery_required".to_owned(),
            message: "No retained state or workload exists to resume; unload the deployment and create it again".to_owned(),
            diagnostics: Vec::new(),
        },
    )
}

pub(super) fn previously_realized(state: &ModelDeploymentState) -> bool {
    matches!(
        ModelDeploymentStatus::try_from(state.status),
        Ok(ModelDeploymentStatus::WarmingUp
            | ModelDeploymentStatus::Ready
            | ModelDeploymentStatus::Unavailable
            | ModelDeploymentStatus::Pausing
            | ModelDeploymentStatus::Paused)
    ) || (ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Loading)
        && state.status_message == "Resuming")
}

pub(super) fn realization_prepared_state(
    previous: Option<&ModelDeploymentState>,
) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Loading as i32;
    state.status_message = status_message(ModelDeploymentStatus::Loading).to_owned();
    state.progress = None;
    state.updated_at = Some(now());
    state.failure = None;
    state
}

pub(super) fn acquiring_engine_image_state(
    previous: Option<&ModelDeploymentState>,
) -> Option<ModelDeploymentState> {
    if previous.is_some_and(|state| {
        ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Ready)
    }) {
        return None;
    }
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Loading as i32;
    state.status_message = ACQUIRING_ENGINE_IMAGE.to_owned();
    state.progress = None;
    state.updated_at = Some(now());
    state.failure = None;
    Some(state)
}

/// Replaces a leftover preparation message with the generic Loading one once preparation has
/// finished. Only a Loading record still showing a preparation message is rewritten, so a Ready
/// record is never downgraded when preparation re-runs on a later pass.
pub(super) fn preparation_completed_state(
    previous: Option<&ModelDeploymentState>,
) -> Option<ModelDeploymentState> {
    let previous = previous.filter(|state| {
        ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Loading)
            && PREPARATION_MESSAGES.contains(&state.status_message.as_str())
    })?;
    Some(realization_prepared_state(Some(previous)))
}

pub(super) fn engine_image_acquisition_failed_state(
    previous: Option<&ModelDeploymentState>,
    diagnostics: Vec<String>,
) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Failed as i32;
    state.status_message = "Engine image acquisition failed".to_owned();
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state.failure = Some(ModelDeploymentFailure {
        code: "engine_image_acquisition_failed".to_owned(),
        message: "Engine image acquisition failed; unload and deploy the model again".to_owned(),
        diagnostics,
    });
    state
}

/// Diagnostics of a failed engine artifact preparation: the stage and output tail of a Docker
/// image derivation loader that failed, and none for other failures.
pub(super) fn engine_image_acquisition_diagnostics(
    loader_output_tail: Option<&str>,
) -> Vec<String> {
    loader_output_tail
        .map(|output| {
            vec![
                "Stage: engine image acquisition".to_owned(),
                format!("Output tail: {output}"),
            ]
        })
        .unwrap_or_default()
}

pub(super) fn engine_startup_failed_state(
    previous: &ModelDeploymentState,
    diagnostics: Vec<String>,
) -> ModelDeploymentState {
    let mut state = previous.clone();
    state.status = ModelDeploymentStatus::Failed as i32;
    state.status_message = "Engine startup failed".to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state.failure = Some(ModelDeploymentFailure {
        code: "engine_startup_failed".to_owned(),
        message:
            "Engine stopped during startup; correct the engine arguments and deploy the model again"
                .to_owned(),
        diagnostics,
    });
    state
}

pub(super) fn startup_failure_diagnostics(
    exit_status: Option<i64>,
    output_tail: Option<String>,
) -> Vec<String> {
    let mut diagnostics = vec!["Stage: engine startup".to_owned()];
    if let Some(status) = exit_status {
        diagnostics.push(format!("Exit status: {status}"));
    }
    if let Some(output) = output_tail {
        diagnostics.push(format!("Output tail: {output}"));
    }
    diagnostics
}

pub(super) fn unloading_state(previous: Option<&ModelDeploymentState>) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Unloading as i32;
    state.status_message = status_message(ModelDeploymentStatus::Unloading).to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state
}

pub(super) fn pausing_state(previous: Option<&ModelDeploymentState>) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Pausing as i32;
    state.status_message = status_message(ModelDeploymentStatus::Pausing).to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state
}

pub(super) fn paused_state(previous: Option<&ModelDeploymentState>) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Paused as i32;
    state.status_message = status_message(ModelDeploymentStatus::Paused).to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state
}

pub(super) fn unavailable_state(previous: Option<&ModelDeploymentState>) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Unavailable as i32;
    state.status_message = status_message(ModelDeploymentStatus::Unavailable).to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state
}

pub(super) fn failed_state(
    previous: Option<&ModelDeploymentState>,
    status_message: &str,
    failure: ModelDeploymentFailure,
) -> ModelDeploymentState {
    let mut state = previous.cloned().unwrap_or_else(loading_state);
    state.status = ModelDeploymentStatus::Failed as i32;
    state.status_message = status_message.to_owned();
    state.activation = None;
    state.progress = None;
    state.observed_placements.clear();
    state.updated_at = Some(now());
    state.failure = Some(failure);
    state
}

pub(super) fn deployment_status_state(
    lifecycle: ModelDeploymentStatus,
    workload: Option<&ObservedWorkload>,
    execution_target: Option<&dotcore_runtime::ExecutionTarget>,
) -> ModelDeploymentState {
    let timestamp = now();
    ModelDeploymentState {
        status: lifecycle as i32,
        activation: None,
        status_message: match lifecycle {
            ModelDeploymentStatus::Ready
            | ModelDeploymentStatus::Unloading
            | ModelDeploymentStatus::Unavailable => status_message(lifecycle),
            _ => status_message(ModelDeploymentStatus::Loading),
        }
        .to_owned(),
        progress: None,
        started_at: Some(timestamp),
        updated_at: Some(timestamp),
        failure: None,
        observed_placements: observed_placement(lifecycle, workload, execution_target)
            .into_iter()
            .collect(),
    }
}

pub(super) fn state_from_observation(
    observation: &WorkloadObservation,
) -> Option<ModelDeploymentState> {
    let lifecycle = match observation {
        WorkloadObservation::Starting(_) | WorkloadObservation::Running(_) => {
            ModelDeploymentStatus::Loading
        }
        WorkloadObservation::Stopping(_) => ModelDeploymentStatus::Unloading,
        WorkloadObservation::Stopped { .. } | WorkloadObservation::Unavailable(_) => {
            ModelDeploymentStatus::Unavailable
        }
        WorkloadObservation::Absent(_) | WorkloadObservation::Unknown { .. } => return None,
    };
    Some(deployment_status_state(
        lifecycle,
        observation_identity(observation),
        None,
    ))
}

pub(super) fn desired_state(
    desired: &ModelDeploymentDeclaration,
) -> Result<ModelDeploymentDesiredState, ModelLifecycleError> {
    match ModelDeploymentDesiredState::try_from(desired.desired_state) {
        Ok(ModelDeploymentDesiredState::Present) => Ok(ModelDeploymentDesiredState::Present),
        Ok(
            state @ (ModelDeploymentDesiredState::Teardown | ModelDeploymentDesiredState::Paused),
        ) => Ok(state),
        Ok(ModelDeploymentDesiredState::Unspecified) => Err(configuration(
            "desired deployment must declare PRESENT, PAUSED, or TEARDOWN",
        )),
        Err(_) => Err(configuration(
            "desired deployment has an invalid desired state",
        )),
    }
}

pub(super) fn is_absent(desired: &ModelDeploymentDeclaration) -> bool {
    matches!(
        desired_state(desired),
        Ok(ModelDeploymentDesiredState::Teardown)
    )
}

pub(super) fn observed_placement(
    lifecycle: ModelDeploymentStatus,
    workload: Option<&ObservedWorkload>,
    execution_target: Option<&dotcore_runtime::ExecutionTarget>,
) -> Option<ObservedPlacement> {
    if lifecycle != ModelDeploymentStatus::Ready {
        return None;
    }
    let (Some(_workload), Some(execution_target)) = (workload, execution_target) else {
        return None;
    };
    Some(ObservedPlacement {
        managed_node_id: execution_target.managed_node_id.to_string(),
        accelerator_device_ids: execution_target.accelerator_device_ids.clone(),
    })
}

pub(super) fn observation_identity(observation: &WorkloadObservation) -> Option<&ObservedWorkload> {
    match observation {
        WorkloadObservation::Starting(value)
        | WorkloadObservation::Running(value)
        | WorkloadObservation::Stopping(value)
        | WorkloadObservation::Unavailable(value)
        | WorkloadObservation::Absent(value) => Some(value),
        WorkloadObservation::Stopped { workload, .. } => Some(workload),
        WorkloadObservation::Unknown { .. } => None,
    }
}

pub(super) fn validate_id(value: &str) -> Result<(), ModelLifecycleError> {
    if !valid_id(value) {
        Err(ModelLifecycleError::InvalidArgument)
    } else {
        Ok(())
    }
}

pub(super) fn now() -> Timestamp {
    let duration = real_time::wall_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    }
}

pub(super) fn desired_error(error: SiteRepositoryError) -> ModelLifecycleError {
    ModelLifecycleError::SiteRepository(error)
}

pub(super) fn current_error(error: CurrentStateError) -> ModelLifecycleError {
    ModelLifecycleError::from_current_state(error)
}

pub(super) fn partial_current_error(
    operation: &str,
    error: CurrentStateError,
) -> ModelLifecycleError {
    ModelLifecycleError::OutcomeUncertain(anyhow::Error::new(error).context(format!(
        "{operation} accepted desired deployment intent but could not persist current state; retry the operation"
    )))
}

pub(super) fn configuration(message: impl Into<String>) -> ModelLifecycleError {
    ModelLifecycleError::configuration(message)
}

pub(super) fn configuration_with_source(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ModelLifecycleError {
    ModelLifecycleError::Configuration(anyhow::Error::new(source).context(message.into()))
}

pub(super) fn catalog_configuration(error: dotcore_catalog::CatalogError) -> ModelLifecycleError {
    ModelLifecycleError::CatalogConfiguration(error)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use dotcore_proto::dotcore::model::v1::{
        ModelDeploymentDesiredState, ModelDeploymentFailure, ModelDeploymentState,
        ModelDeploymentStatus, ObservedPlacement,
    };
    use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadObservation};
    use dotcore_site::{persistence::SiteStorageError, repository::SiteRepositoryError};

    use super::super::ModelLifecycleError;
    use super::{
        acquiring_engine_image_state, apply_observation, artifact_preparation_stage_state,
        checking_model_artifacts_state, configuration_with_source, current_error,
        deployment_status_state, desired_error, downloading_state, engine_startup_failed_state,
        first_realization_pending_state, loading_state, partial_current_error, pausing_state,
        preparation_completed_state, previously_realized, realization_startup_admitted_state,
        resuming_state, runtime_startup_progress_state, status_from_observation,
    };
    use crate::model::{
        controller::{
            CurrentStateRepository as _,
            test_support::{
                TestCurrent, expected_identity, test_desired_deployment, test_engine_integrations,
                test_ninfer_desired_deployment, test_site,
            },
        },
        repository::ModelDeclarationRepositoryError,
        state::CurrentStateError,
    };

    #[test]
    fn unchanged_observation_for_a_new_generation_records_that_generation() {
        let current = TestCurrent::new(Arc::new(Mutex::new(Vec::new())));
        let mut desired = test_desired_deployment("model", ModelDeploymentDesiredState::Present);
        let observation = WorkloadObservation::Starting(expected_identity(
            &test_site(),
            &desired,
            "runtime/test",
        ));
        apply_observation(
            &current,
            &desired,
            &observation,
            ModelDeploymentStatus::Loading,
            None,
        )
        .expect("first observation");
        let first = current.snapshot().expect("state").model_deployments[0].clone();
        desired.lifecycle_generation = 2;

        apply_observation(
            &current,
            &desired,
            &observation,
            ModelDeploymentStatus::Loading,
            None,
        )
        .expect("same observation for the next generation");
        let second = current.snapshot().expect("state").model_deployments[0].clone();

        assert_eq!(first.reconciled_lifecycle_generation, Some(1));
        assert_eq!(second.reconciled_lifecycle_generation, Some(2));
        assert_eq!(second.record_revision, first.record_revision + 1);
        assert_eq!(
            second.state.expect("second state").status,
            ModelDeploymentStatus::Loading as i32
        );
    }

    #[test]
    fn configuration_failures_retain_context_and_concrete_sources() {
        let error = configuration_with_source(
            "failed to create upstream verification client",
            std::io::Error::other("client unavailable"),
        );
        let ModelLifecycleError::Configuration(error) = error else {
            panic!("verification client must remain a configuration category");
        };

        assert_eq!(
            error.to_string(),
            "failed to create upstream verification client"
        );
        assert_eq!(
            error
                .chain()
                .find_map(|source| source.downcast_ref::<std::io::Error>())
                .expect("concrete client source")
                .to_string(),
            "client unavailable"
        );
    }

    #[tokio::test]
    async fn transient_readiness_probe_maps_to_loading_controller_state() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        let mut workload = expected_identity(&test_site(), &desired, "runtime/test");
        workload.loopback_endpoints = vec![LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        }];
        workload.services = vec![dotcore_runtime::WorkloadServiceBinding::loopback(
            workload.workload_id.0.clone(),
            "http",
            workload.correlation.reconcile_generation,
            LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: port,
            },
        )];

        assert_eq!(
            status_from_observation(
                &crate::model::controller::test_support::test_engine_integrations(),
                &desired,
                &WorkloadObservation::Running(workload),
            )
            .await
            .expect("transient readiness result")
            .0,
            ModelDeploymentStatus::Loading
        );
    }

    #[tokio::test]
    async fn ninfer_deployment_becomes_ready_and_serves_a_chat_completion_over_loopback() {
        let engine = FakeNinferEngine::start().await;
        let desired = test_ninfer_desired_deployment("qwen3-8-27b");
        let mut workload = expected_identity(&test_site(), &desired, "runtime/test");
        let endpoint = LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: engine.port,
        };
        workload.loopback_endpoints = vec![endpoint.clone()];
        workload.services = vec![dotcore_runtime::WorkloadServiceBinding::loopback(
            workload.workload_id.0.clone(),
            "http",
            workload.correlation.reconcile_generation,
            endpoint,
        )];
        let observation = WorkloadObservation::Running(workload);
        let integrations = test_engine_integrations();

        let started = std::time::Instant::now();
        let (loading, binding) = status_from_observation(&integrations, &desired, &observation)
            .await
            .expect("loading status");
        assert_eq!(loading, ModelDeploymentStatus::Loading);
        assert!(binding.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "loading probe took {:?}",
            started.elapsed()
        );

        engine
            .ready
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (ready, binding) = status_from_observation(&integrations, &desired, &observation)
            .await
            .expect("ready status");
        assert_eq!(ready, ModelDeploymentStatus::Ready);
        let binding = binding.expect("ready engine binding");

        let (_, cancellation) = crate::inference::inference_cancellation();
        let attempt = binding
            .invoker()
            .invoke(
                dotcore_proto::dotcore::inference::v1::ChatRequest {
                    model_id: "route-name".to_owned(),
                    messages: vec![dotcore_proto::dotcore::inference::v1::ChatMessage {
                        role: dotcore_proto::dotcore::inference::v1::ChatMessageRole::User as i32,
                        content: Some(
                            dotcore_proto::dotcore::inference::v1::chat_message::Content::TextContent(
                                "hello".to_owned(),
                            ),
                        ),
                        ..Default::default()
                    }],
                    stream: Some(false),
                    ..Default::default()
                },
                cancellation,
            )
            .await;
        let crate::inference::InferenceAttempt::Response(
            crate::inference::InferenceResponse::Complete(completion),
        ) = attempt
        else {
            panic!("ninfer chat completion");
        };
        assert_eq!(completion.model_id, "route-name");
        assert_eq!(
            engine.chat_models.lock().expect("chat models").as_slice(),
            ["qwen3-8-27b"]
        );
    }

    #[test]
    fn ready_state_reports_exact_selected_execution_placement() {
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        let workload = expected_identity(&test_site(), &desired, "runtime/test");
        let target = ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node-two"),
            accelerator_device_ids: vec!["GPU-abc".to_owned(), "GPU-def".to_owned()],
        };

        let state =
            deployment_status_state(ModelDeploymentStatus::Ready, Some(&workload), Some(&target));

        assert_eq!(
            state.observed_placements,
            vec![ObservedPlacement {
                managed_node_id: "node-two".to_owned(),
                accelerator_device_ids: vec!["GPU-abc".to_owned(), "GPU-def".to_owned()],
            }]
        );
    }

    #[test]
    fn non_ready_state_does_not_claim_observed_placement() {
        let desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        let workload = expected_identity(&test_site(), &desired, "runtime/test");
        let target = ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node-two"),
            accelerator_device_ids: vec!["GPU-abc".to_owned()],
        };

        for lifecycle in [
            ModelDeploymentStatus::Loading,
            ModelDeploymentStatus::Unloading,
            ModelDeploymentStatus::Unavailable,
        ] {
            let state = deployment_status_state(lifecycle, Some(&workload), Some(&target));
            assert!(state.observed_placements.is_empty(), "{lifecycle:?}");
        }
    }

    #[test]
    fn previously_realized_requires_lifecycle_evidence() {
        assert!(!previously_realized(&loading_state()));
        assert!(!previously_realized(&downloading_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactProgress {
                completed: 0,
                total: None,
            },
        )));
        assert!(previously_realized(&resuming_state(1)));

        let mut ready = loading_state();
        ready.status = ModelDeploymentStatus::Ready as i32;
        ready.status_message = "Ready".to_owned();
        assert!(previously_realized(&ready));
    }

    #[test]
    fn engine_startup_failure_reports_actionable_deployment_state() {
        let previous = loading_state();

        let failed = engine_startup_failed_state(
            &previous,
            super::startup_failure_diagnostics(Some(17), None),
        );

        assert_eq!(failed.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(failed.status_message, "Engine startup failed");
        assert_eq!(failed.progress, None);
        assert_eq!(
            failed.failure,
            Some(ModelDeploymentFailure {
                code: "engine_startup_failed".to_owned(),
                message: "Engine stopped during startup; correct the engine arguments and deploy the model again"
                    .to_owned(),
                diagnostics: vec!["Stage: engine startup".to_owned(), "Exit status: 17".to_owned()],
            })
        );
    }
    #[test]
    fn engine_image_acquisition_failures_carry_the_loader_output_tail() {
        let state = super::engine_image_acquisition_failed_state(
            None,
            super::engine_image_acquisition_diagnostics(Some(
                "docker-image-vllm: pulling\ndocker-image-vllm: error: pull failed",
            )),
        );

        let failure = state.failure.expect("failure");
        assert_eq!(failure.code, "engine_image_acquisition_failed");
        assert_eq!(
            failure.diagnostics,
            [
                "Stage: engine image acquisition",
                "Output tail: docker-image-vllm: pulling\ndocker-image-vllm: error: pull failed",
            ]
        );
        assert!(super::engine_image_acquisition_diagnostics(None).is_empty());
    }

    #[test]
    fn startup_failure_diagnostics_preserve_exit_status_without_a_log_tail() {
        assert_eq!(
            super::startup_failure_diagnostics(
                Some(17),
                Some("FileNotFoundError: [REDACTED]".to_owned())
            ),
            vec![
                "Stage: engine startup",
                "Exit status: 17",
                "Output tail: FileNotFoundError: [REDACTED]",
            ]
        );
        assert_eq!(
            super::startup_failure_diagnostics(Some(1), None),
            vec!["Stage: engine startup", "Exit status: 1"]
        );
    }

    #[test]
    fn download_state_exposes_exact_known_and_unknown_byte_progress() {
        let known = downloading_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactProgress {
                completed: 40,
                total: Some(100),
            },
        );
        assert_eq!(known.status, ModelDeploymentStatus::Downloading as i32);
        assert_eq!(known.status_message, "Downloading model artifacts");
        assert_eq!(
            known.progress,
            Some(dotcore_proto::dotcore::model::v1::ModelDeploymentProgress {
                percent: Some(40),
                completed: 40,
                total: Some(100),
                unit: dotcore_proto::dotcore::model::v1::ModelDeploymentProgressUnit::Bytes as i32,
            })
        );

        let unknown = downloading_state(
            Some(&known),
            dotcore_artifact::ModelArtifactProgress {
                completed: 60,
                total: None,
            },
        );
        assert_eq!(
            unknown.progress,
            Some(dotcore_proto::dotcore::model::v1::ModelDeploymentProgress {
                percent: None,
                completed: 60,
                total: None,
                unit: dotcore_proto::dotcore::model::v1::ModelDeploymentProgressUnit::Bytes as i32,
            })
        );
    }

    #[test]
    fn local_preparation_steps_record_their_messages_under_their_status() {
        use ModelDeploymentStatus::{Downloading, Loading};
        use dotcore_artifact::{ModelArtifactProgress, ModelArtifactStage};
        let progress = ModelArtifactProgress {
            completed: 40,
            total: Some(100),
        };
        let downloading = downloading_state(Some(&loading_state()), progress.clone());
        let loading = loading_state();
        let cases = [
            (
                checking_model_artifacts_state(Some(&downloading)),
                "Checking model artifacts",
                Loading,
            ),
            (
                artifact_preparation_stage_state(Some(&loading), ModelArtifactStage::Verify),
                "Verifying model artifacts",
                Loading,
            ),
            (
                artifact_preparation_stage_state(Some(&loading), ModelArtifactStage::Publish),
                "Publishing model artifacts",
                Loading,
            ),
            (
                artifact_preparation_stage_state(Some(&loading), ModelArtifactStage::Reuse),
                "Reusing model artifacts",
                Loading,
            ),
            (
                Some(downloading.clone()),
                "Downloading model artifacts",
                Downloading,
            ),
            (
                acquiring_engine_image_state(None),
                "Acquiring engine image",
                Loading,
            ),
            (
                realization_startup_admitted_state(Some(&first_realization_pending_state())),
                "Starting model realization",
                Downloading,
            ),
            (
                Some(runtime_startup_progress_state(
                    None,
                    progress,
                    "Simulating model artifact download",
                )),
                "Simulating model artifact download",
                Downloading,
            ),
            (
                Some(first_realization_pending_state()),
                "Preparing model realization",
                Downloading,
            ),
        ];

        for (state, message, status) in cases {
            let state = state.expect(message);
            assert_eq!(state.status_message, message);
            assert_eq!(state.status, status as i32, "{message}");
        }
    }

    #[test]
    fn runtime_startup_progress_uses_runtime_detail() {
        let state = runtime_startup_progress_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactProgress {
                completed: 25,
                total: Some(100),
            },
            "Simulating model artifact download",
        );

        assert_eq!(state.status, ModelDeploymentStatus::Downloading as i32);
        assert_eq!(state.status_message, "Simulating model artifact download");
        assert_eq!(
            state.progress,
            Some(dotcore_proto::dotcore::model::v1::ModelDeploymentProgress {
                percent: Some(25),
                completed: 25,
                total: Some(100),
                unit: dotcore_proto::dotcore::model::v1::ModelDeploymentProgressUnit::Bytes as i32,
            })
        );
    }

    #[test]
    fn artifact_stage_state_shows_work_without_transfer_bytes_or_warm_ready_churn() {
        let downloading = downloading_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactProgress {
                completed: 40,
                total: Some(100),
            },
        );
        let verifying = artifact_preparation_stage_state(
            Some(&downloading),
            dotcore_artifact::ModelArtifactStage::Verify,
        )
        .expect("verification transition");
        assert_eq!(verifying.status, ModelDeploymentStatus::Loading as i32);
        assert_eq!(verifying.status_message, "Verifying model artifacts");
        assert_eq!(verifying.progress, None);
        let publishing = artifact_preparation_stage_state(
            Some(&verifying),
            dotcore_artifact::ModelArtifactStage::Publish,
        )
        .expect("publication transition");
        assert_eq!(publishing.status_message, "Publishing model artifacts");
        assert_eq!(publishing.progress, None);
        let reusing = artifact_preparation_stage_state(
            Some(&verifying),
            dotcore_artifact::ModelArtifactStage::Reuse,
        )
        .expect("reuse transition");
        assert_eq!(reusing.status_message, "Reusing model artifacts");
        assert_eq!(reusing.progress, None);

        let mut ready = loading_state();
        ready.status = ModelDeploymentStatus::Ready as i32;
        ready.status_message = "Ready".to_owned();
        assert_eq!(
            artifact_preparation_stage_state(
                Some(&ready),
                dotcore_artifact::ModelArtifactStage::Verify
            ),
            None
        );
        assert_eq!(
            artifact_preparation_stage_state(
                Some(&ready),
                dotcore_artifact::ModelArtifactStage::Reuse
            ),
            None
        );
        assert_eq!(ready.status, ModelDeploymentStatus::Ready as i32);
    }

    #[test]
    fn warm_ready_state_is_not_rewritten_for_engine_image_recheck() {
        let mut ready = loading_state();
        ready.status = ModelDeploymentStatus::Ready as i32;
        ready.status_message = "Ready".to_owned();
        assert_eq!(acquiring_engine_image_state(Some(&ready)), None);
        assert_eq!(
            acquiring_engine_image_state(Some(&loading_state()))
                .expect("initial image acquisition")
                .status_message,
            "Acquiring engine image"
        );
    }

    #[test]
    fn finished_engine_acquisition_replaces_the_preparation_message_with_loading() {
        let mut acquiring = acquiring_engine_image_state(None).expect("engine acquisition");
        acquiring.activation = resuming_state(3).activation;
        assert!(acquiring.activation.is_some());

        let completed = preparation_completed_state(Some(&acquiring)).expect("completed state");

        assert_eq!(completed.status, ModelDeploymentStatus::Loading as i32);
        assert_eq!(completed.status_message, "Loading");
        assert_eq!(completed.progress, None);
        assert_eq!(completed.failure, None);
        assert_eq!(completed.activation, acquiring.activation);
        assert_eq!(completed.observed_placements, acquiring.observed_placements);
        assert_eq!(completed.started_at, acquiring.started_at);

        let verifying = artifact_preparation_stage_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactStage::Verify,
        )
        .expect("verify stage");
        assert_eq!(verifying.status_message, "Verifying model artifacts");
        assert_eq!(
            preparation_completed_state(Some(&verifying))
                .expect("completed verify stage")
                .status_message,
            "Loading"
        );
    }

    #[test]
    fn finished_engine_acquisition_leaves_other_records_untouched() {
        let mut ready = loading_state();
        ready.status = ModelDeploymentStatus::Ready as i32;
        ready.status_message = "Ready".to_owned();
        let mut unavailable = loading_state();
        unavailable.status = ModelDeploymentStatus::Unavailable as i32;
        unavailable.status_message = "Unavailable".to_owned();
        let mut failed = acquiring_engine_image_state(None).expect("engine acquisition");
        failed.status = ModelDeploymentStatus::Failed as i32;
        let progress = dotcore_artifact::ModelArtifactProgress {
            completed: 40,
            total: Some(100),
        };
        let downloading = downloading_state(Some(&loading_state()), progress.clone());
        let runtime_assets =
            runtime_startup_progress_state(None, progress, "Simulating model artifact download");

        for state in [
            ready,
            unavailable,
            failed,
            resuming_state(3),
            loading_state(),
            downloading,
            runtime_assets,
        ] {
            assert_eq!(
                preparation_completed_state(Some(&state)),
                None,
                "{} / {}",
                state.status,
                state.status_message
            );
        }
        assert_eq!(preparation_completed_state(None), None);
    }

    #[test]
    fn stale_download_state_is_cleared_only_when_download_was_persisted() {
        let downloading = downloading_state(
            Some(&loading_state()),
            dotcore_artifact::ModelArtifactProgress {
                completed: 40,
                total: Some(100),
            },
        );
        let checking = checking_model_artifacts_state(Some(&downloading))
            .expect("stale download state must be cleared");
        assert_eq!(checking.status, ModelDeploymentStatus::Loading as i32);
        assert_eq!(checking.status_message, "Checking model artifacts");
        assert_eq!(checking.progress, None);

        let mut resuming = loading_state();
        resuming.status_message = "Resuming".to_owned();
        assert_eq!(checking_model_artifacts_state(Some(&resuming)), None);
        assert_eq!(checking_model_artifacts_state(None), None);
    }

    #[test]
    fn resume_activation_survives_preparation_stages_and_reconstruction() {
        let initial = super::resuming_state(1);
        let mut state = initial.clone();
        for stage in [
            dotcore_artifact::ModelArtifactStage::Verify,
            dotcore_artifact::ModelArtifactStage::Reuse,
            dotcore_artifact::ModelArtifactStage::Publish,
        ] {
            state =
                artifact_preparation_stage_state(Some(&state), stage).expect("preparation stage");
            assert_eq!(state.activation, initial.activation);
            assert!(!super::startup_pending(&state));
        }
        let encoded = prost::Message::encode_to_vec(&state);
        let restored = <ModelDeploymentState as prost::Message>::decode(encoded.as_slice())
            .expect("retained state");
        assert_eq!(restored.activation, initial.activation);
        assert!(!super::startup_pending(&restored));
    }

    #[test]
    fn pausing_state_preserves_existing_details_while_changing_lifecycle() {
        let previous = ModelDeploymentState {
            status: ModelDeploymentStatus::Failed as i32,
            activation: None,
            status_message: "loading failed".to_owned(),
            progress: Some(dotcore_proto::dotcore::model::v1::ModelDeploymentProgress {
                percent: Some(37),
                completed: 37,
                total: Some(100),
                unit: dotcore_proto::dotcore::model::v1::ModelDeploymentProgressUnit::Bytes as i32,
            }),
            started_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 11,
                nanos: 12,
            }),
            updated_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 13,
                nanos: 14,
            }),
            failure: Some(dotcore_proto::dotcore::model::v1::ModelDeploymentFailure {
                code: "runtime".to_owned(),
                message: "failed".to_owned(),
                diagnostics: vec!["detail".to_owned()],
            }),
            observed_placements: Vec::new(),
        };
        let pausing = pausing_state(Some(&previous));
        assert_eq!(pausing.status, ModelDeploymentStatus::Pausing as i32);
        assert_eq!(pausing.status_message, "Pausing");
        assert_eq!(pausing.progress, None);
        assert_eq!(pausing.started_at, previous.started_at);
        assert_eq!(pausing.failure, previous.failure);
    }

    #[test]
    fn desired_state_accepts_paused_declarations() {
        let declaration =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Paused);
        assert_eq!(
            super::desired_state(&declaration).expect("paused desired state"),
            ModelDeploymentDesiredState::Paused
        );
    }

    #[test]
    fn desired_and_current_errors_retain_typed_sources_and_status_categories() {
        let desired = desired_error(SiteRepositoryError::Storage(SiteStorageError::Storage(
            anyhow!("injected desired storage failure"),
        )));
        assert!(matches!(
            &desired,
            ModelLifecycleError::SiteRepository(SiteRepositoryError::Storage(_))
        ));
        assert!(source_chain_contains(
            &desired,
            "injected desired storage failure"
        ));

        let current = current_error(CurrentStateError::Storage(anyhow!(
            "injected current-state storage failure"
        )));
        assert!(matches!(
            &current,
            ModelLifecycleError::CurrentState(CurrentStateError::Storage(_))
        ));
        assert!(source_chain_contains(
            &current,
            "injected current-state storage failure"
        ));

        assert!(matches!(
            ModelLifecycleError::from_model_repository(
                ModelDeclarationRepositoryError::DeploymentNotFound("deployment".to_owned())
            ),
            ModelLifecycleError::NotFound
        ));
        assert!(matches!(
            ModelLifecycleError::from_model_repository(
                ModelDeclarationRepositoryError::ExecutionTargetNotFound("target-one".to_owned())
            ),
            ModelLifecycleError::ExecutionTargetNotFound(target_id) if target_id == "target-one"
        ));
        assert!(matches!(
            ModelLifecycleError::from_model_repository(
                ModelDeclarationRepositoryError::InvalidTransition {
                    deployment_id: "deployment".to_owned(),
                    operation: "pause",
                }
            ),
            ModelLifecycleError::FailedPrecondition
        ));
        assert!(matches!(
            current_error(CurrentStateError::Poisoned),
            ModelLifecycleError::OutcomeUncertain(_)
        ));
    }

    #[test]
    fn partial_current_error_retains_operation_context_and_typed_source() {
        let error = partial_current_error("Load", CurrentStateError::Poisoned);

        assert_eq!(
            error.to_string(),
            "Model lifecycle outcome is uncertain: Load accepted desired deployment intent but could not persist current state; retry the operation"
        );
        let ModelLifecycleError::OutcomeUncertain(source) = &error else {
            panic!("partial current-state error should be outcome uncertain");
        };
        assert!(source.chain().any(|source| {
            source
                .downcast_ref::<CurrentStateError>()
                .is_some_and(|error| matches!(error, CurrentStateError::Poisoned))
        }));
    }

    fn source_chain_contains(error: &dyn std::error::Error, expected: &str) -> bool {
        let mut source = error.source();
        while let Some(current) = source {
            if current.to_string().contains(expected) {
                return true;
            }
            source = current.source();
        }
        false
    }

    /// Loopback stand-in for `ninfer-serve` that hangs every connection until marked ready.
    struct FakeNinferEngine {
        port: u16,
        ready: Arc<std::sync::atomic::AtomicBool>,
        chat_models: Arc<Mutex<Vec<String>>>,
    }

    impl FakeNinferEngine {
        async fn start() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("loopback listener");
            let port = listener.local_addr().expect("listener address").port();
            let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let chat_models = Arc::new(Mutex::new(Vec::new()));
            tokio::spawn({
                let ready = ready.clone();
                let chat_models = chat_models.clone();
                async move {
                    // Loading ninfer accepts connections and never answers, like the real engine.
                    let mut loading_connections = Vec::new();
                    loop {
                        let Ok((mut stream, _)) = listener.accept().await else {
                            return;
                        };
                        if !ready.load(std::sync::atomic::Ordering::SeqCst) {
                            loading_connections.push(stream);
                            continue;
                        }
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 4096];
                        let (head, body) = loop {
                            let read = stream.read(&mut buffer).await.expect("request bytes");
                            assert_ne!(read, 0, "request ended early");
                            request.extend_from_slice(&buffer[..read]);
                            let Some(end) =
                                request.windows(4).position(|window| window == b"\r\n\r\n")
                            else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .map_or(0, |value| value.trim().parse().expect("length"));
                            if request.len() >= end + 4 + length {
                                break (head, request[end + 4..end + 4 + length].to_vec());
                            }
                        };
                        let (status, body) = if head.starts_with("get /health") {
                            (200, r#"{"status":"ok"}"#.to_owned())
                        } else if head.starts_with("get /v1/models") {
                            (
                                200,
                                r#"{"object":"list","data":[{"id":"qwen3-8-27b","object":"model"}]}"#
                                    .to_owned(),
                            )
                        } else if head.starts_with("post /v1/chat/completions") {
                            let request: serde_json::Value =
                                serde_json::from_slice(&body).expect("chat request JSON");
                            chat_models
                                .lock()
                                .expect("chat models")
                                .push(request["model"].as_str().expect("model").to_owned());
                            (
                                200,
                                r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"qwen3-8-27b","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#
                                    .to_owned(),
                            )
                        } else {
                            (404, "{}".to_owned())
                        };
                        let response = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .await
                            .expect("response");
                    }
                }
            });
            Self {
                port,
                ready,
                chat_models,
            }
        }
    }
}
