//! Tears down a model deployment's runtime workload and removes its durable
//! records once absence is confirmed.
//!
//! Teardown stops and removes the workload and confirms its absence before
//! the current-state record and the desired declaration are deleted. A
//! failed stop, remove, or absence confirmation records a failed lifecycle
//! state with a retryable failure message instead of deleting state, so a
//! later unload can retry.

use dotcore_domain::diagnostic::diagnostic_chain;
use dotcore_proto::dotcore::{
    model::v1::{
        CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentFailure,
        ModelDeploymentStatus,
    },
    site::v1::Site,
};
use dotcore_runtime::WorkloadObservation;
use dotcore_site::view::fencing::{LifecycleOperationAttempt, LifecycleOperationToken};
use tracing::{Instrument, Span, error, field, info_span};

use super::{
    InferenceControllerInner, ModelLifecycleError,
    ownership::validate_teardown_observation,
    state::{current_error, observation_identity, unloading_state},
};
use crate::{
    dispatch::model::{RuntimeTargetMap, TargetObservation},
    model::state::{CurrentStateError, CurrentStateRepository},
};

/// Typed failures that are retained as model teardown status data.
#[derive(Debug, thiserror::Error)]
pub(super) enum TeardownError {
    #[error("Runtime teardown observation is inconclusive")]
    InconclusiveObservation,

    #[error("Runtime teardown inspection is inconclusive")]
    InconclusiveInspection,

    #[error("Runtime teardown lifecycle fence rejected the operation: {source}")]
    Fence {
        #[source]
        source: crate::dispatch::model::RuntimeTargetError,
    },

    #[error("Runtime teardown stop failed: {source}")]
    Stop {
        #[source]
        source: crate::dispatch::model::RuntimeTargetError,
    },

    #[error("Runtime teardown inspect failed: {source}")]
    Inspect {
        #[source]
        source: crate::dispatch::model::RuntimeTargetError,
    },

    #[error("Runtime teardown inspection returned invalid ownership: {source}")]
    InvalidInspection {
        #[source]
        source: ModelLifecycleError,
    },

    #[error("Runtime teardown removal failed: {source}")]
    Remove {
        #[source]
        source: crate::dispatch::model::RuntimeTargetError,
    },

    #[error("Runtime teardown absence confirmation failed: {source}")]
    AbsenceConfirmation {
        #[source]
        source: crate::dispatch::model::RuntimeTargetError,
    },

    #[error("Runtime teardown absence confirmation returned invalid ownership: {source}")]
    InvalidAbsenceConfirmation {
        #[source]
        source: ModelLifecycleError,
    },
}

