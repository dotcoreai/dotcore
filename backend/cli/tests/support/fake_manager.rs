//! An in-process stand-in for the running site manager's management API, which `dotc model`,
//! `dotc application` and `dotc code` contract tests start on a free loopback port and pass to
//! `dotc` through `DOTCORE_MANAGEMENT_ADDR`, for example seeded with one ready and one paused
//! deployment.
//!
//! It keeps deployments and inference routes in memory and changes deployments the way the manager
//! accepts a lifecycle request, without reconciling afterwards, so a test can seed state, run
//! `dotc`, and compare the state before and after. Every call is recorded, and a test can script one call to fail with a
//! chosen status or to answer without changing anything.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread,
    time::Duration,
};

use dotcore_proto::dotcore::{
    application::v1::{
        self as application, ApplicationDeployment, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationLifecycleState, ListApplicationDeploymentsRequest,
        ListApplicationDeploymentsResponse, PauseDeploymentRequest, PauseDeploymentResponse,
        RemoveApplicationDeploymentRequest, RemoveApplicationDeploymentResponse,
        ResolvedApplicationDeploymentSnapshot, ResumeDeploymentRequest, ResumeDeploymentResponse,
        application_deployment_service_server::{
            ApplicationDeploymentService, ApplicationDeploymentServiceServer,
        },
    },
    deployment::v1::ExactPlacement,
    model::v1::{
        CreateModelDeploymentRequest, CreateModelDeploymentResponse,
        EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
        FollowDeploymentLogsRequest, FollowDeploymentLogsResponse, ListModelDeploymentsRequest,
        ListModelDeploymentsResponse, ListModelProfileCompatibilityRequest,
        ListModelProfileCompatibilityResponse, ModelDeployment, ModelDeploymentDesiredState,
        ModelDeploymentIntent, ModelDeploymentSnapshot, ModelDeploymentState,
        ModelDeploymentStatus, PauseModelRequest, PauseModelResponse,
        PreviewLocalModelDeploymentRequest, PreviewLocalModelDeploymentResponse,
        ReadDeploymentLogsRequest, ReadDeploymentLogsResponse, RemoveModelDeploymentRequest,
        RemoveModelDeploymentResponse, ResolvedEngineArtifact,
        ResolvedLocalModelDeploymentSnapshot, ResumeModelRequest, ResumeModelResponse,
        UpdateModelDeploymentMetadataRequest, UpdateModelDeploymentMetadataResponse,
        UpdateModelDeploymentRequest, UpdateModelDeploymentResponse,
        model_deployment_service_server::{ModelDeploymentService, ModelDeploymentServiceServer},
        model_deployment_snapshot,
    },
    site::v1::{
        ConfiguredExecutionTarget, ExecutionTargetWorkloadKind,
        ListConfiguredExecutionTargetsRequest, ListConfiguredExecutionTargetsResponse,
        ListHardwareSnapshotsRequest, ListHardwareSnapshotsResponse, ListMachinesRequest,
        ListMachinesResponse,
        site_service_server::{SiteService, SiteServiceServer},
    },
};
use tokio::sync::oneshot;
use tonic::{
    Request, Response, Status,
    transport::{Server, server::TcpIncoming},
};

/// Node id of the node that seeded deployments run on.
pub const NODE_ID: &str = "node-1";

/// Display name the fake reports for [`NODE_ID`].
pub const NODE_NAME: &str = "gb10-host";

/// Model execution target of [`NODE_ID`] that seeded deployments are placed on.
pub const MODEL_TARGET_ID: &str = "node-1-model";

/// Application execution target of [`NODE_ID`] that seeded application deployments are placed on.
pub const APPLICATION_TARGET_ID: &str = "node-1-apps";

/// Display name of [`MODEL_TARGET_ID`].
pub const MODEL_TARGET_NAME: &str = "Simulated models";

/// Display name of [`APPLICATION_TARGET_ID`].
pub const APPLICATION_TARGET_NAME: &str = "Docker applications";

/// How long starting the fake may take before the test fails.
const START_DEADLINE: Duration = Duration::from_secs(30);

