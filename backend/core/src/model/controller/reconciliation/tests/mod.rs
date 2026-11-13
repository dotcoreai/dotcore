mod acquisition_contract;

use std::{
    collections::BTreeMap,
    future::ready,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::anyhow;
use dotcore_artifact::{
    ArtifactManager, BlockingWorkGate, ModelArtifactCache, ModelArtifactProgress,
};
use dotcore_domain::{
    clock::SystemClock,
    test_support::tracing::{Record, capture_records, capture_records_async},
    timing::Timing,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        HuggingFaceSnapshotArtifact, HuggingFaceSnapshotFile,
        hugging_face_artifact_source::Content as HuggingFaceContent,
    },
    model::v1::{
        ModelDeploymentDesiredState, ModelDeploymentLifecycleTransition, ModelDeploymentSnapshot,
        ModelDeploymentStatus, model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
    site::v1::{ModelRuntimeBinding, execution_target, model_runtime_binding},
};
use dotcore_runtime::{
    LoopbackEndpoint, ObservedWorkload, RuntimeBackend, RuntimeBackendId, RuntimeFuture,
    WorkloadCorrelation, WorkloadId, WorkloadLaunch, WorkloadObservation,
};
use dotcore_runtime_backends::simulated::{
    InMemoryRuntimeBackend, SimulatedRuntimeConfig, SimulatedRuntimeTestSupport,
    SimulatedStartupConfig,
};
use dotcore_site::{
    repository::ModelSiteSnapshot,
    store::FileNodeState,
    view::fencing::{
        AcceptedNodeViewAuthority, LifecycleOperationAttempt, ManagerSession, NodeIncarnation,
        NodeViewProjection, RuntimeActionAuthority,
    },
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Notify},
    time::Instant,
};
use tracing::{debug, info, info_span};

use super::{
    DownloadProgressKind, DownloadProgressPublication, ModelLifecycleError,
    ObservedPreparationProgress, ReconcileOutcome, emit_observer_failure, paused_initial_state,
    replace_stale_workload, runtime_startup_progress_state,
};
use crate::{
    dispatch::model::{RuntimeTargetMapBuilder, TargetObservation},
    model::{
        controller::{
            ArtifactDependencies, DesiredDeploymentRepository, InferenceControllerInner,
            ModelDeploymentExposureIntent, ModelRealizationInput,
            launch::{resolved_launch, selected_target, workload_launch},
            preparation::PreparationCoordinator,
            serving_targets::serving_targets,
            test_support::{
                CountingRuntime, RecordingRuntime, TestCurrent, TestDesired, TestFailure,
                current_deployment, test_desired_deployment, test_placement, test_runtime_error,
                test_runtime_target, test_site,
            },
        },
        state::CurrentStateRepository,
    },
};

static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Returns the node realizer a test drives directly; its test input also confirms cleanup.
fn manager_test_controller(
    _desired: TestDesired,
    realizer: super::super::ModelRealizer<TestDesired, TestCurrent, TestDesired>,
) -> super::super::test_support::TestRealizer {
    realizer
}

#[test]
fn startup_log_protection_selects_sensitive_environment_values() {
    let environment = BTreeMap::from([
        (
            "DOTCORE_API_KEY".to_owned(),
            dotcore_runtime::WorkloadValue::literal("sk-test-private"),
        ),
        (
            "THREADS".to_owned(),
            dotcore_runtime::WorkloadValue::literal("16"),
        ),
    ]);
    assert_eq!(
        super::protected_startup_values(&environment).values(),
        &[b"sk-test-private".to_vec()]
    );
}

#[test]
fn startup_log_protection_selects_literal_parts_of_symbolic_sensitive_values() {
    let environment = BTreeMap::from([
        (
            "UPSTREAM_TOKEN".to_owned(),
            dotcore_runtime::WorkloadValue::parse("sk-secret@{service:http.host}")
                .expect("valid template"),
        ),
        (
            "LISTEN".to_owned(),
            dotcore_runtime::WorkloadValue::parse("{service:http.port}").expect("valid template"),
        ),
    ]);
    assert_eq!(
        super::protected_startup_values(&environment).values(),
        &[b"sk-secret@".to_vec()]
    );
}

#[test]
fn definitive_realization_failures_are_only_capability_and_contract_mismatches() {
    use dotcore_runtime::RuntimeBackendError;

    use crate::dispatch::model::RuntimeTargetError;

    let adapter = |source: RuntimeBackendError| {
        ModelLifecycleError::Runtime(RuntimeTargetError::Adapter {
            execution_target_id: "target-one".into(),
            source,
        })
    };

    // Capability and contract mismatches are terminal for the deployment.
    assert!(super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::HostSharedIpcUnavailable
    )));
    assert!(super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::ExecutionPlatformUnavailable
    )));
    assert!(super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::LogCapabilityUnavailable
    )));
    assert!(super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::UnsupportedExecutableArtifact {
            backend_id: "runtime/native",
            kind: dotcore_domain::WorkloadArtifactKind::Oci,
        }
    )));
    assert!(super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::TargetPlatformMismatch {
            required_operating_system: "linux".to_owned(),
            required_architecture: "x86_64".to_owned(),
            observed_operating_system: "darwin".to_owned(),
            observed_architecture: "aarch64".to_owned(),
        }
    )));

    // Image acquisition, runtime faults, cancellation, and limits are retryable.
    assert!(!super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::PreparationCancelled
    )));
    assert!(!super::is_definitive_realization_failure(&adapter(
        RuntimeBackendError::LogSourceLimitReached
    )));
    assert!(!super::is_definitive_realization_failure(
        &ModelLifecycleError::Runtime(RuntimeTargetError::MissingTarget("target-one".to_owned(),))
    ));
    assert!(!super::is_definitive_realization_failure(
        &ModelLifecycleError::InvalidArgument
    ));
}

#[tokio::test]
async fn existing_teardown_precedes_later_missing_initialization_without_replacing_current_state() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut existing =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    existing.model_deployment_id = "a-existing".to_owned();
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments = vec![existing.clone()];

    let current = TestCurrent::new(calls);
    let mut existing_current = current_deployment("a-existing");
    let existing_state = existing_current.state.as_mut().expect("existing state");
    existing_state.status = ModelDeploymentStatus::Ready as i32;
    existing_state.status_message = "Ready".to_owned();
    current
        .upsert(existing_current, None)
        .expect("existing current state");
    let cache = crate::test_support::canonical_tempdir().expect("artifact cache parent");
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64()),
        )
        .expect("simulated runtime");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        runtimes.build().expect("Runtime map"),
        Vec::new(),
        cache.path().join("cache"),
        None,
    )
    .expect("controller construction");
    let existing_before = current
        .snapshot()
        .expect("state before replacement")
        .model_deployments[0]
        .clone();

    let mut teardown = existing;
    teardown.desired_state = ModelDeploymentDesiredState::Teardown as i32;
    teardown.lifecycle_transition = ModelDeploymentLifecycleTransition::Teardown as i32;
    let mut missing =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    missing.model_deployment_id = "b-new".to_owned();
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments = vec![teardown, missing];

    controller
        .inner
        .initialize_missing_current_records()
        .await
        .expect("missing-state initialization");

    let initialized = current.snapshot().expect("initialized current state");
    assert_eq!(initialized.model_deployments.len(), 2);
    assert_eq!(initialized.model_deployments[0], existing_before);
    assert_eq!(
        initialized.model_deployments[1].model_deployment_id,
        "b-new"
    );
    assert_eq!(
        initialized.model_deployments[1]
            .state
            .as_ref()
            .expect("new state")
            .status,
        ModelDeploymentStatus::Downloading as i32
    );
    assert_eq!(
        initialized.model_deployments[1]
            .state
            .as_ref()
            .expect("new state")
            .status_message,
        "Preparing model realization"
    );

    controller
        .inner
        .reconcile()
        .await
        .expect("mixed reconciliation");
    controller
        .inner
        .reconcile()
        .await
        .expect("ready reconciliation");

    let records = current
        .snapshot()
        .expect("settled current state")
        .model_deployments;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].model_deployment_id, "b-new");
    assert_eq!(
        records[0].state.as_ref().expect("ready state").status,
        ModelDeploymentStatus::Ready as i32
    );
    assert_eq!(
        desired
            .model_deployments()
            .expect("remaining desired deployments")
            .iter()
            .map(|deployment| deployment.model_deployment_id.as_str())
            .collect::<Vec<_>>(),
        ["b-new"]
    );
}

#[tokio::test]
async fn recovery_required_intent_never_prepares_or_mutates_runtime() {
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-recovery-barrier-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    deployment.lifecycle_recovery_required = true;
    desired
        .state
        .lock()
        .expect("desired site")
        .model_deployments = vec![deployment.clone()];
    let current = TestCurrent::new(calls.clone());
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            RecordingRuntime {
                calls: calls.clone(),
                virtualized: true,
            },
        )
        .expect("Runtime adapter");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        runtimes.build().expect("Runtime map"),
        Vec::new(),
        cache_root.clone(),
        None,
    )
    .expect("controller construction");
    calls.lock().expect("calls").clear();

    controller.inner.reconcile().await.expect("reconciliation");

    assert!(runtime_calls(&calls).is_empty());
    let record = current
        .snapshot()
        .expect("current state")
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == deployment.model_deployment_id)
        .expect("recovery state");
    assert_eq!(
        record
            .recovery_barrier
            .expect("durable recovery barrier")
            .lifecycle_generation,
        deployment.lifecycle_generation
    );
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn realization_failure_records_terminal_state_with_reason() {
    use dotcore_runtime::RuntimeBackendError;

    use crate::dispatch::model::RuntimeTargetError;

    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-realization-failure-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let current = TestCurrent::new(calls.clone());
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            RecordingRuntime {
                calls: calls.clone(),
                virtualized: true,
            },
        )
        .expect("runtime adapter");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        runtimes.build().expect("runtime map"),
        Vec::new(),
        cache_root.clone(),
        None,
    )
    .expect("controller construction");

    let deployment = test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    let error = ModelLifecycleError::Runtime(RuntimeTargetError::Adapter {
        execution_target_id: "target-one".into(),
        source: RuntimeBackendError::HostSharedIpcUnavailable,
    });
    let expected_reason = RuntimeBackendError::HostSharedIpcUnavailable.to_string();
    controller
        .inner
        .record_realization_failure(
            &deployment,
            &error,
            &crate::management_logs::ProtectedLogValues::default(),
        )
        .await
        .expect("record failure");

    let record = current
        .snapshot()
        .expect("current state")
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == deployment.model_deployment_id)
        .expect("deployment recorded");
    let state = record.state.expect("recorded state");
    assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
    assert_eq!(state.status_message, expected_reason);
    assert!(state.activation.is_none());
    let failure = state.failure.expect("failure recorded");
    assert_eq!(failure.code, "runtime_realization_failed");
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|entry| entry == &expected_reason)
    );
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn a_target_capability_mismatch_fails_only_its_deployment() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut mismatched =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    mismatched.model_deployment_id = "a-mismatched".to_owned();
    let Some(ModelDeploymentSnapshotSource::Local(local)) = mismatched
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    else {
        panic!("local snapshot");
    };
    local
        .engine_artifact
        .as_mut()
        .expect("snapshot engine")
        .minimum_nvidia_driver_major_version = 999;
    let mut fitting =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    fitting.model_deployment_id = "b-fitting".to_owned();
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments = vec![mismatched, fitting];
    let current = TestCurrent::new(calls);
    let cache = crate::test_support::canonical_tempdir().expect("artifact cache parent");
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64()),
        )
        .expect("simulated runtime");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired,
        current.clone(),
        runtimes.build().expect("Runtime map"),
        Vec::new(),
        cache.path().join("cache"),
        None,
    )
    .expect("controller construction");
    let state_of = |deployment_id: &str| {
        current
            .snapshot()
            .expect("current state")
            .model_deployments
            .into_iter()
            .find(|record| record.model_deployment_id == deployment_id)
            .and_then(|record| record.state)
    };
    let status_of = |deployment_id: &str| {
        state_of(deployment_id).and_then(|state| ModelDeploymentStatus::try_from(state.status).ok())
    };

    for _ in 0..20 {
        controller
            .inner
            .reconcile()
            .await
            .expect("reconciliation pass");
        if status_of("b-fitting") == Some(ModelDeploymentStatus::Ready) {
            break;
        }
    }

    assert_eq!(status_of("b-fitting"), Some(ModelDeploymentStatus::Ready));
    let failed = state_of("a-mismatched").expect("mismatched state");
    assert_eq!(failed.status, ModelDeploymentStatus::Failed as i32);
    assert_eq!(
        failed.failure,
        Some(dotcore_proto::dotcore::model::v1::ModelDeploymentFailure {
            code: "target_capability_mismatch".to_owned(),
            message: "Execution target \"target-one\" cannot run this deployment: target NVIDIA driver 580 is below retained engine artifact minimum 999; pause and resume the deployment once the target supports it, or tear it down".to_owned(),
            diagnostics: Vec::new(),
        })
    );
}

#[tokio::test]
async fn protected_launch_values_never_leave_the_node_in_a_realization_failure() {
    use dotcore_runtime::WorkloadValue;
    use prost::Message as _;

    use crate::management_logs::ProtectedLogValues;

    const SECRET: &str = "synthetic-protected-launch-token";
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-realization-failure-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let current = TestCurrent::new(calls.clone());
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            RecordingRuntime {
                calls: calls.clone(),
                virtualized: true,
            },
        )
        .expect("runtime adapter");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        TestDesired::new(calls.clone()),
        current.clone(),
        runtimes.build().expect("runtime map"),
        Vec::new(),
        cache_root.clone(),
        None,
    )
    .expect("controller construction");
    let deployment = test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    let environment = BTreeMap::from([
        (
            "HF_TOKEN".to_owned(),
            WorkloadValue::literal(SECRET.to_owned()),
        ),
        (
            "MODEL_NAME".to_owned(),
            WorkloadValue::literal("fixture-model".to_owned()),
        ),
    ]);
    let protected: ProtectedLogValues = super::protected_startup_values(&environment);
    let error = ModelLifecycleError::configuration(format!(
        "engine rejected HF_TOKEN={SECRET} for fixture-model"
    ));

    controller
        .inner
        .record_realization_failure(&deployment, &error, &protected)
        .await
        .expect("record failure");
    let record = current
        .snapshot()
        .expect("current state")
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == deployment.model_deployment_id)
        .expect("deployment recorded");
    let node_id = &deployment
        .placement
        .as_ref()
        .expect("placement")
        .site_node_id;
    let status =
        crate::model::controller::deployment_status(node_id, &deployment, Some(&record), None);

    let failure = status.failure.as_ref().expect("reported failure");
    assert_eq!(
        failure.message,
        "Model lifecycle configuration failed: engine rejected HF_TOKEN=[REDACTED] for \
             fixture-model"
    );
    assert_eq!(status.status_message, failure.message);
    assert!(
        failure
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.contains("HF_TOKEN=[REDACTED] for fixture-model"))
    );
    assert!(
        !status
            .encode_to_vec()
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    );
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}
#[test]
fn startup_output_tail_masks_secrets_and_paths_before_bounding_the_last_bytes() {
    use dotcore_runtime::{
        RuntimeLogEntry, RuntimeLogHistory, RuntimeLogSource, RuntimeLogStreamKind,
    };

    use crate::management_logs::ProtectedLogValues;
    let source = RuntimeLogSource {
        name: "model".to_owned(),
        label: "Model".to_owned(),
    };
    let entry = |content: Vec<u8>| RuntimeLogEntry {
        source: source.clone(),
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        stream: RuntimeLogStreamKind::Stderr,
        content,
        truncated: false,
    };
    let history = RuntimeLogHistory {
            entries: vec![
                entry(b"booting".to_vec()),
                entry(b"File \"/opt/dotcore/models/snapshot/model_mtp.safetensors\" key=sk-test-private FileNotFoundError".to_vec()),
            ],
            has_older_output: false,
            candidate_budget_truncated: false,
        };
    let output = super::startup_output(
        history,
        &ProtectedLogValues::new(["sk-test-private".to_owned()]),
        &|_| None,
    );
    assert_eq!(
        output,
        super::StartupOutput {
            tail: Some("booting\nFile \"[REDACTED]\" key=[REDACTED] FileNotFoundError".to_owned()),
            refusal: None,
        }
    );

    let history = RuntimeLogHistory {
        entries: vec![entry(
            format!("{}\nlast failure", "x".repeat(9000)).into_bytes(),
        )],
        has_older_output: true,
        candidate_budget_truncated: false,
    };
    let tail = super::startup_output(history, &ProtectedLogValues::default(), &|_| None)
        .tail
        .expect("bounded output");
    assert!(tail.len() <= 4096);
    assert!(tail.ends_with("last failure"));
}

