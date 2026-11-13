//! Application protobuf contract tests.
//!
//! These tests pin generated application graph, deployment-state, descriptor,
//! and public/private field boundaries. Breakage changes application clients or
//! persisted current-state interpretation. Catalog declaration validation,
//! controller lifecycle, Docker behavior, and transport serving are
//! intentionally excluded and covered by their owning suites.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        application::v1::{
            ApplicationConfigurationAttachment, ApplicationDeployment,
            ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
            ApplicationDeploymentMetadata, ApplicationDeploymentState, ApplicationLifecycleState,
            ApplicationReadinessState, ApplicationSelectedInferenceRoute,
            ApplicationServiceAcceleratorMemory, CreateApplicationDeploymentRequest,
            ListApplicationDeploymentsResponse, PauseDeploymentRequest, PauseDeploymentResponse,
            ResolvedApplicationDeploymentSnapshot, ResolvedApplicationServiceSelection,
            ResumeDeploymentRequest, ResumeDeploymentResponse,
            UpdateApplicationDeploymentMetadataRequest,
            UpdateApplicationDeploymentMetadataResponse,
        },
        catalog::v1::{
            Application, ApplicationConfigurationBinding, ApplicationDependencyCondition,
            ApplicationDeploymentInput, ApplicationEndpointProtocol,
            ApplicationInferenceRouteInput, ApplicationInferenceRouteProjection,
            ListApplicationsResponse, application_configuration_binding,
        },
        deployment::v1::{
            AcceleratorMemoryObservation, ArtifactPreparationPolicy, NodeResolvedLaunchIdentity,
            accelerator_memory_observation,
        },
        site::{store::v1::SiteCurrentState, v1::Site},
    },
    google::protobuf::{Empty, FieldMask},
};
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

#[test]
fn generated_bindings_expose_application_deployment_names_and_optional_create_names() {
    let declaration = ApplicationDeploymentDeclaration {
        application_deployment_id: "app-01".to_owned(),
        application_deployment_name: "Research Console".to_owned(),
        ..Default::default()
    };
    assert_eq!(
        ApplicationDeploymentDeclaration::decode(declaration.encode_to_vec().as_slice())
            .expect("the named desired application deployment fixture must decode"),
        declaration
    );

    let deployment = ApplicationDeployment {
        application_deployment_id: "app-01".to_owned(),
        application_deployment_name: "Research Console".to_owned(),
        ..Default::default()
    };
    assert_eq!(
        ApplicationDeployment::decode(deployment.encode_to_vec().as_slice())
            .expect("the named public application deployment fixture must decode"),
        deployment
    );

    let omitted_name = CreateApplicationDeploymentRequest::default();
    let submitted_name = CreateApplicationDeploymentRequest {
        application_deployment_name: Some("Research Console".to_owned()),
        ..Default::default()
    };
    assert_eq!(omitted_name.application_deployment_name, None);
    assert_eq!(
        CreateApplicationDeploymentRequest::decode(submitted_name.encode_to_vec().as_slice())
            .expect("the explicitly named application create fixture must decode")
            .application_deployment_name,
        Some("Research Console".to_owned())
    );
}

#[test]
fn generated_bindings_preserve_application_metadata_update_and_accepted_deployment() {
    let request = UpdateApplicationDeploymentMetadataRequest {
        application_deployment_id: "app-01".to_owned(),
        metadata: Some(ApplicationDeploymentMetadata {
            display_name: Some("Renamed Console".to_owned()),
        }),
        update_mask: Some(FieldMask {
            paths: vec!["display_name".to_owned()],
        }),
    };
    assert_eq!(
        UpdateApplicationDeploymentMetadataRequest::decode(request.encode_to_vec().as_slice())
            .expect("the application metadata update fixture must decode"),
        request
    );

    let response = UpdateApplicationDeploymentMetadataResponse {
        deployment: Some(ApplicationDeployment {
            application_deployment_id: "app-01".to_owned(),
            application_deployment_name: "Renamed Console".to_owned(),
            ..Default::default()
        }),
    };
    assert_eq!(
        UpdateApplicationDeploymentMetadataResponse::decode(response.encode_to_vec().as_slice())
            .expect("the application metadata response fixture must decode"),
        response
    );
}

