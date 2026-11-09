//! Starts real launcher-held workloads inside a test site directory, so site removal tests can
//! check that cleanup stops their processes. Each workload watches the test process, so it stops
//! on its own even when a test fails before cleaning up.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use dotcore_domain::{SiteNodeId, real_time};
use dotcore_launcher::{
    supervisor::Supervisor,
    workload::{Lifetime, PROCESS_FILE, ProcessRecord, StoredRecord},
};
use dotcore_runtime::{
    ExecutionTarget, RuntimeBackend, WorkloadArtifactBinding, WorkloadExecutable,
    WorkloadExecutableArtifact, WorkloadId, WorkloadIpcMode, WorkloadLaunch, WorkloadPort,
    WorkloadSpec, WorkloadValue,
};
use tempfile::TempDir;

use super::{
    NativeRuntimeBackend, NativeRuntimeConfig, NativeStateRoot, NativeWorkloadLifetime,
    launch::{prepare_launch, workload_id},
};

/// Valid Nix store entry hash used by test store paths.
pub const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// SiteNode every test workload records.
pub const TEST_SITE_NODE: &str = "node-a";

/// Execution target every test workload runs on.
pub const TEST_EXECUTION_TARGET: &str = "mac-gpu";

/// Fake store and artifact roots for test workloads, plus the backend that created them.
pub struct NativeTestHost {
    _roots: TempDir,
    store_path: String,
    artifact: PathBuf,
    config: NativeRuntimeConfig,
    backend: NativeRuntimeBackend,
}

impl NativeTestHost {
    /// Prepares a fake engine that runs until stopped and a backend whose workloads record
    /// `site_id`, under the native state root of `site_root`.
    pub fn new(site_root: &Path, site_id: &str) -> Self {
        let roots = tempfile::tempdir().expect("native test roots");
        let store_root = roots.path().join("store");
        let store_path = store_root.join(format!("{HASH}-fake"));
        fs::create_dir_all(store_path.join("bin")).expect("fake store entry");
        let engine = store_path.join("bin/server");
        // The launcher clears the environment, so the script carries this test's `PATH`.
        let path = std::env::var("PATH").expect("PATH");
        fs::write(
            &engine,
            format!("#!/bin/sh\nPATH='{path}'\ntrap 'exit 0' TERM\nwhile :; do sleep 0.05; done\n"),
        )
        .expect("fake engine");
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).expect("engine mode");
        let artifact_root = roots.path().join("artifacts");
        fs::create_dir(&artifact_root).expect("artifact root");
        let artifact = artifact_root.join("model.bin");
        fs::write(&artifact, "weights").expect("artifact");
        let config = NativeRuntimeConfig {
            state_root: NativeStateRoot::for_site(site_root),
            store_root,
            approved_artifact_root: artifact_root,
            site_id: site_id.to_owned(),
            site_node_id: SiteNodeId::parse(TEST_SITE_NODE).expect("node"),
            execution_target_id: TEST_EXECUTION_TARGET.to_owned(),
            lifetime: NativeWorkloadLifetime::Ephemeral,
        };
        // The fake engine never exits on its own, so a create would otherwise watch it for the
        // whole production settle window.
        let backend = NativeRuntimeBackend::new(config.clone(), launcher_binary())
            .expect("native backend")
            .with_port_settle_window(
                dotcore_domain::timing::Timing::test().native_port_settle_window,
            );
        Self {
            _roots: roots,
            store_path: store_path.to_str().expect("UTF-8 store path").to_owned(),
            artifact,
            config,
            backend,
        }
    }

    /// Creates the workload of `owner_id` through the backend and returns it once its engine
    /// process runs.
    pub async fn spawn(&self, owner_id: &str) -> NativeTestWorkload {
        let launch = self.launch(owner_id);
        self.backend
            .create_workload(&launch)
            .await
            .expect("create native workload");
        self.running(workload_id(&launch.correlation)).await
    }

    /// Creates the workload of `owner_id` straight through the launcher, with an owner record
    /// naming `site_id` and the given lifetime, and returns it once its engine process runs.
    pub async fn spawn_recorded(
        &self,
        owner_id: &str,
        site_id: &str,
        lifetime: Lifetime,
    ) -> NativeTestWorkload {
        let config = NativeRuntimeConfig {
            site_id: site_id.to_owned(),
            ..self.config.clone()
        };
        let state_root = config
            .state_root
            .open(TEST_EXECUTION_TARGET)
            .expect("state root");
        let prepared = prepare_launch(
            &config,
            &state_root,
            &self.launch(owner_id),
            ports(),
            lifetime,
        )
        .expect("prepare launch");
        Supervisor::new(&state_root.target_directory, launcher_binary())
            .expect("supervisor")
            .create(&prepared.workload_id, &prepared.spec)
            .await
            .expect("create native workload");
        self.running(prepared.workload_id).await
    }

    /// Stops a workload through the backend, leaving its finished directory behind.
    pub async fn stop(&self, workload: &NativeTestWorkload) {
        self.backend
            .stop_workload(&WorkloadId(workload.id.clone()))
            .await
            .expect("stop native workload");
    }

    /// Lifetime that watches this test process, as an ephemeral backend's workloads do.
    pub fn watching_this_process(&self) -> Lifetime {
        self.backend.supervisor.watch_this_process()
    }

    fn launch(&self, owner_id: &str) -> WorkloadLaunch {
        let template = launch(&self.store_path, &self.artifact);
        WorkloadLaunch::new(owner_id, template.execution_target, template.spec)
    }

    async fn running(&self, id: String) -> NativeTestWorkload {
        let directory = self.backend.workload_directory(&id);
        let process = directory.join(PROCESS_FILE);
        wait_until(|| process.is_file()).await;
        let record = ProcessRecord::decode(&fs::read(&process).expect("process record"))
            .expect("decode process record");
        NativeTestWorkload {
            id,
            directory,
            engine_pid: record.pid,
            engine_start_token: record.start_token,
        }
    }
}

