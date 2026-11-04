//! Private inter-node transport and generated worker startup integration tests.

use std::{
    error::Error as _,
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

use dotcore_backend::{
    composition::{Backend, BackendConfiguration, BackendRole, ServerConfiguration},
    model::engine::declaration_validator,
    server::private::PrivateServiceAssembly,
};
use dotcore_proto::{
    dotcore::{
        inference::{
            private::v1::{
                ExactServingTarget, ExactTargetRejected, ExactTargetRejectionReason,
                InvokeExactTargetRequest, invoke_exact_target_request,
                invoke_exact_target_response,
                peer_inference_service_client::PeerInferenceServiceClient,
            },
            v1::{ChatMessage, ChatMessageRole, ChatRequest, chat_message},
        },
        node::v1::node_execution_target,
        observation::v1::{
            ObservationUnavailable, RuntimeKind, RuntimeObservation, runtime_observation,
        },
        site::{
            private::v1::{
                DesiredViewSectionKind, NodeControlOpen, NodeHardwareObservation, NodeObservation,
                NodeObservationSnapshot, NodeViewAcknowledgement, NodeViewReference,
                SupportedDesiredViewSection, node_hardware_observation, node_view_acknowledgement,
                stream_node_control_request, stream_node_control_response,
            },
            v1::{
                ListHardwareSnapshotsRequest, ListMachinesRequest, NodeObservationState, Site,
                current_hardware_snapshot_availability, hardware_refresh_state,
                site_service_client::SiteServiceClient,
            },
        },
    },
    google::protobuf::Empty,
    yaml,
};
use dotcore_site::{
    initial::{
        input_store::InitialSiteInputWriter,
        inputs::InitialSiteInputs,
        probe::InitialSiteProbeReport,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
            RandomInitialSiteIdentityGenerator,
        },
        validation::SiteInputBundleValidator,
    },
    manager_trust::{SITE_CA_PRIVATE_KEY_FILE, SITE_TRUST_DIRECTORY},
    node::{
        folder::{
            CertificateFingerprint, NODE_CERTIFICATE_FILE, NODE_PRIVATE_KEY_FILE,
            NodeFolderValidationError, NodeFolderValidator, TRUST_ANCHOR_FILE,
            WORKER_MANIFEST_FILE, certificate_identity_uri,
        },
        observation_configuration::NodeObservationConfigurationDigest,
    },
};
use rcgen::{
    CertificateParams, ExtendedKeyUsagePurpose, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tempfile::TempDir;
use time::{Duration as TimeDuration, OffsetDateTime};
use tonic::transport::{Channel, Endpoint};
use x509_parser::{extensions::GeneralName, prelude::FromDer};

use crate::support::{self, node_control::ControlStream};
mod test_service;

use test_service::{InvocationCounts, QualificationClient, QualificationService};

#[tokio::test]
async fn native_mtls_ingress_exposes_exact_identity_and_enforces_receiver_policies() {
    let fixture = PrivateFixture::new();
    let manager_counts = InvocationCounts::default();
    let worker_counts = InvocationCounts::default();
    let (manager, worker) = fixture
        .prepare_pair_with_services(manager_counts.clone(), worker_counts.clone())
        .await;
    let manager_address = manager
        .addresses()
        .private
        .expect("manager private address");
    let worker_address = worker.addresses().private.expect("worker private address");
    let manager_task = tokio::spawn(manager.serve());
    let worker_task = tokio::spawn(worker.serve());

    let mut worker_hosted = QualificationClient::new(fixture.manager_channel(worker_address).await);
    let manager_identity = worker_hosted
        .manager_call()
        .await
        .expect("worker-hosted manager-caller policy")
        .into_inner();
    assert_eq!(manager_identity.site_id, "fixture-site");
    assert_eq!(manager_identity.node_id, "fixture-node");
    let manager_as_worker = worker_hosted
        .worker_call()
        .await
        .expect_err("manager cannot use worker-caller policy");
    assert_eq!(manager_as_worker.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        manager_as_worker.message(),
        "private operation is not authorized"
    );
    assert_eq!(worker_counts.manager_calls(), 1);
    assert_eq!(worker_counts.worker_calls(), 0);

    let mut manager_hosted =
        QualificationClient::new(fixture.worker_channel(manager_address).await);
    let worker_identity = manager_hosted
        .worker_call()
        .await
        .expect("manager-hosted worker-caller policy")
        .into_inner();
    assert_eq!(worker_identity.site_id, "fixture-site");
    assert_eq!(worker_identity.node_id, support::WORKER_NODE_ID);
    let worker_as_manager = manager_hosted
        .manager_call()
        .await
        .expect_err("worker cannot use manager-caller policy");
    assert_eq!(worker_as_manager.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        worker_as_manager.message(),
        "private operation is not authorized"
    );
    assert_eq!(manager_counts.worker_calls(), 1);
    assert_eq!(manager_counts.manager_calls(), 0);

    manager_task.abort();
    worker_task.abort();
}

#[tokio::test]
async fn native_tls_and_ingress_reject_foreign_trust_unknown_identity_and_pin_mismatch() {
    let fixture = PrivateFixture::new();
    let (second_node_folder, second_node_id) = fixture.add_second_worker();
    fs::remove_file(
        fixture
            .manager_root
            .join(SITE_TRUST_DIRECTORY)
            .join(format!("worker-pins/{second_node_id}.sha256")),
    )
    .expect("remove second worker admission pin");
    let manager_counts = InvocationCounts::default();
    let worker_counts = InvocationCounts::default();
    let (manager, worker) = fixture
        .prepare_pair_with_services(manager_counts.clone(), worker_counts.clone())
        .await;
    let manager_address = manager
        .addresses()
        .private
        .expect("manager private address");
    let worker_address = worker.addresses().private.expect("worker private address");
    let manager_task = tokio::spawn(manager.serve());
    let worker_task = tokio::spawn(worker.serve());

    let foreign = PrivateFixture::new();
    let error = foreign
        .manager_endpoint(worker_address)
        .connect()
        .await
        .expect_err("foreign CA must fail native TLS");
    assert!(error.to_string().contains("transport error"));

    let unknown = fixture
        .worker_channel_from(&second_node_folder, manager_address)
        .await;
    let error = QualificationClient::new(unknown)
        .worker_call()
        .await
        .expect_err("unknown exact identity");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert_eq!(error.message(), "private peer authentication failed");
    assert_eq!(manager_counts.worker_calls(), 0);
    assert_eq!(manager_counts.manager_calls(), 0);
    assert_eq!(worker_counts.worker_calls(), 0);
    assert_eq!(worker_counts.manager_calls(), 0);
    manager_task.abort();
    worker_task.abort();

    let pin_fixture = PrivateFixture::new();
    let pin = pin_fixture
        .manager_root
        .join(SITE_TRUST_DIRECTORY)
        .join(format!("worker-pins/{}.sha256", support::WORKER_NODE_ID));
    fs::write(&pin, format!("{}\n", "0".repeat(64))).expect("replace manager worker pin");
    let manager_counts = InvocationCounts::default();
    let worker_counts = InvocationCounts::default();
    let (manager, worker) = pin_fixture
        .prepare_pair_with_services(manager_counts.clone(), worker_counts.clone())
        .await;
    let manager_address = manager
        .addresses()
        .private
        .expect("manager private address");
    let manager_task = tokio::spawn(manager.serve());
    let worker_task = tokio::spawn(worker.serve());
    let channel = pin_fixture.worker_channel(manager_address).await;
    let error = QualificationClient::new(channel)
        .worker_call()
        .await
        .expect_err("leaf pin mismatch");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert_eq!(error.message(), "private peer authentication failed");
    assert_eq!(manager_counts.worker_calls(), 0);
    assert_eq!(manager_counts.manager_calls(), 0);
    assert_eq!(worker_counts.worker_calls(), 0);
    assert_eq!(worker_counts.manager_calls(), 0);
    manager_task.abort();
    worker_task.abort();
}

#[tokio::test]
async fn ingress_rejects_a_pinned_leaf_with_the_wrong_site_claim_before_handler_invocation() {
    let fixture = PrivateFixture::new();
    replace_worker_identity_claim(
        &fixture.manager_root,
        &fixture.node_root,
        "foreign-site",
        support::WORKER_NODE_ID,
    );
    let counts = InvocationCounts::default();
    let manager = Backend::prepare_with_private_service_assembly(
        fixture.manager_configuration(),
        PrivateServiceAssembly::new({
            let counts = counts.clone();
            move |context, routes| QualificationService::add_routes(context, counts, routes)
        }),
        std::future::pending(),
    )
    .await
    .expect("manager private transport");
    let manager_address = manager
        .addresses()
        .private
        .expect("manager private address");
    let manager_task = tokio::spawn(manager.serve());

    let error = QualificationClient::new(fixture.worker_channel(manager_address).await)
        .worker_call()
        .await
        .expect_err("wrong site claim");
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert_eq!(error.message(), "private peer authentication failed");
    assert_eq!(counts.worker_calls(), 0);
    assert_eq!(counts.manager_calls(), 0);

    manager_task.abort();
}

#[tokio::test]
async fn native_mtls_rejects_an_expired_client_credential() {
    let fixture = PrivateFixture::new();
    let manager_counts = InvocationCounts::default();
    let worker_counts = InvocationCounts::default();
    let (manager, worker) = fixture
        .prepare_pair_with_services(manager_counts.clone(), worker_counts.clone())
        .await;
    replace_manager_identity_with_expired(&fixture.manager_root);
    let worker_address = worker.addresses().private.expect("worker private address");
    let manager_task = tokio::spawn(manager.serve());
    let worker_task = tokio::spawn(worker.serve());

    let channel = fixture
        .manager_endpoint(worker_address)
        .connect()
        .await
        .expect("channel construction may complete before client authentication");
    let error = QualificationClient::new(channel)
        .manager_call()
        .await
        .expect_err("expired client credential must fail native TLS");
    assert_eq!(error.code(), tonic::Code::Unknown);
    assert_eq!(manager_counts.worker_calls(), 0);
    assert_eq!(manager_counts.manager_calls(), 0);
    assert_eq!(worker_counts.worker_calls(), 0);
    assert_eq!(worker_counts.manager_calls(), 0);

    manager_task.abort();
    worker_task.abort();
}

#[tokio::test]
async fn production_private_listener_mounts_generic_node_control_plane_without_test_routes() {
    let fixture = PrivateFixture::new();
    let (manager, worker) = fixture.prepare_pair().await;
    let manager_address = manager
        .addresses()
        .private
        .expect("manager private address");
    let management_address = manager.addresses().management.expect("management listener");
    drop(worker);
    let folder =
        NodeFolderValidator::validate_declared(&fixture.node_root).expect("validated node folder");
    let runtimes: Vec<_> = folder
        .observation_configuration()
        .execution_targets
        .iter()
        .map(|target| RuntimeObservation {
            execution_target_id: target.execution_target_id.clone(),
            kind: target.kind,
            result: Some(runtime_observation::Result::Available(Empty {})),
        })
        .collect();
    let expected_runtimes = runtimes.clone();
    let hardware_failure = ObservationUnavailable {
        code: "COLLECTION_FAILED".to_owned(),
        message: "Hardware observation failed on this worker.".to_owned(),
    };
    let request = NodeObservation {
        site_id: "fixture-site".to_owned(),
        node_id: support::WORKER_NODE_ID.to_owned(),
        incarnation_id: "1".repeat(32),
        revision: 1,
        snapshot: Some(NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Unavailable(
                    hardware_failure.clone(),
                )),
            }),
            runtimes,
        }),
        observation_configuration_sha256: folder
            .observation_configuration_sha256()
            .as_str()
            .to_owned(),
        private_port: None,
    };
    let manager_task = tokio::spawn(manager.serve());
    let open = NodeControlOpen {
        site_id: "fixture-site".to_owned(),
        node_id: support::WORKER_NODE_ID.to_owned(),
        node_incarnation_id: "1".repeat(32),
        supported_sections: vec![SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            minimum_schema_version: 1,
            maximum_schema_version: 1,
        }],
    };
    for (claim, expected) in [
        (
            NodeControlOpen {
                site_id: "other-site".to_owned(),
                ..open.clone()
            },
            tonic::Code::PermissionDenied,
        ),
        (
            NodeControlOpen {
                node_id: "fixture-node".to_owned(),
                ..open.clone()
            },
            tonic::Code::PermissionDenied,
        ),
        (
            NodeControlOpen {
                supported_sections: vec![SupportedDesiredViewSection {
                    minimum_schema_version: 2,
                    ..open.supported_sections[0]
                }],
                ..open.clone()
            },
            tonic::Code::InvalidArgument,
        ),
    ] {
        let mut stream =
            ControlStream::open(fixture.worker_channel(manager_address).await, claim).await;
        assert_eq!(
            stream.next().await.expect_err("refused open").code(),
            expected
        );
    }

    let mut mismatched_identity = request.clone();
    mismatched_identity.node_id = "fixture-node".to_owned();
    let mut stream =
        ControlStream::open(fixture.worker_channel(manager_address).await, open.clone()).await;
    stream.opened().await;
    stream.send(stream_node_control_request::Message::Observation(
        mismatched_identity,
    ));
    assert_eq!(
        stream
            .next()
            .await
            .expect_err("claimed node must match authenticated worker")
            .code(),
        tonic::Code::PermissionDenied
    );

    let mut stream =
        ControlStream::open(fixture.worker_channel(manager_address).await, open.clone()).await;
    let opened = stream.opened().await;
    let manager_session = opened.manager_session_id;
    let view = opened.view.expect("complete worker view");
    stream.send(stream_node_control_request::Message::Observation(
        request.clone(),
    ));
    let Ok(stream_node_control_response::Message::ObservationAccepted(observation_response)) =
        stream.next().await
    else {
        panic!("production node control stream accepts the observation")
    };
    let identity = view.identity.clone().expect("view identity");
    assert_eq!(identity.site_id, "fixture-site");
    assert_eq!(identity.node_id, support::WORKER_NODE_ID);
    assert_eq!(identity.manager_session_id, manager_session);
    assert_eq!(identity.node_incarnation_id, "1".repeat(32));
    assert_eq!(identity.publication_sequence, 1);
    assert_eq!(identity.view_revision, 1);
    assert_eq!(
        view.required_sections
            .iter()
            .map(|section| section.kind)
            .collect::<Vec<_>>(),
        [
            DesiredViewSectionKind::Model as i32,
            DesiredViewSectionKind::Application as i32
        ],
        "a worker with an application target receives explicit model and application sections"
    );
    assert_eq!(view.sections.len(), 2);
    let acknowledgement_request = NodeViewAcknowledgement {
        view: Some(NodeViewReference {
            site_id: identity.site_id.clone(),
            node_id: identity.node_id.clone(),
            manager_session_id: identity.manager_session_id.clone(),
            node_incarnation_id: identity.node_incarnation_id.clone(),
            view_revision: identity.view_revision,
            content_sha256: identity.content_sha256.clone(),
        }),
        acknowledgement_sequence: 1,
        outcome: Some(node_view_acknowledgement::Outcome::Accepted(Empty {})),
    };
    let mut acknowledgements = Vec::new();
    for _ in 0..2 {
        stream.send(stream_node_control_request::Message::AcknowledgeView(
            acknowledgement_request.clone(),
        ));
        let Ok(stream_node_control_response::Message::AcknowledgementAccepted(accepted)) =
            stream.next().await
        else {
            panic!("desired-view acknowledgement and its exact retry are accepted")
        };
        acknowledgements.push(accepted.accepted_acknowledgement_sequence);
    }
    assert_eq!(acknowledgements, [1, 1]);

    // Closing the stream disconnects the worker at once, so a new incarnation may open.
    drop(stream);
    support::wait_for_worker_observation(management_address, NodeObservationState::Disconnected)
        .await;
    let mut replacement_stream = ControlStream::open(
        fixture.worker_channel(manager_address).await,
        NodeControlOpen {
            node_incarnation_id: "2".repeat(32),
            ..open
        },
    )
    .await;
    let replacement_identity = replacement_stream
        .opened()
        .await
        .view
        .expect("replacement complete view")
        .identity
        .expect("replacement view identity");
    assert_eq!(replacement_identity.node_incarnation_id, "2".repeat(32));
    assert_eq!(replacement_identity.publication_sequence, 1);
    let mut replacement_observation = request.clone();
    replacement_observation.incarnation_id = "2".repeat(32);
    replacement_observation.revision = 1;
    replacement_stream.send(stream_node_control_request::Message::Observation(
        replacement_observation,
    ));
    let Ok(stream_node_control_response::Message::ObservationAccepted(
        replacement_observation_response,
    )) = replacement_stream.next().await
    else {
        panic!("replacement observation sequence one")
    };
    assert_eq!(replacement_observation_response.accepted_revision, 1);
    let mut installed_targets = folder
        .realization_targets()
        .iter()
        .filter(|target| {
            matches!(
                target.runtime_binding,
                Some(
                    dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(_)
                        | dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(_)
                )
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    installed_targets
        .sort_by(|left, right| left.execution_target_id.cmp(&right.execution_target_id));
    assert_eq!(view.execution_targets, installed_targets);

    assert_eq!(observation_response.accepted_revision, 1);
    let mut site = SiteServiceClient::connect(format!("http://{management_address}"))
        .await
        .expect("manager site API connection");
    let machines = site
        .list_machines(ListMachinesRequest {})
        .await
        .expect("public machine inventory")
        .into_inner()
        .machines;
    let worker = machines
        .into_iter()
        .find(|machine| machine.machine_id == support::WORKER_NODE_ID)
        .expect("reported worker projection");
    assert_eq!(
        worker
            .current_hardware_snapshot_availability
            .expect("worker hardware availability")
            .availability,
        Some(
            current_hardware_snapshot_availability::Availability::Unavailable(
                hardware_failure.clone(),
            ),
        )
    );
    let freshness = worker
        .observation_freshness
        .expect("worker observation freshness");
    assert_eq!(
        (freshness.state, freshness.received_at),
        (
            NodeObservationState::Connected as i32,
            replacement_observation_response.received_at,
        )
    );
    assert_eq!(worker.runtimes, expected_runtimes);

    let hardware = site
        .list_hardware_snapshots(ListHardwareSnapshotsRequest {})
        .await
        .expect("public worker hardware")
        .into_inner()
        .machines
        .into_iter()
        .find(|machine| machine.machine_id == support::WORKER_NODE_ID)
        .expect("worker hardware entry");
    assert_eq!(hardware.snapshot, None);
    assert_eq!(
        hardware.refresh.expect("worker refresh outcome").outcome,
        Some(hardware_refresh_state::Outcome::Failed(hardware_failure))
    );
    assert_eq!(
        hardware
            .observation_freshness
            .expect("worker hardware freshness")
            .state,
        NodeObservationState::Connected as i32
    );

    let error = QualificationClient::new(fixture.worker_channel(manager_address).await)
        .worker_call()
        .await
        .expect_err("production assembly must not mount test qualification routes");
    assert_eq!(error.code(), tonic::Code::Unimplemented);

    drop(replacement_stream);
    manager_task.abort();
}

#[tokio::test]
async fn worker_listener_serves_exact_target_inference_to_the_manager_with_full_size_requests() {
    let fixture = PrivateFixture::new();
    let worker = fixture
        .prepare_worker()
        .await
        .expect("joined node folder worker");
    let worker_address = worker.addresses().private.expect("worker private address");
    let worker_task = tokio::spawn(worker.serve());
    // Larger than tonic's 4 MiB default, as a public request carrying images can be.
    let text = "x".repeat(24 * 1024 * 1024);
    let request = InvokeExactTargetRequest {
        target: Some(ExactServingTarget {
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "f".repeat(32),
            model_deployment_id: "deployment-a".to_owned(),
            lifecycle_generation: 1,
            binding_token: "0".repeat(64),
        }),
        request: Some(invoke_exact_target_request::Request::Chat(ChatRequest {
            model_id: "route-a".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent(text)),
                ..Default::default()
            }],
            ..Default::default()
        })),
    };

    let mut frames = PeerInferenceServiceClient::new(fixture.manager_channel(worker_address).await)
        .max_encoding_message_size(64 * 1024 * 1024)
        .invoke_exact_target(request)
        .await
        .expect("worker mounts exact-target inference for the manager")
        .into_inner();

    assert_eq!(
        frames
            .message()
            .await
            .expect("first frame")
            .and_then(|response| response.frame),
        Some(invoke_exact_target_response::Frame::Rejected(
            ExactTargetRejected {
                reason: ExactTargetRejectionReason::NodeIncarnationMismatch as i32,
                ..Default::default()
            }
        )),
        "another incarnation's target is rejected before dispatch"
    );
    assert_eq!(frames.message().await.expect("end of stream"), None);

    worker_task.abort();
}

