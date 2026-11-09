//! Fakes and fixtures for the application controller tests.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    future::{Future, poll_fn, ready},
    net::Ipv4Addr,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
};

use anyhow::anyhow;
use dotcore_catalog::{Catalog, CatalogBuilder, OciArtifactPolicy};
use dotcore_domain::test_support::oci::{ServiceArtifactOci as _, digest_artifact};
use dotcore_proto::dotcore::{
    application::private::v1::{
        ApplicationDeploymentStatus, ApplicationLifecycleObservation, ApplicationStatusStage,
    },
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationEndpoint, ApplicationLifecycleState,
        ApplicationReadinessState, ApplicationServiceLifecycleState, ApplicationServiceObservation,
        CreateApplicationDeploymentRequest, CurrentApplicationDeployment,
    },
    catalog::v1::{
        Application, ApplicationConfigurationBinding, ApplicationDeploymentInput,
        ApplicationEndpointProtocol, ApplicationGraph, ApplicationHttpReadiness,
        ApplicationInferenceRouteInput, ApplicationInferenceRouteProjection,
        ApplicationPrimaryEndpoint, ApplicationService, ApplicationServicePort,
        ApplicationServiceReadiness, application_configuration_binding,
        application_service_readiness,
    },
    deployment::v1::{ArtifactPreparationPolicy, ExactPlacement},
    site::v1::{
        ApplicationRuntimeBinding, ExecutionTarget, Site, SiteManagerRole, SiteNode,
        application_runtime_binding, execution_target,
    },
};
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackend, RuntimeBackendError, RuntimeFuture, WorkloadGroupId,
    WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadObservation, WorkloadStorageKey,
    WorkloadUnitName, WorkloadUnitState,
};
use dotcore_site::{
    node::{
        control::{
            AcceptedNodeObservationInput, ManagerNodeControlCoordinator, PublishedViewInput,
        },
        observation_configuration::NodeObservationConfigurationDigest,
        target_inventory::{
            RuntimePlatformObservation, StaticTargetCapabilitySource, TargetCapabilityInventory,
            TargetCapabilityReconciler, TargetCapabilitySource,
        },
    },
    repository::{ApplicationSiteSnapshot, SiteRepository},
    store::{FileCurrentState, InMemoryCurrentState},
    view::fencing::{ActionAssignment, ManagerSession, NodeIncarnation, NodeViewProjection},
};
use tokio::sync::Notify;

use super::*;

#[derive(Default)]
pub(super) struct FailingApplicationPreparer {
    pub(super) requests: Mutex<Vec<dotcore_artifact::OciPreparationRequest>>,
}

impl FailingApplicationPreparer {
    pub(super) fn requests(&self) -> Vec<dotcore_artifact::OciPreparationRequest> {
        self.requests.lock().expect("OCI requests").clone()
    }
}

impl dotcore_artifact::OciImagePreparer for FailingApplicationPreparer {
    fn prepare(
        &self,
        request: dotcore_artifact::OciPreparationRequest,
        _: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        self.requests.lock().expect("OCI requests").push(request);
        Box::pin(async { Err(dotcore_artifact::OciPreparationError::VerificationMismatch) })
    }
}

pub(super) struct RecordingApplicationPreparer {
    pub(super) requests: Mutex<Vec<dotcore_artifact::OciPreparationRequest>>,
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
}

impl dotcore_artifact::OciImagePreparer for RecordingApplicationPreparer {
    fn prepare(
        &self,
        request: dotcore_artifact::OciPreparationRequest,
        _: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        self.requests.lock().expect("OCI requests").push(request);
        self.calls.lock().expect("calls").push("prepare");
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
pub(super) struct StallingApplicationPreparer {
    pub(super) started: Arc<Notify>,
    pub(super) cancelled: Arc<Notify>,
}

impl dotcore_artifact::OciImagePreparer for StallingApplicationPreparer {
    fn prepare(
        &self,
        _: dotcore_artifact::OciPreparationRequest,
        cancellation: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        let started = Arc::clone(&self.started);
        let cancelled = Arc::clone(&self.cancelled);
        Box::pin(async move {
            started.notify_one();
            cancellation.cancelled().await;
            cancelled.notify_one();
            Err(dotcore_artifact::OciPreparationError::Cancelled)
        })
    }
}

pub(super) async fn assert_future_blocks<T>(mut future: Pin<&mut impl Future<Output = T>>) {
    poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("operation completed while its gate is occupied"),
    })
    .await;
}

#[derive(Clone)]
pub(super) struct TestApplicationDeclarationRepository {
    pub(super) state: Arc<Mutex<(u64, Vec<ApplicationDeploymentDeclaration>)>>,
    pub(super) pause_calls: Arc<Mutex<usize>>,
    pub(super) pause_failure: Arc<Mutex<Option<TestPauseFailure>>>,
    pub(super) upsert_failure: Arc<Mutex<Option<TestDeclarationWriteFailure>>>,
    pub(super) resume_calls: Arc<Mutex<usize>>,
    pub(super) resume_failure: Arc<Mutex<Option<TestResumeFailure>>>,
    pub(super) listener: Arc<Notify>,
    pub(super) execution_targets: Arc<Mutex<Vec<ExecutionTarget>>>,
}

#[derive(Clone, Copy)]
pub(super) enum TestPauseFailure {
    Storage,
    OutcomeUncertain,
}

#[derive(Clone, Copy)]
pub(super) enum TestDeclarationWriteFailure {
    OutcomeUncertain,
}

#[derive(Clone, Copy)]
pub(super) enum TestResumeFailure {
    Write,
    Rename,
    Sync,
}

impl TestApplicationDeclarationRepository {
    pub(super) fn open(_: &Path, _: &str) -> Result<Self, ApplicationRepositoryError> {
        Ok(Self {
            state: Arc::new(Mutex::new((0, Vec::new()))),
            pause_calls: Arc::new(Mutex::new(0)),
            pause_failure: Arc::new(Mutex::new(None)),
            upsert_failure: Arc::new(Mutex::new(None)),
            resume_calls: Arc::new(Mutex::new(0)),
            resume_failure: Arc::new(Mutex::new(None)),
            listener: Arc::new(Notify::new()),
            execution_targets: Arc::new(Mutex::new(vec![application_execution_target()])),
        })
    }

    /// Replaces the site's declared execution targets with `target` alone.
    pub(super) fn set_execution_target(&self, target: ExecutionTarget) {
        *self.execution_targets.lock().expect("execution targets") = vec![target];
    }

    pub(super) fn fail_next_pause(&self, failure: TestPauseFailure) {
        *self.pause_failure.lock().expect("pause failure") = Some(failure);
    }

    pub(super) fn fail_next_upsert(&self, failure: TestDeclarationWriteFailure) {
        *self
            .upsert_failure
            .lock()
            .expect("declaration upsert failure") = Some(failure);
    }

    pub(super) fn pause_calls(&self) -> usize {
        *self.pause_calls.lock().expect("pause calls")
    }

    pub(super) fn fail_next_resume(&self, failure: TestResumeFailure) {
        *self.resume_failure.lock().expect("resume failure") = Some(failure);
    }

    pub(super) fn resume_calls(&self) -> usize {
        *self.resume_calls.lock().expect("resume calls")
    }

    pub(super) fn seed(&self, deployment: ApplicationDeploymentDeclaration) {
        self.state
            .lock()
            .expect("declaration state")
            .1
            .push(deployment);
    }
}

impl ApplicationDeclarationRepository for TestApplicationDeclarationRepository {
    fn snapshot(
        &self,
    ) -> Result<
        crate::application::repository::ApplicationDeclarationSnapshot,
        ApplicationRepositoryError,
    > {
        let state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        Ok(
            crate::application::repository::ApplicationDeclarationSnapshot {
                site_id: "site-one".to_owned(),
                storage_revision: state.0,
                deployments: state.1.clone(),
            },
        )
    }