#[test]
fn startup_output_recognizes_an_engine_refusal_older_than_the_tail() {
    use dotcore_proto::dotcore::engine::v1::EngineKind;
    use dotcore_runtime::{
        RuntimeLogEntry, RuntimeLogHistory, RuntimeLogSource, RuntimeLogStreamKind,
    };

    use crate::{
        management_logs::ProtectedLogValues, model::engine::startup_refusal::StartupRefusal,
    };
    const REFUSAL: &str = "(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080] ValueError: Free memory on device cuda:0 (111.66/121.63 GiB) on startup is less than desired GPU memory utilization (0.97, 117.98 GiB). Decrease GPU memory utilization or reduce GPU memory used by other processes.";
    let later = (0..20)
        .map(|frame| format!("(APIServer pid=1)   frame {frame}, in run_engine_core"))
        .chain(["(APIServer pid=1) RuntimeError: Engine core initialization failed. See root cause above. Failed core proc(s): {}".to_owned()])
        .collect::<Vec<_>>();
    let history = RuntimeLogHistory {
        entries: std::iter::once(REFUSAL.to_owned())
            .chain(later.iter().cloned())
            .map(|line| RuntimeLogEntry {
                source: RuntimeLogSource {
                    name: "model".to_owned(),
                    label: "Model".to_owned(),
                },
                timestamp: std::time::SystemTime::UNIX_EPOCH,
                stream: RuntimeLogStreamKind::Stderr,
                content: line.into_bytes(),
                truncated: false,
            })
            .collect(),
        has_older_output: false,
        candidate_budget_truncated: false,
    };
    let integrations = crate::model::controller::test_support::test_engine_integrations();
    let vllm = integrations
        .integration(EngineKind::Vllm)
        .expect("vLLM integration");

    let output = super::startup_output(history, &ProtectedLogValues::default(), &|output| {
        vllm.startup_refusal(output)
    });

    assert_eq!(
        output,
        super::StartupOutput {
            tail: Some(later[later.len() - 16..].join("\n")),
            refusal: Some(StartupRefusal::InsufficientFreeMemory {
                needed_bytes: 126_680_060_396,
                free_bytes: 119_894_012_068,
                line: REFUSAL.to_owned(),
            }),
        }
    );
}
#[tokio::test(start_paused = true)]
async fn download_progress_publication_writes_first_changed_interval_and_final_snapshots() {
    let declaration =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    let coordinator = PreparationCoordinator::from_deployments(std::slice::from_ref(&declaration))
        .expect("preparation coordinator");
    let lease = coordinator
        .entry(&declaration.model_deployment_id)
        .register(declaration)
        .expect("preparation lease");
    let mut publication = DownloadProgressPublication::new(
        lease.ticket(),
        DownloadProgressKind::Artifact,
        Timing::test().model,
    );
    let started = tokio::time::Instant::now();
    let first = ObservedPreparationProgress::artifact(ModelArtifactProgress {
        completed: 10,
        total: Some(100),
    });
    publication.latest = Some(first.clone());
    assert_eq!(
        publication
            .pending_write(started, false)
            .map(|(_, progress)| progress),
        Some(first.clone())
    );
    publication.mark_persisted(first.clone(), started);
    publication.latest = Some(first);
    assert!(publication.pending_write(Instant::now(), false).is_none());

    let changed = ObservedPreparationProgress::artifact(ModelArtifactProgress {
        completed: 40,
        total: Some(100),
    });
    publication.latest = Some(changed.clone());
    tokio::time::advance(Timing::test().model.download_progress_interval / 2).await;
    assert!(publication.pending_write(Instant::now(), false).is_none());
    assert_eq!(
        publication
            .pending_write(Instant::now(), true)
            .map(|(_, progress)| progress),
        Some(changed.clone())
    );
    tokio::time::advance(Timing::test().model.download_progress_interval / 2).await;
    assert_eq!(
        publication
            .pending_write(Instant::now(), false)
            .map(|(_, progress)| progress),
        Some(changed)
    );
}

#[tokio::test]
async fn download_progress_observer_coalesces_a_flood_to_one_latest_snapshot() {
    let declaration =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    let coordinator = PreparationCoordinator::from_deployments(std::slice::from_ref(&declaration))
        .expect("preparation coordinator");
    let lease = coordinator
        .entry(&declaration.model_deployment_id)
        .register(declaration)
        .expect("preparation lease");
    let mut publication = DownloadProgressPublication::new(
        lease.ticket(),
        DownloadProgressKind::Artifact,
        Timing::test().model,
    );
    let notification = Arc::new(Notify::new());
    let observer = publication.observer(notification.clone());

    for completed in 0..10_000 {
        observer(ModelArtifactProgress {
            completed,
            total: Some(10_000),
        });
    }

    tokio::time::timeout(Duration::from_millis(100), notification.notified())
        .await
        .expect("one coalesced notification");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), notification.notified())
            .await
            .is_err()
    );
    publication.collect_observed();
    assert_eq!(
        publication.latest,
        Some(ObservedPreparationProgress::artifact(
            ModelArtifactProgress {
                completed: 9_999,
                total: Some(10_000),
            }
        ))
    );
}
#[tokio::test]
async fn artifact_stage_observer_retains_transfer_order_across_transitions() {
    let declaration =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    let coordinator = PreparationCoordinator::from_deployments(std::slice::from_ref(&declaration))
        .expect("preparation coordinator");
    let lease = coordinator
        .entry(&declaration.model_deployment_id)
        .register(declaration)
        .expect("preparation lease");
    let mut publication = DownloadProgressPublication::new(
        lease.ticket(),
        DownloadProgressKind::Artifact,
        Timing::test().model,
    );
    let notification = Arc::new(Notify::new());
    let progress = publication.observer(notification.clone());
    let stage = publication.stage_observer(notification);
    progress(ModelArtifactProgress {
        completed: 20,
        total: Some(100),
    });
    stage(dotcore_artifact::ModelArtifactStage::Verify);
    progress(ModelArtifactProgress {
        completed: 50,
        total: Some(100),
    });
    stage(dotcore_artifact::ModelArtifactStage::Publish);
    progress(ModelArtifactProgress {
        completed: 75,
        total: Some(100),
    });
    publication.collect_observed();

    assert_eq!(publication.pending_stages.len(), 2);
    assert_eq!(
        publication.pending_stages[0].progress_before,
        Some(ObservedPreparationProgress::artifact(
            ModelArtifactProgress {
                completed: 20,
                total: Some(100),
            }
        ))
    );
    assert_eq!(
        publication.pending_stages[0].stage,
        dotcore_artifact::ModelArtifactStage::Verify
    );
    assert_eq!(
        publication.pending_stages[1].progress_before,
        Some(ObservedPreparationProgress::artifact(
            ModelArtifactProgress {
                completed: 50,
                total: Some(100),
            }
        ))
    );
    assert_eq!(
        publication.pending_stages[1].stage,
        dotcore_artifact::ModelArtifactStage::Publish
    );
    assert_eq!(
        publication.pending_trailing,
        Some(ObservedPreparationProgress::artifact(
            ModelArtifactProgress {
                completed: 75,
                total: Some(100),
            }
        ))
    );
}

struct OwnedOrphanRuntime {
    calls: Arc<Mutex<Vec<&'static str>>>,
    orphan: ObservedWorkload,
}

struct ReplacingStoppedRuntime {
    calls: Arc<Mutex<Vec<&'static str>>>,
    stopped: ObservedWorkload,
    created: ObservedWorkload,
}
struct ReplacingMissingRuntime {
    calls: Arc<Mutex<Vec<&'static str>>>,
    created: ObservedWorkload,
}

#[derive(Clone)]
enum StartupLogBehavior {
    Unavailable,
    History {
        expected_workload_id: WorkloadId,
        history: dotcore_runtime::RuntimeLogHistory,
    },
    Delayed,
}

struct PausedRuntime {
    calls: Arc<Mutex<Vec<&'static str>>>,
    observations: Arc<Mutex<Vec<Vec<WorkloadObservation>>>>,
    start_observation: Option<WorkloadObservation>,
    startup_logs: Arc<Mutex<StartupLogBehavior>>,
    backend_id: RuntimeBackendId,
}

struct GatedSecondRuntime {
    observations: Vec<WorkloadObservation>,
    second_started: Arc<Notify>,
    release_second: Arc<Notify>,
    second_running: ObservedWorkload,
}

#[derive(Clone, Copy)]
enum GatedMutation {
    Create,
    Start,
}

struct GatedMutationRuntime {
    mutation: GatedMutation,
    started: Arc<Notify>,
    release: Arc<Notify>,
    observation: ObservedWorkload,
}

struct FirstReadinessGatedProvider {
    bindings: AtomicUsize,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct FirstReadinessGatedProbe {
    block: bool,
    model_id: String,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[derive(Default)]
struct CountingOciPreparer {
    calls: AtomicUsize,
}

impl dotcore_artifact::OciImagePreparer for CountingOciPreparer {
    fn prepare(
        &self,
        _request: dotcore_artifact::OciPreparationRequest,
        _cancellation: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct GatedOciPreparer {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl dotcore_artifact::OciImagePreparer for GatedOciPreparer {
    fn prepare(
        &self,
        _request: dotcore_artifact::OciPreparationRequest,
        cancellation: tokio_util::sync::CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> dotcore_artifact::OciPreparationFuture {
        let started = self.started.clone();
        let release = self.release.clone();
        Box::pin(async move {
            started.notify_one();
            tokio::select! {
                () = release.notified() => Ok(()),
                () = cancellation.cancelled() => Err(dotcore_artifact::OciPreparationError::Cancelled),
            }
        })
    }
}

impl crate::inference::NativeInferenceProvider for FirstReadinessGatedProvider {
    fn bind(
        &self,
        identity: crate::inference::EngineBindingIdentity,
        expected_model_id: String,
    ) -> Result<crate::inference::EngineBinding, crate::inference::EngineBindingError> {
        let base = crate::inference::simulated::SimulatedNativeProvider::new(
            Timing::test().simulated,
            Arc::new(SystemClock),
        )
        .bind(identity.clone(), expected_model_id.clone())?;
        Ok(crate::inference::EngineBinding::new(
            identity,
            Arc::new(FirstReadinessGatedProbe {
                block: self.bindings.fetch_add(1, Ordering::SeqCst) == 0,
                model_id: expected_model_id,
                started: self.started.clone(),
                release: self.release.clone(),
            }),
            Arc::clone(base.invoker()),
        ))
    }
}

impl crate::inference::EngineReadinessProbe for FirstReadinessGatedProbe {
    fn probe<'a>(
        &'a self,
        cancellation: crate::inference::InferenceCancellation,
    ) -> crate::inference::InferenceFuture<
        'a,
        Result<crate::inference::EngineReadiness, crate::inference::EngineProbeError>,
    > {
        Box::pin(async move {
            if cancellation.reason().is_some() {
                return Err(crate::inference::EngineProbeError::Transient);
            }
            if self.block {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(crate::inference::EngineReadiness::Ready {
                served_model_id: self.model_id.clone(),
            })
        })
    }
}

impl RuntimeBackend for GatedMutationRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(dotcore_runtime_backends::simulated::BACKEND_ID.to_owned())
    }

    fn materialization_requirements(&self) -> dotcore_runtime::WorkloadMaterializationRequirements {
        dotcore_runtime::WorkloadMaterializationRequirements::virtualized()
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            if !matches!(self.mutation, GatedMutation::Create) {
                return Err(test_runtime_error());
            }
            self.started.notify_one();
            self.release.notified().await;
            Ok(WorkloadObservation::Running(self.observation.clone()))
        })
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            if !matches!(self.mutation, GatedMutation::Start) {
                return Err(test_runtime_error());
            }
            self.started.notify_one();
            self.release.notified().await;
            Ok(WorkloadObservation::Running(self.observation.clone()))
        })
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

impl RuntimeBackend for GatedSecondRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(dotcore_runtime_backends::simulated::BACKEND_ID.to_owned())
    }

    fn materialization_requirements(&self) -> dotcore_runtime::WorkloadMaterializationRequirements {
        dotcore_runtime::WorkloadMaterializationRequirements::virtualized()
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(self.observations.clone())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            assert_eq!(workload_id, &self.second_running.workload_id);
            self.second_started.notify_one();
            self.release_second.notified().await;
            Ok(WorkloadObservation::Running(self.second_running.clone()))
        })
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

impl RuntimeBackend for PausedRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        self.backend_id.clone()
    }

    fn materialization_requirements(&self) -> dotcore_runtime::WorkloadMaterializationRequirements {
        if self.backend_id.0 == dotcore_runtime_backends::simulated::BACKEND_ID {
            dotcore_runtime::WorkloadMaterializationRequirements::virtualized()
        } else {
            dotcore_runtime::WorkloadMaterializationRequirements::physical()
        }
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        let mut observations = self.observations.lock().expect("observations");
        let result = if observations.len() > 1 {
            observations.remove(0)
        } else {
            observations.first().cloned().unwrap_or_default()
        };
        Box::pin(ready(Ok(result)))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("create");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("start");
        Box::pin(ready(
            self.start_observation
                .clone()
                .ok_or_else(test_runtime_error),
        ))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("inspect");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("stop");
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("remove");
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn read_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        _query: &'a dotcore_runtime::RuntimeWorkloadLogQuery,
    ) -> dotcore_runtime::RuntimeLogFuture<'a, dotcore_runtime::RuntimeLogHistory> {
        let behavior = self.startup_logs.lock().expect("startup logs").clone();
        Box::pin(async move {
            match behavior {
                StartupLogBehavior::Unavailable => Err(test_runtime_error()),
                StartupLogBehavior::History {
                    expected_workload_id,
                    history,
                } => {
                    assert_eq!(workload_id, &expected_workload_id);
                    Ok(history)
                }
                StartupLogBehavior::Delayed => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Err(test_runtime_error())
                }
            }
        })
    }
}

impl RuntimeBackend for ReplacingMissingRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/docker".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(Vec::new())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("create");
        Box::pin(ready(Ok(WorkloadObservation::Running(
            self.created.clone(),
        ))))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("start");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("stop");
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("remove");
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

impl RuntimeBackend for ReplacingStoppedRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/docker".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(vec![WorkloadObservation::stopped(
            self.stopped.clone(),
        )])))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("create");
        Box::pin(ready(Ok(WorkloadObservation::Running(
            self.created.clone(),
        ))))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.calls.lock().expect("calls").push("start");
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("stop");
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("remove");
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

impl RuntimeBackend for OwnedOrphanRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/test".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(vec![WorkloadObservation::Running(
            self.orphan.clone(),
        )])))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("stop");
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls.lock().expect("calls").push("remove");
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

async fn paused_reconciliation(
    observations: Vec<Vec<WorkloadObservation>>,
    has_current_state: bool,
) -> (bool, Option<i32>, Arc<Mutex<Vec<&'static str>>>) {
    paused_reconciliation_with_integrations(
        observations,
        has_current_state,
        crate::model::controller::test_support::test_engine_integrations(),
    )
    .await
}

async fn paused_reconciliation_with_integrations(
    observations: Vec<Vec<WorkloadObservation>>,
    has_current_state: bool,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
) -> (bool, Option<i32>, Arc<Mutex<Vec<&'static str>>>) {
    let (reconciled, record, calls) = paused_reconciliation_from(
        observations,
        has_current_state.then(|| current_deployment("deployment")),
        engine_integrations,
    )
    .await;
    let lifecycle = record
        .and_then(|record| record.state)
        .map(|state| state.status);
    (reconciled, lifecycle, calls)
}

async fn paused_reconciliation_from(
    observations: Vec<Vec<WorkloadObservation>>,
    retained: Option<dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
) -> (
    bool,
    Option<dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
    Arc<Mutex<Vec<&'static str>>>,
) {
    reconciliation_from(
        crate::model::controller::test_support::test_desired_deployment(
            "model",
            dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
        ),
        observations,
        retained,
        engine_integrations,
    )
    .await
}

/// Runs one reconciliation per observation batch for a single declared deployment and returns
/// whether every pass succeeded, the retained record, and the recorded runtime calls.
async fn reconciliation_from(
    declaration: dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration,
    observations: Vec<Vec<WorkloadObservation>>,
    retained: Option<dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
) -> (
    bool,
    Option<dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
    Arc<Mutex<Vec<&'static str>>>,
) {
    let reconciliation_count = observations.len().max(1);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments
        .push(declaration);
    let current = TestCurrent::new(calls.clone());
    if let Some(retained) = retained {
        current.upsert(retained, None).expect("current state");
    }
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: Arc::new(Mutex::new(observations)),
                start_observation: None,
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId("runtime/test".to_owned()),
            },
        )
        .expect("Runtime adapter");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired,
        state: current.clone(),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations,
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            dotcore_artifact::ModelArtifactCache::new("/test-cache".into()).expect("test cache"),
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        accelerator_attribution: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };
    calls.lock().expect("calls").clear();
    let mut reconciled = true;
    for _ in 0..reconciliation_count {
        reconciled &= inner.reconcile().await.is_ok();
    }
    let record = current
        .snapshot()
        .expect("current state")
        .model_deployments
        .into_iter()
        .next();
    (reconciled, record, calls)
}

fn paused_identity() -> ObservedWorkload {
    let desired = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
    );
    crate::model::controller::test_support::expected_identity(
        &test_site(),
        &desired,
        "runtime/test",
    )
}

fn paused_binding(
    engine_integrations: &crate::model::engine::EngineIntegrationRegistry,
    identity: &ObservedWorkload,
) -> crate::inference::EngineBinding {
    let deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
    );
    let resolved = resolved_launch(&deployment).expect("resolved paused model");
    engine_integrations
        .bind_workload(
            &resolved,
            &deployment.model_deployment_id,
            identity.services.first().expect("paused service binding"),
        )
        .expect("paused engine binding")
}

fn runtime_calls(calls: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
    calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| matches!(**call, "create" | "start" | "stop" | "inspect" | "remove"))
        .copied()
        .collect()
}

#[tokio::test]
async fn reconciliation_stops_and_removes_a_valid_owned_orphan() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            OwnedOrphanRuntime {
                calls: calls.clone(),
                orphan: ObservedWorkload {
                    workload_id: WorkloadId("owned-orphan".to_owned()),
                    correlation: WorkloadCorrelation {
                        owner_id: "orphan-deployment".to_owned(),
                        reconcile_generation: 1,
                    },
                    backend_id: RuntimeBackendId("runtime/test".to_owned()),
                    loopback_endpoints: Vec::new(),
                    services: Vec::new(),
                    host_process: None,
                },
            },
        )
        .expect("Runtime adapter");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let desired = TestDesired::new(calls.clone());
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired,
        state: TestCurrent::new(calls.clone()),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations: crate::model::controller::test_support::test_engine_integrations(),
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            dotcore_artifact::ModelArtifactCache::new("/test-cache".into()).expect("test cache"),
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        accelerator_attribution: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };

    assert!(matches!(
        inner.reconcile().await,
        Ok(ReconcileOutcome::Settled)
    ));
    assert_eq!(calls.lock().expect("calls").as_slice(), ["stop", "remove"]);
}

#[tokio::test]
async fn manager_pass_never_treats_changed_or_newly_declared_workloads_as_orphans() {
    let race = OrphanRace::new();
    let desired = TestDesired::new(Arc::new(Mutex::new(Vec::new())));
    *desired.state.lock().expect("desired site") = race.site.clone();
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        TestCurrent::new(Arc::new(Mutex::new(Vec::new()))),
        race.runtimes(RuntimeTargetMapBuilder::new()),
        Vec::new(),
        "/test-cache".into(),
        Some(crate::model::controller::test_support::ready_oci_preparer()),
    )
    .expect("manager controller");

    let (result, ()) = tokio::join!(controller.inner.reconcile(), async {
        race.stop_started.notified().await;
        *desired.state.lock().expect("desired site") = race.changed_site();
        race.release_stop.notify_one();
    });

    assert!(result.is_ok(), "reconcile pass: {result:?}");
    assert_eq!(race.calls(), [("stop", "a-workload".to_owned())]);
}

