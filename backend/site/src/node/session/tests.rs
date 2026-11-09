use std::time::SystemTime;

use dotcore_domain::{
    clock::VirtualClock,
    timing::{NodeControlTiming, Timing},
};
use dotcore_proto::dotcore::{
    application::{
        private::v1::{
            ApplicationAssignment, ApplicationControllerStatusSection, ApplicationStatusFailureCode,
        },
        v1::ApplicationDeploymentDeclaration,
    },
    catalog::v1::OciPlatform,
    deployment::v1::ExactPlacement,
    model::{
        private::v1::{
            ModelBindingReadiness, ModelControllerStatusSection,
            ModelDeploymentStatus as PrivateModelDeploymentStatus, ModelLifecycleObservation,
        },
        v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
    },
    site::{
        private::v1::{
            ControllerStatusReceipt, NodeControlOpened, NodeObservationAcceptance,
            NodeViewAcknowledgementAcceptance,
        },
        v1::{
            ApplicationRuntimeBinding, ExecutionTarget, ModelRuntimeBinding,
            SimulatedRuntimeAdapter, application_runtime_binding, execution_target,
            model_runtime_binding,
        },
    },
};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::*;
use crate::{
    node::hardware_inventory::{
        HardwareCollection, HardwareInventory, HardwareInventoryEntry, HardwareInventoryPublisher,
    },
    store::StoreError,
    view::{
        desired::{NodeViewOwnerRules, node_desired_view},
        fencing::{NodeIncarnation, NodeViewProjection},
    },
};

/// Longest real wait for one node message before a test fails; behavior never waits on it.
const MESSAGE_DEADLINE: Duration = Duration::from_secs(10);

const HEARTBEAT: Duration = Duration::from_secs(3);

const MANAGER_SILENCE: Duration = Duration::from_secs(10);

/// Status check interval of tests that wake only on stream deadlines.
const STATUS_CHECK_OFF: Duration = Duration::from_secs(3_600);

#[tokio::test]
async fn every_open_resends_the_observation_then_installs_the_view_then_reports_status() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));

    let mut stream = node.next_stream().await;
    assert_eq!(stream.message().await, Message::Open(node_open()));
    stream.answer_open('a', view('a', 1, 1, Vec::new()));
    assert_eq!(stream.message().await, Message::Observation(observation(1)));
    assert_eq!(
        stream.acknowledgement().await,
        (1, 1, true),
        "the opened view installs and is acknowledged"
    );
    assert_eq!(status_sequence(&stream.message().await), 1);
    stream.close().await;

    node.observations.seed(observation(2));
    node.clock.advance_to_next_wake().await;
    let mut reopened = node.next_stream().await;
    assert_eq!(reopened.message().await, Message::Open(node_open()));
    reopened.answer_open('a', view('a', 1, 2, vec![present("model-one")]));
    assert_eq!(
        reopened.message().await,
        Message::Observation(observation(2)),
        "the reopened stream re-sends the latest observation first"
    );
    assert_eq!(reopened.acknowledgement().await, (2, 2, true));
    let status = reopened.message().await;
    assert_eq!(
        status_sequence(&status),
        2,
        "status sequences keep increasing across streams"
    );
    assert_eq!(
        status_view_revision(&status),
        2,
        "status names the new view"
    );
}

#[tokio::test]
async fn the_node_heartbeats_only_after_an_idle_interval() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;
    let quiet_since = node.clock.now();

    node.clock.advance(HEARTBEAT - Duration::from_millis(100));
    node.observations.seed(observation(2));
    assert_eq!(
        stream.message().await,
        Message::Observation(observation(2)),
        "no heartbeat precedes a message sent within the interval"
    );
    let sent_at = node.clock.now();
    assert!(sent_at < quiet_since + HEARTBEAT);

    let woke_at = node.clock.advance_to_next_wake().await;
    assert_eq!(woke_at, sent_at + HEARTBEAT);
    assert_eq!(stream.message().await, heartbeat());
    let woke_at = node.clock.advance_to_next_wake().await;
    assert_eq!(
        woke_at,
        sent_at + HEARTBEAT * 2,
        "heartbeats repeat while idle"
    );
    assert_eq!(stream.message().await, heartbeat());
}

#[tokio::test]
async fn a_dropped_stream_is_reopened_after_the_retry_pause() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let stream = node.opened_stream().await;
    let dropped_at = node.clock.now();

    stream.close().await;

    let woke_at = node.clock.advance_to_next_wake().await;
    assert_eq!(
        woke_at,
        dropped_at + Timing::production().node_control.stream_retry.initial
    );
    let mut reopened = node.next_stream().await;
    assert_eq!(reopened.message().await, Message::Open(node_open()));
}

