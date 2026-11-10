//! Apps-controller realizer reconciliation selection and per-deployment failure classification.
//!
//! Each pass reconciles every assigned deployment on its own: a deployment whose runtime actions
//! the node view lease refuses keeps its observed state, and one deployment's failure never fails
//! the pass. A deployment is realized only with ownership evidence for its storage and with its
//! local-development images pinned once in durable node records.

use std::{collections::BTreeSet, sync::Mutex};

use dotcore_domain::{
    deployment::launch_pin::{
        LaunchPinError, LaunchPinResolver, SyntheticLaunchPins, UnpinnedSelection,
    },
    timing::ReconcileOutcome,
};
use dotcore_proto::dotcore::{
    deployment::v1::NodeResolvedLaunchIdentity, site::store::v1::NodeApplicationRealization,
};
use dotcore_runtime::{
    WorkloadAssetMaterialization, WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadUnitState,
};
use dotcore_site::{
    repository::ApplicationSiteSnapshot,
    view::fencing::{ActionAssignment, LifecycleOperationAttempt, LifecycleOperationToken},
};
use tracing::{debug, error, info, warn};

use super::errors::{
    DeploymentFailureCode, DeploymentReconcileFailure, RetainedTargetMismatch,
    coded_deployment_failure, deployment_failure,
};
use super::observation::{
    ExactOwnedObservation, exact_owned_observation, group_has_absent_unit,
    group_has_active_or_unavailable_unit, group_has_stopped_unit, group_has_stopping_unit,
    group_is_all_stopped, observation_matches_group, observation_owner_id, stale_owned_group,
};
use super::readiness::{EndpointProbe, awaits_readiness};
use super::*;
use crate::deployment_logs::PreparationOwner;
use crate::dispatch::application::{ApplicationRuntimeError, TargetWorkloadGroupObservation};

impl ApplicationRealizer {
    /// Runs one serialized pass and reports whether any deployment still awaits its primary
    /// endpoint, so the observer probes again soon.
    pub(super) async fn reconcile(&self) -> Result<ReconcileOutcome, ApplicationLifecycleError> {
        let _operation = self.coordination.operation.lock().await;
        let cancellation = self.shutdown.child_token();
        *self
            .active_reconciliation
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(cancellation);
        let result = self.reconcile_active().await;
        self.active_reconciliation
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .take();
        result
    }

