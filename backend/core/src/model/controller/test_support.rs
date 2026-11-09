//! Unit-test fakes and fixtures for the inference controller modules.
//!
//! The controller's test modules build a controller from in-memory site and
//! deployment fixtures and drive it with repository and runtime fakes that
//! record their calls and can be armed to fail. This keeps controller tests
//! free of real site files, runtime backends, and model artifacts.

use dotcore_domain::clock::SystemClock;
use dotcore_domain::timing::{ReconcileOutcome, Timing};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::ready,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use anyhow::anyhow;
use dotcore_domain::SiteNodeId;
use dotcore_domain::test_support::oci::digest_reference;
use dotcore_proto::dotcore::{
    catalog::v1::{
        ModelParallelShape, OciPlatform, UpstreamModelBoundary, UpstreamModelDataEgressClass,
        UpstreamModelLocality, UpstreamModelTemplate, UpstreamWireProtocol,
    },
    deployment::v1::ExactPlacement,
    model::v1::{
        CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentDesiredState,
        ModelDeploymentLifecycleTransition, ModelDeploymentSnapshot, ModelDeploymentStatus,
        OpenAiCompatibleEndpointConfiguration, UpstreamModelDeploymentSnapshot,
        UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
    site::v1::{
        ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter, SimulatedRuntimeScenario,
        Site, execution_target, model_runtime_binding,
    },
};
use dotcore_runtime::{
    LoopbackEndpoint, ObservedWorkload, RuntimeBackend, RuntimeBackendError, RuntimeBackendId,
    RuntimeFuture, WorkloadId, WorkloadLaunch, WorkloadObservation,
};
use dotcore_site::{
    node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, StaticTargetCapabilitySource,
        TargetCapabilityReconciler, TargetCapabilitySource,
    },
    persistence::SiteStorageError,
    repository::{ModelSiteSnapshot, SiteRepositoryError},
    store::InMemoryCurrentState,
    view::fencing::{
        AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
        RuntimeActionAuthority,
    },
};
use tokio::sync::Notify;

use super::{
    DesiredDeploymentRepository, ModelLifecycleError, ModelRealizationInput,
    ModelRealizationResults,
    launch::{resolved_launch, selected_target, workload_launch},
    state::loading_state,
};
use crate::{
    dispatch::model::{RuntimeTargetMap, RuntimeTargetMapBuilder},
    model::{
        repository::ModelDeclarationRepositoryError,
        state::{
            CurrentStateError, CurrentStateMutation, CurrentStateRepository, CurrentStateSnapshot,
        },
    },
};

pub(super) fn test_target_capability_source() -> Arc<dyn TargetCapabilitySource> {
    let platform = dotcore_domain::ExecutionPlatform::linux_amd64();
    let inventory = TargetCapabilityReconciler::reconcile(
        [("target-one", "node-one")],
        [RuntimePlatformObservation::available(
            "target-one",
            platform,
        )],
        [NodeAcceleratorObservation::nvidia("node-one", "580.173.02")],
        Timing::test().node_control.capability_max_age,
        std::time::SystemTime::now(),
    );
    Arc::new(StaticTargetCapabilitySource::new(inventory))
}

pub(super) fn ready_oci_preparer() -> Arc<dyn dotcore_artifact::OciImagePreparer> {
    Arc::new(ReadyOciPreparer)
}

struct ReadyOciPreparer;

impl dotcore_artifact::OciImagePreparer for ReadyOciPreparer {
    fn prepare(
        &self,
        _request: dotcore_artifact::OciPreparationRequest,
        _cancellation: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        Box::pin(async { Ok(()) })
    }
}

/// Node-local Docker image store fake: a mutable tag-to-ID map over the image IDs present.
/// It resolves tags like the artifact manager's resolver and prepares retained image IDs by
/// presence only, recording both so tests can prove what acceptance and launch asked for.
#[derive(Clone, Default)]
pub(super) struct TestImageStore {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
    images: Arc<Mutex<BTreeSet<String>>>,
    resolved_references: Arc<Mutex<Vec<String>>>,
    inspection_fails: Arc<Mutex<bool>>,
    preparation_requests: Arc<Mutex<Vec<dotcore_artifact::OciPreparationRequest>>>,
}

impl TestImageStore {
    /// Builds or retags: the image becomes present and the tag names it.
    pub(super) fn tag(&self, reference: &str, image_id: &str) {
        self.images
            .lock()
            .expect("images")
            .insert(image_id.to_owned());
        self.tags
            .lock()
            .expect("tags")
            .insert(reference.to_owned(), image_id.to_owned());
    }

    pub(super) fn remove_image(&self, image_id: &str) {
        self.images.lock().expect("images").remove(image_id);
    }

    pub(super) fn resolved_references(&self) -> Vec<String> {
        self.resolved_references
            .lock()
            .expect("resolved references")
            .clone()
    }

    pub(super) fn preparation_requests(&self) -> Vec<dotcore_artifact::OciPreparationRequest> {
        self.preparation_requests
            .lock()
            .expect("preparation requests")
            .clone()
    }

    fn prepare_local_image(
        &self,
        request: dotcore_artifact::OciPreparationRequest,
    ) -> Result<(), dotcore_artifact::OciPreparationError> {
        let present = self
            .images
            .lock()
            .expect("images")
            .contains(&request.reference);
        self.preparation_requests
            .lock()
            .expect("preparation requests")
            .push(request);
        present
            .then_some(())
            .ok_or(dotcore_artifact::OciPreparationError::LocalImageMissing)
    }
}

impl dotcore_artifact::OciImagePreparer for TestImageStore {
    fn prepare(
        &self,
        request: dotcore_artifact::OciPreparationRequest,
        _cancellation: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        let result = match request.source {
            dotcore_artifact::OciPreparationSource::LocalImageId => {
                self.prepare_local_image(request)
            }
            dotcore_artifact::OciPreparationSource::ImmutableRemote => Ok(()),
        };
        Box::pin(ready(result))
    }

    fn as_local_image_identity_resolver(
        &self,
    ) -> Option<&dyn dotcore_artifact::LocalImageIdentityResolver> {
        Some(self)
    }
}

impl dotcore_artifact::LocalImageIdentityResolver for TestImageStore {
    fn resolve_local_image_identity(
        &self,
        reference: String,
    ) -> dotcore_artifact::LocalImageIdentityResolutionFuture {
        let inspection_fails = *self.inspection_fails.lock().expect("inspection failure");
        let identity = if inspection_fails {
            Err(
                dotcore_artifact::LocalImageIdentityResolutionError::Inspection(anyhow::anyhow!(
                    "Cannot connect to the Docker daemon"
                )),
            )
        } else {
            self.tags
                .lock()
                .expect("tags")
                .get(&reference)
                .cloned()
                .ok_or(dotcore_artifact::LocalImageIdentityResolutionError::LocalImageMissing)
        };
        self.resolved_references
            .lock()
            .expect("resolved references")
            .push(reference);
        Box::pin(ready(identity))
    }
}

pub(super) struct RecordingRuntime {
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
    pub(super) virtualized: bool,
}

impl RuntimeBackend for RecordingRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/test".to_owned())
    }

    fn materialization_requirements(&self) -> dotcore_runtime::WorkloadMaterializationRequirements {
        if self.virtualized {
            dotcore_runtime::WorkloadMaterializationRequirements::virtualized()
        } else {
            dotcore_runtime::WorkloadMaterializationRequirements::physical()
        }
    }

    fn discover_workloads<'a>(
        &'a self,
    ) -> RuntimeFuture<'a, Vec<dotcore_runtime::WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        self.calls.lock().expect("calls").push("create");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        self.calls.lock().expect("calls").push("start");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("stop");
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("remove");
        Box::pin(ready(Ok(())))
    }
}