#[tokio::test]
async fn a_manager_silent_past_the_bound_makes_the_node_reopen() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;
    let last_manager_message = node.clock.now();

    let mut heartbeats = 0;
    let ended_at = loop {
        let woke_at = node.clock.advance_to_next_wake().await;
        match stream.next().await {
            Some(message) => {
                assert_eq!(message.message, Some(heartbeat()));
                heartbeats += 1;
            }
            None => break woke_at,
        }
    };

    assert_eq!(ended_at, last_manager_message + MANAGER_SILENCE);
    assert_eq!(heartbeats, 3, "the node kept heartbeating until the bound");
    node.clock.advance_to_next_wake().await;
    let mut reopened = node.next_stream().await;
    assert_eq!(reopened.message().await, Message::Open(node_open()));
}

#[tokio::test]
async fn a_pushed_publication_installs_and_is_acknowledged_without_a_poll() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;

    stream.publish(view('a', 2, 2, vec![present("model-one")]));

    assert_eq!(stream.acknowledgement().await, (2, 2, true));
    assert_eq!(
        node.authority
            .accepted_view_reference()
            .map(|reference| reference.view_revision),
        Some(2)
    );
    node.wake_at_status_boundary().await;
    let status = stream.message().await;
    assert_eq!(status_view_revision(&status), 2);
    assert_eq!(status_deployments(&status), ["model-one"]);

    stream.publish(view('a', 2, 3, Vec::new()));
    assert_eq!(
        stream.acknowledgement().await,
        (3, 3, false),
        "a publication that does not advance the sequence is rejected"
    );
}

#[tokio::test]
async fn a_rejected_status_is_rebuilt_on_the_next_trigger() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.connect().await;
    let first = status_sequence(&stream.message().await);
    stream.reply(Reply::Rejected(NodeControlRejection {
        kind: NodeControlMessageKind::Status as i32,
        sequence: first,
        code: NodeControlRejectionCode::Conflict as i32,
    }));

    node.model.changed();

    node.wake_at_status_boundary().await;
    let rebuilt = stream.message().await;
    assert_eq!(status_sequence(&rebuilt), first + 1);
}

#[tokio::test]
async fn one_status_is_in_flight_and_triggers_during_it_send_one_more() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.connect().await;
    let first = status_sequence(&stream.message().await);

    node.model.changed();
    node.model.changed();
    node.observations.seed(observation(2));
    assert_eq!(
        stream.message().await,
        Message::Observation(observation(2)),
        "no second status is sent while the first is unanswered"
    );
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));

    node.wake_at_status_boundary().await;
    assert_eq!(status_sequence(&stream.message().await), first + 1);
}

#[tokio::test]
async fn status_triggers_within_a_second_share_one_message_at_the_next_wall_second() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;
    let opened_at = node.clock.now();

    node.clock.advance(Duration::from_millis(300));
    node.model.changed();
    node.clock.advance(Duration::from_millis(300));
    node.model.changed();
    node.observations.seed(observation(2));
    assert_eq!(
        stream.message().await,
        Message::Observation(observation(2)),
        "no status goes out before the wall-second boundary"
    );

    assert_eq!(
        node.wake_at_status_boundary().await,
        opened_at + Duration::from_secs(1),
        "the boundary is the next wall second, not a second after the first trigger"
    );
    let status = stream.message().await;
    assert_eq!(status_sequence(&status), 2);
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: 2,
        receipts: Vec::new(),
    }));

    let heartbeat_at = node.clock.now() + HEARTBEAT;
    while node.clock.advance_to_next_wake().await < heartbeat_at {}
    assert_eq!(
        stream.message().await,
        heartbeat(),
        "the two triggers produced no second status"
    );
}

#[tokio::test]
async fn under_the_test_policy_triggers_share_one_message_at_the_next_wall_aligned_window() {
    let window = Timing::test().node_control.status_coalesce_window;
    let mut node = Node::start_with_timing(vec![model_target()], {
        let mut timing = node_timing();
        timing.node_control.status_coalesce_window = window;
        timing
    });
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;
    let opened_at = node.clock.now();
    let wall = node.clock.wall_now();

    node.clock.advance(window / 4);
    node.model.changed();
    node.clock.advance(window / 4);
    node.model.changed();

    let boundary = dotcore_domain::clock::next_boundary(wall, window);
    assert_eq!(
        node.wake_at_status_boundary().await,
        opened_at + boundary.duration_since(wall).expect("a later boundary"),
        "the boundary is the next multiple of the window since the epoch"
    );
    assert_eq!(status_sequence(&stream.message().await), 2);
}

