//! Collects complete node-local hardware and runtime observations and hands them to the node's
//! control stream to the site manager, and builds the authenticated transport that stream uses.
//!
//! Collection is bounded and sequential and keeps independent failures explicit. The private
//! transport verifies the exact manager identity before sending anything, and the `SiteNode` that
//! shares the manager process registers its first observation in process, only ever for itself.
//! Between snapshots, the agent samples hardware memory every wall-clock second and records it as
//! metrics; it retains nothing itself. Every schedule, timeout, and timestamp reads the backend
//! clock.

mod sampler;

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use dotcore_domain::hardware_observation::{AcceleratorProcessObservation, HardwareSnapshotSource};
use dotcore_domain::{
    ExecutionPlatform, SiteNodeId,
    clock::{Clock, next_boundary, slot_start},
};
use dotcore_node_hardware::LocalNodeCapability;
use dotcore_observability::MetricRecorder;
use dotcore_proto::{
    dotcore::{
        node::v1::{NodeExecutionTarget, NodeObservationConfiguration, node_execution_target},
        observation::v1::{ObservationUnavailable, RuntimeObservation, runtime_observation},
        site::{
            private::v1::{
                NodeHardwareObservation, NodeObservation, NodeObservationSnapshot,
                node_hardware_observation,
            },
            v1::DockerAcceleratorTransport,
        },
    },
    google::protobuf::Empty,
};
use dotcore_runtime::{DockerCapabilityReport, NativeCapabilityReport, RuntimeCapabilityProbes};
use tokio::sync::watch;
use tonic::transport::{ClientTlsConfig, Endpoint, Identity};
use tracing::{debug, info, warn};
use url::Url;

use self::sampler::NodeSampler;
use crate::{
    node::{
        folder::CertificateFingerprint,
        observation_configuration::{
            NodeObservationConfigurationError, validate_node_observation_configuration,
        },
        registry::NodeRegistry,
    },
    private_transport::{PinnedPeerVerifier, pem},
    view::fencing::NodeIncarnation,
};

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest wait for one observation delivery to the manager. It exceeds the transport's connect,
/// handshake, and request bounds together, so their errors surface first; it only stops a delivery
/// that ignores them. Missed report ticks are delayed, so a delivery that runs to this bound delays
/// the next report by at most this long.
const DEFAULT_REPORT_DELIVERY_TIMEOUT: Duration = Duration::from_secs(90);

/// Authenticated outbound manager transport projected once from a validated node folder.
#[derive(Clone)]
pub struct WorkerPrivateClientConfiguration {
    manager_endpoint: Url,
    site_id: String,
    manager_node_id: String,
    node_id: String,
    trust_anchor_der: Vec<u8>,
    manager_certificate_der: Vec<u8>,
    worker_certificate_der: Vec<u8>,
    worker_private_key_pkcs8: Vec<u8>,
    bounds: ManagerTransportBounds,
}

impl WorkerPrivateClientConfiguration {
    /// Projects exact manager pin, site identity, and node identity from startup custody.
    pub fn from_node_folder(folder: &crate::node::folder::ValidatedNodeFolder) -> Self {
        Self {
            manager_endpoint: folder.manager_endpoint().clone(),
            site_id: folder.site_id().to_owned(),
            manager_node_id: folder.manager_node_id().to_owned(),
            node_id: folder.worker_node_id().to_owned(),
            trust_anchor_der: folder.trust_anchor_der().to_vec(),
            manager_certificate_der: folder.manager_certificate_der().to_vec(),
            worker_certificate_der: folder.worker_certificate_der().to_vec(),
            worker_private_key_pkcs8: folder.worker_private_key_pkcs8_der().to_vec(),
            bounds: ManagerTransportBounds::default(),
        }
    }

