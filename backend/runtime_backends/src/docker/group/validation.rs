//! Portable workload-group validation before Docker lifecycle mutation.

use std::collections::BTreeSet;

use dotcore_runtime::{RuntimeBackendError, WorkloadDependencyCondition, WorkloadGroupLaunch};

use super::super::{
    adapter::{DockerAcceleratorTransport, DockerRuntimeConfig},
    runtime_error,
    storage::validate_storage,
    validation::{
        valid_container_path, valid_device_id, valid_environment_name,
        validate_execution_reference, validate_name,
    },
};

pub(super) fn validate_group_launch(
    group: &WorkloadGroupLaunch,
    config: &DockerRuntimeConfig,
) -> Result<(), RuntimeBackendError> {
    if group.correlation.reconcile_generation == 0
        || group.units.is_empty()
        || group.group_id.0.is_empty()
        || group.group_id.0.len() > 40
        || !group
            .group_id
            .0
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(runtime_error(
            "Docker runtime rejected an unsafe workload group identity",
        ));
    }
    validate_name(&group.execution_target_id, "execution target ID")?;
    if group.execution_target_id != config.execution_target_id {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker application workload group does not belong to this exact execution target",
        ));
    }
    // Application targets select their devices only through CDI.
    if group
        .execution_target
        .accelerator_device_ids
        .iter()
        .any(|id| !valid_device_id(id, DockerAcceleratorTransport::Cdi))
    {
        return Err(runtime_error(
            "Docker runtime rejected an unsafe accelerator device",
        ));
    }
    if !config.application_data_root.is_absolute()
        || config.application_data_root == std::path::Path::new("/")
        || config.application_data_root.canonicalize().is_err()
    {
        return Err(runtime_error(
            "Docker runtime requires an available non-root application site-data directory",
        ));
    }
    if !group.units.contains_key(&group.publication.unit_name) {
        return Err(runtime_error(
            "Docker runtime group publication references an unknown unit",
        ));
    }
    for (name, unit) in &group.units {
        validate_name(&name.0, "group unit name")?;
        validate_execution_reference(&unit.image)?;
        if unit
            .command
            .iter()
            .chain(&unit.arguments)
            .any(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(runtime_error(
                "Docker runtime group requires safe nonempty exec-form commands",
            ));
        }
        if unit.environment.iter().any(|(name, value)| {
            !valid_environment_name(name)
                || value.contains('=')
                || value.chars().any(char::is_control)
        }) {
            return Err(runtime_error(
                "Docker runtime group requires safe explicit environment values",
            ));
        }
        if unit
            .reconfigurable_environment
            .iter()
            .any(|name| !unit.environment.contains_key(name))
        {
            return Err(runtime_error(
                "Docker runtime group has an unknown reconfigurable environment variable",
            ));
        }
        for storage in &unit.storage {
            validate_storage(storage)?;
            if !valid_container_path(&storage.destination) {
                return Err(runtime_error(
                    "Docker runtime rejected an unsafe storage attachment",
                ));
            }
        }
        let names = unit
            .ports
            .iter()
            .map(|port| &port.name)
            .collect::<BTreeSet<_>>();
        if names.len() != unit.ports.len()
            || unit.ports.iter().any(|port| {
                port.container_port == 0 || validate_name(&port.name, "group port name").is_err()
            })
        {
            return Err(runtime_error("Docker runtime rejected unsafe group ports"));
        }
        if name == &group.publication.unit_name && !names.contains(&group.publication.port_name) {
            return Err(runtime_error(
                "Docker runtime group publication references an unknown port",
            ));
        }
        for dependency in &unit.dependencies {
            if !group.units.contains_key(&dependency.unit_name) {
                return Err(runtime_error(
                    "Docker runtime group dependency references an unknown unit",
                ));
            }
        }
        for dependency in &unit.dependencies {
            if dependency.condition == WorkloadDependencyCondition::Ready
                && group.units[&dependency.unit_name]
                    .ready_launch_gate
                    .is_none()
            {
                return Err(runtime_error(
                    "Docker runtime READY dependency requires a launch gate",
                ));
            }
        }
    }
    Ok(())
}