    fn upsert(
        &self,
        deployment: ApplicationDeploymentDeclaration,
        expected: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        if state.0 != expected {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        if let Some(index) = state.1.iter().position(|value| {
            value.application_deployment_id == deployment.application_deployment_id
        }) {
            state.1[index] = deployment;
        } else {
            state.1.push(deployment);
        }
        state.0 += 1;
        if matches!(
            self.upsert_failure
                .lock()
                .expect("declaration upsert failure")
                .take(),
            Some(TestDeclarationWriteFailure::OutcomeUncertain)
        ) {
            return Err(ApplicationRepositoryError::OutcomeUncertain {
                operation: "injected desired sync",
                path: "site.yaml".to_owned(),
                source: anyhow!("injected desired sync uncertainty"),
            });
        }
        Ok(
            crate::application::repository::ApplicationRepositoryMutation {
                storage_revision: state.0,
                record_revision: 0,
            },
        )
    }

    fn update_metadata(
        &self,
        deployment_id: &str,
        patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
        expected: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        if state.0 != expected {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        let deployment = state
            .1
            .iter_mut()
            .find(|value| value.application_deployment_id == deployment_id)
            .ok_or(ApplicationRepositoryError::NotFound)?;
        deployment.application_deployment_name = patch.display_name().to_owned();
        state.0 += 1;
        if matches!(
            self.upsert_failure
                .lock()
                .expect("declaration upsert failure")
                .take(),
            Some(TestDeclarationWriteFailure::OutcomeUncertain)
        ) {
            return Err(ApplicationRepositoryError::OutcomeUncertain {
                operation: "injected desired sync",
                path: "site.yaml".to_owned(),
                source: anyhow!("injected desired sync uncertainty"),
            });
        }
        Ok(
            crate::application::repository::ApplicationRepositoryMutation {
                storage_revision: state.0,
                record_revision: 0,
            },
        )
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected: u64,
        reserved_generation: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        if state.0 != expected {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        let deployment = state
            .1
            .iter_mut()
            .find(|value| value.application_deployment_id == deployment_id)
            .ok_or(ApplicationRepositoryError::NotFound)?;
        deployment.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
        deployment.lifecycle_generation = reserved_generation;
        state.0 += 1;
        Ok(
            crate::application::repository::ApplicationRepositoryMutation {
                storage_revision: state.0,
                record_revision: 0,
            },
        )
    }

    fn pause(
        &self,
        deployment_id: &str,
        expected: u64,
        reserved_generation: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        *self.pause_calls.lock().expect("pause calls") += 1;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        if state.0 != expected {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        let deployment = state
            .1
            .iter_mut()
            .find(|value| value.application_deployment_id == deployment_id)
            .ok_or(ApplicationRepositoryError::NotFound)?;
        match ApplicationDeploymentDesiredState::try_from(deployment.desired_state) {
            Ok(ApplicationDeploymentDesiredState::Present) => {
                deployment.desired_state = ApplicationDeploymentDesiredState::Paused as i32;
                deployment.lifecycle_generation = reserved_generation;
            }
            Ok(ApplicationDeploymentDesiredState::Paused) => {}
            _ => return Err(ApplicationRepositoryError::InvalidTransition),
        }
        state.0 += 1;
        match self.pause_failure.lock().expect("pause failure").take() {
            Some(TestPauseFailure::Storage) => {
                return Err(ApplicationRepositoryError::Storage {
                    operation: "injected pause persistence",
                    path: "site.yaml".to_owned(),
                    source: anyhow!("injected desired write failure"),
                });
            }
            Some(TestPauseFailure::OutcomeUncertain) => {
                return Err(ApplicationRepositoryError::OutcomeUncertain {
                    operation: "injected pause persistence",
                    path: "site.yaml".to_owned(),
                    source: anyhow!("injected desired sync uncertainty"),
                });
            }
            None => {}
        }
        Ok(
            crate::application::repository::ApplicationRepositoryMutation {
                storage_revision: state.0,
                record_revision: 0,
            },
        )
    }
    fn resume(
        &self,
        deployment_id: &str,
        expected: u64,
        reserved_generation: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        *self.resume_calls.lock().expect("resume calls") += 1;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        if state.0 != expected {
            return Err(ApplicationRepositoryError::RevisionConflict);
        }
        let deployment = state
            .1
            .iter_mut()
            .find(|value| value.application_deployment_id == deployment_id)
            .ok_or(ApplicationRepositoryError::NotFound)?;
        match ApplicationDeploymentDesiredState::try_from(deployment.desired_state) {
            Ok(ApplicationDeploymentDesiredState::Paused) => {
                deployment.desired_state = ApplicationDeploymentDesiredState::Present as i32;
                deployment.lifecycle_generation = reserved_generation;
            }
            Ok(ApplicationDeploymentDesiredState::Present) => {}
            _ => return Err(ApplicationRepositoryError::InvalidTransition),
        }
        state.0 += 1;
        match self.resume_failure.lock().expect("resume failure").take() {
            Some(TestResumeFailure::Write) => {
                return Err(ApplicationRepositoryError::Storage {
                    operation: "injected desired write",
                    path: "site.yaml".to_owned(),
                    source: anyhow!("injected desired write failure"),
                });
            }
            Some(TestResumeFailure::Rename) => {
                return Err(ApplicationRepositoryError::Storage {
                    operation: "injected desired rename",
                    path: "site.yaml".to_owned(),
                    source: anyhow!("injected desired rename failure"),
                });
            }
            Some(TestResumeFailure::Sync) => {
                return Err(ApplicationRepositoryError::OutcomeUncertain {
                    operation: "injected desired sync",
                    path: "site.yaml".to_owned(),
                    source: anyhow!("injected desired sync uncertainty"),
                });
            }
            None => {}
        }
        Ok(
            crate::application::repository::ApplicationRepositoryMutation {
                storage_revision: state.0,
                record_revision: 0,
            },
        )
    }
    fn declaration_snapshot(&self) -> Result<ApplicationSiteSnapshot, ApplicationRepositoryError> {
        Ok(ApplicationSiteSnapshot {
            site_id: "site-one".to_owned(),
            nodes: vec![SiteNode {
                node_id: "node-one".to_owned(),
                display_name: "Node One".to_owned(),
                ..Default::default()
            }],
            execution_targets: self
                .execution_targets
                .lock()
                .map_err(|_| ApplicationRepositoryError::Unavailable)?
                .clone(),
            deployments: self
                .state
                .lock()
                .map_err(|_| ApplicationRepositoryError::Unavailable)?
                .1
                .clone(),
        })
    }

    fn with_declared_deployment_ids<T>(
        &self,
        read: impl FnOnce(&std::collections::BTreeSet<String>) -> T,
    ) -> Result<T, ApplicationRepositoryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        Ok(read(
            &state
                .1
                .iter()
                .map(|deployment| deployment.application_deployment_id.clone())
                .collect(),
        ))
    }
}

impl TestDeclarations for TestApplicationDeclarationRepository {
    fn finalize_teardown(
        &self,
        teardown: &ApplicationDeploymentDeclaration,
    ) -> Result<(), ApplicationRepositoryError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApplicationRepositoryError::Unavailable)?;
        let Some(index) = state.1.iter().position(|value| {
            value.application_deployment_id == teardown.application_deployment_id
        }) else {
            return Ok(());
        };
        let declared = &state.1[index];
        if declared.lifecycle_generation != teardown.lifecycle_generation
            || declared.desired_state != ApplicationDeploymentDesiredState::Teardown as i32
            || declared
                .placement
                .as_ref()
                .map(|placement| &placement.site_node_id)
                != teardown
                    .placement
                    .as_ref()
                    .map(|placement| &placement.site_node_id)
        {
            return Err(ApplicationRepositoryError::TeardownMismatch);
        }
        state.1.remove(index);
        state.0 += 1;
        Ok(())
    }

    fn listener(&self) -> Arc<Notify> {
        Arc::clone(&self.listener)
    }
}

#[derive(Clone)]
pub(super) struct FailingCurrentApplicationRepository {
    pub(super) state: InMemoryCurrentState,
    pub(super) upsert_failure: Arc<Mutex<Option<TestCurrentWriteFailure>>>,
    pub(super) status_after_next_read: Arc<Mutex<Option<TestNodeStatus>>>,
}

/// A node status whose acceptance lands right after the next current-state read, unless the
/// declaration guard it needs is held at that moment; it then stays pending.
pub(super) struct TestNodeStatus {
    pub(super) guard: TestApplicationDeclarationRepository,
    pub(super) status: CurrentApplicationDeployment,
}

#[derive(Clone, Copy)]
pub(super) enum TestCurrentWriteFailure {
    Storage,
    OutcomeUncertain,
}

impl FailingCurrentApplicationRepository {
    pub(super) fn open(_: &Path, site_id: &str) -> Result<Self, ApplicationRepositoryError> {
        Ok(Self {
            state: InMemoryCurrentState::new(site_id)
                .map_err(|_| ApplicationRepositoryError::InvalidState)?,
            upsert_failure: Arc::new(Mutex::new(None)),
            status_after_next_read: Arc::new(Mutex::new(None)),
        })
    }

    pub(super) fn fail_next_upsert(&self, failure: TestCurrentWriteFailure) {
        *self.upsert_failure.lock().expect("current upsert failure") = Some(failure);
    }

    pub(super) fn accept_status_after_next_read(
        &self,
        guard: &TestApplicationDeclarationRepository,
        status: CurrentApplicationDeployment,
    ) {
        *self
            .status_after_next_read
            .lock()
            .expect("pending node status") = Some(TestNodeStatus {
            guard: guard.clone(),
            status,
        });
    }

    /// Accepts a still-pending node status; returns whether one was pending.
    pub(super) fn accept_pending_status(&self) -> bool {
        let Some(pending) = self
            .status_after_next_read
            .lock()
            .expect("pending node status")
            .take()
        else {
            return false;
        };
        let _guard = pending.guard.state.lock().expect("declaration guard");
        self.accept_status(pending.status);
        true
    }

    pub(super) fn accept_status(&self, status: CurrentApplicationDeployment) {
        let revision = CurrentApplicationRepository::snapshot(&self.state)
            .expect("current state")
            .deployments
            .into_iter()
            .find(|record| record.application_deployment_id == status.application_deployment_id)
            .map(|record| record.record_revision);
        CurrentApplicationRepository::upsert(&self.state, status, revision)
            .expect("accepted node status");
    }
}

impl ApplicationLifecycleGenerations for FailingCurrentApplicationRepository {
    fn reserve_lifecycle_generation(
        &self,
        deployment_id: &str,
    ) -> Result<u64, ApplicationRepositoryError> {
        self.state.reserve_lifecycle_generation(deployment_id)
    }
}

impl CurrentApplicationRepository for FailingCurrentApplicationRepository {
    fn snapshot(
        &self,
    ) -> Result<
        crate::application::repository::CurrentApplicationSnapshot,
        ApplicationRepositoryError,
    > {
        let snapshot = CurrentApplicationRepository::snapshot(&self.state);
        let mut pending = self
            .status_after_next_read
            .lock()
            .expect("pending node status");
        let guard = pending
            .as_ref()
            .map(|status| Arc::clone(&status.guard.state));
        if let Some(guard) = guard
            && let Ok(_guard) = guard.try_lock()
        {
            let status = pending.take().expect("pending node status").status;
            self.accept_status(status);
        }
        snapshot
    }

