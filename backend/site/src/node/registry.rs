//! Retains the infrastructure observations every SiteNode reports to the manager, the manager's
//! own node included, with each node's liveness and the age of its latest observation.
//!
//! The registry validates every report against current authoritative site topology, fences replaced
//! node process incarnations, and keeps observations as derived state rather than desired site
//! data. It is also the one source of execution-target capabilities for manager admission.

use std::{collections::BTreeSet, sync::Arc};

use dotcore_domain::{SiteNodeId, SiteNodeIdError};
use dotcore_node_hardware::hardware::timestamp_from;
use dotcore_proto::{
    dotcore::{
        observation::v1::{
            AcceleratorFacts, AcceleratorVendor, HardwareSnapshot, ObservationUnavailable,
            accelerator_observation, host_observation, runtime_observation,
        },
        site::private::v1::{
            NodeObservation, NodeObservationAcceptance, NodeObservationSnapshot,
            node_hardware_observation,
        },
    },
    google::protobuf::Timestamp,
};

use crate::{
    node::{
        control::{
            AcceptedNodeObservationInput, CoordinatorConnectionState, ManagerNodeControlCoordinator,
        },
        observation_configuration::{
            NodeObservationConfigurationDigest, NodeObservationConfigurationError,
            node_observation_configuration,
        },
        target_inventory::{TargetCapabilityInventory, observation_is_fresh},
    },
    repository::SiteRepositoryError,
    service::SiteConfigurationSource,
    view::fencing::{IncarnationFenceError, NodeIncarnation},
};

/// Manager-owned derived state for authenticated node observations.
#[derive(Clone)]
pub struct NodeRegistry {
    site: Arc<dyn SiteConfigurationSource>,
    coordinator: ManagerNodeControlCoordinator,
}

impl NodeRegistry {
    /// Creates an empty registry backed by current authoritative site configuration.
    #[cfg(test)]
    pub(crate) fn new(site: Arc<dyn SiteConfigurationSource>) -> Self {
        let manager_session =
            crate::view::fencing::ManagerSession::generate().expect("manager test session");
        Self::with_coordinator(
            site,
            ManagerNodeControlCoordinator::on_system_clock(manager_session),
        )
    }

    /// Creates an empty registry stamping receipt times with `coordinator`'s clock.
    pub fn with_coordinator(
        site: Arc<dyn SiteConfigurationSource>,
        coordinator: ManagerNodeControlCoordinator,
    ) -> Self {
        Self { site, coordinator }
    }

    /// Validates and atomically retains one complete node observation revision.
    pub(crate) fn accept(
        &self,
        request: NodeObservation,
    ) -> Result<NodeObservationAcceptance, NodeRegistryError> {
        self.accept_from(request, None)
    }

    /// Validates and retains one observation carried by the node control stream of `epoch`,
    /// refused once that stream is no longer its incarnation's open one.
    pub(crate) fn accept_on_stream(
        &self,
        request: NodeObservation,
        epoch: u64,
    ) -> Result<NodeObservationAcceptance, NodeRegistryError> {
        self.accept_from(request, Some(epoch))
    }

    fn accept_from(
        &self,
        request: NodeObservation,
        stream_epoch: Option<u64>,
    ) -> Result<NodeObservationAcceptance, NodeRegistryError> {
        if let (Ok(node_id), Ok(incarnation)) = (
            SiteNodeId::parse(&request.node_id),
            NodeIncarnation::parse(request.incarnation_id.clone()),
        ) && let Some(response) = self
            .coordinator
            .replay_observation(node_id.as_str(), &incarnation, &request)
            .map_err(NodeRegistryError::from_incarnation_fence)?
        {
            return Ok(response);
        }
        let validated = self.validate(&request)?;
        let input = AcceptedNodeObservationInput {
            node_id: validated.node_id.as_str().to_owned(),
            incarnation: validated.incarnation,
            revision: validated.revision,
            snapshot: validated.snapshot,
            observation_configuration_sha256: validated.observation_configuration_sha256,
            received_at: timestamp_from(self.coordinator.clock().wall_now()),
            private_port: validated.private_port,
        };
        let (response, _) = match stream_epoch {
            Some(epoch) => self
                .coordinator
                .register_stream_observation(&request, input, epoch),
            None => self.coordinator.register_new_observation(&request, input),
        }
        .map_err(NodeRegistryError::from_incarnation_fence)?;
        Ok(response)
    }

    /// Returns the retained observation and receiver-derived connection state for one node.
    pub(crate) fn observation(&self, node_id: &SiteNodeId) -> Option<NodeObservationView> {
        let current = self.coordinator.observation(node_id)?;
        Some(NodeObservationView {
            node_id: node_id.clone(),
            incarnation_id: current.incarnation.as_str().to_owned(),
            revision: current.revision,
            snapshot: current.snapshot,
            observation_configuration_sha256: current.observation_configuration_sha256,
            received_at: current.received_at,
            connection_state: match current.connection_state {
                CoordinatorConnectionState::Connected => NodeConnectionState::Connected,
                CoordinatorConnectionState::Stale => NodeConnectionState::Stale,
                CoordinatorConnectionState::Disconnected => NodeConnectionState::Disconnected,
            },
            replaced_incarnations: current
                .replaced_incarnations
                .into_iter()
                .map(|incarnation| incarnation.as_str().to_owned())
                .collect(),
        })
    }

    /// Projects every node's current report, the manager's own node included, into exact
    /// target-attributed admission capabilities. Missing reports remain absent, and stale or
    /// unavailable outcomes never become usable values.
    pub fn target_capabilities(
        &self,
    ) -> Result<
        crate::node::target_inventory::TargetCapabilityInventory,
        crate::node::target_inventory::TargetCapabilitySourceError,
    > {
        let site = self.site.snapshot().map_err(|source| {
            crate::node::target_inventory::TargetCapabilitySourceError::Observation(
                anyhow::Error::new(source).context("read site topology for node admission"),
            )
        })?;
        let max_age = self.coordinator.timing().capability_max_age;
        let now = self.coordinator.clock().wall_now();
        let mut inventory = TargetCapabilityInventory::empty(max_age, now);
        for node in &site.nodes {
            let node_id = SiteNodeId::parse(&node.node_id).map_err(|source| {
                crate::node::target_inventory::TargetCapabilitySourceError::Observation(
                    anyhow::Error::new(source).context("read node identity for admission"),
                )
            })?;
            let Some(observation) = self.observation(&node_id) else {
                continue;
            };
            let stale = observation.connection_state != NodeConnectionState::Connected;
            let observed_at =
                crate::node::target_inventory::timestamp_to_system_time(&observation.received_at);
            for target in site
                .execution_targets
                .iter()
                .filter(|target| target.site_node_id == node.node_id)
            {
                inventory.extend(crate::node::capabilities::node_target_capabilities(
                    target,
                    &observation.snapshot,
                    stale,
                    observed_at,
                    max_age,
                    now,
                )?);
            }
        }
        Ok(inventory)
    }

