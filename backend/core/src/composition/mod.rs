//! Process configuration and complete backend capability composition.

mod backend;
mod configuration;
mod controllers;
mod filesystem;
mod isolated;
mod loading;
#[cfg(test)]
mod local_node;
mod node;
mod node_binding;
mod private;
mod routes;
mod runtime;
mod runtime_adapters;
mod services;
#[cfg(test)]
mod tests;

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::anyhow;
use dotcore_catalog::{Catalog, service::CatalogService};
use dotcore_domain::SitePaths;
use dotcore_domain::hardware_observation::HardwareSnapshotSource;
use dotcore_observability::{
    InMemoryMetricStore, InMemoryStoreConfig, InProcessBucketTransport, MetricBucketIngest,
    MetricDescriptor, forwarder_metric_descriptors,
};
use dotcore_site::{
    SITE_FILE,
    initial::input_store::SiteInputLockError,
    node::{
        binding::BoundSiteNode,
        control::ManagerNodeControlCoordinator,
        metrics::hardware_metric_descriptors,
        registry::NodeRegistry,
        session::InProcessNodeControlTransport,
        stream::{ManagerNodeControlStreams, NodeControlStreamConfiguration},
        target_inventory::TargetCapabilitySource,
    },
    repository::{SiteRepository, SiteRepositoryError},
    service::SiteService,
    store::{FileCurrentState, prepare_site_node_store},
    view::{
        desired::ManagerNodeViewPublisher,
        fencing::{ManagerSession, NodeIncarnation},
    },
    worker_channels::WorkerChannels,
};
use tracing::{Instrument, Span, debug, error, field, info, info_span, warn};

use crate::{
    application::{
        ApplicationConfigurationSources, ApplicationController, ApplicationControllerConfiguration,
        service::ApplicationService,
    },
    model::{controller::InferenceController, service::ModelDeploymentService},
    route_manager::{
        RouteAvailabilityTargetsSource, RouteDeclarationRepository, RouteManager, RouteService,
    },
    router::InferenceRouter,
};
use backend::ObserverStarters;
pub use backend::{Backend, BackendTestSupport};
#[cfg(feature = "test-support")]
pub use backend::{MetricRecorderSlot, NodeLiveness};
pub use configuration::{
    BackendConfiguration, BackendConfigurationError, BackendRole, DEFAULT_INFERENCE_PORT,
    DEFAULT_MANAGEMENT_ADDR, DEFAULT_PRIVATE_PORT, DEV_SOURCE_FLAKE_ENV, FRONTEND_ORIGIN_ENV,
    INFERENCE_ADDR_ENV, MANAGEMENT_ADDR_ENV, ManagerPrivateListener, NodeRole,
    PACKAGED_SOURCE_FLAKE_ENV, PRIVATE_ADDR_ENV, PathOverrides, ServerConfiguration, node_role,
};
#[cfg(test)]
use controllers::default_collector;
use controllers::system_collector;
pub use isolated::{IsolatedApplicationService, IsolatedModelService};
use loading::{load_catalog, load_site};
use services::inference_route_resolver;

pub type SelectedModelService = ModelDeploymentService<
    InferenceController<crate::model::repository::ModelDeclarationRepository, FileCurrentState>,
>;
pub type SelectedCatalogService = CatalogService;
pub(crate) type SelectedRouteService = RouteService;
pub type SelectedApplicationService =
    ApplicationService<ApplicationController<SiteRepository, FileCurrentState>>;

/// Backend services built from an immutable catalog snapshot and validated site state.
struct BackendServices {
    model_service: SelectedModelService,
    application_service: SelectedApplicationService,
    manager: ManagerBackendServices,
    observers: ObserverStarters,
    node_lifecycle: ManagerNodeControlCoordinator,

    /// Realization of the manager's own SiteNode, alive for as long as the backend serves.
    node: node::NodeLifecycle,
}

/// Public owner and routing capabilities composed only for the site manager process.
struct ManagerBackendServices {
    catalog_service: SelectedCatalogService,
    route_service: SelectedRouteService,
    inference_router: InferenceRouter,
    site_service: Arc<SiteService>,
    diagnostic_service: crate::diagnostic::SiteDiagnosticService,
    metrics_service: crate::metrics::MetricsService,