    fn upsert(
        &self,
        deployment: CurrentApplicationDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        match self
            .upsert_failure
            .lock()
            .expect("current upsert failure")
            .take()
        {
            Some(TestCurrentWriteFailure::Storage) => {
                return Err(ApplicationRepositoryError::Storage {
                    operation: "injected current write",
                    path: "state.pb".to_owned(),
                    source: anyhow!("injected current write failure"),
                });
            }
            Some(TestCurrentWriteFailure::OutcomeUncertain) => {
                return Err(ApplicationRepositoryError::OutcomeUncertain {
                    operation: "injected current sync",
                    path: "state.pb".to_owned(),
                    source: anyhow!("injected current sync uncertainty"),
                });
            }
            None => {}
        }
        CurrentApplicationRepository::upsert(&self.state, deployment, expected_record_revision)
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<
        crate::application::repository::ApplicationRepositoryMutation,
        ApplicationRepositoryError,
    > {
        CurrentApplicationRepository::remove(&self.state, deployment_id, expected_record_revision)
    }
}

pub(super) struct TestCurrentApplicationRepository;

impl TestCurrentApplicationRepository {
    pub(super) fn open(
        _: &Path,
        site_id: &str,
    ) -> Result<InMemoryCurrentState, ApplicationRepositoryError> {
        InMemoryCurrentState::new(site_id).map_err(|_| ApplicationRepositoryError::InvalidState)
    }
}

pub(super) fn write_lifecycle_site(
    directory: &Path,
    deployments: Vec<ApplicationDeploymentDeclaration>,
) -> std::path::PathBuf {
    let path = directory.join("site.yaml");
    let mut persisted = site();
    persisted.model_deployment_identity_allocator = Some(
        dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
            namespace: "0123456789abcdef".to_owned(),
            next_sequence: 1,
        },
    );
    persisted.application_deployments = deployments;
    fs::write(
        &path,
        dotcore_proto::yaml::to_string(&persisted).expect("site YAML"),
    )
    .expect("write site YAML");
    path
}

pub(super) fn generation_deployment(lifecycle_generation: u64) -> ApplicationDeploymentDeclaration {
    ApplicationDeploymentDeclaration {
        lifecycle_generation,
        ..deployment(ApplicationDeploymentDesiredState::Present)
    }
}

pub(super) fn declared_lifecycle(path: &Path) -> (ApplicationDeploymentDesiredState, u64) {
    let deployment = ApplicationDeclarationRepository::snapshot(
        &SiteRepository::load(path, crate::test_support::site_declaration_rules())
            .expect("reloaded site repository"),
    )
    .expect("reloaded declarations")
    .deployments
    .into_iter()
    .find(|deployment| deployment.application_deployment_id == "open-webui-1")
    .expect("declared deployment");
    (
        ApplicationDeploymentDesiredState::try_from(deployment.desired_state)
            .expect("desired state"),
        deployment.lifecycle_generation,
    )
}

pub(super) fn application_high_water(current: &FileCurrentState) -> Option<u64> {
    current
        .lifecycle_high_water(
            dotcore_domain::deployment::DeploymentOwner::Application,
            "open-webui-1",
        )
        .expect("lifecycle high-water")
}

pub(super) fn http_readiness_deployment() -> ApplicationDeploymentDeclaration {
    let mut desired = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0]
        .readiness = Some(ApplicationServiceReadiness {
        check: Some(application_service_readiness::Check::HttpGet(
            ApplicationHttpReadiness {
                port_name: "http".to_owned(),
                path: "/health".to_owned(),
            },
        )),
    });
    desired
}