#[tokio::test]
async fn a_trigger_after_a_boundary_status_waits_for_the_following_wall_second() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;

    node.model.changed();
    let first_at = node.wake_at_status_boundary().await;
    let first = status_sequence(&stream.message().await);
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));

    node.model.changed();
    assert_eq!(
        node.wake_at_status_boundary().await,
        first_at + Duration::from_secs(1),
        "at most one status per wall second"
    );
    assert_eq!(status_sequence(&stream.message().await), first + 1);
}

#[tokio::test]
async fn a_wall_clock_stepped_back_or_forward_delays_status_by_less_than_a_second() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let mut stream = node.opened_stream().await;

    let before_back = node.clock.now();
    node.clock.step_wall_back(Duration::from_millis(2_400));
    node.model.changed();
    let back_at = node.wake_at_status_boundary().await;
    assert_eq!(
        back_at - before_back,
        Duration::from_millis(400),
        "the boundary of the stepped-back wall clock"
    );
    let first = status_sequence(&stream.message().await);
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));

    node.clock
        .step_wall_forward(Duration::from_millis(3_600_700));
    node.model.changed();
    let forward_at = node.wake_at_status_boundary().await;
    assert_eq!(
        forward_at - back_at,
        Duration::from_millis(300),
        "the boundary of the stepped-forward wall clock, with no burst or stall"
    );
    assert_eq!(status_sequence(&stream.message().await), first + 1);
}

#[tokio::test]
async fn a_status_change_no_signal_announced_is_sent_at_the_next_check() {
    let check = Duration::from_secs(1);
    let mut node = Node::start_checking_status(vec![model_target()], check);
    node.observations.seed(observation(1));
    let (mut stream, first) = node.connect_with_model().await;
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));
    let opened_at = node.clock.now();

    *node.model.message.lock().expect("status message") = "Loading weights".to_owned();
    assert_eq!(node.clock.advance_to_next_wake().await, opened_at + check);
    let changed = stream.message().await;
    assert_eq!(status_sequence(&changed), first + 1);
    assert_eq!(
        status_messages(&changed),
        ["Loading weights"],
        "the changed content is reported"
    );
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first + 1,
        receipts: Vec::new(),
    }));

    let heartbeat_at = node.clock.now() + HEARTBEAT;
    while node.clock.advance_to_next_wake().await < heartbeat_at {}
    assert_eq!(
        stream.message().await,
        heartbeat(),
        "an unchanged status is not sent again at later checks"
    );
}

#[tokio::test]
async fn a_status_an_observation_changed_follows_it_before_the_next_check() {
    let mut node = Node::start_checking_status(vec![model_target()], Duration::from_secs(1));
    node.observations.seed(observation(1));
    let (mut stream, first) = node.connect_with_model().await;
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));
    let opened_at = node.clock.now();

    // Attribution computed from the new observation changes the status.
    *node.model.message.lock().expect("status message") = "Loading weights".to_owned();
    node.observations.seed(observation(2));
    assert_eq!(stream.message().await, Message::Observation(observation(2)));
    let status = stream.message().await;
    assert_eq!(status_sequence(&status), first + 1);
    assert_eq!(status_messages(&status), ["Loading weights"]);
    assert_eq!(
        node.clock.now(),
        opened_at,
        "the status went out without waiting for the next check"
    );
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first + 1,
        receipts: Vec::new(),
    }));

    // An observation that leaves the status unchanged sends only itself, and later checks
    // find nothing new to send.
    node.observations.seed(observation(3));
    assert_eq!(stream.message().await, Message::Observation(observation(3)));
    let heartbeat_at = node.clock.now() + HEARTBEAT;
    while node.clock.advance_to_next_wake().await < heartbeat_at {}
    assert_eq!(
        stream.message().await,
        heartbeat(),
        "an unchanged status is not sent after an observation or at later checks"
    );
}

#[tokio::test]
async fn a_status_an_observation_changed_while_another_is_unanswered_follows_its_answer() {
    let mut node = Node::start(vec![model_target()]);
    node.observations.seed(observation(1));
    let (mut stream, first) = node.connect_with_model().await;
    let opened_at = node.clock.now();

    *node.model.message.lock().expect("status message") = "Loading weights".to_owned();
    node.observations.seed(observation(2));
    assert_eq!(
        stream.message().await,
        Message::Observation(observation(2)),
        "no second status is sent while the first is unanswered"
    );
    stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
        accepted_status_sequence: first,
        receipts: Vec::new(),
    }));

    let status = stream.message().await;
    assert_eq!(status_sequence(&status), first + 1);
    assert_eq!(status_messages(&status), ["Loading weights"]);
    assert_eq!(
        node.clock.now(),
        opened_at,
        "the status went out on the answer, not at the next wall second"
    );
}

