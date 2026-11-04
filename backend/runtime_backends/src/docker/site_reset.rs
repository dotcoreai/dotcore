//! Docker half of a stopped-site reset: removes exactly the containers and networks
//! labeled for the target site, then proves nothing of the site remains on the host.
//! A cleanup may be narrowed to some execution targets of the site, for a node leaving it
//! while other nodes share the Docker daemon.
//!
//! Cleanup re-discovers the host, removes each container and then each network by its
//! exact recorded identity, and re-discovers again afterwards. Discovery only claims
//! resources whose `com.dotcore.*` labels match the target site, and the reset fails,
//! without removing anything, if that evidence is incomplete, ambiguous, or changes
//! during the run.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_runtime::{RuntimeBackendError, RuntimeBackendId, SiteRuntimeScope};

use super::{
    BACKEND_ID,
    client::{
        ClientError, DockerClient, DockerContainer, DockerNetwork, DockerNetworkSummary,
        GroupContainerOwnership, LocalDockerClient,
    },
    group::{
        GROUP_SCHEMA, SiteResetGroupIdentity, expected_group_labels,
        group_network_has_expected_ownership, site_reset_group_container_identity,
        site_reset_group_network_identity, validate_reset_unit_network,
    },
    ownership::{
        LABEL_CORRELATION, LABEL_EXECUTION_TARGET, LABEL_MANAGED, LABEL_SCHEMA, LABEL_SITE,
        OWNERSHIP_SCHEMA,
    },
    request::DockerGroupNetworkRequest,
};

/// Docker cleanup implementation used only by the stopped-site runtime reset boundary.
pub(super) struct DockerSiteReset<C> {
    client: C,
}

impl DockerSiteReset<LocalDockerClient> {
    async fn connect() -> Result<Self, anyhow::Error> {
        let client = LocalDockerClient::connect()
            .map_err(anyhow::Error::new)
            .map_err(|error| error.context("connect to local Docker daemon for site cleanup"))?;
        let client = client
            .negotiate_version()
            .await
            .map_err(anyhow::Error::new)
            .map_err(|error| error.context("negotiate Docker API for site cleanup"))?;
        Ok(Self { client })
    }
}

pub(crate) fn site_reset_adapter() -> Box<dyn crate::site_reset::SiteRuntimeResetAdapter> {
    Box::new(LocalDockerSiteReset {
        reset: tokio::sync::OnceCell::new(),
    })
}

struct LocalDockerSiteReset {
    reset: tokio::sync::OnceCell<DockerSiteReset<LocalDockerClient>>,
}

impl LocalDockerSiteReset {
    async fn reset(&self) -> Result<&DockerSiteReset<LocalDockerClient>, anyhow::Error> {
        self.reset.get_or_try_init(DockerSiteReset::connect).await
    }
}

impl crate::site_reset::SiteRuntimeResetAdapter for LocalDockerSiteReset {
    fn name(&self) -> &'static str {
        "Docker runtime"
    }

    fn connect(&self) -> crate::site_reset::SiteRuntimeResetFuture<'_> {
        Box::pin(async move { self.reset().await.map(|_| ()) })
    }

    fn prevalidate<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> crate::site_reset::SiteRuntimeResetFuture<'a, Vec<String>> {
        Box::pin(async move {
            self.reset()
                .await?
                .prevalidate(site_id, scope)
                .await
                .map_err(anyhow::Error::new)
        })
    }

    fn cleanup<'a>(
        &'a self,
        site_id: &'a str,
        scope: &'a SiteRuntimeScope,
    ) -> crate::site_reset::SiteRuntimeResetFuture<'a> {
        Box::pin(async move {
            self.reset()
                .await?
                .cleanup(site_id, scope)
                .await
                .map_err(anyhow::Error::new)
        })
    }
}

impl<C: DockerClient> DockerSiteReset<C> {
    #[cfg(test)]
    fn new(client: C) -> Self {
        Self { client }
    }

    /// Validates ownership evidence and describes every container and network the site owns
    /// within `scope`.
    pub(super) async fn prevalidate(
        &self,
        site_id: &str,
        scope: &SiteRuntimeScope,
    ) -> Result<Vec<String>, DockerSiteResetError> {
        Ok(self.discover_plan(site_id, scope).await?.descriptions())
    }

    pub(super) async fn cleanup(
        &self,
        site_id: &str,
        scope: &SiteRuntimeScope,
    ) -> Result<(), DockerSiteResetError> {
        let initial = self.discover_plan(site_id, scope).await?;
        for container in &initial.containers {
            self.remove_exact_container(container).await?;
        }

        let after_containers = self.discover_plan(site_id, scope).await?;
        if !after_containers.containers.is_empty() {
            return Err(invalid_evidence(
                "target-site Docker containers appeared during cleanup",
            ));
        }
        let remaining_networks = self
            .confirmed_remaining_networks(&initial.networks, &after_containers.networks)
            .await?;
        for network in remaining_networks {
            self.remove_exact_network(&network).await?;
        }

        let final_plan = self.discover_plan(site_id, scope).await?;
        if !final_plan.containers.is_empty() || !final_plan.networks.is_empty() {
            return Err(invalid_evidence(
                "target-site Docker resources remain after cleanup",
            ));
        }
        Ok(())
    }