pub(super) struct CountingRuntime {
    pub(super) discoveries: Arc<AtomicUsize>,
    pub(super) discovered: Arc<Notify>,
}

impl RuntimeBackend for CountingRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/counting-test".to_owned())
    }

    fn materialization_requirements(&self) -> dotcore_runtime::WorkloadMaterializationRequirements {
        dotcore_runtime::WorkloadMaterializationRequirements::virtualized()
    }

    fn discover_workloads<'a>(
        &'a self,
    ) -> RuntimeFuture<'a, Vec<dotcore_runtime::WorkloadObservation>> {
        self.discoveries.fetch_add(1, Ordering::SeqCst);
        self.discovered.notify_one();
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, dotcore_runtime::WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(test_runtime_error())))
    }
}

#[derive(Clone)]
pub(super) struct TestDesired {
    pub(super) state: Arc<Mutex<Site>>,
    failure: Arc<Mutex<Option<TestFailure>>>,
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
    listener: Arc<Notify>,
    next_deployment_id: Arc<Mutex<Option<String>>>,
    unload_hook: Arc<Mutex<Option<UnloadHook>>>,
    /// While set, reads fail as on a node that has not accepted a desired view.
    view_withheld: Arc<AtomicBool>,
}

/// Work run by the next desired unload before it applies, standing in for a concurrent writer.
type UnloadHook = Box<dyn FnOnce() + Send>;

/// Read-only node realization input over a replaceable projected snapshot, as an installed view
/// presents it; replacing a changed snapshot wakes the realizer.
#[derive(Clone)]
pub(crate) struct ProjectedTestInput {
    snapshot: Arc<Mutex<ModelSiteSnapshot>>,
    listener: Arc<Notify>,
}

impl ProjectedTestInput {
    pub(crate) fn new(site_id: String, projection: NodeViewProjection) -> Self {
        Self::from_snapshot(ModelSiteSnapshot {
            site_id,
            execution_targets: projection.execution_targets().to_vec(),
            deployments: projection.model_deployments().to_vec(),
        })
    }

    pub(crate) fn from_snapshot(snapshot: ModelSiteSnapshot) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(snapshot)),
            listener: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn replace(&self, snapshot: ModelSiteSnapshot) {
        let mut current = self.snapshot.lock().expect("projected snapshot");
        if *current != snapshot {
            *current = snapshot;
            self.listener.notify_one();
        }
    }
}

impl ModelRealizationInput for ProjectedTestInput {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        Ok(self.snapshot.lock().expect("projected snapshot").clone())
    }

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(self
            .snapshot
            .lock()
            .expect("projected snapshot")
            .deployments
            .clone())
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        Arc::clone(&self.listener)
    }
}

#[derive(Clone)]
pub(super) struct TestCurrent {
    pub(super) state: InMemoryCurrentState,
    failure: Arc<Mutex<Option<TestFailure>>>,
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Clone, Copy)]
pub(super) enum TestFailure {
    Failed,

    OutcomeUncertain,
    Write,
    Rename,
    Sync,
    Current,
    CurrentUncertain,
}

impl TestFailure {
    fn is_outcome_uncertain(self) -> bool {
        matches!(
            self,
            Self::OutcomeUncertain | Self::Sync | Self::CurrentUncertain
        )
    }

    fn desired_error(self) -> ModelDeclarationRepositoryError {
        match self {
            Self::Failed | Self::Write => SiteRepositoryError::Storage(SiteStorageError::Storage(
                anyhow!("injected desired write failure"),
            ))
            .into(),
            Self::OutcomeUncertain | Self::Sync => SiteRepositoryError::Storage(
                SiteStorageError::OutcomeUncertain(anyhow!("injected desired sync uncertainty")),
            )
            .into(),
            Self::Rename => SiteRepositoryError::Storage(SiteStorageError::Storage(anyhow!(
                "injected desired rename failure"
            )))
            .into(),
            Self::Current | Self::CurrentUncertain => {
                panic!("current-state fault used for desired mutation")
            }
        }
    }

    fn current_error(self) -> CurrentStateError {
        match self {
            Self::Failed | Self::Current => CurrentStateError::Unavailable,
            Self::OutcomeUncertain | Self::CurrentUncertain => CurrentStateError::Poisoned,
            Self::Write | Self::Rename | Self::Sync => {
                panic!("desired-state fault used for current-state mutation")
            }
        }
    }

    fn asserts(self, error: &ModelLifecycleError) -> bool {
        if self.is_outcome_uncertain() {
            matches!(error, ModelLifecycleError::OutcomeUncertain(_))
        } else {
            matches!(
                error,
                ModelLifecycleError::ModelRepository(_) | ModelLifecycleError::CurrentState(_)
            )
        }
    }
}

impl TestDesired {
    pub(super) fn new(calls: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Site {
                site_id: "site-one".to_owned(),
                execution_targets: test_site().execution_targets,
                ..Default::default()
            })),
            failure: Arc::new(Mutex::new(None)),
            calls,
            listener: Arc::new(Notify::new()),
            next_deployment_id: Arc::new(Mutex::new(None)),
            unload_hook: Arc::new(Mutex::new(None)),
            view_withheld: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn fail_next(&self, failure: TestFailure) {
        *self.failure.lock().expect("failure") = Some(failure);
    }

    /// Makes reads fail as on a node without an accepted desired view until `withheld` is cleared.
    pub(super) fn withhold_view(&self, withheld: bool) {
        self.view_withheld.store(withheld, Ordering::SeqCst);
    }

    /// Runs `hook` inside the next desired unload, before the unload applies.
    pub(super) fn before_next_unload(&self, hook: impl FnOnce() + Send + 'static) {
        *self.unload_hook.lock().expect("unload hook") = Some(Box::new(hook));
    }

    pub(super) fn persist_present(&self, model_id: &str) {
        self.state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(test_desired_deployment(
                model_id,
                ModelDeploymentDesiredState::Present,
            ));
    }

    pub(super) fn listener(&self) -> Arc<Notify> {
        Arc::clone(&self.listener)
    }

    /// Applies a lifecycle transition as the site repository does, with a new lifecycle
    /// generation, the transition, and the desired state, then wakes the realizer reading this
    /// input as an installed view carrying the change does.
    pub(super) fn transition(
        &self,
        deployment_id: &str,
        desired_state: ModelDeploymentDesiredState,
    ) -> ModelDeploymentDeclaration {
        let mut site = self.state.lock().expect("desired state");
        let deployment = site
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .expect("declared deployment");
        deployment.lifecycle_generation += 1;
        deployment.lifecycle_transition = match desired_state {
            ModelDeploymentDesiredState::Paused => ModelDeploymentLifecycleTransition::Pause,
            ModelDeploymentDesiredState::Teardown => ModelDeploymentLifecycleTransition::Teardown,
            _ => ModelDeploymentLifecycleTransition::Resume,
        } as i32;
        deployment.desired_state = desired_state as i32;
        let transitioned = deployment.clone();
        drop(site);
        self.listener.notify_one();
        transitioned
    }

    pub(super) fn set_next_deployment_id(&self, deployment_id: String) {
        *self.next_deployment_id.lock().expect("next deployment ID") = Some(deployment_id);
    }

    fn operation(
        &self,
        call: &'static str,
    ) -> Result<Option<TestFailure>, ModelDeclarationRepositoryError> {
        self.calls.lock().expect("calls").push(call);
        match self.failure.lock().expect("failure").take() {
            Some(failure @ (TestFailure::Failed | TestFailure::OutcomeUncertain)) => {
                Err(failure.desired_error())
            }
            failure => Ok(failure),
        }
    }
}