/// Apps-controller owner over a site with the node `node-one`.
pub(super) fn manager_controller(
    desired: SiteRepository,
    current: FileCurrentState,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
) -> ApplicationController<SiteRepository, FileCurrentState> {
    let mut local = application();
    local.application_id = "local-webui".to_owned();
    local.graph.as_mut().expect("graph").services[0].artifact = Some(local_artifact("open-webui"));
    ApplicationController::new(
        Arc::new(
            CatalogBuilder::new()
                .oci_artifact_policy(OciArtifactPolicy::Development)
                .engine_validator(crate::model::engine::declaration_validator())
                .application("open-webui", application())
                .application("local-webui", local)
                .build()
                .expect("Catalog"),
        ),
        desired,
        current,
        ApplicationControllerConfiguration {
            target_capabilities,
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("manager apps controller")
}

pub(super) fn node_target_capabilities() -> Arc<dyn TargetCapabilitySource> {
    Arc::new(StaticTargetCapabilitySource::new(
        TargetCapabilityReconciler::reconcile(
            [("application-target-one", "node-one")],
            [RuntimePlatformObservation::available(
                "application-target-one",
                dotcore_domain::ExecutionPlatform::linux_amd64(),
            )],
            std::iter::empty(),
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        ),
    ))
}

pub(super) async fn listed_state(
    controller: &ApplicationController<SiteRepository, FileCurrentState>,
    deployment_id: &str,
) -> ApplicationDeploymentState {
    controller
        .get_deployment(deployment_id.to_owned())
        .await
        .expect("listed deployment")
        .state
        .expect("deployment state")
}

/// Writes [`node_site`] after `customize` and opens its site and current state.
/// Writes [`node_site`], changed by `customize`, to `site.yaml` in `directory` and opens its
/// site and current state.
pub(super) fn open_node_site_with(
    directory: &Path,
    customize: impl FnOnce(&mut Site),
) -> (SiteRepository, FileCurrentState) {
    let mut site = node_site(Vec::new());
    customize(&mut site);
    let path = directory.join("site.yaml");
    fs::write(
        &path,
        dotcore_proto::yaml::to_string(&site).expect("site YAML"),
    )
    .expect("node site fixture");
    (
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("validated node site"),
        FileCurrentState::open(&path, "site-one").expect("current state"),
    )
}

/// The manager side of node `node-one`: its node-control coordinator, the views published to
/// it, and the application status it reports, accepted by the real apps-controller section.
pub(super) struct ConnectedWorker {
    pub(super) coordinator: ManagerNodeControlCoordinator,
    pub(super) section: super::ApplicationNodeViewSection,
    pub(super) desired: SiteRepository,
    pub(super) incarnation: NodeIncarnation,
    pub(super) observation_revision: u64,
    pub(super) status_sequence: u64,
}

impl ConnectedWorker {
    pub(super) fn new(desired: SiteRepository, current: FileCurrentState) -> Self {
        Self {
            coordinator: ManagerNodeControlCoordinator::on_system_clock(
                ManagerSession::generate().expect("manager session"),
            ),
            section: super::ApplicationNodeViewSection::new(
                test_inference_route_resolver(),
                desired.clone(),
                current,
            ),
            desired,
            incarnation: NodeIncarnation::parse("1".repeat(32)).expect("node incarnation"),
            observation_revision: 0,
            status_sequence: 0,
        }
    }

    /// Accepts a fresh node observation, as the node reports every few seconds.
    pub(super) fn connect(&mut self) {
        self.observation_revision += 1;
        self.coordinator
            .register_observation(
                &dotcore_proto::dotcore::site::private::v1::NodeObservation {
                    site_id: "site-one".to_owned(),
                    node_id: "node-one".to_owned(),
                    incarnation_id: self.incarnation.as_str().to_owned(),
                    revision: self.observation_revision,
                    snapshot: None,
                    observation_configuration_sha256: "0".repeat(64),
                    private_port: None,
                },
                AcceptedNodeObservationInput {
                    node_id: "node-one".to_owned(),
                    incarnation: self.incarnation.clone(),
                    revision: self.observation_revision,
                    snapshot: Default::default(),
                    observation_configuration_sha256: NodeObservationConfigurationDigest::parse(
                        "0".repeat(64),
                    )
                    .expect("configuration digest"),
                    received_at: Default::default(),
                    private_port: None,
                },
            )
            .expect("node observation");
    }

    pub(super) fn assignments(
        &self,
    ) -> Vec<dotcore_proto::dotcore::application::private::v1::ApplicationAssignment> {
        use dotcore_site::view::desired::ApplicationNodeViewOwner as _;
        let declarations = self
            .desired
            .application_snapshot()
            .expect("Site")
            .deployments
            .into_iter()
            .filter(|declaration| {
                declaration
                    .placement
                    .as_ref()
                    .is_some_and(|placement| placement.site_node_id == "node-one")
            })
            .collect::<Vec<_>>();
        self.section.assignments(&declarations)
    }

    /// Publishes the node's current view and returns its reference.
    pub(super) fn publish(
        &mut self,
    ) -> dotcore_proto::dotcore::site::private::v1::NodeViewReference {
        let projection = NodeViewProjection::new(vec![application_execution_target()], Vec::new())
            .expect("node projection")
            .with_application_assignments(self.assignments())
            .expect("application assignments");
        self.coordinator
            .publish_view_for_test(
                "node-one",
                &self.incarnation,
                PublishedViewInput {
                    site_id: "site-one".to_owned(),
                    projection,
                },
            )
            .expect("published view")
    }

    /// Reports one application status for `view` and returns each evidence disposition.
    pub(super) fn report(
        &mut self,
        view: dotcore_proto::dotcore::site::private::v1::NodeViewReference,
        deployments: Vec<ApplicationDeploymentStatus>,
        cleanup_evidence: Vec<
            dotcore_proto::dotcore::application::private::v1::ApplicationCleanupEvidence,
        >,
    ) -> Vec<i32> {
        use dotcore_proto::dotcore::site::private::v1::{
            ControllerStatusReceipt, ControllerStatusSection, NodeControllerStatus,
            NodeControllerStatusAcceptance, controller_status_receipt, controller_status_section,
        };
        use dotcore_site::view::desired::ApplicationNodeViewOwner as _;
        self.status_sequence += 1;
        let application =
            dotcore_proto::dotcore::application::private::v1::ApplicationControllerStatusSection {
                deployments,
                cleanup_evidence,
            };
        let request = NodeControllerStatus {
            view: Some(view),
            status_sequence: self.status_sequence,
            sections: vec![ControllerStatusSection {
                payload: Some(controller_status_section::Payload::Application(
                    application.clone(),
                )),
            }],
        };
        let response = self
            .coordinator
            .accept_status("node-one", &self.incarnation, &request, |projection| {
                let receipts = self.section.accept_remote_status(
                    "node-one",
                    projection,
                    &application,
                    self.coordinator.manager_session(),
                    &self.incarnation,
                    request.status_sequence,
                );
                Ok::<_, std::convert::Infallible>(NodeControllerStatusAcceptance {
                    accepted_status_sequence: request.status_sequence,
                    receipts: receipts
                        .into_iter()
                        .map(|receipt| ControllerStatusReceipt {
                            payload: Some(controller_status_receipt::Payload::Application(receipt)),
                        })
                        .collect(),
                })
            })
            .expect("accepted status");
        response
            .receipts
            .into_iter()
            .filter_map(|receipt| match receipt.payload {
                Some(controller_status_receipt::Payload::Application(receipt)) => {
                    Some(receipt.disposition)
                }
                _ => None,
            })
            .collect()
    }
}

pub(super) fn node_status(
    deployment_id: &str,
    lifecycle_generation: u64,
    lifecycle: ApplicationLifecycleObservation,
    view: &dotcore_proto::dotcore::site::private::v1::NodeViewReference,
) -> ApplicationDeploymentStatus {
    ApplicationDeploymentStatus {
        application_deployment_id: deployment_id.to_owned(),
        lifecycle_generation,
        placement: Some(exact_placement()),
        realization_generation: Some(1),
        accepted_view_revision: view.view_revision,
        state_revision: 1,
        lifecycle: lifecycle as i32,
        readiness: ApplicationReadinessState::NotReady as i32,
        ..Default::default()
    }
}

pub(super) fn node_ready(
    deployment_id: &str,
    lifecycle_generation: u64,
    view: &dotcore_proto::dotcore::site::private::v1::NodeViewReference,
) -> ApplicationDeploymentStatus {
    ApplicationDeploymentStatus {
        stage: ApplicationStatusStage::Ready as i32,
        readiness: ApplicationReadinessState::Ready as i32,
        endpoint: Some(
            dotcore_proto::dotcore::application::private::v1::ApplicationPublishedEndpoint {
                host: WORKER_HOST.to_owned(),
                port: 41_000,
            },
        ),
        ..node_status(
            deployment_id,
            lifecycle_generation,
            ApplicationLifecycleObservation::Ready,
            view,
        )
    }
}

pub(super) fn controller_with_runtime<D, S>(
    desired: D,
    current: S,
    runtime: impl dotcore_runtime::WorkloadGroupBackend + 'static,
) -> TestController<D, S>
where
    D: TestDeclarations + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + Clone + 'static,
{
    controller_with_runtime_and_resolver(desired, current, runtime, test_inference_route_resolver())
}

pub(super) fn controller_with_runtime_and_artifacts<D, S>(
    desired: D,
    current: S,
    runtime: impl dotcore_runtime::WorkloadGroupBackend + 'static,
    artifacts: dotcore_artifact::ArtifactManager,
) -> TestController<D, S>
where
    D: TestDeclarations + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + Clone + 'static,
{
    TestController::new_with_artifact_manager(
        test_catalog(),
        desired,
        current,
        target_runtimes(runtime),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
        artifacts,
    )
    .expect("Apps controller")
}

pub(super) fn controller_with_runtime_and_resolver<D, S>(
    desired: D,
    current: S,
    runtime: impl dotcore_runtime::WorkloadGroupBackend + 'static,
    inference_route_resolver: Arc<dyn InferenceRouteResolver>,
) -> TestController<D, S>
where
    D: TestDeclarations + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + Clone + 'static,
{
    TestController::new(
        test_catalog(),
        desired,
        current,
        target_runtimes(runtime),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver,
        },
    )
    .expect("Apps controller")
}

/// Owner-only apps controller over the platform catalog, for tests of owner operations alone.
pub(super) fn owner_controller<D, S>(desired: D, current: S) -> ApplicationController<D, S>
where
    D: ApplicationDeclarationRepository + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + 'static,
{
    ApplicationController::new(
        test_catalog(),
        desired,
        current,
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller")
}

/// Synthetic platform catalog holding the base-url application, for tests whose catalog is incidental.
pub(super) fn test_catalog() -> Arc<Catalog> {
    Arc::new(
        CatalogBuilder::new()
            .oci_artifact_policy(OciArtifactPolicy::Development)
            .engine_validator(crate::model::engine::declaration_validator())
            .application("open-webui", base_url_application())
            .build()
            .expect("Catalog"),
    )
}

/// Runtime map binding `runtime` to the fixture's application target, without view fencing.
pub(super) fn target_runtimes(
    runtime: impl dotcore_runtime::WorkloadGroupBackend + 'static,
) -> crate::dispatch::application::ApplicationRuntimeMap {
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert("application-target-one", runtime)
        .expect("Runtime adapter");
    runtimes.build()
}

/// The owner-only apps controller paired with a node realizer over the same repositories.
///
/// The realizer reads the owner's declarations directly instead of a node view, writes the
/// owner's current records, and finalizes a completed teardown at once, so in-crate tests drive
/// owner operations and realization passes without a node view or status reports. The node
/// records start out owning every deployment declared when the pair is built.
pub(super) struct TestController<D, S> {
    pub(super) owner: ApplicationController<D, S>,
    pub(super) realizer: Arc<ApplicationRealizer>,
    pub(super) desired: D,
    pub(super) current: S,
    pub(super) _lifetime: Arc<TestRealizerLifetime>,
}

impl<D, S> TestController<D, S>
where
    D: TestDeclarations + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + Clone + 'static,
{
    pub(super) fn new(
        catalog: Arc<Catalog>,
        desired: D,
        current: S,
        runtimes: crate::dispatch::application::ApplicationRuntimeMap,
        configuration: ApplicationControllerConfiguration,
    ) -> Result<Self, ApplicationLifecycleError> {
        Self::with_artifacts(catalog, desired, current, runtimes, configuration, None)
    }

    pub(super) fn new_with_artifact_manager(
        catalog: Arc<Catalog>,
        desired: D,
        current: S,
        runtimes: crate::dispatch::application::ApplicationRuntimeMap,
        configuration: ApplicationControllerConfiguration,
        artifacts: dotcore_artifact::ArtifactManager,
    ) -> Result<Self, ApplicationLifecycleError> {
        Self::with_artifacts(
            catalog,
            desired,
            current,
            runtimes,
            configuration,
            Some(artifacts),
        )
    }

    pub(super) fn with_artifacts(
        catalog: Arc<Catalog>,
        desired: D,
        current: S,
        runtimes: crate::dispatch::application::ApplicationRuntimeMap,
        configuration: ApplicationControllerConfiguration,
        artifact_manager: Option<dotcore_artifact::ArtifactManager>,
    ) -> Result<Self, ApplicationLifecycleError> {
        let inference_route_resolver = Arc::clone(&configuration.inference_route_resolver);
        let records = Arc::new(TestNodeRecords::owning(
            desired
                .snapshot()
                .map_err(declaration_error)?
                .deployments
                .iter()
                .map(|deployment| deployment.application_deployment_id.clone()),
        ));
        let owner =
            ApplicationController::new(catalog, desired.clone(), current.clone(), configuration)?;
        let (test_desired, test_current) = (desired.clone(), current.clone());
        let realizer = Arc::new(ApplicationRealizer::new(
            Arc::new(TestRealizationInput {
                desired: desired.clone(),
            }),
            Arc::new(current.clone()),
            Arc::new(TestRealizationResults {
                desired,
                current,
                records: Arc::clone(&records),
            }),
            super::realizer::RealizerDependencies {
                runtimes,
                publication_host: Ipv4Addr::LOCALHOST,
                launch_pins: artifact_manager.clone().map(|artifacts| {
                    Arc::new(artifacts)
                        as Arc<dyn dotcore_domain::deployment::launch_pin::LaunchPinResolver>
                }),
                artifact_manager,
                // These realizers' fakes report no preparation output, so nothing is written.
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                inference_route_resolver,
                accelerator_attribution: None,
                resource_attribution: None,
                timing: dotcore_domain::timing::Timing::production().application,
                clock: Arc::new(dotcore_domain::clock::SystemClock),
                node: super::realizer::NodeRealization {
                    records,
                    reconciled: ReconciledAssignments::default(),
                },
            },
        ));
        let lifetime = Arc::new(TestRealizerLifetime {
            shutdown: realizer.shutdown_token(),
        });
        Ok(Self {
            owner,
            realizer,
            desired: test_desired,
            current: test_current,
            _lifetime: lifetime,
        })
    }

    /// Starts the realizer's serialized observer.
    pub(super) fn start_observer(&self) {
        ApplicationRealizer::start_observer(&self.realizer);
    }
}

impl<D: Clone, S: Clone> Clone for TestController<D, S> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            realizer: Arc::clone(&self.realizer),
            desired: self.desired.clone(),
            current: self.current.clone(),
            _lifetime: Arc::clone(&self._lifetime),
        }
    }
}