#[tokio::test]
async fn manager_rejects_stale_worker_pins_and_symlinked_trust_artifacts() {
    use std::os::unix::fs::symlink;

    let stale_pin_fixture = PrivateFixture::new();
    let site_path = stale_pin_fixture.manager_root.join("site.yaml");
    let mut site: Site =
        yaml::from_str(&fs::read_to_string(&site_path).expect("read site")).expect("parse site");
    site.nodes
        .retain(|node| node.node_id != support::WORKER_NODE_ID);
    site.execution_targets
        .retain(|target| target.site_node_id != support::WORKER_NODE_ID);
    fs::write(&site_path, yaml::to_string(&site).expect("serialize site")).expect("rewrite site");

    let error = match Backend::prepare(
        stale_pin_fixture.manager_configuration(),
        std::future::pending(),
    )
    .await
    {
        Ok(_) => panic!("stale worker pin must not grant membership"),
        Err(error) => error,
    };
    assert!(
        error
            .source()
            .into_iter()
            .chain(error.source().and_then(std::error::Error::source))
            .any(|cause| cause.to_string() == "worker pin does not name a declared worker node")
    );

    let symlink_fixture = PrivateFixture::new();
    let trust = symlink_fixture.manager_root.join(SITE_TRUST_DIRECTORY);
    let key = trust.join(NODE_PRIVATE_KEY_FILE);
    let moved_key = trust.join("moved-manager-key.pk8");
    fs::rename(&key, &moved_key).expect("move manager key");
    symlink(&moved_key, &key).expect("symlink manager key");

    let error = match Backend::prepare(
        symlink_fixture.manager_configuration(),
        std::future::pending(),
    )
    .await
    {
        Ok(_) => panic!("symlinked manager key must be rejected"),
        Err(error) => error,
    };
    let mut cause = error.source();
    let mut messages = Vec::new();
    while let Some(current) = cause {
        messages.push(current.to_string());
        cause = current.source();
    }
    assert!(
        messages
            .iter()
            .any(|message| message == "trust artifact is not a regular file"),
        "{messages:?}"
    );
}