/// A running fake manager, stopped when dropped.
pub struct FakeManager {
    address: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeManager {
    /// Starts serving `state` on a free loopback port.
    pub fn start(state: FakeState) -> Self {
        let state = Arc::new(Mutex::new(state));
        let (bound, address) = mpsc::channel();
        let (shutdown, stop) = oneshot::channel::<()>();
        let served = Arc::clone(&state);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake manager runtime");
            runtime.block_on(async move {
                let incoming =
                    TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind fake manager");
                bound
                    .send(incoming.local_addr().expect("fake manager address"))
                    .expect("report fake manager address");
                Server::builder()
                    .add_service(ModelDeploymentServiceServer::new(FakeModels {
                        state: Arc::clone(&served),
                    }))
                    .add_service(ApplicationDeploymentServiceServer::new(FakeApplications {
                        state: Arc::clone(&served),
                    }))
                    .add_service(SiteServiceServer::new(FakeSite { state: served }))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop.await;
                    })
                    .await
                    .expect("fake manager serves");
            });
        });
        let address = address
            .recv_timeout(START_DEADLINE)
            .expect("fake manager starts");

        Self {
            address,
            state,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    /// The loopback address to pass as `--manager` or `DOTCORE_MANAGEMENT_ADDR`.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Locks the in-memory state so a test can read or change it between calls.
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake manager state")
    }

    /// Returns a copy of one seeded model deployment.
    pub fn model_deployment(&self, id: &str) -> ModelDeployment {
        self.state()
            .model_deployments
            .iter()
            .find(|deployment| deployment.model_deployment_id == id)
            .cloned()
            .unwrap_or_else(|| panic!("fake manager has no model deployment {id:?}"))
    }

    /// Returns a copy of one seeded application deployment.
    pub fn application_deployment(&self, id: &str) -> ApplicationDeployment {
        self.state()
            .application_deployments
            .iter()
            .find(|deployment| deployment.application_deployment_id == id)
            .cloned()
            .unwrap_or_else(|| panic!("fake manager has no application deployment {id:?}"))
    }

    /// Returns every call received so far, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }
}

impl Drop for FakeManager {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the fake serves and what it has received.
#[derive(Debug, Default)]
pub struct FakeState {
    /// Model deployments in list order.
    pub model_deployments: Vec<ModelDeployment>,

    /// Application deployments in list order.
    pub application_deployments: Vec<ApplicationDeployment>,

    /// Execution targets that carry the node display names.
    pub execution_targets: Vec<ConfiguredExecutionTarget>,

    /// Scripted answers that replace the normal behavior of one call.
    pub scripted: HashMap<Rpc, Scripted>,

    /// Every call received, oldest first.
    pub calls: Vec<Call>,
}

impl FakeState {
    /// Serves `deployments` with the model target of [`NODE_ID`] named [`NODE_NAME`].
    pub fn with_models(deployments: Vec<ModelDeployment>) -> Self {
        Self {
            model_deployments: deployments,
            execution_targets: vec![model_execution_target(NODE_ID, NODE_NAME, MODEL_TARGET_ID)],
            ..Self::default()
        }
    }
}

impl FakeState {
    /// Serves `deployments` with the application target of [`NODE_ID`] named [`NODE_NAME`].
    pub fn with_applications(deployments: Vec<ApplicationDeployment>) -> Self {
        Self {
            application_deployments: deployments,
            execution_targets: vec![application_execution_target(
                NODE_ID,
                NODE_NAME,
                APPLICATION_TARGET_ID,
            )],
            ..Self::default()
        }
    }
}

/// A faked management call.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Rpc {
    ListModelDeployments,

    PauseModel,

    ResumeModel,

    RemoveModelDeployment,

    ListApplicationDeployments,

    PauseApplicationDeployment,

    ResumeApplicationDeployment,

    RemoveApplicationDeployment,

    ListConfiguredExecutionTargets,
}

/// One received call and the deployment id it named, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Call {
    pub rpc: Rpc,
    pub id: Option<String>,
}

/// A scripted answer to one call.
#[derive(Clone, Debug)]
pub enum Scripted {
    /// Fail with this status, as the manager does when it refuses a request.
    Fail(Status),

    /// Answer with the deployment as it is, without changing it.
    Unchanged,
}

