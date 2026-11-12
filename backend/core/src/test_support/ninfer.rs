//! In-code ninfer fixtures for backend unit tests, shaped like the Qwen3.8 27B ninfer profile.
//!
//! They provide a model with vision and MTP speculative-decoding options, one valid ninfer
//! configuration, and an accepted deployment snapshot whose launch plan is the canonical plan for
//! the profile at engine defaults (a 32,768-token total context and context per request, 4
//! parallel requests, MTP with 3 draft tokens).

use dotcore_catalog::{
    options::SPECULATIVE_TOKEN_COUNT_PARAMETER_ID,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID,
    },
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        BooleanModelOptionParameterSchema, EngineParameterSchema, EngineRelease,
        HardwareRequirements, HardwareSupport, HuggingFaceArtifactSource, HuggingFaceFileArtifact,
        InferenceRequestKind, Model, ModelArtifact, ModelArtifactBinding, ModelConfigurationOption,
        ModelEngineConfiguration, ModelExecutionCapabilities, ModelImageDetail,
        ModelImageInputCapacity, ModelImageMediaType, ModelInputCapabilities, ModelInputModality,
        ModelOption, ModelOptionParameterDefinition, ModelOptionParameterValue, ModelOptionValues,
        ModelParallelShape, ParameterDefinition, ParameterValue, ParameterValues,
        ParameterVisibility, SpeculativeDecodingMethod, SpeculativeDecodingModelOption,
        UnsignedIntegerModelOptionParameterSchema, UnsignedIntegerParameterSchema,
        VisionModelOption, hugging_face_artifact_source, model_option,
        model_option_parameter_definition, model_option_parameter_value, parameter_definition,
        parameter_value,
    },
    engine::{
        ninfer::v1::{
            NinferFixedModelSettings, NinferKvCacheDataType, NinferModelConfiguration,
            NinferMtpOptionRealization, NinferOptionRealization, NinferVisionOptionRealization,
            ninfer_option_realization::Realization,
        },
        v1::{EngineKind, EngineModelConfiguration, engine_model_configuration::Configuration},
    },
    model::v1::{
        ModelDeploymentSnapshot, ResolvedLocalModelDeploymentSnapshot, ResolvedModelOption,
        model_deployment_snapshot,
    },
};

/// Resolved engine parameter values of the Qwen3.8 27B ninfer profile: the engine's default total
/// context, the context per request filled from it, and the profile's parallel requests and host
/// context.
const PROFILE_PARAMETERS: [(&str, u64); 4] = [
    (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
    (PARALLEL_REQUESTS_PARAMETER_ID, 4),
    ("host-context-mib", 20_480),
];

/// Returns an accepted ninfer snapshot for the profile defaults with vision on and MTP at a nonzero token count.
pub(crate) fn local_model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    local_model_snapshot_with(model_id, true, 3)
}

/// Returns an accepted ninfer snapshot for the profile parameters and the given option values.
/// A speculative-token count of 0 turns MTP off.
pub(crate) fn local_model_snapshot_with(
    model_id: &str,
    vision: bool,
    token_count: u64,
) -> ModelDeploymentSnapshot {
    let model = model();
    let configuration = configuration();
    let option_values = vec![
        option_values("vision", [("enabled", boolean(vision))]),
        option_values(
            "mtp",
            [(SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, unsigned(token_count))],
        ),
    ];
    let selection = crate::model::engine::ninfer::realize_model(
        &model,
        &configuration,
        &configuration.artifact_bindings[0],
        &option_values,
    )
    .expect("ninfer fixture realization");

    let mut snapshot = super::local_model_snapshot(model_id);
    let Some(model_deployment_snapshot::Source::Local(local)) = snapshot.source.as_mut() else {
        panic!("local fixture snapshot");
    };
    let engine = local.engine_artifact.as_mut().expect("engine artifact");
    engine.engine_release_id = "fixture-ninfer".to_owned();
    engine.engine_kind = EngineKind::Ninfer as i32;
    local.model_artifact = Some(model.artifacts[0].clone());
    local
        .hardware_support
        .as_mut()
        .expect("hardware support")
        .hardware_requirements = configuration.hardware_support[0].hardware_requirements;
    local.resolved_engine_state = Some(selection.state);
    local.input_capabilities = Some(selection.input_capabilities);
    local.engine_parameter_schema = release().parameter_schema;
    local.engine_parameter_schema_id = "fixture-ninfer-v2".to_owned();
    local.engine_parameter_values = Some(parameter_values(PROFILE_PARAMETERS));
    local.resolved_options = option_values
        .into_iter()
        .map(|values| ResolvedModelOption {
            values: Some(values),
        })
        .collect();
    local.desired_launch_plan = None;
    refresh_launch_plan(local);
    snapshot
}

