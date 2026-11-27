//! In-code oMLX fixtures for backend unit tests, shaped like a Qwen3.5 4B oMLX profile on an
//! Apple Silicon Mac.
//!
//! They provide a text-only model with one MLX snapshot, a valid oMLX configuration, its release
//! with a native darwin/arm64 Nix artifact, and an accepted deployment snapshot whose launch plan
//! is the canonical plan for the profile: a 32,768-token context per request for 2 parallel
//! requests, a 65,536-token total context, a 2 GiB prompt cache, and a 10 GB accelerator plus
//! 2 GiB host estimate.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
};
use dotcore_domain::ExecutionPlatform;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, EngineParameterSchema, EngineRelease, HardwareRequirements,
        HardwareSupport, HuggingFaceArtifactSource, HuggingFaceSnapshotArtifact,
        HuggingFaceSnapshotFile, InferenceRequestKind, MemoryPool, Model, ModelArtifact,
        ModelArtifactBinding, ModelEngineConfiguration, ModelExecutionCapabilities,
        ModelInputCapabilities, ModelInputModality, ModelParallelShape, NixArtifact, OciPlatform,
        ParameterDefinition, ParameterValue, ParameterValues, ParameterVisibility, Selector,
        UnsignedIntegerParameterSchema, engine_artifact::Kind as EngineArtifactKind,
        hugging_face_artifact_source, nix_artifact::Provides, parameter_definition,
        parameter_value,
    },
    deployment::v1::ArtifactPreparationPolicy,
    engine::{
        omlx::v1::{OmlxFixedModelSettings, OmlxModelConfiguration},
        v1::{EngineKind, EngineModelConfiguration, engine_model_configuration::Configuration},
    },
    model::v1::{
        ModelDeploymentSnapshot, ResolvedLocalModelDeploymentSnapshot, ResolvedSizing,
        model_deployment_snapshot,
    },
    observation::v1::AcceleratorMemoryArchitecture,
};

/// Engine parameter holding the in-memory prompt cache size, in MiB.
pub(crate) const PROMPT_CACHE_MIB_PARAMETER_ID: &str = "prompt-cache-memory-mib";

/// Store path the fixture's native Nix artifact is retained at.
pub(crate) const STORE_PATH: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-omlx-0.8.4";

