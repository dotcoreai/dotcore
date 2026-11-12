//! Explicitly gated local-NVIDIA Docker lifecycle and completion integration test.
//!
//! This suite pins real NVIDIA Docker runtime execution, router compatibility,
//! and recovery for catalog-derived models. It is ignored without the explicit
//! hardware gate; ordinary lifecycle, protocol, and catalog contracts
//! intentionally exclude host qualification. It runs on the native Docker platform and on
//! dedicated or unified-memory GPUs.
//!
//! The engine comes from the master catalog as a development-source Docker image: the hosting
//! node evaluates it from the workspace, runs its loader and launches the image it tags, and the
//! suite checks that pin and image. Preparing the engine has its own deadline, so a cold image
//! does not count against the serving deadlines.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Cursor},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bollard::{
    Docker,
    models::ContainerInspectResponse,
    query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder},
};
use dotcore::composition::{
    Backend, BackendConfiguration, BackendRole, IsolatedModelService, SelectedModelService,
    ServerConfiguration, model_service_with_sources_for,
};
use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_domain::{ExecutionPlatform, SitePaths, nix::DotcoreSources};
use dotcore_proto::dotcore::{
    catalog::v1::{
        HuggingFaceArtifactSource, hugging_face_artifact_source::Content as HuggingFaceContent,
    },
    engine::{
        llama_cpp::v1::LlamaCppFixedModelSettings,
        v1::resolved_engine_model_state::State as ResolvedEngineState,
    },
    inference::v1::FallbackGroupSelection,
    model::v1::{
        CreateModelDeploymentRequest, DirectRouteExposure, ListModelDeploymentsRequest,
        ListModelDeploymentsResponse, ModelDeployment, ModelDeploymentDesiredState,
        ModelDeploymentExposure, ModelDeploymentSnapshot, ModelDeploymentStatus, PauseModelRequest,
        PreviewLocalModelDeploymentRequest, RemoveModelDeploymentRequest,
        RemoveModelDeploymentResponse, ResolvedEngineArtifact,
        ResolvedLocalModelDeploymentSnapshot, ResumeModelRequest, model_deployment_exposure,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        model_deployment_snapshot, portable_model_artifact_binding::Artifact as BoundArtifact,
        portable_model_launch_argument_token,
    },
    site::v1::Site,
};
use dotcore_runtime_backends::docker::OWNERSHIP_SCHEMA;
use dotcore_site::{
    initial::inputs::InitialSiteInputs,
    node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityReconciler,
    },
};
use prost::Message;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
};
use tonic::Request;

use crate::{
    decision_requests,
    nvidia_qualification::{self, EnginePin, QualificationSite},
    support,
};

const MODEL_ID: &str = "gemma-4-e2b";
const PROFILE_IDS: [&str; 2] = ["fast-start", "higher-precision"];
const SPLIT_MODEL_ID: &str = "stories15m-split-qualification";
const SPLIT_PROFILE_ID: &str = "split-text";
/// Total context the fast-start qualification supplies, below the engine default.
const NON_DEFAULT_TOTAL_CONTEXT: u64 = 30_000;
/// The llama.cpp release's engine default total context.
const DEFAULT_TOTAL_CONTEXT: u64 = 32_768;
const QUALIFICATION_NODE_ID: &str = "qualification-node";
const NVIDIA_GPU_MINIMUM_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Covers a cold engine preparation: building the loader and pulling the CUDA image layers.
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Allows one settled observer scan plus backend preparation after service drop.
const RESTART_TIMEOUT: Duration = Duration::from_secs(45);
const LABEL_CORRELATION: &str = "com.dotcore.workload.correlation";
const LABEL_SITE: &str = "com.dotcore.site";
const LABEL_SCHEMA: &str = "com.dotcore.runtime.schema";
const FIXTURE: &str = "llama-cpp-nvidia-site";
const NVIDIA_CDI_SELECTOR: &str = "nvidia.com/gpu=0";
const COMPLETION_DIAGNOSTIC_MAX_CHARS: usize = 256;
const MODEL_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_PROBE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MODEL_PROBE_MAX_RESPONSE_BYTES: usize = 64 * 1024;
const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS: usize = 256;
const IMAGE_PROMPT: &str = "Identify the dominant color of this image. Reply with one color word.";
const IMAGE_MAX_COMPLETION_TOKENS: u32 = 256;
const RED_JPEG: &str = "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/2wBDAQMDAwQDBAgEBAgQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/wAARCABAAEADAREAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAj/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFgEBAQEAAAAAAAAAAAAAAAAAAAcJ/8QAFBEBAAAAAAAAAAAAAAAAAAAAAP/aAAwDAQACEQMRAD8AnRDGqYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD//Z";
const BLUE_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABAAQMAAACQp+OdAAAAIGNIUk0AAHomAACAhAAA+gAAAIDoAAB1MAAA6mAAADqYAAAXcJy6UTwAAAAGUExURQAA/////3vcmSwAAAABYktHRAH/Ai3eAAAAD0lEQVQoz2NgGAWjgHwAAAJAAAGMxat3AAAAAElFTkSuQmCC";
const VALID_LLAMA_CPP_CUSTOM_ARGUMENTS: [&str; 2] = ["--reasoning-format", "none"];
const INVALID_LLAMA_CPP_ARGUMENT: &str = "--dotcore-invalid-llama-argument";
const STRATA_RTX_5090_QWEN3_6: StrataCase = StrataCase {
    model_id: "qwen3-6-35b-a3b",
    profile_id: "strata-q4-xl",
    minimum_free_bytes: STRATA_RTX_5090_MINIMUM_FREE_BYTES,
    ready_timeout: DEPLOYMENT_TIMEOUT,
    built_pack_path: None,
};
const STRATA_GB10_QWEN3_6: StrataCase = StrataCase {
    model_id: "qwen3-6-35b-a3b",
    profile_id: "strata-gb10-q4-xl",
    minimum_free_bytes: STRATA_GB10_MINIMUM_FREE_BYTES,
    ready_timeout: DEPLOYMENT_TIMEOUT,
    built_pack_path: None,
};
/// The IQ2_XS profile builds its pack under the data storage on first start.
const STRATA_RTX_5090_QWEN3_8_IQ2_XS: StrataCase = StrataCase {
    model_id: "qwen3-8-flash-next",
    profile_id: "iq2-xs",
    minimum_free_bytes: STRATA_RTX_5090_QWEN3_8_MINIMUM_FREE_BYTES,
    ready_timeout: STRATA_QWEN3_8_READY_TIMEOUT,
    built_pack_path: Some("pack"),
};
/// Where Strata's deployment-scoped cache storage is mounted in its container.
const STRATA_DATA_PATH: &str = "/data";
/// The profile's default expert cap and context hold about 22.6 GB of the RTX 5090.
const STRATA_RTX_5090_MINIMUM_FREE_BYTES: u64 = 24_000_000_000;
/// The profile's default context with every expert on the GB10 holds about 28 GB of host memory,
/// and the host keeps room for the OS.
const STRATA_GB10_MINIMUM_FREE_BYTES: u64 = 48_000_000_000;
/// The IQ2_XS profile's default expert cap and total context hold about 26.8 GB of the RTX 5090.
const STRATA_RTX_5090_QWEN3_8_MINIMUM_FREE_BYTES: u64 = 28_000_000_000;
/// Covers the first start of the 68 GB model: the pack build and reading the weights through the
/// host's page cache before the engine reports ready.
const STRATA_QWEN3_8_READY_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia"]
async fn hw_nvidia_lifecycle_and_completion() -> anyhow::Result<()> {
    let site = QualificationSite::new(FIXTURE)
        .await
        .context("prepare NVIDIA Docker qualification site")?;
    for profile_id in PROFILE_IDS {
        if let Some(reason) = site.unsupported_platform_reason(MODEL_ID, profile_id)? {
            eprintln!("[nvidia-docker] skipped: {reason}");
            return site.cleanup().await;
        }
    }
    let primary = async {
        let mut cases = qualification_cases(
            &site.catalog,
            &site.platform,
            &support::observed_node_nvidia(QUALIFICATION_NODE_ID),
        )?;
        record_engine_pins(&mut cases, &site.dotcore_sources).await?;
        nvidia_qualification::ensure_free_gpu_memory(
            "nvidia-docker",
            NVIDIA_GPU_MINIMUM_FREE_BYTES,
        )?;
        qualify_deployment_memory_api(&site.configuration, &site.dotcore_sources, &cases[0])
            .await?;
        let configuration = site.configuration.clone();
        let dotcore_sources = site.dotcore_sources.clone();
        tokio::spawn(async move {
            for case in cases {
                nvidia_qualification::ensure_free_gpu_memory(
                    "nvidia-docker",
                    NVIDIA_GPU_MINIMUM_FREE_BYTES,
                )?;
                qualify_model(&configuration, &dotcore_sources, case).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("NVIDIA Docker harness failed")?
    }
    .await;
    eprintln!("[nvidia-docker] operation=cleanup phase=before");
    let cleanup = site.cleanup().await;
    match &cleanup {
        Ok(()) => eprintln!("[nvidia-docker] operation=cleanup phase=after state=complete"),
        Err(error) => {
            eprintln!("[nvidia-docker] operation=cleanup phase=after state=failed error={error}")
        }
    }
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup.context("NVIDIA Docker harness cleanup failed")),
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("cleanup also failed: {cleanup}")))
        }
    }
}

/// Launches the Qwen3.8 27B llama.cpp profile at the largest fit the capacity component computes
/// on the idle GPU and compares the memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_llama_cpp_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "largest-fit-llama-cpp",
            model_id: "qwen3-8-27b",
            profile_id: "q3-xl",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Launches the Qwen3.8 27B llama.cpp profile at the largest fit on a GB10, whose GPU shares host
/// memory, and compares the host memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-gb10"]
async fn hw_gb10_llama_cpp_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "gb10-largest-fit-llama-cpp",
            model_id: "qwen3-8-27b",
            profile_id: "q3-xl",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Qualifies a split GGUF model artifact: the hard-linked publication mounts as a directory,
/// llama.cpp loads the set from its first shard and serves a completion, and a relaunch reuses
/// the publication without downloading or rehashing it.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia"]
async fn hw_nvidia_split_gguf_lifecycle_and_warm_reuse() -> anyhow::Result<()> {
    let site = QualificationSite::new(FIXTURE)
        .await
        .context("prepare NVIDIA Docker split qualification site")?;
    if let Some(reason) = site.unsupported_platform_reason(SPLIT_MODEL_ID, SPLIT_PROFILE_ID)? {
        eprintln!("[nvidia-docker] skipped: {reason}");
        return site.cleanup().await;
    }
    let primary = async {
        let mut case = qualification_case(
            &site.catalog,
            SPLIT_MODEL_ID,
            SPLIT_PROFILE_ID,
            &site.platform,
            &support::observed_node_nvidia(QUALIFICATION_NODE_ID),
        )?;
        record_engine_pins(std::slice::from_mut(&mut case), &site.dotcore_sources).await?;
        nvidia_qualification::ensure_free_gpu_memory(
            "nvidia-docker",
            NVIDIA_GPU_MINIMUM_FREE_BYTES,
        )?;
        let configuration = site.configuration.clone();
        let dotcore_sources = site.dotcore_sources.clone();
        tokio::spawn(
            async move { qualify_split_model(&configuration, &dotcore_sources, case).await },
        )
        .await
        .context("NVIDIA Docker split harness failed")?
    }
    .await;
    let cleanup = site.cleanup().await;
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => {
            Err(cleanup.context("NVIDIA Docker split harness cleanup failed"))
        }
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("cleanup also failed: {cleanup}")))
        }
    }
}

/// Qualifies clef-flash on the real engine through the router's decision route: a research
/// example round trip, and the client error of a request larger than the total context.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia"]
async fn hw_nvidia_clef_flash_serves_decisions_and_rejects_oversized_requests() -> anyhow::Result<()>
{
    let site = QualificationSite::new(FIXTURE)
        .await
        .context("prepare NVIDIA Docker decision site")?;
    if let Some(reason) = site
        .unsupported_platform_reason(decision_requests::MODEL_ID, decision_requests::PROFILE_ID)?
    {
        eprintln!("[nvidia-docker] skipped: {reason}");
        return site.cleanup().await;
    }
    let primary = async {
        nvidia_qualification::ensure_free_gpu_memory(
            "nvidia-docker",
            NVIDIA_GPU_MINIMUM_FREE_BYTES,
        )?;
        let configuration = site.configuration.clone();
        let dotcore_sources = site.dotcore_sources.clone();
        tokio::spawn(async move { qualify_decisions(&configuration, &dotcore_sources).await })
            .await
            .context("NVIDIA Docker decision harness failed")?
    }
    .await;
    let cleanup = site.cleanup().await;
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => {
            Err(cleanup.context("NVIDIA Docker decision harness cleanup failed"))
        }
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("cleanup also failed: {cleanup}")))
        }
    }
}

/// Qualifies Qwen3.6 35B A3B on Strata through the router on an RTX 5090: the deployment reaches
/// Ready, a chat completion answers under the route's model name, and removal deletes the
/// deployment's storage.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_strata_serves_chat_through_the_router_and_removes_its_storage()
-> anyhow::Result<()> {
    qualify_strata_site(&STRATA_RTX_5090_QWEN3_6).await
}

/// Qualifies Qwen3.8 Flash Next IQ2_XS on Strata through the router on an RTX 5090: the deployment
/// builds its pack and reaches Ready, a chat completion answers under the route's model name, and
/// removal deletes the deployment's storage, pack included.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_strata_qwen3_8_iq2_xs_serves_chat_through_the_router_and_removes_its_storage()
-> anyhow::Result<()> {
    qualify_strata_site(&STRATA_RTX_5090_QWEN3_8_IQ2_XS).await
}

/// Qualifies Qwen3.6 35B A3B on Strata through the router on a GB10, whose GPU shares host memory:
/// the deployment reaches Ready, a chat completion answers under the route's model name, and
/// removal deletes the deployment's storage.
#[tokio::test]
#[ignore = "hardware: just backend::test-gb10"]
async fn hw_gb10_strata_serves_chat_through_the_router_and_removes_its_storage()
-> anyhow::Result<()> {
    qualify_strata_site(&STRATA_GB10_QWEN3_6).await
}

/// Pinned catalog provenance of one selected artifact. `file` is the GGUF file llama.cpp loads:
/// the artifact itself, or the first shard of a split set whose shards are in `split_files`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogArtifact {
    artifact_id: String,
    weight_quantization: String,
    repository: String,
    revision: String,
    file: String,
    provenance_digest: String,
    split_files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