    /// Builds the bounded, pinned endpoint every worker-to-manager channel connects through.
    pub fn endpoint(&self) -> anyhow::Result<Endpoint> {
        let host = self
            .manager_endpoint
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("validated manager endpoint has no host"))?
            .to_owned();
        let verifier = Arc::new(PinnedPeerVerifier::new(
            &self.trust_anchor_der,
            &self.site_id,
            &self.manager_node_id,
            CertificateFingerprint::for_certificate(&self.manager_certificate_der),
        )?);
        let identity = Identity::from_pem(
            pem("CERTIFICATE", &self.worker_certificate_der),
            pem("PRIVATE KEY", &self.worker_private_key_pkcs8),
        );
        let bounds = &self.bounds;
        Ok(Endpoint::from_shared(self.manager_endpoint.to_string())?
            .connect_timeout(bounds.connect)
            .timeout(bounds.request)
            .http2_keep_alive_interval(bounds.keepalive_interval)
            .keep_alive_timeout(bounds.keepalive_timeout)
            .keep_alive_while_idle(true)
            .tls_config_with_verifier(
                ClientTlsConfig::new()
                    .domain_name(host)
                    .identity(identity)
                    .assume_http2(true)
                    .timeout(bounds.tls_handshake),
                verifier,
            )?)
    }
}

impl fmt::Debug for WorkerPrivateClientConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerPrivateClientConfiguration")
            .field("manager_endpoint", &self.manager_endpoint)
            .field("site_id", &self.site_id)
            .field("manager_node_id", &self.manager_node_id)
            .field("node_id", &self.node_id)
            .field("trust_anchor_der", &"<DER certificate>")
            .field("manager_certificate_der", &"<DER certificate>")
            .field("worker_certificate_der", &"<DER certificate>")
            .field("worker_private_key_pkcs8", &"<redacted>")
            .field("bounds", &self.bounds)
            .finish()
    }
}

/// Bounds of the authenticated worker-to-manager transport.
///
/// A manager host can vanish mid-exchange without closing its connections, for example when it
/// powers off after accepting a connection. Every connection step and request is bounded and
/// established connections are pinged, so no node channel waits on a silent manager indefinitely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ManagerTransportBounds {
    /// TCP connection establishment.
    connect: Duration,

    /// TLS handshake once the TCP connection is established; the connect bound does not cover it.
    tls_handshake: Duration,

    /// One request on an established connection. Manager exchanges answer immediately, without
    /// holding a request open.
    request: Duration,

    /// Interval of HTTP/2 keepalive pings on an established connection, including while idle.
    keepalive_interval: Duration,

    /// Time an HTTP/2 keepalive probe may go unanswered before the connection is closed.
    keepalive_timeout: Duration,
}

impl Default for ManagerTransportBounds {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            tls_handshake: Duration::from_secs(10),
            request: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(5),
        }
    }
}

/// Runs sequential node observations on a fixed schedule, starting immediately, and reports
/// each revision to the manager.
/// Each completed collection is also published to node-local consumers before reporting.
pub struct NodeReporter<C, S> {
    collector: NodeObservationCollector<C>,
    sink: S,
    identity: NodeReportIdentity,
    interval: Duration,
    next_revision: u64,
    observations: NodeObservationFeed,
    private_port: Option<u16>,
    delivery_timeout: Duration,
    clock: Arc<dyn Clock>,
    sampler: Option<NodeSampler<C>>,

    /// Node-local readings taken with every collection, tagged with its revision.
    readings: Vec<Arc<dyn CollectionReading>>,

    /// Whether the manager accepted any report of this process yet, so that first acceptance is
    /// logged once.
    registered: bool,
}

/// Identity every observation report of one node process carries.
pub struct NodeReportIdentity {
    pub site_id: String,
    pub node_id: String,
    pub incarnation: NodeIncarnation,
    pub observation_configuration_sha256: String,
}

impl<C, S> NodeReporter<C, S> {
    /// Creates a reporter that delivers each collection to `sink` under `identity`, scheduling
    /// and stamping on `clock`.
    pub fn new(
        collector: NodeObservationCollector<C>,
        sink: S,
        identity: NodeReportIdentity,
        interval: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            collector,
            sink,
            identity,
            interval,
            next_revision: 1,
            observations: NodeObservationFeed::default(),
            private_port: None,
            delivery_timeout: DEFAULT_REPORT_DELIVERY_TIMEOUT,
            clock,
            sampler: None,
            readings: Vec::new(),
            registered: false,
        }
    }

    /// Reports `port` as this node's bound private listener in every registration; `None`
    /// reports no listener, so the manager never dials this node.
    pub fn with_private_port(mut self, port: Option<u16>) -> Self {
        self.private_port = port;
        self
    }

    /// Also takes `reading` with every collection, before that collection is published.
    pub fn with_collection_reading(mut self, reading: Arc<dyn CollectionReading>) -> Self {
        self.readings.push(reading);
        self
    }

    /// Bounds each delivery by `timeout` instead of the production default.
    #[cfg(test)]
    fn with_delivery_timeout(mut self, timeout: Duration) -> Self {
        self.delivery_timeout = timeout;
        self
    }

    /// Returns the feed that receives every completed collection of this reporter.
    pub fn observations(&self) -> NodeObservationFeed {
        self.observations.clone()
    }
}

