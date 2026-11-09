use std::{collections::VecDeque, time::Duration};

use dotcore_domain::{
    SiteNodeId,
    clock::ManualClock,
    timing::{NodeControlTiming, Timing},
};
use dotcore_proto::{
    dotcore::{
        application::{
            private::v1::{
                ApplicationAssignment, ApplicationCleanupReceipt,
                ApplicationControllerStatusSection, ApplicationStatusFailureCode,
            },
            v1::ApplicationDeploymentDeclaration,
        },
        catalog::v1::ModelParallelShape,
        deployment::v1::ExactPlacement,
        model::{
            private::v1::ModelControllerStatusSection,
            v1::{
                ModelDeploymentDeclaration, ModelDeploymentDesiredState,
                ModelDeploymentLifecycleTransition,
            },
        },
        observation::v1::{ObservationUnavailable, RuntimeObservation, runtime_observation},
        site::{
            private::v1::{
                ControllerStatusReceipt, ControllerStatusSection, DesiredViewSectionKind,
                NodeControlOpen, NodeHardwareObservation, NodeObservationSnapshot,
                NodeViewAcknowledgement, NodeViewReference, SupportedDesiredViewSection,
                controller_status_section, desired_view_section, node_hardware_observation,
                node_view_acknowledgement,
            },
            v1::ExecutionTarget,
        },
    },
    google::protobuf::Empty,
};
use tokio::sync::mpsc;

use super::*;
use crate::{
    node::{
        control::CoordinatorConnectionState,
        observation_configuration::{
            NodeObservationConfigurationDigest, node_observation_configuration,
        },
    },
    repository::SiteRepository,
    view::{
        contract::{APPLICATION_SECTION_SCHEMA_VERSION, MODEL_SECTION_SCHEMA_VERSION},
        desired::{
            ApplicationNodeViewOwner, ControllerStatusError, ModelStatusOwner, NodeViewInstaller,
            NodeViewOwnerRules,
        },
        fencing::{AcceptedNodeViewAuthority, ManagerSession, NodeViewProjection, ViewAcceptance},
    },
};

/// Longest real wait for one stream message before a test fails; behavior never waits on it.
const RESPONSE_DEADLINE: Duration = Duration::from_secs(10);

/// Node silence of the fixtures, long enough that real time never ends a stream on its own.
const TEST_NODE_SILENCE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn observation_and_status_answers_classify_each_registry_and_owner_outcome() {
    let fixture = Fixture::new();
    let mut node = fixture.open("worker-one");
    let opened = node.opened(&incarnation('1')).await;
    assert_eq!(opened.manager_session_id, "a".repeat(32));
    let view = opened.view.expect("opened view");
    let identity = view.identity.as_ref().expect("view identity");
    assert_eq!(
        (identity.publication_sequence, identity.view_revision),
        (1, 1),
        "the opened view is the stream's first publication"
    );
    assert_eq!(identity.node_incarnation_id, incarnation('1').as_str());

    let observation = fixture.observation("worker-one", &incarnation('1'), 1);
    node.send(Message::Observation(observation.clone()));
    let accepted = node.observation_accepted().await;
    assert_eq!(accepted.accepted_revision, 1);
    // A node re-sends its current observation on every open: the exact retry is replayed.
    node.send(Message::Observation(observation.clone()));
    assert_eq!(node.observation_accepted().await, accepted);

    // Each rejection carries the code that classifies its registry or owner outcome.
    let conflicting = NodeObservation {
        private_port: Some(9000),
        ..observation.clone()
    };
    node.send(Message::Observation(conflicting));
    node.expect_rejection(
        NodeControlMessageKind::Observation,
        1,
        NodeControlRejectionCode::Invalid,
    )
    .await;
    let stale_configuration = NodeObservation {
        revision: 2,
        observation_configuration_sha256: "0".repeat(64),
        ..observation.clone()
    };
    node.send(Message::Observation(stale_configuration));
    node.expect_rejection(
        NodeControlMessageKind::Observation,
        2,
        NodeControlRejectionCode::Conflict,
    )
    .await;
    let incomplete = NodeObservation {
        revision: 3,
        snapshot: None,
        ..observation
    };
    node.send(Message::Observation(incomplete));
    node.expect_rejection(
        NodeControlMessageKind::Observation,
        3,
        NodeControlRejectionCode::Invalid,
    )
    .await;

    node.send(Message::Status(status(&view, 1)));
    assert_eq!(node.status_accepted().await.accepted_status_sequence, 1);
    fixture
        .status_owner
        .reply(Err(ControllerStatusError::Rejected(anyhow::anyhow!(
            "contradicts manager intent"
        ))));
    node.send(Message::Status(status(&view, 2)));
    node.expect_rejection(
        NodeControlMessageKind::Status,
        2,
        NodeControlRejectionCode::Invalid,
    )
    .await;
    fixture
        .status_owner
        .reply(Err(ControllerStatusError::Retryable(anyhow::anyhow!(
            "state write failed"
        ))));
    node.send(Message::Status(status(&view, 3)));
    node.expect_rejection(
        NodeControlMessageKind::Status,
        3,
        NodeControlRejectionCode::Unavailable,
    )
    .await;
    let mut unpublished = status(&view, 4);
    unpublished.view.as_mut().expect("reference").view_revision += 1;
    node.send(Message::Status(unpublished));
    node.expect_rejection(
        NodeControlMessageKind::Status,
        4,
        NodeControlRejectionCode::Conflict,
    )
    .await;
    node.send(Message::Status(NodeControllerStatus {
        view: None,
        ..status(&view, 5)
    }));
    node.expect_rejection(
        NodeControlMessageKind::Status,
        5,
        NodeControlRejectionCode::Invalid,
    )
    .await;
    node.send(Message::Status(status(&view, 6)));
    assert_eq!(node.status_accepted().await.accepted_status_sequence, 6);
    node.send(Message::Status(status(&view, 6)));
    assert_eq!(
        node.status_accepted().await.accepted_status_sequence,
        6,
        "an exact retry is replayed"
    );
    node.send(Message::Status(status(&view, 2)));
    node.expect_rejection(
        NodeControlMessageKind::Status,
        2,
        NodeControlRejectionCode::Conflict,
    )
    .await;

    node.send(Message::AcknowledgeView(acknowledgement(&view, 1)));
    let acknowledged = node.reply().await;
    assert!(matches!(
        acknowledged,
        Reply::AcknowledgementAccepted(response) if response.accepted_acknowledgement_sequence == 1
    ));
    let mut unoffered = acknowledgement(&view, 2);
    unoffered.view.as_mut().expect("reference").view_revision += 1;
    node.send(Message::AcknowledgeView(unoffered));
    node.expect_rejection(
        NodeControlMessageKind::Acknowledgement,
        2,
        NodeControlRejectionCode::Conflict,
    )
    .await;
    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Connected
    );
}

