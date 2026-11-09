use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, UNIX_EPOCH},
};

use dotcore_domain::{
    clock::{Clock, VirtualClock},
    hardware_observation::NodeHardwareObservation,
};
use dotcore_observability::{
    ForwarderConfig, InMemoryMetricStore, InMemoryStoreConfig, InProcessBucketTransport,
    MetricBucketIngest, MetricForwarder, MetricQuery, MetricStore, PointValue, QueryRange, Reduce,
    ResultSeries, SeriesAggregate, forwarder_metric_descriptors, labels::LabelSet,
};
use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    deployment::v1::ExactPlacement,
    model::v1::{
        ModelDeploymentFailure, ModelDeploymentLifecycleTransition, ModelDeploymentState,
        ObservedPlacement,
    },
    observation::v1::{
        AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, AcceleratorVendor,
        HardwareSnapshot, accelerator_observation,
    },
    site::{
        private::v1::{
            ControllerStatusSection, NodeControllerStatus, NodeViewReference,
            controller_status_section,
        },
        v1::{
            ModelRuntimeBinding, SimulatedRuntimeAdapter, execution_target, model_runtime_binding,
        },
    },
};
use dotcore_site::{
    node::{
        agent::CollectionReading as _,
        hardware_inventory::{
            HardwareInventoryEntry, HardwareInventoryPublisher, HardwareInventorySource as _,
        },
    },
    view::{
        contract::validate_controller_status,
        fencing::{AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection},
    },
};

use super::*;
use crate::model::controller::{
    accelerator_memory::{
        ProcessMemoryPort, RuntimeMemoryReading,
        tests::{observed_processes, process, workload},
    },
    metrics::{
        DEPLOYMENT_ACCELERATOR_MEMORY_USED, DEPLOYMENT_MEMORY_SOURCE, deployment_memory_records,
        deployment_metric_descriptors,
    },
    serving_targets::{LocalServingTargetsPublisher, local_target_for_test},
};

#[test]
fn status_reports_reconciling_until_the_record_reflects_the_assigned_generation() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        2,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(1));
    let stale = fixture.status();
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(2));
    fixture.serve(vec![live_target("model-one", 2, 7, 1)]);
    let reconciled = fixture.status();

    assert_eq!(stale.deployments.len(), 1);
    assert_eq!(stale.deployments[0].lifecycle_generation, 2);
    assert_eq!(
        stale.deployments[0].lifecycle,
        ModelLifecycleObservation::Reconciling as i32
    );
    assert_eq!(
        stale.deployments[0].binding_readiness,
        ModelBindingReadiness::Absent as i32
    );
    assert_eq!(stale.deployments[0].realization_generation, None);
    assert!(stale.deployments[0].observed_placements.is_empty());
    assert_eq!(
        reconciled.deployments[0].lifecycle,
        ModelLifecycleObservation::Ready as i32
    );
    assert_eq!(
        reconciled.deployments[0].binding_readiness,
        ModelBindingReadiness::Ready as i32
    );
    assert_eq!(reconciled.deployments[0].realization_generation, Some(7));
    assert_eq!(reconciled.deployments[0].observed_placements.len(), 1);
}