    async fn reconcile_active(&self) -> Result<ReconcileOutcome, ApplicationLifecycleError> {
        let ApplicationRealizationSnapshot {
            site:
                ApplicationSiteSnapshot {
                    site_id,
                    nodes,
                    execution_targets,
                    deployments,
                },
            assignments,
            view,
        } = self.input.realization_snapshot()?;
        let site = Site {
            site_id,
            nodes,
            execution_targets,
            ..Default::default()
        };
        let current = self
            .current
            .snapshot()
            .map_err(current_error)?
            .deployments
            .into_iter()
            .map(|record| (record.application_deployment_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        let mut teardown = Vec::new();
        let mut retained = Vec::new();
        for deployment in deployments {
            let Some(assignment) = assignments.get(&deployment.application_deployment_id) else {
                continue;
            };
            if desired_state(&deployment)? == ApplicationDeploymentDesiredState::Teardown {
                teardown.push((deployment, assignment));
            } else {
                retained.push((deployment, assignment));
            }
        }

        let had_teardown = !teardown.is_empty();
        for (deployment, assignment) in teardown {
            debug!(
                event_kind = "application_reconciliation_decision",
                deployment_id = %deployment.application_deployment_id,
                desired_state = deployment.desired_state,
                "Application teardown reconciliation decision"
            );
            let pass = self.reconcile_teardown(&deployment, assignment).await;
            let record = PassRecord::new(
                current.get(&deployment.application_deployment_id),
                assignment,
                view.as_ref(),
            );
            self.publish_pass(&deployment, &record, pass)?;
        }
        if had_teardown && retained.is_empty() {
            if let Some(attribution) = &self.accelerator_attribution {
                attribution.retain(&BTreeSet::new());
            }
            if let Some(resources) = &self.resource_attribution {
                resources.retain(&BTreeSet::new());
            }
            return Ok(ReconcileOutcome::Settled);
        }

        let observations = self
            .runtimes
            .discover_workload_groups()
            .await
            .map_err(|source| {
                configuration_with_source("Runtime group discovery failed", source)
            })?;
        let retained_ids = retained
            .iter()
            .map(|(deployment, _)| deployment.application_deployment_id.as_str())
            .collect::<BTreeSet<_>>();
        self.startup.retain(&retained_ids);
        if let Some(resources) = &self.resource_attribution {
            resources.retain(&retained_ids);
        }
        if let Some(attribution) = &self.accelerator_attribution {
            attribution.retain(&retained_ids);
            for (deployment, _) in &retained {
                let deployment_id = deployment.application_deployment_id.as_str();
                match observations
                    .iter()
                    .find(|candidate| observation_owner_id(candidate) == Some(deployment_id))
                {
                    Some(candidate) => attribution.observe(deployment_id, &candidate.observation),
                    // A group that vanished leaves no process to attribute, so its old identities
                    // must not stand in for it.
                    None => attribution.forget(deployment_id),
                }
            }
        }
        let mut reconciliation = ReconcileOutcome::Settled;
        for (deployment, assignment) in retained {
            debug!(
                event_kind = "application_reconciliation_decision",
                deployment_id = %deployment.application_deployment_id,
                desired_state = deployment.desired_state,
                observed_group_count = observations.len(),
                "Application deployment reconciliation decision"
            );
            let record = PassRecord::new(
                current.get(&deployment.application_deployment_id),
                assignment,
                view.as_ref(),
            );
            let pass = match validate_retained_target(&deployment, &site) {
                Ok(()) => {
                    self.reconcile_retained(&deployment, &record, &observations)
                        .await
                }
                Err(source) => DeploymentPass::reconciled(Err(deployment_failure(
                    "target-compatibility",
                    source,
                ))),
            };
            let pass = self.settle_startup(&deployment, pass)?;
            if matches!(&pass, DeploymentPass::Reconciled(result)
                if result.as_ref().as_ref().is_ok_and(awaits_readiness))
            {
                reconciliation = ReconcileOutcome::Pending;
            }
            self.publish_pass(&deployment, &record, pass)?;
        }
        Ok(reconciliation)
    }

    /// Reports a present deployment that boots in its current start as `Starting` rather than
    /// `Unavailable`; other passes are returned unchanged.
    fn settle_startup(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        pass: DeploymentPass,
    ) -> Result<DeploymentPass, ApplicationLifecycleError> {
        if desired_state(deployment)? != ApplicationDeploymentDesiredState::Present {
            return Ok(pass);
        }
        let DeploymentPass::Reconciled(result) = pass else {
            return Ok(pass);
        };
        Ok(DeploymentPass::reconciled((*result).map(|state| {
            self.startup.settle(
                &deployment.application_deployment_id,
                deployment.lifecycle_generation,
                state,
            )
        })))
    }

    /// Reconciles one retained deployment under one captured lifecycle operation; a refused
    /// capture freezes the deployment for this pass.
    async fn reconcile_retained(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        record: &PassRecord<'_>,
        observations: &[TargetWorkloadGroupObservation],
    ) -> DeploymentPass {
        let Ok(token) = self.runtimes.capture_operation(
            record.assignment,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation: deployment.lifecycle_generation,
            },
        ) else {
            return DeploymentPass::Frozen;
        };
        let effective = match self
            .prepare_node_launch(deployment, record.get().as_ref(), observations)
            .await
        {
            Ok(effective) => effective,
            Err(failure) => return DeploymentPass::reconciled(Err(failure)),
        };
        let launch = match super::launch::workload_group_launch(
            &effective,
            self.publication_host,
            self.inference_route_resolver.as_ref(),
        ) {
            Ok(launch) => launch,
            Err(source) => {
                return DeploymentPass::reconciled(Err(deployment_failure(
                    "launch-preparation",
                    source,
                )));
            }
        };
        self.track_resources(&effective, &launch);
        DeploymentPass::reconciled(
            reconcile_deployment(self, &effective, &launch, &token, record, observations).await,
        )
    }

    /// Reads the host resources of the group a present deployment runs as `launch` with every
    /// hardware collection; any other deployment's group is not read.
    fn track_resources(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        launch: &WorkloadGroupLaunch,
    ) {
        let Some(resources) = &self.resource_attribution else {
            return;
        };
        let deployment_id = deployment.application_deployment_id.as_str();
        match (desired_state(deployment), deployment.placement.as_ref()) {
            (Ok(ApplicationDeploymentDesiredState::Present), Some(placement)) => {
                resources.track(deployment_id, &placement.execution_target_id, launch);
            }
            _ => resources.forget(deployment_id),
        }
    }

    /// Checks a deployment's node records before any runtime action: a recovery barrier
    /// at this generation or an uncertain start of a newer one stops it, unowned application data
    /// fails it closed, and its local-development images are pinned once and persisted. Returns
    /// the declaration with every launch identity filled in.
    async fn prepare_node_launch(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        existing: Option<&CurrentApplicationDeployment>,
        observations: &[TargetWorkloadGroupObservation],
    ) -> Result<ApplicationDeploymentDeclaration, DeploymentReconcileFailure> {
        let deployment_id = deployment.application_deployment_id.as_str();
        let generation = deployment.lifecycle_generation;
        if let Some(record) = existing
            && let Some(state) = record.state.as_ref()
        {
            let reconciled = record.reconciled_lifecycle_generation;
            if reconciled == Some(generation)
                && state.failure.as_ref().is_some_and(|failure| {
                    failure.code == DeploymentFailureCode::RecoveryRequired.code()
                })
            {
                return Err(coded_deployment_failure(
                    "runtime-ownership",
                    DeploymentFailureCode::RecoveryRequired,
                    configuration("Application deployment awaits recovery on this node"),
                ));
            }
            // A newer generation that prepared its image may have created its group in the same
            // pass, so a Preparing record is as uncertain as a Starting one.
            if reconciled.is_some_and(|reconciled| reconciled > generation)
                && matches!(
                    ApplicationLifecycleState::try_from(state.lifecycle_state),
                    Ok(ApplicationLifecycleState::Preparing | ApplicationLifecycleState::Starting)
                )
            {
                warn!(
                    event_kind = "application_start_uncertain",
                    deployment_id,
                    lifecycle_generation = generation,
                    reconciled_lifecycle_generation = reconciled,
                    "Application start of a newer lifecycle generation is uncertain; the deployment requires recovery"
                );
                return Err(coded_deployment_failure(
                    "runtime-ownership",
                    DeploymentFailureCode::RecoveryRequired,
                    configuration("Application start of a newer lifecycle generation is uncertain"),
                ));
            }
        }

        let realization = self
            .node
            .records
            .realization(deployment_id)
            .map_err(|source| deployment_failure("runtime-ownership", source))?;
        if realization.is_none() {
            let owned_group = observations
                .iter()
                .any(|candidate| observation_owner_id(candidate) == Some(deployment_id));
            let execution_target_id = deployment
                .placement
                .as_ref()
                .map(|placement| placement.execution_target_id.as_str())
                .unwrap_or_default();
            if !owned_group
                && self
                    .runtimes
                    .storage_exists(
                        execution_target_id,
                        &super::launch::deployment_data(deployment),
                    )
                    .await
                    .map_err(|source| deployment_failure("runtime-ownership", source))?
            {
                warn!(
                    event_kind = "application_unowned_data",
                    deployment_id,
                    execution_target_id,
                    "Application data exists on this node without ownership evidence; realization fails closed"
                );
                return Err(coded_deployment_failure(
                    "runtime-ownership",
                    DeploymentFailureCode::RecoveryRequired,
                    configuration("Application data exists without ownership evidence"),
                ));
            }
        }

        let retained = realization
            .as_ref()
            .map(|realization| realization.resolved_launch_identities.as_slice())
            .unwrap_or_default();
        let pins = self.pin_launch_identities(deployment, retained).await?;
        if pins.as_slice() != retained {
            info!(
                event_kind = "application_launch_identity_pinned",
                deployment_id,
                pinned_selections = pins.len(),
                "Application local-development launch identities pinned on this node"
            );
        }
        self.node
            .records
            .record_realization(NodeApplicationRealization {
                application_deployment_id: deployment_id.to_owned(),
                realization_generation: realization
                    .as_ref()
                    .and_then(|realization| realization.realization_generation),
                resolved_launch_identities: pins.clone(),
            })
            .map_err(|source| deployment_failure("launch-preparation", source))?;
        Ok(with_launch_identities(deployment, &pins))
    }

    /// Pins every local-development or Docker-delivered Nix selection that has no launch identity,
    /// resolving only selections without a retained or projected pin.
    async fn pin_launch_identities(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        retained: &[NodeResolvedLaunchIdentity],
    ) -> Result<Vec<NodeResolvedLaunchIdentity>, DeploymentReconcileFailure> {
        let invalid =
            |message: &str| deployment_failure("launch-preparation", configuration(message));
        let snapshot = deployment
            .snapshot
            .as_ref()
            .ok_or_else(|| invalid("Application snapshot is missing"))?;
        let selections = snapshot
            .service_selections
            .iter()
            .filter(|selection| selection.launch_identity.is_empty())
            .map(|selection| {
                Ok(UnpinnedSelection {
                    selection_key: selection.service_name.clone(),
                    source: crate::application::repository::selection_pin_source(
                        snapshot, selection,
                    )
                    .ok_or_else(|| {
                        invalid("Application service selection has nothing to pin on this node")
                    })?,
                })
            })
            .collect::<Result<Vec<_>, DeploymentReconcileFailure>>()?;
        if selections.is_empty() {
            return Ok(Vec::new());
        }
        let platform = snapshot
            .platform
            .as_ref()
            .ok_or_else(|| invalid("Application retained platform is missing"))?;
        let placement = deployment
            .placement
            .as_ref()
            .ok_or_else(|| invalid("Application placement is missing"))?;
        // A runtime that launches from references alone, such as the simulated one, never reads
        // the artifact, so its services pin synthetic identities without the node's resolver.
        let resolver: Option<&dyn LaunchPinResolver> = if self
            .launches_from_references(&placement.execution_target_id)
            .map_err(|source| deployment_failure("launch-preparation", source))?
        {
            Some(&SyntheticLaunchPins)
        } else {
            self.launch_pins.as_deref()
        };
        let cancellation = self.active_cancellation("launch-preparation")?;
        dotcore_domain::deployment::launch_pin::pin_launch_identities(
            &selections,
            &deployment.node_resolved_launch_identities,
            retained,
            platform,
            resolver,
            &cancellation,
        )
        .await
        .map_err(|error| {
            let failure = match error {
                LaunchPinError::ProjectedMismatch { .. } | LaunchPinError::KindMismatch { .. } => {
                    DeploymentFailureCode::RecoveryRequired
                }
                LaunchPinError::Unresolved { .. } => {
                    DeploymentFailureCode::LaunchIdentityUnresolved
                }
            };
            coded_deployment_failure("launch-preparation", failure, error)
        })
    }

    /// Removes one withdrawn deployment's workload group and the storage this node owns. A
    /// deployment whose cleanup evidence is already retained needs nothing.
    async fn reconcile_teardown(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        assignment: &ActionAssignment,
    ) -> DeploymentPass {
        match self.node.records.cleanup_confirmed(
            &deployment.application_deployment_id,
            deployment.lifecycle_generation,
        ) {
            Ok(true) => return DeploymentPass::Settled,
            Ok(false) => {}
            Err(source) => {
                return DeploymentPass::reconciled(Err(deployment_failure(
                    "runtime-cleanup",
                    source,
                )));
            }
        }
        let Ok(token) = self.runtimes.capture_operation(
            assignment,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation: deployment.lifecycle_generation,
            },
        ) else {
            return DeploymentPass::Frozen;
        };
        DeploymentPass::reconciled(
            self.remove_deployment_infrastructure(deployment, &token)
                .await,
        )
    }

