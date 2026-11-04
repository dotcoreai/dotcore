//! Accelerator memory attribution for application workload groups, by unit: the node-local
//! attribution from the group's node-qualified host-process identities and the node's own
//! hardware observations, by the attribution rule shared with model deployments, and the
//! manager's read-time projection of what the hosting node last reported.
//!
//! Each unit of a deployment projects its own result: a deployment that is not running, or whose
//! retained snapshot received no accelerator device, reads as not applicable, and a unit without
//! an observed host-process identity, or one the node's process observations do not cover, reads
//! as unavailable with its reason, never as a false zero. On the manager, a unit the hosting
//! node's current status does not report reads as unavailable. Clones share the same identities,
//! so the realizer that observes groups and the readers that project them read one view.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use dotcore_domain::host_process::HostProcessIdentity;
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationLifecycleState,
        ApplicationServiceAcceleratorMemory,
    },
    deployment::v1::AcceleratorMemoryObservation,
    site::v1::ExecutionTarget,
};
use dotcore_runtime::WorkloadGroupObservation;
use dotcore_site::node::hardware_inventory::HardwareCollection;

use crate::hardware::{
    accelerator_presence,
    attribution::{
        AttributionScope, attribution_scope, node_status_unavailable, not_applicable,
        project_identities,
    },
};

/// Attributes accelerator memory to the application deployments realized in this process, by
/// unit.
#[derive(Clone, Default)]
pub(super) struct GroupAcceleratorAttribution {
    /// Observed host-process identity of every running unit, by deployment and unit name.
    identities: Arc<Mutex<BTreeMap<String, BTreeMap<String, HostProcessIdentity>>>>,
}

impl GroupAcceleratorAttribution {
    /// Records the observed host-process identities of one group's units; an observation
    /// without safe units clears every identity it cannot name.
    pub(super) fn observe(&self, deployment_id: &str, observation: &WorkloadGroupObservation) {
        let units = match observation {
            WorkloadGroupObservation::Observed(group)
            | WorkloadGroupObservation::StaleConfiguration { group, .. } => &group.units,
            WorkloadGroupObservation::Absent { .. } | WorkloadGroupObservation::Unknown { .. } => {
                self.forget(deployment_id);
                return;
            }
        };
        let mut units_by_identity = BTreeMap::new();
        for (unit, observed) in units {
            if let Some(identity) = &observed.host_process {
                units_by_identity.insert(unit.0.clone(), identity.clone());
            }
        }
        let mut identities = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match identities.get_mut(deployment_id) {
            Some(stored) => *stored = units_by_identity,
            None => {
                if !units_by_identity.is_empty() {
                    identities.insert(deployment_id.to_owned(), units_by_identity);
                }
            }
        }
    }

    /// Drops every observed identity of one deployment.
    pub(super) fn forget(&self, deployment_id: &str) {
        self.identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(deployment_id);
    }

    /// Retains exactly the deployments still assigned this pass, dropping every other view.
    pub(super) fn retain(&self, retained: &BTreeSet<&str>) {
        let mut identities = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identities.retain(|deployment_id, _| retained.contains(&deployment_id.as_str()));
    }

    /// Projects one attributed result per named unit of the deployment in its `lifecycle`, for
    /// its assigned execution target and the accelerator devices its snapshot retained, from one
    /// of the node's hardware collections.
    pub(super) fn projection(
        &self,
        deployment: &ApplicationDeploymentDeclaration,
        lifecycle: ApplicationLifecycleState,
        targets: &[ExecutionTarget],
        collection: &HardwareCollection,
    ) -> BTreeMap<String, AcceleratorMemoryObservation> {
        let Some(target) = placed_target(deployment, targets) else {
            return BTreeMap::new();
        };
        let Some(scope) = deployment_scope(deployment, lifecycle, target) else {
            return deployment_unit_names(deployment)
                .into_iter()
                .map(|unit| (unit, not_applicable()))
                .collect();
        };

        let identities = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&deployment.application_deployment_id)
            .cloned()
            .unwrap_or_default();
        deployment_unit_names(deployment)
            .into_iter()
            .map(|unit| {
                let unit_identities = identities
                    .get(&unit)
                    .cloned()
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                // Applications hold no claim in capacity, so a unit recorded after the
                // collection keeps reading not observed instead of pending.
                let result =
                    project_identities(&scope, &unit_identities, &BTreeSet::new(), collection);
                (unit, result)
            })
            .collect()
    }
}

