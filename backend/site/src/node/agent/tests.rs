use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_domain::hardware_observation::{HardwareSample, HardwareSnapshotSource};
use dotcore_domain::test_support::tracing::{Record, capture_records_async};
use dotcore_domain::{
    ExecutionPlatform,
    clock::{Clock as _, VirtualClock},
};
use dotcore_node_hardware::LocalNodeCapability;
use dotcore_proto::{
    dotcore::{
        catalog::v1::OciPlatform,
        node::v1::{
            DockerRuntimeObservationConfiguration, NativeRuntimeObservationConfiguration,
            NodeExecutionTarget, NodeObservationConfiguration, node_execution_target,
        },
        observation::v1::{
            HardwareSnapshot, HostFacts, HostObservation, RuntimeKind, host_observation,
            runtime_observation,
        },
        site::{
            private::v1::{
                NodeObservation, StreamNodeControlRequest,
                node_control_service_server::{NodeControlService, NodeControlServiceServer},
                node_hardware_observation,
            },
            v1::DockerAcceleratorTransport,
        },
    },
    google::protobuf::Empty,
};
use dotcore_runtime::{
    DockerCapabilityProbe, DockerCapabilityReport, NativeCapabilityProbe, NativeCapabilityReport,
    RuntimeCapabilityProbes,
};
use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use super::{
    CollectionReading, InProcessObservationSink, ManagerTransportBounds, NodeObservationCollector,
    NodeObservationFeed, NodeReportIdentity, NodeReporter, NodeReporterTask, ObservationReportSink,
    WorkerPrivateClientConfiguration,
};
use crate::{
    node::session::{ManagerNodeControlClient, NodeControlObservations, NodeControlTransport},
    node::{
        folder::CertificateFingerprint,
        observation_configuration::NodeObservationConfigurationDigest,
    },
    private_transport::{PrivateNodeRole, PrivatePeerPolicy, PrivateServerConfiguration},
    test_support::private_tls::{MutualTlsListener, TestAuthority},
};