#[test]
fn status_drops_observed_placement_rows_the_manager_would_reject() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        2,
    )]);
    let placement = |node_id: &str, device_ids: &[&str]| ObservedPlacement {
        managed_node_id: node_id.to_owned(),
        accelerator_device_ids: device_ids.iter().map(|id| (*id).to_owned()).collect(),
    };
    fixture
        .state
        .upsert(
            CurrentModelDeployment {
                model_deployment_id: "model-one".to_owned(),
                reconciled_lifecycle_generation: Some(2),
                state: Some(ModelDeploymentState {
                    status: ModelDeploymentStatus::Loading as i32,
                    status_message: "Loading".to_owned(),
                    observed_placements: vec![
                        placement("worker-two", &["GPU-other-node"]),
                        placement("worker-one", &["GPU-a", "GPU-b"]),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .expect("node current record");

    let status = fixture.status();

    assert_eq!(
        status.deployments[0].observed_placements,
        [placement("worker-one", &["GPU-a", "GPU-b"])]
    );
    validate_controller_status(&report(status)).expect("bounded status is accepted");
}

#[test]
fn status_reports_ready_only_from_a_live_target_serving_the_declared_generation() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        2,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(2));

    let without_target = fixture.status();
    fixture.serve(vec![live_target("model-one", 1, 40, 3)]);
    let earlier_generation_target = fixture.status();
    fixture.serve(vec![live_target("model-one", 2, 41, 3)]);
    let live = fixture.status();
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(1));
    let earlier_generation_record = fixture.status();

    let evidence = |status: &ModelControllerStatusSection| {
        let deployment = &status.deployments[0];
        (
            ModelLifecycleObservation::try_from(deployment.lifecycle).expect("lifecycle"),
            ModelBindingReadiness::try_from(deployment.binding_readiness).expect("readiness"),
            deployment.binding_token.clone(),
            deployment.realization_generation,
        )
    };
    assert_eq!(
        evidence(&without_target),
        (
            ModelLifecycleObservation::Ready,
            ModelBindingReadiness::NotReady,
            String::new(),
            None
        )
    );
    assert_eq!(
        evidence(&earlier_generation_target),
        evidence(&without_target)
    );
    assert_eq!(
        live.deployments,
        [PrivateModelDeploymentStatus {
            model_deployment_id: "model-one".to_owned(),
            lifecycle_generation: 2,
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: "target-one".to_owned(),
            }),
            realization_generation: Some(41),
            lifecycle: ModelLifecycleObservation::Ready as i32,
            status_message: "MODEL_DEPLOYMENT_STATUS_READY".to_owned(),
            preparation_progress: None,
            binding_readiness: ModelBindingReadiness::Ready as i32,
            failure: None,
            state_revision: 1,
            observed_placements: vec![ObservedPlacement {
                managed_node_id: "worker-one".to_owned(),
                accelerator_device_ids: Vec::new(),
            }],
            binding_token: expected_binding_token("model-one", "model-one-realization", 3, 2),
            resolved_launch_identities: Vec::new(),
            accelerator_memory: Some(not_applicable_memory()),
        }]
    );
    assert_eq!(
        evidence(&earlier_generation_record),
        (
            ModelLifecycleObservation::Reconciling,
            ModelBindingReadiness::Absent,
            String::new(),
            None
        )
    );
}

#[test]
fn replacing_the_binding_or_its_served_generation_changes_the_reported_token() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        2,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(2));
    let resumed = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        3,
    )]);
    resumed.record("model-one", ModelDeploymentStatus::Ready, Some(3));

    fixture.serve(vec![live_target("model-one", 2, 41, 3)]);
    let original = fixture.status().deployments[0].binding_token.clone();
    fixture.serve(vec![live_target("model-one", 2, 41, 3)]);
    let republished = fixture.status().deployments[0].binding_token.clone();
    fixture.serve(vec![live_target("model-one", 2, 41, 4)]);
    let replaced = fixture.status().deployments[0].binding_token.clone();
    resumed.serve(vec![live_target("model-one", 3, 41, 3)]);
    let next_generation = resumed.status().deployments[0].binding_token.clone();

    assert_eq!(
        original,
        expected_binding_token("model-one", "model-one-realization", 3, 2)
    );
    assert_eq!(republished, original);
    assert_eq!(
        replaced,
        expected_binding_token("model-one", "model-one-realization", 4, 2)
    );
    assert_eq!(
        next_generation,
        expected_binding_token("model-one", "model-one-realization", 3, 3)
    );
    assert_eq!(
        [&original, &replaced, &next_generation]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
}

#[test]
fn serving_changes_notify_observers_and_revocation_reports_not_ready() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        2,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(2));
    let changes = Arc::new(AtomicUsize::new(0));
    fixture.status.register_change_observer({
        let changes = Arc::clone(&changes);
        Arc::new(move || {
            changes.fetch_add(1, Ordering::SeqCst);
        })
    });

    fixture.serve(vec![live_target("model-one", 2, 41, 3)]);
    let ready = fixture.status().deployments[0].clone();
    fixture.serve(vec![live_target("model-one", 2, 42, 4)]);
    let replaced = fixture.status().deployments[0].clone();
    fixture.serve(Vec::new());
    let revoked = fixture.status().deployments[0].clone();

    assert_eq!(changes.load(Ordering::SeqCst), 3);
    assert_eq!(
        ready.binding_token,
        expected_binding_token("model-one", "model-one-realization", 3, 2)
    );
    assert_eq!(
        (
            replaced.binding_readiness,
            replaced.realization_generation,
            replaced.binding_token.as_str()
        ),
        (
            ModelBindingReadiness::Ready as i32,
            Some(42),
            expected_binding_token("model-one", "model-one-realization", 4, 2).as_str()
        )
    );
    assert_eq!(
        (
            revoked.lifecycle,
            revoked.binding_readiness,
            revoked.binding_token.as_str(),
            revoked.realization_generation
        ),
        (
            ModelLifecycleObservation::Ready as i32,
            ModelBindingReadiness::NotReady as i32,
            "",
            None
        )
    );
}

