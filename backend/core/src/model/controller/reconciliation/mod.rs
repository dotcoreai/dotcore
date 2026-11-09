//! Reconciles desired model deployments against live runtime observations,
//! preparing each deployment's model artifacts and driving its workload and
//! lifecycle state toward the desired configuration.
//!
//! A single observer task runs one reconciliation at a time, waking promptly
//! when a lifecycle change occurs and otherwise settling into a periodic
//! safety scan that catches missing workloads and orphaned resources; a
//! failed reconciliation clears the serving endpoints it previously
//! published rather than leaving stale ones.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_artifact::{
    ArtifactCacheError, ArtifactManager, ArtifactPreparationError, ModelArtifactPreparation,
    ModelArtifactProgress, ModelArtifactProgressObserver, ModelArtifactStage,
    ModelArtifactStageObserver, PreparedModelArtifacts,
};
use dotcore_domain::real_time;
use dotcore_domain::{
    clock::Clock,
    diagnostic::diagnostic_chain,
    timing::{ReconcileCadence, ReconcileOutcome},
};
use dotcore_proto::dotcore::model::v1::{
    CurrentModelDeployment, ModelDeploymentActivationPhase, ModelDeploymentFailure,
    ModelDeploymentStatus,
};
use dotcore_runtime::{
    RuntimeBackendError, WorkloadAssetMaterialization, WorkloadObservation,
    WorkloadRealizationProvenance, WorkloadStartupObserver, WorkloadStartupProgressUnit,
    WorkloadStartupRequest,
};
use dotcore_site::{
    validation::hardware_refresh_interval,
    view::fencing::{LifecycleOperationAttempt, LifecycleOperationToken, RuntimeActionPermit},
};
use tokio::{sync::Notify, time::Instant};
use tracing::{Instrument, Span, debug, error, field, info, info_span, warn};

use super::{
    CurrentStateRepository, InferenceController, InferenceControllerInner, ModelLifecycleError,
    construction::site_with_deployments,
    launch::{resolved_launch, selected_target, workload_launch},
    ownership::{validate_action_observation, validate_observations},
    preparation::{PreparationEntry, PreparationLease, PreparationTicket},
    serving_targets::{ReadyServingTarget, serving_targets},
    state::{
        RegisteredObservationStatus, apply_observation, apply_startup_failure,
        checking_model_artifacts_state, configuration, current_error, desired_error,
        downloading_state, failed_state, is_absent, observation_identity, paused_state,
        pausing_state, previously_realized, realization_prepared_state,
        realization_startup_admitted_state, reconciled_generation, register_observation_status,
        resolve_observation_status, resume_matches_generation, resume_matches_lifecycle,
        resume_phase, resume_start_state, resume_start_uncertain_state, resuming_state,
        runtime_startup_progress_state, startup_failure_diagnostics, startup_pending,
        unavailable_state,
    },
};
use crate::{
    deployment_logs::PreparationOwner,
    dispatch::model::{RuntimeTargetError, RuntimeTargetMap, TargetObservation},
    model::{
        domain::ModelDeploymentId, resolved::ResolvedModelLaunch,
        selection::RetainedTargetCapabilityError,
    },
};