#[tokio::test]
async fn identity_mismatches_end_the_stream_with_the_documented_codes() {
    let fixture = Fixture::new();

    let mut not_opened = fixture.open("worker-one");
    not_opened.send(Message::Heartbeat(Empty {}));
    assert_eq!(
        not_opened.ended().await.code(),
        tonic::Code::InvalidArgument
    );

    let mut other_node = fixture.open("worker-one");
    other_node.send(Message::Open(NodeControlOpen {
        node_id: "worker-two".to_owned(),
        ..open(&incarnation('1'))
    }));
    assert_eq!(
        other_node.ended().await.code(),
        tonic::Code::PermissionDenied
    );

    let mut other_incarnation = fixture.open("worker-one");
    other_incarnation.opened(&incarnation('1')).await;
    other_incarnation.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('2'),
        1,
    )));
    assert_eq!(
        other_incarnation.ended().await.code(),
        tonic::Code::FailedPrecondition
    );

    let mut other_session = fixture.open("worker-one");
    let view = other_session
        .opened(&incarnation('1'))
        .await
        .view
        .expect("view");
    let mut stale_session = status(&view, 1);
    stale_session
        .view
        .as_mut()
        .expect("reference")
        .manager_session_id = "b".repeat(32);
    other_session.send(Message::Status(stale_session));
    assert_eq!(
        other_session.ended().await.code(),
        tonic::Code::FailedPrecondition
    );

    let mut reopened = fixture.open("worker-one");
    reopened.opened(&incarnation('1')).await;
    reopened.send(Message::Open(open(&incarnation('1'))));
    assert_eq!(reopened.ended().await.code(), tonic::Code::InvalidArgument);

    let mut empty = fixture.open("worker-one");
    empty.opened(&incarnation('1')).await;
    empty
        .sender
        .send(Ok(StreamNodeControlRequest { message: None }))
        .expect("send empty message");
    assert_eq!(empty.ended().await.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_newer_stream_of_the_same_incarnation_aborts_the_older_without_disconnecting() {
    let fixture = Fixture::new();
    let mut older = fixture.open("worker-one");
    let view = older.opened(&incarnation('1')).await.view.expect("view");
    older.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    older.observation_accepted().await;
    older.send(Message::Status(status(&view, 1)));
    older.status_accepted().await;
    let serving_before = fixture.serving("worker-one").expect("serving evidence");

    let mut newer = fixture.open("worker-one");
    newer.opened(&incarnation('1')).await;
    assert_eq!(older.ended().await.code(), tonic::Code::Aborted);

    // The older stream recorded its close before ending, and it changed nothing.
    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Connected
    );
    let serving_after = fixture.serving("worker-one").expect("serving evidence");
    assert_eq!(serving_after.status, serving_before.status);
    assert!(serving_after.connected_until >= serving_before.connected_until);
    newer.send(Message::Status(status(&view, 2)));
    assert_eq!(newer.status_accepted().await.accepted_status_sequence, 2);
}

