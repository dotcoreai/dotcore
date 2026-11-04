//! A site whose node declares a machine: the node reports the declared machine and never reads
//! the host's hardware, its simulated models take their estimate on that machine, a machine of
//! another platform runs simulated models without downloading engine artifacts, each GPU of a
//! machine with GPUs of different sizes launches against its own memory, a starting deployment
//! counts once in the fits, starting values and placement previews of the next one, and a node
//! memory watch follows a deployment's claim, then its measured use, then its removal, never
//! drawing a deployment's memory twice when its measured use arrives, nor dropping the estimate
//! of a model that turns ready before a reading measures it.

use std::{
    collections::BTreeMap,
    fs,
    ops::RangeInclusive,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use dotcore_backend::composition::BackendTestSupport;
use dotcore_domain::{
    ExecutionPlatform,
    hardware_observation::{HardwareSample, HardwareSnapshotSource, NodeHardwareObservation},
};
use dotcore_node_hardware::{MachineDescription, MachineWorkload};
use dotcore_proto::dotcore::{
    capacity::v1::{
        MemoryPool, MemoryPoolKind, MemoryPoolPreview, MemorySegment, MemorySegmentSource,
        NodeMemoryState, PlacementPreview, memory_segment,
    },
    deployment::v1::{MemoryFit, accelerator_memory_observation},
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        EstimateLocalModelResourcesRequest, ListModelDeploymentsRequest,
        ListModelProfileCompatibilityRequest, LocalModelDeploymentConfiguration,
        ModelProfileCompatibility, RemoveModelDeploymentRequest, SizingScenario,
        StartingTotalContextSource, create_model_deployment_request, model_deployment_snapshot,
    },
    observation::v1::{
        AcceleratorFacts, AcceleratorMemoryArchitecture, HardwareSnapshot, accelerator_observation,
        host_observation,
    },
    site::v1::{Site, execution_target, model_runtime_binding},
    watch::v1::{
        KeyedResource, ResourceSubscription, WatchResourcesRequest, WatchResourcesResponse,
        keyed_resource::Payload, resource_subscription,
        resource_watch_service_client::ResourceWatchServiceClient, watch_resources_response,
    },
};
use dotcore_proto::google::protobuf::Timestamp;
use dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport;

use crate::support;

/// Total context of the deployed probe, whose 8192 bytes per token make 2 GiB.
const PROBE_DEPLOYED_TOKENS: u64 = 262_144;
/// Accelerator memory the deployed probe takes.
const PROBE_DEPLOYED_ACCELERATOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Host memory the probe takes whatever its total context.
const PROBE_HOST_BYTES: u64 = 256 * 1024 * 1024;
/// Memory the probe takes per token of total context, accelerator memory for the probe profile
/// and host memory for the host probe profile.
const PROBE_BYTES_PER_TOKEN: u64 = 8192;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Events a node emits only when it evaluates, builds or publishes a Nix engine artifact.
const NIX_EVENTS: [&str; 3] = [
    "nix_engine_artifact_evaluating",
    "nix_engine_build_started",
    "nix_engine_artifact_published",
];

#[tokio::test]
async fn a_node_on_a_declared_machine_never_reads_real_hardware_and_its_models_take_their_estimate()
{
    let root = support::temporary_fast_fixture();
    let machine = support::declare_fixture_machine(&root, &["gb10"]);
    fs::write(
        root.join("catalog/models/fixture-model.yaml"),
        support::SIZED_FIXTURE_MODEL,
    )
    .expect("sized fixture model");
    let (collector, real_hardware_reads) = RealHardware::new();
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        fast_simulation(),
        collector,
    )
    .await;
    let address = backend.management_address();

    // The node reports the GB10's unified host memory, not this host's.
    support::wait_for_listed_hardware(address, "fixture-node", render(&machine, &[])).await;

    let deployment_id =
        support::place_model(address, "fixture-node", "fixture-target", "gb10").await;
    support::wait_for_ready(address, &deployment_id).await;
    let gpu = machine.gpu_ids().next().expect("GB10 GPU").to_owned();
    support::wait_for_listed_hardware(
        address,
        "fixture-node",
        render(
            &machine,
            &[MachineWorkload {
                gpu_bytes: BTreeMap::from([(gpu, support::SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES)]),
                host_bytes: support::SIZED_FIXTURE_MODEL_HOST_BYTES,
            }],
        ),
    )
    .await;

    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id,
        })
        .await
        .expect("remove the deployment");
    support::wait_for_listed_hardware(address, "fixture-node", render(&machine, &[])).await;

    assert_eq!(
        real_hardware_reads.load(Ordering::SeqCst),
        0,
        "a node that declares a machine never reads its real hardware"
    );
    backend.stop().await;
}

#[tokio::test]
async fn a_declared_mac_runs_darwin_simulated_models_without_downloading_engine_artifacts() {
    let root = support::temporary_fast_fixture();
    let machine = support::declare_fixture_machine(&root, &["mac-64g"]);
    for (path, contents) in support::DARWIN_NIX_ENGINE_CATALOG {
        let path = root.join("catalog").join(path);
        fs::create_dir_all(path.parent().expect("catalog directory")).expect("catalog directory");
        fs::write(path, contents).expect("darwin fixture catalog");
    }
    let (collector, real_hardware_reads) = RealHardware::new();
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            ..fast_simulation()
        },
        collector,
    )
    .await;
    let address = backend.management_address();

    let deployment_id = backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "fixture-nix-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            )),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("a darwin model on the declared Mac")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id;
    support::wait_for_ready(address, &deployment_id).await;

    // The model target runs simulated models on the Mac's platform and GPU, whatever this host is.
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("site");
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == "fixture-target")
        .expect("model target");
    assert_eq!(
        target.platform,
        Some(ExecutionPlatform::darwin_arm64().to_oci_platform())
    );
    assert_eq!(target.accelerator_device_ids, ["apple-gpu-0"]);
    assert!(matches!(
        &target.runtime_binding,
        Some(execution_target::RuntimeBinding::ModelRuntime(binding))
            if matches!(binding.adapter, Some(model_runtime_binding::Adapter::SimulatedRuntime(_)))
    ));
    support::wait_for_listed_hardware(address, "fixture-node", render(&machine, &[])).await;

    // Nothing evaluated, built or downloaded the engine, and no artifact reached the cache.
    let nix_events = support::retained_json_records(&root)
        .into_iter()
        .filter(|record| {
            record["fields"]["event_kind"]
                .as_str()
                .is_some_and(|kind| NIX_EVENTS.contains(&kind))
        })
        .count();
    assert_eq!(nix_events, 0);
    assert_eq!(files_under(&root.join("cache")), Vec::<String>::new());
    assert_eq!(real_hardware_reads.load(Ordering::SeqCst), 0);
    backend.stop().await;
}

