//! Apps controller behind the application service: a manager owner of application deployment
//! intent and a node-local realizer that drives runtime workload groups.
//!
//! The owner accepts declarations, allocates never-reused lifecycle generations, projects status
//! that hosting nodes report, and removes a declaration only when the hosting node's cleanup
//! evidence matches the teardown. Every node, including the manager's own, composes the same
//! realizer over its installed node view and reports typed status instead of writing owner
//! state.

mod accelerator_memory;
mod errors;
mod identity;
mod launch;
mod logs;
mod metrics;
mod node;
mod node_view;
mod observation;
mod operations;
mod owner;
mod parameters;
mod projection;
mod readiness;
mod realization;
mod realizer;
mod reconciliation;
mod remote_status;
mod resources;
mod serving;
mod startup;
mod state;
mod status_text;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    num::NonZeroU16,
    sync::Arc,
    time::{Duration, SystemTime},
};

use dotcore_catalog::Catalog;
use dotcore_domain::{SiteNodeHost, valid_id};
use dotcore_proto::{
    dotcore::{
        application::v1::{
            ApplicationDeployment, ApplicationDeploymentDeclaration,
            ApplicationDeploymentDesiredState, ApplicationDeploymentFailure,
            ApplicationDeploymentState, ApplicationEndpoint, ApplicationLifecycleState,
            ApplicationReadinessState, ApplicationServiceLifecycleState,
            ApplicationServiceObservation, CreateApplicationDeploymentRequest,
            CurrentApplicationDeployment, ListApplicationDeploymentsResponse,
        },
        site::v1::{ExecutionTarget as SiteExecutionTarget, Site},
    },
    google::protobuf::Timestamp,
};
use dotcore_site::node::{
    control::ManagerNodeControlCoordinator, target_inventory::TargetCapabilitySource,
};
#[cfg(test)]
use dotcore_site::{repository::SiteRepository, store::FileCurrentState};
use tokio::sync::Mutex as AsyncMutex;

use crate::application::repository::{
    ApplicationDeclarationRepository, ApplicationLifecycleGenerations, ApplicationRepositoryError,
    CurrentApplicationRepository,
};
pub use errors::{
    ApplicationLifecycleError, ParameterRejection, RetainedTargetMismatch, StorageChoiceRejection,
    TargetIneligibility,
};
use errors::{configuration, configuration_with_source, outcome_uncertain_with_source};
pub use logs::ApplicationDeploymentLogController;
pub(crate) use metrics::{
    APPLICATION_RESOURCES_SOURCE, application_metric_descriptors, application_resource_records,
};
use node::ReconciledAssignments;
pub(crate) use node::{
    NodeApplicationComposition, NodeApplicationRealizer, classify_application_assignment,
};
pub(crate) use node_view::ApplicationNodeViewSection;
use operations::{
    create_deployment, pause_deployment, remove_deployment, resume_deployment, update_metadata,
};
use owner::ApplicationOwner;
use projection::project;
use realization::{
    ApplicationRealizationInput, ApplicationRealizationResults, ApplicationRealizationSnapshot,
    NodeRealizationRecords, RealizedView,
};
use realizer::ApplicationRealizer;
pub use serving::{InferenceRouteResolutionError, InferenceRouteResolver, ResolvedInferenceRoute};
use state::*;

/// Longest one readiness probe of an application deployment may take.
const READINESS_TIMEOUT: Duration = Duration::from_secs(3);

/// Name catalog applications resolve for the OpenAI-compatible base URL of the site inference
/// listener.
const INFERENCE_BASE_URL_SOURCE: &str = "configured-inference-openai-base-url";

/// Name catalog applications resolve for the URL their own primary endpoint is published at.
const PUBLISHED_ENDPOINT_URL_SOURCE: &str = "published-endpoint-url";

/// Active apps-controller interface used by the application service edge.
pub trait ApplicationLifecycleController: Send + Sync {
    fn list_deployments(
        &self,
    ) -> impl Future<Output = Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError>> + Send;

    fn get_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;