#[tokio::test]
async fn the_observation_sink_waits_for_the_manager_answer_on_an_open_stream() {
    let mut node = Node::start(vec![model_target()]);
    let not_open = node
        .observations
        .report(observation(1))
        .await
        .expect_err("no stream is open yet");
    assert!(
        not_open.to_string().contains("not open"),
        "unexpected error: {not_open}"
    );

    let mut stream = node.next_stream().await;
    assert_eq!(stream.message().await, Message::Open(node_open()));
    stream.answer_open('a', view('a', 1, 1, Vec::new()));
    assert_eq!(stream.message().await, Message::Observation(observation(1)));
    stream.acknowledgement().await;
    stream.message().await;

    let observations = node.observations.clone();
    let accepted = tokio::spawn(async move { observations.report(observation(2)).await });
    assert_eq!(stream.message().await, Message::Observation(observation(2)));
    stream.reply(Reply::ObservationAccepted(NodeObservationAcceptance {
        accepted_revision: 2,
        received_at: None,
    }));
    accepted
        .await
        .expect("report task")
        .expect("accepted observation");

    let observations = node.observations.clone();
    let rejected = tokio::spawn(async move { observations.report(observation(3)).await });
    assert_eq!(stream.message().await, Message::Observation(observation(3)));
    stream.reply(Reply::Rejected(NodeControlRejection {
        kind: NodeControlMessageKind::Observation as i32,
        sequence: 3,
        code: NodeControlRejectionCode::Invalid as i32,
    }));
    let error = rejected
        .await
        .expect("report task")
        .expect_err("rejected observation");
    assert!(error.to_string().contains("revision 3"), "{error}");
}

#[tokio::test]
async fn an_invalid_application_assignment_installs_and_keeps_model_status() {
    let mut node = Node::start(vec![model_target(), application_target()]);
    node.observations.seed(observation(1));
    let mut invalid = application_assignment("application-one");
    invalid
        .declaration
        .as_mut()
        .and_then(|declaration| declaration.placement.as_mut())
        .expect("placement")
        .execution_target_id = "target-one".to_owned();

    let mut stream = node.next_stream().await;
    stream.message().await;
    stream.answer_open(
        'a',
        mixed_view(vec![present("observed")], vec![invalid.clone()]),
    );
    stream.message().await;

    assert_eq!(
        stream.acknowledgement().await,
        (1, 1, true),
        "the invalid assignment fails only its own deployment"
    );
    assert_eq!(status_deployments(&stream.message().await), ["observed"]);
}

#[tokio::test]
async fn node_status_carries_the_application_section_and_routes_its_receipts() {
    let mut node = Node::prepare(vec![model_target(), application_target()]);
    let evidence =
        crate::view::contract::application_cleanup_evidence("application-gone", 2, Some(7));
    let owner = RecordingApplicationStatus::new(ApplicationControllerStatusSection {
        deployments: Vec::new(),
        cleanup_evidence: vec![evidence.clone()],
    });
    let session = node.session(Some(
        Arc::clone(&owner) as Arc<dyn NodeApplicationStatusOwner>
    ));
    let stream = node
        .authority
        .open_stream(manager_session('a'))
        .expect("view stream");
    session
        .installer
        .install_publication(
            &session.open,
            &stream,
            &mixed_view(
                vec![present("observed")],
                vec![application_assignment("application-one")],
            ),
        )
        .expect("installed view");

    let request = session.status_request(1).expect("status");
    assert_eq!(
        crate::view::contract::validate_application_status(&request)
            .expect("valid application section"),
        Some(&owner.section)
    );
    let view = request.view.as_ref().expect("view");
    let receipt = |disposition| ControllerStatusReceipt {
        payload: Some(controller_status_receipt::Payload::Application(
            crate::view::contract::application_cleanup_receipt(
                &ManagerSession::parse(view.manager_session_id.clone()).unwrap(),
                &NodeIncarnation::parse(view.node_incarnation_id.clone()).unwrap(),
                1,
                &evidence,
                disposition,
            ),
        )),
    };
    for disposition in [
        ApplicationCleanupDisposition::Rejected,
        ApplicationCleanupDisposition::Acknowledged,
    ] {
        session
            .accept_status_receipts(
                &request,
                &NodeControllerStatusAcceptance {
                    accepted_status_sequence: 1,
                    receipts: vec![receipt(disposition)],
                },
            )
            .expect("receipts recorded");
    }
    assert_eq!(
        *owner.acknowledged.lock().unwrap(),
        [BTreeSet::from([("application-gone".to_owned(), 2)])],
        "only an acknowledged disposition reaches the application owner"
    );
}