impl<D, S> ApplicationLifecycleController for TestController<D, S>
where
    D: ApplicationDeclarationRepository + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + 'static,
{
    fn list_deployments(
        &self,
    ) -> impl Future<Output = Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError>> + Send
    {
        self.owner.list_deployments()
    }

    fn get_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.get_deployment(deployment_id)
    }

    fn create_deployment(
        &self,
        request: CreateApplicationDeploymentRequest,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.create_deployment(request)
    }

    fn remove_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.remove_deployment(deployment_id)
    }

    fn update_metadata(
        &self,
        deployment_id: String,
        patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.update_metadata(deployment_id, patch)
    }

    fn pause_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.pause_deployment(deployment_id)
    }

    fn resume_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        self.owner.resume_deployment(deployment_id)
    }
}

/// Cancels the realizer's in-flight work once the last controller handle drops.
pub(super) struct TestRealizerLifetime {
    pub(super) shutdown: tokio_util::sync::CancellationToken,
}

impl Drop for TestRealizerLifetime {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Declaration repositories a test realizer reads intent from and finalizes teardowns in.
pub(super) trait TestDeclarations: ApplicationDeclarationRepository + Clone {
    /// Removes the declaration while it still matches the exact teardown.
    fn finalize_teardown(
        &self,
        teardown: &ApplicationDeploymentDeclaration,
    ) -> Result<(), ApplicationRepositoryError>;

    /// Listener woken when declarations change.
    fn listener(&self) -> Arc<Notify>;
}

/// Realization input reading every declaration of the owner's repository.
pub(super) struct TestRealizationInput<D> {
    pub(super) desired: D,
}

impl<D: TestDeclarations> ApplicationRealizationInput for TestRealizationInput<D> {
    fn realization_snapshot(
        &self,
    ) -> Result<ApplicationRealizationSnapshot, ApplicationLifecycleError> {
        let topology = self
            .desired
            .declaration_snapshot()
            .map_err(declaration_error)?;
        let deployments = self
            .desired
            .snapshot()
            .map_err(declaration_error)?
            .deployments;
        let assignments = deployments
            .iter()
            .map(|declaration| {
                (
                    declaration.application_deployment_id.clone(),
                    ActionAssignment::from(
                        &dotcore_proto::dotcore::application::private::v1::ApplicationAssignment {
                            declaration: Some(declaration.clone()),
                            linked_route: None,
                        },
                    ),
                )
            })
            .collect();
        Ok(ApplicationRealizationSnapshot {
            site: ApplicationSiteSnapshot {
                deployments,
                ..topology
            },
            assignments,
            view: None,
        })
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.desired.listener()
    }
}

/// Cleanup results that finalize the teardown declaration and its current record at once.
pub(super) struct TestRealizationResults<D, S> {
    pub(super) desired: D,
    pub(super) current: S,
    pub(super) records: Arc<TestNodeRecords>,
}

impl<D, S> ApplicationRealizationResults for TestRealizationResults<D, S>
where
    D: TestDeclarations,
    S: CurrentApplicationRepository,
{
    fn confirm_cleanup(
        &self,
        teardown: &ApplicationDeploymentDeclaration,
    ) -> Result<(), ApplicationLifecycleError> {
        self.records.confirm_cleanup(teardown);
        self.desired
            .finalize_teardown(teardown)
            .map_err(declaration_error)?;
        let existing = self
            .current
            .snapshot()
            .map_err(current_error)?
            .deployments
            .into_iter()
            .find(|record| record.application_deployment_id == teardown.application_deployment_id);
        if let Some(existing) = existing {
            self.current
                .remove(
                    &teardown.application_deployment_id,
                    existing.record_revision,
                )
                .map_err(current_error)?;
        }
        Ok(())
    }
}

/// In-memory node records: realization records and confirmed cleanups.
#[derive(Default)]
pub(super) struct TestNodeRecords {
    pub(super) realizations: Mutex<
        std::collections::BTreeMap<
            String,
            dotcore_proto::dotcore::site::store::v1::NodeApplicationRealization,
        >,
    >,
    pub(super) cleaned: Mutex<BTreeSet<(String, u64)>>,
}

impl TestNodeRecords {
    /// Records that this node already realized every deployment in `deployment_ids`.
    pub(super) fn owning(deployment_ids: impl IntoIterator<Item = String>) -> Self {
        let records = Self::default();
        records
            .realizations
            .lock()
            .expect("realization records")
            .extend(deployment_ids.into_iter().map(|deployment_id| {
                (
                    deployment_id.clone(),
                    dotcore_proto::dotcore::site::store::v1::NodeApplicationRealization {
                        application_deployment_id: deployment_id,
                        ..Default::default()
                    },
                )
            }));
        records
    }

    pub(super) fn confirm_cleanup(&self, teardown: &ApplicationDeploymentDeclaration) {
        self.realizations
            .lock()
            .expect("realization records")
            .remove(&teardown.application_deployment_id);
        self.cleaned.lock().expect("cleanup records").insert((
            teardown.application_deployment_id.clone(),
            teardown.lifecycle_generation,
        ));
    }
}

impl NodeRealizationRecords for TestNodeRecords {
    fn realization(
        &self,
        deployment_id: &str,
    ) -> Result<
        Option<dotcore_proto::dotcore::site::store::v1::NodeApplicationRealization>,
        ApplicationLifecycleError,
    > {
        Ok(self
            .realizations
            .lock()
            .expect("realization records")
            .get(deployment_id)
            .cloned())
    }

    fn record_realization(
        &self,
        realization: dotcore_proto::dotcore::site::store::v1::NodeApplicationRealization,
    ) -> Result<(), ApplicationLifecycleError> {
        self.realizations
            .lock()
            .expect("realization records")
            .insert(realization.application_deployment_id.clone(), realization);
        Ok(())
    }

    fn cleanup_confirmed(
        &self,
        deployment_id: &str,
        lifecycle_generation: u64,
    ) -> Result<bool, ApplicationLifecycleError> {
        Ok(self
            .cleaned
            .lock()
            .expect("cleanup records")
            .contains(&(deployment_id.to_owned(), lifecycle_generation)))
    }
}

#[derive(Clone)]
pub(super) struct ObserverRuntime {
    pub(super) state: Arc<ObserverRuntimeState>,
}

pub(super) struct ObserverRuntimeState {
    pub(super) discoveries: AtomicUsize,
    pub(super) discovery: Arc<Notify>,
    pub(super) drop_probe: Option<Arc<Notify>>,
}

impl ObserverRuntime {
    pub(super) fn new() -> Self {
        Self::with_state(None)
    }

    pub(super) fn with_drop_probe(drop_probe: Arc<Notify>) -> Self {
        Self::with_state(Some(drop_probe))
    }

    pub(super) fn with_state(drop_probe: Option<Arc<Notify>>) -> Self {
        Self {
            state: Arc::new(ObserverRuntimeState {
                discoveries: AtomicUsize::new(0),
                discovery: Arc::new(Notify::new()),
                drop_probe,
            }),
        }
    }

    pub(super) fn discovery_count(&self) -> usize {
        self.state.discoveries.load(Ordering::SeqCst)
    }

    pub(super) fn next_discovery(&self) -> impl Future<Output = ()> + '_ {
        self.state.discovery.notified()
    }

    pub(super) fn discovery_listener(&self) -> Arc<Notify> {
        Arc::clone(&self.state.discovery)
    }
}

impl Drop for ObserverRuntimeState {
    fn drop(&mut self) {
        if let Some(drop_probe) = &self.drop_probe {
            drop_probe.notify_one();
        }
    }
}

impl dotcore_runtime::WorkloadGroupBackend for ObserverRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/observer-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        self.state.discoveries.fetch_add(1, Ordering::SeqCst);
        self.state.discovery.notify_one();
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

pub(super) struct FailingDiscoveryRuntime;

impl dotcore_runtime::WorkloadGroupBackend for FailingDiscoveryRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/failing-discovery-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Err(RuntimeBackendError::Docker(anyhow!(
            "injected runtime discovery failure"
        )))))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

pub(super) struct TeardownSequenceRuntime {
    pub(super) discoveries: Arc<Mutex<VecDeque<WorkloadGroupObservation>>>,
    pub(super) inspections: Arc<Mutex<VecDeque<WorkloadGroupObservation>>>,
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
    pub(super) retained_data: Option<Arc<Mutex<bool>>>,
    pub(super) data_removal_failures: Arc<Mutex<usize>>,
}

impl TeardownSequenceRuntime {
    pub(super) fn new(
        _launch: WorkloadGroupLaunch,
        discoveries: Vec<WorkloadGroupObservation>,
        inspections: Vec<WorkloadGroupObservation>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Self {
        Self {
            discoveries: Arc::new(Mutex::new(discoveries.into_iter().collect())),
            inspections: Arc::new(Mutex::new(inspections.into_iter().collect())),
            calls,
            retained_data: None,
            data_removal_failures: Arc::new(Mutex::new(0)),
        }
    }

    pub(super) fn with_retained_data(
        _launch: WorkloadGroupLaunch,
        discoveries: Vec<WorkloadGroupObservation>,
        inspections: Vec<WorkloadGroupObservation>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        retained_data: Arc<Mutex<bool>>,
    ) -> Self {
        Self {
            discoveries: Arc::new(Mutex::new(discoveries.into_iter().collect())),
            inspections: Arc::new(Mutex::new(inspections.into_iter().collect())),
            calls,
            retained_data: Some(retained_data),
            data_removal_failures: Arc::new(Mutex::new(0)),
        }
    }

    /// Fails the next `failures` data removals before removing the retained data.
    pub(super) fn with_data_removal_failures(self, failures: usize) -> Self {
        *self
            .data_removal_failures
            .lock()
            .expect("data removal failures") = failures;
        self
    }
}

impl dotcore_runtime::WorkloadGroupBackend for TeardownSequenceRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/teardown-sequence-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        let observation = self.discoveries.lock().expect("discoveries").pop_front();
        Box::pin(ready(Ok(observation.into_iter().collect())))
    }

    fn storage_exists<'a>(&'a self, _: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
        let retained = self
            .retained_data
            .as_ref()
            .map(|value| *value.lock().expect("retained data"))
            .unwrap_or(false);
        Box::pin(ready(Ok(retained)))
    }

    fn remove_storage<'a>(&'a self, _: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        let Some(retained) = self.retained_data.as_ref() else {
            return Box::pin(ready(Ok(())));
        };
        self.calls
            .lock()
            .expect("Runtime calls")
            .push("remove-data");
        let mut failures = self
            .data_removal_failures
            .lock()
            .expect("data removal failures");
        if *failures > 0 {
            *failures -= 1;
            return Box::pin(ready(Err(RuntimeBackendError::InvalidLaunch(anyhow!(
                "simulated application data removal failure"
            )))));
        }
        *retained.lock().expect("retained data") = false;
        Box::pin(ready(Ok(())))
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        _: &'a WorkloadGroupId,
        _: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        self.calls
            .lock()
            .expect("Runtime calls")
            .push("ensure-absent");
        let result = match self
            .inspections
            .lock()
            .expect("cleanup results")
            .pop_front()
        {
            Some(WorkloadGroupObservation::Absent { .. }) => Ok(()),
            Some(_) | None => Err(runtime_error()),
        };
        Box::pin(ready(result))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("inspect");
        Box::pin(ready(
            self.inspections
                .lock()
                .expect("inspections")
                .pop_front()
                .ok_or_else(runtime_error),
        ))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("Runtime calls").push("remove");
        Box::pin(ready(Ok(())))
    }
}

