//! Model deployment snapshot fixtures for backend unit tests: a complete local snapshot the site
//! and the model owner both accept.

use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, EngineParameterDefinition, EngineParameterSchema, EngineParameterValue,
        EngineParameterValues, HardwareRequirements, HuggingFaceArtifactSource,
        HuggingFaceFileArtifact, InferenceRequestKind, ModelArtifact, ModelExecutionCapabilities,
        ModelInputCapabilities, ModelInputModality, ModelParallelShape, OciPlatform, Selector,
        UnsignedIntegerEngineParameterSchema, engine_parameter_definition, engine_parameter_value,
        hugging_face_artifact_source,
    },
    deployment::v1::ArtifactPreparationPolicy,
    engine::{
        llama_cpp::v1::{LlamaCppFixedModelSettings, LlamaCppResolvedModelState},
        v1::{EngineKind, ResolvedEngineModelState, resolved_engine_model_state},
    },
    model::v1::{
        LocalModelDeploymentConfiguration, ModelDeploymentSnapshot, PortableModelArtifactBinding,
        PortableModelLaunchArgumentToken, PortableModelLaunchPlan, ResolvedEngineArtifact,
        ResolvedHardwareSupport, ResolvedLocalModelDeploymentSnapshot, model_deployment_snapshot,
        portable_model_artifact_binding, portable_model_launch_argument_token,
    },
};

/// A complete local llama.cpp model deployment snapshot with one Hugging Face weight file, an
/// immutable remote engine image and a launch plan, for tests that need a valid local snapshot.
pub fn local_model_snapshot(model_id: &str) -> ModelDeploymentSnapshot {
    let model_artifact = ModelArtifact {
        artifact_id: "fixture-artifact".to_owned(),
        weight_quantization: "Q8_0".to_owned(),
        source: Some(HuggingFaceArtifactSource {
            repository: "dotcore/fixture".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            content: Some(hugging_face_artifact_source::Content::File(
                HuggingFaceFileArtifact {
                    path: "fixture-Q8_0.gguf".to_owned(),
                    provenance_digest:
                        "f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d"
                            .to_owned(),
                },
            )),
        }),
    };
    let binding = PortableModelArtifactBinding {
        binding_id: "model".to_owned(),
        artifact: Some(portable_model_artifact_binding::Artifact::ModelArtifactId(
            model_artifact.artifact_id.clone(),
        )),
        read_only: true,
    };

    ModelDeploymentSnapshot {
        source: Some(model_deployment_snapshot::Source::Local(
            ResolvedLocalModelDeploymentSnapshot {
                model_id: model_id.to_owned(),
                model_display_name: "Fixture Model".to_owned(),
                model_version: "1".to_owned(),
                configuration_id: "fixture-configuration".to_owned(),
                profile_id: "fixture-profile".to_owned(),
                engine_artifact: Some(ResolvedEngineArtifact {
                    engine_release_id: "fixture-engine".to_owned(),
                    engine_kind: EngineKind::LlamaCpp as i32,
                    engine_version: "1".to_owned(),
                    artifact: Some(EngineArtifact {
                        selectors: Vec::new(),
                        ..super::oci::engine_artifact(
                            "fixture-linux",
                            Some("registry.dotcore.test"),
                            "fixture",
                            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                            "linux",
                            "amd64",
                            "nvidia",
                        )
                    }),
                    platform: Some(OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    }),
                    accelerator: "nvidia".to_owned(),
                    minimum_nvidia_driver_major_version: 580,
                    preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                    launch_identity: "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                    storage_bindings: Vec::new(),
                }),
                model_artifact: Some(model_artifact),
                auxiliary_artifacts: Vec::new(),
                hardware_support: Some(ResolvedHardwareSupport {
                    hardware_class_id: "nvidia".to_owned(),
                    selector: Some(Selector {
                        platform: None,
                        accelerators: vec!["nvidia".to_owned()],
                        minimum_nvidia_driver_major_version: 0,
                        ..Default::default()
                    }),
                    hardware_requirements: Some(HardwareRequirements {
                        minimum_memory_bytes_per_device: 1,
                        minimum_system_memory_bytes: 1,
                    }),
                }),
                resolved_engine_state: Some(ResolvedEngineModelState {
                    engine_kind: EngineKind::LlamaCpp as i32,
                    semantics_version: 2,
                    state: Some(resolved_engine_model_state::State::LlamaCpp(
                        LlamaCppResolvedModelState {
                            fixed_settings: Some(LlamaCppFixedModelSettings {
                                gpu_layers: 0,
                                flash_attention: false,
                                speculative: None,
                                vision_projector: None,
                                jinja: false,
                                sampling: None,
                                single_micro_batch: false,
                            }),
                            option_realizations: Vec::new(),
                        },
                    )),
                }),
                execution_capabilities: Some(ModelExecutionCapabilities {
                    supported_parallel_shapes: vec![ModelParallelShape::One as i32],
                }),
                input_capabilities: Some(ModelInputCapabilities {
                    supported_modalities: vec![ModelInputModality::Text as i32],
                    image_input_capacity: None,
                }),
                engine_parameter_values: Some(unsigned_values(&[
                    ("prompt-cache-memory-mib", 0),
                    ("total-context", 32_768),
                    ("parallel-requests", 4),
                    ("context-per-request", 32_768),
                ])),
                engine_parameter_schema_id: "fixture-context-v1".to_owned(),
                engine_parameter_schema: Some(EngineParameterSchema {
                    schema_id: "fixture-context-v1".to_owned(),
                    parameters: vec![
                        unsigned_parameter(
                            "total-context",
                            "Total context",
                            1024,
                            16_777_216,
                            "tokens",
                        ),
                        unsigned_parameter(
                            "context-per-request",
                            "Context per request",
                            1024,
                            16_777_216,
                            "tokens",
                        ),
                        unsigned_parameter("parallel-requests", "Parallel requests", 1, 64, "requests"),
                        unsigned_parameter(
                            "prompt-cache-memory-mib",
                            "Prompt cache memory",
                            0,
                            65_536,
                            "MiB",
                        ),
                    ],
                }),
                custom_argument_tokens: Vec::new(),
                resolved_options: Vec::new(),
                selected_target: Some(dotcore_proto::dotcore::model::v1::ResolvedSelectedTarget {
                    accelerator_memory_architecture:
                        dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated as i32,
                    accelerator_memory_pool_total_bytes: Some(34_190_917_632),
                }),
                sizing: None,
                desired_launch_plan: Some(PortableModelLaunchPlan {
                    arguments: vec![
                        token(portable_model_launch_argument_token::Value::EngineExecutable(
                            dotcore_proto::google::protobuf::Empty {},
                        )),
                        literal("--model"),
                        artifact_binding("model"),
                        literal("--alias"),
                        literal(model_id),
                        literal("--host"),
                        token(portable_model_launch_argument_token::Value::ServiceHost(
                            "http".to_owned(),
                        )),
                        literal("--port"),
                        token(portable_model_launch_argument_token::Value::ServicePort(
                            "http".to_owned(),
                        )),
                        literal("--ctx-size"),
                        literal("32768"),
                        literal("--n-gpu-layers"),
                        literal("0"),
                        literal("--cache-ram"),
                        literal("0"),
                        literal("-np"),
                        literal("4"),
                        literal("--kv-unified"),
                    ],
                    format_version: 4,
                    artifact_bindings: vec![binding],
                }),
                request_kinds: vec![InferenceRequestKind::Chat as i32],
            },
        )),
    }
}