struct QualificationCase {
    model_id: String,
    profile_id: String,
    artifact: CatalogArtifact,
    draft: Option<CatalogArtifact>,
    projector: Option<CatalogArtifact>,
    engine: ResolvedEngineArtifact,

    /// Native platform the engine artifact was selected for.
    platform: ExecutionPlatform,
    settings: LlamaCppFixedModelSettings,
    snapshot: ModelDeploymentSnapshot,
    configured_snapshot: ModelDeploymentSnapshot,

    /// Engine pin this host must record, evaluated before a hardware run.
    engine_pin: Option<EnginePin>,
    engine_image_cache: Option<ImageCacheState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageCacheState {
    Cold,

    Warm,
}

impl ImageCacheState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

fn qualification_cases(
    catalog: &Catalog,
    platform: &ExecutionPlatform,
    nvidia: &NodeAcceleratorObservation,
) -> anyhow::Result<Vec<QualificationCase>> {
    PROFILE_IDS
        .iter()
        .map(|profile_id| qualification_case(catalog, MODEL_ID, profile_id, platform, nvidia))
        .collect()
}

/// Driver observation for tests that only exercise catalog resolution and launch arguments, so
/// they run on hosts without an NVIDIA driver. Lifecycle qualification uses the real observation.
fn synthetic_nvidia_observation() -> NodeAcceleratorObservation {
    NodeAcceleratorObservation::nvidia(QUALIFICATION_NODE_ID, "580.173.02")
}

fn qualification_case(
    catalog: &Catalog,
    model_id: &str,
    profile_id: &str,
    platform: &ExecutionPlatform,
    nvidia: &NodeAcceleratorObservation,
) -> anyhow::Result<QualificationCase> {
    let model = catalog
        .protobuf_records_without_sizing()
        .models
        .into_iter()
        .find(|model| model.model_id == model_id)
        .ok_or_else(|| anyhow!("Catalog model {model_id:?} is missing"))?;
    let profile = model
        .profiles
        .iter()
        .find(|profile| profile.profile_id == profile_id)
        .ok_or_else(|| anyhow!("Catalog model profile {profile_id:?} is missing"))?;
    let profile_id = profile.profile_id.clone();
    let catalog_model_id = CatalogModelId(model_id.to_owned());
    let configuration = support::local_model_configuration(
        &profile.default_model_artifact_binding_id,
        qualification_total_context(&profile_id),
    );
    let target = dotcore_proto::dotcore::site::v1::ExecutionTarget {
        execution_target_id: "qualification-target".to_owned(),
        display_name: "qualification-target".to_owned(),
        site_node_id: QUALIFICATION_NODE_ID.to_owned(),
        accelerator_device_ids: vec!["0".to_owned()],
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
            "qualification-target",
            platform.clone(),
        )],
        [nvidia.clone()],
    );
    let local = dotcore::model::resolution::resolve_local_launch(
        catalog,
        &catalog_model_id,
        &profile_id,
        &configuration,
        &target,
        &inventory,
        None,
    )?;
    let snapshot = ModelDeploymentSnapshot {
        source: Some(
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                local.clone(),
            ),
        ),
    };
    let configured_snapshot = configured_qualification_snapshot(&snapshot)?;
    let settings = llama_cpp_settings(&snapshot)?;
    let selected_artifact_id = model
        .configurations
        .iter()
        .find(|configuration| configuration.configuration_id == profile.configuration_id)
        .and_then(|configuration| {
            configuration
                .artifact_bindings
                .iter()
                .find(|binding| binding.binding_id == profile.default_model_artifact_binding_id)
        })
        .map(|binding| binding.model_artifact_id.as_str())
        .ok_or_else(|| anyhow!("Catalog profile {profile_id:?} has no default artifact binding"))?;
    let selected_artifact = model
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == selected_artifact_id)
        .ok_or_else(|| anyhow!("Catalog model {model_id:?} has no selected artifact"))?;
    let artifact = catalog_artifact(
        &selected_artifact.artifact_id,
        &selected_artifact.weight_quantization,
        selected_artifact
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("Catalog model {model_id:?} has no primary artifact source"))?,
    )?;
    let resolved_artifact = local
        .model_artifact
        .as_ref()
        .and_then(|resolved| resolved.source.as_ref())
        .ok_or_else(|| anyhow!("Catalog profile {profile_id:?} resolves no primary artifact"))?;
    if catalog_artifact(
        &artifact.artifact_id,
        &artifact.weight_quantization,
        resolved_artifact,
    )? != artifact
    {
        anyhow::bail!("resolved primary artifact differs from catalog provenance");
    }
    let draft = bound_auxiliary_artifact(&local, "speculative-draft", "mtp-draft")?;
    let projector = bound_auxiliary_artifact(&local, "vision-projector", "projector")?;
    let engine = local
        .engine_artifact
        .clone()
        .ok_or_else(|| anyhow!("Catalog profile {profile_id:?} resolves no engine artifact"))?;
    if engine.platform.as_ref().is_none_or(|resolved| {
        resolved.operating_system != platform.operating_system()
            || resolved.architecture != platform.architecture()
    }) || engine.accelerator != "nvidia"
    {
        anyhow::bail!(
            "Catalog profile {profile_id:?} does not resolve to the native {}/{} NVIDIA engine artifact",
            platform.operating_system(),
            platform.architecture(),
        );
    }
    if !nvidia_qualification::development_source_docker_image(&engine) {
        anyhow::bail!(
            "Catalog engine artifact for profile {profile_id:?} is not a development-source Docker-delivered Nix artifact"
        );
    }
    Ok(QualificationCase {
        model_id: model.model_id,
        profile_id,
        artifact,
        draft,
        projector,
        engine,
        platform: platform.clone(),
        settings,
        snapshot,
        configured_snapshot,
        engine_pin: None,
        engine_image_cache: None,
    })
}

/// Returns the pinned provenance of a Hugging Face artifact source: its file, or the first shard
/// of a split snapshot with every shard.
fn catalog_artifact(
    artifact_id: &str,
    weight_quantization: &str,
    source: &HuggingFaceArtifactSource,
) -> anyhow::Result<CatalogArtifact> {
    let (file, provenance_digest, split_files) = match source.content.as_ref() {
        Some(HuggingFaceContent::File(file)) => (
            file.path.clone(),
            file.provenance_digest.clone(),
            BTreeMap::new(),
        ),
        Some(HuggingFaceContent::Snapshot(snapshot)) => {
            let entry = snapshot
                .files
                .iter()
                .find(|file| file.path.contains("-00001-of-"))
                .ok_or_else(|| anyhow!("artifact {artifact_id:?} has no first split shard"))?;
            (
                entry.path.clone(),
                entry.provenance_digest.clone(),
                snapshot
                    .files
                    .iter()
                    .map(|file| (file.path.clone(), file.provenance_digest.clone()))
                    .collect(),
            )
        }
        None => anyhow::bail!("artifact {artifact_id:?} has no content"),
    };
    Ok(CatalogArtifact {
        artifact_id: artifact_id.to_owned(),
        weight_quantization: weight_quantization.to_owned(),
        repository: source.repository.clone(),
        revision: source.revision.clone(),
        file,
        provenance_digest,
        split_files,
    })
}

/// Returns the auxiliary artifact the snapshot's launch plan binds under `binding_id`, if any.
fn bound_auxiliary_artifact(
    local: &ResolvedLocalModelDeploymentSnapshot,
    binding_id: &str,
    artifact_id: &str,
) -> anyhow::Result<Option<CatalogArtifact>> {
    let plan = local
        .desired_launch_plan
        .as_ref()
        .ok_or_else(|| anyhow!("Catalog snapshot has no desired launch plan"))?;
    let Some(binding) = plan
        .artifact_bindings
        .iter()
        .find(|binding| binding.binding_id == binding_id)
    else {
        return Ok(None);
    };
    let Some(BoundArtifact::AuxiliaryArtifactId(auxiliary_id)) = binding.artifact.as_ref() else {
        anyhow::bail!("launch binding {binding_id:?} names no auxiliary artifact");
    };
    let source = local
        .auxiliary_artifacts
        .iter()
        .find(|artifact| &artifact.auxiliary_artifact_id == auxiliary_id)
        .and_then(|artifact| artifact.source.as_ref())
        .ok_or_else(|| anyhow!("Catalog snapshot has no auxiliary artifact {auxiliary_id:?}"))?;
    catalog_artifact(artifact_id, "", source).map(Some)
}

fn llama_cpp_settings(
    snapshot: &ModelDeploymentSnapshot,
) -> anyhow::Result<LlamaCppFixedModelSettings> {
    snapshot
        .source
        .as_ref()
        .and_then(|source| match source {
            model_deployment_snapshot::Source::Local(local) => local.resolved_engine_state.as_ref(),
            model_deployment_snapshot::Source::Upstream(_) => None,
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            ResolvedEngineState::LlamaCpp(state) => state.fixed_settings.clone(),
            ResolvedEngineState::Vllm(_)
            | ResolvedEngineState::Ninfer(_)
            | ResolvedEngineState::Strata(_) => None,
        })
        .ok_or_else(|| anyhow!("Catalog snapshot has no llama.cpp fixed settings"))
}

fn configured_qualification_snapshot(
    catalog_snapshot: &ModelDeploymentSnapshot,
) -> anyhow::Result<ModelDeploymentSnapshot> {
    let mut configured = catalog_snapshot.clone();
    let local = match configured.source.as_mut() {
        Some(model_deployment_snapshot::Source::Local(local)) => local,
        _ => anyhow::bail!("Catalog snapshot is not local"),
    };
    local.custom_argument_tokens = VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned).to_vec();
    let plan = local
        .desired_launch_plan
        .as_mut()
        .ok_or_else(|| anyhow!("Catalog snapshot has no desired launch plan"))?;
    plan.arguments
        .extend(VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(|argument| {
            dotcore_proto::dotcore::model::v1::PortableModelLaunchArgumentToken {
                value: Some(portable_model_launch_argument_token::Value::Literal(
                    argument.to_owned(),
                )),
            }
        }));
    Ok(configured)
}

/// Evaluates each case's engine pin and records whether its loader's image is already in Docker,
/// which tells a cold engine preparation from a warm one.
async fn record_engine_pins(
    cases: &mut [QualificationCase],
    dotcore_sources: &DotcoreSources,
) -> anyhow::Result<()> {
    for case in cases {
        let pin = EnginePin::evaluate(dotcore_sources, &case.engine).await?;
        let state = if pin.image_present().await? {
            ImageCacheState::Warm
        } else {
            ImageCacheState::Cold
        };
        eprintln!(
            "[nvidia-docker] Model={} operation=engine image preflight phase=after state={} store_path={} image={} platform={}/{}",
            case.model_id,
            state.as_str(),
            pin.store_path,
            pin.launch_reference,
            case.platform.operating_system(),
            case.platform.architecture(),
        );
        case.engine_pin = Some(pin);
        case.engine_image_cache = Some(state);
    }
    Ok(())
}