#[test]
fn a_failed_record_reports_its_recorded_failure_summary() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        3,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Failed, Some(3));
    let mut record = fixture
        .state
        .snapshot()
        .expect("node state")
        .model_deployments[0]
        .clone();
    let revision = record.record_revision;
    record.state.as_mut().expect("failed state").failure = Some(ModelDeploymentFailure {
        code: "resume_recovery_required".to_owned(),
        ..Default::default()
    });
    fixture
        .state
        .upsert(record, Some(revision))
        .expect("recovery-required record");

    let status = fixture.status();

    assert_eq!(
        status.deployments[0].lifecycle,
        ModelLifecycleObservation::Failed as i32
    );
    assert_eq!(
        status.deployments[0].failure,
        Some(ModelDeploymentFailure {
            code: "resume_recovery_required".to_owned(),
            ..Default::default()
        })
    );
}

#[test]
fn pausing_record_is_reported_as_pausing_until_the_stop_is_confirmed() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Paused,
        3,
    )]);
    fixture.record("model-one", ModelDeploymentStatus::Pausing, Some(3));
    let pausing = fixture.status();
    fixture.record("model-one", ModelDeploymentStatus::Paused, Some(3));
    let paused = fixture.status();

    assert_eq!(
        pausing.deployments[0].lifecycle,
        ModelLifecycleObservation::Pausing as i32
    );
    assert_eq!(
        pausing.deployments[0].binding_readiness,
        ModelBindingReadiness::NotReady as i32
    );
    assert_eq!(
        paused.deployments[0].lifecycle,
        ModelLifecycleObservation::Paused as i32
    );
}

#[test]
fn recorded_lifecycle_message_and_download_bytes_are_reported_as_recorded() {
    use ModelLifecycleObservation as Observed;
    let bytes = |completed, total| ModelPreparationProgress {
        completed_bytes: completed,
        total_bytes: total,
    };
    let cases = [
        (
            ModelDeploymentStatus::Downloading,
            "Downloading model artifacts",
            Some((40, Some(100))),
            Observed::Downloading,
            Some(bytes(40, Some(100))),
        ),
        (
            ModelDeploymentStatus::Downloading,
            "Simulating model artifact download",
            Some((25, Some(100))),
            Observed::Downloading,
            Some(bytes(25, Some(100))),
        ),
        (
            ModelDeploymentStatus::Downloading,
            "Preparing model realization",
            Some((0, None)),
            Observed::Downloading,
            Some(bytes(0, None)),
        ),
        (
            ModelDeploymentStatus::Downloading,
            "Downloading model artifacts",
            Some((101, Some(100))),
            Observed::Downloading,
            Some(bytes(101, None)),
        ),
        (
            ModelDeploymentStatus::Preparing,
            "Preparing",
            None,
            Observed::Preparing,
            None,
        ),
        (
            ModelDeploymentStatus::Loading,
            "Checking model artifacts",
            Some((5, Some(10))),
            Observed::Loading,
            None,
        ),
        (
            ModelDeploymentStatus::Loading,
            "Resuming",
            None,
            Observed::Loading,
            None,
        ),
        (
            ModelDeploymentStatus::WarmingUp,
            "Warming up",
            None,
            Observed::WarmingUp,
            None,
        ),
        (
            ModelDeploymentStatus::Unavailable,
            "Unavailable",
            None,
            Observed::Unavailable,
            None,
        ),
        (
            ModelDeploymentStatus::Unloading,
            "Unloading",
            None,
            Observed::Stopping,
            None,
        ),
    ];

    for (local, message, progress, lifecycle, reported) in cases {
        let status = local_status(local, message, progress);

        assert_eq!(status.lifecycle, lifecycle as i32, "{message}");
        assert_eq!(status.status_message, message);
        assert_eq!(status.preparation_progress, reported, "{message}");
        assert_eq!(status.failure, None, "{message}");
    }
}