#[test]
fn provisioned_manager_and_worker_use_the_same_node_identity_layout() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = PrivateFixture::new();
    let manager_trust = fixture.manager_root.join("trust");
    let worker_trust = fixture.node_root.join("trust");
    let expected = [
        (&manager_trust, "fixture-node"),
        (&worker_trust, support::WORKER_NODE_ID),
    ];
    let mut leaf_keys = Vec::new();
    for (trust, node_id) in expected {
        assert_eq!(
            fs::metadata(trust).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let anchor = fs::read(trust.join("site-ca.der")).expect("node trust anchor");
        assert_eq!(anchor, fs::read(manager_trust.join("site-ca.der")).unwrap());
        for file in ["site-ca.der", "node.der", "node-key.pk8"] {
            assert_eq!(
                fs::metadata(trust.join(file)).unwrap().permissions().mode() & 0o777,
                0o600,
                "{file}"
            );
        }
        let leaf = fs::read(trust.join("node.der")).expect("node certificate");
        let (trailing, certificate) =
            x509_parser::certificate::X509Certificate::from_der(&leaf).unwrap();
        assert!(trailing.is_empty());
        let names = &certificate
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names;
        // The SiteNode identity is the leaf's only name: peers never authenticate by address.
        let identity = certificate_identity_uri("fixture-site", node_id).unwrap();
        assert!(
            matches!(names.as_slice(), [GeneralName::URI(uri)] if *uri == identity),
            "{names:?}"
        );
        let usages = certificate.extended_key_usage().unwrap().unwrap();
        assert!(usages.value.server_auth && usages.value.client_auth);
        leaf_keys.push(fs::read(trust.join("node-key.pk8")).expect("node private key"));
    }
    assert_ne!(leaf_keys[0], leaf_keys[1]);
    assert!(manager_trust.join("site-ca-key.pk8").is_file());
    assert!(!worker_trust.join("site-ca-key.pk8").exists());
    assert!(!manager_trust.join("manager-key.pk8").exists());
    assert!(!manager_trust.join("manager.der").exists());
    assert!(!fixture.node_root.join("worker-key.pk8").exists());
    assert!(!fixture.node_root.join("worker.der").exists());
    assert!(fixture.node_root.join("manager.der").is_file());
    let folder = NodeFolderValidator::validate_declared(&fixture.node_root)
        .expect("validated provisioned node folder");
    assert_eq!(folder.observation_configuration().schema_version, 1);
    assert_eq!(folder.observation_configuration().interval_seconds, 5);
    assert_eq!(
        folder
            .observation_configuration()
            .execution_targets
            .iter()
            .map(|target| (
                target.execution_target_id.as_str(),
                RuntimeKind::try_from(target.kind).expect("Runtime kind"),
                matches!(
                    target.implementation,
                    Some(node_execution_target::Implementation::Simulated(_))
                ),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                support::WORKER_APPLICATION_TARGET_ID,
                RuntimeKind::Application,
                true,
            ),
            (support::WORKER_MODEL_TARGET_ID, RuntimeKind::Model, true),
        ]
    );
    assert_eq!(
        folder.observation_configuration_sha256(),
        &NodeObservationConfigurationDigest::for_configuration(folder.observation_configuration())
            .expect("digest of the installed observation configuration")
    );
}