async fn qualify_deployment_memory_api(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    case: &QualificationCase,
) -> anyhow::Result<()> {
    let service = wait_for_released_backend(
        configuration,
        dotcore_sources,
        &case.model_id,
        "deployment-memory setup",
    )
    .await;
    service.start_observer();
    let deployment = load_model(&service, configuration, case).await;
    let deployment_id = deployment.model_deployment_id.clone();
    wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "deployment-memory setup",
    )
    .await;
    drop(service);

    // The backend's own shutdown future stops it, so its node tasks release the site stores
    // before the next composition opens them.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let backend_configuration = BackendConfiguration {
        site_paths: configuration.clone(),
        telemetry_directory: None,
        dotcore_sources: dotcore_sources.clone(),
        role: BackendRole::Manager(ServerConfiguration {
            management_bind_address: "127.0.0.1:0".parse().expect("management address"),
            inference_bind_address: "127.0.0.1:0".parse().expect("inference address"),
            private_listener: dotcore::composition::ManagerPrivateListener::SiteDefault { port: 0 },
            allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
        }),
    };
    let backend = tokio::time::timeout(RESTART_TIMEOUT, async {
        loop {
            match Backend::prepare(
                backend_configuration.clone(),
                shutdown.clone().cancelled_owned(),
            )
            .await
            {
                Ok(backend) => break backend,
                Err(error) if error.to_string().contains("already locked") => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("deployment-memory backend restart failed: {error}"),
            }
        }
    })
    .await
    .context("deployment-memory backend restart timeout")?;
    let management_address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let response: ListModelDeploymentsResponse = management_grpc_call(
                management_address,
                "/dotcore.model.v1.ModelDeploymentService/ListModelDeployments",
                &ListModelDeploymentsRequest {},
                &mut server,
            )
            .await;
            let deployment = response
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .expect("deployment-memory qualification deployment");
            let state = deployment.state.as_ref().expect("deployment state");
            assert_ne!(
                state.status,
                ModelDeploymentStatus::Failed as i32,
                "deployment-memory qualification failed: {:?}",
                state.failure,
            );
            if state.status == ModelDeploymentStatus::Ready as i32
                && deployment.accelerator_memory.as_ref().is_some_and(|observation| {
                    matches!(
                        observation.result,
                        Some(dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::Available(_))
                    )
                })
            {
                let observation = deployment
                    .accelerator_memory
                    .expect("available accelerator-memory observation");
                let Some(dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::Available(usage)) = observation.result else {
                    unreachable!("availability checked above");
                };
                assert!(observation.observed_at.is_some());
                assert_eq!(usage.devices.len(), 1);
                assert_eq!(usage.devices[0].managed_node_id, "development-node");
                assert!(usage.devices[0].device_id.starts_with("GPU-"));
                assert!(usage.devices[0].used_memory_bytes > 0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("deployment-memory qualification timeout")?;

    let _: RemoveModelDeploymentResponse = management_grpc_call(
        management_address,
        "/dotcore.model.v1.ModelDeploymentService/RemoveModelDeployment",
        &RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        },
        &mut server,
    )
    .await;
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let response: ListModelDeploymentsResponse = management_grpc_call(
                management_address,
                "/dotcore.model.v1.ModelDeploymentService/ListModelDeployments",
                &ListModelDeploymentsRequest {},
                &mut server,
            )
            .await;
            if response
                .deployments
                .iter()
                .all(|deployment| deployment.model_deployment_id != deployment_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("deployment-memory teardown timeout")?;

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .context("deployment-memory backend shutdown timeout")?
        .context("deployment-memory backend task")?
        .context("deployment-memory backend result")?;
    Ok(())
}

async fn management_grpc_call<Request, Response>(
    address: std::net::SocketAddr,
    path: &str,
    request: &Request,
    server: &mut tokio::task::JoinHandle<dotcore::composition::CompositionResult<()>>,
) -> Response
where
    Request: Message,
    Response: Message + Default,
{
    let message = request.encode_to_vec();
    let mut frame = Vec::with_capacity(message.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(&message);
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        frame.len()
    )
    .into_bytes();
    request.extend_from_slice(&frame);

    let mut stream = loop {
        tokio::select! {
            result = &mut *server => {
                panic!("Backend stopped before deployment-memory request: {:?}", result);
            }
            result = TcpStream::connect(address) => match result {
                Ok(stream) => break stream,
                Err(_) => tokio::task::yield_now().await,
            },
        }
    };
    stream.write_all(&request).await.expect("gRPC-Web request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("gRPC-Web response");
    decode_grpc_web_message(&response)
}

fn decode_grpc_web_message<Response: Message + Default>(response: &[u8]) -> Response {
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let headers_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response headers")
        + 4;
    let headers = &response[..headers_end];
    let body = &response[headers_end..];
    let frame = if String::from_utf8_lossy(headers)
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let chunk_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("HTTP response chunk");
        let chunk_length = usize::from_str_radix(
            std::str::from_utf8(&body[..chunk_end]).expect("chunk length"),
            16,
        )
        .expect("hex chunk length");
        let chunk_start = chunk_end + 2;
        &body[chunk_start..chunk_start + chunk_length]
    } else {
        body
    };
    assert!(
        !frame.is_empty(),
        "gRPC-Web response has no body: {}",
        String::from_utf8_lossy(response)
    );
    assert_eq!(
        frame[0],
        0,
        "first gRPC-Web frame is data: {}",
        String::from_utf8_lossy(&frame[5..])
    );
    Response::decode(&frame[5..]).expect("gRPC-Web protobuf response")
}

async fn qualify_model(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    case: QualificationCase,
) {
    let qualification_started = Instant::now();
    let engine_pin = case.engine_pin.clone().expect("evaluated engine pin");
    eprintln!(
        "[nvidia-docker] Model={} operation=qualification phase=before profile={}",
        case.model_id, case.profile_id
    );
    let cache = &configuration.model_artifact_cache_root();
    eprintln!(
        "[nvidia-docker] Model={} operation=cache verification phase=before state=shared",
        case.model_id
    );
    eprintln!(
        "[nvidia-docker] Model={} operation=cache verification phase=after state=shared",
        case.model_id
    );
    let service = wait_for_released_backend(
        configuration,
        dotcore_sources,
        &case.model_id,
        "backend release",
    )
    .await;
    service.start_observer();

    let cold_started = Instant::now();
    let deployment = load_model(&service, configuration, &case).await;
    assert_configured_snapshot(&deployment, &case.configured_snapshot);
    assert_qualification_deployment(&deployment, &case);
    let deployment_id = deployment.model_deployment_id.clone();
    assert_persisted_qualification_route(configuration, &case.model_id, &deployment_id);
    wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "cold lifecycle wait",
    )
    .await;
    let cold_duration = cold_started.elapsed();
    assert_eq!(
        ModelDeploymentDesiredState::try_from(deployment.desired_state),
        Ok(ModelDeploymentDesiredState::Present),
    );
    let endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "cold endpoint discovery",
    )
    .await
    .expect("inspect cold READY NVIDIA Docker model endpoint");
    assert_eq!(
        endpoint.image, engine_pin.launch_reference,
        "cold NVIDIA Docker container runs the loader's image"
    );
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=before",
        case.model_id
    );
    assert_cache_mount(&endpoint, cache);
    assert_selected_artifact_workload(&endpoint, cache, &case.artifact);
    assert_total_context_argument(&endpoint, qualification_total_context(&case.profile_id));
    assert_valid_custom_arguments_in_command(&endpoint);
    assert_auxiliary_artifact_command(&endpoint, case.projector.as_ref(), case.draft.as_ref());
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=after state=confirmed",
        case.model_id
    );
    let cold_http_port = endpoint.http_port.expect(
        "cold READY NVIDIA Docker model must publish its 8080 loopback host port before direct inference",
    );
    assert_exact_alias(cold_http_port, &case.model_id).await;
    let completion_duration = assert_completion(cold_http_port, &case.model_id).await;
    unload_and_wait(
        &service,
        configuration,
        &case.model_id,
        &deployment_id,
        "cold unload",
    )
    .await;

    let warm_started = Instant::now();
    let warm = load_model(&service, configuration, &case).await;
    assert_configured_snapshot(&warm, &case.configured_snapshot);
    assert_qualification_deployment(&warm, &case);
    // Deployment identities are never reused, so the warm load is a new deployment.
    assert_ne!(
        warm.model_deployment_id, deployment_id,
        "warm load reused the unloaded deployment identity",
    );
    let deployment_id = warm.model_deployment_id.clone();
    wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "warm lifecycle wait",
    )
    .await;
    let warm_duration = warm_started.elapsed();
    let warm_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "warm endpoint discovery",
    )
    .await
    .expect("inspect warm READY NVIDIA Docker model endpoint");
    assert_total_context_argument(
        &warm_endpoint,
        qualification_total_context(&case.profile_id),
    );
    let warm_http_port = warm_endpoint.http_port.expect(
        "warm READY NVIDIA Docker model must restore its 8080 loopback host port before inference",
    );
    assert_exact_alias(warm_http_port, &case.model_id).await;
    let retained_artifacts = cache_snapshot(cache).expect("retained model artifacts");
    assert!(
        !retained_artifacts.is_empty(),
        "Model artifact cache is empty"
    );
    let running_gpu = nvidia_qualification::wait_for_workload(
        u32::try_from(warm_endpoint.pid).expect("warm NVIDIA model container host process"),
        DEPLOYMENT_TIMEOUT,
    )
    .await
    .expect("NVIDIA model GPU process and memory allocation");
    let pausing = pause_model(&service, &case.model_id, &deployment_id).await;
    assert_eq!(
        ModelDeploymentDesiredState::try_from(pausing.desired_state),
        Ok(ModelDeploymentDesiredState::Paused),
    );
    let paused = wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Paused,
        "pause lifecycle wait",
    )
    .await;
    assert_configured_snapshot(&paused, &case.configured_snapshot);
    let paused_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "paused endpoint discovery",
    )
    .await
    .expect("inspect paused retained NVIDIA Docker model endpoint");
    assert_stopped_endpoint(&paused_endpoint);
    assert_eq!(
        paused_endpoint.container_id, warm_endpoint.container_id,
        "Pause replaced the retained Docker container",
    );
    assert_mounts_equal(&paused_endpoint, &warm_endpoint);
    assert_eq!(paused_endpoint.command, warm_endpoint.command);
    assert_total_context_argument(
        &paused_endpoint,
        qualification_total_context(&case.profile_id),
    );
    assert_eq!(
        cache_snapshot(cache).expect("paused model artifact cache"),
        retained_artifacts,
        "Pause changed retained model artifacts",
    );
    let released_gpu = nvidia_qualification::wait_for_release(&running_gpu, DEPLOYMENT_TIMEOUT)
        .await
        .expect("NVIDIA model GPU process and memory release");
    eprintln!(
        "[nvidia-docker] Model={} operation=pause retention phase=after state=paused container_id={} pid={} gpu_processes={:?} gpu_process_memory_bytes={} gpu_memory_after=({}) artifacts={} mounts={}",
        case.model_id,
        paused_endpoint.container_id,
        paused_endpoint.pid,
        running_gpu.process_ids,
        running_gpu.process_memory_bytes,
        released_gpu.memory,
        retained_artifacts.len(),
        paused_endpoint.mounts.len(),
    );
    let resumed = resume_model(&service, &case.model_id, &deployment_id).await;
    assert_eq!(
        ModelDeploymentDesiredState::try_from(resumed.desired_state),
        Ok(ModelDeploymentDesiredState::Present),
    );
    let resumed_ready = wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "resume lifecycle wait",
    )
    .await;
    assert_configured_snapshot(&resumed_ready, &case.configured_snapshot);
    let resumed_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "resumed endpoint discovery",
    )
    .await
    .expect("inspect resumed READY retained NVIDIA Docker model endpoint");
    let resumed_http_port = resumed_endpoint.http_port.expect(
        "retained Resume READY NVIDIA Docker model must restore its 8080 loopback host port before inference",
    );
    assert_exact_alias(resumed_http_port, &case.model_id).await;
    assert_eq!(
        resumed_endpoint.container_id, warm_endpoint.container_id,
        "Resume recreated a valid retained Docker container",
    );
    assert_mounts_equal(&resumed_endpoint, &warm_endpoint);
    assert_eq!(resumed_endpoint.command, warm_endpoint.command);
    assert_total_context_argument(
        &resumed_endpoint,
        qualification_total_context(&case.profile_id),
    );
    assert_eq!(
        cache_snapshot(cache).expect("resumed model artifact cache"),
        retained_artifacts,
        "Resume changed retained model artifacts",
    );

    pause_model(&service, &case.model_id, &deployment_id).await;
    wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Paused,
        "missing-container pause lifecycle wait",
    )
    .await;
    let missing_container = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "missing-container endpoint discovery",
    )
    .await
    .expect("inspect paused missing-container NVIDIA Docker model endpoint");
    assert_stopped_endpoint(&missing_container);
    assert_total_context_argument(
        &missing_container,
        qualification_total_context(&case.profile_id),
    );
    let docker = Docker::connect_with_local_defaults().expect("Docker connection for drift");
    let remove_options = RemoveContainerOptionsBuilder::default().force(true).build();
    docker
        .remove_container(&missing_container.container_id, Some(remove_options))
        .await
        .expect("remove retained Docker container for drift test");
    let unavailable = wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Unavailable,
        "missing-container paused drift wait",
    )
    .await;
    assert_eq!(
        ModelDeploymentDesiredState::try_from(unavailable.desired_state),
        Ok(ModelDeploymentDesiredState::Paused),
    );
    assert_model_container_absent(configuration, &deployment_id).await;
    let healed = resume_model(&service, &case.model_id, &deployment_id).await;
    assert_eq!(
        ModelDeploymentDesiredState::try_from(healed.desired_state),
        Ok(ModelDeploymentDesiredState::Present),
    );
    let healed_ready = wait_for_lifecycle(
        &service,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "missing-container resume lifecycle wait",
    )
    .await;
    assert_configured_snapshot(&healed_ready, &case.configured_snapshot);
    let healed_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "missing-container healed endpoint discovery",
    )
    .await
    .expect("inspect healed READY NVIDIA Docker model endpoint");
    assert_ne!(
        healed_endpoint.container_id, missing_container.container_id,
        "missing retained Docker container was not recreated after Resume",
    );
    assert_mounts_equal(&healed_endpoint, &missing_container);
    assert_eq!(healed_endpoint.command, missing_container.command);
    assert_total_context_argument(
        &healed_endpoint,
        qualification_total_context(&case.profile_id),
    );
    assert_eq!(
        cache_snapshot(cache).expect("healed model artifact cache"),
        retained_artifacts,
        "missing-container recovery changed retained model artifacts",
    );
    eprintln!(
        "[nvidia-docker] Model={} operation=pause-resume retention phase=after state=complete retained_container_id={} resumed_container_id={} missing_container_id={} healed_container_id={} missing_container_recovery=resume-only",
        case.model_id,
        paused_endpoint.container_id,
        resumed_endpoint.container_id,
        missing_container.container_id,
        healed_endpoint.container_id,
    );
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=before state=warm",
        case.model_id
    );
    assert_cache_mount(&warm_endpoint, cache);
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=after state=warm",
        case.model_id
    );
    drop(service);
    drop(
        wait_for_released_backend(
            configuration,
            dotcore_sources,
            &case.model_id,
            "backend release before router Chat",
        )
        .await,
    );
    let healed_http_port = healed_endpoint.http_port.expect(
        "healed READY NVIDIA Docker model must publish its 8080 loopback host port before router inference",
    );
    assert_router_chat(configuration, dotcore_sources, &case, healed_http_port).await;

    eprintln!(
        "[nvidia-docker] Model={} operation=restart/recovery phase=before",
        case.model_id
    );
    let restarted = wait_for_released_backend(
        configuration,
        dotcore_sources,
        &case.model_id,
        "restart backend release",
    )
    .await;
    restarted.start_observer();
    let recovery_started = Instant::now();
    let recovered = wait_for_lifecycle(
        &restarted,
        &case.model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "recovery lifecycle wait",
    )
    .await;
    assert_configured_snapshot(&recovered, &case.configured_snapshot);
    let recovery_duration = recovery_started.elapsed();
    assert_eq!(
        ModelDeploymentDesiredState::try_from(recovered.desired_state),
        Ok(ModelDeploymentDesiredState::Present),
    );
    let recovered_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &deployment_id,
        "recovery endpoint discovery",
    )
    .await
    .expect("inspect recovered READY NVIDIA Docker model endpoint");
    let recovered_http_port = recovered_endpoint.http_port.expect(
        "restart recovery READY NVIDIA Docker model must restore its 8080 loopback host port before inference",
    );
    assert_exact_alias(recovered_http_port, &case.model_id).await;
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=before state=recovery",
        case.model_id
    );
    assert_cache_mount(&recovered_endpoint, cache);
    eprintln!(
        "[nvidia-docker] Model={} operation=cache mount verification phase=after state=recovery",
        case.model_id
    );
    assert_total_context_argument(
        &recovered_endpoint,
        qualification_total_context(&case.profile_id),
    );
    assert_valid_custom_arguments_in_command(&recovered_endpoint);
    assert_eq!(
        healed_endpoint.container_id, recovered_endpoint.container_id,
        "persisted-Present recovery recreated the Docker container",
    );
    unload_and_wait(
        &restarted,
        configuration,
        &case.model_id,
        &deployment_id,
        "recovery unload",
    )
    .await;

    let invalid = load_invalid_custom_argument_model(&restarted, &case).await;
    let failed = wait_for_failed_lifecycle(
        &restarted,
        &case.model_id,
        &invalid.model_deployment_id,
        "invalid custom argument startup wait",
    )
    .await;
    assert_invalid_custom_argument_failure(&failed);
    let invalid_endpoint = runtime_endpoint(
        configuration,
        &case.model_id,
        &invalid.model_deployment_id,
        "invalid custom argument runtime inspection",
    )
    .await
    .expect("inspect stopped NVIDIA Docker model with invalid custom argument");
    assert!(
        invalid_endpoint
            .command
            .iter()
            .any(|argument| argument == INVALID_LLAMA_CPP_ARGUMENT),
        "llama.cpp did not receive the invalid custom argv token",
    );
    unload_and_wait(
        &restarted,
        configuration,
        &case.model_id,
        &invalid.model_deployment_id,
        "invalid custom argument unload",
    )
    .await;
    eprintln!(
        "[nvidia-docker] Model={} operation=restart/recovery phase=after state=complete",
        case.model_id
    );
    eprintln!(
        "NVIDIA Docker integration-test evidence: model={} profile={} accelerator_transport=cdi accelerator_selector={} engine_release={} engine_version={} engine_store_path={} engine_image={} engine_image_cache={} engine_acquisition=loader model_repository={} model_revision={} model_file={} model_provenance_digest={} draft={:?} context={} gpu_layers={} flash_attention={} mtp={} cold_load_ms={} completion_ms={} warm_reload_ms={} present_recovery_outcome=recovered present_recovery_ms={} cache=retained egress=ordinary cleanup=confirmed",
        case.model_id,
        case.profile_id,
        NVIDIA_CDI_SELECTOR,
        case.engine.engine_release_id,
        case.engine.engine_version,
        engine_pin.store_path,
        engine_pin.launch_reference,
        case.engine_image_cache
            .expect("recorded pre-run engine image cache state")
            .as_str(),
        case.artifact.repository,
        case.artifact.revision,
        case.artifact.file,
        case.artifact.provenance_digest,
        case.draft,
        qualification_total_context(&case.profile_id),
        case.settings.gpu_layers,
        case.settings.flash_attention,
        case.settings.speculative.is_some(),
        cold_duration.as_millis(),
        completion_duration.as_millis(),
        warm_duration.as_millis(),
        recovery_duration.as_millis(),
    );
    eprintln!(
        "[nvidia-docker] Model={} operation=qualification phase=after state=complete elapsed_ms={}",
        case.model_id,
        qualification_started.elapsed().as_millis()
    );
}