#[tokio::test]
async fn every_section_of_one_status_attributes_from_the_collection_pinned_for_it() {
    let mut node = Node::prepare(vec![model_target(), application_target()]);
    node.hardware
        .publish(&worker_one(), HardwareInventoryEntry::pending());
    *node.model.publish_during_build.lock().expect("publisher") = Some(node.hardware.clone());
    let application =
        RecordingApplicationStatus::new(ApplicationControllerStatusSection::default());
    let session = node.session(Some(
        Arc::clone(&application) as Arc<dyn NodeApplicationStatusOwner>
    ));
    let stream = node
        .authority
        .open_stream(manager_session('a'))
        .expect("view stream");
    session
        .installer
        .install_publication(
            &session.open,
            &stream,
            &mixed_view(
                vec![present("model-one")],
                vec![application_assignment("application-one")],
            ),
        )
        .expect("installed view");

    session.status_request(1).expect("first status");
    session.status_request(2).expect("second status");

    // The model section publishes a newer collection while each status is being built.
    assert_eq!(*node.model.collections.lock().expect("collections"), [1, 2]);
    assert_eq!(
        *application.collections.lock().expect("collections"),
        [1, 2],
        "the application section reads the collection the model section read"
    );
    assert_eq!(node.inventory.collection().revision(), 3);
}

type Message = stream_node_control_request::Message;
type Reply = stream_node_control_response::Message;

/// One node exchange over a fake transport, a virtual clock and real retained node state.
struct Node {
    _directory: tempfile::TempDir,
    clock: VirtualClock,
    authority: AcceptedNodeViewAuthority,
    installer: Option<NodeViewInstaller>,
    state: FileNodeState,
    model: Arc<TestModelStatus>,
    observations: NodeControlObservations,
    hardware: HardwareInventoryPublisher,
    inventory: HardwareInventory,
    transport: Arc<FakeTransport>,
    streams: mpsc::UnboundedReceiver<FakeStream>,
    status_check_interval: Duration,
    timing: Timing,
    _task: Option<NodeControlSessionTask>,
}

impl Node {
    /// Prepares a node with `installed` targets and starts its exchange.
    fn start(installed: Vec<ExecutionTarget>) -> Self {
        Self::start_checking_status(installed, STATUS_CHECK_OFF)
    }

    /// Starts a node that looks for an unannounced status change every `interval`.
    fn start_checking_status(installed: Vec<ExecutionTarget>, interval: Duration) -> Self {
        let mut node = Self::prepare(installed);
        node.status_check_interval = interval;
        let session = node.session(None);
        node._task = Some(session.start());
        node
    }

    /// Starts a node whose stream runs on `timing`, with status checks off.
    fn start_with_timing(installed: Vec<ExecutionTarget>, timing: Timing) -> Self {
        let mut node = Self::prepare(installed);
        node.timing = timing;
        let session = node.session(None);
        node._task = Some(session.start());
        node
    }

