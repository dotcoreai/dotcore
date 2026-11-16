//! Explicitly gated NVIDIA Docker qualification for a small test-only vLLM model, and the load of
//! a master catalog vLLM profile at its largest fit.
//!
//! The first suite verifies generic Docker, inference, and lifecycle behavior without
//! resolving or launching a model from the master catalog. It runs on the native Docker platform
//! and on dedicated or unified-memory GPUs.
//!
//! The engine is the master catalog's development-source Docker image: the hosting node evaluates
//! it from the workspace, runs its loader and launches the image it tags, and the suite checks
//! that pin, image and startup telemetry. Preparing the engine has its own deadline, so a cold
//! image does not count against the serving deadlines.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow};
use bollard::{
    Docker,
    models::ContainerInspectResponse,
    query_parameters::{ListContainersOptionsBuilder, LogsOptionsBuilder},
};
use dotcore::model::resolution::resolve_local_launch;
use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_domain::{ExecutionPlatform, SitePaths, nix::DotcoreSources};
use dotcore_proto::dotcore::{
    catalog::v1::ModelInputModality,
    deployment::v1::accelerator_memory_observation,
    inference::v1::FallbackGroupSelection,
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ListModelDeploymentsRequest, LocalModelDeploymentConfiguration, ModelDeployment,
        ModelDeploymentDesiredState, ModelDeploymentExposure, ModelDeploymentStatus,
        PauseModelRequest, PreviewLocalModelDeploymentRequest, RemoveModelDeploymentRequest,
        ResolvedEngineArtifact, ResumeModelRequest, model_deployment_exposure,
        model_deployment_service_client::ModelDeploymentServiceClient, model_deployment_snapshot,
    },
    site::v1::Site,
    telemetry::v1::{WorkloadResourceObservationKind, workload_resource_observation::EngineFacts},
};
use dotcore_runtime_backends::docker::OWNERSHIP_SCHEMA;
use dotcore_site::{
    initial::inputs::InitialSiteInputs,
    node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityInventory,
        TargetCapabilityReconciler,
    },
};
use futures_util::StreamExt as _;
use serde_json::Value;
use sha2::Digest as _;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tonic::{Request, transport::Channel};

use crate::{
    nvidia_qualification::{self, EnginePin, GpuObservation, GpuWorkload, QualificationSite},
    support,
};

const MODEL_ID: &str = "qwen35-08b-qualification";
/// Direct route name, distinct from the model id the engine serves, so routing must map between them.
const ROUTE_NAME: &str = "qwen35-08b-route";
const TEXT_PROFILE_ID: &str = "small-text";
const MULTIMODAL_PROFILE_ID: &str = "small-multimodal";
const MODEL_BINDING_ID: &str = "qualification-binding";
const CHAT_TEMPLATE_ID: &str = "qualification-chat-template";
const EXECUTION_TARGET_ID: &str = "nvidia-target";
const QUALIFICATION_NODE_ID: &str = "development-node";
const CONTAINER_PORT: u16 = 15_000;
const NVIDIA_CDI_SELECTOR: &str = "nvidia.com/gpu=0";
const DEFAULT_CHAT_TEMPLATE_KWARGS_FLAG: &str = "--default-chat-template-kwargs";
const THINKING_ENABLED_JSON: &str = r#"{"enable_thinking":true}"#;
const FIXTURE: &str = "vllm-nvidia-site";
const LABEL_CORRELATION: &str = "com.dotcore.workload.correlation";
const LABEL_SITE: &str = "com.dotcore.site";
const LABEL_SCHEMA: &str = "com.dotcore.runtime.schema";
const NVIDIA_GPU_MINIMUM_FREE_BYTES: u64 = 4_000_000_000;
const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(4 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const QUALIFICATION_TIMEOUT: Duration = Duration::from_secs(9 * 60);
/// Covers a cold engine preparation: building the loader and pulling the multi-GB vLLM CUDA image.
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RED_JPEG: &str = "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/2wBDAQMDAwQDBAgEBAgQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/wAARCABAAEADAREAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAj/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFgEBAQEAAAAAAAAAAAAAAAAAAAcJ/8QAFBEBAAAAAAAAAAAAAAAAAAAAAP/aAAwDAQACEQMRAD8AnRDGqYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD//Z";

type ModelClient = ModelDeploymentServiceClient<Channel>;

#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia"]
async fn hw_nvidia_vllm_text_and_multimodal_qualification() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new(FIXTURE).await?;
    for profile_id in [TEXT_PROFILE_ID, MULTIMODAL_PROFILE_ID] {
        if let Some(reason) = site.unsupported_platform_reason(MODEL_ID, profile_id)? {
            eprintln!("[vllm-nvidia] skipped: {reason}");
            return site.cleanup().await;
        }
    }
    let primary = qualify_on(&site).await;
    let cleanup = match tokio::time::timeout(CLEANUP_TIMEOUT, site.cleanup()).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "vLLM qualification cleanup exceeded its one-minute deadline"
        )),
    };

    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("vLLM qualification cleanup failed")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("cleanup also failed: {cleanup:#}")))
        }
    }
}

/// Launches the Qwen3.8 27B vLLM profile at the largest fit the capacity component computes on the
/// idle GPU and compares the memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_vllm_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "largest-fit-vllm",
            model_id: "qwen3-8-27b",
            profile_id: "nvfp4-text",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Launches the Qwen3.8 27B vLLM profile at the largest fit on a GB10, whose GPU shares host
/// memory, and compares the host memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-gb10"]
async fn hw_gb10_vllm_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "gb10-largest-fit-vllm",
            model_id: "qwen3-8-27b",
            profile_id: "nvfp4-text",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Qualifies the text and multimodal profiles in turn on a GPU with enough free memory within the