#[tokio::test]
async fn node_pass_never_treats_changed_or_newly_declared_workloads_as_orphans() {
    let race = OrphanRace::new();
    let node_id = race.site.execution_targets[0].site_node_id.clone();
    let projection =
        NodeViewProjection::for_site_node(&race.site, &node_id).expect("node projection");
    let authority = AcceptedNodeViewAuthority::new(
        race.site.site_id.clone(),
        node_id.clone(),
        NodeIncarnation::parse("b".repeat(32)).expect("node incarnation"),
        ManagerSession::parse("a".repeat(32)).expect("manager session"),
        Duration::from_secs(30),
    )
    .expect("view authority");
    accept_view(&authority, &race.site, 1);
    let input = crate::model::controller::test_support::ProjectedTestInput::new(
        race.site.site_id.clone(),
        projection,
    );
    let directory = crate::test_support::canonical_tempdir().expect("state directory");
    let current = FileNodeState::open(
        directory.path().join("node"),
        race.site.site_id.clone(),
        node_id.clone(),
    )
    .expect("node current state");
    let realizer =
        crate::model::controller::ModelRealizer::new_node_realizer(
            crate::model::controller::NodeModelRealizerDependencies {
                input: input.clone(),
                composition: crate::model::controller::ModelControllerDependencies {
                    artifacts: ArtifactDependencies {
                        oci_preparer: Some(
                            crate::model::controller::test_support::ready_oci_preparer(),
                        ),
                        ..Default::default()
                    },
                    target_capabilities:
                        crate::model::controller::test_support::test_target_capability_source(),
                    engine_integrations:
                        crate::model::controller::test_support::test_engine_integrations(),
                    accelerator_attribution: None,
                },
                serving_targets:
                    crate::model::controller::serving_targets::ServingTargetsPublisher::new().0,
                shutdown: tokio_util::sync::CancellationToken::new(),
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    directory.path(),
                ),
            },
            current,
            race.runtimes(
                RuntimeTargetMapBuilder::new()
                    .with_action_authority(RuntimeActionAuthority::Leased(authority.clone())),
            ),
            Vec::new(),
            "/test-cache".into(),
        )
        .expect("node realizer");

    let (result, ()) = tokio::join!(realizer.inner.reconcile(), async {
        race.stop_started.notified().await;
        let changed = race.changed_site();
        accept_view(&authority, &changed, 2);
        input.replace(ModelSiteSnapshot {
            site_id: changed.site_id.clone(),
            execution_targets: changed.execution_targets.clone(),
            deployments: changed.model_deployments.clone(),
        });
        race.release_stop.notify_one();
    });

    assert!(result.is_ok(), "reconcile pass: {result:?}");
    assert_eq!(race.calls(), [("stop", "a-workload".to_owned())]);
}

#[tokio::test]
async fn paused_stopped_workload_projects_paused_without_runtime_mutation() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
    );
    let identity = crate::model::controller::test_support::expected_identity(
        &test_site(),
        &deployment,
        "runtime/test",
    );
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments
        .push(deployment);
    let current = TestCurrent::new(calls.clone());
    current
        .upsert(current_deployment("deployment"), None)
        .expect("current state");
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: Arc::new(Mutex::new(vec![vec![WorkloadObservation::stopped(
                    identity,
                )]])),
                start_observation: None,
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId("runtime/test".to_owned()),
            },
        )
        .expect("Runtime adapter");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired,
        state: current.clone(),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations: crate::model::controller::test_support::test_engine_integrations(),
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            dotcore_artifact::ModelArtifactCache::new("/test-cache".into()).expect("test cache"),
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        accelerator_attribution: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };
    calls.lock().expect("calls").clear();

    inner.reconcile().await.expect("reconciliation");

    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );
    assert!(
        calls
            .lock()
            .expect("calls")
            .iter()
            .all(|call| { !matches!(*call, "create" | "start" | "stop" | "inspect" | "remove") })
    );
}

#[tokio::test]
async fn missing_oci_preparer_follows_model_preparation_before_runtime_work() {
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-missing-oci-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime_calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "fixture-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("model artifact"),
    )
    .provenance_digest
    .clear();
    let cache = ModelArtifactCache::new(cache_root.clone()).expect("artifact cache");
    let resolved = resolved_launch(&deployment).expect("resolved model");
    for path in cache
        .expected_paths(&resolved.artifacts)
        .expect("artifact paths")
        .artifacts
        .into_values()
    {
        std::fs::create_dir_all(path.parent().expect("artifact parent")).expect("artifact parent");
        std::fs::write(path, b"prepared model artifact").expect("prepared model artifact");
    }
    desired
        .state
        .lock()
        .expect("desired site")
        .model_deployments
        .push(deployment.clone());
    desired
        .state
        .lock()
        .expect("desired site")
        .execution_targets[0]
        .runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
        ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        },
    ));
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            RecordingRuntime {
                calls: runtime_calls.clone(),
                virtualized: false,
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        runtimes.build().expect("Runtime map"),
        Vec::new(),
        cache_root.clone(),
        None,
    )
    .expect("controller construction");

    let error = controller
        .inner
        .reconcile()
        .await
        .expect_err("missing OCI preparer");

    assert_eq!(
        error.to_string(),
        "Model lifecycle configuration failed: physical OCI model deployment requires image preparation"
    );
    assert!(
        current
            .snapshot()
            .expect("current state")
            .model_deployments
            .iter()
            .any(|record| {
                record.model_deployment_id == deployment.model_deployment_id
                    && record.state.as_ref().is_some_and(|state| {
                        state.status == ModelDeploymentStatus::Loading as i32
                            && state.status_message == "Acquiring engine image"
                    })
            })
    );
    assert_eq!(runtime_calls.lock().expect("Runtime calls").len(), 0);
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn nix_engine_on_a_native_target_reaches_preparation_without_an_oci_preparer() {
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-native-nix-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime_calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "fixture-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("model artifact"),
    )
    .provenance_digest
    .clear();
    crate::test_support::nix::retain_development_source_engine(
        deployment.snapshot.as_mut().expect("snapshot"),
        dotcore_domain::test_support::nix::STORE_PATH,
    );
    let cache = ModelArtifactCache::new(cache_root.clone()).expect("artifact cache");
    let resolved = resolved_launch(&deployment).expect("resolved model");
    for path in cache
        .expected_paths(&resolved.artifacts)
        .expect("artifact paths")
        .artifacts
        .into_values()
    {
        std::fs::create_dir_all(path.parent().expect("artifact parent")).expect("artifact parent");
        std::fs::write(path, b"prepared model artifact").expect("prepared model artifact");
    }
    desired
        .state
        .lock()
        .expect("desired site")
        .model_deployments
        .push(deployment.clone());
    desired
        .state
        .lock()
        .expect("desired site")
        .execution_targets[0]
        .runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
        ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                dotcore_proto::dotcore::site::v1::NativeRuntimeAdapter::default(),
            )),
        },
    ));
    let mut runtimes = RuntimeTargetMapBuilder::new();
    runtimes
        .insert(
            "target-one",
            RecordingRuntime {
                calls: runtime_calls.clone(),
                virtualized: false,
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        runtimes.build().expect("Runtime map"),
        Vec::new(),
        cache_root.clone(),
        None,
    )
    .expect("controller construction");

    controller
        .inner
        .reconcile()
        .await
        .expect("Nix preparation needs no OCI preparer");

    // This node has no development source, so the Nix preparer itself fails the acquisition.
    let snapshot = current.snapshot().expect("current state");
    let state = snapshot
        .model_deployments
        .iter()
        .find(|record| record.model_deployment_id == deployment.model_deployment_id)
        .and_then(|record| record.state.as_ref())
        .expect("deployment state");
    assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
    assert_eq!(
        state.failure.as_ref().map(|failure| failure.code.as_str()),
        Some("engine_image_acquisition_failed")
    );
    assert_eq!(runtime_calls.lock().expect("Runtime calls").len(), 0);
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn upstream_declarations_do_not_block_local_runtime_reconciliation() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let local = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    let mut upstream = local.clone();
    upstream.model_deployment_id = "upstream-deployment".to_owned();
    upstream.placement = None;
    upstream.requested_parallel_shape = 0;
    upstream.desired_state =
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present as i32;
    upstream.snapshot = Some(ModelDeploymentSnapshot {
        source: Some(ModelDeploymentSnapshotSource::Upstream(Default::default())),
    });
    upstream.desired_state =
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Teardown as i32;
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments = vec![local.clone(), upstream.clone()];
    let current = TestCurrent::new(calls.clone());
    current
        .upsert(current_deployment(&local.model_deployment_id), None)
        .expect("local current state");
    current
        .upsert(current_deployment(&upstream.model_deployment_id), None)
        .expect("upstream current state");
    let identity = crate::model::controller::test_support::expected_identity(
        &test_site(),
        &local,
        dotcore_runtime_backends::simulated::BACKEND_ID,
    );
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: Arc::new(Mutex::new(vec![vec![WorkloadObservation::Running(
                    identity.clone(),
                )]])),
                start_observation: None,
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId(
                    dotcore_runtime_backends::simulated::BACKEND_ID.to_owned(),
                ),
            },
        )
        .expect("Runtime adapter");
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        builder.build().expect("Runtime map"),
        vec![TargetObservation {
            execution_target_id: "target-one".to_owned(),
            observation: WorkloadObservation::Running(identity),
        }],
        "/test-cache".into(),
        Some(crate::model::controller::test_support::ready_oci_preparer()),
    )
    .expect("controller construction");

    let (reconciliation, records, _) = capture_records_async(|| controller.inner.reconcile()).await;
    assert!(matches!(reconciliation, Ok(ReconcileOutcome::Settled)));
    let ready_events = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_serving_target_ready\"".to_owned())
            )
        })
        .collect::<Vec<_>>();
    let published_events = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_serving_target_published\"".to_owned())
            )
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            published_events.as_slice(),
            [Record::Event { level, fields }]
                if level == "INFO"
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
                    && fields.get("outcome") == Some(&"\"published\"".to_owned())
        ),
        "{records:#?}"
    );
    assert!(
        matches!(
            ready_events.as_slice(),
            [Record::Event { level, fields }]
                if level == "INFO"
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
                    && fields.get("execution_target_id") == Some(&"target-one".to_owned())
                    && fields.get("workload_id") == Some(&"test-workload".to_owned())
        ),
        "{records:#?}"
    );
    let (_, repeated_records, _) = capture_records_async(|| controller.inner.reconcile()).await;
    assert_eq!(
        repeated_records
            .iter()
            .filter(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_serving_target_ready\"".to_owned())
            ))
            .count(),
        0
    );
    let repeated_publications = repeated_records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_serving_target_published\"".to_owned())
            )
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            repeated_publications.as_slice(),
            [Record::Event { level, fields }]
                if level == "DEBUG"
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
                    && fields.get("outcome") == Some(&"\"unchanged\"".to_owned())
        ),
        "{repeated_records:#?}"
    );
    assert_eq!(
        repeated_records
            .iter()
            .filter(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_lifecycle_transition\"".to_owned())
            ))
            .count(),
        0
    );
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments[0]
        .desired_state =
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused as i32;
    let operation = controller
        .inner
        .capture_operation(
            &local,
            LifecycleOperationAttempt::Runtime {
                reconcile_generation: 1,
            },
        )
        .expect("local lifecycle operation");
    let (published, rejected_records, _) = capture_records_async(|| {
        controller
            .inner
            .publish_ready_serving_target(&local, Some(Vec::new()), Some(&operation))
    })
    .await;
    assert!(!published.expect("rejected publication"));
    assert_eq!(
        rejected_records
            .iter()
            .filter(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_serving_target_ready\"".to_owned())
            ))
            .count(),
        0
    );
    let current_snapshot = current.snapshot().expect("current state");
    let local_record = current_snapshot
        .model_deployments
        .iter()
        .find(|deployment| deployment.model_deployment_id == local.model_deployment_id)
        .expect("local current state");
    assert_eq!(
        local_record.state.as_ref().expect("local state").status,
        ModelDeploymentStatus::Ready as i32
    );
    // The realizer never touches the record of a deployment it does not realize.
    assert_eq!(
        current_snapshot
            .model_deployments
            .iter()
            .map(|deployment| deployment.model_deployment_id.as_str())
            .collect::<Vec<_>>(),
        [
            local.model_deployment_id.as_str(),
            upstream.model_deployment_id.as_str()
        ]
    );
    assert_eq!(
        desired
            .model_deployments()
            .expect("desired state")
            .iter()
            .map(|deployment| deployment.model_deployment_id.as_str())
            .collect::<Vec<_>>(),
        [
            local.model_deployment_id.as_str(),
            upstream.model_deployment_id.as_str()
        ]
    );
}

#[tokio::test]
async fn paused_matrix_maps_every_conclusive_workload_observation_without_start_or_create() {
    let identity = paused_identity();
    for (observation, lifecycle, expected_calls) in [
        (
            WorkloadObservation::Starting(identity.clone()),
            ModelDeploymentStatus::Pausing,
            vec!["stop"],
        ),
        (
            WorkloadObservation::Running(identity.clone()),
            ModelDeploymentStatus::Pausing,
            vec!["stop"],
        ),
        (
            WorkloadObservation::Stopping(identity.clone()),
            ModelDeploymentStatus::Pausing,
            Vec::new(),
        ),
        (
            WorkloadObservation::stopped(identity.clone()),
            ModelDeploymentStatus::Paused,
            Vec::new(),
        ),
        (
            WorkloadObservation::Unavailable(identity.clone()),
            ModelDeploymentStatus::Pausing,
            vec!["stop"],
        ),
        (
            WorkloadObservation::Absent(identity.clone()),
            ModelDeploymentStatus::Unavailable,
            Vec::new(),
        ),
    ] {
        let (reconciled, state, calls) = paused_reconciliation(vec![vec![observation]], true).await;
        assert!(reconciled);
        assert_eq!(state, Some(lifecycle as i32));
        assert_eq!(runtime_calls(&calls), expected_calls);
    }

    let (reconciled, state, calls) = paused_reconciliation(vec![Vec::new()], true).await;
    assert!(reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Unavailable as i32));
    assert!(runtime_calls(&calls).is_empty());
}

#[tokio::test]
async fn paused_binding_is_retired_only_after_exact_stopped_observation() {
    for observation in [
        WorkloadObservation::Starting(paused_identity()),
        WorkloadObservation::Running(paused_identity()),
        WorkloadObservation::Stopping(paused_identity()),
    ] {
        let engine_integrations =
            crate::model::controller::test_support::test_engine_integrations();
        let binding = paused_binding(&engine_integrations, &paused_identity());

        let (reconciled, _, _) = paused_reconciliation_with_integrations(
            vec![vec![observation]],
            true,
            engine_integrations,
        )
        .await;

        assert!(reconciled);
        assert!(
            binding.revoke(),
            "active or draining binding must remain retained"
        );
    }

    let engine_integrations = crate::model::controller::test_support::test_engine_integrations();
    let identity = paused_identity();
    let binding = paused_binding(&engine_integrations, &identity);
    let (reconciled, state, _) = paused_reconciliation_with_integrations(
        vec![vec![WorkloadObservation::stopped(identity)]],
        true,
        engine_integrations,
    )
    .await;

    assert!(reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Paused as i32));
    assert!(
        !binding.revoke(),
        "confirmed stopped binding must already be retired"
    );
}

#[tokio::test]
async fn old_format_paused_record_is_stamped_with_the_reconciled_generation() {
    let mut retained = current_deployment("deployment");
    retained.state = Some(super::super::state::paused_state(None));

    let (reconciled, record, calls) = paused_reconciliation_from(
        vec![vec![WorkloadObservation::stopped(paused_identity())]],
        Some(retained),
        crate::model::controller::test_support::test_engine_integrations(),
    )
    .await;
    let record = record.expect("retained paused record");

    assert!(reconciled);
    assert_eq!(record.reconciled_lifecycle_generation, Some(1));
    assert_eq!(
        record.state.expect("current state").status,
        ModelDeploymentStatus::Paused as i32
    );
    assert!(runtime_calls(&calls).is_empty());
}

#[tokio::test]
async fn start_in_flight_from_a_newer_generation_fails_without_retry_when_older_intent_returns() {
    let declaration = present_declaration(3);
    let identity = crate::model::controller::test_support::expected_identity(
        &test_site(),
        &declaration,
        "runtime/test",
    );
    for observation in [
        WorkloadObservation::Absent(identity.clone()),
        WorkloadObservation::stopped(identity.clone()),
    ] {
        let mut retained = current_deployment("deployment");
        retained.reconciled_lifecycle_generation = Some(5);
        retained.state = Some(super::super::state::resume_start_state(
            &super::super::state::resuming_state(5),
            dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartInFlight,
            5,
        ));

        let (reconciled, record, calls) = reconciliation_from(
            declaration.clone(),
            vec![vec![observation.clone()], vec![observation]],
            Some(retained),
            crate::model::controller::test_support::test_engine_integrations(),
        )
        .await;
        let record = record.expect("retained record");
        let state = record.state.expect("current state");

        assert!(reconciled);
        assert!(runtime_calls(&calls).is_empty());
        assert_eq!(record.reconciled_lifecycle_generation, Some(3));
        assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(state.activation, None);
        assert_eq!(
            state.failure.expect("failure").code,
            "resume_start_uncertain"
        );
    }
}

#[tokio::test]
async fn start_in_flight_from_an_older_generation_resumes_under_a_newer_resume() {
    let declaration = present_declaration(5);
    let identity = crate::model::controller::test_support::expected_identity(
        &test_site(),
        &declaration,
        "runtime/test",
    );
    for observation in [
        WorkloadObservation::Absent(identity.clone()),
        WorkloadObservation::stopped(identity.clone()),
        WorkloadObservation::Running(identity.clone()),
    ] {
        let mut retained = current_deployment("deployment");
        retained.reconciled_lifecycle_generation = Some(3);
        retained.state = Some(super::super::state::resume_start_state(
            &super::super::state::resuming_state(3),
            dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartInFlight,
            identity.correlation.reconcile_generation,
        ));

        let (reconciled, record, calls) = reconciliation_from(
            declaration.clone(),
            vec![vec![observation]],
            Some(retained),
            crate::model::controller::test_support::test_engine_integrations(),
        )
        .await;
        let record = record.expect("retained record");
        let state = record.state.expect("current state");

        assert!(reconciled);
        assert!(runtime_calls(&calls).is_empty());
        assert_eq!(record.reconciled_lifecycle_generation, Some(5));
        assert_eq!(state.status, ModelDeploymentStatus::Loading as i32);
        assert_eq!(state.status_message, "Resuming");
        assert_eq!(state.failure, None);
        assert_eq!(
                state.activation,
                Some(dotcore_proto::dotcore::model::v1::ModelDeploymentActivation {
                    phase:
                        dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::AwaitingStart
                            as i32,
                    workload_generation: None,
                    lifecycle_generation: 5,
                })
            );
    }
}