    /// Prepares a node with `installed` targets without starting any exchange.
    fn prepare(installed: Vec<ExecutionTarget>) -> Self {
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            node_incarnation(),
            ManagerSession::parse("0".repeat(32)).expect("discovery session"),
            Duration::from_secs(90),
        )
        .expect("view authority");
        let installer = NodeViewInstaller::new(authority.clone(), &installed, owner_rules());
        let directory =
            dotcore_domain::test_support::canonical_tempdir().expect("node state directory");
        let state = FileNodeState::open(directory.path().join("node"), "site-one", "worker-one")
            .expect("node state");
        let model = TestModelStatus::new(&installer);
        let (opens, streams) = mpsc::unbounded_channel();
        let (hardware, inventory) = HardwareInventoryPublisher::new();
        Self {
            _directory: directory,
            clock: VirtualClock::starting_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)),
            authority,
            installer: Some(installer),
            state,
            model,
            observations: NodeControlObservations::new(),
            hardware,
            inventory,
            transport: Arc::new(FakeTransport { opens }),
            streams,
            status_check_interval: STATUS_CHECK_OFF,
            timing: node_timing(),
            _task: None,
        }
    }

    fn session(
        &mut self,
        application: Option<Arc<dyn NodeApplicationStatusOwner>>,
    ) -> NodeControlSession {
        NodeControlSession::new(
            "site-one",
            "worker-one",
            Arc::clone(&self.transport) as Arc<dyn NodeControlTransport>,
            self.authority.clone(),
            self.installer.take().expect("one session per node"),
            self.observations.clone(),
            Arc::new(self.inventory.clone()),
            self.state.clone(),
            Arc::clone(&self.model) as Arc<dyn NodeModelStatusOwner>,
            application,
            &NodeControlTiming {
                status_check_interval: self.status_check_interval,
                ..self.timing.node_control
            },
            Arc::new(self.clock.clone()),
        )
    }

    /// Waits until the session has scheduled a status, then wakes it at that boundary; the
    /// heartbeat and silence deadlines are further off than a second.
    async fn wake_at_status_boundary(&self) -> tokio::time::Instant {
        self.clock
            .advance_to_next_wake_within(Duration::from_secs(1))
            .await
    }

    async fn next_stream(&mut self) -> FakeStream {
        tokio::time::timeout(MESSAGE_DEADLINE, self.streams.recv())
            .await
            .expect("a stream opens within the deadline")
            .expect("the transport stays alive")
    }

    /// Opens a stream and drains its open, observation, acknowledgement and status, answering
    /// the status, so the stream is idle when this returns.
    async fn opened_stream(&mut self) -> FakeStream {
        let mut stream = self.connect().await;
        let sequence = status_sequence(&stream.message().await);
        stream.reply(Reply::StatusAccepted(NodeControllerStatusAcceptance {
            accepted_status_sequence: sequence,
            receipts: Vec::new(),
        }));
        stream.reply(Reply::AcknowledgementAccepted(
            NodeViewAcknowledgementAcceptance {
                accepted_acknowledgement_sequence: 1,
            },
        ));
        // The manager's replies are processed before the next node message is answered.
        self.observations.seed(observation(1));
        assert_eq!(stream.message().await, Message::Observation(observation(1)));
        stream
    }

    /// Opens a stream on a view with one model deployment and drains it up to its first status,
    /// which stays unanswered, returning that status's sequence.
    async fn connect_with_model(&mut self) -> (FakeStream, u64) {
        let mut stream = self.next_stream().await;
        assert_eq!(stream.message().await, Message::Open(node_open()));
        stream.answer_open('a', view('a', 1, 1, vec![present("model-one")]));
        assert!(matches!(stream.message().await, Message::Observation(_)));
        stream.acknowledgement().await;
        let first = status_sequence(&stream.message().await);
        (stream, first)
    }

    /// Opens a stream and drains it up to its first status, which stays unanswered.
    async fn connect(&mut self) -> FakeStream {
        let mut stream = self.next_stream().await;
        assert_eq!(stream.message().await, Message::Open(node_open()));
        stream.answer_open('a', view('a', 1, 1, Vec::new()));
        assert!(matches!(stream.message().await, Message::Observation(_)));
        stream.acknowledgement().await;
        stream
    }
}

/// Transport handing each opened stream to the test.
struct FakeTransport {
    opens: mpsc::UnboundedSender<FakeStream>,
}

impl NodeControlTransport for FakeTransport {
    fn open(
        &self,
        outbound: mpsc::Receiver<StreamNodeControlRequest>,
    ) -> BoxFuture<'_, Result<ManagerMessages, Status>> {
        let (inbound, messages) = mpsc::unbounded_channel();
        let _ = self.opens.send(FakeStream { outbound, inbound });
        Box::pin(std::future::ready(Ok(UnboundedReceiverStream::new(
            messages,
        )
        .boxed())))
    }
}

/// The manager's end of one stream.
struct FakeStream {
    outbound: mpsc::Receiver<StreamNodeControlRequest>,
    inbound: mpsc::UnboundedSender<Result<StreamNodeControlResponse, Status>>,
}

impl FakeStream {
    /// The next node message, or `None` once the node dropped the stream.
    async fn next(&mut self) -> Option<StreamNodeControlRequest> {
        tokio::time::timeout(MESSAGE_DEADLINE, self.outbound.recv())
            .await
            .expect("a node message within the deadline")
    }

    async fn message(&mut self) -> Message {
        self.next()
            .await
            .and_then(|request| request.message)
            .expect("node message")
    }

    /// The next message as an acknowledgement's sequence, view revision and acceptance.
    async fn acknowledgement(&mut self) -> (u64, u64, bool) {
        match self.message().await {
            Message::AcknowledgeView(acknowledgement) => (
                acknowledgement.acknowledgement_sequence,
                acknowledgement
                    .view
                    .map_or(0, |reference| reference.view_revision),
                matches!(
                    acknowledgement.outcome,
                    Some(node_view_acknowledgement::Outcome::Accepted(_))
                ),
            ),
            other => panic!("expected an acknowledgement, got {other:?}"),
        }
    }

    fn reply(&self, reply: Reply) {
        self.inbound
            .send(Ok(StreamNodeControlResponse {
                message: Some(reply),
            }))
            .expect("the node reads the stream");
    }

    fn answer_open(&self, session: char, view: NodeDesiredView) {
        self.reply(Reply::Opened(NodeControlOpened {
            manager_session_id: manager_session(session).as_str().to_owned(),
            view: Some(view),
        }));
    }