impl TeardownError {
    fn is_stale_completion(&self) -> bool {
        let source = match self {
            Self::Fence { source }
            | Self::Stop { source }
            | Self::Inspect { source }
            | Self::Remove { source }
            | Self::AbsenceConfirmation { source } => Some(source),
            Self::InconclusiveObservation
            | Self::InconclusiveInspection
            | Self::InvalidInspection { .. }
            | Self::InvalidAbsenceConfirmation { .. } => None,
        };
        matches!(
            source,
            Some(crate::dispatch::model::RuntimeTargetError::StaleCompletion)
        )
    }
}
impl<D, S, R> InferenceControllerInner<D, S, R>
where
    D: super::ModelRealizationInput,
    S: CurrentStateRepository,
    R: super::ModelRealizationResults,
{
    pub(super) async fn reconcile_absent(
        &self,
        site: &Site,
        desired: &ModelDeploymentDeclaration,
        observation: Option<TargetObservation>,
    ) -> Result<dotcore_domain::timing::ReconcileOutcome, ModelLifecycleError> {
        let span = info_span!(
            "model_teardown",
            operation = "model_teardown",
            deployment_id = %desired.model_deployment_id,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        // Without an observed workload the teardown finalizes in this pass, so no Unloading
        // record is written; repeated passes over a removed workload leave state untouched.
        let unloading = if observation.is_some() {
            ensure_unloading_state(&self.state, desired)
        } else {
            Ok(())
        };
        let outcome = match unloading {
            Err(error) => TeardownOutcome::StateError(error),
            Ok(()) => {
                let result = teardown_workload(&self.runtimes, site, desired, observation)
                    .instrument(span.clone())
                    .await;
                match result {
                    Ok(completion) => {
                        let _operation = self.operation.lock().await;
                        let commit = self.commit_operation(&completion.operation, || {
                            if completion.absent {
                                self.finalize_absent(desired, completion.realization_generation)
                                    .map(|()| TeardownOutcome::Settled)
                            } else {
                                Ok(TeardownOutcome::Pending)
                            }
                        });
                        match commit {
                            Ok(Ok(outcome)) => outcome,
                            Ok(Err(error)) | Err(error) => TeardownOutcome::StateError(error),
                        }
                    }
                    Err(message) if message.is_stale_completion() => {
                        TeardownOutcome::StateError(ModelLifecycleError::Runtime(
                            crate::dispatch::model::RuntimeTargetError::StaleCompletion,
                        ))
                    }
                    Err(message) => match self.authorize_deployment_action(desired) {
                        Err(error) => TeardownOutcome::StateError(error),
                        Ok(_) => match record_teardown_failure(&self.state, desired, &message) {
                            Ok(()) => TeardownOutcome::RuntimeFailure(message),
                            Err(error) => TeardownOutcome::StateError(error),
                        },
                    },
                }
            }
        };
        record_teardown_result(&span, &outcome);
        outcome.into_result()
    }
}

enum TeardownOutcome {
    Settled,

    Pending,

    RuntimeFailure(TeardownError),

    StateError(ModelLifecycleError),
}

impl TeardownOutcome {
    fn into_result(self) -> Result<dotcore_domain::timing::ReconcileOutcome, ModelLifecycleError> {
        match self {
            Self::Settled => Ok(dotcore_domain::timing::ReconcileOutcome::Settled),
            Self::Pending | Self::RuntimeFailure(_) => {
                Ok(dotcore_domain::timing::ReconcileOutcome::Pending)
            }
            Self::StateError(error) => Err(error),
        }
    }
}

fn record_teardown_result(span: &Span, outcome: &TeardownOutcome) {
    match outcome {
        TeardownOutcome::Settled => {
            span.record("outcome", "success");
        }
        TeardownOutcome::Pending => {
            span.record("outcome", "pending");
        }
        TeardownOutcome::RuntimeFailure(error) => {
            span.record("outcome", "failed");
            span.record("error_kind", "runtime_teardown");
            error!(
                parent: span,
                error_kind = "runtime_teardown",
                error = %diagnostic_chain(error),
                "Runtime teardown failed"
            );
        }
        TeardownOutcome::StateError(error) => {
            span.record("outcome", "failed");
            span.record("error_kind", "state");
            error!(
                parent: span,
                error_kind = "state",
                error = %diagnostic_chain(error),
                "teardown state update failed"
            );
        }
    }
}

#[derive(Debug)]
pub(super) struct TeardownWorkloadResult {
    absent: bool,
    realization_generation: Option<u64>,
    operation: LifecycleOperationToken,
}

pub(super) async fn teardown_workload(
    runtimes: &RuntimeTargetMap,
    site: &Site,
    desired: &ModelDeploymentDeclaration,
    observation: Option<TargetObservation>,
) -> Result<TeardownWorkloadResult, TeardownError> {
    let realization_generation = observation
        .as_ref()
        .and_then(|target| observation_identity(&target.observation))
        .map(|workload| workload.correlation.reconcile_generation);
    let reconcile_generation = realization_generation.unwrap_or(desired.lifecycle_generation);
    let operation = runtimes
        .capture_operation(
            desired,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation,
            },
        )
        .map_err(|source| TeardownError::Fence { source })?;
    let Some(observation) = observation else {
        return Ok(TeardownWorkloadResult {
            absent: true,
            realization_generation,
            operation,
        });
    };
    let workload = observation_identity(&observation.observation)
        .ok_or(TeardownError::InconclusiveObservation)?;
    if matches!(observation.observation, WorkloadObservation::Absent(_)) {
        return Ok(TeardownWorkloadResult {
            absent: true,
            realization_generation,
            operation,
        });
    }
    let inspected = stop_and_inspect_workload(runtimes, &operation, &observation, workload).await?;
    validate_teardown_observation(
        site,
        runtimes,
        desired,
        &observation.execution_target_id,
        workload,
        &inspected,
    )
    .map_err(|source| TeardownError::InvalidInspection { source })?;
    let absent = match inspected {
        WorkloadObservation::Absent(_) => true,
        WorkloadObservation::Stopped {
            workload: inspected,
            ..
        } => {
            remove_and_confirm_absence(
                runtimes,
                site,
                desired,
                TeardownRemoval {
                    operation: &operation,
                    target: &observation.execution_target_id,
                    expected: workload,
                    workload: &inspected.workload_id,
                },
            )
            .await?
        }
        WorkloadObservation::Starting(_)
        | WorkloadObservation::Running(_)
        | WorkloadObservation::Stopping(_)
        | WorkloadObservation::Unavailable(_) => false,
        WorkloadObservation::Unknown { .. } => return Err(TeardownError::InconclusiveInspection),
    };
    runtimes
        .commit_operation(&operation, || ())
        .map_err(|source| TeardownError::Fence { source })?;
    Ok(TeardownWorkloadResult {
        absent,
        realization_generation,
        operation,
    })
}

async fn stop_and_inspect_workload(
    runtimes: &RuntimeTargetMap,
    operation: &LifecycleOperationToken,
    observation: &TargetObservation,
    workload: &dotcore_runtime::ObservedWorkload,
) -> Result<WorkloadObservation, TeardownError> {
    match &observation.observation {
        WorkloadObservation::Stopped { .. } => {
            runtimes
                .inspect_workload(&observation.execution_target_id, &workload.workload_id)
                .await
        }
        WorkloadObservation::Starting(_)
        | WorkloadObservation::Running(_)
        | WorkloadObservation::Stopping(_)
        | WorkloadObservation::Unavailable(_) => {
            runtimes
                .stop_workload_admitted(
                    operation,
                    &observation.execution_target_id,
                    &workload.workload_id,
                )
                .await
                .map_err(|source| TeardownError::Stop { source })?;
            runtimes
                .inspect_workload(&observation.execution_target_id, &workload.workload_id)
                .await
        }
        WorkloadObservation::Absent(_) | WorkloadObservation::Unknown { .. } => unreachable!(),
    }
    .map_err(|source| TeardownError::Inspect { source })
}