#[tokio::test]
async fn failed_record_from_another_generation_is_restamped_without_retry() {
    for retained_generation in [None, Some(5)] {
        let mut retained = current_deployment("deployment");
        retained.reconciled_lifecycle_generation = retained_generation;
        retained.state = Some(failed_state("engine_startup_failed"));

        let (reconciled, record, calls) = reconciliation_from(
            present_declaration(3),
            vec![Vec::new(), Vec::new()],
            Some(retained),
            crate::model::controller::test_support::test_engine_integrations(),
        )
        .await;
        let record = record.expect("retained record");
        let state = record.state.expect("current state");

        assert!(reconciled);
        assert!(runtime_calls(&calls).is_empty());
        assert_eq!(record.reconciled_lifecycle_generation, Some(3));
        assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
        assert_eq!(
            state.failure.expect("failure").code,
            "engine_startup_failed"
        );
    }
}

#[tokio::test]
async fn newer_resume_recovers_a_failed_record_whose_pause_was_never_observed() {
    let mut retained = current_deployment("deployment");
    retained.reconciled_lifecycle_generation = Some(1);
    retained.state = Some(failed_state("engine_startup_failed"));

    let (reconciled, record, calls) = reconciliation_from(
        present_declaration(3),
        vec![Vec::new()],
        Some(retained),
        crate::model::controller::test_support::test_engine_integrations(),
    )
    .await;
    let record = record.expect("retained record");
    let state = record.state.expect("current state");

    assert!(reconciled);
    assert!(runtime_calls(&calls).is_empty());
    assert_eq!(record.reconciled_lifecycle_generation, Some(3));
    assert_eq!(state.status, ModelDeploymentStatus::Loading as i32);
    assert_eq!(state.status_message, "Resuming");
    assert_eq!(state.failure, None);
    assert_eq!(
        state.activation,
        Some(
            dotcore_proto::dotcore::model::v1::ModelDeploymentActivation {
                phase:
                    dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::AwaitingStart
                        as i32,
                workload_generation: None,
                lifecycle_generation: 3,
            }
        )
    );
}

/// Returns the fixture deployment as recovered Present intent at an explicit Resume generation.
fn present_declaration(
    lifecycle_generation: u64,
) -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
    let mut declaration = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    declaration.lifecycle_generation = lifecycle_generation;
    declaration.lifecycle_transition =
        dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Resume as i32;
    declaration
}

fn failed_state(code: &str) -> dotcore_proto::dotcore::model::v1::ModelDeploymentState {
    let mut state = super::super::state::loading_state();
    state.status = ModelDeploymentStatus::Failed as i32;
    state.status_message = "failed".to_owned();
    state.failure = Some(dotcore_proto::dotcore::model::v1::ModelDeploymentFailure {
        code: code.to_owned(),
        message: "failed".to_owned(),
        diagnostics: Vec::new(),
    });
    state
}

#[tokio::test]
async fn paused_unavailable_remains_pausing_until_a_later_conclusive_observation() {
    let identity = paused_identity();
    let (reconciled, state, calls) = paused_reconciliation(
        vec![
            vec![WorkloadObservation::Unavailable(identity.clone())],
            vec![WorkloadObservation::stopped(identity)],
        ],
        true,
    )
    .await;
    assert!(reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Paused as i32));
    assert_eq!(runtime_calls(&calls), ["stop"]);
}

#[tokio::test]
async fn paused_stale_and_unsafe_ownership_evidence_fail_closed_without_replacement() {
    let mut stale = paused_identity();
    let generation = stale.correlation.reconcile_generation;
    stale.correlation.reconcile_generation = generation.checked_add(1).unwrap_or(generation - 1);
    let (reconciled, state, calls) =
        paused_reconciliation(vec![vec![WorkloadObservation::Running(stale)]], true).await;
    assert!(reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Unavailable as i32));
    assert_eq!(runtime_calls(&calls), ["stop"]);

    let identity = paused_identity();
    let (reconciled, state, calls) = paused_reconciliation(
        vec![vec![WorkloadObservation::Unknown {
            workload_id: identity.workload_id,
            backend_id: identity.backend_id,
        }]],
        true,
    )
    .await;
    assert!(!reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Loading as i32));
    assert!(runtime_calls(&calls).is_empty());

    let mut malformed = paused_identity();
    malformed.workload_id = WorkloadId("invalid workload identity".to_owned());
    let (reconciled, state, calls) =
        paused_reconciliation(vec![vec![WorkloadObservation::Running(malformed)]], true).await;
    assert!(!reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Loading as i32));
    assert!(runtime_calls(&calls).is_empty());

    let mut foreign = paused_identity();
    foreign.backend_id = RuntimeBackendId("runtime/foreign".to_owned());
    let (reconciled, state, calls) =
        paused_reconciliation(vec![vec![WorkloadObservation::Running(foreign)]], true).await;
    assert!(!reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Loading as i32));
    assert!(runtime_calls(&calls).is_empty());

    let identity = paused_identity();
    let (reconciled, state, calls) = paused_reconciliation(
        vec![vec![
            WorkloadObservation::stopped(identity.clone()),
            WorkloadObservation::stopped(identity),
        ]],
        true,
    )
    .await;
    assert!(!reconciled);
    assert_eq!(state, Some(ModelDeploymentStatus::Loading as i32));
    assert!(runtime_calls(&calls).is_empty());
}

#[test]
fn paused_initial_state_accepts_configured_artifact_cache_paths() {
    let site = test_site();
    let desired = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
    );
    let artifacts =
        ArtifactManager::new("/configured-artifact-cache".into(), None).expect("artifact manager");
    let resolved = resolved_launch(&desired).expect("resolved model");
    let prepared = artifacts
        .expected_model_paths(&resolved.artifacts)
        .expect("configured artifact paths");
    let launch = workload_launch(
        &desired,
        selected_target(&site, &desired).expect("execution target"),
        &prepared,
    )
    .expect("workload launch");
    let identity = ObservedWorkload {
        workload_id: WorkloadId("configured-cache-workload".to_owned()),
        correlation: launch.correlation,
        backend_id: RuntimeBackendId("runtime/test".to_owned()),
        loopback_endpoints: Vec::new(),
        services: Vec::new(),
        host_process: None,
    };

    let state = paused_initial_state(
        &site,
        &artifacts,
        &desired,
        Some(TargetObservation {
            execution_target_id: "target-one".to_owned(),
            observation: WorkloadObservation::stopped(identity),
        }),
    )
    .expect("matching configured-cache paused observation");

    assert_eq!(state.status, ModelDeploymentStatus::Paused as i32);
}

#[tokio::test]
async fn paused_reconciliation_reconstructs_missing_current_state_from_safe_runtime_evidence() {
    let identity = paused_identity();
    for (observation, lifecycle, expected_calls) in [
        (
            WorkloadObservation::stopped(identity.clone()),
            ModelDeploymentStatus::Paused,
            Vec::new(),
        ),
        (
            WorkloadObservation::Absent(identity.clone()),
            ModelDeploymentStatus::Unavailable,
            Vec::new(),
        ),
        (
            WorkloadObservation::Running(identity),
            ModelDeploymentStatus::Pausing,
            vec!["stop"],
        ),
    ] {
        let (reconciled, state, calls) =
            paused_reconciliation(vec![vec![observation]], false).await;
        assert!(reconciled);
        assert_eq!(state, Some(lifecycle as i32));
        assert_eq!(runtime_calls(&calls), expected_calls);
    }

    let identity = paused_identity();
    let (reconciled, state, calls) = paused_reconciliation(
        vec![vec![WorkloadObservation::Unknown {
            workload_id: identity.workload_id,
            backend_id: identity.backend_id,
        }]],
        false,
    )
    .await;
    assert!(!reconciled);
    assert_eq!(state, None);
    assert!(runtime_calls(&calls).is_empty());
}

#[tokio::test]
async fn loading_stopped_workload_records_startup_failure_without_restart() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    let identity = crate::model::controller::test_support::expected_identity(
        &test_site(),
        &deployment,
        dotcore_runtime_backends::simulated::BACKEND_ID,
    );
    desired
        .state
        .lock()
        .expect("desired state")
        .model_deployments
        .push(deployment);
    let current = TestCurrent::new(calls.clone());
    let mut starting = current_deployment("deployment");
    starting.state = Some(crate::model::controller::state::realization_prepared_state(
        None,
    ));
    current.upsert(starting, None).expect("current state");
    let mut builder = RuntimeTargetMapBuilder::new();
    let startup_logs = Arc::new(Mutex::new(StartupLogBehavior::Unavailable));
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: Arc::new(Mutex::new(vec![vec![WorkloadObservation::Stopped {
                    workload: identity.clone(),
                    exit_status: Some(17),
                }]])),
                start_observation: Some(WorkloadObservation::Running(identity.clone())),
                startup_logs: startup_logs.clone(),
                backend_id: RuntimeBackendId(
                    dotcore_runtime_backends::simulated::BACKEND_ID.to_owned(),
                ),
            },
        )
        .expect("Runtime adapter");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired: desired.clone(),
        state: current.clone(),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations: crate::model::controller::test_support::test_engine_integrations(),
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            dotcore_artifact::ModelArtifactCache::new("/test-cache".into()).expect("test cache"),
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        accelerator_attribution: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(inner),
        },
    );
    calls.lock().expect("calls").clear();

    let (reconciliation, records, _) = capture_records_async(|| controller.inner.reconcile()).await;
    reconciliation.expect("startup reconciliation");

    assert!(runtime_calls(&calls).is_empty());
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "DEBUG"
                    && fields.get("event_kind")
                        == Some(&"\"model_reconciliation_decision\"".to_owned())
                    && fields.get("deployment_id") == Some(&"deployment".to_owned())
        )),
        "{records:#?}"
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "INFO"
                    && fields.get("event_kind")
                        == Some(&"\"model_lifecycle_transition\"".to_owned())
                    && fields.get("previous_state") == Some(&"Some(Loading)".to_owned())
                    && fields.get("next_state") == Some(&"Failed".to_owned())
        )),
        "{records:#?}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_lifecycle_transition\"".to_owned())
            ))
            .count(),
        1
    );
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "WARN"
                    && fields.get("event_kind")
                        == Some(&"\"model_lifecycle_problem\"".to_owned())
                    && fields.get("error_kind")
                        == Some(&"\"engine_startup_failed\"".to_owned())
        )),
        "{records:#?}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if fields.get("event_kind")
                        == Some(&"\"model_lifecycle_problem\"".to_owned())
            ))
            .count(),
        1
    );
    let state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("deployment state");
    assert_eq!(state.status, ModelDeploymentStatus::Failed as i32);
    let failure = state.failure.expect("startup failure");
    assert_eq!(failure.code, "engine_startup_failed");
    assert_eq!(
        failure.diagnostics,
        ["Stage: engine startup", "Exit status: 17"]
    );
    // The failure part-way through the starting step clears the steps.
    assert_eq!(current.start_step_history("deployment"), [Some(3), None]);

    let source = dotcore_runtime::RuntimeLogSource {
        name: "model".to_owned(),
        label: "Model".to_owned(),
    };
    *startup_logs.lock().expect("startup logs") = StartupLogBehavior::History {
        expected_workload_id: identity.workload_id.clone(),
        history: dotcore_runtime::RuntimeLogHistory {
            entries: vec![dotcore_runtime::RuntimeLogEntry {
                source,
                timestamp: std::time::SystemTime::UNIX_EPOCH,
                stream: dotcore_runtime::RuntimeLogStreamKind::Stderr,
                content:
                    b"File \"/opt/dotcore/models/snapshot/model_mtp.safetensors\" FileNotFoundError"
                        .to_vec(),
                truncated: false,
            }],
            has_older_output: false,
            candidate_budget_truncated: false,
        },
    };
    let reset_to_loading = || {
        let revision =
            current.snapshot().expect("current state").model_deployments[0].record_revision;
        current
            .upsert(current_deployment("deployment"), Some(revision))
            .expect("reset loading state");
    };
    reset_to_loading();
    controller
        .inner
        .reconcile()
        .await
        .expect("startup output reconciliation");
    let state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("deployment state");
    assert_eq!(
        state.failure.expect("startup failure").diagnostics,
        [
            "Stage: engine startup",
            "Exit status: 17",
            "Output tail: File \"[REDACTED]\" FileNotFoundError",
        ]
    );

    *startup_logs.lock().expect("startup logs") = StartupLogBehavior::Delayed;
    reset_to_loading();
    controller
        .inner
        .reconcile()
        .await
        .expect("late startup output reconciliation");
    let state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("deployment state");
    assert_eq!(
        state.failure.expect("startup failure").diagnostics,
        ["Stage: engine startup", "Exit status: 17"]
    );

    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    controller
        .inner
        .reconcile()
        .await
        .expect("pause reconciliation");
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );

    desired.transition("deployment", ModelDeploymentDesiredState::Present);
    calls.lock().expect("calls").clear();
    // The node first records its resume activation, then touches the retained workload.
    controller
        .inner
        .reconcile()
        .await
        .expect("resume intent reconciliation");

    controller
        .inner
        .reconcile()
        .await
        .expect("resume reconciliation");

    assert_eq!(runtime_calls(&calls), ["start"]);
    let resumed_state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("resumed state");
    assert_ne!(resumed_state.status, ModelDeploymentStatus::Failed as i32);
    assert!(resumed_state.failure.is_none());
}

#[tokio::test(start_paused = true)]
async fn simulated_download_precedes_runtime_creation_clears_progress_and_skips_oci() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired.persist_present("fixture-model");
    let _site = test_site();
    let current = TestCurrent::new(calls);
    let mut pending = current_deployment("deployment");
    pending.state = Some(crate::model::controller::state::first_realization_pending_state());
    current
        .upsert(pending, None)
        .expect("durable first-realization intent");
    let runtime_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
                default_ready_delay: Duration::from_secs(10),
                startup: Some(SimulatedStartupConfig::for_test(
                    8,
                    [0, 2, 5, 7, 8],
                    Duration::from_secs(2),
                )),
                test_support: Some(runtime_support.clone()),
                ..Default::default()
            }),
        )
        .expect("simulated runtime");
    let oci = Arc::new(CountingOciPreparer::default());
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                artifacts: ArtifactDependencies {
                    oci_preparer: Some(oci.clone()),
                    ..Default::default()
                },
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("controller");

    let reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    let initial = wait_for_download_progress(&current, 0).await;
    assert_eq!(initial.status_message, "Simulating model artifact download");
    assert_eq!(runtime_support.call_counts().creates, 0);

    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_download_progress(&current, 2).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_download_progress(&current, 5).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_download_progress(&current, 7).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_for_download_progress(&current, 8).await;
    assert_eq!(runtime_support.call_counts().creates, 0);
    assert!(!reconciliation.is_finished(), "final progress must be held");

    tokio::time::advance(Duration::from_secs(2)).await;
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("reconciliation");
    let loading = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("loading state");
    assert_eq!(loading.status, ModelDeploymentStatus::Loading as i32);
    assert_eq!(loading.status_message, "Loading");
    assert_eq!(loading.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 1);
    assert_eq!(oci.calls.load(Ordering::SeqCst), 0);

    tokio::time::advance(Duration::from_secs(10)).await;
    controller
        .inner
        .reconcile()
        .await
        .expect("ready reconciliation");
    let ready = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("ready state");
    assert_eq!(ready.status, ModelDeploymentStatus::Ready as i32);
    assert_eq!(ready.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 1);
    assert_eq!(oci.calls.load(Ordering::SeqCst), 0);
    // Checking, then the simulated download, then starting; the engine step is passed.
    assert_eq!(
        current.start_step_history("deployment"),
        [Some(0), Some(1), Some(3), None]
    );
}

#[tokio::test]
async fn simulated_start_without_a_download_passes_the_download_and_engine_steps() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired.persist_present("fixture-model");
    let current = TestCurrent::new(calls);
    let mut pending = current_deployment("deployment");
    pending.state = Some(crate::model::controller::state::first_realization_pending_state());
    current
        .upsert(pending, None)
        .expect("durable first-realization intent");
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig::default()),
        )
        .expect("simulated runtime");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                artifacts: ArtifactDependencies::default(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("controller");

    for _ in 0..2 {
        controller.inner.reconcile().await.expect("reconciliation");
    }

    let ready = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("ready state");
    assert_eq!(ready.status, ModelDeploymentStatus::Ready as i32);
    assert_eq!(
        current.start_step_history("deployment"),
        [Some(0), Some(3), None]
    );
}

#[tokio::test]
async fn teardown_deletes_the_deployment_storage_that_pause_keeps_and_leaves_shared_storage() {
    use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StorageScope};
    use dotcore_runtime::{WorkloadStorageKey, WorkloadStorageKind};

    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "fixture-model",
        ModelDeploymentDesiredState::Present,
    );
    let Some(ModelDeploymentSnapshotSource::Local(local)) = deployment
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    else {
        panic!("local fixture snapshot");
    };
    local
        .engine_artifact
        .as_mut()
        .expect("engine artifact")
        .storage_bindings = [
        ("data", "/data", StorageScope::Deployment),
        ("compile-cache", "/cache", StorageScope::EngineArtifact),
    ]
    .map(|(binding_name, mount_path, scope)| StorageBinding {
        binding_name: binding_name.to_owned(),
        mount_path: mount_path.to_owned(),
        kind: StorageKind::Cache as i32,
        scope: scope as i32,
        environment: Vec::new(),
    })
    .to_vec();
    desired.persist(deployment);
    let current = TestCurrent::new(calls);
    let mut pending = current_deployment("deployment");
    pending.state = Some(crate::model::controller::state::first_realization_pending_state());
    current
        .upsert(pending, None)
        .expect("durable first-realization intent");
    let runtime_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
                test_support: Some(runtime_support.clone()),
                ..Default::default()
            }),
        )
        .expect("simulated runtime");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                artifacts: ArtifactDependencies::default(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("controller");
    let deployment_storage = WorkloadStorageKey {
        kind: WorkloadStorageKind::Cache,
        namespace: "deployment".to_owned(),
        key: "deployment".to_owned(),
    };
    let shared_storage = runtime_support.storage_keys();
    let reconcile = || async {
        for _ in 0..3 {
            controller.inner.reconcile().await.expect("reconciliation");
        }
    };

    reconcile().await;
    let attached = runtime_support.storage_keys();
    assert_eq!(shared_storage, Vec::new());
    assert_eq!(attached.len(), 2, "{attached:?}");
    assert!(runtime_support.storage_exists(&deployment_storage));

    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    reconcile().await;
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );
    assert_eq!(
        runtime_support.storage_keys(),
        attached,
        "pause keeps all storage"
    );

    desired.transition("deployment", ModelDeploymentDesiredState::Teardown);
    reconcile().await;
    assert!(desired.model_deployments().expect("desired").is_empty());
    assert_eq!(
        runtime_support.storage_keys(),
        attached
            .into_iter()
            .filter(|key| *key != deployment_storage)
            .collect::<Vec<_>>(),
        "teardown deletes the deployment's own storage and keeps the engine artifact's"
    );
}

