//! The inference controller: the manager's lifecycle owner of every model
//! deployment in the site, and the node-local model realizer every SiteNode
//! runs.
//!
//! The owner accepts declarations, records intent and node-reported status,
//! reconciles upstream deployments, and holds no runtime. A node realizer
//! reads only its node's installed view and drives that node's runtime
//! workloads to match it. Neither owns catalog data, transport, or route
//! manager state; lifecycle operations and deployment logs reach the model
//! service through dedicated controller traits.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex},
};

use dotcore_artifact::ArtifactManager;
use dotcore_catalog::CatalogModelId;
use dotcore_domain::diagnostic::truncate_utf8_with_ellipsis;
use dotcore_proto::dotcore::model::v1::{
    CreateLocalModelDeploymentRequest, CreateUpstreamModelDeploymentRequest,
    EstimateLocalModelResourcesResponse, ListModelDeploymentsResponse,
    ListModelProfileCompatibilityResponse, ListModelRecommendationsResponse, ModelDeployment,
    ModelDeploymentDeclaration, ResolvedLocalModelDeploymentSnapshot,
    UpstreamModelDesiredConfiguration,
};
#[cfg(test)]
use dotcore_site::store::FileCurrentState;
use dotcore_site::{
    node::{
        control::ManagerNodeControlCoordinator,
        target_inventory::{TargetCapabilityInventory, TargetCapabilitySource},
    },
    persistence::SiteStorageError,
    repository::{ModelSiteSnapshot, SiteRepository, SiteRepositoryError},
};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{
    dispatch::model::RuntimeTargetMap,
    hardware::workloads::SiteWorkloadSource,
    model::{
        ModelTeardownApplicationRouteBlocker, domain::ModelDeploymentId,
        repository::ModelDeclarationRepositoryError, sizing_scenario::SizingScenario,
        state::CurrentStateRepository,
    },
};

mod accelerator_memory;
mod construction;
mod launch;
mod launch_pins;
mod logs;
pub(crate) mod metrics;
mod node_targets;
mod node_view;
mod operations;
mod ownership;
mod preparation;
mod projection;
mod realization;
mod reconciliation;
mod remote_status;
mod resource_telemetry;
pub(crate) mod serving_targets;
mod state;
mod teardown;
mod upstream;

#[cfg(test)]
pub(crate) mod test_support;

use anyhow::anyhow;

pub(crate) use accelerator_memory::AcceleratorAttribution;
pub(crate) use construction::{
    ArtifactDependencies, ModelControllerDependencies, NodeModelRealizerDependencies,
};
pub(crate) use launch_pins::NodePinnedModelInput;
pub use logs::ModelDeploymentLogController;
pub(crate) use logs::{MODEL_LOG_SOURCE_NAME, NodeModelLogs};
pub(crate) use node_targets::NodeTargetProjectorTask;
#[cfg(test)]
pub(crate) use node_view::deployment_status;
pub(crate) use node_view::{NodeModelStatus, NodeViewModelInput};
pub(crate) use projection::DeploymentProjection;
pub(crate) use realization::DurableModelRealizationResults;
use serving_targets::{
    LocalServingTargetsPublisher, LocalServingTargetsSource, ServingTargetsPublisher,
    ServingTargetsSource,
};

/// Explicit callable exposure requested for one model deployment load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelDeploymentExposureIntent {
    Direct { route_name: String },

    Hidden,
}

/// Read-only, typed model-owner input consumed by a node-local reconciler.
///
/// Implementations expose only the exact execution targets and declarations assigned to the
/// reconciler. They cannot accept management mutations or project public status.
pub(crate) trait ModelRealizationInput: Send + Sync {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError>;

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError>;

    fn register_change_listener(&self) -> Arc<Notify>;

    /// Local deployments withheld from [`Self::model_deployments`] until their development engine
    /// is pinned on this node. Only a node input withholds any.
    fn withheld_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(Vec::new())
    }

    /// Node-local engine launch pinning of a node input; `None` where the owner retains every
    /// launch identity.
    fn launch_pinning(&self) -> Option<&launch_pins::NodeLaunchPinning> {
        None
    }
}

/// Receives every deployment ID declared by the complete authoritative intent.
pub(crate) type DeclaredDeploymentIds<'a> =
    dyn FnMut(&BTreeSet<String>) -> Result<(), ModelLifecycleError> + 'a;

/// Completion boundary through which a node-local reconciler returns cleanup evidence.
pub(crate) trait ModelRealizationResults: Send + Sync {
    fn confirm_cleanup(
        &self,
        deployment: &ModelDeploymentDeclaration,
        realization_generation: Option<u64>,
    ) -> Result<(), ModelLifecycleError>;

