//! Turns an owner's workload launch into the exact launcher request of one native workload: the
//! resolved command, only its declared environment, and the owner record that lets a later scan
//! recompute the workload's identity.
//!
//! Tokens resolve to host values: the executable to `<store path>/<relative path>`, for example
//! `/nix/store/<hash>-server/bin/server`, artifact bindings to their prepared host paths, service
//! hosts to `127.0.0.1`, and service ports to assigned loopback ports. Resolution never touches
//! the filesystem, so a scan can recompute a workload whose store path or artifact is gone;
//! existence and executability are checked only before a create.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use dotcore_domain::{WorkloadArtifactKind, nix::valid_store_entry, valid_id};
use dotcore_launcher::workload::{DEFAULT_GRACE_PERIOD_MS, LaunchSpec, Lifetime};
use dotcore_runtime::{
    ResolvedWorkloadCommand, RuntimeBackendError, WorkloadArtifactBinding, WorkloadCorrelation,
    WorkloadExecutable, WorkloadExecutableArtifact, WorkloadLaunch, WorkloadPort, WorkloadSpec,
    WorkloadSpecError, WorkloadTokenResolver,
};

use super::{
    BACKEND_ID, NativeRuntimeConfig,
    record::{IDENTITY_VERSION, NativeOwnerRecord, NativeRecordError},
    state_root::OpenedStateRoot,
};

/// Address every native workload service binds to.
pub(super) const LOOPBACK_HOST: &str = "127.0.0.1";

/// Launcher request of one native workload, the directory name it is created under, and the
/// owner record its request carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PreparedLaunch {
    /// Workload id and directory name, `<owner>_<generation>`.
    pub(super) workload_id: String,
    pub(super) spec: LaunchSpec,
    pub(super) record: NativeOwnerRecord,
}

/// Validates a launch against the host, then builds its launcher request with the assigned
/// loopback ports and lifetime. The request's environment is exactly the declared one.
pub(super) fn prepare_launch(
    config: &NativeRuntimeConfig,
    state_root: &OpenedStateRoot,
    launch: &WorkloadLaunch,
    ports: BTreeMap<String, u16>,
    lifetime: Lifetime,
) -> Result<PreparedLaunch, NativeLaunchError> {
    let correlation = &launch.correlation;
    if !valid_id(&correlation.owner_id) || correlation.reconcile_generation == 0 {
        return Err(NativeLaunchError::InvalidCorrelation {
            owner_id: correlation.owner_id.clone(),
        });
    }
    if launch.execution_target.managed_node_id != config.site_node_id {
        return Err(NativeLaunchError::OtherSiteNode {
            site_node_id: launch.execution_target.managed_node_id.to_string(),
        });
    }
    let command = resolve_command(&launch.spec, config, &ports)?;
    check_host_paths(&launch.spec, config)?;

    let record = NativeOwnerRecord {
        identity_version: IDENTITY_VERSION,
        state_root: state_root.canonical_root.clone(),
        site_id: config.site_id.clone(),
        execution_target_id: config.execution_target_id.clone(),
        launch: launch.clone(),
        ports,
    };
    Ok(PreparedLaunch {
        workload_id: workload_id(correlation),
        spec: LaunchSpec {
            executable: PathBuf::from(command.executable),
            args: command.arguments,
            environment: command.environment,
            grace_period_ms: DEFAULT_GRACE_PERIOD_MS,
            lifetime,
            owner: record.encode().map_err(NativeLaunchError::Record)?,
            fingerprint: launch.exact_intent_fingerprint(),
        },
        record,
    })
}