    async fn discover_plan(
        &self,
        site_id: &str,
        scope: &SiteRuntimeScope,
    ) -> Result<DockerResetPlan, DockerSiteResetError> {
        let listed_containers = self
            .client
            .list_site_containers(site_id)
            .await
            .map_err(|source| discovery_error("container discovery", source))?;
        let listed_networks = self
            .client
            .list_site_group_network_summaries(site_id)
            .await
            .map_err(|source| discovery_error("network discovery", source))?;

        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut individual_workloads = BTreeSet::new();
        let mut individual_correlations = BTreeSet::new();
        let mut groups = BTreeMap::<String, ResetGroup>::new();
        let mut containers = Vec::new();

        for listed in listed_containers {
            require_target_site(&listed.labels, site_id, "container")?;
            if !in_scope(&listed.labels, scope, "container")? {
                continue;
            }
            validate_unique_resource(&listed.id, &listed.name, &mut ids, &mut names)?;
            require_managed(&listed.labels, "container")?;
            let schema = required_label(&listed.labels, LABEL_SCHEMA, "container")?;
            match schema {
                OWNERSHIP_SCHEMA => {
                    let known = listed
                        .classify_owned_for_site(site_id, RuntimeBackendId(BACKEND_ID.to_owned()))
                        .map_err(runtime_evidence_error)?
                        .ok_or_else(|| {
                            invalid_evidence("target-site individual container was not classified")
                        })?;
                    if !individual_workloads.insert(known.identity.workload_id.0.clone())
                        || !individual_correlations
                            .insert(known.identity.correlation.owner_id.clone())
                    {
                        return Err(invalid_evidence(
                            "target-site individual container ownership is duplicate or ambiguous",
                        ));
                    }
                    let inspected = self
                        .client
                        .inspect_owned_container(&listed.id)
                        .await
                        .map_err(|source| {
                            discovery_error("container ownership inspection", source)
                        })?;
                    if inspected != listed {
                        return Err(invalid_evidence(
                            "target-site individual container was replaced during discovery",
                        ));
                    }
                    let inspected_known = inspected
                        .classify_owned_for_site(site_id, RuntimeBackendId(BACKEND_ID.to_owned()))
                        .map_err(runtime_evidence_error)?
                        .ok_or_else(|| {
                            invalid_evidence("target-site individual ownership changed")
                        })?;
                    if inspected_known.container_id != known.container_id
                        || inspected_known.identity != known.identity
                        || inspected_known.ports != known.ports
                    {
                        return Err(invalid_evidence(
                            "target-site individual ownership changed during discovery",
                        ));
                    }
                    containers.push(ResetContainer::Individual(inspected));
                }
                GROUP_SCHEMA => {
                    let parsed = site_reset_group_container_identity(&listed, site_id)
                        .map_err(runtime_evidence_error)?;
                    let (identity, unit) = parsed.into_parts();
                    let group = groups
                        .entry(identity.group_id().to_owned())
                        .or_insert_with(|| ResetGroup::new(identity.clone()));
                    group.require_same_identity(&identity)?;
                    if group.units.insert(unit, listed.clone()).is_some() {
                        return Err(invalid_evidence(
                            "target-site group has duplicate service units",
                        ));
                    }
                }
                _ => {
                    return Err(invalid_evidence(format!(
                        "target-site Docker container uses unsupported ownership schema {schema}"
                    )));
                }
            }
        }

        for listed in listed_networks {
            require_target_site(&listed.labels, site_id, "network")?;
            if !in_scope(&listed.labels, scope, "network")? {
                continue;
            }
            validate_unique_resource(&listed.id, &listed.name, &mut ids, &mut names)?;
            require_managed(&listed.labels, "network")?;
            let schema = required_label(&listed.labels, LABEL_SCHEMA, "network")?;
            if schema != GROUP_SCHEMA {
                return Err(invalid_evidence(format!(
                    "target-site Docker network uses unsupported ownership schema {schema}"
                )));
            }
            let identity = site_reset_group_network_identity(&listed, site_id)
                .map_err(runtime_evidence_error)?;
            let expected = DockerGroupNetworkRequest {
                name: identity.network_name(),
                labels: identity.network_labels(),
            };
            let inspected = self
                .client
                .inspect_owned_network(&listed.id)
                .await
                .map_err(|source| discovery_error("group network ownership inspection", source))?;
            if !network_summary_matches(&listed, &inspected)
                || !group_network_has_expected_ownership(&inspected, &expected)
            {
                return Err(invalid_evidence(
                    "target-site group network was replaced during discovery",
                ));
            }
            let group = groups
                .entry(identity.group_id().to_owned())
                .or_insert_with(|| ResetGroup::new(identity.clone()));
            group.require_same_identity(&identity)?;
            if group.network.replace(inspected).is_some() {
                return Err(invalid_evidence(
                    "target-site group network identity is duplicate or ambiguous",
                ));
            }
        }

        let mut networks = Vec::new();
        for group in groups.into_values() {
            let network = group.network.ok_or_else(|| {
                invalid_evidence("target-site group containers have no exact owned network")
            })?;
            let expected_ids = group
                .units
                .values()
                .map(|container| container.id.clone())
                .collect::<BTreeSet<_>>();
            if !network.attached_container_ids.is_empty()
                && network.attached_container_ids != expected_ids
            {
                return Err(invalid_evidence(
                    "target-site group network has foreign or ambiguous attachments",
                ));
            }
            for (unit, listed) in group.units {
                let ownership = self
                    .client
                    .inspect_group_container_ownership(&listed.id)
                    .await
                    .map_err(|source| {
                        discovery_error("group container ownership inspection", source)
                    })?;
                if ownership.id != listed.id || ownership.name != listed.name {
                    return Err(invalid_evidence(
                        "target-site group container was replaced during discovery",
                    ));
                }
                let runtime_labels = group.identity.container_labels(&unit);
                if ownership.labels
                    != expected_group_labels(&ownership.image_labels, &runtime_labels)
                {
                    return Err(invalid_evidence(
                        "target-site group container ownership is malformed",
                    ));
                }
                let inspection = self
                    .client
                    .inspect_group_container(&listed.id)
                    .await
                    .map_err(|source| {
                        discovery_error("group container network inspection", source)
                    })?;
                validate_reset_unit_network(
                    &inspection,
                    &group.identity.network_name(),
                    &unit,
                    &network.id,
                )
                .map_err(runtime_evidence_error)?;
                containers.push(ResetContainer::Group(ownership));
            }
            networks.push(network);
        }

        containers.sort_by(|left, right| left.id().cmp(right.id()));
        networks.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(DockerResetPlan {
            containers,
            networks,
        })
    }

