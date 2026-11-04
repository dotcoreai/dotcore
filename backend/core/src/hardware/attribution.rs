//! Attributes a realized workload's accelerator memory on its hosting node, the one rule both
//! workload controllers apply to their own workloads.
//!
//! The scope decides whether attribution applies at all: a workload that is not running, or one
//! whose target hands it no accelerator, has nothing to attribute. Where it applies, the node
//! relates the runtime's node-qualified process identities to its own per-process accelerator
//! observations. Missing or unusable evidence reads as unavailable with its reason, never as
//! device-wide memory or a false zero; a process that holds no accelerator memory is never
//! observed on an accelerator, so it reads as not observed rather than zero, once a collection
//! read after the process was recorded misses it. Before that, its memory reads as pending.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_domain::SiteNodeId;
use dotcore_domain::hardware_observation::{
    AcceleratorProcessObservation, AcceleratorVendorProcessResult,
};
use dotcore_domain::host_process::HostProcessIdentity;
use dotcore_host_process::ProcessLifetime;
use dotcore_proto::{
    dotcore::{
        deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation,
            AcceleratorMemoryRefreshState, AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet,
            accelerator_memory_observation, accelerator_memory_refresh_state,
        },
        site::v1::{
            ExecutionTarget, application_runtime_binding, execution_target, hardware_refresh_state,
            model_runtime_binding,
        },
    },
    google::protobuf::Empty,
};
use dotcore_site::node::hardware_inventory::{HardwareCollection, HardwareInventoryEntry};

use super::AcceleratorPresence;

/// Reason a workload's process memory has no reading yet: no collection read since its process
/// was recorded has measured it. Capacity keeps such a workload's claim while it lasts.
pub(crate) const PROCESS_MEMORY_PENDING: &str = "WORKLOAD_PROCESS_MEMORY_PENDING";

/// Where accelerator memory attribution applies to one workload: its hosting node and how that
/// node attributes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttributionScope {
    pub(crate) node: SiteNodeId,
    pub(crate) source: AttributionSource,
}

/// How the hosting node attributes a workload's accelerator memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttributionSource {
    /// The node relates the workload's process identities to its own per-process accelerator
    /// observations.
    ProcessObservations,

    /// A simulated runtime attributes only the processes it simulates.
    Simulated,

    /// The runtime reports the workload's process memory itself; process observations apply
    /// only when it reports none.
    RuntimeReported,
}

/// Returns where accelerator memory attribution applies to a workload on `target`, or none when
/// it does not: the workload is not `active`, its target's runtime realizes no accelerator
/// workloads, or a container target hands it no accelerator device. `devices` comes from the
/// owner's record of what the workload received, such as a retained snapshot, not necessarily
/// from the live target.
pub(crate) fn attribution_scope(
    active: bool,
    target: &ExecutionTarget,
    devices: AcceleratorPresence,
) -> Option<AttributionScope> {
    if !active {
        return None;
    }
    let source = match (workload_runtime(target)?, devices) {
        (WorkloadRuntime::Simulated, _) => AttributionSource::Simulated,
        (WorkloadRuntime::Native, _) => AttributionSource::RuntimeReported,
        (WorkloadRuntime::Docker, AcceleratorPresence::Devices) => {
            AttributionSource::ProcessObservations
        }
        (WorkloadRuntime::Docker, AcceleratorPresence::None) => return None,
    };
    // Site validation already constrains every declared execution target to an existing
    // SiteNode identity, so an unparsable value cannot claim an accelerator placement.
    let node = SiteNodeId::parse(&target.site_node_id).ok()?;
    Some(AttributionScope { node, source })
}

/// Projects a scoped workload's memory from the process identities its runtime reported, against
/// one hardware collection. `recorded_after` names the identities recorded after that collection
/// began reading hardware: while each identity it misses is one of them, the workload's memory
/// reads as pending rather than not observed, since the collection may predate the process. A
/// workload without an identity reads as unavailable, except on a simulated runtime, which
/// attributes only the processes it simulates.
pub(crate) fn project_identities(
    scope: &AttributionScope,
    identities: &BTreeSet<HostProcessIdentity>,
    recorded_after: &BTreeSet<HostProcessIdentity>,
    collection: &HardwareCollection,
) -> AcceleratorMemoryObservation {
    if identities.is_empty() && scope.source == AttributionSource::Simulated {
        return not_applicable();
    }
    project_process_usage(&scope.node, identities, recorded_after, collection)
}