/// Accelerator and host memory estimates of the profile, in bytes.
pub(crate) const ACCELERATOR_ESTIMATE_BYTES: u64 = 10_000_000_000;
pub(crate) const HOST_ESTIMATE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Resolved engine parameter values of the profile: the engine's default context per request for
/// 2 parallel requests, their product as the total context, and a 2 GiB prompt cache.
const PROFILE_PARAMETERS: [(&str, u64); 4] = [
    (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
    (PARALLEL_REQUESTS_PARAMETER_ID, 2),
    (PROMPT_CACHE_MIB_PARAMETER_ID, 2_048),
];

/// Returns an accepted oMLX snapshot for the profile values on an Apple native target, serving
/// `model_id`.
pub(crate) fn local_model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    let model = model();
    let configuration = configuration();
    let selection = crate::model::engine::omlx::realize_model(
        &model,
        &configuration,
        &configuration.artifact_bindings[0],
        &[],
    )
    .expect("oMLX fixture realization");

    let mut snapshot = super::local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("local fixture snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("engine artifact");
    engine.engine_release_id = "fixture-omlx".to_owned();
    engine.engine_kind = EngineKind::Omlx as i32;
    engine.engine_version = "0.8.4".to_owned();
    engine.artifact = Some(EngineArtifact {
        selectors: Vec::new(),
        ..release().artifacts[0].clone()
    });
    if let Some(EngineArtifactKind::Nix(nix)) = engine
        .artifact
        .as_mut()
        .and_then(|artifact| artifact.kind.as_mut())
    {
        // A deployment retains the locked revision the materialized catalog pins.
        nix.flake = dotcore_domain::test_support::nix::locked_flake();
        nix.store_paths = vec![dotcore_proto::dotcore::catalog::v1::NixSystemStorePath {
            system: "aarch64-darwin".to_owned(),
            store_path: STORE_PATH.to_owned(),
        }];
    }
    engine.platform = Some(ExecutionPlatform::darwin_arm64().to_oci_platform());
    engine.accelerator = "apple".to_owned();
    engine.minimum_nvidia_driver_major_version = 0;
    engine.preparation_policy = ArtifactPreparationPolicy::ImmutableRemote as i32;
    engine.launch_identity = STORE_PATH.to_owned();
    local.model_artifact = Some(model.artifacts[0].clone());
    let hardware_support = local.hardware_support.as_mut().expect("hardware support");
    hardware_support.hardware_class_id = "apple".to_owned();
    hardware_support
        .selector
        .as_mut()
        .expect("hardware class selector")
        .accelerators = vec!["apple".to_owned()];
    hardware_support.hardware_requirements =
        configuration.hardware_support[0].hardware_requirements;
    local.resolved_engine_state = Some(selection.state);
    local.input_capabilities = Some(selection.input_capabilities);
    local.engine_parameter_schema = release().parameter_schema;
    local.engine_parameter_schema_id = "fixture-omlx-v1".to_owned();
    local.engine_parameter_values = Some(parameter_values(PROFILE_PARAMETERS));
    local.resolved_options = Vec::new();
    local
        .selected_target
        .as_mut()
        .expect("selected target")
        .accelerator_memory_architecture = AcceleratorMemoryArchitecture::Unified as i32;
    local.sizing = Some(ResolvedSizing {
        accelerator_memory_bytes: Some(ACCELERATOR_ESTIMATE_BYTES),
        host_memory_bytes: Some(HOST_ESTIMATE_BYTES),
        ..Default::default()
    });
    local.desired_launch_plan = None;
    refresh_launch_plan(local);
    snapshot
}

/// Recomputes the canonical launch plan after a test edits an oMLX snapshot.
pub(crate) fn refresh_launch_plan(local: &mut ResolvedLocalModelDeploymentSnapshot) {
    local.desired_launch_plan =
        Some(crate::model::engine::omlx::translate(local).expect("oMLX fixture plan"));
}

/// Returns oMLX engine parameter values for the given pairs.
pub(crate) fn parameter_values<const N: usize>(values: [(&str, u64); N]) -> ParameterValues {
    ParameterValues {
        values: values
            .into_iter()
            .map(|(parameter_id, value)| ParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(parameter_value::Value::UnsignedInteger(value)),
            })
            .collect(),
    }
}

/// Returns an oMLX engine release with the shared context parameters, their engine defaults and
/// the prompt cache drawn from host memory, and one native darwin/arm64 Apple artifact.
pub(crate) fn release() -> EngineRelease {
    EngineRelease {
        engine_release_id: "fixture-omlx".to_owned(),
        engine_kind: EngineKind::Omlx as i32,
        version: "0.8.4".to_owned(),
        parameter_schema: Some(EngineParameterSchema {
            schema_id: "fixture-omlx-v1".to_owned(),
            parameters: [
                (
                    TOTAL_CONTEXT_PARAMETER_ID,
                    "Total context",
                    512,
                    "tokens",
                    None,
                    None,
                ),
                (
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    "Context per request",
                    512,
                    "tokens",
                    Some(32_768),
                    None,
                ),
                (
                    PARALLEL_REQUESTS_PARAMETER_ID,
                    "Parallel requests",
                    1,
                    "requests",
                    Some(2),
                    None,
                ),
                (
                    PROMPT_CACHE_MIB_PARAMETER_ID,
                    "Prompt cache memory",
                    0,
                    "MiB",
                    None,
                    Some(MemoryPool::Host as i32),
                ),
            ]
            .into_iter()
            .map(
                |(parameter_id, display_name, minimum, unit, default_value, memory_pool)| {
                    ParameterDefinition {
                        parameter_id: parameter_id.to_owned(),
                        display_name: display_name.to_owned(),
                        parameter_type: Some(parameter_definition::ParameterType::UnsignedInteger(
                            UnsignedIntegerParameterSchema {
                                minimum,
                                maximum: None,
                                unit: unit.to_owned(),
                                default_value,
                                memory_pool,
                            },
                        )),
                        visibility: if memory_pool.is_some() {
                            ParameterVisibility::Advanced
                        } else {
                            ParameterVisibility::Basic
                        } as i32,
                        description: parameter_id.to_owned(),
                        enabled_by_parameter_id: None,
                        group_id: None,
                    }
                },
            )
            .collect(),
            parameter_groups: Vec::new(),
        }),
        artifacts: vec![EngineArtifact {
            artifact_id: "darwin-arm64".to_owned(),
            kind: Some(EngineArtifactKind::Nix(NixArtifact {
                flake: "self".to_owned(),
                package: Some("engines.omlx.native".to_owned()),
                provides: Some(Provides::Executable("omlx-launcher".to_owned())),
                store_paths: Vec::new(),
            })),
            selectors: vec![Selector {
                platform: Some(OciPlatform {
                    operating_system: "darwin".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                }),
                accelerators: vec!["apple".to_owned()],
                ..Default::default()
            }],
        }],
        ..Default::default()
    }
}

