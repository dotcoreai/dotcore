//! Serves the public read-only gRPC API for the site: its machines, execution targets, and current hardware.
//!
//! It answers inventory and hardware reads from current site configuration and the latest
//! site-manager-owned node observation. Requests perform no collection or mutation.

use std::sync::Arc;

use dotcore_domain::{ResolvedAccelerators, SiteNodeId};
use dotcore_proto::{
    dotcore::{
        observation::v1::{
            HardwareSnapshot, ObservationUnavailable, RuntimeKind, RuntimeObservation,
            runtime_observation,
        },
        site::private::v1::node_hardware_observation,
        site::v1::{
            ConfiguredExecutionTarget, CurrentHardwareSnapshotAvailability,
            ExecutionTargetAvailability, ExecutionTargetWorkloadKind, HardwareRefreshState,
            ListConfiguredExecutionTargetsRequest, ListConfiguredExecutionTargetsResponse,
            ListHardwareSnapshotsRequest, ListHardwareSnapshotsResponse, ListMachinesRequest,
            ListMachinesResponse, Machine, MachineHardwareSnapshot, NodeObservationFreshness,
            NodeObservationState, RuntimeTargetAvailability, Site, SiteNode,
            current_hardware_snapshot_availability, execution_target, hardware_refresh_state,
            runtime_target_availability, site_service_server::SiteService as SiteServiceContract,
        },
    },
    google::protobuf::Empty,
};
use tonic::{Request, Response, Status};
use tracing::{Instrument, debug_span, error, field};

use crate::node::registry::{NodeConnectionState, NodeObservationView, NodeRegistry};
use crate::repository::{SiteRepository, SiteRepositoryError};

const CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_CODE: &str = "CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE";
const CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_MESSAGE: &str =
    "A current hardware snapshot is unavailable for this machine.";
const RUNTIME_OBSERVATION_UNAVAILABLE_CODE: &str = "RUNTIME_OBSERVATION_UNAVAILABLE";
const RUNTIME_OBSERVATION_UNAVAILABLE_MESSAGE: &str = "This runtime target has not been observed.";
const RUNTIME_OBSERVATION_STALE_CODE: &str = "RUNTIME_OBSERVATION_STALE";
const RUNTIME_OBSERVATION_STALE_MESSAGE: &str =
    "The retained runtime target observation is no longer fresh.";

/// Supplies current authoritative site configuration to public inventory reads.
pub trait SiteConfigurationSource: Send + Sync {
    fn snapshot(&self) -> Result<Site, SiteRepositoryError>;
}

impl SiteConfigurationSource for SiteRepository {
    fn snapshot(&self) -> Result<Site, SiteRepositoryError> {
        SiteRepository::snapshot(self)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SiteConfigurationSource for Site {
    fn snapshot(&self) -> Result<Site, SiteRepositoryError> {
        Ok(self.clone())
    }
}

/// Public site inventory service over the observations every SiteNode reports to the manager.
pub struct SiteService {
    site: Arc<dyn SiteConfigurationSource>,

    /// SiteNode hosting the manager process, projected with the site-manager marker.
    manager_node_id: SiteNodeId,

    nodes: NodeRegistry,
}

impl SiteService {
    /// Creates a cache-only public service over the manager's registry of node observations.
    pub fn new(
        site: Arc<dyn SiteConfigurationSource>,
        manager_node_id: SiteNodeId,
        nodes: NodeRegistry,
    ) -> Self {
        Self {
            site,
            manager_node_id,
            nodes,
        }
    }

    /// Answers every configured machine, in machine-identifier order.
    pub fn machines(&self) -> Result<ListMachinesResponse, SiteRepositoryError> {
        let site = self.site.snapshot()?;
        let machines = sorted_nodes(&site)
            .into_iter()
            .map(|node| self.machine(&site, node))
            .collect();
        Ok(ListMachinesResponse { machines })
    }

    /// Answers every configured execution target with its placement context and availability,
    /// in execution-target-identifier order.
    pub fn configured_execution_targets(
        &self,
    ) -> Result<ListConfiguredExecutionTargetsResponse, SiteRepositoryError> {
        let site = self.site.snapshot()?;
        let mut targets = site.execution_targets.iter().collect::<Vec<_>>();
        targets.sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));
        let execution_targets = targets
            .into_iter()
            .map(|target| {
                let node_id = SiteNodeId::parse(&target.site_node_id)
                    .expect("validated execution target has a valid node identity");
                let observation = self.nodes.observation(&node_id);
                let connection_state = match observation
                    .as_ref()
                    .map(|observation| observation.connection_state)
                {
                    Some(NodeConnectionState::Connected) => NodeObservationState::Connected,
                    Some(NodeConnectionState::Stale) => NodeObservationState::Stale,
                    Some(NodeConnectionState::Disconnected) => NodeObservationState::Disconnected,
                    None => NodeObservationState::Missing,
                };
                let reported =
                    observation.as_ref().and_then(|observation| {
                        observation.snapshot.runtimes.iter().find(|runtime| {
                            runtime.execution_target_id == target.execution_target_id
                        })
                    });
                let runtime_availability = match connection_state {
                    NodeObservationState::Connected => project_runtime_availability(
                        reported.and_then(|runtime| runtime.result.as_ref()),
                    ),
                    NodeObservationState::Stale | NodeObservationState::Disconnected => {
                        runtime_target_availability::Availability::Unavailable(
                            ObservationUnavailable {
                                code: RUNTIME_OBSERVATION_STALE_CODE.to_owned(),
                                message: RUNTIME_OBSERVATION_STALE_MESSAGE.to_owned(),
                            },
                        )
                    }
                    _ => project_runtime_availability(None),
                };
                let workload_kind = match target.runtime_binding {
                    Some(execution_target::RuntimeBinding::ModelRuntime(_)) => {
                        ExecutionTargetWorkloadKind::Model
                    }
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(_)) => {
                        ExecutionTargetWorkloadKind::Application
                    }
                    None => {
                        unreachable!("validated execution target requires a runtime binding")
                    }
                };
                // Derived on every read from the target's own node, never stored or admitted on.
                let accelerators = ResolvedAccelerators::resolve(
                    &target.accelerator_device_ids,
                    observation.as_ref().and_then(hardware_snapshot),
                );
                ConfiguredExecutionTarget {
                    execution_target_id: target.execution_target_id.clone(),
                    display_name: target.display_name.clone(),
                    site_node_id: target.site_node_id.clone(),
                    site_node_display_name: site
                        .nodes
                        .iter()
                        .find(|node| node.node_id == target.site_node_id)
                        .map(|node| node.display_name.clone())
                        .unwrap_or_default(),
                    site_manager: node_id == self.manager_node_id,
                    platform_label: target
                        .platform
                        .as_ref()
                        .map(|platform| {
                            format!("{}/{}", platform.operating_system, platform.architecture)
                        })
                        .unwrap_or_default(),
                    workload_kind: workload_kind as i32,
                    availability: Some(ExecutionTargetAvailability {
                        connection_state: connection_state as i32,
                        runtime_availability: Some(RuntimeTargetAvailability {
                            availability: Some(runtime_availability),
                        }),
                    }),
                    accelerator_device_ids: target.accelerator_device_ids.clone(),
                    observed_accelerator_device_ids: accelerators.ids,
                    accelerator_resolution: accelerators.status.to_proto() as i32,
                }
            })
            .collect::<Vec<_>>();
        Ok(ListConfiguredExecutionTargetsResponse { execution_targets })
    }

    /// Answers the current hardware snapshot of every configured machine, in machine-identifier
    /// order. A machine without a snapshot is still listed with its pending or failed refresh.
    pub fn hardware_snapshots(&self) -> Result<ListHardwareSnapshotsResponse, SiteRepositoryError> {
        let site = self.site.snapshot()?;
        let machines = sorted_nodes(&site)
            .into_iter()
            .map(|node| {
                let node_id = SiteNodeId::parse(&node.node_id)
                    .expect("validated site configuration must contain valid node identities");
                self.machine_hardware(&node_id)
            })
            .collect();
        Ok(ListHardwareSnapshotsResponse { machines })
    }

    fn machine(&self, site: &Site, node: &SiteNode) -> Machine {
        let node_id = SiteNodeId::parse(&node.node_id)
            .expect("validated site configuration must contain valid node identities");
        let (availability, freshness, runtimes) = match self.nodes.observation(&node_id) {
            Some(observation) => (
                node_snapshot_availability(&observation),
                node_observation_freshness(&observation),
                observation.snapshot.runtimes,
            ),
            None => (
                unavailable_snapshot(),
                observation_freshness(NodeObservationState::Missing, None),
                unobserved_runtimes(site, node),
            ),
        };

        Machine {
            machine_id: node.node_id.clone(),
            display_name: node.display_name.clone(),
            site_manager: node_id == self.manager_node_id,
            current_hardware_snapshot_availability: Some(availability),
            observation_freshness: Some(freshness),
            runtimes,
        }
    }

    fn machine_hardware(&self, node_id: &SiteNodeId) -> MachineHardwareSnapshot {
        let Some(observation) = self.nodes.observation(node_id) else {
            return MachineHardwareSnapshot {
                machine_id: node_id.as_str().to_owned(),
                snapshot: None,
                refresh: Some(HardwareRefreshState {
                    attempted_at: None,
                    outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
                }),
                observation_freshness: Some(observation_freshness(
                    NodeObservationState::Missing,
                    None,
                )),
            };
        };
        let freshness = node_observation_freshness(&observation);
        let (snapshot, outcome) = match observation
            .snapshot
            .hardware
            .expect("validated node observation must contain a hardware result")
            .result
            .expect("validated node observation must contain a hardware outcome")
        {
            node_hardware_observation::Result::Snapshot(snapshot) => (
                Some(snapshot),
                hardware_refresh_state::Outcome::Succeeded(Empty {}),
            ),
            node_hardware_observation::Result::Unavailable(reason) => {
                (None, hardware_refresh_state::Outcome::Failed(reason))
            }
        };

        MachineHardwareSnapshot {
            machine_id: node_id.as_str().to_owned(),
            snapshot,
            refresh: Some(HardwareRefreshState {
                // The manager received the outcome right after the node attempted collection.
                attempted_at: Some(observation.received_at),
                outcome: Some(outcome),
            }),
            observation_freshness: Some(freshness),
        }
    }
}

