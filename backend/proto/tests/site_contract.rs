//! Site, node, catalog, and current-state protobuf contract tests.
//!
//! These tests pin descriptor identities, field/enum layout, restricted YAML
//! round trips, and declaration/current-state boundaries. Breakage changes
//! persisted site interpretation or generated clients. Runtime lifecycle,
//! catalog loading, and application/model behavior are intentionally excluded
//! and covered by backend integration and catalog suites.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        application::v1::{
            ApplicationConfigurationAttachment, ApplicationDeploymentDeclaration,
            ApplicationDeploymentDesiredState, ApplicationStorageAttachment,
            ResolvedApplicationDeploymentSnapshot, ResolvedApplicationServiceSelection,
        },
        catalog::v1::{
            Application, ApplicationConfigurationBinding, ApplicationDependencyCondition,
            ApplicationEndpointProtocol, ApplicationGraph, ApplicationHttpReadiness,
            ApplicationPrimaryEndpoint, ApplicationService, ApplicationServiceArtifact,
            ApplicationServiceDependency, ApplicationServicePort, ApplicationServiceReadiness,
            EngineArtifact, EngineParameterSchema, HuggingFaceArtifactSource,
            HuggingFaceFileArtifact, ModelArtifact, ModelExecutionCapabilities,
            ModelInputCapabilities, ModelInputModality, ModelParallelShape, OciDigest,
            OciImageReference, OciPlatform, OciRegistryLocation, OciRepository,
            ParameterDefinition, ParameterValue, ParameterValues, ParameterVisibility, Selector,
            StorageBinding, StorageEnvironmentVariable, StorageKind, StorageScope,
            UnsignedIntegerParameterSchema, application_configuration_binding,
            application_service_artifact, application_service_readiness, engine_artifact,
            hugging_face_artifact_source, parameter_definition, parameter_value,
        },
        deployment::v1::{ArtifactPreparationPolicy, NodeResolvedLaunchIdentity},
        engine::{
            llama_cpp::v1::{LlamaCppFixedModelSettings, LlamaCppResolvedModelState},
            v1::{EngineKind, ResolvedEngineModelState, resolved_engine_model_state},
        },
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentSnapshot,
            PortableModelArtifactBinding, PortableModelLaunchArgumentToken,
            PortableModelLaunchPlan, ResolvedEngineArtifact, ResolvedHardwareSupport,
            ResolvedLocalModelDeploymentSnapshot,
            model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
            portable_model_artifact_binding::Artifact as PortableModelArtifact,
            portable_model_launch_argument_token::Value as PortableArgumentValue,
        },
        node::v1::{LoggingConfig, NodeConfiguration},
        observation::v1::{
            AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, AcceleratorVendor,
            ObservationUnavailable, accelerator_observation,
        },
        site::v1::{
            DockerAcceleratorTransport, DockerApplicationRuntimeAdapter, DockerRuntimeAdapter,
            ExecutionTarget, HardwareObservationConfiguration, ModelRuntimeBinding, Site,
            SiteManagerRole, SiteNode, application_runtime_binding, execution_target,
            model_runtime_binding,
        },
    },
    yaml::{self, RestrictedYamlError},
};
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

#[test]
fn site_node_keeps_the_common_stable_identifier_wire_contract() {
    let node = SiteNode {
        node_id: "local-node-1".to_owned(),
        display_name: "Local node".to_owned(),
        site_manager: Some(SiteManagerRole {}),
        host: "192.0.2.10".to_owned(),
        manager_host: String::new(),
    };

    assert_eq!(
        node.encode_to_vec(),
        [
            0x0a, 0x0c, b'l', b'o', b'c', b'a', b'l', b'-', b'n', b'o', b'd', b'e', b'-', b'1',
            0x12, 0x0a, b'L', b'o', b'c', b'a', b'l', b' ', b'n', b'o', b'd', b'e', 0x1a, 0x00,
            0x2a, 0x0a, b'1', b'9', b'2', b'.', b'0', b'.', b'2', b'.', b'1', b'0',
        ]
    );
}

#[test]
fn execution_target_round_trips_one_concrete_platform_and_typed_binding() {
    let target = ExecutionTarget {
        execution_target_id: "application-target-1".to_owned(),
        site_node_id: "local-node-1".to_owned(),
        display_name: "Docker applications".to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
            dotcore_proto::dotcore::site::v1::ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                    DockerApplicationRuntimeAdapter {},
                )),
            },
        )),
    };

    assert_eq!(
        ExecutionTarget::decode(target.encode_to_vec().as_slice())
            .expect("Execution target must decode"),
        target
    );
}

#[test]
fn structured_oci_contract_preserves_tag_digest_registry_and_platform_presence() {
    let artifact = ApplicationServiceArtifact {
        kind: Some(application_service_artifact::Kind::Oci(OciImageReference {
            repository: Some(OciRepository {
                registry: Some(OciRegistryLocation {
                    host: "registry.dotcore.test".to_owned(),
                    port: Some(5_000),
                }),
                name: "dotcore/hermes".to_owned(),
            }),
            tag: Some("development".to_owned()),
            digest: Some(OciDigest {
                algorithm: "sha256".to_owned(),
                encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_owned(),
            }),
        })),
        supported_platforms: vec![OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: Some("v8".to_owned()),
        }],
    };

    assert_eq!(
        ApplicationServiceArtifact::decode(artifact.encode_to_vec().as_slice())
            .expect("the structured OCI artifact must decode"),
        artifact,
    );
    assert_eq!(OciImageReference::default().tag, None);
    assert_eq!(OciRegistryLocation::default().port, None);
    assert_eq!(OciPlatform::default().variant, None);
}