    /// The capacity component's node memory read.
    capacity_service: crate::node_memory::CapacityService,

    /// Publisher of owner reads to open resource watches, parked while nobody watches.
    resource_watch: crate::resource_watch::ResourceWatchHub,

    /// Receiver of every node's metric bucket stream, the manager's own node in process.
    metric_buckets: Arc<MetricBucketIngest>,

    /// Node control streams of every node, the manager's own node in process.
    node_control_streams: ManagerNodeControlStreams,

    /// Projection of node-hosted deployments into serving targets, stopped with this composition.
    node_targets: crate::model::controller::NodeTargetProjectorTask,
}

/// Builds the active model service from validated catalog, site, and runtime inputs, composed
/// with the whole site manager, including its own SiteNode in process, but without backend
/// transports.
pub async fn model_service_for(
    configuration: &SitePaths,
) -> CompositionResult<IsolatedModelService> {
    model_service_with_sources_for(configuration, Default::default()).await
}

/// Builds the active model service like `model_service_for`, with the Dotcore sources the
/// manager's own SiteNode builds Nix artifacts from, so it can host development-source
/// deployments.
pub async fn model_service_with_sources_for(
    configuration: &SitePaths,
    dotcore_sources: dotcore_domain::nix::DotcoreSources,
) -> CompositionResult<IsolatedModelService> {
    let span = info_span!(
        "model_composition",
        operation = "model_composition",
        outcome = field::Empty,
        error_kind = field::Empty,
    );
    let result = async {
        let services = compose_capabilities_with_current(
            configuration,
            ManagerNodeProcessInputs {
                telemetry_sink: None,
                dotcore_sources,
            },
            0,
            BackendTestSupport::default(),
            system_collector(Arc::new(dotcore_domain::clock::SystemClock)),
            None,
            None,
        )
        .await?;
        Ok(IsolatedModelService::new(services))
    }
    .instrument(span.clone())
    .await;
    record_composition_result(&span, &result);
    result
}

/// Builds the application service of a fully composed site manager, including its own SiteNode in
/// process, but without backend transports. Applications publish `inference_port` as the site's
/// inference endpoint.
pub async fn application_service_for(
    configuration: &SitePaths,
    inference_port: u16,
) -> CompositionResult<IsolatedApplicationService> {
    let span = info_span!(
        "application_composition",
        operation = "application_composition",
        outcome = field::Empty,
        error_kind = field::Empty,
    );
    let result = async {
        let services = compose_capabilities_with_current(
            configuration,
            ManagerNodeProcessInputs::default(),
            inference_port,
            BackendTestSupport::default(),
            system_collector(Arc::new(dotcore_domain::clock::SystemClock)),
            None,
            None,
        )
        .await?;
        Ok(IsolatedApplicationService::new(services))
    }
    .instrument(span.clone())
    .await;
    record_composition_result(&span, &result);
    result
}

#[cfg(test)]
async fn compose_capabilities<C>(
    configuration: &SitePaths,
    telemetry_sink: Option<Arc<dyn crate::telemetry::TelemetrySink>>,
    inference_port: u16,
    test_support: BackendTestSupport,
    collector: C,
) -> CompositionResult<BackendServices>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    compose_capabilities_with_current(
        configuration,
        ManagerNodeProcessInputs {
            telemetry_sink,
            dotcore_sources: Default::default(),
        },
        inference_port,
        test_support,
        collector,
        None,
        None,
    )
    .await
}

/// Inputs of the site manager's own SiteNode taken from the process configuration rather than
/// the site.
#[derive(Default)]
struct ManagerNodeProcessInputs {
    telemetry_sink: Option<Arc<dyn crate::telemetry::TelemetrySink>>,

    /// Dotcore sources the manager node builds Nix artifacts from, when it hosts them.
    dotcore_sources: dotcore_domain::nix::DotcoreSources,
}

