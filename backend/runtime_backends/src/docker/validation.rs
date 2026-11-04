//! Validation of workload intent before Docker mutation: the spec must resolve for a
//! container, and every resolved value, mount, port, and device must be safe to hand to
//! the Docker Engine.

use std::{collections::BTreeSet, fs, path::Path};

use dotcore_domain::oci::validate_rendered_reference;
use dotcore_runtime::{
    MAX_WORKLOAD_ID_BYTES, ResolvedWorkloadCommand, RuntimeBackendError,
    WorkloadExecutableArtifact, WorkloadLaunch,
};

use super::{
    BACKEND_ID,
    adapter::{DockerAcceleratorTransport, DockerRuntimeConfig},
    request::ContainerTokens,
    storage::validate_storage,
};

/// Validates launch intent before the adapter calls Docker and returns the container command.
pub(super) fn validate_launch(
    workload: &WorkloadLaunch,
    config: &DockerRuntimeConfig,
) -> Result<ResolvedWorkloadCommand, RuntimeBackendError> {
    if workload.correlation.reconcile_generation == 0 {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime requires a nonzero reconcile generation",
        ));
    }
    validate_workload_component(&workload.correlation.owner_id, "workload correlation")?;
    validate_workload_component(&config.site_id, "Site identity")?;
    if !valid_approved_mount_root(&config.approved_mount_root) {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime requires a non-root approved mount directory",
        ));
    }
    let spec = &workload.spec;
    let WorkloadExecutableArtifact::OciImage(image) = &spec.executable.artifact else {
        return Err(RuntimeBackendError::UnsupportedExecutableArtifact {
            backend_id: BACKEND_ID,
            kind: spec.executable.artifact.kind(),
        });
    };
    validate_execution_reference(image)?;
    let command = spec.resolve(&ContainerTokens).map_err(|error| {
        RuntimeBackendError::InvalidLaunch(
            anyhow::Error::new(error).context("Docker runtime rejected the workload spec"),
        )
    })?;
    if command.executable.is_empty() || command.arguments.iter().any(|value| !valid_argument(value))
    {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime requires a safe nonempty exec-form command",
        ));
    }
    if spec.accelerator_device_ids != workload.execution_target.accelerator_device_ids {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime accelerator devices must match the selected execution target",
        ));
    }
    if spec.accelerator_device_ids.len() != spec.required_accelerator_device_count as usize {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime accelerator devices do not meet the workload requirement",
        ));
    }
    let mut environment_names = BTreeSet::new();
    for (name, value) in &command.environment {
        if !valid_environment_name(name)
            || value.contains('=')
            || value.chars().any(char::is_control)
            || !environment_names.insert(name)
        {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker runtime requires safe explicit environment values",
            ));
        }
    }
    let mut mount_names = BTreeSet::new();
    let mut mount_destinations = BTreeSet::new();
    for mount in &spec.artifacts {
        // Runtime-provisioned caches are the only writable host paths.
        if validate_name(&mount.name, "mount name").is_err()
            || !mount.read_only
            || !mount.source.is_absolute()
            || !path_is_under(&mount.source, &config.approved_mount_root)
            || !valid_container_path(&mount.destination)
            || !mount_names.insert(&mount.name)
            || !mount_destinations.insert(&mount.destination)
        {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker runtime rejected an unsafe named mount",
            ));
        }
    }
    let mut storage_names = BTreeSet::new();
    for storage in &spec.storage {
        validate_storage(storage)?;
        if !valid_container_path(&storage.destination)
            || !storage_names.insert(&storage.name)
            || !mount_destinations.insert(&storage.destination)
        {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker runtime rejected an unsafe storage attachment",
            ));
        }
    }
    let mut port_names = BTreeSet::new();
    let mut port_numbers = BTreeSet::new();
    for port in &spec.ports {
        if validate_name(&port.name, "port name").is_err()
            || port.container_port == 0
            || !port_names.insert(&port.name)
            || !port_numbers.insert(port.container_port)
        {
            return Err(RuntimeBackendError::invalid_launch(
                "Docker runtime rejected an unsafe declared port",
            ));
        }
    }
    if spec
        .accelerator_device_ids
        .iter()
        .any(|id| !valid_device_id(id, config.accelerator_transport))
    {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime rejected an unsafe accelerator device",
        ));
    }
    Ok(command)
}