    async fn remove_exact_container(
        &self,
        container: &ResetContainer,
    ) -> Result<(), DockerSiteResetError> {
        container.validate_current(&self.client).await?;
        if let Err(source) = self.client.stop_container(container.id()).await
            && !source.is_not_found()
            && !source.is_not_modified()
        {
            return Err(mutation_error("container stop", source));
        }
        if let Err(source) = self.client.remove_container(container.id()).await
            && !source.is_not_found()
            && !self.container_is_absent(container).await?
        {
            return Err(mutation_error("container removal", source));
        }
        if !self.container_is_absent(container).await? {
            return Err(invalid_evidence(
                "target-site Docker container removal is uncertain or replaced",
            ));
        }
        Ok(())
    }

    async fn container_is_absent(
        &self,
        container: &ResetContainer,
    ) -> Result<bool, DockerSiteResetError> {
        match self.client.inspect_owned_container(container.id()).await {
            Err(source) if source.is_not_found() => {}
            Ok(_) => return Ok(false),
            Err(source) => return Err(discovery_error("container absence inspection", source)),
        }
        self.client
            .container_exists(container.name())
            .await
            .map(|exists| !exists)
            .map_err(|source| discovery_error("container replacement inspection", source))
    }

    async fn confirmed_remaining_networks(
        &self,
        initial: &[DockerNetwork],
        discovered: &[DockerNetwork],
    ) -> Result<Vec<DockerNetwork>, DockerSiteResetError> {
        let discovered_by_id = discovered
            .iter()
            .map(|network| (network.id.as_str(), network))
            .collect::<BTreeMap<_, _>>();
        let initial_ids = initial
            .iter()
            .map(|network| network.id.as_str())
            .collect::<BTreeSet<_>>();
        if discovered
            .iter()
            .any(|network| !initial_ids.contains(network.id.as_str()))
        {
            return Err(invalid_evidence(
                "target-site group network was replaced during container cleanup",
            ));
        }
        let mut remaining = Vec::new();
        for network in initial {
            if let Some(current) = discovered_by_id.get(network.id.as_str()) {
                if !same_network_ownership(network, current)
                    || !current.attached_container_ids.is_empty()
                {
                    return Err(invalid_evidence(
                        "target-site group network is changed or nonempty after container cleanup",
                    ));
                }
                remaining.push((*current).clone());
            } else if !self
                .client
                .list_group_networks(&network.name)
                .await
                .map_err(|source| discovery_error("network replacement inspection", source))?
                .is_empty()
            {
                return Err(invalid_evidence(
                    "target-site group network was replaced during container cleanup",
                ));
            }
        }
        Ok(remaining)
    }

    async fn remove_exact_network(
        &self,
        network: &DockerNetwork,
    ) -> Result<(), DockerSiteResetError> {
        match self.client.inspect_owned_network(&network.id).await {
            Ok(current) if current == *network && current.attached_container_ids.is_empty() => {}
            Ok(_) => {
                return Err(invalid_evidence(
                    "target-site group network changed before removal",
                ));
            }
            Err(source) if source.is_not_found() => {
                return self.confirm_network_name_absent(&network.name).await;
            }
            Err(source) => return Err(discovery_error("network removal preflight", source)),
        }
        if let Err(source) = self.client.remove_network(&network.id).await
            && !source.is_not_found()
        {
            if self
                .confirm_network_name_absent(&network.name)
                .await
                .is_err()
            {
                return Err(mutation_error("network removal", source));
            }
            return Ok(());
        }
        self.confirm_network_name_absent(&network.name).await
    }

    async fn confirm_network_name_absent(
        &self,
        network_name: &str,
    ) -> Result<(), DockerSiteResetError> {
        let networks = self
            .client
            .list_group_networks(network_name)
            .await
            .map_err(|source| discovery_error("network absence inspection", source))?;
        if networks.is_empty() {
            Ok(())
        } else {
            Err(invalid_evidence(
                "target-site group network removal is uncertain or replaced",
            ))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum DockerSiteResetError {
    #[error("Docker site cleanup discovery failed during {operation}: {source}")]
    Discovery {
        operation: &'static str,
        #[source]
        source: ClientError,
    },

    #[error("Docker site cleanup rejected unsafe ownership evidence: {0}")]
    InvalidEvidence(#[source] anyhow::Error),

    #[error("Docker site cleanup mutation failed during {operation}: {source}")]
    Mutation {
        operation: &'static str,
        #[source]
        source: ClientError,
    },
}

#[derive(Default)]
struct DockerResetPlan {
    containers: Vec<ResetContainer>,
    networks: Vec<DockerNetwork>,
}

impl DockerResetPlan {
    fn descriptions(&self) -> Vec<String> {
        let containers = self.containers.iter().map(ResetContainer::description);
        let networks = self
            .networks
            .iter()
            .map(|network| format!("application network {}", network.name));
        containers.chain(networks).collect()
    }
}

enum ResetContainer {
    Individual(DockerContainer),
    Group(GroupContainerOwnership),
}

impl ResetContainer {
    fn id(&self) -> &str {
        match self {
            Self::Individual(container) => &container.id,
            Self::Group(container) => &container.id,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Individual(container) => &container.name,
            Self::Group(container) => &container.name,
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Individual(container) => match container.labels.get(LABEL_CORRELATION) {
                Some(owner_id) => format!("model container {} (owner {owner_id})", container.name),
                None => format!("model container {}", container.name),
            },
            Self::Group(container) => format!("application container {}", container.name),
        }
    }

    async fn validate_current<C: DockerClient>(
        &self,
        client: &C,
    ) -> Result<(), DockerSiteResetError> {
        let current = match self {
            Self::Individual(expected) => client
                .inspect_owned_container(&expected.id)
                .await
                .map(|current| current == *expected),
            Self::Group(expected) => client
                .inspect_group_container_ownership(&expected.id)
                .await
                .map(|current| current == *expected),
        };
        match current {
            Ok(true) => Ok(()),
            Ok(false) => Err(invalid_evidence(
                "target-site Docker container changed before removal",
            )),
            Err(source) => Err(discovery_error("container removal preflight", source)),
        }
    }
}

struct ResetGroup {
    identity: SiteResetGroupIdentity,
    network: Option<DockerNetwork>,
    units: BTreeMap<String, DockerContainer>,
}

impl ResetGroup {
    fn new(identity: SiteResetGroupIdentity) -> Self {
        Self {
            identity,
            network: None,
            units: BTreeMap::new(),
        }
    }

    fn require_same_identity(
        &self,
        identity: &SiteResetGroupIdentity,
    ) -> Result<(), DockerSiteResetError> {
        if self.identity == *identity {
            Ok(())
        } else {
            Err(invalid_evidence(
                "target-site group ownership correlation is ambiguous",
            ))
        }
    }
}

fn require_target_site(
    labels: &BTreeMap<String, String>,
    site_id: &str,
    resource: &'static str,
) -> Result<(), DockerSiteResetError> {
    if labels.get(LABEL_SITE).map(String::as_str) == Some(site_id) {
        Ok(())
    } else {
        Err(invalid_evidence(format!(
            "Docker site-filtered {resource} has mismatched target ownership"
        )))
    }
}

/// Reports whether a site-labelled resource belongs to the cleanup scope. Under a target scope the
/// target label must be present and well formed, so no resource is skipped on unreadable evidence.
fn in_scope(
    labels: &BTreeMap<String, String>,
    scope: &SiteRuntimeScope,
    resource: &'static str,
) -> Result<bool, DockerSiteResetError> {
    let SiteRuntimeScope::ExecutionTargets(targets) = scope else {
        return Ok(true);
    };
    let target = required_label(labels, LABEL_EXECUTION_TARGET, resource)?;
    if !dotcore_domain::valid_id(target) {
        return Err(invalid_evidence(format!(
            "target-site Docker {resource} has malformed ownership label {LABEL_EXECUTION_TARGET}"
        )));
    }
    Ok(targets.contains(target))
}

fn require_managed(
    labels: &BTreeMap<String, String>,
    resource: &'static str,
) -> Result<(), DockerSiteResetError> {
    if labels.get(LABEL_MANAGED).map(String::as_str) == Some("true") {
        Ok(())
    } else {
        Err(invalid_evidence(format!(
            "target-site Docker {resource} has malformed managed ownership"
        )))
    }
}

fn required_label<'a>(
    labels: &'a BTreeMap<String, String>,
    label: &str,
    resource: &'static str,
) -> Result<&'a str, DockerSiteResetError> {
    labels
        .get(label)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            invalid_evidence(format!(
                "target-site Docker {resource} is missing ownership label {label}"
            ))
        })
}