async fn compose_capabilities_with_current<C>(
    configuration: &SitePaths,
    process: ManagerNodeProcessInputs,
    inference_port: u16,
    test_support: BackendTestSupport,
    collector: C,
    current: Option<FileCurrentState>,
    worker_channels: Option<WorkerChannels>,
) -> CompositionResult<BackendServices>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    let span = info_span!(
        "backend_composition",
        operation = "backend_composition",
        execution_target_count = field::Empty,
        outcome = field::Empty,
        error_kind = field::Empty,
    );
    let result = async {
        let inputs = complete_inputs(configuration, current)?;
        let log_admission = crate::management_logs::ManagementLogAdmission::new();
        let bound_node = node_binding::node_configuration_for(&configuration.root, &inputs.site)?;
        let manager_session = ManagerSession::generate().map_err(|error| {
            CompositionError::composition_with_source(
                "generate manager process lifecycle session",
                error,
            )
        })?;
        let mut manager_node_control = ManagerNodeControlCoordinator::with_clock(
            manager_session,
            backend_clock(&test_support),
        );
        if let Some(silence_bound) = test_support.liveness_silence_bound {
            manager_node_control = manager_node_control.with_silence_bound(silence_bound);
        }
        // Site-wide metric history lives with the site manager for this process's lifetime.
        let clock = backend_clock(&test_support);
        let metric_store = Arc::new(
            InMemoryMetricStore::start(
                InMemoryStoreConfig::default(),
                metric_catalog(dotcore_site::validation::hardware_refresh_interval(
                    &inputs.site,
                )),
                Arc::clone(&clock),
            )
            .map_err(|error| {
                CompositionError::composition_with_source("start the metric store", error)
            })?,
        );
        let metric_buckets = Arc::new(MetricBucketIngest::new(
            metric_store.ingestor(),
            Arc::clone(&clock),
        ));
        #[cfg(feature = "test-support")]
        if let Some(slot) = &test_support.metric_recorder {
            slot.fill(metric_store.recorder());
        }
        let manager_node_registry = NodeRegistry::with_coordinator(
            Arc::new(inputs.site_repository.clone()),
            manager_node_control.clone(),
        );
        // One incarnation names the manager node's observations, views, and status.
        let node_incarnation = NodeIncarnation::generate().map_err(|error| {
            CompositionError::composition_with_source(
                "generate the manager node's process incarnation",
                error,
            )
        })?;
        let declared_machine = bound_node.declared_machine.clone().map(|machine| {
            dotcore_runtime_backends::simulated::DeclaredMachine::new(machine, Arc::clone(&clock))
        });
        // The manager's own node reports like any node, in process, before any owner composes,
        // so admission reads its target capabilities from the first request served.
        let manager_node_observation = node::compose_manager_node_observation(
            &inputs.site,
            &bound_node.node_id,
            node_incarnation.clone(),
            manager_node_registry.clone(),
            node::hardware_collector(declared_machine.as_ref(), || Box::new(collector)),
            configuration,
            &test_support,
        )
        .await?;
        let target_capabilities: Arc<dyn TargetCapabilitySource> = Arc::new(
            controllers::RegistryTargetCapabilitySource::new(manager_node_registry.clone()),
        );
        let model_service = model_owner_for(
            &inputs,
            Arc::clone(&target_capabilities),
            log_admission.clone(),
        )
        .await?;
        let manager_host = bound_node_host(&bound_node)?;
        let publication_host = publication_host_for(
            &bound_node,
            &manager_host,
            &dotcore_domain::SystemHostResolver,
        )
        .await?;
        let routing = routing_for(&inputs.site_repository, &model_service)?;
        let application_service = application_owner_for(
            &inputs,
            ApplicationControllerConfiguration {
                configuration_sources: ApplicationConfigurationSources::new(
                    manager_host,
                    inference_port,
                ),
                inference_route_resolver: inference_route_resolver(routing.route_manager.clone()),
                target_capabilities,
            },
            log_admission,
        )?;
        let site_service = Arc::new(SiteService::new(
            Arc::new(inputs.site_repository.clone()),
            bound_node.node_id.clone(),
            manager_node_registry.clone(),
        ));
        let coordinator = manager_node_control.clone();
        let model_controller = model_service.observer_controller();
        model_controller.set_remote_status_source(coordinator.clone());
        application_service
            .observer_controller()
            .set_remote_status_source(coordinator.clone());
        let site_workloads: Arc<dyn crate::hardware::workloads::SiteWorkloadSource> = Arc::new(
            controllers::ControllerWorkloads::new(model_controller.deployment_projection(), {
                let applications = application_service.observer_controller();
                move || Ok(applications.public_projection()?.deployments)
            }),
        );
        model_controller.set_site_workloads(Arc::clone(&site_workloads));
        let node_memory = Arc::new(crate::node_memory::NodeMemoryRead::new(
            Arc::new(manager_node_registry.clone()),
            site_workloads,
        ));
        let node_targets = model_controller.start_node_targets(
            inputs.site_repository.clone(),
            coordinator.clone(),
            bound_node.node_id.to_string(),
        );
        let desired_view_publisher = ManagerNodeViewPublisher::new(
            inputs.site_repository.clone(),
            coordinator.clone(),
            Arc::new(model_controller.clone()),
            Arc::new(crate::application::ApplicationNodeViewSection::new(
                inference_route_resolver(routing.route_manager.clone()),
                inputs.site_repository.clone(),
                inputs.current.clone(),
            )),
        );
        let node_control_streams = ManagerNodeControlStreams::new(
            manager_node_registry.clone(),
            desired_view_publisher,
            node_control_stream_configuration(&test_support)?,
        );
        let manager_node = node::compose_node(
            node::NodeInputs {
                site_id: inputs.site.site_id.clone(),
                node_id: bound_node.node_id.to_string(),
                installed_targets: manager_node_targets(&inputs.site, &bound_node),
                incarnation: node_incarnation,
                observation: manager_node_observation,
                paths: configuration.clone(),
                node_store: inputs.node_store.clone(),
                publication_host: Some(publication_host),
                telemetry_sink: process.telemetry_sink,
                dotcore_sources: process.dotcore_sources,
                declared_machine,
                test_support,
            },
            node::NodeTransports {
                control: Arc::new(InProcessNodeControlTransport::new(
                    inputs.site.site_id.clone(),
                    bound_node.node_id.to_string(),
                    node_control_streams.clone(),
                )),
                metrics: Arc::new(InProcessBucketTransport::new(Arc::clone(&metric_buckets))),
            },
        )
        .await?;
        let node_logs = crate::deployment_logs::NodeDeploymentLogs::new(
            worker_channels.clone(),
            inputs.site_repository.clone(),
            coordinator.clone(),
        )
        .with_local_node(bound_node.node_id.to_string(), manager_node.log_reader());
        let diagnostic_service = crate::diagnostic::SiteDiagnosticService::new(
            crate::diagnostic::NodeDiagnostics::new(
                worker_channels.clone(),
                inputs.site_repository.clone(),
                coordinator,
            )
            .with_local_node(
                bound_node.node_id.to_string(),
                manager_node.diagnostic_reader(),
            ),
        );
        model_controller.set_node_log_source(node_logs.clone());
        application_service
            .observer_controller()
            .set_node_log_source(node_logs);
        let inference_router = routing
            .inference_router
            .with_peer_client(crate::router::adapters::peer::PeerInferenceClient::new(
                worker_channels,
                manager_node.exact_target_admission(),
            ))
            .with_metrics(metric_store.recorder(), Arc::clone(&clock));
        let resource_watch = crate::resource_watch::ResourceWatchHub::new(
            manager_node_control.manager_session().as_str().to_owned(),
            crate::resource_watch::owner_reads(
                Arc::clone(&site_service),
                model_controller.clone(),
                application_service.observer_controller(),
                routing.route_manager.clone(),
                Arc::clone(&node_memory),
            ),
            clock,
            crate::resource_watch::ResourceWatchConfig::default(),
        );
        let manager = ManagerBackendServices {
            catalog_service: CatalogService::new(Arc::clone(&inputs.catalog)),
            route_service: routing.route_service,
            inference_router,
            site_service,
            diagnostic_service,
            metrics_service: crate::metrics::MetricsService::new(metric_store),
            capacity_service: crate::node_memory::CapacityService::new(node_memory),
            resource_watch,
            metric_buckets,
            node_control_streams,
            node_targets,
        };
        let (node_starters, node_lifecycle) = manager_node.into_starters(None);
        let observers = capture_observers(&model_service, &manager.route_service, node_starters);
        let execution_target_count = inputs.site.execution_targets.len();
        let services = BackendServices {
            model_service,
            application_service,
            manager,
            observers,
            node_lifecycle: manager_node_control,
            node: node_lifecycle,
        };
        span.record("execution_target_count", execution_target_count);
        Ok(services)
    }
    .instrument(span.clone())
    .await;
    record_composition_result(&span, &result);
    result
}