pub(super) struct PausedMatrixRuntime {
    pub(super) observations: Vec<WorkloadGroupObservation>,
    pub(super) start_observation: Option<WorkloadGroupObservation>,
    pub(super) create_observation: Option<WorkloadGroupObservation>,
    pub(super) inspect_observation: Option<WorkloadGroupObservation>,
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
    pub(super) removed_groups: Arc<Mutex<Vec<ObservedWorkloadGroup>>>,
}

impl PausedMatrixRuntime {
    pub(super) fn new(
        observations: Vec<WorkloadGroupObservation>,
        start_observation: Option<WorkloadGroupObservation>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Self {
        Self {
            observations,
            start_observation,
            create_observation: None,
            inspect_observation: None,
            calls,
            removed_groups: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn with_recovery(
        observations: Vec<WorkloadGroupObservation>,
        create_observation: WorkloadGroupObservation,
        calls: Arc<Mutex<Vec<&'static str>>>,
        removed_groups: Arc<Mutex<Vec<ObservedWorkloadGroup>>>,
    ) -> Self {
        Self {
            observations,
            start_observation: None,
            create_observation: Some(create_observation),
            inspect_observation: None,
            calls,
            removed_groups,
        }
    }

    pub(super) fn with_inspection(mut self, inspect_observation: WorkloadGroupObservation) -> Self {
        self.inspect_observation = Some(inspect_observation);
        self
    }
}

impl dotcore_runtime::WorkloadGroupBackend for PausedMatrixRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Ok(self.observations.clone())))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("create");
        Box::pin(ready(
            self.create_observation.clone().ok_or_else(runtime_error),
        ))
    }

    fn start_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("start");
        Box::pin(ready(
            self.start_observation.clone().ok_or_else(runtime_error),
        ))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("inspect");
        Box::pin(ready(
            self.inspect_observation.clone().ok_or_else(runtime_error),
        ))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("Runtime calls").push("remove");
        Box::pin(ready(
            self.create_observation
                .as_ref()
                .map_or_else(|| Err(runtime_error()), |_| Ok(())),
        ))
    }

    fn remove_observed_workload_group<'a>(
        &'a self,
        group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("Runtime calls").push("remove");
        self.removed_groups
            .lock()
            .expect("Removed workload groups")
            .push(group.clone());
        Box::pin(ready(Ok(())))
    }

    fn stop_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("Runtime calls").push("stop");
        Box::pin(ready(Ok(())))
    }
}

pub(super) struct NetworkResidueRetryRuntime {
    pub(super) launch: WorkloadGroupLaunch,
    pub(super) removals: Arc<Mutex<usize>>,
    pub(super) creations: Arc<Mutex<usize>>,
}

impl dotcore_runtime::WorkloadGroupBackend for NetworkResidueRetryRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/network-residue-retry-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Ok(vec![WorkloadGroupObservation::Observed(
            ObservedWorkloadGroup {
                group_id: self.launch.group_id.clone(),
                correlation: self.launch.correlation.clone(),
                backend_id: self.backend_id(),
                units: self
                    .launch
                    .units
                    .keys()
                    .cloned()
                    .map(|name| {
                        (
                            name,
                            dotcore_runtime::WorkloadUnitObservation {
                                state: WorkloadUnitState::Absent,
                                ready_launch_gate: None,
                                host_process: None,
                            },
                        )
                    })
                    .collect(),
                assigned_endpoint: None,
            },
        )])))
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        _: &'a WorkloadGroupId,
        _: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        let mut removals = self.removals.lock().expect("removals");
        *removals += 1;
        if *removals == 1 {
            Box::pin(ready(Err(RuntimeBackendError::Docker(anyhow!(
                "injected network deletion failure"
            )))))
        } else {
            Box::pin(ready(Ok(())))
        }
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        *self.creations.lock().expect("creations") += 1;
        Box::pin(ready(Ok(running_group(group, self.backend_id()))))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        if *self.removals.lock().expect("removals") < 2 {
            return Box::pin(ready(Err(runtime_error())));
        }
        Box::pin(ready(Ok(WorkloadGroupObservation::Absent {
            group_id: self.launch.group_id.clone(),
            correlation: self.launch.correlation.clone(),
            backend_id: self.backend_id(),
        })))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        let mut removals = self.removals.lock().expect("removals");
        *removals += 1;
        if *removals == 1 {
            Box::pin(ready(Err(RuntimeBackendError::Docker(anyhow!(
                "injected network deletion failure"
            )))))
        } else {
            Box::pin(ready(Ok(())))
        }
    }
}

pub(super) struct ColdAbsentRuntime {
    pub(super) cleanup_identities: Arc<Mutex<Vec<(WorkloadGroupId, String)>>>,
}

impl RuntimeBackend for ColdAbsentRuntime {
    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/cold-absent-test".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

impl dotcore_runtime::WorkloadGroupBackend for ColdAbsentRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        RuntimeBackend::backend_id(self)
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        group_id: &'a WorkloadGroupId,
        owner_id: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        self.cleanup_identities
            .lock()
            .expect("cleanup identities")
            .push((group_id.clone(), owner_id.to_owned()));
        Box::pin(ready(Ok(())))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

pub(super) struct SelectiveCreateRuntime {
    pub(super) failures: Arc<Mutex<BTreeSet<String>>>,
    pub(super) missing_images: Arc<Mutex<BTreeSet<String>>>,
    pub(super) taken_ports: Arc<Mutex<BTreeSet<String>>>,
    pub(super) calls: Arc<Mutex<Vec<String>>>,
}

impl RuntimeBackend for SelectiveCreateRuntime {
    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/selective-create-test".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

impl dotcore_runtime::WorkloadGroupBackend for SelectiveCreateRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        RuntimeBackend::backend_id(self)
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls
            .lock()
            .expect("Runtime calls")
            .push(group.correlation.owner_id.clone());
        if self
            .missing_images
            .lock()
            .expect("missing images")
            .contains(&group.correlation.owner_id)
        {
            return Box::pin(ready(Err(RuntimeBackendError::ImageMissing(anyhow!(
                "secret-runtime-detail /var/lib/docker/container-deadbeef"
            )))));
        }
        if self
            .taken_ports
            .lock()
            .expect("taken ports")
            .contains(&group.correlation.owner_id)
        {
            return Box::pin(ready(Err(RuntimeBackendError::PublishedPortInUse(
                anyhow!("secret-runtime-detail port is already allocated"),
            ))));
        }
        if self
            .failures
            .lock()
            .expect("failures")
            .contains(&group.correlation.owner_id)
        {
            return Box::pin(ready(Err(RuntimeBackendError::InvalidLaunch(anyhow!(
                "secret-runtime-detail /var/lib/docker/container-deadbeef"
            )))));
        }
        Box::pin(ready(Ok(running_group(
            group,
            RuntimeBackend::backend_id(self),
        ))))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Ok(())))
    }
}

pub(super) fn group_observation(
    group: &WorkloadGroupLaunch,
    units: impl IntoIterator<Item = (&'static str, WorkloadUnitState)>,
    stale: bool,
) -> WorkloadGroupObservation {
    let group = observed_group(
        group,
        units,
        dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned()),
    );
    if stale {
        WorkloadGroupObservation::StaleConfiguration {
            group,
            reason: dotcore_runtime::WorkloadGroupStaleReason::PublicationHostChanged,
        }
    } else {
        WorkloadGroupObservation::Observed(group)
    }
}