#[tokio::test]
async fn teardown_after_a_relaunch_that_failed_before_its_workload_deletes_only_its_own_storage() {
    use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StorageScope};
    use dotcore_runtime::{WorkloadStorageKey, WorkloadStorageKind};

    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "fixture-model",
        ModelDeploymentDesiredState::Teardown,
    );
    let Some(ModelDeploymentSnapshotSource::Local(local)) = deployment
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    else {
        panic!("local fixture snapshot");
    };
    local
        .engine_artifact
        .as_mut()
        .expect("engine artifact")
        .storage_bindings = vec![StorageBinding {
        binding_name: "data".to_owned(),
        mount_path: "/data".to_owned(),
        kind: StorageKind::Cache as i32,
        scope: StorageScope::Deployment as i32,
        environment: Vec::new(),
    }];
    desired.persist(deployment);
    // A relaunch removed the earlier workload and failed before creating the next one, so the
    // deployment's storage remains with no workload holding it.
    let mut failed = current_deployment("deployment");
    failed.state = Some(crate::model::controller::state::first_realization_pending_state());
    let current = TestCurrent::new(calls);
    current
        .upsert(failed, None)
        .expect("failed relaunch record");
    let runtime_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let key = |namespace: &str, key: &str| WorkloadStorageKey {
        kind: WorkloadStorageKind::Cache,
        namespace: namespace.to_owned(),
        key: key.to_owned(),
    };
    let own = key("deployment", "deployment");
    let others = [
        key("deployment", "other-deployment"),
        key("engine-artifact", "deployment"),
    ];
    runtime_support.insert_storage(own.clone());
    for other in &others {
        runtime_support.insert_storage(other.clone());
    }
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
                test_support: Some(runtime_support.clone()),
                ..Default::default()
            }),
        )
        .expect("simulated runtime");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                artifacts: ArtifactDependencies::default(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("controller");

    controller.inner.reconcile().await.expect("reconciliation");

    assert!(desired.model_deployments().expect("desired").is_empty());
    assert_eq!(
        runtime_support.storage_keys(),
        others.to_vec(),
        "the deployment's own storage is gone; another deployment's and the engine artifact's stay"
    );
}

#[tokio::test(start_paused = true)]
async fn restarted_simulated_download_clears_stale_progress_without_replaying() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired.persist_present("fixture-model");
    let current = TestCurrent::new(calls);
    let mut persisted = current_deployment("deployment");
    persisted.state = Some(runtime_startup_progress_state(
        persisted.state.as_ref(),
        ModelArtifactProgress {
            completed: 50,
            total: Some(100),
        },
        "Simulating model artifact download",
    ));
    current
        .upsert(persisted, None)
        .expect("persisted downloading deployment");
    let runtime_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
                default_ready_delay: Duration::from_secs(10),
                startup: Some(SimulatedStartupConfig::for_test(
                    100,
                    [0, 100],
                    Duration::from_secs(60),
                )),
                test_support: Some(runtime_support.clone()),
                ..Default::default()
            }),
        )
        .expect("simulated runtime");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                artifacts: ArtifactDependencies::default(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("restarted controller");

    controller
        .inner
        .reconcile()
        .await
        .expect("restart reconciliation");

    let loading = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("loading state");
    assert_eq!(loading.status, ModelDeploymentStatus::Loading as i32);
    assert_eq!(loading.status_message, "Loading");
    assert_eq!(loading.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 1);

    tokio::time::advance(Duration::from_secs(10)).await;
    controller
        .inner
        .reconcile()
        .await
        .expect("ready reconciliation");
    let ready = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("ready state");
    assert_eq!(ready.status, ModelDeploymentStatus::Ready as i32);
    assert_eq!(ready.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 1);
}

#[tokio::test(start_paused = true)]
async fn pausing_simulated_download_cancels_late_progress_and_resume_does_not_replay() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired.persist_present("fixture-model");
    let current = TestCurrent::new(calls);
    let runtime_support = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let mut pending = current_deployment("deployment");
    pending.state = Some(crate::model::controller::state::first_realization_pending_state());
    current
        .upsert(pending, None)
        .expect("durable first-realization intent");
    let mut runtime_builder = RuntimeTargetMapBuilder::new();
    runtime_builder
        .insert(
            "target-one",
            InMemoryRuntimeBackend::with_config(SimulatedRuntimeConfig {
                default_ready_delay: Duration::from_secs(10),
                startup: Some(SimulatedStartupConfig::for_test(
                    100,
                    [0, 50, 100],
                    Duration::from_secs(60),
                )),
                test_support: Some(runtime_support.clone()),
                ..Default::default()
            }),
        )
        .expect("simulated runtime");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtime_builder.build().expect("Runtime map"),
            Vec::new(),
            "/test-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                artifacts: ArtifactDependencies::default(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                accelerator_attribution: None,
            },
        )
        .expect("controller");

    let reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    wait_for_download_progress(&current, 0).await;
    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("cancelled reconciliation");
    controller
        .inner
        .reconcile()
        .await
        .expect("paused reconciliation");
    let paused = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("paused state");
    assert_eq!(paused.status, ModelDeploymentStatus::Unavailable as i32);
    assert_eq!(paused.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 0);

    desired.transition("deployment", ModelDeploymentDesiredState::Present);
    controller
        .inner
        .reconcile()
        .await
        .expect("resumed reconciliation");
    let loading = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("loading state");
    assert_eq!(loading.status, ModelDeploymentStatus::Loading as i32);
    assert_ne!(loading.status_message, "Simulating model artifact download");
    assert_eq!(loading.progress, None);
    assert_eq!(runtime_support.call_counts().creates, 1);
}

#[tokio::test]
async fn repeated_reconciliation_reuses_snapshot_identity_evidence_without_rehashing() {
    let body = b"snapshot-model".to_vec();
    let (source_root, source) = response_server(vec![body.clone()]).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-snapshot-evidence-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let digest_count = Arc::new(AtomicUsize::new(0));
    let mut deployment = test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
        .model_artifact
        .as_mut()
        .expect("model artifact")
        .source
        .as_mut()
        .expect("artifact source")
        .content = Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
        files: vec![HuggingFaceSnapshotFile {
            path: "model.safetensors".to_owned(),
            provenance_digest: hex::encode(Sha256::digest(&body)),
        }],
    }));
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );

    for (pass, expected_hashes) in [1, 1].into_iter().enumerate() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        {
            let mut desired_site = desired.state.lock().expect("desired site");
            desired_site.execution_targets = site.execution_targets.clone();
            desired_site.model_deployments.push(deployment.clone());
        }
        let current = TestCurrent::new(calls);
        let oci = crate::model::controller::test_support::ready_oci_preparer();
        let cache = ModelArtifactCache::for_test_with_digest_count(
            cache_root.clone(),
            &source_root,
            digest_count.clone(),
        );
        let mut controller = crate::model::controller::test_support::TestRealizer::for_test(
            desired.clone(),
            current.clone(),
            test_runtime_target(),
            Vec::new(),
            cache_root.clone(),
            Some(oci.clone()),
        )
        .expect("controller");
        Arc::get_mut(&mut controller.inner)
            .expect("unshared model realizer")
            .artifacts = ArtifactManager::from_parts(cache, Some(oci));

        let (preparation, records, _) =
            capture_records_async(|| controller.inner.prepare_model_artifacts()).await;
        let preparations = preparation.expect("model preparation");
        let prepared_state = current.snapshot().expect("current state").model_deployments[0]
            .state
            .clone()
            .expect("prepared state");
        assert_eq!(prepared_state.status, ModelDeploymentStatus::Loading as i32);
        assert_eq!(prepared_state.status_message, "Loading");
        assert_eq!(prepared_state.progress, None);
        assert_eq!(prepared_state.failure, None);
        let published = preparations
            .get(&deployment.model_deployment_id)
            .expect("prepared deployment")
            .prepared
            .artifacts
            .path("model")
            .join("model.safetensors");
        assert_eq!(
            records
                .iter()
                .filter_map(|record| match record {
                    Record::Event { fields, .. }
                        if fields.get("event_kind")
                            == Some(&"\"model_preparation_stage\"".to_owned()) =>
                    {
                        fields.get("stage").map(String::as_str)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            if pass == 0 {
                vec!["\"verify\"", "\"publish\""]
            } else {
                vec!["\"verify\"", "\"reuse\""]
            }
        );
        let before_revalidation =
            current.snapshot().expect("current state").model_deployments[0].clone();
        let (loading, loading_records, _) =
            capture_records_async(|| controller.inner.prepare_model_artifacts()).await;
        loading.expect("loading revalidation");
        assert_eq!(
            current.snapshot().expect("current state").model_deployments[0],
            before_revalidation,
            "a quiet revalidation of a Loading record writes nothing"
        );
        assert_eq!(
            current.snapshot().expect("current state").model_deployments[0]
                .state
                .as_ref()
                .expect("loading state")
                .status_message,
            "Loading"
        );
        assert!(
            !loading_records.iter().any(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if matches!(
                        fields.get("event_kind").map(String::as_str),
                        Some("\"model_preparation_stage\""
                            | "\"model_preparation_ready\""
                            | "\"model_preparation_complete\"")
                    )
            )),
            "{loading_records:#?}"
        );

        let mut record = current.snapshot().expect("current state").model_deployments[0].clone();
        let revision = record.record_revision;
        let state = record.state.as_mut().expect("prepared state");
        state.status = ModelDeploymentStatus::Ready as i32;
        state.status_message = "Ready".to_owned();
        current.upsert(record, Some(revision)).expect("ready state");

        assert_eq!(
            controller
                .inner
                .state
                .snapshot()
                .expect("controller state")
                .model_deployments[0]
                .state
                .as_ref()
                .expect("controller ready")
                .status,
            ModelDeploymentStatus::Ready as i32
        );
        let (warm, warm_records, _) =
            capture_records_async(|| controller.inner.prepare_model_artifacts()).await;
        warm.expect("warm model preparation");
        assert_eq!(
            current.snapshot().expect("current state").model_deployments[0]
                .state
                .as_ref()
                .expect("warm state")
                .status_message,
            "Ready"
        );
        assert!(
            !warm_records.iter().any(|record| matches!(
                record,
                Record::Event { fields, .. }
                    if matches!(
                        fields.get("event_kind").map(String::as_str),
                        Some("\"model_preparation_stage\""
                            | "\"model_preparation_ready\""
                            | "\"model_preparation_complete\"")
                    )
            )),
            "{warm_records:#?}"
        );
        assert_eq!(digest_count.load(Ordering::SeqCst), expected_hashes);
        if pass == 1 {
            // Replacing the published file leaves the hard-linked hub blob intact for local repair.
            let replacement = published.with_extension("replacement");
            std::fs::write(&replacement, b"corrupt").expect("corrupt replacement");
            std::fs::rename(&replacement, &published).expect("corrupt published file");
            let (repair, repair_records, _) =
                capture_records_async(|| controller.inner.prepare_model_artifacts()).await;
            repair.expect("local repair");
            assert_eq!(std::fs::read(&published).expect("repaired file"), body);
            assert!(
                repair_records.iter().any(|record| matches!(
                    record,
                    Record::Event { level, fields }
                        if level == "INFO"
                            && fields.get("event_kind")
                                == Some(&"\"model_preparation_stage\"".to_owned())
                            && fields.get("stage") == Some(&"\"publish\"".to_owned())
                )),
                "{repair_records:#?}"
            );
        }
    }

    source.await.expect("source server");
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn download_progress_clears_restart_state_and_finishes_before_engine_acquisition() {
    let body = b"model-download-progress".to_vec();
    let (source_root, head_received, allow_head, first_bytes_sent, allow_finish, source) =
        streaming_response_server(body.clone(), 5).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-download-progress-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("artifact"),
    )
    .provenance_digest = hex::encode(Sha256::digest(&body));
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    {
        let mut desired_site = desired.state.lock().expect("desired site");
        desired_site.execution_targets = site.execution_targets.clone();
        desired_site.model_deployments.push(deployment.clone());
    }
    let current = TestCurrent::new(calls);
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                model_deployment_id: deployment.model_deployment_id.clone(),
                reconciled_lifecycle_generation: None,
                record_revision: 0,
                recovery_barrier: None,
                state: Some(super::super::state::downloading_state(
                    Some(&super::super::state::loading_state()),
                    ModelArtifactProgress {
                        completed: 3,
                        total: Some(body.len() as u64),
                    },
                )),
            },
            None,
        )
        .expect("stale download state");
    let engine_started = Arc::new(Notify::new());
    let release_engine = Arc::new(Notify::new());
    let oci = Arc::new(GatedOciPreparer {
        started: engine_started.clone(),
        release: release_engine,
    });
    let mut controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        current.clone(),
        test_runtime_target(),
        Vec::new(),
        cache_root.clone(),
        Some(oci.clone()),
    )
    .expect("controller");
    Arc::get_mut(&mut controller.inner)
        .expect("unshared model realizer")
        .artifacts = ArtifactManager::from_parts(cache, Some(oci));

    let reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    head_received.notified().await;
    let checking = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("checking state");
    assert_eq!(checking.status, ModelDeploymentStatus::Loading as i32);
    assert_eq!(checking.status_message, "Checking model artifacts");
    assert_eq!(checking.progress, None);

    allow_head.notify_one();
    first_bytes_sent.notified().await;
    let downloading = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = current.snapshot().expect("current state").model_deployments[0]
                .state
                .clone()
                .expect("download state");
            if state.status == ModelDeploymentStatus::Downloading as i32
                && state
                    .progress
                    .as_ref()
                    .is_some_and(|progress| progress.completed >= 5)
            {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("persisted download progress");
    let progress = downloading.progress.expect("download progress");
    assert_eq!(downloading.status_message, "Downloading model artifacts");
    assert_eq!(progress.completed, 5);
    assert_eq!(progress.total, Some(body.len() as u64));
    assert_eq!(progress.percent, Some((5 * 100 / body.len()) as u32));

    allow_finish.notify_one();
    engine_started.notified().await;
    let acquiring = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("engine acquisition state");
    assert_eq!(acquiring.status, ModelDeploymentStatus::Loading as i32);
    assert_eq!(acquiring.status_message, "Acquiring engine image");
    assert_eq!(acquiring.progress, None);
    tokio::task::yield_now().await;
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("joined sampler state")
            .status_message,
        "Acquiring engine image"
    );

    reconciliation.abort();
    reconciliation.await.expect_err("aborted reconciliation");
    source.await.expect("source server");
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn retained_docker_workload_restarts_after_pause_and_resume() {
    let PausedRetainedDockerDeployment {
        source,
        cache_root,
        calls,
        desired,
        current,
        controller,
        ready_server,
    } = paused_retained_docker_deployment().await;

    let resumed = desired.transition("deployment", ModelDeploymentDesiredState::Present);
    // The node recorded its resume activation and then stopped mid-preparation.
    let paused_record = current
        .snapshot()
        .expect("paused snapshot")
        .model_deployments[0]
        .clone();
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                reconciled_lifecycle_generation: Some(resumed.lifecycle_generation),
                state: Some(crate::model::controller::state::resuming_state(
                    resumed.lifecycle_generation,
                )),
                ..paused_record.clone()
            },
            Some(paused_record.record_revision),
        )
        .expect("node resume activation");
    let record = current
        .snapshot()
        .expect("resume snapshot")
        .model_deployments[0]
        .clone();
    let stage = crate::model::controller::state::artifact_preparation_stage_state(
        record.state.as_ref(),
        dotcore_artifact::ModelArtifactStage::Verify,
    )
    .expect("verification stage");
    assert_eq!(stage.status_message, "Verifying model artifacts");
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                state: Some(stage),
                ..record.clone()
            },
            Some(record.record_revision),
        )
        .expect("persist preparation stage");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let reconstructed = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                desired: controller.inner.desired.clone(),
                state: current.clone(),
                runtimes: controller.inner.runtimes.clone(),
                target_capabilities: controller.inner.target_capabilities.clone(),
                engine_integrations: controller.inner.engine_integrations.clone(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("reconstructed preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: controller.inner.artifacts.clone(),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );
    calls.lock().expect("calls").clear();

    reconstructed
        .inner
        .reconcile()
        .await
        .expect("resume reconciliation");

    assert_eq!(runtime_calls(&calls), ["start"]);
    let resumed_state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("resumed state");
    assert_eq!(resumed_state.status, ModelDeploymentStatus::Ready as i32);
    assert_eq!(resumed_state.status_message, "Ready");
    assert!(resumed_state.failure.is_none());
    // The restarted controller goes on from the retained steps; the download is passed.
    assert_eq!(
        current.start_step_history("deployment"),
        [None, Some(0), Some(2), Some(3), None]
    );
    ready_server.await.expect("readiness server");

    source.abort();
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn retained_docker_workload_resumes_through_the_same_node_realizer() {
    let PausedRetainedDockerDeployment {
        source,
        cache_root,
        calls,
        desired,
        current,
        controller,
        ready_server,
    } = paused_retained_docker_deployment().await;
    calls.lock().expect("calls").clear();
    let mut changes = controller.inner.workload_lifecycle.subscribe("deployment");

    desired.transition("deployment", ModelDeploymentDesiredState::Present);
    // Resume records its activation on one pass and starts the workload on a later one.
    let mut resumed_state = None;
    for _ in 0..4 {
        controller
            .inner
            .reconcile()
            .await
            .expect("resume reconciliation");
        let state = current.snapshot().expect("current state").model_deployments[0]
            .state
            .clone()
            .expect("resumed state");
        if state.status != ModelDeploymentStatus::Loading as i32 {
            resumed_state = Some(state);
            break;
        }
    }

    let resumed_state = resumed_state.expect("resume settles");
    assert_eq!(resumed_state.failure, None);
    assert_eq!(resumed_state.status, ModelDeploymentStatus::Ready as i32);
    assert_eq!(resumed_state.status_message, "Ready");
    assert_eq!(runtime_calls(&calls), ["start"]);
    // Resume starts a fresh step list after the paused record; the download is passed.
    assert_eq!(
        current.start_step_history("deployment"),
        [None, Some(0), Some(2), Some(3), None]
    );
    assert_eq!(
        published_changes(&mut changes),
        [model_started()],
        "only the pass that starts the workload announces it"
    );
    ready_server.await.expect("readiness server");

    source.abort();
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

/// A Docker deployment that reached Ready and was then paused, leaving its workload retained
/// and stopped; starting it again observes the running replacement.
struct PausedRetainedDockerDeployment {
    source: tokio::task::JoinHandle<()>,
    cache_root: std::path::PathBuf,
    calls: Arc<Mutex<Vec<&'static str>>>,
    desired: TestDesired,
    current: TestCurrent,
    controller: super::super::test_support::TestRealizer,
    ready_server: tokio::task::JoinHandle<()>,
}

async fn paused_retained_docker_deployment() -> PausedRetainedDockerDeployment {
    let (source_root, source) = response_server(vec![b"model".to_vec()]).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-resume-retained-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("artifact"),
    )
    .provenance_digest = hex::encode(Sha256::digest(b"model"));
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    {
        let mut desired_site = desired.state.lock().expect("desired site");
        desired_site.execution_targets = site.execution_targets.clone();
        desired_site.model_deployments.push(deployment.clone());
    }
    cache
        .prepare(
            &resolved_launch(&deployment)
                .expect("resolved launch")
                .artifacts,
        )
        .await
        .expect("initial cache fill");
    let mut stopped_identity = crate::model::controller::test_support::expected_identity(
        &site,
        &deployment,
        "runtime/docker",
    );
    let unavailable = dotcore_domain::test_support::ports::reserve_port();
    let unavailable_port = unavailable.port();
    stopped_identity.loopback_endpoints = vec![dotcore_runtime::LoopbackEndpoint {
        port_name: "http".to_owned(),
        host_port: unavailable_port,
    }];
    stopped_identity.services = vec![dotcore_runtime::WorkloadServiceBinding::loopback(
        stopped_identity.workload_id.0.clone(),
        "http",
        stopped_identity.correlation.reconcile_generation,
        stopped_identity.loopback_endpoints[0].clone(),
    )];
    let resolved = resolved_launch(&deployment).expect("resolved launch");
    let (ready_port, ready_server) = ready_server(&resolved.model_id.0).await;
    let mut running_identity = stopped_identity.clone();
    running_identity.loopback_endpoints[0].host_port = ready_port;
    running_identity.services = vec![dotcore_runtime::WorkloadServiceBinding::loopback(
        running_identity.workload_id.0.clone(),
        "http",
        running_identity.correlation.reconcile_generation,
        running_identity.loopback_endpoints[0].clone(),
    )];
    let engine_integrations = crate::model::controller::test_support::test_engine_integrations();
    engine_integrations
        .bind_workload(
            &resolved,
            &deployment.model_deployment_id,
            &stopped_identity.services[0],
        )
        .expect("pre-pause engine binding");
    let mut builder = RuntimeTargetMapBuilder::new();
    let observations = Arc::new(Mutex::new(vec![vec![WorkloadObservation::stopped(
        stopped_identity.clone(),
    )]]));
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: observations.clone(),
                start_observation: Some(WorkloadObservation::Running(running_identity)),
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId("runtime/docker".to_owned()),
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    let mut ready = current_deployment("deployment");
    ready.state.as_mut().expect("current state").status = ModelDeploymentStatus::Ready as i32;
    current.upsert(ready, None).expect("current deployment");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                desired: desired.clone(),
                state: current.clone(),
                runtimes: builder.build().expect("Runtime map"),
                engine_integrations,
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: ArtifactManager::from_parts(
                    cache,
                    Some(crate::model::controller::test_support::ready_oci_preparer()),
                ),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );

    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    controller
        .inner
        .reconcile()
        .await
        .expect("pause reconciliation");
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );

    PausedRetainedDockerDeployment {
        source,
        cache_root,
        calls,
        desired,
        current,
        controller,
        ready_server,
    }
}