/// work deadline.
async fn qualify_on(site: &QualificationSite) -> anyhow::Result<()> {
    nvidia_qualification::ensure_free_gpu_memory("vllm-nvidia", NVIDIA_GPU_MINIMUM_FREE_BYTES)?;
    let configuration = site.configuration.clone();
    let catalog = site.catalog.clone();
    let platform = site.platform.clone();
    let dotcore_sources = site.dotcore_sources.clone();
    let mut qualification = tokio::spawn(async move {
        let backend = support::RunningBackend::start_at_recording_telemetry(
            configuration.clone(),
            "127.0.0.1".parse().expect("loopback bind address"),
            configuration.root.join("telemetry"),
            dotcore_sources.clone(),
        )
        .await;
        let result = async {
            qualify_profile(
                &backend,
                &configuration,
                hardware_case(&catalog, TEXT_PROFILE_ID, &platform, &dotcore_sources).await?,
            )
            .await?;
            nvidia_qualification::ensure_free_gpu_memory(
                "vllm-nvidia",
                NVIDIA_GPU_MINIMUM_FREE_BYTES,
            )
            .context("text-only qualification left too little free GPU memory")?;
            qualify_profile(
                &backend,
                &configuration,
                hardware_case(&catalog, MULTIMODAL_PROFILE_ID, &platform, &dotcore_sources).await?,
            )
            .await?;
            nvidia_qualification::ensure_free_gpu_memory(
                "vllm-nvidia",
                NVIDIA_GPU_MINIMUM_FREE_BYTES,
            )
            .context("multimodal qualification left too little free GPU memory")?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        backend.stop().await;
        result
    });
    // Engine preparation is bounded by its own deadline, so the work deadline adds it to the
    // serving work's.
    match tokio::time::timeout(
        QUALIFICATION_TIMEOUT + PREPARATION_TIMEOUT,
        &mut qualification,
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow!("vLLM NVIDIA qualification task panicked: {error}")),
        Err(_) => {
            qualification.abort();
            let _ = qualification.await;
            Err(anyhow!(
                "vLLM NVIDIA qualification exceeded its work and preparation deadline"
            ))
        }
    }
}

/// Resolves a profile's qualification case on this host and evaluates the engine pin its
/// deployment must record.
async fn hardware_case(
    catalog: &Catalog,
    profile_id: &str,
    platform: &ExecutionPlatform,
    dotcore_sources: &DotcoreSources,
) -> anyhow::Result<QualificationCase> {
    let mut case = qualification_case(
        catalog,
        profile_id,
        platform,
        support::observed_node_nvidia(QUALIFICATION_NODE_ID),
    )?;
    let pin = EnginePin::evaluate(dotcore_sources, &case.engine).await?;
    eprintln!(
        "[vllm-nvidia] profile={profile_id} phase=engine preflight store_path={} image={} image_present={}",
        pin.store_path,
        pin.launch_reference,
        pin.image_present().await?
    );
    case.engine_pin = Some(pin);
    Ok(case)
}

async fn qualify_profile(
    backend: &support::RunningBackend,
    configuration: &SitePaths,
    case: QualificationCase,
) -> anyhow::Result<()> {
    eprintln!(
        "[vllm-nvidia] profile={} phase=start engine_artifact={} model_revision={} template_revision={}",
        case.profile_id, case.engine.engine_release_id, case.model_revision, case.template_revision
    );
    let engine_pin = case.engine_pin.as_ref().context("unevaluated engine pin")?;
    let mut service = backend.model_client();
    let deployment = create_deployment(&mut service, &case).await?;
    let deployment_id = deployment.model_deployment_id.clone();
    assert_route(configuration, &deployment_id)?;
    let preparation_started = Instant::now();
    let prepared = wait_for_preparation(&mut service, configuration, &deployment_id).await?;
    engine_pin.check(&prepared)?;
    eprintln!(
        "[vllm-nvidia] profile={} phase=prepared store_path={} image={} preparation_ms={}",
        case.profile_id,
        engine_pin.store_path,
        engine_pin.launch_reference,
        preparation_started.elapsed().as_millis()
    );
    wait_for_ready_with_diagnostics(
        &mut service,
        configuration,
        &deployment_id,
        "initial readiness",
        None,
    )
    .await?;

    let endpoint = inspect_endpoint(configuration, &deployment_id).await?;
    assert_generic_docker_realization(&endpoint, &case, configuration)?;
    assert_exact_alias(endpoint.host_port, MODEL_ID).await?;
    let running_gpu = wait_for_gpu_attribution(&mut service, &deployment_id, &endpoint).await?;
    assert_startup_telemetry(configuration, &deployment_id, &case).await?;

    if case.multimodal {
        exercise_multimodal_router(backend).await?;
    } else {
        exercise_text_router(backend).await?;
    }

    let (active_gpu, paused_gpu) = if case.multimodal {
        (running_gpu.clone(), None)
    } else {
        pause(&mut service, &deployment_id).await?;
        wait_for_status(
            &mut service,
            &deployment_id,
            ModelDeploymentStatus::Paused,
            "pause",
        )
        .await?;
        let paused = inspect_endpoint(configuration, &deployment_id).await?;
        anyhow::ensure!(
            !paused.running && paused.pid == 0,
            "paused vLLM workload still runs: container={} pid={} status={}",
            paused.container_id,
            paused.pid,
            paused.status
        );
        let paused_gpu = nvidia_qualification::wait_for_release(&running_gpu, DEPLOYMENT_TIMEOUT)
            .await
            .context("paused vLLM workload kept the GPU")?;

        resume(&mut service, &deployment_id).await?;
        wait_for_ready_with_diagnostics(
            &mut service,
            configuration,
            &deployment_id,
            "resume readiness",
            Some(endpoint.host_port),
        )
        .await?;
        let resumed = inspect_endpoint(configuration, &deployment_id).await?;
        eprintln!(
            "[vllm-nvidia] profile={} phase=resumed initial_host_port={} resumed_host_port={} changed={}",
            case.profile_id,
            endpoint.host_port,
            resumed.host_port,
            endpoint.host_port != resumed.host_port,
        );
        assert_generic_docker_realization(&resumed, &case, configuration)?;
        assert_exact_alias(resumed.host_port, MODEL_ID).await?;
        let resumed_gpu = wait_for_gpu_attribution(&mut service, &deployment_id, &resumed).await?;
        assert_nonstreaming_chat(backend.inference_address()).await?;
        (resumed_gpu, Some(paused_gpu.memory))
    };

    remove(&mut service, &deployment_id).await?;
    wait_for_absence(&mut service, &deployment_id).await?;
    assert_container_absent(configuration, &deployment_id).await?;
    let removed_gpu = nvidia_qualification::wait_for_release(&active_gpu, DEPLOYMENT_TIMEOUT)
        .await
        .context("removed vLLM workload kept the GPU")?;
    assert_route_absent(configuration)?;

    eprintln!(
        "[vllm-nvidia] profile={} phase=complete engine_artifact={} model_revision={} template_revision={} entrypoint={:?} argv={:?} gpu_processes={:?} gpu_process_bytes={} gpu_paused_memory={:?} gpu_removed_memory=({})",
        case.profile_id,
        case.engine.engine_release_id,
        case.model_revision,
        case.template_revision,
        endpoint.entrypoint,
        endpoint.argv,
        running_gpu.process_ids,
        running_gpu.process_memory_bytes,
        paused_gpu.map(|memory| memory.to_string()),
        removed_gpu.memory,
    );
    Ok(())
}