impl<D, S, R> InferenceControllerInner<D, S, R>
where
    D: super::ModelRealizationInput,
    S: CurrentStateRepository,
    R: super::ModelRealizationResults,
{
    pub(super) async fn reconcile(&self) -> Result<ReconcileOutcome, ModelLifecycleError> {
        let span = tracing::debug_span!(
            "model_reconcile",
            operation = "model_reconcile",
            state = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
        );

        let result = self.reconcile_operation().instrument(span.clone()).await;
        match &result {
            Ok(state) => {
                span.record("state", format!("{state:?}"));
                span.record("outcome", "success");
            }
            // The observer reports entering and leaving this state once, so each pass stays quiet.
            Err(error) if error.awaits_desired_view() => {
                span.record("outcome", "waiting");
                span.record("error_kind", "awaiting_desired_view");
            }
            Err(error)
                if matches!(
                    error.category(),
                    super::ModelLifecycleErrorCategory::OutcomeUncertain
                ) =>
            {
                span.record("outcome", "uncertain");
                span.record("error_kind", "outcome_uncertain");
                error!(
                    parent: &span,
                    error_kind = "outcome_uncertain",
                    error = %diagnostic_chain(error),
                    "reconciliation outcome is uncertain"
                );
            }
            Err(error) => {
                span.record("outcome", "failed");
                span.record("error_kind", "reconciliation");
                error!(
                    parent: &span,
                    error_kind = "reconciliation",
                    error = %diagnostic_chain(error),
                    "reconciliation failed"
                );
            }
        }
        result
    }

    async fn reconcile_operation(&self) -> Result<ReconcileOutcome, ModelLifecycleError> {
        let _reconciliation = self.reconciliation.lock().await;
        let result = async {
            self.pin_withheld_engines().await?;
            // A shut-down realizer starts no further work, such as a build or a workload.
            if self.shutdown.is_cancelled() {
                return Ok(ReconcileOutcome::Settled);
            }
            self.initialize_missing_current_records().await?;
            let previously_realized_deployments = self
                .state
                .snapshot()
                .map_err(current_error)?
                .model_deployments
                .into_iter()
                .filter_map(|record| {
                    record
                        .state
                        .as_ref()
                        .is_some_and(previously_realized)
                        .then_some(record.model_deployment_id)
                })
                .collect::<BTreeSet<_>>();
            let preparations = self.prepare_model_artifacts().await?;
            self.reconcile_once(&preparations, &previously_realized_deployments)
                .await
        }
        .await;
        if !matches!(
            &result,
            Err(ModelLifecycleError::Runtime(
                crate::dispatch::model::RuntimeTargetError::StaleCompletion
            ))
        ) && result.is_err()
        {
            self.serving_targets_publisher.withdraw_all();
        }
        result
    }

    async fn initialize_missing_current_records(&self) -> Result<(), ModelLifecycleError> {
        let operation = self.operation.lock().await;
        let site = site_with_deployments(&self.desired, &self.runtimes).map_err(desired_error)?;
        let snapshot = self.state.snapshot().map_err(current_error)?;
        let mut missing_operations = BTreeMap::new();
        for deployment in &site.model_deployments {
            // A teardown whose cleanup is already confirmed needs no operational record; it
            // would only be recreated and removed again on every pass.
            if snapshot
                .model_deployments
                .iter()
                .any(|record| record.model_deployment_id == deployment.model_deployment_id)
                || self.results.cleanup_confirmed(deployment)?
            {
                continue;
            }
            let operation = self.capture_operation(
                deployment,
                LifecycleOperationAttempt::Runtime {
                    reconcile_generation: deployment.lifecycle_generation,
                },
            )?;
            missing_operations.insert(deployment.model_deployment_id.clone(), operation);
        }
        if missing_operations.is_empty() {
            return Ok(());
        }
        drop(operation);

        let observations = self.runtimes.discover_workloads().await?;
        let _operation = self.operation.lock().await;
        let site = site_with_deployments(&self.desired, &self.runtimes).map_err(desired_error)?;
        validate_observations(&site, &self.runtimes, &observations)?;
        let snapshot = self.state.snapshot().map_err(current_error)?;
        let missing = super::construction::missing_current_records(
            &site,
            &snapshot.model_deployments,
            &observations,
            &self.artifacts,
        )?;
        for record in missing
            .into_iter()
            .filter(|record| missing_operations.contains_key(&record.model_deployment_id))
        {
            let desired = site
                .model_deployments
                .iter()
                .find(|deployment| deployment.model_deployment_id == record.model_deployment_id)
                .expect("missing current record belongs to projected desired input");
            let operation = missing_operations
                .get(&record.model_deployment_id)
                .ok_or_else(|| configuration("missing state belongs to changed desired input"))?;
            let initialized = self.commit_operation(operation, || {
                if self
                    .state
                    .snapshot()
                    .map_err(current_error)?
                    .model_deployments
                    .iter()
                    .any(|existing| existing.model_deployment_id == record.model_deployment_id)
                {
                    return Ok(false);
                }
                self.state
                    .upsert(record, None)
                    .map(|_| true)
                    .map_err(current_error)
            })??;
            if initialized {
                info!(
                    event_kind = "model_current_state_initialized",
                    deployment_id = %desired.model_deployment_id,
                    lifecycle_generation = desired.lifecycle_generation,
                    "model realizer initialized missing operational state"
                );
            }
        }
        Ok(())
    }

    async fn prepare_model_artifacts(
        &self,
    ) -> Result<BTreeMap<String, PreparedDeployment>, ModelLifecycleError> {
        const MAX_CONCURRENT_DEPLOYMENT_PREPARATIONS: usize = 2;

        let site = {
            let _operation = self.operation.lock().await;
            site_with_deployments(&self.desired, &self.runtimes).map_err(desired_error)?
        };
        self.sync_preparation_gates(&site.model_deployments)?;
        let mut preparations = BTreeMap::new();
        let mut model_tasks = tokio::task::JoinSet::new();
        let mut acquisition_tasks = tokio::task::JoinSet::new();
        let progress_notification = Arc::new(Notify::new());
        let mut wakeup = PassWakeup::new(&self.wakeup);
        let mut progress_publications = BTreeMap::new();
        let mut active_entries = Vec::new();
        let mut active_leases = BTreeMap::new();
        let mut active_operations = BTreeMap::new();
        let mut failed_downloads = Vec::new();
        // Each deployment's preparation attempt reports all its steps through one output, so a
        // removal of its logs mid-attempt stops every later step from writing.
        let mut attempt_outputs = BTreeMap::new();
        let mut planned = site.model_deployments.iter();
        let mut planning_complete = false;
        let mut first_error = None;
        while !planning_complete || !model_tasks.is_empty() {
            while first_error.is_none()
                && model_tasks.len() < MAX_CONCURRENT_DEPLOYMENT_PREPARATIONS
                && !planning_complete
            {
                let Some(desired) = planned.next() else {
                    planning_complete = true;
                    break;
                };
                let Some((deployment_id, resolved, requirements, lease, entry)) =
                    self.plan_preparation(&site, desired).await?
                else {
                    continue;
                };
                let ticket = lease.ticket();
                let quiet_revalidation = entry.previously_prepared(&ticket);
                active_entries.push(Arc::clone(&entry));
                if !self
                    .invalidate_stale_download_state(&deployment_id, &ticket)
                    .await?
                {
                    continue;
                }
                let operation = {
                    let _transition = entry.transition().await;
                    let _controller_operation = self.operation.lock().await;
                    self.capture_operation(
                        &ticket.declaration,
                        LifecycleOperationAttempt::Preparation {
                            incarnation: ticket.incarnation,
                            generation: ticket.generation,
                        },
                    )?
                };
                let cancellation = lease.cancellation();
                active_leases.insert(deployment_id.clone(), lease);
                active_operations.insert(deployment_id.clone(), operation);
                let mut publication = DownloadProgressPublication::new(
                    ticket.clone(),
                    download_progress_kind(requirements),
                    self.timing.download_progress_interval,
                );
                publication.quiet_revalidation = quiet_revalidation;
                let artifact_progress = publication.observer(progress_notification.clone());
                let artifact_stage = publication.stage_observer(progress_notification.clone());
                let startup_progress = publication.runtime_observer(progress_notification.clone());
                progress_publications.insert(deployment_id.clone(), publication);
                let artifacts = self.artifacts.clone();
                let preparation_output = self
                    .preparation_logs
                    .output(PreparationOwner::Model, &deployment_id);
                attempt_outputs.insert(deployment_id.clone(), preparation_output.clone());
                let runtimes = self.runtimes.clone();
                let execution_target_id = desired
                    .placement
                    .as_ref()
                    .ok_or_else(|| configuration("local model deployment has no exact placement"))?
                    .execution_target_id
                    .clone();
                let startup_request = WorkloadStartupRequest {
                    owner_id: deployment_id.clone(),
                    provenance: WorkloadRealizationProvenance::FirstActivation,
                };
                let parent = Span::current();
                model_tasks.spawn(
                    async move {
                        let result = match requirements.host_mounts {
                            WorkloadAssetMaterialization::Physical => artifacts
                                .prepare_model(
                                    &resolved.artifacts,
                                    &cancellation,
                                    Some(artifact_progress),
                                    Some(artifact_stage),
                                    &preparation_output,
                                )
                                .await
                                .map(|prepared| ModelPreparationResult::Physical {
                                    prepared,
                                    prepare_engine: requirements.executable_artifact
                                        == WorkloadAssetMaterialization::Physical,
                                })
                                .map_err(ModelPreparationError::Artifact),
                            WorkloadAssetMaterialization::ReferenceOnly => runtimes
                                .prepare_realization(
                                    &execution_target_id,
                                    &startup_request,
                                    cancellation,
                                    Some(startup_progress),
                                )
                                .await
                                .map(|()| ModelPreparationResult::ReferenceOnly)
                                .map_err(ModelPreparationError::Runtime),
                        };
                        (deployment_id, resolved, ticket, result)
                    }
                    .instrument(parent),
                );
            }

            if model_tasks.is_empty() {
                break;
            }
            let now = self.clock.now();
            let next_progress_write = progress_publications
                .values()
                .filter_map(|publication| publication.next_due_write(now))
                .min();
            let joined = tokio::select! {
                biased;
                joined = model_tasks.join_next() => joined,
                () = wait_for_progress_write(self.clock.as_ref(), next_progress_write) => {
                    collect_observed_download_progress(&mut progress_publications);
                    self.flush_due_download_progress(
                        &mut progress_publications,
                        &active_operations,
                    ).await?;
                    continue;
                }
                () = progress_notification.notified() => {
                    collect_observed_download_progress(&mut progress_publications);
                    self.flush_due_download_progress(
                        &mut progress_publications,
                        &active_operations,
                    ).await?;
                    continue;
                }
                () = wakeup.notified() => {
                    self.sync_input_preparation_gates().await?;
                    continue;
                }
            };
            let Some(joined) = joined else {
                break;
            };
            match joined {
                Err(error) => {
                    first_error = Some(ModelLifecycleError::Task(error));
                    cancel_active_preparations(&active_entries);
                }
                Ok((
                    deployment_id,
                    resolved,
                    ticket,
                    Err(ModelPreparationError::Artifact(ArtifactCacheError::Cancelled)),
                )) => {
                    let operation = active_operations
                        .get(&deployment_id)
                        .expect("active preparation operation");
                    let downloaded = self
                        .finish_download_progress(
                            &deployment_id,
                            &mut progress_publications,
                            operation,
                        )
                        .await?;
                    active_leases.remove(&deployment_id);
                    self.invalidate_stale_download_state_fenced(&deployment_id, &ticket, operation)
                        .await?;
                    if downloaded {
                        debug!(event_kind = "model_artifact_download_cancelled", deployment_id = %deployment_id, "model artifact download cancelled");
                    }
                    drop((resolved, ticket));
                }
                Ok((
                    deployment_id,
                    resolved,
                    ticket,
                    Err(ModelPreparationError::Runtime(
                        crate::dispatch::model::RuntimeTargetError::Adapter { source, .. },
                    )),
                )) if source.is_preparation_cancelled() => {
                    let operation = active_operations
                        .get(&deployment_id)
                        .expect("active preparation operation");
                    let downloaded = self
                        .finish_download_progress(
                            &deployment_id,
                            &mut progress_publications,
                            operation,
                        )
                        .await?;
                    active_leases.remove(&deployment_id);
                    self.invalidate_stale_download_state_fenced(&deployment_id, &ticket, operation)
                        .await?;
                    if downloaded {
                        debug!(event_kind = "model_runtime_preparation_cancelled", deployment_id = %deployment_id, "Runtime realization preparation cancelled");
                    }
                    drop((resolved, ticket));
                }
                Ok((deployment_id, _resolved, ticket, Err(error))) => {
                    let operation = active_operations
                        .get(&deployment_id)
                        .expect("active preparation operation");
                    self.finish_download_progress(
                        &deployment_id,
                        &mut progress_publications,
                        operation,
                    )
                    .await?;
                    failed_downloads.push((deployment_id.clone(), ticket));
                    match error {
                        ModelPreparationError::Artifact(error) => {
                            warn!(event_kind = "model_artifact_download_failed", deployment_id = %deployment_id, error = %error, "model artifact preparation failed");
                            first_error = Some(ModelLifecycleError::Artifact(error));
                        }
                        ModelPreparationError::Runtime(error) => {
                            warn!(event_kind = "model_runtime_preparation_failed", deployment_id = %deployment_id, error = %error, "Runtime realization preparation failed");
                            first_error = Some(ModelLifecycleError::Runtime(error));
                        }
                    }
                    cancel_active_preparations(&active_entries);
                }
                Ok((deployment_id, resolved, ticket, Ok(prepared))) => {
                    let operation = active_operations
                        .get(&deployment_id)
                        .expect("active preparation operation")
                        .clone();
                    let downloaded = self
                        .finish_download_progress(
                            &deployment_id,
                            &mut progress_publications,
                            &operation,
                        )
                        .await?;
                    if downloaded {
                        info!(event_kind = "model_preparation_progress_completed", deployment_id = %deployment_id, "model preparation progress completed");
                    }
                    match prepared {
                        ModelPreparationResult::Physical {
                            prepared,
                            prepare_engine,
                        } => {
                            let quiet_revalidation = ticket.entry.previously_prepared(&ticket)
                                && !downloaded
                                && !prepared.replaced;
                            if !prepare_engine {
                                if self
                                    .finish_virtualized_realization_preparation(
                                        &deployment_id,
                                        &ticket,
                                        &operation,
                                    )
                                    .await?
                                {
                                    self.commit_preparation_if_current(
                                        &deployment_id,
                                        &ticket,
                                        &operation,
                                        || {
                                            ticket.entry.mark_prepared(&ticket);
                                            preparations.insert(
                                                deployment_id.clone(),
                                                PreparedDeployment { prepared },
                                            );
                                        },
                                    )
                                    .await?;
                                }
                                active_leases.remove(&deployment_id);
                            } else if let Some(announce) = self
                                .begin_engine_acquisition(
                                    &deployment_id,
                                    &ticket,
                                    &operation,
                                    quiet_revalidation,
                                )
                                .await?
                            {
                                if announce {
                                    debug!(
                                        event_kind = "model_preparation_complete",
                                        deployment_id = %deployment_id,
                                        stage = "artifact_prepared",
                                        "model artifacts prepared; acquiring engine image",
                                    );
                                }
                                let cancellation = active_leases
                                    .get(&deployment_id)
                                    .expect("active preparation lease")
                                    .cancellation();
                                let artifacts = self.artifacts.clone();
                                let preparation_output =
                                    attempt_outputs.remove(&deployment_id).unwrap_or_else(|| {
                                        self.preparation_logs
                                            .output(PreparationOwner::Model, &deployment_id)
                                    });
                                let parent = Span::current();
                                acquisition_tasks.spawn(
                                    async move {
                                        let result = artifacts
                                            .prepare_engine(
                                                &resolved.engine.preparation(),
                                                cancellation,
                                                &preparation_output,
                                            )
                                            .await;
                                        (deployment_id, ticket, prepared, announce, result)
                                    }
                                    .instrument(parent),
                                );
                            } else {
                                active_leases.remove(&deployment_id);
                            }
                        }
                        ModelPreparationResult::ReferenceOnly => {
                            self.finish_virtualized_realization_preparation(
                                &deployment_id,
                                &ticket,
                                &operation,
                            )
                            .await?;
                            active_leases.remove(&deployment_id);
                        }
                    }
                }
            }
        }

        for (deployment_id, publication) in &progress_publications {
            active_leases.remove(deployment_id);
            let operation = active_operations
                .get(deployment_id)
                .expect("active preparation operation");
            self.invalidate_stale_download_state_fenced(
                deployment_id,
                &publication.ticket,
                operation,
            )
            .await?;
        }
        progress_publications.clear();
        for (deployment_id, ticket) in failed_downloads {
            active_leases.remove(&deployment_id);
            let operation = active_operations
                .get(&deployment_id)
                .expect("active preparation operation");
            self.invalidate_stale_download_state_fenced(&deployment_id, &ticket, operation)
                .await?;
        }
        loop {
            let joined = tokio::select! {
                joined = acquisition_tasks.join_next() => joined,
                () = wakeup.notified() => {
                    self.sync_input_preparation_gates().await?;
                    continue;
                }
            };
            let Some(joined) = joined else {
                break;
            };
            match joined {
                Err(error) => {
                    first_error.get_or_insert(ModelLifecycleError::Task(error));
                    cancel_active_preparations(&active_entries);
                }
                Ok((deployment_id, ticket, prepared, announce, result)) => {
                    let operation = active_operations
                        .get(&deployment_id)
                        .expect("active preparation operation")
                        .clone();
                    self.commit_engine_acquisition(
                        &mut preparations,
                        EngineAcquisitionCommit {
                            deployment_id: &deployment_id,
                            ticket: &ticket,
                            operation: &operation,
                            prepared,
                            announce,
                            result,
                        },
                    )
                    .await?;
                    active_leases.remove(&deployment_id);
                }
            }
        }

        drop(active_leases);
        for entry in active_entries {
            let transition = entry.transition().await;
            entry.refresh_after_drain();
            drop(transition);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(preparations)
    }

    /// Re-reads the input after a change notification during preparation and closes the
    /// preparation of every deployment it no longer wants present. The pass that follows
    /// preparation reconciles the change itself.
    async fn sync_input_preparation_gates(&self) -> Result<(), ModelLifecycleError> {
        let site = {
            let _operation = self.operation.lock().await;
            site_with_deployments(&self.desired, &self.runtimes).map_err(desired_error)?
        };
        self.sync_preparation_gates(&site.model_deployments)
    }

    /// Keeps each deployment's preparation gate in step with its intent: a pause or teardown
    /// cancels the active preparation and fences its result, and Present intent admits a fresh one.
    fn sync_preparation_gates(
        &self,
        deployments: &[dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration],
    ) -> Result<(), ModelLifecycleError> {
        for deployment in deployments {
            let entry = self.preparations.entry(&deployment.model_deployment_id);
            match super::state::desired_state(deployment)? {
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present => {
                    entry.reopen();
                }
                _ => entry.withdraw(),
            }
        }
        Ok(())
    }

    async fn plan_preparation(
        &self,
        site: &dotcore_proto::dotcore::site::v1::Site,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    ) -> Result<
        Option<(
            String,
            ResolvedModelLaunch,
            dotcore_runtime::WorkloadMaterializationRequirements,
            PreparationLease,
            Arc<PreparationEntry>,
        )>,
        ModelLifecycleError,
    > {
        if is_absent(desired)
            || desired.lifecycle_recovery_required
            || matches!(
                super::state::desired_state(desired)?,
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused
            )
        {
            return Ok(None);
        }
        if self
            .state
            .snapshot()
            .map_err(current_error)?
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id)
            .is_some_and(|record| {
                // Progress from another generation is rebound by the reconcile pass first, and a
                // paused record gets its resume activation there before preparation may replace it.
                super::state::record_for_declared_generation(&record, desired).is_some()
                    || record.state.is_some_and(|state| {
                        matches!(
                            ModelDeploymentStatus::try_from(state.status),
                            Ok(ModelDeploymentStatus::Failed
                                | ModelDeploymentStatus::Paused
                                | ModelDeploymentStatus::Pausing)
                        )
                    })
            })
        {
            return Ok(None);
        }
        let selected_target = selected_target(site, desired)?;
        let requirements = self.runtimes.materialization_requirements(
            &desired
                .placement
                .as_ref()
                .ok_or_else(|| configuration("local model deployment has no exact placement"))?
                .execution_target_id,
        )?;
        let resolved = resolved_launch(desired)?;
        // The present reconcile records the mismatch, under the deployment's runtime permit.
        if self
            .target_capability_mismatch(selected_target, &resolved)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        let deployment_id = desired.model_deployment_id.clone();
        let entry = self.preparations.entry(&deployment_id);
        let _transition = entry.transition().await;
        let _operation = self.operation.lock().await;
        let current = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .find(|candidate| candidate.model_deployment_id == deployment_id);
        if current.as_ref() != Some(desired)
            || current.as_ref().is_none_or(|candidate| {
                !super::state::desired_state(candidate).is_ok_and(|state| {
                    state == dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present
                })
            })
        {
            return Ok(None);
        }
        let Some(lease) = entry.register(desired.clone()) else {
            return Ok(None);
        };
        if requirements.host_mounts == WorkloadAssetMaterialization::ReferenceOnly {
            let snapshot = self.state.snapshot().map_err(current_error)?;
            let existing = snapshot
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment_id);
            let Some(state) = realization_startup_admitted_state(
                existing.as_ref().and_then(|record| record.state.as_ref()),
            ) else {
                return Ok(None);
            };
            self.state
                .upsert(
                    dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                        model_deployment_id: deployment_id.clone(),
                        reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                        record_revision: 0,
                        recovery_barrier: None,
                        state: Some(state),
                    },
                    existing.map(|record| record.record_revision),
                )
                .map_err(current_error)?;
        }
        Ok(Some((deployment_id, resolved, requirements, lease, entry)))
    }

    async fn invalidate_stale_download_state(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_preparation_state_if(deployment_id, ticket, checking_model_artifacts_state)
            .await
    }

    async fn invalidate_stale_download_state_fenced(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: &LifecycleOperationToken,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_preparation_state_if_inner(
            deployment_id,
            ticket,
            Some(operation),
            checking_model_artifacts_state,
        )
        .await
    }

    async fn flush_due_download_progress(
        &self,
        publications: &mut BTreeMap<String, DownloadProgressPublication>,
        operations: &BTreeMap<String, LifecycleOperationToken>,
    ) -> Result<(), ModelLifecycleError> {
        let now = self.clock.now();
        let due = publications
            .iter()
            .filter(|(_, publication)| publication.write_is_due(now))
            .map(|(deployment_id, _)| deployment_id.clone())
            .collect::<Vec<_>>();
        for deployment_id in due {
            let operation = operations
                .get(&deployment_id)
                .expect("active preparation operation");
            self.persist_artifact_stages(&deployment_id, publications, operation)
                .await?;
            self.persist_download_progress(&deployment_id, publications, operation, false)
                .await?;
        }
        Ok(())
    }

    async fn finish_download_progress(
        &self,
        deployment_id: &str,
        publications: &mut BTreeMap<String, DownloadProgressPublication>,
        operation: &LifecycleOperationToken,
    ) -> Result<bool, ModelLifecycleError> {
        collect_observed_download_progress(publications);
        self.persist_artifact_stages(deployment_id, publications, operation)
            .await?;
        self.persist_download_progress(deployment_id, publications, operation, true)
            .await?;
        let downloaded = publications
            .remove(deployment_id)
            .is_some_and(|publication| publication.latest.is_some());
        Ok(downloaded)
    }

    async fn persist_artifact_stages(
        &self,
        deployment_id: &str,
        publications: &mut BTreeMap<String, DownloadProgressPublication>,
        operation: &LifecycleOperationToken,
    ) -> Result<(), ModelLifecycleError> {
        while let Some(observed) = publications
            .get_mut(deployment_id)
            .and_then(|publication| publication.pending_stages.pop_front())
        {
            let active =
                observed.progress_before.is_some() || observed.stage == ModelArtifactStage::Publish;
            if let Some(progress) = observed.progress_before
                && let Some(publication) = publications.get_mut(deployment_id)
            {
                publication.latest = Some(progress);
            }
            self.persist_download_progress(deployment_id, publications, operation, true)
                .await?;
            let ticket = match publications.get(deployment_id) {
                Some(publication) => publication.ticket.clone(),
                None => return Ok(()),
            };
            let stage = observed.stage;
            let quiet = publications
                .get_mut(deployment_id)
                .is_some_and(|publication| {
                    if active {
                        publication.quiet_revalidation = false;
                    }
                    publication.quiet_revalidation
                });
            if quiet
                && matches!(
                    stage,
                    ModelArtifactStage::Verify | ModelArtifactStage::Reuse
                )
            {
                continue;
            }
            let mut changed = false;
            let current = self
                .update_preparation_state_if_inner(
                    deployment_id,
                    &ticket,
                    Some(operation),
                    |previous| {
                        let next = super::state::artifact_preparation_stage_state(previous, stage);
                        changed = next.is_some();
                        next
                    },
                )
                .await?;
            if current && changed {
                info!(
                    event_kind = "model_preparation_stage",
                    deployment_id = %deployment_id,
                    stage = stage.as_str(),
                    "model artifact preparation stage changed",
                );
            }
        }
        if let Some(publication) = publications.get_mut(deployment_id)
            && let Some(progress) = publication.pending_trailing.take()
        {
            publication.latest = Some(progress);
        }
        Ok(())
    }

    async fn persist_download_progress(
        &self,
        deployment_id: &str,
        publications: &mut BTreeMap<String, DownloadProgressPublication>,
        operation: &LifecycleOperationToken,
        force: bool,
    ) -> Result<(), ModelLifecycleError> {
        let now = self.clock.now();
        let Some((ticket, observed, kind, first)) =
            publications.get(deployment_id).and_then(|publication| {
                publication
                    .pending_write(now, force)
                    .map(|(ticket, progress)| {
                        (
                            ticket,
                            progress,
                            publication.kind,
                            publication.persisted.is_none(),
                        )
                    })
            })
        else {
            return Ok(());
        };
        let progress = observed.progress.clone();
        let current = self
            .update_preparation_state_fenced(deployment_id, &ticket, operation, |previous| {
                match kind {
                    DownloadProgressKind::Artifact => downloading_state(previous, progress.clone()),
                    DownloadProgressKind::RuntimeStartup => runtime_startup_progress_state(
                        previous,
                        progress.clone(),
                        observed
                            .detail
                            .as_deref()
                            .unwrap_or("Preparing model realization"),
                    ),
                }
            })
            .await?;
        if current {
            if let Some(publication) = publications.get_mut(deployment_id) {
                publication.mark_persisted(observed, now);
            }
            if first {
                info!(event_kind = "model_preparation_progress_started", deployment_id = %deployment_id, "model preparation progress started");
            }
        } else {
            publications.remove(deployment_id);
        }
        Ok(())
    }

    async fn finish_virtualized_realization_preparation(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: &LifecycleOperationToken,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_preparation_state_fenced(
            deployment_id,
            ticket,
            operation,
            realization_prepared_state,
        )
        .await
    }

    async fn begin_engine_acquisition(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: &LifecycleOperationToken,
        quiet_revalidation: bool,
    ) -> Result<Option<bool>, ModelLifecycleError> {
        let mut announced = false;
        let current = self
            .update_preparation_state_if_inner(deployment_id, ticket, Some(operation), |previous| {
                let next = if quiet_revalidation {
                    None
                } else {
                    super::state::acquiring_engine_image_state(previous)
                };
                announced = next.is_some();
                next
            })
            .await?;
        Ok(current.then_some(announced))
    }

    async fn commit_engine_acquisition(
        &self,
        preparations: &mut BTreeMap<String, PreparedDeployment>,
        commit: EngineAcquisitionCommit<'_>,
    ) -> Result<(), ModelLifecycleError> {
        match commit.result {
            Ok(()) => {
                // The record keeps the acquisition message until its next write, which a Loading
                // observation skips, so clear it here. The guard leaves a Ready record alone
                // because preparation re-runs on every pass.
                if self
                    .update_preparation_state_if_inner(
                        commit.deployment_id,
                        commit.ticket,
                        Some(commit.operation),
                        super::state::preparation_completed_state,
                    )
                    .await?
                    && self
                        .commit_preparation_if_current(
                            commit.deployment_id,
                            commit.ticket,
                            commit.operation,
                            || {
                                commit.ticket.entry.mark_prepared(commit.ticket);
                                preparations.insert(
                                    commit.deployment_id.to_owned(),
                                    PreparedDeployment {
                                        prepared: commit.prepared,
                                    },
                                );
                            },
                        )
                        .await?
                {
                    if commit.announce {
                        info!(
                            event_kind = "model_preparation_ready",
                            deployment_id = %commit.deployment_id,
                            stage = "ready",
                            "model preparation complete",
                        );
                    }
                } else {
                    debug!(event_kind = "model_preparation_stale", deployment_id = %commit.deployment_id, "model preparation result ignored after desired-state change");
                }
            }
            Err(ArtifactPreparationError::OciPreparerNotConfigured) => {
                return Err(ModelLifecycleError::configuration(
                    "physical OCI model deployment requires image preparation",
                ));
            }
            Err(
                ArtifactPreparationError::Oci(dotcore_artifact::OciPreparationError::Cancelled)
                | ArtifactPreparationError::Nix(dotcore_artifact::NixPreparationError::Cancelled)
                | ArtifactPreparationError::DockerImage(
                    dotcore_artifact::DockerImagePreparationError::Cancelled
                    | dotcore_artifact::DockerImagePreparationError::Nix(
                        dotcore_artifact::NixPreparationError::Cancelled,
                    ),
                ),
            ) => {
                debug!(
                    event_kind = "model_preparation_cancelled",
                    deployment_id = %commit.deployment_id,
                    "model engine artifact preparation cancelled",
                );
            }
            Err(error) => {
                let loader_output_tail = match &error {
                    ArtifactPreparationError::DockerImage(error) => error.loader_stderr(),
                    _ => None,
                };
                let diagnostics =
                    super::state::engine_image_acquisition_diagnostics(loader_output_tail);
                self.update_preparation_state_fenced(
                    commit.deployment_id,
                    commit.ticket,
                    commit.operation,
                    |previous| {
                        super::state::engine_image_acquisition_failed_state(previous, diagnostics)
                    },
                )
                .await?;
                warn!(event_kind = "model_preparation_failed", deployment_id = %commit.deployment_id, error_kind = "engine_image_acquisition", error = %error, "model engine artifact preparation failed");
            }
        }
        Ok(())
    }

    pub(super) fn capture_operation(
        &self,
        deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        attempt: LifecycleOperationAttempt,
    ) -> Result<LifecycleOperationToken, ModelLifecycleError> {
        self.runtimes
            .capture_operation(deployment, attempt)
            .map_err(ModelLifecycleError::Runtime)
    }

    pub(super) fn commit_operation<T>(
        &self,
        token: &LifecycleOperationToken,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ModelLifecycleError> {
        self.runtimes
            .commit_operation(token, commit)
            .map_err(ModelLifecycleError::Runtime)
    }

    fn commit_status_operation<T>(
        &self,
        token: &LifecycleOperationToken,
        binding: Option<&crate::inference::EngineBinding>,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ModelLifecycleError> {
        match self.commit_operation(token, commit) {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(binding) = binding {
                    self.engine_integrations.retire_inactive_binding(binding);
                }
                Err(error)
            }
        }
    }

    pub(super) fn authorize_deployment_action(
        &self,
        deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    ) -> Result<RuntimeActionPermit, ModelLifecycleError> {
        self.runtimes
            .authorize_deployment_action(deployment)
            .map_err(|error| {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(
                        deployment.model_deployment_id.clone(),
                    ));
                self.engine_integrations
                    .retire_inactive_deployment(&deployment.model_deployment_id);
                ModelLifecycleError::Runtime(error)
            })
    }

    fn preparation_is_current(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
    ) -> Result<bool, ModelLifecycleError> {
        let current = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .find(|candidate| candidate.model_deployment_id == deployment_id);
        Ok(self
            .preparations
            .is_current(deployment_id, ticket.incarnation)
            && ticket.entry.accepts(ticket)
            && current.as_ref() == Some(&ticket.declaration)
            && current.as_ref().is_some_and(|candidate| {
                super::state::desired_state(candidate).is_ok_and(|state| {
                    state == dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present
                })
            }))
    }

    async fn commit_preparation_if_current<T>(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: &LifecycleOperationToken,
        commit: impl FnOnce() -> T,
    ) -> Result<bool, ModelLifecycleError> {
        let _transition = ticket.entry.transition().await;
        let _controller_operation = self.operation.lock().await;
        if !self.preparation_is_current(deployment_id, ticket)? {
            return Ok(false);
        }
        self.commit_operation(operation, commit)?;
        Ok(true)
    }

    async fn update_preparation_state_fenced(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: &LifecycleOperationToken,
        next: impl FnOnce(
            Option<&dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
        ) -> dotcore_proto::dotcore::model::v1::ModelDeploymentState,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_preparation_state_if_inner(deployment_id, ticket, Some(operation), |previous| {
            Some(next(previous))
        })
        .await
    }

    async fn update_preparation_state_if(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        next: impl FnOnce(
            Option<&dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
        ) -> Option<dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_preparation_state_if_inner(deployment_id, ticket, None, next)
            .await
    }

    async fn update_preparation_state_if_inner(
        &self,
        deployment_id: &str,
        ticket: &PreparationTicket,
        operation: Option<&LifecycleOperationToken>,
        next: impl FnOnce(
            Option<&dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
        ) -> Option<dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
    ) -> Result<bool, ModelLifecycleError> {
        let _transition = ticket.entry.transition().await;
        let _controller_operation = self.operation.lock().await;
        if !self.preparation_is_current(deployment_id, ticket)? {
            return Ok(false);
        }
        let update = || {
            let snapshot = self.state.snapshot().map_err(current_error)?;
            let existing = snapshot
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment_id);
            let Some(state) = next(existing.as_ref().and_then(|record| record.state.as_ref()))
            else {
                return Ok(());
            };
            self.state
                .upsert(
                    dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                        model_deployment_id: deployment_id.to_owned(),
                        reconciled_lifecycle_generation: Some(
                            ticket.declaration.lifecycle_generation,
                        ),
                        record_revision: 0,
                        recovery_barrier: None,
                        state: Some(state),
                    },
                    existing.map(|record| record.record_revision),
                )
                .map(|_| ())
                .map_err(current_error)
        };
        match operation {
            Some(operation) => self.commit_operation(operation, update)??,
            None => {
                let _permit = self.authorize_deployment_action(&ticket.declaration)?;
                update()?;
            }
        }
        Ok(true)
    }

    async fn reconcile_once(
        &self,
        preparations: &BTreeMap<String, PreparedDeployment>,
        previously_realized_deployments: &BTreeSet<String>,
    ) -> Result<ReconcileOutcome, ModelLifecycleError> {
        let site = site_with_deployments(&self.desired, &self.runtimes).map_err(desired_error)?;
        let observations = self.runtimes.discover_workloads().await?;
        validate_observations(&site, &self.runtimes, &observations)?;
        let mut observations = observations
            .into_iter()
            .filter_map(|target| {
                let deployment_id = observation_identity(&target.observation)
                    .map(|identity| identity.correlation.owner_id.clone());
                deployment_id.map(|deployment_id| (deployment_id, target))
            })
            .collect::<BTreeMap<_, _>>();
        let current = self
            .state
            .snapshot()
            .map_err(current_error)?
            .model_deployments
            .into_iter()
            .map(|record| (record.model_deployment_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        // Every deployment in this pass's snapshot claims its observation before any runtime call,
        // so a declaration that changes mid-pass is skipped rather than left behind as an orphan.
        let mut deployment_observations = site
            .model_deployments
            .iter()
            .filter_map(|desired| observations.remove_entry(&desired.model_deployment_id))
            .collect::<BTreeMap<_, _>>();
        let orphans = observations;
        // Revoke leases for stale targets before runtime mutations can replace their workloads.
        self.retain_ready_serving_targets(&site, &current, &deployment_observations, preparations)
            .await?;
        let mut pending = false;
        let mut ready_targets = Vec::new();
        for desired in &site.model_deployments {
            let _transition = self
                .preparations
                .entry(&desired.model_deployment_id)
                .transition()
                .await;
            let operation = self.operation.lock().await;
            let declaration_is_current = self
                .desired
                .model_deployments()
                .map_err(desired_error)?
                .into_iter()
                .find(|candidate| candidate.model_deployment_id == desired.model_deployment_id)
                .as_ref()
                == Some(desired);
            drop(operation);
            if !declaration_is_current {
                pending = true;
                continue;
            }
            let _permit = self.authorize_deployment_action(desired)?;
            let observation = deployment_observations.remove(&desired.model_deployment_id);
            if is_absent(desired) {
                pending |= matches!(
                    self.reconcile_absent(&site, desired, observation).await?,
                    ReconcileOutcome::Pending
                );
                continue;
            }
            if matches!(
                super::state::desired_state(desired)?,
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused
            ) {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
                pending |= matches!(
                    self.reconcile_paused(&site, desired, observation).await?,
                    ReconcileOutcome::Pending
                );
                continue;
            }
            if desired.lifecycle_recovery_required {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
                if let Some(current_record) = current.get(&desired.model_deployment_id)
                    && current_record.recovery_barrier.is_none()
                {
                    let mut blocked = current_record.clone();
                    blocked.reconciled_lifecycle_generation = Some(desired.lifecycle_generation);
                    blocked.recovery_barrier = Some(
                        dotcore_proto::dotcore::model::v1::ModelLifecycleRecoveryBarrier {
                            lifecycle_generation: desired.lifecycle_generation,
                            reason: "legacy-activation-uncertain".to_owned(),
                        },
                    );
                    self.state
                        .upsert(blocked, Some(current_record.record_revision))
                        .map_err(current_error)?;
                }
                continue;
            }
            let Some(current_record) = current.get(&desired.model_deployment_id) else {
                pending = true;
                continue;
            };
            if let Some(rebound) =
                super::state::record_for_declared_generation(current_record, desired)
            {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
                let failed = rebound
                    .state
                    .as_ref()
                    .is_some_and(|state| state.status == ModelDeploymentStatus::Failed as i32);
                self.state
                    .upsert(rebound, Some(current_record.record_revision))
                    .map_err(current_error)?;
                warn!(
                    event_kind = "model_lifecycle_generation_rebound",
                    deployment_id = %desired.model_deployment_id,
                    retained_generation = ?current_record.reconciled_lifecycle_generation,
                    declared_generation = desired.lifecycle_generation,
                    failed,
                    "retained lifecycle progress belongs to another generation and was rebound to the declared one",
                );
                pending |= !failed;
                continue;
            }
            let failed = current_record.state.as_ref().is_some_and(|state| {
                ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Failed)
            });
            let resumes_failed =
                failed && super::state::resumes_after_unobserved_pause(current_record, desired);
            if failed && !resumes_failed {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
                continue;
            }
            if resumes_failed
                || current_record.state.as_ref().is_some_and(|state| {
                    matches!(
                        ModelDeploymentStatus::try_from(state.status),
                        Ok(ModelDeploymentStatus::Paused | ModelDeploymentStatus::Pausing)
                    )
                })
            {
                self.state
                    .upsert(
                        CurrentModelDeployment {
                            model_deployment_id: desired.model_deployment_id.clone(),
                            reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                            record_revision: 0,
                            recovery_barrier: None,
                            state: Some(resuming_state(desired.lifecycle_generation)),
                        },
                        Some(current_record.record_revision),
                    )
                    .map_err(current_error)?;
                info!(
                    event_kind = "model_resume_intent_recovered",
                    deployment_id = %desired.model_deployment_id,
                    "persisted Resume activation before touching the retained workload",
                );
                pending = true;
                continue;
            }
            let target = selected_target(&site, desired)?;
            let resolved = resolved_launch(desired)?;
            // Checked before artifact selection, since a mismatch is never prepared.
            if let Some(mismatch) = self.target_capability_mismatch(target, &resolved).await? {
                self.record_capability_mismatch(desired, &target.execution_target_id, &mismatch)?;
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
                error!(
                    event_kind = "model_target_capability_mismatch",
                    deployment_id = %desired.model_deployment_id,
                    execution_target_id = %target.execution_target_id,
                    reason = %diagnostic_chain(&mismatch),
                    "execution target cannot run the deployment; recorded Failed and stopped retrying",
                );
                continue;
            }
            let Some((artifacts, artifacts_replaced)) = self.select_artifacts(
                target,
                &resolved,
                preparations,
                &desired.model_deployment_id,
            )?
            else {
                pending = true;
                continue;
            };
            let launch = workload_launch(desired, target, &artifacts)?;
            let activation_state = current_record.state.as_ref();
            let startup_pending = activation_state.is_some_and(startup_pending);
            let previously_realized =
                previously_realized_deployments.contains(&desired.model_deployment_id);
            debug!(
                event_kind = "model_reconciliation_decision",
                deployment_id = %desired.model_deployment_id,
                reason = "desired_present",
                current_state = ?current_record.state.as_ref().and_then(|state| {
                    ModelDeploymentStatus::try_from(state.status).ok()
                }),
                observation_present = observation.is_some(),
                artifacts_replaced,
                "model reconciliation selected runtime workload action",
            );
            match self
                .transition_workload(
                    desired,
                    &launch,
                    observation,
                    artifacts_replaced,
                    startup_pending,
                    previously_realized,
                    activation_state,
                )
                .await
            {
                Ok(Some(observation)) => {
                    let result = self
                        .record_workload_observation(&site, desired, target, &launch, observation)
                        .await?;
                    pending |= result.pending;
                    if let Some(ready_target) = result.ready_target {
                        ready_targets.push((
                            ready_target,
                            result.ready_transition,
                            result.operation,
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) if is_definitive_realization_failure(&error) => {
                    self.record_realization_failure(
                        desired,
                        &error,
                        &protected_startup_values(&launch.spec.environment),
                    )
                    .await?;
                    self.serving_targets_publisher
                        .withdraw_deployment(&ModelDeploymentId(
                            desired.model_deployment_id.clone(),
                        ));
                    error!(
                        event_kind = "model_realization_failed",
                        deployment_id = %desired.model_deployment_id,
                        error_kind = "definitive",
                        reason = %diagnostic_chain(&error),
                        "model workload realization failed definitively; recorded Failed and stopped retrying",
                    );
                    continue;
                }
                Err(error) => {
                    debug!(
                        event_kind = "model_realization_retry",
                        deployment_id = %desired.model_deployment_id,
                        error_kind = "transient",
                        reason = %diagnostic_chain(&error),
                        "model workload realization failed transiently; keeping in-progress and retrying",
                    );
                    pending = true;
                    continue;
                }
            }
        }
        pending |= self.remove_orphans(orphans).await?;
        let mut ready_targets = ready_targets
            .into_iter()
            .map(|(target, ready_transition, operation)| {
                let deployment_id = target.deployment.model_deployment_id.clone();
                let execution_target_id = target.target.execution_target_id.clone();
                let workload_id = target.workload.workload_id.0.clone();
                serving_targets([target]).map(|targets| {
                    (
                        deployment_id,
                        (
                            targets,
                            execution_target_id,
                            workload_id,
                            ready_transition,
                            operation,
                        ),
                    )
                })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        for desired in &site.model_deployments {
            let ready_target = ready_targets.remove(&desired.model_deployment_id);
            let (target, execution_target_id, workload_id, ready_transition, operation) =
                match ready_target {
                    Some((
                        target,
                        execution_target_id,
                        workload_id,
                        ready_transition,
                        operation,
                    )) => (
                        Some(target),
                        Some(execution_target_id),
                        Some(workload_id),
                        ready_transition,
                        Some(operation),
                    ),
                    None => (None, None, None, false, None),
                };
            let published = self
                .publish_ready_serving_target(desired, target, operation.as_ref())
                .await?;
            if ready_transition && published {
                let (Some(execution_target_id), Some(workload_id)) =
                    (execution_target_id, workload_id)
                else {
                    unreachable!("ready target metadata");
                };
                info!(
                    event_kind = "model_serving_target_ready",
                    deployment_id = %desired.model_deployment_id,
                    execution_target_id = %execution_target_id,
                    workload_id = %workload_id,
                    outcome = "ready",
                    "model serving target became ready",
                );
            }
        }
        Ok(if pending {
            ReconcileOutcome::Pending
        } else {
            ReconcileOutcome::Settled
        })
    }

    /// Records a terminal `Failed` state with an actionable reason after a definitive runtime
    /// realization failure, so the deployment stops retrying and stays removable. The error text
    /// may quote the launch, so its `protected` values are redacted before anything is recorded.
    async fn record_realization_failure(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        error: &ModelLifecycleError,
        protected: &crate::management_logs::ProtectedLogValues,
    ) -> Result<(), ModelLifecycleError> {
        let reason = protected.redact(&realization_failure_reason(error));
        let failure = ModelDeploymentFailure {
            code: "runtime_realization_failed".to_owned(),
            message: reason.clone(),
            diagnostics: vec![reason.clone(), protected.redact(&error.to_string())],
        };
        self.record_failure(desired, &reason, failure)
    }

    /// Records a terminal `Failed` state for a deployment its execution target cannot run, so it
    /// stops retrying until a resume re-evaluates it. A workload it already runs is left running,
    /// unserved, until the deployment is paused or torn down.
    fn record_capability_mismatch(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        execution_target_id: &str,
        mismatch: &RetainedTargetCapabilityError,
    ) -> Result<(), ModelLifecycleError> {
        let failure = ModelDeploymentFailure {
            code: "target_capability_mismatch".to_owned(),
            message: format!(
                "Execution target {execution_target_id:?} cannot run this deployment: {mismatch}; pause and resume the deployment once the target supports it, or tear it down"
            ),
            diagnostics: Vec::new(),
        };
        self.record_failure(
            desired,
            "Execution target cannot run the deployment",
            failure,
        )
    }

    /// Records `failure` as the deployment's terminal `Failed` state for its declared lifecycle
    /// generation.
    fn record_failure(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        status_message: &str,
        failure: ModelDeploymentFailure,
    ) -> Result<(), ModelLifecycleError> {
        let snapshot = self.state.snapshot().map_err(current_error)?;
        let existing = snapshot
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id);
        let state = failed_state(
            existing.as_ref().and_then(|record| record.state.as_ref()),
            status_message,
            failure,
        );
        self.state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: desired.model_deployment_id.clone(),
                    reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(state),
                },
                existing.map(|record| record.record_revision),
            )
            .map_err(current_error)?;
        Ok(())
    }

    /// Stops and removes owned workloads whose deployment was absent from this pass's snapshot.
    /// Absence is rechecked against current intent before each runtime action, and each action is
    /// admitted only while no accepted deployment owns the workload. Returns whether any orphan
    /// was left for a later pass because its deployment became declared.
    async fn remove_orphans(
        &self,
        orphans: BTreeMap<String, TargetObservation>,
    ) -> Result<bool, ModelLifecycleError> {
        let mut pending = false;
        for TargetObservation {
            execution_target_id,
            observation,
        } in orphans.into_values()
        {
            let Some(workload) = observation_identity(&observation) else {
                continue;
            };
            if !matches!(
                observation,
                WorkloadObservation::Stopped { .. } | WorkloadObservation::Absent(_)
            ) {
                if self.orphan_owner_declared(workload)? {
                    pending = true;
                    continue;
                }
                self.runtimes
                    .stop_orphan_workload(&execution_target_id, workload)
                    .await?;
            }
            if !matches!(observation, WorkloadObservation::Absent(_)) {
                if self.orphan_owner_declared(workload)? {
                    pending = true;
                    continue;
                }
                self.runtimes
                    .remove_orphan_workload(&execution_target_id, workload)
                    .await?;
            }
        }
        Ok(pending)
    }

    fn orphan_owner_declared(
        &self,
        workload: &dotcore_runtime::ObservedWorkload,
    ) -> Result<bool, ModelLifecycleError> {
        let declared = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .iter()
            .any(|deployment| deployment.model_deployment_id == workload.correlation.owner_id);
        if declared {
            info!(
                event_kind = "model_orphan_cleanup_skipped",
                deployment_id = %workload.correlation.owner_id,
                workload_id = %workload.workload_id.0,
                "owned workload became declared during reconciliation; orphan cleanup skipped",
            );
        }
        Ok(declared)
    }

    async fn retain_ready_serving_targets(
        &self,
        site: &dotcore_proto::dotcore::site::v1::Site,
        current: &BTreeMap<String, dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
        observations: &BTreeMap<String, TargetObservation>,
        preparations: &BTreeMap<String, PreparedDeployment>,
    ) -> Result<(), ModelLifecycleError> {
        for desired in &site.model_deployments {
            let mut retained_target = None;
            if is_absent(desired)
                || matches!(
                    super::state::desired_state(desired)?,
                    dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused
                )
                || !current.contains_key(&desired.model_deployment_id)
                || current
                    .get(&desired.model_deployment_id)
                    .and_then(|record| record.state.as_ref())
                    .is_some_and(|state| {
                        ModelDeploymentStatus::try_from(state.status)
                            == Ok(ModelDeploymentStatus::Failed)
                    })
            {
                self.retain_ready_serving_target(desired, None, None)
                    .await?;
                continue;
            }
            if let Some(TargetObservation {
                observation: WorkloadObservation::Running(workload),
                ..
            }) = observations.get(&desired.model_deployment_id)
            {
                let selected_target = selected_target(site, desired)?;
                let resolved = resolved_launch(desired)?;
                if let Some((artifacts, _)) = self.select_artifacts(
                    selected_target,
                    &resolved,
                    preparations,
                    &desired.model_deployment_id,
                )? {
                    let expected_generation =
                        workload_launch(desired, selected_target, &artifacts)?
                            .correlation
                            .reconcile_generation;
                    if workload.correlation.reconcile_generation == expected_generation {
                        let (status, binding, operation, attempt_binding) = self
                            .fenced_status_from_observation(
                                desired,
                                &WorkloadObservation::Running(workload.clone()),
                                false,
                            )
                            .await?;
                        if status != ModelDeploymentStatus::Ready {
                            continue;
                        }
                        retained_target = Some((
                            serving_targets([ReadyServingTarget {
                                deployment: desired,
                                target: selected_target,
                                workload: workload.clone(),
                                binding: binding.ok_or_else(|| {
                                    configuration("READY workload has no resolved engine binding")
                                })?,
                            }])?,
                            operation,
                            attempt_binding,
                        ));
                    }
                }
            }
            let (retained_target, operation, attempt_binding) = match retained_target {
                Some((target, operation, attempt_binding)) => {
                    (Some(target), Some(operation), attempt_binding)
                }
                None => (None, None, None),
            };
            if let Err(error) = self
                .retain_ready_serving_target(desired, retained_target, operation.as_ref())
                .await
            {
                if let Some(binding) = attempt_binding {
                    self.engine_integrations.retire_inactive_binding(&binding);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    async fn retain_ready_serving_target(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        target: Option<Vec<super::serving_targets::LocalServingTarget>>,
        operation: Option<&LifecycleOperationToken>,
    ) -> Result<(), ModelLifecycleError> {
        self.update_serving_target(desired, target, operation, ServingTargetUpdate::Retain)
            .await
            .map(|_| ())
    }

    async fn publish_ready_serving_target(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        target: Option<Vec<super::serving_targets::LocalServingTarget>>,
        operation: Option<&LifecycleOperationToken>,
    ) -> Result<bool, ModelLifecycleError> {
        self.update_serving_target(desired, target, operation, ServingTargetUpdate::Upsert)
            .await
    }

    async fn update_serving_target(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        target: Option<Vec<super::serving_targets::LocalServingTarget>>,
        operation: Option<&LifecycleOperationToken>,
        update: ServingTargetUpdate,
    ) -> Result<bool, ModelLifecycleError> {
        let deployment_id = ModelDeploymentId(desired.model_deployment_id.clone());
        let entry = self.preparations.entry(&desired.model_deployment_id);
        let _transition = entry.transition().await;
        let _operation = self.operation.lock().await;
        let current = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .find(|candidate| candidate.model_deployment_id == desired.model_deployment_id);
        let can_publish = current.as_ref() == Some(desired)
            && current.as_ref().is_some_and(|candidate| {
                super::state::desired_state(candidate).is_ok_and(|state| {
                    state == dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present
                })
            });
        let upserted = if can_publish && target.is_some() {
            let operation = operation.ok_or_else(|| {
                configuration("ready serving target has no lifecycle operation identity")
            })?;
            self.commit_operation(operation, || match (target, update) {
                (Some(target), ServingTargetUpdate::Retain) => {
                    self.serving_targets_publisher
                        .retain_deployment(&deployment_id, &target);
                    false
                }
                (Some(target), ServingTargetUpdate::Upsert) => {
                    let previous = self.serving_targets_source.snapshot();
                    self.serving_targets_publisher
                        .upsert_deployment(&deployment_id, target);
                    let changed = self.serving_targets_source.snapshot() != previous;
                    if changed {
                        info!(event_kind = "model_serving_target_published", deployment_id = %deployment_id.0, outcome = "published", "model serving target published");
                    } else {
                        debug!(event_kind = "model_serving_target_published", deployment_id = %deployment_id.0, outcome = "unchanged", "model serving target publication unchanged");
                    }
                    changed
                }
                (None, _) => unreachable!("ready target checked above"),
            })?
        } else {
            self.serving_targets_publisher
                .withdraw_deployment(&deployment_id);
            debug!(event_kind = "model_serving_target_withdrawn", deployment_id = %deployment_id.0, reason = "not_ready_or_stale", "model serving target withdrawn");
            false
        };
        Ok(upserted)
    }

    async fn fenced_status_from_observation(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        observation: &WorkloadObservation,
        transition_held: bool,
    ) -> Result<
        (
            ModelDeploymentStatus,
            Option<crate::inference::EngineBinding>,
            LifecycleOperationToken,
            Option<crate::inference::EngineBinding>,
        ),
        ModelLifecycleError,
    > {
        let reconcile_generation = observation_identity(observation)
            .map_or(desired.lifecycle_generation, |workload| {
                workload.correlation.reconcile_generation
            });
        let entry = self.preparations.entry(&desired.model_deployment_id);
        let registration = {
            let _transition = if transition_held {
                None
            } else {
                Some(entry.transition().await)
            };
            let _operation = self.operation.lock().await;
            let token = self.capture_operation(
                desired,
                LifecycleOperationAttempt::Runtime {
                    reconcile_generation,
                },
            )?;
            let registered = self.commit_operation(&token, || {
                register_observation_status(&self.engine_integrations, desired, observation)
            })??;
            (token, registered)
        };
        let (token, registered) = registration;
        let attempt_binding = match &registered {
            RegisteredObservationStatus::Bound { binding, .. } => Some(binding.clone()),
            RegisteredObservationStatus::Immediate(_) => None,
        };
        let result = resolve_observation_status(registered).await?;
        let committed = {
            let _transition = if transition_held {
                None
            } else {
                Some(entry.transition().await)
            };
            let _operation = self.operation.lock().await;
            self.commit_operation(&token, || ())
        };
        if let Err(error) = committed {
            if let Some(binding) = &attempt_binding {
                self.engine_integrations.retire_inactive_binding(binding);
            }
            return Err(error);
        }
        Ok((result.0, result.1, token, attempt_binding))
    }

    /// Revalidates a retained launch against its target's current capabilities and returns the
    /// mismatch when the target cannot run it. A missing or stale observation is an error of the
    /// whole pass, since it holds for every deployment until the next observation.
    async fn target_capability_mismatch(
        &self,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        resolved: &crate::model::resolved::ResolvedModelLaunch,
    ) -> Result<Option<RetainedTargetCapabilityError>, ModelLifecycleError> {
        let inventory = self
            .target_capabilities
            .observe()
            .await
            .map_err(ModelLifecycleError::configuration_source)?;
        match crate::model::selection::validate_retained_target_capabilities(
            target, &inventory, resolved,
        ) {
            Ok(()) => Ok(None),
            Err(error) if error.is_observation_gap() => {
                Err(ModelLifecycleError::configuration_source(error))
            }
            Err(error) => Ok(Some(error)),
        }
    }

    fn select_artifacts(
        &self,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        resolved: &crate::model::resolved::ResolvedModelLaunch,
        preparations: &BTreeMap<String, PreparedDeployment>,
        deployment_id: &str,
    ) -> Result<Option<(PreparedModelArtifacts, bool)>, ModelLifecycleError> {
        let requirements = self
            .runtimes
            .materialization_requirements(&target.execution_target_id)?;
        if requirements.host_mounts == WorkloadAssetMaterialization::Physical {
            let Some(PreparedDeployment { prepared }) = preparations.get(deployment_id) else {
                return Ok(None);
            };
            Ok(Some((prepared.artifacts.clone(), prepared.replaced)))
        } else {
            Ok(Some((
                self.artifacts.expected_model_paths(&resolved.artifacts)?,
                false,
            )))
        }
    }

    async fn reconcile_paused(
        &self,
        site: &dotcore_proto::dotcore::site::v1::Site,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        observation: Option<TargetObservation>,
    ) -> Result<ReconcileOutcome, ModelLifecycleError> {
        let reconcile_generation = observation
            .as_ref()
            .and_then(|target| observation_identity(&target.observation))
            .map_or(desired.lifecycle_generation, |workload| {
                workload.correlation.reconcile_generation
            });
        let operation = self.capture_operation(
            desired,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation,
            },
        )?;
        match paused_observation(site, &self.artifacts, desired, observation)? {
            PausedObservation::Absent => {
                self.commit_operation(&operation, || {
                    if let Some(attribution) = &self.accelerator_attribution {
                        attribution.forget(&desired.model_deployment_id);
                    }
                    self.record_paused_state(desired, ModelDeploymentStatus::Unavailable)
                })??;
                Ok(ReconcileOutcome::Settled)
            }
            PausedObservation::Exact(TargetObservation {
                execution_target_id,
                observation:
                    observation @ (WorkloadObservation::Starting(_) | WorkloadObservation::Running(_)),
            })
            | PausedObservation::Exact(TargetObservation {
                execution_target_id,
                observation: observation @ WorkloadObservation::Unavailable(_),
            }) => {
                let workload = observation_identity(&observation)
                    .expect("conclusive paused observation has an identity");
                self.runtimes
                    .stop_workload_admitted(&operation, &execution_target_id, &workload.workload_id)
                    .await?;
                self.commit_operation(&operation, || {
                    self.record_paused_state(desired, ModelDeploymentStatus::Pausing)
                })??;
                Ok(ReconcileOutcome::Pending)
            }
            PausedObservation::Exact(TargetObservation {
                observation: WorkloadObservation::Stopping(_),
                ..
            }) => {
                self.commit_operation(&operation, || {
                    self.record_paused_state(desired, ModelDeploymentStatus::Pausing)
                })??;
                Ok(ReconcileOutcome::Pending)
            }
            PausedObservation::Exact(TargetObservation {
                observation: WorkloadObservation::Stopped { .. },
                ..
            }) => {
                let retired = self.commit_operation(&operation, || {
                    if let Some(attribution) = &self.accelerator_attribution {
                        attribution.forget(&desired.model_deployment_id);
                    }
                    let retired = self
                        .engine_integrations
                        .retire_inactive_deployment(&desired.model_deployment_id);
                    self.record_paused_state(desired, ModelDeploymentStatus::Paused)
                        .map(|()| retired)
                })??;
                debug!(
                    event_kind = "model_engine_bindings_retired",
                    deployment_id = %desired.model_deployment_id,
                    binding_count = retired,
                    reason = "workload_stopped",
                    "inactive model engine bindings retired"
                );
                Ok(ReconcileOutcome::Settled)
            }
            PausedObservation::Exact(TargetObservation {
                observation: WorkloadObservation::Absent(_),
                ..
            }) => {
                self.commit_operation(&operation, || {
                    if let Some(attribution) = &self.accelerator_attribution {
                        attribution.forget(&desired.model_deployment_id);
                    }
                    self.record_paused_state(desired, ModelDeploymentStatus::Unavailable)
                })??;
                Ok(ReconcileOutcome::Settled)
            }
            PausedObservation::Exact(TargetObservation {
                observation: WorkloadObservation::Unknown { .. },
                ..
            }) => Err(configuration(
                "Runtime returned an inconclusive paused observation",
            )),
            PausedObservation::Stale(TargetObservation {
                execution_target_id,
                observation:
                    observation @ (WorkloadObservation::Starting(_) | WorkloadObservation::Running(_)),
            })
            | PausedObservation::Stale(TargetObservation {
                execution_target_id,
                observation: observation @ WorkloadObservation::Unavailable(_),
            }) => {
                let workload = observation_identity(&observation)
                    .expect("conclusive stale paused observation has an identity");
                self.runtimes
                    .stop_workload_admitted(&operation, &execution_target_id, &workload.workload_id)
                    .await?;
                self.commit_operation(&operation, || {
                    self.record_paused_state(desired, ModelDeploymentStatus::Unavailable)
                })??;
                Ok(ReconcileOutcome::Settled)
            }
            PausedObservation::Stale(_) => {
                self.commit_operation(&operation, || {
                    self.record_paused_state(desired, ModelDeploymentStatus::Unavailable)
                })??;
                Ok(ReconcileOutcome::Settled)
            }
        }
    }

    fn record_paused_state(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        lifecycle: ModelDeploymentStatus,
    ) -> Result<(), ModelLifecycleError> {
        let snapshot = self.state.snapshot().map_err(current_error)?;
        let existing = snapshot
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id);
        if reconciled_generation(existing.as_ref(), desired)
            && existing
                .as_ref()
                .and_then(|record| record.state.as_ref())
                .is_some_and(|state| state.status == lifecycle as i32)
        {
            return Ok(());
        }
        let previous = existing.as_ref().and_then(|record| record.state.as_ref());
        let state = match lifecycle {
            ModelDeploymentStatus::Pausing => pausing_state(previous),
            ModelDeploymentStatus::Paused => paused_state(previous),
            ModelDeploymentStatus::Unavailable => unavailable_state(previous),
            _ => unreachable!("paused reconciliation only records paused lifecycle states"),
        };
        self.state
            .upsert(
                dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                    model_deployment_id: desired.model_deployment_id.clone(),
                    reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(state),
                },
                existing.map(|record| record.record_revision),
            )
            .map_err(current_error)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn transition_workload(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        launch: &dotcore_runtime::WorkloadLaunch,
        observation: Option<TargetObservation>,
        artifacts_replaced: bool,
        startup_pending: bool,
        previously_realized: bool,
        activation_state: Option<&dotcore_proto::dotcore::model::v1::ModelDeploymentState>,
    ) -> Result<Option<WorkloadObservation>, ModelLifecycleError> {
        let generation = launch.correlation.reconcile_generation;
        let operation = self.capture_operation(
            desired,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation: generation,
            },
        )?;
        let observation_missing = observation.is_none();
        let observation = match observation {
            Some(target_observation)
                if artifacts_replaced
                    || observation_identity(&target_observation.observation).is_some_and(
                        |identity| identity.correlation.reconcile_generation != generation,
                    ) =>
            {
                replace_stale_workload_admitted(&self.runtimes, &operation, target_observation)
                    .await?;
                self.engine_integrations
                    .retire_inactive_deployment(&desired.model_deployment_id);
                None
            }
            observation => observation,
        };
        let running = matches!(
            observation,
            Some(TargetObservation {
                observation: WorkloadObservation::Running(_) | WorkloadObservation::Starting(_),
                ..
            })
        );
        if !running {
            // A workload this pass may create or start is announced once a pass sees it
            // running, even when that action fails after acting.
            self.workload_lifecycle.forget(
                &desired.model_deployment_id,
                super::logs::MODEL_LOG_SOURCE_NAME,
            );
        }
        match observation {
            None => {
                if observation_missing && previously_realized {
                    self.engine_integrations
                        .retire_inactive_deployment(&desired.model_deployment_id);
                }
                let created = self
                    .runtimes
                    .create_workload_admitted(
                        &operation,
                        &desired
                            .placement
                            .as_ref()
                            .ok_or_else(|| {
                                configuration("local model deployment has no exact placement")
                            })?
                            .execution_target_id,
                        launch,
                    )
                    .await?;
                self.announce_started_workload(desired);
                self.commit_operation(&operation, || ())?;
                Ok(Some(created))
            }
            Some(TargetObservation {
                observation: WorkloadObservation::Absent(_),
                ..
            }) => {
                if activation_state.is_some_and(|state| {
                    resume_phase(state) == ModelDeploymentActivationPhase::StartInFlight
                }) {
                    self.persist_resume_uncertain(desired)?;
                    warn!(
                        event_kind = "model_lifecycle_problem",
                        deployment_id = %desired.model_deployment_id,
                        error_kind = "resume_start_uncertain",
                        "retained workload disappeared before its Start outcome was confirmed",
                    );
                    return Ok(None);
                }
                if activation_state.is_some_and(|state| {
                    resume_phase(state) == ModelDeploymentActivationPhase::AwaitingStart
                }) {
                    self.persist_resume_activation(
                        desired,
                        ModelDeploymentActivationPhase::Unspecified,
                        generation,
                    )?;
                }
                self.engine_integrations
                    .retire_inactive_deployment(&desired.model_deployment_id);
                let created = self
                    .runtimes
                    .create_workload_admitted(
                        &operation,
                        &desired
                            .placement
                            .as_ref()
                            .ok_or_else(|| {
                                configuration("local model deployment has no exact placement")
                            })?
                            .execution_target_id,
                        launch,
                    )
                    .await?;
                self.announce_started_workload(desired);
                self.commit_operation(&operation, || ())?;
                Ok(Some(created))
            }
            Some(TargetObservation {
                observation:
                    WorkloadObservation::Stopped {
                        workload,
                        exit_status,
                    },
                ..
            }) if activation_state.is_some_and(|state| {
                resume_phase(state) == ModelDeploymentActivationPhase::StartInFlight
                    && resume_matches_lifecycle(state, desired.lifecycle_generation)
                    && resume_matches_generation(state, generation)
            }) =>
            {
                Ok(Some(WorkloadObservation::Stopped {
                    workload,
                    exit_status,
                }))
            }
            Some(TargetObservation {
                execution_target_id,
                observation: WorkloadObservation::Stopped { workload, .. },
            }) if activation_state.is_some_and(|state| {
                resume_phase(state) == ModelDeploymentActivationPhase::AwaitingStart
                    && resume_matches_lifecycle(state, desired.lifecycle_generation)
                    && state.activation.as_ref().is_some_and(|activation| {
                        activation.workload_generation.is_none()
                            || activation.workload_generation == Some(generation)
                    })
            }) =>
            {
                // Fence the side effect before runtime Start; recovery never retries an unknown call.
                self.commit_operation(&operation, || {
                    self.persist_resume_activation(
                        desired,
                        ModelDeploymentActivationPhase::StartInFlight,
                        generation,
                    )
                })??;
                let started = self
                    .runtimes
                    .start_workload_admitted(
                        &operation,
                        &execution_target_id,
                        &workload.workload_id,
                    )
                    .await?;
                self.announce_started_workload(desired);
                self.commit_operation(&operation, || {
                    self.persist_resume_activation(
                        desired,
                        ModelDeploymentActivationPhase::StartConfirmed,
                        generation,
                    )
                })??;
                Ok(Some(started))
            }
            Some(TargetObservation {
                observation:
                    WorkloadObservation::Stopped {
                        workload,
                        exit_status,
                    },
                ..
            }) if startup_pending => Ok(Some(WorkloadObservation::Stopped {
                workload,
                exit_status,
            })),
            Some(TargetObservation {
                execution_target_id,
                observation: WorkloadObservation::Stopped { workload, .. },
            }) => {
                self.engine_integrations
                    .retire_inactive_deployment(&desired.model_deployment_id);
                let started = self
                    .runtimes
                    .start_workload_admitted(
                        &operation,
                        &execution_target_id,
                        &workload.workload_id,
                    )
                    .await?;
                self.announce_started_workload(desired);
                self.commit_operation(&operation, || ())?;
                Ok(Some(started))
            }
            Some(TargetObservation { observation, .. }) => {
                if running {
                    self.workload_lifecycle.running(
                        &desired.model_deployment_id,
                        super::logs::MODEL_LOG_SOURCE_NAME,
                    );
                }
                Ok(Some(observation))
            }
        }
    }

    /// Tells the node's log follows of the deployment that its engine workload was created or
    /// started again, so they follow the new output.
    fn announce_started_workload(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    ) {
        self.workload_lifecycle.started(
            &desired.model_deployment_id,
            super::logs::MODEL_LOG_SOURCE_NAME,
        );
    }

    fn persist_resume_activation(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        phase: ModelDeploymentActivationPhase,
        generation: u64,
    ) -> Result<(), ModelLifecycleError> {
        let record = self
            .state
            .snapshot()
            .map_err(current_error)?
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id)
            .ok_or_else(|| configuration("Resume has no retained deployment state"))?;
        let previous = record
            .state
            .as_ref()
            .ok_or_else(|| configuration("Resume has no retained operational state"))?;
        if !resume_matches_lifecycle(previous, desired.lifecycle_generation) {
            return Err(configuration(
                "Resume activation does not match the desired lifecycle generation",
            ));
        }
        self.state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: desired.model_deployment_id.clone(),
                    reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(resume_start_state(previous, phase, generation)),
                },
                Some(record.record_revision),
            )
            .map_err(current_error)?;
        Ok(())
    }

    fn persist_resume_uncertain(
        &self,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    ) -> Result<(), ModelLifecycleError> {
        let record = self
            .state
            .snapshot()
            .map_err(current_error)?
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id)
            .ok_or_else(|| configuration("Resume has no retained deployment state"))?;
        let previous = record
            .state
            .as_ref()
            .ok_or_else(|| configuration("Resume has no retained operational state"))?;
        if !resume_matches_lifecycle(previous, desired.lifecycle_generation) {
            return Err(configuration(
                "Resume activation does not match the desired lifecycle generation",
            ));
        }
        self.state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: desired.model_deployment_id.clone(),
                    reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(resume_start_uncertain_state(previous)),
                },
                Some(record.record_revision),
            )
            .map_err(current_error)?;
        Ok(())
    }

    async fn record_workload_observation<'a>(
        &self,
        site: &dotcore_proto::dotcore::site::v1::Site,
        desired: &'a dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        target: &'a dotcore_proto::dotcore::site::v1::ExecutionTarget,
        launch: &dotcore_runtime::WorkloadLaunch,
        observation: WorkloadObservation,
    ) -> Result<RecordedWorkloadObservation<'a>, ModelLifecycleError> {
        validate_action_observation(
            site,
            &self.runtimes,
            &self.artifacts,
            desired,
            &desired
                .placement
                .as_ref()
                .ok_or_else(|| configuration("local model deployment has no exact placement"))?
                .execution_target_id,
            None,
            &observation,
        )?;
        let (mut lifecycle, binding, operation, attempt_binding) = self
            .fenced_status_from_observation(desired, &observation, true)
            .await?;
        self.commit_status_operation(&operation, attempt_binding.as_ref(), || {
            if let Some(attribution) = &self.accelerator_attribution {
                attribution.observe(&desired.model_deployment_id, Some(target), &observation);
            }
            if matches!(
                observation,
                WorkloadObservation::Starting(_) | WorkloadObservation::Running(_)
            ) {
                let record = self
                    .state
                    .snapshot()
                    .map_err(current_error)?
                    .model_deployments
                    .into_iter()
                    .find(|record| record.model_deployment_id == desired.model_deployment_id);
                if record
                    .as_ref()
                    .and_then(|record| record.state.as_ref())
                    .is_some_and(|state| {
                        resume_phase(state) == ModelDeploymentActivationPhase::StartInFlight
                            && resume_matches_lifecycle(state, desired.lifecycle_generation)
                            && resume_matches_generation(
                                state,
                                launch.correlation.reconcile_generation,
                            )
                    })
                {
                    self.persist_resume_activation(
                        desired,
                        ModelDeploymentActivationPhase::StartConfirmed,
                        launch.correlation.reconcile_generation,
                    )?;
                }
            }
            Ok::<(), ModelLifecycleError>(())
        })??;
        let previous_state = self
            .state
            .snapshot()
            .map_err(current_error)?
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == desired.model_deployment_id)
            .and_then(|record| record.state);
        let previous_lifecycle = previous_state
            .as_ref()
            .and_then(|state| ModelDeploymentStatus::try_from(state.status).ok());
        let resume_uncertain = matches!(observation, WorkloadObservation::Stopped { .. })
            && previous_state.as_ref().is_some_and(|state| {
                resume_phase(state) == ModelDeploymentActivationPhase::StartInFlight
                    && resume_matches_lifecycle(state, desired.lifecycle_generation)
                    && resume_matches_generation(state, launch.correlation.reconcile_generation)
            });
        if resume_uncertain {
            self.commit_status_operation(&operation, attempt_binding.as_ref(), || {
                self.persist_resume_uncertain(desired)
            })??;
            warn!(
                event_kind = "model_lifecycle_problem",
                deployment_id = %desired.model_deployment_id,
                error_kind = "resume_start_uncertain",
                "retained workload start outcome is uncertain",
            );
            lifecycle = ModelDeploymentStatus::Failed;
        }
        let startup_failed = if previous_state.as_ref().is_some_and(startup_pending) {
            if let WorkloadObservation::Stopped {
                workload,
                exit_status,
            } = &observation
            {
                let output = self
                    .capture_startup_output_tail(target, workload, launch)
                    .await;
                self.commit_status_operation(&operation, attempt_binding.as_ref(), || {
                    apply_startup_failure(
                        &self.state,
                        desired,
                        startup_failure_diagnostics(*exit_status, output),
                    )
                })??
            } else {
                false
            }
        } else {
            false
        };
        if startup_failed {
            lifecycle = ModelDeploymentStatus::Failed;
            warn!(
                event_kind = "model_lifecycle_problem",
                deployment_id = %desired.model_deployment_id,
                error_kind = "engine_startup_failed",
                "model runtime workload stopped during startup",
            );
        }
        if lifecycle != ModelDeploymentStatus::Ready
            && matches!(observation, WorkloadObservation::Running(_))
        {
            self.commit_status_operation(&operation, attempt_binding.as_ref(), || {
                self.serving_targets_publisher
                    .withdraw_deployment(&ModelDeploymentId(desired.model_deployment_id.clone()));
            })?;
        }
        if !startup_failed && !resume_uncertain {
            let execution_target = dotcore_runtime::ExecutionTarget {
                managed_node_id: dotcore_domain::SiteNodeId::parse(&target.site_node_id).map_err(
                    |error| {
                        ModelLifecycleError::Configuration(
                            anyhow::Error::new(error)
                                .context("execution target node identity is invalid"),
                        )
                    },
                )?,
                accelerator_device_ids: target.accelerator_device_ids.clone(),
            };
            self.commit_status_operation(&operation, attempt_binding.as_ref(), || {
                apply_observation(
                    &self.state,
                    desired,
                    &observation,
                    lifecycle,
                    Some(&execution_target),
                )
            })??;
        }
        if previous_lifecycle != Some(lifecycle) {
            info!(
                event_kind = "model_lifecycle_transition",
                deployment_id = %desired.model_deployment_id,
                previous_state = ?previous_lifecycle,
                next_state = ?lifecycle,
                "model deployment lifecycle transitioned",
            );
        } else {
            debug!(event_kind = "model_lifecycle_unchanged", deployment_id = %desired.model_deployment_id, state = ?lifecycle, "model deployment lifecycle unchanged");
        }
        let ready_transition =
            lifecycle == ModelDeploymentStatus::Ready && previous_lifecycle != Some(lifecycle);
        let ready_target = if lifecycle == ModelDeploymentStatus::Ready {
            let workload = observation_identity(&observation).ok_or_else(|| {
                configuration("Runtime returned no identity for a READY workload")
            })?;
            self.start_resource_telemetry(site, desired, target, workload, ready_transition);
            Some(ReadyServingTarget {
                deployment: desired,
                target,
                workload: workload.clone(),
                binding: binding.ok_or_else(|| {
                    configuration("READY workload has no resolved engine binding")
                })?,
            })
        } else {
            None
        };
        Ok(RecordedWorkloadObservation {
            pending: matches!(
                lifecycle,
                ModelDeploymentStatus::Loading | ModelDeploymentStatus::Unloading
            ),
            ready_transition,
            ready_target,
            operation,
        })
    }

    /// Starts resource telemetry for a ready Docker or native realization, or a simulated one with
    /// a simulated process; a realization first seen already ready, such as after a backend
    /// restart, records no startup record.
    fn start_resource_telemetry(
        &self,
        site: &dotcore_proto::dotcore::site::v1::Site,
        desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        workload: &dotcore_runtime::ObservedWorkload,
        first_ready: bool,
    ) {
        let Some(attribution) = &self.accelerator_attribution else {
            return;
        };
        attribution.ready(
            &desired.model_deployment_id,
            workload.correlation.reconcile_generation,
            || {
                use dotcore_proto::dotcore::site::v1::{
                    execution_target::RuntimeBinding, model_runtime_binding::Adapter,
                };

                let adapter = match &target.runtime_binding {
                    Some(RuntimeBinding::ModelRuntime(binding)) => binding.adapter.as_ref(),
                    _ => None,
                };
                let sampled = match adapter {
                    Some(Adapter::DockerRuntime(_)) => true,
                    Some(Adapter::SimulatedRuntime(_)) => workload.host_process.is_some(),
                    Some(Adapter::NativeRuntime(_)) => true,
                    None => false,
                };
                if !sampled {
                    return None;
                }
                let launch = super::launch::resolved_launch(desired).ok()?;
                Some(super::resource_telemetry::ReadyRealization {
                    desired: desired.clone(),
                    target: target.clone(),
                    generation: workload.correlation.reconcile_generation,
                    inputs: launch.telemetry_inputs(),
                    ready_at: self.clock.wall_now(),
                    first_ready,
                    interval: hardware_refresh_interval(site),
                    engine_facts: super::resource_telemetry::engine_facts_future(
                        Arc::clone(&self.engine_integrations),
                        self.runtimes.clone(),
                        launch.snapshot,
                        target.execution_target_id.clone(),
                        workload.workload_id.clone(),
                    ),
                    clock: Arc::clone(&self.clock),
                })
            },
        );
    }

    async fn capture_startup_output_tail(
        &self,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        workload: &dotcore_runtime::ObservedWorkload,
        launch: &dotcore_runtime::WorkloadLaunch,
    ) -> Option<String> {
        let source = dotcore_runtime::RuntimeLogSource {
            name: super::logs::MODEL_LOG_SOURCE_NAME.to_owned(),
            label: super::logs::MODEL_LOG_SOURCE_LABEL.to_owned(),
        };
        let history = dotcore_runtime::RuntimeLogQuery::new(
            std::num::NonZeroU16::new(16).expect("positive startup log limit"),
            None,
        )
        .expect("startup log limit is within runtime bounds");
        let query = dotcore_runtime::RuntimeWorkloadLogQuery::new(source, history);
        let protected = protected_startup_values(&launch.spec.environment);
        match real_time::bound(
            Duration::from_secs(2),
            self.runtimes.read_workload_logs(
                &target.execution_target_id,
                &workload.workload_id,
                &query,
            ),
        )
        .await
        {
            Ok(Ok(history)) => startup_output_tail(history, &protected),
            Ok(Err(_)) => {
                debug!(
                    event_kind = "model_startup_output_unavailable",
                    reason = "read_failed",
                    "startup output unavailable"
                );
                None
            }
            Err(_) => {
                debug!(
                    event_kind = "model_startup_output_unavailable",
                    reason = "timeout",
                    "startup output timed out"
                );
                None
            }
        }
    }
}

