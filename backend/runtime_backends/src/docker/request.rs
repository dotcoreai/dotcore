//! Translates validated workload launch intent into the Docker create requests that the
//! local Docker client sends to the Docker Engine.
//!
//! An individual workload is prepared as a complete container request (name, image,
//! entrypoint, ports, mounts, device requests, environment) plus the observed workload
//! identity; the container name follows `dotcore_<site>_<owner>_<generation>` and the
//! ownership labels are stamped on creation. The same module provides the
//! workload-group request equivalents for networks and group containers.

use std::{collections::BTreeMap, net::SocketAddrV4, path::PathBuf};

use dotcore_domain::SiteNodeId;
use dotcore_runtime::{
    ObservedWorkload, RuntimeBackendError, RuntimeBackendId, WorkloadArtifactBinding,
    WorkloadExecutable, WorkloadExecutableArtifact, WorkloadId, WorkloadLaunch, WorkloadPort,
    WorkloadTokenResolver,
};

use super::{
    adapter::{DockerAcceleratorTransport, DockerRuntimeConfig},
    ownership::{
        LABEL_CORRELATION, LABEL_EXECUTION_TARGET, LABEL_GENERATION, LABEL_LAUNCH_FINGERPRINT,
        LABEL_MANAGED, LABEL_PORTS, LABEL_SCHEMA, LABEL_SITE, LABEL_WORKLOAD, OWNERSHIP_SCHEMA,
    },
    storage::storage_path,
    validation::validate_launch,
};

#[derive(Clone)]
pub(super) struct PreparedWorkload {
    pub(super) request: DockerCreateRequest,
    pub(super) identity: ObservedWorkload,
    pub(super) managed_node_id: SiteNodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerCreateRequest {
    pub(super) name: String,
    pub(super) image: String,
    pub(super) entrypoint: Vec<String>,
    pub(super) argv: Vec<String>,
    pub(super) labels: BTreeMap<String, String>,
    pub(super) ports: Vec<DockerPort>,
    pub(super) mounts: Vec<DockerMount>,
    pub(super) device_requests: Vec<DockerDeviceRequest>,
    pub(super) environment: Vec<String>,
    pub(super) ipc_mode: DockerIpcMode,
}

/// Docker Engine IPC value derived from owner-neutral workload intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DockerIpcMode {
    Private,

    Host,
}

