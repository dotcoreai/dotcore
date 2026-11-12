//! Model protobuf binding contract tests.
//!
//! These tests pin generated model wire bytes and optional progress presence,
//! preventing changes from silently altering lifecycle clients. Catalog source
//! declarations, controller behavior, transport serving, and site contracts
//! are intentionally excluded and covered by sibling suites.

use dotcore_proto::google::protobuf::FieldMask;
use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        capacity::v1::{
            MemoryPool, MemoryPoolKind, MemoryPoolPreview, MemorySegment, MemorySegmentSource,
            PlacementPreview, memory_segment,
        },
        catalog::v1::{
            ApplicationServiceArtifact, BooleanModelOptionParameterSchema, EngineArtifact,
            EngineArtifactReference, EngineParameterSchema, EngineRelease, Model, ModelArtifact,
            ModelArtifactBinding, ModelAuxiliaryArtifact, ModelConfigurationOption,
            ModelEngineConfiguration, ModelExecutionCapabilities, ModelInputCapabilities,
            ModelInputModality, ModelOption, ModelOptionParameterDefinition,
            ModelOptionParameterValue, ModelOptionValues, ModelParallelShape, ModelProfile,
            OciImageReference, OciPlatform, ParameterDefinition, ParameterValue, ParameterValues,
            ParameterVisibility, Selector, StringEnumModelOptionParameterSchema,
            UnsignedIntegerModelOptionParameterSchema, UnsignedIntegerParameterSchema,
            VisionModelOption, engine_artifact, model_option::Kind as ModelOptionKind,
            model_option_parameter_definition::ParameterType as ModelOptionParameterType,
            model_option_parameter_value::Value as ModelOptionValueKind,
            parameter_definition::ParameterType as EngineParameterType,
            parameter_value::Value as EngineParameterValueKind,
        },
        deployment::v1::{ArtifactPreparationPolicy, MemoryFit},
        engine::{
            llama_cpp::v1::{
                LlamaCppFixedModelSettings, LlamaCppModelConfiguration, LlamaCppOptionRealization,
                LlamaCppResolvedModelState, LlamaCppVisionOptionRealization,
                llama_cpp_option_realization::Realization as LlamaCppOptionRealizationKind,
            },
            v1::{
                EngineKind, EngineModelConfiguration, ResolvedEngineModelState,
                engine_model_configuration::Configuration as EngineConfiguration,
                resolved_engine_model_state::State as ResolvedEngineState,
            },
        },
        model::v1::{
            CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
            EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
            HardwareClassResourceEstimate, ListModelDeploymentsResponse, ModelDeployment,
            ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentMetadata,
            ModelDeploymentProgress, ModelDeploymentProgressUnit, ModelDeploymentSnapshot,
            ModelDeploymentState, ModelDeploymentStatus, PauseModelRequest, PauseModelResponse,
            PortableModelArtifactBinding, PortableModelLaunchArgumentToken,
            PortableModelLaunchPlan, PreviewLocalModelDeploymentRequest,
            PreviewLocalModelDeploymentResponse, ResolvedEngineArtifact, ResolvedHardwareSupport,
            ResolvedLocalModelDeploymentSnapshot, ResolvedModelOption, ResumeModelRequest,
            ResumeModelResponse, ScalarResourceEstimate, UpdateModelDeploymentMetadataRequest,
            UpdateModelDeploymentMetadataResponse,
            model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
            portable_model_artifact_binding::Artifact as PortableModelArtifact,
            portable_model_launch_argument_token::Value as PortableArgumentValue,
        },
    },
    yaml,
};
use prost::Message;
use prost_types::{
    FileDescriptorProto, FileDescriptorSet, field_descriptor_proto::Label as FieldLabel,
    field_descriptor_proto::Type as FieldType,
};

#[test]
fn portable_launch_plan_round_trips_complete_symbolic_arguments() {
    let plan = PortableModelLaunchPlan {
        arguments: vec![
            PortableModelLaunchArgumentToken {
                value: Some(PortableArgumentValue::EngineExecutable(Default::default())),
            },
            literal_argument("--port"),
            PortableModelLaunchArgumentToken {
                value: Some(PortableArgumentValue::ServicePort("http".to_owned())),
            },
            literal_argument("--model"),
            binding_argument("model"),
            literal_argument("--mmproj"),
            binding_argument("vision-projector"),
            literal_argument("--model-draft"),
            binding_argument("mtp-draft"),
            literal_argument("--ctx-size"),
            literal_argument("8192"),
        ],
        format_version: 1,
        artifact_bindings: vec![
            model_artifact_binding("model", "q4-k-m"),
            auxiliary_artifact_binding("vision-projector", "vision-projector"),
            auxiliary_artifact_binding("mtp-draft", "mtp-draft"),
        ],
    };

    assert_eq!(
        render_workload_arguments(&plan),
        [
            "{engine.executable}",
            "--port",
            "{service:http.port}",
            "--model",
            "{artifact:model}",
            "--mmproj",
            "{artifact:vision-projector}",
            "--model-draft",
            "{artifact:mtp-draft}",
            "--ctx-size",
            "8192",
        ]
    );
    assert_eq!(
        PortableModelLaunchPlan::decode(plan.encode_to_vec().as_slice())
            .expect("the portable launch plan fixture must decode"),
        plan
    );
}

#[test]
fn generated_bindings_expose_model_deployment_names_and_optional_create_names() {
    let declaration = ModelDeploymentDeclaration {
        model_deployment_id: "model-01".to_owned(),
        model_deployment_name: "Research Model".to_owned(),
        ..Default::default()
    };
    assert_eq!(
        ModelDeploymentDeclaration::decode(declaration.encode_to_vec().as_slice())
            .expect("the named desired model deployment fixture must decode"),
        declaration
    );

    let deployment = ModelDeployment {
        model_deployment_id: "model-01".to_owned(),
        model_deployment_name: "Research Model".to_owned(),
        ..Default::default()
    };
    assert_eq!(
        ModelDeployment::decode(deployment.encode_to_vec().as_slice())
            .expect("the named public model deployment fixture must decode"),
        deployment
    );

    let omitted_name = CreateModelDeploymentRequest::default();
    let submitted_name = CreateModelDeploymentRequest {
        model_deployment_name: Some("Research Model".to_owned()),
        ..Default::default()
    };
    assert_eq!(omitted_name.model_deployment_name, None);
    assert_eq!(
        CreateModelDeploymentRequest::decode(submitted_name.encode_to_vec().as_slice())
            .expect("the explicitly named model create fixture must decode")
            .model_deployment_name,
        Some("Research Model".to_owned())
    );
}

#[test]
fn generated_bindings_preserve_model_metadata_update_and_accepted_deployment() {
    let request = UpdateModelDeploymentMetadataRequest {
        model_deployment_id: "model-01".to_owned(),
        metadata: Some(ModelDeploymentMetadata {
            display_name: Some("Renamed Model".to_owned()),
        }),
        update_mask: Some(FieldMask {
            paths: vec!["display_name".to_owned()],
        }),
    };
    assert_eq!(
        UpdateModelDeploymentMetadataRequest::decode(request.encode_to_vec().as_slice())
            .expect("the model metadata update fixture must decode"),
        request
    );

    let response = UpdateModelDeploymentMetadataResponse {
        deployment: Some(ModelDeployment {
            model_deployment_id: "model-01".to_owned(),
            model_deployment_name: "Renamed Model".to_owned(),
            ..Default::default()
        }),
    };
    assert_eq!(
        UpdateModelDeploymentMetadataResponse::decode(response.encode_to_vec().as_slice())
            .expect("the model metadata response fixture must decode"),
        response
    );
}

#[test]
fn generated_bindings_preserve_typed_status_and_byte_progress() {
    let known_total = ModelDeploymentState {
        status: ModelDeploymentStatus::Downloading as i32,
        status_message: "Downloading model artifacts".to_owned(),
        progress: Some(ModelDeploymentProgress {
            percent: Some(25),
            completed: 256,
            total: Some(1024),
            unit: ModelDeploymentProgressUnit::Bytes as i32,
        }),
        ..Default::default()
    };
    let unknown_total = ModelDeploymentState {
        status: ModelDeploymentStatus::Downloading as i32,
        status_message: "Downloading model artifacts".to_owned(),
        progress: Some(ModelDeploymentProgress {
            percent: None,
            completed: 256,
            total: None,
            unit: ModelDeploymentProgressUnit::Bytes as i32,
        }),
        ..Default::default()
    };

    assert_eq!(
        ModelDeploymentState::decode(known_total.encode_to_vec().as_slice())
            .expect("the known-total progress fixture must decode"),
        known_total,
    );
    assert_eq!(
        ModelDeploymentState::decode(unknown_total.encode_to_vec().as_slice())
            .expect("the unknown-total progress fixture must decode"),
        unknown_total,
    );
    assert_eq!(ModelDeploymentStatus::Loading as i32, 4);
    assert_eq!(ModelDeploymentStatus::Ready as i32, 6);
    assert_eq!(ModelDeploymentStatus::Downloading as i32, 12);

    let deployments = ListModelDeploymentsResponse::default();
    assert_eq!(
        ListModelDeploymentsResponse::decode(deployments.encode_to_vec().as_slice())
            .expect("the deployment list fixture must decode"),
        deployments,
    );
}