#[tokio::test]
async fn startup_rejects_old_only_and_mixed_own_identity_layouts() {
    for old_only in [true, false] {
        let fixture = PrivateFixture::new();
        let trust = fixture.manager_root.join(SITE_TRUST_DIRECTORY);
        for (current, obsolete) in [
            (NODE_CERTIFICATE_FILE, "manager.der"),
            (NODE_PRIVATE_KEY_FILE, "manager-key.pk8"),
        ] {
            let current = trust.join(current);
            let obsolete = trust.join(obsolete);
            if old_only {
                fs::rename(current, obsolete)
                    .expect("replace manager node identity with old layout");
            } else {
                fs::copy(current, obsolete).expect("add mixed manager identity layout");
            }
        }
        let error =
            match Backend::prepare(fixture.manager_configuration(), std::future::pending()).await {
                Ok(_) => panic!("obsolete manager identity must not start"),
                Err(error) => error,
            };
        let mut cause = error.source();
        let mut rejected = false;
        while let Some(current) = cause {
            rejected |= current
                .to_string()
                .contains("obsolete manager-only node identity artifact");
            cause = current.source();
        }
        assert!(
            rejected,
            "expected obsolete manager identity rejection: {error}"
        );

        let fixture = PrivateFixture::new();
        let trust = fixture.node_root.join(SITE_TRUST_DIRECTORY);
        for (current, obsolete) in [
            (TRUST_ANCHOR_FILE, "site-ca.der"),
            (NODE_CERTIFICATE_FILE, "worker.der"),
            (NODE_PRIVATE_KEY_FILE, "worker-key.pk8"),
        ] {
            let current = trust.join(current);
            let obsolete = fixture.node_root.join(obsolete);
            if old_only {
                fs::rename(current, obsolete)
                    .expect("replace worker node identity with old layout");
            } else {
                fs::copy(current, obsolete).expect("add mixed worker identity layout");
            }
        }
        assert!(matches!(
            NodeFolderValidator::validate_declared(&fixture.node_root),
            Err(NodeFolderValidationError::ObsoleteIdentityLayout)
        ));
    }
}