#[test]
fn every_recorded_status_passes_status_validation() {
    let messages = [
        "Downloading model artifacts",
        "Simulating model artifact download",
        "Resuming",
        "",
    ];
    let progresses = [
        None,
        Some((0, None)),
        Some((40, Some(100))),
        Some((101, Some(100))),
        Some((0, Some(0))),
    ];
    let mut derived = BTreeSet::new();

    for local in (1..=12).filter_map(|value| ModelDeploymentStatus::try_from(value).ok()) {
        for message in messages.into_iter().chain([local.as_str_name()]) {
            for progress in progresses {
                let mut status = local_status(local, message, progress);
                status.accelerator_memory = Some(not_applicable_memory());
                derived.insert(status.lifecycle);

                validate_controller_status(&report(ModelControllerStatusSection {
                    deployments: vec![status],
                    cleanup_evidence: Vec::new(),
                }))
                .unwrap_or_else(|error| panic!("{local:?} {message:?} {progress:?}: {error}"));
            }
        }
    }
    assert_eq!(
        derived.len(),
        11,
        "every recorded status has its own lifecycle"
    );
}

#[test]
fn status_reports_assignments_without_records_and_keeps_cleanup_evidence() {
    let fixture = StatusFixture::new(vec![
        assigned("observed", ModelDeploymentDesiredState::Present, 1),
        assigned("unobserved", ModelDeploymentDesiredState::Present, 1),
        assigned("withdrawn", ModelDeploymentDesiredState::Teardown, 2),
    ]);
    fixture.record("observed", ModelDeploymentStatus::Ready, Some(1));
    let evidence = fixture
        .state
        .confirm_node_model_cleanup("withdrawn", 2, Some(2))
        .expect("node cleanup evidence");

    let model = fixture.status();

    assert_eq!(
        model
            .deployments
            .iter()
            .map(|status| (
                status.model_deployment_id.as_str(),
                ModelLifecycleObservation::try_from(status.lifecycle).expect("lifecycle"),
                status.state_revision,
            ))
            .collect::<Vec<_>>(),
        [
            ("observed", ModelLifecycleObservation::Ready, 1),
            ("unobserved", ModelLifecycleObservation::Reconciling, 0),
        ]
    );
    assert_eq!(
        model.cleanup_evidence,
        [cleanup_evidence_from_state(&evidence)]
    );
    validate_controller_status(&report(model)).expect("valid controller status");
}

#[test]
fn acknowledged_evidence_is_never_reported_again_while_its_teardown_stays_installed() {
    let fixture = StatusFixture::new(vec![assigned(
        "withdrawn",
        ModelDeploymentDesiredState::Teardown,
        2,
    )]);
    let evidence = fixture
        .state
        .confirm_node_model_cleanup("withdrawn", 2, Some(2))
        .expect("node cleanup evidence");
    assert_eq!(
        fixture.status().cleanup_evidence,
        [cleanup_evidence_from_state(&evidence)]
    );

    fixture
        .status
        .accept_receipts(&BTreeSet::from([("withdrawn".to_owned(), 2)]))
        .expect("acknowledged evidence");
    let fixture = fixture.reopened();
    assert_eq!(
        fixture
            .state
            .confirm_node_model_cleanup("withdrawn", 2, Some(9))
            .expect("reconciling the installed teardown again"),
        NodeModelCleanupEvidence {
            acknowledged: true,
            ..evidence
        }
    );
    fixture.status.retire_withdrawn_cleanup_evidence();
    let status = fixture.status();

    assert!(status.cleanup_evidence.is_empty());
    assert_eq!(
        status
            .deployments
            .iter()
            .map(|status| (
                status.model_deployment_id.as_str(),
                status.lifecycle_generation,
                ModelLifecycleObservation::try_from(status.lifecycle).expect("lifecycle"),
            ))
            .collect::<Vec<_>>(),
        [("withdrawn", 2, ModelLifecycleObservation::Reconciling)]
    );
    assert_eq!(
        fixture
            .state
            .node_model_cleanup_evidence()
            .expect("retained while installed")
            .len(),
        1
    );
}