#[test]
fn structured_oci_contract_distinguishes_reference_forms_and_optional_presence() {
    let repository = Some(OciRepository {
        registry: None,
        name: "dotcore/hermes".to_owned(),
    });
    let digest = Some(OciDigest {
        algorithm: "sha256".to_owned(),
        encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
    });
    let references = [
        OciImageReference {
            repository: repository.clone(),
            tag: Some("dev".to_owned()),
            digest: None,
        },
        OciImageReference {
            repository: repository.clone(),
            tag: None,
            digest: digest.clone(),
        },
        OciImageReference {
            repository,
            tag: Some("dev".to_owned()),
            digest,
        },
    ];

    for reference in references {
        assert_eq!(
            OciImageReference::decode(reference.encode_to_vec().as_slice())
                .expect("the structured OCI reference must decode"),
            reference,
        );
    }

    let absent_tag = OciImageReference::default();
    let present_empty_tag = OciImageReference {
        tag: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(absent_tag.tag, None);
    assert_eq!(present_empty_tag.tag.as_deref(), Some(""));
    assert_ne!(
        absent_tag.encode_to_vec(),
        present_empty_tag.encode_to_vec()
    );

    let engine_artifact = EngineArtifact {
        selectors: vec![Selector {
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerators: vec!["nvidia".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(engine_artifact.selectors[0].accelerators, ["nvidia"]);
    assert_eq!(
        engine_artifact
            .selectors
            .first()
            .and_then(|selector| selector.platform.as_ref())
            .map(|platform| {
                (
                    platform.operating_system.as_str(),
                    platform.architecture.as_str(),
                )
            }),
        Some(("linux", "amd64")),
    );
}

#[test]
fn execution_target_contract_requires_one_explicit_supported_linux_platform() {
    let target = ExecutionTarget {
        execution_target_id: "model-target-1".to_owned(),
        site_node_id: "local-node-1".to_owned(),
        display_name: "NVIDIA GeForce RTX 5090".to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: vec!["GPU-1".to_owned()],
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                    DockerRuntimeAdapter {
                        accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                    },
                )),
            },
        )),
    };

    let platform = target.platform.as_ref().expect("execution target platform");
    assert_eq!(
        (
            platform.operating_system.as_str(),
            platform.architecture.as_str()
        ),
        ("linux", "arm64")
    );
    assert!(matches!(
        target.runtime_binding,
        Some(execution_target::RuntimeBinding::ModelRuntime(_))
    ));
}

#[test]
fn complete_site_contract_round_trips_restricted_yaml_and_rejects_unknown_fields() {
    let site = Site {
        site_id: "site-one".to_owned(),
        model_deployments: vec![ModelDeploymentDeclaration {
            model_deployment_id: "model-one".to_owned(),
            model_deployment_name: "Model One".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "local-node-1".to_owned(),
                execution_target_id: "model-target-1".to_owned(),
            }),
            requested_parallel_shape: ModelParallelShape::One as i32,
            snapshot: Some(complete_model_snapshot()),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: None,
            ..Default::default()
        }],
        application_deployments: vec![ApplicationDeploymentDeclaration {
            application_deployment_id: "app-one-260812t120000z-000001".to_owned(),
            application_deployment_name: "Application One".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "local-node-1".to_owned(),
                execution_target_id: "app-target-1".to_owned(),
            }),
            snapshot: Some(complete_application_snapshot()),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            storage_attachments: vec![ApplicationStorageAttachment {
                service_name: "web".to_owned(),
                binding_name: "data".to_owned(),
                storage_name: "web-data".to_owned(),
            }],
            configuration_attachments: vec![ApplicationConfigurationAttachment {
                service_name: "web".to_owned(),
                binding_name: "endpoint".to_owned(),
                environment_variable: "ENDPOINT".to_owned(),
                value: "http://192.0.2.1/v1".to_owned(),
            }],
            secret_attachments: Vec::new(),
            inference_route_id: None,
            lifecycle_generation: 4,
            node_resolved_launch_identities: vec![NodeResolvedLaunchIdentity {
                selection_key: "web".to_owned(),
                launch_identity:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_owned(),
            }],
            published_port: 31_234,
        }],
        nodes: vec![SiteNode {
            node_id: "local-node-1".to_owned(),
            display_name: "Local node".to_owned(),
            site_manager: Some(SiteManagerRole {}),
            host: "192.0.2.10".to_owned(),
            manager_host: String::new(),
        }],
        hardware_observation: Some(HardwareObservationConfiguration {
            refresh_interval_seconds: Some(5),
        }),
        ..Default::default()
    };

    let encoded = yaml::to_string(&site).expect("complete site YAML");
    assert_eq!(yaml::from_str::<Site>(&encoded).expect("round trip"), site);
    assert_eq!(
        encoded
            .matches("preparationPolicy: ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE")
            .count(),
        3,
        "the model engine artifact and both application services share one policy enum: {encoded}"
    );
    assert!(
        encoded.contains("selectionKey: web"),
        "node-resolved launch identities persist with the application declaration: {encoded}"
    );
    assert!(encoded.contains(
        "launchIdentity: registry.dotcore.test/engine@sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
    ));
    let application_owned_name = encoded.replace(
        "preparationPolicy: ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE",
        "preparationPolicy: APPLICATION_ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE",
    );
    let error = yaml::from_str::<Site>(&application_owned_name)
        .expect_err("Application-owned policy value name");
    let RestrictedYamlError::ProtoJsonCodecError(source) = &error else {
        panic!("expected ProtoJSON codec error, got {error}");
    };
    assert!(
        source.to_string().starts_with(
            "unknown variant `APPLICATION_ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE`"
        ),
        "unexpected decode error: {source}"
    );
    let error = yaml::from_str::<Site>(&format!("{encoded}unknownField: true\n"))
        .expect_err("unknown site field");
    let RestrictedYamlError::ProtoJsonCodecError(source) = &error else {
        panic!("expected ProtoJSON codec error, got {error}");
    };
    assert!(source.to_string().contains("unknown field `unknownField`"));
    assert_eq!(error.to_string(), "ProtoJSON codec error");
}

