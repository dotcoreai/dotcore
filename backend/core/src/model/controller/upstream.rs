//! Keeps the site's upstream model deployments probed, recorded, and routable from the manager.
//!
//! Upstream deployments run at a privately configured endpoint rather than on any node, so no node
//! realizer ever sees them. This manager-side reconciler probes each declared endpoint on its
//! health schedule, records the resulting lifecycle in the model owner's current state, publishes
//! or withdraws its router target, finalizes upstream teardowns, and prunes owner current records
//! whose deployment is no longer declared. Local runtime failures never reach it, so they cannot
//! revoke an admitted upstream route.

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    time::Duration,
};

use dotcore_domain::{
    clock::Clock,
    diagnostic::diagnostic_chain,
    timing::{ReconcileCadence, ReconcileSchedule, ReconciliationState},
};
use dotcore_proto::dotcore::model::v1::{
    CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentStatus,
};
use dotcore_site::repository::SiteRepositoryError;
use futures_util::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, field, info};

use super::{
    DesiredDeploymentRepository, ModelLifecycleError, is_upstream_deployment,
    serving_targets::{ServingTarget, ServingTargetsPublisher, upstream_serving_targets},
    state::{
        current_error, deployment_status_state, desired_error, is_absent, loading_state,
        reconciled_generation,
    },
};
use crate::model::{
    domain::ModelDeploymentId,
    state::{CurrentStateError, CurrentStateRepository},
    upstream::{
        HealthDependencies, HealthJitter, HealthSchedule, ProbeClassification, ProbePacing,
        VerificationHttp, VerificationObservation, VerificationRequest, VerificationTransportError,
    },
};

/// Manager-side reconciler for upstream model deployments.
///
/// It reads the owner's authoritative declarations, writes the owner's current state, and shares
/// the owner's lifecycle operation fence, so a declaration mutation and a stale probe result or
/// publication can never interleave.
pub(super) struct UpstreamReconciler<D, S> {
    desired: D,
    state: S,
    operation: Arc<AsyncMutex<()>>,
    serving_targets_publisher: ServingTargetsPublisher,
    verification_http: Arc<dyn VerificationHttp>,
    clock: Arc<dyn Clock>,

    /// Instant health scheduling counts its time from.
    health_origin: tokio::time::Instant,
    health_jitter: Arc<dyn HealthJitter>,
    pacing: ProbePacing,
    reconcile: ReconcileSchedule,
    health_shutdown: CancellationToken,
    upstream_health: AsyncMutex<BTreeMap<String, UpstreamHealthEntry>>,
    wakeup: Arc<Notify>,
}