#[test]
fn old_format_record_without_reconciled_generation_reports_reconciling() {
    let fixture = StatusFixture::new(vec![assigned(
        "model-one",
        ModelDeploymentDesiredState::Present,
        1,
    )]);
    // A record without the reconciled generation encodes exactly as the earlier schema did.
    fixture.record("model-one", ModelDeploymentStatus::Ready, None);
    let fixture = fixture.reopened();

    let status = fixture.status();

    assert_eq!(
        fixture
            .state
            .snapshot()
            .expect("reopened node state")
            .model_deployments[0]
            .reconciled_lifecycle_generation,
        None
    );
    assert_eq!(
        status.deployments[0].lifecycle,
        ModelLifecycleObservation::Reconciling as i32
    );
    assert_eq!(status.deployments[0].state_revision, 1);
}

#[test]
fn every_deployment_of_one_status_attributes_from_the_collection_pinned_for_it() {
    let (publisher, inventory) = HardwareInventoryPublisher::new();
    let fixture = StatusFixture::with_attribution(
        vec![
            assigned("model-one", ModelDeploymentDesiredState::Present, 1),
            assigned("model-two", ModelDeploymentDesiredState::Present, 1),
        ],
        model_target(),
        AcceleratorAttribution::new(Arc::new(inventory)),
    );
    for (id, process_id) in [("model-one", 41), ("model-two", 42)] {
        fixture.record(id, ModelDeploymentStatus::Ready, Some(1));
        fixture.attribution.observe(
            id,
            None,
            &dotcore_runtime::WorkloadObservation::Running(workload(
                "worker-one",
                id,
                process_id,
                700,
            )),
        );
    }
    publish_gpu_processes(&publisher, 1_000, 2_000);
    let pinned = fixture.attribution.collection();

    // A newer collection is published after the status pinned its own.
    publish_gpu_processes(&publisher, 5_000, 6_000);
    let status = fixture
        .status
        .status_section(&pinned)
        .expect("node model status");

    assert_eq!(pinned.revision(), 1);
    assert_eq!(
        attributed_bytes(&status),
        [
            ("model-one".to_owned(), 1_000),
            ("model-two".to_owned(), 2_000)
        ]
    );
    assert_eq!(
        attributed_bytes(&fixture.status()),
        [
            ("model-one".to_owned(), 5_000),
            ("model-two".to_owned(), 6_000)
        ],
        "the next status pins the newer collection"
    );
}