impl<C, S> NodeReporter<C, S>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
    S: ObservationReportSink + Send + Sync + 'static,
{
    /// Also samples hardware memory every wall-clock second once started, recording it through
    /// `recorder`.
    pub fn with_sampling(mut self, recorder: Arc<dyn MetricRecorder>) -> Self {
        self.sampler = Some(NodeSampler::new(
            &self.identity.node_id,
            Arc::clone(&self.collector.hardware),
            recorder,
            Arc::clone(&self.clock),
        ));
        self
    }

    /// Starts the report loop. The first observation begins immediately.
    pub fn start(self) -> NodeReporterTask {
        self.spawn(true)
    }

    /// Completes one collection and report before the loop starts, failing when the control
    /// plane does not accept it.
    pub async fn report_initial(&mut self) -> anyhow::Result<()> {
        self.report_once(self.current_slot()).await?;
        self.mark_registered();
        Ok(())
    }

    /// Starts the report loop after [`Self::report_initial`]: the next report begins at the next
    /// interval boundary.
    pub fn start_after_initial(self) -> NodeReporterTask {
        self.spawn(false)
    }

    fn spawn(mut self, immediate: bool) -> NodeReporterTask {
        let sampler = self.sampler.take();
        NodeReporterTask {
            report: spawn_loop("node_reporter", self.run(immediate)),
            sample: sampler.map(|sampler| spawn_loop("node_sampler", sampler.run())),
        }
    }

    /// Reports at every wall-clock multiple of the interval, stamping each collection with the
    /// boundary it started for, so readings of all nodes line up. An immediate first report
    /// is stamped with the boundary of the slot it falls in. An attempt that overruns its slot,
    /// or a wall clock stepped across the awaited boundary, resumes at the next boundary after
    /// the wall time it observes: never a burst of catch-up reports, never a wait for a boundary
    /// that has passed.
    async fn run(mut self, immediate: bool) {
        if immediate {
            self.report_and_register(self.current_slot()).await;
        }
        let interval = self.interval;
        loop {
            let wall = self.clock.wall_now();
            let boundary = next_boundary(wall, interval);
            let wait = boundary.duration_since(wall).unwrap_or_default();
            self.clock.sleep_until(self.clock.now() + wait).await;

            let woken = self.clock.wall_now();
            if woken < boundary || woken >= boundary + interval {
                continue;
            }
            self.report_and_register(boundary).await;
        }
    }

    async fn report_and_register(&mut self, observed_at: SystemTime) {
        match self.report_once(observed_at).await {
            Ok(()) => self.mark_registered(),
            Err(error) if error.is::<ManagerChannelClosed>() => {
                debug!(
                    event_kind = "node_observation_report",
                    outcome = "deferred",
                    "Node observation waits for the control stream to open"
                );
            }
            Err(error) => {
                warn!(
                    event_kind = "node_observation_report",
                    outcome = "failed",
                    error = %error,
                    "Node observation report failed"
                );
            }
        }
    }

    /// Logs the manager's first acceptance of this process's observations.
    fn mark_registered(&mut self) {
        if !std::mem::replace(&mut self.registered, true) {
            info!(
                event_kind = "node_observation_registration",
                outcome = "accepted",
                "Manager accepted the node process observation"
            );
        }
    }

    /// Returns the interval boundary at or before the current wall time.
    fn current_slot(&self) -> SystemTime {
        slot_start(self.clock.wall_now(), self.interval)
    }

    async fn report_once(&mut self, observed_at: SystemTime) -> anyhow::Result<()> {
        let revision = self.next_revision;
        self.next_revision = self
            .next_revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("node observation revision exhausted"))?;
        self.observations.begin(revision);
        // Readings run beside the hardware collection, so they describe the same moment, and
        // finish before it is published, so every reader of the collection finds them.
        let (collection, ()) = tokio::join!(self.collector.collect(), self.take_readings(revision));
        let snapshot = collection.snapshot.clone();
        self.observations.publish(collection, revision, observed_at);
        let request = NodeObservation {
            site_id: self.identity.site_id.clone(),
            node_id: self.identity.node_id.clone(),
            incarnation_id: self.identity.incarnation.as_str().to_owned(),
            revision,
            snapshot: Some(snapshot),
            observation_configuration_sha256: self
                .identity
                .observation_configuration_sha256
                .clone(),
            private_port: self.private_port.map(u32::from),
        };
        within(
            self.clock.as_ref(),
            self.delivery_timeout,
            self.sink.report(request),
        )
        .await
        .ok_or_else(|| anyhow::anyhow!("node observation delivery did not complete in time"))?
    }

    /// Takes every collection reading for `revision`, each bounded like a probe. One that runs
    /// out of time is dropped, so that collection has no reading from it.
    async fn take_readings(&self, revision: u64) {
        let readings = self.readings.iter().map(|reading| async move {
            if within(
                self.clock.as_ref(),
                self.collector.probe_timeout,
                reading.read(revision),
            )
            .await
            .is_none()
            {
                warn!(
                    event_kind = "node_collection_reading",
                    outcome = "timed_out",
                    revision,
                    "Node collection reading timed out"
                );
            }
        });
        futures_util::future::join_all(readings).await;
    }
}