pub(super) fn teardown_group_observation(
    group: &WorkloadGroupLaunch,
    units: impl IntoIterator<Item = (&'static str, WorkloadUnitState)>,
) -> WorkloadGroupObservation {
    WorkloadGroupObservation::Observed(observed_group(
        group,
        units,
        dotcore_runtime::RuntimeBackendId("runtime/teardown-sequence-test".to_owned()),
    ))
}

pub(super) fn observed_group(
    group: &WorkloadGroupLaunch,
    units: impl IntoIterator<Item = (&'static str, WorkloadUnitState)>,
    backend_id: dotcore_runtime::RuntimeBackendId,
) -> ObservedWorkloadGroup {
    ObservedWorkloadGroup {
        group_id: group.group_id.clone(),
        correlation: group.correlation.clone(),
        backend_id,
        units: units
            .into_iter()
            .map(|(name, state)| {
                (
                    WorkloadUnitName(name.to_owned()),
                    dotcore_runtime::WorkloadUnitObservation {
                        state,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                )
            })
            .collect(),
        assigned_endpoint: None,
    }
}

pub(super) fn running_group(
    group: &WorkloadGroupLaunch,
    backend_id: dotcore_runtime::RuntimeBackendId,
) -> WorkloadGroupObservation {
    WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
        group_id: group.group_id.clone(),
        correlation: group.correlation.clone(),
        backend_id,
        units: group
            .units
            .keys()
            .cloned()
            .map(|name| {
                (
                    name,
                    dotcore_runtime::WorkloadUnitObservation {
                        state: WorkloadUnitState::Running,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                )
            })
            .collect(),
        assigned_endpoint: None,
    })
}

pub(super) struct RestartRuntime {
    pub(super) calls: Arc<Mutex<Vec<&'static str>>>,
}

impl RuntimeBackend for RestartRuntime {
    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/restart-test".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }

    fn remove_workload<'a>(&'a self, _: &'a dotcore_runtime::WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(runtime_error())))
    }
}

impl dotcore_runtime::WorkloadGroupBackend for RestartRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        RuntimeBackend::backend_id(self)
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(ready(Ok(vec![WorkloadGroupObservation::Unknown {
            group_id: WorkloadGroupId("open-webui-1".to_owned()),
            backend_id: RuntimeBackend::backend_id(self),
        }])))
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        _: &'a WorkloadGroupId,
        _: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        self.calls
            .lock()
            .expect("Runtime calls")
            .push("ensure-absent");
        Box::pin(ready(Ok(())))
    }

    fn create_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("create");
        Box::pin(ready(Err(runtime_error())))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls.lock().expect("Runtime calls").push("inspect");
        Box::pin(ready(Ok(WorkloadGroupObservation::Observed(
            ObservedWorkloadGroup {
                group_id: group.group_id.clone(),
                correlation: group.correlation.clone(),
                backend_id: RuntimeBackend::backend_id(self),
                units: group
                    .units
                    .keys()
                    .cloned()
                    .map(|name| {
                        (
                            name,
                            dotcore_runtime::WorkloadUnitObservation {
                                state: WorkloadUnitState::Running,
                                ready_launch_gate: None,
                                host_process: None,
                            },
                        )
                    })
                    .collect(),
                assigned_endpoint: None,
            },
        ))))
    }

    fn remove_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("Runtime calls").push("remove");
        Box::pin(ready(Ok(())))
    }
}

/// Group runtime that retains every created group as running, recording each create and remove
/// with its launch so tests can observe whether reconciliation recomputed or recreated a group.
#[derive(Clone, Default)]
pub(super) struct RetainedGroupRuntime {
    pub(super) groups: Arc<Mutex<Vec<WorkloadGroupLaunch>>>,
    pub(super) calls: Arc<Mutex<Vec<(&'static str, WorkloadGroupLaunch)>>>,
}

impl RetainedGroupRuntime {
    pub(super) fn calls(&self) -> Vec<(&'static str, WorkloadGroupLaunch)> {
        self.calls.lock().expect("Runtime calls").clone()
    }
}

impl dotcore_runtime::WorkloadGroupBackend for RetainedGroupRuntime {
    fn execution_platform<'a>(
        &'a self,
        _: &'a str,
    ) -> dotcore_runtime::RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
        Box::pin(ready(Ok(dotcore_domain::ExecutionPlatform::linux_amd64())))
    }

    fn backend_id(&self) -> dotcore_runtime::RuntimeBackendId {
        dotcore_runtime::RuntimeBackendId("runtime/retained-group-test".to_owned())
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        let backend_id = dotcore_runtime::WorkloadGroupBackend::backend_id(self);
        Box::pin(ready(Ok(self
            .groups
            .lock()
            .expect("Runtime groups")
            .iter()
            .map(|group| running_group(group, backend_id.clone()))
            .collect())))
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        self.calls
            .lock()
            .expect("Runtime calls")
            .push(("create", group.clone()));
        self.groups
            .lock()
            .expect("Runtime groups")
            .push(group.clone());
        Box::pin(ready(Ok(running_group(
            group,
            dotcore_runtime::WorkloadGroupBackend::backend_id(self),
        ))))
    }

    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(ready(Ok(running_group(
            group,
            dotcore_runtime::WorkloadGroupBackend::backend_id(self),
        ))))
    }

    fn remove_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        self.calls
            .lock()
            .expect("Runtime calls")
            .push(("remove", group.clone()));
        self.groups
            .lock()
            .expect("Runtime groups")
            .retain(|retained| retained.group_id != group.group_id);
        Box::pin(ready(Ok(())))
    }
}

pub(super) fn stale_ready_state() -> ApplicationDeploymentState {
    ApplicationDeploymentState {
        lifecycle_state: ApplicationLifecycleState::Ready as i32,
        readiness_state: ApplicationReadinessState::Ready as i32,
        stage: "ready".to_owned(),
        services: vec![ApplicationServiceObservation {
            service_name: "web".to_owned(),
            lifecycle_state: ApplicationServiceLifecycleState::Running as i32,
            readiness_state: ApplicationReadinessState::Ready as i32,
        }],
        endpoint: Some(ApplicationEndpoint {
            url: "http://127.0.0.1:8080".to_owned(),
        }),
        ..Default::default()
    }
}

pub(super) fn stale_paused_state(
    lifecycle: ApplicationLifecycleState,
) -> ApplicationDeploymentState {
    let mut state = stale_ready_state();
    state.lifecycle_state = lifecycle as i32;
    state.stage = match lifecycle {
        ApplicationLifecycleState::Pausing => "pausing",
        ApplicationLifecycleState::Paused => "paused",
        _ => unreachable!("only paused lifecycle states are test inputs"),
    }
    .to_owned();
    state
}

pub(super) fn assert_suppressed(
    state: &ApplicationDeploymentState,
    lifecycle: ApplicationLifecycleState,
) {
    assert_eq!(state.lifecycle_state, lifecycle as i32);
    assert_eq!(
        state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert!(state.services.is_empty());
    assert_eq!(state.endpoint, None);
}

#[derive(Clone)]
pub(super) struct CountingCreateInferenceRouteResolver {
    pub(super) calls: Arc<AtomicUsize>,
}

impl InferenceRouteResolver for CountingCreateInferenceRouteResolver {
    fn resolve_for_create(
        &self,
        _: &str,
        _: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(resolved_route("route-one"))
    }

    fn project_current(
        &self,
        _: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        Ok(resolved_route("route-one"))
    }
}

pub(super) struct ChangingCurrentInferenceRouteResolver {
    pub(super) calls: Arc<AtomicUsize>,
}

impl InferenceRouteResolver for ChangingCurrentInferenceRouteResolver {
    fn resolve_for_create(
        &self,
        _: &str,
        _: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        unreachable!("launch compilation resolves only current routes")
    }

    fn project_current(
        &self,
        _: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(resolved_route(&format!("route-{call}")))
    }
}

pub(super) struct TestInferenceRouteResolver {
    pub(super) result: Result<ResolvedInferenceRoute, InferenceRouteResolutionError>,
}

impl InferenceRouteResolver for TestInferenceRouteResolver {
    fn resolve_for_create(
        &self,
        _: &str,
        _: u64,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        self.result.clone()
    }

    fn project_current(
        &self,
        _: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        self.result.clone()
    }
}

pub(super) fn test_target_capability_source() -> Arc<dyn TargetCapabilitySource> {
    let inventory = TargetCapabilityReconciler::reconcile(
        [("application-target-one", "node-one")],
        [RuntimePlatformObservation::available(
            "application-target-one",
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )],
        std::iter::empty(),
        dotcore_domain::timing::Timing::production()
            .node_control
            .capability_max_age,
    );
    Arc::new(StaticTargetCapabilitySource::new(inventory))
}

pub(super) fn test_inference_route_resolver() -> Arc<dyn InferenceRouteResolver> {
    test_inference_route_resolver_for("route-one")
}

pub(super) fn test_inference_route_resolver_for(
    route_name: &str,
) -> Arc<dyn InferenceRouteResolver> {
    test_inference_route_resolver_with(Ok(resolved_route(route_name)))
}

pub(super) fn test_inference_route_resolver_with(
    result: Result<ResolvedInferenceRoute, InferenceRouteResolutionError>,
) -> Arc<dyn InferenceRouteResolver> {
    Arc::new(TestInferenceRouteResolver { result })
}

pub(super) fn resolved_route(route_name: &str) -> ResolvedInferenceRoute {
    ResolvedInferenceRoute {
        inference_route_id: "route-id".to_owned(),
        route_name: route_name.to_owned(),
    }
}

pub(super) type DeploymentMutator = fn(&mut ApplicationDeploymentDeclaration);

pub(super) type LaunchProjectionChange = (&'static str, DeploymentMutator, Ipv4Addr);

pub(super) fn route_configuration_binding(
    binding_name: &str,
    environment_variable: &str,
    projection: ApplicationInferenceRouteProjection,
) -> ApplicationConfigurationBinding {
    ApplicationConfigurationBinding {
        binding_name: binding_name.to_owned(),
        environment_variable: environment_variable.to_owned(),
        source: Some(application_configuration_binding::Source::LiteralValue(
            "default-route".to_owned(),
        )),
        input: Some(ApplicationDeploymentInput {
            kind: Some(
                dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                    ApplicationInferenceRouteInput {
                        projection: projection as i32,
                    },
                ),
            ),
        }),
    }
}

pub(super) fn deployment(
    desired_state: ApplicationDeploymentDesiredState,
) -> ApplicationDeploymentDeclaration {
    ApplicationDeploymentDeclaration {
        application_deployment_id: "open-webui-1".to_owned(),
        application_deployment_name: "open-webui-1".to_owned(),
        placement: Some(exact_placement()),
        snapshot: Some(resolved_snapshot(application())),
        desired_state: desired_state as i32,
        storage_attachments: Vec::new(),
        configuration_attachments: Vec::new(),
        inference_route_id: None,
        lifecycle_generation: 1,
        node_resolved_launch_identities: Vec::new(),
        published_port: 30_000,
    }
}

pub(super) fn local_artifact(
    repository: &str,
) -> dotcore_proto::dotcore::catalog::v1::ApplicationServiceArtifact {
    let mut artifact = digest_artifact(
        Some("example.test"),
        repository,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        None,
    );
    let reference = artifact.oci_mut().expect("reference");
    reference.digest = None;
    reference.tag = Some("dev".to_owned());
    artifact
}

pub(super) fn controller_for_target_inventory(
    application: Application,
    inventory: TargetCapabilityInventory,
) -> ApplicationController<TestApplicationDeclarationRepository, InMemoryCurrentState> {
    controller_for_application_and_target_source(
        application,
        TestApplicationDeclarationRepository::open(Path::new("."), "site-one")
            .expect("declaration repository"),
        Arc::new(StaticTargetCapabilitySource::new(inventory)),
    )
}

pub(super) fn controller_for_application_and_target_source(
    application: Application,
    desired: TestApplicationDeclarationRepository,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
) -> ApplicationController<TestApplicationDeclarationRepository, InMemoryCurrentState> {
    ApplicationController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application)
                .build()
                .expect("Catalog"),
        ),
        desired,
        InMemoryCurrentState::new("site-one").expect("current repository"),
        ApplicationControllerConfiguration {
            target_capabilities,
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller")
}

