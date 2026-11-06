//! Proves that a Mac worker joined to a separate manager process hosts development-source Nix
//! engines: it pins the engine's store path itself, and its native runtime serves the deployment
//! through the manager's inference route.
//!
//! One scenario places the deployment on the worker's simulated darwin target, which pins and
//! reports the store path without building it, and proves the pin survives pause, resume, and a
//! worker restart while the manager, which has no Nix, only records it. The other places it on
//! the worker's native Apple target with a fake `llama-server` added to the host's Nix store,
//! and proves peer dispatch reaches the engine and a worker restart re-adopts it without
//! restarting it. Both need the worker's loopback alias, and the native one also needs Nix.

#![cfg(target_os = "macos")]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use dotcore_launcher::workload::{LAUNCH_FILE, LauncherRecord, ProcessRecord, StoredRecord};
use dotcore_proto::dotcore::{
    deployment::v1::NodeResolvedLaunchIdentity,
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ListModelDeploymentsRequest, ModelDeployment, ModelDeploymentExposure,
        ModelDeploymentStatus, PauseModelRequest, RemoveModelDeploymentRequest, ResumeModelRequest,
        create_model_deployment_request, model_deployment_exposure,
        model_deployment_service_client::ModelDeploymentServiceClient, model_deployment_snapshot,
    },
    site::{store::v1::NodeCurrentState, v1::NodeObservationState},
};
use prost::Message as _;
use support::{ProcessTestControls, TwoProcessObservationHarness, WorkerModelTarget};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::transport::Channel;

use crate::support;

const MILESTONE_DEADLINE: Duration = Duration::from_secs(15);
/// Deadline for a native deployment to become ready: launch, port settle, and readiness probing
/// of a fake engine started from the test executable on a possibly busy Mac.
const NATIVE_READY_DEADLINE: Duration = Duration::from_secs(60);
const PROCESS_EXIT_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Direct route through which the manager exposes the worker-hosted deployment.
const ROUTE_NAME: &str = "worker-native-model";
/// Package of the development source (`self`) that the fixture engine release declares.
const NIX_ENGINE_PACKAGE: &str = "engines.llama-cpp.native";
/// Contents of the fixture model weights, whose SHA-256 the fixture catalog declares.
const FAKE_WEIGHTS: &str = "dotcore fake weights\n";
/// Hugging Face coordinates of the fixture model weights.
const WEIGHTS_REPOSITORY_DIRECTORY: &str = "models--dotcore--fixture";
const WEIGHTS_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const WEIGHTS_FILE: &str = "fixture-Q8_0.gguf";
/// Environment variable carrying the fake `llama-server` arguments into the test executable.
const FAKE_LLAMA_SERVER_ARGS: &str = "DOTCORE_FAKE_LLAMA_SERVER_ARGS";
/// Nix events that only a node evaluating or building a development source emits.
const NIX_EVENTS: [&str; 3] = [
    "nix_engine_artifact_evaluating",
    "nix_engine_build_started",
    "nix_engine_artifact_published",
];