fn complete_model_snapshot() -> ModelDeploymentSnapshot {
    ModelDeploymentSnapshot {
        source: Some(ModelDeploymentSnapshotSource::Local(
            ResolvedLocalModelDeploymentSnapshot {
                model_id: "model-one".to_owned(),
                model_display_name: "Model One".to_owned(),
                model_version: "1.0.0".to_owned(),
                configuration_id: "configuration-one".to_owned(),
                profile_id: "profile-one".to_owned(),
                engine_artifact: Some(ResolvedEngineArtifact {
                    engine_release_id: "engine-one".to_owned(),
                    engine_kind: EngineKind::LlamaCpp as i32,
                    engine_version: "1.0.0".to_owned(),
                    artifact: Some(EngineArtifact {
                        artifact_id: "engine-linux-nvidia".to_owned(),
                        kind: Some(engine_artifact::Kind::Oci(OciImageReference {
                            repository: Some(OciRepository {
                                registry: Some(OciRegistryLocation {
                                    host: "registry.dotcore.test".to_owned(),
                                    port: None,
                                }),
                                name: "engine".to_owned(),
                            }),
                            tag: None,
                            digest: Some(OciDigest {
                                algorithm: "sha256".to_owned(),
                                encoded: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                                    .to_owned(),
                            }),
                        })),
                        selectors: Vec::new(),
                    }),
                    platform: Some(OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    }),
                    accelerator: "nvidia".to_owned(),
                    minimum_nvidia_driver_major_version: 580,
                    preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                    launch_identity: "registry.dotcore.test/engine@sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_owned(),
                    storage_bindings: vec![StorageBinding {
                        binding_name: "compile-cache".to_owned(),
                        mount_path: "/cache".to_owned(),
                        kind: StorageKind::Cache as i32,
                        scope: StorageScope::EngineArtifact as i32,
                        environment: vec![StorageEnvironmentVariable {
                            name: "CACHE_HOME".to_owned(),
                            relative_path: "home".to_owned(),
                        }],
                    }],
                }),
                model_artifact: Some(ModelArtifact {
                    source: Some(HuggingFaceArtifactSource {
                        repository: "dotcore/model-one".to_owned(),
                        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                        content: Some(hugging_face_artifact_source::Content::File(
                            HuggingFaceFileArtifact {
                                path: "model-one.gguf".to_owned(),
                                provenance_digest:
                                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                                        .to_owned(),
                            },
                        )),
                    }),
                    artifact_id: "q8-0".to_owned(),
                    weight_quantization: "Q8_0".to_owned(),
                }),
                auxiliary_artifacts: Vec::new(),
                hardware_support: Some(ResolvedHardwareSupport::default()),
                resolved_engine_state: Some(ResolvedEngineModelState {
                    engine_kind: EngineKind::LlamaCpp as i32,
                    semantics_version: 1,
                    state: Some(resolved_engine_model_state::State::LlamaCpp(
                        LlamaCppResolvedModelState {
                            fixed_settings: Some(LlamaCppFixedModelSettings {
                                gpu_layers: 99,
                                flash_attention: true,
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
                engine_parameter_values: Some(ParameterValues {
                    values: vec![ParameterValue {
                        parameter_id: "total-context".to_owned(),
                        value: Some(parameter_value::Value::UnsignedInteger(8_192)),
                    }],
                }),
                engine_parameter_schema_id: "schema-one".to_owned(),
                engine_parameter_schema: Some(EngineParameterSchema {
                    schema_id: "schema-one".to_owned(),
                    parameters: vec![ParameterDefinition {
                        parameter_id: "total-context".to_owned(),
                        display_name: "Total context".to_owned(),
                        parameter_type: Some(
                            parameter_definition::ParameterType::UnsignedInteger(
                                UnsignedIntegerParameterSchema {
                                    minimum: 1,
                                    maximum: 131_072,
                                    unit: "tokens".to_owned(),
                                    default_value: None,
                                },
                            ),
                        ),
                        visibility: ParameterVisibility::Basic as i32,
                        description: "Maximum prompt and generation context.".to_owned(),
                    }],
                }),
                custom_argument_tokens: Vec::new(),
                resolved_options: Vec::new(),
                selected_target: Some(dotcore_proto::dotcore::model::v1::ResolvedSelectedTarget {
                    accelerator_memory_architecture:
                        dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated as i32,
                    accelerator_memory_pool_total_bytes: None,
                }),
                sizing: None,
                desired_launch_plan: Some(PortableModelLaunchPlan {
                    arguments: vec![
                        PortableModelLaunchArgumentToken {
                            value: Some(PortableArgumentValue::Literal("--model".to_owned())),
                        },
                        PortableModelLaunchArgumentToken {
                            value: Some(PortableArgumentValue::ArtifactBindingId(
                                "model".to_owned(),
                            )),
                        },
                        PortableModelLaunchArgumentToken {
                            value: Some(PortableArgumentValue::Literal("--ctx-size".to_owned())),
                        },
                        PortableModelLaunchArgumentToken {
                            value: Some(PortableArgumentValue::Literal("8192".to_owned())),
                        },
                    ],
                    format_version: 1,
                    artifact_bindings: vec![PortableModelArtifactBinding {
                        binding_id: "model".to_owned(),
                        artifact: Some(PortableModelArtifact::ModelArtifactId("q8-0".to_owned())),
                        read_only: true,
                    }],
                }),
                request_kinds: Vec::new(),
            },
        )),
    }
}

fn complete_application_snapshot() -> ResolvedApplicationDeploymentSnapshot {
    let application = complete_application();
    let graph = application.graph.clone().expect("Application graph");
    ResolvedApplicationDeploymentSnapshot {
        application_id: application.application_id,
        display_name: application.display_name,
        version: application.version,
        notes: "First line.\nSecond line.".to_owned(),
        graph: Some(graph.clone()),
        placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "local-node-1".to_owned(),
            execution_target_id: "application-target".to_owned(),
        }),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        service_selections: graph
            .services
            .iter()
            .map(|service| ResolvedApplicationServiceSelection {
                service_name: service.service_name.clone(),
                preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                launch_identity: format!(
                    "registry.dotcore.test/{}@sha256:{}",
                    service.service_name,
                    if service.service_name == "database" {
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    } else {
                        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                    }
                ),
            })
            .collect(),
        parameter_values: Vec::new(),
        generated_parameter_ids: Vec::new(),
    }
}

fn complete_application() -> Application {
    Application {
        application_id: "app-one".to_owned(),
        display_name: "Application One".to_owned(),
        description: String::new(),
        notes: "First line.\nSecond line.".to_owned(),
        version: "1.0.0".to_owned(),
        graph: Some(ApplicationGraph {
            services: vec![
                ApplicationService {
                    service_name: "database".to_owned(),
                    artifact: Some(ApplicationServiceArtifact {
                        kind: Some(application_service_artifact::Kind::Oci(OciImageReference {
                            repository: Some(OciRepository {
                                registry: Some(OciRegistryLocation {
                                    host: "registry.dotcore.test".to_owned(),
                                    port: None,
                                }),
                                name: "database".to_owned(),
                            }),
                            tag: None,
                            digest: Some(OciDigest {
                                algorithm: "sha256".to_owned(),
                                encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                                    .to_owned(),
                            }),
                        })),
                        supported_platforms: vec![OciPlatform {
                            operating_system: "linux".to_owned(),
                            architecture: "amd64".to_owned(),
                            variant: None,
                        }],
                    }),
                    command: vec!["database".to_owned()],
                    arguments: vec!["--listen=5432".to_owned()],
                    ports: vec![ApplicationServicePort {
                        port_name: "database".to_owned(),
                        port_number: 5_432,
                    }],
                    configuration_bindings: Vec::new(),
                    storage_bindings: vec![StorageBinding {
                        binding_name: "database-data".to_owned(),
                        mount_path: "/var/lib/database".to_owned(),
                        kind: StorageKind::Data as i32,
                        scope: StorageScope::Deployment as i32,
                        environment: Vec::new(),
                    }],
                    secret_bindings: Vec::new(),
                    dependencies: Vec::new(),
                    readiness: None,
                },
                ApplicationService {
                    service_name: "web".to_owned(),
                    artifact: Some(ApplicationServiceArtifact {
                        kind: Some(application_service_artifact::Kind::Oci(OciImageReference {
                            repository: Some(OciRepository {
                                registry: Some(OciRegistryLocation {
                                    host: "registry.dotcore.test".to_owned(),
                                    port: None,
                                }),
                                name: "web".to_owned(),
                            }),
                            tag: None,
                            digest: Some(OciDigest {
                                algorithm: "sha256".to_owned(),
                                encoded: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                                    .to_owned(),
                            }),
                        })),
                        supported_platforms: vec![OciPlatform {
                            operating_system: "linux".to_owned(),
                            architecture: "amd64".to_owned(),
                            variant: None,
                        }],
                    }),
                    command: vec!["web".to_owned()],
                    arguments: vec!["--port=8080".to_owned()],
                    ports: vec![ApplicationServicePort {
                        port_name: "http".to_owned(),
                        port_number: 8_080,
                    }],
                    configuration_bindings: vec![ApplicationConfigurationBinding {
                        binding_name: "endpoint".to_owned(),
                        environment_variable: "ENDPOINT".to_owned(),
                        source: Some(application_configuration_binding::Source::ResolvedValueName(
                            "inference-openai-url".to_owned(),
                        )),
                        input: None,
                    }],
                    storage_bindings: vec![StorageBinding {
                        binding_name: "data".to_owned(),
                        mount_path: "/app/data".to_owned(),
                        kind: StorageKind::Data as i32,
                        scope: StorageScope::Deployment as i32,
                        environment: Vec::new(),
                    }],
                    secret_bindings: Vec::new(),
                    dependencies: vec![ApplicationServiceDependency {
                        service_name: "database".to_owned(),
                        condition: ApplicationDependencyCondition::Started as i32,
                    }],
                    readiness: Some(ApplicationServiceReadiness {
                        check: Some(application_service_readiness::Check::HttpGet(
                            ApplicationHttpReadiness {
                                port_name: "http".to_owned(),
                                path: "/health".to_owned(),
                            },
                        )),
                    }),
                },
            ],
            primary_endpoint: Some(ApplicationPrimaryEndpoint {
                service_name: "web".to_owned(),
                port_name: "http".to_owned(),
                protocol: ApplicationEndpointProtocol::Http as i32,
            }),
            parameters: Vec::new(),
        }),
    }
}

#[test]
fn node_configuration_uses_restricted_yaml_and_supports_optional_policy() {
    let configuration = yaml::from_str::<NodeConfiguration>("nodeId: local-node-1\n")
        .expect("node configuration should decode");

    assert_eq!(configuration.node_id, "local-node-1");
    assert_eq!(configuration.logging, None);

    let policy = yaml::from_str::<NodeConfiguration>(
        "logging:\n  retentionDays: '365'\nnodeId: local-node-1\n",
    )
    .expect("log retention policy should decode");
    assert_eq!(
        policy.logging,
        Some(LoggingConfig {
            retention_days: 365
        })
    );

    let error = yaml::from_str::<NodeConfiguration>("nodeId: local-node-1\nsiteManager: {}\n")
        .expect_err("node configuration must reject unknown YAML keys");
    let RestrictedYamlError::ProtoJsonCodecError(source) = &error else {
        panic!("expected ProtoJSON codec error, got {error:?}");
    };
    assert_eq!(
        source.to_string(),
        "unknown field `siteManager`, expected one of `node_id`, `nodeId`, `logging`, \
         `declared_machine`, `declaredMachine`"
    );
    assert_eq!(error.to_string(), "ProtoJSON codec error");
}

#[test]
fn generated_descriptors_preserve_site_inventory_contracts() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let site = descriptor_file(&descriptor, "dotcore/site/v1/site.proto");
    let site_service = descriptor_file(&descriptor, "dotcore/site/v1/service.proto");
    let observation = descriptor_file(&descriptor, "dotcore/observation/v1/observation.proto");
    let node_configuration = descriptor_file(&descriptor, "dotcore/node/v1/config.proto");

    assert_eq!(field_number(site, "Site", "site_id"), 1);
    assert_eq!(field_number(site, "Site", "execution_targets"), 2);
    assert_eq!(field_number(site, "Site", "model_deployments"), 3);
    assert_eq!(field_number(site, "Site", "nodes"), 4);
    assert_eq!(field_number(site, "Site", "application_deployments"), 5);
    assert_eq!(field_number(site, "Site", "inference_routes"), 6);
    assert_eq!(field_number(site, "Site", "hardware_observation"), 7);
    assert_eq!(
        field_number(site, "Site", "model_deployment_identity_allocator"),
        8
    );
    assert_eq!(
        field_type_name(site, "Site", "hardware_observation"),
        ".dotcore.site.v1.HardwareObservationConfiguration"
    );
    assert_eq!(
        field_number(
            site,
            "HardwareObservationConfiguration",
            "refresh_interval_seconds"
        ),
        1
    );
    assert!(
        field(
            site,
            "HardwareObservationConfiguration",
            "refresh_interval_seconds"
        )
        .proto3_optional
        .expect("refresh interval presence")
    );
    assert_eq!(field_number(site, "SiteNode", "node_id"), 1);
    assert_eq!(field_number(site, "SiteNode", "display_name"), 2);
    assert_eq!(field_number(site, "SiteNode", "site_manager"), 3);
    assert_eq!(field_number(site, "SiteNode", "host"), 5);
    assert_eq!(field_number(site, "SiteNode", "manager_host"), 6);
    assert_eq!(
        field_names(site, "SiteNode"),
        [
            "node_id",
            "display_name",
            "site_manager",
            "host",
            "manager_host"
        ]
    );
    assert_eq!(
        field_number(node_configuration, "NodeConfiguration", "node_id"),
        1
    );
    assert_eq!(
        field_number(node_configuration, "NodeConfiguration", "logging"),
        2
    );
    assert_eq!(
        field_type_name(node_configuration, "NodeConfiguration", "logging"),
        ".dotcore.node.v1.LoggingConfig"
    );
    assert_eq!(
        field_type_name(node_configuration, "NodeExecutionTarget", "kind"),
        ".dotcore.observation.v1.RuntimeKind"
    );
    assert_eq!(
        field_number(node_configuration, "LoggingConfig", "retention_days"),
        1
    );
    assert_eq!(
        field_number(site, "DockerRuntimeAdapter", "accelerator_transport"),
        1
    );
    assert_eq!(
        field_names(site_service, "Machine"),
        [
            "machine_id",
            "display_name",
            "site_manager",
            "current_hardware_snapshot_availability",
            "observation_freshness",
            "runtimes",
        ]
    );
    assert_eq!(field_number(site_service, "Machine", "machine_id"), 1);
    assert_eq!(field_number(site_service, "Machine", "display_name"), 2);
    assert_eq!(field_number(site_service, "Machine", "site_manager"), 6);
    assert_eq!(
        field_names(site_service, "ConfiguredExecutionTarget"),
        [
            "execution_target_id",
            "display_name",
            "site_node_id",
            "site_node_display_name",
            "site_manager",
            "platform_label",
            "workload_kind",
            "availability",
            "accelerator_device_ids",
            "observed_accelerator_device_ids",
            "accelerator_resolution",
        ]
    );
    assert_eq!(
        field_number(site_service, "ConfiguredExecutionTarget", "site_manager"),
        7
    );
    assert_eq!(
        field_number(site_service, "ConfiguredExecutionTarget", "display_name"),
        11
    );
    assert_eq!(
        field_number(
            site_service,
            "ConfiguredExecutionTarget",
            "site_node_display_name"
        ),
        8
    );
    assert_eq!(
        field_number(
            site_service,
            "Machine",
            "current_hardware_snapshot_availability"
        ),
        3
    );
    assert_eq!(
        field_type_name(
            site_service,
            "Machine",
            "current_hardware_snapshot_availability"
        ),
        ".dotcore.site.v1.CurrentHardwareSnapshotAvailability"
    );
    assert_eq!(
        field_number(site_service, "Machine", "observation_freshness"),
        4
    );
    assert_eq!(
        field_type_name(site_service, "Machine", "observation_freshness"),
        ".dotcore.site.v1.NodeObservationFreshness"
    );
    assert_eq!(field_number(site_service, "Machine", "runtimes"), 5);
    assert_eq!(
        field_type_name(site_service, "Machine", "runtimes"),
        ".dotcore.observation.v1.RuntimeObservation"
    );
    assert_eq!(
        field_names(site_service, "NodeObservationFreshness"),
        ["state", "received_at"]
    );
    assert_eq!(
        field_type_name(site_service, "NodeObservationFreshness", "state"),
        ".dotcore.site.v1.NodeObservationState"
    );
    assert_eq!(
        field_type_name(site_service, "NodeObservationFreshness", "received_at"),
        ".google.protobuf.Timestamp"
    );
    let node_observation_state = site_service
        .enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some("NodeObservationState"))
        .expect("NodeObservationState enum must exist");
    assert_eq!(
        node_observation_state
            .value
            .iter()
            .map(|value| (
                value.name.as_deref().expect("enum value name"),
                value.number.expect("enum value number"),
            ))
            .collect::<Vec<_>>(),
        [
            ("NODE_OBSERVATION_STATE_UNSPECIFIED", 0),
            ("NODE_OBSERVATION_STATE_MISSING", 2),
            ("NODE_OBSERVATION_STATE_CONNECTED", 3),
            ("NODE_OBSERVATION_STATE_STALE", 4),
            ("NODE_OBSERVATION_STATE_DISCONNECTED", 5),
        ]
    );
    assert_field_absent(site_service, "Machine", "snapshot");
    assert_eq!(
        field_names(site_service, "CurrentHardwareSnapshotAvailability"),
        ["available", "unavailable"]
    );
    assert_eq!(
        field_number(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "available"
        ),
        1
    );
    assert_eq!(
        field_type_name(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "available"
        ),
        ".google.protobuf.Empty"
    );
    assert_eq!(
        field_oneof_index(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "available"
        ),
        0
    );
    assert_eq!(
        field_number(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "unavailable"
        ),
        2
    );
    assert_eq!(
        field_type_name(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "unavailable"
        ),
        ".dotcore.observation.v1.ObservationUnavailable"
    );
    assert_eq!(
        field_oneof_index(
            site_service,
            "CurrentHardwareSnapshotAvailability",
            "unavailable"
        ),
        0
    );
    assert_eq!(
        oneof_names(site_service, "CurrentHardwareSnapshotAvailability"),
        ["availability"]
    );
    assert_message_absent(site_service, "GetCurrentHardwareSnapshotRequest");
    assert_message_absent(site_service, "GetCurrentHardwareSnapshotResponse");
    assert!(field_names(site_service, "ListHardwareSnapshotsRequest").is_empty());
    assert_eq!(
        field_names(site_service, "ListHardwareSnapshotsResponse"),
        ["machines"]
    );
    assert_eq!(
        field_type_name(site_service, "ListHardwareSnapshotsResponse", "machines"),
        ".dotcore.site.v1.MachineHardwareSnapshot"
    );
    assert_eq!(
        field_names(site_service, "MachineHardwareSnapshot"),
        ["machine_id", "snapshot", "refresh", "observation_freshness"]
    );
    assert_eq!(
        field_number(site_service, "MachineHardwareSnapshot", "machine_id"),
        1
    );
    for (field_name, number, type_name) in [
        ("snapshot", 2, ".dotcore.observation.v1.HardwareSnapshot"),
        ("refresh", 3, ".dotcore.site.v1.HardwareRefreshState"),
        (
            "observation_freshness",
            4,
            ".dotcore.site.v1.NodeObservationFreshness",
        ),
    ] {
        assert_eq!(
            field_number(site_service, "MachineHardwareSnapshot", field_name),
            number,
            "{field_name} number"
        );
        assert_eq!(
            field_type_name(site_service, "MachineHardwareSnapshot", field_name),
            type_name,
            "{field_name} type"
        );
    }
    assert_eq!(
        field_names(site_service, "HardwareRefreshState"),
        ["attempted_at", "pending", "succeeded", "failed"]
    );
    assert_eq!(
        field_number(site_service, "HardwareRefreshState", "attempted_at"),
        1
    );
    assert_eq!(
        field_number(site_service, "HardwareRefreshState", "pending"),
        2
    );
    assert_eq!(
        field_number(site_service, "HardwareRefreshState", "succeeded"),
        3
    );
    assert_eq!(
        field_number(site_service, "HardwareRefreshState", "failed"),
        4
    );
    assert_eq!(
        oneof_names(site_service, "HardwareRefreshState"),
        ["outcome"]
    );
    assert_message_absent(site_service, "GetMachineRequest");
    assert_message_absent(site_service, "GetMachineResponse");
    assert_eq!(
        field_number(observation, "HardwareSnapshot", "observed_at"),
        1
    );
    assert_eq!(field_number(observation, "HardwareSnapshot", "host"), 2);
    assert_eq!(
        field_number(observation, "HardwareSnapshot", "accelerators"),
        3
    );
    assert_eq!(
        field_type_name(observation, "HardwareSnapshot", "accelerators"),
        ".dotcore.observation.v1.AcceleratorObservation"
    );
    assert_field_absent(observation, "HardwareSnapshot", "nvidia");
    assert_eq!(field_number(observation, "HostObservation", "facts"), 1);
    assert_eq!(
        field_number(observation, "HostObservation", "unavailable"),
        2
    );
    assert_eq!(
        field_number(observation, "AcceleratorObservation", "vendor"),
        1
    );
    assert_eq!(
        field_type_name(observation, "AcceleratorObservation", "vendor"),
        ".dotcore.observation.v1.AcceleratorVendor"
    );
    assert_eq!(
        field_number(observation, "AcceleratorObservation", "facts"),
        2
    );
    assert_eq!(
        field_type_name(observation, "AcceleratorObservation", "facts"),
        ".dotcore.observation.v1.AcceleratorFactsSet"
    );
    assert_eq!(
        field_oneof_index(observation, "AcceleratorObservation", "facts"),
        0
    );
    assert_eq!(
        field_number(observation, "AcceleratorObservation", "unavailable"),
        3
    );
    assert_eq!(
        field_type_name(observation, "AcceleratorObservation", "unavailable"),
        ".dotcore.observation.v1.ObservationUnavailable"
    );
    assert_eq!(
        field_oneof_index(observation, "AcceleratorObservation", "unavailable"),
        0
    );
    assert_eq!(
        oneof_names(observation, "AcceleratorObservation"),
        ["result"]
    );
    assert_eq!(field_number(observation, "AcceleratorFactsSet", "facts"), 1);
    assert_eq!(
        field_type_name(observation, "AcceleratorFactsSet", "facts"),
        ".dotcore.observation.v1.AcceleratorFacts"
    );
    assert_eq!(field_number(observation, "AcceleratorFacts", "vendor"), 1);
    assert_eq!(field_number(observation, "AcceleratorFacts", "model"), 2);
    assert_eq!(
        field_number(observation, "AcceleratorFacts", "total_memory_bytes"),
        3
    );
    assert!(
        field(observation, "AcceleratorFacts", "total_memory_bytes")
            .proto3_optional
            .expect("total memory bytes presence")
    );
    assert_eq!(
        field_number(observation, "AcceleratorFacts", "driver_or_runtime_version"),
        4
    );
    assert_eq!(
        field_number(observation, "AcceleratorFacts", "device_id"),
        5
    );
    assert_eq!(
        field_number(observation, "AcceleratorFacts", "used_memory_bytes"),
        6
    );
    assert!(
        field(observation, "AcceleratorFacts", "used_memory_bytes")
            .proto3_optional
            .expect("used memory bytes presence")
    );
    assert_eq!(
        field_names(observation, "RuntimeObservation"),
        [
            "execution_target_id",
            "kind",
            "available",
            "unsupported",
            "unavailable"
        ]
    );
    assert_eq!(
        field_number(observation, "RuntimeObservation", "execution_target_id"),
        1
    );
    assert_eq!(field_number(observation, "RuntimeObservation", "kind"), 2);
    assert_eq!(
        field_type_name(observation, "RuntimeObservation", "kind"),
        ".dotcore.observation.v1.RuntimeKind"
    );
    assert_eq!(
        field_number(observation, "RuntimeObservation", "available"),
        3
    );
    assert_eq!(
        field_number(observation, "RuntimeObservation", "unsupported"),
        4
    );
    assert_eq!(
        field_number(observation, "RuntimeObservation", "unavailable"),
        5
    );
    assert_eq!(oneof_names(observation, "RuntimeObservation"), ["result"]);
    let runtime_kind = observation
        .enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some("RuntimeKind"))
        .expect("RuntimeKind enum must exist");
    assert_eq!(
        runtime_kind
            .value
            .iter()
            .map(|value| (
                value.name.as_deref().expect("enum value name"),
                value.number.expect("enum value number"),
            ))
            .collect::<Vec<_>>(),
        [
            ("RUNTIME_KIND_UNSPECIFIED", 0),
            ("RUNTIME_KIND_MODEL", 1),
            ("RUNTIME_KIND_APPLICATION", 2),
        ]
    );
    assert_message_absent(observation, "NvidiaObservation");
    assert_message_absent(observation, "NvidiaFacts");
    let vendor = observation
        .enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some("AcceleratorVendor"))
        .expect("AcceleratorVendor enum must exist");
    assert_eq!(
        vendor
            .value
            .iter()
            .map(|value| {
                (
                    value.name.as_deref().expect("enum value name"),
                    value.number.expect("enum value number"),
                )
            })
            .collect::<Vec<_>>(),
        [
            ("ACCELERATOR_VENDOR_UNSPECIFIED", 0),
            ("ACCELERATOR_VENDOR_NVIDIA", 1),
            ("ACCELERATOR_VENDOR_AMD", 2),
            ("ACCELERATOR_VENDOR_APPLE", 3),
        ]
    );
    assert!(has_service(
        site_service,
        "SiteService",
        &[
            (
                "ListMachines",
                ".dotcore.site.v1.ListMachinesRequest",
                ".dotcore.site.v1.ListMachinesResponse",
            ),
            (
                "ListConfiguredExecutionTargets",
                ".dotcore.site.v1.ListConfiguredExecutionTargetsRequest",
                ".dotcore.site.v1.ListConfiguredExecutionTargetsResponse",
            ),
            (
                "ListHardwareSnapshots",
                ".dotcore.site.v1.ListHardwareSnapshotsRequest",
                ".dotcore.site.v1.ListHardwareSnapshotsResponse",
            ),
        ]
    ));
}

