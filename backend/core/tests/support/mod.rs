//! Isolated backend test fixture and environment helpers.
#![allow(
    dead_code,
    reason = "each integration-test crate includes this shared module but uses a different helper subset"
)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    ops::Deref,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use dotcore::{
    composition::{IsolatedApplicationService, IsolatedModelService, SelectedModelService},
    model::engine::declaration_validator,
    testing::{SimulatedInferenceTestSupport, Timing, relax_sync_for_tests},
};
use dotcore_domain::{
    APPLE_GPU_DEVICE_ID, ExecutionPlatform, SiteNodeHost,
    clock::SystemClock,
    hardware_observation::HardwareSnapshotSource,
    test_support::ports::{ReservedPort, reserve_port},
};
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::{
    dotcore::{
        catalog::v1::{
            EngineParameterValue, EngineParameterValues, OciImageReference, engine_parameter_value,
        },
        inference::v1::{ListRoutesRequest, route_service_client::RouteServiceClient},
        model::v1::{
            CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
            HiddenDeploymentExposure, ListModelDeploymentsRequest, ModelDeploymentExposure,
            ModelDeploymentStatus, create_model_deployment_request, model_deployment_exposure,
            model_deployment_service_client::ModelDeploymentServiceClient,
            model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        },
        site::{
            store::v1::{NodeCurrentState, SiteCurrentState},
            v1::{
                HardwareObservationConfiguration, ListHardwareSnapshotsRequest,
                ListMachinesRequest, NodeObservationState, Site,
                site_service_client::SiteServiceClient,
            },
        },
    },
    yaml,
};
use dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport;
use dotcore_site::{
    initial::{
        input_store::InitialSiteInputWriter,
        inputs::{
            ApplicationTargetChoice, InitialSiteInputs, ModelTargetChoice, NodeTargetChoices,
        },
        probe::InitialSiteProbeReport,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
            RandomInitialSiteIdentityGenerator, derive_model_execution_target_id,
        },
        validation::SiteInputBundleValidator,
    },
    manager_trust::SITE_TRUST_DIRECTORY,
    membership::{
        OfflineSite,
        join::{
            DialBack, JoinApproval, JoinConnection, JoinDecision, JoinHost, JoinHostLimits,
            JoinHostSettings, JoinProposal, JoinReview, JoinToken, JoinedNodeFolder, NodeKey,
            RejoinProposal, RejoiningNodeFolder,
        },
    },
    node::{
        folder::{
            NODE_CERTIFICATE_FILE, NODE_PRIVATE_KEY_FILE, NodeFolderValidator, TRUST_ANCHOR_FILE,
            ValidatedNodeFolder,
        },
        target_inventory::NodeAcceleratorObservation,
    },
    store::FileCurrentState,
};
use prost::Message as _;
use tokio::sync::{oneshot, watch};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};

pub mod node_control;
pub mod node_liveness;
pub mod process_protocol;

pub use process_protocol::ProcessCommand;
use process_protocol::{CommandChannel, PROCESS_SETTINGS, ProcessSettings};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

const DEVELOPMENT_RUNTIME_SCENARIO: &str =
    "defaultScenario:\n        readyDelayMillis: '2000'\n        stopDelayMillis: '2000'";
pub const FAST_RUNTIME_SCENARIO: &str =
    "defaultScenario:\n        readyDelayMillis: 0\n        stopDelayMillis: 0";
pub const DIAGNOSTIC_DEADLOCK_WATCHDOG: Duration = Duration::from_secs(10);
/// Failure deadline for waits on convergence a test already triggered. It is generous so a loaded
/// machine makes a test slow rather than failing; it never decides the behavior under test.
pub const CONVERGENCE_DEADLINE: Duration = Duration::from_secs(30);

pub fn oci_reference(reference: &OciImageReference) -> Option<String> {
    let repository = reference.repository.as_ref()?;
    let mut rendered = String::new();
    if let Some(registry) = repository.registry.as_ref() {
        rendered.push_str(&registry.host);
        if let Some(port) = registry.port {
            rendered.push(':');
            rendered.push_str(&port.to_string());
        }
        rendered.push('/');
    }
    rendered.push_str(&repository.name);
    if let Some(tag) = reference.tag.as_ref() {
        rendered.push(':');
        rendered.push_str(tag);
    }
    if let Some(digest) = reference.digest.as_ref() {
        rendered.push('@');
        rendered.push_str(&digest.algorithm);
        rendered.push(':');
        rendered.push_str(&digest.encoded);
    }
    (reference.tag.is_some() || reference.digest.is_some()).then_some(rendered)
}

pub fn local_model_configuration(
    binding_id: &str,
    total_context: u64,
) -> dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
    dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
        model_artifact_binding_id: binding_id.to_owned(),
        engine_parameter_values: Some(EngineParameterValues {
            values: [
                ("total-context", total_context),
                ("prompt-cache-memory-mib", 0),
                ("parallel-requests", 4),
            ]
            .into_iter()
            .map(|(parameter_id, value)| EngineParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
            })
            .collect(),
        }),
        custom_argument_tokens: Vec::new(),
        option_overrides: Vec::new(),
    }
}

/// Observes this host's NVIDIA facts the way the backend does, so an expected snapshot carries
/// the same memory architecture and memory-pool total as a live deployment on this host.
pub fn observed_node_nvidia(managed_node_id: &str) -> NodeAcceleratorObservation {
    use dotcore_domain::hardware_observation::HardwareSnapshotSource;
    use dotcore_node_hardware::hardware::{
        SystemHardwareSnapshotCollector, SystemNvidiaAcceleratorAdapter,
    };

    let snapshot = SystemHardwareSnapshotCollector::for_current_host(
        vec![Box::new(SystemNvidiaAcceleratorAdapter::for_current_host())],
        Arc::new(SystemClock),
    )
    .collect_snapshot();
    NodeAcceleratorObservation::from_hardware_snapshot(managed_node_id, &snapshot, false)
}

/// Reports whether this host observes an NVIDIA driver, printing the skip reason when it does not;
/// a simulated model target admits the fixture model only on such a host.
pub fn nvidia_driver_observed() -> bool {
    const PROBE_NODE_ID: &str = "nvidia-driver-probe";

    let observed = observed_node_nvidia(PROBE_NODE_ID)
        != NodeAcceleratorObservation::unavailable(PROBE_NODE_ID);
    if !observed {
        eprintln!(
            "SKIPPED: this host has no NVIDIA driver observation, which the fixture model target \
             needs"
        );
    }
    observed
}

pub fn hidden_model_exposure() -> ModelDeploymentExposure {
    ModelDeploymentExposure {
        kind: Some(model_deployment_exposure::Kind::Hidden(
            HiddenDeploymentExposure {},
        )),
    }
}

pub fn exact_placement(
    site_node_id: &str,
    execution_target_id: &str,
) -> dotcore_proto::dotcore::deployment::v1::ExactPlacement {
    dotcore_proto::dotcore::deployment::v1::ExactPlacement {
        site_node_id: site_node_id.to_owned(),
        execution_target_id: execution_target_id.to_owned(),
    }
}

pub fn configuration(root: &Path) -> dotcore_domain::SitePaths {
    relax_file_sync();
    dotcore_domain::SitePaths::from_values(
        Some(root.to_str().expect("fixture path")),
        // Master catalog wiring: every harness site runs against the master catalog, with
        // application images its simulated nodes realize without a development source.
        Some(
            simulated_master_catalog()
                .to_str()
                .expect("master catalog path"),
        ),
        Some(root.join("cache").to_str().expect("cache path")),
    )
    .expect("configuration")
}

/// Composes the isolated application service of the site at `configuration`, after any earlier
/// composition of the same site released its stores.
pub async fn application_service_for(
    configuration: &dotcore_domain::SitePaths,
) -> dotcore::composition::CompositionResult<IsolatedApplicationService> {
    wait_for_site_store_release(configuration).await;
    dotcore::composition::application_service_for(configuration, 50052).await
}

/// Composes the isolated model service of the site at `configuration`, after any earlier
/// composition of the same site released its stores.
pub async fn model_service_for(
    configuration: &dotcore_domain::SitePaths,
) -> dotcore::composition::CompositionResult<IsolatedModelService> {
    wait_for_site_store_release(configuration).await;
    dotcore::composition::model_service_for(configuration).await
}

/// Waits until a dropped in-process manager composition has released the site's owner store and
/// its node store. Its node's tasks stop asynchronously after their handles drop, so a restart in
/// the same process waits for them instead of failing on a store lock.
pub async fn wait_for_site_store_release(site_paths: &dotcore_domain::SitePaths) {
    relax_file_sync();
    let site_path = site_paths.root.join("site.yaml");
    let Ok(site) = dotcore_proto::yaml::from_file::<Site>(&site_path) else {
        return;
    };
    let node_lock_path = site_paths.root.join("node").join("state.pb.lock");
    tokio::time::timeout(CONVERGENCE_DEADLINE, async {
        loop {
            let owner_released = match FileCurrentState::open(&site_path, &site.site_id) {
                Err(error) => !error.to_string().contains("already locked"),
                Ok(_) => true,
            };
            if owner_released && lock_file_released(&node_lock_path) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("released site owner and node stores");
}

/// Whether no open store holds the exclusive lock at `lock_path`; a missing lock file means no
/// store was ever opened there. A probe that acquires the lock releases it when its file closes.
fn lock_file_released(lock_path: &Path) -> bool {
    use fs4::fs_std::FileExt as _;

    let Ok(file) = fs::File::open(lock_path) else {
        return true;
    };
    file.try_lock_exclusive().expect("probe node store lock")
}

pub async fn prepare_backend<F>(
    site_paths: dotcore_domain::SitePaths,
    shutdown: F,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    prepare_backend_at(
        site_paths,
        "127.0.0.1".parse().expect("loopback bind address"),
        Default::default(),
        shutdown,
    )
    .await
}

/// Prepares a manager backend listening at `bind_address`, whose own SiteNode builds Nix artifacts
/// from `dotcore_sources`.
pub async fn prepare_backend_at<F>(
    site_paths: dotcore_domain::SitePaths,
    bind_address: std::net::IpAddr,
    dotcore_sources: dotcore_domain::nix::DotcoreSources,
    shutdown: F,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    wait_for_site_store_release(&site_paths).await;
    dotcore::composition::Backend::prepare_with_test_support(
        dotcore::composition::BackendConfiguration {
            site_paths,
            telemetry_directory: None,
            dotcore_sources,
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: bind_address_at(bind_address, 0),
                    inference_bind_address: bind_address_at(bind_address, 0),
                    private_listener: test_private_listener(),
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                },
            ),
        },
        shutdown,
        dotcore::composition::BackendTestSupport::test_development(),
    )
    .await
}

/// Prepares a manager backend on loopback whose management and inference listeners bind the given
/// ports, numbers from [`reserve_port`] that the test keeps held, so it can bind them again after
/// the backend drops.
pub async fn prepare_backend_on_ports<F>(
    site_paths: dotcore_domain::SitePaths,
    management_port: u16,
    inference_port: u16,
    shutdown: F,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    wait_for_site_store_release(&site_paths).await;
    let loopback: std::net::IpAddr = std::net::Ipv4Addr::LOCALHOST.into();
    dotcore::composition::Backend::prepare_with_test_support(
        dotcore::composition::BackendConfiguration {
            site_paths,
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: bind_address_at(loopback, management_port),
                    inference_bind_address: bind_address_at(loopback, inference_port),
                    private_listener: test_private_listener(),
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                },
            ),
        },
        shutdown,
        dotcore::composition::BackendTestSupport::test_development(),
    )
    .await
}

/// Private listener of every in-process test manager: the site default on an ephemeral port, so
/// multi-node sites run the real default path and concurrent tests never share port 50053.
fn test_private_listener() -> dotcore::composition::ManagerPrivateListener {
    dotcore::composition::ManagerPrivateListener::SiteDefault { port: 0 }
}

/// Prepares a manager backend on loopback with explicit test controls.
pub async fn prepare_backend_with_test_support<F>(
    site_paths: dotcore_domain::SitePaths,
    shutdown: F,
    test_support: dotcore::composition::BackendTestSupport,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    wait_for_site_store_release(&site_paths).await;
    dotcore::composition::Backend::prepare_with_test_support(
        dotcore::composition::BackendConfiguration {
            site_paths,
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: "127.0.0.1:0"
                        .parse()
                        .expect("management bind address"),
                    inference_bind_address: "127.0.0.1:0".parse().expect("inference bind address"),
                    private_listener: test_private_listener(),
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                },
            ),
        },
        shutdown,
        test_support,
    )
    .await
}