#[test]
fn generated_bindings_preserve_exact_ordered_custom_argument_tokens_in_preview_and_snapshot() {
    let context_values = ParameterValues {
        values: vec![ParameterValue {
            parameter_id: "total-context".to_owned(),
            value: Some(EngineParameterValueKind::UnsignedInteger(30_000)),
        }],
    };
    let preview_request = PreviewLocalModelDeploymentRequest {
        local: Some(CreateLocalModelDeploymentRequest {
            model_id: "gemma-4-e2b".to_owned(),
            profile_id: "fast-start".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "local-node-1".to_owned(),
                execution_target_id: "model-target-1".to_owned(),
            }),
            configuration: Some(
                dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "q4-k-m".to_owned(),
                    engine_parameter_values: Some(context_values.clone()),
                    custom_argument_tokens: vec![
                        "--reasoning-format".to_owned(),
                        "deepseek".to_owned(),
                        "--flag=value".to_owned(),
                        "$(touch /tmp/not-a-shell-command)".to_owned(),
                        " value with spaces ".to_owned(),
                    ],
                    option_overrides: Vec::new(),
                },
            ),
        }),
    };
    assert_eq!(
        PreviewLocalModelDeploymentRequest::decode(preview_request.encode_to_vec().as_slice())
            .expect("the typed local preview request fixture must decode"),
        preview_request
    );

    let resolved = ResolvedLocalModelDeploymentSnapshot {
        model_id: "gemma-4-e2b".to_owned(),
        model_display_name: "Gemma 4 E2B".to_owned(),
        model_version: "4-e2b".to_owned(),
        configuration_id: "cuda".to_owned(),
        profile_id: "fast-start".to_owned(),
        engine_artifact: Some(ResolvedEngineArtifact {
            engine_release_id: "llama-cpp-2026-07-28".to_owned(),
            engine_kind: EngineKind::LlamaCpp as i32,
            engine_version: "b10156".to_owned(),
            artifact: Some(EngineArtifact {
                artifact_id: "cuda13".to_owned(),
                kind: Some(engine_artifact::Kind::Oci(OciImageReference::default())),
                selectors: Vec::new(),
            }),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator: "nvidia".to_owned(),
            minimum_nvidia_driver_major_version: 580,
            preparation_policy: ArtifactPreparationPolicy::LocalDevelopment as i32,
            launch_identity:
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
            storage_bindings: Vec::new(),
        }),
        model_artifact: Some(ModelArtifact {
            artifact_id: "q4-k-m".to_owned(),
            weight_quantization: "Q4_K_M".to_owned(),
            ..Default::default()
        }),
        auxiliary_artifacts: vec![ModelAuxiliaryArtifact {
            auxiliary_artifact_id: "vision-projector".to_owned(),
            ..Default::default()
        }],
        hardware_support: Some(ResolvedHardwareSupport::default()),
        resolved_engine_state: Some(ResolvedEngineModelState {
            engine_kind: EngineKind::LlamaCpp as i32,
            semantics_version: 1,
            state: Some(ResolvedEngineState::LlamaCpp(LlamaCppResolvedModelState {
                fixed_settings: Some(LlamaCppFixedModelSettings {
                    gpu_layers: 99,
                    flash_attention: true,
                    ..Default::default()
                }),
                option_realizations: Vec::new(),
            })),
        }),
        execution_capabilities: Some(ModelExecutionCapabilities {
            supported_parallel_shapes: vec![ModelParallelShape::One as i32],
        }),
        input_capabilities: Some(ModelInputCapabilities {
            supported_modalities: vec![
                ModelInputModality::Text as i32,
                ModelInputModality::Image as i32,
            ],
            ..Default::default()
        }),
        engine_parameter_values: Some(context_values),
        engine_parameter_schema_id: "llama-cpp-b10156-v1".to_owned(),
        engine_parameter_schema: Some(EngineParameterSchema {
            schema_id: "llama-cpp-b10156-v1".to_owned(),
            parameters: vec![ParameterDefinition {
                parameter_id: "total-context".to_owned(),
                display_name: "Total context".to_owned(),
                parameter_type: Some(EngineParameterType::UnsignedInteger(
                    UnsignedIntegerParameterSchema {
                        minimum: 1,
                        maximum: 131_072,
                        unit: "tokens".to_owned(),
                        default_value: None,
                    },
                )),
                visibility: ParameterVisibility::Basic as i32,
                description: "Maximum prompt and generation context.".to_owned(),
            }],
        }),
        custom_argument_tokens: vec![
            "--reasoning-format".to_owned(),
            "deepseek".to_owned(),
            "--flag=value".to_owned(),
            "$(touch /tmp/not-a-shell-command)".to_owned(),
            " value with spaces ".to_owned(),
        ],
        resolved_options: Vec::new(),
        selected_target: Some(dotcore_proto::dotcore::model::v1::ResolvedSelectedTarget {
            accelerator_memory_architecture:
                dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated
                    as i32,
            accelerator_memory_pool_total_bytes: Some(34_190_917_632),
        }),
        sizing: Some(dotcore_proto::dotcore::model::v1::ResolvedSizing {
            accelerator_memory_bytes: Some(3_634_770_000),
            derived_engine_parameter_ids: vec!["kv-cache-memory-bytes".to_owned()],
            host_memory_bytes: Some(1_073_741_824),
        }),
        desired_launch_plan: Some(PortableModelLaunchPlan {
            arguments: vec![
                literal_argument("--model"),
                binding_argument("model"),
                literal_argument("--mmproj"),
                binding_argument("vision-projector"),
                literal_argument("--ctx-size"),
                literal_argument("30000"),
                literal_argument("--flash-attn"),
                literal_argument("on"),
            ],
            format_version: 1,
            artifact_bindings: vec![
                model_artifact_binding("model", "q4-k-m"),
                auxiliary_artifact_binding("vision-projector", "vision-projector"),
            ],
        }),
        request_kinds: Vec::new(),
    };
    let response = PreviewLocalModelDeploymentResponse {
        resolved_snapshot: Some(resolved.clone()),
        advisories: vec![
            dotcore_proto::dotcore::model::v1::LocalModelDeploymentAdvisory::HostKvOffloadSharesMemory
                as i32,
        ],
    };
    assert_eq!(
        PreviewLocalModelDeploymentResponse::decode(response.encode_to_vec().as_slice())
            .expect("the resolved local preview fixture must decode"),
        response
    );

    let snapshot = ModelDeploymentSnapshot {
        source: Some(ModelDeploymentSnapshotSource::Local(resolved)),
    };
    let decoded = ModelDeploymentSnapshot::decode(snapshot.encode_to_vec().as_slice())
        .expect("the resolved local deployment snapshot fixture must decode");
    assert_eq!(decoded, snapshot);
    let Some(ModelDeploymentSnapshotSource::Local(decoded)) = decoded.source else {
        panic!("decoded local snapshot");
    };
    let engine = decoded.engine_artifact.expect("decoded engine artifact");
    assert_eq!(
        engine.preparation_policy(),
        ArtifactPreparationPolicy::LocalDevelopment
    );
    assert_eq!(
        engine.launch_identity,
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    );
}

#[test]
fn generated_bindings_preserve_minimal_generic_resource_estimates() {
    let configuration = dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
        model_artifact_binding_id: "q4-k-m".to_owned(),
        engine_parameter_values: Some(ParameterValues {
            values: vec![ParameterValue {
                parameter_id: "total-context".to_owned(),
                value: Some(EngineParameterValueKind::UnsignedInteger(30_000)),
            }],
        }),
        custom_argument_tokens: Vec::new(),
        option_overrides: Vec::new(),
    };
    let request = EstimateLocalModelResourcesRequest {
        model_id: "gemma-4-e2b".to_owned(),
        profile_id: "fast-start".to_owned(),
        configuration: Some(configuration),
        placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "node-a".to_owned(),
            execution_target_id: "gpu-0".to_owned(),
        }),
    };
    assert_eq!(
        EstimateLocalModelResourcesRequest::decode(request.encode_to_vec().as_slice())
            .expect("resource estimate request fixture must decode"),
        request
    );

    let response = EstimateLocalModelResourcesResponse {
        hardware_class_estimates: vec![HardwareClassResourceEstimate {
            hardware_class_id: "nvidia-dedicated".to_owned(),
            estimates: vec![
                ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 3_634_770_000,
                    unit: "bytes".to_owned(),
                },
                ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 1_200_000_000,
                    unit: "bytes".to_owned(),
                },
            ],
            derived_engine_parameter_values: Some(ParameterValues {
                values: vec![ParameterValue {
                    parameter_id: "kv-cache-memory-bytes".to_owned(),
                    value: Some(EngineParameterValueKind::UnsignedInteger(5_443_884_300)),
                }],
            }),
            launch_rejection: Some("lower the total context".to_owned()),
        }],
        placement_preview: Some(PlacementPreview {
            pools: vec![
                MemoryPoolPreview {
                    pool: Some(MemoryPool {
                        kind: MemoryPoolKind::Device as i32,
                        device_id: "GPU-a".to_owned(),
                        device_name: "NVIDIA GeForce RTX 5090".to_owned(),
                        device_index: Some(0),
                        total_bytes: Some(34_190_917_632),
                        segments: vec![
                            segment(MemorySegmentSource::Measured, "model-a", 2_147_483_648),
                            segment(MemorySegmentSource::Claimed, "model-b", 1_073_741_824),
                            MemorySegment {
                                source: MemorySegmentSource::Measured as i32,
                                owner: Some(memory_segment::Owner::ApplicationDeploymentId(
                                    "app-a".to_owned(),
                                )),
                                bytes: 1_073_741_824,
                            },
                            segment(MemorySegmentSource::Other, "", 521_142_272),
                            segment(MemorySegmentSource::Previewed, "", 3_634_770_000),
                        ],
                        over_total_bytes: 0,
                    }),
                    fit_line_bytes: Some(33_849_008_456),
                    free_memory_fit: MemoryFit::Fits as i32,
                    over_by_bytes: 0,
                    beyond_total: false,
                },
                // An unread host pool keeps its absent total and fit line apart from zero.
                MemoryPoolPreview {
                    pool: Some(MemoryPool {
                        kind: MemoryPoolKind::Host as i32,
                        segments: vec![segment(MemorySegmentSource::Previewed, "", 1_200_000_000)],
                        ..Default::default()
                    }),
                    free_memory_fit: MemoryFit::Unknown as i32,
                    ..Default::default()
                },
            ],
            free_memory_fit: MemoryFit::Unknown as i32,
            observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
        }),
    };
    assert_eq!(
        EstimateLocalModelResourcesResponse::decode(response.encode_to_vec().as_slice())
            .expect("resource estimate response fixture must decode"),
        response
    );
}