fn download_progress_kind(
    requirements: dotcore_runtime::WorkloadMaterializationRequirements,
) -> DownloadProgressKind {
    match requirements.host_mounts {
        WorkloadAssetMaterialization::Physical => DownloadProgressKind::Artifact,
        WorkloadAssetMaterialization::ReferenceOnly => DownloadProgressKind::RuntimeStartup,
    }
}

enum ModelPreparationError {
    Artifact(ArtifactCacheError),

    Runtime(crate::dispatch::model::RuntimeTargetError),
}

enum ModelPreparationResult {
    Physical {
        prepared: ModelArtifactPreparation,
        prepare_engine: bool,
    },

    ReferenceOnly,
}

#[derive(Clone, Copy)]
enum DownloadProgressKind {
    Artifact,

    RuntimeStartup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedPreparationProgress {
    progress: ModelArtifactProgress,
    detail: Option<String>,
}

impl ObservedPreparationProgress {
    fn artifact(progress: ModelArtifactProgress) -> Self {
        Self {
            progress,
            detail: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedArtifactStage {
    progress_before: Option<ObservedPreparationProgress>,
    stage: ModelArtifactStage,
}

#[derive(Default)]
struct ObservedPreparationActivity {
    progress: Option<ObservedPreparationProgress>,
    stages: VecDeque<ObservedArtifactStage>,
}

struct DownloadProgressPublication {
    ticket: PreparationTicket,
    kind: DownloadProgressKind,
    observed: Arc<Mutex<ObservedPreparationActivity>>,
    pending_stages: VecDeque<ObservedArtifactStage>,
    pending_trailing: Option<ObservedPreparationProgress>,
    latest: Option<ObservedPreparationProgress>,
    persisted: Option<ObservedPreparationProgress>,
    quiet_revalidation: bool,
    next_write_at: Option<Instant>,

    /// Least time between two stored progress updates.
    write_interval: Duration,
}

impl DownloadProgressPublication {
    fn new(
        ticket: PreparationTicket,
        kind: DownloadProgressKind,
        write_interval: Duration,
    ) -> Self {
        Self {
            ticket,
            kind,
            observed: Arc::new(Mutex::new(ObservedPreparationActivity::default())),
            pending_stages: VecDeque::new(),
            pending_trailing: None,
            latest: None,
            persisted: None,
            quiet_revalidation: false,
            next_write_at: None,
            write_interval,
        }
    }

    fn observer(&self, notification: Arc<Notify>) -> ModelArtifactProgressObserver {
        let observed = self.observed.clone();
        Arc::new(move |progress| {
            observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .progress = Some(ObservedPreparationProgress::artifact(progress));
            notification.notify_one();
        })
    }

    fn stage_observer(&self, notification: Arc<Notify>) -> ModelArtifactStageObserver {
        let observed = self.observed.clone();
        Arc::new(move |stage| {
            let mut observed = observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let progress_before = observed.progress.take();
            observed.stages.push_back(ObservedArtifactStage {
                progress_before,
                stage,
            });
            notification.notify_one();
        })
    }

    fn runtime_observer(&self, notification: Arc<Notify>) -> WorkloadStartupObserver {
        let observed = self.observed.clone();
        Arc::new(move |activity| {
            let Some(progress) = activity.progress else {
                return;
            };
            if progress.unit != WorkloadStartupProgressUnit::Bytes {
                return;
            }
            observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .progress = Some(ObservedPreparationProgress {
                progress: ModelArtifactProgress {
                    completed: progress.completed,
                    total: progress.total,
                },
                detail: activity.detail,
            });
            notification.notify_one();
        })
    }

    fn collect_observed(&mut self) {
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.pending_stages.append(&mut observed.stages);
        if let Some(progress) = observed.progress.take() {
            if self.pending_stages.is_empty() {
                self.latest = Some(progress);
            } else {
                self.pending_trailing = Some(progress);
            }
        }
    }

    fn next_due_write(&self, now: Instant) -> Option<Instant> {
        if !self.pending_stages.is_empty() {
            return Some(now);
        }
        (self.latest != self.persisted)
            .then_some(self.next_write_at)
            .flatten()
    }

    fn write_is_due(&self, now: Instant) -> bool {
        !self.pending_stages.is_empty()
            || (self.latest != self.persisted
                && self
                    .next_write_at
                    .is_none_or(|next_write_at| now >= next_write_at))
    }

    fn pending_write(
        &self,
        now: Instant,
        force: bool,
    ) -> Option<(PreparationTicket, ObservedPreparationProgress)> {
        let progress = self.latest.clone()?;
        if self.persisted.as_ref() == Some(&progress) {
            return None;
        }
        if !force
            && self
                .next_write_at
                .is_some_and(|next_write_at| now < next_write_at)
        {
            return None;
        }
        Some((self.ticket.clone(), progress))
    }

    fn mark_persisted(&mut self, progress: ObservedPreparationProgress, now: Instant) {
        self.persisted = Some(progress);
        self.next_write_at = Some(now + self.write_interval);
    }
}

fn collect_observed_download_progress(
    publications: &mut BTreeMap<String, DownloadProgressPublication>,
) {
    for publication in publications.values_mut() {
        publication.collect_observed();
    }
}

async fn wait_for_progress_write(clock: &dyn Clock, deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        clock.sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn cancel_active_preparations(entries: &[Arc<PreparationEntry>]) {
    for entry in entries {
        entry.cancel_active();
    }
}

struct PreparedDeployment {
    prepared: ModelArtifactPreparation,
}

struct EngineAcquisitionCommit<'a> {
    deployment_id: &'a str,
    ticket: &'a PreparationTicket,
    operation: &'a LifecycleOperationToken,
    prepared: ModelArtifactPreparation,
    announce: bool,
    result: Result<(), ArtifactPreparationError>,
}

#[derive(Clone, Copy)]
enum ServingTargetUpdate {
    Retain,
    Upsert,
}

enum PausedObservation {
    Absent,
    Exact(TargetObservation),
    Stale(TargetObservation),
}

fn paused_observation(
    site: &dotcore_proto::dotcore::site::v1::Site,
    artifacts: &ArtifactManager,
    desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    observation: Option<TargetObservation>,
) -> Result<PausedObservation, ModelLifecycleError> {
    let Some(observation) = observation else {
        return Ok(PausedObservation::Absent);
    };
    let identity = observation_identity(&observation.observation)
        .ok_or_else(|| configuration("Runtime returned an inconclusive paused observation"))?;
    if observation.execution_target_id
        != desired
            .placement
            .as_ref()
            .ok_or_else(|| configuration("local model deployment has no exact placement"))?
            .execution_target_id
        || identity.correlation.owner_id != desired.model_deployment_id
    {
        return Err(configuration(
            "Runtime returned ownership evidence that conflicts with the paused deployment",
        ));
    }
    let target = selected_target(site, desired)?;
    let resolved = resolved_launch(desired)?;
    let artifacts = artifacts.expected_model_paths(&resolved.artifacts)?;
    let launch = workload_launch(desired, target, &artifacts)?;
    if identity.correlation == launch.correlation {
        Ok(PausedObservation::Exact(observation))
    } else {
        Ok(PausedObservation::Stale(observation))
    }
}

pub(super) fn paused_initial_state(
    site: &dotcore_proto::dotcore::site::v1::Site,
    artifacts: &ArtifactManager,
    desired: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    observation: Option<TargetObservation>,
) -> Result<dotcore_proto::dotcore::model::v1::ModelDeploymentState, ModelLifecycleError> {
    match paused_observation(site, artifacts, desired, observation)? {
        PausedObservation::Absent | PausedObservation::Stale(_) => Ok(unavailable_state(None)),
        PausedObservation::Exact(TargetObservation {
            observation:
                WorkloadObservation::Starting(_)
                | WorkloadObservation::Running(_)
                | WorkloadObservation::Stopping(_)
                | WorkloadObservation::Unavailable(_),
            ..
        }) => Ok(pausing_state(None)),
        PausedObservation::Exact(TargetObservation {
            observation: WorkloadObservation::Stopped { .. },
            ..
        }) => Ok(paused_state(None)),
        PausedObservation::Exact(TargetObservation {
            observation: WorkloadObservation::Absent(_),
            ..
        }) => Ok(unavailable_state(None)),
        PausedObservation::Exact(TargetObservation {
            observation: WorkloadObservation::Unknown { .. },
            ..
        }) => Err(configuration(
            "Runtime returned an inconclusive paused observation",
        )),
    }
}

struct RecordedWorkloadObservation<'a> {
    pending: bool,
    ready_transition: bool,
    ready_target: Option<ReadyServingTarget<'a>>,
    operation: LifecycleOperationToken,
}

impl<D, S, R> super::ModelRealizer<D, S, R>
where
    D: super::ModelRealizationInput + 'static,
    S: super::CurrentStateRepository + 'static,
    R: super::ModelRealizationResults + 'static,
{
    /// Starts the observer that reconciles desired deployments with runtime observations.
    pub(crate) fn start_observer(&self) {
        let mut started = self
            .inner
            .observer_started
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if *started {
            return;
        }
        *started = true;
        drop(started);
        let inner = Arc::downgrade(&self.inner);
        let wakeup = Arc::clone(&self.inner.wakeup);
        let schedule = self.inner.timing.reconcile;
        let clock = Arc::clone(&self.inner.clock);
        let task_span = info_span!("model_observer", operation = "model_observer");
        let observer_span = task_span.clone();
        tokio::spawn(
            async move {
                let mut cadence = ReconcileCadence::new(schedule);
                let mut awaiting_view = false;
                loop {
                    let Some(inner) = inner.upgrade() else {
                        info!(
                            parent: &observer_span,
                            operation = "model_observer",
                            outcome = "stopped",
                            state = "dropped",
                            "model observer stopped",
                        );
                        return;
                    };
                    let result = inner.reconcile().await;
                    let awaits_view = result
                        .as_ref()
                        .is_err_and(ModelLifecycleError::awaits_desired_view);
                    if awaits_view != awaiting_view {
                        emit_desired_view_wait(&observer_span, awaits_view);
                        awaiting_view = awaits_view;
                    }
                    let state = match result {
                        Err(_) if awaits_view => ReconcileOutcome::Pending,
                        Ok(state) => {
                            debug!(
                                parent: &observer_span,
                                operation = "model_reconcile",
                                outcome = "success",
                                state = ?state,
                                "model reconciliation completed",
                            );
                            state
                        }
                        Err(error) => {
                            emit_observer_failure(&observer_span, &error);
                            ReconcileOutcome::Pending
                        }
                    };
                    let delay = cadence.next_delay(state);
                    drop(inner);
                    tokio::select! {
                        _ = wakeup.notified() => cadence.reset(),
                        () = clock.sleep(delay) => {},
                    }
                }
            }
            .instrument(task_span),
        );
    }
}

#[expect(
    private_bounds,
    reason = "the public selected controller alias retains private owner-specific repository ports"
)]
impl<D, S> InferenceController<D, S>
where
    D: super::DesiredDeploymentRepository + 'static,
    S: super::CurrentStateRepository + 'static,
{
    /// Starts the owner's upstream reconciler once; node-hosted deployments are reconciled by
    /// their hosting node.
    pub(crate) fn start_observer(&self) {
        let mut started = self
            .inner
            .observer_started
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if *started {
            return;
        }
        *started = true;
        drop(started);
        self.inner.upstream.start();
    }
}

/// Reports the observer entering or leaving the wait for a desired view from the site manager.
fn emit_desired_view_wait(span: &Span, waiting: bool) {
    if waiting {
        warn!(
            parent: span,
            operation = "model_reconcile",
            outcome = "awaiting_desired_view",
            "model reconciliation is waiting for a desired view from the site manager",
        );
    } else {
        info!(
            parent: span,
            operation = "model_reconcile",
            outcome = "desired_view_available",
            "model reconciliation resumed with a desired view from the site manager",
        );
    }
}

fn emit_observer_failure(span: &Span, error: &ModelLifecycleError) {
    error!(
        parent: span,
        operation = "model_reconcile",
        outcome = "failed",
        error_kind = match error.category() {
            super::ModelLifecycleErrorCategory::OutcomeUncertain => "outcome_uncertain",
            _ => "reconciliation",
        },
        "model reconciliation failed",
    );
}

/// Returns whether a workload-realization failure is a stable capability or contract mismatch that
/// will not clear without a new deployment mutation, so the deployment is recorded `Failed`
/// instead of retried.
fn is_definitive_realization_failure(error: &ModelLifecycleError) -> bool {
    let ModelLifecycleError::Runtime(RuntimeTargetError::Adapter { source, .. }) = error else {
        return false;
    };
    matches!(
        source,
        RuntimeBackendError::ExecutionPlatformUnavailable
            | RuntimeBackendError::TargetPlatformMismatch { .. }
            | RuntimeBackendError::ExecutionTargetMismatch { .. }
            | RuntimeBackendError::ImagePlatformMismatch { .. }
            | RuntimeBackendError::HostSharedIpcUnavailable
            | RuntimeBackendError::LogCapabilityUnavailable
            | RuntimeBackendError::UnsupportedExecutableArtifact { .. }
    )
}

fn realization_failure_reason(error: &ModelLifecycleError) -> String {
    if let ModelLifecycleError::Runtime(RuntimeTargetError::Adapter { source, .. }) = error {
        source.to_string()
    } else {
        error.to_string()
    }
}

/// A wait on the observer's wakeup from inside a pass. A wakeup the pass consumes is raised again
/// once the wait is dropped, so the observer runs another pass for the change instead of waiting
/// for the safety scan.
pub(super) struct PassWakeup<'a> {
    wakeup: &'a Notify,
    consumed: bool,
}