    fn publish(&self, view: NodeDesiredView) {
        self.reply(Reply::ViewPublished(view));
    }

    /// Closes the manager's side and waits until the node let go of the stream.
    async fn close(mut self) {
        let (closed, _) = mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.inbound, closed));
        while self.next().await.is_some() {}
    }
}

/// Node model status owner reporting every installed assignment as reconciling, whose serving
/// changes the test raises. It records the revision of each collection it builds from, and can
/// publish a newer collection while it builds.
struct TestModelStatus {
    input: crate::view::fencing::AcceptedModelOwnerInput,
    observers: std::sync::Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    message: std::sync::Mutex<String>,
    collections: std::sync::Mutex<Vec<u64>>,
    publish_during_build: std::sync::Mutex<Option<HardwareInventoryPublisher>>,
}

impl TestModelStatus {
    fn new(installer: &NodeViewInstaller) -> Arc<Self> {
        Arc::new(Self {
            input: installer.projected_input(),
            observers: std::sync::Mutex::new(Vec::new()),
            message: std::sync::Mutex::new(String::new()),
            collections: std::sync::Mutex::new(Vec::new()),
            publish_during_build: std::sync::Mutex::new(None),
        })
    }

    /// Calls every registered observer, as a change of what the node serves does.
    fn changed(&self) {
        let observers = self.observers.lock().expect("observers").clone();
        for observer in observers {
            observer();
        }
    }
}

impl NodeModelStatusOwner for TestModelStatus {
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ModelControllerStatusSection> {
        self.collections
            .lock()
            .expect("collections")
            .push(collection.revision());
        if let Some(hardware) = self
            .publish_during_build
            .lock()
            .expect("publisher")
            .as_ref()
        {
            hardware.publish(&worker_one(), HardwareInventoryEntry::pending());
        }
        let snapshot = self.input.declaration_snapshot().ok()?;
        Some(ModelControllerStatusSection {
            deployments: snapshot
                .deployments
                .iter()
                .map(|declaration| PrivateModelDeploymentStatus {
                    model_deployment_id: declaration.model_deployment_id.clone(),
                    lifecycle_generation: declaration.lifecycle_generation,
                    placement: declaration.placement.clone(),
                    lifecycle: ModelLifecycleObservation::Reconciling as i32,
                    binding_readiness: ModelBindingReadiness::Absent as i32,
                    accelerator_memory: Some(not_applicable_memory()),
                    status_message: self.message.lock().expect("status message").clone(),
                    ..Default::default()
                })
                .collect(),
            cleanup_evidence: Vec::new(),
        })
    }

    fn accept_receipts(&self, _: &BTreeSet<(String, u64)>) -> Result<(), StoreError> {
        Ok(())
    }

    fn retire_withdrawn_cleanup_evidence(&self) {}

    fn register_change_observer(&self, observer: Arc<dyn Fn() + Send + Sync>) {
        self.observers.lock().expect("observers").push(observer);
    }
}

/// Application status owner returning one fixed section and recording acknowledgements and the
/// revision of each collection it builds from.
struct RecordingApplicationStatus {
    section: ApplicationControllerStatusSection,
    acknowledged: std::sync::Mutex<Vec<BTreeSet<(String, u64)>>>,
    collections: std::sync::Mutex<Vec<u64>>,
}