/// Waits for the deployment's startup telemetry record and checks its inputs and vLLM facts.
async fn assert_startup_telemetry(
    configuration: &SitePaths,
    deployment_id: &str,
    case: &QualificationCase,
) -> anyhow::Result<()> {
    let directory = configuration.root.join("telemetry");
    let record = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if directory.is_dir() {
                let records = dotcore::telemetry::WorkloadResourceRecords::read(
                    std::slice::from_ref(&directory),
                )
                .context("read deployment telemetry")?;
                // Deployment ids repeat across this test's profiles, so match the profile too.
                if let Some(record) = records.records.into_iter().rev().find(|record| {
                    record
                        .realization
                        .as_ref()
                        .is_some_and(|realization| realization.owner_id == deployment_id)
                        && record
                            .inputs
                            .as_ref()
                            .is_some_and(|inputs| inputs.profile_id == case.profile_id)
                        && record.observation.as_ref().is_some_and(|observation| {
                            observation.kind == WorkloadResourceObservationKind::Startup as i32
                        })
                }) {
                    return Ok::<_, anyhow::Error>(record);
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("vLLM deployment produced no startup telemetry record")??;
    let inputs = record.inputs.context("startup record has no inputs")?;
    anyhow::ensure!(
        inputs.model_id == MODEL_ID,
        "startup record inputs differ from the deployment: {inputs:?}"
    );
    let engine_pin = case.engine_pin.as_ref().context("unevaluated engine pin")?;
    anyhow::ensure!(
        inputs.engine_artifact_reference == engine_pin.store_path,
        "startup record engine {} differs from the pinned store path {}",
        inputs.engine_artifact_reference,
        engine_pin.store_path
    );
    let observation = record
        .observation
        .context("startup record has no observation")?;
    anyhow::ensure!(
        observation.attributed_memory_bytes > 0,
        "startup record has no attributed memory"
    );
    let Some(EngineFacts::Vllm(facts)) = observation.engine_facts else {
        anyhow::bail!("startup record has no vLLM facts");
    };
    anyhow::ensure!(
        facts
            .kv_cache_token_capacity
            .is_some_and(|tokens| tokens > 0)
            && facts.model_loading_bytes.is_some_and(|bytes| bytes > 0)
            && facts.kv_cache_reserved_bytes.is_some_and(|bytes| bytes > 0),
        "startup record vLLM facts are incomplete: {facts:?}"
    );
    eprintln!(
        "[vllm-nvidia] profile={} phase=telemetry attributed_bytes={} facts={facts:?}",
        case.profile_id, observation.attributed_memory_bytes
    );
    Ok(())
}

async fn exercise_text_router(backend: &support::RunningBackend) -> anyhow::Result<()> {
    let address = backend.inference_address();

    wait_for_route(backend, address).await?;
    assert_tool_round_trip(address).await?;
    assert_nonstreaming_chat(address).await?;
    assert_streaming_chat(address).await?;
    assert_responses_reasoning(address).await?;
    Ok(())
}

async fn exercise_multimodal_router(backend: &support::RunningBackend) -> anyhow::Result<()> {
    let address = backend.inference_address();

    wait_for_route(backend, address).await?;
    assert_nonstreaming_chat(address).await?;
    assert_image_input(address).await?;
    Ok(())
}

async fn assert_nonstreaming_chat(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = post_json(
        address,
        "/v1/chat/completions",
        serde_json::json!({
            "model": ROUTE_NAME,
            "messages": [{"role":"user","content":"Compute 17 + 25. Reply with only the number."}],
            "temperature": 0,
            "max_completion_tokens": 1024
        }),
    )
    .await?;
    anyhow::ensure!(
        response.get("model").and_then(Value::as_str) == Some(ROUTE_NAME),
        "non-streaming Chat response does not echo the route name: {response}"
    );
    let message = response
        .pointer("/choices/0/message")
        .context("non-streaming Chat response has no assistant message")?;
    let content = nonempty_string(message.get("content"), "non-streaming final content")
        .with_context(|| format!("non-streaming Chat response: {response}"))?;
    anyhow::ensure!(
        content.trim() == "42",
        "non-streaming Chat returned unexpected final content {content:?}: {response}"
    );
    nonempty_string(message.get("reasoning"), "non-streaming separate reasoning")
        .with_context(|| format!("non-streaming Chat response: {response}"))?;
    anyhow::ensure!(
        response
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            == Some("stop"),
        "non-streaming Chat did not finish normally: {response}"
    );
    Ok(())
}

async fn assert_streaming_chat(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = post_raw(
        address,
        "/v1/chat/completions",
        serde_json::json!({
            "model": ROUTE_NAME,
            "messages": [{"role":"user","content":"Compute 17 + 25. Reply with only the number."}],
            "temperature": 0,
            "max_completion_tokens": 1024,
            "stream": true,
            "stream_options": {"include_usage": true}
        }),
    )
    .await?;
    anyhow::ensure!(
        response
            .windows(b"data: [DONE]".len())
            .any(|part| part == b"data: [DONE]"),
        "streaming Chat response ended without [DONE]: {}",
        bounded_response(&response)
    );
    let frames = sse_json_frames(&response)?;
    anyhow::ensure!(
        frames.iter().any(|frame| frame
            .pointer("/choices/0/delta/reasoning")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())),
        "streaming Chat emitted no separate reasoning deltas: {}",
        bounded_response(&response)
    );
    let final_content = frames
        .iter()
        .filter_map(|frame| {
            frame
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
        })
        .collect::<String>();
    anyhow::ensure!(
        final_content.trim() == "42",
        "streaming Chat returned unexpected final content {final_content:?}: {}",
        bounded_response(&response)
    );
    anyhow::ensure!(
        frames.iter().any(|frame| {
            frame
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                == Some("stop")
        }),
        "streaming Chat did not finish normally: {}",
        bounded_response(&response)
    );
    Ok(())
}

async fn assert_responses_reasoning(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = post_json(
        address,
        "/v1/responses",
        serde_json::json!({
            "model": ROUTE_NAME,
            "input": "Compute 17 + 25. Reply with only the number.",
            "store": false,
            "temperature": 0,
            "max_output_tokens": 1024
        }),
    )
    .await?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .context("Responses result has no output items")?;
    anyhow::ensure!(
        output
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning")),
        "Responses result has no reasoning output item: {}",
        response
    );
    let final_text = output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    anyhow::ensure!(
        final_text.trim() == "42",
        "Responses result has unexpected final output {final_text:?}: {response}"
    );
    anyhow::ensure!(
        response.get("status").and_then(Value::as_str) == Some("completed"),
        "Responses result did not complete: {response}"
    );
    Ok(())
}

async fn assert_tool_round_trip(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let first = post_json(
        address,
        "/v1/chat/completions",
        serde_json::json!({
            "model": ROUTE_NAME,
            "messages": [{"role":"user","content":"Use the ready tool, then report its result."}],
            "tools": [{"type":"function","function":{"name":"ready","description":"Returns readiness","parameters":{"type":"object","properties":{},"additionalProperties":false}}}],
            "tool_choice": {"type":"function","function":{"name":"ready"}},
            "temperature": 0,
            "max_completion_tokens": 512
        }),
    )
    .await?;
    let call = first
        .pointer("/choices/0/message/tool_calls/0")
        .context("tool response has no tool call")?;
    let call_id = call
        .get("id")
        .and_then(Value::as_str)
        .context("tool call has no ID")?;
    anyhow::ensure!(
        call.pointer("/function/name").and_then(Value::as_str) == Some("ready"),
        "tool response invoked the wrong function: {call}"
    );
    let arguments = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .context("tool call has no arguments")?;
    anyhow::ensure!(
        serde_json::from_str::<Value>(arguments)? == serde_json::json!({}),
        "tool call arguments were not an empty object: {arguments}"
    );

    let second = post_json(
        address,
        "/v1/chat/completions",
        serde_json::json!({
            "model": ROUTE_NAME,
            "messages": [
                {"role":"user","content":"Use the ready tool, then report its result."},
                {"role":"assistant","content":null,"tool_calls":[call.clone()]},
                {"role":"tool","tool_call_id":call_id,"content":"READY"}
            ],
            "tools": [{"type":"function","function":{"name":"ready","description":"Returns readiness","parameters":{"type":"object","properties":{},"additionalProperties":false}}}],
            "tool_choice": "none",
            "temperature": 0,
            "max_completion_tokens": 512
        }),
    )
    .await?;
    nonempty_string(
        second.pointer("/choices/0/message/content"),
        "tool round-trip final content",
    )
    .with_context(|| format!("tool round-trip response: {second}"))?;
    Ok(())
}

async fn assert_image_input(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = post_json(
        address,
        "/v1/chat/completions",
        serde_json::json!({
            "model": ROUTE_NAME,
            "messages": [{"role":"user","content":[
                {"type":"text","text":"Identify the dominant color. Reply with one color word."},
                {"type":"image_url","image_url":{"url":RED_JPEG,"detail":"auto"}}
            ]}],
            "temperature": 0,
            "max_completion_tokens": 512
        }),
    )
    .await?;
    let content = nonempty_string(
        response.pointer("/choices/0/message/content"),
        "multimodal final content",
    )
    .with_context(|| format!("multimodal response: {response}"))?;
    anyhow::ensure!(
        content.to_ascii_lowercase().contains("red"),
        "multimodal response did not identify the fixture content: {content:?}"
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct QualificationCase {
    profile_id: String,
    configuration: LocalModelDeploymentConfiguration,
    expected_command: Vec<String>,

    /// Development-source engine artifact the hosting node pins and prepares at deployment.
    engine: ResolvedEngineArtifact,

    /// Engine pin this host must record, evaluated before a hardware run.
    engine_pin: Option<EnginePin>,
    model_revision: String,
    template_revision: String,
    multimodal: bool,
}

fn qualification_case(
    catalog: &Catalog,
    profile_id: &str,
    platform: &ExecutionPlatform,
    node_nvidia: NodeAcceleratorObservation,
) -> anyhow::Result<QualificationCase> {
    // Engine defaults fill the shared context parameters the profile leaves out.
    let (model_artifact_binding_id, engine_parameter_values) = catalog
        .local_defaults_for(&CatalogModelId(MODEL_ID.to_owned()), profile_id)
        .with_context(|| {
            format!("required vLLM catalog model {MODEL_ID:?} profile {profile_id:?} is absent")
        })?;
    let configuration = LocalModelDeploymentConfiguration {
        model_artifact_binding_id,
        engine_parameter_values: Some(engine_parameter_values),
        custom_argument_tokens: vec![
            DEFAULT_CHAT_TEMPLATE_KWARGS_FLAG.to_owned(),
            THINKING_ENABLED_JSON.to_owned(),
        ],
        option_overrides: Vec::new(),
    };
    let (execution_target, target_inventory) =
        qualification_target_capabilities(platform, node_nvidia);
    let snapshot = resolve_local_launch(
        catalog,
        &CatalogModelId(MODEL_ID.to_owned()),
        profile_id,
        &configuration,
        &execution_target,
        &target_inventory,
        None,
        dotcore::model::resolution::EngineSecrets::Generate(&dotcore::RandomSecretGenerator),
    )
    .with_context(|| format!("resolve vLLM qualification profile {profile_id:?}"))?;
    let plan = snapshot
        .desired_launch_plan
        .as_ref()
        .context("vLLM resolution produced no desired launch plan")?;
    let expected_command = plan
        .arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal(value)) => Ok(value.clone()),
            Some(dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::ArtifactBindingId(id)) if id == "model-snapshot" => Ok("/opt/dotcore/models/snapshot".to_owned()),
            Some(dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::ArtifactBindingId(id)) if id == "chat-template" => Ok("/opt/dotcore/models/chat-template.jinja".to_owned()),
            _ => Err(anyhow!("unexpected vLLM desired-plan token")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let engine = snapshot
        .engine_artifact
        .clone()
        .context("resolved vLLM profile has no engine artifact")?;
    anyhow::ensure!(
        nvidia_qualification::development_source_docker_image(&engine),
        "resolved vLLM engine is not a development-source Docker-delivered Nix artifact"
    );
    let source = snapshot
        .model_artifact
        .as_ref()
        .and_then(|artifact| artifact.source.as_ref())
        .context("resolved vLLM profile has no model source")?;
    let template = snapshot
        .auxiliary_artifacts
        .iter()
        .find(|artifact| artifact.auxiliary_artifact_id == CHAT_TEMPLATE_ID)
        .and_then(|artifact| artifact.source.as_ref())
        .context("resolved vLLM profile has no pinned chat template")?;
    anyhow::ensure!(
        configuration.model_artifact_binding_id == MODEL_BINDING_ID,
        "vLLM qualification profile selected unexpected binding {:?}",
        configuration.model_artifact_binding_id
    );
    let multimodal = snapshot
        .input_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities
                .supported_modalities
                .contains(&(ModelInputModality::Image as i32))
        });
    Ok(QualificationCase {
        profile_id: profile_id.to_owned(),
        configuration,
        expected_command,
        engine,
        engine_pin: None,
        model_revision: source.revision.clone(),
        template_revision: template.revision.clone(),
        multimodal,
    })
}

/// Declares the fixture execution target on `platform` and a fresh capability inventory from one
/// node observation.
fn qualification_target_capabilities(
    platform: &ExecutionPlatform,
    node_nvidia: NodeAcceleratorObservation,
) -> (
    dotcore_proto::dotcore::site::v1::ExecutionTarget,
    TargetCapabilityInventory,
) {
    let node_id = QUALIFICATION_NODE_ID;
    let target = dotcore_proto::dotcore::site::v1::ExecutionTarget {
        execution_target_id: EXECUTION_TARGET_ID.to_owned(),
        display_name: EXECUTION_TARGET_ID.to_owned(),
        site_node_id: node_id.to_owned(),
        accelerator_device_ids: vec![NVIDIA_CDI_SELECTOR.to_owned()],
        platform: Some(platform.to_oci_platform()),
        runtime_binding: Some(
            dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(
                        dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::DockerRuntime(
                            Default::default(),
                        ),
                    ),
                },
            ),
        ),
    };
    let inventory = TargetCapabilityReconciler::reconcile_now(
        [&target],
        [RuntimePlatformObservation::available(
            EXECUTION_TARGET_ID,
            platform.clone(),
        )],
        [node_nvidia],
    );
    (target, inventory)
}