    /// Reports whether confirming cleanup also removes the deployment's current record in the
    /// same state transaction, so the reconciler must not remove that record separately first.
    fn cleanup_removes_current_state(&self) -> bool {
        false
    }

    /// Reports whether cleanup of this exact deployment generation is already confirmed and
    /// retained, so a teardown still present in the input needs no further operational state.
    fn cleanup_confirmed(
        &self,
        _deployment: &ModelDeploymentDeclaration,
    ) -> Result<bool, ModelLifecycleError> {
        Ok(false)
    }
}

/// Manager-owned model declaration acceptance and mutation authority.
///
/// Implementations persist accepted declarations in the authoritative site. Node-local
/// reconciliation receives only [`ModelRealizationInput`] and cannot call these methods.
pub(crate) trait DesiredDeploymentRepository: ModelRealizationInput {
    fn replace_target_capabilities(&self, _inventory: TargetCapabilityInventory) {}

    /// Runs `read` with every declared deployment ID while holding the declaration guard, so no
    /// declaration mutation can interleave with it.
    fn with_declared_deployment_ids(
        &self,
        read: &mut DeclaredDeploymentIds<'_>,
    ) -> Result<(), ModelLifecycleError>;

    fn load_model(
        &self,
        model_id: &str,
        profile_id: &str,
        placement: &dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        exposure: &ModelDeploymentExposureIntent,
        name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError>;

    fn preview_local_model(
        &self,
        _request: &CreateLocalModelDeploymentRequest,
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "local deployment preview is unavailable",
        ))
    }

    fn estimate_local_model_resources(
        &self,
        _model_id: &str,
        _profile_id: &str,
        _configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        _target: Option<crate::model::repository::EstimateTarget<'_>>,
    ) -> Result<EstimateLocalModelResourcesResponse, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "local model resource estimation is unavailable",
        ))
    }

    fn profile_compatibility(
        &self,
        _model_id: Option<&str>,
        _target: Option<&dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
        _scenario: Option<&SizingScenario>,
        _inventory: Option<&TargetCapabilityInventory>,
        _workloads: &crate::hardware::capacity::NodeWorkloads,
        _now: std::time::SystemTime,
    ) -> Result<ListModelProfileCompatibilityResponse, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "model profile compatibility is unavailable",
        ))
    }

    fn model_recommendations(
        &self,
        _use_cases: &[dotcore_proto::dotcore::catalog::v1::UseCase],
        _inventory: Option<&TargetCapabilityInventory>,
        _deployments: &[ModelDeployment],
        _now: std::time::SystemTime,
    ) -> Result<ListModelRecommendationsResponse, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "model recommendations are unavailable",
        ))
    }

    fn load_upstream_model(
        &self,
        _request: &CreateUpstreamModelDeploymentRequest,
        _exposure: &ModelDeploymentExposureIntent,
        _name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "Upstream model declarations are unavailable",
        ))
    }

    fn update_metadata(
        &self,
        _deployment_id: &str,
        _patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "Model deployment metadata updates are unavailable",
        ))
    }

    fn update_upstream_model(
        &self,
        _deployment_id: &str,
        _replacement: &UpstreamModelDesiredConfiguration,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        Err(ModelDeclarationRepositoryError::InvalidState(
            "Upstream model declarations are unavailable",
        ))
    }

    fn unload_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError>;

    fn pause_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError>;

    fn resume_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError>;

    /// Removes a deployment whose teardown is finalized only while its declaration still matches
    /// that exact teardown; no path removes a deployment by identity alone.
    fn finalize_teardown(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Result<(), ModelDeclarationRepositoryError>;
}

/// Active controller interface used by the model service edge.
pub trait ModelLifecycleController: Send + Sync {
    fn list_deployments(
        &self,
    ) -> impl Future<Output = Result<ListModelDeploymentsResponse, ModelLifecycleError>> + Send;