impl ModelRealizationInput for TestDesired {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        if self.view_withheld.load(Ordering::SeqCst) {
            return Err(SiteRepositoryError::NoAcceptedView);
        }
        let state = self.state.lock().expect("desired state");
        Ok(ModelSiteSnapshot {
            site_id: state.site_id.clone(),
            execution_targets: state.execution_targets.clone(),
            deployments: state.model_deployments.clone(),
        })
    }

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(self
            .state
            .lock()
            .expect("desired state")
            .model_deployments
            .clone())
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.listener()
    }
}

impl ModelRealizationResults for TestDesired {
    fn confirm_cleanup(
        &self,
        deployment: &ModelDeploymentDeclaration,
        _realization_generation: Option<u64>,
    ) -> Result<(), ModelLifecycleError> {
        self.finalize_teardown(deployment)
            .map_err(ModelLifecycleError::from_model_repository)
    }
}

impl DesiredDeploymentRepository for TestDesired {
    fn with_declared_deployment_ids(
        &self,
        read: &mut super::DeclaredDeploymentIds<'_>,
    ) -> Result<(), ModelLifecycleError> {
        let state = self.state.lock().expect("desired state");
        read(
            &state
                .model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.clone())
                .collect(),
        )
    }

    fn estimate_local_model_resources(
        &self,
        _model_id: &str,
        _profile_id: &str,
        _configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        _target: Option<crate::model::repository::EstimateTarget<'_>>,
    ) -> Result<
        dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse,
        ModelDeclarationRepositoryError,
    > {
        self.operation("desired estimate")?;
        Ok(
            dotcore_proto::dotcore::model::v1::EstimateLocalModelResourcesResponse {
                hardware_class_estimates: vec![
                    dotcore_proto::dotcore::model::v1::HardwareClassResourceEstimate {
                        hardware_class_id: "fixture-class".to_owned(),
                        estimates: vec![
                            dotcore_proto::dotcore::model::v1::ScalarResourceEstimate {
                                resource: "acceleratorMemory".to_owned(),
                                value: 112,
                                unit: "bytes".to_owned(),
                            },
                        ],
                        derived_engine_parameter_values: None,
                        launch_rejection: None,
                    },
                ],
                placement_preview: None,
            },
        )
    }

    fn load_model(
        &self,
        model_id: &str,
        _profile_id: &str,
        placement: &dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        _configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        _exposure: &super::ModelDeploymentExposureIntent,
        _name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let failure = self.operation("desired load")?;
        let mut deployment =
            test_desired_deployment(model_id, ModelDeploymentDesiredState::Present);
        deployment.placement = Some(placement.clone());
        let engine = deployment
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
            .and_then(|source| match source {
                ModelDeploymentSnapshotSource::Local(local) => local.engine_artifact.as_mut(),
                ModelDeploymentSnapshotSource::Upstream(_) => None,
            })
            .expect("test engine artifact");
        if let Some(deployment_id) = self
            .next_deployment_id
            .lock()
            .expect("next deployment ID")
            .take()
        {
            deployment.model_deployment_id = deployment_id.clone();
            set_test_oci_reference(engine, &deployment_id);
        }
        self.state
            .lock()
            .expect("desired state")
            .model_deployments
            .push(deployment.clone());
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(deployment),
        }
    }

    fn update_upstream_model(
        &self,
        deployment_id: &str,
        replacement: &UpstreamModelDesiredConfiguration,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let failure = self.operation("desired update")?;
        let mut state = self.state.lock().expect("desired state");
        let deployment = state
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
            })?;
        deployment.upstream_desired_configuration = Some(replacement.clone());
        let deployment = deployment.clone();
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(deployment),
        }
    }

    fn unload_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let failure = self.operation("desired unload")?;
        if let Some(hook) = self.unload_hook.lock().expect("unload hook").take() {
            hook();
        }
        let mut state = self.state.lock().expect("desired state");
        let deployment = state
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
            })?;
        deployment.desired_state = ModelDeploymentDesiredState::Teardown as i32;
        let deployment = deployment.clone();
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(deployment),
        }
    }

    fn pause_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let failure = self.operation("desired pause")?;
        let mut state = self.state.lock().expect("desired state");
        let deployment = state
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
            })?;
        let desired = ModelDeploymentDesiredState::try_from(deployment.desired_state)
            .map_err(|_| ModelDeclarationRepositoryError::InvalidState("invalid desired state"))?;
        if desired != ModelDeploymentDesiredState::Present
            && desired != ModelDeploymentDesiredState::Paused
        {
            return Err(ModelDeclarationRepositoryError::InvalidTransition {
                deployment_id: deployment_id.to_owned(),
                operation: "pause",
            });
        }
        deployment.desired_state = ModelDeploymentDesiredState::Paused as i32;
        let deployment = deployment.clone();
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(deployment),
        }
    }

    fn resume_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        let failure = self.operation("desired resume")?;
        let mut state = self.state.lock().expect("desired state");
        let deployment = state
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .ok_or_else(|| {
                ModelDeclarationRepositoryError::DeploymentNotFound(deployment_id.to_owned())
            })?;
        let desired = ModelDeploymentDesiredState::try_from(deployment.desired_state)
            .map_err(|_| ModelDeclarationRepositoryError::InvalidState("invalid desired state"))?;
        if desired != ModelDeploymentDesiredState::Paused
            && desired != ModelDeploymentDesiredState::Present
        {
            return Err(ModelDeclarationRepositoryError::InvalidTransition {
                deployment_id: deployment_id.to_owned(),
                operation: "resume",
            });
        }
        deployment.desired_state = ModelDeploymentDesiredState::Present as i32;
        let deployment = deployment.clone();
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(deployment),
        }
    }

    fn finalize_teardown(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Result<(), ModelDeclarationRepositoryError> {
        let failure = self.operation("desired remove")?;
        let mut state = self.state.lock().expect("desired state");
        let Some(index) = state.model_deployments.iter().position(|deployment| {
            deployment.model_deployment_id == declaration.model_deployment_id
        }) else {
            return Ok(());
        };
        let retained = &state.model_deployments[index];
        if retained.lifecycle_generation != declaration.lifecycle_generation
            || retained.desired_state != ModelDeploymentDesiredState::Teardown as i32
            || retained.placement != declaration.placement
        {
            return Err(ModelDeclarationRepositoryError::TeardownMismatch(
                declaration.model_deployment_id.clone(),
            ));
        }
        state.model_deployments.remove(index);
        match failure {
            Some(failure) => Err(failure.desired_error()),
            None => Ok(()),
        }
    }
}

impl TestCurrent {
    pub(super) fn new(calls: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            state: InMemoryCurrentState::new("site-one").expect("current state"),
            failure: Arc::new(Mutex::new(None)),
            calls,
        }
    }

    pub(super) fn fail_next(&self, failure: TestFailure) {
        *self.failure.lock().expect("failure") = Some(failure);
    }

    fn operation(&self, call: &'static str) -> Result<(), CurrentStateError> {
        self.calls.lock().expect("calls").push(call);
        match self.failure.lock().expect("failure").take() {
            Some(failure) => Err(failure.current_error()),
            None => Ok(()),
        }
    }
}

impl CurrentStateRepository for TestCurrent {
    fn snapshot(&self) -> Result<CurrentStateSnapshot, CurrentStateError> {
        self.state.snapshot()
    }

    fn prepare(
        &self,
        deployments: Vec<CurrentModelDeployment>,
        expected_storage_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.operation("current prepare")?;
        self.state.prepare(deployments, expected_storage_revision)
    }

    fn upsert(
        &self,
        deployment: CurrentModelDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.operation("current upsert")?;
        self.state.upsert(deployment, expected_record_revision)
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.operation("current remove")?;
        self.state.remove(deployment_id, expected_record_revision)
    }
}