#[tokio::test]
async fn each_gpu_of_a_mixed_machine_launches_vllm_against_its_own_total() {
    const RTX_4090_TOTAL: u64 = 24 * 1024 * 1024 * 1024;
    const RTX_4080_TOTAL: u64 = 16 * 1024 * 1024 * 1024;
    let root = support::temporary_fast_fixture();
    let machine = support::declare_fixture_machine(&root, &["rtx-4090", "rtx-4080"]);
    let second_gpu = machine.gpu_ids().nth(1).expect("second GPU").to_owned();
    add_simulated_target(&root, "second-gpu-target", &second_gpu);
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/vllm-nvidia-site/catalog/models/qwen35-08b-qualification.yaml"),
        root.join("catalog/models/qwen35-08b-qualification.yaml"),
    )
    .expect("vLLM fixture model");
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            ..fast_simulation()
        },
        // The node declares its machine, so this stands in for hardware nothing reads.
        RealHardware::new().0,
    )
    .await;
    let address = backend.management_address();

    let mut deployments = Vec::new();
    for target in ["fixture-target", "second-gpu-target"] {
        let deployment = backend
            .model_client()
            .create_model_deployment(CreateModelDeploymentRequest {
                source: Some(create_model_deployment_request::Source::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: "qwen35-08b-qualification".to_owned(),
                        profile_id: "small-text".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", target)),
                        configuration: Some(LocalModelDeploymentConfiguration {
                            model_artifact_binding_id: "qualification-binding".to_owned(),
                            ..Default::default()
                        }),
                    },
                )),
                exposure: Some(support::hidden_model_exposure()),
                model_deployment_name: None,
            })
            .await
            .unwrap_or_else(|status| panic!("vLLM on {target}: {status}"))
            .into_inner()
            .deployment
            .expect("accepted deployment");
        deployments.push(deployment);
    }
    for deployment in &deployments {
        support::wait_for_ready(address, &deployment.model_deployment_id).await;
    }

    // Each launch divides its memory utilization by its own GPU's total.
    let retained_totals = deployments
        .iter()
        .map(|deployment| {
            match deployment
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.source.as_ref())
            {
                Some(model_deployment_snapshot::Source::Local(local)) => local
                    .selected_target
                    .as_ref()
                    .and_then(|target| target.accelerator_memory_pool_total_bytes),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retained_totals,
        [Some(RTX_4090_TOTAL), Some(RTX_4080_TOTAL)]
    );
    backend.stop().await;
}

#[tokio::test]
async fn a_starting_deployment_counts_once_on_a_dedicated_gpu() {
    // The probe: 8192 bytes per token of the GPU's free memory less its 1% headroom, 32607 MiB
    // less the driver's 497 MiB, then less the 2 GiB the deployment takes. The host probe: 8192
    // bytes per token of the host's free memory less its 1% headroom, 64 GiB less 4 GiB at idle,
    // less the deployment's 256 MiB of host memory while it starts, and twice that while it
    // loads, since its host estimate stays claimed beside the host memory it already uses.
    claims_count_a_starting_deployment_once(
        "rtx-5090",
        [4_068_343, 3_806_199, 3_806_199, 3_806_199],
        [7_780_433, 7_747_665, 7_714_897, 7_747_665],
    )
    .await;
}

#[tokio::test]
async fn a_starting_deployment_counts_once_on_gb10_unified_memory() {
    // 8192 bytes per token of the host's free memory less its 1% headroom and the probe's 256 MiB
    // of host memory: 130596048896 bytes less 7938146304 bytes at idle, then less the deployment's
    // 2 GiB and 256 MiB, and 256 MiB more while it loads, since its host estimate stays claimed
    // beside the host memory it already uses. Both probes draw on the one host pool.
    let largest_fits = [14_780_701, 14_485_789, 14_453_021, 14_485_789];
    claims_count_a_starting_deployment_once("gb10", largest_fits, largest_fits).await;
}

#[tokio::test]
async fn a_starting_deployment_counts_once_on_a_mac() {
    // As on GB10, from 64 GiB less 8 GiB at idle.
    let largest_fits = [7_223_377, 6_928_465, 6_895_697, 6_928_465];
    claims_count_a_starting_deployment_once("mac-64g", largest_fits, largest_fits).await;
}