/// A local model deployment on [`MODEL_TARGET_ID`] of model `Gemma 4 E4B`, with the given name,
/// observed status, and desired state.
pub fn local_model_deployment(
    id: &str,
    name: &str,
    status: ModelDeploymentStatus,
    desired: ModelDeploymentDesiredState,
) -> ModelDeployment {
    ModelDeployment {
        model_deployment_id: id.to_owned(),
        model_deployment_name: name.to_owned(),
        intent: Some(ModelDeploymentIntent {
            placement: Some(ExactPlacement {
                site_node_id: NODE_ID.to_owned(),
                execution_target_id: MODEL_TARGET_ID.to_owned(),
            }),
            ..Default::default()
        }),
        snapshot: Some(ModelDeploymentSnapshot {
            source: Some(model_deployment_snapshot::Source::Local(
                ResolvedLocalModelDeploymentSnapshot {
                    model_id: "gemma-4-e4b".to_owned(),
                    model_display_name: "Gemma 4 E4B".to_owned(),
                    profile_id: "default".to_owned(),
                    engine_artifact: Some(ResolvedEngineArtifact {
                        engine_release_id: "llama-cpp".to_owned(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        }),
        state: Some(ModelDeploymentState {
            status: status as i32,
            ..Default::default()
        }),
        desired_state: desired as i32,
        ..Default::default()
    }
}

/// A local application deployment on [`APPLICATION_TARGET_ID`] of application `Open WebUI`, with
/// the given name, observed lifecycle, and desired state.
pub fn local_application_deployment(
    id: &str,
    name: &str,
    lifecycle: ApplicationLifecycleState,
    desired: ApplicationDeploymentDesiredState,
) -> ApplicationDeployment {
    let placement = ExactPlacement {
        site_node_id: NODE_ID.to_owned(),
        execution_target_id: APPLICATION_TARGET_ID.to_owned(),
    };
    ApplicationDeployment {
        application_deployment_id: id.to_owned(),
        application_deployment_name: name.to_owned(),
        placement: Some(placement.clone()),
        snapshot: Some(ResolvedApplicationDeploymentSnapshot {
            application_id: "open-webui".to_owned(),
            display_name: "Open WebUI".to_owned(),
            placement: Some(placement),
            ..Default::default()
        }),
        desired_state: desired as i32,
        state: Some(ApplicationDeploymentState {
            lifecycle_state: lifecycle as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A model execution target owned by a node with the given display name.
pub fn model_execution_target(
    site_node_id: &str,
    site_node_display_name: &str,
    execution_target_id: &str,
) -> ConfiguredExecutionTarget {
    ConfiguredExecutionTarget {
        execution_target_id: execution_target_id.to_owned(),
        display_name: MODEL_TARGET_NAME.to_owned(),
        site_node_id: site_node_id.to_owned(),
        site_node_display_name: site_node_display_name.to_owned(),
        workload_kind: ExecutionTargetWorkloadKind::Model as i32,
        ..Default::default()
    }
}

/// An application execution target owned by a node with the given display name.
pub fn application_execution_target(
    site_node_id: &str,
    site_node_display_name: &str,
    execution_target_id: &str,
) -> ConfiguredExecutionTarget {
    ConfiguredExecutionTarget {
        display_name: APPLICATION_TARGET_NAME.to_owned(),
        workload_kind: ExecutionTargetWorkloadKind::Application as i32,
        ..model_execution_target(site_node_id, site_node_display_name, execution_target_id)
    }
}

/// The model deployment service over the shared state.
struct FakeModels {
    state: Arc<Mutex<FakeState>>,
}

impl FakeModels {
    /// Records the call, applies its script, then moves the deployment to `desired` and, when
    /// given, to the observed `status` the manager reports on acceptance.
    fn lifecycle(
        &self,
        rpc: Rpc,
        id: String,
        desired: ModelDeploymentDesiredState,
        status: Option<ModelDeploymentStatus>,
    ) -> Result<ModelDeployment, Status> {
        let mut state = self.state.lock().expect("fake manager state");
        let scripted = record(&mut state, rpc, Some(&id))?;
        if id.is_empty() {
            return Err(Status::invalid_argument(
                "model lifecycle request contains an invalid ID",
            ));
        }
        let deployment = state
            .model_deployments
            .iter_mut()
            .find(|deployment| deployment.model_deployment_id == id)
            .ok_or_else(|| Status::not_found("model lifecycle resource was not found"))?;
        if scripted.is_none() {
            deployment.desired_state = desired as i32;
            if let Some(status) = status {
                deployment.state.get_or_insert_default().status = status as i32;
            }
        }
        Ok(deployment.clone())
    }
}

#[tonic::async_trait]
impl ModelDeploymentService for FakeModels {
    type FollowDeploymentLogsStream =
        futures_util::stream::Empty<Result<FollowDeploymentLogsResponse, Status>>;

    async fn list_model_deployments(
        &self,
        _request: Request<ListModelDeploymentsRequest>,
    ) -> Result<Response<ListModelDeploymentsResponse>, Status> {
        let mut state = self.state.lock().expect("fake manager state");
        record(&mut state, Rpc::ListModelDeployments, None)?;
        Ok(Response::new(ListModelDeploymentsResponse {
            deployments: state.model_deployments.clone(),
        }))
    }

    async fn pause_model(
        &self,
        request: Request<PauseModelRequest>,
    ) -> Result<Response<PauseModelResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::PauseModel,
            request.into_inner().model_deployment_id,
            ModelDeploymentDesiredState::Paused,
            None,
        )?;
        Ok(Response::new(PauseModelResponse {
            deployment: Some(deployment),
        }))
    }

    async fn resume_model(
        &self,
        request: Request<ResumeModelRequest>,
    ) -> Result<Response<ResumeModelResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::ResumeModel,
            request.into_inner().model_deployment_id,
            ModelDeploymentDesiredState::Present,
            None,
        )?;
        Ok(Response::new(ResumeModelResponse {
            deployment: Some(deployment),
        }))
    }

    async fn remove_model_deployment(
        &self,
        request: Request<RemoveModelDeploymentRequest>,
    ) -> Result<Response<RemoveModelDeploymentResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::RemoveModelDeployment,
            request.into_inner().model_deployment_id,
            ModelDeploymentDesiredState::Teardown,
            Some(ModelDeploymentStatus::Unloading),
        )?;
        Ok(Response::new(RemoveModelDeploymentResponse {
            deployment: Some(deployment),
        }))
    }

    async fn create_model_deployment(
        &self,
        _request: Request<CreateModelDeploymentRequest>,
    ) -> Result<Response<CreateModelDeploymentResponse>, Status> {
        Err(not_faked())
    }

    async fn preview_local_model_deployment(
        &self,
        _request: Request<PreviewLocalModelDeploymentRequest>,
    ) -> Result<Response<PreviewLocalModelDeploymentResponse>, Status> {
        Err(not_faked())
    }

    async fn estimate_local_model_resources(
        &self,
        _request: Request<EstimateLocalModelResourcesRequest>,
    ) -> Result<Response<EstimateLocalModelResourcesResponse>, Status> {
        Err(not_faked())
    }

    async fn list_model_profile_compatibility(
        &self,
        _request: Request<ListModelProfileCompatibilityRequest>,
    ) -> Result<Response<ListModelProfileCompatibilityResponse>, Status> {
        Err(not_faked())
    }

    async fn update_model_deployment(
        &self,
        _request: Request<UpdateModelDeploymentRequest>,
    ) -> Result<Response<UpdateModelDeploymentResponse>, Status> {
        Err(not_faked())
    }

    async fn read_deployment_logs(
        &self,
        _request: Request<ReadDeploymentLogsRequest>,
    ) -> Result<Response<ReadDeploymentLogsResponse>, Status> {
        Err(not_faked())
    }

    async fn follow_deployment_logs(
        &self,
        _request: Request<FollowDeploymentLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        Err(not_faked())
    }

    async fn update_model_deployment_metadata(
        &self,
        _request: Request<UpdateModelDeploymentMetadataRequest>,
    ) -> Result<Response<UpdateModelDeploymentMetadataResponse>, Status> {
        Err(not_faked())
    }
}

/// The application deployment service over the shared state.
struct FakeApplications {
    state: Arc<Mutex<FakeState>>,
}

impl FakeApplications {
    /// Records the call, applies its script, then moves the deployment to `desired` and, when
    /// given, to the observed `lifecycle` the manager reports on acceptance.
    fn lifecycle(
        &self,
        rpc: Rpc,
        id: String,
        desired: ApplicationDeploymentDesiredState,
        lifecycle: Option<ApplicationLifecycleState>,
    ) -> Result<ApplicationDeployment, Status> {
        let mut state = self.state.lock().expect("fake manager state");
        let scripted = record(&mut state, rpc, Some(&id))?;
        if id.is_empty() {
            return Err(Status::invalid_argument(
                "Application lifecycle request contains an invalid ID",
            ));
        }
        let deployment = state
            .application_deployments
            .iter_mut()
            .find(|deployment| deployment.application_deployment_id == id)
            .ok_or_else(|| Status::not_found("Application deployment was not found"))?;
        if scripted.is_none() {
            deployment.desired_state = desired as i32;
            if let Some(lifecycle) = lifecycle {
                deployment.state.get_or_insert_default().lifecycle_state = lifecycle as i32;
            }
        }
        Ok(deployment.clone())
    }
}

#[tonic::async_trait]
impl ApplicationDeploymentService for FakeApplications {
    type FollowDeploymentLogsStream =
        futures_util::stream::Empty<Result<application::FollowDeploymentLogsResponse, Status>>;

    async fn list_application_deployments(
        &self,
        _request: Request<ListApplicationDeploymentsRequest>,
    ) -> Result<Response<ListApplicationDeploymentsResponse>, Status> {
        let mut state = self.state.lock().expect("fake manager state");
        record(&mut state, Rpc::ListApplicationDeployments, None)?;
        Ok(Response::new(ListApplicationDeploymentsResponse {
            deployments: state.application_deployments.clone(),
        }))
    }

    async fn pause_deployment(
        &self,
        request: Request<PauseDeploymentRequest>,
    ) -> Result<Response<PauseDeploymentResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::PauseApplicationDeployment,
            request.into_inner().application_deployment_id,
            ApplicationDeploymentDesiredState::Paused,
            None,
        )?;
        Ok(Response::new(PauseDeploymentResponse {
            deployment: Some(deployment),
        }))
    }

    async fn resume_deployment(
        &self,
        request: Request<ResumeDeploymentRequest>,
    ) -> Result<Response<ResumeDeploymentResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::ResumeApplicationDeployment,
            request.into_inner().application_deployment_id,
            ApplicationDeploymentDesiredState::Present,
            None,
        )?;
        Ok(Response::new(ResumeDeploymentResponse {
            deployment: Some(deployment),
        }))
    }

    async fn remove_application_deployment(
        &self,
        request: Request<RemoveApplicationDeploymentRequest>,
    ) -> Result<Response<RemoveApplicationDeploymentResponse>, Status> {
        let deployment = self.lifecycle(
            Rpc::RemoveApplicationDeployment,
            request.into_inner().application_deployment_id,
            ApplicationDeploymentDesiredState::Teardown,
            Some(ApplicationLifecycleState::Stopping),
        )?;
        Ok(Response::new(RemoveApplicationDeploymentResponse {
            deployment: Some(deployment),
        }))
    }

    async fn get_application_deployment(
        &self,
        _request: Request<application::GetApplicationDeploymentRequest>,
    ) -> Result<Response<application::GetApplicationDeploymentResponse>, Status> {
        Err(not_faked())
    }

    async fn create_application_deployment(
        &self,
        _request: Request<application::CreateApplicationDeploymentRequest>,
    ) -> Result<Response<application::CreateApplicationDeploymentResponse>, Status> {
        Err(not_faked())
    }

    async fn read_deployment_logs(
        &self,
        _request: Request<application::ReadDeploymentLogsRequest>,
    ) -> Result<Response<application::ReadDeploymentLogsResponse>, Status> {
        Err(not_faked())
    }

    async fn follow_deployment_logs(
        &self,
        _request: Request<application::FollowDeploymentLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        Err(not_faked())
    }

    async fn update_application_deployment_metadata(
        &self,
        _request: Request<application::UpdateApplicationDeploymentMetadataRequest>,
    ) -> Result<Response<application::UpdateApplicationDeploymentMetadataResponse>, Status> {
        Err(not_faked())
    }
}