#[tokio::test]
async fn a_new_incarnation_is_refused_while_the_old_stream_is_live_and_accepted_once_it_closed() {
    let fixture = Fixture::new();
    let mut first = fixture.open("worker-one");
    first.opened(&incarnation('1')).await;
    first.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    first.observation_accepted().await;

    let mut refused = fixture.open("worker-one");
    refused.send(Message::Open(open(&incarnation('2'))));
    assert_eq!(
        refused.ended().await.code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Connected
    );

    first.close().await;
    let mut restarted = fixture.open("worker-one");
    let view = restarted
        .opened(&incarnation('2'))
        .await
        .view
        .expect("view");
    assert_eq!(
        view.identity.expect("identity").node_incarnation_id,
        incarnation('2').as_str()
    );
    restarted.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('2'),
        1,
    )));
    restarted.observation_accepted().await;
    let observation = fixture
        .registry
        .observation(&SiteNodeId::for_test("worker-one"))
        .expect("observation");
    assert_eq!(observation.incarnation_id, incarnation('2').as_str());
    assert_eq!(
        observation.replaced_incarnations,
        [incarnation('1').as_str().to_owned()].into()
    );
}

#[tokio::test]
async fn a_closed_stream_disconnects_at_once_while_serving_keeps_the_node_silence() {
    let fixture = Fixture::new();
    let mut node = fixture.open("worker-one");
    let view = node.opened(&incarnation('1')).await.view.expect("view");
    node.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    node.observation_accepted().await;
    node.send(Message::Status(status(&view, 1)));
    node.status_accepted().await;
    let before = fixture.serving("worker-one").expect("serving evidence");

    node.close().await;

    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Disconnected
    );
    assert_eq!(
        fixture.serving("worker-one"),
        Some(before.clone()),
        "serving evidence, and so every serving target and in-flight request, outlives the close"
    );
    fixture.clock.advance(TEST_NODE_SILENCE / 2);
    assert!(before.connected_until > fixture.coordinator.clock().now());
    fixture
        .clock
        .advance(TEST_NODE_SILENCE / 2 + Duration::from_secs(1));
    assert!(
        before.connected_until < fixture.coordinator.clock().now(),
        "serving ends at the silence bound"
    );
}

#[tokio::test]
async fn a_streamed_status_stays_serving_evidence_for_as_long_as_heartbeats_keep_the_node_live() {
    let node_silence = Duration::from_secs(10);
    let fixture = Fixture::with_node_silence(node_silence);
    let mut node = fixture.open("worker-one");
    let view = node.opened(&incarnation('1')).await.view.expect("view");
    node.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    node.observation_accepted().await;
    node.send(Message::Status(status(&view, 1)));
    node.status_accepted().await;
    let evidence = || {
        fixture
            .coordinator
            .serving_evidence()
            .into_iter()
            .find(|evidence| evidence.node_id == "worker-one")
            .expect("serving evidence")
    };
    let received = evidence().status.expect("accepted status").received;

    for _ in 0..5 {
        fixture.clock.advance(Duration::from_secs(3));
        node.send(Message::Heartbeat(Empty {}));
        fixture
            .wait_for_contact_within("worker-one", Duration::from_secs(1))
            .await;
    }
    let now = fixture.coordinator.clock().now();
    let live = evidence();
    assert!(now >= received + Duration::from_secs(15));
    assert_eq!(
        live.status.as_ref().map(|status| status.received),
        Some(received),
        "no new status arrived"
    );
    assert!(
        live.connected_until > now + node_silence - Duration::from_secs(1),
        "heartbeats keep the node live past the status age: {:?}",
        live.connected_until - now
    );

    node.close().await;
    assert_eq!(
        evidence().connected_until,
        live.connected_until,
        "the close itself leaves serving until the silence bound"
    );
}

#[tokio::test]
async fn a_silent_stream_ends_at_the_node_silence_and_heartbeats_keep_it_open() {
    let fixture = Fixture::new();
    let mut node = fixture.open("worker-one");
    node.opened(&incarnation('1')).await;
    node.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    node.observation_accepted().await;

    let quiet = TEST_NODE_SILENCE * 2 / 3;
    fixture.clock.advance(quiet);
    node.send(Message::Heartbeat(Empty {}));
    fixture
        .wait_for_contact_within("worker-one", quiet / 2)
        .await;
    fixture.clock.advance(quiet);
    node.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        2,
    )));
    assert_eq!(
        node.observation_accepted().await.accepted_revision,
        2,
        "a heartbeat is contact, so the stream outlives a silence bound since its open"
    );
    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Connected
    );

    fixture
        .clock
        .advance(TEST_NODE_SILENCE + Duration::from_secs(1));
    assert_eq!(node.ended().await.code(), tonic::Code::DeadlineExceeded);
    assert_eq!(
        fixture.connection("worker-one"),
        CoordinatorConnectionState::Disconnected
    );
}

