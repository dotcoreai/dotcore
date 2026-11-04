//! Private node-agent observation protobuf contract tests.
//!
//! These tests pin the authenticated observation envelope, its carriage on the node control
//! stream, and independent hardware and runtime outcomes. They do not test transport
//! authentication or freshness transitions, which belong to the manager implementation.

use dotcore_proto::{
    FILE_DESCRIPTOR_SET,
    dotcore::{
        observation::v1::{
            HardwareSnapshot, ObservationUnavailable, RuntimeKind, RuntimeObservation,
            runtime_observation,
        },
        site::private::v1::{
            NodeControlMessageKind, NodeControlRejection, NodeControlRejectionCode,
            NodeHardwareObservation, NodeObservation, NodeObservationAcceptance,
            NodeObservationSnapshot, StreamNodeControlRequest, StreamNodeControlResponse,
            node_hardware_observation, stream_node_control_request, stream_node_control_response,
        },
    },
    google::protobuf::{Empty, Timestamp},
};
use prost::Message;
use prost_types::FileDescriptorSet;

#[test]
fn node_observation_reports_one_complete_revision_on_the_node_control_stream() {
    let descriptor = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("descriptor set");
    let node_services = descriptor
        .file
        .iter()
        .filter(|file| file.package.as_deref() == Some("dotcore.node.v1"))
        .flat_map(|file| &file.service)
        .map(|service| service.name.as_deref().expect("service name"))
        .collect::<Vec<_>>();
    assert_eq!(
        node_services,
        ["NodeJoinService"],
        "node observations travel only on the node control stream"
    );
    let file = descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some("dotcore/site/private/v1/node_control.proto"))
        .expect("node control contract");

    let hardware = file
        .message_type
        .iter()
        .find(|message| message.name.as_deref() == Some("NodeHardwareObservation"))
        .expect("private hardware result");
    assert_eq!(
        hardware
            .field
            .iter()
            .map(|field| field.type_name.as_deref().expect("field type"))
            .collect::<Vec<_>>(),
        [
            ".dotcore.observation.v1.HardwareSnapshot",
            ".dotcore.observation.v1.ObservationUnavailable",
        ]
    );
    let observation = descriptor
        .file
        .iter()
        .find(|file| file.name.as_deref() == Some("dotcore/observation/v1/observation.proto"))
        .expect("shared observation values");
    assert!(
        observation.service.is_empty(),
        "shared values cannot own a service"
    );

    let request = NodeObservation {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        incarnation_id: "8e4984f2-ef34-4ba7-9557-521b606c1ba1".to_owned(),
        revision: 7,
        snapshot: Some(NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Snapshot(
                    HardwareSnapshot {
                        observed_at: Some(Timestamp {
                            seconds: 1_700_000_000,
                            nanos: 0,
                        }),
                        ..Default::default()
                    },
                )),
            }),
            runtimes: vec![
                RuntimeObservation {
                    execution_target_id: "model-target".to_owned(),
                    kind: RuntimeKind::Model as i32,
                    result: Some(runtime_observation::Result::Available(Empty {})),
                },
                RuntimeObservation {
                    execution_target_id: "application-target".to_owned(),
                    kind: RuntimeKind::Application as i32,
                    result: Some(runtime_observation::Result::Unsupported(
                        ObservationUnavailable {
                            code: "RUNTIME_UNSUPPORTED".to_owned(),
                            message: "Application runtime is not supported on this node."
                                .to_owned(),
                        },
                    )),
                },
            ],
        }),
        observation_configuration_sha256:
            "6c9cb449feea3e1e70174b7b40c4b35a86e3d6eb95d87a0c5e81c34f067e2322".to_owned(),
        private_port: Some(47_123),
    };
    assert_eq!(
        NodeObservation::decode(request.encode_to_vec().as_slice()).expect("request round trip"),
        request
    );

    let response = NodeObservationAcceptance {
        accepted_revision: 7,
        received_at: Some(Timestamp {
            seconds: 1_700_000_001,
            nanos: 0,
        }),
    };
    assert_eq!(
        NodeObservationAcceptance::decode(response.encode_to_vec().as_slice())
            .expect("response round trip"),
        response
    );
}

#[test]
fn failed_hardware_collection_does_not_erase_runtime_results() {
    let request = NodeObservation {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        incarnation_id: "8e4984f2-ef34-4ba7-9557-521b606c1ba1".to_owned(),
        revision: 8,
        snapshot: Some(NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "HARDWARE_COLLECTION_FAILED".to_owned(),
                        message: "The hardware collector failed.".to_owned(),
                    },
                )),
            }),
            runtimes: vec![RuntimeObservation {
                execution_target_id: "model-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                result: Some(runtime_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "RUNTIME_UNAVAILABLE".to_owned(),
                        message: "The configured runtime is unavailable.".to_owned(),
                    },
                )),
            }],
        }),
        observation_configuration_sha256:
            "6c9cb449feea3e1e70174b7b40c4b35a86e3d6eb95d87a0c5e81c34f067e2322".to_owned(),
        private_port: None,
    };
    let decoded = NodeObservation::decode(request.encode_to_vec().as_slice())
        .expect("partial failure round trip");
    assert_eq!(decoded, request);
}

#[test]
fn node_control_stream_carries_one_observation_and_its_answer() {
    let observation = NodeObservation {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        incarnation_id: "8e4984f2-ef34-4ba7-9557-521b606c1ba1".to_owned(),
        revision: 9,
        snapshot: Some(NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Snapshot(
                    HardwareSnapshot::default(),
                )),
            }),
            runtimes: Vec::new(),
        }),
        observation_configuration_sha256:
            "6c9cb449feea3e1e70174b7b40c4b35a86e3d6eb95d87a0c5e81c34f067e2322".to_owned(),
        private_port: Some(47_123),
    };
    let request = StreamNodeControlRequest {
        message: Some(stream_node_control_request::Message::Observation(
            observation.clone(),
        )),
    };
    assert_eq!(
        StreamNodeControlRequest::decode(request.encode_to_vec().as_slice())
            .expect("stream observation round trip")
            .message,
        Some(stream_node_control_request::Message::Observation(
            observation
        ))
    );

    let answers = [
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::ObservationAccepted(
                NodeObservationAcceptance {
                    accepted_revision: 9,
                    received_at: Some(Timestamp {
                        seconds: 1_700_000_001,
                        nanos: 0,
                    }),
                },
            )),
        },
        StreamNodeControlResponse {
            message: Some(stream_node_control_response::Message::Rejected(
                NodeControlRejection {
                    kind: NodeControlMessageKind::Observation as i32,
                    sequence: 9,
                    code: NodeControlRejectionCode::Invalid as i32,
                },
            )),
        },
    ];
    for answer in answers {
        assert_eq!(
            StreamNodeControlResponse::decode(answer.encode_to_vec().as_slice())
                .expect("stream observation answer round trip"),
            answer
        );
    }
}