/// Prepares a development manager backend on loopback that collects hardware from `collector`.
pub async fn prepare_backend_with_snapshot_collector<F, C>(
    site_paths: dotcore_domain::SitePaths,
    shutdown: F,
    collector: C,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    prepare_backend_with_collector_and_support(
        site_paths,
        shutdown,
        dotcore::composition::BackendTestSupport::test_development(),
        collector,
    )
    .await
}

/// Prepares a manager backend on loopback with explicit test controls that collects hardware
/// from `collector`.
pub async fn prepare_backend_with_collector_and_support<F, C>(
    site_paths: dotcore_domain::SitePaths,
    shutdown: F,
    test_support: dotcore::composition::BackendTestSupport,
    collector: C,
) -> dotcore::composition::CompositionResult<dotcore::composition::Backend>
where
    F: std::future::Future<Output = ()> + Send + 'static,
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    wait_for_site_store_release(&site_paths).await;
    dotcore::composition::Backend::prepare_with_snapshot_collector(
        dotcore::composition::BackendConfiguration {
            site_paths,
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: "127.0.0.1:0"
                        .parse()
                        .expect("management bind address"),
                    inference_bind_address: "127.0.0.1:0".parse().expect("inference bind address"),
                    private_listener: test_private_listener(),
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                },
            ),
        },
        shutdown,
        test_support,
        collector,
    )
    .await
}

/// Listener address at `host` on `port`; port 0 is an ephemeral port.
fn bind_address_at(host: std::net::IpAddr, port: u16) -> dotcore_domain::BindAddress {
    std::net::SocketAddr::new(host, port)
        .to_string()
        .parse()
        .expect("IPv4 bind address")
}

/// Runs a provisioned manager and joined-node-folder worker through real private transport.
pub struct TwoProcessObservationHarness {
    manager: ExternalBackendProcess,
    worker: ExternalBackendProcess,
    _manager_fixture: TemporaryFixture,
    worker_fixture: TemporaryFixture,
    worker_target: WorkerModelTarget,
    /// Reserved manager private address joined workers dial, kept while the manager opens no
    /// private listener because its site has no other node.
    manager_private_address: SocketAddr,
    /// Reservations of the manager's private port and of the private port the worker's node
    /// folder records, held so every restart and rejoin binds them again.
    _manager_private_port: ReservedPort,
    worker_private_port: ReservedPort,
    forwarders: Vec<LoopbackForwarder>,
}

impl TwoProcessObservationHarness {
    /// Provisions separate roots and starts the manager before its reporting worker. Returns
    /// `None`, after printing a notice, when the host cannot bind the worker's loopback address.
    pub fn start() -> Option<Self> {
        Self::start_with_worker_controls(ProcessTestControls::default())
    }

    /// Starts like [`Self::start`], with the worker process, including its restarts, running
    /// under `worker_controls`.
    pub fn start_with_worker_controls(worker_controls: ProcessTestControls) -> Option<Self> {
        Self::start_with_controls(ProcessTestControls::default(), worker_controls)
    }

    /// Starts like [`Self::start`], with each process, including its restarts, running under its
    /// own controls.
    pub fn start_with_controls(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
    ) -> Option<Self> {
        Self::start_with_worker_target(
            manager_controls,
            worker_controls,
            WorkerModelTarget::SimulatedLinux,
        )
    }

    /// Starts like [`Self::start_with_controls`], with the worker joining with `worker_target` as
    /// its model execution target, including when it rejoins.
    pub fn start_with_worker_target(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
        worker_target: WorkerModelTarget,
    ) -> Option<Self> {
        Self::start_on(
            manager_controls,
            worker_controls,
            WorkerNetwork::Loopback,
            worker_target,
            InitialApplicationRuntime::Simulated,
            None,
        )
    }

    /// Starts like [`Self::start_with_controls`], with the site declaring the worker by the host
    /// name `localhost` and the worker binding its private port through that name. The manager
    /// is declared by its IPv4 address and the token names it, so no manager host is recorded.
    pub fn start_named_worker(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
    ) -> Option<Self> {
        Self::start_on(
            manager_controls,
            worker_controls,
            WorkerNetwork::NamedHost,
            WorkerModelTarget::SimulatedLinux,
            InitialApplicationRuntime::Simulated,
            None,
        )
    }

    /// Starts like [`Self::start_with_controls`] on simulated split networks. The worker reaches
    /// the manager only at [`SPLIT_MANAGER_HOST`], and the site declares the worker at
    /// [`SPLIT_WORKER_HOST`] while it binds [`WORKER_LOOPBACK_HOST`]; forwarders carry each
    /// address to the other side's listener, including every port in `published_ports` the
    /// worker publishes applications on. Returns `None` when the addresses cannot be bound.
    pub fn start_split(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
        published_ports: &[u16],
    ) -> Option<Self> {
        Self::start_on(
            manager_controls,
            worker_controls,
            WorkerNetwork::Split(published_ports),
            WorkerModelTarget::SimulatedLinux,
            InitialApplicationRuntime::Simulated,
            None,
        )
    }

    /// Starts like [`Self::start_with_controls`], with the worker joining with a Docker
    /// application target on this machine's Docker daemon, so its applications publish on its
    /// declared host. Models stay simulated.
    pub fn start_docker_applications(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
    ) -> Option<Self> {
        Self::start_on(
            manager_controls,
            worker_controls,
            WorkerNetwork::Loopback,
            WorkerModelTarget::SimulatedLinux,
            InitialApplicationRuntime::Docker,
            None,
        )
    }

    /// Starts like [`Self::start`], with the manager's own node declaring the named
    /// `manager_machine`, beside a worker on the declared machine of its simulated Linux target.
    pub fn start_with_manager_machine(manager_machine: &str) -> Option<Self> {
        Self::start_on(
            ProcessTestControls::default(),
            ProcessTestControls::default(),
            WorkerNetwork::Loopback,
            WorkerModelTarget::SimulatedLinux,
            InitialApplicationRuntime::Simulated,
            Some(manager_machine),
        )
    }

    fn start_on(
        manager_controls: ProcessTestControls,
        worker_controls: ProcessTestControls,
        network: WorkerNetwork,
        worker_target: WorkerModelTarget,
        applications: InitialApplicationRuntime,
        manager_machine: Option<&str>,
    ) -> Option<Self> {
        let addresses: &[Ipv4Addr] = match network {
            WorkerNetwork::Split(_) => {
                &[WORKER_LOOPBACK_HOST, SPLIT_MANAGER_HOST, SPLIT_WORKER_HOST]
            }
            WorkerNetwork::Loopback => &[WORKER_LOOPBACK_HOST],
            WorkerNetwork::NamedHost => &[],
        };
        if !loopback_addresses_available(addresses) {
            return None;
        }

        let manager_private_port = reserve_port();
        let worker_private_port = reserve_port();
        let manager_private_address =
            SocketAddr::from((Ipv4Addr::LOCALHOST, manager_private_port.port()));
        let (manager_fixture, worker_fixture) = temporary_observation_fixtures(
            manager_private_address,
            worker_private_port.port(),
            network,
            worker_target,
            applications,
        );
        if let Some(machine) = manager_machine {
            declare_fixture_machine(&manager_fixture, &[machine]);
        }

        // The manager starts without DOTCORE_PRIVATE_ADDR, as a packaged manager does: its
        // declared host is this loopback address, so the site default binds the reservation.
        let mut manager = ExternalBackendProcess::spawn(
            manager_fixture.path.clone(),
            ProcessPrivateListener::ManagerSiteDefault(manager_private_address.port()),
            manager_controls,
        );
        manager.capture_startup();
        // A named worker binds the `privateBindAddress` its node folder recorded.
        let (worker_private_listener, forwarders) = match network {
            WorkerNetwork::Split(published_ports) => {
                let (address, forwarders) = split_forwarders(
                    manager_private_address,
                    worker_private_port.port(),
                    published_ports,
                );
                (ProcessPrivateListener::Address(address), forwarders)
            }
            WorkerNetwork::Loopback => (
                ProcessPrivateListener::Address(SocketAddr::from((WORKER_LOOPBACK_HOST, 0))),
                Vec::new(),
            ),
            WorkerNetwork::NamedHost => (ProcessPrivateListener::Unset, Vec::new()),
        };
        let mut worker = ExternalBackendProcess::spawn(
            worker_fixture.path.clone(),
            worker_private_listener,
            worker_controls,
        );
        worker.capture_startup();

        Some(Self {
            manager,
            worker,
            _manager_fixture: manager_fixture,
            worker_fixture,
            worker_target,
            manager_private_address,
            _manager_private_port: manager_private_port,
            worker_private_port,
            forwarders,
        })
    }

    /// Returns the manager process and its authoritative site root.
    pub fn manager(&self) -> &ExternalBackendProcess {
        &self.manager
    }

    /// Returns mutable manager lifecycle control.
    pub fn manager_mut(&mut self) -> &mut ExternalBackendProcess {
        &mut self.manager
    }

    /// Returns the network path the worker reaches the manager's private listener through. Only a
    /// harness started on split networks has one.
    pub fn manager_path(&self) -> &LoopbackForwarder {
        self.forwarders
            .first()
            .expect("a split-network harness routes the worker through a manager path")
    }

    /// Expires the stopped or paused worker's contact lease and serving targets on the manager's
    /// manual clock, instead of waiting for them in real time.
    ///
    /// Two advances of half the node silence each wait until every other node has just renewed,
    /// so only the silent worker expires. The manager must run with `manual_clock`.
    pub async fn expire_silent_worker(&mut self) {
        let half_silence = self.manager.controls.timing.node_control.node_silence / 2;
        self.advance_past_silent_worker(half_silence).await;
        self.advance_past_silent_worker(half_silence + Duration::from_millis(1))
            .await;
    }

    /// Advances the manager's manual clock `by` once the worker has fallen silent and every
    /// other node has just renewed.
    pub async fn advance_past_silent_worker(&mut self, by: Duration) {
        self.manager
            .command(ProcessCommand::AdvanceClock {
                by,
                expiring: vec![WORKER_NODE_ID.to_owned()],
            })
            .await
            .expect("advance the manager's manual clock past the silent worker");
    }

    /// Returns the worker process and its joined node folder root.
    pub fn worker(&self) -> &ExternalBackendProcess {
        &self.worker
    }

    /// Returns mutable worker lifecycle control.
    pub fn worker_mut(&mut self) -> &mut ExternalBackendProcess {
        &mut self.worker
    }

    /// Returns the manager's current site configuration.
    pub fn manager_site(&self) -> Site {
        yaml::from_str(
            &fs::read_to_string(self.manager.site_root().join("site.yaml"))
                .expect("manager site.yaml"),
        )
        .expect("manager site")
    }

    /// Loads the validated node folder used by the worker process.
    pub fn node_folder(&self) -> ValidatedNodeFolder {
        NodeFolderValidator::validate_declared(&self.worker_fixture.path)
            .expect("validated observation node folder")
    }

    /// Opens an authenticated worker-to-manager channel, on which a test opens node control
    /// streams as the worker's SiteNode.
    pub async fn worker_control_channel(&self) -> Channel {
        let manager_address = self
            .manager
            .addresses()
            .private
            .expect("manager private address");
        private_tls_endpoint(manager_address, &self.worker_fixture.path)
            .connect()
            .await
            .expect("worker control channel")
    }

    /// Replaces the stopped worker with a freshly joined node folder of the same name and host,
    /// and starts its process. The manager must be stopped, since the join takes the site lock;
    /// restart it before the new worker can connect. The old node folder is deleted.
    pub fn rejoin_worker(&mut self) -> JoinedNodeFolder {
        assert!(
            !self.manager.is_running() && !self.worker.is_running(),
            "rejoining needs a stopped manager and a stopped worker"
        );
        let manager_private_address = self.manager_private_address;
        let fixture = temporary_uncreated_fixture("dotcore-observation-worker");
        let joined = join_worker_with_target(
            self.manager.site_root(),
            &format!("https://{manager_private_address}"),
            &fixture.path,
            WORKER_DISPLAY_NAME,
            (
                SiteNodeHost::from_ipv4(WORKER_LOOPBACK_HOST).expect("specific worker host"),
                self.worker_private_port.port(),
            ),
            None,
            (self.worker_target, InitialApplicationRuntime::Simulated),
        );
        let mut worker = ExternalBackendProcess::spawn(
            fixture.path.clone(),
            ProcessPrivateListener::Address(SocketAddr::from((WORKER_LOOPBACK_HOST, 0))),
            self.worker.controls,
        );
        worker.capture_startup();
        self.worker = worker;
        self.worker_fixture = fixture;
        joined
    }
}