pub(super) fn resolved_snapshot(
    application: Application,
) -> dotcore_proto::dotcore::application::v1::ResolvedApplicationDeploymentSnapshot {
    resolve_target_snapshot(&application, &site(), "application-target-one")
        .expect("resolved application snapshot")
}

/// Resolves the snapshot of `application` on the declared target `execution_target_id`, at the
/// target's declared platform.
pub(super) fn resolve_target_snapshot(
    application: &Application,
    site: &Site,
    execution_target_id: &str,
) -> Result<
    dotcore_proto::dotcore::application::v1::ResolvedApplicationDeploymentSnapshot,
    ApplicationLifecycleError,
> {
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == execution_target_id)
        .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
    let platform = target
        .platform
        .as_ref()
        .ok_or(ApplicationLifecycleError::FailedPrecondition)?;
    super::operations::resolve_snapshot(
        application,
        site,
        &ExactPlacement {
            site_node_id: target.site_node_id.clone(),
            execution_target_id: target.execution_target_id.clone(),
        },
        platform,
    )
}

pub(super) fn synchronize_service_selections(deployment: &mut ApplicationDeploymentDeclaration) {
    let snapshot = deployment.snapshot.as_mut().expect("snapshot");
    let graph = snapshot.graph.as_ref().expect("graph");
    snapshot.service_selections = graph
        .services
        .iter()
        .map(|service| {
            let reference = service
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.oci().cloned())
                .expect("service reference");
            let rendered =
                dotcore_domain::oci::render_reference(&reference).expect("rendered reference");
            let launch_identity = dotcore_domain::oci::canonical_digest_reference(&rendered)
                .expect("canonical reference")
                .expect("digest reference");
            dotcore_proto::dotcore::application::v1::ResolvedApplicationServiceSelection {
                service_name: service.service_name.clone(),
                preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                launch_identity,
            }
        })
        .collect();
}

pub(super) fn application_with_route_input() -> Application {
    let mut application = application();
    application.graph.as_mut().expect("graph").services[0].configuration_bindings =
        vec![route_configuration_binding(
            "route",
            "ROUTE_NAME",
            ApplicationInferenceRouteProjection::RouteName,
        )];
    application
}

/// The test application with an advanced `mode` choice and a basic `workers` count, each bound to
/// an environment variable of its web service.
pub(super) fn application_with_parameters() -> Application {
    use dotcore_proto::dotcore::catalog::v1::{
        ApplicationParameter, ParameterDefinition, StringEnumParameterSchema,
        UnsignedIntegerParameterSchema, parameter_definition::ParameterType,
    };

    let mut application = application();
    let graph = application.graph.as_mut().expect("graph");
    graph.parameters = vec![
        ApplicationParameter {
            definition: Some(ParameterDefinition {
                parameter_id: "mode".to_owned(),
                display_name: "Mode".to_owned(),
                description: String::new(),
                parameter_type: Some(ParameterType::StringEnum(StringEnumParameterSchema {
                    values: vec!["auto".to_owned(), "highvram".to_owned()],
                    default_value: Some("auto".to_owned()),
                })),
            }),
            advanced: true,
        },
        ApplicationParameter {
            definition: Some(ParameterDefinition {
                parameter_id: "workers".to_owned(),
                display_name: "Workers".to_owned(),
                description: String::new(),
                parameter_type: Some(ParameterType::UnsignedInteger(
                    UnsignedIntegerParameterSchema {
                        minimum: 1,
                        maximum: 8,
                        unit: String::new(),
                        default_value: Some(2),
                    },
                )),
            }),
            advanced: false,
        },
    ];
    graph.services[0].configuration_bindings = ["mode", "workers"]
        .into_iter()
        .map(|id| ApplicationConfigurationBinding {
            binding_name: id.to_owned(),
            environment_variable: id.to_uppercase(),
            source: Some(application_configuration_binding::Source::ParameterId(
                id.to_owned(),
            )),
            input: None,
        })
        .collect();
    application
}

/// The test application with its web service bound to the hosting node's inference base URL.
pub(super) fn base_url_application() -> Application {
    let mut application = application();
    application.graph.as_mut().expect("graph").services[0]
        .configuration_bindings
        .push(ApplicationConfigurationBinding {
            binding_name: "base-url".to_owned(),
            environment_variable: "OPENAI_API_BASE_URL".to_owned(),
            source: Some(
                application_configuration_binding::Source::ResolvedValueName(
                    "configured-inference-openai-base-url".to_owned(),
                ),
            ),
            input: None,
        });
    application
}

pub(super) fn application() -> Application {
    Application {
        application_id: "open-webui".to_owned(),
        display_name: "Open WebUI".to_owned(),
        description: String::new(),
        notes: String::new(),
        version: "1.0.0".to_owned(),
        graph: Some(ApplicationGraph {
            services: vec![ApplicationService {
                service_name: "web".to_owned(),
                artifact: Some(digest_artifact(
                    Some("example.test"),
                    "open-webui",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    None,
                )),
                command: vec!["web".to_owned()],
                ports: vec![ApplicationServicePort {
                    port_name: "http".to_owned(),
                    port_number: 8080,
                }],
                ..Default::default()
            }],
            primary_endpoint: Some(ApplicationPrimaryEndpoint {
                service_name: "web".to_owned(),
                port_name: "http".to_owned(),
                protocol: ApplicationEndpointProtocol::Http as i32,
            }),
            parameters: Vec::new(),
        }),
    }
}

pub(super) fn site() -> Site {
    Site {
        site_id: "site-one".to_owned(),
        model_deployment_identity_allocator: Some(crate::test_support::identity_allocator()),
        nodes: vec![SiteNode {
            node_id: "node-one".to_owned(),
            host: "127.0.0.1".to_owned(),
            display_name: "Node one".to_owned(),
            site_manager: Some(SiteManagerRole {}),
            manager_host: String::new(),
        }],
        execution_targets: vec![application_execution_target()],
        ..Default::default()
    }
}

pub(super) fn exact_placement() -> ExactPlacement {
    ExactPlacement {
        site_node_id: "node-one".to_owned(),
        execution_target_id: "application-target-one".to_owned(),
    }
}

pub(super) fn application_execution_target() -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: "application-target-one".to_owned(),
        display_name: "application-target-one".to_owned(),
        site_node_id: "node-one".to_owned(),
        platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
            ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                    Default::default(),
                )),
            },
        )),
    }
}

/// Declared host of the node SiteNode `node-one` in [`node_site`].
pub(super) const WORKER_HOST: &str = "10.0.0.2";

/// A site whose manager `manager-node` hosts `manager-target` and whose node `node-one` hosts
/// the application target that [`deployment`] places on, with `deployments` declared.
pub(super) fn node_site(deployments: Vec<ApplicationDeploymentDeclaration>) -> Site {
    let mut manager_target = application_execution_target();
    manager_target.execution_target_id = "manager-target".to_owned();
    manager_target.site_node_id = "manager-node".to_owned();
    Site {
        site_id: "site-one".to_owned(),
        model_deployment_identity_allocator: Some(crate::test_support::identity_allocator()),
        nodes: vec![
            SiteNode {
                node_id: "manager-node".to_owned(),
                host: "127.0.0.1".to_owned(),
                display_name: "Manager node".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            },
            SiteNode {
                node_id: "node-one".to_owned(),
                host: WORKER_HOST.to_owned(),
                display_name: "Worker one".to_owned(),
                site_manager: None,
                manager_host: String::new(),
            },
        ],
        execution_targets: vec![manager_target, application_execution_target()],
        application_deployments: deployments,
        ..Default::default()
    }
}

pub(super) fn runtime_error() -> RuntimeBackendError {
    RuntimeBackendError::InvalidLaunch(anyhow!("unused runtime operation"))
}

pub(super) fn test_directory() -> std::path::PathBuf {
    crate::test_support::canonical_tempdir()
        .expect("test directory")
        .keep()
}