fn expected_entrypoint() -> Vec<String> {
    vec!["vllm".to_owned()]
}

fn assert_generic_docker_realization(
    endpoint: &RuntimeEndpoint,
    case: &QualificationCase,
    configuration: &SitePaths,
) -> anyhow::Result<()> {
    let engine_pin = case.engine_pin.as_ref().context("unevaluated engine pin")?;
    anyhow::ensure!(
        endpoint.image == engine_pin.launch_reference,
        "vLLM Docker image {} is not the loader's image {}",
        endpoint.image,
        engine_pin.launch_reference
    );
    let expected_entrypoint = expected_entrypoint();
    anyhow::ensure!(
        endpoint.entrypoint == expected_entrypoint,
        "vLLM Docker entrypoint violates the adapter contract: expected {:?}, found {:?}",
        expected_entrypoint,
        endpoint.entrypoint
    );
    anyhow::ensure!(
        endpoint.argv == case.expected_command[1..],
        "vLLM Docker argv differs from the resolved site catalog fixture: expected {:?}, found {:?}",
        &case.expected_command[1..],
        endpoint.argv
    );
    anyhow::ensure!(
        endpoint.ipc_mode.as_deref() == Some("host"),
        "vLLM Docker IPC is not host: {:?}",
        endpoint.ipc_mode
    );
    anyhow::ensure!(
        !endpoint.privileged,
        "vLLM Docker workload is unexpectedly privileged"
    );
    anyhow::ensure!(
        endpoint.device_driver.as_deref() == Some("cdi"),
        "vLLM Docker device driver is not CDI: {:?}",
        endpoint.device_driver
    );
    anyhow::ensure!(
        endpoint.device_ids == [NVIDIA_CDI_SELECTOR],
        "vLLM Docker device selection differs: {:?}",
        endpoint.device_ids
    );
    anyhow::ensure!(
        endpoint.host_ip.as_deref() == Some("127.0.0.1"),
        "vLLM host port is not loopback-only: {:?}",
        endpoint.host_ip
    );
    anyhow::ensure!(
        endpoint.container_port == CONTAINER_PORT,
        "vLLM container port differs: {}",
        endpoint.container_port
    );
    let mut destinations = endpoint
        .mounts
        .iter()
        .map(|(_, destination, read_only)| (destination.as_str(), *read_only))
        .collect::<Vec<_>>();
    destinations.sort_unstable();
    anyhow::ensure!(
        destinations
            == [
                ("/opt/dotcore/models/chat-template.jinja", true),
                ("/opt/dotcore/models/snapshot", true),
                ("/var/cache/dotcore-engine", false),
            ],
        "vLLM Docker mounts violate the read-only artifact and writable engine cache contract: {:?}",
        endpoint.mounts
    );
    let cache = endpoint
        .mounts
        .iter()
        .find_map(|(source, destination, _)| {
            (destination == "/var/cache/dotcore-engine").then(|| PathBuf::from(source))
        })
        .context("vLLM Docker workload has no engine cache mount")?;
    let engine_cache_suffix = Path::new("engine-artifact")
        .join(engine_artifact_key(&engine_pin.store_path))
        .join("engine-cache");
    anyhow::ensure!(
        cache.starts_with(configuration.workload_cache_root())
            && cache.ends_with(&engine_cache_suffix),
        "vLLM engine cache is not the engine artifact's workload cache folder {}: {}",
        engine_cache_suffix.display(),
        cache.display()
    );
    anyhow::ensure!(
        cache.join("vllm/torch_compile_cache").is_dir(),
        "vLLM did not write its compile cache to {}",
        cache.display()
    );
    Ok(())
}

