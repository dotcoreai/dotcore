//! Defines the `com.dotcore.*` labels Dotcore stamps on every container and network it
//! creates, and uses them to classify discovered Docker containers into known runtime
//! workload identities.
//!
//! The same classification works with or without a live backend connection, so the
//! development runtime and site reset can reuse it; a classified container carries the
//! workload's stable identity (such as `dotcore_<site>_<owner>_<generation>`) and its
//! declared ports. Containers belonging to another site or an older label schema are
//! skipped, while malformed labels on a target-site container are an error.

use dotcore_runtime::{
    ObservedWorkload, RuntimeBackendError, RuntimeBackendId, WorkloadCorrelation, WorkloadId,
};

use super::{
    adapter::{DockerRuntimeConfig, KnownWorkload},
    client::DockerContainer,
    group::GROUP_SCHEMA,
    request::DockerPort,
    validation::{validate_name, validate_workload_name},
};

pub(super) const LABEL_MANAGED: &str = "com.dotcore.managed";
pub(super) const LABEL_SITE: &str = "com.dotcore.site";
pub(super) const LABEL_EXECUTION_TARGET: &str = "com.dotcore.execution-target";
pub(super) const LABEL_SCHEMA: &str = "com.dotcore.runtime.schema";
pub(super) const LABEL_CORRELATION: &str = "com.dotcore.workload.correlation";
pub(super) const LABEL_WORKLOAD: &str = "com.dotcore.runtime.workload";
pub(super) const LABEL_GENERATION: &str = "com.dotcore.reconcile.generation";
pub(super) const LABEL_PORTS: &str = "com.dotcore.workload.ports";
pub(super) const LABEL_LAUNCH_FINGERPRINT: &str = "com.dotcore.workload.launch-fingerprint";
/// Version of the Docker ownership labels emitted by runtime.
pub const OWNERSHIP_SCHEMA: &str = "6";

impl DockerContainer {
    /// Classifies complete, schema-valid ownership evidence for this site.
    pub(super) fn classify_owned(
        &self,
        config: &DockerRuntimeConfig,
        backend_id: RuntimeBackendId,
    ) -> Result<Option<KnownWorkload>, RuntimeBackendError> {
        if self.labels.get(LABEL_MANAGED).map(String::as_str) != Some("true") {
            return Ok(None);
        }
        let site_id = required_label(self, LABEL_SITE)?;
        if site_id != config.site_id {
            return Ok(None);
        }
        let execution_target_id = required_label(self, LABEL_EXECUTION_TARGET)?;
        if !valid_identity(execution_target_id) {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed ownership label {LABEL_EXECUTION_TARGET}",
                self.name
            )));
        }
        if execution_target_id != config.execution_target_id {
            return Ok(None);
        }
        self.classify_owned_for_site(&config.site_id, backend_id)
    }

    /// Classifies complete individual-workload evidence without constructing a live adapter.
    pub(super) fn classify_owned_for_site(
        &self,
        site_id: &str,
        backend_id: RuntimeBackendId,
    ) -> Result<Option<KnownWorkload>, RuntimeBackendError> {
        if self.labels.get(LABEL_MANAGED).map(String::as_str) != Some("true") {
            return Ok(None);
        }
        let site = required_label(self, LABEL_SITE)?;
        if !valid_identity(site) {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed ownership label {LABEL_SITE}",
                self.name
            )));
        }
        if site != site_id {
            return Ok(None);
        }
        if !self.name_is_unambiguous || self.name.is_empty() {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker owned container must have exactly one unambiguous name",
            ));
        }
        if self.id.is_empty() || self.id.chars().any(char::is_control) {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has no immutable container identity",
                self.name
            )));
        }
        let schema = required_label(self, LABEL_SCHEMA)?;
        if schema == GROUP_SCHEMA {
            // Owned by the application workload-group ownership domain, not this runtime.
            return Ok(None);
        }
        if schema != OWNERSHIP_SCHEMA {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} uses unsupported ownership schema {schema}",
                self.name
            )));
        }
        let execution_target_id = required_label(self, LABEL_EXECUTION_TARGET)?;
        if !valid_identity(execution_target_id) {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed ownership label {LABEL_EXECUTION_TARGET}",
                self.name
            )));
        }
        let owner_id = required_label(self, LABEL_CORRELATION)?;
        let workload = required_label(self, LABEL_WORKLOAD)?;
        let generation = required_label(self, LABEL_GENERATION)?
            .parse::<u64>()
            .ok()
            .filter(|generation| *generation > 0)
            .ok_or_else(|| {
                RuntimeBackendError::invalid_launch(format!(
                    "Docker workload {} has malformed reconcile generation",
                    self.name
                ))
            })?;
        let ports = parse_ports(required_label(self, LABEL_PORTS)?)?;
        let launch_fingerprint = required_label(self, LABEL_LAUNCH_FINGERPRINT)?;
        if launch_fingerprint.len() != 64
            || !launch_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed launch fingerprint",
                self.name
            )));
        }
        if !valid_identity(owner_id) {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed workload correlation",
                self.name
            )));
        }
        let expected_workload = format!("dotcore_{site}_{owner_id}_{generation}");
        if validate_workload_name(workload).is_err()
            || workload != expected_workload
            || self.name != expected_workload
        {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has ambiguous workload correlation or reconcile generation",
                self.name
            )));
        }
        Ok(Some(KnownWorkload {
            container_id: self.id.clone(),
            identity: ObservedWorkload {
                workload_id: WorkloadId(workload.to_owned()),
                correlation: WorkloadCorrelation {
                    owner_id: owner_id.to_owned(),
                    reconcile_generation: generation,
                },
                backend_id,
                loopback_endpoints: Vec::new(),
                services: Vec::new(),
                host_process: None,
            },
            ports,
            launch_fingerprint: launch_fingerprint.to_owned(),
            managed_node_id: None,
        }))
    }
}