#[test]
fn accelerator_observation_contract_round_trips_vendors_optional_memory_and_oneof() {
    let nvidia_facts = AcceleratorObservation {
        vendor: AcceleratorVendor::Nvidia as i32,
        result: Some(accelerator_observation::Result::Facts(
            AcceleratorFactsSet {
                facts: vec![
                    accelerator_facts(
                        AcceleratorVendor::Nvidia,
                        "GPU-5090",
                        "NVIDIA GeForce RTX 5090",
                        dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated,
                        Some(32_762_152_448),
                        Some(4_294_967_296),
                        "580.81.06",
                    ),
                    accelerator_facts(
                        AcceleratorVendor::Nvidia,
                        "GPU-GB10",
                        "NVIDIA GB10",
                        dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Unified,
                        None,
                        None,
                        "580.81.06",
                    ),
                ],
            },
        )),
    };
    let amd_unavailable = AcceleratorObservation {
        vendor: AcceleratorVendor::Amd as i32,
        result: Some(accelerator_observation::Result::Unavailable(
            ObservationUnavailable {
                code: "UNSUPPORTED".to_owned(),
                message: "AMD observation is unavailable because this host does not support it."
                    .to_owned(),
            },
        )),
    };

    let decoded_nvidia =
        AcceleratorObservation::decode(&mut nvidia_facts.encode_to_vec().as_slice())
            .expect("the facts observation must round-trip");
    assert_eq!(decoded_nvidia, nvidia_facts);
    let accelerator_observation::Result::Facts(decoded_set) =
        decoded_nvidia.result.expect("facts result")
    else {
        panic!("the facts observation must keep its facts result");
    };
    assert_eq!(decoded_set.facts.len(), 2);
    assert_eq!(
        decoded_set.facts[0].total_memory_bytes,
        Some(32_762_152_448)
    );
    assert_eq!(decoded_set.facts[1].total_memory_bytes, None);
    assert_eq!(
        decoded_set.facts[1].memory_architecture(),
        dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Unified
    );

    let decoded_amd =
        AcceleratorObservation::decode(&mut amd_unavailable.encode_to_vec().as_slice())
            .expect("the unavailable observation must round-trip");
    assert_eq!(decoded_amd, amd_unavailable);
}