    async fn remove_deployment_infrastructure(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        token: &LifecycleOperationToken,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        let placement = deployment.placement.as_ref().ok_or_else(|| {
            deployment_failure(
                "runtime-cleanup",
                ApplicationLifecycleError::RetainedTargetIncompatible(
                    RetainedTargetMismatch::Placement,
                ),
            )
        })?;
        let group_id =
            dotcore_runtime::WorkloadGroupId(deployment.application_deployment_id.clone());
        self.runtimes
            .ensure_workload_group_absent(
                token,
                &placement.execution_target_id,
                &group_id,
                &deployment.application_deployment_id,
            )
            .await
            .map_err(|source| deployment_failure("runtime-cleanup", source))?;
        self.lifecycle
            .removed(&deployment.application_deployment_id);
        if self.owns_storage(deployment)? {
            self.runtimes
                .remove_storage(
                    token,
                    &placement.execution_target_id,
                    &deployment.application_deployment_id,
                    &super::launch::deployment_data(deployment),
                )
                .await
                .map_err(|source| deployment_failure("runtime-cleanup", source))?;
        }
        Ok(stopped_state())
    }

    /// Whether this node owns the deployment's storage, so teardown may delete it: only with its
    /// realization record.
    fn owns_storage(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
    ) -> Result<bool, DeploymentReconcileFailure> {
        self.node
            .records
            .realization(&deployment.application_deployment_id)
            .map(|realization| realization.is_some())
            .map_err(|source| deployment_failure("runtime-cleanup", source))
    }

    /// Publishes one deployment's pass outcome: its state, and the view it reconciled. A frozen or settled deployment publishes nothing.
    fn publish_pass(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        record: &PassRecord<'_>,
        pass: DeploymentPass,
    ) -> Result<(), ApplicationLifecycleError> {
        let result = match pass {
            DeploymentPass::Settled => return Ok(()),
            DeploymentPass::Frozen => {
                debug!(event_kind = "application_reconciliation_frozen", deployment_id = %deployment.application_deployment_id, "Application runtime actions are frozen until the node view lease is renewed");
                return Ok(());
            }
            DeploymentPass::Reconciled(result) => *result,
        };
        if let Err(failure) = &result
            && failure.action_refused()
        {
            debug!(event_kind = "application_reconciliation_frozen", deployment_id = %deployment.application_deployment_id, failure_stage = failure.stage, "Application runtime action was refused by the node view lease; state is kept");
            return Ok(());
        }
        self.publish_reconciliation(deployment, record.get().as_ref(), result)?;
        self.mark_reconciled(deployment, record);
        Ok(())
    }

