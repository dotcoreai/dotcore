//! What application deployments hold of each site node's memory, as capacity workloads.
//!
//! Applications have no memory estimate and no claim: each counts only the accelerator memory its
//! hosting node attributes to its service units, summed per device. An application whose target
//! is gone or whose memory architecture isn't known counts as other use. Without any target
//! observations, devices don't resolve: an application may then be on any dedicated device of its
//! node, so it holds what is attributed there and is unsized where nothing is.

use std::collections::BTreeMap;

use dotcore_domain::MemoryArchitecture;
use dotcore_proto::dotcore::{
    application::v1::ApplicationDeployment, deployment::v1::accelerator_memory_observation,
};
use dotcore_site::node::target_inventory::{CapabilityObservation, TargetCapabilityInventory};

use crate::hardware::capacity::{Estimate, NodeWorkloads, Workload, WorkloadKey, WorkloadPhase};

/// Returns the application deployments of each node as workloads holding its memory, in
/// deployment order, each with its target's devices and memory architecture from `inventory`;
/// without one, each reads as dedicated with devices that don't resolve.
/// An application holds only the memory attributed to its units, summed per device; it is
/// expected to hold memory unless every unit's attribution is not applicable, as for an
/// application without accelerator devices.
pub(crate) fn node_workloads(
    deployments: &[ApplicationDeployment],
    inventory: Option<&TargetCapabilityInventory>,
) -> NodeWorkloads {
    let mut workloads = NodeWorkloads::new();
    for deployment in deployments {
        let Some(placement) = deployment.placement.as_ref() else {
            continue;
        };
        let (memory_architecture, device_ids) = match inventory {
            Some(inventory) => {
                let Some(capabilities) = inventory.for_target(&placement.execution_target_id)
                else {
                    continue;
                };
                let memory_architecture = match capabilities.accelerator_memory {
                    CapabilityObservation::Available(architecture)
                    | CapabilityObservation::Stale(architecture) => architecture,
                    CapabilityObservation::Unavailable => continue,
                };
                (
                    memory_architecture,
                    capabilities
                        .accelerator_device_ids
                        .clone()
                        .unwrap_or_default(),
                )
            }
            // Without target observations its devices don't resolve, so it may be on any
            // dedicated device of its node.
            None => (MemoryArchitecture::Dedicated, Vec::new()),
        };
        let results = deployment
            .service_accelerator_memory
            .iter()
            .map(|unit| {
                unit.accelerator_memory
                    .as_ref()
                    .and_then(|memory| memory.result.as_ref())
            })
            .collect::<Vec<_>>();
        let mut attributed = None::<BTreeMap<String, u64>>;
        for result in &results {
            let Some(accelerator_memory_observation::Result::Available(usage)) = result else {
                continue;
            };
            let devices = attributed.get_or_insert_default();
            for device in usage
                .devices
                .iter()
                .filter(|device| device.managed_node_id == placement.site_node_id)
            {
                let bytes = devices.entry(device.device_id.clone()).or_default();
                *bytes = bytes.saturating_add(device.used_memory_bytes);
            }
        }
        // A unit without a reported result is still expected to be measured.
        let expects_use = results.iter().any(|result| {
            !matches!(
                result,
                Some(accelerator_memory_observation::Result::NotApplicable(_))
            )
        });
        workloads
            .entry(placement.site_node_id.clone())
            .or_default()
            .push(Workload {
                key: WorkloadKey::applications(deployment.application_deployment_id.clone()),
                phase: WorkloadPhase::Other,
                expects_use,
                memory_architecture,
                device_ids,
                estimate: Estimate::default(),
                attributed: attributed.filter(|attributed| !attributed.is_empty()),
            });
    }
    workloads
}