#[tokio::test]
async fn outbound_tls_requires_site_chain_host_exact_manager_identity_and_pin_before_dispatch() {
    let fixture = TransportFixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let routes =
        tonic::service::Routes::new(NodeControlServiceServer::new(CountingNodeControlService {
            calls: Arc::clone(&calls),
        }));
    let server = MutualTlsListener::start(&fixture.manager_server_configuration(), routes).await;
    let address = server.address;

    open_stream(&fixture.private_configuration(
        address,
        "manager-one",
        fixture.manager_certificate.clone(),
    ))
    .await
    .expect("authenticated stream");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (alternate_manager, _) = fixture.authority.issue("site-one", "manager-one", true);
    assert!(
        open_stream(&fixture.private_configuration(address, "manager-one", alternate_manager))
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert!(
        open_stream(&fixture.private_configuration(
            address,
            "another-manager",
            fixture.manager_certificate.clone(),
        ))
        .await
        .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_manager_that_accepts_tcp_but_never_answers_tls_fails_the_stream_open() {
    let fixture = TransportFixture::new();
    let silent = SilentListener::start().await;
    let mut configuration = fixture.private_configuration(
        silent.address,
        "manager-one",
        fixture.manager_certificate.clone(),
    );
    configuration.bounds.tls_handshake = Duration::from_millis(200);

    let outcome = tokio::time::timeout(Duration::from_secs(10), open_stream(&configuration)).await;

    let error = outcome
        .expect("the open ends instead of waiting on the silent handshake")
        .expect_err("a silent manager fails the open");
    assert_eq!(silent.accepted.load(Ordering::SeqCst), 1);
    assert!(
        format!("{error:#}").contains("TLS handshake timeout"),
        "the handshake bound ends the open: {error:#}"
    );
}

#[tokio::test]
async fn a_delivery_that_never_completes_does_not_stop_later_reports() {
    let sink = StallFirstSink::default();
    let requests = Arc::clone(&sink.requests);
    let reported = Arc::clone(&sink.reported);
    let collector = NodeObservationCollector::with_timeout(
        simulated_observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
        Duration::from_nanos(1),
        system_clock(),
    )
    .expect("collector configuration");
    let reporter = NodeReporter::new(
        collector,
        sink,
        report_identity("0".repeat(64)),
        Duration::from_millis(50),
        system_clock(),
    )
    .with_delivery_timeout(Duration::from_millis(100));

    let _task = reporter.start();
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().expect("delivered reports").is_empty() {
            reported.notified().await;
        }
    })
    .await
    .expect("a later report is delivered after the stalled one");

    assert_eq!(
        requests
            .lock()
            .expect("delivered reports")
            .first()
            .map(|request| request.revision),
        Some(2)
    );
}

#[tokio::test]
async fn a_panicking_report_loop_is_logged_instead_of_ending_silently() {
    let ((), records, _) = capture_records_async(|| async {
        let collector = NodeObservationCollector::with_timeout(
            simulated_observation_configuration(),
            Arc::new(LocalNodeCapability::new(FixedHardware::available())),
            probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
            Duration::from_nanos(1),
            system_clock(),
        )
        .expect("collector configuration");
        let reporter = NodeReporter::new(
            collector,
            PanickingSink,
            report_identity("0".repeat(64)),
            Duration::from_secs(5),
            system_clock(),
        );
        let mut task = reporter.start();
        (&mut task.report)
            .await
            .expect("the loop's panic is logged, not propagated");
    })
    .await;

    let ended = records
        .iter()
        .filter_map(|record| match record {
            Record::Event { level, fields }
                if fields.get("event_kind") == Some(&"\"background_loop_ended\"".to_owned()) =>
            {
                Some((level.as_str(), fields))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 1, "one loop-end event: {records:?}");
    let (level, fields) = ended[0];
    assert_eq!(level, "ERROR");
    assert_eq!(
        fields.get("loop_name").map(String::as_str),
        Some("\"node_reporter\"")
    );
    assert_eq!(
        fields.get("outcome").map(String::as_str),
        Some("\"panicked\"")
    );
    assert_eq!(
        fields.get("panic").map(String::as_str),
        Some("\"injected report panic\"")
    );
}

#[tokio::test]
async fn the_manager_node_registers_in_process_before_its_report_loop_starts() {
    let site = manager_site();
    let registry = crate::node::registry::NodeRegistry::new(Arc::new(site.clone()));
    let configuration = crate::node::observation_configuration::node_observation_configuration(
        &site,
        "manager-node",
    )
    .expect("manager node observation configuration");
    let digest = NodeObservationConfigurationDigest::for_configuration(&configuration)
        .expect("manager node observation digest");
    let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_000));
    let stream = NodeControlObservations::new();
    let mut reporter = NodeReporter::new(
        NodeObservationCollector::with_timeout(
            configuration,
            Arc::new(LocalNodeCapability::new(FixedHardware::observed())),
            probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
            FAR_BOUND,
            Arc::new(clock.clone()),
        )
        .expect("collector configuration"),
        InProcessObservationSink::new(
            site.site_id.clone(),
            dotcore_domain::SiteNodeId::for_test("manager-node"),
            registry.clone(),
            stream.clone(),
        ),
        NodeReportIdentity {
            site_id: site.site_id.clone(),
            node_id: "manager-node".to_owned(),
            ..report_identity(digest.as_str().to_owned())
        },
        Duration::from_secs(5),
        Arc::new(clock.clone()),
    );
    let revision = || {
        registry
            .observation(&dotcore_domain::SiteNodeId::for_test("manager-node"))
            .map(|observation| observation.revision)
    };

    reporter
        .report_initial()
        .await
        .expect("initial in-process registration");
    assert_eq!(revision(), Some(1));
    let streamed = || stream.latest_request().map(|request| request.revision);
    assert_eq!(
        streamed(),
        Some(1),
        "the stream re-sends the registered one"
    );

    let _task = reporter.start_after_initial();
    clock.advance_to_next_wake_within(NEAR_DEADLINES).await;
    tokio::time::timeout(TEST_DEADLINE, async {
        while streamed() == Some(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the next observation reaches the stream");
    assert_eq!(
        streamed(),
        Some(2),
        "later observations travel on the stream"
    );
    assert_eq!(revision(), Some(1), "only the first registers directly");
}

#[tokio::test]
async fn the_in_process_sink_refuses_a_report_for_another_node_before_the_registry() {
    let site = manager_site();
    let registry = crate::node::registry::NodeRegistry::new(Arc::new(site.clone()));
    let sink = InProcessObservationSink::new(
        site.site_id.clone(),
        dotcore_domain::SiteNodeId::for_test("manager-node"),
        registry.clone(),
        NodeControlObservations::new(),
    );

    let error = sink
        .report(NodeObservation {
            site_id: site.site_id.clone(),
            node_id: "worker-node".to_owned(),
            incarnation_id: "1".repeat(32),
            revision: 1,
            ..Default::default()
        })
        .await
        .expect_err("report for another node");

    assert_eq!(
        error.to_string(),
        "in-process observation report names node \"worker-node\" instead of \"manager-node\""
    );
    assert!(
        registry
            .observation(&dotcore_domain::SiteNodeId::for_test("worker-node"))
            .is_none()
    );
}

const INTERVAL: Duration = Duration::from_secs(5);

/// Longest real wait before a test fails; behavior never waits on it.
const TEST_DEADLINE: Duration = Duration::from_secs(10);

/// Window holding the report period and delivery waits, but not the far-off safety bounds.
const NEAR_DEADLINES: Duration = Duration::from_secs(30);

/// Safety bound of probes and deliveries, far beyond any virtual time a test advances.
const FAR_BOUND: Duration = Duration::from_secs(3_600);

/// A reporter on a virtual clock whose wall time is `into_slot` past an interval boundary, with
/// each delivery taking `delivery` and recording the boundary it was stamped with.
struct AlignedReporter {
    clock: VirtualClock,
    observations: NodeObservationFeed,
    stamps: Arc<Mutex<Vec<SystemTime>>>,
    reported: Arc<Notify>,
    reporter: Option<NodeReporter<FixedHardware, StampSink>>,
}

impl AlignedReporter {
    fn new(into_slot: Duration, delivery: Duration) -> Self {
        let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_000) + into_slot);
        let collector = NodeObservationCollector::with_timeout(
            simulated_observation_configuration(),
            Arc::new(LocalNodeCapability::new(FixedHardware::available())),
            probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
            FAR_BOUND,
            Arc::new(clock.clone()),
        )
        .expect("collector configuration");
        let stamps = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::new(Notify::new());
        let mut reporter = NodeReporter::new(
            collector,
            StampSink {
                clock: clock.clone(),
                observations: NodeObservationFeed::default(),
                delivery,
                stamps: Arc::clone(&stamps),
                reported: Arc::clone(&reported),
            },
            report_identity("0".repeat(64)),
            INTERVAL,
            Arc::new(clock.clone()),
        )
        .with_delivery_timeout(FAR_BOUND);
        let observations = reporter.observations();
        reporter.sink.observations = observations.clone();
        Self {
            clock,
            observations,
            stamps,
            reported,
            reporter: Some(reporter),
        }
    }

    fn start(&mut self) -> NodeReporterTask {
        self.reporter.take().expect("started once").start()
    }

    /// Wall time since the Unix epoch, in whole seconds, of every stamp so far.
    fn stamped_seconds(&self) -> Vec<u64> {
        self.stamps
            .lock()
            .expect("stamps")
            .iter()
            .map(|stamp| {
                stamp
                    .duration_since(UNIX_EPOCH)
                    .expect("after epoch")
                    .as_secs()
            })
            .collect()
    }

    /// Wakes the reporter's waits due within the report period, one after another, until it
    /// makes its next report. Its safety bounds are far off, so only the boundary and delivery
    /// waits are ever advanced to.
    async fn next_report(&self) {
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                tokio::select! {
                    biased;
                    () = self.reported.notified() => return,
                    _ = self.clock.advance_to_next_wake_within(NEAR_DEADLINES) => {}
                }
            }
        })
        .await
        .expect("the reporter reports within the deadline");
    }
}

