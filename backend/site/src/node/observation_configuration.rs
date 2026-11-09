//! Derives the immutable node-local observation configuration of one SiteNode: installed in a
//! node folder, or derived in process for the manager's own node.
//!
//! The projection contains only infrastructure facts required by node-agent probes. Site topology,
//! workload intent, and manager authority remain outside the node root.

use dotcore_domain::{AcceleratorDeviceSelector, ExecutionPlatform, valid_id};
use dotcore_proto::{
    dotcore::{
        node::v1::{
            DockerRuntimeObservationConfiguration, NativeRuntimeObservationConfiguration,
            NodeExecutionTarget, NodeObservationConfiguration, node_execution_target,
        },
        observation::v1::RuntimeKind,
        site::v1::{
            DockerAcceleratorTransport, NativeWorkloadLifetime, Site, application_runtime_binding,
            execution_target, model_runtime_binding,
        },
    },
    google::protobuf::Empty,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::validation::hardware_refresh_interval;

pub(crate) const NODE_OBSERVATION_CONFIGURATION_SCHEMA_VERSION: u32 = 1;
const DIGEST_DOMAIN: &[u8] = b"dotcore.worker-observation.v1\0";
const MIN_INTERVAL_SECONDS: u32 = 1;
const MAX_INTERVAL_SECONDS: u32 = 3_600;

/// Lowercase SHA-256 identity of one canonical node observation configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeObservationConfigurationDigest(String);

impl NodeObservationConfigurationDigest {
    /// Computes the digest after verifying canonical field values and ordering.
    pub fn for_configuration(
        configuration: &NodeObservationConfiguration,
    ) -> Result<Self, NodeObservationConfigurationError> {
        validate_node_observation_configuration(configuration)?;
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_DOMAIN);
        hasher.update(configuration.encode_to_vec());
        Ok(Self(encode_hex(&hasher.finalize())))
    }

    /// Parses one complete canonical lowercase SHA-256 digest.
    pub fn parse(value: impl Into<String>) -> Result<Self, NodeObservationConfigurationError> {
        let digest = Self(value.into());
        if digest.0.len() != 64
            || !digest
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(NodeObservationConfigurationError::Digest);
        }
        Ok(digest)
    }

    /// Returns the canonical lowercase hexadecimal digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Derives the complete local observation configuration for one declared SiteNode; a node with
/// no execution targets still observes its hardware.
pub fn node_observation_configuration(
    site: &Site,
    worker_node_id: &str,
) -> Result<NodeObservationConfiguration, NodeObservationConfigurationError> {
    if !site.nodes.iter().any(|node| node.node_id == worker_node_id) {
        return Err(NodeObservationConfigurationError::UnknownNode(
            worker_node_id.to_owned(),
        ));
    }
    let mut execution_targets = Vec::new();

    for target in site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == worker_node_id)
    {
        let (kind, implementation) = match target.runtime_binding.as_ref().ok_or_else(|| {
            NodeObservationConfigurationError::MissingImplementation(
                target.execution_target_id.clone(),
            )
        })? {
            execution_target::RuntimeBinding::ModelRuntime(binding) => {
                let implementation = match binding.adapter.as_ref().ok_or_else(|| {
                    NodeObservationConfigurationError::MissingImplementation(
                        target.execution_target_id.clone(),
                    )
                })? {
                    model_runtime_binding::Adapter::SimulatedRuntime(_) => {
                        node_execution_target::Implementation::Simulated(Empty {})
                    }
                    model_runtime_binding::Adapter::NativeRuntime(configuration) => {
                        node_execution_target::Implementation::Native(
                            NativeRuntimeObservationConfiguration {
                                workload_lifetime: configuration.workload_lifetime,
                            },
                        )
                    }
                    model_runtime_binding::Adapter::DockerRuntime(configuration) => {
                        let mut accelerator_device_ids = target.accelerator_device_ids.clone();
                        accelerator_device_ids.sort();
                        node_execution_target::Implementation::Docker(
                            DockerRuntimeObservationConfiguration {
                                accelerator_transport: configuration.accelerator_transport,
                                accelerator_device_ids,
                            },
                        )
                    }
                };
                (RuntimeKind::Model, implementation)
            }
            execution_target::RuntimeBinding::ApplicationRuntime(binding) => {
                let implementation = match binding.adapter.as_ref().ok_or_else(|| {
                    NodeObservationConfigurationError::MissingImplementation(
                        target.execution_target_id.clone(),
                    )
                })? {
                    application_runtime_binding::Adapter::SimulatedRuntime(_) => {
                        node_execution_target::Implementation::Simulated(Empty {})
                    }
                    application_runtime_binding::Adapter::DockerRuntime(_) => {
                        node_execution_target::Implementation::Docker(
                            application_docker_observation(&target.accelerator_device_ids),
                        )
                    }
                };
                (RuntimeKind::Application, implementation)
            }
        };
        execution_targets.push(NodeExecutionTarget {
            execution_target_id: target.execution_target_id.clone(),
            kind: kind as i32,
            platform: target.platform.clone(),
            implementation: Some(implementation),
        });
    }
    execution_targets
        .sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));

    let configuration = NodeObservationConfiguration {
        schema_version: NODE_OBSERVATION_CONFIGURATION_SCHEMA_VERSION,
        interval_seconds: hardware_refresh_interval(site).as_secs() as u32,
        execution_targets,
    };
    validate_node_observation_configuration(&configuration)?;
    Ok(configuration)
}