/// Resolves a spec into its host command without touching the filesystem, rejecting what a
/// native workload cannot run: non-Nix executables, paths outside the store or the approved
/// artifact root, cache attachments, and ports without exactly one distinct assigned port.
pub(super) fn resolve_command(
    spec: &WorkloadSpec,
    config: &NativeRuntimeConfig,
    ports: &BTreeMap<String, u16>,
) -> Result<ResolvedWorkloadCommand, NativeLaunchError> {
    let WorkloadExecutableArtifact::NixStorePath(store_path) = &spec.executable.artifact else {
        return Err(NativeLaunchError::UnsupportedExecutable {
            kind: spec.executable.artifact.kind(),
        });
    };
    let store_path = Path::new(store_path);
    if store_path.parent() != Some(config.store_root.as_path())
        || !store_path
            .file_name()
            .and_then(|entry| entry.to_str())
            .is_some_and(valid_store_entry)
    {
        return Err(NativeLaunchError::StorePathOutsideStore {
            store_path: store_path.to_path_buf(),
            store_root: config.store_root.clone(),
        });
    }
    if !is_plain_relative(Path::new(&spec.executable.path)) {
        return Err(NativeLaunchError::UnsafeExecutablePath {
            path: spec.executable.path.clone(),
        });
    }
    if !spec.storage.is_empty() {
        return Err(NativeLaunchError::StorageAttachments);
    }
    for binding in &spec.artifacts {
        if !binding.read_only {
            return Err(NativeLaunchError::WritableArtifact {
                binding: binding.name.clone(),
            });
        }
        let inside_root = binding
            .source
            .strip_prefix(&config.approved_artifact_root)
            .is_ok_and(is_plain_relative);
        if !binding.source.is_absolute() || !inside_root || binding.source.to_str().is_none() {
            return Err(NativeLaunchError::ArtifactOutsideRoot {
                binding: binding.name.clone(),
            });
        }
    }
    validate_ports(&spec.ports, ports)?;

    let command = spec
        .resolve(&HostTokens { ports })
        .map_err(NativeLaunchError::Spec)?;
    let safe_environment = command.environment.iter().all(|(name, value)| {
        !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0')
    });
    if command.arguments.iter().any(|value| value.contains('\0')) || !safe_environment {
        return Err(NativeLaunchError::UnsafeValue);
    }
    Ok(command)
}

/// Directory name and id of the workload for one owner correlation.
pub(super) fn workload_id(correlation: &WorkloadCorrelation) -> String {
    format!(
        "{}_{}",
        correlation.owner_id, correlation.reconcile_generation
    )
}

/// Checks the host paths a validated spec uses: the executable exists inside the store and is
/// executable, possibly through symlinks, and every artifact exists inside the approved root.
fn check_host_paths(
    spec: &WorkloadSpec,
    config: &NativeRuntimeConfig,
) -> Result<(), NativeLaunchError> {
    let root = &config.approved_artifact_root;
    let canonical_root = fs::canonicalize(root)
        .ok()
        .filter(|canonical| root.is_absolute() && canonical.is_dir() && canonical != Path::new("/"))
        .ok_or_else(|| NativeLaunchError::ArtifactRoot { root: root.clone() })?;
    for binding in &spec.artifacts {
        let source = fs::canonicalize(&binding.source).map_err(|source| {
            NativeLaunchError::ArtifactUnavailable {
                binding: binding.name.clone(),
                source,
            }
        })?;
        if !source.starts_with(&canonical_root) {
            return Err(NativeLaunchError::ArtifactOutsideRoot {
                binding: binding.name.clone(),
            });
        }
    }

    let path = PathBuf::from(executable_path(&spec.executable));
    let unavailable = |source| NativeLaunchError::ExecutableUnavailable {
        path: path.clone(),
        source,
    };
    let canonical_store = fs::canonicalize(&config.store_root).map_err(unavailable)?;
    let canonical = fs::canonicalize(&path).map_err(unavailable)?;
    if !canonical.starts_with(&canonical_store) {
        return Err(NativeLaunchError::ExecutableOutsideStore { path });
    }
    let metadata = fs::metadata(&canonical).map_err(unavailable)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(NativeLaunchError::NotExecutable { path });
    }
    Ok(())
}

fn validate_ports(
    declared: &[WorkloadPort],
    assigned: &BTreeMap<String, u16>,
) -> Result<(), NativeLaunchError> {
    for port in declared {
        if !assigned
            .get(&port.name)
            .is_some_and(|host_port| *host_port != 0)
        {
            return Err(NativeLaunchError::UnassignedPort {
                port: port.name.clone(),
            });
        }
    }
    if let Some(name) = assigned
        .keys()
        .find(|name| !declared.iter().any(|port| &port.name == *name))
    {
        return Err(NativeLaunchError::UndeclaredPort { port: name.clone() });
    }
    let mut host_ports = BTreeSet::new();
    if let Some(port) = assigned.values().find(|port| !host_ports.insert(**port)) {
        return Err(NativeLaunchError::SharedHostPort { port: *port });
    }
    Ok(())
}