    fn load_model(
        &self,
        model_id: CatalogModelId,
        profile_id: String,
        placement: dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        exposure: ModelDeploymentExposureIntent,
        name: Option<String>,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn preview_local_model(
        &self,
        _request: CreateLocalModelDeploymentRequest,
    ) -> impl Future<Output = Result<ResolvedLocalModelDeploymentSnapshot, ModelLifecycleError>> + Send
    {
        std::future::ready(Err(ModelLifecycleError::configuration(
            "local deployment preview is unavailable",
        )))
    }

    fn estimate_local_model_resources(
        &self,
        _model_id: String,
        _profile_id: String,
        _configuration: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        _placement: Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    ) -> impl Future<Output = Result<EstimateLocalModelResourcesResponse, ModelLifecycleError>> + Send
    {
        std::future::ready(Err(ModelLifecycleError::configuration(
            "local model resource estimation is unavailable",
        )))
    }

    /// Reports which catalog model profiles can run on which configured model execution targets,
    /// with each profile's sizing for an optional sizing scenario and its memory fit on them.
    fn list_profile_compatibility(
        &self,
        _model_id: Option<String>,
        _target: Option<dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
        _scenario: Option<SizingScenario>,
    ) -> impl Future<Output = Result<ListModelProfileCompatibilityResponse, ModelLifecycleError>> + Send
    {
        std::future::ready(Err(ModelLifecycleError::configuration(
            "model profile compatibility is unavailable",
        )))
    }

    /// Ranks the catalog's recommended model profiles for each of `use_cases` on the site's
    /// configured model execution targets, marking the profiles already deployed. Advisory: it
    /// never changes deployments, admission or placement.
    fn list_model_recommendations(
        &self,
        _use_cases: Vec<dotcore_proto::dotcore::catalog::v1::UseCase>,
    ) -> impl Future<Output = Result<ListModelRecommendationsResponse, ModelLifecycleError>> + Send
    {
        std::future::ready(Err(ModelLifecycleError::configuration(
            "model recommendations are unavailable",
        )))
    }

    fn load_upstream_model(
        &self,
        request: CreateUpstreamModelDeploymentRequest,
        exposure: ModelDeploymentExposureIntent,
        name: Option<String>,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn update_metadata(
        &self,
        deployment_id: ModelDeploymentId,
        patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn update_upstream_model(
        &self,
        deployment_id: ModelDeploymentId,
        replacement: UpstreamModelDesiredConfiguration,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn unload_model(
        &self,
        deployment_id: ModelDeploymentId,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn pause_model(
        &self,
        deployment_id: ModelDeploymentId,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn resume_model(
        &self,
        deployment_id: ModelDeploymentId,
    ) -> impl Future<Output = Result<ModelDeployment, ModelLifecycleError>> + Send;

    fn start_observer(&self) {}
}

/// Supplies private controller-published serving targets to the inference router.
pub(crate) trait ServingTargetsSourceProvider: Send + Sync {
    fn serving_targets(&self) -> ServingTargetsSource;
}

impl ModelRealizationInput for crate::model::repository::ModelDeclarationRepository {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        self.snapshot()
    }

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(self.snapshot()?.deployments)
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.site_repository().register_change_listener()
    }
}

impl DesiredDeploymentRepository for crate::model::repository::ModelDeclarationRepository {
    fn replace_target_capabilities(&self, inventory: TargetCapabilityInventory) {
        self.replace_target_capabilities(inventory);
    }

    fn with_declared_deployment_ids(
        &self,
        read: &mut DeclaredDeploymentIds<'_>,
    ) -> Result<(), ModelLifecycleError> {
        self.site_repository()
            .with_model_deployments(|deployments| {
                read(
                    &deployments
                        .iter()
                        .map(|deployment| deployment.model_deployment_id.clone())
                        .collect(),
                )
            })
            .map_err(state::desired_error)?
    }

    fn load_model(
        &self,
        model_id: &str,
        profile_id: &str,
        placement: &dotcore_proto::dotcore::deployment::v1::ExactPlacement,
        configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        exposure: &ModelDeploymentExposureIntent,
        name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.load_model(
            model_id,
            profile_id,
            placement,
            configuration,
            exposure,
            name,
        )
    }

    fn preview_local_model(
        &self,
        request: &CreateLocalModelDeploymentRequest,
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelDeclarationRepositoryError> {
        let configuration =
            request
                .configuration
                .as_ref()
                .ok_or(ModelDeclarationRepositoryError::InvalidState(
                    "local deployment preview has no configuration",
                ))?;
        self.preview_local_model(
            &request.model_id,
            &request.profile_id,
            request
                .placement
                .as_ref()
                .ok_or(ModelDeclarationRepositoryError::InvalidState(
                    "local deployment preview has no exact placement",
                ))?,
            configuration,
        )
    }

    fn estimate_local_model_resources(
        &self,
        model_id: &str,
        profile_id: &str,
        configuration: &dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
        target: Option<crate::model::repository::EstimateTarget<'_>>,
    ) -> Result<EstimateLocalModelResourcesResponse, ModelDeclarationRepositoryError> {
        self.estimate_local_model_resources(model_id, profile_id, configuration, target)
    }

    fn profile_compatibility(
        &self,
        model_id: Option<&str>,
        target: Option<&dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
        scenario: Option<&SizingScenario>,
        inventory: Option<&TargetCapabilityInventory>,
        workloads: &crate::hardware::capacity::NodeWorkloads,
        now: std::time::SystemTime,
    ) -> Result<ListModelProfileCompatibilityResponse, ModelDeclarationRepositoryError> {
        self.profile_compatibility(model_id, target, scenario, inventory, workloads, now)
    }

    fn model_recommendations(
        &self,
        use_cases: &[dotcore_proto::dotcore::catalog::v1::UseCase],
        inventory: Option<&TargetCapabilityInventory>,
        deployments: &[ModelDeployment],
        now: std::time::SystemTime,
    ) -> Result<ListModelRecommendationsResponse, ModelDeclarationRepositoryError> {
        self.model_recommendations(use_cases, inventory, deployments, now)
    }

    fn load_upstream_model(
        &self,
        request: &CreateUpstreamModelDeploymentRequest,
        exposure: &ModelDeploymentExposureIntent,
        name: Option<&str>,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.load_upstream_model(request, exposure, name)
    }

    fn update_metadata(
        &self,
        deployment_id: &str,
        patch: &dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.update_metadata(deployment_id, patch)
    }

    fn update_upstream_model(
        &self,
        deployment_id: &str,
        replacement: &UpstreamModelDesiredConfiguration,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.update_upstream_model(deployment_id, replacement)
    }

    fn unload_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.unload_model(deployment_id)
    }

    fn pause_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.pause_model(deployment_id)
    }

    fn resume_model(
        &self,
        deployment_id: &str,
    ) -> Result<ModelDeploymentDeclaration, ModelDeclarationRepositoryError> {
        self.resume_model(deployment_id)
    }

    fn finalize_teardown(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Result<(), ModelDeclarationRepositoryError> {
        self.finalize_teardown(declaration)
    }
}

/// Manager-owned lifecycle authority over model deployment declarations.
///
/// It accepts lifecycle mutations into the authoritative site, records each accepted intent in
/// the owner's current state, and projects every deployment from the status its hosting node
/// reported through the owner acceptance boundary. It holds no runtime: node-hosted deployments
/// are realized by their node, and upstream deployments by the owner's upstream reconciler.
///
/// Cloned handles share one inner state, so lifecycle operations are serialized across all of
/// them. Accepted desired-state mutations stay authoritative in memory and wake reconciliation
/// even when `site.yaml` persistence reports an error.
pub struct InferenceController<D, S> {
    inner: Arc<ManagerInferenceControllerInner<D, S>>,
}

impl<D, S> Clone for InferenceController<D, S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub(crate) struct ModelRealizer<D, S, R> {
    inner: Arc<InferenceControllerInner<D, S, R>>,
}

impl<D, S, R> Clone for ModelRealizer<D, S, R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
impl<D, S, R> ModelRealizer<D, S, R> {
    /// Returns the serving targets this realizer publishes on its node.
    pub(crate) fn serving_targets(&self) -> LocalServingTargetsSource {
        self.inner.serving_targets_source.clone()
    }
}

pub(crate) fn is_upstream_deployment(
    deployment: &dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
) -> bool {
    matches!(
        deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref()),
        Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Upstream(_))
    )
}

/// Shared state of the manager-owned lifecycle authority: declarations, owner current state,
/// the upstream reconciler, and the node-control evidence its projection reads.
pub(super) struct ManagerInferenceControllerInner<D, S> {
    desired: D,
    state: S,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
    /// Lifecycle operation fence shared with the upstream reconciler.
    operation: Arc<AsyncMutex<()>>,
    observer_started: Mutex<bool>,
    serving_targets_publisher: ServingTargetsPublisher,
    serving_targets_source: ServingTargetsSource,
    upstream: Arc<upstream::UpstreamReconciler<D, S>>,
    remote_status: std::sync::RwLock<Option<ManagerNodeControlCoordinator>>,
    node_logs: std::sync::RwLock<Option<crate::deployment_logs::NodeDeploymentLogs>>,

    /// What every lane's deployments hold of each node's memory, once composition installs it;
    /// until then estimates count only this controller's deployments.
    site_workloads: std::sync::RwLock<Option<Arc<dyn SiteWorkloadSource>>>,
}

impl<D, S> ManagerInferenceControllerInner<D, S>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    /// Wakes the upstream reconciler after an accepted mutation; hosting nodes learn of it from
    /// their next desired view.
    fn notify_reconciliation(&self) {
        self.upstream.wake();
    }
}

/// Node-local model realizer over projected input, operational state, and an owner result port.
/// It has no site repository, catalog, or public declaration mutation capability.
pub(super) struct InferenceControllerInner<D, S, R> {
    desired: D,
    results: R,
    state: S,
    runtimes: RuntimeTargetMap,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
    operation: Arc<AsyncMutex<()>>,
    reconciliation: AsyncMutex<()>,
    preparations: preparation::PreparationCoordinator,
    observer_started: Mutex<bool>,
    wakeup: Arc<Notify>,
    artifacts: ArtifactManager,