    /// Reads every site node's memory from its current report, in site order, whether or not it
    /// has execution targets. A node that never reported reads as unavailable, and a report from
    /// a node that isn't connected or that is older than the maximum capability age reads as
    /// stale.
    pub fn node_memory(
        &self,
    ) -> Result<
        Vec<crate::node::target_inventory::NodeMemoryObservation>,
        crate::node::target_inventory::TargetCapabilitySourceError,
    > {
        let site = self.site.snapshot().map_err(|source| {
            crate::node::target_inventory::TargetCapabilitySourceError::Observation(
                anyhow::Error::new(source).context("read site topology for node memory"),
            )
        })?;
        let now = self.coordinator.clock().wall_now();
        site.nodes
            .iter()
            .map(|node| {
                let node_id = SiteNodeId::parse(&node.node_id).map_err(|source| {
                    crate::node::target_inventory::TargetCapabilitySourceError::Observation(
                        anyhow::Error::new(source).context("read node identity for node memory"),
                    )
                })?;
                let Some(observation) = self.observation(&node_id) else {
                    return Ok(
                        crate::node::target_inventory::NodeMemoryObservation::unavailable(
                            node.node_id.clone(),
                        ),
                    );
                };
                let observed_at = crate::node::target_inventory::timestamp_to_system_time(
                    &observation.received_at,
                );
                let stale = observation.connection_state != NodeConnectionState::Connected
                    || !observation_is_fresh(
                        observed_at,
                        now,
                        self.coordinator.timing().capability_max_age,
                    );
                Ok(crate::node::capabilities::node_memory_observation(
                    &node.node_id,
                    &observation.snapshot,
                    stale,
                    observed_at,
                ))
            })
            .collect()
    }

    fn validate(
        &self,
        request: &NodeObservation,
    ) -> Result<ValidatedNodeObservation, NodeRegistryError> {
        let site = self.site.snapshot().map_err(NodeRegistryError::Site)?;
        if request.site_id != site.site_id {
            return Err(NodeRegistryError::SiteIdentityMismatch);
        }
        let node_id =
            SiteNodeId::parse(&request.node_id).map_err(NodeRegistryError::InvalidNodeIdentity)?;
        if !site
            .nodes
            .iter()
            .any(|node| node.node_id == request.node_id)
        {
            return Err(NodeRegistryError::UndeclaredNode);
        }
        let incarnation = NodeIncarnation::parse(request.incarnation_id.clone())
            .map_err(|_| NodeRegistryError::InvalidIncarnation)?;
        if request.revision == 0 {
            return Err(NodeRegistryError::InvalidRevision);
        }

        let configuration = node_observation_configuration(&site, &request.node_id)
            .map_err(NodeRegistryError::Configuration)?;
        let expected_digest = NodeObservationConfigurationDigest::for_configuration(&configuration)
            .map_err(NodeRegistryError::Configuration)?;
        if request.observation_configuration_sha256 != expected_digest.as_str() {
            return Err(NodeRegistryError::ConfigurationDigestMismatch);
        }
        let snapshot = request
            .snapshot
            .clone()
            .ok_or(NodeRegistryError::InvalidReport("snapshot is required"))?;
        validate_snapshot(&snapshot, &configuration.execution_targets)?;
        let private_port = request
            .private_port
            .map(|port| {
                u16::try_from(port).ok().filter(|port| *port != 0).ok_or(
                    NodeRegistryError::InvalidReport("private port must be within 1..=65535"),
                )
            })
            .transpose()?;
        Ok(ValidatedNodeObservation {
            node_id,
            incarnation,
            revision: request.revision,
            snapshot,
            observation_configuration_sha256: expected_digest,
            private_port,
        })
    }
}

/// Receiver-derived connection state for the current node process incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeConnectionState {
    /// Live, with a current observation.
    Connected,

    /// Live, but its latest observation is overdue.
    Stale,

    /// No authenticated contact within the liveness window.
    Disconnected,
}

/// Current retained node observation together with freshness and replacement evidence.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeObservationView {
    pub node_id: SiteNodeId,
    pub incarnation_id: String,
    pub revision: u64,
    pub snapshot: NodeObservationSnapshot,
    pub observation_configuration_sha256: NodeObservationConfigurationDigest,
    pub received_at: Timestamp,
    pub connection_state: NodeConnectionState,
    pub replaced_incarnations: BTreeSet<String>,
}

struct ValidatedNodeObservation {
    node_id: SiteNodeId,
    incarnation: NodeIncarnation,
    revision: u64,
    snapshot: NodeObservationSnapshot,
    observation_configuration_sha256: NodeObservationConfigurationDigest,
    private_port: Option<u16>,
}