/// Whether a path is relative and made only of plain names, without `.`, `..`, or a root.
fn is_plain_relative(path: &Path) -> bool {
    path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn executable_path(executable: &WorkloadExecutable) -> String {
    format!("{}/{}", executable.artifact.reference(), executable.path)
}

/// Host values of a native workload's tokens.
struct HostTokens<'a> {
    /// Assigned loopback port by declared port name, checked complete before resolution.
    ports: &'a BTreeMap<String, u16>,
}

impl WorkloadTokenResolver for HostTokens<'_> {
    fn executable(&self, executable: &WorkloadExecutable) -> String {
        executable_path(executable)
    }

    fn artifact(&self, binding: &WorkloadArtifactBinding) -> String {
        binding.source.to_string_lossy().into_owned()
    }

    fn service_host(&self, _: &WorkloadPort) -> String {
        LOOPBACK_HOST.to_owned()
    }

    fn service_port(&self, port: &WorkloadPort) -> u16 {
        self.ports.get(&port.name).copied().unwrap_or_default()
    }
}

/// Workload launch the native runtime rejects.
#[derive(Debug, thiserror::Error)]
pub enum NativeLaunchError {
    #[error("native runtime cannot launch {kind} executables")]
    UnsupportedExecutable { kind: WorkloadArtifactKind },

    #[error(
        "native workload correlation {owner_id:?} needs a valid owner id and a nonzero generation"
    )]
    InvalidCorrelation { owner_id: String },

    #[error("native workload targets node {site_node_id}, which this runtime does not host")]
    OtherSiteNode { site_node_id: String },

    #[error(
        "native executable store path {} is not a top-level entry of the store root {}",
        store_path.display(),
        store_root.display()
    )]
    StorePathOutsideStore {
        store_path: PathBuf,
        store_root: PathBuf,
    },

    #[error(
        "native executable path {path:?} must be relative to its store path, without `.` or `..`"
    )]
    UnsafeExecutablePath { path: String },

    #[error("native executable {} is unavailable", path.display())]
    ExecutableUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("native executable {} resolves outside the store root", path.display())]
    ExecutableOutsideStore { path: PathBuf },

    #[error("native executable {} is not an executable file", path.display())]
    NotExecutable { path: PathBuf },

    #[error("native artifact root {} must be an absolute directory other than /", root.display())]
    ArtifactRoot { root: PathBuf },

    #[error("native artifact binding {binding:?} is outside the approved artifact root")]
    ArtifactOutsideRoot { binding: String },

    #[error("native artifact binding {binding:?} must be read-only")]
    WritableArtifact { binding: String },

    #[error("native artifact binding {binding:?} is unavailable")]
    ArtifactUnavailable {
        binding: String,
        #[source]
        source: io::Error,
    },

    #[error("native workloads cannot attach runtime-provisioned storage")]
    StorageAttachments,

    #[error("native workload port {port:?} has no assigned loopback port")]
    UnassignedPort { port: String },

    #[error("native workload assigns loopback port to undeclared port {port:?}")]
    UndeclaredPort { port: String },

    #[error("native workload assigns loopback port {port} to more than one declared port")]
    SharedHostPort { port: u16 },

    #[error("native workload spec cannot be resolved")]
    Spec(#[source] WorkloadSpecError),

    #[error(
        "native workload arguments and environment must not contain NUL bytes, and environment \
         names must be nonempty without `=`"
    )]
    UnsafeValue,

    #[error("native owner record cannot be stored")]
    Record(#[source] NativeRecordError),
}

