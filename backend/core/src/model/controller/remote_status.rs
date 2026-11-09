//! Accepts complete node model observations into manager-owned current state.
//!
//! The manager validates every observation against the exact current node projection, then
//! answers each reported cleanup entry individually: a teardown is removed only for evidence from
//! the node its placement names, and acknowledged only once the removal is durable. A rejected
//! entry never prevents acceptance of the report or of other entries.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, SystemTime},
};

use dotcore_domain::{
    clock::Clock,
    deployment::{DeploymentOwner, artifact::LaunchIdentityResolution},
    timing::ModelTiming,
};
use dotcore_proto::dotcore::{
    model::{
        private::v1::{
            ModelBindingReadiness, ModelCleanupDisposition, ModelControllerStatusSection,
            ModelDeploymentStatus as PrivateModelDeploymentStatus, ModelLifecycleObservation,
            ModelPreparationProgress,
        },
        v1::{
            CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentProgress, ModelDeploymentProgressUnit, ModelDeploymentState,
            ModelDeploymentStatus, ModelDeploymentStep, ModelDeploymentSteps,
        },
    },
    site::private::v1::{ControllerStatusReceipt, controller_status_receipt},
};
use dotcore_site::{
    node::target_inventory::timestamp_to_system_time,
    repository::{DeploymentTeardownTransaction, SiteRepository, SiteRepositoryError},
    store::FileCurrentState,
    view::{
        cleanup_evidence::{classify_cleanup_evidence, finalize_cleanup_decisions},
        contract::cleanup_receipt,
        desired::{ControllerStatusError, ModelStatusOwner},
        fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    },
};
use tracing::{debug, info, warn};

use super::{
    CurrentStateRepository, InferenceController, ModelLifecycleError, state::status_message,
};
use crate::model::{
    engine_pin::{validate_local_declaration, with_node_pins},
    service::timestamp_from,
    state::{CurrentStateError, retain_start_steps},
};

/// Stored node byte progress moves in steps of this fraction of a known total (5%).
const PROGRESS_STEPS: u64 = 20;

/// Stored node byte progress of an unknown total moves in steps of this many bytes.
const UNKNOWN_TOTAL_PROGRESS_STEP_BYTES: u64 = 64 * 1024 * 1024;

impl ModelStatusOwner
    for InferenceController<crate::model::repository::ModelDeclarationRepository, FileCurrentState>
{
    fn accept_remote_status(
        &self,
        node_id: &str,
        projection: &NodeViewProjection,
        model: &ModelControllerStatusSection,
        manager_session: &ManagerSession,
        node_incarnation: &NodeIncarnation,
        status_sequence: u64,
    ) -> Result<Vec<ControllerStatusReceipt>, ControllerStatusError> {
        accept_node_status(
            self.inner.desired.site_repository(),
            &self.inner.state,
            &NodeStatusReport {
                node_id,
                projection,
                model,
                manager_session,
                node_incarnation,
                status_sequence,
            },
            self.inner.timing,
            self.inner.clock.as_ref(),
        )
        .map_err(ControllerStatusError::from)
    }
}

/// One authenticated node model status report for the node view it names.
pub(crate) struct NodeStatusReport<'a> {
    pub node_id: &'a str,
    pub projection: &'a NodeViewProjection,
    pub model: &'a ModelControllerStatusSection,
    pub manager_session: &'a ManagerSession,
    pub node_incarnation: &'a NodeIncarnation,
    pub status_sequence: u64,
}