#[tokio::test]
async fn a_node_memory_watch_streams_a_deployments_claim_then_its_measured_use_then_its_removal() {
    let root = support::temporary_fast_fixture();
    let description = support::declare_fixture_machine(&root, &["rtx-5090"]);
    fs::write(
        root.join("catalog/models/probe-model.yaml"),
        probe_model(false),
    )
    .expect("probe model");
    let simulation = support::simulated_model_transition_test_support();
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            simulated_runtime: Some(simulation.clone()),
            ..BackendTestSupport::default()
        },
        RealHardware::new().0,
    )
    .await;
    let address = backend.management_address();
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let mut watch = watch_node_memory(&backend).await;
    let pools = |gpu_segments, host_segments| {
        vec![
            MemoryPool {
                kind: MemoryPoolKind::Device as i32,
                device_id: gpu.clone(),
                device_name: "NVIDIA GeForce RTX 5090".to_owned(),
                device_index: Some(0),
                total_bytes: Some(32_607 * MIB),
                segments: gpu_segments,
                over_total_bytes: 0,
            },
            MemoryPool {
                kind: MemoryPoolKind::Host as i32,
                total_bytes: Some(64 * GIB),
                segments: host_segments,
                ..MemoryPool::default()
            },
        ]
    };
    let other = |bytes| MemorySegment {
        source: MemorySegmentSource::Other as i32,
        owner: None,
        bytes,
    };
    let idle = pools(vec![other(497 * MIB)], vec![other(4 * GIB)]);

    // Idle, the GPU holds its driver's reserved memory and the host its idle use.
    wait_for_node_memory(&mut watch, &idle, "idle").await;

    let deployment_id = backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "probe-model".to_owned(),
                    profile_id: "probe-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "probe-artifact",
                        PROBE_DEPLOYED_TOKENS,
                    )),
                },
            )),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("the probe deployment")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id;
    let owned = |source: MemorySegmentSource, bytes| MemorySegment {
        source: source as i32,
        owner: Some(memory_segment::Owner::ModelDeploymentId(
            deployment_id.clone(),
        )),
        bytes,
    };

    // Held before loading, its whole estimate is claimed.
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        simulation.wait_for_preparation_gate(),
    )
    .await
    .expect("the deployment reaches preparation");
    wait_for_node_memory(
        &mut watch,
        &pools(
            vec![
                owned(
                    MemorySegmentSource::Claimed,
                    PROBE_DEPLOYED_ACCELERATOR_BYTES,
                ),
                other(497 * MIB),
            ],
            vec![
                owned(MemorySegmentSource::Claimed, PROBE_HOST_BYTES),
                other(4 * GIB),
            ],
        ),
        "before loading",
    )
    .await;

    // Loading, its GPU memory is measured and attributed, and its host estimate stays claimed
    // beside the host memory it already uses.
    simulation.release_preparation_gate();
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        simulation.wait_for_readiness_gate(),
    )
    .await
    .expect("the deployment reaches loading");
    wait_for_node_memory(
        &mut watch,
        &pools(
            vec![
                owned(
                    MemorySegmentSource::Measured,
                    PROBE_DEPLOYED_ACCELERATOR_BYTES,
                ),
                other(497 * MIB),
            ],
            vec![
                owned(MemorySegmentSource::Claimed, PROBE_HOST_BYTES),
                other(4 * GIB + PROBE_HOST_BYTES),
            ],
        ),
        "loading",
    )
    .await;

    // Ready, it holds its measured GPU use, and its host estimate stands for its host use.
    simulation.release_readiness_gate();
    support::wait_for_ready(address, &deployment_id).await;
    wait_for_node_memory(
        &mut watch,
        &pools(
            vec![
                owned(
                    MemorySegmentSource::Measured,
                    PROBE_DEPLOYED_ACCELERATOR_BYTES,
                ),
                other(497 * MIB),
            ],
            vec![
                owned(MemorySegmentSource::Estimated, PROBE_HOST_BYTES),
                other(4 * GIB),
            ],
        ),
        "ready",
    )
    .await;

    // Removed, the node returns to its idle use.
    backend
        .model_client()
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("remove the deployment");
    wait_for_node_memory(&mut watch, &idle, "removed").await;

    drop(watch);
    backend.stop().await;
}

#[tokio::test]
async fn a_deployments_measured_use_on_a_dedicated_gpu_never_counts_twice() {
    measured_use_counts_once("rtx-5090", &[5_530 * MIB]).await;
}

#[tokio::test]
async fn a_deployments_measured_use_on_gb10_unified_memory_never_counts_twice() {
    measured_use_counts_once("gb10", &[5_530 * MIB]).await;
}

#[tokio::test]
async fn two_deployments_that_fit_a_dedicated_gpu_never_read_past_its_total() {
    // With the GPU's 497 MiB reserved, 31,727 MiB of its 32,607 MiB.
    measured_use_counts_once("rtx-5090", &[5_530 * MIB, 26_200 * MIB]).await;
}

#[tokio::test]
async fn two_models_ready_before_their_first_reading_keep_their_estimates_until_measured() {
    ready_models_keep_their_estimates("rtx-5090", &[5_530 * MIB, 8_192 * MIB]).await;
}

/// Deploys one probe model per entry of `accelerator_bytes` on the declared `machine`, holds them
/// all before loading, then while loading, then lets them get ready. Across every distinct state
/// the node memory watch streams, each pool draws at most its idle use plus the deployments'
/// estimates, which their simulated use equals, and never past its total. The host pool may also
/// count each host estimate twice while loading: its claim stays beside the host memory already
/// used.
async fn measured_use_counts_once(machine: &str, accelerator_bytes: &[u64]) {
    let root = support::temporary_fast_fixture();
    support::declare_fixture_machine(&root, &[machine]);
    for (index, bytes) in accelerator_bytes.iter().enumerate() {
        fs::write(
            root.join(format!("catalog/models/probe-model-{index}.yaml")),
            sized_probe_model(&format!("probe-model-{index}"), *bytes),
        )
        .expect("sized probe model");
    }
    let simulation = support::simulated_model_transition_test_support();
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            simulated_runtime: Some(simulation.clone()),
            ..BackendTestSupport::default()
        },
        RealHardware::new().0,
    )
    .await;
    let mut watch = watch_node_memory(&backend).await;
    let mut states = Vec::new();
    let unbounded = |_: &MemoryPool, _: &[MemoryPool]| 0..=u64::MAX;
    let (_, idle) = follow_node_memory(&mut watch, &mut states, "idle", &unbounded, &|_, pools| {
        idle_pools(pools)
    })
    .await;
    // Unified GPUs draw accelerator memory from the host pool, the node's only one.
    let unified = idle.iter().all(|pool| pool.kind() == MemoryPoolKind::Host);
    let idle_bytes = |pool: &MemoryPool| {
        idle.iter()
            .find(|idle| idle.kind == pool.kind && idle.device_id == pool.device_id)
            .map_or(0, drawn_bytes)
    };
    let estimates: u64 = accelerator_bytes.iter().sum();
    let host_estimates = PROBE_HOST_BYTES * accelerator_bytes.len() as u64;
    let bound = |pool: &MemoryPool, _: &[MemoryPool]| {
        0..=match pool.kind() {
            MemoryPoolKind::Host if unified => idle_bytes(pool) + estimates + 2 * host_estimates,
            MemoryPoolKind::Host => idle_bytes(pool) + 2 * host_estimates,
            _ => idle_bytes(pool) + estimates,
        }
    };

    let mut deployments = Vec::new();
    for index in 0..accelerator_bytes.len() {
        deployments.push(deploy_probe(&backend, &format!("probe-model-{index}")).await);
    }
    // Whether each deployment draws a `source` segment of its accelerator memory, plus `host`
    // bytes on unified GPUs, from the pool its accelerator memory comes from.
    let accelerator_segments = |pools: &[MemoryPool], source: MemorySegmentSource, host: u64| {
        let pool = pools
            .iter()
            .find(|pool| (pool.kind() == MemoryPoolKind::Host) == unified)
            .expect("the pool the deployments draw accelerator memory from");
        let host = if unified { host } else { 0 };
        deployments
            .iter()
            .zip(accelerator_bytes)
            .all(|(id, bytes)| {
                pool.segments.iter().any(|segment| {
                    segment.source() == source
                        && segment.bytes == bytes + host
                        && segment.owner
                            == Some(memory_segment::Owner::ModelDeploymentId(id.clone()))
                })
            })
    };

    // Held before loading, every estimate is claimed.
    follow_node_memory(
        &mut watch,
        &mut states,
        "before loading",
        &bound,
        &|_, pools| accelerator_segments(pools, MemorySegmentSource::Claimed, PROBE_HOST_BYTES),
    )
    .await;

    // Loading, the use each sample measures arrives with its attribution.
    simulation.release_preparation_gate();
    follow_node_memory(&mut watch, &mut states, "loading", &bound, &|_, pools| {
        accelerator_segments(pools, MemorySegmentSource::Measured, 0)
    })
    .await;

    // Ready, nothing is claimed any more.
    simulation.release_readiness_gate();
    for deployment_id in &deployments {
        support::wait_for_ready(backend.management_address(), deployment_id).await;
    }
    follow_node_memory(&mut watch, &mut states, "ready", &bound, &|_, pools| {
        accelerator_segments(pools, MemorySegmentSource::Measured, 0)
            && pools.iter().all(|pool| {
                pool.segments
                    .iter()
                    .all(|segment| segment.source() != MemorySegmentSource::Claimed)
            })
    })
    .await;

    drop(watch);
    backend.stop().await;
}