#[tokio::test]
async fn readings_are_stamped_on_interval_multiples_whatever_the_start_time() {
    for into_slot in [0, 1_300, 3_900, 4_999] {
        let mut fixture = AlignedReporter::new(Duration::from_millis(into_slot), Duration::ZERO);
        let _task = fixture.start();
        fixture.reported.notified().await;
        for _ in 0..3 {
            fixture.next_report().await;
        }

        assert_eq!(
            fixture.stamped_seconds(),
            [1_000, 1_005, 1_010, 1_015],
            "start {into_slot}ms into a slot: the first report takes its slot's boundary and the rest follow on the grid"
        );
        assert_eq!(
            fixture.observations.latest().expect("latest").collected_at,
            UNIX_EPOCH + Duration::from_secs(1_015)
        );
    }
}

#[tokio::test]
async fn an_overrunning_report_skips_to_the_next_boundary_without_a_burst() {
    let mut fixture = AlignedReporter::new(Duration::from_secs(1), Duration::from_secs(7));
    let _task = fixture.start();
    fixture.reported.notified().await;
    fixture.next_report().await;
    fixture.next_report().await;

    // Each delivery takes 7s, so reports at 1_005 and 1_015 are skipped.
    assert_eq!(fixture.stamped_seconds(), [1_000, 1_010, 1_020]);
}

#[tokio::test]
async fn a_wall_clock_stepped_back_waits_for_a_boundary_without_a_burst_or_stall() {
    let mut fixture = AlignedReporter::new(Duration::from_secs(1), Duration::ZERO);
    let _task = fixture.start();
    fixture.reported.notified().await;

    fixture.clock.step_wall_back(Duration::from_secs(32));
    fixture.clock.advance_to_next_wake().await;
    // Woken before its boundary by the stepped wall clock: it waits for the next one instead.
    fixture.next_report().await;
    fixture.next_report().await;

    assert_eq!(
        fixture.stamped_seconds(),
        [1_000, 975, 980],
        "one report per boundary of the corrected clock"
    );
}

#[tokio::test]
async fn a_wall_clock_stepped_forward_reports_once_at_the_next_boundary() {
    let mut fixture = AlignedReporter::new(Duration::from_secs(1), Duration::ZERO);
    let _task = fixture.start();
    fixture.reported.notified().await;

    fixture.clock.step_wall_forward(Duration::from_secs(102));
    fixture.clock.advance_to_next_wake().await;
    assert_eq!(
        fixture.stamped_seconds(),
        [1_000],
        "no catch-up report for the skipped boundaries"
    );
    fixture.next_report().await;
    fixture.next_report().await;

    assert_eq!(fixture.stamped_seconds(), [1_000, 1_110, 1_115]);
}