struct CompleteCompositionInputs {
    catalog: SharedCatalog,
    site_repository: SiteRepository,
    site: dotcore_proto::dotcore::site::v1::Site,
    current: FileCurrentState,

    /// Directory of the manager node's node store.
    node_store: std::path::PathBuf,
}

struct ManagerRoutingComposition {
    route_manager: RouteManager,
    route_service: SelectedRouteService,
    inference_router: InferenceRouter,
}

fn complete_inputs(
    configuration: &SitePaths,
    current: Option<FileCurrentState>,
) -> CompositionResult<CompleteCompositionInputs> {
    debug!(event_kind = "backend_inputs_loading", site_root = %configuration.root.display(), "loading backend catalog and site inputs");
    node_binding::require_full_site_manager(&configuration.root)?;
    let catalog = Arc::new(load_catalog(configuration)?);
    let current = match current {
        Some(current) => current,
        None => open_owner_state(configuration)?,
    };
    let node_store = prepare_site_node_store(&configuration.root)
        .map_err(|error| CompositionError::Composition(anyhow::Error::new(error)))?;
    let site_repository = load_site(configuration)?;
    let site = site_repository.snapshot()?;
    info!(event_kind = "backend_inputs_loaded", site_id = %site.site_id, "backend catalog and site inputs loaded");
    Ok(CompleteCompositionInputs {
        catalog,
        site_repository,
        site,
        current,
        node_store,
    })
}