/// Deploys one probe model per entry of `accelerator_bytes` on the dedicated GPU of the declared
/// `machine`, one after the other, each right after a hardware reading, so it turns ready a whole
/// reading interval before the next reading can show its memory. In every distinct state the node
/// memory watch streams, each pool draws exactly its idle use plus the estimate there of every model
/// the state shows, which their simulated use equals: a ready model keeps its estimate until a
/// reading measures it, and is never counted twice.
async fn ready_models_keep_their_estimates(machine: &str, accelerator_bytes: &[u64]) {
    let root = support::temporary_fast_fixture();
    support::declare_fixture_machine(&root, &[machine]);
    for (index, bytes) in accelerator_bytes.iter().enumerate() {
        fs::write(
            root.join(format!("catalog/models/probe-model-{index}.yaml")),
            sized_probe_model(&format!("probe-model-{index}"), *bytes),
        )
        .expect("sized probe model");
    }
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            simulated_runtime: Some(
                SimulatedRuntimeTestSupport::with_stop_failures(0)
                    .with_representative_startup(Duration::ZERO)
                    .with_lifecycle_delays(Duration::ZERO, Duration::ZERO),
            ),
            ..BackendTestSupport::default()
        },
        RealHardware::new().0,
    )
    .await;
    let mut watch = watch_node_memory(&backend).await;
    let mut states = Vec::new();
    let unbounded = |_: &MemoryPool, _: &[MemoryPool]| 0..=u64::MAX;
    let (mut reading, idle) =
        follow_node_memory(&mut watch, &mut states, "idle", &unbounded, &|_, pools| {
            idle_pools(pools)
        })
        .await;
    assert!(
        idle.iter()
            .any(|pool| pool.kind() == MemoryPoolKind::Device),
        "a dedicated GPU: {idle:#?}"
    );
    let idle_bytes = |pool: &MemoryPool| {
        idle.iter()
            .find(|idle| idle.kind == pool.kind && idle.device_id == pool.device_id)
            .map_or(0, drawn_bytes)
    };
    // Accelerator estimate of each deployment, by ID, from its deploy on.
    let estimates = std::cell::RefCell::new(BTreeMap::<String, u64>::new());
    let bounds = |pool: &MemoryPool, pools: &[MemoryPool]| {
        let shown = pools
            .iter()
            .flat_map(|pool| &pool.segments)
            .filter_map(|segment| match &segment.owner {
                Some(memory_segment::Owner::ModelDeploymentId(id)) => Some(id.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        // Each shown model counts its accelerator estimate on the GPU and its host estimate on
        // the host pool, once each.
        let estimate = |id: &String| match pool.kind() {
            MemoryPoolKind::Host => PROBE_HOST_BYTES,
            _ => estimates.borrow()[id],
        };
        let drawn = idle_bytes(pool) + shown.iter().map(estimate).sum::<u64>();
        drawn..=drawn
    };

    for (index, bytes) in accelerator_bytes.iter().enumerate() {
        // Deploying right after a reading leaves a whole interval before the next one.
        let previous = reading;
        follow_node_memory(
            &mut watch,
            &mut states,
            &format!("reading before deploying model {index}"),
            &bounds,
            &|observed_at, _| observed_at != previous.as_ref(),
        )
        .await;
        let deployment_id = deploy_probe(&backend, &format!("probe-model-{index}")).await;
        estimates.borrow_mut().insert(deployment_id.clone(), *bytes);
        support::wait_for_ready(backend.management_address(), &deployment_id).await;

        (reading, _) = follow_node_memory(
            &mut watch,
            &mut states,
            &format!("model {index} measured"),
            &bounds,
            &|_, pools| {
                pools.iter().all(|pool| {
                    pool.segments
                        .iter()
                        .all(|segment| segment.source() != MemorySegmentSource::Claimed)
                }) && pools.iter().any(|pool| {
                    pool.segments.iter().any(|segment| {
                        segment.source() == MemorySegmentSource::Measured
                            && segment.bytes == *bytes
                            && segment.owner
                                == Some(memory_segment::Owner::ModelDeploymentId(
                                    deployment_id.clone(),
                                ))
                    })
                })
            },
        )
        .await;
    }

    drop(watch);
    backend.stop().await;
}

/// Whether every pool holds only other use: nothing deployed is drawn.
fn idle_pools(pools: &[MemoryPool]) -> bool {
    pools.iter().all(|pool| {
        pool.segments
            .iter()
            .all(|segment| segment.source() == MemorySegmentSource::Other)
    })
}

/// Opens a resource watch of every node's memory.
async fn watch_node_memory(
    backend: &support::RunningBackend,
) -> tonic::Streaming<WatchResourcesResponse> {
    ResourceWatchServiceClient::connect(format!("http://{}", backend.management_address()))
        .await
        .expect("resource watch connection")
        .watch_resources(WatchResourcesRequest {
            subscriptions: vec![ResourceSubscription {
                key: "memory".to_owned(),
                kind: Some(resource_subscription::Kind::NodeMemory(Default::default())),
            }],
        })
        .await
        .expect("open resource watch")
        .into_inner()
}

/// Deploys `model_id`'s probe profile on the fixture target and returns the deployment's ID.
async fn deploy_probe(backend: &support::RunningBackend, model_id: &str) -> String {
    backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: model_id.to_owned(),
                    profile_id: "probe-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "probe-artifact",
                        PROBE_DEPLOYED_TOKENS,
                    )),
                },
            )),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("the probe deployment")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id
}