    /// Records that the deployment's state now stands for the assignment and view of this pass,
    /// so the node's status reports that view revision.
    fn mark_reconciled(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        record: &PassRecord<'_>,
    ) {
        if let Some(view) = record.view {
            self.node.reconciled.record(
                &deployment.application_deployment_id,
                record.assignment,
                view,
            );
        }
    }

    fn publish_reconciliation(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        existing: Option<&CurrentApplicationDeployment>,
        reconciliation: Result<ApplicationDeploymentState, DeploymentReconcileFailure>,
    ) -> Result<(), ApplicationLifecycleError> {
        let previous = existing.and_then(|record| record.state.as_ref());
        let mut next = reconciliation.unwrap_or_else(|failure| {
            error!(event_kind = "application_reconciliation_failed", deployment_id = %deployment.application_deployment_id, failure_stage = failure.stage, failure_code = failure.failure.map(DeploymentFailureCode::code), error = ?failure.source, "Application deployment reconciliation failed");
            deployment_failure_state(failure.stage, failure.failure)
        });
        next.observed_placement = match ApplicationLifecycleState::try_from(next.lifecycle_state)
            .ok()
        {
            Some(ApplicationLifecycleState::Stopped | ApplicationLifecycleState::Queued) => None,
            Some(ApplicationLifecycleState::Ready | ApplicationLifecycleState::Paused) => {
                deployment.placement.clone()
            }
            _ => previous.and_then(|state| state.observed_placement.clone()),
        };
        let generation_changed = existing.is_none_or(|record| {
            record.reconciled_lifecycle_generation != Some(deployment.lifecycle_generation)
        });
        next = with_start_steps(
            previous
                .filter(|_| !generation_changed)
                .and_then(|state| state.steps.as_ref()),
            next,
        );
        let teardown_complete = desired_state(deployment)?
            == ApplicationDeploymentDesiredState::Teardown
            && next.lifecycle_state == ApplicationLifecycleState::Stopped as i32;
        match settled_state(previous, next.clone())
            .or_else(|| generation_changed.then(|| previous.cloned()).flatten())
        {
            Some(settled) => {
                next = settled;
                self.write_current(deployment, next.clone(), existing)?;
            }
            None => {
                debug!(
                    event_kind = "application_reconciliation_unchanged",
                    deployment_id = %deployment.application_deployment_id,
                    stage = %next.stage,
                    "Application deployment state unchanged; skipped current state write"
                );
            }
        }
        if previous.is_none_or(|previous| {
            previous.lifecycle_state != next.lifecycle_state
                || previous.readiness_state != next.readiness_state
                || previous.stage != next.stage
        }) {
            info!(
                event_kind = "application_lifecycle_transition",
                deployment_id = %deployment.application_deployment_id,
                previous_lifecycle_state = previous.map(|state| state.lifecycle_state),
                next_lifecycle_state = next.lifecycle_state,
                previous_readiness_state = previous.map(|state| state.readiness_state),
                next_readiness_state = next.readiness_state,
                stage = %next.stage,
                "Application deployment lifecycle transitioned"
            );
        }
        if teardown_complete {
            self.results.confirm_cleanup(deployment)?;
        }
        Ok(())
    }

    async fn reconcile_observation(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        candidate: &TargetWorkloadGroupObservation,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        if (desired.inference_route_id.is_some()
            || matches!(
                desired_state(desired),
                Ok(ApplicationDeploymentDesiredState::Present
                    | ApplicationDeploymentDesiredState::Teardown)
            ))
            && let Some(group) =
                stale_owned_group(candidate, launch, execution_target_id, &self.runtimes)
        {
            return self
                .reconcile_stale_group(token, record, desired, execution_target_id, launch, group)
                .await;
        }
        let rehydrated;
        let candidate = if matches!(
            candidate.observation,
            WorkloadGroupObservation::Unknown { .. }
        ) {
            rehydrated = self
                .runtimes
                .inspect_workload_group(execution_target_id, launch)
                .await
                .map_err(|source| deployment_failure("runtime-inspection", source))?;
            TargetWorkloadGroupObservation {
                execution_target_id: candidate.execution_target_id.clone(),
                observation: rehydrated,
            }
        } else {
            candidate.clone()
        };
        let recognized =
            exact_owned_observation(&candidate, launch, execution_target_id, &self.runtimes);
        match desired_state(desired)
            .map_err(|source| deployment_failure("launch-preparation", source))?
        {
            ApplicationDeploymentDesiredState::Teardown => match recognized {
                Some(ExactOwnedObservation::Complete { .. })
                | Some(ExactOwnedObservation::Partial { .. }) => {
                    self.remove_and_confirm_absence(token, execution_target_id, launch)
                        .await
                }
                Some(ExactOwnedObservation::Absent) => Ok(stopped_state()),
                _ => Ok(unavailable_state(
                    "Application runtime group is not safe to remove",
                    None,
                )),
            },
            ApplicationDeploymentDesiredState::Present => match recognized {
                Some(ExactOwnedObservation::Absent) => {
                    self.create_and_observe(
                        token,
                        record,
                        desired,
                        execution_target_id,
                        launch,
                        "runtime-creation",
                    )
                    .await
                }
                Some(ExactOwnedObservation::Partial { .. }) => {
                    self.create_and_observe(
                        token,
                        record,
                        desired,
                        execution_target_id,
                        launch,
                        "runtime-recovery-creation",
                    )
                    .await
                }
                Some(ExactOwnedObservation::Complete {
                    group: _,
                    stale: true,
                }) => {
                    self.runtimes
                        .remove_workload_group(token, execution_target_id, launch)
                        .await
                        .map_err(|source| {
                            deployment_failure("runtime-recovery-replacement-removal", source)
                        })?;
                    self.create_and_observe(
                        token,
                        record,
                        desired,
                        execution_target_id,
                        launch,
                        "runtime-recovery-replacement-creation",
                    )
                    .await
                }
                Some(ExactOwnedObservation::Complete {
                    group,
                    stale: false,
                }) if group_has_absent_unit(group) => {
                    self.create_and_observe(
                        token,
                        record,
                        desired,
                        execution_target_id,
                        launch,
                        "runtime-recovery-creation",
                    )
                    .await
                }
                Some(ExactOwnedObservation::Complete {
                    group,
                    stale: false,
                }) if group_is_all_stopped(group) => {
                    self.start_and_observe(token, record, desired, execution_target_id, launch)
                        .await
                }
                Some(ExactOwnedObservation::Complete {
                    group,
                    stale: false,
                }) if group_has_stopped_unit(group) => {
                    self.runtimes
                        .remove_workload_group(token, execution_target_id, launch)
                        .await
                        .map_err(|source| {
                            deployment_failure("runtime-replacement-removal", source)
                        })?;
                    self.create_and_observe(
                        token,
                        record,
                        desired,
                        execution_target_id,
                        launch,
                        "runtime-replacement-creation",
                    )
                    .await
                }
                Some(ExactOwnedObservation::Complete {
                    group,
                    stale: false,
                }) => {
                    self.announce_running(desired, group);
                    readiness::state_from_group(
                        desired,
                        launch,
                        group,
                        self.publication_host,
                        self.endpoint_probe(desired),
                    )
                    .await
                    .map_err(|source| deployment_failure("readiness-operation", source))
                }
                None => Ok(unavailable_state(
                    "Application runtime group is stale or inconclusive",
                    None,
                )),
            },
            ApplicationDeploymentDesiredState::Paused => {
                self.reconcile_paused_observation(
                    token,
                    desired,
                    execution_target_id,
                    launch,
                    &candidate,
                    recognized,
                )
                .await
            }
            ApplicationDeploymentDesiredState::Unspecified => Err(deployment_failure(
                "launch-preparation",
                configuration("Application declaration has unspecified desired state"),
            )),
        }
    }