fn site_status(error: SiteRepositoryError) -> Status {
    error!(error = %error, "Site configuration snapshot failed");
    Status::internal("Site configuration snapshot failed")
}

/// Configured nodes in node-identifier order.
fn sorted_nodes(site: &Site) -> Vec<&SiteNode> {
    let mut nodes = site.nodes.iter().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    nodes
}

fn node_snapshot_availability(
    observation: &NodeObservationView,
) -> CurrentHardwareSnapshotAvailability {
    if observation.connection_state != NodeConnectionState::Connected {
        return unavailable_snapshot();
    }
    match observation
        .snapshot
        .hardware
        .as_ref()
        .and_then(|hardware| hardware.result.as_ref())
    {
        Some(node_hardware_observation::Result::Snapshot(_)) => available_snapshot(),
        Some(node_hardware_observation::Result::Unavailable(reason)) => {
            CurrentHardwareSnapshotAvailability {
                availability: Some(
                    current_hardware_snapshot_availability::Availability::Unavailable(
                        reason.clone(),
                    ),
                ),
            }
        }
        None => unavailable_snapshot(),
    }
}

/// Returns the node's latest reported hardware snapshot, or none when its last collection failed.
fn hardware_snapshot(observation: &NodeObservationView) -> Option<&HardwareSnapshot> {
    match observation.snapshot.hardware.as_ref()?.result.as_ref()? {
        node_hardware_observation::Result::Snapshot(snapshot) => Some(snapshot),
        node_hardware_observation::Result::Unavailable(_) => None,
    }
}

fn available_snapshot() -> CurrentHardwareSnapshotAvailability {
    CurrentHardwareSnapshotAvailability {
        availability: Some(
            current_hardware_snapshot_availability::Availability::Available(Empty {}),
        ),
    }
}

fn unavailable_snapshot() -> CurrentHardwareSnapshotAvailability {
    CurrentHardwareSnapshotAvailability {
        availability: Some(
            current_hardware_snapshot_availability::Availability::Unavailable(
                ObservationUnavailable {
                    code: CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_CODE.to_owned(),
                    message: CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_MESSAGE.to_owned(),
                },
            ),
        ),
    }
}

fn observation_freshness(
    state: NodeObservationState,
    received_at: Option<dotcore_proto::google::protobuf::Timestamp>,
) -> NodeObservationFreshness {
    NodeObservationFreshness {
        state: state as i32,
        received_at,
    }
}

/// Freshness of one node: its liveness state and latest observation receipt.
fn node_observation_freshness(observation: &NodeObservationView) -> NodeObservationFreshness {
    let state = match observation.connection_state {
        NodeConnectionState::Connected => NodeObservationState::Connected,
        NodeConnectionState::Stale => NodeObservationState::Stale,
        NodeConnectionState::Disconnected => NodeObservationState::Disconnected,
    };
    observation_freshness(state, Some(observation.received_at))
}

