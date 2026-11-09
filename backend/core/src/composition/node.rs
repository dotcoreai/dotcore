//! Composes the node-local realization of one SiteNode: its accepted node view, leased runtime
//! action fencing, model and application realizers, launch-identity pins, cleanup evidence,
//! deployment logs, serving admission, the observation reporter, and the metric forwarder.
//!
//! The node reads owner intent and reports status, cleanup evidence, and observations only
//! through its control-stream and metric transports, so the same composition serves any
//! node whatever carries its control-plane traffic. It receives no site, owner repository, or
//! owner current state, and keeps its own realization state in its node store.

use std::{
    future::Future,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use dotcore_domain::SitePaths;
use dotcore_domain::{
    background_loop::log_unexpected_loop_end, hardware_observation::HardwareSnapshotSource,
};
use dotcore_node_hardware::LocalNodeCapability;
use dotcore_observability::{
    ForwarderConfig, MetricBucketTransport, MetricForwarder, MetricForwarderTask,
};
use dotcore_proto::dotcore::{
    node::v1::NodeObservationConfiguration,
    site::v1::{ExecutionTarget, execution_target},
};
use dotcore_runtime::RuntimeCapabilityProbes;
use dotcore_runtime_backends::simulated::DeclaredMachine;
use dotcore_site::{
    node::{
        agent::{
            InProcessObservationSink, NodeObservationCollector, NodeObservationFeed,
            NodeReportIdentity, NodeReporter, NodeReporterTask, ObservationReportSink,
        },
        capabilities::NodeTargetCapabilitySource,
        hardware_inventory::{HardwareInventorySource, NodeObservationInventory},
        observation_configuration::{
            NodeObservationConfigurationDigest, node_observation_configuration,
        },
        registry::NodeRegistry,
        session::{
            NodeControlObservations, NodeControlSession, NodeControlSessionTask,
            NodeControlTransport,
        },
        target_inventory::TargetCapabilitySource,
    },
    store::FileNodeState,
    view::{
        desired::{NodeViewInstaller, NodeViewOwnerRules},
        fencing::{AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation},
    },
};
use tracing::{info, warn};

use super::{BackendTestSupport, CompositionError, runtime_adapters::RuntimeAdapterRegistry};
use crate::collection_metrics::{CollectionMetricsSource, CollectionMetricsWriter};

/// Node-local inputs of one SiteNode composition.
pub(super) struct NodeInputs {
    pub(super) site_id: String,
    pub(super) node_id: String,

    /// Execution targets installed on this node; the node realizes workloads only on these.
    pub(super) installed_targets: Vec<ExecutionTarget>,

    /// Process incarnation that fences this node's view and status; the same incarnation names
    /// its observation reports.
    pub(super) incarnation: NodeIncarnation,

    /// The node's observation reporter, composed with the same incarnation.
    pub(super) observation: NodeObservation,

    /// Cache and workload paths of the node's site or node folder root.
    pub(super) paths: SitePaths,

    /// Directory of the node store holding this node's realization state.
    pub(super) node_store: PathBuf,

    /// Specific IPv4 address on which the node publishes application endpoints; required only
    /// when the node has application targets.
    pub(super) publication_host: Option<Ipv4Addr>,

    pub(super) telemetry_sink: Option<Arc<dyn crate::telemetry::TelemetrySink>>,

    /// Dotcore sources this node builds Nix artifacts from: its development checkout, which also
    /// pins development-source artifacts, and the packaged runtime's own source.
    pub(super) dotcore_sources: dotcore_domain::nix::DotcoreSources,

    /// Machine the node declares, which its observation reports and its simulated model workloads
    /// take memory on; `None` when the node reads its real hardware.
    pub(super) declared_machine: Option<DeclaredMachine>,

    pub(super) test_support: BackendTestSupport,
}

/// Control-plane transports of one SiteNode composition.
pub(super) struct NodeTransports {
    /// Carries the node's control stream: observations, status and view acknowledgements up,
    /// desired views down.
    pub(super) control: Arc<dyn NodeControlTransport>,

    /// Streams the node's metric buckets to the site manager.
    pub(super) metrics: Arc<dyn MetricBucketTransport>,
}

/// Composed SiteNode, prepared but not yet started.
pub(super) struct NodeServices {
    view: AcceptedNodeViewAuthority,
    serving_targets: crate::model::controller::serving_targets::LocalServingTargetsSource,
    log_owners: crate::deployment_logs::DeploymentLogOwners,
    diagnostics: crate::diagnostic::NodeDiagnosticReader,
    observation: NodeObservation,
    collection_metrics: CollectionMetricsWriter,
    model_start: Box<dyn FnOnce() + Send>,
    application_start: Box<dyn FnOnce() + Send>,
    session: NodeControlSession,
    lifecycle: NodeLifecycle,
}

impl NodeServices {
    /// Returns the peer-facing serving and deployment-log services a node with a private listener
    /// mounts for the manager.
    pub(super) fn private_services(&self) -> crate::server::private::PrivateServiceAssembly {
        super::routes::worker_private_services(
            self.view.clone(),
            self.serving_targets.clone(),
            self.log_owners.clone(),
            self.diagnostics.clone(),
        )
    }

    /// Returns the node's exact-target admission, through which a router in the same process
    /// dispatches the node's targets.
    pub(super) fn exact_target_admission(
        &self,
    ) -> crate::router::peer_service::ExactTargetAdmission {
        crate::router::peer_service::ExactTargetAdmission::new(
            self.view.clone(),
            self.serving_targets.clone(),
        )
    }

    /// Returns the node's deployment-log reader, through which owners in the same process read
    /// the node's deployment logs.
    pub(super) fn log_reader(&self) -> crate::deployment_logs::DeploymentLogReader {
        crate::deployment_logs::DeploymentLogReader::new(
            self.view.clone(),
            self.log_owners.clone(),
            crate::management_logs::ManagementLogAdmission::new(),
        )
    }

    /// Returns the node's retained-diagnostic reader, through which the Site manager in the same
    /// process reads the node's backend logs.
    pub(super) fn diagnostic_reader(&self) -> crate::diagnostic::NodeDiagnosticReader {
        self.diagnostics.clone()
    }

    /// Splits the node into its observer starters and the handles that keep its realization
    /// alive. Every observation report names `private_port` as the node's private listener.
    pub(super) fn into_starters(self, private_port: Option<u16>) -> (NodeStarters, NodeLifecycle) {
        let observation = self.observation;
        let reporter = observation.reporter.with_private_port(private_port);
        let reported_initial = observation.reported_initial;
        let session = self.session;
        let collection_metrics = self.collection_metrics;
        let collection_metrics_task = Arc::clone(&self.lifecycle._collection_metrics.0);
        (
            NodeStarters {
                reporter: Box::new(move || {
                    // Records each collection the reporter publishes, including one already
                    // published by the node's first report.
                    let writer = tokio::spawn(log_unexpected_loop_end(
                        "collection_metrics_writer",
                        collection_metrics.run(),
                    ));
                    *collection_metrics_task
                        .lock()
                        .expect("node collection metrics writer lock") =
                        Some(writer.abort_handle());
                    if reported_initial {
                        reporter.start_after_initial()
                    } else {
                        reporter.start()
                    }
                }),
                model: self.model_start,
                application: self.application_start,
                control: Box::new(move || session.start()),
            },
            self.lifecycle,
        )
    }
}

/// Starters of one node's observer, realizer, and control-stream tasks.
pub(super) struct NodeStarters {
    pub(super) reporter: Box<dyn FnOnce() -> NodeReporterTask + Send>,
    pub(super) model: Box<dyn FnOnce() + Send>,
    pub(super) application: Box<dyn FnOnce() + Send>,
    pub(super) control: Box<dyn FnOnce() -> NodeControlSessionTask + Send>,
}

/// Handles that keep a node's incarnation and realizers alive; dropping them stops the node's
/// realization and releases its node store.
pub(super) struct NodeLifecycle {
    _incarnation: NodeIncarnation,
    _metric_forwarder: MetricForwarderTask,
    _collection_metrics: CollectionMetricsTask,
    _model: NodeModelLifecycle,
    _application: Option<Arc<crate::application::NodeApplicationRealizer>>,

    /// Model runtime targets of the node, whose action authority tests inspect.
    #[cfg(test)]
    pub(super) model_runtimes: crate::dispatch::model::RuntimeTargetMap,

    /// The node store, which tests inspect for retained cleanup evidence.
    #[cfg(test)]
    pub(super) node_state: FileNodeState,
}

/// Composes one SiteNode's realization from node-local `inputs` and control-plane `transports`.
/// Nothing runs until the returned starters are invoked.
pub(super) async fn compose_node(
    inputs: NodeInputs,
    transports: NodeTransports,
) -> Result<NodeServices, CompositionError> {
    let NodeInputs {
        site_id,
        node_id,
        installed_targets,
        incarnation,
        observation,
        paths,
        node_store,
        publication_host,
        telemetry_sink,
        dotcore_sources,
        declared_machine,
        test_support,
    } = inputs;
    let clock = super::backend_clock(&test_support);
    // Opens its stream to the manager once the node's hardware sampler records into it.
    let (metric_forwarder, metric_forwarder_task) = MetricForwarder::start(
        &node_id,
        transports.metrics,
        Arc::clone(&clock),
        ForwarderConfig::default(),
    )
    .map_err(|error| {
        CompositionError::composition_with_source("start the node metric forwarder", error)
    })?;
    let observation = observation
        .with_sampling(metric_forwarder.recorder(dotcore_site::node::metrics::HARDWARE_SOURCE));
    let manager_session = ManagerSession::parse("0".repeat(32)).map_err(|error| {
        CompositionError::composition_with_source("prepare node manager-session discovery", error)
    })?;
    let accepted_view = AcceptedNodeViewAuthority::new(
        site_id.clone(),
        node_id.clone(),
        incarnation.clone(),
        manager_session,
        test_support.timing.node_control.action_lease,
    )
    .map_err(|error| {
        CompositionError::composition_with_source("prepare accepted node view", error)
    })?;
    let installer = NodeViewInstaller::new(
        accepted_view.clone(),
        &installed_targets,
        node_view_owner_rules(),
    );
    let model_input =
        crate::model::controller::NodeViewModelInput::new(installer.projected_input());
    let state = FileNodeState::open(&node_store, site_id.as_str(), node_id.as_str())
        .map_err(super::lifecycle_current_state_error)?;
    // Preparation output is realization data the node keeps beside its node state.
    let preparation_logs =
        crate::deployment_logs::PreparationLogs::new(&node_store, Arc::clone(&clock));
    #[cfg(test)]
    let node_state_for_tests = state.clone();
    let site_node_id = dotcore_domain::SiteNodeId::parse(&node_id).map_err(|error| {
        CompositionError::composition_with_source(
            "node composition has invalid node identity",
            error,
        )
    })?;
    let registry = super::runtime::runtime_adapter_registry_for_site_id(
        &paths,
        &site_id,
        &site_node_id,
        test_support.native_launcher.as_deref(),
        &test_support.timing.native,
        super::runtime::SimulatedRuntimeInputs {
            test_support: test_support.simulated_runtime.clone(),
            machine: declared_machine,
            clock: Arc::clone(&clock),
        },
    )?;
    let runtimes = super::runtime::node_runtime_targets_for(
        &installed_targets,
        &registry,
        accepted_view.clone(),
    )
    .await?;
    // The node attributes accelerator memory from its reporter's own collections, so no second
    // hardware collector runs on the node.
    let accelerator_attribution =
        crate::model::controller::AcceleratorAttribution::new(observation.inventory())
            .with_process_memory(Arc::new(runtimes.clone()))
            .with_telemetry(telemetry_sink);
    // Runtime-reported workload memory is read with each collection, tagged with its revision.
    let observation =
        observation.with_collection_reading(Arc::new(accelerator_attribution.clone()));
    let target_capabilities = target_capability_source(
        &installed_targets,
        &runtimes,
        observation.reporter.observations(),
        test_support.timing.node_control.capability_max_age,
        Arc::clone(&clock),
    )
    .await?;
    // One set of sources serves both pinning and building, so a node always builds the store
    // path it evaluated from the same development checkout. Roots sit beside the node store, so
    // resetting the site or leaving it drops them with the folder.
    let nix_roots = node_store
        .parent()
        .unwrap_or(&node_store)
        .join(dotcore_artifact::NIX_ROOTS_DIRECTORY);
    let nix = dotcore_artifact::NixSources::new(dotcore_sources).with_roots(nix_roots.clone());
    let nix_root_keeper = dotcore_artifact::NixRootKeeper::new(
        nix_roots,
        Arc::new(NodeStorePins {
            state: state.clone(),
        }),
        &test_support.timing.artifact,
        Arc::clone(&clock),
    );
    let artifacts = crate::model::controller::ArtifactDependencies {
        oci_preparer: super::controllers::oci_preparer_for(
            &installed_targets,
            dotcore_artifact::DockerOciClient::connect,
        )
        .await?,
        nix: nix.clone(),
    };
    let model_composition = crate::model::controller::ModelControllerDependencies {
        artifacts,
        target_capabilities,
        engine_integrations: super::controllers::engine_integrations(
            test_support.simulated_inference,
            &test_support.timing.simulated,
            Arc::clone(&clock),
        ),
        accelerator_attribution: Some(accelerator_attribution.clone()),
    };
    // Created once at startup so status reporting reads the same view the lazily built realizer
    // publishes, before and across realizer construction.
    let (serving_targets, serving_targets_source) =
        crate::model::controller::serving_targets::LocalServingTargetsPublisher::new();
    let model_lifecycle = NodeModelLifecycle::new();
    let model_slot = Arc::clone(&model_lifecycle.realizer);
    let model_state = state.clone();
    let model_runtimes = runtimes.clone();
    let model_cache =
        super::filesystem::model_artifact_cache_path(&paths.model_artifact_cache_root())?;
    // Node logs resolve workloads exactly as the realizer launches them, and read as not created
    // until the realizer has discovered its workloads.
    let realizer_started = Arc::new(AtomicBool::new(false));
    let pinned_model_input = crate::model::controller::NodePinnedModelInput::new(
        model_input.clone(),
        state.clone(),
        node_id.clone(),
        launch_pin_resolver(
            FixedLaunchPins {
                store_paths: test_support.local_development_store_paths.clone(),
            },
            &model_cache,
            model_composition.artifacts.oci_preparer.clone(),
            nix.clone(),
        )?,
    )
    .map_err(super::lifecycle_current_state_error)?;
    // The realizer publishes its engine workload changes to the follows the node logs serve.
    let model_workload_lifecycle = crate::deployment_logs::WorkloadLifecycle::new();
    let model_logs = Arc::new(crate::model::controller::NodeModelLogs::new(
        pinned_model_input.clone(),
        runtimes.clone(),
        dotcore_artifact::ArtifactManager::new(model_cache.clone(), None).map_err(|error| {
            CompositionError::composition_with_source(
                "prepare node deployment-log artifact paths",
                error,
            )
        })?,
        Arc::clone(&realizer_started),
        preparation_logs.clone(),
        model_workload_lifecycle.clone(),
    ));
    let model_status = Arc::new(crate::model::controller::NodeModelStatus::new(
        node_id.clone(),
        installer.projected_input(),
        state.clone(),
        serving_targets_source.clone(),
        accelerator_attribution,
    ));
    let model_construction = Arc::clone(&model_lifecycle.construction);
    let model_shutdown = model_lifecycle.shutdown.clone();
    let model_timing = test_support.timing.model;
    let model_clock = Arc::clone(&clock);
    let model_root_keeper = Arc::clone(&model_lifecycle.root_keeper);
    let model_preparation_logs = preparation_logs.clone();
    let model_start = Box::new(move || {
        // The node keeps the Nix artifacts it pins rooted for as long as it realizes deployments.
        let keeper = tokio::spawn(log_unexpected_loop_end(
            "nix_root_keeper",
            nix_root_keeper.run(),
        ));
        *model_root_keeper.lock().expect("node Nix root keeper lock") = Some(keeper.abort_handle());
        let listener =
            crate::model::controller::ModelRealizationInput::register_change_listener(&model_input);
        let construction = tokio::spawn(async move {
            let retry = model_timing.construction_retry_interval;
            let realizer = construct_with_retry(listener, Arc::clone(&model_clock), retry, || {
                construct_model_realizer(
                    crate::model::controller::NodeModelRealizerDependencies {
                        input: pinned_model_input.clone(),
                        composition: model_composition.clone(),
                        serving_targets: serving_targets.clone(),
                        shutdown: model_shutdown.clone(),
                        preparation_logs: model_preparation_logs.clone(),
                        workload_lifecycle: model_workload_lifecycle.clone(),
                        timing: model_timing,
                        clock: Arc::clone(&model_clock),
                    },
                    model_state.clone(),
                    model_runtimes.clone(),
                    model_cache.clone(),
                )
            })
            .await;
            // Set before the observer can start a workload, so a follow re-resolving the workload
            // when the realizer publishes its start always reads it.
            realizer_started.store(true, std::sync::atomic::Ordering::Release);
            realizer.start_observer();
            *model_slot.lock().expect("node model lifecycle lock") = Some(realizer);
            info!(
                event_kind = "node_model_composition",
                outcome = "started",
                "Node model realizer started from an authenticated complete view"
            );
        });
        *model_construction
            .lock()
            .expect("node model construction lock") = Some(construction.abort_handle());
    });
    let application = application_realizer_for(
        &installed_targets,
        &registry,
        &accepted_view,
        ApplicationRealizerSources {
            state: state.clone(),
            artifact_cache_root: paths.model_artifact_cache_root(),
            nix,
            fixed_pins: FixedLaunchPins {
                store_paths: test_support.local_development_store_paths.clone(),
            },
            preparation_logs,
            inventory: observation.inventory(),
            timing: test_support.timing.application,
            clock: Arc::clone(&clock),
        },
        publication_host,
    )
    .await?
    .map(Arc::new);
    let application_status = application
        .as_ref()
        .map(|application| application.status_owner());
    // Application group resources are read with each collection, beside the runtime-reported
    // workload memory.
    let observation = match &application {
        Some(application) => observation.with_collection_reading(application.resource_reading()),
        None => observation,
    };
    let collection_metrics = collection_metrics_writer(
        &node_id,
        &observation,
        &metric_forwarder,
        Arc::clone(&model_status) as _,
        application.as_deref(),
        Arc::clone(&clock),
    );
    let application_start: Box<dyn FnOnce() + Send> = match application.clone() {
        Some(application) => Box::new(move || {
            application.start_observer();
            info!(
                event_kind = "node_application_composition",
                outcome = "started",
                "Node application realizer started"
            );
        }),
        None => Box::new(|| {}),
    };
    let log_owners = crate::deployment_logs::DeploymentLogOwners {
        model: model_logs,
        application: application
            .as_ref()
            .map(|application| application.log_owner()),
    };
    let session = NodeControlSession::new(
        site_id,
        node_id,
        transports.control,
        accepted_view.clone(),
        installer,
        observation.control.clone(),
        observation.inventory(),
        state,
        model_status,
        application_status,
        &test_support.timing.node_control,
        clock,
    );
    let diagnostics =
        crate::diagnostic::NodeDiagnosticReader::new(&paths.root, accepted_view.clone());
    Ok(NodeServices {
        view: accepted_view,
        serving_targets: serving_targets_source,
        log_owners,
        diagnostics,
        observation,
        collection_metrics,
        model_start,
        application_start,
        session,
        lifecycle: NodeLifecycle {
            _incarnation: incarnation,
            _metric_forwarder: metric_forwarder_task,
            _collection_metrics: CollectionMetricsTask::default(),
            _model: model_lifecycle,
            _application: application,
            #[cfg(test)]
            model_runtimes: runtimes,
            #[cfg(test)]
            node_state: node_state_for_tests,
        },
    })
}

/// The node's per-collection metrics writer task once started, aborted with the node.
#[derive(Default)]
struct CollectionMetricsTask(Arc<Mutex<Option<tokio::task::AbortHandle>>>);

impl Drop for CollectionMetricsTask {
    fn drop(&mut self) {
        if let Some(task) = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// The writer recording, once per collection of SiteNode `node_id`, the memory its model status
/// and the resources its application status attribute, so their history never diverges from the
/// current values.
fn collection_metrics_writer(
    node_id: &str,
    observation: &NodeObservation,
    forwarder: &MetricForwarder,
    models: Arc<dyn crate::model::controller::metrics::HostedAcceleratorMemory>,
    applications: Option<&crate::application::NodeApplicationRealizer>,
    clock: Arc<dyn dotcore_domain::clock::Clock>,
) -> CollectionMetricsWriter {
    let model_node = node_id.to_owned();
    let mut sources = vec![CollectionMetricsSource::new(
        move |at| {
            crate::model::controller::metrics::deployment_memory_records(
                &model_node,
                models.as_ref(),
                at,
            )
        },
        forwarder.recorder(crate::model::controller::metrics::DEPLOYMENT_MEMORY_SOURCE),
    )];
    if let Some(applications) = applications {
        let hosted = applications.hosted_resources();
        let application_node = node_id.to_owned();
        sources.push(CollectionMetricsSource::new(
            move |at| {
                crate::application::application_resource_records(
                    &application_node,
                    hosted.as_ref(),
                    at,
                )
            },
            forwarder.recorder(crate::application::APPLICATION_RESOURCES_SOURCE),
        ));
    }
    CollectionMetricsWriter::new(observation.reporter.observations(), clock, sources)
}

/// Composes the observation part of the SiteNode that shares the manager process: the same
/// reporter as any node, whose first observation registers with the manager's `registry` in
/// process and whose later ones travel on the node's in-process control stream, so it needs no
/// private listener or site trust. Its first collection and registration complete before this
/// returns, so admission reads the node's capabilities from the first request served.
pub(super) async fn compose_manager_node_observation(
    site: &dotcore_proto::dotcore::site::v1::Site,
    node_id: &dotcore_domain::SiteNodeId,
    incarnation: NodeIncarnation,
    registry: NodeRegistry,
    hardware_collector: Box<dyn HardwareSnapshotSource + Send + Sync>,
    paths: &dotcore_domain::SitePaths,
    test_support: &BackendTestSupport,
) -> Result<NodeObservation, CompositionError> {
    let observation_configuration = node_observation_configuration(site, node_id.as_str())
        .map_err(|error| {
            CompositionError::composition_with_source(
                "derive the manager node's observation configuration",
                error,
            )
        })?;
    let observation_configuration_sha256 = NodeObservationConfigurationDigest::for_configuration(
        &observation_configuration,
    )
    .map_err(|error| {
        CompositionError::composition_with_source(
            "digest the manager node's observation configuration",
            error,
        )
    })?;
    let control = NodeControlObservations::new();
    let mut observation = NodeObservation::compose(
        NodeObservationInputs {
            site_id: site.site_id.clone(),
            node_id: node_id.clone(),
            observation_configuration,
            observation_configuration_sha256: observation_configuration_sha256.as_str().to_owned(),
            incarnation,
            hardware_collector,
            probes: capability_probes(paths, test_support),
            clock: super::backend_clock(test_support),
        },
        control.clone(),
        Arc::new(InProcessObservationSink::new(
            site.site_id.clone(),
            node_id.clone(),
            registry,
            control,
        )),
    )?;
    observation.report_initial().await?;
    info!(
        event_kind = "manager_node_observation",
        outcome = "registered",
        node_id = %node_id,
        "Manager node registered its first observation in process"
    );
    Ok(observation)
}

/// Observation part of one SiteNode: the reporter of its hardware and runtime collections, the
/// latest observation its control stream sends, and the inventory its realizers attribute
/// accelerator memory from.
pub(super) struct NodeObservation {
    reporter: SiteNodeReporter,
    control: NodeControlObservations,
    inventory: Arc<NodeObservationInventory>,

    /// Whether the first report already completed, so the report loop starts one interval later.
    reported_initial: bool,
}

impl NodeObservation {
    /// Composes the reporter that delivers this node's collections to `sink`, which hands them
    /// to the control stream through `control`. Nothing is collected until it reports or starts.
    pub(super) fn compose(
        inputs: NodeObservationInputs,
        control: NodeControlObservations,
        sink: Arc<dyn ObservationReportSink + Send + Sync>,
    ) -> Result<Self, CompositionError> {
        let NodeObservationInputs {
            site_id,
            node_id,
            observation_configuration,
            observation_configuration_sha256,
            incarnation,
            hardware_collector,
            probes,
            clock,
        } = inputs;
        let interval = Duration::from_secs(u64::from(observation_configuration.interval_seconds));
        let reporter = NodeReporter::new(
            NodeObservationCollector::new(
                observation_configuration,
                Arc::new(LocalNodeCapability::new(hardware_collector)),
                probes,
                Arc::clone(&clock),
            )
            .map_err(|error| {
                CompositionError::composition_with_source(
                    "prepare node observation collector",
                    error,
                )
            })?,
            sink,
            NodeReportIdentity {
                site_id,
                node_id: node_id.as_str().to_owned(),
                incarnation,
                observation_configuration_sha256,
            },
            interval,
            clock,
        );
        let inventory = Arc::new(NodeObservationInventory::new(
            node_id,
            reporter.observations(),
        ));
        Ok(Self {
            reporter,
            control,
            inventory,
            reported_initial: false,
        })
    }

    /// Also samples the node's hardware memory every second once its reporter starts,
    /// recording through `recorder`.
    fn with_sampling(mut self, recorder: Arc<dyn dotcore_observability::MetricRecorder>) -> Self {
        self.reporter = self.reporter.with_sampling(recorder);
        self
    }

    /// Also takes `reading` with every collection of this node, before it is published.
    fn with_collection_reading(
        mut self,
        reading: Arc<dyn dotcore_site::node::agent::CollectionReading>,
    ) -> Self {
        self.reporter = self.reporter.with_collection_reading(reading);
        self
    }

    /// Hardware inventory read from this node's own collections.
    pub(super) fn inventory(&self) -> Arc<dyn HardwareInventorySource> {
        Arc::clone(&self.inventory) as Arc<dyn HardwareInventorySource>
    }

    /// Completes the node's first collection and report, failing when the control plane does not
    /// accept it.
    async fn report_initial(&mut self) -> Result<(), CompositionError> {
        self.reporter.report_initial().await.map_err(|error| {
            CompositionError::composition_with_source(
                "register the node's first observation",
                error,
            )
        })?;
        self.reported_initial = true;
        Ok(())
    }
}

/// Identity and collection inputs of one node's observation reporter.
pub(super) struct NodeObservationInputs {
    pub(super) site_id: String,
    pub(super) node_id: dotcore_domain::SiteNodeId,
    pub(super) observation_configuration: NodeObservationConfiguration,
    pub(super) observation_configuration_sha256: String,
    pub(super) incarnation: NodeIncarnation,
    pub(super) hardware_collector: Box<dyn HardwareSnapshotSource + Send + Sync>,
    pub(super) probes: RuntimeCapabilityProbes,

    /// Backend clock the reporter and sampler schedule, time out, and stamp on.
    pub(super) clock: Arc<dyn dotcore_domain::clock::Clock>,
}

/// Builds the node's runtime capability probes: the local Docker daemon, and the native host
/// checks over the site's native state root, honoring the test launcher.
pub(super) fn capability_probes(
    paths: &dotcore_domain::SitePaths,
    test_support: &BackendTestSupport,
) -> RuntimeCapabilityProbes {
    RuntimeCapabilityProbes {
        docker: Arc::new(
            dotcore_runtime_backends::docker::capabilities::SystemDockerCapabilityProbe,
        ),
        native: Arc::new(dotcore_runtime_backends::native::NativeProbe::new(
            dotcore_runtime_backends::native::NativeStateRoot::for_site(&paths.root),
            test_support.native_launcher.clone(),
        )),
    }
}

/// Returns the hardware collector of a node: its declared machine when it declares one, so the
/// node never reads its real hardware, else `system`.
pub(super) fn hardware_collector(
    declared_machine: Option<&DeclaredMachine>,
    system: impl FnOnce() -> Box<dyn HardwareSnapshotSource + Send + Sync>,
) -> Box<dyn HardwareSnapshotSource + Send + Sync> {
    match declared_machine {
        Some(machine) => {
            info!(
                event_kind = "declared_machine_selected",
                machine = machine.description().name(),
                "node reports its declared machine instead of its real hardware"
            );
            Box::new(machine.clone())
        }
        None => system(),
    }
}

type SiteNodeReporter = NodeReporter<
    Box<dyn HardwareSnapshotSource + Send + Sync>,
    Arc<dyn ObservationReportSink + Send + Sync>,
>;

/// Waits for the first accepted-view notification, then runs `attempt` until it succeeds. After
/// a failure the next attempt starts on the next view change or after `retry` on `clock`,
/// whichever comes first, and only one attempt is ever in flight. Retrying after `retry` even on
/// an unchanged view recovers from transient failures.
async fn construct_with_retry<T, F, A>(
    listener: Arc<tokio::sync::Notify>,
    clock: Arc<dyn dotcore_domain::clock::Clock>,
    retry: Duration,
    mut attempt: A,
) -> T
where
    A: FnMut() -> F,
    F: Future<Output = anyhow::Result<T>>,
{
    listener.notified().await;
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(error) => {
                warn!(
                    event_kind = "node_model_composition",
                    outcome = "retrying",
                    error = %dotcore_domain::diagnostic::diagnostic_chain(error.as_ref()),
                    retry_seconds = retry.as_secs(),
                    "Node model realizer construction failed; retrying"
                );
            }
        }
        tokio::select! {
            () = listener.notified() => {}
            () = clock.sleep(retry) => {}
        }
    }
}

/// Discovers runtime workloads and constructs the node model realizer from the accepted view.
async fn construct_model_realizer(
    node: crate::model::controller::NodeModelRealizerDependencies<NodeModelInput>,
    state: FileNodeState,
    runtimes: crate::dispatch::model::RuntimeTargetMap,
    cache: PathBuf,
) -> anyhow::Result<NodeModelRealizer> {
    let observations = runtimes
        .discover_workloads()
        .await
        .map_err(|error| anyhow::Error::new(error).context("discover node runtime workloads"))?;
    crate::model::controller::ModelRealizer::new_node_realizer(
        node,
        state,
        runtimes,
        observations,
        cache,
    )
    .map_err(|error| anyhow::Error::new(error).context("construct node model realizer"))
}

/// Confirms every installed model target's runtime platform matches its installed configuration,
/// then returns capabilities refreshed by each completed infrastructure collection and reported
/// stale once the latest collection is older than `max_age` on `clock`.
async fn target_capability_source(
    targets: &[ExecutionTarget],
    runtimes: &crate::dispatch::model::RuntimeTargetMap,
    observations: NodeObservationFeed,
    max_age: Duration,
    clock: Arc<dyn dotcore_domain::clock::Clock>,
) -> Result<Arc<dyn TargetCapabilitySource>, CompositionError> {
    for target in targets.iter().filter(|target| {
        matches!(
            target.runtime_binding,
            Some(execution_target::RuntimeBinding::ModelRuntime(_))
        )
    }) {
        let observed = runtimes
            .execution_platform(&target.execution_target_id)
            .await
            .map_err(|source| {
                CompositionError::composition_with_source(
                    "node runtime platform validation failed",
                    source,
                )
            })?;
        let configured = target.platform.as_ref().ok_or_else(|| {
            CompositionError::composition("node execution target has no platform")
        })?;
        let configured =
            dotcore_domain::ExecutionPlatform::try_from(configured).map_err(|source| {
                CompositionError::composition_with_source(
                    "node execution-target platform is unsupported",
                    source,
                )
            })?;
        if observed != configured {
            return Err(CompositionError::composition(format!(
                "node execution target {:?} runtime platform does not match installed configuration",
                target.execution_target_id
            )));
        }
    }
    Ok(Arc::new(NodeTargetCapabilitySource::new(
        targets,
        observations,
        max_age,
        clock,
    )))
}

/// Composes the apps-controller realizer when the node has application targets; it publishes
/// application endpoints on `publication_host` and pins and builds development-source services
/// from `nix`, the source the node's model realization uses.
struct ApplicationRealizerSources {
    state: FileNodeState,
    artifact_cache_root: PathBuf,
    nix: dotcore_artifact::NixSources,
    fixed_pins: FixedLaunchPins,
    preparation_logs: crate::deployment_logs::PreparationLogs,
    inventory: Arc<dyn HardwareInventorySource>,
    timing: dotcore_domain::timing::ApplicationTiming,
    clock: Arc<dyn dotcore_domain::clock::Clock>,
}

async fn application_realizer_for(
    targets: &[ExecutionTarget],
    runtime_adapters: &RuntimeAdapterRegistry,
    view: &AcceptedNodeViewAuthority,
    sources: ApplicationRealizerSources,
    publication_host: Option<Ipv4Addr>,
) -> Result<Option<crate::application::NodeApplicationRealizer>, CompositionError> {
    let ApplicationRealizerSources {
        state,
        artifact_cache_root,
        nix,
        fixed_pins,
        preparation_logs,
        inventory,
        timing,
        clock,
    } = sources;
    let application_targets = targets
        .iter()
        .filter(|target| {
            matches!(
                target.runtime_binding,
                Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if application_targets.is_empty() {
        return Ok(None);
    }
    let publication_host = publication_host.ok_or_else(|| {
        CompositionError::composition(
            "the node's application targets require a specific IPv4 publication host for their endpoints",
        )
    })?;
    let oci_preparer =
        super::services::application_oci_preparer_for_targets(&application_targets, || async {
            let client = dotcore_artifact::DockerOciClient::connect().await?;
            Ok(
                Arc::new(dotcore_artifact::OciPreparer::new(Arc::new(client)))
                    as Arc<dyn dotcore_artifact::OciImagePreparer>,
            )
        })
        .await?;
    // Pins need no Docker for Nix sources, so every node resolves them; only a node with a Docker
    // application target prepares images.
    let launch_pins = launch_pin_resolver(
        fixed_pins,
        &artifact_cache_root,
        oci_preparer.clone(),
        nix.clone(),
    )?;
    let artifact_manager = oci_preparer
        .map(|preparer| {
            dotcore_artifact::ArtifactManager::new(artifact_cache_root, Some(preparer))
                .map(|artifacts| artifacts.with_nix_sources(nix))
                .map_err(|source| {
                    CompositionError::composition_with_source(
                        "Application artifact manager initialization failed",
                        source,
                    )
                })
        })
        .transpose()?;
    let runtimes = super::runtime::node_application_runtimes_for(
        &application_targets,
        runtime_adapters,
        view.clone(),
    )
    .await?;
    crate::application::NodeApplicationRealizer::new(
        crate::application::NodeApplicationComposition {
            input: view.application_input(),
            state,
            runtimes,
            launch_pins: Some(launch_pins),
            artifact_manager,
            publication_host,
            preparation_logs,
            inventory,
            timing,
            clock,
        },
    )
    .map(Some)
    .map_err(|error| {
        CompositionError::composition_with_source("node application composition", error)
    })
}

/// Model realization input of a node: the installed view with this node's engine pins applied.
type NodeModelInput =
    crate::model::controller::NodePinnedModelInput<crate::model::controller::NodeViewModelInput>;

type NodeModelRealizer = crate::model::controller::ModelRealizer<
    NodeModelInput,
    FileNodeState,
    crate::model::controller::DurableModelRealizationResults,
>;

/// Slot holding the node model realizer once its first construction succeeds, the tasks that
/// construct it and keep its Nix roots, aborted with the node, and the realizer's shutdown,
/// cancelled with the node.
struct NodeModelLifecycle {
    realizer: Arc<Mutex<Option<NodeModelRealizer>>>,
    construction: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    root_keeper: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl NodeModelLifecycle {
    fn new() -> Self {
        Self {
            realizer: Arc::new(Mutex::new(None)),
            construction: Arc::new(Mutex::new(None)),
            root_keeper: Arc::new(Mutex::new(None)),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }
}

impl Drop for NodeModelLifecycle {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for task in [&self.construction, &self.root_keeper] {
            if let Some(task) = task
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                task.abort();
            }
        }
    }
}

/// Returns the node's resolver of launch identities for engines or application services: its
/// Docker image store and Nix sources, or the fixed store paths injected by integration fixtures.
fn launch_pin_resolver(
    fixed: FixedLaunchPins,
    cache: &Path,
    oci_preparer: Option<Arc<dyn dotcore_artifact::OciImagePreparer>>,
    nix: dotcore_artifact::NixSources,
) -> Result<Arc<dyn dotcore_domain::deployment::launch_pin::LaunchPinResolver>, CompositionError> {
    if !fixed.store_paths.is_empty() {
        return Ok(Arc::new(fixed));
    }
    let artifacts = dotcore_artifact::ArtifactManager::new(cache.to_path_buf(), oci_preparer)
        .map_err(|error| {
            CompositionError::composition_with_source(
                "prepare node launch identity resolution",
                error,
            )
        })?
        .with_nix_sources(nix);
    Ok(Arc::new(artifacts))
}

/// Integration-fixture launch identities standing in for a node's development source, by package.
struct FixedLaunchPins {
    store_paths: std::collections::BTreeMap<String, String>,
}

impl dotcore_domain::deployment::launch_pin::LaunchPinResolver for FixedLaunchPins {
    fn resolve_pin<'a>(
        &'a self,
        source: &'a dotcore_domain::deployment::launch_pin::PinSource,
        _platform: &'a dotcore_proto::dotcore::catalog::v1::OciPlatform,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
        use dotcore_domain::deployment::launch_pin::PinSource;

        let resolved = match source {
            PinSource::LocalImage { reference } => {
                Err(anyhow::anyhow!("local image {reference:?} is not present"))
            }
            PinSource::NixDevelopmentSource { package, .. } => {
                self.store_paths.get(package).cloned().ok_or_else(|| {
                    anyhow::Error::new(
                        dotcore_artifact::NixPinError::DevelopmentSourceUnavailable {
                            package: package.clone(),
                        },
                    )
                })
            }
            PinSource::NixLocked { .. } => {
                let system = dotcore_domain::nix::host_system();
                system
                    .and_then(|system| source.locked_store_path(system))
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("no locked store path for {system:?}"))
            }
        };
        Box::pin(std::future::ready(resolved))
    }
}

/// Timing of the node's control stream: production defaults, or the test overrides.
/// The Nix store paths a node pins, read from its node store: the launch identities resolved for
/// its model deployments and application services that are store paths.
struct NodeStorePins {
    state: FileNodeState,
}

impl dotcore_artifact::PinnedStorePaths for NodeStorePins {
    fn pinned_store_paths(&self) -> anyhow::Result<std::collections::BTreeSet<String>> {
        let models = self
            .state
            .node_model_resolved_launch_identities()
            .map_err(|error| anyhow::Error::new(error).context("read model launch pins"))?;
        let applications = self
            .state
            .node_application_realizations()
            .map_err(|error| anyhow::Error::new(error).context("read application launch pins"))?;
        Ok(models
            .into_iter()
            .flat_map(|record| record.resolved_launch_identities)
            .chain(
                applications
                    .into_iter()
                    .flat_map(|record| record.resolved_launch_identities),
            )
            .map(|pin| pin.launch_identity)
            .filter(|identity| dotcore_domain::nix::valid_store_path(identity))
            .collect())
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.subscribe_changes()
    }
}

/// The model and application rules the node installer applies to each accepted view.
pub(super) fn node_view_owner_rules() -> NodeViewOwnerRules {
    NodeViewOwnerRules {
        valid_model_assignment: crate::model::engine_pin::valid_node_view_assignment,
        classify_application_assignment: crate::application::classify_application_assignment,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use dotcore_domain::{ExecutionPlatform, SiteNodeId};
    use dotcore_proto::dotcore::{
        catalog::v1::OciPlatform,
        site::v1::{
            ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter,
            SimulatedRuntimeScenario, execution_target, model_runtime_binding,
        },
    };
    use dotcore_runtime_backends::simulated::InMemoryRuntimeBackend;
    use dotcore_site::node::agent::{NodeCollection, NodeObservationFeed};
    use tokio::sync::Notify;

    use super::{NodeStorePins, construct_with_retry, target_capability_source};
    use crate::dispatch::model::RuntimeTargetMapBuilder;

    #[test]
    fn node_store_pins_are_its_store_paths_until_every_cleanup_is_confirmed() {
        use dotcore_artifact::PinnedStorePaths as _;
        use dotcore_proto::dotcore::{
            deployment::v1::NodeResolvedLaunchIdentity,
            site::store::v1::{NodeApplicationRealization, NodeResolvedLaunchIdentities},
        };

        const SHARED: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-vllm";
        const SERVICE: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxza-docker-image-n8n";
        let pin = |selection_key: &str, launch_identity: &str| NodeResolvedLaunchIdentity {
            selection_key: selection_key.to_owned(),
            launch_identity: launch_identity.to_owned(),
        };
        let directory = tempfile::tempdir().expect("node store");
        let state = dotcore_site::store::FileNodeState::open(directory.path(), "site", "node")
            .expect("node store");
        let pins = NodeStorePins {
            state: state.clone(),
        };
        let mut changes = pins.subscribe();
        for deployment_id in ["first", "second"] {
            state
                .retain_node_model_resolved_launch_identities(NodeResolvedLaunchIdentities {
                    deployment_id: deployment_id.to_owned(),
                    resolved_launch_identities: vec![
                        pin("engine", SHARED),
                        pin("image", &format!("sha256:{}", "0".repeat(64))),
                    ],
                })
                .expect("model pins");
        }
        state
            .record_node_application_realization(NodeApplicationRealization {
                application_deployment_id: "app".to_owned(),
                realization_generation: None,
                resolved_launch_identities: vec![pin("service", SERVICE)],
            })
            .expect("application pins");
        assert!(
            changes.has_changed().expect("node store"),
            "pins signal a change"
        );
        changes.borrow_and_update();

        let pinned = |pins: &NodeStorePins| pins.pinned_store_paths().expect("pins");
        assert_eq!(pinned(&pins), [SHARED, SERVICE].map(str::to_owned).into());

        state
            .confirm_node_model_cleanup("first", 2, None)
            .expect("first cleanup");
        assert_eq!(pinned(&pins), [SHARED, SERVICE].map(str::to_owned).into());
        state
            .confirm_node_model_cleanup("second", 2, None)
            .expect("second cleanup");
        state
            .confirm_node_application_cleanup("app", 2)
            .expect("application cleanup");
        assert!(pinned(&pins).is_empty());
        assert!(
            changes.has_changed().expect("node store"),
            "cleanup signals a change"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_construction_retries_on_the_timer_with_an_unchanged_view() {
        let listener = Arc::new(Notify::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let construction = tokio::spawn(construct_with_retry(
            Arc::clone(&listener),
            Arc::new(dotcore_domain::clock::SystemClock),
            Duration::from_secs(5),
            {
                let attempts = Arc::clone(&attempts);
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        if attempt == 1 {
                            Err(anyhow::anyhow!("transient runtime discovery failure"))
                        } else {
                            Ok(attempt)
                        }
                    }
                }
            },
        ));
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);

        listener.notify_one();
        let constructed = tokio::time::timeout(Duration::from_secs(60), construction)
            .await
            .expect("construction recovers without a view change")
            .expect("construction task");
        tokio::time::advance(Duration::from_secs(60)).await;

        assert_eq!(constructed, 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn node_capabilities_follow_each_completed_collection() {
        let node_target = target("worker-target");
        let mut builder = RuntimeTargetMapBuilder::new();
        builder
            .insert(
                "worker-target",
                InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
            )
            .expect("node target");
        let observations = NodeObservationFeed::default();
        let source = target_capability_source(
            std::slice::from_ref(&node_target),
            &builder.build().expect("node targets"),
            observations.clone(),
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            Arc::new(dotcore_domain::clock::SystemClock),
        )
        .await
        .expect("node capability source");
        // The node declares an RTX 5090 desktop, so its collection is that machine's snapshot.
        let machine =
            dotcore_node_hardware::MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
                .expect("named machine")
                .render(&[], std::time::SystemTime::now())
                .expect("rendered snapshot");
        let snapshot = dotcore_proto::dotcore::site::private::v1::NodeObservationSnapshot {
            hardware: Some(
                dotcore_proto::dotcore::site::private::v1::NodeHardwareObservation {
                    result: Some(
                        dotcore_proto::dotcore::site::private::v1::node_hardware_observation::Result::Snapshot(
                            machine,
                        ),
                    ),
                },
            ),
            runtimes: vec![dotcore_proto::dotcore::observation::v1::RuntimeObservation {
                execution_target_id: "worker-target".to_owned(),
                result: Some(
                    dotcore_proto::dotcore::observation::v1::runtime_observation::Result::Available(
                        dotcore_proto::google::protobuf::Empty {},
                    ),
                ),
                ..Default::default()
            }],
        };
        observations.publish(
            NodeCollection::without_processes(snapshot.clone()),
            1,
            std::time::SystemTime::now()
                - dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age
                - Duration::from_secs(1),
        );

        assert_eq!(
            crate::hardware::selection::select_target_capabilities(
                &node_target,
                &source.observe().await.expect("aged inventory")
            ),
            Err(
                crate::hardware::selection::TargetCapabilitySelectionError::StalePlatform {
                    execution_target_id: "worker-target".to_owned(),
                }
            )
        );

        observations.publish(
            NodeCollection::without_processes(snapshot),
            2,
            std::time::SystemTime::now(),
        );
        let refreshed = crate::hardware::selection::select_target_capabilities(
            &node_target,
            &source.observe().await.expect("refreshed inventory"),
        )
        .expect("refreshed node capabilities");

        assert_eq!(refreshed.platform, ExecutionPlatform::linux_amd64());
        assert_eq!(
            refreshed.memory_architecture,
            dotcore_domain::MemoryArchitecture::Dedicated
        );
        assert_eq!(
            refreshed.memory_pool_total_bytes(),
            Some(32_607 * 1024 * 1024)
        );
        assert_eq!(
            refreshed
                .capacity
                .accelerator_pools()
                .next()
                .and_then(crate::hardware::capacity::Pool::free_bytes),
            Some((32_607 - 497) * 1024 * 1024)
        );
    }

    fn target(id: &str) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: SiteNodeId::for_test("node-one").to_string(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()],
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
        }
    }
}
