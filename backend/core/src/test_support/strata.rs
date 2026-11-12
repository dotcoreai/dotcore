//! In-code Strata fixtures for backend unit tests, shaped like the Qwen3.6 35B-A3B Strata profile.
//!
//! They provide a text-only model with one GGUF artifact, one valid Strata configuration, and an
//! accepted deployment snapshot whose launch plan is the canonical plan for the profile: a
//! 32,768-token total context and context per request for one parallel request, and an 8 GiB
//! expert accelerator memory cap.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterDefinition, EngineParameterSchema, EngineParameterValue,
        EngineParameterValues, EngineRelease, HardwareRequirements, HardwareSupport,
        HuggingFaceArtifactSource, HuggingFaceFileArtifact, InferenceRequestKind, Model,
        ModelArtifact, ModelArtifactBinding, ModelEngineConfiguration, ModelExecutionCapabilities,
        ModelInputCapabilities, ModelInputModality, ModelParallelShape, StorageBinding,
        StorageKind, StorageScope, UnsignedIntegerEngineParameterSchema,
        engine_parameter_definition, engine_parameter_value, hugging_face_artifact_source,
    },
    engine::{
        strata::v1::{StrataFixedModelSettings, StrataModelConfiguration},
        v1::{EngineKind, EngineModelConfiguration, engine_model_configuration::Configuration},
    },
    model::v1::{
        ModelDeploymentSnapshot, ResolvedLocalModelDeploymentSnapshot, model_deployment_snapshot,
    },
};

/// Engine parameter holding the accelerator memory cap of the model's experts, in MiB.
pub(crate) const EXPERT_VRAM_MIB_PARAMETER_ID: &str = "expert-vram-mib";

/// Resolved engine parameter values of the profile: the engine's default total context, the
/// context per request filled from it, one parallel request and the expert cap from sizing.
const PROFILE_PARAMETERS: [(&str, u64); 4] = [
    (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
    (PARALLEL_REQUESTS_PARAMETER_ID, 1),
    (EXPERT_VRAM_MIB_PARAMETER_ID, 8_192),
];

/// Returns an accepted Strata snapshot for the profile values, serving `model_id`.
pub(crate) fn local_model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    let model = model();
    let configuration = configuration();
    let selection = crate::model::engine::strata::realize_model(
        &model,
        &configuration,
        &configuration.artifact_bindings[0],
        &[],
    )
    .expect("Strata fixture realization");

    let mut snapshot = super::local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("local fixture snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("engine artifact");
    engine.engine_release_id = "fixture-strata".to_owned();
    engine.engine_kind = EngineKind::Strata as i32;
    local.model_artifact = Some(model.artifacts[0].clone());
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

/// Recomputes the canonical launch plan after a test edits a Strata snapshot.
pub(crate) fn refresh_launch_plan(local: &mut ResolvedLocalModelDeploymentSnapshot) {
    local.desired_launch_plan =
        Some(crate::model::engine::strata::translate(local).expect("Strata fixture plan"));
}

/// Returns Strata engine parameter values for the given pairs.
pub(crate) fn parameter_values<const N: usize>(values: [(&str, u64); N]) -> EngineParameterValues {
    EngineParameterValues {
        values: values
            .into_iter()
            .map(|(parameter_id, value)| EngineParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
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
                    EngineParameterDefinition {
                        parameter_id: parameter_id.to_owned(),
                        display_name: display_name.to_owned(),
                        parameter_type: Some(
                            engine_parameter_definition::ParameterType::UnsignedInteger(
                                UnsignedIntegerEngineParameterSchema {
                                    minimum,
                                    maximum,
                                    unit: unit.to_owned(),
                                    default_value,
                                },
                            ),
                        ),
                        exposed: true,
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
            scope: StorageScope::Deployment as i32,
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

/// Returns a model with one GGUF artifact and no options.
pub(crate) fn model() -> Model {
    Model {
        model_id: "fixture-strata-model".to_owned(),
        artifacts: vec![ModelArtifact {
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