/// A stable unavailable reason without a refresh state.
pub(crate) fn unavailable(code: &str, message: &str) -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        observed_at: None,
        refresh: None,
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        )),
    }
}

/// The manager's result for a workload whose hosting node has no current status reporting its
/// memory.
pub(crate) fn node_status_unavailable() -> AcceleratorMemoryObservation {
    unavailable(
        "NODE_STATUS_UNAVAILABLE",
        "The hosting node has not reported current status for this deployment.",
    )
}

/// A result for a workload that has no local accelerator workload to attribute.
pub(crate) fn not_applicable() -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        observed_at: None,
        refresh: None,
        result: Some(accelerator_memory_observation::Result::NotApplicable(
            Empty {},
        )),
    }
}

/// An unavailable reason resolved against one node's current observation, or a pending one when
/// the observation has not completed.
pub(crate) fn unavailable_with_node_refresh(
    entry: Option<&HardwareInventoryEntry>,
    code: &str,
    message: &str,
) -> AcceleratorMemoryObservation {
    let Some(entry) = entry else {
        return unavailable_pending(
            "NODE_OBSERVATION_PENDING",
            "Hardware observation has not completed for this managed node.",
        );
    };
    match refresh_state(entry) {
        Ok(refresh) => unavailable_with_refresh(refresh, code, message),
        Err(observation) => *observation,
    }
}

/// Runtime that realizes a target's workloads, as far as attribution distinguishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkloadRuntime {
    Docker,
    Native,
    Simulated,
}

/// Returns the runtime of a target's model or application binding, or none without one.
fn workload_runtime(target: &ExecutionTarget) -> Option<WorkloadRuntime> {
    match target.runtime_binding.as_ref()? {
        execution_target::RuntimeBinding::ModelRuntime(binding) => {
            match binding.adapter.as_ref()? {
                model_runtime_binding::Adapter::DockerRuntime(_) => Some(WorkloadRuntime::Docker),
                model_runtime_binding::Adapter::NativeRuntime(_) => Some(WorkloadRuntime::Native),
                model_runtime_binding::Adapter::SimulatedRuntime(_) => {
                    Some(WorkloadRuntime::Simulated)
                }
            }
        }
        execution_target::RuntimeBinding::ApplicationRuntime(binding) => {
            match binding.adapter.as_ref()? {
                application_runtime_binding::Adapter::DockerRuntime(_) => {
                    Some(WorkloadRuntime::Docker)
                }
                application_runtime_binding::Adapter::SimulatedRuntime(_) => {
                    Some(WorkloadRuntime::Simulated)
                }
            }
        }
    }
}