    /// Preparation logs each deployment's artifact preparation writes its steps to.
    preparation_logs: crate::deployment_logs::PreparationLogs,

    /// Tells the node's deployment-log follows when a deployment's engine workload starts.
    workload_lifecycle: crate::deployment_logs::WorkloadLifecycle,
    serving_targets_publisher: LocalServingTargetsPublisher,
    serving_targets_source: LocalServingTargetsSource,
    accelerator_attribution: Option<accelerator_memory::AcceleratorAttribution>,

    /// Cancelled when the realizer's owner shuts it down, cancelling node-local work in flight
    /// such as a development engine evaluation.
    shutdown: tokio_util::sync::CancellationToken,
}

impl<D, S> InferenceController<D, S> {
    /// Returns the complete private serving-target source for the inference router.
    pub(crate) fn serving_targets(&self) -> ServingTargetsSource {
        self.inner.serving_targets_source.clone()
    }

    /// Installs the manager's sole node-control coordinator as remote status freshness authority.
    pub(crate) fn set_remote_status_source(&self, coordinator: ManagerNodeControlCoordinator) {
        *self
            .inner
            .remote_status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(coordinator);
    }

    /// Installs the source of every lane's workloads, so estimates name application deployments
    /// in their previews instead of counting their memory as other use.
    pub(crate) fn set_site_workloads(&self, workloads: Arc<dyn SiteWorkloadSource>) {
        *self
            .inner
            .site_workloads
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(workloads);
    }

