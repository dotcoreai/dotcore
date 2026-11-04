//! Private desired-view protobuf contract tests.
//!
//! These tests pin the node control stream, the generic desired-view envelope and the typed
//! model- and application-owner wire boundaries while ensuring private packages remain outside browser
//! generation. Runtime installation and transport serving are excluded.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        application::{
            private::v1::{
                ApplicationAssignment, ApplicationCleanupDisposition, ApplicationCleanupEvidence,
                ApplicationCleanupReceipt, ApplicationControllerStatusSection,
                ApplicationDeploymentStatus, ApplicationDesiredViewSection,
                ApplicationLifecycleObservation, ApplicationLinkedRouteProjection,
                ApplicationPublishedEndpoint, ApplicationStatusFailureCode, ApplicationStatusStage,
                application_linked_route_projection,
            },
            v1::{
                ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
                ApplicationReadinessState,
            },
        },
        deployment::v1::{ExactPlacement, NodeResolvedLaunchIdentity},
        model::private::v1::{
            ModelAssignment, ModelBindingReadiness, ModelCleanupDisposition, ModelCleanupEvidence,
            ModelCleanupReceipt, ModelControllerStatusSection, ModelDeploymentStatus,
            ModelDesiredViewSection, ModelLifecycleObservation,
        },
        site::private::v1::{
            ControllerStatusReceipt, ControllerStatusSection, DesiredViewSection,
            DesiredViewSectionKind, NodeControlMessageKind, NodeControlOpen, NodeControlOpened,
            NodeControlRejection, NodeControlRejectionCode, NodeControllerStatus,
            NodeControllerStatusAcceptance, NodeDesiredView, NodeViewAcknowledgement,
            NodeViewAcknowledgementAcceptance, NodeViewIdentity, NodeViewReference,
            RequiredDesiredViewSection, StreamNodeControlRequest, StreamNodeControlResponse,
            SupportedDesiredViewSection, controller_status_receipt, controller_status_section,
            desired_view_section, node_view_acknowledgement, stream_node_control_request,
            stream_node_control_response,
        },
    },
    google::protobuf::Empty,
};
use prost::Message;
use prost_types::{FileDescriptorProto, FileDescriptorSet};

#[test]
fn private_descriptor_keeps_desired_views_service_free_with_typed_model_sections() {
    let descriptors =
        FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("generated descriptor set");
    let site = descriptor_file(&descriptors, "dotcore/site/private/v1/desired_view.proto");
    let model = descriptor_file(&descriptors, "dotcore/model/private/v1/controller.proto");

    assert!(
        site.service.is_empty(),
        "desired views travel only on the node control stream"
    );
    assert_eq!(
        field_names(message(model, "ModelAssignment")),
        ["declaration"]
    );
    assert!(
        model
            .message_type
            .iter()
            .all(|message| message.name.as_deref() != Some("ModelResumeAuthorization"))
    );
    assert!(
        message(model, "ModelDeploymentStatus")
            .field
            .iter()
            .all(|field| {
                !matches!(
                    field.name.as_deref(),
                    Some("endpoint" | "runtime_handle" | "process_id")
                )
            })
    );
}

#[test]
fn removed_resume_authorization_receipt_and_sequence_fields_stay_reserved() {
    let descriptors =
        FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("generated descriptor set");
    let model = descriptor_file(&descriptors, "dotcore/model/private/v1/controller.proto");
    let store = descriptor_file(&descriptors, "dotcore/site/store/v1/snapshot.proto");

    for (file, name, numbers) in [
        (model, "ModelAssignment", vec![2]),
        (model, "ModelCleanupEvidence", vec![4]),
        (model, "ModelCleanupReceipt", vec![5, 7]),
        (store, "SiteCurrentState", vec![8]),
        (store, "ManagerLifecycleRecord", vec![3, 4]),
    ] {
        assert_eq!(reserved_numbers(message(file, name)), numbers, "{name}");
    }
}