impl TwoProcessObservationHarness {
    /// Joins the stopped worker's intact node folder to the stopped manager's site again at
    /// `host`, keeping the folder, and starts its process on the folder's recorded bind address.
    /// Restart the manager before the node can connect.
    pub fn rejoin_worker_in_place(&mut self, host: SiteNodeHost) -> JoinedNodeFolder {
        assert!(
            !self.manager.is_running() && !self.worker.is_running(),
            "rejoining needs a stopped manager and a stopped worker"
        );
        let manager_private_address = self.manager_private_address;
        let joined = rejoin_worker_folder(
            self.manager.site_root(),
            &format!("https://{manager_private_address}"),
            self.worker.site_root(),
            host,
        );
        let mut worker = ExternalBackendProcess::spawn(
            self.worker.site_root().to_owned(),
            ProcessPrivateListener::Unset,
            self.worker.controls,
        );
        worker.capture_startup();
        self.worker = worker;
        joined
    }
}

/// Reports whether the addresses of a worker rejoining at [`SPLIT_WORKER_HOST`] after joining at
/// [`WORKER_LOOPBACK_HOST`] can be bound, printing the setup command when macOS lacks an alias.
pub fn rejoin_loopback_addresses_available() -> bool {
    loopback_addresses_available(&[WORKER_LOOPBACK_HOST, SPLIT_WORKER_HOST])
}

/// Model execution target a harness worker proposes when it joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerModelTarget {
    /// Simulated, on a declared RTX 5090 desktop, whose models run on linux/amd64 whatever this
    /// machine's architecture.
    SimulatedLinux,

    /// Simulated, on a declared Mac, so it admits Apple-accelerated catalog configurations on a
    /// Mac without running engines.
    SimulatedDarwin,

    /// Native runtime on the Apple GPU of a Mac, with persistent workload lifetime.
    AppleNative,
}

impl WorkerModelTarget {
    /// Returns the ID the join derives for this model target of [`WORKER_NODE_ID`].
    pub fn execution_target_id(self) -> String {
        let target = self.proposal();
        derive_model_execution_target_id(WORKER_NODE_ID, target.inference(), target.device())
    }

    /// Returns the simulated hardware the worker declares, when it simulates its models.
    pub fn simulated_hardware(self) -> Option<&'static [&'static str]> {
        match self {
            Self::SimulatedLinux => Some(&["rtx-5090"]),
            Self::SimulatedDarwin => Some(&["mac-64g"]),
            Self::AppleNative => None,
        }
    }

    /// Returns the model target the join proposes: simulated models on the first GPU of the
    /// declared machine, or the Mac's Apple GPU.
    fn proposal(self) -> ModelTargetChoice {
        match self.simulated_hardware() {
            Some(choices) => {
                let machine = simulated_machine(choices);
                ModelTargetChoice::Simulated {
                    platform: machine.platform().clone(),
                    device_id: machine
                        .gpu_ids()
                        .next()
                        .expect("the declared machine has a GPU")
                        .to_owned(),
                }
            }
            None => ModelTargetChoice::Apple {
                device_id: APPLE_GPU_DEVICE_ID.to_owned(),
                lifetime: dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime::Persistent,
            },
        }
    }
}

/// How the harness's worker is declared and reached.
#[derive(Clone, Copy)]
enum WorkerNetwork<'a> {
    /// Declared at [`WORKER_LOOPBACK_HOST`] on the manager's network.
    Loopback,
    /// Declared by the host name [`WORKER_HOST_NAME`] on the manager's network.
    NamedHost,
    /// Declared on a separate network behind forwarders, with these extra published ports.
    Split(&'a [u16]),
}

impl Drop for TwoProcessObservationHarness {
    fn drop(&mut self) {
        self.manager.stop();
        self.worker.stop();
    }
}

/// Deadline for waits that end only once a silent worker's liveness expires: the manager's fixed
/// 10 s liveness window plus a margin for its last contact and sampling.
pub const LIVENESS_EXPIRY_DEADLINE: Duration = Duration::from_secs(20);
const RETAINED_STATE_DEADLINE: Duration = CONVERGENCE_DEADLINE;
const RETAINED_STATE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Waits until the manager projects the observation harness worker as Connected.
pub async fn wait_for_worker_connection(manager_address: SocketAddr) {
    wait_for_worker_observation(manager_address, NodeObservationState::Connected).await;
}

/// Waits for the observation harness worker to reach `expected`, allowing its liveness to expire.
/// The connection and polling are bounded, with the last observed freshness reported on timeout.
pub async fn wait_for_worker_observation(
    manager_address: SocketAddr,
    expected: NodeObservationState,
) {
    let mut last_observed = None;
    tokio::time::timeout(LIVENESS_EXPIRY_DEADLINE, async {
        let mut site = SiteServiceClient::connect(format!("http://{manager_address}"))
            .await
            .expect("manager site API connection");
        loop {
            let machines = site
                .list_machines(ListMachinesRequest {})
                .await
                .expect("manager machine inventory")
                .into_inner()
                .machines;
            last_observed = machines
                .into_iter()
                .find(|machine| machine.machine_id == WORKER_NODE_ID)
                .and_then(|machine| machine.observation_freshness);
            if last_observed
                .as_ref()
                .is_some_and(|freshness| freshness.state == expected as i32)
            {
                break;
            }
            tokio::time::sleep(RETAINED_STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "worker {WORKER_NODE_ID} observation deadline waiting for {expected:?}; \
             last observed freshness: {last_observed:?}"
        )
    });
}

const MODEL_READY_DEADLINE: Duration = CONVERGENCE_DEADLINE;

/// Creates one simulated deployment of the fixture model directly exposed as `route` and
/// returns its id.
pub async fn place_model(
    manager_address: SocketAddr,
    site_node_id: &str,
    execution_target_id: &str,
    route: &str,
) -> String {
    ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection")
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "fixture-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(exact_placement(site_node_id, execution_target_id)),
                    configuration: Some(local_model_configuration("fixture-artifact", 32_768)),
                },
            )),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: route.to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("manager accepts the deployment")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id
}