/// Projects the node-qualified process lifetimes of a set of authoritative host processes on
/// `node` into accelerator memory usage from `node`'s entry in one collection. An empty set reads
/// as an unavailable identity, never as usage on no device.
///
/// A process on another node, a missing observation, an unavailable vendor reading, or an
/// identity never observed on an accelerator reads as unavailable with the node's refresh state,
/// never as device-wide memory or a false zero; one recorded after the collection, in
/// `recorded_after`, reads as pending instead of never observed.
fn project_process_usage(
    node: &SiteNodeId,
    identities: &BTreeSet<HostProcessIdentity>,
    recorded_after: &BTreeSet<HostProcessIdentity>,
    collection: &HardwareCollection,
) -> AcceleratorMemoryObservation {
    let entry = collection.entry(node);
    if identities.is_empty() {
        return unavailable_with_node_refresh(
            entry,
            "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
            "Runtime did not report an authoritative workload process identity.",
        );
    }
    if identities
        .iter()
        .any(|identity| identity.managed_node_id != *node)
    {
        return unavailable_pending(
            "WORKLOAD_NODE_MISMATCH",
            "Runtime reported a workload process outside the deployment execution target.",
        );
    }

    let Some(entry) = entry else {
        return unavailable_pending(
            "NODE_OBSERVATION_PENDING",
            "Hardware observation has not completed for this managed node.",
        );
    };
    let refresh = match refresh_state(entry) {
        Ok(refresh) => refresh,
        Err(observation) => return *observation,
    };
    let observed_at = entry
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.observed_at);
    let Some(processes) = &entry.accelerator_processes else {
        return unavailable_with_refresh(
            refresh,
            "PROCESS_OBSERVATION_UNAVAILABLE",
            "Accelerator process observation is unavailable.",
        );
    };
    let vendors = match processes {
        AcceleratorProcessObservation::Unavailable(unavailable) => {
            return unavailable_with_refresh(refresh, &unavailable.code, &unavailable.message);
        }
        AcceleratorProcessObservation::Observed(vendors) => vendors,
    };
    let mut rows = Vec::new();
    for vendor in vendors {
        match &vendor.result {
            AcceleratorVendorProcessResult::Observed(observed) => rows.extend(observed),
            AcceleratorVendorProcessResult::Unavailable(unavailable) => {
                return unavailable_with_refresh(refresh, &unavailable.code, &unavailable.message);
            }
        }
    }

    let mut usage = BTreeMap::<String, u64>::new();
    let mut matched_identities = BTreeSet::new();
    for row in rows {
        // A worker process, such as a vLLM engine child, belongs to the workload whose root
        // process is itself or one of its ancestors.
        let lifetimes = std::iter::once(ProcessLifetime {
            process_id: row.process_id,
            start_token: row.process_start_token,
        })
        .chain(row.ancestors.iter().copied())
        .collect::<Vec<_>>();
        let row_identities = identities
            .iter()
            .filter(|identity| {
                lifetimes.contains(&ProcessLifetime {
                    process_id: identity.process_id,
                    start_token: identity.process_start_token,
                })
            })
            .collect::<Vec<_>>();
        if row_identities.is_empty() {
            continue;
        }
        matched_identities.extend(row_identities.into_iter().cloned());
        let Some(bytes) = row.used_memory_bytes else {
            return unavailable_with_refresh(
                refresh,
                "PROCESS_MEMORY_UNAVAILABLE",
                "Accelerator memory is unavailable for the attributed workload process.",
            );
        };
        // Each row counts once even when nested workload roots both match it.
        *usage.entry(row.device_id.clone()).or_default() += bytes;
    }
    let mut missed = identities.difference(&matched_identities).peekable();
    if missed.peek().is_some() {
        if missed.all(|identity| recorded_after.contains(identity)) {
            return unavailable_with_refresh(
                refresh,
                PROCESS_MEMORY_PENDING,
                "Workload process memory has not been observed since the process started.",
            );
        }
        return unavailable_with_refresh(
            refresh,
            "PROCESS_NOT_OBSERVED",
            "An authoritative workload process was not observed on an accelerator.",
        );
    }

    AcceleratorMemoryObservation {
        observed_at,
        refresh: Some(refresh),
        result: Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet {
                devices: usage
                    .into_iter()
                    .map(
                        |(device_id, used_memory_bytes)| AcceleratorMemoryDeviceUsage {
                            managed_node_id: node.as_str().to_owned(),
                            device_id,
                            used_memory_bytes,
                        },
                    )
                    .collect(),
            },
        )),
    }
}

/// A node refresh state projected into the accelerator memory result vocabulary, or the pending
/// result while the node's first observation has not completed.
fn refresh_state(
    entry: &HardwareInventoryEntry,
) -> Result<AcceleratorMemoryRefreshState, Box<AcceleratorMemoryObservation>> {
    match entry.refresh.outcome.as_ref() {
        Some(hardware_refresh_state::Outcome::Pending(_)) | None => {
            Err(Box::new(unavailable_pending(
                "NODE_OBSERVATION_PENDING",
                "Hardware observation has not completed for this managed node.",
            )))
        }
        Some(hardware_refresh_state::Outcome::Failed(failed)) => {
            Ok(AcceleratorMemoryRefreshState {
                attempted_at: entry.refresh.attempted_at,
                outcome: Some(accelerator_memory_refresh_state::Outcome::Failed(
                    AcceleratorMemoryUnavailable {
                        code: failed.code.clone(),
                        message: failed.message.clone(),
                    },
                )),
            })
        }
        Some(hardware_refresh_state::Outcome::Succeeded(_)) => Ok(AcceleratorMemoryRefreshState {
            attempted_at: entry.refresh.attempted_at,
            outcome: Some(accelerator_memory_refresh_state::Outcome::Succeeded(
                Empty {},
            )),
        }),
    }
}