#[test]
fn private_wire_round_trip_keeps_manifest_independent_and_cleanup_receipt_exact() {
    let view = NodeDesiredView {
        identity: Some(NodeViewIdentity {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "1".repeat(32),
            publication_sequence: 8,
            view_revision: 13,
            content_sha256: "b".repeat(64),
            projection_schema_version: 1,
        }),
        required_sections: vec![RequiredDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
        }],
        sections: vec![DesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
            payload: Some(desired_view_section::Payload::Model(
                ModelDesiredViewSection {
                    assignments: vec![ModelAssignment::default()],
                },
            )),
        }],
        execution_targets: Vec::new(),
    };
    assert_eq!(
        NodeDesiredView::decode(view.encode_to_vec().as_slice()).expect("view round trip"),
        view
    );

    let status = NodeControllerStatus {
        view: Some(NodeViewReference {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "1".repeat(32),
            view_revision: 13,
            content_sha256: "b".repeat(64),
        }),
        status_sequence: 21,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(
                ModelControllerStatusSection {
                    deployments: vec![ModelDeploymentStatus {
                        model_deployment_id: "model-one".to_owned(),
                        lifecycle_generation: 7,
                        realization_generation: Some(9),
                        lifecycle: ModelLifecycleObservation::Ready as i32,
                        binding_readiness: ModelBindingReadiness::Ready as i32,
                        state_revision: 4,
                        ..Default::default()
                    }],
                    cleanup_evidence: vec![ModelCleanupEvidence {
                        model_deployment_id: "model-gone".to_owned(),
                        lifecycle_generation: 12,
                        realization_generation: Some(9),
                        never_realized: false,
                        evidence_sha256: "c".repeat(64),
                    }],
                },
            )),
        }],
    };
    let receipt = ModelCleanupReceipt {
        manager_session_id: "a".repeat(32),
        node_incarnation_id: "1".repeat(32),
        model_deployment_id: "model-gone".to_owned(),
        lifecycle_generation: 12,
        status_sequence: 21,
        evidence_sha256: "c".repeat(64),
        disposition: ModelCleanupDisposition::Acknowledged as i32,
    };
    let status_response = NodeControllerStatusAcceptance {
        accepted_status_sequence: 21,
        receipts: vec![ControllerStatusReceipt {
            payload: Some(controller_status_receipt::Payload::Model(receipt.clone())),
        }],
    };
    assert_eq!(
        NodeControllerStatus::decode(status.encode_to_vec().as_slice()).expect("status round trip"),
        status
    );
    assert_eq!(
        NodeControllerStatusAcceptance::decode(status_response.encode_to_vec().as_slice())
            .expect("receipt round trip")
            .receipts[0]
            .payload,
        Some(controller_status_receipt::Payload::Model(receipt))
    );
}