/// Rejection of a node observation before derived registry state changes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NodeRegistryError {
    #[error("node observation site identity does not match this site")]
    SiteIdentityMismatch,

    #[error("node observation node identity is invalid")]
    InvalidNodeIdentity(#[source] SiteNodeIdError),

    #[error("node observation node is not declared in this site")]
    UndeclaredNode,

    #[error("node observation process incarnation is invalid")]
    InvalidIncarnation,

    #[error("node observation revision must be nonzero")]
    InvalidRevision,

    #[error("node observation configuration does not match current site topology")]
    ConfigurationDigestMismatch,

    #[error("node observation is incomplete: {0}")]
    InvalidReport(&'static str),

    #[error("node observation revision {received} is not newer than accepted revision {accepted}")]
    StaleRevision { accepted: u64, received: u64 },

    #[error("another node process incarnation is still connected")]
    CurrentIncarnationFresh,

    #[error("node process incarnation has been replaced")]
    ReplacedIncarnation,

    #[error("current site configuration is unavailable")]
    Site(#[source] SiteRepositoryError),

    #[error("current node observation configuration is invalid")]
    Configuration(#[source] NodeObservationConfigurationError),
}

impl NodeRegistryError {
    fn from_incarnation_fence(error: IncarnationFenceError) -> Self {
        match error {
            IncarnationFenceError::InvalidSequence => Self::InvalidRevision,
            IncarnationFenceError::CurrentIncarnationActive => Self::CurrentIncarnationFresh,
            IncarnationFenceError::UnregisteredIncarnation => {
                Self::InvalidReport("node incarnation is not registered")
            }
            IncarnationFenceError::ReplacedIncarnation => Self::ReplacedIncarnation,
            IncarnationFenceError::ConflictingSequence => {
                Self::InvalidReport("node lifecycle sequence content conflicts")
            }
            IncarnationFenceError::UnofferedView => {
                Self::InvalidReport("node observation cannot acknowledge a desired view")
            }
            IncarnationFenceError::StreamReplaced => Self::ReplacedIncarnation,
            IncarnationFenceError::StaleSequence { accepted, received } => {
                Self::StaleRevision { accepted, received }
            }
        }
    }
}

fn validate_snapshot(
    snapshot: &NodeObservationSnapshot,
    expected_targets: &[dotcore_proto::dotcore::node::v1::NodeExecutionTarget],
) -> Result<(), NodeRegistryError> {
    let hardware = snapshot
        .hardware
        .as_ref()
        .ok_or(NodeRegistryError::InvalidReport(
            "hardware outcome is required",
        ))?;
    match hardware.result.as_ref() {
        Some(node_hardware_observation::Result::Snapshot(snapshot)) => {
            validate_hardware_snapshot(snapshot)?
        }
        Some(node_hardware_observation::Result::Unavailable(reason)) => validate_unavailable(
            reason,
            "hardware unavailability requires a code and message",
        )?,
        None => {
            return Err(NodeRegistryError::InvalidReport(
                "hardware outcome result is required",
            ));
        }
    }

    if snapshot.runtimes.len() != expected_targets.len() {
        return Err(NodeRegistryError::InvalidReport(
            "one runtime outcome is required per configured target",
        ));
    }
    for (runtime, target) in snapshot.runtimes.iter().zip(expected_targets) {
        if runtime.execution_target_id != target.execution_target_id || runtime.kind != target.kind
        {
            return Err(NodeRegistryError::InvalidReport(
                "Runtime outcomes must match configured target order and kind",
            ));
        }
        match runtime.result.as_ref() {
            Some(runtime_observation::Result::Available(_)) => {}
            Some(runtime_observation::Result::Unsupported(reason))
            | Some(runtime_observation::Result::Unavailable(reason)) => {
                validate_unavailable(reason, "Runtime unavailability requires a code and message")?
            }
            None => {
                return Err(NodeRegistryError::InvalidReport(
                    "each runtime outcome requires a result",
                ));
            }
        }
    }
    Ok(())
}

fn validate_hardware_snapshot(snapshot: &HardwareSnapshot) -> Result<(), NodeRegistryError> {
    if snapshot.observed_at.is_none() {
        return Err(NodeRegistryError::InvalidReport(
            "hardware snapshot observation time is required",
        ));
    }
    let host = snapshot
        .host
        .as_ref()
        .ok_or(NodeRegistryError::InvalidReport(
            "host observation is required",
        ))?;
    match host.result.as_ref() {
        Some(host_observation::Result::Facts(facts)) => {
            if facts.host_name.trim().is_empty()
                || facts.operating_system_or_kernel.trim().is_empty()
                || facts.cpu_count == 0
                || facts.total_memory_bytes == 0
                || facts.available_memory_bytes > facts.total_memory_bytes
                // A host may list no filesystem, but each listed one names its metric labels.
                || facts.filesystems.iter().any(|filesystem| {
                    filesystem.mountpoint.is_empty() || filesystem.fstype.is_empty()
                })
            {
                return Err(NodeRegistryError::InvalidReport(
                    "host facts are incomplete or inconsistent",
                ));
            }
        }
        Some(host_observation::Result::Unavailable(reason)) => {
            validate_unavailable(reason, "host unavailability requires a code and message")?
        }
        None => {
            return Err(NodeRegistryError::InvalidReport(
                "host observation result is required",
            ));
        }
    }

    let mut vendors = BTreeSet::new();
    for accelerator in &snapshot.accelerators {
        let vendor = AcceleratorVendor::try_from(accelerator.vendor)
            .ok()
            .filter(|vendor| *vendor != AcceleratorVendor::Unspecified)
            .ok_or(NodeRegistryError::InvalidReport(
                "accelerator observation vendor is required",
            ))?;
        if !vendors.insert(vendor) {
            return Err(NodeRegistryError::InvalidReport(
                "accelerator observation vendors must be unique",
            ));
        }
        match accelerator.result.as_ref() {
            Some(accelerator_observation::Result::Facts(facts)) => {
                let mut device_ids = BTreeSet::new();
                for fact in &facts.facts {
                    validate_accelerator_fact(vendor, fact, &mut device_ids)?;
                }
            }
            Some(accelerator_observation::Result::Unavailable(reason)) => validate_unavailable(
                reason,
                "accelerator unavailability requires a code and message",
            )?,
            None => {
                return Err(NodeRegistryError::InvalidReport(
                    "accelerator observation result is required",
                ));
            }
        }
    }
    Ok(())
}

fn validate_accelerator_fact<'a>(
    vendor: AcceleratorVendor,
    fact: &'a AcceleratorFacts,
    device_ids: &mut BTreeSet<&'a str>,
) -> Result<(), NodeRegistryError> {
    if fact.vendor != vendor as i32 {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact vendor must match its observation vendor",
        ));
    }
    if fact.model.trim().is_empty() {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact model is required",
        ));
    }
    if vendor_reports_driver(vendor) && fact.driver_or_runtime_version.trim().is_empty() {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact driver or runtime version is required",
        ));
    }
    if fact.device_id.trim().is_empty() {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact device id is required",
        ));
    }
    if !device_ids.insert(fact.device_id.as_str()) {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact device ids must be unique",
        ));
    }
    if matches!(
        (fact.used_memory_bytes, fact.total_memory_bytes),
        (Some(used), Some(total)) if used > total
    ) {
        return Err(NodeRegistryError::InvalidReport(
            "accelerator fact used memory must not exceed total memory",
        ));
    }

    Ok(())
}

