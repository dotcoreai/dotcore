//! Exact ownership, execution, network, and endpoint verification for Docker groups.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU16,
};

use dotcore_runtime::{
    AssignedWorkloadEndpoint, RuntimeBackendError, WorkloadGroupLaunch, WorkloadGroupStaleReason,
};

use super::{
    super::{
        adapter::DockerRuntimeConfig,
        client::{DockerNetwork, GroupContainerInspection, GroupContainerOwnership},
        request::DockerGroupContainerRequest,
        runtime_error,
    },
    group_labels,
    types::{KnownGroup, KnownGroupUnit},
};

pub(super) fn strict_endpoint(
    inspection: &GroupContainerInspection,
    unit: &KnownGroupUnit,
    requested: &WorkloadGroupLaunch,
) -> Result<AssignedWorkloadEndpoint, RuntimeBackendError> {
    let port = unit
        .request
        .ports
        .iter()
        .find(|port| port.name == requested.publication.port_name)
        .ok_or_else(|| runtime_error("Docker group publication port is missing"))?;
    let expected_key = format!("{}/tcp", port.container_port);
    if inspection.host_bindings.iter().any(|(key, bindings)| {
        key != &expected_key
            && bindings
                .as_ref()
                .is_some_and(|bindings| !bindings.is_empty())
    }) {
        return Err(runtime_error(
            "Docker group publication has unexpected host bindings",
        ));
    }
    let bindings = inspection
        .host_bindings
        .get(&expected_key)
        .and_then(Option::as_ref)
        .ok_or_else(|| runtime_error("Docker group publication has no host binding"))?;
    if bindings.len() != 1 {
        return Err(runtime_error(
            "Docker group publication has malformed or multiple host bindings",
        ));
    }
    let binding = &bindings[0];
    let host: std::net::Ipv4Addr = binding
        .host
        .as_deref()
        .and_then(|host| host.parse().ok())
        .ok_or_else(|| runtime_error("Docker group publication has malformed host binding"))?;
    if host.is_unspecified() {
        return Err(runtime_error(
            "Docker group publication has wildcard host binding",
        ));
    }
    let assigned_port = binding
        .port
        .as_deref()
        .and_then(|port| port.parse().ok())
        .and_then(NonZeroU16::new)
        .ok_or_else(|| runtime_error("Docker group publication has malformed host binding"))?;
    Ok(AssignedWorkloadEndpoint {
        unit_name: requested.publication.unit_name.clone(),
        port_name: requested.publication.port_name.clone(),
        configured_host: host,
        assigned_port,
    })
}

/// Names why an owned group's observed publication no longer matches the requested one, for
/// example a host port other than the one its owner chose.
pub(super) fn publication_stale_reason(
    endpoint: &AssignedWorkloadEndpoint,
    requested: &WorkloadGroupLaunch,
) -> Option<WorkloadGroupStaleReason> {
    if endpoint.configured_host != requested.publication.configured_host {
        Some(WorkloadGroupStaleReason::PublicationHostChanged)
    } else if endpoint.assigned_port != requested.publication.host_port {
        Some(WorkloadGroupStaleReason::PublicationPortChanged)
    } else {
        None
    }
}

pub(super) fn has_host_bindings(inspection: &GroupContainerInspection) -> bool {
    inspection.host_bindings.values().any(|bindings| {
        bindings
            .as_ref()
            .is_some_and(|bindings| !bindings.is_empty())
    })
}

pub(super) fn validate_unit_network(
    inspection: &GroupContainerInspection,
    unit: &KnownGroupUnit,
    network_id: &str,
) -> Result<(), RuntimeBackendError> {
    validate_reset_unit_network(
        inspection,
        &unit.request.network_name,
        &unit.request.aliases[0],
        network_id,
    )
}

pub fn validate_reset_unit_network(
    inspection: &GroupContainerInspection,
    network_name: &str,
    unit_alias: &str,
    network_id: &str,
) -> Result<(), RuntimeBackendError> {
    if inspection.networks.len() != 1 {
        return Err(runtime_error(
            "Docker group unit has unexpected network attachments",
        ));
    }
    let attachment = inspection
        .networks
        .get(network_name)
        .ok_or_else(|| runtime_error("Docker group unit is missing its owned private network"))?;
    if attachment.network_id.as_deref() != Some(network_id)
        || !attachment
            .aliases
            .as_ref()
            .is_some_and(|aliases| aliases.iter().any(|alias| alias == unit_alias))
    {
        return Err(runtime_error(
            "Docker group unit has malformed private network aliases",
        ));
    }
    Ok(())
}