/// Declarations of every metric the site manager's store accepts, beyond those the store records
/// about itself. Metrics recorded once per hardware collection follow the site's
/// `refresh_interval`, the same one every node collects at.
fn metric_catalog(refresh_interval: Duration) -> Vec<MetricDescriptor> {
    let mut descriptors = hardware_metric_descriptors();
    descriptors.extend(forwarder_metric_descriptors());
    descriptors.extend(crate::router::metrics::router_metric_descriptors());
    descriptors
        .extend(crate::model::controller::metrics::deployment_metric_descriptors(refresh_interval));
    descriptors.extend(crate::application::application_metric_descriptors(
        refresh_interval,
    ));
    descriptors
}

/// Timing of the manager's node control streams: production defaults, or the test override.
fn node_control_stream_configuration(
    test_support: &BackendTestSupport,
) -> CompositionResult<NodeControlStreamConfiguration> {
    let defaults = NodeControlStreamConfiguration::default();
    match test_support.view_republish_interval {
        Some(interval) => defaults.with_republish_interval(interval).map_err(|error| {
            CompositionError::composition_with_source("configure node control streams", error)
        }),
        None => Ok(defaults),
    }
}

/// Chooses the clock of one composed backend: the test's manual clock when it supplies one,
/// otherwise system time.
fn backend_clock(test_support: &BackendTestSupport) -> Arc<dyn dotcore_domain::clock::Clock> {
    #[cfg(feature = "test-support")]
    if let Some(clock) = &test_support.clock {
        return Arc::new(clock.clone());
    }
    #[cfg(not(feature = "test-support"))]
    let _ = test_support;
    Arc::new(dotcore_domain::clock::SystemClock)
}

/// Composes the inference controller that owns model declarations, over the site's owner current
/// state.
async fn model_owner_for(
    inputs: &CompleteCompositionInputs,
    target_capabilities: Arc<dyn TargetCapabilitySource>,
    log_admission: crate::management_logs::ManagementLogAdmission,
) -> CompositionResult<SelectedModelService> {
    let inventory = target_capabilities.observe().await.map_err(|error| {
        CompositionError::composition_with_source("target capability observation failed", error)
    })?;
    let desired = crate::model::repository::ModelDeclarationRepository::new_with_lifecycle(
        inputs.site_repository.clone(),
        Arc::clone(&inputs.catalog),
        inventory,
        inputs.current.clone(),
    );
    let controller =
        InferenceController::new(desired, inputs.current.clone(), target_capabilities)?;
    Ok(ModelDeploymentService::new(controller, log_admission))
}