pub(super) fn test_site() -> Site {
    Site {
        site_id: "site-one".to_owned(),
        model_deployment_identity_allocator: Some(
            dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
                namespace: "0123456789abcdef".to_owned(),
                next_sequence: 1,
            },
        ),
        execution_targets: vec![ExecutionTarget {
            execution_target_id: "target-one".to_owned(),
            display_name: "target-one".to_owned(),
            site_node_id: SiteNodeId::for_test("node-one").to_string(),
            accelerator_device_ids: vec!["device-one".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedRuntimeAdapter {
                            default_scenario: Some(SimulatedRuntimeScenario::default()),
                            deployment_scenarios: Vec::new(),
                        },
                    )),
                },
            )),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
        }],
        ..Default::default()
    }
}

pub(super) fn test_placement() -> ExactPlacement {
    ExactPlacement {
        site_node_id: "node-one".to_owned(),
        execution_target_id: "target-one".to_owned(),
    }
}

pub(super) fn test_desired_deployment(
    model_id: &str,
    desired_state: ModelDeploymentDesiredState,
) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: "deployment".to_owned(),
        model_deployment_name: "Model".to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        }),
        requested_parallel_shape: ModelParallelShape::One as i32,
        snapshot: Some({
            let mut snapshot = valid_snapshot();
            match snapshot.source.as_mut() {
                Some(ModelDeploymentSnapshotSource::Local(model)) => {
                    model.model_id = model_id.to_owned();
                    let Some(plan) = model.desired_launch_plan.as_mut() else {
                        panic!("test snapshot has a desired launch plan")
                    };
                    let Some(argument) = plan.arguments.get_mut(4) else {
                        panic!("test desired launch plan has an alias value")
                    };
                    argument.value = Some(
                        dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal(
                            model_id.to_owned(),
                        ),
                    );
                }
                Some(ModelDeploymentSnapshotSource::Upstream(_)) | None => {
                    panic!("test snapshot has a local model")
                }
            }
            snapshot
        }),
        desired_state: desired_state as i32,
        upstream_desired_configuration: None,
        lifecycle_generation: 1,
        lifecycle_transition: match desired_state {
            ModelDeploymentDesiredState::Present => {
                dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Create
            }
            ModelDeploymentDesiredState::Paused => {
                dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Pause
            }
            ModelDeploymentDesiredState::Teardown => {
                dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Teardown
            }
            ModelDeploymentDesiredState::Unspecified => {
                dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Unspecified
            }
        } as i32,
        lifecycle_recovery_required: false,
        node_resolved_launch_identities: Vec::new(),
    }
}

/// Returns a present deployment of the in-code ninfer fixture serving `model_id`.
pub(super) fn test_ninfer_desired_deployment(model_id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        snapshot: Some(crate::test_support::ninfer::local_model_snapshot(model_id)),
        ..test_desired_deployment(model_id, ModelDeploymentDesiredState::Present)
    }
}

pub(super) fn valid_upstream_desired_deployment() -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: "upstream-deployment".to_owned(),
        snapshot: Some(ModelDeploymentSnapshot {
            source: Some(ModelDeploymentSnapshotSource::Upstream(
                UpstreamModelDeploymentSnapshot {
                    upstream_model_template: Some(UpstreamModelTemplate {
                        upstream_model_template_id: "openai-compatible".to_owned(),
                        display_name: "OpenAI-compatible".to_owned(),
                        wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
                    }),
                },
            )),
        }),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        upstream_desired_configuration: Some(UpstreamModelDesiredConfiguration {
            upstream_model_id: "gpt-4o".to_owned(),
            api_key: "test-key".to_owned(),
            input_capabilities: Some(UpstreamModelInputCapabilities {
                supports_text: true,
                supports_image: false,
            }),
            endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                base_url: "https://upstream.test/v1".to_owned(),
                allow_insecure_http: false,
            }),
            boundary: Some(UpstreamModelBoundary {
                locality: UpstreamModelLocality::PublicExternal as i32,
                trust_domain: "upstream.test".to_owned(),
                data_egress_class: UpstreamModelDataEgressClass::PublicInternet as i32,
            }),
        }),
        ..Default::default()
    }
}

pub(super) fn upstream_desired_deployment() -> ModelDeploymentDeclaration {
    let mut deployment = valid_upstream_desired_deployment();
    deployment.model_deployment_id = "deployment".to_owned();
    deployment
}

pub(super) fn expected_identity(
    site: &Site,
    desired: &ModelDeploymentDeclaration,
    backend_id: &str,
) -> ObservedWorkload {
    let resolved = resolved_launch(desired).expect("resolved test model");
    let artifacts = dotcore_artifact::ModelArtifactCache::new("/test-cache".into())
        .expect("test cache")
        .expected_paths(&resolved.artifacts)
        .expect("test artifact paths");
    let launch = workload_launch(
        desired,
        selected_target(site, desired).expect("test execution target"),
        &artifacts,
    )
    .expect("test launch");
    let generation = launch.correlation.reconcile_generation;
    ObservedWorkload {
        workload_id: WorkloadId("test-workload".to_owned()),
        correlation: launch.correlation,
        backend_id: RuntimeBackendId(backend_id.to_owned()),
        loopback_endpoints: Vec::new(),
        services: vec![dotcore_runtime::WorkloadServiceBinding::native(
            "test-workload",
            "http",
            generation,
        )],
        host_process: None,
    }
}

pub(super) fn test_runtime_target() -> RuntimeTargetMap {
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            RecordingRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
                virtualized: false,
            },
        )
        .expect("Runtime adapter");
    builder.build().expect("Runtime map")
}

pub(super) fn test_simulated_runtime_target() -> RuntimeTargetMap {
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            dotcore_runtime_backends::simulated::InMemoryRuntimeBackend::new(
                dotcore_domain::ExecutionPlatform::linux_amd64(),
            ),
        )
        .expect("simulated runtime adapter");
    builder.build().expect("Runtime map")
}

pub(super) fn test_engine_integrations() -> Arc<crate::model::engine::EngineIntegrationRegistry> {
    Arc::new(
        crate::model::engine::EngineIntegrationRegistry::with_native_provider_and_transports(
            Arc::new(crate::inference::simulated::SimulatedNativeProvider::new(
                &Timing::test().simulated,
                Arc::new(SystemClock),
            )),
            crate::model::engine::EngineTransportFactories::local_engines(),
        ),
    )
}

pub(super) fn test_engine_binding(
    deployment: &ModelDeploymentDeclaration,
    workload: &ObservedWorkload,
) -> crate::inference::EngineBinding {
    let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(
        deployment.snapshot.as_ref().expect("deployment snapshot"),
    )
    .expect("resolved launch");
    let service = workload
        .services
        .iter()
        .find(|service| service.reference.service_name == "http")
        .expect("http service binding");
    test_engine_integrations()
        .bind_workload(&resolved, &deployment.model_deployment_id, service)
        .expect("engine binding")
}

pub(crate) fn test_engine_binding_for_values(
    deployment_id: &str,
    realization_id: &str,
    model_id: &str,
    reconcile_generation: u64,
    endpoint: Option<LoopbackEndpoint>,
) -> crate::inference::EngineBinding {
    let service = match endpoint {
        Some(endpoint) => dotcore_runtime::WorkloadServiceBinding::loopback(
            realization_id,
            "http",
            reconcile_generation,
            endpoint,
        ),
        None => dotcore_runtime::WorkloadServiceBinding::native(
            realization_id,
            "http",
            reconcile_generation,
        ),
    };
    let mut snapshot = valid_snapshot();
    let source = snapshot.source.as_mut().expect("snapshot source");
    match source {
        dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
            local.model_id = model_id.to_owned();
            local
                .desired_launch_plan
                .as_mut()
                .expect("launch plan")
                .arguments[4]
                .value = Some(
                dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal(
                    model_id.to_owned(),
                ),
            );
        }
        _ => panic!("local snapshot"),
    }
    let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
        .expect("resolved launch");
    test_engine_integrations()
        .bind_workload(&resolved, deployment_id, &service)
        .expect("engine binding")
}