/// Follows the node memory watch until the fixture node's fresh reading and pools satisfy
/// `reached`, and returns them. Each distinct fresh state on the way is recorded in `states` and
/// asserted to draw, in every pool, bytes within what `bounds` gives for that pool of the state,
/// and never past its total.
async fn follow_node_memory(
    watch: &mut tonic::Streaming<WatchResourcesResponse>,
    states: &mut Vec<Vec<MemoryPool>>,
    step: &str,
    bounds: &dyn Fn(&MemoryPool, &[MemoryPool]) -> RangeInclusive<u64>,
    reached: &dyn Fn(Option<&Timestamp>, &[MemoryPool]) -> bool,
) -> (Option<Timestamp>, Vec<MemoryPool>) {
    let followed = tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let response = watch
                .message()
                .await
                .expect("resource watch")
                .expect("open resource watch");
            let Some(watch_resources_response::Event::Resource(KeyedResource {
                payload: Some(Payload::NodeMemory(memory)),
                ..
            })) = response.event
            else {
                continue;
            };
            let Some(node) = memory.nodes.into_iter().find(|node| {
                node.site_node_id == "fixture-node" && node.state() == NodeMemoryState::Available
            }) else {
                continue;
            };
            let memory_observed_at = node.observed_at;
            let pools = node
                .pools
                .into_iter()
                .map(|pool| pool.pool.unwrap_or_default())
                .collect::<Vec<_>>();
            if states.last() != Some(&pools) {
                for pool in &pools {
                    let bounds = bounds(pool, &pools);
                    assert!(
                        bounds.contains(&drawn_bytes(pool)) && pool.over_total_bytes == 0,
                        "{step}: pool {:?} {} draws {} bytes, bounds {bounds:?}, past its total \
                         by {}; distinct states so far {states:#?}, this one {pools:#?}",
                        pool.kind(),
                        pool.device_id,
                        drawn_bytes(pool),
                        pool.over_total_bytes,
                    );
                }
                states.push(pools.clone());
            }
            if reached(memory_observed_at.as_ref(), &pools) {
                return (memory_observed_at, pools);
            }
        }
    })
    .await;
    followed.unwrap_or_else(|_| panic!("{step}: never reached; distinct states {states:#?}"))
}

/// Bytes a pool draws: every segment's.
fn drawn_bytes(pool: &MemoryPool) -> u64 {
    pool.segments.iter().map(|segment| segment.bytes).sum()
}

/// Waits until the node memory watch shows the fixture node, fresh, drawing exactly `pools` with
/// no unsized deployment, and reports the last payload otherwise.
async fn wait_for_node_memory(
    watch: &mut tonic::Streaming<WatchResourcesResponse>,
    pools: &[MemoryPool],
    step: &str,
) {
    let mut last = None;
    let reached = tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let response = watch
                .message()
                .await
                .expect("resource watch")
                .expect("open resource watch");
            let Some(watch_resources_response::Event::Resource(KeyedResource {
                payload: Some(Payload::NodeMemory(memory)),
                ..
            })) = response.event
            else {
                continue;
            };
            let node = memory
                .nodes
                .iter()
                .find(|node| node.site_node_id == "fixture-node");
            if node.is_some_and(|node| {
                node.state() == NodeMemoryState::Available
                    && node.pools.iter().all(|pool| pool.r#unsized.is_empty())
                    && node
                        .pools
                        .iter()
                        .map(|pool| pool.pool.clone().unwrap_or_default())
                        .eq(pools.iter().cloned())
            }) {
                return;
            }
            last = Some(memory);
        }
    })
    .await;
    assert!(reached.is_ok(), "{step}: last node memory {last:?}");
}

/// Deploys the probe model on the declared `machine` and reads the largest fits of the probe,
/// bound by accelerator memory, and of the host probe, bound by host memory, on the same target
/// at each step: idle before the deploy, then held before it loads, held while it loads, and
/// ready. The deployment's accelerator estimate counts once at every step, and its host estimate
/// stays claimed until it is ready. At each step, each probe starts from its largest fit in whole
/// 1024-token blocks, and the placement preview there and one block above draws the deployment
/// and judges as the compatibility read does.
async fn claims_count_a_starting_deployment_once(
    machine: &str,
    probe: [u32; 4],
    host_probe: [u32; 4],
) {
    let root = support::temporary_fast_fixture();
    let description = support::declare_fixture_machine(&root, &[machine]);
    let darwin = description.platform().operating_system() == "darwin";
    if darwin {
        let [(engine_path, engine), _] = support::DARWIN_NIX_ENGINE_CATALOG else {
            panic!("darwin fixture catalog holds an engine and a model");
        };
        fs::write(root.join("catalog").join(engine_path), engine).expect("darwin fixture engine");
    }
    fs::write(
        root.join("catalog/models/probe-model.yaml"),
        probe_model(darwin),
    )
    .expect("probe model");
    let simulation = support::simulated_model_transition_test_support();
    let backend = support::RunningBackend::start_with_snapshot_collector(
        support::configuration(&root),
        BackendTestSupport {
            simulated_runtime: Some(simulation.clone()),
            ..BackendTestSupport::default()
        },
        // The node declares its machine, so this stands in for hardware nothing reads.
        RealHardware::new().0,
    )
    .await;
    let address = backend.management_address();
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let idle = render(&description, &[]);
    let largest_fit = |step| read_largest_fits(&backend, machine, &idle, &gpu, step);
    let step = |index: usize| [Some(probe[index]), Some(host_probe[index])];
    support::wait_for_listed_hardware(address, "fixture-node", render(&description, &[])).await;
    assert_eq!(largest_fit(None).await, step(0), "{machine} idle");

    let deployment_id = backend
        .model_client()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "probe-model".to_owned(),
                    profile_id: "probe-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "probe-artifact",
                        PROBE_DEPLOYED_TOKENS,
                    )),
                },
            )),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("the probe deployment")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id;

    // Held before loading, nothing is measured yet: the whole estimate is claimed.
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        simulation.wait_for_preparation_gate(),
    )
    .await
    .expect("the deployment reaches preparation");
    support::wait_for_listed_hardware(address, "fixture-node", render(&description, &[])).await;
    assert_eq!(
        largest_fit(Some((1, &deployment_id))).await,
        step(1),
        "{machine} before loading"
    );

    // Held while loading, the memory is measured and attributed to the deployment.
    simulation.release_preparation_gate();
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        simulation.wait_for_readiness_gate(),
    )
    .await
    .expect("the deployment reaches loading");
    let loaded = MachineWorkload {
        gpu_bytes: BTreeMap::from([(gpu.clone(), PROBE_DEPLOYED_ACCELERATOR_BYTES)]),
        host_bytes: PROBE_HOST_BYTES,
    };
    support::wait_for_listed_hardware(
        address,
        "fixture-node",
        render(&description, std::slice::from_ref(&loaded)),
    )
    .await;
    wait_for_attributed_memory(
        &backend,
        &deployment_id,
        &gpu,
        PROBE_DEPLOYED_ACCELERATOR_BYTES,
    )
    .await;
    assert_eq!(
        largest_fit(Some((2, &deployment_id))).await,
        step(2),
        "{machine} loading"
    );

    // Ready, it counts its measured use only.
    simulation.release_readiness_gate();
    support::wait_for_ready(address, &deployment_id).await;
    assert_eq!(
        largest_fit(Some((3, &deployment_id))).await,
        step(3),
        "{machine} ready"
    );
    backend.stop().await;
}