impl RecordingApplicationStatus {
    fn new(section: ApplicationControllerStatusSection) -> Arc<Self> {
        Arc::new(Self {
            section,
            acknowledged: std::sync::Mutex::new(Vec::new()),
            collections: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl NodeApplicationStatusOwner for RecordingApplicationStatus {
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ApplicationControllerStatusSection> {
        self.collections
            .lock()
            .expect("collections")
            .push(collection.revision());
        Some(self.section.clone())
    }

    fn accept_receipts(&self, acknowledged: &BTreeSet<(String, u64)>) -> Result<(), StoreError> {
        self.acknowledged.lock().unwrap().push(acknowledged.clone());
        Ok(())
    }

    fn retire_withdrawn_cleanup_evidence(&self) {}
}

/// Owner rules accepting every model assignment and classifying an application assignment as a
/// target mismatch unless it is placed on one of the node's application targets.
fn owner_rules() -> NodeViewOwnerRules {
    NodeViewOwnerRules {
        valid_model_assignment: |_| true,
        classify_application_assignment: |_, targets, assignment| {
            let placed = assignment
                .declaration
                .as_ref()
                .and_then(|declaration| declaration.placement.as_ref());
            targets
                .iter()
                .find(|target| {
                    placed.is_some_and(|placement| {
                        placement.execution_target_id == target.execution_target_id
                    })
                })
                .filter(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                    )
                })
                .map(|_| ())
                .ok_or(ApplicationStatusFailureCode::TargetMismatch)
        },
    }
}

fn node_open() -> NodeControlOpen {
    NodeControlOpen {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        node_incarnation_id: node_incarnation().as_str().to_owned(),
        supported_sections: supported_sections(),
    }
}

fn heartbeat() -> Message {
    Message::Heartbeat(dotcore_proto::google::protobuf::Empty {})
}

fn observation(revision: u64) -> NodeObservation {
    NodeObservation {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        incarnation_id: node_incarnation().as_str().to_owned(),
        revision,
        ..Default::default()
    }
}

/// A view of worker-one's model target published on the stream of manager session `session`.
fn view(
    session: char,
    sequence: u64,
    revision: u64,
    deployments: Vec<ModelDeploymentDeclaration>,
) -> NodeDesiredView {
    let projection =
        NodeViewProjection::new(vec![model_target()], deployments).expect("node projection");
    node_desired_view(
        &manager_session(session),
        &node_incarnation(),
        sequence,
        revision,
        &projection,
        "site-one",
        "worker-one",
    )
}

/// The first view of a node with a model and an application target.
fn mixed_view(
    deployments: Vec<ModelDeploymentDeclaration>,
    applications: Vec<ApplicationAssignment>,
) -> NodeDesiredView {
    let projection =
        NodeViewProjection::new(vec![application_target(), model_target()], deployments)
            .and_then(|projection| projection.with_application_assignments(applications))
            .expect("mixed node projection");
    node_desired_view(
        &manager_session('a'),
        &node_incarnation(),
        1,
        1,
        &projection,
        "site-one",
        "worker-one",
    )
}

fn status_request(message: &Message) -> &NodeControllerStatus {
    match message {
        Message::Status(request) => request,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn status_sequence(message: &Message) -> u64 {
    status_request(message).status_sequence
}

fn status_view_revision(message: &Message) -> u64 {
    status_request(message)
        .view
        .as_ref()
        .map_or(0, |view| view.view_revision)
}

fn status_deployments(message: &Message) -> Vec<String> {
    model_deployment_statuses(message)
        .map(|deployment| deployment.model_deployment_id.clone())
        .collect()
}

/// The status message of each model deployment a status reports.
fn status_messages(message: &Message) -> Vec<String> {
    model_deployment_statuses(message)
        .map(|deployment| deployment.status_message.clone())
        .collect()
}

fn model_deployment_statuses(
    message: &Message,
) -> impl Iterator<Item = &PrivateModelDeploymentStatus> {
    status_request(message)
        .sections
        .iter()
        .filter_map(|section| match section.payload.as_ref() {
            Some(controller_status_section::Payload::Model(model)) => Some(model),
            _ => None,
        })
        .flat_map(|model| &model.deployments)
}

fn model_target() -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: "target-one".to_owned(),
        display_name: "target-one".to_owned(),
        site_node_id: "worker-one".to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                    SimulatedRuntimeAdapter::default(),
                )),
            },
        )),
    }
}

fn application_target() -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: "application-target".to_owned(),
        display_name: "application-target".to_owned(),
        runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
            ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                    Default::default(),
                )),
            },
        )),
        ..model_target()
    }
}

fn present(id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: id.to_owned(),
        snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
            id,
        )),
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

fn application_assignment(id: &str) -> ApplicationAssignment {
    ApplicationAssignment {
        declaration: Some(ApplicationDeploymentDeclaration {
            application_deployment_id: id.to_owned(),
            application_deployment_name: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: "application-target".to_owned(),
            }),
            lifecycle_generation: 1,
            ..Default::default()
        }),
        linked_route: None,
    }
}

fn not_applicable_memory() -> dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryObservation {
    dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryObservation {
        result: Some(
            dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::NotApplicable(
                dotcore_proto::google::protobuf::Empty {},
            ),
        ),
        ..Default::default()
    }
}

fn worker_one() -> dotcore_domain::SiteNodeId {
    dotcore_domain::SiteNodeId::for_test("worker-one")
}

fn node_incarnation() -> NodeIncarnation {
    NodeIncarnation::parse("1".repeat(32)).expect("node incarnation")
}

fn manager_session(character: char) -> ManagerSession {
    ManagerSession::parse(character.to_string().repeat(32)).expect("manager session")
}

/// Production stream timing with this module's heartbeat and manager silence.
fn node_timing() -> Timing {
    {
        let mut timing = Timing::production();
        timing.node_control.heartbeat_interval = HEARTBEAT;
        timing.node_control.manager_silence = MANAGER_SILENCE;
        timing
    }
}