    async fn reconcile_stale_group(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        observed: &dotcore_runtime::ObservedWorkloadGroup,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        let desired_state = desired_state(desired)
            .map_err(|source| deployment_failure("launch-preparation", source))?;
        if desired_state == ApplicationDeploymentDesiredState::Unspecified {
            return Err(deployment_failure(
                "launch-preparation",
                configuration("Application declaration has unspecified desired state"),
            ));
        }
        self.runtimes
            .remove_observed_workload_group(token, execution_target_id, observed)
            .await
            .map_err(|source| deployment_failure("runtime-stale-removal", source))?;
        match desired_state {
            ApplicationDeploymentDesiredState::Present => {
                self.create_and_observe(
                    token,
                    record,
                    desired,
                    execution_target_id,
                    launch,
                    "runtime-replacement-creation",
                )
                .await
            }
            ApplicationDeploymentDesiredState::Teardown => {
                self.remove_and_confirm_absence(token, execution_target_id, launch)
                    .await
            }
            ApplicationDeploymentDesiredState::Paused => {
                self.prepare_application_artifacts(desired).await?;
                let observation = self
                    .runtimes
                    .create_workload_group(token, execution_target_id, launch)
                    .await
                    .map_err(|source| deployment_failure("runtime-replacement-creation", source))?;
                self.announce_started(desired, &observation, |state| {
                    state != WorkloadUnitState::Absent
                });
                self.record_realized_launch(desired, launch, "runtime-replacement-creation")?;
                self.runtimes
                    .stop_workload_group(token, execution_target_id, launch)
                    .await
                    .map_err(|source| deployment_failure("runtime-replacement-stop", source))?;
                self.coordination.wakeup.notify_one();
                Ok(pausing_state(None))
            }
            ApplicationDeploymentDesiredState::Unspecified => unreachable!(),
        }
    }

    async fn reconcile_missing(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        let desired_state = desired_state(desired)
            .map_err(|source| deployment_failure("launch-preparation", source))?;
        match desired_state {
            ApplicationDeploymentDesiredState::Teardown => {
                self.remove_and_confirm_absence(token, execution_target_id, launch)
                    .await
            }
            ApplicationDeploymentDesiredState::Paused => Ok(unavailable_state(
                "Application runtime group is unavailable while paused",
                None,
            )),
            ApplicationDeploymentDesiredState::Present => {
                self.create_and_observe(
                    token,
                    record,
                    desired,
                    execution_target_id,
                    launch,
                    "runtime-creation",
                )
                .await
            }
            ApplicationDeploymentDesiredState::Unspecified => Err(deployment_failure(
                "launch-preparation",
                configuration("Application declaration has unspecified desired state"),
            )),
        }
    }

    async fn remove_and_confirm_absence(
        &self,
        token: &LifecycleOperationToken,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        self.runtimes
            .remove_workload_group(token, execution_target_id, launch)
            .await
            .map_err(|source| deployment_failure("runtime-removal", source))?;
        let observation = self
            .runtimes
            .inspect_workload_group(execution_target_id, launch)
            .await
            .map_err(|source| deployment_failure("runtime-removal-confirmation", source))?;
        let candidate = TargetWorkloadGroupObservation {
            execution_target_id: execution_target_id.to_owned(),
            observation,
        };
        match exact_owned_observation(&candidate, launch, execution_target_id, &self.runtimes) {
            Some(ExactOwnedObservation::Absent) => Ok(stopped_state()),
            Some(ExactOwnedObservation::Complete { .. })
            | Some(ExactOwnedObservation::Partial { .. }) => Ok(unavailable_state(
                "Application runtime group remains after removal",
                None,
            )),
            None => Ok(unavailable_state(
                "Application runtime group removal is inconclusive",
                None,
            )),
        }
    }