/// The site service over the shared state; only the execution targets are faked.
struct FakeSite {
    state: Arc<Mutex<FakeState>>,
}

#[tonic::async_trait]
impl SiteService for FakeSite {
    async fn list_configured_execution_targets(
        &self,
        _request: Request<ListConfiguredExecutionTargetsRequest>,
    ) -> Result<Response<ListConfiguredExecutionTargetsResponse>, Status> {
        let mut state = self.state.lock().expect("fake manager state");
        record(&mut state, Rpc::ListConfiguredExecutionTargets, None)?;
        Ok(Response::new(ListConfiguredExecutionTargetsResponse {
            execution_targets: state.execution_targets.clone(),
        }))
    }

    async fn list_machines(
        &self,
        _request: Request<ListMachinesRequest>,
    ) -> Result<Response<ListMachinesResponse>, Status> {
        Err(not_faked())
    }

    async fn list_hardware_snapshots(
        &self,
        _request: Request<ListHardwareSnapshotsRequest>,
    ) -> Result<Response<ListHardwareSnapshotsResponse>, Status> {
        Err(not_faked())
    }
}

/// Records one call and applies its script: a scripted failure is returned as the error, and a
/// scripted unchanged answer is returned so the caller skips its change.
fn record(state: &mut FakeState, rpc: Rpc, id: Option<&str>) -> Result<Option<Scripted>, Status> {
    state.calls.push(Call {
        rpc,
        id: id.map(str::to_owned),
    });
    match state.scripted.get(&rpc).cloned() {
        Some(Scripted::Fail(status)) => Err(status),
        scripted => Ok(scripted),
    }
}

fn not_faked() -> Status {
    Status::unimplemented("the fake manager does not serve this call")
}