pub(super) fn current_deployment(id: &str) -> CurrentModelDeployment {
    CurrentModelDeployment {
        model_deployment_id: id.to_owned(),
        reconciled_lifecycle_generation: None,
        record_revision: 0,
        recovery_barrier: None,
        state: Some(loading_state()),
    }
}

pub(super) fn test_runtime_error() -> RuntimeBackendError {
    RuntimeBackendError::InvalidLaunch(anyhow!("test operation is unsupported"))
}

pub(super) fn valid_snapshot() -> ModelDeploymentSnapshot {
    crate::test_support::local_model_snapshot("fixture-model")
}

#[derive(Clone, Debug)]
pub(super) enum AcquisitionOutcome {
    Ready,

    Failed(&'static str),
}

/// Node realizer over test input that also confirms cleanup, as the in-crate realizer tests
/// drive it.
pub(super) type TestRealizer = super::ModelRealizer<TestDesired, TestCurrent, TestDesired>;

pub(super) struct AcquisitionControllerHarness {
    controller: TestRealizer,
    desired: TestDesired,
    current: TestCurrent,
    acquisition: Arc<ControlledOciPreparer>,
    runtime: Arc<AcquisitionRuntimeState>,
    events: Arc<AcquisitionEvents>,
    reconciliation_outcomes: Arc<Mutex<Vec<String>>>,
    model_file_blocks: Arc<Mutex<BTreeMap<String, bool>>>,
    action_authority: Option<AcceptedNodeViewAuthority>,
    cache_root: tempfile::TempDir,
}

impl AcquisitionControllerHarness {
    pub(super) fn docker(deployment_ids: &[&str]) -> Self {
        Self::new(
            deployment_ids
                .iter()
                .map(|id| ((*id).to_owned(), ModelDeploymentStatus::Loading))
                .collect(),
        )
    }

    pub(super) fn docker_leased(
        deployment_ids: &[&str],
        lease_duration: std::time::Duration,
    ) -> Self {
        Self::new_with_lease(
            deployment_ids
                .iter()
                .map(|id| ((*id).to_owned(), ModelDeploymentStatus::Loading))
                .collect(),
            Some(lease_duration),
        )
    }

    pub(super) fn persisted_failed(deployment_id: &str, code: &str, status_message: &str) -> Self {
        let harness = Self::new(vec![(
            deployment_id.to_owned(),
            ModelDeploymentStatus::Failed,
        )]);
        let mut state = harness.persisted_state(deployment_id);
        state.status_message = status_message.to_owned();
        state.failure = Some(dotcore_proto::dotcore::model::v1::ModelDeploymentFailure {
            code: code.to_owned(),
            message: "Engine image acquisition failed; unload and deploy the model again"
                .to_owned(),
            diagnostics: Vec::new(),
        });
        harness.replace_persisted_state(deployment_id, state);
        harness
    }

    pub(super) fn restart(deployments: &[(&str, ModelDeploymentStatus)]) -> Self {
        Self::new(
            deployments
                .iter()
                .map(|(id, lifecycle)| ((*id).to_owned(), *lifecycle))
                .collect(),
        )
    }

    pub(super) fn controller(&self) -> TestRealizer {
        self.controller.clone()
    }

    pub(super) fn block_model_files(&self, deployment_id: &str) {
        self.model_file_blocks
            .lock()
            .expect("model file blocks")
            .insert(deployment_id.to_owned(), true);
    }

    pub(super) fn release_model_files(&self, deployment_id: &str) {
        self.events.push(deployment_id, "model files ready");
        self.model_file_blocks
            .lock()
            .expect("model file blocks")
            .insert(deployment_id.to_owned(), false);
        self.acquisition.release_waiters();
    }

    pub(super) fn block_acquisition(&self, deployment_id: &str) {
        self.acquisition.block(deployment_id);
    }

    pub(super) fn set_acquisition_outcome(&self, deployment_id: &str, outcome: AcquisitionOutcome) {
        self.acquisition.set_outcome(deployment_id, outcome, false);
    }

    pub(super) fn release_acquisition(&self, deployment_id: &str, outcome: AcquisitionOutcome) {
        self.acquisition.set_outcome(deployment_id, outcome, true);
    }

    pub(super) fn finish_cancelled_acquisition(&self, deployment_id: &str) {
        self.acquisition.finish_cancelled(deployment_id);
    }

    pub(super) fn start_reconciliation(
        &self,
    ) -> tokio::task::JoinHandle<Result<ReconcileOutcome, ModelLifecycleError>> {
        for (deployment_id, blocked) in self
            .model_file_blocks
            .lock()
            .expect("model file blocks")
            .iter()
        {
            if *blocked {
                self.events.push(deployment_id, "model files preparing");
            }
        }
        let controller = self.controller.clone();
        let outcomes = self.reconciliation_outcomes.clone();
        tokio::spawn(async move {
            let result = controller.inner.reconcile().await;
            outcomes
                .lock()
                .expect("reconciliation outcomes")
                .push(format!("{result:?}"));
            result
        })
    }

    pub(super) async fn reconcile_until_settled(&self) -> Result<(), ModelLifecycleError> {
        for _ in 0..10 {
            if matches!(
                self.controller.inner.reconcile().await?,
                ReconcileOutcome::Settled
            ) {
                return Ok(());
            }
        }
        Err(ModelLifecycleError::configuration(
            "test reconciliation did not settle",
        ))
    }

    /// Moves a deployment to teardown in the realizer's input and wakes the realizer, as an
    /// installed view carrying the teardown does.
    pub(super) fn unload(&self, deployment_id: &str) -> ModelDeploymentDeclaration {
        let unloaded = self
            .desired
            .unload_model(deployment_id)
            .expect("teardown intent");
        self.desired.listener().notify_one();
        unloaded
    }

    /// Declares `deployment_id` with a local-development engine already pinned to `image_id`, as
    /// the node's pinning presents it to the realizer, and wakes the realizer.
    pub(super) fn load_pinned_local_engine(
        &self,
        deployment_id: &str,
        image_id: &str,
    ) -> ModelDeploymentDeclaration {
        self.set_next_deployment_id(deployment_id);
        let mut loaded = self.load();
        let mut site = self.desired.state.lock().expect("desired site");
        let declared = site
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .expect("declared deployment");
        if let Some(ModelDeploymentSnapshotSource::Local(local)) = declared
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        {
            retain_local_development_image(
                local.engine_artifact.as_mut().expect("Engine artifact"),
                image_id,
            );
        }
        loaded.clone_from(declared);
        loaded
    }

    /// Declares a fresh fixture deployment in the realizer's input and wakes the realizer.
    pub(super) fn load(&self) -> ModelDeploymentDeclaration {
        let loaded = self
            .desired
            .load_model(
                "fixture-model",
                "fixture-profile",
                &test_placement(),
                &crate::test_support::local_model_configuration(),
                &super::ModelDeploymentExposureIntent::Hidden,
                None,
            )
            .expect("present intent");
        self.desired.listener().notify_one();
        loaded
    }

