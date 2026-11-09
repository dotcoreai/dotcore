//! Local Docker daemon negotiation and read-only capability inspection.

use anyhow::Context;
use bollard::models::SystemInfo;
use dotcore_domain::{ExecutionPlatform, ExecutionPlatformError};

use crate::docker::client::{LocalDockerClient, WORKLOAD_LOG_DRIVER, bounded_operation};

const DAEMON_CAPABILITY_PROBE: &str = "Docker daemon capability probe";

/// Effective platform and CDI facts reported by one Docker daemon.
pub(in crate::docker) struct DockerDaemonCapability {
    pub(in crate::docker) execution_platform: ExecutionPlatform,
    pub(super) cdi_spec_dirs: Vec<String>,
}

/// Docker daemon capability that every workload container needs and the daemon lacks.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::docker) enum DaemonCapabilityError {
    #[error("Docker daemon lacks the {driver:?} log driver that workload containers use")]
    MissingLogDriver { driver: &'static str },
}

/// Connects to the local Docker daemon, negotiates its API, and reads capability facts.
pub(super) async fn probe_local_daemon() -> anyhow::Result<DockerDaemonCapability> {
    let docker = LocalDockerClient::connect()
        .map_err(anyhow::Error::new)
        .context("connect to local Docker daemon")?;
    let docker = docker
        .negotiate_version()
        .await
        .map_err(anyhow::Error::new)
        .context("negotiate Docker Engine API version")?;
    probe_connected_daemon(&docker).await
}

pub(in crate::docker) async fn probe_connected_daemon(
    docker: &LocalDockerClient,
) -> anyhow::Result<DockerDaemonCapability> {
    let information = bounded_operation(DAEMON_CAPABILITY_PROBE, docker.raw().info())
        .await
        .map_err(anyhow::Error::new)
        .context("query Docker daemon capability")?;
    let execution_platform = execution_platform_from_information(&information)
        .map_err(anyhow::Error::new)
        .context("normalize Docker daemon execution platform")?;
    require_workload_log_driver(&information)
        .map_err(anyhow::Error::new)
        .context("check Docker daemon log drivers")?;
    Ok(DockerDaemonCapability {
        execution_platform,
        cdi_spec_dirs: information.cdi_spec_dirs.unwrap_or_default(),
    })
}

pub(in crate::docker) fn execution_platform_from_information(
    information: &SystemInfo,
) -> Result<ExecutionPlatform, ExecutionPlatformError> {
    ExecutionPlatform::normalize(
        information.os_type.as_deref().unwrap_or_default(),
        information.architecture.as_deref().unwrap_or_default(),
    )
}

fn require_workload_log_driver(information: &SystemInfo) -> Result<(), DaemonCapabilityError> {
    let available = information
        .plugins
        .as_ref()
        .and_then(|plugins| plugins.log.as_ref())
        .is_some_and(|drivers| drivers.iter().any(|driver| driver == WORKLOAD_LOG_DRIVER));
    if available {
        Ok(())
    } else {
        Err(DaemonCapabilityError::MissingLogDriver {
            driver: WORKLOAD_LOG_DRIVER,
        })
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{PluginsInfo, SystemInfo};
    use dotcore_domain::ExecutionPlatform;

    use super::{
        DaemonCapabilityError, execution_platform_from_information, require_workload_log_driver,
    };

    #[test]
    fn daemon_platform_uses_destination_os_and_normalizes_supported_architectures() {
        for (architecture, expected) in [
            ("x86_64", ExecutionPlatform::linux_amd64()),
            ("amd64", ExecutionPlatform::linux_amd64()),
            ("aarch64", ExecutionPlatform::linux_arm64()),
            ("arm64", ExecutionPlatform::linux_arm64()),
        ] {
            let information = SystemInfo {
                operating_system: Some("NixOS 26.05 (Yarara)".to_owned()),
                os_type: Some("linux".to_owned()),
                architecture: Some(architecture.to_owned()),
                ..Default::default()
            };

            assert_eq!(
                execution_platform_from_information(&information)
                    .expect("supported Docker destination platform"),
                expected
            );
        }
    }

    #[test]
    fn daemon_platform_rejects_unknown_destination_components_with_typed_errors() {
        let unknown_os = SystemInfo {
            os_type: Some("windows".to_owned()),
            architecture: Some("amd64".to_owned()),
            ..Default::default()
        };
        let unknown_architecture = SystemInfo {
            os_type: Some("linux".to_owned()),
            architecture: Some("riscv64".to_owned()),
            ..Default::default()
        };

        assert_eq!(
            execution_platform_from_information(&unknown_os),
            Err(
                dotcore_domain::ExecutionPlatformError::UnsupportedOperatingSystem {
                    observed: "windows".to_owned(),
                }
            )
        );
        assert_eq!(
            execution_platform_from_information(&unknown_architecture),
            Err(
                dotcore_domain::ExecutionPlatformError::UnsupportedArchitecture {
                    observed: "riscv64".to_owned(),
                }
            )
        );
    }

    #[test]
    fn daemon_without_the_local_log_driver_is_rejected() {
        let with_drivers = |drivers: Option<Vec<&str>>| SystemInfo {
            plugins: Some(PluginsInfo {
                log: drivers.map(|drivers| drivers.into_iter().map(str::to_owned).collect()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let missing = Err(DaemonCapabilityError::MissingLogDriver { driver: "local" });

        assert_eq!(
            require_workload_log_driver(&with_drivers(Some(vec![
                "journald",
                "json-file",
                "local"
            ]))),
            Ok(())
        );
        assert_eq!(
            require_workload_log_driver(&with_drivers(Some(vec!["journald", "json-file"]))),
            missing
        );
        assert_eq!(require_workload_log_driver(&with_drivers(None)), missing);
        assert_eq!(require_workload_log_driver(&SystemInfo::default()), missing);
    }
}