async fn remove_and_confirm_absence(
    runtimes: &RuntimeTargetMap,
    site: &Site,
    desired: &ModelDeploymentDeclaration,
    removal: TeardownRemoval<'_>,
) -> Result<bool, TeardownError> {
    runtimes
        .remove_workload_admitted(removal.operation, removal.target, removal.workload)
        .await
        .map_err(|source| TeardownError::Remove { source })?;
    runtimes
        .commit_operation(removal.operation, || ())
        .map_err(|source| TeardownError::Fence { source })?;
    let confirmation = runtimes
        .inspect_workload(removal.target, removal.workload)
        .await
        .map_err(|source| TeardownError::AbsenceConfirmation { source })?;
    validate_teardown_observation(
        site,
        runtimes,
        desired,
        removal.target,
        removal.expected,
        &confirmation,
    )
    .map_err(|source| TeardownError::InvalidAbsenceConfirmation { source })?;
    Ok(matches!(confirmation, WorkloadObservation::Absent(_)))
}

struct TeardownRemoval<'a> {
    operation: &'a LifecycleOperationToken,
    target: &'a str,
    expected: &'a dotcore_runtime::ObservedWorkload,
    workload: &'a dotcore_runtime::WorkloadId,
}

fn ensure_unloading_state<S: CurrentStateRepository>(
    state: &S,
    desired: &ModelDeploymentDeclaration,
) -> Result<(), ModelLifecycleError> {
    let snapshot = state.snapshot().map_err(current_error)?;
    let existing = snapshot
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id);
    if super::state::reconciled_generation(existing.as_ref(), desired)
        && existing
            .as_ref()
            .and_then(|record| record.state.as_ref())
            .is_some_and(|state| state.status == ModelDeploymentStatus::Unloading as i32)
    {
        return Ok(());
    }
    state
        .upsert(
            CurrentModelDeployment {
                model_deployment_id: desired.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(unloading_state(
                    existing.as_ref().and_then(|record| record.state.as_ref()),
                )),
            },
            existing.map(|record| record.record_revision),
        )
        .map_err(current_error)?;
    Ok(())
}

pub(super) fn record_teardown_failure<S: CurrentStateRepository>(
    state: &S,
    desired: &ModelDeploymentDeclaration,
    _error: &TeardownError,
) -> Result<(), ModelLifecycleError> {
    let snapshot = state.snapshot().map_err(current_error)?;
    let existing = snapshot
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == desired.model_deployment_id);
    let mut next = unloading_state(existing.as_ref().and_then(|record| record.state.as_ref()));
    next.status = ModelDeploymentStatus::Failed as i32;
    next.status_message = "teardown failed".to_owned();
    next.failure = Some(ModelDeploymentFailure {
        code: "runtime_teardown_failed".to_owned(),
        message: "Runtime teardown failed. Retry the unload or inspect runtime state.".to_owned(),
        diagnostics: Vec::new(),
    });
    state
        .upsert(
            CurrentModelDeployment {
                model_deployment_id: desired.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(next),
            },
            existing.map(|record| record.record_revision),
        )
        .map_err(current_error)?;
    Ok(())
}

