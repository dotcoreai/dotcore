//! Reads each watched resource kind from the component that owns it, answering exactly what that
//! owner's list read answers.
//!
//! Site reads take the node coordinator's lock, which is held across disk writes, so they run on
//! the blocking thread pool, as does the capacity component's node memory read, which reads the
//! node registry; model and application reads take their owner's operation lock, so a slow
//! mutation delays only that kind.

use std::sync::Arc;

use dotcore_proto::dotcore::{
    inference::v1::ListRoutesResponse, watch::v1::keyed_resource::Payload,
};
use dotcore_site::service::SiteService;
use futures_util::future::BoxFuture;

use super::hub::{ResourceRead, ResourceReadError, ResourceReads};
use crate::{
    application::ApplicationLifecycleController, model::controller::ModelLifecycleController,
    node_memory::NodeMemoryRead, route_manager::RouteManager,
};

/// The reads of every resource kind from their owners: the site inventory and console settings,
/// the inference and apps controllers, the route manager, and the capacity component's node
/// memory.
pub(crate) fn owner_reads<M, A>(
    site: Arc<SiteService>,
    models: M,
    applications: A,
    routes: RouteManager,
    node_memory: Arc<NodeMemoryRead>,
) -> ResourceReads
where
    M: ModelLifecycleController + 'static,
    A: ApplicationLifecycleController + 'static,
{
    ResourceReads {
        machines: Arc::new(BlockingRead::new({
            let site = Arc::clone(&site);
            move || site.machines().map(Payload::Machines)
        })),
        hardware_snapshots: Arc::new(BlockingRead::new({
            let site = Arc::clone(&site);
            move || site.hardware_snapshots().map(Payload::HardwareSnapshots)
        })),
        execution_targets: Arc::new(BlockingRead::new({
            let site = Arc::clone(&site);
            move || {
                site.configured_execution_targets()
                    .map(Payload::ExecutionTargets)
            }
        })),
        model_deployments: Arc::new(ModelDeploymentsRead(models)),
        application_deployments: Arc::new(ApplicationDeploymentsRead(applications)),
        routes: Arc::new(RoutesRead(routes)),
        node_memory: Arc::new(BlockingRead::new(move || {
            node_memory.list().map(Payload::NodeMemory)
        })),
        console_settings: Arc::new(BlockingRead::new(move || {
            site.console_settings().map(Payload::ConsoleSettings)
        })),
    }
}

/// A synchronous owner read run on the blocking thread pool.
struct BlockingRead {
    read: Arc<dyn Fn() -> Result<Payload, ResourceReadError> + Send + Sync>,
}

impl BlockingRead {
    fn new<E>(read: impl Fn() -> Result<Payload, E> + Send + Sync + 'static) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            read: Arc::new(move || read().map_err(ResourceReadError::new)),
        }
    }
}

impl ResourceRead for BlockingRead {
    fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>> {
        let read = Arc::clone(&self.read);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || read())
                .await
                .map_err(ResourceReadError::new)?
        })
    }
}

/// Model deployments from the inference controller. Generic because the controller interface
/// returns `impl Future` and cannot be a trait object.
struct ModelDeploymentsRead<M>(M);

impl<M: ModelLifecycleController> ResourceRead for ModelDeploymentsRead<M> {
    fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>> {
        Box::pin(async move {
            self.0
                .list_deployments()
                .await
                .map(Payload::ModelDeployments)
                .map_err(ResourceReadError::new)
        })
    }
}

/// Application deployments from the apps controller. Generic because the controller interface
/// returns `impl Future` and cannot be a trait object.
struct ApplicationDeploymentsRead<A>(A);

impl<A: ApplicationLifecycleController> ResourceRead for ApplicationDeploymentsRead<A> {
    fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>> {
        Box::pin(async move {
            self.0
                .list_deployments()
                .await
                .map(Payload::ApplicationDeployments)
                .map_err(ResourceReadError::new)
        })
    }
}

/// Routes from the route manager, whose in-memory projection answers without waiting.
struct RoutesRead(RouteManager);

impl ResourceRead for RoutesRead {
    fn read(&self) -> BoxFuture<'_, Result<Payload, ResourceReadError>> {
        Box::pin(async move {
            Ok(Payload::Routes(ListRoutesResponse {
                routes: self.0.list(),
            }))
        })
    }
}