#[test]
fn application_owner_extends_the_generic_envelope_without_free_text_or_reservations() {
    let descriptors =
        FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("generated descriptor set");
    let site = descriptor_file(&descriptors, "dotcore/site/private/v1/desired_view.proto");
    let application = descriptor_file(
        &descriptors,
        "dotcore/application/private/v1/controller.proto",
    );
    let logs = descriptor_file(&descriptors, "dotcore/deployment/private/v1/logs.proto");

    assert_eq!(
        enum_values(site, "DesiredViewSectionKind"),
        [
            ("DESIRED_VIEW_SECTION_KIND_UNSPECIFIED", 0),
            ("DESIRED_VIEW_SECTION_KIND_MODEL", 1),
            ("DESIRED_VIEW_SECTION_KIND_APPLICATION", 2),
        ]
    );
    for (name, application_number) in [
        ("DesiredViewSection", 4),
        ("ControllerStatusSection", 2),
        ("ControllerStatusReceipt", 2),
    ] {
        let field = message(site, name)
            .field
            .iter()
            .find(|field| field.name.as_deref() == Some("application"))
            .unwrap_or_else(|| panic!("{name} application payload"));
        assert_eq!(field.number, Some(application_number), "{name}");
        assert_eq!(field.oneof_index, Some(0), "{name} payload oneof");
    }
    assert_eq!(
        enum_values(logs, "DeploymentOwnerKind"),
        [
            ("DEPLOYMENT_OWNER_KIND_UNSPECIFIED", 0),
            ("DEPLOYMENT_OWNER_KIND_MODEL", 1),
            ("DEPLOYMENT_OWNER_KIND_APPLICATION", 2),
        ]
    );

    // The route id stays in the declaration; the assignment adds only the projected name.
    assert_eq!(
        field_names(message(application, "ApplicationAssignment")),
        ["declaration", "linked_route"]
    );
    assert_eq!(
        field_names(message(application, "ApplicationLinkedRouteProjection")),
        ["route_name", "unknown"]
    );
    assert_eq!(
        field_names(message(application, "ApplicationDeploymentStatus")),
        [
            "application_deployment_id",
            "lifecycle_generation",
            "placement",
            "realization_generation",
            "accepted_view_revision",
            "state_revision",
            "lifecycle",
            "stage",
            "readiness",
            "failure_code",
            "services",
            "endpoint",
            "service_accelerator_memory",
            "resolved_launch_identities",
        ]
    );
    let string_fields = message(application, "ApplicationDeploymentStatus")
        .field
        .iter()
        .filter(|field| field.r#type() == prost_types::field_descriptor_proto::Type::String)
        .map(|field| field.name.as_deref().expect("field name"))
        .collect::<Vec<_>>();
    assert_eq!(
        string_fields,
        ["application_deployment_id"],
        "status carries typed stage and failure, never free text"
    );
    assert_eq!(
        field_names(message(application, "ApplicationCleanupEvidence")),
        [
            "application_deployment_id",
            "lifecycle_generation",
            "realization_generation",
            "never_realized",
            "evidence_sha256",
        ]
    );
    assert_eq!(
        field_names(message(application, "ApplicationCleanupReceipt")),
        [
            "manager_session_id",
            "node_incarnation_id",
            "application_deployment_id",
            "lifecycle_generation",
            "status_sequence",
            "evidence_sha256",
            "disposition",
        ]
    );
    assert!(
        application
            .message_type
            .iter()
            .all(|message| message.reserved_range.is_empty() && message.reserved_name.is_empty()),
        "no application private field is reserved ahead of use"
    );
}

#[test]
fn application_wire_round_trip_keeps_route_projection_pins_and_cleanup_receipt_exact() {
    let placement = ExactPlacement {
        site_node_id: "worker-one".to_owned(),
        execution_target_id: "application-target".to_owned(),
    };
    let pin = NodeResolvedLaunchIdentity {
        selection_key: "web".to_owned(),
        launch_identity: format!("sha256:{}", "d".repeat(64)),
    };
    let declaration = ApplicationDeploymentDeclaration {
        application_deployment_id: "app-one".to_owned(),
        placement: Some(placement.clone()),
        desired_state: ApplicationDeploymentDesiredState::Present as i32,
        inference_route_id: Some("route-one".to_owned()),
        lifecycle_generation: 5,
        node_resolved_launch_identities: vec![pin.clone()],
        ..Default::default()
    };
    let section = ApplicationDesiredViewSection {
        assignments: vec![
            ApplicationAssignment {
                declaration: Some(declaration.clone()),
                linked_route: Some(ApplicationLinkedRouteProjection {
                    resolution: Some(application_linked_route_projection::Resolution::RouteName(
                        "chat".to_owned(),
                    )),
                }),
            },
            ApplicationAssignment {
                declaration: Some(ApplicationDeploymentDeclaration {
                    application_deployment_id: "app-unknown-route".to_owned(),
                    ..declaration
                }),
                linked_route: Some(ApplicationLinkedRouteProjection {
                    resolution: Some(application_linked_route_projection::Resolution::Unknown(
                        dotcore_proto::google::protobuf::Empty {},
                    )),
                }),
            },
        ],
    };
    let view = NodeDesiredView {
        required_sections: vec![
            RequiredDesiredViewSection {
                kind: DesiredViewSectionKind::Model as i32,
                schema_version: 1,
            },
            RequiredDesiredViewSection {
                kind: DesiredViewSectionKind::Application as i32,
                schema_version: 1,
            },
        ],
        sections: vec![
            DesiredViewSection {
                kind: DesiredViewSectionKind::Model as i32,
                schema_version: 1,
                payload: Some(desired_view_section::Payload::Model(
                    ModelDesiredViewSection::default(),
                )),
            },
            DesiredViewSection {
                kind: DesiredViewSectionKind::Application as i32,
                schema_version: 1,
                payload: Some(desired_view_section::Payload::Application(section)),
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        NodeDesiredView::decode(view.encode_to_vec().as_slice()).expect("view round trip"),
        view
    );

    let status = NodeControllerStatus {
        status_sequence: 3,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Application(
                ApplicationControllerStatusSection {
                    deployments: vec![ApplicationDeploymentStatus {
                        application_deployment_id: "app-one".to_owned(),
                        lifecycle_generation: 5,
                        placement: Some(placement),
                        realization_generation: Some(77),
                        accepted_view_revision: 13,
                        state_revision: 2,
                        lifecycle: ApplicationLifecycleObservation::Unavailable as i32,
                        stage: ApplicationStatusStage::EndpointNotReady as i32,
                        readiness: ApplicationReadinessState::NotReady as i32,
                        failure_code: ApplicationStatusFailureCode::Unspecified as i32,
                        services: Vec::new(),
                        endpoint: Some(ApplicationPublishedEndpoint {
                            host: "192.0.2.20".to_owned(),
                            port: 32768,
                        }),
                        service_accelerator_memory: Vec::new(),
                        resolved_launch_identities: vec![pin],
                    }],
                    cleanup_evidence: vec![ApplicationCleanupEvidence {
                        application_deployment_id: "app-gone".to_owned(),
                        lifecycle_generation: 9,
                        realization_generation: None,
                        never_realized: true,
                        evidence_sha256: "e".repeat(64),
                    }],
                },
            )),
        }],
        ..Default::default()
    };
    assert_eq!(
        NodeControllerStatus::decode(status.encode_to_vec().as_slice()).expect("status round trip"),
        status
    );

    let receipt = ApplicationCleanupReceipt {
        manager_session_id: "a".repeat(32),
        node_incarnation_id: "1".repeat(32),
        application_deployment_id: "app-gone".to_owned(),
        lifecycle_generation: 9,
        status_sequence: 3,
        evidence_sha256: "e".repeat(64),
        disposition: ApplicationCleanupDisposition::Rejected as i32,
    };
    let response = NodeControllerStatusAcceptance {
        accepted_status_sequence: 3,
        receipts: vec![ControllerStatusReceipt {
            payload: Some(controller_status_receipt::Payload::Application(
                receipt.clone(),
            )),
        }],
    };
    assert_eq!(
        NodeControllerStatusAcceptance::decode(response.encode_to_vec().as_slice())
            .expect("receipt round trip")
            .receipts[0]
            .payload,
        Some(controller_status_receipt::Payload::Application(receipt))
    );
}