    async fn create_and_observe(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        stage: &'static str,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        self.prepare_start(token, record, desired).await?;
        self.forget_announced(desired, launch);
        let observation = match self
            .runtimes
            .create_workload_group(token, execution_target_id, launch)
            .await
        {
            Ok(observation) => {
                self.announce_started(desired, &observation, |state| {
                    state != WorkloadUnitState::Absent
                });
                self.record_realized_launch(desired, launch, stage)?;
                observation
            }
            Err(source @ ApplicationRuntimeError::ActionRejected) => {
                return Err(deployment_failure(stage, source));
            }
            Err(source) => {
                warn!(event_kind = "application_reconciliation_retryable_failure", deployment_id = %desired.application_deployment_id, failure_stage = stage, outcome = "failed", error = ?source, "Application workload-group creation failed");
                return Ok(workload_creation_failure_state(
                    launch,
                    runtime_failure_code(&source),
                ));
            }
        };
        readiness::state_from_observation(
            desired,
            launch,
            &observation,
            self.publication_host,
            self.endpoint_probe(desired),
        )
        .await
        .map_err(|source| deployment_failure("readiness-operation", source))
    }

    /// Returns how readiness checks one deployment: a runtime that launches from references
    /// alone, such as the simulated one, runs nothing that could answer a probe.
    fn endpoint_probe(&self, desired: &ApplicationDeploymentDeclaration) -> EndpointProbe {
        let virtualized = desired.placement.as_ref().is_some_and(|placement| {
            self.launches_from_references(&placement.execution_target_id)
                .unwrap_or(false)
        });
        if virtualized {
            EndpointProbe::Skip
        } else {
            EndpointProbe::Probe
        }
    }

    /// Returns whether the runtime of one target launches from artifact references alone, with
    /// nothing prepared on the host.
    fn launches_from_references(
        &self,
        execution_target_id: &str,
    ) -> Result<bool, crate::dispatch::application::ApplicationRuntimeError> {
        Ok(self
            .runtimes
            .materialization_requirements(execution_target_id)?
            .executable_artifact
            == WorkloadAssetMaterialization::ReferenceOnly)
    }

    /// Prepares the image of a deployment about to start. A deployment whose image this node
    /// prepares is first recorded as preparing, so its status shows that step while it runs.
    async fn prepare_start(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
    ) -> Result<(), DeploymentReconcileFailure> {
        if self.image_preparer(desired)?.is_some() {
            self.record_preparing(token, record, desired)?;
        }
        self.prepare_application_artifacts(desired).await
    }

    /// Records the deployment as preparing its image, under the operation `token` captured, so a
    /// node whose lease expired writes nothing. Only a start still on its first step is recorded
    /// as preparing: one that went further, became ready or failed keeps its state while its
    /// group is recreated. Failing to write leaves the state for the pass outcome and lets
    /// preparation go on.
    fn record_preparing(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
    ) -> Result<(), DeploymentReconcileFailure> {
        let existing = record.get();
        let previous = existing.as_ref().and_then(|record| record.state.as_ref());
        let same_start = existing.as_ref().is_some_and(|record| {
            record.reconciled_lifecycle_generation == Some(desired.lifecycle_generation)
        });
        if same_start
            && !previous
                .and_then(|state| state.steps.as_ref())
                .is_some_and(|steps| steps.current == 0)
        {
            return Ok(());
        }
        let preparing = with_start_steps(
            previous
                .filter(|_| same_start)
                .and_then(|state| state.steps.as_ref()),
            preparing_state(previous),
        );
        // An equal state of another generation is still written, so the record names this one.
        let Some(state) = settled_state(previous, preparing)
            .or_else(|| (!same_start).then(|| previous.cloned()).flatten())
        else {
            // This start is already recorded as preparing, as after a node restart: its status
            // reports it for this pass's view.
            self.mark_reconciled(desired, record);
            return Ok(());
        };
        let next = CurrentApplicationDeployment {
            application_deployment_id: desired.application_deployment_id.clone(),
            record_revision: 0,
            state: Some(state),
            reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
        };
        let expected = existing.as_ref().map(|record| record.record_revision);
        let written = self
            .runtimes
            .commit_operation(token, || self.current.upsert(next.clone(), expected))
            .map_err(|source| deployment_failure("artifact-preparation", source))?;
        match written {
            Ok(mutation) => {
                record.replace(CurrentApplicationDeployment {
                    record_revision: mutation.record_revision,
                    ..next
                });
                self.mark_reconciled(desired, record);
                info!(
                    event_kind = "application_lifecycle_transition",
                    deployment_id = %desired.application_deployment_id,
                    previous_lifecycle_state = previous.map(|state| state.lifecycle_state),
                    next_lifecycle_state = ApplicationLifecycleState::Preparing as i32,
                    stage = "preparing",
                    "Application deployment lifecycle transitioned"
                );
            }
            Err(error) => {
                warn!(
                    event_kind = "application_preparing_unrecorded",
                    deployment_id = %desired.application_deployment_id,
                    error = ?error,
                    "Application preparing state could not be recorded; image preparation goes on"
                );
            }
        }
        Ok(())
    }

    /// Returns the artifact manager that prepares a deployment's image on this node, `None` when
    /// this node prepares none for it.
    fn image_preparer(
        &self,
        desired: &ApplicationDeploymentDeclaration,
    ) -> Result<Option<&dotcore_artifact::ArtifactManager>, DeploymentReconcileFailure> {
        let Some(artifacts) = self.artifact_manager.as_ref() else {
            return Ok(None);
        };
        let placement = desired.placement.as_ref().ok_or_else(|| {
            deployment_failure(
                "artifact-preparation",
                configuration("Application placement is missing"),
            )
        })?;
        // A runtime that launches from references alone, such as the simulated one, needs no
        // image on the host, even on a node whose other targets run applications on Docker.
        if self
            .launches_from_references(&placement.execution_target_id)
            .map_err(|source| deployment_failure("artifact-preparation", source))?
        {
            return Ok(None);
        }
        Ok(Some(artifacts))
    }