/// Returns the storage key the backend derives from an engine launch identity: `sha256-` and the
/// first 16 bytes of the identity's SHA-256 in lowercase hex.
fn engine_artifact_key(launch_identity: &str) -> String {
    let digest = sha2::Sha256::digest(launch_identity.as_bytes());
    let hex = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256-{hex}")
}

#[derive(Debug)]
struct RuntimeEndpoint {
    container_id: String,
    image: String,
    entrypoint: Vec<String>,
    argv: Vec<String>,
    mounts: Vec<(String, String, bool)>,
    ipc_mode: Option<String>,
    privileged: bool,
    device_driver: Option<String>,
    device_ids: Vec<String>,
    container_port: u16,
    host_ip: Option<String>,
    host_port: u16,
    running: bool,
    pid: i64,
    status: String,
}

async fn inspect_endpoint(
    configuration: &SitePaths,
    deployment_id: &str,
) -> anyhow::Result<RuntimeEndpoint> {
    let docker =
        Docker::connect_with_local_defaults().context("connect Docker for vLLM inspection")?;
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))?;
    let filters = HashMap::from([(
        "label",
        vec![
            format!("{LABEL_CORRELATION}={deployment_id}"),
            format!("{LABEL_SITE}={}", site.site_id),
            format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
        ],
    )]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("list vLLM Docker workloads")?;
    anyhow::ensure!(
        containers.len() == 1,
        "expected exactly one vLLM Docker workload for deployment {deployment_id}, found {}",
        containers.len()
    );
    let id = containers[0]
        .id
        .clone()
        .context("vLLM Docker workload has no container ID")?;
    let inspection = docker
        .inspect_container(&id, None)
        .await
        .context("inspect vLLM Docker workload")?;
    normalize_inspection(id, inspection)
}