#[tokio::test]
async fn omitted_docker_workload_recreation_rebinds_healthy_replacement() {
    let (source_root, source) = response_server(vec![b"model".to_vec()]).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-absent-replacement-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("artifact"),
    )
    .provenance_digest = hex::encode(Sha256::digest(b"model"));
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    {
        let mut desired_site = desired.state.lock().expect("desired site");
        desired_site.execution_targets = site.execution_targets.clone();
        desired_site.model_deployments.push(deployment.clone());
    }
    cache
        .prepare(
            &resolved_launch(&deployment)
                .expect("resolved launch")
                .artifacts,
        )
        .await
        .expect("initial cache fill");
    let resolved = resolved_launch(&deployment).expect("resolved launch");
    let artifacts = cache
        .expected_paths(&resolved.artifacts)
        .expect("artifact paths");
    let launch = workload_launch(
        &deployment,
        selected_target(&site, &deployment).expect("target-one"),
        &artifacts,
    )
    .expect("workload launch");
    let generation = launch.correlation.reconcile_generation;
    let unavailable = dotcore_domain::test_support::ports::reserve_port();
    let unavailable_port = unavailable.port();
    let stale_service = dotcore_runtime::WorkloadServiceBinding::loopback(
        "retained-workload",
        "http",
        generation,
        LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: unavailable_port,
        },
    );
    let absent = ObservedWorkload {
        workload_id: WorkloadId("retained-workload".to_owned()),
        correlation: launch.correlation.clone(),
        backend_id: RuntimeBackendId("runtime/docker".to_owned()),
        loopback_endpoints: vec![LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: unavailable_port,
        }],
        services: vec![stale_service.clone()],
        host_process: None,
    };
    let (ready_port, ready) = ready_server(&resolved.model_id.0).await;
    let replacement_endpoint = LoopbackEndpoint {
        port_name: "http".to_owned(),
        host_port: ready_port,
    };
    let created = ObservedWorkload {
        workload_id: absent.workload_id.clone(),
        correlation: launch.correlation,
        backend_id: absent.backend_id.clone(),
        loopback_endpoints: vec![replacement_endpoint.clone()],
        services: vec![dotcore_runtime::WorkloadServiceBinding::loopback(
            "retained-workload",
            "http",
            generation,
            replacement_endpoint,
        )],
        host_process: None,
    };
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            ReplacingMissingRuntime {
                calls: calls.clone(),
                created,
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    let mut ready_state = current_deployment("deployment");
    ready_state.state.as_mut().expect("current state").status = ModelDeploymentStatus::Ready as i32;
    current
        .upsert(ready_state, None)
        .expect("current deployment");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let engine_integrations = crate::model::controller::test_support::test_engine_integrations();
    engine_integrations
        .bind_workload(&resolved, &deployment.model_deployment_id, &stale_service)
        .expect("removed realization binding");
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        accelerator_attribution: None,
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired,
        state: current.clone(),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations,
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            cache,
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };

    inner.reconcile().await.expect("replacement reconciliation");
    assert_eq!(runtime_calls(&calls), ["create"]);
    let replacement_state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("replacement state");
    assert_eq!(
        replacement_state.status,
        ModelDeploymentStatus::Ready as i32
    );
    assert_eq!(replacement_state.status_message, "Ready");
    assert!(replacement_state.failure.is_none());
    ready.await.expect("replacement readiness server");

    source.abort();
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn retained_docker_workload_resume_recovers_from_unconfirmed_start() {
    let (source_root, source) = response_server(vec![b"model".to_vec()]).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-resume-retained-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("artifact"),
    )
    .provenance_digest = hex::encode(Sha256::digest(b"model"));
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    {
        let mut desired_site = desired.state.lock().expect("desired site");
        desired_site.execution_targets = site.execution_targets.clone();
        desired_site.model_deployments.push(deployment.clone());
    }
    cache
        .prepare(
            &resolved_launch(&deployment)
                .expect("resolved launch")
                .artifacts,
        )
        .await
        .expect("initial cache fill");
    let identity = crate::model::controller::test_support::expected_identity(
        &site,
        &deployment,
        "runtime/docker",
    );
    let mut builder = RuntimeTargetMapBuilder::new();
    let observations = Arc::new(Mutex::new(vec![vec![WorkloadObservation::stopped(
        identity.clone(),
    )]]));
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: observations.clone(),
                start_observation: Some(WorkloadObservation::Starting(identity.clone())),
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId("runtime/docker".to_owned()),
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    let mut ready = current_deployment("deployment");
    ready.state.as_mut().expect("current state").status = ModelDeploymentStatus::Ready as i32;
    current.upsert(ready, None).expect("current deployment");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                desired: desired.clone(),
                state: current.clone(),
                runtimes: builder.build().expect("Runtime map"),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: ArtifactManager::from_parts(
                    cache,
                    Some(crate::model::controller::test_support::ready_oci_preparer()),
                ),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );

    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    controller
        .inner
        .reconcile()
        .await
        .expect("pause reconciliation");
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );

    let resumed = desired.transition("deployment", ModelDeploymentDesiredState::Present);
    // The node recorded its resume activation and then stopped mid-preparation.
    let paused_record = current
        .snapshot()
        .expect("paused snapshot")
        .model_deployments[0]
        .clone();
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                reconciled_lifecycle_generation: Some(resumed.lifecycle_generation),
                state: Some(crate::model::controller::state::resuming_state(
                    resumed.lifecycle_generation,
                )),
                ..paused_record.clone()
            },
            Some(paused_record.record_revision),
        )
        .expect("node resume activation");
    let record = current
        .snapshot()
        .expect("resume snapshot")
        .model_deployments[0]
        .clone();
    let stage = crate::model::controller::state::artifact_preparation_stage_state(
        record.state.as_ref(),
        dotcore_artifact::ModelArtifactStage::Verify,
    )
    .expect("verification stage");
    assert_eq!(stage.status_message, "Verifying model artifacts");
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                state: Some(stage),
                ..record.clone()
            },
            Some(record.record_revision),
        )
        .expect("persist preparation stage");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let reconstructed = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                desired: controller.inner.desired.clone(),
                state: current.clone(),
                runtimes: controller.inner.runtimes.clone(),
                target_capabilities: controller.inner.target_capabilities.clone(),
                engine_integrations: controller.inner.engine_integrations.clone(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("reconstructed preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: controller.inner.artifacts.clone(),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );
    calls.lock().expect("calls").clear();

    reconstructed
        .inner
        .reconcile()
        .await
        .expect("resume reconciliation");

    assert_eq!(runtime_calls(&calls), ["start"]);
    let resumed_state = current.snapshot().expect("current state").model_deployments[0]
        .state
        .clone()
        .expect("resumed state");
    assert_ne!(resumed_state.status, ModelDeploymentStatus::Failed as i32);
    assert!(resumed_state.failure.is_none());

    assert_eq!(
        resumed_state
            .activation
            .as_ref()
            .expect("confirmed Resume")
            .phase,
        dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartConfirmed as i32
    );
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("post-Start stopped reconciliation");
    assert_eq!(runtime_calls(&calls), ["start"]);
    let failed = current
        .snapshot()
        .expect("failed snapshot")
        .model_deployments[0]
        .state
        .clone()
        .expect("failed state");
    assert_eq!(failed.status, ModelDeploymentStatus::Failed as i32);
    let failure = failed.failure.expect("failure");
    assert_eq!(failure.code, "engine_startup_failed");
    assert_eq!(failure.diagnostics, ["Stage: engine startup"]);

    let record = current
        .snapshot()
        .expect("current snapshot")
        .model_deployments[0]
        .clone();
    let generation = resumed_state
        .activation
        .as_ref()
        .and_then(|activation| activation.workload_generation)
        .expect("claimed workload generation");
    current
            .upsert(
                dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                    state: Some(crate::model::controller::state::resume_start_state(
                        &crate::model::controller::state::resuming_state(
                            resumed.lifecycle_generation,
                        ),
                        dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartInFlight,
                        generation,
                    )),
                    ..record.clone()
                },
                Some(record.record_revision),
            )
            .expect("restore interrupted Start");
    calls.lock().expect("calls").clear();
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("interrupted Start reconciliation");
    assert!(runtime_calls(&calls).is_empty());
    let uncertain = current
        .snapshot()
        .expect("uncertain snapshot")
        .model_deployments[0]
        .state
        .clone()
        .expect("uncertain state");
    assert_eq!(uncertain.status, ModelDeploymentStatus::Failed as i32);
    assert_eq!(
        uncertain.failure.expect("failure").code,
        "resume_start_uncertain"
    );
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("uncertain outcome stays terminal");
    assert!(runtime_calls(&calls).is_empty());

    let record = current
        .snapshot()
        .expect("current snapshot")
        .model_deployments[0]
        .clone();
    current
            .upsert(
                dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                    state: Some(crate::model::controller::state::resume_start_state(
                        &crate::model::controller::state::resuming_state(
                            resumed.lifecycle_generation,
                        ),
                        dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::StartInFlight,
                        generation,
                    )),
                    ..record.clone()
                },
                Some(record.record_revision),
            )
            .expect("restore unconfirmed Start before absent observation");
    *observations.lock().expect("observations") =
        vec![vec![WorkloadObservation::Absent(identity.clone())]];
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("absent unconfirmed Start reconciliation");
    assert!(runtime_calls(&calls).is_empty());
    let absent = current
        .snapshot()
        .expect("absent snapshot")
        .model_deployments[0]
        .state
        .clone()
        .expect("absent outcome");
    assert_eq!(absent.status, ModelDeploymentStatus::Failed as i32);
    assert_eq!(
        absent.failure.expect("failure").code,
        "resume_start_uncertain"
    );

    let record = current
        .snapshot()
        .expect("current snapshot")
        .model_deployments[0]
        .clone();
    current
        .upsert(
            dotcore_proto::dotcore::model::v1::CurrentModelDeployment {
                state: Some(crate::model::controller::state::paused_state(
                    record.state.as_ref(),
                )),
                ..record.clone()
            },
            Some(record.record_revision),
        )
        .expect("retain Paused after desired Resume");
    *observations.lock().expect("observations") = vec![vec![WorkloadObservation::Stopped {
        workload: identity.clone(),
        exit_status: Some(0),
    }]];
    current.fail_next(TestFailure::Current);
    assert!(reconstructed.inner.reconcile().await.is_err());
    assert!(runtime_calls(&calls).is_empty());
    assert_eq!(
        current
            .snapshot()
            .expect("failed write snapshot")
            .model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("reconstruct missing Resume intent");
    assert!(runtime_calls(&calls).is_empty());
    let repaired = current
        .snapshot()
        .expect("repaired snapshot")
        .model_deployments[0]
        .state
        .clone()
        .expect("repaired state");
    assert_eq!(
        repaired.activation.expect("durable Resume intent").phase,
        dotcore_proto::dotcore::model::v1::ModelDeploymentActivationPhase::AwaitingStart as i32
    );
    reconstructed
        .inner
        .reconcile()
        .await
        .expect("start after persisting Resume intent");
    assert_eq!(runtime_calls(&calls), ["start"]);
    source.abort();
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn stale_observation_is_stopped_and_removed_before_replacement() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            RecordingRuntime {
                calls: calls.clone(),
                virtualized: false,
            },
        )
        .expect("Runtime adapter");
    let stale = TargetObservation {
        execution_target_id: "target-one".to_owned(),
        observation: WorkloadObservation::Running(ObservedWorkload {
            workload_id: WorkloadId("old-workload".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "deployment".to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId("runtime/test".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        }),
    };

    replace_stale_workload(&builder.build().expect("Runtime map"), stale)
        .await
        .expect("stale workload replacement");

    assert_eq!(calls.lock().expect("calls").as_slice(), ["stop", "remove"]);
}

#[tokio::test]
async fn repaired_docker_artifact_recreates_a_stopped_workload_before_ready() {
    let (source_root, source) = response_server(vec![b"model".to_vec(), b"model".to_vec()]).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-test-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    desired.persist_present("model");
    let deployment = {
        let mut deployments = desired.state.lock().expect("desired deployments");
        let deployment = deployments
            .model_deployments
            .first_mut()
            .expect("desired deployment");
        let digest = hex::encode(Sha256::digest(b"model"));
        let artifact = model_file_source_mut(
            local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
                .model_artifact
                .as_mut()
                .expect("artifact"),
        );
        artifact.provenance_digest = digest;
        deployment.clone()
    };
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    desired
        .state
        .lock()
        .expect("desired site")
        .execution_targets = site.execution_targets.clone();
    let resolved = resolved_launch(&deployment).expect("resolved launch");
    let prepared = cache
        .prepare(&resolved.artifacts)
        .await
        .expect("initial cache fill");
    std::fs::write(prepared.artifacts.path("model"), b"corrupt").expect("corrupt cached model");
    let artifacts = cache
        .expected_paths(&resolved.artifacts)
        .expect("artifact paths");
    let launch = workload_launch(
        &deployment,
        selected_target(&site, &deployment).expect("target-one"),
        &artifacts,
    )
    .expect("workload launch");
    let generation = launch.correlation.reconcile_generation;
    let (ready_port, ready) = ready_server("model").await;
    let stopped = ObservedWorkload {
        workload_id: WorkloadId("stopped-workload".to_owned()),
        correlation: launch.correlation.clone(),
        backend_id: RuntimeBackendId("runtime/docker".to_owned()),
        loopback_endpoints: Vec::new(),
        services: Vec::new(),
        host_process: None,
    };
    let created = ObservedWorkload {
        workload_id: WorkloadId("replacement-workload".to_owned()),
        correlation: launch.correlation,
        backend_id: RuntimeBackendId("runtime/docker".to_owned()),
        loopback_endpoints: vec![LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: ready_port,
        }],
        services: vec![dotcore_runtime::WorkloadServiceBinding::loopback(
            "replacement-workload",
            "http",
            generation,
            LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: ready_port,
            },
        )],
        host_process: None,
    };
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            ReplacingStoppedRuntime {
                calls: calls.clone(),
                stopped,
                created,
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    current
        .upsert(current_deployment("deployment"), None)
        .expect("current deployment");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let engine_integrations = crate::model::controller::test_support::test_engine_integrations();
    let stale_service = dotcore_runtime::WorkloadServiceBinding::loopback(
        "stopped-workload",
        "http",
        generation,
        LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: ready_port,
        },
    );
    let stale_binding = engine_integrations
        .bind_workload(&resolved, &deployment.model_deployment_id, &stale_service)
        .expect("stale engine binding");
    let inner = InferenceControllerInner {
        workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
        preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
            std::path::Path::new("/test-preparation-logs"),
        ),
        results: desired.clone(),
        target_capabilities: crate::model::controller::test_support::test_target_capability_source(
        ),

        desired,
        state: current.clone(),
        runtimes: builder.build().expect("Runtime map"),
        engine_integrations,
        operation: Arc::new(AsyncMutex::new(())),
        reconciliation: AsyncMutex::new(()),
        preparations:
            crate::model::controller::preparation::PreparationCoordinator::from_deployments(&[])
                .expect("preparation coordinator"),
        observer_started: Mutex::new(false),
        wakeup: Arc::new(Notify::new()),
        artifacts: ArtifactManager::from_parts(
            cache,
            Some(crate::model::controller::test_support::ready_oci_preparer()),
        ),
        serving_targets_publisher,
        serving_targets_source,
        accelerator_attribution: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        timing: Timing::test().model,
        clock: Arc::new(SystemClock),
    };
    let mut changes = inner.workload_lifecycle.subscribe("deployment");

    assert!(matches!(
        inner.reconcile().await,
        Ok(ReconcileOutcome::Settled)
    ));
    let runtime_calls = calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| matches!(**call, "stop" | "remove" | "create" | "start"))
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(runtime_calls, ["remove", "create"]);
    assert_eq!(
        published_changes(&mut changes),
        [model_started()],
        "the replacement is announced once, when it is created"
    );
    let (_, cancellation) = crate::inference::inference_cancellation();
    assert!(matches!(
        stale_binding.probe().probe(cancellation).await,
        Err(crate::inference::EngineProbeError::Transient)
    ));
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("deployment state")
            .status,
        ModelDeploymentStatus::Ready as i32
    );

    source.abort();
    ready.abort();
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn preparation_failure_cancels_and_drains_other_deployments_before_returning() {
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-cross-deployment-drain-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let gate = Arc::new(BlockingWorkGate::default());
    let cache = ModelArtifactCache::for_test(cache_root.clone(), "http://127.0.0.1:1")
        .with_cross_deployment_panic("a-Q8_0.gguf", gate.clone());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut first = crate::model::controller::test_support::test_desired_deployment(
        "first-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    first.model_deployment_id = "deployment-a".to_owned();
    let mut second = crate::model::controller::test_support::test_desired_deployment(
        "second-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    second.model_deployment_id = "deployment-b".to_owned();
    for (deployment, file, contents) in [
        (&mut first, "a-Q8_0.gguf", b"first".as_slice()),
        (&mut second, "b-Q8_0.gguf", b"second".as_slice()),
    ] {
        let artifact = model_file_source_mut(
            local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
                .model_artifact
                .as_mut()
                .expect("artifact"),
        );
        artifact.path = file.to_owned();
        artifact.provenance_digest = hex::encode(Sha256::digest(contents));
    }
    let second_path = cache
        .expected_paths(
            &resolved_launch(&second)
                .expect("second resolved launch")
                .artifacts,
        )
        .expect("second artifact paths")
        .path("model")
        .clone();
    std::fs::create_dir_all(second_path.parent().expect("second artifact parent"))
        .expect("second artifact parent");
    std::fs::write(&second_path, b"second").expect("second cached artifact");
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    {
        let mut state = desired.state.lock().expect("desired site");
        state.execution_targets = site.execution_targets;
        state.model_deployments = vec![first, second];
    }
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            RecordingRuntime {
                calls: calls.clone(),
                virtualized: false,
            },
        )
        .expect("Runtime adapter");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                desired,
                state: TestCurrent::new(calls.clone()),
                runtimes: builder.build().expect("Runtime map"),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: ArtifactManager::from_parts(
                    cache,
                    Some(crate::model::controller::test_support::ready_oci_preparer()),
                ),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );

    let mut reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    gate.wait_started().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut reconciliation)
            .await
            .is_err(),
        "reconciliation must wait for blocking sibling work"
    );
    gate.release();
    assert!(matches!(
        reconciliation.await.expect("reconciliation task"),
        Err(ModelLifecycleError::Task(_))
    ));
    assert_eq!(
        std::fs::read(&second_path).expect("second cached artifact"),
        b"second"
    );
    assert!(
        std::fs::read_dir(cache_root.join(".dotcore-coordinate-locks"))
            .expect("coordinate lock directory")
            .all(|entry| !entry
                .expect("coordinate lock entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".verification.json"))
    );
    assert!(runtime_calls(&calls).is_empty());

    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn pause_intent_cancels_a_permanently_stalled_artifact_repair() {
    let (source_root, preparation_started, allow_preparation, source) =
        gated_response_server(b"model".to_vec()).await;
    let cache_root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-reconciliation-pause-race-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&cache_root).expect("cache root");
    let cache = ModelArtifactCache::for_test(cache_root.clone(), &source_root);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    let digest = hex::encode(Sha256::digest(b"model"));
    model_file_source_mut(
        local_model_mut(deployment.snapshot.as_mut().expect("snapshot"))
            .model_artifact
            .as_mut()
            .expect("artifact"),
    )
    .provenance_digest = digest;
    let mut site = test_site();
    site.execution_targets[0].runtime_binding = Some(
        execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
            )),
        }),
    );
    desired
        .state
        .lock()
        .expect("desired site")
        .execution_targets = site.execution_targets.clone();
    desired
        .state
        .lock()
        .expect("desired deployments")
        .model_deployments
        .push(deployment.clone());
    let resolved = resolved_launch(&deployment).expect("resolved launch");
    let cached_artifact = cache
        .expected_paths(&resolved.artifacts)
        .expect("artifact paths")
        .path("model")
        .clone();
    std::fs::create_dir_all(cached_artifact.parent().expect("snapshot parent"))
        .expect("snapshot parent");
    std::fs::write(&cached_artifact, b"corrupt").expect("corrupt cached model");
    let identity = crate::model::controller::test_support::expected_identity(
        &site,
        &deployment,
        "runtime/docker",
    );
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            PausedRuntime {
                calls: calls.clone(),
                observations: Arc::new(Mutex::new(vec![vec![WorkloadObservation::stopped(
                    identity,
                )]])),
                start_observation: None,
                startup_logs: Arc::new(Mutex::new(StartupLogBehavior::Unavailable)),
                backend_id: RuntimeBackendId("runtime/docker".to_owned()),
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls.clone());
    current
        .upsert(current_deployment("deployment"), None)
        .expect("current deployment");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                desired: desired.clone(),
                state: current.clone(),
                runtimes: builder.build().expect("Runtime map"),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[],
                    )
                    .expect("preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: desired.listener(),
                artifacts: ArtifactManager::from_parts(
                    cache,
                    Some(crate::model::controller::test_support::ready_oci_preparer()),
                ),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );

    let reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    preparation_started.notified().await;
    desired.transition("deployment", ModelDeploymentDesiredState::Paused);
    let reconciled = tokio::time::timeout(Duration::from_secs(1), reconciliation)
        .await
        .expect("pause intent cancels the stalled preparation")
        .expect("reconciliation task");
    assert!(reconciled.is_ok());
    assert!(matches!(
        controller.inner.reconcile().await,
        Ok(ReconcileOutcome::Settled)
    ));
    assert_eq!(
        current.snapshot().expect("current state").model_deployments[0]
            .state
            .as_ref()
            .expect("paused state")
            .status,
        ModelDeploymentStatus::Paused as i32
    );
    assert_eq!(
        std::fs::read(&cached_artifact).expect("cached artifact"),
        b"corrupt"
    );
    assert!(
        std::fs::read_dir(cache_root.join(".dotcore-coordinate-locks"))
            .expect("coordinate lock directory")
            .all(|entry| !entry
                .expect("coordinate lock entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".verification.json"))
    );
    assert!(runtime_calls(&calls).is_empty());

    source.abort();
    drop(allow_preparation);
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn pause_intent_is_not_undone_by_stale_final_publication_and_other_deployments_remain_live() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    let mut first = crate::model::controller::test_support::test_desired_deployment(
        "first-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    first.model_deployment_id = "deployment-a".to_owned();
    let mut second = crate::model::controller::test_support::test_desired_deployment(
        "second-model",
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
    );
    second.model_deployment_id = "deployment-b".to_owned();
    desired
        .state
        .lock()
        .expect("desired site")
        .model_deployments = vec![first.clone(), second.clone()];
    let site = test_site();
    let mut first_identity = crate::model::controller::test_support::expected_identity(
        &site,
        &first,
        dotcore_runtime_backends::simulated::BACKEND_ID,
    );
    first_identity.workload_id = WorkloadId("workload-a".to_owned());
    first_identity.services[0].reference.realization_id = "workload-a".to_owned();
    let mut second_identity = crate::model::controller::test_support::expected_identity(
        &site,
        &second,
        dotcore_runtime_backends::simulated::BACKEND_ID,
    );
    second_identity.workload_id = WorkloadId("workload-b".to_owned());
    second_identity.services[0].reference.realization_id = "workload-b".to_owned();
    let second_started = Arc::new(Notify::new());
    let release_second = Arc::new(Notify::new());
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            GatedSecondRuntime {
                observations: vec![
                    WorkloadObservation::Running(first_identity),
                    WorkloadObservation::stopped(second_identity.clone()),
                ],
                second_started: second_started.clone(),
                release_second: release_second.clone(),
                second_running: second_identity,
            },
        )
        .expect("Runtime adapter");
    let current = TestCurrent::new(calls);
    current
        .upsert(current_deployment("deployment-a"), None)
        .expect("first current state");
    let mut second_current = current_deployment("deployment-b");
    second_current
        .state
        .as_mut()
        .expect("second current state")
        .status = ModelDeploymentStatus::Ready as i32;
    current
        .upsert(second_current, None)
        .expect("second current state");
    let (serving_targets_publisher, serving_targets_source) =
        crate::model::controller::serving_targets::ServingTargetsPublisher::new();
    let controller = manager_test_controller(
        desired.clone(),
        super::super::ModelRealizer {
            inner: Arc::new(InferenceControllerInner {
                workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                    std::path::Path::new("/test-preparation-logs"),
                ),
                results: desired.clone(),
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),

                desired: desired.clone(),
                state: current,
                runtimes: builder.build().expect("Runtime map"),
                engine_integrations:
                    crate::model::controller::test_support::test_engine_integrations(),
                operation: Arc::new(AsyncMutex::new(())),
                reconciliation: AsyncMutex::new(()),
                preparations:
                    crate::model::controller::preparation::PreparationCoordinator::from_deployments(
                        &[first, second],
                    )
                    .expect("preparation coordinator"),
                observer_started: Mutex::new(false),
                wakeup: Arc::new(Notify::new()),
                artifacts: ArtifactManager::from_parts(
                    ModelArtifactCache::new("/test-cache".into()).expect("artifact cache"),
                    Some(crate::model::controller::test_support::ready_oci_preparer()),
                ),
                serving_targets_publisher,
                serving_targets_source,
                accelerator_attribution: None,
                shutdown: tokio_util::sync::CancellationToken::new(),
                timing: Timing::test().model,
                clock: Arc::new(SystemClock),
            }),
        },
    );
    let reconciliation = tokio::spawn({
        let controller = controller.clone();
        async move { controller.inner.reconcile().await }
    });
    if tokio::time::timeout(Duration::from_secs(1), second_started.notified())
        .await
        .is_err()
    {
        panic!(
            "second deployment gate was not reached: {:?}",
            reconciliation.await
        );
    }

    desired.transition("deployment-a", ModelDeploymentDesiredState::Paused);
    release_second.notify_one();
    tokio::time::timeout(Duration::from_secs(1), reconciliation)
        .await
        .expect("reconciliation completion")
        .expect("reconciliation task")
        .expect("reconciliation");

    assert!(
        controller
            .inner
            .serving_targets_source
            .snapshot()
            .targets
            .iter()
            .all(|target| target.deployment_id.0 != "deployment-a")
    );
    assert!(
        controller
            .inner
            .serving_targets_source
            .snapshot()
            .targets
            .iter()
            .any(|target| target.deployment_id.0 == "deployment-b")
    );
}

