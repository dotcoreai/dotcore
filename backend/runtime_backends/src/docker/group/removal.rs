//! Reverse dependency ordering for Docker workload-group removal.

use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackend, RuntimeBackendError, RuntimeFuture, WorkloadGroupId,
    WorkloadGroupLaunch, WorkloadUnitName,
};

use super::super::{
    client::{DockerContainer, DockerNetwork, GroupContainerOwnership},
    ownership::LABEL_SITE,
    request::DockerGroupNetworkRequest,
};
use super::creation::topological_order;

pub(super) fn reverse_topological_order(
    group: &WorkloadGroupLaunch,
) -> Result<Vec<WorkloadUnitName>, RuntimeBackendError> {
    let mut ordered = topological_order(group)?;
    ordered.reverse();
    Ok(ordered)
}

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn validate_empty_known_network(
        &self,
        known: &KnownGroup,
    ) -> Result<(), RuntimeBackendError> {
        match self.client.inspect_owned_network(&known.network_id).await {
            Ok(network) if exact_known_network(&network, known, &BTreeSet::new(), &self.config) => {
                Ok(())
            }
            Ok(_) => Err(runtime_error(
                "Docker workload group network is not empty and exact before removal",
            )),
            Err(error) if error.is_not_found() => {
                if self.network_is_absent_without_replacement(known).await? {
                    Ok(())
                } else {
                    Err(runtime_error(
                        "Docker workload group network was replaced before removal",
                    ))
                }
            }
            Err(error) => Err(super::runtime_error_with_source(
                "Docker empty group network inspection failed",
                error,
            )),
        }
    }

    pub(super) async fn remove_quiescent_network(
        &self,
        group: &WorkloadGroupLaunch,
    ) -> Result<(), RuntimeBackendError> {
        let prepared = prepare_group(group, &self.config)?;
        self.validate_expected_container_names_absent(&prepared)
            .await?;
        let networks = self.exact_group_networks(&prepared.network_name).await?;
        match networks.as_slice() {
            [] => Ok(()),
            [network] if exact_quiescent_network(network, &prepared.network) => self
                .client
                .remove_network(&network.id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker quiescent group network removal failed",
                        error,
                    )
                }),
            _ => Err(runtime_error(
                "Docker workload-group residue is not one empty expected owned network",
            )),
        }
    }
}

use super::*;

#[derive(Clone)]
struct StableGroupCleanupContainer {
    listed: DockerContainer,
    ownership: GroupContainerOwnership,
    unit: String,
}

struct StableGroupCleanupPlan {
    containers: Vec<StableGroupCleanupContainer>,
    network: Option<DockerNetwork>,
}