    /// Starts publishing node-hosted deployments as node serving targets from the manager's
    /// node-control evidence and its authoritative site.
    ///
    /// Deployments hosted on `local_node_id`, the SiteNode sharing this process, are dispatched in
    /// process instead of over a private channel. Publication stops when the returned task drops.
    pub(crate) fn start_node_targets(
        &self,
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
        local_node_id: String,
    ) -> NodeTargetProjectorTask {
        node_targets::NodeTargetProjector::new(
            site,
            coordinator,
            self.inner.serving_targets_publisher.clone(),
            local_node_id,
        )
        .start()
    }
}

impl<D, S> ServingTargetsSourceProvider for InferenceController<D, S>
where
    D: DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    fn serving_targets(&self) -> ServingTargetsSource {
        self.serving_targets()
    }
}

#[cfg(test)]
impl InferenceController<crate::model::repository::ModelDeclarationRepository, FileCurrentState> {
    pub(crate) fn composition_pointers(&self) -> (*const dotcore_catalog::Catalog, usize, usize) {
        let (catalog, site) = self.inner.desired.composition_pointers();
        (catalog, site, self.inner.state.state_pointer())
    }
}

/// Typed controller failures with stable model service categories.
#[derive(Debug, thiserror::Error)]
pub enum ModelLifecycleError {
    #[error("Upstream endpoint preflight failed: {summary}")]
    UpstreamPreflight {
        summary: &'static str,
        primary_cause: &'static str,
        request_url: Box<str>,
        http_status: Option<u32>,
        response_body: Option<Box<[u8]>>,

        #[source]
        source: Option<anyhow::Error>,
    },