#[tokio::test]
async fn a_worker_pins_its_development_source_nix_engine_once_across_its_lifecycle() {
    let Some(mut harness) = TwoProcessObservationHarness::start_with_worker_target(
        ProcessTestControls {
            site_catalog: support::DARWIN_NIX_ENGINE_CATALOG,
            ..ProcessTestControls::default()
        },
        ProcessTestControls::default(),
        WorkerModelTarget::SimulatedDarwin,
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");

    let deployment_id =
        create_worker_deployment(&mut models, WorkerModelTarget::SimulatedDarwin).await;
    let ready =
        wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;

    // The worker retained the pin before realizing; the manager recorded it once and exposes it
    // beside the immutable snapshot, which keeps the development-source engine unpinned. The
    // simulated target pins a synthetic store path, never evaluating the development source.
    let [
        NodeResolvedLaunchIdentity {
            selection_key,
            launch_identity: pinned,
        },
    ] = ready.node_resolved_launch_identities.as_slice()
    else {
        panic!("one pin: {:?}", ready.node_resolved_launch_identities);
    };
    assert_eq!(selection_key, "engine");
    assert!(
        pinned.ends_with(&format!(
            "-simulated-{}",
            NIX_ENGINE_PACKAGE.replace('.', "-")
        )),
        "{pinned}"
    );
    let Some(model_deployment_snapshot::Source::Local(snapshot)) = ready
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    else {
        panic!("local snapshot");
    };
    assert_eq!(
        snapshot
            .engine_artifact
            .as_ref()
            .map(|engine| engine.launch_identity.as_str()),
        Some("")
    );
    assert_eq!(
        worker_engine_pins(&harness, &deployment_id),
        [pinned.as_str()]
    );

    models
        .pause_model(PauseModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts pause");
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Paused).await;
    models
        .resume_model(ResumeModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts resume");
    wait_for_worker_status(
        harness.worker().node_state_path(),
        &deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    // A restarted worker realizes the retained pin again without resolving it.
    harness.worker_mut().stop();
    support::wait_for_worker_observation(manager_address, NodeObservationState::Disconnected).await;
    harness.worker_mut().restart();
    support::wait_for_worker_connection(manager_address).await;
    wait_for_worker_status(
        harness.worker().node_state_path(),
        &deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    assert_eq!(
        worker_engine_pins(&harness, &deployment_id),
        [pinned.as_str()]
    );
    let restarted =
        wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;
    assert_eq!(
        restarted.node_resolved_launch_identities,
        ready.node_resolved_launch_identities
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_manager_absence(&mut models, &deployment_id).await;
    wait_for_worker_cleanup_retirement(harness.worker().node_state_path(), &deployment_id).await;
    assert!(
        worker_engine_pins(&harness, &deployment_id).is_empty(),
        "cleanup removes the retained pin with the deployment"
    );

    harness.worker_mut().shutdown();
    harness.manager_mut().shutdown();
    let worker_records = support::retained_json_records(harness.worker().site_root());
    assert_eq!(
        event_count(
            &worker_records,
            "model_engine_launch_identity_pinned",
            &deployment_id
        ),
        1,
        "the worker pins the store path once across pause, resume, and restart"
    );
    let manager_records = support::retained_json_records(harness.manager().site_root());
    assert_eq!(
        manager_records
            .iter()
            .filter(|record| {
                record["fields"]["event_kind"] == "manager_model_launch_identity"
                    && record["fields"]["outcome"] == "recorded"
            })
            .count(),
        1,
        "the manager records the pin once"
    );
    assert_eq!(
        nix_event_kinds(&manager_records),
        Vec::<String>::new(),
        "the manager never evaluates or builds Nix"
    );
    assert_eq!(
        nix_event_kinds(&worker_records),
        Vec::<String>::new(),
        "a simulated target pins without building"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_worker_serves_a_native_engine_through_the_manager_and_keeps_it_across_a_restart() {
    if !support::worker_loopback_address_available() {
        return;
    }
    let Some(engine) = FakeNixEngine::add_to_store() else {
        return;
    };
    // The worker process, and every restart of it, keeps the pin for its whole life.
    let store_path: &'static str = engine.store_path.clone().leak();
    let Some(mut harness) = TwoProcessObservationHarness::start_with_worker_target(
        ProcessTestControls {
            site_catalog: support::DARWIN_NIX_ENGINE_CATALOG,
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            local_development_store_path: Some((NIX_ENGINE_PACKAGE, store_path)),
            native_host: true,
            ..ProcessTestControls::default()
        },
        WorkerModelTarget::AppleNative,
    ) else {
        return;
    };
    // Dropped before the harness, so a failed run never leaves an independent engine running.
    let _leftovers = NativeProcessGuard {
        state_root: native_state_root(harness.worker().site_root()),
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    let cache_root = harness
        .worker()
        .site_root()
        .join("cache/model-artifacts/v1");
    seed_fake_weights(&cache_root);
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");

    let deployment_id = create_worker_deployment(&mut models, WorkerModelTarget::AppleNative).await;
    let ready = wait_for_manager_status_within(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        NATIVE_READY_DEADLINE,
    )
    .await;
    assert_eq!(
        ready.node_resolved_launch_identities,
        [NodeResolvedLaunchIdentity {
            selection_key: "engine".to_owned(),
            launch_identity: engine.store_path.clone(),
        }]
    );

    // The manager dispatches to the worker, whose engine read the weights the worker prepared.
    let (status, body) = wait_for_chat(manager_inference, ROUTE_NAME).await;
    assert_eq!(status, 200, "chat through the manager: {body}");
    assert_eq!(body["model"], ROUTE_NAME);
    assert_eq!(
        body["choices"][0]["message"]["content"],
        fake_engine_reply()
    );
    let workload = NativeWorkload::find(&native_state_root(harness.worker().site_root()));
    workload.assert_running();

    // An independent engine outlives the worker process, and the restarted worker re-adopts it.
    harness.worker_mut().stop();
    support::wait_for_worker_observation(manager_address, NodeObservationState::Disconnected).await;
    workload.assert_running();
    harness.worker_mut().restart();
    support::wait_for_worker_connection(manager_address).await;
    assert!(
        wait_for_worker_status_within(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Ready,
            NATIVE_READY_DEADLINE,
        )
        .await,
        "the restarted worker reports the re-adopted deployment Ready"
    );
    let (status, body) = wait_for_chat(manager_inference, ROUTE_NAME).await;
    assert_eq!(status, 200, "chat after the worker restart: {body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        fake_engine_reply()
    );
    assert_eq!(
        NativeWorkload::find(&native_state_root(harness.worker().site_root())),
        workload,
        "the engine and its launcher survive the worker restart unchanged"
    );
    workload.assert_running();

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_manager_absence(&mut models, &deployment_id).await;
    workload.wait_for_exit().await;
    wait_for_worker_cleanup_retirement(harness.worker().node_state_path(), &deployment_id).await;
    assert_eq!(
        workload_directories(&native_state_root(harness.worker().site_root())),
        Vec::<PathBuf>::new(),
        "teardown removes every native workload directory"
    );

    harness.worker_mut().shutdown();
    harness.manager_mut().shutdown();
    let worker_records = support::retained_json_records(harness.worker().site_root());
    assert_eq!(
        event_count(
            &worker_records,
            "model_engine_launch_identity_pinned",
            &deployment_id
        ),
        1,
        "the worker pins the store path once across its restart"
    );
    assert!(
        nix_event_kinds(&worker_records).is_empty(),
        "the worker roots the store path already in its store without evaluating or building it"
    );
    assert!(
        nix_event_kinds(&support::retained_json_records(
            harness.manager().site_root()
        ))
        .is_empty(),
        "the manager never evaluates or builds Nix"
    );
}

/// Serves `llama-server`'s readiness and Chat endpoints when the fake engine's store script runs
/// the test executable; otherwise returns at once.
#[test]
#[ignore = "fake llama-server entry point of the native scenario"]
fn fake_llama_server_helper() {
    let Ok(arguments) = std::env::var(FAKE_LLAMA_SERVER_ARGS) else {
        return;
    };
    let arguments = arguments.split_whitespace().collect::<Vec<_>>();
    let value = |flag: &str| {
        arguments
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].to_owned())
            .unwrap_or_else(|| panic!("fake llama-server needs {flag}: {arguments:?}"))
    };
    let alias = value("--alias");
    let weights = fs::read_to_string(value("--model")).expect("fake llama-server weights");
    let port = value("--port")
        .parse::<u16>()
        .expect("fake llama-server port");
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("fake llama-server listener");
    println!("fake llama-server serving {alias} on port {port}");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let path = read_http_request_path(&mut stream);
        let body = match path.as_str() {
            "/health" => r#"{"status":"ok"}"#.to_owned(),
            "/v1/models" => {
                serde_json::json!({"object": "list", "data": [{"id": alias}]}).to_string()
            }
            "/v1/chat/completions" => serde_json::json!({
                "id": "chatcmpl-fake",
                "object": "chat.completion",
                "created": 1,
                "model": alias,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": format!("{alias}: {}", weights.trim())},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string(),
            _ => String::new(),
        };
        let status = if body.is_empty() {
            "404 Not Found"
        } else {
            "200 OK"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
    }
}

/// Reply the fake engine gives every Chat request: the model alias and the weights it read.
fn fake_engine_reply() -> String {
    format!("fixture-nix-model: {}", FAKE_WEIGHTS.trim())
}

/// Fake `llama-server` added to the host's Nix store: a script running this test executable's
/// [`fake_llama_server_helper`] entry point with the engine arguments.
struct FakeNixEngine {
    store_path: String,
    _source: tempfile::TempDir,
}

impl FakeNixEngine {
    /// Adds the fake engine to the Nix store, or returns `None`, after printing a notice, when
    /// this host has no usable `nix-store`.
    fn add_to_store() -> Option<Self> {
        let source = support::canonical_tempdir().expect("fake engine source directory");
        let package = source.path().join("dotcore-fake-llama-server");
        let bin = package.join("bin");
        fs::create_dir_all(&bin).expect("fake engine bin directory");
        let executable = std::env::current_exe().expect("integration-test executable");
        // The launcher clears the environment, so the script passes the engine arguments on.
        let script = format!(
            "#!/bin/sh\nexport {FAKE_LLAMA_SERVER_ARGS}=\"$*\"\nexec '{}' --ignored --exact remote_native_model_lifecycle::fake_llama_server_helper --nocapture --test-threads=1\n",
            executable.display()
        );
        let server = bin.join("llama-server");
        fs::write(&server, script).expect("fake llama-server script");
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755))
            .expect("fake llama-server mode");
        let output = match Command::new("nix-store")
            .arg("--add")
            .arg(&package)
            .output()
        {
            Ok(output) if output.status.success() => output,
            result => {
                eprintln!(
                    "SKIPPED: this host cannot add the fake engine to its Nix store ({result:?})"
                );
                return None;
            }
        };
        let store_path = String::from_utf8(output.stdout)
            .expect("UTF-8 store path")
            .trim()
            .to_owned();
        assert!(
            store_path.starts_with("/nix/store/")
                && store_path.ends_with("-dotcore-fake-llama-server"),
            "unexpected fake engine store path {store_path:?}"
        );
        Some(Self {
            store_path,
            _source: source,
        })
    }
}

/// Places the fixture weights in the worker's Hugging Face cache, so preparation verifies them
/// against the declared digest without downloading.
fn seed_fake_weights(cache_root: &Path) {
    let snapshot = cache_root
        .join(WEIGHTS_REPOSITORY_DIRECTORY)
        .join("snapshots")
        .join(WEIGHTS_REVISION);
    fs::create_dir_all(&snapshot).expect("fixture weights snapshot directory");
    fs::write(snapshot.join(WEIGHTS_FILE), FAKE_WEIGHTS).expect("fixture weights");
}

/// Launcher and engine process identities of the worker's one native workload.
#[derive(Debug, PartialEq, Eq)]
struct NativeWorkload {
    directory: PathBuf,
    launcher: ProcessIdentity,
    engine: ProcessIdentity,
}

impl NativeWorkload {
    /// Reads the records of the only workload under `state_root`.
    fn find(state_root: &Path) -> Self {
        let directories = workload_directories(state_root);
        let [directory] = directories.as_slice() else {
            panic!("expected one native workload, found {directories:?}");
        };
        let launcher = read_record::<LauncherRecord>(directory).expect("launcher record");
        let engine = read_record::<ProcessRecord>(directory).expect("engine process record");
        Self {
            directory: directory.clone(),
            launcher: ProcessIdentity {
                pid: launcher.pid,
                start_token: launcher.start_token,
            },
            engine: ProcessIdentity {
                pid: engine.pid,
                start_token: engine.start_token,
            },
        }
    }

    fn assert_running(&self) {
        assert!(self.launcher.is_running(), "launcher {:?}", self.launcher);
        assert!(self.engine.is_running(), "engine {:?}", self.engine);
    }

    async fn wait_for_exit(&self) {
        tokio::time::timeout(PROCESS_EXIT_DEADLINE, async {
            while self.launcher.is_running() || self.engine.is_running() {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("native workload processes still run: {self:?}"));
    }
}

/// One host process, told apart from a later process reusing its pid by its start token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_token: u64,
}

impl ProcessIdentity {
    fn is_running(self) -> bool {
        dotcore_host_process::start_token(self.pid).expect("process start token")
            == Some(self.start_token)
    }

    fn kill(self) {
        if self.is_running() {
            // The start token check above guards against signalling a reused pid.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Kills the launcher and engine of every workload left under the native state root.
struct NativeProcessGuard {
    state_root: PathBuf,
}

impl Drop for NativeProcessGuard {
    fn drop(&mut self) {
        for directory in workload_directories(&self.state_root) {
            let launcher =
                read_record::<LauncherRecord>(&directory).map(|record| ProcessIdentity {
                    pid: record.pid,
                    start_token: record.start_token,
                });
            let engine = read_record::<ProcessRecord>(&directory).map(|record| ProcessIdentity {
                pid: record.pid,
                start_token: record.start_token,
            });
            for process in [launcher, engine].into_iter().flatten() {
                eprintln!("killing leftover native process {process:?}");
                process.kill();
            }
        }
    }
}

fn native_state_root(site_root: &Path) -> PathBuf {
    site_root.join("runtime/native")
}

/// Returns every workload directory under `state_root`: a directory holding a launch record.
fn workload_directories(state_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![state_root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        if directory.join(LAUNCH_FILE).is_file() {
            found.push(directory);
            continue;
        }
        pending.extend(
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path()),
        );
    }
    found.sort();
    found
}

fn read_record<R: StoredRecord>(directory: &Path) -> Option<R> {
    let bytes = fs::read(directory.join(R::FILE)).ok()?;
    Some(R::decode(&bytes).expect("valid launcher-owned record"))
}

/// Creates the fixture deployment on the worker's `target`, exposed at [`ROUTE_NAME`].
async fn create_worker_deployment(
    models: &mut ModelDeploymentServiceClient<Channel>,
    target: WorkerModelTarget,
) -> String {
    models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "fixture-nix-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(support::exact_placement(
                        support::WORKER_NODE_ID,
                        &target.execution_target_id(),
                    )),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            )),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: ROUTE_NAME.to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("manager accepts a worker-hosted development-source Nix engine")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id
}

fn worker_engine_pins(harness: &TwoProcessObservationHarness, deployment_id: &str) -> Vec<String> {
    support::retained_node_state(harness.worker().node_state_path())
        .model_resolved_launch_identities
        .into_iter()
        .filter(|record| record.deployment_id == deployment_id)
        .flat_map(|record| record.resolved_launch_identities)
        .map(|pin| pin.launch_identity)
        .collect()
}

fn event_count(records: &[serde_json::Value], event_kind: &str, deployment_id: &str) -> usize {
    records
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == event_kind
                && record["fields"]["deployment_id"] == deployment_id
        })
        .count()
}

/// Returns the kinds of every retained Nix evaluation or build event, in order.
fn nix_event_kinds(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| record["fields"]["event_kind"].as_str())
        .filter(|kind| NIX_EVENTS.contains(kind))
        .map(str::to_owned)
        .collect()
}

async fn wait_for_manager_status(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
) -> ModelDeployment {
    wait_for_manager_status_within(models, deployment_id, expected, MILESTONE_DEADLINE).await
}

/// Polls the manager list until the deployment reaches `expected`, reporting the last listed
/// deployment on timeout.
async fn wait_for_manager_status_within(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    deadline: Duration,
) -> ModelDeployment {
    let mut last = None;
    let reached = tokio::time::timeout(deadline, async {
        loop {
            let deployment = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model status")
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id);
            if let Some(deployment) = deployment.as_ref().filter(|deployment| {
                deployment
                    .state
                    .as_ref()
                    .is_some_and(|state| state.status == expected as i32)
            }) {
                return deployment.clone();
            }
            last = deployment;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    reached.unwrap_or_else(|_| {
        panic!(
            "manager status deadline waiting for {expected:?}; last listed: {:?}",
            last.and_then(|deployment| deployment.state)
        )
    })
}

async fn wait_for_manager_absence(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let deployments = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model list after teardown")
                .into_inner()
                .deployments;
            if deployments
                .iter()
                .all(|deployment| deployment.model_deployment_id != deployment_id)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager teardown finalization deadline");
}

async fn wait_for_worker_cleanup_retirement(state_path: PathBuf, deployment_id: &str) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            if decoded_node_state(&state_path).is_some_and(|state| {
                state
                    .model_deployments
                    .iter()
                    .all(|record| record.model_deployment_id != deployment_id)
                    && state
                        .model_cleanup_evidence
                        .iter()
                        .all(|evidence| evidence.model_deployment_id != deployment_id)
            }) {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker cleanup receipt deadline");
}

async fn wait_for_worker_status(
    state_path: PathBuf,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
) {
    assert!(
        wait_for_worker_status_within(state_path, deployment_id, expected, MILESTONE_DEADLINE)
            .await,
        "worker status deadline waiting for {expected:?}"
    );
}

async fn wait_for_worker_status_within(
    state_path: PathBuf,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    deadline: Duration,
) -> bool {
    tokio::time::timeout(deadline, async {
        loop {
            if decoded_node_state(&state_path)
                .and_then(|state| {
                    state
                        .model_deployments
                        .into_iter()
                        .find(|record| record.model_deployment_id == deployment_id)
                })
                .and_then(|record| record.state)
                .is_some_and(|state| state.status == expected as i32)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .is_ok()
}

fn decoded_node_state(state_path: &Path) -> Option<NodeCurrentState> {
    fs::read(state_path)
        .ok()
        .and_then(|bytes| NodeCurrentState::decode(bytes.as_slice()).ok())
}

/// Sends Chat requests through `route` until one succeeds, since the manager's route can lag the
/// deployment's readiness, and right after a worker restart can still name its previous endpoint.
async fn wait_for_chat(address: std::net::SocketAddr, route: &str) -> (u16, serde_json::Value) {
    let mut last = None;
    let answered = tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let (status, body) = chat(address, route).await;
            if status == 200 {
                return (status, body);
            }
            last = Some((status, body));
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    answered.unwrap_or_else(|_| panic!("chat deadline; last answer: {last:?}"))
}

/// Sends one non-streaming Chat request through `route` and returns its status and JSON body.
async fn chat(address: std::net::SocketAddr, route: &str) -> (u16, serde_json::Value) {
    let body = format!(
        r#"{{"model":"{route}","stream":false,"messages":[{{"role":"user","content":"hello"}}]}}"#
    );
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("inference connection");
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("chat request head");
    stream
        .write_all(body.as_bytes())
        .await
        .expect("chat request body");
    let mut response = Vec::new();
    tokio::time::timeout(MILESTONE_DEADLINE, stream.read_to_end(&mut response))
        .await
        .expect("chat response deadline")
        .expect("chat response");
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("chat response headers");
    let status = String::from_utf8_lossy(&response[..end])
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("chat response status");
    let body = serde_json::from_slice(&response[end + 4..]).expect("chat JSON body");
    (status, body)
}

/// Reads one whole HTTP request, headers and `Content-Length` body, and returns its path.
fn read_http_request_path(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = std::io::Read::read(stream, &mut buffer).unwrap_or(0);
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let length = String::from_utf8_lossy(&request[..end])
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return String::from_utf8_lossy(&request[..end])
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
        }
    }
    String::new()
}