impl<D, S> UpstreamReconciler<D, S>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    /// Creates the reconciler and records a Loading state for every declared upstream deployment
    /// that has no current record yet, so the owner can project it before the first probe.
    pub(super) fn new(
        desired: D,
        state: S,
        operation: Arc<AsyncMutex<()>>,
        serving_targets_publisher: ServingTargetsPublisher,
        health: HealthDependencies,
    ) -> Result<Self, ModelLifecycleError> {
        let wakeup = desired.register_change_listener();
        let reconciler = Self {
            desired,
            state,
            operation,
            serving_targets_publisher,
            verification_http: health.verification_http,
            health_origin: health.clock.now(),
            clock: health.clock,
            health_jitter: health.jitter,
            pacing: ProbePacing::new(&health.timing),
            reconcile: health.timing.model_reconcile,
            health_shutdown: CancellationToken::new(),
            upstream_health: AsyncMutex::new(BTreeMap::new()),
            wakeup,
        };
        reconciler.initialize_missing_current_records()?;
        Ok(reconciler)
    }

    /// Runs one complete pass: probes due endpoints, publishes ready targets, finalizes upstream
    /// teardowns, and prunes owner current records of undeclared deployments.
    pub(super) async fn reconcile(&self) -> Result<(), ModelLifecycleError> {
        let span = tracing::debug_span!(
            "model_upstream_reconcile",
            operation = "model_upstream_reconcile",
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let result = self.reconcile_operation().instrument(span.clone()).await;
        match &result {
            Ok(()) => {
                span.record("outcome", "success");
            }
            Err(error) => {
                span.record("outcome", "failed");
                span.record("error_kind", "upstream_reconciliation");
                error!(
                    parent: &span,
                    error_kind = "upstream_reconciliation",
                    error = %diagnostic_chain(error),
                    "upstream model reconciliation failed"
                );
            }
        }
        result
    }

    /// Wakes the reconciliation loop after an owner mutation.
    pub(super) fn wake(&self) {
        self.wakeup.notify_one();
    }

    /// Cancels an in-flight probe so its result is never recorded for a replaced or withdrawn
    /// declaration.
    pub(super) async fn cancel_health(&self, deployment_id: &str) {
        if let Some(entry) = self.upstream_health.lock().await.get(deployment_id) {
            entry.cancellation.cancel();
        }
    }

    /// Sends one owner preflight request through the same verification client as health probes;
    /// dropping the reconciler cancels it.
    pub(super) async fn verify(
        &self,
        request: VerificationRequest,
    ) -> Result<VerificationObservation, VerificationTransportError> {
        self.verification_http
            .verify(request, self.health_shutdown.child_token())
            .await
    }

    /// Publishes ready upstream targets and withdraws the rest, each only while its declaration is
    /// still current under the owner's operation fence.
    pub(super) async fn publish_upstream_serving_targets(
        &self,
        ready: Vec<(ModelDeploymentDeclaration, Option<Vec<ServingTarget>>)>,
    ) -> Result<(), ModelLifecycleError> {
        for (deployment, target) in ready {
            let deployment_id = ModelDeploymentId(deployment.model_deployment_id.clone());
            let _operation = self.operation.lock().await;
            let current = self.declared(&deployment.model_deployment_id)?;
            // Desired mutations and publication share this fence so a stale reconciliation snapshot
            // cannot restore a route after its generation was withdrawn.
            match (
                current.as_ref() == Some(&deployment) && !is_absent(&deployment),
                target,
            ) {
                (true, Some(target)) => self
                    .serving_targets_publisher
                    .upsert_deployment(&deployment_id, target),
                _ => self
                    .serving_targets_publisher
                    .withdraw_deployment(&deployment_id),
            }
        }
        Ok(())
    }

    async fn reconcile_operation(&self) -> Result<(), ModelLifecycleError> {
        let upstreams = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .filter(is_upstream_deployment)
            .collect::<Vec<_>>();
        self.reconcile_upstreams(&upstreams).await?;
        self.sync_upstream_serving_targets(&upstreams).await?;
        self.finalize_teardowns(&upstreams).await?;
        self.remove_stale_current_state().await
    }

    fn initialize_missing_current_records(&self) -> Result<(), ModelLifecycleError> {
        let snapshot = self.state.snapshot().map_err(current_error)?;
        let missing = self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .filter(|deployment| is_upstream_deployment(deployment) && !is_absent(deployment))
            .filter(|deployment| {
                !snapshot
                    .model_deployments
                    .iter()
                    .any(|record| record.model_deployment_id == deployment.model_deployment_id)
            })
            .map(|deployment| CurrentModelDeployment {
                model_deployment_id: deployment.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(deployment.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(loading_state()),
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        self.state
            .prepare(missing, snapshot.storage_revision)
            .map(|_| ())
            .map_err(ModelLifecycleError::from_current_state)
    }

    /// Time since this reconciler's origin on its clock, which health schedules count in.
    fn health_now(&self) -> Duration {
        self.clock
            .now()
            .saturating_duration_since(self.health_origin)
    }

    async fn next_upstream_probe_delay(&self) -> Option<Duration> {
        let now = self.health_now();
        self.upstream_health
            .lock()
            .await
            .values()
            .map(|entry| entry.schedule.next_probe_at().saturating_sub(now))
            .min()
    }

    async fn reconcile_upstreams(
        &self,
        upstreams: &[ModelDeploymentDeclaration],
    ) -> Result<(), ModelLifecycleError> {
        let now = self.health_now();
        let present = upstreams
            .iter()
            .filter(|deployment| !is_absent(deployment))
            .collect::<Vec<_>>();
        let mut requests = Vec::new();
        {
            let mut health = self.upstream_health.lock().await;
            health.retain(|deployment_id, entry| {
                let retained = present.iter().any(|deployment| {
                    deployment.model_deployment_id == *deployment_id
                        && *deployment == &entry.declaration
                });
                if !retained {
                    entry.cancellation.cancel();
                }
                retained
            });
            for deployment in present {
                let entry = health
                    .entry(deployment.model_deployment_id.clone())
                    .or_insert_with(|| UpstreamHealthEntry {
                        declaration: deployment.clone(),
                        schedule: HealthSchedule::new(now, self.pacing),
                        cancellation: self.health_shutdown.child_token(),
                    });
                if entry.declaration != *deployment {
                    entry.cancellation.cancel();
                    *entry = UpstreamHealthEntry {
                        declaration: deployment.clone(),
                        schedule: HealthSchedule::new(now, self.pacing),
                        cancellation: self.health_shutdown.child_token(),
                    };
                }
                if entry.schedule.withdraw_if_stale(now) {
                    self.record_upstream_lifecycle(deployment, ModelDeploymentStatus::Unavailable)?;
                }
                if entry.schedule.due(now) {
                    let Ok(verification) =
                        crate::model::upstream::verification_requests(deployment)
                    else {
                        self.record_upstream_lifecycle(
                            deployment,
                            ModelDeploymentStatus::Unavailable,
                        )?;
                        continue;
                    };
                    requests.push((deployment.clone(), verification, entry.cancellation.clone()));
                }
            }
        }
        let mut probes = futures_util::stream::FuturesUnordered::new();
        for (deployment, verification, cancellation) in requests {
            let verification_http = Arc::clone(&self.verification_http);
            probes.push(async move {
                let mut minimum_retry = None;
                for request in verification {
                    let expected_model_id = request.expected_model_id.clone();
                    let expectation = request.expectation;
                    let result = verification_http
                        .verify(request, cancellation.clone())
                        .await;
                    minimum_retry = minimum_retry.max(
                        result
                            .as_ref()
                            .ok()
                            .and_then(|observation| observation.minimum_retry),
                    );
                    let classification = crate::model::upstream::classify_observation(
                        result,
                        expectation,
                        expected_model_id.as_deref(),
                    );
                    if classification != ProbeClassification::Success {
                        return (deployment, classification, minimum_retry, cancellation);
                    }
                }
                (
                    deployment,
                    ProbeClassification::Success,
                    minimum_retry,
                    cancellation,
                )
            });
        }
        while let Some((deployment, classification, minimum_retry, cancellation)) =
            probes.next().await
        {
            let _operation = self.operation.lock().await;
            if !self
                .current_upstream_probe(&deployment, &cancellation)
                .map_err(desired_error)?
            {
                continue;
            }
            let ready = {
                let mut health = self.upstream_health.lock().await;
                let Some(entry) = health.get_mut(&deployment.model_deployment_id) else {
                    continue;
                };
                if entry.declaration != deployment || entry.cancellation.is_cancelled() {
                    continue;
                }
                entry.schedule.record(
                    now,
                    classification,
                    minimum_retry,
                    self.health_jitter.fraction(&deployment.model_deployment_id),
                );
                entry.schedule.ready()
            };
            let next_lifecycle = if ready {
                ModelDeploymentStatus::Ready
            } else {
                ModelDeploymentStatus::Unavailable
            };
            debug!(
                event_kind = "model_upstream_probe",
                deployment_id = %deployment.model_deployment_id,
                classification = ?classification,
                retry_scheduled = minimum_retry.is_some(),
                "upstream model health probe completed",
            );
            self.record_upstream_lifecycle(&deployment, next_lifecycle)?;
        }
        Ok(())
    }

    async fn sync_upstream_serving_targets(
        &self,
        upstreams: &[ModelDeploymentDeclaration],
    ) -> Result<(), ModelLifecycleError> {
        let ready = {
            let health = self.upstream_health.lock().await;
            upstreams
                .iter()
                .map(|deployment| {
                    let target = (!is_absent(deployment))
                        .then(|| health.get(&deployment.model_deployment_id))
                        .flatten()
                        .filter(|entry| entry.declaration == *deployment && entry.schedule.ready())
                        .map(|_| upstream_serving_targets(deployment))
                        .transpose()?;
                    Ok((deployment.clone(), target))
                })
                .collect::<Result<Vec<_>, ModelLifecycleError>>()?
        };
        self.publish_upstream_serving_targets(ready).await
    }

    /// Removes the current record of each upstream deployment in teardown, then finalizes its
    /// teardown with the owner. Upstream deployments hold no workload, so teardown completes as
    /// soon as the record is gone.
    async fn finalize_teardowns(
        &self,
        upstreams: &[ModelDeploymentDeclaration],
    ) -> Result<(), ModelLifecycleError> {
        let absent = upstreams
            .iter()
            .filter(|deployment| is_absent(deployment))
            .collect::<Vec<_>>();
        if absent.is_empty() {
            return Ok(());
        }
        let _operation = self.operation.lock().await;
        for deployment in absent {
            if self.declared(&deployment.model_deployment_id)?.as_ref() != Some(deployment) {
                continue;
            }
            let record = self
                .state
                .snapshot()
                .map_err(current_error)?
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment.model_deployment_id);
            if let Some(record) = record {
                match self
                    .state
                    .remove(&record.model_deployment_id, record.record_revision)
                {
                    Ok(_) | Err(CurrentStateError::NotFound) => {}
                    // A concurrent record write wins; the next pass retries the teardown.
                    Err(CurrentStateError::RevisionConflict) => continue,
                    Err(error) => return Err(current_error(error)),
                }
            }
            self.serving_targets_publisher
                .withdraw_deployment(&ModelDeploymentId(deployment.model_deployment_id.clone()));
            self.desired
                .finalize_teardown(deployment)
                .map_err(ModelLifecycleError::from_model_repository)?;
            info!(
                event_kind = "model_upstream_teardown_finalized",
                deployment_id = %deployment.model_deployment_id,
                lifecycle_generation = deployment.lifecycle_generation,
                "upstream model deployment teardown finalized",
            );
        }
        Ok(())
    }

    /// Removes current records whose deployment is absent from the complete authoritative
    /// intent. Records of deployments realized on any node stay intact while declared.
    async fn remove_stale_current_state(&self) -> Result<(), ModelLifecycleError> {
        let _operation = self.operation.lock().await;
        self.desired.with_declared_deployment_ids(&mut |declared| {
            let current = self.state.snapshot().map_err(current_error)?;
            for record in current.model_deployments {
                if declared.contains(&record.model_deployment_id) {
                    continue;
                }
                match self
                    .state
                    .remove(&record.model_deployment_id, record.record_revision)
                {
                    Ok(_) => info!(
                        event_kind = "model_stale_current_state_removed",
                        deployment_id = %record.model_deployment_id,
                        "removed current state of an undeclared model deployment",
                    ),
                    Err(CurrentStateError::NotFound | CurrentStateError::RevisionConflict) => {}
                    Err(error) => return Err(current_error(error)),
                }
            }
            Ok(())
        })
    }

    fn declared(
        &self,
        deployment_id: &str,
    ) -> Result<Option<ModelDeploymentDeclaration>, ModelLifecycleError> {
        Ok(self
            .desired
            .model_deployments()
            .map_err(desired_error)?
            .into_iter()
            .find(|candidate| candidate.model_deployment_id == deployment_id))
    }

    fn current_upstream_probe(
        &self,
        deployment: &ModelDeploymentDeclaration,
        cancellation: &CancellationToken,
    ) -> Result<bool, SiteRepositoryError> {
        Ok(!cancellation.is_cancelled()
            && self
                .desired
                .model_deployments()?
                .into_iter()
                .find(|candidate| candidate.model_deployment_id == deployment.model_deployment_id)
                .as_ref()
                == Some(deployment))
    }

    fn record_upstream_lifecycle(
        &self,
        desired: &ModelDeploymentDeclaration,
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
        self.state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: desired.model_deployment_id.clone(),
                    reconciled_lifecycle_generation: Some(desired.lifecycle_generation),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(deployment_status_state(lifecycle, None, None)),
                },
                existing.map(|record| record.record_revision),
            )
            .map(|_| {
                info!(
                    event_kind = "model_upstream_health_transition",
                    deployment_id = %desired.model_deployment_id,
                    next_state = ?lifecycle,
                    "upstream model health changed",
                );
            })
            .map_err(current_error)
    }
}