/// Reads the largest fits of the probe and of the host probe on the fixture target, checking that
/// each starts from its largest fit in whole 1024-token blocks and previews as
/// [`check_previews`] expects at `step`.
async fn read_largest_fits(
    backend: &support::RunningBackend,
    machine: &str,
    idle: &HardwareSnapshot,
    gpu: &str,
    step: Option<(usize, &str)>,
) -> [Option<u32>; 2] {
    let response = backend
        .model_client()
        .list_model_profile_compatibility(ListModelProfileCompatibilityRequest {
            model_id: Some("probe-model".to_owned()),
            target: Some(support::exact_placement("fixture-node", "fixture-target")),
            scenario: None,
        })
        .await
        .expect("profile compatibility")
        .into_inner();
    let [probe, host_probe] = response.entries.as_slice() else {
        panic!("probe and host probe entries: {response:?}");
    };
    assert_eq!(
        (probe.profile_id.as_str(), host_probe.profile_id.as_str()),
        ("probe-profile", "host-probe-profile")
    );
    let largest = [
        probe.largest_total_context_tokens,
        host_probe.largest_total_context_tokens,
    ];
    for (entry, largest) in [probe, host_probe].into_iter().zip(largest) {
        let largest = largest.expect("a largest fit");
        assert_eq!(
            (
                entry.starting_total_context_tokens,
                entry.starting_total_context_source(),
            ),
            (
                Some(largest / 1024 * 1024),
                StartingTotalContextSource::LargestFit
            ),
            "{machine} {}",
            entry.profile_id
        );
        check_previews(backend, machine, idle, gpu, entry, step).await;
    }
    largest
}

/// Previews the compatibility `entry`'s profile at its starting total context and one 1024-token
/// block above it, and checks each preview against the compatibility read's free memory verdict
/// for the same total and against the pools `machine` draws from `idle` plus the probe deployment
/// at `step` (1 before loading, 2 loading, 3 ready), when it is deployed.
async fn check_previews(
    backend: &support::RunningBackend,
    machine: &str,
    idle: &HardwareSnapshot,
    gpu: &str,
    entry: &ModelProfileCompatibility,
    step: Option<(usize, &str)>,
) {
    let starting = u64::from(
        entry
            .starting_total_context_tokens
            .expect("a starting value"),
    );
    for (total_context, expected_fit) in [
        (starting, MemoryFit::Fits),
        (starting + 1024, MemoryFit::Exceeds),
    ] {
        let label = format!("{machine} {} at {total_context}", entry.profile_id);
        let preview = backend
            .model_client()
            .estimate_local_model_resources(EstimateLocalModelResourcesRequest {
                model_id: "probe-model".to_owned(),
                profile_id: entry.profile_id.clone(),
                configuration: Some(support::local_model_configuration(
                    "probe-artifact",
                    total_context,
                )),
                placement: Some(support::exact_placement("fixture-node", "fixture-target")),
            })
            .await
            .expect("resource estimate")
            .into_inner()
            .placement_preview
            .expect("a placement preview");
        let compatibility = backend
            .model_client()
            .list_model_profile_compatibility(ListModelProfileCompatibilityRequest {
                model_id: Some("probe-model".to_owned()),
                target: Some(support::exact_placement("fixture-node", "fixture-target")),
                scenario: Some(SizingScenario {
                    total_context_tokens: u32::try_from(total_context).expect("total context"),
                    context_tokens_per_request: None,
                }),
            })
            .await
            .expect("profile compatibility")
            .into_inner()
            .entries
            .into_iter()
            .find(|candidate| candidate.profile_id == entry.profile_id)
            .expect("the profile's entry");

        assert_eq!(preview.free_memory_fit(), expected_fit, "{label}");
        assert_eq!(compatibility.free_memory_fit(), expected_fit, "{label}");
        assert!(preview.observed_at.is_some(), "{label}: reading time");
        assert_eq!(
            PlacementPreview {
                observed_at: None,
                ..preview
            },
            expected_preview(idle, gpu, &entry.profile_id, total_context, step),
            "{label}"
        );
    }
}