#[tokio::test]
async fn reporter_reports_its_bound_private_port_only_when_configured() {
    let sink = RecordingSink::default();
    let requests = Arc::clone(&sink.requests);
    let collector = NodeObservationCollector::with_timeout(
        simulated_observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");
    let mut reporter = NodeReporter::new(
        collector,
        sink,
        report_identity("0".repeat(64)),
        Duration::from_secs(5),
        system_clock(),
    );

    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("report without listener");
    let mut reporter = reporter.with_private_port(Some(47_123));
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("report with listener");

    assert_eq!(
        requests
            .lock()
            .expect("recorded requests")
            .iter()
            .map(|request| (request.revision, request.private_port))
            .collect::<Vec<_>>(),
        [(1, None), (2, Some(47_123))]
    );
}

#[tokio::test]
async fn reporter_binds_identity_digest_incarnation_and_monotonic_revisions() {
    let sink = RecordingSink::default();
    let requests = Arc::clone(&sink.requests);
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(Arc::new(FakeDocker::available(
            DockerCapabilityReport::new(ExecutionPlatform::linux_amd64(), ["/etc/cdi"]),
            ["nvidia.com/gpu=GPU-1"],
        ))),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");
    let mut reporter = NodeReporter::new(
        collector,
        sink,
        report_identity(
            "6fec233b9cf043af5fdfa517e1d315b29c492ac7772047bb8ba35e274f10dacd".to_owned(),
        ),
        Duration::from_secs(5),
        system_clock(),
    );

    let observations = reporter.observations();
    assert_eq!(observations.latest(), None);
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("first report");
    let first = observations.latest().expect("first published collection");
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("second report");
    let second = observations.latest().expect("second published collection");

    let requests = requests.lock().expect("recorded requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].snapshot.as_ref(), Some(&second.snapshot));
    assert_eq!(
        (first.revision, second.revision),
        (requests[0].revision, requests[1].revision),
        "each published collection carries the revision that reports it"
    );
    assert!(second.collected_at >= first.collected_at);
    assert_eq!(
        requests
            .iter()
            .map(|request| (
                request.site_id.as_str(),
                request.node_id.as_str(),
                request.incarnation_id.as_str(),
                request.revision,
                request.observation_configuration_sha256.as_str(),
                request
                    .snapshot
                    .as_ref()
                    .expect("required snapshot")
                    .runtimes
                    .len(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "site-one",
                "worker-one",
                "11111111111111111111111111111111",
                1,
                "6fec233b9cf043af5fdfa517e1d315b29c492ac7772047bb8ba35e274f10dacd",
                3,
            ),
            (
                "site-one",
                "worker-one",
                "11111111111111111111111111111111",
                2,
                "6fec233b9cf043af5fdfa517e1d315b29c492ac7772047bb8ba35e274f10dacd",
                3,
            ),
        ]
    );
}

#[tokio::test]
async fn a_collection_already_reading_hardware_is_never_the_first_to_observe_a_new_process() {
    let reading = FeedProbe::default();
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(reading.clone())),
        probes(Arc::new(FakeDocker::available(
            DockerCapabilityReport::new(ExecutionPlatform::linux_amd64(), ["/etc/cdi"]),
            ["nvidia.com/gpu=GPU-1"],
        ))),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");
    let mut reporter = NodeReporter::new(
        collector,
        RecordingSink::default(),
        report_identity("digest".to_owned()),
        Duration::from_secs(5),
        system_clock(),
    );
    let observations = reporter.observations();
    *reading.feed.lock().expect("probed feed") = Some(observations.clone());

    assert_eq!(observations.next_revision(), 1);
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("first report");
    assert_eq!(observations.next_revision(), 2);
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("second report");

    // While collection 1 read the hardware, a process recorded then could first show in 2.
    assert_eq!(*reading.seen.lock().expect("seen revisions"), vec![2, 3]);
    assert_eq!(observations.next_revision(), 3);
}

#[tokio::test]
async fn each_collection_reading_is_taken_for_its_revision_before_that_collection_is_published() {
    let collector = NodeObservationCollector::with_timeout(
        simulated_observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(Arc::new(FakeDocker::unavailable("must not probe Docker"))),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");
    let reading = RevisionReading::default();
    let mut reporter = NodeReporter::new(
        collector,
        RecordingSink::default(),
        report_identity("digest".to_owned()),
        Duration::from_secs(5),
        system_clock(),
    )
    .with_collection_reading(Arc::new(reading.clone()));
    *reading.feed.lock().expect("read feed") = Some(reporter.observations());

    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("first report");
    reporter
        .report_once(UNIX_EPOCH)
        .await
        .expect("second report");

    // Each reading names its collection's revision while the previous one is still the latest.
    assert_eq!(
        *reading.taken.lock().expect("taken readings"),
        vec![(1, None), (2, Some(1))]
    );
    assert_eq!(
        reporter
            .observations()
            .latest()
            .map(|latest| latest.revision),
        Some(2)
    );
}

#[tokio::test]
async fn native_targets_report_availability_from_the_shared_host_probe() {
    let collector = native_collector(ready_native_report);

    let runtimes = collector.collect().await.snapshot.runtimes;

    assert_eq!(
        runtimes
            .iter()
            .map(|runtime| (
                runtime.execution_target_id.as_str(),
                runtime_result_code(runtime)
            ))
            .collect::<Vec<_>>(),
        vec![
            ("native-darwin", "available".to_owned()),
            (
                "native-linux",
                "unsupported PLATFORM_UNSUPPORTED".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn hung_native_probe_is_not_stacked_by_later_collections() {
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    static RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    fn blocking_report() -> NativeCapabilityReport {
        STARTED.fetch_add(1, Ordering::SeqCst);
        while !RELEASED.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        ready_native_report()
    }

    let mut collector = native_collector(blocking_report);
    collector.runtime.probe_timeout = Duration::from_millis(50);

    let timed_out = collector.collect().await.snapshot.runtimes;
    let skipped = collector.collect().await.snapshot.runtimes;

    assert_eq!(
        runtime_result_code(&timed_out[0]),
        "unavailable RUNTIME_PROBE_FAILED"
    );
    assert_eq!(
        runtime_result_code(&skipped[0]),
        "unavailable RUNTIME_PROBE_FAILED"
    );
    assert_eq!(STARTED.load(Ordering::SeqCst), 1);

    RELEASED.store(true, Ordering::SeqCst);
    collector.runtime.probe_timeout = Duration::from_secs(5);
    let mut recovered = skipped;
    for _ in 0..500 {
        recovered = collector.collect().await.snapshot.runtimes;
        if runtime_result_code(&recovered[0]) == "available" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(runtime_result_code(&recovered[0]), "available");
    assert_eq!(STARTED.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn collection_reports_every_configured_target_from_one_bounded_probe_set() {
    let docker = Arc::new(FakeDocker::available(
        DockerCapabilityReport::new(ExecutionPlatform::linux_amd64(), ["/etc/cdi"]),
        ["nvidia.com/gpu=GPU-1"],
    ));
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(docker.clone()),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");

    let snapshot = collector.collect().await.snapshot;

    assert!(matches!(
        snapshot.hardware.and_then(|hardware| hardware.result),
        Some(node_hardware_observation::Result::Snapshot(
            HardwareSnapshot {
                host: Some(HostObservation {
                    result: Some(host_observation::Result::Facts(HostFacts {
                        cpu_count: 8,
                        ..
                    })),
                }),
                ..
            }
        ))
    ));
    assert_eq!(
        snapshot
            .runtimes
            .iter()
            .map(|runtime| (
                runtime.execution_target_id.as_str(),
                RuntimeKind::try_from(runtime.kind).expect("Runtime kind"),
                runtime.result.as_ref(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "application-target",
                RuntimeKind::Application,
                Some(&runtime_observation::Result::Available(Empty {})),
            ),
            (
                "model-target",
                RuntimeKind::Model,
                Some(&runtime_observation::Result::Available(Empty {})),
            ),
            (
                "simulated-target",
                RuntimeKind::Model,
                Some(&runtime_observation::Result::Available(Empty {})),
            ),
        ]
    );
    assert_eq!(docker.probe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(docker.selector_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timed_out_runtime_probe_is_cancelled_before_the_next_snapshot() {
    let docker = Arc::new(SlowDocker::default());
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(docker.clone()),
        Duration::from_millis(1),
        system_clock(),
    )
    .expect("collector");

    for expected_calls in 1..=2 {
        let snapshot = collector.collect().await.snapshot;

        assert_eq!(docker.active.load(Ordering::SeqCst), 0);
        assert_eq!(docker.calls.load(Ordering::SeqCst), expected_calls);
        assert!(snapshot.runtimes[..2].iter().all(|runtime| matches!(
            runtime.result,
            Some(runtime_observation::Result::Unavailable(ref unavailable))
                if unavailable.code == "RUNTIME_PROBE_FAILED"
        )));
        assert!(matches!(
            snapshot.runtimes[2].result,
            Some(runtime_observation::Result::Available(_))
        ));
    }
}

#[tokio::test]
async fn collection_preserves_independent_hardware_and_runtime_failures() {
    let docker = Arc::new(FakeDocker::unavailable("socket unavailable"));
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(PanickingHardware)),
        probes(docker.clone()),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");

    let snapshot = collector.collect().await.snapshot;

    let hardware = snapshot.hardware.expect("required hardware outcome");
    assert!(matches!(
        hardware.result,
        Some(node_hardware_observation::Result::Unavailable(ref unavailable))
            if unavailable.code == "COLLECTION_FAILED"
                && unavailable.message == "Hardware observation failed on this node."
    ));
    assert!(matches!(
        snapshot.runtimes[0].result,
        Some(runtime_observation::Result::Unavailable(ref unavailable))
            if unavailable.code == "RUNTIME_PROBE_FAILED"
                && unavailable.message == "The configured runtime infrastructure is unavailable."
    ));
    assert!(matches!(
        snapshot.runtimes[1].result,
        Some(runtime_observation::Result::Unavailable(ref unavailable))
            if unavailable.code == "RUNTIME_PROBE_FAILED"
    ));
    assert!(matches!(
        snapshot.runtimes[2].result,
        Some(runtime_observation::Result::Available(_))
    ));
    assert_eq!(docker.probe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(docker.selector_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn collection_qualifies_platform_and_target_specific_cdi_prerequisites() {
    let docker = Arc::new(FakeDocker::available(
        DockerCapabilityReport::new(ExecutionPlatform::linux_arm64(), ["/etc/cdi"]),
        ["nvidia.com/gpu=another-device"],
    ));
    let collector = NodeObservationCollector::with_timeout(
        observation_configuration(),
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        probes(docker.clone()),
        Duration::from_secs(1),
        system_clock(),
    )
    .expect("collector configuration");

    let snapshot = collector.collect().await.snapshot;

    for runtime in &snapshot.runtimes[..2] {
        assert!(matches!(
            runtime.result,
            Some(runtime_observation::Result::Unsupported(ref unavailable))
                if unavailable.code == "PLATFORM_UNSUPPORTED"
                    && unavailable.message == "The runtime infrastructure platform cannot support this execution target."
        ));
    }
    assert!(matches!(
        snapshot.runtimes[2].result,
        Some(runtime_observation::Result::Available(_))
    ));
    assert_eq!(docker.selector_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn device_bearing_application_targets_qualify_cdi_and_their_devices() {
    let configuration = crate::node::observation_configuration::node_observation_configuration(
        &application_targets_site(),
        "node-one",
    )
    .expect("node observation configuration");
    let outcome = |result: Option<&runtime_observation::Result>| match result {
        Some(runtime_observation::Result::Available(_)) => "available".to_owned(),
        Some(runtime_observation::Result::Unavailable(unavailable)) => unavailable.code.clone(),
        other => panic!("unexpected runtime outcome: {other:?}"),
    };

    for (cdi_spec_dirs, selectors, expected_gpu_target) in [
        (Vec::new(), vec!["nvidia.com/gpu=0"], "CDI_UNAVAILABLE"),
        (
            vec!["/etc/cdi"],
            vec!["nvidia.com/gpu=1"],
            "CDI_DEVICE_UNAVAILABLE",
        ),
        (vec!["/etc/cdi"], vec!["nvidia.com/gpu=0"], "available"),
    ] {
        let docker = Arc::new(FakeDocker::available(
            DockerCapabilityReport::new(ExecutionPlatform::linux_amd64(), cdi_spec_dirs),
            selectors,
        ));
        let collector = NodeObservationCollector::with_timeout(
            configuration.clone(),
            Arc::new(LocalNodeCapability::new(FixedHardware::available())),
            probes(docker.clone()),
            Duration::from_secs(1),
            system_clock(),
        )
        .expect("collector configuration");

        let snapshot = collector.collect().await.snapshot;

        assert_eq!(
            snapshot
                .runtimes
                .iter()
                .map(|runtime| (
                    runtime.execution_target_id.as_str(),
                    outcome(runtime.result.as_ref())
                ))
                .collect::<Vec<_>>(),
            [
                ("deviceless-application-target", "available".to_owned()),
                ("gpu-application-target", expected_gpu_target.to_owned()),
            ],
            "{expected_gpu_target}"
        );
        assert_eq!(docker.selector_calls.load(Ordering::SeqCst), 1);
    }
}

/// A site whose node `node-one` holds a Docker application target with one GPU and one without.
fn application_targets_site() -> dotcore_proto::dotcore::site::v1::Site {
    use dotcore_proto::dotcore::site::v1::{
        ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget, Site,
        SiteNode, application_runtime_binding, execution_target,
    };

    let target = |execution_target_id: &str, devices: &[&str]| ExecutionTarget {
        execution_target_id: execution_target_id.to_owned(),
        display_name: execution_target_id.to_owned(),
        site_node_id: "node-one".to_owned(),
        platform: Some(platform()),
        accelerator_device_ids: devices.iter().map(|&device| device.to_owned()).collect(),
        runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
            ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                    DockerApplicationRuntimeAdapter {},
                )),
            },
        )),
    };
    Site {
        nodes: vec![SiteNode {
            node_id: "node-one".to_owned(),
            ..Default::default()
        }],
        execution_targets: vec![
            target("gpu-application-target", &["nvidia.com/gpu=0"]),
            target("deviceless-application-target", &[]),
        ],
        ..Default::default()
    }
}

fn observation_configuration() -> NodeObservationConfiguration {
    NodeObservationConfiguration {
        schema_version: 1,
        interval_seconds: 5,
        execution_targets: vec![
            NodeExecutionTarget {
                execution_target_id: "application-target".to_owned(),
                kind: RuntimeKind::Application as i32,
                platform: Some(platform()),
                implementation: Some(node_execution_target::Implementation::Docker(
                    DockerRuntimeObservationConfiguration::default(),
                )),
            },
            NodeExecutionTarget {
                execution_target_id: "model-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                platform: Some(platform()),
                implementation: Some(node_execution_target::Implementation::Docker(
                    DockerRuntimeObservationConfiguration {
                        accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                        accelerator_device_ids: vec!["nvidia.com/gpu=GPU-1".to_owned()],
                    },
                )),
            },
            NodeExecutionTarget {
                execution_target_id: "simulated-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                platform: Some(platform()),
                implementation: Some(node_execution_target::Implementation::Simulated(Empty {})),
            },
        ],
    }
}

fn native_collector(
    native: fn() -> NativeCapabilityReport,
) -> NodeObservationCollector<FixedHardware> {
    let native_target = |id: &str, operating_system: &str| NodeExecutionTarget {
        execution_target_id: id.to_owned(),
        kind: RuntimeKind::Model as i32,
        platform: Some(OciPlatform {
            operating_system: operating_system.to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }),
        implementation: Some(node_execution_target::Implementation::Native(
            NativeRuntimeObservationConfiguration {
                workload_lifetime: 0,
            },
        )),
    };
    let configuration = NodeObservationConfiguration {
        schema_version: 1,
        interval_seconds: 5,
        execution_targets: vec![
            native_target("native-darwin", "darwin"),
            native_target("native-linux", "linux"),
        ],
    };
    NodeObservationCollector::new(
        configuration,
        Arc::new(LocalNodeCapability::new(FixedHardware::available())),
        RuntimeCapabilityProbes {
            docker: Arc::new(FakeDocker::unavailable("must not probe Docker")),
            native: Arc::new(FakeNative(native)),
        },
        system_clock(),
    )
    .expect("collector configuration")
}

fn runtime_result_code(
    runtime: &dotcore_proto::dotcore::observation::v1::RuntimeObservation,
) -> String {
    match runtime.result.as_ref().expect("runtime result") {
        runtime_observation::Result::Available(_) => "available".to_owned(),
        runtime_observation::Result::Unsupported(detail) => {
            format!("unsupported {}", detail.code)
        }
        runtime_observation::Result::Unavailable(detail) => {
            format!("unavailable {}", detail.code)
        }
    }
}

fn report_identity(observation_configuration_sha256: String) -> NodeReportIdentity {
    NodeReportIdentity {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        incarnation: crate::view::fencing::NodeIncarnation::parse("1".repeat(32))
            .expect("fixed node incarnation"),
        observation_configuration_sha256,
    }
}

fn simulated_observation_configuration() -> NodeObservationConfiguration {
    NodeObservationConfiguration {
        schema_version: 1,
        interval_seconds: 5,
        execution_targets: vec![NodeExecutionTarget {
            execution_target_id: "simulated-target".to_owned(),
            kind: RuntimeKind::Model as i32,
            platform: Some(platform()),
            implementation: Some(node_execution_target::Implementation::Simulated(Empty {})),
        }],
    }
}

/// Site whose manager node hosts one simulated model target and declares one node.
fn manager_site() -> dotcore_proto::dotcore::site::v1::Site {
    use dotcore_proto::dotcore::site::v1::{
        ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter, Site, SiteManagerRole,
        SiteNode, execution_target, model_runtime_binding,
    };

    Site {
        site_id: "site-one".to_owned(),
        nodes: vec![
            SiteNode {
                node_id: "manager-node".to_owned(),
                display_name: "Manager".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                host: "127.0.0.1".to_owned(),
                manager_host: String::new(),
            },
            SiteNode {
                node_id: "worker-node".to_owned(),
                display_name: "Node".to_owned(),
                site_manager: None,
                host: "127.0.0.2".to_owned(),
                manager_host: String::new(),
            },
        ],
        execution_targets: vec![ExecutionTarget {
            execution_target_id: "simulated-target".to_owned(),
            display_name: "simulated-target".to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(platform()),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedRuntimeAdapter::default(),
                    )),
                },
            )),
        }],
        ..Default::default()
    }
}

fn platform() -> OciPlatform {
    OciPlatform {
        operating_system: "linux".to_owned(),
        architecture: "amd64".to_owned(),
        variant: None,
    }
}

struct FixedHardware(HardwareSnapshot);

impl FixedHardware {
    fn available() -> Self {
        Self(HardwareSnapshot {
            observed_at: None,
            host: Some(HostObservation {
                result: Some(host_observation::Result::Facts(HostFacts {
                    host_name: "worker-host".to_owned(),
                    operating_system_or_kernel: "Linux".to_owned(),
                    cpu_count: 8,
                    total_memory_bytes: 16 * 1024 * 1024 * 1024,
                    available_memory_bytes: 8 * 1024 * 1024 * 1024,
                    cached_memory_bytes: None,
                    filesystems: vec![dotcore_proto::dotcore::observation::v1::FilesystemFacts {
                        mountpoint: "/".to_owned(),
                        fstype: "ext4".to_owned(),
                        size_bytes: 100 * 1024 * 1024 * 1024,
                    }],
                })),
            }),
            accelerators: Vec::new(),
        })
    }
}

impl FixedHardware {
    /// Complete host facts with the observation time the manager requires.
    fn observed() -> Self {
        Self(HardwareSnapshot {
            observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 100,
                nanos: 0,
            }),
            ..Self::available().0
        })
    }
}

impl HardwareSnapshotSource for FixedHardware {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.0.clone()
    }

    fn sample(&self) -> HardwareSample {
        HardwareSample::from_snapshot(&self.0)
    }
}

/// Hardware that records, each time it is read, the revision the feed then names as the first
/// that can observe a new process.
#[derive(Clone, Default)]
struct FeedProbe {
    feed: Arc<Mutex<Option<NodeObservationFeed>>>,
    seen: Arc<Mutex<Vec<u64>>>,
}

impl HardwareSnapshotSource for FeedProbe {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        if let Some(feed) = self.feed.lock().expect("probed feed").as_ref() {
            self.seen
                .lock()
                .expect("seen revisions")
                .push(feed.next_revision());
        }
        FixedHardware::available().0
    }

    fn sample(&self) -> HardwareSample {
        HardwareSample::from_snapshot(&FixedHardware::available().0)
    }
}

/// A collection reading that records each revision it was taken for and the revision of the
/// latest collection the feed published at that moment.
#[derive(Clone, Default)]
struct RevisionReading {
    feed: Arc<Mutex<Option<NodeObservationFeed>>>,
    taken: Arc<Mutex<Vec<TakenReading>>>,
}

/// The revision a reading was taken for and the latest published revision when it was taken.
type TakenReading = (u64, Option<u64>);

impl CollectionReading for RevisionReading {
    fn read(&self, revision: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let published = self
            .feed
            .lock()
            .expect("read feed")
            .as_ref()
            .and_then(NodeObservationFeed::latest)
            .map(|latest| latest.revision);
        self.taken
            .lock()
            .expect("taken readings")
            .push((revision, published));
        Box::pin(std::future::ready(()))
    }
}

struct PanickingHardware;

impl HardwareSnapshotSource for PanickingHardware {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        panic!("injected hardware collection panic")
    }

    fn sample(&self) -> HardwareSample {
        panic!("injected hardware sample panic")
    }
}

/// Opens one node control stream through the worker's pinned manager endpoint.
async fn open_stream(configuration: &WorkerPrivateClientConfiguration) -> anyhow::Result<()> {
    let client = ManagerNodeControlClient::new(configuration.endpoint()?);
    let (_outbound, receiver) = tokio::sync::mpsc::channel(1);
    let _messages = client.open(receiver).await.map_err(anyhow::Error::new)?;
    Ok(())
}

/// Manager stand-in that counts the node control streams it accepted.
#[derive(Clone)]
struct CountingNodeControlService {
    calls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl NodeControlService for CountingNodeControlService {
    type StreamNodeControlStream = futures_util::stream::BoxStream<
        'static,
        Result<dotcore_proto::dotcore::site::private::v1::StreamNodeControlResponse, Status>,
    >;

    async fn stream_node_control(
        &self,
        _request: Request<tonic::Streaming<StreamNodeControlRequest>>,
    ) -> Result<Response<Self::StreamNodeControlStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(Box::pin(futures_util::stream::pending())))
    }
}

struct TransportFixture {
    authority: TestAuthority,
    trust_anchor: Vec<u8>,
    manager_certificate: Vec<u8>,
    manager_key: Vec<u8>,
    worker_certificate: Vec<u8>,
    worker_key: Vec<u8>,
}

impl TransportFixture {
    fn new() -> Self {
        let authority = TestAuthority::new();
        let trust_anchor = authority.certificate.der().to_vec();
        let (manager_certificate, manager_key) = authority.issue("site-one", "manager-one", true);
        let (worker_certificate, worker_key) = authority.issue("site-one", "worker-one", false);
        Self {
            authority,
            trust_anchor,
            manager_certificate,
            manager_key,
            worker_certificate,
            worker_key,
        }
    }

    fn manager_server_configuration(&self) -> PrivateServerConfiguration {
        PrivateServerConfiguration {
            bind_address: "127.0.0.1:0".parse().expect("manager bind address"),
            site_id: "site-one".to_owned(),
            local_node_id: "manager-one".to_owned(),
            local_role: PrivateNodeRole::Manager,
            certificate_der: self.manager_certificate.clone(),
            private_key_pkcs8: self.manager_key.clone(),
            trust_anchor_der: self.trust_anchor.clone(),
            peers: BTreeMap::from([(
                "worker-one".to_owned(),
                PrivatePeerPolicy {
                    role: PrivateNodeRole::Worker,
                    certificate_sha256: CertificateFingerprint::for_certificate(
                        &self.worker_certificate,
                    ),
                },
            )]),
        }
    }

    fn private_configuration(
        &self,
        address: std::net::SocketAddr,
        manager_node_id: &str,
        manager_certificate: Vec<u8>,
    ) -> WorkerPrivateClientConfiguration {
        WorkerPrivateClientConfiguration {
            manager_endpoint: format!("https://{address}")
                .parse()
                .expect("manager endpoint"),
            site_id: "site-one".to_owned(),
            manager_node_id: manager_node_id.to_owned(),
            node_id: "worker-one".to_owned(),
            trust_anchor_der: self.trust_anchor.clone(),
            manager_certificate_der: manager_certificate,
            worker_certificate_der: self.worker_certificate.clone(),
            worker_private_key_pkcs8: self.worker_key.clone(),
            bounds: ManagerTransportBounds::default(),
        }
    }
}

#[derive(Default)]
struct RecordingSink {
    requests: Arc<Mutex<Vec<NodeObservation>>>,
    reported: Arc<Notify>,
}

impl ObservationReportSink for RecordingSink {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        self.requests.lock().expect("record requests").push(request);
        self.reported.notify_one();
        Box::pin(async { Ok(()) })
    }
}