impl StableGroupCleanupPlan {
    fn is_empty(&self) -> bool {
        self.containers.is_empty() && self.network.is_none()
    }
}

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) fn ensure_group_absent<'a>(
        &'a self,
        group_id: &'a WorkloadGroupId,
        owner_id: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            if !dotcore_domain::valid_id(&group_id.0) || !dotcore_domain::valid_id(owner_id) {
                return Err(runtime_error(
                    "Docker workload group cleanup identity is invalid",
                ));
            }
            if let Some(known) = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group_id.0)
                && known.launch.correlation.owner_id != owner_id
            {
                return Err(runtime_error(
                    "Docker workload group cleanup identity belongs to another owner",
                ));
            }

            let plan = self.stable_group_cleanup_plan(group_id, owner_id).await?;
            for container in &plan.containers {
                self.validate_cleanup_container(container).await?;
                if let Err(error) = self.client.stop_container(&container.listed.id).await
                    && !error.is_not_found()
                    && !error.is_not_modified()
                {
                    return Err(super::runtime_error_with_source(
                        "Docker workload group cleanup stop failed",
                        error,
                    ));
                }
                if let Err(error) = self.client.remove_container(&container.listed.id).await {
                    match self
                        .client
                        .inspect_group_container_ownership(&container.listed.id)
                        .await
                    {
                        Err(refresh) if refresh.is_not_found() => {}
                        Ok(current) if current == container.ownership => {
                            return Err(super::runtime_error_with_source(
                                "Docker workload group cleanup removal failed and exact owned container remains",
                                error,
                            ));
                        }
                        Ok(_) => {
                            return Err(runtime_error(
                                "Docker workload group cleanup container ownership changed after removal",
                            ));
                        }
                        Err(refresh) => {
                            return Err(super::runtime_error_with_source(
                                "Docker workload group cleanup container confirmation failed",
                                refresh,
                            ));
                        }
                    }
                }
            }

            if let Some(network) = &plan.network {
                let current = self.inspect_cleanup_network(network).await?;
                if !current.attached_container_ids.is_empty() {
                    return Err(runtime_error(
                        "Docker workload group cleanup network retains foreign or ambiguous attachments",
                    ));
                }
                if let Err(error) = self.client.remove_network(&network.id).await {
                    match self.client.inspect_owned_network(&network.id).await {
                        Err(refresh) if refresh.is_not_found() => {}
                        Ok(current) if current == *network => {
                            return Err(super::runtime_error_with_source(
                                "Docker workload group cleanup network removal failed and exact owned network remains",
                                error,
                            ));
                        }
                        Ok(_) => {
                            return Err(runtime_error(
                                "Docker workload group cleanup network ownership changed after removal",
                            ));
                        }
                        Err(refresh) => {
                            return Err(super::runtime_error_with_source(
                                "Docker workload group cleanup network confirmation failed",
                                refresh,
                            ));
                        }
                    }
                }
            }

            if !self
                .stable_group_cleanup_plan(group_id, owner_id)
                .await?
                .is_empty()
            {
                return Err(runtime_error(
                    "Docker workload group resources remain after cleanup",
                ));
            }
            self.known_groups()
                .lock()
                .expect("Docker group registry")
                .remove(&group_id.0);
            Ok(())
        })
    }

    async fn stable_group_cleanup_plan(
        &self,
        group_id: &WorkloadGroupId,
        owner_id: &str,
    ) -> Result<StableGroupCleanupPlan, RuntimeBackendError> {
        let listed_containers = self
            .client
            .list_managed_containers()
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker workload group cleanup container discovery failed",
                    error,
                )
            })?;
        let listed_networks = self
            .client
            .list_managed_group_network_summaries()
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker workload group cleanup network discovery failed",
                    error,
                )
            })?;
        let mut identity = None;
        let mut units = BTreeSet::new();
        let mut container_ids = BTreeSet::new();
        let mut containers = Vec::new();
        for listed in listed_containers.into_iter().filter(|container| {
            cleanup_candidate(&container.labels, &self.config.site_id, &group_id.0)
        }) {
            let parsed = site_reset_group_container_identity(&listed, &self.config.site_id)?;
            let (candidate, unit) = parsed.into_parts();
            merge_cleanup_identity(
                &mut identity,
                &candidate,
                owner_id,
                &self.config.execution_target_id,
            )?;
            if !units.insert(unit.clone()) || !container_ids.insert(listed.id.clone()) {
                return Err(runtime_error(
                    "Docker workload group cleanup container identity is duplicate or ambiguous",
                ));
            }
            let ownership = self
                .client
                .inspect_group_container_ownership(&listed.id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker workload group cleanup container ownership inspection failed",
                        error,
                    )
                })?;
            let expected_labels =
                expected_group_labels(&ownership.image_labels, &candidate.container_labels(&unit));
            if ownership.id != listed.id
                || ownership.name != listed.name
                || ownership.labels != expected_labels
            {
                return Err(runtime_error(
                    "Docker workload group cleanup container ownership is not exact",
                ));
            }
            containers.push(StableGroupCleanupContainer {
                listed,
                ownership,
                unit,
            });
        }

        let mut network = None;
        for listed in listed_networks
            .into_iter()
            .filter(|network| cleanup_candidate(&network.labels, &self.config.site_id, &group_id.0))
        {
            let candidate = site_reset_group_network_identity(&listed, &self.config.site_id)?;
            merge_cleanup_identity(
                &mut identity,
                &candidate,
                owner_id,
                &self.config.execution_target_id,
            )?;
            if network.is_some() {
                return Err(runtime_error(
                    "Docker workload group cleanup network identity is duplicate or ambiguous",
                ));
            }
            let inspected = self
                .client
                .inspect_owned_network(&listed.id)
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker workload group cleanup network ownership inspection failed",
                        error,
                    )
                })?;
            let expected = DockerGroupNetworkRequest {
                name: candidate.network_name(),
                labels: candidate.network_labels(),
            };
            if inspected.id != listed.id
                || inspected.name != listed.name
                || inspected.labels != listed.labels
                || !group_network_has_expected_ownership(&inspected, &expected)
            {
                return Err(runtime_error(
                    "Docker workload group cleanup network ownership is not exact",
                ));
            }
            network = Some(inspected);
        }
        if !containers.is_empty() && network.is_none() {
            return Err(runtime_error(
                "Docker workload group cleanup containers have no exact owned network",
            ));
        }
        if let Some(network) = &network {
            if !network.attached_container_ids.is_empty()
                && network.attached_container_ids != container_ids
            {
                return Err(runtime_error(
                    "Docker workload group cleanup network has foreign or ambiguous attachments",
                ));
            }
            for container in &containers {
                let inspection = self
                    .client
                    .inspect_group_container(&container.listed.id)
                    .await
                    .map_err(|error| {
                        super::runtime_error_with_source(
                            "Docker workload group cleanup container network inspection failed",
                            error,
                        )
                    })?;
                validate_reset_unit_network(
                    &inspection,
                    &network.name,
                    &container.unit,
                    &network.id,
                )?;
            }
        }
        containers.sort_by(|left, right| left.listed.id.cmp(&right.listed.id));
        Ok(StableGroupCleanupPlan {
            containers,
            network,
        })
    }

    async fn validate_cleanup_container(
        &self,
        expected: &StableGroupCleanupContainer,
    ) -> Result<(), RuntimeBackendError> {
        let current = self
            .client
            .inspect_group_container_ownership(&expected.listed.id)
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker workload group cleanup container ownership refresh failed",
                    error,
                )
            })?;
        if current != expected.ownership {
            return Err(runtime_error(
                "Docker workload group cleanup container ownership changed before removal",
            ));
        }
        Ok(())
    }

    async fn inspect_cleanup_network(
        &self,
        expected: &DockerNetwork,
    ) -> Result<DockerNetwork, RuntimeBackendError> {
        let current = self
            .client
            .inspect_owned_network(&expected.id)
            .await
            .map_err(|error| {
                super::runtime_error_with_source(
                    "Docker workload group cleanup network ownership refresh failed",
                    error,
                )
            })?;
        if current.id != expected.id
            || current.name != expected.name
            || current.labels != expected.labels
            || current.internal != expected.internal
            || current.attachable != expected.attachable
            || current.driver != expected.driver
        {
            return Err(runtime_error(
                "Docker workload group cleanup network ownership changed before removal",
            ));
        }
        Ok(current)
    }

    pub fn remove_observed_group<'a>(
        &'a self,
        group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            if group.backend_id != RuntimeBackend::backend_id(self) {
                return Err(runtime_error(
                    "Docker observed workload group belongs to another runtime",
                ));
            }
            let known = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group.group_id.0)
                .cloned()
                .ok_or_else(|| runtime_error("Docker observed workload group is not retained"))?;
            if known.launch.group_id != group.group_id
                || known.launch.correlation != group.correlation
                || known.units.keys().ne(group.units.keys())
            {
                return Err(runtime_error(
                    "Docker observed workload group is not exact owned infrastructure",
                ));
            }
            let launch = known.launch.clone();
            self.remove_group(&launch).await
        })
    }

    pub(super) fn remove_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            validate_group_launch(group, &self.config)?;
            let known = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .get(&group.group_id.0)
                .cloned();
            let Some(mut known) = known else {
                return self.remove_quiescent_network(group).await;
            };
            if known.launch != *group {
                return Err(runtime_error(
                    "Docker workload group launch identity changed before removal",
                ));
            }
            let remaining = self.validate_group_removal_evidence(&known).await?;
            if remaining.is_empty() && self.network_is_absent_without_replacement(&known).await? {
                self.known_groups()
                    .lock()
                    .expect("Docker group registry")
                    .remove(&group.group_id.0);
                return Ok(());
            }
            for name in reverse_topological_order(&known.launch)? {
                let Some(unit) = known.units.get(&name) else {
                    continue;
                };
                if !remaining.contains(&unit.container_id) {
                    continue;
                }
                if let Err(error) = self.client.stop_container(&unit.container_id).await
                    && !error.is_not_found()
                    && !error.is_not_modified()
                {
                    return Err(super::runtime_error_with_source(
                        "Docker group unit stop before removal failed",
                        error,
                    ));
                }
                if let Err(error) = self.client.remove_container(&unit.container_id).await {
                    match self
                        .client
                        .inspect_group_container_ownership(&unit.container_id)
                        .await
                    {
                        Err(refresh_error) if refresh_error.is_not_found() => {}
                        Ok(ownership)
                            if group_container_has_expected_ownership(&ownership, unit) =>
                        {
                            return Err(super::runtime_error_with_source(
                                "Docker group unit removal failed and exact owned unit remains",
                                error,
                            ));
                        }
                        Ok(_) => {
                            return Err(runtime_error(
                                "Docker group unit removal outcome conflicts with refreshed ownership evidence",
                            ));
                        }
                        Err(refresh_error) => {
                            return Err(super::runtime_error_with_source(
                                "Docker group unit removal evidence refresh failed",
                                refresh_error,
                            ));
                        }
                    }
                }
                known.units.remove(&name);
                self.known_groups()
                    .lock()
                    .expect("Docker group registry")
                    .insert(group.group_id.0.clone(), known.clone());
            }
            self.validate_empty_known_network(&known).await?;
            if let Err(error) = self.client.remove_network(&known.network_id).await
                && !self.network_is_absent_without_replacement(&known).await?
            {
                return Err(super::runtime_error_with_source(
                    "Docker group network removal outcome is uncertain and owned network remains",
                    error,
                ));
            }
            self.known_groups()
                .lock()
                .expect("Docker group registry")
                .remove(&group.group_id.0);
            Ok(())
        })
    }
}

fn cleanup_candidate(labels: &BTreeMap<String, String>, site_id: &str, group_id: &str) -> bool {
    labels.get(LABEL_SITE).map(String::as_str) == Some(site_id)
        && labels.get(LABEL_GROUP).map(String::as_str) == Some(group_id)
}

fn merge_cleanup_identity(
    retained: &mut Option<SiteResetGroupIdentity>,
    candidate: &SiteResetGroupIdentity,
    owner_id: &str,
    execution_target_id: &str,
) -> Result<(), RuntimeBackendError> {
    if candidate.owner_id() != owner_id {
        return Err(runtime_error(
            "Docker workload group cleanup identity belongs to another owner",
        ));
    }
    // A shared daemon may host several execution targets; cleanup acts only on its own target.
    if candidate.execution_target_id() != execution_target_id {
        return Err(runtime_error(
            "Docker workload group cleanup identity belongs to another execution target",
        ));
    }
    if retained
        .as_ref()
        .is_some_and(|current| current != candidate)
    {
        return Err(runtime_error(
            "Docker workload group cleanup resources have inconsistent generations or ownership",
        ));
    }
    retained.get_or_insert_with(|| candidate.clone());
    Ok(())
}