/// Returns a text-only model with a 262,144-token maximum context, one MLX 4-bit snapshot and no
/// options.
pub(crate) fn model() -> Model {
    Model {
        model_id: "fixture-omlx-model".to_owned(),
        maximum_context_tokens: 262_144,
        artifacts: vec![ModelArtifact {
            artifact_id: "fixture-mlx-4bit".to_owned(),
            weight_quantization: "4bit".to_owned(),
            source: Some(HuggingFaceArtifactSource {
                repository: "dotcore/fixture-mlx-4bit".to_owned(),
                revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                content: Some(hugging_face_artifact_source::Content::Snapshot(
                    HuggingFaceSnapshotArtifact {
                        files: SNAPSHOT_FILES
                            .into_iter()
                            .map(|path| HuggingFaceSnapshotFile {
                                path: path.to_owned(),
                                provenance_digest:
                                    "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
                                        .to_owned(),
                            })
                            .collect(),
                    },
                )),
            }),
        }],
        ..Default::default()
    }
}

/// Files of the fixture's MLX snapshot, as an mlx-community 4-bit conversion publishes them.
const SNAPSHOT_FILES: [&str; 5] = [
    "config.json",
    "model.safetensors",
    "model.safetensors.index.json",
    "tokenizer.json",
    "tokenizer_config.json",
];

/// Returns a valid text-only oMLX configuration for [`model`] and [`release`].
pub(crate) fn configuration() -> ModelEngineConfiguration {
    ModelEngineConfiguration {
        configuration_id: "omlx-apple".to_owned(),
        artifact_bindings: vec![ModelArtifactBinding {
            binding_id: "fixture-mlx-4bit".to_owned(),
            model_artifact_id: "fixture-mlx-4bit".to_owned(),
            auxiliary_artifact_ids: Vec::new(),
        }],
        hardware_support: vec![HardwareSupport {
            hardware_class_id: "apple".to_owned(),
            hardware_requirements: Some(HardwareRequirements {
                minimum_memory_bytes_per_device: 0,
                minimum_system_memory_bytes: 16_000_000_000,
            }),
        }],
        engine_configuration: Some(EngineModelConfiguration {
            engine_kind: EngineKind::Omlx as i32,
            configuration: Some(Configuration::Omlx(OmlxModelConfiguration {
                fixed_settings: Some(OmlxFixedModelSettings {
                    prompt_cache_mib_parameter_id: PROMPT_CACHE_MIB_PARAMETER_ID.to_owned(),
                }),
            })),
        }),
        execution_capabilities: Some(ModelExecutionCapabilities {
            supported_parallel_shapes: vec![ModelParallelShape::One as i32],
        }),
        request_kinds: vec![InferenceRequestKind::Chat as i32],
        input_capabilities: Some(ModelInputCapabilities {
            supported_modalities: vec![ModelInputModality::Text as i32],
            image_input_capacity: None,
        }),
        ..Default::default()
    }
}