    async fn prepare_application_artifacts(
        &self,
        desired: &ApplicationDeploymentDeclaration,
    ) -> Result<(), DeploymentReconcileFailure> {
        let Some(artifacts) = self.image_preparer(desired)? else {
            return Ok(());
        };
        let snapshot = desired.snapshot.as_ref().ok_or_else(|| {
            deployment_failure(
                "artifact-preparation",
                configuration("Application snapshot is missing"),
            )
        })?;
        let cancellation = self.active_cancellation("artifact-preparation")?;
        artifacts
            .prepare_application(
                snapshot,
                cancellation,
                &self.preparation_logs.output(
                    PreparationOwner::Application,
                    &desired.application_deployment_id,
                ),
            )
            .await
            .map_err(|source| match source {
                // A node reports its missing pinned image as a typed failure code.
                dotcore_artifact::ApplicationArtifactPreparationError::Service {
                    source: dotcore_artifact::OciPreparationError::LocalImageMissing,
                    ..
                } => coded_deployment_failure(
                    "artifact-preparation",
                    DeploymentFailureCode::ImageMissing,
                    source,
                ),
                // An image that keeps failing verification is reported publicly while retried.
                dotcore_artifact::ApplicationArtifactPreparationError::Service {
                    source: dotcore_artifact::OciPreparationError::VerificationMismatch,
                    ..
                } => coded_deployment_failure(
                    "artifact-preparation",
                    DeploymentFailureCode::ArtifactPreparationFailed,
                    source,
                ),
                // A Docker image derivation that cannot build, load, or verify its image is
                // reported publicly while retried.
                dotcore_artifact::ApplicationArtifactPreparationError::DockerImageService {
                    source: ref error,
                    ..
                } if !matches!(
                    error,
                    dotcore_artifact::DockerImagePreparationError::Cancelled
                        | dotcore_artifact::DockerImagePreparationError::Nix(
                            dotcore_artifact::NixPreparationError::Cancelled
                        )
                ) =>
                {
                    coded_deployment_failure(
                        "artifact-preparation",
                        DeploymentFailureCode::ArtifactPreparationFailed,
                        source,
                    )
                }
                source => deployment_failure("artifact-preparation", source),
            })
    }

    /// Retains the launch generation of the workload group this node just created, so cleanup
    /// evidence and status name the realization.
    fn record_realized_launch(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        launch: &WorkloadGroupLaunch,
        stage: &'static str,
    ) -> Result<(), DeploymentReconcileFailure> {
        let node = &self.node;
        let generation = launch.correlation.reconcile_generation;
        let realized = |mut realization: NodeApplicationRealization| {
            (realization.realization_generation != Some(generation)).then(|| {
                realization.realization_generation = Some(generation);
                realization
            })
        };
        node.records
            .realization(&desired.application_deployment_id)
            .and_then(|realization| match realization.and_then(realized) {
                Some(realization) => node.records.record_realization(realization),
                None => Ok(()),
            })
            .map_err(|source| deployment_failure(stage, source))
    }

    /// Tells the deployment's open log follows about each unit a runtime action just created or
    /// started, as `started` selects them by their state after it, so they follow its new output.
    fn announce_started(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        observation: &WorkloadGroupObservation,
        started: fn(WorkloadUnitState) -> bool,
    ) {
        let WorkloadGroupObservation::Observed(group) = observation else {
            return;
        };
        for (name, unit) in &group.units {
            if !started(unit.state) {
                continue;
            }
            debug!(
                event_kind = "application_workload_started",
                deployment_id = %desired.application_deployment_id,
                unit_name = %name.0,
                "Application workload unit started; its log follows are told"
            );
            self.lifecycle
                .started(&desired.application_deployment_id, &name.0);
        }
    }

    /// Tells the deployment's open log follows about each running unit not announced since it
    /// last changed, as when the action that created or started it failed after acting.
    fn announce_running(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        group: &dotcore_runtime::ObservedWorkloadGroup,
    ) {
        for (name, unit) in &group.units {
            if matches!(
                unit.state,
                WorkloadUnitState::Starting | WorkloadUnitState::Running
            ) {
                self.lifecycle
                    .running(&desired.application_deployment_id, &name.0);
            } else {
                self.lifecycle
                    .forget(&desired.application_deployment_id, &name.0);
            }
        }
    }

    /// Forgets the announcement of every unit of `launch` before an action that may create or
    /// start them, so each is announced once a pass sees it running even when that action fails
    /// after acting.
    fn forget_announced(
        &self,
        desired: &ApplicationDeploymentDeclaration,
        launch: &WorkloadGroupLaunch,
    ) {
        for name in launch.units.keys() {
            self.lifecycle
                .forget(&desired.application_deployment_id, &name.0);
        }
    }