fn unobserved_runtimes(site: &Site, node: &SiteNode) -> Vec<RuntimeObservation> {
    let mut runtimes = site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == node.node_id)
        .map(|target| {
            let kind = match target.runtime_binding {
                Some(execution_target::RuntimeBinding::ModelRuntime(_)) => RuntimeKind::Model,
                Some(execution_target::RuntimeBinding::ApplicationRuntime(_)) => {
                    RuntimeKind::Application
                }
                None => unreachable!("validated execution target requires a runtime binding"),
            };
            unobserved_runtime(&target.execution_target_id, kind)
        })
        .collect::<Vec<_>>();
    runtimes.sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));
    runtimes
}

fn unobserved_runtime(execution_target_id: &str, kind: RuntimeKind) -> RuntimeObservation {
    RuntimeObservation {
        execution_target_id: execution_target_id.to_owned(),
        kind: kind as i32,
        result: Some(runtime_observation::Result::Unavailable(
            ObservationUnavailable {
                code: RUNTIME_OBSERVATION_UNAVAILABLE_CODE.to_owned(),
                message: RUNTIME_OBSERVATION_UNAVAILABLE_MESSAGE.to_owned(),
            },
        )),
    }
}

fn project_runtime_availability(
    result: Option<&runtime_observation::Result>,
) -> runtime_target_availability::Availability {
    match result {
        Some(runtime_observation::Result::Available(_)) => {
            runtime_target_availability::Availability::Available(Empty {})
        }
        Some(runtime_observation::Result::Unavailable(reason)) => {
            runtime_target_availability::Availability::Unavailable(reason.clone())
        }
        Some(runtime_observation::Result::Unsupported(reason)) => {
            runtime_target_availability::Availability::Unsupported(reason.clone())
        }
        None => runtime_target_availability::Availability::Unavailable(ObservationUnavailable {
            code: RUNTIME_OBSERVATION_UNAVAILABLE_CODE.to_owned(),
            message: RUNTIME_OBSERVATION_UNAVAILABLE_MESSAGE.to_owned(),
        }),
    }
}

#[tonic::async_trait]
impl SiteServiceContract for SiteService {
    async fn list_machines(
        &self,
        _request: Request<ListMachinesRequest>,
    ) -> Result<Response<ListMachinesResponse>, Status> {
        let span = debug_span!(
            "site_list_machines",
            operation = "site_list_machines",
            machine_count = field::Empty,
            outcome = field::Empty,
        );
        let result = async {
            let response = self.machines().map_err(site_status)?;
            span.record("machine_count", response.machines.len());
            Ok(Response::new(response))
        }
        .instrument(span.clone())
        .await;
        span.record("outcome", if result.is_ok() { "success" } else { "failed" });
        result
    }

    async fn list_configured_execution_targets(
        &self,
        _request: Request<ListConfiguredExecutionTargetsRequest>,
    ) -> Result<Response<ListConfiguredExecutionTargetsResponse>, Status> {
        let span = debug_span!(
            "site_list_configured_execution_targets",
            operation = "site_list_configured_execution_targets",
            execution_target_count = field::Empty,
            outcome = field::Empty,
        );
        let result = async {
            let response = self.configured_execution_targets().map_err(site_status)?;
            span.record("execution_target_count", response.execution_targets.len());
            Ok(Response::new(response))
        }
        .instrument(span.clone())
        .await;
        span.record("outcome", if result.is_ok() { "success" } else { "failed" });
        result
    }