/// The placement preview the probe profile `profile_id` at `total_context` gets on the target of
/// `gpu`, from the machine's `idle` reading plus the probe deployment at `step`: claimed before
/// loading; while loading, its accelerator memory measured and its host memory measured and still
/// claimed; once ready, its measured accelerator memory, and on a dedicated GPU its host estimate
/// standing for its host use.
fn expected_preview(
    idle: &HardwareSnapshot,
    gpu: &str,
    profile_id: &str,
    total_context: u64,
    step: Option<(usize, &str)>,
) -> PlacementPreview {
    use MemorySegmentSource::{Claimed, Estimated, Measured, Other, Previewed};

    let scaled = total_context * PROBE_BYTES_PER_TOKEN;
    let (accelerator_demand, host_demand) = match profile_id {
        "probe-profile" => (scaled, PROBE_HOST_BYTES),
        _ => (PROBE_HOST_BYTES, scaled),
    };
    let segment = |source: MemorySegmentSource, owned: bool, bytes| MemorySegment {
        source: source as i32,
        owner: step
            .filter(|_| owned)
            .map(|(_, id)| memory_segment::Owner::ModelDeploymentId(id.to_owned())),
        bytes,
    };
    let device = idle
        .accelerators
        .iter()
        .flat_map(|observation| match &observation.result {
            Some(accelerator_observation::Result::Facts(set)) => set.facts.clone(),
            _ => Vec::new(),
        })
        .find(|fact| fact.device_id == gpu)
        .expect("the target's GPU");
    let Some(host_observation::Result::Facts(host)) =
        idle.host.as_ref().and_then(|host| host.result.clone())
    else {
        panic!("host facts");
    };
    let host_idle = host.total_memory_bytes - host.available_memory_bytes;
    let unified = device.memory_architecture() == AcceleratorMemoryArchitecture::Unified;
    let deployed = PROBE_DEPLOYED_ACCELERATOR_BYTES;
    let step = step.map_or(0, |(step, _)| step);

    let mut pools = Vec::new();
    if !unified {
        let deployment = match step {
            0 => Vec::new(),
            1 => vec![segment(Claimed, true, deployed)],
            _ => vec![segment(Measured, true, deployed)],
        };
        let used = device.used_memory_bytes.expect("GPU use");
        pools.push(pool(
            MemoryPoolKind::Device,
            &device,
            device.total_memory_bytes.expect("GPU total"),
            [deployment, vec![segment(Other, false, used)]].concat(),
            used + if step > 0 { deployed } else { 0 },
            accelerator_demand,
        ));
    }
    let (deployment, measured) = match (unified, step) {
        (_, 0) => (Vec::new(), host_idle),
        (false, 1) => (vec![segment(Claimed, true, PROBE_HOST_BYTES)], host_idle),
        (false, 2) => (
            vec![segment(Claimed, true, PROBE_HOST_BYTES)],
            host_idle + PROBE_HOST_BYTES,
        ),
        (false, _) => (
            vec![segment(Estimated, true, PROBE_HOST_BYTES)],
            host_idle + PROBE_HOST_BYTES,
        ),
        (true, 1) => (
            vec![segment(Claimed, true, deployed + PROBE_HOST_BYTES)],
            host_idle,
        ),
        (true, 2) => (
            vec![
                segment(Measured, true, deployed),
                segment(Claimed, true, PROBE_HOST_BYTES),
            ],
            host_idle + deployed + PROBE_HOST_BYTES,
        ),
        (true, _) => (
            vec![segment(Measured, true, deployed)],
            host_idle + deployed + PROBE_HOST_BYTES,
        ),
    };
    let accounted = deployment
        .iter()
        .filter(|segment| segment.source() != Claimed)
        .map(|segment| segment.bytes)
        .sum::<u64>();
    let claimed = deployment
        .iter()
        .filter(|segment| segment.source() == Claimed)
        .map(|segment| segment.bytes)
        .sum::<u64>();
    pools.push(pool(
        MemoryPoolKind::Host,
        &AcceleratorFacts::default(),
        host.total_memory_bytes,
        [
            deployment,
            vec![segment(Other, false, measured - accounted)],
        ]
        .concat(),
        measured + claimed,
        if unified {
            accelerator_demand + host_demand
        } else {
            host_demand
        },
    ));
    for pool in &mut pools {
        let pool = pool.pool.as_mut().expect("pool");
        let demand = pool.segments.last().expect("segments").bytes;
        pool.segments
            .retain(|segment| segment.bytes > 0 || segment.source() == Previewed);
        assert!(demand > 0, "every pool holds part of the demand");
    }
    let free_memory_fit = if pools
        .iter()
        .any(|pool| pool.free_memory_fit() == MemoryFit::Exceeds)
    {
        MemoryFit::Exceeds
    } else {
        MemoryFit::Fits
    };
    PlacementPreview {
        pools,
        free_memory_fit: free_memory_fit as i32,
        observed_at: None,
    }
}

/// One pool of an expected preview holding `occupied` bytes drawn as `segments`, with `demand`
/// added on top and judged against its total less the 1% headroom.
fn pool(
    kind: MemoryPoolKind,
    device: &AcceleratorFacts,
    total: u64,
    segments: Vec<MemorySegment>,
    occupied: u64,
    demand: u64,
) -> MemoryPoolPreview {
    let line = total - total / 100;
    let drawn = occupied + demand;
    let (over_by_bytes, beyond_total) = match drawn {
        drawn if drawn > total => (drawn - total, true),
        drawn if drawn > line => (drawn - line, false),
        _ => (0, false),
    };
    MemoryPoolPreview {
        pool: Some(MemoryPool {
            kind: kind as i32,
            device_id: device.device_id.clone(),
            device_name: device.model.clone(),
            device_index: device.vendor_index,
            total_bytes: Some(total),
            segments: [
                segments,
                vec![MemorySegment {
                    source: MemorySegmentSource::Previewed as i32,
                    owner: None,
                    bytes: demand,
                }],
            ]
            .concat(),
            over_total_bytes: drawn.saturating_sub(total),
        }),
        fit_line_bytes: Some(line),
        free_memory_fit: if drawn > line {
            MemoryFit::Exceeds
        } else {
            MemoryFit::Fits
        } as i32,
        over_by_bytes,
        beyond_total,
    }
}

/// Waits until the manager lists `bytes` of accelerator memory on `gpu` attributed to the
/// deployment.
async fn wait_for_attributed_memory(
    backend: &support::RunningBackend,
    deployment_id: &str,
    gpu: &str,
    bytes: u64,
) {
    let mut last = None;
    let attributed = tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let memory = backend
                .model_client()
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("model deployments")
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .and_then(|deployment| deployment.accelerator_memory)
                .and_then(|memory| memory.result);
            if let Some(accelerator_memory_observation::Result::Available(usage)) = &memory
                && usage
                    .devices
                    .iter()
                    .map(|device| (device.device_id.as_str(), device.used_memory_bytes))
                    .eq([(gpu, bytes)])
            {
                return;
            }
            last = memory;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        attributed.is_ok(),
        "no attributed memory of {bytes} bytes on {gpu}; last {last:?}"
    );
}

