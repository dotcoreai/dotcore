//! Remote endpoint protobuf contract tests.
//!
//! These tests pin the OpenAI-compatible upstream model template and the
//! source-specific model deployment contracts. Catalog loading, persistence,
//! reconciliation, and routing are intentionally excluded.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        catalog::v1::{UpstreamModelBoundary, UpstreamModelDataEgressClass, UpstreamModelLocality},
        model::v1::{OpenAiCompatibleEndpointConfiguration, UpstreamModelDesiredConfiguration},
    },
};
use prost::Message;
use prost_types::{
    FileDescriptorProto, FileDescriptorSet, field_descriptor_proto::Type as FieldType,
};

#[test]
fn upstream_model_template_contract_has_no_named_service_variants() {
    let descriptor = descriptor_set();
    let catalog = descriptor_file(
        &descriptor,
        "dotcore/catalog/v1/upstream_model_template.proto",
    );
    let service = descriptor_file(&descriptor, "dotcore/catalog/v1/service.proto");

    assert_eq!(
        field_layout(catalog, "UpstreamModelTemplate"),
        [
            ("upstream_model_template_id", 1, "TYPE_STRING"),
            ("display_name", 2, "TYPE_STRING"),
            (
                "wire_protocol",
                4,
                ".dotcore.catalog.v1.UpstreamWireProtocol",
            ),
        ]
    );
    assert_eq!(
        enum_values(catalog, "UpstreamWireProtocol"),
        [
            ("UPSTREAM_WIRE_PROTOCOL_UNSPECIFIED", 0),
            ("UPSTREAM_WIRE_PROTOCOL_OPENAI_COMPATIBLE", 1),
        ]
    );
    assert!(
        catalog.message_type.iter().all(|message| !message
            .name
            .as_deref()
            .is_some_and(|name| name.contains("OpenAi") || name.contains("Custom"))),
        "the upstream model template must not encode named service variants"
    );
    assert!(
        catalog.enum_type.iter().all(|enum_type| !enum_type
            .name
            .as_deref()
            .is_some_and(|name| name.contains("Identity") || name.contains("Health"))),
        "the upstream model template must not encode service identities or health modes"
    );
    assert_service_methods(
        service,
        "CatalogService",
        &[
            ("ListModels", "ListModelsRequest", "ListModelsResponse"),
            (
                "ListEngineReleases",
                "ListEngineReleasesRequest",
                "ListEngineReleasesResponse",
            ),
            (
                "ListApplications",
                "ListApplicationsRequest",
                "ListApplicationsResponse",
            ),
            (
                "ListUpstreamModelTemplates",
                "ListUpstreamModelTemplatesRequest",
                "ListUpstreamModelTemplatesResponse",
            ),
            (
                "ListHardwareClasses",
                "ListHardwareClassesRequest",
                "ListHardwareClassesResponse",
            ),
        ],
    );
}

#[test]
fn generic_endpoint_wire_bytes_preserve_optional_key_and_operator_boundary() {
    let configuration = UpstreamModelDesiredConfiguration {
        upstream_model_id: "private-model".to_owned(),
        api_key: String::new(),
        input_capabilities: None,
        endpoint: Some(OpenAiCompatibleEndpointConfiguration {
            base_url: "https://endpoint.example/v1".to_owned(),
            allow_insecure_http: false,
        }),
        boundary: Some(UpstreamModelBoundary {
            locality: UpstreamModelLocality::Private as i32,
            trust_domain: "example.internal".to_owned(),
            data_egress_class: UpstreamModelDataEgressClass::PrivateNetwork as i32,
        }),
    };

    assert_eq!(
        configuration.encode_to_vec(),
        [
            0x0a, 0x0d, b'p', b'r', b'i', b'v', b'a', b't', b'e', b'-', b'm', b'o', b'd', b'e',
            b'l', 0x2a, 0x1d, 0x0a, 0x1b, b'h', b't', b't', b'p', b's', b':', b'/', b'/', b'e',
            b'n', b'd', b'p', b'o', b'i', b'n', b't', b'.', b'e', b'x', b'a', b'm', b'p', b'l',
            b'e', b'/', b'v', b'1', 0x32, 0x16, 0x08, 0x02, 0x12, 0x10, b'e', b'x', b'a', b'm',
            b'p', b'l', b'e', b'.', b'i', b'n', b't', b'e', b'r', b'n', b'a', b'l', 0x18, 0x02,
        ]
    );
    assert_eq!(
        UpstreamModelDesiredConfiguration::decode(configuration.encode_to_vec().as_slice())
            .expect("the generic endpoint configuration fixture must decode"),
        configuration
    );
}