    fn create_deployment(
        &self,
        request: CreateApplicationDeploymentRequest,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;

    fn remove_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;

    fn update_metadata(
        &self,
        deployment_id: String,
        patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;

    fn pause_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;

    fn resume_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send;
}

/// Non-secret values that catalog applications resolve by name for the SiteNode hosting a
/// deployment, resolved once when the site manager creates it.
///
/// The inference base URL names the site manager's inference listener by the host the hosting
/// node reaches the manager at: its recorded manager host, else the declared one, for example
/// `http://manager.example.ts.net:6421/v1`. The published endpoint URL names the deployment's
/// own primary endpoint by the host the site manager reaches the hosting node at and the
/// deployment's published host port, for example `http://worker.example.ts.net:31234`.
#[derive(Clone, Debug, Default)]
pub struct ApplicationConfigurationSources {
    manager_host: Option<SiteNodeHost>,
    inference_port: u16,
}

impl ApplicationConfigurationSources {
    /// Builds the sources for the site manager's inference listener on `inference_port` at the
    /// manager's declared host.
    pub fn new(manager_host: SiteNodeHost, inference_port: u16) -> Self {
        Self {
            manager_host: Some(manager_host),
            inference_port,
        }
    }

    /// Reports whether the value of source `name` is a URL naming a site host that the
    /// deployment's containers dial, which the hosting node resolves for them rather than leaving
    /// to their DNS. The published endpoint URL is only advertised, never dialed from inside.
    pub(crate) fn carries_site_host(name: &str) -> bool {
        name == INFERENCE_BASE_URL_SOURCE
    }

    /// Returns the value of `name` for a deployment placed on `site_node_id` in `site` and
    /// published on `published_port`, or `None` when the name is unknown or the SiteNode
    /// is not declared.
    pub(crate) fn resolve(
        &self,
        name: &str,
        site: &Site,
        site_node_id: &str,
        published_port: NonZeroU16,
    ) -> Option<String> {
        let node = site
            .nodes
            .iter()
            .find(|node| node.node_id == site_node_id)?;
        match name {
            INFERENCE_BASE_URL_SOURCE => {
                let declared = self.manager_host.as_ref()?;
                let host = match node.manager_host.as_str() {
                    "" => declared.clone(),
                    manager_host => SiteNodeHost::parse(manager_host).ok()?,
                };
                Some(format!("http://{host}:{}/v1", self.inference_port))
            }
            PUBLISHED_ENDPOINT_URL_SOURCE => {
                let host = SiteNodeHost::parse(&node.host).ok()?;
                Some(format!("http://{host}:{published_port}"))
            }
            _ => None,
        }
    }
}

/// Composed controller inputs that are not catalog or site-owned state.
pub struct ApplicationControllerConfiguration {
    pub configuration_sources: ApplicationConfigurationSources,
    pub inference_route_resolver: Arc<dyn InferenceRouteResolver>,
    pub target_capabilities: Arc<dyn TargetCapabilitySource>,
    pub secret_generator: Arc<dyn crate::SecretGenerator>,
}

/// Apps-controller lifecycle authority on the site manager: the owner of application deployment
/// intent. It holds no runtime; the hosting node realizes each deployment and reports its status.
pub struct ApplicationController<D, S> {
    owner: Arc<ApplicationOwner<D, S>>,
}

#[expect(
    private_bounds,
    reason = "the public selected controller alias retains private owner-specific repository ports"
)]
impl<D, S> ApplicationController<D, S>
where
    D: ApplicationDeclarationRepository + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + 'static,
{
    /// Validates retained declarations against the site and prunes current records left without
    /// a declaration.
    pub fn new(
        catalog: Arc<Catalog>,
        desired: D,
        current: S,
        controller_configuration: ApplicationControllerConfiguration,
    ) -> Result<Self, ApplicationLifecycleError> {
        let ApplicationControllerConfiguration {
            configuration_sources,
            inference_route_resolver,
            target_capabilities,
            secret_generator,
        } = controller_configuration;
        let owner = Arc::new(ApplicationOwner {
            catalog,
            configuration_sources,
            inference_route_resolver,
            target_capabilities,
            secret_generator,
            desired: Arc::new(desired),
            current: Arc::new(current),
            operation: AsyncMutex::new(()),
            remote_status: std::sync::RwLock::new(None),
            node_logs: std::sync::RwLock::new(None),
        });
        owner.validate_retained_deployments()?;
        owner.prune_undeclared_current_records()?;
        Ok(Self { owner })
    }

    /// Installs the manager's sole node-control coordinator as node status freshness authority.
    pub(crate) fn set_remote_status_source(&self, coordinator: ManagerNodeControlCoordinator) {
        *self
            .owner
            .remote_status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(coordinator);
    }

    /// Projects every deployment as the list read does, without waiting for a lifecycle operation
    /// in progress. Advisory reads use it so they never serialize with mutations: a deployment
    /// being created or removed may be missing or still listed until the next read.
    pub(crate) fn public_projection(
        &self,
    ) -> Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError> {
        self.owner.projection()
    }
}

impl<D, S> Clone for ApplicationController<D, S> {
    fn clone(&self) -> Self {
        Self {
            owner: Arc::clone(&self.owner),
        }
    }
}

#[cfg(test)]
impl ApplicationController<SiteRepository, FileCurrentState> {
    pub(crate) fn composition_pointers(&self) -> (*const Catalog, usize, usize, Option<usize>) {
        (
            Arc::as_ptr(&self.owner.catalog),
            self.owner.desired.state_pointer(),
            self.owner.current.state_pointer(),
            self.owner
                .inference_route_resolver
                .composition_route_manager_pointer(),
        )
    }

    pub(crate) fn project_route_for_test(
        &self,
        inference_route_id: &str,
    ) -> Result<ResolvedInferenceRoute, InferenceRouteResolutionError> {
        self.owner
            .inference_route_resolver
            .project_current(inference_route_id)
    }
}