#[tokio::test]
async fn native_runtime_memory_reaches_the_metric_store_as_deployment_history() {
    let clock = VirtualClock::starting_at(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let (publisher, inventory) = HardwareInventoryPublisher::new();
    let attribution = AcceleratorAttribution::new(Arc::new(inventory.clone()))
        .with_process_memory(Arc::new(CurrentMemoryPort(clock.clone())));
    let fixture = StatusFixture::with_attribution(
        vec![assigned(
            "model-one",
            ModelDeploymentDesiredState::Present,
            2,
        )],
        native_target(),
        attribution.clone(),
    );
    fixture.record("model-one", ModelDeploymentStatus::Ready, Some(2));
    attribution.observe(
        "model-one",
        Some(&native_target()),
        &dotcore_runtime::WorkloadObservation::Running(workload(
            "worker-one",
            "workload-one",
            41,
            700,
        )),
    );
    // The node reads runtime memory with its collection, before publishing it.
    attribution.read(inventory.next_revision()).await;
    publish_apple_gpu(&publisher);
    let store = InMemoryMetricStore::start(
        InMemoryStoreConfig::default(),
        [
            forwarder_metric_descriptors(),
            deployment_metric_descriptors(Duration::from_secs(5)),
        ]
        .concat(),
        Arc::new(clock.clone()),
    )
    .expect("start metric store");
    let ingest = Arc::new(MetricBucketIngest::new(
        store.ingestor(),
        Arc::new(clock.clone()),
    ));
    let (forwarder, _forwarder_task) = MetricForwarder::start(
        "worker-one",
        Arc::new(InProcessBucketTransport::new(ingest)),
        Arc::new(clock.clone()),
        ForwarderConfig::default(),
    )
    .expect("start metric forwarder");
    // The node records the status's attribution once for the published collection.
    forwarder
        .recorder(DEPLOYMENT_MEMORY_SOURCE)
        .record(deployment_memory_records(
            "worker-one",
            fixture.status.as_ref(),
            clock.wall_now(),
        ));

    let series = stored_deployment_memory(&store, &clock).await;

    assert_eq!(
        series.labels,
        LabelSet::new()
            .with("deployment", "model-one")
            .with("node", "worker-one")
            .with("device", "apple-gpu-0")
    );
    assert!(
        series
            .points
            .iter()
            .all(|point| point.value == PointValue::Gauge(NATIVE_MEMORY_BYTES as f64)),
        "every second holds the attributed bytes: {series:?}"
    );
    assert_eq!(forwarder.stale_records(), 0);
}

/// Node model status over one installed view, real retained node state and a live serving
/// view the test publishes.
struct StatusFixture {
    directory: tempfile::TempDir,
    authority: AcceptedNodeViewAuthority,
    state: FileNodeState,
    serving_targets: LocalServingTargetsPublisher,
    source: LocalServingTargetsSource,
    attribution: AcceleratorAttribution,
    status: Arc<NodeModelStatus>,
}

impl StatusFixture {
    fn new(deployments: Vec<ModelDeploymentDeclaration>) -> Self {
        Self::with_attribution(
            deployments,
            model_target(),
            AcceleratorAttribution::new(Arc::new(HardwareInventoryPublisher::new().1)),
        )
    }

    /// A fixture whose single installed target is `target`, attributing memory through
    /// `attribution`.
    fn with_attribution(
        deployments: Vec<ModelDeploymentDeclaration>,
        target: ExecutionTarget,
        attribution: AcceleratorAttribution,
    ) -> Self {
        let authority = AcceptedNodeViewAuthority::new(
            "site-one",
            "worker-one",
            NodeIncarnation::parse("1".repeat(32)).expect("node incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("manager session"),
            Duration::from_secs(30),
        )
        .expect("node view authority");
        let projection =
            NodeViewProjection::new(vec![target], deployments).expect("node projection");
        authority
            .install_for_test("site-one", "worker-one", 1, &projection)
            .expect("installed node view");
        let directory =
            dotcore_domain::test_support::canonical_tempdir().expect("node state directory");
        let state = open_node_state(&directory);
        let (serving_targets, source) = LocalServingTargetsPublisher::new();
        let status = node_status(&authority, &state, &source, &attribution);
        Self {
            directory,
            authority,
            state,
            serving_targets,
            source,
            attribution,
            status,
        }
    }

    /// Releases the retained node state and decodes it again from its node store.
    fn reopened(self) -> Self {
        let Self {
            directory,
            authority,
            state,
            serving_targets,
            source,
            attribution,
            status,
        } = self;
        drop(status);
        drop(state);
        let state = open_node_state(&directory);
        let status = node_status(&authority, &state, &source, &attribution);
        Self {
            directory,
            authority,
            state,
            serving_targets,
            source,
            attribution,
            status,
        }
    }

    /// Publishes `targets` as the node's complete live local serving view.
    fn serve(&self, targets: Vec<LocalServingTarget>) {
        self.serving_targets.publish(targets);
    }

    fn record(
        &self,
        id: &str,
        status: ModelDeploymentStatus,
        reconciled_lifecycle_generation: Option<u64>,
    ) {
        let existing = self
            .state
            .snapshot()
            .expect("node state")
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == id)
            .map(|record| record.record_revision);
        self.state
            .upsert(
                CurrentModelDeployment {
                    model_deployment_id: id.to_owned(),
                    reconciled_lifecycle_generation,
                    state: Some(ModelDeploymentState {
                        status: status as i32,
                        status_message: status.as_str_name().to_owned(),
                        observed_placements: (status == ModelDeploymentStatus::Ready)
                            .then(|| ObservedPlacement {
                                managed_node_id: "worker-one".to_owned(),
                                accelerator_device_ids: Vec::new(),
                            })
                            .into_iter()
                            .collect(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                existing,
            )
            .expect("node current record");
    }

    /// The status section built from the node's current collection.
    fn status(&self) -> ModelControllerStatusSection {
        self.status
            .status_section(&self.attribution.collection())
            .expect("node model status")
    }
}

fn open_node_state(directory: &tempfile::TempDir) -> FileNodeState {
    FileNodeState::open(directory.path().join("node"), "site-one", "worker-one")
        .expect("node current state")
}

fn node_status(
    authority: &AcceptedNodeViewAuthority,
    state: &FileNodeState,
    source: &LocalServingTargetsSource,
    attribution: &AcceleratorAttribution,
) -> Arc<NodeModelStatus> {
    Arc::new(NodeModelStatus::new(
        "worker-one",
        authority.model_input(),
        state.clone(),
        source.clone(),
        attribution.clone(),
    ))
}

/// One status report carrying `model` for the installed view.
fn report(model: ModelControllerStatusSection) -> NodeControllerStatus {
    NodeControllerStatus {
        view: Some(NodeViewReference {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "f".repeat(32),
            view_revision: 1,
            content_sha256: "b".repeat(64),
        }),
        status_sequence: 1,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(model)),
        }],
    }
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
        accelerator_device_ids: vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()],
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                    SimulatedRuntimeAdapter::default(),
                )),
            },
        )),
    }
}