#[tokio::test]
async fn replacement_session_rejects_suspended_create_and_start_completions() {
    for mutation in [GatedMutation::Create, GatedMutation::Start] {
        let mut site = test_site();
        let desired_deployment =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        site.model_deployments.push(desired_deployment.clone());
        let artifacts =
            ArtifactManager::new("/session-fencing-cache".into(), None).expect("artifact manager");
        let resolved = resolved_launch(&desired_deployment).expect("resolved launch");
        let prepared = artifacts
            .expected_model_paths(&resolved.artifacts)
            .expect("prepared paths");
        let launch = workload_launch(
            &desired_deployment,
            selected_target(&site, &desired_deployment).expect("selected target"),
            &prepared,
        )
        .expect("workload launch");
        let observed = ObservedWorkload {
            workload_id: WorkloadId("session-workload".to_owned()),
            correlation: launch.correlation.clone(),
            backend_id: RuntimeBackendId(
                dotcore_runtime_backends::simulated::BACKEND_ID.to_owned(),
            ),
            loopback_endpoints: Vec::new(),
            services: vec![dotcore_runtime::WorkloadServiceBinding::native(
                "session-workload",
                "http",
                launch.correlation.reconcile_generation,
            )],
            host_process: None,
        };
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (controller, authority, current) = leased_controller_for_runtime(
            site.clone(),
            GatedMutationRuntime {
                mutation,
                started: started.clone(),
                release: release.clone(),
                observation: observed.clone(),
            },
            crate::model::controller::test_support::test_engine_integrations(),
        );
        let before = current.snapshot().expect("current state before mutation");
        let task = tokio::spawn({
            let controller = controller.clone();
            let desired_deployment = desired_deployment.clone();
            let launch = launch.clone();
            let observation =
                matches!(mutation, GatedMutation::Start).then_some(TargetObservation {
                    execution_target_id: "target-one".to_owned(),
                    observation: WorkloadObservation::stopped(observed),
                });
            async move {
                controller
                    .inner
                    .transition_workload(
                        &desired_deployment,
                        &launch,
                        observation,
                        false,
                        false,
                        false,
                        None,
                    )
                    .await
            }
        });
        started.notified().await;
        replace_session_and_accept_same_view(&authority, &site);
        release.notify_one();
        let error = task
            .await
            .expect("suspended mutation task")
            .expect_err("old-session mutation completion");

        assert!(matches!(
            error,
            ModelLifecycleError::Runtime(
                crate::dispatch::model::RuntimeTargetError::StaleCompletion
            )
        ));
        assert_eq!(
            current.snapshot().expect("current state after mutation"),
            before,
            "stale create/start completion must not alter current state or activation",
        );
        assert!(
            controller.serving_targets().snapshot().targets.is_empty(),
            "stale create/start completion must not publish serving state",
        );
    }
}

#[tokio::test]
async fn replacement_session_rejects_suspended_status_without_retiring_replacement_binding() {
    let mut site = test_site();
    let desired_deployment =
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
    site.model_deployments.push(desired_deployment.clone());
    let artifacts =
        ArtifactManager::new("/session-status-cache".into(), None).expect("artifact manager");
    let resolved = resolved_launch(&desired_deployment).expect("resolved launch");
    let prepared = artifacts
        .expected_model_paths(&resolved.artifacts)
        .expect("prepared paths");
    let launch = workload_launch(
        &desired_deployment,
        selected_target(&site, &desired_deployment).expect("selected target"),
        &prepared,
    )
    .expect("workload launch");
    let running = |workload_id: &str| {
        WorkloadObservation::Running(ObservedWorkload {
            workload_id: WorkloadId(workload_id.to_owned()),
            correlation: launch.correlation.clone(),
            backend_id: RuntimeBackendId(
                dotcore_runtime_backends::simulated::BACKEND_ID.to_owned(),
            ),
            loopback_endpoints: Vec::new(),
            services: vec![dotcore_runtime::WorkloadServiceBinding::native(
                workload_id,
                "http",
                launch.correlation.reconcile_generation,
            )],
            host_process: None,
        })
    };
    let readiness_started = Arc::new(Notify::new());
    let release_readiness = Arc::new(Notify::new());
    let engine_integrations = Arc::new(
        crate::model::engine::EngineIntegrationRegistry::with_native_provider_and_transports(
            Arc::new(FirstReadinessGatedProvider {
                bindings: AtomicUsize::new(0),
                started: readiness_started.clone(),
                release: release_readiness.clone(),
            }),
            crate::model::engine::EngineTransportFactories::local_engines(),
        ),
    );
    let (controller, authority, current) = leased_controller_for_runtime(
        site.clone(),
        InMemoryRuntimeBackend::new(dotcore_domain::ExecutionPlatform::linux_amd64()),
        engine_integrations,
    );
    let old_status = tokio::spawn({
        let controller = controller.clone();
        let site = site.clone();
        let desired_deployment = desired_deployment.clone();
        let launch = launch.clone();
        let observation = running("old-session-workload");
        async move {
            controller
                .inner
                .record_workload_observation(
                    &site,
                    &desired_deployment,
                    &site.execution_targets[0],
                    &launch,
                    observation,
                )
                .await
                .map(|_| ())
        }
    });
    readiness_started.notified().await;
    replace_session_and_accept_same_view(&authority, &site);

    let replacement = controller
        .inner
        .record_workload_observation(
            &site,
            &desired_deployment,
            &site.execution_targets[0],
            &launch,
            running("replacement-workload"),
        )
        .await
        .expect("replacement status");
    let replacement_binding = replacement
        .ready_target
        .as_ref()
        .expect("replacement ready target")
        .binding
        .clone();
    let replacement_targets =
        serving_targets([replacement.ready_target.expect("replacement ready target")])
            .expect("replacement serving targets");
    controller
        .inner
        .publish_ready_serving_target(
            &desired_deployment,
            Some(replacement_targets),
            Some(&replacement.operation),
        )
        .await
        .expect("replacement serving publication");
    let replacement_state = current.snapshot().expect("replacement current state");

    release_readiness.notify_one();
    let error = old_status
        .await
        .expect("old status task")
        .expect_err("old-session status completion");

    assert!(matches!(
        error,
        ModelLifecycleError::Runtime(crate::dispatch::model::RuntimeTargetError::StaleCompletion)
    ));
    assert_eq!(
        current.snapshot().expect("state after stale readiness"),
        replacement_state,
        "stale readiness must not overwrite replacement current state or activation",
    );
    assert_eq!(
        controller.serving_targets().snapshot().targets.len(),
        1,
        "stale readiness must not withdraw replacement serving publication",
    );
    assert!(
        controller
            .inner
            .engine_integrations
            .retire_inactive_binding(&replacement_binding),
        "stale binding cleanup must leave the replacement binding retained",
    );
}

