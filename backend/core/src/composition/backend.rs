//! Sole whole-backend preparation and serving lifecycle.

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use dotcore_domain::{
    BindAddress, HostResolver, SiteNodeHost, SystemHostResolver, clock::Clock,
    hardware_observation::HardwareSnapshotSource, timing::Timing,
};
#[cfg(feature = "test-support")]
use dotcore_domain::{
    clock::{ManualClock, slot_start},
    real_time,
};
#[cfg(feature = "test-support")]
use dotcore_site::node::control::NodeLivenessAges;
use dotcore_site::{
    initial::input_store::{SiteInputLock, SiteInputLockError, acquire_shared_site_lock},
    node::{
        agent::{NodeReporterTask, WorkerPrivateClientConfiguration},
        binding::DEFAULT_LOG_RETENTION_DAYS,
        control::ManagerNodeControlCoordinator,
        folder::{NodeFolderValidator, WorkerNodeCustody},
        session::{ManagerNodeControlClient, NodeControlObservations, NodeControlSessionTask},
    },
    store::prepare_joined_node_store,
    view::fencing::NodeIncarnation,
    worker_channels::WorkerChannels,
};
use tracing::{error, info, warn};

use super::{
    BackendConfiguration, BackendRole, CompositionResult, ManagerPrivateListener,
    compose_capabilities_with_current,
    node::{
        NodeInputs, NodeObservation, NodeObservationInputs, NodeStarters, NodeTransports,
        compose_node,
    },
    routes, system_collector,
};
use crate::server::private::PrivateListener;

/// Longest time startup waits for a site writer to release the site input lock.
const SITE_INPUT_LOCK_WAIT: Duration = Duration::from_secs(30);

/// Complete prepared backend and the only whole-product composition façade.
///
/// Preparation composes capabilities, registers the node's first observation on the manager, and reserves transport addresses without starting observer or reconciliation tasks. Serving starts the node observation reporter, then model and application observers, and the manager-only route observer before transport dispatch without waiting for convergence.
pub struct Backend {
    addresses: crate::server::ServerAddresses,
    server: crate::server::Server,
    observers: ObserverStarters,
    _logging: crate::logging::BackendLogging,
    _site_input_lock: Option<SiteInputLock>,
    _worker_custody: Option<WorkerNodeCustody>,
    _lifecycle: BackendLifecycleHandles,
}

impl Backend {
    /// Completes fallible composition and transport preparation without starting observers or
    /// reconciliation tasks. The backend runs this process's timing policy.
    pub async fn prepare<F>(
        configuration: BackendConfiguration,
        shutdown: F,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self::prepare_with_snapshot_collector_factory(
            configuration,
            shutdown,
            BackendTestSupport::development_on(Timing::for_process()),
            |_, _, clock| Ok(system_collector(clock)),
            || {},
            crate::server::private::PrivateServiceAssembly::empty(),
        )
        .await
    }

    /// Completes preparation with private services assembled after node custody is established.
    #[cfg(feature = "test-support")]
    pub async fn prepare_with_private_service_assembly<F>(
        configuration: BackendConfiguration,
        private_services: crate::server::private::PrivateServiceAssembly,
        shutdown: F,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self::prepare_with_snapshot_collector_factory(
            configuration,
            shutdown,
            BackendTestSupport::development(),
            |site, site_root, clock| {
                super::node_binding::node_configuration_for(site_root, site)?;
                Ok(system_collector(clock))
            },
            || {},
            private_services,
        )
        .await
    }

    /// Completes preparation with explicit concrete simulation controls for integration fixtures.
    #[cfg(feature = "test-support")]
    pub async fn prepare_with_test_support<F>(
        configuration: BackendConfiguration,
        shutdown: F,
        test_support: BackendTestSupport,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self::prepare_with_snapshot_collector_factory(
            configuration,
            shutdown,
            test_support,
            |site, site_root, clock| {
                super::node_binding::node_configuration_for(site_root, site)?;
                Ok(system_collector(clock))
            },
            || {},
            crate::server::private::PrivateServiceAssembly::empty(),
        )
        .await
    }

