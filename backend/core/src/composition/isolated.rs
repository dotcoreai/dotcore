//! Gives tests and tools the model or application service of a fully composed site manager
//! without its network transports, for example to create a deployment before a backend starts
//! serving.
//!
//! The service is composed exactly as a serving backend composes it, including the manager's own
//! SiteNode realized in process, so there is one composition shape. The node's observation
//! reporter runs from composition; starting the observer starts the node's control stream and its
//! realizer for the service's deployments. Dropping the service stops every task it started, and
//! the site's owner and node stores are released once those tasks have stopped.

use std::sync::Mutex;

use dotcore_proto::dotcore::{
    application::v1::{
        CreateApplicationDeploymentRequest, CreateApplicationDeploymentResponse,
        FollowDeploymentLogsRequest as FollowApplicationLogsRequest,
        GetApplicationDeploymentRequest, GetApplicationDeploymentResponse,
        ListApplicationDeploymentsRequest, ListApplicationDeploymentsResponse,
        PauseDeploymentRequest, PauseDeploymentResponse,
        ReadDeploymentLogsRequest as ReadApplicationLogsRequest,
        ReadDeploymentLogsResponse as ReadApplicationLogsResponse,
        RemoveApplicationDeploymentRequest, RemoveApplicationDeploymentResponse,
        ResumeDeploymentRequest, ResumeDeploymentResponse,
        UpdateApplicationDeploymentMetadataRequest, UpdateApplicationDeploymentMetadataResponse,
        application_deployment_service_server::ApplicationDeploymentService as ApplicationDeploymentServiceContract,
    },
    model::v1::{
        CreateModelDeploymentRequest, CreateModelDeploymentResponse,
        EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
        FollowDeploymentLogsRequest, ListModelDeploymentsRequest, ListModelDeploymentsResponse,
        ListModelProfileCompatibilityRequest, ListModelProfileCompatibilityResponse,
        PauseModelRequest, PauseModelResponse, PreviewLocalModelDeploymentRequest,
        PreviewLocalModelDeploymentResponse, ReadDeploymentLogsRequest, ReadDeploymentLogsResponse,
        RemoveModelDeploymentRequest, RemoveModelDeploymentResponse, ResumeModelRequest,
        ResumeModelResponse, UpdateModelDeploymentMetadataRequest,
        UpdateModelDeploymentMetadataResponse, UpdateModelDeploymentRequest,
        UpdateModelDeploymentResponse,
        model_deployment_service_server::ModelDeploymentService as ModelDeploymentServiceContract,
    },
};
use dotcore_site::node::{
    agent::NodeReporterTask, control::ManagerNodeControlCoordinator,
    session::NodeControlSessionTask,
};
use tonic::{Request, Response, Status};

use super::{
    BackendServices, SelectedApplicationService, SelectedModelService, backend::ObserverStarters,
};

/// Model service of a fully composed site manager, without backend transports.
///
/// It dereferences to the selected model service and answers its management contract.
pub struct IsolatedModelService {
    service: SelectedModelService,
    manager: IsolatedManager,
}

impl IsolatedModelService {
    pub(super) fn new(services: BackendServices) -> Self {
        let BackendServices {
            model_service,
            application_service,
            manager,
            observers,
            node_lifecycle,
            node,
        } = services;
        // Applications and routes are not reconciled by an isolated model service.
        let ObserverStarters {
            reporter,
            node_control,
            model,
            application: _,
            route: _,
        } = observers;
        Self {
            service: model_service,
            manager: IsolatedManager::new(
                IsolatedStarters {
                    realizer: model,
                    node_control,
                },
                reporter(),
                IsolatedComposition {
                    _service: Box::new(application_service),
                    _manager: manager,
                    _coordinator: node_lifecycle,
                    _node: node,
                },
            ),
        }
    }

    /// Starts model reconciliation once: the manager node's control stream and model realizer,
    /// and the owner's upstream reconciler.
    pub fn start_observer(&self) {
        self.manager.start();
    }
}

impl std::ops::Deref for IsolatedModelService {
    type Target = SelectedModelService;

    fn deref(&self) -> &Self::Target {
        &self.service
    }
}