/// A node-local reading taken with every hardware collection, such as the memory a runtime
/// reports for its workloads. It is taken at the same time as the hardware and completes before
/// the collection is published, so a reader of one collection can take the readings tagged with
/// its revision.
pub trait CollectionReading: Send + Sync {
    /// Takes the reading for the collection reported as `revision`.
    fn read(&self, revision: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Latest complete node observation, shared with node-local consumers such as admission of
/// accepted model intent and accelerator-memory attribution. Each snapshot carries the start of
/// the reporting slot it was collected for, so a failed probe appears as an explicit unavailable
/// outcome and never re-stamps an older fact. Consumers that act once per collection are told the
/// revision of each one as it is published.
#[derive(Clone, Default)]
pub struct NodeObservationFeed {
    state: Arc<Mutex<FeedState>>,
    published: Arc<watch::Sender<u64>>,
}

/// The latest completed collection and the revision of the latest one that began reading.
#[derive(Default)]
struct FeedState {
    latest: Option<CollectedNodeObservation>,
    begun: u64,
}

/// One complete node observation, its revision and the start of the reporting slot it was
/// collected for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CollectedNodeObservation {
    pub(crate) snapshot: NodeObservationSnapshot,

    /// Node-private accelerator process rows of the same hardware collection; absent when that
    /// collection failed. They are never reported to the manager.
    pub(crate) accelerator_processes: Option<AcceleratorProcessObservation>,

    /// Revision of the node observation that reports this collection to the manager.
    pub(crate) revision: u64,

    pub(crate) collected_at: SystemTime,
}

/// One complete collection: the snapshot reported to the manager and the node-private accelerator
/// process rows collected with it.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeCollection {
    pub(crate) snapshot: NodeObservationSnapshot,

    /// Accelerator process rows; absent when hardware collection failed.
    pub(crate) accelerator_processes: Option<AcceleratorProcessObservation>,
}

#[cfg(any(test, feature = "test-support"))]
impl NodeCollection {
    /// A collection whose hardware result carries no accelerator process rows.
    pub fn without_processes(snapshot: NodeObservationSnapshot) -> Self {
        Self {
            snapshot,
            accelerator_processes: None,
        }
    }
}

impl NodeObservationFeed {
    /// Replaces the latest observation with one completed collection, reported as observation
    /// `revision` and stamped with the start of its reporting slot, then announces its revision.
    pub fn publish(&self, collection: NodeCollection, revision: u64, collected_at: SystemTime) {
        self.lock().latest = Some(CollectedNodeObservation {
            snapshot: collection.snapshot,
            accelerator_processes: collection.accelerator_processes,
            revision,
            collected_at,
        });
        self.published.send_replace(revision);
    }