fn leased_controller_for_runtime<R>(
    site: dotcore_proto::dotcore::site::v1::Site,
    runtime: R,
    engine_integrations: Arc<crate::model::engine::EngineIntegrationRegistry>,
) -> (
    super::super::test_support::TestRealizer,
    AcceptedNodeViewAuthority,
    TestCurrent,
)
where
    R: RuntimeBackend + 'static,
{
    let calls = Arc::new(Mutex::new(Vec::new()));
    let desired = TestDesired::new(calls.clone());
    *desired.state.lock().expect("desired site") = site.clone();
    let current = TestCurrent::new(calls);
    let node_id = site.execution_targets[0].site_node_id.clone();
    let projection = NodeViewProjection::for_site_node(&site, &node_id).expect("leased node view");
    let authority = AcceptedNodeViewAuthority::new(
        site.site_id.clone(),
        node_id.clone(),
        NodeIncarnation::parse("b".repeat(32)).expect("node incarnation"),
        ManagerSession::parse("a".repeat(32)).expect("manager session"),
        Duration::from_secs(30),
    )
    .expect("view authority");
    authority
        .install_for_test(site.site_id.clone(), node_id, 1, &projection)
        .expect("initial accepted view");
    let mut runtimes = RuntimeTargetMapBuilder::new()
        .with_action_authority(RuntimeActionAuthority::Leased(authority.clone()));
    runtimes
        .insert("target-one", runtime)
        .expect("Runtime adapter");
    let controller =
        crate::model::controller::test_support::TestRealizer::for_test_with_dependencies(
            desired.clone(),
            current.clone(),
            runtimes.build().expect("Runtime map"),
            Vec::new(),
            "/session-fencing-cache".into(),
            crate::model::controller::ModelControllerDependencies {
                target_capabilities:
                    crate::model::controller::test_support::test_target_capability_source(),
                artifacts: ArtifactDependencies::default(),
                engine_integrations,
                accelerator_attribution: None,
            },
        )
        .expect("leased controller");
    (controller, authority, current)
}

fn replace_session_and_accept_same_view(
    authority: &AcceptedNodeViewAuthority,
    site: &dotcore_proto::dotcore::site::v1::Site,
) {
    authority
        .replace_manager_session(
            ManagerSession::parse("c".repeat(32)).expect("replacement session"),
        )
        .expect("replace manager session");
    let node_id = site.execution_targets[0].site_node_id.clone();
    let projection =
        NodeViewProjection::for_site_node(site, &node_id).expect("replacement node view");
    authority
        .install_for_test(site.site_id.clone(), node_id, 1, &projection)
        .expect("replacement accepted view");
}

#[test]
fn observer_failures_emit_bounded_error_events() {
    let (_, records, output) = capture_records(|| {
        let span = info_span!("model_observer", operation = "model_observer");
        span.in_scope(|| {
            emit_observer_failure(
                &span,
                &ModelLifecycleError::outcome_uncertain_with_source(
                    "synthetic-required-launch-secret",
                    anyhow!("synthetic-required-launch-secret"),
                ),
            );
        });
    });

    assert!(records.iter().any(|record| matches!(
        record,
        Record::SpanClosed { name, fields }
            if name == "model_observer"
                && fields.get("operation") == Some(&"\"model_observer\"".to_owned())
                && !fields.contains_key("outcome")
                && !fields.contains_key("error_kind")
    )));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "ERROR"
                && fields.get("operation").is_some_and(|value| value.contains("model_reconcile"))
                && fields.get("outcome").is_some_and(|value| value.contains("failed"))
                && fields.get("error_kind").is_some_and(|value| value.contains("outcome_uncertain"))
    )));
    assert!(!output.contains("synthetic-required-launch-secret"));
}

#[test]
fn repeated_observer_outcomes_do_not_accumulate_span_fields() {
    let (_, records, output) = capture_records(|| {
        let span = info_span!("model_observer", operation = "model_observer");
        span.in_scope(|| {
                debug!(parent: &span, operation = "model_reconcile", outcome = "success", state = ?ReconcileOutcome::Settled, "model reconciliation completed");
                debug!(parent: &span, operation = "model_reconcile", outcome = "success", state = ?ReconcileOutcome::Pending, "model reconciliation completed");
                emit_observer_failure(
                    &span,
                    &ModelLifecycleError::outcome_uncertain_with_source("failure", anyhow!("failure")),
                );
                emit_observer_failure(&span, &ModelLifecycleError::configuration("failure"));
                info!(parent: &span, operation = "model_observer", outcome = "stopped", state = "dropped", "model observer stopped");
            });
    });

    let events = records
        .iter()
        .filter(|record| matches!(record, Record::Event { .. }))
        .count();
    assert_eq!(events, 5);
    assert_eq!(output.matches("outcome=").count(), 5);
    assert_eq!(output.matches("error_kind=").count(), 2);
    assert_eq!(output.matches("outcome=\"success\"").count(), 2);
    assert_eq!(output.matches("outcome=\"failed\"").count(), 2);
    assert_eq!(output.matches("outcome=\"stopped\"").count(), 1);
}

#[tokio::test]
async fn an_input_change_wakes_a_settled_observer_without_waiting_for_the_safety_scan() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let discoveries = Arc::new(AtomicUsize::new(0));
    let discovered = Arc::new(Notify::new());
    let mut builder = RuntimeTargetMapBuilder::new();
    builder
        .insert(
            "target-one",
            CountingRuntime {
                discoveries: discoveries.clone(),
                discovered: discovered.clone(),
            },
        )
        .expect("Runtime adapter");
    let desired = TestDesired::new(calls.clone());
    let controller = crate::model::controller::test_support::TestRealizer::for_test(
        desired.clone(),
        TestCurrent::new(calls),
        builder.build().expect("Runtime map"),
        Vec::new(),
        PathBuf::from("/test-cache"),
        Some(crate::model::controller::test_support::ready_oci_preparer()),
    )
    .expect("controller");
    controller.start_observer();

    wait_for_discoveries(&discoveries, &discovered, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(discoveries.load(Ordering::SeqCst), 1);

    desired
        .load_model(
            "model",
            "profile",
            &test_placement(),
            &crate::test_support::local_model_configuration(),
            &ModelDeploymentExposureIntent::Hidden,
            None,
        )
        .expect("Load");
    desired.listener().notify_one();
    tokio::time::timeout(
        Duration::from_millis(250),
        wait_for_discoveries(&discoveries, &discovered, 2),
    )
    .await
    .expect("mutation wakeup");
}

#[tokio::test(start_paused = true)]
async fn waiting_for_a_desired_view_is_logged_once_instead_of_an_error_per_pass() {
    let ((), records, _) = capture_records_async(|| async {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let mut runtimes = RuntimeTargetMapBuilder::new();
        runtimes
            .insert(
                "target-one",
                CountingRuntime {
                    discoveries: Arc::new(AtomicUsize::new(0)),
                    discovered: Arc::new(Notify::new()),
                },
            )
            .expect("Runtime adapter");
        let controller = crate::model::controller::test_support::TestRealizer::for_test(
            desired.clone(),
            TestCurrent::new(calls),
            runtimes.build().expect("Runtime map"),
            Vec::new(),
            PathBuf::from("/test-cache"),
            None,
        )
        .expect("controller");
        desired.withhold_view(true);
        controller.start_observer();
        // The paused clock runs the pending cadence for many passes without a view.
        tokio::time::sleep(Duration::from_secs(5)).await;

        desired.withhold_view(false);
        desired.listener().notify_one();
        tokio::time::sleep(Duration::from_secs(1)).await;
    })
    .await;

    let waiting_passes = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::SpanRecorded { name, fields }
                    if name == "model_reconcile"
                        && fields.get("outcome") == Some(&"\"waiting\"".to_owned())
            )
        })
        .count();
    assert!(
        waiting_passes >= 10,
        "reconciliation ran many passes without a view: {waiting_passes}"
    );
    let events = |outcome: &str| {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Event { level, fields }
                    if fields.get("outcome") == Some(&format!("\"{outcome}\"")) =>
                {
                    Some(level.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(events("awaiting_desired_view"), ["WARN"]);
    assert_eq!(events("desired_view_available"), ["INFO"]);
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::Event { level, .. } if level == "ERROR"
        )),
        "no ERROR while waiting for a view: {records:?}"
    );
}

async fn wait_for_discoveries(discoveries: &AtomicUsize, discovered: &Notify, expected: usize) {
    while discoveries.load(Ordering::SeqCst) < expected {
        discovered.notified().await;
    }
}

async fn wait_for_download_progress(
    current: &TestCurrent,
    completed: u64,
) -> dotcore_proto::dotcore::model::v1::ModelDeploymentState {
    loop {
        let state = current.snapshot().expect("current state").model_deployments[0]
            .state
            .clone()
            .expect("deployment state");
        if state.status == ModelDeploymentStatus::Downloading as i32
            && state.status_message != "Preparing model realization"
            && state.status_message != "Starting model realization"
            && state
                .progress
                .as_ref()
                .is_some_and(|progress| progress.completed == completed)
        {
            return state;
        }
        tokio::task::yield_now().await;
    }
}

async fn response_server(responses: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let root = format!("http://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        for body in responses {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("request");
                let mut request = [0_u8; 1024];
                let bytes = stream.read(&mut request).await.expect("request bytes");
                let method = String::from_utf8_lossy(&request[..bytes])
                    .split_whitespace()
                    .next()
                    .expect("request method")
                    .to_owned();
                stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"fixture-blob\"\r\nX-Repo-Commit: 0123456789abcdef0123456789abcdef01234567\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("response headers");
                if method != "HEAD" {
                    stream.write_all(&body).await.expect("response body");
                }
            }
        }
    });
    (root, task)
}

async fn streaming_response_server(
    body: Vec<u8>,
    first_chunk_length: usize,
) -> (
    String,
    Arc<Notify>,
    Arc<Notify>,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let root = format!("http://{}/", listener.local_addr().expect("address"));
    let head_received = Arc::new(Notify::new());
    let allow_head = Arc::new(Notify::new());
    let first_bytes_sent = Arc::new(Notify::new());
    let allow_finish = Arc::new(Notify::new());
    let task = tokio::spawn({
        let head_received = head_received.clone();
        let allow_head = allow_head.clone();
        let first_bytes_sent = first_bytes_sent.clone();
        let allow_finish = allow_finish.clone();
        async move {
            let (mut head, _) = listener.accept().await.expect("HEAD request");
            let mut request = [0_u8; 1024];
            let bytes = head.read(&mut request).await.expect("HEAD request bytes");
            assert!(String::from_utf8_lossy(&request[..bytes]).starts_with("HEAD "));
            head_received.notify_one();
            allow_head.notified().await;
            head.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nETag: \"fixture-blob\"\r\nX-Repo-Commit: 0123456789abcdef0123456789abcdef01234567\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("HEAD response");

            let (mut get, _) = listener.accept().await.expect("GET request");
            let bytes = get.read(&mut request).await.expect("GET request bytes");
            assert!(String::from_utf8_lossy(&request[..bytes]).starts_with("GET "));
            get.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nETag: \"fixture-blob\"\r\nX-Repo-Commit: 0123456789abcdef0123456789abcdef01234567\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("GET response headers");
            get.write_all(&body[..first_chunk_length])
                .await
                .expect("first response chunk");
            get.flush().await.expect("first response flush");
            first_bytes_sent.notify_one();
            allow_finish.notified().await;
            get.write_all(&body[first_chunk_length..])
                .await
                .expect("remaining response body");
        }
    });
    (
        root,
        head_received,
        allow_head,
        first_bytes_sent,
        allow_finish,
        task,
    )
}

async fn gated_response_server(
    body: Vec<u8>,
) -> (
    String,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let root = format!("http://{}/", listener.local_addr().expect("address"));
    let preparation_started = Arc::new(Notify::new());
    let allow_preparation = Arc::new(Notify::new());
    let task = tokio::spawn({
        let preparation_started = Arc::clone(&preparation_started);
        let allow_preparation = Arc::clone(&allow_preparation);
        async move {
            for request_number in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("request");
                let mut request = [0_u8; 1024];
                let bytes = stream.read(&mut request).await.expect("request bytes");
                let method = String::from_utf8_lossy(&request[..bytes])
                    .split_whitespace()
                    .next()
                    .expect("request method")
                    .to_owned();
                if request_number == 0 {
                    preparation_started.notify_one();
                    allow_preparation.notified().await;
                }
                stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nETag: \"fixture-blob\"\r\nX-Repo-Commit: 0123456789abcdef0123456789abcdef01234567\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("response headers");
                if method != "HEAD" {
                    stream.write_all(&body).await.expect("response body");
                }
            }
        }
    });
    (root, preparation_started, allow_preparation, task)
}

async fn ready_server(model_id: &str) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("readiness listener");
    let port = listener.local_addr().expect("readiness address").port();
    let model_id = model_id.to_owned();
    let task = tokio::spawn(async move {
        for body in [
            br#"{"status":"ok"}"#.to_vec(),
            format!(r#"{{"object":"list","data":[{{"id":"{model_id}"}}]}}"#).into_bytes(),
        ] {
            let (mut stream, _) = listener.accept().await.expect("readiness request");
            let mut request = [0_u8; 1024];
            let _ = stream
                .read(&mut request)
                .await
                .expect("readiness request bytes");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("readiness response headers");
            stream
                .write_all(&body)
                .await
                .expect("readiness response body");
        }
    });
    (port, task)
}

fn model_file_source_mut(
    artifact: &mut dotcore_proto::dotcore::catalog::v1::ModelArtifact,
) -> &mut dotcore_proto::dotcore::catalog::v1::HuggingFaceFileArtifact {
    use dotcore_proto::dotcore::catalog::v1::hugging_face_artifact_source::Content;

    match artifact
        .source
        .as_mut()
        .expect("Hugging Face source")
        .content
        .as_mut()
        .expect("Hugging Face source content")
    {
        Content::File(file) => file,
        Content::Snapshot(_) => panic!("test artifact source is a file"),
    }
}

/// Deployment A tears down a running workload whose stop blocks until released, while X has
/// a running workload and O is an owned workload with no declaration. While A's stop is
/// held, X's declaration changes and O becomes declared.
struct OrphanRace {
    site: dotcore_proto::dotcore::site::v1::Site,
    calls: Arc<Mutex<Vec<(&'static str, String)>>>,
    stop_started: Arc<Notify>,
    release_stop: Arc<Notify>,
}

impl OrphanRace {
    fn new() -> Self {
        let mut site = test_site();
        site.model_deployments = vec![
            orphan_race_deployment(
                "a-teardown",
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Teardown,
            ),
            orphan_race_deployment(
                "x-paused",
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused,
            ),
        ];
        Self {
            site,
            calls: Arc::new(Mutex::new(Vec::new())),
            stop_started: Arc::new(Notify::new()),
            release_stop: Arc::new(Notify::new()),
        }
    }

    fn changed_site(&self) -> dotcore_proto::dotcore::site::v1::Site {
        let mut site = self.site.clone();
        let changed = &mut site.model_deployments[1];
        changed.desired_state =
            dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Teardown as i32;
        changed.lifecycle_generation += 1;
        site.model_deployments.push(orphan_race_deployment(
            "o-declared",
            dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
        ));
        site
    }

    fn runtimes(
        &self,
        mut builder: RuntimeTargetMapBuilder,
    ) -> crate::dispatch::model::RuntimeTargetMap {
        let identity = |deployment_id: &str, workload_id: &str| {
            let mut site = self.site.clone();
            site.model_deployments.push(orphan_race_deployment(
                "o-declared",
                dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present,
            ));
            let deployment = site
                .model_deployments
                .iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .expect("race deployment");
            let mut identity = crate::model::controller::test_support::expected_identity(
                &site,
                deployment,
                "runtime/test",
            );
            identity.workload_id = WorkloadId(workload_id.to_owned());
            identity
        };
        builder
            .insert(
                "target-one",
                OrphanRaceRuntime {
                    calls: Arc::clone(&self.calls),
                    teardown: identity("a-teardown", "a-workload"),
                    discovered: vec![
                        WorkloadObservation::Running(identity("a-teardown", "a-workload")),
                        WorkloadObservation::Running(identity("x-paused", "x-workload")),
                        WorkloadObservation::Running(identity("o-declared", "o-workload")),
                    ],
                    stop_started: Arc::clone(&self.stop_started),
                    release_stop: Arc::clone(&self.release_stop),
                },
            )
            .expect("Runtime adapter");
        builder.build().expect("Runtime map")
    }

    fn calls(&self) -> Vec<(&'static str, String)> {
        self.calls.lock().expect("calls").clone()
    }
}

struct OrphanRaceRuntime {
    calls: Arc<Mutex<Vec<(&'static str, String)>>>,
    teardown: ObservedWorkload,
    discovered: Vec<WorkloadObservation>,
    stop_started: Arc<Notify>,
    release_stop: Arc<Notify>,
}

impl RuntimeBackend for OrphanRaceRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/test".to_owned())
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(ready(Ok(self.discovered.clone())))
    }

    fn create_workload<'a>(
        &'a self,
        _workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(ready(Err(test_runtime_error())))
    }

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        if *workload_id == self.teardown.workload_id {
            Box::pin(ready(Ok(WorkloadObservation::Absent(
                self.teardown.clone(),
            ))))
        } else {
            Box::pin(ready(Err(test_runtime_error())))
        }
    }

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls
            .lock()
            .expect("calls")
            .push(("stop", workload_id.0.clone()));
        let gated = *workload_id == self.teardown.workload_id;
        Box::pin(async move {
            if gated {
                self.stop_started.notify_one();
                self.release_stop.notified().await;
            }
            Ok(())
        })
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.calls
            .lock()
            .expect("calls")
            .push(("remove", workload_id.0.clone()));
        Box::pin(ready(Ok(())))
    }

    fn remove_storage<'a>(
        &'a self,
        _: &'a dotcore_runtime::WorkloadStorageKey,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

fn orphan_race_deployment(
    deployment_id: &str,
    desired_state: dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState,
) -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
    let mut deployment = crate::model::controller::test_support::test_desired_deployment(
        "fixture-model",
        desired_state,
    );
    deployment.model_deployment_id = deployment_id.to_owned();
    deployment
}

fn accept_view(
    authority: &AcceptedNodeViewAuthority,
    site: &dotcore_proto::dotcore::site::v1::Site,
    revision: u64,
) {
    let node_id = site.execution_targets[0].site_node_id.clone();
    let projection = NodeViewProjection::for_site_node(site, &node_id).expect("node view");
    authority
        .install_for_test(site.site_id.clone(), node_id, revision, &projection)
        .expect("accepted node view");
}

fn local_model_mut(
    snapshot: &mut ModelDeploymentSnapshot,
) -> &mut dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot {
    match snapshot.source.as_mut() {
        Some(ModelDeploymentSnapshotSource::Local(model)) => model,
        Some(ModelDeploymentSnapshotSource::Upstream(_)) | None => {
            panic!("test snapshot has a local model")
        }
    }
}

/// The workload changes published since the subscription that are already waiting.
fn published_changes(
    changes: &mut crate::deployment_logs::WorkloadChanges,
) -> Vec<crate::deployment_logs::WorkloadChange> {
    let mut published = Vec::new();
    while let Some(Some(Ok(change))) = futures_util::FutureExt::now_or_never(changes.next()) {
        published.push(change);
    }
    published
}

fn model_started() -> crate::deployment_logs::WorkloadChange {
    crate::deployment_logs::WorkloadChange::Started {
        source_name: "model".to_owned(),
    }
}