async fn qualify_split_model(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    case: QualificationCase,
) -> anyhow::Result<()> {
    let cache = configuration.model_artifact_cache_root();
    let service = wait_for_released_backend(
        configuration,
        dotcore_sources,
        &case.model_id,
        "backend release",
    )
    .await;
    service.start_observer();

    let mut cache_after_cold = None;
    for phase in ["cold", "warm"] {
        let deployment = load_model(&service, configuration, &case).await;
        let deployment_id = deployment.model_deployment_id.clone();
        wait_for_lifecycle(
            &service,
            &case.model_id,
            &deployment_id,
            ModelDeploymentStatus::Ready,
            &format!("{phase} lifecycle wait"),
        )
        .await;
        let endpoint = runtime_endpoint(
            configuration,
            &case.model_id,
            &deployment_id,
            &format!("{phase} endpoint discovery"),
        )
        .await
        .with_context(|| format!("inspect {phase} READY split model endpoint"))?;
        let engine_pin = case.engine_pin.as_ref().expect("evaluated engine pin");
        anyhow::ensure!(
            endpoint.image == engine_pin.launch_reference,
            "{phase} split container image {} is not the loader's image {}",
            endpoint.image,
            engine_pin.launch_reference
        );
        assert_split_artifact_workload(&endpoint, &cache, &case.artifact);
        let port = endpoint
            .http_port
            .ok_or_else(|| anyhow!("{phase} split model publishes no HTTP port"))?;
        assert_generation(port, &case.model_id).await;
        unload_and_wait(
            &service,
            configuration,
            &case.model_id,
            &deployment_id,
            &format!("{phase} unload"),
        )
        .await;

        let modifications = cache_modifications(&cache)?;
        match &cache_after_cold {
            None => cache_after_cold = Some(modifications),
            Some(cold) => assert_eq!(
                &modifications, cold,
                "warm split relaunch downloaded or re-verified cache content"
            ),
        }
        eprintln!(
            "[nvidia-docker] Model={} operation={phase} split qualification phase=after state=complete cache_files={}",
            case.model_id,
            cache_after_cold.as_ref().map_or(0, BTreeMap::len)
        );
    }
    Ok(())
}

/// Deploys clef-flash, then sends the decision requests through a router started on the site.
async fn qualify_decisions(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
) -> anyhow::Result<()> {
    let model_id = decision_requests::MODEL_ID;
    let service =
        wait_for_released_backend(configuration, dotcore_sources, model_id, "backend release")
            .await;
    service.start_observer();
    let local = dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
        model_id: model_id.to_owned(),
        profile_id: decision_requests::PROFILE_ID.to_owned(),
        placement: Some(support::exact_placement(
            "development-node",
            "nvidia-target",
        )),
        configuration: Some(decision_requests::decision_model_configuration()),
    };
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(qualification_load_request(local)),
    )
    .await
    .context("load clef-flash")?
    .into_inner()
    .deployment
    .context("loaded clef-flash deployment")?;
    let deployment_id = deployment.model_deployment_id;
    wait_for_lifecycle(
        &service,
        model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "decision lifecycle wait",
    )
    .await;
    let endpoint = runtime_endpoint(
        configuration,
        model_id,
        &deployment_id,
        "decision endpoint discovery",
    )
    .await?;
    assert_total_context_argument(&endpoint, decision_requests::DECISION_TOTAL_CONTEXT);
    let engine_port = endpoint
        .http_port
        .context("READY clef-flash must publish its loopback host port")?;

    // The router needs the backend's state lock back.
    drop(service);
    drop(
        wait_for_released_backend(
            configuration,
            dotcore_sources,
            model_id,
            "backend release before router",
        )
        .await,
    );
    let router = start_router(configuration, dotcore_sources, model_id, engine_port).await;
    let started = Instant::now();
    let research = router_request(
        router.inference_address,
        &decision_requests::research_example_http(model_id),
    )
    .await;
    decision_requests::assert_research_example_response(model_id, &research);
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=decision research example elapsed_ms={}",
        started.elapsed().as_millis()
    );
    let oversized = router_request(
        router.inference_address,
        &decision_requests::oversized_http(model_id),
    )
    .await;
    decision_requests::assert_context_length_exceeded_response(&oversized);
    router.stop().await;

    let service = wait_for_released_backend(
        configuration,
        dotcore_sources,
        model_id,
        "backend release before unload",
    )
    .await;
    // Removal completes through the observer's reconciliation.
    service.start_observer();
    unload_and_wait(
        &service,
        configuration,
        model_id,
        &deployment_id,
        "decision unload",
    )
    .await;
    Ok(())
}

/// Runs the Strata qualification of one case on a fresh site once the GPU's pool has the case's
/// minimum free bytes, skipping where the profile approves no engine artifact for the platform.
async fn qualify_strata_site(case: &'static StrataCase) -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site")
        .await
        .context("prepare Strata qualification site")?;
    if let Some(reason) = site.unsupported_platform_reason(case.model_id, case.profile_id)? {
        eprintln!("[nvidia-docker] skipped: {reason}");
        return site.cleanup().await;
    }
    let primary = async {
        nvidia_qualification::ensure_free_gpu_memory("nvidia-docker", case.minimum_free_bytes)?;
        let configuration = site.configuration.clone();
        let dotcore_sources = site.dotcore_sources.clone();
        let profile = strata_profile_defaults(&site.catalog, case)?;
        tokio::spawn(async move {
            qualify_strata(&configuration, &dotcore_sources, case, profile).await
        })
        .await
        .context("Strata harness failed")?
    }
    .await;
    let cleanup = site.cleanup().await;
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup.context("Strata harness cleanup failed")),
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("cleanup also failed: {cleanup}")))
        }
    }
}

/// Selects the Strata profile's binding and parameter defaults without overrides.
fn strata_profile_defaults(
    catalog: &Catalog,
    case: &StrataCase,
) -> anyhow::Result<dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration> {
    let profile = catalog
        .protobuf_records_without_sizing()
        .models
        .into_iter()
        .find(|model| model.model_id == case.model_id)
        .and_then(|model| {
            model
                .profiles
                .into_iter()
                .find(|profile| profile.profile_id == case.profile_id)
        })
        .ok_or_else(|| anyhow!("Master catalog has no profile {:?}", case.profile_id))?;
    Ok(
        dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
            model_artifact_binding_id: profile.default_model_artifact_binding_id,
            engine_parameter_values: profile.engine_parameter_defaults,
            custom_argument_tokens: Vec::new(),
            option_overrides: Vec::new(),
        },
    )
}

async fn qualify_strata(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    case: &StrataCase,
    profile: dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration,
) -> anyhow::Result<()> {
    let model_id = case.model_id;
    let service =
        wait_for_released_backend(configuration, dotcore_sources, model_id, "backend release")
            .await;
    service.start_observer();
    let local = dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
        model_id: model_id.to_owned(),
        profile_id: case.profile_id.to_owned(),
        placement: Some(support::exact_placement(
            "development-node",
            "nvidia-target",
        )),
        configuration: Some(profile),
    };
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(qualification_load_request(local)),
    )
    .await
    .context("load Strata model")?
    .into_inner()
    .deployment
    .context("loaded Strata deployment")?;
    let deployment_id = deployment.model_deployment_id.clone();
    let Some(model_deployment_snapshot::Source::Local(snapshot)) = deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    else {
        anyhow::bail!("Strata deployment has no local snapshot");
    };
    let engine = snapshot
        .engine_artifact
        .as_ref()
        .context("Strata deployment has no engine artifact")?;
    anyhow::ensure!(
        nvidia_qualification::development_source_docker_image(engine),
        "Strata engine is not a development-source Docker-delivered Nix artifact: {engine:?}"
    );
    let pin = EnginePin::evaluate(dotcore_sources, engine).await?;

    // Preparation builds or loads the engine image before the workload starts, on its own deadline.
    let prepared = tokio::time::timeout(PREPARATION_TIMEOUT, async {
        loop {
            let current = read_deployment(&service, &deployment_id).await;
            if nvidia_qualification::engine_prepared(configuration, &current).await? {
                return Ok::<_, anyhow::Error>(current);
            }
            let state = current.state.as_ref().context("deployment state")?;
            anyhow::ensure!(
                state.status != ModelDeploymentStatus::Failed as i32,
                "Strata engine preparation failed: {:?}",
                state.failure
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("Strata engine preparation exceeded its deadline")??;
    pin.check(&prepared)?;
    wait_for_lifecycle_within(
        case.ready_timeout,
        &service,
        model_id,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        "strata lifecycle wait",
    )
    .await;
    let endpoint = runtime_endpoint(
        configuration,
        model_id,
        &deployment_id,
        "strata endpoint discovery",
    )
    .await?;
    anyhow::ensure!(
        endpoint.image == pin.launch_reference,
        "Strata container runs {} instead of the loader's image {}",
        endpoint.image,
        pin.launch_reference
    );
    let data_source = endpoint
        .mounts
        .iter()
        .find(|(_, destination, read_only)| destination == STRATA_DATA_PATH && !read_only)
        .map(|(source, _, _)| PathBuf::from(source))
        .context("Strata container has no writable data mount")?;
    anyhow::ensure!(
        data_source.is_dir(),
        "Strata data storage {} is missing while the deployment runs",
        data_source.display()
    );
    let built_pack = case.built_pack_path.map(|path| data_source.join(path));
    if let Some(pack) = &built_pack {
        anyhow::ensure!(
            pack.is_dir(),
            "Strata built no pack at {} while the deployment runs",
            pack.display()
        );
    }
    let engine_port = endpoint
        .http_port
        .context("READY Strata deployment must publish its loopback host port")?;
    assert_exact_alias(engine_port, model_id).await;

    // The router needs the backend's state lock back.
    drop(service);
    drop(
        wait_for_released_backend(
            configuration,
            dotcore_sources,
            model_id,
            "backend release before router",
        )
        .await,
    );
    let router = start_router(configuration, dotcore_sources, model_id, engine_port).await;
    let body = serde_json::to_vec(&serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "Compute 17 + 25. Reply with only the number."}],
        "temperature": 0,
        "max_completion_tokens": 4096,
    }))?;
    let response = router_request(router.inference_address, &http_post(&body)).await;
    router.stop().await;
    anyhow::ensure!(
        response.starts_with(b"HTTP/1.1 200"),
        "Strata router response failed: {}",
        router_failure_diagnostics(&response)
    );
    let value = http_response_json(&response);
    anyhow::ensure!(
        value.get("model").and_then(Value::as_str) == Some(model_id),
        "Strata response does not carry the model name {model_id:?}: {}",
        completion_diagnostics(&value)
    );
    anyhow::ensure!(
        value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::trim)
            == Some("42"),
        "Strata answered something other than 42: {}",
        completion_diagnostics(&value)
    );

    let service = wait_for_released_backend(
        configuration,
        dotcore_sources,
        model_id,
        "backend release before unload",
    )
    .await;
    // Removal completes through the observer's reconciliation.
    service.start_observer();
    unload_and_wait(
        &service,
        configuration,
        model_id,
        &deployment_id,
        "strata unload",
    )
    .await;
    anyhow::ensure!(
        !data_source.exists(),
        "removed Strata deployment left its data storage {}",
        data_source.display()
    );
    if let Some(pack) = built_pack {
        anyhow::ensure!(
            !pack.exists(),
            "removed Strata deployment left its pack {}",
            pack.display()
        );
    }
    Ok(())
}

/// One Strata model profile qualified on one machine. `built_pack_path` is the pack the engine
/// builds under the data storage on first start, relative to it, for profiles that have one.
struct StrataCase {
    model_id: &'static str,
    profile_id: &'static str,
    minimum_free_bytes: u64,
    ready_timeout: Duration,
    built_pack_path: Option<&'static str>,
}