impl<'a> PassWakeup<'a> {
    pub(super) fn new(wakeup: &'a Notify) -> Self {
        Self {
            wakeup,
            consumed: false,
        }
    }

    /// Waits for the next wakeup and keeps it to raise again.
    pub(super) async fn notified(&mut self) {
        self.wakeup.notified().await;
        self.consumed = true;
    }
}

impl Drop for PassWakeup<'_> {
    fn drop(&mut self) {
        if self.consumed {
            self.wakeup.notify_one();
        }
    }
}

#[cfg(test)]
pub(super) async fn replace_stale_workload(
    runtimes: &RuntimeTargetMap,
    stale: TargetObservation,
) -> Result<Option<WorkloadObservation>, ModelLifecycleError> {
    let Some(workload) = observation_identity(&stale.observation) else {
        return Err(configuration(
            "Runtime returned an inconclusive stale observation",
        ));
    };
    if !matches!(
        stale.observation,
        WorkloadObservation::Stopped { .. } | WorkloadObservation::Absent(_)
    ) {
        runtimes
            .stop_workload(&stale.execution_target_id, &workload.workload_id)
            .await?;
    }
    if !matches!(stale.observation, WorkloadObservation::Absent(_)) {
        runtimes
            .remove_workload(&stale.execution_target_id, &workload.workload_id)
            .await?;
    }
    Ok(None)
}