    /// Completes preparation with explicit simulation controls and the local hardware snapshot
    /// collector a node that declares no machine reads, without starting observers or
    /// reconciliation tasks.
    #[cfg(feature = "test-support")]
    pub async fn prepare_with_snapshot_collector<F, C>(
        configuration: BackendConfiguration,
        shutdown: F,
        test_support: BackendTestSupport,
        collector: C,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
        C: HardwareSnapshotSource + Send + Sync + 'static,
    {
        Self::prepare_with_snapshot_collector_factory(
            configuration,
            shutdown,
            test_support,
            move |_, _, _| Ok(collector),
            || {},
            crate::server::private::PrivateServiceAssembly::empty(),
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn prepare_with_snapshot_collector_after_lock_attempt<F, C, A>(
        configuration: BackendConfiguration,
        shutdown: F,
        collector: C,
        lock_attempt: A,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
        C: HardwareSnapshotSource + Send + Sync + 'static,
        A: FnOnce(),
    {
        Self::prepare_with_snapshot_collector_factory(
            configuration,
            shutdown,
            BackendTestSupport::default(),
            move |_, _, _| Ok(collector),
            lock_attempt,
            crate::server::private::PrivateServiceAssembly::empty(),
        )
        .await
    }

    async fn prepare_with_snapshot_collector_factory<F, C, B, A>(
        configuration: BackendConfiguration,
        shutdown: F,
        test_support: BackendTestSupport,
        collector_for: B,
        lock_attempt: A,
        private_services: crate::server::private::PrivateServiceAssembly,
    ) -> CompositionResult<Self>
    where
        F: Future<Output = ()> + Send + 'static,
        C: HardwareSnapshotSource + Send + Sync + 'static,
        B: FnOnce(
            &dotcore_proto::dotcore::site::v1::Site,
            &std::path::Path,
            Arc<dyn Clock>,
        ) -> CompositionResult<C>,
        A: FnOnce(),
    {
        info!(event_kind = "backend_preparation_started", site_root = %configuration.site_paths.root.display(), "backend preparation started");
        if let Some(ignored) = dotcore_domain::docker::ignored_endpoint() {
            warn!(event_kind = "docker_endpoint_ignored", ignored = %ignored, endpoint = %dotcore_domain::docker::local_endpoint(), "Docker endpoint from the environment is not a Unix socket; using the system Docker socket instead");
        }
        lock_attempt();
        let server = match configuration.role.clone() {
            BackendRole::Manager(server) => server,
            BackendRole::JoinedNode {
                private_bind_address,
            } => {
                return prepare_joined_node(
                    configuration,
                    private_bind_address,
                    private_services,
                    shutdown,
                    test_support,
                )
                .await;
            }
        };
        let site_root = configuration.site_paths.root.clone();
        let site_input_lock = tokio::task::spawn_blocking(move || {
            acquire_shared_site_lock(&site_root, SITE_INPUT_LOCK_WAIT)
        })
        .await
        .unwrap_or_else(|error| {
            Err(SiteInputLockError::Storage(
                anyhow::Error::new(error).context("join site input lock acquisition"),
            ))
        })
        .map_err(|error| {
            error!(event_kind = "backend_preparation_failed", error_kind = "site_lock", error = %error, "backend preparation failed");
            error
        })?;
        super::node_binding::require_full_site_manager(&configuration.site_paths.root)?;
        let current = super::open_owner_state(&configuration.site_paths)?;
        let site = super::loading::load_site(&configuration.site_paths)?.snapshot()?;
        let logging_retention_days = super::node_binding::validated_logging_retention_days_for(
            &configuration.site_paths.root,
            &site,
        )?;
        let collector = collector_for(
            &site,
            &configuration.site_paths.root,
            super::backend_clock(&test_support),
        )?;
        let logging = crate::logging::activate_backend_logging(
            &configuration.site_paths.root,
            logging_retention_days,
        )
        .map_err(|error| {
            error!(event_kind = "backend_preparation_failed", error_kind = "logging", error = %error, "backend preparation failed");
            error
        })?;
        let resolver = SystemHostResolver;
        let private_bind_address =
            super::private::manager_bind_address(&server.private_listener, &site)?;
        let private_source = match server.private_listener {
            ManagerPrivateListener::Configured(_) => "Configured",
            ManagerPrivateListener::SiteDefault { .. } => "SiteDefault",
        };
        let private_configuration = match &private_bind_address {
            Some(address) => Some(super::private::manager_configuration(
                &configuration.site_paths.root,
                &site,
                resolve_manager_private_address(address, &server.private_listener, &resolver)
                    .await?,
            )?),
            None => None,
        };
        let transports = crate::server::bind(
            resolve_bind_address(&server.management_bind_address, "management", &resolver).await?,
            resolve_bind_address(&server.inference_bind_address, "inference", &resolver).await?,
        )
        .await?;
        let mut addresses = transports.addresses();
        let inference_port = addresses
            .inference
            .map(|address| address.port())
            .ok_or_else(|| {
                super::CompositionError::composition(
                    "the manager's inference listener is not bound",
                )
            })?;
        let timing = super::checked_timing(&test_support)?;
        // One holder of worker pins and channels serves both forwarded inference and logs.
        let worker_channels = private_configuration
            .as_ref()
            .map(|configuration| WorkerChannels::new(configuration, timing.private));
        let capabilities = compose_capabilities_with_current(
            &configuration.site_paths,
            super::ManagerNodeProcessInputs {
                telemetry_sink: configuration.telemetry_directory.clone().map(|directory| {
                    Arc::new(crate::telemetry::FileTelemetrySink::new(directory))
                        as Arc<dyn crate::telemetry::TelemetrySink>
                }),
                dotcore_sources: configuration.dotcore_sources.clone(),
            },
            inference_port,
            test_support,
            collector,
            Some(current),
            worker_channels,
        )
        .await?;
        let (routes, router, observers, manager_private_services, lifecycle, resource_watch) =
            routes::assemble(capabilities);
        // Open watches would otherwise hold the graceful shutdown of the management listener.
        let shutdown = async move {
            shutdown.await;
            resource_watch.shutdown();
        };
        let private_services = private_services.combine(manager_private_services);
        let private_listener = private_configuration.map(|configuration| PrivateListener {
            routes: private_services
                .assemble(configuration.local_role, &configuration.local_node_id),
            configuration,
            timing: timing.private,
        });
        let server = crate::server::Server::prepare(
            transports,
            server.allowed_origins,
            routes,
            Some(router),
            private_listener,
            shutdown,
        )
        .await
        .map_err(|error| {
            manager_private_bind_failure(
                error,
                private_bind_address.as_ref(),
                &server.private_listener,
            )
        })?;
        addresses.private = server.private_address();
        if let (Some(address), Some(requested)) = (addresses.private, &private_bind_address) {
            info!(event_kind = "manager_private_listener_bound", address = %address, requested = %requested, source = private_source, "manager private listener bound");
        }
        info!(event_kind = "backend_preparation_completed", management_address = ?addresses.management, inference_address = ?addresses.inference, "backend preparation completed");
        Ok(Self {
            addresses,
            server,
            observers,
            _logging: logging,
            _site_input_lock: Some(site_input_lock),
            _worker_custody: None,
            _lifecycle: lifecycle,
        })
    }

    /// Returns the exact addresses reserved by this prepared backend.
    pub fn addresses(&self) -> crate::server::ServerAddresses {
        self.addresses
    }

    /// Returns a view of the node liveness this site manager tracks, which stays usable after
    /// serving consumes the backend; `None` on a joined node.
    #[cfg(feature = "test-support")]
    pub fn node_liveness(&self) -> Option<NodeLiveness> {
        match &self._lifecycle {
            BackendLifecycleHandles::Manager {
                _nodes: coordinator,
                ..
            } => Some(NodeLiveness {
                coordinator: coordinator.clone(),
            }),
            BackendLifecycleHandles::Worker { .. } => None,
        }
    }

    /// Starts the node observation reporter, followed by model, node view, application, and manager-only route observers before transport dispatch.
    ///
    /// Observer invocation does not wait for initial observation, reconciliation, target publication, route availability, or controller convergence.
    pub async fn serve(self) -> CompositionResult<()> {
        info!(
            event_kind = "backend_observers_starting",
            "backend observers starting"
        );
        let _observer_tasks = self.observers.start();
        let result = self.server.serve().await;
        if let Err(error) = &result {
            error!(event_kind = "backend_server_failed", error = %error, "backend server failed");
        }
        result?;
        info!(
            event_kind = "backend_server_stopped",
            "backend server stopped"
        );
        Ok(())
    }
}

/// Test view of a site manager's node liveness: how long ago each node's current incarnation
/// made contact and had a status accepted, on the manager's clock.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct NodeLiveness {
    coordinator: ManagerNodeControlCoordinator,
}

#[cfg(feature = "test-support")]
impl NodeLiveness {
    /// Returns the current contact and status ages by SiteNode ID.
    pub fn ages(&self) -> std::collections::BTreeMap<String, NodeLivenessAges> {
        self.coordinator.liveness_ages()
    }
}

/// Starts a joined node from its node folder: the folder supplies the node inputs, and the node
/// reaches the manager through authenticated private RPC.
async fn prepare_joined_node<F>(
    configuration: BackendConfiguration,
    private_bind_address: Option<BindAddress>,
    private_services: crate::server::private::PrivateServiceAssembly,
    shutdown: F,
    test_support: BackendTestSupport,
) -> CompositionResult<Backend>
where
    F: Future<Output = ()> + Send + 'static,
{
    let root = &configuration.site_paths.root;
    let custody = WorkerNodeCustody::try_acquire(root).map_err(|error| {
        super::CompositionError::composition_with_source("worker node custody", error)
    })?;
    let node_store = prepare_joined_node_store(root)
        .map_err(|error| super::CompositionError::Composition(anyhow::Error::new(error)))?;
    let folder = NodeFolderValidator::validate_declared(root).map_err(|error| {
        super::CompositionError::composition_with_source("validate node folder", error)
    })?;
    // The process override wins over the address recorded in the node folder.
    let private_address = resolve_bind_address(
        private_bind_address
            .as_ref()
            .unwrap_or_else(|| folder.private_bind_address()),
        "private",
        &SystemHostResolver,
    )
    .await?;
    let private_client = WorkerPrivateClientConfiguration::from_node_folder(&folder);
    let incarnation = NodeIncarnation::generate().map_err(|error| {
        super::CompositionError::composition_with_source(
            "generate worker process incarnation",
            error,
        )
    })?;
    let clock = super::backend_clock(&test_support);
    let timing = super::checked_timing(&test_support)?;
    let declared_machine = folder.declared_machine().map(|machine| {
        dotcore_runtime_backends::simulated::DeclaredMachine::new(machine, Arc::clone(&clock))
    });
    let control = NodeControlObservations::new();
    let observation = NodeObservation::compose(
        NodeObservationInputs {
            site_id: folder.site_id().to_owned(),
            node_id: dotcore_domain::SiteNodeId::parse(folder.worker_node_id()).map_err(
                |error| {
                    super::CompositionError::composition_with_source(
                        "parse the worker's node ID",
                        error,
                    )
                },
            )?,
            observation_configuration: folder.observation_configuration().clone(),
            observation_configuration_sha256: folder
                .observation_configuration_sha256()
                .as_str()
                .to_owned(),
            incarnation: incarnation.clone(),
            hardware_collector: super::node::hardware_collector(declared_machine.as_ref(), || {
                Box::new(system_collector(Arc::clone(&clock)))
            }),
            clock,
            probes: super::node::capability_probes(&configuration.site_paths, &test_support),
        },
        control.clone(),
        Arc::new(control),
    )?;
    let inputs = NodeInputs {
        site_id: folder.site_id().to_owned(),
        node_id: folder.worker_node_id().to_owned(),
        installed_targets: folder.realization_targets().to_vec(),
        incarnation,
        observation,
        paths: configuration.site_paths.clone(),
        node_store,
        publication_host: match private_address.ip() {
            std::net::IpAddr::V4(host) if !host.is_unspecified() => Some(host),
            _ => None,
        },
        telemetry_sink: configuration.telemetry_directory.clone().map(|directory| {
            Arc::new(crate::telemetry::FileTelemetrySink::new(directory))
                as Arc<dyn crate::telemetry::TelemetrySink>
        }),
        dotcore_sources: configuration.dotcore_sources.clone(),
        declared_machine,
        test_support,
    };
    let manager_endpoint = private_client.endpoint().map_err(|error| {
        super::CompositionError::composition_with_source("prepare worker manager client", error)
    })?;
    let transports = NodeTransports {
        control: Arc::new(ManagerNodeControlClient::new(manager_endpoint.clone())),
        metrics: Arc::new(crate::metric_stream::ManagerMetricBucketClient::new(
            manager_endpoint,
            folder.site_id().to_owned(),
        )),
    };
    let private_configuration =
        super::private::worker_configuration_from_node_folder(folder, private_address)?;
    let node = compose_node(inputs, transports).await?;
    let private_routes = private_services.combine(node.private_services()).assemble(
        private_configuration.local_role,
        &private_configuration.local_node_id,
    );
    let logging = crate::logging::activate_backend_logging(root, DEFAULT_LOG_RETENTION_DAYS)?;
    let transports = crate::server::BoundTransports::private_only();
    let mut addresses = transports.addresses();
    let server = crate::server::Server::prepare(
        transports,
        Vec::new(),
        tonic::service::Routes::default(),
        None,
        Some(PrivateListener {
            configuration: private_configuration,
            routes: private_routes,
            timing: timing.private,
        }),
        shutdown,
    )
    .await?;
    addresses.private = server.private_address();
    let (starters, node) = node.into_starters(addresses.private.map(|address| address.port()));
    Ok(Backend {
        addresses,
        server,
        observers: ObserverStarters::joined_node_worker(starters),
        _logging: logging,
        _site_input_lock: None,
        _worker_custody: Some(custody),
        _lifecycle: BackendLifecycleHandles::Worker { _node: node },
    })
}

pub(super) enum BackendLifecycleHandles {
    Manager {
        _nodes: ManagerNodeControlCoordinator,
        _node_targets: crate::model::controller::NodeTargetProjectorTask,
        _node: super::node::NodeLifecycle,
    },
    Worker {
        _node: super::node::NodeLifecycle,
    },
}

pub(super) struct ObserverStarters {
    pub(super) reporter: Box<dyn FnOnce() -> NodeReporterTask + Send>,
    pub(super) node_control: Box<dyn FnOnce() -> NodeControlSessionTask + Send>,
    pub(super) model: Box<dyn FnOnce() + Send>,
    pub(super) application: Box<dyn FnOnce() + Send>,
    pub(super) route: Option<Box<dyn FnOnce() + Send>>,
}

impl ObserverStarters {
    fn joined_node_worker(node: NodeStarters) -> Self {
        Self {
            reporter: node.reporter,
            node_control: node.control,
            model: node.model,
            application: node.application,
            route: None,
        }
    }

    pub(super) fn start(self) -> ObserverTasks {
        info!(
            operation = "observer_start",
            observer = "node_reporter",
            "starting node observation reporter"
        );
        let reporter = (self.reporter)();
        info!(
            operation = "observer_start",
            observer = "model",
            "starting controller observer"
        );
        (self.model)();
        info!(
            operation = "observer_start",
            observer = "node_control",
            "starting node control stream"
        );
        let node_control = (self.node_control)();
        info!(
            operation = "observer_start",
            observer = "application",
            "starting controller observer"
        );
        (self.application)();
        if let Some(route) = self.route {
            info!(
                operation = "observer_start",
                observer = "route",
                "starting controller observer"
            );
            route();
        }
        ObserverTasks {
            _reporter: reporter,
            _node_control: node_control,
        }
    }
}

/// Running observer tasks, stopped when dropped.
pub(super) struct ObserverTasks {
    _reporter: NodeReporterTask,
    _node_control: NodeControlSessionTask,
}

/// Explicit concrete simulation controls used by backend integration fixtures.
#[doc(hidden)]
#[derive(Clone)]
pub struct BackendTestSupport {
    pub simulated_runtime: Option<dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport>,
    pub simulated_inference: Option<crate::inference::simulated::SimulatedInferenceTestSupport>,

    /// Every behavior duration of the composed backend; production unless a test compresses it.
    pub timing: Timing,

    /// Store paths by engine package that a node pins for development-source Nix engines
    /// instead of evaluating its development source; empty keeps the production resolver.
    pub local_development_store_paths: BTreeMap<String, String>,

    /// Launcher binary native execution targets start their workloads with; `None` keeps the
    /// production `dotcore-launcher` beside the server executable.
    pub native_launcher: Option<std::path::PathBuf>,

    /// Clock the composed backend reads instead of system time, such as a manual or virtual test
    /// clock; the test keeps a clone to advance it. `None` keeps system time.
    #[cfg(feature = "test-support")]
    pub clock: Option<Arc<dyn Clock>>,

    /// Receives the site manager's metric recorder, so a test records metrics into the served
    /// history as in-process writers do. `None` keeps the recorder private.
    #[cfg(feature = "test-support")]
    pub metric_recorder: Option<MetricRecorderSlot>,
}

impl BackendTestSupport {
    /// Controls used by an ordinary development backend: representative simulated startup on
    /// the production timing.
    pub fn development() -> Self {
        Self::development_on(Timing::production())
    }

    /// Representative simulated startup whose steps follow `timing`.
    fn development_on(timing: Timing) -> Self {
        Self {
            simulated_runtime: Some(
                dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0)
                    .with_representative_startup(timing.simulated.startup_step_interval),
            ),
            timing,
            ..Self::default()
        }
    }

    /// Controls of a backend test: the test timing on a manual clock whose wall time starts on a
    /// whole second, so wall-aligned schedules start at a known phase.
    #[cfg(feature = "test-support")]
    pub fn test() -> Self {
        Self {
            timing: Timing::test(),
            clock: Some(Arc::new(test_clock())),
            ..Self::default()
        }
    }

    /// Like [`Self::test`], with the development backend's representative simulated startup.
    #[cfg(feature = "test-support")]
    pub fn test_development() -> Self {
        Self {
            clock: Some(Arc::new(test_clock())),
            ..Self::development_on(Timing::test())
        }
    }
}

impl Default for BackendTestSupport {
    /// No simulation controls, on the production timing and system time.
    fn default() -> Self {
        Self {
            simulated_runtime: None,
            simulated_inference: None,
            timing: Timing::production(),
            local_development_store_paths: BTreeMap::new(),
            native_launcher: None,
            #[cfg(feature = "test-support")]
            clock: None,
            #[cfg(feature = "test-support")]
            metric_recorder: None,
        }
    }
}

/// Manual clock of a backend test, starting at the current wall second.
#[cfg(feature = "test-support")]
fn test_clock() -> ManualClock {
    ManualClock::starting_at(slot_start(real_time::wall_now(), Duration::from_secs(1)))
}

/// Shared slot a composed site manager fills with its metric recorder; clones share the slot.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct MetricRecorderSlot(
    Arc<std::sync::OnceLock<Arc<dyn dotcore_observability::MetricRecorder>>>,
);

#[cfg(feature = "test-support")]
impl MetricRecorderSlot {
    /// The recorder, once a site manager composed with this slot started.
    pub fn recorder(&self) -> Option<Arc<dyn dotcore_observability::MetricRecorder>> {
        self.0.get().cloned()
    }