/// Waits until the manager lists the deployment as Ready.
pub async fn wait_for_ready(manager_address: SocketAddr, deployment_id: &str) {
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    tokio::time::timeout(MODEL_READY_DEADLINE, async {
        loop {
            let ready = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model list")
                .into_inner()
                .deployments
                .iter()
                .any(|deployment| {
                    deployment.model_deployment_id == deployment_id
                        && deployment.state.as_ref().is_some_and(|state| {
                            state.status == ModelDeploymentStatus::Ready as i32
                        })
                });
            if ready {
                return;
            }
            tokio::time::sleep(RETAINED_STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("deployment Ready deadline");
}

/// Returns the sorted ids of the manager's model deployments.
pub async fn listed_deployments(manager_address: SocketAddr) -> Vec<String> {
    let mut ids = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection")
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("manager model list")
        .into_inner()
        .deployments
        .into_iter()
        .map(|deployment| deployment.model_deployment_id)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Returns the manager's route names, sorted by the API.
pub async fn listed_routes(manager_address: SocketAddr) -> Vec<String> {
    RouteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager route API connection")
        .list_routes(ListRoutesRequest {})
        .await
        .expect("manager route list")
        .into_inner()
        .routes
        .into_iter()
        .map(|view| view.route.expect("route declaration").name)
        .collect()
}

/// Waits until the manager lists `expected` as the hardware of node `node_id`, comparing
/// everything but the observation time, and reports the last listed hardware on timeout.
pub async fn wait_for_listed_hardware(
    manager_address: SocketAddr,
    node_id: &str,
    expected: dotcore_proto::dotcore::observation::v1::HardwareSnapshot,
) {
    let expected = dotcore_proto::dotcore::observation::v1::HardwareSnapshot {
        observed_at: None,
        ..expected
    };
    let mut site = SiteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager site API connection");
    let mut last = None;
    let listed = tokio::time::timeout(CONVERGENCE_DEADLINE, async {
        loop {
            let snapshot = site
                .list_hardware_snapshots(ListHardwareSnapshotsRequest {})
                .await
                .expect("manager hardware list")
                .into_inner()
                .machines
                .into_iter()
                .find(|machine| machine.machine_id == node_id)
                .and_then(|machine| machine.snapshot)
                .map(
                    |snapshot| dotcore_proto::dotcore::observation::v1::HardwareSnapshot {
                        observed_at: None,
                        ..snapshot
                    },
                );
            if snapshot.as_ref() == Some(&expected) {
                return;
            }
            last = snapshot;
            tokio::time::sleep(RETAINED_STATE_POLL_INTERVAL).await;
        }
    })
    .await;
    assert!(
        listed.is_ok(),
        "node {node_id} hardware never matched; expected {expected:?}, last listed {last:?}"
    );
}

/// Returns the sorted ids of the machines the manager lists.
pub async fn machine_ids(manager_address: SocketAddr) -> Vec<String> {
    machine_states(manager_address)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// Returns each machine the manager lists with its observation state, sorted by id.
pub async fn machine_states(
    manager_address: SocketAddr,
) -> Vec<(String, Option<NodeObservationState>)> {
    let mut machines = SiteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager site API connection")
        .list_machines(ListMachinesRequest {})
        .await
        .expect("manager machine inventory")
        .into_inner()
        .machines
        .into_iter()
        .map(|machine| {
            let state = machine
                .observation_freshness
                .and_then(|freshness| NodeObservationState::try_from(freshness.state).ok());
            (machine.machine_id, state)
        })
        .collect::<Vec<_>>();
    machines.sort();
    machines
}

/// Sends one chat completion through `route` on the manager's inference listener and returns the
/// status.
pub async fn chat_status(
    harness: &TwoProcessObservationHarness,
    route: &str,
) -> reqwest::StatusCode {
    let address = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": route,
            "messages": [{"role": "user", "content": "hello"}],
        }))
        .send()
        .await
        .expect("inference response")
        .status()
}

/// Waits until a process has written its retained current state, which a worker does once its
/// realizers have bootstrapped.
pub async fn wait_for_node_realizer_bootstrap(state_path: PathBuf) {
    tokio::time::timeout(RETAINED_STATE_DEADLINE, async {
        loop {
            if fs::read(&state_path)
                .ok()
                .and_then(|bytes| NodeCurrentState::decode(bytes.as_slice()).ok())
                .is_some()
            {
                return;
            }
            tokio::time::sleep(RETAINED_STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker realizer bootstrap deadline");
}

/// Decodes the retained owner current state at `path`.
pub fn retained_state(path: PathBuf) -> SiteCurrentState {
    SiteCurrentState::decode(fs::read(path).expect("retained current state").as_slice())
        .expect("decode retained current state")
}

/// Decodes the retained node state at `path`.
pub fn retained_node_state(path: PathBuf) -> NodeCurrentState {
    NodeCurrentState::decode(fs::read(path).expect("retained node state").as_slice())
        .expect("decode retained node state")
}

/// Counts the complete desired views a node installed, as recorded in its retained site logs.
pub fn installed_views(site_root: &Path) -> usize {
    retained_json_records(site_root)
        .iter()
        .filter(|record| record["fields"]["event_kind"] == "node_desired_view_installed")
        .count()
}

/// Returns every structured record a process retained in its site logs, oldest first. A record
/// the process is still writing, after a file's last line break, is left for a later read.
pub fn retained_json_records(site_root: &Path) -> Vec<serde_json::Value> {
    retained_log_files(site_root)
        .iter()
        .flat_map(|bytes| complete_json_records(bytes))
        .collect()
}

/// Counts the node control stream opens a worker retained as failed with an error containing
/// `failure`, such as a manager refusal or a connection failure.
pub fn refused_stream_opens(worker_root: &Path, failure: &str) -> usize {
    failed_stream_open_errors(worker_root)
        .iter()
        .filter(|error| error.contains(failure))
        .count()
}

/// Waits until the worker under `worker_root` retains at least `count` stream opens failed with
/// `failure`, proving its process tried the manager that many times and was turned away. A
/// manager refusal also proves the manager received and rejected those opens.
pub async fn wait_for_refused_stream_opens(worker_root: &Path, failure: &str, count: usize) {
    const REFUSAL_DEADLINE: Duration = Duration::from_secs(15);
    const REFUSAL_POLL_INTERVAL: Duration = Duration::from_millis(25);

    let mut refused = 0;
    let observed = tokio::time::timeout(REFUSAL_DEADLINE, async {
        loop {
            refused = refused_stream_opens(worker_root, failure);
            if refused >= count {
                return;
            }
            tokio::time::sleep(REFUSAL_POLL_INTERVAL).await;
        }
    })
    .await;
    if observed.is_err() {
        let errors = failed_stream_open_errors(worker_root);
        panic!(
            "refused worker stream open deadline waiting for {count} {failure:?} failed opens, \
             last {refused}; failed open errors: {:?}",
            &errors[errors.len().saturating_sub(3)..]
        );
    }
}

/// Errors of every node control stream open a worker retained as failed, oldest first.
fn failed_stream_open_errors(worker_root: &Path) -> Vec<String> {
    retained_json_records(worker_root)
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "node_control_stream_open"
                && record["fields"]["outcome"] == "failed"
        })
        .filter_map(|record| record["fields"]["error"].as_str().map(str::to_owned))
        .collect()
}

/// Returns the newest 64 KiB of a process's retained site logs, for failure messages.
pub fn retained_log_text(site_root: &Path) -> String {
    const FAILURE_LOG_LIMIT_BYTES: usize = 64 * 1024;

    let mut bytes = retained_log_bytes(site_root);
    if bytes.len() > FAILURE_LOG_LIMIT_BYTES {
        bytes.drain(..bytes.len() - FAILURE_LOG_LIMIT_BYTES);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn retained_log_bytes(site_root: &Path) -> Vec<u8> {
    retained_log_files(site_root).concat()
}

/// The contents of each retained log file, in file name order.
fn retained_log_files(site_root: &Path) -> Vec<Vec<u8>> {
    let mut paths = fs::read_dir(site_root.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| fs::read(path).expect("retained backend log"))
        .collect()
}

/// Parses the complete lines of one JSONL file; a complete line that isn't JSON panics with it.
fn complete_json_records(bytes: &[u8]) -> Vec<serde_json::Value> {
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[][..], |end| &bytes[..end]);
    complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).unwrap_or_else(|error| {
                panic!(
                    "retained JSONL record {:?}: {error}",
                    String::from_utf8_lossy(line)
                )
            })
        })
        .collect()
}

/// Test-only controls the harness passes to one backend subprocess when it starts.
#[derive(Clone, Copy, Debug)]
pub struct ProcessTestControls {
    /// Holds every simulated inference attempt after admission until it is cancelled; a streaming
    /// attempt first sends this many events.
    pub held_inference_stream_events: Option<usize>,

    /// Development-source Nix engine pin, as (package, store path), that a worker records instead
    /// of evaluating its development source.
    pub local_development_store_path: Option<(&'static str, &'static str)>,

    /// Site catalog declarations, as (path below `catalog/`, YAML), written before every start.
    pub site_catalog: &'static [(&'static str, &'static str)],

    /// Port the simulated runtime assigns as every application group's endpoint, on the
    /// publication host of the process; the test serves readiness there.
    pub application_endpoint_port: Option<u16>,

    /// Runs native workloads on this host: the process keeps the host variables that Nix
    /// discovery, evaluation, and builds need, including `DOTCORE_DEV_SOURCE_FLAKE`, and starts
    /// workloads with the `dotcore-launcher` of this Cargo profile.
    pub native_host: bool,

    /// Prepares model artifacts in the host's `DOTCORE_CACHE_DIR`, when set, instead of a cache
    /// inside the site root, so real model weights download once per host.
    pub shared_artifact_cache: bool,

    /// Holds simulated model preparation after its first download activity and workload readiness
    /// while loading, until the test releases each with a [`ProcessCommand`].
    pub model_transition_gates: bool,

    /// Runs a manager on a manual clock the test advances with
    /// [`ProcessCommand::AdvanceClock`], instead of system time.
    pub manual_clock: bool,

    /// Every behavior duration of the process; the test timing unless the test adjusts it.
    pub timing: Timing,
}

impl ProcessTestControls {
    /// Controls of a manager whose clock the test advances.
    pub fn manual_clock() -> Self {
        Self {
            manual_clock: true,
            ..Self::default()
        }
    }
}

impl Default for ProcessTestControls {
    fn default() -> Self {
        Self {
            held_inference_stream_events: None,
            local_development_store_path: None,
            site_catalog: &[],
            application_endpoint_port: None,
            native_host: false,
            shared_artifact_cache: false,
            model_transition_gates: false,
            manual_clock: false,
            timing: Timing::test(),
        }
    }
}

/// One external backend process with captured listener addresses and an isolated site root.
pub struct ExternalBackendProcess {
    site_root: PathBuf,
    private_listener: ProcessPrivateListener,
    controls: ProcessTestControls,
    child: Option<Child>,
    commands: Option<CommandChannel>,
    lines: Option<mpsc::Receiver<String>>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
    addresses: Option<dotcore::server::ServerAddresses>,
}

impl ExternalBackendProcess {
    /// Starts one backend process for the site at `site_root` and waits for its listener
    /// addresses.
    pub fn start(site_root: PathBuf, controls: ProcessTestControls) -> Self {
        let mut process = Self::spawn(site_root, ProcessPrivateListener::Unset, controls);
        process.capture_startup();
        process
    }

    /// Returns the exact addresses reported after listener preparation.
    pub fn addresses(&self) -> dotcore::server::ServerAddresses {
        self.addresses.expect("external backend startup addresses")
    }

    /// Returns this process's exclusive site root.
    pub fn site_root(&self) -> &Path {
        &self.site_root
    }

    /// Returns this process's server-local node binding path.
    pub fn node_binding_path(&self) -> PathBuf {
        self.site_root.join("node.yaml")
    }

    /// Returns this process's owner current-state file path.
    pub fn state_path(&self) -> PathBuf {
        self.site_root.join("state.pb")
    }

    /// Returns this process's node-store state file path.
    pub fn node_state_path(&self) -> PathBuf {
        self.site_root.join("node/state.pb")
    }

    /// Reports whether the owned child process remains active.
    pub fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().expect("external backend status").is_none())
    }

    /// Suspends the owned process while preserving its operating-system identity.
    pub fn pause(&mut self) {
        self.signal(libc::SIGSTOP, "pause external backend");
    }

    /// Resumes a previously suspended owned process.
    pub fn resume(&mut self) {
        self.signal(libc::SIGCONT, "resume external backend");
    }

    /// Sends `command` to the running process and waits for its acknowledgment; fails with the
    /// process's refusal or when it does not answer in time.
    pub async fn command(&mut self, command: ProcessCommand) -> Result<(), String> {
        let lines = self.lines.as_ref().expect("external backend stdout lines");
        self.commands
            .as_mut()
            .expect("external backend command channel")
            .send(command, lines)
            .await
    }

    /// Requests graceful process shutdown so retained logs are drained before exit. Kills the
    /// process and fails when it has not exited within the shutdown deadline.
    pub fn shutdown(&mut self) {
        const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

        if self.child.is_none() {
            return;
        }
        // Closing standard input first ends the process's command reader.
        self.commands.take();
        self.signal(libc::SIGTERM, "shutdown external backend");
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        while self.is_running() {
            if Instant::now() >= deadline {
                self.stop();
                panic!("external backend did not exit within {SHUTDOWN_DEADLINE:?} of SIGTERM");
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.stop();
    }

    /// Stops this process without affecting its peer or deleting its site.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.commands.take();
        if child.try_wait().expect("external backend status").is_none() {
            child.kill().expect("stop external backend");
        }
        child.wait().expect("join external backend");
        self.lines.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("external backend stdout reader");
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("external backend stderr reader");
        }
    }

    /// Restarts this process against the same isolated site and captures new addresses.
    pub fn restart(&mut self) {
        self.stop();
        let mut replacement =
            Self::spawn(self.site_root.clone(), self.private_listener, self.controls);
        replacement.capture_startup();
        *self = replacement;
    }

    fn spawn(
        site_root: PathBuf,
        private_listener: ProcessPrivateListener,
        controls: ProcessTestControls,
    ) -> Self {
        let cache_dir = controls
            .shared_artifact_cache
            .then(|| std::env::var_os("DOTCORE_CACHE_DIR"))
            .flatten()
            .map_or_else(|| site_root.join("cache"), PathBuf::from);
        let mut command =
            Command::new(std::env::current_exe().expect("integration-test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "support::tests::external_backend_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env(EXTERNAL_BACKEND_PROCESS, "1")
            .env("DOTCORE_SITE_DIR", &site_root)
            .env("DOTCORE_CACHE_DIR", cache_dir)
            // A target prefix, so the events of every backend crate (`dotcore`,
            // `dotcore_site`, ...) reach the retained logs the tests read.
            .env("RUST_LOG", "dotcore=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A joined node starts from its folder alone; a manager also needs its listeners and catalog.
        if !is_joined_node_folder(&site_root) {
            // Master catalog wiring: spawned managers run against the master catalog, with
            // application images an image-free simulated node needs no development source for.
            command
                .env("DOTCORE_CATALOG_DIR", simulated_master_catalog())
                .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
                .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
                .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173");
        }
        let mut manager_private_port = None;
        match private_listener {
            ProcessPrivateListener::Unset => {}
            ProcessPrivateListener::ManagerSiteDefault(port) => manager_private_port = Some(port),
            ProcessPrivateListener::Address(address) => {
                command.env("DOTCORE_PRIVATE_ADDR", address.to_string());
            }
        }
        let native_launcher = controls.native_host.then(profile_launcher_binary);
        command.env(
            PROCESS_SETTINGS,
            ProcessSettings::new(&controls, native_launcher, manager_private_port).encode(),
        );
        for (path, contents) in controls.site_catalog {
            let path = site_root.join("catalog").join(path);
            fs::create_dir_all(path.parent().expect("Site catalog directory"))
                .expect("Site catalog directory");
            fs::write(path, contents).expect("Site catalog declaration");
        }
        if let Some(certificates) = std::env::var_os("SSL_CERT_FILE") {
            command.env("SSL_CERT_FILE", certificates);
        }
        if controls.native_host {
            command.envs(std::env::vars_os().filter(|(name, _)| is_native_host_variable(name)));
        }
        let mut child = command.spawn().expect("start external backend");
        let commands = CommandChannel::new(child.stdin.take().expect("external backend stdin"));
        let stdout = child.stdout.take().expect("external backend stdout");
        let (sender, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender
                    .send(line.expect("external backend stdout line"))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Forwarded through `eprintln!` so libtest captures the child's logs per test: silent on
        // success, shown on failure, and live under `--nocapture`.
        let stderr = child.stderr.take().expect("external backend stderr");
        let stderr_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("{line}");
            }
        });

        Self {
            site_root,
            private_listener,
            controls,
            child: Some(child),
            commands: Some(commands),
            lines: Some(lines),
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            addresses: None,
        }
    }

    fn signal(&mut self, signal: libc::c_int, context: &str) {
        let child = self.child.as_ref().expect("external backend child");
        // The harness owns this live child PID for the complete signal operation.
        let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
        assert_eq!(result, 0, "{context}: {}", std::io::Error::last_os_error());
    }

    fn capture_startup(&mut self) {
        self.try_capture_startup()
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn try_capture_startup(&mut self) -> Result<(), String> {
        // A native-host process collects real host facts before listening, which can take
        // tens of seconds on a busy Mac.
        let timeout = if self.controls.native_host {
            Duration::from_secs(90)
        } else {
            Duration::from_secs(10)
        };
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("external backend child")
                .try_wait()
                .expect("external backend status")
            {
                return Err(format!(
                    "external backend exited before startup with {status}; stdout: {output:?}"
                ));
            }
            if let Ok(line) = self
                .lines
                .as_ref()
                .expect("external backend stdout lines")
                .recv_timeout(Duration::from_millis(25))
            {
                output.push(line);
                if let Some(addresses) = startup_addresses(&output, self.expects_private_listener())
                {
                    self.addresses = Some(addresses);
                    return Ok(());
                }
            }
        }
        Err(format!(
            "external backend did not start within {timeout:?}; stdout: {output:?}"
        ))
    }
}

impl ExternalBackendProcess {
    /// Reports whether this process announces a private listener at startup: a joined node
    /// always does, a manager with an explicit address too, and a manager on its site default only
    /// while its site declares other nodes.
    fn expects_private_listener(&self) -> bool {
        match self.private_listener {
            ProcessPrivateListener::Address(_) => true,
            ProcessPrivateListener::ManagerSiteDefault(_) => {
                site_declares_other_nodes(&self.site_root)
            }
            ProcessPrivateListener::Unset => is_joined_node_folder(&self.site_root),
        }
    }
}

impl Drop for ExternalBackendProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Private listener setting the harness gives one external backend process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessPrivateListener {
    /// No setting: a joined node binds the address its folder recorded, a manager the site
    /// default port.
    Unset,

    /// A manager's site default on this reserved port, carried in the process settings.
    ManagerSiteDefault(u16),

    /// An explicit `DOTCORE_PRIVATE_ADDR`.
    Address(SocketAddr),
}

/// Runs the real composition root for an isolated subprocess launched by the external harness.
pub async fn run_external_backend_process() {
    relax_file_sync();
    assert_eq!(std::env::var(EXTERNAL_BACKEND_PROCESS).as_deref(), Ok("1"));
    dotcore::logging::initialize();
    let mut configuration = dotcore::composition::BackendConfiguration::from_environment()
        .expect("external backend configuration");
    let settings = ProcessSettings::from_environment();
    if let (Some(port), dotcore::composition::BackendRole::Manager(server)) =
        (settings.manager_private_port(), &mut configuration.role)
    {
        server.private_listener =
            dotcore::composition::ManagerPrivateListener::SiteDefault { port };
    }
    let manual_clock = settings.manual_clock();
    let test_support = settings.test_support(manual_clock.clone());
    let simulated_runtime = test_support.simulated_runtime.clone();
    let backend = dotcore::composition::Backend::prepare_with_test_support(
        configuration,
        std::future::pending(),
        test_support,
    )
    .await
    .expect("prepare external backend");
    let addresses = backend.addresses();
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout).expect("separate startup protocol from the test runner");
    writeln!(stdout, "Dotcore server starting").expect("startup heading");
    if let Some(address) = addresses.management {
        writeln!(stdout, "DOTCORE_MANAGEMENT_ADDRESS=http://{address}")
            .expect("management startup address");
    }
    if let Some(address) = addresses.inference {
        writeln!(stdout, "DOTCORE_INFERENCE_ADDRESS=http://{address}")
            .expect("inference startup address");
    }
    if let Some(address) = addresses.private {
        writeln!(stdout, "DOTCORE_PRIVATE_ADDRESS=https://{address}")
            .expect("private startup address");
    }
    stdout.flush().expect("flush startup protocol");
    drop(stdout);
    // The liveness view is taken before serving consumes the backend.
    let manager_clock = manual_clock.map(|clock| {
        let liveness = backend
            .node_liveness()
            .expect("only a site manager runs on a manual clock");
        node_liveness::ManagerClock::new(liveness, settings.timing().node_control, clock)
    });
    settings.serve_commands(simulated_runtime, manager_clock);
    backend.serve().await.expect("serve external backend");
}

/// Display name of the observation fixture's worker.
pub const WORKER_DISPLAY_NAME: &str = "Worker one";
/// SiteNode ID the join derives for [`WORKER_DISPLAY_NAME`] in the fixture site.
pub const WORKER_NODE_ID: &str = "node-3113e65ad218f556";
/// Model execution target ID the join derives for [`WORKER_NODE_ID`] on the GPU of its declared
/// RTX 5090 desktop.
pub const WORKER_MODEL_TARGET_ID: &str = "execution-target-2fd15a6d3bb72208-simulated-4d747ac0";
/// Application execution target ID the join derives for [`WORKER_NODE_ID`].
pub const WORKER_APPLICATION_TARGET_ID: &str =
    "application-execution-target-2fd15a6d3bb72208-simulated";
/// Application execution target ID the join derives for [`WORKER_NODE_ID`] when it joins with a
/// Docker application runtime.
pub const WORKER_DOCKER_APPLICATION_TARGET_ID: &str =
    "application-execution-target-2fd15a6d3bb72208-docker";

/// Host name the site declares for a named-host worker; it resolves to the loopback address the
/// manager's own declared host also names, so it needs no extra address.
pub const WORKER_HOST_NAME: &str = "localhost";
/// Loopback address of the worker process; distinct from the manager's because worker
/// provisioning requires a unique host per SiteNode.
pub const WORKER_LOOPBACK_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
/// Address a split-network worker reaches the manager at, forwarded to the manager's listeners.
pub const SPLIT_MANAGER_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);
/// Address the site declares for a split-network worker, forwarded to [`WORKER_LOOPBACK_HOST`].
pub const SPLIT_WORKER_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 4);
const EXTERNAL_BACKEND_PROCESS: &str = "DOTCORE_EXTERNAL_BACKEND_PROCESS";
/// Host variables a native-host subprocess keeps, besides every `NIX_` one: `nix` lookup on
/// `PATH`, its user cache and configuration under `HOME`, its temporary files, and the
/// development source that `self` engine artifacts build from.
const NATIVE_HOST_VARIABLES: [&str; 5] =
    ["PATH", "HOME", "USER", "TMPDIR", "DOTCORE_DEV_SOURCE_FLAKE"];