#[tokio::test]
async fn joined_node_folder_starts_without_site_replica_and_second_custodian_fails() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = PrivateFixture::new();
    let pins = fixture
        .manager_root
        .join(SITE_TRUST_DIRECTORY)
        .join("worker-pins");
    assert_eq!(
        fs::metadata(&pins).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(pins.join(format!("{}.sha256", support::WORKER_NODE_ID)))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!fixture.node_root.join("site.yaml").exists());
    assert!(!fixture.node_root.join("node.yaml").exists());
    let worker = fixture
        .prepare_worker()
        .await
        .expect("joined node folder worker");
    assert!(worker.addresses().private.is_some());

    let error = match fixture.prepare_worker().await {
        Ok(_) => panic!("second node custodian must fail"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "Backend composition failed");
    let mut cause = error.source();
    let mut messages = Vec::new();
    while let Some(current) = cause {
        messages.push(current.to_string());
        cause = current.source();
    }
    assert_eq!(
        messages,
        [
            "worker node custody".to_owned(),
            "worker node custody is already held by another process".to_owned(),
        ]
    );
}

#[tokio::test]
async fn joined_node_folder_binds_a_private_host_name_resolved_at_startup() {
    let fixture = PrivateFixture::new();
    let reserved = support::ports::reserve_port();
    let port = reserved.port();
    let manifest = fixture.node_root.join(WORKER_MANIFEST_FILE);
    let text = fs::read_to_string(&manifest).expect("worker manifest");
    let named = text
        .lines()
        .map(|line| match line.strip_prefix("privateBindAddress: ") {
            Some(_) => format!("privateBindAddress: localhost:{port}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(named, text.trim_end());
    fs::write(&manifest, format!("{named}\n")).expect("named worker manifest");

    let worker = Backend::prepare(
        BackendConfiguration {
            role: BackendRole::JoinedNode {
                private_bind_address: None,
            },
            ..fixture.worker_configuration()
        },
        std::future::pending(),
    )
    .await
    .expect("joined node with a private host name");

    assert_eq!(
        worker.addresses().private,
        Some(std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    );
}

struct PrivateFixture {
    manager_root: support::TemporaryFixture,
    node_parent: TempDir,
    node_root: PathBuf,
}

impl PrivateFixture {
    fn new() -> Self {
        let manager_root = support::temporary_fast_fixture();
        let node_parent = crate::support::canonical_tempdir().expect("folder parent");
        let node_root = node_parent.path().join(support::WORKER_NODE_ID);
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_id_override("fixture-site")
        .with_node_id_override("fixture-node");
        let identities = request
            .resolve_identities(&mut RandomInitialSiteIdentityGenerator)
            .expect("manager fixture identities");
        let inputs =
            InitialSiteInputs::compose(&request, identities, &InitialSiteProbeReport::default())
                .expect("manager fixture inputs");
        let initialized_site = node_parent.path().join("initialized-manager");
        InitialSiteInputWriter::new(SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore_backend::model::site_declaration_rules(),
        ))
        .create(&initialized_site, &inputs)
        .expect("initialize manager fixture trust");
        fs::rename(
            initialized_site.join(SITE_TRUST_DIRECTORY),
            manager_root.join(SITE_TRUST_DIRECTORY),
        )
        .expect("install initialized manager trust in fixture");
        let node_id = add_worker(
            &manager_root,
            &node_root,
            support::WORKER_DISPLAY_NAME,
            "127.0.0.2",
        );
        assert_eq!(node_id, support::WORKER_NODE_ID);
        Self {
            manager_root,
            node_parent,
            node_root,
        }
    }

    async fn prepare_pair(&self) -> (Backend, Backend) {
        let manager = Backend::prepare(self.manager_configuration(), std::future::pending())
            .await
            .expect("manager private transport");
        let worker = self
            .prepare_worker()
            .await
            .expect("worker private transport");
        (manager, worker)
    }

    async fn prepare_pair_with_services(
        &self,
        manager_counts: InvocationCounts,
        worker_counts: InvocationCounts,
    ) -> (Backend, Backend) {
        let manager = Backend::prepare_with_private_service_assembly(
            self.manager_configuration(),
            PrivateServiceAssembly::new(move |context, routes| {
                QualificationService::add_routes(context, manager_counts, routes)
            }),
            std::future::pending(),
        )
        .await
        .expect("manager private transport");
        let worker = Backend::prepare_with_private_service_assembly(
            self.worker_configuration(),
            PrivateServiceAssembly::new(move |context, routes| {
                QualificationService::add_routes(context, worker_counts, routes)
            }),
            std::future::pending(),
        )
        .await
        .expect("worker private transport");
        (manager, worker)
    }

    async fn prepare_worker(&self) -> dotcore_backend::composition::CompositionResult<Backend> {
        Backend::prepare(self.worker_configuration(), std::future::pending()).await
    }

    fn manager_configuration(&self) -> BackendConfiguration {
        manager_role_configuration(support::configuration(&self.manager_root))
    }

    fn worker_configuration(&self) -> BackendConfiguration {
        joined_node_configuration(support::configuration(&self.node_root))
    }

    fn add_second_worker(&self) -> (PathBuf, String) {
        let root = self.node_parent.path().join("worker-two");
        let node_id = add_worker(&self.manager_root, &root, "Worker two", "127.0.0.3");
        (root, node_id)
    }

    fn manager_endpoint(&self, address: std::net::SocketAddr) -> Endpoint {
        support::private_tls_endpoint(address, &self.manager_root)
    }

    async fn manager_channel(&self, address: std::net::SocketAddr) -> Channel {
        self.manager_endpoint(address)
            .connect()
            .await
            .expect("manager mTLS channel")
    }

    async fn worker_channel(&self, address: std::net::SocketAddr) -> Channel {
        self.worker_channel_from(&self.node_root, address).await
    }

    async fn worker_channel_from(&self, root: &Path, address: std::net::SocketAddr) -> Channel {
        support::private_tls_endpoint(address, root)
            .connect()
            .await
            .expect("worker mTLS channel")
    }
}

fn manager_role_configuration(site_paths: dotcore_domain::SitePaths) -> BackendConfiguration {
    BackendConfiguration {
        site_paths,
        telemetry_directory: None,
        dotcore_sources: Default::default(),
        role: BackendRole::Manager(ServerConfiguration {
            management_bind_address: "127.0.0.1:0".parse().unwrap(),
            inference_bind_address: "127.0.0.1:0".parse().unwrap(),
            private_listener: dotcore_backend::composition::ManagerPrivateListener::Configured(
                "127.0.0.1:0".parse().unwrap(),
            ),
            allowed_origins: vec!["http://127.0.0.1:5173".parse().unwrap()],
        }),
    }
}

fn joined_node_configuration(site_paths: dotcore_domain::SitePaths) -> BackendConfiguration {
    BackendConfiguration {
        site_paths,
        telemetry_directory: None,
        dotcore_sources: Default::default(),
        role: BackendRole::JoinedNode {
            private_bind_address: Some("127.0.0.1:0".parse().unwrap()),
        },
    }
}

fn add_worker(manager_root: &Path, node_root: &Path, display_name: &str, host: &str) -> String {
    support::join_worker(
        manager_root,
        "https://127.0.0.1:9443",
        node_root,
        display_name,
        host.parse().expect("worker host"),
        // These workers bind an explicit private address, never the port their folder records.
        9444,
        None,
    )
    .node_id
}

fn replace_worker_identity_claim(
    manager_root: &Path,
    worker_root: &Path,
    site_id: &str,
    node_id: &str,
) {
    let trust = manager_root.join(SITE_TRUST_DIRECTORY);
    let ca_der = fs::read(trust.join(TRUST_ANCHOR_FILE)).expect("read trust anchor");
    let ca_key = KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(
            fs::read(trust.join(SITE_CA_PRIVATE_KEY_FILE)).expect("read CA key"),
        ),
        &PKCS_ECDSA_P256_SHA256,
    )
    .expect("parse CA key");
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(ca_der), ca_key)
        .expect("construct test issuer");
    let mut parameters = CertificateParams::new(vec!["127.0.0.2".to_owned()])
        .expect("worker certificate parameters");
    parameters.subject_alt_names.push(SanType::URI(
        certificate_identity_uri(site_id, node_id)
            .expect("worker identity URI")
            .try_into()
            .expect("worker identity IA5 string"),
    ));
    parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    parameters.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let key = KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(
            fs::read(
                worker_root
                    .join(SITE_TRUST_DIRECTORY)
                    .join(NODE_PRIVATE_KEY_FILE),
            )
            .expect("read worker key"),
        ),
        &PKCS_ECDSA_P256_SHA256,
    )
    .expect("parse worker key");
    let certificate = parameters
        .signed_by(&key, &issuer)
        .expect("worker certificate");
    fs::write(
        worker_root
            .join(SITE_TRUST_DIRECTORY)
            .join(NODE_CERTIFICATE_FILE),
        certificate.der(),
    )
    .expect("replace worker certificate");
    fs::write(
        trust.join(format!("worker-pins/{}.sha256", support::WORKER_NODE_ID)),
        format!(
            "{}\n",
            CertificateFingerprint::for_certificate(certificate.der()).as_str()
        ),
    )
    .expect("replace worker pin");
}

