//! Safe extraction of unknown owned Docker workload-group identities.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_runtime::{RuntimeBackendError, WorkloadGroupId};

use super::{
    super::{
        client::{DockerContainer, DockerNetworkSummary},
        ownership::{LABEL_CORRELATION, LABEL_GENERATION, LABEL_MANAGED, LABEL_SCHEMA, LABEL_SITE},
        runtime_error,
        validation::validate_name,
    },
    types::{GROUP_SCHEMA, LABEL_GROUP, LABEL_NETWORK, LABEL_UNIT},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteResetGroupIdentity {
    site_id: String,
    group_id: String,
    correlation: String,
    generation: u64,
    execution_target_id: String,
}

impl SiteResetGroupIdentity {
    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    pub fn owner_id(&self) -> &str {
        &self.correlation
    }

    pub fn execution_target_id(&self) -> &str {
        &self.execution_target_id
    }

    #[cfg(test)]
    pub fn test_identity(
        site_id: &str,
        group_id: &str,
        correlation: &str,
        generation: u64,
    ) -> Self {
        Self::test_identity_on_target(site_id, group_id, correlation, generation, "test-target")
    }

    #[cfg(test)]
    pub fn test_identity_on_target(
        site_id: &str,
        group_id: &str,
        correlation: &str,
        generation: u64,
        execution_target_id: &str,
    ) -> Self {
        Self {
            site_id: site_id.to_owned(),
            group_id: group_id.to_owned(),
            correlation: correlation.to_owned(),
            generation,
            execution_target_id: execution_target_id.to_owned(),
        }
    }

    pub fn network_name(&self) -> String {
        format!("dotcore_{}_group_{}", self.site_id, self.group_id)
    }

    pub fn network_labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.common_labels();
        labels.insert(LABEL_NETWORK.to_owned(), "true".to_owned());
        labels
    }

    pub fn container_labels(&self, unit: &str) -> BTreeMap<String, String> {
        let mut labels = self.common_labels();
        labels.insert(LABEL_UNIT.to_owned(), unit.to_owned());
        labels
    }

    fn common_labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (LABEL_MANAGED.to_owned(), "true".to_owned()),
            (LABEL_SITE.to_owned(), self.site_id.clone()),
            (
                "com.dotcore.execution-target".to_owned(),
                self.execution_target_id.clone(),
            ),
            (LABEL_SCHEMA.to_owned(), GROUP_SCHEMA.to_owned()),
            (LABEL_CORRELATION.to_owned(), self.correlation.clone()),
            (LABEL_GENERATION.to_owned(), self.generation.to_string()),
            (LABEL_GROUP.to_owned(), self.group_id.clone()),
        ])
    }
}

pub struct SiteResetGroupContainerIdentity {
    group: SiteResetGroupIdentity,
    unit: String,
}

impl SiteResetGroupContainerIdentity {
    pub fn into_parts(self) -> (SiteResetGroupIdentity, String) {
        (self.group, self.unit)
    }
}

pub fn site_reset_group_container_identity(
    container: &DockerContainer,
    site_id: &str,
) -> Result<SiteResetGroupContainerIdentity, RuntimeBackendError> {
    let group = site_reset_group_identity(&container.labels, site_id)?;
    let unit = required_label(&container.labels, LABEL_UNIT)?;
    validate_name(unit, "group unit name")?;
    if container.labels.contains_key(LABEL_NETWORK) {
        return Err(runtime_error(
            "Docker group container has network ownership labels",
        ));
    }
    let expected_name = format!("{}_unit_{unit}", group.network_name());
    if !container.name_is_unambiguous || container.name != expected_name {
        return Err(runtime_error("Docker group container name is ambiguous"));
    }
    Ok(SiteResetGroupContainerIdentity {
        group,
        unit: unit.to_owned(),
    })
}

pub fn site_reset_group_network_identity(
    network: &DockerNetworkSummary,
    site_id: &str,
) -> Result<SiteResetGroupIdentity, RuntimeBackendError> {
    let group = site_reset_group_identity(&network.labels, site_id)?;
    if network.labels.get(LABEL_NETWORK).map(String::as_str) != Some("true")
        || network.labels.contains_key(LABEL_UNIT)
        || network.id.is_empty()
        || network.name != group.network_name()
        || network.labels != group.network_labels()
    {
        return Err(runtime_error(
            "Docker group network has malformed ownership labels",
        ));
    }
    Ok(group)
}