/// Validates the canonical configuration shape stored in one node folder.
pub(crate) fn validate_node_observation_configuration(
    configuration: &NodeObservationConfiguration,
) -> Result<(), NodeObservationConfigurationError> {
    if configuration.schema_version != NODE_OBSERVATION_CONFIGURATION_SCHEMA_VERSION {
        return Err(NodeObservationConfigurationError::SchemaVersion(
            configuration.schema_version,
        ));
    }
    if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&configuration.interval_seconds) {
        return Err(NodeObservationConfigurationError::Interval(
            configuration.interval_seconds,
        ));
    }
    let mut previous_target_id: Option<&str> = None;
    for target in &configuration.execution_targets {
        if !valid_id(&target.execution_target_id) {
            return Err(NodeObservationConfigurationError::TargetId(
                target.execution_target_id.clone(),
            ));
        }
        if previous_target_id
            .is_some_and(|previous| previous >= target.execution_target_id.as_str())
        {
            return Err(NodeObservationConfigurationError::TargetOrdering);
        }
        previous_target_id = Some(&target.execution_target_id);
        let kind = RuntimeKind::try_from(target.kind)
            .ok()
            .filter(|kind| *kind != RuntimeKind::Unspecified)
            .ok_or_else(|| {
                NodeObservationConfigurationError::RuntimeKind(target.execution_target_id.clone())
            })?;
        let platform = target.platform.as_ref().ok_or_else(|| {
            NodeObservationConfigurationError::Platform(target.execution_target_id.clone())
        })?;
        let normalized = ExecutionPlatform::try_from(platform).map_err(|_| {
            NodeObservationConfigurationError::Platform(target.execution_target_id.clone())
        })?;
        if normalized.to_oci_platform() != *platform {
            return Err(NodeObservationConfigurationError::Platform(
                target.execution_target_id.clone(),
            ));
        }
        match target.implementation.as_ref().ok_or_else(|| {
            NodeObservationConfigurationError::MissingImplementation(
                target.execution_target_id.clone(),
            )
        })? {
            node_execution_target::Implementation::Simulated(_) => {}
            node_execution_target::Implementation::Native(configuration) => {
                if kind != RuntimeKind::Model
                    || NativeWorkloadLifetime::try_from(configuration.workload_lifetime).is_err()
                {
                    return Err(NodeObservationConfigurationError::NativeConfiguration(
                        target.execution_target_id.clone(),
                    ));
                }
            }
            node_execution_target::Implementation::Docker(configuration) => {
                if configuration
                    .accelerator_device_ids
                    .windows(2)
                    .any(|ids| ids[0] >= ids[1])
                    || configuration
                        .accelerator_device_ids
                        .iter()
                        .any(|id| id.trim().is_empty())
                {
                    return Err(NodeObservationConfigurationError::DeviceOrdering(
                        target.execution_target_id.clone(),
                    ));
                }
                let invalid_configuration = match kind {
                    // Application targets select devices only through CDI, and only when they
                    // hold some, so a deviceless target needs no CDI on its node.
                    RuntimeKind::Application => {
                        configuration.accelerator_transport
                            != application_docker_observation(&configuration.accelerator_device_ids)
                                .accelerator_transport
                            || AcceleratorDeviceSelector::for_docker_transport(
                                DockerAcceleratorTransport::Cdi as i32,
                                &configuration.accelerator_device_ids,
                            )
                            .is_err()
                    }
                    RuntimeKind::Model => AcceleratorDeviceSelector::for_docker_transport(
                        configuration.accelerator_transport,
                        &configuration.accelerator_device_ids,
                    )
                    .is_err(),
                    RuntimeKind::Unspecified => unreachable!("validated runtime kind"),
                };
                if invalid_configuration {
                    return Err(NodeObservationConfigurationError::DockerConfiguration(
                        target.execution_target_id.clone(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Failure while deriving or validating node-local observation configuration.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NodeObservationConfigurationError {
    #[error("node observation configuration digest must be 64 lowercase hexadecimal characters")]
    Digest,

    #[error("node observation configuration schema version {0} is unsupported")]
    SchemaVersion(u32),

    #[error("node observation interval {0} seconds is outside 1..=3600")]
    Interval(u32),

    #[error("node observation configuration refers to unknown node {0:?}")]
    UnknownNode(String),

    #[error("node {0:?} has no application execution target")]
    MissingApplicationTarget(String),

    #[error("node {0:?} has no application runtime")]
    MissingApplicationRuntime(String),

    #[error("execution target {0:?} has no runtime implementation")]
    MissingImplementation(String),

    #[error("execution target ID {0:?} is invalid")]
    TargetId(String),

    #[error("node execution targets must be sorted by unique execution-target ID")]
    TargetOrdering,

    #[error("execution target {0:?} has an invalid runtime kind")]
    RuntimeKind(String),

    #[error("execution target {0:?} has an invalid or non-canonical platform")]
    Platform(String),

    #[error("execution target {0:?} has invalid Docker observation prerequisites")]
    DockerConfiguration(String),

    #[error("execution target {0:?} has invalid native runtime prerequisites")]
    NativeConfiguration(String),

    #[error("execution target {0:?} has invalid, duplicate, or unsorted accelerator devices")]
    DeviceOrdering(String),
}

/// Returns the Docker observation prerequisites of an application target holding `device_ids`:
/// CDI with the sorted devices when it holds any, else none.
fn application_docker_observation(device_ids: &[String]) -> DockerRuntimeObservationConfiguration {
    if device_ids.is_empty() {
        return DockerRuntimeObservationConfiguration::default();
    }
    let mut accelerator_device_ids = device_ids.to_vec();
    accelerator_device_ids.sort();
    DockerRuntimeObservationConfiguration {
        accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
        accelerator_device_ids,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use dotcore_proto::{
        dotcore::{
            catalog::v1::OciPlatform,
            node::v1::{
                DockerRuntimeObservationConfiguration, NodeExecutionTarget,
                NodeObservationConfiguration, node_execution_target,
            },
            observation::v1::RuntimeKind,
            site::v1::DockerAcceleratorTransport,
        },
        google::protobuf::Empty,
    };

    use super::*;

    #[test]
    fn canonical_digest_has_one_fixed_cross_component_value() {
        let configuration = NodeObservationConfiguration {
            schema_version: 1,
            interval_seconds: 5,
            execution_targets: vec![
                NodeExecutionTarget {
                    execution_target_id: "application-target".to_owned(),
                    kind: RuntimeKind::Application as i32,
                    platform: Some(platform()),
                    implementation: Some(node_execution_target::Implementation::Simulated(
                        Empty {},
                    )),
                },
                NodeExecutionTarget {
                    execution_target_id: "model-target".to_owned(),
                    kind: RuntimeKind::Model as i32,
                    platform: Some(platform()),
                    implementation: Some(node_execution_target::Implementation::Docker(
                        DockerRuntimeObservationConfiguration {
                            accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                            accelerator_device_ids: vec![
                                "nvidia.com/gpu=GPU-1".to_owned(),
                                "nvidia.com/gpu=GPU-2".to_owned(),
                            ],
                        },
                    )),
                },
            ],
        };

        assert_eq!(
            NodeObservationConfigurationDigest::for_configuration(&configuration)
                .expect("canonical digest")
                .as_str(),
            "6fec233b9cf043af5fdfa517e1d315b29c492ac7772047bb8ba35e274f10dacd"
        );
    }

    #[test]
    fn validation_rejects_reordered_targets_and_devices() {
        let mut configuration = NodeObservationConfiguration {
            schema_version: 1,
            interval_seconds: 5,
            execution_targets: vec![NodeExecutionTarget {
                execution_target_id: "model-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                platform: Some(platform()),
                implementation: Some(node_execution_target::Implementation::Docker(
                    DockerRuntimeObservationConfiguration {
                        accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                        accelerator_device_ids: vec![
                            "nvidia.com/gpu=GPU-2".to_owned(),
                            "nvidia.com/gpu=GPU-1".to_owned(),
                        ],
                    },
                )),
            }],
        };
        assert_eq!(
            validate_node_observation_configuration(&configuration),
            Err(NodeObservationConfigurationError::DeviceOrdering(
                "model-target".to_owned()
            ))
        );

        configuration.execution_targets = vec![
            NodeExecutionTarget {
                execution_target_id: "z-target".to_owned(),
                implementation: Some(node_execution_target::Implementation::Simulated(Empty {})),
                ..configuration.execution_targets[0].clone()
            },
            NodeExecutionTarget {
                execution_target_id: "a-target".to_owned(),
                implementation: Some(node_execution_target::Implementation::Simulated(Empty {})),
                ..configuration.execution_targets[0].clone()
            },
        ];
        assert_eq!(
            validate_node_observation_configuration(&configuration),
            Err(NodeObservationConfigurationError::TargetOrdering)
        );
    }

    #[test]
    fn application_docker_targets_observe_cdi_only_with_devices() {
        let application_target = |devices: Vec<String>| NodeExecutionTarget {
            execution_target_id: "application-target".to_owned(),
            kind: RuntimeKind::Application as i32,
            platform: Some(platform()),
            implementation: Some(node_execution_target::Implementation::Docker(
                application_docker_observation(&devices),
            )),
        };
        let configuration = |target| NodeObservationConfiguration {
            schema_version: 1,
            interval_seconds: 5,
            execution_targets: vec![target],
        };

        assert_eq!(
            application_docker_observation(&[]),
            DockerRuntimeObservationConfiguration::default()
        );
        assert_eq!(
            application_docker_observation(&[
                "nvidia.com/gpu=1".to_owned(),
                "nvidia.com/gpu=0".to_owned(),
            ]),
            DockerRuntimeObservationConfiguration {
                accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                accelerator_device_ids: vec![
                    "nvidia.com/gpu=0".to_owned(),
                    "nvidia.com/gpu=1".to_owned(),
                ],
            }
        );
        for devices in [Vec::new(), vec!["nvidia.com/gpu=0".to_owned()]] {
            validate_node_observation_configuration(&configuration(application_target(devices)))
                .expect("canonical application target");
        }

        for invalid in [
            DockerRuntimeObservationConfiguration {
                accelerator_transport: DockerAcceleratorTransport::Unspecified as i32,
                accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
            },
            DockerRuntimeObservationConfiguration {
                accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                accelerator_device_ids: Vec::new(),
            },
            DockerRuntimeObservationConfiguration {
                accelerator_transport: DockerAcceleratorTransport::LegacyNvidia as i32,
                accelerator_device_ids: vec!["0".to_owned()],
            },
        ] {
            let mut target = application_target(Vec::new());
            target.implementation = Some(node_execution_target::Implementation::Docker(invalid));
            assert_eq!(
                validate_node_observation_configuration(&configuration(target)),
                Err(NodeObservationConfigurationError::DockerConfiguration(
                    "application-target".to_owned()
                ))
            );
        }
    }

    #[test]
    fn native_binding_maps_to_native_implementation_with_lifetime() {
        let site = Site {
            nodes: vec![dotcore_proto::dotcore::site::v1::SiteNode {
                node_id: "mac-node".to_owned(),
                ..Default::default()
            }],
            execution_targets: vec![dotcore_proto::dotcore::site::v1::ExecutionTarget {
                execution_target_id: "native-target".to_owned(),
                display_name: "native-target".to_owned(),
                site_node_id: "mac-node".to_owned(),
                platform: Some(OciPlatform {
                    operating_system: "darwin".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                }),
                accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
                runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                    dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                        adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                            dotcore_proto::dotcore::site::v1::NativeRuntimeAdapter {
                                workload_lifetime: NativeWorkloadLifetime::Ephemeral as i32,
                            },
                        )),
                    },
                )),
            }],
            ..Default::default()
        };

        let configuration =
            node_observation_configuration(&site, "mac-node").expect("native configuration");

        assert_eq!(
            configuration.execution_targets,
            vec![NodeExecutionTarget {
                execution_target_id: "native-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                platform: Some(OciPlatform {
                    operating_system: "darwin".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                }),
                implementation: Some(node_execution_target::Implementation::Native(
                    NativeRuntimeObservationConfiguration {
                        workload_lifetime: NativeWorkloadLifetime::Ephemeral as i32,
                    },
                )),
            }]
        );

        // The provisioned-worker topology guard compares these configurations, so a lifetime
        // change is a topology change.
        let mut persistent = site.clone();
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
            persistent.execution_targets[0].runtime_binding.as_mut()
        else {
            unreachable!("model binding");
        };
        binding.adapter = Some(model_runtime_binding::Adapter::NativeRuntime(
            dotcore_proto::dotcore::site::v1::NativeRuntimeAdapter {
                workload_lifetime: NativeWorkloadLifetime::Persistent as i32,
            },
        ));
        assert_ne!(
            node_observation_configuration(&persistent, "mac-node").expect("persistent"),
            configuration
        );
    }

    fn platform() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }
}
