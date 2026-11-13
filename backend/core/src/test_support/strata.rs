//! In-code Strata fixtures for backend unit tests, shaped like the Qwen3.6 35B-A3B and Qwen3.8
//! Flash-Next Strata profiles.
//!
//! They provide a text-only model with a single GGUF, a split GGUF set and a ready-made pack, plus
//! a draft layer; a valid Strata configuration for each; and accepted deployment snapshots whose
//! launch plan is the canonical plan for the profile: a 32,768-token total context and context per
//! request for one parallel request, and an 8 GiB expert accelerator memory cap.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, EngineRelease, HardwareRequirements, HardwareSupport,
        HuggingFaceArtifactSource, HuggingFaceFileArtifact, HuggingFaceSnapshotArtifact,
        HuggingFaceSnapshotFile, InferenceRequestKind, Model, ModelArtifact, ModelArtifactBinding,
        ModelAuxiliaryArtifact, ModelEngineConfiguration, ModelExecutionCapabilities,
        ModelInputCapabilities, ModelInputModality, ModelParallelShape, ParameterDefinition,
        ParameterValue, ParameterValues, ParameterVisibility, SecretParameterSchema,
        StorageBinding, StorageKind, StoragePlacement, UnsignedIntegerParameterSchema,
        hugging_face_artifact_source, parameter_definition, parameter_value,
    },
    engine::{
        strata::v1::{
            StrataFixedModelSettings, StrataModelConfiguration, StrataReadyPack, StrataSingleGguf,
            StrataSplitGguf, strata_fixed_model_settings::ModelFiles,
        },
        v1::{EngineKind, EngineModelConfiguration, engine_model_configuration::Configuration},
    },
    model::v1::{
        ModelDeploymentSnapshot, ResolvedLocalModelDeploymentSnapshot, model_deployment_snapshot,
    },
};

/// Engine parameter holding the accelerator memory cap of the model's experts, in MiB.
pub(crate) const EXPERT_VRAM_MIB_PARAMETER_ID: &str = "expert-vram-mib";

/// Auxiliary artifact holding the fixture model's separate draft layer.
pub(crate) const DRAFT_LAYER_ARTIFACT_ID: &str = "fixture-draft-layer";

/// Secret engine parameter some fixtures add: the deployment's API key.
pub(crate) const API_KEY_PARAMETER_ID: &str = "api-key";

/// Length of a generated [`API_KEY_PARAMETER_ID`] value.
pub(crate) const API_KEY_GENERATED_LENGTH: u32 = 32;

/// Resolved engine parameter values of the profile: the engine's default total context, the
/// context per request filled from it, one parallel request and the expert cap from sizing.
const PROFILE_PARAMETERS: [(&str, u64); 4] = [
    (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
    (PARALLEL_REQUESTS_PARAMETER_ID, 1),
    (EXPERT_VRAM_MIB_PARAMETER_ID, 8_192),
];

/// Returns an accepted single-GGUF Strata snapshot for the profile values, serving `model_id`.
pub(crate) fn local_model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    local_model_snapshot_with(model_id, &configuration())
}

/// Returns an accepted Strata snapshot of `configuration`'s first binding for the profile values,
/// serving `model_id`.
pub(crate) fn local_model_snapshot_with(
    model_id: &str,
    configuration: &ModelEngineConfiguration,
) -> ModelDeploymentSnapshot {
    let model = model();
    let binding = &configuration.artifact_bindings[0];
    let selection =
        crate::model::engine::strata::realize_model(&model, configuration, binding, &[])
            .expect("Strata fixture realization");

    let mut snapshot = super::local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("local fixture snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("engine artifact");
    engine.engine_release_id = "fixture-strata".to_owned();
    engine.engine_kind = EngineKind::Strata as i32;
    local.model_artifact = model
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == binding.model_artifact_id)
        .cloned();
    local.auxiliary_artifacts = model
        .auxiliary_artifacts
        .iter()
        .filter(|artifact| {
            selection
                .auxiliary_artifact_ids
                .contains(&artifact.auxiliary_artifact_id)
        })
        .cloned()
        .collect();
    local
        .hardware_support
        .as_mut()
        .expect("hardware support")
        .hardware_requirements = configuration.hardware_support[0].hardware_requirements;
    local.resolved_engine_state = Some(selection.state);
    local.input_capabilities = Some(selection.input_capabilities);
    local.engine_parameter_schema = release().parameter_schema;
    local.engine_parameter_schema_id = "fixture-strata-v1".to_owned();
    local.engine_parameter_values = Some(parameter_values(PROFILE_PARAMETERS));
    local.resolved_options = Vec::new();
    local.desired_launch_plan = None;
    refresh_launch_plan(local);
    snapshot
}

