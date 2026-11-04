//! Resolves a desired local model deployment into the runtime workload launch used
//! to start its engine.
//!
//! Every controller path resolves the same exact site execution target retained in
//! the deployment placement, so launch cannot silently move between targets. The launch
//! carries the memory the deployment's resolved estimate expects, for runtimes that simulate it.

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_domain::SiteNodeId;
use dotcore_proto::dotcore::{
    model::v1::{ModelDeploymentDeclaration, ResolvedSizing},
    site::v1::ExecutionTarget as SiteExecutionTarget,
};
use dotcore_runtime::{ExecutionTarget, WorkloadExpectedMemory, WorkloadLaunch};

use super::{
    ModelLifecycleError,
    state::{catalog_configuration, configuration, configuration_with_source},
};
use crate::model::{engine::EngineIntegrationRegistry, resolved::ResolvedModelLaunch};

pub(super) fn workload_launch(
    desired: &ModelDeploymentDeclaration,
    site_target: &SiteExecutionTarget,
    artifacts: &PreparedModelArtifacts,
) -> Result<WorkloadLaunch, ModelLifecycleError> {
    let resolved = resolved_launch(desired)?;
    let target = execution_target(site_target)?;
    let mut spec = EngineIntegrationRegistry::production()
        .materialize_workload(&resolved, &target, artifacts, &desired.model_deployment_id)
        .map_err(|error| ModelLifecycleError::EngineIntegration(error.into()))?;
    spec.expected_memory = expected_memory(
        resolved.snapshot.sizing.as_ref(),
        &target.accelerator_device_ids,
    );
    Ok(WorkloadLaunch::new(
        desired.model_deployment_id.clone(),
        target,
        spec,
    ))
}

pub(super) fn resolved_launch(
    desired: &ModelDeploymentDeclaration,
) -> Result<ResolvedModelLaunch, ModelLifecycleError> {
    ResolvedModelLaunch::from_declaration(desired).map_err(catalog_configuration)
}

pub(super) fn selected_target<'a>(
    site: &'a dotcore_proto::dotcore::site::v1::Site,
    desired: &ModelDeploymentDeclaration,
) -> Result<&'a SiteExecutionTarget, ModelLifecycleError> {
    let placement = desired
        .placement
        .as_ref()
        .ok_or_else(|| configuration("local model deployment has no exact placement"))?;
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == placement.execution_target_id)
        .ok_or_else(|| {
            configuration(format!(
                "execution target {:?} is missing",
                placement.execution_target_id
            ))
        })?;
    if target.site_node_id != placement.site_node_id {
        return Err(configuration(format!(
            "node {:?} does not own execution target {:?}",
            placement.site_node_id, placement.execution_target_id
        )));
    }
    Ok(target)
}

pub(super) fn execution_target(
    site_target: &SiteExecutionTarget,
) -> Result<ExecutionTarget, ModelLifecycleError> {
    Ok(ExecutionTarget {
        managed_node_id: site_target
            .site_node_id
            .parse::<SiteNodeId>()
            .map_err(|error| {
                configuration_with_source(
                    format!(
                        "execution target {:?} has an invalid node identity",
                        site_target.execution_target_id
                    ),
                    error,
                )
            })?,
        accelerator_device_ids: site_target.accelerator_device_ids.clone(),
    })
}

/// Returns the memory a resolved estimate expects a workload on `devices` to use: the
/// accelerator estimate split evenly across the devices, the first one taking any remainder, and
/// the host estimate. Without an estimate, or without a device for its accelerator part, that part
/// expects nothing.
fn expected_memory(sizing: Option<&ResolvedSizing>, devices: &[String]) -> WorkloadExpectedMemory {
    let Some(sizing) = sizing else {
        return WorkloadExpectedMemory::default();
    };
    let accelerator_bytes = sizing.accelerator_memory_bytes.unwrap_or_default();
    let device_count = devices.len() as u64;
    let device_bytes = if accelerator_bytes == 0 || device_count == 0 {
        Default::default()
    } else {
        let share = accelerator_bytes / device_count;
        let remainder = accelerator_bytes % device_count;
        devices
            .iter()
            .enumerate()
            .map(|(index, device)| {
                let bytes = if index == 0 { share + remainder } else { share };
                (device.clone(), bytes)
            })
            .collect()
    };
    WorkloadExpectedMemory {
        device_bytes,
        host_bytes: sizing.host_memory_bytes.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn the_resolved_estimate_is_split_across_the_target_devices_beside_host_memory() {
        let sizing = ResolvedSizing {
            accelerator_memory_bytes: Some(10),
            derived_engine_parameter_ids: Vec::new(),
            host_memory_bytes: Some(7),
        };
        let devices = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();

        assert_eq!(
            expected_memory(Some(&sizing), &devices(&["gpu-a"])),
            WorkloadExpectedMemory {
                device_bytes: BTreeMap::from([("gpu-a".to_owned(), 10)]),
                host_bytes: 7,
            }
        );
        assert_eq!(
            expected_memory(Some(&sizing), &devices(&["gpu-a", "gpu-b", "gpu-c"])),
            WorkloadExpectedMemory {
                device_bytes: BTreeMap::from([
                    ("gpu-a".to_owned(), 4),
                    ("gpu-b".to_owned(), 3),
                    ("gpu-c".to_owned(), 3),
                ]),
                host_bytes: 7,
            }
        );
        assert_eq!(
            expected_memory(Some(&sizing), &[]),
            WorkloadExpectedMemory {
                device_bytes: BTreeMap::new(),
                host_bytes: 7,
            }
        );
        assert_eq!(
            expected_memory(None, &devices(&["gpu-a"])),
            WorkloadExpectedMemory::default()
        );
    }
}