/// Reports whether a native-host subprocess keeps the host variable `name`.
fn is_native_host_variable(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| NATIVE_HOST_VARIABLES.contains(&name) || name.starts_with("NIX_"))
}

/// Returns the `dotcore-launcher` binary of the current Cargo profile, which a workspace build
/// places beside the `deps` directory holding the test executable.
fn profile_launcher_binary() -> PathBuf {
    let executable = std::env::current_exe().expect("integration-test executable");
    let launcher = executable
        .parent()
        .and_then(Path::parent)
        .expect("test executable inside a Cargo profile directory")
        .join("dotcore-launcher");
    assert!(
        launcher.is_file(),
        "{} is missing; build it with `just backend::build-launcher`",
        launcher.display()
    );
    launcher
}

fn startup_addresses(
    output: &[String],
    expects_private: bool,
) -> Option<dotcore::server::ServerAddresses> {
    let heading = output
        .iter()
        .position(|line| line == "Dotcore server starting")?;
    let lines = &output[heading + 1..];
    // A node serving only its private channel reports just that address.
    if let Some(private) = lines
        .first()
        .and_then(|line| marker_address(line, "DOTCORE_PRIVATE_ADDRESS="))
    {
        return Some(dotcore::server::ServerAddresses {
            management: None,
            inference: None,
            private: Some(private),
        });
    }
    let width = if expects_private { 3 } else { 2 };
    if lines.len() < width {
        return None;
    }
    Some(dotcore::server::ServerAddresses {
        management: Some(marker_address(&lines[0], "DOTCORE_MANAGEMENT_ADDRESS=")?),
        inference: Some(marker_address(&lines[1], "DOTCORE_INFERENCE_ADDRESS=")?),
        private: if expects_private {
            Some(marker_address(&lines[2], "DOTCORE_PRIVATE_ADDRESS=")?)
        } else {
            None
        },
    })
}

/// Reports whether `root` holds a joined node's folder rather than a manager's site.
fn is_joined_node_folder(root: &Path) -> bool {
    dotcore::composition::node_role(root) == dotcore::composition::NodeRole::JoinedNode
}

/// Reports whether the manager site at `root` declares a SiteNode besides the manager, the
/// condition for its site-default private listener.
fn site_declares_other_nodes(root: &Path) -> bool {
    dotcore_proto::yaml::from_file::<Site>(&root.join("site.yaml"))
        .expect("manager site")
        .nodes
        .iter()
        .any(|node| node.site_manager.is_none())
}

fn marker_address(line: &str, marker: &str) -> Option<SocketAddr> {
    line.strip_prefix(marker)?.split_once("://")?.1.parse().ok()
}

/// Consumes one whole HTTP request, headers and `Content-Length` body, from a fixture server.
/// Replying and closing before the request is fully read makes the kernel reset the connection,
/// which discards the client's pending response on macOS.
pub async fn read_http_request(stream: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt as _;

    let mut request = Vec::new();
    let mut scratch = [0_u8; 1024];
    loop {
        let Ok(read @ 1..) = stream.read(&mut scratch).await else {
            return;
        };
        request.extend_from_slice(&scratch[..read]);
        let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = headers_end + 4;
        let content_length = String::from_utf8_lossy(&request[..headers_end])
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() - body_start >= content_length {
            return;
        }
    }
}

/// Reports whether the worker's distinct loopback address can be bound, printing the setup
/// command when macOS lacks the `lo0` alias. Any other bind failure panics so it is never hidden.
pub fn worker_loopback_address_available() -> bool {
    loopback_addresses_available(&[WORKER_LOOPBACK_HOST])
}

/// Reports whether every loopback address can be bound, printing the setup command for the first
/// missing one, as on macOS without its `lo0` alias. Any other bind failure panics.
fn loopback_addresses_available(addresses: &[Ipv4Addr]) -> bool {
    addresses
        .iter()
        .all(|address| match TcpListener::bind((*address, 0)) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => {
                eprintln!(
                    "SKIPPED: {address} is not bound on this host ({error}); run \
                     `sudo ifconfig lo0 alias {address}` to enable two-process worker tests"
                );
                false
            }
            Err(error) => panic!("probe loopback address {address}: {error}"),
        })
}

/// Starts the forwarders of a split-network pair and returns the worker's private bind address.
/// The worker reaches the manager's private listener through [`SPLIT_MANAGER_HOST`], and the
/// manager reaches the worker's private listener and published ports through
/// [`SPLIT_WORKER_HOST`]; each forwarder connects from the address its peer side would show.
fn split_forwarders(
    manager_private_address: SocketAddr,
    worker_private_port: u16,
    published_ports: &[u16],
) -> (SocketAddr, Vec<LoopbackForwarder>) {
    let worker_private_address = SocketAddr::from((WORKER_LOOPBACK_HOST, worker_private_port));
    let mut forwarders = vec![LoopbackForwarder::start(
        SocketAddr::from((SPLIT_MANAGER_HOST, manager_private_address.port())),
        SPLIT_WORKER_HOST,
        manager_private_address,
    )];
    for port in
        std::iter::once(worker_private_address.port()).chain(published_ports.iter().copied())
    {
        forwarders.push(LoopbackForwarder::start(
            SocketAddr::from((SPLIT_WORKER_HOST, port)),
            SPLIT_MANAGER_HOST,
            SocketAddr::from((WORKER_LOOPBACK_HOST, port)),
        ));
    }
    (worker_private_address, forwarders)
}

/// A TCP forwarder standing for the network path between two simulated networks: it accepts at
/// one address and connects to the target from `source`, so the target sees `source` as the
/// peer. It runs on its own thread and runtime, and stops when dropped.
///
/// The path can go silent, like a host that vanished without closing its connections: every
/// connection stays open but carries no more bytes, including ones accepted while silent.
pub struct LoopbackForwarder {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    silent: watch::Sender<bool>,
    held: Arc<AtomicUsize>,
}

impl LoopbackForwarder {
    /// Binds `listen` before returning, so connections made afterwards are forwarded.
    pub fn start(listen: SocketAddr, source: Ipv4Addr, target: SocketAddr) -> Self {
        let listener = TcpListener::bind(listen)
            .unwrap_or_else(|error| panic!("bind forwarder at {listen}: {error}"));
        listener
            .set_nonblocking(true)
            .expect("non-blocking forwarder listener");
        let (stop, stopped) = oneshot::channel();
        let (silent, silenced) = watch::channel(false);
        let held = Arc::new(AtomicUsize::new(0));
        let held_by_path = Arc::clone(&held);
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("forwarder runtime")
                .block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(listener).expect("forwarder listener");
                    let accept = async {
                        while let Ok((inbound, _)) = listener.accept().await {
                            if *silenced.borrow() {
                                held_by_path.fetch_add(1, Ordering::SeqCst);
                                tokio::spawn(hold(inbound));
                            } else {
                                tokio::spawn(forward(inbound, source, target, silenced.clone()));
                            }
                        }
                    };
                    tokio::select! {
                        _ = stopped => {}
                        () = accept => {}
                    }
                });
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
            silent,
            held,
        }
    }

    /// Silences the path, or carries new connections again once `silent` is cleared. Connections
    /// silenced earlier stay silent, as connections to a vanished host never recover.
    pub fn set_silent(&self, silent: bool) {
        self.silent.send_replace(silent);
    }

    /// Number of connections accepted while the path was silent.
    pub fn held_connections(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }
}

impl Drop for LoopbackForwarder {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("forwarder thread");
        }
    }
}

/// Copies one connection both ways until either side closes it, or holds it once the path goes
/// silent.
async fn forward(
    mut inbound: tokio::net::TcpStream,
    source: Ipv4Addr,
    target: SocketAddr,
    mut silenced: watch::Receiver<bool>,
) {
    let Ok(socket) = tokio::net::TcpSocket::new_v4() else {
        return;
    };
    if socket.bind(SocketAddr::from((source, 0))).is_err() {
        return;
    }
    let Ok(mut outbound) = socket.connect(target).await else {
        return;
    };
    let went_silent = tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => false,
        silenced = silenced.wait_for(|silent| *silent) => silenced.is_ok(),
    };
    if went_silent {
        // Both ends stay open and unread until the forwarder stops.
        std::future::pending::<()>().await;
    }
}