/// Sink that records the stamp of the collection it delivers, then takes `delivery` on its clock.
struct StampSink {
    clock: VirtualClock,
    observations: NodeObservationFeed,
    delivery: Duration,
    stamps: Arc<Mutex<Vec<SystemTime>>>,
    reported: Arc<Notify>,
}

impl ObservationReportSink for StampSink {
    fn report(
        &self,
        _: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        let stamp = self.observations.latest().expect("collection").collected_at;
        Box::pin(async move {
            self.stamps.lock().expect("stamps").push(stamp);
            self.reported.notify_one();
            self.clock
                .sleep_until(self.clock.now() + self.delivery)
                .await;
            Ok(())
        })
    }
}

/// Sink whose first delivery never completes and whose later deliveries succeed.
#[derive(Default)]
struct StallFirstSink {
    attempts: AtomicUsize,
    requests: Arc<Mutex<Vec<NodeObservation>>>,
    reported: Arc<Notify>,
}

impl ObservationReportSink for StallFirstSink {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(std::future::pending());
        }
        self.requests.lock().expect("record requests").push(request);
        self.reported.notify_one();
        Box::pin(async { Ok(()) })
    }
}

/// Sink whose delivery panics before returning a future.
struct PanickingSink;

impl ObservationReportSink for PanickingSink {
    fn report(
        &self,
        _: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        panic!("injected report panic")
    }
}