pub(super) fn discovered_group_ids(
    containers: impl IntoIterator<Item = DockerContainer>,
    site_id: &str,
    execution_target_id: &str,
) -> Result<BTreeSet<WorkloadGroupId>, RuntimeBackendError> {
    let mut discovered = BTreeSet::new();
    for container in containers {
        if container.labels.get(LABEL_SCHEMA).map(String::as_str) != Some(GROUP_SCHEMA)
            || container.labels.get(LABEL_SITE).map(String::as_str) != Some(site_id)
            || container
                .labels
                .get("com.dotcore.execution-target")
                .map(String::as_str)
                != Some(execution_target_id)
        {
            continue;
        }
        let identity = site_reset_group_container_identity(&container, site_id)?;
        discovered.insert(WorkloadGroupId(identity.group.group_id));
    }
    Ok(discovered)
}

fn site_reset_group_identity(
    labels: &BTreeMap<String, String>,
    site_id: &str,
) -> Result<SiteResetGroupIdentity, RuntimeBackendError> {
    if labels.get(LABEL_MANAGED).map(String::as_str) != Some("true")
        || labels.get(LABEL_SITE).map(String::as_str) != Some(site_id)
        || labels.get(LABEL_SCHEMA).map(String::as_str) != Some(GROUP_SCHEMA)
    {
        return Err(runtime_error(
            "Docker group resource has malformed ownership labels",
        ));
    }
    let execution_target_id = required_label(labels, "com.dotcore.execution-target")?;
    if !dotcore_domain::valid_id(execution_target_id) {
        return Err(runtime_error(
            "Docker group execution-target identity is malformed",
        ));
    }
    let group_id = required_label(labels, LABEL_GROUP)?;
    if group_id.len() > 40
        || !group_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(runtime_error("Docker group identity is malformed"));
    }
    let correlation = required_label(labels, LABEL_CORRELATION)?;
    if !dotcore_domain::valid_id(correlation) {
        return Err(runtime_error("Docker group correlation is malformed"));
    }
    let generation = required_label(labels, LABEL_GENERATION)?
        .parse::<u64>()
        .ok()
        .filter(|generation| *generation > 0)
        .ok_or_else(|| runtime_error("Docker group generation is malformed"))?;
    Ok(SiteResetGroupIdentity {
        site_id: site_id.to_owned(),
        group_id: group_id.to_owned(),
        correlation: correlation.to_owned(),
        generation,
        execution_target_id: execution_target_id.to_owned(),
    })
}

fn required_label<'a>(
    labels: &'a BTreeMap<String, String>,
    label: &str,
) -> Result<&'a str, RuntimeBackendError> {
    labels
        .get(label)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| runtime_error(format!("Docker group resource is missing label {label}")))
}

use super::*;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) fn discover_groups<'a>(
        &'a self,
    ) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(async move {
            let groups = self
                .known_groups()
                .lock()
                .expect("Docker group registry")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut observations = Vec::with_capacity(groups.len());
            let mut known_ids = BTreeSet::new();
            for group in groups {
                known_ids.insert(group.launch.group_id.0.clone());
                observations.push(self.inspect_known_group(&group, &group.launch).await?);
            }
            let containers = self
                .client
                .list_managed_containers()
                .await
                .map_err(|error| {
                    super::runtime_error_with_source(
                        "Docker owned-container discovery failed",
                        error,
                    )
                })?;
            let discovered = discovered_group_ids(
                containers,
                &self.config.site_id,
                &self.config.execution_target_id,
            )?;
            observations.extend(
                discovered
                    .into_iter()
                    .filter(|group_id| !known_ids.contains(&group_id.0))
                    .map(|group_id| WorkloadGroupObservation::Unknown {
                        group_id,
                        backend_id: dotcore_runtime::RuntimeBackend::backend_id(self),
                    }),
            );
            Ok(observations)
        })
    }
}
