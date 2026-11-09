//! Builds the manager's model owner and a node's model realizer, validating
//! their inputs before any lifecycle work starts.
//!
//! The owner is built over declarations and the owner's current state only and
//! rejects a current state that belongs to another site. A realizer is built
//! over its node input, node store, and runtime targets, and rejects a
//! mismatched site ID and malformed runtime observations, so no lifecycle work
//! ever starts from a misconfigured site or runtime.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use dotcore_artifact::{ArtifactManager, OciImagePreparer};
use dotcore_domain::{clock::Clock, timing::ModelTiming};
#[cfg(test)]
use dotcore_domain::{clock::SystemClock, timing::Timing};
use dotcore_proto::dotcore::{
    model::v1::{CurrentModelDeployment, ModelDeploymentLifecycleTransition},
    site::v1::Site,
};
use dotcore_site::{
    node::target_inventory::TargetCapabilitySource, repository::SiteRepositoryError,
    store::FileNodeState,
};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

use super::{
    InferenceController, InferenceControllerInner, ModelLifecycleError,
    ownership::{validate_desired, validate_observations},
    state::{configuration, configuration_with_source, is_absent, observation_identity},
};
use super::{
    reconciliation::paused_initial_state,
    serving_targets::{LocalServingTargetsPublisher, ServingTargetsPublisher},
    state::{current_error, desired_state, state_from_observation, unloading_state},
};
use crate::{
    dispatch::model::{RuntimeTargetMap, TargetObservation},
    model::state::CurrentStateRepository,
};