#[tokio::test]
async fn a_published_view_reaches_the_open_stream_without_a_poll() {
    let fixture = Fixture::new();
    let mut node = fixture.open("worker-one");
    let opened = node.opened(&incarnation('1')).await.view.expect("view");
    assert!(model_deployment_ids(&opened).is_empty());

    fixture
        .site
        .mutate_model_deployments(|transaction| {
            transaction
                .deployments
                .push(present_deployment("deployment-one"));
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("accepted site change");

    let published = node.publication().await;
    let identity = published.identity.as_ref().expect("identity");
    assert_eq!(
        (identity.publication_sequence, identity.view_revision),
        (2, 2)
    );
    assert_eq!(model_deployment_ids(&published), ["deployment-one"]);
}

#[tokio::test]
async fn a_view_that_cannot_be_built_at_open_answers_with_the_last_published_view() {
    let fixture = Fixture::new();
    let mut first = fixture.open("worker-one");
    let published = first.opened(&incarnation('1')).await.view.expect("view");
    first.send(Message::Observation(fixture.observation(
        "worker-one",
        &incarnation('1'),
        1,
    )));
    first.observation_accepted().await;
    first.close().await;

    fixture.applications.set_unbuildable(true);
    let mut reopened = fixture.open("worker-one");
    let fallback = reopened.opened(&incarnation('1')).await.view.expect("view");
    let identity = fallback.identity.as_ref().expect("identity");
    let published_identity = published.identity.as_ref().expect("identity");
    assert_eq!(identity.publication_sequence, 1);
    assert_eq!(
        (identity.view_revision, &identity.content_sha256),
        (
            published_identity.view_revision,
            &published_identity.content_sha256
        ),
        "the open is answered with the last published view"
    );
    reopened.send(Message::AcknowledgeView(acknowledgement(&fallback, 1)));
    assert!(matches!(
        reopened.reply().await,
        Reply::AcknowledgementAccepted(response) if response.accepted_acknowledgement_sequence == 1
    ));

    fixture.applications.set_unbuildable(false);
    fixture
        .clock
        .advance(Timing::test().node_control.view_republish_interval);
    let recovered = reopened.publication().await;
    let identity = recovered.identity.as_ref().expect("identity");
    assert_eq!(
        (identity.publication_sequence, identity.view_revision),
        (2, published_identity.view_revision),
        "the republication recovers once the view builds again"
    );
}

#[tokio::test]
async fn an_open_without_any_published_view_is_unavailable_until_the_view_builds() {
    let fixture = Fixture::new();
    fixture.applications.set_unbuildable(true);
    let mut refused = fixture.open("worker-one");
    refused.send(Message::Open(open(&incarnation('1'))));
    assert_eq!(refused.ended().await.code(), tonic::Code::Unavailable);

    fixture.applications.set_unbuildable(false);
    let mut retried = fixture.open("worker-one");
    let view = retried.opened(&incarnation('1')).await.view.expect("view");
    assert_eq!(view.identity.expect("identity").view_revision, 1);
}

#[tokio::test(start_paused = true)]
async fn an_unchanged_view_is_republished_before_the_lease_expires_and_actions_stay_allowed() {
    // Paused Tokio time drives both the manager's clock and the node's lease, and auto-advances
    // only to the next republication, the earliest pending wait.
    let fixture = Fixture::with_node_silence(Duration::from_secs(3_600));
    let timing = Timing::test().node_control;
    let lease = timing.action_lease;
    // Republications that together outlast the lease.
    let republications = lease.as_millis() / timing.view_republish_interval.as_millis() + 1;
    let (authority, installer) = node_view(&fixture, lease);
    let started = tokio::time::Instant::now();
    let mut node = fixture.open("worker-one");
    // A failure deadline would be the earliest wait and fire first under paused time.
    node.deadline = None;
    let opened = node.opened(&incarnation('1')).await;
    let stream = authority
        .open_stream(ManagerSession::parse(opened.manager_session_id).expect("manager session"))
        .expect("node stream");
    let open = open(&incarnation('1'));
    assert_eq!(
        installer
            .install_publication(&open, &stream, &opened.view.expect("view"))
            .expect("installed view"),
        ViewAcceptance::Installed
    );

    for expected_sequence in 2..=(republications as u64 + 1) {
        let republished = node.publication().await;
        let identity = republished.identity.as_ref().expect("identity");
        assert_eq!(
            (identity.publication_sequence, identity.view_revision),
            (expected_sequence, 1),
            "the unchanged view is republished under a new sequence"
        );
        assert_eq!(
            installer
                .install_publication(&open, &stream, &republished)
                .expect("refreshed view"),
            ViewAcceptance::Refreshed
        );
    }

    assert!(tokio::time::Instant::now() - started > lease);
    assert!(
        authority.action_permit().is_ok(),
        "republications keep actions allowed past the lease"
    );
    tokio::time::advance(lease).await;
    assert_eq!(
        authority.action_permit().map(|_| ()),
        Err(crate::view::fencing::ActionPermitError::Expired),
        "without accepted republications the lease expires"
    );
}

#[tokio::test]
async fn replies_are_never_dropped_and_an_unread_stream_ends_resource_exhausted() {
    let fixture = Fixture::with_configuration(
        NodeControlTiming {
            node_silence: TEST_NODE_SILENCE,
            view_republish_interval: Duration::from_secs(20),
            ..Timing::test().node_control
        },
        NodeControlStreamConfiguration::new(2).expect("configuration"),
    );
    let mut node = fixture.open("worker-one");
    node.opened(&incarnation('1')).await;
    let observation = fixture.observation("worker-one", &incarnation('1'), 1);
    for _ in 0..3 {
        node.send(Message::Observation(observation.clone()));
    }
    // Nothing is read until the manager gave up on the stream, which it records as a close.
    fixture
        .wait_for_connection("worker-one", CoordinatorConnectionState::Disconnected)
        .await;

    assert_eq!(node.observation_accepted().await.accepted_revision, 1);
    assert_eq!(node.observation_accepted().await.accepted_revision, 1);
    assert_eq!(node.ended().await.code(), tonic::Code::ResourceExhausted);
}

#[tokio::test]
async fn outbound_keeps_every_reply_and_only_the_latest_view() {
    let outbound = Outbound::new(4);
    let reply = |sequence| StreamNodeControlResponse {
        message: Some(stream_node_control_response::Message::Rejected(
            NodeControlRejection {
                kind: NodeControlMessageKind::Status as i32,
                sequence,
                code: NodeControlRejectionCode::Conflict as i32,
            },
        )),
    };
    let view = |revision| NodeDesiredView {
        identity: Some(
            dotcore_proto::dotcore::site::private::v1::NodeViewIdentity {
                view_revision: revision,
                ..Default::default()
            },
        ),
        ..Default::default()
    };
    outbound.publish(view(1));
    outbound.reply(reply(1)).expect("first reply queued");
    outbound.publish(view(2));
    outbound.reply(reply(2)).expect("second reply queued");

    assert_eq!(outbound.next().await.expect("reply").ok(), Some(reply(1)));
    assert_eq!(outbound.next().await.expect("reply").ok(), Some(reply(2)));
    let latest = outbound.next().await.expect("view").expect("view message");
    assert!(matches!(
        latest.message,
        Some(stream_node_control_response::Message::ViewPublished(view))
            if view.identity.as_ref().is_some_and(|identity| identity.view_revision == 2)
    ));
    outbound.publish(view(3));
    outbound.finish(Some(Status::aborted("replaced")));
    assert_eq!(
        outbound
            .next()
            .await
            .expect("end")
            .err()
            .map(|status| status.code()),
        Some(tonic::Code::Aborted),
        "a failed stream drops its unsent view"
    );
    assert!(outbound.next().await.is_none());
}

#[test]
fn a_stream_must_queue_at_least_one_reply() {
    assert_eq!(
        NodeControlStreamConfiguration::new(0),
        Err(NodeControlConfigurationError::OutboundReplies)
    );
}

type Message = stream_node_control_request::Message;
type Reply = stream_node_control_response::Message;

/// Manager node control streams over a real site with injected status owners and a manual clock.
struct Fixture {
    _directory: tempfile::TempDir,
    site: SiteRepository,
    clock: ManualClock,
    coordinator: ManagerNodeControlCoordinator,
    registry: NodeRegistry,
    status_owner: Arc<ScriptedModelStatus>,
    applications: Arc<TestApplications>,
    streams: ManagerNodeControlStreams,
}

impl Fixture {
    fn new() -> Self {
        Self::with_node_silence(TEST_NODE_SILENCE)
    }

    fn with_node_silence(node_silence: Duration) -> Self {
        Self::with_configuration(
            NodeControlTiming {
                node_silence,
                ..Timing::test().node_control
            },
            NodeControlStreamConfiguration::default(),
        )
    }

    fn with_configuration(
        timing: NodeControlTiming,
        configuration: NodeControlStreamConfiguration,
    ) -> Self {
        let directory =
            dotcore_domain::test_support::canonical_tempdir().expect("manager site directory");
        let site_path = directory.path().join("site.yaml");
        std::fs::write(&site_path, SITE).expect("site file");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated site");
        let clock = ManualClock::new();
        let coordinator = ManagerNodeControlCoordinator::new(
            ManagerSession::parse("a".repeat(32)).expect("manager session"),
            timing,
            Arc::new(clock.clone()),
        );
        let registry = NodeRegistry::with_coordinator(Arc::new(site.clone()), coordinator.clone());
        let status_owner = Arc::new(ScriptedModelStatus::default());
        let applications = Arc::new(TestApplications::default());
        let publisher = ManagerNodeViewPublisher::new(
            site.clone(),
            coordinator.clone(),
            Arc::clone(&status_owner) as Arc<dyn ModelStatusOwner>,
            Arc::clone(&applications) as Arc<dyn ApplicationNodeViewOwner>,
        );
        let streams = ManagerNodeControlStreams::new(registry.clone(), publisher, configuration);
        Self {
            _directory: directory,
            site,
            clock,
            coordinator,
            registry,
            status_owner,
            applications,
            streams,
        }
    }

    /// Opens one stream authenticated as `node_id`, without sending anything on it.
    fn open(&self, node_id: &str) -> NodeStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        let responses = self.streams.serve(
            "site-one".to_owned(),
            node_id.to_owned(),
            tokio_stream::wrappers::UnboundedReceiverStream::new(receiver).boxed(),
        );
        NodeStream {
            sender,
            responses,
            deadline: Some(RESPONSE_DEADLINE),
        }
    }

    /// A complete valid observation of `node_id` for the site's current topology.
    fn observation(
        &self,
        node_id: &str,
        incarnation: &NodeIncarnation,
        revision: u64,
    ) -> NodeObservation {
        let site = self.site.snapshot().expect("site snapshot");
        let configuration =
            node_observation_configuration(&site, node_id).expect("observation configuration");
        let digest = NodeObservationConfigurationDigest::for_configuration(&configuration)
            .expect("configuration digest");
        let unavailable = || ObservationUnavailable {
            code: "UNAVAILABLE".to_owned(),
            message: "Unavailable in this test.".to_owned(),
        };
        NodeObservation {
            site_id: site.site_id.clone(),
            node_id: node_id.to_owned(),
            incarnation_id: incarnation.as_str().to_owned(),
            revision,
            snapshot: Some(NodeObservationSnapshot {
                hardware: Some(NodeHardwareObservation {
                    result: Some(node_hardware_observation::Result::Unavailable(unavailable())),
                }),
                runtimes: configuration
                    .execution_targets
                    .iter()
                    .map(|target| RuntimeObservation {
                        execution_target_id: target.execution_target_id.clone(),
                        kind: target.kind,
                        result: Some(runtime_observation::Result::Unavailable(unavailable())),
                    })
                    .collect(),
            }),
            observation_configuration_sha256: digest.as_str().to_owned(),
            private_port: None,
        }
    }

    fn connection(&self, node_id: &str) -> CoordinatorConnectionState {
        self.coordinator
            .observation(&SiteNodeId::for_test(node_id))
            .expect("node observation")
            .connection_state
    }

    fn serving(&self, node_id: &str) -> Option<ServingSnapshot> {
        self.coordinator
            .serving_evidence()
            .into_iter()
            .find(|evidence| evidence.node_id == node_id)
            .map(|evidence| ServingSnapshot {
                connected_until: evidence.connected_until,
                status: evidence.status.map(|status| status.received),
            })
    }

    /// Waits until `node_id` reads as `expected`, failing after a real deadline.
    async fn wait_for_connection(&self, node_id: &str, expected: CoordinatorConnectionState) {
        let deadline = std::time::Instant::now() + RESPONSE_DEADLINE;
        loop {
            let observed = self
                .coordinator
                .observation(&SiteNodeId::for_test(node_id))
                .map(|observation| observation.connection_state);
            if observed == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "connection stayed {observed:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until `node_id`'s latest contact is younger than `age`, failing after a real
    /// deadline with the last observed age.
    async fn wait_for_contact_within(&self, node_id: &str, age: Duration) {
        let deadline = std::time::Instant::now() + RESPONSE_DEADLINE;
        loop {
            let contact = self.coordinator.liveness_ages()[node_id].contact;
            if contact < age {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "contact age stayed at {contact:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The parts of one node's serving evidence a stream change must keep.
#[derive(Clone, Debug, PartialEq)]
struct ServingSnapshot {
    connected_until: tokio::time::Instant,
    status: Option<tokio::time::Instant>,
}

/// The node's end of one stream.
struct NodeStream {
    sender: mpsc::UnboundedSender<Result<StreamNodeControlRequest, Status>>,
    responses: BoxStream<'static, Result<StreamNodeControlResponse, Status>>,
    /// Longest wait for each message before the test fails, when one applies.
    deadline: Option<Duration>,
}

impl NodeStream {
    fn send(&self, message: Message) {
        self.sender
            .send(Ok(StreamNodeControlRequest {
                message: Some(message),
            }))
            .expect("stream accepts node messages");
    }

    async fn next(&mut self) -> Option<Result<StreamNodeControlResponse, Status>> {
        match self.deadline {
            Some(deadline) => tokio::time::timeout(deadline, self.responses.next())
                .await
                .expect("stream message within the deadline"),
            None => self.responses.next().await,
        }
    }

    /// Sends the open of `incarnation` and returns the manager's answer.
    async fn opened(&mut self, incarnation: &NodeIncarnation) -> NodeControlOpened {
        self.send(Message::Open(open(incarnation)));
        match self.next().await {
            Some(Ok(StreamNodeControlResponse {
                message: Some(Reply::Opened(opened)),
            })) => opened,
            other => panic!("expected opened, got {other:?}"),
        }
    }

    /// Returns the next reply, skipping view publications.
    async fn reply(&mut self) -> Reply {
        loop {
            match self.next().await {
                Some(Ok(StreamNodeControlResponse {
                    message: Some(Reply::ViewPublished(_)),
                })) => continue,
                Some(Ok(StreamNodeControlResponse {
                    message: Some(reply),
                })) => return reply,
                other => panic!("expected a reply, got {other:?}"),
            }
        }
    }

    /// Returns the next view publication, skipping replies.
    async fn publication(&mut self) -> NodeDesiredView {
        loop {
            match self.next().await {
                Some(Ok(StreamNodeControlResponse {
                    message: Some(Reply::ViewPublished(view)),
                })) => return view,
                Some(Ok(_)) => continue,
                other => panic!("expected a view publication, got {other:?}"),
            }
        }
    }

    async fn observation_accepted(
        &mut self,
    ) -> dotcore_proto::dotcore::site::private::v1::NodeObservationAcceptance {
        match self.reply().await {
            Reply::ObservationAccepted(response) => response,
            other => panic!("expected an accepted observation, got {other:?}"),
        }
    }

    async fn status_accepted(
        &mut self,
    ) -> dotcore_proto::dotcore::site::private::v1::NodeControllerStatusAcceptance {
        match self.reply().await {
            Reply::StatusAccepted(response) => response,
            other => panic!("expected an accepted status, got {other:?}"),
        }
    }

    async fn expect_rejection(
        &mut self,
        kind: NodeControlMessageKind,
        sequence: u64,
        code: NodeControlRejectionCode,
    ) {
        assert_eq!(
            self.reply().await,
            Reply::Rejected(NodeControlRejection {
                kind: kind as i32,
                sequence,
                code: code as i32,
            })
        );
    }

    /// Returns the status that ended the stream, skipping view publications.
    async fn ended(&mut self) -> Status {
        loop {
            match self.next().await {
                Some(Ok(StreamNodeControlResponse {
                    message: Some(Reply::ViewPublished(_)),
                })) => continue,
                Some(Err(status)) => {
                    assert!(self.next().await.is_none(), "nothing follows the end");
                    return status;
                }
                other => panic!("expected the stream to end, got {other:?}"),
            }
        }
    }

    /// Closes the node's side and waits until the manager ended the stream.
    async fn close(mut self) {
        let (closed, _) = mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.sender, closed));
        loop {
            match self.next().await {
                None => return,
                Some(Ok(_)) => continue,
                Some(Err(status)) => panic!("expected a clean end, got {status:?}"),
            }
        }
    }
}

/// Model status owner answering each report with the next scripted reply, or accepting it.
#[derive(Default)]
struct ScriptedModelStatus {
    replies: Mutex<VecDeque<Result<Vec<ControllerStatusReceipt>, ControllerStatusError>>>,
}

impl ScriptedModelStatus {
    fn reply(&self, reply: Result<Vec<ControllerStatusReceipt>, ControllerStatusError>) {
        self.replies
            .lock()
            .expect("scripted replies")
            .push_back(reply);
    }
}

impl ModelStatusOwner for ScriptedModelStatus {
    fn accept_remote_status(
        &self,
        _: &str,
        _: &NodeViewProjection,
        _: &ModelControllerStatusSection,
        _: &ManagerSession,
        _: &NodeIncarnation,
        _: u64,
    ) -> Result<Vec<ControllerStatusReceipt>, ControllerStatusError> {
        self.replies
            .lock()
            .expect("scripted replies")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

/// Application owner of a site without application intent, which can be made to supply an
/// assignment no view accepts, so building any view fails.
#[derive(Default)]
struct TestApplications {
    unbuildable: std::sync::atomic::AtomicBool,
}

impl TestApplications {
    fn set_unbuildable(&self, unbuildable: bool) {
        self.unbuildable
            .store(unbuildable, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ApplicationNodeViewOwner for TestApplications {
    fn assignments(&self, _: &[ApplicationDeploymentDeclaration]) -> Vec<ApplicationAssignment> {
        if self.unbuildable.load(std::sync::atomic::Ordering::SeqCst) {
            return vec![ApplicationAssignment::default()];
        }
        Vec::new()
    }

    fn accept_remote_status(
        &self,
        _: &str,
        _: &NodeViewProjection,
        _: &ApplicationControllerStatusSection,
        _: &ManagerSession,
        _: &NodeIncarnation,
        _: u64,
    ) -> Vec<ApplicationCleanupReceipt> {
        Vec::new()
    }
}

/// A node-side view authority and installer for worker-one's installed targets.
fn node_view(fixture: &Fixture, lease: Duration) -> (AcceptedNodeViewAuthority, NodeViewInstaller) {
    let authority = AcceptedNodeViewAuthority::new(
        "site-one",
        "worker-one",
        incarnation('1'),
        ManagerSession::parse("0".repeat(32)).expect("discovery session"),
        lease,
    )
    .expect("view authority");
    let installed: Vec<ExecutionTarget> = fixture
        .site
        .snapshot()
        .expect("site snapshot")
        .execution_targets
        .into_iter()
        .filter(|target| target.site_node_id == "worker-one")
        .collect();
    let installer = NodeViewInstaller::new(
        authority.clone(),
        &installed,
        NodeViewOwnerRules {
            valid_model_assignment: |_| true,
            classify_application_assignment: |_, _, _| Ok::<(), ApplicationStatusFailureCode>(()),
        },
    );
    (authority, installer)
}

fn open(incarnation: &NodeIncarnation) -> NodeControlOpen {
    NodeControlOpen {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        node_incarnation_id: incarnation.as_str().to_owned(),
        supported_sections: vec![
            SupportedDesiredViewSection {
                kind: DesiredViewSectionKind::Model as i32,
                minimum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
                maximum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
            },
            SupportedDesiredViewSection {
                kind: DesiredViewSectionKind::Application as i32,
                minimum_schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
                maximum_schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
            },
        ],
    }
}

fn reference(view: &NodeDesiredView) -> NodeViewReference {
    let identity = view.identity.as_ref().expect("view identity");
    NodeViewReference {
        site_id: identity.site_id.clone(),
        node_id: identity.node_id.clone(),
        manager_session_id: identity.manager_session_id.clone(),
        node_incarnation_id: identity.node_incarnation_id.clone(),
        view_revision: identity.view_revision,
        content_sha256: identity.content_sha256.clone(),
    }
}

fn status(view: &NodeDesiredView, status_sequence: u64) -> NodeControllerStatus {
    NodeControllerStatus {
        view: Some(reference(view)),
        status_sequence,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(
                ModelControllerStatusSection::default(),
            )),
        }],
    }
}

fn acknowledgement(
    view: &NodeDesiredView,
    acknowledgement_sequence: u64,
) -> NodeViewAcknowledgement {
    NodeViewAcknowledgement {
        view: Some(reference(view)),
        acknowledgement_sequence,
        outcome: Some(node_view_acknowledgement::Outcome::Accepted(Empty {})),
    }
}

fn present_deployment(id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: id.to_owned(),
        snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
            "model-one",
        )),
        requested_parallel_shape: ModelParallelShape::One as i32,
        placement: Some(ExactPlacement {
            site_node_id: "worker-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        }),
        desired_state: ModelDeploymentDesiredState::Present as i32,
        lifecycle_generation: 1,
        lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
        ..Default::default()
    }
}

fn model_deployment_ids(view: &NodeDesiredView) -> Vec<String> {
    view.sections
        .iter()
        .filter_map(|section| match section.payload.as_ref() {
            Some(desired_view_section::Payload::Model(model)) => Some(model),
            _ => None,
        })
        .flat_map(|model| &model.assignments)
        .filter_map(|assignment| assignment.declaration.as_ref())
        .map(|declaration| declaration.model_deployment_id.clone())
        .collect()
}

fn incarnation(character: char) -> NodeIncarnation {
    NodeIncarnation::parse(character.to_string().repeat(32)).expect("node incarnation")
}

const SITE: &str = r#"siteId: site-one
modelDeploymentIdentityAllocator:
  namespace: 0123456789abcdef
  nextSequence: '1'
nodes:
- nodeId: manager-node
  host: 127.0.0.1
  displayName: Manager node
  siteManager: {}
- nodeId: worker-one
  host: 127.0.0.2
  displayName: Worker one
- nodeId: worker-two
  host: 127.0.0.3
  displayName: Worker two
executionTargets:
- executionTargetId: target-one
  siteNodeId: worker-one
  displayName: target-one
  platform: { operatingSystem: linux, architecture: amd64 }
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
modelDeployments: []
applicationDeployments: []
"#;
