//! Derives non-blocking advisories for a resolved local model deployment, such as host KV cache
//! offload configured on a target whose accelerators already share host memory, or a unified
//! memory pool smaller than the profile's per-device accelerator memory minimum.
//!
//! Advisories never change the snapshot or block creation. Engine-specific meaning comes from the
//! engine adapters, so the console can show warnings without interpreting engine parameters.

use dotcore_domain::MemoryArchitecture;
use dotcore_proto::dotcore::model::v1::{
    LocalModelDeploymentAdvisory, ResolvedLocalModelDeploymentSnapshot,
};

use crate::{
    hardware::capacity::below_device_minimum,
    model::{
        engine::{EngineIntegrationError, EngineIntegrationRegistry},
        resolved::selected_target_memory_architecture,
    },
};

/// Returns every advisory for one resolved local snapshot in stable enum order.
pub(crate) fn local_deployment_advisories(
    registry: &EngineIntegrationRegistry,
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<Vec<LocalModelDeploymentAdvisory>, LocalDeploymentAdvisoryError> {
    let memory_architecture = selected_target_memory_architecture(snapshot)
        .map_err(|source| LocalDeploymentAdvisoryError::Snapshot(anyhow::Error::new(source)))?;
    let mut advisories = Vec::new();
    if memory_architecture == MemoryArchitecture::Unified
        && registry.enables_host_kv_offload(snapshot)?
    {
        advisories.push(LocalModelDeploymentAdvisory::HostKvOffloadSharesMemory);
    }
    if memory_architecture == MemoryArchitecture::Unified
        && unified_pool_below_device_minimum(snapshot)
    {
        advisories.push(LocalModelDeploymentAdvisory::UnifiedMemoryPoolBelowDeviceMinimum);
    }
    Ok(advisories)
}

/// Returns whether the retained unified memory-pool total is below the resolved hardware class's
/// per-device accelerator memory minimum, by the capacity component's rule. Unified devices have
/// no dedicated memory, so the node pool stands in for the device; an unknown total never raises
/// the advisory.
fn unified_pool_below_device_minimum(snapshot: &ResolvedLocalModelDeploymentSnapshot) -> bool {
    below_device_minimum(
        snapshot
            .selected_target
            .as_ref()
            .and_then(|target| target.accelerator_memory_pool_total_bytes),
        snapshot
            .hardware_support
            .as_ref()
            .and_then(|support| support.hardware_requirements.as_ref()),
    )
}

/// Failure while deriving advisories from a resolved local snapshot.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalDeploymentAdvisoryError {
    #[error("resolved local snapshot is invalid for advisories: {0}")]
    Snapshot(#[source] anyhow::Error),

    #[error("engine integration could not evaluate advisories: {0}")]
    Engine(#[from] EngineIntegrationError),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::HardwareRequirements,
        model::v1::{LocalModelDeploymentAdvisory, model_deployment_snapshot::Source},
        observation::v1::AcceleratorMemoryArchitecture,
    };

    use super::local_deployment_advisories;
    use crate::model::engine::EngineIntegrationRegistry;

    #[test]
    fn host_kv_offload_advisory_requires_unified_memory_and_offload() {
        let cases = [
            (AcceleratorMemoryArchitecture::Unified, vec!["-nkvo"], true),
            (
                AcceleratorMemoryArchitecture::Unified,
                vec!["--no-kv-offload"],
                true,
            ),
            (AcceleratorMemoryArchitecture::Unified, vec![], false),
            (
                AcceleratorMemoryArchitecture::Dedicated,
                vec!["-nkvo"],
                false,
            ),
        ];
        for (memory_architecture, tokens, expected) in cases {
            let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
            let Some(Source::Local(local)) = snapshot.source.as_mut() else {
                panic!("local snapshot");
            };
            local
                .selected_target
                .as_mut()
                .expect("selected target")
                .accelerator_memory_architecture = memory_architecture as i32;
            local.custom_argument_tokens = tokens.iter().map(|token| (*token).to_owned()).collect();

            assert_eq!(
                local_deployment_advisories(&EngineIntegrationRegistry::production(), local)
                    .expect("advisories"),
                if expected {
                    vec![LocalModelDeploymentAdvisory::HostKvOffloadSharesMemory]
                } else {
                    Vec::new()
                },
                "{memory_architecture:?} with {tokens:?}"
            );
        }
    }

    #[test]
    fn unified_pool_below_a_per_device_minimum_is_advisory_only_on_unified_targets() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let cases = [
            (AcceleratorMemoryArchitecture::Unified, Some(16 * GIB), true),
            (
                AcceleratorMemoryArchitecture::Unified,
                Some(32 * GIB),
                false,
            ),
            (AcceleratorMemoryArchitecture::Unified, None, false),
            (
                AcceleratorMemoryArchitecture::Dedicated,
                Some(16 * GIB),
                false,
            ),
        ];
        for (memory_architecture, pool_total, expected) in cases {
            let mut snapshot = crate::test_support::local_model_snapshot("fixture-model");
            let Some(Source::Local(local)) = snapshot.source.as_mut() else {
                panic!("local snapshot");
            };
            let target = local.selected_target.as_mut().expect("selected target");
            target.accelerator_memory_architecture = memory_architecture as i32;
            target.accelerator_memory_pool_total_bytes = pool_total;
            local
                .hardware_support
                .as_mut()
                .expect("hardware support")
                .hardware_requirements = Some(HardwareRequirements {
                minimum_memory_bytes_per_device: 24 * GIB,
                minimum_system_memory_bytes: 0,
            });

            assert_eq!(
                local_deployment_advisories(&EngineIntegrationRegistry::production(), local)
                    .expect("advisories"),
                if expected {
                    vec![LocalModelDeploymentAdvisory::UnifiedMemoryPoolBelowDeviceMinimum]
                } else {
                    Vec::new()
                },
                "{memory_architecture:?} with pool {pool_total:?}"
            );
        }
    }
}