/// The probe model, on the fixture Linux NVIDIA engine or the darwin Nix fixture engine. Its probe
/// profile takes 8192 bytes of accelerator memory per token of total context and 256 MiB of host
/// memory; its host probe profile swaps the two, so host memory bounds its largest fit.
fn probe_model(darwin: bool) -> String {
    let (engine, artifact, class) = if darwin {
        ("fixture-nix-engine", "fixture-metal", "apple")
    } else {
        ("fixture-engine", "fixture-linux", "fixture-nvidia")
    };
    format!(
        r#"modelId: "probe-model"
displayName: "probe model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "probe-artifact"
    weightQuantization: "Q8_0"
    source:
      repository: "dotcore/fixture"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture-Q8_0.gguf"
        provenanceDigest: "0000000000000000000000000000000000000000000000000000000000000000"
configurations:
  - configurationId: "probe-configuration"
    engineArtifacts: [{{ engineReleaseId: "{engine}", engineArtifactId: "{artifact}" }}]
    artifactBindings: [{{ bindingId: "probe-artifact", modelArtifactId: "probe-artifact" }}]
    sizing:
      parameters: {{ bytesPerToken: 8192, hostBytes: {PROBE_HOST_BYTES} }}
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${{bytesPerToken}} * ${{total-context}}"
        hostMemory:
          unit: bytes
          equation: "${{hostBytes}}"
    hardwareSupport:
      - hardwareClassId: "{class}"
        hardwareRequirements:
          minimumSystemMemoryBytes: "1"
        bindingSizing: [{{ bindingId: "probe-artifact" }}]
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: {{ fixedSettings: {{ gpuLayers: 0 }} }}
    executionCapabilities: {{ supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }}
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: {{ supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }}
  - configurationId: "host-probe-configuration"
    engineArtifacts: [{{ engineReleaseId: "{engine}", engineArtifactId: "{artifact}" }}]
    artifactBindings: [{{ bindingId: "probe-artifact", modelArtifactId: "probe-artifact" }}]
    sizing:
      parameters: {{ bytesPerToken: 8192, acceleratorBytes: {PROBE_HOST_BYTES} }}
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${{acceleratorBytes}}"
        hostMemory:
          unit: bytes
          equation: "${{bytesPerToken}} * ${{total-context}}"
    hardwareSupport:
      - hardwareClassId: "{class}"
        hardwareRequirements:
          minimumSystemMemoryBytes: "1"
        bindingSizing: [{{ bindingId: "probe-artifact" }}]
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: {{ fixedSettings: {{ gpuLayers: 0 }} }}
    executionCapabilities: {{ supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }}
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: {{ supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }}
profiles:
  - profileId: "probe-profile"
    displayName: "Probe profile"
    description: "Sizes linearly in the total context."
    configurationId: "probe-configuration"
    defaultModelArtifactBindingId: "probe-artifact"
    engineParameterDefaults:
      values:
        - {{ parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }}
  - profileId: "host-probe-profile"
    displayName: "Host probe profile"
    description: "Sizes linearly in the total context in host memory."
    configurationId: "host-probe-configuration"
    defaultModelArtifactBindingId: "probe-artifact"
    engineParameterDefaults:
      values:
        - {{ parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }}
"#
    )
}

/// A probe model named `model_id` whose probe profile takes `accelerator_bytes` at the deployed
/// total context.
fn sized_probe_model(model_id: &str, accelerator_bytes: u64) -> String {
    assert_eq!(
        accelerator_bytes % PROBE_DEPLOYED_TOKENS,
        0,
        "whole bytes per token"
    );
    probe_model(false)
        .replace(
            "modelId: \"probe-model\"",
            &format!("modelId: \"{model_id}\""),
        )
        .replace(
            "bytesPerToken: 8192",
            &format!(
                "bytesPerToken: {}",
                accelerator_bytes / PROBE_DEPLOYED_TOKENS
            ),
        )
}

/// Adds a simulated model target to the fixture site at `root` beside its first one, holding the
/// declared machine's GPU `gpu`.
fn add_simulated_target(root: &Path, execution_target_id: &str, gpu: &str) {
    let site_path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&site_path).expect("fixture site.yaml");
    let mut target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == "fixture-target")
        .expect("simulated model target")
        .clone();
    target.execution_target_id = execution_target_id.to_owned();
    target.display_name = format!("{} · {execution_target_id}", target.display_name);
    target.accelerator_device_ids = vec![gpu.to_owned()];
    site.execution_targets.push(target);
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("site.yaml"),
    )
    .expect("write site.yaml");
}

/// Simulated runtime controls with no startup activity or lifecycle delay.
fn fast_simulation() -> BackendTestSupport {
    BackendTestSupport {
        simulated_runtime: Some(
            SimulatedRuntimeTestSupport::with_stop_failures(0)
                .with_lifecycle_delays(Duration::ZERO, Duration::ZERO),
        ),
        ..BackendTestSupport::default()
    }
}

/// The snapshot `machine` renders with `workloads` running, as the manager lists it.
fn render(machine: &MachineDescription, workloads: &[MachineWorkload]) -> HardwareSnapshot {
    machine
        .render(workloads, UNIX_EPOCH)
        .expect("declared machine snapshot")
}

/// Every regular file under `root`, relative to it; none when it doesn't exist.
fn files_under(root: &Path) -> Vec<String> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("cache entry").path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .expect("cache file")
                        .display()
                        .to_string(),
                );
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort();
    files
}

/// Stands in for the host's hardware collector and counts every read of it.
struct RealHardware {
    reads: Arc<AtomicUsize>,
}

impl RealHardware {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        (
            Self {
                reads: Arc::clone(&reads),
            },
            reads,
        )
    }
}

impl HardwareSnapshotSource for RealHardware {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.reads.fetch_add(1, Ordering::SeqCst);
        HardwareSnapshot::default()
    }

    fn sample(&self) -> HardwareSample {
        self.reads.fetch_add(1, Ordering::SeqCst);
        HardwareSample::default()
    }

    fn collect_observation(&self) -> NodeHardwareObservation {
        self.reads.fetch_add(1, Ordering::SeqCst);
        NodeHardwareObservation {
            snapshot: HardwareSnapshot::default(),
            accelerator_processes:
                dotcore_domain::hardware_observation::AcceleratorProcessObservation::Observed(
                    Vec::new(),
                ),
        }
    }
}