    async fn list_hardware_snapshots(
        &self,
        _request: Request<ListHardwareSnapshotsRequest>,
    ) -> Result<Response<ListHardwareSnapshotsResponse>, Status> {
        let span = debug_span!(
            "site_list_hardware_snapshots",
            operation = "site_list_hardware_snapshots",
            machine_count = field::Empty,
            outcome = field::Empty,
        );
        let result = async {
            let response = self.hardware_snapshots().map_err(site_status)?;
            span.record("machine_count", response.machines.len());
            Ok(Response::new(response))
        }
        .instrument(span.clone())
        .await;
        span.record("outcome", if result.is_ok() { "success" } else { "failed" });
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_domain::SiteNodeId;
    use dotcore_proto::{
        dotcore::{
            catalog::v1::OciPlatform,
            observation::v1::{
                AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
                AcceleratorObservation, AcceleratorVendor, HardwareSnapshot, HostFacts,
                HostObservation, ObservationUnavailable, RuntimeKind, RuntimeObservation,
                accelerator_observation, host_observation, runtime_observation,
            },
            site::private::v1::{
                NodeHardwareObservation as ReportedHardwareObservation, NodeObservation,
                NodeObservationAcceptance, NodeObservationSnapshot, node_hardware_observation,
            },
            site::v1::{
                AcceleratorResolutionStatus, ApplicationRuntimeBinding, ExecutionTarget,
                HardwareObservationConfiguration, HardwareRefreshState,
                ListConfiguredExecutionTargetsRequest, ListHardwareSnapshotsRequest,
                ListMachinesRequest, ModelRuntimeBinding, SimulatedApplicationRuntimeAdapter,
                SimulatedRuntimeAdapter, Site, SiteManagerRole, SiteNode,
                application_runtime_binding, execution_target, hardware_refresh_state,
                model_runtime_binding,
            },
        },
        google::protobuf::{Empty, Timestamp},
    };
    use tokio::time::{Duration, advance};

    use super::*;
    use crate::{
        node::{
            control::{ManagerNodeControlCoordinator, PublishedViewInput},
            observation_configuration::{
                NodeObservationConfigurationDigest, node_observation_configuration,
            },
        },
        view::fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    };

    struct MutableSite(Mutex<Site>);

    impl SiteConfigurationSource for MutableSite {
        fn snapshot(&self) -> Result<Site, SiteRepositoryError> {
            Ok(self.0.lock().expect("Site fixture lock").clone())
        }
    }

    #[tokio::test]
    async fn list_machines_is_sorted_and_availability_is_node_keyed() {
        let mut site = node_site();
        site.nodes.reverse();
        let (service, registry) = service_with_registry(site.clone());
        registry
            .accept(report(
                &site,
                "worker-node",
                1,
                node_hardware_observation::Result::Snapshot(reported_snapshot()),
            ))
            .expect("node observation");

        let response =
            SiteServiceContract::list_machines(&service, Request::new(ListMachinesRequest {}))
                .await
                .expect("machine list")
                .into_inner();

        assert_eq!(response.machines[0].machine_id, "manager-node");
        assert!(response.machines[0].site_manager);
        assert_eq!(
            response.machines[0].current_hardware_snapshot_availability,
            Some(unavailable())
        );
        assert_eq!(response.machines[1].machine_id, "worker-node");
        assert!(!response.machines[1].site_manager);
        assert_eq!(
            response.machines[1].current_hardware_snapshot_availability,
            Some(available())
        );
    }

    #[tokio::test]
    async fn manager_node_targets_project_its_reports_with_the_site_manager_marker() {
        let site = Arc::new(MutableSite(Mutex::new(manager_site())));
        let source: Arc<dyn SiteConfigurationSource> = site.clone();
        let registry = NodeRegistry::new(Arc::clone(&source));
        let service = SiteService::new(source, SiteNodeId::for_test("node-a"), registry.clone());
        let reported = manager_site();
        registry
            .accept(manager_report(
                &reported,
                1,
                vec![
                    model_runtime("target-a", runtime_observation::Result::Available(Empty {})),
                    model_runtime("target-b", runtime_observation::Result::Available(Empty {})),
                ],
            ))
            .expect("manager node observation");

        let first = configured_target_response(&service).await;
        assert_eq!(
            first
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<Vec<_>>(),
            ["target-a", "target-b"]
        );
        assert!(first.iter().all(|target| target.site_manager));
        assert!(
            first
                .iter()
                .all(|target| target.site_node_display_name == "A")
        );
        assert_eq!(first[0].accelerator_device_ids, ["configured-device"]);
        assert_eq!(
            first[0].availability,
            Some(ExecutionTargetAvailability {
                connection_state: NodeObservationState::Connected as i32,
                runtime_availability: Some(RuntimeTargetAvailability {
                    availability: Some(runtime_target_availability::Availability::Available(
                        Empty {}
                    )),
                }),
            })
        );

        let probe_failure = ObservationUnavailable {
            code: "RUNTIME_PROBE_FAILED".to_owned(),
            message: "The configured runtime infrastructure is unavailable.".to_owned(),
        };
        registry
            .accept(manager_report(
                &reported,
                2,
                vec![
                    model_runtime(
                        "target-a",
                        runtime_observation::Result::Unavailable(probe_failure.clone()),
                    ),
                    model_runtime("target-b", runtime_observation::Result::Available(Empty {})),
                ],
            ))
            .expect("second manager node observation");
        let unavailable = configured_target_response(&service).await;
        assert_eq!(
            runtime_availability(&unavailable[0]),
            Some(&runtime_target_availability::Availability::Unavailable(
                probe_failure
            ))
        );
        assert_eq!(
            runtime_availability(&unavailable[1]),
            Some(&runtime_target_availability::Availability::Available(
                Empty {}
            ))
        );

        site.0.lock().expect("Site fixture lock").execution_targets =
            vec![execution_target("target-c")];
        let second = configured_target_response(&service).await;
        assert_eq!(second[0].execution_target_id, "target-c");
        assert_eq!(
            runtime_availability(&second[0]),
            Some(&runtime_target_availability::Availability::Unavailable(
                ObservationUnavailable {
                    code: RUNTIME_OBSERVATION_UNAVAILABLE_CODE.to_owned(),
                    message: RUNTIME_OBSERVATION_UNAVAILABLE_MESSAGE.to_owned(),
                }
            ))
        );
    }

    #[tokio::test]
    async fn configured_targets_resolve_selectors_from_their_own_node_snapshot() {
        let mut site = node_site();
        let selector_target = |id: &str, node: &str, selectors: &[&str]| ExecutionTarget {
            execution_target_id: id.to_owned(),
            site_node_id: node.to_owned(),
            accelerator_device_ids: selectors.iter().map(|id| (*id).to_owned()).collect(),
            ..execution_target(id)
        };
        site.execution_targets = vec![
            selector_target("manager-gpu", "manager-node", &["nvidia.com/gpu=0"]),
            selector_target("worker-second", "worker-node", &["nvidia.com/gpu=1"]),
            selector_target(
                "worker-both",
                "worker-node",
                &["nvidia.com/gpu=gpu1", "nvidia.com/gpu=0"],
            ),
            selector_target("worker-simulated", "worker-node", &[]),
        ];
        let (service, registry) = service_with_registry(site.clone());
        registry
            .accept(report(
                &site,
                "worker-node",
                1,
                node_hardware_observation::Result::Snapshot(nvidia_snapshot(&[
                    WORKER_FIRST_GPU,
                    WORKER_SECOND_GPU,
                ])),
            ))
            .expect("worker node observation");

        let before_manager = configured_target_response(&service).await;
        assert_eq!(
            resolutions(&before_manager),
            [
                (
                    "manager-gpu",
                    Vec::new(),
                    AcceleratorResolutionStatus::SnapshotUnavailable
                ),
                (
                    "worker-both",
                    vec![WORKER_FIRST_GPU, WORKER_SECOND_GPU],
                    AcceleratorResolutionStatus::Resolved
                ),
                (
                    "worker-second",
                    vec![WORKER_SECOND_GPU],
                    AcceleratorResolutionStatus::Resolved
                ),
                (
                    "worker-simulated",
                    Vec::new(),
                    AcceleratorResolutionStatus::NotApplicable
                ),
            ]
        );
        assert_eq!(
            before_manager[2].accelerator_device_ids,
            ["nvidia.com/gpu=1"]
        );

        registry
            .accept(report(
                &site,
                "manager-node",
                1,
                node_hardware_observation::Result::Snapshot(nvidia_snapshot(&[MANAGER_GPU])),
            ))
            .expect("manager node observation");

        let after_manager = configured_target_response(&service).await;
        assert_eq!(
            resolutions(&after_manager),
            [
                (
                    "manager-gpu",
                    vec![MANAGER_GPU],
                    AcceleratorResolutionStatus::Resolved
                ),
                (
                    "worker-both",
                    vec![WORKER_FIRST_GPU, WORKER_SECOND_GPU],
                    AcceleratorResolutionStatus::Resolved
                ),
                (
                    "worker-second",
                    vec![WORKER_SECOND_GPU],
                    AcceleratorResolutionStatus::Resolved
                ),
                (
                    "worker-simulated",
                    Vec::new(),
                    AcceleratorResolutionStatus::NotApplicable
                ),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_manager_node_turns_stale_then_disconnected_like_any_node() {
        let site = manager_site();
        let coordinator = ManagerNodeControlCoordinator::on_system_clock(
            ManagerSession::generate().expect("manager session"),
        );
        let registry = NodeRegistry::with_coordinator(Arc::new(site.clone()), coordinator.clone());
        let service = SiteService::new(
            Arc::new(site.clone()),
            SiteNodeId::for_test("node-a"),
            registry.clone(),
        );
        let accepted = registry
            .accept(manager_report(
                &site,
                1,
                vec![
                    model_runtime("target-a", runtime_observation::Result::Available(Empty {})),
                    model_runtime("target-b", runtime_observation::Result::Available(Empty {})),
                ],
            ))
            .expect("manager node observation");
        let received_at = accepted.received_at.expect("manager receipt time");
        assert_eq!(
            target_connection_states(&configured_target_response(&service).await),
            [NodeObservationState::Connected as i32; 2]
        );
        let connected = machine_response(&service, "node-a").await;
        assert!(connected.site_manager);
        assert_eq!(
            connected.observation_freshness,
            Some(observation_freshness(
                NodeObservationState::Connected,
                Some(received_at)
            ))
        );

        // Publications keep the manager node live while its observation ages past the capability age.
        let mut elapsed = Duration::ZERO;
        while elapsed
            <= dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age
        {
            advance(
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .node_silence
                    / 2,
            )
            .await;
            elapsed += dotcore_domain::timing::Timing::production()
                .node_control
                .node_silence
                / 2;
            publish_as(&coordinator, &site, "node-a");
        }
        let stale_targets = configured_target_response(&service).await;
        assert_eq!(
            target_connection_states(&stale_targets),
            [NodeObservationState::Stale as i32; 2]
        );
        assert!(stale_targets.iter().all(|target| {
            runtime_availability(target)
                == Some(&runtime_target_availability::Availability::Unavailable(
                    ObservationUnavailable {
                        code: RUNTIME_OBSERVATION_STALE_CODE.to_owned(),
                        message: RUNTIME_OBSERVATION_STALE_MESSAGE.to_owned(),
                    },
                ))
        }));
        let stale = machine_response(&service, "node-a").await;
        assert!(stale.site_manager);
        assert_eq!(
            stale.observation_freshness.map(|freshness| freshness.state),
            Some(NodeObservationState::Stale as i32)
        );

        advance(
            dotcore_domain::timing::Timing::production()
                .node_control
                .node_silence
                + Duration::from_millis(1),
        )
        .await;
        assert_eq!(
            target_connection_states(&configured_target_response(&service).await),
            [NodeObservationState::Disconnected as i32; 2]
        );
    }

    #[tokio::test]
    async fn manager_node_hardware_reads_pending_success_and_failure_from_its_reports() {
        let site = manager_site();
        let (service, registry) = service_with_manager(site.clone());

        assert_eq!(
            current(&service, "node-a").await,
            MachineHardwareSnapshot {
                machine_id: "node-a".to_owned(),
                snapshot: None,
                refresh: Some(HardwareRefreshState {
                    attempted_at: None,
                    outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
                }),
                observation_freshness: Some(observation_freshness(
                    NodeObservationState::Missing,
                    None,
                )),
            }
        );
        assert_eq!(
            machine_response(&service, "node-a")
                .await
                .current_hardware_snapshot_availability,
            Some(unavailable())
        );

        let accepted = registry
            .accept(report(
                &site,
                "node-a",
                1,
                node_hardware_observation::Result::Snapshot(reported_snapshot()),
            ))
            .expect("manager node observation");
        let connected = connected_freshness(&accepted);
        assert_eq!(
            current(&service, "node-a").await,
            MachineHardwareSnapshot {
                machine_id: "node-a".to_owned(),
                snapshot: Some(reported_snapshot()),
                refresh: Some(HardwareRefreshState {
                    attempted_at: accepted.received_at,
                    outcome: Some(hardware_refresh_state::Outcome::Succeeded(Empty {})),
                }),
                observation_freshness: Some(connected),
            }
        );
        let machine = machine_response(&service, "node-a").await;
        assert_eq!(
            machine.current_hardware_snapshot_availability,
            Some(available())
        );
        assert_eq!(machine.observation_freshness, Some(connected));

        let failed = ObservationUnavailable {
            code: "COLLECTION_TIMED_OUT".to_owned(),
            message: "Hardware observation timed out on this node.".to_owned(),
        };
        let accepted = registry
            .accept(report(
                &site,
                "node-a",
                2,
                node_hardware_observation::Result::Unavailable(failed.clone()),
            ))
            .expect("failed manager node observation");
        assert_eq!(
            current(&service, "node-a").await,
            MachineHardwareSnapshot {
                machine_id: "node-a".to_owned(),
                snapshot: None,
                refresh: Some(HardwareRefreshState {
                    attempted_at: accepted.received_at,
                    outcome: Some(hardware_refresh_state::Outcome::Failed(failed.clone())),
                }),
                observation_freshness: Some(connected_freshness(&accepted)),
            }
        );
        assert_eq!(
            machine_response(&service, "node-a")
                .await
                .current_hardware_snapshot_availability,
            Some(CurrentHardwareSnapshotAvailability {
                availability: Some(
                    current_hardware_snapshot_availability::Availability::Unavailable(failed),
                ),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remote_projection_preserves_outcomes_across_freshness_states() {
        let site = node_site();
        let coordinator = ManagerNodeControlCoordinator::on_system_clock(
            ManagerSession::generate().expect("manager session"),
        );
        let registry = NodeRegistry::with_coordinator(Arc::new(site.clone()), coordinator.clone());
        let service = SiteService::new(
            Arc::new(site.clone()),
            SiteNodeId::for_test("manager-node"),
            registry.clone(),
        );
        let accepted = registry
            .accept(node_report(
                &site,
                1,
                node_hardware_observation::Result::Snapshot(reported_snapshot()),
            ))
            .expect("node observation");
        let received_at = accepted.received_at.expect("manager receipt time");
        let expected_runtimes = node_runtimes();

        let connected_targets = configured_target_response(&service).await;
        assert!(
            connected_targets
                .iter()
                .all(|target| { target.site_manager == (target.site_node_id == "manager-node") })
        );
        assert_eq!(
            connected_targets
                .iter()
                .map(|target| (
                    target.execution_target_id.as_str(),
                    target.display_name.as_str(),
                    target.site_node_display_name.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("application-target", "Target application-target", "Node"),
                (
                    "manager-application",
                    "Target manager-application",
                    "Manager"
                ),
                ("model-target", "Target model-target", "Node"),
                ("unavailable-target", "Target unavailable-target", "Node"),
            ]
        );
        let unsupported = connected_targets
            .iter()
            .find(|target| target.execution_target_id == "application-target")
            .expect("application target");
        assert_eq!(
            unsupported
                .availability
                .as_ref()
                .expect("target availability")
                .runtime_availability
                .as_ref()
                .expect("Runtime availability")
                .availability,
            Some(runtime_target_availability::Availability::Unsupported(
                ObservationUnavailable {
                    code: "RUNTIME_UNSUPPORTED".to_owned(),
                    message: "Runtime is unsupported.".to_owned(),
                }
            ))
        );
        let connected = machine_response(&service, "worker-node").await;
        assert_eq!(
            connected.current_hardware_snapshot_availability,
            Some(available())
        );
        assert_eq!(
            connected.observation_freshness,
            Some(observation_freshness(
                NodeObservationState::Connected,
                Some(received_at)
            ))
        );
        assert_eq!(connected.runtimes, expected_runtimes);

        // Publications keep the node live while its observation ages past the capability age.
        let mut elapsed = Duration::ZERO;
        while elapsed
            <= dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age
        {
            advance(
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .node_silence
                    / 2,
            )
            .await;
            elapsed += dotcore_domain::timing::Timing::production()
                .node_control
                .node_silence
                / 2;
            publish_as(&coordinator, &site, "worker-node");
        }
        let stale_targets = configured_target_response(&service).await;
        assert!(
            stale_targets
                .iter()
                .filter(|target| target.site_node_id == "worker-node")
                .all(|target| {
                    target.availability.as_ref().is_some_and(|availability| {
                        availability.connection_state == NodeObservationState::Stale as i32
                    })
                })
        );
        assert_remote_runtimes_stale(&stale_targets);
        let stale = machine_response(&service, "worker-node").await;
        assert_eq!(
            stale.current_hardware_snapshot_availability,
            Some(unavailable())
        );
        let stale_freshness = stale.observation_freshness.expect("stale freshness");
        assert_eq!(
            (stale_freshness.state, stale_freshness.received_at),
            (NodeObservationState::Stale as i32, Some(received_at))
        );
        assert_eq!(stale.runtimes, expected_runtimes);
        assert_eq!(
            current(&service, "worker-node").await,
            MachineHardwareSnapshot {
                machine_id: "worker-node".to_owned(),
                snapshot: Some(reported_snapshot()),
                refresh: Some(HardwareRefreshState {
                    attempted_at: Some(received_at),
                    outcome: Some(hardware_refresh_state::Outcome::Succeeded(Empty {})),
                }),
                observation_freshness: Some(stale_freshness),
            }
        );

        advance(
            dotcore_domain::timing::Timing::production()
                .node_control
                .node_silence
                + Duration::from_millis(1),
        )
        .await;
        let disconnected_targets = configured_target_response(&service).await;
        assert!(
            disconnected_targets
                .iter()
                .filter(|target| target.site_node_id == "worker-node")
                .all(|target| {
                    target.availability.as_ref().is_some_and(|availability| {
                        availability.connection_state == NodeObservationState::Disconnected as i32
                    })
                })
        );
        assert_remote_runtimes_stale(&disconnected_targets);
        let disconnected = machine_response(&service, "worker-node").await;
        assert_eq!(
            disconnected.observation_freshness,
            Some(NodeObservationFreshness {
                state: NodeObservationState::Disconnected as i32,
                ..stale_freshness
            })
        );
        assert_eq!(disconnected.runtimes, expected_runtimes);

        registry
            .accept(node_report(
                &site,
                2,
                node_hardware_observation::Result::Snapshot(reported_snapshot()),
            ))
            .expect("fresh node recovery");
        let recovered_targets = configured_target_response(&service).await;
        assert_eq!(
            recovered_targets
                .iter()
                .find(|target| target.execution_target_id == "model-target")
                .and_then(|target| target.availability.as_ref())
                .and_then(|availability| availability.runtime_availability.as_ref())
                .and_then(|runtime| runtime.availability.as_ref()),
            Some(&runtime_target_availability::Availability::Available(
                Empty {}
            ))
        );
    }

    #[tokio::test]
    async fn remote_missing_and_collection_failure_are_explicit() {
        let site = node_site();
        let (service, registry) = service_with_registry(site.clone());

        let missing = machine_response(&service, "worker-node").await;
        assert_eq!(
            missing.observation_freshness,
            Some(observation_freshness(NodeObservationState::Missing, None))
        );
        assert_eq!(missing.runtimes, unobserved_runtimes(&site, &site.nodes[1]));
        assert_eq!(
            current(&service, "worker-node").await,
            MachineHardwareSnapshot {
                machine_id: "worker-node".to_owned(),
                snapshot: None,
                refresh: Some(HardwareRefreshState {
                    attempted_at: None,
                    outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
                }),
                observation_freshness: Some(observation_freshness(
                    NodeObservationState::Missing,
                    None,
                )),
            }
        );

        let failure = ObservationUnavailable {
            code: "HARDWARE_COLLECTION_FAILED".to_owned(),
            message: "Hardware collection failed.".to_owned(),
        };
        registry
            .accept(node_report(
                &site,
                1,
                node_hardware_observation::Result::Unavailable(failure.clone()),
            ))
            .expect("failed hardware observation");
        let projected = machine_response(&service, "worker-node").await;
        assert_eq!(
            projected.current_hardware_snapshot_availability,
            Some(CurrentHardwareSnapshotAvailability {
                availability: Some(
                    current_hardware_snapshot_availability::Availability::Unavailable(
                        failure.clone(),
                    ),
                ),
            })
        );
        let detail = current(&service, "worker-node").await;
        assert_eq!(detail.snapshot, None);
        assert_eq!(
            detail.refresh.expect("remote refresh outcome").outcome,
            Some(hardware_refresh_state::Outcome::Failed(failure))
        );
        assert_eq!(
            detail
                .observation_freshness
                .expect("remote freshness")
                .state,
            NodeObservationState::Connected as i32
        );
    }

    #[tokio::test]
    async fn hardware_snapshots_list_every_machine_in_identifier_order() {
        let site = node_site();
        let (service, registry) = service_with_registry(site.clone());
        let pending = |machine_id: &str| MachineHardwareSnapshot {
            machine_id: machine_id.to_owned(),
            snapshot: None,
            refresh: Some(HardwareRefreshState {
                attempted_at: None,
                outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
            }),
            observation_freshness: Some(observation_freshness(NodeObservationState::Missing, None)),
        };

        // Never-observed machines are listed with a pending refresh.
        assert_eq!(
            hardware_list(&service).await,
            [pending("manager-node"), pending("worker-node")]
        );

        let failure = ObservationUnavailable {
            code: "HARDWARE_COLLECTION_FAILED".to_owned(),
            message: "Hardware collection failed.".to_owned(),
        };
        let accepted = registry
            .accept(node_report(
                &site,
                1,
                node_hardware_observation::Result::Unavailable(failure.clone()),
            ))
            .expect("failed hardware observation");
        assert_eq!(
            hardware_list(&service).await,
            [
                pending("manager-node"),
                MachineHardwareSnapshot {
                    machine_id: "worker-node".to_owned(),
                    snapshot: None,
                    refresh: Some(HardwareRefreshState {
                        attempted_at: accepted.received_at,
                        outcome: Some(hardware_refresh_state::Outcome::Failed(failure)),
                    }),
                    observation_freshness: Some(connected_freshness(&accepted)),
                },
            ]
        );
    }

    async fn configured_target_response(service: &SiteService) -> Vec<ConfiguredExecutionTarget> {
        SiteServiceContract::list_configured_execution_targets(
            service,
            Request::new(ListConfiguredExecutionTargetsRequest {}),
        )
        .await
        .expect("configured execution targets")
        .into_inner()
        .execution_targets
    }

    async fn machine_response(service: &SiteService, machine_id: &str) -> Machine {
        SiteServiceContract::list_machines(service, Request::new(ListMachinesRequest {}))
            .await
            .expect("machine list")
            .into_inner()
            .machines
            .into_iter()
            .find(|machine| machine.machine_id == machine_id)
            .expect("configured machine")
    }

    /// Publishes an empty view on a new stream of the node, which is authenticated contact.
    fn publish_as(coordinator: &ManagerNodeControlCoordinator, site: &Site, node_id: &str) {
        coordinator
            .publish_view_for_test(
                node_id,
                &NodeIncarnation::parse("1".repeat(32)).expect("incarnation"),
                PublishedViewInput {
                    site_id: site.site_id.clone(),
                    projection: NodeViewProjection::new(Vec::new(), Vec::new())
                        .expect("empty view"),
                },
            )
            .expect("node publication");
    }

    fn service_with_registry(site: Site) -> (SiteService, NodeRegistry) {
        service_for(site, "manager-node")
    }

    fn service_with_manager(site: Site) -> (SiteService, NodeRegistry) {
        service_for(site, "node-a")
    }

    fn service_for(site: Site, manager_node_id: &str) -> (SiteService, NodeRegistry) {
        let site: Arc<dyn SiteConfigurationSource> = Arc::new(site);
        let registry = NodeRegistry::new(Arc::clone(&site));
        (
            SiteService::new(
                site,
                SiteNodeId::for_test(manager_node_id),
                registry.clone(),
            ),
            registry,
        )
    }

    fn report(
        site: &Site,
        node_id: &str,
        revision: u64,
        hardware: node_hardware_observation::Result,
    ) -> NodeObservation {
        let configuration =
            node_observation_configuration(site, node_id).expect("node observation configuration");
        let runtimes = configuration
            .execution_targets
            .iter()
            .map(|target| RuntimeObservation {
                execution_target_id: target.execution_target_id.clone(),
                kind: target.kind,
                result: Some(runtime_observation::Result::Available(Empty {})),
            })
            .collect();
        report_with_runtimes(site, node_id, revision, hardware, runtimes)
    }

    fn node_report(
        site: &Site,
        revision: u64,
        hardware: node_hardware_observation::Result,
    ) -> NodeObservation {
        report_with_runtimes(site, "worker-node", revision, hardware, node_runtimes())
    }

    fn manager_report(
        site: &Site,
        revision: u64,
        runtimes: Vec<RuntimeObservation>,
    ) -> NodeObservation {
        report_with_runtimes(
            site,
            "node-a",
            revision,
            node_hardware_observation::Result::Snapshot(reported_snapshot()),
            runtimes,
        )
    }

    fn report_with_runtimes(
        site: &Site,
        node_id: &str,
        revision: u64,
        hardware: node_hardware_observation::Result,
        runtimes: Vec<RuntimeObservation>,
    ) -> NodeObservation {
        let configuration =
            node_observation_configuration(site, node_id).expect("node observation configuration");
        let digest = NodeObservationConfigurationDigest::for_configuration(&configuration)
            .expect("node observation digest");
        NodeObservation {
            site_id: site.site_id.clone(),
            node_id: node_id.to_owned(),
            incarnation_id: "1".repeat(32),
            revision,
            snapshot: Some(NodeObservationSnapshot {
                hardware: Some(ReportedHardwareObservation {
                    result: Some(hardware),
                }),
                runtimes,
            }),
            observation_configuration_sha256: digest.as_str().to_owned(),
            private_port: None,
        }
    }

    fn node_runtimes() -> Vec<RuntimeObservation> {
        vec![
            RuntimeObservation {
                execution_target_id: "application-target".to_owned(),
                kind: RuntimeKind::Application as i32,
                result: Some(runtime_observation::Result::Unsupported(
                    ObservationUnavailable {
                        code: "RUNTIME_UNSUPPORTED".to_owned(),
                        message: "Runtime is unsupported.".to_owned(),
                    },
                )),
            },
            RuntimeObservation {
                execution_target_id: "model-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                result: Some(runtime_observation::Result::Available(Empty {})),
            },
            RuntimeObservation {
                execution_target_id: "unavailable-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                result: Some(runtime_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "RUNTIME_UNAVAILABLE".to_owned(),
                        message: "Runtime is unavailable.".to_owned(),
                    },
                )),
            },
        ]
    }

    fn model_runtime(
        execution_target_id: &str,
        result: runtime_observation::Result,
    ) -> RuntimeObservation {
        RuntimeObservation {
            execution_target_id: execution_target_id.to_owned(),
            kind: RuntimeKind::Model as i32,
            result: Some(result),
        }
    }

    fn runtime_availability(
        target: &ConfiguredExecutionTarget,
    ) -> Option<&runtime_target_availability::Availability> {
        target
            .availability
            .as_ref()
            .and_then(|availability| availability.runtime_availability.as_ref())
            .and_then(|runtime| runtime.availability.as_ref())
    }

    /// Single-node site whose manager node `node-a` hosts two simulated model targets.
    fn manager_site() -> Site {
        Site {
            site_id: "site-one".to_owned(),
            nodes: vec![SiteNode {
                node_id: "node-a".to_owned(),
                display_name: "A".to_owned(),
                host: "127.0.0.1".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            }],
            execution_targets: vec![execution_target("target-b"), execution_target("target-a")],
            hardware_observation: Some(HardwareObservationConfiguration {
                refresh_interval_seconds: Some(5),
            }),
            ..Default::default()
        }
    }

    fn node_site() -> Site {
        let platform = OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        };
        let target = |id: &str, node: &str, application: bool| ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: format!("Target {id}"),
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
                        simulated_model_runtime(),
                    )),
                })
            }),
        };
        Site {
            site_id: "site-one".to_owned(),
            nodes: vec![
                SiteNode {
                    node_id: "manager-node".to_owned(),
                    display_name: "Manager".to_owned(),
                    host: "127.0.0.1".to_owned(),
                    site_manager: Some(SiteManagerRole {}),
                    manager_host: String::new(),
                },
                SiteNode {
                    node_id: "worker-node".to_owned(),
                    display_name: "Node".to_owned(),
                    host: "127.0.0.2".to_owned(),
                    site_manager: None,
                    manager_host: String::new(),
                },
            ],
            execution_targets: vec![
                target("manager-application", "manager-node", true),
                target("application-target", "worker-node", true),
                target("model-target", "worker-node", false),
                target("unavailable-target", "worker-node", false),
            ],
            hardware_observation: Some(HardwareObservationConfiguration {
                refresh_interval_seconds: Some(5),
            }),
            ..Default::default()
        }
    }

    fn simulated_model_runtime() -> SimulatedRuntimeAdapter {
        SimulatedRuntimeAdapter {
            default_scenario: Some(dotcore_proto::dotcore::site::v1::SimulatedRuntimeScenario {
                ready_delay_millis: 1,
                stop_delay_millis: 1,
            }),
            deployment_scenarios: Vec::new(),
        }
    }

    fn reported_snapshot() -> HardwareSnapshot {
        HardwareSnapshot {
            observed_at: Some(timestamp(100)),
            host: Some(HostObservation {
                result: Some(host_observation::Result::Facts(HostFacts {
                    host_name: "worker-node".to_owned(),
                    operating_system_or_kernel: "NixOS".to_owned(),
                    cpu_count: 8,
                    total_memory_bytes: 16_000,
                    available_memory_bytes: 8_000,
                    cached_memory_bytes: None,
                    filesystems: vec![dotcore_proto::dotcore::observation::v1::FilesystemFacts {
                        mountpoint: "/".to_owned(),
                        fstype: "ext4".to_owned(),
                        size_bytes: 100_000,
                    }],
                })),
            }),
            accelerators: Vec::new(),
        }
    }

    const MANAGER_GPU: &str = "GPU-5e2b9c71-0a4d-4c3e-9f18-7d6a2b1c0e94";
    const WORKER_FIRST_GPU: &str = "GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b";
    const WORKER_SECOND_GPU: &str = "GPU-3c1e7b52-8d0a-4f6e-b1a9-52d7e0c4a816";

    /// Reported snapshot with one dedicated NVIDIA GPU per id, enumerated in the given order.
    fn nvidia_snapshot(device_ids: &[&str]) -> HardwareSnapshot {
        HardwareSnapshot {
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Nvidia as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet {
                        facts: device_ids
                            .iter()
                            .zip(0..)
                            .map(|(device_id, index)| AcceleratorFacts {
                                vendor: AcceleratorVendor::Nvidia as i32,
                                model: "NVIDIA GeForce RTX 5090".to_owned(),
                                memory_architecture: AcceleratorMemoryArchitecture::Dedicated
                                    as i32,
                                total_memory_bytes: Some(34_190_917_632),
                                driver_or_runtime_version: "580.95.05".to_owned(),
                                device_id: (*device_id).to_owned(),
                                vendor_index: Some(index),
                                used_memory_bytes: Some(0),
                                working_set_limit_bytes: None,
                                supported_signals: Vec::new(),
                            })
                            .collect(),
                    },
                )),
            }],
            ..reported_snapshot()
        }
    }

    fn resolutions(
        targets: &[ConfiguredExecutionTarget],
    ) -> Vec<(&str, Vec<&str>, AcceleratorResolutionStatus)> {
        targets
            .iter()
            .map(|target| {
                (
                    target.execution_target_id.as_str(),
                    target
                        .observed_accelerator_device_ids
                        .iter()
                        .map(String::as_str)
                        .collect(),
                    target.accelerator_resolution(),
                )
            })
            .collect()
    }

    async fn current(service: &SiteService, machine_id: &str) -> MachineHardwareSnapshot {
        hardware_list(service)
            .await
            .into_iter()
            .find(|machine| machine.machine_id == machine_id)
            .expect("configured machine hardware")
    }

    async fn hardware_list(service: &SiteService) -> Vec<MachineHardwareSnapshot> {
        SiteServiceContract::list_hardware_snapshots(
            service,
            Request::new(ListHardwareSnapshotsRequest {}),
        )
        .await
        .expect("hardware snapshot list")
        .into_inner()
        .machines
    }

    fn execution_target(execution_target_id: &str) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: execution_target_id.to_owned(),
            display_name: execution_target_id.to_owned(),
            site_node_id: "node-a".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: vec!["configured-device".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        simulated_model_runtime(),
                    )),
                },
            )),
        }
    }

    fn timestamp(seconds: i64) -> Timestamp {
        Timestamp { seconds, nanos: 0 }
    }

    fn connected_freshness(accepted: &NodeObservationAcceptance) -> NodeObservationFreshness {
        observation_freshness(NodeObservationState::Connected, accepted.received_at)
    }

    fn target_connection_states(targets: &[ConfiguredExecutionTarget]) -> Vec<i32> {
        targets
            .iter()
            .map(|target| {
                target
                    .availability
                    .as_ref()
                    .expect("target availability")
                    .connection_state
            })
            .collect()
    }

    fn available() -> CurrentHardwareSnapshotAvailability {
        CurrentHardwareSnapshotAvailability {
            availability: Some(
                current_hardware_snapshot_availability::Availability::Available(Empty {}),
            ),
        }
    }

    fn assert_remote_runtimes_stale(targets: &[ConfiguredExecutionTarget]) {
        for target in targets
            .iter()
            .filter(|target| target.site_node_id == "worker-node")
        {
            assert_eq!(
                target
                    .availability
                    .as_ref()
                    .expect("target availability")
                    .runtime_availability
                    .as_ref()
                    .expect("Runtime availability")
                    .availability,
                Some(runtime_target_availability::Availability::Unavailable(
                    ObservationUnavailable {
                        code: RUNTIME_OBSERVATION_STALE_CODE.to_owned(),
                        message: RUNTIME_OBSERVATION_STALE_MESSAGE.to_owned(),
                    }
                ))
            );
        }
    }

    fn unavailable() -> CurrentHardwareSnapshotAvailability {
        CurrentHardwareSnapshotAvailability {
            availability: Some(
                current_hardware_snapshot_availability::Availability::Unavailable(
                    ObservationUnavailable {
                        code: CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_CODE.to_owned(),
                        message: CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE_MESSAGE.to_owned(),
                    },
                ),
            ),
        }
    }
}