#[test]
fn node_control_descriptor_defines_one_bidi_stream_with_its_own_payloads() {
    let descriptors =
        FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("generated descriptor set");
    let control = descriptor_file(&descriptors, "dotcore/site/private/v1/node_control.proto");

    let methods = control
        .service
        .iter()
        .map(|service| {
            (
                service.name.as_deref().expect("service name"),
                service
                    .method
                    .iter()
                    .map(|method| {
                        (
                            method.name.as_deref().expect("method name"),
                            method.input_type.as_deref().expect("input type"),
                            method.output_type.as_deref().expect("output type"),
                            method.client_streaming(),
                            method.server_streaming(),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [(
            "NodeControlService",
            vec![(
                "StreamNodeControl",
                ".dotcore.site.private.v1.StreamNodeControlRequest",
                ".dotcore.site.private.v1.StreamNodeControlResponse",
                true,
                true,
            )]
        )]
    );

    // Node payloads are the stream's own messages; views reuse the desired-view envelope.
    assert_eq!(
        oneof_fields(message(control, "StreamNodeControlRequest")),
        [
            ("open", 1, ".dotcore.site.private.v1.NodeControlOpen"),
            ("observation", 2, ".dotcore.site.private.v1.NodeObservation"),
            ("status", 3, ".dotcore.site.private.v1.NodeControllerStatus"),
            (
                "acknowledge_view",
                4,
                ".dotcore.site.private.v1.NodeViewAcknowledgement"
            ),
            ("heartbeat", 5, ".google.protobuf.Empty"),
        ]
    );
    assert_eq!(
        oneof_fields(message(control, "StreamNodeControlResponse")),
        [
            ("opened", 1, ".dotcore.site.private.v1.NodeControlOpened"),
            (
                "view_published",
                2,
                ".dotcore.site.private.v1.NodeDesiredView"
            ),
            (
                "observation_accepted",
                3,
                ".dotcore.site.private.v1.NodeObservationAcceptance"
            ),
            (
                "status_accepted",
                4,
                ".dotcore.site.private.v1.NodeControllerStatusAcceptance"
            ),
            (
                "acknowledgement_accepted",
                5,
                ".dotcore.site.private.v1.NodeViewAcknowledgementAcceptance"
            ),
            (
                "rejected",
                6,
                ".dotcore.site.private.v1.NodeControlRejection"
            ),
        ]
    );
    assert_eq!(
        field_names(message(control, "NodeControlOpen")),
        [
            "site_id",
            "node_id",
            "node_incarnation_id",
            "supported_sections"
        ]
    );
    assert_eq!(
        field_names(message(control, "NodeControlOpened")),
        ["manager_session_id", "view"]
    );
    for (name, fields) in [
        (
            "NodeObservation",
            vec![
                "site_id",
                "node_id",
                "incarnation_id",
                "revision",
                "snapshot",
                "observation_configuration_sha256",
                "private_port",
            ],
        ),
        (
            "NodeControllerStatus",
            vec!["view", "status_sequence", "sections"],
        ),
        (
            "NodeViewAcknowledgement",
            vec!["view", "acknowledgement_sequence", "accepted", "rejected"],
        ),
        (
            "NodeObservationAcceptance",
            vec!["accepted_revision", "received_at"],
        ),
        (
            "NodeControllerStatusAcceptance",
            vec!["accepted_status_sequence", "receipts"],
        ),
        (
            "NodeViewAcknowledgementAcceptance",
            vec!["accepted_acknowledgement_sequence"],
        ),
    ] {
        assert_eq!(field_names(message(control, name)), fields, "{name}");
    }
    assert_eq!(
        field_names(message(control, "NodeControlRejection")),
        ["kind", "sequence", "code"]
    );
    assert_eq!(
        enum_values(control, "NodeControlMessageKind"),
        [
            ("NODE_CONTROL_MESSAGE_KIND_UNSPECIFIED", 0),
            ("NODE_CONTROL_MESSAGE_KIND_OBSERVATION", 1),
            ("NODE_CONTROL_MESSAGE_KIND_STATUS", 2),
            ("NODE_CONTROL_MESSAGE_KIND_ACKNOWLEDGEMENT", 3),
        ]
    );
    assert_eq!(
        enum_values(control, "NodeControlRejectionCode"),
        [
            ("NODE_CONTROL_REJECTION_CODE_UNSPECIFIED", 0),
            ("NODE_CONTROL_REJECTION_CODE_INVALID", 1),
            ("NODE_CONTROL_REJECTION_CODE_CONFLICT", 2),
            ("NODE_CONTROL_REJECTION_CODE_UNAVAILABLE", 3),
        ]
    );
}

#[test]
fn node_control_wire_round_trip_keeps_open_view_and_per_message_answers_exact() {
    let view_reference = NodeViewReference {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        manager_session_id: "a".repeat(32),
        node_incarnation_id: "1".repeat(32),
        view_revision: 13,
        content_sha256: "b".repeat(64),
    };
    let requests = [
        StreamNodeControlRequest {
            message: Some(stream_node_control_request::Message::Open(
                NodeControlOpen {
                    site_id: "site-one".to_owned(),
                    node_id: "worker-one".to_owned(),
                    node_incarnation_id: "1".repeat(32),
                    supported_sections: vec![SupportedDesiredViewSection {
                        kind: DesiredViewSectionKind::Application as i32,
                        minimum_schema_version: 1,
                        maximum_schema_version: 2,
                    }],
                },
            )),
        },
        StreamNodeControlRequest {
            message: Some(stream_node_control_request::Message::AcknowledgeView(
                NodeViewAcknowledgement {
                    view: Some(view_reference.clone()),
                    acknowledgement_sequence: 4,
                    outcome: Some(node_view_acknowledgement::Outcome::Accepted(Empty {})),
                },
            )),
        },
        StreamNodeControlRequest {
            message: Some(stream_node_control_request::Message::Status(
                NodeControllerStatus {
                    view: Some(view_reference),
                    status_sequence: 21,
                    sections: vec![ControllerStatusSection {
                        payload: Some(controller_status_section::Payload::Model(
                            ModelControllerStatusSection::default(),
                        )),
                    }],
                },
            )),
        },
        StreamNodeControlRequest {
            message: Some(stream_node_control_request::Message::Heartbeat(Empty {})),
        },
    ];
    for request in requests {
        assert_eq!(
            StreamNodeControlRequest::decode(request.encode_to_vec().as_slice())
                .expect("node message round trip"),
            request
        );
    }

    let view = NodeDesiredView {
        identity: Some(NodeViewIdentity {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "1".repeat(32),
            publication_sequence: 2,
            view_revision: 13,
            content_sha256: "b".repeat(64),
            projection_schema_version: 1,
        }),
        ..Default::default()
    };
    let responses = [
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::Opened(
                NodeControlOpened {
                    manager_session_id: "a".repeat(32),
                    view: Some(view.clone()),
                },
            )),
        },
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::ViewPublished(view)),
        },
        StreamNodeControlResponse {
            message: Some(
                stream_node_control_response::Message::AcknowledgementAccepted(
                    NodeViewAcknowledgementAcceptance {
                        accepted_acknowledgement_sequence: 4,
                    },
                ),
            ),
        },
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::StatusAccepted(
                NodeControllerStatusAcceptance {
                    accepted_status_sequence: 21,
                    receipts: Vec::new(),
                },
            )),
        },
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::Rejected(
                NodeControlRejection {
                    kind: NodeControlMessageKind::Status as i32,
                    sequence: 22,
                    code: NodeControlRejectionCode::Conflict as i32,
                },
            )),
        },
    ];
    for response in responses {
        assert_eq!(
            StreamNodeControlResponse::decode(response.encode_to_vec().as_slice())
                .expect("manager message round trip"),
            response
        );
    }
}