/// Accepts one structurally valid report and returns one disposition per cleanup evidence entry.
///
/// Evidence classification, conditional teardown removal, and current-record writes all run
/// under the site guard (site, then current state), so no site mutation or manager pruning can
/// interleave. Records are written only for deployments still declared, at the reported
/// generation, after this report's removals, so a report for an earlier view never replaces the
/// record of a newer accepted generation. Each acknowledged deployment's lifecycle record is
/// pruned once its removal is durable. A record that differs from the retained one only in
/// progress waits for the node progress interval of `timing`; `clock`'s wall time ages and stamps
/// the records.
pub(crate) fn accept_node_status(
    site: &SiteRepository,
    state: &FileCurrentState,
    report: &NodeStatusReport<'_>,
    timing: ModelTiming,
    clock: &dyn Clock,
) -> Result<Vec<ControllerStatusReceipt>, RemoteStatusError> {
    validate_complete_status(report.projection, report.model)
        .map_err(RemoteStatusError::rejected)?;
    let records = report
        .model
        .deployments
        .iter()
        .map(current_record)
        .collect::<Result<Vec<_>, _>>()
        .map_err(RemoteStatusError::rejected)?;

    site.with_deployment_teardowns(|teardowns| {
        let decisions = report
            .model
            .cleanup_evidence
            .iter()
            .map(|evidence| {
                classify_cleanup_evidence(
                    teardowns,
                    state,
                    DeploymentOwner::Model,
                    report.node_id,
                    &evidence.model_deployment_id,
                    evidence.lifecycle_generation,
                )
                .map(|decision| (evidence.model_deployment_id.as_str(), decision))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(state_error)?;
        let finalized = finalize_cleanup_decisions(
            teardowns,
            DeploymentOwner::Model,
            report.node_id,
            &decisions,
        );
        if let Some(finalized) = &finalized {
            state
                .prune_lifecycle_records(DeploymentOwner::Model, finalized)
                .map_err(state_error)?;
        }
        let durable = finalized.is_some();
        let declared = report
            .model
            .deployments
            .iter()
            .zip(records)
            .filter(|(status, _)| {
                teardowns
                    .model_deployment(&status.model_deployment_id)
                    .is_some_and(|declaration| {
                        declaration.lifecycle_generation == status.lifecycle_generation
                    })
            })
            .map(|(_, record)| record)
            .collect();
        commit_changed_records(state, declared, timing, clock)?;
        let pinned = report
            .model
            .deployments
            .iter()
            .filter_map(|status| {
                let declared = teardowns.model_deployment(&status.model_deployment_id)?;
                let placed_on_reporter = declared
                    .placement
                    .as_ref()
                    .is_some_and(|placement| placement.site_node_id == report.node_id);
                (declared.lifecycle_generation == status.lifecycle_generation && placed_on_reporter)
                    .then(|| with_reported_pins(declared, status, report.node_id))
                    .flatten()
            })
            .collect();
        record_pins(teardowns, pinned, report.node_id);
        Ok(report
            .model
            .cleanup_evidence
            .iter()
            .zip(decisions)
            .map(|(evidence, (_, decision))| {
                let disposition = if decision.acknowledged(durable) {
                    ModelCleanupDisposition::Acknowledged
                } else {
                    ModelCleanupDisposition::Rejected
                };
                ControllerStatusReceipt {
                    payload: Some(controller_status_receipt::Payload::Model(cleanup_receipt(
                        report.manager_session,
                        report.node_incarnation,
                        report.status_sequence,
                        evidence,
                        disposition,
                    ))),
                }
            })
            .collect())
    })
    .map_err(site_error)?
}

/// Returns the declaration with the engine launch identity its node resolved and reported, or
/// `None` when the report adds nothing. A pin is recorded once: a report that contradicts the
/// recorded pin, or that the declaration does not admit, is ignored.
fn with_reported_pins(
    declared: &ModelDeploymentDeclaration,
    status: &PrivateModelDeploymentStatus,
    node_id: &str,
) -> Option<ModelDeploymentDeclaration> {
    if declared.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
        return None;
    }
    let reported = dotcore_domain::deployment::launch_pin::record_reported_pins(
        &declared.node_resolved_launch_identities,
        &status.resolved_launch_identities,
    );
    for contradiction in &reported.contradicted {
        warn!(event_kind = "manager_model_launch_identity", outcome = "contradicted", deployment_id = %declared.model_deployment_id, node_id, selection_key = %contradiction.reported.selection_key, recorded = %contradiction.recorded.launch_identity, reported = %contradiction.reported.launch_identity, "Node reported an engine launch identity that contradicts the recorded pin; the recorded pin is kept");
    }
    let declaration = with_node_pins(declared, reported.recorded?);
    match validate_local_declaration(&declaration, LaunchIdentityResolution::NodeResolved) {
        Ok(()) => Some(declaration),
        Err(error) => {
            // The node reports it again with every status, so this stays at debug level.
            debug!(event_kind = "manager_model_launch_identity", outcome = "rejected", deployment_id = %declared.model_deployment_id, node_id, error = %error, "Node reported launch identities the declaration does not admit");
            None
        }
    }
}

/// Records node-resolved engine launch identities in their declarations and makes them durable
/// on their own, so a failed write never delays cleanup acknowledgement; the node keeps
/// reporting them until they are projected back.
fn record_pins(
    teardowns: &mut DeploymentTeardownTransaction<'_>,
    pinned: Vec<ModelDeploymentDeclaration>,
    node_id: &str,
) {
    if pinned.is_empty() {
        return;
    }
    let deployments = pinned
        .iter()
        .map(|declaration| declaration.model_deployment_id.clone())
        .collect::<Vec<_>>();
    for declaration in pinned {
        teardowns.replace_model_deployment(declaration);
    }
    match teardowns.make_durable() {
        Ok(()) => {
            info!(event_kind = "manager_model_launch_identity", outcome = "recorded", node_id, deployments = ?deployments, "Manager recorded node-resolved engine launch identities")
        }
        Err(error) => {
            warn!(event_kind = "manager_model_launch_identity", outcome = "retryable", node_id, error = %error, "Manager could not record node-resolved engine launch identities")
        }
    }
}

/// Typed manager outcome for a node controller status report that was not accepted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteStatusError {
    /// The report contradicts current manager intent; resending the same report cannot succeed.
    #[error("node controller status was rejected: {0}")]
    Rejected(#[source] Box<ModelLifecycleError>),

    /// Manager storage could not accept the report; the same report may be retried.
    #[error("node controller status storage failed: {0}")]
    Storage(#[source] Box<ModelLifecycleError>),

    /// Manager persistence may or may not have completed; the same report may be retried.
    #[error("node controller status persistence outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] Box<ModelLifecycleError>),
}

impl From<RemoteStatusError> for ControllerStatusError {
    fn from(error: RemoteStatusError) -> Self {
        match error {
            RemoteStatusError::Rejected(error) => Self::Rejected(anyhow::Error::new(*error)),
            RemoteStatusError::Storage(error) | RemoteStatusError::OutcomeUncertain(error) => {
                Self::Retryable(anyhow::Error::new(*error))
            }
        }
    }
}

impl RemoteStatusError {
    fn rejected(error: ModelLifecycleError) -> Self {
        Self::Rejected(Box::new(error))
    }

    fn storage(error: ModelLifecycleError) -> Self {
        match error.category() {
            super::ModelLifecycleErrorCategory::OutcomeUncertain => {
                Self::OutcomeUncertain(Box::new(error))
            }
            _ => Self::Storage(Box::new(error)),
        }
    }
}

/// Requires one status for every assignment of the reported view at its exact generation and
/// placement, except a teardown covered by evidence at that same generation.
fn validate_complete_status(
    projection: &NodeViewProjection,
    model: &ModelControllerStatusSection,
) -> Result<(), ModelLifecycleError> {
    let assignments = projection
        .model_deployments()
        .iter()
        .map(|deployment| (deployment.model_deployment_id.as_str(), deployment))
        .collect::<BTreeMap<_, _>>();
    let mut observed = BTreeSet::new();

    for status in &model.deployments {
        let Some(declaration) = assignments.get(status.model_deployment_id.as_str()) else {
            return Err(ModelLifecycleError::configuration(
                "node model status does not match the current assignment set",
            ));
        };
        if status.lifecycle_generation != declaration.lifecycle_generation
            || status.placement.as_ref() != declaration.placement.as_ref()
            || !observed.insert(status.model_deployment_id.as_str())
        {
            return Err(ModelLifecycleError::configuration(
                "node model status identity does not match current intent",
            ));
        }
    }
    for evidence in &model.cleanup_evidence {
        let covers_assignment = assignments
            .get(evidence.model_deployment_id.as_str())
            .is_some_and(|declaration| {
                declaration.lifecycle_generation == evidence.lifecycle_generation
            });
        if covers_assignment && !observed.insert(evidence.model_deployment_id.as_str()) {
            return Err(ModelLifecycleError::configuration(
                "node reported both status and cleanup evidence for one assignment",
            ));
        }
    }
    if observed.len() != assignments.len() {
        return Err(ModelLifecycleError::configuration(
            "node model status is incomplete for the current assignment set",
        ));
    }
    Ok(())
}

/// Builds the owner record a node status stands for: the node's lifecycle, status message,
/// failure summary, start steps, and Downloading progress as the node recorded them, so a
/// node-hosted record reads like a record the realizer writes in this process.
///
/// Two observations are the owner's own: a Reconciling node has no observation for this
/// generation and reads as Loading without start steps, and a Ready record without a Ready
/// binding reads as Unavailable. Start steps the manager cannot read are dropped while the rest
/// of the status is kept.
pub(super) fn current_record(
    status: &PrivateModelDeploymentStatus,
) -> Result<CurrentModelDeployment, ModelLifecycleError> {
    let lifecycle = ModelLifecycleObservation::try_from(status.lifecycle).map_err(|_| {
        ModelLifecycleError::configuration("node model lifecycle observation is invalid")
    })?;
    let readiness = ModelBindingReadiness::try_from(status.binding_readiness).map_err(|_| {
        ModelLifecycleError::configuration("node model binding readiness is invalid")
    })?;
    let public_status = match lifecycle {
        ModelLifecycleObservation::Queued => ModelDeploymentStatus::Queued,
        ModelLifecycleObservation::Preparing => ModelDeploymentStatus::Preparing,
        ModelLifecycleObservation::Downloading => ModelDeploymentStatus::Downloading,
        ModelLifecycleObservation::Loading | ModelLifecycleObservation::Reconciling => {
            ModelDeploymentStatus::Loading
        }
        ModelLifecycleObservation::WarmingUp => ModelDeploymentStatus::WarmingUp,
        ModelLifecycleObservation::Ready if readiness == ModelBindingReadiness::Ready => {
            ModelDeploymentStatus::Ready
        }
        ModelLifecycleObservation::Ready | ModelLifecycleObservation::Unavailable => {
            ModelDeploymentStatus::Unavailable
        }
        ModelLifecycleObservation::Pausing => ModelDeploymentStatus::Pausing,
        ModelLifecycleObservation::Paused => ModelDeploymentStatus::Paused,
        ModelLifecycleObservation::Stopping => ModelDeploymentStatus::Unloading,
        ModelLifecycleObservation::Failed => ModelDeploymentStatus::Failed,
        ModelLifecycleObservation::Unspecified => {
            return Err(ModelLifecycleError::configuration(
                "node model lifecycle observation is unspecified",
            ));
        }
    };
    // A Reconciling node has no observation for this generation, so the record carries no
    // reconciled generation and never retains an earlier generation's readiness.
    let reconciling = lifecycle == ModelLifecycleObservation::Reconciling;
    let unbound_ready = lifecycle == ModelLifecycleObservation::Ready
        && public_status != ModelDeploymentStatus::Ready;
    let message = if reconciling {
        "Reconciling".to_owned()
    } else if unbound_ready || status.status_message.is_empty() {
        status_message(public_status).to_owned()
    } else {
        status.status_message.clone()
    };
    let progress = status
        .preparation_progress
        .as_ref()
        .filter(|_| public_status == ModelDeploymentStatus::Downloading)
        .map(stored_progress);
    let mut state = ModelDeploymentState {
        status: public_status as i32,
        status_message: message,
        progress,
        failure: status
            .failure
            .clone()
            .filter(|_| public_status == ModelDeploymentStatus::Failed),
        observed_placements: status.observed_placements.clone(),
        steps: status
            .steps
            .clone()
            .filter(|steps| !reconciling && valid_steps(steps)),
        ..Default::default()
    };
    retain_start_steps(&mut state);
    Ok(CurrentModelDeployment {
        model_deployment_id: status.model_deployment_id.clone(),
        reconciled_lifecycle_generation: (!reconciling).then_some(status.lifecycle_generation),
        record_revision: 0,
        state: Some(state),
        recovery_barrier: None,
    })
}

/// Reports whether node start steps name only known steps and a current step within the list.
fn valid_steps(steps: &ModelDeploymentSteps) -> bool {
    (steps.current as usize) < steps.steps.len()
        && steps.steps.iter().all(|step| {
            ModelDeploymentStep::try_from(*step)
                .is_ok_and(|step| step != ModelDeploymentStep::Unspecified)
        })
}

/// Quantizes reported byte progress to coarse steps and derives the percent from the stored
/// bytes, so the stored and displayed values agree and small changes compare equal. Completion
/// of a known total is kept exact.
fn stored_progress(progress: &ModelPreparationProgress) -> ModelDeploymentProgress {
    let total = progress.total_bytes.filter(|total| *total != 0);
    let step = total.map_or(UNKNOWN_TOTAL_PROGRESS_STEP_BYTES, |total| {
        total.div_ceil(PROGRESS_STEPS)
    });
    let completed = if Some(progress.completed_bytes) == total {
        progress.completed_bytes
    } else {
        progress.completed_bytes / step * step
    };
    ModelDeploymentProgress {
        percent: total.map(|total| (u128::from(completed) * 100 / u128::from(total)) as u32),
        completed,
        total,
        unit: ModelDeploymentProgressUnit::Bytes as i32,
    }
}

/// Writes only records whose observation differs from the retained record, so an unchanged
/// report leaves `state.pb` untouched. A change of stored progress alone waits until the retained
/// record is as old as the node progress interval of `timing`. Written records keep the retained
/// `started_at` (else the current wall time of `clock`) and get that wall time as `updated_at`;
/// timestamps never take part in the comparison.
fn commit_changed_records(
    state: &impl CurrentStateRepository,
    records: Vec<CurrentModelDeployment>,
    timing: ModelTiming,
    clock: &dyn Clock,
) -> Result<(), RemoteStatusError> {
    let now = clock.wall_now();
    let snapshot = state.snapshot().map_err(state_error)?;
    let retained = snapshot
        .model_deployments
        .iter()
        .map(|record| (record.model_deployment_id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let stamp = timestamp_from(now);
    let changed = records
        .into_iter()
        .filter_map(|mut record| {
            let current = retained.get(record.model_deployment_id.as_str());
            if current.is_some_and(|current| {
                comparable(current, true) == comparable(&record, true)
                    || (only_progress_differs(current, &record)
                        && record_age(current, now)
                            .is_some_and(|age| age < timing.node_progress_interval))
            }) {
                return None;
            }
            let started_at = current
                .and_then(|current| current.state.as_ref())
                .and_then(|state| state.started_at)
                .unwrap_or(stamp);
            if let Some(state) = record.state.as_mut() {
                state.started_at = Some(started_at);
                state.updated_at = Some(stamp);
            }
            Some(record)
        })
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    state
        .prepare(changed, snapshot.storage_revision)
        .map_err(state_error)?;
    Ok(())
}

/// Reports whether `record` differs from the retained `current` only in progress.
fn only_progress_differs(
    current: &CurrentModelDeployment,
    record: &CurrentModelDeployment,
) -> bool {
    let has_progress = |record: &CurrentModelDeployment| {
        record
            .state
            .as_ref()
            .is_some_and(|state| state.progress.is_some())
    };
    has_progress(current)
        && has_progress(record)
        && comparable(current, false) == comparable(record, false)
}

/// Age of a stored record at wall time `now`, when it carries an update time not after `now`.
fn record_age(record: &CurrentModelDeployment, now: SystemTime) -> Option<Duration> {
    record
        .state
        .as_ref()
        .and_then(|state| state.updated_at.as_ref())
        .and_then(timestamp_to_system_time)
        .and_then(|updated_at| now.duration_since(updated_at).ok())
}

/// The observed content of a record: no timestamps or record revision, and progress only when
/// `with_progress` is set.
fn comparable(record: &CurrentModelDeployment, with_progress: bool) -> CurrentModelDeployment {
    let mut record = CurrentModelDeployment {
        record_revision: 0,
        ..record.clone()
    };
    if let Some(state) = record.state.as_mut() {
        state.started_at = None;
        state.updated_at = None;
        if !with_progress {
            state.progress = None;
        }
    }
    record
}

fn site_error(error: SiteRepositoryError) -> RemoteStatusError {
    RemoteStatusError::storage(ModelLifecycleError::from_model_repository(
        crate::model::repository::ModelDeclarationRepositoryError::Site(error),
    ))
}

fn state_error(error: impl Into<CurrentStateError>) -> RemoteStatusError {
    RemoteStatusError::storage(ModelLifecycleError::from_current_state(error))
}

#[cfg(test)]
mod tests {
    use dotcore_domain::{
        clock::{SystemClock, VirtualClock},
        timing::Timing,
    };
    use dotcore_proto::dotcore::{
        deployment::v1::ExactPlacement,
        model::{
            private::v1::{
                ModelBindingReadiness, ModelCleanupEvidence, ModelControllerStatusSection,
                ModelDeploymentStatus as PrivateModelDeploymentStatus, ModelLifecycleObservation,
                ModelPreparationProgress,
            },
            v1::{
                ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentFailure,
                ModelDeploymentLifecycleTransition, ObservedPlacement,
            },
        },
        site::v1::ExecutionTarget,
    };
    use dotcore_proto::google::protobuf::Timestamp;
    use dotcore_site::{
        repository::{SiteWriteFault, SiteWriteFaults},
        store::{FileCurrentState, cleanup_evidence_digest},
        view::desired::ControllerStatusError,
    };

    use super::*;
    use crate::model::controller::test_support::expected_start_steps;

    #[test]
    fn storage_and_uncertain_outcomes_are_retryable_and_rejections_are_not() {
        let storage = RemoteStatusError::Storage(Box::new(ModelLifecycleError::CurrentState(
            crate::model::state::CurrentStateError::Storage(anyhow::anyhow!(
                "injected state.pb write failure"
            )),
        )));
        let uncertain = RemoteStatusError::OutcomeUncertain(Box::new(
            ModelLifecycleError::OutcomeUncertain(anyhow::anyhow!("injected uncertain rename")),
        ));
        let rejected = RemoteStatusError::Rejected(Box::new(ModelLifecycleError::InvalidArgument));

        for retryable in [storage, uncertain] {
            assert!(matches!(
                ControllerStatusError::from(retryable),
                ControllerStatusError::Retryable(_)
            ));
        }
        assert!(matches!(
            ControllerStatusError::from(rejected),
            ControllerStatusError::Rejected(_)
        ));
    }

    #[test]
    fn complete_status_requires_every_exact_assignment_and_generation_atomically() {
        let projection = NodeViewProjection::new(
            vec![ExecutionTarget {
                execution_target_id: "target-one".to_owned(),
                site_node_id: "worker-one".to_owned(),
                ..Default::default()
            }],
            vec![declaration("model-one", 7), declaration("model-two", 4)],
        )
        .expect("projection");
        let mut section = ModelControllerStatusSection {
            deployments: vec![status("model-one", 7)],
            cleanup_evidence: Vec::new(),
        };

        assert!(validate_complete_status(&projection, &section).is_err());
        section.deployments.push(status("model-two", 3));
        assert!(validate_complete_status(&projection, &section).is_err());
        section.deployments[1].lifecycle_generation = 4;
        assert!(validate_complete_status(&projection, &section).is_ok());
    }

    #[test]
    fn ready_status_preserves_node_observed_node_and_devices_without_diagnostics() {
        let status = status("model-one", 7);
        let record = current_record(&status).expect("current record");
        let state = record.state.expect("current state");

        assert_eq!(state.status, ModelDeploymentStatus::Ready as i32);
        assert_eq!(state.status_message, "Ready");
        assert_eq!(
            state.observed_placements,
            [ObservedPlacement {
                managed_node_id: "worker-one".to_owned(),
                accelerator_device_ids: vec!["nvidia.com/gpu=3".to_owned()],
            }]
        );
        assert_eq!(state.failure, None);
    }

    #[test]
    fn unchanged_remote_status_does_not_rewrite_state() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = crate::test_support::canonical_tempdir().expect("manager state directory");
        let state_path = directory.path().join("state.pb");
        let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
            .expect("manager current state");
        let ready = current_record(&status("model-one", 7)).expect("ready record");
        commit(&current, vec![ready.clone()]).expect("first status");
        let written_inode = std::fs::metadata(&state_path).expect("state.pb").ino();
        let written_revision = current.snapshot().expect("state").storage_revision;

        commit(&current, vec![ready]).expect("unchanged status");
        let unchanged_inode = std::fs::metadata(&state_path).expect("state.pb").ino();
        let unchanged_revision = current.snapshot().expect("state").storage_revision;
        let mut paused = status("model-one", 7);
        paused.lifecycle = ModelLifecycleObservation::Paused as i32;
        paused.binding_readiness = ModelBindingReadiness::NotReady as i32;
        commit(
            &current,
            vec![current_record(&paused).expect("paused record")],
        )
        .expect("changed status");
        let changed = current.snapshot().expect("state");

        assert_eq!(unchanged_inode, written_inode);
        assert_eq!(unchanged_revision, written_revision);
        assert_eq!(changed.storage_revision, written_revision + 1);
        assert_eq!(
            changed.model_deployments[0]
                .state
                .as_ref()
                .expect("current state")
                .status,
            ModelDeploymentStatus::Paused as i32
        );
        assert_eq!(changed.model_deployments[0].record_revision, 2);
    }

    #[test]
    fn node_records_get_started_and_updated_times_that_only_change_with_the_observation() {
        let directory = tempfile::tempdir().expect("manager state directory");
        let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
            .expect("manager current state");
        let ready = current_record(&status("model-one", 7)).expect("ready record");

        commit(&current, vec![ready.clone()]).expect("first status");
        let first = retained_state(&current);
        let first_started = first.started_at.expect("started_at is set on first report");
        assert_eq!(first.updated_at, Some(first_started));

        commit(&current, vec![ready]).expect("unchanged status");
        let unchanged = retained_state(&current);
        assert_eq!(unchanged.started_at, first.started_at);
        assert_eq!(unchanged.updated_at, first.updated_at);
        assert_eq!(current.snapshot().expect("state").storage_revision, 1);

        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut paused = status("model-one", 7);
        paused.lifecycle = ModelLifecycleObservation::Paused as i32;
        paused.binding_readiness = ModelBindingReadiness::NotReady as i32;
        commit(
            &current,
            vec![current_record(&paused).expect("paused record")],
        )
        .expect("changed status");
        let changed = retained_state(&current);
        assert_eq!(changed.status, ModelDeploymentStatus::Paused as i32);
        assert_eq!(changed.started_at, first.started_at);
        let (before, after) = (first.updated_at.unwrap(), changed.updated_at.unwrap());
        assert!((after.seconds, after.nanos) > (before.seconds, before.nanos));
    }

    #[test]
    fn node_record_keeps_started_at_of_the_retained_intent_record() {
        let directory = tempfile::tempdir().expect("manager state directory");
        let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
            .expect("manager current state");
        let intent_time = Timestamp {
            seconds: 1_000,
            nanos: 5,
        };
        current
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: "model-one".to_owned(),
                    state: Some(ModelDeploymentState {
                        status: ModelDeploymentStatus::Pausing as i32,
                        status_message: "Pausing".to_owned(),
                        started_at: Some(intent_time),
                        updated_at: Some(intent_time),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .expect("intent record");

        commit(
            &current,
            vec![current_record(&status("model-one", 7)).expect("ready record")],
        )
        .expect("node status");

        let state = retained_state(&current);
        assert_eq!(state.status, ModelDeploymentStatus::Ready as i32);
        assert_eq!(state.started_at, Some(intent_time));
        assert_ne!(state.updated_at, Some(intent_time));
    }

    #[test]
    fn reconciling_status_replaces_an_earlier_ready_observation() {
        let directory = crate::test_support::canonical_tempdir().expect("manager state directory");
        let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
            .expect("manager current state");
        commit(
            &current,
            vec![current_record(&status("model-one", 1)).expect("ready record")],
        )
        .expect("ready status");
        let ready = current.snapshot().expect("state").model_deployments[0].clone();
        let mut reconciling = status("model-one", 2);
        reconciling.lifecycle = ModelLifecycleObservation::Reconciling as i32;
        reconciling.steps = Some(ModelDeploymentSteps {
            steps: expected_start_steps(),
            current: 1,
        });
        reconciling.binding_readiness = ModelBindingReadiness::Absent as i32;
        reconciling.realization_generation = None;
        reconciling.state_revision = 0;
        reconciling.observed_placements.clear();

        commit(
            &current,
            vec![current_record(&reconciling).expect("reconciling record")],
        )
        .expect("reconciling status");
        let record = current.snapshot().expect("state").model_deployments[0].clone();

        assert_eq!(ready.reconciled_lifecycle_generation, Some(1));
        assert_eq!(record.reconciled_lifecycle_generation, None);
        assert_eq!(record.record_revision, ready.record_revision + 1);
        let ready_started_at = ready.state.as_ref().and_then(|state| state.started_at);
        assert!(ready_started_at.is_some());
        let state = record.state.as_ref().expect("current state");
        assert_eq!(state.started_at, ready_started_at);
        assert_eq!(
            comparable(&record, true).state,
            Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Loading as i32,
                status_message: "Reconciling".to_owned(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn pausing_status_is_recorded_as_pausing_not_paused() {
        let mut pausing = status("model-one", 3);
        pausing.lifecycle = ModelLifecycleObservation::Pausing as i32;
        pausing.status_message = "Pausing".to_owned();
        pausing.binding_readiness = ModelBindingReadiness::NotReady as i32;
        pausing.observed_placements.clear();

        let record = current_record(&pausing).expect("pausing record");

        assert_eq!(record.reconciled_lifecycle_generation, Some(3));
        let state = record.state.expect("current state");
        assert_eq!(state.status, ModelDeploymentStatus::Pausing as i32);
        assert_eq!(state.status_message, "Pausing");
    }

    #[test]
    fn node_records_keep_the_recorded_lifecycle_and_message() {
        use ModelLifecycleObservation as Observed;
        let cases = [
            (
                Observed::Queued,
                "Queued",
                ModelDeploymentStatus::Queued,
                "Queued",
            ),
            (
                Observed::Preparing,
                "Preparing",
                ModelDeploymentStatus::Preparing,
                "Preparing",
            ),
            (
                Observed::Downloading,
                "Simulating model artifact download",
                ModelDeploymentStatus::Downloading,
                "Simulating model artifact download",
            ),
            (
                Observed::Loading,
                "Resuming",
                ModelDeploymentStatus::Loading,
                "Resuming",
            ),
            (
                Observed::WarmingUp,
                "Warming up",
                ModelDeploymentStatus::WarmingUp,
                "Warming up",
            ),
            (
                Observed::Unavailable,
                "Unavailable",
                ModelDeploymentStatus::Unavailable,
                "Unavailable",
            ),
            (
                Observed::Pausing,
                "Pausing",
                ModelDeploymentStatus::Pausing,
                "Pausing",
            ),
            (
                Observed::Paused,
                "Paused",
                ModelDeploymentStatus::Paused,
                "Paused",
            ),
            (
                Observed::Stopping,
                "Unloading",
                ModelDeploymentStatus::Unloading,
                "Unloading",
            ),
            (
                Observed::Reconciling,
                "",
                ModelDeploymentStatus::Loading,
                "Reconciling",
            ),
            (
                Observed::Ready,
                "",
                ModelDeploymentStatus::Unavailable,
                "Unavailable",
            ),
            (
                Observed::Loading,
                "",
                ModelDeploymentStatus::Loading,
                "Loading",
            ),
        ];
        for (lifecycle, recorded, public_status, message) in cases {
            let state = current_record(&preparation_status(lifecycle, recorded, None))
                .expect("record")
                .state
                .expect("state");

            assert_eq!(
                (state.status, state.status_message.as_str()),
                (public_status as i32, message),
                "{lifecycle:?} {recorded:?}"
            );
            assert_eq!(state.failure, None, "{lifecycle:?}");
        }

        let mut ready_without_binding = status("model-one", 3);
        ready_without_binding.binding_readiness = ModelBindingReadiness::NotReady as i32;
        let unavailable = current_record(&ready_without_binding)
            .expect("record")
            .state
            .expect("state");
        assert_eq!(
            (unavailable.status, unavailable.status_message.as_str()),
            (ModelDeploymentStatus::Unavailable as i32, "Unavailable")
        );
    }

    #[test]
    fn node_hosted_failure_projects_the_manager_local_code_message_and_diagnostics() {
        use super::super::state::{
            engine_image_acquisition_failed_state, engine_startup_failed_state, failed_state,
            loading_state, resume_recovery_required_state, startup_failure_diagnostics,
        };
        let realization_failure = failed_state(
            None,
            "Runtime adapter refused the workload",
            ModelDeploymentFailure {
                code: "runtime_realization_failed".to_owned(),
                message: "Runtime adapter refused the workload".to_owned(),
                diagnostics: vec![
                    "Runtime adapter refused the workload".to_owned(),
                    "model runtime operation failed: Runtime adapter refused the workload"
                        .to_owned(),
                ],
            },
        );
        let manager_local = [
            engine_startup_failed_state(
                &loading_state(),
                None,
                startup_failure_diagnostics(
                    Some(17),
                    None,
                    Some("FileNotFoundError: [REDACTED]\nengine exited".to_owned()),
                ),
            ),
            engine_image_acquisition_failed_state(None, Vec::new()),
            resume_recovery_required_state(),
            realization_failure,
        ];

        for local in manager_local {
            let node_hosted = node_hosted(&local);

            assert_eq!(
                (
                    node_hosted.status,
                    &node_hosted.status_message,
                    &node_hosted.failure
                ),
                (local.status, &local.status_message, &local.failure),
            );
        }
    }

    #[test]
    fn node_hosted_progress_steps_project_like_manager_local_records() {
        use super::super::state::{
            acquiring_engine_image_state, checking_model_artifacts_state, deployment_status_state,
            downloading_state, paused_state, pausing_state, realization_prepared_state,
            resuming_state, runtime_startup_progress_state, unavailable_state, unloading_state,
        };
        let progress = dotcore_artifact::ModelArtifactProgress {
            completed: 40,
            total: Some(100),
        };
        let manager_local = [
            downloading_state(None, progress.clone()),
            runtime_startup_progress_state(
                None,
                progress.clone(),
                "Simulating model artifact download",
            ),
            checking_model_artifacts_state(Some(&downloading_state(None, progress)))
                .expect("checking state"),
            acquiring_engine_image_state(None).expect("engine state"),
            realization_prepared_state(None),
            resuming_state(3),
            deployment_status_state(ModelDeploymentStatus::WarmingUp, None, None),
            unavailable_state(None),
            pausing_state(None),
            paused_state(None),
            unloading_state(None),
        ];

        for local in manager_local {
            let node_hosted = node_hosted(&local);

            assert_eq!(
                (
                    node_hosted.status,
                    &node_hosted.status_message,
                    &node_hosted.progress,
                    &node_hosted.failure,
                    &node_hosted.steps
                ),
                (
                    local.status,
                    &local.status_message,
                    &local.progress,
                    &local.failure,
                    &local.steps
                ),
                "{}",
                local.status_message
            );
        }
    }

    #[test]
    fn invalid_node_steps_are_dropped_while_the_status_is_kept() {
        let with_steps = |steps: Vec<i32>, current| {
            let mut status = preparation_status(
                ModelLifecycleObservation::Loading,
                "Acquiring engine image",
                None,
            );
            status.steps = Some(ModelDeploymentSteps { steps, current });
            current_record(&status)
                .expect("status accepted")
                .state
                .expect("state")
        };
        let mut unknown = expected_start_steps();
        unknown[1] = 99;
        let mut unspecified = expected_start_steps();
        unspecified[0] = ModelDeploymentStep::Unspecified as i32;

        assert_eq!(
            with_steps(expected_start_steps(), 4).steps,
            Some(ModelDeploymentSteps {
                steps: expected_start_steps(),
                current: 4,
            })
        );
        for (name, steps, current) in [
            ("current past the list", expected_start_steps(), 5),
            ("empty list", Vec::new(), 0),
            ("unknown step", unknown, 1),
            ("unspecified step", unspecified, 1),
        ] {
            let state = with_steps(steps, current);
            assert_eq!(state.steps, None, "{name}");
            assert_eq!(
                state.status,
                ModelDeploymentStatus::Loading as i32,
                "{name}"
            );
            assert_eq!(state.status_message, "Acquiring engine image", "{name}");
        }
    }

    #[test]
    fn stored_node_progress_moves_in_coarse_steps_that_match_its_percent() {
        let stored = |completed, total| {
            current_record(&preparation_status(
                ModelLifecycleObservation::Downloading,
                "Downloading model artifacts",
                Some((completed, total)),
            ))
            .expect("record")
            .state
            .expect("state")
            .progress
            .map(|progress| (progress.completed, progress.total, progress.percent))
        };
        const MIB: u64 = 1024 * 1024;

        assert_eq!(stored(437, Some(1000)), Some((400, Some(1000), Some(40))));
        assert_eq!(stored(999, Some(1000)), Some((950, Some(1000), Some(95))));
        assert_eq!(
            stored(1000, Some(1000)),
            Some((1000, Some(1000), Some(100)))
        );
        assert_eq!(stored(3, Some(7)), Some((3, Some(7), Some(42))));
        assert_eq!(stored(100 * MIB + 3, None), Some((64 * MIB, None, None)));
    }

    #[test]
    fn fine_grained_node_progress_writes_a_bounded_number_of_times() {
        let directory = tempfile::tempdir().expect("manager state directory");
        let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
            .expect("manager current state");
        let downloading = |completed| {
            current_record(&preparation_status(
                ModelLifecycleObservation::Downloading,
                "Downloading model artifacts",
                Some((completed, Some(1000))),
            ))
            .expect("record")
        };
        let revision = || current.snapshot().expect("state").storage_revision;
        // Frozen wall time keeps every report within the progress interval of the first.
        let clock = VirtualClock::starting_at(SystemTime::now());
        let commit =
            |records| commit_changed_records(&current, records, Timing::test().model, &clock);

        commit(vec![downloading(0)]).expect("first report");
        for completed in 1..=1000 {
            commit(vec![downloading(completed)]).expect("progress report");
        }
        assert_eq!(revision(), 1);
        let first = retained_state(&current);
        assert_eq!(first.progress.expect("progress").completed, 0);

        let written_long_ago = Timestamp {
            seconds: 1_000,
            nanos: 0,
        };
        let mut backdated = current.snapshot().expect("state").model_deployments[0].clone();
        let expected_revision = backdated.record_revision;
        if let Some(state) = backdated.state.as_mut() {
            state.updated_at = Some(written_long_ago);
        }
        current
            .upsert(backdated, Some(expected_revision))
            .expect("backdated record");
        commit(vec![downloading(437)]).expect("due progress");
        let stepped = retained_state(&current);
        assert_eq!(revision(), 3);
        assert_eq!(stepped.progress.expect("progress").completed, 400);
        assert_eq!(stepped.started_at, first.started_at);
        assert_ne!(stepped.updated_at, Some(written_long_ago));

        let checking = current_record(&preparation_status(
            ModelLifecycleObservation::Loading,
            "Checking model artifacts",
            None,
        ))
        .expect("record");
        commit(vec![checking]).expect("phase change");
        assert_eq!(revision(), 4);
        assert_eq!(
            retained_state(&current).status_message,
            "Checking model artifacts"
        );
    }

    #[test]
    fn owner_failures_are_classified_by_retryability() {
        use dotcore_site::persistence::SiteStorageError;

        assert!(matches!(
            site_error(SiteRepositoryError::Storage(SiteStorageError::Storage(
                anyhow::anyhow!("injected site.yaml write failure")
            ))),
            RemoteStatusError::Storage(_)
        ));
        assert!(matches!(
            site_error(SiteRepositoryError::Storage(
                SiteStorageError::OutcomeUncertain(anyhow::anyhow!(
                    "injected site.yaml rename uncertainty"
                ))
            )),
            RemoteStatusError::OutcomeUncertain(_)
        ));
        assert!(matches!(
            state_error(CurrentStateError::RevisionConflict),
            RemoteStatusError::Storage(_)
        ));
        assert!(matches!(
            state_error(CurrentStateError::Poisoned),
            RemoteStatusError::OutcomeUncertain(_)
        ));
    }

    #[test]
    fn teardown_evidence_from_its_placement_node_is_removed_durably_before_acknowledgement() {
        let live = site_declaration("model-live", "worker-one", Present, 1);
        let manager = ManagerSite::new(vec![
            live.clone(),
            site_declaration("model-gone", "worker-one", Teardown, 2),
        ]);

        let dispositions = manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 2)],
            )
            .expect("valid report");

        assert_eq!(dispositions, [("model-gone".to_owned(), 2, Acknowledged)]);
        manager.assert_durable(std::slice::from_ref(&live));
        assert_eq!(manager.records(), [("model-live".to_owned(), Some(1))]);
    }

    #[test]
    fn obsolete_evidence_is_acknowledged_without_effect() {
        let present = site_declaration("model-one", "worker-one", Present, 1);
        let manager = ManagerSite::new(vec![present.clone()]);
        manager.reserve_generations("model-one", 3);
        let before = manager.disk();
        let attempts = manager.faults.attempts();

        let dispositions = manager
            .report(
                "worker-one",
                vec![status("model-one", 1)],
                vec![evidence("model-one", 2)],
            )
            .expect("valid report");

        assert_eq!(dispositions, [("model-one".to_owned(), 2, Acknowledged)]);
        assert_eq!(manager.disk(), before);
        assert_eq!(manager.faults.attempts(), attempts);
        manager.assert_durable(&[present]);
        assert_eq!(manager.records(), [("model-one".to_owned(), Some(1))]);
    }

    #[test]
    fn future_and_non_teardown_evidence_is_rejected_without_effect() {
        let present = site_declaration("model-one", "worker-one", Present, 1);
        let current = site_declaration("model-two", "worker-one", Present, 2);
        let manager = ManagerSite::new(vec![present.clone(), current.clone()]);
        let before = manager.disk();

        let dispositions = manager
            .report(
                "worker-one",
                vec![status("model-one", 1)],
                vec![evidence("model-one", 5), evidence("model-two", 2)],
            )
            .expect("entry rejections never reject the report");

        assert_eq!(
            dispositions,
            [
                ("model-one".to_owned(), 5, Rejected),
                ("model-two".to_owned(), 2, Rejected),
            ]
        );
        assert_eq!(manager.disk(), before);
        manager.assert_durable(&[present, current]);
        assert_eq!(manager.records(), [("model-one".to_owned(), Some(1))]);
    }

    #[test]
    fn wrong_node_evidence_has_zero_effects_while_other_entries_proceed() {
        let other_node = site_declaration("model-other", "worker-two", Teardown, 1);
        let manager_local = site_declaration("model-local", "manager-node", Teardown, 1);
        let manager = ManagerSite::new(vec![
            other_node.clone(),
            manager_local.clone(),
            site_declaration("model-own", "worker-one", Teardown, 1),
        ]);

        let dispositions = manager
            .report(
                "worker-one",
                Vec::new(),
                vec![
                    evidence("model-other", 1),
                    evidence("model-local", 1),
                    evidence("model-own", 1),
                ],
            )
            .expect("valid report");

        assert_eq!(
            dispositions,
            [
                ("model-other".to_owned(), 1, Rejected),
                ("model-local".to_owned(), 1, Rejected),
                ("model-own".to_owned(), 1, Acknowledged),
            ]
        );
        manager.assert_durable(&[other_node, manager_local]);
        assert!(manager.records().is_empty());
    }

    #[test]
    fn absent_evidence_on_a_clean_site_is_acknowledged_without_a_write() {
        let manager = ManagerSite::new(Vec::new());
        let before = manager.disk();
        let attempts = manager.faults.attempts();

        let dispositions = manager
            .report("worker-one", Vec::new(), vec![evidence("model-gone", 4)])
            .expect("valid report");

        assert_eq!(dispositions, [("model-gone".to_owned(), 4, Acknowledged)]);
        assert_eq!(manager.disk(), before);
        assert_eq!(manager.faults.attempts(), attempts);
    }

    #[test]
    fn absent_evidence_on_a_dirty_site_is_acknowledged_only_after_a_successful_flush() {
        let live = site_declaration("model-live", "worker-one", Present, 1);
        let manager = ManagerSite::new(vec![live.clone()]);
        let before = manager.disk();
        manager.faults.push(SiteWriteFault::Storage);
        assert!(
            manager
                .site
                .mutate_model_deployment_metadata(|transaction| {
                    transaction.deployments[0].model_deployment_name = "Renamed".to_owned();
                    Ok::<_, std::convert::Infallible>(())
                })
                .is_err()
        );
        assert!(manager.site.is_dirty());

        manager.faults.push(SiteWriteFault::Storage);
        let blocked = manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 4)],
            )
            .expect("valid report");
        assert_eq!(blocked, [("model-gone".to_owned(), 4, Rejected)]);
        assert!(manager.site.is_dirty());
        assert_eq!(manager.disk(), before);

        let flushed = manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 4)],
            )
            .expect("valid report");
        let renamed = ModelDeploymentDeclaration {
            model_deployment_name: "Renamed".to_owned(),
            ..live
        };
        assert_eq!(flushed, [("model-gone".to_owned(), 4, Acknowledged)]);
        manager.assert_durable(&[renamed]);
    }

    #[test]
    fn failed_or_uncertain_removal_blocks_acknowledgement_until_a_later_write() {
        for fault in [SiteWriteFault::Storage, SiteWriteFault::Uncertain] {
            let teardown = site_declaration("model-gone", "worker-one", Teardown, 2);
            let manager = ManagerSite::new(vec![teardown.clone()]);
            manager.faults.push(fault);

            let blocked = manager
                .report("worker-one", Vec::new(), vec![evidence("model-gone", 2)])
                .expect("valid report");

            assert_eq!(
                blocked,
                [("model-gone".to_owned(), 2, Rejected)],
                "{fault:?}"
            );
            assert!(manager.site.is_dirty());
            assert!(manager.declared().is_empty());
            let on_disk = match fault {
                SiteWriteFault::Storage => vec![teardown],
                SiteWriteFault::Uncertain => Vec::new(),
            };
            assert_eq!(manager.reloaded(), on_disk, "{fault:?}");

            let acknowledged = manager
                .report("worker-one", Vec::new(), vec![evidence("model-gone", 2)])
                .expect("valid report");

            assert_eq!(acknowledged, [("model-gone".to_owned(), 2, Acknowledged)]);
            manager.assert_durable(&[]);
        }
    }

    #[test]
    fn acknowledged_teardown_prunes_its_lifecycle_record_only_once_removal_is_durable() {
        let live = site_declaration("model-live", "worker-one", Present, 1);
        let manager = ManagerSite::new(vec![
            live.clone(),
            site_declaration("model-gone", "worker-one", Teardown, 2),
        ]);
        manager.reserve_generations("model-live", 1);
        manager.reserve_generations("model-gone", 3);
        manager.faults.push(SiteWriteFault::Storage);

        let blocked = manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 2)],
            )
            .expect("valid report");

        assert_eq!(blocked, [("model-gone".to_owned(), 2, Rejected)]);
        assert_eq!(manager.high_water("model-gone"), Some(3));

        let acknowledged = manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 2)],
            )
            .expect("valid report");

        assert_eq!(acknowledged, [("model-gone".to_owned(), 2, Acknowledged)]);
        manager.assert_durable(std::slice::from_ref(&live));
        assert_eq!(manager.high_water("model-gone"), None);
        assert_eq!(manager.high_water("model-live"), Some(1));
        let reopened_path = manager.directory.path().join("site.yaml");
        drop(manager.state);
        let reopened = FileCurrentState::open(&reopened_path, "site-one").expect("restart");
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-gone"
                )
                .expect("pruned high-water"),
            None
        );
    }

    #[test]
    fn status_writes_no_record_for_a_deployment_the_site_no_longer_declares() {
        let live = site_declaration("model-live", "worker-one", Present, 1);
        let manager = ManagerSite::new(vec![
            live.clone(),
            site_declaration("model-gone", "worker-one", Teardown, 2),
        ]);
        let published = manager.projection("worker-one");
        manager
            .report(
                "worker-one",
                vec![status("model-live", 1)],
                vec![evidence("model-gone", 2)],
            )
            .expect("removing report");
        let mut stopping = status("model-gone", 2);
        stopping.lifecycle = ModelLifecycleObservation::Stopping as i32;
        stopping.binding_readiness = ModelBindingReadiness::Absent as i32;
        stopping.observed_placements.clear();

        manager
            .report_for(
                &published,
                "worker-one",
                vec![status("model-live", 1), stopping],
                Vec::new(),
            )
            .expect("report for the earlier published view");

        manager.assert_durable(&[live]);
        assert_eq!(manager.records(), [("model-live".to_owned(), Some(1))]);
    }

    #[test]
    fn node_resolved_engine_launch_identity_is_recorded_once_and_projected_back() {
        let manager = ManagerSite::new(vec![node_resolved_declaration("model-one", "worker-one")]);
        let mut pinned = status("model-one", 1);
        pinned.resolved_launch_identities = vec![engine_pin(FIRST_IMAGE)];

        manager
            .report("worker-one", vec![pinned], Vec::new())
            .expect("pinned report");
        let recorded = ModelDeploymentDeclaration {
            node_resolved_launch_identities: vec![engine_pin(FIRST_IMAGE)],
            ..node_resolved_declaration("model-one", "worker-one")
        };
        manager.assert_durable(std::slice::from_ref(&recorded));
        assert_eq!(
            manager.projection("worker-one").model_deployments(),
            std::slice::from_ref(&recorded)
        );

        let mut contradicting = status("model-one", 1);
        contradicting.resolved_launch_identities = vec![engine_pin(SECOND_IMAGE)];
        manager
            .report("worker-one", vec![contradicting], Vec::new())
            .expect("contradicting report");
        manager.assert_durable(&[recorded]);
    }

    #[test]
    fn development_source_engine_store_path_is_recorded_once_and_only_as_a_store_path() {
        let unpinned = development_source_declaration("model-one", "worker-one");
        let manager = ManagerSite::new(vec![unpinned.clone()]);
        let report = |launch_identity: &str| {
            let mut pinned = status("model-one", 1);
            pinned.resolved_launch_identities = vec![engine_pin(launch_identity)];
            manager
                .report("worker-one", vec![pinned], Vec::new())
                .expect("pinned report");
        };

        report(FIRST_IMAGE);
        manager.assert_durable(std::slice::from_ref(&unpinned));

        report(dotcore_domain::test_support::nix::STORE_PATH);
        let recorded = ModelDeploymentDeclaration {
            node_resolved_launch_identities: vec![engine_pin(
                dotcore_domain::test_support::nix::STORE_PATH,
            )],
            ..unpinned
        };
        manager.assert_durable(std::slice::from_ref(&recorded));
        assert_eq!(
            manager.projection("worker-one").model_deployments(),
            std::slice::from_ref(&recorded)
        );

        report("/nix/store/00000000000000000000000000000002-llama-cpp-b11418");
        manager.assert_durable(&[recorded]);
    }

    #[test]
    fn a_manager_node_placement_leaves_its_engine_launch_identity_for_its_node_to_pin() {
        let manager = ManagerSite::new(Vec::new());

        manager
            .site
            .mutate_model_deployments(|transaction| {
                transaction
                    .deployments
                    .push(node_resolved_declaration("model-one", "manager-node"));
                Ok::<_, std::convert::Infallible>(())
            })
            .expect("unpinned manager-node declaration");

        assert_eq!(
            manager.declared(),
            [node_resolved_declaration("model-one", "manager-node")]
        );
    }

    #[test]
    fn status_for_a_superseded_generation_keeps_the_record_of_the_declared_generation() {
        let manager = ManagerSite::new(vec![site_declaration(
            "model-one",
            "worker-one",
            Present,
            1,
        )]);
        let published = manager.projection("worker-one");
        manager
            .site
            .mutate_model_deployments(|transaction| {
                let deployment = &mut transaction.deployments[0];
                deployment.desired_state = ModelDeploymentDesiredState::Paused as i32;
                deployment.lifecycle_generation = 2;
                deployment.lifecycle_transition = ModelDeploymentLifecycleTransition::Pause as i32;
                Ok::<_, std::convert::Infallible>(())
            })
            .expect("accepted pause");
        let pausing = ModelDeploymentState {
            status: ModelDeploymentStatus::Pausing as i32,
            status_message: "Pausing".to_owned(),
            ..Default::default()
        };
        manager
            .state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: "model-one".to_owned(),
                    reconciled_lifecycle_generation: None,
                    record_revision: 0,
                    state: Some(pausing.clone()),
                    recovery_barrier: None,
                },
                None,
            )
            .expect("pause intent record");

        manager
            .report_for(
                &published,
                "worker-one",
                vec![status("model-one", 1)],
                Vec::new(),
            )
            .expect("report for the earlier published view");

        let records = manager
            .state
            .snapshot()
            .expect("manager state")
            .model_deployments;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reconciled_lifecycle_generation, None);
        assert_eq!(records[0].state, Some(pausing));
    }

    fn declaration(id: &str, generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: "target-one".to_owned(),
            }),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            lifecycle_generation: generation,
            ..Default::default()
        }
    }

    use ModelCleanupDisposition::{Acknowledged, Rejected};
    use ModelDeploymentDesiredState::{Present, Teardown};

    /// Manager site and current state over real files with injectable `site.yaml` write faults.
    struct ManagerSite {
        directory: tempfile::TempDir,
        site: SiteRepository,
        faults: SiteWriteFaults,
        state: FileCurrentState,
    }

    impl ManagerSite {
        fn new(deployments: Vec<ModelDeploymentDeclaration>) -> Self {
            let directory =
                crate::test_support::canonical_tempdir().expect("manager site directory");
            let site_path = directory.path().join("site.yaml");
            std::fs::write(&site_path, MANAGER_SITE).expect("manager site");
            let (site, faults) = SiteRepository::load_with_write_faults_for_test(&site_path)
                .expect("validated manager site");
            site.mutate_model_deployments(|transaction| {
                transaction.deployments.extend(deployments);
                Ok::<_, std::convert::Infallible>(())
            })
            .expect("accepted deployments");
            let state = FileCurrentState::open(&site_path, "site-one").expect("manager state");
            Self {
                directory,
                site,
                faults,
                state,
            }
        }

        fn reserve_generations(&self, deployment_id: &str, high_water: u64) {
            while self
                .state
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    deployment_id,
                )
                .expect("high-water")
                .unwrap_or(0)
                < high_water
            {
                self.state
                    .reserve_lifecycle_generation(
                        dotcore_domain::deployment::DeploymentOwner::Model,
                        deployment_id,
                    )
                    .expect("burned generation");
            }
        }

        fn high_water(&self, deployment_id: &str) -> Option<u64> {
            self.state
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    deployment_id,
                )
                .expect("high-water")
        }

        fn projection(&self, node_id: &str) -> NodeViewProjection {
            NodeViewProjection::for_site_node(&self.site.snapshot().expect("Site"), node_id)
                .expect("published node view")
        }

        fn report(
            &self,
            node_id: &str,
            deployments: Vec<PrivateModelDeploymentStatus>,
            cleanup_evidence: Vec<ModelCleanupEvidence>,
        ) -> Result<Vec<(String, u64, ModelCleanupDisposition)>, RemoteStatusError> {
            self.report_for(
                &self.projection(node_id),
                node_id,
                deployments,
                cleanup_evidence,
            )
        }

        fn report_for(
            &self,
            projection: &NodeViewProjection,
            node_id: &str,
            deployments: Vec<PrivateModelDeploymentStatus>,
            cleanup_evidence: Vec<ModelCleanupEvidence>,
        ) -> Result<Vec<(String, u64, ModelCleanupDisposition)>, RemoteStatusError> {
            let receipts = accept_node_status(
                &self.site,
                &self.state,
                &NodeStatusReport {
                    node_id,
                    projection,
                    model: &ModelControllerStatusSection {
                        deployments,
                        cleanup_evidence,
                    },
                    manager_session: &ManagerSession::parse("a".repeat(32)).expect("session"),
                    node_incarnation: &NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
                    status_sequence: 1,
                },
                Timing::test().model,
                &SystemClock,
            )?;
            Ok(receipts
                .into_iter()
                .map(|receipt| match receipt.payload {
                    Some(controller_status_receipt::Payload::Model(receipt)) => (
                        receipt.model_deployment_id,
                        receipt.lifecycle_generation,
                        ModelCleanupDisposition::try_from(receipt.disposition)
                            .expect("disposition"),
                    ),
                    Some(controller_status_receipt::Payload::Application(_)) | None => {
                        panic!("model receipt")
                    }
                })
                .collect())
        }

        fn disk(&self) -> Vec<u8> {
            std::fs::read(self.directory.path().join("site.yaml")).expect("site.yaml")
        }

        fn declared(&self) -> Vec<ModelDeploymentDeclaration> {
            self.site.snapshot().expect("Site").model_deployments
        }

        fn reloaded(&self) -> Vec<ModelDeploymentDeclaration> {
            SiteRepository::load(
                &self.directory.path().join("site.yaml"),
                crate::test_support::site_declaration_rules(),
            )
            .expect("restarted site")
            .snapshot()
            .expect("restarted site snapshot")
            .model_deployments
        }

        /// Asserts accepted memory, a clean site, and a restart all hold exactly `expected`.
        fn assert_durable(&self, expected: &[ModelDeploymentDeclaration]) {
            assert!(!self.site.is_dirty());
            assert_eq!(self.declared(), expected);
            assert_eq!(self.reloaded(), expected);
        }

        fn records(&self) -> Vec<(String, Option<u64>)> {
            self.state
                .snapshot()
                .expect("manager state")
                .model_deployments
                .into_iter()
                .map(|record| {
                    (
                        record.model_deployment_id,
                        record.reconciled_lifecycle_generation,
                    )
                })
                .collect()
        }
    }

    const MANAGER_SITE: &str = r#"siteId: site-one