    /// Follows the revision of the latest published collection, 0 before the first. Revisions
    /// published in quick succession may be seen only as the latest one; every reader of the
    /// feed finds the announced collection, or a newer one, already published.
    pub fn published_revisions(&self) -> watch::Receiver<u64> {
        self.published.subscribe()
    }

    /// Returns the latest completed collection, if any has finished.
    pub(crate) fn latest(&self) -> Option<CollectedNodeObservation> {
        self.lock().latest.clone()
    }

    /// Revision of the first collection that begins reading hardware after this call. A collection
    /// already reading, or published, may have read the hardware before something that exists now.
    pub(crate) fn next_revision(&self) -> u64 {
        let state = self.lock();
        let published = state.latest.as_ref().map_or(0, |latest| latest.revision);
        state.begun.max(published).saturating_add(1)
    }

    /// Marks that the collection reported as `revision` begins reading hardware.
    fn begin(&self, revision: u64) {
        let mut state = self.lock();
        state.begun = state.begun.max(revision);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Running node report loop and its hardware sampler, aborted when backend serving stops.
pub struct NodeReporterTask {
    report: tokio::task::JoinHandle<()>,
    sample: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for NodeReporterTask {
    fn drop(&mut self) {
        self.report.abort();
        if let Some(sample) = &self.sample {
            sample.abort();
        }
    }
}

fn spawn_loop(
    name: &'static str,
    background_loop: impl Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(dotcore_domain::background_loop::log_unexpected_loop_end(
        name,
        background_loop,
    ))
}

/// Delivers one node observation to the manager. A sink that cannot deliver now because no
/// channel to the manager is open fails with [`ManagerChannelClosed`], an expected outcome the
/// reporter does not treat as a failure.
pub trait ObservationReportSink {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;
}

/// No channel to the manager is open, so an observation waits for the next one to open.
#[derive(Debug, thiserror::Error)]
#[error(
    "the node control stream to the manager is not open; the observation is sent when it opens"
)]
pub(crate) struct ManagerChannelClosed;

impl<S: ObservationReportSink + ?Sized> ObservationReportSink for Arc<S> {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        (**self).report(request)
    }
}

/// Delivers the first observation of the `SiteNode` that shares the manager process straight to
/// the manager's registry, before the manager can serve its stream, and every later one through
/// the node control stream.
///
/// The node identity is fixed at construction, so the first report is refused when it names
/// another site or node instead of letting co-location speak for a different node.
pub struct InProcessObservationSink {
    site_id: String,
    node_id: SiteNodeId,
    registry: NodeRegistry,
    stream: crate::node::session::NodeControlObservations,
    registered: AtomicBool,
}

impl InProcessObservationSink {
    pub fn new(
        site_id: String,
        node_id: SiteNodeId,
        registry: NodeRegistry,
        stream: crate::node::session::NodeControlObservations,
    ) -> Self {
        Self {
            site_id,
            node_id,
            registry,
            stream,
            registered: AtomicBool::new(false),
        }
    }
}

impl ObservationReportSink for InProcessObservationSink {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if self.registered.load(Ordering::Acquire) {
                return self.stream.report(request).await;
            }
            if request.site_id != self.site_id || request.node_id != self.node_id.as_str() {
                anyhow::bail!(
                    "in-process observation report names node {:?} instead of {:?}",
                    request.node_id,
                    self.node_id.as_str()
                );
            }
            let revision = request.revision;
            let registry = self.registry.clone();
            let registered = request.clone();
            // Acceptance takes the coordinator and site locks, so it runs off the executor.
            let response = tokio::task::spawn_blocking(move || registry.accept(request))
                .await
                .context("in-process observation acceptance task failed")?
                .context("manager rejected the in-process observation report")?;
            if response.accepted_revision != revision {
                anyhow::bail!(
                    "manager acknowledged revision {} for node revision {revision}",
                    response.accepted_revision
                );
            }
            // The stream re-sends the registered observation when it opens; the manager answers
            // the exact repeat as already accepted.
            self.stream.seed(registered);
            self.registered.store(true, Ordering::Release);
            Ok(())
        })
    }
}