/// An unavailable reason while the owning node's observation is still working.
fn unavailable_pending(code: &str, message: &str) -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        observed_at: None,
        refresh: Some(AcceleratorMemoryRefreshState {
            attempted_at: None,
            outcome: Some(accelerator_memory_refresh_state::Outcome::Pending(Empty {})),
        }),
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        )),
    }
}

/// An unavailable reason carrying the node refresh state it was read from.
fn unavailable_with_refresh(
    refresh: AcceleratorMemoryRefreshState,
    code: &str,
    message: &str,
) -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        observed_at: None,
        refresh: Some(refresh),
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        )),
    }
}

/// An unavailable reason while the owning node's observation is still working, for the unit tests
/// of the lanes that expect it.
#[cfg(test)]
pub(crate) fn pending_unavailable(code: &str, message: &str) -> AcceleratorMemoryObservation {
    unavailable_pending(code, message)
}

#[cfg(test)]
mod tests {
    use dotcore_domain::hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
        AcceleratorVendorProcessResult, NodeHardwareObservation, ObservedAcceleratorProcess,
    };
    use dotcore_proto::dotcore::{
        observation::v1::{AcceleratorVendor, HardwareSnapshot, ObservationUnavailable},
        site::v1::{
            ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ModelRuntimeBinding,
            SimulatedApplicationRuntimeAdapter,
        },
    };
    use dotcore_site::node::hardware_inventory::{
        HardwareInventoryPublisher, HardwareInventorySource,
    };

    use super::*;
    use crate::hardware::AcceleratorPresence::{Devices, None as NoDevices};

    #[test]
    fn scope_applies_only_to_running_workloads_that_receive_an_accelerator() {
        let docker = application_target(application_runtime_binding::Adapter::DockerRuntime(
            DockerApplicationRuntimeAdapter::default(),
        ));
        assert_eq!(
            attribution_scope(true, &docker, Devices),
            Some(scope(AttributionSource::ProcessObservations))
        );

        // A deviceless container and a workload that is not running have nothing to attribute.
        assert_eq!(attribution_scope(true, &docker, NoDevices), None);
        assert_eq!(attribution_scope(false, &docker, Devices), None);

        // A target without a runtime binding realizes no workload.
        let unbound = ExecutionTarget {
            runtime_binding: None,
            ..docker.clone()
        };
        assert_eq!(attribution_scope(true, &unbound, Devices), None);

        // An unparsable node identity can't claim an accelerator placement.
        let unparsable = ExecutionTarget {
            site_node_id: String::new(),
            ..docker
        };
        assert_eq!(attribution_scope(true, &unparsable, Devices), None);
    }

    #[test]
    fn simulated_and_native_runtimes_scope_without_devices() {
        let simulated = application_target(application_runtime_binding::Adapter::SimulatedRuntime(
            SimulatedApplicationRuntimeAdapter::default(),
        ));
        assert_eq!(
            attribution_scope(true, &simulated, NoDevices),
            Some(scope(AttributionSource::Simulated))
        );

        let native = ExecutionTarget {
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..simulated
        };
        assert_eq!(
            attribution_scope(true, &native, NoDevices),
            Some(scope(AttributionSource::RuntimeReported))
        );
        assert_eq!(attribution_scope(false, &native, Devices), None);
    }

    #[test]
    fn identities_project_sorted_usage_including_child_processes() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish(
            &publisher,
            observed(vec![
                row("GPU-b", 41, 700, &[], Some(30)),
                row("GPU-a", 41, 700, &[], Some(10)),
                // A worker child of the workload root counts toward it.
                row("GPU-a", 77, 900, &[(41, 700)], Some(5)),
                // A process outside the workload never counts.
                row("GPU-a", 99, 100, &[], Some(1_000)),
            ]),
        );

        let observation = project_identities(
            &scope(AttributionSource::ProcessObservations),
            &BTreeSet::from([identity("node-a", 41, 700)]),
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(observation.observed_at, Some(timestamp(10)));
        assert_eq!(
            observation.result,
            Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![device_usage("GPU-a", 15), device_usage("GPU-b", 30)],
                },
            ))
        );
    }

    #[test]
    fn an_empty_identity_set_reads_identity_unavailable_unless_simulated() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish(&publisher, observed(Vec::new()));

        assert_eq!(
            project_identities(
                &scope(AttributionSource::ProcessObservations),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &inventory.collection(),
            )
            .result,
            unavailable_result(
                "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE",
                "Runtime did not report an authoritative workload process identity.",
            )
        );
        assert_eq!(
            project_identities(
                &scope(AttributionSource::Simulated),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &inventory.collection()
            ),
            not_applicable()
        );
    }

    #[test]
    fn missing_or_unusable_evidence_reads_unavailable_with_its_reason() {
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);
        let process_scope = scope(AttributionSource::ProcessObservations);

        // No observation for the node yet.
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        assert_eq!(
            project_identities(
                &process_scope,
                &identities,
                &BTreeSet::new(),
                &inventory.collection()
            ),
            unavailable_pending(
                "NODE_OBSERVATION_PENDING",
                "Hardware observation has not completed for this managed node.",
            )
        );

        // A process on another node can't belong to the scoped workload.
        assert_eq!(
            project_identities(
                &process_scope,
                &BTreeSet::from([identity("node-b", 41, 700)]),
                &BTreeSet::new(),
                &inventory.collection()
            ),
            unavailable_pending(
                "WORKLOAD_NODE_MISMATCH",
                "Runtime reported a workload process outside the deployment execution target.",
            )
        );

        // A process that holds no accelerator memory is never observed on an accelerator.
        publish(
            &publisher,
            observed(vec![row("GPU-a", 99, 100, &[], Some(1))]),
        );
        assert_eq!(
            project_identities(
                &process_scope,
                &identities,
                &BTreeSet::new(),
                &inventory.collection()
            )
            .result,
            unavailable_result(
                "PROCESS_NOT_OBSERVED",
                "An authoritative workload process was not observed on an accelerator.",
            )
        );

        // A matched row without a memory reading is never a zero.
        publish(&publisher, observed(vec![row("GPU-a", 41, 700, &[], None)]));
        assert_eq!(
            project_identities(
                &process_scope,
                &identities,
                &BTreeSet::new(),
                &inventory.collection()
            )
            .result,
            unavailable_result(
                "PROCESS_MEMORY_UNAVAILABLE",
                "Accelerator memory is unavailable for the attributed workload process.",
            )
        );

        // An unavailable vendor reading hides any matching rows of other vendors.
        let unavailable = ObservationUnavailable {
            code: "NVML_UNAVAILABLE".to_owned(),
            message: "NVML is unavailable.".to_owned(),
        };
        publish(
            &publisher,
            AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Nvidia,
                result: AcceleratorVendorProcessResult::Unavailable(unavailable.clone()),
            }]),
        );
        let observation = project_identities(
            &process_scope,
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );
        assert_eq!(
            observation.result,
            unavailable_result(&unavailable.code, &unavailable.message)
        );
        assert_eq!(
            observation.refresh.and_then(|refresh| refresh.outcome),
            Some(accelerator_memory_refresh_state::Outcome::Succeeded(
                Empty {}
            ))
        );
    }

    #[test]
    fn a_process_recorded_after_the_collection_reads_pending_only_while_absent_from_it() {
        let process_scope = scope(AttributionSource::ProcessObservations);
        let recorded = identity("node-a", 41, 700);
        let earlier = identity("node-a", 42, 710);
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish(
            &publisher,
            observed(vec![row("GPU-a", 99, 100, &[], Some(1))]),
        );
        let pending = unavailable_with_refresh(
            succeeded_refresh(),
            PROCESS_MEMORY_PENDING,
            "Workload process memory has not been observed since the process started.",
        );

        // The collection was read before the process was recorded, so its absence is no finding.
        assert_eq!(
            project_identities(
                &process_scope,
                &BTreeSet::from([recorded.clone()]),
                &BTreeSet::from([recorded.clone()]),
                &inventory.collection(),
            ),
            pending
        );

        // A process recorded before the collection was read and absent from it was not observed,
        // whatever the other processes of the workload.
        let not_observed = unavailable_result(
            "PROCESS_NOT_OBSERVED",
            "An authoritative workload process was not observed on an accelerator.",
        );
        assert_eq!(
            project_identities(
                &process_scope,
                &BTreeSet::from([recorded.clone()]),
                &BTreeSet::new(),
                &inventory.collection(),
            )
            .result,
            not_observed
        );
        assert_eq!(
            project_identities(
                &process_scope,
                &BTreeSet::from([recorded.clone(), earlier]),
                &BTreeSet::from([recorded.clone()]),
                &inventory.collection(),
            )
            .result,
            not_observed
        );

        // A collection that shows the process measures it, however recently it was recorded.
        publish(
            &publisher,
            observed(vec![row("GPU-a", 41, 700, &[], Some(30))]),
        );
        assert_eq!(
            project_identities(
                &process_scope,
                &BTreeSet::from([recorded.clone()]),
                &BTreeSet::from([recorded]),
                &inventory.collection(),
            )
            .result,
            Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![device_usage("GPU-a", 30)],
                },
            ))
        );
    }

    fn scope(source: AttributionSource) -> AttributionScope {
        AttributionScope {
            node: SiteNodeId::for_test("node-a"),
            source,
        }
    }

    fn application_target(adapter: application_runtime_binding::Adapter) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "target-a".to_owned(),
            display_name: "target-a".to_owned(),
            site_node_id: SiteNodeId::for_test("node-a").to_string(),
            accelerator_device_ids: Vec::new(),
            platform: None,
            runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                ApplicationRuntimeBinding {
                    adapter: Some(adapter),
                },
            )),
        }
    }

    fn identity(node: &str, process_id: u32, process_start_token: u64) -> HostProcessIdentity {
        HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test(node),
            process_id,
            process_start_token,
        }
    }

    fn row(
        device_id: &str,
        process_id: u32,
        process_start_token: u64,
        ancestors: &[(u32, u64)],
        used_memory_bytes: Option<u64>,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            device_id: device_id.to_owned(),
            process_id,
            process_start_token,
            ancestors: ancestors
                .iter()
                .map(|&(process_id, start_token)| ProcessLifetime {
                    process_id,
                    start_token,
                })
                .collect(),
            used_memory_bytes,
        }
    }

    fn observed(rows: Vec<ObservedAcceleratorProcess>) -> AcceleratorProcessObservation {
        AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
            vendor: AcceleratorVendor::Nvidia,
            result: AcceleratorVendorProcessResult::Observed(rows),
        }])
    }

    fn publish(publisher: &HardwareInventoryPublisher, processes: AcceleratorProcessObservation) {
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: processes,
                },
                timestamp(11),
            ),
        );
    }

    fn device_usage(device_id: &str, used_memory_bytes: u64) -> AcceleratorMemoryDeviceUsage {
        AcceleratorMemoryDeviceUsage {
            managed_node_id: SiteNodeId::for_test("node-a").to_string(),
            device_id: device_id.to_owned(),
            used_memory_bytes,
        }
    }

    fn unavailable_result(
        code: &str,
        message: &str,
    ) -> Option<accelerator_memory_observation::Result> {
        Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        ))
    }

    fn succeeded_refresh() -> AcceleratorMemoryRefreshState {
        AcceleratorMemoryRefreshState {
            attempted_at: Some(timestamp(11)),
            outcome: Some(accelerator_memory_refresh_state::Outcome::Succeeded(
                Empty {},
            )),
        }
    }

    fn timestamp(seconds: i64) -> dotcore_proto::google::protobuf::Timestamp {
        dotcore_proto::google::protobuf::Timestamp { seconds, nanos: 0 }
    }
}