fn normalize_inspection(
    container_id: String,
    inspection: ContainerInspectResponse,
) -> anyhow::Result<RuntimeEndpoint> {
    let config = inspection
        .config
        .context("vLLM Docker inspection has no Config")?;
    let host = inspection
        .host_config
        .context("vLLM Docker inspection has no HostConfig")?;
    let log_driver = host
        .log_config
        .as_ref()
        .and_then(|config| config.typ.as_deref());
    anyhow::ensure!(
        log_driver == Some("local"),
        "vLLM Docker workload log driver is {log_driver:?}, not \"local\""
    );
    let request = host
        .device_requests
        .as_deref()
        .and_then(|requests| requests.first())
        .context("vLLM Docker inspection has no accelerator device request")?;
    let state = inspection
        .state
        .as_ref()
        .context("vLLM Docker inspection has no State")?;
    let running = state.running.unwrap_or(false);
    let binding = inspection
        .network_settings
        .as_ref()
        .and_then(|settings| settings.ports.as_ref())
        .and_then(|ports| ports.get(&format!("{CONTAINER_PORT}/tcp")))
        .and_then(Option::as_ref)
        .and_then(|bindings| bindings.first());
    anyhow::ensure!(
        !running || binding.is_some(),
        "running vLLM Docker inspection has no {CONTAINER_PORT}/tcp binding"
    );
    let host_port = binding
        .and_then(|binding| binding.host_port.as_deref())
        .map(|port| {
            port.parse::<u16>()
                .context("vLLM Docker host port is malformed")
        })
        .transpose()?
        .unwrap_or_default();
    let mounts = inspection
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mount| Some((mount.source?, mount.destination?, mount.rw == Some(false))))
        .collect();
    Ok(RuntimeEndpoint {
        container_id,
        image: config.image.unwrap_or_default(),
        entrypoint: config.entrypoint.unwrap_or_default(),
        argv: config.cmd.unwrap_or_default(),
        mounts,
        ipc_mode: host.ipc_mode,
        privileged: host.privileged.unwrap_or(false),
        device_driver: request.driver.clone(),
        device_ids: request.device_ids.clone().unwrap_or_default(),
        container_port: CONTAINER_PORT,
        host_ip: binding.and_then(|binding| binding.host_ip.clone()),
        host_port,
        running,
        pid: state.pid.unwrap_or_default(),
        status: state
            .status
            .map(|status| format!("{status:?}"))
            .unwrap_or_default(),
    })
}

async fn create_deployment(
    service: &mut ModelClient,
    case: &QualificationCase,
) -> anyhow::Result<ModelDeployment> {
    let local = CreateLocalModelDeploymentRequest {
        model_id: MODEL_ID.to_owned(),
        profile_id: case.profile_id.clone(),
        placement: Some(support::exact_placement(
            "development-node",
            EXECUTION_TARGET_ID,
        )),
        configuration: Some(case.configuration.clone()),
    };
    let preview = service
        .preview_local_model_deployment(Request::new(PreviewLocalModelDeploymentRequest {
            local: Some(local.clone()),
        }))
        .await
        .context("preview vLLM deployment")?
        .into_inner()
        .resolved_snapshot
        .context("vLLM preview returned no snapshot")?;
    let deployment = service
        .create_model_deployment(Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    local,
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: ROUTE_NAME.to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        }))
        .await
        .context("create vLLM deployment")?
        .into_inner()
        .deployment
        .context("create vLLM deployment returned no deployment")?;
    let stored = deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
        .and_then(|source| match source {
            model_deployment_snapshot::Source::Local(local) => Some(local),
            model_deployment_snapshot::Source::Upstream(_) => None,
        })
        .context("created vLLM deployment has no local snapshot")?;
    anyhow::ensure!(
        stored == &preview,
        "created vLLM snapshot differs from preview"
    );
    anyhow::ensure!(
        ModelDeploymentDesiredState::try_from(deployment.desired_state)
            == Ok(ModelDeploymentDesiredState::Present),
        "created vLLM deployment did not retain Present intent"
    );
    Ok(deployment)
}

async fn wait_for_ready_with_diagnostics(
    service: &mut ModelClient,
    configuration: &SitePaths,
    deployment_id: &str,
    operation: &str,
    previous_host_port: Option<u16>,
) -> anyhow::Result<ModelDeployment> {
    match wait_for_status(
        service,
        deployment_id,
        ModelDeploymentStatus::Ready,
        operation,
    )
    .await
    {
        Ok(deployment) => Ok(deployment),
        Err(readiness) => {
            let diagnostics =
                readiness_diagnostics(service, configuration, deployment_id, previous_host_port)
                    .await;
            Err(readiness.context(format!(
                "vLLM readiness diagnostics before cleanup: {diagnostics}"
            )))
        }
    }
}

async fn readiness_diagnostics(
    service: &mut ModelClient,
    configuration: &SitePaths,
    deployment_id: &str,
    previous_host_port: Option<u16>,
) -> String {
    let deployment = service
        .list_model_deployments(Request::new(ListModelDeploymentsRequest {}))
        .await
        .map(|response| {
            response
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
        });
    let endpoint = inspect_endpoint(configuration, deployment_id).await;
    let endpoint_diagnostics = match endpoint {
        Ok(endpoint) => {
            let direct_models = if endpoint.running {
                assert_exact_alias(endpoint.host_port, MODEL_ID)
                    .await
                    .map(|()| "ok".to_owned())
                    .unwrap_or_else(|error| format!("error={error:#}"))
            } else {
                "not attempted for stopped container".to_owned()
            };
            let logs = container_log_tail(&endpoint.container_id)
                .await
                .unwrap_or_else(|error| format!("error={error:#}"));
            format!(
                "previous_host_port={previous_host_port:?} endpoint={endpoint:?} direct_models={direct_models} gpu={} container_log_tail={logs:?}",
                gpu_diagnostics(),
            )
        }
        Err(error) => format!(
            "previous_host_port={previous_host_port:?} endpoint_error={error:#} gpu={}",
            gpu_diagnostics(),
        ),
    };
    format!("deployment={deployment:?} {endpoint_diagnostics}")
}