#[tonic::async_trait]
impl ModelDeploymentServiceContract for IsolatedModelService {
    type FollowDeploymentLogsStream =
        <SelectedModelService as ModelDeploymentServiceContract>::FollowDeploymentLogsStream;

    async fn list_model_deployments(
        &self,
        request: Request<ListModelDeploymentsRequest>,
    ) -> Result<Response<ListModelDeploymentsResponse>, Status> {
        self.service.list_model_deployments(request).await
    }

    async fn preview_local_model_deployment(
        &self,
        request: Request<PreviewLocalModelDeploymentRequest>,
    ) -> Result<Response<PreviewLocalModelDeploymentResponse>, Status> {
        self.service.preview_local_model_deployment(request).await
    }

    async fn estimate_local_model_resources(
        &self,
        request: Request<EstimateLocalModelResourcesRequest>,
    ) -> Result<Response<EstimateLocalModelResourcesResponse>, Status> {
        self.service.estimate_local_model_resources(request).await
    }

    async fn list_model_profile_compatibility(
        &self,
        request: Request<ListModelProfileCompatibilityRequest>,
    ) -> Result<Response<ListModelProfileCompatibilityResponse>, Status> {
        self.service.list_model_profile_compatibility(request).await
    }

    async fn create_model_deployment(
        &self,
        request: Request<CreateModelDeploymentRequest>,
    ) -> Result<Response<CreateModelDeploymentResponse>, Status> {
        self.service.create_model_deployment(request).await
    }

    async fn update_model_deployment_metadata(
        &self,
        request: Request<UpdateModelDeploymentMetadataRequest>,
    ) -> Result<Response<UpdateModelDeploymentMetadataResponse>, Status> {
        self.service.update_model_deployment_metadata(request).await
    }

    async fn update_model_deployment(
        &self,
        request: Request<UpdateModelDeploymentRequest>,
    ) -> Result<Response<UpdateModelDeploymentResponse>, Status> {
        self.service.update_model_deployment(request).await
    }

    async fn remove_model_deployment(
        &self,
        request: Request<RemoveModelDeploymentRequest>,
    ) -> Result<Response<RemoveModelDeploymentResponse>, Status> {
        self.service.remove_model_deployment(request).await
    }

    async fn pause_model(
        &self,
        request: Request<PauseModelRequest>,
    ) -> Result<Response<PauseModelResponse>, Status> {
        self.service.pause_model(request).await
    }

    async fn resume_model(
        &self,
        request: Request<ResumeModelRequest>,
    ) -> Result<Response<ResumeModelResponse>, Status> {
        self.service.resume_model(request).await
    }

    async fn read_deployment_logs(
        &self,
        request: Request<ReadDeploymentLogsRequest>,
    ) -> Result<Response<ReadDeploymentLogsResponse>, Status> {
        self.service.read_deployment_logs(request).await
    }

    async fn follow_deployment_logs(
        &self,
        request: Request<FollowDeploymentLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        self.service.follow_deployment_logs(request).await
    }
}

/// Application service of a fully composed site manager, without backend transports.
///
/// It dereferences to the selected application service and answers its management contract.
pub struct IsolatedApplicationService {
    service: SelectedApplicationService,
    manager: IsolatedManager,
}

impl IsolatedApplicationService {
    pub(super) fn new(services: BackendServices) -> Self {
        let BackendServices {
            model_service,
            application_service,
            manager,
            observers,
            node_lifecycle,
            node,
        } = services;
        // Models and routes are not reconciled by an isolated application service.
        let ObserverStarters {
            reporter,
            node_control,
            model: _,
            application,
            route: _,
        } = observers;
        Self {
            service: application_service,
            manager: IsolatedManager::new(
                IsolatedStarters {
                    realizer: application,
                    node_control,
                },
                reporter(),
                IsolatedComposition {
                    _service: Box::new(model_service),
                    _manager: manager,
                    _coordinator: node_lifecycle,
                    _node: node,
                },
            ),
        }
    }

    /// Starts application reconciliation once: the manager node's control stream and
    /// application realizer.
    pub fn start_observer(&self) {
        self.manager.start();
    }
}

impl std::ops::Deref for IsolatedApplicationService {
    type Target = SelectedApplicationService;

    fn deref(&self) -> &Self::Target {
        &self.service
    }
}