/// TCP listener that accepts connections and holds them open without ever writing, like a
/// manager host that vanished after its kernel accepted the connection.
struct SilentListener {
    address: std::net::SocketAddr,
    accepted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl SilentListener {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent listener");
        let address = listener.local_addr().expect("silent listener address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
        });
        Self {
            address,
            accepted,
            task,
        }
    }
}

impl Drop for SilentListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct SlowDocker {
    active: Arc<AtomicUsize>,
    calls: AtomicUsize,
}

impl DockerCapabilityProbe for SlowDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = Arc::clone(&self.active);
        Box::pin(async move {
            let _active = ActiveProbe(active);
            tokio::time::sleep(Duration::from_secs(1)).await;
            anyhow::bail!("slow Docker probe completed")
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(async { anyhow::bail!("selectors must not be queried") })
    }
}

struct ActiveProbe(Arc<AtomicUsize>);

impl Drop for ActiveProbe {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct FakeDocker {
    probe_result: Result<DockerCapabilityReport, String>,
    selectors: Result<BTreeSet<String>, String>,
    probe_calls: AtomicUsize,
    selector_calls: AtomicUsize,
}

impl FakeDocker {
    fn available(
        report: DockerCapabilityReport,
        selectors: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            probe_result: Ok(report),
            selectors: Ok(selectors.into_iter().map(str::to_owned).collect()),
            probe_calls: AtomicUsize::new(0),
            selector_calls: AtomicUsize::new(0),
        }
    }