async fn assert_router_chat(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    case: &QualificationCase,
    engine_port: u16,
) {
    let model_id = &case.model_id;
    let router_started = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=router Chat phase=before engine_port={engine_port}"
    );
    let router = start_router(configuration, dotcore_sources, model_id, engine_port).await;
    let inference_address = router.inference_address;
    let text = serde_json::to_vec(&serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "Reply with exactly READY."}],
        "max_completion_tokens": 256,
        "temperature": 0,
    }))
    .expect("router text payload");
    let text_response = router_request(inference_address, &http_post(&text)).await;
    assert!(
        text_response.starts_with(b"HTTP/1.1 200"),
        "router text response"
    );
    let text_value = http_response_json(&text_response);
    let text = text_value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .expect("router text response has OpenAI-shaped generated text");
    assert!(
        !text.trim().is_empty(),
        "router text response has empty generated text: {}",
        completion_diagnostics(&text_value),
    );
    let tool = serde_json::to_vec(&serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "Call the ready tool."}],
        "tools": [{"type": "function", "function": {"name": "ready", "parameters": {"type": "object"}}}],
        "tool_choice": {"type": "function", "function": {"name": "ready"}},
        "stream": true,
        "stream_options": {"include_usage": true},
    }))
    .expect("router tool payload");
    let tool_response = router_request(inference_address, &http_post(&tool)).await;
    if !tool_response.starts_with(b"HTTP/1.1 200") {
        eprintln!(
            "[nvidia-docker] Model={model_id} operation=router tool phase=after state=failed {}",
            router_failure_diagnostics(&tool_response),
        );
    }
    assert!(
        tool_response.starts_with(b"HTTP/1.1 200"),
        "router tool response"
    );
    assert!(
        tool_response
            .windows(b"data: [DONE]".len())
            .any(|value| value == b"data: [DONE]")
    );
    let frames = sse_json_frames(&tool_response);
    let calls = frames
        .iter()
        .filter_map(|frame| frame.pointer("/choices/0/delta/tool_calls/0"));
    let mut arguments = String::new();
    let mut saw_call_id = false;
    let mut saw_name = false;
    for call in calls {
        saw_call_id |= call.get("id").and_then(Value::as_str).is_some();
        saw_name |= call.pointer("/function/name").and_then(Value::as_str) == Some("ready");
        if let Some(fragment) = call.pointer("/function/arguments").and_then(Value::as_str) {
            arguments.push_str(fragment);
        }
    }
    assert!(saw_call_id, "router stream must include a tool-call ID");
    assert!(
        saw_name,
        "router stream must include the requested function name"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&arguments).expect("reconstructed tool arguments"),
        serde_json::json!({})
    );
    if case.projector.is_some() {
        assert_router_image_inputs(inference_address, model_id).await;
    }
    router.stop().await;
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=router Chat phase=after state=complete elapsed_ms={}",
        router_started.elapsed().as_millis()
    );
}

/// A router serving the site's inference listener, started by `start_router`.
struct RunningRouter {
    inference_address: std::net::SocketAddr,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<dotcore::composition::CompositionResult<()>>,
}

impl RunningRouter {
    async fn stop(self) {
        self.shutdown.send(()).expect("Docker router shutdown");
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .expect("Docker router join deadline")
            .expect("Docker router task")
            .expect("Docker router result");
    }
}

/// Starts the site's router and waits until `/v1/models` lists `model_id`.
async fn start_router(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    model_id: &str,
    engine_port: u16,
) -> RunningRouter {
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = crate::support::prepare_backend_at(
        configuration.clone(),
        "127.0.0.1".parse().expect("loopback bind address"),
        dotcore_sources.clone(),
        async move {
            let _ = shutdown_signal.await;
        },
    )
    .await
    .expect("prepared Docker router backend");
    let inference_address = backend.addresses().inference.expect("inference listener");
    assert_ne!(
        inference_address.port(),
        engine_port,
        "router must not expose the engine port"
    );
    let mut server = tokio::spawn(backend.serve());
    let availability_started = Instant::now();
    let mut availability_heartbeat = Instant::now();
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            if server.is_finished() {
                let error = (&mut server)
                    .await
                    .expect("Docker router task")
                    .expect_err("Docker router must remain active");
                panic!("Docker router failed before model availability: {error}");
            }
            let response = router_request(
                inference_address,
                b"GET /v1/models HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
            )
            .await;
            if response
                .windows(model_id.len())
                .any(|value| value == model_id.as_bytes())
            {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation=router model availability phase=after state=available elapsed_ms={}",
                    availability_started.elapsed().as_millis()
                );
                return;
            }
            if availability_heartbeat.elapsed() >= PROGRESS_HEARTBEAT_INTERVAL {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation=router model availability heartbeat elapsed_ms={} last_observed_state=unavailable next_poll_delay_ms=250",
                    availability_started.elapsed().as_millis()
                );
                availability_heartbeat = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("Docker router model availability");
    RunningRouter {
        inference_address,
        shutdown,
        server,
    }
}

fn sse_json_frames(response: &[u8]) -> Vec<Value> {
    let body = response
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .map(|end| &response[end + 4..])
        .expect("router HTTP response headers");
    std::str::from_utf8(body)
        .expect("router SSE UTF-8")
        .replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|frame| {
            frame
                .strip_prefix("data: ")
                .filter(|data| *data != "[DONE]")
        })
        .map(|data| serde_json::from_str(data).expect("router SSE JSON frame"))
        .collect()
}

fn http_post(body: &[u8]) -> Vec<u8> {
    http_post_to("/v1/chat/completions", body)
}

fn http_post_to(path: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

async fn assert_router_image_inputs(address: std::net::SocketAddr, model_id: &str) {
    for (media_type, image, color) in [("JPEG", RED_JPEG, "red"), ("PNG", BLUE_PNG, "blue")] {
        let chat = serde_json::to_vec(&serde_json::json!({"model":model_id,"messages":[{"role":"user","content":[{"type":"text","text":IMAGE_PROMPT},{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}],"max_completion_tokens":IMAGE_MAX_COMPLETION_TOKENS,"temperature":0})).expect("router Chat image payload");
        let chat_response = router_request(address, &http_post(&chat)).await;
        assert!(
            chat_response.starts_with(b"HTTP/1.1 200"),
            "router Chat image response: {}",
            router_failure_diagnostics(&chat_response)
        );
        let chat_value = http_response_json(&chat_response);
        assert_image_color(
            &chat_value,
            "/choices/0/message/content",
            model_id,
            media_type,
            color,
            "Chat",
            &chat_response,
        );
        let responses = serde_json::to_vec(&serde_json::json!({"model":model_id,"store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":IMAGE_PROMPT},{"type":"input_image","image_url":image,"detail":"auto"}]}],"max_output_tokens":IMAGE_MAX_COMPLETION_TOKENS,"temperature":0})).expect("router Responses image payload");
        let responses_response =
            router_request(address, &http_post_to("/v1/responses", &responses)).await;
        assert!(
            responses_response.starts_with(b"HTTP/1.1 200"),
            "router Responses image response: {}",
            router_failure_diagnostics(&responses_response)
        );
        let responses_value = http_response_json(&responses_response);
        assert_image_color(
            &responses_value,
            "/output/0/content/0/text",
            model_id,
            media_type,
            color,
            "Responses",
            &responses_response,
        );
    }
}

#[test]
fn image_fixtures_are_canonical_and_valid() {
    assert_image_fixture(
        RED_JPEG,
        "data:image/jpeg;base64",
        image::ImageFormat::Jpeg,
        "red",
    );
    assert_image_fixture(
        BLUE_PNG,
        "data:image/png;base64",
        image::ImageFormat::Png,
        "blue",
    );
}

fn assert_image_fixture(
    data_url: &str,
    expected_prefix: &str,
    format: image::ImageFormat,
    color: &str,
) {
    let (prefix, encoded) = data_url.split_once(',').expect("image data URL");
    assert_eq!(prefix, expected_prefix);
    let bytes = STANDARD.decode(encoded).expect("canonical image Base64");
    assert_eq!(STANDARD.encode(&bytes), encoded);

    let image = image::ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .expect("valid image fixture");
    assert_eq!((image.width(), image.height()), (64, 64));
    let image = image.to_rgb8();
    match color {
        "red" => assert!(
            image
                .pixels()
                .all(|pixel| { pixel[0] > 200 && pixel[0] > pixel[1] && pixel[0] > pixel[2] })
        ),
        "blue" => assert!(
            image
                .pixels()
                .all(|pixel| { pixel[2] > 200 && pixel[2] > pixel[0] && pixel[2] > pixel[1] })
        ),
        _ => panic!("unsupported image fixture color {color}"),
    }
}

fn http_response_json(response: &[u8]) -> Value {
    let header_end = response
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .expect("router HTTP response headers");
    serde_json::from_slice(&response[header_end + 4..]).expect("router JSON response")
}

fn assert_image_color(
    response: &Value,
    pointer: &str,
    model_id: &str,
    media_type: &str,
    expected_color: &str,
    endpoint: &str,
    raw_response: &[u8],
) {
    let text = response
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "{endpoint} {media_type} image response has no output text for {model_id:?}: {}",
                image_response_diagnostics(response, pointer, raw_response),
            )
        });
    let diagnostics = image_response_diagnostics(response, pointer, raw_response);
    if endpoint == "Chat" {
        assert_eq!(
            response
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str),
            Some("stop"),
            "{endpoint} {media_type} image response did not stop for {model_id:?}: {diagnostics}",
        );
    }
    assert!(
        !text.trim().is_empty(),
        "{endpoint} {media_type} image response has empty output for {model_id:?}: {diagnostics}",
    );
    assert!(
        text.to_ascii_lowercase().contains(expected_color),
        "{endpoint} {media_type} image response does not identify {expected_color} for {model_id:?}: {diagnostics}",
    );
}

fn image_response_diagnostics(response: &Value, pointer: &str, raw_response: &[u8]) -> String {
    let completion_use = response
        .pointer("/usage/completion_tokens")
        .or_else(|| response.pointer("/usage/output_tokens"));
    let visible_content = response.pointer(pointer);
    let public_response = raw_response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|header_end| String::from_utf8_lossy(&raw_response[header_end + 4..]))
        .map(|body| bounded_text(&body, ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS))
        .unwrap_or_else(|| "<missing>".to_owned());
    format!(
        "finish_reason={} visible_content={} completion_use={} public_response={public_response:?}",
        bounded_completion_value(response.pointer("/choices/0/finish_reason")),
        bounded_completion_value(visible_content),
        bounded_completion_usage(completion_use),
    )
}

fn bounded_completion_usage(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(value)) => {
            bounded_text(&value.to_string(), COMPLETION_DIAGNOSTIC_MAX_CHARS)
        }
        _ => bounded_completion_value(value),
    }
}

async fn router_request(address: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("Docker router connection");
    stream
        .write_all(request)
        .await
        .expect("Docker router request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("Docker router response");
    response
}

async fn wait_for_released_backend(
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    model_id: &str,
    operation: &str,
) -> IsolatedModelService {
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation={operation} phase=before timeout_secs={}",
        RESTART_TIMEOUT.as_secs()
    );
    tokio::time::timeout(RESTART_TIMEOUT, async {
        loop {
            match model_service_with_sources_for(configuration, dotcore_sources.clone()).await {
                Ok(service) => {
                    eprintln!(
                        "[nvidia-docker] Model={model_id} operation={operation} phase=after state=released elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    return service;
                }
                Err(error) if error.to_string().contains("already locked") => {
                    if heartbeat.elapsed() >= PROGRESS_HEARTBEAT_INTERVAL {
                        eprintln!(
                            "[nvidia-docker] Model={model_id} operation={operation} heartbeat elapsed_ms={} last_observed_state=locked next_poll_delay_ms=100",
                            started.elapsed().as_millis()
                        );
                        heartbeat = Instant::now();
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("NVIDIA Docker backend restart failed: {error}"),
            }
        }
    })
    .await
    .expect("previous backend observer and state lock release")
}

fn qualification_local_request(
    case: &QualificationCase,
) -> dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
    let mut configuration = support::local_model_configuration(
        &case.artifact.artifact_id,
        qualification_total_context(&case.profile_id),
    );
    configuration.custom_argument_tokens =
        VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned).to_vec();

    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
        model_id: case.model_id.clone(),
        profile_id: case.profile_id.clone(),
        placement: Some(support::exact_placement(
            "development-node",
            "nvidia-target",
        )),
        configuration: Some(configuration),
    }
}

fn qualification_total_context(profile_id: &str) -> u64 {
    if profile_id == "fast-start" {
        NON_DEFAULT_TOTAL_CONTEXT
    } else {
        DEFAULT_TOTAL_CONTEXT
    }
}

fn qualification_load_request(
    local: dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest,
) -> CreateModelDeploymentRequest {
    let route_name = local.model_id.clone();
    CreateModelDeploymentRequest {
        source: Some(
            dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                local,
            ),
        ),
        exposure: Some(ModelDeploymentExposure {
            kind: Some(model_deployment_exposure::Kind::Direct(
                DirectRouteExposure { route_name },
            )),
        }),
        model_deployment_name: None,
    }
}

#[test]
fn qualification_configured_snapshot_appends_exact_custom_arguments() {
    let catalog = nvidia_qualification::load_catalog(Path::new("/nonexistent-nvidia-site"))
        .expect("NVIDIA catalog");

    for platform in [
        ExecutionPlatform::linux_amd64(),
        ExecutionPlatform::linux_arm64(),
    ] {
        for profile_id in PROFILE_IDS {
            let case = qualification_case(
                &catalog,
                MODEL_ID,
                profile_id,
                &platform,
                &synthetic_nvidia_observation(),
            )
            .expect("qualification case");
            let mut expected = case.snapshot.clone();
            let local = match expected.source.as_mut() {
                Some(model_deployment_snapshot::Source::Local(local)) => local,
                _ => panic!("expected local catalog snapshot"),
            };
            local.custom_argument_tokens =
                VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned).to_vec();
            local
                .desired_launch_plan
                .as_mut()
                .expect("Catalog snapshot desired launch plan")
                .arguments
                .extend(VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(|argument| {
                    dotcore_proto::dotcore::model::v1::PortableModelLaunchArgumentToken {
                        value: Some(portable_model_launch_argument_token::Value::Literal(
                            argument.to_owned(),
                        )),
                    }
                }));

            assert_eq!(case.configured_snapshot, expected);
        }
    }
}

#[test]
fn qualification_load_request_uses_each_profile_selected_artifact_set_and_direct_route() {
    let catalog = nvidia_qualification::load_catalog(Path::new("/nonexistent-nvidia-site"))
        .expect("NVIDIA catalog");

    for platform in [
        ExecutionPlatform::linux_amd64(),
        ExecutionPlatform::linux_arm64(),
    ] {
        for profile_id in PROFILE_IDS {
            let case = qualification_case(
                &catalog,
                MODEL_ID,
                profile_id,
                &platform,
                &synthetic_nvidia_observation(),
            )
            .expect("qualification case");
            let request = qualification_load_request(qualification_local_request(&case));
            let custom_argument_tokens = match request.source.as_ref() {
            Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    local,
                ),
            ) => {
                &local
                    .configuration
                    .as_ref()
                    .expect("qualification configuration")
                    .custom_argument_tokens
            }
            _ => panic!("qualification request has no local configuration"),
        };
            assert_eq!(
                custom_argument_tokens,
                &VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned),
            );

            assert_eq!(
            request.source,
            Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: MODEL_ID.to_owned(),
                        profile_id: profile_id.to_owned(),
                        placement: Some(support::exact_placement("development-node", "nvidia-target")),
                        configuration: Some({
                            let mut configuration = support::local_model_configuration(
                                &case.artifact.artifact_id,
                                if profile_id == "fast-start" {
                                    NON_DEFAULT_TOTAL_CONTEXT
                                } else {
                                    DEFAULT_TOTAL_CONTEXT
                                },
                            );
                            configuration.custom_argument_tokens =
                                VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned).to_vec();
                            configuration
                        }),
                    },
                ),
            )
        );
            assert_eq!(
                request.exposure,
                Some(ModelDeploymentExposure {
                    kind: Some(model_deployment_exposure::Kind::Direct(
                        DirectRouteExposure {
                            route_name: MODEL_ID.to_owned(),
                        },
                    )),
                })
            );
        }
    }
}