#[test]
fn generated_bindings_round_trip_model_configurations_and_typed_context_schema() {
    let schema = EngineParameterSchema {
        schema_id: "llama-cpp-b10156-v1".to_owned(),
        parameters: vec![ParameterDefinition {
            parameter_id: "total-context".to_owned(),
            display_name: "Total context".to_owned(),
            parameter_type: Some(EngineParameterType::UnsignedInteger(
                UnsignedIntegerParameterSchema {
                    minimum: 512,
                    maximum: 131_072,
                    unit: "tokens".to_owned(),
                    default_value: None,
                },
            )),
            visibility: ParameterVisibility::Basic as i32,
            description: "Maximum prompt and generation context.".to_owned(),
        }],
    };
    assert_eq!(
        EngineParameterSchema::decode(schema.encode_to_vec().as_slice())
            .expect("the typed engine parameter schema fixture must decode"),
        schema
    );

    let configuration = ModelEngineConfiguration {
        configuration_id: "cuda".to_owned(),
        engine_artifacts: vec![EngineArtifactReference {
            engine_release_id: "llama-cpp-2026-07-28".to_owned(),
            engine_artifact_id: "cuda13".to_owned(),
        }],
        artifact_bindings: vec![ModelArtifactBinding {
            binding_id: "q4-k-m".to_owned(),
            model_artifact_id: "q4-k-m".to_owned(),
            auxiliary_artifact_ids: vec!["vision-projector".to_owned()],
        }],
        engine_configuration: Some(EngineModelConfiguration {
            engine_kind: EngineKind::LlamaCpp as i32,
            configuration: Some(EngineConfiguration::LlamaCpp(LlamaCppModelConfiguration {
                fixed_settings: Some(LlamaCppFixedModelSettings {
                    gpu_layers: 99,
                    flash_attention: true,
                    ..Default::default()
                }),
                option_realizations: Vec::new(),
            })),
        }),
        ..Default::default()
    };
    let profile = ModelProfile {
        profile_id: "fast-start".to_owned(),
        configuration_id: "cuda".to_owned(),
        default_model_artifact_binding_id: "q4-k-m".to_owned(),
        engine_parameter_defaults: Some(ParameterValues {
            values: vec![ParameterValue {
                parameter_id: "total-context".to_owned(),
                value: Some(EngineParameterValueKind::UnsignedInteger(30_000)),
            }],
        }),
        display_name: "Fast start".to_owned(),
        description: "Balances model quality and startup speed with Q4 weights.".to_owned(),
        option_defaults: Vec::new(),
        exposed_parameter_ids: vec!["total-context".to_owned()],
    };
    let model = Model {
        model_id: "gemma-4-e2b".to_owned(),
        configurations: vec![configuration],
        profiles: vec![profile],
        ..Default::default()
    };
    assert_eq!(
        Model::decode(model.encode_to_vec().as_slice())
            .expect("the model configuration and profile fixture must decode"),
        model
    );
}