    pub(super) async fn wait_for_event(&self, deployment_id: &str, event: &str) {
        let expected = format!("{deployment_id}:{event}");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let notified = self.events.changed.notified();
                if self.events.snapshot().contains(&expected) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "controller event {expected:?}: {error}; observed events: {:?}; reconciliation outcomes: {:?}",
                self.events.snapshot(),
                self.reconciliation_outcomes
                    .lock()
                    .expect("reconciliation outcomes")
            )
        });
    }

    pub(super) fn events(&self) -> Vec<String> {
        self.events.snapshot()
    }

    pub(super) fn runtime_create_count(&self, deployment_id: &str) -> usize {
        self.runtime
            .create_counts
            .lock()
            .expect("create counts")
            .get(deployment_id)
            .copied()
            .unwrap_or_default()
    }

    /// The image the runtime last created the deployment's workload with.
    pub(super) fn runtime_created_image(&self, deployment_id: &str) -> Option<String> {
        self.runtime
            .created_images
            .lock()
            .expect("created images")
            .get(deployment_id)
            .cloned()
    }

    pub(super) fn acquisition_call_count(&self, deployment_id: &str) -> usize {
        self.acquisition.call_count(deployment_id)
    }

    pub(super) fn runtime_workload_count(&self) -> usize {
        self.runtime.workloads.lock().expect("workloads").len()
    }

    pub(super) fn serving_target_count(&self) -> usize {
        self.controller
            .inner
            .serving_targets_source
            .snapshot()
            .targets
            .len()
    }

    pub(super) fn replace_manager_session_with_same_view(&self) {
        let authority = self.action_authority.as_ref().expect("leased authority");
        authority
            .replace_manager_session(
                ManagerSession::parse("c".repeat(32)).expect("replacement session"),
            )
            .expect("replace manager session");
        self.accept_current_view(1);
    }

    pub(super) fn refresh_current_view(&self, revision: u64) {
        self.accept_current_view(revision);
    }

    pub(super) fn install_view_with_unrelated_deployment(&self, revision: u64) {
        let authority = self.action_authority.as_ref().expect("leased authority");
        let mut site = self.desired.state.lock().expect("desired site").clone();
        let mut unrelated =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        unrelated.model_deployment_id = "unrelated".to_owned();
        site.model_deployments.push(unrelated);
        let node_id = site.execution_targets[0].site_node_id.clone();
        let projection = NodeViewProjection::for_site_node(&site, &node_id)
            .expect("node view with unrelated deployment");
        authority
            .install_for_test(site.site_id, node_id, revision, &projection)
            .expect("accept unrelated view refresh");
    }

    /// Returns the lines one preparation step of a deployment wrote to its log, oldest first.
    pub(super) async fn preparation_log(
        &self,
        deployment_id: &str,
        step: dotcore_artifact::PreparationStep,
    ) -> Vec<String> {
        let query = dotcore_runtime::RuntimeLogQuery::new(
            std::num::NonZeroU16::new(200).expect("limit"),
            None,
        )
        .expect("query");
        crate::deployment_logs::PreparationLogs::on_system_clock(self.cache_root.path())
            .read(deployment_id, &[step], &query)
            .await
            .expect("preparation log")
            .remove(0)
            .result
            .expect("readable preparation log")
            .entries
            .into_iter()
            .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
            .collect()
    }

    /// Returns the steps of a deployment whose logs hold output.
    pub(super) fn prepared_steps(
        &self,
        deployment_id: &str,
    ) -> Vec<dotcore_artifact::PreparationStep> {
        crate::deployment_logs::PreparationLogs::on_system_clock(self.cache_root.path())
            .select(
                crate::deployment_logs::PreparationOwner::Model,
                deployment_id,
                &[],
                &crate::deployment_logs::model_steps(),
                1,
            )
            .steps
    }

    pub(super) fn persisted_state(
        &self,
        deployment_id: &str,
    ) -> dotcore_proto::dotcore::model::v1::ModelDeploymentState {
        self.persisted_deployment(deployment_id)
            .and_then(|record| record.state)
            .expect("persisted deployment state")
    }

    pub(super) fn persisted_deployment(
        &self,
        deployment_id: &str,
    ) -> Option<CurrentModelDeployment> {
        self.current
            .snapshot()
            .expect("current snapshot")
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == deployment_id)
    }

    pub(super) fn desired_deployment(
        &self,
        deployment_id: &str,
    ) -> Option<ModelDeploymentDeclaration> {
        self.desired
            .model_deployments()
            .expect("desired deployments")
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
    }

    pub(super) fn storage_revision(&self) -> u64 {
        self.current
            .snapshot()
            .expect("current snapshot")
            .storage_revision
    }

    pub(super) fn set_next_deployment_id(&self, deployment_id: &str) {
        self.desired
            .set_next_deployment_id(deployment_id.to_owned());
    }

    /// Returns the node-local image store behind the controller's artifact manager.
    pub(super) fn local_images(&self) -> TestImageStore {
        self.acquisition.local_images.clone()
    }

    fn accept_current_view(&self, revision: u64) {
        let authority = self.action_authority.as_ref().expect("leased authority");
        let site = self.desired.state.lock().expect("desired site").clone();
        let node_id = site.execution_targets[0].site_node_id.clone();
        let projection =
            NodeViewProjection::for_site_node(&site, &node_id).expect("current node view");
        authority
            .install_for_test(site.site_id, node_id, revision, &projection)
            .expect("accept current node view");
    }

    fn new(deployments: Vec<(String, ModelDeploymentStatus)>) -> Self {
        Self::new_with_lease(deployments, None)
    }

    fn new_with_lease(
        deployments: Vec<(String, ModelDeploymentStatus)>,
        lease_duration: Option<std::time::Duration>,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let current = TestCurrent::new(calls);
        let mut site = test_site();
        site.execution_targets[0].runtime_binding = Some(
            execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                    dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
                )),
            }),
        );
        desired
            .state
            .lock()
            .expect("desired site")
            .execution_targets = site.execution_targets.clone();
        let cache_root = tempfile::Builder::new()
            .prefix("dotcore-controller-acquisition-")
            .tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("artifact cache root");
        let cache = dotcore_artifact::ModelArtifactCache::new(cache_root.path().to_path_buf())
            .expect("artifact cache");
        // Every fixture deployment shares one model artifact, cached before any deployment exists.
        let model_path = cache
            .expected_paths(
                &resolved_launch(&test_desired_deployment(
                    "fixture-model",
                    ModelDeploymentDesiredState::Present,
                ))
                .expect("resolved deployment")
                .artifacts,
            )
            .expect("artifact paths")
            .path("model")
            .clone();
        std::fs::create_dir_all(model_path.parent().expect("artifact parent"))
            .expect("artifact parent");
        std::fs::write(model_path, b"fixture").expect("cached model artifact");
        for (deployment_id, lifecycle) in deployments {
            let mut declaration =
                test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
            declaration.model_deployment_id = deployment_id.clone();
            set_test_oci_reference(
                declaration
                    .snapshot
                    .as_mut()
                    .and_then(|snapshot| snapshot.source.as_mut())
                    .and_then(|source| match source {
                        ModelDeploymentSnapshotSource::Local(local) => {
                            local.engine_artifact.as_mut()
                        }
                        ModelDeploymentSnapshotSource::Upstream(_) => None,
                    })
                    .expect("Engine artifact"),
                &deployment_id,
            );
            desired
                .state
                .lock()
                .expect("desired site")
                .model_deployments
                .push(declaration);
            let mut state = loading_state();
            state.status = lifecycle as i32;
            state.status_message = lifecycle.as_str_name().to_ascii_lowercase();
            state.progress = None;
            if lifecycle == ModelDeploymentStatus::Failed {
                state =
                    super::state::engine_image_acquisition_failed_state(Some(&state), Vec::new());
            }
            current
                .upsert(
                    CurrentModelDeployment {
                        model_deployment_id: deployment_id,
                        reconciled_lifecycle_generation: None,
                        record_revision: 0,
                        recovery_barrier: None,
                        state: Some(state),
                    },
                    None,
                )
                .expect("current deployment");
        }
        let events = Arc::new(AcquisitionEvents::default());
        let reconciliation_outcomes = Arc::new(Mutex::new(Vec::new()));
        let model_file_blocks = Arc::new(Mutex::new(BTreeMap::new()));
        let acquisition = Arc::new(ControlledOciPreparer::new(
            events.clone(),
            model_file_blocks.clone(),
        ));
        let runtime = Arc::new(AcquisitionRuntimeState::new(events.clone()));
        let mut builder = RuntimeTargetMapBuilder::new();
        let mut action_authority = None;
        if let Some(lease_duration) = lease_duration {
            let assigned_site = desired.state.lock().expect("desired site").clone();
            let node_id = assigned_site.execution_targets[0].site_node_id.clone();
            let projection = NodeViewProjection::for_site_node(&assigned_site, &node_id)
                .expect("leased node view");
            let node = NodeIncarnation::parse("b".repeat(32)).expect("node incarnation");
            let manager = ManagerSession::parse("a".repeat(32)).expect("manager session");
            let authority = AcceptedNodeViewAuthority::new(
                assigned_site.site_id.clone(),
                node_id.clone(),
                node,
                manager,
                lease_duration,
            )
            .expect("accepted view authority");
            authority
                .install_for_test(assigned_site.site_id, node_id, 1, &projection)
                .expect("accepted leased view");
            action_authority = Some(authority.clone());
            builder = builder.with_action_authority(RuntimeActionAuthority::Leased(authority));
        }
        builder
            .insert("target-one", AcquisitionRuntime(runtime.clone()))
            .expect("Runtime adapter");
        let controller = TestRealizer::for_test(
            desired.clone(),
            current.clone(),
            builder.build().expect("Runtime map"),
            Vec::new(),
            cache_root.path().to_path_buf(),
            Some(acquisition.clone()),
        )
        .expect("acquisition realizer");
        Self {
            controller,
            desired,
            current,
            acquisition,
            runtime,
            events,
            reconciliation_outcomes,
            model_file_blocks,
            action_authority,
            cache_root,
        }
    }

    fn replace_persisted_state(
        &self,
        deployment_id: &str,
        state: dotcore_proto::dotcore::model::v1::ModelDeploymentState,
    ) {
        let existing = self
            .persisted_deployment(deployment_id)
            .expect("persisted deployment");
        self.current
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: deployment_id.to_owned(),
                    reconciled_lifecycle_generation: None,
                    record_revision: 0,
                    recovery_barrier: None,
                    state: Some(state),
                },
                Some(existing.record_revision),
            )
            .expect("replace persisted state");
    }
}