#[test]
fn source_descriptor_preserves_the_canonical_unavailable_reason() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let site_service = descriptor_file(&descriptor, "dotcore/site/v1/service.proto");

    let (code, message) = field_leading_comment(
        site_service,
        "CurrentHardwareSnapshotAvailability",
        "unavailable",
    )
    .trim()
    .split_once(": ")
    .expect("unavailable source comment must separate its code and message");

    assert_eq!(code, "CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE");
    assert_eq!(
        message,
        "A current hardware snapshot is unavailable for this machine."
    );
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
}

fn message<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
) -> &'a prost_types::DescriptorProto {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
}

fn assert_message_absent(file: &FileDescriptorProto, message_name: &str) {
    assert!(
        file.message_type
            .iter()
            .all(|message| message.name.as_deref() != Some(message_name)),
        "{message_name} message must be absent"
    );
}

fn field<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a prost_types::FieldDescriptorProto {
    message(file, message_name)
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some(field_name))
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must exist"))
}

fn field_leading_comment<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a str {
    let message_index = file
        .message_type
        .iter()
        .position(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"));
    let field_index = message(file, message_name)
        .field
        .iter()
        .position(|field| field.name.as_deref() == Some(field_name))
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must exist"));
    let path = [4, message_index as i32, 2, field_index as i32];

    file.source_code_info
        .as_ref()
        .expect("descriptor must include source information")
        .location
        .iter()
        .find(|location| location.path == path)
        .and_then(|location| location.leading_comments.as_deref())
        .unwrap_or_else(|| panic!("{message_name}.{field_name} field must have a source comment"))
}

fn assert_field_absent(file: &FileDescriptorProto, message_name: &str, field_name: &str) {
    assert!(
        message(file, message_name)
            .field
            .iter()
            .all(|field| field.name.as_deref() != Some(field_name)),
        "{message_name}.{field_name} field must be absent"
    );
}

fn field_number(file: &FileDescriptorProto, message_name: &str, field_name: &str) -> i32 {
    field(file, message_name, field_name)
        .number
        .expect("field number")
}

fn field_type_name<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
    field_name: &str,
) -> &'a str {
    field(file, message_name, field_name)
        .type_name
        .as_deref()
        .expect("message field type name")
}