/// Returns [`local_model_snapshot`] from [`release_with_api_key`], with `api_key` as the value of
/// its secret.
pub(crate) fn local_model_snapshot_with_api_key(
    model_id: &str,
    api_key: &str,
) -> ModelDeploymentSnapshot {
    let mut snapshot = local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("local fixture snapshot");
    };
    local.engine_parameter_schema = release_with_api_key().parameter_schema;
    local
        .engine_parameter_values
        .as_mut()
        .expect("engine parameter values")
        .values
        .push(ParameterValue {
            parameter_id: API_KEY_PARAMETER_ID.to_owned(),
            value: Some(parameter_value::Value::String(api_key.to_owned())),
        });
    refresh_launch_plan(local);
    snapshot
}

/// Recomputes the canonical launch plan after a test edits a Strata snapshot.
pub(crate) fn refresh_launch_plan(local: &mut ResolvedLocalModelDeploymentSnapshot) {
    local.desired_launch_plan =
        Some(crate::model::engine::strata::translate(local).expect("Strata fixture plan"));
}

/// Returns Strata engine parameter values for the given pairs.
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

/// Returns a Strata engine release with the shared context parameters at the wrapper's limits and
/// engine defaults, the expert cap, and the deployment's data folder.
pub(crate) fn release() -> EngineRelease {
    EngineRelease {
        engine_release_id: "fixture-strata".to_owned(),
        engine_kind: EngineKind::Strata as i32,
        parameter_schema: Some(EngineParameterSchema {
            schema_id: "fixture-strata-v1".to_owned(),
            parameters: [
                (
                    TOTAL_CONTEXT_PARAMETER_ID,
                    "Total context",
                    1024,
                    2_097_152,
                    "tokens",
                    Some(32_768),
                ),
                (
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    "Context per request",
                    1024,
                    262_144,
                    "tokens",
                    None,
                ),
                (
                    PARALLEL_REQUESTS_PARAMETER_ID,
                    "Parallel requests",
                    1,
                    8,
                    "requests",
                    Some(1),
                ),
                (
                    EXPERT_VRAM_MIB_PARAMETER_ID,
                    "Expert accelerator memory",
                    1,
                    1_048_576,
                    "MiB",
                    None,
                ),
            ]
            .into_iter()
            .map(
                |(parameter_id, display_name, minimum, maximum, unit, default_value)| {
                    ParameterDefinition {
                        parameter_id: parameter_id.to_owned(),
                        display_name: display_name.to_owned(),
                        parameter_type: Some(parameter_definition::ParameterType::UnsignedInteger(
                            UnsignedIntegerParameterSchema {
                                minimum,
                                maximum,
                                unit: unit.to_owned(),
                                default_value,
                            },
                        )),
                        visibility: match parameter_id {
                            "total-context" | "context-per-request" => ParameterVisibility::Basic,
                            _ => ParameterVisibility::Advanced,
                        } as i32,
                        description: parameter_id.to_owned(),
                    }
                },
            )
            .collect(),
        }),
        storage_bindings: vec![StorageBinding {
            binding_name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            kind: StorageKind::Cache as i32,
            placements: vec![StoragePlacement::Deployment as i32],
            environment: Vec::new(),
        }],
        artifacts: vec![dotcore_domain::test_support::oci::engine_artifact(
            "linux-amd64-cuda",
            Some("registry.dotcore.test"),
            "strata",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "linux",
            "amd64",
            "nvidia",
        )],
        ..Default::default()
    }
}