    #[error("Model lifecycle configuration failed: {0}")]
    Configuration(#[source] anyhow::Error),

    #[error(transparent)]
    CatalogConfiguration(#[from] dotcore_catalog::CatalogError),

    #[error("Model lifecycle outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),

    #[error("Upstream model replacement persistence failed: {0}")]
    DesiredPersistence(#[source] anyhow::Error),

    #[error(transparent)]
    SiteRepository(#[from] SiteRepositoryError),

    #[error(transparent)]
    ModelRepository(#[from] ModelDeclarationRepositoryError),

    #[error(transparent)]
    CurrentState(#[from] crate::model::state::CurrentStateError),

    #[error(transparent)]
    Runtime(#[from] crate::dispatch::model::RuntimeTargetError),

    #[error(transparent)]
    Artifact(#[from] dotcore_artifact::ArtifactCacheError),

    #[error("Engine integration failed: {0}")]
    EngineIntegration(#[source] anyhow::Error),

    #[error("Model artifact preparation task failed: {0}")]
    Task(#[from] tokio::task::JoinError),

    #[error("model lifecycle request contains an invalid ID")]
    InvalidArgument,

    #[error("Catalog model {0:?} was not found")]
    ModelNotFound(String),

    #[error("Catalog model {model_id:?} has no profile {profile_id:?}")]
    ModelProfileNotFound {
        model_id: String,
        profile_id: String,
    },

    #[error("site execution target {0:?} was not found")]
    ExecutionTargetNotFound(String),

    #[error("site execution target {0:?} is not a model execution target")]
    NotModelExecutionTarget(String),

    /// The selected execution target can't run the model profile's engine; the message is safe
    /// to show operators.
    #[error(transparent)]
    TargetIncompatible(Box<crate::model::resolution::ModelLaunchResolutionError>),

    /// The local deployment configuration breaks a launch rule the operator can fix by changing
    /// the request; the message is safe to show operators.
    #[error(transparent)]
    ConfigurationRejected(Box<crate::model::resolution::ModelLaunchResolutionError>),

    #[error("Model deployment name must be 1–128 non-control characters after trimming")]
    InvalidName,

    #[error("model lifecycle resource was not found")]
    NotFound,

    #[error("model lifecycle operation is not valid for the current state")]
    FailedPrecondition,

    #[error("Inference route name already exists: {0}")]
    RouteNameTaken(String),

    #[error("Inference routes shared with other deployments reference the model deployment: {0:?}")]
    SharedRouteReferences(Vec<String>),

    #[error(
        "Application deployments use inference routes that would be removed with the model deployment: {0:?}"
    )]
    ApplicationRouteReferences(Vec<ModelTeardownApplicationRouteBlocker>),
}

/// Exhaustive transport-neutral lifecycle categories for service and controller boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelLifecycleErrorCategory {
    Configuration,

    OutcomeUncertain,

    InvalidArgument,

    NotFound,

    AlreadyExists,

    FailedPrecondition,
}

impl ModelLifecycleError {
    pub(crate) fn upstream_preflight(
        summary: &'static str,
        primary_cause: &'static str,
        request_url: impl Into<Box<str>>,
        http_status: Option<u32>,
        response_body: Option<Box<[u8]>>,
        source: Option<anyhow::Error>,
    ) -> Self {
        let request_url = normalize_whitespace(&request_url.into());

        Self::UpstreamPreflight {
            summary,
            primary_cause,
            request_url: request_url.into_boxed_str(),
            http_status,
            response_body,
            source,
        }
    }

    pub(crate) fn upstream_preflight_message(&self) -> Option<String> {
        let Self::UpstreamPreflight {
            summary,
            primary_cause,
            request_url,
            http_status,
            response_body,
            ..
        } = self
        else {
            return None;
        };
        let summary = truncate_utf8_with_ellipsis(summary, 384);
        let request_url = if request_url.is_empty() {
            "unavailable".to_owned()
        } else {
            truncate_utf8_with_ellipsis(request_url, 768)
        };
        let message = match (http_status, response_body) {
            (Some(status), Some(body)) => format!(
                "Upstream endpoint preflight failed: {summary}; request URL: {request_url}; HTTP status: {status}; response: {}",
                response_excerpt(body),
            ),
            (Some(status), None) => format!(
                "Upstream endpoint preflight failed: {summary}; request URL: {request_url}; HTTP status: {status}"
            ),
            (None, _) => format!(
                "Upstream endpoint preflight failed: {summary}; request URL: {request_url}; cause: {}",
                truncate_utf8_with_ellipsis(primary_cause, 192),
            ),
        };
        Some(truncate_utf8_with_ellipsis(&message, 2_048))
    }

    pub(crate) fn category(&self) -> ModelLifecycleErrorCategory {
        match self {
            Self::UpstreamPreflight { .. }
            | Self::Configuration(_)
            | Self::CatalogConfiguration(_)
            | Self::SiteRepository(_)
            | Self::ModelRepository(_)
            | Self::CurrentState(_)
            | Self::Runtime(_)
            | Self::Artifact(_)
            | Self::EngineIntegration(_)
            | Self::Task(_) => ModelLifecycleErrorCategory::Configuration,
            Self::OutcomeUncertain(_) | Self::DesiredPersistence(_) => {
                ModelLifecycleErrorCategory::OutcomeUncertain
            }
            Self::InvalidArgument
            | Self::InvalidName
            | Self::NotModelExecutionTarget(_)
            | Self::ConfigurationRejected(_) => ModelLifecycleErrorCategory::InvalidArgument,
            Self::ModelNotFound(_)
            | Self::ModelProfileNotFound { .. }
            | Self::ExecutionTargetNotFound(_)
            | Self::NotFound => ModelLifecycleErrorCategory::NotFound,
            Self::RouteNameTaken(_) => ModelLifecycleErrorCategory::AlreadyExists,
            Self::FailedPrecondition
            | Self::TargetIncompatible(_)
            | Self::SharedRouteReferences(_)
            | Self::ApplicationRouteReferences(_) => {
                ModelLifecycleErrorCategory::FailedPrecondition
            }
        }
    }

    /// Whether reconciliation is only waiting for the node to accept a desired view from the site
    /// manager, which is an expected state after a manager restart rather than a failure.
    pub(super) fn awaits_desired_view(&self) -> bool {
        matches!(
            self,
            Self::SiteRepository(SiteRepositoryError::NoAcceptedView)
        )
    }

    pub(super) fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration(anyhow!(message.into()))
    }
    pub(super) fn configuration_source(
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Configuration(anyhow::Error::new(source))
    }

    pub(super) fn from_model_repository(error: ModelDeclarationRepositoryError) -> Self {
        match error {
            ModelDeclarationRepositoryError::Site(SiteRepositoryError::Storage(
                SiteStorageError::OutcomeUncertain(_),
            )) => Self::outcome_uncertain_with_source(
                "desired site persistence outcome is uncertain",
                error,
            ),
            ModelDeclarationRepositoryError::ExposureRoute(
                crate::route_manager::RouteRepositoryError::NameAlreadyExists(name),
            ) => Self::RouteNameTaken(name),
            ModelDeclarationRepositoryError::ExposureConflict(_)
            | ModelDeclarationRepositoryError::ExposureRoute(
                crate::route_manager::RouteRepositoryError::IdCollision(_)
                | crate::route_manager::RouteRepositoryError::NotFound(_)
                | crate::route_manager::RouteRepositoryError::InvalidArgument(_)
                | crate::route_manager::RouteRepositoryError::InvalidShape(_)
                | crate::route_manager::RouteRepositoryError::FailedPrecondition(_),
            )
            | ModelDeclarationRepositoryError::InvalidTransition { .. } => Self::FailedPrecondition,
            ModelDeclarationRepositoryError::SharedRouteReferences(names) => {
                Self::SharedRouteReferences(names)
            }
            ModelDeclarationRepositoryError::ApplicationRouteReferences(references) => {
                Self::ApplicationRouteReferences(references)
            }
            ModelDeclarationRepositoryError::ModelNotFound(model_id) => {
                Self::ModelNotFound(model_id)
            }
            ModelDeclarationRepositoryError::ModelProfileNotFound {
                model_id,
                profile_id,
            } => Self::ModelProfileNotFound {
                model_id,
                profile_id,
            },
            ModelDeclarationRepositoryError::ExecutionTargetNotFound(execution_target_id) => {
                Self::ExecutionTargetNotFound(execution_target_id)
            }
            ModelDeclarationRepositoryError::NotModelExecutionTarget(execution_target_id) => {
                Self::NotModelExecutionTarget(execution_target_id)
            }
            ModelDeclarationRepositoryError::TargetIncompatible(error) => {
                Self::TargetIncompatible(error)
            }
            ModelDeclarationRepositoryError::ConfigurationRejected(error) => {
                Self::ConfigurationRejected(error)
            }
            ModelDeclarationRepositoryError::DeploymentNotFound(_) => Self::NotFound,
            ModelDeclarationRepositoryError::InvalidName(_) => Self::InvalidName,
            other => Self::ModelRepository(other),
        }
    }

    pub(super) fn from_current_state(
        error: impl Into<crate::model::state::CurrentStateError>,
    ) -> Self {
        let error = error.into();
        match error {
            crate::model::state::CurrentStateError::OutcomeUncertain(_)
            | crate::model::state::CurrentStateError::Poisoned => {
                Self::outcome_uncertain_with_source(
                    "current-state persistence outcome is uncertain",
                    error,
                )
            }
            other => Self::CurrentState(other),
        }
    }

    pub(super) fn desired_persistence(source: impl Into<anyhow::Error>) -> Self {
        Self::DesiredPersistence(source.into().context("desired site persistence failed"))
    }

    pub(super) fn outcome_uncertain_with_source(
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self::OutcomeUncertain(source.into().context(message.into()))
    }
}

pub(super) fn response_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let visible = text
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_utf8_with_ellipsis(&normalize_whitespace(&visible), 512)
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, io};

    use super::{ModelLifecycleError, ModelLifecycleErrorCategory, response_excerpt};
    use crate::model::{
        ModelTeardownApplicationRouteBlocker, repository::ModelDeclarationRepositoryError,
    };

    #[test]
    fn application_route_references_preserve_facts_and_failed_precondition_category() {
        let blockers = vec![ModelTeardownApplicationRouteBlocker {
            application_deployment_id: "application-one".to_owned(),
            application_deployment_name: "Open WebUI".to_owned(),
            inference_route_id: "route-one".to_owned(),
            inference_route_name: "Gemma route".to_owned(),
        }];

        let error = ModelLifecycleError::from_model_repository(
            ModelDeclarationRepositoryError::ApplicationRouteReferences(blockers.clone()),
        );

        assert_eq!(
            error.category(),
            ModelLifecycleErrorCategory::FailedPrecondition
        );
        assert!(matches!(
            error,
            ModelLifecycleError::ApplicationRouteReferences(actual) if actual == blockers
        ));
    }

    #[test]
    fn target_incompatibility_keeps_its_typed_source_as_a_failed_precondition() {
        let error = ModelLifecycleError::from_model_repository(
            ModelDeclarationRepositoryError::TargetIncompatible(Box::new(
                crate::model::resolution::ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
                    execution_target_id: "gpu-pair".to_owned(),
                    device_count: 3,
                    required: 1,
                },
            )),
        );

        assert_eq!(
            error.category(),
            ModelLifecycleErrorCategory::FailedPrecondition
        );
        assert!(matches!(error, ModelLifecycleError::TargetIncompatible(_)));
        assert_eq!(
            error.to_string(),
            "execution target \"gpu-pair\" has 3 accelerator devices, but the profile's launch uses 1"
        );
    }

    #[test]
    fn route_name_collision_maps_to_route_name_taken_and_id_collision_stays_generic() {
        use crate::route_manager::RouteRepositoryError;

        let name_taken = ModelLifecycleError::from_model_repository(
            ModelDeclarationRepositoryError::ExposureRoute(
                RouteRepositoryError::NameAlreadyExists("public-model".to_owned()),
            ),
        );
        assert_eq!(
            name_taken.category(),
            ModelLifecycleErrorCategory::AlreadyExists
        );
        assert!(matches!(
            name_taken,
            ModelLifecycleError::RouteNameTaken(name) if name == "public-model"
        ));

        let id_collision = ModelLifecycleError::from_model_repository(
            ModelDeclarationRepositoryError::ExposureRoute(RouteRepositoryError::IdCollision(
                "route-1".to_owned(),
            )),
        );
        assert!(matches!(
            id_collision,
            ModelLifecycleError::FailedPrecondition
        ));
    }

    #[test]
    fn upstream_preflight_error_preserves_compact_message_inputs() {
        let error = preflight_error(None);

        assert_eq!(
            error.to_string(),
            "Upstream endpoint preflight failed: The endpoint could not be reached"
        );
        let ModelLifecycleError::UpstreamPreflight {
            summary,
            primary_cause,
            request_url,
            http_status,
            response_body,
            source,
        } = error
        else {
            panic!("upstream preflight error")
        };
        assert_eq!(summary, "The endpoint could not be reached");
        assert_eq!(primary_cause, "network connection failed");
        assert_eq!(request_url.as_ref(), "https://provider.example/v1/models");
        assert_eq!(http_status, None);
        assert!(response_body.is_none());
        assert!(source.is_none());
    }

    #[test]
    fn response_excerpt_replaces_binary_and_invalid_prefixes_with_readable_text() {
        assert_eq!(
            response_excerpt(b"\0\xff provider\n\tresponse"),
            "�� provider response"
        );
    }

    #[test]
    fn upstream_preflight_error_retains_contextual_source_chain() {
        let source = anyhow::Error::new(io::Error::other("retained transport leaf"))
            .context("upstream preflight transport context");
        let error = preflight_error(Some(source));

        let context = error.source().expect("contextual source");
        assert_eq!(context.to_string(), "upstream preflight transport context");
        let leaf = context.source().expect("retained source");
        assert_eq!(leaf.to_string(), "retained transport leaf");
        assert!(leaf.downcast_ref::<io::Error>().is_some());
    }

    #[test]
    fn upstream_preflight_error_without_source_stays_source_less() {
        let error = preflight_error(None);

        assert!(error.source().is_none());
    }

    fn preflight_error(source: Option<anyhow::Error>) -> ModelLifecycleError {
        ModelLifecycleError::upstream_preflight(
            "The endpoint could not be reached",
            "network connection failed",
            "https://provider.example/v1/models",
            None,
            None,
            source,
        )
    }
}