/// One running test workload and its held engine process.
pub struct NativeTestWorkload {
    pub id: String,
    pub directory: PathBuf,
    engine_pid: u32,
    engine_start_token: u64,
}

impl NativeTestWorkload {
    /// Whether the engine process this workload started still runs.
    pub fn engine_runs(&self) -> bool {
        dotcore_host_process::start_token(self.engine_pid)
            .expect("read engine start token")
            .is_some_and(|token| token == self.engine_start_token)
    }
}

/// Polls `condition` until it holds, failing the test after ten seconds.
pub async fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition not met in time");
        real_time::poll_pause(Duration::from_millis(20)).await;
    }
}

/// Owner launch with every token kind, one artifact, and one declared port.
pub fn launch(store_path: &str, artifact: &Path) -> WorkloadLaunch {
    let template = |value: &str| WorkloadValue::parse(value).expect("valid template");
    WorkloadLaunch::new(
        "deployment-one",
        ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-a"),
            accelerator_device_ids: Vec::new(),
        },
        WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::NixStorePath(store_path.to_owned()),
                path: "bin/server".to_owned(),
            },
            required_platform: super::adapter::host_platform().expect("host platform"),
            arguments: vec![
                WorkloadValue::literal("--model"),
                template("{artifact:model}"),
                WorkloadValue::literal("--host"),
                template("{service:http.host}"),
                template("--port={service:http.port}"),
                WorkloadValue::literal("--options={\"json\":true}"),
            ],
            environment: BTreeMap::from([
                ("LAUNCHER".to_owned(), template("{engine.executable}")),
                ("MODE".to_owned(), WorkloadValue::literal("serve")),
            ]),
            artifacts: vec![WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: artifact.to_path_buf(),
                destination: "/opt/model.bin".to_owned(),
                read_only: true,
            }],
            storage: Vec::new(),
            ports: vec![WorkloadPort {
                name: "http".to_owned(),
                container_port: 8080,
            }],
            accelerator_device_ids: Vec::new(),
            required_accelerator_device_count: 0,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        },
    )
}

/// Assigned loopback ports of the test launch.
pub fn ports() -> BTreeMap<String, u16> {
    BTreeMap::from([("http".to_owned(), 41_234)])
}

/// Returns the `dotcore-launcher` binary of the current Cargo profile, which a workspace build
/// places beside the `deps` directory holding the test executable.
pub fn launcher_binary() -> PathBuf {
    let executable = std::env::current_exe().expect("test executable path");
    let launcher = executable
        .parent()
        .and_then(Path::parent)
        .expect("test executable inside a Cargo profile directory")
        .join("dotcore-launcher");
    assert!(
        launcher.is_file(),
        "{} is missing; build it with `cargo build -p dotcore-launcher` or run the workspace tests",
        launcher.display()
    );
    launcher
}