impl<D, S> ApplicationLifecycleController for ApplicationController<D, S>
where
    D: ApplicationDeclarationRepository + 'static,
    S: CurrentApplicationRepository + ApplicationLifecycleGenerations + 'static,
{
    fn list_deployments(
        &self,
    ) -> impl Future<Output = Result<ListApplicationDeploymentsResponse, ApplicationLifecycleError>> + Send
    {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            owner.projection()
        }
    }

    fn get_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            validate_id(&deployment_id)?;
            let _operation = owner.operation.lock().await;
            let response = owner.projection()?;
            response
                .deployments
                .into_iter()
                .find(|deployment| deployment.application_deployment_id == deployment_id)
                .ok_or(ApplicationLifecycleError::NotFound)
        }
    }

    fn create_deployment(
        &self,
        request: CreateApplicationDeploymentRequest,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            create_deployment(&owner, request).await
        }
    }

    fn remove_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            remove_deployment(&owner, deployment_id)
        }
    }

    fn update_metadata(
        &self,
        deployment_id: String,
        patch: dotcore_domain::deployment::metadata::DeploymentMetadataPatch,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            update_metadata(&owner, deployment_id, patch)
        }
    }

    fn pause_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            pause_deployment(&owner, deployment_id)
        }
    }

    fn resume_deployment(
        &self,
        deployment_id: String,
    ) -> impl Future<Output = Result<ApplicationDeployment, ApplicationLifecycleError>> + Send {
        let owner = Arc::clone(&self.owner);
        async move {
            let _operation = owner.operation.lock().await;
            resume_deployment(&owner, deployment_id)
        }
    }
}

/// Resolves the exact application target a placement names: a declared application target
/// owned by the placement's SiteNode, which realizes it from its node view.
fn eligible_target<'a>(
    site: &'a Site,
    placement: &dotcore_proto::dotcore::deployment::v1::ExactPlacement,
) -> Result<&'a SiteExecutionTarget, ApplicationLifecycleError> {
    let ineligible = |reason| ApplicationLifecycleError::IneligibleTarget {
        site_node_id: placement.site_node_id.clone(),
        execution_target_id: placement.execution_target_id.clone(),
        reason,
    };
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .ok_or_else(|| ineligible(TargetIneligibility::Undeclared))?;
    if target.site_node_id != placement.site_node_id {
        return Err(ineligible(TargetIneligibility::OwnedByAnotherNode));
    }
    if !matches!(
        target.runtime_binding,
        Some(
            dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(
                _
            )
        )
    ) {
        return Err(ineligible(TargetIneligibility::NotApplicationTarget));
    }
    Ok(target)
}

/// Rechecks a deployment's retained target facts against the current site before any launch is
/// compiled: the target must still exist on the placement's node with the retained platform and,
/// unless simulated, the retained whole accelerator device set.
fn validate_retained_target(
    deployment: &ApplicationDeploymentDeclaration,
    site: &Site,
) -> Result<(), ApplicationLifecycleError> {
    let incompatible = |mismatch| ApplicationLifecycleError::RetainedTargetIncompatible(mismatch);
    let placement = deployment
        .placement
        .as_ref()
        .ok_or_else(|| incompatible(RetainedTargetMismatch::Placement))?;
    let snapshot = deployment
        .snapshot
        .as_ref()
        .ok_or_else(|| incompatible(RetainedTargetMismatch::Placement))?;
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .filter(|target| target.site_node_id == placement.site_node_id)
        .ok_or_else(|| incompatible(RetainedTargetMismatch::Placement))?;
    if snapshot.placement.as_ref() != Some(placement) {
        return Err(incompatible(RetainedTargetMismatch::Placement));
    }
    if target.platform.as_ref() != snapshot.platform.as_ref() {
        return Err(incompatible(RetainedTargetMismatch::Platform));
    }
    if !retained_devices_match(target, &snapshot.accelerator_device_ids) {
        return Err(incompatible(RetainedTargetMismatch::Devices));
    }
    Ok(())
}

/// Returns whether a target's configured accelerator devices are the set the deployment retains.
/// A simulated target simulates its accelerators, so it always matches.
fn retained_devices_match(target: &SiteExecutionTarget, retained: &[String]) -> bool {
    is_simulated_application_target(target)
        || retained.iter().collect::<BTreeSet<_>>()
            == target
                .accelerator_device_ids
                .iter()
                .collect::<BTreeSet<_>>()
}

/// Returns whether a target's runtime binding is a simulated application runtime.
pub(in crate::application) fn is_simulated_application_target(
    target: &SiteExecutionTarget,
) -> bool {
    matches!(
        &target.runtime_binding,
        Some(
            dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(
                dotcore_proto::dotcore::site::v1::ApplicationRuntimeBinding {
                    adapter: Some(
                        dotcore_proto::dotcore::site::v1::application_runtime_binding::Adapter::SimulatedRuntime(_)
                    ),
                }
            )
        )
    )
}

fn validate_id(value: &str) -> Result<(), ApplicationLifecycleError> {
    valid_id(value)
        .then_some(())
        .ok_or(ApplicationLifecycleError::InvalidArgument)
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