#[test]
fn generated_bindings_preserve_application_graph_and_deployment_state() {
    let application = Application {
        application_id: "open-webui-0.6.18".to_owned(),
        ..Default::default()
    };
    let deployments = ListApplicationDeploymentsResponse {
        deployments: vec![ApplicationDeployment {
            application_deployment_id: "app-01".to_owned(),
            application_deployment_name: "open-webui-brisk-otter".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "local-apps".to_owned(),
                execution_target_id: "application-target".to_owned(),
            }),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                application_id: application.application_id.clone(),
                display_name: "Open WebUI".to_owned(),
                version: "1.0.0".to_owned(),
                notes: String::new(),
                graph: application.graph.clone(),
                placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                    site_node_id: "local-apps".to_owned(),
                    execution_target_id: "application-target".to_owned(),
                }),
                platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                }),
                accelerator_device_ids: Vec::new(),
                service_selections: vec![ResolvedApplicationServiceSelection {
                    service_name: "web".to_owned(),
                    preparation_policy: ArtifactPreparationPolicy::ImmutableRemote as i32,
                    launch_identity: "example.test/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                }],
                parameter_values: Vec::new(),
                generated_parameter_ids: Vec::new(),
            }),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            state: Some(ApplicationDeploymentState {
                lifecycle_state: ApplicationLifecycleState::Ready as i32,
                readiness_state: ApplicationReadinessState::Ready as i32,
                ..Default::default()
            }),
            storage_attachments: Vec::new(),
            selected_inference_route: Some(ApplicationSelectedInferenceRoute {
                inference_route_id: "route-1".to_owned(),
                route_name: "gpu-serving".to_owned(),
            }),
            node_resolved_launch_identities: vec![NodeResolvedLaunchIdentity {
                selection_key: "web".to_owned(),
                launch_identity:
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_owned(),
            }],
            service_accelerator_memory: vec![ApplicationServiceAcceleratorMemory {
                service_name: "web".to_owned(),
                accelerator_memory: Some(AcceleratorMemoryObservation {
                    result: Some(accelerator_memory_observation::Result::NotApplicable(
                        Empty {},
                    )),
                    ..Default::default()
                }),
            }],
        }],
    };

    assert_eq!(
        ListApplicationDeploymentsResponse::decode(deployments.encode_to_vec().as_slice())
            .expect("the application deployment list fixture must decode"),
        deployments,
    );
    let desired = ApplicationDeploymentDeclaration {
        application_deployment_id: "app-01".to_owned(),
        configuration_attachments: vec![ApplicationConfigurationAttachment {
            service_name: "web".to_owned(),
            binding_name: "provider".to_owned(),
            environment_variable: "PROVIDER_URL".to_owned(),
            value: "http://192.0.2.10:6470/v1".to_owned(),
        }],
        inference_route_id: Some("route-1".to_owned()),
        ..Default::default()
    };
    assert_eq!(
        ApplicationDeploymentDeclaration::decode(desired.encode_to_vec().as_slice())
            .expect("the private desired application deployment fixture must decode"),
        desired
    );
    assert_eq!(
        ApplicationConfigurationBinding {
            binding_name: "provider".to_owned(),
            environment_variable: "PROVIDER_URL".to_owned(),
            source: Some(application_configuration_binding::Source::LiteralValue(
                String::new()
            )),
            input: Some(ApplicationDeploymentInput {
                kind: Some(
                    dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                        ApplicationInferenceRouteInput {
                            projection: ApplicationInferenceRouteProjection::RouteName as i32,
                        },
                    ),
                ),
            }),
        }
        .source,
        Some(application_configuration_binding::Source::LiteralValue(
            String::new()
        ))
    );
    let request = CreateApplicationDeploymentRequest {
        application_id: "open-webui".to_owned(),
        placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
            site_node_id: "node-one".to_owned(),
            execution_target_id: "application-target".to_owned(),
        }),
        inference_route_id: Some("route-id".to_owned()),
        expected_inference_route_generation: 7,
        ..Default::default()
    };
    assert_eq!(
        CreateApplicationDeploymentRequest::decode(request.encode_to_vec().as_slice())
            .expect("the application deployment create request must decode"),
        request
    );
    assert_eq!(
        ListApplicationsResponse::decode(
            ListApplicationsResponse {
                applications: vec![application],
            }
            .encode_to_vec()
            .as_slice(),
        )
        .expect("the catalog application list fixture must decode")
        .applications
        .len(),
        1,
    );
}