/// Process memory the native runtime reports for every workload in the native scenario.
const NATIVE_MEMORY_BYTES: u64 = 4_096;

/// A runtime that reports [`NATIVE_MEMORY_BYTES`] observed at the clock's current time.
struct CurrentMemoryPort(VirtualClock);

impl ProcessMemoryPort for CurrentMemoryPort {
    fn observe<'a>(
        &'a self,
        _execution_target_id: &'a str,
        _workload_id: &'a dotcore_runtime::WorkloadId,
        _expected_process: Option<&'a dotcore_domain::host_process::HostProcessIdentity>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuntimeMemoryReading> + Send + 'a>>
    {
        Box::pin(std::future::ready(RuntimeMemoryReading::Available {
            bytes: NATIVE_MEMORY_BYTES,
            observed_at: self.0.wall_now(),
        }))
    }
}

/// Advances `clock` until the store holds deployment memory history, then returns its only
/// series.
async fn stored_deployment_memory(
    store: &InMemoryMetricStore,
    clock: &VirtualClock,
) -> ResultSeries {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        clock.advance(Duration::from_millis(250));
        tokio::time::sleep(Duration::from_millis(5)).await;
        let result = store
            .query(MetricQuery {
                metric: DEPLOYMENT_ACCELERATOR_MEMORY_USED.to_owned(),
                matchers: BTreeMap::new(),
                group_by: vec![
                    "deployment".to_owned(),
                    "node".to_owned(),
                    "device".to_owned(),
                ],
                reduce: Some(Reduce::Last),
                aggregate: Some(SeriesAggregate::Sum),
                percentiles: vec![],
                range: QueryRange::Last(Duration::from_secs(60)),
                step_hint: Duration::from_secs(1),
            })
            .await
            .expect("deployment memory query");
        if result.series.iter().any(|series| !series.points.is_empty()) {
            assert_eq!(result.series.len(), 1, "one series: {result:?}");
            return result.series.into_iter().next().expect("one series");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "deployment memory history did not reach the store: {result:?}"
        );
    }
}

/// Publishes a collection in which processes 41 and 42 of `worker-one` hold `first` and
/// `second` bytes on one GPU.
fn publish_gpu_processes(publisher: &HardwareInventoryPublisher, first: u64, second: u64) {
    publisher.publish(
        &dotcore_domain::SiteNodeId::for_test("worker-one"),
        HardwareInventoryEntry::succeeded(
            NodeHardwareObservation {
                snapshot: HardwareSnapshot::default(),
                accelerator_processes: observed_processes(vec![
                    process("GPU-a", 41, 700, Some(first)),
                    process("GPU-a", 42, 700, Some(second)),
                ]),
            },
            dotcore_proto::google::protobuf::Timestamp::default(),
        ),
    );
}

/// The accelerator bytes each reported deployment was attributed, in report order.
fn attributed_bytes(status: &ModelControllerStatusSection) -> Vec<(String, u64)> {
    status
        .deployments
        .iter()
        .map(|deployment| {
            let bytes = match deployment
                .accelerator_memory
                .as_ref()
                .and_then(|memory| memory.result.as_ref())
            {
                Some(dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::Available(usage)) => usage
                    .devices
                    .iter()
                    .map(|device| device.used_memory_bytes)
                    .sum(),
                other => panic!("expected attributed memory, got {other:?}"),
            };
            (deployment.model_deployment_id.clone(), bytes)
        })
        .collect()
}