async fn container_log_tail(container_id: &str) -> anyhow::Result<String> {
    let docker = Docker::connect_with_local_defaults().context("connect Docker for vLLM logs")?;
    let options = LogsOptionsBuilder::default()
        .follow(false)
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail("200")
        .build();
    let mut stream = docker.logs(container_id, Some(options));
    tokio::time::timeout(HTTP_TIMEOUT, async {
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            output.push_str(&chunk.context("read vLLM container log tail")?.to_string());
        }
        Ok(output)
    })
    .await
    .context("read vLLM container log tail before cleanup timed out")?
}

fn gpu_diagnostics() -> String {
    match GpuObservation::collect() {
        Ok(observation) => format!(
            "memory=({}) processes={:?}",
            observation.memory, observation.processes
        ),
        Err(error) => format!("error={error:#}"),
    }
}

async fn wait_for_status(
    service: &mut ModelClient,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    operation: &str,
) -> anyhow::Result<ModelDeployment> {
    let started = Instant::now();
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployment = read_deployment(service, deployment_id, operation).await?;
            let state = deployment
                .state
                .as_ref()
                .context("vLLM deployment has no state")?;
            if state.status == expected as i32 {
                return Ok(deployment);
            }
            if state.status == ModelDeploymentStatus::Failed as i32 {
                anyhow::bail!(
                    "vLLM deployment failed during {operation} after {:?}: {:?}",
                    started.elapsed(),
                    state.failure
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| format!("vLLM deployment never reached {expected:?} during {operation}"))?
}

/// Waits until the hosting node prepared the deployment's engine and returns the deployment.
/// Fails on a failed deployment or once the preparation deadline passes.
async fn wait_for_preparation(
    service: &mut ModelClient,
    configuration: &SitePaths,
    deployment_id: &str,
) -> anyhow::Result<ModelDeployment> {
    tokio::time::timeout(PREPARATION_TIMEOUT, async {
        loop {
            let deployment = read_deployment(service, deployment_id, "engine preparation").await?;
            if nvidia_qualification::engine_prepared(configuration, &deployment).await? {
                return Ok(deployment);
            }
            let state = deployment
                .state
                .as_ref()
                .context("vLLM deployment has no state")?;
            anyhow::ensure!(
                state.status != ModelDeploymentStatus::Failed as i32,
                "vLLM deployment failed during engine preparation: {:?}",
                state.failure
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("vLLM engine preparation exceeded its deadline")?
}

async fn read_deployment(
    service: &mut ModelClient,
    deployment_id: &str,
    operation: &str,
) -> anyhow::Result<ModelDeployment> {
    service
        .list_model_deployments(Request::new(ListModelDeploymentsRequest {}))
        .await
        .context("read vLLM deployments")?
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .with_context(|| format!("vLLM deployment disappeared during {operation}"))
}

async fn pause(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    service
        .pause_model(Request::new(PauseModelRequest {
            model_deployment_id: deployment_id.to_owned(),
        }))
        .await
        .context("pause vLLM deployment")?;
    Ok(())
}

async fn resume(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    service
        .resume_model(Request::new(ResumeModelRequest {
            model_deployment_id: deployment_id.to_owned(),
        }))
        .await
        .context("resume vLLM deployment")?;
    Ok(())
}

async fn remove(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    service
        .remove_model_deployment(Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.to_owned(),
        }))
        .await
        .context("remove vLLM deployment")?;
    Ok(())
}

async fn wait_for_absence(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployments = service
                .list_model_deployments(Request::new(ListModelDeploymentsRequest {}))
                .await
                .context("read vLLM deployments during removal")?
                .into_inner()
                .deployments;
            if deployments
                .iter()
                .all(|deployment| deployment.model_deployment_id != deployment_id)
            {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("removed vLLM deployment remained in controller state")?
}

async fn wait_for_route(
    backend: &support::RunningBackend,
    address: std::net::SocketAddr,
) -> anyhow::Result<()> {
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            anyhow::ensure!(
                !backend.is_finished(),
                "routed backend stopped before exposing vLLM"
            );
            if let Ok(response) = raw_request(
                address,
                b"GET /v1/models HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
            )
            .await
                && response
                    .windows(ROUTE_NAME.len())
                    .any(|part| part == ROUTE_NAME.as_bytes())
            {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("route never exposed the ready vLLM deployment")?
}

async fn assert_exact_alias(port: u16, expected: &str) -> anyhow::Result<()> {
    let response = raw_request(
        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        b"GET /v1/models HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    )
    .await
    .context("vLLM model discovery endpoint is absent")?;
    let value = response_json(&response)?;
    let ids = value
        .get("data")
        .and_then(Value::as_array)
        .context("vLLM /v1/models has no data array")?
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        ids == [expected],
        "vLLM served the wrong alias: expected only {expected:?}, found {ids:?}"
    );
    Ok(())
}

async fn post_json(
    address: std::net::SocketAddr,
    path: &str,
    value: Value,
) -> anyhow::Result<Value> {
    response_json(&post_raw(address, path, value).await?)
}

async fn post_raw(
    address: std::net::SocketAddr,
    path: &str,
    value: Value,
) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(&value)?;
    let headers = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut request = headers.into_bytes();
    request.extend(body);
    raw_request(address, &request).await
}

async fn raw_request(address: std::net::SocketAddr, request: &[u8]) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(HTTP_TIMEOUT, async {
        let mut stream = TcpStream::connect(address)
            .await
            .context("connect inference HTTP")?;
        stream
            .write_all(request)
            .await
            .context("write inference HTTP request")?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .context("read inference HTTP response")?;
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .context("inference HTTP request timed out")?
}

fn response_json(response: &[u8]) -> anyhow::Result<Value> {
    let header_end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("inference response has no complete HTTP headers")?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    anyhow::ensure!(
        headers.starts_with("HTTP/1.1 200"),
        "inference request failed: {}",
        bounded_response(response)
    );
    serde_json::from_slice(&response[header_end + 4..]).context("decode inference JSON response")
}

fn sse_json_frames(response: &[u8]) -> anyhow::Result<Vec<Value>> {
    let header_end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("stream response has no complete HTTP headers")?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    anyhow::ensure!(
        headers.starts_with("HTTP/1.1 200"),
        "stream request failed: {}",
        bounded_response(response)
    );
    std::str::from_utf8(&response[header_end + 4..])?
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).context("decode streaming Chat frame"))
        .collect()
}

fn nonempty_string<'a>(value: Option<&'a Value>, field: &str) -> anyhow::Result<&'a str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{field} is missing or empty"))
}

fn bounded_response(response: &[u8]) -> String {
    String::from_utf8_lossy(response)
        .chars()
        .take(1_024)
        .collect()
}

/// Waits until the backend attributes GPU memory to the deployment through its production
/// process attribution, which must count the vLLM engine child of the container's root process,
/// and returns the GPU workload rooted at the container's process on the same device.
async fn wait_for_gpu_attribution(
    service: &mut ModelClient,
    deployment_id: &str,
    endpoint: &RuntimeEndpoint,
) -> anyhow::Result<GpuWorkload> {
    let attributed_device = tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployment = service
                .list_model_deployments(Request::new(ListModelDeploymentsRequest {}))
                .await
                .context("read vLLM deployments")?
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .context("vLLM deployment disappeared during GPU attribution")?;
            if let Some(accelerator_memory_observation::Result::Available(usage)) = deployment
                .accelerator_memory
                .and_then(|observation| observation.result)
            {
                let [device_usage] = usage.devices.as_slice() else {
                    anyhow::bail!("vLLM deployment memory spans devices: {:?}", usage.devices);
                };
                anyhow::ensure!(
                    device_usage.used_memory_bytes > 0,
                    "vLLM deployment has zero attributed GPU memory"
                );
                return Ok(device_usage.device_id.clone());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("ready vLLM workload never acquired attributed GPU memory")??;
    let root = u32::try_from(endpoint.pid)
        .with_context(|| format!("vLLM container has no host process: {}", endpoint.pid))?;
    let workload = nvidia_qualification::wait_for_workload(root, DEPLOYMENT_TIMEOUT).await?;
    anyhow::ensure!(
        workload.device_id == attributed_device,
        "vLLM deployment memory is attributed to {attributed_device}, but its processes run on {}",
        workload.device_id
    );
    Ok(workload)
}

async fn assert_container_absent(
    configuration: &SitePaths,
    deployment_id: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_local_defaults().context("connect Docker for leak check")?;
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))?;
    let filters = HashMap::from([(
        "label",
        vec![
            format!("{LABEL_CORRELATION}={deployment_id}"),
            format!("{LABEL_SITE}={}", site.site_id),
        ],
    )]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("list vLLM workloads after removal")?;
    anyhow::ensure!(
        containers.is_empty(),
        "removed vLLM deployment leaked Docker workloads: {:?}",
        containers
            .iter()
            .filter_map(|container| container.id.as_deref())
            .collect::<Vec<_>>()
    );
    Ok(())
}

fn assert_route(configuration: &SitePaths, deployment_id: &str) -> anyhow::Result<()> {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))?;
    let route = site
        .inference_routes
        .iter()
        .find(|route| route.name == ROUTE_NAME)
        .context("vLLM exposure route was not persisted")?;
    anyhow::ensure!(route.enabled, "vLLM exposure route is disabled");
    anyhow::ensure!(
        route.fallback_groups.len() == 1,
        "vLLM exposure route has wrong fallback groups"
    );
    let group = &route.fallback_groups[0];
    anyhow::ensure!(
        group.selection == FallbackGroupSelection::Single as i32,
        "vLLM exposure route is not single-selection"
    );
    anyhow::ensure!(
        group.model_deployment_ids == [deployment_id],
        "vLLM exposure route points at the wrong deployment"
    );
    Ok(())
}