impl<D, S, R> InferenceControllerInner<D, S, R>
where
    D: super::ModelRealizationInput,
    S: CurrentStateRepository,
    R: super::ModelRealizationResults,
{
    /// Removes local operational state and returns exact cleanup evidence to the manager owner.
    pub(super) fn finalize_absent(
        &self,
        desired: &ModelDeploymentDeclaration,
        realization_generation: Option<u64>,
    ) -> Result<(), ModelLifecycleError> {
        let preparation = self.preparations.entry(&desired.model_deployment_id);
        let incarnation = preparation.incarnation();
        let snapshot = self.state.snapshot().map_err(current_error)?;
        if !self.results.cleanup_removes_current_state()
            && let Some(record) = snapshot
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == desired.model_deployment_id)
        {
            match self
                .state
                .remove(&record.model_deployment_id, record.record_revision)
            {
                Ok(_) | Err(CurrentStateError::NotFound | CurrentStateError::RevisionConflict) => {}
                Err(error) => return Err(ModelLifecycleError::from_current_state(error)),
            }
        }
        self.results
            .confirm_cleanup(desired, realization_generation)?;
        self.engine_integrations
            .retire_inactive_deployment(&desired.model_deployment_id);
        self.preparations
            .retire(&desired.model_deployment_id, incarnation);
        if let Some(attribution) = &self.accelerator_attribution {
            attribution.forget(&desired.model_deployment_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::clock::SystemClock;
    use dotcore_domain::timing::{ReconcileOutcome, Timing};
    use std::{
        error::Error as _,
        future::ready,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use dotcore_artifact::{ArtifactManager, PreparationStep};
    use dotcore_domain::{SiteNodeId, host_process::HostProcessIdentity};
    use dotcore_proto::dotcore::{
        deployment::v1::accelerator_memory_observation,
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentState,
            ModelDeploymentStatus,
        },
        site::v1::{
            DockerRuntimeAdapter, ExecutionTarget, ModelRuntimeBinding, Site, execution_target,
            model_runtime_binding,
        },
    };
    use dotcore_runtime::{
        ObservedWorkload, RuntimeBackend, RuntimeBackendId, RuntimeFuture, RuntimeLogStreamKind,
        WorkloadId, WorkloadLaunch, WorkloadObservation,
    };
    use dotcore_site::{
        node::hardware_inventory::HardwareInventoryPublisher,
        store::{FileNodeState, InMemoryCurrentState},
        view::fencing::{
            AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
            RuntimeActionAuthority,
        },
    };
    use tokio::sync::{Mutex as AsyncMutex, Notify};

    use super::{record_teardown_failure, teardown_workload};
    use crate::{
        deployment_logs::{
            PreparationLogs, PreparationOwner, WorkloadChange, WorkloadLifecycle, model_steps,
        },
        dispatch::model::{RuntimeTargetMapBuilder, TargetObservation},
        model::{
            controller::{
                InferenceControllerInner, ModelRealizationInput,
                accelerator_memory::AcceleratorAttribution,
                launch::workload_launch,
                test_support::{
                    TestCurrent, TestDesired, TestFailure, assert_result, current_deployment,
                    test_desired_deployment, test_runtime_error, test_runtime_target, test_site,
                },
            },
            state::CurrentStateRepository,
        },
    };

    struct TeardownRuntime {
        calls: Arc<Mutex<Vec<&'static str>>>,
        inspection: WorkloadObservation,
        absence_confirmation: WorkloadObservation,
        failure: Option<TeardownAction>,
        advance_after_stop: Option<Duration>,
    }

    #[derive(Clone, Copy)]
    enum TeardownAction {
        Stop,

        Inspect,

        Remove,
    }

    impl RuntimeBackend for TeardownRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("runtime/teardown-test".to_owned())
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            Box::pin(ready(Ok(Vec::new())))
        }

        fn create_workload<'a>(
            &'a self,
            _workload: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn start_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(ready(Err(test_runtime_error())))
        }

        fn inspect_workload<'a>(
            &'a self,
            _workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            let mut calls = self.calls.lock().expect("calls");
            if matches!(self.failure, Some(TeardownAction::Inspect)) {
                calls.push("inspect");
                return Box::pin(ready(Err(test_runtime_error())));
            }
            let result = if calls.contains(&"remove") {
                self.absence_confirmation.clone()
            } else {
                self.inspection.clone()
            };
            calls.push("inspect");
            Box::pin(ready(Ok(result)))
        }

        fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            self.calls.lock().expect("calls").push("stop");
            if matches!(self.failure, Some(TeardownAction::Stop)) {
                Box::pin(ready(Err(test_runtime_error())))
            } else if let Some(duration) = self.advance_after_stop {
                Box::pin(async move {
                    tokio::time::advance(duration).await;
                    Ok(())
                })
            } else {
                Box::pin(ready(Ok(())))
            }
        }

        fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            self.calls.lock().expect("calls").push("remove");
            if matches!(self.failure, Some(TeardownAction::Remove)) {
                Box::pin(ready(Err(test_runtime_error())))
            } else {
                Box::pin(ready(Ok(())))
            }
        }
    }

    #[tokio::test]
    async fn teardown_stops_inspects_removes_and_confirms_absence_without_recreation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workload = teardown_workload_identity();
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target-one",
                TeardownRuntime {
                    calls: calls.clone(),
                    inspection: WorkloadObservation::stopped(workload.clone()),
                    absence_confirmation: WorkloadObservation::Absent(workload.clone()),
                    failure: None,
                    advance_after_stop: None,
                },
            )
            .expect("Runtime adapter");

        assert!(
            teardown_workload(
                &builder.build().expect("Runtime map"),
                &test_site(),
                &teardown_desired(),
                Some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: WorkloadObservation::Running(workload),
                }),
            )
            .await
            .expect("teardown")
            .absent
        );
        assert_eq!(
            calls.lock().expect("calls").as_slice(),
            ["stop", "inspect", "remove", "inspect"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_lease_allows_inflight_stop_to_finish_but_blocks_followup_remove() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workload = teardown_workload_identity();
        let desired = teardown_desired();
        let authority = leased_action_authority(&desired, Duration::from_secs(5));
        let mut builder = RuntimeTargetMapBuilder::new().with_action_authority(authority);
        builder
            .insert(
                "target-one",
                TeardownRuntime {
                    calls: calls.clone(),
                    inspection: WorkloadObservation::stopped(workload.clone()),
                    absence_confirmation: WorkloadObservation::Absent(workload.clone()),
                    failure: None,
                    advance_after_stop: Some(Duration::from_secs(6)),
                },
            )
            .expect("Runtime adapter");

        let error = teardown_workload(
            &builder.build().expect("Runtime map"),
            &test_site(),
            &desired,
            Some(TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(workload),
            }),
        )
        .await
        .expect_err("expired follow-up removal");

        assert!(matches!(
            error,
            super::TeardownError::Remove {
                source: crate::dispatch::model::RuntimeTargetError::ActionRejected
            }
        ));
        assert_eq!(calls.lock().expect("calls").as_slice(), ["stop", "inspect"]);
    }

    #[tokio::test]
    async fn teardown_action_failures_retain_actionable_failure_state() {
        for action in [
            TeardownAction::Stop,
            TeardownAction::Inspect,
            TeardownAction::Remove,
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let workload = teardown_workload_identity();
            let mut builder = RuntimeTargetMapBuilder::new();
            builder
                .insert(
                    "target-one",
                    TeardownRuntime {
                        calls,
                        inspection: WorkloadObservation::stopped(workload.clone()),
                        absence_confirmation: WorkloadObservation::Absent(workload.clone()),
                        failure: Some(action),
                        advance_after_stop: None,
                    },
                )
                .expect("Runtime adapter");
            let desired = teardown_desired();
            let result = teardown_workload(
                &builder.build().expect("Runtime map"),
                &test_site(),
                &desired,
                Some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: if matches!(action, TeardownAction::Remove) {
                        WorkloadObservation::stopped(workload)
                    } else {
                        WorkloadObservation::Running(workload)
                    },
                }),
            )
            .await;
            let current = InMemoryCurrentState::new("site-one").expect("current state");
            record_teardown_failure(&current, &desired, &result.expect_err("action failure"))
                .expect("failure state");
            let state = current
                .snapshot()
                .expect("current snapshot")
                .model_deployments
                .pop()
                .expect("retained state")
                .state
                .expect("failure state");
            assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
            assert_eq!(
                state.failure.expect("failure").code,
                "runtime_teardown_failed"
            );
        }
    }

    #[tokio::test]
    async fn teardown_errors_retain_the_runtime_target_source() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workload = teardown_workload_identity();
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target-one",
                TeardownRuntime {
                    calls,
                    inspection: WorkloadObservation::Running(workload.clone()),
                    absence_confirmation: WorkloadObservation::Absent(workload),
                    failure: Some(TeardownAction::Stop),
                    advance_after_stop: None,
                },
            )
            .expect("Runtime adapter");

        let error = teardown_workload(
            &builder.build().expect("Runtime map"),
            &test_site(),
            &teardown_desired(),
            Some(TargetObservation {
                execution_target_id: "target-one".to_owned(),
                observation: WorkloadObservation::Running(teardown_workload_identity()),
            }),
        )
        .await
        .expect_err("Runtime stop failure");

        assert_eq!(
            error.to_string(),
            "Runtime teardown stop failed: execution target \"target-one\" runtime adapter failure: Invalid runtime launch: test operation is unsupported"
        );
        assert_eq!(
            error.source().expect("Runtime target source").to_string(),
            "execution target \"target-one\" runtime adapter failure: Invalid runtime launch: test operation is unsupported"
        );
    }

    #[tokio::test]
    async fn teardown_removes_the_workload_without_deriving_the_current_launch() {
        let older_launch = {
            let mut workload = teardown_workload_identity();
            workload.correlation.reconcile_generation += 1;
            (teardown_desired(), workload)
        };
        let unpinned_nix_engine = {
            let mut desired = teardown_desired();
            crate::test_support::nix::retain_development_source_engine(
                desired.snapshot.as_mut().expect("snapshot"),
                "",
            );
            (desired, teardown_workload_identity())
        };
        let unpinned_image_engine = {
            let mut desired = teardown_desired();
            crate::test_support::retain_tag_only_engine(
                desired.snapshot.as_mut().expect("snapshot"),
                "",
            );
            (desired, teardown_workload_identity())
        };
        for (desired, workload) in [older_launch, unpinned_nix_engine, unpinned_image_engine] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut builder = RuntimeTargetMapBuilder::new();
            builder
                .insert(
                    "target-one",
                    TeardownRuntime {
                        calls: calls.clone(),
                        inspection: WorkloadObservation::stopped(workload.clone()),
                        absence_confirmation: WorkloadObservation::Absent(workload.clone()),
                        failure: None,
                        advance_after_stop: None,
                    },
                )
                .expect("Runtime adapter");

            let result = teardown_workload(
                &builder.build().expect("Runtime map"),
                &test_site(),
                &desired,
                Some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: WorkloadObservation::Running(workload.clone()),
                }),
            )
            .await
            .expect("teardown");

            assert!(result.absent);
            assert_eq!(
                result.realization_generation,
                Some(workload.correlation.reconcile_generation)
            );
            assert_eq!(
                calls.lock().expect("calls").as_slice(),
                ["stop", "inspect", "remove", "inspect"]
            );
        }
    }

    #[tokio::test]
    async fn teardown_never_removes_a_workload_it_did_not_observe_for_the_deployment() {
        let other_owner = {
            let mut workload = teardown_workload_identity();
            workload.correlation.owner_id = "other-deployment".to_owned();
            workload
        };
        let other_workload = {
            let mut workload = teardown_workload_identity();
            workload.workload_id = WorkloadId("other-workload".to_owned());
            workload
        };
        let other_generation = {
            let mut workload = teardown_workload_identity();
            workload.correlation.reconcile_generation += 1;
            workload
        };
        for inspected in [other_owner, other_workload, other_generation] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut builder = RuntimeTargetMapBuilder::new();
            builder
                .insert(
                    "target-one",
                    TeardownRuntime {
                        calls: calls.clone(),
                        inspection: WorkloadObservation::stopped(inspected.clone()),
                        absence_confirmation: WorkloadObservation::Absent(inspected),
                        failure: None,
                        advance_after_stop: None,
                    },
                )
                .expect("Runtime adapter");

            let error = teardown_workload(
                &builder.build().expect("Runtime map"),
                &test_site(),
                &teardown_desired(),
                Some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: WorkloadObservation::Running(teardown_workload_identity()),
                }),
            )
            .await
            .expect_err("mismatched inspection");

            assert!(matches!(
                error,
                super::TeardownError::InvalidInspection { .. }
            ));
            assert_eq!(calls.lock().expect("calls").as_slice(), ["stop", "inspect"]);
        }
    }

    #[tokio::test]
    async fn repeated_node_teardown_of_removed_workload_keeps_existing_evidence() {
        let mut site = test_site();
        let desired = teardown_desired();
        site.model_deployments.push(desired.clone());
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let current = FileNodeState::open(
            directory.path().join("node"),
            site.site_id.clone(),
            "node-one",
        )
        .expect("node current state");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workload = teardown_workload_identity();
        let realizer = node_teardown_realizer(
            &site,
            directory.path(),
            &current,
            &calls,
            &WorkloadLifecycle::new(),
        );
        assert!(matches!(
            realizer
                .inner
                .reconcile_absent(
                    &site,
                    &desired,
                    Some(TargetObservation {
                        execution_target_id: "target-one".to_owned(),
                        observation: WorkloadObservation::Running(workload.clone()),
                    }),
                )
                .await,
            Ok(ReconcileOutcome::Settled)
        ));
        let evidence = current
            .node_model_cleanup_evidence()
            .expect("first cleanup evidence");
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].realization_generation,
            Some(workload.correlation.reconcile_generation)
        );

        let state_path = directory.path().join("node/state.pb");
        let state_before = std::fs::read(&state_path).expect("state after first pass");

        let repeated = realizer.inner.reconcile().await;

        assert!(repeated.is_ok(), "repeated teardown pass: {repeated:?}");
        assert_eq!(
            current
                .node_model_cleanup_evidence()
                .expect("retained cleanup evidence"),
            evidence
        );
        assert_eq!(
            std::fs::read(&state_path).expect("state after repeated pass"),
            state_before
        );
        assert!(
            current
                .snapshot()
                .expect("node current state")
                .model_deployments
                .is_empty()
        );
        assert_eq!(
            calls.lock().expect("calls").as_slice(),
            ["stop", "inspect", "remove", "inspect"]
        );
    }

    #[tokio::test]
    async fn confirmed_node_teardown_removes_only_its_preparation_logs_and_ends_its_follows() {
        let mut site = test_site();
        let desired = teardown_desired();
        site.model_deployments.push(desired.clone());
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let current = FileNodeState::open(
            directory.path().join("node"),
            site.site_id.clone(),
            "node-one",
        )
        .expect("node current state");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workload = teardown_workload_identity();
        let lifecycle = WorkloadLifecycle::new();
        let realizer =
            node_teardown_realizer(&site, directory.path(), &current, &calls, &lifecycle);
        let mut changes = lifecycle.subscribe(&desired.model_deployment_id);
        let preparation = PreparationLogs::on_system_clock(directory.path());
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for step in model_steps() {
            preparation
                .writer(&desired.model_deployment_id, step)
                .append(time, RuntimeLogStreamKind::Stdout, b"prepared");
        }
        preparation
            .writer("other-deployment", PreparationStep::EngineBuild)
            .append(time, RuntimeLogStreamKind::Stdout, b"kept");
        let started = |deployment_id: &str| {
            PreparationLogs::on_system_clock(directory.path())
                .select(
                    PreparationOwner::Model,
                    deployment_id,
                    &[],
                    &model_steps(),
                    1,
                )
                .steps
        };
        assert_eq!(started(&desired.model_deployment_id).len(), 3);

        realizer
            .inner
            .reconcile_absent(
                &site,
                &desired,
                Some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: WorkloadObservation::Running(workload),
                }),
            )
            .await
            .expect("node teardown");
        // The change is published before the teardown returns, so it is already waiting.
        let change = futures_util::FutureExt::now_or_never(changes.next()).flatten();

        assert_eq!(
            current
                .node_model_cleanup_evidence()
                .expect("cleanup evidence")
                .len(),
            1
        );
        assert_eq!(
            change.map(Result::ok),
            Some(Some(WorkloadChange::Removed)),
            "the confirmed cleanup ends the deployment's log follows"
        );
        // The confirmed deployment, and any later one reusing its identity, has no steps left;
        // the other deployment keeps its own.
        assert!(started(&desired.model_deployment_id).is_empty());
        assert!(
            !directory
                .path()
                .join("preparation-logs/model")
                .join(&desired.model_deployment_id)
                .exists()
        );
        assert_eq!(started("other-deployment"), [PreparationStep::EngineBuild]);
    }

    /// A node realizer over `site` whose only target tears down the stopped teardown workload.
    fn node_teardown_realizer(
        site: &Site,
        directory: &std::path::Path,
        current: &FileNodeState,
        calls: &Arc<Mutex<Vec<&'static str>>>,
        workload_lifecycle: &WorkloadLifecycle,
    ) -> crate::model::controller::ModelRealizer<
        crate::model::controller::test_support::ProjectedTestInput,
        FileNodeState,
        crate::model::controller::DurableModelRealizationResults,
    > {
        let projection =
            NodeViewProjection::for_site_node(site, "node-one").expect("node projection");
        let workload = teardown_workload_identity();
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target-one",
                TeardownRuntime {
                    calls: calls.clone(),
                    inspection: WorkloadObservation::stopped(workload.clone()),
                    absence_confirmation: WorkloadObservation::Absent(workload),
                    failure: None,
                    advance_after_stop: None,
                },
            )
            .expect("Runtime adapter");
        crate::model::controller::ModelRealizer::new_node_realizer(
            crate::model::controller::NodeModelRealizerDependencies {
                input: crate::model::controller::test_support::ProjectedTestInput::new(
                    site.site_id.clone(),
                    projection,
                ),
                composition: crate::model::controller::ModelControllerDependencies {
                    artifacts: crate::model::controller::ArtifactDependencies::default(),
                    target_capabilities:
                        crate::model::controller::test_support::test_target_capability_source(),
                    engine_integrations:
                        crate::model::controller::test_support::test_engine_integrations(),
                    accelerator_attribution: None,
                },
                serving_targets:
                    crate::model::controller::serving_targets::ServingTargetsPublisher::new().0,
                shutdown: tokio_util::sync::CancellationToken::new(),
                preparation_logs: PreparationLogs::on_system_clock(directory),
                workload_lifecycle: workload_lifecycle.clone(),
                timing: Timing::test().model,
                clock: std::sync::Arc::new(SystemClock),
            },
            current.clone(),
            builder.build().expect("Runtime map"),
            Vec::new(),
            directory.join("cache"),
        )
        .expect("node realizer")
    }

    #[test]
    fn teardown_completion_removes_current_state_before_desired_intent() {
        for failure in [TestFailure::Failed, TestFailure::OutcomeUncertain] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            desired
                .state
                .lock()
                .expect("desired state")
                .model_deployments
                .push(teardown_desired());
            let current = TestCurrent::new(calls.clone());
            current
                .upsert(current_deployment("deployment"), None)
                .expect("current state");
            current.fail_next(failure);
            let inner = test_inner(desired, current, calls.clone());
            assert_result(failure, inner.finalize_absent(&teardown_desired(), None));
            assert!(calls.lock().expect("calls").ends_with(&["current remove"]));
        }

        for failure in [TestFailure::Failed, TestFailure::OutcomeUncertain] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            desired
                .state
                .lock()
                .expect("desired state")
                .model_deployments
                .push(teardown_desired());
            desired.fail_next(failure);
            let current = TestCurrent::new(calls.clone());
            current
                .upsert(current_deployment("deployment"), None)
                .expect("current state");
            let inner = test_inner(desired.clone(), current.clone(), calls.clone());
            assert_result(failure, inner.finalize_absent(&teardown_desired(), None));
            assert!(
                current
                    .snapshot()
                    .expect("current state")
                    .model_deployments
                    .is_empty()
            );
            assert_eq!(desired.model_deployments().expect("desired state").len(), 1);
            assert!(
                calls
                    .lock()
                    .expect("calls")
                    .ends_with(&["current remove", "desired remove"])
            );
        }
    }

    #[test]
    fn confirmed_teardown_retires_coordinator_before_textual_id_reuse() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(teardown_desired());
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current state");
        let inner = test_inner(desired.clone(), current.clone(), calls);
        let old_incarnation = inner.preparations.entry("deployment").incarnation();

        inner
            .finalize_absent(&teardown_desired(), None)
            .expect("confirmed teardown finalization");
        let new_incarnation = inner.preparations.entry("deployment").incarnation();

        assert!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .is_empty()
        );
        assert!(
            desired
                .model_deployments()
                .expect("desired state")
                .is_empty()
        );
        assert_ne!(new_incarnation, old_incarnation);
    }

    #[test]
    fn confirmed_teardown_forgets_old_process_before_deployment_id_reuse() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(teardown_desired());
        let current = TestCurrent::new(calls.clone());
        current
            .upsert(current_deployment("deployment"), None)
            .expect("current state");
        let mut inner = test_inner(desired, current, calls);
        let (_, inventory) = HardwareInventoryPublisher::new();
        inner.accelerator_attribution = Some(AcceleratorAttribution::new(Arc::new(inventory)));
        let attribution = inner.accelerator_attribution.as_ref().expect("attribution");
        let mut old = teardown_workload_identity();
        old.host_process = Some(HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test("node-a"),
            process_id: 41,
            process_start_token: 700,
        });
        attribution.observe("deployment", None, &WorkloadObservation::Running(old));
        let target = ExecutionTarget {
            execution_target_id: "target-one".to_owned(),
            site_node_id: "node-b".to_owned(),
            accelerator_device_ids: vec!["GPU-b".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                        DockerRuntimeAdapter::default(),
                    )),
                },
            )),
            ..Default::default()
        };
        let state = ModelDeploymentState {
            status: ModelDeploymentStatus::Ready as i32,
            ..Default::default()
        };
        let projection_code = || match attribution
            .projection(
                &teardown_desired(),
                &state,
                Some(&target),
                &attribution.collection(),
            )
            .result
        {
            Some(accelerator_memory_observation::Result::Unavailable(reason)) => reason.code,
            other => panic!("expected unavailable attribution, got {other:?}"),
        };
        assert_eq!(projection_code(), "WORKLOAD_NODE_MISMATCH");

        inner
            .finalize_absent(&teardown_desired(), None)
            .expect("confirmed teardown");
        let mut replacement = teardown_workload_identity();
        replacement.workload_id = WorkloadId("replacement".to_owned());
        replacement.host_process = Some(HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test("node-b"),
            process_id: 42,
            process_start_token: 900,
        });
        attribution.observe(
            "deployment",
            None,
            &WorkloadObservation::Running(replacement),
        );
        assert_eq!(projection_code(), "NODE_OBSERVATION_PENDING");
    }

    fn leased_action_authority(
        desired: &ModelDeploymentDeclaration,
        lease_duration: Duration,
    ) -> RuntimeActionAuthority {
        let mut site = test_site();
        site.model_deployments.push(desired.clone());
        let node_id = site.execution_targets[0].site_node_id.clone();
        let projection =
            NodeViewProjection::for_site_node(&site, &node_id).expect("leased node view");
        let node = NodeIncarnation::parse("b".repeat(32)).expect("node incarnation");
        let manager = ManagerSession::parse("a".repeat(32)).expect("manager session");
        let authority = AcceptedNodeViewAuthority::new(
            site.site_id.clone(),
            node_id.clone(),
            node,
            manager,
            lease_duration,
        )
        .expect("accepted view authority");
        authority
            .install_for_test(site.site_id, node_id, 1, &projection)
            .expect("accepted leased view");
        RuntimeActionAuthority::Leased(authority)
    }

    fn teardown_desired() -> ModelDeploymentDeclaration {
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Teardown)
    }

    fn teardown_workload_identity() -> ObservedWorkload {
        let desired = teardown_desired();
        let resolved =
            super::super::launch::resolved_launch(&desired).expect("resolved test model");
        let artifacts = dotcore_artifact::ModelArtifactCache::new("/test-cache".into())
            .expect("test cache")
            .expected_paths(&resolved.artifacts)
            .expect("test artifact paths");
        let launch = workload_launch(
            &desired,
            test_site()
                .execution_targets
                .first()
                .expect("test execution target"),
            &artifacts,
        )
        .expect("test launch");
        ObservedWorkload {
            workload_id: WorkloadId("workload".to_owned()),
            correlation: launch.correlation,
            backend_id: RuntimeBackendId("runtime/teardown-test".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        }
    }

    fn test_inner(
        desired: TestDesired,
        current: TestCurrent,
        _calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> InferenceControllerInner<TestDesired, TestCurrent, TestDesired> {
        let (serving_targets_publisher, serving_targets_source) =
            crate::model::controller::serving_targets::ServingTargetsPublisher::new();
        InferenceControllerInner {
            workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
            preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                std::path::Path::new("/test-preparation-logs"),
            ),
            results: desired.clone(),
            target_capabilities:
                crate::model::controller::test_support::test_target_capability_source(),

            desired,
            state: current,
            runtimes: test_runtime_target(),
            engine_integrations: crate::model::controller::test_support::test_engine_integrations(),
            operation: Arc::new(AsyncMutex::new(())),
            reconciliation: AsyncMutex::new(()),
            preparations:
                crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                    .expect("preparation coordinator"),
            observer_started: Mutex::new(false),
            wakeup: Arc::new(Notify::new()),
            artifacts: ArtifactManager::from_parts(
                dotcore_artifact::ModelArtifactCache::new("/test-cache".into())
                    .expect("test cache"),
                Some(crate::model::controller::test_support::ready_oci_preparer()),
            ),
            serving_targets_publisher,
            serving_targets_source,
            accelerator_attribution: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            timing: Timing::test().model,
            clock: std::sync::Arc::new(SystemClock),
        }
    }
}