/// Holds one accepted connection open without reading or answering it until the forwarder stops.
async fn hold(_inbound: tokio::net::TcpStream) {
    std::future::pending::<()>().await;
}

fn temporary_observation_fixtures(
    manager_private_address: SocketAddr,
    worker_private_port: u16,
    network: WorkerNetwork,
    worker_target: WorkerModelTarget,
    applications: InitialApplicationRuntime,
) -> (TemporaryFixture, TemporaryFixture) {
    let manager = temporary_fast_fixture();
    configure_observation_interval(&manager, 1);
    install_manager_trust(&manager);
    let worker = temporary_uncreated_fixture("dotcore-observation-worker");
    let (host, manager_host) = match network {
        WorkerNetwork::Split(_) => (SPLIT_WORKER_HOST.to_string(), Some(SPLIT_MANAGER_HOST)),
        WorkerNetwork::Loopback => (WORKER_LOOPBACK_HOST.to_string(), None),
        WorkerNetwork::NamedHost => (WORKER_HOST_NAME.to_owned(), None),
    };
    let joined = join_worker_with_target(
        &manager,
        &format!("https://{manager_private_address}"),
        &worker.path,
        WORKER_DISPLAY_NAME,
        (host.parse().expect("worker host"), worker_private_port),
        manager_host,
        (worker_target, applications),
    );
    assert_eq!(joined.node_id, WORKER_NODE_ID);
    (manager, worker)
}

/// Joins one simulated node to the stopped site at `manager_root` through the join library, host
/// and client in this process over loopback, and installs its node folder at `node_root`, which
/// must not exist. The node proposes `host` (an address or a name) explicitly and records
/// `private_port` as its private port, where it listens while it joins so the manager can dial it
/// back; the port must come from [`reserve_port`], held while it may be bound. With
/// `manager_host`, the node dials the join listener at that address through a forwarder, as a
/// node on another network would, and keeps that manager address; `host` must then be an IPv4
/// address, which the forwarder connects from.
pub fn join_worker(
    manager_root: &Path,
    manager_private_endpoint: &str,
    node_root: &Path,
    display_name: &str,
    host: SiteNodeHost,
    private_port: u16,
    manager_host: Option<Ipv4Addr>,
) -> JoinedNodeFolder {
    join_worker_with_target(
        manager_root,
        manager_private_endpoint,
        node_root,
        display_name,
        (host, private_port),
        manager_host,
        (
            WorkerModelTarget::SimulatedLinux,
            InitialApplicationRuntime::Simulated,
        ),
    )
}

/// Joins like [`join_worker`], with the node proposing the given model execution target and
/// application runtime. Its application target is on the linux platform of this machine's
/// architecture.
pub fn join_worker_with_target(
    manager_root: &Path,
    manager_private_endpoint: &str,
    node_root: &Path,
    display_name: &str,
    (host, private_port): (SiteNodeHost, u16),
    manager_host: Option<Ipv4Addr>,
    (target, applications): (WorkerModelTarget, InitialApplicationRuntime),
) -> JoinedNodeFolder {
    let manager_root = manager_root.to_owned();
    let manager_private_endpoint = manager_private_endpoint.to_owned();
    let node_root = node_root.to_owned();
    let display_name = display_name.to_owned();
    let forwarder = manager_host.map(|manager_host| {
        (
            manager_host,
            host.ipv4()
                .expect("a forwarded join needs an IPv4 node host"),
        )
    });
    let model_target = target.proposal();
    let manager_host =
        manager_host.map(|host| SiteNodeHost::from_ipv4(host).expect("specific manager host"));
    host_join_session(
        &manager_root,
        &manager_private_endpoint,
        forwarder,
        manager_host.clone(),
        move |mut connection, _| async move {
            connection
                .join(
                    &JoinProposal {
                        display_name,
                        host: Some(host),
                        private_port,
                        targets: NodeTargetChoices {
                            model_targets: vec![model_target],
                            application_targets: vec![ApplicationTargetChoice {
                                applications,
                                platform: linux_host_platform(),
                                accelerator_device_ids: Vec::new(),
                            }],
                            accelerator_facts: Vec::new(),
                            declared_machine: target.simulated_hardware().map(declared_machine),
                        },
                    },
                    &NodeKey::generate().expect("node key"),
                    &node_root,
                )
                .await
                .expect("join the site")
        },
    )
}

/// Joins the machine again from its intact node folder at `node_root`, through the join library
/// like [`join_worker`]: host and client in this process over loopback, on the stopped site at
/// `manager_root`. The node proposes `host` (an address or a name) explicitly and keeps its
/// folder's private port. The folder's custody lock is held for the rejoin, so its node must be
/// stopped.
pub fn rejoin_worker_folder(
    manager_root: &Path,
    manager_private_endpoint: &str,
    node_root: &Path,
    host: SiteNodeHost,
) -> JoinedNodeFolder {
    let node_root = node_root.to_owned();
    host_join_session(
        manager_root,
        manager_private_endpoint,
        None,
        None,
        move |mut connection, token| async move {
            let folder =
                RejoiningNodeFolder::open(&node_root, &token).expect("open the intact node folder");
            let private_port = folder.private_port();
            connection
                .rejoin(
                    &RejoinProposal {
                        host: Some(host),
                        private_port,
                    },
                    &folder,
                )
                .await
                .expect("rejoin the site")
        },
    )
}

/// Hosts one join session of the stopped site at `manager_root` in this process, on a dedicated
/// thread and runtime so it is callable from sync and async tests alike, and runs `client`
/// against a connection to it and the session's token. With `forwarder` as (manager host, node source address), the node
/// dials the join listener at the manager host through a forwarder, as a node on another network
/// would.
fn host_join_session<F, Fut>(
    manager_root: &Path,
    manager_private_endpoint: &str,
    forwarder: Option<(Ipv4Addr, Ipv4Addr)>,
    manager_host: Option<SiteNodeHost>,
    client: F,
) -> JoinedNodeFolder
where
    F: FnOnce(JoinConnection, JoinToken) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = JoinedNodeFolder>,
{
    let manager_root = manager_root.to_owned();
    let manager_private_endpoint = manager_private_endpoint.to_owned();
    thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("join runtime")
            .block_on(async move {
                let site = OfflineSite::open(
                    &manager_root,
                    &SiteInputBundleValidator::new(
                        declaration_validator(),
                        dotcore::model::site_declaration_rules(),
                    ),
                )
                .expect("open manager site");
                let identity = site.host_identity().expect("manager join identity");
                // The forwarder binds the same number on another address, so it must come from the
                // reserved pool, which no ephemeral socket can hold.
                let join_port = reserve_port();
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", join_port.port()))
                    .await
                    .expect("bind join listener");
                let token = identity
                    .token(
                        format!("https://{}", listener.local_addr().expect("join address"))
                            .parse()
                            .expect("join endpoint"),
                    )
                    .expect("join token");
                let join_address = listener.local_addr().expect("join address");
                let _forwarder = forwarder.map(|(manager_host, source)| {
                    LoopbackForwarder::start(
                        SocketAddr::from((manager_host, join_address.port())),
                        source,
                        join_address,
                    )
                });
                let host_task = tokio::spawn(
                    JoinHost::new(
                        JoinHostSettings {
                            identity,
                            token: token.clone(),
                            manager_private_port: manager_private_port(&manager_private_endpoint),
                            limits: JoinHostLimits::default(),
                        },
                        std::sync::Arc::new(site),
                        std::sync::Arc::new(AllowEveryNode),
                        Timing::test().join,
                    )
                    .serve(listener),
                );
                let connection =
                    JoinConnection::connect(token.clone(), manager_host, Timing::test().join)
                        .await
                        .expect("connect to join host");
                let joined = client(connection, token).await;
                host_task
                    .await
                    .expect("join host task")
                    .expect("join host result");
                drop(join_port);
                joined
            })
    })
    .join()
    .expect("join thread")
}

fn manager_private_port(endpoint: &str) -> u16 {
    endpoint
        .parse::<url::Url>()
        .expect("manager private endpoint")
        .port_or_known_default()
        .expect("manager private port")
}

/// Returns the linux platform of this machine's architecture.
fn linux_host_platform() -> ExecutionPlatform {
    ExecutionPlatform::normalize("linux", std::env::consts::ARCH)
        .expect("this machine's architecture has a supported execution platform")
}

/// Allows every node that presents the token.
struct AllowEveryNode;

impl JoinApproval for AllowEveryNode {
    fn dialing_back(&self, _: &SiteNodeHost, _: u16) {}

    fn dialed_back(&self, _: &DialBack) {}

    fn review(
        &self,
        _: JoinReview,
        _: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'static, JoinDecision> {
        Box::pin(async { JoinDecision::Allow })
    }
}

/// Sets how often the site under `root` samples node hardware, so tests waiting for a fresh
/// sample do not wait the default interval.
pub fn configure_observation_interval(root: &Path, interval_seconds: u32) {
    let path = root.join("site.yaml");
    let mut site: Site = yaml::from_str(&fs::read_to_string(&path).expect("read observation site"))
        .expect("parse observation site");
    site.hardware_observation = Some(HardwareObservationConfiguration {
        refresh_interval_seconds: Some(interval_seconds),
    });
    fs::write(
        path,
        yaml::to_string(&site).expect("serialize observation site"),
    )
    .expect("write observation site");
}

/// Installs site trust for the fixture's `fixture-node` manager, as site initialization does, so
/// the manager can serve its private listener.
pub fn install_manager_trust(root: &Path) {
    let request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    )
    .with_site_id_override("fixture-site")
    .with_node_id_override("fixture-node");
    let identities = request
        .resolve_identities(&mut RandomInitialSiteIdentityGenerator)
        .expect("manager fixture identities");
    let inputs =
        InitialSiteInputs::compose(&request, identities, &InitialSiteProbeReport::default())
            .expect("manager fixture inputs");
    let initialized = TemporaryFixture {
        path: root.join("initialized-manager"),
    };
    InitialSiteInputWriter::new(SiteInputBundleValidator::new(
        declaration_validator(),
        dotcore::model::site_declaration_rules(),
    ))
    .create(&initialized.path, &inputs)
    .expect("initialize manager fixture trust");
    fs::rename(
        initialized.path.join(SITE_TRUST_DIRECTORY),
        root.join(SITE_TRUST_DIRECTORY),
    )
    .expect("install manager fixture trust");
}

fn temporary_uncreated_fixture(prefix: &str) -> TemporaryFixture {
    TemporaryFixture {
        path: canonical_temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )),
    }
}

/// Builds an mTLS client endpoint presenting the node identity under `root`'s trust directory.
/// It verifies the peer as the backend does, by the site chain and server-auth usage and never by
/// the dialed address, since node leaves name only their SiteNode identity.
pub fn private_tls_endpoint(address: SocketAddr, root: &Path) -> Endpoint {
    let trust = root.join(SITE_TRUST_DIRECTORY);
    let verifier = Arc::new(SiteChainVerifier::new(
        &fs::read(trust.join(TRUST_ANCHOR_FILE)).expect("node trust anchor"),
    ));
    Endpoint::from_shared(format!("https://{address}"))
        .expect("private endpoint")
        .timeout(Duration::from_secs(2))
        .tls_config_with_verifier(
            ClientTlsConfig::new()
                .identity(Identity::from_pem(
                    pem(
                        "CERTIFICATE",
                        &fs::read(trust.join(NODE_CERTIFICATE_FILE)).expect("node certificate"),
                    ),
                    pem(
                        "PRIVATE KEY",
                        &fs::read(trust.join(NODE_PRIVATE_KEY_FILE)).expect("node private key"),
                    ),
                ))
                .domain_name(address.ip().to_string()),
            verifier,
        )
        .expect("private TLS configuration")
}

/// Accepts a server leaf that chains to the site trust anchor for server authentication, without
/// matching any name. Tests use it to reach a private listener as a site node; the backend's own
/// clients additionally pin the exact leaf.
#[derive(Debug)]
struct SiteChainVerifier {
    roots: rustls::RootCertStore,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl SiteChainVerifier {
    fn new(trust_anchor_der: &[u8]) -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                trust_anchor_der.to_vec(),
            ))
            .expect("Site trust anchor");
        Self {
            roots,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for SiteChainVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &rustls::server::ParsedCertificate::try_from(end_entity)?,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let body = encoded
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 PEM chunk"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
}

pub struct RunningBackend {
    channel: tonic::transport::Channel,
    management_address: std::net::SocketAddr,
    inference_address: std::net::SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<dotcore::composition::CompositionResult<()>>,
}