#[test]
fn generated_bindings_preserve_typed_option_defaults_overrides_and_resolved_values() {
    let defaults = ModelOptionValues {
        option_id: "vision".to_owned(),
        values: vec![
            ModelOptionParameterValue {
                parameter_id: "enabled".to_owned(),
                value: Some(ModelOptionValueKind::Boolean(false)),
            },
            ModelOptionParameterValue {
                parameter_id: "token-budget".to_owned(),
                value: Some(ModelOptionValueKind::UnsignedInteger(0)),
            },
            ModelOptionParameterValue {
                parameter_id: "projector".to_owned(),
                value: Some(ModelOptionValueKind::String("bf16".to_owned())),
            },
        ],
    };
    let configuration_option = ModelConfigurationOption {
        option_id: "vision".to_owned(),
        parameters: vec![
            ModelOptionParameterDefinition {
                parameter_id: "enabled".to_owned(),
                display_name: "Enabled".to_owned(),
                parameter_type: Some(ModelOptionParameterType::Boolean(
                    BooleanModelOptionParameterSchema {},
                )),
            },
            ModelOptionParameterDefinition {
                parameter_id: "token-budget".to_owned(),
                display_name: "Token budget".to_owned(),
                parameter_type: Some(ModelOptionParameterType::UnsignedInteger(
                    UnsignedIntegerModelOptionParameterSchema {
                        minimum: 0,
                        maximum: 4_096,
                        unit: "tokens".to_owned(),
                    },
                )),
            },
            ModelOptionParameterDefinition {
                parameter_id: "projector".to_owned(),
                display_name: "Projector".to_owned(),
                parameter_type: Some(ModelOptionParameterType::StringEnum(
                    StringEnumModelOptionParameterSchema {
                        values: vec!["bf16".to_owned()],
                    },
                )),
            },
        ],
        default_values: Some(defaults.clone()),
        supported_binding_ids: vec!["q4-k-m".to_owned()],
        compatible_when: "${options.vision.token-budget} <= 4096".to_owned(),
    };
    let minimal_cpu_option = ModelConfigurationOption {
        option_id: "vision".to_owned(),
        parameters: vec![ModelOptionParameterDefinition {
            parameter_id: "enabled".to_owned(),
            display_name: "Enabled".to_owned(),
            parameter_type: Some(ModelOptionParameterType::Boolean(
                BooleanModelOptionParameterSchema {},
            )),
        }],
        default_values: Some(ModelOptionValues {
            option_id: "vision".to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: "enabled".to_owned(),
                value: Some(ModelOptionValueKind::Boolean(false)),
            }],
        }),
        ..Default::default()
    };
    let model = Model {
        model_id: "fixture-model".to_owned(),
        options: vec![ModelOption {
            option_id: "vision".to_owned(),
            display_name: "Vision".to_owned(),
            kind: Some(ModelOptionKind::Vision(VisionModelOption {})),
        }],
        configurations: vec![
            ModelEngineConfiguration {
                configuration_id: "cuda".to_owned(),
                engine_configuration: Some(EngineModelConfiguration {
                    engine_kind: EngineKind::LlamaCpp as i32,
                    configuration: Some(EngineConfiguration::LlamaCpp(
                        LlamaCppModelConfiguration {
                            fixed_settings: Some(LlamaCppFixedModelSettings::default()),
                            option_realizations: vec![LlamaCppOptionRealization {
                                option_id: "vision".to_owned(),
                                realization: Some(LlamaCppOptionRealizationKind::Vision(
                                    LlamaCppVisionOptionRealization {
                                        enabled_parameter_id: "enabled".to_owned(),
                                        auxiliary_artifact_id: "vision-projector".to_owned(),
                                        image_input_capacity: None,
                                    },
                                )),
                            }],
                        },
                    )),
                }),
                options: vec![configuration_option],
                ..Default::default()
            },
            ModelEngineConfiguration {
                configuration_id: "cpu".to_owned(),
                options: vec![minimal_cpu_option],
                ..Default::default()
            },
        ],
        profiles: vec![ModelProfile {
            profile_id: "balanced".to_owned(),
            configuration_id: "cuda".to_owned(),
            option_defaults: vec![defaults.clone()],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        Model::decode(model.encode_to_vec().as_slice()).expect("typed model options must decode"),
        model
    );

    let explicitly_enabled = ModelOptionValues {
        option_id: "vision".to_owned(),
        values: vec![ModelOptionParameterValue {
            parameter_id: "enabled".to_owned(),
            value: Some(ModelOptionValueKind::Boolean(true)),
        }],
    };
    let request_configuration =
        dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration {
            option_overrides: vec![explicitly_enabled.clone()],
            ..Default::default()
        };
    assert_eq!(
        dotcore_proto::dotcore::model::v1::LocalModelDeploymentConfiguration::decode(
            request_configuration.encode_to_vec().as_slice()
        )
        .expect("typed option overrides must decode"),
        request_configuration
    );

    let snapshot = ResolvedLocalModelDeploymentSnapshot {
        resolved_options: vec![ResolvedModelOption {
            values: Some(explicitly_enabled),
        }],
        resolved_engine_state: Some(ResolvedEngineModelState {
            engine_kind: EngineKind::LlamaCpp as i32,
            semantics_version: 1,
            state: Some(ResolvedEngineState::LlamaCpp(LlamaCppResolvedModelState {
                fixed_settings: Some(LlamaCppFixedModelSettings::default()),
                option_realizations: vec![LlamaCppOptionRealization {
                    option_id: "vision".to_owned(),
                    realization: Some(LlamaCppOptionRealizationKind::Vision(
                        LlamaCppVisionOptionRealization {
                            enabled_parameter_id: "enabled".to_owned(),
                            auxiliary_artifact_id: "vision-projector".to_owned(),
                            image_input_capacity: Some(Default::default()),
                        },
                    )),
                }],
            })),
        }),
        ..Default::default()
    };
    assert_eq!(
        ResolvedLocalModelDeploymentSnapshot::decode(snapshot.encode_to_vec().as_slice())
            .expect("resolved option values must decode"),
        snapshot
    );
}

#[test]
fn generated_bindings_preserve_model_pause_values_and_accepted_deployments() {
    let pausing = ModelDeployment {
        desired_state: ModelDeploymentDesiredState::Paused as i32,
        state: Some(ModelDeploymentState {
            status: ModelDeploymentStatus::Pausing as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let paused = ModelDeployment {
        desired_state: ModelDeploymentDesiredState::Paused as i32,
        state: Some(ModelDeploymentState {
            status: ModelDeploymentStatus::Paused as i32,
            ..Default::default()
        }),
        ..Default::default()
    };

    for deployment in [pausing, paused] {
        assert_eq!(
            ModelDeployment::decode(deployment.encode_to_vec().as_slice())
                .expect("the model pause deployment fixture must decode"),
            deployment,
        );
    }

    let pause_response = PauseModelResponse {
        deployment: Some(ModelDeployment {
            desired_state: ModelDeploymentDesiredState::Paused as i32,
            ..Default::default()
        }),
    };
    assert_eq!(
        PauseModelResponse::decode(pause_response.encode_to_vec().as_slice())
            .expect("the model pause response fixture must decode"),
        pause_response,
    );
    let resume_response = ResumeModelResponse {
        deployment: Some(ModelDeployment {
            desired_state: ModelDeploymentDesiredState::Present as i32,
            ..Default::default()
        }),
    };
    assert_eq!(
        ResumeModelResponse::decode(resume_response.encode_to_vec().as_slice())
            .expect("the model resume response fixture must decode"),
        resume_response,
    );

    let pause_request = PauseModelRequest {
        model_deployment_id: "model-01".to_owned(),
    };
    assert_eq!(
        PauseModelRequest::decode(pause_request.encode_to_vec().as_slice())
            .expect("the model pause request fixture must decode"),
        pause_request,
    );
    let resume_request = ResumeModelRequest {
        model_deployment_id: "model-01".to_owned(),
    };
    assert_eq!(
        ResumeModelRequest::decode(resume_request.encode_to_vec().as_slice())
            .expect("the model resume request fixture must decode"),
        resume_request,
    );
}

#[test]
fn multi_platform_engine_contract_uses_explicit_sets_and_rejects_removed_scalar_fields() {
    let engine = yaml::from_str::<EngineRelease>(
        r#"engineReleaseId: fixture-engine
engineKind: ENGINE_KIND_LLAMA_CPP
version: fixture
artifacts:
  - artifactId: cuda13
    oci:
      repository: { name: fixture/engine }
      digest: { algorithm: sha256, encoded: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa }
    selectors:
      - platform: { operatingSystem: linux, architecture: amd64 }
        accelerators: [nvidia]
        minimumNvidiaDriverMajorVersion: 580
      - platform: { operatingSystem: darwin, architecture: arm64 }
        accelerators: [apple]
parameterSchema:
  schemaId: fixture
  parameters:
    - parameterId: total-context
      displayName: Total context
      visibility: "PARAMETER_VISIBILITY_BASIC"
      unsignedInteger: { minimum: "1", maximum: "2", unit: tokens }
"#,
    )
    .expect("multi-platform engine artifact YAML");
    assert_eq!(
        engine.artifacts[0]
            .selectors
            .iter()
            .map(|selector| (
                selector
                    .platform
                    .as_ref()
                    .map(|platform| platform.operating_system.as_str()),
                selector.accelerators.clone(),
                selector.minimum_nvidia_driver_major_version,
            ))
            .collect::<Vec<_>>(),
        [
            (Some("linux"), vec!["nvidia".to_owned()], 580),
            (Some("darwin"), vec!["apple".to_owned()], 0)
        ]
    );

    let model = yaml::from_str::<Model>(
        r#"modelId: fixture-model
displayName: Fixture
version: fixture
configurations:
  - configurationId: local
    engineArtifacts:
      - { engineReleaseId: fixture-engine, engineArtifactId: cuda13 }
      - { engineReleaseId: fixture-engine, engineArtifactId: fallback }
"#,
    )
    .expect("approved engine artifact set YAML");
    assert_eq!(model.configurations[0].engine_artifacts.len(), 2);

    assert!(
        yaml::from_str::<EngineArtifact>("minimumAcceleratorDriverMajorVersion: 580\n").is_err()
    );
    assert!(yaml::from_str::<Selector>("minimumNvidiaDriverMajorVersion: not-a-number\n").is_err());
    assert!(
        yaml::from_str::<ApplicationServiceArtifact>(
            "platform: { operatingSystem: linux, architecture: any }\n"
        )
        .is_err()
    );
    assert!(
        yaml::from_str::<ModelEngineConfiguration>(
            "engineArtifact: { engineReleaseId: fixture, engineArtifactId: old }\n"
        )
        .is_err()
    );
}

#[test]
fn model_descriptors_define_typed_configuration_and_resolved_plan_contracts() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let application = descriptor_file(&descriptor, "dotcore/catalog/v1/application.proto");
    let engine = descriptor_file(&descriptor, "dotcore/catalog/v1/engine_release.proto");
    let parameter = descriptor_file(&descriptor, "dotcore/catalog/v1/parameter.proto");
    let nix = descriptor_file(&descriptor, "dotcore/catalog/v1/nix.proto");
    let selector = descriptor_file(&descriptor, "dotcore/catalog/v1/selector.proto");
    let catalog = descriptor_file(&descriptor, "dotcore/catalog/v1/model.proto");
    let deployment = descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto");
    let service = descriptor_file(&descriptor, "dotcore/model/v1/service.proto");

    assert_eq!(
        field_numbers(application, "ApplicationServiceArtifact"),
        [("oci", 1), ("nix", 3), ("supported_platforms", 2)]
    );
    assert_eq!(
        field_labels(application, "ApplicationServiceArtifact"),
        [
            ("oci", "LABEL_OPTIONAL"),
            ("nix", "LABEL_OPTIONAL"),
            ("supported_platforms", "LABEL_REPEATED"),
        ]
    );
    assert_eq!(
        field_numbers(engine, "EngineRelease"),
        [
            ("engine_release_id", 1),
            ("engine_kind", 2),
            ("version", 3),
            ("artifacts", 4),
            ("parameter_schema", 5),
            ("storage_bindings", 6),
        ]
    );
    assert_eq!(
        field_numbers(engine, "EngineArtifact"),
        [("artifact_id", 1), ("oci", 2), ("nix", 4), ("selectors", 3)]
    );
    assert_eq!(
        field_numbers(nix, "NixArtifact"),
        [
            ("flake", 1),
            ("package", 2),
            ("executable", 3),
            ("docker_image", 5),
            ("store_paths", 4)
        ]
    );
    assert_eq!(field_numbers(nix, "NixDockerImage"), []);
    assert_eq!(
        field_numbers(nix, "NixSystemStorePath"),
        [("system", 1), ("store_path", 2)]
    );
    assert_eq!(
        field_numbers(selector, "Selector"),
        [
            ("platform", 1),
            ("accelerators", 2),
            ("minimum_nvidia_driver_major_version", 3),
            ("memory_architecture", 4),
            ("device_models", 5),
        ]
    );
    assert!(
        engine.enum_type.iter().all(|enumeration| {
            enumeration.name.as_deref() != Some("EngineArtifactQualification")
        }),
        "configuration membership must not expose qualification state"
    );
    assert_eq!(
        field_layout(engine, "EngineParameterSchema"),
        [
            ("schema_id", 1, "TYPE_STRING"),
            ("parameters", 2, ".dotcore.catalog.v1.ParameterDefinition"),
        ]
    );
    assert_eq!(
        field_layout(parameter, "ParameterDefinition"),
        [
            ("parameter_id", 1, "TYPE_STRING"),
            ("display_name", 2, "TYPE_STRING"),
            ("description", 3, "TYPE_STRING"),
            ("visibility", 7, ".dotcore.catalog.v1.ParameterVisibility"),
            ("boolean", 4, ".dotcore.catalog.v1.BooleanParameterSchema"),
            (
                "unsigned_integer",
                5,
                ".dotcore.catalog.v1.UnsignedIntegerParameterSchema"
            ),
            (
                "string_enum",
                6,
                ".dotcore.catalog.v1.StringEnumParameterSchema"
            ),
        ]
    );
    assert_eq!(
        enum_values(parameter, "ParameterVisibility"),
        [
            ("PARAMETER_VISIBILITY_UNSPECIFIED", 0),
            ("PARAMETER_VISIBILITY_BASIC", 1),
            ("PARAMETER_VISIBILITY_ADVANCED", 2),
            ("PARAMETER_VISIBILITY_HIDDEN", 3),
        ]
    );
    assert_eq!(
        field_numbers(parameter, "UnsignedIntegerParameterSchema"),
        [
            ("minimum", 1),
            ("maximum", 2),
            ("unit", 3),
            ("default_value", 4)
        ]
    );
    assert_eq!(
        field_layout(parameter, "ParameterValues"),
        [("values", 1, ".dotcore.catalog.v1.ParameterValue")]
    );
    assert_eq!(
        field_layout(parameter, "ParameterValue"),
        [
            ("parameter_id", 1, "TYPE_STRING"),
            ("boolean", 2, "TYPE_BOOL"),
            ("unsigned_integer", 3, "TYPE_UINT64"),
            ("string", 4, "TYPE_STRING"),
        ]
    );

    assert_eq!(
        field_layout(catalog, "Model"),
        [
            ("model_id", 1, "TYPE_STRING"),
            ("display_name", 2, "TYPE_STRING"),
            ("version", 3, "TYPE_STRING"),
            ("maximum_context_tokens", 12, "TYPE_UINT32"),
            ("artifacts", 4, ".dotcore.catalog.v1.ModelArtifact"),
            ("profiles", 5, ".dotcore.catalog.v1.ModelProfile"),
            (
                "auxiliary_artifacts",
                7,
                ".dotcore.catalog.v1.ModelAuxiliaryArtifact"
            ),
            (
                "configurations",
                8,
                ".dotcore.catalog.v1.ModelEngineConfiguration"
            ),
            ("options", 9, ".dotcore.catalog.v1.ModelOption"),
            ("intelligence", 10, ".dotcore.catalog.v1.IntelligenceScore"),
            ("license", 11, ".dotcore.catalog.v1.License"),
        ]
    );
    assert_eq!(
        field_layout(catalog, "IntelligenceScore"),
        [("overall", 1, "TYPE_DOUBLE")]
    );
    assert_eq!(
        field_layout(catalog, "License"),
        [("id", 1, "TYPE_STRING"), ("terms_url", 2, "TYPE_STRING")]
    );
    assert_eq!(
        field_type_name(catalog, "Model", "intelligence"),
        ".dotcore.catalog.v1.IntelligenceScore"
    );
    assert_eq!(
        field_type_name(catalog, "Model", "license"),
        ".dotcore.catalog.v1.License"
    );
    assert_eq!(
        field_labels(catalog, "Model"),
        [
            ("model_id", "LABEL_OPTIONAL"),
            ("display_name", "LABEL_OPTIONAL"),
            ("version", "LABEL_OPTIONAL"),
            ("maximum_context_tokens", "LABEL_OPTIONAL"),
            ("artifacts", "LABEL_REPEATED"),
            ("profiles", "LABEL_REPEATED"),
            ("auxiliary_artifacts", "LABEL_REPEATED"),
            ("configurations", "LABEL_REPEATED"),
            ("options", "LABEL_REPEATED"),
            ("intelligence", "LABEL_OPTIONAL"),
            ("license", "LABEL_OPTIONAL"),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelEngineConfiguration"),
        [
            ("configuration_id", 1),
            ("engine_artifacts", 2),
            ("artifact_bindings", 3),
            ("hardware_support", 9),
            ("engine_configuration", 5),
            ("execution_capabilities", 6),
            ("input_capabilities", 7),
            ("request_kinds", 11),
            ("options", 8),
            ("maximum_context_tokens", 10),
        ]
    );
    assert_eq!(
        field_labels(catalog, "ModelEngineConfiguration"),
        [
            ("configuration_id", "LABEL_OPTIONAL"),
            ("engine_artifacts", "LABEL_REPEATED"),
            ("artifact_bindings", "LABEL_REPEATED"),
            ("hardware_support", "LABEL_REPEATED"),
            ("engine_configuration", "LABEL_OPTIONAL"),
            ("execution_capabilities", "LABEL_OPTIONAL"),
            ("input_capabilities", "LABEL_OPTIONAL"),
            ("request_kinds", "LABEL_REPEATED"),
            ("options", "LABEL_REPEATED"),
            ("maximum_context_tokens", "LABEL_OPTIONAL"),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelArtifactBinding"),
        [
            ("binding_id", 1),
            ("model_artifact_id", 2),
            ("auxiliary_artifact_ids", 3),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelProfile"),
        [
            ("profile_id", 1),
            ("configuration_id", 2),
            ("default_model_artifact_binding_id", 3),
            ("engine_parameter_defaults", 4),
            ("display_name", 5),
            ("description", 6),
            ("option_defaults", 7),
            ("exposed_parameter_ids", 8),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelOption"),
        [
            ("option_id", 1),
            ("display_name", 2),
            ("vision", 3),
            ("speculative_decoding", 4),
            ("kv_cache_precision", 5),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelConfigurationOption"),
        [
            ("option_id", 1),
            ("parameters", 2),
            ("default_values", 3),
            ("supported_binding_ids", 4),
            ("compatible_when", 5),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelOptionParameterDefinition"),
        [
            ("parameter_id", 1),
            ("display_name", 2),
            ("boolean", 3),
            ("unsigned_integer", 4),
            ("string_enum", 5),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelOptionParameterValue"),
        [
            ("parameter_id", 1),
            ("boolean", 2),
            ("unsigned_integer", 3),
            ("string", 4),
        ]
    );
    assert_eq!(
        field_numbers(catalog, "ModelOptionValues"),
        [("option_id", 1), ("values", 2)]
    );
    assert_eq!(
        field_numbers(service, "CreateModelDeploymentRequest"),
        [
            ("local", 1),
            ("upstream", 2),
            ("exposure", 3),
            ("model_deployment_name", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "CreateLocalModelDeploymentRequest"),
        [
            ("model_id", 1),
            ("profile_id", 2),
            ("placement", 3),
            ("configuration", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "LocalModelDeploymentConfiguration"),
        [
            ("model_artifact_binding_id", 1),
            ("engine_parameter_values", 2),
            ("custom_argument_tokens", 3),
            ("option_overrides", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "PreviewLocalModelDeploymentRequest"),
        [("local", 1)]
    );
    assert_eq!(
        field_numbers(service, "PreviewLocalModelDeploymentResponse"),
        [("resolved_snapshot", 1), ("advisories", 2)]
    );
    assert_eq!(
        field_numbers(service, "EstimateLocalModelResourcesRequest"),
        [
            ("model_id", 1),
            ("profile_id", 2),
            ("configuration", 3),
            ("placement", 4)
        ]
    );
    assert_eq!(
        field_layout(service, "EstimateLocalModelResourcesResponse"),
        [
            (
                "hardware_class_estimates",
                1,
                ".dotcore.model.v1.HardwareClassResourceEstimate"
            ),
            (
                "placement_preview",
                2,
                ".dotcore.capacity.v1.PlacementPreview"
            ),
        ]
    );
    let preview = descriptor_file(&descriptor, "dotcore/capacity/v1/placement_preview.proto");
    assert_eq!(
        field_layout(preview, "PlacementPreview"),
        [
            ("pools", 1, ".dotcore.capacity.v1.MemoryPoolPreview"),
            ("free_memory_fit", 2, ".dotcore.deployment.v1.MemoryFit"),
            ("observed_at", 3, ".google.protobuf.Timestamp"),
        ]
    );
    assert_eq!(
        field_layout(preview, "MemoryPoolPreview"),
        [
            ("pool", 1, ".dotcore.capacity.v1.MemoryPool"),
            ("fit_line_bytes", 2, "TYPE_UINT64"),
            ("free_memory_fit", 3, ".dotcore.deployment.v1.MemoryFit"),
            ("over_by_bytes", 4, "TYPE_UINT64"),
            ("beyond_total", 5, "TYPE_BOOL"),
        ]
    );
    assert!(
        field_is_proto3_optional(preview, "MemoryPoolPreview", "fit_line_bytes"),
        "pool preview fit_line_bytes must preserve scalar presence"
    );
    let memory = descriptor_file(&descriptor, "dotcore/capacity/v1/memory.proto");
    assert_eq!(
        field_layout(memory, "NodeMemory"),
        [
            ("site_node_id", 1, "TYPE_STRING"),
            ("state", 2, ".dotcore.capacity.v1.NodeMemoryState"),
            ("observed_at", 3, ".google.protobuf.Timestamp"),
            ("pools", 4, ".dotcore.capacity.v1.NodeMemoryPool"),
        ]
    );
    assert_eq!(
        field_layout(memory, "NodeMemoryPool"),
        [
            ("pool", 1, ".dotcore.capacity.v1.MemoryPool"),
            ("unsized", 2, ".dotcore.capacity.v1.MemoryOwner"),
        ]
    );
    assert_eq!(
        field_layout(memory, "MemoryPool"),
        [
            ("kind", 1, ".dotcore.capacity.v1.MemoryPoolKind"),
            ("device_id", 2, "TYPE_STRING"),
            ("device_name", 3, "TYPE_STRING"),
            ("device_index", 4, "TYPE_UINT32"),
            ("total_bytes", 5, "TYPE_UINT64"),
            ("segments", 6, ".dotcore.capacity.v1.MemorySegment"),
            ("over_total_bytes", 7, "TYPE_UINT64"),
        ]
    );
    for field in ["device_index", "total_bytes"] {
        assert!(
            field_is_proto3_optional(memory, "MemoryPool", field),
            "memory pool {field} must preserve scalar presence"
        );
    }
    assert_eq!(
        field_layout(memory, "MemorySegment"),
        [
            ("source", 1, ".dotcore.capacity.v1.MemorySegmentSource"),
            ("model_deployment_id", 2, "TYPE_STRING"),
            ("application_deployment_id", 3, "TYPE_STRING"),
            ("bytes", 4, "TYPE_UINT64"),
        ]
    );
    assert_eq!(
        field_layout(memory, "MemoryOwner"),
        [
            ("model_deployment_id", 1, "TYPE_STRING"),
            ("application_deployment_id", 2, "TYPE_STRING"),
        ]
    );
    assert_eq!(
        enum_values(memory, "NodeMemoryState"),
        [
            ("NODE_MEMORY_STATE_UNSPECIFIED", 0),
            ("NODE_MEMORY_STATE_AVAILABLE", 1),
            ("NODE_MEMORY_STATE_STALE", 2),
            ("NODE_MEMORY_STATE_UNAVAILABLE", 3),
        ]
    );
    assert_eq!(
        enum_values(memory, "MemoryPoolKind"),
        [
            ("MEMORY_POOL_KIND_UNSPECIFIED", 0),
            ("MEMORY_POOL_KIND_DEVICE", 1),
            ("MEMORY_POOL_KIND_HOST", 2),
        ]
    );
    assert_eq!(
        enum_values(memory, "MemorySegmentSource"),
        [
            ("MEMORY_SEGMENT_SOURCE_UNSPECIFIED", 0),
            ("MEMORY_SEGMENT_SOURCE_MEASURED", 1),
            ("MEMORY_SEGMENT_SOURCE_ESTIMATED", 2),
            ("MEMORY_SEGMENT_SOURCE_CLAIMED", 3),
            ("MEMORY_SEGMENT_SOURCE_OTHER", 4),
            ("MEMORY_SEGMENT_SOURCE_PREVIEWED", 5),
        ]
    );
    assert_eq!(
        field_numbers(service, "HardwareClassResourceEstimate"),
        [
            ("hardware_class_id", 1),
            ("estimates", 2),
            ("derived_engine_parameter_values", 3),
            ("launch_rejection", 4)
        ]
    );
    assert_eq!(
        field_numbers(service, "ScalarResourceEstimate"),
        [("resource", 1), ("value", 2), ("unit", 3)]
    );
    assert_eq!(
        field_layout(service, "ListModelProfileCompatibilityRequest"),
        [
            ("model_id", 1, "TYPE_STRING"),
            ("target", 2, ".dotcore.deployment.v1.ExactPlacement"),
            ("scenario", 3, ".dotcore.model.v1.SizingScenario"),
        ]
    );
    assert_eq!(
        field_numbers(service, "SizingScenario"),
        [
            ("total_context_tokens", 1),
            ("context_tokens_per_request", 2)
        ]
    );
    assert!(
        field_is_proto3_optional(service, "SizingScenario", "context_tokens_per_request"),
        "scenario context per request must preserve scalar presence"
    );
    assert_eq!(
        field_layout(service, "ListModelRecommendationsRequest"),
        [("use_cases", 1, ".dotcore.catalog.v1.UseCase")]
    );
    assert_eq!(
        field_layout(service, "RecommendedProfileTarget"),
        [
            ("target", 1, ".dotcore.deployment.v1.ExactPlacement"),
            ("fit", 2, ".dotcore.model.v1.ModelRecommendationFit"),
            ("deployed", 3, "TYPE_BOOL"),
            ("starting_total_context_tokens", 4, "TYPE_UINT64"),
        ]
    );
    assert_eq!(
        field_layout(service, "ListModelProfileCompatibilityResponse"),
        [
            ("targets", 1, ".dotcore.model.v1.TargetCompatibility"),
            ("entries", 2, ".dotcore.model.v1.ModelProfileCompatibility"),
            ("profiles", 3, ".dotcore.model.v1.ModelProfileSizing"),
        ]
    );
    assert_eq!(
        field_numbers(service, "ModelProfileCompatibility"),
        [
            ("model_id", 1),
            ("profile_id", 2),
            ("target", 3),
            ("status", 4),
            ("reason", 5),
            ("advisories", 6),
            ("hardware_class_id", 7),
            ("memory_fit", 8),
            ("free_memory_fit", 11),
            ("unsupported_reason", 9),
            ("largest_total_context_tokens", 10),
            ("starting_total_context_tokens", 12),
            ("starting_total_context_source", 13),
        ]
    );
    for field in [
        "largest_total_context_tokens",
        "starting_total_context_tokens",
    ] {
        assert!(
            field_is_proto3_optional(service, "ModelProfileCompatibility", field),
            "{field} must preserve scalar presence"
        );
    }
    assert_eq!(
        field_layout(service, "ModelProfileSizing"),
        [
            ("model_id", 1, "TYPE_STRING"),
            ("profile_id", 2, "TYPE_STRING"),
            (
                "engine_parameter_values",
                3,
                ".dotcore.catalog.v1.ParameterValues"
            ),
            (
                "hardware_class_estimates",
                4,
                ".dotcore.model.v1.HardwareClassResourceEstimate"
            ),
            (
                "unsupported_reason",
                5,
                ".dotcore.model.v1.SizingScenarioUnsupportedReason"
            ),
            ("unsupported_hardware_class_ids", 6, "TYPE_STRING"),
        ]
    );
    assert_eq!(
        field_type_name(service, "ModelProfileCompatibility", "memory_fit"),
        ".dotcore.deployment.v1.MemoryFit"
    );
    assert_eq!(
        enum_values(
            descriptor_file(&descriptor, "dotcore/deployment/v1/memory_fit.proto"),
            "MemoryFit"
        ),
        [
            ("MEMORY_FIT_UNSPECIFIED", 0),
            ("MEMORY_FIT_FITS", 1),
            ("MEMORY_FIT_EXCEEDS", 2),
            ("MEMORY_FIT_UNKNOWN", 3),
        ]
    );
    assert_eq!(
        enum_values(service, "SizingScenarioUnsupportedReason"),
        [
            ("SIZING_SCENARIO_UNSUPPORTED_REASON_UNSPECIFIED", 0),
            (
                "SIZING_SCENARIO_UNSUPPORTED_REASON_CONTEXT_ABOVE_MAXIMUM",
                1
            ),
            (
                "SIZING_SCENARIO_UNSUPPORTED_REASON_PARAMETER_OUT_OF_RANGE",
                3
            ),
            (
                "SIZING_SCENARIO_UNSUPPORTED_REASON_DERIVED_PARAMETER_OUT_OF_RANGE",
                4
            ),
        ]
    );
    assert_eq!(
        enum_values(service, "StartingTotalContextSource"),
        [
            ("STARTING_TOTAL_CONTEXT_SOURCE_UNSPECIFIED", 0),
            ("STARTING_TOTAL_CONTEXT_SOURCE_LARGEST_FIT", 1),
            ("STARTING_TOTAL_CONTEXT_SOURCE_PROFILE_DEFAULT", 2),
        ]
    );

    assert_eq!(
        field_layout(deployment, "ModelDeploymentSnapshot"),
        [
            (
                "local",
                1,
                ".dotcore.model.v1.ResolvedLocalModelDeploymentSnapshot"
            ),
            (
                "upstream",
                4,
                ".dotcore.model.v1.UpstreamModelDeploymentSnapshot"
            ),
        ]
    );
    assert_eq!(
        field_numbers(deployment, "ResolvedLocalModelDeploymentSnapshot"),
        [
            ("model_id", 1),
            ("model_display_name", 2),
            ("model_version", 3),
            ("configuration_id", 4),
            ("profile_id", 5),
            ("engine_artifact", 6),
            ("model_artifact", 7),
            ("auxiliary_artifacts", 8),
            ("hardware_support", 21),
            ("resolved_engine_state", 10),
            ("execution_capabilities", 11),
            ("input_capabilities", 12),
            ("request_kinds", 22),
            ("engine_parameter_values", 13),
            ("engine_parameter_schema_id", 14),
            ("engine_parameter_schema", 15),
            ("desired_launch_plan", 16),
            ("custom_argument_tokens", 17),
            ("resolved_options", 18),
            ("selected_target", 19),
            ("sizing", 20),
        ]
    );
    assert_eq!(
        field_layout(deployment, "ResolvedModelOption"),
        [("values", 1, ".dotcore.catalog.v1.ModelOptionValues")]
    );
    assert_eq!(
        field_layout(deployment, "ResolvedSelectedTarget"),
        [
            (
                "accelerator_memory_architecture",
                1,
                ".dotcore.observation.v1.AcceleratorMemoryArchitecture"
            ),
            ("accelerator_memory_pool_total_bytes", 2, "TYPE_UINT64"),
        ]
    );
    assert_eq!(
        field_numbers(deployment, "ResolvedSizing"),
        [
            ("accelerator_memory_bytes", 1),
            ("derived_engine_parameter_ids", 2),
            ("host_memory_bytes", 3),
        ]
    );
    assert_eq!(
        field_layout(deployment, "ResolvedEngineArtifact"),
        [
            ("engine_release_id", 1, "TYPE_STRING"),
            ("engine_kind", 2, ".dotcore.engine.v1.EngineKind"),
            ("engine_version", 3, "TYPE_STRING"),
            ("artifact", 4, ".dotcore.catalog.v1.EngineArtifact"),
            ("platform", 7, ".dotcore.catalog.v1.OciPlatform"),
            ("accelerator", 8, "TYPE_STRING"),
            ("minimum_nvidia_driver_major_version", 9, "TYPE_UINT32"),
            (
                "preparation_policy",
                5,
                ".dotcore.deployment.v1.ArtifactPreparationPolicy"
            ),
            ("launch_identity", 6, "TYPE_STRING"),
            ("storage_bindings", 10, ".dotcore.catalog.v1.StorageBinding"),
        ]
    );
    assert_eq!(
        field_layout(deployment, "PortableModelLaunchPlan"),
        [
            (
                "arguments",
                1,
                ".dotcore.model.v1.PortableModelLaunchArgumentToken"
            ),
            ("format_version", 2, "TYPE_UINT32"),
            (
                "artifact_bindings",
                3,
                ".dotcore.model.v1.PortableModelArtifactBinding"
            ),
        ]
    );
    assert_eq!(
        field_layout(deployment, "PortableModelLaunchArgumentToken"),
        [
            ("literal", 1, "TYPE_STRING"),
            ("artifact_binding_id", 2, "TYPE_STRING"),
            ("engine_executable", 3, ".google.protobuf.Empty"),
            ("service_host", 4, "TYPE_STRING"),
            ("service_port", 5, "TYPE_STRING"),
        ]
    );
    assert_eq!(
        field_layout(deployment, "PortableModelArtifactBinding"),
        [
            ("binding_id", 1, "TYPE_STRING"),
            ("model_artifact_id", 2, "TYPE_STRING"),
            ("auxiliary_artifact_id", 3, "TYPE_STRING"),
            ("read_only", 5, "TYPE_BOOL"),
        ]
    );
}

#[test]
fn model_contracts_exclude_runtime_and_resource_authority_scope() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let files = [
        descriptor_file(&descriptor, "dotcore/catalog/v1/engine_release.proto"),
        descriptor_file(&descriptor, "dotcore/catalog/v1/model.proto"),
        descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto"),
        descriptor_file(&descriptor, "dotcore/model/v1/service.proto"),
    ];

    for file in files {
        let field_names = file
            .message_type
            .iter()
            .flat_map(|message| message.field.iter())
            .filter_map(|field| field.name.as_deref())
            .collect::<Vec<_>>();
        for excluded in [
            "custom_argv",
            "observation",
            "host_path",
            "prepared_path",
            "runtime_id",
            "secret",
            "capability_activation",
        ] {
            assert!(
                field_names
                    .iter()
                    .all(|field_name| !field_name.contains(excluded)),
                "Model contracts must exclude {excluded}"
            );
        }
    }
}

#[test]
fn model_descriptors_preserve_existing_status_values_and_define_pause_api() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let deployment = descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto");
    let state = descriptor_file(&descriptor, "dotcore/model/v1/state.proto");
    let service = descriptor_file(&descriptor, "dotcore/model/v1/service.proto");

    assert_eq!(
        enum_values(deployment, "ModelDeploymentDesiredState"),
        [
            ("MODEL_DEPLOYMENT_DESIRED_STATE_UNSPECIFIED", 0),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_PRESENT", 1),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_TEARDOWN", 2),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_PAUSED", 3),
        ]
    );
    assert_eq!(
        enum_values(state, "ModelDeploymentStatus"),
        [
            ("MODEL_DEPLOYMENT_STATUS_UNSPECIFIED", 0),
            ("MODEL_DEPLOYMENT_STATUS_QUEUED", 2),
            ("MODEL_DEPLOYMENT_STATUS_PREPARING", 3),
            ("MODEL_DEPLOYMENT_STATUS_LOADING", 4),
            ("MODEL_DEPLOYMENT_STATUS_WARMING_UP", 5),
            ("MODEL_DEPLOYMENT_STATUS_READY", 6),
            ("MODEL_DEPLOYMENT_STATUS_UNLOADING", 7),
            ("MODEL_DEPLOYMENT_STATUS_FAILED", 8),
            ("MODEL_DEPLOYMENT_STATUS_UNAVAILABLE", 9),
            ("MODEL_DEPLOYMENT_STATUS_PAUSING", 10),
            ("MODEL_DEPLOYMENT_STATUS_PAUSED", 11),
            ("MODEL_DEPLOYMENT_STATUS_DOWNLOADING", 12),
        ]
    );
    assert_service_methods(
        service,
        "ModelDeploymentService",
        &[
            (
                "ListModelDeployments",
                "ListModelDeploymentsRequest",
                "ListModelDeploymentsResponse",
            ),
            (
                "CreateModelDeployment",
                "CreateModelDeploymentRequest",
                "CreateModelDeploymentResponse",
            ),
            (
                "PreviewLocalModelDeployment",
                "PreviewLocalModelDeploymentRequest",
                "PreviewLocalModelDeploymentResponse",
            ),
            (
                "EstimateLocalModelResources",
                "EstimateLocalModelResourcesRequest",
                "EstimateLocalModelResourcesResponse",
            ),
            (
                "ListModelProfileCompatibility",
                "ListModelProfileCompatibilityRequest",
                "ListModelProfileCompatibilityResponse",
            ),
            (
                "ListModelRecommendations",
                "ListModelRecommendationsRequest",
                "ListModelRecommendationsResponse",
            ),
            (
                "UpdateModelDeployment",
                "UpdateModelDeploymentRequest",
                "UpdateModelDeploymentResponse",
            ),
            (
                "RemoveModelDeployment",
                "RemoveModelDeploymentRequest",
                "RemoveModelDeploymentResponse",
            ),
            ("PauseModel", "PauseModelRequest", "PauseModelResponse"),
            ("ResumeModel", "ResumeModelRequest", "ResumeModelResponse"),
            (
                "ReadDeploymentLogs",
                "ReadDeploymentLogsRequest",
                "ReadDeploymentLogsResponse",
            ),
            (
                "FollowDeploymentLogs",
                "FollowDeploymentLogsRequest",
                "FollowDeploymentLogsResponse",
            ),
            (
                "UpdateModelDeploymentMetadata",
                "UpdateModelDeploymentMetadataRequest",
                "UpdateModelDeploymentMetadataResponse",
            ),
        ],
    );
    for site_owned_message in [
        "ListConfiguredExecutionTargetsRequest",
        "ListConfiguredExecutionTargetsResponse",
        "ConfiguredExecutionTarget",
    ] {
        assert!(
            service
                .message_type
                .iter()
                .all(|message| message.name.as_deref() != Some(site_owned_message)),
            "{site_owned_message} must be owned by the site service"
        );
    }
    assert_eq!(
        field_numbers(service, "CreateModelDeploymentRequest"),
        [
            ("local", 1),
            ("upstream", 2),
            ("exposure", 3),
            ("model_deployment_name", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "CreateLocalModelDeploymentRequest"),
        [
            ("model_id", 1),
            ("profile_id", 2),
            ("placement", 3),
            ("configuration", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "CreateUpstreamModelDeploymentRequest"),
        [
            ("upstream_model_template_id", 1),
            ("desired_configuration", 2)
        ]
    );
    assert_eq!(
        field_numbers(service, "ModelDeploymentExposure"),
        [("direct", 1), ("hidden", 2)]
    );
    assert_eq!(
        field_numbers(service, "DirectRouteExposure"),
        [("route_name", 1)]
    );
    assert_eq!(field_numbers(service, "HiddenDeploymentExposure"), []);
    assert_eq!(
        field_numbers(service, "UpdateModelDeploymentRequest"),
        [
            ("model_deployment_id", 1),
            ("upstream_desired_configuration", 2),
        ]
    );
    assert_eq!(
        field_numbers(service, "UpdateModelDeploymentResponse"),
        [("deployment", 1)]
    );
    assert_eq!(
        field_numbers(service, "PauseModelRequest"),
        [("model_deployment_id", 1)]
    );
    assert_eq!(
        field_numbers(service, "PauseModelResponse"),
        [("deployment", 1)]
    );
    assert_eq!(
        field_numbers(service, "ResumeModelRequest"),
        [("model_deployment_id", 1)]
    );
    assert_eq!(
        field_numbers(service, "ResumeModelResponse"),
        [("deployment", 1)]
    );
    assert_eq!(
        field_numbers(service, "ReadDeploymentLogsRequest"),
        [
            ("model_deployment_id", 1),
            ("source_names", 2),
            ("page_size", 3),
            ("before_timestamp", 4),
        ]
    );
    assert_eq!(
        field_numbers(service, "FollowDeploymentLogsRequest"),
        [
            ("model_deployment_id", 1),
            ("source_names", 2),
            ("resume_positions", 3),
        ]
    );
    assert_eq!(
        field_numbers(service, "DeploymentLogEntry"),
        [
            ("source_name", 1),
            ("source_label", 2),
            ("stream", 3),
            ("timestamp", 4),
            ("content", 5),
            ("truncated", 6),
        ]
    );
    assert_eq!(
        field_numbers(service, "UpdateModelDeploymentMetadataRequest"),
        [
            ("model_deployment_id", 1),
            ("metadata", 2),
            ("update_mask", 3),
        ]
    );
    assert_eq!(
        field_numbers(service, "ModelDeploymentMetadata"),
        [("display_name", 1)]
    );
    assert_eq!(
        field_numbers(service, "UpdateModelDeploymentMetadataResponse"),
        [("deployment", 1)]
    );
    assert!(
        field_is_proto3_optional(service, "ModelDeploymentMetadata", "display_name"),
        "metadata display_name must preserve scalar presence"
    );
    assert_eq!(
        field_type_name(
            service,
            "UpdateModelDeploymentMetadataRequest",
            "update_mask"
        ),
        ".google.protobuf.FieldMask"
    );
    assert_message_absent(service, "RenameModelDeploymentRequest");
    assert_message_absent(service, "RenameModelDeploymentResponse");
    assert_service_method_absent(service, "ModelDeploymentService", "RenameModelDeployment");
    assert_eq!(
        field_numbers(deployment, "ModelDeploymentDeclaration"),
        [
            ("model_deployment_id", 1),
            ("placement", 2),
            ("requested_parallel_shape", 3),
            ("snapshot", 4),
            ("desired_state", 5),
            ("upstream_desired_configuration", 6),
            ("model_deployment_name", 7),
            ("lifecycle_generation", 8),
            ("lifecycle_transition", 9),
            ("lifecycle_recovery_required", 10),
            ("node_resolved_launch_identities", 11),
        ]
    );
    assert_eq!(
        field_numbers(deployment, "ModelDeployment"),
        [
            ("model_deployment_id", 1),
            ("intent", 2),
            ("snapshot", 3),
            ("state", 4),
            ("desired_state", 5),
            ("upstream_desired_configuration", 6),
            ("model_deployment_name", 7),
            ("accelerator_memory", 8),
            ("node_resolved_launch_identities", 9),
        ]
    );
    assert_eq!(
        field_type_name(deployment, "ModelDeployment", "accelerator_memory"),
        ".dotcore.deployment.v1.AcceleratorMemoryObservation"
    );
    assert_message_absent(deployment, "AcceleratorMemoryObservation");
    assert_eq!(
        field_numbers(
            descriptor_file(
                &descriptor,
                "dotcore/deployment/v1/accelerator_memory.proto"
            ),
            "AcceleratorMemoryDeviceUsage"
        ),
        [
            ("managed_node_id", 1),
            ("device_id", 2),
            ("used_memory_bytes", 3),
        ]
    );
}

#[test]
fn public_model_contracts_exclude_runtime_native_details_outside_log_vocabulary() {
    let contracts = [
        include_str!("../dotcore/model/v1/deployment.proto"),
        include_str!("../dotcore/model/v1/state.proto"),
        include_str!("../dotcore/model/v1/service.proto"),
    ];

    for contract in contracts {
        let non_log_contract = contract
            .split_once("// Requests a bounded retained-log page")
            .map_or(contract, |(prefix, _)| prefix);
        for private_name in [
            "docker",
            "container_id",
            "network_name",
            "volume_name",
            "host_port",
            "runtime_id",
            "workload_id",
        ] {
            assert!(
                !non_log_contract.to_ascii_lowercase().contains(private_name),
                "public model contract must exclude {private_name} outside log vocabulary"
            );
        }
    }
}

fn render_workload_arguments(plan: &PortableModelLaunchPlan) -> Vec<String> {
    plan.arguments
        .iter()
        .map(
            |argument| match argument.value.as_ref().expect("argument value") {
                PortableArgumentValue::Literal(value) => value.clone(),
                PortableArgumentValue::ArtifactBindingId(binding_id) => {
                    assert!(
                        plan.artifact_bindings
                            .iter()
                            .any(|binding| binding.binding_id == *binding_id),
                        "referenced artifact binding"
                    );
                    format!("{{artifact:{binding_id}}}")
                }
                PortableArgumentValue::EngineExecutable(_) => "{engine.executable}".to_owned(),
                PortableArgumentValue::ServiceHost(name) => format!("{{service:{name}.host}}"),
                PortableArgumentValue::ServicePort(name) => format!("{{service:{name}.port}}"),
            },
        )
        .collect()
}

/// A segment of a model deployment, or of no deployment when `model_deployment_id` is empty.
fn segment(source: MemorySegmentSource, model_deployment_id: &str, bytes: u64) -> MemorySegment {
    MemorySegment {
        source: source as i32,
        owner: (!model_deployment_id.is_empty())
            .then(|| memory_segment::Owner::ModelDeploymentId(model_deployment_id.to_owned())),
        bytes,
    }
}

fn literal_argument(value: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(PortableArgumentValue::Literal(value.to_owned())),
    }
}

fn binding_argument(binding_id: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(PortableArgumentValue::ArtifactBindingId(
            binding_id.to_owned(),
        )),
    }
}

fn model_artifact_binding(binding_id: &str, artifact_id: &str) -> PortableModelArtifactBinding {
    PortableModelArtifactBinding {
        binding_id: binding_id.to_owned(),
        artifact: Some(PortableModelArtifact::ModelArtifactId(
            artifact_id.to_owned(),
        )),
        read_only: true,
    }
}

fn auxiliary_artifact_binding(binding_id: &str, artifact_id: &str) -> PortableModelArtifactBinding {
    PortableModelArtifactBinding {
        binding_id: binding_id.to_owned(),
        artifact: Some(PortableModelArtifact::AuxiliaryArtifactId(
            artifact_id.to_owned(),
        )),
        read_only: true,
    }
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
}

fn enum_values<'a>(file: &'a FileDescriptorProto, enum_name: &str) -> Vec<(&'a str, i32)> {
    file.enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some(enum_name))
        .unwrap_or_else(|| panic!("{enum_name} enum must exist"))
        .value
        .iter()
        .map(|value| {
            (
                value.name.as_deref().expect("enum value name"),
                value.number.expect("enum value number"),
            )
        })
        .collect()
}

fn field_numbers<'a>(file: &'a FileDescriptorProto, message_name: &str) -> Vec<(&'a str, i32)> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
            )
        })
        .collect()
}

fn field_labels<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
) -> Vec<(&'a str, &'static str)> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                match field.label() {
                    FieldLabel::Optional => "LABEL_OPTIONAL",
                    FieldLabel::Required => "LABEL_REQUIRED",
                    FieldLabel::Repeated => "LABEL_REPEATED",
                },
            )
        })
        .collect()
}