/// The execution targets of the manager's own SiteNode, which it realizes like any node.
fn manager_node_targets(
    site: &dotcore_proto::dotcore::site::v1::Site,
    bound_node: &BoundSiteNode,
) -> Vec<dotcore_proto::dotcore::site::v1::ExecutionTarget> {
    site.execution_targets
        .iter()
        .filter(|target| target.site_node_id == bound_node.node_id.as_str())
        .cloned()
        .collect()
}

fn routing_for(
    site_repository: &SiteRepository,
    model_service: &SelectedModelService,
) -> CompositionResult<ManagerRoutingComposition> {
    let serving_targets = model_service.serving_targets();
    let route_manager = RouteManager::new(
        RouteDeclarationRepository::new(site_repository.clone()),
        RouteAvailabilityTargetsSource::new(serving_targets.clone()),
    )
    .map_err(|error| {
        CompositionError::composition_with_source("Route-manager composition", error)
    })?;
    let target_change_manager = route_manager.clone();
    serving_targets.register_change_observer(Arc::new(move || {
        target_change_manager.observe_serving_target_change();
    }));
    let inference_router = InferenceRouter::new(serving_targets).with_routes(route_manager.clone());
    let route_service = RouteService::new(route_manager.clone());
    Ok(ManagerRoutingComposition {
        route_manager,
        route_service,
        inference_router,
    })
}

/// Composes the apps-controller owner of application declarations, over the site's owner current
/// state.
fn application_owner_for(
    inputs: &CompleteCompositionInputs,
    configuration: ApplicationControllerConfiguration,
    log_admission: crate::management_logs::ManagementLogAdmission,
) -> CompositionResult<SelectedApplicationService> {
    let controller = ApplicationController::new(
        Arc::clone(&inputs.catalog),
        inputs.site_repository.clone(),
        inputs.current.clone(),
        configuration,
    )
    .map_err(|error| CompositionError::composition_with_source("Application composition", error))?;
    Ok(ApplicationService::new(controller, log_admission))
}

fn capture_observers(
    model_service: &SelectedModelService,
    route_service: &SelectedRouteService,
    node: node::NodeStarters,
) -> ObserverStarters {
    let model_observer = model_service.observer_controller();
    let route_observer = route_service.clone();
    let node_model = node.model;
    ObserverStarters {
        reporter: node.reporter,
        node_control: node.control,
        model: Box::new(move || {
            model_observer.start_observer();
            node_model();
        }),
        application: node.application,
        route: Some(Box::new(move || route_observer.start_observer())),
    }
}

/// Loads the immutable catalog sources for the catalog service.
pub fn catalog_service_for(configuration: &SitePaths) -> CompositionResult<SelectedCatalogService> {
    Ok(CatalogService::new(Arc::new(load_catalog(configuration)?)))
}

/// Loads the complete desired site and its catalog without opening current state.
pub fn desired_site_for(configuration: &SitePaths) -> CompositionResult<SiteRepository> {
    let catalog = Arc::new(load_catalog(configuration)?);
    desired_site_from_catalog(configuration, catalog)
}