impl RunningBackend {
    pub async fn start(site_paths: dotcore_domain::SitePaths) -> Self {
        Self::start_with_simulated_runtime_test_support(
            site_paths,
            SimulatedRuntimeTestSupport::with_stop_failures(0),
        )
        .await
    }

    pub async fn start_with_simulated_runtime_test_support(
        site_paths: dotcore_domain::SitePaths,
        simulated_runtime_test_support: SimulatedRuntimeTestSupport,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            "127.0.0.1".parse().expect("loopback bind address"),
            Some(simulated_runtime_test_support),
            None,
            None,
            Default::default(),
            0,
        )
        .await
    }

    /// Starts a backend like `start_with_simulated_runtime_test_support` whose inference listener
    /// binds `inference_port`, a number from [`reserve_port`] that the test keeps held, so it can
    /// bind that number again after the backend stops.
    pub async fn start_on_inference_port(
        site_paths: dotcore_domain::SitePaths,
        simulated_runtime_test_support: SimulatedRuntimeTestSupport,
        inference_port: u16,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            "127.0.0.1".parse().expect("loopback bind address"),
            Some(simulated_runtime_test_support),
            None,
            None,
            Default::default(),
            inference_port,
        )
        .await
    }

    pub async fn start_with_simulated_test_supports(
        site_paths: dotcore_domain::SitePaths,
        simulated_runtime_test_support: SimulatedRuntimeTestSupport,
        simulated_inference_test_support: SimulatedInferenceTestSupport,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            "127.0.0.1".parse().expect("loopback bind address"),
            Some(simulated_runtime_test_support),
            Some(simulated_inference_test_support),
            None,
            Default::default(),
            0,
        )
        .await
    }

    /// Starts a real-runtime backend whose own SiteNode builds Nix artifacts from
    /// `dotcore_sources`.
    pub async fn start_at(
        site_paths: dotcore_domain::SitePaths,
        bind_address: std::net::IpAddr,
        dotcore_sources: dotcore_domain::nix::DotcoreSources,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            bind_address,
            None,
            None,
            None,
            dotcore_sources,
            0,
        )
        .await
    }

    /// Starts a real-runtime backend like `start_at` that also records workload resource
    /// telemetry to a directory.
    pub async fn start_at_recording_telemetry(
        site_paths: dotcore_domain::SitePaths,
        bind_address: std::net::IpAddr,
        telemetry_directory: std::path::PathBuf,
        dotcore_sources: dotcore_domain::nix::DotcoreSources,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            bind_address,
            None,
            None,
            Some(telemetry_directory),
            dotcore_sources,
            0,
        )
        .await
    }

    pub async fn start_at_with_simulated_runtime_test_support(
        site_paths: dotcore_domain::SitePaths,
        bind_address: std::net::IpAddr,
        simulated_runtime_test_support: SimulatedRuntimeTestSupport,
    ) -> Self {
        Self::start_at_with_test_supports(
            site_paths,
            bind_address,
            Some(simulated_runtime_test_support),
            None,
            None,
            Default::default(),
            0,
        )
        .await
    }

    /// Starts a backend under `test_support` whose node, unless it declares a machine, reads
    /// `collector` as its hardware.
    pub async fn start_with_snapshot_collector<C>(
        site_paths: dotcore_domain::SitePaths,
        test_support: dotcore::composition::BackendTestSupport,
        collector: C,
    ) -> Self
    where
        C: HardwareSnapshotSource + Send + Sync + 'static,
    {
        let (shutdown, signal) = oneshot::channel();
        wait_for_site_store_release(&site_paths).await;
        let backend = dotcore::composition::Backend::prepare_with_snapshot_collector(
            dotcore::composition::BackendConfiguration {
                site_paths,
                telemetry_directory: None,
                dotcore_sources: Default::default(),
                role: dotcore::composition::BackendRole::Manager(
                    dotcore::composition::ServerConfiguration {
                        management_bind_address: bind_address_at(
                            std::net::Ipv4Addr::LOCALHOST.into(),
                            0,
                        ),
                        inference_bind_address: bind_address_at(
                            std::net::Ipv4Addr::LOCALHOST.into(),
                            0,
                        ),
                        private_listener: test_private_listener(),
                        allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                    },
                ),
            },
            async move {
                let _ = signal.await;
            },
            test_support,
            collector,
        )
        .await
        .expect("prepared backend");
        Self::serve(backend, shutdown).await
    }

    /// Starts a simulated-runtime backend under `test_support`, such as a test's own timing and
    /// manual clock.
    pub async fn start_with_test_support(
        site_paths: dotcore_domain::SitePaths,
        test_support: dotcore::composition::BackendTestSupport,
    ) -> Self {
        Self::start_at_under(
            site_paths,
            "127.0.0.1".parse().expect("loopback bind address"),
            None,
            0,
            Default::default(),
            test_support,
        )
        .await
    }

    async fn start_at_with_test_supports(
        site_paths: dotcore_domain::SitePaths,
        bind_address: std::net::IpAddr,
        simulated_runtime_test_support: Option<SimulatedRuntimeTestSupport>,
        simulated_inference_test_support: Option<SimulatedInferenceTestSupport>,
        telemetry_directory: Option<std::path::PathBuf>,
        dotcore_sources: dotcore_domain::nix::DotcoreSources,
        inference_port: u16,
    ) -> Self {
        Self::start_at_under(
            site_paths,
            bind_address,
            telemetry_directory,
            inference_port,
            dotcore_sources,
            dotcore::composition::BackendTestSupport {
                simulated_runtime: simulated_runtime_test_support,
                simulated_inference: simulated_inference_test_support,
                ..dotcore::composition::BackendTestSupport::test()
            },
        )
        .await
    }

    async fn start_at_under(
        site_paths: dotcore_domain::SitePaths,
        bind_address: std::net::IpAddr,
        telemetry_directory: Option<std::path::PathBuf>,
        inference_port: u16,
        dotcore_sources: dotcore_domain::nix::DotcoreSources,
        test_support: dotcore::composition::BackendTestSupport,
    ) -> Self {
        let (shutdown, signal) = oneshot::channel();
        wait_for_site_store_release(&site_paths).await;
        let backend = dotcore::composition::Backend::prepare_with_test_support(
            dotcore::composition::BackendConfiguration {
                site_paths,
                telemetry_directory,
                dotcore_sources,
                role: dotcore::composition::BackendRole::Manager(
                    dotcore::composition::ServerConfiguration {
                        management_bind_address: bind_address_at(bind_address, 0),
                        inference_bind_address: bind_address_at(bind_address, inference_port),
                        private_listener: test_private_listener(),
                        allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                    },
                ),
            },
            async move {
                let _ = signal.await;
            },
            test_support,
        )
        .await
        .expect("prepared backend");
        Self::serve(backend, shutdown).await
    }

    /// Serves a prepared backend until `shutdown` fires, once its management listener accepts a
    /// connection.
    async fn serve(backend: dotcore::composition::Backend, shutdown: oneshot::Sender<()>) -> Self {
        let addresses = backend.addresses();
        let endpoint = tonic::transport::Endpoint::from_shared(format!(
            "http://{}",
            addresses.management.expect("management listener")
        ))
        .expect("management endpoint");
        let mut task = tokio::spawn(backend.serve());
        let channel = tokio::time::timeout(CONVERGENCE_DEADLINE, async {
            loop {
                tokio::select! {
                    result = &mut task => {
                        let error = result.expect("backend task").expect_err("backend must remain active");
                        panic!("backend failed before management connection: {error}");
                    }
                    result = endpoint.connect() => match result {
                        Ok(channel) => break channel,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                    },
                }
            }
        })
        .await
        .expect("management connection deadline");
        Self {
            channel,
            management_address: addresses.management.expect("management listener"),
            inference_address: addresses.inference.expect("inference listener"),
            shutdown: Some(shutdown),
            task,
        }
    }

    pub fn management_address(&self) -> std::net::SocketAddr {
        self.management_address
    }

    pub fn inference_address(&self) -> std::net::SocketAddr {
        self.inference_address
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn application_client(
        &self,
    ) -> dotcore_proto::dotcore::application::v1::application_deployment_service_client::ApplicationDeploymentServiceClient<
        tonic::transport::Channel,
    >{
        dotcore_proto::dotcore::application::v1::application_deployment_service_client::ApplicationDeploymentServiceClient::new(
            self.channel.clone(),
        )
    }

    pub fn model_client(
        &self,
    ) -> dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient<
        tonic::transport::Channel,
    >{
        dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient::new(
            self.channel.clone(),
        )
    }

    pub fn route_client(
        &self,
    ) -> dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    > {
        dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient::new(
            self.channel.clone(),
        )
    }

    pub fn site_client(&self) -> SiteServiceClient<tonic::transport::Channel> {
        SiteServiceClient::new(self.channel.clone())
    }

    pub fn metrics_client(
        &self,
    ) -> dotcore_proto::dotcore::metrics::v1::metrics_service_client::MetricsServiceClient<
        tonic::transport::Channel,
    > {
        dotcore_proto::dotcore::metrics::v1::metrics_service_client::MetricsServiceClient::new(
            self.channel.clone(),
        )
    }

    pub async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("backend shutdown sender")
            .send(())
            .expect("backend shutdown");
        tokio::time::timeout(CONVERGENCE_DEADLINE, &mut self.task)
            .await
            .expect("backend shutdown deadline")
            .expect("backend task")
            .expect("backend result");
    }
}

impl Drop for RunningBackend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct TemporaryFixture {
    path: PathBuf,
}

impl Deref for TemporaryFixture {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for TemporaryFixture {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let lock_path = PathBuf::from(format!("{}.lock", self.path.display()));
        let _ = fs::remove_file(lock_path);
    }
}

/// Hardware class the simulated fixture models run on, declared by the fixture catalog itself.
pub const FIXTURE_HARDWARE_CLASS: &str = "catalog/hardware-classes/fixture-nvidia.yaml";