fn parse_ports(value: &str) -> Result<Vec<DockerPort>, RuntimeBackendError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let ports = value
        .split(',')
        .map(|value| {
            let Some((name, port)) = value.split_once('=') else {
                return Err(RuntimeBackendError::invalid_launch(
                    "Docker workload has malformed declared ports",
                ));
            };
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| {
                    RuntimeBackendError::invalid_launch(
                        "Docker workload has malformed declared ports",
                    )
                })?;
            if validate_name(name, "declared port name").is_err() {
                return Err(RuntimeBackendError::invalid_launch(
                    "Docker workload has malformed declared ports",
                ));
            }
            Ok(DockerPort {
                name: name.to_owned(),
                container_port: port,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let names = ports
        .iter()
        .map(|port| &port.name)
        .collect::<std::collections::BTreeSet<_>>();
    let numbers = ports
        .iter()
        .map(|port| port.container_port)
        .collect::<std::collections::BTreeSet<_>>();
    if names.len() != ports.len() || numbers.len() != ports.len() {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker workload has duplicate declared ports",
        ));
    }
    Ok(ports)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.starts_with(|character: char| character.is_ascii_lowercase())
}

fn required_label<'a>(
    container: &'a DockerContainer,
    label: &str,
) -> Result<&'a str, RuntimeBackendError> {
    container
        .labels
        .get(label)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RuntimeBackendError::invalid_launch(format!(
                "Docker workload {} has malformed ownership label {label}",
                container.name
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::*;
    use crate::docker::{DockerAcceleratorTransport, DockerRuntimeConfig};

    fn config() -> DockerRuntimeConfig {
        DockerRuntimeConfig {
            site_id: "site-one".to_owned(),
            execution_target_id: "test-target".to_owned(),
            approved_mount_root: PathBuf::from("/tmp"),
            application_data_root: PathBuf::from("/tmp"),
            workload_cache_root: PathBuf::from("/tmp"),
            accelerator_transport: DockerAcceleratorTransport::LegacyNvidia,
            source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
        }
    }

    fn owned_container() -> DockerContainer {
        DockerContainer {
            id: "owned".to_owned(),
            name: "dotcore_site-one_deployment-one_1".to_owned(),
            name_is_unambiguous: true,
            labels: BTreeMap::from([
                (LABEL_MANAGED.to_owned(), "true".to_owned()),
                (LABEL_SITE.to_owned(), "site-one".to_owned()),
                (LABEL_EXECUTION_TARGET.to_owned(), "test-target".to_owned()),
                (LABEL_SCHEMA.to_owned(), OWNERSHIP_SCHEMA.to_owned()),
                (LABEL_CORRELATION.to_owned(), "deployment-one".to_owned()),
                (
                    LABEL_WORKLOAD.to_owned(),
                    "dotcore_site-one_deployment-one_1".to_owned(),
                ),
                (LABEL_GENERATION.to_owned(), "1".to_owned()),
                (LABEL_PORTS.to_owned(), "http=8080".to_owned()),
                (LABEL_LAUNCH_FINGERPRINT.to_owned(), "a".repeat(64)),
            ]),
        }
    }

    #[test]
    fn ignores_incomplete_legacy_ownership_for_another_site() {
        let mut container = owned_container();
        container
            .labels
            .insert(LABEL_SITE.to_owned(), "other-site".to_owned());
        container.labels.remove(LABEL_SCHEMA);

        assert!(matches!(
            container.classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned())),
            Ok(None)
        ));
    }

    #[test]
    fn ignores_foreign_site_containers_before_runtime_validation() {
        let mut malformed = owned_container();
        malformed
            .labels
            .insert(LABEL_SITE.to_owned(), "other-site".to_owned());
        malformed.name.clear();
        malformed.id.clear();
        malformed.name_is_unambiguous = false;

        for schema in [None, Some(""), Some("unsupported")] {
            let mut container = malformed.clone();
            match schema {
                Some(schema) => {
                    container
                        .labels
                        .insert(LABEL_SCHEMA.to_owned(), schema.to_owned());
                }
                None => {
                    container.labels.remove(LABEL_SCHEMA);
                }
            }
            assert!(matches!(
                container.classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned())),
                Ok(None)
            ));
        }
    }

    #[test]
    fn rejects_equivalent_local_containers_before_discovery() {
        for schema in [None, Some(""), Some("unsupported")] {
            let mut container = owned_container();
            match schema {
                Some(schema) => {
                    container
                        .labels
                        .insert(LABEL_SCHEMA.to_owned(), schema.to_owned());
                }
                None => {
                    container.labels.remove(LABEL_SCHEMA);
                }
            }
            assert!(
                container
                    .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                    .is_err()
            );
        }

        for (name, id, unambiguous) in [
            (String::new(), "owned", true),
            ("ambiguous".to_owned(), "owned", false),
            ("valid".to_owned(), "", true),
        ] {
            let mut container = owned_container();
            container.name = name;
            container.id = id.to_owned();
            container.name_is_unambiguous = unambiguous;
            assert!(
                container
                    .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                    .is_err()
            );
        }
    }

    #[test]
    fn rejects_schema_two_ownership_without_compatibility() {
        let mut container = owned_container();
        container
            .labels
            .insert(LABEL_SCHEMA.to_owned(), "2".to_owned());

        assert!(
            container
                .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                .is_err()
        );
    }

    #[test]
    fn skips_application_group_owned_containers_for_any_site() {
        for site in ["site-one", "other-site"] {
            let mut container = owned_container();
            container
                .labels
                .insert(LABEL_SCHEMA.to_owned(), GROUP_SCHEMA.to_owned());
            container
                .labels
                .insert(LABEL_SITE.to_owned(), site.to_owned());

            let classified = container
                .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                .expect("group-owned containers are not a model runtime error");
            assert!(classified.is_none());
        }
    }

    #[test]
    fn rejects_duplicate_endpoint_names_and_port_numbers() {
        for ports in ["http=8080,http=8081", "http=8080,metrics=8080"] {
            let mut container = owned_container();
            container
                .labels
                .insert(LABEL_PORTS.to_owned(), ports.to_owned());

            assert!(
                container
                    .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                    .is_err()
            );
        }
    }

    #[test]
    fn exact_target_classification_never_adopts_another_targets_workload() {
        let mut other = owned_container();
        other
            .labels
            .insert(LABEL_EXECUTION_TARGET.to_owned(), "other-target".to_owned());

        assert!(
            other
                .classify_owned(&config(), RuntimeBackendId("runtime/docker".to_owned()))
                .expect("ownership classification")
                .is_none()
        );
    }
}