/// Collects one complete node observation from host-local, read-only infrastructure probes.
/// All configured targets receive an explicit outcome even when another probe fails.
pub struct NodeObservationCollector<C> {
    hardware: Arc<LocalNodeCapability<C>>,
    runtime: RuntimeObservationCollector,
    probe_timeout: Duration,
    clock: Arc<dyn Clock>,
}

/// Collects complete target-attributed runtime observations from host-local infrastructure probes.
struct RuntimeObservationCollector {
    configuration: NodeObservationConfiguration,
    probes: RuntimeCapabilityProbes,

    /// Set while a native probe thread runs, so a hung probe is never stacked by later cycles.
    native_in_flight: Arc<AtomicBool>,
    probe_timeout: Duration,
    clock: Arc<dyn Clock>,
}

/// Clears the in-flight flag when the native probe thread ends, including by panic.
struct NativeInFlightGuard(Arc<AtomicBool>);

impl Drop for NativeInFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<C> NodeObservationCollector<C> {
    /// Creates a collector with the production probe time bound, timed on `clock`.
    pub fn new(
        configuration: NodeObservationConfiguration,
        hardware: Arc<LocalNodeCapability<C>>,
        probes: RuntimeCapabilityProbes,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, NodeObservationConfigurationError> {
        Self::with_timeout(
            configuration,
            hardware,
            probes,
            DEFAULT_PROBE_TIMEOUT,
            clock,
        )
    }

    fn with_timeout(
        configuration: NodeObservationConfiguration,
        hardware: Arc<LocalNodeCapability<C>>,
        probes: RuntimeCapabilityProbes,
        probe_timeout: Duration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, NodeObservationConfigurationError> {
        Ok(Self {
            hardware,
            runtime: RuntimeObservationCollector::with_timeout(
                configuration,
                probes,
                probe_timeout,
                Arc::clone(&clock),
            )?,
            probe_timeout,
            clock,
        })
    }
}

impl<C> NodeObservationCollector<C>
where
    C: HardwareSnapshotSource + Send + Sync + 'static,
{
    /// Collects one complete snapshot. Probe failures become explicit per-capability outcomes.
    pub(crate) async fn collect(&self) -> NodeCollection {
        let (hardware, accelerator_processes) = self.collect_hardware().await;
        let runtimes = self.runtime.collect().await;

        NodeCollection {
            snapshot: NodeObservationSnapshot {
                hardware: Some(hardware),
                runtimes,
            },
            accelerator_processes,
        }
    }

    async fn collect_hardware(
        &self,
    ) -> (
        NodeHardwareObservation,
        Option<AcceleratorProcessObservation>,
    ) {
        match within(
            self.clock.as_ref(),
            self.probe_timeout,
            self.hardware.collect_observation(),
        )
        .await
        {
            Some(Ok(observation)) => (
                NodeHardwareObservation {
                    result: Some(node_hardware_observation::Result::Snapshot(
                        observation.snapshot,
                    )),
                },
                Some(observation.accelerator_processes),
            ),
            Some(Err(error)) => {
                warn!(
                    event_kind = "node_hardware_observation",
                    outcome = "failed",
                    error = %error,
                    "Node hardware observation failed"
                );
                (
                    unavailable_hardware(
                        "COLLECTION_FAILED",
                        "Hardware observation failed on this node.",
                    ),
                    None,
                )
            }
            None => {
                warn!(
                    event_kind = "node_hardware_observation",
                    outcome = "timed_out",
                    "Node hardware observation timed out"
                );
                (
                    unavailable_hardware(
                        "COLLECTION_TIMED_OUT",
                        "Hardware observation timed out on this node.",
                    ),
                    None,
                )
            }
        }
    }
}

impl RuntimeObservationCollector {
    fn with_timeout(
        configuration: NodeObservationConfiguration,
        probes: RuntimeCapabilityProbes,
        probe_timeout: Duration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, NodeObservationConfigurationError> {
        validate_node_observation_configuration(&configuration)?;
        Ok(Self {
            configuration,
            probes,
            native_in_flight: Arc::new(AtomicBool::new(false)),
            probe_timeout,
            clock,
        })
    }

    /// Collects one complete target-attributed runtime snapshot.
    async fn collect(&self) -> Vec<RuntimeObservation> {
        let (mut probes, native) =
            tokio::join!(self.collect_runtime_probes(), self.collect_native_probe());
        probes.native = native;
        self.configuration
            .execution_targets
            .iter()
            .map(|target| runtime_observation(target, &probes))
            .collect()
    }

    /// Runs the native host checks on a blocking thread, only when a native target exists.
    async fn collect_native_probe(&self) -> Option<Result<NativeCapabilityReport, ()>> {
        let native_required = self.configuration.execution_targets.iter().any(|target| {
            matches!(
                target.implementation,
                Some(node_execution_target::Implementation::Native(_))
            )
        });
        if !native_required {
            return None;
        }
        let native = Arc::clone(&self.probes.native);

        // A timed-out probe thread cannot be cancelled, so the targets stay unavailable until it
        // ends instead of starting another one.
        if self.native_in_flight.swap(true, Ordering::AcqRel) {
            warn!(
                event_kind = "node_runtime_observation",
                runtime = "native",
                outcome = "in_flight",
                "Previous node native probe is still running"
            );
            return Some(Err(()));
        }
        let guard = NativeInFlightGuard(Arc::clone(&self.native_in_flight));
        let probe = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            native.probe()
        });
        Some(
            match within(self.clock.as_ref(), self.probe_timeout, probe).await {
                Some(Ok(report)) => Ok(report),
                Some(Err(error)) => {
                    warn!(
                        event_kind = "node_runtime_observation",
                        runtime = "native",
                        outcome = "failed",
                        error = %error,
                        "Node native probe failed"
                    );
                    Err(())
                }
                None => {
                    warn!(
                        event_kind = "node_runtime_observation",
                        runtime = "native",
                        outcome = "timed_out",
                        "Node native probe timed out"
                    );
                    Err(())
                }
            },
        )
    }

    async fn collect_runtime_probes(&self) -> RuntimeProbeSet {
        let docker_required = self.configuration.execution_targets.iter().any(|target| {
            matches!(
                target.implementation,
                Some(node_execution_target::Implementation::Docker(_))
            )
        });
        if !docker_required {
            return RuntimeProbeSet::default();
        }

        let docker = match within(
            self.clock.as_ref(),
            self.probe_timeout,
            self.probes.docker.probe(),
        )
        .await
        {
            Some(Ok(report)) => Ok(report),
            Some(Err(error)) => {
                warn!(
                    event_kind = "node_runtime_observation",
                    runtime = "docker",
                    outcome = "failed",
                    error = %error,
                    "Node Docker infrastructure probe failed"
                );
                Err(())
            }
            None => {
                warn!(
                    event_kind = "node_runtime_observation",
                    runtime = "docker",
                    outcome = "timed_out",
                    "Node Docker infrastructure probe timed out"
                );
                Err(())
            }
        };
        let cdi_required = self.configuration.execution_targets.iter().any(|target| {
            matches!(
                target.implementation,
                Some(node_execution_target::Implementation::Docker(ref configuration))
                    if configuration.accelerator_transport == DockerAcceleratorTransport::Cdi as i32
            )
        });
        let cdi_selectors = if cdi_required && docker.is_ok() {
            Some(
                match within(
                    self.clock.as_ref(),
                    self.probe_timeout,
                    self.probes.docker.nvidia_cdi_selectors(),
                )
                .await
                {
                    Some(Ok(selectors)) => Ok(selectors),
                    Some(Err(error)) => {
                        warn!(
                            event_kind = "node_runtime_observation",
                            runtime = "docker_cdi",
                            outcome = "failed",
                            error = %error,
                            "Node Docker CDI selector probe failed"
                        );
                        Err(())
                    }
                    None => {
                        warn!(
                            event_kind = "node_runtime_observation",
                            runtime = "docker_cdi",
                            outcome = "timed_out",
                            "Node Docker CDI selector probe timed out"
                        );
                        Err(())
                    }
                },
            )
        } else {
            None
        };

        RuntimeProbeSet {
            docker: Some(docker),
            cdi_selectors,
            native: None,
        }
    }
}

#[derive(Default)]
struct RuntimeProbeSet {
    docker: Option<Result<DockerCapabilityReport, ()>>,
    cdi_selectors: Option<Result<BTreeSet<String>, ()>>,
    native: Option<Result<NativeCapabilityReport, ()>>,
}

fn runtime_observation(
    target: &NodeExecutionTarget,
    probes: &RuntimeProbeSet,
) -> RuntimeObservation {
    let result = match target
        .implementation
        .as_ref()
        .expect("validated node target implementation")
    {
        node_execution_target::Implementation::Simulated(_) => available_runtime(),
        node_execution_target::Implementation::Docker(configuration) => {
            qualify_docker_target(target, configuration, probes)
        }
        node_execution_target::Implementation::Native(_) => qualify_native_target(target, probes),
    };

    RuntimeObservation {
        execution_target_id: target.execution_target_id.clone(),
        kind: target.kind,
        result: Some(result),
    }
}

fn qualify_native_target(
    target: &NodeExecutionTarget,
    probes: &RuntimeProbeSet,
) -> runtime_observation::Result {
    let Some(Ok(native)) = probes.native.as_ref() else {
        return unavailable_runtime(
            "RUNTIME_PROBE_FAILED",
            "The native runtime probe did not complete on this node.",
        );
    };
    let required_platform = ExecutionPlatform::try_from(
        target
            .platform
            .as_ref()
            .expect("validated node target platform"),
    )
    .expect("validated canonical node target platform");

    match native.qualify(&required_platform) {
        Ok(()) => available_runtime(),
        Err(failure) if failure.platform_mismatch => {
            unsupported_runtime(failure.code, &failure.message)
        }
        Err(failure) => unavailable_runtime(failure.code, &failure.message),
    }
}

fn qualify_docker_target(
    target: &NodeExecutionTarget,
    configuration: &dotcore_proto::dotcore::node::v1::DockerRuntimeObservationConfiguration,
    probes: &RuntimeProbeSet,
) -> runtime_observation::Result {
    let Some(Ok(docker)) = probes.docker.as_ref() else {
        return unavailable_runtime(
            "RUNTIME_PROBE_FAILED",
            "The configured runtime infrastructure is unavailable.",
        );
    };
    let required_platform = ExecutionPlatform::try_from(
        target
            .platform
            .as_ref()
            .expect("validated node target platform"),
    )
    .expect("validated canonical node target platform");
    if docker.execution_platform() != &required_platform {
        return unsupported_runtime(
            "PLATFORM_UNSUPPORTED",
            "The runtime infrastructure platform cannot support this execution target.",
        );
    }

    if configuration.accelerator_transport == DockerAcceleratorTransport::Cdi as i32 {
        if !docker.cdi_enabled() {
            return unavailable_runtime(
                "CDI_UNAVAILABLE",
                "Docker CDI device injection is unavailable for this execution target.",
            );
        }
        let Some(Ok(selectors)) = probes.cdi_selectors.as_ref() else {
            return unavailable_runtime(
                "CDI_PROBE_FAILED",
                "Docker CDI device discovery is unavailable for this execution target.",
            );
        };
        if configuration
            .accelerator_device_ids
            .iter()
            .any(|selector| !selectors.contains(selector))
        {
            return unavailable_runtime(
                "CDI_DEVICE_UNAVAILABLE",
                "A configured Docker CDI device is unavailable for this execution target.",
            );
        }
    }

    available_runtime()
}

fn available_runtime() -> runtime_observation::Result {
    runtime_observation::Result::Available(Empty {})
}

fn unsupported_runtime(code: &str, message: &str) -> runtime_observation::Result {
    runtime_observation::Result::Unsupported(ObservationUnavailable {
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

fn unavailable_runtime(code: &str, message: &str) -> runtime_observation::Result {
    runtime_observation::Result::Unavailable(ObservationUnavailable {
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

/// Runs `future` until it completes or `timeout` passes on `clock`; `None` when it timed out, in
/// which case the future is dropped.
pub(super) async fn within<F: Future>(
    clock: &dyn Clock,
    timeout: Duration,
    future: F,
) -> Option<F::Output> {
    let deadline = clock.now() + timeout;
    tokio::select! {
        output = future => Some(output),
        () = clock.sleep_until(deadline) => None,
    }
}

fn unavailable_hardware(code: &str, message: &str) -> NodeHardwareObservation {
    NodeHardwareObservation {
        result: Some(node_hardware_observation::Result::Unavailable(
            ObservationUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        )),
    }
}

#[cfg(test)]
mod tests;