/// Error raised while composing backend capabilities.
#[derive(Debug, thiserror::Error)]
pub enum CompositionError {
    #[error("Backend composition failed")]
    Composition(#[source] anyhow::Error),

    /// The SiteNode bound at a full site root is a worker, which starts only from its joined
    /// node folder.
    #[error(
        "Backend composition failed: node {node_id} is not the site manager and cannot start from \
         a full site; start it from its node folder after joining it to the site"
    )]
    NotSiteManager { node_id: String },

    /// The node host name does not resolve on this machine, usually because the network
    /// that provides it is not up, so the server can neither publish applications on it nor
    /// bind the private listener derived from it.
    #[error(
        "cannot resolve the node host {host}; start the network that provides it and start the \
         server again, or reset the site with another node host"
    )]
    UnresolvableNodeHost {
        host: dotcore_domain::SiteNodeHost,
        #[source]
        source: dotcore_domain::HostResolutionError,
    },

    #[error("Backend composition Server failure")]
    Server(#[from] crate::server::ServerError),

    #[error("Backend composition catalog failure")]
    Catalog(#[from] dotcore_catalog::CatalogError),

    #[error("Backend composition desired site failure")]
    SiteRepository(#[from] SiteRepositoryError),

    #[error("Backend composition site input lock failure")]
    SiteInputLock(#[from] SiteInputLockError),

    #[error("Backend composition node configuration failure: node.yaml")]
    NodeConfiguration(#[from] dotcore_proto::yaml::RestrictedYamlError),

    #[error("Backend composition logging failure")]
    Logging(#[from] crate::logging::LoggingError),

    #[error("Backend composition runtime target failure")]
    Runtime(#[from] crate::dispatch::model::RuntimeTargetError),

    #[error("Backend composition runtime backend failure")]
    RuntimeBackend(#[from] dotcore_runtime::RuntimeBackendError),

    #[error("Backend composition inference-controller failure")]
    Controller(#[from] crate::model::controller::ModelLifecycleError),

    #[error(transparent)]
    CurrentState(#[from] crate::model::state::CurrentStateError),
}

/// Opens the site's owner current state for a full-site start after checking that the site keeps
/// node state in its node store. The check precedes the open, which may update `state.pb`.
pub(super) fn open_owner_state(configuration: &SitePaths) -> CompositionResult<FileCurrentState> {
    prepare_site_node_store(&configuration.root).map_err(|error| {
        error!(event_kind = "backend_preparation_failed", error_kind = "state_layout", error = %error, "backend preparation failed");
        CompositionError::Composition(anyhow::Error::new(error))
    })?;
    FileCurrentState::open_at_startup(
        configuration.root.join(SITE_FILE),
        &crate::model::site_declaration_rules(),
    )
    .map_err(lifecycle_current_state_error)
}

pub(super) fn lifecycle_current_state_error(
    error: impl Into<crate::model::state::CurrentStateError>,
) -> CompositionError {
    match error.into() {
        crate::model::state::CurrentStateError::SiteInput(error) => {
            CompositionError::SiteRepository(error)
        }
        error => CompositionError::CurrentState(error),
    }
}
/// Result type owned by backend composition.
pub type CompositionResult<T> = Result<T, CompositionError>;

impl CompositionError {
    fn composition(message: impl Into<String>) -> Self {
        Self::Composition(anyhow!(message.into()))
    }

    pub(super) fn composition_with_source(
        message: &'static str,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self::Composition(source.into().context(message))
    }
}

type SharedCatalog = Arc<Catalog>;

fn record_composition_result<T>(span: &Span, result: &CompositionResult<T>) {
    match result {
        Ok(_) => {
            span.record("outcome", "success");
        }
        Err(_) => {
            span.record("outcome", "failed");
            span.record("error_kind", "composition");
            error!(parent: span, error_kind = "composition", "backend composition failed");
        }
    }
}

/// Returns the bound SiteNode's declared host, the name or address other nodes reach it at.
fn bound_node_host(bound_node: &BoundSiteNode) -> CompositionResult<dotcore_domain::SiteNodeHost> {
    dotcore_domain::SiteNodeHost::parse(&bound_node.declaration.host).map_err(|error| {
        CompositionError::composition_with_source("bound node host is not valid", error)
    })
}

/// Resolves the manager's declared host to the IPv4 address its applications publish on. A host
/// name that resolves to a loopback address here is logged as a warning, since other machines
/// cannot reach what is published there.
async fn publication_host_for(
    bound_node: &BoundSiteNode,
    host: &dotcore_domain::SiteNodeHost,
    resolver: &dyn dotcore_domain::HostResolver,
) -> CompositionResult<Ipv4Addr> {
    let (address, loopback_warning) =
        host.resolve_with_loopback_warning(resolver)
            .await
            .map_err(|source| CompositionError::UnresolvableNodeHost {
                host: host.clone(),
                source,
            })?;
    if let Some(warning) = loopback_warning {
        warn!(event_kind = "site_node_host_loopback", node_id = %bound_node.node_id, host = %host, "{warning}");
    }
    Ok(address)
}

fn desired_site_from_catalog(
    configuration: &SitePaths,
    _catalog: SharedCatalog,
) -> Result<SiteRepository, CompositionError> {
    load_site(configuration)
}

#[cfg(test)]
static CATALOG_LOAD_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SITE_LOAD_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
