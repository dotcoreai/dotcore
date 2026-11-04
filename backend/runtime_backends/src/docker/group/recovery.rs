//! Exact recovery of interrupted Docker workload-group lifecycle operations.

use super::super::ownership::{LABEL_CORRELATION, LABEL_GENERATION};
use super::*;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn exact_network_only_residue(
        &self,
        known: &KnownGroup,
        requested: &WorkloadGroupLaunch,
    ) -> Result<bool, RuntimeBackendError> {
        if known.launch != *requested {
            return Ok(false);
        }
        let remaining = self.validate_group_removal_evidence(known).await?;
        if !remaining.is_empty() {
            return Ok(false);
        }
        match self.client.inspect_owned_network(&known.network_id).await {
            Ok(network) => Ok(exact_known_network(
                &network,
                known,
                &BTreeSet::new(),
                &self.config,
            )),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(super::runtime_error_with_source(
                "Docker network-only group residue inspection failed",
                error,
            )),
        }
    }

    pub(super) async fn validate_group_removal_evidence(
        &self,
        known: &KnownGroup,
    ) -> Result<BTreeSet<String>, RuntimeBackendError> {
        let mut remaining = BTreeSet::new();
        for unit in known.units.values() {
            match self
                .client
                .inspect_group_container_ownership(&unit.container_id)
                .await
            {
                Ok(ownership) => {
                    if !group_container_has_expected_ownership(&ownership, unit) {
                        return Err(runtime_error(
                            "Docker workload group unit identity conflicts during removal retry",
                        ));
                    }
                    self.validate_unit_execution(unit).await?;
                    let inspection = self
                        .client
                        .inspect_group_container(&unit.container_id)
                        .await
                        .map_err(|error| {
                            super::runtime_error_with_source(
                                "Docker group unit retry network inspection failed",
                                error,
                            )
                        })?;
                    validate_unit_network(&inspection, unit, &known.network_id)?;
                    remaining.insert(unit.container_id.clone());
                }
                Err(error) if error.is_not_found() => {
                    if self
                        .client
                        .container_exists(&unit.request.name)
                        .await
                        .map_err(|error| {
                            super::runtime_error_with_source(
                                "Docker canonical group unit retry inspection failed",
                                error,
                            )
                        })?
                    {
                        return Err(runtime_error(
                            "Docker canonical group unit name was replaced during removal retry",
                        ));
                    }
                }
                Err(error) => {
                    return Err(super::runtime_error_with_source(
                        "Docker group unit retry ownership inspection failed",
                        error,
                    ));
                }
            }
        }
        let containers = self
            .client
            .list_managed_containers()
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker owned-container retry discovery failed",
                    error,
                )
            })?;
        let mut listed = BTreeSet::new();
        for container in containers.into_iter().filter(|container| {
            container.labels.get("com.dotcore.site").map(String::as_str)
                == Some(self.config.site_id.as_str())
                && container.labels.get(LABEL_GROUP).map(String::as_str)
                    == Some(known.launch.group_id.0.as_str())
        }) {
            let unit_name = container.labels.get(LABEL_UNIT).ok_or_else(|| {
                runtime_error("Docker workload group has malformed owned service units")
            })?;
            let unit = known
                .units
                .get(&WorkloadUnitName(unit_name.clone()))
                .ok_or_else(|| {
                    runtime_error("Docker workload group has an unexpected owned service unit")
                })?;
            if !container.name_is_unambiguous
                || container.id != unit.container_id
                || container.name != unit.request.name
                || !remaining.contains(&container.id)
                || !listed.insert(unit_name.clone())
            {
                return Err(runtime_error(
                    "Docker workload group has conflicting replacement evidence during removal retry",
                ));
            }
        }
        if listed.len() != remaining.len() {
            return Err(runtime_error(
                "Docker workload group ownership evidence changed during discovery",
            ));
        }
        let network = match self.client.inspect_owned_network(&known.network_id).await {
            Ok(network) => network,
            Err(error) if error.is_not_found() => {
                if remaining.is_empty() && self.network_is_absent_without_replacement(known).await?
                {
                    return Ok(remaining);
                }
                return Err(runtime_error(
                    "Docker workload group network disappeared while owned units remain",
                ));
            }
            Err(error) => {
                return Err(super::runtime_error_with_source(
                    "Docker group network retry inspection failed",
                    error,
                ));
            }
        };
        // Docker lists only running containers as network attachments, so stopped owned units
        // are absent here; each remaining unit's own network was verified above.
        if !known_network_has_expected_identity(&network, known, &self.config)
            || !network.attached_container_ids.is_subset(&remaining)
        {
            return Err(runtime_error(
                "Docker workload group network conflicts during removal retry",
            ));
        }
        Ok(remaining)
    }

    pub(super) async fn network_is_absent_without_replacement(
        &self,
        known: &KnownGroup,
    ) -> Result<bool, RuntimeBackendError> {
        Ok(self
            .exact_group_networks(&known.network_name)
            .await?
            .is_empty())
    }

    pub(super) async fn cleanup_partial_group(
        &self,
        known: &mut KnownGroup,
    ) -> Result<(), RuntimeBackendError> {
        self.validate_network_ownership(known).await?;
        for unit in known.units.values() {
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
                    "Docker partial-group ownership changed before cleanup",
                ));
            }
            self.validate_unit_execution(unit).await?;
        }
        for name in reverse_topological_order(&known.launch)? {
            let Some(unit) = known.units.get(&name).cloned() else {
                continue;
            };
            if let Err(error) = self.client.stop_container(&unit.container_id).await
                && !error.is_not_found()
                && !error.is_not_modified()
            {
                return Err(super::runtime_error_with_source(
                    "Docker partial-group stop failed",
                    error,
                ));
            }
            if let Err(error) = self.client.remove_container(&unit.container_id).await
                && !error.is_not_found()
            {
                return Err(super::runtime_error_with_source(
                    "Docker partial-group removal failed",
                    error,
                ));
            }
            known.units.remove(&name);
            self.known_groups()
                .lock()
                .expect("Docker group registry")
                .insert(known.launch.group_id.0.clone(), known.clone());
        }
        self.client
            .remove_network(&known.network_id)
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker partial-group network removal failed",
                    error,
                )
            })
    }

    pub(super) async fn known_group_from_launch(
        &self,
        group: &WorkloadGroupLaunch,
    ) -> Result<Option<KnownGroup>, RuntimeBackendError> {
        let current_prepared = prepare_group(group, &self.config)?;
        let managed_networks =
            self.client
                .list_managed_group_networks()
                .await
                .map_err(|error| {
                    super::runtime_error_with_source("Docker owned-network discovery failed", error)
                })?;
        let candidates = managed_networks
            .into_iter()
            .filter(|network| {
                network.labels.get("com.dotcore.site").map(String::as_str)
                    == Some(self.config.site_id.as_str())
                    && network.labels.get(LABEL_GROUP).map(String::as_str)
                        == Some(group.group_id.0.as_str())
            })
            .collect::<Vec<_>>();
        let containers = self
            .client
            .list_managed_containers()
            .await
            .map_err(|error| {
                super::runtime_error_with_source("Docker owned-container discovery failed", error)
            })?;
        let group_containers = containers
            .into_iter()
            .filter(|container| {
                container.labels.get("com.dotcore.site").map(String::as_str)
                    == Some(self.config.site_id.as_str())
                    && container.labels.get(LABEL_GROUP).map(String::as_str)
                        == Some(group.group_id.0.as_str())
            })
            .collect::<Vec<_>>();
        let network = match candidates.as_slice() {
            [] if group_containers.is_empty() => return Ok(None),
            [] => {
                return Err(runtime_error(
                    "Docker workload group has containers without its owned network",
                ));
            }
            [network]
                if !network.id.is_empty()
                    && !network.name.is_empty()
                    && !network.internal
                    && !network.attachable
                    && network.driver == "bridge" =>
            {
                network.clone()
            }
            [_] => {
                return Err(runtime_error(
                    "Docker workload group has malformed or unowned network",
                ));
            }
            _ => {
                return Err(runtime_error(
                    "Docker workload group network identity is ambiguous",
                ));
            }
        };
        let generation = network
            .labels
            .get(LABEL_GENERATION)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|generation| *generation > 0)
            .ok_or_else(|| runtime_error("Docker workload group has malformed generation"))?;
        if network.labels.get(LABEL_CORRELATION).map(String::as_str)
            != Some(group.correlation.owner_id.as_str())
        {
            return Err(runtime_error(
                "Docker workload group correlation does not belong to the requested owner",
            ));
        }
        let mut observed_group = group.clone();
        observed_group.correlation.reconcile_generation = generation;
        if network.labels != group_labels(&self.config, &observed_group, None) {
            return Err(runtime_error(
                "Docker workload group has malformed or unowned network",
            ));
        }
        let prepared = prepare_group(&observed_group, &self.config)?;
        let mut units = BTreeMap::new();
        for container in group_containers {
            if container.id.is_empty()
                || !container.name_is_unambiguous
                || container.name.is_empty()
            {
                return Err(runtime_error(
                    "Docker workload group has malformed owned service units",
                ));
            }
            let name = container.labels.get(LABEL_UNIT).cloned().ok_or_else(|| {
                runtime_error("Docker workload group has malformed ownership labels")
            })?;
            let name = WorkloadUnitName(name);
            if units.contains_key(&name) {
                return Err(runtime_error(
                    "Docker workload group has duplicate owned service units",
                ));
            }
            let mut request = prepared.units.get(&name).cloned().ok_or_else(|| {
                runtime_error("Docker workload group has an unexpected owned service unit")
            })?;
            request.name = container.name;
            request.network_name = network.name.clone();
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
            if !group_container_request_has_expected_ownership(&ownership, &request) {
                return Err(runtime_error(
                    "Docker workload group has malformed or unowned service containers",
                ));
            }
            let execution = self
                .client
                .inspect_group_execution(&ownership.id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker recovered group execution inspection failed",
                        error,
                    )
                })?;
            let current_unit = group
                .units
                .get(&name)
                .expect("validated current group unit");
            let current_request = current_prepared
                .units
                .get(&name)
                .expect("validated current group unit");
            if execution.image != current_request.image
                || (!current_request.entrypoint.is_empty()
                    && execution.entrypoint != current_request.entrypoint)
                || (!current_request.argv.is_empty() && execution.argv != current_request.argv)
                || !environment_matches_recovery(
                    &execution.environment,
                    &current_request.environment,
                    &current_unit.reconfigurable_environment,
                )
                || execution.mounts != current_request.mounts
            {
                return Err(runtime_error(
                    "Docker recovered workload group unit execution changed unexpectedly",
                ));
            }
            request.image = execution.image;
            request.entrypoint = execution.entrypoint;
            request.argv = execution.argv;
            request.environment = execution.environment;
            request.mounts = execution.mounts;
            units.insert(
                name,
                KnownGroupUnit {
                    container_id: ownership.id,
                    request,
                    gate: None,
                },
            );
        }
        Ok(Some(KnownGroup {
            launch: observed_group,
            network_id: network.id,
            network_name: network.name,
            units,
        }))
    }

    pub(super) async fn exact_group_networks(
        &self,
        network_name: &str,
    ) -> Result<Vec<DockerNetwork>, RuntimeBackendError> {
        self.client
            .list_group_networks(network_name)
            .await
            .map_err(|error| {
                super::runtime_error_with_source("Docker group network discovery failed", error)
            })
    }
}

pub(super) fn environment_matches_recovery(
    actual: &[String],
    expected: &[String],
    reconfigurable: &BTreeSet<String>,
) -> bool {
    let actual_len = actual.len();
    let expected_len = expected.len();
    let Some(actual) = environment_map(actual) else {
        return false;
    };
    let Some(expected) = environment_map(expected) else {
        return false;
    };
    actual.len() == actual_len
        && expected.len() == expected_len
        && expected.keys().all(|key| actual.contains_key(key))
        && reconfigurable.iter().all(|key| expected.contains_key(key))
        && expected
            .iter()
            .all(|(key, value)| reconfigurable.contains(key) || actual.get(key) == Some(value))
}

fn environment_map(values: &[String]) -> Option<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let (key, value) = value.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}