pub(super) fn validate_execution_reference(image: &str) -> Result<(), RuntimeBackendError> {
    validate_rendered_reference(image)
        .map(|_| ())
        .map_err(|error| {
            RuntimeBackendError::invalid_launch(format!(
                "Docker runtime requires a canonical OCI image reference: {error}"
            ))
        })
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return false;
    };
    root.is_dir() && path.starts_with(&root) && !is_socket(&metadata.file_type())
}

fn valid_approved_mount_root(root: &Path) -> bool {
    root.is_absolute()
        && root != Path::new("/")
        && root
            .canonicalize()
            .is_ok_and(|path| path.is_dir() && path != Path::new("/"))
}

fn is_socket(file_type: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;

    file_type.is_socket()
}

pub(super) fn valid_container_path(value: &str) -> bool {
    value.starts_with('/')
        && value != "/"
        && value
            .split('/')
            .skip(1)
            .all(|part| !matches!(part, "" | "." | ".."))
}

pub(super) fn validate_name(value: &str, description: &str) -> Result<(), RuntimeBackendError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RuntimeBackendError::invalid_launch(format!(
            "Docker runtime requires a safe {description}"
        )));
    }
    Ok(())
}

pub(super) fn validate_workload_name(value: &str) -> Result<(), RuntimeBackendError> {
    if value.is_empty()
        || value.len() > MAX_WORKLOAD_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(RuntimeBackendError::invalid_launch(
            "Docker runtime requires a safe Dotcore workload name",
        ));
    }
    Ok(())
}

fn validate_workload_component(value: &str, description: &str) -> Result<(), RuntimeBackendError> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(RuntimeBackendError::invalid_launch(format!(
            "Docker runtime requires a safe {description}"
        )));
    }
    Ok(())
}

fn valid_argument(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(char::is_control)
}

pub(super) fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(super) fn valid_device_id(value: &str, transport: DockerAcceleratorTransport) -> bool {
    match transport {
        DockerAcceleratorTransport::Cdi => valid_cdi_device_id(value),
        DockerAcceleratorTransport::LegacyNvidia => {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        }
    }
}