    fn unavailable(message: &str) -> Self {
        Self {
            probe_result: Err(message.to_owned()),
            selectors: Err("selectors must not be queried".to_owned()),
            probe_calls: AtomicUsize::new(0),
            selector_calls: AtomicUsize::new(0),
        }
    }
}

impl DockerCapabilityProbe for FakeDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { self.probe_result.clone().map_err(anyhow::Error::msg) })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        self.selector_calls.fetch_add(1, Ordering::SeqCst);
        let result = self.selectors.clone().map_err(anyhow::Error::msg);
        Box::pin(async move { result })
    }
}

/// Native host checks answered by a fixed report function.
struct FakeNative(fn() -> NativeCapabilityReport);

impl NativeCapabilityProbe for FakeNative {
    fn probe(&self) -> NativeCapabilityReport {
        (self.0)()
    }
}

/// Probe set with `docker` and a native host ready on darwin/arm64.
fn probes(docker: Arc<dyn DockerCapabilityProbe>) -> RuntimeCapabilityProbes {
    RuntimeCapabilityProbes {
        docker,
        native: Arc::new(FakeNative(ready_native_report)),
    }
}

fn ready_native_report() -> NativeCapabilityReport {
    NativeCapabilityReport {
        host_platform: Some(ExecutionPlatform::darwin_arm64()),
        launcher: Ok(()),
        state_root: Ok(()),
        nix: Ok(()),
    }
}

fn system_clock() -> Arc<dyn dotcore_domain::clock::Clock> {
    Arc::new(dotcore_domain::clock::SystemClock)
}