impl DockerIpcMode {
    pub(super) const fn as_docker_value(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Host => "host",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerPort {
    pub(super) name: String,
    pub(super) container_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerMount {
    pub(super) source: PathBuf,
    pub(super) target: String,
    pub(super) read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerDeviceRequest {
    pub(super) driver: String,
    pub(super) device_ids: Vec<String>,
    pub(super) capabilities: Option<Vec<Vec<String>>>,
}

/// Builds the one device request that passes a launch's accelerator devices to a container,
/// in the form the backend's accelerator transport addresses them.
pub(super) fn accelerator_device_requests(
    transport: DockerAcceleratorTransport,
    device_ids: &[String],
) -> Vec<DockerDeviceRequest> {
    if device_ids.is_empty() {
        return Vec::new();
    }
    let request = match transport {
        DockerAcceleratorTransport::Cdi => DockerDeviceRequest {
            driver: "cdi".to_owned(),
            device_ids: device_ids.to_vec(),
            capabilities: None,
        },
        DockerAcceleratorTransport::LegacyNvidia => DockerDeviceRequest {
            driver: "nvidia".to_owned(),
            device_ids: device_ids.to_vec(),
            capabilities: Some(vec![vec!["gpu".to_owned()]]),
        },
    };
    vec![request]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerGroupNetworkRequest {
    pub(super) name: String,
    pub(super) labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerGroupContainerRequest {
    pub(super) name: String,
    pub(super) image: String,
    pub(super) entrypoint: Vec<String>,
    pub(super) argv: Vec<String>,
    pub(super) labels: BTreeMap<String, String>,
    pub(super) ports: Vec<DockerGroupPort>,
    pub(super) mounts: Vec<DockerMount>,
    pub(super) device_requests: Vec<DockerDeviceRequest>,
    pub(super) environment: Vec<String>,
    pub(super) network_name: String,
    pub(super) aliases: Vec<String>,
    /// Host entries as `name:address`, resolved by the hosting node when the container is
    /// created.
    pub(super) extra_hosts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DockerGroupPort {
    pub(super) name: String,
    pub(super) container_port: u16,

    /// Host address and port the port is published on, for the group's one published port.
    pub(super) publication: Option<SocketAddrV4>,
}

impl PreparedWorkload {
    /// Prepares a Docker workload after all launch validators succeed.
    pub(super) fn from_launch(
        workload: &WorkloadLaunch,
        config: &DockerRuntimeConfig,
        backend_id: RuntimeBackendId,
    ) -> Result<Self, RuntimeBackendError> {
        let command = validate_launch(workload, config)?;
        let spec = &workload.spec;
        let WorkloadExecutableArtifact::OciImage(image) = &spec.executable.artifact else {
            return Err(RuntimeBackendError::UnsupportedExecutableArtifact {
                backend_id: super::BACKEND_ID,
                kind: spec.executable.artifact.kind(),
            });
        };

        let workload_id = format!(
            "dotcore_{}_{}_{}",
            config.site_id,
            workload.correlation.owner_id,
            workload.correlation.reconcile_generation
        );
        let labels = BTreeMap::from([
            (LABEL_SITE.to_owned(), config.site_id.clone()),
            (
                LABEL_EXECUTION_TARGET.to_owned(),
                config.execution_target_id.clone(),
            ),
            (LABEL_MANAGED.to_owned(), "true".to_owned()),
            (
                LABEL_CORRELATION.to_owned(),
                workload.correlation.owner_id.clone(),
            ),
            (
                LABEL_GENERATION.to_owned(),
                workload.correlation.reconcile_generation.to_string(),
            ),
            (LABEL_SCHEMA.to_owned(), OWNERSHIP_SCHEMA.to_owned()),
            (LABEL_WORKLOAD.to_owned(), workload_id.clone()),
            (
                LABEL_LAUNCH_FINGERPRINT.to_owned(),
                workload.exact_intent_fingerprint(),
            ),
            (
                LABEL_PORTS.to_owned(),
                spec.ports
                    .iter()
                    .map(|port| format!("{}={}", port.name, port.container_port))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ]);
        Ok(Self {
            request: DockerCreateRequest {
                name: workload_id.clone(),
                image: image.clone(),
                entrypoint: vec![command.executable],
                argv: command.arguments,
                labels,
                ports: spec
                    .ports
                    .iter()
                    .map(|port| DockerPort {
                        name: port.name.clone(),
                        container_port: port.container_port,
                    })
                    .collect(),
                mounts: spec
                    .artifacts
                    .iter()
                    .map(|mount| {
                        Ok(DockerMount {
                            source: mount.source.clone(),
                            target: mount.destination.clone(),
                            read_only: mount.read_only,
                        })
                    })
                    .chain(spec.storage.iter().map(|storage| {
                        Ok(DockerMount {
                            source: storage_path(config, &storage.into())?,
                            target: storage.destination.clone(),
                            read_only: storage.read_only,
                        })
                    }))
                    .collect::<Result<_, RuntimeBackendError>>()?,
                device_requests: accelerator_device_requests(
                    config.accelerator_transport,
                    &spec.accelerator_device_ids,
                ),
                environment: command
                    .environment
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
                ipc_mode: match spec.ipc_mode {
                    dotcore_runtime::WorkloadIpcMode::Isolated => DockerIpcMode::Private,
                    dotcore_runtime::WorkloadIpcMode::HostShared => DockerIpcMode::Host,
                },
            },
            identity: ObservedWorkload {
                workload_id: WorkloadId(workload_id),
                correlation: workload.correlation.clone(),
                backend_id,
                loopback_endpoints: Vec::new(),
                services: Vec::new(),
                host_process: None,
            },
            managed_node_id: workload.execution_target.managed_node_id.clone(),
        })
    }
}

/// Resolves workload tokens to the values visible inside the container.
///
/// The executable and artifacts use their in-image and mount destination paths, and every
/// service binds all container interfaces on its fixed container port, which the adapter
/// publishes on an ephemeral loopback host port.
pub struct ContainerTokens;

impl WorkloadTokenResolver for ContainerTokens {
    fn executable(&self, executable: &WorkloadExecutable) -> String {
        executable.path.clone()
    }

    fn artifact(&self, binding: &WorkloadArtifactBinding) -> String {
        binding.destination.clone()
    }

    fn service_host(&self, _: &WorkloadPort) -> String {
        "0.0.0.0".to_owned()
    }

    fn service_port(&self, port: &WorkloadPort) -> u16 {
        port.container_port
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dotcore_runtime::{
        ExecutionTarget, MAX_WORKLOAD_ID_BYTES, WorkloadCorrelation, WorkloadSpec,
        WorkloadStorageAttachment, WorkloadStorageKind, WorkloadValue,
    };

    use super::*;
    use crate::docker::{DockerAcceleratorTransport, DockerRuntimeConfig};

    fn config() -> DockerRuntimeConfig {
        DockerRuntimeConfig {
            site_id: "site-one".to_owned(),
            execution_target_id: "test-target".to_owned(),
            approved_mount_root: dotcore_domain::test_support::canonical_temp_dir(),
            application_data_root: dotcore_domain::test_support::canonical_temp_dir(),
            workload_cache_root: dotcore_domain::test_support::canonical_temp_dir(),
            accelerator_transport: DockerAcceleratorTransport::LegacyNvidia,
            source_stream_permits: crate::docker::configuration::DockerLogSourcePermits::new(),
        }
    }

    fn workload() -> WorkloadLaunch {
        WorkloadLaunch {
            correlation: WorkloadCorrelation {
                owner_id: "deployment-one".to_owned(),
                reconcile_generation: 1,
            },
            execution_target: ExecutionTarget {
                managed_node_id: SiteNodeId::for_test("node-one"),
                accelerator_device_ids: vec!["0".to_owned()],
            },
            spec: WorkloadSpec {
                executable: WorkloadExecutable {
                    artifact: WorkloadExecutableArtifact::OciImage("example.test/workload@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                    path: "/bin/server".to_owned(),
                },
                required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
                arguments: vec![WorkloadValue::literal("--serve")],
                environment: BTreeMap::from([("EXPLICIT_VALUE".to_owned(), WorkloadValue::literal("value"))]),
                artifacts: vec![WorkloadArtifactBinding {
                    name: "data".to_owned(),
                    source: dotcore_domain::test_support::canonical_temp_dir(),
                    destination: "/var/lib/workload".to_owned(),
                    read_only: true,
                }],
                storage: Vec::new(),
                ports: vec![WorkloadPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                accelerator_device_ids: vec!["0".to_owned()],
                required_accelerator_device_count: 1,
                ipc_mode: dotcore_runtime::WorkloadIpcMode::Isolated,
                expected_memory: Default::default(),
            },
        }
    }

    #[test]
    fn translates_storage_attachments_to_mounts_at_the_storage_layout() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("storage roots");
        let root = directory.path().canonicalize().expect("canonical root");
        let config = DockerRuntimeConfig {
            workload_cache_root: root.join("workload-cache/v2"),
            application_data_root: root.join("applications-data"),
            ..config()
        };
        std::fs::create_dir_all(&config.workload_cache_root).expect("cache root");
        std::fs::create_dir_all(&config.application_data_root).expect("data root");
        let mut launch = workload();
        launch.spec.storage = vec![
            storage(
                "engine-cache",
                "/var/cache/engine",
                WorkloadStorageKind::Cache,
                "deployment",
                "deployment-one",
            ),
            storage(
                "models",
                "/var/lib/models",
                WorkloadStorageKind::Data,
                "application",
                "open-webui",
            ),
        ];
        let mut other_owner = launch.clone();
        other_owner.correlation.owner_id = "deployment-two".to_owned();
        let mounts = |launch: &WorkloadLaunch| {
            PreparedWorkload::from_launch(
                launch,
                &config,
                RuntimeBackendId("runtime/docker".to_owned()),
            )
            .expect("OCI workload with storage")
            .request
            .mounts
        };

        assert_eq!(
            mounts(&launch),
            vec![
                DockerMount {
                    source: dotcore_domain::test_support::canonical_temp_dir(),
                    target: "/var/lib/workload".to_owned(),
                    read_only: true,
                },
                DockerMount {
                    source: root
                        .join("workload-cache/v2/site-one/deployment/deployment-one/engine-cache"),
                    target: "/var/cache/engine".to_owned(),
                    read_only: false,
                },
                DockerMount {
                    source: root.join("applications-data/application/open-webui/models"),
                    target: "/var/lib/models".to_owned(),
                    read_only: true,
                },
            ]
        );
        // The folder follows the attachment, not the workload that names it.
        assert_eq!(mounts(&other_owner), mounts(&launch));
        for (part, change) in [
            (
                "kind",
                (|s: &mut WorkloadStorageAttachment| s.kind = WorkloadStorageKind::Data)
                    as fn(&mut WorkloadStorageAttachment),
            ),
            ("namespace", |s| s.namespace = "engine-artifact".to_owned()),
            ("key", |s| s.key = "deployment-two".to_owned()),
            ("name", |s| s.name = "other-cache".to_owned()),
        ] {
            let mut changed = launch.clone();
            change(&mut changed.spec.storage[0]);
            assert_ne!(
                mounts(&changed)[1].source,
                mounts(&launch)[1].source,
                "{part}"
            );
        }
    }

    fn storage(
        name: &str,
        destination: &str,
        kind: WorkloadStorageKind,
        namespace: &str,
        key: &str,
    ) -> WorkloadStorageAttachment {
        WorkloadStorageAttachment {
            name: name.to_owned(),
            destination: destination.to_owned(),
            kind,
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            read_only: kind == WorkloadStorageKind::Data,
            expires_when_unused: false,
        }
    }

    #[test]
    fn translates_owner_neutral_oci_intent_to_safe_docker_request() {
        let prepared = PreparedWorkload::from_launch(
            &workload(),
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("valid OCI workload");

        assert_eq!(prepared.managed_node_id, SiteNodeId::for_test("node-one"));
        let request = prepared.request;

        assert_eq!(request.name, "dotcore_site-one_deployment-one_1");
        assert_eq!(request.entrypoint, vec!["/bin/server"]);
        assert_eq!(request.argv, vec!["--serve"]);
        assert_eq!(request.environment, vec!["EXPLICIT_VALUE=value"]);
        assert_eq!(request.ipc_mode, DockerIpcMode::Private);
        assert_eq!(
            request.ports,
            vec![DockerPort {
                name: "http".to_owned(),
                container_port: 8080,
            }]
        );
        assert_eq!(
            request.labels.get(LABEL_CORRELATION),
            Some(&"deployment-one".to_owned())
        );
        assert_eq!(
            request.labels.get(LABEL_LAUNCH_FINGERPRINT),
            Some(&workload().exact_intent_fingerprint())
        );
        assert_eq!(
            request.device_requests,
            vec![DockerDeviceRequest {
                driver: "nvidia".to_owned(),
                device_ids: vec!["0".to_owned()],
                capabilities: Some(vec![vec!["gpu".to_owned()]]),
            }]
        );
    }

    #[test]
    fn translates_host_shared_ipc_to_the_exact_docker_request() {
        let mut workload = workload();
        workload.spec.ipc_mode = dotcore_runtime::WorkloadIpcMode::HostShared;

        let request = PreparedWorkload::from_launch(
            &workload,
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("host-shared IPC workload")
        .request;

        assert_eq!(request.ipc_mode, DockerIpcMode::Host);
    }

    #[test]
    fn translates_cdi_transport_without_legacy_request_fields() {
        let mut config = config();
        config.accelerator_transport = DockerAcceleratorTransport::Cdi;
        let mut workload = workload();
        workload.execution_target.accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
        workload.spec.accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];

        let request = PreparedWorkload::from_launch(
            &workload,
            &config,
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("valid CDI OCI workload")
        .request;

        assert_eq!(
            request.device_requests,
            vec![DockerDeviceRequest {
                driver: "cdi".to_owned(),
                device_ids: vec!["nvidia.com/gpu=0".to_owned()],
                capabilities: None,
            }]
        );
    }

    #[test]
    fn preserves_a_read_only_local_model_mount_in_the_docker_request() {
        let mut workload = workload();
        workload.spec.executable.path = "/app/llama-server".to_owned();
        workload.spec.arguments = vec![
            WorkloadValue::literal("--model"),
            WorkloadValue::literal("/opt/dotcore/models/model.gguf"),
        ];
        workload.spec.artifacts[0] = WorkloadArtifactBinding {
            name: "model".to_owned(),
            source: dotcore_domain::test_support::canonical_temp_dir(),
            destination: "/opt/dotcore/models/model.gguf".to_owned(),
            read_only: true,
        };

        let request = PreparedWorkload::from_launch(
            &workload,
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("valid OCI workload")
        .request;

        assert_eq!(request.entrypoint, vec!["/app/llama-server"]);
        assert_eq!(
            request.argv,
            vec!["--model", "/opt/dotcore/models/model.gguf"]
        );
        assert_eq!(
            request.mounts,
            vec![DockerMount {
                source: dotcore_domain::test_support::canonical_temp_dir(),
                target: "/opt/dotcore/models/model.gguf".to_owned(),
                read_only: true,
            }]
        );
    }

    #[test]
    fn resolves_spec_tokens_to_container_values() {
        let mut workload = workload();
        workload.spec.arguments = [
            "--model",
            "{artifact:data}",
            "--host",
            "{service:http.host}",
            "--port={service:http.port}",
        ]
        .map(|value| WorkloadValue::parse(value).expect("valid template"))
        .to_vec();
        workload.spec.environment.insert(
            "LAUNCHER".to_owned(),
            WorkloadValue::parse("{engine.executable}").expect("valid template"),
        );

        let request = PreparedWorkload::from_launch(
            &workload,
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("symbolic workload")
        .request;

        assert_eq!(request.entrypoint, vec!["/bin/server"]);
        assert_eq!(
            request.argv,
            vec![
                "--model",
                "/var/lib/workload",
                "--host",
                "0.0.0.0",
                "--port=8080"
            ]
        );
        assert_eq!(
            request.environment,
            vec!["EXPLICIT_VALUE=value", "LAUNCHER=/bin/server"]
        );
    }

    #[test]
    fn rejects_tokens_for_undeclared_ports_before_docker_mutation() {
        let mut workload = workload();
        workload
            .spec
            .arguments
            .push(WorkloadValue::parse("{service:metrics.port}").expect("valid template"));

        let error = PreparedWorkload::from_launch(
            &workload,
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .err()
        .expect("undeclared service port");

        assert_eq!(
            error.to_string(),
            "Invalid runtime launch: Docker runtime rejected the workload spec"
        );
        assert!(format!("{:#}", anyhow::Error::new(error)).ends_with(
            "workload token references service \"metrics\", which the workload does not \
             declare as a port"
        ));
    }

    /// Golden check that a symbolic workload spec, as the llama.cpp engine plans one for a
    /// deployment with vision and with or without an MTP draft, produces the literal container
    /// request. With MTP off the plan names no draft, so nothing mounts it.
    #[test]
    fn symbolic_workload_spec_produces_the_literal_container_request() {
        for mtp in [false, true] {
            assert_llama_cpp_container_request(mtp);
        }
    }

    fn assert_llama_cpp_container_request(mtp: bool) {
        let draft_arguments: &[&str] = if mtp {
            &[
                "--spec-type",
                "draft-mtp",
                "--model-draft",
                "{artifact:speculative-draft}",
                "--spec-draft-n-max",
                "3",
            ]
        } else {
            &[]
        };
        let source = dotcore_domain::test_support::canonical_temp_dir();
        let target = ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-one"),
            accelerator_device_ids: vec!["0".to_owned()],
        };
        let spec = WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::OciImage(
                    "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ),
                path: "/app/llama-server".to_owned(),
            },
            required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
            arguments: [
                "--model",
                "{artifact:model}",
                "--alias",
                "fixture-model",
                "--host",
                "{service:http.host}",
                "--port",
                "{service:http.port}",
                "--ctx-size",
                "1",
                "--n-gpu-layers",
                "0",
                "--mmproj",
                "{artifact:vision-projector}",
            ]
            .iter()
            .chain(draft_arguments)
            .map(|value| WorkloadValue::parse(value).expect("valid template"))
            .collect(),
            environment: BTreeMap::new(),
            artifacts: [
                ("model", "/opt/dotcore/models/model.gguf"),
                ("speculative-draft", "/opt/dotcore/models/speculative-draft.gguf"),
                ("vision-projector", "/opt/dotcore/models/projector.gguf"),
            ]
            .into_iter()
            .filter(|(name, _)| mtp || *name != "speculative-draft")
            .map(|(name, destination)| WorkloadArtifactBinding {
                name: name.to_owned(),
                source: source.clone(),
                destination: destination.to_owned(),
                read_only: true,
            })
            .collect(),
            storage: Vec::new(),
            ports: vec![WorkloadPort {
                name: "http".to_owned(),
                container_port: 8080,
            }],
            accelerator_device_ids: vec!["0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: dotcore_runtime::WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        };

        let request = PreparedWorkload::from_launch(
            &WorkloadLaunch::new("deployment-one", target, spec),
            &config(),
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("llama.cpp Docker request")
        .request;

        assert_eq!(
            request.image,
            "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(request.entrypoint, vec!["/app/llama-server"]);
        let draft_argv: &[&str] = if mtp {
            &[
                "--spec-type",
                "draft-mtp",
                "--model-draft",
                "/opt/dotcore/models/speculative-draft.gguf",
                "--spec-draft-n-max",
                "3",
            ]
        } else {
            &[]
        };
        assert_eq!(
            request.argv,
            [
                "--model",
                "/opt/dotcore/models/model.gguf",
                "--alias",
                "fixture-model",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--ctx-size",
                "1",
                "--n-gpu-layers",
                "0",
                "--mmproj",
                "/opt/dotcore/models/projector.gguf",
            ]
            .iter()
            .chain(draft_argv)
            .copied()
            .collect::<Vec<_>>()
        );
        let draft_mount: &[&str] = if mtp {
            &["/opt/dotcore/models/speculative-draft.gguf"]
        } else {
            &[]
        };
        assert_eq!(
            request.mounts,
            ["/opt/dotcore/models/model.gguf"]
                .iter()
                .chain(draft_mount)
                .chain(&["/opt/dotcore/models/projector.gguf"])
                .map(|target| DockerMount {
                    source: source.clone(),
                    target: (*target).to_owned(),
                    read_only: true,
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            request.ports,
            [DockerPort {
                name: "http".to_owned(),
                container_port: 8080,
            }]
        );
        assert_eq!(
            request.device_requests,
            [DockerDeviceRequest {
                driver: "nvidia".to_owned(),
                device_ids: vec!["0".to_owned()],
                capabilities: Some(vec![vec!["gpu".to_owned()]]),
            }]
        );
        assert_eq!(request.environment, Vec::<String>::new());
        assert_eq!(request.ipc_mode, DockerIpcMode::Private);
        assert_eq!(
            request.labels.get(LABEL_PORTS),
            Some(&"http=8080".to_owned())
        );
    }

    #[test]
    fn uses_the_site_scoped_schema_five_name_for_maximum_valid_identity_values() {
        let mut config = config();
        config.site_id = format!("a{}", "a".repeat(62));
        let mut workload = workload();
        workload.correlation.owner_id = format!("a{}", "a".repeat(62));
        workload.correlation.reconcile_generation = u64::MAX;

        let request = PreparedWorkload::from_launch(
            &workload,
            &config,
            RuntimeBackendId("runtime/docker".to_owned()),
        )
        .expect("maximum valid workload identity")
        .request;

        assert_eq!(request.name.len(), MAX_WORKLOAD_ID_BYTES);
        assert_eq!(
            request.name,
            format!(
                "dotcore_{}_{}_{}",
                config.site_id,
                workload.correlation.owner_id,
                u64::MAX
            )
        );
        assert_eq!(
            request.labels.get(LABEL_SCHEMA),
            Some(&OWNERSHIP_SCHEMA.to_owned())
        );
    }
}