/// Recomputes the canonical launch plan after a test edits a ninfer snapshot.
pub(crate) fn refresh_launch_plan(local: &mut ResolvedLocalModelDeploymentSnapshot) {
    local.desired_launch_plan =
        Some(crate::model::engine::ninfer::translate(local).expect("ninfer fixture plan"));
}

/// Returns ninfer engine parameter values for the given pairs.
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

/// Returns a ninfer engine release with the shared context parameters at ninfer's limits and
/// engine defaults, then the host context.
pub(crate) fn release() -> EngineRelease {
    EngineRelease {
        engine_release_id: "fixture-ninfer".to_owned(),
        engine_kind: EngineKind::Ninfer as i32,
        parameter_schema: Some(EngineParameterSchema {
            schema_id: "fixture-ninfer-v2".to_owned(),
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
                    Some(4),
                ),
                (
                    "host-context-mib",
                    "Host context memory",
                    0,
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
        artifacts: vec![dotcore_domain::test_support::oci::engine_artifact(
            "linux-amd64-cuda",
            Some("registry.dotcore.test"),
            "ninfer",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "linux",
            "amd64",
            "nvidia",
        )],
        ..Default::default()
    }
}

/// Returns a model with one `.ninfer` artifact and vision and MTP speculative-decoding options.
pub(crate) fn model() -> Model {
    Model {
        model_id: "fixture-ninfer-model".to_owned(),
        options: vec![
            ModelOption {
                option_id: "vision".to_owned(),
                display_name: "Vision".to_owned(),
                kind: Some(model_option::Kind::Vision(VisionModelOption {})),
            },
            ModelOption {
                option_id: "mtp".to_owned(),
                display_name: "MTP".to_owned(),
                kind: Some(model_option::Kind::SpeculativeDecoding(
                    SpeculativeDecodingModelOption {
                        method: SpeculativeDecodingMethod::Mtp as i32,
                    },
                )),
            },
        ],
        artifacts: vec![ModelArtifact {
            artifact_id: "fixture-ninfer-artifact".to_owned(),
            weight_quantization: "NVFP4".to_owned(),
            source: Some(HuggingFaceArtifactSource {
                repository: "dotcore/fixture-ninfer".to_owned(),
                revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                content: Some(hugging_face_artifact_source::Content::File(
                    HuggingFaceFileArtifact {
                        path: "fixture_nvfp4.ninfer".to_owned(),
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

/// Returns a valid ninfer configuration for [`model`] and [`release`].
pub(crate) fn configuration() -> ModelEngineConfiguration {
    ModelEngineConfiguration {
        configuration_id: "ninfer-nvidia".to_owned(),
        artifact_bindings: vec![ModelArtifactBinding {
            binding_id: "fixture-ninfer-artifact".to_owned(),
            model_artifact_id: "fixture-ninfer-artifact".to_owned(),
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
            engine_kind: EngineKind::Ninfer as i32,
            configuration: Some(Configuration::Ninfer(NinferModelConfiguration {
                fixed_settings: Some(NinferFixedModelSettings {
                    kv_cache_data_type: NinferKvCacheDataType::Fp8 as i32,
                    pending_timeout_milliseconds: 120_000,
                    preserve_thinking: true,
                    host_context_mib_parameter_id: "host-context-mib".to_owned(),
                    state_image_bytes: 153_954_304,
                    kv_bytes_per_token: 33_024,
                    draft_kv_bytes_per_token: 2_064,
                }),
                option_realizations: vec![
                    NinferOptionRealization {
                        option_id: "vision".to_owned(),
                        realization: Some(Realization::Vision(NinferVisionOptionRealization {
                            enabled_parameter_id: "enabled".to_owned(),
                        })),
                    },
                    NinferOptionRealization {
                        option_id: "mtp".to_owned(),
                        realization: Some(Realization::Mtp(NinferMtpOptionRealization {
                            optimized_proposal_head: true,
                        })),
                    },
                ],
            })),
        }),
        execution_capabilities: Some(ModelExecutionCapabilities {
            supported_parallel_shapes: vec![ModelParallelShape::One as i32],
        }),
        request_kinds: vec![InferenceRequestKind::Chat as i32],
        input_capabilities: Some(ModelInputCapabilities {
            supported_modalities: vec![
                ModelInputModality::Text as i32,
                ModelInputModality::Image as i32,
            ],
            image_input_capacity: Some(ModelImageInputCapacity {
                maximum_image_count: 4,
                supported_media_types: vec![
                    ModelImageMediaType::Jpeg as i32,
                    ModelImageMediaType::Png as i32,
                ],
                maximum_encoded_bytes: 20_971_520,
                maximum_decoded_bytes: 67_108_864,
                maximum_width: 8192,
                maximum_height: 8192,
                maximum_pixels: 16_000_000,
                fixed_visual_tokens: 256,
                fixed_detail: ModelImageDetail::Auto as i32,
            }),
        }),
        options: vec![
            configuration_option(
                "vision",
                vec![boolean_parameter("enabled")],
                [("enabled", boolean(true))],
            ),
            configuration_option(
                "mtp",
                vec![ModelOptionParameterDefinition {
                    parameter_id: SPECULATIVE_TOKEN_COUNT_PARAMETER_ID.to_owned(),
                    display_name: "Speculative tokens".to_owned(),
                    parameter_type: Some(
                        model_option_parameter_definition::ParameterType::UnsignedInteger(
                            UnsignedIntegerModelOptionParameterSchema {
                                minimum: 0,
                                maximum: 5,
                                unit: "tokens".to_owned(),
                            },
                        ),
                    ),
                }],
                [(SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, unsigned(3))],
            ),
        ],
        ..Default::default()
    }
}

fn configuration_option<const N: usize>(
    option_id: &str,
    parameters: Vec<ModelOptionParameterDefinition>,
    defaults: [(&str, model_option_parameter_value::Value); N],
) -> ModelConfigurationOption {
    ModelConfigurationOption {
        option_id: option_id.to_owned(),
        parameters,
        default_values: Some(option_values(option_id, defaults)),
        ..Default::default()
    }
}

fn boolean_parameter(parameter_id: &str) -> ModelOptionParameterDefinition {
    ModelOptionParameterDefinition {
        parameter_id: parameter_id.to_owned(),
        display_name: parameter_id.to_owned(),
        parameter_type: Some(model_option_parameter_definition::ParameterType::Boolean(
            BooleanModelOptionParameterSchema {},
        )),
    }
}

fn option_values<const N: usize>(
    option_id: &str,
    values: [(&str, model_option_parameter_value::Value); N],
) -> ModelOptionValues {
    ModelOptionValues {
        option_id: option_id.to_owned(),
        values: values
            .into_iter()
            .map(|(parameter_id, value)| ModelOptionParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(value),
            })
            .collect(),
    }
}

fn boolean(value: bool) -> model_option_parameter_value::Value {
    model_option_parameter_value::Value::Boolean(value)
}

fn unsigned(value: u64) -> model_option_parameter_value::Value {
    model_option_parameter_value::Value::UnsignedInteger(value)
}