fn replace_manager_identity_with_expired(root: &Path) {
    let trust = root.join(SITE_TRUST_DIRECTORY);
    let ca_der = fs::read(trust.join(TRUST_ANCHOR_FILE)).expect("read trust anchor");
    let ca_key = KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(
            fs::read(trust.join(SITE_CA_PRIVATE_KEY_FILE)).expect("read CA key"),
        ),
        &PKCS_ECDSA_P256_SHA256,
    )
    .expect("parse CA key");
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(ca_der), ca_key)
        .expect("construct test issuer");
    let mut parameters = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("expired manager certificate parameters");
    parameters.subject_alt_names.push(SanType::URI(
        certificate_identity_uri("fixture-site", "fixture-node")
            .expect("manager identity URI")
            .try_into()
            .expect("manager identity IA5 string"),
    ));
    parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    parameters.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    parameters.not_before = OffsetDateTime::now_utc() - TimeDuration::hours(2);
    parameters.not_after = OffsetDateTime::now_utc() - TimeDuration::hours(1);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("manager key");
    let certificate = parameters
        .signed_by(&key, &issuer)
        .expect("expired manager certificate");

    fs::write(trust.join(NODE_CERTIFICATE_FILE), certificate.der())
        .expect("replace manager certificate");
    fs::write(trust.join(NODE_PRIVATE_KEY_FILE), key.serialize_der()).expect("replace manager key");
}
