//! Non-mutating owned-group observation construction for Docker inspection.

use dotcore_domain::{SiteNodeId, host_process::HostProcessIdentity};
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackendId, WorkloadGroupLaunch, WorkloadGroupObservation,
    WorkloadUnitObservation, WorkloadUnitState,
};

pub(super) fn network_only_residue_observation(
    requested: &WorkloadGroupLaunch,
    backend_id: RuntimeBackendId,
) -> WorkloadGroupObservation {
    WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
        group_id: requested.group_id.clone(),
        correlation: requested.correlation.clone(),
        backend_id,
        units: requested
            .units
            .keys()
            .cloned()
            .map(|name| {
                (
                    name,
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Absent,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                )
            })
            .collect(),
        assigned_endpoint: None,
    })
}

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn retained_group_observation(
        &self,
        known: &KnownGroup,
        requested: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, RuntimeBackendError> {
        let mut retained_identity = known.launch.clone();
        retained_identity.publication.configured_host = requested.publication.configured_host;
        retained_identity.publication.host_port = requested.publication.host_port;
        if retained_identity != *requested {
            return Err(runtime_error(
                "Docker workload group launch identity changed before reversible operation",
            ));
        }
        if known
            .units
            .keys()
            .any(|name| !requested.units.contains_key(name))
        {
            return Err(runtime_error(
                "Docker workload group has an unexpected owned service unit",
            ));
        }
        let remaining = self.validate_group_removal_evidence(known).await?;
        let mut units = BTreeMap::new();
        let mut endpoint = None;
        for name in requested.units.keys() {
            let Some(unit) = known.units.get(name) else {
                units.insert(
                    name.clone(),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Absent,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                );
                continue;
            };
            if !remaining.contains(&unit.container_id) {
                units.insert(
                    name.clone(),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Absent,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                );
                continue;
            }
            let inspection = self
                .client
                .inspect_group_container(&unit.container_id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker reversible group unit inspection failed",
                        error,
                    )
                })?;
            validate_unit_network(&inspection, unit, &known.network_id)?;
            let state = if inspection.running {
                WorkloadUnitState::Running
            } else {
                WorkloadUnitState::Stopped
            };
            if name == &requested.publication.unit_name && state == WorkloadUnitState::Running {
                endpoint = Some(strict_endpoint(&inspection, unit, requested)?);
            } else if has_host_bindings(&inspection) {
                return Err(runtime_error(
                    "Docker reversible group unit has an unexpected host port binding",
                ));
            }
            units.insert(
                name.clone(),
                WorkloadUnitObservation {
                    state,
                    ready_launch_gate: unit.gate,
                    host_process: unit_host_process(
                        &inspection,
                        state,
                        &requested.execution_target.managed_node_id,
                    ),
                },
            );
        }
        let observed = ObservedWorkloadGroup {
            group_id: requested.group_id.clone(),
            correlation: requested.correlation.clone(),
            backend_id: dotcore_runtime::RuntimeBackend::backend_id(self),
            units,
            assigned_endpoint: endpoint,
        };
        if let Some(reason) = observed
            .assigned_endpoint
            .as_ref()
            .and_then(|endpoint| publication_stale_reason(endpoint, requested))
        {
            return Ok(WorkloadGroupObservation::StaleConfiguration {
                group: observed,
                reason,
            });
        }
        Ok(WorkloadGroupObservation::Observed(observed))
    }

    pub(super) async fn inspect_known_group(
        &self,
        known: &KnownGroup,
        requested: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, RuntimeBackendError> {
        if self.exact_network_only_residue(known, requested).await? {
            return Ok(network_only_residue_observation(
                requested,
                dotcore_runtime::RuntimeBackend::backend_id(self),
            ));
        }
        self.validate_network_ownership(known).await?;
        if known.units.len() != requested.units.len() {
            return self.partial_group_observation(known, requested).await;
        }
        self.validate_owned_unit_set(known, requested).await?;
        let mut units = BTreeMap::new();
        let mut endpoint = None;
        for name in requested.units.keys() {
            let unit = known.units.get(name).expect("validated group unit set");
            let ownership = self
                .client
                .inspect_group_container_ownership(&unit.container_id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker group ownership inspection failed",
                        error,
                    )
                })?;
            if !group_container_has_expected_ownership(&ownership, unit) {
                return Err(runtime_error(
                    "Docker workload group unit ownership changed after creation",
                ));
            }
            self.validate_unit_execution(unit).await?;
            let inspection = match self
                .client
                .inspect_group_container(&unit.container_id)
                .await
            {
                Ok(inspection) => inspection,
                Err(error) if error.is_not_found() => {
                    return Err(runtime_error(
                        "Docker workload group unit disappeared during inspection",
                    ));
                }
                Err(error) => {
                    return Err(super::runtime_error_with_source(
                        "Docker group unit inspection failed",
                        error,
                    ));
                }
            };
            validate_unit_network(&inspection, unit, &known.network_id)?;
            let state = if inspection.running {
                WorkloadUnitState::Running
            } else {
                WorkloadUnitState::Stopped
            };
            if name == &known.launch.publication.unit_name && state == WorkloadUnitState::Running {
                endpoint = Some(strict_endpoint(&inspection, unit, requested)?);
            } else if has_host_bindings(&inspection) {
                return Err(runtime_error(
                    "Docker private group unit has a host port binding",
                ));
            }
            units.insert(
                name.clone(),
                WorkloadUnitObservation {
                    state,
                    ready_launch_gate: unit.gate,
                    host_process: unit_host_process(
                        &inspection,
                        state,
                        &requested.execution_target.managed_node_id,
                    ),
                },
            );
        }
        let observed = ObservedWorkloadGroup {
            group_id: requested.group_id.clone(),
            correlation: known.launch.correlation.clone(),
            backend_id: dotcore_runtime::RuntimeBackend::backend_id(self),
            units,
            assigned_endpoint: endpoint,
        };
        if let Some(reason) = observed
            .assigned_endpoint
            .as_ref()
            .and_then(|endpoint| publication_stale_reason(endpoint, requested))
        {
            return Ok(WorkloadGroupObservation::StaleConfiguration {
                group: observed,
                reason,
            });
        }
        Ok(WorkloadGroupObservation::Observed(observed))
    }

    pub(super) async fn validate_owned_unit_set(
        &self,
        known: &KnownGroup,
        requested: &WorkloadGroupLaunch,
    ) -> Result<(), RuntimeBackendError> {
        if known.units.len() != requested.units.len()
            || !requested
                .units
                .keys()
                .all(|name| known.units.contains_key(name))
        {
            return Err(runtime_error(
                "Docker workload group has a partial owned unit set",
            ));
        }
        let containers = self
            .client
            .list_managed_containers()
            .await
            .map_err(|error| {
                super::runtime_error_with_source("Docker owned-container discovery failed", error)
            })?;
        let mut counts = BTreeMap::<WorkloadUnitName, usize>::new();
        for container in containers {
            if container.labels.get("com.dotcore.site").map(String::as_str)
                != Some(self.config.site_id.as_str())
            {
                continue;
            }
            if container.labels.get(LABEL_GROUP).map(String::as_str)
                != Some(requested.group_id.0.as_str())
            {
                continue;
            }
            if container
                .labels
                .get("com.dotcore.managed")
                .map(String::as_str)
                != Some("true")
                || container.labels.get("com.dotcore.site").map(String::as_str)
                    != Some(self.config.site_id.as_str())
                || container
                    .labels
                    .get("com.dotcore.runtime.schema")
                    .map(String::as_str)
                    != Some(GROUP_SCHEMA)
            {
                return Err(runtime_error(
                    "Docker workload group has malformed ownership labels",
                ));
            }
            let unit_name = container.labels.get(LABEL_UNIT).ok_or_else(|| {
                runtime_error("Docker workload group has malformed ownership labels")
            })?;
            let unit_name = WorkloadUnitName(unit_name.clone());
            let unit = known.units.get(&unit_name).ok_or_else(|| {
                runtime_error("Docker workload group has an unexpected owned service unit")
            })?;
            if !container.name_is_unambiguous
                || container.id.is_empty()
                || container.name != unit.request.name
            {
                return Err(runtime_error(
                    "Docker workload group has malformed owned service units",
                ));
            }
            let ownership = self
                .client
                .inspect_group_container_ownership(&container.id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker group ownership inspection failed",
                        error,
                    )
                })?;
            if !group_container_has_expected_ownership(&ownership, unit) {
                return Err(runtime_error(
                    "Docker workload group has malformed owned service units",
                ));
            }
            *counts.entry(unit_name).or_default() += 1;
        }
        if requested
            .units
            .keys()
            .any(|name| counts.get(name) != Some(&1))
        {
            return Err(runtime_error(
                "Docker workload group has a partial or duplicate owned unit set",
            ));
        }
        Ok(())
    }

    pub(super) async fn partial_group_observation(
        &self,
        known: &KnownGroup,
        requested: &WorkloadGroupLaunch,
    ) -> Result<WorkloadGroupObservation, RuntimeBackendError> {
        let mut units = BTreeMap::new();
        let mut endpoint = None;
        for name in requested.units.keys() {
            let Some(unit) = known.units.get(name) else {
                units.insert(
                    name.clone(),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Absent,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                );
                continue;
            };
            let ownership = self
                .client
                .inspect_group_container_ownership(&unit.container_id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker partial-group ownership inspection failed",
                        error,
                    )
                })?;
            if !group_container_has_expected_ownership(&ownership, unit) {
                return Err(runtime_error(
                    "Docker partial-group ownership changed before observation",
                ));
            }
            self.validate_unit_execution(unit).await?;
            let inspection = self
                .client
                .inspect_group_container(&unit.container_id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker partial-group unit inspection failed",
                        error,
                    )
                })?;
            validate_unit_network(&inspection, unit, &known.network_id)?;
            let state = if inspection.running {
                WorkloadUnitState::Running
            } else {
                WorkloadUnitState::Stopped
            };
            if name == &requested.publication.unit_name && state == WorkloadUnitState::Running {
                endpoint = Some(strict_endpoint(&inspection, unit, requested)?);
            } else if has_host_bindings(&inspection) {
                return Err(runtime_error(
                    "Docker partial private group unit has a host port binding",
                ));
            }
            units.insert(
                name.clone(),
                WorkloadUnitObservation {
                    state,
                    ready_launch_gate: unit.gate,
                    host_process: unit_host_process(
                        &inspection,
                        state,
                        &requested.execution_target.managed_node_id,
                    ),
                },
            );
        }
        Ok(WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
            group_id: requested.group_id.clone(),
            correlation: requested.correlation.clone(),
            backend_id: dotcore_runtime::RuntimeBackend::backend_id(self),
            units,
            assigned_endpoint: endpoint,
        }))
    }
}