pub(super) fn exact_quiescent_network(
    network: &DockerNetwork,
    requested: &super::super::request::DockerGroupNetworkRequest,
) -> bool {
    group_network_has_expected_ownership(network, requested)
        && network.attached_container_ids.is_empty()
}

pub fn group_network_has_expected_ownership(
    network: &DockerNetwork,
    requested: &super::super::request::DockerGroupNetworkRequest,
) -> bool {
    !network.id.is_empty()
        && network.name == requested.name
        && network.labels == requested.labels
        && !network.internal
        && !network.attachable
        && network.driver == "bridge"
}

pub(super) fn exact_known_network(
    network: &DockerNetwork,
    known: &KnownGroup,
    expected_attachments: &BTreeSet<String>,
    config: &DockerRuntimeConfig,
) -> bool {
    known_network_has_expected_identity(network, known, config)
        && &network.attached_container_ids == expected_attachments
}

pub(super) fn known_network_has_expected_identity(
    network: &DockerNetwork,
    known: &KnownGroup,
    config: &DockerRuntimeConfig,
) -> bool {
    network.id == known.network_id
        && network.name == known.network_name
        && network.labels == group_labels(config, &known.launch, None)
        && !network.internal
        && !network.attachable
        && network.driver == "bridge"
}

pub(super) fn group_container_has_expected_ownership(
    ownership: &GroupContainerOwnership,
    unit: &KnownGroupUnit,
) -> bool {
    ownership.id == unit.container_id
        && group_container_request_has_expected_ownership(ownership, &unit.request)
}

pub(super) fn group_container_request_has_expected_ownership(
    ownership: &GroupContainerOwnership,
    request: &DockerGroupContainerRequest,
) -> bool {
    !ownership.id.is_empty()
        && ownership.name == request.name
        && ownership.labels == expected_group_container_labels(&ownership.image_labels, request)
}

pub(super) fn expected_group_container_labels(
    image_labels: &BTreeMap<String, String>,
    request: &DockerGroupContainerRequest,
) -> BTreeMap<String, String> {
    expected_group_labels(image_labels, &request.labels)
}

pub fn expected_group_labels(
    image_labels: &BTreeMap<String, String>,
    runtime_labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut expected = image_labels.clone();
    expected.extend(runtime_labels.clone());
    expected
}

use super::*;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn validate_network_ownership(
        &self,
        known: &KnownGroup,
    ) -> Result<(), RuntimeBackendError> {
        let network = self
            .client
            .inspect_owned_network(&known.network_id)
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker group network ownership inspection failed",
                    error,
                )
            })?;
        let expected_attachments = known
            .units
            .values()
            .map(|unit| unit.container_id.clone())
            .collect();
        if !known_network_has_expected_identity(&network, known, &self.config) {
            return Err(runtime_error(
                "Docker workload group network ownership changed after creation",
            ));
        }
        if network.attached_container_ids == expected_attachments {
            return Ok(());
        }
        // Docker omits stopped containers from network attachments, so a partially or fully
        // stopped group attaches a subset of its units; each unit's own network is verified below.
        if !network
            .attached_container_ids
            .is_subset(&expected_attachments)
            || expected_attachments.is_empty()
        {
            return Err(runtime_error(
                "Docker workload group network ownership changed after creation",
            ));
        }
        for unit in known.units.values() {
            let inspection = self
                .client
                .inspect_group_container(&unit.container_id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker group unit network ownership inspection failed",
                        error,
                    )
                })?;
            validate_unit_network(&inspection, unit, &known.network_id)?;
        }
        Ok(())
    }

    pub(super) async fn validate_unit_execution(
        &self,
        unit: &KnownGroupUnit,
    ) -> Result<(), RuntimeBackendError> {
        let execution = self
            .client
            .inspect_group_execution(&unit.container_id)
            .await
            .map_err(|error| {
                super::runtime_error_with_source("Docker group execution inspection failed", error)
            })?;
        if execution.image != unit.request.image
            || (!unit.request.entrypoint.is_empty()
                && execution.entrypoint != unit.request.entrypoint)
            || (!unit.request.argv.is_empty() && execution.argv != unit.request.argv)
            || !unit
                .request
                .environment
                .iter()
                .all(|value| execution.environment.contains(value))
            || execution.mounts != unit.request.mounts
        {
            return Err(runtime_error(
                "Docker workload group unit execution changed after creation",
            ));
        }
        Ok(())
    }
}