/// Projects one manager-side result per named unit of a node-hosted deployment, in graph order:
/// the memory its hosting node's current status reports for the unit, else unavailable while
/// attribution applies to the deployment in `lifecycle`, else not applicable.
pub(super) fn node_hosted_projection(
    deployment: &ApplicationDeploymentDeclaration,
    lifecycle: ApplicationLifecycleState,
    targets: &[ExecutionTarget],
    reported: Vec<ApplicationServiceAcceleratorMemory>,
) -> Vec<ApplicationServiceAcceleratorMemory> {
    let Some(target) = placed_target(deployment, targets) else {
        return Vec::new();
    };
    let applies = deployment_scope(deployment, lifecycle, target).is_some();
    let mut reported = reported
        .into_iter()
        .filter_map(|unit| Some((unit.service_name, unit.accelerator_memory?)))
        .collect::<BTreeMap<_, _>>();
    deployment_unit_names(deployment)
        .into_iter()
        .map(|unit| {
            let accelerator_memory = reported.remove(&unit).unwrap_or_else(|| {
                if applies {
                    node_status_unavailable()
                } else {
                    not_applicable()
                }
            });
            ApplicationServiceAcceleratorMemory {
                service_name: unit,
                accelerator_memory: Some(accelerator_memory),
            }
        })
        .collect()
}

/// The deployment's assigned execution target among `targets`.
fn placed_target<'a>(
    deployment: &ApplicationDeploymentDeclaration,
    targets: &'a [ExecutionTarget],
) -> Option<&'a ExecutionTarget> {
    let placement = deployment.placement.as_ref()?;
    targets
        .iter()
        .find(|target| placement.execution_target_id == target.execution_target_id)
}

/// Where attribution applies to the deployment on `target` in `lifecycle`, driven by the
/// accelerator devices its snapshot retained rather than the live target.
fn deployment_scope(
    deployment: &ApplicationDeploymentDeclaration,
    lifecycle: ApplicationLifecycleState,
    target: &ExecutionTarget,
) -> Option<AttributionScope> {
    let active = !matches!(
        lifecycle,
        ApplicationLifecycleState::Paused
            | ApplicationLifecycleState::Stopped
            | ApplicationLifecycleState::Failed
    );
    let retained_devices = deployment
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.accelerator_device_ids.as_slice())
        .unwrap_or_default();
    attribution_scope(active, target, accelerator_presence(retained_devices))
}