#[test]
fn acquisition_controller_harness_owns_process_unique_cache_root() {
    const CHILD_ROLE: &str = "DOTCORE_ACQUISITION_CACHE_CHILD_ROLE";
    const COORDINATION_ROOT: &str = "DOTCORE_ACQUISITION_CACHE_COORDINATION_ROOT";
    const TEST_NAME: &str = "model::controller::test_support::acquisition_controller_harness_owns_process_unique_cache_root";

    if let Some(role) = std::env::var_os(CHILD_ROLE) {
        let role = role.to_string_lossy();
        let coordination = std::path::PathBuf::from(
            std::env::var_os(COORDINATION_ROOT).expect("coordination root"),
        );
        let harness = AcquisitionControllerHarness::docker(&["deployment"]);
        let cache_root = harness.cache_root.path().to_path_buf();
        std::fs::write(
            coordination.join(format!("{role}-root")),
            cache_root.as_os_str().as_encoded_bytes(),
        )
        .expect("publish cache root");
        std::fs::write(cache_root.join(format!("{role}-marker")), b"fixture")
            .expect("seed cache marker");

        match role.as_ref() {
            "owner" => {
                wait_for_acquisition_cache_checkpoint(&coordination.join("drop-owner"));
                drop(harness);
                std::fs::write(coordination.join("owner-dropped"), b"done")
                    .expect("publish owner drop");
            }
            "keeper" => {
                wait_for_acquisition_cache_checkpoint(&coordination.join("check-keeper"));
                assert!(cache_root.join("keeper-marker").is_file());
                assert!(cache_root.is_dir());
                std::fs::write(coordination.join("keeper-preserved"), b"done")
                    .expect("publish preserved cache");
            }
            _ => panic!("unknown acquisition cache child role"),
        }
        return;
    }

    let _host_load = crate::test_support::host_process_load_guard();
    let coordination = crate::test_support::canonical_tempdir().expect("coordination directory");
    let owner = crate::test_support::child::current_test_command(TEST_NAME)
        .env(CHILD_ROLE, "owner")
        .env(COORDINATION_ROOT, coordination.path())
        .spawn()
        .expect("owner child");
    let keeper = crate::test_support::child::current_test_command(TEST_NAME)
        .env(CHILD_ROLE, "keeper")
        .env(COORDINATION_ROOT, coordination.path())
        .spawn()
        .expect("keeper child");

    let owner_root_file = coordination.path().join("owner-root");
    let keeper_root_file = coordination.path().join("keeper-root");
    wait_for_acquisition_cache_checkpoint(&owner_root_file);
    wait_for_acquisition_cache_checkpoint(&keeper_root_file);
    let owner_root = std::fs::read(&owner_root_file).expect("owner cache root");
    let keeper_root = std::fs::read(&keeper_root_file).expect("keeper cache root");
    assert_ne!(owner_root, keeper_root);

    std::fs::write(coordination.path().join("drop-owner"), b"drop").expect("release owner");
    wait_for_acquisition_cache_checkpoint(&coordination.path().join("owner-dropped"));
    std::fs::write(coordination.path().join("check-keeper"), b"check").expect("release keeper");

    crate::test_support::child::assert_success(
        &owner.wait_with_output().expect("owner output"),
        "owner child failed",
    );
    crate::test_support::child::assert_success(
        &keeper.wait_with_output().expect("keeper output"),
        "keeper child failed",
    );
    assert!(coordination.path().join("keeper-preserved").is_file());
}

fn wait_for_acquisition_cache_checkpoint(path: &std::path::Path) {
    // Generous because each child is a fresh test-binary process that builds a controller harness
    // while the rest of the parallel suite saturates the CPU.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("acquisition cache checkpoint deadline: {}", path.display());
}

#[derive(Default)]
struct AcquisitionEvents {
    entries: Mutex<Vec<String>>,
    changed: Notify,
}

