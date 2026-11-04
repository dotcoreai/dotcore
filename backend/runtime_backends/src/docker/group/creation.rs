//! Dependency ordering for Docker workload-group creation.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_runtime::{
    ReadyLaunchGateOutcome, RuntimeBackendError, RuntimeFuture, WorkloadDependencyCondition,
    WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadUnitName,
};
use tracing::warn;

use super::{
    super::{
        adapter::DockerAdapter,
        client::{ClientError, DockerClient},
        runtime_error,
        storage::materialize_storage,
    },
    errors::{DockerGroupCreationError, OwnedWorkloadGroupRealization},
    request::{PreparedGroup, prepare_group},
    types::{KnownGroup, KnownGroupUnit},
    validation::validate_group_launch,
    verification::exact_quiescent_network,
};

pub(super) fn topological_order(
    group: &WorkloadGroupLaunch,
) -> Result<Vec<WorkloadUnitName>, RuntimeBackendError> {
    let mut remaining = group.units.keys().cloned().collect::<BTreeSet<_>>();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .find(|name| {
                group.units[*name]
                    .dependencies
                    .iter()
                    .all(|dependency| ordered.contains(&dependency.unit_name))
            })
            .cloned();
        match next {
            Some(name) => {
                remaining.remove(&name);
                ordered.push(name);
            }
            None => {
                return Err(runtime_error(
                    "Docker runtime group dependencies contain a cycle",
                ));
            }
        }
    }
    Ok(ordered)
}

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn validate_launch_constraints(
        &self,
        group: &WorkloadGroupLaunch,
        prepared: &PreparedGroup,
    ) -> Result<(), RuntimeBackendError> {
        let observed_destination = self.client.execution_platform().await.map_err(|error| {
            super::runtime_error_with_source("Docker destination platform inspection failed", error)
        })?;
        if observed_destination != group.required_platform {
            return Err(RuntimeBackendError::TargetPlatformMismatch {
                required_operating_system: group.required_platform.operating_system().to_owned(),
                required_architecture: group.required_platform.architecture().to_owned(),
                observed_operating_system: observed_destination.operating_system().to_owned(),
                observed_architecture: observed_destination.architecture().to_owned(),
            });
        }

        let mut first_failure = None;
        for request in prepared.units.values() {
            let available = self
                .client
                .image_exists(&request.image)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker local image availability inspection failed",
                        error,
                    )
                })?;
            if !available {
                first_failure.get_or_insert_with(|| {
                    RuntimeBackendError::local_image_missing(format!(
                        "Docker local image is unavailable for group unit {}: {}",
                        request.name, request.image
                    ))
                });
                continue;
            }
            let observed_image = self
                .client
                .inspect_image_platform(&request.image)
                .await
                .map_err(|error| {
                    if error.is_missing_image() {
                        RuntimeBackendError::image_missing_with_source(
                            "Docker group image platform inspection failed because the retained image is unavailable",
                            error,
                        )
                    } else {
                        super::runtime_error_with_source(
                            "Docker group image platform inspection failed",
                            error,
                        )
                    }
                })?;
            if observed_image != group.required_platform {
                first_failure.get_or_insert_with(|| RuntimeBackendError::ImagePlatformMismatch {
                    image: request.image.clone(),
                    required: group.required_platform.clone(),
                    observed: observed_image,
                });
            }
        }
        first_failure.map_or(Ok(()), Err)
    }

    pub(super) async fn validate_expected_container_names_absent(
        &self,
        prepared: &PreparedGroup,
    ) -> Result<(), RuntimeBackendError> {
        for request in prepared.units.values() {
            if self
                .client
                .container_exists(&request.name)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker group unit existence inspection failed",
                        error,
                    )
                })?
            {
                return Err(runtime_error(
                    "Docker workload group has an expected canonical container",
                ));
            }
        }
        Ok(())
    }
}

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) fn create_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            let mut prepared = prepare_group(group, &self.config)?;
            let existing = {
                self.known_groups()
                    .lock()
                    .expect("Docker group registry")
                    .get(&group.group_id.0)
                    .cloned()
            }
            .or(self.known_group_from_launch(group).await?);
            if let Some(mut known) = existing {
                if self.exact_network_only_residue(&known, group).await? {
                    self.validate_launch_constraints(group, &prepared).await?;
                    self.known_groups()
                        .lock()
                        .expect("Docker group registry")
                        .remove(&group.group_id.0);
                } else if known.units.len() != group.units.len() {
                    self.validate_launch_constraints(group, &prepared).await?;
                    self.cleanup_partial_group(&mut known).await?;
                    self.known_groups()
                        .lock()
                        .expect("Docker group registry")
                        .remove(&group.group_id.0);
                } else {
                    return self.inspect_known_group(&known, group).await;
                }
            } else {
                self.validate_launch_constraints(group, &prepared).await?;
            }
            self.validate_expected_container_names_absent(&prepared)
                .await?;
            let extra_hosts = self.host_alias_entries(group).await?;
            for request in prepared.units.values_mut() {
                request.extra_hosts.clone_from(&extra_hosts);
            }
            let networks = self.exact_group_networks(&prepared.network_name).await?;
            let network_id = match networks.as_slice() {
                [] => match self.client.create_group_network(&prepared.network).await {
                    Ok(network_id) => network_id,
                    Err(error) => {
                        let refreshed = self.known_group_from_launch(group).await?;
                        match refreshed {
                            Some(known) if known.units.is_empty() => known.network_id,
                            _ => {
                                return Err(super::runtime_error_with_source(
                                    "Docker group network creation failed",
                                    error,
                                ));
                            }
                        }
                    }
                },
                [network] if exact_quiescent_network(network, &prepared.network) => {
                    network.id.clone()
                }
                _ => {
                    return Err(runtime_error(
                        "Docker workload-group network is not one empty expected owned network",
                    ));
                }
            };
            let mut known = KnownGroup {
                launch: group.clone(),
                network_id,
                network_name: prepared.network_name,
                units: BTreeMap::new(),
            };
            self.known_groups()
                .lock()
                .expect("Docker group registry")
                .insert(group.group_id.0.clone(), known.clone());
            let result = async {
                materialize_storage(
                    &self.config,
                    group
                        .units
                        .values()
                        .flat_map(|unit| &unit.storage)
                        .map(Into::into),
                )?;
                for name in topological_order(group)? {
                    let dependencies_satisfied =
                        group.units[&name].dependencies.iter().all(|dependency| {
                            let Some(unit) = known.units.get(&dependency.unit_name) else {
                                return false;
                            };
                            dependency.condition == WorkloadDependencyCondition::Started
                                || unit.gate == Some(ReadyLaunchGateOutcome::Passed)
                        });
                    if !dependencies_satisfied {
                        continue;
                    }
                    let request = prepared
                        .units
                        .get(&name)
                        .expect("validated group unit")
                        .clone();
                    let container_id = match self.client.create_group_container(&request).await {
                        Ok(container_id) => container_id,
                        Err(error) => {
                            if let Some(refreshed) = self.known_group_from_launch(group).await? {
                                known = refreshed;
                                self.known_groups()
                                    .lock()
                                    .expect("Docker group registry")
                                    .insert(group.group_id.0.clone(), known.clone());
                            }
                            let message =
                                format!("Docker group unit creation failed for {}", name.0);
                            return Err(if error.is_missing_image() {
                                RuntimeBackendError::image_missing_with_source(message, error)
                            } else {
                                super::runtime_error_with_source(message, error)
                            });
                        }
                    };
                    known.units.insert(
                        name.clone(),
                        KnownGroupUnit {
                            container_id: container_id.clone(),
                            request: request.clone(),
                            gate: None,
                        },
                    );
                    self.known_groups()
                        .lock()
                        .expect("Docker group registry")
                        .insert(group.group_id.0.clone(), known.clone());
                    self.client
                        .start_container(&container_id)
                        .await
                        .map_err(|error| group_unit_start_error(&name, error))?;
                    let gate = match group.units[&name].ready_launch_gate.as_ref() {
                        Some(gate) => Some(self.wait_ready(&container_id, &request, gate).await?),
                        None => None,
                    };
                    known
                        .units
                        .get_mut(&name)
                        .expect("retained group unit")
                        .gate = gate;
                    self.known_groups()
                        .lock()
                        .expect("Docker group registry")
                        .insert(group.group_id.0.clone(), known.clone());
                }
                self.inspect_known_group(&known, group).await
            }
            .await;
            match result {
                Ok(observation) => Ok(observation),
                Err(error) => match self.cleanup_partial_group(&mut known).await {
                    Ok(()) => {
                        self.known_groups()
                            .lock()
                            .expect("Docker group registry")
                            .remove(&group.group_id.0);
                        Err(error)
                    }
                    Err(cleanup_error) => {
                        match self.network_is_absent_without_replacement(&known).await {
                            Ok(true) => {
                                self.known_groups()
                                    .lock()
                                    .expect("Docker group registry")
                                    .remove(&group.group_id.0);
                                Err(DockerGroupCreationError::CreationAndCleanup {
                                    creation_error: error,
                                    cleanup_error,
                                    owned_realization: None,
                                }
                                .into())
                            }
                            Ok(false) => {
                                let owned_realization = owned_realization(&known);
                                self.known_groups()
                                    .lock()
                                    .expect("Docker group registry")
                                    .insert(group.group_id.0.clone(), known);
                                Err(DockerGroupCreationError::CreationAndCleanup {
                                    creation_error: error,
                                    cleanup_error,
                                    owned_realization: Some(owned_realization),
                                }
                                .into())
                            }
                            Err(absence_inspection_error) => {
                                let owned_realization = owned_realization(&known);
                                self.known_groups()
                                    .lock()
                                    .expect("Docker group registry")
                                    .insert(group.group_id.0.clone(), known);
                                Err(DockerGroupCreationError::CreationCleanupAndInspection {
                                    creation_error: error,
                                    cleanup_error,
                                    absence_inspection_error,
                                    owned_realization,
                                }
                                .into())
                            }
                        }
                    }
                },
            }
        })
    }

    /// Resolves the group's site host aliases on this node into `name:address` host entries.
    /// A name resolving to loopback gets no entry, since inside a container it would reach the
    /// container itself; a failed lookup fails the creation, which the owner retries.
    async fn host_alias_entries(
        &self,
        group: &WorkloadGroupLaunch,
    ) -> Result<Vec<String>, RuntimeBackendError> {
        let node_id = &group.execution_target.managed_node_id;
        let mut entries = Vec::new();
        for alias in &group.host_aliases {
            let (address, loopback_warning) = alias
                .resolve_with_loopback_warning(self.host_resolver.as_ref())
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        format!(
                            "Docker group host alias {alias} cannot be resolved on node {node_id}"
                        ),
                        error,
                    )
                })?;
            if let Some(warning) = loopback_warning {
                warn!(event_kind = "docker_group_host_alias_loopback", deployment_id = %group.correlation.owner_id, group_id = %group.group_id.0, node_id = %node_id, alias = %alias, "{warning}; the deployment's containers get no host entry for it");
                continue;
            }
            entries.push(format!("{alias}:{address}"));
        }
        Ok(entries)
    }
}

/// Classifies a failed group unit start, keeping a taken published host port typed so its owner
/// can report it.
pub(super) fn group_unit_start_error(
    name: &WorkloadUnitName,
    error: ClientError,
) -> RuntimeBackendError {
    let message = format!("Docker group unit start failed for {}", name.0);
    if error.is_port_in_use() {
        RuntimeBackendError::published_port_in_use_with_source(message, error)
    } else {
        super::runtime_error_with_source(message, error)
    }
}

fn owned_realization(known: &KnownGroup) -> OwnedWorkloadGroupRealization {
    OwnedWorkloadGroupRealization {
        group_id: known.launch.group_id.clone(),
        network_id: known.network_id.clone(),
        units: known
            .units
            .iter()
            .map(|(name, unit)| (name.clone(), unit.container_id.clone()))
            .collect(),
    }
}