fn field_oneof_index(file: &FileDescriptorProto, message_name: &str, field_name: &str) -> i32 {
    field(file, message_name, field_name)
        .oneof_index
        .expect("oneof index")
}

fn field_names<'a>(file: &'a FileDescriptorProto, message_name: &str) -> Vec<&'a str> {
    message(file, message_name)
        .field
        .iter()
        .map(|field| field.name.as_deref().expect("field name"))
        .collect()
}

fn oneof_names<'a>(file: &'a FileDescriptorProto, message_name: &str) -> Vec<&'a str> {
    message(file, message_name)
        .oneof_decl
        .iter()
        .map(|oneof| oneof.name.as_deref().expect("oneof name"))
        .collect()
}

fn has_service(file: &FileDescriptorProto, name: &str, methods: &[(&str, &str, &str)]) -> bool {
    file.service
        .iter()
        .find(|service| service.name.as_deref() == Some(name))
        .is_some_and(|service| {
            service.method.len() == methods.len()
                && service.method.iter().zip(methods).all(
                    |(method, (name, input_type, output_type))| {
                        method.name.as_deref() == Some(*name)
                            && method.input_type.as_deref() == Some(*input_type)
                            && method.output_type.as_deref() == Some(*output_type)
                    },
                )
        })
}

fn accelerator_facts(
    vendor: AcceleratorVendor,
    device_id: &str,
    model: &str,
    memory_architecture: dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture,
    total_memory_bytes: Option<u64>,
    used_memory_bytes: Option<u64>,
    driver_or_runtime_version: &str,
) -> AcceleratorFacts {
    AcceleratorFacts {
        vendor: vendor as i32,
        model: model.to_owned(),
        memory_architecture: memory_architecture as i32,
        total_memory_bytes,
        driver_or_runtime_version: driver_or_runtime_version.to_owned(),
        device_id: device_id.to_owned(),
        vendor_index: None,
        used_memory_bytes,
        working_set_limit_bytes: None,
        supported_signals: Vec::new(),
    }
}