fn field_layout<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
) -> Vec<(&'a str, i32, &'a str)> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .map(|field| {
            (
                field.name.as_deref().expect("field name"),
                field.number.expect("field number"),
                field
                    .type_name
                    .as_deref()
                    .unwrap_or_else(|| match field.r#type() {
                        FieldType::String => "TYPE_STRING",
                        FieldType::Bool => "TYPE_BOOL",
                        FieldType::Uint64 => "TYPE_UINT64",
                        FieldType::Uint32 => "TYPE_UINT32",
                        FieldType::Double => "TYPE_DOUBLE",
                        field_type => panic!("unexpected scalar field type {field_type:?}"),
                    }),
            )
        })
        .collect()
}

fn field_is_proto3_optional(
    file: &FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> bool {
    message_field(file, message_name, field_name).proto3_optional()
}

fn field_type_name<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a str {
    message_field(file, message_name, field_name)
        .type_name
        .as_deref()
        .expect("message field must name its message type")
}

fn message_field<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a prost_types::FieldDescriptorProto {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(field_name))
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must exist"))
}

fn assert_message_absent(file: &FileDescriptorProto, message_name: &str) {
    assert!(
        !file
            .message_type
            .iter()
            .any(|message| message.name.as_deref() == Some(message_name)),
        "{message_name} must be absent"
    );
}

fn assert_service_method_absent(file: &FileDescriptorProto, service_name: &str, method_name: &str) {
    let service = file
        .service
        .iter()
        .find(|service| service.name.as_deref() == Some(service_name))
        .unwrap_or_else(|| panic!("{service_name} service must exist"));
    assert!(
        !service
            .method
            .iter()
            .any(|method| method.name.as_deref() == Some(method_name)),
        "{method_name} must be absent"
    );
}

fn assert_service_methods(
    file: &FileDescriptorProto,
    service_name: &str,
    methods: &[(&str, &str, &str)],
) {
    let service = file
        .service
        .iter()
        .find(|service| service.name.as_deref() == Some(service_name))
        .unwrap_or_else(|| panic!("{service_name} service must exist"));

    assert_eq!(service.method.len(), methods.len());
    for (method, (name, input, output)) in service.method.iter().zip(methods) {
        assert_eq!(method.name.as_deref(), Some(*name));
        assert!(
            method
                .input_type
                .as_deref()
                .is_some_and(|actual| actual.ends_with(input))
        );
        assert!(
            method
                .output_type
                .as_deref()
                .is_some_and(|actual| actual.ends_with(output))
        );
    }
}