impl<D, S> UpstreamReconciler<D, S>
where
    D: DesiredDeploymentRepository + 'static,
    S: CurrentStateRepository + 'static,
{
    /// Starts the loop that reconciles after each owner mutation or site change and otherwise
    /// wakes for the next due probe. The loop stops once the reconciler is dropped.
    pub(super) fn start(self: &Arc<Self>) {
        let observer = UpstreamObserver {
            reconciler: Arc::downgrade(self),
            wakeup: Arc::clone(&self.wakeup),
            clock: Arc::clone(&self.clock),
            reconcile: self.reconcile,
        };
        let task_span = tracing::info_span!(
            "model_upstream_observer",
            operation = "model_upstream_observer"
        );
        tokio::spawn(run_observer(observer).instrument(task_span));
    }
}

impl<D, S> Drop for UpstreamReconciler<D, S> {
    fn drop(&mut self) {
        self.health_shutdown.cancel();
    }
}

struct UpstreamHealthEntry {
    declaration: ModelDeploymentDeclaration,
    schedule: HealthSchedule,
    cancellation: CancellationToken,
}

/// What the upstream observer task holds: its reconciler, which it never keeps alive, and the
/// wakeup and timing it waits on.
struct UpstreamObserver<D, S> {
    reconciler: Weak<UpstreamReconciler<D, S>>,
    wakeup: Arc<Notify>,
    clock: Arc<dyn Clock>,
    reconcile: ReconcileSchedule,
}