fn validate_unique_resource(
    id: &str,
    name: &str,
    ids: &mut BTreeSet<String>,
    names: &mut BTreeSet<String>,
) -> Result<(), DockerSiteResetError> {
    if id.is_empty()
        || id.chars().any(char::is_control)
        || name.is_empty()
        || !ids.insert(id.to_owned())
        || !names.insert(name.to_owned())
    {
        return Err(invalid_evidence(
            "target-site Docker resource identity is duplicate or ambiguous",
        ));
    }
    Ok(())
}

fn network_summary_matches(summary: &DockerNetworkSummary, network: &DockerNetwork) -> bool {
    summary.id == network.id && summary.name == network.name && summary.labels == network.labels
}

fn same_network_ownership(left: &DockerNetwork, right: &DockerNetwork) -> bool {
    left.id == right.id
        && left.name == right.name
        && left.labels == right.labels
        && left.internal == right.internal
        && left.attachable == right.attachable
        && left.driver == right.driver
}

fn runtime_evidence_error(source: RuntimeBackendError) -> DockerSiteResetError {
    DockerSiteResetError::InvalidEvidence(anyhow::Error::new(source))
}

fn invalid_evidence(message: impl Into<String>) -> DockerSiteResetError {
    DockerSiteResetError::InvalidEvidence(anyhow::anyhow!(message.into()))
}

fn discovery_error(operation: &'static str, source: ClientError) -> DockerSiteResetError {
    DockerSiteResetError::Discovery { operation, source }
}

fn mutation_error(operation: &'static str, source: ClientError) -> DockerSiteResetError {
    DockerSiteResetError::Mutation { operation, source }
}