use super::*;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) fn inspect_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            let known = {
                self.known_groups()
                    .lock()
                    .expect("Docker group registry")
                    .get(&group.group_id.0)
                    .cloned()
            };
            match known {
                Some(known) => self.inspect_known_group(&known, group).await,
                None => match self.known_group_from_launch(group).await? {
                    Some(known) => {
                        let observation = self.inspect_known_group(&known, group).await?;
                        self.known_groups()
                            .lock()
                            .expect("Docker group registry")
                            .insert(group.group_id.0.clone(), known);
                        Ok(observation)
                    }
                    None => Ok(WorkloadGroupObservation::Absent {
                        group_id: group.group_id.clone(),
                        correlation: group.correlation.clone(),
                        backend_id: dotcore_runtime::RuntimeBackend::backend_id(self),
                    }),
                },
            }
        })
    }
}

/// The host process identity of a running unit's container, node-qualified against the
/// execution target's node; a stopped container, or a host that cannot read its own process
/// table for container processes, reports none.
fn unit_host_process(
    inspection: &GroupContainerInspection,
    state: WorkloadUnitState,
    managed_node_id: &SiteNodeId,
) -> Option<HostProcessIdentity> {
    inspection
        .process_id
        .filter(|_| state == WorkloadUnitState::Running)
        .and_then(|process_id| container_host_process(managed_node_id.clone(), process_id))
}