fn assert_persisted_qualification_route(
    configuration: &SitePaths,
    model_id: &str,
    deployment_id: &str,
) {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))
        .expect("persisted NVIDIA site after qualification load");
    assert_eq!(site.inference_routes.len(), 1);
    let route = &site.inference_routes[0];
    assert_eq!(route.name, model_id);
    assert!(route.enabled);
    assert_eq!(route.fallback_groups.len(), 1);
    assert_eq!(
        route.fallback_groups[0].selection,
        FallbackGroupSelection::Single as i32
    );
    assert_eq!(
        route.fallback_groups[0].model_deployment_ids,
        vec![deployment_id.to_owned()]
    );
}

fn assert_persisted_qualification_route_absent(configuration: &SitePaths, model_id: &str) {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))
        .expect("persisted NVIDIA site after qualification teardown");
    assert!(
        site.inference_routes
            .iter()
            .all(|route| route.name != model_id),
        "teardown retained the owned exposure route for {model_id:?}",
    );
}

async fn load_model(
    service: &SelectedModelService,
    configuration: &SitePaths,
    case: &QualificationCase,
) -> dotcore_proto::dotcore::model::v1::ModelDeployment {
    let started = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={} operation=load phase=before profile={}",
        case.model_id, case.profile_id
    );
    let local = qualification_local_request(case);
    let preview = ModelServiceContract::preview_local_model_deployment(
        service,
        Request::new(PreviewLocalModelDeploymentRequest {
            local: Some(local.clone()),
        }),
    )
    .await
    .expect("preview NVIDIA Docker model")
    .into_inner()
    .resolved_snapshot
    .expect("previewed NVIDIA Docker model snapshot");
    assert_valid_custom_arguments_snapshot(&preview);
    let deployment = ModelServiceContract::create_model_deployment(
        service,
        Request::new(qualification_load_request(local)),
    )
    .await
    .expect("load NVIDIA Docker model")
    .into_inner()
    .deployment
    .expect("loaded NVIDIA Docker model deployment");
    assert_deployment_matches_preview(&deployment, &preview);
    eprintln!(
        "[nvidia-docker] Model={} operation=load phase=after state=accepted profile={} deployment_id={} elapsed_ms={}",
        case.model_id,
        case.profile_id,
        deployment.model_deployment_id,
        started.elapsed().as_millis()
    );
    wait_for_preparation(
        service,
        configuration,
        case,
        &deployment.model_deployment_id,
    )
    .await;
    deployment
}

async fn load_invalid_custom_argument_model(
    service: &SelectedModelService,
    case: &QualificationCase,
) -> ModelDeployment {
    let mut local = qualification_local_request(case);
    local
        .configuration
        .as_mut()
        .expect("qualification configuration")
        .custom_argument_tokens = vec![INVALID_LLAMA_CPP_ARGUMENT.to_owned()];
    let preview = ModelServiceContract::preview_local_model_deployment(
        service,
        Request::new(PreviewLocalModelDeploymentRequest {
            local: Some(local.clone()),
        }),
    )
    .await
    .expect("preview NVIDIA Docker model with invalid custom argument")
    .into_inner()
    .resolved_snapshot
    .expect("previewed NVIDIA Docker model with invalid custom argument");
    let deployment = ModelServiceContract::create_model_deployment(
        service,
        Request::new(qualification_load_request(local)),
    )
    .await
    .expect("load NVIDIA Docker model with invalid custom argument")
    .into_inner()
    .deployment
    .expect("loaded NVIDIA Docker model with invalid custom argument");
    assert_deployment_matches_preview(&deployment, &preview);
    deployment
}

async fn pause_model(
    service: &SelectedModelService,
    model_id: &str,
    deployment_id: &str,
) -> ModelDeployment {
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=pause phase=before deployment_id={deployment_id}"
    );
    let deployment = ModelServiceContract::pause_model(
        service,
        Request::new(PauseModelRequest {
            model_deployment_id: deployment_id.to_owned(),
        }),
    )
    .await
    .expect("pause NVIDIA Docker model")
    .into_inner()
    .deployment
    .expect("paused NVIDIA Docker model deployment");
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=pause phase=after state=accepted deployment_id={deployment_id}"
    );
    deployment
}

async fn resume_model(
    service: &SelectedModelService,
    model_id: &str,
    deployment_id: &str,
) -> ModelDeployment {
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=resume phase=before deployment_id={deployment_id}"
    );
    let deployment = ModelServiceContract::resume_model(
        service,
        Request::new(ResumeModelRequest {
            model_deployment_id: deployment_id.to_owned(),
        }),
    )
    .await
    .expect("resume NVIDIA Docker model")
    .into_inner()
    .deployment
    .expect("resumed NVIDIA Docker model deployment");
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=resume phase=after state=accepted deployment_id={deployment_id}"
    );
    deployment
}

async fn wait_for_lifecycle(
    service: &SelectedModelService,
    model_id: &str,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    operation: &str,
) -> dotcore_proto::dotcore::model::v1::ModelDeployment {
    wait_for_lifecycle_within(
        DEPLOYMENT_TIMEOUT,
        service,
        model_id,
        deployment_id,
        expected,
        operation,
    )
    .await
}

async fn wait_for_lifecycle_within(
    timeout: Duration,
    service: &SelectedModelService,
    model_id: &str,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    operation: &str,
) -> dotcore_proto::dotcore::model::v1::ModelDeployment {
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation={operation} phase=before expected_state={expected:?}"
    );
    tokio::time::timeout(timeout, async {
        loop {
            let deployment = read_deployment(service, deployment_id).await;
            let state = deployment.state.as_ref().expect("deployment state");
            let observed = ModelDeploymentStatus::try_from(state.status)
                .map(|state| format!("{state:?}"))
                .unwrap_or_else(|_| format!("unknown({})", state.status));
            if state.status == expected as i32 {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation={operation} phase=after state={observed} elapsed_ms={}",
                    started.elapsed().as_millis()
                );
                return deployment;
            }
            assert_ne!(
                state.status,
                ModelDeploymentStatus::Failed as i32,
                "NVIDIA Docker model deployment failed: {:?}",
                state.failure,
            );
            if heartbeat.elapsed() >= PROGRESS_HEARTBEAT_INTERVAL {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation={operation} heartbeat elapsed_ms={} last_observed_state={observed} next_poll_delay_ms=250",
                    started.elapsed().as_millis()
                );
                heartbeat = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("NVIDIA Docker model lifecycle timeout")
}

/// Waits until the hosting node prepared the deployment's engine, then checks the engine pin it
/// recorded. A failure during preparation fails the wait.
async fn wait_for_preparation(
    service: &SelectedModelService,
    configuration: &SitePaths,
    case: &QualificationCase,
    deployment_id: &str,
) -> ModelDeployment {
    let model_id = &case.model_id;
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=engine preparation phase=before deployment_id={deployment_id}"
    );
    let prepared = tokio::time::timeout(PREPARATION_TIMEOUT, async {
        loop {
            let deployment = read_deployment(service, deployment_id).await;
            if nvidia_qualification::engine_prepared(configuration, &deployment)
                .await
                .expect("observe NVIDIA Docker engine preparation")
            {
                return deployment;
            }
            let state = deployment.state.as_ref().expect("deployment state");
            assert_ne!(
                state.status,
                ModelDeploymentStatus::Failed as i32,
                "NVIDIA Docker engine preparation failed: {:?}",
                state.failure,
            );
            if heartbeat.elapsed() >= PROGRESS_HEARTBEAT_INTERVAL {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation=engine preparation heartbeat elapsed_ms={} last_observed_state={:?} message={:?} next_poll_delay_ms=250",
                    started.elapsed().as_millis(),
                    ModelDeploymentStatus::try_from(state.status),
                    state.status_message,
                );
                heartbeat = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("NVIDIA Docker engine preparation timeout");
    let pin = case.engine_pin.as_ref().expect("evaluated engine pin");
    pin.check(&prepared).expect("NVIDIA Docker engine pin");
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=engine preparation phase=after deployment_id={deployment_id} store_path={} image={} preparation_ms={}",
        pin.store_path,
        pin.launch_reference,
        started.elapsed().as_millis()
    );
    prepared
}

async fn read_deployment(service: &SelectedModelService, deployment_id: &str) -> ModelDeployment {
    ModelServiceContract::list_model_deployments(
        service,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("deployment read")
    .into_inner()
    .deployments
    .into_iter()
    .find(|deployment| deployment.model_deployment_id == deployment_id)
    .expect("NVIDIA Docker model deployment")
}

async fn wait_for_failed_lifecycle(
    service: &SelectedModelService,
    model_id: &str,
    deployment_id: &str,
    operation: &str,
) -> ModelDeployment {
    let deployment = wait_for_lifecycle(
        service,
        model_id,
        deployment_id,
        ModelDeploymentStatus::Failed,
        operation,
    )
    .await;
    let failure = deployment
        .state
        .as_ref()
        .and_then(|state| state.failure.as_ref())
        .expect("FAILED deployment has an actionable failure");
    assert_eq!(failure.code, "engine_startup_failed");
    assert_eq!(
        failure.message,
        "Engine stopped during startup; correct the engine arguments and deploy the model again"
    );
    deployment
}

fn assert_invalid_custom_argument_failure(deployment: &ModelDeployment) {
    let custom_argument_tokens = match deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    {
        Some(model_deployment_snapshot::Source::Local(local)) => &local.custom_argument_tokens,
        _ => panic!("invalid deployment has no local snapshot"),
    };
    assert_eq!(
        custom_argument_tokens.as_slice(),
        [INVALID_LLAMA_CPP_ARGUMENT.to_owned()]
    );
}

async fn unload_and_wait(
    service: &SelectedModelService,
    configuration: &SitePaths,
    model_id: &str,
    deployment_id: &str,
    operation: &str,
) {
    eprintln!(
        "[nvidia-docker] Model={model_id} operation={operation} phase=before deployment_id={deployment_id}"
    );
    ModelServiceContract::remove_model_deployment(
        service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.to_owned(),
        }),
    )
    .await
    .expect("unload NVIDIA Docker model");
    eprintln!("[nvidia-docker] Model={model_id} operation={operation} phase=after state=accepted");
    wait_for_absence(service, model_id, operation).await;
    assert_model_container_absent(configuration, deployment_id).await;
    assert_persisted_qualification_route_absent(configuration, model_id);
}

async fn wait_for_absence(service: &SelectedModelService, model_id: &str, operation: &str) {
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    eprintln!("[nvidia-docker] Model={model_id} operation={operation}/absence phase=before");
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployments = ModelServiceContract::list_model_deployments(
                service,
                Request::new(ListModelDeploymentsRequest {}),
            )
            .await
            .expect("deployment read")
                .into_inner()
                .deployments;
            if deployments.is_empty() {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation={operation}/absence phase=after state=absent elapsed_ms={}",
                    started.elapsed().as_millis()
                );
                return;
            }
            if heartbeat.elapsed() >= PROGRESS_HEARTBEAT_INTERVAL {
                eprintln!(
                    "[nvidia-docker] Model={model_id} operation={operation}/absence heartbeat elapsed_ms={} last_observed_state=present({}) next_poll_delay_ms=250",
                    started.elapsed().as_millis(),
                    deployments.len()
                );
                heartbeat = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("NVIDIA Docker model unload timeout");
}

#[derive(Debug)]
struct RuntimeEndpoint {
    http_port: Option<u16>,
    container_id: String,
    image: String,
    running: bool,
    pid: i64,
    status: String,
    mounts: Vec<(String, String, bool)>,
    command: Vec<String>,
}

async fn runtime_endpoint(
    configuration: &SitePaths,
    model_id: &str,
    deployment_id: &str,
    operation: &str,
) -> anyhow::Result<RuntimeEndpoint> {
    let started = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation={operation} phase=before deployment_id={deployment_id}"
    );
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))
        .expect("local-NVIDIA site configuration");
    let docker = Docker::connect_with_local_defaults().expect("Docker connection");
    let filters = HashMap::from([(
        "label",
        vec![
            format!("{LABEL_CORRELATION}={deployment_id}"),
            format!("{LABEL_SITE}={}", site.site_id),
            format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
        ],
    )]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let containers = docker
        .list_containers(Some(options))
        .await
        .expect("Docker workload list");
    let container = containers
        .into_iter()
        .find(|container| {
            container.labels.as_ref().is_some_and(|labels| {
                labels
                    .get(LABEL_CORRELATION)
                    .is_some_and(|value| value == deployment_id)
                    && labels
                        .get(LABEL_SITE)
                        .is_some_and(|value| value == &site.site_id)
                    && labels
                        .get(LABEL_SCHEMA)
                        .is_some_and(|value| value == OWNERSHIP_SCHEMA)
            })
        })
        .expect("NVIDIA Docker container");
    let container_id = container.id.expect("container ID");
    let inspection = docker
        .inspect_container(&container_id, None)
        .await
        .expect("NVIDIA Docker container inspection");
    let endpoint =
        runtime_endpoint_from_inspection(container_id, inspection).with_context(|| {
            format!("normalize Docker endpoint inspection for model {model_id} during {operation}")
        })?;
    eprintln!(
        "[nvidia-docker] Model={model_id} operation={operation} phase=after state=found http_port={:?} elapsed_ms={}",
        endpoint.http_port,
        started.elapsed().as_millis()
    );
    Ok(endpoint)
}