#[test]
fn browser_generation_inputs_exclude_private_control_plane_packages() {
    let generation = include_str!("../../../proto/buf.gen.yaml");
    assert!(!generation.contains("dotcore.site.private"));
    assert!(!generation.contains("dotcore.model.private"));
    assert!(!generation.contains("dotcore.application.private"));
}

fn descriptor_file<'a>(descriptors: &'a FileDescriptorSet, name: &str) -> &'a FileDescriptorProto {
    descriptors
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing descriptor {name}"))
}

fn message<'a>(file: &'a FileDescriptorProto, name: &str) -> &'a prost_types::DescriptorProto {
    file.message_type
        .iter()
        .find(|message| message.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing message {name}"))
}

fn field_names(message: &prost_types::DescriptorProto) -> Vec<&str> {
    message
        .field
        .iter()
        .map(|field| field.name.as_deref().expect("field name"))
        .collect()
}

fn oneof_fields(message: &prost_types::DescriptorProto) -> Vec<(&str, i32, &str)> {
    message
        .field
        .iter()
        .map(|field| {
            assert_eq!(
                field.oneof_index,
                Some(0),
                "{:?} is in the oneof",
                field.name
            );
            (
                field.name.as_deref().expect("field name"),
                field.number(),
                field.type_name.as_deref().expect("field type"),
            )
        })
        .collect()
}

fn reserved_numbers(message: &prost_types::DescriptorProto) -> Vec<i32> {
    message
        .reserved_range
        .iter()
        .flat_map(|range| range.start()..range.end())
        .collect()
}

fn enum_values<'a>(file: &'a FileDescriptorProto, name: &str) -> Vec<(&'a str, i32)> {
    file.enum_type
        .iter()
        .find(|enum_type| enum_type.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("missing enum {name}"))
        .value
        .iter()
        .map(|value| (value.name.as_deref().expect("value name"), value.number()))
        .collect()
}