#[tonic::async_trait]
impl ApplicationDeploymentServiceContract for IsolatedApplicationService {
    type FollowDeploymentLogsStream =
        <SelectedApplicationService as ApplicationDeploymentServiceContract>::FollowDeploymentLogsStream;

    async fn list_application_deployments(
        &self,
        request: Request<ListApplicationDeploymentsRequest>,
    ) -> Result<Response<ListApplicationDeploymentsResponse>, Status> {
        self.service.list_application_deployments(request).await
    }

    async fn get_application_deployment(
        &self,
        request: Request<GetApplicationDeploymentRequest>,
    ) -> Result<Response<GetApplicationDeploymentResponse>, Status> {
        self.service.get_application_deployment(request).await
    }

    async fn create_application_deployment(
        &self,
        request: Request<CreateApplicationDeploymentRequest>,
    ) -> Result<Response<CreateApplicationDeploymentResponse>, Status> {
        self.service.create_application_deployment(request).await
    }

    async fn remove_application_deployment(
        &self,
        request: Request<RemoveApplicationDeploymentRequest>,
    ) -> Result<Response<RemoveApplicationDeploymentResponse>, Status> {
        self.service.remove_application_deployment(request).await
    }

    async fn update_application_deployment_metadata(
        &self,
        request: Request<UpdateApplicationDeploymentMetadataRequest>,
    ) -> Result<Response<UpdateApplicationDeploymentMetadataResponse>, Status> {
        self.service
            .update_application_deployment_metadata(request)
            .await
    }

    async fn pause_deployment(
        &self,
        request: Request<PauseDeploymentRequest>,
    ) -> Result<Response<PauseDeploymentResponse>, Status> {
        self.service.pause_deployment(request).await
    }

    async fn resume_deployment(
        &self,
        request: Request<ResumeDeploymentRequest>,
    ) -> Result<Response<ResumeDeploymentResponse>, Status> {
        self.service.resume_deployment(request).await
    }

    async fn read_deployment_logs(
        &self,
        request: Request<ReadApplicationLogsRequest>,
    ) -> Result<Response<ReadApplicationLogsResponse>, Status> {
        self.service.read_deployment_logs(request).await
    }

    async fn follow_deployment_logs(
        &self,
        request: Request<FollowApplicationLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        self.service.follow_deployment_logs(request).await
    }
}

/// Composed site manager behind an isolated service: its starters, the tasks it started, and the
/// capabilities it keeps alive.
struct IsolatedManager {
    starters: Mutex<Option<IsolatedStarters>>,
    tasks: Mutex<IsolatedTasks>,
    _composition: IsolatedComposition,
}

impl IsolatedManager {
    /// Keeps the composition alive with its node reporting through `reporter`, so its targets
    /// stay admissible before any observer starts.
    fn new(
        starters: IsolatedStarters,
        reporter: NodeReporterTask,
        composition: IsolatedComposition,
    ) -> Self {
        Self {
            starters: Mutex::new(Some(starters)),
            tasks: Mutex::new(IsolatedTasks {
                _reporter: reporter,
                _view: None,
            }),
            _composition: composition,
        }
    }

    /// Starts the service's realization once: its reconciliation, then the node's control stream.
    fn start(&self) {
        let Some(starters) = self
            .starters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return;
        };
        (starters.realizer)();
        let view = (starters.node_control)();
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            ._view = Some(view);
    }
}

/// Starters of the reconciliation an isolated service runs once its observer starts.
struct IsolatedStarters {
    realizer: Box<dyn FnOnce() + Send>,
    node_control: Box<dyn FnOnce() -> NodeControlSessionTask + Send>,
}

/// Tasks an isolated service started, stopped when the service drops.
struct IsolatedTasks {
    _reporter: NodeReporterTask,
    _view: Option<NodeControlSessionTask>,
}

/// Composed capabilities kept alive for as long as the isolated service exists, including the
/// owner service the isolated service does not expose.
struct IsolatedComposition {
    _service: Box<dyn std::any::Any + Send + Sync>,
    _manager: super::ManagerBackendServices,
    _coordinator: ManagerNodeControlCoordinator,
    _node: super::node::NodeLifecycle,
}