fn assert_route_absent(configuration: &SitePaths) -> anyhow::Result<()> {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))?;
    anyhow::ensure!(
        site.inference_routes
            .iter()
            .all(|route| route.name != ROUTE_NAME),
        "removed vLLM deployment leaked its exposure route"
    );
    Ok(())
}

#[test]
fn small_site_catalog_fixture_loads_resolves_and_varies_only_vision() {
    for platform in [
        ExecutionPlatform::linux_amd64(),
        ExecutionPlatform::linux_arm64(),
    ] {
        let root = crate::support::canonical_tempdir().expect("temporary vLLM fixture root");
        nvidia_qualification::materialize_site(FIXTURE, &platform, root.path())
            .expect("materialize vLLM qualification fixture");
        InitialSiteInputs::load(root.path(), &dotcore::model::site_declaration_rules())
            .expect("vLLM qualification site must pass shared validation");

        let catalog = nvidia_qualification::load_catalog(root.path())
            .expect("load master engine release with test-only site catalog model");
        let text = qualification_case(&catalog, TEXT_PROFILE_ID, &platform, fixture_node_nvidia())
            .unwrap_or_else(|error| {
                panic!("resolve text fixture profile on {platform}: {error:#}")
            });
        let multimodal = qualification_case(
            &catalog,
            MULTIMODAL_PROFILE_ID,
            &platform,
            fixture_node_nvidia(),
        )
        .unwrap_or_else(|error| {
            panic!("resolve multimodal fixture profile on {platform}: {error:#}")
        });

        assert_eq!(
            &text.expected_command[text.expected_command.len() - 2..],
            [DEFAULT_CHAT_TEMPLATE_KWARGS_FLAG, THINKING_ENABLED_JSON]
        );
        for case in [&text, &multimodal] {
            assert!(
                nvidia_qualification::development_source_docker_image(&case.engine),
                "{platform} {}: {:?}",
                case.profile_id,
                case.engine
            );
            assert_eq!(case.engine.platform, Some(platform.to_oci_platform()));
        }
        assert!(!text.multimodal, "text fixture unexpectedly retains vision");
        assert!(multimodal.multimodal, "multimodal fixture omits vision");
        let language_only = text
            .expected_command
            .iter()
            .position(|token| token == "--language-model-only")
            .expect("text fixture profile must request language-only mode");
        let mut expected_multimodal = text.expected_command;
        expected_multimodal.remove(language_only);
        assert_eq!(multimodal.expected_command, expected_multimodal);
    }
}

/// Host-independent NVIDIA observation for the ungated fixture check, sized like an RTX 5090.
fn fixture_node_nvidia() -> NodeAcceleratorObservation {
    NodeAcceleratorObservation::nvidia(QUALIFICATION_NODE_ID, "580.173.02").with_device_memory(
        "GPU-0",
        Some(34_190_917_632),
        None,
    )
}