/// Publishes one Apple GPU in the node's hardware inventory.
fn publish_apple_gpu(publisher: &HardwareInventoryPublisher) {
    publisher.publish(
        &dotcore_domain::SiteNodeId::for_test("worker-one"),
        HardwareInventoryEntry::succeeded(
            NodeHardwareObservation {
                snapshot: HardwareSnapshot {
                    accelerators: vec![AcceleratorObservation {
                        vendor: AcceleratorVendor::Apple as i32,
                        result: Some(accelerator_observation::Result::Facts(
                            AcceleratorFactsSet {
                                facts: vec![AcceleratorFacts {
                                    vendor: AcceleratorVendor::Apple as i32,
                                    device_id: "apple-gpu-0".to_owned(),
                                    vendor_index: Some(0),
                                    ..Default::default()
                                }],
                            },
                        )),
                    }],
                    ..Default::default()
                },
                accelerator_processes: observed_processes(Vec::new()),
            },
            dotcore_proto::google::protobuf::Timestamp::default(),
        ),
    );
}

/// The installed native target, on the node's Apple GPU.
fn native_target() -> ExecutionTarget {
    ExecutionTarget {
        platform: Some(OciPlatform {
            operating_system: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: vec!["apple-gpu-0".to_owned()],
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                    Default::default(),
                )),
            },
        )),
        ..model_target()
    }
}

fn assigned(
    id: &str,
    desired_state: ModelDeploymentDesiredState,
    generation: u64,
) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: id.to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: "worker-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        }),
        desired_state: desired_state as i32,
        lifecycle_generation: generation,
        lifecycle_transition: match desired_state {
            ModelDeploymentDesiredState::Paused => ModelDeploymentLifecycleTransition::Pause,
            ModelDeploymentDesiredState::Teardown => ModelDeploymentLifecycleTransition::Teardown,
            _ => ModelDeploymentLifecycleTransition::Create,
        } as i32,
        snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
            id,
        )),
        ..Default::default()
    }
}

/// Builds a live local target for `id` bound to a loopback engine endpoint, so a status that
/// leaked addressing would differ from the exact expected value.
fn live_target(
    id: &str,
    lifecycle_generation: u64,
    reconcile_generation: u64,
    binding_revision: u64,
) -> LocalServingTarget {
    let binding = crate::model::controller::test_support::test_engine_binding_for_values(
        id,
        &format!("{id}-realization"),
        "fixture-model",
        binding_revision,
        Some(dotcore_runtime::LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: 47_123,
        }),
    );
    let mut target = local_target_for_test(id, None);
    target.lifecycle_generation = lifecycle_generation;
    target.reconcile_generation = reconcile_generation;
    target.binding_identity = binding.identity().clone();
    target.binding = binding;
    target
}

/// Independent rendering of the binding-token contract: domain-separated SHA-256 over the
/// length-prefixed binding identity, its revision, and the served lifecycle generation.
fn expected_binding_token(
    id: &str,
    realization_id: &str,
    binding_revision: u64,
    lifecycle_generation: u64,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"dotcore.model-binding-token.v1\0");
    for value in [id, realization_id, "http"] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(binding_revision.to_be_bytes());
    hasher.update(lifecycle_generation.to_be_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Node status for a reconciled local record with `status`, `message`, and optional byte
/// progress as (completed, total).
fn local_status(
    status: ModelDeploymentStatus,
    message: &str,
    progress: Option<(u64, Option<u64>)>,
) -> PrivateModelDeploymentStatus {
    let declaration = assigned("model-one", ModelDeploymentDesiredState::Present, 2);
    let record = CurrentModelDeployment {
        model_deployment_id: "model-one".to_owned(),
        reconciled_lifecycle_generation: Some(2),
        record_revision: 1,
        state: Some(ModelDeploymentState {
            status: status as i32,
            status_message: message.to_owned(),
            progress: progress.map(|(completed, total)| ModelDeploymentProgress {
                percent: None,
                completed,
                total,
                unit: ModelDeploymentProgressUnit::Bytes as i32,
            }),
            failure: (status == ModelDeploymentStatus::Failed).then(|| ModelDeploymentFailure {
                code: "engine_startup_failed".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        recovery_barrier: None,
    };
    deployment_status("worker-one", &declaration, Some(&record), None)
}

fn not_applicable_memory() -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        result: Some(
            dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation::Result::NotApplicable(
                dotcore_proto::google::protobuf::Empty {},
            ),
        ),
        ..Default::default()
    }
}