#[cfg(test)]
mod tests {
    use std::{
        future::ready,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::docker::{
        client::{
            ContainerInspection, DockerGroupExecution, GroupContainerInspection,
            GroupNetworkAttachment,
        },
        ownership::{LABEL_CORRELATION, LABEL_EXECUTION_TARGET, LABEL_GENERATION},
        request::{DockerCreateRequest, DockerMount},
    };

    #[tokio::test]
    async fn mixed_exact_cleanup_removes_all_containers_before_networks_and_is_idempotent() {
        let client = FakeDockerClient::mixed();
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);

        reset
            .prevalidate("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("safe plan");
        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("exact cleanup");
        let first_mutations = state.lock().expect("state").mutations.clone();
        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("idempotent retry");

        let state = state.lock().expect("state");
        assert_eq!(
            first_mutations,
            [
                "stop:model-id",
                "remove-container:model-id",
                "stop:unit-id",
                "remove-container:unit-id",
                "remove-network:network-id",
            ]
        );
        assert_eq!(state.mutations, first_mutations);
        assert_eq!(state.containers, [foreign_malformed_container()]);
        assert!(state.networks.is_empty());
        assert_eq!(state.image_operations, 0);
    }

    #[tokio::test]
    async fn inventory_describes_every_owned_resource_without_mutating() {
        let client = FakeDockerClient::mixed();
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);

        let descriptions = reset
            .prevalidate("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("safe plan");

        assert_eq!(
            descriptions,
            [
                "model container dotcore_site-one_deployment-one_1 (owner deployment-one)",
                "application container dotcore_site-one_group_app-one_unit_web",
                "application network dotcore_site-one_group_app-one",
            ]
        );
        let state = state.lock().expect("state");
        assert!(state.mutations.is_empty());
        assert_eq!(state.networks.len(), 1);
    }

    #[tokio::test]
    async fn every_unsafe_target_evidence_case_fails_before_destructive_calls() {
        for case in [
            UnsafeCase::Malformed,
            UnsafeCase::MissingManagedContainer,
            UnsafeCase::FalseManagedContainer,
            UnsafeCase::MalformedManagedContainer,
            UnsafeCase::MissingManagedNetwork,
            UnsafeCase::FalseManagedNetwork,
            UnsafeCase::MalformedManagedNetwork,
            UnsafeCase::UnsupportedSchema,
            UnsafeCase::Duplicate,
            UnsafeCase::Replaced,
            UnsafeCase::Ambiguous,
            UnsafeCase::ForeignAttachment,
        ] {
            let client = FakeDockerClient::unsafe_case(case);
            let state = Arc::clone(&client.state);
            let reset = DockerSiteReset::new(client);

            reset
                .prevalidate("site-one", &SiteRuntimeScope::Site)
                .await
                .expect_err("unsafe ownership evidence");

            let state = state.lock().expect("state");
            assert!(state.mutations.is_empty(), "case {case:?}");
            assert_eq!(state.containers, case.expected_containers());
            assert_eq!(state.networks, case.expected_networks());
        }
    }

    #[tokio::test]
    async fn exact_empty_group_network_only_residue_is_removed_and_repeat_is_absent() {
        let mut network = group_network();
        network.attached_container_ids.clear();
        let client = FakeDockerClient::new(Vec::new(), vec![network]);
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("network cleanup");
        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("absent retry");

        let state = state.lock().expect("state");
        assert!(state.containers.is_empty());
        assert!(state.networks.is_empty());
        assert_eq!(state.mutations, ["remove-network:network-id"]);
    }

    #[tokio::test]
    async fn foreign_malformed_resources_are_ignored_before_remaining_label_parsing() {
        let client = FakeDockerClient::new(
            vec![foreign_malformed_container()],
            vec![foreign_malformed_network()],
        );
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("foreign evidence is outside the target site");

        let state = state.lock().expect("state");
        assert!(state.mutations.is_empty());
        assert_eq!(state.containers, [foreign_malformed_container()]);
        assert_eq!(state.networks, [foreign_malformed_network()]);
    }

    #[tokio::test]
    async fn partial_container_removal_failure_converges_on_retry_before_network_deletion() {
        let client = FakeDockerClient::mixed();
        let state = Arc::clone(&client.state);
        state
            .lock()
            .expect("state")
            .fail_remove_once
            .insert("unit-id".to_owned());
        let reset = DockerSiteReset::new(client);

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect_err("injected partial removal");
        {
            let state = state.lock().expect("state");
            assert_eq!(
                state.containers,
                [group_container(), foreign_malformed_container()]
            );
            assert_eq!(state.networks, [group_network()]);
            assert_eq!(
                state.mutations,
                [
                    "stop:model-id",
                    "remove-container:model-id",
                    "stop:unit-id",
                    "remove-container:unit-id",
                ]
            );
        }

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("retry cleanup");

        let state = state.lock().expect("state");
        assert_eq!(state.containers, [foreign_malformed_container()]);
        assert!(state.networks.is_empty());
        assert_eq!(
            state.mutations.last(),
            Some(&"remove-network:network-id".to_owned())
        );
    }

    #[tokio::test]
    async fn target_scope_plans_and_removes_only_the_worker_targets_resources() {
        let client = FakeDockerClient::shared_daemon();
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);
        let scope = worker_scope();

        let descriptions = reset
            .prevalidate("site-one", &scope)
            .await
            .expect("scoped plan");
        reset
            .cleanup("site-one", &scope)
            .await
            .expect("scoped cleanup");

        assert_eq!(
            descriptions,
            [
                "model container dotcore_site-one_worker-deployment_1 (owner worker-deployment)",
                "application container dotcore_site-one_group_app-worker_unit_web",
                "application network dotcore_site-one_group_app-worker",
            ]
        );
        let state = state.lock().expect("state");
        assert_eq!(
            state.mutations,
            [
                "stop:worker-model-id",
                "remove-container:worker-model-id",
                "stop:worker-unit-id",
                "remove-container:worker-unit-id",
                "remove-network:worker-network-id",
            ]
        );
        assert_eq!(
            state
                .containers
                .iter()
                .map(|container| container.id.as_str())
                .collect::<Vec<_>>(),
            ["manager-model-id", "manager-unit-id"]
        );
        assert_eq!(
            state
                .networks
                .iter()
                .map(|network| network.id.as_str())
                .collect::<Vec<_>>(),
            ["manager-network-id"]
        );
    }

    #[tokio::test]
    async fn site_scope_still_removes_every_target_of_the_site() {
        let client = FakeDockerClient::shared_daemon();
        let state = Arc::clone(&client.state);
        let reset = DockerSiteReset::new(client);

        reset
            .cleanup("site-one", &SiteRuntimeScope::Site)
            .await
            .expect("site-wide cleanup");

        let state = state.lock().expect("state");
        assert!(state.containers.is_empty());
        assert!(state.networks.is_empty());
    }

    #[tokio::test]
    async fn unattributable_site_resources_fail_the_scoped_cleanup_before_any_removal() {
        for (name, damage) in [
            (
                "missing container target",
                (|client: &mut FakeDockerState| {
                    client.containers[0].labels.remove(LABEL_EXECUTION_TARGET);
                }) as fn(&mut FakeDockerState),
            ),
            ("malformed container target", |state| {
                state.containers[0]
                    .labels
                    .insert(LABEL_EXECUTION_TARGET.to_owned(), "Bad_Target".to_owned());
            }),
            ("empty container target", |state| {
                state.containers[0]
                    .labels
                    .insert(LABEL_EXECUTION_TARGET.to_owned(), String::new());
            }),
            ("missing network target", |state| {
                state.networks[0].labels.remove(LABEL_EXECUTION_TARGET);
            }),
            ("malformed network target", |state| {
                state.networks[0]
                    .labels
                    .insert(LABEL_EXECUTION_TARGET.to_owned(), "Bad_Target".to_owned());
            }),
        ] {
            let client = FakeDockerClient::shared_daemon();
            let state = Arc::clone(&client.state);
            damage(&mut state.lock().expect("state"));
            let before = {
                let state = state.lock().expect("state");
                (state.containers.clone(), state.networks.clone())
            };
            let reset = DockerSiteReset::new(client);

            let error = reset
                .cleanup("site-one", &worker_scope())
                .await
                .expect_err(name);

            assert!(
                error.to_string().contains(LABEL_EXECUTION_TARGET),
                "{name}: {error}"
            );
            let state = state.lock().expect("state");
            assert!(state.mutations.is_empty(), "{name}");
            assert_eq!((state.containers.clone(), state.networks.clone()), before);
        }
    }

