//! Explicitly gated Apple Silicon tests for macOS hosts: Apple GPU observation, and llama.cpp
//! on Metal served end to end by the native runtime.
//!
//! The observation test collects real node snapshots through Metal and checks the host's one
//! Apple GPU. The end-to-end test deploys the smallest Gemma on a site with an persistent native
//! target in a separate backend process, chats through its route, and proves the engine survives
//! a backend restart. The decision test deploys clef-flash and sends decision requests through
//! the router. The largest-fit test loads a model at the largest context the Mac's unified memory
//! fits. All are ignored without the explicit hardware gate.

#![cfg(target_os = "macos")]

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use dotcore_backend::model::engine::declaration_validator;
use dotcore_domain::hardware_observation::HardwareSnapshotSource;
use dotcore_launcher::workload::{LauncherRecord, ProcessRecord, StoredRecord};
use dotcore_node_hardware::{
    apple::SystemAppleAcceleratorAdapter,
    hardware::{SystemHardwareSnapshotCollector, SystemNvidiaAcceleratorAdapter},
};
use dotcore_proto::dotcore::{
    catalog::v1::engine_parameter_value,
    deployment::v1::accelerator_memory_observation,
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ListModelDeploymentsRequest, LocalModelDeploymentConfiguration, ModelDeployment,
        ModelDeploymentExposure, ModelDeploymentStatus, RemoveModelDeploymentRequest,
        create_model_deployment_request, model_deployment_exposure,
        model_deployment_service_client::ModelDeploymentServiceClient,
    },
    observation::v1::{
        AcceleratorFacts, AcceleratorMemoryArchitecture, AcceleratorVendor, HardwareSnapshot,
        accelerator_observation, host_observation,
    },
    site::v1::NativeWorkloadLifetime,
};
use dotcore_runtime_backends::docker::capabilities::SystemDockerCapabilityProbe;
use dotcore_site::{
    initial::{
        input_store::InitialSiteInputWriter,
        inputs::InitialSiteInputs,
        probe::InitialSiteProber,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
            RandomInitialSiteIdentityGenerator, derive_model_execution_target_id,
        },
        validation::SiteInputBundleValidator,
    },
    node::target_inventory::{NodeAcceleratorFacts, NodeAcceleratorObservation},
};
use serde_json::Value;
use support::{ExternalBackendProcess, ProcessTestControls};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use tonic::transport::Channel;

use crate::{decision_requests, nvidia_qualification, support};

const MODEL_ID: &str = "gemma-4-e2b";
const PROFILE_ID: &str = "fast-start";
const MODEL_ARTIFACT_BINDING_ID: &str = "q4-k-m";
const TOTAL_CONTEXT: u64 = 8_192;
const PROMPT_CACHE_MEMORY_MIB: u64 = 2_048;
const ROUTE_NAME: &str = "gemma-4-e2b";
const APPLE_DEVICE_ID: &str = "apple-gpu-0";

/// Covers a first download of the weights and the engine build into empty caches.
const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const RESTART_TIMEOUT: Duration = Duration::from_secs(120);
const MEMORY_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

/// Lowest plausible footprint of the engine holding the Q4 weights in the shared pool.
const MINIMUM_ENGINE_FOOTPRINT_BYTES: u64 = 1024 * 1024 * 1024;