modelDeploymentIdentityAllocator:
  namespace: 0123456789abcdef
  nextSequence: '1'
nodes:
- nodeId: manager-node
  host: 127.0.0.1
  displayName: Manager node
  siteManager: {}
- nodeId: worker-one
  host: 127.0.0.2
  displayName: Worker one
- nodeId: worker-two
  host: 127.0.0.3
  displayName: Worker two
executionTargets:
- executionTargetId: manager-target
  siteNodeId: manager-node
  displayName: manager-target
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
- executionTargetId: target-one
  siteNodeId: worker-one
  displayName: target-one
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
- executionTargetId: target-two
  siteNodeId: worker-two
  displayName: target-two
  platform: { operatingSystem: linux, architecture: amd64 }
  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
modelDeployments: []
applicationDeployments: []
"#;

    fn site_declaration(
        id: &str,
        node_id: &str,
        desired_state: ModelDeploymentDesiredState,
        generation: u64,
    ) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: node_id.to_owned(),
                execution_target_id: match node_id {
                    "manager-node" => "manager-target",
                    "worker-two" => "target-two",
                    _ => "target-one",
                }
                .to_owned(),
            }),
            requested_parallel_shape: dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One
                as i32,
            snapshot: Some(crate::test_support::local_model_snapshot(id)),
            desired_state: desired_state as i32,
            lifecycle_generation: generation,
            lifecycle_transition: match desired_state {
                Teardown => ModelDeploymentLifecycleTransition::Teardown,
                _ => ModelDeploymentLifecycleTransition::Create,
            } as i32,
            ..Default::default()
        }
    }

    /// Present deployment on `node_id` whose local-development engine its node resolves.
    fn node_resolved_declaration(id: &str, node_id: &str) -> ModelDeploymentDeclaration {
        let mut declaration = site_declaration(id, node_id, Present, 1);
        crate::test_support::retain_tag_only_engine(
            declaration.snapshot.as_mut().expect("snapshot"),
            "",
        );
        declaration
    }

    /// Present deployment on `node_id` whose development-source Nix engine its node pins.
    fn development_source_declaration(id: &str, node_id: &str) -> ModelDeploymentDeclaration {
        let mut declaration = site_declaration(id, node_id, Present, 1);
        crate::test_support::nix::retain_development_source_engine(
            declaration.snapshot.as_mut().expect("snapshot"),
            "",
        );
        declaration
    }

    fn engine_pin(
        launch_identity: &str,
    ) -> dotcore_proto::dotcore::deployment::v1::NodeResolvedLaunchIdentity {
        dotcore_proto::dotcore::deployment::v1::NodeResolvedLaunchIdentity {
            selection_key: crate::model::engine_pin::ENGINE_SELECTION_KEY.to_owned(),
            launch_identity: launch_identity.to_owned(),
        }
    }

    const FIRST_IMAGE: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const SECOND_IMAGE: &str =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn evidence(id: &str, generation: u64) -> ModelCleanupEvidence {
        ModelCleanupEvidence {
            model_deployment_id: id.to_owned(),
            lifecycle_generation: generation,
            realization_generation: Some(generation),
            never_realized: false,
            evidence_sha256: cleanup_evidence_digest(id, generation),
        }
    }

    fn retained_state(current: &impl CurrentStateRepository) -> ModelDeploymentState {
        current.snapshot().expect("state").model_deployments[0]
            .state
            .clone()
            .expect("current state")
    }

    /// Node status of a deployment under `lifecycle` with its recorded `message`, and optional
    /// byte progress as (completed, total).
    fn preparation_status(
        lifecycle: ModelLifecycleObservation,
        message: &str,
        progress: Option<(u64, Option<u64>)>,
    ) -> PrivateModelDeploymentStatus {
        let mut status = status("model-one", 3);
        status.lifecycle = lifecycle as i32;
        status.status_message = message.to_owned();
        status.binding_readiness = ModelBindingReadiness::NotReady as i32;
        status.binding_token.clear();
        status.realization_generation = None;
        status.observed_placements.clear();
        status.preparation_progress =
            progress.map(|(completed_bytes, total_bytes)| ModelPreparationProgress {
                completed_bytes,
                total_bytes,
            });
        status
    }

    /// The owner record state a node-hosted deployment gets once its node reports a record
    /// holding `local`, the state its node realizer recorded.
    fn node_hosted(local: &ModelDeploymentState) -> ModelDeploymentState {
        let record = CurrentModelDeployment {
            model_deployment_id: "model-one".to_owned(),
            record_revision: 1,
            reconciled_lifecycle_generation: Some(3),
            state: Some(local.clone()),
            recovery_barrier: None,
        };
        let declaration = declaration("model-one", 3);
        let status = crate::model::controller::deployment_status(
            &declaration
                .placement
                .as_ref()
                .expect("placement")
                .site_node_id,
            &declaration,
            Some(&record),
            None,
        );
        current_record(&status)
            .expect("node-hosted record")
            .state
            .expect("node-hosted state")
    }

    fn status(id: &str, generation: u64) -> PrivateModelDeploymentStatus {
        PrivateModelDeploymentStatus {
            steps: None,
            model_deployment_id: id.to_owned(),
            lifecycle_generation: generation,
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: "target-one".to_owned(),
            }),
            realization_generation: Some(generation),
            lifecycle: ModelLifecycleObservation::Ready as i32,
            status_message: "Ready".to_owned(),
            preparation_progress: None,
            binding_readiness: ModelBindingReadiness::Ready as i32,
            failure: None,
            state_revision: 2,
            observed_placements: vec![ObservedPlacement {
                managed_node_id: "worker-one".to_owned(),
                accelerator_device_ids: vec!["nvidia.com/gpu=3".to_owned()],
            }],
            binding_token: "b".repeat(64),
            resolved_launch_identities: Vec::new(),
            accelerator_memory: None,
        }
    }

    /// Commits `records` like a status report on the test timing and system time.
    fn commit(
        state: &impl CurrentStateRepository,
        records: Vec<CurrentModelDeployment>,
    ) -> Result<(), RemoteStatusError> {
        commit_changed_records(state, records, Timing::test().model, &SystemClock)
    }
}