/// Returns [`release`] with the secret [`API_KEY_PARAMETER_ID`] added after its other parameters.
pub(crate) fn release_with_api_key() -> EngineRelease {
    let mut release = release();
    release
        .parameter_schema
        .as_mut()
        .expect("schema")
        .parameters
        .push(ParameterDefinition {
            parameter_id: API_KEY_PARAMETER_ID.to_owned(),
            display_name: "API key".to_owned(),
            parameter_type: Some(parameter_definition::ParameterType::Secret(
                SecretParameterSchema {
                    generated_length: API_KEY_GENERATED_LENGTH,
                    minimum_length: 16,
                },
            )),
            visibility: ParameterVisibility::Advanced as i32,
            description: API_KEY_PARAMETER_ID.to_owned(),
        });
    release
}

/// Returns [`CATALOG_ENGINE`] with the secret [`API_KEY_PARAMETER_ID`] of
/// [`release_with_api_key`].
pub(crate) fn catalog_engine_with_api_key() -> String {
    CATALOG_ENGINE.replace(
        "storageBindings:\n",
        "    - { parameterId: \"api-key\", displayName: \"API key\", visibility: \"PARAMETER_VISIBILITY_ADVANCED\", description: \"api-key\", secret: { generatedLength: 32, minimumLength: 16 } }\nstorageBindings:\n",
    )
}

/// Loads the given engine and model declarations with the shared hardware classes.
pub(crate) fn load_catalog(engine: &str, model: &str) -> anyhow::Result<dotcore_catalog::Catalog> {
    let root = tempfile::tempdir().expect("Catalog root");
    for (path, content) in [
        ("engines/strata.yaml", engine),
        ("models/strata-model.yaml", model),
    ] {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().expect("directory")).expect("directory");
        std::fs::write(path, content).expect("declaration");
    }
    dotcore_catalog::test_support::write_hardware_classes(root.path());

    dotcore_catalog::Catalog::load_with_policy(
        root.path(),
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .map_err(anyhow::Error::new)
}

/// Returns a model with a single GGUF, a split GGUF set, a ready-made pack and a draft layer, and
/// no options.
pub(crate) fn model() -> Model {
    Model {
        model_id: "fixture-strata-model".to_owned(),
        artifacts: vec![
            ModelArtifact {
                artifact_id: "fixture-strata-artifact".to_owned(),
                weight_quantization: "Q4_K_XL".to_owned(),
                source: Some(HuggingFaceArtifactSource {
                    repository: "dotcore/fixture-strata".to_owned(),
                    revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    content: Some(hugging_face_artifact_source::Content::File(
                        HuggingFaceFileArtifact {
                            path: "fixture-Q4_K_XL.gguf".to_owned(),
                            provenance_digest:
                                "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
                                    .to_owned(),
                        },
                    )),
                }),
            },
            ModelArtifact {
                artifact_id: "fixture-split-gguf".to_owned(),
                weight_quantization: "IQ2_XS".to_owned(),
                source: Some(snapshot_source([
                    "IQ2_XS/fixture-IQ2_XS-00001-of-00002.gguf",
                    "IQ2_XS/fixture-IQ2_XS-00002-of-00002.gguf",
                ])),
            },
            ModelArtifact {
                artifact_id: "fixture-ready-pack".to_owned(),
                weight_quantization: "NVFP4".to_owned(),
                source: Some(snapshot_source([
                    "pack/dense.bin",
                    "pack/experts.bin",
                    "pack/index.txt",
                    "pack/native_experts.txt",
                    "pack/tokenizer/vocab.json",
                    "dense.gguf",
                    "lookup-table.gguf",
                    "token-embedding.gguf",
                ])),
            },
        ],
        auxiliary_artifacts: vec![ModelAuxiliaryArtifact {
            auxiliary_artifact_id: DRAFT_LAYER_ARTIFACT_ID.to_owned(),
            source: Some(snapshot_source([
                "mtp/dense.bin",
                "mtp/dense.txt",
                "mtp/experts.bin",
            ])),
        }],
        ..Default::default()
    }
}