#[test]
fn generated_bindings_preserve_application_pause_values_and_accepted_deployments() {
    let pausing = ApplicationDeployment {
        desired_state: ApplicationDeploymentDesiredState::Paused as i32,
        state: Some(ApplicationDeploymentState {
            lifecycle_state: ApplicationLifecycleState::Pausing as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let paused = ApplicationDeployment {
        desired_state: ApplicationDeploymentDesiredState::Paused as i32,
        state: Some(ApplicationDeploymentState {
            lifecycle_state: ApplicationLifecycleState::Paused as i32,
            ..Default::default()
        }),
        ..Default::default()
    };

    for deployment in [pausing, paused] {
        assert_eq!(
            ApplicationDeployment::decode(deployment.encode_to_vec().as_slice())
                .expect("the application pause deployment fixture must decode"),
            deployment,
        );
    }

    let pause_response = PauseDeploymentResponse {
        deployment: Some(ApplicationDeployment {
            desired_state: ApplicationDeploymentDesiredState::Paused as i32,
            ..Default::default()
        }),
    };
    assert_eq!(
        PauseDeploymentResponse::decode(pause_response.encode_to_vec().as_slice())
            .expect("the application pause response fixture must decode"),
        pause_response,
    );
    let resume_response = ResumeDeploymentResponse {
        deployment: Some(ApplicationDeployment {
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            ..Default::default()
        }),
    };
    assert_eq!(
        ResumeDeploymentResponse::decode(resume_response.encode_to_vec().as_slice())
            .expect("the application resume response fixture must decode"),
        resume_response,
    );

    let pause_request = PauseDeploymentRequest {
        application_deployment_id: "app-01".to_owned(),
    };
    assert_eq!(
        PauseDeploymentRequest::decode(pause_request.encode_to_vec().as_slice())
            .expect("the application pause request fixture must decode"),
        pause_request,
    );
    let resume_request = ResumeDeploymentRequest {
        application_deployment_id: "app-01".to_owned(),
    };
    assert_eq!(
        ResumeDeploymentRequest::decode(resume_request.encode_to_vec().as_slice())
            .expect("the application resume request fixture must decode"),
        resume_request,
    );
}

#[test]
fn application_descriptors_define_portable_graph_and_owner_separated_records() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode");
    let catalog = descriptor_file(&descriptor, "dotcore/catalog/v1/application.proto");
    let service = descriptor_file(&descriptor, "dotcore/application/v1/service.proto");
    let site = descriptor_file(&descriptor, "dotcore/site/v1/site.proto");
    let current = descriptor_file(&descriptor, "dotcore/site/store/v1/snapshot.proto");

    assert_eq!(
        field_names(catalog, "ApplicationService"),
        [
            "service_name",
            "artifact",
            "command",
            "arguments",
            "ports",
            "configuration_bindings",
            "storage_bindings",
            "secret_bindings",
            "dependencies",
            "readiness",
        ]
    );
    assert_eq!(
        enum_value_names(catalog, "ApplicationDependencyCondition"),
        [
            "APPLICATION_DEPENDENCY_CONDITION_UNSPECIFIED",
            "APPLICATION_DEPENDENCY_CONDITION_STARTED",
            "APPLICATION_DEPENDENCY_CONDITION_READY",
        ]
    );
    assert_eq!(
        enum_value_names(catalog, "ApplicationEndpointProtocol"),
        [
            "APPLICATION_ENDPOINT_PROTOCOL_UNSPECIFIED",
            "APPLICATION_ENDPOINT_PROTOCOL_HTTP",
        ]
    );
    assert_service_methods(
        service,
        "ApplicationDeploymentService",
        &[
            (
                "ListApplicationDeployments",
                "ListApplicationDeploymentsRequest",
                "ListApplicationDeploymentsResponse",
            ),
            (
                "GetApplicationDeployment",
                "GetApplicationDeploymentRequest",
                "GetApplicationDeploymentResponse",
            ),
            (
                "CreateApplicationDeployment",
                "CreateApplicationDeploymentRequest",
                "CreateApplicationDeploymentResponse",
            ),
            (
                "RemoveApplicationDeployment",
                "RemoveApplicationDeploymentRequest",
                "RemoveApplicationDeploymentResponse",
            ),
            (
                "PauseDeployment",
                "PauseDeploymentRequest",
                "PauseDeploymentResponse",
            ),
            (
                "ResumeDeployment",
                "ResumeDeploymentRequest",
                "ResumeDeploymentResponse",
            ),
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
                "UpdateApplicationDeploymentMetadata",
                "UpdateApplicationDeploymentMetadataRequest",
                "UpdateApplicationDeploymentMetadataResponse",
            ),
        ],
    );
    // The application contracts share types through owner-neutral packages, never the model
    // package.
    for file in [
        "dotcore/catalog/v1/application.proto",
        "dotcore/application/v1/deployment.proto",
        "dotcore/application/v1/service.proto",
        "dotcore/application/v1/state.proto",
    ] {
        let dependencies = &descriptor_file(&descriptor, file).dependency;
        assert!(
            dependencies
                .iter()
                .all(|dependency| !dependency.starts_with("dotcore/model/")
                    && dependency != "dotcore/catalog/v1/model.proto"),
            "{file} imports {dependencies:?}"
        );
    }
    assert_eq!(
        field_names(site, "Site"),
        [
            "site_id",
            "execution_targets",
            "model_deployments",
            "nodes",
            "application_deployments",
            "inference_routes",
            "hardware_observation",
            "model_deployment_identity_allocator",
        ]
    );
    let deployment = descriptor_file(&descriptor, "dotcore/application/v1/deployment.proto");
    let model_deployment = descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto");
    assert_eq!(
        field_names(catalog, "ApplicationConfigurationBinding"),
        [
            "binding_name",
            "environment_variable",
            "literal_value",
            "resolved_value_name",
            "parameter_id",
            "input",
        ]
    );
    assert_eq!(
        field_names(catalog, "ApplicationDeploymentInput"),
        ["inference_route"]
    );
    assert_eq!(
        field_names(catalog, "ApplicationInferenceRouteInput"),
        ["projection"]
    );
    assert_eq!(
        enum_value_names(catalog, "ApplicationInferenceRouteProjection"),
        [
            "APPLICATION_INFERENCE_ROUTE_PROJECTION_UNSPECIFIED",
            "APPLICATION_INFERENCE_ROUTE_PROJECTION_ROUTE_NAME",
        ]
    );
    assert_eq!(
        field_names(service, "CreateApplicationDeploymentRequest"),
        [
            "application_id",
            "placement",
            "inference_route_id",
            "expected_inference_route_generation",
            "application_deployment_name",
            "parameter_values",
            "storage_choices",
        ]
    );
    assert_eq!(
        field_names(service, "ApplicationStorageChoice"),
        ["service_name", "binding_name", "placement"]
    );
    assert_eq!(
        field_type_name(service, "ApplicationStorageChoice", "placement"),
        ".dotcore.catalog.v1.StoragePlacement"
    );
    assert_eq!(
        field_names(deployment, "ApplicationStorageAttachment"),
        ["service_name", "binding_name", "storage_name", "placement"]
    );
    assert_eq!(
        field_names(deployment, "ApplicationDeploymentDeclaration"),
        [
            "application_deployment_id",
            "application_deployment_name",
            "placement",
            "snapshot",
            "desired_state",
            "storage_attachments",
            "configuration_attachments",
            "secret_attachments",
            "inference_route_id",
            "lifecycle_generation",
            "node_resolved_launch_identities",
            "published_port",
        ]
    );
    assert_eq!(
        field_type_name(deployment, "ApplicationDeploymentDeclaration", "snapshot"),
        ".dotcore.application.v1.ResolvedApplicationDeploymentSnapshot"
    );
    assert_eq!(
        field_names(deployment, "ApplicationDeployment"),
        [
            "application_deployment_id",
            "application_deployment_name",
            "placement",
            "snapshot",
            "desired_state",
            "state",
            "storage_attachments",
            "selected_inference_route",
            "node_resolved_launch_identities",
            "service_accelerator_memory",
        ]
    );
    assert_eq!(
        field_names(deployment, "ApplicationServiceAcceleratorMemory"),
        ["service_name", "accelerator_memory"]
    );
    assert_eq!(
        field_type_name(
            deployment,
            "ApplicationServiceAcceleratorMemory",
            "accelerator_memory"
        ),
        ".dotcore.deployment.v1.AcceleratorMemoryObservation"
    );
    for message_name in ["ApplicationDeploymentDeclaration", "ApplicationDeployment"] {
        assert_eq!(
            field_type_name(deployment, message_name, "node_resolved_launch_identities"),
            ".dotcore.deployment.v1.NodeResolvedLaunchIdentity",
            "{message_name} keeps node-resolved identities in the shared deployment type"
        );
    }
    assert_eq!(
        field_names(deployment, "ResolvedApplicationDeploymentSnapshot"),
        [
            "application_id",
            "display_name",
            "version",
            "notes",
            "graph",
            "parameter_values",
            "generated_parameter_ids",
            "placement",
            "accelerator_device_ids",
            "platform",
            "service_selections",
        ]
    );
    assert_eq!(
        field_names(deployment, "ResolvedApplicationServiceSelection"),
        ["service_name", "preparation_policy", "launch_identity",]
    );
    assert_eq!(
        field_type_name(
            deployment,
            "ResolvedApplicationServiceSelection",
            "preparation_policy"
        ),
        ".dotcore.deployment.v1.ArtifactPreparationPolicy"
    );
    assert!(
        deployment.enum_type.iter().all(|enum_type| {
            enum_type.name.as_deref() != Some("ApplicationArtifactPreparationPolicy")
        }),
        "the preparation policy is shared, not application-owned"
    );
    assert_eq!(
        enum_values(
            descriptor_file(&descriptor, "dotcore/deployment/v1/artifact.proto"),
            "ArtifactPreparationPolicy"
        ),
        [
            ("ARTIFACT_PREPARATION_POLICY_UNSPECIFIED", 0),
            ("ARTIFACT_PREPARATION_POLICY_IMMUTABLE_REMOTE", 1),
            ("ARTIFACT_PREPARATION_POLICY_LOCAL_DEVELOPMENT", 2),
            ("ARTIFACT_PREPARATION_POLICY_DEVELOPMENT_SOURCE", 3),
        ]
    );
    assert_eq!(
        field_names(deployment, "ApplicationSelectedInferenceRoute"),
        ["inference_route_id", "route_name"]
    );
    assert!(
        !field_names(deployment, "ApplicationDeployment").contains(&"configuration_attachments"),
        "resolved configuration must remain private desired state"
    );
    assert!(
        !field_names(deployment, "ApplicationDeployment").contains(&"secret_attachments"),
        "secret attachments must remain private desired state"
    );
    assert_eq!(
        field_names(current, "SiteCurrentState"),
        [
            "format_version",
            "site_id",
            "storage_revision",
            "model_deployments",
            "application_deployments",
            "manager_model_lifecycle_records",
            "manager_application_lifecycle_records",
        ]
    );
    assert_eq!(
        field_names(current, "NodeCurrentState"),
        [
            "format_version",
            "site_id",
            "site_node_id",
            "storage_revision",
            "model_deployments",
            "application_deployments",
            "model_cleanup_evidence",
            "model_resolved_launch_identities",
            "application_realizations",
            "application_cleanup_evidence",
        ],
        "node custody state lives in its own snapshot, apart from owner records"
    );

    assert_eq!(
        ApplicationDependencyCondition::Started as i32,
        1,
        "started dependency condition must keep its stable wire value"
    );
    assert_eq!(ApplicationEndpointProtocol::Http as i32, 1);
    assert_eq!(
        enum_values(deployment, "ApplicationDeploymentDesiredState"),
        [
            ("APPLICATION_DEPLOYMENT_DESIRED_STATE_UNSPECIFIED", 0),
            ("APPLICATION_DEPLOYMENT_DESIRED_STATE_PRESENT", 1),
            ("APPLICATION_DEPLOYMENT_DESIRED_STATE_TEARDOWN", 2),
            ("APPLICATION_DEPLOYMENT_DESIRED_STATE_PAUSED", 3),
        ]
    );
    assert!(
        !enum_value_names(deployment, "ApplicationDeploymentDesiredState")
            .iter()
            .any(|name| name.contains("ABSENT"))
    );
    assert_eq!(
        enum_values(model_deployment, "ModelDeploymentDesiredState"),
        [
            ("MODEL_DEPLOYMENT_DESIRED_STATE_UNSPECIFIED", 0),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_PRESENT", 1),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_TEARDOWN", 2),
            ("MODEL_DEPLOYMENT_DESIRED_STATE_PAUSED", 3),
        ]
    );
    assert!(
        !enum_value_names(model_deployment, "ModelDeploymentDesiredState")
            .iter()
            .any(|name| name.contains("ABSENT"))
    );
    assert!(Site::default().application_deployments.is_empty());
    assert!(
        SiteCurrentState::default()
            .application_deployments
            .is_empty()
    );
    let application_state = descriptor_file(&descriptor, "dotcore/application/v1/state.proto");
    assert_eq!(
        enum_values(application_state, "ApplicationLifecycleState"),
        [
            ("APPLICATION_LIFECYCLE_STATE_UNSPECIFIED", 0),
            ("APPLICATION_LIFECYCLE_STATE_QUEUED", 1),
            ("APPLICATION_LIFECYCLE_STATE_PREPARING", 2),
            ("APPLICATION_LIFECYCLE_STATE_STARTING", 3),
            ("APPLICATION_LIFECYCLE_STATE_RUNNING", 4),
            ("APPLICATION_LIFECYCLE_STATE_READY", 5),
            ("APPLICATION_LIFECYCLE_STATE_STOPPING", 6),
            ("APPLICATION_LIFECYCLE_STATE_STOPPED", 7),
            ("APPLICATION_LIFECYCLE_STATE_FAILED", 8),
            ("APPLICATION_LIFECYCLE_STATE_UNAVAILABLE", 9),
            ("APPLICATION_LIFECYCLE_STATE_PAUSING", 10),
            ("APPLICATION_LIFECYCLE_STATE_PAUSED", 11),
        ]
    );
    assert_eq!(
        field_names(application_state, "ApplicationServiceObservation"),
        ["service_name", "lifecycle_state", "readiness_state"],
        "live unit memory never enters retained state"
    );
    assert_eq!(
        field_numbers(service, "PauseDeploymentRequest"),
        [("application_deployment_id", 1)]
    );
    assert_eq!(
        field_numbers(service, "PauseDeploymentResponse"),
        [("deployment", 1)]
    );
    assert_eq!(
        field_numbers(service, "ResumeDeploymentRequest"),
        [("application_deployment_id", 1)]
    );
    assert_eq!(
        field_numbers(service, "ResumeDeploymentResponse"),
        [("deployment", 1)]
    );
    assert_eq!(
        field_names(service, "ReadDeploymentLogsRequest"),
        [
            "application_deployment_id",
            "source_names",
            "page_size",
            "before_timestamp",
        ]
    );
    assert_eq!(
        field_names(service, "FollowDeploymentLogsRequest"),
        [
            "application_deployment_id",
            "source_names",
            "resume_positions",
        ]
    );
    assert_eq!(
        field_names(service, "DeploymentLogEntry"),
        [
            "source_name",
            "source_label",
            "stream",
            "timestamp",
            "content",
            "truncated",
        ]
    );
    assert_eq!(
        field_numbers(service, "UpdateApplicationDeploymentMetadataRequest"),
        [
            ("application_deployment_id", 1),
            ("metadata", 2),
            ("update_mask", 3),
        ]
    );
    assert_eq!(
        field_numbers(service, "ApplicationDeploymentMetadata"),
        [("display_name", 1)]
    );
    assert_eq!(
        field_numbers(service, "UpdateApplicationDeploymentMetadataResponse"),
        [("deployment", 1)]
    );
    assert!(
        field_is_proto3_optional(service, "ApplicationDeploymentMetadata", "display_name"),
        "metadata display_name must preserve scalar presence"
    );
    assert_eq!(
        field_type_name(
            service,
            "UpdateApplicationDeploymentMetadataRequest",
            "update_mask"
        ),
        ".google.protobuf.FieldMask"
    );
    assert_message_absent(service, "RenameApplicationDeploymentRequest");
    assert_message_absent(service, "RenameApplicationDeploymentResponse");
    assert_service_method_absent(
        service,
        "ApplicationDeploymentService",
        "RenameApplicationDeployment",
    );
}

#[test]
fn public_application_contracts_exclude_runtime_native_details() {
    let contracts = [
        include_str!("../dotcore/catalog/v1/application.proto"),
        include_str!("../dotcore/application/v1/deployment.proto"),
        include_str!("../dotcore/application/v1/state.proto"),
        include_str!("../dotcore/application/v1/service.proto"),
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
                "public application contract must exclude {private_name} outside log vocabulary"
            );
        }
    }
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
}

fn field_names<'a>(file: &'a FileDescriptorProto, message_name: &str) -> Vec<&'a str> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .map(|field| field.name.as_deref().expect("field name"))
        .collect()
}

fn enum_value_names<'a>(file: &'a FileDescriptorProto, enum_name: &str) -> Vec<&'a str> {
    file.enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some(enum_name))
        .unwrap_or_else(|| panic!("{enum_name} enum must exist"))
        .value
        .iter()
        .map(|value| value.name.as_deref().expect("enum value name"))
        .collect()
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