#[test]
#[ignore = "hardware: just backend::test-apple"]
fn hw_apple_gpu_is_observed_with_a_fixed_device_id_across_polls() {
    let collector = SystemHardwareSnapshotCollector::with_platform_accelerators(
        std::sync::Arc::new(dotcore_domain::clock::SystemClock),
    );

    let snapshots = (0..3)
        .map(|_| collector.collect_snapshot())
        .collect::<Vec<_>>();

    let gpus = snapshots.iter().map(apple_gpu).collect::<Vec<_>>();
    let gpu = &gpus[0];
    println!(
        "Apple GPU: model={:?} device_id={} working_set_limit_bytes={:?}",
        gpu.model, gpu.device_id, gpu.working_set_limit_bytes
    );
    assert!(
        gpus.iter().all(|polled| polled.device_id == gpu.device_id),
        "device id changed across polls: {gpus:?}"
    );
    assert_eq!(gpu.device_id, APPLE_DEVICE_ID);
    assert!(gpu.model.starts_with("Apple M"), "model {:?}", gpu.model);
    assert_eq!(
        gpu.memory_architecture(),
        AcceleratorMemoryArchitecture::Unified
    );
    assert_eq!(gpu.total_memory_bytes, None);
    assert_eq!(gpu.used_memory_bytes, None);
    assert_eq!(gpu.driver_or_runtime_version, "");
    let (host_total, host_available) = host_memory_bytes(&snapshots[0]);
    let working_set_limit = gpu.working_set_limit_bytes.expect("working-set limit");
    assert!(
        working_set_limit > 0 && working_set_limit <= host_total,
        "working-set limit {working_set_limit} outside the host pool {host_total}"
    );
    let observed_at = snapshots[0].observed_at.expect("observation time");
    let observed_at = UNIX_EPOCH
        + Duration::new(
            u64::try_from(observed_at.seconds).expect("observed after the epoch"),
            u32::try_from(observed_at.nanos).expect("valid nanoseconds"),
        );
    assert_eq!(
        NodeAcceleratorObservation::from_hardware_snapshot("node-a", &snapshots[0], false),
        NodeAcceleratorObservation::available_at(
            "node-a",
            NodeAcceleratorFacts::apple(),
            observed_at
        )
        .with_device_memory(gpu.device_id.clone(), None, None)
        .with_host_memory(host_total, host_available)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hardware: just backend::test-apple"]
async fn hw_apple_llama_cpp_on_metal_serves_chat_and_survives_a_backend_restart() {
    // Drop order matters: the backend stops first, then leftover engines, then the site files.
    let site = AppleSite::initialize().await;
    let _leftovers = NativeProcessGuard {
        state_root: site.native_state_root(),
    };
    let mut backend = ExternalBackendProcess::start(
        site.root.clone(),
        ProcessTestControls {
            native_host: true,
            shared_artifact_cache: true,
            ..Default::default()
        },
    );

    let mut models = model_client(&backend).await;
    let deployment_id = create_deployment(
        &mut models,
        &site,
        MODEL_ID,
        PROFILE_ID,
        ROUTE_NAME,
        model_configuration(),
    )
    .await;
    wait_for_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        DEPLOYMENT_TIMEOUT,
    )
    .await;
    assert_route_chat(inference_address(&backend)).await;
    let workload = NativeWorkload::find(&site.native_state_root());
    workload.assert_running();
    let memory = wait_for_process_memory(&mut models, &deployment_id).await;
    let engine_footprint = dotcore_host_process::memory_bytes(workload.engine.pid)
        .expect("engine footprint")
        .expect("running engine footprint");
    eprintln!(
        "[apple-metal] deployment memory {memory} bytes, engine footprint {engine_footprint} bytes"
    );
    assert!(
        memory >= MINIMUM_ENGINE_FOOTPRINT_BYTES,
        "deployment memory {memory} bytes is below the engine's plausible footprint"
    );

    let restart_started = Instant::now();
    backend.restart();
    let mut models = model_client(&backend).await;
    wait_for_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        RESTART_TIMEOUT,
    )
    .await;
    eprintln!(
        "[apple-metal] deployment ready {:?} after the backend restart",
        restart_started.elapsed()
    );
    let readopted = NativeWorkload::find(&site.native_state_root());
    assert_eq!(
        readopted, workload,
        "the engine and its launcher must survive the backend restart unchanged"
    );
    readopted.assert_running();
    assert_route_chat(inference_address(&backend)).await;

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("remove the deployment");
    wait_for_absence(&mut models, &deployment_id).await;
    workload.wait_for_exit().await;
    assert_eq!(
        workload_directories(&site.native_state_root()),
        Vec::<PathBuf>::new(),
        "teardown must remove every native workload directory"
    );
    backend.shutdown();
}