    #[tokio::test]
    async fn another_targets_resources_are_skipped_before_further_validation() {
        let client = FakeDockerClient::shared_daemon();
        let state = Arc::clone(&client.state);
        {
            // Malformed beyond its target label: only a target-scoped pass may skip it unread.
            let mut state = state.lock().expect("state");
            state.containers[0].labels.remove(LABEL_MANAGED);
            state.containers[0].labels.remove(LABEL_SCHEMA);
            state.networks[0].labels.remove(LABEL_SCHEMA);
        }
        let reset = DockerSiteReset::new(client);

        reset
            .cleanup("site-one", &worker_scope())
            .await
            .expect("other target is skipped");
        let site_wide = reset
            .prevalidate("site-one", &SiteRuntimeScope::Site)
            .await
            .expect_err("site-wide pass still validates every resource");

        assert!(site_wide.to_string().contains("managed"), "{site_wide}");
        let state = state.lock().expect("state");
        assert_eq!(state.mutations.len(), 5);
    }

    #[derive(Clone, Copy, Debug)]
    enum UnsafeCase {
        Malformed,
        MissingManagedContainer,
        FalseManagedContainer,
        MalformedManagedContainer,
        MissingManagedNetwork,
        FalseManagedNetwork,
        MalformedManagedNetwork,
        UnsupportedSchema,
        Duplicate,
        Replaced,
        Ambiguous,
        ForeignAttachment,
    }

    impl UnsafeCase {
        fn expected_containers(self) -> Vec<DockerContainer> {
            match self {
                Self::Malformed => {
                    let mut container = individual_container("model-id", "deployment-one", 1);
                    container.labels.remove(LABEL_GENERATION);
                    vec![container]
                }
                Self::MissingManagedContainer
                | Self::FalseManagedContainer
                | Self::MalformedManagedContainer => {
                    let mut container = individual_container("model-id", "deployment-one", 1);
                    match self {
                        Self::MissingManagedContainer => {
                            container.labels.remove(LABEL_MANAGED);
                        }
                        Self::FalseManagedContainer => {
                            container
                                .labels
                                .insert(LABEL_MANAGED.to_owned(), "false".to_owned());
                        }
                        Self::MalformedManagedContainer => {
                            container
                                .labels
                                .insert(LABEL_MANAGED.to_owned(), "TRUE".to_owned());
                        }
                        _ => unreachable!(),
                    }
                    vec![container]
                }
                Self::MissingManagedNetwork
                | Self::FalseManagedNetwork
                | Self::MalformedManagedNetwork => Vec::new(),
                Self::UnsupportedSchema => {
                    let mut container = individual_container("model-id", "deployment-one", 1);
                    container
                        .labels
                        .insert(LABEL_SCHEMA.to_owned(), "99".to_owned());
                    vec![container]
                }
                Self::Duplicate => vec![
                    individual_container("model-a", "deployment-one", 1),
                    individual_container("model-b", "deployment-one", 2),
                ],
                Self::Replaced | Self::Ambiguous => {
                    let mut container = individual_container("model-id", "deployment-one", 1);
                    if matches!(self, Self::Ambiguous) {
                        container.name_is_unambiguous = false;
                    }
                    vec![container]
                }
                Self::ForeignAttachment => vec![group_container()],
            }
        }

        fn expected_networks(self) -> Vec<DockerNetwork> {
            if matches!(self, Self::ForeignAttachment) {
                let mut network = group_network();
                network
                    .attached_container_ids
                    .insert("foreign-container".to_owned());
                vec![network]
            } else if matches!(
                self,
                Self::MissingManagedNetwork
                    | Self::FalseManagedNetwork
                    | Self::MalformedManagedNetwork
            ) {
                let mut network = group_network();
                match self {
                    Self::MissingManagedNetwork => {
                        network.labels.remove(LABEL_MANAGED);
                    }
                    Self::FalseManagedNetwork => {
                        network
                            .labels
                            .insert(LABEL_MANAGED.to_owned(), "false".to_owned());
                    }
                    Self::MalformedManagedNetwork => {
                        network
                            .labels
                            .insert(LABEL_MANAGED.to_owned(), "TRUE".to_owned());
                    }
                    _ => unreachable!(),
                }
                vec![network]
            } else {
                Vec::new()
            }
        }
    }

    #[derive(Clone)]
    struct FakeDockerClient {
        state: Arc<Mutex<FakeDockerState>>,
    }

    #[derive(Default)]
    struct FakeDockerState {
        containers: Vec<DockerContainer>,
        networks: Vec<DockerNetwork>,
        owned_overrides: BTreeMap<String, DockerContainer>,
        mutations: Vec<String>,
        fail_remove_once: BTreeSet<String>,
        image_operations: usize,
    }