fn runtime_endpoint_from_inspection(
    container_id: String,
    inspection: ContainerInspectResponse,
) -> anyhow::Result<RuntimeEndpoint> {
    let http_port = inspection
        .network_settings
        .as_ref()
        .and_then(|settings| settings.ports.as_ref())
        .and_then(|ports| ports.get("8080/tcp"))
        .and_then(|bindings| bindings.as_ref())
        .map(|bindings| {
            let mut http_port = None;
            for binding in bindings {
                let Some(host_port) = binding.host_port.as_deref() else {
                    continue;
                };
                let port = host_port.parse::<u16>().with_context(|| {
                    format!(
                        "Docker 8080/tcp HostPort {host_port:?} is malformed or out of range; expected an unsigned 16-bit port"
                    )
                })?;
                http_port.get_or_insert(port);
            }
            Ok::<_, anyhow::Error>(http_port)
        })
        .transpose()?
        .flatten();
    let mounts = inspection
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mount| Some((mount.source?, mount.destination?, mount.rw == Some(false))))
        .collect();
    let (image, command) = inspection
        .config
        .map(|config| {
            (
                config.image.unwrap_or_default(),
                config.cmd.unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    Ok(RuntimeEndpoint {
        http_port,
        container_id,
        image,
        running: inspection
            .state
            .as_ref()
            .and_then(|state| state.running)
            .expect("NVIDIA Docker container running state"),
        pid: inspection
            .state
            .as_ref()
            .and_then(|state| state.pid)
            .expect("NVIDIA Docker container process ID"),
        status: inspection
            .state
            .as_ref()
            .and_then(|state| state.status.as_ref())
            .map(|status| format!("{status:?}"))
            .expect("NVIDIA Docker container status"),
        mounts,
        command,
    })
}

fn assert_stopped_endpoint(endpoint: &RuntimeEndpoint) {
    assert!(
        !endpoint.running,
        "paused Docker container is still running"
    );
    assert_eq!(
        endpoint.pid, 0,
        "paused Docker container still has a process"
    );
    assert_eq!(endpoint.status, "EXITED", "paused Docker container status");
}

async fn model_container_ids(configuration: &SitePaths, deployment_id: &str) -> Vec<String> {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))
        .expect("local-NVIDIA site configuration");
    let docker = Docker::connect_with_local_defaults().expect("Docker connection");
    let filters = HashMap::from([(
        "label",
        vec![
            format!("{LABEL_CORRELATION}={deployment_id}"),
            format!("{LABEL_SITE}={}", site.site_id),
            format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
        ],
    )]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    docker
        .list_containers(Some(options))
        .await
        .expect("Docker workload list")
        .into_iter()
        .filter_map(|container| container.id)
        .collect()
}

async fn assert_model_container_absent(configuration: &SitePaths, deployment_id: &str) {
    let ids = model_container_ids(configuration, deployment_id).await;
    assert!(
        ids.is_empty(),
        "paused missing-container model was recreated before Resume: {ids:?}",
    );
}

fn cache_snapshot(root: &Path) -> anyhow::Result<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    collect_cache_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_cache_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, u64)>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory).with_context(|| {
        format!(
            "read model artifact cache directory {}",
            directory.display()
        )
    })? {
        let entry = entry.context("read model artifact cache entry")?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .context("inspect model artifact cache entry")?;
        if file_type.is_dir() {
            collect_cache_files(root, &path, files)?;
        } else if file_type.is_file() {
            files.push((
                path.strip_prefix(root)
                    .context("make model artifact cache path relative")?
                    .to_owned(),
                entry
                    .metadata()
                    .context("inspect model artifact cache file")?
                    .len(),
            ));
        }
    }
    Ok(())
}

fn assert_deployment_matches_preview(
    deployment: &ModelDeployment,
    preview: &dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
) {
    let snapshot = deployment.snapshot.as_ref().expect("deployment snapshot");
    let actual = match snapshot.source.as_ref() {
        Some(model_deployment_snapshot::Source::Local(local)) => local,
        Some(model_deployment_snapshot::Source::Upstream(_)) | None => {
            panic!("deployment snapshot has no local model")
        }
    };
    assert_eq!(
        actual, preview,
        "stored immutable snapshot differs from the previewed resolution and desired plan",
    );
}

fn assert_configured_snapshot(deployment: &ModelDeployment, expected: &ModelDeploymentSnapshot) {
    assert_eq!(
        deployment.snapshot.as_ref(),
        Some(expected),
        "deployment snapshot differs from the exact configured immutable snapshot",
    );
}

fn assert_qualification_deployment(deployment: &ModelDeployment, case: &QualificationCase) {
    assert_eq!(
        deployment
            .intent
            .as_ref()
            .and_then(|intent| intent.placement.as_ref()),
        Some(&support::exact_placement(
            "development-node",
            "nvidia-target",
        )),
        "deployment retains the explicit exact placement intent",
    );
    let snapshot = deployment.snapshot.as_ref().expect("deployment snapshot");
    let model = match snapshot.source.as_ref() {
        Some(model_deployment_snapshot::Source::Local(model)) => model,
        _ => panic!("qualification snapshot has no local catalog model"),
    };
    assert_eq!(
        model.profile_id, case.profile_id,
        "snapshot identifies the selected model profile",
    );
    assert_eq!(
        model
            .model_artifact
            .as_ref()
            .map(|artifact| artifact.artifact_id.as_str()),
        Some(case.artifact.artifact_id.as_str()),
        "snapshot identifies the selected model artifact",
    );
    let artifact = model
        .model_artifact
        .as_ref()
        .expect("snapshot selected model artifact");
    assert_eq!(
        artifact.weight_quantization,
        case.artifact.weight_quantization
    );
    assert_eq!(
        artifact
            .source
            .as_ref()
            .and_then(|source| source.content.as_ref())
            .and_then(|content| match content {
                HuggingFaceContent::File(file) => Some(file.path.as_str()),
                HuggingFaceContent::Snapshot(_) => None,
            }),
        Some(case.artifact.file.as_str()),
        "snapshot artifact filename",
    );
    let expected = match case.snapshot.source.as_ref() {
        Some(model_deployment_snapshot::Source::Local(local)) => local,
        _ => panic!("expected local catalog snapshot"),
    };
    assert_eq!(
        model.engine_artifact, expected.engine_artifact,
        "snapshot identifies the pinned engine artifact",
    );
}

fn assert_total_context_argument(endpoint: &RuntimeEndpoint, expected: u64) {
    let expected = expected.to_string();
    assert!(
        endpoint
            .command
            .windows(2)
            .any(|arguments| arguments == ["--ctx-size", expected.as_str()]),
        "llama.cpp command has no expected --ctx-size {expected}",
    );
}

fn assert_valid_custom_arguments_snapshot(
    snapshot: &dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
) {
    let expected = VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.map(str::to_owned);
    assert_eq!(snapshot.custom_argument_tokens, expected);
    let plan = snapshot
        .desired_launch_plan
        .as_ref()
        .expect("qualification desired launch plan");
    let suffix = plan
        .arguments
        .get(plan.arguments.len().saturating_sub(expected.len())..)
        .expect("qualification desired launch plan custom argument suffix");
    let actual = suffix
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(portable_model_launch_argument_token::Value::Literal(value)) => value.as_str(),
            _ => panic!("custom desired launch argument is not a literal"),
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, VALID_LLAMA_CPP_CUSTOM_ARGUMENTS);
}

fn assert_valid_custom_arguments_in_command(endpoint: &RuntimeEndpoint) {
    assert!(
        endpoint
            .command
            .windows(VALID_LLAMA_CPP_CUSTOM_ARGUMENTS.len())
            .any(|arguments| arguments == VALID_LLAMA_CPP_CUSTOM_ARGUMENTS),
        "llama.cpp command has no exact valid custom argument sequence",
    );
}

fn assert_selected_artifact_workload(
    endpoint: &RuntimeEndpoint,
    cache: &Path,
    artifact: &CatalogArtifact,
) {
    assert!(
        endpoint
            .command
            .windows(2)
            .any(|arguments| arguments == ["--model", "/opt/dotcore/models/model.gguf"]),
        "llama.cpp command has no selected model artifact mount",
    );
    let expected = cache
        .join(format!(
            "models--{}",
            artifact.repository.replace('/', "--")
        ))
        .join("snapshots")
        .join(&artifact.revision)
        .join(&artifact.file)
        .canonicalize()
        .expect("canonical selected model artifact cache path");
    let source = endpoint
        .mounts
        .iter()
        .find_map(|(source, destination, read_only)| {
            (destination == "/opt/dotcore/models/model.gguf" && *read_only).then_some(source)
        })
        .expect("read-only llama.cpp model mount");
    assert_eq!(
        Path::new(source)
            .canonicalize()
            .expect("canonical mounted model artifact path"),
        expected,
        "llama.cpp model mount does not identify the selected artifact file {}",
        artifact.file,
    );
}

/// Asserts that a split model mounts its publication directory, that `--model` names the first
/// shard inside it, and that every published shard is a hard link to its hub blob.
fn assert_split_artifact_workload(
    endpoint: &RuntimeEndpoint,
    cache: &Path,
    artifact: &CatalogArtifact,
) {
    let entry = format!("/opt/dotcore/models/model/{}", artifact.file);
    assert!(
        endpoint
            .command
            .windows(2)
            .any(|arguments| arguments == ["--model", entry.as_str()]),
        "llama.cpp command does not load the first split shard {entry}: {:?}",
        endpoint.command
    );
    let publication = endpoint
        .mounts
        .iter()
        .find_map(|(source, destination, read_only)| {
            (destination == "/opt/dotcore/models/model" && *read_only).then_some(source)
        })
        .expect("read-only llama.cpp split model directory mount");
    let publication = Path::new(publication)
        .canonicalize()
        .expect("canonical split publication");
    assert!(
        publication.starts_with(
            cache
                .join(".dotcore-snapshots")
                .canonicalize()
                .expect("canonical snapshot publication root")
        ),
        "split model mount is not a snapshot publication: {}",
        publication.display()
    );
    for path in artifact.split_files.keys() {
        let blob = cache
            .join(format!(
                "models--{}",
                artifact.repository.replace('/', "--")
            ))
            .join("snapshots")
            .join(&artifact.revision)
            .join(path);
        let published = fs::metadata(publication.join(path)).expect("published shard");
        let blob = fs::metadata(&blob).expect("hub blob");
        eprintln!(
            "[nvidia-docker] Model={} operation=split link check shard={path} published_inode={} blob_inode={} links={}",
            artifact.artifact_id,
            published.ino(),
            blob.ino(),
            published.nlink()
        );
        assert_eq!(
            (published.dev(), published.ino()),
            (blob.dev(), blob.ino()),
            "published shard {path} is not a hard link to its hub blob"
        );
    }
}

/// Returns the modification time of every regular file in the cache, so a relaunch that neither
/// downloads nor refreshes a verification marker leaves it unchanged.
fn cache_modifications(root: &Path) -> anyhow::Result<BTreeMap<PathBuf, std::time::SystemTime>> {
    cache_snapshot(root)?
        .into_iter()
        .map(|(path, _)| {
            let modified = fs::metadata(root.join(&path))
                .and_then(|metadata| metadata.modified())
                .with_context(|| format!("inspect cache file {}", path.display()))?;
            Ok((path, modified))
        })
        .collect()
}

fn assert_mounts_equal(left: &RuntimeEndpoint, right: &RuntimeEndpoint) {
    let mut left_mounts = left.mounts.clone();
    let mut right_mounts = right.mounts.clone();
    left_mounts.sort_by(|left, right| {
        (left.1.as_str(), left.0.as_str(), left.2).cmp(&(
            right.1.as_str(),
            right.0.as_str(),
            right.2,
        ))
    });
    right_mounts.sort_by(|left, right| {
        (left.1.as_str(), left.0.as_str(), left.2).cmp(&(
            right.1.as_str(),
            right.0.as_str(),
            right.2,
        ))
    });
    assert_eq!(left_mounts, right_mounts, "Docker mount sets differ");
}

fn assert_cache_mount(endpoint: &RuntimeEndpoint, cache: &Path) {
    let source = cache.to_string_lossy();
    assert!(
        endpoint
            .mounts
            .iter()
            .all(|(mount_source, destination, read_only)| {
                mount_source.starts_with(source.as_ref())
                    && destination.starts_with("/opt/dotcore/models/")
                    && *read_only
            })
    );
    assert!(
        endpoint.command.iter().all(|argument| {
            !argument.contains("http") && argument != "curl" && argument != "sh"
        })
    );
}

fn assert_auxiliary_artifact_command(
    endpoint: &RuntimeEndpoint,
    projector: Option<&CatalogArtifact>,
    draft: Option<&CatalogArtifact>,
) {
    match projector {
        Some(_) => {
            assert!(
                endpoint.command.windows(2).any(
                    |arguments| arguments == ["--mmproj", "/opt/dotcore/models/projector.gguf"]
                ),
                "vision workload command has no --mmproj"
            );
            assert!(
                endpoint
                    .command
                    .windows(2)
                    .any(|arguments| arguments == ["--image-min-tokens", "280"]),
                "vision workload command has wrong image minimum"
            );
            assert!(
                endpoint
                    .command
                    .windows(2)
                    .any(|arguments| arguments == ["--image-max-tokens", "280"]),
                "vision workload command has wrong image maximum"
            );
        }
        None => assert!(
            !endpoint
                .command
                .iter()
                .any(|argument| argument == "--mmproj")
        ),
    }
    match draft {
        Some(_) => {
            assert!(
                endpoint
                    .command
                    .windows(2)
                    .any(|arguments| arguments == ["--spec-type", "draft-mtp"]),
                "MTP workload command has no draft-mtp type"
            );
            assert!(
                endpoint.command.windows(2).any(|arguments| arguments
                    == [
                        "--model-draft",
                        "/opt/dotcore/models/speculative-draft.gguf"
                    ]),
                "MTP workload command has no local draft path"
            );
        }
        None => assert!(
            !endpoint
                .command
                .iter()
                .any(|argument| argument == "--model-draft")
        ),
    }
}