#[test]
fn source_specific_deployment_contract_keeps_resolved_local_snapshot_and_upstream_configuration() {
    let descriptor = descriptor_set();
    let deployment = descriptor_file(&descriptor, "dotcore/model/v1/deployment.proto");
    let service = descriptor_file(&descriptor, "dotcore/model/v1/service.proto");

    assert_eq!(
        field_layout(deployment, "ModelDeploymentSnapshot"),
        [
            (
                "local",
                1,
                ".dotcore.model.v1.ResolvedLocalModelDeploymentSnapshot",
            ),
            (
                "upstream",
                4,
                ".dotcore.model.v1.UpstreamModelDeploymentSnapshot",
            ),
        ]
    );
    assert_eq!(
        field_oneof_indexes(deployment, "ModelDeploymentSnapshot"),
        [("local", 0), ("upstream", 0)]
    );
    assert_eq!(
        field_layout(deployment, "UpstreamModelDeploymentSnapshot"),
        [(
            "upstream_model_template",
            1,
            ".dotcore.catalog.v1.UpstreamModelTemplate",
        )]
    );
    assert_eq!(
        field_layout(deployment, "UpstreamModelDesiredConfiguration"),
        [
            ("upstream_model_id", 1, "TYPE_STRING"),
            ("api_key", 2, "TYPE_STRING"),
            (
                "input_capabilities",
                4,
                ".dotcore.model.v1.UpstreamModelInputCapabilities",
            ),
            (
                "endpoint",
                5,
                ".dotcore.model.v1.OpenAiCompatibleEndpointConfiguration",
            ),
            ("boundary", 6, ".dotcore.catalog.v1.UpstreamModelBoundary",),
        ]
    );
    assert_eq!(
        field_layout(deployment, "OpenAiCompatibleEndpointConfiguration"),
        [
            ("base_url", 1, "TYPE_STRING"),
            ("allow_insecure_http", 2, "TYPE_BOOL"),
        ]
    );
    assert_eq!(
        field_layout(deployment, "ModelDeployment"),
        [
            ("model_deployment_id", 1, "TYPE_STRING"),
            ("intent", 2, ".dotcore.model.v1.ModelDeploymentIntent"),
            ("snapshot", 3, ".dotcore.model.v1.ModelDeploymentSnapshot"),
            ("state", 4, ".dotcore.model.v1.ModelDeploymentState"),
            (
                "desired_state",
                5,
                ".dotcore.model.v1.ModelDeploymentDesiredState",
            ),
            (
                "upstream_desired_configuration",
                6,
                ".dotcore.model.v1.UpstreamModelDesiredConfiguration",
            ),
            ("model_deployment_name", 7, "TYPE_STRING"),
            (
                "accelerator_memory",
                8,
                ".dotcore.deployment.v1.AcceleratorMemoryObservation",
            ),
            (
                "node_resolved_launch_identities",
                9,
                ".dotcore.deployment.v1.NodeResolvedLaunchIdentity",
            ),
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
}

fn descriptor_set() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("the generated descriptor set must decode")
}

fn descriptor_file<'a>(descriptor: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("descriptor must contain {name}"))
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
                        field_type => panic!("unexpected scalar field type {field_type:?}"),
                    }),
            )
        })
        .collect()
}

fn field_oneof_indexes<'a>(
    file: &'a FileDescriptorProto,
    message_name: &str,
) -> Vec<(&'a str, i32)> {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(message_name))
        .unwrap_or_else(|| panic!("{message_name} message must exist"))
        .field
        .iter()
        .filter_map(|field| Some((field.name.as_deref()?, field.oneof_index?)))
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