/// Whether the vendor's accelerators carry a separate driver or runtime version. Apple GPUs do
/// not; Metal ships with the operating system.
fn vendor_reports_driver(vendor: AcceleratorVendor) -> bool {
    vendor != AcceleratorVendor::Apple
}

fn validate_unavailable(
    reason: &ObservationUnavailable,
    message: &'static str,
) -> Result<(), NodeRegistryError> {
    if reason.code.trim().is_empty() || reason.message.trim().is_empty() {
        return Err(NodeRegistryError::InvalidReport(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use dotcore_domain::{
        clock::{Clock, ClockSleep},
        timing::Timing,
    };
    use dotcore_proto::{
        dotcore::{
            catalog::v1::OciPlatform,
            observation::v1::{
                AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
                AcceleratorObservation, AcceleratorVendor, FilesystemFacts, HardwareSnapshot,
                HostFacts, HostObservation, ObservationUnavailable, RuntimeObservation,
                accelerator_observation, host_observation, runtime_observation,
            },
            site::private::v1::{
                NodeHardwareObservation, NodeObservation, NodeObservationSnapshot,
                node_hardware_observation,
            },
            site::v1::{
                ApplicationRuntimeBinding, ExecutionTarget, HardwareObservationConfiguration,
                ModelRuntimeBinding, SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter,
                SimulatedRuntimeScenario, Site, SiteManagerRole, SiteNode,
                application_runtime_binding, execution_target, model_runtime_binding,
            },
        },
        google::protobuf::{Empty, Timestamp},
    };
    use tokio::time::advance;

    use super::*;
    use crate::node::target_inventory::{
        CapabilityObservation, NodeMemoryObservation, TargetCapabilityInventory,
    };

    #[tokio::test(start_paused = true)]
    async fn accepts_newer_revisions_replays_exact_requests_and_rejects_mismatches() {
        let site = test_site();
        let registry = test_registry(site.clone());
        let accepted = registry
            .accept(report(&site, incarnation('1'), 1))
            .expect("first report accepted");
        assert_eq!(accepted.accepted_revision, 1);
        assert_eq!(
            accepted.received_at,
            Some(Timestamp {
                seconds: 123,
                nanos: 456
            })
        );

        assert_eq!(
            registry
                .accept(report(&site, incarnation('1'), 1))
                .expect("exact retry"),
            accepted
        );
        let mut mismatched = report(&site, incarnation('1'), 2);
        mismatched.observation_configuration_sha256 = "0".repeat(64);
        assert!(matches!(
            registry.accept(mismatched),
            Err(NodeRegistryError::ConfigurationDigestMismatch)
        ));
        let mut incomplete = report(&site, incarnation('1'), 2);
        incomplete
            .snapshot
            .as_mut()
            .expect("test snapshot")
            .hardware = None;
        assert!(matches!(
            registry.accept(incomplete),
            Err(NodeRegistryError::InvalidReport(
                "hardware outcome is required"
            ))
        ));
        let mut malformed = report(&site, incarnation('1'), 2);
        malformed
            .snapshot
            .as_mut()
            .expect("test snapshot")
            .hardware
            .as_mut()
            .expect("test hardware")
            .result = Some(node_hardware_observation::Result::Snapshot(
            HardwareSnapshot::default(),
        ));
        assert!(matches!(
            registry.accept(malformed),
            Err(NodeRegistryError::InvalidReport(
                "hardware snapshot observation time is required"
            ))
        ));
        let mut malformed_facts = report(&site, incarnation('1'), 2);
        malformed_facts
            .snapshot
            .as_mut()
            .expect("test snapshot")
            .hardware
            .as_mut()
            .expect("test hardware")
            .result = Some(node_hardware_observation::Result::Snapshot(
            HardwareSnapshot {
                observed_at: Some(Timestamp {
                    seconds: 100,
                    nanos: 0,
                }),
                host: Some(HostObservation {
                    result: Some(host_observation::Result::Facts(HostFacts::default())),
                }),
                accelerators: Vec::new(),
            },
        ));
        assert!(matches!(
            registry.accept(malformed_facts),
            Err(NodeRegistryError::InvalidReport(
                "host facts are incomplete or inconsistent"
            ))
        ));

        let retained = registry
            .observation(&SiteNodeId::for_test("worker-node"))
            .expect("accepted observation retained");
        assert_eq!(retained.revision, 1);
        assert_eq!(retained.incarnation_id, incarnation('1'));
        assert_eq!(retained.connection_state, NodeConnectionState::Connected);
        assert!(retained.replaced_incarnations.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn private_port_is_retained_only_within_the_valid_range() {
        let site = test_site();
        let registry = test_registry(site.clone());
        for invalid in [0, 65_536, u32::MAX] {
            let mut report = report(&site, incarnation('1'), 1);
            report.private_port = Some(invalid);
            assert!(matches!(
                registry.accept(report),
                Err(NodeRegistryError::InvalidReport(
                    "private port must be within 1..=65535"
                ))
            ));
        }
        let retained_port = || {
            registry
                .coordinator
                .serving_evidence()
                .into_iter()
                .map(|node| (node.node_id, node.private_port))
                .collect::<Vec<_>>()
        };
        assert!(retained_port().is_empty());

        let mut with_port = report(&site, incarnation('1'), 1);
        with_port.private_port = Some(65_535);
        registry.accept(with_port).expect("valid port accepted");
        assert_eq!(retained_port(), [("worker-node".to_owned(), Some(65_535))]);

        registry
            .accept(report(&site, incarnation('1'), 2))
            .expect("report without a port accepted");
        assert_eq!(retained_port(), [("worker-node".to_owned(), None)]);
    }

    #[tokio::test(start_paused = true)]
    async fn retained_remote_capabilities_admit_both_linux_architectures_without_local_probes() {
        for architecture in ["amd64", "arm64"] {
            let mut site = test_site();
            site.execution_targets
                .iter_mut()
                .find(|target| target.execution_target_id == "model-target")
                .expect("model target")
                .platform
                .as_mut()
                .expect("target platform")
                .architecture = architecture.to_owned();
            let registry = admission_registry(site.clone());
            let accepted = registry
                .accept(report(&site, incarnation('1'), 1))
                .expect("node report accepted");

            let inventory = registry
                .target_capabilities()
                .expect("retained node target capabilities");
            let capabilities = inventory
                .for_target("model-target")
                .expect("node target capabilities");
            let received_at = crate::node::target_inventory::timestamp_to_system_time(
                accepted.received_at.as_ref().expect("manager receipt time"),
            )
            .expect("valid manager receipt time");
            assert_eq!(capabilities.platform_observed_at, Some(received_at));
            assert_eq!(capabilities.accelerator_observed_at, Some(received_at));
            assert!(inventory.is_fresh(capabilities.platform_observed_at));
            assert!(matches!(
                &capabilities.platform,
                CapabilityObservation::Available(platform) if platform.architecture() == architecture
            ));
            assert_eq!(
                capabilities.accelerator,
                CapabilityObservation::Available(dotcore_domain::Accelerator::Nvidia)
            );
            assert!(matches!(
                &capabilities.nvidia_driver,
                CapabilityObservation::Available(driver) if driver.major() == 580
            ));
            assert_eq!(
                capabilities.accelerator_memory,
                CapabilityObservation::Available(dotcore_domain::MemoryArchitecture::Dedicated)
            );
            assert!(inventory.for_target("manager-application").is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retained_remote_capabilities_fail_closed_for_missing_stale_and_unsupported_evidence() {
        let mut site = test_site();
        site.execution_targets
            .iter_mut()
            .find(|target| target.execution_target_id == "model-target")
            .expect("model target")
            .runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                    dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter {
                        accelerator_transport:
                            dotcore_proto::dotcore::site::v1::DockerAcceleratorTransport::Cdi as i32,
                    },
                )),
            },
        ));
        let model_target = |inventory: &TargetCapabilityInventory| {
            inventory
                .for_target("model-target")
                .expect("model target capabilities")
                .clone()
        };
        let missing = admission_registry(site.clone())
            .target_capabilities()
            .expect("missing inventory");
        assert!(missing.for_target("model-target").is_none());

        let no_driver_registry = admission_registry(site.clone());
        let mut no_driver = report(&site, incarnation('3'), 1);
        no_driver
            .snapshot
            .as_mut()
            .expect("snapshot")
            .hardware
            .as_mut()
            .expect("hardware")
            .result
            .as_mut()
            .and_then(|result| match result {
                node_hardware_observation::Result::Snapshot(snapshot) => Some(snapshot),
                node_hardware_observation::Result::Unavailable(_) => None,
            })
            .expect("hardware snapshot")
            .accelerators[0]
            .result = Some(accelerator_observation::Result::Unavailable(
            ObservationUnavailable {
                code: "NVIDIA_DRIVER_UNAVAILABLE".to_owned(),
                message: "The NVIDIA driver is unavailable.".to_owned(),
            },
        ));
        no_driver_registry
            .accept(no_driver)
            .expect("report without driver retained");
        let no_driver_inventory = no_driver_registry
            .target_capabilities()
            .expect("inventory without driver");
        assert_eq!(
            model_target(&no_driver_inventory).accelerator,
            CapabilityObservation::Unavailable
        );

        let no_memory_registry = admission_registry(site.clone());
        let mut no_memory = report(&site, incarnation('4'), 1);
        no_memory
            .snapshot
            .as_mut()
            .expect("snapshot")
            .hardware
            .as_mut()
            .expect("hardware")
            .result
            .as_mut()
            .and_then(|result| match result {
                node_hardware_observation::Result::Snapshot(snapshot) => Some(snapshot),
                node_hardware_observation::Result::Unavailable(_) => None,
            })
            .expect("hardware snapshot")
            .accelerators[0]
            .result
            .as_mut()
            .and_then(|result| match result {
                accelerator_observation::Result::Facts(facts) => Some(facts),
                accelerator_observation::Result::Unavailable(_) => None,
            })
            .expect("accelerator facts")
            .facts[0]
            .memory_architecture = AcceleratorMemoryArchitecture::Unspecified as i32;
        no_memory_registry
            .accept(no_memory)
            .expect("report without memory classification retained");
        let no_memory_inventory = no_memory_registry
            .target_capabilities()
            .expect("inventory without memory classification");
        assert_eq!(
            model_target(&no_memory_inventory).accelerator_memory,
            CapabilityObservation::Unavailable
        );

        let registry = admission_registry(site.clone());
        registry
            .accept(report(&site, incarnation('1'), 1))
            .expect("node report accepted");
        advance(Duration::from_secs(10) + Duration::from_millis(1)).await;
        let stale = registry.target_capabilities().expect("stale inventory");
        assert!(matches!(
            model_target(&stale).platform,
            CapabilityObservation::Stale(_)
        ));

        let unsupported_registry = admission_registry(site.clone());
        let mut unsupported = report(&site, incarnation('2'), 1);
        unsupported
            .snapshot
            .as_mut()
            .expect("snapshot")
            .runtimes
            .iter_mut()
            .find(|runtime| runtime.execution_target_id == "model-target")
            .expect("model runtime")
            .result = Some(runtime_observation::Result::Unsupported(
            ObservationUnavailable {
                code: "RUNTIME_UNSUPPORTED".to_owned(),
                message: "The configured runtime is unsupported.".to_owned(),
            },
        ));
        unsupported_registry
            .accept(unsupported)
            .expect("unsupported report retained");
        let unsupported_inventory = unsupported_registry
            .target_capabilities()
            .expect("unsupported inventory");
        assert_eq!(
            model_target(&unsupported_inventory).platform,
            CapabilityObservation::Unavailable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_manager_node_registers_like_any_node_and_admits_its_own_targets() {
        let mut site = test_site();
        site.execution_targets.push(ExecutionTarget {
            execution_target_id: "manager-model".to_owned(),
            display_name: "manager-model".to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: vec![GB10_GPU_ID.to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        simulated_runtime(),
                    )),
                },
            )),
        });
        let registry = admission_registry(site.clone());
        // The manager node declares a GB10, so its report is that machine's rendered snapshot.
        let mut manager_report = report_for(&site, "manager-node", incarnation('1'), 1);
        manager_report.snapshot.as_mut().expect("snapshot").hardware =
            Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Snapshot(
                    dotcore_node_hardware::MachineDescription::named("gb10")
                        .expect("named machine")
                        .render(&[], SystemTime::now())
                        .expect("rendered snapshot"),
                )),
            });
        let expected_digest = NodeObservationConfigurationDigest::for_configuration(
            &node_observation_configuration(&site, "manager-node")
                .expect("manager node observation configuration"),
        )
        .expect("manager node observation digest");

        let accepted = registry
            .accept(manager_report)
            .expect("manager node report accepted");

        assert_eq!(accepted.accepted_revision, 1);
        let retained = registry
            .observation(&SiteNodeId::for_test("manager-node"))
            .expect("manager node observation retained");
        assert_eq!(retained.observation_configuration_sha256, expected_digest);
        assert_eq!(retained.connection_state, NodeConnectionState::Connected);
        let inventory = registry
            .target_capabilities()
            .expect("node target capabilities");
        // The simulated target reads the declared GB10: unified NVIDIA memory over the host total.
        let capabilities = inventory
            .for_target("manager-model")
            .expect("manager model target capabilities");
        assert!(inventory.is_fresh(capabilities.platform_observed_at));
        assert!(matches!(
            &capabilities.platform,
            CapabilityObservation::Available(platform) if platform.architecture() == "arm64"
        ));
        assert_eq!(
            capabilities.accelerator,
            CapabilityObservation::Available(dotcore_domain::Accelerator::Nvidia)
        );
        assert!(matches!(
            &capabilities.nvidia_driver,
            CapabilityObservation::Available(driver) if driver.major() == 580
        ));
        assert_eq!(
            capabilities.accelerator_memory,
            CapabilityObservation::Available(dotcore_domain::MemoryArchitecture::Unified)
        );
        assert!(matches!(
            &capabilities.memory,
            CapabilityObservation::Available(memory) if memory.host
                == Some(crate::node::target_inventory::HostMemoryFacts {
                    total_bytes: GB10_HOST_TOTAL_BYTES,
                    available_bytes: GB10_HOST_TOTAL_BYTES - GB10_IDLE_USED_BYTES,
                })
        ));
        assert!(inventory.for_target("manager-application").is_some());
        assert!(inventory.for_target("model-target").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_and_incarnation_fencing_are_receiver_timed() {
        let site = test_site();
        let registry = test_registry(site.clone());
        let state = || {
            registry
                .observation(&SiteNodeId::for_test("worker-node"))
                .expect("observation")
                .connection_state
        };
        registry
            .accept(report(&site, incarnation('1'), 1))
            .expect("first report accepted");

        advance(Timing::test().node_control.node_silence).await;
        assert_eq!(state(), NodeConnectionState::Connected);
        assert!(matches!(
            registry.accept(report(&site, incarnation('2'), 1)),
            Err(NodeRegistryError::CurrentIncarnationFresh)
        ));

        // Silence past the liveness window disconnects at once, with no Stale step in between.
        advance(Duration::from_millis(1)).await;
        assert_eq!(state(), NodeConnectionState::Disconnected);
        registry
            .accept(report(&site, incarnation('2'), 1))
            .expect("silent incarnation replaced atomically");
        assert!(matches!(
            registry.accept(report(&site, incarnation('1'), 2)),
            Err(NodeRegistryError::ReplacedIncarnation)
        ));

        let replacement = registry
            .observation(&SiteNodeId::for_test("worker-node"))
            .expect("replacement retained");
        assert_eq!(replacement.incarnation_id, incarnation('2'));
        assert_eq!(
            replacement.replaced_incarnations,
            [incarnation('1')].into_iter().collect()
        );
        assert_eq!(replacement.connection_state, NodeConnectionState::Connected);
        advance(Timing::test().node_control.node_silence + Duration::from_millis(1)).await;
        assert_eq!(state(), NodeConnectionState::Disconnected);
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_node_with_an_overdue_observation_is_stale_and_refused_admission() {
        let site = test_site();
        let registry = admission_registry(site.clone());
        registry
            .accept(report(&site, incarnation('1'), 1))
            .expect("node report accepted");
        let node = NodeIncarnation::parse(incarnation('1')).expect("incarnation");
        // An hour of view publications without another observation keeps the node live.
        for _ in 1..=720 {
            advance(Duration::from_secs(5)).await;
            registry
                .coordinator
                .publish_view_for_test(
                    "worker-node",
                    &node,
                    crate::node::control::PublishedViewInput {
                        site_id: site.site_id.clone(),
                        projection: crate::view::fencing::NodeViewProjection::new(
                            Vec::new(),
                            Vec::new(),
                        )
                        .expect("empty view"),
                    },
                )
                .expect("node publication");
        }

        assert_eq!(
            registry
                .observation(&SiteNodeId::for_test("worker-node"))
                .expect("observation")
                .connection_state,
            NodeConnectionState::Stale
        );
        let inventory = registry
            .target_capabilities()
            .expect("retained node target capabilities");
        assert!(matches!(
            inventory
                .for_target("model-target")
                .expect("model target capabilities")
                .platform,
            CapabilityObservation::Stale(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn node_memory_lists_every_node_in_site_order_and_a_disconnected_one_as_stale() {
        let site = test_site();
        let registry = test_registry(site.clone());
        registry
            .accept(report(&site, incarnation('1'), 1))
            .expect("node report accepted");
        let received_at = std::time::UNIX_EPOCH + Duration::new(123, 456);
        let worker = |memory: &[NodeMemoryObservation]| {
            let worker = &memory[1];
            assert_eq!(worker.site_node_id, "worker-node");
            assert_eq!(worker.observed_at, Some(received_at));
            worker.reading.clone()
        };
        let devices = |reading: &crate::node::target_inventory::NodeMemoryReading| {
            assert_eq!(reading.host(), None);
            reading
                .devices()
                .map(|device| {
                    (
                        device.memory.device_id.clone(),
                        device.memory.total_bytes,
                        device.memory.used_bytes,
                    )
                })
                .collect::<Vec<_>>()
        };
        let reported = vec![(
            "synthetic-device-0".to_owned(),
            Some(24 * 1024 * 1024 * 1024),
            Some(0),
        )];

        // The manager node never reported, so it is listed with nothing to read.
        let memory = registry.node_memory().expect("node memory");
        assert_eq!(memory.len(), 2);
        assert_eq!(
            memory[0],
            NodeMemoryObservation::unavailable("manager-node")
        );
        let CapabilityObservation::Available(reading) = worker(&memory) else {
            panic!("fresh worker memory");
        };
        assert_eq!(devices(&reading), reported);

        advance(Timing::test().node_control.node_silence + Duration::from_millis(1)).await;
        let memory = registry.node_memory().expect("node memory");
        assert_eq!(
            memory[0],
            NodeMemoryObservation::unavailable("manager-node")
        );
        let CapabilityObservation::Stale(reading) = worker(&memory) else {
            panic!("stale worker memory");
        };
        assert_eq!(devices(&reading), reported);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_past_the_liveness_window_disconnects_at_any_observation_interval() {
        for interval_seconds in [1, 3_600] {
            let mut site = test_site();
            site.hardware_observation = Some(
                dotcore_proto::dotcore::site::v1::HardwareObservationConfiguration {
                    refresh_interval_seconds: Some(interval_seconds),
                },
            );
            let registry = test_registry(site.clone());
            registry
                .accept(report(&site, incarnation('1'), 1))
                .expect("node report accepted");
            let state = || {
                registry
                    .observation(&SiteNodeId::for_test("worker-node"))
                    .expect("observation")
                    .connection_state
            };

            advance(Timing::test().node_control.node_silence).await;
            assert_eq!(
                state(),
                NodeConnectionState::Connected,
                "{interval_seconds}"
            );
            advance(Duration::from_millis(1)).await;
            assert_eq!(
                state(),
                NodeConnectionState::Disconnected,
                "{interval_seconds}"
            );
        }
    }

    #[test]
    fn host_facts_may_list_no_filesystem_but_each_listed_one_is_labeled() {
        let snapshot = |filesystems: Vec<FilesystemFacts>| HardwareSnapshot {
            observed_at: Some(Timestamp {
                seconds: 100,
                nanos: 0,
            }),
            host: Some(HostObservation {
                result: Some(host_observation::Result::Facts(HostFacts {
                    host_name: "worker".to_owned(),
                    operating_system_or_kernel: "NixOS 26.05".to_owned(),
                    cpu_count: 8,
                    total_memory_bytes: 1_024,
                    available_memory_bytes: 512,
                    cached_memory_bytes: None,
                    filesystems,
                })),
            }),
            accelerators: Vec::new(),
        };
        let filesystem = |mountpoint: &str, fstype: &str| FilesystemFacts {
            mountpoint: mountpoint.to_owned(),
            fstype: fstype.to_owned(),
            size_bytes: 1_000,
        };

        assert!(validate_hardware_snapshot(&snapshot(Vec::new())).is_ok());
        assert!(validate_hardware_snapshot(&snapshot(vec![filesystem("/", "ext4")])).is_ok());
        for unlabeled in [filesystem("", "ext4"), filesystem("/", "")] {
            assert!(matches!(
                validate_hardware_snapshot(&snapshot(vec![filesystem("/", "ext4"), unlabeled])),
                Err(NodeRegistryError::InvalidReport(
                    "host facts are incomplete or inconsistent"
                ))
            ));
        }
    }

    #[test]
    fn accelerator_facts_are_validated_with_specific_errors() {
        let snapshot = |vendor: AcceleratorVendor, facts: Vec<AcceleratorFacts>| HardwareSnapshot {
            observed_at: Some(Timestamp {
                seconds: 100,
                nanos: 0,
            }),
            host: Some(HostObservation {
                result: Some(host_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "HOST_UNAVAILABLE".to_owned(),
                        message: "Host observation is unavailable.".to_owned(),
                    },
                )),
            }),
            accelerators: vec![AcceleratorObservation {
                vendor: vendor as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet { facts },
                )),
            }],
        };
        let fact = |vendor: AcceleratorVendor| AcceleratorFacts {
            vendor: vendor as i32,
            model: "synthetic-device".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
            total_memory_bytes: Some(1024),
            driver_or_runtime_version: "1.0".to_owned(),
            device_id: "gpu-0".to_owned(),
            vendor_index: None,
            used_memory_bytes: Some(512),
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        };
        let nvidia = AcceleratorVendor::Nvidia;
        let rejection = |facts: Vec<AcceleratorFacts>| match validate_hardware_snapshot(&snapshot(
            nvidia, facts,
        )) {
            Err(NodeRegistryError::InvalidReport(message)) => message,
            other => panic!("expected InvalidReport, got {other:?}"),
        };

        // Apple GPUs may omit the driver version; other vendors may not.
        let mut apple = fact(AcceleratorVendor::Apple);
        apple.driver_or_runtime_version = String::new();
        assert!(
            validate_hardware_snapshot(&snapshot(AcceleratorVendor::Apple, vec![apple.clone()]))
                .is_ok()
        );
        assert!(validate_hardware_snapshot(&snapshot(nvidia, vec![fact(nvidia)])).is_ok());

        let mut no_driver = fact(nvidia);
        no_driver.driver_or_runtime_version = " ".to_owned();
        assert_eq!(
            rejection(vec![no_driver]),
            "accelerator fact driver or runtime version is required"
        );

        let mut wrong_vendor = fact(nvidia);
        wrong_vendor.vendor = AcceleratorVendor::Apple as i32;
        assert_eq!(
            rejection(vec![wrong_vendor]),
            "accelerator fact vendor must match its observation vendor"
        );

        let mut no_model = fact(nvidia);
        no_model.model = " ".to_owned();
        assert_eq!(
            rejection(vec![no_model]),
            "accelerator fact model is required"
        );

        let mut no_device_id = fact(nvidia);
        no_device_id.device_id = String::new();
        assert_eq!(
            rejection(vec![no_device_id]),
            "accelerator fact device id is required"
        );

        assert_eq!(
            rejection(vec![fact(nvidia), fact(nvidia)]),
            "accelerator fact device ids must be unique"
        );

        let mut over_used = fact(nvidia);
        over_used.used_memory_bytes = Some(2048);
        assert_eq!(
            rejection(vec![over_used]),
            "accelerator fact used memory must not exceed total memory"
        );
    }

    fn admission_registry(site: Site) -> NodeRegistry {
        NodeRegistry::with_coordinator(
            Arc::new(site),
            ManagerNodeControlCoordinator::on_system_clock(
                crate::view::fencing::ManagerSession::generate().expect("manager test session"),
            ),
        )
    }

    fn test_registry(site: Site) -> NodeRegistry {
        NodeRegistry::with_coordinator(
            Arc::new(site),
            ManagerNodeControlCoordinator::new(
                crate::view::fencing::ManagerSession::generate().expect("manager test session"),
                Timing::test().node_control,
                Arc::new(FixedWallClock),
            ),
        )
    }

    /// GPU ID, host total and idle host use of the named `gb10` machine.
    const GB10_GPU_ID: &str = "GPU-00000000-0000-0000-0000-00000000gb10";
    const GB10_HOST_TOTAL_BYTES: u64 = 130_596_048_896;
    const GB10_IDLE_USED_BYTES: u64 = 7_938_146_304;

    fn report(site: &Site, incarnation_id: String, revision: u64) -> NodeObservation {
        report_for(site, "worker-node", incarnation_id, revision)
    }

    fn report_for(
        site: &Site,
        node_id: &str,
        incarnation_id: String,
        revision: u64,
    ) -> NodeObservation {
        let configuration =
            node_observation_configuration(site, node_id).expect("node observation configuration");
        let digest = NodeObservationConfigurationDigest::for_configuration(&configuration)
            .expect("node observation digest");
        NodeObservation {
            site_id: site.site_id.clone(),
            node_id: node_id.to_owned(),
            incarnation_id,
            revision,
            snapshot: Some(NodeObservationSnapshot {
                hardware: Some(NodeHardwareObservation {
                    result: Some(node_hardware_observation::Result::Snapshot(
                        HardwareSnapshot {
                            observed_at: Some(Timestamp {
                                seconds: 100,
                                nanos: 0,
                            }),
                            host: Some(HostObservation {
                                result: Some(host_observation::Result::Unavailable(
                                    ObservationUnavailable {
                                        code: "HOST_UNAVAILABLE".to_owned(),
                                        message: "Host observation is unavailable.".to_owned(),
                                    },
                                )),
                            }),
                            accelerators: vec![AcceleratorObservation {
                                vendor: AcceleratorVendor::Nvidia as i32,
                                result: Some(accelerator_observation::Result::Facts(
                                    AcceleratorFactsSet {
                                        facts: vec![AcceleratorFacts {
                                            vendor: AcceleratorVendor::Nvidia as i32,
                                            model: "synthetic-nvidia-device".to_owned(),
                                            memory_architecture:
                                                AcceleratorMemoryArchitecture::Dedicated as i32,
                                            total_memory_bytes: Some(24 * 1024 * 1024 * 1024),
                                            driver_or_runtime_version: "580.173.02".to_owned(),
                                            device_id: "synthetic-device-0".to_owned(),
                                            vendor_index: None,
                                            used_memory_bytes: Some(0),
                                            working_set_limit_bytes: None,
                                            supported_signals: Vec::new(),
                                        }],
                                    },
                                )),
                            }],
                        },
                    )),
                }),
                runtimes: configuration
                    .execution_targets
                    .iter()
                    .map(|target| RuntimeObservation {
                        execution_target_id: target.execution_target_id.clone(),
                        kind: target.kind,
                        result: Some(runtime_observation::Result::Available(Empty {})),
                    })
                    .collect(),
            }),
            observation_configuration_sha256: digest.as_str().to_owned(),
            private_port: None,
        }
    }

    fn test_site() -> Site {
        let platform = OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        };
        let target = |id: &str, node: &str, application: bool| ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: node.to_owned(),
            platform: Some(platform.clone()),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(if application {
                execution_target::RuntimeBinding::ApplicationRuntime(ApplicationRuntimeBinding {
                    adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedApplicationRuntimeAdapter {},
                    )),
                })
            } else {
                execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        simulated_runtime(),
                    )),
                })
            }),
        };
        Site {
            site_id: "site-one".to_owned(),
            execution_targets: vec![
                target("model-target", "worker-node", false),
                target("manager-application", "manager-node", true),
                target("worker-application", "worker-node", true),
            ],
            nodes: vec![
                SiteNode {
                    node_id: "manager-node".to_owned(),
                    display_name: "Manager".to_owned(),
                    site_manager: Some(SiteManagerRole {}),
                    host: "127.0.0.1".to_owned(),
                    manager_host: String::new(),
                },
                SiteNode {
                    node_id: "worker-node".to_owned(),
                    display_name: "Node".to_owned(),
                    site_manager: None,
                    host: "127.0.0.2".to_owned(),
                    manager_host: String::new(),
                },
            ],
            hardware_observation: Some(HardwareObservationConfiguration {
                refresh_interval_seconds: Some(5),
            }),
            ..Default::default()
        }
    }

    fn simulated_runtime() -> SimulatedRuntimeAdapter {
        SimulatedRuntimeAdapter {
            default_scenario: Some(SimulatedRuntimeScenario {
                ready_delay_millis: 1,
                stop_delay_millis: 1,
            }),
            deployment_scenarios: Vec::new(),
        }
    }

    fn incarnation(character: char) -> String {
        character.to_string().repeat(32)
    }

    /// Tokio monotonic time with wall time fixed at 123 s and 456 ns after the Unix epoch.
    #[derive(Debug)]
    struct FixedWallClock;

    impl Clock for FixedWallClock {
        fn now(&self) -> tokio::time::Instant {
            tokio::time::Instant::now()
        }

        fn wall_now(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + std::time::Duration::new(123, 456)
        }

        fn sleep_until(&self, deadline: tokio::time::Instant) -> ClockSleep {
            Box::pin(tokio::time::sleep_until(deadline))
        }
    }
}