async fn assert_exact_alias(port: u16, expected_alias: &str) {
    let started = Instant::now();
    eprintln!(
        "[nvidia-docker] Model={expected_alias} operation=alias probe phase=before port={port}"
    );
    let request = format!(
        "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    let mut stream = tokio::time::timeout(
        MODEL_PROBE_CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .expect("model alias connection timeout")
    .expect("model alias connection");
    let response = tokio::time::timeout(MODEL_PROBE_RESPONSE_TIMEOUT, async {
        stream
            .write_all(request.as_bytes())
            .await
            .expect("model alias request");
        read_bounded_response(&mut stream)
            .await
            .expect("model alias response")
    })
    .await
    .expect("model alias response timeout");
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("model alias response headers");
    let headers = std::str::from_utf8(&response[..header_end]).expect("model alias status");
    assert!(
        // Engines built on Python's HTTP server answer with HTTP/1.0.
        headers.starts_with("HTTP/1.1 200") || headers.starts_with("HTTP/1.0 200"),
        "model alias failed: {headers}"
    );
    let value: Value =
        serde_json::from_slice(&response[header_end + 4..]).expect("model alias JSON response");
    assert_eq!(
        value
            .get("data")
            .and_then(Value::as_array)
            .and_then(|models| models.first())
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str),
        Some(expected_alias),
    );
    assert_eq!(
        value.get("data").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
    eprintln!(
        "[nvidia-docker] Model={expected_alias} operation=alias probe phase=after state=confirmed elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

async fn read_bounded_response(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(response);
        }
        if response.len().saturating_add(read) > MODEL_PROBE_MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("model probe response exceeds {MODEL_PROBE_MAX_RESPONSE_BYTES} bytes"),
            ));
        }
        response.extend_from_slice(&chunk[..read]);
    }
}

async fn assert_completion(port: u16, model_id: &str) -> Duration {
    let started = Instant::now();
    let value = completion_value(port, model_id).await;
    if let Err(error) = parse_completion_response(&value) {
        panic!("{error}: {}", completion_diagnostics(&value));
    }
    let duration = started.elapsed();
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=direct completion phase=after state=complete elapsed_ms={}",
        duration.as_millis()
    );
    duration
}

/// Asserts that a model too small to follow instructions still generates text.
async fn assert_generation(port: u16, model_id: &str) {
    let value = completion_value(port, model_id).await;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !content.trim().is_empty(),
        "completion generated no text: {}",
        completion_diagnostics(&value)
    );
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=direct generation phase=after state=complete content={:?}",
        bounded_completion_text(content)
    );
}

/// Sends one bounded chat completion to a model's direct port and returns its JSON response.
async fn completion_value(port: u16, model_id: &str) -> Value {
    eprintln!(
        "[nvidia-docker] Model={model_id} operation=direct completion phase=before port={port}"
    );
    let payload = serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": "Reply with exactly READY."}],
        "max_tokens": 8,
        "temperature": 0,
        "reasoning_effort": "none",
    });
    let body = serde_json::to_vec(&payload).expect("completion payload");
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = tokio::time::timeout(
        Duration::from_secs(60),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .expect("completion connection timeout")
    .expect("completion connection");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("completion request headers");
    stream
        .write_all(&body)
        .await
        .expect("completion request body");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut response))
        .await
        .expect("completion response timeout")
        .expect("completion response");
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("completion response headers");
    let headers = std::str::from_utf8(&response[..header_end]).expect("completion response status");
    assert!(
        headers.starts_with("HTTP/1.1 200"),
        "completion failed: {headers}"
    );
    serde_json::from_slice(&response[header_end + 4..]).expect("completion JSON response")
}

fn parse_completion_response(value: &Value) -> anyhow::Result<()> {
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .context("completion response has no message")?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .context("completion response has no final content")?;
    if !matches!(content.trim(), "READY" | "READY.") {
        anyhow::bail!("completion final content is not exactly READY or READY.");
    }
    match message.get("reasoning_content") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(reasoning)) if reasoning.trim().is_empty() => Ok(()),
        Some(_) => anyhow::bail!("completion reasoning content must be missing, null, or empty"),
    }
}

fn completion_diagnostics(value: &Value) -> String {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));
    format!(
        "finish_reason={} visible_content={} reasoning_content={}",
        bounded_completion_value(choice.and_then(|choice| choice.get("finish_reason"))),
        bounded_completion_value(message.and_then(|message| message.get("content"))),
        bounded_completion_value(message.and_then(|message| message.get("reasoning_content"))),
    )
}

fn bounded_completion_value(value: Option<&Value>) -> String {
    match value {
        None => "<missing>".to_owned(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::String(value)) => format!("{:?}", bounded_completion_text(value)),
        Some(Value::Bool(_)) => "<boolean>".to_owned(),
        Some(Value::Number(_)) => "<number>".to_owned(),
        Some(Value::Array(_)) => "<array>".to_owned(),
        Some(Value::Object(_)) => "<object>".to_owned(),
    }
}

fn bounded_completion_text(value: &str) -> String {
    bounded_text(value, COMPLETION_DIAGNOSTIC_MAX_CHARS)
}

fn router_failure_diagnostics(response: &[u8]) -> String {
    let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return "status=<missing> content_type=<missing> public_error=<missing>".to_owned();
    };
    let headers = std::str::from_utf8(&response[..header_end]).unwrap_or_default();
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("<missing>");
    let content_type = headers
        .lines()
        .skip(1)
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.trim())
        })
        .map(|value| bounded_text(value, ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS))
        .unwrap_or_else(|| "<missing>".to_owned());
    let body = String::from_utf8_lossy(&response[header_end + 4..]);
    let body = bounded_text(&body, ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS);
    format!("status={status} content_type={content_type:?} public_error={body:?}")
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_site_fixture_is_semantically_valid() {
        for platform in [
            ExecutionPlatform::linux_amd64(),
            ExecutionPlatform::linux_arm64(),
        ] {
            let root = crate::support::canonical_tempdir().expect("temporary site fixture");
            nvidia_qualification::materialize_site(FIXTURE, &platform, root.path())
                .expect("materialize NVIDIA site fixture");
            InitialSiteInputs::load(root.path(), &dotcore::model::site_declaration_rules())
                .expect("NVIDIA site fixture must pass shared site validation");
        }
    }

    #[test]
    fn split_fixture_resolves_a_snapshot_artifact_loaded_from_its_first_shard() {
        let root = crate::support::canonical_tempdir().expect("temporary site fixture");
        nvidia_qualification::materialize_site(
            FIXTURE,
            &ExecutionPlatform::linux_amd64(),
            root.path(),
        )
        .expect("materialize NVIDIA site fixture");
        let catalog = nvidia_qualification::load_catalog(root.path()).expect("NVIDIA site catalog");

        for platform in [
            ExecutionPlatform::linux_amd64(),
            ExecutionPlatform::linux_arm64(),
        ] {
            let case = qualification_case(
                &catalog,
                SPLIT_MODEL_ID,
                SPLIT_PROFILE_ID,
                &platform,
                &synthetic_nvidia_observation(),
            )
            .expect("split qualification case");
            let Some(model_deployment_snapshot::Source::Local(local)) =
                case.snapshot.source.as_ref()
            else {
                panic!("split model resolves to a local snapshot");
            };
            let source = local
                .model_artifact
                .as_ref()
                .and_then(|artifact| artifact.source.as_ref())
                .expect("split model artifact source");

            assert!(matches!(
                source.content,
                Some(HuggingFaceContent::Snapshot(_))
            ));
            assert_eq!(
                catalog_artifact(
                    &case.artifact.artifact_id,
                    &case.artifact.weight_quantization,
                    source,
                )
                .expect("resolved split artifact"),
                case.artifact
            );
            assert_eq!(
                case.artifact.file,
                "tinyllamas/split/stories15M-q8_0-00001-of-00003.gguf"
            );
            assert_eq!(
                case.artifact.split_files.keys().collect::<Vec<_>>(),
                [
                    "tinyllamas/split/stories15M-q8_0-00001-of-00003.gguf",
                    "tinyllamas/split/stories15M-q8_0-00002-of-00003.gguf",
                    "tinyllamas/split/stories15M-q8_0-00003-of-00003.gguf",
                ]
            );
        }
    }

    #[test]
    fn nvidia_catalog_fixture_resolves_development_source_docker_image_engines() {
        let site_root = crate::support::canonical_tempdir().expect("temporary site root");
        let catalog = nvidia_qualification::load_catalog(site_root.path()).expect("NVIDIA catalog");

        for platform in [
            ExecutionPlatform::linux_amd64(),
            ExecutionPlatform::linux_arm64(),
        ] {
            for case in qualification_cases(&catalog, &platform, &synthetic_nvidia_observation())
                .expect("qualification cases")
            {
                assert!(
                    nvidia_qualification::development_source_docker_image(&case.engine),
                    "{platform} {}: {:?}",
                    case.profile_id,
                    case.engine
                );
                assert_eq!(
                    case.engine.platform,
                    Some(platform.to_oci_platform()),
                    "{platform} {}",
                    case.profile_id
                );
            }
        }
    }

    #[test]
    fn stopped_inspection_without_host_mappings_preserves_retained_endpoint_observation() {
        let inspection: ContainerInspectResponse = serde_json::from_value(serde_json::json!({
            "State": {"Running": false, "Pid": 0, "Status": "exited"},
            "NetworkSettings": {"Ports": null},
            "Mounts": [],
            "Config": {"Cmd": ["/server", "--port", "8080"]},
        }))
        .expect("stopped Docker inspection fixture");

        let endpoint =
            runtime_endpoint_from_inspection("retained-container-identity".to_owned(), inspection)
                .expect("stopped inspection without host mappings");

        assert_eq!(endpoint.container_id, "retained-container-identity");
        assert!(!endpoint.running);
        assert_eq!(endpoint.pid, 0);
        assert_eq!(endpoint.status, "EXITED");
        assert_eq!(endpoint.http_port, None);
    }

    #[test]
    fn inspection_rejects_malformed_or_out_of_range_host_ports() {
        for host_port in ["not-a-port", "65536"] {
            let inspection: ContainerInspectResponse = serde_json::from_value(serde_json::json!({
                "State": {"Running": false, "Pid": 0, "Status": "exited"},
                "NetworkSettings": {"Ports": {"8080/tcp": [{"HostPort": host_port}]}},
                "Mounts": [],
                "Config": {"Cmd": ["/server", "--port", "8080"]},
            }))
            .expect("malformed Docker inspection fixture");

            let error = runtime_endpoint_from_inspection(
                "retained-container-identity".to_owned(),
                inspection,
            )
            .expect_err("present malformed Docker HostPort must be rejected");
            let diagnostic = format!("{error:#}");

            assert!(diagnostic.contains("Docker 8080/tcp HostPort"));
            assert!(diagnostic.contains(host_port));
            assert!(diagnostic.contains("unsigned 16-bit port"));
        }
    }

    #[test]
    fn completion_diagnostics_are_bounded_and_include_reasoning() {
        let value = serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "content": "visible",
                    "reasoning_content": "x".repeat(COMPLETION_DIAGNOSTIC_MAX_CHARS + 1),
                },
            }],
        });

        let diagnostics = completion_diagnostics(&value);

        assert!(diagnostics.contains("finish_reason=\"length\""));
        assert!(diagnostics.contains("visible_content=\"visible\""));
        assert!(diagnostics.contains("reasoning_content="));
        assert!(diagnostics.contains("…"));
        assert!(!diagnostics.contains(&"x".repeat(COMPLETION_DIAGNOSTIC_MAX_CHARS + 1)));
    }

    #[test]
    fn router_failure_diagnostics_are_bounded_and_public_only() {
        let public_error = format!(
            r#"{{"error":{{"message":"{}","type":"upstream_error"}}}}"#,
            "x".repeat(ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS + 1),
        );
        let response = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nX-Private-Engine-Endpoint: http://127.0.0.1:4891\r\n\r\n{public_error}"
        );

        let diagnostics = router_failure_diagnostics(response.as_bytes());

        assert!(diagnostics.contains("status=502"));
        assert!(diagnostics.contains("content_type=\"application/json\""));
        assert!(diagnostics.contains("public_error="));
        assert!(diagnostics.contains('…'));
        assert!(!diagnostics.contains("X-Private-Engine-Endpoint"));
        assert!(!diagnostics.contains("127.0.0.1"));
        assert!(!diagnostics.contains(&"x".repeat(ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS + 1)));
    }

    #[test]
    fn image_response_diagnostics_are_bounded_and_include_public_completion_data() {
        let value = serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"content": "visible"},
            }],
            "usage": {"completion_tokens": 7},
        });
        let public_response = "x".repeat(ROUTER_FAILURE_DIAGNOSTIC_MAX_CHARS + 1);
        let raw_response =
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{public_response}");

        let diagnostics = image_response_diagnostics(
            &value,
            "/choices/0/message/content",
            raw_response.as_bytes(),
        );

        assert!(diagnostics.contains("finish_reason=\"length\""));
        assert!(diagnostics.contains("visible_content=\"visible\""));
        assert!(diagnostics.contains("completion_use=7"));
        assert!(diagnostics.contains("public_response="));
        assert!(diagnostics.contains('…'));
        assert!(!diagnostics.contains(&public_response));
    }

    #[test]
    fn completion_parser_accepts_trimmed_ready_markers_without_reasoning() {
        for reasoning in [None, Some(Value::Null), Some(Value::String(String::new()))] {
            for marker in ["READY", "READY."] {
                let mut message = serde_json::Map::from_iter([(
                    "content".to_owned(),
                    Value::String(format!("  {marker}\n")),
                )]);
                if let Some(reasoning) = reasoning.clone() {
                    message.insert("reasoning_content".to_owned(), reasoning);
                }
                let response = serde_json::json!({"choices": [{"message": message}]});
                assert!(parse_completion_response(&response).is_ok());
            }
        }
    }

    #[test]
    fn completion_parser_rejects_non_marker_content_or_reasoning() {
        for content in ["ready", "READY..", "READY now", "READY. now", ""] {
            let response = serde_json::json!({
                "choices": [{"message": {"content": content}}]
            });
            assert!(parse_completion_response(&response).is_err());
        }
        let response = serde_json::json!({
            "choices": [{"message": {"content": "READY", "reasoning_content": "think"}}]
        });
        assert!(parse_completion_response(&response).is_err());
    }

    #[tokio::test]
    async fn model_probe_response_collection_is_size_bounded() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("probe listener");
        let address = listener.local_addr().expect("probe listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("probe connection");
            stream
                .write_all(&vec![b'x'; MODEL_PROBE_MAX_RESPONSE_BYTES + 1])
                .await
                .expect("probe response");
        });
        let mut stream = TcpStream::connect(address).await.expect("probe connection");

        let error = read_bounded_response(&mut stream)
            .await
            .expect_err("oversized probe response");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        server.await.expect("probe server");
    }

    #[tokio::test]
    async fn model_probe_response_deadline_bounds_a_stalled_response() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("probe listener");
        let address = listener.local_addr().expect("probe listener address");
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("probe connection");
            std::future::pending::<()>().await;
        });
        let mut stream = TcpStream::connect(address).await.expect("probe connection");

        let result = tokio::time::timeout(
            Duration::from_millis(20),
            read_bounded_response(&mut stream),
        )
        .await;

        assert!(result.is_err(), "stalled probe response must time out");
        server.abort();
    }
}