#[expect(
    private_bounds,
    reason = "the public selected controller alias retains private owner-specific repository ports"
)]
impl<D, S> InferenceController<D, S>
where
    D: super::DesiredDeploymentRepository + Clone + 'static,
    S: CurrentStateRepository + Clone + 'static,
{
    /// Creates the manager-owned lifecycle authority over `desired` declarations and the owner's
    /// current `state`, after checking that both belong to the same site. It runs `timing` and
    /// waits through `clock`.
    pub(crate) fn new(
        desired: D,
        state: S,
        target_capabilities: Arc<dyn TargetCapabilitySource>,
        timing: ModelTiming,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ModelLifecycleError> {
        let verification_http = Arc::new(
            crate::model::upstream::ReqwestVerificationHttp::new().map_err(|source| {
                configuration_with_source("failed to create upstream verification client", source)
            })?,
        );
        Self::new_with_health(
            desired,
            state,
            target_capabilities,
            health(verification_http),
            timing,
            clock,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_verification(
        desired: D,
        state: S,
        verification_http: Arc<dyn crate::model::upstream::VerificationHttp>,
    ) -> Result<Self, ModelLifecycleError> {
        Self::new_with_health(
            desired,
            state,
            crate::model::controller::test_support::test_target_capability_source(),
            health(verification_http),
            Timing::test().model,
            Arc::new(SystemClock),
        )
    }

    fn new_with_health(
        desired: D,
        state: S,
        target_capabilities: Arc<dyn TargetCapabilitySource>,
        health: crate::model::upstream::HealthDependencies,
        timing: ModelTiming,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ModelLifecycleError> {
        let declared_site_id = desired
            .declaration_snapshot()
            .map_err(super::state::desired_error)?
            .site_id;
        if state.snapshot().map_err(current_error)?.site_id != declared_site_id {
            return Err(configuration(
                "current-state site ID does not match desired site",
            ));
        }
        let operation = Arc::new(AsyncMutex::new(()));
        let (serving_targets_publisher, serving_targets_source) = ServingTargetsPublisher::new();
        let upstream = super::upstream::UpstreamReconciler::new(
            desired.clone(),
            state.clone(),
            Arc::clone(&operation),
            serving_targets_publisher.clone(),
            health,
            timing,
            Arc::clone(&clock),
        )?;
        Ok(InferenceController {
            inner: Arc::new(super::ManagerInferenceControllerInner {
                desired,
                state,
                target_capabilities,
                operation,
                observer_started: std::sync::Mutex::new(false),
                serving_targets_publisher,
                serving_targets_source,
                upstream: Arc::new(upstream),
                remote_status: std::sync::RwLock::new(None),
                node_logs: std::sync::RwLock::new(None),
                site_workloads: std::sync::RwLock::new(None),
                timing,
                clock,
            }),
        })
    }
}

fn build_realizer<D, S, R>(
    site: Site,
    boundary: ModelRealizationBoundary<D, R>,
    state: S,
    runtimes: RuntimeTargetMap,
    observations: Vec<TargetObservation>,
    artifact_cache_path: PathBuf,
    dependencies: ControllerDependencies,
) -> Result<super::ModelRealizer<D, S, R>, ModelLifecycleError>
where
    D: super::ModelRealizationInput + 'static,
    S: CurrentStateRepository + 'static,
    R: super::ModelRealizationResults + 'static,
{
    let ModelRealizationBoundary {
        input: desired,
        results,
    } = boundary;
    let wakeup = desired.register_change_listener();
    let snapshot = state.snapshot().map_err(current_error)?;
    if snapshot.site_id != site.site_id {
        return Err(configuration(
            "current-state site ID does not match desired site",
        ));
    }
    validate_runtime_inputs(&site, &runtimes, &observations)?;
    let artifacts = ArtifactManager::new(artifact_cache_path, dependencies.artifacts.oci_preparer)?
        .with_nix_sources(dependencies.artifacts.nix);
    let missing = missing_current_records(
        &site,
        &snapshot.model_deployments,
        &observations,
        &artifacts,
    )?;
    state
        .prepare(missing, snapshot.storage_revision)
        .map_err(ModelLifecycleError::from_current_state)?;
    let serving_targets_publisher = dependencies.serving_targets;
    let serving_targets_source = serving_targets_publisher.source();
    let accelerator_attribution = dependencies.accelerator_attribution;
    if let Some(accelerator_attribution) = &accelerator_attribution {
        for target_observation in &observations {
            if let Some(workload) = observation_identity(&target_observation.observation) {
                // Runtime memory is read from the next reconciled observation, which names its
                // target.
                accelerator_attribution.observe(
                    &workload.correlation.owner_id,
                    None,
                    &target_observation.observation,
                );
            }
        }
    }
    let realizer = super::ModelRealizer {
        inner: Arc::new(InferenceControllerInner {
            results,
            desired,
            state,
            runtimes,
            target_capabilities: dependencies.target_capabilities,
            engine_integrations: dependencies.engine_integrations,
            operation: Arc::new(AsyncMutex::new(())),
            reconciliation: AsyncMutex::new(()),
            preparations: super::preparation::PreparationCoordinator::from_deployments(
                &site.model_deployments,
            )?,
            observer_started: std::sync::Mutex::new(false),
            wakeup,
            artifacts,
            preparation_logs: dependencies.preparation_logs,
            workload_lifecycle: dependencies.workload_lifecycle,
            serving_targets_publisher,
            serving_targets_source,
            accelerator_attribution,
            shutdown: dependencies.shutdown,
            timing: dependencies.timing,
            clock: dependencies.clock,
        }),
    };
    Ok(realizer)
}

pub(super) fn missing_current_records(
    site: &Site,
    current: &[CurrentModelDeployment],
    observations: &[TargetObservation],
    artifacts: &ArtifactManager,
) -> Result<Vec<CurrentModelDeployment>, ModelLifecycleError> {
    let current_records = current
        .iter()
        .map(|deployment| (deployment.model_deployment_id.as_str(), deployment))
        .collect::<BTreeMap<_, _>>();
    let mut missing = Vec::new();
    for desired_deployment in &site.model_deployments {
        let existing = current_records.get(desired_deployment.model_deployment_id.as_str());
        if is_absent(desired_deployment) {
            if existing
                .and_then(|record| record.state.as_ref())
                .is_some_and(|state| {
                    matches!(
                        dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::try_from(
                            state.status,
                        ),
                        Ok(
                            dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Unloading
                                | dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Failed
                        )
                    )
                })
            {
                continue;
            }
            missing.push(CurrentModelDeployment {
                model_deployment_id: desired_deployment.model_deployment_id.clone(),
                reconciled_lifecycle_generation: Some(desired_deployment.lifecycle_generation),
                record_revision: 0,
                recovery_barrier: None,
                state: Some(unloading_state(
                    existing.and_then(|record| record.state.as_ref()),
                )),
            });
            continue;
        }
        if existing.is_some() {
            continue;
        }
        let observation = observations
            .iter()
            .find(|target_observation| {
                observation_identity(&target_observation.observation).is_some_and(|workload| {
                    workload.correlation.owner_id == desired_deployment.model_deployment_id
                })
            })
            .cloned();
        let initial_state = if matches!(
            desired_state(desired_deployment)?,
            dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused
        ) {
            paused_initial_state(site, artifacts, desired_deployment, observation)?
        } else if let Some(observed) = observation
            .as_ref()
            .and_then(|target_observation| state_from_observation(&target_observation.observation))
        {
            observed
        } else {
            match ModelDeploymentLifecycleTransition::try_from(
                desired_deployment.lifecycle_transition,
            ) {
                Ok(ModelDeploymentLifecycleTransition::Create)
                    if !desired_deployment.lifecycle_recovery_required =>
                {
                    super::state::first_realization_pending_state()
                }
                Ok(ModelDeploymentLifecycleTransition::Resume) => {
                    warn!(
                        event_kind = "model_lifecycle_problem",
                        deployment_id = %desired_deployment.model_deployment_id,
                        lifecycle_generation = desired_deployment.lifecycle_generation,
                        error_kind = "resume_recovery_required",
                        "Resume has no retained or observed operational state; the deployment requires recovery",
                    );
                    super::state::resume_recovery_required_state()
                }
                _ => super::state::loading_state(),
            }
        };
        missing.push(CurrentModelDeployment {
            model_deployment_id: desired_deployment.model_deployment_id.clone(),
            reconciled_lifecycle_generation: Some(desired_deployment.lifecycle_generation),
            record_revision: 0,
            recovery_barrier: None,
            state: Some(initial_state),
        });
    }
    Ok(missing)
}

impl<D> super::ModelRealizer<D, FileNodeState, super::DurableModelRealizationResults>
where
    D: super::ModelRealizationInput + 'static,
{
    /// Constructs a node-local reconciler from projected owner input without manager repositories.
    pub(crate) fn new_node_realizer(
        node: NodeModelRealizerDependencies<D>,
        state: FileNodeState,
        runtimes: RuntimeTargetMap,
        observations: Vec<TargetObservation>,
        artifact_cache_path: PathBuf,
    ) -> Result<Self, ModelLifecycleError> {
        let input = node.input;
        let site = site_with_deployments(&input, &runtimes).map_err(super::state::desired_error)?;
        let results = super::DurableModelRealizationResults::new(
            state.clone(),
            node.preparation_logs.clone(),
            node.workload_lifecycle.clone(),
        );
        build_realizer(
            site,
            ModelRealizationBoundary { input, results },
            state,
            runtimes,
            observations,
            artifact_cache_path,
            ControllerDependencies {
                artifacts: node.composition.artifacts,
                target_capabilities: node.composition.target_capabilities,
                engine_integrations: node.composition.engine_integrations,
                accelerator_attribution: node.composition.accelerator_attribution,
                serving_targets: node.serving_targets,
                shutdown: node.shutdown,
                preparation_logs: node.preparation_logs,
                workload_lifecycle: node.workload_lifecycle,
                timing: node.timing,
                clock: node.clock,
            },
        )
    }
}

#[cfg(test)]
impl<D, S> super::ModelRealizer<D, S, D>
where
    D: super::ModelRealizationInput + super::ModelRealizationResults + Clone + 'static,
    S: CurrentStateRepository + 'static,
{
    /// Builds a node realizer whose test input also confirms cleanup, with test target
    /// capabilities and engine integrations.
    pub(crate) fn for_test(
        input: D,
        state: S,
        runtimes: RuntimeTargetMap,
        observations: Vec<TargetObservation>,
        artifact_cache_path: PathBuf,
        oci_preparer: Option<Arc<dyn OciImagePreparer>>,
    ) -> Result<Self, ModelLifecycleError> {
        Self::for_test_with_dependencies(
            input,
            state,
            runtimes,
            observations,
            artifact_cache_path,
            ModelControllerDependencies {
                artifacts: ArtifactDependencies {
                    oci_preparer,
                    ..Default::default()
                },
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
    }

    /// Builds a node realizer whose test input also confirms cleanup, with explicit dependencies.
    pub(crate) fn for_test_with_dependencies(
        input: D,
        state: S,
        runtimes: RuntimeTargetMap,
        observations: Vec<TargetObservation>,
        artifact_cache_path: PathBuf,
        composition: ModelControllerDependencies,
    ) -> Result<Self, ModelLifecycleError> {
        let site = site_with_deployments(&input, &runtimes).map_err(super::state::desired_error)?;
        let results = input.clone();
        // Test realizers keep their preparation logs beside their artifact cache.
        let preparation_logs =
            crate::deployment_logs::PreparationLogs::on_system_clock(&artifact_cache_path);
        build_realizer(
            site,
            ModelRealizationBoundary { input, results },
            state,
            runtimes,
            observations,
            artifact_cache_path,
            ControllerDependencies {
                artifacts: composition.artifacts,
                target_capabilities: composition.target_capabilities,
                engine_integrations: composition.engine_integrations,
                accelerator_attribution: composition.accelerator_attribution,
                serving_targets: LocalServingTargetsPublisher::new().0,
                shutdown: tokio_util::sync::CancellationToken::new(),
                preparation_logs,
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            },
        )
    }
}

#[derive(Clone, Default)]
pub(crate) struct ArtifactDependencies {
    pub(crate) oci_preparer: Option<Arc<dyn OciImagePreparer>>,

    /// Development source this node pins and builds development-source Nix engine artifacts from.
    pub(crate) nix: dotcore_artifact::NixSources,
}

struct ModelRealizationBoundary<D, R> {
    input: D,
    results: R,
}

pub(crate) struct NodeModelRealizerDependencies<D: super::ModelRealizationInput> {
    pub(crate) input: D,
    pub(crate) composition: ModelControllerDependencies,
    /// Node-lifetime serving-target view, shared with status reporting and private dispatch.
    pub(crate) serving_targets: LocalServingTargetsPublisher,

    /// Cancelled when the node shuts its realizer down.
    pub(crate) shutdown: tokio_util::sync::CancellationToken,

    /// Preparation logs the node keeps for its deployments, removed with their cleanup.
    pub(crate) preparation_logs: crate::deployment_logs::PreparationLogs,

    /// Tells the node's deployment-log follows when a deployment's engine workload starts or is
    /// removed.
    pub(crate) workload_lifecycle: crate::deployment_logs::WorkloadLifecycle,

    /// Timing of the realizer's observer passes and of its stored download progress.
    pub(crate) timing: ModelTiming,

    /// Backend clock the realizer waits through.
    pub(crate) clock: Arc<dyn Clock>,
}

#[derive(Clone)]
pub(crate) struct ModelControllerDependencies {
    pub(crate) artifacts: ArtifactDependencies,
    pub(crate) target_capabilities: Arc<dyn TargetCapabilitySource>,
    pub(crate) engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
    /// Memory attribution and resource telemetry of the realized deployments; shared with the
    /// node's status reporting on a hosting node.
    pub(crate) accelerator_attribution: Option<super::AcceleratorAttribution>,
}

struct ControllerDependencies {
    artifacts: ArtifactDependencies,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
    accelerator_attribution: Option<super::AcceleratorAttribution>,
    serving_targets: LocalServingTargetsPublisher,
    shutdown: tokio_util::sync::CancellationToken,
    preparation_logs: crate::deployment_logs::PreparationLogs,
    workload_lifecycle: crate::deployment_logs::WorkloadLifecycle,
    timing: ModelTiming,
    clock: Arc<dyn Clock>,
}

/// Upstream health dependencies over `verification_http`, with stable per-deployment jitter.
fn health(
    verification_http: Arc<dyn crate::model::upstream::VerificationHttp>,
) -> crate::model::upstream::HealthDependencies {
    crate::model::upstream::HealthDependencies {
        verification_http,
        jitter: Arc::new(crate::model::upstream::StableHealthJitter),
    }
}

/// Projects the controller input assigned to the configured process-local runtime pools. Only
/// deployments placed on those pools remain, so upstream deployments never reach a node realizer.
pub(super) fn project_local_site(mut site: Site, runtimes: &RuntimeTargetMap) -> Site {
    site.execution_targets
        .retain(|target| runtimes.runs_target(&target.execution_target_id));
    site.model_deployments.retain(|deployment| {
        deployment
            .placement
            .as_ref()
            .is_some_and(|placement| runtimes.runs_target(&placement.execution_target_id))
    });
    site
}

pub(super) fn site_with_deployments<D: super::ModelRealizationInput>(
    desired: &D,
    runtimes: &RuntimeTargetMap,
) -> Result<Site, SiteRepositoryError> {
    let snapshot = desired.declaration_snapshot()?;
    Ok(project_local_site(
        Site {
            site_id: snapshot.site_id,
            execution_targets: snapshot.execution_targets,
            model_deployments: snapshot.deployments,
            ..Default::default()
        },
        runtimes,
    ))
}

/// Validates desired deployments and discovered observations before controller construction.
pub(super) fn validate_runtime_inputs(
    site: &Site,
    runtimes: &RuntimeTargetMap,
    observations: &[TargetObservation],
) -> Result<(), ModelLifecycleError> {
    let configured = site
        .execution_targets
        .iter()
        .map(|target| target.execution_target_id.as_str())
        .collect::<BTreeSet<_>>();
    let injected = runtimes.target_ids().collect::<BTreeSet<_>>();
    if configured != injected {
        return Err(configuration(
            "execution-target runtime mappings do not match desired site targets",
        ));
    }
    validate_desired(site, runtimes)?;
    validate_observations(site, runtimes, observations)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_domain::{clock::SystemClock, timing::Timing};
    use dotcore_proto::dotcore::model::v1::{
        ModelDeploymentActivation, ModelDeploymentActivationPhase, ModelDeploymentDesiredState,
        ModelDeploymentLifecycleTransition, ModelDeploymentStatus,
    };
    use dotcore_runtime::WorkloadObservation;
    use dotcore_site::{
        repository::ModelSiteSnapshot, store::FileNodeState, view::fencing::NodeViewProjection,
    };

    use super::{
        ArtifactDependencies, InferenceController, site_with_deployments, validate_runtime_inputs,
    };
    use crate::{
        dispatch::model::TargetObservation,
        model::{
            controller::{
                ModelRealizationInput,
                test_support::{
                    TestCurrent, TestDesired, current_deployment, expected_identity,
                    test_desired_deployment, test_runtime_target, test_site,
                    valid_upstream_desired_deployment,
                },
            },
            state::CurrentStateRepository,
        },
    };

    #[test]
    fn owner_rejects_current_state_of_another_site() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.state.lock().expect("desired state").site_id = "another-site".to_owned();

        let error = InferenceController::new(
            desired,
            TestCurrent::new(calls),
            crate::model::controller::test_support::test_target_capability_source(),
            Timing::test().model,
            Arc::new(SystemClock),
        )
        .err()
        .expect("mismatched owner state");

        assert_eq!(
            error.to_string(),
            "Model lifecycle configuration failed: current-state site ID does not match desired site"
        );
    }

    #[test]
    fn controller_input_projects_only_deployments_for_configured_local_targets() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls);
        let mut node_deployment =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        node_deployment.model_deployment_id = "worker-deployment".to_owned();
        let node_placement = node_deployment.placement.as_mut().expect("placement");
        node_placement.site_node_id = "worker-node".to_owned();
        node_placement.execution_target_id = "worker-target".to_owned();
        {
            let mut site = desired.state.lock().expect("desired state");
            let mut node_target = site.execution_targets[0].clone();
            node_target.execution_target_id = "worker-target".to_owned();
            node_target.site_node_id = "worker-node".to_owned();
            site.execution_targets.push(node_target);
            site.model_deployments.push(test_desired_deployment(
                "fixture-model",
                ModelDeploymentDesiredState::Present,
            ));
            site.model_deployments.push(node_deployment);
        }

        let local = site_with_deployments(&desired, &test_runtime_target())
            .expect("node-scoped controller input");

        assert_eq!(
            local
                .execution_targets
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<Vec<_>>(),
            vec!["target-one"]
        );
        assert_eq!(
            local
                .model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["deployment"]
        );
        validate_runtime_inputs(&local, &test_runtime_target(), &[])
            .expect("configured local target set");
    }

    #[test]
    fn upstream_deployments_never_reach_a_node_realizer() {
        let runtimes = test_runtime_target();
        let mut site = test_site();
        site.model_deployments
            .push(valid_upstream_desired_deployment());

        let projected = super::project_local_site(site, &runtimes);

        assert_eq!(projected.model_deployments, Vec::new());
    }

    #[tokio::test]
    async fn node_controller_does_not_probe_or_persist_upstream_deployments() {
        let mut site = test_site();
        site.model_deployments
            .push(valid_upstream_desired_deployment());
        let projection =
            NodeViewProjection::for_site_node(&site, "node-one").expect("node projection");
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let current = FileNodeState::open(
            directory.path().join("node"),
            site.site_id.clone(),
            "node-one",
        )
        .expect("node current state");
        let controller = super::super::ModelRealizer::new_node_realizer(
            node_realizer(site.site_id, projection, directory.path()),
            current.clone(),
            test_runtime_target(),
            Vec::new(),
            directory.path().join("cache"),
        )
        .expect("node controller");

        controller.inner.reconcile().await.expect("reconciliation");

        assert_eq!(
            current.snapshot().expect("current state").model_deployments,
            Vec::new()
        );
    }

    #[test]
    fn missing_record_initialization_preserves_existing_resume_state_and_revision() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired_input = TestDesired::new(calls.clone());
        let mut desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        desired.model_deployment_id = "deployment".to_owned();
        desired.lifecycle_generation = 7;
        desired.lifecycle_transition = ModelDeploymentLifecycleTransition::Resume as i32;
        desired_input
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(desired);

        let current = TestCurrent::new(calls);
        let mut resume = current_deployment("deployment");
        let state = resume.state.as_mut().expect("current state");
        state.status_message = "Resuming retained workload".to_owned();
        state.activation = Some(ModelDeploymentActivation {
            phase: ModelDeploymentActivationPhase::StartInFlight as i32,
            workload_generation: Some(4),
            lifecycle_generation: 7,
        });
        current.upsert(resume, None).expect("resume current state");
        let before = current.snapshot().expect("state before construction");

        crate::model::controller::test_support::TestRealizer::for_test(
            desired_input,
            current.clone(),
            test_runtime_target(),
            Vec::new(),
            "/test-cache".into(),
            None,
        )
        .expect("controller construction");

        let after = current.snapshot().expect("state after construction");
        assert_eq!(after.model_deployments, before.model_deployments);
        assert_eq!(after.storage_revision, before.storage_revision + 1);
    }

    #[tokio::test]
    async fn node_pass_fails_only_a_resume_without_state_or_workload() {
        let site = test_site();
        let projection =
            NodeViewProjection::for_site_node(&site, "node-one").expect("node projection");
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let node = node_realizer(site.site_id.clone(), projection, directory.path());
        let input = node.input.clone();
        let current = FileNodeState::open(
            directory.path().join("node"),
            site.site_id.clone(),
            "node-one",
        )
        .expect("node current state");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let controller = super::super::ModelRealizer::new_node_realizer(
            node,
            current.clone(),
            recording_runtime_target(&calls),
            Vec::new(),
            directory.path().join("cache"),
        )
        .expect("node controller");

        input.replace(ModelSiteSnapshot {
            site_id: site.site_id.clone(),
            execution_targets: site.execution_targets.clone(),
            deployments: vec![lost_teardown_resumed(), paused_sibling()],
        });
        for _ in 0..2 {
            controller
                .inner
                .reconcile()
                .await
                .expect("node reconciliation");
        }

        assert_recovery_required_only_for_the_lost_resume(&current);
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn realizer_construction_fails_only_a_resume_without_state_or_workload() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .extend([lost_teardown_resumed(), paused_sibling()]);
        let current = TestCurrent::new(calls.clone());

        let controller = crate::model::controller::test_support::TestRealizer::for_test(
            desired.clone(),
            current.clone(),
            recording_runtime_target(&calls),
            Vec::new(),
            "/test-cache".into(),
            None,
        )
        .expect("manager controller");
        assert_recovery_required_only_for_the_lost_resume(&current);
        controller
            .inner
            .reconcile()
            .await
            .expect("manager reconciliation");

        assert_recovery_required_only_for_the_lost_resume(&current);
        assert!(
            calls
                .lock()
                .expect("calls")
                .iter()
                .all(|call| !matches!(*call, "create" | "start" | "stop" | "remove"))
        );
    }

    /// Present intent at an explicit Resume generation whose deployment has neither retained
    /// state nor a workload, as after the manager recovered from a lost teardown.
    fn lost_teardown_resumed() -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
        let mut deployment =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        deployment.model_deployment_id = "lost-teardown".to_owned();
        deployment.lifecycle_generation = 3;
        deployment.lifecycle_transition = ModelDeploymentLifecycleTransition::Resume as i32;
        deployment
    }

    fn paused_sibling() -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
        let mut deployment =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Paused);
        deployment.model_deployment_id = "paused-sibling".to_owned();
        deployment
    }

    fn assert_recovery_required_only_for_the_lost_resume(current: &impl CurrentStateRepository) {
        let records = current
            .snapshot()
            .expect("current state")
            .model_deployments
            .into_iter()
            .map(|record| (record.model_deployment_id.clone(), record))
            .collect::<std::collections::BTreeMap<_, _>>();
        let lost = &records["lost-teardown"];
        let lost_state = lost.state.as_ref().expect("recovery state");
        assert_eq!(lost.reconciled_lifecycle_generation, Some(3));
        assert_eq!(lost_state.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(lost_state.status_message, "Recovery required");
        assert_eq!(lost_state.activation, None);
        assert_eq!(
            lost_state.failure.as_ref().expect("failure").code,
            "resume_recovery_required"
        );
        let sibling = &records["paused-sibling"];
        assert_eq!(sibling.reconciled_lifecycle_generation, Some(1));
        assert_eq!(
            sibling.state.as_ref().expect("sibling state").status,
            ModelDeploymentStatus::Unavailable as i32
        );
    }

    fn recording_runtime_target(
        calls: &Arc<Mutex<Vec<&'static str>>>,
    ) -> crate::dispatch::model::RuntimeTargetMap {
        let mut builder = crate::dispatch::model::RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "target-one",
                crate::model::controller::test_support::RecordingRuntime {
                    calls: Arc::clone(calls),
                    virtualized: false,
                },
            )
            .expect("Runtime adapter");
        builder.build().expect("Runtime map")
    }

    #[tokio::test]
    async fn empty_node_input_does_not_prune_foreign_owner_current_state() {
        let site = test_site();
        let projection =
            NodeViewProjection::for_site_node(&site, "node-one").expect("node projection");
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let current = FileNodeState::open(
            directory.path().join("node"),
            site.site_id.clone(),
            "node-one",
        )
        .expect("node current state");
        current
            .upsert(current_deployment("foreign-deployment"), None)
            .expect("foreign owner current state");
        let controller = super::super::ModelRealizer::new_node_realizer(
            node_realizer(site.site_id, projection, directory.path()),
            current.clone(),
            test_runtime_target(),
            Vec::new(),
            directory.path().join("cache"),
        )
        .expect("node controller");

        controller
            .inner
            .reconcile()
            .await
            .expect("node reconciliation");

        assert_eq!(
            current
                .snapshot()
                .expect("current state")
                .model_deployments
                .iter()
                .map(|record| record.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["foreign-deployment"]
        );
    }

    #[test]
    fn node_cleanup_is_durable_without_mutating_projected_declaration() {
        let mut site = test_site();
        let mut teardown =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Teardown);
        teardown.model_deployment_id = "worker-teardown".to_owned();
        site.model_deployments.push(teardown.clone());
        let projection =
            NodeViewProjection::for_site_node(&site, "node-one").expect("node projection");
        let directory = crate::test_support::canonical_tempdir().expect("state directory");
        let state_path = directory.path().join("node");
        let current = FileNodeState::open(&state_path, site.site_id.clone(), "node-one")
            .expect("node current state");
        current
            .upsert(
                dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                    model_deployment_id: "worker-teardown".to_owned(),
                    reconciled_lifecycle_generation: Some(1),
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(super::super::state::unloading_state(None)),
                },
                None,
            )
            .expect("unloading record");
        let controller = super::super::ModelRealizer::new_node_realizer(
            node_realizer(site.site_id.clone(), projection, directory.path()),
            current.clone(),
            test_runtime_target(),
            Vec::new(),
            directory.path().join("cache"),
        )
        .expect("node controller");
        let before = current.snapshot().expect("state before cleanup");

        controller
            .inner
            .finalize_absent(&teardown, None)
            .expect("node cleanup result");
        let after = current.snapshot().expect("state after cleanup");
        assert_eq!(
            controller
                .inner
                .desired
                .model_deployments()
                .expect("projected declarations"),
            [teardown]
        );
        let retained = current
            .node_model_cleanup_evidence()
            .expect("cleanup evidence");
        assert_eq!(
            retained,
            [
                dotcore_proto::dotcore::site::store::v1::NodeModelCleanupEvidence {
                    model_deployment_id: "worker-teardown".to_owned(),
                    lifecycle_generation: 1,
                    realization_generation: None,
                    never_realized: true,
                    acknowledged: false,
                }
            ]
        );
        // The record removal and the evidence are one state transaction.
        assert!(after.model_deployments.is_empty());
        assert_eq!(after.storage_revision, before.storage_revision + 1);

        drop(controller);
        drop(current);
        let reopened = FileNodeState::open(state_path, site.site_id, "node-one")
            .expect("reopened node current state");
        assert_eq!(
            reopened
                .node_model_cleanup_evidence()
                .expect("retained cleanup evidence"),
            retained
        );
    }

    fn node_realizer(
        site_id: String,
        projection: NodeViewProjection,
        directory: &std::path::Path,
    ) -> super::NodeModelRealizerDependencies<super::super::test_support::ProjectedTestInput> {
        super::NodeModelRealizerDependencies {
            input: super::super::test_support::ProjectedTestInput::new(site_id, projection),
            composition: super::ModelControllerDependencies {
                artifacts: ArtifactDependencies::default(),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
            serving_targets:
                crate::model::controller::serving_targets::ServingTargetsPublisher::new().0,
            shutdown: tokio_util::sync::CancellationToken::new(),
            workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
            timing: Timing::test().model,
            clock: Arc::new(SystemClock),
            preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(directory),
        }
    }

    #[test]
    fn controller_rejects_unspecified_desired_state_before_runtime_actions() {
        let mut site = test_site();
        let mut desired =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        desired.desired_state = ModelDeploymentDesiredState::Unspecified as i32;
        site.model_deployments.push(desired);

        assert!(validate_runtime_inputs(&site, &test_runtime_target(), &[]).is_err());
    }

    #[test]
    fn backend_restart_does_not_admit_an_existing_simulated_deployment() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        desired.persist_present("fixture-model");
        let current = TestCurrent::new(calls);
        current
            .upsert(current_deployment("deployment"), None)
            .expect("existing current deployment");

        let controller =
            crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
                desired.clone(),
                current,
                test_runtime_target(),
                Vec::new(),
                "/test-cache".into(),
                crate::model::controller::ModelControllerDependencies {
                    target_capabilities:
                        crate::model::controller::test_support::test_target_capability_source(),

                    artifacts: ArtifactDependencies::default(),
                    engine_integrations:
                        crate::model::controller::test_support::test_engine_integrations(),
                    accelerator_attribution: None,
                },
            )
            .expect("restarted controller");

        let state = controller.inner.state.snapshot().expect("current state");
        assert_eq!(
            state.model_deployments[0]
                .state
                .as_ref()
                .map(|state| state.status),
            Some(ModelDeploymentStatus::Loading as i32),
        );
    }

    #[test]
    fn paused_restart_reconstructs_safe_missing_current_state_without_loading_or_ready() {
        for (observation, lifecycle) in [
            ("stopped", ModelDeploymentStatus::Paused),
            ("absent", ModelDeploymentStatus::Unavailable),
            ("active", ModelDeploymentStatus::Pausing),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let desired = TestDesired::new(calls.clone());
            let deployment =
                test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Paused);
            let identity = expected_identity(&test_site(), &deployment, "runtime/test");
            desired
                .state
                .lock()
                .expect("desired state")
                .model_deployments
                .push(deployment);
            let current = TestCurrent::new(calls);
            let observation = match observation {
                "stopped" => WorkloadObservation::stopped(identity),
                "absent" => WorkloadObservation::Absent(identity),
                "active" => WorkloadObservation::Running(identity),
                _ => unreachable!(),
            };

            crate::model::controller::test_support::TestRealizer::for_test(
                desired.clone(),
                current.clone(),
                test_runtime_target(),
                vec![TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation,
                }],
                "/test-cache".into(),
                Some(crate::model::controller::test_support::ready_oci_preparer()),
            )
            .expect("paused controller construction");

            assert_eq!(
                current.snapshot().expect("current state").model_deployments[0]
                    .state
                    .as_ref()
                    .expect("reconstructed state")
                    .status,
                lifecycle as i32
            );
        }
    }
}