impl AcquisitionEvents {
    fn push(&self, deployment_id: &str, event: &str) {
        self.entries
            .lock()
            .expect("events")
            .push(format!("{deployment_id}:{event}"));
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Vec<String> {
        self.entries.lock().expect("events").clone()
    }
}

struct ControlledOciPreparer {
    state: Arc<Mutex<BTreeMap<String, ControlledAcquisition>>>,
    changed: Arc<Notify>,
    events: Arc<AcquisitionEvents>,
    model_file_blocks: Arc<Mutex<BTreeMap<String, bool>>>,
    local_images: TestImageStore,
}

impl ControlledOciPreparer {
    fn new(
        events: Arc<AcquisitionEvents>,
        model_file_blocks: Arc<Mutex<BTreeMap<String, bool>>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(BTreeMap::new())),
            changed: Arc::new(Notify::new()),
            events,
            model_file_blocks,
            local_images: TestImageStore::default(),
        }
    }

    fn block(&self, deployment_id: &str) {
        self.state
            .lock()
            .expect("acquisitions")
            .entry(deployment_id.to_owned())
            .or_default()
            .blocked = true;
    }

    fn set_outcome(&self, deployment_id: &str, outcome: AcquisitionOutcome, release: bool) {
        let mut state = self.state.lock().expect("acquisitions");
        let acquisition = state.entry(deployment_id.to_owned()).or_default();
        acquisition.outcome = outcome;
        if release {
            acquisition.blocked = false;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn finish_cancelled(&self, deployment_id: &str) {
        self.state
            .lock()
            .expect("acquisitions")
            .entry(deployment_id.to_owned())
            .or_default()
            .finish_cancelled = true;
        self.changed.notify_waiters();
    }

    fn release_waiters(&self) {
        self.changed.notify_waiters();
    }

    fn call_count(&self, deployment_id: &str) -> usize {
        self.state
            .lock()
            .expect("acquisitions")
            .get(deployment_id)
            .map(|acquisition| acquisition.call_count)
            .unwrap_or_default()
    }

    async fn prepare_controlled(
        self: Arc<Self>,
        request: dotcore_artifact::OciPreparationRequest,
        cancellation: tokio_util::sync::CancellationToken,
        output: dotcore_artifact::StepOutput,
    ) -> Result<(), dotcore_artifact::OciPreparationError> {
        // A retained local image ID names no deployment, so it bypasses the per-deployment gates.
        if request.source == dotcore_artifact::OciPreparationSource::LocalImageId {
            return self.local_images.prepare_local_image(request);
        }
        let deployment_id = request
            .reference
            .split('@')
            .next()
            .and_then(|repository| repository.rsplit('/').next())
            .expect("test OCI reference")
            .to_owned();
        loop {
            let notified = self.changed.notified();
            let blocked = self
                .model_file_blocks
                .lock()
                .expect("model file blocks")
                .get(&deployment_id)
                .copied()
                .unwrap_or(false);
            if !blocked {
                break;
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(dotcore_artifact::OciPreparationError::Cancelled),
                () = notified => {}
            }
        }
        let first_call = {
            let mut state = self.state.lock().expect("acquisitions");
            let acquisition = state.entry(deployment_id.clone()).or_default();
            acquisition.call_count += 1;
            acquisition.call_count == 1
        };
        self.events.push(&deployment_id, "acquiring engine image");
        // Only the first call pulls, as a real pull finds the image present afterwards.
        if first_call {
            output.line(
                dotcore_artifact::OutputStream::Stdout,
                format!("pulling the engine image of {deployment_id}"),
            );
        }
        loop {
            let notified = self.changed.notified();
            let blocked = self
                .state
                .lock()
                .expect("acquisitions")
                .entry(deployment_id.clone())
                .or_default()
                .blocked;
            if !blocked {
                break;
            }
            tokio::select! {
                () = cancellation.cancelled() => {
                    self.events.push(&deployment_id, "engine image acquisition cancelled");
                    loop {
                        let notified = self.changed.notified();
                        if self.state.lock().expect("acquisitions")
                            .entry(deployment_id.clone()).or_default().finish_cancelled {
                            return Err(dotcore_artifact::OciPreparationError::Cancelled);
                        }
                        notified.await;
                    }
                }
                () = notified => {}
            }
        }
        match self
            .state
            .lock()
            .expect("acquisitions")
            .entry(deployment_id.clone())
            .or_default()
            .outcome
            .clone()
        {
            AcquisitionOutcome::Ready => {
                self.events.push(&deployment_id, "engine image ready");
                Ok(())
            }
            AcquisitionOutcome::Failed(detail) => {
                let _ = detail;
                Err(dotcore_artifact::OciPreparationError::Daemon(
                    anyhow::anyhow!("injected OCI daemon failure"),
                ))
            }
        }
    }
}

impl dotcore_artifact::OciImagePreparer for ControlledOciPreparer {
    fn prepare(
        &self,
        request: dotcore_artifact::OciPreparationRequest,
        cancellation: tokio_util::sync::CancellationToken,
        output: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        let owned = Arc::new(Self {
            state: self.state.clone(),
            changed: self.changed.clone(),
            events: self.events.clone(),
            model_file_blocks: self.model_file_blocks.clone(),
            local_images: self.local_images.clone(),
        });
        Box::pin(async move {
            owned
                .prepare_controlled(request, cancellation, output)
                .await
        })
    }

    fn as_local_image_identity_resolver(
        &self,
    ) -> Option<&dyn dotcore_artifact::LocalImageIdentityResolver> {
        Some(&self.local_images)
    }
}

#[derive(Clone)]
struct ControlledAcquisition {
    blocked: bool,
    outcome: AcquisitionOutcome,
    call_count: usize,
    finish_cancelled: bool,
}

impl Default for ControlledAcquisition {
    fn default() -> Self {
        Self {
            blocked: false,
            outcome: AcquisitionOutcome::Ready,
            call_count: 0,
            finish_cancelled: false,
        }
    }
}

struct AcquisitionRuntime(Arc<AcquisitionRuntimeState>);

struct AcquisitionRuntimeState {
    events: Arc<AcquisitionEvents>,
    workloads: Mutex<BTreeMap<String, ObservedWorkload>>,
    create_counts: Mutex<BTreeMap<String, usize>>,
    created_images: Mutex<BTreeMap<String, String>>,
}

impl AcquisitionRuntimeState {
    fn new(events: Arc<AcquisitionEvents>) -> Self {
        Self {
            events,
            workloads: Mutex::new(BTreeMap::new()),
            create_counts: Mutex::new(BTreeMap::new()),
            created_images: Mutex::new(BTreeMap::new()),
        }
    }
}

impl RuntimeBackend for AcquisitionRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(dotcore_runtime_backends::simulated::BACKEND_ID.to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(self
            .0
            .workloads
            .lock()
            .expect("workloads")
            .values()
            .cloned()
            .map(WorkloadObservation::Running)
            .collect())))
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        let deployment_id = workload.correlation.owner_id.clone();
        self.0.events.push(&deployment_id, "runtime create");
        *self
            .0
            .create_counts
            .lock()
            .expect("create counts")
            .entry(deployment_id.clone())
            .or_default() += 1;
        self.0
            .created_images
            .lock()
            .expect("created images")
            .insert(
                deployment_id.clone(),
                workload.spec.executable.artifact.reference().to_owned(),
            );
        let observed = ObservedWorkload {
            workload_id: WorkloadId(format!("workload-{deployment_id}")),
            correlation: workload.correlation.clone(),
            backend_id: self.backend_id(),
            loopback_endpoints: vec![dotcore_runtime::LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: 1,
            }],
            services: vec![dotcore_runtime::WorkloadServiceBinding::native(
                format!("workload-{}", workload.correlation.owner_id),
                "http",
                workload.correlation.reconcile_generation,
            )],
            host_process: None,
        };
        self.0
            .workloads
            .lock()
            .expect("workloads")
            .insert(deployment_id, observed.clone());
        Box::pin(ready(Ok(WorkloadObservation::Running(observed))))
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
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.0
            .workloads
            .lock()
            .expect("workloads")
            .retain(|_, workload| &workload.workload_id != workload_id);
        Box::pin(ready(Ok(())))
    }
}

/// Gives one deployment its own digest-pinned engine image and matching launch identity.
fn set_test_oci_reference(
    engine: &mut dotcore_proto::dotcore::model::v1::ResolvedEngineArtifact,
    deployment_id: &str,
) {
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let artifact = engine.artifact.as_mut().expect("Engine artifact");
    artifact.kind = Some(
        dotcore_proto::dotcore::catalog::v1::engine_artifact::Kind::Oci(digest_reference(
            Some("registry.dotcore.test"),
            deployment_id,
            DIGEST,
        )),
    );
    engine.launch_identity = format!("registry.dotcore.test/{deployment_id}@sha256:{DIGEST}");
}

/// Turns the retained engine image into a local-development one launched from `image_id`.
fn retain_local_development_image(
    engine: &mut dotcore_proto::dotcore::model::v1::ResolvedEngineArtifact,
    image_id: &str,
) {
    let reference = engine
        .artifact
        .as_mut()
        .and_then(dotcore_domain::test_support::oci::reference_mut)
        .expect("Engine OCI reference");
    reference.tag = Some("dev".to_owned());
    reference.digest = None;
    engine.preparation_policy =
        dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy::LocalDevelopment as i32;
    engine.launch_identity = image_id.to_owned();
}

pub(super) fn assert_result(
    failure: TestFailure,
    result: Result<impl std::fmt::Debug, ModelLifecycleError>,
) {
    assert!(result.is_err_and(|error| failure.asserts(&error)));
}