    impl FakeDockerClient {
        fn new(containers: Vec<DockerContainer>, networks: Vec<DockerNetwork>) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeDockerState {
                    containers,
                    networks,
                    ..FakeDockerState::default()
                })),
            }
        }

        fn mixed() -> Self {
            Self::new(
                vec![
                    individual_container("model-id", "deployment-one", 1),
                    group_container(),
                    foreign_malformed_container(),
                ],
                vec![group_network()],
            )
        }

        /// A daemon shared by the manager and the worker of one site, each with a model container,
        /// an application group container, and its group network.
        fn shared_daemon() -> Self {
            let mut manager_model =
                individual_container("manager-model-id", "manager-deployment", 1);
            manager_model.labels.insert(
                LABEL_EXECUTION_TARGET.to_owned(),
                "manager-target".to_owned(),
            );
            let mut worker_model = individual_container("worker-model-id", "worker-deployment", 1);
            worker_model.labels.insert(
                LABEL_EXECUTION_TARGET.to_owned(),
                "worker-target".to_owned(),
            );
            Self::new(
                vec![
                    manager_model,
                    group_container_of("manager-unit-id", "app-manager", "manager-target"),
                    worker_model,
                    group_container_of("worker-unit-id", "app-worker", "worker-target"),
                ],
                vec![
                    group_network_of(
                        "manager-network-id",
                        "app-manager",
                        "manager-target",
                        "manager-unit-id",
                    ),
                    group_network_of(
                        "worker-network-id",
                        "app-worker",
                        "worker-target",
                        "worker-unit-id",
                    ),
                ],
            )
        }

        fn unsafe_case(case: UnsafeCase) -> Self {
            let client = Self::new(case.expected_containers(), case.expected_networks());
            if matches!(case, UnsafeCase::Replaced) {
                let mut replacement = individual_container("model-id", "deployment-one", 1);
                replacement.id = "replacement-id".to_owned();
                client
                    .state
                    .lock()
                    .expect("state")
                    .owned_overrides
                    .insert("model-id".to_owned(), replacement);
            }
            client
        }

        fn find_container(&self, id_or_name: &str) -> Result<DockerContainer, ClientError> {
            let state = self.state.lock().expect("state");
            if let Some(container) = state.owned_overrides.get(id_or_name) {
                return Ok(container.clone());
            }
            state
                .containers
                .iter()
                .find(|container| container.id == id_or_name || container.name == id_or_name)
                .cloned()
                .ok_or_else(|| ClientError::not_found("container absent"))
        }

        fn find_network(&self, id: &str) -> Result<DockerNetwork, ClientError> {
            self.state
                .lock()
                .expect("state")
                .networks
                .iter()
                .find(|network| network.id == id)
                .cloned()
                .ok_or_else(|| ClientError::not_found("network absent"))
        }
    }

    impl DockerClient for FakeDockerClient {
        fn list_managed_containers(
            &self,
        ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
            ready(Err(ClientError::new(
                "reset must not query containers by managed ownership",
            )))
        }

        fn list_site_containers(
            &self,
            site_id: &str,
        ) -> impl Future<Output = Result<Vec<DockerContainer>, ClientError>> + Send {
            ready(Ok(self
                .state
                .lock()
                .expect("state")
                .containers
                .iter()
                .filter(|container| {
                    container.labels.get(LABEL_SITE).map(String::as_str) == Some(site_id)
                })
                .cloned()
                .collect()))
        }

        fn create_container(
            &self,
            _request: &DockerCreateRequest,
        ) -> impl Future<Output = Result<String, ClientError>> + Send {
            ready(Err(ClientError::new("unexpected container creation")))
        }

        fn inspect_owned_container(
            &self,
            container_id: &str,
        ) -> impl Future<Output = Result<DockerContainer, ClientError>> + Send {
            ready(self.find_container(container_id))
        }

        fn container_exists(
            &self,
            workload_id: &str,
        ) -> impl Future<Output = Result<bool, ClientError>> + Send {
            ready(Ok(self.find_container(workload_id).is_ok()))
        }

        fn image_exists(
            &self,
            _image: &str,
        ) -> impl Future<Output = Result<bool, ClientError>> + Send {
            self.state.lock().expect("state").image_operations += 1;
            ready(Err(ClientError::new("unexpected image operation")))
        }

        fn start_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            ready(Err(ClientError::new("unexpected container start")))
        }

        fn inspect_container(
            &self,
            _workload_id: &str,
        ) -> impl Future<Output = Result<ContainerInspection, ClientError>> + Send {
            ready(Err(ClientError::new("unexpected generic inspection")))
        }

        fn stop_container(
            &self,
            workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            self.state
                .lock()
                .expect("state")
                .mutations
                .push(format!("stop:{workload_id}"));
            ready(Ok(()))
        }

        fn remove_container(
            &self,
            workload_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            let mut state = self.state.lock().expect("state");
            state
                .mutations
                .push(format!("remove-container:{workload_id}"));
            if state.fail_remove_once.remove(workload_id) {
                return ready(Err(ClientError::new("injected removal failure")));
            }
            state
                .containers
                .retain(|container| container.id != workload_id);
            for network in &mut state.networks {
                network.attached_container_ids.remove(workload_id);
            }
            ready(Ok(()))
        }

        fn inspect_owned_network(
            &self,
            network_id: &str,
        ) -> impl Future<Output = Result<DockerNetwork, ClientError>> + Send {
            ready(self.find_network(network_id))
        }

        fn list_group_networks(
            &self,
            network_name: &str,
        ) -> impl Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
            ready(Ok(self
                .state
                .lock()
                .expect("state")
                .networks
                .iter()
                .filter(|network| network.name == network_name)
                .cloned()
                .collect()))
        }

        fn list_managed_group_network_summaries(
            &self,
        ) -> impl Future<Output = Result<Vec<DockerNetworkSummary>, ClientError>> + Send {
            ready(Err(ClientError::new(
                "reset must not query networks by managed ownership",
            )))
        }

        fn list_site_group_network_summaries(
            &self,
            site_id: &str,
        ) -> impl Future<Output = Result<Vec<DockerNetworkSummary>, ClientError>> + Send {
            ready(Ok(self
                .state
                .lock()
                .expect("state")
                .networks
                .iter()
                .filter(|network| {
                    network.labels.get(LABEL_SITE).map(String::as_str) == Some(site_id)
                })
                .map(|network| DockerNetworkSummary {
                    id: network.id.clone(),
                    name: network.name.clone(),
                    labels: network.labels.clone(),
                })
                .collect()))
        }

        fn list_managed_group_networks(
            &self,
        ) -> impl Future<Output = Result<Vec<DockerNetwork>, ClientError>> + Send {
            ready(Ok(self.state.lock().expect("state").networks.clone()))
        }

        fn remove_network(
            &self,
            network_id: &str,
        ) -> impl Future<Output = Result<(), ClientError>> + Send {
            let mut state = self.state.lock().expect("state");
            state.mutations.push(format!("remove-network:{network_id}"));
            state.networks.retain(|network| network.id != network_id);
            ready(Ok(()))
        }

        fn inspect_group_container(
            &self,
            container_id: &str,
        ) -> impl Future<Output = Result<GroupContainerInspection, ClientError>> + Send {
            let container = self.find_container(container_id);
            ready(container.map(|container| {
                let unit = container.labels["com.dotcore.runtime.unit"].clone();
                let network_name = format!(
                    "dotcore_site-one_group_{}",
                    container.labels["com.dotcore.runtime.group"]
                );
                let network_id = self
                    .state
                    .lock()
                    .expect("state")
                    .networks
                    .iter()
                    .find(|network| network.name == network_name)
                    .map_or_else(|| "network-id".to_owned(), |network| network.id.clone());
                GroupContainerInspection {
                    running: false,
                    process_id: None,
                    host_bindings: BTreeMap::new(),
                    private_address: None,
                    networks: BTreeMap::from([(
                        network_name,
                        GroupNetworkAttachment {
                            network_id: Some(network_id),
                            aliases: Some(vec![unit]),
                        },
                    )]),
                }
            }))
        }

        fn inspect_group_container_ownership(
            &self,
            container_id: &str,
        ) -> impl Future<Output = Result<GroupContainerOwnership, ClientError>> + Send {
            ready(
                self.find_container(container_id)
                    .map(|container| GroupContainerOwnership {
                        id: container.id,
                        name: container.name,
                        labels: container.labels,
                        image_labels: BTreeMap::new(),
                    }),
            )
        }

        fn inspect_group_execution(
            &self,
            _container_id: &str,
        ) -> impl Future<Output = Result<DockerGroupExecution, ClientError>> + Send {
            ready(Ok(DockerGroupExecution {
                image: String::new(),
                entrypoint: Vec::new(),
                argv: Vec::new(),
                environment: Vec::new(),
                mounts: Vec::<DockerMount>::new(),
            }))
        }
    }

    fn individual_container(id: &str, correlation: &str, generation: u64) -> DockerContainer {
        let name = format!("dotcore_site-one_{correlation}_{generation}");
        DockerContainer {
            id: id.to_owned(),
            name: name.clone(),
            name_is_unambiguous: true,
            labels: BTreeMap::from([
                (LABEL_MANAGED.to_owned(), "true".to_owned()),
                (LABEL_SITE.to_owned(), "site-one".to_owned()),
                (LABEL_EXECUTION_TARGET.to_owned(), "test-target".to_owned()),
                (LABEL_SCHEMA.to_owned(), OWNERSHIP_SCHEMA.to_owned()),
                (LABEL_CORRELATION.to_owned(), correlation.to_owned()),
                (LABEL_GENERATION.to_owned(), generation.to_string()),
                ("com.dotcore.runtime.workload".to_owned(), name),
                (
                    "com.dotcore.workload.ports".to_owned(),
                    "inference=8080".to_owned(),
                ),
                (
                    "com.dotcore.workload.launch-fingerprint".to_owned(),
                    "a".repeat(64),
                ),
            ]),
        }
    }

    fn group_container() -> DockerContainer {
        DockerContainer {
            id: "unit-id".to_owned(),
            name: "dotcore_site-one_group_app-one_unit_web".to_owned(),
            name_is_unambiguous: true,
            labels: SiteResetGroupIdentity::test_identity(
                "site-one",
                "app-one",
                "application-one",
                1,
            )
            .container_labels("web"),
        }
    }

    fn group_network() -> DockerNetwork {
        DockerNetwork {
            id: "network-id".to_owned(),
            name: "dotcore_site-one_group_app-one".to_owned(),
            labels: SiteResetGroupIdentity::test_identity(
                "site-one",
                "app-one",
                "application-one",
                1,
            )
            .network_labels(),
            internal: false,
            attachable: false,
            driver: "bridge".to_owned(),
            attached_container_ids: BTreeSet::from(["unit-id".to_owned()]),
        }
    }

    fn group_container_of(id: &str, group: &str, target: &str) -> DockerContainer {
        DockerContainer {
            id: id.to_owned(),
            name: format!("dotcore_site-one_group_{group}_unit_web"),
            name_is_unambiguous: true,
            labels: SiteResetGroupIdentity::test_identity_on_target(
                "site-one",
                group,
                "application-one",
                1,
                target,
            )
            .container_labels("web"),
        }
    }

    fn group_network_of(id: &str, group: &str, target: &str, unit_id: &str) -> DockerNetwork {
        DockerNetwork {
            id: id.to_owned(),
            name: format!("dotcore_site-one_group_{group}"),
            labels: SiteResetGroupIdentity::test_identity_on_target(
                "site-one",
                group,
                "application-one",
                1,
                target,
            )
            .network_labels(),
            internal: false,
            attachable: false,
            driver: "bridge".to_owned(),
            attached_container_ids: BTreeSet::from([unit_id.to_owned()]),
        }
    }

    fn worker_scope() -> SiteRuntimeScope {
        SiteRuntimeScope::ExecutionTargets(BTreeSet::from(["worker-target".to_owned()]))
    }

    fn foreign_malformed_container() -> DockerContainer {
        DockerContainer {
            id: "foreign-id".to_owned(),
            name: String::new(),
            name_is_unambiguous: false,
            labels: BTreeMap::from([
                (LABEL_MANAGED.to_owned(), "true".to_owned()),
                (LABEL_SITE.to_owned(), "foreign-site".to_owned()),
            ]),
        }
    }

    fn foreign_malformed_network() -> DockerNetwork {
        DockerNetwork {
            id: String::new(),
            name: String::new(),
            labels: BTreeMap::from([(LABEL_SITE.to_owned(), "foreign-site".to_owned())]),
            internal: true,
            attachable: true,
            driver: String::new(),
            attached_container_ids: BTreeSet::from(["unknown".to_owned()]),
        }
    }
}