/// Launches the Qwen3.5 4B llama.cpp profile at the largest fit the capacity component computes
/// for the Mac's unified memory, or a smaller context that leaves the host room, and checks that
/// the host memory the load takes stays within what was available before it. While it loads, it
/// records whether the deployment reports its memory yet, the window in which capacity counts the
/// load twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "hardware: just backend::test-apple"]
async fn hw_apple_llama_cpp_loads_at_the_largest_fit() {
    let suite = "apple-largest-fit";
    let site = AppleSite::initialize().await;
    let _leftovers = NativeProcessGuard {
        state_root: site.native_state_root(),
    };
    let catalog = nvidia_qualification::load_catalog(&site.root).expect("load the site catalog");
    let mut backend = ExternalBackendProcess::start(
        site.root.clone(),
        ProcessTestControls {
            native_host: true,
            shared_artifact_cache: true,
            ..Default::default()
        },
    );
    let mut models = model_client(&backend).await;
    let case = nvidia_qualification::LargestFitCase {
        suite,
        // A small model, so the reference Mac, a laptop in daily use, has room for it.
        model_id: "qwen3-5-4b",
        profile_id: "q4-xl",
        node_id: &site.node_id,
        target_id: &site.execution_target_id,
    };
    let placement = support::exact_placement(&site.node_id, &site.execution_target_id);

    let idle_available = host_available_bytes();
    let (largest, starting) =
        nvidia_qualification::wait_for_largest_fit(&mut models, &case, &placement)
            .await
            .expect("the largest fit on the idle Mac");
    // The Mac shares its memory with the OS, so the load leaves it room like any unified GPU.
    let idle_memory = nvidia_qualification::GpuMemory::Unified {
        host_available_bytes: idle_available,
    };
    let (total_context, configuration, _, demand) = nvidia_qualification::host_safe_load(
        &mut models,
        &catalog,
        &case,
        &placement,
        &idle_memory,
        starting,
    )
    .await
    .expect("a load that leaves the host room");
    eprintln!(
        "[{suite}] largest_fit={largest} starting_total_context={starting} \
         total_context={total_context} estimate_host_bytes={demand} idle_available={idle_available}"
    );

    let started = Instant::now();
    let deployment_id = create_deployment(
        &mut models,
        &site,
        case.model_id,
        case.profile_id,
        "largest-fit",
        configuration,
    )
    .await;
    let unmeasured =
        load_without_measured_memory(&mut models, &deployment_id, idle_available).await;
    let ready_after = started.elapsed();
    let measured = wait_for_process_memory(&mut models, &deployment_id).await;
    let after_available = settled_host_available_bytes().await;
    let held = idle_available.saturating_sub(after_available);
    eprintln!(
        "[{suite}] ready_after_ms={} held={held} deployment_reading={measured} \
         after_available={after_available} estimate_minus_held_negative_is_over={} \
         loading_polls_without_reading={} largest_drop_without_reading={}",
        ready_after.as_millis(),
        i128::from(demand) - i128::from(held),
        unmeasured.polls,
        unmeasured.largest_drop,
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("remove the deployment");
    wait_for_absence(&mut models, &deployment_id).await;
    backend.shutdown();

    // The estimate fills the available memory less a headroom, so a load may pass it by less than
    // the headroom but must stay within the memory that was available.
    assert!(
        held <= idle_available,
        "[{suite}] the load took {held} B of host memory, more than the {idle_available} B available"
    );
}

/// Qualifies clef-flash on Metal through the router's decision route: a research example round
/// trip, and the client error of a request larger than the total context.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "hardware: just backend::test-apple"]
async fn hw_apple_clef_flash_serves_decisions_and_rejects_oversized_requests() {
    // Drop order matters: the backend stops first, then leftover engines, then the site files.
    let site = AppleSite::initialize().await;
    let _leftovers = NativeProcessGuard {
        state_root: site.native_state_root(),
    };
    let mut backend = ExternalBackendProcess::start(
        site.root.clone(),
        ProcessTestControls {
            native_host: true,
            shared_artifact_cache: true,
            ..Default::default()
        },
    );

    let mut models = model_client(&backend).await;
    let deployment_id = create_deployment(
        &mut models,
        &site,
        decision_requests::MODEL_ID,
        decision_requests::PROFILE_ID,
        decision_requests::MODEL_ID,
        decision_requests::decision_model_configuration(),
    )
    .await;
    wait_for_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        DEPLOYMENT_TIMEOUT,
    )
    .await;

    let address = inference_address(&backend);
    let started = Instant::now();
    let research = tokio::time::timeout(
        COMPLETION_TIMEOUT,
        route_request(
            address,
            &decision_requests::research_example_http(decision_requests::MODEL_ID),
        ),
    )
    .await
    .expect("research example deadline");
    decision_requests::assert_research_example_response(decision_requests::MODEL_ID, &research);
    eprintln!(
        "[apple-metal] research example answered in {:?}",
        started.elapsed()
    );
    let oversized = tokio::time::timeout(
        COMPLETION_TIMEOUT,
        route_request(
            address,
            &decision_requests::oversized_http(decision_requests::MODEL_ID),
        ),
    )
    .await
    .expect("oversized request deadline");
    decision_requests::assert_context_length_exceeded_response(&oversized);

    let workload = NativeWorkload::find(&site.native_state_root());
    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("remove the deployment");
    wait_for_absence(&mut models, &deployment_id).await;
    workload.wait_for_exit().await;
    backend.shutdown();
}