fn valid_cdi_device_id(value: &str) -> bool {
    let Some((vendor, qualified_device)) = value.split_once('/') else {
        return false;
    };
    let Some((class, device)) = qualified_device.split_once('=') else {
        return false;
    };
    !vendor.is_empty()
        && vendor.contains('.')
        && !class.is_empty()
        && !device.is_empty()
        && [vendor, class, device].iter().all(|part| {
            part.len() <= 128
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
                })
        })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use dotcore_domain::SiteNodeId;
    use dotcore_runtime::{
        ExecutionTarget, WorkloadArtifactBinding, WorkloadCorrelation, WorkloadExecutable,
        WorkloadExecutableArtifact, WorkloadPort, WorkloadSpec, WorkloadStorageAttachment,
        WorkloadStorageKind, WorkloadValue,
    };

    use super::*;

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
    fn rejects_writable_or_unsafe_mounts_and_storage_attachments() {
        let storage = |name: &str, destination: &str| WorkloadStorageAttachment {
            name: name.to_owned(),
            destination: destination.to_owned(),
            kind: WorkloadStorageKind::Cache,
            namespace: "deployment".to_owned(),
            key: "deployment-one".to_owned(),
            read_only: false,
            expires_when_unused: false,
        };
        let mut valid = workload();
        valid
            .spec
            .storage
            .push(storage("engine-cache", "/var/cache/engine"));
        assert!(validate_launch(&valid, &config()).is_ok());

        let mut writable = workload();
        writable.spec.artifacts[0].read_only = false;
        let mut unsafe_name = workload();
        unsafe_name
            .spec
            .storage
            .push(storage("..", "/var/cache/engine"));
        let mut unsafe_key = valid.clone();
        unsafe_key.spec.storage[0].key = "a/b".to_owned();
        let mut relative = workload();
        relative
            .spec
            .storage
            .push(storage("engine-cache", "var/cache"));
        let mut over_artifact = workload();
        over_artifact
            .spec
            .storage
            .push(storage("engine-cache", "/var/lib/workload"));
        let mut colliding = valid.clone();
        colliding
            .spec
            .storage
            .push(storage("other-cache", "/var/cache/engine"));
        let mut duplicate = valid.clone();
        duplicate
            .spec
            .storage
            .push(storage("engine-cache", "/var/cache/other"));

        assert_eq!(
            validate_launch(&writable, &config())
                .expect_err("writable owner mount")
                .to_string(),
            "Invalid runtime launch: Docker runtime rejected an unsafe named mount"
        );
        let RuntimeBackendError::InvalidLaunch(error) =
            validate_launch(&over_artifact, &config()).expect_err("storage over an artifact")
        else {
            panic!("storage over an artifact must be an invalid launch");
        };
        assert_eq!(
            format!("{error:#}"),
            "Docker runtime rejected the workload spec: workload storage \"engine-cache\" is mounted at \"/var/lib/workload\", where an artifact is mounted"
        );
        for (case, workload) in [
            ("unsafe name", unsafe_name),
            ("unsafe key", unsafe_key),
            ("relative destination", relative),
            ("colliding destination", colliding),
            ("duplicate name", duplicate),
        ] {
            assert_eq!(
                validate_launch(&workload, &config())
                    .expect_err(case)
                    .to_string(),
                "Invalid runtime launch: Docker runtime rejected an unsafe storage attachment",
                "{case}"
            );
        }
    }

    #[test]
    fn owner_mounts_cannot_reach_the_workload_cache_root() {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-validation-cache-roots-{}",
            std::process::id()
        ));
        let artifacts = root.join("model-artifacts");
        let caches = root.join("workload-cache");
        fs::create_dir_all(&artifacts).expect("artifact root");
        fs::create_dir_all(caches.join("site-one/other-deployment")).expect("cache folder");
        let config = DockerRuntimeConfig {
            approved_mount_root: artifacts,
            workload_cache_root: caches.clone(),
            ..config()
        };
        let mut workload = workload();
        workload.spec.artifacts[0].source = caches.join("site-one/other-deployment");

        assert!(validate_launch(&workload, &config).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn accepts_tag_digest_and_combined_execution_references() {
        for image in [
            "example.test/workload:dev",
            "example.test/workload@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "[::1]:5000/example/workload:release@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut workload = workload();
            workload.spec.executable.artifact =
                WorkloadExecutableArtifact::OciImage(image.to_owned());
            assert!(validate_launch(&workload, &config()).is_ok(), "{image}");
        }
    }

    #[test]
    fn rejects_malformed_execution_references() {
        for image in [
            "example.test/workload",
            "https://example.test/workload:dev",
            "Example.test/workload:dev",
            "example.test/workload@@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "example.test/workload@sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            let mut workload = workload();
            workload.spec.executable.artifact =
                WorkloadExecutableArtifact::OciImage(image.to_owned());
            assert!(validate_launch(&workload, &config()).is_err(), "{image}");
        }
    }

    #[test]
    fn rejects_nix_store_path_executables_with_a_typed_error() {
        let mut workload = workload();
        workload.spec.executable.artifact = WorkloadExecutableArtifact::NixStorePath(
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-server".to_owned(),
        );

        let error = validate_launch(&workload, &config()).expect_err("Nix executable rejected");

        assert!(
            matches!(
                error,
                RuntimeBackendError::UnsupportedExecutableArtifact {
                    backend_id: "runtime/docker",
                    kind: dotcore_domain::WorkloadArtifactKind::Nix,
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "Runtime runtime/docker cannot launch Nix store path executables"
        );
    }

    #[test]
    fn permits_empty_nonexecutable_argv_tokens() {
        let mut workload = workload();
        workload.spec.arguments.push(WorkloadValue::literal(""));

        assert!(validate_launch(&workload, &config()).is_ok());
    }

    #[test]
    fn rejects_unsafe_oci_input() {
        let mut unsafe_image = workload();
        unsafe_image.spec.executable.artifact =
            WorkloadExecutableArtifact::OciImage("example.test/Unsafe:dev".to_owned());
        let mut unsafe_command = workload();
        unsafe_command.spec.executable.path.clear();
        let mut unsafe_mount = workload();
        unsafe_mount.spec.artifacts[0].source = PathBuf::from("/var/run/docker.sock");
        let mut unsafe_port = workload();
        unsafe_port.spec.ports[0].container_port = 0;
        let mut unsafe_device = workload();
        unsafe_device.spec.accelerator_device_ids = vec!["0,1".to_owned()];
        let mut missing_required_device = workload();
        missing_required_device.spec.accelerator_device_ids.clear();
        missing_required_device
            .execution_target
            .accelerator_device_ids
            .clear();
        let mut root_mount_config = config();
        root_mount_config.approved_mount_root = PathBuf::from("/");

        for unsafe_workload in [
            unsafe_image,
            unsafe_command,
            unsafe_mount,
            unsafe_port,
            unsafe_device,
            missing_required_device,
        ] {
            assert!(validate_launch(&unsafe_workload, &config()).is_err());
        }
        assert!(validate_launch(&workload(), &root_mount_config).is_err());
    }

    #[test]
    fn rejects_transport_identifier_mismatches_and_malformed_identifiers() {
        for (transport, device_id) in [
            (DockerAcceleratorTransport::Cdi, "0"),
            (DockerAcceleratorTransport::LegacyNvidia, "nvidia.com/gpu=0"),
            (DockerAcceleratorTransport::Cdi, "nvidia.com/gpu"),
            (DockerAcceleratorTransport::Cdi, "gpu=0"),
            (DockerAcceleratorTransport::LegacyNvidia, "-1"),
            (DockerAcceleratorTransport::LegacyNvidia, "gpu0"),
        ] {
            let mut config = config();
            config.accelerator_transport = transport;
            let mut workload = workload();
            workload.execution_target.accelerator_device_ids = vec![device_id.to_owned()];
            workload.spec.accelerator_device_ids = vec![device_id.to_owned()];

            assert!(validate_launch(&workload, &config).is_err());
        }
    }

    #[test]
    fn mount_validation_rejects_symlink_escapes_and_socket_targets() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        // Kept short: the socket path below must fit the 104-byte `sockaddr_un` limit on macOS.
        let root = dotcore_domain::test_support::canonical_temp_dir()
            .join(format!("dotcore-mount-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("mount root");
        let mut config = config();
        config.approved_mount_root = root.clone();
        let cache = root.join("cache");
        fs::create_dir(&cache).expect("cache directory");
        let mut allowed = workload();
        allowed.spec.artifacts[0].source = cache;
        assert!(validate_launch(&allowed, &config).is_ok());
        let escape = root.join("escape");
        symlink(dotcore_domain::test_support::canonical_temp_dir(), &escape)
            .expect("escape symlink");
        let mut escaped = workload();
        escaped.spec.artifacts[0].source = escape;
        assert!(validate_launch(&escaped, &config).is_err());

        let socket = root.join("socket");
        let _socket = UnixListener::bind(&socket).expect("socket target");
        let mut socket_mount = workload();
        socket_mount.spec.artifacts[0].source = socket;
        assert!(validate_launch(&socket_mount, &config).is_err());
        fs::remove_dir_all(root).expect("remove mount root");
    }
}