    pub(super) fn fill(&self, recorder: Arc<dyn dotcore_observability::MetricRecorder>) {
        let _ = self.0.set(recorder);
    }
}

/// Resolves the manager's private listener address. A default derived from a manager host name
/// that resolves to loopback fails, since joined nodes on other machines could never reach it.
async fn resolve_manager_private_address(
    address: &BindAddress,
    listener: &ManagerPrivateListener,
    resolver: &dyn HostResolver,
) -> CompositionResult<std::net::SocketAddr> {
    let resolved = match resolve_listener_address(address, "private", resolver).await {
        Ok(resolved) => resolved,
        Err(source) => {
            return Err(match (listener, address.host()) {
                (ManagerPrivateListener::SiteDefault { .. }, Some(host)) => {
                    super::CompositionError::UnresolvableNodeHost {
                        host: host.clone(),
                        source,
                    }
                }
                _ => listener_address_failure(address, "private", source),
            });
        }
    };
    let derived_name = matches!(listener, ManagerPrivateListener::SiteDefault { .. })
        && address.host().is_some_and(SiteNodeHost::is_name);
    if derived_name && resolved.ip().is_loopback() {
        error!(event_kind = "backend_preparation_failed", error_kind = "listener_address", listener = "private", address = %address, resolved = %resolved, "backend preparation failed");
        return Err(super::CompositionError::composition(format!(
            "the manager private listener defaults to the site manager host {address}, which \
             resolves to the loopback address {} on this machine, so joined nodes cannot reach \
             it; declare a manager host they reach, or set {} to a local address",
            resolved.ip(),
            super::PRIVATE_ADDR_ENV,
        )));
    }
    Ok(resolved)
}

/// Explains a failure to bind the manager's default private listener, which binds the site
/// manager host, so a host the machine does not own (for example behind NAT) fails here.
fn manager_private_bind_failure(
    error: crate::server::ServerError,
    private_bind_address: Option<&BindAddress>,
    listener: &ManagerPrivateListener,
) -> super::CompositionError {
    let default_address = private_bind_address
        .filter(|_| matches!(listener, ManagerPrivateListener::SiteDefault { .. }));
    match (default_address, &error) {
        (
            Some(address),
            crate::server::ServerError::Listener(crate::server::ListenerError::Private { .. }),
        ) => super::CompositionError::Composition(anyhow::Error::new(error).context(format!(
            "bind the manager private listener at its default {address}, the site manager host \
             that joined nodes dial; set {} to a local or wildcard address, for example 0.0.0.0:{}",
            super::PRIVATE_ADDR_ENV,
            address.port(),
        ))),
        _ => error.into(),
    }
}

/// Resolves a listener address when the backend starts, failing with the generic listener
/// address error.
async fn resolve_bind_address(
    address: &BindAddress,
    listener: &'static str,
    resolver: &dyn HostResolver,
) -> CompositionResult<std::net::SocketAddr> {
    resolve_listener_address(address, listener, resolver)
        .await
        .map_err(|source| listener_address_failure(address, listener, source))
}

/// Resolves a listener address when the backend starts. A host name that resolves to a loopback
/// address here is logged as a warning, since other machines cannot reach the listener.
async fn resolve_listener_address(
    address: &BindAddress,
    listener: &'static str,
    resolver: &dyn HostResolver,
) -> Result<std::net::SocketAddr, dotcore_domain::HostResolutionError> {
    let (ip, loopback_warning) = match address.host() {
        Some(host) => host.resolve_with_loopback_warning(resolver).await,
        None => Ok((std::net::Ipv4Addr::UNSPECIFIED, None)),
    }
    .inspect_err(|error| {
        error!(event_kind = "backend_preparation_failed", error_kind = "listener_address", listener, address = %address, error = %error, "backend preparation failed");
    })?;
    let resolved = std::net::SocketAddr::from((ip, address.port()));
    if address.host().is_some_and(SiteNodeHost::is_name) {
        info!(event_kind = "listener_address_resolved", listener, address = %address, resolved = %resolved, "listener host name resolved");
    }
    if let Some(warning) = loopback_warning {
        warn!(event_kind = "listener_address_loopback", listener, address = %address, "{warning}");
    }
    Ok(resolved)
}

/// Explains a listener address that does not resolve, naming the listener and its address.
fn listener_address_failure(
    address: &BindAddress,
    listener: &'static str,
    source: dotcore_domain::HostResolutionError,
) -> super::CompositionError {
    super::CompositionError::Composition(
        anyhow::Error::new(source)
            .context(format!("resolve the {listener} listener address {address}")),
    )
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use dotcore_domain::test_support::host_resolver::FakeHostResolver;

    use super::*;

    #[tokio::test]
    async fn default_manager_host_name_resolving_to_loopback_is_rejected() {
        let resolver = FakeHostResolver::default().with("gb10-host", [Ipv4Addr::new(127, 0, 1, 1)]);
        let address = BindAddress::parse("gb10-host:50053").expect("address");

        let error = resolve_manager_private_address(
            &address,
            &ManagerPrivateListener::SiteDefault { port: 50053 },
            &resolver,
        )
        .await
        .expect_err("loopback default");

        let super::super::CompositionError::Composition(source) = error else {
            panic!("expected a composition failure, got {error:?}");
        };
        assert_eq!(
            source.to_string(),
            "the manager private listener defaults to the site manager host gb10-host:50053, which \
             resolves to the loopback address 127.0.1.1 on this machine, so joined nodes cannot \
             reach it; declare a manager host they reach, or set DOTCORE_PRIVATE_ADDR to a local \
             address"
        );
    }

    #[tokio::test]
    async fn default_manager_host_name_that_does_not_resolve_is_an_unresolvable_node_host() {
        let resolver = FakeHostResolver::default();
        let address = BindAddress::parse("gpu1.example.ts.net:50053").expect("address");

        let error = resolve_manager_private_address(
            &address,
            &ManagerPrivateListener::SiteDefault { port: 50053 },
            &resolver,
        )
        .await
        .expect_err("unresolvable default");

        assert!(
            matches!(
                &error,
                super::super::CompositionError::UnresolvableNodeHost { host, .. }
                    if host.to_string() == "gpu1.example.ts.net"
            ),
            "expected an unresolvable node host, got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "cannot resolve the node host gpu1.example.ts.net; start the network that provides \
             it and start the server again, or reset the site with another node host"
        );
    }

    #[tokio::test]
    async fn configured_host_name_that_does_not_resolve_names_the_listener_address() {
        let resolver = FakeHostResolver::default();
        let address = BindAddress::parse("gpu1.example.ts.net:50053").expect("address");

        let error = resolve_manager_private_address(
            &address,
            &ManagerPrivateListener::Configured(address.clone()),
            &resolver,
        )
        .await
        .expect_err("unresolvable configured address");

        let super::super::CompositionError::Composition(source) = error else {
            panic!("expected a composition failure, got {error:?}");
        };
        assert_eq!(
            source.to_string(),
            "resolve the private listener address gpu1.example.ts.net:50053"
        );
    }

    #[tokio::test]
    async fn configured_host_name_resolving_to_loopback_is_kept() {
        let resolver = FakeHostResolver::default().with("gb10-host", [Ipv4Addr::new(127, 0, 1, 1)]);
        let address = BindAddress::parse("gb10-host:50053").expect("address");

        let resolved = resolve_manager_private_address(
            &address,
            &ManagerPrivateListener::Configured(address.clone()),
            &resolver,
        )
        .await
        .expect("configured address");

        assert_eq!(resolved, SocketAddr::from(([127, 0, 1, 1], 50053)));
    }

    #[tokio::test]
    async fn default_manager_loopback_address_is_kept_for_single_host_sites() {
        let resolver = FakeHostResolver::default();
        let address = BindAddress::parse("127.0.0.1:50053").expect("address");

        let resolved = resolve_manager_private_address(
            &address,
            &ManagerPrivateListener::SiteDefault { port: 50053 },
            &resolver,
        )
        .await
        .expect("loopback address");

        assert_eq!(resolved, SocketAddr::from(([127, 0, 0, 1], 50053)));
        assert_eq!(resolver.lookups(), Vec::<String>::new());
    }

    #[test]
    fn default_private_bind_failure_names_the_override() {
        let address = BindAddress::parse("203.0.113.7:50053").expect("address");
        let error = crate::server::ServerError::Listener(crate::server::ListenerError::Private {
            address: SocketAddr::from(([203, 0, 113, 7], 50053)),
            source: std::io::Error::from(std::io::ErrorKind::AddrNotAvailable),
        });

        let error = manager_private_bind_failure(
            error,
            Some(&address),
            &ManagerPrivateListener::SiteDefault { port: 50053 },
        );

        let super::super::CompositionError::Composition(source) = error else {
            panic!("expected a composition failure, got {error:?}");
        };
        assert_eq!(
            source.to_string(),
            "bind the manager private listener at its default 203.0.113.7:50053, the site manager \
             host that joined nodes dial; set DOTCORE_PRIVATE_ADDR to a local or wildcard address, \
             for example 0.0.0.0:50053"
        );
    }

    #[test]
    fn configured_private_bind_failure_is_unchanged() {
        let address = BindAddress::parse("203.0.113.7:50053").expect("address");
        let error = crate::server::ServerError::Listener(crate::server::ListenerError::Private {
            address: SocketAddr::from(([203, 0, 113, 7], 50053)),
            source: std::io::Error::from(std::io::ErrorKind::AddrNotAvailable),
        });

        let error = manager_private_bind_failure(
            error,
            Some(&address),
            &ManagerPrivateListener::Configured(address.clone()),
        );

        assert!(
            matches!(
                error,
                super::super::CompositionError::Server(crate::server::ServerError::Listener(
                    crate::server::ListenerError::Private { .. }
                ))
            ),
            "{error:?}"
        );
    }
}