async fn run_observer<D, S>(observer: UpstreamObserver<D, S>)
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    let UpstreamObserver {
        reconciler,
        wakeup,
        clock,
        reconcile,
    } = observer;
    let mut cadence = ReconcileCadence::new(reconcile);
    loop {
        let Some(reconciler) = reconciler.upgrade() else {
            info!(
                operation = "model_upstream_observer",
                outcome = "stopped",
                state = "dropped",
                "upstream model observer stopped",
            );
            return;
        };
        let state = match reconciler.reconcile().await {
            Ok(()) => ReconciliationState::Settled,
            Err(_) => ReconciliationState::Pending,
        };
        let delay = cadence.next_delay(state).min(
            reconciler
                .next_upstream_probe_delay()
                .await
                .unwrap_or(Duration::MAX),
        );
        drop(reconciler);
        tokio::select! {
            _ = wakeup.notified() => cadence.reset(),
            () = clock.sleep_until(clock.now() + delay) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_proto::dotcore::model::v1::{
        ModelDeploymentDesiredState, ModelDeploymentSnapshot, ModelDeploymentStatus,
        UpstreamModelDesiredConfiguration,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    };
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_util::sync::CancellationToken;

    use super::UpstreamReconciler;
    use crate::model::{
        controller::{
            DesiredDeploymentRepository, InferenceController, ModelLifecycleController,
            ModelRealizationInput,
            serving_targets::{
                ServingTarget, ServingTargetsPublisher, ServingTargetsRevocationReason,
                upstream_serving_targets,
            },
            test_support::{
                TestCurrent, TestDesired, current_deployment, test_desired_deployment,
                upstream_desired_deployment, valid_upstream_desired_deployment,
            },
        },
        domain::ModelDeploymentId,
        state::CurrentStateRepository,
        upstream::{
            HealthDependencies, ScriptedVerificationHttp, StableHealthJitter,
            VerificationObservation, VerificationTransportError,
        },
    };

    #[tokio::test]
    async fn upstream_only_site_without_execution_targets_serves_and_unloads() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let deployment = valid_upstream_desired_deployment();
        {
            let mut site = desired.state.lock().expect("desired state");
            site.execution_targets.clear();
            site.model_deployments.push(deployment.clone());
        }
        let current = TestCurrent::new(calls);
        let verification = Arc::new(ScriptedVerificationHttp::new([Ok(
            VerificationObservation {
                status: 200,
                minimum_retry: None,
                body: br#"{}"#.to_vec(),
            },
        )]));
        let controller = InferenceController::new_with_verification(
            desired.clone(),
            current.clone(),
            verification,
        )
        .expect("upstream-only controller");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream reconciliation");
        let sources = controller.serving_targets();
        let (upstream_model_id, lease) = sources
            .admit(|targets| {
                let target = targets.first().expect("published upstream target").clone();
                let ServingTarget::Upstream(upstream) = &target else {
                    panic!("upstream-only site must publish an upstream target");
                };
                let upstream_model_id = upstream.dispatch().upstream_model_id.clone();
                Ok::<_, ()>((target, upstream_model_id))
            })
            .expect("upstream selection")
            .expect("upstream lease");
        assert_eq!(upstream_model_id, "gpt-4o");
        assert_eq!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .iter()
                .map(|record| (
                    record.model_deployment_id.as_str(),
                    record.state.as_ref().map(|state| state.status)
                ))
                .collect::<Vec<_>>(),
            [(
                deployment.model_deployment_id.as_str(),
                Some(ModelDeploymentStatus::Ready as i32)
            )]
        );

        controller
            .unload_model(ModelDeploymentId(deployment.model_deployment_id.clone()))
            .await
            .expect("unload upstream deployment");
        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream teardown reconciliation");

        assert!(sources.snapshot().targets.is_empty());
        assert_eq!(
            *lease.into_parts().1.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .is_empty()
        );
        assert_eq!(
            desired.model_deployments().expect("desired state"),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn cancelled_or_replaced_upstream_probe_is_not_current() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let deployment = upstream_desired_deployment();
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(deployment.clone());
        let inner = test_reconciler(desired.clone(), TestCurrent::new(calls));
        let cancellation = CancellationToken::new();

        assert!(
            inner
                .current_upstream_probe(&deployment, &cancellation)
                .expect("current probe")
        );
        cancellation.cancel();
        assert!(
            !inner
                .current_upstream_probe(&deployment, &cancellation)
                .expect("cancelled probe")
        );

        let replacement = UpstreamModelDesiredConfiguration {
            api_key: "replacement-key".to_owned(),
            ..Default::default()
        };
        desired
            .update_upstream_model(&deployment.model_deployment_id, &replacement)
            .expect("replace upstream declaration");
        assert!(
            !inner
                .current_upstream_probe(&deployment, &CancellationToken::new())
                .expect("replaced probe")
        );
    }

    #[tokio::test]
    async fn verified_upstream_publication_is_generation_scoped_and_failed_replacement_withdraws() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let deployment = valid_upstream_desired_deployment();
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(deployment.clone());
        let verification = Arc::new(ScriptedVerificationHttp::new([
            Ok(VerificationObservation {
                status: 200,
                minimum_retry: None,
                body: br#"{"id":"gpt-4o"}"#.to_vec(),
            }),
            Ok(VerificationObservation {
                status: 200,
                minimum_retry: None,
                body: br#"{"id":"replacement-model"}"#.to_vec(),
            }),
            Err(VerificationTransportError::Network),
        ]));
        let controller = InferenceController::new_with_verification(
            desired.clone(),
            TestCurrent::new(calls),
            verification.clone(),
        )
        .expect("controller construction");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("initial reconciliation");
        let sources = controller.serving_targets();
        let published = sources.snapshot();
        let ServingTarget::Upstream(target) = &published.targets[0] else {
            panic!("verified deployment must publish an upstream target");
        };
        let first_generation = target.reconcile_generation;
        let lease = sources
            .lease_for(&published.targets[0])
            .expect("target lease");

        let mut replacement = deployment
            .upstream_desired_configuration
            .clone()
            .expect("upstream configuration");
        replacement.upstream_model_id = "replacement-model".to_owned();
        replacement.api_key = "replacement-key".to_owned();
        desired
            .update_upstream_model("upstream-deployment", &replacement)
            .expect("replacement desired configuration");
        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("replacement reconciliation");
        let replacement = sources.snapshot();
        let replacement_target = replacement.targets[0].clone();
        let ServingTarget::Upstream(replacement) = &replacement_target else {
            panic!("verified replacement must publish an upstream target");
        };
        assert_ne!(first_generation, replacement.reconcile_generation);
        let replacement_lease = sources
            .lease_for(&replacement_target)
            .expect("replacement target lease");
        assert_eq!(
            *lease.into_parts().1.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );

        let mut unavailable = deployment
            .upstream_desired_configuration
            .clone()
            .expect("upstream configuration");
        unavailable.upstream_model_id = "unavailable-model".to_owned();
        unavailable.api_key = "unavailable-key".to_owned();
        desired
            .update_upstream_model("upstream-deployment", &unavailable)
            .expect("unavailable desired configuration");
        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("unavailable reconciliation");

        assert!(sources.snapshot().targets.is_empty());
        assert_eq!(
            *replacement_lease.into_parts().1.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(verification.requests().len(), 3);
    }

    #[tokio::test]
    async fn stale_upstream_publication_is_fenced_after_update_and_unload() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let deployment = valid_upstream_desired_deployment();
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(deployment.clone());
        let verification = Arc::new(ScriptedVerificationHttp::new([
            Ok(VerificationObservation {
                status: 200,
                minimum_retry: None,
                body: br#"{"id":"gpt-4o"}"#.to_vec(),
            }),
            Ok(VerificationObservation {
                status: 200,
                minimum_retry: None,
                body: br#"{"id":"replacement-model"}"#.to_vec(),
            }),
        ]));
        let controller = InferenceController::new_with_verification(
            desired.clone(),
            TestCurrent::new(calls),
            verification,
        )
        .expect("controller construction");
        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("initial reconciliation");
        assert!(matches!(
            controller.serving_targets().snapshot().targets.as_slice(),
            [ServingTarget::Upstream(target)]
                if target.dispatch().upstream_model_id == "gpt-4o"
        ));

        let stale = vec![{
            let target = upstream_serving_targets(&deployment).expect("stale upstream target");
            (deployment.clone(), Some(target))
        }];
        let mut replacement = deployment
            .upstream_desired_configuration
            .clone()
            .expect("upstream configuration");
        replacement.upstream_model_id = "replacement-model".to_owned();
        replacement.api_key = "replacement-key".to_owned();
        controller
            .update_upstream_model(
                ModelDeploymentId(deployment.model_deployment_id.clone()),
                replacement,
            )
            .await
            .expect("update upstream deployment");

        controller
            .inner
            .upstream
            .publish_upstream_serving_targets(stale)
            .await
            .expect("stale publication is ignored");
        assert!(controller.serving_targets().snapshot().targets.is_empty());

        let replacement = desired
            .model_deployments()
            .expect("replacement desired declaration")
            .into_iter()
            .next()
            .expect("replacement deployment");
        let stale = vec![{
            let target = upstream_serving_targets(&replacement).expect("stale replacement target");
            (replacement, Some(target))
        }];
        controller
            .unload_model(ModelDeploymentId(deployment.model_deployment_id.clone()))
            .await
            .expect("unload upstream deployment");

        controller
            .inner
            .upstream
            .publish_upstream_serving_targets(stale)
            .await
            .expect("stale publication is ignored");
        assert!(controller.serving_targets().snapshot().targets.is_empty());
    }

    #[tokio::test]
    async fn upstream_only_snapshot_removes_stale_non_upstream_current_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(valid_upstream_desired_deployment());
        let current = TestCurrent::new(calls);
        current
            .upsert(current_deployment("stale-local-deployment"), None)
            .expect("stale current deployment");
        let verification = Arc::new(ScriptedVerificationHttp::new([]));
        let controller =
            InferenceController::new_with_verification(desired, current.clone(), verification)
                .expect("controller");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream-only reconciliation");

        assert_eq!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .into_iter()
                .map(|record| record.model_deployment_id)
                .collect::<Vec<_>>(),
            vec!["upstream-deployment"]
        );
    }

    #[tokio::test]
    async fn unloading_an_upstream_only_deployment_removes_its_current_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let deployment = upstream_desired_deployment();
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(deployment.clone());
        let current = TestCurrent::new(calls.clone());
        let controller = InferenceController::new_with_verification(
            desired.clone(),
            current.clone(),
            Arc::new(ScriptedVerificationHttp::new([])),
        )
        .expect("model owner");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream reconciliation");
        assert_eq!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .iter()
                .map(|record| record.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            [deployment.model_deployment_id.as_str()]
        );

        controller
            .unload_model(ModelDeploymentId(deployment.model_deployment_id.clone()))
            .await
            .expect("unload upstream deployment");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream reconciliation");
        assert!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .is_empty()
        );
    }

    #[tokio::test]
    async fn upstream_only_declarations_are_recorded_by_the_owner_without_any_runtime() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let mut upstream =
            test_desired_deployment("upstream-deployment", ModelDeploymentDesiredState::Present);
        upstream.placement = None;
        upstream.requested_parallel_shape = 0;
        upstream.snapshot = Some(ModelDeploymentSnapshot {
            source: Some(ModelDeploymentSnapshotSource::Upstream(Default::default())),
        });
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(upstream);
        let controller = InferenceController::new_with_verification(
            desired.clone(),
            TestCurrent::new(calls),
            Arc::new(ScriptedVerificationHttp::new([])),
        )
        .expect("model owner");

        controller
            .inner
            .upstream
            .reconcile()
            .await
            .expect("upstream reconciliation");
    }

    fn test_reconciler(
        desired: TestDesired,
        state: TestCurrent,
    ) -> UpstreamReconciler<TestDesired, TestCurrent> {
        UpstreamReconciler::new(
            desired,
            state,
            Arc::new(AsyncMutex::new(())),
            ServingTargetsPublisher::new().0,
            HealthDependencies {
                verification_http: Arc::new(ScriptedVerificationHttp::new([])),
                clock: Arc::new(dotcore_domain::clock::SystemClock),
                jitter: Arc::new(StableHealthJitter),
                timing: dotcore_domain::timing::Timing::production(),
            },
        )
        .expect("upstream reconciler")
    }
}