/// The local model deployment configuration that [`local_model_snapshot`] resolves.
pub fn local_model_configuration() -> LocalModelDeploymentConfiguration {
    LocalModelDeploymentConfiguration {
        model_artifact_binding_id: "fixture-artifact".to_owned(),
        engine_parameter_values: Some(unsigned_values(&[
            ("prompt-cache-memory-mib", 0),
            ("total-context", 32_768),
            ("parallel-requests", 4),
        ])),
        custom_argument_tokens: Vec::new(),
        option_overrides: Vec::new(),
    }
}

fn unsigned_values(values: &[(&str, u64)]) -> EngineParameterValues {
    EngineParameterValues {
        values: values
            .iter()
            .map(|(parameter_id, value)| EngineParameterValue {
                parameter_id: (*parameter_id).to_owned(),
                value: Some(engine_parameter_value::Value::UnsignedInteger(*value)),
            })
            .collect(),
    }
}

fn literal(value: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(portable_model_launch_argument_token::Value::Literal(
            value.to_owned(),
        )),
    }
}

fn token(value: portable_model_launch_argument_token::Value) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken { value: Some(value) }
}

fn artifact_binding(binding_id: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(
            portable_model_launch_argument_token::Value::ArtifactBindingId(binding_id.to_owned()),
        ),
    }
}

fn unsigned_parameter(
    parameter_id: &str,
    display_name: &str,
    minimum: u64,
    maximum: u64,
    unit: &str,
) -> EngineParameterDefinition {
    EngineParameterDefinition {
        parameter_id: parameter_id.to_owned(),
        display_name: display_name.to_owned(),
        parameter_type: Some(engine_parameter_definition::ParameterType::UnsignedInteger(
            UnsignedIntegerEngineParameterSchema {
                minimum,
                maximum,
                unit: unit.to_owned(),
                default_value: None,
            },
        )),
        exposed: true,
        description: String::new(),
    }
}