pub fn temporary_fixture() -> TemporaryFixture {
    let root = canonical_temp_dir().join(format!(
        "dotcore-integration-fixture-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let source = workspace_root().join("backend/fixtures/simulated-site");
    for directory in [
        "catalog/models",
        "catalog/engines",
        "catalog/hardware-classes",
    ] {
        fs::create_dir_all(root.join(directory)).expect("fixture directory");
    }
    fs::copy(
        source.join(FIXTURE_HARDWARE_CLASS),
        root.join(FIXTURE_HARDWARE_CLASS),
    )
    .expect("hardware class fixture");
    fs::copy(source.join("site.yaml"), root.join("site.yaml")).expect("desired site fixture");
    fs::copy(source.join("node.yaml"), root.join("node.yaml")).expect("node fixture");
    fs::copy(
        source.join("catalog/models/fixture-model.yaml"),
        root.join("catalog/models/fixture-model.yaml"),
    )
    .expect("Model fixture");
    fs::copy(
        source.join("catalog/engines/fixture.yaml"),
        root.join("catalog/engines/fixture.yaml"),
    )
    .expect("Engine fixture");
    TemporaryFixture { path: root }
}

/// Simulated models with their representative startup, no ready or stop delays, and transition
/// gates; run it on [`instant_startup_timing`], so lifecycle tests wait on no simulated time.
pub fn simulated_model_transition_test_support() -> SimulatedRuntimeTestSupport {
    SimulatedRuntimeTestSupport::with_stop_failures(0)
        .with_representative_startup()
        .with_lifecycle_delays(Duration::ZERO, Duration::ZERO)
        .with_model_transition_gates()
}

/// The test timing with a simulated startup that reports its steps without pause.
pub fn instant_startup_timing() -> Timing {
    let mut timing = Timing::test();
    timing.simulated.startup_step_interval = Duration::ZERO;
    timing
}

/// Manager site catalog additions: a llama.cpp release whose darwin/arm64 Apple artifact is the
/// development source's engine package, and a small model using it.
pub const DARWIN_NIX_ENGINE_CATALOG: &[(&str, &str)] = &[
    (
        "engines/fixture-nix.yaml",
        r#"engineReleaseId: "fixture-nix-engine"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "1"
parameterSchema:
  schemaId: "fixture-nix-schema"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      exposed: true
      unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" }
    - parameterId: "context-per-request"
      displayName: "Context per request"
      exposed: true
      unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens" }
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      exposed: true
      unsignedInteger: { minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" }
    - parameterId: "prompt-cache-memory-mib"
      displayName: "Prompt cache memory"
      unsignedInteger: { minimum: "0", maximum: "65536", unit: "MiB" }
artifacts:
  - artifactId: "fixture-metal"
    nix: { flake: "self", package: "engines.llama-cpp.native", executable: "llama-server" }
    selectors:
      - platform: { operatingSystem: "darwin", architecture: "arm64" }
        accelerators: ["apple"]
"#,
    ),
    (
        "models/fixture-nix-model.yaml",
        r#"modelId: "fixture-nix-model"
displayName: "fixture Nix engine model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "fixture-artifact"
    weightQuantization: "Q8_0"
    source:
      repository: "dotcore/fixture"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture-Q8_0.gguf"
        provenanceDigest: "a05776b49d2b78f7a861ca387a96de317c90d7e20878b9167e20f44e1d2aa900"
configurations:
  - configurationId: "fixture-configuration"
    engineArtifacts: [{ engineReleaseId: "fixture-nix-engine", engineArtifactId: "fixture-metal" }]
    artifactBindings: [{ bindingId: "fixture-artifact", modelArtifactId: "fixture-artifact" }]
    hardwareSupport:
      - hardwareClassId: "apple"
        hardwareRequirements: { minimumSystemMemoryBytes: "1" }
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: { fixedSettings: { gpuLayers: 0 } }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
profiles:
  - profileId: "fixture-profile"
    displayName: "Fixture profile"
    description: "Launches the development-source Nix fixture engine."
    configurationId: "fixture-configuration"
    defaultModelArtifactBindingId: "fixture-artifact"
    engineParameterDefaults:
      values:
        - { parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }
"#,
    ),
];

/// The fixture model with fixed memory estimates, which a test writes over the fixture catalog's
/// model to watch simulated deployments take memory on a declared machine.
pub const SIZED_FIXTURE_MODEL: &str =
    include_str!("../../../fixtures/simulated-site/sized-fixture-model.yaml");

/// Accelerator memory the sized fixture model's estimate gives, 3 GiB.
pub const SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Host memory the sized fixture model's estimate gives, 256 MiB.
pub const SIZED_FIXTURE_MODEL_HOST_BYTES: u64 = 256 * 1024 * 1024;

/// Resolves simulated hardware `choices` (named GPUs, or one named machine) for this machine's CPU
/// architecture, as a node created with them declares it.
pub fn simulated_machine(choices: &[&str]) -> MachineDescription {
    let choices: Vec<_> = choices.iter().map(|choice| (*choice).to_owned()).collect();
    MachineDescription::simulated(&choices, std::env::consts::ARCH).expect("simulated hardware")
}

/// The stored declaration of simulated hardware `choices`, as a node created with them keeps it.
pub fn declared_machine(choices: &[&str]) -> dotcore_proto::dotcore::node::v1::DeclaredMachine {
    dotcore_proto::dotcore::node::v1::DeclaredMachine {
        choices: choices.iter().map(|choice| (*choice).to_owned()).collect(),
        description: Some(simulated_machine(choices).to_stored()),
    }
}

/// Declares simulated hardware `choices` for the manager node of the fixture site at `root`: its
/// node.yaml stores the resolved machine, and its simulated model targets run on the machine's
/// platform and hold its first GPU. Returns the machine's description.
pub fn declare_fixture_machine(root: &Path, choices: &[&str]) -> MachineDescription {
    let description = simulated_machine(choices);
    let node_path = root.join("node.yaml");
    let mut node: dotcore_proto::dotcore::node::v1::NodeConfiguration =
        yaml::from_file(&node_path).expect("fixture node.yaml");
    node.declared_machine = Some(declared_machine(choices));
    fs::write(&node_path, yaml::to_string(&node).expect("node.yaml")).expect("write node.yaml");

    let site_path = root.join("site.yaml");
    let mut site: Site = yaml::from_file(&site_path).expect("fixture site.yaml");
    let gpu = description
        .gpu_ids()
        .next()
        .expect("the declared machine has a GPU")
        .to_owned();
    for target in site
        .execution_targets
        .iter_mut()
        .filter(|target| target.site_node_id == node.node_id)
    {
        let Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(
            binding,
        )) = &target.runtime_binding
        else {
            continue;
        };
        if matches!(
            binding.adapter,
            Some(
                dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::SimulatedRuntime(
                    _
                )
            )
        ) {
            target.platform = Some(description.platform().to_oci_platform());
            target.accelerator_device_ids = vec![gpu.clone()];
        }
    }
    fs::write(&site_path, yaml::to_string(&site).expect("site.yaml")).expect("write site.yaml");
    description
}

pub fn temporary_fast_fixture() -> TemporaryFixture {
    let root = temporary_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    let configured = source.replace(DEVELOPMENT_RUNTIME_SCENARIO, FAST_RUNTIME_SCENARIO);
    assert_ne!(configured, source, "development runtime scenario");
    fs::write(site_path, configured).expect("fast runtime scenario");
    root
}

/// Returns the fast fixture with its manager node on its real hardware: node.yaml declares no
/// machine and the simulated model target, which needs one, is removed. Tests of the hardware
/// collection path inject their collector into such a node.
pub fn temporary_real_hardware_fixture() -> TemporaryFixture {
    let root = temporary_fast_fixture();
    let node_path = root.join("node.yaml");
    let mut node: dotcore_proto::dotcore::node::v1::NodeConfiguration =
        yaml::from_file(&node_path).expect("fixture node.yaml");
    node.declared_machine = None;
    fs::write(&node_path, yaml::to_string(&node).expect("node.yaml")).expect("write node.yaml");

    let site_path = root.join("site.yaml");
    let mut site: Site = yaml::from_file(&site_path).expect("fixture site.yaml");
    site.execution_targets.retain(|target| {
        !matches!(
            target.runtime_binding,
            Some(
                dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(_)
            )
        )
    });
    fs::write(&site_path, yaml::to_string(&site).expect("site.yaml")).expect("write site.yaml");
    root
}

pub fn temporary_simulated_docker_fixture() -> TemporaryFixture {
    let root = canonical_temp_dir().join(format!(
        "dotcore-simulated-docker-fixture-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let source = workspace_root().join("backend/fixtures/simulated-docker-site");
    for directory in [
        "catalog/models",
        "catalog/engines",
        "catalog/hardware-classes",
    ] {
        fs::create_dir_all(root.join(directory)).expect("simulated-docker fixture directory");
    }
    for file in [
        "site.yaml",
        "node.yaml",
        "catalog/models/fixture-model.yaml",
        "catalog/models/fixture-model-two.yaml",
        "catalog/engines/fixture.yaml",
        FIXTURE_HARDWARE_CLASS,
    ] {
        fs::copy(source.join(file), root.join(file)).expect("simulated-docker fixture file");
    }
    let site_path = root.join("site.yaml");
    let site = fs::read_to_string(&site_path).expect("simulated-docker site fixture");
    let site_id = format!(
        "simulated-docker-site-test-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let configured = site.replace(
        "siteId: __DOTCORE_SIMULATED_DOCKER_SITE_ID__",
        &format!("siteId: {site_id}"),
    );
    assert_ne!(
        configured, site,
        "simulated-docker site identity placeholder"
    );
    fs::write(site_path, configured).expect("simulated-docker site identity");
    TemporaryFixture { path: root }
}

/// Returns a copy of the master catalog whose Docker-delivered application services name
/// digest-pinned OCI images instead, created once per test process. Simulated application runtimes
/// realize no image, while a development-source (`self`) Nix service would need the node to evaluate
/// the workspace flake.
pub fn simulated_master_catalog() -> PathBuf {
    static CATALOG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let root = std::env::temp_dir().join(format!(
                "dotcore-simulated-master-catalog-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            copy_directory(&workspace_root().join("catalog"), &root);
            for entry in fs::read_dir(root.join("applications")).expect("application catalog") {
                let path = entry.expect("application entry").path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "yaml")
                {
                    let source = fs::read_to_string(&path).expect("application declaration");
                    fs::write(&path, with_oci_service_images(&source))
                        .expect("simulated application declaration");
                }
            }
            root
        })
        .clone()
}

/// Replaces each Docker-delivered Nix service artifact of an application declaration (a `nix:`
/// line and the lines indented under it) with a digest-pinned OCI image of a test registry.
pub fn with_oci_service_images(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut output = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let block = if line.trim() == "nix:" {
            lines[index + 1..]
                .iter()
                .take_while(|next| indent(next) > indent(line))
                .count()
        } else {
            0
        };
        if block > 0 {
            let pad = " ".repeat(indent(line));
            let digest = "a".repeat(64);
            output.push_str(&format!(
                "{pad}oci:\n{pad}  repository:\n{pad}    registry: {{ host: \"registry.dotcore.test\" }}\n{pad}    name: \"application\"\n{pad}  digest: {{ algorithm: \"sha256\", encoded: \"{digest}\" }}\n"
            ));
        } else {
            output.push_str(line);
            output.push('\n');
        }
        index += 1 + block;
    }
    output
}

pub fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("catalog copy directory");
    for entry in fs::read_dir(source).expect("catalog source directory") {
        let entry = entry.expect("catalog source entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("catalog entry type").is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            // Copies content only: a read-only source, such as the Nix store in the sandbox
            // suite, must still yield declarations the copy can rewrite.
            fs::write(
                &target,
                fs::read(entry.path()).expect("read catalog declaration"),
            )
            .expect("copy catalog declaration");
        }
    }
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Waits for a simulated fixture model to become READY through the public service.
pub async fn wait_for_model_ready(service: &SelectedModelService) {
    let mut deployments = Vec::new();
    tokio::time::timeout(CONVERGENCE_DEADLINE, async {
        loop {
            deployments = ModelServiceContract::list_model_deployments(
                service,
                tonic::Request::new(ListModelDeploymentsRequest {}),
            )
            .await
            .expect("fixture deployment")
            .into_inner()
            .deployments;
            if deployments.iter().any(|deployment| {
                deployment
                    .state
                    .as_ref()
                    .is_some_and(|state| state.status == ModelDeploymentStatus::Ready as i32)
            }) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture ready; last deployments: {deployments:?}"));
}

/// Waits until one deployment of a running backend reports READY through its model service.
pub async fn wait_for_ready_deployment(backend: &RunningBackend, deployment_id: &str) {
    let mut models = backend.model_client();
    let mut last_state = None;
    tokio::time::timeout(CONVERGENCE_DEADLINE, async {
        loop {
            let response = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("list deployments")
                .into_inner();
            let deployment = response
                .deployments
                .into_iter()
                .find(|candidate| candidate.model_deployment_id == deployment_id)
                .expect("loaded deployment listed");
            last_state = Some(deployment.state.expect("deployment state"));
            if last_state
                .as_ref()
                .is_some_and(|state| state.status == ModelDeploymentStatus::Ready as i32)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("model readiness deadline; last state: {last_state:?}"));
}

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink. Every fixture directory
/// starts here, so it also relaxes the process's file sync.
pub fn canonical_temp_dir() -> std::path::PathBuf {
    relax_file_sync();
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}

/// Creates a temporary directory under the canonical system temporary directory, removed on drop.
pub fn canonical_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(canonical_temp_dir())
}

/// Switches this test process to the relaxed file sync. Integration tests have no process-start
/// hook and nextest runs each test in its own process, so every support entry that creates a
/// fixture directory, names a site or composes a backend calls it; the switch is idempotent.
fn relax_file_sync() {
    relax_sync_for_tests();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "subprocess helper"]
    async fn external_backend_process_helper() {
        // Runs of ignored tests outside a hardware profile reach this helper too; only the harness's subprocess runs it.
        if std::env::var(EXTERNAL_BACKEND_PROCESS).as_deref() != Ok("1") {
            return;
        }
        run_external_backend_process().await;
    }

    #[test]
    fn retained_records_leave_out_a_line_still_being_written() {
        let records = complete_json_records(b"{\"n\":1}\n{\"n\":2}\n{\"n\":3,\"msg\":\"half");

        assert_eq!(
            records,
            vec![serde_json::json!({"n": 1}), serde_json::json!({"n": 2})]
        );
        assert!(complete_json_records(b"{\"n\":1").is_empty());
    }

    #[test]
    #[should_panic(expected = "retained JSONL record \"not json\"")]
    fn retained_records_reject_a_complete_line_that_is_not_json() {
        complete_json_records(b"{\"n\":1}\nnot json\n");
    }

    #[test]
    fn fast_fixture_removes_the_development_readiness_delay() {
        let fixture = temporary_fast_fixture();
        let site = fs::read_to_string(fixture.join("site.yaml")).expect("fast site fixture");

        assert!(site.contains("readyDelayMillis: 0"));
        assert!(site.contains("stopDelayMillis: 0"));
        assert!(!site.contains("readyDelayMillis: '2000'"));
    }

    #[test]
    fn shared_fixture_and_readiness_helpers_remain_available_to_integration_suites() {
        let _ = temporary_simulated_docker_fixture;
        let _ = wait_for_model_ready;
    }
}