/// Temporary site with one persistent native Apple inference target, initialized the way site
/// initialization does it on this host.
struct AppleSite {
    root: PathBuf,
    node_id: String,
    execution_target_id: String,
    _directory: tempfile::TempDir,
}

impl AppleSite {
    async fn initialize() -> Self {
        let directory = support::canonical_tempdir().expect("temporary site directory");
        let root = directory.path().join("site");
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Apple],
            [InitialApplicationRuntime::Simulated],
        )
        .with_native_workload_lifetime(NativeWorkloadLifetime::Persistent);
        let prober = InitialSiteProber::new(
            Box::new(SystemDockerCapabilityProbe),
            Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
        )
        .with_apple_accelerator(Box::new(SystemAppleAcceleratorAdapter::for_current_host()));
        let probe = prober.probe(&request).await.expect("probe the Apple GPU");
        let identities = request
            .resolve_identities(&mut RandomInitialSiteIdentityGenerator)
            .expect("Site identities");
        let node_id = identities.node_id().to_owned();
        let execution_target_id = derive_model_execution_target_id(
            &node_id,
            InitialInferenceRuntime::Apple,
            probe.apple().map(|apple| apple.device_id()),
        );
        let inputs =
            InitialSiteInputs::compose(&request, identities, &probe).expect("Apple site inputs");
        InitialSiteInputWriter::new(SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore_backend::model::site_declaration_rules(),
        ))
        .create(&root, &inputs)
        .expect("write the Apple site");
        support::configure_observation_interval(&root, 1);
        Self {
            root,
            node_id,
            execution_target_id,
            _directory: directory,
        }
    }

    fn native_state_root(&self) -> PathBuf {
        self.root.join("runtime").join("native")
    }
}