/// The deployment's unit names, in graph order.
fn deployment_unit_names(deployment: &ApplicationDeploymentDeclaration) -> Vec<String> {
    deployment
        .snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .service_selections
                .iter()
                .map(|selection| selection.service_name.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    use dotcore_domain::hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
        AcceleratorVendorProcessResult, NodeHardwareObservation, ObservedAcceleratorProcess,
    };
    use dotcore_domain::{SiteNodeId, host_process::HostProcessIdentity};
    use dotcore_host_process::ProcessLifetime;
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentDeclaration, ResolvedApplicationDeploymentSnapshot,
            ResolvedApplicationServiceSelection,
        },
        deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryUsageSet, ExactPlacement,
            accelerator_memory_observation, accelerator_memory_refresh_state,
        },
        observation::v1::{AcceleratorVendor, HardwareSnapshot, ObservationUnavailable},
        site::v1::{
            ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget,
            application_runtime_binding, execution_target,
        },
    };
    use dotcore_runtime::{
        ObservedWorkloadGroup, RuntimeBackendId, WorkloadCorrelation, WorkloadGroupId,
        WorkloadGroupObservation, WorkloadUnitName, WorkloadUnitObservation, WorkloadUnitState,
    };
    use dotcore_site::node::hardware_inventory::{
        HardwareInventoryEntry, HardwareInventoryPublisher, HardwareInventorySource,
    };

    use super::*;

    fn deployment() -> ApplicationDeploymentDeclaration {
        ApplicationDeploymentDeclaration {
            application_deployment_id: "deployment-a".to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "node-a".to_owned(),
                execution_target_id: "target-a".to_owned(),
            }),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                service_selections: vec![
                    ResolvedApplicationServiceSelection {
                        service_name: "cache".to_owned(),
                        ..Default::default()
                    },
                    ResolvedApplicationServiceSelection {
                        service_name: "web".to_owned(),
                        ..Default::default()
                    },
                ],
                accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn target() -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "target-a".to_owned(),
            display_name: "target-a".to_owned(),
            site_node_id: SiteNodeId::for_test("node-a").to_string(),
            accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                        DockerApplicationRuntimeAdapter::default(),
                    )),
                },
            )),
            platform: None,
        }
    }

    fn simulated_target() -> ExecutionTarget {
        ExecutionTarget {
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..target()
        }
    }

    fn identity(node_id: &str, process_id: u32, process_start_token: u64) -> HostProcessIdentity {
        HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test(node_id),
            process_id,
            process_start_token,
        }
    }

    fn observed_group(
        owner: &str,
        units: &[(&str, Option<HostProcessIdentity>)],
    ) -> WorkloadGroupObservation {
        WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
            group_id: WorkloadGroupId(format!("{owner}-group")),
            correlation: WorkloadCorrelation {
                owner_id: owner.to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId("docker".to_owned()),
            units: units
                .iter()
                .map(|(name, identity)| {
                    (
                        WorkloadUnitName(name.to_string()),
                        WorkloadUnitObservation {
                            state: WorkloadUnitState::Running,
                            ready_launch_gate: None,
                            host_process: identity.clone(),
                        },
                    )
                })
                .collect(),
            assigned_endpoint: None,
        })
    }

    fn process(
        device_id: &str,
        process_id: u32,
        process_start_token: u64,
        used_memory_bytes: Option<u64>,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            device_id: device_id.to_owned(),
            process_id,
            process_start_token,
            ancestors: Vec::new(),
            used_memory_bytes,
        }
    }

    fn publish_node(
        publisher: &HardwareInventoryPublisher,
        processes: Vec<AcceleratorVendorProcessObservation>,
    ) {
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        ..Default::default()
                    },
                    accelerator_processes: AcceleratorProcessObservation::Observed(processes),
                },
                timestamp(11),
            ),
        );
    }

    fn nvidia(processes: Vec<ObservedAcceleratorProcess>) -> AcceleratorVendorProcessObservation {
        AcceleratorVendorProcessObservation {
            vendor: AcceleratorVendor::Nvidia,
            result: AcceleratorVendorProcessResult::Observed(processes),
        }
    }

    fn unavailable(code: &str, message: &str) -> Option<accelerator_memory_observation::Result> {
        Some(accelerator_memory_observation::Result::Unavailable(
            dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        ))
    }

    fn succeeded_refresh() -> dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryRefreshState
    {
        dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryRefreshState {
            attempted_at: Some(timestamp(11)),
            outcome: Some(accelerator_memory_refresh_state::Outcome::Succeeded(
                dotcore_proto::google::protobuf::Empty {},
            )),
        }
    }

    fn node_a_usage(used_memory_bytes: u64) -> Option<accelerator_memory_observation::Result> {
        Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet {
                devices: vec![AcceleratorMemoryDeviceUsage {
                    managed_node_id: SiteNodeId::for_test("node-a").to_string(),
                    device_id: "GPU-a".to_owned(),
                    used_memory_bytes,
                }],
            },
        ))
    }

    fn not_applicable_memory() -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::NotApplicable(
                dotcore_proto::google::protobuf::Empty {},
            )),
        }
    }

    fn timestamp(seconds: i64) -> dotcore_proto::google::protobuf::Timestamp {
        dotcore_proto::google::protobuf::Timestamp { seconds, nanos: 0 }
    }

    #[test]
    fn running_units_project_their_own_node_process_usage() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(
            &publisher,
            vec![nvidia(vec![
                process("GPU-a", 41, 700, Some(1000)),
                process("GPU-a", 42, 701, Some(2000)),
            ])],
        );
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[
                    ("cache", Some(identity("node-a", 42, 701))),
                    ("web", Some(identity("node-a", 41, 700))),
                ],
            ),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(projection["cache"].result, node_a_usage(2000));
        assert_eq!(projection["web"].result, node_a_usage(1000));
    }

    #[test]
    fn child_process_rows_attribute_to_the_unit_root() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(
            &publisher,
            vec![nvidia(vec![ObservedAcceleratorProcess {
                ancestors: vec![ProcessLifetime {
                    process_id: 41,
                    start_token: 700,
                }],
                ..process("GPU-a", 55, 900, Some(500))
            }])],
        );
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-a", 41, 700)))],
            ),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(projection["web"].result, node_a_usage(500));
        assert_eq!(
            projection["cache"].result,
            unavailable(
                "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                "Runtime did not report an authoritative workload process identity."
            )
        );
    }

    #[test]
    fn units_without_an_observed_identity_read_the_node_refresh() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(&publisher, vec![nvidia(Vec::new())]);
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group("deployment-a", &[("web", None), ("cache", None)]),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        for unit in ["cache", "web"] {
            assert_eq!(
                projection[unit].result,
                unavailable(
                    "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                    "Runtime did not report an authoritative workload process identity."
                )
            );
            assert_eq!(projection[unit].refresh, Some(succeeded_refresh()));
        }
    }

    #[test]
    fn a_snapshot_without_retained_devices_reads_not_applicable() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let attribution = GroupAcceleratorAttribution::default();
        let mut deviceless = deployment();
        deviceless
            .snapshot
            .as_mut()
            .expect("snapshot")
            .accelerator_device_ids
            .clear();

        // The retained snapshot decides, even when the live target now lists a device.
        let projection = attribution.projection(
            &deviceless,
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        for unit in ["cache", "web"] {
            assert_eq!(projection[unit], not_applicable_memory());
        }
    }

    #[test]
    fn paused_stopped_and_failed_deployments_read_not_applicable() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(
            &publisher,
            vec![nvidia(vec![process("GPU-a", 41, 700, Some(1000))])],
        );
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-a", 41, 700)))],
            ),
        );

        for lifecycle in [
            ApplicationLifecycleState::Paused,
            ApplicationLifecycleState::Stopped,
            ApplicationLifecycleState::Failed,
        ] {
            let projection = attribution.projection(
                &deployment(),
                lifecycle,
                &[target()],
                &inventory.collection(),
            );
            assert_eq!(projection["web"], not_applicable_memory(), "{lifecycle:?}");
        }
        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Ready,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(projection["web"].result, node_a_usage(1000));
    }

    #[test]
    fn a_simulated_target_reads_not_applicable() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let attribution = GroupAcceleratorAttribution::default();

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[simulated_target()],
            &inventory.collection(),
        );
        for unit in ["cache", "web"] {
            assert_eq!(projection[unit], not_applicable_memory());
        }
    }

    #[test]
    fn a_vendor_unavailable_reads_the_vendor_reason() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(
            &publisher,
            vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Nvidia,
                result: AcceleratorVendorProcessResult::Unavailable(ObservationUnavailable {
                    code: "NVML_FAILED".to_owned(),
                    message: "The NVML process observation failed.".to_owned(),
                }),
            }],
        );
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-a", 41, 700)))],
            ),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(
            projection["web"].result,
            unavailable("NVML_FAILED", "The NVML process observation failed.")
        );
        assert_eq!(projection["web"].refresh, Some(succeeded_refresh()));
    }

    #[test]
    fn apple_nodes_never_attribute_group_units_from_empty_rows() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(
            &publisher,
            vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Apple,
                result: AcceleratorVendorProcessResult::Observed(Vec::new()),
            }],
        );
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-a", 41, 700)))],
            ),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(
            projection["web"].result,
            unavailable(
                "PROCESS_NOT_OBSERVED",
                "An authoritative workload process was not observed on an accelerator."
            )
        );
    }

    #[test]
    fn an_identity_outside_the_target_node_reads_node_mismatch() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-b", 41, 700)))],
            ),
        );

        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(
            projection["web"].result,
            unavailable(
                "WORKLOAD_NODE_MISMATCH",
                "Runtime reported a workload process outside the deployment execution target."
            )
        );
    }

    #[test]
    fn retain_and_absent_observations_drop_the_stored_identities() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node(&publisher, vec![nvidia(Vec::new())]);
        let attribution = GroupAcceleratorAttribution::default();
        attribution.observe(
            "deployment-a",
            &observed_group(
                "deployment-a",
                &[("web", Some(identity("node-a", 41, 700)))],
            ),
        );
        let mut other = deployment();
        other.application_deployment_id = "deployment-b".to_owned();
        attribution.observe(
            "deployment-b",
            &observed_group(
                "deployment-b",
                &[("web", Some(identity("node-a", 42, 701)))],
            ),
        );

        attribution.retain(&std::collections::BTreeSet::from(["deployment-a"]));
        let projection = attribution.projection(
            &other,
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(
            projection["web"].result,
            unavailable(
                "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                "Runtime did not report an authoritative workload process identity."
            )
        );

        attribution.observe(
            "deployment-a",
            &WorkloadGroupObservation::Unknown {
                group_id: WorkloadGroupId("deployment-a-group".to_owned()),
                backend_id: RuntimeBackendId("docker".to_owned()),
            },
        );
        let projection = attribution.projection(
            &deployment(),
            ApplicationLifecycleState::Running,
            &[target()],
            &inventory.collection(),
        );
        assert_eq!(
            projection["web"].result,
            unavailable(
                "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                "Runtime did not report an authoritative workload process identity."
            )
        );
    }
}