impl From<NativeLaunchError> for RuntimeBackendError {
    fn from(error: NativeLaunchError) -> Self {
        match error {
            NativeLaunchError::UnsupportedExecutable { kind } => {
                Self::UnsupportedExecutableArtifact {
                    backend_id: BACKEND_ID,
                    kind,
                }
            }
            error => Self::InvalidLaunch(
                anyhow::Error::new(error).context("native runtime rejected the workload launch"),
            ),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::os::unix::fs::symlink;

    use dotcore_domain::SiteNodeId;
    use dotcore_runtime::{
        ExecutionTarget, WorkloadIpcMode, WorkloadStorageAttachment, WorkloadStorageKind,
        WorkloadValue,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::native::{
        NativeStateRoot, NativeWorkloadLifetime,
        test_support::{HASH, launch, ports},
    };

    #[test]
    fn launches_resolve_tokens_to_host_values_with_only_the_declared_environment() {
        let fixture = Fixture::new();
        let launch = fixture.launch();

        let prepared = fixture.prepare(&launch).expect("prepare launch");

        assert_eq!(
            prepared.workload_id,
            format!("deployment-one_{}", launch.correlation.reconcile_generation)
        );
        let executable = format!("{}/bin/server", fixture.store_path);
        assert_eq!(prepared.spec.executable, PathBuf::from(&executable));
        assert_eq!(
            prepared.spec.args,
            [
                "--model",
                fixture.artifact.to_str().unwrap(),
                "--host",
                "127.0.0.1",
                "--port=41234",
                "--options={\"json\":true}",
            ]
        );
        assert_eq!(
            prepared.spec.environment,
            BTreeMap::from([
                ("LAUNCHER".to_owned(), executable),
                ("MODE".to_owned(), "serve".to_owned()),
            ])
        );
        assert_eq!(prepared.spec.grace_period_ms, DEFAULT_GRACE_PERIOD_MS);
        assert_eq!(prepared.spec.lifetime, Lifetime::Independent);
        assert_eq!(prepared.spec.fingerprint, launch.exact_intent_fingerprint());
        let record = NativeOwnerRecord::decode(&prepared.spec.owner).expect("owner record");
        assert_eq!(record.launch, launch);
        assert_eq!(record.state_root, fixture.state_root.canonical_root);
        assert_eq!(record.identity_version, IDENTITY_VERSION);
        assert_eq!(
            (record.site_id.as_str(), record.execution_target_id.as_str()),
            ("site-one", "mac-gpu")
        );
        assert_eq!(record.ports, ports());
    }

    #[test]
    fn launches_with_unusable_executables_are_rejected() {
        let fixture = Fixture::new();
        let store = fixture.config.store_root.clone();
        let entry = |name: &str| store.join(name).to_str().unwrap().to_owned();
        let mut oci = fixture.launch();
        oci.spec.executable.artifact =
            WorkloadExecutableArtifact::OciImage("example.test/workload:dev".to_owned());

        assert!(matches!(
            fixture.prepare(&oci),
            Err(NativeLaunchError::UnsupportedExecutable {
                kind: WorkloadArtifactKind::Oci
            })
        ));
        assert!(matches!(
            RuntimeBackendError::from(fixture.prepare(&oci).unwrap_err()),
            RuntimeBackendError::UnsupportedExecutableArtifact {
                backend_id: "runtime/native",
                kind: WorkloadArtifactKind::Oci,
            }
        ));
        for store_path in [
            format!("/elsewhere/{HASH}-fake"),
            format!("{}/bin", fixture.store_path),
            entry("not-a-store-entry"),
            entry(&format!("{HASH}-fake/../{HASH}-fake")),
        ] {
            let mut launch = fixture.launch();
            launch.spec.executable.artifact =
                WorkloadExecutableArtifact::NixStorePath(store_path.clone());
            assert!(
                matches!(
                    fixture.prepare(&launch),
                    Err(NativeLaunchError::StorePathOutsideStore { .. })
                ),
                "{store_path}"
            );
        }
        for path in [
            "../escape/bin/server",
            "bin/../../server",
            "/bin/server",
            "./bin/server",
            "",
        ] {
            let mut launch = fixture.launch();
            launch.spec.executable.path = path.to_owned();
            assert!(
                matches!(
                    fixture.prepare(&launch),
                    Err(NativeLaunchError::UnsafeExecutablePath { .. })
                ),
                "{path:?}"
            );
        }
    }

    #[test]
    fn create_checks_that_the_executable_exists_inside_the_store_and_is_executable() {
        let fixture = Fixture::new();
        let package = PathBuf::from(&fixture.store_path);
        fs::write(package.join("bin/plain"), "data").unwrap();
        fs::create_dir(package.join("bin/directory")).unwrap();
        symlink("server", package.join("bin/linked")).unwrap();
        let outside = fixture.root.path().join("outside");
        fs::write(&outside, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&outside, package.join("bin/escape")).unwrap();
        let with_path = |path: &str| {
            let mut launch = fixture.launch();
            launch.spec.executable.path = path.to_owned();
            fixture.prepare(&launch)
        };

        assert!(with_path("bin/linked").is_ok());
        assert!(matches!(
            with_path("bin/missing"),
            Err(NativeLaunchError::ExecutableUnavailable { .. })
        ));
        assert!(matches!(
            with_path("bin/plain"),
            Err(NativeLaunchError::NotExecutable { .. })
        ));
        assert!(matches!(
            with_path("bin/directory"),
            Err(NativeLaunchError::NotExecutable { .. })
        ));
        assert!(matches!(
            with_path("bin/escape"),
            Err(NativeLaunchError::ExecutableOutsideStore { .. })
        ));
    }

    #[test]
    fn launches_with_artifacts_outside_the_approved_root_or_caches_are_rejected() {
        let fixture = Fixture::new();
        let outside = fixture.root.path().join("outside.bin");
        fs::write(&outside, "weights").unwrap();
        symlink(
            &outside,
            fixture.config.approved_artifact_root.join("escape.bin"),
        )
        .unwrap();
        let with_source = |source: PathBuf| {
            let mut launch = fixture.launch();
            launch.spec.artifacts[0].source = source;
            fixture.prepare(&launch)
        };
        let root = &fixture.config.approved_artifact_root;

        for source in [
            outside.clone(),
            root.join("../outside.bin"),
            PathBuf::from("artifacts/model.bin"),
            root.join("escape.bin"),
        ] {
            assert!(
                matches!(
                    with_source(source.clone()),
                    Err(NativeLaunchError::ArtifactOutsideRoot { .. })
                ),
                "{}",
                source.display()
            );
        }
        assert!(matches!(
            with_source(root.join("missing.bin")),
            Err(NativeLaunchError::ArtifactUnavailable { .. })
        ));
        let mut writable = fixture.launch();
        writable.spec.artifacts[0].read_only = false;
        assert!(matches!(
            fixture.prepare(&writable),
            Err(NativeLaunchError::WritableArtifact { .. })
        ));
        let mut stored = fixture.launch();
        stored.spec.storage.push(WorkloadStorageAttachment {
            name: "compile".to_owned(),
            destination: "/var/cache/compile".to_owned(),
            kind: WorkloadStorageKind::Cache,
            namespace: "deployment".to_owned(),
            key: "deployment-one".to_owned(),
            read_only: false,
            expires_when_unused: false,
        });
        assert!(matches!(
            fixture.prepare(&stored),
            Err(NativeLaunchError::StorageAttachments)
        ));
    }

    #[test]
    fn directory_artifacts_are_accepted_only_inside_the_approved_root() {
        let fixture = Fixture::new();
        let inside = fixture.config.approved_artifact_root.join("split");
        let outside = fixture.root.path().join("split");
        for directory in [&inside, &outside] {
            fs::create_dir_all(directory.join("Q4")).unwrap();
            fs::write(directory.join("Q4/model-00001-of-00002.gguf"), "first").unwrap();
        }
        let with_source = |source: &Path| {
            let mut launch = fixture.launch();
            launch.spec.artifacts[0].source = source.to_path_buf();
            fixture.prepare(&launch)
        };

        assert!(with_source(&inside).is_ok());
        assert!(matches!(
            with_source(&outside),
            Err(NativeLaunchError::ArtifactOutsideRoot { .. })
        ));
    }

    #[test]
    fn split_model_argument_resolves_to_its_first_shard_under_the_host_directory() {
        let fixture = Fixture::new();
        let config = NativeRuntimeConfig {
            store_root: PathBuf::from("/nix/store"),
            ..fixture.config.clone()
        };
        let directory = fixture.config.approved_artifact_root.join("split");
        let mut launch = llama_cpp_apple_launch(&directory);
        launch.spec.arguments[1] =
            WorkloadValue::parse("{artifact:model}/Q4/model-00001-of-00002.gguf")
                .expect("valid template");

        let command = resolve_command(&launch.spec, &config, &ports()).expect("resolved command");

        assert_eq!(
            command.arguments[..2],
            [
                "--model".to_owned(),
                format!("{}/Q4/model-00001-of-00002.gguf", directory.display()),
            ]
        );
    }

    #[test]
    fn every_declared_port_needs_its_own_assigned_port() {
        let fixture = Fixture::new();
        let launch = fixture.launch();
        let resolve = |ports: &[(&str, u16)]| {
            resolve_command(
                &launch.spec,
                &fixture.config,
                &ports
                    .iter()
                    .map(|(name, port)| ((*name).to_owned(), *port))
                    .collect(),
            )
        };
        let mut two_ports = launch.clone();
        two_ports.spec.ports.push(WorkloadPort {
            name: "metrics".to_owned(),
            container_port: 9090,
        });

        assert!(matches!(
            resolve(&[]),
            Err(NativeLaunchError::UnassignedPort { .. })
        ));
        assert!(matches!(
            resolve(&[("http", 0)]),
            Err(NativeLaunchError::UnassignedPort { .. })
        ));
        assert!(matches!(
            resolve(&[("http", 41_234), ("metrics", 41_235)]),
            Err(NativeLaunchError::UndeclaredPort { .. })
        ));
        assert!(matches!(
            resolve_command(
                &two_ports.spec,
                &fixture.config,
                &BTreeMap::from([("http".to_owned(), 41_234), ("metrics".to_owned(), 41_234)]),
            ),
            Err(NativeLaunchError::SharedHostPort { port: 41_234 })
        ));
        assert_eq!(
            resolve(&[("http", 52_001)]).expect("resolved").arguments[4],
            "--port=52001"
        );
    }

    #[test]
    fn launches_for_another_site_node_or_without_a_correlation_are_rejected() {
        let fixture = Fixture::new();
        let mut other_node = fixture.launch();
        other_node.execution_target.managed_node_id = SiteNodeId::for_test("node-b");
        let mut zero = fixture.launch();
        zero.correlation.reconcile_generation = 0;
        let mut invalid_owner = fixture.launch();
        invalid_owner.correlation.owner_id = "Owner_1".to_owned();

        assert!(matches!(
            fixture.prepare(&other_node),
            Err(NativeLaunchError::OtherSiteNode { .. })
        ));
        for launch in [zero, invalid_owner] {
            assert!(matches!(
                fixture.prepare(&launch),
                Err(NativeLaunchError::InvalidCorrelation { .. })
            ));
        }
    }

    #[test]
    fn llama_cpp_apple_workload_resolves_to_the_store_server_host_model_and_loopback_port() {
        let fixture = Fixture::new();
        let config = NativeRuntimeConfig {
            store_root: PathBuf::from("/nix/store"),
            ..fixture.config.clone()
        };
        let launch = llama_cpp_apple_launch(&fixture.artifact);

        let command = resolve_command(&launch.spec, &config, &ports()).expect("resolved command");

        assert_eq!(
            command.executable,
            format!(
                "{}/bin/llama-server",
                dotcore_domain::test_support::nix::STORE_PATH
            )
        );
        assert_eq!(
            command.arguments,
            [
                "--model",
                fixture.artifact.to_str().unwrap(),
                "--alias",
                "fixture-model",
                "--host",
                "127.0.0.1",
                "--port",
                "41234",
                "--ctx-size",
                "1",
                "--n-gpu-layers",
                "99",
                "--load-mode",
                "none",
                "--cache-ram",
                "2048",
            ]
        );
        assert!(command.environment.is_empty());
    }

    #[test]
    fn launches_requiring_the_apple_gpu_keep_it_in_their_owner_record() {
        let fixture = Fixture::new();
        let server = PathBuf::from(&fixture.store_path).join("bin/llama-server");
        fs::copy(
            PathBuf::from(&fixture.store_path).join("bin/server"),
            &server,
        )
        .unwrap();
        let mut launch = llama_cpp_apple_launch(&fixture.artifact);
        // The temporary store stands in for `/nix/store`, which a test cannot write to.
        launch.spec.executable.artifact =
            WorkloadExecutableArtifact::NixStorePath(fixture.store_path.clone());
        let launch = WorkloadLaunch::new("deployment-one", launch.execution_target, launch.spec);

        let prepared = fixture.prepare(&launch).expect("prepare launch");

        assert_eq!(prepared.spec.executable, server);
        let record = NativeOwnerRecord::decode(&prepared.spec.owner).expect("owner record");
        assert_eq!(
            record.launch.execution_target.accelerator_device_ids,
            ["apple-gpu-0"]
        );
        assert_eq!(record.launch.spec.accelerator_device_ids, ["apple-gpu-0"]);
        assert_eq!(record.launch.spec.required_accelerator_device_count, 1);
    }

    /// Temporary site directory, store with one executable package, and artifact root.
    pub(in crate::native) struct Fixture {
        pub(in crate::native) root: TempDir,
        pub(in crate::native) config: NativeRuntimeConfig,
        pub(in crate::native) state_root: OpenedStateRoot,
        pub(in crate::native) store_path: String,
        pub(in crate::native) artifact: PathBuf,
    }

    impl Fixture {
        pub(in crate::native) fn new() -> Self {
            let root = tempfile::tempdir().expect("fixture directory");
            let store_root = root.path().join("store");
            let store_path = store_root.join(format!("{HASH}-fake"));
            fs::create_dir_all(store_path.join("bin")).unwrap();
            let server = store_path.join("bin/server");
            fs::write(&server, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();
            let artifact_root = root.path().join("artifacts");
            fs::create_dir(&artifact_root).unwrap();
            let artifact = artifact_root.join("model.bin");
            fs::write(&artifact, "weights").unwrap();
            let config = NativeRuntimeConfig {
                state_root: NativeStateRoot::for_site(root.path().join("site")),
                store_root,
                approved_artifact_root: artifact_root,
                site_id: "site-one".to_owned(),
                site_node_id: SiteNodeId::for_test("node-a"),
                execution_target_id: "mac-gpu".to_owned(),
                lifetime: NativeWorkloadLifetime::Persistent,
            };
            let state_root = config.state_root.open("mac-gpu").expect("state root");
            Self {
                store_path: store_path.to_str().unwrap().to_owned(),
                root,
                config,
                state_root,
                artifact,
            }
        }

        pub(in crate::native) fn launch(&self) -> WorkloadLaunch {
            launch(&self.store_path, &self.artifact)
        }

        pub(in crate::native) fn prepare(
            &self,
            launch: &WorkloadLaunch,
        ) -> Result<PreparedLaunch, NativeLaunchError> {
            prepare_launch(
                &self.config,
                &self.state_root,
                launch,
                ports(),
                Lifetime::Independent,
            )
        }
    }

    /// Launch the inference controller materializes for the Apple Metal llama.cpp fixture on the
    /// node's Apple GPU, with its model artifact at `artifact`.
    fn llama_cpp_apple_launch(artifact: &Path) -> WorkloadLaunch {
        let target = ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-a"),
            accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
        };
        let spec = WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::NixStorePath(
                    dotcore_domain::test_support::nix::STORE_PATH.to_owned(),
                ),
                path: "bin/llama-server".to_owned(),
            },
            required_platform: dotcore_domain::ExecutionPlatform::darwin_arm64(),
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
                "99",
                "--load-mode",
                "none",
                "--cache-ram",
                "2048",
            ]
            .map(|value| WorkloadValue::parse(value).expect("valid template"))
            .into(),
            environment: BTreeMap::new(),
            artifacts: vec![WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: artifact.to_path_buf(),
                destination: "/opt/dotcore/models/model.gguf".to_owned(),
                read_only: true,
            }],
            storage: Vec::new(),
            ports: vec![WorkloadPort {
                name: "http".to_owned(),
                container_port: 8080,
            }],
            accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        };
        WorkloadLaunch::new("deployment-one", target, spec)
    }
}