/// Launcher and engine process identities of the site's one native workload.
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
        tokio::time::timeout(PROCESS_EXIT_TIMEOUT, async {
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

/// Kills the launcher and engine of every workload left under the native state root, so a
/// failed run never leaves an persistent engine running.
struct NativeProcessGuard {
    state_root: PathBuf,
}

impl Drop for NativeProcessGuard {
    fn drop(&mut self) {
        for directory in workload_directories(&self.state_root) {
            for process in [
                read_record::<LauncherRecord>(&directory).map(|record| ProcessIdentity {
                    pid: record.pid,
                    start_token: record.start_token,
                }),
                read_record::<ProcessRecord>(&directory).map(|record| ProcessIdentity {
                    pid: record.pid,
                    start_token: record.start_token,
                }),
            ]
            .into_iter()
            .flatten()
            {
                eprintln!("[apple-metal] killing leftover native process {process:?}");
                process.kill();
            }
        }
    }
}

/// Returns every workload directory under `state_root`: a directory holding a launch record.
fn workload_directories(state_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![state_root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        if directory
            .join(dotcore_launcher::workload::LAUNCH_FILE)
            .is_file()
        {
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
    let bytes = std::fs::read(directory.join(R::FILE)).ok()?;
    Some(R::decode(&bytes).expect("valid launcher-owned record"))
}

async fn model_client(backend: &ExternalBackendProcess) -> ModelDeploymentServiceClient<Channel> {
    let address = backend.addresses().management.expect("management listener");
    ModelDeploymentServiceClient::connect(format!("http://{address}"))
        .await
        .expect("management connection")
}

fn inference_address(backend: &ExternalBackendProcess) -> SocketAddr {
    backend.addresses().inference.expect("inference listener")
}

async fn create_deployment(
    models: &mut ModelDeploymentServiceClient<Channel>,
    site: &AppleSite,
    model_id: &str,
    profile_id: &str,
    route_name: &str,
    configuration: LocalModelDeploymentConfiguration,
) -> String {
    let deployment = models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: model_id.to_owned(),
                    profile_id: profile_id.to_owned(),
                    placement: Some(support::exact_placement(
                        &site.node_id,
                        &site.execution_target_id,
                    )),
                    configuration: Some(configuration),
                },
            )),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: route_name.to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("create the Apple deployment")
        .into_inner()
        .deployment
        .expect("created deployment");
    deployment.model_deployment_id
}

/// Deployment configuration with a smaller context than the profile to shorten loading. A request
/// carries every engine parameter once, so it sets the prompt cache bound to the profile's value.
fn model_configuration() -> LocalModelDeploymentConfiguration {
    let mut configuration =
        support::local_model_configuration(MODEL_ARTIFACT_BINDING_ID, TOTAL_CONTEXT);
    let prompt_cache = configuration
        .engine_parameter_values
        .as_mut()
        .expect("engine parameter values")
        .values
        .iter_mut()
        .find(|value| value.parameter_id == "prompt-cache-memory-mib")
        .expect("prompt cache parameter value");
    prompt_cache.value = Some(engine_parameter_value::Value::UnsignedInteger(
        PROMPT_CACHE_MEMORY_MIB,
    ));
    configuration
}

async fn deployment(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
) -> Option<ModelDeployment> {
    models
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("list deployments")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
}

/// Waits until the deployment reports `expected`, failing at once when it fails.
async fn wait_for_status(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    timeout: Duration,
) {
    let started = Instant::now();
    let mut reported = Instant::now();
    tokio::time::timeout(timeout, async {
        loop {
            let state = deployment(models, deployment_id)
                .await
                .and_then(|deployment| deployment.state)
                .expect("deployment state");
            let status = state.status();
            if status == expected {
                return;
            }
            assert_ne!(
                status,
                ModelDeploymentStatus::Failed,
                "deployment failed: {state:?}"
            );
            if reported.elapsed() >= PROGRESS_INTERVAL {
                eprintln!(
                    "[apple-metal] waiting for {expected:?} after {:?}: {status:?} {:?} {:?}",
                    started.elapsed(),
                    state.status_message,
                    state.progress
                );
                reported = Instant::now();
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deployment did not reach {expected:?} within {timeout:?}"));
}

/// Waits for the deployment's available memory figure on the Apple GPU and returns it.
async fn wait_for_process_memory(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
) -> u64 {
    let mut last = None;
    tokio::time::timeout(MEMORY_TIMEOUT, async {
        loop {
            let observation = deployment(models, deployment_id)
                .await
                .and_then(|deployment| deployment.accelerator_memory);
            if let Some(accelerator_memory_observation::Result::Available(usage)) =
                observation.as_ref().and_then(|value| value.result.as_ref())
            {
                let [device] = usage.devices.as_slice() else {
                    panic!("expected one device usage: {usage:?}");
                };
                assert_eq!(device.device_id, APPLE_DEVICE_ID);
                return device.used_memory_bytes;
            }
            last = observation;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no available deployment memory; last observation {last:?}"))
}

/// Polls taken while a deployment loads without reporting its memory, and the largest drop in
/// host available memory seen meanwhile: memory capacity counts both as the claim and as other use.
struct UnmeasuredLoad {
    polls: u32,
    largest_drop: u64,
}

/// Waits for the deployment to turn Ready, recording how long it holds host memory without a
/// memory reading of its own.
async fn load_without_measured_memory(
    models: &mut ModelDeploymentServiceClient<Channel>,
    deployment_id: &str,
    idle_available: u64,
) -> UnmeasuredLoad {
    let mut unmeasured = UnmeasuredLoad {
        polls: 0,
        largest_drop: 0,
    };
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployment = deployment(models, deployment_id)
                .await
                .expect("the loading deployment");
            let state = deployment.state.expect("deployment state");
            let status = state.status();
            assert_ne!(
                status,
                ModelDeploymentStatus::Failed,
                "deployment failed: {state:?}"
            );
            if status == ModelDeploymentStatus::Ready {
                return;
            }
            let measured = matches!(
                deployment
                    .accelerator_memory
                    .and_then(|memory| memory.result),
                Some(accelerator_memory_observation::Result::Available(_))
            );
            if !measured {
                unmeasured.polls += 1;
                unmeasured.largest_drop = unmeasured
                    .largest_drop
                    .max(idle_available.saturating_sub(host_available_bytes()));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deployment did not turn Ready within {DEPLOYMENT_TIMEOUT:?}"));
    unmeasured
}

/// Host available memory as the node's collector reads it.
fn host_available_bytes() -> u64 {
    let collector = SystemHardwareSnapshotCollector::with_platform_accelerators(
        std::sync::Arc::new(dotcore_domain::clock::SystemClock),
    );
    match collector
        .collect_snapshot()
        .host
        .and_then(|host| host.result)
    {
        Some(host_observation::Result::Facts(host)) => host.available_memory_bytes,
        other => panic!("host memory is observed: {other:?}"),
    }
}

/// Host available memory once five readings a second apart stay within 64 MiB of each other, or
/// the last reading after two minutes.
async fn settled_host_available_bytes() -> u64 {
    const MARGIN: u64 = 64 * 1024 * 1024;
    let mut previous = host_available_bytes();
    let mut steady = 0;
    let started = Instant::now();
    while steady < 5 && started.elapsed() < Duration::from_secs(120) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let current = host_available_bytes();
        steady = if current.abs_diff(previous) <= MARGIN {
            steady + 1
        } else {
            0
        };
        previous = current;
    }
    previous
}

async fn wait_for_absence(models: &mut ModelDeploymentServiceClient<Channel>, deployment_id: &str) {
    tokio::time::timeout(PROCESS_EXIT_TIMEOUT, async {
        while deployment(models, deployment_id).await.is_some() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("deployment removal deadline");
}

/// Sends one Chat Completions request through the Direct-exposure route and checks that the
/// engine generated text.
async fn assert_route_chat(inference_address: SocketAddr) {
    let started = Instant::now();
    let body = serde_json::to_vec(&serde_json::json!({
        "model": ROUTE_NAME,
        "messages": [{"role": "user", "content": "Reply with exactly READY."}],
        "max_completion_tokens": 256,
        "temperature": 0,
    }))
    .expect("chat payload");
    let mut request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let response = tokio::time::timeout(
        COMPLETION_TIMEOUT,
        route_request(inference_address, &request),
    )
    .await
    .expect("chat completion deadline");
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("chat response headers");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "chat failed: {}",
        String::from_utf8_lossy(&response)
    );
    let value: Value =
        serde_json::from_slice(&response[header_end + 4..]).expect("chat JSON response");
    // Gemma reasons before it answers, so a bounded completion may hold only reasoning text.
    let text = ["/choices/0/message/content", "/choices/0/message/reasoning"]
        .into_iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .find(|text| !text.trim().is_empty())
        .unwrap_or_else(|| panic!("chat response has no generated text: {value}"));
    eprintln!(
        "[apple-metal] route chat answered {text:?} in {:?}",
        started.elapsed()
    );
}

/// Sends one raw HTTP request to the router and reads the whole response.
async fn route_request(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).await.expect("route connection");
    stream.write_all(request).await.expect("route request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("route response");
    response
}

/// Returns the snapshot's one Apple GPU, failing on any other vendor or device count.
fn apple_gpu(snapshot: &HardwareSnapshot) -> AcceleratorFacts {
    let [observation] = snapshot.accelerators.as_slice() else {
        panic!(
            "expected one accelerator vendor: {:?}",
            snapshot.accelerators
        );
    };
    assert_eq!(observation.vendor(), AcceleratorVendor::Apple);
    let Some(accelerator_observation::Result::Facts(set)) = &observation.result else {
        panic!("Apple observation unavailable: {observation:?}");
    };
    let [gpu] = set.facts.as_slice() else {
        panic!("expected one Apple GPU: {:?}", set.facts);
    };
    gpu.clone()
}

/// The host's total and available memory in one snapshot.
fn host_memory_bytes(snapshot: &HardwareSnapshot) -> (u64, u64) {
    match snapshot.host.as_ref().and_then(|host| host.result.as_ref()) {
        Some(host_observation::Result::Facts(facts)) => {
            (facts.total_memory_bytes, facts.available_memory_bytes)
        }
        other => panic!("host facts unavailable: {other:?}"),
    }
}