    /// Returns the cancellation of the active pass, which realizer shutdown cancels.
    fn active_cancellation(
        &self,
        stage: &'static str,
    ) -> Result<tokio_util::sync::CancellationToken, DeploymentReconcileFailure> {
        self.active_reconciliation
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                deployment_failure(
                    stage,
                    configuration("Application reconciliation cancellation is unavailable"),
                )
            })
    }

    async fn start_and_observe(
        &self,
        token: &LifecycleOperationToken,
        record: &PassRecord<'_>,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        self.prepare_start(token, record, desired).await?;
        self.forget_announced(desired, launch);
        let observation = self
            .runtimes
            .start_workload_group(token, execution_target_id, launch)
            .await
            .map_err(|source| match runtime_failure_code(&source) {
                Some(failure) => coded_deployment_failure("runtime-start", failure, source),
                None => deployment_failure("runtime-start", source),
            })?;
        // Every unit was stopped before, so each one running now was started again in place.
        self.announce_started(desired, &observation, |state| {
            matches!(
                state,
                WorkloadUnitState::Starting | WorkloadUnitState::Running
            )
        });
        readiness::state_from_observation(
            desired,
            launch,
            &observation,
            self.publication_host,
            self.endpoint_probe(desired),
        )
        .await
        .map_err(|source| deployment_failure("readiness-operation", source))
    }

    async fn reconcile_paused_observation(
        &self,
        token: &LifecycleOperationToken,
        desired: &ApplicationDeploymentDeclaration,
        execution_target_id: &str,
        launch: &WorkloadGroupLaunch,
        candidate: &TargetWorkloadGroupObservation,
        recognized: Option<ExactOwnedObservation<'_>>,
    ) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
        let Some(recognized) = recognized else {
            // Inspection resolves ownership, so `Unknown` here is only an inconclusive answer
            // from the runtime, unlike a group that verifiably does not match this deployment.
            if matches!(
                candidate.observation,
                WorkloadGroupObservation::Unknown { .. }
            ) {
                warn!(
                    event_kind = "application_paused_ownership_unverified",
                    deployment_id = %desired.application_deployment_id,
                    "Application runtime group ownership could not be verified while paused"
                );
                return Ok(unavailable_state(
                    "Application runtime group ownership is unverified while paused",
                    None,
                ));
            }
            return Err(deployment_failure(
                "runtime-ownership",
                configuration(
                    "Application runtime group does not match this deployment, so it is left untouched while paused",
                ),
            ));
        };
        let (group, complete, stale) = match recognized {
            ExactOwnedObservation::Complete { group, stale } => (group, true, stale),
            ExactOwnedObservation::Partial { group, stale } => (group, false, stale),
            ExactOwnedObservation::Absent => {
                return Ok(unavailable_state(
                    "Application runtime group is unavailable while paused",
                    None,
                ));
            }
        };
        if group_has_active_or_unavailable_unit(group) {
            self.runtimes
                .stop_workload_group(token, execution_target_id, launch)
                .await
                .map_err(|source| deployment_failure("runtime-stop", source))?;
            self.coordination.wakeup.notify_one();
            return Ok(pausing_state(None));
        }
        if group_has_stopping_unit(group) {
            return Ok(pausing_state(None));
        }
        if complete && !stale && group_is_all_stopped(group) {
            return Ok(paused_state(None));
        }
        Ok(unavailable_state(
            "Application runtime group is unavailable while paused",
            None,
        ))
    }

    fn write_current(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        state: ApplicationDeploymentState,
        existing: Option<&CurrentApplicationDeployment>,
    ) -> Result<(), ApplicationLifecycleError> {
        self.current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(state),
                    reconciled_lifecycle_generation: Some(deployment.lifecycle_generation),
                },
                existing.map(|record| record.record_revision),
            )
            .map_err(current_error)?;
        Ok(())
    }
}

/// Reconciles one desired declaration and its launch against the complete runtime observation
/// snapshot, acting only under `token`.
pub(super) async fn reconcile_deployment(
    inner: &ApplicationRealizer,
    deployment: &ApplicationDeploymentDeclaration,
    launch: &WorkloadGroupLaunch,
    token: &LifecycleOperationToken,
    record: &PassRecord<'_>,
    observations: &[TargetWorkloadGroupObservation],
) -> Result<ApplicationDeploymentState, DeploymentReconcileFailure> {
    let execution_target_id = deployment
        .placement
        .as_ref()
        .ok_or_else(|| {
            deployment_failure(
                "launch-preparation",
                configuration("Application deployment exact placement is missing"),
            )
        })?
        .execution_target_id
        .clone();
    let matches = observations
        .iter()
        .filter(|candidate| {
            observation_owner_id(candidate) == Some(deployment.application_deployment_id.as_str())
                || observation_matches_group(candidate, launch)
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(deployment_failure(
            "runtime-ownership",
            configuration("ambiguous runtime group observation"),
        ));
    }
    if let Some(candidate) = matches.first() {
        inner
            .reconcile_observation(
                token,
                record,
                deployment,
                &execution_target_id,
                launch,
                candidate,
            )
            .await
    } else {
        inner
            .reconcile_missing(token, record, deployment, &execution_target_id, launch)
            .await
    }
}

/// Outcome of reconciling one deployment within a pass.
pub(super) enum DeploymentPass {
    /// Reconciliation produced a state or a deployment failure.
    Reconciled(Box<Result<ApplicationDeploymentState, DeploymentReconcileFailure>>),

    /// The node view lease refused the deployment's runtime actions; its state stays unchanged.
    Frozen,

    /// Cleanup evidence for this teardown is already retained, so nothing remains to do.
    Settled,
}

impl DeploymentPass {
    fn reconciled(result: Result<ApplicationDeploymentState, DeploymentReconcileFailure>) -> Self {
        Self::Reconciled(Box::new(result))
    }
}

/// One deployment's place in a pass: the current record it reconciles from, and the assignment
/// and view it acts for. A state the pass records before its outcome, such as image preparation,
/// replaces the record, so the outcome is written over it.
pub(super) struct PassRecord<'a> {
    record: Mutex<Option<CurrentApplicationDeployment>>,
    assignment: &'a ActionAssignment,
    view: Option<&'a RealizedView>,
}

impl<'a> PassRecord<'a> {
    fn new(
        record: Option<&CurrentApplicationDeployment>,
        assignment: &'a ActionAssignment,
        view: Option<&'a RealizedView>,
    ) -> Self {
        Self {
            record: Mutex::new(record.cloned()),
            assignment,
            view,
        }
    }

    fn get(&self) -> Option<CurrentApplicationDeployment> {
        self.lock().clone()
    }

    fn replace(&self, record: CurrentApplicationDeployment) {
        *self.lock() = Some(record);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CurrentApplicationDeployment>> {
        self.record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Returns `deployment` with each empty launch identity replaced by its node pin.
pub(super) fn with_launch_identities(
    deployment: &ApplicationDeploymentDeclaration,
    pins: &[NodeResolvedLaunchIdentity],
) -> ApplicationDeploymentDeclaration {
    let mut effective = deployment.clone();
    if let Some(snapshot) = effective.snapshot.as_mut() {
        for selection in &mut snapshot.service_selections {
            if let Some(pin) = pins.iter().find(|pin| {
                selection.launch_identity.is_empty() && pin.selection_key == selection.service_name
            }) {
                selection.launch_identity = pin.launch_identity.clone();
            }
        }
    }
    effective
}

/// Returns the stable failure a runtime error names on the hosting node, such as a missing
/// image or a published host port something else holds; other errors have none.
fn runtime_failure_code(error: &ApplicationRuntimeError) -> Option<DeploymentFailureCode> {
    let ApplicationRuntimeError::TargetAdapter { source, .. } = error else {
        return None;
    };
    if source.is_local_image_missing() {
        Some(DeploymentFailureCode::ImageMissing)
    } else if source.is_published_port_in_use() {
        Some(DeploymentFailureCode::PublishedPortInUse)
    } else {
        None
    }
}