async fn replace_stale_workload_admitted(
    runtimes: &RuntimeTargetMap,
    token: &LifecycleOperationToken,
    stale: TargetObservation,
) -> Result<Option<WorkloadObservation>, ModelLifecycleError> {
    let Some(workload) = observation_identity(&stale.observation) else {
        return Err(configuration(
            "Runtime returned an inconclusive stale observation",
        ));
    };
    if !matches!(
        stale.observation,
        WorkloadObservation::Stopped { .. } | WorkloadObservation::Absent(_)
    ) {
        runtimes
            .stop_workload_admitted(token, &stale.execution_target_id, &workload.workload_id)
            .await?;
        runtimes.commit_operation(token, || ())?;
    }
    if !matches!(stale.observation, WorkloadObservation::Absent(_)) {
        runtimes
            .remove_workload_admitted(token, &stale.execution_target_id, &workload.workload_id)
            .await?;
        runtimes.commit_operation(token, || ())?;
    }
    Ok(None)
}

fn startup_output_tail(
    history: dotcore_runtime::RuntimeLogHistory,
    protected: &crate::management_logs::ProtectedLogValues,
) -> Option<String> {
    const MAX_STARTUP_OUTPUT_BYTES: usize = 4096;
    let mut tail = String::new();
    for entry in history.entries {
        let filtered = crate::management_logs::filter_entry(entry, protected);
        let safe =
            crate::diagnostic::redact_filesystem_paths(&String::from_utf8_lossy(&filtered.content));
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.extend(safe.chars().filter(|character| {
            !character.is_control() || *character == '\n' || *character == '\t'
        }));
        if tail.len() > MAX_STARTUP_OUTPUT_BYTES {
            let mut start = tail.len() - MAX_STARTUP_OUTPUT_BYTES;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            tail.drain(..start);
        }
    }
    (!tail.is_empty()).then_some(tail)
}

fn protected_startup_values(
    environment: &BTreeMap<String, dotcore_runtime::WorkloadValue>,
) -> crate::management_logs::ProtectedLogValues {
    // Token values are runtime-resolved paths, hosts, and ports, never owner secrets, so each
    // literal segment of a sensitive value is protected on its own.
    let values = environment
        .iter()
        .filter(|(key, _)| crate::management_logs::is_sensitive_environment_name(key))
        .flat_map(|(_, value)| value.segments())
        .filter_map(|segment| match segment {
            dotcore_runtime::WorkloadValueSegment::Literal(literal) if !literal.is_empty() => {
                Some(literal.clone())
            }
            _ => None,
        });
    crate::management_logs::ProtectedLogValues::new(values)
}

#[cfg(test)]
mod tests;