/// Returns a valid text-only Strata configuration for [`model`] and [`release`].
pub(crate) fn configuration() -> ModelEngineConfiguration {
    ModelEngineConfiguration {
        configuration_id: "strata-nvidia".to_owned(),
        artifact_bindings: vec![ModelArtifactBinding {
            binding_id: "fixture-strata-artifact".to_owned(),
            model_artifact_id: "fixture-strata-artifact".to_owned(),
            auxiliary_artifact_ids: Vec::new(),
        }],
        hardware_support: vec![HardwareSupport {
            hardware_class_id: "nvidia".to_owned(),
            hardware_requirements: Some(HardwareRequirements {
                minimum_memory_bytes_per_device: 32_000_000_000,
                minimum_system_memory_bytes: 64_000_000_000,
            }),
        }],
        engine_configuration: Some(EngineModelConfiguration {
            engine_kind: EngineKind::Strata as i32,
            configuration: Some(Configuration::Strata(StrataModelConfiguration {
                fixed_settings: Some(StrataFixedModelSettings {
                    expert_vram_mib_parameter_id: EXPERT_VRAM_MIB_PARAMETER_ID.to_owned(),
                    architecture: "qwen35moe".to_owned(),
                    mtp_draft_tokens: 2,
                    model_files: Some(ModelFiles::SingleGguf(StrataSingleGguf {})),
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

/// Returns a valid Strata configuration that packs the fixture's split GGUF set and drafts with
/// its draft layer.
pub(crate) fn split_gguf_configuration() -> ModelEngineConfiguration {
    pack_configuration(
        "fixture-split-gguf",
        ModelFiles::SplitGguf(StrataSplitGguf {
            draft_layer_auxiliary_artifact_id: DRAFT_LAYER_ARTIFACT_ID.to_owned(),
        }),
    )
}

/// Returns a valid Strata configuration that runs the fixture's ready-made pack and drafts with
/// its draft layer.
pub(crate) fn ready_pack_configuration() -> ModelEngineConfiguration {
    pack_configuration(
        "fixture-ready-pack",
        ModelFiles::ReadyPack(StrataReadyPack {
            draft_layer_auxiliary_artifact_id: DRAFT_LAYER_ARTIFACT_ID.to_owned(),
            pack_path: "pack".to_owned(),
            dense_gguf_path: "dense.gguf".to_owned(),
            lookup_table_gguf_path: "lookup-table.gguf".to_owned(),
            token_embedding_gguf_path: "token-embedding.gguf".to_owned(),
        }),
    )
}

/// Returns [`configuration`] binding `artifact_id` with the draft layer, for the pack path.
fn pack_configuration(artifact_id: &str, model_files: ModelFiles) -> ModelEngineConfiguration {
    let mut configuration = configuration();
    configuration.configuration_id = format!("strata-{artifact_id}");
    configuration.artifact_bindings = vec![ModelArtifactBinding {
        binding_id: artifact_id.to_owned(),
        model_artifact_id: artifact_id.to_owned(),
        auxiliary_artifact_ids: vec![DRAFT_LAYER_ARTIFACT_ID.to_owned()],
    }];
    configuration.engine_configuration = Some(EngineModelConfiguration {
        engine_kind: EngineKind::Strata as i32,
        configuration: Some(Configuration::Strata(StrataModelConfiguration {
            fixed_settings: Some(StrataFixedModelSettings {
                expert_vram_mib_parameter_id: EXPERT_VRAM_MIB_PARAMETER_ID.to_owned(),
                architecture: "qwen4exp".to_owned(),
                mtp_draft_tokens: 4,
                model_files: Some(model_files),
            }),
        })),
    });
    configuration
}

/// Returns a pinned snapshot source of `paths`.
fn snapshot_source<const N: usize>(paths: [&str; N]) -> HuggingFaceArtifactSource {
    HuggingFaceArtifactSource {
        repository: "dotcore/fixture-strata".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        content: Some(hugging_face_artifact_source::Content::Snapshot(
            HuggingFaceSnapshotArtifact {
                files: paths
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
    }
}

/// The catalog declaration of [`release`], for tests that load a catalog.
pub(crate) const CATALOG_ENGINE: &str = r#"
engineReleaseId: "fixture-strata"
engineKind: "ENGINE_KIND_STRATA"
version: "1"
parameterSchema:
  schemaId: "fixture-strata-v1"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", visibility: "PARAMETER_VISIBILITY_BASIC", description: "total-context", unsignedInteger: { minimum: "1024", maximum: "2097152", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", visibility: "PARAMETER_VISIBILITY_BASIC", description: "context-per-request", unsignedInteger: { minimum: "1024", maximum: "262144", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", visibility: "PARAMETER_VISIBILITY_ADVANCED", description: "parallel-requests", unsignedInteger: { minimum: "1", maximum: "8", unit: "requests", defaultValue: "1" } }
    - { parameterId: "expert-vram-mib", displayName: "Expert accelerator memory", visibility: "PARAMETER_VISIBILITY_ADVANCED", description: "expert-vram-mib", unsignedInteger: { minimum: "1", maximum: "1048576", unit: "MiB" } }
storageBindings:
  - { bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_CACHE", placements: ["STORAGE_PLACEMENT_DEPLOYMENT"] }
artifacts:
  - artifactId: "linux-amd64-cuda"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "strata" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

/// The catalog declaration of a profile-shaped model for [`CATALOG_ENGINE`]: profile `strata-q4`,
/// whose sizing derives the expert cap.
pub(crate) const CATALOG_MODEL: &str = r#"
modelId: "fixture-strata-model"
displayName: "Fixture Strata model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "fixture-strata-artifact"
    weightQuantization: "Q4_K_XL"
    source:
      repository: "dotcore/fixture-strata"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture-Q4_K_XL.gguf"
        provenanceDigest: "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
configurations:
  - configurationId: "strata-nvidia"
    engineArtifacts: [{ engineReleaseId: "fixture-strata", engineArtifactId: "linux-amd64-cuda" }]
    artifactBindings: [{ bindingId: "fixture-strata-artifact", modelArtifactId: "fixture-strata-artifact" }]
    sizing:
      parameters: { baseBytes: 1000, bytesPerKvToken: 10, expertMib: 20000, expertVramMib: 8192 }
      derivedParameters:
        expert-vram-mib: "${expertVramMib}"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${bytesPerKvToken} * ${total-context} + ${expert-vram-mib} * MiB"
        hostMemory:
          unit: bytes
          equation: "${expertMib} * MiB"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        hardwareRequirements:
          minimumMemoryBytesPerDevice: "32000000000"
          minimumSystemMemoryBytes: "64000000000"
        bindingSizing:
          - bindingId: "fixture-strata-artifact"
    engineConfiguration:
      engineKind: "ENGINE_KIND_STRATA"
      strata:
        fixedSettings:
          expertVramMibParameterId: "expert-vram-mib"
          architecture: "qwen35moe"
          mtpDraftTokens: 2
          singleGguf: {}
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
profiles:
  - profileId: "strata-q4"
    displayName: "Q4 Strata"
    description: "Runs the fixture through Strata."
    configurationId: "strata-nvidia"
    defaultModelArtifactBindingId: "fixture-strata-artifact"
    engineParameterDefaults: {}
    "#;
